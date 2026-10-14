use gp200_domain::*;
use gp200_protocol::{
    RawRecord, RawTransfer, Reassembler, StreamDecoder, StreamEvent, encode_transfer,
};
use gp200_transport::{DeviceActor, TransportAction, TransportConfig};
use std::time::Duration;

const CURRENT: &[u8] = include_bytes!("../../gp200-protocol/tests/fixtures/current-baseline.bin");
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

fn submit(actor: &DeviceActor, metadata: MetadataPatch) -> DeviceCommand {
    DeviceCommand::SubmitEdit {
        stamp: stamp(actor, 9),
        target: PresetTarget::EditBuffer,
        base_revision: actor.snapshot(PresetTarget::EditBuffer).unwrap().revision,
        patch: EditPatch::SetMetadata(metadata),
    }
}
fn sent_raw(output: &[TransportAction]) -> RawTransfer {
    let packets = output
        .iter()
        .find_map(|a| match a {
            TransportAction::Send { packets } => Some(packets.concat()),
            _ => None,
        })
        .expect("send");
    raw_capture(&packets)
}
#[test]
fn pan_and_style_have_exact_selectors_after_fresh_complete_preflight() {
    for (patch, body) in [
        (
            MetadataPatch {
                native_pan: Some(PatchPan::new(1).unwrap()),
                ..Default::default()
            },
            vec![6, 0, 1, 0],
        ),
        (
            MetadataPatch {
                native_style: Some(NativeStyleId::new(0).unwrap()),
                ..Default::default()
            },
            vec![2, 0, 0, 0],
        ),
    ] {
        let mut actor = ready();
        let before = actor.snapshot(PresetTarget::EditBuffer).unwrap().clone();
        actor.command(submit(&actor, patch), Duration::from_millis(10));
        assert_eq!(
            sent_raw(&actions(&mut actor)),
            gp200_protocol::read_current_request()
        );
        actor.sent(Ok(()), Duration::from_millis(11));
        receive(&mut actor, &before.raw, 12);
        let request = sent_raw(&actions(&mut actor));
        assert_eq!(request.records()[0].body, [0, 0, 23, 0]);
        assert_eq!(request.records()[1], RawRecord { command: 6, body });
        assert_eq!(request.records().len(), 2);
    }
}
#[test]
fn hardware_outside_proven_pairs_and_fx_local_annotations_refuse_before_any_tx() {
    for patch in [
        MetadataPatch {
            native_pan: Some(PatchPan::new(-1).unwrap()),
            ..Default::default()
        },
        MetadataPatch {
            native_style: Some(NativeStyleId::new(19).unwrap()),
            ..Default::default()
        },
        MetadataPatch {
            fx_send_level: Some(1),
            ..Default::default()
        },
        MetadataPatch {
            fx_loop_mode: Some(PatchFxLoopMode::new(1).unwrap()),
            ..Default::default()
        },
        MetadataPatch {
            notes: Some("Local".into()),
            ..Default::default()
        },
        MetadataPatch {
            native_pan: Some(PatchPan::from_raw(127)),
            ..Default::default()
        },
    ] {
        let mut actor = ready();
        actor.command(submit(&actor, patch), Duration::from_millis(10));
        let output = actions(&mut actor);
        assert!(
            !output
                .iter()
                .any(|action| matches!(action, TransportAction::Send { .. }))
        );
        assert!(
            events(output)
                .iter()
                .any(|event| matches!(event, DeviceEvent::OperationFailed { .. }))
        );
    }
}
#[test]
fn changed_pan_style_context_or_unrelated_field_preflight_never_sends_mutation() {
    for (command, offset, value) in [(2, 7, 2), (2, 8, 4), (0, 2, 24), (2, 4, 51)] {
        let mut actor = ready();
        let before = actor.snapshot(PresetTarget::EditBuffer).unwrap().clone();
        actor.command(
            submit(
                &actor,
                MetadataPatch {
                    native_pan: Some(PatchPan::new(1).unwrap()),
                    ..Default::default()
                },
            ),
            Duration::from_millis(10),
        );
        actions(&mut actor);
        actor.sent(Ok(()), Duration::from_millis(11));
        let mut records = before.raw.records().to_vec();
        records
            .iter_mut()
            .find(|r| r.command == command)
            .unwrap()
            .body[offset] = value;
        let actual = RawTransfer::from_records(before.raw.kind(), &records).unwrap();
        receive(&mut actor, &actual, 12);
        let output = actions(&mut actor);
        assert!(
            !output
                .iter()
                .any(|action| matches!(action, TransportAction::Send { .. }))
        );
        assert!(events(output).iter().any(|event|matches!(event,DeviceEvent::OperationFailed{error,..} if error.kind==ErrorKind::Conflict)));
    }
}
#[test]
fn stale_revision_epoch_and_stored_target_cannot_own_a_metadata_write() {
    for mode in 0..3 {
        let mut actor = ready();
        let mut command = submit(
            &actor,
            MetadataPatch {
                native_style: Some(NativeStyleId::new(0).unwrap()),
                ..Default::default()
            },
        );
        if let DeviceCommand::SubmitEdit {
            stamp,
            target,
            base_revision,
            ..
        } = &mut command
        {
            match mode {
                0 => base_revision.0 += 1,
                1 => stamp.epoch.0 += 1,
                _ => *target = PresetTarget::Stored(SlotId::new(255).unwrap()),
            }
        }
        actor.command(command, Duration::from_millis(10));
        assert!(
            !actions(&mut actor)
                .iter()
                .any(|action| matches!(action, TransportAction::Send { .. }))
        );
    }
}
#[test]
fn simulator_full_native_boundaries_use_exact_signed_pan_and_fx_bytes() {
    let mut actor = ready_mode(BackendMode::Simulator);
    actor.command(
        submit(
            &actor,
            MetadataPatch {
                native_pan: Some(PatchPan::new(-100).unwrap()),
                native_style: Some(NativeStyleId::new(19).unwrap()),
                fx_send_level: Some(100),
                fx_return_level: Some(99),
                fx_loop_mode: Some(PatchFxLoopMode::new(1).unwrap()),
                ..Default::default()
            },
        ),
        Duration::from_millis(10),
    );
    let request = sent_raw(&actions(&mut actor));
    let bodies: Vec<_> = request.records()[1..]
        .iter()
        .map(|r| r.body.clone())
        .collect();
    assert_eq!(
        bodies,
        vec![
            vec![6, 0, 156, 255],
            vec![2, 0, 19, 0],
            vec![3, 0, 100, 0],
            vec![4, 0, 99, 0],
            vec![5, 0, 1, 0]
        ]
    );
}
