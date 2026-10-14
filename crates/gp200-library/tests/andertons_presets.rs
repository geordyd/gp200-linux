//! Real public inputs are optional and private; retained evidence contains hashes only.
#![cfg(gp200_private_evidence)]
use gp200_domain::*;
use gp200_library::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, io::Write, path::PathBuf, sync::Arc};

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "gp200-andertons-native-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
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

// Independent byte walk: do not ask RawTransfer for the expected edit position.
fn amp_first_parameter(bytes: &[u8]) -> usize {
    let chunk = u32::from_le_bytes(bytes[32..36].try_into().unwrap()) as usize;
    assert_eq!(&bytes[chunk..chunk + 4], b"MRAP");
    let mut offset = chunk + 8;
    while offset < bytes.len() - 8 {
        let command = u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap());
        let length = u16::from_le_bytes(bytes[offset + 2..offset + 4].try_into().unwrap()) as usize;
        if command == 0x14 && bytes[offset + 4] == 3 {
            assert_eq!(length, 68);
            return offset + 4 + 8;
        }
        offset += 4 + length;
    }
    panic!("AMP record not found");
}

#[test]
fn six_public_presets_import_reopen_export_exactly_and_edit_only_amp_bits_and_checksum() {
    let profiles: Value = serde_json::from_slice(include_bytes!(
        "../../../tests/fixtures/assets/andertons-profiles.json"
    ))
    .unwrap();
    let configured = std::env::var_os("GP200_ANDERTONS_CORPUS");
    let corpus = configured
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."));
    assert!(
        profiles["presets"]
            .as_array()
            .unwrap()
            .iter()
            .all(|profile| corpus.join(profile["path"].as_str().unwrap()).is_file()),
        "Explicit private evidence test requires the complete Andertons corpus"
    );
    assert_eq!(profiles["presets"].as_array().unwrap().len(), 6);
    let mut cases = Vec::new();
    for profile in profiles["presets"].as_array().unwrap() {
        let path = corpus.join(profile["path"].as_str().unwrap());
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 1224);
        assert_eq!(digest(&bytes), profile["sha256"].as_str().unwrap());
        let file = NativeFile::parse(&bytes).unwrap();
        assert_eq!(file.mode(), 1);
        assert_eq!(file.raw().records().len(), 33);
        assert_eq!(file.raw().payload().len(), 1168);
        assert_eq!(file.report().level, CompatibilityLevel::Partial);
        assert_eq!(file.preserve_with_raw(file.raw()).unwrap(), bytes);
        let temp = Temp::new();
        let library = temp.path("library");
        let mut store = LibraryStore::open(&library).unwrap();
        let (original, import_report) = store
            .import_native(&ImportRequest { path: path.clone() })
            .unwrap();
        assert_eq!(import_report.level, CompatibilityLevel::Partial);
        let sources = store.original_sources(&original).unwrap().to_vec();
        assert_eq!(sources.len(), 1);
        assert_eq!(store.original_bytes(&sources[0]).unwrap(), bytes);
        let loaded = store.load_version(&original).unwrap();
        let module = ModuleId::new(3).unwrap();
        let model = loaded.snapshot.view.module(module).unwrap().model;
        let mut draft = PresetDraft::from_snapshot(&loaded.snapshot);
        let edit_bits = 51.0f32.to_bits();
        draft
            .apply(&EditPatch::SetParameter {
                module,
                model,
                model_index: ModelIndex(0), // Offline slot edit; no XML index is sent.
                parameter: ParamId::new(0).unwrap(),
                value: F32Bits(edit_bits),
            })
            .unwrap();
        let edited = draft
            .snapshot(&loaded.snapshot, SnapshotSource::Simulator, Revision(1))
            .unwrap();
        let edited_version = store
            .store_snapshot(Arc::new(edited), loaded.metadata, Some(original.clone()))
            .unwrap();
        drop(store);
        let store = LibraryStore::open(&library).unwrap();
        assert_eq!(store.original_bytes(&sources[0]).unwrap(), bytes);
        assert_eq!(store.original_sources(&edited_version).unwrap(), sources);
        let unchanged_output = temp.path("unchanged.prst");
        store
            .export_native(&NativeExportRequest {
                path: unchanged_output.clone(),
                version: original,
                mode: NativeExportMode::PreserveOriginal,
            })
            .unwrap();
        assert_eq!(fs::read(unchanged_output).unwrap(), bytes);
        let edited_output = temp.path("edited.prst");
        store
            .export_native(&NativeExportRequest {
                path: edited_output.clone(),
                version: edited_version,
                mode: NativeExportMode::PreserveOriginal,
            })
            .unwrap();
        let exported = fs::read(edited_output).unwrap();
        let position = amp_first_parameter(&bytes);
        let mut expected = bytes.clone();
        expected[position..position + 4].copy_from_slice(&edit_bits.to_le_bytes());
        let checksum_at = expected.len() - 4;
        let sum = expected[..checksum_at]
            .iter()
            .fold(0_u32, |sum, value| sum.wrapping_add(u32::from(*value)));
        expected[checksum_at..].copy_from_slice(&sum.to_be_bytes());
        assert_ne!(exported, bytes);
        assert_eq!(exported, expected, "Unedited or opaque bytes changed");
        let after = NativeFile::parse(&exported).unwrap();
        assert_eq!(after.header(), file.header());
        assert_eq!(after.embedded_ir_bytes(), None);
        let before_snapshot = &loaded.snapshot;
        let after_snapshot = PresetSnapshot::from_raw(
            after.raw().clone(),
            SnapshotSource::Simulator,
            None,
            Revision(2),
        );
        assert_eq!(before_snapshot.view.name, after_snapshot.view.name);
        assert_eq!(
            before_snapshot.view.patch_assignments,
            after_snapshot.view.patch_assignments
        );
        assert_eq!(before_snapshot.view.chain, after_snapshot.view.chain);
        assert_eq!(before_snapshot.view.metadata, after_snapshot.view.metadata);
        assert_eq!(before_snapshot.view.tempo, after_snapshot.view.tempo);
        assert_eq!(before_snapshot.view.level, after_snapshot.view.level);
        let mut corrupt = bytes.clone();
        corrupt[position] ^= 1;
        assert!(
            NativeFile::parse(&corrupt)
                .unwrap_err()
                .message
                .contains("checksum")
        );
        assert_eq!(fs::read(path).unwrap(), bytes);
        cases.push(json!({"path": profile["path"], "source_sha256": profile["sha256"],
            "mode": 1, "bytes": bytes.len(), "records": 33, "name": before_snapshot.view.display_name(),
            "original_retained_after_reopen": true, "unchanged_export_byte_exact": true,
            "edited_export_sha256": digest(&exported), "parameter_file_offset": position,
            "parameter_bits": edit_bits, "edit_matches_independent_byte_walk": true,
            "opaque_bytes_and_assignments_unchanged": true, "bad_checksum_rejected": true,
            "original_editor_interoperability_tested": false, "compatibility": "Partial"}));
    }
    let report = json!({"schema": 1, "midi_opened": false, "public_presets_redistributed": false, "cases": cases});
    eprintln!("{report}");
    if let Some(path) = std::env::var_os("GP200_ANDERTONS_PRESET_REPORT") {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap()
            .write_all(&serde_json::to_vec_pretty(&report).unwrap())
            .unwrap();
    }
}
