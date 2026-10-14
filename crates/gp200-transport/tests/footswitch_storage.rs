//! Complete independent physical captures exercise the production actor.
use gp200_domain::*;
use gp200_protocol::*;
#[cfg(gp200_private_evidence)]
use gp200_transport::hardware_global_verified;
use gp200_transport::{DeviceActor, TransportAction, TransportConfig};
use serde_json::Value;
use std::time::Duration;
#[cfg(gp200_private_evidence)]
use std::{fs, path::PathBuf};

fn entries() -> Vec<Value> {
    let m: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/globals/footswitch-storage/manifest.json"
    ))
    .unwrap();
    m["entries"].as_array().unwrap().clone()
}
#[cfg(gp200_private_evidence)]
fn bytes(entry: &Value, key: &str) -> Vec<u8> {
    fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(entry["files"][key]["path"].as_str().unwrap()),
    )
    .unwrap()
}
#[cfg(gp200_private_evidence)]
fn raw(entry: &Value, key: &str) -> RawTransfer {
    RawTransfer::from_payload(WRITE_KIND, bytes(entry, key)).unwrap()
}
fn patch(entry: &Value) -> GlobalControlEdit {
    GlobalControlEdit::Footswitch {
        table: GlobalFootswitchTableId::new(entry["table"].as_u64().unwrap() as u16).unwrap(),
        event: GlobalFootswitchEvent::new(entry["event"].as_u64().unwrap() as u8).unwrap(),
        function: GlobalFootswitchFunction::new(entry["requested"].as_u64().unwrap() as u8)
            .unwrap(),
    }
}
fn actions(actor: &mut DeviceActor) -> Vec<TransportAction> {
    std::iter::from_fn(|| actor.next_action()).collect()
}
fn receive(actor: &mut DeviceActor, raw: &RawTransfer, ms: u64) {
    for packet in encode_transfer(raw).unwrap() {
        actor.receive(actor.epoch(), &packet, Duration::from_millis(ms));
    }
}
#[cfg(gp200_private_evidence)]
fn ready(mode: BackendMode, model: u8) -> DeviceActor {
    ready_identity(mode, 1, model)
}
fn ready_identity(mode: BackendMode, state: u8, model: u8) -> DeviceActor {
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
    actor.sent(Ok(()), Duration::ZERO);
    receive(
        &mut actor,
        &RawTransfer::from_records(
            WRITE_KIND,
            &[RawRecord {
                command: 0x2001,
                body: vec![state, 0, model, 0],
            }],
        )
        .unwrap(),
        1,
    );
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(2));
    receive(
        &mut actor,
        &RawTransfer::from_payload(
            WRITE_KIND,
            include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
        )
        .unwrap(),
        3,
    );
    actions(&mut actor);
    actor
}
#[cfg(gp200_private_evidence)]
fn baseline(actor: &mut DeviceActor, raw: &RawTransfer) {
    actor.command(
        DeviceCommand::ReadGlobals(RequestStamp {
            epoch: actor.epoch(),
            intent: IntentId(2),
        }),
        Duration::from_millis(10),
    );
    actions(actor);
    actor.sent(Ok(()), Duration::from_millis(10));
    receive(actor, raw, 11);
    actions(actor);
}
fn submit(actor: &mut DeviceActor, entry: &Value, revision: u64, epoch: SessionEpoch) {
    actor.command(
        DeviceCommand::SubmitGlobalControlEdit {
            stamp: RequestStamp {
                epoch,
                intent: IntentId(3),
            },
            base_revision: Revision(revision),
            patch: patch(entry),
        },
        Duration::from_millis(20),
    );
}
#[cfg(gp200_private_evidence)]
fn verified(actions: &[TransportAction]) -> bool {
    actions.iter().any(|a| {
        matches!(
            a,
            TransportAction::Event(DeviceEvent::EditProgress {
                stage: EditStage::Verified,
                ..
            })
        )
    })
}

#[test]
#[cfg(gp200_private_evidence)]
fn all_ninety_captures_use_fresh_preflight_echo_is_insufficient_and_full_readback_verifies() {
    for entry in entries() {
        let mut actor = ready(BackendMode::Hardware, 2);
        let current = actor.snapshot(PresetTarget::EditBuffer).unwrap().clone();
        baseline(&mut actor, &raw(&entry, "before"));
        assert!(hardware_global_verified(
            &patch(&entry),
            actor.global_state().unwrap()
        ));
        let epoch = actor.epoch();
        submit(&mut actor, &entry, 1, epoch);
        assert!(actions(&mut actor).iter().any(|a| matches!(a,TransportAction::Send { packets } if *packets == encode_transfer(&read_globals_request().unwrap()).unwrap())));
        actor.sent(Ok(()), Duration::from_millis(20));
        receive(&mut actor, &raw(&entry, "before"), 21);
        assert!(actions(&mut actor).iter().any(|a| matches!(a,TransportAction::Send { packets } if packets.concat() == bytes(&entry,"request"))));
        actor.sent(Ok(()), Duration::from_millis(22));
        receive(&mut actor, &patch(&entry).request().unwrap(), 23);
        assert!(!verified(&actions(&mut actor)));
        receive(&mut actor, &raw(&entry, "after"), 24);
        assert!(!verified(&actions(&mut actor)));
        actor.tick(Duration::from_millis(70));
        actions(&mut actor);
        actor.sent(Ok(()), Duration::from_millis(70));
        receive(&mut actor, &raw(&entry, "after"), 71);
        assert!(verified(&actions(&mut actor)), "trial{}", entry["index"]);
        assert_eq!(actor.snapshot(PresetTarget::EditBuffer).unwrap(), &current);
    }
}

#[test]
#[cfg(gp200_private_evidence)]
fn physical_mode_change_during_preflight_stops_the_mapping_before_send() {
    let entry = &entries()[0];
    let mut actor = ready(BackendMode::Hardware, 2);
    baseline(&mut actor, &raw(entry, "before"));
    let epoch = actor.epoch();
    submit(&mut actor, entry, 1, epoch);
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(20));
    let fresh = actor
        .global_state()
        .unwrap()
        .expected_after(&GlobalControlEdit::General {
            field: GeneralSetting::FootswitchMode,
            value: 2,
        })
        .unwrap();
    receive(&mut actor, &fresh, 21);
    let result = actions(&mut actor);
    assert!(
        !result
            .iter()
            .any(|a| matches!(a, TransportAction::Send { .. }))
    );
    assert!(result.iter().any(|a| matches!(a,TransportAction::Event(DeviceEvent::OperationFailed { error,.. }) if error.kind == ErrorKind::Conflict)));
    assert_eq!(actor.global_state().unwrap().revision, Revision(2));
}

#[test]
#[cfg(gp200_private_evidence)]
fn stale_session_revision_and_replay_mode_never_send_a_mapping() {
    let entry = &entries()[0];
    for (mode, revision, old_epoch) in [
        (BackendMode::Hardware, 0, false),
        (BackendMode::Hardware, 1, true),
        (BackendMode::CaptureReplay, 1, false),
    ] {
        let mut actor = ready(mode, 2);
        baseline(&mut actor, &raw(entry, "before"));
        let epoch = if old_epoch {
            SessionEpoch(actor.epoch().0 + 1)
        } else {
            actor.epoch()
        };
        submit(&mut actor, entry, revision, epoch);
        let result = actions(&mut actor);
        assert!(
            !result
                .iter()
                .any(|a| matches!(a, TransportAction::Send { .. }))
        );
        assert!(!verified(&result));
    }
}

#[test]
fn another_device_variant_or_non_normal_state_cannot_use_the_normal_gp200_storage_profile() {
    let entry = &entries()[0];
    for (state, model) in [(1, 3), (1, 4), (1, 255), (0, 2), (2, 2)] {
        let mut actor = ready_identity(BackendMode::Hardware, state, model);
        assert_eq!(actor.state(), ConnectionState::Unsupported);
        let epoch = actor.epoch();
        submit(&mut actor, entry, 1, epoch);
        assert!(
            !actions(&mut actor)
                .iter()
                .any(|a| matches!(a, TransportAction::Send { .. }))
        );
    }
}

#[test]
#[cfg(gp200_private_evidence)]
fn mismatched_complete_readback_reports_conflict_and_does_not_replay() {
    let entry = &entries()[0];
    let mut actor = ready(BackendMode::Hardware, 2);
    baseline(&mut actor, &raw(entry, "before"));
    let epoch = actor.epoch();
    submit(&mut actor, entry, 1, epoch);
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(20));
    receive(&mut actor, &raw(entry, "before"), 21);
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(22));
    actor.tick(Duration::from_millis(70));
    actions(&mut actor);
    actor.sent(Ok(()), Duration::from_millis(70));
    receive(&mut actor, &raw(entry, "before"), 71);
    let result = actions(&mut actor);
    assert!(!verified(&result));
    assert!(result.iter().any(|a| matches!(
        a,
        TransportAction::Event(DeviceEvent::EditProgress {
            stage: EditStage::Failed(error),
            ..
        }) if error.kind == ErrorKind::Conflict
    )));
    actor.tick(Duration::from_secs(5));
    assert!(
        !actions(&mut actor)
            .iter()
            .any(|a| matches!(a, TransportAction::Send { .. }))
    );
}
