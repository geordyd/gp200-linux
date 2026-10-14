// Private capture acceptance suite; excluded from public source builds.
// Run with RUSTFLAGS="--cfg gp200_private_evidence" only in the retained lab checkout.
#![cfg(gp200_private_evidence)]

//! Synthetic factory candidate continuation ordering.
#![cfg(feature = "factory-amp-trials")]
use gp200_app::*;
use gp200_catalog::Catalog;
use gp200_domain::*;
use serde_json::Value;
use std::sync::Arc;
const VECTORS: &str = include_str!(
    "../../../analysis/implementation/factory-amp-expansion-execution/synthetic-vectors.json"
);
fn cases() -> Vec<Value> {
    serde_json::from_str::<Value>(VECTORS).unwrap()["cases"]
        .as_array()
        .unwrap()
        .clone()
}
fn raw(hex: &str) -> RawTransfer {
    RawTransfer::from_payload(
        0x12,
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect(),
    )
    .unwrap()
}
fn snapshot(value: RawTransfer, revision: u64) -> Arc<PresetSnapshot> {
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(value),
        SnapshotSource::Hardware {
            epoch: SessionEpoch(1),
        },
        Some(PresetTarget::EditBuffer),
        Revision(revision),
    ))
}
fn global(value: RawTransfer, revision: u64) -> Arc<DeviceGlobalState> {
    Arc::new(
        DeviceGlobalState::from_transfer(Revision(revision), SessionEpoch(1), value, vec![])
            .unwrap(),
    )
}
fn readback(
    app: &mut AppController,
    stamp: RequestStamp,
    value: RawTransfer,
    revision: u64,
) -> Vec<Effect> {
    app.dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
        stamp,
        target: PresetTarget::EditBuffer,
        snapshot: snapshot(value, revision),
    }))
}
fn globals(
    app: &mut AppController,
    stamp: RequestStamp,
    value: RawTransfer,
    revision: u64,
) -> Vec<Effect> {
    app.dispatch(AppInput::Device(DeviceEvent::GlobalStateReceived {
        stamp,
        state: global(value, revision),
    }))
}
#[cfg(feature = "factory-amp-trials")]
fn stage(app: &mut AppController, stamp: RequestStamp, stage: EditStage) -> Vec<Effect> {
    app.dispatch(AppInput::Device(DeviceEvent::EditProgress { stamp, stage }))
}
fn connected(case: &Value, with_globals: bool) -> AppController {
    let mut app = AppController::new(Catalog::embedded().unwrap(), AppPreferences::default());
    let effects = app.dispatch(AppInput::Ui(UiIntent::Connect(DeviceSelection {
        input_id: "fixture-in".into(),
        output_id: "fixture-out".into(),
        fingerprint_hint: None,
    })));
    let Effect::Device(DeviceCommand::Connect { intent, .. }) = effects[0] else {
        panic!("{effects:?}")
    };
    app.dispatch(AppInput::Device(DeviceEvent::SessionChanged {
        epoch: SessionEpoch(1),
        identity: DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Hardware,
            firmware: None,
            display_name: "Synthetic factory candidate replay".into(),
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
        raw(case["before"].as_str().unwrap()),
        1,
    );
    if with_globals {
        let effects = app.dispatch(AppInput::Ui(UiIntent::RefreshDeviceGlobals));
        let Effect::Device(DeviceCommand::ReadGlobals(stamp)) = effects[0] else {
            panic!("{effects:?}")
        };
        globals(&mut app, stamp, raw(case["globals"].as_str().unwrap()), 1);
    }
    app
}
fn apply(app: &mut AppController, model: u32) -> Vec<Effect> {
    app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
        document: device_document_id(),
        patch: EditPatch::SetModel {
            module: ModuleId::new(3).unwrap(),
            model: ModelCode(model),
        },
    }))
}
#[cfg(feature = "factory-amp-trials")]
fn command(effects: &[Effect], case: &Value, target: u32) -> RequestStamp {
    let [
        Effect::Device(DeviceCommand::SubmitFactoryAmpModel {
            stamp,
            model,
            profile,
            before_globals,
            expected_auto_cab,
            ..
        }),
    ] = effects
    else {
        panic!("{effects:?}")
    };
    assert_eq!(model.0, target);
    assert_eq!(profile.name(), case["name"].as_str().unwrap());
    assert_eq!(before_globals.epoch, stamp.epoch);
    assert_eq!(*expected_auto_cab, case["auto_cab"].as_bool().unwrap());
    *stamp
}
#[cfg(feature = "factory-amp-trials")]
fn finish(
    app: &mut AppController,
    stamp: RequestStamp,
    value: RawTransfer,
    case: &Value,
    revision: u64,
) -> Vec<Effect> {
    stage(app, stamp, EditStage::Sent);
    stage(app, stamp, EditStage::AwaitingReadback);
    assert!(readback(app, stamp, value, revision).is_empty());
    assert!(globals(app, stamp, raw(case["globals"].as_str().unwrap()), revision).is_empty());
    stage(app, stamp, EditStage::Verified)
}

fn hint(app: &mut AppController, command: u16) {
    assert!(
        app.dispatch(AppInput::Device(DeviceEvent::IncomingNotification {
            epoch: SessionEpoch(1),
            raw: Arc::new(
                RawTransfer::from_records(
                    0x12,
                    &[RawRecord {
                        command,
                        body: vec![0; 4]
                    }]
                )
                .unwrap()
            )
        }))
        .is_empty()
    );
}
fn waiting() -> (AppController, Value, RequestStamp) {
    let case = cases().remove(1);
    let mut app = connected(&case, true);
    let effects = apply(&mut app, 0x07000003);
    let forward = command(&effects, &case, 0x07000003);
    assert!(
        finish(
            &mut app,
            forward,
            raw(case["forward"].as_str().unwrap()),
            &case,
            2
        )
        .is_empty()
    );
    let effects = app.dispatch(AppInput::Ui(UiIntent::Undo(device_document_id())));
    let reverse = command(&effects, &case, FACTORY_AMP_BASELINE.0);
    stage(&mut app, reverse, EditStage::Sent);
    stage(&mut app, reverse, EditStage::AwaitingReadback);
    hint(&mut app, 0x1008);
    assert!(readback(&mut app, reverse, raw(case["reverse"].as_str().unwrap()), 3).is_empty());
    assert!(globals(&mut app, reverse, raw(case["globals"].as_str().unwrap()), 3).is_empty());
    assert!(stage(&mut app, reverse, EditStage::Verified).is_empty());
    let effects = app.dispatch(AppInput::Tick(100));
    let [Effect::Device(DeviceCommand::RefreshTarget { stamp, .. })] = effects.as_slice() else {
        panic!("{effects:?}")
    };
    (app, case, *stamp)
}
fn finish_recovery(
    app: &mut AppController,
    case: &Value,
    read: RequestStamp,
    changed_runtime: Option<usize>,
) -> Vec<Effect> {
    assert!(readback(app, read, raw(case["reverse"].as_str().unwrap()), 4).is_empty());
    assert!(app.external_reconciliation_pending());
    let effects = app.dispatch(AppInput::Tick(600));
    let [Effect::Device(DeviceCommand::ReadGlobals(stamp))] = effects.as_slice() else {
        panic!("{effects:?}")
    };
    let mut records = raw(case["globals"].as_str().unwrap()).records().to_vec();
    if let Some(offset) = changed_runtime {
        records[3].body[offset] ^= 1;
    }
    globals(
        app,
        *stamp,
        RawTransfer::from_records(0x12, &records).unwrap(),
        4,
    )
}
#[test]
fn unchanged_complete_recovery_releases_one_never_submitted_declared_restore() {
    let (mut app, case, read) = waiting();
    let effects = finish_recovery(&mut app, &case, read, None);
    let [
        Effect::Device(DeviceCommand::SubmitEdit {
            stamp,
            patch:
                EditPatch::SetParameter {
                    module,
                    parameter,
                    value,
                    ..
                },
            ..
        }),
    ] = effects.as_slice()
    else {
        panic!("{effects:?}")
    };
    let expected = &case["restores"][0];
    assert_eq!(module.get(), expected["module"].as_u64().unwrap() as u8);
    assert_eq!(
        parameter.get(),
        expected["parameter"].as_u64().unwrap() as u8
    );
    assert_eq!(value.0, expected["raw_bits"].as_u64().unwrap() as u32);
    let owned = *stamp;
    stage(&mut app, owned, EditStage::Sent);
    assert!(stage(&mut app, owned, EditStage::Verified).is_empty());
    assert!(app.device_operation_pending());
    assert!(app.dispatch(AppInput::Tick(20000)).is_empty());
}
#[test]
fn each_strict_runtime_change_retires_continuation_without_replay() {
    for offset in [0, 2, 4] {
        let (mut app, case, read) = waiting();
        assert!(finish_recovery(&mut app, &case, read, Some(offset)).is_empty());
        assert!(app.dispatch(AppInput::Tick(20000)).is_empty());
        assert_eq!(app.active_document().unwrap().undo.future.len(), 1);
    }
}
#[test]
fn changed_complete_current_or_stale_unowned_read_never_releases_restores() {
    let (mut app, case, read) = waiting();
    let unowned = RequestStamp {
        intent: IntentId(read.intent.0 + 300),
        ..read
    };
    assert!(readback(&mut app, unowned, raw(case["reverse"].as_str().unwrap()), 4).is_empty());
    assert!(app.external_reconciliation_pending());
    assert!(readback(&mut app, read, raw(case["reverse"].as_str().unwrap()), 3).is_empty());
    assert!(app.external_reconciliation_pending());
    let mut records = raw(case["reverse"].as_str().unwrap()).records().to_vec();
    records[6].body[8] ^= 1;
    assert!(
        readback(
            &mut app,
            read,
            RawTransfer::from_records(0x12, &records).unwrap(),
            4
        )
        .is_empty()
    );
    let effects = app.dispatch(AppInput::Tick(600));
    if let [Effect::Device(DeviceCommand::ReadGlobals(stamp))] = effects.as_slice() {
        assert!(globals(&mut app, *stamp, raw(case["globals"].as_str().unwrap()), 4).is_empty());
    }
    assert!(app.dispatch(AppInput::Tick(20000)).is_empty());
}
#[test]
fn continuous_hints_session_loss_and_uncertain_restore_never_replay_owned_steps() {
    let (mut app, case, read) = waiting();
    hint(&mut app, 0x1008);
    assert!(readback(&mut app, read, raw(case["reverse"].as_str().unwrap()), 4).is_empty());
    hint(&mut app, 0x1008);
    let effects = app.dispatch(AppInput::Tick(600));
    assert!(effects.iter().all(|effect| matches!(
        effect,
        Effect::Device(DeviceCommand::RefreshTarget { .. } | DeviceCommand::ReadGlobals(_))
    )));
    app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
        epoch: SessionEpoch(1),
        state: ConnectionState::Disconnected,
    }));
    assert!(readback(&mut app, read, raw(case["reverse"].as_str().unwrap()), 9).is_empty());
    assert!(app.dispatch(AppInput::Tick(20000)).is_empty());
    let (mut app, case, read) = waiting();
    let effects = finish_recovery(&mut app, &case, read, None);
    let [Effect::Device(DeviceCommand::SubmitEdit { stamp, .. })] = effects.as_slice() else {
        panic!("{effects:?}")
    };
    let owned = *stamp;
    stage(&mut app, owned, EditStage::Sent);
    assert!(
        stage(
            &mut app,
            owned,
            EditStage::Indeterminate(ServiceError::new(
                ErrorKind::Indeterminate,
                "synthetic uncertain restore"
            ))
        )
        .is_empty()
    );
    assert!(stage(&mut app, owned, EditStage::Verified).is_empty());
    assert!(app.dispatch(AppInput::Tick(20000)).is_empty());
}
#[test]
fn unrelated_local_gesture_expires_the_bound_device_undo_continuation() {
    let (mut app, case, read) = waiting();
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
            document: device_document_id(),
            patch: EditPatch::SetEnabled {
                module: ModuleId::new(0).unwrap(),
                enabled: true
            }
        }))
        .is_empty()
    );
    assert!(finish_recovery(&mut app, &case, read, None).is_empty());
    assert!(app.dispatch(AppInput::Tick(20000)).is_empty());
}
