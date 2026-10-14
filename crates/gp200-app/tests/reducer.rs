use gp200_app::*;
use gp200_catalog::Catalog;
use gp200_domain::*;
use std::sync::Arc;

fn capture(revision: u64) -> Arc<PresetSnapshot> {
    let raw = RawTransfer::from_payload(
        0x12,
        include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
    )
    .unwrap();
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(raw),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(revision),
    ))
}
fn selection() -> DeviceSelection {
    DeviceSelection {
        input_id: "sim:in".into(),
        output_id: "sim:out".into(),
        fingerprint_hint: None,
    }
}
fn connected() -> AppController {
    let mut app = AppController::new(Catalog::embedded().unwrap(), AppPreferences::default());
    let effects = app.dispatch(AppInput::Ui(UiIntent::Connect(selection())));
    let Effect::Device(DeviceCommand::Connect { intent, .. }) = &effects[0] else {
        panic!()
    };
    let intent = *intent;
    app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
        epoch: SessionEpoch(1),
        state: ConnectionState::Connecting,
    }));
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
    assert!(app.state().confirmed_buffer.is_some());
    app
}
fn gain(value: f32) -> EditPatch {
    EditPatch::SetParameter {
        module: ModuleId::new(3).unwrap(),
        model: ModelCode(0x07000015),
        model_index: ModelIndex(13),
        parameter: ParamId::new(0).unwrap(),
        value: F32Bits(value.to_bits()),
    }
}
fn mod_sync(enabled: bool, ordinary_restore: Option<F32Bits>) -> EditPatch {
    EditPatch::SetSync {
        module: ModuleId::new(7).unwrap(),
        model: ModelCode(0x04000015),
        model_index: ModelIndex(9),
        power_parameter: ParamId::new(2).unwrap(),
        value_parameter: ParamId::new(1).unwrap(),
        enabled,
        ordinary_restore,
    }
}

#[test]
fn sync_requires_explicit_off_value_and_acknowledges_both_parameter_fields() {
    let mut app = connected();
    assert!(apply(&mut app, mod_sync(false, None)).is_empty());
    let patch = mod_sync(false, Some(F32Bits::from_f32(0.73)));
    let effects = apply(&mut app, patch.clone());
    let stamp = command_stamp(&effects);
    readback(&mut app, stamp, edited(&capture(0), &[patch], 1));
    verified(&mut app, stamp);
    let status = app.state().document_status(&device_document_id()).unwrap();
    assert!(status.verified_in_buffer);
    assert_eq!(status.pending, 0);
    assert!(
        app.state().documents[&device_document_id()]
            .conflicts
            .is_empty()
    );
}

#[test]
fn undo_sync_restores_power_then_the_original_nondefault_subdivision() {
    let mut app = connected();
    let subdivision = EditPatch::SetParameter {
        module: ModuleId::new(7).unwrap(),
        model: ModelCode(0x04000015),
        model_index: ModelIndex(9),
        parameter: ParamId::new(1).unwrap(),
        value: F32Bits::from_f32(5.0),
    };
    let stamp = command_stamp(&apply(&mut app, subdivision.clone()));
    let synced = edited(&capture(0), std::slice::from_ref(&subdivision), 1);
    readback(&mut app, stamp, synced.clone());
    verified(&mut app, stamp);
    let off = mod_sync(false, Some(F32Bits::from_f32(0.73)));
    let stamp = command_stamp(&apply(&mut app, off.clone()));
    let ordinary = edited(&synced, &[off], 2);
    readback(&mut app, stamp, ordinary.clone());
    verified(&mut app, stamp);
    let effects = app.dispatch(AppInput::Ui(UiIntent::Undo(device_document_id())));
    let stamp = command_stamp(&effects);
    let Effect::Device(DeviceCommand::SubmitEdit { patch, .. }) = &effects[0] else {
        panic!()
    };
    assert_eq!(*patch, mod_sync(true, None));
    let initialized = edited(&ordinary, &[mod_sync(true, None)], 3);
    readback(&mut app, stamp, initialized.clone());
    let followup = verified(&mut app, stamp);
    let followup_stamp = command_stamp(&followup);
    let Effect::Device(DeviceCommand::SubmitEdit { patch, .. }) = &followup[0] else {
        panic!()
    };
    assert_eq!(*patch, subdivision);
    readback(
        &mut app,
        followup_stamp,
        edited(&initialized, &[subdivision], 4),
    );
    verified(&mut app, followup_stamp);
    let doc = &app.state().documents[&device_document_id()];
    assert!(doc.conflicts.is_empty());
    assert_eq!(
        doc.draft
            .view
            .module(ModuleId::new(7).unwrap())
            .unwrap()
            .parameters[1],
        F32Bits::from_f32(5.0)
    );
}
fn amp_parameter(id: u8, value: f32) -> EditPatch {
    EditPatch::SetParameter {
        module: ModuleId::new(3).unwrap(),
        model: ModelCode(0x07000015),
        model_index: ModelIndex(13),
        parameter: ParamId::new(id).unwrap(),
        value: F32Bits(value.to_bits()),
    }
}
fn apply(app: &mut AppController, patch: EditPatch) -> Vec<Effect> {
    app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: device_document_id(),
        patch,
    }))
}
fn command_stamp(effects: &[Effect]) -> RequestStamp {
    match &effects[0] {
        Effect::Device(
            DeviceCommand::SubmitEdit { stamp, .. }
            | DeviceCommand::RefreshTarget { stamp, .. }
            | DeviceCommand::SaveBufferTo { stamp, .. },
        ) => *stamp,
        _ => panic!(),
    }
}
fn edited(base: &PresetSnapshot, patches: &[EditPatch], revision: u64) -> Arc<PresetSnapshot> {
    let mut draft = PresetDraft::from_snapshot(base);
    for patch in patches {
        draft.apply(patch).unwrap();
    }
    Arc::new(
        draft
            .snapshot(base, SnapshotSource::Simulator, Revision(revision))
            .unwrap(),
    )
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
fn verified(app: &mut AppController, stamp: RequestStamp) -> Vec<Effect> {
    app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
        stamp,
        stage: EditStage::Verified,
    }))
}

fn global_capture(revision: u64) -> Arc<DeviceGlobalState> {
    Arc::new(
        DeviceGlobalState::from_transfer(
            Revision(revision),
            SessionEpoch(1),
            RawTransfer::from_payload(
                0x12,
                include_bytes!("../../../tests/fixtures/globals/baseline.payload.bin").to_vec(),
            )
            .unwrap(),
            vec![],
        )
        .unwrap(),
    )
}
fn read_globals(app: &mut AppController, revision: u64) {
    let mut capabilities = app.state().capabilities.clone();
    for operation in [OperationKind::ReadGlobals, OperationKind::WriteGlobals] {
        capabilities.operations.insert(
            operation,
            FeatureSupport::Verified {
                profile: ValidationProfileId("test-global-codec".into()),
                mode: BackendMode::Simulator,
                firmware: None,
                evidence: vec![],
            },
        );
    }
    app.dispatch(AppInput::Device(DeviceEvent::SessionChanged {
        epoch: SessionEpoch(1),
        identity: app.state().identity.clone().unwrap(),
        capabilities,
    }));
    let effects = app.dispatch(AppInput::Ui(UiIntent::RefreshDeviceGlobals));
    let Effect::Device(DeviceCommand::ReadGlobals(stamp)) = effects[0] else {
        panic!("expected stamped global read");
    };
    app.dispatch(AppInput::Device(DeviceEvent::GlobalStateReceived {
        stamp,
        state: global_capture(revision),
    }));
}
fn global_edit(app: &mut AppController) -> (RequestStamp, GlobalControlEdit) {
    let patch = GlobalControlEdit::Drum {
        field: gp200_protocol::DrumSetting::Tempo,
        value: 121,
    };
    let effects = app.dispatch(AppInput::Ui(UiIntent::ApplyGlobalControlEdit(
        patch.clone(),
    )));
    let Effect::Device(DeviceCommand::SubmitGlobalControlEdit { stamp, .. }) = effects[0] else {
        panic!("expected typed global edit");
    };
    (stamp, patch)
}

#[test]
fn global_edit_requires_fresh_scope_and_both_exact_readback_and_verified_stage() {
    let mut app = connected();
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::ApplyGlobalControlEdit(
            GlobalControlEdit::DrumPlay(false)
        )))
        .is_empty()
    );
    read_globals(&mut app, 1);
    let original = app.state().device_globals.clone().unwrap();
    let (stamp, patch) = global_edit(&mut app);
    assert_eq!(
        app.state().device_globals.as_ref().unwrap().revision,
        Revision(1)
    );
    assert!(app.device_operation_pending());
    verified(&mut app, stamp);
    assert!(app.device_operation_pending());
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::ApplyGlobalControlEdit(
            patch.clone()
        )))
        .is_empty()
    );
    let actual = Arc::new(
        DeviceGlobalState::from_transfer(
            Revision(2),
            SessionEpoch(1),
            original.expected_after(&patch).unwrap(),
            vec![],
        )
        .unwrap(),
    );
    app.dispatch(AppInput::Device(DeviceEvent::GlobalStateReceived {
        stamp: RequestStamp {
            epoch: SessionEpoch(0),
            ..stamp
        },
        state: actual.clone(),
    }));
    assert_eq!(
        app.state().device_globals.as_ref().unwrap().revision,
        Revision(1)
    );
    app.dispatch(AppInput::Device(DeviceEvent::GlobalStateReceived {
        stamp,
        state: actual,
    }));
    assert!(!app.device_operation_pending());
    assert_eq!(
        app.state().device_globals.as_ref().unwrap().revision,
        Revision(2)
    );
    assert_eq!(
        app.state().confirmed_buffer.as_ref().unwrap().revision,
        Revision(0)
    );
}

#[test]
fn mismatched_global_readback_is_indeterminate_and_needs_a_new_read() {
    let mut app = connected();
    read_globals(&mut app, 1);
    let (stamp, patch) = global_edit(&mut app);
    app.dispatch(AppInput::Device(DeviceEvent::GlobalStateReceived {
        stamp,
        state: global_capture(2),
    }));
    assert!(app.state().device_globals.is_none());
    assert!(!app.device_operation_pending());
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::ApplyGlobalControlEdit(patch)))
            .is_empty()
    );
    assert_eq!(
        app.state()
            .messages
            .back()
            .unwrap()
            .error
            .as_ref()
            .unwrap()
            .kind,
        ErrorKind::Busy
    );
    read_globals(&mut app, 3);
    assert!(app.state().device_globals.is_some());
    let (_, _) = global_edit(&mut app);
    assert!(app.device_operation_pending());
}

#[test]
fn disconnect_retires_global_state_and_never_replays_a_submitted_edit() {
    let mut app = connected();
    read_globals(&mut app, 1);
    let (stamp, _) = global_edit(&mut app);
    app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
        epoch: SessionEpoch(2),
        state: ConnectionState::Disconnected,
    }));
    assert!(app.state().device_globals.is_none());
    assert!(!app.device_operation_pending());
    assert!(
        app.dispatch(AppInput::Device(DeviceEvent::GlobalStateReceived {
            stamp,
            state: global_capture(2)
        }))
        .is_empty()
    );
    assert!(app.state().device_globals.is_none());
}

#[test]
fn readback_verifies_buffer_but_does_not_claim_slot_save() {
    let mut app = connected();
    let effects = apply(&mut app, gain(51.0));
    let stamp = command_stamp(&effects);
    readback(&mut app, stamp, edited(&capture(0), &[gain(51.0)], 1));
    verified(&mut app, stamp);
    let status = app.state().document_status(&device_document_id()).unwrap();
    assert!(status.changed_since_baseline);
    assert!(status.verified_in_buffer);
    assert!(!status.local_draft);
    assert_eq!(status.pending, 0);
    assert_eq!(status.persistence, Some(BufferPersistence::Unsaved));
    assert_eq!(
        app.state().documents[&device_document_id()]
            .baseline
            .revision,
        Revision(0)
    );
}

#[test]
fn acknowledgement_before_readback_does_not_complete_or_flush() {
    let mut app = connected();
    let effects = apply(&mut app, gain(51.0));
    let stamp = command_stamp(&effects);
    assert!(apply(&mut app, gain(52.0)).is_empty());
    assert!(verified(&mut app, stamp).is_empty());
    assert_eq!(
        app.state()
            .document_status(&device_document_id())
            .unwrap()
            .pending,
        1
    );
    let effects = readback(&mut app, stamp, edited(&capture(0), &[gain(51.0)], 1));
    assert_eq!(effects.len(), 1);
    let Effect::Device(DeviceCommand::SubmitEdit {
        patch,
        base_revision,
        ..
    }) = &effects[0]
    else {
        panic!()
    };
    assert_eq!(patch, &gain(52.0));
    assert_eq!(*base_revision, Revision(1));
}

#[test]
fn gesture_coalesces_latest_value_and_history_keeps_one_action() {
    let mut app = connected();
    let id = device_document_id();
    let key = GestureKey {
        module: ModuleId::new(3).unwrap(),
        model: ModelCode(0x07000015),
        parameter: ParamId::new(0).unwrap(),
    };
    app.dispatch(AppInput::Ui(UiIntent::BeginGesture {
        document: id.clone(),
        key: key.clone(),
    }));
    let stamp = command_stamp(&apply(&mut app, gain(51.0)));
    assert!(apply(&mut app, gain(52.0)).is_empty());
    assert!(apply(&mut app, gain(53.0)).is_empty());
    app.dispatch(AppInput::Ui(UiIntent::EndGesture {
        document: id.clone(),
        key,
    }));
    assert_eq!(app.state().documents[&id].undo.past.len(), 1);
    readback(&mut app, stamp, edited(&capture(0), &[gain(51.0)], 1));
    let effects = verified(&mut app, stamp);
    let Effect::Device(DeviceCommand::SubmitEdit {
        patch,
        base_revision,
        ..
    }) = &effects[0]
    else {
        panic!()
    };
    assert_eq!(patch, &gain(53.0));
    assert_eq!(*base_revision, Revision(1));
    assert!(app.state().documents[&id].conflicts.is_empty());
}

#[test]
fn stale_epoch_context_and_read_completions_do_not_replace_confirmed_buffer() {
    let mut app = connected();
    let first = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
    let second = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
    readback(
        &mut app,
        command_stamp(&first),
        edited(&capture(0), &[gain(60.0)], 10),
    );
    assert_eq!(
        app.state().confirmed_buffer.as_ref().unwrap().revision,
        Revision(0)
    );
    let mut wrong = (*capture(12)).clone();
    wrong.context = Some(WireContext {
        group: 1,
        selector: 0,
    });
    readback(&mut app, command_stamp(&second), Arc::new(wrong));
    assert_eq!(
        app.state().confirmed_buffer.as_ref().unwrap().revision,
        Revision(0)
    );
    readback(
        &mut app,
        RequestStamp {
            epoch: SessionEpoch(0),
            intent: command_stamp(&second).intent,
        },
        capture(20),
    );
    assert_eq!(
        app.state().confirmed_buffer.as_ref().unwrap().revision,
        Revision(0)
    );
    // Even a valid-looking unsolicited completion is not a request match.
    readback(
        &mut app,
        RequestStamp {
            epoch: SessionEpoch(1),
            intent: IntentId(9999),
        },
        capture(30),
    );
    assert_eq!(
        app.state().confirmed_buffer.as_ref().unwrap().revision,
        Revision(0)
    );
}

#[test]
fn stored_preview_is_offline_and_does_not_steal_buffer_context() {
    let mut app = connected();
    let slot = SlotId::new(42).unwrap();
    let effects = app.dispatch(AppInput::Ui(UiIntent::PreviewStored(slot)));
    let stamp = command_stamp(&effects);
    let mut stored = (*capture(5)).clone();
    stored.target = Some(PresetTarget::Stored(slot));
    stored.context = Some(WireContext {
        group: 1,
        selector: 42,
    });
    app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
        stamp,
        target: PresetTarget::Stored(slot),
        snapshot: Arc::new(stored),
    }));
    assert_eq!(
        app.state().confirmed_buffer.as_ref().unwrap().context,
        Some(WireContext {
            group: 0,
            selector: 23
        })
    );
    assert!(matches!(
        app.active_document().unwrap().target,
        EditTarget::Offline(_)
    ));
    assert_eq!(
        app.state().bank.slots[&slot]
            .snapshot
            .context
            .unwrap()
            .selector,
        42
    );
}

#[test]
fn hardware_missing_capability_keeps_edits_local_and_preserves_raw_bits() {
    let mut app = connected();
    app.dispatch(AppInput::Device(DeviceEvent::SessionChanged {
        epoch: SessionEpoch(1),
        identity: DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Hardware,
            firmware: None,
            display_name: "GP-200".into(),
        },
        capabilities: Capabilities::gp200_read_only(),
    }));
    assert!(apply(&mut app, gain(51.0)).is_empty());
    assert_eq!(
        app.state()
            .confirmed_buffer
            .as_ref()
            .unwrap()
            .view
            .module(ModuleId::new(3).unwrap())
            .unwrap()
            .parameters[0],
        F32Bits(50.0f32.to_bits())
    );
    let snapshot = app.document_snapshot(&device_document_id()).unwrap();
    let baseline = capture(0);
    for (index, record) in baseline.raw.records().iter().enumerate() {
        if record.command != 0x14 || record.body[0] != 3 {
            assert_eq!(record, &snapshot.raw.records()[index]);
        } else {
            assert_eq!(
                &record.body[12..],
                &snapshot.raw.records()[index].body[12..]
            );
        }
    }
}

#[test]
fn model_index_and_xml_parameter_ids_are_validated() {
    let mut app = connected();
    let mut wrong = gain(51.0);
    if let EditPatch::SetParameter { model_index, .. } = &mut wrong {
        *model_index = ModelIndex(0x07000015);
    }
    assert!(apply(&mut app, wrong).is_empty());
    assert_eq!(
        app.state().documents[&device_document_id()].local_revision,
        Revision(0)
    );
    // CAB display position zero is Volume at XML ID one; XML ID zero is not a control.
    let cab = app
        .active_document()
        .unwrap()
        .draft
        .view
        .module(ModuleId::new(5).unwrap())
        .unwrap();
    let model = app.catalog().model_by_id(5, cab.model.0).unwrap();
    let patch = EditPatch::SetParameter {
        module: ModuleId::new(5).unwrap(),
        model: cab.model,
        model_index: ModelIndex(u32::from(model.xml_index)),
        parameter: ParamId::new(0).unwrap(),
        value: F32Bits(61.0f32.to_bits()),
    };
    assert!(apply(&mut app, patch).is_empty());
    assert!(
        app.state()
            .messages
            .back()
            .unwrap()
            .message
            .contains("not declared")
    );
}

#[test]
fn timeout_after_send_is_indeterminate_and_is_not_replayed() {
    let mut app = connected();
    let stamp = command_stamp(&apply(&mut app, gain(51.0)));
    app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
        stamp,
        stage: EditStage::Sent,
    }));
    apply(&mut app, gain(52.0));
    app.dispatch(AppInput::Device(DeviceEvent::OperationFailed {
        stamp,
        error: ServiceError::new(ErrorKind::Timeout, "Readback timed out"),
    }));
    assert!(matches!(
        app.state().documents[&device_document_id()].pending[&stamp.intent].stage,
        EditStage::Indeterminate(_)
    ));
    assert!(apply(&mut app, gain(53.0)).is_empty());
    let refresh = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
    assert!(
        readback(
            &mut app,
            command_stamp(&refresh),
            edited(&capture(0), &[gain(51.0)], 1)
        )
        .is_empty()
    );
}

#[test]
fn save_needs_persistent_readback_fresh_current_context_and_verified_stage() {
    let mut app = connected();
    let stamp = command_stamp(&apply(&mut app, gain(51.0)));
    let current = edited(&capture(0), &[gain(51.0)], 1);
    readback(&mut app, stamp, current.clone());
    verified(&mut app, stamp);
    let slot = SlotId::new(25).unwrap();
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::SaveBufferTo {
            slot,
            expected_baseline: Revision(0)
        }))
        .is_empty()
    );
    let effects = app.dispatch(AppInput::Ui(UiIntent::SaveBufferTo {
        slot,
        expected_baseline: Revision(1),
    }));
    let save = command_stamp(&effects);
    verified(&mut app, save);
    assert!(matches!(
        app.state().buffer_persistence,
        BufferPersistence::Saving { .. }
    ));
    let mut stored = (*current).clone();
    stored.target = Some(PresetTarget::Stored(slot));
    stored.context = Some(WireContext {
        group: 1,
        selector: slot.get(),
    });
    app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
        stamp: save,
        target: PresetTarget::Stored(slot),
        snapshot: Arc::new(stored),
    }));
    assert!(matches!(
        app.state().buffer_persistence,
        BufferPersistence::Saving { .. }
    ));
    let mut payload = current.raw.payload().to_vec();
    payload[6..8].copy_from_slice(&slot.get().to_le_bytes());
    let fresh = Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_payload(0x12, payload).unwrap()),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(2),
    ));
    readback(&mut app, save, fresh);
    assert_eq!(
        app.state().buffer_persistence,
        BufferPersistence::Saved {
            slot,
            revision: Revision(2)
        }
    );
    assert!(
        !app.state()
            .document_status(&device_document_id())
            .unwrap()
            .changed_since_baseline
    );
    assert_eq!(
        app.state().confirmed_buffer.as_ref().unwrap().context,
        Some(WireContext {
            group: 0,
            selector: 25
        })
    );
    assert_eq!(
        command_stamp(&apply(&mut app, gain(52.0))).epoch,
        SessionEpoch(1)
    );
}

#[test]
fn physical_conflict_accepts_exact_buffer_and_retains_whole_local_work_offline() {
    let mut app = connected();
    app.dispatch(AppInput::Device(DeviceEvent::SessionChanged {
        epoch: SessionEpoch(1),
        identity: DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Hardware,
            firmware: None,
            display_name: "GP-200".into(),
        },
        capabilities: Capabilities::gp200_read_only(),
    }));
    apply(&mut app, gain(51.0));
    apply(&mut app, amp_parameter(1, 41.0));
    let before = app.state().documents[&device_document_id()].clone();
    let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
    let stamp = command_stamp(&effects);
    let actual = edited(&capture(0), &[gain(55.0)], 1);
    readback(&mut app, stamp, actual.clone());
    assert_eq!(
        app.state().documents[&device_document_id()].conflicts.len(),
        1
    );
    let effects = app.dispatch(AppInput::Ui(UiIntent::ResolveConflict(
        ConflictResolution::AcceptDevice(stamp.intent),
    )));
    assert!(
        effects.is_empty(),
        "Whole-buffer acceptance sends no mutation"
    );
    let physical = &app.state().documents[&device_document_id()];
    assert!(Arc::ptr_eq(&physical.baseline, &actual));
    assert_eq!(physical.draft, PresetDraft::from_snapshot(&actual));
    assert!(physical.undo.past.is_empty());
    let retained = app
        .state()
        .documents
        .values()
        .find(|document| matches!(document.target, EditTarget::Offline(_)))
        .expect("Entire prior work retained under a fresh local identity");
    assert_ne!(retained.id, device_document_id());
    assert_eq!(retained.target, EditTarget::Offline(retained.id.clone()));
    assert_eq!(retained.draft, before.draft);
    assert_eq!(retained.undo.past, before.undo.past);
    assert!(Arc::ptr_eq(&retained.baseline, &before.baseline));
    let module = app.state().documents[&device_document_id()]
        .draft
        .view
        .module(ModuleId::new(3).unwrap())
        .unwrap();
    assert_eq!(module.parameters[0], F32Bits(55.0f32.to_bits()));
    assert_eq!(
        module.parameters[1],
        actual
            .view
            .module(ModuleId::new(3).unwrap())
            .unwrap()
            .parameters[1]
    );
    let local = retained
        .draft
        .view
        .module(ModuleId::new(3).unwrap())
        .unwrap();
    assert_eq!(local.parameters[0], F32Bits(51.0f32.to_bits()));
    assert_eq!(local.parameters[1], F32Bits(41.0f32.to_bits()));
}

#[test]
fn reconnect_retains_detached_draft_and_marks_bank_stale() {
    let mut app = connected();
    apply(&mut app, gain(51.0));
    app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
        epoch: SessionEpoch(2),
        state: ConnectionState::Connecting,
    }));
    assert!(app.state().confirmed_buffer.is_none());
    assert!(!app.state().documents.contains_key(&device_document_id()));
    assert!(matches!(
        app.active_document().unwrap().target,
        EditTarget::Offline(_)
    ));
    let draft = app
        .active_document()
        .unwrap()
        .draft
        .view
        .module(ModuleId::new(3).unwrap())
        .unwrap();
    assert_eq!(draft.parameters[0], F32Bits(51.0f32.to_bits()));
}

#[test]
fn offline_comparison_history_and_theme_changes_are_independent() {
    let mut app = AppController::with_demo(capture(0), AppPreferences::default()).unwrap();
    let id = app.active_document_id().unwrap().clone();
    app.capture_comparison(&id, ComparisonSlot::A).unwrap();
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
            document: id.clone(),
            patch: gain(51.0)
        }))
        .is_empty()
    );
    app.capture_comparison(&id, ComparisonSlot::B).unwrap();
    app.recall_comparison(&id, ComparisonSlot::A);
    assert_eq!(
        app.active_document()
            .unwrap()
            .draft
            .view
            .module(ModuleId::new(3).unwrap())
            .unwrap()
            .parameters[0],
        F32Bits(50.0f32.to_bits())
    );
    app.dispatch(AppInput::Ui(UiIntent::Undo(id)));
    assert_eq!(
        app.active_document()
            .unwrap()
            .draft
            .view
            .module(ModuleId::new(3).unwrap())
            .unwrap()
            .parameters[0],
        F32Bits(51.0f32.to_bits())
    );
    let effects = app.dispatch(AppInput::Ui(UiIntent::SetPreferences(AppPreferences {
        theme: ThemePreference::Dark,
        ..Default::default()
    })));
    assert!(matches!(effects[0], Effect::PersistSettings(_)));
    assert_eq!(app.state().preferences.theme, ThemePreference::Dark);
    assert_eq!(
        app.active_document()
            .unwrap()
            .draft
            .view
            .module(ModuleId::new(3).unwrap())
            .unwrap()
            .parameters[0],
        F32Bits(51.0f32.to_bits())
    );
}

#[test]
fn local_model_selection_sets_only_declared_defaults_and_updates_asset_dependencies() {
    let mut app = AppController::with_demo(capture(0), AppPreferences::default()).unwrap();
    let id = app.active_document_id().unwrap().clone();
    let before = app
        .active_document()
        .unwrap()
        .draft
        .view
        .module(ModuleId::new(5).unwrap())
        .unwrap()
        .parameters
        .clone();
    let model = ModelCode(0x0a100003);
    app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: id.clone(),
        patch: EditPatch::SetModel {
            module: ModuleId::new(5).unwrap(),
            model,
        },
    }));
    let definition = app.catalog().model_by_id(5, model.0).unwrap();
    let after = &app
        .active_document()
        .unwrap()
        .draft
        .view
        .module(ModuleId::new(5).unwrap())
        .unwrap()
        .parameters;
    for (index, value) in before.iter().enumerate() {
        if let Some(default) = definition.default_values().get(&(index as u8)) {
            assert_eq!(after[index], F32Bits(default.to_bits()));
        } else {
            assert_eq!(after[index], *value);
        }
    }
    let snapshot = app.document_snapshot(&id).unwrap();
    assert!(snapshot.asset_refs.contains(&AssetRef::UserSlot {
        kind: AssetKind::ImpulseResponse,
        slot: 3,
        content: None
    }));
    assert!(!snapshot.asset_refs.contains(&AssetRef::Factory {
        module: ModuleId::new(5).unwrap(),
        model: ModelCode(0x0a000015)
    }));
}

#[test]
fn reviewed_stored_write_needs_fresh_destination_and_does_not_replace_buffer() {
    let mut app = connected();
    let slot = SlotId::new(255).unwrap();
    let read = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::Stored(slot))));
    let mut raw = capture(0).raw.payload().to_vec();
    raw[4..6].copy_from_slice(&1_u16.to_le_bytes());
    raw[6..8].copy_from_slice(&slot.get().to_le_bytes());
    let target = Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_payload(0x12, raw).unwrap()),
        SnapshotSource::Simulator,
        Some(PresetTarget::Stored(slot)),
        Revision(1),
    ));
    app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
        stamp: command_stamp(&read),
        target: PresetTarget::Stored(slot),
        snapshot: target,
    }));
    let source = edited(&capture(0), &[gain(55.0)], 2);
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::WriteStoredPreset {
            slot,
            expected_target_revision: Revision(0),
            snapshot: source.clone()
        }))
        .is_empty()
    );
    let effects = app.dispatch(AppInput::Ui(UiIntent::WriteStoredPreset {
        slot,
        expected_target_revision: Revision(1),
        snapshot: source.clone(),
    }));
    let Effect::Device(DeviceCommand::WriteStoredPreset { stamp, .. }) = effects[0] else {
        panic!("stored write");
    };
    verified(&mut app, stamp);
    assert_eq!(
        app.state().bank.slots[&slot]
            .snapshot
            .view
            .module(ModuleId::new(3).unwrap())
            .unwrap()
            .parameters[0]
            .as_f32(),
        50.0
    );
    let mut raw = source.raw.payload().to_vec();
    raw[4..6].copy_from_slice(&1_u16.to_le_bytes());
    raw[6..8].copy_from_slice(&slot.get().to_le_bytes());
    let stored = Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_payload(0x12, raw).unwrap()),
        SnapshotSource::Simulator,
        Some(PresetTarget::Stored(slot)),
        Revision(3),
    ));
    app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
        stamp,
        target: PresetTarget::Stored(slot),
        snapshot: stored,
    }));
    assert!(
        app.state()
            .messages
            .back()
            .unwrap()
            .message
            .contains("Wrote and verified")
    );
    assert_eq!(
        app.state().confirmed_buffer.as_ref().unwrap().context,
        Some(WireContext {
            group: 0,
            selector: 23
        })
    );
    assert_eq!(
        app.state()
            .confirmed_buffer
            .as_ref()
            .unwrap()
            .view
            .module(ModuleId::new(3).unwrap())
            .unwrap()
            .parameters[0]
            .as_f32(),
        50.0
    );
}

#[test]
fn selection_retains_actual_readback_but_needs_matching_context_and_verified() {
    let mut app = connected();
    let slot = SlotId::new(255).unwrap();
    let effects = app.dispatch(AppInput::Ui(UiIntent::AuditionStored(slot)));
    let Effect::Device(DeviceCommand::SelectStored { stamp, .. }) = effects[0] else {
        panic!("select");
    };
    readback(&mut app, stamp, capture(1));
    assert!(!matches!(
        app.state().buffer_persistence,
        BufferPersistence::Selected { .. }
    ));
    verified(&mut app, stamp);
    assert!(!matches!(
        app.state().buffer_persistence,
        BufferPersistence::Selected { .. }
    ));
    let effects = app.dispatch(AppInput::Ui(UiIntent::AuditionStored(slot)));
    let Effect::Device(DeviceCommand::SelectStored { stamp, .. }) = effects[0] else {
        panic!("select");
    };
    let mut raw = capture(1).raw.payload().to_vec();
    raw[6..8].copy_from_slice(&slot.get().to_le_bytes());
    let actual = Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_payload(0x12, raw).unwrap()),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(2),
    ));
    readback(&mut app, stamp, actual);
    assert!(!matches!(
        app.state().buffer_persistence,
        BufferPersistence::Selected { .. }
    ));
    verified(&mut app, stamp);
    assert!(
        matches!(app.state().buffer_persistence, BufferPersistence::Selected { slot:selected, .. } if selected == slot)
    );
}
