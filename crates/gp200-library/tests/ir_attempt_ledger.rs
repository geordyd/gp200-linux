//! Durable local evidence tests; no transport or device is opened.
use gp200_domain::*;
use gp200_library::*;
use gp200_protocol::{RawRecord, RawTransfer, WRITE_KIND};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

const WAV: &[u8] = include_bytes!("../../../tests/fixtures/device-assets/synthetic-ir-pcm24.wav");
const COEFFICIENTS: &[u8] =
    include_bytes!("../../../tests/fixtures/device-assets/synthetic-ir-coefficients.bin");
struct TempRoot(PathBuf);
impl TempRoot {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "gp200-ir-ledger-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn journal(&self, id: &IrAttemptId) -> PathBuf {
        self.0
            .join("ir-attempt-ledger")
            .join(format!("{}.journal", id.as_str()))
    }
}
impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Fixture {
    job: PreparedDeviceJob,
    identity: RawTransfer,
}
fn retained(bytes: &[u8]) -> RetainedJobBlob {
    RetainedJobBlob::checked(
        AssetId(format!("sha256:{}", JobSha256::of(bytes).hex())),
        Arc::from(bytes),
        JobSha256::of(bytes),
    )
    .unwrap()
}
fn metadata(
    name: [u8; 16],
    sum: u32,
    flags: u16,
    revision: u64,
    captured_at_ms: u64,
) -> DeviceAssetMetadata {
    let mut body = 19u16.to_le_bytes().to_vec();
    body.extend_from_slice(&flags.to_le_bytes());
    body.extend_from_slice(&sum.to_le_bytes());
    body.extend_from_slice(&name);
    DeviceAssetMetadata::from_transfer(
        AssetSlot::new(AssetBank::Ir, 19).unwrap(),
        RequestStamp {
            epoch: SessionEpoch(7),
            intent: IntentId(23),
        },
        Revision(revision),
        SnapshotSource::Hardware {
            epoch: SessionEpoch(7),
        },
        captured_at_ms,
        RawTransfer::from_records(
            WRITE_KIND,
            &[RawRecord {
                command: 0x1009,
                body,
            }],
        )
        .unwrap(),
    )
    .unwrap()
}
fn fixture() -> Fixture {
    let identity = RawTransfer::from_records(
        WRITE_KIND,
        &[RawRecord {
            command: 0x2001,
            body: vec![1, 0, 2, 0],
        }],
    )
    .unwrap();
    let caps = Capabilities::gp200_read_only();
    let session = DeviceJobSession::observe_normal(
        SessionEpoch(7),
        DeviceSelection {
            input_id: "synthetic:input".into(),
            output_id: "synthetic:output".into(),
            fingerprint_hint: None,
        },
        Revision(1),
        &identity,
        &DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Hardware,
            firmware: None,
            display_name: "Offline fixture".into(),
        },
        &caps,
    )
    .unwrap();
    let source = retained(WAV);
    let coefficients = retained(COEFFICIENTS);
    let mut before_name = [0; 16];
    before_name[..7].copy_from_slice(b"User IR");
    let baseline = Arc::new(metadata(before_name, 0, 0, 2, 99));
    let options: Arc<[u8]> = Arc::from(b"{\"truncate\":false}".as_slice());
    let recipe = NormalAssetRecipe::ir(
        AssetSlot::new(AssetBank::Ir, 19).unwrap(),
        "Linux IR test".into(),
        source.clone(),
        IrJobLengthPolicy::RejectLong,
        coefficients.clone(),
    )
    .unwrap();
    let conversion = JobConversion::checked(
        recipe.codec_revision().into(),
        options.clone(),
        JobSha256::of(&options),
        BTreeMap::new(),
    )
    .unwrap();
    let job = PreparedDeviceJob::prepare(
        JobId(4),
        session.clone(),
        source.clone(),
        recipe,
        conversion,
        baseline.clone(),
        AssetBackupReview::Unavailable {
            reason: "Only metadata is retained".into(),
        },
    )
    .unwrap();
    Fixture { job, identity }
}
fn material(fixture: &Fixture) -> IrAttemptMaterials {
    IrAttemptMaterials::from_prepared(&fixture.job, WAV, fixture.identity.payload()).unwrap()
}
fn status(
    context: IrObservationContextDto,
    at_ms: u64,
    slot: u16,
    flags: u16,
    value: u32,
) -> IrAttemptEventDto {
    let mut body = slot.to_le_bytes().to_vec();
    body.extend_from_slice(&flags.to_le_bytes());
    body.extend_from_slice(&value.to_le_bytes());
    IrAttemptEventDto::Status {
        context,
        at_ms,
        raw: RawTransfer::from_records(
            WRITE_KIND,
            &[RawRecord {
                command: 0x100b,
                body,
            }],
        )
        .unwrap(),
    }
}
fn send(
    context: IrObservationContextDto,
    at_ms: u64,
    count: u64,
    completion: IrSendCompletion,
) -> IrAttemptEventDto {
    IrAttemptEventDto::Send {
        context,
        at_ms,
        extent: IrTransportExtent::OfferedToProcess,
        wire_bytes: count,
        completion,
    }
}
fn complete() -> IrSendCompletion {
    IrSendCompletion::Process(IrProcessObservation {
        exit_code: Some(0),
        timed_out: false,
        reaped: true,
    })
}
fn metadata_event(binding: &IrAttemptBinding, at_ms: u64) -> IrAttemptEventDto {
    let pins = binding.pins();
    let snapshot = metadata(pins.name_raw, pins.additive_checksum, 0, 3, at_ms);
    IrAttemptEventDto::Metadata {
        context: binding.context().to_dto(),
        at_ms,
        metadata: IrMetadataObservationDto {
            slot: snapshot.slot(),
            stamp: snapshot.stamp(),
            revision: snapshot.revision(),
            source: snapshot.source().clone(),
            captured_at_ms: snapshot.captured_at_ms(),
            raw: (**snapshot.raw()).clone(),
        },
    }
}
fn first_frame_end(bytes: &[u8]) -> usize {
    44 + u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize
}

#[test]
fn reservation_is_permanent_across_reopen_and_library_writer_is_exclusive() {
    let root = TempRoot::new("ids");
    let mut store = LibraryStore::open(&root.0).unwrap();
    assert_eq!(
        LibraryStore::open(&root.0).err().unwrap().kind,
        ErrorKind::Busy
    );
    let old = {
        let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
        let id = ledger.reserve_id().unwrap();
        assert!(ledger.load(&id).unwrap().is_none());
        id
    };
    drop(store);
    let mut store = LibraryStore::open(&root.0).unwrap();
    let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
    assert_eq!(ledger.attempt_ids().unwrap(), vec![old.clone()]);
    let new = ledger.reserve_id().unwrap();
    assert_ne!(old, new);
    let marker = root
        .0
        .join("ir-attempt-ledger")
        .join(format!("{}.reserved", old.as_str()));
    assert!(
        fs::read(marker)
            .unwrap()
            .ends_with(format!("{}\n", old.as_str()).as_bytes())
    );
}

#[test]
fn full_local_observation_is_unverified_and_retains_unknown_full_words_and_all_materials() {
    let root = TempRoot::new("complete");
    let f = fixture();
    let mut store = LibraryStore::open(&root.0).unwrap();
    let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
    let id = ledger.reserve_id().unwrap();
    let binding = IrAttemptBinding::from_prepared(id.clone(), &f.job).unwrap();
    let mut cursor = ledger
        .begin_reserved(&id, binding.clone(), &material(&f), 100)
        .unwrap();
    cursor = ledger
        .append_observation(
            &cursor,
            send(binding.context().to_dto(), 101, 8570, complete()),
        )
        .unwrap();
    let raw = status(binding.context().to_dto(), 102, 19, 0x9001, 0xfedcba98);
    cursor = ledger.append_observation(&cursor, raw.clone()).unwrap();
    cursor = ledger
        .append_observation(&cursor, metadata_event(&binding, 103))
        .unwrap();
    let closed = ledger
        .close(&cursor, IrAttemptFinishReason::ObservationComplete)
        .unwrap();
    assert_eq!(closed.local_state, IrLocalAttemptState::LocalUnverified);
    assert_eq!(
        closed.reconstructed.summary().unwrap().final_state,
        IrAttemptFinalState::ContentUnverified
    );
    assert_eq!(
        closed.reconstructed.statuses()[0]
            .fields()
            .unwrap()
            .flags_raw,
        0x9001
    );
    assert_eq!(
        closed.reconstructed.statuses()[0]
            .fields()
            .unwrap()
            .value_raw,
        0xfedcba98
    );
    assert_eq!(closed.recorded.events[1], raw);
    assert_eq!(closed.materials.len(), 8);
    for blob in &closed.materials {
        let bytes = ledger.material_bytes(blob).unwrap();
        assert_eq!(ContentHash::of(&bytes), blob.sha256);
        assert_eq!(bytes.len() as u64, blob.bytes);
    }
    assert_eq!(
        ledger
            .append_observation(&cursor, status(binding.context().to_dto(), 104, 19, 1, 0))
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    let original = ledger.export_journal(&id).unwrap();
    drop(ledger);
    drop(store);
    let mut reopened = LibraryStore::open(&root.0).unwrap();
    let ledger = IrAttemptLedger::open(&mut reopened).unwrap();
    assert_eq!(ledger.export_journal(&id).unwrap(), original);
    assert_eq!(
        ledger.load(&id).unwrap().unwrap().journal_sha256,
        ContentHash::of(&original)
    );
}

#[test]
fn reopened_unfinished_journal_closes_import_projection_and_never_accepts_old_cursor() {
    let root = TempRoot::new("restart");
    let f = fixture();
    let mut store = LibraryStore::open(&root.0).unwrap();
    let (id, cursor, context) = {
        let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
        let id = ledger.reserve_id().unwrap();
        let binding = IrAttemptBinding::from_prepared(id.clone(), &f.job).unwrap();
        let cursor = ledger
            .begin_reserved(&id, binding.clone(), &material(&f), 100)
            .unwrap();
        (id, cursor, binding.context().to_dto())
    };
    let original = fs::read(root.journal(&id)).unwrap();
    let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
    let found = ledger.load(&id).unwrap().unwrap();
    assert_eq!(found.recorded.finish, None); // historical raw input is untouched
    assert_eq!(
        found.reconstructed.finish_reason(),
        Some(IrAttemptFinishReason::InterruptedByRestart)
    );
    assert_eq!(found.local_state, IrLocalAttemptState::Indeterminate);
    assert_eq!(
        ledger
            .append_observation(&cursor, send(context, 101, 8570, complete()))
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(fs::read(root.journal(&id)).unwrap(), original);
}

#[test]
fn truncated_and_corrupt_tail_keep_exact_raw_file_and_only_checked_prefix_events() {
    let root = TempRoot::new("tails");
    let f = fixture();
    let mut store = LibraryStore::open(&root.0).unwrap();
    let (id, full, prefix) = {
        let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
        let id = ledger.reserve_id().unwrap();
        let binding = IrAttemptBinding::from_prepared(id.clone(), &f.job).unwrap();
        let cursor = ledger
            .begin_reserved(&id, binding.clone(), &material(&f), 100)
            .unwrap();
        ledger
            .append_observation(&cursor, status(binding.context().to_dto(), 99, 19, 1, 0))
            .unwrap();
        let full = ledger.export_journal(&id).unwrap();
        let prefix = first_frame_end(&full);
        (id, full, prefix)
    };
    for cut in [
        prefix + 1,
        prefix + 8,
        prefix + 43,
        prefix + 44,
        full.len() - 1,
    ] {
        let partial = &full[..cut];
        fs::write(root.journal(&id), partial).unwrap();
        let ledger = IrAttemptLedger::open(&mut store).unwrap();
        let found = ledger.load(&id).unwrap().unwrap();
        assert_eq!(found.valid_prefix_bytes, prefix);
        assert!(found.recorded.events.is_empty());
        assert!(matches!(
            found.issues[0],
            IrJournalIssue::IncompleteTail { .. }
        ));
        assert_eq!(found.local_state, IrLocalAttemptState::Indeterminate);
        assert_eq!(fs::read(root.journal(&id)).unwrap(), partial);
    }
    let mut corrupt = full.clone();
    corrupt[prefix + 12] ^= 1;
    fs::write(root.journal(&id), &corrupt).unwrap();
    let ledger = IrAttemptLedger::open(&mut store).unwrap();
    let found = ledger.load(&id).unwrap().unwrap();
    assert_eq!(found.valid_prefix_bytes, prefix);
    assert!(found.recorded.events.is_empty());
    assert!(matches!(
        found.issues[0],
        IrJournalIssue::CorruptTail { .. }
    ));
    assert_eq!(ledger.export_journal(&id).unwrap(), corrupt);
}

#[test]
fn exchanged_identity_or_materials_and_modified_immutable_blob_are_rejected() {
    let root = TempRoot::new("material-integrity");
    let f = fixture();
    let mut store = LibraryStore::open(&root.0).unwrap();
    let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
    let id = ledger.reserve_id().unwrap();
    let binding = IrAttemptBinding::from_prepared(id.clone(), &f.job).unwrap();
    let mut wrong: BTreeMap<_, _> = [
        IrMaterialRole::SourceWav,
        IrMaterialRole::Coefficients,
        IrMaterialRole::IntendedTlv,
        IrMaterialRole::IntendedWire,
        IrMaterialRole::BaselinePayload,
        IrMaterialRole::IdentityPayload,
        IrMaterialRole::SessionJson,
        IrMaterialRole::ReviewPreimage,
    ]
    .into_iter()
    .map(|role| (role, material(&f).bytes(role).unwrap().to_vec()))
    .collect();
    wrong.get_mut(&IrMaterialRole::IdentityPayload).unwrap()[4] ^= 1;
    assert!(
        ledger
            .begin_reserved(
                &id,
                binding.clone(),
                &IrAttemptMaterials::from_retained(wrong).unwrap(),
                100
            )
            .is_err()
    );
    assert!(!root.journal(&id).exists());
    let cursor = ledger
        .begin_reserved(&id, binding.clone(), &material(&f), 100)
        .unwrap();
    let refs = ledger.load(&id).unwrap().unwrap().materials;
    let original = ledger.export_journal(&id).unwrap();
    let source = refs
        .iter()
        .find(|entry| entry.role == IrMaterialRole::SourceWav)
        .unwrap();
    let path = root.0.join("blobs").join(source.sha256.as_str());
    let mut bytes = fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    fs::write(path, bytes).unwrap();
    assert!(ledger.load(&id).is_err());
    assert!(
        ledger
            .append_observation(
                &cursor,
                send(binding.context().to_dto(), 101, 8570, complete())
            )
            .is_err()
    );
    assert_eq!(fs::read(root.journal(&id)).unwrap(), original);
}

#[test]
fn stale_cursor_and_cross_attempt_binding_do_not_publish_another_record() {
    let root = TempRoot::new("cursor");
    let f = fixture();
    let mut store = LibraryStore::open(&root.0).unwrap();
    let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
    let id = ledger.reserve_id().unwrap();
    let other = ledger.reserve_id().unwrap();
    let binding = IrAttemptBinding::from_prepared(id.clone(), &f.job).unwrap();
    assert!(
        ledger
            .begin_reserved(&other, binding.clone(), &material(&f), 100)
            .is_err()
    );
    let old = ledger
        .begin_reserved(&id, binding.clone(), &material(&f), 100)
        .unwrap();
    let new = ledger
        .append_observation(&old, status(binding.context().to_dto(), 99, 19, 1, 0))
        .unwrap();
    let bytes = ledger.export_journal(&id).unwrap();
    assert_eq!(
        ledger
            .append_observation(
                &old,
                send(binding.context().to_dto(), 101, 8570, complete())
            )
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(ledger.export_journal(&id).unwrap(), bytes);
    ledger
        .close(&new, IrAttemptFinishReason::ObservationFailed)
        .unwrap();
}

#[test]
fn pre_send_mismatched_and_duplicate_status_preserve_receive_order_and_stay_indeterminate() {
    let root = TempRoot::new("raw-order");
    let f = fixture();
    let mut store = LibraryStore::open(&root.0).unwrap();
    let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
    let id = ledger.reserve_id().unwrap();
    let binding = IrAttemptBinding::from_prepared(id.clone(), &f.job).unwrap();
    let mut cursor = ledger
        .begin_reserved(&id, binding.clone(), &material(&f), 100)
        .unwrap();
    let early = status(binding.context().to_dto(), 100, 19, 1, 0);
    cursor = ledger.append_observation(&cursor, early.clone()).unwrap();
    cursor = ledger
        .append_observation(
            &cursor,
            send(binding.context().to_dto(), 101, 8570, complete()),
        )
        .unwrap();
    cursor = ledger
        .append_observation(&cursor, status(binding.context().to_dto(), 102, 19, 1, 0))
        .unwrap();
    let mut wrong = binding.context().to_dto();
    wrong.session.selection.output_id = "different:endpoint".into();
    cursor = ledger
        .append_observation(&cursor, status(wrong, 103, 19, 0xabcd, 0x12345678))
        .unwrap();
    cursor = ledger
        .append_observation(&cursor, metadata_event(&binding, 104))
        .unwrap();
    let found = ledger
        .close(&cursor, IrAttemptFinishReason::ObservationComplete)
        .unwrap();
    assert_eq!(found.recorded.events[0], early);
    assert!(!found.reconstructed.statuses()[0].after_accounted_send());
    assert_eq!(found.reconstructed.statuses()[1].duplicate_of(), Some(0));
    assert!(!found.reconstructed.statuses()[2].relation().session_matches);
    assert_eq!(found.local_state, IrLocalAttemptState::Indeterminate);
}

#[test]
fn old_import_missing_review_preimage_has_explicit_gap_and_cannot_reuse_existing_id() {
    let root = TempRoot::new("old-import");
    let f = fixture();
    let mut store = LibraryStore::open(&root.0).unwrap();
    let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
    let id = IrAttemptId::parse("123456789abcdef0123456789abcdef0").unwrap();
    let binding = IrAttemptBinding::from_prepared(id.clone(), &f.job).unwrap();
    let mut evidence = IrAttemptEvidence::start(binding.clone(), 100);
    evidence
        .observe_send(
            binding.context(),
            101,
            IrTransportExtent::OfferedToProcess,
            8570,
            complete(),
        )
        .unwrap();
    if let IrAttemptEventDto::Status { raw, .. } = status(binding.context().to_dto(), 102, 19, 1, 0)
    {
        evidence
            .observe_status(binding.context(), 102, raw)
            .unwrap();
    }
    evidence
        .observe_metadata(
            binding.context(),
            103,
            metadata(
                binding.pins().name_raw,
                binding.pins().additive_checksum,
                0,
                3,
                103,
            ),
        )
        .unwrap();
    evidence
        .finish(IrAttemptFinishReason::ObservationComplete)
        .unwrap();
    let mut bytes: BTreeMap<_, _> = [
        IrMaterialRole::SourceWav,
        IrMaterialRole::Coefficients,
        IrMaterialRole::IntendedTlv,
        IrMaterialRole::IntendedWire,
        IrMaterialRole::BaselinePayload,
        IrMaterialRole::IdentityPayload,
        IrMaterialRole::SessionJson,
    ]
    .into_iter()
    .map(|role| (role, material(&f).bytes(role).unwrap().to_vec()))
    .collect();
    let missing = IrAttemptMaterials::from_retained(bytes.clone()).unwrap();
    let imported = ledger.import_closed(evidence.to_dto(), &missing).unwrap();
    assert_eq!(
        ledger
            .original_import_bytes(imported.original_import.as_ref().unwrap())
            .unwrap(),
        serde_json::to_vec(&evidence.to_dto()).unwrap()
    );
    assert_eq!(imported.local_state, IrLocalAttemptState::Indeterminate);
    assert!(
        imported
            .issues
            .contains(&IrJournalIssue::MissingReviewPreimage)
    );
    assert_eq!(
        imported.recorded.binding.review_sha256,
        f.job.review_sha256().hex()
    );
    assert!(
        imported
            .materials
            .iter()
            .all(|entry| entry.role != IrMaterialRole::ReviewPreimage)
    );
    assert_eq!(
        ledger
            .import_closed(evidence.to_dto(), &missing)
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    // Current review bytes cannot silently repair an older receipt: the same ID
    // is permanently occupied, including when its missing bytes are now known.
    bytes.insert(
        IrMaterialRole::ReviewPreimage,
        f.job.review_preimage().unwrap(),
    );
    assert_eq!(
        ledger
            .import_closed(
                evidence.to_dto(),
                &IrAttemptMaterials::from_retained(bytes).unwrap()
            )
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
}

#[test]
fn incomplete_reserved_marker_and_partial_start_remain_retained_and_non_resumable() {
    let root = TempRoot::new("partial-start");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let id = {
        let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
        ledger.reserve_id().unwrap()
    };
    let marker = root
        .0
        .join("ir-attempt-ledger")
        .join(format!("{}.reserved", id.as_str()));
    fs::write(&marker, b"partial").unwrap();
    let ledger = IrAttemptLedger::open(&mut store).unwrap();
    assert!(ledger.attempt_ids().unwrap().contains(&id));
    assert!(ledger.load(&id).is_err());
    assert_eq!(fs::read(&marker).unwrap(), b"partial");
    assert!(ledger.export_raw_journal(&id).is_err());
    drop(ledger);
    fs::write(
        &marker,
        format!("gp200-ir-observation-id-v1:{}\n", id.as_str()),
    )
    .unwrap();
    fs::write(root.journal(&id), b"GP2IRL01\x01").unwrap();
    let f = fixture();
    let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
    assert!(ledger.load(&id).is_err());
    assert_eq!(
        ledger
            .begin_reserved(
                &id,
                IrAttemptBinding::from_prepared(id.clone(), &f.job).unwrap(),
                &material(&f),
                100
            )
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(fs::read(root.journal(&id)).unwrap(), b"GP2IRL01\x01");
}

#[test]
fn event_limit_and_large_raw_status_do_not_append_rejected_bytes() {
    let root = TempRoot::new("bounds");
    let f = fixture();
    let mut store = LibraryStore::open(&root.0).unwrap();
    let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
    let id = ledger.reserve_id().unwrap();
    let binding = IrAttemptBinding::from_prepared(id.clone(), &f.job).unwrap();
    let mut cursor = ledger
        .begin_reserved(&id, binding.clone(), &material(&f), 100)
        .unwrap();
    let oversized = IrAttemptEventDto::Status {
        context: binding.context().to_dto(),
        at_ms: 101,
        raw: RawTransfer::from_records(
            WRITE_KIND,
            &[RawRecord {
                command: 0x100b,
                body: vec![0; 4096],
            }],
        )
        .unwrap(),
    };
    let before = ledger.export_journal(&id).unwrap();
    assert!(ledger.append_observation(&cursor, oversized).is_err());
    assert_eq!(ledger.export_journal(&id).unwrap(), before);
    for i in 0..256 {
        cursor = ledger
            .append_observation(
                &cursor,
                status(binding.context().to_dto(), 101 + i, 19, 1, 0),
            )
            .unwrap();
    }
    let before = ledger.export_journal(&id).unwrap();
    let mut offset = 0;
    for sequence in 0..257 {
        let start = offset;
        offset +=
            44 + u32::from_le_bytes(before[start + 8..start + 12].try_into().unwrap()) as usize;
        if matches!(sequence, 0 | 128 | 256) {
            let checkpoint: serde_json::Value = serde_json::from_slice(
                &fs::read(
                    root.0
                        .join("ir-attempt-ledger")
                        .join(format!("{}.checkpoint-{sequence:03}", id.as_str())),
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(
                checkpoint["boundary"]["prefix_bytes"],
                serde_json::json!(offset)
            );
            assert_eq!(
                checkpoint["boundary"]["prefix_sha256"],
                serde_json::json!(ContentHash::of(&before[..offset]))
            );
            assert_eq!(
                checkpoint["boundary"]["last_record_sha256"],
                serde_json::json!(ContentHash::of(&before[start..offset]))
            );
        }
    }
    assert_eq!(offset, before.len());
    assert!(
        ledger
            .append_observation(&cursor, status(binding.context().to_dto(), 400, 19, 1, 0))
            .is_err()
    );
    assert_eq!(ledger.export_journal(&id).unwrap(), before);
    ledger
        .close(&cursor, IrAttemptFinishReason::Timeout)
        .unwrap();
}

#[cfg(unix)]
#[test]
fn ledger_and_material_symlinks_are_rejected_without_writing_targets() {
    use std::os::unix::fs::symlink;
    let root = TempRoot::new("symlinks");
    let outside = TempRoot::new("outside");
    let mut store = LibraryStore::open(&root.0).unwrap();
    symlink(&outside.0, root.0.join("ir-attempt-ledger")).unwrap();
    assert!(IrAttemptLedger::open(&mut store).is_err());
    assert!(fs::read_dir(&outside.0).unwrap().next().is_none());
    fs::remove_file(root.0.join("ir-attempt-ledger")).unwrap();
    let f = fixture();
    let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
    let id = ledger.reserve_id().unwrap();
    let binding = IrAttemptBinding::from_prepared(id.clone(), &f.job).unwrap();
    ledger
        .begin_reserved(&id, binding, &material(&f), 100)
        .unwrap();
    let sentinel = outside.0.join("sentinel");
    fs::write(&sentinel, b"untouched").unwrap();
    fs::remove_file(root.journal(&id)).unwrap();
    symlink(&sentinel, root.journal(&id)).unwrap();
    assert!(ledger.load(&id).is_err());
    assert!(ledger.export_raw_journal(&id).is_err());
    let checkpoint = root
        .0
        .join("ir-attempt-ledger")
        .join(format!("{}.checkpoint-000", id.as_str()));
    fs::remove_file(&checkpoint).unwrap();
    symlink(&sentinel, checkpoint).unwrap();
    assert!(ledger.raw_checkpoint_receipts(&id).is_err());
    assert_eq!(fs::read(sentinel).unwrap(), b"untouched");
    let ledger_root = root.0.join("ir-attempt-ledger");
    let moved = root.0.join("retained-ledger");
    fs::rename(&ledger_root, &moved).unwrap();
    symlink(&outside.0, &ledger_root).unwrap();
    assert!(ledger.reserve_id().is_err());
    assert_eq!(fs::read_dir(&outside.0).unwrap().count(), 1); // untouched sentinel only
}

#[test]
fn closed_prefix_with_bad_tail_preserves_recorded_finish_and_separate_interruption() {
    let root = TempRoot::new("closed-tail");
    let f = fixture();
    let mut store = LibraryStore::open(&root.0).unwrap();
    let (id, cursor, recorded, clean, record_hash, boundary) = {
        let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
        let id = ledger.reserve_id().unwrap();
        let binding = IrAttemptBinding::from_prepared(id.clone(), &f.job).unwrap();
        let cursor = ledger
            .begin_reserved(&id, binding, &material(&f), 100)
            .unwrap();
        let closed = ledger
            .close(&cursor, IrAttemptFinishReason::Timeout)
            .unwrap();
        (
            id.clone(),
            cursor,
            closed.recorded.clone(),
            ledger.export_journal(&id).unwrap(),
            closed.record_sha256.clone(),
            closed.confirmed_local_fsync_boundary.clone(),
        )
    };
    for tail in [
        b"broken tail".as_slice(),
        b"GP2IRL01\x10\x00\x00\x00".as_slice(),
    ] {
        let mut raw = clean.clone();
        raw.extend_from_slice(tail);
        fs::write(root.journal(&id), &raw).unwrap();
        let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
        let found = ledger.load(&id).unwrap().unwrap();
        assert_eq!(found.recorded, recorded);
        assert_eq!(found.recorded.finish, Some(IrAttemptFinishReason::Timeout));
        assert_eq!(
            found.reconstructed.finish_reason(),
            Some(IrAttemptFinishReason::InterruptedByRestart)
        );
        assert_eq!(found.valid_prefix_bytes, clean.len());
        assert_eq!(found.record_sha256, record_hash);
        assert_eq!(found.confirmed_local_fsync_boundary, boundary);
        assert_eq!(found.local_state, IrLocalAttemptState::Indeterminate);
        assert_eq!(ledger.export_journal(&id).unwrap(), raw);
        assert_eq!(
            ledger
                .close(&cursor, IrAttemptFinishReason::ObservationComplete)
                .unwrap_err()
                .kind,
            ErrorKind::Conflict
        );
        assert_eq!(fs::read(root.journal(&id)).unwrap(), raw);
    }
}

#[test]
fn a_complete_unsynced_suffix_is_retained_separately_from_confirmed_boundary() {
    let root = TempRoot::new("boundary");
    let f = fixture();
    let mut store = LibraryStore::open(&root.0).unwrap();
    let (id, cursor, start_boundary, raw) = {
        let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
        let id = ledger.reserve_id().unwrap();
        let binding = IrAttemptBinding::from_prepared(id.clone(), &f.job).unwrap();
        let cursor = ledger
            .begin_reserved(&id, binding.clone(), &material(&f), 100)
            .unwrap();
        let start = ledger.load(&id).unwrap().unwrap();
        let cursor = ledger
            .append_observation(
                &cursor,
                send(binding.context().to_dto(), 101, 8570, complete()),
            )
            .unwrap();
        let raw = ledger.export_journal(&id).unwrap();
        (id, cursor, start.confirmed_local_fsync_boundary, raw)
    };
    // Model death after a complete write, before a matching fsync receipt.
    fs::remove_file(
        root.0
            .join("ir-attempt-ledger")
            .join(format!("{}.checkpoint-001", id.as_str())),
    )
    .unwrap();
    let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
    let found = ledger.load(&id).unwrap().unwrap();
    assert_eq!(found.recorded.events.len(), 1);
    assert_eq!(found.valid_prefix_bytes, raw.len());
    assert_eq!(found.confirmed_local_fsync_boundary, start_boundary);
    assert!(
        found
            .issues
            .iter()
            .any(|issue| matches!(issue, IrJournalIssue::UnconfirmedLocalDurability { .. }))
    );
    assert_eq!(found.local_state, IrLocalAttemptState::Indeterminate);
    assert_eq!(
        ledger
            .close(&cursor, IrAttemptFinishReason::ObservationComplete)
            .unwrap_err()
            .kind,
        ErrorKind::Conflict
    );
    assert_eq!(ledger.export_journal(&id).unwrap(), raw);
}

#[test]
fn rehashed_bad_sequence_or_invalid_event_cannot_leak_into_the_checked_prefix() {
    use sha2::{Digest, Sha256};
    let root = TempRoot::new("sequence");
    let f = fixture();
    let mut store = LibraryStore::open(&root.0).unwrap();
    let (id, full) = {
        let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
        let id = ledger.reserve_id().unwrap();
        let binding = IrAttemptBinding::from_prepared(id.clone(), &f.job).unwrap();
        let cursor = ledger
            .begin_reserved(&id, binding.clone(), &material(&f), 100)
            .unwrap();
        ledger
            .append_observation(
                &cursor,
                send(binding.context().to_dto(), 101, 8570, complete()),
            )
            .unwrap();
        (id.clone(), ledger.export_journal(&id).unwrap())
    };
    let prefix = first_frame_end(&full);
    let original: serde_json::Value = serde_json::from_slice(&full[prefix + 44..]).unwrap();
    for bad_sequence in [true, false] {
        let mut bad = original.clone();
        if bad_sequence {
            bad["sequence"] = serde_json::json!(0);
        } else {
            bad["entry"]["Observed"]["event"]["Send"]["wire_bytes"] = serde_json::json!(8571);
        }
        let json = serde_json::to_vec(&bad).unwrap();
        let mut raw = full[..prefix].to_vec();
        raw.extend_from_slice(b"GP2IRL01");
        raw.extend_from_slice(&(json.len() as u32).to_le_bytes());
        raw.extend_from_slice(&Sha256::digest(&json));
        raw.extend_from_slice(&json);
        fs::write(root.journal(&id), &raw).unwrap();
        let ledger = IrAttemptLedger::open(&mut store).unwrap();
        let found = ledger.load(&id).unwrap().unwrap();
        assert_eq!(found.valid_prefix_bytes, prefix);
        assert!(found.recorded.events.is_empty());
        assert_eq!(found.recorded.finish, None);
        assert_eq!(
            found.reconstructed.finish_reason(),
            Some(IrAttemptFinishReason::InterruptedByRestart)
        );
        assert_eq!(found.local_state, IrLocalAttemptState::Indeterminate);
        assert!(matches!(
            found.issues[0],
            IrJournalIssue::CorruptTail { .. }
        ));
        assert_eq!(ledger.export_journal(&id).unwrap(), raw);
    }
}

#[test]
fn validly_framed_event_or_second_finish_after_closure_rolls_back_to_exact_closed_prefix() {
    use sha2::{Digest, Sha256};
    let root = TempRoot::new("closed-framed-tail");
    let f = fixture();
    let mut store = LibraryStore::open(&root.0).unwrap();
    let (id, binding, recorded, clean, record_hash, boundary) = {
        let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
        let id = ledger.reserve_id().unwrap();
        let binding = IrAttemptBinding::from_prepared(id.clone(), &f.job).unwrap();
        let cursor = ledger
            .begin_reserved(&id, binding.clone(), &material(&f), 100)
            .unwrap();
        let closed = ledger
            .close(&cursor, IrAttemptFinishReason::Timeout)
            .unwrap();
        (
            id.clone(),
            binding,
            closed.recorded.clone(),
            ledger.export_journal(&id).unwrap(),
            closed.record_sha256.clone(),
            closed.confirmed_local_fsync_boundary.clone(),
        )
    };
    for entry in [
        serde_json::json!({"Closed": {"reason": "ObservationComplete"}}),
        serde_json::json!({"Observed": {"event": status(binding.context().to_dto(), 101, 19, 1, 0)}}),
    ] {
        let json = serde_json::to_vec(&serde_json::json!({
            "schema": 1, "attempt": id, "sequence": 2,
            "previous": record_hash, "entry": entry,
        }))
        .unwrap();
        let mut raw = clean.clone();
        raw.extend_from_slice(b"GP2IRL01");
        raw.extend_from_slice(&(json.len() as u32).to_le_bytes());
        raw.extend_from_slice(&Sha256::digest(&json));
        raw.extend_from_slice(&json);
        fs::write(root.journal(&id), &raw).unwrap();
        let ledger = IrAttemptLedger::open(&mut store).unwrap();
        let found = ledger.load(&id).unwrap().unwrap();
        assert_eq!(found.recorded, recorded);
        assert_eq!(found.recorded.finish, Some(IrAttemptFinishReason::Timeout));
        assert_eq!(found.valid_prefix_bytes, clean.len());
        assert_eq!(found.record_sha256, record_hash);
        assert_eq!(found.confirmed_local_fsync_boundary, boundary);
        assert_eq!(
            found.reconstructed.finish_reason(),
            Some(IrAttemptFinishReason::InterruptedByRestart)
        );
        assert!(matches!(
            found.issues[0],
            IrJournalIssue::CorruptTail { .. }
        ));
        assert_eq!(ledger.export_journal(&id).unwrap(), raw);
    }
}

#[test]
fn raw_diagnostic_exports_retain_unloadable_journal_and_exact_invalid_checkpoint_bytes() {
    let root = TempRoot::new("raw-diagnostics");
    let f = fixture();
    let mut store = LibraryStore::open(&root.0).unwrap();
    let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
    let id = ledger.reserve_id().unwrap();
    let binding = IrAttemptBinding::from_prepared(id.clone(), &f.job).unwrap();
    let cursor = ledger
        .begin_reserved(&id, binding, &material(&f), 100)
        .unwrap();
    let closed = ledger
        .close(&cursor, IrAttemptFinishReason::Timeout)
        .unwrap();
    assert_eq!(closed.record_count(), 2);
    let checkpoint = root.0.join("ir-attempt-ledger");
    let first = fs::read(checkpoint.join(format!("{}.checkpoint-000", id.as_str()))).unwrap();
    let second_path = checkpoint.join(format!("{}.checkpoint-001", id.as_str()));
    let invalid = b"Original invalid checkpoint JSON\n";
    fs::write(&second_path, invalid).unwrap();
    let mut raw = ledger.export_raw_journal(&id).unwrap();
    raw[12] ^= 1; // Corrupt the first complete frame's JSON digest.
    fs::write(root.journal(&id), &raw).unwrap();
    assert!(ledger.load(&id).is_err());
    assert!(ledger.export_journal(&id).is_err());
    assert_eq!(ledger.export_raw_journal(&id).unwrap(), raw);
    let receipts = ledger.raw_checkpoint_receipts(&id).unwrap();
    assert_eq!(receipts.len(), 2);
    assert_eq!(receipts[0].sequence, 0);
    assert_eq!(receipts[0].sha256, ContentHash::of(&first));
    assert_eq!(receipts[0].bytes, first);
    assert_eq!(receipts[1].sequence, 1);
    assert_eq!(receipts[1].sha256, ContentHash::of(invalid));
    assert_eq!(receipts[1].bytes, invalid);
    assert_eq!(fs::read(root.journal(&id)).unwrap(), raw);
    fs::write(&second_path, vec![0; 2049]).unwrap();
    assert!(ledger.raw_checkpoint_receipts(&id).is_err());
    fs::write(root.journal(&id), vec![0; 2 * 1024 * 1024 + 1]).unwrap();
    assert!(ledger.export_raw_journal(&id).is_err());
}
