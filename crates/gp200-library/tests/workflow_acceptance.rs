use gp200_catalog::Catalog;
use gp200_domain::*;
use gp200_library::*;
use gp200_protocol::{
    AssetBank, AssetSlotMetadata, RawRecord, RawTransfer, full_stored_write_request,
};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

struct TempRoot(PathBuf);
impl TempRoot {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "gp200-workflows-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
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
fn origin(epoch: u64) -> BackupOrigin {
    BackupOrigin {
        identity: DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Hardware,
            firmware: None,
            display_name: "GP-200".into(),
        },
        epoch: SessionEpoch(epoch),
        device_key: "usb-serial-20220118".into(),
        evidence: vec![EvidenceRef::new(
            "test fixture; not a new physical test",
            EvidenceLevel::StaticRecovered,
        )],
    }
}
fn current(epoch: u64) -> Arc<PresetSnapshot> {
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(
            RawTransfer::from_payload(
                0x12,
                include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
            )
            .unwrap(),
        ),
        SnapshotSource::Hardware {
            epoch: SessionEpoch(epoch),
        },
        Some(PresetTarget::EditBuffer),
        Revision(1),
    ))
}
fn stored(index: u16, epoch: u64, revision: u64) -> Arc<PresetSnapshot> {
    let wire = full_stored_write_request(index, &current(epoch).raw).unwrap();
    let mut records = vec![wire.records()[0].clone()];
    records.extend_from_slice(&wire.records()[2..]);
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        SnapshotSource::Hardware {
            epoch: SessionEpoch(epoch),
        },
        Some(PresetTarget::Stored(slot(index))),
        Revision(revision),
    ))
}
fn changed(snapshot: &PresetSnapshot, module: u8, parameter: u8, bits: u32) -> Arc<PresetSnapshot> {
    let mut records = snapshot.raw.records().to_vec();
    let record = records
        .iter_mut()
        .find(|record| record.command == 0x14 && record.body[0] == module)
        .unwrap();
    let offset = 8 + usize::from(parameter) * 4;
    record.body[offset..offset + 4].copy_from_slice(&bits.to_le_bytes());
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(snapshot.raw.kind(), &records).unwrap()),
        snapshot.source.clone(),
        snapshot.target,
        Revision(snapshot.revision.0 + 1),
    ))
}
fn retarget_epoch(snapshot: &PresetSnapshot, epoch: u64, revision: u64) -> Arc<PresetSnapshot> {
    let mut snapshot = snapshot.clone();
    snapshot.source = SnapshotSource::Hardware {
        epoch: SessionEpoch(epoch),
    };
    snapshot.revision = Revision(revision);
    Arc::new(snapshot)
}
fn globals(epoch: u64) -> DeviceGlobalState {
    DeviceGlobalState::from_transfer(
        Revision(1),
        SessionEpoch(epoch),
        RawTransfer::from_payload(
            0x12,
            include_bytes!("../../../tests/fixtures/globals/baseline.payload.bin").to_vec(),
        )
        .unwrap(),
        vec![],
    )
    .unwrap()
}
fn metadata(kind: DeviceAssetKind, index: u8) -> AssetSlotMetadata {
    let bank = match kind {
        DeviceAssetKind::Ir => AssetBank::Ir,
        DeviceAssetKind::SnapTone => AssetBank::SnapTone,
    };
    let mut body = vec![0; 24];
    body[..2].copy_from_slice(&u16::from(index).to_le_bytes());
    body[2..4].copy_from_slice(&1u16.to_le_bytes());
    body[8..12].copy_from_slice(b"Test");
    AssetSlotMetadata::decode(
        bank,
        &RawRecord {
            command: bank.metadata_command(),
            body,
        },
    )
    .unwrap()
}
fn request(backup: &str, epoch: u64, mappings: &[(u16, u16)]) -> RestoreRequest {
    RestoreRequest {
        target: origin(epoch),
        current_buffer: current(epoch),
        selections: mappings
            .iter()
            .map(|(from, to)| RestoreSelection {
                source: RestoreSource::BackupStored {
                    backup: backup.into(),
                    slot: slot(*from),
                },
                target: slot(*to),
            })
            .collect(),
        observed_targets: mappings
            .iter()
            .map(|(_, to)| (slot(*to), stored(*to, epoch, 1)))
            .collect(),
        asset_bindings: vec![],
        target_assets: vec![],
    }
}
fn review(store: &mut LibraryStore, plan: RestorePlan) -> RestoreJournal {
    let plan_hash = plan.review_hash().unwrap();
    store
        .begin_restore(
            plan,
            RestoreReview {
                plan_hash,
                accept_unknown_firmware: false,
            },
        )
        .unwrap()
}
fn rewrite_zip(
    source: &std::path::Path,
    destination: &std::path::Path,
    mut modify: impl FnMut(&str, &mut Vec<u8>),
) {
    let mut reader = ZipArchive::new(File::open(source).unwrap()).unwrap();
    let mut writer = ZipWriter::new(File::create(destination).unwrap());
    for index in 0..reader.len() {
        let mut entry = reader.by_index(index).unwrap();
        let name = entry.name().to_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        modify(&name, &mut bytes);
        writer
            .start_file(
                name,
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
            )
            .unwrap();
        writer.write_all(&bytes).unwrap();
    }
    writer.finish().unwrap();
}

#[test]
fn cancelled_acquisition_survives_restart_and_edit_buffer_does_not_fill_missing_slot() {
    let root = TempRoot::new("partial");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let backup = store.begin_device_backup(origin(4)).unwrap();
    store.capture_backup_preset(&backup.id, current(4)).unwrap();
    store
        .capture_backup_preset(&backup.id, stored(0, 4, 7))
        .unwrap();
    store
        .record_backup_read_error(&backup.id, "preset-1", "Disconnected during read")
        .unwrap();
    store
        .finish_device_backup(&backup.id, BackupAcquisition::Cancelled)
        .unwrap();
    drop(store);
    let mut store = LibraryStore::open(&root.0).unwrap();
    let loaded = store.device_backup(&backup.id).unwrap();
    assert_eq!(loaded.acquisition, BackupAcquisition::Cancelled);
    assert_eq!(loaded.completeness().stored_presets, 1);
    assert_eq!(loaded.completeness().missing_presets.len(), 255);
    assert!(loaded.completeness().edit_buffer_captured);
    assert!(!loaded.completeness().complete_device_state);
    assert_eq!(
        store
            .load_backup_preset(&backup.id, PresetTarget::Stored(slot(0)))
            .unwrap()
            .raw,
        stored(0, 4, 7).raw
    );
    assert!(
        store
            .capture_backup_preset(&backup.id, stored(1, 4, 8))
            .is_err()
    );
    assert_eq!(loaded.read_errors["preset-1"], "Disconnected during read");
}

#[test]
fn all_256_presets_and_30_metadata_records_still_do_not_claim_asset_content() {
    let root = TempRoot::new("whole-bank");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let backup = store.begin_device_backup(origin(4)).unwrap();
    for index in 0..256 {
        store
            .capture_backup_preset(&backup.id, stored(index, 4, u64::from(index) + 1))
            .unwrap();
    }
    store
        .capture_backup_globals(&backup.id, &globals(4))
        .unwrap();
    for kind in [DeviceAssetKind::Ir, DeviceAssetKind::SnapTone] {
        for index in 0..kind.slots() {
            store
                .capture_backup_asset_metadata(&backup.id, SessionEpoch(4), &metadata(kind, index))
                .unwrap();
        }
    }
    let report = store.device_backup(&backup.id).unwrap().completeness();
    assert_eq!(report.stored_presets, 256);
    assert_eq!(report.asset_metadata, 30);
    assert!(report.globals_captured);
    assert!(report.missing_presets.is_empty());
    assert!(report.missing_asset_metadata.is_empty());
    assert_eq!(report.missing_asset_content.len(), 30);
    assert!(!report.complete_device_state);
    // Even an archive claiming every asset payload remains incomplete when
    // it omits the separate unsaved edit buffer. Claims never authorize I/O.
    let mut claimed = store.device_backup(&backup.id).unwrap();
    for asset in claimed.assets.values_mut() {
        asset.content = Some(CapturedAssetContent {
            hash: ContentHash::of(b"retained test provenance claim"),
            bytes: 4096,
            format: "test claim; not a hardware observation".into(),
            origin: AssetContentOrigin::DeviceReadback {
                evidence: vec![EvidenceRef::new(
                    "retained fixture claim",
                    EvidenceLevel::LiveVerified,
                )],
            },
        });
    }
    assert!(claimed.completeness().missing_asset_content.is_empty());
    assert!(!claimed.completeness().complete_device_state);
    assert!(claimed.completeness().requires_hardware_revalidation);
    let current_only = store.begin_device_backup(origin(4)).unwrap();
    store
        .capture_backup_preset(&current_only.id, current(4))
        .unwrap();
    claimed.edit_buffer = store.device_backup(&current_only.id).unwrap().edit_buffer;
    assert!(claimed.completeness().complete_device_state);
    assert!(claimed.completeness().requires_hardware_revalidation);
    assert_eq!(
        store.load_backup_globals(&backup.id).unwrap(),
        *globals(4).records().raw()
    );
    store
        .capture_backup_asset_content(
            &backup.id,
            SessionEpoch(4),
            DeviceAssetSlot::new(DeviceAssetKind::Ir, 0).unwrap(),
            b"original WAV but unknown device payload",
            "original WAV",
            AssetContentOrigin::LocalUnverified,
        )
        .unwrap();
    assert_eq!(
        store
            .device_backup(&backup.id)
            .unwrap()
            .completeness()
            .missing_asset_content
            .len(),
        30
    );
    store
        .finish_device_backup(&backup.id, BackupAcquisition::Finished)
        .unwrap();
    drop(store);
    assert_eq!(
        LibraryStore::open(&root.0)
            .unwrap()
            .device_backup(&backup.id)
            .unwrap()
            .completeness(),
        report
    );
}

#[test]
fn session_context_wrong_buffer_and_incomplete_records_are_rejected_without_progress() {
    let root = TempRoot::new("provenance");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let backup = store.begin_device_backup(origin(4)).unwrap();
    let generation = store.generation();
    assert!(
        store
            .capture_backup_preset(&backup.id, stored(0, 5, 1))
            .is_err()
    );
    let mut forged = (*stored(0, 4, 1)).clone();
    forged.target = Some(PresetTarget::Stored(slot(1)));
    assert!(
        store
            .capture_backup_preset(&backup.id, Arc::new(forged))
            .is_err()
    );
    let mut incomplete = stored(0, 4, 1).raw.records().to_vec();
    incomplete.retain(|record| record.command != 0x0c);
    let incomplete = Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &incomplete).unwrap()),
        SnapshotSource::Hardware {
            epoch: SessionEpoch(4),
        },
        Some(PresetTarget::Stored(slot(0))),
        Revision(1),
    ));
    assert!(store.capture_backup_preset(&backup.id, incomplete).is_err());
    assert!(
        store
            .capture_backup_globals(&backup.id, &globals(5))
            .is_err()
    );
    assert_eq!(store.generation(), generation);
    assert_eq!(store.device_backup(&backup.id).unwrap().presets.len(), 0);
}

#[test]
fn capture_does_not_mix_different_reads_of_the_same_slot() {
    let root = TempRoot::new("mutating-slot");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let backup = store.begin_device_backup(origin(4)).unwrap();
    let original = stored(0, 4, 1);
    store
        .capture_backup_preset(&backup.id, original.clone())
        .unwrap();
    let altered = changed(&original, 3, 0, 0x80000000);
    assert_eq!(
        store
            .capture_backup_preset(&backup.id, altered)
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(
        store
            .load_backup_preset(&backup.id, PresetTarget::Stored(slot(0)))
            .unwrap()
            .raw,
        original.raw
    );
}

#[test]
fn independent_device_archive_preserves_partial_acquisition_and_exact_unknown_float_bits() {
    let root = TempRoot::new("independent-archive");
    let mut first = LibraryStore::open(root.0.join("first")).unwrap();
    let backup = first.begin_device_backup(origin(4)).unwrap();
    let original = changed(
        &changed(&stored(7, 4, 1), 3, 14, 0x7fc000a5),
        3,
        13,
        0x80000000,
    );
    first
        .capture_backup_preset(&backup.id, original.clone())
        .unwrap();
    first
        .finish_device_backup(&backup.id, BackupAcquisition::Cancelled)
        .unwrap();
    let path = root.0.join("pedal.gp200backup");
    let report = first.export_device_backup(&backup.id, &path).unwrap();
    assert_eq!(
        LibraryStore::preview_device_backup(&path, BundleLimits::default())
            .unwrap()
            .completeness(),
        report
    );
    let mut second = LibraryStore::open(root.0.join("second")).unwrap();
    let imported = second.import_device_backup(&path).unwrap();
    assert_ne!(imported.id, backup.id);
    assert_eq!(imported.origin, backup.origin);
    assert_eq!(imported.completeness(), report);
    assert_eq!(
        second
            .load_backup_preset(&imported.id, PresetTarget::Stored(slot(7)))
            .unwrap()
            .raw,
        original.raw
    );
}

#[test]
fn restore_preview_retargets_only_context_and_two_slot_headers_and_preserves_unknown_bits() {
    let root = TempRoot::new("retarget");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let backup = store.begin_device_backup(origin(4)).unwrap();
    let source = changed(&stored(3, 4, 1), 3, 14, 0x7fc000a5);
    store
        .capture_backup_preset(&backup.id, source.clone())
        .unwrap();
    let plan = store
        .plan_restore(&request(&backup.id, 4, &[(3, 254)]))
        .unwrap();
    assert_eq!(plan.entries.len(), 1);
    assert!(plan.excluded.is_empty());
    let entry = &plan.entries[0];
    assert!(!entry.compatibility_review_required);
    let journal = review(&mut store, plan);
    let dispatch = store
        .begin_restore_entry(&journal.id, slot(254), stored(254, 4, 1), current(4))
        .unwrap();
    assert_eq!(dispatch.target, slot(254));
    assert_eq!(dispatch.expected_target_revision, Revision(1));
    let a = source.raw.records();
    let b = dispatch.snapshot.raw.records();
    assert_eq!(a.len(), b.len());
    for (a, b) in a.iter().zip(b) {
        assert_eq!(a.command, b.command);
        let mut expected = a.body.clone();
        match a.command {
            0 => {
                expected[..2].copy_from_slice(&1u16.to_le_bytes());
                expected[2..4].copy_from_slice(&254u16.to_le_bytes());
            }
            2 | 8 => expected[..2].copy_from_slice(&254u16.to_le_bytes()),
            _ => {}
        }
        assert_eq!(expected, b.body);
    }
    assert_eq!(
        store.restore_journal(&journal.id).unwrap().entries[0].state,
        RestoreEntryState::InFlight
    );
}

#[test]
fn active_target_duplicate_mapping_and_unreviewed_firmware_are_explicitly_blocked() {
    let root = TempRoot::new("restore-review");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let backup = store.begin_device_backup(origin(4)).unwrap();
    store
        .capture_backup_preset(&backup.id, stored(1, 4, 1))
        .unwrap();
    let active = current(4).context.unwrap().selector;
    let plan = store
        .plan_restore(&request(&backup.id, 4, &[(1, active)]))
        .unwrap();
    assert!(plan.entries.is_empty());
    assert_eq!(plan.excluded.len(), 1);
    assert!(
        store
            .plan_restore(&request(&backup.id, 4, &[(1, 4), (1, 4)]))
            .is_err()
    );
    let mut other = request(&backup.id, 4, &[(1, 4)]);
    other.target.device_key = "another-pedal".into();
    let plan = store.plan_restore(&other).unwrap();
    assert!(plan.entries[0].compatibility_review_required);
    let hash = plan.review_hash().unwrap();
    assert_eq!(
        store
            .begin_restore(
                plan.clone(),
                RestoreReview {
                    plan_hash: hash,
                    accept_unknown_firmware: false
                }
            )
            .unwrap_err()
            .kind,
        ErrorKind::Unverified
    );
    let mut modified = plan.clone();
    modified.entries[0].before_revision = Revision(8);
    assert_eq!(
        store
            .begin_restore(
                modified,
                RestoreReview {
                    plan_hash: plan.review_hash().unwrap(),
                    accept_unknown_firmware: true
                }
            )
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
}

#[test]
fn dirty_and_stale_target_readback_never_complete_or_allow_following_writes() {
    let root = TempRoot::new("dirty-readback");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let backup = store.begin_device_backup(origin(4)).unwrap();
    store
        .capture_backup_preset(&backup.id, changed(&stored(1, 4, 1), 3, 0, 60f32.to_bits()))
        .unwrap();
    let plan = store
        .plan_restore(&request(&backup.id, 4, &[(1, 252), (1, 253)]))
        .unwrap();
    let journal = review(&mut store, plan);
    assert!(
        store
            .begin_restore_entry(&journal.id, slot(252), stored(252, 4, 0), current(4))
            .is_err()
    );
    let dispatch = store
        .begin_restore_entry(&journal.id, slot(252), stored(252, 4, 1), current(4))
        .unwrap();
    let stale = retarget_epoch(&dispatch.snapshot, 5, 2);
    assert!(store.confirm_restore_readback(&journal.id, stale).is_err());
    let dirty = changed(&dispatch.snapshot, 3, 14, 0x80000000);
    let result = store.confirm_restore_readback(&journal.id, dirty).unwrap();
    assert!(matches!(
        result.entries[0].state,
        RestoreEntryState::Conflict { .. }
    ));
    assert!(matches!(result.state, RestoreRunState::Paused { .. }));
    assert!(
        store
            .begin_restore_entry(&journal.id, slot(253), stored(253, 4, 1), current(4))
            .is_err()
    );
    assert_eq!(result.verified_count(), 0);
}

#[test]
fn crash_reconnect_reconciles_applied_write_and_only_dispatches_remaining_planned_slots() {
    let root = TempRoot::new("crash-applied");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let backup = store.begin_device_backup(origin(4)).unwrap();
    store
        .capture_backup_preset(&backup.id, changed(&stored(1, 4, 1), 3, 0, 60f32.to_bits()))
        .unwrap();
    let plan = store
        .plan_restore(&request(&backup.id, 4, &[(1, 252), (1, 253)]))
        .unwrap();
    let journal = review(&mut store, plan);
    let first = store
        .begin_restore_entry(&journal.id, slot(252), stored(252, 4, 1), current(4))
        .unwrap();
    drop(store);
    let mut store = LibraryStore::open(&root.0).unwrap();
    let recovered = store.restore_journal(&journal.id).unwrap();
    assert!(matches!(
        recovered.entries[0].state,
        RestoreEntryState::Uncertain { .. }
    ));
    assert!(matches!(recovered.state, RestoreRunState::Paused { .. }));
    assert!(store.recovery_note.is_some());
    assert!(
        store
            .begin_restore_entry(&journal.id, slot(253), stored(253, 4, 1), current(4))
            .is_err()
    );
    let readbacks = BTreeMap::from([
        (slot(252), retarget_epoch(&first.snapshot, 5, 2)),
        (slot(253), stored(253, 5, 2)),
    ]);
    let reconciled = store
        .reconcile_restore(&journal.id, origin(5), readbacks)
        .unwrap();
    assert_eq!(reconciled.verified_count(), 1);
    assert_eq!(reconciled.state, RestoreRunState::Ready);
    assert!(
        store
            .begin_restore_entry(
                &journal.id,
                slot(252),
                retarget_epoch(&first.snapshot, 5, 2),
                current(5)
            )
            .is_err()
    );
    let second = store
        .begin_restore_entry(&journal.id, slot(253), stored(253, 5, 2), current(5))
        .unwrap();
    let result = store
        .confirm_restore_readback(&journal.id, retarget_epoch(&second.snapshot, 5, 3))
        .unwrap();
    assert_eq!(result.state, RestoreRunState::Completed);
    assert_eq!(result.verified_count(), 2);
}

#[test]
fn unapplied_uncertain_write_requires_explicit_retry_review_after_fresh_readback() {
    let root = TempRoot::new("crash-unapplied");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let backup = store.begin_device_backup(origin(4)).unwrap();
    store
        .capture_backup_preset(&backup.id, changed(&stored(1, 4, 1), 3, 0, 60f32.to_bits()))
        .unwrap();
    let plan = store
        .plan_restore(&request(&backup.id, 4, &[(1, 252)]))
        .unwrap();
    let journal = review(&mut store, plan);
    store
        .begin_restore_entry(&journal.id, slot(252), stored(252, 4, 1), current(4))
        .unwrap();
    store
        .pause_restore(&journal.id, "Connection lost after dispatch")
        .unwrap();
    let actual = stored(252, 5, 2);
    let reconciled = store
        .reconcile_restore(
            &journal.id,
            origin(5),
            BTreeMap::from([(slot(252), actual.clone())]),
        )
        .unwrap();
    assert_eq!(
        reconciled.entries[0].state,
        RestoreEntryState::NeedsRetryReview
    );
    assert!(
        store
            .begin_restore_entry(&journal.id, slot(252), actual.clone(), current(5))
            .is_err()
    );
    assert!(
        store
            .approve_restore_retry(&journal.id, slot(252), ContentHash::of(b"wrong state"))
            .is_err()
    );
    let reviewed = store
        .approve_restore_retry(
            &journal.id,
            slot(252),
            reconciled.entries[0].observed_raw_hash.clone(),
        )
        .unwrap();
    assert_eq!(reviewed.state, RestoreRunState::Ready);
    store
        .begin_restore_entry(&journal.id, slot(252), actual, current(5))
        .unwrap();
    assert_eq!(
        store.restore_journal(&journal.id).unwrap().entries[0].attempts,
        2
    );
}

#[test]
fn narrowly_observed_padding_is_ignored_but_unknown_bytes_and_float_bits_remain_strict() {
    let source = stored(252, 4, 1);
    let mut records = source.raw.records().to_vec();
    for record in &mut records {
        match record.command {
            2 | 8 => record.body[15] ^= 0x55,
            0x0f => {
                record.body[2] ^= 0x44;
                record.body[3] ^= 0x33
            }
            _ => {}
        }
    }
    let padded = RawTransfer::from_records(0x11, &records).unwrap();
    assert!(stored_readback_equal(&source.raw, &padded));
    records
        .iter_mut()
        .find(|record| record.command == 2)
        .unwrap()
        .body[14] ^= 1;
    assert!(!stored_readback_equal(
        &source.raw,
        &RawTransfer::from_records(0x12, &records).unwrap()
    ));
    assert!(!stored_readback_equal(
        &source.raw,
        &changed(&source, 3, 14, 0x7fc000a5).raw
    ));
}

#[test]
fn setlists_keep_revision_pins_order_notes_and_missing_or_trash_references_after_reopen() {
    let root = TempRoot::new("setlists");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let version = store
        .store_snapshot(
            current(4),
            LibraryMetadata {
                name: "Clean".into(),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let loaded = store.load_version(&version).unwrap();
    let mut setlist = store.create_setlist("Friday rehearsal").unwrap();
    let mut first = SetlistEntry::new(version.clone(), "First song").unwrap();
    first.section = "Chorus".into();
    first.notes = "Use bridge pickup".into();
    first.tempo_fallback = Some(120);
    first.proposed_slot = Some(slot(16));
    let missing =
        SetlistEntry::new(LibraryVersionId("missing-revision".into()), "Missing song").unwrap();
    setlist.entries = vec![missing.clone(), first.clone()];
    let setlist = store.save_setlist(setlist, Revision(0)).unwrap();
    let newer = store
        .store_snapshot(
            changed(&loaded.snapshot, 3, 0, 51f32.to_bits()),
            loaded.metadata.clone(),
            Some(version.clone()),
        )
        .unwrap();
    assert!(store.save_setlist(setlist.clone(), Revision(0)).is_err());
    drop(store);
    let mut store = LibraryStore::open(&root.0).unwrap();
    let resolved = store.resolve_setlist(&setlist.id).unwrap();
    assert_eq!(resolved.setlist.entries, vec![missing, first]);
    assert_eq!(resolved.entries[0].state, SetlistReferenceState::Missing);
    assert!(
        matches!(&resolved.entries[1].state,SetlistReferenceState::Available{newer_revision:Some(id),..} if id==&newer)
    );
    assert_eq!(resolved.entries[1].entry.version, version);
    store.delete_to_trash(&loaded.library).unwrap();
    assert!(matches!(
        store.resolve_setlist(&setlist.id).unwrap().entries[1].state,
        SetlistReferenceState::Trashed { .. }
    ));
    let trashed = store.setlist_trash(&setlist.id, Revision(1), true).unwrap();
    assert!(store.setlists(false).unwrap().is_empty());
    drop(store);
    let mut store = LibraryStore::open(&root.0).unwrap();
    assert_eq!(store.setlists(true).unwrap()[0], trashed);
    store
        .setlist_trash(&setlist.id, trashed.revision, false)
        .unwrap();
    assert_eq!(store.setlists(false).unwrap().len(), 1);
}

#[test]
fn patch_assignment_changes_have_semantic_diff_and_padding_stays_raw_only() {
    let before = current(4);
    let mut records = before.raw.records().to_vec();
    records
        .iter_mut()
        .find(|record| record.command == 0x0c)
        .unwrap()
        .body[4..8]
        .copy_from_slice(&99f32.to_le_bytes());
    records
        .iter_mut()
        .find(|record| record.command == 0x0f)
        .unwrap()
        .body[4] ^= 1;
    records
        .iter_mut()
        .find(|record| record.command == 0x10)
        .unwrap()
        .body[1] = 0xff;
    records
        .iter_mut()
        .find(|record| record.command == 8)
        .unwrap()
        .body[2] = 3;
    let after = PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        before.source.clone(),
        before.target,
        Revision(2),
    );
    let diff = compare_presets(&before, &after, &Catalog::embedded().unwrap());
    assert!(diff.known.contains(&KnownChange::Expressions));
    assert!(diff.known.contains(&KnownChange::Footswitches));
    assert!(diff.known.contains(&KnownChange::QuickControls));
    assert!(diff.known.contains(&KnownChange::FxLoopPositions));
    let mut padded = before.raw.records().to_vec();
    padded
        .iter_mut()
        .find(|record| record.command == 0x0f)
        .unwrap()
        .body[2] ^= 55;
    let padded = PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &padded).unwrap()),
        before.source.clone(),
        before.target,
        Revision(2),
    );
    let diff = compare_presets(&before, &padded, &Catalog::embedded().unwrap());
    assert!(!diff.known.contains(&KnownChange::Footswitches));
    assert_eq!(diff.raw.len(), 1);
}

#[test]
fn worker_serializes_new_workflows_with_library_commands_without_second_writer() {
    let root = TempRoot::new("worker");
    let worker = LibraryWorker::open(&root.0, no_wake()).unwrap();
    worker
        .try_submit_workflow(WorkflowCommand {
            intent: IntentId(11),
            action: WorkflowAction::CreateSetlist("Practice".into()),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let created = loop {
        if let Some(event) = worker.try_next_workflow_event() {
            assert_eq!(event.intent, IntentId(11));
            break match event.result.unwrap() {
                WorkflowResult::Setlist(setlist) => setlist,
                other => panic!("{other:?}"),
            };
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    };
    worker
        .try_submit(LibraryCommand::Query {
            intent: IntentId(12),
            query: LibraryQuery::default(),
        })
        .unwrap();
    loop {
        if let Some(event) = worker.try_next_event() {
            assert!(matches!(
                event,
                LibraryEvent::QueryResult {
                    intent: IntentId(12),
                    ..
                }
            ));
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(LibraryStore::open(&root.0).is_err());
    worker.shutdown();
    drop(worker);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match LibraryStore::open(&root.0) {
            Ok(store) => {
                assert_eq!(store.setlist(&created.id).unwrap(), created);
                break;
            }
            Err(error) if error.kind == ErrorKind::Busy => {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("{error}"),
        }
    }
}

#[test]
fn device_archive_hash_mismatch_and_cancellation_publish_no_partial_backup() {
    let root = TempRoot::new("bad-archive");
    let mut source = LibraryStore::open(root.0.join("source")).unwrap();
    let backup = source.begin_device_backup(origin(4)).unwrap();
    source
        .capture_backup_preset(&backup.id, stored(1, 4, 1))
        .unwrap();
    let good = root.0.join("good.gp200backup");
    source.export_device_backup(&backup.id, &good).unwrap();
    let bad = root.0.join("bad.gp200backup");
    rewrite_zip(&good, &bad, |name, bytes| {
        if name.starts_with("blobs/") {
            bytes[0] ^= 1;
        }
    });
    let mut destination = LibraryStore::open(root.0.join("destination")).unwrap();
    let generation = destination.generation();
    assert!(destination.import_device_backup(&bad).is_err());
    assert_eq!(destination.generation(), generation);
    assert!(destination.device_backups().unwrap().is_empty());
    assert_eq!(
        fs::read_dir(destination.root().join("blobs"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        destination
            .import_device_backup_cancellable(&good, &|| true)
            .unwrap_err()
            .kind,
        ErrorKind::Cancelled
    );
    assert_eq!(destination.generation(), generation);
    let cancelled = root.0.join("cancelled.gp200backup");
    assert_eq!(
        source
            .export_device_backup_cancellable(&backup.id, &cancelled, &|| true)
            .unwrap_err()
            .kind,
        ErrorKind::Cancelled
    );
    assert!(!cancelled.exists());
}

#[test]
fn device_archive_rejects_traversal_duplicate_names_and_declared_bombs() {
    let root = TempRoot::new("archive-shape");
    for name in ["../outside", "/absolute", "C:/drive", "a\\b", "a//b"] {
        let path = root.0.join("unsafe.gp200backup");
        let mut writer = ZipWriter::new(File::create(&path).unwrap());
        writer
            .start_file(name, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"{}").unwrap();
        writer.finish().unwrap();
        assert!(
            LibraryStore::preview_device_backup(&path, BundleLimits::default()).is_err(),
            "{name}"
        );
    }
    let path = root.0.join("duplicate.gp200backup");
    let mut writer = ZipWriter::new(File::create(&path).unwrap());
    for name in ["aaaa", "bbbb"] {
        writer
            .start_file(name, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"same").unwrap();
    }
    writer.finish().unwrap();
    let mut bytes = fs::read(&path).unwrap();
    let positions = bytes
        .windows(4)
        .enumerate()
        .filter_map(|(index, value)| (value == b"bbbb").then_some(index))
        .collect::<Vec<_>>();
    for position in positions {
        bytes[position..position + 4].copy_from_slice(b"aaaa");
    }
    fs::write(&path, bytes).unwrap();
    assert!(
        LibraryStore::preview_device_backup(&path, BundleLimits::default())
            .unwrap_err()
            .message
            .contains("Duplicate")
    );
    let path = root.0.join("bomb.gp200backup");
    let mut writer = ZipWriter::new(File::create(&path).unwrap());
    writer
        .start_file(
            "manifest.json",
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )
        .unwrap();
    writer.write_all(&vec![0; 10000]).unwrap();
    writer.finish().unwrap();
    assert!(
        LibraryStore::preview_device_backup(
            &path,
            BundleLimits {
                uncompressed_bytes: 64,
                ..Default::default()
            }
        )
        .is_err()
    );
}

#[test]
fn durable_journal_publication_failure_never_returns_a_dispatch_token() {
    use std::os::unix::fs::symlink;
    let root = TempRoot::new("commit-failure");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let backup = store.begin_device_backup(origin(4)).unwrap();
    store
        .capture_backup_preset(&backup.id, changed(&stored(1, 4, 1), 3, 0, 60f32.to_bits()))
        .unwrap();
    let plan = store
        .plan_restore(&request(&backup.id, 4, &[(1, 252)]))
        .unwrap();
    let journal = review(&mut store, plan);
    let current_manifest = root.0.join("manifest.json");
    let saved = root.0.join("valid-manifest.json");
    fs::rename(&current_manifest, &saved).unwrap();
    symlink(&saved, &current_manifest).unwrap();
    assert!(
        store
            .begin_restore_entry(&journal.id, slot(252), stored(252, 4, 1), current(4))
            .is_err()
    );
    assert_eq!(
        store.restore_journal(&journal.id).unwrap().entries[0].state,
        RestoreEntryState::Planned
    );
    fs::remove_file(&current_manifest).unwrap();
    fs::rename(&saved, &current_manifest).unwrap();
    drop(store);
    let store = LibraryStore::open(&root.0).unwrap();
    assert_eq!(
        store.restore_journal(&journal.id).unwrap().entries[0].state,
        RestoreEntryState::Planned
    );
}

#[test]
fn asset_restore_requires_source_content_and_matching_verified_target_not_metadata() {
    let root = TempRoot::new("asset-mapping");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let backup = store.begin_device_backup(origin(4)).unwrap();
    let source = stored(1, 4, 1);
    let mut records = source.raw.records().to_vec();
    records
        .iter_mut()
        .find(|record| record.command == 0x14 && record.body[0] == 5)
        .unwrap()
        .body[4..8]
        .copy_from_slice(&0x0a100000u32.to_le_bytes());
    let source = Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        source.source.clone(),
        source.target,
        source.revision,
    ));
    store
        .capture_backup_preset(&backup.id, source.clone())
        .unwrap();
    store
        .capture_backup_asset_metadata(
            &backup.id,
            SessionEpoch(4),
            &metadata(DeviceAssetKind::Ir, 0),
        )
        .unwrap();
    let mut request = request(&backup.id, 4, &[(1, 252)]);
    let unknown = store.plan_restore(&request).unwrap();
    assert!(unknown.entries.is_empty());
    assert!(
        unknown.excluded[0]
            .reasons
            .iter()
            .any(|reason| reason.contains("No reviewed content binding"))
    );
    let payload = vec![0; 4096];
    let source_slot = DeviceAssetSlot::new(DeviceAssetKind::Ir, 0).unwrap();
    let target_slot = DeviceAssetSlot::new(DeviceAssetKind::Ir, 5).unwrap();
    let hash = ContentHash::of(&payload);
    request.asset_bindings.push(RestoreAssetBinding {
        source: source_slot,
        target: target_slot,
        content_hash: hash.clone(),
    });
    let still_unknown = store.plan_restore(&request).unwrap();
    assert!(still_unknown.entries.is_empty());
    let evidence = vec![EvidenceRef::new(
        "synthetic verified-content fixture; not a hardware test",
        EvidenceLevel::LiveVerified,
    )];
    store
        .capture_backup_asset_content(
            &backup.id,
            SessionEpoch(4),
            source_slot,
            &payload,
            "device IR coefficients",
            AssetContentOrigin::VerifiedLocalMatch {
                evidence: evidence.clone(),
            },
        )
        .unwrap();
    assert!(store.plan_restore(&request).unwrap().entries.is_empty());
    let profile = ValidationProfileId("synthetic-asset-transfer-fixture".into());
    assert!(
        TargetAssetProof::from_verified_transfer(
            target_slot,
            &origin(4),
            &payload,
            &Capabilities::gp200_read_only(),
            &profile
        )
        .is_err()
    );
    let mut capabilities = Capabilities::empty(DeviceModel::Gp200);
    capabilities.operations.insert(
        OperationKind::IrWrite,
        FeatureSupport::Verified {
            profile: profile.clone(),
            mode: BackendMode::Hardware,
            firmware: None,
            evidence,
        },
    );
    request.target_assets.push(
        TargetAssetProof::from_verified_transfer(
            target_slot,
            &origin(4),
            &payload,
            &capabilities,
            &profile,
        )
        .unwrap(),
    );
    let plan = store.plan_restore(&request).unwrap();
    assert_eq!(plan.entries.len(), 1);
    assert!(plan.excluded.is_empty());
    let journal = review(&mut store, plan);
    let dispatch = store
        .begin_restore_entry(&journal.id, slot(252), stored(252, 4, 1), current(4))
        .unwrap();
    assert_eq!(
        dispatch
            .snapshot
            .view
            .module(ModuleId::new(5).unwrap())
            .unwrap()
            .model,
        ModelCode(0x0a100005)
    );
    let before = source
        .raw
        .records()
        .iter()
        .find(|record| record.command == 0x14 && record.body[0] == 5)
        .unwrap();
    let after = dispatch
        .snapshot
        .raw
        .records()
        .iter()
        .find(|record| record.command == 0x14 && record.body[0] == 5)
        .unwrap();
    assert_eq!(&before.body[..4], &after.body[..4]);
    assert_eq!(&before.body[8..], &after.body[8..]);
}

#[test]
fn fresh_preflight_revision_and_volatile_padding_advance_the_persisted_dispatch_precondition() {
    let root = TempRoot::new("fresh-preflight");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let backup = store.begin_device_backup(origin(4)).unwrap();
    store
        .capture_backup_preset(&backup.id, changed(&stored(1, 4, 1), 3, 0, 60f32.to_bits()))
        .unwrap();
    let plan = store
        .plan_restore(&request(&backup.id, 4, &[(1, 252)]))
        .unwrap();
    let journal = review(&mut store, plan);
    let before = stored(252, 4, 2);
    let mut records = before.raw.records().to_vec();
    for record in &mut records {
        match record.command {
            2 | 8 => record.body[15] ^= 0x55,
            0x0f => {
                record.body[2] ^= 0x33;
                record.body[3] ^= 0x44;
            }
            _ => {}
        }
    }
    let fresh = Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        before.source.clone(),
        before.target,
        before.revision,
    ));
    assert!(
        store
            .begin_restore_entry(
                &journal.id,
                slot(252),
                changed(&fresh, 3, 14, 0x7fc000a5),
                current(4)
            )
            .is_err()
    );
    let mut unknown = records.clone();
    unknown
        .iter_mut()
        .find(|record| record.command == 2)
        .unwrap()
        .body[14] ^= 1;
    let unknown = Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &unknown).unwrap()),
        before.source.clone(),
        before.target,
        Revision(2),
    ));
    assert!(
        store
            .begin_restore_entry(&journal.id, slot(252), unknown, current(4))
            .is_err()
    );
    let dispatch = store
        .begin_restore_entry(&journal.id, slot(252), fresh.clone(), current(4))
        .unwrap();
    assert_eq!(dispatch.expected_target_revision, Revision(2));
    let persisted = store.restore_journal(&journal.id).unwrap();
    assert_eq!(persisted.entries[0].observed_revision, Revision(2));
    let raw = [vec![fresh.raw.kind()], fresh.raw.payload().to_vec()].concat();
    assert_eq!(
        persisted.entries[0].observed_raw_hash,
        ContentHash::of(&raw)
    );
    assert!(
        store
            .confirm_restore_readback(&journal.id, dispatch.snapshot.clone())
            .is_err()
    );
    assert_eq!(
        store
            .confirm_restore_readback(&journal.id, retarget_epoch(&dispatch.snapshot, 4, 3))
            .unwrap()
            .state,
        RestoreRunState::Completed
    );
}

#[test]
fn target_selected_after_review_is_blocked_without_creating_an_in_flight_checkpoint() {
    let root = TempRoot::new("active-after-review");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let backup = store.begin_device_backup(origin(4)).unwrap();
    store
        .capture_backup_preset(&backup.id, stored(1, 4, 1))
        .unwrap();
    let plan = store
        .plan_restore(&request(&backup.id, 4, &[(1, 252)]))
        .unwrap();
    let journal = review(&mut store, plan);
    let selected = stored(252, 4, 2);
    let mut records = selected.raw.records().to_vec();
    records[0].body[..2].copy_from_slice(&0u16.to_le_bytes());
    let buffer = Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        selected.source.clone(),
        Some(PresetTarget::EditBuffer),
        Revision(2),
    ));
    assert!(
        store
            .begin_restore_entry(&journal.id, slot(252), selected, buffer)
            .is_err()
    );
    assert_eq!(
        store.restore_journal(&journal.id).unwrap().entries[0].state,
        RestoreEntryState::Planned
    );
}

#[test]
fn dependency_impact_labels_historical_revisions_and_does_not_hide_unread_bank_slots() {
    let root = TempRoot::new("dependency-impact");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let source = stored(1, 4, 1);
    let mut records = source.raw.records().to_vec();
    records
        .iter_mut()
        .find(|record| record.command == 0x14 && record.body[0] == 5)
        .unwrap()
        .body[4..8]
        .copy_from_slice(&0x0a100000u32.to_le_bytes());
    let source = Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        source.source.clone(),
        source.target,
        source.revision,
    ));
    let first = store
        .store_snapshot(
            source.clone(),
            LibraryMetadata {
                name: "User cab".into(),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let second = store
        .store_snapshot(
            source.clone(),
            LibraryMetadata {
                name: "User cab revision".into(),
                ..Default::default()
            },
            Some(first.clone()),
        )
        .unwrap();
    let backup = store.begin_device_backup(origin(4)).unwrap();
    store.capture_backup_preset(&backup.id, source).unwrap();
    let slot = DeviceAssetSlot::new(DeviceAssetKind::Ir, 0).unwrap();
    let impact = store.asset_replacement_impact(slot).unwrap();
    assert_eq!(impact.uses.len(), 3);
    assert_eq!(impact.missing_backup_slots.len(), 255);
    assert!(impact.requires_hardware_revalidation);
    assert!(impact.uses.iter().any(|usage|matches!(&usage.owner,AssetDependencyOwner::LibraryRevision{version,latest:false,..} if version==&first)));
    assert!(impact.uses.iter().any(|usage|matches!(&usage.owner,AssetDependencyOwner::LibraryRevision{version,latest:true,..} if version==&second)));
    assert!(impact.uses.iter().all(|usage| usage.content_hash.is_none()));
    assert_eq!(store.referenced_device_asset_slots().unwrap(), vec![slot]);
}
