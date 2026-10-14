use gp200_domain::*;
use gp200_protocol::{RawRecord, WRITE_KIND, encode_transfer};
use gp200_transport::{DeviceActor, TransportAction, TransportConfig, capabilities_for_mode};
use std::time::Duration;

fn actions(actor: &mut DeviceActor) -> Vec<TransportAction> {
    std::iter::from_fn(|| actor.next_action()).collect()
}
fn receive(actor: &mut DeviceActor, epoch: SessionEpoch, raw: &RawTransfer, ms: u64) {
    for packet in encode_transfer(raw).unwrap() {
        actor.receive(epoch, &packet, Duration::from_millis(ms));
    }
}
fn stamp(actor: &DeviceActor, intent: u64) -> RequestStamp {
    RequestStamp {
        epoch: actor.epoch(),
        intent: IntentId(intent),
    }
}
fn fixture(index: usize) -> RawTransfer {
    let bank = RawTransfer::from_payload(
        WRITE_KIND,
        include_bytes!("../../../tests/fixtures/device-assets/connected-ir-metadata.payload.bin")
            .to_vec(),
    )
    .unwrap();
    RawTransfer::from_records(WRITE_KIND, &bank.records()[index..index + 1]).unwrap()
}
fn ready(mode: BackendMode) -> DeviceActor {
    let mut actor = DeviceActor::with_mode(
        TransportConfig {
            minimum_request_spacing: Duration::ZERO,
            request_timeout: Duration::from_millis(100),
            ..Default::default()
        },
        mode,
    );
    actor.command(
        DeviceCommand::Connect {
            intent: IntentId(1),
            selection: DeviceSelection {
                input_id: "fake-in".into(),
                output_id: "fake-out".into(),
                fingerprint_hint: None,
            },
        },
        Duration::ZERO,
    );
    actions(&mut actor);
    actor.opened(Ok(()), Duration::ZERO);
    actions(&mut actor);
    actor.sent(Ok(()), Duration::ZERO);
    let epoch = actor.epoch();
    receive(
        &mut actor,
        epoch,
        &RawTransfer::from_records(
            WRITE_KIND,
            &[RawRecord {
                command: 0x2001,
                body: vec![1, 0, 2, 0],
            }],
        )
        .unwrap(),
        1,
    );
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(2));
    receive(
        &mut actor,
        epoch,
        &RawTransfer::from_payload(
            WRITE_KIND,
            include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
        )
        .unwrap(),
        3,
    );
    actions(&mut actor);
    assert_eq!(actor.state(), ConnectionState::Ready);
    actor
}
fn begin(actor: &mut DeviceActor, index: u16, intent: u64) -> RequestStamp {
    let stamp = stamp(actor, intent);
    let slot = AssetSlot::new(AssetBank::Ir, index).unwrap();
    actor.command(
        DeviceCommand::ReadAssetMetadata { stamp, slot },
        Duration::from_millis(10),
    );
    let issued = actions(actor);
    let expected = encode_transfer(&gp200_protocol::asset_metadata_read_candidate(slot)).unwrap();
    assert!(
        issued.iter().any(
            |action| matches!(action, TransportAction::Send { packets } if *packets == expected)
        )
    );
    actor.sent(Ok(()), Duration::from_millis(10));
    stamp
}
fn has_metadata(actions: &[TransportAction]) -> bool {
    actions.iter().any(|action| {
        matches!(
            action,
            TransportAction::Event(DeviceEvent::AssetMetadataReceived { .. })
        )
    })
}
#[test]
fn all20_captures_are_exactly_correlated_and_hardware_provenance_is_preserved() {
    let mut actor = ready(BackendMode::Hardware);
    for index in 0..20 {
        let stamp = begin(&mut actor, index, u64::from(index) + 2);
        receive(&mut actor, stamp.epoch, &fixture(usize::from(index)), 11);
        let result = actions(&mut actor);
        assert!(result.iter().any(|action| matches!(action, TransportAction::Event(DeviceEvent::AssetMetadataReceived { stamp: actual, metadata }) if *actual == stamp && metadata.slot().index() == index && metadata.source() == &SnapshotSource::Hardware { epoch: stamp.epoch } && **metadata.raw() == fixture(usize::from(index)) )));
        assert!(!actor.is_busy());
        assert!(
            !result
                .iter()
                .any(|action| matches!(action, TransportAction::Send { .. }))
        );
    }
}
#[test]
fn wrong_slot_bank_and_status_do_not_satisfy_owned_metadata_read() {
    let mut actor = ready(BackendMode::Hardware);
    let request = begin(&mut actor, 0, 2);
    let mut wrong_bank = fixture(0).records()[0].clone();
    wrong_bank.command = 0x1012;
    let status = RawTransfer::from_records(
        WRITE_KIND,
        &[RawRecord {
            command: 0x100b,
            body: vec![0; 8],
        }],
    )
    .unwrap();
    for wrong in [
        fixture(1),
        RawTransfer::from_records(WRITE_KIND, &[wrong_bank]).unwrap(),
        status,
    ] {
        receive(&mut actor, request.epoch, &wrong, 11);
        assert!(!has_metadata(&actions(&mut actor)));
        assert!(actor.is_busy());
    }
    receive(&mut actor, request.epoch, &fixture(0), 12);
    assert!(has_metadata(&actions(&mut actor)));
}
#[test]
fn malformed_duplicate_and_extra_records_close_session_without_cache_or_write() {
    let record = fixture(0).records()[0].clone();
    for records in [
        vec![RawRecord {
            command: record.command,
            body: record.body[..23].to_vec(),
        }],
        vec![record.clone(), record.clone()],
        vec![
            record.clone(),
            RawRecord {
                command: 0x100b,
                body: vec![0; 8],
            },
        ],
    ] {
        let mut actor = ready(BackendMode::Hardware);
        let request = begin(&mut actor, 0, 2);
        receive(
            &mut actor,
            request.epoch,
            &RawTransfer::from_records(WRITE_KIND, &records).unwrap(),
            11,
        );
        let result = actions(&mut actor);
        assert!(!has_metadata(&result));
        assert!(result.iter().any(|action| matches!(action, TransportAction::Event(DeviceEvent::OperationFailed { stamp, error }) if *stamp == request && error.kind == ErrorKind::Protocol)));
        assert_eq!(actor.state(), ConnectionState::Recovering);
        assert!(
            !result
                .iter()
                .any(|action| matches!(action, TransportAction::Send { .. }))
        );
    }
}
#[test]
fn timeout_advances_epoch_and_late_or_stale_requests_cannot_satisfy_a_read() {
    let mut actor = ready(BackendMode::Hardware);
    let request = begin(&mut actor, 0, 2);
    actor.tick(Duration::from_millis(111));
    let result = actions(&mut actor);
    assert!(result.iter().any(|action| matches!(action, TransportAction::Event(DeviceEvent::OperationFailed { stamp, error }) if *stamp == request && error.kind == ErrorKind::Timeout)));
    assert!(actor.epoch() > request.epoch);
    receive(&mut actor, request.epoch, &fixture(0), 112);
    assert!(!has_metadata(&actions(&mut actor)));
    actor.command(
        DeviceCommand::ReadAssetMetadata {
            stamp: request,
            slot: AssetSlot::new(AssetBank::Ir, 0).unwrap(),
        },
        Duration::from_millis(113),
    );
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|action| matches!(action, TransportAction::Send { .. }))
    );
}
#[test]
fn hardware_snaptone_read_stays_unverified_and_simulator_has_separate_profiles() {
    let mut actor = ready(BackendMode::Hardware);
    actor.command(
        DeviceCommand::ReadAssetMetadata {
            stamp: stamp(&actor, 2),
            slot: AssetSlot::new(AssetBank::SnapTone, 0).unwrap(),
        },
        Duration::from_millis(10),
    );
    let result = actions(&mut actor);
    assert!(
        !result
            .iter()
            .any(|action| matches!(action, TransportAction::Send { .. }))
    );
    assert!(result.iter().any(|action| matches!(action, TransportAction::Event(DeviceEvent::OperationFailed { error, .. }) if error.kind == ErrorKind::Unverified)));
    let physical = DeviceIdentity {
        model: DeviceModel::Gp200,
        mode: BackendMode::Hardware,
        firmware: None,
        display_name: "GP-200".into(),
    };
    let simulation = capabilities_for_mode(BackendMode::Simulator);
    assert!(!simulation.allows(OperationKind::IrRead, &physical));
    assert!(!simulation.allows(OperationKind::SnapToneRead, &physical));
}
#[test]
fn unsolicited_reply_does_not_construct_metadata_or_change_current_buffer() {
    let mut actor = ready(BackendMode::Hardware);
    let before = actor.snapshot(PresetTarget::EditBuffer).unwrap().clone();
    let epoch = actor.epoch();
    receive(&mut actor, epoch, &fixture(0), 10);
    assert!(!has_metadata(&actions(&mut actor)));
    assert_eq!(actor.snapshot(PresetTarget::EditBuffer).unwrap(), &before);
}
