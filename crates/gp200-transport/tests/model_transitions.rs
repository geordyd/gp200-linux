use gp200_domain::*;
use gp200_protocol::{RawTransfer, encode_transfer};
use gp200_transport::{
    DeviceActor, TransportAction, TransportConfig, hardware_model_blocked_reason,
};
use std::{sync::Arc, time::Duration};

#[path = "../../../tests/fixtures/model-breadth/cases.rs"]
mod breadth;

fn raw(payload: &[u8]) -> RawTransfer {
    RawTransfer::from_payload(0x12, payload.to_vec()).unwrap()
}
fn actions(actor: &mut DeviceActor) -> Vec<TransportAction> {
    std::iter::from_fn(|| actor.next_action()).collect()
}
fn receive(actor: &mut DeviceActor, transfer: &RawTransfer, ms: u64) {
    for bytes in encode_transfer(transfer).unwrap() {
        actor.receive(actor.epoch(), &bytes, Duration::from_millis(ms));
    }
}
fn ready(payload: &[u8]) -> DeviceActor {
    let mut actor = DeviceActor::new(TransportConfig {
        minimum_request_spacing: Duration::ZERO,
        ..TransportConfig::default()
    });
    actor.command(
        DeviceCommand::Connect {
            intent: IntentId(1),
            selection: DeviceSelection {
                input_id: "fixture-in".into(),
                output_id: "fixture-out".into(),
                fingerprint_hint: None,
            },
        },
        Duration::ZERO,
    );
    actions(&mut actor);
    actor.opened(Ok(()), Duration::ZERO);
    actions(&mut actor);
    actor.sent(Ok(()), Duration::ZERO);
    receive(&mut actor, &raw(&[1, 32, 4, 0, 1, 0, 2, 0]), 1);
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(2));
    receive(&mut actor, &raw(payload), 3);
    actions(&mut actor);
    assert_eq!(actor.state(), ConnectionState::Ready);
    actor
}
fn code(raw: &RawTransfer, module: u8) -> u32 {
    let record = raw
        .records()
        .iter()
        .find(|r| r.command == 0x14 && r.body[0] == module)
        .unwrap();
    u32::from_le_bytes(record.body[4..8].try_into().unwrap())
}
fn submit(actor: &mut DeviceActor, module: u8, target: u32) {
    actor.command(
        DeviceCommand::SubmitEdit {
            stamp: RequestStamp {
                epoch: actor.epoch(),
                intent: IntentId(2),
            },
            target: PresetTarget::EditBuffer,
            base_revision: actor.snapshot(PresetTarget::EditBuffer).unwrap().revision,
            patch: EditPatch::SetModel {
                module: ModuleId::new(module).unwrap(),
                model: ModelCode(target),
            },
        },
        Duration::from_millis(4),
    );
}
fn begin_readback(actor: &mut DeviceActor) {
    let sent = actions(actor);
    assert!(
        sent.iter()
            .any(|a| matches!(a, TransportAction::Send { .. }))
    );
    actor.sent(Ok(()), Duration::from_millis(5));
    actions(actor);
    actor.tick(Duration::from_millis(50));
    assert!(
        actions(actor)
            .iter()
            .any(|a| matches!(a, TransportAction::Send { .. }))
    );
    actor.sent(Ok(()), Duration::from_millis(51));
}

#[test]
fn complete_captured_sparse_model_readbacks_verify_without_erasing_hidden_slots() {
    let fixtures = [
        (
            4,
            include_bytes!("../../../tests/fixtures/model-transitions/nr-before.payload.bin")
                .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/nr-after.payload.bin")
                .as_slice(),
        ),
        (
            5,
            include_bytes!("../../../tests/fixtures/model-transitions/cab-before.payload.bin")
                .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/cab-after.payload.bin")
                .as_slice(),
        ),
        (
            6,
            include_bytes!("../../../tests/fixtures/model-transitions/eq-before.payload.bin")
                .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/eq-after.payload.bin")
                .as_slice(),
        ),
        (
            7,
            include_bytes!("../../../tests/fixtures/model-transitions/mod-before.payload.bin")
                .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/mod-after.payload.bin")
                .as_slice(),
        ),
        (
            8,
            include_bytes!("../../../tests/fixtures/model-transitions/dly-before.payload.bin")
                .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/dly-after.payload.bin")
                .as_slice(),
        ),
        (
            9,
            include_bytes!("../../../tests/fixtures/model-transitions/rvb-before.payload.bin")
                .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/rvb-after.payload.bin")
                .as_slice(),
        ),
        (
            6,
            include_bytes!("../../../tests/fixtures/model-transitions/eq-after.payload.bin")
                .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/eq-return.payload.bin")
                .as_slice(),
        ),
        (
            8,
            include_bytes!("../../../tests/fixtures/model-transitions/dly-after.payload.bin")
                .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/dly-return.payload.bin")
                .as_slice(),
        ),
        (
            5,
            include_bytes!(
                "../../../tests/fixtures/model-transitions/cab-return-before.payload.bin"
            )
            .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/cab-return.payload.bin")
                .as_slice(),
        ),
        (
            7,
            include_bytes!(
                "../../../tests/fixtures/model-transitions/mod-return-before.payload.bin"
            )
            .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/mod-return.payload.bin")
                .as_slice(),
        ),
        (
            9,
            include_bytes!(
                "../../../tests/fixtures/model-transitions/rvb-return-before.payload.bin"
            )
            .as_slice(),
            include_bytes!("../../../tests/fixtures/model-transitions/rvb-return.payload.bin")
                .as_slice(),
        ),
    ];
    for (module, before, after) in fixtures {
        let mut actor = ready(before);
        let expected = raw(after);
        submit(&mut actor, module, code(&expected, module));
        begin_readback(&mut actor);
        receive(&mut actor, &expected, 52);
        let result = actions(&mut actor);
        let stages: Vec<_> = result
            .iter()
            .filter_map(|a| {
                if let TransportAction::Event(DeviceEvent::EditProgress { stage, .. }) = a {
                    Some(stage)
                } else {
                    None
                }
            })
            .collect();
        assert!(
            result.iter().any(|a| matches!(
                a,
                TransportAction::Event(DeviceEvent::EditProgress {
                    stage: EditStage::Verified,
                    ..
                })
            )),
            "module {module}, stages {stages:?}, actor {:?}",
            actor.state()
        );
        assert_eq!(
            actor
                .snapshot(PresetTarget::EditBuffer)
                .unwrap()
                .raw
                .records(),
            expected.records()
        );
    }
}

#[test]
fn complete_readback_with_wrong_hidden_bits_fails_and_actual_is_retained() {
    let before = include_bytes!("../../../tests/fixtures/model-transitions/cab-before.payload.bin");
    let mut records = raw(include_bytes!(
        "../../../tests/fixtures/model-transitions/cab-after.payload.bin"
    ))
    .records()
    .to_vec();
    let cab = records
        .iter_mut()
        .find(|r| r.command == 0x14 && r.body[0] == 5)
        .unwrap();
    cab.body[8..12].fill(0); // Undeclared legacy CAB control was actually retained.
    let actual = RawTransfer::from_records(0x12, &records).unwrap();
    let mut actor = ready(before);
    submit(&mut actor, 5, 0x0a000000);
    begin_readback(&mut actor);
    receive(&mut actor, &actual, 52);
    let result = actions(&mut actor);
    assert!(!result.iter().any(|a| matches!(
        a,
        TransportAction::Event(DeviceEvent::EditProgress {
            stage: EditStage::Verified,
            ..
        })
    )));
    let stages: Vec<_> = result
        .iter()
        .filter_map(|a| {
            if let TransportAction::Event(DeviceEvent::EditProgress { stage, .. }) = a {
                Some(stage)
            } else {
                None
            }
        })
        .collect();
    assert!(
        result.iter().any(|a| matches!(
            a,
            TransportAction::Event(DeviceEvent::EditProgress {
                stage: EditStage::Failed(_),
                ..
            })
        )),
        "{stages:?}, actor {:?}",
        actor.state()
    );
    assert_eq!(
        actor
            .snapshot(PresetTarget::EditBuffer)
            .unwrap()
            .raw
            .records(),
        actual.records()
    );
}

#[test]
fn assigned_amp_user_slots_ignored_model_and_invalid_context_never_send() {
    let baseline =
        include_bytes!("../../../tests/fixtures/model-transitions/dly-before.payload.bin");
    for (module, target) in [(3, 0x07000001), (5, 0x0a100000), (7, 0x04000000), (0, 0x1)] {
        let mut actor = ready(baseline);
        submit(&mut actor, module, target);
        assert!(
            !actions(&mut actor)
                .iter()
                .any(|a| matches!(a, TransportAction::Send { .. }))
        );
    }
    let mut snapshot = PresetSnapshot::from_raw(
        Arc::new(raw(baseline)),
        SnapshotSource::Hardware {
            epoch: SessionEpoch(1),
        },
        Some(PresetTarget::EditBuffer),
        Revision(1),
    );
    let module = ModuleId::new(8).unwrap();
    assert!(hardware_model_blocked_reason(&snapshot, module, ModelCode(0x0b00001d)).is_none());
    snapshot.context = Some(WireContext {
        group: 1,
        selector: 23,
    });
    assert!(hardware_model_blocked_reason(&snapshot, module, ModelCode(0x0b00001d)).is_some());
}

#[test]
fn eleven_new_pairs_verify_complete_actual_sparse_readbacks_in_both_directions() {
    for &(name, module, baseline, target, before, after, returned) in breadth::CASES {
        assert_eq!(code(&raw(before), module), baseline, "{name}");
        assert_eq!(code(&raw(after), module), target, "{name}");
        assert_eq!(code(&raw(returned), module), baseline, "{name}");
        for (source, expected_payload) in [(before, after), (after, returned)] {
            let mut actor = ready(source);
            let expected = raw(expected_payload);
            submit(&mut actor, module, code(&expected, module));
            begin_readback(&mut actor);
            receive(&mut actor, &expected, 52);
            assert!(
                actions(&mut actor).iter().any(|action| matches!(
                    action,
                    TransportAction::Event(DeviceEvent::EditProgress {
                        stage: EditStage::Verified,
                        ..
                    })
                )),
                "{name}"
            );
            assert_eq!(
                actor
                    .snapshot(PresetTarget::EditBuffer)
                    .unwrap()
                    .raw
                    .records(),
                expected.records(),
                "{name}"
            );
        }
    }
}

#[test]
fn every_new_pair_rejects_an_independent_readback_with_changed_hidden_float_bits() {
    let catalog = gp200_catalog::Catalog::embedded().unwrap();
    for &(name, module, _, target, before, after, _) in breadth::CASES {
        let definition = catalog.model_by_id(module, target).unwrap();
        let hidden = (0..15)
            .find(|index| definition.parameter(*index).is_none())
            .unwrap();
        let mut records = raw(after).records().to_vec();
        let record = records
            .iter_mut()
            .find(|record| record.command == 0x14 && record.body[0] == module)
            .unwrap();
        record.body[8 + 4 * usize::from(hidden)] ^= 1;
        let actual = RawTransfer::from_records(0x12, &records).unwrap();
        let mut actor = ready(before);
        submit(&mut actor, module, target);
        begin_readback(&mut actor);
        receive(&mut actor, &actual, 52);
        let result = actions(&mut actor);
        assert!(
            !result.iter().any(|action| matches!(
                action,
                TransportAction::Event(DeviceEvent::EditProgress {
                    stage: EditStage::Verified,
                    ..
                })
            )),
            "{name}"
        );
        assert!(
            result.iter().any(|action| matches!(
                action,
                TransportAction::Event(DeviceEvent::EditProgress {
                    stage: EditStage::Failed(_),
                    ..
                })
            )),
            "{name}"
        );
        assert_eq!(
            actor
                .snapshot(PresetTarget::EditBuffer)
                .unwrap()
                .raw
                .records(),
            actual.records(),
            "{name}"
        );
    }
}

#[test]
fn captured_targets_do_not_promote_cross_pairs_assigned_modules_or_malformed_shapes() {
    for &(name, module, _, target, before, after, _) in breadth::CASES {
        for other in gp200_transport::captured_model_profiles()
            .iter()
            .filter(|other| other.module == module && other.target != target)
        {
            let mut actor = ready(after);
            submit(&mut actor, module, other.target);
            assert!(
                !actions(&mut actor)
                    .iter()
                    .any(|action| matches!(action, TransportAction::Send { .. })),
                "{name} → {}",
                other.name
            );
        }
        for assignment_command in [0x0c, 0x10] {
            let mut records = raw(before).records().to_vec();
            records
                .iter_mut()
                .find(|record| record.command == assignment_command)
                .unwrap()
                .body[1] = module;
            let assigned = RawTransfer::from_records(0x12, &records).unwrap();
            let mut actor = ready(assigned.payload());
            submit(&mut actor, module, target);
            assert!(
                !actions(&mut actor)
                    .iter()
                    .any(|action| matches!(action, TransportAction::Send { .. })),
                "assigned {name}"
            );
        }
        for malformed in 0..3 {
            let mut records = raw(before).records().to_vec();
            let index = records
                .iter()
                .position(|record| record.command == 0x14 && record.body[0] == module)
                .unwrap();
            match malformed {
                0 => records[index].body[1] = 2,
                1 => records[index].body[2] = 14,
                _ => records.push(records[index].clone()),
            }
            let transfer = RawTransfer::from_records(0x12, &records).unwrap();
            let snapshot = PresetSnapshot::from_raw(
                Arc::new(transfer),
                SnapshotSource::Simulator,
                Some(PresetTarget::EditBuffer),
                Revision(1),
            );
            assert!(
                hardware_model_blocked_reason(
                    &snapshot,
                    ModuleId::new(module).unwrap(),
                    ModelCode(target)
                )
                .is_some(),
                "malformed {name}"
            );
        }
    }
    for target in [0x21, 0x04000000, 0x0a100000, 0x0f000000] {
        let module = match target {
            0x21 => 4,
            0x04000000 => 7,
            0x0a100000 => 5,
            _ => 3,
        };
        let mut actor = ready(breadth::CASES[0].4);
        submit(&mut actor, module, target);
        assert!(
            !actions(&mut actor)
                .iter()
                .any(|action| matches!(action, TransportAction::Send { .. }))
        );
    }
}
