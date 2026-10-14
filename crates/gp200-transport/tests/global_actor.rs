use gp200_domain::*;
use gp200_protocol::*;
use gp200_transport::{DeviceActor, TransportAction, TransportConfig, hardware_global_verified};
use std::time::Duration;

fn raw() -> RawTransfer {
    RawTransfer::from_payload(
        WRITE_KIND,
        include_bytes!("../../../tests/fixtures/globals/baseline.payload.bin").to_vec(),
    )
    .unwrap()
}

#[test]
fn hardware_profiles_are_field_specific_and_stop_works_at_ordinary_volume() {
    let before =
        DeviceGlobalState::from_transfer(Revision(1), SessionEpoch(1), raw(), vec![]).unwrap();
    assert_eq!(
        before
            .records()
            .drum
            .as_ref()
            .unwrap()
            .value(DrumSetting::Volume),
        80
    );
    assert!(hardware_global_verified(
        &GlobalControlEdit::DrumPlay(true),
        &before
    ));
    let playing = DeviceGlobalState::from_transfer(
        Revision(2),
        SessionEpoch(1),
        before
            .expected_after(&GlobalControlEdit::DrumPlay(true))
            .unwrap(),
        vec![],
    )
    .unwrap();
    assert!(hardware_global_verified(
        &GlobalControlEdit::DrumPlay(false),
        &playing
    ));
    assert!(!hardware_global_verified(
        &GlobalControlEdit::Drum {
            field: DrumSetting::Volume,
            value: 81
        },
        &playing
    ));
    let edit = GlobalControlEdit::Footswitch {
        table: GlobalFootswitchTableId::new(2).unwrap(),
        event: GlobalFootswitchEvent::new(19).unwrap(),
        function: GlobalFootswitchFunction::new(1).unwrap(),
    };
    assert!(hardware_global_verified(&edit, &before));
    for (table, event, function) in [(0, 19, 1), (2, 1, 1), (2, 19, 8)] {
        assert!(!hardware_global_verified(
            &GlobalControlEdit::Footswitch {
                table: GlobalFootswitchTableId::new(table).unwrap(),
                event: GlobalFootswitchEvent::new(event).unwrap(),
                function: GlobalFootswitchFunction::new(function).unwrap(),
            },
            &before
        ));
    }
    let active_user = DeviceGlobalState::from_transfer(
        Revision(2),
        SessionEpoch(1),
        before
            .expected_after(&GlobalControlEdit::General {
                field: GeneralSetting::FootswitchMode,
                value: 2,
            })
            .unwrap(),
        vec![],
    )
    .unwrap();
    assert!(!hardware_global_verified(&edit, &active_user));
    assert!(hardware_global_verified(
        &GlobalControlEdit::Eq {
            parameter: EqParameter::Band2Gain,
            value: F32Bits::from_f32(0.1)
        },
        &before
    ));
}
fn actions(actor: &mut DeviceActor) -> Vec<TransportAction> {
    std::iter::from_fn(|| actor.next_action()).collect()
}
fn events(actions: Vec<TransportAction>) -> Vec<DeviceEvent> {
    actions
        .into_iter()
        .filter_map(|a| {
            if let TransportAction::Event(e) = a {
                Some(e)
            } else {
                None
            }
        })
        .collect()
}
fn stamp(actor: &DeviceActor, intent: u64) -> RequestStamp {
    RequestStamp {
        epoch: actor.epoch(),
        intent: IntentId(intent),
    }
}
fn receive(actor: &mut DeviceActor, raw: &RawTransfer, ms: u64) {
    for packet in encode_transfer(raw).unwrap() {
        actor.receive(actor.epoch(), &packet, Duration::from_millis(ms));
    }
}
fn sent(actor: &mut DeviceActor, ms: u64) {
    actor.sent(Ok(()), Duration::from_millis(ms));
}
fn ready(mode: BackendMode) -> DeviceActor {
    let mut actor = DeviceActor::with_mode(
        TransportConfig {
            minimum_request_spacing: Duration::ZERO,
            ..TransportConfig::default()
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
    sent(&mut actor, 0);
    receive(
        &mut actor,
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
    sent(&mut actor, 2);
    let current = RawTransfer::from_payload(
        WRITE_KIND,
        include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
    )
    .unwrap();
    receive(&mut actor, &current, 3);
    actions(&mut actor);
    assert_eq!(actor.state(), ConnectionState::Ready);
    actor
}
fn baseline(actor: &mut DeviceActor) {
    actor.command(
        DeviceCommand::ReadGlobals(stamp(actor, 2)),
        Duration::from_millis(10),
    );
    let sends = actions(actor);
    assert_eq!(
        sends
            .iter()
            .filter(|a| matches!(a, TransportAction::Send { .. }))
            .count(),
        1
    );
    sent(actor, 10);
    receive(actor, &raw(), 11);
    let read = events(actions(actor));
    assert!(read.iter().any(
        |e| matches!(e,DeviceEvent::GlobalStateReceived{state,..} if state.revision==Revision(1))
    ));
    assert!(!read.iter().any(|e| matches!(
        e,
        DeviceEvent::EditProgress {
            stage: EditStage::Verified,
            ..
        }
    )));
}
fn submit(actor: &mut DeviceActor, patch: GlobalControlEdit) {
    actor.command(
        DeviceCommand::SubmitGlobalControlEdit {
            stamp: stamp(actor, 3),
            base_revision: actor.global_state().unwrap().revision,
            patch,
        },
        Duration::from_millis(20),
    );
}
fn display() -> GlobalControlEdit {
    GlobalControlEdit::General {
        field: GeneralSetting::DisplayMode,
        value: 1,
    }
}
fn verified(events: &[DeviceEvent]) -> bool {
    events.iter().any(|e| {
        matches!(
            e,
            DeviceEvent::EditProgress {
                stage: EditStage::Verified,
                ..
            }
        )
    })
}

#[test]
fn fresh_global_preflight_then_complete_readback_verifies_without_touching_presets() {
    let mut actor = ready(BackendMode::Hardware);
    let current = actor.snapshot(PresetTarget::EditBuffer).unwrap().clone();
    baseline(&mut actor);
    let before = actor.global_state().unwrap().clone();
    submit(&mut actor, display());
    let preflight = actions(&mut actor);
    assert!(preflight.iter().any(|a|matches!(a,TransportAction::Send{packets} if *packets==encode_transfer(&read_globals_request().unwrap()).unwrap())));
    sent(&mut actor, 20);
    receive(&mut actor, &raw(), 21);
    let write = actions(&mut actor);
    assert!(write.iter().any(|a|matches!(a,TransportAction::Send{packets} if *packets==encode_transfer(&display().request().unwrap()).unwrap())));
    sent(&mut actor, 22);
    receive(&mut actor, &display().request().unwrap(), 23);
    assert!(!verified(&events(actions(&mut actor))));
    receive(&mut actor, &before.expected_after(&display()).unwrap(), 24);
    assert!(!verified(&events(actions(&mut actor)))); // full state before the query is not readback
    actor.tick(Duration::from_millis(70));
    let readback = actions(&mut actor);
    assert!(readback.iter().any(|a|matches!(a,TransportAction::Send{packets} if *packets==encode_transfer(&read_globals_request().unwrap()).unwrap())));
    sent(&mut actor, 70);
    let expected = before.expected_after(&display()).unwrap();
    receive(&mut actor, &expected, 71);
    let observed = events(actions(&mut actor));
    assert!(verified(&observed));
    let actual = observed
        .iter()
        .find_map(|event| match event {
            DeviceEvent::GlobalStateReceived { stamp, state } if stamp.intent == IntentId(3) => {
                Some(state)
            }
            _ => None,
        })
        .expect("Exact owned complete global readback was emitted");
    assert_eq!(actual.revision, Revision(2));
    assert_eq!(actual.records().raw().payload(), expected.payload());
    // The bare kind12 global write echo invalidates Globals. The complete
    // kind11 state seen before the query is diagnostic and grants no ACK or
    // invalidation authority for Current.
    assert!(actor.global_state().is_none());
    assert_eq!(actor.snapshot(PresetTarget::EditBuffer).unwrap(), &current);
    actor.command(
        DeviceCommand::ReadGlobals(stamp(&actor, 4)),
        Duration::from_millis(72),
    );
    actions(&mut actor);
    sent(&mut actor, 72);
    receive(&mut actor, &expected, 73);
    actions(&mut actor);
    assert_eq!(actor.global_state().unwrap().revision, Revision(3));
    assert_eq!(actor.snapshot(PresetTarget::EditBuffer).unwrap(), &current);
    assert_eq!(
        actor
            .global_state()
            .unwrap()
            .records()
            .general
            .as_ref()
            .unwrap()
            .value(GeneralSetting::DisplayMode),
        1
    );
}

#[test]
fn changed_preflight_refreshes_actual_state_and_never_sends_the_write() {
    let mut actor = ready(BackendMode::Hardware);
    baseline(&mut actor);
    submit(&mut actor, display());
    actions(&mut actor);
    sent(&mut actor, 20);
    let mut records = raw().records().to_vec();
    records
        .iter_mut()
        .find(|r| r.command == GENERAL_SETTINGS_COMMAND)
        .unwrap()
        .body[53] ^= 1;
    receive(
        &mut actor,
        &RawTransfer::from_records(WRITE_KIND, &records).unwrap(),
        21,
    );
    let result = actions(&mut actor);
    assert!(
        !result
            .iter()
            .any(|a| matches!(a, TransportAction::Send { .. }))
    );
    assert!(events(result).iter().any(
        |e| matches!(e,DeviceEvent::OperationFailed{error,..} if error.kind==ErrorKind::Conflict)
    ));
    assert!(!actor.is_busy());
    assert_eq!(actor.global_state().unwrap().revision, Revision(2));
}

#[test]
fn unsupported_hardware_mapping_and_stale_revision_are_rejected_before_queries() {
    let mut actor = ready(BackendMode::Hardware);
    baseline(&mut actor);
    submit(
        &mut actor,
        GlobalControlEdit::Footswitch {
            table: GlobalFootswitchTableId::new(0).unwrap(),
            event: GlobalFootswitchEvent::new(18).unwrap(),
            function: GlobalFootswitchFunction::new(2).unwrap(),
        },
    );
    let a = actions(&mut actor);
    assert!(!a.iter().any(|a| matches!(a, TransportAction::Send { .. })));
    assert!(events(a).iter().any(
        |e| matches!(e,DeviceEvent::OperationFailed{error,..} if error.kind==ErrorKind::Unverified)
    ));
    actor.command(
        DeviceCommand::SubmitGlobalControlEdit {
            stamp: stamp(&actor, 4),
            base_revision: Revision(0),
            patch: display(),
        },
        Duration::from_millis(21),
    );
    assert!(events(actions(&mut actor)).iter().any(
        |e| matches!(e,DeviceEvent::OperationFailed{error,..} if error.kind==ErrorKind::Conflict)
    ));
}

#[test]
fn malformed_global_read_cannot_satisfy_the_request() {
    let mut actor = ready(BackendMode::Hardware);
    actor.command(
        DeviceCommand::ReadGlobals(stamp(&actor, 2)),
        Duration::from_millis(10),
    );
    actions(&mut actor);
    sent(&mut actor, 10);
    let mut records = raw().records().to_vec();
    records.push(
        records
            .iter()
            .find(|r| r.command == GENERAL_SETTINGS_COMMAND)
            .unwrap()
            .clone(),
    );
    receive(
        &mut actor,
        &RawTransfer::from_records(WRITE_KIND, &records).unwrap(),
        11,
    );
    assert!(actor.global_state().is_none());
    assert!(actor.is_busy());
    assert!(
        !events(actions(&mut actor))
            .iter()
            .any(|e| matches!(e, DeviceEvent::GlobalStateReceived { .. }))
    );
    actor.tick(Duration::from_millis(1600));
    assert_eq!(actor.state(), ConnectionState::Recovering);
}

#[test]
fn lost_global_write_is_indeterminate_and_is_never_replayed() {
    let mut actor = ready(BackendMode::Hardware);
    baseline(&mut actor);
    submit(&mut actor, display());
    actions(&mut actor);
    sent(&mut actor, 20);
    receive(&mut actor, &raw(), 21);
    actions(&mut actor);
    sent(&mut actor, 22);
    actor.lost(ServiceError::new(ErrorKind::Disconnected, "test unplug"));
    let result = events(actions(&mut actor));
    assert!(result.iter().any(|e| matches!(
        e,
        DeviceEvent::EditProgress {
            stage: EditStage::Indeterminate(_),
            ..
        }
    )));
    assert!(actor.global_state().is_none());
    assert!(actor.snapshot(PresetTarget::EditBuffer).is_none());
    actor.tick(Duration::from_secs(5));
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|a| matches!(a, TransportAction::Send { .. }))
    );
}

#[test]
fn unrelated_readback_change_fails_while_returning_the_actual_globals() {
    let mut actor = ready(BackendMode::Hardware);
    baseline(&mut actor);
    let before = actor.global_state().unwrap().clone();
    submit(&mut actor, display());
    actions(&mut actor);
    sent(&mut actor, 20);
    receive(&mut actor, &raw(), 21);
    actions(&mut actor);
    sent(&mut actor, 22);
    actor.tick(Duration::from_millis(70));
    actions(&mut actor);
    sent(&mut actor, 70);
    let mut records = before
        .expected_after(&display())
        .unwrap()
        .records()
        .to_vec();
    records
        .iter_mut()
        .find(|r| r.command == 0x1010)
        .unwrap()
        .body[3] ^= 1;
    receive(
        &mut actor,
        &RawTransfer::from_records(WRITE_KIND, &records).unwrap(),
        71,
    );
    let e = events(actions(&mut actor));
    assert!(!verified(&e));
    assert!(e.iter().any(
        |e| matches!(e,DeviceEvent::OperationFailed{error,..} if error.kind==ErrorKind::Conflict)
    ));
    assert_eq!(actor.global_state().unwrap().revision, Revision(2));
}
