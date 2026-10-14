use gp200_app::*;
use gp200_catalog::Catalog;
use gp200_domain::*;
use std::sync::Arc;

fn capture(revision: u64) -> Arc<PresetSnapshot> {
    let raw = RawTransfer::from_payload(
        0x12,
        include_bytes!("../../../tests/fixtures/patch-assignments/current.payload.bin").to_vec(),
    )
    .unwrap();
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(raw),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(revision),
    ))
}
fn connected() -> AppController {
    let mut app = AppController::new(Catalog::embedded().unwrap(), AppPreferences::default());
    let effects = app.dispatch(AppInput::Ui(UiIntent::Connect(DeviceSelection {
        input_id: "sim:in".into(),
        output_id: "sim:out".into(),
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
            display_name: "Simulator".into(),
        },
        capabilities: Capabilities::simulator(),
    }));
    app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
        epoch: SessionEpoch(1),
        state: ConnectionState::Ready,
    }));
    app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
        stamp: RequestStamp {
            epoch: SessionEpoch(1),
            intent,
        },
        target: PresetTarget::EditBuffer,
        snapshot: capture(0),
    }));
    app
}
fn apply(app: &mut AppController, edit: AssignmentEdit) -> Vec<Effect> {
    app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: device_document_id(),
        patch: EditPatch::SetPatchAssignment(edit),
    }))
}
fn stamp(effects: &[Effect]) -> RequestStamp {
    let Effect::Device(DeviceCommand::SubmitEdit { stamp, .. }) = effects[0] else {
        panic!("No assignment command: {effects:?}")
    };
    stamp
}
fn readback(app: &mut AppController, stamp: RequestStamp, snapshot: Arc<PresetSnapshot>) {
    app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
        stamp,
        target: PresetTarget::EditBuffer,
        snapshot,
    }));
    app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
        stamp,
        stage: EditStage::Verified,
    }));
}
fn edited(base: &PresetSnapshot, edit: AssignmentEdit) -> Arc<PresetSnapshot> {
    let mut draft = PresetDraft::from_snapshot(base);
    draft.apply(&EditPatch::SetPatchAssignment(edit)).unwrap();
    Arc::new(
        draft
            .snapshot(
                base,
                SnapshotSource::Simulator,
                Revision(base.revision.0 + 1),
            )
            .unwrap(),
    )
}
fn ctrl() -> AssignmentEdit {
    AssignmentEdit::CtrlModules {
        slot: 7,
        modules: vec![ModuleId::new(10).unwrap()],
    }
}

#[test]
fn ctrl_membership_acknowledges_volatile_padding_and_undo_restores_membership() {
    let mut app = connected();
    let request = stamp(&apply(&mut app, ctrl()));
    let changed = edited(&capture(0), ctrl());
    let mut records = changed.raw.records().to_vec();
    for record in &mut records {
        if record.command == 15 {
            record.body[2] ^= 0x4a;
            record.body[3] ^= 0x2c;
        }
    }
    let changed = Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(1),
    ));
    readback(&mut app, request, changed);
    let doc = &app.state().documents[&device_document_id()];
    assert!(doc.conflicts.is_empty());
    assert_eq!(
        app.state()
            .document_status(&device_document_id())
            .unwrap()
            .pending,
        0
    );
    let undo = app.dispatch(AppInput::Ui(UiIntent::Undo(device_document_id())));
    let Effect::Device(DeviceCommand::SubmitEdit { patch, .. }) = &undo[0] else {
        panic!()
    };
    assert_eq!(
        patch,
        &EditPatch::SetPatchAssignment(AssignmentEdit::CtrlModules {
            slot: 7,
            modules: vec![]
        })
    );
}

#[test]
fn invalid_catalog_reference_and_off_range_leave_document_unchanged() {
    let mut app = connected();
    let before = app.state().documents[&device_document_id()].draft.clone();
    let pre = before.view.module(ModuleId::new(0).unwrap()).unwrap();
    let target = ParameterAssignmentTarget {
        module: pre.module,
        expected_model: pre.model,
        parameter: ParamId::new(14).unwrap(),
    };
    assert!(
        apply(
            &mut app,
            AssignmentEdit::QuickTarget {
                index: 0,
                target: QuickAssignmentTarget::Parameter(target)
            }
        )
        .is_empty()
    );
    assert!(
        apply(
            &mut app,
            AssignmentEdit::ExpressionRange {
                slot: ExpressionSlotId::new(2, 2).unwrap(),
                minimum: 0.0.into(),
                maximum: 99.0.into()
            }
        )
        .is_empty()
    );
    assert_eq!(app.state().documents[&device_document_id()].draft, before);
}

#[test]
fn combined_expression_reference_range_is_retained_in_history_and_exactly_acknowledged() {
    let mut app = connected();
    let before = capture(0);
    let wah = before.view.module(ModuleId::new(1).unwrap()).unwrap();
    let edit = AssignmentEdit::ExpressionTargetAndRange {
        slot: ExpressionSlotId::new(2, 2).unwrap(),
        target: ExpressionAssignmentTarget::Parameter(ParameterAssignmentTarget {
            module: wah.module,
            expected_model: wah.model,
            parameter: ParamId::new(3).unwrap(),
        }),
        minimum: 80.0.into(),
        maximum: 20.0.into(),
    };
    let request = stamp(&apply(&mut app, edit.clone()));
    readback(&mut app, request, edited(&before, edit));
    let doc = &app.state().documents[&device_document_id()];
    assert!(doc.conflicts.is_empty());
    let actual = &doc.draft.view.patch_assignments.expressions[8].record;
    assert_eq!(
        (
            actual.module_raw,
            actual.parameter_raw,
            actual.minimum,
            actual.maximum
        ),
        (1, 3, 80.0.into(), 20.0.into())
    );
    assert_eq!(
        app.state()
            .document_status(&device_document_id())
            .unwrap()
            .pending,
        0
    );
}

#[test]
fn routing_edit_and_unrelated_quick_readback_merge_without_conflict() {
    let mut app = connected();
    let edit = AssignmentEdit::FxLoopPositions {
        send_boundary: 3,
        return_boundary: 4,
    };
    let request = stamp(&apply(&mut app, edit.clone()));
    let changed = edited(&capture(0), edit);
    let concurrent = edited(
        &changed,
        AssignmentEdit::QuickTarget {
            index: 1,
            target: QuickAssignmentTarget::PatchTempo,
        },
    );
    readback(&mut app, request, concurrent);
    let doc = &app.state().documents[&device_document_id()];
    assert!(doc.conflicts.is_empty());
    assert_eq!(
        doc.draft
            .view
            .patch_assignments
            .routing
            .as_ref()
            .unwrap()
            .record,
        PatchRouting {
            send_boundary: 3,
            return_boundary: 4
        }
    );
    assert_eq!(
        doc.draft.view.patch_assignments.quicks[1].record.module_raw,
        12
    );
}
