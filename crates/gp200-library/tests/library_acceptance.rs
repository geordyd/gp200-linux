use gp200_catalog::Catalog;
use gp200_domain::*;
use gp200_library::*;
use gp200_protocol::{F32Bits, RawRecord, RawTransfer};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
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
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "gp200-library-test-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn child(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn capture() -> Arc<PresetSnapshot> {
    let raw = RawTransfer::from_payload(
        0x12,
        include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
    )
    .unwrap();
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(raw),
        SnapshotSource::Capture {
            capture_id: "gp200-readonly-live".into(),
        },
        Some(PresetTarget::EditBuffer),
        Revision(0),
    ))
}
fn changed(snapshot: &PresetSnapshot, module: u8, parameter: u8, bits: u32) -> Arc<PresetSnapshot> {
    let mut records = snapshot.raw.records().to_vec();
    let index = records
        .iter()
        .position(|record| record.command == 0x14 && record.body[0] == module)
        .unwrap();
    let offset = 8 + usize::from(parameter) * 4;
    records[index].body[offset..offset + 4].copy_from_slice(&bits.to_le_bytes());
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(snapshot.raw.kind(), &records).unwrap()),
        snapshot.source.clone(),
        snapshot.target,
        Revision(snapshot.revision.0 + 1),
    ))
}
fn metadata(name: &str) -> LibraryMetadata {
    LibraryMetadata {
        name: name.into(),
        author: "Geordy".into(),
        style: "Ambient".into(),
        notes: "Live rehearsal".into(),
        tags: vec!["Stage".into(), "Clean".into()],
        favorite: true,
    }
}
fn export(
    store: &LibraryStore,
    path: PathBuf,
    versions: Vec<LibraryVersionId>,
    include_assets: bool,
) -> ConversionReport {
    store
        .export_bundle(&ExportRequest {
            path,
            versions,
            include_assets,
        })
        .unwrap()
}
fn rewrite_zip(source: &Path, destination: &Path, mut modify: impl FnMut(&str, &mut Vec<u8>)) {
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
fn wait_event(worker: &LibraryWorker) -> LibraryEvent {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(event) = worker.try_next_event() {
            return event;
        }
        assert!(Instant::now() < deadline, "worker event deadline");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn captured_raw_unknown_records_and_nan_bits_survive_reopen() {
    let root = TempRoot::new("lossless");
    let mut records = capture().raw.records().to_vec();
    records.push(RawRecord {
        command: 0xbeef,
        body: vec![0, 255, 8, 9, 10],
    });
    records.push(RawRecord {
        command: 0xbeef,
        body: vec![77],
    });
    let snapshot = PresetSnapshot::from_raw(
        Arc::new(RawTransfer::from_records(0x12, &records).unwrap()),
        SnapshotSource::Capture {
            capture_id: "opaque".into(),
        },
        None,
        Revision(5),
    );
    let snapshot = changed(&snapshot, 3, 0, 0x7fc0_00a5);
    let snapshot = changed(&snapshot, 3, 14, 0x8000_0000);
    let mut store = LibraryStore::open(&root.0).unwrap();
    let id = store
        .store_with_originals(
            snapshot.clone(),
            metadata("音色"),
            None,
            &[(
                "original.prst".into(),
                "opaque-vendor-file".into(),
                vec![0, 255, 8, 2, 1],
            )],
        )
        .unwrap();
    drop(store);
    let store = LibraryStore::open(&root.0).unwrap();
    let loaded = store.load_version(&id).unwrap();
    assert_eq!(loaded.snapshot.as_ref(), snapshot.as_ref());
    assert_eq!(loaded.snapshot.raw.records(), snapshot.raw.records());
    assert_eq!(
        store
            .original_bytes(&store.original_sources(&id).unwrap()[0])
            .unwrap(),
        vec![0, 255, 8, 2, 1]
    );
    assert_eq!(loaded.metadata.name, "音色");
}

#[test]
fn writer_lock_is_exclusive_and_released() {
    let root = TempRoot::new("lock");
    let first = LibraryStore::open(&root.0).unwrap();
    assert!(matches!(
        LibraryStore::open(&root.0),
        Err(ServiceError {
            kind: ErrorKind::Busy,
            ..
        })
    ));
    drop(first);
    assert!(LibraryStore::open(&root.0).is_ok());
}

#[test]
fn orphan_blobs_and_unpublished_manifest_do_not_change_committed_library() {
    let root = TempRoot::new("orphan");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let id = store
        .store_snapshot(capture(), metadata("Committed"), None)
        .unwrap();
    let generation = store.generation();
    drop(store);
    // Model a process dying after a blob fsync but before manifest publication.
    fs::write(
        root.child("blobs")
            .join(format!("{:x}", Sha256::digest(b"orphan"))),
        b"orphan",
    )
    .unwrap();
    fs::write(root.child(".publish-crashed.tmp"), b"incomplete manifest").unwrap();
    let store = LibraryStore::open(&root.0).unwrap();
    assert_eq!(store.generation(), generation);
    assert_eq!(store.query(&LibraryQuery::default()).len(), 1);
    assert_eq!(store.load_version(&id).unwrap().metadata.name, "Committed");
}

#[test]
fn corrupted_current_manifest_recovers_previous_commit_and_reports_recovery() {
    let root = TempRoot::new("recover");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let first = store
        .store_snapshot(capture(), metadata("First"), None)
        .unwrap();
    store
        .store_snapshot(
            changed(&capture(), 3, 0, 51.0f32.to_bits()),
            metadata("Second"),
            Some(first.clone()),
        )
        .unwrap();
    drop(store);
    fs::write(root.child("manifest.json"), b"truncated").unwrap();
    let store = LibraryStore::open(&root.0).unwrap();
    assert!(store.recovery_note.is_some());
    assert_eq!(
        store.query(&LibraryQuery::default())[0].latest_version,
        first
    );
    drop(store);
    let store = LibraryStore::open(&root.0).unwrap();
    assert!(store.recovery_note.is_none());
    assert_eq!(store.query(&LibraryQuery::default())[0].version_count, 1);
}

#[test]
fn future_schema_is_rejected_without_silent_rollback() {
    let root = TempRoot::new("future");
    let mut store = LibraryStore::open(&root.0).unwrap();
    store
        .store_snapshot(capture(), metadata("Existing"), None)
        .unwrap();
    drop(store);
    let mut envelope: serde_json::Value =
        serde_json::from_slice(&fs::read(root.child("manifest.json")).unwrap()).unwrap();
    envelope["manifest"]["schema_version"] = 2.into();
    fs::write(
        root.child("manifest.json"),
        serde_json::to_vec(&envelope).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        LibraryStore::open(&root.0),
        Err(ServiceError {
            kind: ErrorKind::Unsupported,
            ..
        })
    ));
}

#[test]
fn stale_revision_save_conflicts_and_trash_preserves_history() {
    let root = TempRoot::new("revision");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let first = store
        .store_snapshot(capture(), metadata("One"), None)
        .unwrap();
    let library = store.load_version(&first).unwrap().library;
    let second = store
        .store_snapshot(capture(), metadata("Two"), Some(first.clone()))
        .unwrap();
    assert!(matches!(
        store.store_snapshot(capture(), metadata("Stale"), Some(first)),
        Err(ServiceError {
            kind: ErrorKind::Conflict,
            ..
        })
    ));
    store.delete_to_trash(&library).unwrap();
    assert!(store.query(&LibraryQuery::default()).is_empty());
    assert_eq!(
        store.query(&LibraryQuery {
            trashed_only: true,
            ..Default::default()
        })[0]
            .id,
        library
    );
    assert!(
        store
            .store_snapshot(capture(), metadata("Hidden"), Some(second))
            .is_err()
    );
    store.restore_trash(&library).unwrap();
    assert_eq!(store.list_versions(&library).unwrap().len(), 2);
}

#[test]
fn search_matches_metadata_tags_and_model_names_without_changing_ids() {
    let root = TempRoot::new("query");
    let mut store = LibraryStore::open(&root.0).unwrap();
    let id = store
        .store_snapshot(capture(), metadata("Clouds"), None)
        .unwrap();
    let library = store.load_version(&id).unwrap().library;
    for text in [
        "clouds",
        "geordy",
        "ambient",
        "rehearsal",
        "stage",
        "Match CL",
    ] {
        let query = LibraryQuery {
            text: text.into(),
            ..Default::default()
        };
        assert_eq!(store.query(&query)[0].id, library);
    }
    assert!(
        store
            .query(&LibraryQuery {
                text: "not-found".into(),
                ..Default::default()
            })
            .is_empty()
    );
    let mut local = metadata("Clouds");
    local.tags.push("Custom".into());
    store.update_metadata(&library, local).unwrap();
    assert_eq!(
        store.query(&LibraryQuery {
            tags: vec!["custom".into()],
            favorites_only: true,
            ..Default::default()
        })[0]
            .version_count,
        2
    );
}

#[test]
fn bundle_moves_complete_history_originals_and_exact_payloads_between_clean_stores() {
    let root = TempRoot::new("bundle");
    let mut source = LibraryStore::open(root.child("source")).unwrap();
    let original = capture();
    let first = source
        .store_with_originals(
            original.clone(),
            metadata("Start"),
            None,
            &[("captured.prst".into(), "opaque".into(), vec![6, 7, 255])],
        )
        .unwrap();
    let edited = changed(&original, 3, 0, 51f32.to_bits());
    let second = source
        .store_snapshot(edited.clone(), metadata("Changed"), Some(first.clone()))
        .unwrap();
    let path = root.child("portable.gp200bundle");
    let report = export(&source, path.clone(), vec![second.clone()], false);
    assert_eq!(report.level, CompatibilityLevel::Lossless);
    assert!(fs::read(&path).unwrap().starts_with(b"PK"));
    let preview = LibraryStore::preview_bundle(&path, BundleLimits::default()).unwrap();
    assert_eq!(
        (
            preview.preset_count,
            preview.version_count,
            preview.scope.as_str()
        ),
        (1, 2, "preset-only")
    );
    let mut destination = LibraryStore::open(root.child("destination")).unwrap();
    let (ids, _) = destination
        .import_bundle(&ImportRequest { path: path.clone() })
        .unwrap();
    assert_eq!(ids.len(), 2);
    assert_eq!(
        destination.load_version(&first).unwrap().snapshot.as_ref(),
        original.as_ref()
    );
    assert_eq!(
        destination.load_version(&second).unwrap().snapshot.as_ref(),
        edited.as_ref()
    );
    assert_eq!(
        destination
            .original_bytes(&destination.original_sources(&second).unwrap()[0])
            .unwrap(),
        vec![6, 7, 255]
    );
    // Identical re-import links existing immutable revisions; it does not overwrite tags.
    destination.import_bundle(&ImportRequest { path }).unwrap();
    assert_eq!(destination.query(&LibraryQuery::default()).len(), 1);
}

#[test]
fn conflicting_bundle_keeps_local_metadata_and_imports_separate_copy() {
    let root = TempRoot::new("conflict");
    let mut source = LibraryStore::open(root.child("source")).unwrap();
    let id = source
        .store_snapshot(capture(), metadata("Imported"), None)
        .unwrap();
    let path = root.child("bundle.gp200bundle");
    export(&source, path.clone(), vec![id.clone()], false);
    let mut destination = LibraryStore::open(root.child("dest")).unwrap();
    destination
        .import_bundle(&ImportRequest { path: path.clone() })
        .unwrap();
    let library = destination.load_version(&id).unwrap().library;
    let mut local = metadata("Local edit");
    local.tags = vec!["Keep me".into()];
    destination.update_metadata(&library, local).unwrap();
    let (_, report) = destination.import_bundle(&ImportRequest { path }).unwrap();
    assert_eq!(destination.query(&LibraryQuery::default()).len(), 2);
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("separate local copies"))
    );
    assert_eq!(
        destination.query(&LibraryQuery {
            text: "Keep me".into(),
            ..Default::default()
        })[0]
            .id,
        library
    );
}

#[test]
fn assets_are_only_satisfied_by_included_content_hash_not_slot_number() {
    let root = TempRoot::new("assets");
    let mut source = LibraryStore::open(root.child("source")).unwrap();
    let asset = source
        .put_asset(
            AssetKind::ImpulseResponse,
            "Cab IR.wav".into(),
            b"real-contained-IR-bytes",
        )
        .unwrap();
    let same = source
        .put_asset(
            AssetKind::ImpulseResponse,
            "Other filename.wav".into(),
            b"real-contained-IR-bytes",
        )
        .unwrap();
    assert_eq!(asset.id, same.id);
    assert_eq!(source.assets().len(), 1);
    let mut preset = capture().as_ref().clone();
    preset.asset_refs = vec![
        AssetRef::UserSlot {
            kind: AssetKind::ImpulseResponse,
            slot: 5,
            content: Some(asset.id.clone()),
        },
        AssetRef::UserSlot {
            kind: AssetKind::SnapTone,
            slot: 5,
            content: None,
        },
    ];
    let id = source
        .store_snapshot(Arc::new(preset), metadata("User IR"), None)
        .unwrap();
    let without = root.child("without.gp200bundle");
    let report = export(&source, without.clone(), vec![id.clone()], false);
    assert_eq!(report.unresolved_assets.len(), 2);
    assert_eq!(
        LibraryStore::preview_bundle(&without, BundleLimits::default())
            .unwrap()
            .included_assets,
        0
    );
    let with = root.child("with.gp200bundle");
    let report = export(&source, with.clone(), vec![id], true);
    assert_eq!(report.unresolved_assets.len(), 1);
    assert_eq!(report.level, CompatibilityLevel::Partial);
    let mut destination = LibraryStore::open(root.child("dest")).unwrap();
    destination
        .import_bundle(&ImportRequest { path: with })
        .unwrap();
    assert_eq!(
        destination.asset_bytes(&asset.id).unwrap(),
        b"real-contained-IR-bytes"
    );
}

#[test]
fn backup_includes_trashed_items_and_all_revisions_with_honest_scope() {
    let root = TempRoot::new("backup");
    let mut source = LibraryStore::open(root.child("source")).unwrap();
    let first = source
        .store_snapshot(capture(), metadata("Trash"), None)
        .unwrap();
    let library = source.load_version(&first).unwrap().library;
    source
        .update_metadata(&library, metadata("Trash newer"))
        .unwrap();
    source.delete_to_trash(&library).unwrap();
    let path = root.child("backup.gp200bundle");
    let report = source
        .backup(&BackupRequest {
            path: path.clone(),
            versions: Vec::new(),
        })
        .unwrap();
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("Device globals"))
    );
    let mut dest = LibraryStore::open(root.child("dest")).unwrap();
    dest.import_bundle(&ImportRequest { path }).unwrap();
    assert!(dest.query(&LibraryQuery::default()).is_empty());
    assert_eq!(
        dest.query_trash(&LibraryQuery::default())[0].version_count,
        2
    );
    dest.restore_trash(&library).unwrap();
    assert_eq!(dest.query(&LibraryQuery::default()).len(), 1);
}

#[test]
fn bundle_hash_mismatch_is_atomic_and_never_imports_partial_items() {
    let root = TempRoot::new("hash");
    let mut source = LibraryStore::open(root.child("source")).unwrap();
    source
        .store_snapshot(capture(), metadata("Source"), None)
        .unwrap();
    let good = root.child("good.gp200bundle");
    export(&source, good.clone(), Vec::new(), false);
    let bad = root.child("bad.gp200bundle");
    rewrite_zip(&good, &bad, |name, bytes| {
        if name.starts_with("blobs/") {
            bytes[0] ^= 1;
        }
    });
    let mut dest = LibraryStore::open(root.child("dest")).unwrap();
    let generation = dest.generation();
    assert!(dest.import_bundle(&ImportRequest { path: bad }).is_err());
    assert_eq!(dest.generation(), generation);
    assert!(dest.query(&LibraryQuery::default()).is_empty());
    assert_eq!(fs::read_dir(dest.root().join("blobs")).unwrap().count(), 0);
}

#[test]
fn traversal_duplicate_paths_and_zip_bombs_are_rejected_before_import() {
    let root = TempRoot::new("zip-security");
    for name in [
        "../escape",
        "/absolute",
        "C:/drive",
        "a\\b",
        "a/./b",
        "a//b",
    ] {
        let path = root.child("unsafe.gp200bundle");
        let mut writer = ZipWriter::new(File::create(&path).unwrap());
        writer
            .start_file(name, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"{}").unwrap();
        writer.finish().unwrap();
        assert!(
            LibraryStore::preview_bundle(&path, BundleLimits::default()).is_err(),
            "{name}"
        );
    }
    let path = root.child("duplicate.gp200bundle");
    let mut writer = ZipWriter::new(File::create(&path).unwrap());
    for name in ["aaaa", "bbbb"] {
        writer
            .start_file(name, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"same").unwrap();
    }
    writer.finish().unwrap();
    let mut bytes = fs::read(&path).unwrap();
    let positions: Vec<_> = bytes
        .windows(4)
        .enumerate()
        .filter(|(_, value)| *value == b"bbbb")
        .map(|(index, _)| index)
        .collect();
    for index in positions {
        bytes[index..index + 4].copy_from_slice(b"aaaa");
    }
    fs::write(&path, bytes).unwrap();
    let error = LibraryStore::preview_bundle(&path, BundleLimits::default()).unwrap_err();
    assert!(error.message.contains("Duplicate ZIP entry"), "{error}");
    let path = root.child("bomb.gp200bundle");
    let mut writer = ZipWriter::new(File::create(&path).unwrap());
    writer
        .start_file(
            "manifest.json",
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )
        .unwrap();
    writer.write_all(&vec![0; 1024 * 1024]).unwrap();
    writer.finish().unwrap();
    assert!(
        LibraryStore::preview_bundle(
            &path,
            BundleLimits {
                entry_bytes: 1024,
                ..Default::default()
            }
        )
        .is_err()
    );
    assert!(
        LibraryStore::preview_bundle(
            &path,
            BundleLimits {
                uncompressed_bytes: 1024,
                ..Default::default()
            }
        )
        .is_err()
    );
    assert!(
        LibraryStore::preview_bundle(
            &path,
            BundleLimits {
                compressed_bytes: 1,
                ..Default::default()
            }
        )
        .is_err()
    );
    assert!(
        LibraryStore::preview_bundle(
            &path,
            BundleLimits {
                entries: 0,
                ..Default::default()
            }
        )
        .is_err()
    );
    assert!(!root.child("escape").exists());
}

#[test]
fn unsupported_extensions_and_dependency_overclaims_fail_explicitly() {
    let root = TempRoot::new("unsupported");
    let mut source = LibraryStore::open(root.child("source")).unwrap();
    source
        .store_snapshot(capture(), metadata("Source"), None)
        .unwrap();
    let good = root.child("good.gp200bundle");
    export(&source, good.clone(), Vec::new(), false);
    let bad = root.child("bad.gp200bundle");
    rewrite_zip(&good, &bad, |name, bytes| {
        if name == "manifest.json" {
            let mut manifest: serde_json::Value = serde_json::from_slice(bytes).unwrap();
            manifest["scope"] = "complete-device-backup".into();
            *bytes = serde_json::to_vec(&manifest).unwrap();
        }
    });
    assert!(LibraryStore::preview_bundle(&bad, BundleLimits::default()).is_err());
    rewrite_zip(&good, &bad, |name, bytes| {
        if name == "manifest.json" {
            let mut manifest: serde_json::Value = serde_json::from_slice(bytes).unwrap();
            manifest["unrecognized_extension"] = "future".into();
            *bytes = serde_json::to_vec(&manifest).unwrap();
        }
    });
    assert_eq!(
        LibraryStore::preview_bundle(&bad, BundleLimits::default())
            .unwrap_err()
            .kind,
        ErrorKind::Unsupported
    );
}

#[test]
fn diff_uses_xml_parameter_id_and_retains_unclassified_raw_bytes() {
    let original = capture();
    let edited = changed(&original, 5, 5, 200f32.to_bits());
    let edited = changed(&edited, 5, 14, 0x7fc0_1111);
    let diff = compare_presets(&original, &edited, &Catalog::embedded().unwrap());
    assert!(diff.known.iter().any(|change| matches!(change, KnownChange::Parameter(ParameterChange { parameter, name, .. }) if parameter.get() == 5 && name.to_lowercase().contains("low cut"))));
    assert!(!diff.known.iter().any(|change| matches!(change, KnownChange::Parameter(ParameterChange { parameter, .. }) if parameter.get() == 14)));
    assert_eq!(diff.raw.len(), 1);
    assert!(
        diff.raw[0]
            .changed_offsets
            .iter()
            .any(|&offset| offset >= 64)
    );
    assert!(diff.raw[0].classification.contains("unclassified"));
    assert_eq!(
        edited
            .view
            .module(ModuleId::new(5).unwrap())
            .unwrap()
            .parameters[14],
        F32Bits(0x7fc0_1111)
    );
}

#[test]
fn worker_publishes_durable_revision_and_preserves_unsent_commands_when_busy() {
    let root = TempRoot::new("worker");
    let worker = LibraryWorker::open(&root.0, no_wake()).unwrap();
    worker
        .try_submit(LibraryCommand::StoreSnapshot {
            intent: IntentId(7),
            snapshot: capture(),
            metadata: metadata("Async"),
            parent: None,
        })
        .unwrap();
    let id = match wait_event(&worker) {
        LibraryEvent::VersionStored {
            intent: IntentId(7),
            version,
        } => version,
        event => panic!("unexpected {event:?}"),
    };
    let mut busy = false;
    for id in 0..10000 {
        let command = LibraryCommand::Query {
            intent: IntentId(id + 100),
            query: LibraryQuery::default(),
        };
        match worker.try_submit(command) {
            Ok(()) => {}
            Err(SubmitError::Busy(command)) => {
                assert!(matches!(command, LibraryCommand::Query { .. }));
                busy = true;
                break;
            }
            Err(error) => panic!("unexpected {error:?}"),
        }
    }
    assert!(
        busy,
        "bounded queues must reject rather than buffer unlimited commands"
    );
    worker.shutdown();
    assert!(matches!(
        worker.try_submit(LibraryCommand::Query {
            intent: IntentId(1),
            query: LibraryQuery::default()
        }),
        Err(SubmitError::Disconnected(_))
    ));
    drop(worker);
    let deadline = Instant::now() + Duration::from_secs(3);
    let reopened = loop {
        match LibraryStore::open(&root.0) {
            Ok(store) => break store,
            Err(error) if error.kind == ErrorKind::Busy && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(error) => panic!("{error}"),
        }
    };
    assert_eq!(reopened.load_version(&id).unwrap().metadata.name, "Async");
}

#[test]
fn cancellation_before_import_leaves_no_visible_orphan_items() {
    let root = TempRoot::new("cancel");
    let mut source = LibraryStore::open(root.child("source")).unwrap();
    source
        .store_snapshot(capture(), metadata("Source"), None)
        .unwrap();
    let path = root.child("good.gp200bundle");
    export(&source, path.clone(), Vec::new(), false);
    let worker = LibraryWorker::open(root.child("dest"), no_wake()).unwrap();
    worker
        .try_submit(LibraryCommand::Cancel(JobId(99)))
        .unwrap();
    worker
        .try_submit(LibraryCommand::ImportBundle {
            intent: IntentId(99),
            request: ImportRequest { path },
        })
        .unwrap();
    loop {
        if let LibraryEvent::Failed {
            intent: IntentId(99),
            error,
        } = wait_event(&worker)
        {
            assert_eq!(error.kind, ErrorKind::Cancelled);
            break;
        }
    }
    worker
        .try_submit(LibraryCommand::Query {
            intent: IntentId(100),
            query: LibraryQuery::default(),
        })
        .unwrap();
    loop {
        if let LibraryEvent::QueryResult {
            intent: IntentId(100),
            entries,
        } = wait_event(&worker)
        {
            assert!(entries.is_empty());
            break;
        }
    }
}

#[test]
fn independent_python_bundle_matches_v1_format_and_preserves_unknown_state() {
    let root = TempRoot::new("independent");
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/library/portable-v1-independent.gp200bundle");
    let preview = LibraryStore::preview_bundle(&path, BundleLimits::default()).unwrap();
    assert_eq!(preview.version_count, 2);
    assert_eq!(preview.missing_assets.len(), 1);
    let mut store = LibraryStore::open(&root.0).unwrap();
    let (_, report) = store.import_bundle(&ImportRequest { path }).unwrap();
    assert_eq!(report.level, CompatibilityLevel::Partial);
    let first = store
        .load_version(&LibraryVersionId::from("revision-synthetic-0"))
        .unwrap();
    let second = store
        .load_version(&LibraryVersionId::from("revision-synthetic-1"))
        .unwrap();
    assert_eq!(second.parent, Some(first.id.clone()));
    assert_eq!(first.snapshot.view.modules[0].state_raw, 2);
    assert_eq!(
        first.snapshot.view.modules[0].parameters[0],
        F32Bits(0x7fc000a5)
    );
    assert_eq!(
        first.snapshot.view.modules[0].parameters[1],
        F32Bits(0x80000000)
    );
    assert_eq!(
        second.snapshot.view.modules[0].parameters[1],
        F32Bits::from_f32(4.5)
    );
    assert_eq!(
        first
            .snapshot
            .raw
            .records()
            .iter()
            .filter(|record| record.command == 0xbeef)
            .count(),
        2
    );
    assert_eq!(
        store
            .original_bytes(&store.original_sources(&first.id).unwrap()[0])
            .unwrap(),
        b"opaque original fixture\x00\xff"
    );
}
