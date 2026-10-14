use gp200_domain::*;
use gp200_library::*;
use gp200_protocol::{AssetBank, GlobalRecords};
use std::{
    fs::{self, File},
    io::Read,
    path::PathBuf,
};
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "fw04-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn origin() -> BackupOrigin {
    BackupOrigin {
        identity: DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Hardware,
            firmware: None,
            display_name: "GP-200".into(),
        },
        epoch: SessionEpoch(7),
        device_key: "usb-serial-20220118".into(),
        evidence: vec![EvidenceRef::new(
            "Synthetic archival claims, not hardware",
            EvidenceLevel::StaticRecovered,
        )],
    }
}
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
        local_material: vec![FirmwareBackupLocalMaterial {
            object: FirmwareBackupObject::IrMetadata(19),
            path: "retained/Linux IR test.coefficients".into(),
            sha256: "b".repeat(64),
            bytes: 4096,
            format: "Raw coefficients; not original WAV".into(),
            provenance: "Prior one-upload local source; installed correlation unresolved".into(),
        }],
        preservation_uncertainty: "Preservation unknown".into(),
        recovery_limitations: "Recovery profile unavailable".into(),
    }
}
fn boundary(seq: u64) -> FirmwareArchiveBoundary {
    FirmwareArchiveBoundary {
        epoch: SessionEpoch(7),
        identity: origin().identity,
        device_key: origin().device_key,
        sequence: seq,
        monotonic_millis: seq,
        wall_clock_millis: seq,
        raw_identity: vec![1, 0],
    }
}
/// Independent fixture edits TLVs directly; no production stored-write or archive helper.
fn preset(index: Option<u16>) -> Vec<u8> {
    let mut raw = RawTransfer::from_payload(
        0x12,
        include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
    )
    .unwrap()
    .records()
    .to_vec();
    if let Some(i) = index {
        let c = raw.iter_mut().find(|r| r.command == 0).unwrap();
        c.body[..2].copy_from_slice(&1u16.to_le_bytes());
        c.body[2..4].copy_from_slice(&i.to_le_bytes());
        for command in [2, 8] {
            raw.iter_mut().find(|r| r.command == command).unwrap().body[..2]
                .copy_from_slice(&i.to_le_bytes());
        }
    }
    let module = raw
        .iter_mut()
        .find(|r| r.command == 0x14 && r.body[0] == 3)
        .unwrap();
    module.body[60..64].copy_from_slice(&0x7fc1_2345u32.to_le_bytes());
    module.body[64..68].copy_from_slice(&0x8000_0000u32.to_le_bytes());
    raw.push(RawRecord {
        command: 0x6666,
        body: vec![0xff, 0x80, 0, 99],
    });
    let raw = RawTransfer::from_records(0x12, &raw).unwrap();
    [vec![0x12], raw.payload().to_vec()].concat()
}
fn globals() -> Vec<u8> {
    [
        vec![0x12],
        include_bytes!("../../../tests/fixtures/globals/baseline.payload.bin").to_vec(),
    ]
    .concat()
}
fn metadata(i: u8) -> Vec<u8> {
    let mut body = vec![0; 24];
    body[..2].copy_from_slice(&u16::from(i).to_le_bytes());
    body[8..15].copy_from_slice(b"User IR");
    serde_json::to_vec(&RawRecord {
        command: AssetBank::Ir.metadata_command(),
        body,
    })
    .unwrap()
}
fn observation(
    key: FirmwareBackupObject,
    pass: FirmwareObservationPass,
    seq: u64,
) -> FirmwareArchiveObservation {
    let bytes = match key {
        FirmwareBackupObject::Stored(i) => preset(Some(i)),
        FirmwareBackupObject::Current => preset(None),
        FirmwareBackupObject::Globals => globals(),
        FirmwareBackupObject::IrMetadata(i) => metadata(i),
        _ => unreachable!(),
    };
    FirmwareArchiveObservation {
        object: key,
        pass,
        epoch: SessionEpoch(7),
        revision: Revision(seq),
        sequence: seq,
        monotonic_millis: seq,
        wall_clock_millis: seq,
        bytes,
    }
}
fn keys() -> Vec<FirmwareBackupObject> {
    (0..256)
        .map(FirmwareBackupObject::Stored)
        .chain([FirmwareBackupObject::Current, FirmwareBackupObject::Globals])
        .chain((0..20).map(FirmwareBackupObject::IrMetadata))
        .collect()
}
fn full(store: &mut LibraryStore) -> FirmwareBackupReviewDraft {
    let mut d = store
        .begin_firmware_backup_review(origin(), context(), boundary(1))
        .unwrap();
    let mut seq = 2;
    for pass in [
        FirmwareObservationPass::Baseline,
        FirmwareObservationPass::Comparison,
    ] {
        for key in keys() {
            store
                .store_firmware_backup_observation(&mut d, observation(key, pass, seq))
                .unwrap();
            seq += 1;
        }
    }
    store
        .close_firmware_backup_review(&mut d, boundary(seq), BackupAcquisition::Finished)
        .unwrap();
    d
}
#[test]
fn independent_full_bank_archive_preserves_raw_bits_unknowns_and_honest_gaps() {
    let t = Temp::new();
    let mut s = LibraryStore::open(&t.0).unwrap();
    let d = full(&mut s);
    let r = s.publish_firmware_backup_review(d).unwrap();
    r.revalidate().unwrap();
    assert_eq!(r.summary().stored_presets, 256);
    assert!(r.summary().comparison_complete);
    assert!(!r.summary().complete_restorable_backup());
    assert_eq!(r.summary().coverage.len(), 288);
    assert!(
        r.summary()
            .coverage
            .iter()
            .all(|c| !c.installed_content_available)
    );
    assert_eq!(r.summary().references.len(), 257 * 11);
    assert_eq!(
        r.summary()
            .coverage
            .iter()
            .filter(
                |c| matches!(c.object, FirmwareBackupObject::SnapToneMetadata(_))
                    && matches!(
                        c.applicability,
                        FirmwareAssetApplicability::ConditionalNotApplicable { .. }
                    )
            )
            .count(),
        10
    );
    let mut legacy =
        LibraryStore::preview_device_backup(r.archive_path(), BundleLimits::default()).unwrap();
    for kind in [DeviceAssetKind::Ir, DeviceAssetKind::SnapTone] {
        for index in 0..kind.slots() {
            let slot = DeviceAssetSlot::new(kind, index).unwrap();
            let key = format!(
                "{}-{index:02}",
                if kind == DeviceAssetKind::Ir {
                    "ir"
                } else {
                    "snaptone"
                }
            );
            let asset = legacy.assets.entry(key).or_insert(CapturedDeviceAsset {
                slot,
                metadata_hash: ContentHash::of(b"caller metadata"),
                name: "caller complete claim".into(),
                content: None,
            });
            asset.content = Some(CapturedAssetContent {
                hash: ContentHash::of(b"caller content"),
                bytes: if kind == DeviceAssetKind::Ir {
                    4096
                } else {
                    8192
                },
                format: "caller claimed device-ready bytes".into(),
                origin: AssetContentOrigin::VerifiedLocalMatch {
                    evidence: vec![EvidenceRef::new(
                        "caller label with no physical issuer",
                        EvidenceLevel::LiveVerified,
                    )],
                },
            });
        }
    }
    assert!(legacy.completeness().complete_device_state);
    let imported_claim: DeviceBackup =
        serde_json::from_slice(&serde_json::to_vec(&legacy).unwrap()).unwrap();
    assert!(imported_claim.completeness().complete_device_state);
    // A serializable complete-looking legacy assertion cannot alter the sealed local review.
    assert!(!r.summary().complete_restorable_backup());
    r.revalidate().unwrap();
    let proposal = r.proposal();
    assert!(!proposal.grants_approval());
    assert_eq!(proposal.review.context.local_material.len(), 1);
    let mut zip = zip::ZipArchive::new(File::open(r.archive_path()).unwrap()).unwrap();
    let mut text = String::new();
    zip.by_name("manifest.json")
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    let manifest: serde_json::Value = serde_json::from_str(&text).unwrap();
    for i in 0..256 {
        let hash = manifest["backup"]["presets"][i.to_string()]["raw_hash"]
            .as_str()
            .unwrap();
        let mut bytes = vec![];
        zip.by_name(&format!("blobs/{hash}"))
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, preset(Some(i)));
        let raw = RawTransfer::from_payload(bytes[0], bytes[1..].to_vec()).unwrap();
        let m = raw
            .records()
            .iter()
            .find(|r| r.command == 0x14 && r.body[0] == 3)
            .unwrap();
        assert_eq!(&m.body[60..64], &0x7fc1_2345u32.to_le_bytes());
        assert_eq!(&m.body[64..68], &0x8000_0000u32.to_le_bytes());
        assert_eq!(raw.records().last().unwrap().command, 0x6666);
    }
    let summary = reopen_firmware_backup_review(r.archive_path(), r.sidecar_path()).unwrap();
    assert_eq!(&summary, r.summary());
    assert!(!summary.permits_flash());
}
#[test]
fn missing_slot255_and_current_never_count_as_full_bank() {
    let t = Temp::new();
    let mut s = LibraryStore::open(&t.0).unwrap();
    let mut d = s
        .begin_firmware_backup_review(origin(), context(), boundary(1))
        .unwrap();
    let mut seq = 2;
    for pass in [
        FirmwareObservationPass::Baseline,
        FirmwareObservationPass::Comparison,
    ] {
        for key in keys()
            .into_iter()
            .filter(|k| *k != FirmwareBackupObject::Stored(255))
        {
            s.store_firmware_backup_observation(&mut d, observation(key, pass, seq))
                .unwrap();
            seq += 1;
        }
    }
    assert!(
        s.close_firmware_backup_review(&mut d, boundary(seq), BackupAcquisition::Finished)
            .is_err()
    );
    s.close_firmware_backup_review(&mut d, boundary(seq), BackupAcquisition::Cancelled)
        .unwrap();
    let r = s.publish_firmware_backup_review(d).unwrap();
    assert_eq!(r.summary().stored_presets, 255);
    assert!(!r.summary().comparison_complete);
    assert!(r.summary().current_buffer);
}
#[test]
fn duplicates_wrong_context_epoch_kind_and_incomplete_structure_reject_before_commit() {
    let t = Temp::new();
    let mut s = LibraryStore::open(&t.0).unwrap();
    let mut d = s
        .begin_firmware_backup_review(origin(), context(), boundary(1))
        .unwrap();
    let good = observation(
        FirmwareBackupObject::Stored(0),
        FirmwareObservationPass::Baseline,
        2,
    );
    let mut bad = good.clone();
    bad.epoch = SessionEpoch(8);
    assert!(s.store_firmware_backup_observation(&mut d, bad).is_err());
    let mut bad = good.clone();
    bad.bytes = preset(Some(1));
    assert!(s.store_firmware_backup_observation(&mut d, bad).is_err());
    let mut bad = good.clone();
    bad.bytes[0] = 0x13;
    assert!(s.store_firmware_backup_observation(&mut d, bad).is_err());
    for cmd in [0x14, 0x1a] {
        let mut records = RawTransfer::from_payload(0x12, good.bytes[1..].to_vec())
            .unwrap()
            .records()
            .to_vec();
        let remove = records
            .iter()
            .position(|r| r.command == cmd)
            .unwrap_or_else(|| {
                records
                    .iter()
                    .position(|r| {
                        r.command != 0
                            && r.command != 2
                            && r.command != 8
                            && r.command != 0x14
                            && r.command != 0x6666
                    })
                    .unwrap()
            });
        records.remove(remove);
        let raw = RawTransfer::from_records(0x12, &records).unwrap();
        let mut bad = good.clone();
        bad.bytes = [vec![0x12], raw.payload().to_vec()].concat();
        assert!(s.store_firmware_backup_observation(&mut d, bad).is_err());
    }
    s.store_firmware_backup_observation(&mut d, good.clone())
        .unwrap();
    let mut duplicate = good;
    duplicate.sequence = 3;
    assert!(
        s.store_firmware_backup_observation(&mut d, duplicate)
            .is_err()
    );
}
#[test]
fn mapping_coverage_is_required_in_addition_to_general_globals() {
    let t = Temp::new();
    let mut s = LibraryStore::open(&t.0).unwrap();
    let mut d = s
        .begin_firmware_backup_review(origin(), context(), boundary(1))
        .unwrap();
    let mut o = observation(
        FirmwareBackupObject::Globals,
        FirmwareObservationPass::Baseline,
        2,
    );
    let raw = RawTransfer::from_payload(0x12, o.bytes[1..].to_vec()).unwrap();
    let g = GlobalRecords::from_transfer(raw.clone()).unwrap();
    assert_eq!(g.footswitch_tables.len(), 3);
    let table = g.footswitch_tables[0].to_record();
    let records = raw
        .records()
        .iter()
        .filter(|r| **r != table)
        .cloned()
        .collect::<Vec<_>>();
    let r = RawTransfer::from_records(0x12, &records).unwrap();
    o.bytes = [vec![0x12], r.payload().to_vec()].concat();
    assert!(s.store_firmware_backup_observation(&mut d, o).is_err());
}
#[test]
fn changed_comparison_and_mutated_package_remain_blocked() {
    let t = Temp::new();
    let mut s = LibraryStore::open(&t.0).unwrap();
    let mut d = s
        .begin_firmware_backup_review(origin(), context(), boundary(1))
        .unwrap();
    s.store_firmware_backup_observation(
        &mut d,
        observation(
            FirmwareBackupObject::Current,
            FirmwareObservationPass::Baseline,
            2,
        ),
    )
    .unwrap();
    let mut comparison = observation(
        FirmwareBackupObject::Current,
        FirmwareObservationPass::Comparison,
        3,
    );
    *comparison.bytes.last_mut().unwrap() ^= 1;
    s.store_firmware_backup_observation(&mut d, comparison)
        .unwrap();
    s.close_firmware_backup_review(&mut d, boundary(4), BackupAcquisition::Cancelled)
        .unwrap();
    let r = s.publish_firmware_backup_review(d).unwrap();
    assert_eq!(r.summary().diagnostics.len(), 1);
    let mut side: serde_json::Value =
        serde_json::from_slice(&fs::read(r.sidecar_path()).unwrap()).unwrap();
    side["observations"][1]["bytes"][1] = serde_json::json!(255);
    fs::write(r.sidecar_path(), serde_json::to_vec(&side).unwrap()).unwrap();
    assert!(r.revalidate().is_err());
    assert!(reopen_firmware_backup_review(r.archive_path(), r.sidecar_path()).is_err());
}
#[test]
fn imported_legacy_evidence_does_not_enter_updater_receipt() {
    let t = Temp::new();
    let mut s = LibraryStore::open(&t.0).unwrap();
    let mut d = s
        .begin_firmware_backup_review(origin(), context(), boundary(1))
        .unwrap();
    s.close_firmware_backup_review(&mut d, boundary(2), BackupAcquisition::Cancelled)
        .unwrap();
    let r = s.publish_firmware_backup_review(d).unwrap();
    let imported = s.import_device_backup(r.archive_path()).unwrap();
    assert_ne!(imported.id, r.acquisition());
    let summary = reopen_firmware_backup_review(r.archive_path(), r.sidecar_path()).unwrap();
    assert!(!summary.complete_restorable_backup());
    assert!(
        summary
            .coverage
            .iter()
            .filter(|v| matches!(v.object, FirmwareBackupObject::IrMetadata(_)))
            .all(|v| !v.installed_content_available)
    );
}
fn zip_entries(path: &std::path::Path) -> Vec<(String, Vec<u8>, u32)> {
    let mut archive = zip::ZipArchive::new(File::open(path).unwrap()).unwrap();
    (0..archive.len())
        .map(|i| {
            let mut entry = archive.by_index(i).unwrap();
            let name = entry.name().to_owned();
            let mode = entry.unix_mode().unwrap_or(0o600);
            let mut bytes = vec![];
            entry.read_to_end(&mut bytes).unwrap();
            (name, bytes, mode)
        })
        .collect()
}
fn rewrite_zip(path: &std::path::Path, entries: &[(String, Vec<u8>, u32)]) {
    use std::io::Write;
    let mut writer = zip::ZipWriter::new(File::create(path).unwrap());
    for (name, bytes, mode) in entries {
        writer
            .start_file(
                name,
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated)
                    .unix_permissions(*mode),
            )
            .unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap();
}
#[test]
fn independently_rebuilt_bad_archives_fail_parser_checks() {
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    let mut d = store
        .begin_firmware_backup_review(origin(), context(), boundary(1))
        .unwrap();
    store
        .store_firmware_backup_observation(
            &mut d,
            observation(
                FirmwareBackupObject::Current,
                FirmwareObservationPass::Baseline,
                2,
            ),
        )
        .unwrap();
    store
        .close_firmware_backup_review(&mut d, boundary(3), BackupAcquisition::Cancelled)
        .unwrap();
    let r = store.publish_firmware_backup_review(d).unwrap();
    let entries = zip_entries(r.archive_path());
    for case in [
        "schema",
        "missing",
        "unreferenced",
        "hash",
        "traversal",
        "oversize",
        "projection",
    ] {
        let mut bad = entries.clone();
        match case {
            "schema" => {
                let v = &mut bad.iter_mut().find(|v| v.0 == "manifest.json").unwrap().1;
                let mut json: serde_json::Value = serde_json::from_slice(v).unwrap();
                json["schema_version"] = serde_json::json!(999);
                *v = serde_json::to_vec(&json).unwrap();
            }
            "missing" => {
                let i = bad.iter().position(|v| v.0.starts_with("blobs/")).unwrap();
                bad.remove(i);
            }
            "unreferenced" => bad.push((format!("blobs/{}", "f".repeat(64)), vec![1], 0o600)),
            "hash" => {
                let v = &mut bad
                    .iter_mut()
                    .find(|v| v.0.starts_with("blobs/"))
                    .unwrap()
                    .1;
                *v.last_mut().unwrap() ^= 1;
            }
            "traversal" => bad.push(("../outside".into(), vec![1], 0o600)),
            "oversize" => bad.push(("large".into(), vec![1; 256 * 1024 + 1], 0o600)),
            "projection" => {
                let v = &mut bad.iter_mut().find(|v| v.0 == "manifest.json").unwrap().1;
                let mut json: serde_json::Value = serde_json::from_slice(v).unwrap();
                json["backup"]["edit_buffer"]["slot"] = serde_json::json!(99);
                *v = serde_json::to_vec(&json).unwrap();
            }
            _ => unreachable!(),
        }
        let path = t.0.join(format!("bad-{case}.gp200backup"));
        rewrite_zip(&path, &bad);
        let error = reopen_firmware_backup_review(&path, r.sidecar_path()).unwrap_err();
        assert!(
            !error
                .message
                .contains("Sidecar/archive descriptor or file pins disagree"),
            "{case} should fail actual archive validation first: {error}"
        );
    }
}
#[test]
fn altered_archive_after_review_and_unsupported_sidecar_require_new_review() {
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    let mut d = store
        .begin_firmware_backup_review(origin(), context(), boundary(1))
        .unwrap();
    store
        .close_firmware_backup_review(&mut d, boundary(2), BackupAcquisition::Cancelled)
        .unwrap();
    let r = store.publish_firmware_backup_review(d).unwrap();
    let original = fs::read(r.archive_path()).unwrap();
    let mut changed = original.clone();
    changed.push(1);
    fs::write(r.archive_path(), changed).unwrap();
    assert!(r.revalidate().is_err());
    fs::write(r.archive_path(), original).unwrap();
    let mut side: serde_json::Value =
        serde_json::from_slice(&fs::read(r.sidecar_path()).unwrap()).unwrap();
    side["schema"] = serde_json::json!(9);
    fs::write(r.sidecar_path(), serde_json::to_vec(&side).unwrap()).unwrap();
    assert!(r.revalidate().is_err());
}

#[test]
fn duplicate_zip_names_and_symbolic_entries_reject_before_pins() {
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    let mut d = store
        .begin_firmware_backup_review(origin(), context(), boundary(1))
        .unwrap();
    store
        .close_firmware_backup_review(&mut d, boundary(2), BackupAcquisition::Cancelled)
        .unwrap();
    let r = store.publish_firmware_backup_review(d).unwrap();
    let mut entries = zip_entries(r.archive_path());
    entries.push(("duplicate.bin".into(), vec![1], 0o600));
    let path = t.0.join("duplicate.gp200backup");
    rewrite_zip(&path, &entries);
    let bytes = fs::read(&path).unwrap();
    assert_eq!(b"duplicate.bin".len(), b"manifest.json".len());
    let mut bytes = bytes;
    let old = b"duplicate.bin";
    let new = b"manifest.json";
    let positions = bytes
        .windows(old.len())
        .enumerate()
        .filter_map(|(i, b)| (b == old).then_some(i))
        .collect::<Vec<_>>();
    assert_eq!(positions.len(), 2);
    for i in positions {
        bytes[i..i + new.len()].copy_from_slice(new);
    }
    fs::write(&path, bytes).unwrap();
    let error = reopen_firmware_backup_review(&path, r.sidecar_path()).unwrap_err();
    assert!(!error.message.contains("pins disagree"));
    let path = t.0.join("symbolic.gp200backup");
    use std::io::Write;
    let mut writer = zip::ZipWriter::new(File::create(&path).unwrap());
    for (name, bytes, mode) in zip_entries(r.archive_path()) {
        writer
            .start_file(
                name,
                zip::write::SimpleFileOptions::default().unix_permissions(mode),
            )
            .unwrap();
        writer.write_all(&bytes).unwrap();
    }
    writer
        .add_symlink("link", "outside", zip::write::SimpleFileOptions::default())
        .unwrap();
    writer.finish().unwrap();
    let error = reopen_firmware_backup_review(&path, r.sidecar_path()).unwrap_err();
    assert!(!error.message.contains("pins disagree"));
}
#[test]
fn user_asset_codes_in_wrong_modules_remain_unknown_references() {
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    for (module, code, expected) in [
        (3, 0x0a10_0013u32, "Unknown starting model reference"),
        (3, 0x0f00_0009u32, "Unknown starting model reference"),
        (5, 0x0a10_0013u32, "User IR 20"),
    ] {
        let mut d = store
            .begin_firmware_backup_review(origin(), context(), boundary(1))
            .unwrap();
        let mut o = observation(
            FirmwareBackupObject::Current,
            FirmwareObservationPass::Baseline,
            2,
        );
        let mut records = RawTransfer::from_payload(0x12, o.bytes[1..].to_vec())
            .unwrap()
            .records()
            .to_vec();
        records
            .iter_mut()
            .find(|r| r.command == 0x14 && r.body[0] == module)
            .unwrap()
            .body[4..8]
            .copy_from_slice(&code.to_le_bytes());
        let raw = RawTransfer::from_records(0x12, &records).unwrap();
        o.bytes = [vec![0x12], raw.payload().to_vec()].concat();
        store.store_firmware_backup_observation(&mut d, o).unwrap();
        store
            .close_firmware_backup_review(&mut d, boundary(3), BackupAcquisition::Cancelled)
            .unwrap();
        let r = store.publish_firmware_backup_review(d).unwrap();
        let reference = r
            .summary()
            .references
            .iter()
            .find(|v| v.module == module)
            .unwrap();
        assert_eq!(reference.classification, expected);
    }
}

#[test]
fn metadata_format_mismatch_cannot_receive_wrong_baseline_pin() {
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    let mut d = store
        .begin_firmware_backup_review(origin(), context(), boundary(1))
        .unwrap();
    let mut o = observation(
        FirmwareBackupObject::IrMetadata(0),
        FirmwareObservationPass::Baseline,
        2,
    );
    let record: RawRecord = serde_json::from_slice(&o.bytes).unwrap();
    o.bytes = serde_json::to_vec_pretty(&record).unwrap();
    assert!(store.store_firmware_backup_observation(&mut d, o).is_err());
    let good = observation(
        FirmwareBackupObject::IrMetadata(0),
        FirmwareObservationPass::Baseline,
        2,
    );
    let expected = ContentHash::of(&good.bytes);
    assert_eq!(
        store
            .store_firmware_backup_observation(&mut d, good)
            .unwrap(),
        expected
    );
    store
        .close_firmware_backup_review(&mut d, boundary(3), BackupAcquisition::Cancelled)
        .unwrap();
    let r = store.publish_firmware_backup_review(d).unwrap();
    r.revalidate().unwrap();
}
