use gp200_domain::*;
use gp200_protocol::*;

fn raw() -> RawTransfer {
    RawTransfer::from_payload(
        WRITE_KIND,
        include_bytes!("../../../tests/fixtures/globals/baseline.payload.bin").to_vec(),
    )
    .unwrap()
}
fn state(raw: RawTransfer) -> DeviceGlobalState {
    DeviceGlobalState::from_transfer(
        Revision(7),
        SessionEpoch(3),
        raw,
        vec![EvidenceRef::new(
            "captured-global-baseline",
            EvidenceLevel::LiveVerified,
        )],
    )
    .unwrap()
}

#[test]
fn serde_rebuilds_authoritative_raw_and_rejects_short_or_conflicting_views() {
    let before = state(raw());
    let json = serde_json::to_value(&before).unwrap();
    assert_eq!(
        serde_json::from_value::<DeviceGlobalState>(json.clone()).unwrap(),
        before
    );
    assert!(json["records"].get("general").is_none());
    let mut inconsistent = json;
    inconsistent["records"]["general"] = serde_json::json!([0]);
    assert!(serde_json::from_value::<DeviceGlobalState>(inconsistent).is_err());
    for length in [0, 1, 57, 71, 73] {
        assert!(
            serde_json::from_value::<RawGeneralSettings>(serde_json::json!(vec![0u8; length]))
                .is_err()
        );
    }
    let baseline = GlobalRecords::from_transfer(raw()).unwrap();
    let valid = serde_json::to_value(baseline.general.unwrap()).unwrap();
    assert_eq!(
        serde_json::from_value::<RawGeneralSettings>(valid)
            .unwrap()
            .body()
            .len(),
        72
    );
    assert!(serde_json::from_value::<GlobalFootswitchEvent>(serde_json::json!(20)).is_err());
    assert!(serde_json::from_value::<GlobalFootswitchTableId>(serde_json::json!(3)).is_err());
    assert!(serde_json::from_value::<GlobalFootswitchFunction>(serde_json::json!(27)).is_err());
}

#[test]
fn pure_deltas_preserve_every_other_record_and_do_not_advance_evidence() {
    let before = state(raw());
    let saved = before.clone();
    let edit = GlobalControlEdit::General {
        field: GeneralSetting::DisplayMode,
        value: 1,
    };
    let after = before.expected_after(&edit).unwrap();
    let deltas: Vec<_> =
        before
            .records()
            .raw()
            .records()
            .iter()
            .zip(after.records())
            .flat_map(|(a, b)| {
                a.body.iter().zip(&b.body).enumerate().filter_map(
                    move |(offset, (a_byte, b_byte))| {
                        (a_byte != b_byte).then_some((a.command, offset))
                    },
                )
            })
            .collect();
    assert_eq!(deltas, [(GENERAL_SETTINGS_COMMAND, 57)]);
    assert_eq!(before, saved);
    let readback = state(after);
    assert!(before.verifies_expected(&edit, &readback).unwrap());
    assert!(!before.verifies_expected(&edit, &before).unwrap());
    let mut wrong_epoch = readback;
    wrong_epoch.epoch = SessionEpoch(4);
    assert!(!before.verifies_expected(&edit, &wrong_epoch).unwrap());
}

#[test]
fn global_footswitch_target_changes_one_event_in_one_template() {
    let before = state(raw());
    let edit = GlobalControlEdit::Footswitch {
        table: GlobalFootswitchTableId::new(1).unwrap(),
        event: GlobalFootswitchEvent::new(19).unwrap(),
        function: GlobalFootswitchFunction::new(0).unwrap(),
    };
    assert_eq!(
        edit.request().unwrap().payload(),
        &[15, 16, 4, 0, 1, 0, 19, 0]
    );
    let after = before.expected_after(&edit).unwrap();
    let diffs: Vec<_> = before
        .records()
        .raw()
        .records()
        .iter()
        .zip(after.records())
        .enumerate()
        .flat_map(|(index, (a, b))| {
            a.body
                .iter()
                .zip(&b.body)
                .enumerate()
                .filter_map(move |(offset, (x, y))| (x != y).then_some((index, offset)))
        })
        .collect();
    let table_index = after
        .records()
        .iter()
        .position(|r| r.command == GLOBAL_FOOTSWITCH_COMMAND && r.body[..2] == [1, 0])
        .unwrap();
    assert_eq!(diffs, [(table_index, 39)]);
    assert!(before.verifies_expected(&edit, &state(after)).unwrap());
    assert_eq!(GlobalFootswitchFunction::new(10).unwrap().label(), "Bank-");
    assert_eq!(GlobalFootswitchFunction::new(26).unwrap().label(), "CTRL 8");
}

#[test]
fn observed_read_only_volatility_is_narrow_and_unknown_records_are_strict() {
    let baseline = GlobalRecords::from_transfer(raw()).unwrap();
    for fixture in [
        include_bytes!("../../../tests/fixtures/globals/repeat.payload.bin").as_slice(),
        include_bytes!("../../../tests/fixtures/globals/repeat-2.payload.bin").as_slice(),
        include_bytes!("../../../tests/fixtures/globals/repeat-3.payload.bin").as_slice(),
        include_bytes!("../../../tests/fixtures/globals/repeat-4.payload.bin").as_slice(),
    ] {
        let repeat = GlobalRecords::from_transfer(
            RawTransfer::from_payload(WRITE_KIND, fixture.to_vec()).unwrap(),
        )
        .unwrap();
        assert_ne!(baseline.raw(), repeat.raw());
        assert!(baseline.control_state_equal(&repeat));
    }
    let mut records = raw().records().to_vec();
    records
        .iter_mut()
        .find(|r| r.command == 0x1010)
        .unwrap()
        .body[3] ^= 1;
    let unknown_changed =
        GlobalRecords::from_transfer(RawTransfer::from_records(WRITE_KIND, &records).unwrap())
            .unwrap();
    assert!(!baseline.control_state_equal(&unknown_changed));
    records = raw().records().to_vec();
    records
        .iter_mut()
        .find(|r| r.command == GENERAL_SETTINGS_COMMAND)
        .unwrap()
        .body[53] ^= 1;
    assert!(
        !baseline.control_state_equal(
            &GlobalRecords::from_transfer(RawTransfer::from_records(WRITE_KIND, &records).unwrap())
                .unwrap()
        )
    );
    records = raw().records().to_vec();
    records.swap(0, 1);
    assert!(
        !baseline.control_state_equal(
            &GlobalRecords::from_transfer(RawTransfer::from_records(WRITE_KIND, &records).unwrap())
                .unwrap()
        )
    );
}

#[test]
fn incomplete_and_duplicate_global_state_cannot_become_a_device_baseline() {
    let mut records = raw().records().to_vec();
    records.retain(|r| r.command != RUNTIME_STATE_COMMAND);
    assert!(matches!(
        DeviceGlobalState::from_transfer(
            Revision(1),
            SessionEpoch(1),
            RawTransfer::from_records(WRITE_KIND, &records).unwrap(),
            vec![]
        ),
        Err(GlobalError::MissingBaseline)
    ));
    records = raw().records().to_vec();
    records.push(
        records
            .iter()
            .find(|r| r.command == GLOBAL_FOOTSWITCH_COMMAND)
            .unwrap()
            .clone(),
    );
    assert!(matches!(
        GlobalRecords::from_transfer(RawTransfer::from_records(WRITE_KIND, &records).unwrap()),
        Err(GlobalError::DuplicateFootswitchTable(0))
    ));
    assert_eq!(
        DRUM_PATTERN_GROUPS.iter().map(|g| g.count).sum::<u16>(),
        100
    );
    assert_eq!(DRUM_PATTERN_NAMES.len(), 100);
}
