use gp200_app::*;
use gp200_domain::*;
use gp200_library::review_offline_merge;
use std::sync::Arc;

fn capture() -> Arc<PresetSnapshot> {
    let raw = RawTransfer::from_payload(
        0x12,
        include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
    )
    .unwrap();
    let mut records = raw.records().to_vec();
    records.push(RawRecord {
        command: 0xdada,
        body: vec![0xff, 0, 4],
    });
    records.push(RawRecord {
        command: 0xdada,
        body: vec![9],
    });
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(0),
    ))
}
fn source() -> Arc<PresetSnapshot> {
    let original = capture();
    let mut draft = PresetDraft::from_snapshot(&original);
    draft
        .view
        .modules
        .iter_mut()
        .find(|module| module.module.get() == 3)
        .unwrap()
        .parameters[0] = F32Bits(0x7fc0_9999);
    draft
        .view
        .modules
        .iter_mut()
        .find(|module| module.module.get() == 4)
        .unwrap()
        .parameters[0] = F32Bits(0x8000_0000);
    draft.metadata.notes = Some("Keep this exact local note\n第二行".into());
    draft.view.metadata = draft.metadata.clone();
    Arc::new(
        draft
            .snapshot(&original, original.source.clone(), Revision(1))
            .unwrap(),
    )
}
fn fields() -> Vec<OfflineMergeField> {
    vec![
        OfflineMergeField::Parameter {
            module: ModuleId::new(3).unwrap(),
            expected_model: ModelCode(0x07000015),
            parameter: ParamId::new(0).unwrap(),
        },
        OfflineMergeField::Parameter {
            module: ModuleId::new(4).unwrap(),
            expected_model: ModelCode(27),
            parameter: ParamId::new(0).unwrap(),
        },
        OfflineMergeField::Metadata(MergeMetadataField::Notes),
    ]
}

#[test]
fn selected_merge_is_one_exact_offline_history_step_and_undo_redo_emit_zero_effects() {
    let mut app = AppController::with_demo(capture(), AppPreferences::default()).unwrap();
    let id = app.active_document_id().unwrap().clone();
    let before = app.active_document().unwrap().draft.clone();
    let source = source();
    let source_before = (*source).clone();
    let review = review_offline_merge(
        app.active_document().unwrap(),
        source.clone(),
        fields(),
        app.catalog(),
    )
    .unwrap();
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::ApplyOfflineMerge(review.request())))
            .is_empty()
    );
    let document = app.active_document().unwrap();
    assert_eq!(document.undo.past.len(), 1);
    assert_eq!(document.undo.past[0], before);
    assert_eq!(document.local_revision, Revision(1));
    assert_eq!(document.draft.metadata.notes, source.view.metadata.notes);
    let after = document.draft.clone();
    assert_eq!(app.document_snapshot(&id).unwrap().raw, source.raw);
    assert_eq!(*source, source_before);
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::Undo(id.clone())))
            .is_empty()
    );
    assert_eq!(app.active_document().unwrap().draft, before);
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::Redo(id.clone())))
            .is_empty()
    );
    assert_eq!(app.active_document().unwrap().draft, after);
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::ApplyOfflineMerge(review.request())))
            .is_empty()
    );
    assert_eq!(app.active_document().unwrap().draft, after);
    assert_eq!(app.active_document().unwrap().undo.past.len(), 1);
    assert!(
        app.state()
            .messages
            .iter()
            .any(|message| message.message.contains("changed since review"))
    );
}

#[test]
fn review_open_during_an_edit_rejects_stale_apply_and_preserves_newer_work() {
    let mut app = AppController::with_demo(capture(), AppPreferences::default()).unwrap();
    let id = app.active_document_id().unwrap().clone();
    let review = review_offline_merge(
        app.active_document().unwrap(),
        source(),
        fields(),
        app.catalog(),
    )
    .unwrap();
    let metadata = MetadataPatch {
        notes: Some("Newer work wins".into()),
        ..MetadataPatch::default()
    };
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
            document: id,
            patch: EditPatch::SetMetadata(metadata)
        }))
        .is_empty()
    );
    let document_before = app.active_document().unwrap().draft.clone();
    let history_before = app.active_document().unwrap().undo.past.len();
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::ApplyOfflineMerge(review.request())))
            .is_empty()
    );
    assert_eq!(app.active_document().unwrap().draft, document_before);
    assert_eq!(
        app.active_document().unwrap().undo.past.len(),
        history_before
    );
}

#[test]
fn recovery_retains_merged_draft_source_beforeimage_and_one_undo_step() {
    let mut app = AppController::with_demo(capture(), AppPreferences::default()).unwrap();
    let before = app.active_document().unwrap().draft.clone();
    let review = review_offline_merge(
        app.active_document().unwrap(),
        source(),
        fields(),
        app.catalog(),
    )
    .unwrap();
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::ApplyOfflineMerge(review.request())))
            .is_empty()
    );
    let expected = app.active_document().unwrap().draft.clone();
    let session = app.recovery_session();
    let mut restart = AppController::new(
        gp200_catalog::Catalog::embedded().unwrap(),
        AppPreferences::default(),
    );
    let restored = restart.recover_offline(&session).unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restart.active_document().unwrap().draft, expected);
    assert_eq!(
        restart.active_document().unwrap().undo.past,
        vec![before.clone()]
    );
    assert!(
        restart
            .dispatch(AppInput::Ui(UiIntent::Undo(restored[0].clone())))
            .is_empty()
    );
    assert_eq!(restart.active_document().unwrap().draft, before);
    assert_eq!(restart.state().connection, ConnectionState::Offline);
}

#[test]
fn fabricated_review_for_the_device_buffer_cannot_grant_hardware_authority() {
    let mut app = AppController::new(
        gp200_catalog::Catalog::embedded().unwrap(),
        AppPreferences::default(),
    );
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
        snapshot: capture(),
    }));
    let before = app.state().documents[&device_document_id()].draft.clone();
    let mut forged_offline = app.state().documents[&device_document_id()].clone();
    forged_offline.target = EditTarget::Offline(forged_offline.id.clone());
    let request = review_offline_merge(&forged_offline, source(), fields(), app.catalog())
        .unwrap()
        .request();
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::ApplyOfflineMerge(request)))
            .is_empty()
    );
    assert_eq!(app.state().documents[&device_document_id()].draft, before);
    assert!(
        app.state().documents[&device_document_id()]
            .undo
            .past
            .is_empty()
    );
    assert!(
        app.state()
            .messages
            .iter()
            .any(|message| message.message.contains("offline document"))
    );
}
