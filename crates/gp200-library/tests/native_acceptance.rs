use gp200_domain::*;
use gp200_library::*;
use gp200_protocol::{RawRecord, RawTransfer};
use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const MODE1: &[u8] = include_bytes!("../../../tests/fixtures/native/constructed-mode1.prst");
const MODE2: &[u8] = include_bytes!("../../../tests/fixtures/native/constructed-mode2.prst");
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "gp200-native-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn recalculate(bytes: &mut [u8]) {
    let end = bytes.len() - 4;
    let sum = bytes[..end].iter().map(|b| u32::from(*b)).sum::<u32>();
    bytes[end..].copy_from_slice(&sum.to_be_bytes());
}
fn changed(file: &NativeFile, bits: u32) -> RawTransfer {
    let mut records = file.raw().records().to_vec();
    let amp = records
        .iter_mut()
        .find(|r| r.command == 0x14 && r.body[0] == 3)
        .unwrap();
    amp.body[8..12].copy_from_slice(&bits.to_le_bytes());
    RawTransfer::from_records(0x12, &records).unwrap()
}
fn snapshot(raw: RawTransfer) -> PresetSnapshot {
    PresetSnapshot::from_raw(
        Arc::new(raw),
        SnapshotSource::Capture {
            capture_id: "native-acceptance".into(),
        },
        None,
        Revision(0),
    )
}
fn external_fixture() -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os("GP200_NATIVE_INTEROP_FIXTURE")
            .expect("Explicit interoperability test requires GP200_NATIVE_INTEROP_FIXTURE"),
    );
    assert!(
        path.is_file(),
        "Configured independent native fixture does not exist"
    );
    path
}

#[test]
fn independent_python_fixture_and_original_export_layout_agree() {
    let file = NativeFile::parse(MODE1).unwrap();
    assert_eq!(file.mode(), 1);
    assert_eq!(file.raw().records().len(), 33);
    assert_eq!(file.raw().payload().len(), 1168);
    assert_eq!(file.original_bytes().len(), 1224);
    assert_eq!(
        file.header()[24..32],
        [0xa5, 0xa5, 0x5a, 0x5a, 1, 0x23, 0x45, 0x67]
    );
    assert_eq!(file.preserve_with_raw(file.raw()).unwrap(), MODE1);
    let rebuilt =
        NativeFile::preset_only(&snapshot(file.raw().as_ref().clone()), Some(file.header()))
            .unwrap();
    assert_eq!(rebuilt, MODE1);
    assert_eq!(file.report().level, CompatibilityLevel::Partial);
}

#[test]
#[ignore = "requires a separately obtained native export via GP200_NATIVE_INTEROP_FIXTURE"]
fn upstream_independent_native_file_parses_and_exports_byte_identically_when_available() {
    let path = external_fixture();
    let bytes = fs::read(path).unwrap();
    let file = NativeFile::parse(&bytes).unwrap();
    assert_eq!(file.mode(), 1);
    assert_eq!(file.raw().payload().len(), 1168);
    let preset = snapshot(file.raw().as_ref().clone());
    assert_eq!(preset.view.display_name(), "American Idiot");
    assert_eq!(file.preserve_with_raw(file.raw()).unwrap(), bytes);
    assert_eq!(
        NativeFile::preset_only(&preset, Some(file.header())).unwrap(),
        bytes
    );
    let edited = file
        .preserve_with_raw(&changed(&file, 51.0f32.to_bits()))
        .unwrap();
    let mut expected = bytes.clone();
    // Independent sample: records start at48; AMP Gain occupies raw340..344.
    expected[388..392].copy_from_slice(&51.0f32.to_bits().to_le_bytes());
    recalculate(&mut expected);
    assert_eq!(edited, expected);
    assert_eq!(file.report().level, CompatibilityLevel::Partial);
}

#[test]
fn bounded_parser_rejects_truncation_bad_lengths_overlap_modes_versions_and_checksum() {
    for end in [0, 1, 31, 40, 48, 100, MODE1.len() - 1] {
        assert!(
            NativeFile::parse(&MODE1[..end]).is_err(),
            "truncation {end}"
        );
    }
    let mut corrupt = MODE1.to_vec();
    corrupt[80] ^= 1;
    assert!(
        NativeFile::parse(&corrupt)
            .unwrap_err()
            .message
            .contains("checksum")
    );
    for (offset, value) in [(32, u32::MAX), (36, u32::MAX), (44, 1), (1216, 1)] {
        let mut bad = MODE1.to_vec();
        bad[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        recalculate(&mut bad);
        assert!(NativeFile::parse(&bad).is_err(), "length field {offset}");
    }
    let mut overlap = MODE2.to_vec();
    overlap[40..44].copy_from_slice(&48u32.to_le_bytes());
    recalculate(&mut overlap);
    assert!(NativeFile::parse(&overlap).is_err());
    for (offset, value) in [(20, 0x0200), (22, 0), (22, 3)] {
        let mut bad = MODE1.to_vec();
        bad[offset..offset + 2].copy_from_slice(&u16::to_le_bytes(value));
        recalculate(&mut bad);
        assert_eq!(
            NativeFile::parse(&bad).unwrap_err().kind,
            ErrorKind::Unsupported
        );
    }
    let limits = NativeFileLimits {
        maximum_file_bytes: 1200,
        ..NativeFileLimits::default()
    };
    assert!(NativeFile::parse_bounded(MODE1, limits).is_err());
    let limits = NativeFileLimits {
        maximum_records: 32,
        ..NativeFileLimits::default()
    };
    assert!(NativeFile::parse_bounded(MODE1, limits).is_err());
}

#[test]
fn module_count_duplicate_ids_bad_slot_counts_and_chain_permutations_are_rejected() {
    let file = NativeFile::parse(MODE1).unwrap();
    for mutation in 0..4 {
        let mut raw = file.raw().records().to_vec();
        match mutation {
            0 => {
                raw.retain(|r| !(r.command == 0x14 && r.body[0] == 10));
            }
            1 => {
                raw.iter_mut()
                    .find(|r| r.command == 0x14 && r.body[0] == 10)
                    .unwrap()
                    .body[0] = 9;
            }
            2 => {
                raw.iter_mut().find(|r| r.command == 0x14).unwrap().body[2] = 14;
            }
            _ => {
                raw.iter_mut().find(|r| r.command == 8).unwrap().body[4] = 10;
            }
        }
        assert!(
            NativeFile::preset_only(
                &snapshot(RawTransfer::from_records(0x12, &raw).unwrap()),
                None
            )
            .is_err()
        );
    }
}

#[test]
fn mode2_opaque_ir_and_unknown_header_survive_parameter_edit_and_u32_checksum() {
    let file = NativeFile::parse(MODE2).unwrap();
    let ir = file.embedded_ir_bytes().unwrap();
    assert_eq!(ir.len(), 1792);
    let sum = u32::from_be_bytes(MODE2[MODE2.len() - 4..].try_into().unwrap());
    assert!(
        sum > u32::from(u16::MAX),
        "independent fixture distinguishes BE32 from BE16"
    );
    let edited = file
        .preserve_with_raw(&changed(&file, 0x7fc0_00a5))
        .unwrap();
    let after = NativeFile::parse(&edited).unwrap();
    assert_eq!(after.embedded_ir_bytes(), Some(ir));
    assert_eq!(after.header(), file.header());
    assert_eq!(
        after.raw().records()[5].body[8..12],
        0x7fc0_00a5u32.to_le_bytes()
    );
    let differs: Vec<_> = MODE2
        .iter()
        .zip(&edited)
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(i, _)| i)
        .collect();
    // Python fixture: raw stream begins at1856; AMP Gain is raw byte340.
    assert!(
        differs
            .iter()
            .all(|i| (2196..2200).contains(i) || *i >= edited.len() - 4)
    );
    assert!(
        after
            .report()
            .notes
            .iter()
            .any(|n| n.contains("opaque embedded IR"))
    );
}

#[test]
fn unknown_records_raw_float_bits_and_unicode_name_bytes_are_retained() {
    let file = NativeFile::parse(MODE1).unwrap();
    let mut records = changed(&file, 0x8000_0000).records().to_vec();
    records.push(RawRecord {
        command: 0xbeef,
        body: vec![0, 255, 4, 0, 7],
    });
    let metadata = records.iter_mut().find(|r| r.command == 2).unwrap();
    metadata.body[16..32].fill(0);
    metadata.body[16..22].copy_from_slice("音色".as_bytes());
    let raw = RawTransfer::from_records(0x12, &records).unwrap();
    assert!(
        file.preserve_with_raw(&raw).is_err(),
        "same-size preservation cannot silently drop extension"
    );
    let bytes = NativeFile::preset_only(&snapshot(raw.clone()), None).unwrap();
    let imported = NativeFile::parse(&bytes).unwrap();
    assert_eq!(imported.raw().as_ref(), &raw);
    assert!(
        imported
            .report()
            .notes
            .iter()
            .any(|n| n.contains("character encoding"))
    );
}

#[test]
fn serializer_crc_is_standard_crc32_and_distinct_from_file_sum_and_size_word() {
    assert_eq!(native_blob_crc32(b"123456789"), 0xcbf4_3926);
    let file = NativeFile::parse(MODE1).unwrap();
    let size = u32::from_le_bytes(MODE1[1216..1220].try_into().unwrap());
    assert_eq!(size, 1216);
    assert_ne!(native_blob_crc32(file.raw().payload()), size);
}

#[test]
fn native_exports_require_patch_assignment_projection_to_match_encoded_bytes() {
    let file = NativeFile::parse(MODE1).unwrap();
    let original = snapshot(file.raw().as_ref().clone());
    let mut conflicting = original.clone();
    conflicting.view.patch_assignments.quicks[0]
        .record
        .module_raw = 255;
    let error = NativeFile::preset_only(&conflicting, None).unwrap_err();
    assert!(error.message.contains("encoded patch assignments"));
    let mut legacy = original.clone();
    legacy.view.patch_assignments = PatchAssignments::default();
    let exported = NativeFile::preset_only(&legacy, None).unwrap();
    assert_eq!(
        NativeFile::parse(&exported).unwrap().raw().as_ref(),
        original.raw.as_ref()
    );

    let mut draft = PresetDraft::from_snapshot(&original);
    draft
        .apply(&EditPatch::SetPatchAssignment(
            AssignmentEdit::CtrlModules {
                slot: 7,
                modules: vec![ModuleId::new(10).unwrap()],
            },
        ))
        .unwrap();
    let changed = draft
        .snapshot(&original, SnapshotSource::Simulator, Revision(1))
        .unwrap();
    let bytes = NativeFile::preset_only(&changed, None).unwrap();
    let imported = snapshot(NativeFile::parse(&bytes).unwrap().raw().as_ref().clone());
    assert_eq!(
        imported.view.patch_assignments.ctrls[7]
            .record
            .module_mask_raw
            & 0x7ff,
        1 << 10
    );
    for (before, after) in original.raw.records().iter().zip(imported.raw.records()) {
        if before.command == 15 && before.body[0] == 7 {
            assert_eq!(&before.body[..4], &after.body[..4]);
        } else {
            assert_eq!(before, after);
        }
    }
}

#[test]
fn import_reopen_edit_export_keeps_source_bytes_and_never_loses_failed_input() {
    let temp = Temp::new();
    let input = temp.path("source.prst");
    fs::write(&input, MODE1).unwrap();
    let root = temp.path("library");
    let mut store = LibraryStore::open(&root).unwrap();
    let (first, report) = store.import_native(&ImportRequest { path: input }).unwrap();
    assert_eq!(report.level, CompatibilityLevel::Partial);
    let original = store.original_sources(&first).unwrap()[0].clone();
    let loaded = store.load_version(&first).unwrap();
    let file = NativeFile::parse(MODE1).unwrap();
    let mut edited = snapshot(changed(&file, 51.0f32.to_bits()));
    edited.source = loaded.snapshot.source.clone();
    let second = store
        .store_snapshot(Arc::new(edited), loaded.metadata, Some(first.clone()))
        .unwrap();
    let mut invalid = MODE1.to_vec();
    invalid[22..24].copy_from_slice(&99u16.to_le_bytes());
    recalculate(&mut invalid);
    let bad = temp.path("unsupported.prst");
    fs::write(&bad, &invalid).unwrap();
    assert_eq!(
        store
            .import_native(&ImportRequest { path: bad })
            .unwrap_err()
            .kind,
        ErrorKind::Unsupported
    );
    assert_eq!(store.query(&LibraryQuery::default()).len(), 1);
    drop(store);
    let store = LibraryStore::open(&root).unwrap();
    assert_eq!(store.original_bytes(&original).unwrap(), MODE1);
    let rejected = store.rejected_native_sources().unwrap();
    assert_eq!(rejected.len(), 1);
    assert_eq!(store.original_bytes(&rejected[0].source).unwrap(), invalid);
    let output = temp.path("edited.prst");
    store
        .export_native(&NativeExportRequest {
            path: output.clone(),
            version: second,
            mode: NativeExportMode::PreserveOriginal,
        })
        .unwrap();
    let exported = NativeFile::parse(&fs::read(output).unwrap()).unwrap();
    assert_eq!(exported.header(), file.header());
    assert_eq!(
        exported.raw().records()[5].body[8..12],
        51.0f32.to_bits().to_le_bytes()
    );
    let source_output = temp.path("unchanged.prst");
    store
        .export_native(&NativeExportRequest {
            path: source_output.clone(),
            version: first,
            mode: NativeExportMode::PreserveOriginal,
        })
        .unwrap();
    assert_eq!(fs::read(source_output).unwrap(), MODE1);
}

#[test]
fn preset_only_export_removes_transport_context_and_reports_missing_assets() {
    let temp = Temp::new();
    let mut store = LibraryStore::open(temp.path("library")).unwrap();
    let raw = RawTransfer::from_payload(
        0x12,
        include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
    )
    .unwrap();
    let mut raw = raw.records().to_vec();
    raw.iter_mut()
        .find(|r| r.command == 0x14 && r.body[0] == 5)
        .unwrap()
        .body[4..8]
        .copy_from_slice(&0x0a10_0000u32.to_le_bytes());
    let snap = snapshot(RawTransfer::from_records(0x12, &raw).unwrap());
    let id = store
        .store_snapshot(Arc::new(snap), LibraryMetadata::default(), None)
        .unwrap();
    let path = temp.path("preset-only.prst");
    let report = store
        .export_native(&NativeExportRequest {
            path: path.clone(),
            version: id.clone(),
            mode: NativeExportMode::PresetOnly,
        })
        .unwrap();
    assert_eq!(report.unresolved_assets.len(), 1);
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("opaque header fields were initialized"))
    );
    assert_eq!(
        NativeFile::parse(&fs::read(path).unwrap())
            .unwrap()
            .raw()
            .records()
            .len(),
        33
    );
    assert!(
        store
            .export_native(&NativeExportRequest {
                path: temp.path("original.prst"),
                version: id,
                mode: NativeExportMode::PreserveOriginal
            })
            .is_err()
    );
}

#[test]
fn native_commands_run_through_bounded_worker() {
    let temp = Temp::new();
    let source = temp.path("worker.prst");
    fs::write(&source, MODE1).unwrap();
    let worker = LibraryWorker::open(temp.path("library"), Arc::new(|| {})).unwrap();
    worker
        .try_submit(LibraryCommand::ImportNative {
            intent: IntentId(100),
            request: ImportRequest { path: source },
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let version = loop {
        if let Some(event) = worker.try_next_event() {
            match event {
                LibraryEvent::Imported {
                    intent, versions, ..
                } => {
                    assert_eq!(intent, IntentId(100));
                    break versions[0].clone();
                }
                LibraryEvent::Failed { error, .. } => panic!("{error}"),
                _ => {}
            }
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    };
    let path = temp.path("worker-out.prst");
    worker
        .try_submit(LibraryCommand::ExportNative {
            intent: IntentId(101),
            request: NativeExportRequest {
                path: path.clone(),
                version,
                mode: NativeExportMode::PreserveOriginal,
            },
        })
        .unwrap();
    loop {
        if let Some(event) = worker.try_next_event() {
            match event {
                LibraryEvent::Exported { intent, .. } => {
                    assert_eq!(intent, IntentId(101));
                    break;
                }
                LibraryEvent::Failed { error, .. } => panic!("{error}"),
                _ => {}
            }
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(fs::read(path).unwrap(), MODE1);
    worker.shutdown();
}

#[test]
fn local_projection_edits_cannot_silently_export_unchanged_native_settings() {
    let file = NativeFile::parse(MODE1).unwrap();
    let mut snap = snapshot(file.raw().as_ref().clone());
    let mut draft = PresetDraft::from_snapshot(&snap);
    draft
        .apply(&EditPatch::SetExpression(ExpressionPatch {
            controller: 0,
            bank: 0,
            targets: vec![AssignmentTarget {
                module: ModuleId::new(3).unwrap(),
                model: snap.view.module(ModuleId::new(3).unwrap()).unwrap().model,
                parameter: ParamId::new(0).unwrap(),
                minimum: 0.0f32.into(),
                maximum: 100.0f32.into(),
            }],
        }))
        .unwrap();
    snap = draft
        .snapshot(&snap, SnapshotSource::Simulator, Revision(1))
        .unwrap();
    let error = NativeFile::preset_only(&snap, None).unwrap_err();
    assert!(error.message.contains("expression assignments"));
    assert!(error.message.contains(".gp200bundle"));
    let temp = Temp::new();
    let mut store = LibraryStore::open(temp.path("library")).unwrap();
    let id = store
        .store_with_originals(
            Arc::new(snap),
            LibraryMetadata::default(),
            None,
            &[(
                "source.prst".into(),
                "valeton.gp200.prst.static-1.8.1".into(),
                MODE1.to_vec(),
            )],
        )
        .unwrap();
    let output = temp.path("must-not-exist.prst");
    assert!(
        store
            .export_native(&NativeExportRequest {
                path: output.clone(),
                version: id,
                mode: NativeExportMode::PreserveOriginal
            })
            .is_err()
    );
    assert!(!output.exists());
    for field in [
        "name",
        "tempo",
        "level",
        "chain",
        "pan",
        "quick",
        "footswitch",
    ] {
        let mut snap = snapshot(file.raw().as_ref().clone());
        match field {
            "name" => {
                snap.view.name = Recovered::Candidate {
                    value: "Unsaved name".into(),
                    evidence: vec![],
                }
            }
            "tempo" => {
                snap.view.tempo = Recovered::Candidate {
                    value: 250,
                    evidence: vec![],
                }
            }
            "level" => {
                snap.view.level = Recovered::Candidate {
                    value: 0,
                    evidence: vec![],
                }
            }
            "chain" => {
                let mut chain = snap.view.chain.value().unwrap().clone();
                chain.swap(0, 1);
                snap.view.chain = Recovered::Candidate {
                    value: chain,
                    evidence: vec![],
                };
            }
            "pan" => snap.view.metadata.pan = Some(0.5f32.into()),
            "quick" => snap.view.quick_controls.push(QuickControlPatch {
                index: 0,
                target: None,
            }),
            "footswitch" => snap.view.footswitches.push(FootswitchPatch {
                switch: 0,
                tap_action: "Bypass".into(),
                hold_action: "Tuner".into(),
                modules: vec![],
            }),
            _ => unreachable!(),
        }
        assert!(
            NativeFile::preset_only(&snap, None).is_err(),
            "unencoded {field} must fail"
        );
    }
}

#[test]
fn local_library_metadata_omissions_are_explicit_and_preserved_in_store() {
    let temp = Temp::new();
    let mut store = LibraryStore::open(temp.path("library")).unwrap();
    let file = NativeFile::parse(MODE1).unwrap();
    let mut snap = snapshot(file.raw().as_ref().clone());
    snap.view.metadata.author = Some("Local author".into());
    snap.view.metadata.style = Some("Local style".into());
    snap.view.metadata.notes = Some("Local notes".into());
    let metadata = LibraryMetadata {
        favorite: true,
        tags: vec!["Local tag".into()],
        notes: "Library notes".into(),
        ..LibraryMetadata::default()
    };
    let id = store
        .store_snapshot(Arc::new(snap), metadata.clone(), None)
        .unwrap();
    let output = temp.path("local-metadata.prst");
    let report = store
        .export_native(&NativeExportRequest {
            path: output,
            version: id.clone(),
            mode: NativeExportMode::PresetOnly,
        })
        .unwrap();
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("tags, notes, favorites, author and style remain in .gp200bundle"))
    );
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("omitted from native bytes: author, style, notes"))
    );
    assert_eq!(store.load_version(&id).unwrap().metadata, metadata);
}
