//! Root captures are independent of the Rust codec and simulator.
#![cfg(gp200_private_evidence)]

use gp200_domain::*;
use gp200_protocol::*;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fs, path::PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}
fn manifest() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/globals/footswitch-storage/manifest.json"
    ))
    .unwrap()
}
fn checked(file: &Value) -> Vec<u8> {
    let bytes = fs::read(root().join(file["path"].as_str().unwrap())).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(&bytes)),
        file["sha256"].as_str().unwrap()
    );
    bytes
}
fn state(bytes: Vec<u8>, revision: u64) -> DeviceGlobalState {
    DeviceGlobalState::from_transfer(
        Revision(revision),
        SessionEpoch(1),
        RawTransfer::from_payload(WRITE_KIND, bytes).unwrap(),
        vec![],
    )
    .unwrap()
}
fn edit(table: u16, event: u8, function: u8) -> GlobalControlEdit {
    GlobalControlEdit::Footswitch {
        table: GlobalFootswitchTableId::new(table).unwrap(),
        event: GlobalFootswitchEvent::new(event).unwrap(),
        function: GlobalFootswitchFunction::new(function).unwrap(),
    }
}
fn change(before: &DeviceGlobalState, patch: &GlobalControlEdit) -> DeviceGlobalState {
    state(
        before.expected_after(patch).unwrap().payload().to_vec(),
        before.revision.0,
    )
}

#[test]
fn independently_captured_requests_and_complete_readbacks_cover_the_promoted_samples() {
    let manifest = manifest();
    checked(&manifest["source_acceptance"]);
    let mut events = BTreeSet::new();
    let mut tap = BTreeSet::new();
    let mut hold = BTreeSet::new();
    let mut combined = BTreeSet::new();
    let mut tables = BTreeSet::new();
    let mut meaningful = 0;
    let mut noops = 0;
    for entry in manifest["entries"].as_array().unwrap() {
        checked(&entry["result"]);
        let before = state(checked(&entry["files"]["before"]), 1);
        let after = state(checked(&entry["files"]["after"]), 2);
        let restored = state(checked(&entry["files"]["restored"]), 3);
        let table = entry["table"].as_u64().unwrap() as u16;
        let event = entry["event"].as_u64().unwrap() as u8;
        let function = entry["requested"].as_u64().unwrap() as u8;
        let original = entry["original"].as_u64().unwrap() as u8;
        let patch = edit(table, event, function);
        let validated = ValidatedInactiveFootswitchEdit::validate(&patch, &before).unwrap();
        validated.revalidate(&before).unwrap();
        assert_eq!(validated.edit(), &patch);
        assert_eq!(
            encode_transfer(&patch.request().unwrap()).unwrap().concat(),
            checked(&entry["files"]["request"])
        );
        assert!(
            before.verifies_expected(&patch, &after).unwrap(),
            "{}",
            entry["index"]
        );
        let restore = edit(table, event, original);
        assert!(ValidatedInactiveFootswitchEdit::validate(&restore, &after).is_ok());
        assert_eq!(
            encode_transfer(&restore.request().unwrap())
                .unwrap()
                .concat(),
            checked(&entry["files"]["restore_request"])
        );
        assert!(before.records().control_state_equal(restored.records()));
        tables.insert(table);
        if table == 2 {
            events.insert(event);
        }
        if (20..46).contains(&entry["index"].as_u64().unwrap()) {
            tap.insert(function);
        }
        if (46..63).contains(&entry["index"].as_u64().unwrap()) {
            hold.insert(function);
        }
        if (63..88).contains(&entry["index"].as_u64().unwrap()) {
            combined.insert(function);
        }
        if function != original {
            meaningful += 1;
        } else {
            noops += 1;
        }
        assert_eq!(entry["meaningful_delta"], function != original);
    }
    assert_eq!(events, (0..20).collect());
    assert_eq!(tables, (0..3).collect());
    for (event, values) in [(0, tap), (1, hold), (16, combined)] {
        assert_eq!(
            values,
            gp200_footswitch_choices(GlobalFootswitchEvent::new(event).unwrap())
                .iter()
                .copied()
                .collect()
        );
    }
    assert_eq!((meaningful, noops), (87, 3));
}

#[test]
fn inactive_validation_rejects_unknown_mode_active_table_menu_and_changed_baselines() {
    let m = manifest();
    let before = state(checked(&m["entries"][0]["files"]["before"]), 1);
    let active = before
        .records()
        .general
        .as_ref()
        .unwrap()
        .value(GeneralSetting::FootswitchMode) as u16;
    // The native template/style word is independent of the active switch mode.
    // A template value of 1 must not make Stomp table1 appear active when the
    // actual mode is Preset0.
    assert_eq!(active, 0);
    assert_eq!(
        before
            .records()
            .general
            .as_ref()
            .unwrap()
            .value(GeneralSetting::FootswitchTemplate),
        1
    );
    assert!(ValidatedInactiveFootswitchEdit::validate(&edit(1, 0, 17), &before).is_ok());
    assert_eq!(
        ValidatedInactiveFootswitchEdit::validate(&edit(active, 0, 17), &before).unwrap_err(),
        InactiveFootswitchError::ActiveTable
    );
    for (event, function) in [(0, 22), (1, 1), (1, 8), (1, 23), (16, 8), (19, 22)] {
        assert_eq!(
            ValidatedInactiveFootswitchEdit::validate(&edit(2, event, function), &before)
                .unwrap_err(),
            InactiveFootswitchError::UnsupportedFunction
        );
    }
    let unsupported = change(&before, &edit(2, 1, 22));
    assert_eq!(
        ValidatedInactiveFootswitchEdit::validate(&edit(2, 1, 5), &unsupported).unwrap_err(),
        InactiveFootswitchError::UnsupportedBaseline
    );
    let valid = ValidatedInactiveFootswitchEdit::validate(&edit(2, 1, 5), &before).unwrap();
    let mut stale = before.clone();
    stale.epoch = SessionEpoch(2);
    assert_eq!(
        valid.revalidate(&stale),
        Err(InactiveFootswitchError::Stale)
    );
    stale = before.clone();
    stale.revision = Revision(2);
    assert_eq!(
        valid.revalidate(&stale),
        Err(InactiveFootswitchError::Stale)
    );
    let changed = change(
        &before,
        &GlobalControlEdit::General {
            field: GeneralSetting::DisplayMode,
            value: 1,
        },
    );
    assert_eq!(
        valid.revalidate(&changed),
        Err(InactiveFootswitchError::Stale)
    );
    let mut records = before.records().raw().records().to_vec();
    records
        .iter_mut()
        .find(|r| r.command == GENERAL_SETTINGS_COMMAND)
        .unwrap()
        .body[GeneralSetting::FootswitchMode.body_offset()] = 99;
    let unknown = state(
        RawTransfer::from_records(WRITE_KIND, &records)
            .unwrap()
            .payload()
            .to_vec(),
        1,
    );
    assert_eq!(
        ValidatedInactiveFootswitchEdit::validate(&edit(2, 1, 5), &unknown).unwrap_err(),
        InactiveFootswitchError::UnknownMode
    );
}
