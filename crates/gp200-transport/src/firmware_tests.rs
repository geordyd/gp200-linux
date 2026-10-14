use crate::{firmware_actor::*, firmware_backend::*, firmware_service::*, *};
use gp200_domain::*;
use gp200_library::*;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Duration, Instant},
};
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "gp200-fw03-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(crate) fn job() -> Arc<ReviewedFirmwareUpdate> {
    let mut b = vec![0; 96 + 260 + 3 + 4 + 5];
    b[..4].copy_from_slice(b"FRMW");
    let declared = (b.len() - 48) as u32;
    b[4..8].copy_from_slice(&declared.to_le_bytes());
    b[16..22].copy_from_slice(b"GP-200");
    b[33] = 2;
    b[88..92].copy_from_slice(b"V182");
    let mut offset = 96;
    for (i, n) in [260usize, 3, 4, 5].into_iter().enumerate() {
        b[48 + i * 8..52 + i * 8].copy_from_slice(&((offset - 48) as u32).to_le_bytes());
        b[52 + i * 8..56 + i * 8].copy_from_slice(&(n as u32).to_le_bytes());
        for (j, x) in b[offset..offset + n].iter_mut().enumerate() {
            *x = (j % 251) as u8;
        }
        offset += n;
    }
    b[14] = b[48..].iter().fold(0u8, |a, b| a.wrapping_add(*b));
    let ctx = FirmwareUpdateContextDto {
        profile_id: "synthetic only".into(),
        hardware_policy: "unknown".into(),
        update_mode: "mock".into(),
        normal: None,
        versions: vec![],
        backup_objects: vec![],
        loss_items: vec![],
        power_statement: "fixture".into(),
        connection_statement: "fixture".into(),
    };
    let docs = [
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
        bytes: Arc::from(&b"fixture only"[..]),
    })
    .collect();
    Arc::new(
        ReviewedFirmwareUpdate::prepare(
            RetainedJobBlob::checked(
                AssetId("test image".into()),
                b.clone().into(),
                JobSha256::of(&b),
            )
            .unwrap(),
            ctx,
            docs,
            1,
        )
        .unwrap(),
    )
}
fn binding() -> FirmwareUsbObservation {
    FirmwareUsbObservation {
        topology: "synthetic USB 1-2".into(),
        vendor: 1,
        product: 2,
        serial: "test serial".into(),
        interface: "synthetic interface".into(),
        endpoint: "test endpoint".into(),
    }
}
#[derive(Default)]
struct Log {
    wires: Vec<Vec<u8>>,
    drains: u64,
    closed: bool,
    action: u64,
    bytes: Vec<u8>,
}
struct FakeIo {
    log: Arc<Mutex<Log>>,
    sink: Option<FirmwareReceiveSink>,
    late_close: bool,
    partial: usize,
    fail_drain: bool,
    opaque: bool,
    stop: Option<Arc<OwnerStop>>,
}
impl FirmwareIo for FakeIo {
    fn attach(&mut self, sink: FirmwareReceiveSink) {
        self.sink = Some(sink);
    }
    fn write_once(&mut self, b: &[u8]) -> WriteStep {
        let n = b.len().min(self.partial);
        let mut l = self.log.lock().unwrap();
        l.wires.push(b[..n].to_vec());
        l.bytes.extend_from_slice(&b[..n]);
        if let Some(s) = &self.stop {
            s.requested.store(true, Ordering::SeqCst);
        }
        WriteStep::Returned(n)
    }
    fn drain_once(&mut self) -> DrainStep {
        let mut l = self.log.lock().unwrap();
        l.drains += 1;
        if self.fail_drain {
            return DrainStep::Failed(ServiceError::new(ErrorKind::Io, "synthetic drain failed"));
        }
        let frame = if self.opaque {
            vec![0xf0, 0x7e, 0x48, 4, 0x7f, 0xf7]
        } else {
            synthetic_reply(l.action)
        };
        l.action += 1;
        self.sink.as_ref().unwrap().receive(&frame[..3]);
        self.sink.as_ref().unwrap().receive(&frame[3..]);
        DrainStep::Returned
    }
    fn health(&mut self) -> Result<(), ServiceError> {
        Ok(())
    }
    fn ready_for(&self, sink: &FirmwareReceiveSink) -> bool {
        self.sink.as_ref().is_some_and(|s| s.same_source(sink))
    }
    fn close(&mut self) -> FirmwareReaderRetirement {
        let mut log = self.log.lock().unwrap();
        let already_closed = log.closed;
        log.closed = true;
        if self.late_close && !already_closed {
            self.sink
                .as_ref()
                .unwrap()
                .receive(&[0xf0, 0x7d, 0x66, 0xf7]);
        }
        let receipt = self.sink.as_ref().expect("attached sink").synthetic_close();
        if already_closed {
            receipt.repeat()
        } else {
            receipt
        }
    }
}
fn owner(
    t: &Temp,
    partial: usize,
    fail_drain: bool,
    opaque: bool,
    stop_on_write: bool,
) -> (FirmwareUpdateOwner, Arc<Mutex<Log>>) {
    let log = Arc::new(Mutex::new(Log::default()));
    let stop = Arc::new(OwnerStop::default());
    let io = FakeIo {
        log: log.clone(),
        sink: None,
        late_close: false,
        partial,
        fail_drain,
        opaque,
        stop: stop_on_write.then(|| stop.clone()),
    };
    let handoff = FirmwareNormalHandoff {
        binding: binding(),
        normal_epoch: SessionEpoch(10),
        normal_close: None,
        retirement: None,
        transition: None,
        factory: None,
        guard: None,
        mock: Some(Box::new(io)),
    };
    let req = FirmwareOwnershipRequest {
        #[cfg(feature = "firmware-entry-study")]
        entry_study: None,
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        linux_completion: None,
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        linux_monitor: Default::default(),
        job: job(),
        store: LibraryStore::open(t.0.join("library")).unwrap(),
        stop,
        mock: Some((
            crate::firmware_profile::MockFirmwareAuthority::synthetic(),
            t.0.join("registry"),
        )),
    };
    (FirmwareUpdateOwner::start(req, handoff).ok().unwrap(), log)
}
fn finish(o: &mut FirmwareUpdateOwner) {
    let end = Instant::now() + Duration::from_secs(30);
    while !o.stopped() {
        o.tick();
        assert!(Instant::now() < end, "owner deadline: {:?}", o.summary());
        std::thread::sleep(Duration::from_millis(1));
    }
}
#[test]
fn physical_profile_catalog_is_empty_and_public_job_is_only_a_description() {
    assert!(crate::firmware_profile::issue_physical().is_err());
    assert!(!job().permits_flash());
}
#[test]
fn full_real_ledger_mock_transfer_preserves_all_sections_and_wraps() {
    let t = Temp::new();
    let (mut o, l) = owner(&t, usize::MAX, false, false, false);
    let j = job();
    finish(&mut o);
    assert!(!o.summary().quarantined, "{:?}", o.summary());
    assert_eq!(o.summary().simulated_source_bytes, 272);
    assert_eq!(o.summary().device_verified_bytes, 0);
    assert!(!o.summary().completion_verified);
    let (summary, mut store) = o.retire().unwrap();
    assert!(l.lock().unwrap().closed);
    let ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
    let ids = ledger.attempt_ids().unwrap();
    assert_eq!(ids.len(), 1);
    let r = ledger.load(&ids[0]).unwrap();
    assert!(r.local_checkpoint_confirmed);
    assert!(r.issues.is_empty());
    assert!(r.lifecycle.as_ref().unwrap().stopped());
    assert!(!r.can_resume());
    assert!(!r.permits_flash());
    let wires = l.lock().unwrap().wires.clone();
    assert_eq!(wires.len(), j.budget().blocks as usize + 17);
    assert_eq!(wires[0], vec![0xf0, 0x7e, 0x48, 1, 0xf7]);
    let blocks = wires
        .iter()
        .filter(|p| p.get(3) == Some(&4))
        .map(|p| gp200_protocol::FirmwareBlock::decode(p, 1).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(blocks.len(), 272);
    assert_eq!(blocks[127].block_index_mod128, 127);
    assert_eq!(blocks[128].block_index_mod128, 0);
    assert_eq!(
        summary.phase,
        "Mock finished; physical update remains unverified"
    );
    let registry =
        crate::firmware_quarantine::QuarantineRegistry::mock(&t.0.join("registry")).unwrap();
    registry.admit_normal().unwrap();
}
#[test]
fn partial_write_stop_preserves_extent_and_never_continues_or_drains() {
    let t = Temp::new();
    let (mut o, l) = owner(&t, 2, false, false, true);
    finish(&mut o);
    assert!(o.summary().quarantined);
    let (_, mut store) = o.retire().unwrap();
    let log = l.lock().unwrap();
    assert_eq!(log.wires, vec![vec![0xf0, 0x7e]]);
    assert_eq!(log.drains, 0);
    drop(log);
    let ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
    let r = ledger.load(&ledger.attempt_ids().unwrap()[0]).unwrap();
    assert!(r.recent.iter().any(|s| s.label == "Lifecycle OS offer"));
}
#[test]
fn opaque_device_status_cannot_act_as_a_mock_ack() {
    let t = Temp::new();
    let (mut o, l) = owner(&t, usize::MAX, false, true, false);
    finish(&mut o);
    assert!(o.summary().quarantined);
    o.retire().unwrap();
    assert_eq!(l.lock().unwrap().wires.len(), 1);
    assert!(
        crate::firmware_quarantine::QuarantineRegistry::mock(&t.0.join("registry"))
            .unwrap()
            .admit_normal()
            .is_err()
    );
}
#[test]
fn failed_drain_stops_before_the_next_action() {
    let t = Temp::new();
    let (mut o, l) = owner(&t, usize::MAX, true, false, false);
    finish(&mut o);
    assert!(o.summary().quarantined);
    o.retire().unwrap();
    assert_eq!(l.lock().unwrap().wires.len(), 1);
}
#[test]
fn raw_allocator_preserves_fault_frontier_and_never_wraps() {
    let j = job();
    let p = FirmwareLifecyclePlanDto {
        review_sha256: j.digest().hex(),
        profile_sha256: JobSha256::of(b"rule").hex(),
        origin: FirmwareObservationOrigin::MockSimulation,
        nonblock_actions: 17,
        epoch: SessionEpoch(1),
        binding: binding(),
        generation: 1,
        clock_origin: "clock".into(),
    };
    let r = FirmwareReceiver::new(&p);
    for _ in 0..65 {
        r.sink.receive(&[1]);
    }
    assert_eq!(r.sink.allocated.load(Ordering::SeqCst), 65);
    assert!(r.sink.fault.load(Ordering::SeqCst));
    assert_eq!(r.received.len(), 64);
    r.sink.allocated.store(u64::MAX, Ordering::SeqCst);
    r.sink.receive(&[1]);
    assert_eq!(r.sink.allocated.load(Ordering::SeqCst), u64::MAX);
}
struct ServiceBackend {
    transition: Option<Box<dyn crate::firmware_transition::TransitionDriver>>,
    guard_root: Option<PathBuf>,
    late_close: bool,
    late_updater: bool,
    log: Arc<Mutex<Log>>,
    normal: Arc<std::sync::atomic::AtomicU64>,
}
struct NormalSession {
    transition: Option<Box<dyn crate::firmware_transition::TransitionDriver>>,
    guard: Option<crate::device_session_guard::DeviceSessionGuard>,
    late_close: bool,
    late_updater: bool,
    sink: ReceiveSink,
    log: Arc<Mutex<Log>>,
    normal: Arc<std::sync::atomic::AtomicU64>,
}
fn selection() -> DeviceSelection {
    DeviceSelection {
        input_id: "private fixture in".into(),
        output_id: "private fixture out".into(),
        fingerprint_hint: None,
    }
}
impl TransportBackend for ServiceBackend {
    fn enumerate(&mut self) -> Result<Vec<PortPairCandidate>, ServiceError> {
        Ok(vec![PortPairCandidate {
            selection: selection(),
            display_name: "private fixture".into(),
            mode_hint: Some(BackendMode::Hardware),
            matching_confidence: "test".into(),
        }])
    }
    fn connect(
        &mut self,
        _: &DeviceSelection,
        sink: ReceiveSink,
    ) -> Result<Box<dyn MidiSession>, ServiceError> {
        let guard = self
            .guard_root
            .as_ref()
            .map(|root| {
                crate::device_session_guard::DeviceSessionGuard::fixture(
                    root,
                    &[fixture_endpoint("test endpoint")],
                )
            })
            .transpose()?;
        Ok(Box::new(NormalSession {
            transition: self.transition.take(),
            guard,
            late_close: self.late_close,
            late_updater: self.late_updater,
            sink,
            log: self.log.clone(),
            normal: self.normal.clone(),
        }))
    }
}
impl MidiSession for NormalSession {
    fn send(&mut self, b: &[u8]) -> Result<(), ServiceError> {
        use gp200_protocol::*;
        self.normal.fetch_add(1, Ordering::SeqCst);
        let p = decode_packet(b).unwrap();
        let request = RawTransfer::from_payload(p.kind, p.payload).unwrap();
        let response = if request.records().iter().any(|r| r.command == 0x2001) {
            RawTransfer::from_records(
                0x12,
                &[RawRecord {
                    command: 0x2001,
                    body: vec![1, 0, 2, 0],
                }],
            )
            .unwrap()
        } else if request.records().iter().any(|r| r.command == 0x1006) {
            RawTransfer::from_payload(
                WRITE_KIND,
                include_bytes!("../../../tests/fixtures/globals/baseline.payload.bin").to_vec(),
            )
            .unwrap()
        } else {
            let mut result = None;
            let mut assembler = Reassembler::default();
            for event in StreamDecoder::new().feed(include_bytes!(
                "../../gp200-protocol/tests/fixtures/current-baseline.bin"
            )) {
                if let StreamEvent::Packet(p) = event
                    && let Some(raw) = assembler.add(&p, Duration::ZERO).unwrap()
                {
                    result = Some(raw);
                }
            }
            result.unwrap()
        };
        for p in encode_transfer(&response).unwrap() {
            self.sink.receive(&p);
        }
        Ok(())
    }
    fn take_firmware_handoff(&mut self) -> Result<FirmwareNormalHandoff, ServiceError> {
        if self.late_close {
            self.sink.receive(&[0xf8]);
        }
        Ok(FirmwareNormalHandoff {
            binding: binding(),
            normal_epoch: self.sink.epoch,
            normal_close: Some(crate::backend::NormalReaderCloseReceipt {
                source: self.sink.source.clone(),
                epoch: self.sink.epoch,
                started: self.sink.source.now_ns(),
                finished: self.sink.source.now_ns(),
                allocated: self.sink.source.fence(0).allocated,
                join: FirmwareNormalReaderJoinObservation::Returned,
                fault: None,
                overflow: self.sink.source.failed(),
                output_destroyed: true,
            }),
            retirement: None,
            transition: self.transition.take(),
            factory: None,
            guard: self.guard.take(),
            mock: Some(Box::new(FakeIo {
                log: self.log.clone(),
                sink: None,
                late_close: self.late_updater,
                partial: usize::MAX,
                fail_drain: false,
                opaque: false,
                stop: None,
            })),
        })
    }
}
#[test]
fn actual_midi_worker_quiesces_and_owns_the_genuine_mock_ledger() {
    let t = Temp::new();
    let log = Arc::new(Mutex::new(Log::default()));
    let normal = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let service = MidiDeviceService::with_backend(
        Box::new(ServiceBackend {
            transition: None,
            guard_root: None,
            late_close: false,
            late_updater: false,
            log: log.clone(),
            normal: normal.clone(),
        }),
        TransportConfig::default(),
        no_wake(),
    );
    service
        .try_submit(DeviceCommand::Connect {
            intent: IntentId(1),
            selection: selection(),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut ready = false;
    let mut epoch = SessionEpoch(0);
    while Instant::now() < deadline {
        if let Some(e) = service.try_next_event() {
            if let DeviceEvent::ConnectionChanged { epoch: actual, .. } = &e {
                epoch = *actual;
            }
            if matches!(
                e,
                DeviceEvent::ConnectionChanged {
                    state: ConnectionState::Ready,
                    ..
                }
            ) {
                ready = true;
            }
        } else if ready {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(ready);
    while service.try_next_event().is_some() {}
    service
        .try_submit(DeviceCommand::ReadGlobals(RequestStamp {
            epoch,
            intent: IntentId(2),
        }))
        .unwrap();
    let mut globals = false;
    let end = Instant::now() + Duration::from_secs(5);
    while !globals {
        if let Some(e) = service.try_next_event() {
            globals = matches!(e, DeviceEvent::GlobalStateReceived { .. });
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
    while service.try_next_event().is_some() {}
    let before = normal.load(Ordering::SeqCst);
    let request = FirmwareOwnershipRequest {
        #[cfg(feature = "firmware-entry-study")]
        entry_study: None,
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        linux_completion: None,
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        linux_monitor: Default::default(),
        job: job(),
        store: LibraryStore::open(t.0.join("library")).unwrap(),
        stop: Arc::new(OwnerStop::default()),
        mock: Some((
            crate::firmware_profile::MockFirmwareAuthority::synthetic(),
            t.0.join("registry"),
        )),
    };
    let ticket = service.try_start_firmware(request).ok().unwrap();
    assert!(matches!(
        service.try_submit(DeviceCommand::Discover),
        Err(SubmitError::Busy(_))
    ));
    let deadline = Instant::now() + Duration::from_secs(30);
    let final_event = loop {
        if let Some(e) = service.try_next_firmware_event() {
            break e;
        }
        assert!(
            Instant::now() < deadline,
            "{:?}",
            service.firmware_summary()
        );
        std::thread::sleep(Duration::from_millis(2));
    };
    let FirmwareOwnerEvent::Retired { summary, store } = final_event else {
        panic!("actual worker rejected handoff")
    };
    assert!(!summary.quarantined, "{summary:?}");
    assert_eq!(normal.load(Ordering::SeqCst), before);
    assert!(log.lock().unwrap().closed);
    assert!(LibraryStore::open(t.0.join("library")).is_err());
    drop(store);
    LibraryStore::open(t.0.join("library")).unwrap();
    drop(ticket);
    service.shutdown();
    assert!(service.wait_stopped(Duration::from_secs(1)));
}
#[test]
fn stop_before_first_offer_records_real_zero_offer_closure() {
    let t = Temp::new();
    let (mut o, l) = owner(&t, usize::MAX, false, false, false);
    o.request_user_stop_for_test();
    finish(&mut o);
    assert!(!o.summary().quarantined);
    assert_eq!(o.summary().phase, "Cancelled before any driver offer");
    o.retire().unwrap();
    assert!(l.lock().unwrap().wires.is_empty());
}
#[test]
fn driver_zero_count_ends_without_reoffer() {
    let t = Temp::new();
    let (mut o, l) = owner(&t, 0, false, false, false);
    finish(&mut o);
    assert!(o.summary().quarantined);
    o.retire().unwrap();
    assert_eq!(l.lock().unwrap().wires.len(), 1);
    assert_eq!(l.lock().unwrap().drains, 0);
}
#[test]
fn independently_checked_section_request_and_descriptor_schedule() {
    let j = job();
    let p = FirmwareLifecyclePlanDto {
        review_sha256: j.digest().hex(),
        profile_sha256: JobSha256::of(b"private synthetic FW03 absolute action correlation v1")
            .hex(),
        origin: FirmwareObservationOrigin::MockSimulation,
        nonblock_actions: 17,
        epoch: SessionEpoch(1),
        binding: binding(),
        generation: 1,
        clock_origin: "clock".into(),
    };
    let mut at = 2u64;
    // Native traversal visits raw FRMW slots [0, 2, 3, 1].
    for (i, n) in [260u32, 4, 5, 3].into_iter().enumerate() {
        let s = derive_action(&j, &p, at).unwrap();
        assert_eq!(s.wire, vec![0xf0, 0x7e, 0x48, 0x7f, [0, 1, 2, 16][i], 0xf7]);
        let d = derive_action(&j, &p, at + 1).unwrap();
        let mut expected = vec![0xf0, 0x7e, 0x48, 2];
        for b in n.to_be_bytes().into_iter().chain(n.to_be_bytes()) {
            expected.extend([b >> 4, b & 15]);
        }
        expected.push(0xf7);
        assert_eq!(d.wire, expected);
        let block = derive_action(&j, &p, at + 2).unwrap();
        let source = block.block.unwrap();
        assert_eq!(source.section, i as u8);
        assert_eq!(source.selector, [0, 1, 2, 16][i]);
        let raw_slot = [0, 2, 3, 1][i];
        assert_eq!(
            source.file_offset as usize,
            j.review().summary().sections[raw_slot].file_offset
        );
        assert_eq!(block.wire, j.block_wire(&source).unwrap());
        at += u64::from(n) + 3;
    }
    assert_eq!(
        derive_action(&j, &p, at).unwrap().wire,
        vec![0xf0, 0x7e, 0x48, 0x7f, 17, 0xf7]
    );
}
#[test]
fn full_normal_ui_rail_cannot_hide_a_bounded_handoff_rejection() {
    let t = Temp::new();
    let l = Arc::new(Mutex::new(Log::default()));
    let service = MidiDeviceService::with_backend(
        Box::new(ServiceBackend {
            transition: None,
            guard_root: None,
            late_close: false,
            late_updater: false,
            log: l.clone(),
            normal: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }),
        TransportConfig {
            event_capacity: 1,
            ..TransportConfig::default()
        },
        no_wake(),
    );
    service
        .try_submit(DeviceCommand::Connect {
            intent: IntentId(1),
            selection: selection(),
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let req = FirmwareOwnershipRequest {
        #[cfg(feature = "firmware-entry-study")]
        entry_study: None,
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        linux_completion: None,
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        linux_monitor: Default::default(),
        job: job(),
        store: LibraryStore::open(t.0.join("library")).unwrap(),
        stop: Arc::new(OwnerStop::default()),
        mock: Some((
            crate::firmware_profile::MockFirmwareAuthority::synthetic(),
            t.0.join("registry"),
        )),
    };
    let ticket = service.try_start_firmware(req).ok().unwrap();
    let end = Instant::now() + Duration::from_secs(2);
    let e = loop {
        if let Some(e) = service.try_next_firmware_event() {
            break e;
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(2));
    };
    let FirmwareOwnerEvent::Rejected { error, .. } = e else {
        panic!("handoff must reject")
    };
    assert_eq!(error.kind, ErrorKind::Busy);
    assert!(l.lock().unwrap().wires.is_empty());
    drop(ticket);
    service.shutdown();
    assert!(service.wait_stopped(Duration::from_secs(1)));
}
#[test]
fn cancellation_between_partial_steps_durably_records_the_known_extent() {
    let t = Temp::new();
    let (mut o, l) = owner(&t, 2, false, false, false);
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        o.tick();
        if o.stop_between_partial_for_test() {
            break;
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
    finish(&mut o);
    let (_, mut store) = o.retire().unwrap();
    assert_eq!(l.lock().unwrap().wires, vec![vec![0xf0, 0x7e]]);
    assert_eq!(l.lock().unwrap().drains, 0);
    let ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
    let r = ledger.load(&ledger.attempt_ids().unwrap()[0]).unwrap();
    assert!(r.recent.iter().any(|s| s.label == "Lifecycle OS offer"));
    assert!(r.local_checkpoint_confirmed);
}
#[test]
fn missing_first_clock_stamp_blocks_every_driver_offer() {
    let t = Temp::new();
    let (mut o, l) = owner(&t, usize::MAX, false, false, false);
    o.fail_clock_for_test();
    finish(&mut o);
    o.retire().unwrap();
    assert!(l.lock().unwrap().wires.is_empty());
}
#[test]
fn rejected_raw_frame_is_durably_retained_with_negative_interpretation() {
    let t = Temp::new();
    let (mut o, _) = owner(&t, usize::MAX, false, true, false);
    finish(&mut o);
    let (_, mut store) = o.retire().unwrap();
    let ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
    let r = ledger.load(&ledger.attempt_ids().unwrap()[0]).unwrap();
    assert!(r.local_checkpoint_confirmed);
    assert!(
        r.recent
            .iter()
            .any(|s| s.label == "Lifecycle rejected raw interpretation")
    );
}

#[test]
fn malformed_actual_chunks_and_shutdown_keep_exact_failure_bytes_and_extent() {
    for bytes in [
        vec![0xf0, 1, 0xf0, 2, 0xf7],
        vec![1, 2, 3],
        vec![0xf0, 0x80, 0xf7],
        {
            let mut b = vec![0xf0];
            b.extend(vec![1; 518]);
            b.push(0xf7);
            b
        },
    ] {
        let t = Temp::new();
        let (mut o, l) = owner(&t, usize::MAX, false, false, false);
        o.observe_raw_for_test(&bytes);
        finish(&mut o);
        let (summary, mut store) = o.retire().unwrap();
        assert!(summary.quarantined);
        assert!(summary.retained_raw_chunks.iter().any(|r| r.raw == bytes));
        assert!(l.lock().unwrap().wires.is_empty());
        let ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
        let attempt = ledger.attempt_ids().unwrap().remove(0);
        let record = ledger.load(&attempt).unwrap();
        assert!(
            record
                .recent
                .iter()
                .any(|s| s.label == "Lifecycle rejected raw interpretation")
        );
        let journal = std::fs::read(
            t.0.join("library/firmware-attempt-ledger")
                .join(format!("{}.journal", attempt.as_str())),
        )
        .unwrap();
        let serialized = serde_json::to_vec(&bytes).unwrap();
        assert!(journal.windows(serialized.len()).any(|w| w == serialized));
    }
    let t = Temp::new();
    let (mut o, l) = owner(&t, 2, false, false, false);
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        o.tick();
        if o.stop_between_partial_for_test() {
            break;
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
    o.request_host_stop();
    let (summary, mut store) = o.retire().unwrap();
    assert!(summary.quarantined);
    assert_eq!(l.lock().unwrap().wires.len(), 1);
    assert_eq!(l.lock().unwrap().drains, 0);
    assert!(l.lock().unwrap().closed);
    let ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
    let r = ledger.load(&ledger.attempt_ids().unwrap()[0]).unwrap();
    assert!(r.recent.iter().any(|s| s.label == "Lifecycle OS offer"));
}

fn fixture_endpoint(alias: &str) -> crate::device_session_guard::EndpointIdentity {
    crate::device_session_guard::EndpointIdentity {
        location: "synthetic USB 1-2".into(),
        card_id: alias.into(),
        device: 0,
        subdevice: 0,
    }
}
#[derive(Clone, Copy)]
enum TransitionFault {
    None,
    Duplicate,
    StaleIdentity,
    WrongModel,
    NoDisappearance,
    Coexist,
    Unready,
    Stop,
    LateCallback,
    NoRealClose,
    SourceReplacement,
    MissingIdentity,
    Current,
    Globals,
    Stored,
    Version,
    InvalidPlan,
    InvalidJournal,
    SpawnFailure,
}
struct TransitionFixture {
    policy: FirmwareLifecycleTransitionKind,
    fault: TransitionFault,
    log: Arc<Mutex<Log>>,
    opens: Arc<Mutex<Vec<String>>>,
    root: PathBuf,
    stop: Arc<OwnerStop>,
}
impl TransitionFixture {
    fn expected_role(
        &self,
        role: FirmwareLifecycleEpochRole,
    ) -> crate::firmware_transition::TransitionExpected {
        let mut usb = binding();
        let tag = if role == FirmwareLifecycleEpochRole::Bootloader {
            2
        } else {
            3
        };
        usb.interface = format!("private synthetic mode {tag}");
        if self.policy == FirmwareLifecycleTransitionKind::Reenumerated {
            usb.endpoint = format!("alias-mode-{tag}");
        }
        crate::firmware_transition::TransitionExpected {
            binding: usb,
            identity: vec![0xf0, 0x7d, 0x45, tag, 0xf7],
            negotiation: vec![0x45, tag],
        }
    }
}
impl crate::firmware_transition::TransitionDriver for TransitionFixture {
    fn profile(&self) -> crate::firmware_transition::TransitionProfile {
        crate::firmware_transition::TransitionProfile {
            sha256: JobSha256::of(b"private synthetic FW03 three-role transition v1"),
            nonblock_actions: if matches!(self.fault, TransitionFault::InvalidPlan) {
                64
            } else {
                17
            },
        }
    }
    fn ledger_stack_size(&self) -> Option<usize> {
        matches!(self.fault, TransitionFault::SpawnFailure).then_some(usize::MAX / 2)
    }
    fn initial_identity(&self) -> (Vec<u8>, Vec<u8>) {
        (vec![0xf0, 0x7d, 0x45, 0x01, 0xf7], vec![0x45, 0x01])
    }
    fn normal_mode(&self) -> BackendMode {
        BackendMode::Simulator
    }
    fn normal_config(&self) -> TransportConfig {
        TransportConfig {
            minimum_request_spacing: Duration::ZERO,
            ..TransportConfig::default()
        }
    }
    fn policy(&self) -> FirmwareLifecycleTransitionKind {
        self.policy
    }
    fn initial_enumeration(&self) -> u64 {
        1
    }
    fn expected(
        &self,
        role: FirmwareLifecycleEpochRole,
    ) -> crate::firmware_transition::TransitionExpected {
        self.expected_role(role)
    }
    fn endpoints(
        &self,
        candidate: &crate::firmware_transition::TransitionCandidate,
    ) -> Vec<crate::device_session_guard::EndpointIdentity> {
        vec![fixture_endpoint(&candidate.binding.endpoint)]
    }
    fn candidates(
        &mut self,
        role: FirmwareLifecycleEpochRole,
    ) -> Result<crate::firmware_transition::TransitionCandidates, ServiceError> {
        let post = role == FirmwareLifecycleEpochRole::PostUpdateNormal;
        let expected = self.expected_role(role);
        let make = || crate::firmware_transition::TransitionCandidate {
            binding: expected.binding.clone(),
            enumeration: if self.policy == FirmwareLifecycleTransitionKind::Reenumerated {
                if post { 3 } else { 2 }
            } else {
                1
            },
            epoch: SessionEpoch(if post { 4 } else { 3 }),
            generation: if post { 3 } else { 2 },
            source: format!("updater-fresh-{}", if post { 3 } else { 2 }),
            identity: expected.identity.clone(),
            negotiation: expected.negotiation.clone(),
            ready: true,
        };
        let mut candidate = make();
        if !post {
            match self.fault {
                TransitionFault::StaleIdentity => {
                    candidate.identity = vec![0xf0, 0x7d, 0x45, 1, 0xf7]
                }
                TransitionFault::WrongModel => candidate.binding.product = 999,
                TransitionFault::Unready => candidate.ready = false,
                TransitionFault::Stop => self.stop.requested.store(true, Ordering::SeqCst),
                _ => {}
            }
        }
        Ok(crate::firmware_transition::TransitionCandidates {
            candidates: if !post && matches!(self.fault, TransitionFault::Duplicate) {
                vec![candidate, make()]
            } else {
                vec![candidate]
            },
            old_disappeared: !matches!(self.fault, TransitionFault::NoDisappearance),
            coexist: matches!(self.fault, TransitionFault::Coexist),
        })
    }
    fn open_updater(
        &mut self,
        candidate: &crate::firmware_transition::TransitionCandidate,
        sink: FirmwareReceiveSink,
    ) -> Result<Box<dyn FirmwareIo>, ServiceError> {
        assert!(
            crate::device_session_guard::DeviceSessionGuard::fixture(
                &self.root,
                &[fixture_endpoint("test endpoint")]
            )
            .is_err()
        );
        assert!(
            crate::device_session_guard::DeviceSessionGuard::fixture(
                &self.root,
                &self.endpoints(candidate)
            )
            .is_err()
        );
        self.opens
            .lock()
            .unwrap()
            .push(format!("boot:{}", candidate.binding.endpoint));
        self.log.lock().unwrap().closed = false;
        let sink = if matches!(self.fault, TransitionFault::SourceReplacement) {
            let plan = FirmwareLifecyclePlanDto {
                review_sha256: job().digest().hex(),
                profile_sha256: JobSha256::of(b"replacement").hex(),
                origin: FirmwareObservationOrigin::MockSimulation,
                nonblock_actions: 17,
                epoch: candidate.epoch,
                binding: candidate.binding.clone(),
                generation: candidate.generation,
                clock_origin: "different source".into(),
            };
            sink.replacement(&plan, "unexplained replacement".into())
                .sink
        } else {
            sink
        };
        Ok(Box::new(FakeIo {
            log: self.log.clone(),
            sink: Some(sink),
            late_close: false,
            partial: usize::MAX,
            fail_drain: false,
            opaque: false,
            stop: None,
        }))
    }
    fn selection(&self) -> DeviceSelection {
        selection()
    }
    fn application_version(
        &self,
        original_identity: &[u8],
    ) -> Result<Option<String>, ServiceError> {
        let raw = gp200_protocol::RawTransfer::from_payload(0x12, original_identity.to_vec())
            .map_err(|e| ServiceError::invalid(e.to_string()))?;
        Ok(raw
            .records()
            .iter()
            .find(|r| r.command == 0x7d01)
            .and_then(|r| std::str::from_utf8(&r.body).ok())
            .map(str::to_owned))
    }
    fn open_normal(
        &mut self,
        candidate: &crate::firmware_transition::TransitionCandidate,
        sink: ReceiveSink,
    ) -> Result<Box<dyn MidiSession>, ServiceError> {
        assert!(
            crate::device_session_guard::DeviceSessionGuard::fixture(
                &self.root,
                &self.endpoints(candidate)
            )
            .is_err()
        );
        self.opens
            .lock()
            .unwrap()
            .push(format!("normal:{}", candidate.binding.endpoint));
        Ok(Box::new(PostNormalSession {
            sink,
            requests: 0,
            fault: self.fault,
        }))
    }
}
struct PostNormalSession {
    sink: ReceiveSink,
    requests: u8,
    fault: TransitionFault,
}
impl MidiSession for PostNormalSession {
    fn send(&mut self, bytes: &[u8]) -> Result<(), ServiceError> {
        use gp200_protocol::*;
        self.requests += 1;
        let fail = matches!(
            (self.fault, self.requests),
            (TransitionFault::MissingIdentity, 1)
                | (TransitionFault::Current, 2)
                | (TransitionFault::Globals, 3)
                | (TransitionFault::Stored, 4)
        );
        if fail {
            return Err(ServiceError::new(
                ErrorKind::Io,
                "Injected fresh normal read failure",
            ));
        }
        let packet = decode_packet(bytes).unwrap();
        let query = RawTransfer::from_payload(packet.kind, packet.payload).unwrap();
        let response = if query.records().iter().any(|r| r.command == 0x2001) {
            let mut records = vec![RawRecord {
                command: 0x2001,
                body: vec![1, 0, 2, 0],
            }];
            if !matches!(self.fault, TransitionFault::Version) {
                records.push(RawRecord {
                    command: 0x7d01,
                    body: b"synthetic-app-V182".to_vec(),
                });
            }
            RawTransfer::from_records(WRITE_KIND, &records).unwrap()
        } else if query.records().iter().any(|r| r.command == 0x1006) {
            RawTransfer::from_payload(
                WRITE_KIND,
                include_bytes!("../../../tests/fixtures/globals/baseline.payload.bin").to_vec(),
            )
            .unwrap()
        } else {
            let mut decoder = StreamDecoder::new();
            let mut assembler = Reassembler::default();
            let mut result = None;
            for event in decoder.feed(include_bytes!(
                "../../gp200-protocol/tests/fixtures/current-baseline.bin"
            )) {
                if let StreamEvent::Packet(p) = event
                    && let Some(raw) = assembler.add(&p, Duration::ZERO).unwrap()
                {
                    result = Some(raw);
                }
            }
            let original = result.unwrap();
            let mut records = original.records().to_vec();
            if let Some(context) = query.records().iter().find(|r| r.command == 0)
                && context.body[0] == 1
            {
                *records.iter_mut().find(|r| r.command == 0).unwrap() = context.clone();
            }
            RawTransfer::from_records(WRITE_KIND, &records).unwrap()
        };
        for packet in encode_transfer(&response).unwrap() {
            self.sink.receive(&packet);
        }
        Ok(())
    }
    fn close_normal_capture(&mut self) -> Result<(), ServiceError> {
        Ok(())
    }
}
fn service_ready(service: &MidiDeviceService) {
    service
        .try_submit(DeviceCommand::Connect {
            intent: IntentId(1),
            selection: selection(),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut epoch = SessionEpoch(0);
    let mut ready = false;
    while !ready {
        if let Some(DeviceEvent::ConnectionChanged {
            epoch: actual,
            state,
            ..
        }) = service.try_next_event()
        {
            epoch = actual;
            ready = state == ConnectionState::Ready;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    while service.try_next_event().is_some() {}
    service
        .try_submit(DeviceCommand::ReadGlobals(RequestStamp {
            epoch,
            intent: IntentId(2),
        }))
        .unwrap();
    loop {
        if let Some(DeviceEvent::GlobalStateReceived { .. }) = service.try_next_event() {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    while service.try_next_event().is_some() {}
}
fn run_transition(
    policy: FirmwareLifecycleTransitionKind,
    fault: TransitionFault,
    late_normal: bool,
) -> (
    FirmwareOwnerEvent,
    Vec<String>,
    PathBuf,
    Temp,
    Arc<Mutex<Log>>,
) {
    use std::os::unix::fs::PermissionsExt;
    let temp = Temp::new();
    let guard_root = temp.0.join("guards");
    std::fs::create_dir(&guard_root).unwrap();
    std::fs::set_permissions(&guard_root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let log = Arc::new(Mutex::new(Log::default()));
    let opens = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(OwnerStop::default());
    let service = MidiDeviceService::with_backend(
        Box::new(ServiceBackend {
            transition: Some(Box::new(TransitionFixture {
                policy,
                fault,
                log: log.clone(),
                opens: opens.clone(),
                root: guard_root.clone(),
                stop: stop.clone(),
            })),
            guard_root: Some(guard_root.clone()),
            late_close: late_normal,
            late_updater: matches!(
                fault,
                TransitionFault::LateCallback | TransitionFault::SpawnFailure
            ),
            log: log.clone(),
            normal: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }),
        TransportConfig::default(),
        no_wake(),
    );
    service_ready(&service);
    if matches!(fault, TransitionFault::NoRealClose) {
        log.lock().unwrap().closed = true;
    }
    let library_path = if matches!(fault, TransitionFault::InvalidJournal) {
        use std::os::unix::ffi::OsStringExt;
        temp.0
            .join(std::ffi::OsString::from_vec(b"library-\xff".to_vec()))
    } else {
        temp.0.join("library")
    };
    let mut store = LibraryStore::open(&library_path).unwrap();
    store.recovery_note = Some("original service startup store marker".into());
    let request = FirmwareOwnershipRequest {
        #[cfg(feature = "firmware-entry-study")]
        entry_study: None,
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        linux_completion: None,
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        linux_monitor: Default::default(),
        job: job(),
        store,
        stop,
        mock: Some((
            crate::firmware_profile::MockFirmwareAuthority::synthetic(),
            temp.0.join("registry"),
        )),
    };
    let ticket = service.try_start_firmware(request).ok().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let event = loop {
        if let Some(event) = service.try_next_firmware_event() {
            break event;
        }
        assert!(
            Instant::now() < deadline,
            "{:?}",
            service.firmware_summary()
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    if late_normal
        || matches!(
            fault,
            TransitionFault::InvalidPlan
                | TransitionFault::InvalidJournal
                | TransitionFault::SpawnFailure
        )
    {
        // Distinct post-retirement rejection cannot reuse the former epoch.
        // Deliberate fresh Connect goes through the normal actor and guard again.
        service_ready(&service);
    }
    drop(ticket);
    service.shutdown();
    assert!(service.wait_stopped(Duration::from_secs(2)));
    let open_log = opens.lock().unwrap().clone();
    (event, open_log, guard_root, temp, log)
}
#[test]
fn actual_worker_three_roles_reenumerated_and_same_alias_use_fresh_normal_parser_and_real_guards() {
    for policy in [
        FirmwareLifecycleTransitionKind::Reenumerated,
        FirmwareLifecycleTransitionKind::SameEnumerationProtocolChange,
    ] {
        let (event, opens, root, temp, log) = run_transition(policy, TransitionFault::None, false);
        let FirmwareOwnerEvent::Retired { summary, mut store } = event else {
            panic!("three-role owner must retire");
        };
        assert!(!summary.quarantined, "{summary:?}");
        assert_eq!(summary.device_verified_bytes, 0);
        assert!(!summary.completion_verified);
        assert_eq!(opens.len(), 2);
        assert_eq!(
            log.lock().unwrap().wires[0],
            vec![0xf0, 0x7d, 0x45, 1, 0xf7]
        );
        let ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
        let record = ledger.load(&ledger.attempt_ids().unwrap()[0]).unwrap();
        assert!(record.issues.is_empty(), "{:?}", record.issues);
        let lifecycle = record.lifecycle.unwrap();
        assert_eq!(lifecycle.epochs().len(), 3);
        assert_eq!(
            lifecycle.epochs()[0].clock_origin,
            lifecycle.epochs()[1].clock_origin
        );
        let refresh = lifecycle.epochs()[2].post_normal_refresh.as_ref().unwrap();
        assert_eq!(refresh.epoch, SessionEpoch(4));
        assert_ne!(refresh.clock_origin, lifecycle.epochs()[1].clock_origin);
        assert_eq!(
            refresh.application_version.as_deref(),
            Some("synthetic-app-V182")
        );
        assert_eq!(refresh.stored_slot, 0);
        assert!(
            refresh.current_revision > 0
                && refresh.globals_revision > 0
                && refresh.stored_revision > 0
        );
        assert!(
            crate::device_session_guard::DeviceSessionGuard::fixture(
                &root,
                &[fixture_endpoint("test endpoint")]
            )
            .is_ok()
        );
        assert!(
            crate::firmware_quarantine::QuarantineRegistry::mock(&temp.0.join("registry"))
                .unwrap()
                .admit_normal()
                .is_ok()
        );
    }
}
#[test]
fn actual_worker_transition_negatives_stop_before_new_offers_and_never_clear_quarantine() {
    for fault in [
        TransitionFault::Duplicate,
        TransitionFault::StaleIdentity,
        TransitionFault::WrongModel,
        TransitionFault::NoDisappearance,
        TransitionFault::Coexist,
        TransitionFault::Unready,
        TransitionFault::Stop,
        TransitionFault::LateCallback,
        TransitionFault::NoRealClose,
    ] {
        let (event, opens, _, _, log) =
            run_transition(FirmwareLifecycleTransitionKind::Reenumerated, fault, false);
        let FirmwareOwnerEvent::Retired { summary, .. } = event else {
            panic!("negative owner must retire");
        };
        assert!(summary.quarantined, "{summary:?}");
        assert!(opens.is_empty());
        assert_eq!(log.lock().unwrap().wires.len(), 1);
    }
}
#[test]
fn actual_worker_fresh_normal_reads_and_missing_application_version_each_block_mock_finish() {
    for fault in [
        TransitionFault::MissingIdentity,
        TransitionFault::Current,
        TransitionFault::Globals,
        TransitionFault::Stored,
        TransitionFault::Version,
    ] {
        let (event, opens, _, _, _) = run_transition(
            FirmwareLifecycleTransitionKind::SameEnumerationProtocolChange,
            fault,
            false,
        );
        let FirmwareOwnerEvent::Retired { summary, .. } = event else {
            panic!("read failure owner must retire");
        };
        assert!(summary.quarantined, "{summary:?}");
        assert_eq!(opens.len(), 2);
    }
}
#[test]
fn actual_normal_late_callback_returns_original_store_as_retired_rejection_with_original_bytes() {
    let (event, opens, _, temp, log) = run_transition(
        FirmwareLifecycleTransitionKind::Reenumerated,
        TransitionFault::None,
        true,
    );
    let FirmwareOwnerEvent::RejectedAfterNormalRetirement {
        error,
        mut store,
        observation,
    } = event
    else {
        panic!("late callback must reject after retirement");
    };
    assert_eq!(error.kind, ErrorKind::Unverified);
    assert_eq!(
        observation.allocated_after,
        observation.allocated_before + 1
    );
    assert_eq!(observation.retained_raw.len(), 1);
    assert_eq!(observation.retained_raw[0].raw, vec![0xf8]);
    observation.retained_json().unwrap();
    assert!(opens.is_empty());
    assert!(log.lock().unwrap().wires.is_empty());
    assert!(
        FirmwareAttemptLedger::open(&mut store)
            .unwrap()
            .attempt_ids()
            .unwrap()
            .is_empty()
    );
    assert!(!temp.0.join("registry").exists());
}

#[test]
fn actual_worker_unexplained_source_replacement_cannot_offer_after_fresh_open() {
    let (event, opens, _, _, log) = run_transition(
        FirmwareLifecycleTransitionKind::SameEnumerationProtocolChange,
        TransitionFault::SourceReplacement,
        false,
    );
    let FirmwareOwnerEvent::Retired { summary, .. } = event else {
        panic!("replacement owner must retire");
    };
    assert!(summary.quarantined);
    assert_eq!(opens.len(), 1);
    assert_eq!(log.lock().unwrap().wires.len(), 1);
}
#[test]
fn private_continuation_rejects_each_changed_owner_source_review_and_journal_anchor() {
    use crate::firmware_transition::*;
    for change in 0..10 {
        let j = job();
        let plan = FirmwareLifecyclePlanDto {
            review_sha256: j.digest().hex(),
            profile_sha256: JobSha256::of(b"private synthetic FW03 three-role transition v1").hex(),
            origin: FirmwareObservationOrigin::MockSimulation,
            nonblock_actions: 17,
            epoch: SessionEpoch(2),
            binding: binding(),
            generation: 1,
            clock_origin: "test owner clock".into(),
        };
        let receiver = FirmwareReceiver::new(&plan);
        let previous = FirmwareLifecycleEpochObservation {
            role: FirmwareLifecycleEpochRole::NormalEntry,
            epoch: plan.epoch,
            generation: plan.generation,
            binding: plan.binding.clone(),
            previous_binding_sha256: None,
            previous_epoch: None,
            source_instance: receiver.sink.source().into(),
            clock_origin: plan.clock_origin.clone(),
            retired_source: None,
            raw_identity: vec![1],
            negotiation: vec![1],
            transition: None,
            post_normal_refresh: None,
        };
        let correlation = || TransitionCorrelation {
            run: "actual private run".into(),
            review: j.digest().hex(),
            profile: plan.profile_sha256.clone(),
            image: j.review().summary().source_sha256.hex(),
            sequence: 3,
            epoch: plan.epoch,
            generation: plan.generation,
            source: receiver.sink.source().into(),
            binding: plan.binding.digest(),
        };
        let mut retirement = receiver.sink.synthetic_close();
        assert!(retirement.accounts(&receiver.sink, 0));
        let expected = TransitionExpected {
            binding: binding(),
            identity: vec![2],
            negotiation: vec![2],
        };
        let candidate = TransitionCandidate {
            binding: binding(),
            enumeration: 1,
            epoch: SessionEpoch(3),
            generation: 2,
            source: "fresh source".into(),
            identity: vec![2],
            negotiation: vec![2],
            ready: true,
        };
        let continuation = prepare_transition(TransitionRequest {
            previous: &previous,
            previous_enumeration: 1,
            role: FirmwareLifecycleEpochRole::Bootloader,
            retirement,
            processed: 0,
            last_ns: 0,
            policy: TransitionPolicy::SameEnumerationProtocolChange,
            candidates: TransitionCandidates {
                candidates: vec![candidate],
                old_disappeared: false,
                coexist: false,
            },
            expected,
            correlation: correlation(),
        })
        .unwrap();
        let mut changed = correlation();
        match change {
            0 => changed.run.push('x'),
            1 => changed.review = JobSha256::of(b"different review").hex(),
            2 => changed.profile = JobSha256::of(b"different profile").hex(),
            3 => changed.image = JobSha256::of(b"different image").hex(),
            4 => changed.sequence += 1,
            5 => changed.epoch = SessionEpoch(9),
            6 => changed.generation += 1,
            7 => changed.source.push('x'),
            8 => changed.binding = JobSha256::of(b"different physical association").hex(),
            _ => {}
        }
        assert!(continuation.consume(&changed, change == 9).is_err());
    }
}

#[test]
fn actual_startup_plan_path_and_os_spawn_failures_return_original_store_and_require_fresh_connect()
{
    for fault in [
        TransitionFault::InvalidPlan,
        TransitionFault::InvalidJournal,
        TransitionFault::SpawnFailure,
    ] {
        let (event, opens, root, temp, log) =
            run_transition(FirmwareLifecycleTransitionKind::Reenumerated, fault, false);
        let FirmwareOwnerEvent::StartupFailedWithStore {
            error,
            store,
            normal_retirement,
            updater_retirement,
        } = event
        else {
            panic!("startup failure must return the genuinely retained store");
        };
        assert!(opens.is_empty());
        assert!(log.lock().unwrap().wires.is_empty());
        assert_eq!(
            store.recovery_note.as_deref(),
            Some("original service startup store marker")
        );
        assert!(matches!(LibraryStore::open(store.root()), Err(e) if e.kind == ErrorKind::Busy));
        assert!(!store.root().join("firmware-attempt-ledger").exists());
        assert_eq!(
            normal_retirement.unwrap().reader_join,
            FirmwareNormalReaderJoinObservation::Returned
        );
        if matches!(fault, TransitionFault::SpawnFailure) {
            assert_eq!(error.kind, ErrorKind::Io);
            let closed = updater_retirement.unwrap();
            assert_eq!(
                closed.source.reader_join,
                FirmwareNormalReaderJoinObservation::Returned
            );
            assert!(closed.output_handle_destroyed);
            assert_eq!(closed.source.allocated, 1);
            assert_eq!(closed.source.processed, 0);
            assert_eq!(closed.retained_raw_chunks.len(), 1);
            assert_eq!(
                closed.retained_raw_chunks[0].raw,
                vec![0xf0, 0x7d, 0x66, 0xf7]
            );
            assert_eq!(closed.retained_raw_chunks[0].epoch, closed.source.epoch);
            assert_eq!(
                closed.retained_raw_chunks[0].clock_origin,
                closed.source.clock_origin
            );
            assert_eq!(closed.retained_raw_chunks[0].sequence, 1);
            assert!(log.lock().unwrap().closed);
        } else {
            assert_eq!(error.kind, ErrorKind::InvalidData);
            assert!(updater_retirement.is_none());
        }
        assert!(
            crate::device_session_guard::DeviceSessionGuard::fixture(
                &root,
                &[fixture_endpoint("test endpoint")]
            )
            .is_ok()
        );
        let returned_path = store.root().to_path_buf();
        drop(store);
        LibraryStore::open(returned_path).unwrap();
        drop(temp);
    }
}
#[test]
fn actual_uninhabited_physical_opener_error_returns_store_before_any_io() {
    let t = Temp::new();
    let mut store = LibraryStore::open(t.0.join("library")).unwrap();
    store.recovery_note = Some("physical opener rejection original store".into());
    let request = FirmwareOwnershipRequest {
        #[cfg(feature = "firmware-entry-study")]
        entry_study: None,
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        linux_completion: None,
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        linux_monitor: Default::default(),
        job: job(),
        store,
        stop: Arc::new(OwnerStop::default()),
        mock: None,
    };
    let handoff = FirmwareNormalHandoff {
        binding: binding(),
        normal_epoch: SessionEpoch(1),
        normal_close: None,
        retirement: None,
        transition: None,
        factory: None,
        guard: None,
        mock: None,
    };
    let failed = match FirmwareUpdateOwner::start(request, handoff) {
        Err(failed) => failed,
        Ok(_) => panic!("physical issuer remains unavailable"),
    };
    assert_eq!(failed.error.kind, ErrorKind::Unverified);
    assert!(failed.updater_retirement.is_none());
    assert_eq!(
        failed.store.recovery_note.as_deref(),
        Some("physical opener rejection original store")
    );
    assert!(matches!(LibraryStore::open(failed.store.root()), Err(e) if e.kind == ErrorKind::Busy));
    assert!(!failed.store.root().join("firmware-attempt-ledger").exists());
}

#[test]
fn firmware_admission_held_gates_return_original_commands_and_store_without_waiting() {
    let t = Temp::new();
    let service = MidiDeviceService::with_backend(
        Box::new(ServiceBackend {
            transition: None,
            guard_root: None,
            late_close: false,
            late_updater: false,
            log: Arc::new(Mutex::new(Log::default())),
            normal: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }),
        TransportConfig::default(),
        no_wake(),
    );
    let crate::service::FirmwareAdmissionTestGates {
        submission,
        summary,
        active,
    } = service.test_firmware_admission_gates();
    let held = submission.lock().unwrap();
    let began = Instant::now();
    let stamp = RequestStamp {
        epoch: SessionEpoch(42),
        intent: IntentId(73),
    };
    assert!(
        matches!(service.try_submit(DeviceCommand::ReadGlobals(stamp)), Err(SubmitError::Busy(DeviceCommand::ReadGlobals(original))) if original == stamp)
    );
    let request =
        FirmwareOwnershipRequest::new(job(), LibraryStore::open(t.0.join("library")).unwrap());
    let request = match service.try_start_firmware(request) {
        Err(SubmitError::Busy(original)) => original,
        _ => panic!("Original owning request must remain unoffered"),
    };
    assert!(began.elapsed() < Duration::from_millis(100));
    assert!(!active.load(Ordering::Acquire));
    assert!(LibraryStore::open(t.0.join("library")).is_err());
    drop(held);
    let held = summary.lock().unwrap();
    let began = Instant::now();
    let request = match service.try_start_firmware(request) {
        Err(SubmitError::Busy(original)) => original,
        _ => panic!("Held summary must preserve the owning request"),
    };
    assert!(began.elapsed() < Duration::from_millis(100));
    assert!(!active.load(Ordering::Acquire));
    drop(held);
    let store = request.into_store();
    assert!(LibraryStore::open(t.0.join("library")).is_err());
    drop(store);
    LibraryStore::open(t.0.join("library")).unwrap();
    *summary.lock().unwrap() = Some(FirmwareOwnerSummary {
        phase: "old descriptive run".into(),
        simulated_source_bytes: 42,
        os_offered_bytes: 5,
        device_verified_bytes: 0,
        completion_verified: false,
        quarantined: true,
        diagnostic: Some("old fixture".into()),
        retained_raw_chunks: vec![],
        allocated_raw_frontier: 0,
    });
    let held = summary.lock().unwrap();
    let request =
        FirmwareOwnershipRequest::new(job(), LibraryStore::open(t.0.join("next-library")).unwrap());
    let original = match service.try_start_firmware(request) {
        Err(SubmitError::Busy(original)) => original,
        _ => panic!("Held old summary must keep the original request"),
    };
    assert_eq!(held.as_ref().unwrap().phase, "old descriptive run");
    drop(held);
    assert_eq!(
        service.firmware_summary().unwrap().phase,
        "old descriptive run"
    );
    let ticket = service.try_start_firmware(original).ok().unwrap();
    assert!(service.firmware_summary().is_none());
    let end = Instant::now() + Duration::from_secs(5);
    let returned = loop {
        if let Some(event) = service.try_next_firmware_event() {
            break event;
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(1));
    };
    let FirmwareOwnerEvent::Rejected { store, .. } = returned else {
        panic!("Unavailable physical issuer must reject before retirement");
    };
    assert!(LibraryStore::open(t.0.join("next-library")).is_err());
    drop(store);
    drop(ticket);
    service.shutdown();
    let held = submission.lock().unwrap();
    assert!(
        matches!(service.try_submit(DeviceCommand::ReadGlobals(stamp)), Err(SubmitError::Disconnected(DeviceCommand::ReadGlobals(original))) if original == stamp)
    );
    drop(held);
    assert!(service.wait_stopped(Duration::from_secs(1)));
}

// Entry tests use the actual service/owning store/journal/registry, and an
// actual raw-reader thread over fake input. No synthetic reply is generated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryFault {
    None,
    Partial,
    Zero,
    Unknown,
    WouldBlock,
    Interrupted,
    Impossible,
    DrainFailed,
    DrainWouldBlock,
    StopBetween,
    ReaderFailure,
    Overflow,
    RegistryTail,
    LedgerMissing,
    UnreadyBinding,
    LateClose,
    RebindAmbiguous,
}
struct EntryInput;
impl crate::raw_backend::RawMidiInput for EntryInput {
    fn read(&mut self, _: &mut [u8]) -> Result<usize, crate::raw_backend::InputReadError> {
        Err(crate::raw_backend::InputReadError::WouldBlock)
    }
    fn check_status(&mut self) -> Result<(), ServiceError> {
        Ok(())
    }
}
#[derive(Default)]
struct EntryLog {
    wires: Vec<Vec<u8>>,
    durable: Vec<(u64, String)>,
    drains: u64,
    closed: bool,
    joins: u64,
}
struct EntryIo {
    root: PathBuf,
    fault: EntryFault,
    log: Arc<Mutex<EntryLog>>,
    stop: Arc<OwnerStop>,
    sink: Option<FirmwareReceiveSink>,
    reader: Option<crate::firmware_raw::FirmwareRawReader>,
    injected: bool,
}
impl FirmwareIo for EntryIo {
    fn attach(&mut self, sink: FirmwareReceiveSink) {
        self.reader = Some(
            crate::firmware_raw::FirmwareRawReader::start(
                EntryInput,
                sink.clone(),
                Duration::from_secs(1),
            )
            .unwrap(),
        );
        self.sink = Some(sink);
    }
    fn write_once(&mut self, wire: &[u8]) -> WriteStep {
        let ordinal = self.log.lock().unwrap().wires.len() as u64;
        assert!(
            crate::device_session_guard::DeviceSessionGuard::fixture(
                &self.root.join("guards"),
                &[fixture_endpoint("entry-card")]
            )
            .is_err()
        );
        assert!(
            crate::firmware_quarantine::QuarantineRegistry::mock(&self.root.join("physical"))
                .unwrap()
                .physical_lease(&entry_binding())
                .is_err()
        );
        let registry = std::fs::read_dir(self.root.join("registry"))
            .unwrap()
            .filter_map(Result::ok)
            .find(|e| e.path().extension().is_some_and(|e| e == "quarantine"))
            .unwrap();
        let bytes = std::fs::read(registry.path()).unwrap();
        let retained = String::from_utf8_lossy(&bytes);
        assert!(retained.contains(&format!("ordinal={ordinal}\n")));
        let digest = JobSha256::of(wire).hex();
        assert!(retained.contains(&format!("wire={digest}\n")));
        let checkpoint = std::fs::read_dir(self.root.join("library/firmware-attempt-ledger"))
            .unwrap()
            .filter_map(Result::ok)
            .find(|e| e.path().extension().is_some_and(|e| e == "checkpoint"))
            .unwrap();
        let checkpoint: serde_json::Value =
            serde_json::from_slice(&std::fs::read(checkpoint.path()).unwrap()).unwrap();
        assert!(checkpoint["receipt"]["records"].as_u64().unwrap() >= 3);
        let mut log = self.log.lock().unwrap();
        log.wires.push(wire.to_vec());
        log.durable.push((ordinal, digest));
        match self.fault {
            EntryFault::Partial => WriteStep::Returned(3),
            EntryFault::Zero => WriteStep::Returned(0),
            EntryFault::Unknown => {
                WriteStep::Unknown(ServiceError::new(ErrorKind::Io, "fake OS unknown extent"))
            }
            EntryFault::WouldBlock => WriteStep::WouldBlock,
            EntryFault::Interrupted => WriteStep::Interrupted,
            EntryFault::Impossible => WriteStep::Returned(wire.len() + 1),
            _ => WriteStep::Returned(wire.len()),
        }
    }
    fn drain_once(&mut self) -> DrainStep {
        self.log.lock().unwrap().drains += 1;
        if self.fault == EntryFault::StopBetween {
            self.stop.requested.store(true, Ordering::SeqCst);
        }
        if self.fault == EntryFault::ReaderFailure {
            self.sink.as_ref().unwrap().reader_fault();
        }
        if self.fault == EntryFault::Overflow {
            for _ in 0..65 {
                self.sink.as_ref().unwrap().receive(&[0x55]);
            }
        }
        match self.fault {
            EntryFault::DrainFailed => {
                DrainStep::Failed(ServiceError::new(ErrorKind::Io, "fake drain failed"))
            }
            EntryFault::DrainWouldBlock => DrainStep::WouldBlock,
            // Malformed/fragmented raw bytes remain passive originals, not ACKs.
            EntryFault::None => {
                self.sink.as_ref().unwrap().receive(&[0xff, 0, 0xf0]);
                DrainStep::Returned
            }
            _ => DrainStep::Returned,
        }
    }
    fn health(&mut self) -> Result<(), ServiceError> {
        if self.fault == EntryFault::RebindAmbiguous && !self.log.lock().unwrap().wires.is_empty() {
            return Err(ServiceError::new(
                ErrorKind::Disconnected,
                "fake normal endpoint disappeared with ambiguous new aliases",
            ));
        }
        if !self.injected
            && self.root.join("registry").exists()
            && let Some(record) = std::fs::read_dir(self.root.join("registry"))
                .unwrap()
                .filter_map(Result::ok)
                .find(|e| e.path().extension().is_some_and(|e| e == "quarantine"))
        {
            if self.fault == EntryFault::RegistryTail {
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(record.path())
                    .unwrap()
                    .write_all(&[0x77])
                    .unwrap();
                self.injected = true;
            } else if self.fault == EntryFault::LedgerMissing {
                let journal = std::fs::read_dir(self.root.join("library/firmware-attempt-ledger"))
                    .unwrap()
                    .filter_map(Result::ok)
                    .find(|e| e.path().extension().is_some_and(|e| e == "journal"))
                    .unwrap();
                std::fs::rename(journal.path(), journal.path().with_extension("retained")).unwrap();
                self.injected = true;
            }
        }
        Ok(())
    }
    fn ready_for(&self, sink: &FirmwareReceiveSink) -> bool {
        self.fault != EntryFault::UnreadyBinding
            && self.sink.as_ref().is_some_and(|s| s.same_source(sink))
    }
    fn close(&mut self) -> FirmwareReaderRetirement {
        let mut log = self.log.lock().unwrap();
        if self.fault == EntryFault::LateClose && !log.closed {
            self.sink.as_ref().unwrap().receive(&[0xaa, 0xbb]);
        }
        let was_closed = log.closed;
        log.closed = true;
        let mut retired = self.reader.as_mut().unwrap().close();
        assert_eq!(
            retired.observation().reader_join,
            FirmwareNormalReaderJoinObservation::Returned
        );
        if !was_closed {
            log.joins += 1;
        }
        // Fake output is destroyed. The reader receipt comes from actual join.
        retired.output_destroyed = true;
        retired
    }
}
struct EntryBackend {
    inner: ServiceBackend,
    root: PathBuf,
    fault: EntryFault,
    log: Arc<Mutex<EntryLog>>,
    stop: Arc<OwnerStop>,
}
struct EntryNormalSession {
    inner: Box<dyn MidiSession>,
    binding: FirmwareUsbObservation,
    guard: Option<crate::device_session_guard::DeviceSessionGuard>,
    io: Option<EntryIo>,
}
fn entry_binding() -> FirmwareUsbObservation {
    FirmwareUsbObservation {
        endpoint: "hw:entry-card,0,0".into(),
        interface: "synthetic USB 1-2".into(),
        ..binding()
    }
}
impl TransportBackend for EntryBackend {
    fn enumerate(&mut self) -> Result<Vec<PortPairCandidate>, ServiceError> {
        self.inner.enumerate()
    }
    fn connect(
        &mut self,
        selection: &DeviceSelection,
        sink: ReceiveSink,
    ) -> Result<Box<dyn MidiSession>, ServiceError> {
        let inner = self.inner.connect(selection, sink)?;
        let binding = entry_binding();
        let registry =
            crate::firmware_quarantine::QuarantineRegistry::mock(&self.root.join("physical"))?;
        let guard = crate::device_session_guard::DeviceSessionGuard::fixture_physical(
            &self.root.join("guards"),
            &[fixture_endpoint("entry-card")],
            &binding,
            &registry,
        )?;
        Ok(Box::new(EntryNormalSession {
            inner,
            binding,
            guard: Some(guard),
            io: Some(EntryIo {
                root: self.root.clone(),
                fault: self.fault,
                log: self.log.clone(),
                stop: self.stop.clone(),
                sink: None,
                reader: None,
                injected: false,
            }),
        }))
    }
}
impl MidiSession for EntryNormalSession {
    fn send(&mut self, bytes: &[u8]) -> Result<(), ServiceError> {
        self.inner.send(bytes)
    }
    fn health(&mut self) -> Result<(), ServiceError> {
        self.inner.health()
    }
    fn take_firmware_handoff(&mut self) -> Result<FirmwareNormalHandoff, ServiceError> {
        let mut handoff = self.inner.take_firmware_handoff()?;
        handoff.binding = self.binding.clone();
        handoff.guard = self.guard.take();
        handoff.mock = self.io.take().map(|io| Box::new(io) as Box<dyn FirmwareIo>);
        Ok(handoff)
    }
}
fn run_entry(
    fault: EntryFault,
) -> (
    FirmwareOwnerSummary,
    LibraryStore,
    Temp,
    Arc<Mutex<EntryLog>>,
) {
    use std::os::unix::fs::PermissionsExt;
    let temp = Temp::new();
    let guards = temp.0.join("guards");
    std::fs::create_dir(&guards).unwrap();
    std::fs::set_permissions(&guards, std::fs::Permissions::from_mode(0o700)).unwrap();
    let log = Arc::new(Mutex::new(EntryLog::default()));
    let stop = Arc::new(OwnerStop::default());
    let normal = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let service = MidiDeviceService::with_backend(
        Box::new(EntryBackend {
            inner: ServiceBackend {
                transition: None,
                guard_root: None,
                late_close: false,
                late_updater: false,
                log: Arc::new(Mutex::new(Log::default())),
                normal: normal.clone(),
            },
            root: temp.0.clone(),
            fault,
            log: log.clone(),
            stop: stop.clone(),
        }),
        TransportConfig::default(),
        no_wake(),
    );
    service_ready(&service);
    let normal_before = normal.load(Ordering::SeqCst);
    let mut store = LibraryStore::open(temp.0.join("library")).unwrap();
    store.recovery_note = Some("original entry store".into());
    let ticket = service
        .try_start_firmware(FirmwareOwnershipRequest {
            #[cfg(feature = "firmware-entry-study")]
            entry_study: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            linux_completion: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            linux_monitor: Default::default(),
            job: job(),
            store,
            stop,
            mock: Some((
                crate::firmware_profile::MockFirmwareAuthority::entry_observation(),
                temp.0.join("registry"),
            )),
        })
        .ok()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let event = loop {
        if let Some(event) = service.try_next_firmware_event() {
            break event;
        }
        assert!(
            Instant::now() < deadline,
            "{fault:?}: {:?}",
            service.firmware_summary()
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    let FirmwareOwnerEvent::Retired { summary, store } = event else {
        panic!("entry actual owner did not retire: {fault:?}");
    };
    assert_eq!(store.recovery_note.as_deref(), Some("original entry store"));
    assert_eq!(normal.load(Ordering::SeqCst), normal_before);
    assert!(matches!(LibraryStore::open(store.root()), Err(e) if e.kind == ErrorKind::Busy));
    drop(ticket);
    service.shutdown();
    assert!(service.wait_stopped(Duration::from_secs(1)));
    (summary, store, temp, log)
}
#[test]
fn entry_actual_worker_two_durable_candidates_no_ack_no_transfer_and_persistent_quarantine() {
    let (summary, mut store, temp, log) = run_entry(EntryFault::None);
    assert!(summary.quarantined);
    assert_eq!(summary.os_offered_bytes, 36);
    assert_eq!(summary.simulated_source_bytes, 0);
    assert_eq!(summary.device_verified_bytes, 0);
    assert!(!summary.completion_verified);
    let log = log.lock().unwrap();
    let expected: Vec<Vec<u8>> = [
        "f021257e47502d3212040000000000020000000000f7",
        "f021257e47502d321212002000f7",
    ]
    .iter()
    .map(|hex| {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    })
    .collect();
    assert_eq!(log.wires, expected);
    assert_eq!(log.durable.iter().map(|v| v.0).collect::<Vec<_>>(), [0, 1]);
    assert_eq!(log.drains, 2);
    assert!(log.closed);
    assert_eq!(log.joins, 1);
    let ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
    let id = ledger.attempt_ids().unwrap().pop().unwrap();
    let retained = ledger.load(&id).unwrap();
    assert!(retained.local_checkpoint_confirmed);
    assert!(retained.issues.is_empty(), "{:?}", retained.issues);
    let projection = retained.lifecycle.as_ref().unwrap();
    assert_eq!(projection.budget().actions, 2);
    assert_eq!(projection.next_action(), 2);
    assert_eq!(
        projection.closure(),
        Some(FirmwareLifecycleHostClosure::HostStoppedUnresolved)
    );
    assert!(projection.entry_observation().is_some());
    println!(
        "FW06_ENTRY_OBSERVED {}",
        serde_json::json!({
            "offered_wires": log.wires, "durable_ordinals_and_wire_digests": log.durable,
            "drains": log.drains, "actual_reader_joins": log.joins,
            "journal_accepted_records": retained.accepted_records,
            "journal_checkpoint_confirmed": retained.local_checkpoint_confirmed,
            "quarantined": summary.quarantined, "device_verified_bytes": summary.device_verified_bytes,
            "completion_verified": summary.completion_verified,
            "physical_packets": 0,
        })
    );
    assert!(!retained.permits_flash());
    assert!(!retained.can_resume());
    assert!(
        crate::firmware_quarantine::QuarantineRegistry::mock(&temp.0.join("registry"))
            .unwrap()
            .admit_normal()
            .is_err()
    );
}
#[test]
fn entry_actual_owner_each_uncertain_write_drain_stop_reader_and_binding_prevents_second() {
    for fault in [
        EntryFault::Partial,
        EntryFault::Zero,
        EntryFault::Unknown,
        EntryFault::WouldBlock,
        EntryFault::Interrupted,
        EntryFault::Impossible,
        EntryFault::DrainFailed,
        EntryFault::DrainWouldBlock,
        EntryFault::StopBetween,
        EntryFault::ReaderFailure,
        EntryFault::Overflow,
        EntryFault::UnreadyBinding,
        EntryFault::RebindAmbiguous,
    ] {
        let (summary, mut store, temp, log) = run_entry(fault);
        assert!(summary.quarantined, "{fault:?}");
        let log = log.lock().unwrap();
        assert!(log.wires.len() <= 1, "{fault:?}");
        println!(
            "FW06_ENTRY_FAULT {}",
            serde_json::json!({ "fault": format!("{fault:?}"), "offered_wires": log.wires,
            "os_offered_bytes": summary.os_offered_bytes, "diagnostic": summary.diagnostic, "quarantined": summary.quarantined,
            "allocated_raw_frontier": summary.allocated_raw_frontier, "physical_packets": 0 })
        );
        assert!(log.closed);
        assert_eq!(summary.device_verified_bytes, 0);
        assert!(!summary.completion_verified);
        let ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
        let id = ledger.attempt_ids().unwrap().pop().unwrap();
        assert!(!ledger.load(&id).unwrap().can_resume());
        assert!(
            crate::firmware_quarantine::QuarantineRegistry::mock(&temp.0.join("registry"))
                .unwrap()
                .admit_normal()
                .is_err()
        );
    }
}
#[test]
fn entry_actual_registry_tail_and_missing_journal_issue_no_first_syscall_and_return_store() {
    for fault in [EntryFault::RegistryTail, EntryFault::LedgerMissing] {
        let (summary, _store, temp, log) = run_entry(fault);
        assert!(summary.quarantined);
        assert!(log.lock().unwrap().wires.is_empty());
        assert!(summary.diagnostic.is_some());
        assert!(
            crate::firmware_quarantine::QuarantineRegistry::mock(&temp.0.join("registry"))
                .unwrap()
                .admit_normal()
                .is_err()
        );
    }
}
#[test]
fn entry_actual_join_retains_late_original_frontier_and_never_clears_quarantine() {
    let (summary, _store, _temp, log) = run_entry(EntryFault::LateClose);
    assert!(summary.quarantined);
    assert_eq!(log.lock().unwrap().wires.len(), 2);
    assert!(
        summary
            .retained_raw_chunks
            .iter()
            .any(|r| r.raw == [0xaa, 0xbb])
    );
    assert_eq!(summary.allocated_raw_frontier, 1);
}
#[test]
fn entry_public_imported_inputs_cannot_issue_either_production_authority() {
    assert!(crate::firmware_profile::issue_entry().is_err());
    assert!(crate::firmware_profile::issue_physical().is_err());
    assert!(crate::firmware_profile::issue_boot_binding().is_err());
    let temp = Temp::new();
    assert_eq!(
        FirmwareOwnershipRequest::new(job(), LibraryStore::open(temp.0.join("library")).unwrap())
            .purpose(),
        crate::firmware_profile::FirmwarePurpose::BootTransfer
    );
    let plan = crate::firmware_profile::entry_plan(FirmwareLifecyclePlanDto {
        review_sha256: job().digest().hex(),
        profile_sha256: JobSha256::of(b"entry").hex(),
        origin: FirmwareObservationOrigin::LocalObservation,
        nonblock_actions: 2,
        epoch: SessionEpoch(1),
        binding: entry_binding(),
        generation: 1,
        clock_origin: "test".into(),
    });
    assert!(crate::firmware_profile::derive_entry(&plan, 2).is_err());
    assert!(crate::firmware_profile::derive_entry(&plan, u64::MAX).is_err());
}
