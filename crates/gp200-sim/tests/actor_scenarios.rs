use gp200_domain::*;
use gp200_sim::*;
use gp200_transport::{DeviceActor, TransportAction, TransportConfig, capabilities_for_mode};
use std::time::Duration;

struct Harness {
    actor: DeviceActor,
    pedal: SimulatedPedal,
    now: Duration,
    events: Vec<DeviceEvent>,
    scheduled: Vec<Delivery>,
    sends: usize,
}

#[test]
fn asset_metadata_reads_use_real_codecs_keep_partial_banks_and_never_claim_hardware() {
    let mut harness = Harness::new();
    harness.connect();
    let current = harness.pedal.current().clone();
    let stored = harness.pedal.stored(23).unwrap().clone();
    for bank in [AssetBank::Ir, AssetBank::SnapTone] {
        for index in 0..bank.slots() {
            let stamp =
                harness.stamp(100 + u64::from(index) + if bank == AssetBank::Ir { 0 } else { 20 });
            let slot = AssetSlot::new(bank, index).unwrap();
            harness.command(DeviceCommand::ReadAssetMetadata { stamp, slot });
            assert!(harness.events.iter().any(|event| matches!(event, DeviceEvent::AssetMetadataReceived { stamp: actual, metadata } if *actual == stamp && metadata.slot() == slot && metadata.source() == &SnapshotSource::Simulator)));
            assert_eq!(harness.pedal.current(), &current);
            assert_eq!(harness.pedal.stored(23).unwrap(), &stored);
        }
    }
    let physical = DeviceIdentity {
        model: DeviceModel::Gp200,
        mode: BackendMode::Hardware,
        firmware: None,
        display_name: "GP-200".into(),
    };
    assert!(
        !capabilities_for_mode(BackendMode::Simulator).allows(OperationKind::IrRead, &physical)
    );
    assert!(
        !capabilities_for_mode(BackendMode::Simulator)
            .allows(OperationKind::SnapToneRead, &physical)
    );
}
#[test]
fn silent_asset_query_times_out_once_without_replay_or_asset_write() {
    let mut harness = Harness::new();
    harness.connect();
    harness
        .pedal
        .queue_fault(ReplyFault {
            silent: true,
            ..Default::default()
        })
        .unwrap();
    let stamp = harness.stamp(2);
    let before = harness.sends;
    harness.command(DeviceCommand::ReadAssetMetadata {
        stamp,
        slot: AssetSlot::new(AssetBank::Ir, 0).unwrap(),
    });
    harness.advance(101);
    assert!(harness.events.iter().any(|event| matches!(event, DeviceEvent::OperationFailed { stamp: actual, error } if *actual == stamp && error.kind == ErrorKind::Timeout)));
    harness.advance(1000);
    assert_eq!(harness.sends, before + 1);
    assert!(
        !harness
            .events
            .iter()
            .any(|event| matches!(event, DeviceEvent::AssetMetadataReceived { .. }))
    );
}
#[test]
fn fragmented_asset_metadata_with_realtime_bytes_keeps_all_opaque_tail_bytes() {
    let mut harness = Harness::new();
    harness.connect();
    harness.pedal.profile = ReplyProfile {
        decoded_chunk_size: 7,
        callback_chunk_size: 1,
        realtime_byte: Some(0xfa),
        ..Default::default()
    };
    let stamp = harness.stamp(2);
    harness.command(DeviceCommand::ReadAssetMetadata {
        stamp,
        slot: AssetSlot::new(AssetBank::Ir, 19).unwrap(),
    });
    assert!(harness.events.iter().any(|event| matches!(event, DeviceEvent::AssetMetadataReceived { metadata, .. } if metadata.metadata().name_raw[8..] == [0x3c,0x54,0x3b,0x80,0xff,0xff,0xff,0xff])));
}
impl Harness {
    fn new() -> Self {
        let config = TransportConfig {
            minimum_request_spacing: Duration::ZERO,
            write_settle: Duration::from_millis(5),
            request_timeout: Duration::from_millis(100),
            reassembly_timeout: Duration::from_millis(80),
            ..Default::default()
        };
        Self {
            actor: DeviceActor::with_mode(config, BackendMode::Simulator),
            pedal: SimulatedPedal::captured(),
            now: Duration::ZERO,
            events: Vec::new(),
            scheduled: Vec::new(),
            sends: 0,
        }
    }
    fn connect(&mut self) {
        self.command(DeviceCommand::Connect {
            intent: IntentId(1),
            selection: simulator_port().selection,
        });
        assert_eq!(self.actor.state(), ConnectionState::Ready);
    }
    fn stamp(&self, intent: u64) -> RequestStamp {
        RequestStamp {
            epoch: self.actor.epoch(),
            intent: IntentId(intent),
        }
    }
    fn command(&mut self, command: DeviceCommand) {
        self.actor.command(command, self.now);
        self.pump();
    }
    fn advance(&mut self, milliseconds: u64) {
        self.now += Duration::from_millis(milliseconds);
        self.actor.tick(self.now);
        self.pump();
    }
    fn pump(&mut self) {
        for _ in 0..1024 {
            if let Some(action) = self.actor.next_action() {
                match action {
                    TransportAction::Enumerate => self.actor.enumerated(Ok(vec![simulator_port()])),
                    TransportAction::Open { .. } => {
                        self.pedal.reset_stream();
                        self.actor.opened(Ok(()), self.now);
                    }
                    TransportAction::Close => self.pedal.reset_stream(),
                    TransportAction::Send { packets } => {
                        self.sends += 1;
                        let result = packets.iter().try_for_each(|packet| {
                            self.pedal
                                .receive(packet, self.now)
                                .map(|deliveries| self.scheduled.extend(deliveries))
                        });
                        self.actor.sent(result, self.now);
                    }
                    TransportAction::CaptureRead { .. } => {
                        panic!(
                            "Ordinary actor scenario unexpectedly reserved a firmware capture read"
                        )
                    }
                    TransportAction::Event(event) => self.events.push(event),
                }
                continue;
            }
            self.scheduled.sort_by_key(Delivery::at);
            if self
                .scheduled
                .first()
                .is_some_and(|delivery| delivery.at() <= self.now)
            {
                let delivery = self.scheduled.remove(0);
                match delivery {
                    Delivery::Bytes { bytes, .. } => {
                        self.actor.receive(self.actor.epoch(), &bytes, self.now)
                    }
                    Delivery::Disconnected { .. } => self.actor.lost(ServiceError::new(
                        ErrorKind::Disconnected,
                        "Scripted simulator unplug",
                    )),
                }
                continue;
            }
            return;
        }
        panic!("Deterministic harness exceeded its action bound");
    }
}
#[test]
fn production_actor_reports_simulator_identity_source_and_separate_profiles() {
    let mut harness = Harness::new();
    harness.connect();
    let mut saw_identity = false;
    let mut saw_snapshot = false;
    for event in &harness.events {
        match event {
            DeviceEvent::SessionChanged {
                identity,
                capabilities,
                ..
            } => {
                saw_identity = true;
                assert_eq!(identity.mode, BackendMode::Simulator);
                let physical = DeviceIdentity {
                    mode: BackendMode::Hardware,
                    ..identity.clone()
                };
                assert!(!capabilities.allows(OperationKind::SetParameter, &physical));
                assert!(capabilities.allows(OperationKind::SetParameter, identity));
            }
            DeviceEvent::SnapshotReceived { snapshot, .. } => {
                saw_snapshot = true;
                assert_eq!(snapshot.source, SnapshotSource::Simulator);
            }
            _ => {}
        }
    }
    assert!(saw_identity && saw_snapshot);
    let replay = DeviceIdentity {
        model: DeviceModel::Gp200,
        mode: BackendMode::CaptureReplay,
        firmware: None,
        display_name: "Replay".into(),
    };
    let capabilities = capabilities_for_mode(BackendMode::CaptureReplay);
    assert!(capabilities.allows(OperationKind::ReadEditBuffer, &replay));
    for mutation in [
        OperationKind::SetParameter,
        OperationKind::SavePreset,
        OperationKind::PresetImport,
    ] {
        assert!(!capabilities.allows(mutation, &replay));
    }
}
#[test]
fn multipart_with_notification_and_duplicate_chunks_reaches_ready_once() {
    let mut harness = Harness::new();
    harness.pedal.queue_fault(ReplyFault::default()).unwrap(); // identity
    harness
        .pedal
        .queue_fault(ReplyFault {
            duplicate_packet: Some(2),
            unsolicited_after_packet: Some(3),
            ..Default::default()
        })
        .unwrap();
    harness.connect();
    assert_eq!(
        harness
            .events
            .iter()
            .filter(|event| matches!(
                event,
                DeviceEvent::SnapshotReceived {
                    target: PresetTarget::EditBuffer,
                    ..
                }
            ))
            .count(),
        1
    );
    assert!(
        harness
            .events
            .iter()
            .any(|event| matches!(event, DeviceEvent::IncomingNotification { .. }))
    );
}
#[test]
fn a_second_read_during_a_partial_reply_is_busy_and_cannot_merge_selectors() {
    let mut harness = Harness::new();
    harness.connect();
    harness.events.clear();
    harness
        .pedal
        .queue_fault(ReplyFault {
            drop_packet: Some(2),
            ..Default::default()
        })
        .unwrap();
    harness.command(DeviceCommand::ReadStored {
        stamp: harness.stamp(2),
        slot: SlotId::new(23).unwrap(),
    });
    assert!(harness.actor.is_busy());
    let sent = harness.sends;
    harness.command(DeviceCommand::ReadStored {
        stamp: harness.stamp(3),
        slot: SlotId::new(24).unwrap(),
    });
    assert_eq!(harness.sends, sent);
    assert!(harness.events.iter().any(|event| matches!(event, DeviceEvent::OperationFailed { stamp, error } if stamp.intent == IntentId(3) && error.kind == ErrorKind::Busy)));
    harness.advance(101);
    assert!(!harness.events.iter().any(|event| matches!(
        event,
        DeviceEvent::SnapshotReceived {
            target: PresetTarget::Stored(_),
            ..
        }
    )));
    assert_ne!(harness.actor.state(), ConnectionState::Ready);
}
#[test]
fn timeout_reconnect_rejects_old_epoch_and_never_replays_a_sent_edit() {
    let mut harness = Harness::new();
    harness.connect();
    harness.events.clear();
    let snapshot = harness
        .actor
        .snapshot(PresetTarget::EditBuffer)
        .unwrap()
        .clone();
    let original_epoch = harness.actor.epoch();
    harness
        .pedal
        .queue_fault(ReplyFault {
            silent: true,
            ..Default::default()
        })
        .unwrap();
    harness.command(DeviceCommand::SubmitEdit {
        stamp: harness.stamp(2),
        target: PresetTarget::EditBuffer,
        base_revision: snapshot.revision,
        patch: EditPatch::SetParameter {
            module: ModuleId::new(3).unwrap(),
            model: ModelCode(0x0700_0015),
            model_index: ModelIndex(13),
            parameter: ParamId::new(0).unwrap(),
            value: F32Bits::from_f32(51.0),
        },
    });
    harness.advance(6);
    harness.advance(101);
    assert!(harness.events.iter().any(|event| matches!(event, DeviceEvent::EditProgress { stamp, stage: EditStage::Indeterminate(_) } if stamp.intent == IntentId(2))));
    let writes_before = harness
        .pedal
        .log()
        .exchanges
        .iter()
        .filter(|exchange| {
            exchange
                .request
                .records()
                .iter()
                .any(|record| record.command == 5)
        })
        .count();
    harness.connect();
    assert_ne!(harness.actor.epoch(), original_epoch);
    let late = gp200_protocol::encode_transfer(snapshot.raw.as_ref()).unwrap();
    for packet in late {
        harness.actor.receive(original_epoch, &packet, harness.now);
    }
    harness.pump();
    let confirmed = harness.actor.snapshot(PresetTarget::EditBuffer).unwrap();
    assert_eq!(
        confirmed
            .view
            .module(ModuleId::new(3).unwrap())
            .unwrap()
            .parameter(ParamId::new(0).unwrap()),
        Some(F32Bits::from_f32(51.0))
    );
    assert_eq!(
        harness
            .pedal
            .log()
            .exchanges
            .iter()
            .filter(|exchange| exchange
                .request
                .records()
                .iter()
                .any(|record| record.command == 5))
            .count(),
        writes_before
    );
}
#[test]
fn incorrect_context_and_corrupt_reply_never_satisfy_stored_read() {
    for fault in [
        ReplyFault {
            wrong_context: Some((1, 24)),
            ..Default::default()
        },
        ReplyFault {
            corrupt_packet: Some(2),
            ..Default::default()
        },
        ReplyFault {
            conflicting_duplicate: Some(2),
            ..Default::default()
        },
    ] {
        let mut harness = Harness::new();
        harness.connect();
        harness.events.clear();
        harness.pedal.queue_fault(fault).unwrap();
        harness.command(DeviceCommand::ReadStored {
            stamp: harness.stamp(2),
            slot: SlotId::new(23).unwrap(),
        });
        harness.advance(101);
        assert!(!harness.events.iter().any(|event| matches!(event, DeviceEvent::SnapshotReceived { stamp, .. } if stamp.intent == IntentId(2))));
        assert!(harness.events.iter().any(|event| matches!(event, DeviceEvent::OperationFailed { stamp, .. } if stamp.intent == IntentId(2))));
    }
}

#[test]
fn simulated_model_defaults_and_save_as_pass_actual_actor_readback() {
    let mut harness = Harness::new();
    harness.connect();
    let baseline = harness
        .actor
        .snapshot(PresetTarget::EditBuffer)
        .unwrap()
        .clone();
    let catalog = gp200_catalog::Catalog::embedded().unwrap();
    let model = catalog
        .module_by_id(3)
        .unwrap()
        .models
        .iter()
        .find(|model| {
            model.code
                != baseline
                    .view
                    .module(ModuleId::new(3).unwrap())
                    .unwrap()
                    .model
                    .0
        })
        .unwrap();
    harness.command(DeviceCommand::SubmitEdit {
        stamp: harness.stamp(2),
        target: PresetTarget::EditBuffer,
        base_revision: baseline.revision,
        patch: EditPatch::SetModel {
            module: ModuleId::new(3).unwrap(),
            model: ModelCode(model.code),
        },
    });
    harness.advance(6);
    assert!(harness.events.iter().any(|event| matches!(event, DeviceEvent::EditProgress { stamp, stage:EditStage::Verified } if stamp.intent == IntentId(2))));
    let current = harness
        .actor
        .snapshot(PresetTarget::EditBuffer)
        .unwrap()
        .clone();
    let amp = current.view.module(ModuleId::new(3).unwrap()).unwrap();
    assert_eq!(amp.model, ModelCode(model.code));
    let defaults = model.default_values();
    for (id, bits) in amp.parameters.iter().enumerate() {
        assert_eq!(
            *bits,
            defaults
                .get(&(id as u8))
                .copied()
                .map(F32Bits::from_f32)
                .unwrap_or(
                    baseline
                        .view
                        .module(ModuleId::new(3).unwrap())
                        .unwrap()
                        .parameters[id]
                )
        );
    }
    harness.command(DeviceCommand::ReadStored {
        stamp: harness.stamp(30),
        slot: SlotId::new(255).unwrap(),
    });
    harness.command(DeviceCommand::SaveBufferTo {
        stamp: harness.stamp(3),
        slot: SlotId::new(255).unwrap(),
        expected_buffer_revision: current.revision,
    });
    harness.advance(6);
    assert!(harness.events.iter().any(|event| matches!(event, DeviceEvent::EditProgress { stamp, stage:EditStage::Verified } if stamp.intent == IntentId(3))));
    let current = harness.actor.snapshot(PresetTarget::EditBuffer).unwrap();
    assert_eq!(
        current.context,
        Some(WireContext {
            group: 0,
            selector: 255
        })
    );
    assert!(
        harness
            .actor
            .snapshot(PresetTarget::Stored(SlotId::new(255).unwrap()))
            .is_some()
    );
}
