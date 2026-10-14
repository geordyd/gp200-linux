//! IR history worker and export regression tests.
#[path = "common/ir_history_fixture.rs"]
mod fixture;
use fixture::*;
use gp200_domain::*;
use gp200_library::*;
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    sync::Arc,
    time::{Duration, Instant},
};

fn entries(path: &std::path::Path) -> BTreeMap<String, Vec<u8>> {
    let mut zip = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
    let mut entries = BTreeMap::new();
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).unwrap();
        let name = entry.name().to_owned();
        let mut bytes = vec![];
        entry.read_to_end(&mut bytes).unwrap();
        assert!(entries.insert(name, bytes).is_none());
    }
    entries
}
fn request(detail: &IrHistoryDetail, destination: std::path::PathBuf) -> IrHistoryExportRequest {
    IrHistoryExportRequest {
        attempt: detail.attempt.clone(),
        observed_snapshot_sha256: detail.snapshot_sha256.clone(),
        kind: detail.export_kind,
        destination,
    }
}

#[test]
fn empty_page_and_invalid_offsets_are_bounded_without_creating_attempt_authority() {
    let root = TempRoot::new("history-empty");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let page = ir_history_page(&mut store, IrHistoryPageRequest::default(), &|| false).unwrap();
    assert_eq!(page.total, 0);
    assert!(page.rows.is_empty());
    for offset in [1, 8, usize::MAX] {
        assert!(
            ir_history_page(
                &mut store,
                IrHistoryPageRequest {
                    offset,
                    roster_sha256: Some(page.roster_sha256.clone())
                },
                &|| false
            )
            .is_err()
        );
    }
    assert!(
        IrAttemptLedger::open(&mut store)
            .unwrap()
            .attempt_ids()
            .unwrap()
            .is_empty()
    );
}
#[test]
fn pages_have_eight_rows_and_a_changed_roster_cannot_shift_a_reviewed_next_page() {
    let root = TempRoot::new("history-pages");
    let mut store = LibraryStore::open(&root.0).unwrap();
    for _ in 0..9 {
        populate(&mut store, true);
    }
    let page = ir_history_page(&mut store, IrHistoryPageRequest::default(), &|| false).unwrap();
    assert_eq!(page.rows.len(), 8);
    assert_eq!(page.total, 9);
    let next = ir_history_page(
        &mut store,
        IrHistoryPageRequest {
            offset: 8,
            roster_sha256: Some(page.roster_sha256.clone()),
        },
        &|| false,
    )
    .unwrap();
    assert_eq!(next.rows.len(), 1);
    assert!(
        ir_history_page(
            &mut store,
            IrHistoryPageRequest {
                offset: 8,
                roster_sha256: None
            },
            &|| false
        )
        .is_err()
    );
    assert!(
        page.rows
            .iter()
            .all(|row| row.attempt != next.rows[0].attempt)
    );
    populate(&mut store, true);
    assert_eq!(
        ir_history_page(
            &mut store,
            IrHistoryPageRequest {
                offset: 8,
                roster_sha256: Some(page.roster_sha256)
            },
            &|| false
        )
        .unwrap_err()
        .kind,
        ErrorKind::Conflict
    );
}
#[test]
fn missing_required_material_invalidates_a_review_and_becomes_an_explicit_raw_only_row() {
    let root = TempRoot::new("history-missing-material");
    let output = TempRoot::new("history-missing-material-output");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let id = populate(&mut store, true);
    let detail = ir_history_detail(&mut store, &id, &|| false).unwrap();
    let pin = &detail.checked.as_ref().unwrap().materials[0];
    fs::remove_file(root.0.join("blobs").join(pin.sha256.as_str())).unwrap();
    let destination = output.0.join("stale-full.zip");
    assert!(
        export_ir_history(&mut store, request(&detail, destination.clone()), &|| false).is_err()
    );
    assert!(!destination.exists());
    let page = ir_history_page(&mut store, IrHistoryPageRequest::default(), &|| false).unwrap();
    assert_eq!(page.rows[0].attempt, id);
    assert!(page.rows[0].error.is_some());
    let unreadable = ir_history_detail(&mut store, &id, &|| false).unwrap();
    assert!(unreadable.checked.is_none());
    assert_eq!(unreadable.export_kind, IrHistoryExportKind::RawDiagnostics);
    let raw = output.0.join("missing-material-diagnostics.zip");
    export_ir_history(&mut store, request(&unreadable, raw.clone()), &|| false).unwrap();
    let bytes = entries(&raw);
    assert_eq!(
        bytes["original/journal.bin"],
        fs::read(root.journal(&id)).unwrap()
    );
    assert!(
        !bytes
            .keys()
            .any(|name| name.starts_with("original/materials/"))
    );
}
#[test]
fn closed_prefix_and_raw_tail_export_exact_bytes_with_separate_derived_finish() {
    let root = TempRoot::new("history-tail");
    let output = TempRoot::new("history-output");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let id = populate(&mut store, true);
    let mut bytes = fs::read(root.journal(&id)).unwrap();
    let prefix = bytes.len();
    bytes.extend_from_slice(b"bad tail");
    fs::write(root.journal(&id), &bytes).unwrap();
    let detail = ir_history_detail(&mut store, &id, &|| false).unwrap();
    let checked = detail.checked.as_ref().unwrap();
    assert_eq!(
        checked.recorded.finish,
        Some(IrAttemptFinishReason::ObservationComplete)
    );
    assert_eq!(
        checked.reconstructed.finish_reason(),
        Some(IrAttemptFinishReason::InterruptedByRestart)
    );
    assert_eq!(checked.valid_prefix_bytes, prefix);
    assert_eq!(
        detail.checked_prefix_sha256,
        Some(ContentHash::of(&bytes[..prefix]))
    );
    assert_eq!(checked.local_state, IrLocalAttemptState::Indeterminate);
    let path = output.0.join("exact.zip");
    let receipt = export_ir_history(&mut store, request(&detail, path.clone()), &|| false).unwrap();
    let exported = entries(&path);
    assert_eq!(exported["original/journal.bin"], bytes);
    assert_eq!(
        ContentHash::of(&fs::read(&path).unwrap()),
        receipt.archive_sha256
    );
    let recorded: IrAttemptEvidenceDto =
        serde_json::from_slice(&exported["derived/accepted-recorded-prefix.json"]).unwrap();
    let derived: IrAttemptEvidenceDto =
        serde_json::from_slice(&exported["derived/current-interpretation.json"]).unwrap();
    assert_eq!(
        recorded.finish,
        Some(IrAttemptFinishReason::ObservationComplete)
    );
    assert_eq!(
        derived.finish,
        Some(IrAttemptFinishReason::InterruptedByRestart)
    );
    for pin in &detail.observed_files {
        assert_eq!(exported[&pin.path].len() as u64, pin.bytes);
        assert_eq!(ContentHash::of(&exported[&pin.path]), pin.sha256);
    }
    assert_eq!(fs::read(root.journal(&id)).unwrap(), bytes);
}
#[test]
fn unreadable_start_stays_in_the_list_and_only_raw_diagnostics_can_export() {
    let root = TempRoot::new("history-invalid");
    let output = TempRoot::new("history-diagnostic-output");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let id = populate(&mut store, true);
    let bytes = b"uninterpretable original bytes";
    fs::write(root.journal(&id), bytes).unwrap();
    let page = ir_history_page(&mut store, IrHistoryPageRequest::default(), &|| false).unwrap();
    assert_eq!(page.rows[0].attempt, id);
    assert!(page.rows[0].error.is_some());
    let detail = ir_history_detail(&mut store, &id, &|| false).unwrap();
    assert!(detail.checked.is_none());
    assert!(detail.checked_prefix_sha256.is_none());
    assert_eq!(detail.export_kind, IrHistoryExportKind::RawDiagnostics);
    let checked_path = output.0.join("forbidden-checked.zip");
    let mut checked_request = request(&detail, checked_path.clone());
    checked_request.kind = IrHistoryExportKind::CheckedEvidence;
    assert!(export_ir_history(&mut store, checked_request, &|| false).is_err());
    assert!(!checked_path.exists());
    let raw_path = output.0.join("raw-only.zip");
    export_ir_history(&mut store, request(&detail, raw_path.clone()), &|| false).unwrap();
    let exported = entries(&raw_path);
    assert_eq!(exported["original/journal.bin"], bytes);
    assert!(!exported.contains_key("derived/accepted-recorded-prefix.json"));
    assert!(
        !exported
            .keys()
            .any(|name| name.starts_with("original/materials/"))
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&exported["derived/export-manifest.json"]).unwrap();
    assert_eq!(metadata["content_verified"], false);
    assert!(metadata["load_error"].is_string());
}
#[test]
fn changed_journal_checkpoint_and_material_after_dialog_block_the_full_export() {
    for change in ["journal", "checkpoint", "material"] {
        let root = TempRoot::new(change);
        let output = TempRoot::new("history-changed-output");
        let mut store = LibraryStore::open(&root.0).unwrap();
        let id = populate(&mut store, true);
        let detail = ir_history_detail(&mut store, &id, &|| false).unwrap();
        match change {
            "journal" => {
                let mut bytes = fs::read(root.journal(&id)).unwrap();
                bytes.push(1);
                fs::write(root.journal(&id), bytes).unwrap();
            }
            "checkpoint" => {
                fs::write(
                    root.0
                        .join("ir-attempt-ledger")
                        .join(format!("{}.checkpoint-000", id.as_str())),
                    b"changed checkpoint",
                )
                .unwrap();
            }
            "material" => {
                let pin = &detail.checked.as_ref().unwrap().materials[0];
                fs::write(
                    root.0.join("blobs").join(pin.sha256.as_str()),
                    b"changed material",
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        let path = output.0.join("stale.zip");
        assert!(export_ir_history(&mut store, request(&detail, path.clone()), &|| false).is_err());
        assert!(!path.exists());
    }
}
#[test]
fn invalid_checkpoint_bytes_are_exported_exactly_without_confirming_a_boundary() {
    let root = TempRoot::new("history-invalid-checkpoint");
    let output = TempRoot::new("history-invalid-checkpoint-output");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let id = populate(&mut store, true);
    let raw = b"not valid checkpoint JSON";
    fs::write(
        root.0
            .join("ir-attempt-ledger")
            .join(format!("{}.checkpoint-000", id.as_str())),
        raw,
    )
    .unwrap();
    let detail = ir_history_detail(&mut store, &id, &|| false).unwrap();
    assert!(
        detail
            .checked
            .as_ref()
            .unwrap()
            .issues
            .iter()
            .any(|issue| matches!(
                issue,
                IrJournalIssue::InvalidLocalFsyncCheckpoint { sequence: 0, .. }
            ))
    );
    let path = output.0.join("checkpoint.zip");
    export_ir_history(&mut store, request(&detail, path.clone()), &|| false).unwrap();
    assert_eq!(entries(&path)["original/checkpoints/000.json"], raw);
}
#[test]
fn existing_destination_library_path_and_cancelled_export_do_not_publish() {
    let root = TempRoot::new("history-no-overwrite");
    let output = TempRoot::new("history-no-overwrite-output");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let id = populate(&mut store, true);
    let detail = ir_history_detail(&mut store, &id, &|| false).unwrap();
    let path = output.0.join("existing.zip");
    fs::write(&path, b"keep me").unwrap();
    assert!(export_ir_history(&mut store, request(&detail, path.clone()), &|| false).is_err());
    assert_eq!(fs::read(path).unwrap(), b"keep me");
    let internal = root.0.join("inside.zip");
    assert!(export_ir_history(&mut store, request(&detail, internal.clone()), &|| false).is_err());
    assert!(!internal.exists());
    let cancelled = output.0.join("cancelled.zip");
    assert_eq!(
        export_ir_history(&mut store, request(&detail, cancelled.clone()), &|| true)
            .unwrap_err()
            .kind,
        ErrorKind::Cancelled
    );
    assert!(!cancelled.exists());
}
#[test]
fn actual_worker_returns_correlated_local_history_replies() {
    let root = TempRoot::new("history-worker");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let id = populate(&mut store, false);
    drop(store);
    let worker = LibraryWorker::open(&root.0, Arc::new(|| {})).unwrap();
    worker
        .try_submit_workflow(WorkflowCommand {
            intent: IntentId(71),
            action: WorkflowAction::ListIrHistory(IrHistoryPageRequest::default()),
        })
        .unwrap();
    worker
        .try_submit_workflow(WorkflowCommand {
            intent: IntentId(72),
            action: WorkflowAction::GetIrHistory(id.clone()),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut received = BTreeMap::new();
    while received.len() < 2 {
        if let Some(event) = worker.try_next_workflow_event() {
            assert!(
                received
                    .insert(event.intent, event.result.unwrap())
                    .is_none()
            );
        }
        assert!(
            Instant::now() < deadline,
            "Local history worker did not settle"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        matches!(&received[&IntentId(71)], WorkflowResult::IrHistoryPage(page) if page.total == 1)
    );
    assert!(
        matches!(&received[&IntentId(72)], WorkflowResult::IrHistoryDetail(detail)
        if detail.attempt == id && detail.checked.as_ref().unwrap().recorded.finish.is_none())
    );
    worker.shutdown();
}
#[test]
fn original_import_whitespace_and_missing_review_gap_survive_the_checked_zip() {
    let root = TempRoot::new("history-original-import");
    let output = TempRoot::new("history-original-import-output");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let f = fixture();
    let id = IrAttemptId::parse("fffffffffffffffffffffffffffffff1").unwrap();
    let binding = IrAttemptBinding::from_prepared(id.clone(), &f.job).unwrap();
    let mut evidence = IrAttemptEvidence::start(binding, 100);
    evidence
        .finish(IrAttemptFinishReason::ObservationComplete)
        .unwrap();
    let original = format!(
        "\n  {}\n",
        serde_json::to_string_pretty(&evidence.to_dto()).unwrap()
    )
    .into_bytes();
    let materials: BTreeMap<_, _> = [
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
    let materials = IrAttemptMaterials::from_retained(materials).unwrap();
    IrAttemptLedger::open(&mut store)
        .unwrap()
        .import_closed_json(&original, &materials)
        .unwrap();
    let detail = ir_history_detail(&mut store, &id, &|| false).unwrap();
    assert!(
        detail
            .checked
            .as_ref()
            .unwrap()
            .issues
            .contains(&IrJournalIssue::MissingReviewPreimage)
    );
    let path = output.0.join("original.zip");
    export_ir_history(&mut store, request(&detail, path.clone()), &|| false).unwrap();
    let exported = entries(&path);
    assert_eq!(exported["original/imported-receipt.json"], original);
    assert!(!exported.contains_key("original/materials/review-preimage.bin"));
    let checks: serde_json::Value =
        serde_json::from_slice(&exported["derived/local-checks.json"]).unwrap();
    assert!(
        checks["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue == "Exact original review bytes were not retained")
    );
}
#[cfg(unix)]
#[test]
fn symlink_and_oversized_checkpoint_never_become_exported_observations() {
    use std::os::unix::fs::symlink;
    for mode in ["symlink", "oversized"] {
        let root = TempRoot::new("history-bounded-checkpoint");
        let output = TempRoot::new("history-bounded-checkpoint-output");
        let mut store = LibraryStore::open(&root.0).unwrap();
        let id = populate(&mut store, true);
        let path = root
            .0
            .join("ir-attempt-ledger")
            .join(format!("{}.checkpoint-000", id.as_str()));
        fs::remove_file(&path).unwrap();
        let outside = output.0.join("outside.json");
        fs::write(&outside, b"keep outside bytes").unwrap();
        if mode == "symlink" {
            symlink(&outside, &path).unwrap();
        } else {
            fs::write(&path, vec![0; 2049]).unwrap();
        }
        assert!(ir_history_detail(&mut store, &id, &|| false).is_err());
        assert_eq!(fs::read(outside).unwrap(), b"keep outside bytes");
    }
}
