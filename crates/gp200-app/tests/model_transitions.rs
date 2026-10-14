use gp200_app::*;
use gp200_catalog::Catalog;
use gp200_domain::*;
use std::sync::Arc;

#[path = "../../../tests/fixtures/model-breadth/cases.rs"]
mod breadth;

const BEFORE: &[u8] =
    include_bytes!("../../../tests/fixtures/model-transitions/dly-before.payload.bin");
const BBD: &[u8] =
    include_bytes!("../../../tests/fixtures/model-transitions/dly-after.payload.bin");

fn snapshot(payload: &[u8], revision: u64) -> Arc<PresetSnapshot> {
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_payload(0x12, payload.to_vec()).unwrap()),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(revision),
    ))
}
fn connected() -> AppController {
    connected_from(BEFORE)
}
fn connected_from(payload: &[u8]) -> AppController {
    let mut app = AppController::new(Catalog::embedded().unwrap(), AppPreferences::default());
    let effects = app.dispatch(AppInput::Ui(UiIntent::Connect(DeviceSelection {
        input_id: "test-in".into(),
        output_id: "test-out".into(),
        fingerprint_hint: None,
    })));
    let Effect::Device(DeviceCommand::Connect { intent, .. }) = effects[0] else {
        panic!()
    };
    app.dispatch(AppInput::Device(DeviceEvent::SessionChanged {
        epoch: SessionEpoch(1),
        identity: DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Simulator,
            firmware: None,
            display_name: "Fixture simulator".into(),
        },
        capabilities: Capabilities::simulator(),
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
        snapshot(payload, 0),
    );
    app
}

#[test]
fn captured_new_pairs_undo_restores_declared_bits_and_retains_actual_hidden_tail() {
    for &(name, module, baseline, target, before_payload, after_payload, returned_payload) in
        breadth::CASES
    {
        let mut app = connected_from(before_payload);
        let id = device_document_id();
        let original = app.active_document().unwrap().draft.clone();
        let (stamp, patch) = command(&apply(&mut app, &id, select(module, target)));
        assert_eq!(patch, select(module, target), "{name}");
        readback(&mut app, stamp, snapshot(after_payload, 1));
        assert!(verified(&mut app, stamp).is_empty(), "{name}");
        let (undo_stamp, undo_patch) =
            command(&app.dispatch(AppInput::Ui(UiIntent::Undo(id.clone()))));
        assert_eq!(undo_patch, select(module, baseline), "{name}");
        let mut current = snapshot(returned_payload, 2);
        readback(&mut app, undo_stamp, current.clone());
        let actual_return = current
            .view
            .module(ModuleId::new(module).unwrap())
            .unwrap()
            .clone();
        let mut followup = verified(&mut app, undo_stamp);
        let mut count = 0;
        while !followup.is_empty() {
            let (stamp, patch) = command(&followup);
            match patch {
                EditPatch::SetParameter {
                    model, parameter, ..
                } => {
                    assert_eq!(model, ModelCode(baseline), "{name}");
                    assert!(
                        app.catalog()
                            .model_by_id(module, baseline)
                            .unwrap()
                            .parameter(parameter.get())
                            .is_some(),
                        "{name}: unknown control write"
                    );
                }
                EditPatch::SetSync { model, .. } => {
                    assert_eq!(model, ModelCode(baseline), "{name}")
                }
                _ => panic!("{name}: unexpected history patch {patch:?}"),
            }
            count += 1;
            assert!(count < 16, "{name}: history must terminate");
            current = patched(&current, &patch, current.revision.0 + 1);
            readback(&mut app, stamp, current.clone());
            followup = verified(&mut app, stamp);
        }
        let module_id = ModuleId::new(module).unwrap();
        let actual = app
            .active_document()
            .unwrap()
            .draft
            .view
            .module(module_id)
            .unwrap();
        let original_module = original.view.module(module_id).unwrap();
        let definition = app.catalog().model_by_id(module, baseline).unwrap();
        for index in 0..15 {
            assert_eq!(
                actual.parameters[index],
                if definition.parameter(index as u8).is_some() {
                    original_module.parameters[index]
                } else {
                    actual_return.parameters[index]
                },
                "{name}, float {index}"
            );
        }
        assert_eq!(actual.enabled(), original_module.enabled(), "{name}");
        assert!(
            app.active_document().unwrap().conflicts.is_empty(),
            "{name}"
        );
        assert!(
            app.state().document_status(&id).unwrap().verified_in_buffer,
            "{name}"
        );
    }
}
fn select(module: u8, code: u32) -> EditPatch {
    EditPatch::SetModel {
        module: ModuleId::new(module).unwrap(),
        model: ModelCode(code),
    }
}
fn parameter(module: u8, code: u32, id: u8, value: f32) -> EditPatch {
    let catalog = Catalog::embedded().unwrap();
    EditPatch::SetParameter {
        module: ModuleId::new(module).unwrap(),
        model: ModelCode(code),
        model_index: ModelIndex(u32::from(
            catalog.model_by_id(module, code).unwrap().xml_index,
        )),
        parameter: ParamId::new(id).unwrap(),
        value: F32Bits::from_f32(value),
    }
}
fn apply(app: &mut AppController, document: &DocumentId, patch: EditPatch) -> Vec<Effect> {
    app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: document.clone(),
        patch,
    }))
}
fn command(effects: &[Effect]) -> (RequestStamp, EditPatch) {
    let Effect::Device(DeviceCommand::SubmitEdit { stamp, patch, .. }) = &effects[0] else {
        panic!("expected edit, got {effects:?}")
    };
    (*stamp, patch.clone())
}
fn readback(
    app: &mut AppController,
    stamp: RequestStamp,
    value: Arc<PresetSnapshot>,
) -> Vec<Effect> {
    app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
        stamp,
        target: PresetTarget::EditBuffer,
        snapshot: value,
    }))
}
fn verified(app: &mut AppController, stamp: RequestStamp) -> Vec<Effect> {
    app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
        stamp,
        stage: EditStage::Verified,
    }))
}
fn patched(base: &PresetSnapshot, patch: &EditPatch, revision: u64) -> Arc<PresetSnapshot> {
    let mut draft = PresetDraft::from_snapshot(base);
    draft.apply(patch).unwrap();
    Arc::new(
        draft
            .snapshot(base, SnapshotSource::Simulator, Revision(revision))
            .unwrap(),
    )
}

#[test]
fn offline_selection_defaults_undo_redo_comparison_and_encoded_export_are_one_action() {
    let mut app = connected();
    app.dispatch(AppInput::Ui(UiIntent::NewOfflineFrom(snapshot(BEFORE, 0))));
    let id = app.active_document_id().unwrap().clone();
    let original = app.active_document().unwrap().draft.clone();
    app.capture_comparison(&id, ComparisonSlot::A).unwrap();
    assert!(apply(&mut app, &id, select(5, 0x0a000000)).is_empty());
    let selected = app.active_document().unwrap().draft.clone();
    let module = selected.view.module(ModuleId::new(5).unwrap()).unwrap();
    assert_eq!(module.parameters[1], F32Bits::from_f32(50.0));
    assert_eq!(module.parameters[5], F32Bits::from_f32(19.0));
    assert_eq!(module.parameters[6], F32Bits::from_f32(20001.0));
    assert_eq!(
        module.parameters[0],
        original
            .view
            .module(ModuleId::new(5).unwrap())
            .unwrap()
            .parameters[0]
    );
    assert_eq!(
        app.active_document().unwrap().undo.past.as_slice(),
        std::slice::from_ref(&original)
    );
    app.capture_comparison(&id, ComparisonSlot::B).unwrap();
    let exported = app.document_snapshot(&id).unwrap();
    let decoded = PresetSnapshot::from_raw(
        exported.raw.clone(),
        SnapshotSource::Simulator,
        None,
        exported.revision,
    );
    assert_eq!(decoded.view.modules, exported.view.modules);
    for (before, after) in snapshot(BEFORE, 0)
        .raw
        .records()
        .iter()
        .zip(exported.raw.records())
    {
        if before.command == 0x14 && before.body[0] == 5 {
            let changed: Vec<_> = before
                .body
                .iter()
                .zip(&after.body)
                .enumerate()
                .filter_map(|(i, (a, b))| (a != b).then_some(i))
                .collect();
            assert!(
                changed
                    .iter()
                    .all(|i| (4..8).contains(i) || (12..16).contains(i) || (28..36).contains(i))
            );
        } else {
            assert_eq!(before, after);
        }
    }
    app.dispatch(AppInput::Ui(UiIntent::Undo(id.clone())));
    assert_eq!(app.active_document().unwrap().draft, original);
    app.dispatch(AppInput::Ui(UiIntent::Redo(id.clone())));
    assert_eq!(app.active_document().unwrap().draft, selected);
    assert!(app.recall_comparison(&id, ComparisonSlot::A).is_empty());
    assert_eq!(app.active_document().unwrap().draft, original);
    assert!(app.recall_comparison(&id, ComparisonSlot::B).is_empty());
    assert_eq!(app.active_document().unwrap().draft, selected);
}

#[test]
fn selecting_current_model_does_not_reset_existing_values_or_history() {
    let mut app = connected();
    let before = app.active_document().unwrap().clone();
    assert!(apply(&mut app, &device_document_id(), select(8, 0x0b00000b)).is_empty());
    let after = app.active_document().unwrap();
    assert_eq!(after.draft, before.draft);
    assert_eq!(after.local_revision, before.local_revision);
    assert!(after.undo.past.is_empty());
}

#[test]
fn malformed_catalog_default_keeps_the_existing_bits_in_offline_history_and_export() {
    let mut app = connected();
    app.dispatch(AppInput::Ui(UiIntent::NewOfflineFrom(snapshot(BEFORE, 0))));
    let id = app.active_document_id().unwrap().clone();
    let (module, code, unknown) = app
        .catalog()
        .modules()
        .iter()
        .find_map(|module| {
            module.models.iter().find_map(|model| {
                model
                    .parameters
                    .iter()
                    .find(|parameter| parameter.default.is_none())
                    .map(|parameter| {
                        (
                            ModuleId::new(module.module_id().unwrap()).unwrap(),
                            model.code,
                            parameter.parameter_id,
                        )
                    })
            })
        })
        .expect("shipped Detune default is an unresolved textual value");
    let original = app.active_document().unwrap().draft.clone();
    let bits = original.view.module(module).unwrap().parameters[usize::from(unknown)];
    assert!(apply(&mut app, &id, select(module.get(), code)).is_empty());
    assert_eq!(
        app.active_document()
            .unwrap()
            .draft
            .view
            .module(module)
            .unwrap()
            .parameters[usize::from(unknown)],
        bits
    );
    assert_eq!(
        app.document_snapshot(&id)
            .unwrap()
            .view
            .module(module)
            .unwrap()
            .parameters[usize::from(unknown)],
        bits
    );
    app.dispatch(AppInput::Ui(UiIntent::Undo(id)));
    assert_eq!(app.active_document().unwrap().draft, original);
}

#[test]
fn model_readback_preserves_later_control_edits_without_false_conflicts() {
    let mut app = connected();
    let id = device_document_id();
    let (stamp, _) = command(&apply(&mut app, &id, select(8, 0x0b00001d)));
    let mix = parameter(8, 0x0b00001d, 0, 31.0);
    assert!(apply(&mut app, &id, mix.clone()).is_empty());
    assert!(readback(&mut app, stamp, snapshot(BBD, 1)).is_empty());
    assert!(app.active_document().unwrap().conflicts.is_empty());
    let (next_stamp, next_patch) = command(&verified(&mut app, stamp));
    assert_eq!(next_patch, mix);
    assert_eq!(
        app.active_document()
            .unwrap()
            .draft
            .view
            .module(ModuleId::new(8).unwrap())
            .unwrap()
            .parameters[0],
        F32Bits::from_f32(31.0)
    );
    readback(&mut app, next_stamp, patched(&snapshot(BBD, 1), &mix, 2));
    assert!(verified(&mut app, next_stamp).is_empty());
    assert!(app.state().document_status(&id).unwrap().verified_in_buffer);
}

#[test]
fn model_selection_discards_queued_old_code_controls() {
    let mut app = connected();
    let id = device_document_id();
    let amp = parameter(3, 0x07000015, 0, 51.0);
    let (first, _) = command(&apply(&mut app, &id, amp.clone()));
    assert!(apply(&mut app, &id, parameter(8, 0x0b00000b, 0, 12.0)).is_empty());
    assert!(apply(&mut app, &id, select(8, 0x0b00001d)).is_empty());
    readback(&mut app, first, patched(&snapshot(BEFORE, 0), &amp, 1));
    let (_, next) = command(&verified(&mut app, first));
    assert_eq!(next, select(8, 0x0b00001d));
}

#[test]
fn undo_during_pending_model_selection_remains_a_later_intent() {
    let mut app = connected();
    let id = device_document_id();
    let original = app.active_document().unwrap().draft.clone();
    let (stamp, _) = command(&apply(&mut app, &id, select(8, 0x0b00001d)));
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::Undo(id.clone())))
            .is_empty()
    );
    readback(&mut app, stamp, snapshot(BBD, 1));
    assert_eq!(
        app.active_document().unwrap().draft.view.modules,
        original.view.modules
    );
    assert!(
        app.active_document()
            .unwrap()
            .draft
            .view
            .patch_assignments
            .semantically_equal(&original.view.patch_assignments)
    );
    let (_, next) = command(&verified(&mut app, stamp));
    assert_eq!(next, select(8, 0x0b00000b));
    assert!(app.active_document().unwrap().conflicts.is_empty());
}

#[test]
fn superseding_model_keeps_its_defaults_until_its_own_readback() {
    let mut app = connected();
    let id = device_document_id();
    let (stamp, _) = command(&apply(&mut app, &id, select(8, 0x0b00001d)));
    assert!(apply(&mut app, &id, select(8, 0x0b000021)).is_empty());
    let desired = app.active_document().unwrap().draft.clone();
    readback(&mut app, stamp, snapshot(BBD, 1));
    assert_eq!(
        app.active_document().unwrap().draft.view.modules,
        desired.view.modules
    );
    assert!(
        app.active_document()
            .unwrap()
            .draft
            .view
            .patch_assignments
            .semantically_equal(&desired.view.patch_assignments)
    );
    let (_, next) = command(&verified(&mut app, stamp));
    assert_eq!(next, select(8, 0x0b000021));
}

#[test]
fn acknowledged_model_undo_restores_defined_controls_and_adopts_undeclared_actual_bits() {
    let mut app = connected();
    let id = device_document_id();
    let original = app.active_document().unwrap().draft.clone();
    let (stamp, _) = command(&apply(&mut app, &id, select(8, 0x0b00001d)));
    readback(&mut app, stamp, snapshot(BBD, 1));
    assert!(verified(&mut app, stamp).is_empty());
    let (return_stamp, return_patch) =
        command(&app.dispatch(AppInput::Ui(UiIntent::Undo(id.clone()))));
    assert_eq!(return_patch, select(8, 0x0b00000b));
    let mut current = snapshot(
        include_bytes!("../../../tests/fixtures/model-transitions/dly-return.payload.bin"),
        2,
    );
    readback(&mut app, return_stamp, current.clone());
    let mut followup = verified(&mut app, return_stamp);
    let mut count = 0;
    while !followup.is_empty() {
        let (stamp, patch) = command(&followup);
        match &patch {
            EditPatch::SetParameter {
                model,
                model_index,
                parameter,
                ..
            } => {
                assert_eq!(*model, ModelCode(0x0b00000b));
                assert_eq!(
                    *model_index,
                    ModelIndex(u32::from(
                        app.catalog().model_by_id(8, model.0).unwrap().xml_index
                    ))
                );
                assert_ne!(
                    parameter.get(),
                    5,
                    "undefined Tube control must never receive a write"
                );
            }
            EditPatch::SetSync { model, .. } => assert_eq!(*model, ModelCode(0x0b00000b)),
            _ => panic!("unexpected model history patch {patch:?}"),
        }
        count += 1;
        assert!(count < 16, "history reconciliation must terminate");
        current = patched(&current, &patch, current.revision.0 + 1);
        readback(&mut app, stamp, current.clone());
        followup = verified(&mut app, stamp);
    }
    let actual = app
        .active_document()
        .unwrap()
        .draft
        .view
        .module(ModuleId::new(8).unwrap())
        .unwrap();
    let before = original.view.module(ModuleId::new(8).unwrap()).unwrap();
    for control in &app.catalog().model_by_id(8, 0x0b00000b).unwrap().parameters {
        assert_eq!(
            actual.parameters[usize::from(control.parameter_id)],
            before.parameters[usize::from(control.parameter_id)]
        );
    }
    assert_eq!(actual.parameters[5], F32Bits::from_f32(50.0));
    assert!(app.active_document().unwrap().conflicts.is_empty());
    assert!(app.state().document_status(&id).unwrap().verified_in_buffer);
}

#[test]
fn lost_model_readback_does_not_replay_selection_or_follow_up_controls() {
    let mut app = connected();
    let id = device_document_id();
    let (stamp, _) = command(&apply(&mut app, &id, select(8, 0x0b00001d)));
    app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
        stamp,
        stage: EditStage::Sent,
    }));
    assert!(apply(&mut app, &id, parameter(8, 0x0b00001d, 0, 31.0)).is_empty());
    app.dispatch(AppInput::Device(DeviceEvent::OperationFailed {
        stamp,
        error: ServiceError::new(ErrorKind::Timeout, "Lost model readback"),
    }));
    assert!(matches!(
        app.active_document().unwrap().pending[&stamp.intent].stage,
        EditStage::Indeterminate(_)
    ));
    assert!(verified(&mut app, stamp).is_empty());
    let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
    let Effect::Device(DeviceCommand::RefreshTarget {
        stamp: read_stamp, ..
    }) = effects[0]
    else {
        panic!()
    };
    assert!(readback(&mut app, read_stamp, snapshot(BBD, 2)).is_empty());
}
