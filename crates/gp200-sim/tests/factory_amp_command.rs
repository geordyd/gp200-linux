// Private capture acceptance suite; excluded from public source builds.
// Run with RUSTFLAGS="--cfg gp200_private_evidence" only in the retained lab checkout.
#![cfg(gp200_private_evidence)]

use gp200_domain::*;
use gp200_sim::*;
use std::sync::Arc;

#[test]
fn simulator_rejects_the_physical_candidate_without_mutation_or_saved_state() {
    let mut device = DemoDevice::new();
    device.handle(DeviceCommand::Connect {
        intent: IntentId(1),
        selection: simulator_port().selection,
    });
    let before = device.current.clone();
    let globals=DeviceGlobalState::from_transfer(Revision(1),device.epoch,RawTransfer::from_payload(0x12,include_bytes!("../../../analysis/implementation/dependent-model-normal-v3-root/amp-auto-cab-on-v3-run01/globals-before.payload.bin").to_vec()).unwrap(),vec![]).unwrap();
    let events = device.handle(DeviceCommand::SubmitFactoryAmpModel {
        stamp: RequestStamp {
            epoch: device.epoch,
            intent: IntentId(2),
        },
        base_revision: before.revision,
        model: ModelCode(0x07000003),
        profile: FactoryAmpProfileId::new(ModelCode(0x07000003), true).unwrap(),
        before_globals: Arc::new(globals),
        expected_auto_cab: true,
    });
    assert!(
        matches!(events.as_slice(),[DeviceEvent::OperationFailed { error,.. }] if error.kind==ErrorKind::Unsupported)
    );
    assert_eq!(device.current, before);
    assert!(device.stored.is_empty());
}
