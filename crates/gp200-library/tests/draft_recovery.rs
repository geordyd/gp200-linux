use gp200_domain::*;
use gp200_library::*;
use std::{
    fs,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

fn temp(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "gp200-recovery-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    root
}
fn session() -> RecoverySession {
    let raw = RawTransfer::from_payload(
        0x12,
        include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
    )
    .unwrap();
    let baseline = Arc::new(PresetSnapshot::from_raw(
        Arc::new(raw),
        SnapshotSource::Hardware {
            epoch: SessionEpoch(999),
        },
        Some(PresetTarget::EditBuffer),
        Revision(4),
    ));
    let mut draft = PresetDraft::from_snapshot(&baseline);
    draft.view.modules[0].parameters[0] = F32Bits(0x7fc0_0123); // exact NaN payload
    draft.view.modules[1].parameters[3] = F32Bits(0x42c6_b5c9); // real off-grid Position
    draft.metadata.author = Some("Local author, retained beyond native width".into());
    draft.metadata.notes = Some("Unsent local notes\nwith a second line".into());
    let before = PresetDraft::from_snapshot(&baseline);
    let document = RecoveryDocument {
        original_id: DocumentId::from("device-buffer"),
        original_target: EditTarget::DeviceBuffer,
        baseline,
        draft,
        local_revision: Revision(88),
        undo: RecoveryUndo {
            past: vec![before.clone()],
            future: vec![before.clone()],
            gesture: Some((
                GestureKey {
                    module: ModuleId::new(1).unwrap(),
                    model: ModelCode(0x0500000a),
                    parameter: ParamId::new(3).unwrap(),
                },
                before.clone(),
            )),
        },
        comparison: RecoveryComparison {
            a: Some(before.clone()),
            b: Some(before),
            last_recalled: Some('B'),
        },
        uncertainty: vec![RecoveryUncertainty {
            original_intent: 456,
            revision: Revision(87),
            description: "Sent; unknown result".into(),
            patch: None,
            actual: None,
        }],
    };
    let mut second = document.clone();
    second.original_id = DocumentId::from("local-library");
    second.original_target = EditTarget::Offline(second.original_id.clone());
    second.draft.metadata.notes = Some("Second document".into());
    RecoverySession {
        schema_version: 1,
        app_version: "0.1.0".into(),
        active_document: Some(second.original_id.clone()),
        session_uncertainty: vec!["Save intent 123 at epoch 999 was not verified".into()],
        documents: vec![document, second],
    }
}
#[test]
fn exact_multi_document_raw_float_history_assignment_and_ab_roundtrip() {
    let root = temp("exact");
    let original = session();
    let receipt;
    {
        let store = DraftRecoveryStore::open(root.join("journal")).unwrap();
        receipt = store.save(&original).unwrap();
    }
    let store = DraftRecoveryStore::open(root.join("journal")).unwrap();
    let found = store.discover().unwrap().unwrap();
    assert_eq!(found.sha256, receipt.sha256);
    assert_eq!(*found.session, original);
    assert_eq!(
        found.session.documents[0].draft.view.modules[0].parameters[0].0,
        0x7fc00123
    );
    assert_eq!(
        found.session.documents[0].baseline.raw.payload(),
        original.documents[0].baseline.raw.payload()
    );
    drop(store);
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn abandoned_partial_stage_leaves_last_complete_session_available() {
    let root = temp("partial");
    let journal = root.join("journal");
    let receipt = {
        let store = DraftRecoveryStore::open(&journal).unwrap();
        store.save(&session()).unwrap()
    };
    let partial = journal.join(".session-crashed.tmp");
    fs::write(&partial, b"GP2REC01 partial").unwrap();
    let store = DraftRecoveryStore::open(&journal).unwrap();
    assert_eq!(store.discover().unwrap().unwrap().sha256, receipt.sha256);
    assert_eq!(fs::read(&partial).unwrap(), b"GP2REC01 partial");
    drop(store);
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn tampered_or_short_journal_is_retained_and_rejected() {
    let root = temp("tamper");
    let store = DraftRecoveryStore::open(root.join("journal")).unwrap();
    store.save(&session()).unwrap();
    let mut bytes = fs::read(store.path()).unwrap();
    *bytes.last_mut().unwrap() ^= 1;
    fs::write(store.path(), &bytes).unwrap();
    assert!(store.discover().unwrap_err().message.contains("integrity"));
    assert_eq!(fs::read(store.path()).unwrap(), bytes);
    fs::write(store.path(), b"GP2REC01").unwrap();
    assert!(store.discover().is_err());
    assert_eq!(fs::read(store.path()).unwrap(), b"GP2REC01");
    drop(store);
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn future_schema_is_not_silently_rewritten() {
    let root = temp("future");
    let store = DraftRecoveryStore::open(root.join("journal")).unwrap();
    store.save(&session()).unwrap();
    let mut bytes = fs::read(store.path()).unwrap();
    bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
    fs::write(store.path(), &bytes).unwrap();
    assert_eq!(store.discover().unwrap_err().kind, ErrorKind::Unsupported);
    assert_eq!(fs::read(store.path()).unwrap(), bytes);
    let mut future = session();
    future.schema_version = 2;
    assert!(store.save(&future).is_err());
    assert_eq!(fs::read(store.path()).unwrap(), bytes);
    drop(store);
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn stale_review_cannot_discard_newer_work() {
    let root = temp("discard");
    let store = DraftRecoveryStore::open(root.join("journal")).unwrap();
    let a = store.save(&session()).unwrap();
    let mut changed = session();
    changed.documents[1].draft.metadata.notes = Some("Later work".into());
    let b = store.save(&changed).unwrap();
    assert_ne!(a.sha256, b.sha256);
    assert_eq!(
        store.discard(&a.sha256).unwrap_err().kind,
        ErrorKind::Conflict
    );
    assert_eq!(*store.discover().unwrap().unwrap().session, changed);
    store.discard(&b.sha256).unwrap();
    assert!(store.discover().unwrap().is_none());
    drop(store);
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn history_and_document_limits_preserve_previous_journal_without_truncation() {
    let root = temp("bounds");
    let store = DraftRecoveryStore::open(root.join("journal")).unwrap();
    let saved = store.save(&session()).unwrap();
    let mut too_many = session();
    too_many.documents[0].undo.past =
        vec![too_many.documents[0].draft.clone(); MAX_RECOVERY_HISTORY + 1];
    assert!(store.save(&too_many).is_err());
    assert_eq!(store.discover().unwrap().unwrap().sha256, saved.sha256);
    let mut duplicate = session();
    duplicate.documents[1].original_id = duplicate.documents[0].original_id.clone();
    assert!(store.save(&duplicate).is_err());
    let mut too_big = session();
    too_big.documents[0].draft.metadata.notes = Some("x".repeat(4 * 1024 * 1024));
    assert!(store.save(&too_big).is_err());
    assert_eq!(store.discover().unwrap().unwrap().sha256, saved.sha256);
    drop(store);
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn writer_is_exclusive_and_releases_on_drop() {
    let root = temp("lock");
    let store = DraftRecoveryStore::open(root.join("journal")).unwrap();
    assert!(matches!(
        DraftRecoveryStore::open(root.join("journal")),
        Err(ServiceError {
            kind: ErrorKind::Busy,
            ..
        })
    ));
    drop(store);
    let second = DraftRecoveryStore::open(root.join("journal")).unwrap();
    drop(second);
    fs::remove_dir_all(root).unwrap();
}
#[cfg(unix)]
#[test]
fn private_permissions_and_symlinks_are_enforced_without_touching_victim() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = temp("links");
    let victim = root.join("victim");
    fs::write(&victim, b"leave unchanged").unwrap();
    symlink(&root, root.join("redirect")).unwrap();
    assert!(DraftRecoveryStore::open(root.join("redirect/journal")).is_err());
    let journal = root.join("journal");
    let store = DraftRecoveryStore::open(&journal).unwrap();
    store.save(&session()).unwrap();
    assert_eq!(
        fs::metadata(&journal).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(store.path()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    fs::remove_file(store.path()).unwrap();
    symlink(&victim, store.path()).unwrap();
    assert!(store.discover().is_err());
    assert!(store.save(&session()).is_err());
    assert_eq!(fs::read(&victim).unwrap(), b"leave unchanged");
    drop(store);
    fs::remove_file(journal.join("writer.lock")).unwrap();
    symlink(&victim, journal.join("writer.lock")).unwrap();
    assert!(DraftRecoveryStore::open(&journal).is_err());
    assert_eq!(fs::read(&victim).unwrap(), b"leave unchanged");
    fs::remove_dir_all(root).unwrap();
}
fn wait(worker: &DraftRecoveryWorker) -> DraftRecoveryEvent {
    let start = Instant::now();
    loop {
        if let Some(event) = worker.try_next_event() {
            return event;
        }
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn worker_discovers_and_flushes_final_snapshot_then_reopens_after_restart() {
    let root = temp("worker");
    let worker = DraftRecoveryWorker::spawn(root.join("journal"), Arc::new(|| {})).unwrap();
    assert!(matches!(
        wait(&worker),
        DraftRecoveryEvent::Discovered(None)
    ));
    worker
        .try_submit(DraftRecoveryCommand::Save(Arc::new(session())))
        .unwrap();
    assert!(matches!(wait(&worker), DraftRecoveryEvent::Saved(_)));
    let mut final_session = session();
    final_session.documents[0].draft.metadata.notes = Some("Final frame".into());
    worker
        .flush_on_exit(Arc::new(final_session.clone()))
        .unwrap();
    drop(worker);
    let start = Instant::now();
    let second = loop {
        match DraftRecoveryStore::open(root.join("journal")) {
            Ok(store) => break store,
            Err(e) if e.kind == ErrorKind::Busy && start.elapsed() < Duration::from_secs(2) => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(e) => panic!("{e}"),
        }
    };
    assert_eq!(*second.discover().unwrap().unwrap().session, final_session);
    drop(second);
    fs::remove_dir_all(root).unwrap();
}
