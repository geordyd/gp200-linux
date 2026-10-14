use gp200_domain::*;
use gp200_sim::*;

fn connected() -> DemoDevice {
    let mut device = DemoDevice::new();
    let events = device.handle(DeviceCommand::Connect {
        intent: IntentId(1),
        selection: simulator_port().selection,
    });
    assert!(events.iter().any(|event| matches!(event, DeviceEvent::SessionChanged { identity, .. } if identity.mode == BackendMode::Simulator)));
    device
}
fn edit(stamp: RequestStamp, revision: Revision, value: f32) -> DeviceCommand {
    DeviceCommand::SubmitEdit {
        stamp,
        target: PresetTarget::EditBuffer,
        base_revision: revision,
        patch: EditPatch::SetParameter {
            module: ModuleId::new(5).unwrap(),
            model: ModelCode(0x0a00_0010),
            model_index: ModelIndex(16),
            parameter: ParamId::new(1).unwrap(),
            value: F32Bits::from_f32(value),
        },
    }
}
#[test]
fn stored_preview_between_edits_never_redirects_buffer_context() {
    let mut device = connected();
    let epoch = device.epoch;
    let slot = SlotId::new(23).unwrap();
    device.handle(edit(
        RequestStamp {
            epoch,
            intent: IntentId(2),
        },
        Revision(0),
        61.0,
    ));
    let stored = device.handle(DeviceCommand::ReadStored {
        stamp: RequestStamp {
            epoch,
            intent: IntentId(3),
        },
        slot,
    });
    assert!(
        matches!(&stored[0], DeviceEvent::SnapshotReceived { snapshot, .. } if snapshot.context == Some(WireContext { group: 1, selector: 23 }))
    );
    device.handle(edit(
        RequestStamp {
            epoch,
            intent: IntentId(4),
        },
        Revision(1),
        62.0,
    ));
    assert_eq!(
        device.current.context,
        Some(WireContext {
            group: 0,
            selector: 23
        })
    );
    assert_eq!(
        device
            .current
            .view
            .module(ModuleId::new(5).unwrap())
            .unwrap()
            .parameter(ParamId::new(1).unwrap())
            .unwrap()
            .as_f32(),
        62.0
    );
    assert!(device.stored.is_empty());
}
#[test]
fn old_epoch_requests_after_reconnect_fail_without_mutating() {
    let mut device = connected();
    let epoch = device.epoch;
    device.handle(DeviceCommand::Disconnect {
        intent: IntentId(2),
    });
    device.handle(DeviceCommand::Connect {
        intent: IntentId(3),
        selection: simulator_port().selection,
    });
    let before = device.current.clone();
    let events = device.handle(edit(
        RequestStamp {
            epoch,
            intent: IntentId(4),
        },
        Revision(0),
        50.0,
    ));
    assert!(
        matches!(&events[0], DeviceEvent::OperationFailed { error, .. } if error.kind == ErrorKind::Disconnected)
    );
    assert_eq!(device.current, before);
}
#[test]
fn save_is_explicit_and_retains_buffer_context() {
    let mut device = connected();
    let epoch = device.epoch;
    let slot = SlotId::new(52).unwrap();
    device.handle(edit(
        RequestStamp {
            epoch,
            intent: IntentId(2),
        },
        Revision(0),
        88.0,
    ));
    device.handle(DeviceCommand::SaveBufferTo {
        stamp: RequestStamp {
            epoch,
            intent: IntentId(3),
        },
        slot,
        expected_buffer_revision: Revision(1),
    });
    assert_eq!(
        device.stored[&slot].context,
        Some(WireContext {
            group: 1,
            selector: 52
        })
    );
    assert_eq!(
        device.current.context,
        Some(WireContext {
            group: 0,
            selector: 23
        })
    );
    assert_eq!(
        device.stored[&slot].view.modules,
        device.current.view.modules
    );
}
#[test]
fn same_commands_produce_same_bytes() {
    let mut one = connected();
    let mut two = connected();
    let command = edit(
        RequestStamp {
            epoch: one.epoch,
            intent: IntentId(2),
        },
        Revision(0),
        75.0,
    );
    one.handle(command.clone());
    two.handle(command);
    assert_eq!(one.current, two.current);
}
