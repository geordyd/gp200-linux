//! Independent captured requests/readbacks, not simulator-generated goldens.
use gp200_domain::*;
use gp200_protocol::*;
use gp200_transport::{DeviceActor, TransportAction, TransportConfig, hardware_global_verified};
use serde_json::Value;
use std::{collections::BTreeSet, fs, path::PathBuf, time::Duration};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/globals/breadth")
}
fn entries() -> Vec<Value> {
    let manifest: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/globals/breadth/manifest.json"
    ))
    .unwrap();
    manifest["entries"].as_array().unwrap().clone()
}
fn bytes(entry: &Value, name: &str) -> Vec<u8> {
    fs::read(fixtures().join(entry["files"][name]["file"].as_str().unwrap())).unwrap()
}
fn raw(entry: &Value, name: &str) -> RawTransfer {
    RawTransfer::from_payload(WRITE_KIND, bytes(entry, name)).unwrap()
}
fn edit(entry: &Value) -> GlobalControlEdit {
    if entry["kind"] == "general" {
        GlobalControlEdit::General {
            field: serde_json::from_value(entry["field"].clone()).unwrap(),
            value: entry["value"].as_i64().unwrap() as i16,
        }
    } else {
        GlobalControlEdit::Eq {
            parameter: EqParameter::from_index(entry["index"].as_u64().unwrap() as u16).unwrap(),
            value: F32Bits(entry["value"].as_u64().unwrap() as u32),
        }
    }
}
fn state(raw: RawTransfer, revision: u64) -> DeviceGlobalState {
    DeviceGlobalState::from_transfer(Revision(revision), SessionEpoch(1), raw, vec![]).unwrap()
}

#[test]
fn every_promoted_field_matches_independently_captured_wire_and_complete_state() {
    let mut general = BTreeSet::new();
    let mut eq = BTreeSet::new();
    for entry in entries() {
        let before = state(raw(&entry, "before.payload.bin"), 1);
        let after = state(raw(&entry, "after.payload.bin"), 2);
        let restored = state(raw(&entry, "restored.payload.bin"), 3);
        let patch = edit(&entry);
        assert!(
            hardware_global_verified(&patch, &before),
            "{}",
            entry["name"]
        );
        assert_eq!(
            encode_transfer(&patch.request().unwrap()).unwrap().concat(),
            bytes(&entry, "request.bin")
        );
        assert!(
            before.verifies_expected(&patch, &after).unwrap(),
            "{}",
            entry["name"]
        );
        assert!(
            before.records().control_state_equal(restored.records()),
            "{}",
            entry["name"]
        );
        // Raw equality is deliberately not the comparison policy: observed
        // volatile/unused fields remain unchanged in the captured fixtures.
        let restore = match patch {
            GlobalControlEdit::General { field, .. } => {
                general.insert(field);
                GlobalControlEdit::General {
                    field,
                    value: before.records().general.as_ref().unwrap().value(field),
                }
            }
            GlobalControlEdit::Eq { parameter, .. } => {
                eq.insert(parameter);
                GlobalControlEdit::Eq {
                    parameter,
                    value: before.records().eq.as_ref().unwrap().values[parameter.index() as usize],
                }
            }
            _ => unreachable!(),
        };
        assert_eq!(
            encode_transfer(&restore.request().unwrap())
                .unwrap()
                .concat(),
            bytes(&entry, "restore-request.bin")
        );
    }
    assert_eq!(general, GeneralSetting::ALL.into_iter().collect());
    assert_eq!(eq, EqParameter::ALL.into_iter().collect());
}

fn actions(actor: &mut DeviceActor) -> Vec<TransportAction> {
    std::iter::from_fn(|| actor.next_action()).collect()
}
fn receive(actor: &mut DeviceActor, raw: &RawTransfer, ms: u64) {
    for packet in encode_transfer(raw).unwrap() {
        actor.receive(actor.epoch(), &packet, Duration::from_millis(ms));
    }
}
fn ready() -> DeviceActor {
    let mut actor = DeviceActor::with_mode(
        TransportConfig {
            minimum_request_spacing: Duration::ZERO,
            ..TransportConfig::default()
        },
        BackendMode::Hardware,
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

#[test]
fn all_captured_profiles_use_fresh_preflight_and_wait_for_whole_state_readback() {
    for entry in entries() {
        let mut actor = ready();
        let current = actor.snapshot(PresetTarget::EditBuffer).unwrap().clone();
        let stamp = RequestStamp {
            epoch: actor.epoch(),
            intent: IntentId(2),
        };
        actor.command(DeviceCommand::ReadGlobals(stamp), Duration::from_millis(10));
        actions(&mut actor);
        actor.sent(Ok(()), Duration::from_millis(10));
        receive(&mut actor, &raw(&entry, "before.payload.bin"), 11);
        actions(&mut actor);
        let stamp = RequestStamp {
            epoch: actor.epoch(),
            intent: IntentId(3),
        };
        actor.command(
            DeviceCommand::SubmitGlobalControlEdit {
                stamp,
                base_revision: Revision(1),
                patch: edit(&entry),
            },
            Duration::from_millis(20),
        );
        assert!(actions(&mut actor).iter().any(|a| matches!(a, TransportAction::Send { packets } if *packets == encode_transfer(&read_globals_request().unwrap()).unwrap())));
        actor.sent(Ok(()), Duration::from_millis(20));
        receive(&mut actor, &raw(&entry, "before.payload.bin"), 21);
        assert!(actions(&mut actor).iter().any(|a| matches!(a, TransportAction::Send { packets } if packets.concat() == bytes(&entry, "request.bin"))));
        actor.sent(Ok(()), Duration::from_millis(22));
        let packet = decode_packet(&bytes(&entry, "request.bin")).unwrap();
        receive(
            &mut actor,
            &RawTransfer::from_payload(packet.kind, packet.payload).unwrap(),
            23,
        );
        assert!(!actions(&mut actor).iter().any(|a| matches!(
            a,
            TransportAction::Event(DeviceEvent::EditProgress {
                stage: EditStage::Verified,
                ..
            })
        )));
        actor.tick(Duration::from_millis(70));
        actions(&mut actor);
        actor.sent(Ok(()), Duration::from_millis(70));
        receive(&mut actor, &raw(&entry, "after.payload.bin"), 71);
        assert!(
            actions(&mut actor).iter().any(|a| matches!(
                a,
                TransportAction::Event(DeviceEvent::EditProgress {
                    stage: EditStage::Verified,
                    ..
                })
            )),
            "{}",
            entry["name"]
        );
        assert_eq!(actor.snapshot(PresetTarget::EditBuffer).unwrap(), &current);
    }
}

#[test]
fn breadth_does_not_allow_invalid_values_or_unknown_selectors() {
    let before = state(raw(&entries()[0], "before.payload.bin"), 1);
    for (field, value) in [
        (GeneralSetting::MidiInputSource, 3),
        (GeneralSetting::MidiClockSource, 5),
        (GeneralSetting::MidiInputUsb, -1),
        (GeneralSetting::MidiOutputUsb, -2),
        (GeneralSetting::InputLevel, 21),
    ] {
        assert!(!hardware_global_verified(
            &GlobalControlEdit::General { field, value },
            &before
        ));
    }
    for (parameter, value) in [
        (EqParameter::Band1Enabled, 0.5),
        (EqParameter::Band1Frequency, 0.0),
        (EqParameter::Band1Q, 10.1),
        (EqParameter::Level, f32::NAN),
    ] {
        assert!(!hardware_global_verified(
            &GlobalControlEdit::Eq {
                parameter,
                value: F32Bits::from_f32(value)
            },
            &before
        ));
    }
    assert!(EqParameter::from_index(31).is_err());
}

#[test]
#[cfg(gp200_private_evidence)]
fn gp200_footswitch_menus_match_independent_native_vectors_and_keep_wire_ids() {
    let vectors: Value = serde_json::from_str(include_str!(
        "../../../analysis/implementation/research-globals/footswitch-menu-vectors.json"
    ))
    .unwrap();
    let grid: Vec<_> = vectors["grid_to_wire_event"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u8)
        .collect();
    assert_eq!(
        grid.iter().copied().collect::<BTreeSet<_>>(),
        (0..20).collect()
    );
    for (grid_index, wire_event) in grid.into_iter().enumerate() {
        let name = match grid_index / 4 {
            0 | 2 => "tap",
            1 | 3 => "hold",
            _ => "combined",
        };
        let expected: Vec<_> = vectors[name]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap() as u8)
            .collect();
        assert_eq!(
            gp200_footswitch_choices(GlobalFootswitchEvent::new(wire_event).unwrap()),
            expected
        );
        assert!(!expected.contains(&22));
        assert!(
            expected
                .iter()
                .all(|value| GlobalFootswitchFunction::new(*value).is_ok())
        );
    }
    assert!(!gp200_footswitch_choices(GlobalFootswitchEvent::new(1).unwrap()).contains(&1));
    assert!(!gp200_footswitch_choices(GlobalFootswitchEvent::new(16).unwrap()).contains(&8));
}
