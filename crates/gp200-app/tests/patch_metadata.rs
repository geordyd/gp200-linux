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
fn apply(app: &mut AppController, metadata: MetadataPatch) -> Vec<Effect> {
    app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: device_document_id(),
        patch: EditPatch::SetMetadata(metadata),
    }))
}
fn submit(effects: &[Effect]) -> (RequestStamp, MetadataPatch) {
    let Effect::Device(DeviceCommand::SubmitEdit {
        stamp,
        patch: EditPatch::SetMetadata(metadata),
        ..
    }) = &effects[0]
    else {
        panic!("missing metadata effect {effects:?}")
    };
    (*stamp, metadata.clone())
}
fn changed(base: &PresetSnapshot, metadata: MetadataPatch, revision: u64) -> Arc<PresetSnapshot> {
    let mut draft = PresetDraft::from_snapshot(base);
    draft.apply(&EditPatch::SetMetadata(metadata)).unwrap();
    Arc::new(PresetSnapshot::from_raw(
        draft
            .snapshot(base, SnapshotSource::Simulator, Revision(revision))
            .unwrap()
            .raw,
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(revision),
    ))
}
fn complete(
    app: &mut AppController,
    stamp: RequestStamp,
    snapshot: Arc<PresetSnapshot>,
) -> Vec<Effect> {
    let first = app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
        stamp,
        target: PresetTarget::EditBuffer,
        snapshot,
    }));
    assert!(first.is_empty());
    app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
        stamp,
        stage: EditStage::Verified,
    }))
}
#[test]
fn pending_native_edit_and_local_notes_survive_confirmation_history_and_reconnect() {
    let mut app = connected();
    let (stamp, patch) = submit(&apply(
        &mut app,
        MetadataPatch {
            native_pan: Some(PatchPan::new(1).unwrap()),
            ..Default::default()
        },
    ));
    assert!(
        apply(
            &mut app,
            MetadataPatch {
                notes: Some("Remember wah setting".into()),
                author: Some("Me".into()),
                style: Some("Freeform local".into()),
                ..Default::default()
            }
        )
        .is_empty()
    );
    assert!(complete(&mut app, stamp, changed(&capture(0), patch, 1)).is_empty());
    let doc = app.active_document().unwrap();
    assert!(doc.conflicts.is_empty());
    assert_eq!(doc.draft.metadata.native_pan.unwrap().raw(), 1);
    assert_eq!(
        doc.draft.metadata.notes.as_deref(),
        Some("Remember wah setting")
    );
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::Undo(device_document_id())))
            .is_empty()
    );
    assert_eq!(app.active_document().unwrap().draft.metadata.notes, None);
    assert_eq!(
        app.active_document()
            .unwrap()
            .draft
            .metadata
            .native_pan
            .unwrap()
            .raw(),
        1
    );
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::Redo(device_document_id())))
            .is_empty()
    );
    assert_eq!(
        app.active_document()
            .unwrap()
            .draft
            .metadata
            .notes
            .as_deref(),
        Some("Remember wah setting")
    );
    let exported = app.document_snapshot(&device_document_id()).unwrap();
    assert_eq!(exported.view.metadata.author.as_deref(), Some("Me"));
    assert_eq!(
        exported.view.metadata.notes.as_deref(),
        Some("Remember wah setting")
    );
    app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
        epoch: SessionEpoch(2),
        state: ConnectionState::Disconnected,
    }));
    assert!(
        app.state()
            .documents
            .values()
            .any(|doc| matches!(doc.target, EditTarget::Offline(_))
                && doc.draft.metadata.notes.as_deref() == Some("Remember wah setting"))
    );
    assert!(app.state().confirmed_buffer.is_none());
}
#[test]
fn queued_native_metadata_fields_merge_and_use_fresh_confirmed_revision() {
    let mut app = connected();
    let (stamp, first) = submit(&apply(
        &mut app,
        MetadataPatch {
            native_pan: Some(PatchPan::new(1).unwrap()),
            ..Default::default()
        },
    ));
    assert!(
        apply(
            &mut app,
            MetadataPatch {
                level: Some(61),
                ..Default::default()
            }
        )
        .is_empty()
    );
    assert!(
        apply(
            &mut app,
            MetadataPatch {
                native_style: Some(NativeStyleId::new(0).unwrap()),
                ..Default::default()
            }
        )
        .is_empty()
    );
    assert!(
        apply(
            &mut app,
            MetadataPatch {
                notes: Some("Local".into()),
                ..Default::default()
            }
        )
        .is_empty()
    );
    let effects = complete(&mut app, stamp, changed(&capture(0), first, 1));
    let (next, merged) = submit(&effects);
    assert_eq!(merged.level, Some(61));
    assert_eq!(merged.native_style.unwrap().raw(), 0);
    assert_eq!(merged.notes, None);
    assert_eq!(merged.native_pan, None);
    let Effect::Device(DeviceCommand::SubmitEdit { base_revision, .. }) = &effects[0] else {
        panic!()
    };
    assert_eq!(*base_revision, Revision(1));
    let base = app.state().confirmed_buffer.clone().unwrap();
    assert!(complete(&mut app, next, changed(&base, merged, 2)).is_empty());
    assert_eq!(
        app.active_document()
            .unwrap()
            .draft
            .metadata
            .notes
            .as_deref(),
        Some("Local")
    );
    assert!(app.active_document().unwrap().conflicts.is_empty());
}
#[test]
fn local_annotation_only_changes_never_mark_device_unsaved_or_submit_save() {
    let mut app = connected();
    let before = app.state().buffer_persistence.clone();
    assert!(
        apply(
            &mut app,
            MetadataPatch {
                notes: Some("Keep locally".into()),
                ..Default::default()
            }
        )
        .is_empty()
    );
    assert_eq!(app.state().buffer_persistence, before);
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::Undo(device_document_id())))
            .is_empty()
    );
    assert_eq!(app.state().buffer_persistence, before);
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::Redo(device_document_id())))
            .is_empty()
    );
    assert_eq!(app.state().buffer_persistence, before);
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::SaveBufferTo {
            slot: SlotId::new(255).unwrap(),
            expected_baseline: Revision(0)
        }))
        .is_empty()
    );
}
#[test]
fn native_physical_conflict_accepts_actual_raw_value_and_keeps_local_annotations() {
    let mut app = connected();
    let (stamp, _) = submit(&apply(
        &mut app,
        MetadataPatch {
            native_pan: Some(PatchPan::new(1).unwrap()),
            ..Default::default()
        },
    ));
    apply(
        &mut app,
        MetadataPatch {
            notes: Some("Local survives".into()),
            ..Default::default()
        },
    );
    let actual = changed(
        &capture(0),
        MetadataPatch {
            native_pan: Some(PatchPan::new(2).unwrap()),
            ..Default::default()
        },
        1,
    );
    app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
        stamp,
        target: PresetTarget::EditBuffer,
        snapshot: actual,
    }));
    assert!(!app.active_document().unwrap().conflicts.is_empty());
    app.dispatch(AppInput::Ui(UiIntent::ResolveConflict(
        ConflictResolution::AcceptDevice(stamp.intent),
    )));
    assert_eq!(
        app.active_document()
            .unwrap()
            .draft
            .metadata
            .native_pan
            .unwrap()
            .raw(),
        2
    );
    assert_eq!(
        app.active_document()
            .unwrap()
            .draft
            .metadata
            .notes
            .as_deref(),
        Some("Local survives")
    );
}
