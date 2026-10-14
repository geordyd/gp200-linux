//! Original services, receipts and moved stores on replay only; no physical MIDI.
use super::*;
use crate::*;
use gp200_transport::{
    FirmwareCaptureWriteResult, FirmwareNormalCaptureObservation, MidiSession, ReceiveSink,
    TransportBackend,
};
use std::{
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};
static NEXT: AtomicU64 = AtomicU64::new(0);
#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn completion_presentation_identity_tracks_actual_monitor_generation_run_and_retirement() {
    let (_directory, mut app, log) = fixture();
    let coordinator = &mut app.firmware_coordinator;
    coordinator.test_unreadable_completion_monitor("first-ui-run".into());
    let (first, unavailable) = coordinator.linux_completion_presentation();
    let first = first.unwrap();
    assert!(unavailable.is_none());
    assert_eq!(
        coordinator.linux_completion_presentation().0.unwrap(),
        first
    );
    coordinator.generation += 1;
    let second = coordinator.linux_completion_presentation().0.unwrap();
    assert_ne!(second, first);
    coordinator.completion_run_directory = Some("second-ui-run".into());
    let third = coordinator.linux_completion_presentation().0.unwrap();
    assert_ne!(third, second);
    coordinator.install_completion_monitor(LinuxCompletionMonitor::default());
    let fourth = coordinator.linux_completion_presentation().0.unwrap();
    assert_ne!(
        fourth, third,
        "same generation and directory do not identify a replacement monitor"
    );
    coordinator.phase = FirmwareCoordinatorPhase::RecoveryRequired;
    assert!(coordinator.linux_completion_presentation().0.is_none());
    coordinator.phase = FirmwareCoordinatorPhase::Owned;
    coordinator.completion_monitor = None;
    assert!(coordinator.linux_completion_presentation().0.is_none());
    assert_eq!(log.lock().unwrap().offers, 0);
}
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "gp200-coordinator-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[derive(Default)]
struct Log {
    offers: usize,
    opens: usize,
    closes: usize,
    panic_capture_observation: bool,
    capture_observations: usize,
    fail_connect: bool,
    connect_attempts: usize,
    capture_variation: Option<usize>,
    capture_requests: usize,
}
struct ReplayBackend {
    inner: gp200_sim::SimulatorBackend,
    log: Arc<Mutex<Log>>,
}
impl TransportBackend for ReplayBackend {
    fn mode(&self) -> BackendMode {
        BackendMode::Simulator
    }
    fn enumerate(&mut self) -> Result<Vec<PortPairCandidate>, ServiceError> {
        self.inner.enumerate()
    }
    fn connect(
        &mut self,
        selection: &DeviceSelection,
        sink: ReceiveSink,
    ) -> Result<Box<dyn MidiSession>, ServiceError> {
        let fail = {
            let mut log = self.log.lock().unwrap();
            log.connect_attempts += 1;
            log.fail_connect
        };
        if fail {
            return Err(ServiceError::new(
                ErrorKind::Io,
                "Injected actual backend fresh connection failure",
            ));
        }
        let inner = self.inner.connect(selection, sink.clone())?;
        self.log.lock().unwrap().opens += 1;
        Ok(Box::new(ReplaySession {
            inner: Some(inner),
            log: self.log.clone(),
            sink,
            capture_pedal: None,
            counts: Default::default(),
        }))
    }
}
struct ReplaySession {
    inner: Option<Box<dyn MidiSession>>,
    log: Arc<Mutex<Log>>,
    sink: ReceiveSink,
    capture_pedal: Option<gp200_sim::SimulatedPedal>,
    counts: std::collections::BTreeMap<(u16, u16, u16), usize>,
}
impl MidiSession for ReplaySession {
    fn send(&mut self, bytes: &[u8]) -> Result<(), ServiceError> {
        self.log.lock().unwrap().offers += 1;
        self.inner
            .as_mut()
            .ok_or_else(|| ServiceError::new(ErrorKind::Disconnected, "Replay closed"))?
            .send(bytes)
    }
    fn normal_capture_observation(
        &mut self,
    ) -> Result<FirmwareNormalCaptureObservation, ServiceError> {
        let panic = {
            let mut log = self.log.lock().unwrap();
            log.capture_observations += 1;
            log.panic_capture_observation
        };
        assert!(
            !panic,
            "Injected genuine MIDI session capture-observation panic"
        );
        FirmwareNormalCaptureObservation::replay(
            FirmwareUsbObservation {
                topology: "replay only".into(),
                vendor: 1,
                product: 2,
                serial: "not physical".into(),
                interface: "simulator".into(),
                endpoint: "explicit replay".into(),
            },
            "coordinator replay owner",
        )
    }
    fn capture_write_once(&mut self, bytes: &[u8]) -> FirmwareCaptureWriteResult {
        let variation = {
            let mut log = self.log.lock().unwrap();
            log.capture_requests += 1;
            log.capture_variation
        };
        if let Some(offset) = variation {
            self.log.lock().unwrap().offers += 1;
            return match self.varied_capture_response(bytes, offset) {
                Ok(()) => FirmwareCaptureWriteResult::Known {
                    accepted: bytes.len(),
                    error: None,
                },
                Err(e) => FirmwareCaptureWriteResult::Unknown(e),
            };
        }
        match self.send(bytes) {
            Ok(()) => FirmwareCaptureWriteResult::Known {
                accepted: bytes.len(),
                error: None,
            },
            Err(e) => FirmwareCaptureWriteResult::Unknown(e),
        }
    }
    fn close_normal_capture(&mut self) -> Result<(), ServiceError> {
        self.inner.take();
        self.log.lock().unwrap().closes += 1;
        Ok(())
    }
}
impl ReplaySession {
    fn varied_capture_response(
        &mut self,
        bytes: &[u8],
        forbidden_offset: usize,
    ) -> Result<(), ServiceError> {
        use gp200_protocol::{
            RawTransfer, Reassembler, StreamDecoder, StreamEvent, decode_packet, encode_transfer,
        };
        let packet = decode_packet(bytes).map_err(|e| ServiceError::invalid(e.to_string()))?;
        let request = RawTransfer::from_payload(packet.kind, packet.payload)
            .map_err(|e| ServiceError::invalid(e.to_string()))?;
        let first = &request.records()[0];
        let (group, slot) = if first.command == 0 {
            (
                u16::from_le_bytes(first.body[..2].try_into().unwrap()),
                u16::from_le_bytes(first.body[2..4].try_into().unwrap()),
            )
        } else {
            (0, 0)
        };
        let count = self.counts.entry((first.command, group, slot)).or_default();
        *count += 1;
        let repeated = *count >= 2;
        let deliveries = self
            .capture_pedal
            .get_or_insert_with(gp200_sim::SimulatedPedal::captured)
            .receive(bytes, Duration::ZERO)?;
        let mut decoder = StreamDecoder::new();
        let mut assembly = Reassembler::default();
        let mut response = None;
        for delivery in deliveries {
            let gp200_sim::Delivery::Bytes { bytes, .. } = delivery else {
                panic!("unexpected fixture disconnect")
            };
            for event in decoder.feed(&bytes) {
                if let StreamEvent::Packet(packet) = event
                    && let Some(raw) = assembly.add(&packet, Duration::ZERO).unwrap()
                {
                    assert!(response.replace(raw).is_none());
                }
            }
        }
        let mut raw = response.expect("actual simulator must produce a complete capture reply");
        if first.command == 0 && repeated {
            let mut records = raw.records().to_vec();
            let offset = if forbidden_offset == 14 && group == 1 && slot == 255 {
                14
            } else {
                15
            };
            records.iter_mut().find(|r| r.command == 2).unwrap().body[offset] ^= 1;
            raw = RawTransfer::from_records(raw.kind(), &records).unwrap();
        }
        for packet in encode_transfer(&raw).unwrap() {
            for chunk in packet.chunks(97) {
                self.sink.receive(chunk);
            }
        }
        Ok(())
    }
}
fn fixture() -> (Directory, StudioApp, Arc<Mutex<Log>>) {
    let dir = Directory::new();
    let log = Arc::new(Mutex::new(Log::default()));
    let concrete = gp200_transport::MidiDeviceService::with_backend(
        Box::new(ReplayBackend {
            inner: Default::default(),
            log: log.clone(),
        }),
        gp200_transport::TransportConfig {
            minimum_request_spacing: Duration::ZERO,
            ..Default::default()
        },
        no_wake(),
    );
    let controller = AppController::with_demo(
        gp200_sim::captured_demo_snapshot(),
        AppPreferences::default(),
    )
    .unwrap();
    let mut services = RuntimeServices::with_midi_preferences_path(
        concrete,
        no_wake(),
        AppPreferences::default(),
        dir.0.join("preferences.json"),
    );
    services.install_library_worker(
        gp200_library::LibraryWorker::from_store(
            gp200_library::LibraryStore::open(dir.0.join("library")).unwrap(),
            no_wake(),
        )
        .unwrap(),
    );
    services.draft_recovery =
        Some(gp200_library::DraftRecoveryWorker::spawn(dir.0.join("drafts"), no_wake()).unwrap());
    (dir, StudioApp::with_services(controller, services), log)
}
fn frame(app: &mut StudioApp) {
    let (inputs, _) = app.services.pump();
    for original in app.services.take_unoffered_controls() {
        app.controller.settle_unoffered_device_control(original);
    }
    for input in inputs {
        let effects = app.controller.dispatch(input);
        app.services.enqueue(effects);
    }
    let events = app.services.pump_workflows();
    assert!(events.is_empty());
    app.advance_draft_recovery();
    let settled = app.firmware_producers_settled();
    app.firmware_coordinator.poll(
        &mut app.controller,
        &mut app.services,
        app.recovery_ui.last_saved.as_deref(),
        settled,
    );
}
fn until(app: &mut StudioApp, test: impl Fn(&StudioApp) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !test(app) {
        frame(app);
        assert!(
            Instant::now() < deadline,
            "phase {:?}, records {}, blockers {:?}, diagnostics {:?}",
            app.firmware_coordinator_view().phase,
            app.firmware_coordinator_view().records,
            app.firmware_coordinator_view().blockers,
            app.firmware_coordinator_view().diagnostics
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn ready(app: &mut StudioApp) {
    app.send(UiIntent::Connect(gp200_sim::simulator_port().selection));
    until(app, |a| {
        a.controller.state().connection == ConnectionState::Ready
    });
    app.send(UiIntent::RefreshDeviceGlobals);
    until(app, |a| {
        a.controller.state().device_globals.is_some() && a.services.firmware_queues_settled()
    });
    until(app, |a| {
        a.recovery_ui.last_saved.as_deref() == Some(&a.controller.recovery_session())
            && !a.recovery_ui.busy
    });
}
fn context() -> FirmwareBackupReviewContext {
    let device =
        FirmwareDeviceAssociation::named_local_record("GP-200", "explicit simulator test only")
            .unwrap();
    FirmwareBackupReviewContext {
        manual_version: Some(
            UserReportedFirmware::new(
                "V1.6.3",
                device.clone(),
                FirmwareReportDate::new(2026, 10, 11).unwrap(),
            )
            .unwrap(),
        ),
        device,
        destination: FirmwareBackupDestination {
            model: "GP-200".into(),
            version: FirmwareVersion::new(1, 8, 0).unwrap(),
            image_sha256: JobSha256::of(include_bytes!(
                "../../../tests/fixtures/device-assets/synthetic-firmware.bin"
            ))
            .hex(),
            restoration_profile: None,
        },
        local_material: vec![],
        preservation_uncertainty: "Installed audio unavailable".into(),
        recovery_limitations: "Replay only; contingent Windows recovery has not been exercised"
            .into(),
    }
}
#[test]
fn firmware_coordinator_concrete_proxy_freezes_and_returns_before_capture_without_offering_controls()
 {
    let (_dir, mut app, log) = fixture();
    ready(&mut app);
    let before = log.lock().unwrap().offers;
    app.begin_firmware_preparation().unwrap();
    assert!(matches!(
        app.services.device.try_submit(DeviceCommand::Discover),
        Err(SubmitError::Busy(DeviceCommand::Discover))
    ));
    app.send(UiIntent::Refresh(PresetTarget::EditBuffer));
    for _ in 0..3 {
        frame(&mut app);
    }
    assert_eq!(log.lock().unwrap().offers, before);
    assert!(app.firmware_close_blocked());
    app.return_from_firmware_preparation().unwrap();
    assert!(!app.firmware_close_blocked());
    assert!(!app.services.firmware_paused());
}
#[test]
fn firmware_coordinator_unavailable_update_does_not_relinquish_the_real_library() {
    let (_dir, mut app, log) = fixture();
    ready(&mut app);
    app.begin_firmware_preparation().unwrap();
    let before = log.lock().unwrap().offers;
    assert_eq!(
        app.request_firmware_update().unwrap_err().kind,
        ErrorKind::Unverified
    );
    assert_eq!(log.lock().unwrap().offers, before);
    app.request_firmware_history(None).unwrap();
    until(&mut app, |a| {
        a.firmware_coordinator_view().history.is_some()
    });
    assert!(app.services.library.is_some());
    assert_eq!(
        app.firmware_coordinator_view().phase,
        FirmwareCoordinatorPhase::Preparing
    );
    assert!(!app.firmware_coordinator_view().permits_flash);
    app.return_from_firmware_preparation().unwrap();
}
#[test]
fn firmware_coordinator_capture_requires_genuine_saved_session_and_preserves_rejected_context() {
    let (_dir, mut app, log) = fixture();
    ready(&mut app);
    app.begin_firmware_preparation().unwrap();
    let old = app.recovery_ui.last_saved.take();
    let before = log.lock().unwrap().offers;
    assert_eq!(
        app.capture_firmware_backup(context()).unwrap_err().kind,
        ErrorKind::Busy
    );
    assert_eq!(log.lock().unwrap().offers, before);
    app.recovery_ui.last_saved = old;
    let mut bad = context();
    bad.destination.model = "GP-200JR".into();
    assert_eq!(
        app.capture_firmware_backup(bad).unwrap_err().kind,
        ErrorKind::InvalidData
    );
    assert_eq!(
        app.firmware_coordinator_view().phase,
        FirmwareCoordinatorPhase::Preparing
    );
    app.return_from_firmware_preparation().unwrap();
}
#[test]
fn firmware_coordinator_conditional_561_read_capture_keeps_failed_archive_and_accepts_genuine_review_return()
 {
    let (dir, mut app, log) = fixture();
    ready(&mut app);
    log.lock().unwrap().capture_variation = Some(15);
    let epoch = app.controller.state().epoch;
    app.begin_firmware_preparation().unwrap();
    app.capture_firmware_backup(context()).unwrap();
    until(&mut app, |a| {
        matches!(
            a.firmware_coordinator_view().phase,
            FirmwareCoordinatorPhase::ReadOnlyReview | FirmwareCoordinatorPhase::RecoveryRequired
        )
    });
    let view = app.firmware_coordinator_view();
    assert_eq!(
        view.phase,
        FirmwareCoordinatorPhase::ReadOnlyReview,
        "{:?}",
        view.diagnostics
    );
    assert_eq!(log.lock().unwrap().capture_requests, 561);
    assert_eq!(view.records, 561);
    assert!(app.firmware_coordinator.conditional_consistency_observed);
    assert!(!app.firmware_coordinator.capture_io_failed);
    assert!(matches!(
        app.firmware_coordinator.outcome,
        Some(BackupAcquisition::Failed { .. })
    ));
    let evidence = view.capture.unwrap();
    assert!(evidence.closed && evidence.cancelled);
    assert_eq!(evidence.records, 563);
    assert!(!view.backup.unwrap().comparison_complete);
    let image_bytes =
        include_bytes!("../../../tests/fixtures/device-assets/synthetic-firmware.bin");
    let image_path = dir.0.join("conditional-study-image.bin");
    std::fs::write(&image_path, image_bytes).unwrap();
    app.firmware_ui.image = Some(InspectedFirmware {
        path: image_path,
        review: FirmwareReview::inspect(
            RetainedJobBlob::checked(
                AssetId("conditional image fixture".into()),
                Arc::from(&image_bytes[..]),
                JobSha256::of(image_bytes),
            )
            .unwrap(),
            None,
            &FirmwareBackupAssessment::default(),
        )
        .unwrap(),
    });
    app.prepare_local_firmware_review().unwrap();
    until(&mut app, |a| {
        a.firmware_coordinator_view().preparation.is_some()
    });
    let report = app.firmware_coordinator_view().preparation.unwrap();
    assert!(!report.backup.comparison_complete);
    assert!(!report.permits_flash && !report.completion_verified);
    let facts = app.firmware_coordinator.facts.clone().unwrap();
    let original_archive = std::fs::read(&facts.archive).unwrap();
    for _ in 0..10 {
        app.return_from_firmware_preparation().unwrap();
        until(&mut app, |a| {
            matches!(
                a.firmware_coordinator_view().phase,
                FirmwareCoordinatorPhase::Idle | FirmwareCoordinatorPhase::ReadOnlyReview
            )
        });
        if app.firmware_coordinator_view().phase == FirmwareCoordinatorPhase::Idle {
            break;
        }
        assert!(
            app.firmware_coordinator_view()
                .diagnostics
                .iter()
                .any(|s| s == "Admission gate unavailable"),
            "only actual zero-offer fixture contention may be deliberately retried"
        );
    }
    assert_eq!(
        app.firmware_coordinator_view().phase,
        FirmwareCoordinatorPhase::Idle
    );
    assert!(app.controller.state().epoch > epoch);
    assert_eq!(log.lock().unwrap().opens, 2);
    assert_eq!(log.lock().unwrap().closes, 1);
    assert!(!app.services.firmware_paused());
    assert_eq!(std::fs::read(&facts.archive).unwrap(), original_archive);
    assert!(
        reopen_firmware_capture_evidence(&facts.index)
            .unwrap()
            .cancelled
    );
    assert!(
        !reopen_firmware_backup_review(&facts.archive, &facts.sidecar)
            .unwrap()
            .comparison_complete
    );
    println!(
        "FW06_B_CONDITIONAL_REAL_REVIEW {}",
        serde_json::json!({"actual_reads":561,"original_evidence_records":563,"strict_outcome":"Failed","strict_archive_comparison_complete":false,"original_cancelled_summary":true,"private_transport_review_accepted":true,"original_archive_unchanged_after_genuine_return":true,"fresh_normal_opens":2,"permits_flash":false,"hardware_operations":0})
    );
}

#[test]
fn firmware_coordinator_conditional_observation_cannot_hide_actual_cancel_or_adjacent_unsupported_byte()
 {
    for actual_cancel in [false, true] {
        let (dir, mut app, log) = fixture();
        ready(&mut app);
        log.lock().unwrap().capture_variation = Some(if actual_cancel { 15 } else { 14 });
        app.begin_firmware_preparation().unwrap();
        app.capture_firmware_backup(context()).unwrap();
        if actual_cancel {
            until(&mut app, |a| a.firmware_coordinator_view().records >= 400);
            assert!(app.firmware_coordinator.conditional_consistency_observed);
            app.firmware_coordinator
                .device
                .as_ref()
                .unwrap()
                .capture
                .cancel();
        }
        until(&mut app, |a| {
            a.firmware_coordinator_view().phase == FirmwareCoordinatorPhase::RecoveryRequired
        });
        assert!(app.firmware_coordinator.conditional_consistency_observed);
        assert!(!app.firmware_coordinator.reviewable_capture_outcome());
        assert!(app.services.firmware_paused());
        assert!(app.return_from_firmware_preparation().is_err());
        assert!(app.prepare_local_firmware_review().is_err());
        assert!(LibraryStore::open(dir.0.join("library")).is_err());
        let before = log.lock().unwrap().offers;
        for _ in 0..8 {
            frame(&mut app);
        }
        assert_eq!(log.lock().unwrap().offers, before);
        assert_eq!(log.lock().unwrap().opens, 1);
        println!(
            "FW06_B_CONDITIONAL_FAILURE_DENIAL {}",
            serde_json::json!({"actual_cancel":actual_cancel,"unsupported_offset":if actual_cancel {None} else {Some(14)},"prior_admitted_masked_offset":15,"actual_reads":log.lock().unwrap().capture_requests,"normal_gate_held":true,"entry_packets":0,"ordinary_return_rejected":true,"hardware_operations":0})
        );
    }
}

#[test]
fn firmware_coordinator_genuine_capture_publish_revalidate_accept_and_same_store_return_reconnect()
{
    let (dir, mut app, log) = fixture();
    ready(&mut app);
    let epoch = app.controller.state().epoch;
    app.begin_firmware_preparation().unwrap();
    app.capture_firmware_backup(context()).unwrap();
    until(&mut app, |a| {
        matches!(
            a.firmware_coordinator_view().phase,
            FirmwareCoordinatorPhase::ReadOnlyReview | FirmwareCoordinatorPhase::RecoveryRequired
        )
    });
    let view = app.firmware_coordinator_view();
    assert_eq!(
        view.phase,
        FirmwareCoordinatorPhase::ReadOnlyReview,
        "blockers {:?}, diagnostics {:?}",
        view.blockers,
        view.diagnostics
    );
    assert!(view.records >= 561);
    let evidence = view.capture.unwrap();
    assert!(evidence.closed);
    assert!(!evidence.cancelled);
    assert!(!evidence.permits_flash());
    assert!(!view.permits_flash);
    assert!(!view.completion_verified);
    assert!(
        gp200_library::LibraryStore::open(dir.0.join("library")).is_err(),
        "original lock must stay held"
    );
    let image_bytes =
        include_bytes!("../../../tests/fixtures/device-assets/synthetic-firmware.bin");
    let path = dir.0.join("synthetic-source.bin");
    std::fs::write(&path, image_bytes).unwrap();
    app.firmware_ui.image = Some(InspectedFirmware {
        path,
        review: FirmwareReview::inspect(
            RetainedJobBlob::checked(
                AssetId("explicit synthetic image".into()),
                Arc::from(&image_bytes[..]),
                JobSha256::of(image_bytes),
            )
            .unwrap(),
            None,
            &FirmwareBackupAssessment::default(),
        )
        .unwrap(),
    });
    app.prepare_local_firmware_review().unwrap();
    until(&mut app, |a| {
        a.firmware_coordinator_view().phase == FirmwareCoordinatorPhase::ReadOnlyReview
    });
    let prepared = app.firmware_coordinator_view().preparation.unwrap();
    assert_eq!(prepared.image_sha256, JobSha256::of(image_bytes).hex());
    assert!(!prepared.permits_flash);
    assert!(!prepared.completion_verified);
    assert_eq!(
        app.request_firmware_update().unwrap_err().kind,
        ErrorKind::Unverified
    );
    // Revalidate the original durable file again: an altered byte cannot retain old review availability.
    let archive = app
        .firmware_coordinator
        .facts
        .as_ref()
        .unwrap()
        .archive
        .clone();
    let original = std::fs::read(&archive).unwrap();
    let mut altered = original.clone();
    altered[0] ^= 1;
    std::fs::write(&archive, altered).unwrap();
    app.prepare_local_firmware_review().unwrap();
    until(&mut app, |a| {
        a.firmware_coordinator_view().phase == FirmwareCoordinatorPhase::ReadOnlyReview
    });
    assert!(app.firmware_coordinator_view().preparation.is_none());
    std::fs::write(&archive, original).unwrap();
    app.return_from_firmware_preparation().unwrap();
    assert!(app.firmware_close_blocked());
    until(&mut app, |a| {
        a.firmware_coordinator_view().phase == FirmwareCoordinatorPhase::Idle
            || a.firmware_coordinator_view().phase == FirmwareCoordinatorPhase::RecoveryRequired
    });
    assert_eq!(
        app.firmware_coordinator_view().phase,
        FirmwareCoordinatorPhase::Idle,
        "blockers {:?}, diagnostics {:?}",
        app.firmware_coordinator_view().blockers,
        app.firmware_coordinator_view().diagnostics
    );
    assert!(app.controller.state().epoch > epoch);
    assert_eq!(log.lock().unwrap().opens, 2);
    assert_eq!(log.lock().unwrap().closes, 1);
    assert!(app.services.library.is_some());
    assert!(!app.services.firmware_paused());
    assert!(gp200_library::LibraryStore::open(dir.0.join("library")).is_err());
    until(&mut app, |a| {
        a.recovery_ui.last_saved.as_deref() == Some(&a.controller.recovery_session())
            && !a.recovery_ui.busy
    });
    app.begin_firmware_preparation().unwrap();
    app.capture_firmware_backup(context()).unwrap();
    until(&mut app, |a| {
        matches!(
            a.firmware_coordinator_view().phase,
            FirmwareCoordinatorPhase::ReadOnlyReview | FirmwareCoordinatorPhase::RecoveryRequired
        )
    });
    assert_eq!(
        app.firmware_coordinator_view().phase,
        FirmwareCoordinatorPhase::ReadOnlyReview
    );
    app.return_from_firmware_preparation().unwrap();
    until(&mut app, |a| {
        a.firmware_coordinator_view().phase == FirmwareCoordinatorPhase::Idle
    });
}
#[test]
fn firmware_coordinator_original_never_offered_connect_discover_are_settled_before_seal() {
    let (dir, mut app, log) = fixture();
    // Admit the real current-epoch globals prerequisite before queuing normal
    // controls. Neither queued command has reached the original MIDI worker.
    ready(&mut app);
    assert!(normal_firmware_globals_current(app.controller.state()));
    let epoch = app.controller.state().epoch;
    let offers = log.lock().unwrap().offers;
    app.send(UiIntent::DiscoverDevices);
    app.send(UiIntent::Connect(gp200_sim::simulator_port().selection));
    assert!(!app.services.firmware_queues_settled());
    app.begin_firmware_preparation().unwrap();
    frame(&mut app);
    {
        let log = log.lock().unwrap();
        assert_eq!(log.opens, 1);
        assert_eq!(log.connect_attempts, 1);
        assert_eq!(log.closes, 0);
        assert_eq!(log.offers, offers);
    }
    assert_eq!(app.controller.state().epoch, epoch);
    assert_eq!(app.controller.state().connection, ConnectionState::Offline);
    assert!(app.services.firmware_queues_settled());
    assert!(
        !app.firmware_coordinator_view()
            .blockers
            .iter()
            .any(|v| v.contains("PendingConnect"))
    );
    app.return_from_firmware_preparation().unwrap();
    assert_eq!(
        app.firmware_coordinator_view().phase,
        FirmwareCoordinatorPhase::Idle
    );
    assert!(!app.services.firmware_paused());
    assert!(!app.controller.firmware_editor_blocks_normal_actions());
    assert!(app.services.library.is_some());
    assert!(LibraryStore::open(dir.0.join("library")).is_err());
}

#[test]
fn firmware_coordinator_missing_globals_preserves_queued_connect_discover_for_normal_service() {
    let (dir, mut app, log) = fixture();
    let epoch = app.controller.state().epoch;
    app.send(UiIntent::DiscoverDevices);
    app.send(UiIntent::Connect(gp200_sim::simulator_port().selection));
    assert_eq!(
        app.controller.state().connection,
        ConnectionState::Connecting
    );
    assert!(!app.services.firmware_queues_settled());
    assert!(!normal_firmware_globals_current(app.controller.state()));

    let error = app.begin_firmware_preparation().unwrap_err();
    assert_eq!(error.kind, ErrorKind::Busy);
    assert!(
        error
            .message
            .contains("Read global settings in this connection")
    );
    assert_eq!(
        app.firmware_coordinator_view().phase,
        FirmwareCoordinatorPhase::Idle
    );
    assert!(!app.services.firmware_paused());
    assert!(!app.controller.firmware_editor_blocks_normal_actions());
    assert_eq!(
        app.controller.state().connection,
        ConnectionState::Connecting
    );
    assert_eq!(app.controller.state().epoch, epoch);
    assert!(!app.services.firmware_queues_settled());
    assert_eq!(app.services.take_unoffered_controls().count(), 0);
    assert!(app.services.library.is_some());
    assert!(app.services.workflow_available());
    assert!(LibraryStore::open(dir.0.join("library")).is_err());
    {
        let log = log.lock().unwrap();
        assert_eq!(log.opens, 0);
        assert_eq!(log.connect_attempts, 0);
        assert_eq!(log.offers, 0);
        assert_eq!(log.closes, 0);
    }

    // Rejection happened before runtime freeze: the same original worker can
    // still offer both real queued controls through the normal pump.
    until(&mut app, |app| {
        app.controller.state().connection == ConnectionState::Ready
            && app.services.firmware_queues_settled()
    });
    assert!(!app.controller.state().ports.is_empty());
    assert_eq!(log.lock().unwrap().opens, 1);
    assert_eq!(log.lock().unwrap().connect_attempts, 1);
    assert_eq!(log.lock().unwrap().closes, 0);
    assert_eq!(
        app.firmware_coordinator_view().phase,
        FirmwareCoordinatorPhase::Idle
    );
    assert!(!app.services.firmware_paused());
    assert!(LibraryStore::open(dir.0.join("library")).is_err());
}

#[test]
fn firmware_coordinator_actual_writer_exit_and_archival_stop_are_observed_without_timeout_or_replacement()
 {
    let (_dir, mut app, _log) = fixture();
    ready(&mut app);
    app.begin_firmware_preparation().unwrap();
    app.request_firmware_history(None).unwrap();
    app.services.library.as_ref().unwrap().shutdown();
    until(&mut app, |a| {
        a.firmware_coordinator_view().phase == FirmwareCoordinatorPhase::RecoveryRequired
    });
    assert!(app.services.firmware_paused());
    assert!(app.firmware_close_blocked());
    assert!(!app.firmware_coordinator_view().can_retry_library_restore);
    assert!(app.request_firmware_history(None).is_err());
    assert!(app.return_from_firmware_preparation().is_err());
    let (_dir, mut app, log) = fixture();
    ready(&mut app);
    app.begin_firmware_preparation().unwrap();
    app.capture_firmware_backup(context()).unwrap();
    app.firmware_coordinator
        .library
        .as_ref()
        .unwrap()
        .backup
        .cancel();
    until(&mut app, |a| {
        a.firmware_coordinator_view().phase == FirmwareCoordinatorPhase::RecoveryRequired
    });
    let offers = log.lock().unwrap().offers;
    for _ in 0..5 {
        frame(&mut app);
    }
    assert_eq!(log.lock().unwrap().offers, offers);
    assert!(app.services.firmware_paused());
    until(&mut app, |a| !a.firmware_coordinator.originals.is_empty());
    assert_eq!(log.lock().unwrap().offers, offers);
    assert!(app.controller.firmware_editor_blocks_normal_actions());
}

#[test]
fn firmware_coordinator_actual_returned_store_retry_requires_explicit_fresh_normal_check() {
    let (dir, mut app, log) = fixture();
    ready(&mut app);
    app.begin_firmware_preparation().unwrap();
    let epoch = app.controller.state().epoch;
    app.firmware_coordinator
        .begin_retirement(StorePurpose::Recovery)
        .unwrap();
    // Consume the genuine runtime reply and joined store, before its restoration is offered.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        app.firmware_coordinator.poll_runtime(&mut app.controller);
        if app.firmware_coordinator.retirement.is_some() {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    let store = loop {
        if let Some(result) = app
            .firmware_coordinator
            .retirement
            .as_mut()
            .unwrap()
            .try_take_result()
        {
            break result.unwrap();
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    };
    app.firmware_coordinator.retirement = None;
    app.services.retire_library_clients();
    app.firmware_coordinator.library = None;
    app.firmware_coordinator.retained_store = Some(store);
    app.firmware_coordinator.owner_return_observed = true;
    app.firmware_coordinator.fail(
        &mut app.controller,
        "Actual joined store held locally for explicit restore fixture".into(),
    );
    assert!(gp200_library::LibraryStore::open(dir.0.join("library")).is_err());
    assert!(app.firmware_coordinator_view().can_retry_library_restore);
    assert!(!app.firmware_coordinator_view().can_return_to_editing);
    app.retry_firmware_library_restore().unwrap();
    assert!(!app.firmware_coordinator_view().can_retry_library_restore);
    assert!(app.retry_firmware_library_restore().is_err());
    until(&mut app, |a| a.firmware_coordinator.library_restored);
    assert_eq!(
        app.firmware_coordinator_view().phase,
        FirmwareCoordinatorPhase::RecoveryRequired
    );
    assert!(app.services.firmware_paused());
    assert_eq!(log.lock().unwrap().opens, 1);
    assert!(app.firmware_coordinator_view().can_return_to_editing);
    app.return_from_firmware_preparation().unwrap();
    until(&mut app, |a| {
        matches!(
            a.firmware_coordinator_view().phase,
            FirmwareCoordinatorPhase::Idle | FirmwareCoordinatorPhase::RecoveryRequired
        )
    });
    assert_eq!(
        app.firmware_coordinator_view().phase,
        FirmwareCoordinatorPhase::Idle
    );
    assert!(app.controller.state().epoch > epoch);
    assert_eq!(log.lock().unwrap().opens, 2);
    assert!(!app.services.firmware_paused());
    assert!(gp200_library::LibraryStore::open(dir.0.join("library")).is_err());
}

#[test]
fn firmware_coordinator_normal_capture_helper_failure_retry_preserves_original_return_in_both_orders()
 {
    for accepted_return in [false, true] {
        let (dir, mut app, log) = fixture();
        ready(&mut app);
        let epoch = app.controller.state().epoch;
        app.begin_firmware_preparation().unwrap();
        let original_source = app
            .firmware_coordinator
            .source
            .as_ref()
            .unwrap()
            .recovery
            .clone();
        app.capture_firmware_backup(context()).unwrap();
        until(&mut app, |a| {
            matches!(
                a.firmware_coordinator_view().phase,
                FirmwareCoordinatorPhase::ReadOnlyReview
                    | FirmwareCoordinatorPhase::RecoveryRequired
            )
        });
        assert_eq!(
            app.firmware_coordinator_view().phase,
            FirmwareCoordinatorPhase::ReadOnlyReview
        );
        let failure_gate = Arc::new(AtomicBool::new(false));
        app.services
            .fail_next_library_restoration_before_worker_offer(failure_gate.clone());
        app.return_from_firmware_preparation().unwrap();

        // Consume only actual runtime retirement and joined-store results. The
        // helper owns that exact store while the original ReturnToNormal waits.
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.firmware_coordinator.restoration.is_none() {
            app.firmware_coordinator.poll_runtime(&mut app.controller);
            app.firmware_coordinator
                .poll_retirement(&mut app.controller, &mut app.services);
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(app.firmware_coordinator.capture_store_returned);
        assert!(matches!(
            app.firmware_coordinator.control,
            Some(FirmwareBackupCaptureCommand::ReturnToNormal)
        ));
        if accepted_return {
            app.firmware_coordinator.submit_control(&mut app.controller);
            assert!(app.firmware_coordinator.capture_return_submitted);
            assert!(app.firmware_coordinator.control.is_none());
        }
        failure_gate.store(true, Ordering::Release);
        while app.firmware_coordinator.retained_store.is_none() {
            app.firmware_coordinator
                .poll_restore(&mut app.controller, &mut app.services);
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            app.firmware_coordinator_view().phase,
            FirmwareCoordinatorPhase::RecoveryRequired
        );
        assert_eq!(
            app.firmware_coordinator.capture_return_submitted,
            accepted_return
        );
        assert!(!app.firmware_coordinator.owner_return_observed);
        assert!(app.firmware_coordinator_view().can_retry_library_restore);
        assert!(!app.firmware_coordinator_view().can_return_to_editing);
        assert!(app.services.firmware_paused());
        assert!(app.controller.firmware_editor_blocks_normal_actions());
        assert!(gp200_library::LibraryStore::open(dir.0.join("library")).is_err());
        if accepted_return {
            // Receive the genuine boundary only after the helper failure has
            // stopped admission. It must remain an original, not a live claim.
            until(&mut app, |a| {
                a.firmware_coordinator
                    .originals
                    .iter()
                    .any(|e| matches!(e, FirmwareBackupCaptureEvent::NormalRefreshRequired { .. }))
            });
            assert!(app.firmware_coordinator.return_boundary.is_none());
        } else {
            assert!(matches!(
                app.firmware_coordinator.control,
                Some(FirmwareBackupCaptureCommand::ReturnToNormal)
            ));
        }
        app.retry_firmware_library_restore().unwrap();
        until(&mut app, |a| a.firmware_coordinator.library_restored);
        assert_eq!(
            app.firmware_coordinator_view().phase,
            FirmwareCoordinatorPhase::RecoveryRequired
        );
        assert!(app.firmware_coordinator_view().can_return_to_editing);
        assert!(!app.firmware_coordinator_view().can_retry_library_restore);
        assert_eq!(
            app.firmware_coordinator.source.as_ref().unwrap().recovery,
            original_source
        );
        assert_eq!(log.lock().unwrap().opens, 1);
        assert!(app.services.firmware_paused());
        assert!(app.controller.firmware_editor_blocks_normal_actions());
        app.return_from_firmware_preparation().unwrap();
        until(&mut app, |a| {
            matches!(
                a.firmware_coordinator_view().phase,
                FirmwareCoordinatorPhase::Idle | FirmwareCoordinatorPhase::RecoveryRequired
            )
        });
        assert_eq!(
            app.firmware_coordinator_view().phase,
            FirmwareCoordinatorPhase::Idle,
            "original return accepted {accepted_return}: {:?}",
            app.firmware_coordinator_view().diagnostics
        );
        assert!(app.controller.state().epoch > epoch);
        assert!(app.controller.state().identity.is_some());
        assert!(app.controller.state().device_globals.is_some());
        assert_eq!(log.lock().unwrap().opens, 2);
        assert_eq!(log.lock().unwrap().closes, 1);
        assert!(!app.services.firmware_paused());
        assert!(!app.controller.firmware_editor_blocks_normal_actions());
        assert!(gp200_library::LibraryStore::open(dir.0.join("library")).is_err());
        assert!(!app.firmware_coordinator_view().permits_flash);
        assert!(!app.firmware_coordinator_view().completion_verified);
    }
}

#[test]
fn firmware_coordinator_original_midi_worker_panic_after_accepted_capture_is_observed_and_held() {
    let (dir, mut app, log) = fixture();
    ready(&mut app);
    let saved = app.recovery_ui.last_saved.clone();
    app.begin_firmware_preparation().unwrap();
    log.lock().unwrap().panic_capture_observation = true;
    app.capture_firmware_backup(context()).unwrap();
    assert!(
        app.firmware_coordinator.control.is_none(),
        "genuine Begin was accepted once"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.firmware_coordinator_view().phase != FirmwareCoordinatorPhase::RecoveryRequired {
        frame(&mut app);
        assert!(
            Instant::now() < deadline,
            "accepted control must not remain pending after actual worker exit"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(app.firmware_coordinator.device.as_ref().unwrap().stopped());
    assert_eq!(log.lock().unwrap().capture_observations, 1);
    assert!(
        app.firmware_coordinator_view()
            .diagnostics
            .iter()
            .any(|m| m.contains("panicked"))
    );
    let offers = log.lock().unwrap().offers;
    for _ in 0..20 {
        frame(&mut app);
    }
    assert_eq!(log.lock().unwrap().offers, offers);
    assert_eq!(log.lock().unwrap().opens, 1);
    assert!(app.firmware_coordinator.admission_stopped);
    assert!(app.firmware_coordinator.retained_store.is_none());
    assert!(app.firmware_coordinator.restoration.is_none());
    assert!(
        app.services.library.is_some(),
        "original library remains owned"
    );
    assert!(gp200_library::LibraryStore::open(dir.0.join("library")).is_err());
    assert_eq!(app.recovery_ui.last_saved, saved);
    assert!(app.services.firmware_paused());
    assert!(app.controller.firmware_editor_blocks_normal_actions());
    assert!(!app.firmware_coordinator_view().can_return_to_editing);
    assert!(app.return_from_firmware_preparation().is_err());
    assert!(app.capture_firmware_backup(context()).is_err());
    assert!(app.request_firmware_update().is_err());
    assert!(!app.firmware_coordinator_view().permits_flash);
    assert!(!app.firmware_coordinator_view().completion_verified);
}

#[test]
fn firmware_coordinator_restoration_continuation_is_consumed_before_genuine_fresh_connect_failure()
{
    let (dir, mut app, log) = fixture();
    ready(&mut app);
    app.begin_firmware_preparation().unwrap();
    let original = app
        .firmware_coordinator
        .source
        .as_ref()
        .unwrap()
        .recovery
        .clone();
    app.capture_firmware_backup(context()).unwrap();
    until(&mut app, |a| {
        matches!(
            a.firmware_coordinator_view().phase,
            FirmwareCoordinatorPhase::ReadOnlyReview | FirmwareCoordinatorPhase::RecoveryRequired
        )
    });
    assert_eq!(
        app.firmware_coordinator_view().phase,
        FirmwareCoordinatorPhase::ReadOnlyReview
    );
    app.services
        .fail_next_library_restoration_before_worker_offer(Arc::new(AtomicBool::new(true)));
    app.return_from_firmware_preparation().unwrap();
    until(&mut app, |a| {
        a.firmware_coordinator.retained_store.is_some()
    });
    assert!(app.firmware_coordinator.capture_restore_retry);
    app.retry_firmware_library_restore().unwrap();
    until(&mut app, |a| a.firmware_coordinator.library_restored);
    assert!(app.firmware_coordinator_view().can_return_to_editing);
    log.lock().unwrap().fail_connect = true;
    app.return_from_firmware_preparation().unwrap();
    assert!(!app.firmware_coordinator.capture_restore_retry);
    until(&mut app, |a| {
        a.firmware_coordinator_view().phase == FirmwareCoordinatorPhase::RecoveryRequired
    });
    assert_eq!(
        log.lock().unwrap().connect_attempts,
        2,
        "the genuine service attempted fresh Connect"
    );
    assert_eq!(log.lock().unwrap().opens, 1);
    assert!(
        !app.firmware_coordinator.device.as_ref().unwrap().stopped(),
        "the MIDI worker survives this genuine backend error"
    );
    assert!(
        app.firmware_coordinator.return_boundary.is_none(),
        "the original boundary was consumed"
    );
    assert!(!app.firmware_coordinator.capture_return_submitted);
    assert!(!app.firmware_coordinator_view().can_return_to_editing);
    assert!(
        app.return_from_firmware_preparation().is_err(),
        "a consumed return cannot falsely enter waiting again"
    );
    assert_eq!(
        app.firmware_coordinator_view().phase,
        FirmwareCoordinatorPhase::RecoveryRequired
    );
    let offers = log.lock().unwrap().offers;
    for _ in 0..20 {
        frame(&mut app);
    }
    assert_eq!(log.lock().unwrap().offers, offers);
    assert_eq!(log.lock().unwrap().connect_attempts, 2);
    assert_eq!(
        app.firmware_coordinator.source.as_ref().unwrap().recovery,
        original
    );
    assert!(app.services.firmware_paused());
    assert!(app.controller.firmware_editor_blocks_normal_actions());
    assert!(app.services.library.is_some());
    assert!(gp200_library::LibraryStore::open(dir.0.join("library")).is_err());
    assert!(!app.firmware_coordinator_view().permits_flash);
    assert!(!app.firmware_coordinator_view().completion_verified);
}

#[test]
fn firmware_coordinator_close_interception_and_held_file_choices_preserve_ownership() {
    let (_dir, mut app, _log) = fixture();
    ready(&mut app);
    let ctx = egui::Context::default();
    app.begin_firmware_preparation().unwrap();
    assert!(!app.firmware_close_request_pending());
    let mut output = ctx.run_ui(Default::default(), |ui| {
        app.request_application_close(ui.ctx())
    });
    assert!(output.viewport_output.values().any(|v| {
        v.commands
            .iter()
            .any(|c| matches!(c, egui::ViewportCommand::CancelClose))
    }));
    output.textures_delta.clear();
    assert!(app.firmware_close_request_pending());
    app.dismiss_firmware_close_request();
    assert!(!app.firmware_close_request_pending());
    assert!(app.firmware_close_blocked());
    app.receive_file_selection(runtime::FileSelection {
        purpose: FilePurpose::ImportPresets,
        path: Some("held-original-file.prst".into()),
    });
    assert_eq!(app.pending_firmware_file_choices(), 1);
    assert!(!app.firmware_producers_settled());
    assert!(app.apply_pending_firmware_file_choices().is_err());
    app.return_from_firmware_preparation().unwrap();
    for _ in 0..3 {
        frame(&mut app);
    }
    assert_eq!(app.pending_firmware_file_choices(), 1);
    app.dismiss_pending_firmware_file_choices();
    assert_eq!(app.pending_firmware_file_choices(), 0);
    assert!(!app.firmware_close_blocked());
}

#[test]
fn firmware_coordinator_actual_workflow_client_is_private_and_generation_gated() {
    let dir = Directory::new();
    let gate = Arc::new(crate::firmware_device_bridge::RuntimeAdmission::default());
    let bundle = crate::firmware_device_bridge::LibraryBundle::from_worker(
        gp200_library::LibraryWorker::from_store(
            gp200_library::LibraryStore::open(dir.0.join("library")).unwrap(),
            no_wake(),
        )
        .unwrap(),
        gate.clone(),
    );
    let lease = gate.freeze().unwrap();
    let original = gp200_library::WorkflowCommand {
        intent: IntentId(871),
        action: gp200_library::WorkflowAction::ListBackups,
    };
    let retained = match bundle.workflows.try_submit(original) {
        Err(SubmitError::Busy(command)) => command,
        _ => panic!("Frozen actual workflow must preserve its original command"),
    };
    assert_eq!(retained.intent, IntentId(871));
    assert!(matches!(
        retained.action,
        gp200_library::WorkflowAction::ListBackups
    ));
    assert!(bundle.workflows.try_next_event().is_none());
    gate.release(&lease).unwrap();
    bundle.workflows.try_submit(retained).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(event) = bundle.workflows.try_next_event() {
            assert_eq!(event.intent, IntentId(871));
            assert!(event.result.is_ok());
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    let (_dir, app, _log) = fixture();
    assert!(app.services.workflows.is_none());
    assert!(app.services.workflow_available());
}

fn descriptive_fixture_job() -> Arc<ReviewedFirmwareUpdate> {
    let bytes = include_bytes!("../../../tests/fixtures/device-assets/synthetic-firmware.bin");
    let context = FirmwareUpdateContextDto {
        profile_id: "descriptive fixture, no physical issuer".into(),
        hardware_policy: "unavailable".into(),
        update_mode: "fixture".into(),
        normal: None,
        versions: vec![],
        backup_objects: vec![],
        loss_items: vec![],
        power_statement: "fixture".into(),
        connection_statement: "fixture".into(),
    };
    let materials = [
        FirmwareMaterialKind::Profile,
        FirmwareMaterialKind::Evidence,
        FirmwareMaterialKind::BackupArchive,
        FirmwareMaterialKind::BackupInventory,
        FirmwareMaterialKind::RestorePlan,
        FirmwareMaterialKind::Recovery,
        FirmwareMaterialKind::ApprovalStatement,
    ]
    .into_iter()
    .map(|kind| FirmwareMaterialInput {
        kind,
        bytes: Arc::from(&b"descriptive fixture only; no physical permission"[..]),
    })
    .collect();
    Arc::new(
        ReviewedFirmwareUpdate::prepare(
            RetainedJobBlob::checked(
                AssetId("synthetic immutable fixture".into()),
                Arc::from(&bytes[..]),
                JobSha256::of(bytes),
            )
            .unwrap(),
            context,
            materials,
            1,
        )
        .unwrap(),
    )
}
#[test]
fn firmware_coordinator_actual_service_rejection_returns_original_store_and_requires_deliberate_refresh()
 {
    let (dir, mut app, log) = fixture();
    ready(&mut app);
    let epoch = app.controller.state().epoch;
    app.begin_firmware_preparation().unwrap();
    assert_eq!(
        app.request_firmware_update().unwrap_err().kind,
        ErrorKind::Unverified
    );
    // Test only the dormant owner reconciliation branch through a genuine rejected service request.
    // This private fixture cannot issue PhysicalSeal or the transport-only mock authority.
    let reviewed = descriptive_fixture_job();
    app.firmware_coordinator.reviewed = Some(reviewed.clone());
    let offers = log.lock().unwrap().offers;
    let source = app.firmware_coordinator.source.clone().unwrap();
    let mut before = {
        let view = app.firmware_coordinator_view();
        (view.diagnostics.len(), view.omitted_diagnostics)
    };
    assert_eq!(before.1, 0);
    // This fixture pumps faster than native frames. An actual try-lock may
    // reject relinquishment before the dormant service branch is reached.
    // Deliberately reissue only a freshly acknowledged zero-offer contention;
    // production admission and ownership requests never retry automatically.
    for attempt in 0..10 {
        app.firmware_coordinator
            .begin_retirement(StorePurpose::Update)
            .unwrap();
        until(&mut app, |a| {
            a.firmware_coordinator.owner_return_observed && a.firmware_coordinator.library_restored
                || a.firmware_coordinator_view().phase == FirmwareCoordinatorPhase::ReadOnlyReview
        });
        if app.firmware_coordinator.owner_return_observed
            && app.firmware_coordinator.library_restored
        {
            break;
        }
        let view = app.firmware_coordinator_view();
        let fresh = view.diagnostics.get(before.0..).unwrap_or_default();
        assert_eq!(view.phase, FirmwareCoordinatorPhase::ReadOnlyReview);
        assert!(
            view.omitted_diagnostics == 0
                && before.1 == 0
                && !fresh.is_empty()
                && fresh.iter().all(|reason| matches!(
                    reason.as_str(),
                    "Admission gate unavailable" | "Capture route gate unavailable"
                )),
            "only fresh actual zero-offer contention is retried; attempt {attempt}, fresh {fresh:?}, diagnostics {:?}, omitted {} -> {}",
            view.diagnostics,
            before.1,
            view.omitted_diagnostics
        );
        assert_eq!(log.lock().unwrap().offers, offers);
        assert_eq!(log.lock().unwrap().opens, 1);
        assert_eq!(log.lock().unwrap().closes, 0);
        assert_eq!(view.records, 0);
        assert!(app.firmware_coordinator.runtime_request.is_none());
        assert!(app.firmware_coordinator.retirement.is_none());
        assert!(app.firmware_coordinator.restoration.is_none());
        assert!(app.firmware_coordinator.retained_store.is_none());
        assert!(app.firmware_coordinator.purpose.is_none());
        assert!(!app.firmware_coordinator.owner_return_observed);
        assert!(!app.firmware_coordinator.library_restored);
        assert_eq!(app.firmware_coordinator.source.as_ref(), Some(&source));
        assert!(Arc::ptr_eq(
            app.firmware_coordinator.reviewed.as_ref().unwrap(),
            &reviewed
        ));
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        assert_eq!(
            app.firmware_coordinator.library.as_ref().unwrap().root,
            dir.0.join("library")
        );
        assert!(gp200_library::LibraryStore::open(dir.0.join("library")).is_err());
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        assert!(
            !app.firmware_coordinator
                .device
                .as_ref()
                .unwrap()
                .has_owner_ticket()
        );
        before = (view.diagnostics.len(), view.omitted_diagnostics);
        if attempt < 9 {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    assert!(
        app.firmware_coordinator.owner_return_observed && app.firmware_coordinator.library_restored,
        "Original store handoff did not reach the actual rejected-service return after ten deliberate fixture attempts"
    );
    until(&mut app, |a| {
        a.firmware_coordinator.owner_return_observed && a.firmware_coordinator.library_restored
    });
    assert_eq!(
        app.firmware_coordinator_view().phase,
        FirmwareCoordinatorPhase::RecoveryRequired
    );
    assert_eq!(log.lock().unwrap().offers, offers);
    assert_eq!(log.lock().unwrap().opens, 1);
    assert!(!app.firmware_coordinator_view().completion_verified);
    assert!(app.services.firmware_paused());
    assert!(gp200_library::LibraryStore::open(dir.0.join("library")).is_err());
    app.return_from_firmware_preparation().unwrap();
    until(&mut app, |a| {
        matches!(
            a.firmware_coordinator_view().phase,
            FirmwareCoordinatorPhase::Idle | FirmwareCoordinatorPhase::RecoveryRequired
        )
    });
    assert_eq!(
        app.firmware_coordinator_view().phase,
        FirmwareCoordinatorPhase::Idle
    );
    assert!(app.controller.state().epoch > epoch);
    assert_eq!(log.lock().unwrap().opens, 2);
}

#[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
fn entry_study_retirement_diagnostic_baseline(app: &StudioApp) -> (usize, usize) {
    let view = app.firmware_coordinator_view();
    (view.diagnostics.len(), view.omitted_diagnostics)
}
#[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
fn entry_study_wait_actual_owner_return(app: &mut StudioApp, mut before: (usize, usize)) {
    // A fixture pumps much faster than native GUI frames. The library may
    // honestly return either existing try-lock Blocked result before retirement.
    // Deliberately request again only for fresh zero-offer mutex contention;
    // the experimental runner itself performs no automatic retry.
    for _ in 0..10 {
        until(app, |a| {
            a.firmware_coordinator_view().phase == FirmwareCoordinatorPhase::ReadOnlyReview
                || a.firmware_coordinator.owner_return_observed
                    && a.firmware_coordinator.library_restored
                    && a.firmware_coordinator_view().phase
                        == FirmwareCoordinatorPhase::RecoveryRequired
        });
        if app.firmware_coordinator.owner_return_observed
            && app.firmware_coordinator.library_restored
            && app.firmware_coordinator_view().phase == FirmwareCoordinatorPhase::RecoveryRequired
        {
            return;
        }
        assert!(
            app.firmware_coordinator.entry_authorization.is_some(),
            "No original exact authorization left to retry before an offer"
        );
        let view = app.firmware_coordinator_view();
        let fresh = view.diagnostics.get(before.0..).unwrap_or_default();
        assert!(
            view.omitted_diagnostics == before.1
                && !fresh.is_empty()
                && fresh.iter().all(|reason| matches!(
                    reason.as_str(),
                    "Admission gate unavailable" | "Capture route gate unavailable"
                )),
            "only fresh actual zero-offer mutex contention is retried; phase {:?}, fresh reasons {:?}, all diagnostics {:?}, omitted {} -> {}, runtime request pending {}, retirement pending {}, store returned {}, library restored {}",
            view.phase,
            fresh,
            view.diagnostics,
            before.1,
            view.omitted_diagnostics,
            app.firmware_coordinator.runtime_request.is_some(),
            app.firmware_coordinator.retirement.is_some(),
            app.firmware_coordinator.owner_return_observed,
            app.firmware_coordinator.library_restored,
        );
        before = entry_study_retirement_diagnostic_baseline(app);
        app.firmware_coordinator
            .begin_retirement(StorePurpose::EntryStudy)
            .unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("Library fixture remained contended before any firmware ownership request");
}
#[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
#[test]
fn entry_study_actual_capture_private_match_and_original_store_return_keep_normal_gate_held() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, mut app, log) = fixture();
    ready(&mut app);
    log.lock().unwrap().capture_variation = Some(15);
    let pin_path = dir.0.join("review-source.txt");
    std::fs::write(
        &pin_path,
        b"Test source and recovery only; no hardware authority",
    )
    .unwrap();
    let bytes = std::fs::read(&pin_path).unwrap();
    let pin = EntryStudyFilePin {
        path: pin_path,
        bytes: bytes.len() as u64,
        sha256: JobSha256::of(&bytes).hex(),
    };
    let audit = dir.0.join("entry-audit");
    std::fs::create_dir(&audit).unwrap();
    std::fs::set_permissions(&audit, std::fs::Permissions::from_mode(0o700)).unwrap();
    let inputs = EntryStudyInputs {
        sources: vec![pin.clone()],
        loss: pin.clone(),
        recovery: pin.clone(),
        vendor_guidance: pin,
        audit_directory: audit,
        observation_seconds: 1,
        approval_wait_seconds: 30,
    };
    assert!(app.prepare_entry_study(inputs.clone()).is_err());
    app.begin_firmware_preparation().unwrap();
    app.capture_firmware_backup(context()).unwrap();
    until(&mut app, |a| {
        a.firmware_coordinator_view().phase == FirmwareCoordinatorPhase::ReadOnlyReview
    });
    let image_bytes =
        include_bytes!("../../../tests/fixtures/device-assets/synthetic-firmware.bin");
    let image_path = dir.0.join("study-image.bin");
    std::fs::write(&image_path, image_bytes).unwrap();
    app.firmware_ui.image = Some(InspectedFirmware {
        path: image_path,
        review: FirmwareReview::inspect(
            RetainedJobBlob::checked(
                AssetId("test image".into()),
                Arc::from(&image_bytes[..]),
                JobSha256::of(image_bytes),
            )
            .unwrap(),
            None,
            &FirmwareBackupAssessment::default(),
        )
        .unwrap(),
    });
    app.prepare_local_firmware_review().unwrap();
    until(&mut app, |a| {
        a.firmware_coordinator_view().preparation.is_some()
    });
    app.prepare_entry_study(inputs).unwrap();
    until(&mut app, |a| a.entry_study_review().is_some());
    let review = app.entry_study_review().unwrap();
    assert!(review.genuine_preparation["capture_evidence"]["cancelled"] == true);
    assert!(review.genuine_preparation["proposal"]["review"]["comparison_complete"] == false);
    assert_eq!(review.reservation.epoch, app.controller.state().epoch);
    assert_eq!(review.captured_materials.len(), 3);
    assert!(review.genuine_preparation["source_identity"]["firmware"].is_null());
    let digest = JobSha256::of(&serde_json::to_vec(&review).unwrap()).hex();
    let before = log.lock().unwrap().offers;
    let retirement_diagnostics = entry_study_retirement_diagnostic_baseline(&app);
    app.execute_entry_study(format!("EXECUTE ENTRY ONCE {digest}"))
        .unwrap();
    // First wait for the real exact-signal helper to finish and request genuine
    // relinquishment. Its phase remains ReadOnlyReview while hashing inputs.
    until(&mut app, |a| {
        a.firmware_coordinator.entry_preparation.is_none()
    });
    entry_study_wait_actual_owner_return(&mut app, retirement_diagnostics);
    let view = app.firmware_coordinator_view();
    assert_eq!(view.phase, FirmwareCoordinatorPhase::RecoveryRequired);
    assert!(!view.can_return_to_editing);
    assert!(!view.permits_flash);
    assert!(!view.completion_verified);
    assert!(app.firmware_close_blocked());
    // The replay session intentionally lacks a physical entry handoff. The real
    // service has still checked its privately accepted current capture and
    // returned the genuinely relinquished store, without offering entry bytes.
    assert_eq!(log.lock().unwrap().offers, before);
    assert!(
        view.diagnostics
            .iter()
            .any(|message| message.contains("no accepted updater ownership binding")),
        "the genuine private capture must pass before this replay-only handoff rejection: {:?}",
        view.diagnostics
    );
    assert_eq!(log.lock().unwrap().opens, 1);
    assert!(LibraryStore::open(dir.0.join("library")).is_err());
    assert!(app.return_from_firmware_preparation().is_err());
    assert!(matches!(
        app.services.device.try_submit(DeviceCommand::Discover),
        Err(SubmitError::Busy(_))
    ));
    app.send(UiIntent::Connect(gp200_sim::simulator_port().selection));
    for _ in 0..4 {
        frame(&mut app);
    }
    assert_eq!(log.lock().unwrap().opens, 1);
    assert_eq!(log.lock().unwrap().offers, before);
    app.request_firmware_history(None).unwrap();
    until(&mut app, |a| {
        a.firmware_coordinator_view().history.is_some()
    });
    assert!(
        app.firmware_coordinator_view()
            .history
            .unwrap()
            .attempts
            .is_empty()
    );
    println!(
        "FW06_B_REAL_CAPTURE_GATE {}",
        serde_json::json!({"genuine_capture_records":review.genuine_preparation["capture_evidence"]["records"],"same_store_returned":true,"normal_opens":1,"normal_offers_after_entry_request":log.lock().unwrap().offers-before,"entry_packets":0,"normal_gate_held":app.services.firmware_paused(),"ordinary_return_rejected":true,"restoration_or_recovery_proved":false})
    );
    // Try exact signals for altered descriptive inputs against this same actual
    // Closed/Published/Revalidated capture. Each request relinquishes and
    // returns the original live store through the actual services; no fake
    // imported receipt is used to set the transport's private accepted flag.
    for altered in ["reservation", "acquisition", "index", "late-cancel"] {
        let mut reservation = review.reservation.clone();
        let mut acquisition = review.acquisition.clone();
        let mut index = review.evidence_index_sha256.clone();
        match altered {
            "reservation" => reservation.reservation.push_str("-other"),
            "acquisition" => acquisition.push_str("-prior"),
            "index" => index = JobSha256::of(b"another capture index").hex(),
            "late-cancel" => app
                .firmware_coordinator
                .device
                .as_ref()
                .unwrap()
                .capture
                .cancel(),
            _ => unreachable!(),
        }
        let draft = EntryStudyDraft::prepare(
            descriptive_fixture_job(),
            reservation,
            acquisition,
            index,
            serde_json::json!({"test":"altered descriptive input only"}),
            review.captured_materials.clone(),
            review.inputs.clone(),
        )
        .unwrap();
        let signal = format!("EXECUTE ENTRY ONCE {}", draft.digest());
        app.firmware_coordinator.entry_authorization = Some(draft.approve_exact(&signal).unwrap());
        let retirement_diagnostics = entry_study_retirement_diagnostic_baseline(&app);
        app.firmware_coordinator
            .begin_retirement(StorePurpose::EntryStudy)
            .unwrap();
        entry_study_wait_actual_owner_return(&mut app, retirement_diagnostics);
        let view = app.firmware_coordinator_view();
        assert!(
            view.diagnostics
                .iter()
                .any(|message| message
                    .contains("lacks this owner's genuine current accepted capture")),
            "{altered}: {:?}",
            view.diagnostics
        );
        assert!(!view.can_return_to_editing);
        assert_eq!(log.lock().unwrap().offers, before);
        assert_eq!(log.lock().unwrap().opens, 1);
        assert!(LibraryStore::open(dir.0.join("library")).is_err());
        println!(
            "FW06_B_PRIVATE_CAPTURE_DENIAL {}",
            serde_json::json!({"altered":altered,"actual_current_capture":true,"original_store_returned":true,"entry_packets":0,"normal_gate_held":app.services.firmware_paused()})
        );
    }
}

#[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
#[test]
fn entry_study_exact_descriptive_signal_without_any_privately_accepted_capture_is_rejected_by_actual_service()
 {
    use std::os::unix::fs::PermissionsExt;
    let (dir, mut app, log) = fixture();
    ready(&mut app);
    app.begin_firmware_preparation().unwrap();
    let path = dir.0.join("unaccepted-study-source.txt");
    std::fs::write(&path, b"No private capture receipt or physical authority").unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let pin = EntryStudyFilePin {
        path,
        bytes: bytes.len() as u64,
        sha256: JobSha256::of(&bytes).hex(),
    };
    let audit = dir.0.join("audit");
    std::fs::create_dir(&audit).unwrap();
    std::fs::set_permissions(&audit, std::fs::Permissions::from_mode(0o700)).unwrap();
    let inputs = EntryStudyInputs {
        sources: vec![pin.clone()],
        loss: pin.clone(),
        recovery: pin.clone(),
        vendor_guidance: pin,
        audit_directory: audit,
        observation_seconds: 1,
        approval_wait_seconds: 30,
    };
    let reservation = FirmwareCaptureReservation {
        actor_run: "descriptive fixture only".into(),
        reservation: "no accepted reservation".into(),
        normal_owner: "fixture".into(),
        epoch: app.controller.state().epoch,
        usb: FirmwareUsbObservation {
            topology: "fixture".into(),
            interface: "fixture".into(),
            endpoint: "fixture".into(),
            vendor: 1,
            product: 2,
            serial: "fixture".into(),
        },
        clock_origin: "fixture".into(),
        prepared_monotonic_ns: 1,
        prepared_wall_millis: 1,
        initial_fence: FirmwareCaptureFence {
            allocated: 0,
            processed: 0,
        },
    };
    let draft = EntryStudyDraft::prepare(
        descriptive_fixture_job(),
        reservation,
        "absent capture".into(),
        JobSha256::of(b"absent index").hex(),
        serde_json::json!({"test":"descriptive only"}),
        vec![],
        inputs,
    )
    .unwrap();
    let signal = format!("EXECUTE ENTRY ONCE {}", draft.digest());
    app.firmware_coordinator.entry_authorization = Some(draft.approve_exact(&signal).unwrap());
    let before = log.lock().unwrap().offers;
    let retirement_diagnostics = entry_study_retirement_diagnostic_baseline(&app);
    app.firmware_coordinator
        .begin_retirement(StorePurpose::EntryStudy)
        .unwrap();
    entry_study_wait_actual_owner_return(&mut app, retirement_diagnostics);
    let view = app.firmware_coordinator_view();
    assert!(
        view.diagnostics
            .iter()
            .any(|message| message.contains("lacks this owner's genuine current accepted capture"))
    );
    assert_eq!(log.lock().unwrap().capture_observations, 0);
    assert_eq!(log.lock().unwrap().offers, before);
    assert_eq!(log.lock().unwrap().opens, 1);
    assert!(!view.can_return_to_editing);
    assert!(LibraryStore::open(dir.0.join("library")).is_err());
    println!(
        "FW06_B_UNACCEPTED_CAPTURE_DENIAL {}",
        serde_json::json!({"actual_normal_service":true,"capture_observations":0,"original_store_returned":true,"entry_packets":0,"normal_gate_held":app.services.firmware_paused()})
    );
}

// These cfg(test) returned-event descriptions never come from a physical owner.
// They test the actual GUI store restoration/editor reads independently of the
// transport module's private real-image owner mock replay.
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn completion_return_projection(
    epoch: SessionEpoch,
    verified: SessionEpoch,
    allowed: bool,
) -> LinuxCompletionOutcome {
    LinuxCompletionOutcome{view:LinuxCompletionView{phase:if allowed{LinuxCompletionPhase::ReturningEditor}else{LinuxCompletionPhase::RecoveryRequired},run:"explicit GUI description fixture only".into(),review_digest:None,review:None,os_offered_bytes:0,host_write_invocations:0,host_accepted_blocks:0,total_blocks:None,device_verified_bytes:0,host_finished_unverified:allowed,normal_mode_verified:allowed,target_version_verified:allowed,contents_verified:false,quarantined:!allowed,diagnostic:Some("Descriptive returned-event fixture; transport owner tested separately, no hardware".into()),retained_raw_chunks:vec![],allocated_raw_frontier:0,retained_block_outcome:None,reader_retirement_diagnostics:vec![]},selection:Some(gp200_sim::simulator_port().selection),original_normal_epoch:epoch,verified_normal_epoch:Some(verified),application_version:allowed.then(||"V1.8.2".into()),journal_path:"not a real owner journal; cfg(test) GUI projection only".into(),editor_return_allowed:allowed}
}
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn genuine_store_and_normal_retirement_for_completion_projection(
    app: &mut StudioApp,
) -> (LibraryStore, SessionEpoch) {
    app.firmware_coordinator
        .begin_retirement(StorePurpose::Recovery)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        app.firmware_coordinator.poll_runtime(&mut app.controller);
        if app.firmware_coordinator.retirement.is_some() {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    let store = loop {
        if let Some(result) = app
            .firmware_coordinator
            .retirement
            .as_mut()
            .unwrap()
            .try_take_result()
        {
            break result.unwrap();
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    };
    app.firmware_coordinator.retirement = None;
    app.services.retire_library_clients();
    app.firmware_coordinator.library = None;
    loop {
        match app
            .firmware_coordinator
            .device
            .as_ref()
            .unwrap()
            .capture
            .try_submit(FirmwareBackupCaptureCommand::ReturnToNormal)
        {
            Ok(()) => break,
            Err(SubmitError::Busy(_)) => std::thread::sleep(Duration::from_millis(1)),
            Err(_) => panic!("actual replay capture stopped"),
        };
        assert!(Instant::now() < deadline);
    }
    let verified = loop {
        if let Some(event) = app
            .firmware_coordinator
            .device
            .as_ref()
            .unwrap()
            .capture
            .try_next_event()
        {
            match event {
                FirmwareBackupCaptureEvent::NormalRefreshRequired { next_epoch, .. } => {
                    break next_epoch;
                }
                other => panic!("unexpected genuine return event {other:?}"),
            }
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    };
    (store, verified)
}
#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn linux_completion_gui_actual_same_store_and_fresh_editor_return_from_descriptive_event_projection()
 {
    for fail_fresh_connect in [false, true] {
        let (dir, mut app, log) = fixture();
        ready(&mut app);
        let epoch = app.controller.state().epoch;
        app.begin_firmware_preparation().unwrap();
        let original = app
            .firmware_coordinator
            .source
            .as_ref()
            .unwrap()
            .recovery
            .clone();
        app.capture_firmware_backup(context()).unwrap();
        until(&mut app, |app| {
            app.firmware_coordinator_view().phase == FirmwareCoordinatorPhase::ReadOnlyReview
        });
        let (store, verified) =
            genuine_store_and_normal_retirement_for_completion_projection(&mut app);
        assert!(verified > epoch);
        assert!(LibraryStore::open(dir.0.join("library")).is_err());
        app.firmware_coordinator.entry_gate_held = true;
        log.lock().unwrap().fail_connect = fail_fresh_connect;
        let outcome = completion_return_projection(epoch, verified, true);
        let summary = FirmwareOwnerSummary {
            phase: "cfg(test) descriptive completion return".into(),
            simulated_source_bytes: 0,
            os_offered_bytes: 0,
            device_verified_bytes: 0,
            completion_verified: false,
            quarantined: false,
            diagnostic: None,
            retained_raw_chunks: vec![],
            allocated_raw_frontier: 0,
        };
        app.firmware_coordinator.purpose = Some(StorePurpose::LinuxCompletion);
        app.firmware_coordinator.accept_completion_return(
            summary,
            store,
            outcome,
            &mut app.controller,
            &app.services,
        );
        assert!(app.services.firmware_paused());
        assert!(app.controller.firmware_editor_blocks_normal_actions());
        until(&mut app, |app| {
            matches!(
                app.firmware_coordinator_view().phase,
                FirmwareCoordinatorPhase::Idle | FirmwareCoordinatorPhase::RecoveryRequired
            ) && app.services.library.is_some()
        });
        assert!(LibraryStore::open(dir.0.join("library")).is_err());
        assert!(app.services.workflow_available());
        if fail_fresh_connect {
            assert_eq!(
                app.firmware_coordinator_view().phase,
                FirmwareCoordinatorPhase::RecoveryRequired
            );
            assert!(
                app.services.firmware_paused()
                    && app.controller.firmware_editor_blocks_normal_actions()
            );
            assert!(app.firmware_coordinator.entry_gate_held);
        } else {
            assert_eq!(
                app.firmware_coordinator_view().phase,
                FirmwareCoordinatorPhase::Idle
            );
            assert!(
                !app.services.firmware_paused()
                    && !app.controller.firmware_editor_blocks_normal_actions()
            );
            assert!(app.controller.state().epoch > verified);
            assert!(
                app.controller
                    .state()
                    .device_globals
                    .as_ref()
                    .is_some_and(|globals| globals.epoch == app.controller.state().epoch)
            );
            assert!(app.controller.state().confirmed_buffer.is_some());
            assert_eq!(
                app.controller.recovery_session().active_document,
                original.active_document
            );
            assert!(!app.firmware_coordinator.entry_gate_held);
        }
    }
}
#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn linux_completion_gui_failed_or_wrong_original_return_keeps_normal_gate_after_store_restore() {
    for wrong_original in [false, true] {
        let (dir, mut app, log) = fixture();
        ready(&mut app);
        let epoch = app.controller.state().epoch;
        app.begin_firmware_preparation().unwrap();
        app.capture_firmware_backup(context()).unwrap();
        until(&mut app, |app| {
            app.firmware_coordinator_view().phase == FirmwareCoordinatorPhase::ReadOnlyReview
        });
        let (store, verified) =
            genuine_store_and_normal_retirement_for_completion_projection(&mut app);
        app.firmware_coordinator.entry_gate_held = true;
        let mut outcome = completion_return_projection(epoch, verified, wrong_original);
        if wrong_original {
            outcome.original_normal_epoch = SessionEpoch(epoch.0 + 99);
        }
        let summary = FirmwareOwnerSummary {
            phase: "cfg(test) denied completion return projection".into(),
            simulated_source_bytes: 0,
            os_offered_bytes: 0,
            device_verified_bytes: 0,
            completion_verified: false,
            quarantined: !wrong_original,
            diagnostic: None,
            retained_raw_chunks: vec![],
            allocated_raw_frontier: 0,
        };
        let opens = log.lock().unwrap().opens;
        app.firmware_coordinator.purpose = Some(StorePurpose::LinuxCompletion);
        app.firmware_coordinator.accept_completion_return(
            summary,
            store,
            outcome,
            &mut app.controller,
            &app.services,
        );
        until(&mut app, |app| app.firmware_coordinator.library_restored);
        assert_eq!(
            app.firmware_coordinator_view().phase,
            FirmwareCoordinatorPhase::RecoveryRequired
        );
        assert_eq!(log.lock().unwrap().opens, opens);
        assert!(
            app.services.firmware_paused()
                && app.controller.firmware_editor_blocks_normal_actions()
        );
        assert!(app.firmware_coordinator.entry_gate_held);
        assert!(app.return_from_firmware_preparation().is_err());
        assert!(LibraryStore::open(dir.0.join("library")).is_err());
    }
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn prepare_disconnected_recovery_return_seam(app: &mut StudioApp) -> LibraryStore {
    // cfg(test) event seam only: genuine disconnected editor/runtime/store
    // ownership, no recovery opener, historical DTO or transport permit.
    assert!(app.controller.state().identity.is_none());
    let runtime = app.services.freeze_firmware().unwrap();
    let editor = app
        .controller
        .begin_firmware_recovery_preparation()
        .unwrap();
    let source = app
        .controller
        .firmware_editor_bound_source(&editor)
        .unwrap()
        .clone();
    app.controller
        .retain_firmware_local_draft_excluded(&editor, &source.recovery)
        .unwrap();
    app.controller
        .seal_firmware_recovery_preparation(&editor, &source.recovery)
        .unwrap();
    app.firmware_coordinator.generation = editor.generation();
    app.firmware_coordinator.source = Some(source);
    app.firmware_coordinator.editor = Some(editor);
    app.firmware_coordinator.runtime = Some(runtime);
    app.firmware_coordinator.recovery_editor = true;
    app.firmware_coordinator.entry_gate_held = true;
    let original_root = app
        .firmware_coordinator
        .library
        .as_ref()
        .unwrap()
        .root
        .clone();
    app.firmware_coordinator.recovery_store_root = Some(original_root.clone());
    assert!(LibraryStore::open(&original_root).is_err());
    app.firmware_coordinator
        .begin_retirement(StorePurpose::LinuxRecovery)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        app.firmware_coordinator.poll_runtime(&mut app.controller);
        if app.firmware_coordinator.retirement.is_some() {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    let store = loop {
        if let Some(result) = app
            .firmware_coordinator
            .retirement
            .as_mut()
            .unwrap()
            .try_take_result()
        {
            break result.unwrap();
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(store.root(), original_root);
    app.firmware_coordinator.retirement = None;
    app.services.retire_library_clients();
    app.firmware_coordinator.library = None;
    store
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn recovery_return_description(epoch: SessionEpoch, allowed: bool) -> LinuxRecoveryOutcome {
    // Public descriptions are deliberately built as an event seam. They cannot
    // create a native opener, monitor, reservation or linked closure witness.
    let completion = completion_return_projection(epoch, epoch, allowed);
    let attempt = FirmwareAttemptId::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
    let binding = history_binding_description();
    let prefix = serde_json::json!({"path":"descriptive-test-prefix", "bytes":1,
        "sha256":JobSha256::of(b"description").hex(), "device":0, "inode":0});
    let history: gp200_transport::LinuxRecoveryHistoryReview =
        gp200_transport::LinuxRecoveryHistoryReview {
            schema: 1,
            purpose: "cfg(test) returned-event description only".into(),
            selected: attempt.clone(),
            original_association: binding,
            predecessors: vec![
                serde_json::from_value(serde_json::json!({
                    "attempt":attempt,"registry":prefix,"journal":prefix,
                    "original_binding_sha256":JobSha256::of(b"description").hex(),
                    "reservation_binding_sha256":JobSha256::of(b"description").hex(),
                    "image_sha256":JobSha256::of(b"description").hex(),
                    "source_job_sha256":JobSha256::of(b"description").hex()
                }))
                .unwrap(),
            ],
            source_reviews: vec![],
            historical_captures: vec![],
            starts_at_section: 0,
            starts_at_block: 0,
            requires_fresh_live_query: true,
            requires_new_full_image_approval: true,
            resume_supported: false,
            physical_authority: false,
            fresh_normal_backup_available: false,
            installed_ir_audio_available: false,
            limitations: "Descriptive test event; no physical proof".into(),
        };
    LinuxRecoveryOutcome {
        view: completion.view,
        selection: completion.selection,
        editor_source_epoch: epoch,
        verified_normal_epoch: Some(epoch),
        application_version: allowed.then(|| "V1.8.2".into()),
        journal_path: completion.journal_path,
        editor_return_allowed: allowed,
        history: Some(history),
    }
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn recovery_summary_description() -> FirmwareOwnerSummary {
    FirmwareOwnerSummary {
        phase: "cfg(test) descriptive recovery return seam".into(),
        simulated_source_bytes: 0,
        os_offered_bytes: 0,
        device_verified_bytes: 0,
        completion_verified: false,
        quarantined: false,
        diagnostic: None,
        retained_raw_chunks: vec![],
        allocated_raw_frontier: 0,
    }
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn disconnected_recovery_gui_actual_store_and_fresh_simulator_refresh_from_returned_event_seam() {
    for fail_connect in [false, true] {
        let (dir, mut app, log) = fixture();
        let original = app.controller.recovery_session();
        let store = prepare_disconnected_recovery_return_seam(&mut app);
        assert_eq!(store.root(), dir.0.join("library"));
        assert!(LibraryStore::open(dir.0.join("library")).is_err());
        assert_eq!(log.lock().unwrap().capture_observations, 0);
        assert_eq!(log.lock().unwrap().opens, 0);
        log.lock().unwrap().fail_connect = fail_connect;
        app.firmware_coordinator.accept_recovery_return(
            recovery_summary_description(),
            store,
            recovery_return_description(SessionEpoch(0), true),
            &mut app.controller,
            &app.services,
        );
        assert!(
            app.services.firmware_paused()
                && app.controller.firmware_editor_blocks_normal_actions()
        );
        until(&mut app, |app| {
            app.services.library.is_some()
                && matches!(
                    app.firmware_coordinator_view().phase,
                    FirmwareCoordinatorPhase::Idle | FirmwareCoordinatorPhase::RecoveryRequired
                )
        });
        assert!(LibraryStore::open(dir.0.join("library")).is_err());
        assert_eq!(log.lock().unwrap().capture_observations, 0);
        if fail_connect {
            assert!(
                app.services.firmware_paused()
                    && app.controller.firmware_editor_blocks_normal_actions()
            );
            assert!(app.return_from_firmware_preparation().is_err());
        } else {
            assert!(
                !app.services.firmware_paused()
                    && !app.controller.firmware_editor_blocks_normal_actions()
            );
            assert!(app.controller.state().epoch > SessionEpoch(0));
            assert!(app.controller.state().confirmed_buffer.is_some());
            assert!(app.controller.state().device_globals.is_some());
            assert!(
                app.controller
                    .recovery_session()
                    .documents
                    .iter()
                    .any(|document| original
                        .documents
                        .iter()
                        .any(|old| document.draft == old.draft))
            );
        }
    }
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn recovery_returned_event_seam_rejects_wrong_owner_epoch_and_failed_proof_after_same_store_restore()
 {
    for reason in ["owner", "epoch", "proof"] {
        let (dir, mut app, log) = fixture();
        let store = prepare_disconnected_recovery_return_seam(&mut app);
        let mut outcome = recovery_return_description(SessionEpoch(0), reason != "proof");
        if reason == "owner" {
            app.firmware_coordinator.purpose = Some(StorePurpose::LinuxCompletion);
        }
        if reason == "epoch" {
            outcome.editor_source_epoch = SessionEpoch(99);
        }
        app.firmware_coordinator.accept_recovery_return(
            recovery_summary_description(),
            store,
            outcome,
            &mut app.controller,
            &app.services,
        );
        until(&mut app, |app| app.firmware_coordinator.library_restored);
        assert_eq!(
            app.firmware_coordinator_view().phase,
            FirmwareCoordinatorPhase::RecoveryRequired
        );
        assert_eq!(log.lock().unwrap().opens, 0);
        assert_eq!(log.lock().unwrap().capture_observations, 0);
        assert!(
            app.services.firmware_paused()
                && app.controller.firmware_editor_blocks_normal_actions()
        );
        assert!(app.return_from_firmware_preparation().is_err());
        assert!(LibraryStore::open(dir.0.join("library")).is_err());
    }
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn synthetic_recovery_request_descriptions(
    directory: &std::path::Path,
) -> (FirmwareReview, LinuxCompletionInputs) {
    // Deliberately unsupported image/input descriptions. Simulator admission
    // rejects before native enumeration, pin reads or any recovery opener.
    let bytes = include_bytes!("../../../tests/fixtures/device-assets/synthetic-firmware.bin");
    let image = FirmwareReview::inspect(
        RetainedJobBlob::checked(
            AssetId("synthetic-recovery-denial-fixture".into()),
            Arc::from(&bytes[..]),
            JobSha256::of(bytes),
        )
        .unwrap(),
        None,
        &FirmwareBackupAssessment::default(),
    )
    .unwrap();
    let pin = EntryStudyFilePin {
        path: directory.join("no-native-pin"),
        bytes: 0,
        sha256: JobSha256::of(&[]).hex(),
    };
    let inputs = LinuxCompletionInputs {
        sources: vec![pin.clone()],
        loss: pin.clone(),
        recovery: pin.clone(),
        vendor_guidance: pin.clone(),
        contract: pin,
        run_directory: directory.to_path_buf(),
        approval_wait_seconds: 30,
        transition_seconds: 5,
        transfer_seconds: 60,
    };
    (image, inputs)
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn recovery_busy_submit_restores_exact_original_store_without_retry_or_native_calls() {
    let (dir, mut app, log) = fixture();
    let original = prepare_disconnected_recovery_return_seam(&mut app);
    let (image, inputs) = synthetic_recovery_request_descriptions(&dir.0);
    let other_root = dir.0.join("separate-ticket-fixture");
    let other_store = LibraryStore::open(&other_root).unwrap();
    let (mut request, _) = LinuxRecoveryRequest::new(
        image.clone(),
        inputs.clone(),
        None,
        SessionEpoch(0),
        other_store,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match app
            .firmware_coordinator
            .device
            .as_mut()
            .unwrap()
            .submit_recovery(request)
        {
            Ok(()) => break,
            Err(SubmitError::Busy(original)) => request = original,
            Err(_) => panic!("genuine simulator service disconnected"),
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    // First genuine ticket is retained. The coordinator's distinct second
    // request must return Busy with exactly its own unoffered original store.
    app.firmware_coordinator.retained_store = Some(original);
    app.firmware_coordinator.recovery_pending = Some((image, inputs, None));
    app.firmware_coordinator
        .submit_checked_recovery(&mut app.controller, &app.services);
    assert_eq!(
        app.firmware_coordinator.phase,
        FirmwareCoordinatorPhase::RecoveryRequired
    );
    assert!(app.firmware_coordinator.completion_monitor.is_none());
    // Consume the separate fixture ticket directly, never injecting its store
    // into the coordinator's original return rail.
    loop {
        if let Some(event) = app.firmware_coordinator.device.as_mut().unwrap().poll() {
            let FirmwareOwnerEvent::Rejected { store, .. } = event else {
                panic!("unexpected simulator recovery event")
            };
            assert_eq!(store.root(), other_root);
            drop(store);
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    until(&mut app, |app| app.firmware_coordinator.library_restored);
    assert!(LibraryStore::open(dir.0.join("library")).is_err());
    assert_eq!(log.lock().unwrap().opens, 0);
    assert_eq!(log.lock().unwrap().offers, 0);
    assert_eq!(log.lock().unwrap().capture_observations, 0);
    assert!(
        app.services.firmware_paused() && app.controller.firmware_editor_blocks_normal_actions()
    );
    assert!(app.return_from_firmware_preparation().is_err());
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn recovery_review_denies_ready_or_connecting_normal_editor_before_local_freeze() {
    for connection in [ConnectionState::Ready, ConnectionState::Connecting] {
        let (dir, mut app, log) = fixture();
        ready(&mut app);
        app.controller
            .dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
                epoch: app.controller.state().epoch,
                state: connection,
            }));
        let (image, inputs) = synthetic_recovery_request_descriptions(&dir.0);
        let offers = log.lock().unwrap().offers;
        let error = app
            .firmware_coordinator
            .begin_linux_recovery(
                &mut app.controller,
                &mut app.services,
                true,
                image,
                Some(inputs),
                None,
            )
            .unwrap_err();
        assert!(error.message.contains("disconnected editor"));
        assert!(!app.firmware_coordinator_view().can_review_linux_recovery);
        assert!(!app.services.firmware_paused());
        assert!(!app.controller.firmware_editor_blocks_normal_actions());
        assert_eq!(log.lock().unwrap().offers, offers);
        assert_eq!(log.lock().unwrap().capture_observations, 0);
        assert!(LibraryStore::open(dir.0.join("library")).is_err());
    }
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn linux_history_frame(magic: &[u8; 8], body: &[u8]) -> Vec<u8> {
    let mut bytes = magic.to_vec();
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    let digest = JobSha256::of(body).hex();
    bytes.extend(
        (0..64)
            .step_by(2)
            .map(|i| u8::from_str_radix(&digest[i..i + 2], 16).unwrap()),
    );
    bytes.extend_from_slice(body);
    bytes
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn write_private_history(path: &std::path::Path, bytes: &[u8], append: bool) {
    use std::{
        io::Write,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
    };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .append(append)
        .truncate(!append)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn history_binding_description() -> FirmwareUsbObservation {
    FirmwareUsbObservation {
        topology: "disk parser fixture topology".into(),
        vendor: 0x84ef,
        product: 0x2a,
        serial: "descriptive disk fixture".into(),
        interface: "fixture interface".into(),
        endpoint: "hw:Fixture,0,0".into(),
    }
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn write_history_begin(
    root: &std::path::Path,
    registry: &std::path::Path,
    character: char,
    ancestors: &[serde_json::Value],
) -> FirmwareAttemptId {
    write_history_begin_attempt(
        root,
        registry,
        FirmwareAttemptId::parse(&character.to_string().repeat(32)).unwrap(),
        ancestors,
    )
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn write_history_begin_attempt(
    root: &std::path::Path,
    registry: &std::path::Path,
    attempt: FirmwareAttemptId,
    ancestors: &[serde_json::Value],
) -> FirmwareAttemptId {
    use std::os::unix::fs::PermissionsExt;
    let journals = root.join("linux-firmware-completion");
    for directory in [registry, journals.as_path()] {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let journal = journals.join(format!("{}.journal", attempt.as_str()));
    let binding = history_binding_description();
    let capture = FirmwareCaptureReservation {
        actor_run: "MockSimulation disk display fixture".into(),
        reservation: "descriptive original capture".into(),
        normal_owner: "fixture original owner".into(),
        epoch: SessionEpoch(10),
        usb: binding.clone(),
        clock_origin: "fixture original clock".into(),
        prepared_monotonic_ns: 0,
        prepared_wall_millis: 1000,
        initial_fence: FirmwareCaptureFence {
            allocated: 0,
            processed: 0,
        },
    };
    let pins=(0..3).map(|i|serde_json::json!({"path":root.join(format!("historical-{i}.fixture")),"bytes":1,"sha256":"33".repeat(32)})).collect::<Vec<_>>();
    let review = serde_json::json!({"schema":1,"job_sha256":"11".repeat(32),"reservation":capture,"acquisition":"historical disk display fixture","evidence_index_sha256":"44".repeat(32),"captured_materials":pins});
    let metadata = if ancestors.is_empty() {
        serde_json::json!({"review":review,"review_digest":"22".repeat(32),"original_normal_epoch":10})
    } else {
        serde_json::json!({"purpose":"fresh-full-image-recovery-v1","review":review,"review_digest":"22".repeat(32),"original_binding":binding,"anchor_binding":binding,"predecessors":ancestors})
    };
    let begin=serde_json::to_vec(&serde_json::json!({"kind":"begin","schema":1,"run":attempt,"metadata":metadata,"resume_supported":false,"physical_authority":false})).unwrap();
    write_private_history(&journal, &linux_history_frame(b"GP2LCJ01", &begin), false);
    let reservation = format!(
        "schema=1\norigin=LocalObservation\nattempt={}\nreview={}\nprofile={}\nbinding={}\ngeneration=1\nclock=fixture-clock\nimage={}\njournal={}\nupdater_epoch=11\nno_physical_authority=true",
        attempt.as_str(),
        "11".repeat(32),
        gp200_transport::firmware_vendor_sequence::VENDOR_CONTRACT_SHA256,
        binding.digest(),
        gp200_transport::firmware_vendor_sequence::VENDOR_IMAGE_SHA256,
        journal.display()
    );
    let mut body = vec![0];
    body.extend_from_slice(reservation.as_bytes());
    write_private_history(
        &registry.join(format!("{}.quarantine", attempt.as_str())),
        &linux_history_frame(b"GP2QUAR1", &body),
        false,
    );
    attempt
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn history_predecessor_descriptions(
    root: &std::path::Path,
    registry: &std::path::Path,
) -> Vec<serde_json::Value> {
    let catalogue = review_linux_firmware_history_at(root, registry).unwrap();
    assert!(catalogue.diagnostics.is_empty());
    catalogue.rows.into_iter().filter(|row|!row.resolved).map(|row|serde_json::json!({"attempt":row.attempt,"registry":row.registry,"journal":row.journal,"original_binding_sha256":history_binding_description().digest(),"reservation_binding_sha256":history_binding_description().digest(),"image_sha256":gp200_transport::firmware_vendor_sequence::VENDOR_IMAGE_SHA256,"source_job_sha256":"11".repeat(32)})).collect()
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
pub(super) fn write_completed_linux_history_fixture(
    root: &std::path::Path,
    registry: &std::path::Path,
) -> Vec<FirmwareAttemptId> {
    // Compact, valid GP2LCJ01 parser records. These describe disk history;
    // they do not attest hardware acceptance or create private owner witnesses.
    let a = write_history_begin(root, registry, 'a', &[]);
    let b = write_history_begin(
        root,
        registry,
        'b',
        &history_predecessor_descriptions(root, registry),
    );
    let refs = history_predecessor_descriptions(root, registry);
    let c = write_history_begin(root, registry, 'c', &refs);
    let description = refs
        .iter()
        .map(|r| {
            format!(
                "{},{},{},{},{}",
                r["attempt"].as_str().unwrap(),
                r["registry"]["bytes"],
                r["registry"]["sha256"].as_str().unwrap(),
                r["registry"]["device"],
                r["registry"]["inode"]
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let set = JobSha256::of(description.as_bytes()).hex();
    let mut members = Vec::new();
    for r in &refs {
        let body = format!(
            "schema=1\nkind=linked-recovery-predecessor\nsuccessor={}\nset={}\noriginal_bytes={}\noriginal_sha256={}\noriginal_device={}\noriginal_inode={}\nno_physical_authority=true",
            c.as_str(),
            set,
            r["registry"]["bytes"],
            r["registry"]["sha256"].as_str().unwrap(),
            r["registry"]["device"],
            r["registry"]["inode"]
        );
        let mut payload = vec![6];
        payload.extend_from_slice(body.as_bytes());
        let path = std::path::Path::new(r["registry"]["path"].as_str().unwrap());
        write_private_history(path, &linux_history_frame(b"GP2QUAR1", &payload), true);
        let bytes = std::fs::read(path).unwrap();
        members.push(format!(
            "predecessor={},{},{},{},{},{},{}",
            r["attempt"].as_str().unwrap(),
            r["registry"]["bytes"],
            r["registry"]["sha256"].as_str().unwrap(),
            r["registry"]["device"],
            r["registry"]["inode"],
            bytes.len(),
            JobSha256::of(&bytes).hex()
        ));
    }
    let journal = root
        .join("linux-firmware-completion")
        .join(format!("{}.journal", c.as_str()));
    write_private_history(&journal,&linux_history_frame(b"GP2LCJ01",br#"{"kind":"live-recovery-completion-verified","physical_authority":false,"fixture":"descriptive disk catalogue only"}"#),true);
    let journal_bytes = std::fs::read(&journal).unwrap();
    let mut lines = vec![
        "schema=1".into(),
        "kind=linked-recovery-commit".into(),
        format!("successor={}", c.as_str()),
        format!("set={set}"),
        format!("source_review={}", "11".repeat(32)),
        format!(
            "image={}",
            gp200_transport::firmware_vendor_sequence::VENDOR_IMAGE_SHA256
        ),
        format!("journal={}", journal.display()),
        format!("journal_bytes={}", journal_bytes.len()),
        format!(
            "journal_prefix_sha256={}",
            JobSha256::of(&journal_bytes).hex()
        ),
        "journal_sequence=2".into(),
        "normal_epoch=20".into(),
        format!(
            "normal_binding_sha256={}",
            history_binding_description().digest()
        ),
        format!("identity_sha256={}", "33".repeat(32)),
        format!("current_sha256={}", "44".repeat(32)),
        format!("globals_sha256={}", "55".repeat(32)),
        "raw_allocated=3".into(),
        "raw_processed=3".into(),
        "reader_joined=true".into(),
        "output_destroyed=true".into(),
        format!("manual_phase_review={}", "66".repeat(32)),
        "manual_application_version=V1.8.2".into(),
        "contents_verified=false".into(),
        format!("count={}", members.len()),
    ];
    lines.extend(members);
    let mut payload = vec![7];
    payload.extend_from_slice(lines.join("\n").as_bytes());
    write_private_history(
        &registry.join(format!("{}.quarantine", c.as_str())),
        &linux_history_frame(b"GP2QUAR1", &payload),
        true,
    );
    let d = write_history_begin(root, registry, 'd', &[]);
    let journal = root
        .join("linux-firmware-completion")
        .join(format!("{}.journal", d.as_str()));
    let closure = serde_json::json!({"schema":1,"attempt":d,"run":d,"journal":journal,"image_sha256":gp200_transport::firmware_vendor_sequence::VENDOR_IMAGE_SHA256,"sequence":1,"prefix_sha256":JobSha256::of(&std::fs::read(&journal).unwrap()).hex(),"manual_phase_review":"66".repeat(32),"source_review":"11".repeat(32),"manual_application_version":"V1.8.2","contents_verified":false,"normal":{"binding":history_binding_description(),"epoch":20,"allocated":3,"processed":3,"reader_joined":true,"output_destroyed":true,"identity_sha256":"33".repeat(32),"current_sha256":"44".repeat(32),"globals_sha256":"55".repeat(32)}});
    let mut payload = vec![5];
    payload.extend_from_slice(&serde_json::to_vec(&closure).unwrap());
    write_private_history(
        &registry.join(format!("{}.quarantine", d.as_str())),
        &linux_history_frame(b"GP2QUAR1", &payload),
        true,
    );
    vec![a, b, c, d]
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
pub(super) fn write_zero_call_linux_history_fixture(
    root: &std::path::Path,
    registry: &std::path::Path,
) -> FirmwareAttemptId {
    let attempt = write_history_begin(root, registry, 'e', &[]);
    write_private_history(
        &registry.join(format!("{}.quarantine", attempt.as_str())),
        &linux_history_frame(
            b"GP2QUAR1",
            b"\x02creating owner observed zero driver calls",
        ),
        true,
    );
    attempt
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn actual_custom_root_catalogue_rediscovers_completed_initial_and_recovery_after_new_coordinator() {
    let (directory, mut app, log) = fixture();
    let registry = directory.0.join("private-display-registry");
    let attempts = app
        .firmware_coordinator
        .test_completed_linux_history_fixture(&registry);
    for restart in [false, true] {
        if restart {
            app.services.firmware_library = app.firmware_coordinator.library.take();
            app.services.firmware_device = app.firmware_coordinator.device.take();
            app.firmware_coordinator = FirmwareUpdateCoordinator::new(&mut app.services);
        }
        app.firmware_coordinator
            .test_linux_history_registry(registry.clone());
        assert_eq!(
            app.firmware_coordinator.library.as_ref().unwrap().root,
            directory.0.join("library")
        );
        app.refresh_linux_firmware_history().unwrap();
        until(&mut app, |a| {
            !a.firmware_coordinator.linux_history_status().0
        });
        let catalogue = app.firmware_coordinator.linux_history().unwrap();
        assert!(
            catalogue.diagnostics.is_empty(),
            "{:?}",
            catalogue.diagnostics
        );
        assert_eq!(
            catalogue
                .rows
                .iter()
                .map(|row| row.attempt.clone())
                .collect::<Vec<_>>(),
            attempts
        );
        assert!(
            catalogue
                .rows
                .iter()
                .all(|row| row.resolved && !row.physical_authority && !row.resume_supported)
        );
        assert!(
            catalogue
                .rows
                .iter()
                .any(|row| row.local_state == 5 && row.purpose == "captured-normal-linux-update")
        );
        assert!(
            catalogue
                .rows
                .iter()
                .any(|row| row.local_state == 7 && row.purpose == "fresh-full-image-recovery-v1")
        );
        assert!(catalogue.rows.iter().any(|row| {
            row.historical_capture
                .as_ref()
                .is_some_and(|capture| capture.prepared_wall_millis == 1000)
        }));
        assert!(
            app.firmware_coordinator
                .select_linux_history(Some(attempts[2].clone()))
                .is_err()
        );
        assert!(app.firmware_coordinator.completion_monitor.is_none());
        assert!(LibraryStore::open(directory.0.join("library")).is_err());
        assert!(!app.services.firmware_paused() && !app.firmware_close_blocked());
    }
    assert_eq!(log.lock().unwrap().opens, 0);
    assert_eq!(log.lock().unwrap().offers, 0);
    assert_eq!(log.lock().unwrap().capture_observations, 0);
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn catalogue_changed_corrupt_or_unknown_reread_revokes_descriptive_selection_without_owner() {
    for corruption in [false, true] {
        let (directory, mut app, log) = fixture();
        let root = directory.0.join("library");
        let registry = directory.0.join("display-registry");
        let attempt = write_history_begin(&root, &registry, 'a', &[]);
        app.firmware_coordinator
            .test_linux_history_registry(registry.clone());
        app.refresh_linux_firmware_history().unwrap();
        until(&mut app, |a| {
            !a.firmware_coordinator.linux_history_status().0
        });
        app.firmware_coordinator
            .select_linux_history(Some(attempt.clone()))
            .unwrap();
        if corruption {
            write_private_history(
                &root
                    .join("linux-firmware-completion")
                    .join(format!("{}.journal", attempt.as_str())),
                b"crashed tail",
                true,
            );
        } else {
            write_private_history(
                &registry.join("unknown.pending"),
                b"unsettled retained entry",
                false,
            );
        }
        app.refresh_linux_firmware_history().unwrap();
        assert!(app.firmware_coordinator.linux_history_status().2.is_none());
        until(&mut app, |a| {
            !a.firmware_coordinator.linux_history_status().0
        });
        let catalogue = app.firmware_coordinator.linux_history().unwrap();
        assert!(!catalogue.diagnostics.is_empty());
        assert!(
            app.firmware_coordinator
                .select_linux_history(Some(attempt))
                .is_err()
        );
        assert!(app.firmware_coordinator.completion_monitor.is_none());
        assert!(!app.services.firmware_paused());
        assert!(!app.firmware_close_blocked());
        assert_eq!(log.lock().unwrap().opens, 0);
        assert_eq!(log.lock().unwrap().offers, 0);
        assert_eq!(log.lock().unwrap().capture_observations, 0);
        assert!(LibraryStore::open(root).is_err());
    }
}

#[test]
#[cfg(all(
    gp200_private_evidence,
    feature = "firmware-linux-completion",
    target_os = "linux"
))]
fn linux_completion_entry_display_preserves_the_full_genuine_description_outside_render_json() {
    use gp200_transport::{EntryStudyFilePin, LinuxCompletionInputs, LinuxCompletionReview};
    let bytes =
        include_bytes!("../../gp200-transport/test-fixtures/firmware/genuine-entry-review.json");
    assert_eq!(
        JobSha256::of(bytes).hex(),
        "c44ca19fa9b18380275c5c2c7bb9408e4afb6569219cbb0684ece5b446ca9651"
    );
    let r: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    // Historical descriptions exercise the pure display projection. No path is
    // opened, no opaque draft is issued, and no hardware capability is constructed.
    let review = LinuxCompletionReview {
        schema: 1,
        purpose: "inert display fixture only".into(),
        job_sha256: r["job_sha256"].as_str().unwrap().into(),
        reservation: serde_json::from_value(r["reservation"].clone()).unwrap(),
        acquisition: r["acquisition"].as_str().unwrap().into(),
        evidence_index_sha256: r["evidence_index_sha256"].as_str().unwrap().into(),
        genuine_preparation: r["genuine_preparation"].clone(),
        captured_materials: serde_json::from_value(r["captured_materials"].clone()).unwrap(),
        executable: serde_json::from_value(r["executable"].clone()).unwrap(),
        inputs: LinuxCompletionInputs {
            sources: serde_json::from_value(r["inputs"]["sources"].clone()).unwrap(),
            loss: serde_json::from_value(r["inputs"]["loss"].clone()).unwrap(),
            recovery: serde_json::from_value(r["inputs"]["recovery"].clone()).unwrap(),
            vendor_guidance: serde_json::from_value(r["inputs"]["vendor_guidance"].clone())
                .unwrap(),
            contract: EntryStudyFilePin {
                path: PathBuf::from("descriptive contract path; never opened"),
                bytes: 0,
                sha256: gp200_transport::firmware_vendor_sequence::VENDOR_CONTRACT_SHA256.into(),
            },
            run_directory: PathBuf::from(r["inputs"]["audit_directory"].as_str().unwrap()),
            approval_wait_seconds: 30,
            transition_seconds: 5,
            transfer_seconds: 60,
        },
        outgoing: serde_json::Value::Null,
        evidence: serde_json::Value::Null,
        limitations: "inert display fixture; no hardware authority".into(),
    };
    let original = serde_json::to_vec(&review.genuine_preparation).unwrap();
    assert!(original.len() > 400_000);
    let displayed = completion_display_facts(&review);
    assert!(serde_json::to_vec(&displayed).unwrap().len() < 32_768);
    assert_eq!(
        displayed["retained_source_files"].as_u64().unwrap() as usize,
        review.inputs.sources.len()
    );
    assert_eq!(displayed["inputs"]["sources"].as_array().unwrap().len(), 16);
    assert_eq!(displayed["source_pins_truncated"], true);
    assert_eq!(
        displayed.pointer("/genuine_preparation/capture_evidence/records"),
        review
            .genuine_preparation
            .pointer("/capture_evidence/records")
    );
    assert!(
        displayed
            .pointer("/genuine_preparation/capture_evidence/acquisition")
            .is_none()
    );
    assert_eq!(
        serde_json::to_vec(&review.genuine_preparation).unwrap(),
        original
    );
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn recovery_async_input_result_preserves_original_payload_and_ignores_old_generation() {
    // Descriptive asynchronous resource-result seam only. The actual runtime and
    // local editor are frozen; no native owner, capture, approval or Store is moved.
    let (directory, mut app, log) = fixture();
    let runtime = app.services.freeze_firmware().unwrap();
    let editor = app
        .controller
        .begin_firmware_recovery_preparation()
        .unwrap();
    app.firmware_coordinator.generation = editor.generation();
    app.firmware_coordinator.source = Some(
        app.controller
            .firmware_editor_bound_source(&editor)
            .unwrap()
            .clone(),
    );
    app.firmware_coordinator.runtime = Some(runtime);
    app.firmware_coordinator.editor = Some(editor);
    app.firmware_coordinator.recovery_editor = true;
    app.firmware_coordinator.phase = FirmwareCoordinatorPhase::Preparing;
    let (image, inputs) = synthetic_recovery_request_descriptions(&directory.0);
    let original_hash = image.summary().source_sha256.hex();
    let original_inputs = inputs.clone();
    let selected = FirmwareAttemptId::parse("ffffffffffffffffffffffffffffffff").unwrap();
    let original_selected = selected.clone();
    for old_generation in [false, true] {
        let (send, received) = std::sync::mpsc::sync_channel(1);
        let payload = Box::new((
            image.clone(),
            inputs.clone(),
            if old_generation {
                None
            } else {
                Some(selected.clone())
            },
        ));
        let thread = std::thread::spawn(move || {
            send.send(CompletionPreparationReply::RecoveryInputs(Ok(payload)))
                .unwrap()
        });
        app.firmware_coordinator.completion_preparation = Some(CompletionPreparation {
            received,
            thread: Some(thread),
            generation: if old_generation {
                app.firmware_coordinator.generation.saturating_sub(1)
            } else {
                app.firmware_coordinator.generation
            },
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.firmware_coordinator.completion_preparation.is_some() {
            app.firmware_coordinator
                .poll_completion_preparation(&mut app.controller, &app.services);
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        let (retained_image, retained_inputs, retained_selected) =
            app.firmware_coordinator.recovery_pending.as_ref().unwrap();
        assert_eq!(retained_image.summary().source_sha256.hex(), original_hash);
        assert_eq!(retained_inputs.run_directory, original_inputs.run_directory);
        assert_eq!(retained_inputs.sources.len(), original_inputs.sources.len());
        assert_eq!(
            retained_inputs.sources[0].path,
            original_inputs.sources[0].path
        );
        assert_eq!(
            retained_inputs.sources[0].sha256,
            original_inputs.sources[0].sha256
        );
        assert_eq!(retained_selected.as_ref(), Some(&original_selected));
    }
    assert!(
        app.firmware_coordinator.retirement.is_none()
            && app.firmware_coordinator.completion_monitor.is_none()
    );
    assert!(LibraryStore::open(directory.0.join("library")).is_err());
    assert_eq!(log.lock().unwrap().offers, 0);
    assert_eq!(log.lock().unwrap().opens, 0);
    assert_eq!(log.lock().unwrap().capture_observations, 0);
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn catalogue_shares_immutable_snapshot_and_pages_forty_actual_private_closed_rows() {
    let (directory, mut app, log) = fixture();
    let root = directory.0.join("library");
    let registry = directory.0.join("paged-private-display-registry");
    for index in 0..40 {
        let attempt = FirmwareAttemptId::parse(&format!("{:032x}", 0x1000 + index)).unwrap();
        let attempt = write_history_begin_attempt(&root, &registry, attempt, &[]);
        write_private_history(
            &registry.join(format!("{}.quarantine", attempt.as_str())),
            &linux_history_frame(
                b"GP2QUAR1",
                b"\x02creating owner observed zero driver calls",
            ),
            true,
        );
    }
    app.firmware_coordinator
        .test_linux_history_registry(registry);
    app.refresh_linux_firmware_history().unwrap();
    until(&mut app, |a| {
        !a.firmware_coordinator.linux_history_status().0
    });
    let first = app.firmware_coordinator.linux_history().unwrap();
    let second = app.firmware_coordinator.linux_history().unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(first.rows.len(), 40);
    assert!(first.diagnostics.is_empty());
    assert!(
        first
            .rows
            .iter()
            .all(|row| row.resolved && row.local_state == 2 && !row.physical_authority)
    );
    assert_eq!(app.firmware_coordinator.linux_history_page_offset(), 0);
    assert_eq!(
        first
            .rows
            .iter()
            .skip(app.firmware_coordinator.linux_history_page_offset())
            .take(32)
            .count(),
        32
    );
    app.firmware_coordinator.change_linux_history_page(true);
    assert_eq!(app.firmware_coordinator.linux_history_page_offset(), 32);
    assert_eq!(
        first
            .rows
            .iter()
            .skip(app.firmware_coordinator.linux_history_page_offset())
            .take(32)
            .count(),
        8
    );
    app.firmware_coordinator.change_linux_history_page(true);
    assert_eq!(app.firmware_coordinator.linux_history_page_offset(), 32);
    app.firmware_coordinator.change_linux_history_page(false);
    assert_eq!(app.firmware_coordinator.linux_history_page_offset(), 0);
    assert!(
        app.firmware_coordinator
            .select_linux_history(Some(first.rows[0].attempt.clone()))
            .is_err()
    );
    assert!(app.firmware_coordinator.completion_monitor.is_none() && !app.firmware_close_blocked());
    assert!(LibraryStore::open(root).is_err());
    assert_eq!(log.lock().unwrap().offers, 0);
    assert_eq!(log.lock().unwrap().opens, 0);
    assert_eq!(log.lock().unwrap().capture_observations, 0);
}

#[test]
fn normal_firmware_globals_preflight_preserves_original_owners_until_current_read() {
    let (dir, mut app, log) = fixture();
    app.send(UiIntent::Connect(gp200_sim::simulator_port().selection));
    until(&mut app, |app| {
        app.controller.state().connection == ConnectionState::Ready
            && app.services.firmware_queues_settled()
    });
    until(&mut app, |app| {
        app.recovery_ui.last_saved.as_deref() == Some(&app.controller.recovery_session())
            && !app.recovery_ui.busy
    });
    assert!(app.controller.state().device_globals.is_none());
    let epoch = app.controller.state().epoch;
    let offers = log.lock().unwrap().offers;
    let saved = app.recovery_ui.last_saved.clone();
    let error = app.begin_firmware_preparation().unwrap_err();
    assert_eq!(error.kind, ErrorKind::Busy);
    assert!(
        error
            .message
            .contains("Read global settings in this connection")
    );
    assert_eq!(
        app.firmware_coordinator_view().phase,
        FirmwareCoordinatorPhase::Idle
    );
    assert!(!app.services.firmware_paused());
    assert!(!app.controller.firmware_editor_blocks_normal_actions());
    assert!(app.services.library.is_some());
    assert!(app.services.workflow_available());
    assert!(LibraryStore::open(dir.0.join("library")).is_err());
    assert_eq!(app.controller.state().epoch, epoch);
    assert_eq!(app.recovery_ui.last_saved, saved);
    {
        let log = log.lock().unwrap();
        assert_eq!(log.offers, offers);
        assert_eq!(log.opens, 1);
        assert_eq!(log.closes, 0);
        assert_eq!(log.capture_observations, 0);
    }

    // The rejected preparation must leave the actual normal service able to
    // read globals without a cancel, replacement Store or replacement session.
    app.send(UiIntent::RefreshDeviceGlobals);
    until(&mut app, |app| {
        normal_firmware_globals_current(app.controller.state())
            && app.services.firmware_queues_settled()
    });
    until(&mut app, |app| {
        app.recovery_ui.last_saved.as_deref() == Some(&app.controller.recovery_session())
            && !app.recovery_ui.busy
    });
    assert!(normal_firmware_globals_current(app.controller.state()));
    let mut missing = app.controller.state().clone();
    missing.device_globals = None;
    assert!(!normal_firmware_globals_current(&missing));
    // A descriptive snapshot tests stale admission defensively. The real
    // reducer clears globals on reconnect and rejects old-epoch replies.
    let mut stale = app.controller.state().clone();
    stale.epoch = SessionEpoch(stale.epoch.0.checked_add(1).unwrap());
    assert!(stale.device_globals.is_some());
    assert!(!normal_firmware_globals_current(&stale));

    let offers = log.lock().unwrap().offers;
    app.begin_firmware_preparation().unwrap();
    assert_eq!(
        app.firmware_coordinator_view().phase,
        FirmwareCoordinatorPhase::Preparing
    );
    assert!(app.services.firmware_paused());
    assert!(app.controller.firmware_editor_blocks_normal_actions());
    assert!(app.services.library.is_some());
    assert!(LibraryStore::open(dir.0.join("library")).is_err());
    assert_eq!(log.lock().unwrap().offers, offers);
    app.return_from_firmware_preparation().unwrap();
    assert_eq!(
        app.firmware_coordinator_view().phase,
        FirmwareCoordinatorPhase::Idle
    );
    assert!(!app.services.firmware_paused());
    assert!(!app.controller.firmware_editor_blocks_normal_actions());
    assert!(app.services.library.is_some());
    assert!(LibraryStore::open(dir.0.join("library")).is_err());
    assert_eq!(log.lock().unwrap().opens, 1);
    assert_eq!(log.lock().unwrap().closes, 0);
    assert_eq!(log.lock().unwrap().capture_observations, 0);
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn local_recovery_preparation_seam(app: &mut StudioApp) {
    // The same actual unsealed controller/runtime leases used by production;
    // no correct-image bypass, device ticket, monitor or ownership request.
    let runtime = app.services.freeze_firmware().unwrap();
    let editor = app
        .controller
        .begin_firmware_recovery_preparation()
        .unwrap();
    app.firmware_coordinator.generation = editor.generation();
    app.firmware_coordinator.source = Some(
        app.controller
            .firmware_editor_bound_source(&editor)
            .unwrap()
            .clone(),
    );
    app.firmware_coordinator.editor = Some(editor);
    app.firmware_coordinator.runtime = Some(runtime);
    app.firmware_coordinator.recovery_editor = true;
    app.firmware_coordinator.entry_gate_held = true;
    app.firmware_coordinator.phase = FirmwareCoordinatorPhase::Preparing;
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn recovery_public_preflight_keeps_previous_journal_until_recovered_documents_are_saved() {
    let (directory, mut app, log) = fixture();
    let previous_root = directory.0.join("previous-drafts");
    let mut previous = app.controller.recovery_session();
    let mut second = previous.documents[0].clone();
    second.original_id = DocumentId("second-previous-document".into());
    previous.documents.push(second);
    let retained = DraftRecoveryStore::open(&previous_root)
        .unwrap()
        .save(&previous)
        .unwrap();
    let previous_bytes = std::fs::read(previous_root.join("session.gp200recovery")).unwrap();
    app.services.draft_recovery =
        Some(DraftRecoveryWorker::spawn(previous_root.clone(), no_wake()).unwrap());
    app.recovery_ui = Default::default();
    let original_documents = app.controller.recovery_session();
    let error = app.review_linux_recovery().unwrap_err();
    assert!(error.message.contains("discovery"));
    assert!(!app.services.firmware_paused());
    until(&mut app, |app| app.recovery_ui.found.is_some());
    assert_eq!(
        app.recovery_ui.found.as_ref().unwrap().sha256,
        retained.sha256
    );
    let error = app.review_linux_recovery().unwrap_err();
    assert!(error.message.contains("Recover all offline"));
    assert!(!app.firmware_coordinator_view().can_review_linux_recovery);
    assert!(!app.services.firmware_paused());
    assert!(!app.firmware_close_blocked());
    assert!(!app.controller.firmware_editor_blocks_normal_actions());
    assert!(app.firmware_coordinator.completion_preparation.is_none());
    assert_eq!(app.controller.recovery_session(), original_documents);
    assert_eq!(
        std::fs::read(previous_root.join("session.gp200recovery")).unwrap(),
        previous_bytes
    );
    assert!(LibraryStore::open(directory.0.join("library")).is_err());
    app.recover_previous_offline();
    assert!(app.recovery_ui.found.is_none());
    assert_eq!(app.controller.state().documents.len(), 3);
    assert!(app.recovery_ui.last_saved.is_none());
    let error = app.review_linux_recovery().unwrap_err();
    assert!(error.message.contains("Saved"));
    assert!(!app.services.firmware_paused());
    assert_eq!(
        std::fs::read(previous_root.join("session.gp200recovery")).unwrap(),
        previous_bytes
    );
    until(&mut app, |app| {
        !app.recovery_ui.busy
            && app.recovery_ui.last_saved.as_deref() == Some(&app.controller.recovery_session())
    });
    assert!(app.firmware_recovery_draft_blocker().is_none());
    // The synthetic test never has an approved image; reaching this error
    // proves the real Saved acknowledgement now cleared only the draft gate.
    assert!(
        app.review_linux_recovery()
            .unwrap_err()
            .message
            .contains("firmware image")
    );
    assert_eq!(log.lock().unwrap().opens, 0);
    assert_eq!(log.lock().unwrap().offers, 0);
    assert_eq!(log.lock().unwrap().capture_observations, 0);
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn recovery_public_preflight_rejects_missing_busy_failed_uncertain_or_unsaved_drafts_before_freeze()
{
    for blocker in [
        "missing",
        "busy",
        "failed",
        "uncertain",
        "not-ready",
        "unsaved",
    ] {
        let (_directory, mut app, log) = fixture();
        until(&mut app, |app| {
            !app.recovery_ui.busy
                && app.recovery_ui.last_saved.as_deref() == Some(&app.controller.recovery_session())
        });
        let original = app.controller.recovery_session();
        match blocker {
            "missing" => app.services.draft_recovery = None,
            "busy" => app.recovery_ui.busy = true,
            "failed" => app.recovery_ui.error = Some("original discovery/storage error".into()),
            "uncertain" => {
                app.recovery_ui.visible_uncertain_hash =
                    Some("original visible but not durable snapshot".into())
            }
            "not-ready" => app.recovery_ui.ready = false,
            "unsaved" => app.recovery_ui.last_saved = None,
            _ => unreachable!(),
        }
        assert!(app.review_linux_recovery().is_err(), "{blocker}");
        assert!(
            !app.firmware_coordinator_view().can_review_linux_recovery,
            "{blocker}"
        );
        assert!(!app.services.firmware_paused(), "{blocker}");
        assert!(!app.firmware_close_blocked(), "{blocker}");
        assert!(
            !app.controller.firmware_editor_blocks_normal_actions(),
            "{blocker}"
        );
        assert!(
            app.firmware_coordinator.completion_preparation.is_none(),
            "{blocker}"
        );
        assert_eq!(app.controller.recovery_session(), original, "{blocker}");
        assert_eq!(log.lock().unwrap().offers, 0, "{blocker}");
        assert_eq!(log.lock().unwrap().opens, 0, "{blocker}");
    }
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn recovery_stop_joins_original_resource_helper_before_releasing_untouched_local_preparation() {
    for reply_error in [false, true] {
        let (directory, mut app, log) = fixture();
        until(&mut app, |app| {
            !app.recovery_ui.busy
                && app.recovery_ui.last_saved.as_deref() == Some(&app.controller.recovery_session())
        });
        let draft_bytes = std::fs::read(directory.0.join("drafts/session.gp200recovery")).unwrap();
        let original = app.controller.recovery_session();
        local_recovery_preparation_seam(&mut app);
        let (image, inputs) = synthetic_recovery_request_descriptions(&directory.0);
        let (release, waiting) = std::sync::mpsc::channel();
        let (send, received) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            waiting.recv().unwrap();
            let reply = if reply_error {
                Err(ServiceError::invalid(
                    "original resource load error after Stop",
                ))
            } else {
                Ok(Box::new((image, inputs, None)))
            };
            send.send(CompletionPreparationReply::RecoveryInputs(reply))
                .unwrap();
        });
        app.firmware_coordinator.completion_preparation = Some(CompletionPreparation {
            received,
            thread: Some(thread),
            generation: app.firmware_coordinator.generation,
        });
        app.stop_firmware_preparation();
        app.stop_firmware_preparation();
        assert_eq!(
            app.firmware_coordinator.phase,
            FirmwareCoordinatorPhase::StopRequested
        );
        assert_eq!(
            app.controller.state().firmware_editor.phase,
            gp200_app::FirmwareEditorPhase::Preparing
        );
        frame(&mut app);
        assert!(app.services.firmware_paused());
        assert!(app.firmware_close_blocked());
        assert!(app.firmware_coordinator.completion_preparation.is_some());
        assert!(app.firmware_coordinator.recovery_pending.is_none());
        assert_eq!(
            app.firmware_coordinator
                .library
                .as_ref()
                .unwrap()
                .runtime
                .retirement_state(),
            LibraryStoreRetirementState::Running
        );
        assert!(LibraryStore::open(directory.0.join("library")).is_err());
        release.send(()).unwrap();
        until(&mut app, |app| !app.firmware_close_blocked());
        assert!(app.firmware_coordinator.completion_preparation.is_none());
        assert!(app.firmware_coordinator.recovery_pending.is_none());
        assert!(app.firmware_coordinator.runtime_request.is_none());
        assert!(app.firmware_coordinator.retirement.is_none());
        assert!(app.firmware_coordinator.completion_monitor.is_none());
        assert!(!app.firmware_coordinator.entry_gate_held);
        assert!(!app.services.firmware_paused());
        assert!(!app.controller.firmware_editor_blocks_normal_actions());
        assert_eq!(app.controller.state().connection, ConnectionState::Offline);
        assert_eq!(app.controller.recovery_session(), original);
        assert_eq!(
            std::fs::read(directory.0.join("drafts/session.gp200recovery")).unwrap(),
            draft_bytes
        );
        assert_eq!(
            app.firmware_coordinator
                .library
                .as_ref()
                .unwrap()
                .runtime
                .retirement_state(),
            LibraryStoreRetirementState::Running
        );
        assert!(LibraryStore::open(directory.0.join("library")).is_err());
        assert!(app.linux_recovery_outcome().is_none());
        assert_eq!(log.lock().unwrap().opens, 0);
        assert_eq!(log.lock().unwrap().offers, 0);
        assert_eq!(log.lock().unwrap().capture_observations, 0);
    }
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn recovery_pre_owner_cancel_waits_original_runtime_queues_and_never_claims_safe_normal() {
    let (_directory, mut app, log) = fixture();
    until(&mut app, |app| {
        !app.recovery_ui.busy && app.recovery_ui.last_saved.is_some()
    });
    let original = app.controller.recovery_session();
    local_recovery_preparation_seam(&mut app);
    app.services
        .enqueue(vec![Effect::Device(DeviceCommand::Discover)]);
    assert!(!app.services.firmware_queues_settled());
    app.stop_firmware_preparation();
    app.firmware_coordinator
        .poll(&mut app.controller, &mut app.services, None, true);
    assert!(app.firmware_close_blocked());
    assert!(app.services.firmware_paused());
    assert_eq!(
        app.controller.state().firmware_editor.phase,
        gp200_app::FirmwareEditorPhase::Preparing
    );
    for command in app.services.take_unoffered_controls() {
        app.controller.settle_unoffered_device_control(command);
    }
    until(&mut app, |app| !app.firmware_close_blocked());
    assert_eq!(app.controller.recovery_session(), original);
    assert_eq!(app.controller.state().connection, ConnectionState::Offline);
    assert!(app.controller.state().identity.is_none());
    assert!(app.linux_recovery_outcome().is_none());
    assert_eq!(log.lock().unwrap().opens, 0);
    assert_eq!(log.lock().unwrap().offers, 0);
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn recovery_pre_owner_cancel_timeout_keeps_original_unsealed_work_and_admission_held() {
    let (_directory, mut app, log) = fixture();
    until(&mut app, |app| {
        !app.recovery_ui.busy && app.recovery_ui.last_saved.is_some()
    });
    local_recovery_preparation_seam(&mut app);
    app.stop_firmware_preparation();
    app.firmware_coordinator
        .recovery_preparation_cancel_requested = Some(Instant::now() - Duration::from_secs(11));
    app.firmware_coordinator
        .poll(&mut app.controller, &mut app.services, None, false);
    assert_eq!(
        app.firmware_coordinator.phase,
        FirmwareCoordinatorPhase::RecoveryRequired
    );
    assert!(app.services.firmware_paused());
    assert!(app.firmware_close_blocked());
    assert!(app.firmware_coordinator.library.is_some());
    assert!(app.firmware_coordinator.retirement.is_none());
    assert!(app.linux_recovery_outcome().is_none());
    assert_eq!(log.lock().unwrap().opens, 0);
    assert_eq!(log.lock().unwrap().offers, 0);
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn recovery_pre_owner_cancel_cannot_release_sealed_or_retiring_original_store() {
    for retired in [false, true] {
        let (directory, mut app, log) = fixture();
        until(&mut app, |app| {
            !app.recovery_ui.busy && app.recovery_ui.last_saved.is_some()
        });
        local_recovery_preparation_seam(&mut app);
        let editor = app.firmware_coordinator.editor.as_ref().unwrap();
        let source = app.firmware_coordinator.source.as_ref().unwrap();
        app.controller
            .retain_firmware_local_draft_excluded(editor, &source.recovery)
            .unwrap();
        app.controller
            .seal_firmware_recovery_preparation(editor, &source.recovery)
            .unwrap();
        if retired {
            app.firmware_coordinator
                .begin_retirement(StorePurpose::LinuxRecovery)
                .unwrap();
        }
        app.stop_firmware_preparation();
        assert!(
            app.firmware_coordinator
                .recovery_preparation_cancel_requested
                .is_none()
        );
        assert!(app.services.firmware_paused());
        assert!(app.firmware_close_blocked());
        assert!(app.controller.firmware_editor_blocks_normal_actions());
        assert_eq!(
            app.firmware_coordinator.phase,
            FirmwareCoordinatorPhase::StopRequested
        );
        assert!(app.return_from_firmware_preparation().is_err());
        assert_eq!(log.lock().unwrap().offers, 0);
        assert_eq!(log.lock().unwrap().opens, 0);
        if !retired {
            assert!(LibraryStore::open(directory.0.join("library")).is_err());
        }
    }
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn recovery_pre_owner_cancel_rejects_an_actual_unobserved_device_ticket() {
    let (directory, mut app, log) = fixture();
    let other_root = directory.0.join("other-owner-store");
    let (image, inputs) = synthetic_recovery_request_descriptions(&directory.0);
    let (mut request, _) = LinuxRecoveryRequest::new(
        image,
        inputs,
        None,
        SessionEpoch(0),
        LibraryStore::open(other_root.clone()).unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match app
            .firmware_coordinator
            .device
            .as_mut()
            .unwrap()
            .submit_recovery(request)
        {
            Ok(()) => break,
            Err(SubmitError::Busy(original)) => request = original,
            Err(_) => panic!("simulator owner service disconnected"),
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    // The actual ticket is still owned even if its descriptive summary is not
    // published (or its rejection event has not yet been observed).
    assert!(
        app.firmware_coordinator
            .device
            .as_ref()
            .unwrap()
            .has_owner_ticket()
    );
    local_recovery_preparation_seam(&mut app);
    app.stop_firmware_preparation();
    assert!(
        app.firmware_coordinator
            .recovery_preparation_cancel_requested
            .is_none()
    );
    assert!(app.services.firmware_paused());
    assert!(app.firmware_close_blocked());
    loop {
        if let Some(event) = app.firmware_coordinator.device.as_mut().unwrap().poll() {
            let FirmwareOwnerEvent::Rejected { store, .. } = event else {
                panic!("unexpected simulator recovery event")
            };
            assert_eq!(store.root(), other_root);
            drop(store);
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(app.services.firmware_paused());
    assert!(app.firmware_close_blocked());
    assert!(LibraryStore::open(directory.0.join("library")).is_err());
    assert_eq!(log.lock().unwrap().offers, 0);
    assert_eq!(log.lock().unwrap().opens, 0);
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn recovery_pre_owner_cancel_cannot_release_an_actually_retired_and_moved_original_store() {
    let (directory, mut app, log) = fixture();
    let original_store = prepare_disconnected_recovery_return_seam(&mut app);
    assert_eq!(original_store.root(), directory.0.join("library"));
    assert!(app.firmware_coordinator.library.is_none());
    app.firmware_coordinator.retained_store = Some(original_store);
    app.stop_firmware_preparation();
    assert!(
        app.firmware_coordinator
            .recovery_preparation_cancel_requested
            .is_none()
    );
    assert!(app.services.firmware_paused());
    assert!(app.firmware_close_blocked());
    assert!(app.controller.firmware_editor_blocks_normal_actions());
    assert_eq!(
        app.firmware_coordinator
            .retained_store
            .as_ref()
            .unwrap()
            .root(),
        directory.0.join("library")
    );
    assert!(app.return_from_firmware_preparation().is_err());
    assert_eq!(log.lock().unwrap().offers, 0);
    assert_eq!(log.lock().unwrap().opens, 0);
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn submit_original_store_for_host_close_event_seam(
    app: &mut StudioApp,
    directory: &std::path::Path,
) {
    // A real simulator ticket and real relinquished original store. Only its
    // returned event DTO is substituted, never a native opener or physical owner.
    until(app, |app| {
        !app.recovery_ui.busy
            && app.recovery_ui.last_saved.as_deref() == Some(&app.controller.recovery_session())
            && app.services.firmware_queues_settled()
            && app.firmware_producers_settled()
    });
    let original = prepare_disconnected_recovery_return_seam(app);
    let (image, inputs) = synthetic_recovery_request_descriptions(directory);
    let (mut request, monitor) =
        LinuxRecoveryRequest::new(image, inputs, None, SessionEpoch(0), original);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match app
            .firmware_coordinator
            .device
            .as_mut()
            .unwrap()
            .submit_recovery(request)
        {
            Ok(()) => break,
            Err(SubmitError::Busy(original)) => request = original,
            Err(_) => panic!("actual simulator service disconnected before admission"),
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut outcome = recovery_return_description(SessionEpoch(0), false);
    outcome.view.run =
        "cfg(test) expired/rejected query returned-event seam; no physical owner".into();
    outcome.view.diagnostic =
        Some("cfg(test) finite query permission expired with zero writes".into());
    outcome.verified_normal_epoch = None;
    outcome.selection = None;
    let summary = FirmwareOwnerSummary {
        phase: format!("Linux completion: {:?}", outcome.view.phase),
        simulated_source_bytes: 0,
        os_offered_bytes: outcome.view.os_offered_bytes,
        device_verified_bytes: 0,
        completion_verified: false,
        quarantined: outcome.view.quarantined,
        diagnostic: outcome.view.diagnostic.clone(),
        retained_raw_chunks: outcome.view.retained_raw_chunks.clone(),
        allocated_raw_frontier: outcome.view.allocated_raw_frontier,
    };
    app.firmware_coordinator.recovery_history = outcome.history.clone();
    app.firmware_coordinator.completion_monitor = Some(monitor);
    app.firmware_coordinator.phase = FirmwareCoordinatorPhase::Owned;
    app.firmware_coordinator
        .device
        .as_mut()
        .unwrap()
        .test_substitute_rejected_recovery_return(summary, outcome);
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
pub(super) fn prepare_settled_recovery_host_close_event_seam(
    app: &mut StudioApp,
    directory: &std::path::Path,
) {
    submit_original_store_for_host_close_event_seam(app, directory);
    until(app, |app| app.firmware_coordinator.library_restored);
    assert_eq!(
        app.controller.state().connection,
        ConnectionState::Recovering
    );
    assert!(app.controller.state().firmware_editor.source_changed);
    assert!(
        app.can_close_settled_recovery_host(),
        "{:?}",
        app.firmware_coordinator_view()
    );
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn close_output(output: &egui::FullOutput, close: bool) -> bool {
    output.viewport_output.values().any(|viewport| {
        viewport.commands.iter().any(|command| {
            if close {
                matches!(command, egui::ViewportCommand::Close)
            } else {
                matches!(command, egui::ViewportCommand::CancelClose)
            }
        })
    })
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn settled_failed_recovery_explicit_host_close_preserves_saved_draft_original_store_and_history() {
    let (directory, mut app, log) = fixture();
    let registry = directory.0.join("private-host-close-registry");
    let attempt = write_history_begin(&directory.0.join("library"), &registry, 'a', &[]);
    let journal = directory
        .0
        .join("library/linux-firmware-completion")
        .join(format!("{}.journal", attempt.as_str()));
    let quarantine = registry.join(format!("{}.quarantine", attempt.as_str()));
    let original_journal = std::fs::read(&journal).unwrap();
    let original_quarantine = std::fs::read(&quarantine).unwrap();
    prepare_settled_recovery_host_close_event_seam(&mut app, &directory.0);
    let draft = app.controller.recovery_session();
    let draft_bytes = std::fs::read(directory.0.join("drafts/session.gp200recovery")).unwrap();
    let outcome_before = serde_json::to_vec(&app.linux_recovery_outcome()).unwrap();
    let ctx = egui::Context::default();
    let mut ordinary_close = ctx.run_ui(Default::default(), |ui| {
        app.request_application_close(ui.ctx())
    });
    ordinary_close.textures_delta.clear();
    assert!(close_output(&ordinary_close, false));
    assert!(!close_output(&ordinary_close, true));
    assert!(app.firmware_close_blocked());
    let mut explicit = ctx.run_ui(Default::default(), |ui| {
        app.close_settled_recovery_host(ui.ctx()).unwrap();
    });
    explicit.textures_delta.clear();
    assert!(close_output(&explicit, true));
    assert!(!close_output(&explicit, false));
    let mut rechecked = ctx.run_ui(Default::default(), |ui| {
        app.intercept_firmware_application_close(ui.ctx())
    });
    rechecked.textures_delta.clear();
    assert!(!close_output(&rechecked, false));
    assert!(app.firmware_close_blocked());
    assert!(
        app.services.firmware_paused() && app.controller.firmware_editor_blocks_normal_actions()
    );
    assert!(app.firmware_coordinator.entry_gate_held);
    assert_eq!(
        app.firmware_coordinator.phase,
        FirmwareCoordinatorPhase::RecoveryRequired
    );
    assert_eq!(
        app.controller.state().firmware_editor.phase,
        gp200_app::FirmwareEditorPhase::RecoveryRequired
    );
    assert_eq!(
        app.controller.state().connection,
        ConnectionState::Recovering
    );
    assert!(app.controller.state().firmware_editor.source_changed);
    assert!(app.return_from_firmware_preparation().is_err());
    assert!(LibraryStore::open(directory.0.join("library")).is_err());
    assert_eq!(
        serde_json::to_vec(&app.linux_recovery_outcome()).unwrap(),
        outcome_before
    );
    assert_eq!(app.controller.recovery_session(), draft);
    assert_eq!(
        std::fs::read(directory.0.join("drafts/session.gp200recovery")).unwrap(),
        draft_bytes
    );
    assert_eq!(std::fs::read(&journal).unwrap(), original_journal);
    assert_eq!(std::fs::read(&quarantine).unwrap(), original_quarantine);
    assert_eq!(log.lock().unwrap().opens, 0);
    assert_eq!(log.lock().unwrap().offers, 0);
    assert_eq!(log.lock().unwrap().capture_observations, 0);
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn settled_recovery_host_close_requires_consumed_ticket_and_actual_running_same_store_restore() {
    let (directory, mut app, log) = fixture();
    submit_original_store_for_host_close_event_seam(&mut app, &directory.0);
    assert!(
        app.firmware_coordinator
            .device
            .as_ref()
            .unwrap()
            .has_owner_ticket()
    );
    assert!(!app.can_close_settled_recovery_host());
    let ctx = egui::Context::default();
    let mut refused = ctx.run_ui(Default::default(), |ui| {
        assert!(app.close_settled_recovery_host(ui.ctx()).is_err());
    });
    refused.textures_delta.clear();
    assert!(close_output(&refused, false));
    assert!(!close_output(&refused, true));
    let deadline = Instant::now() + Duration::from_secs(10);
    while app.firmware_coordinator.recovery_outcome.is_none() {
        app.firmware_coordinator
            .poll_owner(&mut app.controller, &app.services);
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        !app.firmware_coordinator
            .device
            .as_ref()
            .unwrap()
            .has_owner_ticket()
    );
    assert!(app.firmware_coordinator.restoration.is_some());
    assert!(app.firmware_coordinator.library.is_none());
    assert!(!app.can_close_settled_recovery_host());
    until(&mut app, |app| app.firmware_coordinator.library_restored);
    assert!(app.can_close_settled_recovery_host());
    // Actual restored writer accepts a new relinquishment; it must immediately
    // revoke host-close eligibility even though the previous return proof exists.
    app.firmware_coordinator
        .begin_retirement(StorePurpose::Recovery)
        .unwrap();
    assert!(!app.can_close_settled_recovery_host());
    assert!(app.firmware_coordinator.runtime_request.is_some());
    until(&mut app, |app| {
        app.firmware_coordinator.library_restored
            && app.firmware_coordinator.library.is_some()
            && app.firmware_coordinator.runtime_request.is_none()
            && app.firmware_coordinator.retirement.is_none()
            && app.firmware_coordinator.restoration.is_none()
    });
    assert!(app.can_close_settled_recovery_host());
    assert!(app.services.firmware_paused());
    assert!(app.firmware_close_blocked());
    assert_eq!(log.lock().unwrap().opens, 0);
    assert_eq!(log.lock().unwrap().offers, 0);
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn settled_recovery_host_close_rejects_direct_return_projection_and_stale_owner_bindings() {
    for mutation in [
        "projection",
        "direct-return",
        "editor-generation",
        "editor-lease",
        "runtime-generation",
        "source-epoch",
        "current-epoch",
        "store-root",
        "history",
        "outcome",
        "original-summary",
        "restore-ack",
    ] {
        let (directory, mut app, log) = fixture();
        if mutation == "projection" {
            let outcome = recovery_return_description(SessionEpoch(0), false);
            app.firmware_coordinator
                .test_recovery_projection(outcome.view, true);
        } else if mutation == "direct-return" {
            let store = prepare_disconnected_recovery_return_seam(&mut app);
            app.firmware_coordinator.accept_recovery_return(
                recovery_summary_description(),
                store,
                recovery_return_description(SessionEpoch(0), false),
                &mut app.controller,
                &app.services,
            );
            until(&mut app, |app| app.firmware_coordinator.library_restored);
        } else {
            prepare_settled_recovery_host_close_event_seam(&mut app, &directory.0);
            match mutation {
                "editor-generation" => app.firmware_coordinator.generation += 1,
                "editor-lease" => {
                    let mut other = AppController::with_demo(
                        gp200_sim::captured_demo_snapshot(),
                        AppPreferences::default(),
                    )
                    .unwrap();
                    app.firmware_coordinator.editor =
                        Some(other.begin_firmware_recovery_preparation().unwrap());
                }
                "runtime-generation" => {
                    app.firmware_coordinator
                        .runtime
                        .as_mut()
                        .unwrap()
                        .generation += 1
                }
                "source-epoch" => {
                    app.firmware_coordinator.source.as_mut().unwrap().epoch = SessionEpoch(99)
                }
                "current-epoch" => {
                    let retained = app.controller.recovery_session();
                    let next = SessionEpoch(app.controller.state().epoch.0 + 1);
                    let effects =
                        app.controller
                            .dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
                                epoch: next,
                                state: ConnectionState::Recovering,
                            }));
                    assert!(effects.is_empty());
                    assert_eq!(app.controller.recovery_session(), retained);
                }
                "store-root" => {
                    app.firmware_coordinator.recovery_store_root =
                        Some(directory.0.join("wrong-original-root"))
                }
                "history" => app.firmware_coordinator.recovery_history = None,
                "outcome" => app
                    .firmware_coordinator
                    .recovery_outcome
                    .as_mut()
                    .unwrap()
                    .view
                    .run
                    .push_str("stale"),
                "original-summary" => app.firmware_coordinator.owner_originals.clear(),
                "restore-ack" => app.firmware_coordinator.library_restored = false,
                _ => unreachable!(),
            }
        }
        assert!(!app.can_close_settled_recovery_host(), "{mutation}");
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(Default::default(), |ui| {
            assert!(
                app.close_settled_recovery_host(ui.ctx()).is_err(),
                "{mutation}"
            );
        });
        output.textures_delta.clear();
        // A presentation-only outcome is Idle, so ordinary close stays allowed;
        // it still cannot authorize the separate recovery-held close action.
        if mutation != "projection" {
            assert!(close_output(&output, false), "{mutation}");
            assert!(!close_output(&output, true), "{mutation}");
            assert!(app.services.firmware_paused(), "{mutation}");
        }
        assert_eq!(log.lock().unwrap().opens, 0, "{mutation}");
        assert_eq!(log.lock().unwrap().offers, 0, "{mutation}");
    }
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn settled_recovery_host_close_rechecks_saved_draft_producers_and_original_queued_work() {
    for blocker in [
        "unsaved",
        "previous-drafts",
        "draft-busy",
        "draft-error",
        "draft-uncertain",
        "draft-not-ready",
        "draft-worker",
        "held-choice",
        "producer",
        "runtime-queue",
        "capture-control",
    ] {
        let (directory, mut app, log) = fixture();
        prepare_settled_recovery_host_close_event_seam(&mut app, &directory.0);
        let ctx = egui::Context::default();
        let mut accepted = ctx.run_ui(Default::default(), |ui| {
            app.close_settled_recovery_host(ui.ctx()).unwrap()
        });
        accepted.textures_delta.clear();
        assert!(close_output(&accepted, true));
        match blocker {
            "unsaved" => app.recovery_ui.last_saved = None,
            "previous-drafts" => {
                let retained = DraftRecoveryStore::open(directory.0.join("other-original-drafts"))
                    .unwrap()
                    .save(&app.controller.recovery_session())
                    .unwrap();
                app.recovery_ui.found = Some(retained);
            }
            "draft-busy" => app.recovery_ui.busy = true,
            "draft-error" => app.recovery_ui.error = Some("retained original draft failure".into()),
            "draft-uncertain" => {
                app.recovery_ui.visible_uncertain_hash = Some("uncertain actual publication".into())
            }
            "draft-not-ready" => app.recovery_ui.ready = false,
            "draft-worker" => app.services.draft_recovery = None,
            "held-choice" => app.receive_file_selection(runtime::FileSelection {
                purpose: FilePurpose::ImportPresets,
                path: Some("held-original-file.prst".into()),
            }),
            "producer" => app.bank_scan.running = true,
            "runtime-queue" => app
                .services
                .enqueue(vec![Effect::Device(DeviceCommand::Discover)]),
            "capture-control" => {
                app.firmware_coordinator.control =
                    Some(FirmwareBackupCaptureCommand::ReturnToNormal)
            }
            _ => unreachable!(),
        }
        assert!(!app.can_close_settled_recovery_host(), "{blocker}");
        let mut rechecked = ctx.run_ui(Default::default(), |ui| {
            app.intercept_firmware_application_close(ui.ctx())
        });
        rechecked.textures_delta.clear();
        assert!(close_output(&rechecked, false), "{blocker}");
        assert!(!app.firmware_settled_recovery_close_requested, "{blocker}");
        assert!(app.firmware_close_request_pending(), "{blocker}");
        assert!(
            app.firmware_close_blocked() && app.services.firmware_paused(),
            "{blocker}"
        );
        assert_eq!(log.lock().unwrap().opens, 0, "{blocker}");
        assert_eq!(log.lock().unwrap().offers, 0, "{blocker}");
    }
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn settled_recovery_host_close_waits_original_phase_helper_to_finish_and_join() {
    let (directory, mut app, log) = fixture();
    prepare_settled_recovery_host_close_event_seam(&mut app, &directory.0);
    let (release, waiting) = std::sync::mpsc::channel();
    let (send, received) = std::sync::mpsc::sync_channel(1);
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_finished = finished.clone();
    let thread = std::thread::spawn(move || {
        waiting.recv().unwrap();
        send.send(CompletionPreparationReply::Signal(Ok(())))
            .unwrap();
        worker_finished.store(true, Ordering::Release);
    });
    app.firmware_coordinator.completion_preparation = Some(CompletionPreparation {
        received,
        thread: Some(thread),
        generation: app.firmware_coordinator.generation,
    });
    assert!(!app.can_close_settled_recovery_host());
    let ctx = egui::Context::default();
    let mut rejected = ctx.run_ui(Default::default(), |ui| {
        assert!(app.close_settled_recovery_host(ui.ctx()).is_err())
    });
    rejected.textures_delta.clear();
    assert!(close_output(&rejected, false));
    release.send(()).unwrap();
    until(&mut app, |app| {
        app.firmware_coordinator.completion_preparation.is_none()
    });
    assert!(finished.load(Ordering::Acquire));
    assert!(app.can_close_settled_recovery_host());
    assert!(app.services.firmware_paused() && app.firmware_close_blocked());
    assert_eq!(log.lock().unwrap().opens, 0);
    assert_eq!(log.lock().unwrap().offers, 0);
}

#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn settled_recovery_host_close_rejects_a_new_genuine_unobserved_simulator_ticket() {
    let (directory, mut app, log) = fixture();
    prepare_settled_recovery_host_close_event_seam(&mut app, &directory.0);
    let (image, inputs) = synthetic_recovery_request_descriptions(&directory.0);
    let other_root = directory.0.join("new-genuine-simulator-owner");
    let (mut request, _) = LinuxRecoveryRequest::new(
        image,
        inputs,
        None,
        SessionEpoch(0),
        LibraryStore::open(&other_root).unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match app
            .firmware_coordinator
            .device
            .as_mut()
            .unwrap()
            .submit_recovery(request)
        {
            Ok(()) => break,
            Err(SubmitError::Busy(original)) => request = original,
            Err(_) => panic!("actual simulator endpoint disconnected"),
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        app.firmware_coordinator
            .device
            .as_ref()
            .unwrap()
            .has_owner_ticket()
    );
    assert!(!app.can_close_settled_recovery_host());
    let ctx = egui::Context::default();
    let mut rejected = ctx.run_ui(Default::default(), |ui| {
        assert!(app.close_settled_recovery_host(ui.ctx()).is_err())
    });
    rejected.textures_delta.clear();
    assert!(close_output(&rejected, false));
    loop {
        if let Some(event) = app.firmware_coordinator.device.as_mut().unwrap().poll() {
            let FirmwareOwnerEvent::Rejected { store, .. } = event else {
                panic!("unexpected genuine simulator event")
            };
            assert_eq!(store.root(), other_root);
            drop(store);
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(app.services.firmware_paused() && app.firmware_close_blocked());
    assert_eq!(log.lock().unwrap().opens, 0);
    assert_eq!(log.lock().unwrap().offers, 0);
}

// This seam retains actual normal capture, original writer retirement/join,
// simulator ticket consumption and store restoration. Only the returned
// LinuxCompletionOutcome event description is substituted; no physical Linux
// owner, entry, firmware query, transfer or authority exists in this fixture.
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn submit_original_completion_store_for_host_close_event_seam(app: &mut StudioApp) {
    ready(app);
    let epoch = app.controller.state().epoch;
    app.begin_firmware_preparation().unwrap();
    // Preserve the real normal current-buffer document and exact retained local
    // session; this exclusion is not a removal or replacement of the draft.
    app.retain_local_draft_for_firmware_capture().unwrap();
    app.capture_firmware_backup(context()).unwrap();
    until(app, |app| {
        app.firmware_coordinator.phase == FirmwareCoordinatorPhase::ReadOnlyReview
    });
    let (store, _) = genuine_store_and_normal_retirement_for_completion_projection(app);
    app.firmware_coordinator.purpose = Some(StorePurpose::LinuxCompletion);
    let mut request = FirmwareOwnershipRequest::new(descriptive_fixture_job(), store);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match app
            .firmware_coordinator
            .device
            .as_mut()
            .unwrap()
            .submit_completion(request)
        {
            Ok(()) => break,
            Err(SubmitError::Busy(original)) => request = original,
            Err(_) => panic!("actual simulator endpoint disconnected"),
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut outcome = completion_return_projection(epoch, epoch, false);
    outcome.view.run =
        "cfg(test) failed normal-start completion returned-event seam; no physical owner".into();
    outcome.verified_normal_epoch = None;
    outcome.selection = None;
    let summary = FirmwareOwnerSummary {
        phase: format!("Linux completion: {:?}", outcome.view.phase),
        simulated_source_bytes: 0,
        os_offered_bytes: outcome.view.os_offered_bytes,
        device_verified_bytes: 0,
        completion_verified: false,
        quarantined: outcome.view.quarantined,
        diagnostic: outcome.view.diagnostic.clone(),
        retained_raw_chunks: outcome.view.retained_raw_chunks.clone(),
        allocated_raw_frontier: outcome.view.allocated_raw_frontier,
    };
    app.firmware_coordinator.entry_gate_held = true;
    app.firmware_coordinator.phase = FirmwareCoordinatorPhase::Owned;
    app.firmware_coordinator
        .device
        .as_mut()
        .unwrap()
        .test_substitute_rejected_completion_return(summary, outcome);
}
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn prepare_settled_completion_host_close_event_seam(app: &mut StudioApp) {
    submit_original_completion_store_for_host_close_event_seam(app);
    until(app, |app| app.firmware_coordinator.library_restored);
    assert_eq!(
        app.controller.state().connection,
        ConnectionState::Recovering
    );
    assert!(app.controller.state().firmware_editor.source_changed);
    assert!(
        app.can_close_settled_recovery_host(),
        "{:?}",
        app.firmware_coordinator_view()
    );
}
#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn failed_normal_completion_explicit_host_close_requires_consumed_ticket_and_preserves_capture_hold()
 {
    let (directory, mut app, log) = fixture();
    submit_original_completion_store_for_host_close_event_seam(&mut app);
    let (opens, offers) = {
        let log = log.lock().unwrap();
        (log.opens, log.offers)
    };
    assert!(
        app.firmware_coordinator
            .device
            .as_ref()
            .unwrap()
            .has_owner_ticket()
    );
    assert!(!app.can_close_settled_recovery_host());
    let ctx = egui::Context::default();
    let mut refused = ctx.run_ui(Default::default(), |ui| {
        assert!(app.close_settled_recovery_host(ui.ctx()).is_err());
    });
    refused.textures_delta.clear();
    assert!(close_output(&refused, false));
    assert!(!close_output(&refused, true));
    let deadline = Instant::now() + Duration::from_secs(10);
    while app.firmware_coordinator.completion_outcome.is_none() {
        app.firmware_coordinator
            .poll_owner(&mut app.controller, &app.services);
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        !app.firmware_coordinator
            .device
            .as_ref()
            .unwrap()
            .has_owner_ticket()
    );
    assert!(app.firmware_coordinator.restoration.is_some());
    assert!(!app.can_close_settled_recovery_host());
    until(&mut app, |app| app.firmware_coordinator.library_restored);
    assert!(app.can_close_settled_recovery_host());
    let source = app.controller.recovery_session();
    let closed = serde_json::to_vec(&app.firmware_coordinator.closed).unwrap();
    assert!(app.firmware_coordinator.published.is_none());
    let outcome = serde_json::to_vec(&app.linux_completion_outcome()).unwrap();
    let facts = app.firmware_coordinator.facts.as_ref().unwrap();
    let evidence_paths = [&facts.archive, &facts.sidecar, &facts.index]
        .into_iter()
        .map(|path| (path.clone(), std::fs::read(path).unwrap()))
        .collect::<Vec<_>>();
    // Dedicated placeholders demonstrate preserved local bytes, not a physical
    // journal or quarantine result.
    let journal = directory.0.join("described-completion.journal");
    let quarantine = directory.0.join("described-completion.quarantine");
    std::fs::write(&journal, b"test fixture retained journal bytes").unwrap();
    std::fs::write(&quarantine, b"test fixture retained quarantine bytes").unwrap();
    for phase in [
        FirmwareCoordinatorPhase::RecoveryRequired,
        FirmwareCoordinatorPhase::StopRequested,
    ] {
        app.firmware_coordinator.phase = phase;
        assert!(app.can_close_settled_recovery_host());
        let mut output = ctx.run_ui(Default::default(), |ui| {
            app.close_settled_recovery_host(ui.ctx()).unwrap()
        });
        output.textures_delta.clear();
        assert!(close_output(&output, true));
        assert!(!close_output(&output, false));
        let mut rechecked = ctx.run_ui(Default::default(), |ui| {
            app.intercept_firmware_application_close(ui.ctx())
        });
        rechecked.textures_delta.clear();
        assert!(!close_output(&rechecked, false));
        assert_eq!(app.firmware_coordinator.phase, phase);
    }
    assert!(app.services.firmware_paused());
    assert!(app.firmware_coordinator.entry_gate_held && app.firmware_close_blocked());
    assert!(app.controller.firmware_editor_blocks_normal_actions());
    assert!(app.return_from_firmware_preparation().is_err());
    assert!(LibraryStore::open(directory.0.join("library")).is_err());
    assert_eq!(app.controller.recovery_session(), source);
    assert_eq!(
        serde_json::to_vec(&app.firmware_coordinator.closed).unwrap(),
        closed
    );
    assert!(app.firmware_coordinator.published.is_none());
    assert_eq!(
        serde_json::to_vec(&app.linux_completion_outcome()).unwrap(),
        outcome
    );
    for (path, bytes) in evidence_paths {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    assert_eq!(
        std::fs::read(journal).unwrap(),
        b"test fixture retained journal bytes"
    );
    assert_eq!(
        std::fs::read(quarantine).unwrap(),
        b"test fixture retained quarantine bytes"
    );
    assert_eq!(log.lock().unwrap().opens, opens);
    assert_eq!(log.lock().unwrap().offers, offers);
}
#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn settled_completion_host_close_rejects_stale_kind_source_original_root_outcome_and_return_description()
 {
    let (directory, mut app, log) = fixture();
    prepare_settled_completion_host_close_event_seam(&mut app);
    let (opens, offers) = {
        let log = log.lock().unwrap();
        (log.opens, log.offers)
    };
    let original_outcome = app.firmware_coordinator.completion_outcome.clone();
    for mutation in [
        "generation",
        "runtime-generation",
        "source-epoch",
        "source-documents",
        "store-root",
        "return-kind",
        "outcome",
        "mixed-outcome",
        "source-purpose",
        "original-summary",
    ] {
        let generation = app.firmware_coordinator.generation;
        let runtime_generation = app
            .firmware_coordinator
            .runtime
            .as_ref()
            .unwrap()
            .generation;
        let source = app.firmware_coordinator.source.clone();
        let root = app.firmware_coordinator.recovery_store_root.clone();
        let saved_originals = if mutation == "original-summary" {
            Some(std::mem::take(
                &mut app.firmware_coordinator.owner_originals,
            ))
        } else {
            None
        };
        match mutation {
            "generation" => app.firmware_coordinator.generation += 1,
            "runtime-generation" => {
                app.firmware_coordinator
                    .runtime
                    .as_mut()
                    .unwrap()
                    .generation += 1
            }
            "source-epoch" => {
                app.firmware_coordinator.source.as_mut().unwrap().epoch = SessionEpoch(99)
            }
            "source-documents" => app
                .firmware_coordinator
                .source
                .as_mut()
                .unwrap()
                .recovery
                .documents
                .clear(),
            "store-root" => {
                app.firmware_coordinator.recovery_store_root = Some(directory.0.join("wrong-root"))
            }
            "return-kind" => app.firmware_coordinator.recovery_editor = true,
            "outcome" => app
                .firmware_coordinator
                .completion_outcome
                .as_mut()
                .unwrap()
                .view
                .run
                .push_str(" stale"),
            "mixed-outcome" => {
                app.firmware_coordinator.recovery_outcome =
                    Some(recovery_return_description(SessionEpoch(0), false))
            }
            "source-purpose" => {
                app.firmware_coordinator.purpose = Some(StorePurpose::LinuxCompletion)
            }
            "original-summary" => {}
            _ => unreachable!(),
        }
        assert!(!app.can_close_settled_recovery_host(), "{mutation}");
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(Default::default(), |ui| {
            assert!(
                app.close_settled_recovery_host(ui.ctx()).is_err(),
                "{mutation}"
            )
        });
        output.textures_delta.clear();
        assert!(close_output(&output, false), "{mutation}");
        assert!(!close_output(&output, true), "{mutation}");
        app.firmware_coordinator.generation = generation;
        app.firmware_coordinator
            .runtime
            .as_mut()
            .unwrap()
            .generation = runtime_generation;
        app.firmware_coordinator.source = source;
        app.firmware_coordinator.recovery_store_root = root;
        app.firmware_coordinator.recovery_editor = false;
        app.firmware_coordinator.completion_outcome = original_outcome.clone();
        app.firmware_coordinator.recovery_outcome = None;
        app.firmware_coordinator.purpose = Some(StorePurpose::Recovery);
        if let Some(originals) = saved_originals {
            app.firmware_coordinator.owner_originals = originals;
        }
        assert!(
            app.can_close_settled_recovery_host(),
            "after restoring {mutation}"
        );
    }
    // Direct re-acceptance of even identical DTO/store is not a consumed owner
    // event and must revoke the old proof. Re-retire and return that exact store.
    let closed_outcome = app.firmware_coordinator.completion_outcome.clone().unwrap();
    app.firmware_coordinator
        .begin_retirement(StorePurpose::Recovery)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while app.firmware_coordinator.retirement.is_none() {
        app.firmware_coordinator.poll_runtime(&mut app.controller);
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    let store = loop {
        if let Some(result) = app
            .firmware_coordinator
            .retirement
            .as_mut()
            .unwrap()
            .try_take_result()
        {
            break result.unwrap();
        }
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    };
    app.firmware_coordinator.retirement = None;
    app.services.retire_library_clients();
    app.firmware_coordinator.library = None;
    app.firmware_coordinator.accept_completion_return(
        recovery_summary_description(),
        store,
        closed_outcome,
        &mut app.controller,
        &app.services,
    );
    until(&mut app, |app| app.firmware_coordinator.library_restored);
    assert!(!app.can_close_settled_recovery_host());
    assert!(app.services.firmware_paused() && app.firmware_close_blocked());
    assert_eq!(log.lock().unwrap().opens, opens);
    assert_eq!(log.lock().unwrap().offers, offers);
}
#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn completion_owner_return_cannot_mint_close_authority_for_another_coordinator_purpose() {
    let (_directory, mut app, log) = fixture();
    submit_original_completion_store_for_host_close_event_seam(&mut app);
    let (opens, offers) = {
        let log = log.lock().unwrap();
        (log.opens, log.offers)
    };
    app.firmware_coordinator.purpose = Some(StorePurpose::Update);
    until(&mut app, |app| app.firmware_coordinator.library_restored);
    assert!(app.firmware_coordinator.completion_outcome.is_some());
    assert!(!app.can_close_settled_recovery_host());
    assert!(app.firmware_coordinator.recovery_host_return.is_none());
    assert!(app.services.firmware_paused() && app.firmware_close_blocked());
    assert_eq!(log.lock().unwrap().opens, opens);
    assert_eq!(log.lock().unwrap().offers, offers);
}
#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn settled_both_linux_owner_host_close_waits_accepted_file_work_and_reply_handling() {
    for completion in [false, true] {
        let (directory, mut app, log) = fixture();
        if completion {
            prepare_settled_completion_host_close_event_seam(&mut app);
        } else {
            prepare_settled_recovery_host_close_event_seam(&mut app, &directory.0);
        }
        let (opens, offers) = {
            let log = log.lock().unwrap();
            (log.opens, log.offers)
        };
        let (running, finish_running) = app.services.test_queue_file_work_barrier();
        running.recv_timeout(Duration::from_secs(3)).unwrap();
        let (queued, finish_queued) = app.services.test_queue_file_work_barrier();
        app.services.test_queue_file_selection(
            FilePurpose::ImportPresets,
            Some(directory.0.join("held-original.prst")),
        );
        let (published, finish_published) = app.services.test_queue_file_work_barrier();
        assert!(!app.can_close_settled_recovery_host());
        finish_running.send(()).unwrap();
        queued.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(!app.can_close_settled_recovery_host());
        finish_queued.send(()).unwrap();
        published.recv_timeout(Duration::from_secs(3)).unwrap();
        // The original file-choice event is now actually published but unpumped.
        assert!(!app.can_close_settled_recovery_host());
        finish_published.send(()).unwrap();
        let (inputs, selections) = app.services.pump();
        assert!(inputs.is_empty());
        assert_eq!(selections.len(), 1);
        assert!(!app.can_close_settled_recovery_host());
        app.receive_file_selection(selections.into_iter().next().unwrap());
        assert_eq!(app.pending_firmware_file_choices(), 1);
        assert!(!app.can_close_settled_recovery_host());
        app.dismiss_pending_firmware_file_choices();
        until(&mut app, |app| app.services.file_worker_settled());
        assert!(app.can_close_settled_recovery_host());
        assert!(app.services.firmware_paused() && app.firmware_close_blocked());
        assert_eq!(log.lock().unwrap().opens, opens);
        assert_eq!(log.lock().unwrap().offers, offers);
    }
}
#[test]
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn settled_recovery_host_close_waits_original_busy_preference_retry_and_worker_completion() {
    let (directory, mut app, log) = fixture();
    prepare_settled_recovery_host_close_event_seam(&mut app, &directory.0);
    let (running, finish_running) = app.services.test_queue_file_work_barrier();
    running.recv_timeout(Duration::from_secs(3)).unwrap();
    let queued = (0..32)
        .map(|_| app.services.test_queue_file_work_barrier())
        .collect::<Vec<_>>();
    let preferences = AppPreferences {
        theme: ThemePreference::Light,
        scale: 1.25,
        ..Default::default()
    };
    app.services
        .enqueue(vec![Effect::PersistSettings(preferences.clone())]);
    let (inputs, selections) = app.services.pump();
    assert!(inputs.is_empty() && selections.is_empty());
    assert!(!app.can_close_settled_recovery_host());
    finish_running.send(()).unwrap();
    for (entered, finish) in queued {
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(!app.can_close_settled_recovery_host());
        finish.send(()).unwrap();
    }
    until(&mut app, |app| app.services.file_worker_settled());
    assert!(app.can_close_settled_recovery_host());
    let saved: AppPreferences =
        serde_json::from_slice(&std::fs::read(directory.0.join("preferences.json")).unwrap())
            .unwrap();
    assert_eq!(saved.theme, preferences.theme);
    assert_eq!(saved.scale, preferences.scale);
    assert!(app.services.firmware_paused() && app.firmware_close_blocked());
    assert_eq!(log.lock().unwrap().opens, 0);
    assert_eq!(log.lock().unwrap().offers, 0);
}
