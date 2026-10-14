// Private capture acceptance suite; excluded from public source builds.
// Run with RUSTFLAGS="--cfg gp200_private_evidence" only in the retained lab checkout.
#![cfg(gp200_private_evidence)]

//! Replay fixed root captures. Candidate reverse projections are not hardware proof.
use gp200_catalog::{Catalog, ModelDefaultPlan};
use gp200_domain::*;
use gp200_protocol::{RawRecord, RawTransfer, encode_transfer};
use gp200_transport::{
    DeviceActor, TransportAction, TransportConfig, hardware_model_blocked_reason_with_globals,
};
use std::{sync::Arc, time::Duration};

const BEFORE: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-root/amp-auto-cab-on/current-before.payload.bin"
);
const GLOBALS: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-root/amp-auto-cab-on/globals-before.payload.bin"
);
const OFF: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-root/amp-auto-cab-off/globals-auto-cab-setup.payload.bin"
);
const OFF_AFTER: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-root/amp-auto-cab-off/current-model.payload.bin"
);
const OFF_GLOBAL_AFTER: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-root/amp-auto-cab-off/globals-model.payload.bin"
);
const ON_AFTER: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-root/amp-auto-cab-on/current-model.payload.bin"
);
const ON_GLOBAL_AFTER: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-root/amp-auto-cab-on/globals-model.payload.bin"
);
const WAH_AFTER: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-root/wah-assigned/current-model.payload.bin"
);
const WAH_GLOBAL_AFTER: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-root/wah-assigned/globals-model.payload.bin"
);

fn raw(payload: &[u8]) -> RawTransfer {
    RawTransfer::from_payload(0x12, payload.to_vec()).unwrap()
}
fn snapshot(raw: RawTransfer) -> PresetSnapshot {
    PresetSnapshot::from_raw(
        Arc::new(raw),
        SnapshotSource::Hardware {
            epoch: SessionEpoch(1),
        },
        Some(PresetTarget::EditBuffer),
        Revision(1),
    )
}
fn global(raw: RawTransfer, epoch: SessionEpoch) -> DeviceGlobalState {
    DeviceGlobalState::from_transfer(Revision(1), epoch, raw, vec![]).unwrap()
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
fn failed(actions: &[TransportAction], uncertain: bool) -> bool {
    actions.iter().any(|action| matches!(action, TransportAction::Event(DeviceEvent::EditProgress { stage, .. }) if if uncertain { matches!(stage, EditStage::Indeterminate(_)) } else { matches!(stage, EditStage::Failed(_)) }))
}
fn observed(actions: &[TransportAction]) -> bool {
    actions.iter().any(|action| {
        matches!(
            action,
            TransportAction::Event(
                DeviceEvent::SnapshotReceived { .. } | DeviceEvent::GlobalStateReceived { .. }
            )
        )
    })
}
fn changed(raw: &RawTransfer, index: usize, offset: usize, value: u8) -> RawTransfer {
    let mut records = raw.records().to_vec();
    records[index].body[offset] = value;
    RawTransfer::from_records(raw.kind(), &records).unwrap()
}
fn ready(globals: &[u8]) -> DeviceActor {
    ready_captured(globals, BEFORE)
}
fn ready_captured(globals: &[u8], before: &[u8]) -> DeviceActor {
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
    receive(&mut actor, &raw(&[1, 32, 4, 0, 1, 0, 2, 0]), 1);
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(2));
    receive(&mut actor, &raw(before), 3);
    actions(&mut actor);
    let stamp = RequestStamp {
        epoch: actor.epoch(),
        intent: IntentId(2),
    };
    actor.command(DeviceCommand::ReadGlobals(stamp), Duration::from_millis(4));
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(5));
    receive(&mut actor, &raw(globals), 6);
    actions(&mut actor);
    assert_eq!(actor.state(), ConnectionState::Ready);
    actor
}
fn command(actor: &DeviceActor, module: u8, target: u32) -> DeviceCommand {
    DeviceCommand::SubmitDependentModel {
        stamp: RequestStamp {
            epoch: actor.epoch(),
            intent: IntentId(3),
        },
        base_revision: actor.snapshot(PresetTarget::EditBuffer).unwrap().revision,
        module: ModuleId::new(module).unwrap(),
        model: ModelCode(target),
        before_globals: actor.global_state().unwrap().clone(),
        expected_auto_cab: dependent_auto_cab(actor.global_state().unwrap().records().raw())
            .unwrap(),
    }
}
fn submit(actor: &mut DeviceActor, module: u8, target: u32) -> Vec<TransportAction> {
    actor.command(command(actor, module, target), Duration::from_millis(10));
    actions(actor)
}
fn preflight(actor: &mut DeviceActor, globals: &[u8]) -> Vec<TransportAction> {
    actor.sent(Ok(()), Duration::from_millis(11));
    receive(actor, &raw(globals), 12);
    let global_preflight = actions(actor);
    assert_eq!(sends(&global_preflight), 1);
    assert!(!observed(&global_preflight));
    actor.sent(Ok(()), Duration::from_millis(13));
    receive(actor, &raw(BEFORE), 14);
    let model = actions(actor);
    assert_eq!(sends(&model), 1);
    assert!(!observed(&model));
    model
}
fn postflight_preset(actor: &mut DeviceActor, preset: &RawTransfer) -> Vec<TransportAction> {
    actor.sent(Ok(()), Duration::from_millis(15));
    actions(actor);
    actor.tick(Duration::from_millis(100));
    let query = actions(actor);
    assert_eq!(sends(&query), 1);
    actor.sent(Ok(()), Duration::from_millis(101));
    receive(actor, preset, 102);
    actions(actor)
}

#[test]
fn normal_profiles_are_accepted_only_with_exact_descriptor_contexts() {
    for (globals, module, code, profile, cabinet) in [
        (
            OFF,
            3,
            0x07000001,
            DependentModelProfile::AmpAutoCabOff,
            None,
        ),
        (
            GLOBALS,
            3,
            0x07000001,
            DependentModelProfile::AmpAutoCabOn,
            Some(ModelCode(0x0a000002)),
        ),
        (
            GLOBALS,
            1,
            0x05000001,
            DependentModelProfile::WahAssignedPreserve,
            None,
        ),
    ] {
        let plan = DependentModelPlan::from_snapshot(
            &snapshot(raw(BEFORE)),
            &global(raw(globals), SessionEpoch(1)),
            ModuleId::new(module).unwrap(),
            ModelCode(code),
        )
        .unwrap();
        assert_eq!(plan.profile, profile);
        assert_eq!(plan.paired_cabinet, cabinet);
        assert!(profile.normal_build_accepted());
    }
    for target in [0x07000002, 0x07100000, 0x07000015] {
        assert!(!dependent_model_pair(
            ModuleId::new(3).unwrap(),
            ModelCode(0x07000015),
            ModelCode(target)
        ));
    }
}

#[test]
fn malformed_extra_tlvs_and_uncaptured_assignment_context_fail_closed() {
    let original = raw(BEFORE);
    let globals = global(raw(GLOBALS), SessionEpoch(1));
    let invalid = [
        changed(&original, 14 + 3, 2, 2),
        changed(&original, 23, 1, 1),
        changed(&original, 14 + 4, 1, 1),
        changed(&original, 1, 0, 24),
    ];
    for value in invalid {
        assert!(
            DependentModelPlan::from_snapshot(
                &snapshot(value),
                &globals,
                ModuleId::new(1).unwrap(),
                ModelCode(0x05000001)
            )
            .is_err()
        );
    }
    let mut records = original.records().to_vec();
    records.push(RawRecord {
        command: 0x7777,
        body: vec![0xaa],
    });
    assert!(dependent_preset_shape(&RawTransfer::from_records(0x12, &records).unwrap()).is_err());
    records = raw(GLOBALS).records().to_vec();
    records.push(records[0].clone());
    assert!(dependent_globals_shape(&RawTransfer::from_records(0x12, &records).unwrap()).is_err());
    assert!(
        DependentModelPlan::from_snapshot(
            &snapshot(original),
            &global(raw(OFF), SessionEpoch(1)),
            ModuleId::new(1).unwrap(),
            ModelCode(0x05000001)
        )
        .is_err()
    );
}

#[test]
fn actual_forward_captures_match_sparse_defaults_and_all_assignment_and_hidden_bytes() {
    let catalog = Catalog::embedded().unwrap();
    for (globals, module, code, after) in [
        (OFF, 3, 0x07000001, OFF_AFTER),
        (GLOBALS, 3, 0x07000001, ON_AFTER),
        (GLOBALS, 1, 0x05000001, WAH_AFTER),
    ] {
        let before = snapshot(raw(BEFORE));
        let plan = DependentModelPlan::from_snapshot(
            &before,
            &global(raw(globals), SessionEpoch(1)),
            ModuleId::new(module).unwrap(),
            ModelCode(code),
        )
        .unwrap();
        let actual = raw(after);
        let mut expected = before.raw.records().to_vec();
        for (id, target) in
            std::iter::once((module, code)).chain(plan.paired_cabinet.map(|cabinet| (5, cabinet.0)))
        {
            ModelDefaultPlan::from_definition(catalog.model_by_id(id, target).unwrap())
                .apply_to_module_body(&mut expected[3 + id as usize].body)
                .unwrap();
        }
        // Existing preset equality masks only native metadata/chain copied bytes.
        for (index, (expected, actual)) in expected.iter().zip(actual.records()).enumerate() {
            if matches!(index, 1 | 2) {
                assert_eq!(expected.body[..15], actual.body[..15]);
                assert_eq!(expected.body[16..], actual.body[16..]);
            } else if expected.command == 0x0f {
                assert_eq!(expected.body[..2], actual.body[..2]);
                assert_eq!(expected.body[4..], actual.body[4..]);
            } else {
                assert_eq!(expected, actual, "record {index}, module {module}");
            }
        }
        assert_eq!(expected[17].body, before.raw.records()[17].body); // Exact EXP10 Position3.
        let default_position = 50f32.to_le_bytes();
        assert_eq!(
            &expected[4].body[20..24],
            if module == 1 {
                &default_position
            } else {
                &before.raw.records()[4].body[20..24]
            }
        );
    }
}

#[test]
fn global_policy_projects_latch_explicitly_and_keeps_third_word_strict() {
    let before = raw(GLOBALS);
    let actual = raw(ON_GLOBAL_AFTER);
    assert!(dependent_globals_match(&before, &actual, Some(1)).unwrap());
    assert!(!dependent_globals_match(&before, &changed(&actual, 3, 4, 2), Some(1)).unwrap());
    assert!(!dependent_globals_match(&before, &changed(&actual, 0, 70, 0), Some(1)).unwrap());
    let latch0 = changed(&before, 3, 2, 0);
    assert!(!dependent_globals_match(&latch0, &actual, None).unwrap());
    assert!(dependent_globals_match(&latch0, &actual, Some(1)).unwrap());
    for offset in [54, 55, 56] {
        assert!(
            dependent_globals_match(
                &before,
                &changed(&before, 0, offset, before.records()[0].body[offset] ^ 1),
                None
            )
            .unwrap()
        );
    }
    assert!(
        !dependent_globals_match(
            &before,
            &changed(&before, 0, 57, before.records()[0].body[57] ^ 1),
            None
        )
        .unwrap()
    );
}

#[test]
fn reducer_auto_cab_binding_and_exact_cached_global_authority_cannot_be_forged() {
    for mutation in 0..4 {
        let mut actor = ready(GLOBALS);
        let mut request = command(&actor, 3, 0x07000001);
        let DeviceCommand::SubmitDependentModel {
            expected_auto_cab,
            before_globals,
            base_revision,
            ..
        } = &mut request
        else {
            unreachable!()
        };
        match mutation {
            0 => *expected_auto_cab = false,
            1 => {
                *before_globals = Arc::new(global(changed(&raw(GLOBALS), 0, 50, 99), actor.epoch()))
            }
            2 => *before_globals = Arc::new(global(raw(GLOBALS), SessionEpoch(999))),
            _ => *base_revision = Revision(999),
        }
        actor.command(request, Duration::from_millis(10));
        let result = actions(&mut actor);
        assert_eq!(sends(&result), 0);
        assert!(result.iter().any(|action| matches!(
            action,
            TransportAction::Event(DeviceEvent::OperationFailed { .. })
        )));
    }
}

#[test]
fn generic_model_path_never_bypasses_the_explicit_global_binding() {
    let mut actor = ready(GLOBALS);
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
                model: ModelCode(0x07000001),
            },
        },
        Duration::from_millis(10),
    );
    assert_eq!(sends(&actions(&mut actor)), 0);
}

#[test]
fn shared_picker_policy_requires_globals_in_normal_builds() {
    let before = snapshot(raw(BEFORE));
    let global = global(raw(GLOBALS), SessionEpoch(1));
    let module = ModuleId::new(3).unwrap();
    assert!(
        hardware_model_blocked_reason_with_globals(&before, None, module, ModelCode(0x07000001))
            .is_some()
    );
    assert!(
        hardware_model_blocked_reason_with_globals(
            &before,
            Some(&global),
            module,
            ModelCode(0x07000001)
        )
        .is_none()
    );
}

#[test]
fn normal_actor_only_verifies_after_both_complete_actual_forward_readbacks() {
    for (globals, module, target, after, global_after) in [
        (OFF, 3, 0x07000001, OFF_AFTER, OFF_GLOBAL_AFTER),
        (GLOBALS, 3, 0x07000001, ON_AFTER, ON_GLOBAL_AFTER),
        (GLOBALS, 1, 0x05000001, WAH_AFTER, WAH_GLOBAL_AFTER),
    ] {
        let mut actor = ready(globals);
        assert_eq!(sends(&submit(&mut actor, module, target)), 1);
        preflight(&mut actor, globals);
        let post = postflight_preset(&mut actor, &raw(after));
        assert!(!verified(&post));
        assert!(observed(&post));
        assert_eq!(sends(&post), 1);
        actor.sent(Ok(()), Duration::from_millis(103));
        receive(&mut actor, &raw(global_after), 104);
        let result = actions(&mut actor);
        assert!(verified(&result));
        assert_eq!(sends(&result), 0);
    }
}

#[test]
fn fresh_global_mismatch_does_not_repredict_or_send_a_model() {
    for fresh in [
        raw(OFF),
        changed(&raw(GLOBALS), 3, 4, 2),
        changed(&raw(GLOBALS), 0, 50, 99),
    ] {
        let mut actor = ready(GLOBALS);
        submit(&mut actor, 3, 0x07000001);
        actor.sent(Ok(()), Duration::from_millis(11));
        receive(&mut actor, &fresh, 12);
        let result = actions(&mut actor);
        assert_eq!(sends(&result), 0);
        assert!(failed(&result, false));
        assert!(!failed(&result, true));
        assert_eq!(
            actor.global_state().unwrap().records().raw().payload(),
            fresh.payload()
        );
    }
}

#[test]
fn fresh_current_mismatch_keeps_actual_hidden_data_and_sends_nothing() {
    let mut actor = ready(GLOBALS);
    submit(&mut actor, 3, 0x07000001);
    actor.sent(Ok(()), Duration::from_millis(11));
    receive(&mut actor, &raw(GLOBALS), 12);
    actions(&mut actor);
    let actual = changed(&raw(BEFORE), 3 + 3, 64, 0x45);
    actor.sent(Ok(()), Duration::from_millis(13));
    receive(&mut actor, &actual, 14);
    let result = actions(&mut actor);
    assert_eq!(sends(&result), 0);
    assert!(failed(&result, false));
    assert_eq!(
        actor
            .snapshot(PresetTarget::EditBuffer)
            .unwrap()
            .raw
            .payload(),
        actual.payload()
    );
}

#[test]
fn hidden_assignment_or_strict_global_postflight_changes_remain_indeterminate() {
    for (preset, globals) in [
        (
            changed(&raw(ON_AFTER), 3 + 3, 64, 0x45),
            raw(ON_GLOBAL_AFTER),
        ),
        (changed(&raw(ON_AFTER), 17, 2, 2), raw(ON_GLOBAL_AFTER)),
        (raw(ON_AFTER), changed(&raw(ON_GLOBAL_AFTER), 3, 4, 2)),
        (raw(ON_AFTER), raw(OFF_GLOBAL_AFTER)),
    ] {
        let mut actor = ready(GLOBALS);
        submit(&mut actor, 3, 0x07000001);
        preflight(&mut actor, GLOBALS);
        postflight_preset(&mut actor, &preset);
        actor.sent(Ok(()), Duration::from_millis(103));
        receive(&mut actor, &globals, 104);
        let result = actions(&mut actor);
        assert!(failed(&result, true));
        assert!(!verified(&result));
        assert_eq!(sends(&result), 0);
        assert_eq!(
            actor
                .snapshot(PresetTarget::EditBuffer)
                .unwrap()
                .raw
                .payload(),
            preset.payload()
        );
        actor.tick(Duration::from_secs(10));
        assert_eq!(sends(&actions(&mut actor)), 0);
    }
}

#[test]
fn postflight_global_loss_or_partial_send_cannot_resend_or_claim_definite_failure() {
    for partial in [false, true] {
        let mut actor = ready(GLOBALS);
        submit(&mut actor, 3, 0x07000001);
        preflight(&mut actor, GLOBALS);
        postflight_preset(&mut actor, &raw(ON_AFTER));
        let epoch = actor.epoch();
        if partial {
            actor.sent(
                Err(ServiceError::new(
                    ErrorKind::Io,
                    "partial global query send",
                )),
                Duration::from_millis(103),
            );
        } else {
            actor.sent(Ok(()), Duration::from_millis(103));
            actor.tick(Duration::from_secs(10));
        }
        let result = actions(&mut actor);
        assert!(failed(&result, true));
        assert_eq!(sends(&result), 0);
        assert_ne!(epoch, actor.epoch());
        receive(&mut actor, &raw(ON_GLOBAL_AFTER), 10001);
        assert!(!verified(&actions(&mut actor)));
    }
}

// Actual accepted candidate captures; new no-feature physical trials are separate.
struct ObservedCase {
    name: &'static str,
    module: u8,
    target: u32,
    original: u32,
    before: &'static [u8],
    globals: &'static [u8],
    forward: &'static [u8],
    forward_globals: &'static [u8],
    reverse: &'static [u8],
    reverse_globals: &'static [u8],
    undo: &'static [u8],
    undo_globals: &'static [u8],
    controls: &'static [(&'static [u8], u8, u8, u32)],
}
const OBSERVED_CASES: &[ObservedCase] = &[
    ObservedCase {
        name: "amp-auto-cab-off",
        module: 3,
        target: 117440513,
        original: 117440533,
        before: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-off/rust/own-fresh-current-before.payload.bin"
        ),
        globals: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-off/rust/own-fresh-globals-before.payload.bin"
        ),
        forward: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-off/rust/after-forward.payload.bin"
        ),
        forward_globals: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-off/rust/after-forward-globals.payload.bin"
        ),
        reverse: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-off/rust/undo-63-5.preset.payload.bin"
        ),
        undo: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-off/rust/after-undo.payload.bin"
        ),
        undo_globals: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-off/rust/after-undo-globals.payload.bin"
        ),
        reverse_globals: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-off/rust/undo-66-5.globals.payload.bin"
        ),
        controls: &[
            (
                include_bytes!(
                    "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-off/rust/undo-74-6.preset.payload.bin"
                ),
                3,
                0,
                1112014848,
            ),
            (
                include_bytes!(
                    "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-off/rust/undo-82-7.preset.payload.bin"
                ),
                3,
                1,
                1109393408,
            ),
            (
                include_bytes!(
                    "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-off/rust/undo-90-8.preset.payload.bin"
                ),
                3,
                4,
                1115815936,
            ),
            (
                include_bytes!(
                    "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-off/rust/undo-99-9.preset.payload.bin"
                ),
                3,
                5,
                1106247680,
            ),
        ],
    },
    ObservedCase {
        name: "amp-auto-cab-on",
        module: 3,
        target: 117440513,
        original: 117440533,
        before: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/own-fresh-current-before.payload.bin"
        ),
        globals: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/own-fresh-globals-before.payload.bin"
        ),
        forward: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/after-forward.payload.bin"
        ),
        forward_globals: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/after-forward-globals.payload.bin"
        ),
        reverse: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/undo-58-5.preset.payload.bin"
        ),
        undo: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/after-undo.payload.bin"
        ),
        undo_globals: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/after-undo-globals.payload.bin"
        ),
        reverse_globals: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/undo-62-5.globals.payload.bin"
        ),
        controls: &[
            (
                include_bytes!(
                    "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/undo-71-6.preset.payload.bin"
                ),
                3,
                0,
                1112014848,
            ),
            (
                include_bytes!(
                    "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/undo-79-7.preset.payload.bin"
                ),
                3,
                1,
                1109393408,
            ),
            (
                include_bytes!(
                    "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/undo-87-8.preset.payload.bin"
                ),
                3,
                4,
                1115815936,
            ),
            (
                include_bytes!(
                    "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/undo-95-9.preset.payload.bin"
                ),
                3,
                5,
                1106247680,
            ),
            (
                include_bytes!(
                    "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/undo-103-10.preset.payload.bin"
                ),
                5,
                1,
                1114636288,
            ),
            (
                include_bytes!(
                    "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/undo-111-11.preset.payload.bin"
                ),
                5,
                5,
                1120403456,
            ),
        ],
    },
    ObservedCase {
        name: "wah-assigned-preserve",
        module: 1,
        target: 83886081,
        original: 83886090,
        before: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/wah-assigned-preserve/rust/own-fresh-current-before.payload.bin"
        ),
        globals: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/wah-assigned-preserve/rust/own-fresh-globals-before.payload.bin"
        ),
        forward: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/wah-assigned-preserve/rust/after-forward.payload.bin"
        ),
        forward_globals: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/wah-assigned-preserve/rust/after-forward-globals.payload.bin"
        ),
        reverse: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/wah-assigned-preserve/rust/undo-61-5.preset.payload.bin"
        ),
        undo: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/wah-assigned-preserve/rust/after-undo.payload.bin"
        ),
        undo_globals: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/wah-assigned-preserve/rust/after-undo-globals.payload.bin"
        ),
        reverse_globals: include_bytes!(
            "../../../analysis/implementation/dependent-model-actor-root/wah-assigned-preserve/rust/undo-64-5.globals.payload.bin"
        ),
        controls: &[
            (
                include_bytes!(
                    "../../../analysis/implementation/dependent-model-actor-root/wah-assigned-preserve/rust/undo-73-6.preset.payload.bin"
                ),
                1,
                0,
                1120403456,
            ),
            (
                include_bytes!(
                    "../../../analysis/implementation/dependent-model-actor-root/wah-assigned-preserve/rust/undo-81-7.preset.payload.bin"
                ),
                1,
                1,
                1120403456,
            ),
            (
                include_bytes!(
                    "../../../analysis/implementation/dependent-model-actor-root/wah-assigned-preserve/rust/undo-91-8.preset.payload.bin"
                ),
                1,
                2,
                1120403456,
            ),
            (
                include_bytes!(
                    "../../../analysis/implementation/dependent-model-actor-root/wah-assigned-preserve/rust/undo-99-9.preset.payload.bin"
                ),
                1,
                3,
                1120318921,
            ),
        ],
    },
];

fn observed_transition(
    actor: &mut DeviceActor,
    case: &ObservedCase,
    reverse: bool,
    after: &[u8],
    start: u64,
) -> Vec<TransportAction> {
    let (target, before, globals, globals_after) = if reverse {
        (
            case.original,
            case.forward,
            case.forward_globals,
            case.reverse_globals,
        )
    } else {
        (case.target, case.before, case.globals, case.forward_globals)
    };
    let module = case.module;
    let mut request = command(actor, module, target);
    if let DeviceCommand::SubmitDependentModel { stamp, .. } = &mut request {
        stamp.intent = IntentId(3 + start / 200);
    }
    actor.command(request, Duration::from_millis(start));
    assert_eq!(sends(&actions(actor)), 1);
    actor.sent(Ok(()), Duration::from_millis(start + 1));
    receive(actor, &raw(globals), start + 2);
    let global_preflight = actions(actor);
    assert_eq!(sends(&global_preflight), 1);
    assert!(!observed(&global_preflight));
    actor.sent(Ok(()), Duration::from_millis(start + 3));
    receive(actor, &raw(before), start + 4);
    let preset_preflight = actions(actor);
    assert_eq!(sends(&preset_preflight), 1);
    assert!(!observed(&preset_preflight));
    actor.sent(Ok(()), Duration::from_millis(start + 5));
    actions(actor);
    actor.tick(Duration::from_millis(start + 100));
    assert_eq!(sends(&actions(actor)), 1);
    actor.sent(Ok(()), Duration::from_millis(start + 101));
    receive(actor, &raw(after), start + 102);
    let preset_postflight = actions(actor);
    assert!(!verified(&preset_postflight));
    assert!(observed(&preset_postflight));
    assert_eq!(sends(&preset_postflight), 1);
    actor.sent(Ok(()), Duration::from_millis(start + 103));
    receive(actor, &raw(globals_after), start + 104);
    actions(actor)
}

#[test]
fn actual_candidate_forward_and_direct_reverse_captures_verify_in_normal_actor() {
    for case in OBSERVED_CASES {
        let mut actor = ready_captured(case.globals, case.before);
        let forward = observed_transition(&mut actor, case, false, case.forward, 10);
        assert!(verified(&forward), "{} forward", case.name);
        let reverse = observed_transition(&mut actor, case, true, case.reverse, 210);
        assert!(verified(&reverse), "{} direct reverse", case.name);
        assert_eq!(sends(&reverse), 0);
        assert_eq!(
            actor
                .snapshot(PresetTarget::EditBuffer)
                .unwrap()
                .raw
                .payload(),
            case.reverse
        );
        assert_eq!(
            raw(case.before).records()[17],
            raw(case.reverse).records()[17]
        );
        assert!(
            dependent_globals_match(&raw(case.globals), &raw(case.undo_globals), Some(1)).unwrap()
        );
        assert_eq!(raw(case.before).records()[17], raw(case.undo).records()[17]);
        assert_eq!(
            case.controls.len(),
            if case.name == "amp-auto-cab-on" { 6 } else { 4 }
        );
    }
}

#[test]
fn changed_hidden_byte_in_actual_reverse_stays_indeterminate_without_followups() {
    for case in OBSERVED_CASES {
        let mut actor = ready_captured(case.forward_globals, case.forward);
        let value = changed(&raw(case.reverse), 3 + usize::from(case.module), 64, 0x45);
        let result = observed_transition(&mut actor, case, true, value.payload(), 10);
        assert!(failed(&result, true), "{} reverse negative", case.name);
        assert!(!verified(&result));
        assert_eq!(sends(&result), 0);
        actor.tick(Duration::from_secs(10));
        assert_eq!(sends(&actions(&mut actor)), 0);
    }
}
