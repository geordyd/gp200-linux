use gp200_domain::*;
use serde_json::json;

fn device(serial: &str) -> FirmwareDeviceAssociation {
    FirmwareDeviceAssociation::usb_serial("GP-200", 0x84ef, 0x002a, serial).unwrap()
}
fn report() -> UserReportedFirmware {
    UserReportedFirmware::new(
        "V1.6.3",
        device("20220118"),
        FirmwareReportDate::new(2026, 10, 11).unwrap(),
    )
    .unwrap()
}

#[test]
fn strict_versions_use_numeric_tuple_order_and_canonical_text() {
    let version: FirmwareVersion = "V1.6.3".parse().unwrap();
    assert_eq!(version.components(), [1, 6, 3]);
    assert_eq!(version.to_string(), "1.6.3");
    assert!("1.10.0".parse::<FirmwareVersion>().unwrap() > "1.8.0".parse().unwrap());
    assert!("2.0.0".parse::<FirmwareVersion>().unwrap() > "1.999.999".parse().unwrap());
    for text in ["0.0.0", "V999.999.999", "1.0.999", "v1.6.3", "v1.8.2"] {
        assert!(text.parse::<FirmwareVersion>().is_ok());
    }
    for text in [
        "",
        "1.6",
        "1.6.3.0",
        " V1.6.3",
        "1.6.3 ",
        "1.6.3beta",
        "01.6.3",
        "1.06.3",
        "1.6.003",
        "1000.0.0",
        "1.+6.3",
        "1.-6.3",
        "１.6.3",
        "1.6.3\n",
        "VV1.6.3",
        "1e0.6.3",
    ] {
        assert!(
            text.parse::<FirmwareVersion>().is_err(),
            "accepted {text:?}"
        );
        assert!(serde_json::from_value::<FirmwareVersion>(json!(text)).is_err());
    }
    assert!(FirmwareVersion::new(1000, 0, 0).is_err());
    assert!(serde_json::from_value::<FirmwareVersion>(json!([1, 6, 3])).is_err());
    assert_eq!(serde_json::to_value(version).unwrap(), json!("1.6.3"));
}

#[test]
fn gregorian_date_validation_survives_deserialization() {
    for (y, m, d) in [(2000, 2, 29), (2024, 2, 29), (1, 1, 1), (9999, 12, 31)] {
        let date = FirmwareReportDate::new(y, m, d).unwrap();
        assert_eq!(date.components(), (y, m, d));
        assert_eq!(
            serde_json::from_value::<FirmwareReportDate>(serde_json::to_value(date).unwrap())
                .unwrap(),
            date
        );
    }
    for (y, m, d) in [
        (1900, 2, 29),
        (2023, 2, 29),
        (2026, 4, 31),
        (2026, 0, 1),
        (2026, 13, 1),
        (2026, 1, 0),
        (0, 1, 1),
        (10000, 1, 1),
    ] {
        assert!(FirmwareReportDate::new(y, m, d).is_err());
        assert!(
            serde_json::from_value::<FirmwareReportDate>(json!({"year": y, "month": m, "day": d}))
                .is_err()
        );
    }
}

#[test]
fn association_preserves_serial_and_does_not_derive_from_hints() {
    let normal = device("ABC123");
    assert_ne!(normal, device("abc123"));
    assert_ne!(normal, device(" ABC123 "));
    assert_ne!(
        normal,
        FirmwareDeviceAssociation::usb_serial("GP-200LT", 0x84ef, 0x002a, "ABC123").unwrap()
    );
    assert_ne!(
        normal,
        FirmwareDeviceAssociation::usb_serial("GP-200", 0x84ef, 0x002b, "ABC123").unwrap()
    );
    assert_ne!(
        normal,
        FirmwareDeviceAssociation::usb_serial("GP-200", 0x84ee, 0x002a, "ABC123").unwrap()
    );
    assert_eq!(normal.usb_identity(), Some((0x84ef, 0x002a, "ABC123")));
    let local = FirmwareDeviceAssociation::named_local_record(
        "GP-200",
        "Studio pedal without unique serial",
    )
    .unwrap();
    assert!(local.usb_identity().is_none());
    assert_eq!(
        local.local_record(),
        Some("Studio pedal without unique serial")
    );
    assert!(
        serde_json::from_value::<FirmwareDeviceAssociation>(
            json!({"kind": "Endpoint", "card": 3, "hint": "GP200"})
        )
        .is_err()
    );
    for invalid in ["", "   ", "one\nother", "one\0other"] {
        assert!(FirmwareDeviceAssociation::usb_serial("GP-200", 0x84ef, 0x002a, invalid).is_err());
        assert!(FirmwareDeviceAssociation::named_local_record("GP-200", invalid).is_err());
    }
    assert!(FirmwareDeviceAssociation::usb_serial("GP-200", 0, 0x002a, "x").is_err());
    assert!(FirmwareDeviceAssociation::usb_serial("GP-200", 1, 1, "x".repeat(129)).is_err());
    assert!(FirmwareDeviceAssociation::named_local_record("x".repeat(65), "named").is_err());
}

#[test]
fn report_roundtrip_is_only_a_manual_assertion_with_optional_suggestion() {
    let report = report();
    assert_eq!(report.raw_input(), "V1.6.3");
    assert_eq!(
        report.provenance(),
        FirmwareReportProvenance::UserEnteredPedalDisplay
    );
    assert_eq!(
        report.suggested_metadata_profile(report.device()),
        Some(EditorMetadataProfile::Gp200Editor160)
    );
    assert_eq!(report.suggested_metadata_profile(&device("another")), None);
    let encoded = serde_json::to_value(&report).unwrap();
    let imported: UserReportedFirmware = serde_json::from_value(encoded).unwrap();
    assert_eq!(imported, report);
    for version in ["1.6.0", "1.6.4", "1.7.4", "1.8.2", "2.0.0"] {
        assert_eq!(
            UserReportedFirmware::new(version, device("20220118"), report.recorded_date())
                .unwrap()
                .suggested_metadata_profile(report.device()),
            None
        );
    }
    let absent: Option<UserReportedFirmware> = serde_json::from_value(json!(null)).unwrap();
    assert!(absent.is_none());
    assert!(
        serde_json::to_value(AppPreferences::default())
            .unwrap()
            .get("reported_firmware")
            .is_none()
    );
}

#[test]
fn forged_reports_revalidate_every_descriptive_field() {
    let original = serde_json::to_value(report()).unwrap();
    let mut bad = original.clone();
    bad["version"] = json!("1.8.2");
    assert!(serde_json::from_value::<UserReportedFirmware>(bad).is_err());
    for raw in [
        "V01.6.3".to_owned(),
        "1.6.3\n".to_owned(),
        "1.6.3".repeat(8),
    ] {
        let mut bad = original.clone();
        bad["raw_input"] = json!(raw);
        assert!(serde_json::from_value::<UserReportedFirmware>(bad).is_err());
    }
    let mut bad = original.clone();
    bad["provenance"] = json!("LiveVerified");
    assert!(serde_json::from_value::<UserReportedFirmware>(bad).is_err());
    let mut bad = original.clone();
    bad["recorded_date"]["month"] = json!(13);
    assert!(serde_json::from_value::<UserReportedFirmware>(bad).is_err());
    let mut bad = original.clone();
    bad["device"]["serial"] = json!("");
    assert!(serde_json::from_value::<UserReportedFirmware>(bad).is_err());
    let mut bad = original;
    bad["automatically_detected"] = json!(true);
    assert!(serde_json::from_value::<UserReportedFirmware>(bad).is_err());
}
