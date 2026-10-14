use gp200_domain::*;
use gp200_protocol::{
    RawRecord, RawTransfer, Reassembler, StreamDecoder, StreamEvent, decode_packet, encode_transfer,
};
use gp200_transport::{
    MidiDeviceService, MidiSession, ReceiveSink, TransportBackend, TransportConfig,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

fn current() -> RawTransfer {
    let bytes = include_bytes!("../../gp200-protocol/tests/fixtures/current-baseline.bin");
    let mut assembler = Reassembler::default();
    for event in StreamDecoder::new().feed(bytes) {
        if let StreamEvent::Packet(packet) = event
            && let Some(raw) = assembler.add(&packet, Duration::ZERO).unwrap()
        {
            return raw;
        }
    }
    unreachable!()
}
fn candidate() -> PortPairCandidate {
    PortPairCandidate {
        selection: DeviceSelection {
            input_id: "fake-in".into(),
            output_id: "fake-out".into(),
            fingerprint_hint: None,
        },
        display_name: "Fake GP-200".into(),
        mode_hint: Some(BackendMode::Hardware),
        matching_confidence: "fixture".into(),
    }
}
struct State {
    present: AtomicBool,
    respond: AtomicBool,
    enumerations: AtomicUsize,
    writes: AtomicUsize,
    dropped_sessions: AtomicUsize,
    current: Mutex<RawTransfer>,
}
impl State {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            present: AtomicBool::new(true),
            respond: AtomicBool::new(true),
            enumerations: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            dropped_sessions: AtomicUsize::new(0),
            current: Mutex::new(current()),
        })
    }
}
struct FakeBackend(Arc<State>);
impl TransportBackend for FakeBackend {
    fn enumerate(&mut self) -> Result<Vec<PortPairCandidate>, ServiceError> {
        self.0.enumerations.fetch_add(1, Ordering::Relaxed);
        Ok(if self.0.present.load(Ordering::Acquire) {
            vec![candidate()]
        } else {
            vec![]
        })
    }
    fn connect(
        &mut self,
        _: &DeviceSelection,
        sink: ReceiveSink,
    ) -> Result<Box<dyn MidiSession>, ServiceError> {
        if !self.0.present.load(Ordering::Acquire) {
            return Err(ServiceError::new(ErrorKind::Disconnected, "unplugged"));
        }
        Ok(Box::new(FakeSession {
            state: self.0.clone(),
            sink,
        }))
    }
}
struct FakeSession {
    state: Arc<State>,
    sink: ReceiveSink,
}
impl Drop for FakeSession {
    fn drop(&mut self) {
        self.state.dropped_sessions.fetch_add(1, Ordering::Relaxed);
    }
}
impl MidiSession for FakeSession {
    fn send(&mut self, bytes: &[u8]) -> Result<(), ServiceError> {
        let packet = decode_packet(bytes).unwrap();
        let raw = RawTransfer::from_payload(packet.kind, packet.payload).unwrap();
        if let Some(edit) = raw.records().iter().find(|r| r.command == 5) {
            self.state.writes.fetch_add(1, Ordering::Relaxed);
            let mut current = self.state.current.lock().unwrap();
            let mut records = current.records().to_vec();
            let module = records
                .iter_mut()
                .find(|r| r.command == 0x14 && r.body[0] == edit.body[0])
                .unwrap();
            let offset = 8 + usize::from(edit.body[1]) * 4;
            module.body[offset..offset + 4].copy_from_slice(&edit.body[8..12]);
            *current = RawTransfer::from_records(0x12, &records).unwrap();
        } else if self.state.respond.load(Ordering::Acquire) {
            let response = if raw.records().iter().any(|r| r.command == 0x2001) {
                // The notification precedes the requested response, as captured.
                let notification = RawTransfer::from_records(
                    0x12,
                    &[RawRecord {
                        command: 5,
                        body: vec![1, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0xc6, 0x42],
                    }],
                )
                .unwrap();
                for packet in encode_transfer(&notification).unwrap() {
                    self.sink.receive(&packet);
                }
                RawTransfer::from_records(
                    0x12,
                    &[RawRecord {
                        command: 0x2001,
                        body: vec![1, 0, 2, 0],
                    }],
                )
                .unwrap()
            } else {
                let context = gp200_protocol::WireContext::from_transfer(&raw).unwrap();
                if context.group == 0 {
                    self.state.current.lock().unwrap().clone()
                } else {
                    // Stored data is immutable in this fake; parameter writes
                    // have no path into the bank, as the hardware proof shows.
                    let mut records = current().records().to_vec();
                    records.iter_mut().find(|r| r.command == 0).unwrap().body =
                        context.to_record().body;
                    RawTransfer::from_records(0x12, &records).unwrap()
                }
            };
            for packet in encode_transfer(&response).unwrap() {
                // Callback splits are not packet boundaries.
                self.sink.receive(&packet[..3]);
                self.sink.receive(&packet[3..]);
            }
        }
        Ok(())
    }
}
fn service(state: Arc<State>) -> MidiDeviceService {
    MidiDeviceService::with_backend(
        Box::new(FakeBackend(state)),
        TransportConfig {
            request_timeout: Duration::from_millis(300),
            discovery_interval: Duration::from_millis(100),
            write_settle: Duration::from_millis(5),
            ..TransportConfig::default()
        },
        no_wake(),
    )
}
fn wait(service: &MidiDeviceService, predicate: impl Fn(&DeviceEvent) -> bool) -> DeviceEvent {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Some(event) = service.try_next_event() {
            if predicate(&event) {
                return event;
            }
        } else {
            thread::sleep(Duration::from_millis(2));
        }
    }
    panic!("event deadline")
}
fn connect(service: &MidiDeviceService) -> SessionEpoch {
    service
        .try_submit(DeviceCommand::Connect {
            intent: IntentId(1),
            selection: candidate().selection,
        })
        .unwrap();
    let DeviceEvent::ConnectionChanged { epoch, .. } = wait(service, |event| {
        matches!(
            event,
            DeviceEvent::ConnectionChanged {
                state: ConnectionState::Ready,
                ..
            }
        )
    }) else {
        unreachable!()
    };
    epoch
}

#[test]
fn worker_handles_callback_splits_and_unsolicited_identity_notification() {
    let state = State::new();
    let service = service(state.clone());
    service
        .try_submit(DeviceCommand::Connect {
            intent: IntentId(1),
            selection: candidate().selection,
        })
        .unwrap();
    wait(&service, |event| {
        matches!(event, DeviceEvent::IncomingNotification { .. })
    });
    let DeviceEvent::SnapshotReceived { snapshot, .. } = wait(&service, |event| {
        matches!(event, DeviceEvent::SnapshotReceived { .. })
    }) else {
        unreachable!()
    };
    assert_eq!(snapshot.raw.payload(), current().payload());
    assert_eq!(state.writes.load(Ordering::Acquire), 0);
    service.shutdown();
    assert!(service.wait_stopped(Duration::from_secs(1)));
    assert_eq!(state.dropped_sessions.load(Ordering::Acquire), 1);
}

#[test]
fn hotplug_during_write_is_indeterminate_then_reconnect_reads_without_replay() {
    let state = State::new();
    let service = service(state.clone());
    let epoch = connect(&service);
    // This fake injects a known change hint before identity. Ready alone does
    // not retire that hint's freshness obligation for a direct service client;
    // the normal AppController supplies the corresponding recovery query.
    let fresh_stamp = RequestStamp {
        epoch,
        intent: IntentId(10),
    };
    service
        .try_submit(DeviceCommand::ReadCurrent(fresh_stamp))
        .unwrap();
    let DeviceEvent::SnapshotReceived {
        snapshot: fresh, ..
    } = wait(
        &service,
        |event| matches!(event, DeviceEvent::SnapshotReceived { stamp, target: PresetTarget::EditBuffer, .. } if *stamp == fresh_stamp),
    )
    else {
        unreachable!();
    };
    assert_eq!(fresh.revision, Revision(2));
    assert_eq!(fresh.raw.payload(), current().payload());
    assert_eq!(state.writes.load(Ordering::Acquire), 0);
    state.respond.store(false, Ordering::Release);
    service
        .try_submit(DeviceCommand::SubmitEdit {
            stamp: RequestStamp {
                epoch,
                intent: IntentId(2),
            },
            target: PresetTarget::EditBuffer,
            base_revision: fresh.revision,
            patch: EditPatch::SetParameter {
                module: ModuleId::new(3).unwrap(),
                model: ModelCode(0x0700_0015),
                model_index: ModelIndex(13),
                parameter: ParamId::new(0).unwrap(),
                value: 51.0.into(),
            },
        })
        .unwrap();
    wait(&service, |event| {
        matches!(
            event,
            DeviceEvent::EditProgress {
                stage: EditStage::Sent,
                ..
            }
        )
    });
    state.present.store(false, Ordering::Release);
    wait(&service, |event| {
        matches!(
            event,
            DeviceEvent::EditProgress {
                stage: EditStage::Indeterminate(_),
                ..
            }
        )
    });
    let DeviceEvent::ConnectionChanged {
        epoch: recovered, ..
    } = wait(&service, |event| {
        matches!(
            event,
            DeviceEvent::ConnectionChanged {
                state: ConnectionState::Recovering,
                ..
            }
        )
    })
    else {
        unreachable!()
    };
    assert!(recovered.0 > epoch.0);
    state.present.store(true, Ordering::Release);
    state.respond.store(true, Ordering::Release);
    let new_epoch = connect(&service);
    assert!(new_epoch.0 > recovered.0);
    assert_eq!(state.writes.load(Ordering::Acquire), 1);
    service
        .try_submit(DeviceCommand::ReadStored {
            stamp: RequestStamp {
                epoch: new_epoch,
                intent: IntentId(3),
            },
            slot: SlotId::new(23).unwrap(),
        })
        .unwrap();
    let DeviceEvent::SnapshotReceived { snapshot, .. } = wait(&service, |event| {
        matches!(
            event,
            DeviceEvent::SnapshotReceived {
                target: PresetTarget::Stored(_),
                ..
            }
        )
    }) else {
        unreachable!()
    };
    assert_eq!(
        snapshot
            .view
            .module(ModuleId::new(3).unwrap())
            .unwrap()
            .parameters[0],
        50.0.into()
    );
    service.shutdown();
    assert!(service.wait_stopped(Duration::from_secs(1)));
}

#[test]
fn bounded_submission_reports_busy_and_shutdown_remains_nonblocking_under_backpressure() {
    let state = State::new();
    let service = MidiDeviceService::with_backend(
        Box::new(FakeBackend(state.clone())),
        TransportConfig {
            command_capacity: 1,
            event_capacity: 1,
            ..TransportConfig::default()
        },
        no_wake(),
    );
    service.try_submit(DeviceCommand::Discover).unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while state.enumerations.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(2));
    }
    assert!(state.enumerations.load(Ordering::Acquire) > 0);
    service.try_submit(DeviceCommand::Discover).unwrap();
    assert!(matches!(
        service.try_submit(DeviceCommand::Discover),
        Err(SubmitError::Busy(_))
    ));
    let before = Instant::now();
    service.shutdown();
    assert!(before.elapsed() < Duration::from_millis(20));
    assert!(service.wait_stopped(Duration::from_secs(1)));
}

#[test]
fn stale_epoch_is_rejected_by_actual_service_without_wire_mutation() {
    let state = State::new();
    let service = service(state.clone());
    let epoch = connect(&service);
    service
        .try_submit(DeviceCommand::ReadCurrent(RequestStamp {
            epoch: SessionEpoch(epoch.0 - 1),
            intent: IntentId(9),
        }))
        .unwrap();
    let DeviceEvent::OperationFailed { error, .. } = wait(
        &service,
        |event| matches!(event, DeviceEvent::OperationFailed { stamp, .. } if stamp.intent == IntentId(9)),
    ) else {
        unreachable!()
    };
    assert_eq!(error.kind, ErrorKind::Conflict);
    assert_eq!(state.writes.load(Ordering::Acquire), 0);
    service.shutdown();
    assert!(service.wait_stopped(Duration::from_secs(1)));
}
#[test]
fn descriptive_firmware_request_returns_the_moved_store_without_hardware_offer() {
    use gp200_library::*;
    let root = std::env::temp_dir().join(format!("gp200-fw03-public-{}", std::process::id()));
    let store = LibraryStore::open(&root).unwrap();
    let bytes = include_bytes!("../../../tests/fixtures/device-assets/synthetic-firmware.bin");
    let context = FirmwareUpdateContextDto {
        profile_id: "public description".into(),
        hardware_policy: "unknown".into(),
        update_mode: "mock claim".into(),
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
        bytes: Arc::from(&b"description"[..]),
    })
    .collect();
    let job = Arc::new(
        ReviewedFirmwareUpdate::prepare(
            RetainedJobBlob::checked(
                AssetId("image".into()),
                bytes.as_slice().into(),
                JobSha256::of(bytes),
            )
            .unwrap(),
            context,
            docs,
            8,
        )
        .unwrap(),
    );
    let state = State::new();
    let service = service(state.clone());
    let ticket = service
        .try_start_firmware(gp200_transport::FirmwareOwnershipRequest::new(job, store))
        .ok()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let event = loop {
        if let Some(e) = service.try_next_firmware_event() {
            break e;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(2));
    };
    let gp200_transport::FirmwareOwnerEvent::Rejected { error, store } = event else {
        panic!("physical issuance must reject")
    };
    assert_eq!(error.kind, ErrorKind::Unverified);
    assert_eq!(state.writes.load(Ordering::SeqCst), 0);
    assert_eq!(state.dropped_sessions.load(Ordering::SeqCst), 0);
    assert!(LibraryStore::open(&root).is_err());
    drop(store);
    drop(ticket);
    service.shutdown();
    assert!(service.wait_stopped(Duration::from_secs(1)));
    std::fs::remove_dir_all(root).unwrap();
}
