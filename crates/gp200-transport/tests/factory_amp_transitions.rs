// Private capture acceptance suite; excluded from public source builds.
// Run with RUSTFLAGS="--cfg gp200_private_evidence" only in the retained lab checkout.
#![cfg(gp200_private_evidence)]

//! Exact factory candidates against synthetic independent vectors; no physical acceptance.
use gp200_catalog::{Catalog, ModelDefaultPlan};
use gp200_domain::*;
use gp200_protocol::encode_transfer;
use gp200_transport::{
    DeviceActor, TransportAction, TransportConfig, hardware_model_blocked_reason_with_globals,
};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
const VECTORS: &str = include_str!(
    "../../../analysis/implementation/factory-amp-expansion-execution/synthetic-vectors.json"
);
fn cases() -> Vec<Value> {
    serde_json::from_str::<Value>(VECTORS).unwrap()["cases"]
        .as_array()
        .unwrap()
        .clone()
}
fn raw(hex: &str) -> RawTransfer {
    RawTransfer::from_payload(
        0x12,
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect(),
    )
    .unwrap()
}
fn actions(actor: &mut DeviceActor) -> Vec<TransportAction> {
    std::iter::from_fn(|| actor.next_action()).collect()
}
fn receive(actor: &mut DeviceActor, raw: &RawTransfer, ms: u64) {
    for packet in encode_transfer(raw).unwrap() {
        actor.receive(actor.epoch(), &packet, Duration::from_millis(ms));
    }
}
fn sends(actions: &[TransportAction]) -> usize {
    actions
        .iter()
        .filter(|action| matches!(action, TransportAction::Send { .. }))
        .count()
}
fn verified(actions: &[TransportAction]) -> bool {
    actions.iter().any(|action| {
        matches!(
            action,
            TransportAction::Event(DeviceEvent::EditProgress {
                stage: EditStage::Verified,
                ..
            })
        )
    })
}
#[cfg(feature = "factory-amp-trials")]
fn changed(raw: &RawTransfer, record: usize, offset: usize) -> RawTransfer {
    let mut records = raw.records().to_vec();
    records[record].body[offset] ^= 1;
    RawTransfer::from_records(raw.kind(), &records).unwrap()
}
fn ready(case: &Value, source: &str) -> DeviceActor {
    let mut actor = DeviceActor::with_mode(
        TransportConfig {
            minimum_request_spacing: Duration::ZERO,
            ..TransportConfig::default()
        },
        BackendMode::Hardware,
    );
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
    receive(
        &mut actor,
        &RawTransfer::from_payload(0x12, vec![1, 32, 4, 0, 1, 0, 2, 0]).unwrap(),
        1,
    );
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(2));
    receive(&mut actor, &raw(source), 3);
    actions(&mut actor);
    actor.command(
        DeviceCommand::ReadGlobals(RequestStamp {
            epoch: actor.epoch(),
            intent: IntentId(2),
        }),
        Duration::from_millis(4),
    );
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(5));
    receive(&mut actor, &raw(case["globals"].as_str().unwrap()), 6);
    actions(&mut actor);
    assert_eq!(actor.state(), ConnectionState::Ready);
    actor
}
fn command(actor: &DeviceActor, case: &Value, target: u32) -> DeviceCommand {
    DeviceCommand::SubmitFactoryAmpModel {
        stamp: RequestStamp {
            epoch: actor.epoch(),
            intent: IntentId(3),
        },
        base_revision: actor.snapshot(PresetTarget::EditBuffer).unwrap().revision,
        model: ModelCode(target),
        profile: FactoryAmpProfileId::new(
            ModelCode(case["target"].as_u64().unwrap() as u32),
            case["auto_cab"].as_bool().unwrap(),
        )
        .unwrap(),
        before_globals: actor.global_state().unwrap().clone(),
        expected_auto_cab: case["auto_cab"].as_bool().unwrap(),
    }
}
#[test]
fn all_fixed_descriptors_match_actual_catalog_code_index_cab_and_bits() {
    let catalog = Catalog::embedded().unwrap();
    for descriptor in FACTORY_AMP_CANDIDATES {
        let definition = catalog.model_by_id(3, descriptor.model.0).unwrap();
        assert_eq!(definition.xml_index, descriptor.xml_index);
        assert_eq!(
            definition.raw_attributes["CABCode"].parse::<u32>().unwrap(),
            descriptor.paired_cabinet.0
        );
        let defaults = ModelDefaultPlan::from_definition(definition);
        assert!(defaults.unknown_defaults.is_empty());
        assert_eq!(defaults.declared, descriptor.defaults);
        let cabinet = ModelDefaultPlan::from_definition(
            catalog.model_by_id(5, descriptor.paired_cabinet.0).unwrap(),
        );
        assert_eq!(cabinet.declared, FACTORY_AMP_CAB_DEFAULTS);
    }
}
#[test]
fn shared_picker_and_actor_gate_agree_for_every_exact_context() {
    for case in cases() {
        let mut actor = ready(&case, case["before"].as_str().unwrap());
        let target = ModelCode(case["target"].as_u64().unwrap() as u32);
        let reason = hardware_model_blocked_reason_with_globals(
            actor.snapshot(PresetTarget::EditBuffer).unwrap(),
            actor.global_state().map(Arc::as_ref),
            ModuleId::new(3).unwrap(),
            target,
        );
        assert_eq!(reason.is_none(), cfg!(feature = "factory-amp-trials"));
        actor.command(command(&actor, &case, target.0), Duration::from_millis(10));
        let next = actions(&mut actor);
        assert_eq!(
            sends(&next),
            usize::from(cfg!(feature = "factory-amp-trials"))
        );
        assert!(!verified(&next));
    }
}
#[test]
fn generic_model_command_cannot_bypass_the_typed_candidate_path() {
    let case = cases().remove(0);
    for target in [
        0x07000003, 0x0700004a, 0x0700002c, 0x0700002f, 0x0700005a, 0x07100000,
    ] {
        let mut actor = ready(&case, case["before"].as_str().unwrap());
        actor.command(
            DeviceCommand::SubmitEdit {
                stamp: RequestStamp {
                    epoch: actor.epoch(),
                    intent: IntentId(3),
                },
                target: PresetTarget::EditBuffer,
                base_revision: actor.snapshot(PresetTarget::EditBuffer).unwrap().revision,
                patch: EditPatch::SetModel {
                    module: ModuleId::new(3).unwrap(),
                    model: ModelCode(target),
                },
            },
            Duration::from_millis(10),
        );
        assert_eq!(sends(&actions(&mut actor)), 0);
    }
}

#[cfg(feature = "factory-amp-trials")]
#[test]
fn nonfinite_out_of_range_or_blocked_nondefault_originals_reject_before_model() {
    let cases = cases();
    for bits in [0x7fc00001u32, 101f32.to_bits()] {
        let mut case = cases[1].clone();
        let mut records = raw(case["before"].as_str().unwrap()).records().to_vec();
        records[6].body[8..12].copy_from_slice(&bits.to_le_bytes());
        let before = RawTransfer::from_records(0x12, &records).unwrap();
        case["before"] = before
            .payload()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
            .into();
        let mut actor = ready(&case, case["before"].as_str().unwrap());
        actor.command(
            command(&actor, &case, 0x07000003),
            Duration::from_millis(10),
        );
        assert_eq!(sends(&actions(&mut actor)), 0);
    }
    let case = cases
        .iter()
        .find(|case| case["target"] == 0x07000004u32 && case["auto_cab"] == true)
        .unwrap();
    let mut records = raw(case["forward"].as_str().unwrap()).records().to_vec();
    records[6].body[28..32].copy_from_slice(&0f32.to_le_bytes());
    let before = RawTransfer::from_records(0x12, &records).unwrap();
    let before = before
        .payload()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mut actor = ready(case, &before);
    actor.command(
        command(&actor, case, FACTORY_AMP_BASELINE.0),
        Duration::from_millis(10),
    );
    assert_eq!(sends(&actions(&mut actor)), 0);
}
#[cfg(feature = "factory-amp-trials")]
fn preflight(actor: &mut DeviceActor, case: &Value, source: &RawTransfer) -> Vec<TransportAction> {
    actor.sent(Ok(()), Duration::from_millis(11));
    receive(actor, &raw(case["globals"].as_str().unwrap()), 12);
    let stage = actions(actor);
    assert_eq!(sends(&stage), 1);
    assert!(!stage.iter().any(|action| matches!(
        action,
        TransportAction::Event(
            DeviceEvent::GlobalStateReceived { .. } | DeviceEvent::SnapshotReceived { .. }
        )
    )));
    actor.sent(Ok(()), Duration::from_millis(13));
    receive(actor, source, 14);
    actions(actor)
}
#[cfg(feature = "factory-amp-trials")]
fn postflight(
    actor: &mut DeviceActor,
    preset: &RawTransfer,
    globals: &RawTransfer,
) -> Vec<TransportAction> {
    actor.sent(Ok(()), Duration::from_millis(15));
    actions(actor);
    actor.tick(Duration::from_millis(100));
    assert_eq!(sends(&actions(actor)), 1);
    actor.sent(Ok(()), Duration::from_millis(101));
    receive(actor, preset, 102);
    let post = actions(actor);
    assert_eq!(sends(&post), 1);
    assert!(!verified(&post));
    actor.sent(Ok(()), Duration::from_millis(103));
    receive(actor, globals, 104);
    actions(actor)
}
#[cfg(feature = "factory-amp-trials")]
#[test]
fn every_forward_reverse_requires_two_complete_preflights_and_sends_one_amp_only() {
    for case in cases() {
        for reverse in [false, true] {
            let source = if reverse { "forward" } else { "before" };
            let expected = if reverse { "reverse" } else { "forward" };
            let target = if reverse {
                FACTORY_AMP_BASELINE.0
            } else {
                case["target"].as_u64().unwrap() as u32
            };
            let mut actor = ready(&case, case[source].as_str().unwrap());
            actor.command(command(&actor, &case, target), Duration::from_millis(10));
            assert_eq!(sends(&actions(&mut actor)), 1);
            let model = preflight(&mut actor, &case, &raw(case[source].as_str().unwrap()));
            assert_eq!(sends(&model), 1);
            let mut body = vec![3, 0, 0, 0];
            body.extend_from_slice(&target.to_le_bytes());
            let request = RawTransfer::from_records(
                gp200_protocol::WRITE_KIND,
                &[
                    RawRecord {
                        command: 0,
                        body: vec![0, 0, 23, 0],
                    },
                    RawRecord {
                        command: 0x16,
                        body,
                    },
                ],
            )
            .unwrap();
            let packets: Vec<_> = model
                .iter()
                .flat_map(|action| {
                    if let TransportAction::Send { packets } = action {
                        packets.clone()
                    } else {
                        Vec::new()
                    }
                })
                .collect();
            assert_eq!(packets, encode_transfer(&request).unwrap());
            let post = postflight(
                &mut actor,
                &raw(case[expected].as_str().unwrap()),
                &raw(case["globals"].as_str().unwrap()),
            );
            assert!(verified(&post), "{} reverse {reverse}", case["name"]);
            assert_eq!(sends(&post), 0);
        }
    }
}
#[cfg(feature = "factory-amp-trials")]
#[test]
fn wrong_profile_mode_revision_epoch_or_baseline_fails_before_preflight_or_model() {
    let case = cases().remove(1);
    for error in 0..6 {
        let mut actor = ready(&case, case["before"].as_str().unwrap());
        let mut request = command(&actor, &case, 0x07000003);
        let DeviceCommand::SubmitFactoryAmpModel {
            stamp,
            base_revision,
            profile,
            before_globals,
            expected_auto_cab,
            ..
        } = &mut request
        else {
            panic!()
        };
        match error {
            0 => *base_revision = Revision(0),
            1 => stamp.epoch = SessionEpoch(999),
            2 => *profile = FactoryAmpProfileId::new(ModelCode(0x07000004), true).unwrap(),
            3 => *expected_auto_cab = false,
            4 => {
                *before_globals = Arc::new(
                    DeviceGlobalState::from_transfer(
                        Revision(999),
                        actor.epoch(),
                        raw(case["globals"].as_str().unwrap()),
                        vec![],
                    )
                    .unwrap(),
                )
            }
            _ => {
                *before_globals = Arc::new(
                    DeviceGlobalState::from_transfer(
                        before_globals.revision,
                        actor.epoch(),
                        changed(before_globals.records().raw(), 3, 4),
                        vec![],
                    )
                    .unwrap(),
                )
            }
        }
        actor.command(request, Duration::from_millis(10));
        assert_eq!(sends(&actions(&mut actor)), 0);
    }
}
#[cfg(feature = "factory-amp-trials")]
#[test]
fn complete_preflight_runtime_autocab_and_cmd1010_changes_prevent_model_send() {
    let case = cases().remove(1);
    let before = raw(case["before"].as_str().unwrap());
    let global = raw(case["globals"].as_str().unwrap());
    for (record, offset) in [(3, 0), (3, 2), (3, 4), (0, 70), (2, 0)] {
        let mut actor = ready(&case, case["before"].as_str().unwrap());
        actor.command(
            command(&actor, &case, 0x07000003),
            Duration::from_millis(10),
        );
        actions(&mut actor);
        actor.sent(Ok(()), Duration::from_millis(11));
        receive(&mut actor, &changed(&global, record, offset), 12);
        let next = actions(&mut actor);
        assert_eq!(sends(&next), 0);
        assert!(!verified(&next));
    }
    for (record, offset) in [(6, 64), (3, 8), (17, 2), (8, 4)] {
        let mut actor = ready(&case, case["before"].as_str().unwrap());
        actor.command(
            command(&actor, &case, 0x07000003),
            Duration::from_millis(10),
        );
        actions(&mut actor);
        assert_eq!(
            sends(&preflight(
                &mut actor,
                &case,
                &changed(&before, record, offset)
            )),
            0
        );
    }
}
#[cfg(feature = "factory-amp-trials")]
#[test]
fn postflight_mismatch_or_lost_response_is_indeterminate_and_never_resends() {
    let case = cases().remove(1);
    let expected = raw(case["forward"].as_str().unwrap());
    let global = raw(case["globals"].as_str().unwrap());
    for error in 0..8 {
        let mut actor = ready(&case, case["before"].as_str().unwrap());
        actor.command(
            command(&actor, &case, 0x07000003),
            Duration::from_millis(10),
        );
        actions(&mut actor);
        assert_eq!(
            sends(&preflight(
                &mut actor,
                &case,
                &raw(case["before"].as_str().unwrap())
            )),
            1
        );
        let preset = if error < 3 {
            changed(
                &expected,
                if error == 0 {
                    6
                } else if error == 1 {
                    3
                } else {
                    17
                },
                if error == 0 {
                    64
                } else if error == 1 {
                    8
                } else {
                    2
                },
            )
        } else {
            expected.clone()
        };
        let globals = if error >= 3 {
            changed(
                &global,
                if error == 6 {
                    0
                } else if error == 7 {
                    2
                } else {
                    3
                },
                if error == 6 {
                    70
                } else if error == 7 || error == 3 {
                    0
                } else if error == 4 {
                    2
                } else {
                    4
                },
            )
        } else {
            global.clone()
        };
        let post = postflight(&mut actor, &preset, &globals);
        assert!(!verified(&post));
        assert!(post.iter().any(|action| matches!(
            action,
            TransportAction::Event(DeviceEvent::EditProgress {
                stage: EditStage::Indeterminate(_),
                ..
            })
        )));
        actor.tick(Duration::from_secs(20));
        assert_eq!(sends(&actions(&mut actor)), 0);
    }
    let mut actor = ready(&case, case["before"].as_str().unwrap());
    actor.command(
        command(&actor, &case, 0x07000003),
        Duration::from_millis(10),
    );
    actions(&mut actor);
    preflight(&mut actor, &case, &raw(case["before"].as_str().unwrap()));
    actor.sent(Ok(()), Duration::from_millis(15));
    actions(&mut actor);
    actor.tick(Duration::from_secs(20));
    let first = actions(&mut actor);
    assert!(!verified(&first));
    assert!(sends(&first) <= 1);
    actor.tick(Duration::from_secs(40));
    assert_eq!(sends(&actions(&mut actor)), 0);
}
