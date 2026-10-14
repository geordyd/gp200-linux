use gp200_app::*;
use gp200_catalog::Catalog;
use gp200_domain::*;
use std::sync::Arc;
fn slot(index: u16) -> AssetSlot {
    AssetSlot::new(AssetBank::Ir, index).unwrap()
}
fn app() -> AppController {
    let mut app = AppController::new(Catalog::embedded().unwrap(), AppPreferences::default());
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
    app
}
fn request(app: &mut AppController, index: u16) -> RequestStamp {
    let effects = app.dispatch(AppInput::Ui(UiIntent::RefreshAssetMetadata(slot(index))));
    let [
        Effect::Device(DeviceCommand::ReadAssetMetadata {
            stamp,
            slot: actual,
        }),
    ] = effects.as_slice()
    else {
        panic!("Expected one metadata read")
    };
    assert_eq!(*actual, slot(index));
    *stamp
}
fn metadata(
    stamp: RequestStamp,
    index: u16,
    source: SnapshotSource,
    revision: u64,
) -> Arc<DeviceAssetMetadata> {
    let bank = RawTransfer::from_payload(
        gp200_protocol::WRITE_KIND,
        include_bytes!("../../../tests/fixtures/device-assets/connected-ir-metadata.payload.bin")
            .to_vec(),
    )
    .unwrap();
    Arc::new(
        DeviceAssetMetadata::from_transfer(
            slot(index),
            stamp,
            Revision(revision),
            source,
            55,
            RawTransfer::from_records(
                gp200_protocol::WRITE_KIND,
                &bank.records()[usize::from(index)..usize::from(index) + 1],
            )
            .unwrap(),
        )
        .unwrap(),
    )
}
#[test]
fn one_explicit_read_updates_only_that_slot_with_original_raw_and_simulator_source() {
    let mut app = app();
    let stamp = request(&mut app, 7);
    assert!(app.device_operation_pending());
    let value = metadata(stamp, 7, SnapshotSource::Simulator, 1);
    let effects = app.dispatch(AppInput::Device(DeviceEvent::AssetMetadataReceived {
        stamp,
        metadata: value.clone(),
    }));
    assert!(effects.is_empty());
    assert!(!app.device_operation_pending());
    assert_eq!(app.state().device_assets.complete_count(AssetBank::Ir), 1);
    assert_eq!(
        app.state().device_assets.entry(slot(7)).snapshot.as_ref(),
        Some(&value)
    );
    assert_eq!(
        app.state().device_assets.entry(slot(8)).status,
        AssetMetadataReadStatus::NeverRead
    );
}
#[test]
fn wrong_slot_unowned_stamp_and_hardware_projection_cannot_replace_simulator_cache() {
    let mut app = app();
    let stamp = request(&mut app, 7);
    let unowned = RequestStamp {
        intent: IntentId(stamp.intent.0 + 1),
        ..stamp
    };
    for (actual, value) in [
        (stamp, metadata(stamp, 8, SnapshotSource::Simulator, 1)),
        (unowned, metadata(unowned, 7, SnapshotSource::Simulator, 1)),
        (
            stamp,
            metadata(stamp, 7, SnapshotSource::Hardware { epoch: stamp.epoch }, 1),
        ),
    ] {
        app.dispatch(AppInput::Device(DeviceEvent::AssetMetadataReceived {
            stamp: actual,
            metadata: value,
        }));
        assert_eq!(app.state().device_assets.complete_count(AssetBank::Ir), 0);
        assert!(app.device_operation_pending());
    }
}
#[test]
fn busy_request_cannot_issue_second_read_and_timeout_stops_then_old_reply_is_ignored() {
    let mut app = app();
    let stamp = request(&mut app, 0);
    assert!(
        app.dispatch(AppInput::Ui(UiIntent::RefreshAssetMetadata(slot(1))))
            .is_empty()
    );
    app.dispatch(AppInput::Device(DeviceEvent::OperationFailed {
        stamp,
        error: ServiceError::new(ErrorKind::Timeout, "No metadata reply"),
    }));
    assert!(
        matches!(&app.state().device_assets.entry(slot(0)).status, AssetMetadataReadStatus::Failed { error, .. } if error.kind == ErrorKind::Timeout)
    );
    app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
        epoch: SessionEpoch(2),
        state: ConnectionState::Recovering,
    }));
    app.dispatch(AppInput::Device(DeviceEvent::AssetMetadataReceived {
        stamp,
        metadata: metadata(stamp, 0, SnapshotSource::Simulator, 1),
    }));
    assert_eq!(app.state().device_assets.complete_count(AssetBank::Ir), 0);
    assert!(!app.device_operation_pending());
    assert!(app.dispatch(AppInput::Tick(10_000)).is_empty());
}
#[test]
fn reconnect_retains_old_metadata_as_stale_and_never_replays_read() {
    let mut app = app();
    let stamp = request(&mut app, 0);
    app.dispatch(AppInput::Device(DeviceEvent::AssetMetadataReceived {
        stamp,
        metadata: metadata(stamp, 0, SnapshotSource::Simulator, 1),
    }));
    let effects = app.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
        epoch: SessionEpoch(2),
        state: ConnectionState::Disconnected,
    }));
    assert!(effects.is_empty());
    assert_eq!(
        app.state().device_assets.entry(slot(0)).status,
        AssetMetadataReadStatus::Stale
    );
    assert_eq!(app.state().device_assets.complete_count(AssetBank::Ir), 0);
    assert!(app.state().device_assets.entry(slot(0)).snapshot.is_some());
}
#[test]
fn asset_navigation_and_ticks_are_passive() {
    let mut app = app();
    for input in [
        AppInput::Ui(UiIntent::Navigate(ViewId::Assets)),
        AppInput::Tick(1000),
        AppInput::Tick(5000),
    ] {
        assert!(app.dispatch(input).is_empty());
    }
    assert_eq!(app.state().device_assets.complete_count(AssetBank::Ir), 0);
}
