use gp200_domain::*;
use gp200_library::*;
use gp200_protocol::{RawRecord, RawTransfer, full_stored_write_request};
use std::{
    fs::{self, File},
    io::Read,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

struct TempRoot(PathBuf);
impl TempRoot {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "gp200-movement-review-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn slot(index: u16) -> SlotId {
    SlotId::new(index).unwrap()
}
fn origin() -> BackupOrigin {
    BackupOrigin {
        identity: DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Hardware,
            firmware: None,
            display_name: "GP-200 fixture".into(),
        },
        epoch: SessionEpoch(4),
        device_key: "movement-review-fixture".into(),
        evidence: vec![EvidenceRef::new(
            "synthetic fixture; no device session",
            EvidenceLevel::StaticRecovered,
        )],
    }
}
fn profile() -> ValidationProfileId {
    ValidationProfileId("synthetic-full-stored-fixture".into())
}
fn capabilities() -> Capabilities {
    let mut result = Capabilities::gp200_read_only();
    result.operations.insert(
        OperationKind::PresetImport,
        FeatureSupport::Verified {
            profile: profile(),
            mode: BackendMode::Hardware,
            firmware: None,
            evidence: vec![EvidenceRef::new(
                "synthetic typed profile; not hardware acceptance",
                EvidenceLevel::Proposed,
            )],
        },
    );
    result
}
fn current(active: u16) -> Arc<PresetSnapshot> {
    let raw = RawTransfer::from_payload(
        0x12,
        include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
    )
    .unwrap();
    let mut records = raw.records().to_vec();
    for record in &mut records {
        match record.command {
            0 => {
                record.body[..2].copy_from_slice(&0u16.to_le_bytes());
                record.body[2..4].copy_from_slice(&active.to_le_bytes());
            }
            2 | 8 => record.body[..2].copy_from_slice(&active.to_le_bytes()),
            _ => {}
        }
    }
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        SnapshotSource::Hardware {
            epoch: SessionEpoch(4),
        },
        Some(PresetTarget::EditBuffer),
        Revision(1),
    ))
}
fn stored(index: u16) -> Arc<PresetSnapshot> {
    let wire = full_stored_write_request(index, &current(23).raw).unwrap();
    let mut records = vec![wire.records()[0].clone()];
    records.extend_from_slice(&wire.records()[2..]);
    records
        .iter_mut()
        .find(|record| record.command == 2)
        .unwrap()
        .body[16..32]
        .copy_from_slice(&[
            b'F',
            b'x',
            index as u8,
            0xff,
            0,
            0xab,
            0xcd,
            0xee,
            0,
            1,
            2,
            3,
            4,
            5,
            6,
            7,
        ]);
    records
        .iter_mut()
        .find(|record| record.command == 0x14 && record.body[0] == 3)
        .unwrap()
        .body[64..68]
        .copy_from_slice(&(0x7fc0_0000 | u32::from(index)).to_le_bytes());
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        SnapshotSource::Hardware {
            epoch: SessionEpoch(4),
        },
        Some(PresetTarget::Stored(slot(index))),
        Revision(1),
    ))
}
fn with_records(snapshot: &PresetSnapshot, records: &[RawRecord]) -> Arc<PresetSnapshot> {
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(snapshot.raw.kind(), records).unwrap()),
        snapshot.source.clone(),
        snapshot.target,
        snapshot.revision,
    ))
}
fn globals() -> Arc<DeviceGlobalState> {
    Arc::new(
        DeviceGlobalState::from_transfer(
            Revision(1),
            SessionEpoch(4),
            RawTransfer::from_payload(
                0x12,
                include_bytes!("../../../tests/fixtures/globals/baseline.payload.bin").to_vec(),
            )
            .unwrap(),
            vec![],
        )
        .unwrap(),
    )
}
fn request(movement: PresetMovementPlan) -> PresetMovementRequest {
    let stored = movement
        .affected_interval()
        .into_iter()
        .flat_map(|interval| interval.slots())
        .chain(movement.sentinels().iter().copied())
        .map(|id| (id, stored(id.get())))
        .collect();
    PresetMovementRequest {
        movement,
        before_backup: String::new(),
        observed: PresetMovementObserved {
            origin: origin(),
            current_buffer: current(23),
            globals: globals(),
            stored,
            capabilities: capabilities(),
            expected_profile: profile(),
        },
        asset_bindings: vec![],
        target_assets: vec![],
    }
}
fn scratch_request() -> PresetMovementRequest {
    request(
        PresetMovementPlan::to_destination(
            DeviceModel::Gp200,
            slot(254),
            slot(255),
            Some(slot(23)),
        )
        .unwrap(),
    )
}
fn capture(store: &mut LibraryStore, request: &mut PresetMovementRequest) {
    let backup = store
        .begin_device_backup(request.observed.origin.clone())
        .unwrap();
    store
        .capture_backup_preset(&backup.id, request.observed.current_buffer.clone())
        .unwrap();
    store
        .capture_backup_globals(&backup.id, &request.observed.globals)
        .unwrap();
    for snapshot in request.observed.stored.values() {
        store
            .capture_backup_preset(&backup.id, snapshot.clone())
            .unwrap();
    }
    store
        .finish_device_backup(&backup.id, BackupAcquisition::Finished)
        .unwrap();
    request.before_backup = backup.id;
}
fn ready(store: &LibraryStore, request: &PresetMovementRequest) -> PresetMovementReview {
    match store.prepare_preset_movement(request).unwrap() {
        PresetMovementPreparation::Ready(review) => *review,
        other => panic!("expected complete ready review: {other:?}"),
    }
}
fn expected_snapshot(store: &LibraryStore, entry: &RestorePlanEntry) -> PresetSnapshot {
    let bytes = fs::read(
        store
            .root()
            .join("blobs")
            .join(entry.expected_snapshot_hash.as_str()),
    )
    .unwrap();
    assert_eq!(ContentHash::of(&bytes), entry.expected_snapshot_hash);
    let snapshot: PresetSnapshot = serde_json::from_slice(&bytes).unwrap();
    let raw = fs::read(
        store
            .root()
            .join("blobs")
            .join(entry.expected_raw_hash.as_str()),
    )
    .unwrap();
    assert_eq!(ContentHash::of(&raw), entry.expected_raw_hash);
    assert_eq!(raw[0], snapshot.raw.kind());
    assert_eq!(&raw[1..], snapshot.raw.payload());
    snapshot
}
fn archived_original(before: &PresetMovementBeforeReview, source: u16) -> PresetSnapshot {
    // Independently open the ZIP and verify its descriptor/raw/snapshot hashes.
    // Do not resolve the live observations or use the restore source adapter.
    let mut archive = zip::ZipArchive::new(File::open(before.archive()).unwrap()).unwrap();
    let mut manifest = Vec::new();
    archive
        .by_name("manifest.json")
        .unwrap()
        .read_to_end(&mut manifest)
        .unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
    let backup: DeviceBackup = serde_json::from_value(manifest["backup"].clone()).unwrap();
    assert_eq!(backup.id, before.backup());
    assert_eq!(
        ContentHash::of(&serde_json::to_vec(&backup).unwrap()),
        *before.descriptor_hash()
    );
    let capture = &backup.presets[&slot(source)];
    let mut snapshot = Vec::new();
    archive
        .by_name(&format!("blobs/{}", capture.snapshot_hash.as_str()))
        .unwrap()
        .read_to_end(&mut snapshot)
        .unwrap();
    assert_eq!(ContentHash::of(&snapshot), capture.snapshot_hash);
    let snapshot: PresetSnapshot = serde_json::from_slice(&snapshot).unwrap();
    let mut raw = Vec::new();
    archive
        .by_name(&format!("blobs/{}", capture.raw_hash.as_str()))
        .unwrap()
        .read_to_end(&mut raw)
        .unwrap();
    assert_eq!(ContentHash::of(&raw), capture.raw_hash);
    assert_eq!(raw[0], snapshot.raw.kind());
    assert_eq!(&raw[1..], snapshot.raw.payload());
    assert_eq!(snapshot.context.unwrap().selector, source);
    snapshot
}
fn assert_original_retargeted(original: &PresetSnapshot, actual: &PresetSnapshot, target: u16) {
    assert_eq!(original.raw.records().len(), actual.raw.records().len());
    for (source, actual) in original.raw.records().iter().zip(actual.raw.records()) {
        assert_eq!(source.command, actual.command);
        let mut expected = source.body.clone();
        match source.command {
            0 => {
                expected[..2].copy_from_slice(&1u16.to_le_bytes());
                expected[2..4].copy_from_slice(&target.to_le_bytes());
            }
            2 | 8 => expected[..2].copy_from_slice(&target.to_le_bytes()),
            _ => {}
        }
        assert_eq!(actual.body, expected, "record {:x}", source.command);
    }
}
fn raw_hash(raw: &RawTransfer) -> ContentHash {
    let mut bytes = vec![raw.kind()];
    bytes.extend_from_slice(raw.payload());
    ContentHash::of(&bytes)
}

#[test]
fn scratch_swap_reopens_durable_originals_and_keeps_both_expected_sounds() {
    let root = TempRoot::new("scratch");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let mut request = scratch_request();
    capture(&mut store, &mut request);
    drop(store);
    let store = LibraryStore::open(&root.0).unwrap();
    let review = ready(&store, &request);
    assert_eq!(review.restore_plan().entries.len(), 2);
    assert!(review.restore_plan().excluded.is_empty());
    let reopened =
        LibraryStore::preview_device_backup(review.before().archive(), BundleLimits::default())
            .unwrap();
    assert_eq!(reopened.id, request.before_backup);
    assert_eq!(reopened.origin, request.observed.origin);
    assert_eq!(
        ContentHash::of(&fs::read(review.before().archive()).unwrap()),
        *review.before().archive_hash()
    );
    assert_eq!(
        ContentHash::of(&serde_json::to_vec(&reopened).unwrap()),
        *review.before().descriptor_hash()
    );
    assert_eq!(
        review.before().current().raw_hash,
        raw_hash(&request.observed.current_buffer.raw)
    );
    assert_eq!(
        review.before().globals().raw_hash,
        raw_hash(request.observed.globals.records().raw())
    );
    assert_eq!(
        review
            .before()
            .slots()
            .iter()
            .map(|pin| (pin.capture().slot.get(), pin.role()))
            .collect::<Vec<_>>(),
        vec![
            (253, PresetMovementBeforeRole::Sentinel),
            (254, PresetMovementBeforeRole::Interval),
            (255, PresetMovementBeforeRole::Interval)
        ]
    );
    for (entry, (source, destination)) in review
        .restore_plan()
        .entries
        .iter()
        .zip([(255, 254), (254, 255)])
    {
        assert_eq!(
            entry.selection.source,
            RestoreSource::BackupStored {
                backup: request.before_backup.clone(),
                slot: slot(source)
            }
        );
        assert_eq!(entry.selection.target, slot(destination));
        assert_original_retargeted(
            &archived_original(review.before(), source),
            &expected_snapshot(&store, entry),
            destination,
        );
    }
    assert_eq!(
        review.movement().current_after_original_reselection(),
        Some(slot(23))
    );
    assert!(store.restore_journals().unwrap().is_empty());
    assert_eq!(review.review_hash().unwrap(), review.review_hash().unwrap());
}

#[test]
fn noncontiguous_previous_uses_every_original_in_the_complete_interval() {
    let root = TempRoot::new("noncontiguous");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let movement = PresetMovementPlan::selected(
        DeviceModel::Gp200,
        &[slot(5), slot(2)],
        PresetMoveAction::Previous,
        Some(slot(23)),
    )
    .unwrap();
    let mut request = request(movement);
    capture(&mut store, &mut request);
    let review = ready(&store, &request);
    let expected_originals = [2, 5, 1, 3, 4];
    assert_eq!(review.restore_plan().entries.len(), 5);
    for ((entry, source), destination) in review
        .restore_plan()
        .entries
        .iter()
        .zip(expected_originals)
        .zip(1..=5)
    {
        assert_eq!(
            entry.selection.source,
            RestoreSource::BackupStored {
                backup: request.before_backup.clone(),
                slot: slot(source)
            }
        );
        assert_eq!(entry.selection.target, slot(destination));
        assert_original_retargeted(
            &archived_original(review.before(), source),
            &expected_snapshot(&store, entry),
            destination,
        );
    }
    assert_eq!(
        review
            .before()
            .slots()
            .iter()
            .filter(|pin| pin.role() == PresetMovementBeforeRole::Sentinel)
            .map(|pin| pin.capture().slot.get())
            .collect::<Vec<_>>(),
        vec![0, 6]
    );
    assert!(store.restore_journals().unwrap().is_empty());
}

#[test]
fn one_active_target_blocks_every_row_without_exposing_a_restore_subset() {
    let root = TempRoot::new("active");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let movement = PresetMovementPlan::to_destination(
        DeviceModel::Gp200,
        slot(254),
        slot(255),
        Some(slot(254)),
    )
    .unwrap();
    let mut request = request(movement);
    request.observed.current_buffer = current(254);
    capture(&mut store, &mut request);
    let PresetMovementPreparation::Blocked(blocked) =
        store.prepare_preset_movement(&request).unwrap()
    else {
        panic!("active target must block the entire movement")
    };
    assert_eq!(blocked.compatible_rows(), 1);
    assert_eq!(blocked.excluded().len(), 1);
    assert_eq!(blocked.excluded()[0].selection.target, slot(254));
    assert!(
        blocked.excluded()[0]
            .reasons
            .iter()
            .any(|reason| reason.contains("inactive"))
    );
    assert!(blocked.before().archive().is_file());
    assert!(store.restore_journals().unwrap().is_empty());
}

#[test]
fn one_unsupported_original_shape_blocks_the_complete_permutation() {
    let root = TempRoot::new("shape");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let mut request = scratch_request();
    let original = &request.observed.stored[&slot(254)];
    let mut records = original.raw.records().to_vec();
    records.push(RawRecord {
        command: 0x3131,
        body: vec![0x80, 0xff, 1],
    });
    request
        .observed
        .stored
        .insert(slot(254), with_records(original, &records));
    capture(&mut store, &mut request);
    let PresetMovementPreparation::Blocked(blocked) =
        store.prepare_preset_movement(&request).unwrap()
    else {
        panic!("one unsupported row must block all movement")
    };
    assert_eq!(blocked.compatible_rows(), 1);
    assert_eq!(blocked.excluded()[0].selection.target, slot(255));
    assert!(
        blocked.excluded()[0]
            .reasons
            .iter()
            .any(|reason| reason.contains("record shape"))
    );
    assert!(store.restore_journals().unwrap().is_empty());
}

#[test]
fn user_ir_metadata_and_local_hash_cannot_supply_private_content_proof() {
    let root = TempRoot::new("asset");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let mut request = scratch_request();
    let original = &request.observed.stored[&slot(254)];
    let mut records = original.raw.records().to_vec();
    records
        .iter_mut()
        .find(|record| record.command == 0x14 && record.body[0] == 5)
        .unwrap()
        .body[4..8]
        .copy_from_slice(&0x0a10_0000u32.to_le_bytes());
    request
        .observed
        .stored
        .insert(slot(254), with_records(original, &records));
    capture(&mut store, &mut request);
    let PresetMovementPreparation::Blocked(blocked) =
        store.prepare_preset_movement(&request).unwrap()
    else {
        panic!("unrecoverable IR blocks the whole move")
    };
    assert_eq!(blocked.compatible_rows(), 1);
    assert!(
        blocked.excluded()[0]
            .reasons
            .iter()
            .any(|reason| reason.contains("content binding"))
    );
    let ir = DeviceAssetSlot::new(DeviceAssetKind::Ir, 0).unwrap();
    request.asset_bindings.push(RestoreAssetBinding {
        source: ir,
        target: ir,
        content_hash: ContentHash::of(&vec![0; 4096]),
    });
    let PresetMovementPreparation::Blocked(blocked) =
        store.prepare_preset_movement(&request).unwrap()
    else {
        panic!("a local digest cannot establish pedal content")
    };
    assert!(
        blocked.excluded()[0]
            .reasons
            .iter()
            .any(|reason| reason.contains("Source content"))
    );
    assert!(
        TargetAssetProof::from_verified_transfer(
            ir,
            &origin(),
            &vec![0; 4096],
            &Capabilities::gp200_read_only(),
            &profile()
        )
        .is_err()
    );
    assert!(store.restore_journals().unwrap().is_empty());
}

#[test]
fn identity_only_asset_policy_rejects_remapping_before_archive_publication() {
    let root = TempRoot::new("remap");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let mut request = scratch_request();
    capture(&mut store, &mut request);
    request.asset_bindings.push(RestoreAssetBinding {
        source: DeviceAssetSlot::new(DeviceAssetKind::Ir, 0).unwrap(),
        target: DeviceAssetSlot::new(DeviceAssetKind::Ir, 1).unwrap(),
        content_hash: ContentHash::of(&vec![0; 4096]),
    });
    assert!(store.prepare_preset_movement(&request).is_err());
    assert!(!store.root().join("movement-before").exists());
}

#[test]
fn missing_changed_or_stale_observations_and_wrong_profiles_fail_closed() {
    let root = TempRoot::new("freshness");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let mut base = scratch_request();
    capture(&mut store, &mut base);
    let mut cases = Vec::new();
    for missing in [253, 254, 255] {
        let mut case = base.clone();
        case.observed.stored.remove(&slot(missing));
        cases.push(case);
    }
    let mut stale = base.clone();
    Arc::make_mut(stale.observed.stored.get_mut(&slot(255)).unwrap()).revision = Revision(0);
    cases.push(stale);
    let mut stale = base.clone();
    Arc::make_mut(&mut stale.observed.current_buffer).revision = Revision(0);
    cases.push(stale);
    let mut stale = base.clone();
    Arc::make_mut(&mut stale.observed.globals).revision = Revision(0);
    cases.push(stale);
    let mut wrong = base.clone();
    wrong.observed.origin.device_key = "other-device".into();
    cases.push(wrong);
    let mut wrong = base.clone();
    wrong.observed.origin.epoch = SessionEpoch(5);
    cases.push(wrong);
    let mut wrong = base.clone();
    wrong.observed.origin.identity.mode = BackendMode::CaptureReplay;
    cases.push(wrong);
    let mut wrong = base.clone();
    wrong.observed.origin.identity.model = DeviceModel::Gp200Lt;
    cases.push(wrong);
    let mut wrong = base.clone();
    wrong.observed.capabilities = Capabilities::gp200_read_only();
    cases.push(wrong);
    let mut wrong = base.clone();
    wrong.observed.expected_profile = ValidationProfileId("other-profile".into());
    cases.push(wrong);
    let mut wrong = base.clone();
    Arc::make_mut(&mut wrong.observed.globals).epoch = SessionEpoch(5);
    cases.push(wrong);
    let mut changed = base.clone();
    let original = &changed.observed.stored[&slot(253)];
    let mut records = original.raw.records().to_vec();
    records
        .iter_mut()
        .find(|record| record.command == 2)
        .unwrap()
        .body[31] ^= 0x80;
    changed
        .observed
        .stored
        .insert(slot(253), with_records(original, &records));
    cases.push(changed);
    let mut changed = base.clone();
    let original = &changed.observed.current_buffer;
    let mut records = original.raw.records().to_vec();
    records
        .iter_mut()
        .find(|record| record.command == 2)
        .unwrap()
        .body[31] ^= 0x80;
    changed.observed.current_buffer = with_records(original, &records);
    cases.push(changed);
    let mut changed = base.clone();
    let playing = changed
        .observed
        .globals
        .records()
        .runtime
        .as_ref()
        .unwrap()
        .drum_playing()
        .unwrap();
    let raw = changed
        .observed
        .globals
        .expected_after(&GlobalControlEdit::DrumPlay(!playing))
        .unwrap();
    changed.observed.globals = Arc::new(
        DeviceGlobalState::from_transfer(Revision(2), SessionEpoch(4), raw, vec![]).unwrap(),
    );
    cases.push(changed);
    for (index, case) in cases.iter().enumerate() {
        assert!(store.prepare_preset_movement(case).is_err(), "case {index}");
    }
    assert!(!store.root().join("movement-before").exists());
    assert!(store.restore_journals().unwrap().is_empty());
}

#[test]
fn missing_before_source_current_globals_and_sentinel_cannot_be_inferred() {
    for missing in ["source", "current", "globals", "sentinel"] {
        let root = TempRoot::new(missing);
        let mut store = LibraryStore::open(&root.0).unwrap();
        let mut request = scratch_request();
        let backup = store.begin_device_backup(origin()).unwrap();
        if missing != "current" {
            store
                .capture_backup_preset(&backup.id, request.observed.current_buffer.clone())
                .unwrap();
        }
        if missing != "globals" {
            store
                .capture_backup_globals(&backup.id, &request.observed.globals)
                .unwrap();
        }
        for (id, snapshot) in &request.observed.stored {
            if (missing == "source" && *id == slot(254))
                || (missing == "sentinel" && *id == slot(253))
            {
                continue;
            }
            store
                .capture_backup_preset(&backup.id, snapshot.clone())
                .unwrap();
        }
        store
            .finish_device_backup(&backup.id, BackupAcquisition::Finished)
            .unwrap();
        request.before_backup = backup.id;
        assert!(
            store.prepare_preset_movement(&request).is_err(),
            "missing {missing}"
        );
        assert!(!store.root().join("movement-before").exists());
        assert!(store.restore_journals().unwrap().is_empty());
    }
}

#[test]
fn unfinished_or_cancelled_archive_never_becomes_ready() {
    for finish in [None, Some(BackupAcquisition::Cancelled)] {
        let root = TempRoot::new("terminal");
        let mut store = LibraryStore::open(&root.0).unwrap();
        let mut request = scratch_request();
        let backup = store.begin_device_backup(origin()).unwrap();
        request.before_backup = backup.id;
        if let Some(outcome) = finish {
            store
                .finish_device_backup(&request.before_backup, outcome)
                .unwrap();
        }
        assert!(store.prepare_preset_movement(&request).is_err());
        assert!(!store.root().join("movement-before").exists());
    }
}

#[test]
fn source_blob_corruption_cannot_become_a_durable_movement_review() {
    let root = TempRoot::new("corrupt");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let mut request = scratch_request();
    capture(&mut store, &mut request);
    let backup = store.device_backup(&request.before_backup).unwrap();
    fs::write(
        store
            .root()
            .join("blobs")
            .join(backup.presets[&slot(254)].raw_hash.as_str()),
        b"broken original",
    )
    .unwrap();
    assert!(store.prepare_preset_movement(&request).is_err());
    assert!(!store.root().join("movement-before").exists());
    assert!(store.restore_journals().unwrap().is_empty());
}

#[test]
fn noop_has_no_archive_blob_journal_or_manifest_change() {
    let root = TempRoot::new("noop");
    let store = LibraryStore::open(&root.0).unwrap();
    let mut request = request(
        PresetMovementPlan::to_destination(
            DeviceModel::Gp200,
            slot(254),
            slot(254),
            Some(slot(23)),
        )
        .unwrap(),
    );
    request.before_backup = "not-an-archive".into();
    request.observed.capabilities = Capabilities::gp200_read_only();
    request.observed.expected_profile = ValidationProfileId(String::new());
    let generation = store.generation();
    let manifest = fs::read(store.root().join("manifest.json")).unwrap();
    assert!(matches!(
        store.prepare_preset_movement(&request).unwrap(),
        PresetMovementPreparation::Noop(_)
    ));
    assert_eq!(store.generation(), generation);
    assert_eq!(
        fs::read(store.root().join("manifest.json")).unwrap(),
        manifest
    );
    assert_eq!(fs::read_dir(store.root().join("blobs")).unwrap().count(), 0);
    assert!(!store.root().join("movement-before").exists());
    assert!(store.restore_journals().unwrap().is_empty());
}

#[test]
fn descriptive_original_reselection_and_complete_review_hash_are_separate_from_execution() {
    let root = TempRoot::new("hash");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let mut with_current = scratch_request();
    capture(&mut store, &mut with_current);
    let mut without_current = with_current.clone();
    without_current.movement =
        PresetMovementPlan::to_destination(DeviceModel::Gp200, slot(254), slot(255), None).unwrap();
    let a = ready(&store, &with_current);
    let b = ready(&store, &without_current);
    assert_eq!(
        a.restore_plan().review_hash().unwrap(),
        b.restore_plan().review_hash().unwrap()
    );
    assert_ne!(a.review_hash().unwrap(), b.review_hash().unwrap());
    assert_eq!(a.before().current().raw_hash, b.before().current().raw_hash);
    assert_eq!(a.before().globals().raw_hash, b.before().globals().raw_hash);
    assert!(store.restore_journals().unwrap().is_empty());
}

#[test]
fn overwriting_a_live_destination_cannot_replace_a_later_archived_source() {
    let root = TempRoot::new("originals");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let mut request = scratch_request();
    capture(&mut store, &mut request);
    let review = ready(&store, &request);
    let first_expected = expected_snapshot(&store, &review.restore_plan().entries[0]);
    let later_expected = expected_snapshot(&store, &review.restore_plan().entries[1]);
    let original = archived_original(review.before(), 254);
    assert_original_retargeted(&original, &later_expected, 255);
    // This is a synthetic live map update, not a library/device dispatch.
    request
        .observed
        .stored
        .insert(slot(254), Arc::new(first_expected));
    assert!(store.prepare_preset_movement(&request).is_err());
    assert_original_retargeted(
        &archived_original(review.before(), 254),
        &expected_snapshot(&store, &review.restore_plan().entries[1]),
        255,
    );
    assert!(
        store
            .capture_backup_preset(
                &request.before_backup,
                request.observed.stored[&slot(254)].clone()
            )
            .is_err()
    );
    assert!(store.restore_journals().unwrap().is_empty());
}
