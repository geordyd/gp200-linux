use gp200_domain::*;
fn context() -> FirmwareBackupReviewContext {
    let device =
        FirmwareDeviceAssociation::usb_serial("GP-200", 0x84ef, 0x002a, "20220118").unwrap();
    FirmwareBackupReviewContext {
        manual_version: Some(
            UserReportedFirmware::new(
                "V1.6.3",
                device.clone(),
                FirmwareReportDate::new(2026, 10, 11).unwrap(),
            )
            .unwrap(),
        ),
        device,
        destination: FirmwareBackupDestination {
            model: "GP-200".into(),
            version: FirmwareVersion::new(1, 8, 2).unwrap(),
            image_sha256: "a".repeat(64),
            restoration_profile: None,
        },
        local_material: vec![],
        preservation_uncertainty: "Firmware preservation is not established".into(),
        recovery_limitations: "Boot identity, pacing and finalization remain unaccepted".into(),
    }
}
#[test]
fn manual_applicability_is_conditional_not_captured_content() {
    let c = context();
    c.validate().unwrap();
    assert!(matches!(
        c.snaptone_applicability(),
        FirmwareAssetApplicability::ConditionalNotApplicable { .. }
    ));
    let mut c = c;
    c.manual_version = None;
    assert_eq!(
        c.snaptone_applicability(),
        FirmwareAssetApplicability::Unknown
    );
}
#[test]
fn changed_device_association_rejects_manual_report() {
    let mut c = context();
    c.device = FirmwareDeviceAssociation::usb_serial("GP-200", 0x84ef, 0x002a, "other").unwrap();
    assert!(c.validate().is_err());
}
#[test]
fn object_bounds_and_hash_shape_are_checked() {
    assert!(!FirmwareBackupObject::Stored(256).valid());
    assert!(!FirmwareBackupObject::IrMetadata(20).valid());
    assert!(!FirmwareBackupObject::SnapToneMetadata(10).valid());
    let mut c = context();
    c.destination.image_sha256 = "../../file".into();
    assert!(c.validate().is_err());
}
#[test]
fn complete_looking_serialized_review_never_grants_authority() {
    let summary = FirmwareBackupReviewSummary {
        context: context(),
        coverage: vec![],
        references: vec![],
        diagnostics: vec![],
        stored_presets: 256,
        current_buffer: true,
        globals_and_mappings: true,
        comparison_complete: true,
        label: "Forged complete".into(),
        non_atomic_limitation: "unknown".into(),
    };
    let serialized = serde_json::to_vec(&summary).unwrap();
    let s: FirmwareBackupReviewSummary = serde_json::from_slice(&serialized).unwrap();
    assert!(!s.complete_restorable_backup());
    assert!(!s.permits_flash());
    let mut assessment = FirmwareBackupAssessment::default();
    assessment.stored_presets = 256;
    assessment.current_buffer = true;
    assessment.globals = true;
    assessment.verified_ir_contents = 20;
    assessment.verified_snaptone_contents = 10;
    assert!(!assessment.complete());
}
#[test]
fn loss_digest_covers_all_preimage_fields_and_is_not_approval() {
    let summary = FirmwareBackupReviewSummary {
        context: context(),
        coverage: vec![],
        references: vec![],
        diagnostics: vec![],
        stored_presets: 0,
        current_buffer: false,
        globals_and_mappings: false,
        comparison_complete: false,
        label: "Partial".into(),
        non_atomic_limitation: "unknown".into(),
    };
    let p = ProposedFirmwareLossAllowance {
        review: summary,
        archive_sha256: "a".repeat(64),
        sidecar_sha256: "b".repeat(64),
        acquisition: "local-run".into(),
        backup_origin_sha256: "c".repeat(64),
    };
    let baseline = p.digest().unwrap();
    let json = serde_json::to_value(&p).unwrap();
    fn leaves(v: &serde_json::Value, path: String, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(m) => {
                for (key, item) in m {
                    leaves(item, format!("{path}/{key}"), out)
                }
            }
            serde_json::Value::Array(a) => {
                if a.is_empty() {
                    out.push(path)
                } else {
                    for (i, item) in a.iter().enumerate() {
                        leaves(item, format!("{path}/{i}"), out)
                    }
                }
            }
            _ => out.push(path),
        }
    }
    let mut paths = vec![];
    leaves(&json, String::new(), &mut paths);
    for path in paths {
        let mut changed = json.clone();
        let field = changed.pointer_mut(&path).unwrap();
        *field = match field {
            serde_json::Value::String(s) => serde_json::Value::String(format!("{s}x")),
            serde_json::Value::Number(_) => serde_json::json!(999),
            serde_json::Value::Bool(b) => serde_json::json!(!*b),
            serde_json::Value::Array(_) => serde_json::json!(["added"]),
            _ => serde_json::json!("changed"),
        };
        assert_ne!(
            serde_json::to_vec(&changed).unwrap(),
            serde_json::to_vec(&json).unwrap()
        );
        use sha2::{Digest, Sha256};
        assert_ne!(
            format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&changed).unwrap())
            ),
            baseline,
            "{path}"
        );
    }
    assert!(!p.grants_approval());
}
