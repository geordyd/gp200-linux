use gp200_domain::*;
use gp200_protocol::{
    F32Bits, RawRecord, RawTransfer, Reassembler, StreamDecoder, StreamEvent, encode_transfer,
};
use gp200_transport::{DeviceActor, TransportAction, TransportConfig};
use std::sync::Arc;
use std::time::Duration;

const CURRENT: &[u8] = include_bytes!("../../gp200-protocol/tests/fixtures/current-baseline.bin");
const STORED: &[u8] = include_bytes!("../../gp200-protocol/tests/fixtures/stored-23-original.bin");
fn raw_capture(bytes: &[u8]) -> RawTransfer {
    let mut decoder = StreamDecoder::new();
    let mut assembler = Reassembler::default();
    for event in decoder.feed(bytes) {
        if let StreamEvent::Packet(packet) = event
            && let Some(raw) = assembler.add(&packet, Duration::ZERO).unwrap()
        {
            return raw;
        }
    }
    panic!("capture did not contain a complete transfer")
}
fn selection() -> DeviceSelection {
    DeviceSelection {
        input_id: "fake-in".into(),
        output_id: "fake-out".into(),
        fingerprint_hint: None,
    }
}
fn stamp(actor: &DeviceActor, intent: u64) -> RequestStamp {
    RequestStamp {
        epoch: actor.epoch(),
        intent: IntentId(intent),
    }
}
fn actions(actor: &mut DeviceActor) -> Vec<TransportAction> {
    std::iter::from_fn(|| actor.next_action()).collect()
}
fn events(actions: Vec<TransportAction>) -> Vec<DeviceEvent> {
    actions
        .into_iter()
        .filter_map(|action| match action {
            TransportAction::Event(event) => Some(event),
            _ => None,
        })
        .collect()
}
fn receive(actor: &mut DeviceActor, raw: &RawTransfer, ms: u64) {
    for packet in encode_transfer(raw).unwrap() {
        actor.receive(actor.epoch(), &packet, Duration::from_millis(ms));
    }
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
fn notification() -> RawTransfer {
    RawTransfer::from_records(
        0x12,
        &[RawRecord {
            command: 5,
            body: vec![1, 3, 2, 0, 0, 0, 0, 0, 0, 0, 0xc6, 0x42],
        }],
    )
    .unwrap()
}
fn ready() -> DeviceActor {
    ready_mode(BackendMode::Hardware)
}
fn ready_mode(mode: BackendMode) -> DeviceActor {
    let mut actor = DeviceActor::with_mode(
        TransportConfig {
            minimum_request_spacing: Duration::ZERO,
            ..TransportConfig::default()
        },
        mode,
    );
    actor.command(
        DeviceCommand::Connect {
            intent: IntentId(1),
            selection: selection(),
        },
        Duration::ZERO,
    );
    assert!(
        actions(&mut actor)
            .iter()
            .any(|action| matches!(action, TransportAction::Open { .. }))
    );
    actor.opened(Ok(()), Duration::ZERO);
    actions(&mut actor);
    actor.sent(Ok(()), Duration::ZERO);
    receive(&mut actor, &identity(), 1);
    assert_eq!(actor.state(), ConnectionState::Synchronizing);
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(2));
    actor.receive(actor.epoch(), CURRENT, Duration::from_millis(3));
    assert_eq!(actor.state(), ConnectionState::Ready);
    assert_eq!(
        actor.snapshot(PresetTarget::EditBuffer).unwrap().revision,
        Revision(1)
    );
    actions(&mut actor);
    actor
}
fn edit(actor: &DeviceActor, intent: u64, value: f32) -> DeviceCommand {
    DeviceCommand::SubmitEdit {
        stamp: stamp(actor, intent),
        target: PresetTarget::EditBuffer,
        base_revision: actor.snapshot(PresetTarget::EditBuffer).unwrap().revision,
        patch: EditPatch::SetParameter {
            module: ModuleId::new(3).unwrap(),
            model: ModelCode(0x0700_0015),
            model_index: ModelIndex(13),
            parameter: ParamId::new(0).unwrap(),
            value: F32Bits(value.to_bits()),
        },
    }
}
fn gain(raw: &RawTransfer, value: f32) -> RawTransfer {
    let mut records = raw.records().to_vec();
    let module = records
        .iter_mut()
        .find(|record| record.command == 0x14 && record.body[0] == 3)
        .unwrap();
    module.body[8..12].copy_from_slice(&value.to_le_bytes());
    RawTransfer::from_records(raw.kind(), &records).unwrap()
}

#[test]
fn unsolicited_notification_does_not_satisfy_identity() {
    let mut actor = DeviceActor::new(TransportConfig::default());
    actor.command(
        DeviceCommand::Connect {
            intent: IntentId(1),
            selection: selection(),
        },
        Duration::ZERO,
    );
    actions(&mut actor);
    actor.opened(Ok(()), Duration::ZERO);
    actions(&mut actor);
    actor.sent(Ok(()), Duration::ZERO);
    receive(&mut actor, &notification(), 1);
    assert_eq!(actor.state(), ConnectionState::Identifying);
    assert!(
        events(actions(&mut actor))
            .iter()
            .any(|event| matches!(event, DeviceEvent::IncomingNotification { .. }))
    );
    receive(&mut actor, &identity(), 2);
    assert_eq!(actor.state(), ConnectionState::Synchronizing);
}

#[test]
fn stored_read_keeps_current_context_and_lossless_raw_separate() {
    let mut actor = ready();
    let current = actor.snapshot(PresetTarget::EditBuffer).unwrap().clone();
    let request = stamp(&actor, 2);
    actor.command(
        DeviceCommand::ReadStored {
            stamp: request,
            slot: SlotId::new(23).unwrap(),
        },
        Duration::from_millis(10),
    );
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(10));
    actor.receive(actor.epoch(), STORED, Duration::from_millis(11));
    let stored = actor
        .snapshot(PresetTarget::Stored(SlotId::new(23).unwrap()))
        .unwrap();
    assert_eq!(stored.context.unwrap().group, 1);
    assert_eq!(current.context.unwrap().group, 0);
    assert_eq!(actor.snapshot(PresetTarget::EditBuffer).unwrap(), &current);
    assert_eq!(stored.raw.payload(), raw_capture(STORED).payload());
}

#[test]
fn wrong_slot_reply_is_notification_then_matching_reply_completes() {
    let mut actor = ready();
    actor.command(
        DeviceCommand::ReadStored {
            stamp: stamp(&actor, 2),
            slot: SlotId::new(22).unwrap(),
        },
        Duration::from_millis(10),
    );
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(10));
    actor.receive(actor.epoch(), STORED, Duration::from_millis(11));
    assert!(actor.is_busy());
    let mut records = raw_capture(STORED).records().to_vec();
    records.iter_mut().find(|r| r.command == 0).unwrap().body[2] = 22;
    receive(
        &mut actor,
        &RawTransfer::from_records(0x12, &records).unwrap(),
        12,
    );
    assert!(!actor.is_busy());
    assert!(
        actor
            .snapshot(PresetTarget::Stored(SlotId::new(22).unwrap()))
            .is_some()
    );
}

#[test]
fn read_timeout_invalidates_session_and_old_callback_cannot_complete_new_session() {
    let mut actor = ready();
    let old_epoch = actor.epoch();
    actor.command(
        DeviceCommand::ReadCurrent(stamp(&actor, 2)),
        Duration::from_millis(10),
    );
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(10));
    actor.tick(Duration::from_secs(2));
    assert_eq!(actor.state(), ConnectionState::Recovering);
    assert!(actor.epoch().0 > old_epoch.0);
    assert!(events(actions(&mut actor)).iter().any(|event| matches!(event, DeviceEvent::OperationFailed { error, .. } if error.kind == ErrorKind::Timeout)));
    actor.receive(old_epoch, CURRENT, Duration::from_secs(3));
    assert!(actor.snapshot(PresetTarget::EditBuffer).is_none());
    assert!(!actor.is_busy());
}

#[test]
fn stale_revision_and_stale_epoch_are_rejected_before_send() {
    let mut actor = ready();
    let mut request = edit(&actor, 2, 51.0);
    if let DeviceCommand::SubmitEdit { base_revision, .. } = &mut request {
        *base_revision = Revision(0);
    }
    actor.command(request, Duration::from_millis(10));
    let result = actions(&mut actor);
    assert!(
        !result
            .iter()
            .any(|action| matches!(action, TransportAction::Send { .. }))
    );
    assert!(events(result).iter().any(|event| matches!(event, DeviceEvent::OperationFailed { error, .. } if error.kind == ErrorKind::Conflict)));
    let stale = RequestStamp {
        epoch: SessionEpoch(0),
        intent: IntentId(3),
    };
    actor.command(DeviceCommand::ReadCurrent(stale), Duration::from_millis(11));
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|action| matches!(action, TransportAction::Send { .. }))
    );
}

#[test]
fn gain_write_uses_index_not_code_and_requires_affected_field_readback() {
    let mut actor = ready();
    let original = actor
        .snapshot(PresetTarget::EditBuffer)
        .unwrap()
        .raw
        .clone();
    actor.command(edit(&actor, 2, 51.0), Duration::from_millis(10));
    let emitted = actions(&mut actor);
    let bytes = emitted
        .iter()
        .find_map(|action| match action {
            TransportAction::Send { packets } => Some(packets.concat()),
            _ => None,
        })
        .unwrap();
    let raw = raw_capture(&bytes);
    let cmd = raw
        .records()
        .iter()
        .find(|record| record.command == 5)
        .unwrap();
    assert_eq!(&cmd.body[4..8], &13_u32.to_le_bytes());
    assert_eq!(&cmd.body[2..4], &[0, 0]);
    actor.sent(Ok(()), Duration::from_millis(10));
    receive(&mut actor, &notification(), 11);
    assert!(actor.is_busy());
    actor.tick(Duration::from_millis(51));
    assert!(
        actions(&mut actor)
            .iter()
            .any(|action| matches!(action, TransportAction::Send { .. }))
    );
    actor.sent(Ok(()), Duration::from_millis(52));
    // Unknown padding changes do not invalidate affected-field verification.
    let changed = gain(&original, 51.0);
    let mut records = changed.records().to_vec();
    if let Some(opaque) = records.iter_mut().find(|record| record.command == 0x0f) {
        opaque.body[2] ^= 0x5a;
    }
    let actual_raw = RawTransfer::from_records(0x12, &records).unwrap();
    receive(&mut actor, &actual_raw, 53);
    assert!(!actor.is_busy());
    let observed = events(actions(&mut actor));
    assert!(observed.iter().any(|event| matches!(
        event,
        DeviceEvent::EditProgress {
            stage: EditStage::Verified,
            ..
        }
    )));
    let actual = observed
        .iter()
        .find_map(|event| match event {
            DeviceEvent::SnapshotReceived {
                stamp, snapshot, ..
            } if stamp.intent == IntentId(2) => Some(snapshot),
            _ => None,
        })
        .expect("Owned exact current readback was emitted");
    assert_eq!(actual.revision, Revision(2));
    assert_eq!(actual.raw.payload(), actual_raw.payload());
    assert_eq!(
        actual
            .view
            .module(ModuleId::new(3).unwrap())
            .unwrap()
            .parameters[0],
        F32Bits(51_f32.to_bits())
    );
    assert!(
        actor.snapshot(PresetTarget::EditBuffer).is_none(),
        "Hint during the owned request still needs a fresh complete read"
    );
    actor.command(
        DeviceCommand::ReadCurrent(stamp(&actor, 3)),
        Duration::from_millis(54),
    );
    assert!(
        actions(&mut actor)
            .iter()
            .any(|action| matches!(action, TransportAction::Send { .. }))
    );
    actor.sent(Ok(()), Duration::from_millis(55));
    receive(&mut actor, &actual_raw, 56);
    actions(&mut actor);
    assert_eq!(
        actor
            .snapshot(PresetTarget::EditBuffer)
            .unwrap()
            .view
            .module(ModuleId::new(3).unwrap())
            .unwrap()
            .parameters[0],
        F32Bits(51_f32.to_bits())
    );
}

#[test]
fn parameter_write_never_mutates_stored_cache() {
    let mut actor = ready();
    actor.command(
        DeviceCommand::ReadStored {
            stamp: stamp(&actor, 2),
            slot: SlotId::new(23).unwrap(),
        },
        Duration::from_millis(10),
    );
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(10));
    actor.receive(actor.epoch(), STORED, Duration::from_millis(11));
    actions(&mut actor);
    let stored = actor
        .snapshot(PresetTarget::Stored(SlotId::new(23).unwrap()))
        .unwrap()
        .clone();
    actor.command(edit(&actor, 3, 51.0), Duration::from_millis(12));
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(13));
    actor.tick(Duration::from_millis(54));
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(55));
    receive(&mut actor, &gain(&raw_capture(CURRENT), 51.0), 56);
    assert_eq!(
        actor
            .snapshot(PresetTarget::Stored(SlotId::new(23).unwrap()))
            .unwrap(),
        &stored
    );
}

#[test]
fn lost_write_is_indeterminate_and_never_replayed() {
    let mut actor = ready();
    actor.command(edit(&actor, 2, 51.0), Duration::from_millis(10));
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(11));
    actor.command(edit(&actor, 3, 52.0), Duration::from_millis(12));
    actions(&mut actor);
    actor.lost(ServiceError::new(ErrorKind::Disconnected, "unplugged"));
    let result = actions(&mut actor);
    assert!(
        !result
            .iter()
            .any(|action| matches!(action, TransportAction::Send { .. }))
    );
    let result = events(result);
    assert!(result.iter().any(|event| matches!(event, DeviceEvent::EditProgress { stamp, stage: EditStage::Indeterminate(_), .. } if stamp.intent == IntentId(2))));
    assert!(result.iter().any(|event| matches!(event, DeviceEvent::OperationFailed { stamp, error, .. } if stamp.intent == IntentId(3) && error.kind == ErrorKind::Cancelled)));
    actor.tick(Duration::from_secs(5));
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|action| matches!(action, TransportAction::Send { .. }))
    );
}

#[test]
fn partial_send_error_is_indeterminate() {
    let mut actor = ready();
    actor.command(edit(&actor, 2, 51.0), Duration::from_millis(10));
    actions(&mut actor);
    actor.sent(
        Err(ServiceError::new(ErrorKind::Io, "partial send")),
        Duration::from_millis(11),
    );
    assert!(events(actions(&mut actor)).iter().any(|event| matches!(
        event,
        DeviceEvent::EditProgress {
            stage: EditStage::Indeterminate(_),
            ..
        }
    )));
}

#[test]
fn repeated_unsent_edits_collapse_to_final_value() {
    let mut actor = ready();
    for (id, value) in [(2, 51.0), (3, 52.0), (4, 53.0)] {
        actor.command(edit(&actor, id, value), Duration::from_millis(10));
    }
    let sends = actions(&mut actor)
        .into_iter()
        .filter_map(|action| match action {
            TransportAction::Send { packets } => Some(packets),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(sends.len(), 1);
    let request = raw_capture(&sends[0].concat());
    assert_eq!(&request.records()[1].body[8..12], &53_f32.to_le_bytes());
}

#[test]
fn in_flight_edits_preserve_final_value_after_confirmed_first_readback() {
    let mut actor = ready();
    actor.command(edit(&actor, 2, 51.0), Duration::from_millis(10));
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(11));
    actions(&mut actor);
    actor.command(edit(&actor, 3, 52.0), Duration::from_millis(12));
    actor.command(edit(&actor, 4, 53.0), Duration::from_millis(13));
    actions(&mut actor);
    actor.tick(Duration::from_millis(52));
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(53));
    receive(&mut actor, &gain(&raw_capture(CURRENT), 51.0), 54);
    let emitted = actions(&mut actor);
    let final_bytes = emitted
        .iter()
        .find_map(|action| match action {
            TransportAction::Send { packets } => Some(packets.concat()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        &raw_capture(&final_bytes).records()[1].body[8..12],
        &53_f32.to_le_bytes()
    );
    actor.sent(Ok(()), Duration::from_millis(55));
    actor.tick(Duration::from_millis(96));
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(97));
    receive(&mut actor, &gain(&raw_capture(CURRENT), 53.0), 98);
    assert!(events(actions(&mut actor)).iter().any(|event| matches!(event, DeviceEvent::EditProgress { stamp, stage: EditStage::Verified } if stamp.intent == IntentId(4))));
}

#[test]
fn notification_interleaved_between_snapshot_packets_does_not_corrupt_read() {
    let mut actor = ready();
    actor.command(
        DeviceCommand::ReadCurrent(stamp(&actor, 2)),
        Duration::from_millis(10),
    );
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(10));
    let packets = encode_transfer(&raw_capture(CURRENT)).unwrap();
    actor.receive(actor.epoch(), &packets[0], Duration::from_millis(11));
    receive(&mut actor, &notification(), 12);
    for packet in &packets[1..] {
        actor.receive(actor.epoch(), packet, Duration::from_millis(13));
    }
    assert!(!actor.is_busy());
    let observed = events(actions(&mut actor));
    let actual = observed
        .iter()
        .find_map(|event| match event {
            DeviceEvent::SnapshotReceived {
                stamp, snapshot, ..
            } if stamp.intent == IntentId(2) => Some(snapshot),
            _ => None,
        })
        .expect("Multipart owned read completed independently of the hint");
    assert_eq!(actual.revision, Revision(2));
    assert_eq!(actual.raw.payload(), raw_capture(CURRENT).payload());
    assert!(actor.snapshot(PresetTarget::EditBuffer).is_none());
    actor.command(
        DeviceCommand::ReadCurrent(stamp(&actor, 3)),
        Duration::from_millis(14),
    );
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(15));
    receive(&mut actor, &raw_capture(CURRENT), 16);
    actions(&mut actor);
    assert_eq!(
        actor.snapshot(PresetTarget::EditBuffer).unwrap().revision,
        Revision(3)
    );
}

#[test]
fn disconnect_cancels_unsent_open_and_write_actions() {
    let mut actor = DeviceActor::new(TransportConfig::default());
    actor.command(
        DeviceCommand::Connect {
            intent: IntentId(1),
            selection: selection(),
        },
        Duration::ZERO,
    );
    actor.command(
        DeviceCommand::Disconnect {
            intent: IntentId(2),
        },
        Duration::ZERO,
    );
    assert!(!actions(&mut actor).iter().any(|action| matches!(
        action,
        TransportAction::Open { .. } | TransportAction::Send { .. }
    )));
    let mut actor = ready();
    actor.command(edit(&actor, 2, 51.0), Duration::from_millis(10));
    actor.command(
        DeviceCommand::Disconnect {
            intent: IntentId(3),
        },
        Duration::from_millis(11),
    );
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|action| matches!(action, TransportAction::Send { .. }))
    );
}

#[test]
fn unsupported_control_and_stored_target_are_not_sent() {
    let mut actor = ready();
    let mut request = edit(&actor, 2, 51.0);
    if let DeviceCommand::SubmitEdit {
        patch: EditPatch::SetParameter { parameter, .. },
        ..
    } = &mut request
    {
        *parameter = ParamId::new(14).unwrap();
    }
    actor.command(request, Duration::from_millis(10));
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|action| matches!(action, TransportAction::Send { .. }))
    );
    let mut request = edit(&actor, 3, 51.0);
    if let DeviceCommand::SubmitEdit { target, .. } = &mut request {
        *target = PresetTarget::Stored(SlotId::new(23).unwrap());
    }
    actor.command(request, Duration::from_millis(11));
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|action| matches!(action, TransportAction::Send { .. }))
    );
}

#[test]
fn simulator_profile_is_not_hardware_permission() {
    let identity = DeviceIdentity {
        model: DeviceModel::Gp200,
        mode: BackendMode::Hardware,
        firmware: None,
        display_name: "GP-200".into(),
    };
    assert!(!Capabilities::simulator().allows(OperationKind::SavePreset, &identity));
    let snapshot = Arc::new(PresetSnapshot::from_raw(
        Arc::new(raw_capture(CURRENT)),
        SnapshotSource::Simulator,
        None,
        Revision(1),
    ));
    assert_eq!(snapshot.raw.payload(), raw_capture(CURRENT).payload());
}

#[test]
fn logical_request_spacing_is_enforced_by_actor_not_cli() {
    let mut actor = DeviceActor::new(TransportConfig {
        minimum_request_spacing: Duration::from_millis(100),
        ..TransportConfig::default()
    });
    actor.command(
        DeviceCommand::Connect {
            intent: IntentId(1),
            selection: selection(),
        },
        Duration::ZERO,
    );
    actions(&mut actor);
    actor.opened(Ok(()), Duration::ZERO);
    actions(&mut actor);
    actor.sent(Ok(()), Duration::ZERO);
    receive(&mut actor, &identity(), 1);
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|action| matches!(action, TransportAction::Send { .. }))
    );
    actor.tick(Duration::from_millis(99));
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|action| matches!(action, TransportAction::Send { .. }))
    );
    actor.tick(Duration::from_millis(100));
    assert!(
        actions(&mut actor)
            .iter()
            .any(|action| matches!(action, TransportAction::Send { .. }))
    );
    actor.sent(Ok(()), Duration::from_millis(100));
    actor.receive(actor.epoch(), CURRENT, Duration::from_millis(101));
    actions(&mut actor);
    actor.command(
        DeviceCommand::ReadStored {
            stamp: stamp(&actor, 2),
            slot: SlotId::new(23).unwrap(),
        },
        Duration::from_millis(102),
    );
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|action| matches!(action, TransportAction::Send { .. }))
    );
    actor.tick(Duration::from_millis(199));
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|action| matches!(action, TransportAction::Send { .. }))
    );
    actor.tick(Duration::from_millis(200));
    assert!(
        actions(&mut actor)
            .iter()
            .any(|action| matches!(action, TransportAction::Send { .. }))
    );
}

fn control_edit(
    actor: &DeviceActor,
    module: u8,
    code: u32,
    index: u32,
    id: u8,
    value: f32,
) -> DeviceCommand {
    DeviceCommand::SubmitEdit {
        stamp: stamp(actor, 20),
        target: PresetTarget::EditBuffer,
        base_revision: actor.snapshot(PresetTarget::EditBuffer).unwrap().revision,
        patch: EditPatch::SetParameter {
            module: ModuleId::new(module).unwrap(),
            model: ModelCode(code),
            model_index: ModelIndex(index),
            parameter: ParamId::new(id).unwrap(),
            value: F32Bits(value.to_bits()),
        },
    }
}
fn sync_edit(actor: &DeviceActor, restore: Option<f32>) -> DeviceCommand {
    DeviceCommand::SubmitEdit {
        stamp: stamp(actor, 21),
        target: PresetTarget::EditBuffer,
        base_revision: actor.snapshot(PresetTarget::EditBuffer).unwrap().revision,
        patch: EditPatch::SetSync {
            module: ModuleId::new(7).unwrap(),
            model: ModelCode(0x04000015),
            model_index: ModelIndex(9),
            power_parameter: ParamId::new(2).unwrap(),
            value_parameter: ParamId::new(1).unwrap(),
            enabled: false,
            ordinary_restore: restore.map(|v| F32Bits(v.to_bits())),
        },
    }
}
fn with_controls(raw: &RawTransfer, module: u8, values: &[(u8, f32)]) -> RawTransfer {
    let mut records = raw.records().to_vec();
    let r = records
        .iter_mut()
        .find(|r| r.command == 0x14 && r.body.len() == 68 && r.body[0] == module)
        .unwrap();
    for (id, value) in values {
        let offset = 8 + usize::from(*id) * 4;
        r.body[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    RawTransfer::from_records(raw.kind(), &records).unwrap()
}
fn decoded_transfers(packets: &[Vec<u8>]) -> Vec<RawTransfer> {
    let mut decoder = StreamDecoder::new();
    let mut assembler = Reassembler::default();
    let mut result = Vec::new();
    for packet in packets {
        for event in decoder.feed(packet) {
            if let StreamEvent::Packet(chunk) = event
                && let Some(raw) = assembler.add(&chunk, Duration::ZERO).unwrap()
            {
                result.push(raw);
            }
        }
    }
    result
}

#[test]
fn sync_pair_sends_power_then_exact_ordinary_value_and_requires_full_final_readback() {
    let mut actor = ready();
    let baseline = actor
        .snapshot(PresetTarget::EditBuffer)
        .unwrap()
        .raw
        .clone();
    actor.command(sync_edit(&actor, Some(0.7)), Duration::from_millis(10));
    let packets = actions(&mut actor)
        .into_iter()
        .find_map(|a| match a {
            TransportAction::Send { packets } => Some(packets),
            _ => None,
        })
        .unwrap();
    let wire = decoded_transfers(&packets);
    assert_eq!(wire.len(), 2);
    let independent_power = raw_capture(include_bytes!(
        "../../../tests/fixtures/control-adapters/mod-sync-paired-step-02-request.bin"
    ));
    let independent_value = raw_capture(include_bytes!(
        "../../../tests/fixtures/control-adapters/mod-sync-paired-step-03-request.bin"
    ));
    assert_eq!(wire[0].payload(), independent_power.payload());
    assert_eq!(
        &wire[1].records()[1].body[..8],
        &independent_value.records()[1].body[..8]
    );
    for (raw, id, value) in [(&wire[0], 2, 0.0f32), (&wire[1], 1, 0.7f32)] {
        assert_eq!(raw.records().len(), 2);
        assert_eq!(raw.records()[0].body, [0, 0, 23, 0]);
        let r = &raw.records()[1];
        assert_eq!(r.command, 5);
        assert_eq!(&r.body[..4], &[7, id, 0, 0]);
        assert_eq!(&r.body[4..8], &9u32.to_le_bytes());
        assert_eq!(&r.body[8..12], &value.to_le_bytes());
    }
    actor.sent(Ok(()), Duration::from_millis(10));
    // A paired intermediate device response cannot prematurely verify the pair.
    receive(
        &mut actor,
        &with_controls(&baseline, 7, &[(2, 0.0), (1, 0.5)]),
        11,
    );
    assert!(actor.is_busy());
    assert!(!events(actions(&mut actor)).iter().any(|e| matches!(
        e,
        DeviceEvent::EditProgress {
            stage: EditStage::Verified,
            ..
        }
    )));
    actor.tick(Duration::from_millis(51));
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(52));
    receive(
        &mut actor,
        &with_controls(&baseline, 7, &[(2, 0.0), (1, 0.7)]),
        53,
    );
    assert!(!actor.is_busy());
    assert!(events(actions(&mut actor)).iter().any(|e| matches!(
        e,
        DeviceEvent::EditProgress {
            stage: EditStage::Verified,
            ..
        }
    )));
}

#[test]
fn second_ordered_sync_send_failure_is_indeterminate_and_cannot_replay() {
    let mut actor = ready();
    actor.command(sync_edit(&actor, Some(0.7)), Duration::from_millis(10));
    let packets = actions(&mut actor)
        .into_iter()
        .find_map(|a| match a {
            TransportAction::Send { packets } => Some(packets),
            _ => None,
        })
        .unwrap();
    assert_eq!(decoded_transfers(&packets).len(), 2);
    // The worker sends the first transfer successfully, then the second send
    // fails. Its aggregate result carries no guarantee of rollback.
    actor.sent(
        Err(ServiceError::new(
            ErrorKind::Disconnected,
            "Injected second ordered transfer send failure",
        )),
        Duration::from_millis(11),
    );
    let reported = events(actions(&mut actor));
    assert!(reported.iter().any(|e| matches!(
        e,
        DeviceEvent::EditProgress {
            stage: EditStage::Indeterminate(_),
            ..
        }
    )));
    assert_eq!(actor.state(), ConnectionState::Recovering);
    actor.tick(Duration::from_secs(20));
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|a| matches!(a, TransportAction::Send { .. }))
    );
}

#[test]
fn sync_requires_explicit_value_correct_pair_and_rejects_isolated_power() {
    for bad in 0..4 {
        let mut actor = ready();
        let mut request = sync_edit(&actor, None);
        if let DeviceCommand::SubmitEdit {
            patch:
                EditPatch::SetSync {
                    value_parameter,
                    model_index,
                    ordinary_restore,
                    ..
                },
            ..
        } = &mut request
        {
            match bad {
                1 => *value_parameter = ParamId::new(0).unwrap(),
                2 => *model_index = ModelIndex(0x04000015),
                3 => *ordinary_restore = Some(F32Bits(500.0f32.to_bits())),
                _ => (),
            }
        }
        actor.command(request, Duration::from_millis(10));
        assert!(
            !actions(&mut actor)
                .iter()
                .any(|a| matches!(a, TransportAction::Send { .. }))
        );
    }
    let mut actor = ready();
    actor.command(
        control_edit(&actor, 7, 0x04000015, 9, 2, 0.0),
        Duration::from_millis(10),
    );
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|a| matches!(a, TransportAction::Send { .. }))
    );
}

#[test]
fn special_controls_require_exact_live_profile_and_mode_appropriate_values() {
    for (module, code, index, id, value) in [
        (5, 0x0a000010, 25, 5, 19.0),
        (5, 0x0a000010, 25, 6, 20000.0),
        (1, 0x0500000a, 2, 4, 0.0),
        (8, 0x0b00000b, 12, 4, 1.0),
        (7, 0x04000015, 9, 1, 5.0),
    ] {
        let mut actor = ready();
        actor.command(
            control_edit(&actor, module, code, index, id, value),
            Duration::from_millis(10),
        );
        assert!(
            actions(&mut actor)
                .iter()
                .any(|a| matches!(a, TransportAction::Send { .. }))
        );
    }
    for (module, code, index, id, value) in
        [(7, 0x04000015, 9, 1, 500.0), (8, 0x0b00000b, 12, 4, 2.0)]
    {
        let mut actor = ready();
        actor.command(
            control_edit(&actor, module, code, index, id, value),
            Duration::from_millis(10),
        );
        assert!(
            !actions(&mut actor)
                .iter()
                .any(|a| matches!(a, TransportAction::Send { .. }))
        );
    }
    let catalog = gp200_catalog::Catalog::embedded().unwrap();
    assert!(!gp200_transport::hardware_control_verified(
        5,
        0x0a000000,
        catalog
            .model("CAB", 0x0a000000)
            .unwrap()
            .parameter(6)
            .unwrap()
    ));
    assert!(!gp200_transport::hardware_control_verified(
        0,
        0x01000001,
        catalog
            .model("PRE", 0x01000001)
            .unwrap()
            .parameter(3)
            .unwrap()
    ));
}
