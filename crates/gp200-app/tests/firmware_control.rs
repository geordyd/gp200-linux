// Private capture acceptance suite; excluded from public source builds.
// Run with RUSTFLAGS="--cfg gp200_private_evidence" only in the retained lab checkout.
#![cfg(gp200_private_evidence)]

//! The actual pure reducer is exercised; no hardware/update permit is constructed.
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
    let Effect::Device(DeviceCommand::Connect { intent, .. }) = effects[0] else {
        panic!()
    };
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
            display_name: "Test simulator".into(),
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
    let effects = app.dispatch(AppInput::Ui(UiIntent::RefreshDeviceGlobals));
    let [Effect::Device(DeviceCommand::ReadGlobals(stamp))] = effects.as_slice() else {
        panic!("{effects:?}")
    };
    let raw = RawTransfer::from_payload(0x12, include_bytes!("../../../analysis/implementation/dependent-model-normal-root/amp-auto-cab-off/rust/undo-63-6.globals.payload.bin").to_vec()).unwrap();
    let globals = Arc::new(
        DeviceGlobalState::from_transfer(Revision(1), SessionEpoch(1), raw, vec![]).unwrap(),
    );
    app.dispatch(AppInput::Device(DeviceEvent::GlobalStateReceived {
        stamp: *stamp,
        state: globals,
    }));
    app
}
fn gesture_key() -> GestureKey {
    GestureKey {
        module: ModuleId::new(3).unwrap(),
        model: ModelCode(0x07000015),
        parameter: ParamId::new(0).unwrap(),
    }
}
fn gain(value: f32) -> EditPatch {
    EditPatch::SetParameter {
        module: ModuleId::new(3).unwrap(),
        model: ModelCode(0x07000015),
        model_index: ModelIndex(13),
        parameter: ParamId::new(0).unwrap(),
        value: F32Bits::from_f32(value),
    }
}
fn edited(base: &PresetSnapshot, patches: &[EditPatch], revision: u64) -> Arc<PresetSnapshot> {
    let mut draft = PresetDraft::from_snapshot(base);
    for patch in patches {
        draft.apply(patch).unwrap();
    }
    let mut snapshot = draft
        .snapshot(base, SnapshotSource::Simulator, Revision(revision))
        .unwrap();
    snapshot.target = Some(PresetTarget::EditBuffer);
    Arc::new(snapshot)
}
fn stamp(effects: &[Effect]) -> RequestStamp {
    match &effects[0] {
        Effect::Device(
            DeviceCommand::SubmitEdit { stamp, .. } | DeviceCommand::RefreshTarget { stamp, .. },
        ) => *stamp,
        _ => panic!("{effects:?}"),
    }
}
fn apply(app: &mut AppController, patch: EditPatch) -> Vec<Effect> {
    app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: device_document_id(),
        patch,
    }))
}
fn blockers(app: &AppController, lease: &FirmwareEditorLease) -> Vec<FirmwareEditorBlocker> {
    app.firmware_editor_readiness(lease).unwrap().blockers
}

fn keep_local_notes(app: &mut AppController) {
    assert!(
        apply(
            app,
            EditPatch::SetMetadata(MetadataPatch {
                notes: Some("Exact local work excluded from pedal capture".into()),
                ..Default::default()
            })
        )
        .is_empty()
    );
}

#[test]
fn explicit_exclusion_retains_exact_local_work_and_requires_a_new_choice_after_generation_change() {
    let mut app = connected();
    keep_local_notes(&mut app);
    let mut lease = app.begin_firmware_editor_preparation().unwrap();
    assert!(blockers(&app, &lease).contains(&FirmwareEditorBlocker::DirtyDeviceDraft));
    let exact = app.recovery_session();
    let mut wrong = exact.clone();
    wrong.documents[0].undo.past.clear();
    assert_ne!(wrong, exact);
    assert!(
        app.retain_firmware_local_draft_excluded(&lease, &wrong)
            .is_err()
    );
    app.retain_firmware_local_draft_excluded(&lease, &exact)
        .unwrap();
    assert_eq!(app.recovery_session(), exact);
    assert!(app.state().firmware_editor.local_draft_excluded);
    assert!(blockers(&app, &lease).is_empty());
    assert!(apply(&mut app, gain(8.0)).is_empty());
    assert_eq!(app.recovery_session(), exact);
    app.refresh_firmware_editor_preparation(&mut lease).unwrap();
    assert!(!app.state().firmware_editor.local_draft_excluded);
    assert!(blockers(&app, &lease).contains(&FirmwareEditorBlocker::DirtyDeviceDraft));
    app.retain_firmware_local_draft_excluded(&lease, &exact)
        .unwrap();
    app.seal_firmware_editor_preparation(&lease, &exact)
        .unwrap();
    assert!(app.state().identity.as_ref().unwrap().firmware.is_none());
}

#[test]
fn exclusion_does_not_settle_pending_uncertainty_gestures_or_other_controller_owners() {
    let mut app = connected();
    app.dispatch(AppInput::Ui(UiIntent::BeginGesture {
        document: device_document_id(),
        key: gesture_key(),
    }));
    let effects = apply(&mut app, gain(8.0));
    let original = stamp(&effects);
    app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
        stamp: original,
        stage: EditStage::Sent,
    }));
    apply(&mut app, gain(9.0));
    let mut lease = app.begin_firmware_editor_preparation().unwrap();
    let exact = app.recovery_session();
    app.retain_firmware_local_draft_excluded(&lease, &exact)
        .unwrap();
    assert_eq!(app.recovery_session(), exact);
    assert!(
        blockers(&app, &lease)
            .iter()
            .any(|b| matches!(b, FirmwareEditorBlocker::DocumentPending { .. }))
    );
    assert!(blockers(&app, &lease).contains(&FirmwareEditorBlocker::DeferredEdits(1)));
    assert!(
        blockers(&app, &lease).contains(&FirmwareEditorBlocker::OpenGesture(device_document_id()))
    );
    assert!(
        app.seal_firmware_editor_preparation(&lease, &exact)
            .is_err()
    );
    app.dispatch(AppInput::Device(DeviceEvent::OperationFailed {
        stamp: original,
        error: ServiceError::new(ErrorKind::Io, "Actual offered operation became uncertain"),
    }));
    app.refresh_firmware_editor_preparation(&mut lease).unwrap();
    let uncertain = app.recovery_session();
    app.retain_firmware_local_draft_excluded(&lease, &uncertain)
        .unwrap();
    assert_eq!(app.recovery_session(), uncertain);
    assert!(blockers(&app, &lease).contains(&FirmwareEditorBlocker::DeviceUncertainty));
    assert!(
        app.seal_firmware_editor_preparation(&lease, &uncertain)
            .is_err()
    );
    let mut another = connected();
    assert!(
        another
            .retain_firmware_local_draft_excluded(&lease, &another.recovery_session())
            .is_err()
    );
}

fn begin_normal_refresh(app: &mut AppController, lease: &FirmwareEditorLease) -> IntentId {
    let effects = app
        .begin_firmware_editor_normal_refresh(lease, SessionEpoch(1), SessionEpoch(2), selection())
        .unwrap();
    let [
        Effect::Device(DeviceCommand::Connect {
            intent,
            selection: actual,
        }),
    ] = effects.as_slice()
    else {
        panic!("{effects:?}")
    };
    assert_eq!(actual, &selection());
    *intent
}
fn observe_refresh_connect(app: &mut AppController, intent: IntentId) {
    app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
        epoch: SessionEpoch(3),
        state: ConnectionState::Connecting,
    }));
    app.dispatch(AppInput::Device(DeviceEvent::SessionChanged {
        epoch: SessionEpoch(3),
        identity: DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Simulator,
            firmware: None,
            display_name: "Fresh simulator".into(),
        },
        capabilities: Capabilities::simulator(),
    }));
    app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
        epoch: SessionEpoch(3),
        state: ConnectionState::Ready,
    }));
    app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
        stamp: RequestStamp {
            epoch: SessionEpoch(3),
            intent,
        },
        target: PresetTarget::EditBuffer,
        snapshot: capture(2),
    }));
}
fn refresh_reads(
    app: &mut AppController,
    lease: &FirmwareEditorLease,
) -> (RequestStamp, RequestStamp) {
    let effects = app
        .request_firmware_editor_normal_refresh_reads(lease)
        .unwrap();
    let [
        Effect::Device(DeviceCommand::RefreshTarget {
            stamp: current,
            target: PresetTarget::EditBuffer,
        }),
        Effect::Device(DeviceCommand::ReadGlobals(globals)),
    ] = effects.as_slice()
    else {
        panic!("{effects:?}")
    };
    assert_eq!(current.epoch, SessionEpoch(3));
    assert_eq!(globals.epoch, SessionEpoch(3));
    assert_ne!(current.intent, globals.intent);
    (*current, *globals)
}
fn fresh_globals(stamp: RequestStamp) -> DeviceEvent {
    let raw = RawTransfer::from_payload(0x12, include_bytes!("../../../analysis/implementation/dependent-model-normal-root/amp-auto-cab-off/rust/undo-63-6.globals.payload.bin").to_vec()).unwrap();
    DeviceEvent::GlobalStateReceived {
        stamp,
        state: Arc::new(
            DeviceGlobalState::from_transfer(Revision(2), stamp.epoch, raw, vec![]).unwrap(),
        ),
    }
}

#[test]
fn deliberate_normal_refresh_requires_new_registered_results_and_retains_local_raw_history() {
    let mut app = connected();
    keep_local_notes(&mut app);
    app.capture_comparison(&device_document_id(), ComparisonSlot::A)
        .unwrap();
    let old = app.recovery_session();
    let lease = app.begin_firmware_editor_preparation().unwrap();
    app.retain_firmware_local_draft_excluded(&lease, &old)
        .unwrap();
    app.seal_firmware_editor_preparation(&lease, &old).unwrap();
    let connect = begin_normal_refresh(&mut app, &lease);
    assert!(app.finish_firmware_editor_normal_refresh(&lease).is_err());
    assert!(
        app.request_firmware_editor_normal_refresh_reads(&lease)
            .is_err()
    );
    observe_refresh_connect(&mut app, connect);
    assert!(app.finish_firmware_editor_normal_refresh(&lease).is_err());
    let (current, globals) = refresh_reads(&mut app, &lease);
    assert!(
        app.request_firmware_editor_normal_refresh_reads(&lease)
            .is_err()
    );
    app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
        stamp: current,
        target: PresetTarget::EditBuffer,
        snapshot: capture(3),
    }));
    assert!(app.finish_firmware_editor_normal_refresh(&lease).is_err());
    app.dispatch(AppInput::Device(fresh_globals(globals)));
    assert!(app.firmware_editor_blocks_normal_actions());
    app.finish_firmware_editor_normal_refresh(&lease).unwrap();
    assert!(!app.firmware_editor_blocks_normal_actions());
    assert_eq!(app.state().epoch, SessionEpoch(3));
    assert!(app.state().identity.as_ref().unwrap().firmware.is_none());
    let retained = app
        .recovery_session()
        .documents
        .into_iter()
        .find(|d| d.draft.metadata.notes == old.documents[0].draft.metadata.notes)
        .unwrap();
    assert_eq!(retained.baseline, old.documents[0].baseline);
    assert_eq!(retained.draft, old.documents[0].draft);
    assert_eq!(retained.undo, old.documents[0].undo);
    assert_eq!(retained.comparison, old.documents[0].comparison);
    assert!(matches!(retained.original_target, EditTarget::Offline(_)));
    assert!(app.finish_firmware_editor_normal_refresh(&lease).is_err());
}

#[test]
fn stale_epochs_foreign_leases_and_wrong_stamps_do_not_release_normal_admission() {
    let mut app = connected();
    let lease = app.begin_firmware_editor_preparation().unwrap();
    assert!(
        app.begin_firmware_editor_normal_refresh(
            &lease,
            SessionEpoch(1),
            SessionEpoch(1),
            selection()
        )
        .is_err()
    );
    let foreign = connected().begin_firmware_editor_preparation().unwrap();
    assert!(
        app.begin_firmware_editor_normal_refresh(
            &foreign,
            SessionEpoch(1),
            SessionEpoch(2),
            selection()
        )
        .is_err()
    );
    let connect = begin_normal_refresh(&mut app, &lease);
    app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
        epoch: SessionEpoch(1),
        state: ConnectionState::Ready,
    }));
    assert!(
        app.request_firmware_editor_normal_refresh_reads(&lease)
            .is_err()
    );
    observe_refresh_connect(&mut app, connect);
    let (current, globals) = refresh_reads(&mut app, &lease);
    let wrong = RequestStamp {
        intent: IntentId(globals.intent.0 + 50),
        ..globals
    };
    app.dispatch(AppInput::Device(fresh_globals(wrong)));
    app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
        stamp: RequestStamp {
            epoch: SessionEpoch(1),
            ..current
        },
        target: PresetTarget::EditBuffer,
        snapshot: capture(3),
    }));
    assert!(app.finish_firmware_editor_normal_refresh(&lease).is_err());
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::Connect(selection())))
            .is_empty()
    );
    assert!(app.firmware_editor_blocks_normal_actions());
}

#[test]
fn failed_refresh_and_recovery_hold_cannot_be_released_by_later_ordinary_success() {
    for recovery_hold in [false, true] {
        let mut app = connected();
        let lease = app.begin_firmware_editor_preparation().unwrap();
        let connect = begin_normal_refresh(&mut app, &lease);
        observe_refresh_connect(&mut app, connect);
        let (current, globals) = refresh_reads(&mut app, &lease);
        if recovery_hold {
            app.hold_firmware_editor_for_recovery(&lease, "Actual owner still unresolved".into())
                .unwrap();
        } else {
            app.dispatch(AppInput::Device(DeviceEvent::OperationFailed {
                stamp: globals,
                error: ServiceError::new(ErrorKind::Timeout, "Original refresh failed"),
            }));
        }
        app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
            stamp: current,
            target: PresetTarget::EditBuffer,
            snapshot: capture(3),
        }));
        app.dispatch(AppInput::Device(fresh_globals(globals)));
        assert!(app.finish_firmware_editor_normal_refresh(&lease).is_err());
        assert!(app.cancel_firmware_editor_preparation(&lease).is_err());
        assert!(app.firmware_editor_blocks_normal_actions());
    }
}

#[test]
fn exact_recovery_description_can_seal_local_editor_but_does_not_resume_or_set_firmware() {
    let mut app = connected();
    let lease = app.begin_firmware_editor_preparation().unwrap();
    assert!(
        blockers(&app, &lease).is_empty(),
        "{:?}",
        blockers(&app, &lease)
    );
    let mut wrong = app.recovery_session();
    wrong.documents[0].draft.metadata.notes = Some("changed retained bytes".into());
    assert!(
        app.seal_firmware_editor_preparation(&lease, &wrong)
            .is_err()
    );
    app.seal_firmware_editor_preparation(&lease, &app.recovery_session())
        .unwrap();
    assert_eq!(
        app.state().firmware_editor.phase,
        FirmwareEditorPhase::Sealed
    );
    assert!(app.cancel_firmware_editor_preparation(&lease).is_err());
    assert!(app.state().identity.as_ref().unwrap().firmware.is_none());
    assert!(apply(&mut app, gain(9.0)).is_empty());
}

#[test]
fn frozen_ui_and_direct_helpers_preserve_exact_drafts_history_comparisons_and_selection() {
    let mut app = connected();
    app.dispatch(AppInput::Ui(UiIntent::NewOfflineFrom(capture(10))));
    let offline = app.active_document_id().unwrap().clone();
    app.capture_comparison(&offline, ComparisonSlot::A).unwrap();
    let original = app.recovery_session();
    let lease = app.begin_firmware_editor_preparation().unwrap();
    let intents = vec![
        UiIntent::DiscoverDevices,
        UiIntent::Connect(selection()),
        UiIntent::Disconnect,
        UiIntent::EnterOfflineMode,
        UiIntent::Refresh(PresetTarget::EditBuffer),
        UiIntent::PreviewStored(SlotId::new(0).unwrap()),
        UiIntent::AuditionStored(SlotId::new(0).unwrap()),
        UiIntent::NewOfflineFrom(capture(20)),
        UiIntent::Undo(offline.clone()),
        UiIntent::Redo(offline.clone()),
        UiIntent::DiscardDraft(offline.clone()),
        UiIntent::ApplyEdit {
            document: offline.clone(),
            patch: gain(7.0),
        },
        UiIntent::BeginGesture {
            document: offline.clone(),
            key: gesture_key(),
        },
        UiIntent::EndGesture {
            document: offline.clone(),
            key: gesture_key(),
        },
        UiIntent::RefreshDeviceGlobals,
        UiIntent::ResolveConflict(ConflictResolution::KeepLocal(IntentId(10))),
        UiIntent::CancelJob(JobId(1)),
        UiIntent::Library(LibraryCommand::Query {
            intent: IntentId(90),
            query: LibraryQuery::default(),
        }),
        UiIntent::StartFirmwareJob(FirmwareJobSpec {
            id: JobId(2),
            source: "/tmp/inert.bin".into(),
        }),
    ];
    for intent in intents {
        assert!(app.dispatch(AppInput::Ui(intent)).is_empty());
    }
    assert!(!app.activate_document(&device_document_id()));
    assert!(app.capture_comparison(&offline, ComparisonSlot::B).is_err());
    assert!(
        app.recall_comparison(&offline, ComparisonSlot::A)
            .is_empty()
    );
    assert!(app.stage_document_in_buffer(&offline).is_empty());
    assert!(
        app.execute_prepared_job(JobId(2), JobSha256::of(b"unused"))
            .is_empty()
    );
    assert_eq!(app.recovery_session(), original);
    assert_eq!(app.active_document_id(), Some(&offline));
    assert!(app.document_snapshot(&offline).is_ok());
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::Navigate(ViewId::Diagnostics)))
            .is_empty()
    );
    let mut preferences = app.state().preferences.clone();
    preferences.theme = ThemePreference::Dark;
    assert!(matches!(
        app.dispatch(AppInput::Ui(UiIntent::SetPreferences(preferences)))
            .as_slice(),
        [Effect::PersistSettings(_)]
    ));
    assert!(app.dispatch(AppInput::Tick(9000)).is_empty());
    app.cancel_firmware_editor_preparation(&lease).unwrap();
    assert!(app.firmware_editor_readiness(&lease).is_err());
    let next = app.begin_firmware_editor_preparation().unwrap();
    assert!(next.generation() > lease.generation());
}

#[test]
fn public_pristine_recovery_replacement_is_blocked_before_any_removal_or_insert() {
    let mut app = AppController::with_demo(capture(0), AppPreferences::default()).unwrap();
    let id = app.active_document_id().unwrap().clone();
    let session = app.recovery_session();
    assert!(app.can_replace_pristine_offline(&id));
    let lease = app.begin_firmware_editor_preparation().unwrap();
    assert!(
        app.recover_offline_replacing_pristine(&session, Some(&id))
            .is_err()
    );
    assert!(app.recover_offline(&session).is_err());
    assert_eq!(app.recovery_session(), session);
    app.cancel_firmware_editor_preparation(&lease).unwrap();
    assert_eq!(
        app.recover_offline_replacing_pristine(&session, Some(&id))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn offered_edit_settles_original_readback_and_leaves_unoffered_delta_without_continuation() {
    let mut app = connected();
    let first = gain(10.0);
    let later = gain(11.0);
    let first_stamp = stamp(&apply(&mut app, first.clone()));
    app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
        stamp: first_stamp,
        stage: EditStage::Sent,
    }));
    assert!(apply(&mut app, later.clone()).is_empty());
    let mut lease = app.begin_firmware_editor_preparation().unwrap();
    assert!(
        blockers(&app, &lease)
            .iter()
            .any(|b| matches!(b, FirmwareEditorBlocker::PendingDeviceRequests(1)))
    );
    let actual = edited(&capture(0), &[first], 1);
    assert!(
        app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
            stamp: first_stamp,
            target: PresetTarget::EditBuffer,
            snapshot: actual
        }))
        .is_empty()
    );
    assert!(
        app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
            stamp: first_stamp,
            stage: EditStage::Verified
        }))
        .is_empty()
    );
    assert!(matches!(
        app.state().documents[&device_document_id()].pending[&first_stamp.intent].stage,
        EditStage::Verified
    ));
    assert!(blockers(&app, &lease).contains(&FirmwareEditorBlocker::SourceChanged));
    app.refresh_firmware_editor_preparation(&mut lease).unwrap();
    let remaining = blockers(&app, &lease);
    assert!(
        !remaining
            .iter()
            .any(|b| matches!(b, FirmwareEditorBlocker::PendingDeviceRequests(_)))
    );
    assert!(remaining.contains(&FirmwareEditorBlocker::DeferredEdits(1)));
    assert!(remaining.contains(&FirmwareEditorBlocker::DirtyDeviceDraft));
    assert!(app.dispatch(AppInput::Tick(10000)).is_empty());
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::Undo(device_document_id())))
            .is_empty()
    );
}

#[test]
fn final_read_only_result_needs_explicit_new_source_generation_and_current_recovery_description() {
    let mut app = connected();
    let read_stamp =
        stamp(&app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer))));
    let old_session = app.recovery_session();
    let mut lease = app.begin_firmware_editor_preparation().unwrap();
    assert!(
        app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
            stamp: read_stamp,
            target: PresetTarget::EditBuffer,
            snapshot: capture(1)
        }))
        .is_empty()
    );
    assert!(
        app.seal_firmware_editor_preparation(&lease, &old_session)
            .is_err()
    );
    let old_generation = lease.generation();
    let source = app.refresh_firmware_editor_preparation(&mut lease).unwrap();
    assert!(lease.generation() > old_generation);
    assert_eq!(source.buffer.unwrap().revision, Revision(1));
    assert!(
        blockers(&app, &lease).is_empty(),
        "{:?}",
        blockers(&app, &lease)
    );
    app.seal_firmware_editor_preparation(&lease, &app.recovery_session())
        .unwrap();
}

#[test]
fn epoch_loss_retains_work_and_never_releases_the_barrier_or_stale_caches() {
    let mut app = connected();
    let offered = stamp(&apply(&mut app, gain(10.0)));
    app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
        stamp: offered,
        stage: EditStage::Sent,
    }));
    apply(&mut app, gain(11.0));
    let mut lease = app.begin_firmware_editor_preparation().unwrap();
    assert!(
        app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
            epoch: SessionEpoch(2),
            state: ConnectionState::Recovering
        }))
        .is_empty()
    );
    assert!(app.firmware_editor_blocks_normal_actions());
    assert!(app.state().confirmed_buffer.is_none());
    assert!(app.state().documents.values().any(|d| {
        matches!(d.target, EditTarget::Offline(_))
            && d.pending
                .values()
                .any(|p| matches!(p.stage, EditStage::Indeterminate(_)))
    }));
    app.refresh_firmware_editor_preparation(&mut lease).unwrap();
    let blocked = blockers(&app, &lease);
    assert!(blocked.contains(&FirmwareEditorBlocker::DeferredEdits(1)));
    assert!(blocked.contains(&FirmwareEditorBlocker::ConnectionNotReady));
    app.hold_firmware_editor_for_recovery(&lease, "Owner retirement was indeterminate".into())
        .unwrap();
    assert!(app.cancel_firmware_editor_preparation(&lease).is_err());
    assert!(app.refresh_firmware_editor_preparation(&mut lease).is_err());
    assert_eq!(
        app.state().firmware_editor.phase,
        FirmwareEditorPhase::RecoveryRequired
    );
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::Connect(selection())))
            .is_empty()
    );
}

#[test]
fn leases_are_controller_bound_and_a_cloned_render_status_cannot_unfreeze() {
    let mut first = connected();
    let mut other = connected();
    let lease = first.begin_firmware_editor_preparation().unwrap();
    assert!(other.cancel_firmware_editor_preparation(&lease).is_err());
    assert!(other.firmware_editor_readiness(&lease).is_err());
    let mut fake = first.state().clone();
    fake.firmware_editor = FirmwareEditorStatus::default();
    assert_eq!(fake.firmware_editor.phase, FirmwareEditorPhase::Normal);
    assert!(apply(&mut first, gain(9.0)).is_empty());
    assert!(first.begin_firmware_editor_preparation().is_err());
}

#[test]
fn actual_local_library_and_asset_effects_block_until_matching_terminal_results() {
    let mut app = connected();
    let intent = IntentId(700);
    assert!(matches!(
        app.dispatch(AppInput::Ui(UiIntent::Library(LibraryCommand::Query {
            intent,
            query: LibraryQuery::default()
        })))
        .as_slice(),
        [Effect::Library(_)]
    ));
    app.dispatch(AppInput::Ui(UiIntent::StartAssetJob(AssetJobSpec {
        id: JobId(20),
        kind: AssetKind::ImpulseResponse,
        source: "/tmp/inert.wav".into(),
        output: None,
        options: Default::default(),
    })));
    let mut lease = app.begin_firmware_editor_preparation().unwrap();
    assert!(blockers(&app, &lease).contains(&FirmwareEditorBlocker::PendingLibraryEffects(1)));
    assert!(blockers(&app, &lease).contains(&FirmwareEditorBlocker::PendingAssetEffects(1)));
    app.dispatch(AppInput::Library(LibraryEvent::QueryResult {
        intent: IntentId(701),
        entries: vec![],
    }));
    app.dispatch(AppInput::Assets(AssetEvent::Failed {
        job: JobId(21),
        error: ServiceError::invalid("unrelated job"),
    }));
    assert!(blockers(&app, &lease).contains(&FirmwareEditorBlocker::PendingLibraryEffects(1)));
    assert!(blockers(&app, &lease).contains(&FirmwareEditorBlocker::PendingAssetEffects(1)));
    app.dispatch(AppInput::Library(LibraryEvent::QueryResult {
        intent,
        entries: vec![],
    }));
    app.dispatch(AppInput::Assets(AssetEvent::Failed {
        job: JobId(20),
        error: ServiceError::invalid("Original failure retained"),
    }));
    app.refresh_firmware_editor_preparation(&mut lease).unwrap();
    assert!(
        blockers(&app, &lease).is_empty(),
        "{:?}",
        blockers(&app, &lease)
    );
    assert!(
        app.state()
            .messages
            .iter()
            .any(|m| m.message == "Original failure retained")
    );
}

#[test]
fn delayed_library_open_is_retained_and_requires_an_explicit_normal_action() {
    let mut app = connected();
    let version_id = LibraryVersionId::from("late-version");
    let effects = app.dispatch(AppInput::Ui(UiIntent::OpenLibraryVersion(
        version_id.clone(),
    )));
    let [Effect::Library(LibraryCommand::LoadVersion { intent, .. })] = effects.as_slice() else {
        panic!()
    };
    let intent = *intent;
    let original = app.recovery_session();
    let lease = app.begin_firmware_editor_preparation().unwrap();
    let version = LibraryVersion {
        id: version_id.clone(),
        library: LibraryId::from("late-library"),
        parent: None,
        snapshot: capture(7),
        metadata: LibraryMetadata::default(),
        created_at: "descriptive test date".into(),
    };
    assert!(
        app.dispatch(AppInput::Library(LibraryEvent::VersionLoaded {
            intent,
            version
        }))
        .is_empty()
    );
    assert_eq!(app.recovery_session(), original);
    assert!(app.state().library.versions.contains_key(&version_id));
    assert!(blockers(&app, &lease).contains(&FirmwareEditorBlocker::RetainedDocumentOpenings(1)));
    assert!(app.resume_retained_document_opening(intent).is_empty());
    assert_eq!(app.recovery_session(), original);
    app.cancel_firmware_editor_preparation(&lease).unwrap();
    app.resume_retained_document_opening(intent);
    assert_eq!(app.state().documents.len(), original.documents.len() + 1);
    assert_ne!(app.active_document_id(), original.active_document.as_ref());
}

#[test]
fn open_gesture_and_failure_are_not_silently_reconciled_to_obtain_readiness() {
    let mut app = connected();
    let key = gesture_key();
    app.dispatch(AppInput::Ui(UiIntent::BeginGesture {
        document: device_document_id(),
        key: key.clone(),
    }));
    let offered = stamp(&apply(&mut app, gain(10.0)));
    app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
        stamp: offered,
        stage: EditStage::Sent,
    }));
    apply(&mut app, gain(11.0));
    let lease = app.begin_firmware_editor_preparation().unwrap();
    let original = app.recovery_session();
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::EndGesture {
            document: device_document_id(),
            key
        }))
        .is_empty()
    );
    assert_eq!(app.recovery_session(), original);
    assert!(
        app.dispatch(AppInput::Device(DeviceEvent::OperationFailed {
            stamp: offered,
            error: ServiceError::new(ErrorKind::Io, "original send failure")
        }))
        .is_empty()
    );
    let pending = &app.state().documents[&device_document_id()].pending[&offered.intent];
    assert!(matches!(pending.stage, EditStage::Indeterminate(_)));
    let blocked = blockers(&app, &lease);
    assert!(blocked.contains(&FirmwareEditorBlocker::DeferredEdits(1)));
    assert!(blocked.contains(&FirmwareEditorBlocker::OpenGesture(device_document_id())));
    assert!(blocked.contains(&FirmwareEditorBlocker::DeviceUncertainty));
    assert!(
        blocked
            .iter()
            .any(|b| matches!(b, FirmwareEditorBlocker::DocumentPending { count: 1, .. }))
    );
}

#[test]
fn notification_hints_and_ticks_remain_pending_without_scheduling_a_read() {
    let mut app = connected();
    let lease = app.begin_firmware_editor_preparation().unwrap();
    let raw = RawTransfer::from_records(
        0x12,
        &[RawRecord {
            command: 0x0f,
            body: vec![0; 8],
        }],
    )
    .unwrap();
    assert!(
        app.dispatch(AppInput::Device(DeviceEvent::IncomingNotification {
            epoch: SessionEpoch(1),
            raw: Arc::new(raw)
        }))
        .is_empty()
    );
    assert!(app.external_reconciliation_pending());
    assert!(app.dispatch(AppInput::Tick(5000)).is_empty());
    assert!(app.dispatch(AppInput::Tick(15000)).is_empty());
    assert!(blockers(&app, &lease).contains(&FirmwareEditorBlocker::ExternalRefresh));
    assert!(
        app.state()
            .diagnostics
            .iter()
            .any(|d| d.category == "notification" && d.bytes.is_some())
    );
}

#[test]
fn pending_external_review_survives_connection_reset_and_remains_unavailable_while_frozen() {
    let mut app = connected();
    let offered = stamp(&apply(&mut app, gain(10.0)));
    app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
        stamp: offered,
        stage: EditStage::Sent,
    }));
    app.dispatch(AppInput::Device(DeviceEvent::OperationFailed {
        stamp: offered,
        error: ServiceError::new(ErrorKind::Io, "read manually"),
    }));
    let read_stamp =
        stamp(&app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer))));
    app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
        stamp: read_stamp,
        target: PresetTarget::EditBuffer,
        snapshot: edited(&capture(0), &[gain(8.0)], 1),
    }));
    assert!(app.external_conflict(read_stamp.intent));
    let original = app.state().documents[&device_document_id()].conflicts.len();
    assert!(original != 0);
    let lease = app.begin_firmware_editor_preparation().unwrap();
    assert!(!app.external_conflict_ready(read_stamp.intent));
    assert!(
        app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
            epoch: SessionEpoch(1),
            state: ConnectionState::Recovering
        }))
        .is_empty()
    );
    assert!(app.external_conflict(read_stamp.intent));
    assert_eq!(
        app.state().documents[&device_document_id()].conflicts.len(),
        original
    );
    assert!(blockers(&app, &lease).contains(&FirmwareEditorBlocker::ExternalReviews(1)));
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::ResolveConflict(
            ConflictResolution::AcceptDevice(read_stamp.intent)
        )))
        .is_empty()
    );
    assert!(app.external_conflict(read_stamp.intent));
}

#[test]
fn epoch_retirement_keeps_original_non_document_write_uncertainty_with_its_stamp() {
    let mut app = connected();
    let effects = app.dispatch(AppInput::Ui(UiIntent::SaveBufferTo {
        slot: SlotId::new(10).unwrap(),
        expected_baseline: Revision(0),
    }));
    let [Effect::Device(DeviceCommand::SaveBufferTo { stamp, .. })] = effects.as_slice() else {
        panic!("{effects:?}")
    };
    let stamp = *stamp;
    let before = app.recovery_session();
    assert!(
        before
            .session_uncertainty
            .iter()
            .any(|text| text.contains(&format!(
                "epoch {}, intent {}",
                stamp.epoch.0, stamp.intent.0
            )))
    );
    let mut lease = app.begin_firmware_editor_preparation().unwrap();
    assert!(
        app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
            epoch: SessionEpoch(2),
            state: ConnectionState::Recovering
        }))
        .is_empty()
    );
    for description in &before.session_uncertainty {
        assert!(
            app.state()
                .recovered_session_uncertainty
                .contains(description)
        );
    }
    let retained_count = app.state().recovered_session_uncertainty.len();
    app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
        epoch: SessionEpoch(2),
        state: ConnectionState::Recovering,
    }));
    assert_eq!(
        app.state().recovered_session_uncertainty.len(),
        retained_count
    );
    app.refresh_firmware_editor_preparation(&mut lease).unwrap();
    assert!(blockers(&app, &lease).iter().any(|b| matches!(b, FirmwareEditorBlocker::RecoveredUncertainty(n) if *n >= before.session_uncertainty.len())));
    assert!(
        app.seal_firmware_editor_preparation(&lease, &app.recovery_session())
            .is_err()
    );
}

#[test]
fn explicit_fresh_normal_check_after_recovery_requires_all_new_receipts_and_surfaces_failure() {
    for failed in [false, true] {
        let mut app = connected();
        let lease = app.begin_firmware_editor_preparation().unwrap();
        app.hold_firmware_editor_for_recovery(
            &lease,
            "Actual returned owner still needs a fresh normal check".into(),
        )
        .unwrap();
        assert!(app.cancel_firmware_editor_preparation(&lease).is_err());
        let connect = begin_normal_refresh(&mut app, &lease);
        assert!(app.finish_firmware_editor_normal_refresh(&lease).is_err());
        observe_refresh_connect(&mut app, connect);
        let (current, globals) = refresh_reads(&mut app, &lease);
        assert!(
            !app.firmware_editor_normal_refresh_current_received(&lease)
                .unwrap()
        );
        app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
            stamp: current,
            target: PresetTarget::EditBuffer,
            snapshot: capture(3),
        }));
        assert!(
            app.firmware_editor_normal_refresh_current_received(&lease)
                .unwrap()
        );
        assert!(app.finish_firmware_editor_normal_refresh(&lease).is_err());
        if failed {
            app.dispatch(AppInput::Device(DeviceEvent::OperationFailed {
                stamp: globals,
                error: ServiceError::new(
                    ErrorKind::Disconnected,
                    "Actual fresh settings read failed",
                ),
            }));
            assert_eq!(
                app.finish_firmware_editor_normal_refresh(&lease)
                    .unwrap_err()
                    .kind,
                ErrorKind::Indeterminate
            );
            assert!(app.firmware_editor_blocks_normal_actions());
        } else {
            app.dispatch(AppInput::Device(fresh_globals(globals)));
            app.finish_firmware_editor_normal_refresh(&lease).unwrap();
            assert!(!app.firmware_editor_blocks_normal_actions());
        }
    }
}

#[test]
fn disconnected_recovery_barrier_preserves_exact_local_draft_without_normal_capture() {
    let mut app = AppController::with_demo(capture(0), AppPreferences::default()).unwrap();
    let document = app.active_document_id().unwrap().clone();
    app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: document.clone(),
        patch: EditPatch::SetMetadata(MetadataPatch {
            notes: Some("Offline work retained during boot-only recovery".into()),
            ..Default::default()
        }),
    }));
    let original = app.recovery_session();
    let lease = app.begin_firmware_recovery_preparation().unwrap();
    let source = app.firmware_editor_bound_source(&lease).unwrap();
    assert!(source.identity.is_none() && source.buffer.is_none() && source.globals.is_none());
    assert!(blockers(&app, &lease).is_empty());
    app.retain_firmware_local_draft_excluded(&lease, &original)
        .unwrap();
    app.seal_firmware_recovery_preparation(&lease, &original)
        .unwrap();
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::Connect(selection())))
            .is_empty()
    );
    assert_eq!(app.recovery_session(), original);
    assert!(
        app.begin_firmware_editor_normal_refresh(
            &lease,
            SessionEpoch(0),
            SessionEpoch(2),
            selection()
        )
        .is_err()
    );
    assert!(app.cancel_firmware_editor_preparation(&lease).is_err());
    assert!(app.state().identity.is_none());
}

#[test]
fn disconnected_recovery_still_blocks_pending_connect_and_open_local_gesture() {
    let mut app = AppController::with_demo(capture(0), AppPreferences::default()).unwrap();
    let document = app.active_document_id().unwrap().clone();
    app.dispatch(AppInput::Ui(UiIntent::BeginGesture {
        document: document.clone(),
        key: gesture_key(),
    }));
    let lease = app.begin_firmware_recovery_preparation().unwrap();
    let original = app.recovery_session();
    app.retain_firmware_local_draft_excluded(&lease, &original)
        .unwrap();
    assert!(blockers(&app, &lease).contains(&FirmwareEditorBlocker::OpenGesture(document)));
    assert!(
        app.seal_firmware_recovery_preparation(&lease, &original)
            .is_err()
    );
    assert_eq!(app.recovery_session(), original);

    let mut pending = AppController::new(Catalog::embedded().unwrap(), AppPreferences::default());
    assert!(
        !pending
            .dispatch(AppInput::Ui(UiIntent::Connect(selection())))
            .is_empty()
    );
    let lease = pending.begin_firmware_recovery_preparation().unwrap();
    let original = pending.recovery_session();
    pending
        .retain_firmware_local_draft_excluded(&lease, &original)
        .unwrap();
    assert!(blockers(&pending, &lease).contains(&FirmwareEditorBlocker::PendingConnect));
    assert!(
        pending
            .seal_firmware_recovery_preparation(&lease, &original)
            .is_err()
    );
}

#[test]
fn disconnected_recovery_release_requires_actual_new_connect_identity_current_and_globals() {
    let mut app = AppController::with_demo(capture(0), AppPreferences::default()).unwrap();
    let original = app.recovery_session();
    let lease = app.begin_firmware_recovery_preparation().unwrap();
    app.retain_firmware_local_draft_excluded(&lease, &original)
        .unwrap();
    app.seal_firmware_recovery_preparation(&lease, &original)
        .unwrap();
    let effects = app
        .begin_firmware_recovery_normal_refresh(
            &lease,
            SessionEpoch(0),
            SessionEpoch(2),
            selection(),
        )
        .unwrap();
    let [Effect::Device(DeviceCommand::Connect { intent, .. })] = effects.as_slice() else {
        panic!("{effects:?}")
    };
    assert!(app.finish_firmware_editor_normal_refresh(&lease).is_err());
    observe_refresh_connect(&mut app, *intent);
    assert!(app.finish_firmware_editor_normal_refresh(&lease).is_err());
    let (current, globals) = refresh_reads(&mut app, &lease);
    app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
        stamp: current,
        target: PresetTarget::EditBuffer,
        snapshot: capture(3),
    }));
    assert!(app.finish_firmware_editor_normal_refresh(&lease).is_err());
    app.dispatch(AppInput::Device(fresh_globals(globals)));
    app.finish_firmware_editor_normal_refresh(&lease).unwrap();
    assert!(!app.firmware_editor_blocks_normal_actions());
    assert!(app.recovery_session().documents.iter().any(|document| {
        original
            .documents
            .iter()
            .any(|old| old.draft == document.draft && old.undo == document.undo)
    }));
}

#[test]
fn recovery_barrier_keeps_dirty_device_draft_and_uncertain_work_blockers() {
    let mut app = connected();
    keep_local_notes(&mut app);
    let lease = app.begin_firmware_recovery_preparation().unwrap();
    let exact = app.recovery_session();
    assert!(blockers(&app, &lease).contains(&FirmwareEditorBlocker::DirtyDeviceDraft));
    assert!(
        app.seal_firmware_recovery_preparation(&lease, &exact)
            .is_err()
    );
    let mut wrong = exact.clone();
    wrong.documents[0].draft.metadata.notes = Some("Not the retained local work".into());
    assert!(
        app.retain_firmware_local_draft_excluded(&lease, &wrong)
            .is_err()
    );
    app.retain_firmware_local_draft_excluded(&lease, &exact)
        .unwrap();
    app.seal_firmware_recovery_preparation(&lease, &exact)
        .unwrap();
    assert_eq!(app.recovery_session(), exact);
}
