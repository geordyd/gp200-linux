use crate::{
    Options,
    files::{self, AtomicOutput},
};
use gp200_assets::*;
use gp200_domain::*;
use gp200_library::*;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

pub(crate) fn run(options: &Options) -> Result<Value, ServiceError> {
    match options.command.as_str() {
        "inspect" => inspect(options),
        "library" => library(options),
        "asset" => asset(options),
        "trace" => trace(options),
        _ => Err(ServiceError::invalid("Unknown offline command")),
    }
}
fn selected_nam_size(options: &Options) -> Result<f64, ServiceError> {
    let size = options
        .text("--nam-size")?
        .map(|text| {
            text.parse::<f64>()
                .map_err(|_| ServiceError::invalid("--nam-size must be finite in 0–1"))
        })
        .transpose()?
        .unwrap_or(1.0);
    if !size.is_finite() || !(0.0..=1.0).contains(&size) {
        return Err(ServiceError::invalid("--nam-size must be finite in 0–1"));
    }
    Ok(size)
}
fn inspect(options: &Options) -> Result<Value, ServiceError> {
    let path = options.path("--input")?;
    check_regular(&path, 256 * 1024 * 1024)?;
    if options.action == "backup" {
        let backup = LibraryStore::preview_device_backup(&path, BundleLimits::default())?;
        return Ok(
            json!({"backup": backup, "completeness": backup.completeness(), "hardware_access": false}),
        );
    }
    if options.action == "bundle" {
        return Ok(
            json!({"preview": LibraryStore::preview_bundle(&path, BundleLimits::default())?, "hardware_access": false}),
        );
    }
    let bytes = files::read_bounded(&path, MAX_SOURCE_BYTES)?;
    let details = match options.action.as_str() {
        "prst" => {
            let file = NativeFile::parse(&bytes)?;
            json!({"mode": file.mode(), "report": file.report(), "preset": PresetSnapshot::from_raw(file.raw().clone(), SnapshotSource::Capture { capture_id: format!("native-prst:{}", files::sha256(&bytes)) }, None, Revision(0)), "embedded_ir_bytes": file.embedded_ir_bytes().map(<[u8]>::len), "embedded_ir_bytes_note": "Counts retained opaque RISU bytes; this is not a count of recovered IR audio bytes.", "ir_reference_inspection": file.ir_reference_inspection()})
        }
        "wav" => json!(inspect_wav(&bytes)?.inspect()?),
        "nam" => {
            let info = inspect_nam_with_size(&bytes, selected_nam_size(options)?)?;
            if options.has("--nam-size") && !info.supported_inference {
                return Err(ServiceError::unsupported(info.reason));
            }
            json!(info)
        }
        "clo" => json!(inspect_snaptone(&bytes)?.info()),
        "firmware" => {
            let hash = JobSha256::of(&bytes);
            let retained = RetainedJobBlob::checked(
                AssetId(format!("sha256:{}", hash.hex())),
                std::sync::Arc::from(bytes.clone()),
                hash,
            )?;
            let review =
                FirmwareReview::inspect(retained, None, &FirmwareBackupAssessment::default())?;
            json!({"image": inspect_firmware(&bytes)?.info(), "review": review.summary(), "installed_firmware": null, "flash_executor": false})
        }
        _ => return Err(ServiceError::invalid("Unknown inspection format")),
    };
    Ok(
        json!({"kind": options.action, "bytes": bytes.len(), "sha256": files::sha256(&bytes), "details": details, "hardware_access": false}),
    )
}
pub(crate) fn store_path(options: &Options) -> Result<PathBuf, ServiceError> {
    let path = options.path("--store")?;
    if !path.is_absolute() {
        return Err(ServiceError::invalid("--store must be an absolute path"));
    }
    files::reject_links(&path)?;
    Ok(path)
}
fn check_regular(path: &std::path::Path, maximum: u64) -> Result<(), ServiceError> {
    files::reject_links(path)?;
    let metadata = std::fs::metadata(path).map_err(files::io)?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err(ServiceError::invalid(
            "Select a regular file within the format size limit",
        ));
    }
    Ok(())
}
fn library(options: &Options) -> Result<Value, ServiceError> {
    if matches!(
        options.action.as_str(),
        "import" | "bundle-import" | "backup-import"
    ) {
        check_regular(&options.path("--input")?, 256 * 1024 * 1024)?;
    }
    let mut store = LibraryStore::open(store_path(options)?)?;
    match options.action.as_str() {
        "list" => Ok(
            json!({"entries": store.query(&LibraryQuery { text: options.text("--text")?.unwrap_or_default().into(), ..Default::default() }), "generation": store.generation()}),
        ),
        "backup-list" => Ok(json!({"backups": store.device_backups()?})),
        "import" => {
            let (version, report) = store.import_native(&ImportRequest {
                path: options.path("--input")?,
            })?;
            Ok(json!({"version": version, "report": report}))
        }
        "bundle-import" => {
            let result = store.import_bundle(&ImportRequest {
                path: options.path("--input")?,
            })?;
            Ok(json!({"imported": result}))
        }
        "backup-import" => {
            let backup = store.import_device_backup(options.path("--input")?)?;
            Ok(json!({"backup": backup, "completeness": backup.completeness()}))
        }
        "export" | "bundle-export" | "backup-export" => {
            let path = options.path("--output")?;
            let output = AtomicOutput::new(&path)?;
            let result = match options.action.as_str() {
                "export" => {
                    json!({"report": store.export_native(&NativeExportRequest { path: output.path.clone(), version: checked_version(options)?, mode: if options.has("--preset-only") { NativeExportMode::PresetOnly } else { NativeExportMode::PreserveOriginal } })?})
                }
                "bundle-export" => {
                    json!({"report": store.export_bundle(&ExportRequest { path: output.path.clone(), versions: vec![checked_version(options)?], include_assets: options.has("--include-assets") })?})
                }
                _ => {
                    json!({"completeness": store.export_device_backup(options.required_text("--backup-id")?, &output.path)?})
                }
            };
            let bytes = files::read_bounded(&output.path, 256 * 1024 * 1024)?;
            output.publish()?;
            Ok(json!({"result": result, "bytes": bytes.len(), "sha256": files::sha256(&bytes)}))
        }
        _ => Err(ServiceError::invalid("Unknown library action")),
    }
}
fn checked_version(options: &Options) -> Result<LibraryVersionId, ServiceError> {
    let text = options.required_text("--version")?;
    if text.is_empty() || text.len() > 256 || text.chars().any(char::is_control) {
        return Err(ServiceError::invalid("Invalid library version ID"));
    }
    Ok(LibraryVersionId(text.into()))
}
fn asset(options: &Options) -> Result<Value, ServiceError> {
    let input = options.path("--input")?;
    files::reject_links(&input)?;
    let output = options.path("--output")?;
    files::fresh_output(&output)?;
    if options.action == "ir-pack" {
        let source = files::read_bounded(&input, MAX_SOURCE_BYTES)?;
        let packed = pack_device_ir(
            &source,
            if options.has("--truncate") {
                IrLengthPolicy::KeepFirst1024
            } else {
                IrLengthPolicy::RejectTruncation
            },
        )?;
        files::write_new(&output, packed.raw())?;
        return Ok(
            json!({"source_sha256": files::sha256(&source), "coefficients": packed.info(), "hardware_upload": false}),
        );
    }
    let kind = match options.required_text("--kind")? {
        "ir" => AssetKind::ImpulseResponse,
        "nam" => AssetKind::Nam,
        _ => {
            return Err(ServiceError::unsupported(
                "Only IR and NAM local preparation are supported",
            ));
        }
    };
    let ir_keys = [
        "--sample-rate",
        "--channel",
        "--normalize-peak",
        "--truncate-frames",
    ];
    let nam_keys = [
        "--input-wav",
        "--input-channel",
        "--output-format",
        "--activation",
        "--nam-size",
        "--resample-input",
        "--max-operations",
        "--calibration-timeout-seconds",
    ];
    let disallowed = if kind == AssetKind::Nam {
        &ir_keys[..]
    } else {
        &nam_keys[..]
    };
    if disallowed.iter().any(|key| options.has(key)) {
        return Err(ServiceError::invalid(
            "Preparation options belong to a different asset kind",
        ));
    }
    let mut preparation = BTreeMap::new();
    for key in if kind == AssetKind::Nam {
        &nam_keys[..]
    } else {
        &ir_keys[..]
    } {
        if let Some(value) = options.text(key)? {
            preparation.insert(
                key.trim_start_matches("--").replace('-', "_"),
                value.to_owned(),
            );
        }
    }
    if kind == AssetKind::Nam {
        selected_nam_size(options)?;
        if options.has("--nam-size") && options.text("--output-format")? == Some("clo") {
            return Err(ServiceError::invalid(
                "--nam-size applies only to NAM WAV output; CLO retains its full-size calibration profile",
            ));
        }
        files::reject_links(&options.path("--input-wav")?)?;
        options.required_text("--input-channel")?;
        if options
            .text("--output-format")?
            .is_some_and(|v| !matches!(v, "wav" | "clo"))
        {
            return Err(ServiceError::invalid("--output-format must be wav or clo"));
        }
    }
    if kind == AssetKind::Nam {
        if options.text("--output-format")? != Some("clo") {
            preparation.remove("output_format");
            if options.has("--calibration-timeout-seconds") {
                return Err(ServiceError::invalid(
                    "Calibration timeout applies only to CLO output",
                ));
            }
        } else if options.has("--resample-input") {
            return Err(ServiceError::invalid(
                "CLO calibration performs its validated rate conversion; omit --resample-input",
            ));
        }
    }
    let mut store = AssetStore::open(store_path(options)?)?;
    let result = store.prepare(
        &AssetJobSpec {
            id: JobId(1),
            kind,
            source: input,
            output: None,
            options: preparation,
        },
        || false,
    )?;
    let derived = result.derived.as_ref().ok_or_else(|| {
        ServiceError::new(
            ErrorKind::Protocol,
            "Preparation completed without a derived asset",
        )
    })?;
    let bytes = store.content_bytes(&derived.id)?;
    files::write_new(&output, &bytes)?;
    Ok(
        json!({"asset": derived, "bytes": bytes.len(), "sha256": files::sha256(&bytes), "hardware_upload": false}),
    )
}
fn trace(options: &Options) -> Result<Value, ServiceError> {
    use gp200_protocol::{Reassembler, StreamDecoder, StreamEvent};
    use gp200_sim::{Delivery, PassiveReplay, ReplayManifest};
    let bytes = files::read_bounded(&options.path("--input")?, 8 * 1024 * 1024)?;
    let (chunks, timing): (Vec<Vec<u8>>, &str) = if options.action == "replay" {
        let exchanges = ReplayManifest::parse(&bytes)?.exchanges()?;
        let mut chunks = Vec::new();
        for exchange in exchanges {
            let mut replay = PassiveReplay::new(exchange.response)?;
            chunks.extend(
                replay
                    .advance(Duration::MAX)
                    .into_iter()
                    .filter_map(|d| match d {
                        Delivery::Bytes { bytes, .. } => Some(bytes),
                        Delivery::Disconnected { .. } => None,
                    }),
            );
        }
        (
            chunks,
            "Responses concatenated in manifest order; no exchange timing is asserted",
        )
    } else {
        (
            bytes.chunks(4096).map(<[u8]>::to_vec).collect(),
            "No capture timestamps; parser uses one synthetic zero-time stream",
        )
    };
    let mut stream = StreamDecoder::new();
    let mut assembler = Reassembler::default();
    let mut packets = 0;
    let mut realtime = 0;
    let mut other = 0;
    let mut foreign = 0;
    let mut transfers = Vec::new();
    for chunk in chunks {
        for event in stream.feed(&chunk) {
            match event {
                StreamEvent::Packet(packet) => {
                    packets += 1;
                    if let Some(raw) = assembler
                        .add(&packet, Duration::ZERO)
                        .map_err(|e| ServiceError::new(ErrorKind::Protocol, e.to_string()))?
                    {
                        if transfers.len() >= 4096 {
                            return Err(ServiceError::invalid(
                                "Trace contains more than 4096 transfers",
                            ));
                        }
                        transfers.push(json!({"kind": raw.kind(), "bytes": raw.payload().len(), "sha256": files::sha256(raw.payload()), "records": raw.records().iter().map(|r| json!({"command": r.command, "body_bytes": r.body.len()})).collect::<Vec<_>>()}));
                    }
                }
                StreamEvent::Realtime(_) => realtime += 1,
                StreamEvent::OtherMidi(_) => other += 1,
                StreamEvent::UnrecognizedSysex(_) => foreign += 1,
                StreamEvent::Error(error) => {
                    return Err(ServiceError::new(ErrorKind::Protocol, error.to_string()));
                }
            }
        }
    }
    stream
        .finish()
        .map_err(|e| ServiceError::new(ErrorKind::Protocol, e.to_string()))?;
    if assembler.progress().is_some() {
        return Err(ServiceError::new(
            ErrorKind::Protocol,
            "Trace ends with an incomplete normal-mode transfer",
        ));
    }
    Ok(
        json!({"input_sha256": files::sha256(&bytes), "packets": packets, "transfers": transfers, "realtime_bytes": realtime, "other_midi_bytes": other, "foreign_sysex": foreign, "timing": timing, "hardware_access": false}),
    )
}
