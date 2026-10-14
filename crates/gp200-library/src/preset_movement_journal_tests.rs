use super::*;
use std::{
    cell::RefCell,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
#[allow(dead_code)]
#[path = "fixtures/movement-legacy-reader-v1.rs"]
mod legacy;
thread_local! { static FAULT: RefCell<Option<(PathBuf,&'static str)>> = const { RefCell::new(None) }; }
pub(crate) fn publication_boundary(path: &Path, stage: &'static str) -> Result<(), ServiceError> {
    FAULT.with(|f| {
        let mut f = f.borrow_mut();
        if f.as_ref().is_some_and(|(p, s)| p == path && *s == stage) {
            f.take();
            Err(io(format!("injected publication failure at {stage}")))
        } else {
            Ok(())
        }
    })
}
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "gp200-protected-movement-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        FAULT.with(|f| f.borrow_mut().take());
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn slot(n: u16) -> SlotId {
    SlotId::new(n).unwrap()
}
fn origin(epoch: u64) -> BackupOrigin {
    BackupOrigin {
        identity: DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Hardware,
            firmware: None,
            display_name: "Synthetic GP-200; no device session".into(),
        },
        epoch: SessionEpoch(epoch),
        device_key: "movement-synthetic-fixture".into(),
        evidence: vec![],
    }
}
fn profile() -> ValidationProfileId {
    ValidationProfileId("synthetic-full-stored-no-hardware-proof".into())
}
fn caps() -> Capabilities {
    let mut c = Capabilities::gp200_read_only();
    c.operations.insert(
        OperationKind::PresetImport,
        FeatureSupport::Verified {
            profile: profile(),
            mode: BackendMode::Hardware,
            firmware: None,
            evidence: vec![],
        },
    );
    c
}
fn current() -> Arc<PresetSnapshot> {
    let raw = RawTransfer::from_payload(
        0x12,
        include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
    )
    .unwrap();
    let mut records = raw.records().to_vec();
    for r in &mut records {
        if r.command == 0 {
            r.body[..2].copy_from_slice(&0u16.to_le_bytes());
            r.body[2..4].copy_from_slice(&23u16.to_le_bytes());
        } else if matches!(r.command, 2 | 8) {
            r.body[..2].copy_from_slice(&23u16.to_le_bytes());
        }
    }
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        source_for(&origin(4)),
        Some(PresetTarget::EditBuffer),
        Revision(1),
    ))
}
fn stored(n: u16) -> Arc<PresetSnapshot> {
    let wire = full_stored_write_request(n, &current().raw).unwrap();
    let mut r = vec![wire.records()[0].clone()];
    r.extend_from_slice(&wire.records()[2..]);
    let name = r.iter_mut().find(|r| r.command == 2).unwrap();
    name.body[16..32].fill(0);
    name.body[16..20].copy_from_slice(&[b'S', n as u8, 0x55, 0]);
    r.iter_mut()
        .find(|r| r.command == 0x14 && r.body[0] == 3)
        .unwrap()
        .body[64..68]
        .copy_from_slice(&(0x7fc0_0000 | u32::from(n)).to_le_bytes());
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &r).unwrap()),
        source_for(&origin(4)),
        Some(PresetTarget::Stored(slot(n))),
        Revision(1),
    ))
}
fn globals(epoch: u64, revision: u64) -> Arc<DeviceGlobalState> {
    Arc::new(
        DeviceGlobalState::from_transfer(
            Revision(revision),
            SessionEpoch(epoch),
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
fn at(s: &PresetSnapshot, epoch: u64, revision: u64) -> Arc<PresetSnapshot> {
    let mut s = s.clone();
    s.source = source_for(&origin(epoch));
    s.revision = Revision(revision);
    Arc::new(s)
}
fn change(s: &PresetSnapshot, command: u16, offset: usize) -> Arc<PresetSnapshot> {
    let mut r = s.raw.records().to_vec();
    let row = r.iter_mut().find(|r| r.command == command).unwrap();
    row.body[offset] ^= 0x40;
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(s.raw.kind(), &r).unwrap()),
        s.source.clone(),
        s.target,
        s.revision,
    ))
}
fn round(
    bank: &BTreeMap<SlotId, Arc<PresetSnapshot>>,
    epoch: u64,
    revision: u64,
) -> PresetMovementObserved {
    PresetMovementObserved {
        origin: origin(epoch),
        current_buffer: at(&current(), epoch, revision),
        globals: globals(epoch, revision),
        stored: bank
            .iter()
            .map(|(s, p)| (*s, at(p, epoch, revision)))
            .collect(),
        capabilities: caps(),
        expected_profile: profile(),
    }
}
struct Fixture {
    temp: Temp,
    store: LibraryStore,
    execution: MovementExecutionReview,
    bank: BTreeMap<SlotId, Arc<PresetSnapshot>>,
}
impl Fixture {
    fn new() -> Self {
        let temp = Temp::new();
        let mut store = LibraryStore::open(&temp.0).unwrap();
        let bank = [253, 254, 255]
            .map(|n| (slot(n), stored(n)))
            .into_iter()
            .collect();
        let observed = round(&bank, 4, 1);
        let backup = store.begin_device_backup(observed.origin.clone()).unwrap();
        store
            .capture_backup_preset(&backup.id, observed.current_buffer.clone())
            .unwrap();
        store
            .capture_backup_globals(&backup.id, &observed.globals)
            .unwrap();
        for p in observed.stored.values() {
            store.capture_backup_preset(&backup.id, p.clone()).unwrap();
        }
        store
            .finish_device_backup(&backup.id, BackupAcquisition::Finished)
            .unwrap();
        let request = crate::PresetMovementRequest {
            movement: PresetMovementPlan::to_destination(
                DeviceModel::Gp200,
                slot(254),
                slot(255),
                Some(slot(23)),
            )
            .unwrap(),
            before_backup: backup.id,
            observed,
            asset_bindings: vec![],
            target_assets: vec![],
        };
        let crate::PresetMovementPreparation::Ready(prepared) =
            store.prepare_preset_movement(&request).unwrap()
        else {
            panic!("fixture must be wholly applicable");
        };
        let hash = prepared.review_hash().unwrap();
        let execution = store
            .bind_preset_movement_execution(&prepared, hash)
            .unwrap();
        Self {
            temp,
            store,
            execution,
            bank,
        }
    }
    fn begin(&mut self) -> MovementJournal {
        self.store
            .begin_preset_movement(
                &self.execution,
                self.execution.execution_hash().clone(),
                round(&self.bank, 4, 2),
            )
            .unwrap()
    }
    fn dispatch(&mut self, j: &MovementJournal, epoch: u64, revision: u64) -> MovementDispatch {
        self.store
            .begin_preset_movement_entry(&j.id, j.sequence, round(&self.bank, epoch, revision))
            .unwrap()
    }
    fn confirm(&mut self, d: &MovementDispatch, epoch: u64, revision: u64) -> MovementJournal {
        let actual = at(d.snapshot(), epoch, revision);
        self.bank.insert(d.target(), actual.clone());
        self.store
            .confirm_preset_movement_entry(
                d.journal(),
                d.sequence(),
                actual,
                round(&self.bank, epoch, revision + 1),
            )
            .unwrap()
    }
}
fn disk_journal(root: &Path, id: &str) -> MovementJournal {
    let v: Value = serde_json::from_slice(&fs::read(root.join("manifest.json")).unwrap()).unwrap();
    serde_json::from_value(v["manifest"]["extensions"][MOVEMENT_EXTENSION]["records"][id].clone())
        .unwrap()
}
fn write_envelope(root: &Path, manifest: &Manifest) {
    let bytes = serde_json::to_vec(manifest).unwrap();
    let value = json!({"format":"gp200-studio.library","sha256":ContentHash::of(&bytes),"manifest":manifest});
    fs::write(
        root.join("manifest.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
}
#[test]
fn immutable_preimage_and_policy_match_the_unchanged_review() {
    let f = Fixture::new();
    assert_eq!(
        ContentHash::of(&f.execution.prepared_bytes),
        f.execution.prepared().review_hash().unwrap()
    );
    assert_eq!(
        movement_policy_hash().as_str(),
        "90d476fb6fb1726e3821bcf68d26613451768c898ae6ca7fc6fc2b37119030df"
    );
    assert!(f.store.movement_journals().unwrap().is_empty());
}
#[test]
fn embedded_begin_and_dispatch_are_single_generation_and_private() {
    let mut f = Fixture::new();
    let generation = f.store.generation();
    let j = f.begin();
    assert_eq!(f.store.generation(), generation + 1);
    assert!(f.store.restore_journals().unwrap().is_empty());
    let private = &j.restore.id;
    assert!(f.store.restore_journal(private).is_err());
    assert!(
        f.store
            .begin_restore_entry(
                private,
                slot(254),
                at(&stored(254), 4, 3),
                at(&current(), 4, 3)
            )
            .is_err()
    );
    assert!(
        f.store
            .confirm_restore_readback(private, at(&stored(254), 4, 4))
            .is_err()
    );
    assert!(f.store.pause_restore(private, "stop").is_err());
    assert!(
        f.store
            .reconcile_restore(private, origin(4), BTreeMap::new())
            .is_err()
    );
    assert!(
        f.store
            .approve_restore_retry(private, slot(254), j.original_slots[&slot(254)].clone())
            .is_err()
    );
    let d = f.dispatch(&j, 4, 3);
    assert_eq!(f.store.generation(), generation + 2);
    let disk = disk_journal(&f.temp.0, &j.id);
    assert_eq!(disk.state, MovementRunState::InFlight { target: slot(254) });
    assert_eq!(disk.restore.entries[0].state, RestoreEntryState::InFlight);
    assert_eq!(disk.restore.entries[0].attempts, 1);
    assert_eq!(d.sequence(), disk.sequence);
}
#[test]
fn whole_swap_uses_immutable_zip_originals_after_first_overwrite() {
    let mut f = Fixture::new();
    let j = f.begin();
    let d = f.dispatch(&j, 4, 3);
    let j = f.confirm(&d, 4, 4);
    assert_eq!(j.restore.verified_count(), 1);
    let d = f.dispatch(&j, 4, 6);
    assert_eq!(d.target(), slot(255));
    let archive = f.temp.0.join("movement-before").join(&j.archive_basename);
    let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
    let backup = f.store.device_backup(&j.original_backup).unwrap();
    let hash = &backup.presets[&slot(254)].raw_hash;
    let mut bytes = vec![];
    zip.by_name(&format!("blobs/{}", hash.as_str()))
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(ContentHash::of(&bytes), *hash);
    let original = parse_raw(&bytes).unwrap();
    let wire = full_stored_write_request(255, &original).unwrap();
    let mut records = vec![wire.records()[0].clone()];
    records.extend_from_slice(&wire.records()[2..]);
    let expected = RawTransfer::from_records(d.snapshot().raw.kind(), &records).unwrap();
    assert_eq!(raw_bytes(&expected), raw_bytes(&d.snapshot().raw));
    assert_ne!(raw_bytes(&f.bank[&slot(254)].raw), bytes);
    let j = f.confirm(&d, 4, 7);
    assert!(j.is_completed());
    drop(f.store);
    let reopened = LibraryStore::open(&f.temp.0).unwrap();
    assert!(reopened.movement_journal(&j.id).unwrap().is_completed());
    assert!(reopened.restore_journals().unwrap().is_empty());
}
#[test]
fn stale_missing_wrong_profile_and_mixed_epoch_rounds_never_dispatch() {
    for case in 0..5 {
        let mut f = Fixture::new();
        let j = f.begin();
        let mut fresh = round(&f.bank, 4, 3);
        match case {
            0 => fresh.current_buffer = at(&current(), 4, 2),
            1 => {
                fresh.stored.remove(&slot(253));
            }
            2 => fresh.expected_profile = ValidationProfileId("wrong".into()),
            3 => fresh.globals = globals(5, 3),
            _ => fresh
                .stored
                .insert(slot(254), at(&stored(254), 5, 3))
                .map(|_| ())
                .unwrap(),
        }
        assert!(
            f.store
                .begin_preset_movement_entry(&j.id, j.sequence, fresh)
                .is_err()
        );
        let saved = f.store.movement_journal(&j.id).unwrap();
        assert!(matches!(
            saved.state,
            MovementRunState::PausedReconcileRequired { .. }
        ));
        assert_eq!(saved.restore.entries[0].attempts, 0);
    }
}
#[test]
fn current_sentinel_untouched_and_runtime_changes_pause_with_exact_raw_evidence() {
    for case in 0..4 {
        let mut f = Fixture::new();
        let j = f.begin();
        let mut fresh = round(&f.bank, 4, 3);
        let object = match case {
            0 => {
                fresh.current_buffer = change(&fresh.current_buffer, 2, 16);
                MovementObject::Current
            }
            1 => {
                fresh
                    .stored
                    .insert(slot(253), change(&fresh.stored[&slot(253)], 2, 16));
                MovementObject::Stored(slot(253))
            }
            2 => {
                fresh
                    .stored
                    .insert(slot(255), change(&fresh.stored[&slot(255)], 2, 16));
                MovementObject::Stored(slot(255))
            }
            _ => {
                let mut r = fresh.globals.records().raw().records().to_vec();
                r.iter_mut().find(|r| r.command == 0x1000).unwrap().body[0] ^= 1;
                fresh.globals = Arc::new(
                    DeviceGlobalState::from_transfer(
                        Revision(3),
                        SessionEpoch(4),
                        RawTransfer::from_records(0x12, &r).unwrap(),
                        vec![],
                    )
                    .unwrap(),
                );
                MovementObject::Globals
            }
        };
        assert!(
            f.store
                .begin_preset_movement_entry(&j.id, j.sequence, fresh)
                .is_err()
        );
        let saved = f.store.movement_journal(&j.id).unwrap();
        assert_eq!(saved.failed_rounds.len(), 1);
        let report = f
            .store
            .movement_round_differences(&j.id, &saved.failed_rounds[0], object)
            .unwrap();
        assert!(!report.preserved_under_policy);
        assert!(!report.differences.is_empty());
        assert!(report.differences.iter().any(|d| !d.excluded_by_policy));
    }
}
#[test]
fn existing_padding_exclusions_pass_but_both_raw_frames_and_differences_remain() {
    let mut f = Fixture::new();
    let j = f.begin();
    let mut fresh = round(&f.bank, 4, 3);
    fresh.current_buffer = change(&fresh.current_buffer, 2, 15);
    fresh
        .stored
        .insert(slot(253), change(&fresh.stored[&slot(253)], 8, 15));
    let d = f
        .store
        .begin_preset_movement_entry(&j.id, j.sequence, fresh)
        .unwrap();
    for object in [MovementObject::Current, MovementObject::Stored(slot(253))] {
        let report = f
            .store
            .movement_round_differences(&j.id, d.preflight_hash(), object)
            .unwrap();
        assert!(report.preserved_under_policy);
        assert_ne!(report.reference_raw_hash, report.actual_raw_hash);
        assert_eq!(report.differences.len(), 1);
        assert!(report.differences[0].excluded_by_policy);
    }
}
#[test]
fn postflight_drift_and_stale_actor_readback_do_not_confirm_or_dispatch_next() {
    for stale in [false, true] {
        let mut f = Fixture::new();
        let j = f.begin();
        let d = f.dispatch(&j, 4, 3);
        let actual = at(d.snapshot(), 4, if stale { 3 } else { 4 });
        f.bank.insert(d.target(), actual.clone());
        let mut post = round(&f.bank, 4, 5);
        if !stale {
            post.current_buffer = change(&post.current_buffer, 2, 16);
        }
        assert!(
            f.store
                .confirm_preset_movement_entry(&j.id, d.sequence(), actual, post)
                .is_err()
        );
        let saved = f.store.movement_journal(&j.id).unwrap();
        assert!(!saved.is_completed());
        assert!(matches!(
            saved.restore.entries[0].state,
            RestoreEntryState::Uncertain { .. }
        ));
        assert_eq!(saved.restore.verified_count(), 0);
        assert_eq!(saved.failed_actor_readbacks.len(), 1);
        assert!(
            f.store
                .begin_preset_movement_entry(&j.id, saved.sequence, round(&f.bank, 4, 6))
                .is_err()
        );
    }
}
#[test]
fn ready_inflight_and_between_steps_reopen_paused_without_ordinary_replay() {
    for checkpoint in 0..3 {
        let mut f = Fixture::new();
        let mut j = f.begin();
        if checkpoint > 0 {
            let d = f.dispatch(&j, 4, 3);
            if checkpoint == 2 {
                j = f.confirm(&d, 4, 4);
            }
        }
        drop(f.store);
        let mut reopened = LibraryStore::open(&f.temp.0).unwrap();
        let saved = reopened.movement_journal(&j.id).unwrap();
        assert!(matches!(
            saved.state,
            MovementRunState::PausedReconcileRequired { .. }
        ));
        assert!(reopened.restore_journals().unwrap().is_empty());
        assert!(
            reopened
                .begin_preset_movement_entry(&j.id, saved.sequence, round(&f.bank, 4, 8))
                .is_err()
        );
        if checkpoint == 1 {
            assert!(matches!(
                saved.restore.entries[0].state,
                RestoreEntryState::Uncertain { .. }
            ));
        }
        if checkpoint == 2 {
            assert_eq!(saved.restore.verified_count(), 1);
        }
    }
}
#[test]
fn lost_ack_expected_reconciles_without_replay_and_retry_needs_exact_review_set() {
    for delivered in [false, true] {
        let mut f = Fixture::new();
        let j = f.begin();
        let d = f.dispatch(&j, 4, 3);
        if delivered {
            f.bank.insert(d.target(), at(d.snapshot(), 4, 4));
        }
        let paused = f
            .store
            .pause_preset_movement(&j.id, d.sequence(), "lost acknowledgement")
            .unwrap();
        let report = f
            .store
            .reconcile_preset_movement(&j.id, paused.sequence, round(&f.bank, 4, 5))
            .unwrap();
        assert_eq!(
            report.rows[&slot(254)],
            if delivered {
                MovementReconciledSlot::Expected
            } else {
                MovementReconciledSlot::NeedsRetryReview
            }
        );
        let report_hash = report.review_hash().unwrap();
        assert_eq!(
            f.store.movement_journal(&j.id).unwrap().state,
            MovementRunState::PausedReconciled {
                report_hash: report_hash.clone()
            }
        );
        if !delivered {
            assert!(
                f.store
                    .continue_preset_movement(
                        &j.id,
                        report_hash.clone(),
                        BTreeSet::new(),
                        round(&f.bank, 4, 6)
                    )
                    .is_err()
            );
        }
        assert!(
            f.store
                .continue_preset_movement(
                    &j.id,
                    report_hash.clone(),
                    BTreeSet::from([slot(255)]),
                    round(&f.bank, 4, 6)
                )
                .is_err()
        );
        let retry = if delivered {
            BTreeSet::new()
        } else {
            BTreeSet::from([slot(254)])
        };
        assert!(
            f.store
                .continue_preset_movement(
                    &j.id,
                    report_hash.clone(),
                    retry.clone(),
                    round(&f.bank, 4, 5)
                )
                .is_err()
        );
        let ready = f
            .store
            .continue_preset_movement(&j.id, report_hash, retry, round(&f.bank, 4, 6))
            .unwrap();
        let d = f.dispatch(&ready, 4, 7);
        assert_eq!(d.target(), if delivered { slot(255) } else { slot(254) });
    }
}
#[test]
fn new_epoch_reconcile_keeps_original_bytes_and_rebinds_only_ephemeral_projection() {
    let mut f = Fixture::new();
    let j = f.begin();
    let d = f.dispatch(&j, 4, 3);
    f.bank.insert(d.target(), at(d.snapshot(), 4, 4));
    let paused = f
        .store
        .pause_preset_movement(&j.id, d.sequence(), "disconnected")
        .unwrap();
    let original_plan =
        fs::read(f.temp.0.join("blobs").join(j.restore_plan_hash.as_str())).unwrap();
    let report = f
        .store
        .reconcile_preset_movement(&j.id, paused.sequence, round(&f.bank, 9, 1))
        .unwrap();
    let ready = f
        .store
        .continue_preset_movement(
            &j.id,
            report.review_hash().unwrap(),
            BTreeSet::new(),
            round(&f.bank, 9, 2),
        )
        .unwrap();
    assert_eq!(ready.original_origin.epoch, SessionEpoch(4));
    assert_eq!(ready.live_origin.epoch, SessionEpoch(9));
    assert_eq!(
        fs::read(f.temp.0.join("blobs").join(j.restore_plan_hash.as_str())).unwrap(),
        original_plan
    );
    let d = f.dispatch(&ready, 9, 3);
    assert_eq!(d.epoch(), SessionEpoch(9));
    assert_eq!(
        d.snapshot().source,
        SnapshotSource::Hardware {
            epoch: SessionEpoch(9)
        }
    );
    assert_eq!(
        ContentHash::of(&raw_bytes(&d.snapshot().raw)),
        *d.expected_raw_hash()
    );
}
#[test]
fn third_state_reconciliation_remains_paused_and_no_retry_can_bypass_conflict() {
    let mut f = Fixture::new();
    let j = f.begin();
    let d = f.dispatch(&j, 4, 3);
    let paused = f
        .store
        .pause_preset_movement(&j.id, d.sequence(), "uncertain")
        .unwrap();
    f.bank.insert(slot(254), change(&f.bank[&slot(254)], 2, 18));
    let report = f
        .store
        .reconcile_preset_movement(&j.id, paused.sequence, round(&f.bank, 4, 4))
        .unwrap();
    assert_eq!(report.rows[&slot(254)], MovementReconciledSlot::Conflict);
    assert!(
        f.store
            .continue_preset_movement(
                &j.id,
                report.review_hash().unwrap(),
                BTreeSet::new(),
                round(&f.bank, 4, 5)
            )
            .is_err()
    );
    assert_eq!(
        f.store.movement_journal(&j.id).unwrap().restore.entries[0].attempts,
        1
    );
}
#[test]
fn current_candidate_is_validated_independently_of_the_in_memory_manifest() {
    let mut f = Fixture::new();
    let j = f.begin();
    let mut candidate = f.store.manifest.clone();
    let mut workflows = decode_workflows(&candidate).unwrap();
    workflows
        .backups
        .get_mut(&j.original_backup)
        .unwrap()
        .origin
        .device_key = "different candidate".into();
    candidate.extensions.insert(
        crate::workflows::EXTENSION.into(),
        serde_json::to_value(workflows).unwrap(),
    );
    assert!(f.store.validate_movement_manifest(&candidate).is_err());
    assert!(
        f.store
            .validate_movement_manifest(&f.store.manifest)
            .is_ok()
    );
}
#[test]
fn archive_and_source_corruption_between_steps_block_dispatch_and_stale_commits() {
    for archive in [false, true] {
        let mut f = Fixture::new();
        let j = f.begin();
        let d = f.dispatch(&j, 4, 3);
        let j = f.confirm(&d, 4, 4);
        let path = if archive {
            f.temp.0.join("movement-before").join(&j.archive_basename)
        } else {
            f.temp
                .0
                .join("blobs")
                .join(j.original_slots[&slot(254)].as_str())
        };
        let original = fs::read(&path).unwrap();
        fs::write(&path, b"corrupt preserved input").unwrap();
        assert!(
            f.store
                .begin_preset_movement_entry(&j.id, j.sequence, round(&f.bank, 4, 6))
                .is_err()
        );
        assert!(f.store.movement_publication_uncertain);
        assert!(f.store.create_setlist("must not erase movement").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"corrupt preserved input");
        fs::write(&path, original).unwrap();
        drop(f.store);
        let reopened = LibraryStore::open(&f.temp.0).unwrap();
        assert!(matches!(
            reopened.movement_journal(&j.id).unwrap().state,
            MovementRunState::PausedReconcileRequired { .. }
        ));
    }
}
#[test]
fn consistently_rehashed_expected_payload_still_must_derive_from_archived_source() {
    let mut f = Fixture::new();
    let j = f.begin();
    let mut forged = j.clone();
    let mut plan: RestorePlan =
        serde_json::from_slice(&f.store.read_blob(j.restore_plan_hash.as_str()).unwrap()).unwrap();
    let original = decode_snapshot(
        &f.store
            .read_blob(plan.entries[0].expected_snapshot_hash.as_str())
            .unwrap(),
    )
    .unwrap();
    let changed = change(&original, 2, 18);
    let e = &mut plan.entries[0];
    e.expected_snapshot_hash = ContentHash::new(
        f.store
            .publish_blob(&serde_json::to_vec(changed.as_ref()).unwrap())
            .unwrap(),
    )
    .unwrap();
    e.expected_raw_hash =
        ContentHash::new(f.store.publish_blob(&raw_bytes(&changed.raw)).unwrap()).unwrap();
    forged.restore.entries[0].plan = e.clone();
    forged.restore_plan_hash = ContentHash::new(
        f.store
            .publish_blob(&serde_json::to_vec(&plan).unwrap())
            .unwrap(),
    )
    .unwrap();
    forged.restore.plan_hash = forged.restore_plan_hash.clone();
    let mut preimage: Value =
        serde_json::from_slice(&f.store.read_blob(j.prepared_hash.as_str()).unwrap()).unwrap();
    preimage["restore"] = serde_json::to_value(plan).unwrap();
    forged.prepared_hash = ContentHash::new(
        f.store
            .publish_blob(&serde_json::to_vec(&preimage).unwrap())
            .unwrap(),
    )
    .unwrap();
    forged.execution_hash = execution_hash(
        &forged.prepared_hash,
        &forged.restore_plan_hash,
        &forged.archive_hash,
        &forged.descriptor_hash,
        &forged.profile,
    )
    .unwrap();
    let error = f
        .store
        .protection(&f.store.manifest, &forged)
        .err()
        .unwrap();
    assert!(
        error.message.contains("immutable original source"),
        "{}",
        error.message
    );
}
#[test]
fn forged_completion_without_complete_expected_round_is_rejected() {
    let mut f = Fixture::new();
    let j = f.begin();
    let mut extension = f.store.movement_extension().unwrap();
    let row = extension.records.get_mut(&j.id).unwrap();
    for e in &mut row.restore.entries {
        e.state = RestoreEntryState::Verified {
            readback_raw_hash: e.plan.expected_raw_hash.clone(),
        };
    }
    row.restore.state = RestoreRunState::Completed;
    row.state = MovementRunState::Completed {
        final_round_hash: row.last_round.clone().unwrap(),
    };
    let mut candidate = f.store.manifest.clone();
    candidate.extensions.insert(
        MOVEMENT_EXTENSION.into(),
        serde_json::to_value(extension).unwrap(),
    );
    assert!(
        f.store
            .validate_movement_manifest(&candidate)
            .unwrap_err()
            .message
            .contains("complete final preserved round")
    );
}
#[test]
fn every_manifest_publication_boundary_returns_no_checkpoint_and_poisons_stale_writes() {
    for initial in [false, true] {
        for file in ["manifest.previous.json", "manifest.json"] {
            for stage in [
                "before-create",
                "after-write",
                "after-file-sync",
                "after-rename",
                "after-directory-sync",
            ] {
                let mut f = Fixture::new();
                let j = if initial { None } else { Some(f.begin()) };
                FAULT.with(|h| *h.borrow_mut() = Some((f.temp.0.join(file), stage)));
                if let Some(j) = &j {
                    assert!(
                        f.store
                            .begin_preset_movement_entry(&j.id, j.sequence, round(&f.bank, 4, 3))
                            .is_err()
                    );
                } else {
                    assert!(
                        f.store
                            .begin_preset_movement(
                                &f.execution,
                                f.execution.execution_hash().clone(),
                                round(&f.bank, 4, 2)
                            )
                            .is_err()
                    );
                }
                assert!(f.store.movement_publication_uncertain);
                let disk = fs::read(f.temp.0.join("manifest.json")).unwrap();
                assert!(f.store.create_setlist("stale unrelated write").is_err());
                assert_eq!(fs::read(f.temp.0.join("manifest.json")).unwrap(), disk);
                drop(f.store);
                let reopened = LibraryStore::open(&f.temp.0).unwrap();
                for j in reopened.movement_journals().unwrap() {
                    assert!(matches!(
                        j.state,
                        MovementRunState::PausedReconcileRequired { .. }
                    ));
                    assert!(reopened.restore_journal(&j.restore.id).is_err());
                }
            }
        }
    }
}
#[test]
fn old_reader_preserves_opaque_extension_and_never_discovers_private_restore() {
    let mut f = Fixture::new();
    let j = f.begin();
    let reader = legacy::Reader { store: &f.store };
    let old = reader
        .read_manifest(&f.temp.0.join("manifest.json"))
        .unwrap();
    assert!(legacy::decode_workflows(&old).unwrap().restores.is_empty());
    let value = old.extensions[MOVEMENT_EXTENSION].clone();
    let rewritten = reader.rewrite_preserving(old).unwrap();
    assert_eq!(rewritten.extensions[MOVEMENT_EXTENSION], value);
    let reread = reader
        .read_manifest(&f.temp.0.join("manifest.json"))
        .unwrap();
    assert_eq!(reread.extensions[MOVEMENT_EXTENSION], value);
    drop(f.store);
    let reopened = LibraryStore::open(&f.temp.0).unwrap();
    assert!(matches!(
        reopened.movement_journal(&j.id).unwrap().state,
        MovementRunState::PausedReconcileRequired { .. }
    ));
}
#[test]
fn corrupt_current_fallback_ready_inflight_and_absent_records_never_reconstruct_authority() {
    for previous in 0..3 {
        let mut f = Fixture::new();
        let j = f.begin();
        if previous == 2 {
            f.dispatch(&j, 4, 3);
        }
        if previous > 0 {
            f.store
                .create_setlist("publish previous movement checkpoint")
                .unwrap();
        }
        fs::write(f.temp.0.join("manifest.json"), b"truncated-current").unwrap();
        let old = legacy::Reader { store: &f.store };
        assert!(old.read_manifest(&f.temp.0.join("manifest.json")).is_err());
        let (old_previous, used_fallback) = old.read_with_fallback().unwrap();
        assert!(used_fallback);
        assert!(
            legacy::decode_workflows(&old_previous)
                .unwrap()
                .restores
                .is_empty()
        );
        assert_eq!(
            old_previous.extensions.contains_key(MOVEMENT_EXTENSION),
            previous > 0
        );
        drop(f.store);
        let reopened = LibraryStore::open(&f.temp.0).unwrap();
        assert!(reopened.recovery_note.is_some());
        if previous == 0 {
            assert!(reopened.movement_journals().unwrap().is_empty());
            assert_eq!(
                fs::read_dir(f.temp.0.join("movement-before"))
                    .unwrap()
                    .count(),
                1
            );
        } else {
            let j = reopened.movement_journal(&j.id).unwrap();
            assert!(matches!(
                j.state,
                MovementRunState::PausedReconcileRequired { .. }
            ));
            if previous == 2 {
                assert!(matches!(
                    j.restore.entries[0].state,
                    RestoreEntryState::Uncertain { .. }
                ));
            }
        }
    }
}
#[test]
fn future_schema_does_not_fall_back_and_extension_bounds_are_checked() {
    let mut f = Fixture::new();
    let j = f.begin();
    let mut candidate = f.store.manifest.clone();
    candidate.extensions.get_mut(MOVEMENT_EXTENSION).unwrap()["schema_version"] = json!(2);
    write_envelope(&f.temp.0, &candidate);
    drop(f.store);
    assert_eq!(
        LibraryStore::open(&f.temp.0).err().unwrap().kind,
        ErrorKind::Unsupported
    );
    candidate.extensions.get_mut(MOVEMENT_EXTENSION).unwrap()["schema_version"] = json!(1);
    candidate.extensions.get_mut(MOVEMENT_EXTENSION).unwrap()["oversized"] =
        json!("x".repeat(MAX_EXTENSION));
    assert!(
        decode_movement_extension(&candidate)
            .unwrap_err()
            .message
            .contains("8 MiB")
    );
    let _ = j;
}
#[test]
fn worker_cancellation_after_begin_or_dispatch_durably_pauses_and_returns_no_token() {
    for dispatch in [false, true] {
        let mut f = Fixture::new();
        let action = if dispatch {
            let j = f.begin();
            crate::WorkflowAction::DispatchMovement {
                journal: j.id,
                sequence: j.sequence,
                fresh: Box::new(round(&f.bank, 4, 3)),
            }
        } else {
            crate::WorkflowAction::BeginMovement {
                execution: Box::new(f.execution.clone()),
                reviewed: f.execution.execution_hash().clone(),
                fresh: Box::new(round(&f.bank, 4, 2)),
            }
        };
        let calls = std::cell::Cell::new(0);
        let cancelled = || {
            let n = calls.get();
            calls.set(n + 1);
            n > 0
        };
        assert_eq!(
            crate::workflow_worker::execute_workflow(&mut f.store, action, &cancelled)
                .err()
                .unwrap()
                .kind,
            ErrorKind::Cancelled
        );
        let j = f.store.movement_journals().unwrap().pop().unwrap();
        assert!(matches!(
            j.state,
            MovementRunState::PausedReconcileRequired { .. }
        ));
        if dispatch {
            assert!(matches!(
                j.restore.entries[0].state,
                RestoreEntryState::Uncertain { .. }
            ));
        }
    }
}

fn bind_changed_snapshot(
    f: &Fixture,
    j: &MovementJournal,
    expected: bool,
    mutate: impl FnOnce(&mut PresetSnapshot),
) -> MovementJournal {
    let mut forged = j.clone();
    let mut plan: RestorePlan =
        serde_json::from_slice(&f.store.read_blob(j.restore_plan_hash.as_str()).unwrap()).unwrap();
    let e = &mut plan.entries[0];
    let hash = if expected {
        &e.expected_snapshot_hash
    } else {
        &e.before_snapshot_hash
    };
    let mut snapshot = decode_snapshot(&f.store.read_blob(hash.as_str()).unwrap()).unwrap();
    mutate(&mut snapshot);
    let hash = ContentHash::new(
        f.store
            .publish_blob(&serde_json::to_vec(&snapshot).unwrap())
            .unwrap(),
    )
    .unwrap();
    if expected {
        e.expected_snapshot_hash = hash;
    } else {
        e.before_snapshot_hash = hash;
    }
    forged.restore.entries[0].plan = e.clone();
    forged.restore_plan_hash = ContentHash::new(
        f.store
            .publish_blob(&serde_json::to_vec(&plan).unwrap())
            .unwrap(),
    )
    .unwrap();
    forged.restore.plan_hash = forged.restore_plan_hash.clone();
    let mut preimage: Value =
        serde_json::from_slice(&f.store.read_blob(j.prepared_hash.as_str()).unwrap()).unwrap();
    preimage["restore"] = serde_json::to_value(plan).unwrap();
    forged.prepared_hash = ContentHash::new(
        f.store
            .publish_blob(&serde_json::to_vec(&preimage).unwrap())
            .unwrap(),
    )
    .unwrap();
    forged.execution_hash = execution_hash(
        &forged.prepared_hash,
        &forged.restore_plan_hash,
        &forged.archive_hash,
        &forged.descriptor_hash,
        &forged.profile,
    )
    .unwrap();
    forged
}
#[test]
fn archived_before_and_expected_sources_cannot_be_rehashed_to_a_new_session_or_revision() {
    let mut f = Fixture::new();
    let j = f.begin();
    for expected in [false, true] {
        let forged = bind_changed_snapshot(&f, &j, expected, |s| {
            s.source = SnapshotSource::Hardware {
                epoch: SessionEpoch(9),
            }
        });
        assert!(f.store.protection(&f.store.manifest, &forged).is_err());
    }
    let forged = bind_changed_snapshot(&f, &j, true, |s| s.revision = Revision(99));
    assert!(
        f.store
            .protection(&f.store.manifest, &forged)
            .err()
            .unwrap()
            .message
            .contains("revision changed")
    );
    let forged = bind_changed_snapshot(&f, &j, true, |s| {
        s.target = Some(PresetTarget::Stored(slot(253)))
    });
    assert!(f.store.protection(&f.store.manifest, &forged).is_err());
}
#[test]
fn factory_gate_reparses_raw_user_asset_models_and_rejects_metadata_overrides() {
    let mut f = Fixture::new();
    let j = f.begin();
    let mut p = f.store.protection(&f.store.manifest, &j).unwrap();
    assert!(factory_only(&p).is_ok());
    for model in [0x0a10_0000u32, 0x0f00_0000] {
        let original = p.expected[&slot(254)].clone();
        let mut records = original.raw.records().to_vec();
        records
            .iter_mut()
            .find(|r| r.command == 0x14 && r.body[0] == 5)
            .unwrap()
            .body[4..8]
            .copy_from_slice(&model.to_le_bytes());
        let mut user = PresetSnapshot::from_raw(
            Arc::new(RawTransfer::from_records(original.raw.kind(), &records).unwrap()),
            original.source.clone(),
            original.target,
            original.revision,
        );
        assert!(
            user.asset_refs
                .iter()
                .any(|r| matches!(r, AssetRef::UserSlot { .. }))
        );
        user.asset_refs.clear();
        p.expected.insert(slot(254), Arc::new(user));
        assert_eq!(factory_only(&p).err().unwrap().kind, ErrorKind::Unverified);
        p.expected.insert(slot(254), original);
    }
}
#[test]
fn stale_sequences_invalid_private_ids_and_pause_bounds_do_not_publish() {
    let mut f = Fixture::new();
    let j = f.begin();
    let generation = f.store.generation();
    assert!(
        f.store
            .begin_preset_movement_entry(&j.id, j.sequence + 1, round(&f.bank, 4, 3))
            .is_err()
    );
    assert!(
        f.store
            .pause_preset_movement(&j.id, j.sequence, "")
            .is_err()
    );
    assert!(
        f.store
            .pause_preset_movement(&j.id, j.sequence, &"x".repeat(4097))
            .is_err()
    );
    assert_eq!(f.store.generation(), generation);
    for private in [j.id.clone(), "invalid/restore-id".into()] {
        let mut extension = f.store.movement_extension().unwrap();
        extension.records.get_mut(&j.id).unwrap().restore.id = private;
        let mut candidate = f.store.manifest.clone();
        candidate.extensions.insert(
            MOVEMENT_EXTENSION.into(),
            serde_json::to_value(extension).unwrap(),
        );
        assert!(decode_movement_extension(&candidate).is_err());
    }
}
#[test]
fn ordinary_candidate_changes_cannot_break_movement_references_or_overflow_manifest() {
    let mut f = Fixture::new();
    let j = f.begin();
    let disk = fs::read(f.temp.0.join("manifest.json")).unwrap();
    let mut candidate = f.store.manifest.clone();
    let mut workflows = decode_workflows(&candidate).unwrap();
    workflows.backups.remove(&j.original_backup);
    candidate.extensions.insert(
        crate::workflows::EXTENSION.into(),
        serde_json::to_value(workflows).unwrap(),
    );
    assert!(f.store.commit(candidate).is_err());
    assert_eq!(fs::read(f.temp.0.join("manifest.json")).unwrap(), disk);
    let mut candidate = f.store.manifest.clone();
    candidate.extensions.insert(
        "bounded-manifest-test".into(),
        json!("x".repeat(crate::store::MAX_MANIFEST as usize)),
    );
    assert!(
        f.store
            .commit(candidate)
            .err()
            .unwrap()
            .message
            .contains("manifest exceeds size limit")
    );
    assert!(!f.store.movement_publication_uncertain);
    assert_eq!(fs::read(f.temp.0.join("manifest.json")).unwrap(), disk);
}
#[test]
fn unrelated_pre_movement_publication_failure_keeps_existing_store_behavior() {
    let temp = Temp::new();
    let mut store = LibraryStore::open(&temp.0).unwrap();
    FAULT.with(|h| *h.borrow_mut() = Some((temp.0.join("manifest.json"), "after-rename")));
    assert!(store.create_setlist("old publication behavior").is_err());
    assert!(!store.movement_publication_uncertain);
}
#[test]
fn postcommit_confirm_and_continue_cancellation_pause_and_return_no_dispatch() {
    for continue_step in [false, true] {
        let mut f = Fixture::new();
        let j = f.begin();
        let d = f.dispatch(&j, 4, 3);
        let action = if continue_step {
            let paused = f
                .store
                .pause_preset_movement(&j.id, d.sequence(), "cancel test")
                .unwrap();
            let report = f
                .store
                .reconcile_preset_movement(&j.id, paused.sequence, round(&f.bank, 4, 4))
                .unwrap();
            crate::WorkflowAction::ContinueMovement {
                journal: j.id.clone(),
                reviewed_report: report.review_hash().unwrap(),
                explicitly_reviewed_retry_targets: BTreeSet::from([slot(254)]),
                fresh: Box::new(round(&f.bank, 4, 5)),
            }
        } else {
            let actual = at(d.snapshot(), 4, 4);
            f.bank.insert(d.target(), actual.clone());
            crate::WorkflowAction::ConfirmMovement {
                journal: j.id.clone(),
                sequence: d.sequence(),
                actor_verified_target_readback: actual,
                post: Box::new(round(&f.bank, 4, 5)),
            }
        };
        let calls = std::cell::Cell::new(0);
        let cancelled = || {
            let n = calls.get();
            calls.set(n + 1);
            n > 0
        };
        assert_eq!(
            crate::workflow_worker::execute_workflow(&mut f.store, action, &cancelled)
                .err()
                .unwrap()
                .kind,
            ErrorKind::Cancelled
        );
        let saved = f.store.movement_journal(&j.id).unwrap();
        assert!(matches!(
            saved.state,
            MovementRunState::PausedReconcileRequired { .. }
        ));
        assert_eq!(saved.restore.verified_count(), usize::from(!continue_step));
    }
}
