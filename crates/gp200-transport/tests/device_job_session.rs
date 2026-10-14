use gp200_domain::*;
use gp200_protocol::{RawRecord, WRITE_KIND, encode_transfer};
use gp200_transport::{DeviceActor, TransportAction, TransportConfig, verified_capabilities};
use std::time::Duration;

fn actions(actor: &mut DeviceActor) -> Vec<TransportAction> {
    std::iter::from_fn(|| actor.next_action()).collect()
}
fn identify(state: u16) -> RawTransfer {
    RawTransfer::from_records(
        WRITE_KIND,
        &[RawRecord {
            command: 0x2001,
            body: [state.to_le_bytes(), 2_u16.to_le_bytes()].concat(),
        }],
    )
    .unwrap()
}
fn reply(actor: &mut DeviceActor, raw: &RawTransfer) {
    for packet in encode_transfer(raw).unwrap() {
        actor.receive(actor.epoch(), &packet, Duration::from_millis(1));
    }
}
fn connected() -> (DeviceActor, DeviceSelection) {
    let mut actor = DeviceActor::new(TransportConfig {
        minimum_request_spacing: Duration::ZERO,
        ..TransportConfig::default()
    });
    let selection = DeviceSelection {
        input_id: "hw:GP200,0,0".into(),
        output_id: "hw:GP200,0,0".into(),
        fingerprint_hint: None,
    };
    actor.command(
        DeviceCommand::Connect {
            intent: IntentId(1),
            selection: selection.clone(),
        },
        Duration::ZERO,
    );
    actions(&mut actor);
    actor.opened(Ok(()), Duration::ZERO);
    actions(&mut actor);
    actor.sent(Ok(()), Duration::ZERO);
    (actor, selection)
}
#[test]
fn actor_emits_exact_endpoints_identity_and_capabilities_without_write_authority() {
    let (mut actor, selection) = connected();
    let raw = identify(1);
    reply(&mut actor, &raw);
    let observations: Vec<_> = actions(&mut actor)
        .into_iter()
        .filter_map(|action| match action {
            TransportAction::Event(DeviceEvent::DeviceJobSessionObserved(session)) => Some(session),
            _ => None,
        })
        .collect();
    assert_eq!(observations.len(), 1);
    let observation = &observations[0];
    assert_eq!(observation.selection(), &selection);
    assert_eq!(observation.epoch(), actor.epoch());
    assert_eq!(observation.identity_revision(), Revision(1));
    assert_eq!(observation.identity_sha256(), JobSha256::of(raw.payload()));
    assert!(observation.identity().firmware.is_none());
    assert!(observation.matches_capabilities(observation.identity(), &verified_capabilities()));
    for operation in [
        OperationKind::IrWrite,
        OperationKind::SnapToneWrite,
        OperationKind::FirmwareUpdate,
    ] {
        assert!(!verified_capabilities().allows(operation, observation.identity()));
    }
    assert_eq!(actor.state(), ConnectionState::Synchronizing);
}
#[test]
fn unsupported_native_state_does_not_create_job_session_observation() {
    let (mut actor, _) = connected();
    reply(&mut actor, &identify(2));
    assert!(!actions(&mut actor).iter().any(|action| matches!(
        action,
        TransportAction::Event(DeviceEvent::DeviceJobSessionObserved(_))
    )));
    assert_eq!(actor.state(), ConnectionState::Unsupported);
}
