// Private capture acceptance suite; excluded from public source builds.
// Run with RUSTFLAGS="--cfg gp200_private_evidence" only in the retained lab checkout.
#![cfg(gp200_private_evidence)]

//! Controller ordering and Undo projection tests; reverse fixtures here are synthetic.
use gp200_app::*;
use gp200_catalog::Catalog;
use gp200_catalog::ModelDefaultPlan;
use gp200_domain::*;
use std::sync::Arc;

const BEFORE: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-root/amp-auto-cab-on/current-before.payload.bin"
);
const GLOBALS: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-root/amp-auto-cab-on/globals-before.payload.bin"
);
const OFF: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-root/amp-auto-cab-off/globals-auto-cab-setup.payload.bin"
);
const ON_AFTER: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-root/amp-auto-cab-on/current-model.payload.bin"
);
const OFF_AFTER: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-root/amp-auto-cab-off/current-model.payload.bin"
);
const WAH_AFTER: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-root/wah-assigned/current-model.payload.bin"
);
fn raw(payload: &[u8]) -> RawTransfer {
    RawTransfer::from_payload(0x12, payload.to_vec()).unwrap()
}
fn snapshot(payload: &[u8], revision: u64) -> Arc<PresetSnapshot> {
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(raw(payload)),
        SnapshotSource::Hardware {
            epoch: SessionEpoch(1),
        },
        Some(PresetTarget::EditBuffer),
        Revision(revision),
    ))
}
fn global(payload: &[u8], revision: u64) -> Arc<DeviceGlobalState> {
    Arc::new(
        DeviceGlobalState::from_transfer(Revision(revision), SessionEpoch(1), raw(payload), vec![])
            .unwrap(),
    )
}
fn select(module: u8, model: u32) -> EditPatch {
    EditPatch::SetModel {
        module: ModuleId::new(module).unwrap(),
        model: ModelCode(model),
    }
}
fn apply(app: &mut AppController, patch: EditPatch) -> Vec<Effect> {
    app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: device_document_id(),
        patch,
    }))
}
fn readback(
    app: &mut AppController,
    stamp: RequestStamp,
    snapshot: Arc<PresetSnapshot>,
) -> Vec<Effect> {
    app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
        stamp,
        target: PresetTarget::EditBuffer,
        snapshot,
    }))
}
fn globals(
    app: &mut AppController,
    stamp: RequestStamp,
    state: Arc<DeviceGlobalState>,
) -> Vec<Effect> {
    app.dispatch(AppInput::Device(DeviceEvent::GlobalStateReceived {
        stamp,
        state,
    }))
}
fn stage(app: &mut AppController, stamp: RequestStamp, stage: EditStage) -> Vec<Effect> {
    app.dispatch(AppInput::Device(DeviceEvent::EditProgress { stamp, stage }))
}
fn connected(payload: Option<&[u8]>) -> AppController {
    connected_captured(BEFORE, payload)
}
fn connected_captured(before: &[u8], payload: Option<&[u8]>) -> AppController {
    let mut app = AppController::new(Catalog::embedded().unwrap(), AppPreferences::default());
    let effect = app.dispatch(AppInput::Ui(UiIntent::Connect(DeviceSelection {
        input_id: "fixture-in".into(),
        output_id: "fixture-out".into(),
        fingerprint_hint: None,
    })));
    let Effect::Device(DeviceCommand::Connect { intent, .. }) = effect[0] else {
        panic!()
    };
    app.dispatch(AppInput::Device(DeviceEvent::SessionChanged {
        epoch: SessionEpoch(1),
        identity: DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Hardware,
            firmware: None,
            display_name: "Actual capture replay".into(),
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
        snapshot(before, 1),
    );
    if let Some(payload) = payload {
        let effects = app.dispatch(AppInput::Ui(UiIntent::RefreshDeviceGlobals));
        let Effect::Device(DeviceCommand::ReadGlobals(stamp)) = effects[0] else {
            panic!("{effects:?}")
        };
        globals(&mut app, stamp, global(payload, 1));
    }
    app
}

#[test]
fn hardware_missing_complete_globals_never_mutates_history_or_selects_model() {
    let mut app = connected(None);
    let before = app.active_document().unwrap().clone();
    assert!(apply(&mut app, select(3, 0x07000001)).is_empty());
    assert_eq!(app.active_document().unwrap().draft, before.draft);
    assert!(app.active_document().unwrap().undo.past.is_empty());
}

fn dependent(effects: &[Effect]) -> (RequestStamp, u8, u32, bool) {
    let [
        Effect::Device(DeviceCommand::SubmitDependentModel {
            stamp,
            module,
            model,
            before_globals,
            expected_auto_cab,
            ..
        }),
    ] = effects
    else {
        panic!("Expected bound dependent command: {effects:?}")
    };
    assert_eq!(before_globals.epoch, stamp.epoch);
    assert_eq!(
        dependent_auto_cab(before_globals.records().raw()).unwrap(),
        *expected_auto_cab
    );
    (*stamp, module.get(), model.0, *expected_auto_cab)
}
fn finish(
    app: &mut AppController,
    stamp: RequestStamp,
    value: Arc<PresetSnapshot>,
    global_revision: u64,
) -> Vec<Effect> {
    stage(app, stamp, EditStage::Sent);
    stage(app, stamp, EditStage::AwaitingReadback);
    assert!(readback(app, stamp, value).is_empty());
    let payload = app
        .state()
        .device_globals
        .as_ref()
        .unwrap()
        .records()
        .raw()
        .payload()
        .to_vec();
    assert!(globals(app, stamp, global(&payload, global_revision)).is_empty());
    stage(app, stamp, EditStage::Verified)
}
fn project_defaults(
    app: &AppController,
    current: &PresetSnapshot,
    modules: &[(u8, u32)],
    revision: u64,
) -> Arc<PresetSnapshot> {
    let mut records = current.raw.records().to_vec();
    for &(module, model) in modules {
        ModelDefaultPlan::from_definition(app.catalog().model_by_id(module, model).unwrap())
            .apply_to_module_body(&mut records[3 + usize::from(module)].body)
            .unwrap();
    }
    let raw = RawTransfer::from_records(0x12, &records).unwrap();
    snapshot(raw.payload(), revision)
}

#[test]
fn amp_and_coupled_cab_defaults_share_one_history_action_and_one_bound_command() {
    for (globals, after, coupled) in [(GLOBALS, ON_AFTER, true), (OFF, OFF_AFTER, false)] {
        let mut app = connected(Some(globals));
        let original = app.active_document().unwrap().draft.clone();
        let (stamp, module, target, auto_cab) = dependent(&apply(&mut app, select(3, 0x07000001)));
        assert_eq!((module, target, auto_cab), (3, 0x07000001, coupled));
        assert_eq!(
            app.active_document().unwrap().undo.past.as_slice(),
            std::slice::from_ref(&original)
        );
        assert_eq!(
            app.active_document()
                .unwrap()
                .draft
                .view
                .module(ModuleId::new(5).unwrap())
                .unwrap()
                .model
                .0,
            if coupled { 0x0a000002 } else { 0x0a000010 }
        );
        assert!(finish(&mut app, stamp, snapshot(after, 2), 2).is_empty());
        assert!(app.active_document().unwrap().conflicts.is_empty());
        assert!(
            app.state()
                .document_status(&device_document_id())
                .unwrap()
                .verified_in_buffer
        );
        assert_eq!(app.active_document().unwrap().undo.past.len(), 1);
    }
}

#[test]
fn early_verified_or_postflight_event_order_cannot_complete_without_both_actuals() {
    for globals_first in [false, true] {
        let mut app = connected(Some(GLOBALS));
        let (stamp, ..) = dependent(&apply(&mut app, select(3, 0x07000001)));
        stage(&mut app, stamp, EditStage::Sent);
        assert!(stage(&mut app, stamp, EditStage::Verified).is_empty());
        assert!(app.device_operation_pending());
        if globals_first {
            assert!(globals(&mut app, stamp, global(GLOBALS, 2)).is_empty());
            assert!(app.device_operation_pending());
            readback(&mut app, stamp, snapshot(ON_AFTER, 2));
        } else {
            assert!(readback(&mut app, stamp, snapshot(ON_AFTER, 2)).is_empty());
            assert!(app.device_operation_pending());
            globals(&mut app, stamp, global(GLOBALS, 2));
        }
        assert!(!app.device_operation_pending());
        assert!(
            app.state()
                .document_status(&device_document_id())
                .unwrap()
                .verified_in_buffer
        );
    }
}

#[test]
fn failed_preflight_refresh_keeps_pending_until_definite_failure_and_discards_cab_followups() {
    let mut app = connected(Some(GLOBALS));
    let (stamp, ..) = dependent(&apply(&mut app, select(3, 0x07000001)));
    assert!(readback(&mut app, stamp, snapshot(BEFORE, 2)).is_empty());
    assert!(app.device_operation_pending());
    assert!(globals(&mut app, stamp, global(OFF, 2)).is_empty());
    assert!(app.device_operation_pending());
    assert!(
        stage(
            &mut app,
            stamp,
            EditStage::Failed(ServiceError::new(
                ErrorKind::Conflict,
                "fresh preflight changed; no write"
            ))
        )
        .is_empty()
    );
    assert!(!app.device_operation_pending());
    assert!(stage(&mut app, stamp, EditStage::Verified).is_empty());
}

#[test]
fn mismatching_postflight_code_keeps_authority_until_indeterminate_and_does_not_flush() {
    let mut app = connected(Some(GLOBALS));
    let (stamp, ..) = dependent(&apply(&mut app, select(3, 0x07000001)));
    stage(&mut app, stamp, EditStage::Sent);
    assert!(readback(&mut app, stamp, snapshot(BEFORE, 2)).is_empty());
    assert!(app.device_operation_pending());
    globals(&mut app, stamp, global(GLOBALS, 2));
    assert!(app.device_operation_pending());
    assert!(
        stage(
            &mut app,
            stamp,
            EditStage::Indeterminate(ServiceError::new(
                ErrorKind::Indeterminate,
                "whole model readback mismatch"
            ))
        )
        .is_empty()
    );
    assert!(matches!(
        app.active_document().unwrap().pending[&stamp.intent].stage,
        EditStage::Indeterminate(_)
    ));
    assert!(stage(&mut app, stamp, EditStage::Verified).is_empty());
    assert!(apply(&mut app, select(3, 0x07000001)).is_empty());
}

#[test]
fn undo_restores_only_declared_original_bits_including_exact_fractional_position() {
    for (globals, module, target, after) in [
        (GLOBALS, 3, 0x07000001, ON_AFTER),
        (OFF, 3, 0x07000001, OFF_AFTER),
        (GLOBALS, 1, 0x05000001, WAH_AFTER),
    ] {
        let mut app = connected(Some(globals));
        let original = app.state().confirmed_buffer.clone().unwrap();
        let previous = original
            .view
            .module(ModuleId::new(module).unwrap())
            .unwrap()
            .model
            .0;
        let (stamp, ..) = dependent(&apply(&mut app, select(module, target)));
        finish(&mut app, stamp, snapshot(after, 2), 2);
        let (undo, actual_module, actual_target, _) =
            dependent(&app.dispatch(AppInput::Ui(UiIntent::Undo(device_document_id()))));
        assert_eq!((actual_module, actual_target), (module, previous));
        let mut modules = vec![(module, previous)];
        if module == 3 && globals == GLOBALS {
            modules.push((5, 0x0a000010));
        }
        let mut current = project_defaults(&app, &snapshot(after, 2), &modules, 3);
        let mut effects = finish(&mut app, undo, current.clone(), 3);
        let mut controls = 0;
        while !effects.is_empty() {
            let [Effect::Device(DeviceCommand::SubmitEdit { stamp, patch, .. })] =
                effects.as_slice()
            else {
                panic!("Unexpected Undo followup {effects:?}")
            };
            let EditPatch::SetParameter {
                module,
                model,
                parameter,
                ..
            } = patch
            else {
                panic!("Undo may restore declared controls only: {patch:?}")
            };
            assert!(
                app.catalog()
                    .model_by_id(module.get(), model.0)
                    .unwrap()
                    .parameter(parameter.get())
                    .is_some()
            );
            let stamp = *stamp;
            let patch = patch.clone();
            let mut draft = PresetDraft::from_snapshot(&current);
            draft.apply(&patch).unwrap();
            current = Arc::new(
                draft
                    .snapshot(
                        &current,
                        SnapshotSource::Hardware {
                            epoch: SessionEpoch(1),
                        },
                        Revision(current.revision.0 + 1),
                    )
                    .unwrap(),
            );
            readback(&mut app, stamp, current.clone());
            effects = stage(&mut app, stamp, EditStage::Verified);
            controls += 1;
            assert!(controls < 16);
        }
        assert!(controls > 0);
        for (id, code) in modules {
            let old = original.view.module(ModuleId::new(id).unwrap()).unwrap();
            let restored = current.view.module(ModuleId::new(id).unwrap()).unwrap();
            for control in &app.catalog().model_by_id(id, code).unwrap().parameters {
                assert_eq!(
                    old.parameters[control.parameter_id as usize],
                    restored.parameters[control.parameter_id as usize]
                );
            }
            for slot in 0..15 {
                assert_eq!(
                    old.parameters[slot], restored.parameters[slot],
                    "hidden slot {slot}"
                );
            }
        }
        assert_eq!(current.raw.records()[17], original.raw.records()[17]);
        assert!(app.active_document().unwrap().conflicts.is_empty());
        assert!(
            app.state()
                .document_status(&device_document_id())
                .unwrap()
                .verified_in_buffer
        );
    }
}

#[test]
fn unrelated_pending_read_blocks_dependent_local_history_until_its_global_authority_is_fresh() {
    let mut app = connected(Some(GLOBALS));
    let original = app.active_document().unwrap().draft.clone();
    app.dispatch(AppInput::Ui(UiIntent::RefreshDeviceGlobals));
    assert!(apply(&mut app, select(3, 0x07000001)).is_empty());
    assert_eq!(app.active_document().unwrap().draft, original);
    assert!(app.active_document().unwrap().undo.past.is_empty());
}

#[test]
fn aborted_preflight_retains_local_history_but_explicit_refresh_never_replays_it() {
    let mut app = connected(Some(GLOBALS));
    let (stamp, ..) = dependent(&apply(&mut app, select(3, 0x07000001)));
    let desired = app.active_document().unwrap().draft.clone();
    let mut records = raw(BEFORE).records().to_vec();
    records[3 + 3].body[8..12].copy_from_slice(&46f32.to_le_bytes());
    let fresh = RawTransfer::from_records(0x12, &records).unwrap();
    assert!(readback(&mut app, stamp, snapshot(fresh.payload(), 2)).is_empty());
    assert_eq!(
        app.active_document()
            .unwrap()
            .draft
            .view
            .module(ModuleId::new(3).unwrap())
            .unwrap(),
        desired.view.module(ModuleId::new(3).unwrap()).unwrap()
    );
    stage(
        &mut app,
        stamp,
        EditStage::Failed(ServiceError::new(
            ErrorKind::Conflict,
            "external control changed before model send",
        )),
    );
    assert_eq!(app.active_document().unwrap().undo.past.len(), 1);
    let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
    let [Effect::Device(DeviceCommand::RefreshTarget { stamp: refresh, .. })] = effects.as_slice()
    else {
        panic!("{effects:?}")
    };
    assert!(readback(&mut app, *refresh, snapshot(fresh.payload(), 3)).is_empty());
    assert_eq!(
        app.active_document()
            .unwrap()
            .draft
            .view
            .module(ModuleId::new(3).unwrap())
            .unwrap()
            .model
            .0,
        0x07000001
    );
    assert!(stage(&mut app, stamp, EditStage::Verified).is_empty());
}

#[test]
fn later_cab_control_survives_the_coupled_model_readback_and_waits_for_globals() {
    let mut app = connected(Some(GLOBALS));
    let (stamp, ..) = dependent(&apply(&mut app, select(3, 0x07000001)));
    let model = app.catalog().model_by_id(5, 0x0a000002).unwrap();
    let patch = EditPatch::SetParameter {
        module: ModuleId::new(5).unwrap(),
        model: ModelCode(model.code),
        model_index: ModelIndex(u32::from(model.xml_index)),
        parameter: ParamId::new(1).unwrap(),
        value: F32Bits::from_f32(61.),
    };
    assert!(apply(&mut app, patch.clone()).is_empty());
    stage(&mut app, stamp, EditStage::Sent);
    assert!(readback(&mut app, stamp, snapshot(ON_AFTER, 2)).is_empty());
    assert!(stage(&mut app, stamp, EditStage::Verified).is_empty());
    assert!(app.active_document().unwrap().conflicts.is_empty());
    let effects = globals(&mut app, stamp, global(GLOBALS, 2));
    let [
        Effect::Device(DeviceCommand::SubmitEdit {
            patch: submitted, ..
        }),
    ] = effects.as_slice()
    else {
        panic!("{effects:?}")
    };
    assert_eq!(submitted, &patch);
}

#[test]
fn pending_undo_is_a_later_intent_and_never_reuses_the_forward_global_binding() {
    let mut app = connected(Some(GLOBALS));
    let original = app.active_document().unwrap().draft.clone();
    let (stamp, ..) = dependent(&apply(&mut app, select(3, 0x07000001)));
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::Undo(device_document_id())))
            .is_empty()
    );
    let effects = finish(&mut app, stamp, snapshot(ON_AFTER, 2), 2);
    let (undo, module, code, auto) = dependent(&effects);
    assert_ne!(undo, stamp);
    assert_eq!((module, code, auto), (3, 0x07000015, true));
    let [Effect::Device(DeviceCommand::SubmitDependentModel { before_globals, .. })] =
        effects.as_slice()
    else {
        panic!()
    };
    assert_eq!(before_globals.revision, Revision(2));
    assert_eq!(
        app.active_document().unwrap().draft.view.modules,
        original.view.modules
    );
    assert!(app.active_document().unwrap().conflicts.is_empty());
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

fn finish_observed(
    app: &mut AppController,
    stamp: RequestStamp,
    preset: &[u8],
    settings: &[u8],
    revision: u64,
) -> Vec<Effect> {
    stage(app, stamp, EditStage::Sent);
    stage(app, stamp, EditStage::AwaitingReadback);
    assert!(readback(app, stamp, snapshot(preset, revision)).is_empty());
    assert!(app.device_operation_pending());
    assert!(globals(app, stamp, global(settings, revision)).is_empty());
    stage(app, stamp, EditStage::Verified)
}

fn assert_whole_observed_preset(expected: &[u8], actual: &[u8]) {
    let expected = raw(expected);
    let actual = raw(actual);
    assert_eq!(expected.records().len(), actual.records().len());
    for (before, after) in expected.records().iter().zip(actual.records()) {
        assert_eq!(before.command, after.command);
        assert_eq!(before.body.len(), after.body.len());
        match before.command {
            2 | 8 => {
                assert_eq!(before.body[..15], after.body[..15]);
                assert_eq!(before.body[16..], after.body[16..]);
            }
            0x0f => {
                assert_eq!(before.body[..2], after.body[..2]);
                assert_eq!(before.body[4..], after.body[4..]);
            }
            _ => assert_eq!(before, after),
        }
    }
}

#[test]
fn observed_bidirectional_actor_captures_complete_one_undo_with_exact_original_bits() {
    for case in OBSERVED_CASES {
        let mut app = connected_captured(case.before, Some(case.globals));
        let original = app.active_document().unwrap().draft.clone();
        let (forward, module, target, _) =
            dependent(&apply(&mut app, select(case.module, case.target)));
        assert_eq!(
            (module, target),
            (case.module, case.target),
            "{}",
            case.name
        );
        assert_eq!(app.active_document().unwrap().undo.past.len(), 1);
        assert!(
            finish_observed(&mut app, forward, case.forward, case.forward_globals, 2).is_empty()
        );
        let (reverse, module, target, _) =
            dependent(&app.dispatch(AppInput::Ui(UiIntent::Undo(device_document_id()))));
        assert_ne!(forward, reverse);
        assert_eq!((module, target), (case.module, case.original));
        let mut effects = finish_observed(&mut app, reverse, case.reverse, case.reverse_globals, 3);
        for (index, &(payload, module, parameter, original_bits)) in
            case.controls.iter().enumerate()
        {
            let [Effect::Device(DeviceCommand::SubmitEdit { stamp, patch, .. })] =
                effects.as_slice()
            else {
                panic!("{} control {}: {effects:?}", case.name, index);
            };
            let EditPatch::SetParameter {
                module: actual_module,
                parameter: actual_parameter,
                value,
                ..
            } = patch
            else {
                panic!("Undo attempted a non-parameter followup: {patch:?}");
            };
            assert_eq!(actual_module.get(), module);
            assert_eq!(actual_parameter.get(), parameter);
            assert_eq!(value.0, original_bits);
            let stamp = *stamp;
            stage(&mut app, stamp, EditStage::Sent);
            assert!(readback(&mut app, stamp, snapshot(payload, index as u64 + 4)).is_empty());
            effects = stage(&mut app, stamp, EditStage::Verified);
        }
        assert!(effects.is_empty());
        assert!(!app.device_operation_pending());
        assert!(app.active_document().unwrap().undo.past.is_empty());
        assert_eq!(app.active_document().unwrap().undo.future.len(), 1);
        assert!(
            app.active_document()
                .unwrap()
                .draft
                .semantically_equal(&original)
        );
        assert!(app.active_document().unwrap().conflicts.is_empty());
        assert_whole_observed_preset(
            case.undo,
            app.state().confirmed_buffer.as_ref().unwrap().raw.payload(),
        );
        assert_whole_observed_preset(case.before, case.undo);
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
fn normal_reducer_rejects_wah_with_auto_cab_off_without_mutating_history() {
    let mut app = connected(Some(OFF));
    let original = app.active_document().unwrap().draft.clone();
    assert!(apply(&mut app, select(1, 0x05000001)).is_empty());
    assert_eq!(app.active_document().unwrap().draft, original);
    assert!(app.active_document().unwrap().undo.past.is_empty());
}

// These payloads are the negative NORMAL physical trial, including the complete
// unchanged Current replies after the device's observed model/Quick echoes.
const NORMAL_BEFORE: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-normal-root/amp-auto-cab-off/rust/own-fresh-current-before.payload.bin"
);
const NORMAL_GLOBALS: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-normal-root/amp-auto-cab-off/rust/own-fresh-globals-before.payload.bin"
);
const NORMAL_FORWARD: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-normal-root/amp-auto-cab-off/rust/forward-36-4.preset.payload.bin"
);
const NORMAL_FORWARD_GLOBALS: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-normal-root/amp-auto-cab-off/rust/forward-40-4.globals.payload.bin"
);
const NORMAL_FORWARD_REFRESH: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-normal-root/amp-auto-cab-off/rust/forward-44-5.preset.payload.bin"
);
const NORMAL_REVERSE: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-normal-root/amp-auto-cab-off/rust/undo-60-6.preset.payload.bin"
);
const NORMAL_REVERSE_GLOBALS: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-normal-root/amp-auto-cab-off/rust/undo-63-6.globals.payload.bin"
);
const NORMAL_REVERSE_REFRESH: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-normal-root/amp-auto-cab-off/rust/undo-67-7.preset.payload.bin"
);

fn model_echoes(app: &mut AppController, model: u32) {
    // Exact observed record bodies from normal trial trace/rx: cmd16 then the
    // three Quick records. They remain context-free invalidation, never ACKs.
    let mut body = vec![3, 0, 0, 0];
    body.extend(model.to_le_bytes());
    for (command, body) in [
        (0x16, body),
        (0x10, vec![0, 0, 0, 0]),
        (0x10, vec![1, 2, 0, 0]),
        (0x10, vec![2, 11, 0, 0]),
    ] {
        assert!(
            app.dispatch(AppInput::Device(DeviceEvent::IncomingNotification {
                epoch: SessionEpoch(1),
                raw: Arc::new(
                    RawTransfer::from_records(0x12, &[RawRecord { command, body }]).unwrap()
                ),
            }))
            .is_empty()
        );
    }
}

fn normal_reverse_waiting_for_recovery() -> (AppController, RequestStamp, PresetDraft) {
    let mut app = connected_captured(NORMAL_BEFORE, Some(NORMAL_GLOBALS));
    // The real probe's extra complete baseline read precedes intent4 forward.
    let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
    let [Effect::Device(DeviceCommand::RefreshTarget { stamp, .. })] = effects.as_slice() else {
        panic!("{effects:?}")
    };
    assert!(readback(&mut app, *stamp, snapshot(NORMAL_BEFORE, 2)).is_empty());
    let original = app.active_document().unwrap().draft.clone();
    let (forward, ..) = dependent(&apply(&mut app, select(3, 0x07000001)));
    assert_eq!(forward.intent, IntentId(4));
    stage(&mut app, forward, EditStage::Sent);
    stage(&mut app, forward, EditStage::AwaitingReadback);
    app.dispatch(AppInput::Tick(680));
    model_echoes(&mut app, 0x07000001);
    assert!(readback(&mut app, forward, snapshot(NORMAL_FORWARD, 3)).is_empty());
    assert!(globals(&mut app, forward, global(NORMAL_FORWARD_GLOBALS, 2)).is_empty());
    assert!(stage(&mut app, forward, EditStage::Verified).is_empty());
    let effects = app.dispatch(AppInput::Tick(930));
    let [Effect::Device(DeviceCommand::RefreshTarget { stamp, .. })] = effects.as_slice() else {
        panic!("{effects:?}")
    };
    assert_eq!(stamp.intent, IntentId(5));
    assert!(readback(&mut app, *stamp, snapshot(NORMAL_FORWARD_REFRESH, 4)).is_empty());
    assert_eq!(app.active_document().unwrap().undo.past.len(), 1);
    let (reverse, ..) =
        dependent(&app.dispatch(AppInput::Ui(UiIntent::Undo(device_document_id()))));
    assert_eq!(reverse.intent, IntentId(6));
    stage(&mut app, reverse, EditStage::Sent);
    stage(&mut app, reverse, EditStage::AwaitingReadback);
    app.dispatch(AppInput::Tick(1289));
    model_echoes(&mut app, 0x07000015);
    assert!(readback(&mut app, reverse, snapshot(NORMAL_REVERSE, 5)).is_empty());
    assert!(globals(&mut app, reverse, global(NORMAL_REVERSE_GLOBALS, 3)).is_empty());
    assert!(stage(&mut app, reverse, EditStage::Verified).is_empty());
    let effects = app.dispatch(AppInput::Tick(1532));
    let [Effect::Device(DeviceCommand::RefreshTarget { stamp, .. })] = effects.as_slice() else {
        panic!("{effects:?}")
    };
    assert_eq!(stamp.intent, IntentId(7));
    (app, *stamp, original)
}

#[test]
fn normal_physical_event_order_resumes_only_verified_history_restores_after_unchanged_recovery() {
    let (mut app, recovery, original) = normal_reverse_waiting_for_recovery();
    let baseline = app.active_document().unwrap().baseline.clone();
    let revision = app.active_document().unwrap().local_revision;
    let mut effects = readback(&mut app, recovery, snapshot(NORMAL_REVERSE_REFRESH, 6));
    let case = &OBSERVED_CASES[0];
    for (index, &(payload, module, parameter, bits)) in case.controls.iter().enumerate() {
        let [Effect::Device(DeviceCommand::SubmitEdit { stamp, patch, .. })] = effects.as_slice()
        else {
            panic!(
                "Restoration {index} disappeared after verified reverse + unchanged recovery: {effects:?}"
            )
        };
        assert!(
            matches!(patch, EditPatch::SetParameter { module: m, parameter: p, value, .. }
            if m.get() == module && p.get() == parameter && value.0 == bits)
        );
        let stamp = *stamp;
        stage(&mut app, stamp, EditStage::Sent);
        assert!(readback(&mut app, stamp, snapshot(payload, 7 + index as u64)).is_empty());
        effects = stage(&mut app, stamp, EditStage::Verified);
    }
    assert!(effects.is_empty());
    assert!(!app.device_operation_pending());
    assert!(!app.external_reconciliation_pending());
    assert!(Arc::ptr_eq(
        &baseline,
        &app.active_document().unwrap().baseline
    ));
    assert_eq!(app.active_document().unwrap().local_revision, revision);
    assert!(app.active_document().unwrap().undo.past.is_empty());
    assert_eq!(app.active_document().unwrap().undo.future.len(), 1);
    assert!(
        app.active_document()
            .unwrap()
            .draft
            .semantically_equal(&original)
    );
    assert_whole_observed_preset(
        NORMAL_BEFORE,
        app.state().confirmed_buffer.as_ref().unwrap().raw.payload(),
    );
}

fn synthetic_hint(app: &mut AppController, command: u16, body: Vec<u8>) {
    // Deliberately synthetic receive event; only the negative normal trial's
    // cmd16/Quick sequence in model_echoes() is an observed notification here.
    assert!(
        app.dispatch(AppInput::Device(DeviceEvent::IncomingNotification {
            epoch: SessionEpoch(1),
            raw: Arc::new(RawTransfer::from_records(0x12, &[RawRecord { command, body }]).unwrap()),
        }))
        .is_empty()
    );
}

#[test]
fn every_accepted_history_chain_waits_for_complete_current_and_globals_after_synthetic_echoes() {
    for case in OBSERVED_CASES {
        let mut app = connected_captured(case.before, Some(case.globals));
        let original = app.active_document().unwrap().draft.clone();
        let (forward, ..) = dependent(&apply(&mut app, select(case.module, case.target)));
        assert!(
            finish_observed(&mut app, forward, case.forward, case.forward_globals, 2).is_empty()
        );
        let (reverse, ..) =
            dependent(&app.dispatch(AppInput::Ui(UiIntent::Undo(device_document_id()))));
        stage(&mut app, reverse, EditStage::Sent);
        stage(&mut app, reverse, EditStage::AwaitingReadback);
        synthetic_hint(&mut app, 0x15, vec![case.module, 1, 0, 0]);
        assert!(readback(&mut app, reverse, snapshot(case.reverse, 3)).is_empty());
        assert!(globals(&mut app, reverse, global(case.reverse_globals, 3)).is_empty());
        assert!(stage(&mut app, reverse, EditStage::Verified).is_empty());
        let effects = app.dispatch(AppInput::Tick(100));
        let [Effect::Device(DeviceCommand::RefreshTarget { stamp, .. })] = effects.as_slice()
        else {
            panic!("{effects:?}")
        };
        let mut effects = readback(&mut app, *stamp, snapshot(case.reverse, 4));
        for (index, &(payload, module, parameter, bits)) in case.controls.iter().enumerate() {
            let [Effect::Device(DeviceCommand::SubmitEdit { stamp, patch, .. })] =
                effects.as_slice()
            else {
                panic!("{} restore {index}: {effects:?}", case.name)
            };
            assert!(
                matches!(patch, EditPatch::SetParameter { module: m, parameter: p, value, .. } if m.get() == module && p.get() == parameter && value.0 == bits)
            );
            let stamp = *stamp;
            stage(&mut app, stamp, EditStage::Sent);
            // Synthetic cmd1008 invalidates both families. No ACK, guessed value,
            // or cached globals can allow the next never-submitted restore.
            synthetic_hint(&mut app, 0x1008, vec![0; 4]);
            assert!(readback(&mut app, stamp, snapshot(payload, 5 + index as u64 * 2)).is_empty());
            assert!(stage(&mut app, stamp, EditStage::Verified).is_empty());
            let refresh = app.dispatch(AppInput::Tick(1000 + index as u64 * 1200));
            let [Effect::Device(DeviceCommand::RefreshTarget { stamp, .. })] = refresh.as_slice()
            else {
                panic!("{refresh:?}")
            };
            assert!(readback(&mut app, *stamp, snapshot(payload, 6 + index as u64 * 2)).is_empty());
            assert!(app.external_reconciliation_pending());
            let refresh = app.dispatch(AppInput::Tick(1500 + index as u64 * 1200));
            let [Effect::Device(DeviceCommand::ReadGlobals(stamp))] = refresh.as_slice() else {
                panic!("{refresh:?}")
            };
            effects = globals(
                &mut app,
                *stamp,
                global(case.reverse_globals, 4 + index as u64),
            );
        }
        assert!(effects.is_empty());
        assert!(!app.device_operation_pending());
        assert!(!app.external_reconciliation_pending());
        assert!(
            app.active_document()
                .unwrap()
                .draft
                .semantically_equal(&original)
        );
        assert!(app.active_document().unwrap().undo.past.is_empty());
        assert_eq!(app.active_document().unwrap().undo.future.len(), 1);
        assert_whole_observed_preset(
            case.before,
            app.state().confirmed_buffer.as_ref().unwrap().raw.payload(),
        );
    }
}

#[test]
fn a_changed_whole_recovery_cancels_history_continuation_and_keeps_dirty_conflict_blocked() {
    let (mut app, recovery, original) = normal_reverse_waiting_for_recovery();
    let mut records = raw(NORMAL_REVERSE_REFRESH).records().to_vec();
    // An undeclared AMP float word changes: projected knob equality is not enough.
    records[6].body[64] ^= 1;
    let changed = RawTransfer::from_records(0x12, &records).unwrap();
    assert!(readback(&mut app, recovery, snapshot(changed.payload(), 6)).is_empty());
    assert!(app.external_conflict_ready(recovery.intent));
    assert!(matches!(
        app.operation_availability(OperationKind::SetParameter),
        OperationAvailability::Blocked { .. }
    ));
    assert!(
        app.active_document()
            .unwrap()
            .draft
            .semantically_equal(&original)
    );
    assert_eq!(app.active_document().unwrap().undo.future.len(), 1);
    assert!(app.dispatch(AppInput::Tick(10000)).is_empty());
    let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
    let [Effect::Device(DeviceCommand::RefreshTarget { stamp, .. })] = effects.as_slice() else {
        panic!("{effects:?}")
    };
    assert!(readback(&mut app, *stamp, snapshot(changed.payload(), 7)).is_empty());
    assert!(app.external_conflict_ready(stamp.intent));
    assert!(app.dispatch(AppInput::Tick(20000)).is_empty());
}

#[test]
fn local_edit_after_reverse_verification_expires_the_history_continuation_without_replay() {
    let (mut app, recovery, _) = normal_reverse_waiting_for_recovery();
    let model_index = ModelIndex(u32::from(
        app.catalog().model_by_id(3, 0x07000015).unwrap().xml_index,
    ));
    let before_revision = app.active_document().unwrap().local_revision;
    assert!(
        apply(
            &mut app,
            EditPatch::SetParameter {
                module: ModuleId::new(3).unwrap(),
                model: ModelCode(0x07000015),
                model_index,
                parameter: ParamId::new(0).unwrap(),
                value: F32Bits::from_f32(71.0),
            }
        )
        .is_empty()
    );
    let local = app.active_document().unwrap().clone();
    assert!(local.local_revision > before_revision);
    assert!(readback(&mut app, recovery, snapshot(NORMAL_REVERSE_REFRESH, 6)).is_empty());
    assert!(app.dispatch(AppInput::Tick(10000)).is_empty());
    assert_eq!(app.active_document().unwrap().draft, local.draft);
    assert_eq!(app.active_document().unwrap().undo.past, local.undo.past);
}

#[test]
fn stale_complete_read_cannot_resume_history_or_retire_its_freshness_barrier() {
    let (mut app, recovery, _) = normal_reverse_waiting_for_recovery();
    assert!(readback(&mut app, recovery, snapshot(NORMAL_REVERSE_REFRESH, 4)).is_empty());
    assert!(app.external_reconciliation_pending());
    assert!(matches!(
        app.operation_availability(OperationKind::SetParameter),
        OperationAvailability::Blocked { .. }
    ));
    assert!(app.dispatch(AppInput::Tick(20000)).is_empty());
    let effects = readback(&mut app, recovery, snapshot(NORMAL_REVERSE_REFRESH, 6));
    assert!(matches!(
        effects.as_slice(),
        [Effect::Device(DeviceCommand::SubmitEdit {
            patch: EditPatch::SetParameter { .. },
            ..
        })]
    ));
}

#[test]
fn failed_recovery_or_uncertain_restore_cancels_the_entire_history_chain_without_resending() {
    for fail_during_restore in [false, true] {
        let (mut app, recovery, _) = normal_reverse_waiting_for_recovery();
        let failed = if fail_during_restore {
            let effects = readback(&mut app, recovery, snapshot(NORMAL_REVERSE_REFRESH, 6));
            let [Effect::Device(DeviceCommand::SubmitEdit { stamp, .. })] = effects.as_slice()
            else {
                panic!("{effects:?}")
            };
            let stamp = *stamp;
            stage(&mut app, stamp, EditStage::Sent);
            assert!(
                stage(
                    &mut app,
                    stamp,
                    EditStage::Indeterminate(ServiceError::new(
                        ErrorKind::Indeterminate,
                        "synthetic lost restore readback"
                    ))
                )
                .is_empty()
            );
            stamp
        } else {
            assert!(
                app.dispatch(AppInput::Device(DeviceEvent::OperationFailed {
                    stamp: recovery,
                    error: ServiceError::new(ErrorKind::Timeout, "synthetic failed recovery"),
                }))
                .is_empty()
            );
            recovery
        };
        assert!(stage(&mut app, failed, EditStage::Verified).is_empty());
        assert!(app.dispatch(AppInput::Tick(20000)).is_empty());
        let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
        let [Effect::Device(DeviceCommand::RefreshTarget { stamp, .. })] = effects.as_slice()
        else {
            panic!("{effects:?}")
        };
        assert!(readback(&mut app, *stamp, snapshot(NORMAL_REVERSE_REFRESH, 7)).is_empty());
        assert!(app.dispatch(AppInput::Tick(30000)).is_empty());
        assert_eq!(app.active_document().unwrap().undo.future.len(), 1);
    }
}

#[test]
fn changed_global_runtime_word_cancels_waiting_history_after_both_complete_reads() {
    let (mut app, recovery, _) = normal_reverse_waiting_for_recovery();
    synthetic_hint(&mut app, 0x1010, vec![0; 4]);
    assert!(readback(&mut app, recovery, snapshot(NORMAL_REVERSE_REFRESH, 6)).is_empty());
    let effects = app.dispatch(AppInput::Tick(2100));
    let [Effect::Device(DeviceCommand::ReadGlobals(stamp))] = effects.as_slice() else {
        panic!("{effects:?}")
    };
    let mut records = raw(NORMAL_REVERSE_GLOBALS).records().to_vec();
    records[3].body[4] ^= 1; // Runtime third word remains strict.
    let changed = RawTransfer::from_records(0x12, &records).unwrap();
    assert!(globals(&mut app, *stamp, global(changed.payload(), 4)).is_empty());
    assert!(!app.external_reconciliation_pending());
    assert!(app.dispatch(AppInput::Tick(20000)).is_empty());
    let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
    let [Effect::Device(DeviceCommand::RefreshTarget { stamp, .. })] = effects.as_slice() else {
        panic!("{effects:?}")
    };
    assert!(readback(&mut app, *stamp, snapshot(NORMAL_REVERSE_REFRESH, 7)).is_empty());
}

#[test]
fn early_verified_restore_waits_for_its_owned_complete_readback_before_the_next_step() {
    let (mut app, recovery, _) = normal_reverse_waiting_for_recovery();
    let effects = readback(&mut app, recovery, snapshot(NORMAL_REVERSE_REFRESH, 6));
    let [Effect::Device(DeviceCommand::SubmitEdit { stamp, .. })] = effects.as_slice() else {
        panic!("{effects:?}")
    };
    let stamp = *stamp;
    stage(&mut app, stamp, EditStage::Sent);
    assert!(stage(&mut app, stamp, EditStage::Verified).is_empty());
    assert!(app.device_operation_pending());
    assert!(app.dispatch(AppInput::Tick(20000)).is_empty());
    let effects = readback(
        &mut app,
        stamp,
        snapshot(OBSERVED_CASES[0].controls[0].0, 7),
    );
    assert!(
        matches!(effects.as_slice(), [Effect::Device(DeviceCommand::SubmitEdit {
        patch: EditPatch::SetParameter { parameter, .. }, ..
    })] if parameter.get() == 1)
    );
}

#[test]
fn session_loss_cancels_waiting_history_and_late_recovery_never_sends_a_restore() {
    let (mut app, recovery, _) = normal_reverse_waiting_for_recovery();
    app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
        epoch: SessionEpoch(1),
        state: ConnectionState::Disconnected,
    }));
    assert!(readback(&mut app, recovery, snapshot(NORMAL_REVERSE_REFRESH, 6)).is_empty());
    assert!(app.dispatch(AppInput::Tick(20000)).is_empty());
    assert_eq!(app.state().connection, ConnectionState::Disconnected);
    assert!(!app.external_reconciliation_pending());
}

#[test]
fn unrelated_offline_history_does_not_cancel_the_bound_device_history_chain() {
    let (mut app, recovery, _) = normal_reverse_waiting_for_recovery();
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::NewOfflineFrom(snapshot(
            NORMAL_BEFORE,
            1
        ))))
        .is_empty()
    );
    let offline = app.active_document_id().unwrap().clone();
    let model_index = ModelIndex(u32::from(
        app.catalog().model_by_id(3, 0x07000015).unwrap().xml_index,
    ));
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
            document: offline.clone(),
            patch: EditPatch::SetParameter {
                module: ModuleId::new(3).unwrap(),
                model: ModelCode(0x07000015),
                model_index,
                parameter: ParamId::new(0).unwrap(),
                value: F32Bits::from_f32(71.0),
            },
        }))
        .is_empty()
    );
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::Undo(offline.clone())))
            .is_empty()
    );
    let effects = readback(&mut app, recovery, snapshot(NORMAL_REVERSE_REFRESH, 6));
    assert!(
        matches!(effects.as_slice(), [Effect::Device(DeviceCommand::SubmitEdit {
        patch: EditPatch::SetParameter { parameter, value, .. }, ..
    })] if parameter.get() == 0 && value.0 == OBSERVED_CASES[0].controls[0].3)
    );
    assert_eq!(app.active_document_id(), Some(&offline));
}
