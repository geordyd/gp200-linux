// Private capture acceptance suite; excluded from public source builds.
// Run with RUSTFLAGS="--cfg gp200_private_evidence" only in the retained lab checkout.
#![cfg(gp200_private_evidence)]

//! Synthetic notification injection through the production byte actor; no MIDI.
use gp200_domain::*;
use gp200_protocol::encode_transfer;
use gp200_transport::{DeviceActor, TransportAction, TransportConfig};
use std::{sync::Arc, time::Duration};

const CURRENT: &[u8] = include_bytes!("../../../assets/demo/read-current-payload.bin");
const GLOBALS: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/own-fresh-globals-before.payload.bin"
);
const DEP_BEFORE: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/own-fresh-current-before.payload.bin"
);
const DEP_AFTER: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/after-forward.payload.bin"
);
const GLOBALS_AFTER: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/after-forward-globals.payload.bin"
);
fn raw(payload: &[u8]) -> RawTransfer {
    RawTransfer::from_payload(0x12, payload.to_vec()).unwrap()
}
fn hint(command: u16, body: Vec<u8>) -> RawTransfer {
    RawTransfer::from_records(0x12, &[RawRecord { command, body }]).unwrap()
}
fn actions(actor: &mut DeviceActor) -> Vec<TransportAction> {
    std::iter::from_fn(|| actor.next_action()).collect()
}
fn receive(actor: &mut DeviceActor, raw: &RawTransfer, ms: u64) {
    for packet in encode_transfer(raw).unwrap() {
        actor.receive(actor.epoch(), &packet, Duration::from_millis(ms));
    }
}
fn stamp(actor: &DeviceActor, intent: u64) -> RequestStamp {
    RequestStamp {
        epoch: actor.epoch(),
        intent: IntentId(intent),
    }
}
fn sends(actions: &[TransportAction]) -> usize {
    actions
        .iter()
        .filter(|action| matches!(action, TransportAction::Send { .. }))
        .count()
}
fn ready(payload: &[u8], globals: bool) -> DeviceActor {
    let mut actor = DeviceActor::with_mode(
        TransportConfig {
            minimum_request_spacing: Duration::ZERO,
            ..Default::default()
        },
        BackendMode::Hardware,
    );
    actor.command(
        DeviceCommand::Connect {
            intent: IntentId(1),
            selection: DeviceSelection {
                input_id: "synthetic-in".into(),
                output_id: "synthetic-out".into(),
                fingerprint_hint: None,
            },
        },
        Duration::ZERO,
    );
    actions(&mut actor);
    actor.opened(Ok(()), Duration::ZERO);
    actions(&mut actor);
    actor.sent(Ok(()), Duration::ZERO);
    receive(&mut actor, &hint(0x2001, vec![1, 0, 2, 0]), 1);
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(2));
    receive(&mut actor, &raw(payload), 3);
    actions(&mut actor);
    assert_eq!(actor.state(), ConnectionState::Ready);
    if globals {
        actor.command(
            DeviceCommand::ReadGlobals(stamp(&actor, 2)),
            Duration::from_millis(4),
        );
        assert_eq!(sends(&actions(&mut actor)), 1);
        actor.sent(Ok(()), Duration::from_millis(5));
        receive(&mut actor, &raw(GLOBALS), 6);
        actions(&mut actor);
        assert!(actor.global_state().is_some());
    }
    actor
}

#[test]
fn idle_known_hints_invalidate_cache_without_echo_or_fabricated_request_stamp() {
    let mut actor = ready(CURRENT, false);
    let raw = hint(0x15, vec![3, 1, 0, 0]);
    receive(&mut actor, &raw, 10);
    let output = actions(&mut actor);
    assert_eq!(sends(&output), 0);
    assert!(actor.snapshot(PresetTarget::EditBuffer).is_none());
    assert!(output.iter().any(|action| matches!(action, TransportAction::Event(DeviceEvent::IncomingNotification { epoch, raw: observed }) if *epoch == actor.epoch() && observed.payload() == raw.payload())));
    assert!(!output.iter().any(|action| matches!(
        action,
        TransportAction::Event(
            DeviceEvent::SnapshotReceived { .. } | DeviceEvent::EditProgress { .. }
        )
    )));
    // Only an explicit owned complete query restores write authority.
    let read = stamp(&actor, 3);
    actor.command(DeviceCommand::ReadCurrent(read), Duration::from_millis(11));
    assert_eq!(sends(&actions(&mut actor)), 1);
    actor.sent(Ok(()), Duration::from_millis(12));
    receive(&mut actor, &self::raw(CURRENT), 13);
    assert!(actions(&mut actor).iter().any(|action| matches!(action, TransportAction::Event(DeviceEvent::SnapshotReceived { stamp, .. }) if *stamp == read)));
    assert!(actor.snapshot(PresetTarget::EditBuffer).is_some());
}

#[test]
fn malformed_unknown_and_special_kind_hints_preserve_cache_and_raw_diagnostic() {
    let mut unsupported = vec![
        hint(0x4242, vec![1, 2, 3]),
        hint(5, vec![3, 0]),
        hint(5, vec![11, 0, 0, 0, 13, 0, 0, 0, 0, 0, 0, 0]),
        hint(5, vec![3, 15, 0, 0, 13, 0, 0, 0, 0, 0, 0, 0]),
    ];
    for (command, length, selector) in [
        (2, 88, 256u16),
        (8, 16, 256),
        (7, 20, 256),
        (0x2003, 20, 256),
        (0x2009, 4, 256),
        (0x000a, 20, 20),
    ] {
        let mut body = vec![0; length];
        body[..2].copy_from_slice(&selector.to_le_bytes());
        unsupported.push(hint(command, body));
    }
    unsupported.push(
        RawTransfer::from_records(
            3,
            &[RawRecord {
                command: 0x15,
                body: vec![3, 1, 0, 0],
            }],
        )
        .unwrap(),
    );
    for raw in unsupported {
        let mut actor = ready(CURRENT, true);
        let baseline = actor.snapshot(PresetTarget::EditBuffer).unwrap().clone();
        let global = actor.global_state().unwrap().clone();
        receive(&mut actor, &raw, 10);
        let output = actions(&mut actor);
        assert_eq!(sends(&output), 0);
        assert!(Arc::ptr_eq(
            actor.snapshot(PresetTarget::EditBuffer).unwrap(),
            &baseline
        ));
        assert!(Arc::ptr_eq(actor.global_state().unwrap(), &global));
        assert!(output.iter().any(|action| matches!(action, TransportAction::Event(DeviceEvent::IncomingNotification { raw: observed, .. }) if observed.payload() == raw.payload())));
    }
}

#[test]
fn complete_global_and_style_notification_shapes_only_invalidate_globals() {
    for raw in [hint(0x1010, vec![0; 4]), hint(0x000a, vec![0; 20])] {
        let mut actor = ready(CURRENT, true);
        let baseline = actor.snapshot(PresetTarget::EditBuffer).unwrap().clone();
        receive(&mut actor, &raw, 10);
        assert_eq!(sends(&actions(&mut actor)), 0);
        assert!(actor.global_state().is_none());
        assert!(Arc::ptr_eq(
            actor.snapshot(PresetTarget::EditBuffer).unwrap(),
            &baseline
        ));
    }
}

#[test]
fn notification_between_multipart_read_packets_does_not_become_the_owned_reply() {
    let mut actor = ready(CURRENT, false);
    let baseline = actor.snapshot(PresetTarget::EditBuffer).unwrap().clone();
    let read = stamp(&actor, 3);
    actor.command(DeviceCommand::ReadCurrent(read), Duration::from_millis(10));
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(11));
    let packets = encode_transfer(&raw(CURRENT)).unwrap();
    assert!(packets.len() > 1);
    actor.receive(actor.epoch(), &packets[0], Duration::from_millis(12));
    receive(&mut actor, &hint(0x15, vec![3, 1, 0, 0]), 13);
    let output = actions(&mut actor);
    assert_eq!(sends(&output), 0);
    assert!(Arc::ptr_eq(
        actor.snapshot(PresetTarget::EditBuffer).unwrap(),
        &baseline
    ));
    assert!(actor.is_busy());
    assert!(!output.iter().any(|action| matches!(
        action,
        TransportAction::Event(DeviceEvent::SnapshotReceived { .. })
    )));
    for packet in &packets[1..] {
        actor.receive(actor.epoch(), packet, Duration::from_millis(14));
    }
    let output = actions(&mut actor);
    assert!(output.iter().any(|action| matches!(action, TransportAction::Event(DeviceEvent::SnapshotReceived { stamp, .. }) if *stamp == read)));
    assert!(!actor.is_busy());
    actor.tick(Duration::from_millis(15));
    assert_eq!(sends(&actions(&mut actor)), 0);
    assert!(
        actor.snapshot(PresetTarget::EditBuffer).is_none(),
        "Hint remains an invalidation obligation after owned completion"
    );
}

#[test]
fn dependent_readback_retains_owned_baselines_until_verified_then_invalidates_without_echo() {
    let mut actor = ready(DEP_BEFORE, true);
    let edit = stamp(&actor, 3);
    actor.command(
        DeviceCommand::SubmitDependentModel {
            stamp: edit,
            base_revision: actor.snapshot(PresetTarget::EditBuffer).unwrap().revision,
            module: ModuleId::new(3).unwrap(),
            model: ModelCode(0x07000001),
            before_globals: actor.global_state().unwrap().clone(),
            expected_auto_cab: true,
        },
        Duration::from_millis(10),
    );
    assert_eq!(sends(&actions(&mut actor)), 1);
    actor.sent(Ok(()), Duration::from_millis(11));
    receive(&mut actor, &raw(GLOBALS), 12);
    assert_eq!(sends(&actions(&mut actor)), 1);
    actor.sent(Ok(()), Duration::from_millis(13));
    receive(&mut actor, &raw(DEP_BEFORE), 14);
    assert_eq!(sends(&actions(&mut actor)), 1);
    actor.sent(Ok(()), Duration::from_millis(15));
    actions(&mut actor);
    receive(&mut actor, &hint(0x15, vec![3, 1, 0, 0]), 16);
    assert_eq!(sends(&actions(&mut actor)), 0);
    assert!(actor.snapshot(PresetTarget::EditBuffer).is_some());
    assert!(actor.global_state().is_some());
    actor.tick(Duration::from_millis(100));
    assert_eq!(sends(&actions(&mut actor)), 1);
    actor.sent(Ok(()), Duration::from_millis(101));
    receive(&mut actor, &raw(DEP_AFTER), 102);
    assert_eq!(sends(&actions(&mut actor)), 1);
    actor.sent(Ok(()), Duration::from_millis(103));
    receive(&mut actor, &raw(GLOBALS_AFTER), 104);
    let output = actions(&mut actor);
    assert_eq!(sends(&output), 0);
    assert!(output.iter().any(|action| matches!(action, TransportAction::Event(DeviceEvent::EditProgress { stamp, stage: EditStage::Verified }) if *stamp == edit)));
    actor.tick(Duration::from_millis(105));
    assert_eq!(sends(&actions(&mut actor)), 0);
    assert!(actor.snapshot(PresetTarget::EditBuffer).is_none());
}

#[test]
fn epoch_loss_drops_partial_refresh_and_never_replays_mutations_or_late_input() {
    let mut actor = ready(CURRENT, false);
    let old_epoch = actor.epoch();
    actor.command(
        DeviceCommand::ReadCurrent(stamp(&actor, 3)),
        Duration::from_millis(10),
    );
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(11));
    let packets = encode_transfer(&raw(CURRENT)).unwrap();
    actor.receive(old_epoch, &packets[0], Duration::from_millis(12));
    actor.lost(ServiceError::new(
        ErrorKind::Disconnected,
        "Synthetic unplug",
    ));
    let output = actions(&mut actor);
    assert_eq!(sends(&output), 0);
    for packet in &packets[1..] {
        actor.receive(old_epoch, packet, Duration::from_millis(13));
    }
    receive(&mut actor, &hint(0x15, vec![3, 1, 0, 0]), 14);
    actor.tick(Duration::from_secs(5));
    assert_eq!(sends(&actions(&mut actor)), 0);
    assert!(actor.snapshot(PresetTarget::EditBuffer).is_none());
}

fn parameter(actor: &DeviceActor, intent: u64, value: f32) -> DeviceCommand {
    DeviceCommand::SubmitEdit {
        stamp: stamp(actor, intent),
        target: PresetTarget::EditBuffer,
        base_revision: actor.snapshot(PresetTarget::EditBuffer).unwrap().revision,
        patch: EditPatch::SetParameter {
            module: ModuleId::new(3).unwrap(),
            model: ModelCode(0x07000015),
            model_index: ModelIndex(13),
            parameter: ParamId::new(0).unwrap(),
            value: F32Bits::from_f32(value),
        },
    }
}

#[test]
fn hint_cancels_actor_unsent_control_while_owned_edit_still_completes_exact_readback() {
    for hint in [hint(0x15, vec![3, 1, 0, 0]), hint(0x1010, vec![0; 4])] {
        let mut actor = ready(CURRENT, true);
        actor.command(parameter(&actor, 3, 51.0), Duration::from_millis(10));
        assert_eq!(sends(&actions(&mut actor)), 1);
        actor.sent(Ok(()), Duration::from_millis(11));
        actions(&mut actor);
        let queued = stamp(&actor, 4);
        actor.command(parameter(&actor, 4, 52.0), Duration::from_millis(12));
        assert_eq!(sends(&actions(&mut actor)), 0);
        receive(&mut actor, &hint, 13);
        let output = actions(&mut actor);
        assert_eq!(sends(&output), 0);
        assert!(output.iter().any(|action| matches!(action, TransportAction::Event(DeviceEvent::EditProgress { stamp, stage: EditStage::Failed(error) }) if *stamp == queued && error.kind == ErrorKind::Cancelled)));
        actor.tick(Duration::from_millis(100));
        assert_eq!(sends(&actions(&mut actor)), 1);
        actor.sent(Ok(()), Duration::from_millis(101));
        let mut records = raw(CURRENT).records().to_vec();
        records
            .iter_mut()
            .find(|record| record.command == 0x14 && record.body[0] == 3)
            .unwrap()
            .body[8..12]
            .copy_from_slice(&51f32.to_le_bytes());
        receive(
            &mut actor,
            &RawTransfer::from_records(0x12, &records).unwrap(),
            102,
        );
        let output = actions(&mut actor);
        assert_eq!(sends(&output), 0);
        assert!(output.iter().any(|action| matches!(action, TransportAction::Event(DeviceEvent::EditProgress { stamp, stage: EditStage::Verified }) if stamp.intent == IntentId(3))));
        actor.tick(Duration::from_millis(1000));
        assert_eq!(sends(&actions(&mut actor)), 0);
    }
}

#[test]
fn fresh_query_after_hint_invalidated_completion_restores_its_own_authority() {
    let mut actor = ready(CURRENT, false);
    actor.command(
        DeviceCommand::ReadCurrent(stamp(&actor, 3)),
        Duration::from_millis(10),
    );
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(11));
    receive(&mut actor, &hint(0x15, vec![3, 1, 0, 0]), 12);
    actions(&mut actor);
    receive(&mut actor, &raw(CURRENT), 13);
    actions(&mut actor);
    assert!(!actor.is_busy());
    assert!(actor.snapshot(PresetTarget::EditBuffer).is_none());
    // receive() invokes its completion Tick. The next explicit query began
    // after that invalidation and owns a fresh, independently bound reply.
    actor.command(
        DeviceCommand::ReadCurrent(stamp(&actor, 4)),
        Duration::from_millis(14),
    );
    assert_eq!(sends(&actions(&mut actor)), 1);
    actor.sent(Ok(()), Duration::from_millis(15));
    receive(&mut actor, &raw(CURRENT), 16);
    actions(&mut actor);
    actor.tick(Duration::from_millis(17));
    assert_eq!(sends(&actions(&mut actor)), 0);
    assert!(actor.snapshot(PresetTarget::EditBuffer).is_some());
}
