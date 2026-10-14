use gp200_app::*;
use gp200_catalog::Catalog;
use gp200_domain::*;
use std::sync::Arc;
fn capture() -> Arc<PresetSnapshot> {
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(
            RawTransfer::from_payload(
                0x12,
                include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
            )
            .unwrap(),
        ),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(0),
    ))
}
fn offline() -> AppController {
    AppController::with_demo(capture(), AppPreferences::default()).unwrap()
}
fn gain(value: F32Bits) -> EditPatch {
    EditPatch::SetParameter {
        module: ModuleId::new(3).unwrap(),
        model: ModelCode(0x07000015),
        model_index: ModelIndex(13),
        parameter: ParamId::new(0).unwrap(),
        value,
    }
}
fn edit(app: &mut AppController, patch: EditPatch) -> Vec<Effect> {
    app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: app.active_document_id().unwrap().clone(),
        patch,
    }))
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
    app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
        epoch: SessionEpoch(1),
        state: ConnectionState::Connecting,
    }));
    let mut capabilities = Capabilities::simulator();
    capabilities.operations.insert(
        OperationKind::ReadGlobals,
        FeatureSupport::Verified {
            profile: ValidationProfileId("test-recovery-global-read".into()),
            mode: BackendMode::Simulator,
            firmware: None,
            evidence: vec![],
        },
    );
    app.dispatch(AppInput::Device(DeviceEvent::SessionChanged {
        epoch: SessionEpoch(1),
        identity: DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Simulator,
            firmware: None,
            display_name: "Simulator".into(),
        },
        capabilities,
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
    app
}
#[test]
fn recover_exact_multi_documents_history_ab_and_local_annotations_offline() {
    let mut original = offline();
    let first = original.active_document_id().unwrap().clone();
    original
        .capture_comparison(&first, ComparisonSlot::A)
        .unwrap();
    assert!(edit(&mut original, gain(F32Bits(0x41f6_e979))).is_empty());
    original
        .capture_comparison(&first, ComparisonSlot::B)
        .unwrap();
    assert!(
        edit(
            &mut original,
            EditPatch::SetMetadata(MetadataPatch {
                notes: Some("Long local note kept exactly\nline two".into()),
                author: Some("Local author".into()),
                ..Default::default()
            })
        )
        .is_empty()
    );
    assert!(
        original
            .dispatch(AppInput::Ui(UiIntent::Undo(first.clone())))
            .is_empty()
    );
    original.dispatch(AppInput::Ui(UiIntent::NewOfflineFrom(capture())));
    let second = original.active_document_id().unwrap().clone();
    edit(
        &mut original,
        EditPatch::MoveModule {
            module: ModuleId::new(7).unwrap(),
            before: Some(ModuleId::new(2).unwrap()),
        },
    );
    let snapshot = original.recovery_session();
    snapshot.validate().unwrap();
    let mut restarted = AppController::new(Catalog::embedded().unwrap(), AppPreferences::default());
    let recovered = restarted.recover_offline(&snapshot).unwrap();
    assert_eq!(recovered.len(), 2);
    assert_eq!(restarted.state().connection, ConnectionState::Offline);
    assert!(restarted.state().identity.is_none());
    assert!(restarted.state().confirmed_buffer.is_none());
    assert!(!restarted.device_operation_pending());
    let first_new = recovered
        .iter()
        .find(|id| {
            restarted.state().documents[*id].draft == original.state().documents[&first].draft
        })
        .unwrap()
        .clone();
    let doc = &restarted.state().documents[&first_new];
    assert_eq!(
        doc.baseline.raw,
        original.state().documents[&first].baseline.raw
    );
    assert_eq!(doc.undo.past, original.state().documents[&first].undo.past);
    assert_eq!(
        doc.undo.future,
        original.state().documents[&first].undo.future
    );
    assert_eq!(
        restarted.state().comparisons[&first_new].snapshots,
        original.state().comparisons[&first].snapshots
    );
    assert!(matches!(doc.target, EditTarget::Offline(_)));
    assert_eq!(doc.baseline.target, None);
    assert_eq!(doc.baseline.context, None);
    assert_eq!(
        restarted.active_document().unwrap().draft,
        original.state().documents[&second].draft
    );
    restarted.activate_document(&first_new);
    assert!(
        restarted
            .dispatch(AppInput::Ui(UiIntent::Redo(first_new.clone())))
            .is_empty()
    );
    assert_eq!(
        restarted
            .active_document()
            .unwrap()
            .draft
            .metadata
            .notes
            .as_deref(),
        Some("Long local note kept exactly\nline two")
    );
}
#[test]
fn pending_edit_recovers_as_description_and_offline_undo_never_replays_midi() {
    let mut original = connected();
    assert!(!edit(&mut original, gain(F32Bits::from_f32(31.0))).is_empty());
    let snapshot = original.recovery_session();
    assert_eq!(snapshot.documents[0].uncertainty.len(), 1);
    assert!(
        snapshot.documents[0].uncertainty[0]
            .description
            .contains("unknown")
    );
    let mut restarted = connected();
    let buffer = restarted.state().confirmed_buffer.clone();
    let epoch = restarted.state().epoch;
    let persistence = restarted.state().buffer_persistence.clone();
    let ids = restarted.recover_offline(&snapshot).unwrap();
    let id = ids[0].clone();
    assert_eq!(restarted.state().confirmed_buffer, buffer);
    assert_eq!(restarted.state().epoch, epoch);
    assert_eq!(restarted.state().buffer_persistence, persistence);
    assert!(!restarted.device_operation_pending());
    assert!(restarted.state().documents[&id].pending.is_empty());
    assert!(restarted.state().documents[&id].conflicts.is_empty());
    assert!(
        restarted
            .dispatch(AppInput::Ui(UiIntent::Undo(id.clone())))
            .is_empty()
    );
    assert_eq!(restarted.state().confirmed_buffer, buffer);
    let again = restarted.recovery_session();
    let retained = again
        .documents
        .iter()
        .find(|d| d.original_id == id)
        .unwrap();
    assert_eq!(retained.uncertainty, snapshot.documents[0].uncertainty);
}
#[test]
fn interrupted_gesture_becomes_one_undo_step_with_exact_before_image() {
    let mut original = offline();
    let id = original.active_document_id().unwrap().clone();
    let before = original.active_document().unwrap().draft.clone();
    let key = GestureKey {
        module: ModuleId::new(3).unwrap(),
        model: ModelCode(0x07000015),
        parameter: ParamId::new(0).unwrap(),
    };
    original.dispatch(AppInput::Ui(UiIntent::BeginGesture { document: id, key }));
    edit(&mut original, gain(F32Bits(0x41f6e979)));
    edit(&mut original, gain(F32Bits(0x41f6e980)));
    let snapshot = original.recovery_session();
    assert!(snapshot.documents[0].undo.gesture.is_some());
    let mut restarted = offline();
    let ids = restarted.recover_offline(&snapshot).unwrap();
    let doc = &restarted.state().documents[&ids[0]];
    assert!(doc.undo.gesture.is_none());
    assert_eq!(doc.undo.past.len(), 1);
    assert!(
        restarted
            .dispatch(AppInput::Ui(UiIntent::Undo(ids[0].clone())))
            .is_empty()
    );
    assert_eq!(restarted.active_document().unwrap().draft, before);
}
#[test]
fn unsupported_or_duplicate_session_is_rejected_without_partial_open() {
    let mut restarted = offline();
    let before = restarted.recovery_session();
    let mut bad = before.clone();
    bad.schema_version = 2;
    assert!(restarted.recover_offline(&bad).is_err());
    assert_eq!(restarted.recovery_session(), before);
    bad = before.clone();
    bad.documents.push(bad.documents[0].clone());
    assert!(restarted.recover_offline(&bad).is_err());
    assert_eq!(restarted.recovery_session(), before);
}

#[test]
fn explicitly_replacing_pristine_bootstrap_does_not_accumulate_and_rechecks_edits() {
    let original = offline().recovery_session();
    let mut restarted = offline();
    let startup = restarted.active_document_id().unwrap().clone();
    let ids = restarted
        .recover_offline_replacing_pristine(&original, Some(&startup))
        .unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(restarted.state().documents.len(), 1);
    let again = restarted.recovery_session();
    let mut another = offline();
    let startup = another.active_document_id().unwrap().clone();
    another
        .recover_offline_replacing_pristine(&again, Some(&startup))
        .unwrap();
    assert_eq!(another.state().documents.len(), 1);
    let mut edited = offline();
    let startup = edited.active_document_id().unwrap().clone();
    edit(&mut edited, gain(F32Bits::from_f32(31.0)));
    let before = edited.recovery_session();
    assert_eq!(
        edited
            .recover_offline_replacing_pristine(&original, Some(&startup))
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(edited.recovery_session(), before);
    assert_eq!(edited.recover_offline(&original).unwrap().len(), 1);
    assert_eq!(edited.state().documents.len(), 2);
}

#[test]
fn pending_save_stamp_is_retained_only_as_descriptive_session_uncertainty() {
    let mut original = connected();
    assert!(
        !original
            .dispatch(AppInput::Ui(UiIntent::SaveBufferTo {
                slot: SlotId::new(25).unwrap(),
                expected_baseline: Revision(0)
            }))
            .is_empty()
    );
    let captured = original.recovery_session();
    assert_eq!(captured.session_uncertainty.len(), 1);
    assert!(captured.session_uncertainty[0].contains("Save to slot 25"));
    assert!(captured.session_uncertainty[0].contains("epoch 1, intent"));
    let mut restarted = offline();
    restarted.recover_offline(&captured).unwrap();
    assert!(!restarted.device_operation_pending());
    assert_eq!(
        restarted.state().buffer_persistence,
        BufferPersistence::Unknown
    );
    assert_eq!(
        restarted.recovery_session().session_uncertainty,
        captured.session_uncertainty
    );
}

#[test]
fn epoch_detachment_keeps_ab_and_exact_dirty_history_in_recovery() {
    let mut original = connected();
    let id = original.active_document_id().unwrap().clone();
    original.capture_comparison(&id, ComparisonSlot::A).unwrap();
    let effects = edit(&mut original, gain(F32Bits::from_f32(31.0)));
    let Effect::Device(DeviceCommand::SubmitEdit { stamp, .. }) = effects[0] else {
        panic!()
    };
    original.dispatch(AppInput::Device(DeviceEvent::EditProgress {
        stamp,
        stage: EditStage::Sent,
    }));
    original.capture_comparison(&id, ComparisonSlot::B).unwrap();
    let before = original.recovery_session().documents[0].clone();
    original.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
        epoch: SessionEpoch(2),
        state: ConnectionState::Disconnected,
    }));
    let detached = original.recovery_session().documents[0].clone();
    assert!(matches!(detached.original_target, EditTarget::Offline(_)));
    assert_eq!(detached.draft, before.draft);
    assert_eq!(detached.undo, before.undo);
    assert_eq!(detached.comparison, before.comparison);
    assert_eq!(detached.uncertainty.len(), 1);
    assert!(
        detached.uncertainty[0]
            .description
            .contains("Indeterminate")
    );
}
fn globals(revision: u64, epoch: SessionEpoch, changed: u16) -> Arc<DeviceGlobalState> {
    let raw = RawTransfer::from_payload(
        0x12,
        include_bytes!("../../../tests/fixtures/globals/baseline.payload.bin").to_vec(),
    )
    .unwrap();
    let mut records = raw.records().to_vec();
    let runtime = records.iter_mut().find(|r| r.command == 0x1000).unwrap();
    runtime.body[2..4].copy_from_slice(&changed.to_le_bytes());
    Arc::new(
        DeviceGlobalState::from_transfer(
            Revision(revision),
            epoch,
            RawTransfer::from_records(0x12, &records).unwrap(),
            vec![],
        )
        .unwrap(),
    )
}
#[test]
fn latch_is_distinct_from_exact_dirty_and_persistent_save_and_ignores_stale_epoch() {
    let mut app = connected();
    let persistence = app.state().buffer_persistence.clone();
    let effects = app.dispatch(AppInput::Ui(UiIntent::RefreshDeviceGlobals));
    let Effect::Device(DeviceCommand::ReadGlobals(stamp)) = effects[0] else {
        panic!()
    };
    app.dispatch(AppInput::Device(DeviceEvent::GlobalStateReceived {
        stamp,
        state: globals(1, SessionEpoch(1), 1),
    }));
    assert_eq!(
        app.state()
            .device_globals
            .as_ref()
            .unwrap()
            .records()
            .runtime
            .as_ref()
            .unwrap()
            .preset_changed(),
        Some(true)
    );
    assert!(!app.active_document().unwrap().dirty());
    assert_eq!(app.state().buffer_persistence, persistence);
    let effects = app.dispatch(AppInput::Ui(UiIntent::RefreshDeviceGlobals));
    let Effect::Device(DeviceCommand::ReadGlobals(stamp)) = effects[0] else {
        panic!()
    };
    app.dispatch(AppInput::Device(DeviceEvent::GlobalStateReceived {
        stamp: RequestStamp {
            epoch: SessionEpoch(0),
            ..stamp
        },
        state: globals(99, SessionEpoch(0), 0),
    }));
    assert_eq!(
        app.state()
            .device_globals
            .as_ref()
            .unwrap()
            .records()
            .runtime
            .as_ref()
            .unwrap()
            .preset_changed(),
        Some(true)
    );
    app.dispatch(AppInput::Device(DeviceEvent::GlobalStateReceived {
        stamp,
        state: globals(2, SessionEpoch(1), 0),
    }));
    assert_eq!(
        app.state()
            .device_globals
            .as_ref()
            .unwrap()
            .records()
            .runtime
            .as_ref()
            .unwrap()
            .preset_changed(),
        Some(false)
    );
    assert_eq!(app.state().buffer_persistence, persistence);
    let dirty = globals(3, SessionEpoch(1), 1);
    let clear = globals(4, SessionEpoch(1), 0);
    assert!(!dirty.records().control_state_equal(clear.records()));
}
