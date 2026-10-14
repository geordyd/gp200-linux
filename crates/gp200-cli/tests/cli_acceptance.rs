//! Process-level supported command coverage. No test opens a physical MIDI port.
use gp200_assets::{WaveAudio, WaveEncoding, write_float_wav};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};
use zip::ZipArchive;
static SERIAL: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "gp200-cli-test-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn run(&self, args: &[OsString]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_gp200-cli"))
            .args(args)
            .current_dir(&self.0)
            .env_clear()
            .env("HOME", &self.0)
            .env("GP200_STUDIO_DATA", self.path("data"))
            .output()
            .unwrap()
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}
fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], true);
    value["data"].clone()
}
fn failed(output: Output, code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout {} stderr {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    let value: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["exit_code"], code);
    value
}
fn fixture(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(relative)
}
fn absolute_fixture(relative: &str) -> PathBuf {
    fs::canonicalize(fixture(relative)).unwrap()
}
fn input_command(temp: &Temp, command: &str, action: &str, path: &Path) -> Output {
    temp.run(&[
        command.into(),
        action.into(),
        "--input".into(),
        path.as_os_str().into(),
    ])
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[test]
fn headless_help_version_and_invalid_options_do_not_create_device_lock() {
    let temp = Temp::new();
    for option in ["--help", "--version"] {
        let output = temp.run(&args(&[option]));
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
    }
    for options in [
        &["bogus", "command"][..],
        &["inspect", "nam", "--input", "a", "--input", "b"],
        &["device", "current"],
        &["device", "stored", "--unique", "--slot", "256"],
        &[
            "device",
            "current",
            "--unique",
            "--input-port",
            "a",
            "--output-port",
            "b",
        ],
        &["device", "current", "--unique", "--timeout-seconds", "0"],
        &["device", "current", "--unique", "--send", "f0"],
    ] {
        failed(temp.run(&args(options)), 2);
    }
    assert!(!temp.path(".hardware-session.lock").exists());
    assert!(!temp.path("data").exists());
}
#[test]
fn busy_hardware_lock_fails_before_backend_is_constructed() {
    let temp = Temp::new();
    let lock = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp.path(".hardware-session.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    let error = failed(temp.run(&args(&["device", "identity", "--unique"])), 4);
    assert_eq!(error["error"]["kind"], "Busy");
}
#[test]
fn offline_inspectors_use_shared_formats_and_exact_input_hashes() {
    let temp = Temp::new();
    for (kind, path) in [
        ("prst", "native/constructed-mode1.prst"),
        ("wav", "device-assets/synthetic-ir-pcm24.wav"),
        ("nam", "assets/upstream-lstm-0.5.4.nam"),
        ("clo", "assets/clo/upstream-final1024.clo"),
        ("firmware", "device-assets/synthetic-firmware.bin"),
    ] {
        let source = absolute_fixture(path);
        let data = success(input_command(&temp, "inspect", kind, &source));
        assert_eq!(data["sha256"], digest(&fs::read(source).unwrap()));
        assert_eq!(data["hardware_access"], false);
        if kind == "firmware" {
            assert!(data["details"]["installed_firmware"].is_null());
            assert_eq!(data["details"]["flash_executor"], false);
        }
    }
    assert!(!temp.path(".hardware-session.lock").exists());
}
#[test]
fn corrupt_clo_and_oversized_or_nonregular_inputs_fail_without_publication() {
    let temp = Temp::new();
    let source = temp.path("bad.clo");
    fs::write(&source, b"VTSI").unwrap();
    failed(input_command(&temp, "inspect", "clo", &source), 2);
    let huge = temp.path("huge.nam");
    File::create(&huge)
        .unwrap()
        .set_len(64 * 1024 * 1024 + 1)
        .unwrap();
    failed(input_command(&temp, "inspect", "nam", &huge), 2);
    failed(input_command(&temp, "inspect", "wav", &temp.0), 2);
}
#[test]
#[cfg(unix)]
fn symbolic_link_input_and_output_ancestors_are_rejected() {
    use std::os::unix::fs::symlink;
    let temp = Temp::new();
    let link = temp.path("source.nam");
    symlink(absolute_fixture("assets/upstream-lstm-0.5.4.nam"), &link).unwrap();
    failed(input_command(&temp, "inspect", "nam", &link), 2);
    fs::create_dir(temp.path("real")).unwrap();
    symlink(temp.path("real"), temp.path("alias")).unwrap();
    let output = temp.path("alias/support.zip");
    failed(
        temp.run(&[
            "support".into(),
            "export".into(),
            "--output".into(),
            output.as_os_str().into(),
        ]),
        2,
    );
    assert!(!temp.path("real/support.zip").exists());
}
#[test]
fn native_library_export_roundtrip_is_byte_identical_and_never_overwrites() {
    let temp = Temp::new();
    let source = temp.path("蓝色 preset $literal 50%.prst");
    let original = fs::read(absolute_fixture("native/constructed-mode1.prst")).unwrap();
    fs::write(&source, &original).unwrap();
    let store = temp.path("library");
    let imported = success(temp.run(&[
        "library".into(),
        "import".into(),
        "--store".into(),
        store.as_os_str().into(),
        "--input".into(),
        source.as_os_str().into(),
    ]));
    let version = imported["version"].as_str().unwrap();
    let exported = temp.path("roundtrip.prst");
    let export_args = vec![
        "library".into(),
        "export".into(),
        "--store".into(),
        store.as_os_str().into(),
        "--version".into(),
        version.into(),
        "--output".into(),
        exported.as_os_str().into(),
    ];
    let result = success(temp.run(&export_args));
    assert_eq!(result["sha256"], digest(&original));
    assert_eq!(fs::read(&exported).unwrap(), original);
    failed(temp.run(&export_args), 4);
    assert_eq!(fs::read(&exported).unwrap(), original);
    let listed = success(temp.run(&[
        "library".into(),
        "list".into(),
        "--store".into(),
        store.as_os_str().into(),
    ]));
    assert_eq!(listed["entries"].as_array().unwrap().len(), 1);
    assert!(!temp.path(".hardware-session.lock").exists());
    assert!(!fs::read_dir(&temp.0).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".gp200-cli-")
    }));
}
#[test]
fn portable_library_bundle_exports_previews_and_imports() {
    let temp = Temp::new();
    let store = temp.path("library");
    let source = absolute_fixture("native/constructed-mode1.prst");
    let imported = success(temp.run(&[
        "library".into(),
        "import".into(),
        "--store".into(),
        store.as_os_str().into(),
        "--input".into(),
        source.as_os_str().into(),
    ]));
    let version = imported["version"].as_str().unwrap();
    let archive = temp.path("copy.gp200bundle");
    success(temp.run(&[
        "library".into(),
        "bundle-export".into(),
        "--store".into(),
        store.as_os_str().into(),
        "--version".into(),
        version.into(),
        "--output".into(),
        archive.as_os_str().into(),
    ]));
    let preview = success(input_command(&temp, "inspect", "bundle", &archive));
    assert_eq!(preview["preview"]["version_count"], 1);
    let restored = temp.path("imported");
    success(temp.run(&[
        "library".into(),
        "bundle-import".into(),
        "--store".into(),
        restored.as_os_str().into(),
        "--input".into(),
        archive.as_os_str().into(),
    ]));
    let listed = success(temp.run(&[
        "library".into(),
        "list".into(),
        "--store".into(),
        restored.as_os_str().into(),
    ]));
    assert_eq!(listed["entries"].as_array().unwrap().len(), 1);
}
#[test]
fn exact_ir_pack_exports_actual_4096_bytes_without_midi() {
    let temp = Temp::new();
    let source = absolute_fixture("device-assets/synthetic-ir-pcm24.wav");
    let output = temp.path("ir.bin");
    let data = success(temp.run(&[
        "asset".into(),
        "ir-pack".into(),
        "--input".into(),
        source.as_os_str().into(),
        "--output".into(),
        output.as_os_str().into(),
    ]));
    let expected = gp200_assets::pack_device_ir(
        &fs::read(source).unwrap(),
        gp200_assets::IrLengthPolicy::RejectTruncation,
    )
    .unwrap();
    assert_eq!(fs::read(output).unwrap(), expected.raw());
    assert_eq!(data["hardware_upload"], false);
}
fn nam_args(temp: &Temp, output: &Path, extra: &[&str]) -> Vec<OsString> {
    let audio = WaveAudio {
        sample_rate: 48000,
        channels: 1,
        source_encoding: WaveEncoding::Float,
        source_bits: 32,
        samples: vec![0.1, -0.1, 0.0, 0.5],
    };
    let input = temp.path("input.wav");
    fs::write(&input, write_float_wav(&audio).unwrap()).unwrap();
    let mut options = vec![
        "asset".into(),
        "prepare".into(),
        "--kind".into(),
        "nam".into(),
        "--input".into(),
        absolute_fixture("assets/upstream-lstm-0.5.4.nam").into_os_string(),
        "--input-wav".into(),
        input.into_os_string(),
        "--input-channel".into(),
        "0".into(),
        "--store".into(),
        temp.path("assets").into_os_string(),
        "--output".into(),
        output.as_os_str().into(),
    ];
    options.extend(args(extra));
    options
}
#[test]
fn nam_render_exports_store_bytes_and_small_budget_rejection_has_no_output() {
    let temp = Temp::new();
    let output = temp.path("render.wav");
    let options = nam_args(&temp, &output, &["--output-format", "wav"]);
    let data = success(temp.run(&options));
    let bytes = fs::read(&output).unwrap();
    assert_eq!(data["sha256"], digest(&bytes));
    assert_eq!(data["hardware_upload"], false);
    let wave = gp200_assets::inspect_wav(&bytes).unwrap();
    assert_eq!(wave.channels, 2);
    assert_eq!(wave.frames(), 4);
    let denied = temp.path("too-small.wav");
    failed(
        temp.run(&nam_args(&temp, &denied, &["--max-operations", "1"])),
        3,
    );
    assert!(!denied.exists());
    assert!(!temp.path(".hardware-session.lock").exists());
}
#[test]
fn clo_request_rejects_unverified_stimulus_without_substituting_audio() {
    let temp = Temp::new();
    let output = temp.path("bad.clo");
    failed(
        temp.run(&nam_args(&temp, &output, &["--output-format", "clo"])),
        2,
    );
    assert!(!output.exists());
}
#[test]
fn passive_trace_replay_uses_codec_and_rejects_truncated_streams() {
    let temp = Temp::new();
    let manifest = absolute_fixture("sim/replay-identity-current.json");
    let data = success(input_command(&temp, "trace", "replay", &manifest));
    assert_eq!(data["hardware_access"], false);
    assert!(!data["transfers"].as_array().unwrap().is_empty());
    let broken = temp.path("incomplete.bin");
    fs::write(&broken, [0xf0, 0x21, 0x25]).unwrap();
    failed(input_command(&temp, "trace", "inspect", &broken), 6);
}
#[test]
fn replay_does_not_silently_omit_late_manifest_chunks() {
    let temp = Temp::new();
    let mut manifest: Value = serde_json::from_slice(
        &fs::read(absolute_fixture("sim/replay-identity-current.json")).unwrap(),
    )
    .unwrap();
    let first = manifest["exchanges"][0].clone();
    manifest["exchanges"] = json!([first.clone(), first]);
    for exchange in manifest["exchanges"].as_array_mut().unwrap() {
        for chunk in exchange["response"].as_array_mut().unwrap() {
            chunk["at_ms"] = json!(3_600_000);
        }
    }
    let path = temp.path("late.json");
    fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let data = success(input_command(&temp, "trace", "replay", &path));
    assert!(!data["transfers"].as_array().unwrap().is_empty());
}
fn support_args(path: &Path) -> Vec<OsString> {
    vec![
        "support".into(),
        "export".into(),
        "--output".into(),
        path.as_os_str().into(),
    ]
}
fn zip_contents(path: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut zip = ZipArchive::new(File::open(path).unwrap()).unwrap();
    let mut entries = std::collections::BTreeMap::new();
    for index in 0..zip.len() {
        let mut file = zip.by_index(index).unwrap();
        assert_eq!(file.unix_mode().unwrap() & 0o777, 0o600);
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        entries.insert(file.name().to_owned(), bytes);
    }
    entries
}
#[test]
fn support_default_contains_only_reviewed_allowlist_and_manifest_hashes() {
    let temp = Temp::new();
    fs::write(temp.path("secret-preset.prst"), b"personal name").unwrap();
    let output = temp.path("support.zip");
    let data = success(temp.run(&support_args(&output)));
    let entries = zip_contents(&output);
    assert_eq!(
        entries.keys().map(String::as_str).collect::<Vec<_>>(),
        ["manifest.json", "profiles.json", "summary.json"]
    );
    let manifest: Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    for (name, bytes) in &entries {
        if name != "manifest.json" {
            assert_eq!(manifest["entries"][name]["sha256"], digest(bytes));
            assert_eq!(manifest["entries"][name]["bytes"], bytes.len());
        }
    }
    assert_eq!(data["archive_sha256"], digest(&fs::read(&output).unwrap()));
    assert_eq!(data["attachments"], 0);
    assert!(!temp.path(".hardware-session.lock").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
#[test]
fn support_summary_strips_preset_endpoint_path_and_error_text() {
    let temp = Temp::new();
    let summary = temp.path("result.json");
    fs::write(&summary, serde_json::to_vec(&json!({"schema":1,"ok":true,"command":"device current","data":{"identity":{"model":"Gp200","mode":"Hardware","firmware":null,"display_name":"SECRET_DEVICE"},"selection":{"input_id":"SECRET_PORT"},"snapshot":{"name":"SECRET_PRESET"},"read_only":true,"pedal_mutations":0,"home":"SECRET_HOME"},"error":{"kind":"Timeout","message":"SECRET_ERROR"}})).unwrap()).unwrap();
    let output = temp.path("support.zip");
    let mut options = support_args(&output);
    options.extend(["--session-summary".into(), summary.into_os_string()]);
    success(temp.run(&options));
    let entries = zip_contents(&output);
    let bytes = &entries["session-summary.json"];
    let text = String::from_utf8_lossy(bytes);
    assert!(!text.contains("SECRET"));
    let value: Value = serde_json::from_slice(bytes).unwrap();
    assert_eq!(value["model"], "Gp200");
    assert_eq!(value["read_only"], true);
    assert_eq!(value["error_kind"], "Timeout");
}
#[test]
fn explicit_support_attachment_preserves_bytes_and_hides_original_filename() {
    let temp = Temp::new();
    let source = temp.path("my private live MIDI.bin");
    fs::write(&source, [0x01, 0xff, 0x00]).unwrap();
    let output = temp.path("support.zip");
    let mut options = support_args(&output);
    options.extend(["--include-midi".into(), source.into_os_string()]);
    success(temp.run(&options));
    let entries = zip_contents(&output);
    assert_eq!(entries["attachments/00-midi.bin"], [0x01, 0xff, 0x00]);
    assert!(entries.keys().all(|name| !name.contains("private")));
}
#[test]
fn support_bounds_and_invalid_summary_leave_no_archive() {
    let temp = Temp::new();
    let huge = temp.path("big.wav");
    File::create(&huge)
        .unwrap()
        .set_len(16 * 1024 * 1024 + 1)
        .unwrap();
    let output = temp.path("support.zip");
    let mut options = support_args(&output);
    options.extend(["--include-audio".into(), huge.into_os_string()]);
    failed(temp.run(&options), 2);
    assert!(!output.exists());
    let mut count = support_args(&output);
    for _ in 0..9 {
        count.extend(["--include-midi".into(), "any.bin".into()]);
    }
    failed(temp.run(&count), 2);
    let summary = temp.path("summary.json");
    fs::write(
        &summary,
        b"{\"schema\":1,\"ok\":true,\"command\":\"inspect nam\"}",
    )
    .unwrap();
    let mut bad = support_args(&output);
    bad.extend(["--session-summary".into(), summary.into_os_string()]);
    failed(temp.run(&bad), 2);
    assert!(!output.exists());
}

#[test]
fn partial_backup_archive_is_preserved_without_claiming_complete_device_backup() {
    use gp200_domain::*;
    use gp200_library::{BackupAcquisition, BackupOrigin, LibraryStore};
    use std::sync::Arc;
    let temp = Temp::new();
    let store_path = temp.path("backups");
    let mut store = LibraryStore::open(&store_path).unwrap();
    let origin = BackupOrigin {
        identity: DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Simulator,
            firmware: None,
            display_name: "Synthetic CLI archive fixture".into(),
        },
        epoch: SessionEpoch(1),
        device_key: "test-only-simulator".into(),
        evidence: vec![],
    };
    let backup = store.begin_device_backup(origin).unwrap();
    let raw = gp200_protocol::RawTransfer::from_payload(
        0x12,
        include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
    )
    .unwrap();
    let snapshot = PresetSnapshot::from_raw(
        Arc::new(raw),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(1),
    );
    store
        .capture_backup_preset(&backup.id, Arc::new(snapshot))
        .unwrap();
    store
        .finish_device_backup(&backup.id, BackupAcquisition::Cancelled)
        .unwrap();
    drop(store);
    let archive = temp.path("partial.gp200backup");
    success(temp.run(&[
        "library".into(),
        "backup-export".into(),
        "--store".into(),
        store_path.as_os_str().into(),
        "--backup-id".into(),
        backup.id.into(),
        "--output".into(),
        archive.as_os_str().into(),
    ]));
    let preview = success(input_command(&temp, "inspect", "backup", &archive));
    assert_eq!(preview["backup"]["acquisition"], "Cancelled");
    assert_eq!(preview["backup"]["presets"].as_object().unwrap().len(), 0);
    let imported_path = temp.path("imported-backup");
    let imported = success(temp.run(&[
        "library".into(),
        "backup-import".into(),
        "--store".into(),
        imported_path.into_os_string(),
        "--input".into(),
        archive.into_os_string(),
    ]));
    assert_eq!(
        preview["backup"]["edit_buffer"],
        imported["backup"]["edit_buffer"]
    );
    assert_eq!(preview["completeness"], imported["completeness"]);
    assert!(!temp.path(".hardware-session.lock").exists());
}

#[test]
fn shared_data_lock_blocks_a_cli_in_a_different_working_directory() {
    let temp = Temp::new();
    fs::create_dir(temp.path("data")).unwrap();
    let lock = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp.path("data/hardware-session.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    let error = failed(temp.run(&args(&["device", "identity", "--unique"])), 4);
    assert_eq!(error["error"]["kind"], "Busy");
}

#[test]
fn support_combined_attachment_limit_is_checked_before_publication() {
    let temp = Temp::new();
    let source = temp.path("twelve-megabytes.bin");
    File::create(&source)
        .unwrap()
        .set_len(12 * 1024 * 1024)
        .unwrap();
    let output = temp.path("support.zip");
    let mut options = support_args(&output);
    for _ in 0..3 {
        options.extend(["--include-midi".into(), source.as_os_str().into()]);
    }
    failed(temp.run(&options), 2);
    assert!(!output.exists());
}

#[test]
fn prepared_ir_is_real_float_wav_and_existing_output_is_preserved() {
    let temp = Temp::new();
    let source = absolute_fixture("device-assets/synthetic-ir-pcm24.wav");
    let output = temp.path("prepared.wav");
    let options = vec![
        "asset".into(),
        "prepare".into(),
        "--kind".into(),
        "ir".into(),
        "--input".into(),
        source.into_os_string(),
        "--store".into(),
        temp.path("assets").into_os_string(),
        "--output".into(),
        output.as_os_str().into(),
        "--sample-rate".into(),
        "44100".into(),
        "--channel".into(),
        "preserve".into(),
    ];
    let data = success(temp.run(&options));
    let bytes = fs::read(&output).unwrap();
    assert_eq!(data["sha256"], digest(&bytes));
    let audio = gp200_assets::inspect_wav(&bytes).unwrap();
    assert_eq!(audio.source_encoding, WaveEncoding::Float);
    assert_eq!(audio.sample_rate, 44100);
    failed(temp.run(&options), 4);
    assert_eq!(fs::read(&output).unwrap(), bytes);
}

#[test]
fn selected_nam_inspection_and_render_use_actual_bin_cost_and_independent_output() {
    let temp = Temp::new();
    let source = absolute_fixture("assets/upstream-slimmable_wavenet.nam");
    let inspected = success(temp.run(&[
        "inspect".into(),
        "nam".into(),
        "--input".into(),
        source.as_os_str().into(),
        "--nam-size".into(),
        "0".into(),
    ]));
    assert_eq!(inspected["details"]["requested_size"], 0.0);
    assert_eq!(
        inspected["details"]["size_selection"]["Channels"],
        json!([1])
    );
    let reference: Value = serde_json::from_slice(
        &fs::read(fixture(
            "assets/upstream-slimmable_wavenet-kernel-reference.json",
        ))
        .unwrap(),
    )
    .unwrap();
    let input: Vec<f32> = reference["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_f64().unwrap() as f32)
        .collect();
    let output = temp.path("selected.wav");
    let mut options = nam_args(
        &temp,
        &output,
        &["--output-format", "wav", "--nam-size", "0"],
    );
    let source_index = options
        .iter()
        .position(|option| option == "--input")
        .unwrap()
        + 1;
    options[source_index] = source.into_os_string();
    fs::write(
        temp.path("input.wav"),
        write_float_wav(&WaveAudio {
            sample_rate: 48000,
            channels: 1,
            source_encoding: WaveEncoding::Float,
            source_bits: 32,
            samples: input.clone(),
        })
        .unwrap(),
    )
    .unwrap();
    let cost = inspected["details"]["operations_per_sample"]
        .as_u64()
        .unwrap()
        * input.len() as u64;
    options.extend(["--max-operations".into(), cost.to_string().into()]);
    let data = success(temp.run(&options));
    assert_eq!(data["asset"]["properties"]["nam_size"], "0");
    assert_eq!(
        data["asset"]["properties"]["estimated_operations"],
        cost.to_string()
    );
    assert_eq!(data["asset"]["properties"]["size_selection"], "Channels 1");
    let bytes = fs::read(&output).unwrap();
    assert_eq!(data["sha256"], digest(&bytes));
    let audio = gp200_assets::inspect_wav(&bytes).unwrap();
    let expected = reference["sizes"]["0"]["standard"].as_array().unwrap();
    for (frame, value) in audio.samples.as_chunks::<2>().0.iter().zip(expected) {
        let expected = (value.as_f64().unwrap() as f32).clamp(-1.0, 1.0) * 0.31;
        assert!((frame[0] - expected).abs() <= 1.0 / 32768.0 + 1e-6 + 2e-5 * expected.abs());
        assert_eq!(frame[0], frame[1]);
    }
    let output_index = options
        .iter()
        .position(|option| option == "--output")
        .unwrap()
        + 1;
    let denied = temp.path("budget-denied.wav");
    options[output_index] = denied.clone().into_os_string();
    *options.last_mut().unwrap() = (cost - 1).to_string().into();
    failed(temp.run(&options), 3);
    assert!(!denied.exists());
    assert!(!temp.path(".hardware-session.lock").exists());
}
#[test]
fn invalid_or_fixed_nam_sizes_and_every_explicit_clo_size_are_rejected() {
    let temp = Temp::new();
    let fixed = absolute_fixture("assets/upstream-lstm-0.5.4.nam");
    for size in ["NaN", "inf", "-0.1", "1.1", "tiny"] {
        failed(
            temp.run(&[
                "inspect".into(),
                "nam".into(),
                "--input".into(),
                fixed.as_os_str().into(),
                "--nam-size".into(),
                size.into(),
            ]),
            2,
        );
    }
    failed(
        temp.run(&[
            "inspect".into(),
            "nam".into(),
            "--input".into(),
            fixed.as_os_str().into(),
            "--nam-size".into(),
            "0.5".into(),
        ]),
        3,
    );
    let output = temp.path("fixed.wav");
    failed(
        temp.run(&nam_args(&temp, &output, &["--nam-size", "0.5"])),
        3,
    );
    assert!(!output.exists());
    for size in ["0", "1"] {
        let output = temp.path(&format!("refused-{size}.clo"));
        let error = failed(
            temp.run(&nam_args(
                &temp,
                &output,
                &["--output-format", "clo", "--nam-size", size],
            )),
            2,
        );
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("only to NAM WAV")
        );
        assert!(!output.exists());
    }
    assert!(!temp.path(".hardware-session.lock").exists());
}
