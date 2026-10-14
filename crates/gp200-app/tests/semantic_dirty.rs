// Private capture acceptance suite; excluded from public source builds.
// Run with RUSTFLAGS="--cfg gp200_private_evidence" only in the retained lab checkout.
#![cfg(gp200_private_evidence)]

use gp200_app::*;
use gp200_catalog::Catalog;
use gp200_domain::*;
use std::sync::Arc;

const CONNECT: &[u8] = include_bytes!(
    "../../../analysis/implementation/inactive-restore-recovery/probe/cleanup-fresh-session-24-preset.payload.bin"
);
const REFRESH: &[u8] = include_bytes!(
    "../../../analysis/implementation/inactive-restore-recovery/probe/cleanup-fresh-session-28-preset.payload.bin"
);

fn snapshot(bytes: &[u8], revision: u64) -> Arc<PresetSnapshot> {
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_payload(0x12, bytes.to_vec()).unwrap()),
        SnapshotSource::Hardware {
            epoch: SessionEpoch(1),
        },
        Some(PresetTarget::EditBuffer),
        Revision(revision),
    ))
}

fn connected() -> AppController {
    let mut app = AppController::new(Catalog::embedded().unwrap(), AppPreferences::default());
    let effects = app.dispatch(AppInput::Ui(UiIntent::Connect(DeviceSelection {
        input_id: "offline-recorded-in".into(),
        output_id: "offline-recorded-out".into(),
        fingerprint_hint: None,
    })));
    let [Effect::Device(DeviceCommand::Connect { intent, .. })] = effects.as_slice() else {
        panic!("Expected an explicit connection effect");
    };
    let intent = *intent;
    app.dispatch(AppInput::Device(DeviceEvent::SessionChanged {
        epoch: SessionEpoch(1),
        identity: DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Simulator,
            firmware: None,
            display_name: "Actual payloads, offline reducer only".into(),
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
        snapshot: snapshot(CONNECT, 1),
    }));
    app
}

fn refresh(app: &mut AppController, incoming: Arc<PresetSnapshot>) -> Vec<Effect> {
    let effects = app.dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)));
    let [Effect::Device(DeviceCommand::RefreshTarget { stamp, target })] = effects.as_slice()
    else {
        panic!("Expected read-only Refresh effect: {effects:?}");
    };
    app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
        stamp: *stamp,
        target: *target,
        snapshot: incoming,
    }))
}

#[test]
fn actual_refresh_stays_clean_exports_exact_baseline_and_permits_explicit_selection() {
    let mut app = connected();
    let baseline = app.state().documents[&device_document_id()]
        .baseline
        .clone();
    assert!(refresh(&mut app, snapshot(REFRESH, 2)).is_empty());
    let doc = &app.state().documents[&device_document_id()];
    assert!(!doc.dirty());
    assert!(doc.pending.is_empty());
    assert!(doc.conflicts.is_empty());
    assert!(doc.undo.past.is_empty());
    assert_eq!(doc.baseline, baseline);
    assert_eq!(
        app.state().confirmed_buffer.as_ref().unwrap().raw.payload(),
        REFRESH
    );
    let draft_bytes = serde_json::to_vec(&doc.draft).unwrap();
    let exported = app.document_snapshot(&device_document_id()).unwrap();
    assert_eq!(exported.raw.payload(), CONNECT);
    assert_eq!(
        serde_json::to_vec(&app.state().documents[&device_document_id()].draft).unwrap(),
        draft_bytes
    );
    assert_eq!(app.state().buffer_persistence, BufferPersistence::Unknown);
    assert!(matches!(
        app.dispatch(AppInput::Ui(UiIntent::AuditionStored(
            SlotId::new(23).unwrap()
        )))
        .as_slice(),
        [Effect::Device(DeviceCommand::SelectStored { .. })]
    ));
}

#[test]
fn real_local_draft_is_preserved_across_refresh_and_still_blocks_selection() {
    let mut app = connected();
    let original = app.state().documents[&device_document_id()].draft.clone();
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
            document: device_document_id(),
            patch: EditPatch::SetMetadata(MetadataPatch {
                notes: Some("Keep this unsaved take".into()),
                ..Default::default()
            }),
        }))
        .is_empty()
    );
    let history = app.state().documents[&device_document_id()]
        .undo
        .past
        .clone();
    assert_eq!(history.as_slice(), std::slice::from_ref(&original));
    assert!(refresh(&mut app, snapshot(REFRESH, 2)).is_empty());
    let doc = &app.state().documents[&device_document_id()];
    assert!(doc.dirty());
    assert_eq!(
        doc.draft.metadata.notes.as_deref(),
        Some("Keep this unsaved take")
    );
    assert_eq!(doc.undo.past, history);
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::AuditionStored(
            SlotId::new(23).unwrap()
        )))
        .is_empty()
    );
    assert!(
        app.state()
            .messages
            .back()
            .unwrap()
            .message
            .contains("Save or explicitly discard")
    );
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::Undo(device_document_id())))
            .is_empty()
    );
    assert_eq!(app.state().documents[&device_document_id()].draft, original);
    assert!(!app.state().documents[&device_document_id()].dirty());
}

#[test]
fn clean_external_hidden_bit_change_adopts_exact_state_without_claiming_save() {
    let mut app = connected();
    let mut records = snapshot(REFRESH, 2).raw.records().to_vec();
    // Slot14 is outside the factory PRE module's displayed controls. Its exact
    // retained bits still matter, independently of UI control declarations.
    let module = records
        .iter_mut()
        .find(|record| record.command == 20 && record.body[0] == 0)
        .unwrap();
    module.body[8 + 14 * 4] ^= 1;
    let changed = snapshot(
        RawTransfer::from_records(0x12, &records).unwrap().payload(),
        2,
    );
    assert!(refresh(&mut app, changed.clone()).is_empty());
    let doc = &app.state().documents[&device_document_id()];
    assert!(!doc.dirty());
    assert!(Arc::ptr_eq(&doc.baseline, &changed));
    assert_eq!(doc.baseline.raw.payload(), changed.raw.payload());
    assert!(doc.undo.past.is_empty());
    assert_eq!(
        doc.draft.view.modules[0].parameters[14],
        changed.view.modules[0].parameters[14]
    );
    assert_eq!(app.state().buffer_persistence, BufferPersistence::Unknown);
    let slot = SlotId::new(23).unwrap();
    assert!(
        matches!(app.dispatch(AppInput::Ui(UiIntent::AuditionStored(slot))).as_slice(),
        [Effect::Device(DeviceCommand::SelectStored { slot: actual, .. })] if *actual == slot)
    );
}

#[test]
fn a_verified_save_remains_saved_after_padding_only_refresh_and_keeps_history() {
    let mut app = connected();
    let slot = SlotId::new(255).unwrap();
    let effects = app.dispatch(AppInput::Ui(UiIntent::SaveBufferTo {
        slot,
        expected_baseline: Revision(1),
    }));
    let [Effect::Device(DeviceCommand::SaveBufferTo { stamp, .. })] = effects.as_slice() else {
        panic!("Expected explicit Save effect: {effects:?}");
    };
    let save = *stamp;
    app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
        stamp: save,
        stage: EditStage::Verified,
    }));
    assert!(matches!(
        app.state().buffer_persistence,
        BufferPersistence::Saving { .. }
    ));
    let mut stored = (*snapshot(CONNECT, 2)).clone();
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
    app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
        stamp: save,
        target: PresetTarget::EditBuffer,
        snapshot: snapshot(CONNECT, 2),
    }));
    assert_eq!(
        app.state().buffer_persistence,
        BufferPersistence::Saved {
            slot,
            revision: Revision(2)
        }
    );
    let baseline = app.state().documents[&device_document_id()]
        .baseline
        .clone();
    assert!(refresh(&mut app, snapshot(REFRESH, 3)).is_empty());
    let doc = &app.state().documents[&device_document_id()];
    assert!(!doc.dirty());
    assert_eq!(doc.baseline, baseline);
    assert!(doc.undo.past.is_empty());
    assert_eq!(
        app.state().buffer_persistence,
        BufferPersistence::Saved {
            slot,
            revision: Revision(2)
        }
    );
}

#[test]
fn uncertainty_refresh_remains_read_only_and_does_not_replay_an_edit() {
    let mut app = connected();
    let effects = app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: device_document_id(),
        patch: EditPatch::SetMetadata(MetadataPatch {
            level: Some(51),
            ..Default::default()
        }),
    }));
    let [Effect::Device(DeviceCommand::SubmitEdit { stamp, .. })] = effects.as_slice() else {
        panic!("Expected explicit edit effect: {effects:?}");
    };
    app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
        stamp: *stamp,
        stage: EditStage::Indeterminate(ServiceError::new(
            ErrorKind::Timeout,
            "Readback unavailable",
        )),
    }));
    assert!(refresh(&mut app, snapshot(REFRESH, 2)).is_empty());
    assert!(app.state().documents[&device_document_id()].dirty());
    assert_eq!(
        app.state().documents[&device_document_id()]
            .draft
            .view
            .level
            .value(),
        Some(&51)
    );
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::AuditionStored(
            SlotId::new(23).unwrap()
        )))
        .is_empty()
    );
}
