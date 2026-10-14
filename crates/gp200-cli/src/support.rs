//! Explicit local allowlist. No directory walk, automatic preset or raw capture collection.
use crate::{
    Options,
    files::{self, AtomicOutput},
};
use gp200_domain::ServiceError;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Write,
};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};
pub(crate) const MAX_ATTACHMENTS: usize = 8;
const MAX_ATTACHMENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;

fn session_summary(value: &Value) -> Result<Value, ServiceError> {
    if value.get("schema") != Some(&json!(1)) || !value.get("ok").is_some_and(Value::is_boolean) {
        return Err(ServiceError::invalid(
            "Session summary must be a schema-1 CLI result",
        ));
    }
    // Copy scalar enum/counter fields only. Raw snapshots, endpoints, firmware
    // blobs, preset names, error messages and arbitrary extra keys are excluded.
    let mut result = serde_json::Map::new();
    result.insert("ok".into(), value["ok"].clone());
    if let Some(command) = value.get("command").and_then(Value::as_str) {
        if !matches!(
            command,
            "device ports"
                | "device identity"
                | "device current"
                | "device stored"
                | "device globals"
                | "device ir-metadata"
                | "device bank"
                | "device doctor"
        ) {
            return Err(ServiceError::invalid(
                "Summary must describe a supported device read command",
            ));
        }
        result.insert("command".into(), json!(command));
    }
    if let Some(code) = value
        .get("exit_code")
        .and_then(Value::as_u64)
        .filter(|code| (2..=8).contains(code))
    {
        result.insert("exit_code".into(), json!(code));
    }
    if let Some(kind) = value
        .pointer("/error/kind")
        .and_then(Value::as_str)
        .filter(|kind| {
            matches!(
                *kind,
                "InvalidData"
                    | "Unsupported"
                    | "Unverified"
                    | "Busy"
                    | "Disconnected"
                    | "Timeout"
                    | "Io"
                    | "Protocol"
                    | "Conflict"
                    | "Cancelled"
                    | "Indeterminate"
            )
        })
    {
        result.insert("error_kind".into(), json!(kind));
    }
    for key in [
        "read_only",
        "pedal_mutations",
        "content_captured",
        "occupancy_proven",
        "complete_device_backup",
    ] {
        if let Some(v) = value
            .pointer(&format!("/data/{key}"))
            .filter(|v| v.is_boolean() || v.is_u64())
        {
            result.insert(key.into(), v.clone());
        }
    }
    if let Some(model) = value
        .pointer("/data/identity/model")
        .and_then(Value::as_str)
        .filter(|model| matches!(*model, "Gp200" | "Gp200Lt" | "Gp200Jr" | "Unknown"))
    {
        result.insert("model".into(), json!(model));
    }
    if let Some(mode) = value
        .pointer("/data/identity/mode")
        .and_then(Value::as_str)
        .filter(|mode| matches!(*mode, "Hardware" | "Simulator" | "CaptureReplay"))
    {
        result.insert("backend_mode".into(), json!(mode));
    }
    result.insert(
        "firmware_known".into(),
        json!(
            value
                .pointer("/data/identity/firmware")
                .is_some_and(|v| !v.is_null())
        ),
    );
    Ok(Value::Object(result))
}
pub(crate) fn run(options: &Options) -> Result<Value, ServiceError> {
    let destination = options.path("--output")?;
    files::fresh_output(&destination)?;
    let mut entries = BTreeMap::<String, (String, Vec<u8>)>::new();
    let summary = json!({"schema": 1, "application": "GP200 Linux", "cli_version": env!("CARGO_PKG_VERSION"), "build_target": {"os": std::env::consts::OS, "architecture": std::env::consts::ARCH}, "calibration_helper": {"available": gp200_assets::calibration_helper_available(), "sha256": gp200_assets::calibration_helper_sha256()}, "collection": "Explicit local allowlist; no directory scan or network/MIDI access", "default_exclusions": ["raw MIDI", "audio", "preset bytes and names", "user directories", "environment", "USB serial and endpoint IDs", "unfiltered error messages"]});
    entries.insert(
        "summary.json".into(),
        (
            "build-summary".into(),
            serde_json::to_vec_pretty(&summary).map_err(files::invalid)?,
        ),
    );
    entries.insert(
        "profiles.json".into(),
        (
            "compiled-capability-profiles".into(),
            serde_json::to_vec_pretty(&gp200_transport::verified_capabilities())
                .map_err(files::invalid)?,
        ),
    );
    if let Some(path) = options.optional_path("--session-summary") {
        let bytes = files::read_bounded(&path, 1024 * 1024)?;
        let value: Value = serde_json::from_slice(&bytes).map_err(files::invalid)?;
        entries.insert(
            "session-summary.json".into(),
            (
                "redacted-device-summary".into(),
                serde_json::to_vec_pretty(&session_summary(&value)?).map_err(files::invalid)?,
            ),
        );
    }
    let mut total = entries
        .values()
        .map(|(_, bytes)| bytes.len())
        .sum::<usize>();
    for (index, (kind, path)) in options.inclusions.iter().enumerate() {
        let bytes = files::read_bounded(path, MAX_ATTACHMENT_BYTES)?;
        total = total
            .checked_add(bytes.len())
            .ok_or_else(|| ServiceError::invalid("Attachment length overflow"))?;
        if total > MAX_TOTAL_BYTES {
            return Err(ServiceError::invalid("Support contents exceed 32 MiB"));
        }
        // Numbered names never disclose original filename/directory metadata.
        entries.insert(
            format!("attachments/{index:02}-{kind}.bin"),
            (format!("explicit-user-{kind}"), bytes),
        );
    }
    let manifest = json!({"schema": 1, "format": "gp200-studio.support", "application_version": env!("CARGO_PKG_VERSION"), "hardware_access": false, "network_access": false, "entries": entries.iter().map(|(name, (kind, bytes))| (name, json!({"category": kind, "bytes": bytes.len(), "sha256": files::sha256(bytes)}))).collect::<BTreeMap<_, _>>()});
    let output = AtomicOutput::new(&destination)?;
    let mut writer = ZipWriter::new(
        OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&output.path)
            .map_err(files::io)?,
    );
    let entry_options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o600);
    for (name, (_, bytes)) in &entries {
        writer
            .start_file(name, entry_options)
            .map_err(files::invalid)?;
        writer.write_all(bytes).map_err(files::io)?;
    }
    writer
        .start_file("manifest.json", entry_options)
        .map_err(files::invalid)?;
    writer
        .write_all(&serde_json::to_vec_pretty(&manifest).map_err(files::invalid)?)
        .map_err(files::io)?;
    let file: File = writer.finish().map_err(files::invalid)?;
    file.sync_all().map_err(files::io)?;
    drop(file);
    let bytes = files::read_bounded(&output.path, MAX_TOTAL_BYTES + 1024 * 1024)?;
    output.publish()?;
    Ok(
        json!({"manifest": manifest, "archive_bytes": bytes.len(), "archive_sha256": files::sha256(&bytes), "attachments": options.inclusions.len(), "hardware_access": false, "network_access": false}),
    )
}
