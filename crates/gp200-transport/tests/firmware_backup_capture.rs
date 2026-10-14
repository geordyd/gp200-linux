use gp200_domain::*;
use gp200_library::*;
use gp200_protocol::{
    RawRecord, RawTransfer, Reassembler, StreamDecoder, StreamEvent, decode_packet, encode_transfer,
};
use gp200_transport::*;
use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn usb() -> FirmwareUsbObservation {
    FirmwareUsbObservation {
        topology: "/replay/gp200".into(),
        vendor: 0x84ef,
        product: 0x2a,
        serial: "replay-fixed-serial".into(),
        interface: "/replay/gp200/midi".into(),
        endpoint: "replay:normal".into(),
    }
}
fn context(nam: bool) -> FirmwareBackupReviewContext {
    let device =
        FirmwareDeviceAssociation::named_local_record("GP-200", "independent-replay-fixture")
            .unwrap();
    FirmwareBackupReviewContext {
        device: device.clone(),
        manual_version: (!nam).then(|| {
            UserReportedFirmware::new(
                "V1.6.3",
                device,
                FirmwareReportDate::new(2026, 10, 11).unwrap(),
            )
            .unwrap()
        }),
        destination: FirmwareBackupDestination {
            model: "GP-200".into(),
            version: FirmwareVersion::new(1, 8, 2).unwrap(),
            image_sha256: "a".repeat(64),
            restoration_profile: None,
        },
        local_material: vec![],
        preservation_uncertainty:
            "Both original raw passes; non-atomic and all IR audio unavailable".into(),
        recovery_limitations: "Replay fixture; physical updater unavailable".into(),
    }
}
fn preset(group: u16, slot: u16) -> RawTransfer {
    let mut assembly = Reassembler::default();
    let mut original = None;
    for event in StreamDecoder::new().feed(include_bytes!(
        "../../gp200-protocol/tests/fixtures/current-baseline.bin"
    )) {
        if let StreamEvent::Packet(p) = event
            && let Some(raw) = assembly.add(&p, Duration::ZERO).unwrap()
        {
            original = Some(raw);
        }
    }
    let mut records = original.unwrap().records().to_vec();
    for record in &mut records {
        match record.command {
            0 => {
                record.body[..2].copy_from_slice(&group.to_le_bytes());
                record.body[2..4].copy_from_slice(&slot.to_le_bytes());
            }
            2 | 8 => record.body[..2].copy_from_slice(&slot.to_le_bytes()),
            0x14 => {
                record.body[8 + 13 * 4..8 + 14 * 4]
                    .copy_from_slice(&(0x7fc0_0000_u32 | u32::from(slot)).to_le_bytes());
                record.body[8 + 14 * 4..8 + 15 * 4].copy_from_slice(&0x8000_0000_u32.to_le_bytes());
            }
            _ => {}
        }
    }
    records.push(RawRecord {
        command: 0xeef1,
        body: vec![
            group as u8,
            (slot & 255) as u8,
            ((slot >> 8) & 255) as u8,
            0x80,
            0xff,
        ],
    });
    RawTransfer::from_records(0x12, &records).unwrap()
}
fn globals() -> RawTransfer {
    RawTransfer::from_payload(
        0x12,
        include_bytes!("../../../tests/fixtures/globals/baseline.payload.bin").to_vec(),
    )
    .unwrap()
}
fn identity() -> RawTransfer {
    RawTransfer::from_records(
        0x12,
        &[RawRecord {
            command: 0x2001,
            body: vec![1, 0, 2, 0],
        }],
    )
    .unwrap()
}
#[derive(Clone, Copy, Debug)]
enum Fault {
    None,
    Partial,
    Zero,
    Unknown,
    Impossible,
    Duplicate,
    WrongSlot,
    Malformed,
    Overflow,
    Masked,
    Forbidden,
    WrongKind,
    DuplicateContext,
    MissingModule,
    MissingAssignment,
    MissingMapping,
    Reorder,
    ChangedNan,
    Missing,
}
struct State {
    offers: Mutex<Vec<(Instant, Vec<u8>)>>,
    ordinary: AtomicUsize,
    preflight_delay_ms: AtomicUsize,
    sink: Mutex<Option<ReceiveSink>>,
    fault: Mutex<Fault>,
    owner: Mutex<String>,
    opens: AtomicUsize,
    closes: AtomicUsize,
    counts: Mutex<std::collections::BTreeMap<(u16, u16), usize>>,
}
impl State {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            offers: Mutex::new(vec![]),
            ordinary: AtomicUsize::new(0),
            preflight_delay_ms: AtomicUsize::new(0),
            sink: Mutex::new(None),
            fault: Mutex::new(Fault::None),
            owner: Mutex::new("replay-owner".into()),
            opens: AtomicUsize::new(0),
            closes: AtomicUsize::new(0),
            counts: Mutex::new(std::collections::BTreeMap::new()),
        })
    }
}
struct Backend(Arc<State>);
struct Session {
    state: Arc<State>,
    sink: ReceiveSink,
    closed: bool,
}
fn selection() -> DeviceSelection {
    DeviceSelection {
        input_id: "replay-in".into(),
        output_id: "replay-out".into(),
        fingerprint_hint: Some(DeviceFingerprint {
            vendor: Some(0xffff),
            product: Some(0xffff),
            serial: Some("wrong-saved-hint".into()),
            hint: "saved hint never proof".into(),
        }),
    }
}
impl TransportBackend for Backend {
    fn mode(&self) -> BackendMode {
        BackendMode::Simulator
    }
    fn enumerate(&mut self) -> Result<Vec<PortPairCandidate>, ServiceError> {
        Ok(vec![])
    }
    fn connect(
        &mut self,
        _: &DeviceSelection,
        sink: ReceiveSink,
    ) -> Result<Box<dyn MidiSession>, ServiceError> {
        self.0.opens.fetch_add(1, Ordering::SeqCst);
        *self.0.sink.lock().unwrap() = Some(sink.clone());
        Ok(Box::new(Session {
            state: self.0.clone(),
            sink,
            closed: false,
        }))
    }
}
impl Session {
    fn respond(&self, bytes: &[u8], capture: bool) {
        let p = decode_packet(bytes).unwrap();
        assert_eq!(p.kind, 0x11);
        assert_eq!(p.offset, 0);
        assert_eq!(p.total, p.payload.len());
        let query = RawTransfer::from_payload(p.kind, p.payload).unwrap();
        let first = query.records()[0].command;
        match first {
            0x2001 | 0x1006 => {
                assert_eq!(query.records().len(), 1);
                assert!(query.records()[0].body.is_empty());
            }
            0 => {
                assert_eq!(query.records().len(), 2);
                assert_eq!(query.records()[0].body.len(), 4);
                assert_eq!(query.records()[1].command, 1);
                assert_eq!(query.records()[1].body.len(), 4);
                let c = &query.records()[0].body;
                let group = u16::from_le_bytes([c[0], c[1]]);
                let slot = u16::from_le_bytes([c[2], c[3]]);
                let range = &query.records()[1].body;
                if group == 0 {
                    assert_eq!(slot, 0);
                    assert_eq!(range, &[255, 255, 255, 255]);
                } else {
                    assert_eq!(group, 1);
                    assert!(slot < 256);
                    assert_eq!(range, &[c[2], c[3], c[2], c[3]]);
                }
            }
            0x1009 | 0x1012 => {
                assert_eq!(query.records().len(), 1);
                // Native metadata queries retain the whole canonical 24-byte
                // structure: requested slot, one-record marker, and zero tail.
                assert_eq!(query.records()[0].body.len(), 24);
                assert_eq!(&query.records()[0].body[2..4], &[1, 0]);
                assert!(query.records()[0].body[4..].iter().all(|b| *b == 0));
                let slot = u16::from_le_bytes(query.records()[0].body[..2].try_into().unwrap());
                assert!(slot < if first == 0x1009 { 20 } else { 10 });
            }
            other => panic!("Unexpected mutation/query {other:04x}"),
        }
        let key = if first == 0 {
            let c = &query.records()[0].body;
            assert_eq!(query.records().len(), 2);
            assert_eq!(query.records()[1].command, 1);
            (
                u16::from_le_bytes([c[0], c[1]]),
                u16::from_le_bytes([c[2], c[3]]),
            )
        } else {
            (
                first,
                query.records()[0].body.first().copied().unwrap_or(0) as u16,
            )
        };
        let fault = if capture {
            *self.state.fault.lock().unwrap()
        } else {
            Fault::None
        };
        let mut raw = match first {
            0x2001 => identity(),
            0x1006 => globals(),
            0 => {
                let mut slot = key.1;
                if matches!(fault, Fault::WrongSlot) {
                    slot = (slot + 1) % 256;
                }
                preset(key.0, if key.0 == 0 { 23 } else { slot })
            }
            0x1009 | 0x1012 => {
                let slot = u16::from_le_bytes(query.records()[0].body[..2].try_into().unwrap());
                let mut body = vec![0; 24];
                body[..2].copy_from_slice(&slot.to_le_bytes());
                body[4..8].copy_from_slice(&(0x1234_0000 + u32::from(slot)).to_le_bytes());
                body[8..13].copy_from_slice(b"IRRAW");
                RawTransfer::from_records(
                    0x12,
                    &[RawRecord {
                        command: first,
                        body,
                    }],
                )
                .unwrap()
            }
            _ => panic!("Unexpected mutation/query {first:04x}"),
        };
        if capture && matches!(fault, Fault::WrongKind) {
            raw = RawTransfer::from_records(0x11, raw.records()).unwrap();
        }
        if capture
            && first == 0
            && matches!(
                fault,
                Fault::DuplicateContext | Fault::MissingModule | Fault::MissingAssignment
            )
        {
            let mut records = raw.records().to_vec();
            match fault {
                Fault::DuplicateContext => {
                    records.push(records.iter().find(|r| r.command == 0).unwrap().clone())
                }
                Fault::MissingModule => {
                    let i = records.iter().position(|r| r.command == 0x14).unwrap();
                    records.remove(i);
                }
                Fault::MissingAssignment => {
                    let i = records.iter().position(|r| r.command == 0x0c).unwrap();
                    records.remove(i);
                }
                _ => unreachable!(),
            };
            raw = RawTransfer::from_records(0x12, &records).unwrap();
        }
        if capture && first == 0x1006 && matches!(fault, Fault::MissingMapping) {
            let mut records = raw.records().to_vec();
            let i = records.iter().position(|r| r.command == 0x100e).unwrap();
            records.remove(i);
            raw = RawTransfer::from_records(0x12, &records).unwrap();
        }
        if capture {
            let mut counts = self.state.counts.lock().unwrap();
            let count = counts.entry(key).or_insert(0);
            *count += 1;
            if *count >= 2 && matches!(fault, Fault::Masked | Fault::Forbidden) && first == 0 {
                let mut records = raw.records().to_vec();
                let header = records.iter_mut().find(|r| r.command == 2).unwrap();
                header.body[if matches!(fault, Fault::Masked) {
                    15
                } else {
                    14
                }] ^= 1;
                raw = RawTransfer::from_records(0x12, &records).unwrap();
            }
        }
        if capture
            && matches!(fault, Fault::Reorder | Fault::ChangedNan)
            && first == 0
            && self.state.counts.lock().unwrap()[&key] >= 2
        {
            let mut records = raw.records().to_vec();
            if matches!(fault, Fault::Reorder) {
                records.swap(1, 2);
            } else {
                let module = records.iter_mut().find(|r| r.command == 0x14).unwrap();
                module.body[8 + 13 * 4] ^= 1;
            }
            raw = RawTransfer::from_records(0x12, &records).unwrap();
        }
        if matches!(fault, Fault::Missing) {
            return;
        }
        if matches!(fault, Fault::Malformed) {
            self.sink.receive(&[0xf0, 0x21, 0xf7]);
            return;
        }
        let packets = encode_transfer(&raw).unwrap();
        for packet in &packets {
            for chunk in packet.chunks(97) {
                self.sink.receive(chunk);
            }
        }
        if matches!(fault, Fault::Duplicate) {
            for packet in &packets {
                self.sink.receive(packet);
            }
        }
        if matches!(fault, Fault::Overflow) {
            self.sink
                .receive(&vec![1; gp200_protocol::MAX_PACKET_LENGTH + 31]);
        }
    }
}
impl MidiSession for Session {
    fn send(&mut self, bytes: &[u8]) -> Result<(), ServiceError> {
        self.state.ordinary.fetch_add(1, Ordering::SeqCst);
        self.respond(bytes, false);
        Ok(())
    }
    fn normal_capture_observation(
        &mut self,
    ) -> Result<FirmwareNormalCaptureObservation, ServiceError> {
        let delay = self.state.preflight_delay_ms.swap(0, Ordering::SeqCst);
        if delay > 0 {
            thread::sleep(Duration::from_millis(delay as u64));
        }
        if self.closed {
            return Err(ServiceError::new(ErrorKind::Disconnected, "closed"));
        }
        FirmwareNormalCaptureObservation::replay(usb(), self.state.owner.lock().unwrap().clone())
    }
    fn capture_write_once(&mut self, bytes: &[u8]) -> FirmwareCaptureWriteResult {
        self.state
            .offers
            .lock()
            .unwrap()
            .push((Instant::now(), bytes.to_vec()));
        let fault = *self.state.fault.lock().unwrap();
        match fault {
            Fault::Partial => FirmwareCaptureWriteResult::Known {
                accepted: 3,
                error: None,
            },
            Fault::Zero => FirmwareCaptureWriteResult::Known {
                accepted: 0,
                error: Some(ServiceError::new(ErrorKind::Io, "would block")),
            },
            Fault::Unknown => FirmwareCaptureWriteResult::Unknown(ServiceError::new(
                ErrorKind::Io,
                "unknown offer extent",
            )),
            Fault::Impossible => FirmwareCaptureWriteResult::Known {
                accepted: bytes.len() + 1,
                error: None,
            },
            _ => {
                self.respond(bytes, true);
                FirmwareCaptureWriteResult::Known {
                    accepted: bytes.len(),
                    error: None,
                }
            }
        }
    }
    fn close_normal_capture(&mut self) -> Result<(), ServiceError> {
        if self.closed {
            return Err(ServiceError::new(ErrorKind::Disconnected, "already closed"));
        }
        self.closed = true;
        self.state.closes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "gp200-T-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn wait_capture(client: &FirmwareBackupCaptureClient) -> FirmwareBackupCaptureEvent {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(event) = client.try_next_event() {
            return event;
        }
        assert!(
            Instant::now() < until,
            "Capture result missing: {:?}",
            client.terminal_failure()
        );
        thread::sleep(Duration::from_millis(1));
    }
}
fn wait_library(client: &mut FirmwareBackupClient) -> FirmwareCaptureDurableResult {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(event) = client.try_next_event() {
            return match event.result.unwrap() {
                FirmwareBackupResult::CaptureDurable(r) => *r,
                _ => panic!("Wrong acknowledgement"),
            };
        }
        assert!(
            Instant::now() < until,
            "Library missing: {:?}",
            client.terminal_failure()
        );
        thread::sleep(Duration::from_millis(1));
    }
}
fn store_event(
    client: &mut FirmwareBackupClient,
    event: FirmwareBackupCaptureEvent,
) -> FirmwareCaptureDurableResult {
    client
        .try_submit(event.into_library_action().unwrap())
        .ok()
        .unwrap();
    wait_library(client)
}
fn startup(
    state: Arc<State>,
    spacing: Duration,
) -> (MidiDeviceService, FirmwareBackupCaptureClient, SessionEpoch) {
    let mut service = MidiDeviceService::with_backend(
        Box::new(Backend(state)),
        TransportConfig {
            minimum_request_spacing: spacing,
            poll_ports_while_connected: false,
            ..Default::default()
        },
        no_wake(),
    );
    let capture = service.take_firmware_backup_capture_client().unwrap();
    assert!(service.take_firmware_backup_capture_client().is_none());
    service
        .try_submit(DeviceCommand::Connect {
            intent: IntentId(1),
            selection: selection(),
        })
        .unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    let mut epoch = SessionEpoch(0);
    let mut ready = false;
    while !ready {
        if let Some(event) = service.try_next_event() {
            match event {
                DeviceEvent::SessionChanged { epoch: e, .. } => epoch = e,
                DeviceEvent::ConnectionChanged {
                    state: ConnectionState::Ready,
                    ..
                } => ready = true,
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
    (service, capture, epoch)
}
// A second actual library writer issues genuine sealed review results for
// descriptive replay-only identity evidence. It performs no device offer and
// cannot manufacture the actual normal service's private reservation/run.
fn other_revalidated_review() -> (
    Temp,
    LibraryWorker,
    FirmwareBackupClient,
    FirmwareCaptureDurableResult,
) {
    let temp = Temp::new();
    let mut worker = LibraryWorker::open(&temp.0, no_wake()).unwrap();
    let mut client = worker.take_firmware_backup_client().unwrap();
    let reservation = FirmwareCaptureReservation {
        actor_run: "other-replay-run".into(),
        reservation: "other-replay-reservation".into(),
        normal_owner: "other-descriptive-owner".into(),
        epoch: SessionEpoch(1),
        usb: usb(),
        clock_origin: "other-replay-clock".into(),
        prepared_monotonic_ns: 0,
        prepared_wall_millis: 1,
        initial_fence: FirmwareCaptureFence {
            allocated: 0,
            processed: 0,
        },
    };
    client
        .try_submit(FirmwareBackupAction::PrepareCapture(reservation.clone()))
        .ok()
        .unwrap();
    let prepared = wait_library(&mut client);
    let expected = prepared.receipt.description().clone();
    prepared
        .receipt
        .try_with_first_offer(&expected, || ())
        .unwrap();
    let make = |kind, sequence: u64| {
        let read = FirmwareCaptureRead::Identity(kind);
        let offered = read.request_bytes().unwrap();
        let callback = encode_transfer(&identity()).unwrap().remove(0);
        let mut raw = vec![0x12];
        raw.extend_from_slice(identity().payload());
        FirmwareCaptureEvidence {
            read,
            sequence,
            before: FirmwareCaptureFence {
                allocated: sequence - 1,
                processed: sequence - 1,
            },
            after: FirmwareCaptureFence {
                allocated: sequence,
                processed: sequence,
            },
            offer: Some(FirmwareCaptureOfferInterval {
                started_ns: sequence * 1000,
                returned_ns: sequence * 1000 + 1,
                accepted_bytes: offered.len() as u64,
                requested_bytes: offered.len() as u64,
            }),
            offered_read_bytes: offered,
            completed_ns: Some(sequence * 1000 + 3),
            callbacks: vec![FirmwareCaptureCallback {
                ordinal: sequence,
                epoch: reservation.epoch,
                monotonic_ns: Some(sequence * 1000 + 2),
                original_len: callback.len() as u64,
                retained: callback,
            }],
            missing: vec![],
            raw,
            complete: true,
            diagnostics: vec!["Independent descriptive replay fixture; no physical IO".into()],
        }
    };
    let boundary = |e: &FirmwareCaptureEvidence| FirmwareArchiveBoundary {
        epoch: reservation.epoch,
        identity: DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Simulator,
            firmware: None,
            display_name: "GP-200".into(),
        },
        device_key: reservation.usb.digest(),
        sequence: e.sequence,
        monotonic_millis: e.completed_ns.unwrap() / 1_000_000,
        wall_clock_millis: 1,
        raw_identity: e.raw.clone(),
    };
    let opening = make(FirmwareCaptureBoundaryKind::Opening, 1);
    let opening_boundary = boundary(&opening);
    client
        .try_submit(FirmwareBackupAction::BeginCaptured(Box::new(
            FirmwareCapturedBegin {
                begin: FirmwareBackupBegin {
                    origin: BackupOrigin {
                        identity: opening_boundary.identity.clone(),
                        epoch: reservation.epoch,
                        device_key: reservation.usb.digest(),
                        evidence: vec![],
                    },
                    context: context(false),
                    opening: opening_boundary,
                },
                evidence: opening,
            },
        )))
        .ok()
        .unwrap();
    let begun = wait_library(&mut client);
    let expected = begun.receipt.description().clone();
    begun
        .receipt
        .try_with_first_offer(&expected, || ())
        .unwrap();
    let middle = make(FirmwareCaptureBoundaryKind::Middle, 2);
    let closing = boundary(&middle);
    client
        .try_submit(FirmwareBackupAction::StoreCaptureBoundary(Box::new(middle)))
        .ok()
        .unwrap();
    let middle = wait_library(&mut client);
    let expected = middle.receipt.description().clone();
    middle
        .receipt
        .try_consume_for_capture_abort(&expected)
        .unwrap();
    client
        .try_submit(FirmwareBackupAction::CloseCaptured {
            boundary: closing,
            outcome: BackupAcquisition::Cancelled,
        })
        .ok()
        .unwrap();
    let closed = wait_library(&mut client);
    let expected = closed.receipt.description().clone();
    closed.receipt.try_consume_for_review(&expected).unwrap();
    client
        .try_submit(FirmwareBackupAction::PublishCaptured)
        .ok()
        .unwrap();
    let published = wait_library(&mut client);
    let expected = published.receipt.description().clone();
    published.receipt.try_consume_for_review(&expected).unwrap();
    client
        .try_submit(FirmwareBackupAction::RevalidateCaptured(
            published.publication.unwrap(),
        ))
        .ok()
        .unwrap();
    let result = wait_library(&mut client);
    (temp, worker, client, result)
}
fn full_capture(nam: bool, fault: Fault) {
    full_capture_controlled(nam, fault, false);
}
fn full_capture_controlled(nam: bool, fault: Fault, cancel_final: bool) {
    let state = State::new();
    let (service, capture, epoch) = startup(state.clone(), Duration::ZERO);
    let t = Temp::new();
    let mut worker = LibraryWorker::open(&t.0, no_wake()).unwrap();
    let mut library = worker.take_firmware_backup_client().unwrap();
    *state.fault.lock().unwrap() = fault;
    let expected_reads = if nam { 581 } else { 561 };
    capture
        .try_submit(FirmwareBackupCaptureCommand::Begin(context(nam)))
        .unwrap();
    let mut event = wait_capture(&capture);
    let mut reads = 0;
    let mut slots = [0_usize; 256];
    let mut masked = 0;
    loop {
        match &event {
            FirmwareBackupCaptureEvent::Prepared {
                reservation,
                origin,
            } => {
                assert_eq!(*origin, FirmwareCaptureOrigin::Replay);
                assert_eq!(reservation.usb, usb());
                assert_eq!(reservation.epoch, epoch);
            }
            FirmwareBackupCaptureEvent::Observation {
                evidence,
                consistency,
                ..
            } => {
                reads += 1;
                if let Some(c) = consistency
                    && !c.strict_raw_equal
                {
                    assert!(c.admitted);
                    assert!(matches!(fault, Fault::Masked));
                    masked += 1;
                }
                assert!(evidence.complete);
                assert!(evidence.validate().is_ok());
                assert_eq!(
                    evidence.offered_read_bytes,
                    evidence.read.request_bytes().unwrap()
                );
                if let FirmwareCaptureRead::Object {
                    object: FirmwareBackupObject::Stored(slot),
                    ..
                } = evidence.read
                {
                    slots[usize::from(slot)] += 1;
                    let mut expected = preset(1, slot);
                    if matches!(fault, Fault::Masked)
                        && matches!(
                            evidence.read,
                            FirmwareCaptureRead::Object {
                                pass: FirmwareObservationPass::Comparison,
                                ..
                            }
                        )
                    {
                        let mut records = expected.records().to_vec();
                        records.iter_mut().find(|r| r.command == 2).unwrap().body[15] ^= 1;
                        expected = RawTransfer::from_records(0x12, &records).unwrap();
                    }
                    assert_eq!(&evidence.raw[1..], expected.payload());
                }
            }
            FirmwareBackupCaptureEvent::CloseReady { outcome, boundary } => {
                if cancel_final {
                    assert_eq!(*outcome, BackupAcquisition::Cancelled);
                } else if matches!(fault, Fault::Masked) {
                    assert!(matches!(outcome, BackupAcquisition::Failed { .. }));
                } else {
                    assert_eq!(*outcome, BackupAcquisition::Finished);
                }
                assert_eq!(boundary.sequence, expected_reads);
            }
            _ => panic!("Unexpected capture event {event:?}"),
        }
        let closing = matches!(&event, FirmwareBackupCaptureEvent::CloseReady { .. });
        let final_observation = matches!(
            &event,
            FirmwareBackupCaptureEvent::Observation {
                evidence: FirmwareCaptureEvidence {
                    read: FirmwareCaptureRead::Identity(FirmwareCaptureBoundaryKind::Final),
                    ..
                },
                ..
            }
        );
        let durable = store_event(&mut library, event);
        if cancel_final && final_observation {
            assert_eq!(
                durable.receipt.description().kind,
                FirmwareCaptureReceiptKind::Boundary
            );
            capture.cancel();
            let until = Instant::now() + Duration::from_secs(5);
            while capture.terminal_failure().is_none() {
                while service.try_next_event().is_some() {}
                assert!(
                    Instant::now() < until,
                    "Final cancellation was not observed"
                );
                thread::sleep(Duration::from_millis(1));
            }
        }
        if closing {
            capture
                .try_submit(FirmwareBackupCaptureCommand::CloseAfterBoundary(
                    durable.receipt,
                ))
                .unwrap();
            break;
        }
        capture
            .try_submit(
                FirmwareBackupCaptureCommand::ContinueAfterDurableObservation(durable.receipt),
            )
            .unwrap();
        while service.try_next_event().is_some() {}
        event = wait_capture(&capture);
    }
    assert!(matches!(
        wait_capture(&capture),
        FirmwareBackupCaptureEvent::Closed { .. }
    ));
    assert_eq!(reads, expected_reads);
    if matches!(fault, Fault::Masked) {
        assert!(masked >= 257);
    }
    assert!(slots.into_iter().all(|n| n == 2));
    assert_eq!(state.offers.lock().unwrap().len(), expected_reads as usize);
    assert_eq!(state.opens.load(Ordering::SeqCst), 1);
    assert!(matches!(
        service.try_submit(DeviceCommand::ReadCurrent(RequestStamp {
            epoch,
            intent: IntentId(50)
        })),
        Err(SubmitError::Busy(_))
    ));
    library
        .try_submit(FirmwareBackupAction::PublishCaptured)
        .ok()
        .unwrap();
    let published = wait_library(&mut library);
    let description = published.receipt.description().clone();
    assert_eq!(description.kind, FirmwareCaptureReceiptKind::Published);
    published
        .receipt
        .try_consume_for_review(&description)
        .unwrap();
    let publication = published.publication.unwrap();
    assert_eq!(publication.summary().records, expected_reads as usize + 2);
    assert!(!publication.summary().permits_flash());
    assert!(!publication.summary().complete_restorable_backup());
    assert_eq!(publication.archive().summary().stored_presets, 256);
    assert_eq!(
        publication.archive().summary().comparison_complete,
        !matches!(fault, Fault::Masked) && !cancel_final
    );
    assert_eq!(
        publication.summary().cancelled,
        matches!(fault, Fault::Masked) || cancel_final
    );
    assert_eq!(
        publication
            .archive()
            .summary()
            .coverage
            .iter()
            .filter(|c| c.installed_content_available)
            .count(),
        0
    );
    assert!(
        library
            .capture_retirement_blockers()
            .contains(&"Capture package not revalidated")
    );
    let saved_publication = publication.index_path().to_owned();
    library
        .try_submit(FirmwareBackupAction::RevalidateCaptured(publication))
        .ok()
        .unwrap();
    let revalidated = wait_library(&mut library);
    if !nam && matches!(fault, Fault::None) && !cancel_final {
        let (_temp, _other_worker, _other_client, other) = other_revalidated_review();
        capture
            .try_submit(FirmwareBackupCaptureCommand::AcceptRevalidatedReview(other))
            .unwrap();
        assert!(matches!(
            wait_capture(&capture),
            FirmwareBackupCaptureEvent::Rejected(_)
        ));
        let (_temp2, _other_worker2, _other_client2, mut absent) = other_revalidated_review();
        absent.publication = None;
        capture
            .try_submit(FirmwareBackupCaptureCommand::AcceptRevalidatedReview(
                absent,
            ))
            .unwrap();
        assert!(matches!(
            wait_capture(&capture),
            FirmwareBackupCaptureEvent::Rejected(_)
        ));
        capture
            .try_submit(FirmwareBackupCaptureCommand::ReturnToNormal)
            .unwrap();
        assert!(matches!(
            wait_capture(&capture),
            FirmwareBackupCaptureEvent::Rejected(_)
        ));
        assert_eq!(state.closes.load(Ordering::SeqCst), 0);
    }
    capture
        .try_submit(FirmwareBackupCaptureCommand::AcceptRevalidatedReview(
            revalidated,
        ))
        .unwrap();
    if cancel_final {
        assert!(matches!(
            wait_capture(&capture),
            FirmwareBackupCaptureEvent::Rejected(_)
        ));
        capture
            .try_submit(FirmwareBackupCaptureCommand::ReturnToNormal)
            .unwrap();
        assert!(matches!(
            wait_capture(&capture),
            FirmwareBackupCaptureEvent::Rejected(_)
        ));
        assert_eq!(state.closes.load(Ordering::SeqCst), 0);
        let reopened = reopen_firmware_capture_evidence(&saved_publication).unwrap();
        assert!(reopened.closed && reopened.cancelled);
        assert_eq!(reopened.records, expected_reads as usize + 2);
        // No invented post-Final archive observation or replacement boundary.
        let index: serde_json::Value =
            serde_json::from_slice(&fs::read(&saved_publication).unwrap()).unwrap();
        let entries = index["records"].as_array().unwrap();
        assert_eq!(entries.len(), expected_reads as usize + 2);
        let record_dir = saved_publication.parent().unwrap();
        let final_record = reopen_firmware_capture_record(
            &record_dir.join(entries[entries.len() - 2]["name"].as_str().unwrap()),
        )
        .unwrap();
        assert_eq!(final_record.kind, FirmwareCaptureReceiptKind::Boundary);
        let original = final_record.evidence.unwrap();
        assert_eq!(original.sequence, expected_reads);
        assert_eq!(
            original.read,
            FirmwareCaptureRead::Identity(FirmwareCaptureBoundaryKind::Final)
        );
        assert_eq!(&original.raw[1..], identity().payload());
        let closed_record = reopen_firmware_capture_record(
            &record_dir.join(entries[entries.len() - 1]["name"].as_str().unwrap()),
        )
        .unwrap();
        assert_eq!(closed_record.kind, FirmwareCaptureReceiptKind::Closed);
        assert!(closed_record.cancelled && closed_record.evidence.is_none());
        service.shutdown();
        assert!(service.wait_stopped(Duration::from_secs(2)));
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(FirmwareBackupCaptureEvent::RetainedAfterStop { originals, .. }) =
                service.try_next_retained_capture_event()
            {
                assert!(originals.iter().any(
                    |e| !e.complete && e.diagnostics.iter().any(|d| d.contains("cancellation"))
                ));
                break;
            }
            assert!(
                Instant::now() < until,
                "Post-Final cancellation originals were lost"
            );
            thread::sleep(Duration::from_millis(1));
        }
        return;
    }
    assert!(matches!(
        wait_capture(&capture),
        FirmwareBackupCaptureEvent::ReviewAccepted { .. }
    ));
    assert!(library.capture_retirement_blockers().is_empty());
    while service.try_next_event().is_some() {}
    capture
        .try_submit(FirmwareBackupCaptureCommand::ReturnToNormal)
        .unwrap();
    let retired = wait_capture(&capture);
    match retired {
        FirmwareBackupCaptureEvent::NormalRefreshRequired {
            retired_epoch,
            next_epoch,
            selection: Some(s),
        } => {
            assert_eq!(retired_epoch, epoch);
            assert!(next_epoch.0 > epoch.0);
            assert_eq!(s, selection());
        }
        _ => panic!("No deliberate normal return {retired:?}"),
    }
    assert_eq!(state.closes.load(Ordering::SeqCst), 1);
    service
        .try_submit(DeviceCommand::Connect {
            intent: IntentId(80),
            selection: selection(),
        })
        .unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if matches!(
            service.try_next_event(),
            Some(DeviceEvent::ConnectionChanged {
                state: ConnectionState::Ready,
                ..
            })
        ) {
            break;
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(state.opens.load(Ordering::SeqCst), 2);
    assert_eq!(state.offers.lock().unwrap().len(), expected_reads as usize);
    assert!(
        reopen_firmware_capture_evidence(&saved_publication)
            .unwrap()
            .closed
    );
    service.shutdown();
    assert!(service.wait_stopped(Duration::from_secs(2)));
}
#[test]
fn actual_service_same_writer_full_original_561_reads_and_deliberate_fresh_return() {
    full_capture(false, Fault::None);
}
#[test]
fn actual_service_581_reads_unknown_profile_is_strict_replay() {
    full_capture(true, Fault::None);
}
#[test]
fn actual_final_cancellation_reviews_original_final_and_closes_partial_without_new_observation() {
    full_capture_controlled(false, Fault::None, true);
}
#[test]
fn actual_service_admitted_163_offsets_preserve_both_originals_and_strict_partial_archive() {
    full_capture(false, Fault::Masked);
}

#[test]
fn partial_zero_unknown_impossible_first_offer_retains_original_and_never_retries() {
    for fault in [
        Fault::Partial,
        Fault::Zero,
        Fault::Unknown,
        Fault::Impossible,
        Fault::Duplicate,
        Fault::Malformed,
        Fault::Overflow,
    ] {
        let state = State::new();
        let (service, capture, _) = startup(state.clone(), Duration::ZERO);
        *state.fault.lock().unwrap() = fault;
        let t = Temp::new();
        let mut worker = LibraryWorker::open(&t.0, no_wake()).unwrap();
        let mut library = worker.take_firmware_backup_client().unwrap();
        capture
            .try_submit(FirmwareBackupCaptureCommand::Begin(context(false)))
            .unwrap();
        let prepared = store_event(&mut library, wait_capture(&capture));
        capture
            .try_submit(
                FirmwareBackupCaptureCommand::ContinueAfterDurableObservation(prepared.receipt),
            )
            .unwrap();
        let failed = wait_capture(&capture);
        match &failed {
            FirmwareBackupCaptureEvent::Failed {
                evidence: Some(e), ..
            } => {
                assert!(!e.complete);
                assert!(e.validate().is_ok(), "Invalid failure {fault:?}: {e:?}");
                assert_eq!(e.offered_read_bytes, e.read.request_bytes().unwrap());
                assert!(e.offer.is_some());
                if matches!(fault, Fault::Duplicate | Fault::Malformed | Fault::Overflow) {
                    assert!(!e.callbacks.is_empty() || !e.missing.is_empty());
                }
            }
            _ => panic!("Missing actual failure for {fault:?}: {failed:?}"),
        }
        let receipt = store_event(&mut library, failed);
        let description = receipt.receipt.description().clone();
        receipt
            .receipt
            .try_consume_for_review(&description)
            .unwrap();
        thread::sleep(Duration::from_millis(15));
        assert_eq!(state.offers.lock().unwrap().len(), 1);
        service.shutdown();
        assert!(service.wait_stopped(Duration::from_secs(2)));
    }
}
#[test]
fn changed_guarded_association_and_queued_stale_callback_abort_without_first_offer() {
    for stale in [false, true] {
        let state = State::new();
        let (service, capture, _) = startup(state.clone(), Duration::ZERO);
        let t = Temp::new();
        let mut worker = LibraryWorker::open(&t.0, no_wake()).unwrap();
        let mut library = worker.take_firmware_backup_client().unwrap();
        capture
            .try_submit(FirmwareBackupCaptureCommand::Begin(context(false)))
            .unwrap();
        let prepared = store_event(&mut library, wait_capture(&capture));
        if stale {
            for packet in encode_transfer(&identity()).unwrap() {
                state
                    .sink
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .receive(&packet);
            }
        } else {
            *state.owner.lock().unwrap() = "other-private-owner".into();
        }
        capture
            .try_submit(
                FirmwareBackupCaptureCommand::ContinueAfterDurableObservation(prepared.receipt),
            )
            .unwrap();
        let failed = wait_capture(&capture);
        if let FirmwareBackupCaptureEvent::Failed {
            evidence: Some(e), ..
        } = &failed
        {
            assert!(!e.complete);
            assert!(e.offer.is_none());
            assert!(e.offered_read_bytes.is_empty());
            if stale {
                assert!(!e.callbacks.is_empty());
            }
        } else {
            panic!("Unexpected {failed:?}")
        };
        let receipt = store_event(&mut library, failed);
        let desc = receipt.receipt.description().clone();
        receipt.receipt.try_consume_for_review(&desc).unwrap();
        assert!(state.offers.lock().unwrap().is_empty());
        service.shutdown();
        assert!(service.wait_stopped(Duration::from_secs(2)));
    }
}

#[test]
fn actual_semantic_and_structural_failures_keep_original_raw_and_do_not_retry() {
    for fault in [
        Fault::WrongSlot,
        Fault::WrongKind,
        Fault::DuplicateContext,
        Fault::MissingModule,
        Fault::MissingAssignment,
        Fault::MissingMapping,
        Fault::Forbidden,
        Fault::Reorder,
        Fault::ChangedNan,
        Fault::Missing,
        Fault::Masked,
    ] {
        let state = State::new();
        let (service, capture, _) = startup(state.clone(), Duration::ZERO);
        *state.fault.lock().unwrap() = fault;
        let t = Temp::new();
        let mut worker = LibraryWorker::open(&t.0, no_wake()).unwrap();
        let mut library = worker.take_firmware_backup_client().unwrap();
        capture
            .try_submit(FirmwareBackupCaptureCommand::Begin(context(matches!(
                fault,
                Fault::Masked
            ))))
            .unwrap();
        let mut event = wait_capture(&capture);
        loop {
            if let FirmwareBackupCaptureEvent::Failed {
                evidence: Some(e), ..
            } = &event
            {
                assert!(!e.complete);
                assert!(e.validate().is_ok(), "Failure {fault:?}: {e:?}");
                assert!(e.offer.is_some());
                if !matches!(fault, Fault::Missing) {
                    assert!(!e.raw.is_empty());
                    assert!(!e.callbacks.is_empty());
                }
                let offers = state.offers.lock().unwrap().len();
                let durable = store_event(&mut library, event);
                let expected = durable.receipt.description().clone();
                durable.receipt.try_consume_for_review(&expected).unwrap();
                thread::sleep(Duration::from_millis(10));
                assert_eq!(state.offers.lock().unwrap().len(), offers);
                break;
            }
            let durable = store_event(&mut library, event);
            capture
                .try_submit(
                    FirmwareBackupCaptureCommand::ContinueAfterDurableObservation(durable.receipt),
                )
                .unwrap();
            while service.try_next_event().is_some() {}
            event = wait_capture(&capture);
        }
        service.shutdown();
        assert!(service.wait_stopped(Duration::from_secs(2)));
    }
}

#[test]
fn lost_capture_receiver_recovers_original_completed_callbacks_passively_from_actual_run() {
    let state = State::new();
    let (service, capture, _) = startup(state.clone(), Duration::ZERO);
    let t = Temp::new();
    let mut worker = LibraryWorker::open(&t.0, no_wake()).unwrap();
    let mut library = worker.take_firmware_backup_client().unwrap();
    capture
        .try_submit(FirmwareBackupCaptureCommand::Begin(context(false)))
        .unwrap();
    let prepared = store_event(&mut library, wait_capture(&capture));
    capture
        .try_submit(FirmwareBackupCaptureCommand::ContinueAfterDurableObservation(prepared.receipt))
        .unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while state.offers.lock().unwrap().is_empty() {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    thread::sleep(Duration::from_millis(20));
    drop(capture);
    let mut found = false;
    while !found {
        if let Some(event) = service.try_next_retained_capture_event() {
            match event {
                FirmwareBackupCaptureEvent::RetainedAfterStop {
                    originals,
                    reservation,
                    ..
                } => {
                    for e in originals {
                        if e.raw == [&[0x12][..], identity().payload()].concat() {
                            assert!(!e.callbacks.is_empty());
                            assert!(e.callbacks.iter().all(|c| c.epoch == reservation.epoch));
                            found = true;
                        }
                    }
                }
                FirmwareBackupCaptureEvent::Revoked { original, .. } => {
                    if let FirmwareBackupCaptureEvent::Observation { evidence, .. } = *original {
                        assert!(!evidence.callbacks.is_empty());
                        assert_eq!(evidence.raw, [&[0x12][..], identity().payload()].concat());
                        found = true;
                    }
                }
                _ => {}
            }
        }
        assert!(
            Instant::now() < until,
            "Original queued evidence unavailable after consumer loss"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(state.offers.lock().unwrap().len(), 1);
    service.shutdown();
    assert!(service.wait_stopped(Duration::from_secs(2)));
}
#[test]
fn published_late_callback_survives_event_and_consumer_loss_in_actual_run() {
    let state = State::new();
    let (service, capture, epoch) = startup(state.clone(), Duration::ZERO);
    *state.fault.lock().unwrap() = Fault::Partial;
    let temp = Temp::new();
    let mut worker = LibraryWorker::open(&temp.0, no_wake()).unwrap();
    let mut library = worker.take_firmware_backup_client().unwrap();
    capture
        .try_submit(FirmwareBackupCaptureCommand::Begin(context(false)))
        .unwrap();
    let prepared = store_event(&mut library, wait_capture(&capture));
    capture
        .try_submit(FirmwareBackupCaptureCommand::ContinueAfterDurableObservation(prepared.receipt))
        .unwrap();
    let original_failure = wait_capture(&capture);
    assert!(matches!(
        original_failure,
        FirmwareBackupCaptureEvent::Failed { .. }
    ));
    // Publication made that failure action immutable. This later original is
    // retained separately, including after its passive diagnostic is discarded.
    let wire = vec![0xf0, 0x41, 0x55, 0xf7];
    state.sink.lock().unwrap().as_ref().unwrap().receive(&wire);
    let until = Instant::now() + Duration::from_secs(5);
    let late = loop {
        while service.try_next_event().is_some() {}
        if let Some(event) = capture.try_next_event() {
            break event;
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    };
    match &late {
        FirmwareBackupCaptureEvent::RejectedCallback { callback, .. } => {
            assert_eq!(callback.retained, wire);
            assert_eq!(callback.epoch, epoch);
        }
        _ => panic!("Late original was not retained passively {late:?}"),
    }
    assert!(late.into_library_action().is_err());
    drop(capture);
    loop {
        while service.try_next_event().is_some() {}
        if let Some(FirmwareBackupCaptureEvent::RetainedAfterStop { originals, .. }) =
            service.try_next_retained_capture_event()
        {
            assert!(
                originals
                    .iter()
                    .any(|e| e.callbacks.iter().any(|c| c.retained == wire
                        && c.epoch == epoch
                        && c.original_len == wire.len() as u64))
            );
            break;
        }
        assert!(Instant::now() < until, "Published late original was lost");
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(state.offers.lock().unwrap().len(), 1);
    service.shutdown();
    assert!(service.wait_stopped(Duration::from_secs(2)));
}
#[test]
fn cancelled_library_after_receipt_dequeue_and_capture_cancel_never_offer_a_read() {
    for stop_library in [false, true] {
        let state = State::new();
        let (service, capture, _) = startup(state.clone(), Duration::ZERO);
        let t = Temp::new();
        let mut worker = LibraryWorker::open(&t.0, no_wake()).unwrap();
        let mut library = worker.take_firmware_backup_client().unwrap();
        capture
            .try_submit(FirmwareBackupCaptureCommand::Begin(context(false)))
            .unwrap();
        let prepared = store_event(&mut library, wait_capture(&capture));
        if stop_library {
            library.cancel();
        } else {
            capture.cancel();
        }
        capture
            .try_submit(
                FirmwareBackupCaptureCommand::ContinueAfterDurableObservation(prepared.receipt),
            )
            .unwrap();
        let event = wait_capture(&capture);
        assert!(
            matches!(
                event,
                FirmwareBackupCaptureEvent::Failed { .. }
                    | FirmwareBackupCaptureEvent::Revoked { .. }
                    | FirmwareBackupCaptureEvent::Rejected(_)
            ),
            "Unexpected stopped progression {event:?}"
        );
        assert_eq!(state.offers.lock().unwrap().len(), 0);
        service.shutdown();
        assert!(service.wait_stopped(Duration::from_secs(2)));
    }
}
#[test]
fn genuine_receipt_for_other_reservation_is_denied_then_original_preparation_can_abort_durably() {
    let state = State::new();
    let (service, capture, _) = startup(state.clone(), Duration::ZERO);
    let t = Temp::new();
    let t2 = Temp::new();
    let mut worker = LibraryWorker::open(&t.0, no_wake()).unwrap();
    let mut library = worker.take_firmware_backup_client().unwrap();
    let mut other_worker = LibraryWorker::open(&t2.0, no_wake()).unwrap();
    let mut other = other_worker.take_firmware_backup_client().unwrap();
    capture
        .try_submit(FirmwareBackupCaptureCommand::Begin(context(false)))
        .unwrap();
    let event = wait_capture(&capture);
    let mut foreign = if let FirmwareBackupCaptureEvent::Prepared { reservation, .. } = &event {
        reservation.clone()
    } else {
        panic!("No actual reservation")
    };
    foreign.reservation.push_str("-foreign");
    other
        .try_submit(FirmwareBackupAction::PrepareCapture(foreign))
        .ok()
        .unwrap();
    let foreign = wait_library(&mut other);
    let original = store_event(&mut library, event);
    capture
        .try_submit(FirmwareBackupCaptureCommand::ContinueAfterDurableObservation(foreign.receipt))
        .unwrap();
    thread::sleep(Duration::from_millis(10));
    assert!(state.offers.lock().unwrap().is_empty());
    capture
        .try_submit(FirmwareBackupCaptureCommand::ContinueAfterDurableObservation(original.receipt))
        .unwrap();
    let failure = wait_capture(&capture);
    assert!(matches!(failure, FirmwareBackupCaptureEvent::Failed { .. }));
    let durable = store_event(&mut library, failure);
    let expected = durable.receipt.description().clone();
    durable.receipt.try_consume_for_review(&expected).unwrap();
    assert!(state.offers.lock().unwrap().is_empty());
    service.shutdown();
    assert!(service.wait_stopped(Duration::from_secs(2)));
}

#[test]
fn logical_spacing_uses_actual_first_write_clock_after_slow_preflight() {
    let state = State::new();
    let spacing = Duration::from_millis(25);
    let (service, capture, _) = startup(state.clone(), spacing);
    let t = Temp::new();
    let mut worker = LibraryWorker::open(&t.0, no_wake()).unwrap();
    let mut library = worker.take_firmware_backup_client().unwrap();
    capture
        .try_submit(FirmwareBackupCaptureCommand::Begin(context(false)))
        .unwrap();
    let prepared = store_event(&mut library, wait_capture(&capture));
    state.preflight_delay_ms.store(120, Ordering::SeqCst);
    capture
        .try_submit(FirmwareBackupCaptureCommand::ContinueAfterDurableObservation(prepared.receipt))
        .unwrap();
    let observed = wait_capture(&capture);
    let durable = store_event(&mut library, observed);
    capture
        .try_submit(FirmwareBackupCaptureCommand::ContinueAfterDurableObservation(durable.receipt))
        .unwrap();
    let observed = wait_capture(&capture);
    assert!(matches!(
        observed,
        FirmwareBackupCaptureEvent::Observation { .. }
    ));
    let offers = state.offers.lock().unwrap();
    assert_eq!(offers.len(), 2);
    assert!(
        offers[1].0.duration_since(offers[0].0) >= spacing,
        "Preflight time cannot consume logical write spacing"
    );
    drop(offers);
    capture.cancel();
    service.shutdown();
    assert!(service.wait_stopped(Duration::from_secs(2)));
}
