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

fn submit(actor: &DeviceActor, edit: AssignmentEdit) -> DeviceCommand {
    DeviceCommand::SubmitEdit {
        stamp: stamp(actor, 9),
        target: PresetTarget::EditBuffer,
        base_revision: actor.snapshot(PresetTarget::EditBuffer).unwrap().revision,
        patch: EditPatch::SetPatchAssignment(edit),
    }
}
#[test]
fn only_proved_assignment_families_have_hardware_write_paths() {
    for edit in [
        AssignmentEdit::CtrlState {
            slot: 7,
            enabled: true,
        },
        AssignmentEdit::ExpressionRange {
            slot: ExpressionSlotId::new(1, 0).unwrap(),
            minimum: 0.0.into(),
            maximum: 101.0.into(),
        },
    ] {
        let mut actor = ready();
        actor.command(submit(&actor, edit), Duration::from_millis(10));
        let output = actions(&mut actor);
        assert!(
            !output
                .iter()
                .any(|a| matches!(a, TransportAction::Send { .. }))
        );
        assert!(
            events(output)
                .iter()
                .any(|event| matches!(event, DeviceEvent::OperationFailed { .. }))
        );
    }
}
#[test]
fn ctrl_membership_full_record_preserves_retained_state_padding_and_high_bits() {
    let mut actor = ready();
    let before = actor.snapshot(PresetTarget::EditBuffer).unwrap().clone();
    let original = before
        .view
        .patch_assignments
        .ctrls
        .iter()
        .find(|entry| entry.record.slot == 7)
        .unwrap()
        .record
        .to_record();
    actor.command(
        submit(
            &actor,
            AssignmentEdit::CtrlModules {
                slot: 7,
                modules: vec![ModuleId::new(10).unwrap()],
            },
        ),
        Duration::from_millis(10),
    );
    let emitted = actions(&mut actor);
    let packet = emitted
        .iter()
        .find_map(|a| match a {
            TransportAction::Send { packets } => Some(packets.concat()),
            _ => None,
        })
        .unwrap();
    let request = raw_capture(&packet);
    let record = &request.records()[1];
    assert_eq!(record.command, 15);
    assert_eq!(&record.body[..4], &original.body[..4]);
    assert_eq!(
        u32::from_le_bytes(record.body[4..8].try_into().unwrap()),
        1024
    );
}
#[test]
fn quick_parameter_has_current_model_dependency_and_wire_declared_parameter_id() {
    let mut actor = ready();
    let module = actor
        .snapshot(PresetTarget::EditBuffer)
        .unwrap()
        .view
        .module(ModuleId::new(0).unwrap())
        .unwrap()
        .clone();
    let target = ParameterAssignmentTarget {
        module: module.module,
        expected_model: module.model,
        parameter: ParamId::new(1).unwrap(),
    };
    actor.command(
        submit(
            &actor,
            AssignmentEdit::QuickTarget {
                index: 2,
                target: QuickAssignmentTarget::Parameter(target.clone()),
            },
        ),
        Duration::from_millis(10),
    );
    let packet = actions(&mut actor)
        .into_iter()
        .find_map(|a| match a {
            TransportAction::Send { packets } => Some(packets.concat()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        raw_capture(&packet).records()[1],
        RawRecord {
            command: 16,
            body: vec![2, 0, 1, 0]
        }
    );
    let mut actor = ready();
    let mut target = target;
    target.expected_model.0 ^= 1;
    actor.command(
        submit(
            &actor,
            AssignmentEdit::QuickTarget {
                index: 2,
                target: QuickAssignmentTarget::Parameter(target),
            },
        ),
        Duration::from_millis(10),
    );
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|a| matches!(a, TransportAction::Send { .. }))
    );
}
#[test]
fn fx_boundaries_emit_retained_chain_and_only_ordered_in_range_positions() {
    let mut actor = ready();
    let before = actor
        .snapshot(PresetTarget::EditBuffer)
        .unwrap()
        .raw
        .records()
        .iter()
        .find(|r| r.command == 8)
        .unwrap()
        .clone();
    actor.command(
        submit(
            &actor,
            AssignmentEdit::FxLoopPositions {
                send_boundary: 3,
                return_boundary: 4,
            },
        ),
        Duration::from_millis(10),
    );
    let packet = actions(&mut actor)
        .into_iter()
        .find_map(|a| match a {
            TransportAction::Send { packets } => Some(packets.concat()),
            _ => None,
        })
        .unwrap();
    let mut expected = before;
    expected.body[2] = 3;
    assert_eq!(raw_capture(&packet).records()[1], expected);
    let mut actor = ready();
    actor.command(
        submit(
            &actor,
            AssignmentEdit::FxLoopPositions {
                send_boundary: 5,
                return_boundary: 4,
            },
        ),
        Duration::from_millis(10),
    );
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|a| matches!(a, TransportAction::Send { .. }))
    );
}
#[test]
fn simulator_range_emits_native_endpoint_without_rewriting_target_or_minimum() {
    let mut actor = ready_mode(BackendMode::Simulator);
    actor.command(
        submit(
            &actor,
            AssignmentEdit::ExpressionRange {
                slot: ExpressionSlotId::new(1, 0).unwrap(),
                minimum: 0.0.into(),
                maximum: 99.0.into(),
            },
        ),
        Duration::from_millis(10),
    );
    let packet = actions(&mut actor)
        .into_iter()
        .find_map(|a| match a {
            TransportAction::Send { packets } => Some(packets.concat()),
            _ => None,
        })
        .unwrap();
    let records = raw_capture(&packet).records().to_vec();
    assert_eq!(records.len(), 2);
    assert_eq!(
        records[1],
        RawRecord {
            command: 14,
            body: vec![1, 16, 0, 0, 0, 0, 0xc6, 0x42]
        }
    );
}

#[test]
fn proved_quick_special_targets_use_zero_parameter_and_preserve_index() {
    for (target, module_raw) in [
        (QuickAssignmentTarget::PatchVolume, 11),
        (QuickAssignmentTarget::PatchTempo, 12),
        (QuickAssignmentTarget::Off, 255),
    ] {
        let mut actor = ready();
        actor.command(
            submit(&actor, AssignmentEdit::QuickTarget { index: 0, target }),
            Duration::from_millis(10),
        );
        let packet = actions(&mut actor)
            .into_iter()
            .find_map(|a| match a {
                TransportAction::Send { packets } => Some(packets.concat()),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            raw_capture(&packet).records()[1],
            RawRecord {
                command: 16,
                body: vec![0, module_raw, 0, 0]
            }
        );
    }
}

#[test]
fn hardware_pwah_maximum_uses_endpoint_and_readback_instead_of_full_record() {
    let mut actor = ready();
    let snapshot = actor.snapshot(PresetTarget::EditBuffer).unwrap().clone();
    let edit = AssignmentEdit::ExpressionRange {
        slot: ExpressionSlotId::new(1, 0).unwrap(),
        minimum: 0.0.into(),
        maximum: 99.0.into(),
    };
    actor.command(submit(&actor, edit.clone()), Duration::from_millis(10));
    let packet = actions(&mut actor)
        .into_iter()
        .find_map(|a| match a {
            TransportAction::Send { packets } => Some(packets.concat()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        raw_capture(&packet).records()[1],
        RawRecord {
            command: 14,
            body: vec![1, 16, 0, 0, 0, 0, 0xc6, 0x42]
        }
    );
    actor.sent(Ok(()), Duration::from_millis(10));
    actor.tick(Duration::from_millis(51));
    assert!(
        actions(&mut actor)
            .iter()
            .any(|a| matches!(a, TransportAction::Send { .. }))
    );
    actor.sent(Ok(()), Duration::from_millis(52));
    let mut draft = PresetDraft::from_snapshot(&snapshot);
    draft.apply(&EditPatch::SetPatchAssignment(edit)).unwrap();
    let expected = draft
        .snapshot(&snapshot, SnapshotSource::Simulator, Revision(2))
        .unwrap();
    receive(&mut actor, &expected.raw, 53);
    assert!(!actor.is_busy());
    assert!(events(actions(&mut actor)).iter().any(|event| matches!(
        event,
        DeviceEvent::EditProgress {
            stage: EditStage::Verified,
            ..
        }
    )));
}

#[test]
fn hardware_expression_clear_uses_native_ff_parameter_but_verifies_normalized_zero() {
    let mut actor = ready();
    let before = actor.snapshot(PresetTarget::EditBuffer).unwrap().clone();
    let edit = AssignmentEdit::ExpressionTarget {
        slot: ExpressionSlotId::new(1, 0).unwrap(),
        target: ExpressionAssignmentTarget::Off,
    };
    actor.command(submit(&actor, edit.clone()), Duration::from_millis(10));
    let packet = actions(&mut actor)
        .into_iter()
        .find_map(|a| match a {
            TransportAction::Send { packets } => Some(packets.concat()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        raw_capture(&packet).records()[1].body,
        [16, 255, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    actor.sent(Ok(()), Duration::from_millis(10));
    actor.tick(Duration::from_millis(51));
    assert!(
        actions(&mut actor)
            .iter()
            .any(|a| matches!(a, TransportAction::Send { .. }))
    );
    actor.sent(Ok(()), Duration::from_millis(52));
    let mut draft = PresetDraft::from_snapshot(&before);
    draft.apply(&EditPatch::SetPatchAssignment(edit)).unwrap();
    let normalized = draft
        .snapshot(&before, SnapshotSource::Simulator, Revision(2))
        .unwrap();
    receive(&mut actor, &normalized.raw, 53);
    assert!(!actor.is_busy());
    assert!(events(actions(&mut actor)).iter().any(|e| matches!(
        e,
        DeviceEvent::EditProgress {
            stage: EditStage::Verified,
            ..
        }
    )));
    let module = before.view.module(ModuleId::new(1).unwrap()).unwrap();
    let restore = AssignmentEdit::ExpressionTargetAndRange {
        slot: ExpressionSlotId::new(1, 0).unwrap(),
        target: ExpressionAssignmentTarget::Parameter(ParameterAssignmentTarget {
            module: module.module,
            expected_model: module.model,
            parameter: ParamId::new(3).unwrap(),
        }),
        minimum: 0.0.into(),
        maximum: 100.0.into(),
    };
    actor.command(submit(&actor, restore), Duration::from_millis(100));
    let packet = actions(&mut actor)
        .into_iter()
        .find_map(|a| match a {
            TransportAction::Send { packets } => Some(packets.concat()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        raw_capture(&packet).records()[1].body,
        before.view.patch_assignments.expressions[3]
            .record
            .to_record()
            .body
    );
    actor.sent(Ok(()), Duration::from_millis(100));
    actor.tick(Duration::from_millis(141));
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(142));
    receive(&mut actor, &before.raw, 143);
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
fn widget_assignment_gate_rejects_unknown_ctrl_state_and_foreign_fx_context() {
    let before = ready().snapshot(PresetTarget::EditBuffer).unwrap().clone();
    let mut records = before.raw.records().to_vec();
    let ctrl_index = before.view.patch_assignments.ctrls[7].raw_record_index;
    records[ctrl_index].body[1] = 201;
    let snapshot = PresetSnapshot::from_raw(
        std::sync::Arc::new(RawTransfer::from_records(0x11, &records).unwrap()),
        before.source.clone(),
        before.target,
        before.revision,
    );
    assert!(
        gp200_transport::hardware_assignment_blocked_reason(
            &snapshot,
            &AssignmentEdit::CtrlModules {
                slot: 7,
                modules: vec![ModuleId::new(10).unwrap()],
            }
        )
        .is_some()
    );
    let routing_index = before
        .view
        .patch_assignments
        .routing
        .as_ref()
        .unwrap()
        .raw_record_index;
    records[routing_index].body[..2].copy_from_slice(&255u16.to_le_bytes());
    let snapshot = PresetSnapshot::from_raw(
        std::sync::Arc::new(RawTransfer::from_records(0x11, &records).unwrap()),
        before.source.clone(),
        before.target,
        before.revision,
    );
    assert!(
        gp200_transport::hardware_assignment_blocked_reason(
            &snapshot,
            &AssignmentEdit::FxLoopPositions {
                send_boundary: 3,
                return_boundary: 4,
            }
        )
        .is_some()
    );
}
