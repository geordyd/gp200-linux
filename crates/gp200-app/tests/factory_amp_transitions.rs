// Private capture acceptance suite; excluded from public source builds.
// Run with RUSTFLAGS="--cfg gp200_private_evidence" only in the retained lab checkout.
#![cfg(gp200_private_evidence)]

//! Synthetic factory candidate replays; no physical acceptance.
use gp200_app::*;
use gp200_catalog::Catalog;
use gp200_domain::*;
use serde_json::Value;
use std::sync::Arc;
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
fn snapshot(value: RawTransfer, revision: u64) -> Arc<PresetSnapshot> {
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(value),
        SnapshotSource::Hardware {
            epoch: SessionEpoch(1),
        },
        Some(PresetTarget::EditBuffer),
        Revision(revision),
    ))
}
fn global(value: RawTransfer, revision: u64) -> Arc<DeviceGlobalState> {
    Arc::new(
        DeviceGlobalState::from_transfer(Revision(revision), SessionEpoch(1), value, vec![])
            .unwrap(),
    )
}
fn readback(
    app: &mut AppController,
    stamp: RequestStamp,
    value: RawTransfer,
    revision: u64,
) -> Vec<Effect> {
    app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
        stamp,
        target: PresetTarget::EditBuffer,
        snapshot: snapshot(value, revision),
    }))
}
fn globals(
    app: &mut AppController,
    stamp: RequestStamp,
    value: RawTransfer,
    revision: u64,
) -> Vec<Effect> {
    app.dispatch(AppInput::Device(DeviceEvent::GlobalStateReceived {
        stamp,
        state: global(value, revision),
    }))
}
#[cfg(feature = "factory-amp-trials")]
fn stage(app: &mut AppController, stamp: RequestStamp, stage: EditStage) -> Vec<Effect> {
    app.dispatch(AppInput::Device(DeviceEvent::EditProgress { stamp, stage }))
}
fn connected(case: &Value, with_globals: bool) -> AppController {
    let mut app = AppController::new(Catalog::embedded().unwrap(), AppPreferences::default());
    let effects = app.dispatch(AppInput::Ui(UiIntent::Connect(DeviceSelection {
        input_id: "fixture-in".into(),
        output_id: "fixture-out".into(),
        fingerprint_hint: None,
    })));
    let Effect::Device(DeviceCommand::Connect { intent, .. }) = effects[0] else {
        panic!("{effects:?}")
    };
    app.dispatch(AppInput::Device(DeviceEvent::SessionChanged {
        epoch: SessionEpoch(1),
        identity: DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Hardware,
            firmware: None,
            display_name: "Synthetic factory candidate replay".into(),
        },
        capabilities: gp200_transport::verified_capabilities(),
    }));
    app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
        epoch: SessionEpoch(1),
        state: ConnectionState::Ready,
    }));
    readback(
        &mut app,
        RequestStamp {
            epoch: SessionEpoch(1),
            intent,
        },
        raw(case["before"].as_str().unwrap()),
        1,
    );
    if with_globals {
        let effects = app.dispatch(AppInput::Ui(UiIntent::RefreshDeviceGlobals));
        let Effect::Device(DeviceCommand::ReadGlobals(stamp)) = effects[0] else {
            panic!("{effects:?}")
        };
        globals(&mut app, stamp, raw(case["globals"].as_str().unwrap()), 1);
    }
    app
}
fn apply(app: &mut AppController, model: u32) -> Vec<Effect> {
    app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: device_document_id(),
        patch: EditPatch::SetModel {
            module: ModuleId::new(3).unwrap(),
            model: ModelCode(model),
        },
    }))
}
#[cfg(feature = "factory-amp-trials")]
fn command(effects: &[Effect], case: &Value, target: u32) -> RequestStamp {
    let [
        Effect::Device(DeviceCommand::SubmitFactoryAmpModel {
            stamp,
            model,
            profile,
            before_globals,
            expected_auto_cab,
            ..
        }),
    ] = effects
    else {
        panic!("{effects:?}")
    };
    assert_eq!(model.0, target);
    assert_eq!(profile.name(), case["name"].as_str().unwrap());
    assert_eq!(before_globals.epoch, stamp.epoch);
    assert_eq!(*expected_auto_cab, case["auto_cab"].as_bool().unwrap());
    *stamp
}
#[cfg(feature = "factory-amp-trials")]
fn finish(
    app: &mut AppController,
    stamp: RequestStamp,
    value: RawTransfer,
    case: &Value,
    revision: u64,
) -> Vec<Effect> {
    stage(app, stamp, EditStage::Sent);
    stage(app, stamp, EditStage::AwaitingReadback);
    assert!(readback(app, stamp, value, revision).is_empty());
    assert!(globals(app, stamp, raw(case["globals"].as_str().unwrap()), revision).is_empty());
    stage(app, stamp, EditStage::Verified)
}

#[test]
fn missing_globals_never_changes_draft_or_history() {
    let case = cases().remove(0);
    let mut app = connected(&case, false);
    let before = app.active_document().unwrap().draft.clone();
    assert!(apply(&mut app, 0x07000003).is_empty());
    assert_eq!(app.active_document().unwrap().draft, before);
    assert!(app.active_document().unwrap().undo.past.is_empty());
}

#[cfg(feature = "factory-amp-trials")]
#[test]
fn original_values_that_cannot_be_undone_never_change_local_history() {
    for bits in [0x7fc00001u32, 101f32.to_bits()] {
        let mut case = cases().remove(1);
        let mut records = raw(case["before"].as_str().unwrap()).records().to_vec();
        records[6].body[8..12].copy_from_slice(&bits.to_le_bytes());
        let before = RawTransfer::from_records(0x12, &records).unwrap();
        case["before"] = before
            .payload()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
            .into();
        let mut app = connected(&case, true);
        let draft = app.active_document().unwrap().draft.clone();
        assert!(apply(&mut app, 0x07000003).is_empty());
        assert_eq!(app.active_document().unwrap().draft, draft);
        assert!(app.active_document().unwrap().undo.past.is_empty());
    }
}
#[cfg(not(feature = "factory-amp-trials"))]
#[test]
fn ordinary_controller_rejects_all_130_candidates_without_history_or_device_effects() {
    for case in cases() {
        let mut app = connected(&case, true);
        let before = app.active_document().unwrap().draft.clone();
        assert!(apply(&mut app, case["target"].as_u64().unwrap() as u32).is_empty());
        assert_eq!(app.active_document().unwrap().draft, before);
        assert!(app.active_document().unwrap().undo.past.is_empty());
    }
}
#[cfg(feature = "factory-amp-trials")]
#[test]
fn every_exact_candidate_has_one_history_step_and_completely_reversible_declared_undo() {
    for case in cases() {
        let mut app = connected(&case, true);
        let original = app.active_document().unwrap().draft.clone();
        let effects = apply(&mut app, case["target"].as_u64().unwrap() as u32);
        let forward = command(&effects, &case, case["target"].as_u64().unwrap() as u32);
        assert_eq!(
            app.active_document().unwrap().undo.past.as_slice(),
            std::slice::from_ref(&original)
        );
        assert!(
            finish(
                &mut app,
                forward,
                raw(case["forward"].as_str().unwrap()),
                &case,
                2
            )
            .is_empty()
        );
        assert!(
            app.state()
                .document_status(&device_document_id())
                .unwrap()
                .verified_in_buffer
        );
        let effects = app.dispatch(AppInput::Ui(UiIntent::Undo(device_document_id())));
        let reverse = command(&effects, &case, FACTORY_AMP_BASELINE.0);
        let mut current = raw(case["reverse"].as_str().unwrap());
        let mut effects = finish(&mut app, reverse, current.clone(), &case, 3);
        let restores = case["restores"].as_array().unwrap();
        let mut index = 0;
        while !effects.is_empty() {
            let [
                Effect::Device(DeviceCommand::SubmitEdit {
                    stamp,
                    patch:
                        EditPatch::SetParameter {
                            module,
                            model,
                            model_index,
                            parameter,
                            value,
                        },
                    ..
                }),
            ] = effects.as_slice()
            else {
                panic!("{effects:?}")
            };
            let expected = &restores[index];
            assert_eq!(module.get(), expected["module"].as_u64().unwrap() as u8);
            assert_eq!(model.0, expected["model"].as_u64().unwrap() as u32);
            assert_eq!(
                model_index.0,
                expected["model_index"].as_u64().unwrap() as u32
            );
            assert_eq!(
                parameter.get(),
                expected["parameter"].as_u64().unwrap() as u8
            );
            assert_eq!(value.0, expected["raw_bits"].as_u64().unwrap() as u32);
            let stamp = *stamp;
            let mut records = current.records().to_vec();
            let offset = 8 + usize::from(parameter.get()) * 4;
            records[3 + usize::from(module.get())].body[offset..offset + 4]
                .copy_from_slice(&value.0.to_le_bytes());
            current = RawTransfer::from_records(0x12, &records).unwrap();
            stage(&mut app, stamp, EditStage::Sent);
            assert!(readback(&mut app, stamp, current.clone(), 4 + index as u64).is_empty());
            effects = stage(&mut app, stamp, EditStage::Verified);
            index += 1;
            assert!(index <= 9);
        }
        assert_eq!(index, restores.len());
        assert!(factory_amp_readback_matches(
            &raw(case["before"].as_str().unwrap()),
            &current
        ));
        assert!(
            app.active_document()
                .unwrap()
                .draft
                .semantically_equal(&original)
        );
        assert!(app.active_document().unwrap().undo.past.is_empty());
        assert_eq!(app.active_document().unwrap().undo.future.len(), 1);
        assert!(!app.device_operation_pending());
    }
}
#[cfg(feature = "factory-amp-trials")]
#[test]
fn both_complete_readback_orders_require_owned_current_and_globals_before_completion() {
    for globals_first in [false, true] {
        let case = cases().remove(1);
        let mut app = connected(&case, true);
        let effects = apply(&mut app, 0x07000003);
        let stamp = command(&effects, &case, 0x07000003);
        stage(&mut app, stamp, EditStage::Sent);
        assert!(stage(&mut app, stamp, EditStage::Verified).is_empty());
        let unrelated = RequestStamp {
            intent: IntentId(stamp.intent.0 + 200),
            ..stamp
        };
        assert!(
            globals(
                &mut app,
                unrelated,
                raw(case["globals"].as_str().unwrap()),
                2
            )
            .is_empty()
        );
        assert!(app.device_operation_pending());
        if globals_first {
            assert!(globals(&mut app, stamp, raw(case["globals"].as_str().unwrap()), 2).is_empty());
            assert!(app.device_operation_pending());
            readback(&mut app, stamp, raw(case["forward"].as_str().unwrap()), 2);
        } else {
            assert!(
                readback(&mut app, stamp, raw(case["forward"].as_str().unwrap()), 2).is_empty()
            );
            assert!(app.device_operation_pending());
            globals(&mut app, stamp, raw(case["globals"].as_str().unwrap()), 2);
        }
        assert!(!app.device_operation_pending());
    }
}
#[cfg(feature = "factory-amp-trials")]
#[test]
fn matching_code_with_hidden_or_unaffected_mismatch_cannot_complete_even_after_verified_signal() {
    for (record, offset) in [(6, 64), (3, 8), (8, 1), (17, 2)] {
        let case = cases().remove(1);
        let mut app = connected(&case, true);
        let effects = apply(&mut app, 0x07000003);
        let stamp = command(&effects, &case, 0x07000003);
        stage(&mut app, stamp, EditStage::Sent);
        let mut records = raw(case["forward"].as_str().unwrap()).records().to_vec();
        records[record].body[offset] ^= 1;
        assert!(
            readback(
                &mut app,
                stamp,
                RawTransfer::from_records(0x12, &records).unwrap(),
                2
            )
            .is_empty()
        );
        globals(&mut app, stamp, raw(case["globals"].as_str().unwrap()), 2);
        assert!(stage(&mut app, stamp, EditStage::Verified).is_empty());
        assert!(app.device_operation_pending());
        assert!(
            stage(
                &mut app,
                stamp,
                EditStage::Indeterminate(ServiceError::new(
                    ErrorKind::Indeterminate,
                    "synthetic wrong whole readback"
                ))
            )
            .is_empty()
        );
        assert!(stage(&mut app, stamp, EditStage::Verified).is_empty());
        assert!(app.dispatch(AppInput::Tick(20000)).is_empty());
    }
}

#[cfg(feature = "factory-amp-trials")]
#[test]
fn equal_review_revision_cannot_complete_a_candidate_postflight() {
    let case = cases().remove(1);
    let mut app = connected(&case, true);
    let effects = apply(&mut app, 0x07000003);
    let stamp = command(&effects, &case, 0x07000003);
    stage(&mut app, stamp, EditStage::Sent);
    assert!(readback(&mut app, stamp, raw(case["forward"].as_str().unwrap()), 1).is_empty());
    assert!(globals(&mut app, stamp, raw(case["globals"].as_str().unwrap()), 2).is_empty());
    assert!(stage(&mut app, stamp, EditStage::Verified).is_empty());
    assert!(app.device_operation_pending());
    assert!(readback(&mut app, stamp, raw(case["forward"].as_str().unwrap()), 2).is_empty());
    assert!(!app.device_operation_pending());
}
#[cfg(feature = "factory-amp-trials")]
#[test]
fn excluded_targets_cannot_get_a_factory_command_and_busy_candidates_leave_history_unchanged() {
    let case = cases().remove(1);
    for target in [0x0700004a, 0x0700002c, 0x0700002f, 0x0700005a, 0x07100000] {
        let mut app = connected(&case, true);
        assert!(FactoryAmpProfileId::new(ModelCode(target), true).is_err());
        assert!(
            gp200_transport::hardware_model_blocked_reason_with_globals(
                app.state().confirmed_buffer.as_ref().unwrap(),
                app.state().device_globals.as_deref(),
                ModuleId::new(3).unwrap(),
                ModelCode(target),
            )
            .is_some()
        );
        assert!(apply(&mut app, target).iter().all(|effect| !matches!(
            effect,
            Effect::Device(DeviceCommand::SubmitFactoryAmpModel { .. })
        )));
        // Generic model edits keep the existing reversible draft behavior.
        // The actor still rejects these unverified hardware transitions.
    }
    let mut app = connected(&case, true);
    let before = app.active_document().unwrap().draft.clone();
    app.dispatch(AppInput::Ui(UiIntent::RefreshDeviceGlobals));
    assert!(apply(&mut app, 0x07000003).is_empty());
    assert_eq!(app.active_document().unwrap().draft, before);
}
