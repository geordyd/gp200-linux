// Private capture acceptance suite; excluded from public source builds.
// Run with RUSTFLAGS="--cfg gp200_private_evidence" only in the retained lab checkout.
#![cfg(gp200_private_evidence)]

use gp200_app::*;
use gp200_catalog::Catalog;
use gp200_domain::*;
use std::sync::Arc;

fn capture(revision: u64) -> Arc<PresetSnapshot> {
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
        Revision(revision),
    ))
}

fn connected() -> AppController {
    connected_fixture(capture(0), BackendMode::Simulator)
}

fn connected_fixture(initial: Arc<PresetSnapshot>, mode: BackendMode) -> AppController {
    let mut app = AppController::new(Catalog::embedded().unwrap(), AppPreferences::default());
    let effects = app.dispatch(AppInput::Ui(UiIntent::Connect(DeviceSelection {
        input_id: "synthetic-in".into(),
        output_id: "synthetic-out".into(),
        fingerprint_hint: None,
    })));
    let Effect::Device(DeviceCommand::Connect { intent, .. }) = effects[0] else {
        panic!("connect")
    };
    app.dispatch(AppInput::Device(DeviceEvent::SessionChanged {
        epoch: SessionEpoch(1),
        identity: DeviceIdentity {
            model: DeviceModel::Gp200,
            mode,
            firmware: None,
            display_name: "Synthetic reconciliation fixture".into(),
        },
        capabilities: if mode == BackendMode::Hardware {
            gp200_transport::verified_capabilities()
        } else {
            Capabilities::simulator()
        },
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
        initial,
    );
    app
}

fn notify(app: &mut AppController, command: u16, body: Vec<u8>) -> Vec<Effect> {
    app.dispatch(AppInput::Device(DeviceEvent::IncomingNotification {
        epoch: SessionEpoch(1),
        raw: Arc::new(RawTransfer::from_records(0x12, &[RawRecord { command, body }]).unwrap()),
    }))
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

fn actual_gain(base: &PresetSnapshot, value: f32, revision: u64) -> Arc<PresetSnapshot> {
    let mut records = base.raw.records().to_vec();
    let amp = records
        .iter_mut()
        .find(|record| record.command == 0x14 && record.body[0] == 3)
        .unwrap();
    amp.body[8..12].copy_from_slice(&value.to_le_bytes());
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(revision),
    ))
}

fn readback(app: &mut AppController, stamp: RequestStamp, snapshot: Arc<PresetSnapshot>) {
    app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
        stamp,
        target: PresetTarget::EditBuffer,
        snapshot,
    }));
}

fn stamp(effects: &[Effect]) -> RequestStamp {
    match effects.first().expect("one device command") {
        Effect::Device(DeviceCommand::SubmitEdit { stamp, .. })
        | Effect::Device(DeviceCommand::RefreshTarget { stamp, .. })
        | Effect::Device(DeviceCommand::ReadCurrent(stamp))
        | Effect::Device(DeviceCommand::ReadGlobals(stamp)) => *stamp,
        _ => panic!("unexpected command"),
    }
}

fn annotation(app: &mut AppController) {
    let effects = app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: device_document_id(),
        patch: EditPatch::SetMetadata(MetadataPatch {
            author: Some("Retained local author".into()),
            ..Default::default()
        }),
    }));
    assert!(effects.is_empty());
}

fn current_read(effects: &[Effect]) -> bool {
    matches!(
        effects,
        [Effect::Device(
            DeviceCommand::ReadCurrent(_)
                | DeviceCommand::RefreshTarget {
                    target: PresetTarget::EditBuffer,
                    ..
                }
        )]
    )
}

#[test]
fn a_busy_notification_survives_owned_edit_completion_without_echo() {
    let mut app = connected();
    let effects = app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: device_document_id(),
        patch: gain(51.0),
    }));
    let edit = stamp(&effects);
    assert!(notify(&mut app, 5, vec![3, 0, 0, 0, 13, 0, 0, 0, 0, 0, 0, 0]).is_empty());
    readback(&mut app, edit, actual_gain(&capture(0), 51.0, 1));
    assert!(
        app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
            stamp: edit,
            stage: EditStage::Verified,
        }))
        .is_empty()
    );
    assert!(current_read(&app.dispatch(AppInput::Tick(1000))));
}

#[test]
fn a_global_hint_requests_complete_globals_instead_of_current() {
    let mut app = connected();
    assert!(notify(&mut app, 0x1010, vec![0; 4]).is_empty());
    assert!(matches!(
        app.dispatch(AppInput::Tick(100)),
        effects if matches!(&effects[..], [Effect::Device(DeviceCommand::ReadGlobals(_))])
    ));
}

#[test]
fn unknown_and_malformed_hints_are_raw_diagnostics_without_read_storms() {
    let mut app = connected();
    for (command, body) in [(0x4242, vec![1, 2, 3]), (5, vec![3, 0])] {
        assert!(notify(&mut app, command, body).is_empty());
    }
    assert!(app.dispatch(AppInput::Tick(1000)).is_empty());
    assert_eq!(app.state().diagnostics.len(), 2);
}

#[test]
fn a_thousand_hints_coalesce_before_any_complete_read() {
    let mut app = connected();
    for _ in 0..1000 {
        assert!(notify(&mut app, 0x15, vec![3, 1, 0, 0]).is_empty());
    }
    assert!(app.dispatch(AppInput::Tick(99)).is_empty());
    assert!(current_read(&app.dispatch(AppInput::Tick(100))));
    assert!(app.dispatch(AppInput::Tick(1000)).is_empty());
    assert_eq!(app.state().diagnostics.len(), 256);
}

#[test]
fn a_dirty_document_is_preserved_whole_when_another_pedal_field_changes() {
    let mut app = connected();
    annotation(&mut app);
    let before = app.state().documents[&device_document_id()].clone();
    let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
    let actual = actual_gain(&capture(0), 73.0, 1);
    readback(&mut app, stamp(&effects), actual.clone());
    let document = &app.state().documents[&device_document_id()];
    assert_eq!(document.draft, before.draft);
    assert!(Arc::ptr_eq(&document.baseline, &before.baseline));
    assert_eq!(document.undo.past, before.undo.past);
    assert_eq!(document.conflicts.len(), 1);
    assert!(Arc::ptr_eq(&document.conflicts[0].actual, &actual));
}

#[test]
fn an_external_context_change_detaches_work_and_installs_exact_current() {
    let mut app = connected();
    annotation(&mut app);
    let before = app.state().documents[&device_document_id()].clone();
    let mut records = capture(0).raw.records().to_vec();
    for record in &mut records {
        if record.command == 0 {
            record.body[2..4].copy_from_slice(&24u16.to_le_bytes());
        } else if matches!(record.command, 2 | 8) {
            record.body[..2].copy_from_slice(&24u16.to_le_bytes());
        }
    }
    let actual = Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(1),
    ));
    let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
    readback(&mut app, stamp(&effects), actual.clone());
    let retained = app
        .state()
        .documents
        .values()
        .find(|document| matches!(document.target, EditTarget::Offline(_)))
        .expect("prior work retained Offline");
    assert_eq!(retained.draft, before.draft);
    assert_eq!(retained.undo.past, before.undo.past);
    assert!(Arc::ptr_eq(&retained.baseline, &before.baseline));
    assert_eq!(app.state().active_document, Some(retained.id.clone()));
    assert!(Arc::ptr_eq(
        &app.state().documents[&device_document_id()].baseline,
        &actual
    ));
}

#[test]
fn a_clean_external_change_adopts_the_exact_actual_baseline() {
    let mut app = connected();
    let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
    let actual = actual_gain(&capture(0), 73.0, 1);
    readback(&mut app, stamp(&effects), actual.clone());
    let document = &app.state().documents[&device_document_id()];
    assert!(Arc::ptr_eq(&document.baseline, &actual));
    assert!(!document.dirty());
    assert!(document.undo.past.is_empty());
}

#[test]
fn exact_length_hints_with_unsupported_selectors_do_not_schedule_reads() {
    for (command, mut body, selector) in [
        (2, vec![0; 88], 256u16),
        (8, vec![0; 16], 256),
        (7, vec![0; 20], 256),
        (0x2003, vec![0; 20], 256),
        (0x2009, vec![0; 4], 256),
        (0x000a, vec![0; 20], 20),
    ] {
        body[..2].copy_from_slice(&selector.to_le_bytes());
        let mut app = connected();
        assert!(notify(&mut app, command, body).is_empty());
        assert!(app.dispatch(AppInput::Tick(1000)).is_empty());
    }
    for body in [
        vec![11, 0, 0, 0, 13, 0, 0, 0, 0, 0, 0, 0],
        vec![3, 15, 0, 0, 13, 0, 0, 0, 0, 0, 0, 0],
    ] {
        let mut app = connected();
        assert!(notify(&mut app, 5, body).is_empty());
        assert!(app.dispatch(AppInput::Tick(1000)).is_empty());
    }
}

#[test]
fn old_epoch_disconnected_and_special_kind_hints_grant_no_read_or_write() {
    let raw = Arc::new(
        RawTransfer::from_records(
            3,
            &[RawRecord {
                command: 0x15,
                body: vec![3, 1, 0, 0],
            }],
        )
        .unwrap(),
    );
    let mut app = connected();
    assert!(
        app.dispatch(AppInput::Device(DeviceEvent::IncomingNotification {
            epoch: SessionEpoch(1),
            raw
        }))
        .is_empty()
    );
    assert!(app.dispatch(AppInput::Tick(1000)).is_empty());
    let raw = Arc::new(
        RawTransfer::from_records(
            0x12,
            &[RawRecord {
                command: 0x15,
                body: vec![3, 1, 0, 0],
            }],
        )
        .unwrap(),
    );
    assert!(
        app.dispatch(AppInput::Device(DeviceEvent::IncomingNotification {
            epoch: SessionEpoch(0),
            raw: raw.clone()
        }))
        .is_empty()
    );
    app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
        epoch: SessionEpoch(2),
        state: ConnectionState::Disconnected,
    }));
    assert!(
        app.dispatch(AppInput::Device(DeviceEvent::IncomingNotification {
            epoch: SessionEpoch(2),
            raw
        }))
        .is_empty()
    );
    assert!(app.dispatch(AppInput::Tick(2000)).is_empty());
}

#[test]
fn an_unchanged_read_preserves_baseline_arc_annotations_history_and_revision() {
    let mut app = connected();
    annotation(&mut app);
    let before = app.state().documents[&device_document_id()].clone();
    let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
    readback(&mut app, stamp(&effects), capture(1));
    let document = &app.state().documents[&device_document_id()];
    assert!(Arc::ptr_eq(&document.baseline, &before.baseline));
    assert_eq!(document.draft, before.draft);
    assert_eq!(document.undo.past, before.undo.past);
    assert_eq!(document.local_revision, before.local_revision);
    assert!(document.conflicts.is_empty());
}

fn conflicted() -> (AppController, Arc<PresetSnapshot>, IntentId) {
    let mut app = connected();
    annotation(&mut app);
    let actual = actual_gain(&capture(0), 73.0, 1);
    let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
    let intent = stamp(&effects).intent;
    readback(&mut app, stamp(&effects), actual.clone());
    assert_eq!(
        app.state().documents[&device_document_id()].conflicts.len(),
        1
    );
    (app, actual, intent)
}

#[test]
fn accept_and_keep_are_local_preservation_actions_with_no_physical_undo() {
    for accept in [true, false] {
        let (mut app, actual, intent) = conflicted();
        let before = app.state().documents[&device_document_id()].clone();
        let resolution = if accept {
            ConflictResolution::AcceptDevice(intent)
        } else {
            ConflictResolution::KeepLocal(intent)
        };
        assert!(
            app.dispatch(AppInput::Ui(UiIntent::ResolveConflict(resolution)))
                .is_empty()
        );
        let retained = app
            .state()
            .documents
            .values()
            .find(|document| matches!(document.target, EditTarget::Offline(_)))
            .unwrap();
        let retained_id = retained.id.clone();
        assert_eq!(retained.draft, before.draft);
        assert_eq!(retained.undo.past, before.undo.past);
        assert!(Arc::ptr_eq(&retained.baseline, &before.baseline));
        assert!(Arc::ptr_eq(
            &app.state().documents[&device_document_id()].baseline,
            &actual
        ));
        assert_eq!(
            app.state().active_document,
            Some(if accept {
                device_document_id()
            } else {
                retained_id.clone()
            })
        );
        assert!(
            app.dispatch(AppInput::Ui(UiIntent::Undo(device_document_id())))
                .is_empty()
        );
        assert!(
            app.dispatch(AppInput::Ui(UiIntent::Undo(retained_id)))
                .is_empty()
        );
    }
}

#[test]
fn a_second_unchanged_read_renews_the_exact_external_review() {
    let (mut app, actual, old_intent) = conflicted();
    let before = app.state().documents[&device_document_id()].clone();
    let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
    let newer = Arc::new(PresetSnapshot {
        revision: Revision(2),
        ..(*actual).clone()
    });
    readback(&mut app, stamp(&effects), newer.clone());
    let document = &app.state().documents[&device_document_id()];
    assert_eq!(document.draft, before.draft);
    assert_eq!(document.undo.past, before.undo.past);
    assert!(Arc::ptr_eq(&document.baseline, &before.baseline));
    assert_eq!(document.local_revision, before.local_revision);
    assert_eq!(document.conflicts.len(), 1);
    assert_eq!(document.conflicts[0].intent, stamp(&effects).intent);
    assert!(!app.external_conflict(old_intent));
    assert!(app.external_conflict_ready(stamp(&effects).intent));
    assert!(Arc::ptr_eq(&document.conflicts[0].actual, &newer));
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::ResolveConflict(
            ConflictResolution::KeepLocal(old_intent)
        )))
        .is_empty()
    );
    assert_eq!(app.state().documents.len(), 1);
}

#[test]
fn editing_after_review_expires_it_until_another_complete_read() {
    let (mut app, actual, intent) = conflicted();
    assert!(app.external_conflict_ready(intent));
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
            document: device_document_id(),
            patch: EditPatch::SetMetadata(MetadataPatch {
                notes: Some("Later local note".into()),
                ..Default::default()
            })
        }))
        .is_empty()
    );
    assert!(!app.external_conflict_ready(intent));
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::ResolveConflict(
            ConflictResolution::AcceptDevice(intent)
        )))
        .is_empty()
    );
    assert_eq!(app.state().documents.len(), 1);
    let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
    readback(
        &mut app,
        stamp(&effects),
        Arc::new(PresetSnapshot {
            revision: Revision(2),
            ..(*actual).clone()
        }),
    );
    assert!(app.external_conflict_ready(stamp(&effects).intent));
}

#[test]
fn a_local_edit_during_invalidation_is_retained_but_never_auto_flushed() {
    let mut app = connected();
    assert!(notify(&mut app, 0x15, vec![3, 1, 0, 0]).is_empty());
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
            document: device_document_id(),
            patch: gain(51.0)
        }))
        .is_empty()
    );
    let before = app.state().documents[&device_document_id()].clone();
    let effects = app.dispatch(AppInput::Tick(100));
    assert!(current_read(&effects));
    readback(&mut app, stamp(&effects), capture(1));
    assert!(app.dispatch(AppInput::Tick(1000)).is_empty());
    let document = &app.state().documents[&device_document_id()];
    assert_eq!(document.draft, before.draft);
    assert_eq!(document.undo.past, before.undo.past);
    assert!(document.pending.is_empty());
}

#[test]
fn explicit_unchanged_read_retires_indeterminate_authority_without_inferred_ack() {
    let mut app = connected();
    let effects = app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: device_document_id(),
        patch: gain(51.0),
    }));
    let edit = stamp(&effects);
    app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
        stamp: edit,
        stage: EditStage::Sent,
    }));
    app.dispatch(AppInput::Device(DeviceEvent::OperationFailed {
        stamp: edit,
        error: ServiceError::new(ErrorKind::Indeterminate, "Injected missing completion"),
    }));
    let before = app.state().documents[&device_document_id()].clone();
    let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
    readback(&mut app, stamp(&effects), capture(1));
    assert!(!app.device_operation_pending());
    assert!(app.dispatch(AppInput::Tick(1000)).is_empty());
    let document = &app.state().documents[&device_document_id()];
    assert_eq!(document.draft, before.draft);
    assert_eq!(document.undo.past, before.undo.past);
    assert_eq!(document.local_revision, before.local_revision);
    assert!(Arc::ptr_eq(&document.baseline, &before.baseline));
    assert!(
        matches!(&document.pending[&edit.intent].stage, EditStage::Failed(error) if error.kind == ErrorKind::Indeterminate && error.message.contains("not retried"))
    );
    assert!(
        app.recovery_session().documents[0]
            .uncertainty
            .iter()
            .any(|uncertainty| uncertainty.original_intent == edit.intent.0
                && uncertainty.description.contains("unknown"))
    );
}

#[test]
fn unknown_record_drift_is_a_change_even_when_the_projected_view_is_equal() {
    let (mut app, _, _) = conflicted();
    // Resolve to establish a fresh physical document, then add an opaque record.
    let intent = app.state().documents[&device_document_id()].conflicts[0].intent;
    app.dispatch(AppInput::Ui(UiIntent::ResolveConflict(
        ConflictResolution::AcceptDevice(intent),
    )));
    let mut records = app
        .state()
        .confirmed_buffer
        .as_ref()
        .unwrap()
        .raw
        .records()
        .to_vec();
    records.push(RawRecord {
        command: 0x4242,
        body: vec![0xde, 0xad, 0xbe, 0xef],
    });
    let opaque = Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(2),
    ));
    let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
    readback(&mut app, stamp(&effects), opaque.clone());
    annotation(&mut app);
    let before = app.state().documents[&device_document_id()].clone();
    records.last_mut().unwrap().body[3] ^= 1;
    let changed = Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(3),
    ));
    assert!(
        PresetDraft::from_snapshot(&opaque)
            .semantically_equal(&PresetDraft::from_snapshot(&changed))
    );
    let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
    readback(&mut app, stamp(&effects), changed);
    let document = &app.state().documents[&device_document_id()];
    assert_eq!(document.draft, before.draft);
    assert_eq!(document.conflicts.len(), 1);
    assert!(Arc::ptr_eq(&document.baseline, &before.baseline));
}

const DEP_BEFORE: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/own-fresh-current-before.payload.bin"
);
const DEP_GLOBALS: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/own-fresh-globals-before.payload.bin"
);
const DEP_AFTER: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/after-forward.payload.bin"
);
const DEP_GLOBALS_AFTER: &[u8] = include_bytes!(
    "../../../analysis/implementation/dependent-model-actor-root/amp-auto-cab-on/rust/after-forward-globals.payload.bin"
);
fn payload_snapshot(payload: &[u8], revision: u64) -> Arc<PresetSnapshot> {
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_payload(0x12, payload.to_vec()).unwrap()),
        SnapshotSource::Hardware {
            epoch: SessionEpoch(1),
        },
        Some(PresetTarget::EditBuffer),
        Revision(revision),
    ))
}
fn globals(app: &mut AppController, stamp: RequestStamp, payload: &[u8], revision: u64) {
    assert!(
        app.dispatch(AppInput::Device(DeviceEvent::GlobalStateReceived {
            stamp,
            state: Arc::new(
                DeviceGlobalState::from_transfer(
                    Revision(revision),
                    SessionEpoch(1),
                    RawTransfer::from_payload(0x12, payload.to_vec()).unwrap(),
                    vec![]
                )
                .unwrap()
            )
        }))
        .is_empty()
    );
}

#[test]
fn a_hint_during_dependent_readback_retires_a_queued_cab_followup_without_replay() {
    let mut app = connected_fixture(payload_snapshot(DEP_BEFORE, 1), BackendMode::Hardware);
    let effects = app.dispatch(AppInput::Ui(UiIntent::RefreshDeviceGlobals));
    globals(&mut app, stamp(&effects), DEP_GLOBALS, 1);
    let effects = app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: device_document_id(),
        patch: EditPatch::SetModel {
            module: ModuleId::new(3).unwrap(),
            model: ModelCode(0x07000001),
        },
    }));
    let [Effect::Device(DeviceCommand::SubmitDependentModel { stamp: edit, .. })] = &effects[..]
    else {
        panic!("Expected dependent AMP command: {effects:?}");
    };
    let edit = *edit;
    app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
        stamp: edit,
        stage: EditStage::Sent,
    }));
    // A local CAB change is queued while the AMP request owns its readback.
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
            document: device_document_id(),
            patch: EditPatch::SetModel {
                module: ModuleId::new(5).unwrap(),
                model: ModelCode(0x0a000010)
            }
        }))
        .is_empty()
    );
    let desired_cab = app
        .active_document()
        .unwrap()
        .draft
        .view
        .module(ModuleId::new(5).unwrap())
        .unwrap()
        .clone();
    assert!(notify(&mut app, 0x15, vec![3, 1, 0, 0]).is_empty());
    readback(&mut app, edit, payload_snapshot(DEP_AFTER, 2));
    globals(&mut app, edit, DEP_GLOBALS_AFTER, 2);
    assert!(
        app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
            stamp: edit,
            stage: EditStage::Verified
        }))
        .is_empty()
    );
    let effects = app.dispatch(AppInput::Tick(100));
    assert!(current_read(&effects));
    readback(&mut app, stamp(&effects), payload_snapshot(DEP_AFTER, 3));
    assert!(app.dispatch(AppInput::Tick(1000)).is_empty());
    assert_eq!(
        app.active_document()
            .unwrap()
            .draft
            .view
            .module(ModuleId::new(5).unwrap())
            .unwrap(),
        &desired_cab
    );
    assert!(!app.active_document().unwrap().undo.past.is_empty());
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::EndGesture {
            document: device_document_id(),
            key: GestureKey {
                module: ModuleId::new(5).unwrap(),
                model: ModelCode(0x0a000010),
                parameter: ParamId::new(0).unwrap()
            }
        }))
        .is_empty()
    );
}

#[test]
fn a_global_only_hint_drops_unsent_gesture_deltas_after_complete_globals() {
    let mut app = connected();
    notify(&mut app, 0x1010, vec![0; 4]);
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
            document: device_document_id(),
            patch: gain(51.0)
        }))
        .is_empty()
    );
    let before = app.active_document().unwrap().clone();
    let effects = app.dispatch(AppInput::Tick(100));
    assert!(matches!(
        &effects[..],
        [Effect::Device(DeviceCommand::ReadGlobals(_))]
    ));
    globals(&mut app, stamp(&effects), DEP_GLOBALS, 1);
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::EndGesture {
            document: device_document_id(),
            key: GestureKey {
                module: ModuleId::new(3).unwrap(),
                model: ModelCode(0x07000015),
                parameter: ParamId::new(0).unwrap()
            }
        }))
        .is_empty()
    );
    assert_eq!(app.active_document().unwrap().draft, before.draft);
    assert_eq!(app.active_document().unwrap().undo.past, before.undo.past);
}

#[test]
fn invalidation_blocks_every_device_mutation_and_keeps_explicit_reads_available() {
    let mut app = connected();
    notify(&mut app, 0x15, vec![3, 1, 0, 0]);
    let readonly = [
        OperationKind::Identify,
        OperationKind::ReadEditBuffer,
        OperationKind::ReadStoredPreset,
        OperationKind::ReadGlobals,
        OperationKind::PresetExport,
        OperationKind::IrRead,
        OperationKind::SnapToneRead,
    ];
    for operation in OperationKind::ALL {
        let availability = app.operation_availability(operation);
        if readonly.contains(&operation) {
            assert!(
                !matches!(availability, OperationAvailability::Blocked { ref reason } if reason.contains("changed pedal"))
            );
        } else {
            assert!(
                matches!(availability, OperationAvailability::Blocked { .. }),
                "Mutation must be paused: {operation:?}"
            );
        }
    }
    let slot = SlotId::new(23).unwrap();
    for action in [
        UiIntent::SaveBufferTo {
            slot,
            expected_baseline: Revision(0),
        },
        UiIntent::WriteStoredPreset {
            slot,
            expected_target_revision: Revision(0),
            snapshot: capture(0),
        },
        UiIntent::ClearStoredPreset {
            slot,
            expected_target_revision: Revision(0),
        },
        UiIntent::AuditionStored(slot),
        UiIntent::ApplyGlobalEdit(GlobalPatch {
            section: "general".into(),
            key: "auto-cab".into(),
            value: "1".into(),
        }),
        UiIntent::ApplyGlobalControlEdit(GlobalControlEdit::DrumPlay(false)),
    ] {
        assert!(app.dispatch(AppInput::Ui(action)).is_empty());
    }
    app.dispatch(AppInput::Ui(UiIntent::NewOfflineFrom(capture(0))));
    let local_id = app.active_document_id().unwrap().clone();
    let local_before = app.active_document().unwrap().clone();
    assert!(app.stage_document_in_buffer(&local_id).is_empty());
    assert_eq!(app.active_document().unwrap().draft, local_before.draft);
    assert!(current_read(&app.dispatch(AppInput::Ui(
        UiIntent::Refresh(PresetTarget::EditBuffer)
    ))));
}

fn different_context(revision: u64) -> Arc<PresetSnapshot> {
    let mut records = capture(0).raw.records().to_vec();
    for record in &mut records {
        if record.command == 0 {
            record.body[2..4].copy_from_slice(&24u16.to_le_bytes());
        } else if matches!(record.command, 2 | 8) {
            record.body[..2].copy_from_slice(&24u16.to_le_bytes());
        }
    }
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(revision),
    ))
}

#[test]
fn unexpected_owned_readback_context_is_detached_before_parameter_merge() {
    let mut app = connected();
    let effects = app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: device_document_id(),
        patch: gain(51.0),
    }));
    let edit = stamp(&effects);
    app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
        stamp: edit,
        stage: EditStage::Sent,
    }));
    let before = app.active_document().unwrap().clone();
    let actual = different_context(1);
    readback(&mut app, edit, actual.clone());
    let local_id = app.active_document_id().unwrap().clone();
    assert_ne!(local_id, device_document_id());
    let retained = app.active_document().unwrap();
    assert_eq!(retained.target, EditTarget::Offline(local_id.clone()));
    assert_eq!(retained.draft, before.draft);
    assert_eq!(retained.undo.past, before.undo.past);
    assert!(Arc::ptr_eq(&retained.baseline, &before.baseline));
    assert!(Arc::ptr_eq(
        &app.state().documents[&device_document_id()].baseline,
        &actual
    ));
    assert!(
        app.dispatch(AppInput::Device(DeviceEvent::OperationFailed {
            stamp: edit,
            error: ServiceError::new(
                ErrorKind::Indeterminate,
                "Owned readback found another physical context"
            )
        }))
        .is_empty()
    );
    assert!(matches!(
        &app.state().documents[&local_id].pending[&edit.intent].stage,
        EditStage::Indeterminate(_)
    ));
    assert!(app.dispatch(AppInput::Tick(2000)).is_empty());
    assert!(
        app.state().documents[&device_document_id()]
            .pending
            .is_empty()
    );
}

#[test]
fn physical_context_change_preserves_other_active_local_document_and_comparisons() {
    let mut app = connected();
    annotation(&mut app);
    app.capture_comparison(&device_document_id(), ComparisonSlot::A)
        .unwrap();
    let before = app.state().documents[&device_document_id()].clone();
    app.dispatch(AppInput::Ui(UiIntent::NewOfflineFrom(capture(0))));
    let active = app.active_document_id().unwrap().clone();
    let active_before = app.active_document().unwrap().draft.clone();
    let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
    readback(&mut app, stamp(&effects), different_context(1));
    assert_eq!(app.active_document_id(), Some(&active));
    assert_eq!(app.active_document().unwrap().draft, active_before);
    let retained = app
        .state()
        .documents
        .values()
        .find(|document| document.id != active && matches!(document.target, EditTarget::Offline(_)))
        .unwrap();
    assert_ne!(retained.id, device_document_id());
    assert_eq!(retained.draft, before.draft);
    assert_eq!(
        app.state().comparisons[&retained.id].snapshots[&ComparisonSlot::A],
        before.draft
    );
    assert!(Arc::ptr_eq(&retained.baseline.raw, &before.baseline.raw));
}

#[test]
fn queued_assignment_during_hint_is_local_and_does_not_flush_after_refresh() {
    let mut app = connected();
    let ctrl = &app
        .active_document()
        .unwrap()
        .draft
        .view
        .patch_assignments
        .ctrls[0]
        .record;
    let slot = ctrl.slot;
    let enabled = ctrl.state_raw == 0;
    notify(&mut app, 0x15, vec![3, 1, 0, 0]);
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
            document: device_document_id(),
            patch: EditPatch::SetPatchAssignment(AssignmentEdit::CtrlState { slot, enabled })
        }))
        .is_empty()
    );
    let before = app.active_document().unwrap().clone();
    let effects = app.dispatch(AppInput::Tick(100));
    readback(&mut app, stamp(&effects), capture(1));
    assert_eq!(app.active_document().unwrap().draft, before.draft);
    assert_eq!(app.active_document().unwrap().undo.past, before.undo.past);
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::EndGesture {
            document: device_document_id(),
            key: GestureKey {
                module: ModuleId::new(3).unwrap(),
                model: ModelCode(0x07000015),
                parameter: ParamId::new(0).unwrap()
            }
        }))
        .is_empty()
    );
}

#[test]
fn explicit_offline_transition_and_epoch_loss_stop_pending_refresh_without_replay() {
    for explicit in [true, false] {
        let mut app = connected();
        annotation(&mut app);
        let before = app.active_document().unwrap().clone();
        notify(&mut app, 0x15, vec![3, 1, 0, 0]);
        let effects = app.dispatch(AppInput::Tick(100));
        assert!(current_read(&effects));
        if explicit {
            assert!(matches!(
                app.dispatch(AppInput::Ui(UiIntent::EnterOfflineMode))
                    .as_slice(),
                [Effect::Device(DeviceCommand::Disconnect { .. })]
            ));
        } else {
            assert!(
                app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
                    epoch: SessionEpoch(2),
                    state: ConnectionState::Disconnected
                }))
                .is_empty()
            );
        }
        assert!(!app.external_reconciliation_pending());
        assert!(app.dispatch(AppInput::Tick(1000)).is_empty());
        // The old request cannot install a late current snapshot or send edits.
        readback(&mut app, stamp(&effects), actual_gain(&capture(0), 73.0, 1));
        assert_eq!(app.active_document().unwrap().draft, before.draft);
        assert_eq!(app.active_document().unwrap().undo.past, before.undo.past);
        assert!(app.dispatch(AppInput::Tick(5000)).is_empty());
    }
}

#[test]
fn bursts_during_refresh_coalesce_one_followup_and_read_failure_does_not_retry() {
    let mut app = connected();
    notify(&mut app, 0x15, vec![3, 1, 0, 0]);
    let effects = app.dispatch(AppInput::Tick(100));
    for _ in 0..1000 {
        notify(&mut app, 0x15, vec![3, 1, 0, 0]);
    }
    assert!(app.dispatch(AppInput::Tick(599)).is_empty());
    readback(&mut app, stamp(&effects), capture(1));
    assert!(app.dispatch(AppInput::Tick(599)).is_empty());
    let effects = app.dispatch(AppInput::Tick(600));
    assert!(current_read(&effects));
    app.dispatch(AppInput::Device(DeviceEvent::OperationFailed {
        stamp: stamp(&effects),
        error: ServiceError::new(ErrorKind::Timeout, "Injected read timeout"),
    }));
    assert!(app.dispatch(AppInput::Tick(5000)).is_empty());
    assert!(!app.external_reconciliation_pending());
}

#[test]
fn raw_record_order_length_and_hidden_bytes_are_changes_but_known_volatile_bytes_are_not() {
    for alteration in 0..4 {
        let mut app = connected();
        let mut records = capture(0).raw.records().to_vec();
        records.push(RawRecord {
            command: 0x4242,
            body: vec![1, 2, 3],
        });
        let opaque = Arc::new(PresetSnapshot::from_raw(
            Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
            SnapshotSource::Simulator,
            Some(PresetTarget::EditBuffer),
            Revision(1),
        ));
        let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
        readback(&mut app, stamp(&effects), opaque);
        annotation(&mut app);
        let before = app.active_document().unwrap().clone();
        match alteration {
            0 => records.swap(3, 4),
            1 => records.last_mut().unwrap().body.push(0),
            2 => {
                records
                    .iter_mut()
                    .find(|record| record.command == 0x14)
                    .unwrap()
                    .body[3] ^= 1
            }
            _ => {
                for record in &mut records {
                    match (record.command, record.body.len()) {
                        (2, 88) | (8, 16) => record.body[15] ^= 1,
                        (0x0f, 8) => {
                            record.body[2] ^= 1;
                            record.body[3] ^= 1;
                        }
                        _ => {}
                    }
                }
            }
        }
        let actual = Arc::new(PresetSnapshot::from_raw(
            Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
            SnapshotSource::Simulator,
            Some(PresetTarget::EditBuffer),
            Revision(2),
        ));
        let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
        readback(&mut app, stamp(&effects), actual);
        assert_eq!(app.active_document().unwrap().draft, before.draft);
        assert!(Arc::ptr_eq(
            &app.active_document().unwrap().baseline,
            &before.baseline
        ));
        assert_eq!(
            app.active_document().unwrap().conflicts.len(),
            usize::from(alteration != 3)
        );
    }
}
