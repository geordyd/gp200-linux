//! Actual normal worker admission negatives, with no provider or device access.
use gp200_domain::*;
use gp200_protocol::{
    RawRecord, RawTransfer, Reassembler, StreamDecoder, StreamEvent, decode_packet, encode_transfer,
};
use gp200_transport::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct Backend {
    observations: bool,
    mode: BackendMode,
    writes: Arc<AtomicUsize>,
}
struct Session {
    observations: bool,
    sink: ReceiveSink,
    writes: Arc<AtomicUsize>,
}
fn usb() -> FirmwareUsbObservation {
    FirmwareUsbObservation {
        topology: "/replay/normal".into(),
        vendor: 0x84ef,
        product: 42,
        serial: "fixture".into(),
        interface: "/replay/normal/midi".into(),
        endpoint: "replay:pair".into(),
    }
}
fn context(version: Option<&str>, wrong_usb: bool) -> FirmwareBackupReviewContext {
    let device = if wrong_usb {
        FirmwareDeviceAssociation::usb_serial("GP-200", 0x84ef, 42, "another-pedal").unwrap()
    } else {
        FirmwareDeviceAssociation::named_local_record("GP-200", "descriptive-fixture").unwrap()
    };
    FirmwareBackupReviewContext {
        device: device.clone(),
        manual_version: version.map(|v| {
            UserReportedFirmware::new(v, device, FirmwareReportDate::new(2026, 10, 11).unwrap())
                .unwrap()
        }),
        destination: FirmwareBackupDestination {
            model: "GP-200".into(),
            version: FirmwareVersion::new(1, 8, 2).unwrap(),
            image_sha256: "b".repeat(64),
            restoration_profile: None,
        },
        local_material: vec![],
        preservation_uncertainty: "IR audio unavailable; non-atomic".into(),
        recovery_limitations: "No physical updater grant".into(),
    }
}
impl TransportBackend for Backend {
    fn mode(&self) -> BackendMode {
        self.mode
    }
    fn enumerate(&mut self) -> Result<Vec<PortPairCandidate>, ServiceError> {
        Ok(vec![])
    }
    fn connect(
        &mut self,
        _: &DeviceSelection,
        sink: ReceiveSink,
    ) -> Result<Box<dyn MidiSession>, ServiceError> {
        Ok(Box::new(Session {
            observations: self.observations,
            sink,
            writes: self.writes.clone(),
        }))
    }
}
impl MidiSession for Session {
    fn send(&mut self, bytes: &[u8]) -> Result<(), ServiceError> {
        let packet = decode_packet(bytes).unwrap();
        let request = RawTransfer::from_payload(packet.kind, packet.payload).unwrap();
        let raw = match request.records()[0].command {
            0x2001 => RawTransfer::from_records(
                0x12,
                &[RawRecord {
                    command: 0x2001,
                    body: vec![1, 0, 2, 0],
                }],
            )
            .unwrap(),
            0 => {
                let mut assembler = Reassembler::default();
                let mut result = None;
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
            }
            0x1006 => RawTransfer::from_payload(
                0x12,
                include_bytes!("../../../tests/fixtures/globals/baseline.payload.bin").to_vec(),
            )
            .unwrap(),
            other => panic!("Unexpected normal query {other:x}"),
        };
        for packet in encode_transfer(&raw).unwrap() {
            self.sink.receive(&packet);
        }
        Ok(())
    }
    fn normal_capture_observation(
        &mut self,
    ) -> Result<FirmwareNormalCaptureObservation, ServiceError> {
        if self.observations {
            FirmwareNormalCaptureObservation::replay(usb(), "public-replay-only")
        } else {
            Err(ServiceError::new(
                ErrorKind::Unverified,
                "No capture observation",
            ))
        }
    }
    fn capture_write_once(&mut self, _: &[u8]) -> FirmwareCaptureWriteResult {
        self.writes.fetch_add(1, Ordering::SeqCst);
        FirmwareCaptureWriteResult::Unknown(ServiceError::new(
            ErrorKind::Io,
            "No capture responder",
        ))
    }
}
fn ready(
    observations: bool,
    mode: BackendMode,
) -> (
    MidiDeviceService,
    FirmwareBackupCaptureClient,
    SessionEpoch,
    Arc<AtomicUsize>,
) {
    let writes = Arc::new(AtomicUsize::new(0));
    let mut service = MidiDeviceService::with_backend(
        Box::new(Backend {
            observations,
            mode,
            writes: writes.clone(),
        }),
        TransportConfig {
            minimum_request_spacing: Duration::ZERO,
            poll_ports_while_connected: false,
            ..Default::default()
        },
        no_wake(),
    );
    let capture = service.take_firmware_backup_capture_client().unwrap();
    assert!(service.take_firmware_backup_capture_client().is_none());
    assert_eq!(
        service.firmware_backup_capture_support(),
        FirmwareCaptureSupport::ConcreteClientRequired
    );
    service
        .try_submit(DeviceCommand::Connect {
            intent: IntentId(1),
            selection: DeviceSelection {
                input_id: "fixture-in".into(),
                output_id: "fixture-out".into(),
                fingerprint_hint: None,
            },
        })
        .unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    let mut epoch = SessionEpoch(0);
    loop {
        if let Some(event) = service.try_next_event() {
            match event {
                DeviceEvent::SessionChanged { epoch: e, .. } => epoch = e,
                DeviceEvent::ConnectionChanged {
                    state: ConnectionState::Ready,
                    ..
                } => break,
                _ => {}
            }
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    while service.try_next_event().is_some() {}
    service
        .try_submit(DeviceCommand::ReadGlobals(RequestStamp {
            epoch,
            intent: IntentId(2),
        }))
        .unwrap();
    loop {
        if matches!(
            service.try_next_event(),
            Some(DeviceEvent::GlobalStateReceived { .. })
        ) {
            break;
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    while service.try_next_event().is_some() {}
    thread::sleep(Duration::from_millis(5));
    (service, capture, epoch, writes)
}
fn result(client: &FirmwareBackupCaptureClient) -> FirmwareBackupCaptureEvent {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(event) = client.try_next_event() {
            return event;
        }
        assert!(
            Instant::now() < until,
            "No capture event: {:?}",
            client.terminal_failure()
        );
        thread::sleep(Duration::from_millis(1));
    }
}
#[test]
fn unsupported_and_public_replay_cannot_mint_guarded_system_provenance() {
    for (observations, mode) in [
        (false, BackendMode::Simulator),
        (true, BackendMode::Hardware),
    ] {
        let (service, capture, epoch, writes) = ready(observations, mode);
        capture
            .try_submit(FirmwareBackupCaptureCommand::Begin(context(
                Some("V1.6.3"),
                false,
            )))
            .unwrap();
        assert!(matches!(
            result(&capture),
            FirmwareBackupCaptureEvent::Rejected(_)
        ));
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        service
            .try_submit(DeviceCommand::ReadCurrent(RequestStamp {
                epoch,
                intent: IntentId(3),
            }))
            .unwrap();
        service.shutdown();
        assert!(service.wait_stopped(Duration::from_secs(2)));
    }
}
#[test]
fn usb_report_cannot_be_attached_to_a_different_observed_device() {
    let (service, capture, _, writes) = ready(true, BackendMode::Simulator);
    capture
        .try_submit(FirmwareBackupCaptureCommand::Begin(context(
            Some("V1.6.3"),
            true,
        )))
        .unwrap();
    assert!(matches!(
        result(&capture),
        FirmwareBackupCaptureEvent::Rejected(ServiceError {
            kind: ErrorKind::Conflict,
            ..
        })
    ));
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    service.shutdown();
    assert!(service.wait_stopped(Duration::from_secs(2)));
}
#[test]
fn prepared_durability_stall_blocks_reads_and_premature_return_then_cancel_keeps_original_failure()
{
    let (service, capture, epoch, writes) = ready(true, BackendMode::Simulator);
    capture
        .try_submit(FirmwareBackupCaptureCommand::Begin(context(
            Some("V1.6.3"),
            false,
        )))
        .unwrap();
    assert!(matches!(
        result(&capture),
        FirmwareBackupCaptureEvent::Prepared { .. }
    ));
    capture
        .try_submit(FirmwareBackupCaptureCommand::ReturnToNormal)
        .unwrap();
    assert!(matches!(
        result(&capture),
        FirmwareBackupCaptureEvent::Rejected(_)
    ));
    assert!(matches!(
        service.try_submit(DeviceCommand::ReadCurrent(RequestStamp {
            epoch,
            intent: IntentId(3)
        })),
        Err(SubmitError::Busy(_))
    ));
    thread::sleep(Duration::from_millis(20));
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    capture.cancel();
    thread::sleep(Duration::from_millis(20));
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    assert!(capture.terminal_failure().is_some());
    service.shutdown();
    assert!(service.wait_stopped(Duration::from_secs(2)));
}
#[test]
fn dropping_the_only_capture_consumer_revokes_pending_progress_without_writing() {
    let (service, capture, _, writes) = ready(true, BackendMode::Simulator);
    capture
        .try_submit(FirmwareBackupCaptureCommand::Begin(context(
            Some("V1.6.3"),
            false,
        )))
        .unwrap();
    assert!(matches!(
        result(&capture),
        FirmwareBackupCaptureEvent::Prepared { .. }
    ));
    drop(capture);
    thread::sleep(Duration::from_millis(20));
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    service.shutdown();
    assert!(service.wait_stopped(Duration::from_secs(2)));
}
