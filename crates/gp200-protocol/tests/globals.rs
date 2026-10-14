use gp200_protocol::*;

fn fixture() -> RawTransfer {
    RawTransfer::from_payload(
        WRITE_KIND,
        include_bytes!("../../../tests/fixtures/globals/baseline.payload.bin").to_vec(),
    )
    .unwrap()
}

#[test]
fn connected_read_baseline_is_lossless_and_has_correct_record_roles() {
    let raw = fixture();
    let global = GlobalRecords::from_transfer(raw.clone()).unwrap();
    assert!(global.is_complete());
    assert_eq!(global.raw(), &raw);
    let general = global.general.unwrap();
    assert_eq!(general.body().len(), 72);
    assert_eq!(general.value(GeneralSetting::InputMode), 2);
    assert_eq!(general.value(GeneralSetting::DisplayMode), 0);
    assert_eq!(general.value(GeneralSetting::MidiInputSource), 2);
    assert_eq!(general.value(GeneralSetting::MidiInputUsb), 0);
    assert_eq!(general.value(GeneralSetting::MidiInputDin), 0);
    assert_eq!(general.value(GeneralSetting::MidiOutputDin), 0);
    assert_eq!(general.value(GeneralSetting::MidiClockSource), 4);
    let eq = global.eq.unwrap();
    assert_eq!(eq.values[1].as_f32(), 19.);
    assert_eq!(eq.values[28].as_f32(), 20001.);
    let drum = global.drum.unwrap();
    assert_eq!(drum.value(DrumSetting::Pattern), 0);
    assert_eq!(drum.value(DrumSetting::Tempo), 120);
    assert_eq!(drum.value(DrumSetting::Volume), 80);
    assert_eq!(global.runtime.unwrap().drum_playing(), Some(false));
}

#[test]
fn preset_changed_latch_is_read_only_lossless_and_separate_from_drum_state() {
    for (word, expected) in [
        (0u16, Some(false)),
        (1, Some(true)),
        (2, None),
        (65535, None),
    ] {
        let mut record = RawRecord {
            command: RUNTIME_STATE_COMMAND,
            body: vec![1, 0, 0, 0, 0x34, 0x12],
        };
        record.body[2..4].copy_from_slice(&word.to_le_bytes());
        let runtime = RawGlobalRuntime::from_record(&record).unwrap();
        assert_eq!(runtime.preset_changed(), expected);
        assert_eq!(runtime.drum_playing(), Some(true));
        assert_eq!(runtime.to_record(), record);
    }
}

#[test]
fn selector_encodings_and_global_scope_are_explicit() {
    let query = read_globals_request().unwrap();
    assert_eq!(query.kind(), READ_KIND);
    assert_eq!(query.payload(), &[6, 16, 0, 0]);
    let edit = general_setting_request(GeneralSetting::InputLevel, -12).unwrap();
    assert_eq!(edit.kind(), WRITE_KIND);
    assert_eq!(edit.payload(), &[8, 16, 4, 0, 1, 0, 244, 255]);
    assert_eq!(
        general_setting_request(GeneralSetting::DisplayMode, 2)
            .unwrap()
            .payload(),
        &[8, 16, 4, 0, 40, 0, 2, 0]
    );
    assert_eq!(
        drum_setting_request(DrumSetting::Tempo, 121)
            .unwrap()
            .payload(),
        &[1, 16, 4, 0, 2, 0, 121, 0]
    );
    assert_eq!(
        drum_setting_request(DrumSetting::Volume, 81)
            .unwrap()
            .payload(),
        &[1, 16, 4, 0, 1, 0, 81, 0]
    );
    assert_eq!(
        drum_play_request(true).unwrap().payload(),
        &[17, 16, 4, 0, 0, 0, 1, 0]
    );
    assert_eq!(
        global_eq_request(EqParameter::Band3Gain, 1.)
            .unwrap()
            .payload(),
        &[5, 16, 8, 0, 18, 0, 0, 0, 0, 0, 128, 63]
    );
    for request in [
        edit,
        drum_play_request(false).unwrap(),
        global_eq_request(EqParameter::Level, 50.).unwrap(),
    ] {
        assert_eq!(request.records().len(), 1);
        assert!(request.records().iter().all(|r| r.command != 0));
    }
}

#[test]
fn bounded_changes_preserve_unknown_bytes_and_float_payloads() {
    let global = GlobalRecords::from_transfer(fixture()).unwrap();
    let original = global.general.unwrap();
    let changed = original.with_value(GeneralSetting::DisplayMode, 1).unwrap();
    for i in 0..72 {
        assert_eq!(
            changed.body()[i],
            if i == 57 { 1 } else { original.body()[i] }
        );
    }
    let drum = global.drum.unwrap();
    let changed = drum.with_value(DrumSetting::Tempo, 121).unwrap();
    assert_eq!(&changed.body()[4..], &drum.body()[4..]);
    let mut eq = global.eq.unwrap();
    eq.values[3] = F32Bits(0x7fc01234);
    eq.values[29] = F32Bits(0x80000000);
    let reread = RawGlobalEq::from_record(&eq.to_record()).unwrap();
    assert_eq!(reread, eq);
    let changed = eq.with_value(EqParameter::Level, 51.).unwrap();
    assert_eq!(&changed.values[..30], &eq.values[..30]);
    assert!(general_setting_request(GeneralSetting::DisplayMode, 3).is_err());
    assert!(general_setting_request(GeneralSetting::InputLevel, -21).is_err());
    assert!(general_setting_request(GeneralSetting::MidiInputUsb, -1).is_err());
    assert!(drum_setting_request(DrumSetting::Pattern, 100).is_err());
    assert!(drum_setting_request(DrumSetting::Tempo, 39).is_err());
    assert!(global_eq_request(EqParameter::Band1Enabled, 0.5).is_err());
    assert!(global_eq_request(EqParameter::Band1Gain, f32::NAN).is_err());
    assert!(global_eq_request(EqParameter::Band1Gain, f32::INFINITY).is_err());
    assert!(EqParameter::from_index(31).is_err());
}

#[test]
fn malformed_and_duplicate_singletons_are_not_silently_projected() {
    let mut records = fixture().records().to_vec();
    let general = records
        .iter()
        .find(|r| r.command == GENERAL_SETTINGS_COMMAND)
        .unwrap()
        .clone();
    records.push(general);
    assert_eq!(
        GlobalRecords::from_transfer(RawTransfer::from_records(WRITE_KIND, &records).unwrap())
            .unwrap_err(),
        GlobalError::DuplicateRecord(GENERAL_SETTINGS_COMMAND)
    );
    records = fixture().records().to_vec();
    records
        .iter_mut()
        .find(|r| r.command == DRUM_SETTINGS_COMMAND)
        .unwrap()
        .body
        .pop();
    assert!(matches!(
        GlobalRecords::from_transfer(RawTransfer::from_records(WRITE_KIND, &records).unwrap()),
        Err(GlobalError::InvalidLength {
            command: DRUM_SETTINGS_COMMAND,
            expected: 8,
            actual: 7
        })
    ));
    let global = GlobalRecords::from_transfer(fixture()).unwrap();
    assert_eq!(
        global
            .raw()
            .records()
            .iter()
            .filter(|r| r.command == 0x100e)
            .count(),
        3
    );
    assert_eq!(
        global
            .raw()
            .records()
            .iter()
            .filter(|r| r.command == 0x000a)
            .count(),
        20
    );
}
