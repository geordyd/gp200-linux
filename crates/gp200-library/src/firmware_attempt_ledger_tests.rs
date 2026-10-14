use super::*;
use std::cell::RefCell;
thread_local! {static FAILURE:RefCell<Option<(&'static str,usize)>>=const {RefCell::new(None)};}
pub(super) fn boundary(name: &str) -> Result<(), ServiceError> {
    FAILURE.with(|v| {
        let mut v = v.borrow_mut();
        if let Some((expected, skip)) = &mut *v
            && *expected == name
        {
            if *skip == 0 {
                *v = None;
                return Err(if name == "reservation-before-reread" {
                    io(std::io::Error::other(
                        "injected reservation reread I/O failure",
                    ))
                } else {
                    uncertain(format!("injected {name}"))
                });
            }
            *skip -= 1;
        }
        Ok(())
    })
}
fn fail(name: &'static str, skip: usize) {
    FAILURE.with(|v| *v.borrow_mut() = Some((name, skip)));
}
pub(crate) struct Temp(pub PathBuf);
impl Temp {
    pub(crate) fn new() -> Self {
        let p = std::env::temp_dir().join(format!("gp200-fw-ledger-{}", nonce().unwrap()));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
pub(crate) fn job() -> Arc<ReviewedFirmwareUpdate> {
    let bytes: &[u8] =
        include_bytes!("../../../tests/fixtures/device-assets/synthetic-firmware.bin");
    let source = RetainedJobBlob::checked(
        AssetId("mock firmware".into()),
        bytes.into(),
        JobSha256::of(bytes),
    )
    .unwrap();
    let context = FirmwareUpdateContextDto {
        profile_id: "offline only".into(),
        hardware_policy: "unverified".into(),
        update_mode: "mock".into(),
        normal: None,
        versions: vec![],
        backup_objects: vec![],
        loss_items: vec![],
        power_statement: "descriptive".into(),
        connection_statement: "descriptive".into(),
    };
    let inputs = [
        FirmwareMaterialKind::Profile,
        FirmwareMaterialKind::Evidence,
        FirmwareMaterialKind::BackupArchive,
        FirmwareMaterialKind::BackupInventory,
        FirmwareMaterialKind::RestorePlan,
        FirmwareMaterialKind::Recovery,
        FirmwareMaterialKind::ApprovalStatement,
    ]
    .into_iter()
    .map(|kind| FirmwareMaterialInput {
        kind,
        bytes: Arc::from(&b"retained fixture; no device acceptance"[..]),
    })
    .collect();
    Arc::new(ReviewedFirmwareUpdate::prepare(source, context, inputs, 8).unwrap())
}
pub(crate) fn send(d: &FirmwareBlockDescription) -> FirmwareSendObservation {
    FirmwareSendObservation {
        action: d.ordinal,
        wire_sha256: d.wire_sha256.clone(),
        attempted: u64::from(d.wire_bytes),
        extent: FirmwareOsExtent::Known(u64::from(d.wire_bytes)),
        drain: FirmwareDrainObservation::Returned,
        started_sequence: 1,
        finished_sequence: 2,
        started_at_ms: 10,
        finished_at_ms: 20,
    }
}
fn begin(l: &mut FirmwareAttemptLedger<'_>) -> (FirmwareAttemptId, FirmwareLedgerCursor) {
    let id = l.reserve_id().unwrap();
    let (c, receipt) = l
        .begin_reserved(&id, job(), FirmwareObservationOrigin::MockSimulation)
        .unwrap();
    assert!(!receipt.permits_flash());
    (id, c)
}
#[test]
fn recovery_aliases_one_retained_allocation_across_many_roles() {
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    let mut l = FirmwareAttemptLedger::open(&mut store).unwrap();
    let original = job();
    let mut inputs: Vec<_> = original
        .materials()
        .filter(|(kind, _)| **kind != FirmwareMaterialKind::Image)
        .map(|(kind, bytes)| FirmwareMaterialInput {
            kind: kind.clone(),
            bytes: bytes.into(),
        })
        .collect();
    let common: Arc<[u8]> = vec![42; 64 * 1024].into();
    for i in 0..32 {
        inputs.push(FirmwareMaterialInput {
            kind: FirmwareMaterialKind::BackupObject(format!("role-{i:02}")),
            bytes: Arc::clone(&common),
        });
    }
    let source = original.review().source();
    let reviewed = Arc::new(
        ReviewedFirmwareUpdate::prepare(
            RetainedJobBlob::checked(
                AssetId("aliased fixture".into()),
                source.into(),
                JobSha256::of(source),
            )
            .unwrap(),
            original.context().clone(),
            inputs,
            original.chunk(),
        )
        .unwrap(),
    );
    let id = l.reserve_id().unwrap();
    let (_cursor, _) = l
        .begin_reserved(
            &id,
            Arc::clone(&reviewed),
            FirmwareObservationOrigin::MockSimulation,
        )
        .unwrap();
    let rebuilt = l.read_job(&reviewed.digest().hex()).unwrap();
    let aliases: Vec<_> = rebuilt
        .materials()
        .filter(|(kind, _)| matches!(kind, FirmwareMaterialKind::BackupObject(_)))
        .collect();
    assert_eq!(aliases.len(), 32);
    for (_, bytes) in &aliases {
        assert_eq!(*bytes, &*common);
        assert!(std::ptr::eq(*bytes, aliases[0].1));
    }
    // Declared distinct sizes are rejected before trying any absent hash.
    let mut forged: serde_json::Value = serde_json::from_slice(reviewed.preimage()).unwrap();
    for i in 0..3 {
        forged["pins"].as_array_mut().unwrap().push(
            serde_json::to_value(FirmwareMaterialPin {
                kind: FirmwareMaterialKind::BackupObject(format!("missing-large-{i}")),
                sha256: JobSha256::of(&[i]).hex(),
                bytes: FIRMWARE_SOURCE_LIMIT,
            })
            .unwrap(),
        );
    }
    let hash = l.material(&serde_json::to_vec(&forged).unwrap()).unwrap();
    assert!(
        l.read_job(&hash)
            .unwrap_err()
            .to_string()
            .contains("128 MiB")
    );
}
#[test]
fn complete_mock_is_durable_observation_only_and_reopen_cannot_append() {
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    let mut l = FirmwareAttemptLedger::open(&mut store).unwrap();
    let (id, mut c) = begin(&mut l);
    let j = job();
    for d in j.blocks() {
        let (next, receipt) = l.checkpoint(c, d.ordinal).unwrap();
        assert_eq!(receipt.action(), Some(&d));
        c = l.observe_send(next, send(&d)).unwrap();
        c = l.acknowledge_mock(c, d).unwrap();
    }
    let found = l
        .close(c, FirmwareHostClosure::ObservationFinished)
        .unwrap();
    assert!(found.issues.is_empty());
    assert_eq!(
        found.projection.as_ref().unwrap().simulated_source_bytes(),
        j.review()
            .summary()
            .sections
            .iter()
            .map(|s| s.bytes as u64)
            .sum::<u64>()
    );
    assert!(!found.can_resume());
    assert!(!found.permits_flash());
    drop(l);
    drop(store);
    let mut store = LibraryStore::open(&t.0).unwrap();
    let mut l = FirmwareAttemptLedger::open(&mut store).unwrap();
    assert!(
        l.begin_reserved(&id, j, FirmwareObservationOrigin::MockSimulation)
            .is_err()
    );
    assert!(l.load(&id).unwrap().local_checkpoint_confirmed);
}
#[test]
fn permanent_id_collision_and_corrupt_marker_cannot_restart() {
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    let mut l = FirmwareAttemptLedger::open(&mut store).unwrap();
    let id = l.reserve_id().unwrap();
    assert!(l.reserve_exact(&id).is_err());
    fs::write(l.path(&id, "reserved"), b"partial").unwrap();
    assert!(
        l.begin_reserved(&id, job(), FirmwareObservationOrigin::MockSimulation)
            .is_err()
    );
    assert!(l.reserve_exact(&id).is_err());
}
#[test]
fn all_start_publication_failures_consume_start_and_poison_store() {
    for phase in [
        "new-before-create",
        "new-partial-write",
        "new-write",
        "new-fsync",
        "new-directory-sync",
        "before-checkpoint-rename",
        "after-checkpoint-rename",
        "after-checkpoint-directory-sync",
        "before-reread",
        "after-reread",
    ] {
        let t = Temp::new();
        let mut store = LibraryStore::open(&t.0).unwrap();
        let mut l = FirmwareAttemptLedger::open(&mut store).unwrap();
        let id = l.reserve_id().unwrap();
        fail(phase, 0);
        assert!(
            l.begin_reserved(&id, job(), FirmwareObservationOrigin::MockSimulation)
                .is_err(),
            "{phase}"
        );
        assert!(!l.allocated.contains(&id));
        assert!(l.active.is_empty());
        assert!(l.store.firmware_publication_uncertain);
        assert!(l.store.publish_blob(b"later").is_err());
        assert!(l.store.commit(l.store.manifest.clone()).is_err());
        assert!(l.path(&id, "reserved").exists());
    }
}
#[test]
fn each_append_partial_sync_receipt_and_reread_failure_returns_no_cursor() {
    for phase in [
        "append-partial-write",
        "append-write",
        "append-fsync",
        "new-partial-write",
        "new-fsync",
        "new-directory-sync",
        "before-checkpoint-rename",
        "after-checkpoint-rename",
        "after-checkpoint-directory-sync",
        "before-reread",
        "after-reread",
    ] {
        let t = Temp::new();
        let mut store = LibraryStore::open(&t.0).unwrap();
        let mut l = FirmwareAttemptLedger::open(&mut store).unwrap();
        let (id, c) = begin(&mut l);
        let before = fs::read(l.path(&id, "journal")).unwrap();
        fail(phase, 0);
        assert!(l.checkpoint(c, 0).is_err(), "{phase}");
        assert!(l.active.is_empty());
        assert!(l.store.firmware_publication_uncertain);
        let after = fs::read(l.path(&id, "journal")).unwrap();
        assert_eq!(&after[..before.len()], before);
        assert!(after.len() > before.len());
        assert!(l.store.commit(l.store.manifest.clone()).is_err());
    }
}
#[test]
fn prewrite_validation_does_not_poison_healthy_manifest() {
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    let mut l = FirmwareAttemptLedger::open(&mut store).unwrap();
    let (_, c) = begin(&mut l);
    assert!(l.checkpoint(c, u64::MAX).is_err());
    assert!(!l.store.firmware_publication_uncertain);
    l.store.commit(l.store.manifest.clone()).unwrap();
}
#[test]
fn reservation_reread_io_failure_preserves_id_and_poisoned_store() {
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    let mut ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
    let id = FirmwareAttemptId::parse("1234567890abcdef1234567890abcdef").unwrap();
    fail("reservation-before-reread", 0);
    let error = ledger.reserve_exact(&id).unwrap_err();
    assert!(error.to_string().contains("reservation reread I/O failure"));
    assert_eq!(
        fs::read(ledger.path(&id, "reserved")).unwrap(),
        format!("gp200-firmware-local-attempt-v1:{}\n", id.as_str()).as_bytes()
    );
    assert!(ledger.store.firmware_publication_uncertain);
    assert!(!ledger.allocated.contains(&id));
    assert!(ledger.store.commit(ledger.store.manifest.clone()).is_err());
    assert!(ledger.store.publish_blob(b"later").is_err());
    assert!(ledger.reserve_exact(&id).is_err());
    drop(ledger);
    drop(store);
    let mut reopened = LibraryStore::open(&t.0).unwrap();
    let mut ledger = FirmwareAttemptLedger::open(&mut reopened).unwrap();
    assert_eq!(ledger.attempt_ids().unwrap(), vec![id.clone()]);
    assert!(matches!(ledger.reserve_exact(&id), Err(e) if e.kind == ErrorKind::Conflict));
    assert!(
        ledger
            .begin_reserved(&id, job(), FirmwareObservationOrigin::LocalObservation)
            .is_err()
    );
}
#[test]
fn direct_invalid_send_validation_preserves_bytes_without_poisoning_healthy_store() {
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    let mut ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
    let (id, cursor) = begin(&mut ledger);
    let (cursor, checkpoint) = ledger.checkpoint(cursor, 0).unwrap();
    let prior = fs::read(ledger.path(&id, "journal")).unwrap();
    let mut invalid = send(checkpoint.action().unwrap());
    invalid.wire_sha256 = digest(b"wrong wire");
    assert!(ledger.observe_send(cursor, invalid).is_err());
    assert_eq!(fs::read(ledger.path(&id, "journal")).unwrap(), prior);
    assert!(!ledger.store.firmware_publication_uncertain);
    ledger.store.commit(ledger.store.manifest.clone()).unwrap();
    ledger.store.publish_blob(b"healthy validation").unwrap();
    assert_eq!(ledger.load(&id).unwrap().accepted_records, 2);
}
#[test]
fn partial_tail_is_retained_and_streamed_without_projection_leak() {
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    let mut l = FirmwareAttemptLedger::open(&mut store).unwrap();
    let (id, c) = begin(&mut l);
    let prefix = fs::read(l.path(&id, "journal")).unwrap();
    fail("append-partial-write", 0);
    assert!(l.checkpoint(c, 0).is_err());
    let raw = fs::read(l.path(&id, "journal")).unwrap();
    let found = l.load(&id).unwrap();
    assert_eq!(found.accepted_records, 1);
    assert_eq!(found.valid_prefix_bytes, prefix.len() as u64);
    assert!(found.projection.as_ref().unwrap().pending().is_none());
    let tail = found.raw_tail.as_ref().unwrap();
    let mut out = vec![];
    l.stream_bad_tail(&id, tail, &mut out).unwrap();
    assert_eq!(out, raw[prefix.len()..]);
    assert_eq!(tail.sha256, digest(&out));
    assert!(!found.can_resume());
}
#[test]
fn bad_tail_export_rechecks_source_and_reports_unverified_destination() {
    struct ChangingDestination {
        path: PathBuf,
        bytes: Vec<u8>,
    }
    impl Write for ChangingDestination {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            OpenOptions::new()
                .append(true)
                .open(&self.path)?
                .write_all(b"changed")?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    let mut l = FirmwareAttemptLedger::open(&mut store).unwrap();
    let (id, _cursor) = begin(&mut l);
    let path = l.path(&id, "journal");
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"torn evidence")
        .unwrap();
    let found = l.load(&id).unwrap();
    let mut destination = ChangingDestination {
        path,
        bytes: vec![],
    };
    let error = l
        .stream_bad_tail(&id, &found.raw_tail.unwrap(), &mut destination)
        .unwrap_err();
    assert_eq!(destination.bytes, b"torn evidence");
    assert!(error.to_string().contains("destination is unverified"));
}
#[test]
fn tampered_material_and_journal_inode_stop_and_poison_before_next_frame() {
    for material in [true, false] {
        let t = Temp::new();
        let mut store = LibraryStore::open(&t.0).unwrap();
        let mut l = FirmwareAttemptLedger::open(&mut store).unwrap();
        let (id, c) = begin(&mut l);
        let path = if material {
            l.store
                .root()
                .join("blobs")
                .join(job().pins()[0].sha256.clone())
        } else {
            l.path(&id, "journal")
        };
        let old = fs::read(&path).unwrap();
        let changed = path.with_extension("changed");
        fs::write(&changed, &old).unwrap();
        fs::rename(changed, path).unwrap();
        assert!(l.checkpoint(c, 0).is_err());
        assert!(l.store.firmware_publication_uncertain);
    }
}
#[test]
fn complete_wrong_frame_cannot_leak_a_finish_or_pending_checkpoint() {
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    let mut l = FirmwareAttemptLedger::open(&mut store).unwrap();
    let (id, _) = begin(&mut l);
    let active = l.active.get(&id).unwrap();
    let bogus = Record {
        schema: 1,
        attempt: id.clone(),
        sequence: 88,
        previous: Some(active.last.clone()),
        entry: Entry::Closed(FirmwareHostClosure::ObservationFinished),
    };
    let mut file = OpenOptions::new()
        .append(true)
        .open(l.path(&id, "journal"))
        .unwrap();
    file.write_all(&encode(&bogus).unwrap()).unwrap();
    file.sync_all().unwrap();
    let r = l.load(&id).unwrap();
    assert_eq!(r.accepted_records, 1);
    assert_eq!(r.projection.unwrap().recorded_closure(), None);
    assert!(r.raw_tail.is_some());
}
#[test]
fn raw_status_and_partial_os_send_never_advance_a_block() {
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    let mut l = FirmwareAttemptLedger::open(&mut store).unwrap();
    let id = l.reserve_id().unwrap();
    let (c, _) = l
        .begin_reserved(&id, job(), FirmwareObservationOrigin::LocalObservation)
        .unwrap();
    let (c, r) = l.checkpoint(c, 0).unwrap();
    let mut s = send(r.action().unwrap());
    s.extent = FirmwareOsExtent::Unknown;
    let c = l.observe_send(c, s).unwrap();
    let c = l
        .observe_callback(
            c,
            FirmwareCallbackDto {
                sequence: 3,
                at_ms: 17,
                clock_origin: "unknown-calendar-origin".into(),
                epoch: SessionEpoch(1),
                binding_sha256: digest(b"claimed"),
                raw: vec![0xf0, 0x7e, 0x48, 4, 0x7f, 0xf7],
            },
        )
        .unwrap();
    assert!(l.acknowledge_mock(c, job().block(0).unwrap()).is_err());
    let found = l.load(&id).unwrap();
    let p = found.projection.unwrap();
    assert_eq!(p.next_block(), 0);
    assert_eq!(p.device_verified_bytes(), 0);
    assert!(!p.completion_verified());
}
#[test]
fn mode_observation_survives_fault_and_does_not_offer_safe_cancel() {
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    let mut l = FirmwareAttemptLedger::open(&mut store).unwrap();
    let id = l.reserve_id().unwrap();
    let (c, _) = l
        .begin_reserved(&id, job(), FirmwareObservationOrigin::LocalObservation)
        .unwrap();
    let c = l
        .record_mode_boundary(
            c,
            vec![0xf0, 0x7e, 0x48, 9, 0xf7],
            "foreign observed bytes; effects unverified".into(),
        )
        .unwrap();
    assert!(
        l.close(c, FirmwareHostClosure::CancelledBeforeEntry)
            .is_err()
    );
    let found = l.load(&id).unwrap();
    assert!(found.possible_device_update_mode);
    assert_eq!(
        found.projection.unwrap().state(),
        &FirmwareDerivedLocalState::Indeterminate
    );
}
#[test]
fn traffic_overflow_retains_fault_reserve_and_blocks_ordinary_writes() {
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    let mut l = FirmwareAttemptLedger::open(&mut store).unwrap();
    let (id, c) = begin(&mut l);
    let a = l.active.get_mut(&id).unwrap();
    a.callbacks = a.job.budget().callbacks;
    let callback = FirmwareCallbackDto {
        sequence: 1,
        at_ms: 1,
        clock_origin: "fixture".into(),
        epoch: SessionEpoch(1),
        binding_sha256: digest(b"claim"),
        raw: vec![0xf0, 0x7e, 0x48, 4, 0x7f, 0xf7],
    };
    assert!(l.observe_callback(c, callback).is_err());
    assert!(l.store.firmware_publication_uncertain);
    let found = l.load(&id).unwrap();
    assert!(found.recent.iter().any(|r| r.label == "Evidence fault"));
    assert!(!found.can_resume());
}
#[test]
fn reopen_after_uncertain_checkpoint_never_restores_authority() {
    let t = Temp::new();
    let id;
    {
        let mut store = LibraryStore::open(&t.0).unwrap();
        let mut l = FirmwareAttemptLedger::open(&mut store).unwrap();
        let (i, c) = begin(&mut l);
        id = i;
        fail("append-fsync", 0);
        assert!(l.checkpoint(c, 0).is_err());
    }
    let mut store = LibraryStore::open(&t.0).unwrap();
    assert!(!store.firmware_publication_uncertain);
    let mut l = FirmwareAttemptLedger::open(&mut store).unwrap();
    let found = l.load(&id).unwrap();
    assert!(!found.local_checkpoint_confirmed);
    assert!(!found.can_resume());
    assert!(
        l.begin_reserved(&id, job(), FirmwareObservationOrigin::MockSimulation)
            .is_err()
    );
}
#[test]
fn encoded_worst_case_callback_and_action_fit_declared_preparation_caps() {
    let id = FirmwareAttemptId::parse("1234567890abcdef1234567890abcdef").unwrap();
    let mut raw = vec![127; 518];
    raw[0] = 0xf0;
    raw[517] = 0xf7;
    let e = Entry::Callback(FirmwareCallbackDto {
        sequence: u64::MAX,
        at_ms: u64::MAX,
        clock_origin: "x".repeat(128),
        epoch: SessionEpoch(u64::MAX),
        binding_sha256: "f".repeat(64),
        raw,
    });
    let f = encode(&Record {
        schema: 1,
        attempt: id.clone(),
        sequence: u64::MAX,
        previous: Some("f".repeat(64)),
        entry: e,
    })
    .unwrap();
    assert!(f.len() <= 4 * 1024 + 44);
    let d = job().block(0).unwrap();
    let f = encode(&Record {
        schema: 1,
        attempt: id,
        sequence: u64::MAX,
        previous: Some("f".repeat(64)),
        entry: Entry::Checkpoint(d),
    })
    .unwrap();
    assert!(f.len() <= 4 * 1024 + 44);
}

#[test]
fn official_section_sizes_stream_encode_normal_growth_and_all_kind_caps() {
    // Synthetic contents with the retained official image's independent lengths.
    // This measures encoded local records, not pedal ACKs or a full-disk soak.
    let lengths = [4_192_632usize, 1_834_032, 294_912, 131_072];
    let mut raw = vec![42u8; 96 + lengths.iter().sum::<usize>()];
    raw[..96].fill(0);
    raw[..4].copy_from_slice(b"FRMW");
    let declared = (raw.len() - 48) as u32;
    raw[4..8].copy_from_slice(&declared.to_le_bytes());
    raw[16..22].copy_from_slice(b"GP-200");
    raw[88..92].copy_from_slice(b"V182");
    raw[33] = 2;
    let mut offset = 96;
    for (i, length) in lengths.iter().enumerate() {
        raw[48 + i * 8..52 + i * 8].copy_from_slice(&((offset - 48) as u32).to_le_bytes());
        raw[52 + i * 8..56 + i * 8].copy_from_slice(&(*length as u32).to_le_bytes());
        offset += length;
    }
    raw[14] = raw[48..].iter().fold(0u8, |a, b| a.wrapping_add(*b));
    let original = job();
    let docs = original
        .materials()
        .filter(|(k, _)| **k != FirmwareMaterialKind::Image)
        .map(|(k, b)| FirmwareMaterialInput {
            kind: k.clone(),
            bytes: b.into(),
        })
        .collect();
    let source_hash = JobSha256::of(&raw);
    let j = ReviewedFirmwareUpdate::prepare(
        RetainedJobBlob::checked(
            AssetId("official size synthetic".into()),
            raw.into(),
            source_hash,
        )
        .unwrap(),
        original.context().clone(),
        docs,
        128,
    )
    .unwrap();
    assert_eq!(j.budget().blocks, 50_412);
    assert_eq!(j.budget().bytes, 787_253_116);
    let id = FirmwareAttemptId::parse("1234567890abcdef1234567890abcdef").unwrap();
    let callback = FirmwareCallbackDto {
        sequence: u64::MAX,
        at_ms: u64::MAX,
        clock_origin: "x".repeat(128),
        epoch: SessionEpoch(u64::MAX),
        binding_sha256: "f".repeat(64),
        raw: vec![0xf0, 0x7e, 0x48, 4, 0x7f, 0xf7],
    };
    let mut count = 0u64;
    let mut bytes = 0u64;
    let mut include = |entry: Entry| {
        let cap = entry.cap();
        let frame = encode(&Record {
            schema: 1,
            attempt: id.clone(),
            sequence: count,
            previous: Some("f".repeat(64)),
            entry,
        })
        .unwrap();
        assert!(frame.len() <= cap + HEADER);
        count += 1;
        bytes += frame.len() as u64;
    };
    include(Entry::Prepared {
        review: j.digest().hex(),
        origin: FirmwareObservationOrigin::MockSimulation,
    });
    let mut wraps = 0;
    let mut tails = 0;
    for d in j.blocks() {
        if d.block_in_section > 0 && d.index_mod128 == 0 {
            wraps += 1;
        }
        if d.padded_bytes != 0 {
            tails += 1;
        }
        include(Entry::Send(send(&d)));
        include(Entry::Callback(callback.clone()));
        include(Entry::Callback(callback.clone()));
        include(Entry::Checkpoint(d.clone()));
        include(Entry::MockDecision(d));
    }
    include(Entry::Closed(FirmwareHostClosure::ObservationFinished));
    assert_eq!(count, 252_062);
    assert_eq!(wraps, 390);
    assert_eq!(tails, 2);
    assert!(bytes > 50 * 1024 * 1024 && bytes < 200 * 1024 * 1024);
    assert!(bytes < j.budget().bytes - FIRMWARE_TERMINAL_RESERVE * (8 * 1024 + 44));
    println!(
        "FW02 growth: {} blocks, {count} encoded normal frames, {bytes} normal bytes; admission {} bytes; wraps {wraps}; tails {tails}",
        j.budget().blocks,
        j.budget().bytes
    );
    let mut full_raw = vec![127; 518];
    full_raw[0] = 0xf0;
    full_raw[517] = 0xf7;
    let mut largest_callback = callback;
    largest_callback.raw = full_raw.clone();
    for entry in [
        Entry::Callback(largest_callback),
        Entry::ModeBoundary {
            raw: full_raw,
            note: "x".repeat(512),
        },
        Entry::Fault("x".repeat(4096)),
        Entry::Closed(FirmwareHostClosure::HostStoppedIndeterminate),
    ] {
        let cap = entry.cap();
        assert!(
            encode(&Record {
                schema: 1,
                attempt: id.clone(),
                sequence: u64::MAX,
                previous: Some("f".repeat(64)),
                entry
            })
            .unwrap()
            .len()
                <= cap + HEADER
        );
    }
}
#[test]
fn lifecycle_branch_rejects_legacy_mixing_and_recovery_issues_no_cursor() {
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    let j = job();
    let mut ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
    let id = ledger.reserve_id().unwrap();
    let (c, _) = ledger
        .begin_reserved(&id, j.clone(), FirmwareObservationOrigin::MockSimulation)
        .unwrap();
    let p = FirmwareLifecyclePlanDto {
        review_sha256: j.digest().hex(),
        profile_sha256: JobSha256::of(b"rule").hex(),
        origin: FirmwareObservationOrigin::MockSimulation,
        nonblock_actions: 1,
        epoch: SessionEpoch(1),
        binding: FirmwareUsbObservation {
            topology: "test".into(),
            vendor: 1,
            product: 2,
            serial: "test".into(),
            interface: "test".into(),
            endpoint: "test".into(),
        },
        generation: 1,
        clock_origin: "test".into(),
    };
    let c = ledger.prepare_lifecycle(c, p).unwrap();
    assert!(ledger.checkpoint(c, 0).is_err());
    let r = ledger.load(&id).unwrap();
    assert_eq!(r.accepted_records, 2);
    assert!(r.lifecycle.as_ref().unwrap().stopped());
    assert!(!r.can_resume());
    assert!(!r.permits_flash());
}

pub(crate) fn epoch_plan(j: &ReviewedFirmwareUpdate) -> FirmwareLifecyclePlanDto {
    FirmwareLifecyclePlanDto {
        review_sha256: j.digest().hex(),
        profile_sha256: JobSha256::of(b"synthetic epoch rule").hex(),
        origin: FirmwareObservationOrigin::MockSimulation,
        nonblock_actions: 1,
        epoch: SessionEpoch(9),
        generation: 1,
        clock_origin: "shared updater test clock".into(),
        binding: FirmwareUsbObservation {
            topology: "private test topology".into(),
            vendor: 1,
            product: 2,
            serial: "private test serial".into(),
            interface: "private test interface".into(),
            endpoint: "entry test alias".into(),
        },
    }
}
pub(crate) fn epoch_entry(p: &FirmwareLifecyclePlanDto) -> FirmwareLifecycleEpochObservation {
    FirmwareLifecycleEpochObservation {
        role: FirmwareLifecycleEpochRole::NormalEntry,
        epoch: p.epoch,
        generation: p.generation,
        binding: p.binding.clone(),
        previous_binding_sha256: None,
        previous_epoch: None,
        source_instance: "updater entry test source".into(),
        clock_origin: p.clock_origin.clone(),
        retired_source: None,
        raw_identity: vec![0xf0, 0x7d, 1, 0xf7],
        negotiation: vec![],
        transition: None,
        post_normal_refresh: None,
    }
}
pub(crate) fn epoch_next(
    previous: &FirmwareLifecycleEpochObservation,
    role: FirmwareLifecycleEpochRole,
    frontier: u64,
    at_ns: u64,
) -> FirmwareLifecycleEpochObservation {
    let mut binding = previous.binding.clone();
    binding.endpoint = format!("test {role:?} alias");
    let source = format!("private {role:?} source");
    let clock = if role == FirmwareLifecycleEpochRole::PostUpdateNormal {
        "fresh normal test clock".into()
    } else {
        previous.clock_origin.clone()
    };
    let epoch = SessionEpoch(previous.epoch.0 + 1);
    FirmwareLifecycleEpochObservation {
        role,
        epoch,
        generation: previous.generation + 1,
        binding,
        previous_binding_sha256: Some(previous.binding.digest()),
        previous_epoch: Some(previous.epoch),
        source_instance: source.clone(),
        clock_origin: clock.clone(),
        retired_source: Some(FirmwareLifecycleSourceRetirement {
            source_instance: previous.source_instance.clone(),
            clock_origin: previous.clock_origin.clone(),
            epoch: previous.epoch,
            generation: previous.generation,
            allocated: frontier,
            processed: frontier,
            close_started_at_ns: at_ns,
            close_finished_at_ns: at_ns + 1,
            reader_join: FirmwareNormalReaderJoinObservation::Returned,
            fault_or_missing: false,
        }),
        raw_identity: vec![0xf0, 0x7d, 2, 0xf7],
        negotiation: vec![0xf0, 0x7d, 3, 0xf7],
        transition: Some(FirmwareLifecycleTransitionObservation {
            kind: FirmwareLifecycleTransitionKind::Reenumerated,
            previous_enumeration_generation: previous
                .transition
                .as_ref()
                .map_or(1, |t| t.enumeration_generation),
            enumeration_generation: previous
                .transition
                .as_ref()
                .map_or(2, |t| t.enumeration_generation + 1),
            old_source_disappeared: true,
            old_and_new_coexisted: false,
            candidate_count: 1,
        }),
        post_normal_refresh: (role == FirmwareLifecycleEpochRole::PostUpdateNormal).then(|| {
            FirmwarePostNormalRefreshObservation {
                epoch,
                source_instance: source,
                clock_origin: clock,
                allocated: 4,
                processed: 4,
                identity_sha256: digest(b"fresh parsed identity"),
                current_sha256: digest(b"fresh parsed current"),
                globals_sha256: digest(b"fresh parsed globals"),
                stored_sha256: digest(b"fresh parsed stored zero"),
                current_revision: 1,
                globals_revision: 1,
                stored_revision: 1,
                stored_slot: 0,
                application_version: Some("synthetic fixture version".into()),
                assessment: "Synthetic fixture, no physical acceptance".into(),
            }
        }),
    }
}
fn epoch_action(
    ledger: &mut FirmwareAttemptLedger<'_>,
    mut c: FirmwareLedgerCursor,
    j: &ReviewedFirmwareUpdate,
    e: &FirmwareLifecycleEpochObservation,
    ordinal: u64,
    at_ns: u64,
) -> FirmwareLedgerCursor {
    let block = ordinal.checked_sub(1).map(|i| j.block(i).unwrap());
    let wire = block
        .as_ref()
        .map_or_else(|| vec![0xf0, 0x7d, 1, 0xf7], |b| j.block_wire(b).unwrap());
    let a = FirmwareLifecycleActionDescription {
        ordinal,
        kind: if block.is_some() {
            FirmwareLifecycleKind::ImageBlock
        } else {
            FirmwareLifecycleKind::IdentityQuery
        },
        wire_sha256: digest(&wire),
        wire: wire.clone(),
        block,
        epoch: e.epoch,
        binding_sha256: e.binding.digest(),
        generation: e.generation,
        rule_sha256: JobSha256::of(b"synthetic epoch rule").hex(),
    };
    c = ledger.lifecycle_checkpoint(c, a).unwrap().0;
    c = ledger
        .lifecycle_send(
            c,
            FirmwareLifecycleSendObservation {
                action: ordinal,
                wire_sha256: digest(&wire),
                attempted: wire.len() as u64,
                extent: FirmwareOsExtent::Known(wire.len() as u64),
                drain: FirmwareDrainObservation::Returned,
                started_sequence: ordinal,
                finished_sequence: Some(ordinal + 1),
                started_at_ns: at_ns - 1,
                finished_at_ns: Some(at_ns),
            },
        )
        .unwrap();
    c = ledger
        .lifecycle_raw(
            c,
            FirmwareLifecycleRawObservation {
                sequence: ordinal + 1,
                at_ns,
                epoch: e.epoch,
                generation: e.generation,
                binding_sha256: e.binding.digest(),
                clock_origin: e.clock_origin.clone(),
                raw: vec![0xf0, 0x7d, 0xf7],
            },
        )
        .unwrap();
    ledger
        .lifecycle_decision(
            c,
            FirmwareLifecycleDecision {
                action: ordinal,
                rule_sha256: JobSha256::of(b"synthetic epoch rule").hex(),
                first_raw: ordinal + 1,
                last_raw: ordinal + 1,
                observed_frontier: ordinal + 1,
                mock_matched: true,
            },
        )
        .unwrap()
}
#[test]
fn real_durable_three_role_frames_survive_reopen_without_live_permission() {
    let t = Temp::new();
    let j = job();
    let id;
    let expected;
    {
        let mut store = LibraryStore::open(&t.0).unwrap();
        let mut ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
        id = ledger.reserve_id().unwrap();
        let (mut c, _) = ledger
            .begin_reserved(&id, j.clone(), FirmwareObservationOrigin::MockSimulation)
            .unwrap();
        let plan = epoch_plan(&j);
        let entry = epoch_entry(&plan);
        c = ledger.prepare_lifecycle(c, plan.clone()).unwrap();
        let (c2, receipt) = ledger.lifecycle_epoch(c, entry.clone()).unwrap();
        c = c2;
        assert_eq!(receipt.sequence(), 2);
        assert!(!receipt.permits_flash());
        c = epoch_action(&mut ledger, c, &j, &entry, 0, 1);
        let boot = epoch_next(&entry, FirmwareLifecycleEpochRole::Bootloader, 1, 1);
        c = ledger.lifecycle_epoch(c, boot.clone()).unwrap().0;
        for ordinal in 1..j.budget().blocks + 1 {
            c = epoch_action(&mut ledger, c, &j, &boot, ordinal, ordinal + 2);
        }
        let post = epoch_next(
            &boot,
            FirmwareLifecycleEpochRole::PostUpdateNormal,
            j.budget().blocks + 1,
            j.budget().blocks + 3,
        );
        c = ledger.lifecycle_epoch(c, post.clone()).unwrap().0;
        let retained = ledger
            .lifecycle_close(c, FirmwareLifecycleHostClosure::MockFinished)
            .unwrap();
        assert!(retained.issues.is_empty(), "{:?}", retained.issues);
        assert!(retained.local_checkpoint_confirmed);
        expected = vec![entry, boot, post];
        assert_eq!(retained.lifecycle.as_ref().unwrap().epochs(), expected);
        let raw = fs::read(ledger.path(&id, "journal")).unwrap();
        let mut file = File::open(ledger.path(&id, "journal")).unwrap();
        let mut offset = 0;
        let mut frames = 0;
        while offset < raw.len() as u64 {
            let (record, bytes) = read_frame(&mut file, offset, raw.len() as u64).unwrap();
            if matches!(record.entry, Entry::LifecycleEpoch(_)) {
                assert!(bytes.len() <= 8192 + HEADER);
                frames += 1;
            }
            offset += bytes.len() as u64;
        }
        assert_eq!(frames, 3);
    }
    let mut store = LibraryStore::open(&t.0).unwrap();
    let ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
    let retained = ledger.load(&id).unwrap();
    assert!(retained.issues.is_empty());
    assert_eq!(retained.lifecycle.as_ref().unwrap().epochs(), expected);
    assert!(!retained.can_resume());
    assert!(!retained.permits_flash());
    assert!(!retained.lifecycle.unwrap().completion_verified());
}
#[test]
fn epoch_append_sync_publication_and_independent_reread_failures_issue_no_checkpoint() {
    for phase in [
        "append-partial-write",
        "append-fsync",
        "before-checkpoint-rename",
        "after-checkpoint-directory-sync",
        "before-reread",
        "after-reread",
    ] {
        let t = Temp::new();
        let mut store = LibraryStore::open(&t.0).unwrap();
        let mut ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
        let j = job();
        let id = ledger.reserve_id().unwrap();
        let (c, _) = ledger
            .begin_reserved(&id, j.clone(), FirmwareObservationOrigin::MockSimulation)
            .unwrap();
        let plan = epoch_plan(&j);
        let c = ledger.prepare_lifecycle(c, plan.clone()).unwrap();
        let before = fs::read(ledger.path(&id, "journal")).unwrap();
        fail(phase, 0);
        assert!(
            ledger.lifecycle_epoch(c, epoch_entry(&plan)).is_err(),
            "{phase}"
        );
        assert!(ledger.store.firmware_publication_uncertain);
        assert!(ledger.active.is_empty());
        let after = fs::read(ledger.path(&id, "journal")).unwrap();
        assert_eq!(&after[..before.len()], before);
        assert!(after.len() > before.len());
        let retained = ledger.load(&id).unwrap();
        assert!(!retained.can_resume());
        assert!(!retained.permits_flash());
    }
}
#[test]
fn duplicate_and_unknown_epoch_tail_stop_at_exact_prefix_and_remain_original_bytes() {
    for unknown in [false, true] {
        let t = Temp::new();
        let mut store = LibraryStore::open(&t.0).unwrap();
        let mut ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
        let j = job();
        let id = ledger.reserve_id().unwrap();
        let (c, _) = ledger
            .begin_reserved(&id, j.clone(), FirmwareObservationOrigin::MockSimulation)
            .unwrap();
        let plan = epoch_plan(&j);
        let c = ledger.prepare_lifecycle(c, plan.clone()).unwrap();
        let entry = epoch_entry(&plan);
        let (c, _) = ledger.lifecycle_epoch(c, entry.clone()).unwrap();
        let before = fs::read(ledger.path(&id, "journal")).unwrap();
        let record = Record {
            schema: 1,
            attempt: id.clone(),
            sequence: 3,
            previous: Some(c.last.clone()),
            entry: Entry::LifecycleEpoch(Box::new(entry)),
        };
        let tail = if unknown {
            let mut value = serde_json::to_value(&record).unwrap();
            value["entry"]["LifecycleEpoch"]["role"] =
                serde_json::Value::String("FutureRole".into());
            let body = serde_json::to_vec(&value).unwrap();
            let mut f = MAGIC.to_vec();
            f.extend((body.len() as u32).to_le_bytes());
            f.extend(Sha256::digest(&body));
            f.extend(body);
            f
        } else {
            encode(&record).unwrap()
        };
        let mut all = before.clone();
        all.extend(tail);
        fs::write(ledger.path(&id, "journal"), &all).unwrap();
        let retained = ledger.load(&id).unwrap();
        assert_eq!(retained.accepted_records, 3);
        assert_eq!(retained.valid_prefix_bytes, before.len() as u64);
        assert!(!retained.issues.is_empty());
        assert_eq!(retained.lifecycle.as_ref().unwrap().epochs().len(), 1);
        assert_eq!(fs::read(ledger.path(&id, "journal")).unwrap(), all);
        // The exact older valid prefix still matches its genuine fsync
        // receipt. Bad suffix issues and raw_tail remain independent facts;
        // confirmed local bytes do not grant an accepted tail or resumption.
        assert!(retained.local_checkpoint_confirmed);
        assert!(retained.raw_tail.is_some());
        assert!(!retained.can_resume());
        assert!(!retained.permits_flash());
    }
}
#[test]
fn legacy_prepared_record_serialization_stays_byte_exact_with_additive_epoch_variant() {
    let record = Record {
        schema: 1,
        attempt: FirmwareAttemptId::parse("11111111111111111111111111111111").unwrap(),
        sequence: 0,
        previous: None,
        entry: Entry::Prepared {
            review: "a".repeat(64),
            origin: FirmwareObservationOrigin::LocalObservation,
        },
    };
    let expected=br#"{"schema":1,"attempt":"11111111111111111111111111111111","sequence":0,"previous":null,"entry":{"Prepared":{"review":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","origin":"LocalObservation"}}}"#;
    let frame = encode(&record).unwrap();
    assert_eq!(&frame[HEADER..], expected);
    assert_eq!(&frame[..8], MAGIC);
    assert_eq!(&frame[12..44], &Sha256::digest(expected)[..]);
}

pub(crate) fn entry_study(j: &ReviewedFirmwareUpdate) -> FirmwareEntryObservationPlanDto {
    let mut plan = epoch_plan(j);
    plan.nonblock_actions = 2;
    let candidates = [vec![0xf0, 0x21, 0xf7], vec![0xf0, 0x22, 0xf7]];
    let study_sha256 = FirmwareEntryObservationPlanDto::digest_for(&plan, &candidates);
    FirmwareEntryObservationPlanDto {
        plan,
        candidates,
        study_sha256,
    }
}
pub(crate) fn entry_host_action(
    e: &FirmwareEntryObservationPlanDto,
    ordinal: u64,
) -> FirmwareLifecycleActionDescription {
    let wire = e.candidates[ordinal as usize].clone();
    FirmwareLifecycleActionDescription {
        ordinal,
        kind: FirmwareLifecycleKind::EntryTransition,
        wire_sha256: JobSha256::of(&wire).hex(),
        wire,
        block: None,
        epoch: e.plan.epoch,
        binding_sha256: e.plan.binding.digest(),
        generation: e.plan.generation,
        rule_sha256: e.study_sha256.clone(),
    }
}
#[test]
fn entry_real_ledger_two_checkpoint_host_records_reopen_without_transfer_permission() {
    let t = Temp::new();
    let mut store = LibraryStore::open(&t.0).unwrap();
    let j = job();
    let e = entry_study(&j);
    let mut ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
    let id = ledger.reserve_id().unwrap();
    let (mut c, _) = ledger
        .begin_reserved(&id, j.clone(), e.plan.origin)
        .unwrap();
    c = ledger.prepare_entry_lifecycle(c, e.clone()).unwrap();
    for ordinal in 0..2 {
        let a = entry_host_action(&e, ordinal);
        let (next, checkpoint) = ledger.lifecycle_checkpoint(c, a.clone()).unwrap();
        c = next;
        assert_eq!(checkpoint.sequence(), 2 + ordinal * 3);
        assert!(!checkpoint.permits_flash());
        c = ledger
            .lifecycle_send(
                c,
                FirmwareLifecycleSendObservation {
                    action: ordinal,
                    wire_sha256: a.wire_sha256.clone(),
                    attempted: a.wire.len() as u64,
                    extent: FirmwareOsExtent::Known(a.wire.len() as u64),
                    drain: FirmwareDrainObservation::Returned,
                    started_sequence: 0,
                    finished_sequence: Some(0),
                    started_at_ns: 1 + ordinal * 2,
                    finished_at_ns: Some(2 + ordinal * 2),
                },
            )
            .unwrap();
        c = ledger
            .lifecycle_entry_host_settled(
                c,
                FirmwareEntryHostSettlement {
                    action: ordinal,
                    wire_sha256: a.wire_sha256,
                    study_sha256: e.study_sha256.clone(),
                },
            )
            .unwrap();
    }
    let retained = ledger
        .lifecycle_close(c, FirmwareLifecycleHostClosure::HostStoppedUnresolved)
        .unwrap();
    assert!(retained.local_checkpoint_confirmed);
    assert_eq!(retained.lifecycle.as_ref().unwrap().budget().actions, 2);
    assert!(!retained.can_resume());
    assert!(!retained.permits_flash());
    drop(ledger);
    drop(store);
    let mut reopened = LibraryStore::open(&t.0).unwrap();
    let retained = FirmwareAttemptLedger::open(&mut reopened)
        .unwrap()
        .load(&id)
        .unwrap();
    assert!(retained.local_checkpoint_confirmed);
    assert_eq!(retained.lifecycle.unwrap().next_action(), 2);
}
#[test]
fn entry_real_checkpoint_partial_sync_and_reread_faults_issue_no_cursor() {
    for point in [
        "append-partial-write",
        "append-write",
        "append-fsync",
        "before-checkpoint-rename",
        "after-checkpoint-rename",
        "after-checkpoint-directory-sync",
        "before-reread",
    ] {
        let t = Temp::new();
        let mut store = LibraryStore::open(&t.0).unwrap();
        let j = job();
        let e = entry_study(&j);
        let mut ledger = FirmwareAttemptLedger::open(&mut store).unwrap();
        let id = ledger.reserve_id().unwrap();
        let (c, _) = ledger
            .begin_reserved(&id, j.clone(), e.plan.origin)
            .unwrap();
        let c = ledger.prepare_entry_lifecycle(c, e.clone()).unwrap();
        fail(point, 0);
        assert!(
            ledger
                .lifecycle_checkpoint(c, entry_host_action(&e, 0))
                .is_err(),
            "{point}"
        );
        assert!(ledger.store.firmware_publication_uncertain);
        FAILURE.with(|v| *v.borrow_mut() = None);
        let retained = ledger.load(&id).unwrap();
        assert!(!retained.can_resume());
        assert!(!retained.permits_flash());
    }
}
