//! Internal IR export fault regression tests.
use super::*;
#[path = "../tests/common/ir_history_fixture.rs"]
mod fixture;
use fixture::*;

fn request(detail: &IrHistoryDetail, destination: PathBuf) -> IrHistoryExportRequest {
    IrHistoryExportRequest {
        attempt: detail.attempt.clone(),
        observed_snapshot_sha256: detail.snapshot_sha256.clone(),
        kind: detail.export_kind,
        destination,
    }
}
#[test]
fn changed_checkpoint_between_zip_write_and_final_reload_prevents_publication() {
    let root = TempRoot::new("history-late-checkpoint");
    let output = TempRoot::new("history-late-checkpoint-output");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let id = populate(&mut store, true);
    let detail = ir_history_detail(&mut store, &id, &|| false).unwrap();
    let checkpoint = root
        .0
        .join("ir-attempt-ledger")
        .join(format!("{}.checkpoint-000", id.as_str()));
    let path = output.0.join("changed.zip");
    let result = export_with_before_publication(
        &mut store,
        request(&detail, path.clone()),
        &|| false,
        &|| fs::write(&checkpoint, b"changed after writing ZIP").unwrap(),
    );
    assert!(result.is_err());
    assert!(!path.exists());
    assert_eq!(fs::read(checkpoint).unwrap(), b"changed after writing ZIP");
}
#[test]
fn changed_journal_and_missing_material_between_zip_write_and_reload_fail_closed() {
    for change in ["journal", "missing-material"] {
        let root = TempRoot::new(change);
        let output = TempRoot::new("history-late-byte-output");
        let mut store = LibraryStore::open(&root.0).unwrap();
        let id = populate(&mut store, true);
        let detail = ir_history_detail(&mut store, &id, &|| false).unwrap();
        let material = root.0.join("blobs").join(
            detail.checked.as_ref().unwrap().materials[0]
                .sha256
                .as_str(),
        );
        let path = output.0.join("changed.zip");
        let result = export_with_before_publication(
            &mut store,
            request(&detail, path.clone()),
            &|| false,
            &|| {
                if change == "journal" {
                    let mut bytes = fs::read(root.journal(&id)).unwrap();
                    bytes.extend_from_slice(b"late original bytes");
                    fs::write(root.journal(&id), bytes).unwrap();
                } else {
                    fs::remove_file(&material).unwrap();
                }
            },
        );
        assert!(result.is_err());
        assert!(!path.exists());
        assert_eq!(fs::read_dir(&output.0).unwrap().count(), 0);
    }
}
#[test]
fn destination_created_after_validation_is_never_replaced() {
    let root = TempRoot::new("history-destination-race");
    let output = TempRoot::new("history-destination-race-output");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let id = populate(&mut store, true);
    let detail = ir_history_detail(&mut store, &id, &|| false).unwrap();
    let path = output.0.join("raced.zip");
    let result = export_with_before_publication(
        &mut store,
        request(&detail, path.clone()),
        &|| false,
        &|| fs::write(&path, b"keep raced file").unwrap(),
    );
    assert!(result.is_err());
    assert_eq!(fs::read(path).unwrap(), b"keep raced file");
}
#[test]
fn post_publication_sync_failure_keeps_visible_exact_archive_and_reports_indeterminate() {
    let root = TempRoot::new("history-post-publication");
    let output = TempRoot::new("history-post-publication-output");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let id = populate(&mut store, true);
    let original = fs::read(root.journal(&id)).unwrap();
    let detail = ir_history_detail(&mut store, &id, &|| false).unwrap();
    let path = output.0.join("visible.zip");
    let result = export_with_callbacks(
        &mut store,
        request(&detail, path.clone()),
        &|| false,
        &|| {},
        &|_| Err(std::io::Error::other("injected directory fsync failure")),
    );
    assert_eq!(result.unwrap_err().kind, ErrorKind::Indeterminate);
    assert!(path.is_file());
    let mut zip = ZipArchive::new(File::open(path).unwrap()).unwrap();
    let mut raw = vec![];
    zip.by_name("original/journal.bin")
        .unwrap()
        .read_to_end(&mut raw)
        .unwrap();
    assert_eq!(raw, original);
    assert_eq!(fs::read(root.journal(&id)).unwrap(), original);
}
#[test]
fn cancellation_after_writing_zip_prevents_publication_and_leaves_history_unchanged() {
    use std::cell::Cell;
    let root = TempRoot::new("history-late-cancel");
    let output = TempRoot::new("history-late-cancel-output");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let id = populate(&mut store, true);
    let original = fs::read(root.journal(&id)).unwrap();
    let detail = ir_history_detail(&mut store, &id, &|| false).unwrap();
    let path = output.0.join("cancelled.zip");
    let cancelled = Cell::new(false);
    let result = export_with_before_publication(
        &mut store,
        request(&detail, path.clone()),
        &|| cancelled.get(),
        &|| cancelled.set(true),
    );
    assert_eq!(result.unwrap_err().kind, ErrorKind::Cancelled);
    assert!(!path.exists());
    assert_eq!(fs::read(root.journal(&id)).unwrap(), original);
}
