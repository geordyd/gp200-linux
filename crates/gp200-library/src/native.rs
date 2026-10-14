//! The native GP-200 `.prst` container recovered from the Windows 1.8.1 editor.
//!
//! This is separate from the serializer's bare TLV + CRC blob. The file writer
//! replaces that CRC with a little-endian size word, then appends a big-endian
//! additive checksum. Original bytes and opaque header/asset data are retained.
use crate::store::{atomic_write, digest, invalid, read_bounded};
use crate::{
    LibraryStore, NativeIrReferenceInspection, OriginalSource, inspect_native_ir_reference,
};
use gp200_domain::*;
use gp200_protocol::RawTransfer;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, ops::Range, path::Path, sync::Arc};

const HEADER: usize = 32;
const FORMAT: &str = "valeton.gp200.prst.static-1.8.1";
const RESEARCH: &str = "analysis/implementation/research-native-format/FINDINGS.md";

#[derive(Debug, Clone, Copy)]
pub struct NativeFileLimits {
    pub maximum_file_bytes: usize,
    pub maximum_preset_bytes: usize,
    pub maximum_records: usize,
}
impl Default for NativeFileLimits {
    fn default() -> Self {
        Self {
            maximum_file_bytes: 64 * 1024 * 1024,
            maximum_preset_bytes: 16 * 1024,
            maximum_records: 256,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NativeFile {
    original: Arc<[u8]>,
    raw: Arc<RawTransfer>,
    mode: u16,
    preset: Range<usize>,
    embedded_ir: Option<Range<usize>>,
    report: ConversionReport,
}
impl NativeFile {
    pub fn parse(bytes: &[u8]) -> Result<Self, ServiceError> {
        Self::parse_bounded(bytes, NativeFileLimits::default())
    }
    pub fn parse_bounded(bytes: &[u8], limits: NativeFileLimits) -> Result<Self, ServiceError> {
        if bytes.len() > limits.maximum_file_bytes || bytes.len() < HEADER + 8 + 8 + 4 + 4 {
            return Err(invalid(
                "Native preset length is outside the supported bounds",
            ));
        }
        if &bytes[..4] != b"TSRP" || &bytes[16..20] != b"2-PG" {
            return Err(ServiceError::unsupported(
                "File is not a recovered GP-200 TSRP/2-PG preset",
            ));
        }
        if le16(bytes, 20)? != 0x0100 {
            return Err(ServiceError::unsupported(
                "Unsupported native preset container version",
            ));
        }
        let mode = le16(bytes, 22)?;
        let count = match mode {
            1 => 1,
            2 => 2,
            _ => {
                return Err(ServiceError::unsupported(format!(
                    "Unsupported native preset mode {mode}; original bytes will be retained"
                )));
            }
        };
        let expected = u32::from_be_bytes(bytes[bytes.len() - 4..].try_into().expect("four bytes"));
        if checksum(&bytes[..bytes.len() - 4]) != expected {
            return Err(invalid("Native preset additive checksum mismatch"));
        }
        let directory_end = HEADER + count * 8;
        let mut ranges = Vec::new();
        for index in 0..count {
            let position = HEADER + index * 8;
            let offset = le32(bytes, position)? as usize;
            let length = le32(bytes, position + 4)? as usize;
            let end = offset
                .checked_add(8)
                .and_then(|n| n.checked_add(length))
                .ok_or_else(|| invalid("Native chunk length overflow"))?;
            if offset < directory_end || end > bytes.len() - 4 || length == 0 {
                return Err(invalid(
                    "Native directory points outside its bounded chunk region",
                ));
            }
            if le32(bytes, offset + 4)? as usize != length {
                return Err(invalid("Native directory and chunk lengths disagree"));
            }
            if ranges
                .iter()
                .any(|old: &Range<usize>| offset < old.end && old.start < end)
            {
                return Err(invalid("Native directory chunks overlap"));
            }
            ranges.push(offset..end);
        }
        let preset_chunk = &ranges[count - 1];
        if &bytes[preset_chunk.start..preset_chunk.start + 4] != b"MRAP" {
            return Err(ServiceError::unsupported("Native preset chunk is not MRAP"));
        }
        if preset_chunk.end != bytes.len() - 4 || preset_chunk.len() < 12 {
            return Err(invalid(
                "Native MRAP must finish immediately before the outer checksum",
            ));
        }
        let preset = preset_chunk.start + 8..preset_chunk.end - 4;
        if preset.len() > limits.maximum_preset_bytes {
            return Err(invalid("Native preset record stream exceeds its bound"));
        }
        let size_word = le32(bytes, preset.end)? as usize;
        if size_word != bytes.len() - 8 {
            return Err(invalid(
                "Native final little-endian file size word disagrees",
            ));
        }
        let raw = Arc::new(
            RawTransfer::from_payload(0x12, bytes[preset.clone()].to_vec()).map_err(invalid)?,
        );
        validate_preset(&raw, limits.maximum_records)?;
        let embedded_ir = if mode == 2 {
            let ir = &ranges[0];
            if &bytes[ir.start..ir.start + 4] != b"RISU" {
                return Err(ServiceError::unsupported(
                    "Native mode 2 asset chunk is not RISU",
                ));
            }
            Some(ir.start + 8..ir.end)
        } else {
            None
        };
        let snapshot = PresetSnapshot::from_raw(
            raw.clone(),
            SnapshotSource::Capture {
                capture_id: format!("native-prst:{}", digest(bytes)),
            },
            None,
            Revision(0),
        );
        let mut notes = vec![
            "GP-200 TSRP/2-PG version 0x0100 layout recovered from Windows editor 1.8.1; external mode 1 fixture agrees. Official editor import and sound equivalence have not been tested.".into(),
            "Ordered records, float bits, opaque header bytes and source file are retained exactly. Local library tags, notes, favorites, author and style remain in .gp200bundle; they are not native preset fields.".into(),
        ];
        if embedded_ir.is_some() {
            notes.push("Mode 2 RISU bytes are retained as an opaque embedded IR carrier. The recovered 32-byte layout contains an IR metadata reference, not IR audio; unknown layouts remain opaque. Original-editor mode 2 compatibility and hardware restore are unverified.".into());
            if embedded_ir.as_ref().is_some_and(|range| {
                matches!(
                    inspect_native_ir_reference(&bytes[range.clone()]),
                    NativeIrReferenceInspection::MetadataReference(_)
                )
            }) {
                notes.push("IR reference inspection reports CRC32 and additive32 trailer candidates separately. The recovered producer and multi-import reader use different checks; a match does not establish native mode 2 compatibility or identical IR coefficients.".into());
            }
        }
        if raw.records().len() != 33 || preset.len() != 1168 {
            notes.push("Extended record stream retained; the original 1.8.1 serializer emits 33 records/1168 bytes.".into());
        }
        if raw
            .records()
            .iter()
            .any(|record| record.command == 2 && record.body[16..32].iter().any(|b| *b >= 128))
        {
            notes.push("Name contains non-ASCII bytes; their original character encoding is unresolved and bytes are preserved.".into());
        }
        let unresolved_assets = unresolved(&snapshot);
        if !unresolved_assets.is_empty() {
            notes.push("User asset slot references do not establish that the same IR/SnapTone exists on another pedal.".into());
            notes.push("User IR/SnapTone references require the separate audio/model source files and asset installation. Preset references and metadata are not a complete asset backup; missing IR files may leave the pedal muted.".into());
        }
        Ok(Self {
            original: bytes.into(),
            raw,
            mode,
            preset,
            embedded_ir,
            report: ConversionReport {
                level: CompatibilityLevel::Partial,
                notes,
                unresolved_assets,
            },
        })
    }
    pub fn original_bytes(&self) -> &[u8] {
        &self.original
    }
    pub fn raw(&self) -> &Arc<RawTransfer> {
        &self.raw
    }
    pub fn mode(&self) -> u16 {
        self.mode
    }
    /// Header field semantics beyond signature/version/mode remain opaque.
    pub fn header(&self) -> &[u8] {
        &self.original[..HEADER]
    }
    /// Legacy accessor for the exact opaque RISU payload. Its byte count does
    /// not establish recovered IR audio; use `ir_reference_inspection` for a
    /// descriptive view of the observed metadata carrier.
    pub fn embedded_ir_bytes(&self) -> Option<&[u8]> {
        self.embedded_ir.as_ref().map(|r| &self.original[r.clone()])
    }
    /// Describe a present RISU payload without changing native parse acceptance
    /// or granting mode 2 interoperability, content identity or write authority.
    pub fn ir_reference_inspection(&self) -> Option<NativeIrReferenceInspection> {
        self.embedded_ir_bytes().map(inspect_native_ir_reference)
    }
    pub fn report(&self) -> &ConversionReport {
        &self.report
    }
    /// Apply same-sized record edits while preserving every surrounding byte.
    /// An unchanged preset returns the exact input, including reserved bytes.
    pub fn preserve_with_raw(&self, raw: &RawTransfer) -> Result<Vec<u8>, ServiceError> {
        let raw = file_raw(raw)?;
        validate_preset(&raw, NativeFileLimits::default().maximum_records)?;
        if raw.payload().len() != self.preset.len()
            || raw
                .records()
                .iter()
                .map(|r| (r.command, r.body.len()))
                .collect::<Vec<_>>()
                != self
                    .raw
                    .records()
                    .iter()
                    .map(|r| (r.command, r.body.len()))
                    .collect::<Vec<_>>()
        {
            return Err(ServiceError::unsupported(
                "Preserving a native container requires the same ordered record commands and lengths; choose preset-only export to rebuild it",
            ));
        }
        if raw.payload() == self.raw.payload() {
            return Ok(self.original.to_vec());
        }
        let mut bytes = self.original.to_vec();
        bytes[self.preset.clone()].copy_from_slice(raw.payload());
        let end = bytes.len() - 4;
        let sum = checksum(&bytes[..end]);
        bytes[end..].copy_from_slice(&sum.to_be_bytes());
        Ok(bytes)
    }
    /// Rebuild the original preset-only file layout. Reserved fields default to
    /// zero when no original header exists; this is reported as partial.
    pub fn preset_only(
        snapshot: &PresetSnapshot,
        header: Option<&[u8]>,
    ) -> Result<Vec<u8>, ServiceError> {
        require_encoded_projection(snapshot)?;
        if snapshot.device_model != DeviceModel::Gp200 {
            return Err(ServiceError::unsupported(
                "Native GP-200 preset export does not establish LT/JR compatibility",
            ));
        }
        let raw = file_raw(&snapshot.raw)?;
        validate_preset(&raw, NativeFileLimits::default().maximum_records)?;
        let mut bytes = vec![0; HEADER];
        if let Some(header) = header {
            if header.len() != HEADER {
                return Err(invalid("Native header must contain 32 bytes"));
            }
            bytes.copy_from_slice(header);
        }
        bytes[..4].copy_from_slice(b"TSRP");
        bytes[16..20].copy_from_slice(b"2-PG");
        bytes[20..22].copy_from_slice(&0x0100u16.to_le_bytes());
        bytes[22..24].copy_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&40u32.to_le_bytes());
        let size = u32::try_from(raw.payload().len() + 4).map_err(invalid)?;
        bytes.extend_from_slice(&size.to_le_bytes());
        bytes.extend_from_slice(b"MRAP");
        bytes.extend_from_slice(&size.to_le_bytes());
        bytes.extend_from_slice(raw.payload());
        let size_word = u32::try_from(bytes.len()).map_err(invalid)?;
        bytes.extend_from_slice(&size_word.to_le_bytes());
        let sum = checksum(&bytes);
        bytes.extend_from_slice(&sum.to_be_bytes());
        Ok(bytes)
    }
}

fn le16(bytes: &[u8], offset: usize) -> Result<u16, ServiceError> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .ok_or_else(|| invalid("Truncated native u16"))?
            .try_into()
            .expect("two bytes"),
    ))
}
fn le32(bytes: &[u8], offset: usize) -> Result<u32, ServiceError> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or_else(|| invalid("Truncated native u32"))?
            .try_into()
            .expect("four bytes"),
    ))
}
fn checksum(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(0u32, |sum, byte| sum.wrapping_add(u32::from(*byte)))
}

/// CRC of the bare serializer blob, not the `.prst` file checksum.
pub fn native_blob_crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb8_8320 } else { 0 };
        }
    }
    !crc
}
fn file_raw(raw: &RawTransfer) -> Result<RawTransfer, ServiceError> {
    // Only an established leading transport prefix is removable. A context or
    // query record later in the stream is malformed input, not disposable data.
    let mut start = 0;
    if let Some(first) = raw.records().first()
        && first.command == 0
        && first.body.len() == 4
    {
        start = 1;
        if let Some(second) = raw.records().get(start)
            && second.command == 1
            && second.body.len() == 4
        {
            start += 1;
        }
    }
    let records = &raw.records()[start..];
    let raw = RawTransfer::from_records(0x12, records).map_err(invalid)?;
    if raw.payload().len() > NativeFileLimits::default().maximum_preset_bytes {
        return Err(invalid("Native record stream exceeds its bound"));
    }
    Ok(raw)
}
fn validate_preset(raw: &RawTransfer, maximum_records: usize) -> Result<(), ServiceError> {
    if raw.records().len() > maximum_records {
        return Err(invalid("Native record count exceeds its bound"));
    }
    let mut metadata = 0;
    let mut chain = 0;
    let mut modules = BTreeSet::new();
    for record in raw.records() {
        match record.command {
            0 | 1 => {
                return Err(invalid(
                    "Native file contains a transport context/query record",
                ));
            }
            2 => {
                metadata += 1;
                if record.body.len() != 88 {
                    return Err(invalid("Native metadata record must contain 88 bytes"));
                }
            }
            8 => {
                chain += 1;
                if record.body.len() != 16
                    || record.body[4..15].iter().copied().collect::<BTreeSet<_>>()
                        != (0..11).collect()
                {
                    return Err(invalid(
                        "Native chain must contain each of the 11 modules once",
                    ));
                }
            }
            0x14 => {
                if record.body.len() != 68
                    || record.body[0] >= 11
                    || le16(&record.body, 2)? != 15
                    || !modules.insert(record.body[0])
                {
                    return Err(invalid(
                        "Native module records require unique IDs 0..10 and 15 float slots",
                    ));
                }
            }
            0xc if record.body.len() != 12 => {
                return Err(invalid("Native expression record must contain 12 bytes"));
            }
            0x10 if record.body.len() != 4 => {
                return Err(invalid("Native quick-control record must contain 4 bytes"));
            }
            0xf if record.body.len() != 8 => {
                return Err(invalid("Native footswitch record must contain 8 bytes"));
            }
            _ => {}
        }
    }
    if metadata != 1 || chain != 1 || modules.len() != 11 {
        return Err(invalid(
            "Native preset requires one metadata record, one chain and all 11 modules",
        ));
    }
    Ok(())
}
fn unresolved(snapshot: &PresetSnapshot) -> Vec<AssetRef> {
    snapshot
        .asset_refs
        .iter()
        .filter(|r| !matches!(r, AssetRef::Factory { .. }))
        .cloned()
        .collect()
}

/// A local view must not advertise edits that the exported record bytes omit.
/// Comparing a fresh raw projection also works when a later verified codec
/// starts projecting assignments directly from those same raw records.
pub(crate) fn require_encoded_projection(snapshot: &PresetSnapshot) -> Result<(), ServiceError> {
    let projected = PresetSnapshot::from_raw(
        snapshot.raw.clone(),
        snapshot.source.clone(),
        snapshot.target,
        snapshot.revision,
    );
    let raw_view = &projected.view;
    let view = &snapshot.view;
    let mut fields = Vec::new();
    if view.name.value() != raw_view.name.value()
        || view
            .metadata
            .name
            .as_ref()
            .is_some_and(|name| Some(name) != raw_view.name.value())
    {
        fields.push("preset name");
    }
    if view.tempo.value() != raw_view.tempo.value()
        || view
            .metadata
            .tempo
            .as_ref()
            .is_some_and(|tempo| Some(tempo) != raw_view.tempo.value())
    {
        fields.push("tempo");
    }
    if view.level.value() != raw_view.level.value()
        || view
            .metadata
            .level
            .as_ref()
            .is_some_and(|level| Some(level) != raw_view.level.value())
    {
        fields.push("preset level");
    }
    if view.chain.value() != raw_view.chain.value() {
        fields.push("signal chain order");
    }
    if view.modules.len() != raw_view.modules.len()
        || view.modules.iter().any(|module| {
            raw_view.module(module.module).is_none_or(|raw| {
                raw.model != module.model
                    || raw.state_raw != module.state_raw
                    || raw.parameters != module.parameters
            })
        })
    {
        fields.push("module settings");
    }
    if view.expressions != raw_view.expressions {
        fields.push("expression assignments");
    }
    if view.footswitches != raw_view.footswitches {
        fields.push("footswitch assignments");
    }
    if view.quick_controls != raw_view.quick_controls {
        fields.push("quick control assignments");
    }
    if view.patch_assignments != gp200_domain::PatchAssignments::default()
        && view.patch_assignments != raw_view.patch_assignments
    {
        fields.push("encoded patch assignments and FX loop positions");
    }
    if view.metadata.pan != raw_view.metadata.pan {
        fields.push("pan");
    }
    // Older local snapshots omitted these projections while retaining the raw
    // records. An absent projection is compatible; an explicit disagreement is
    // an unencoded edit and cannot be silently exported.
    if view
        .metadata
        .native_pan
        .is_some_and(|v| Some(v) != raw_view.metadata.native_pan)
        || view
            .metadata
            .native_style
            .is_some_and(|v| Some(v) != raw_view.metadata.native_style)
        || view
            .metadata
            .fx_send_level
            .is_some_and(|v| Some(v) != raw_view.metadata.fx_send_level)
        || view
            .metadata
            .fx_return_level
            .is_some_and(|v| Some(v) != raw_view.metadata.fx_return_level)
        || view
            .metadata
            .fx_loop_mode
            .is_some_and(|v| Some(v) != raw_view.metadata.fx_loop_mode)
    {
        fields.push("native Pan/style and FX loop settings");
    }
    if !fields.is_empty() {
        return Err(ServiceError::unsupported(format!(
            "Native export cannot encode local draft fields: {}. Keep these edits in .gp200bundle until their raw record codec is available.",
            fields.join(", ")
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedNativeSource {
    pub source: OriginalSource,
    pub reason: String,
}
impl LibraryStore {
    /// Unsupported/malformed regular files are retained as inert content-addressed
    /// bytes, without creating a playable preset or overwriting another entry.
    pub fn rejected_native_sources(&self) -> Result<Vec<RejectedNativeSource>, ServiceError> {
        self.manifest
            .extensions
            .get("native_rejected_sources")
            .map_or(Ok(Vec::new()), |v| {
                serde_json::from_value(v.clone()).map_err(invalid)
            })
    }
    fn retain_rejected_native(
        &mut self,
        name: String,
        bytes: &[u8],
        reason: String,
    ) -> Result<(), ServiceError> {
        let mut sources = self.rejected_native_sources()?;
        let hash = self.publish_blob(bytes)?;
        if !sources.iter().any(|s| s.source.hash == hash) {
            if sources.len() >= 1024 {
                return Err(invalid("Rejected native source retention limit reached"));
            }
            sources.push(RejectedNativeSource {
                source: OriginalSource {
                    name,
                    format: "unparsed-valeton-prst".into(),
                    hash,
                    bytes: bytes.len() as u64,
                },
                reason,
            });
        }
        let mut next = self.manifest.clone();
        next.extensions.insert(
            "native_rejected_sources".into(),
            serde_json::to_value(sources).map_err(invalid)?,
        );
        self.commit(next)
    }
    pub fn import_native(
        &mut self,
        request: &ImportRequest,
    ) -> Result<(LibraryVersionId, ConversionReport), ServiceError> {
        self.import_native_cancellable(request, &|| false)
    }
    pub(crate) fn import_native_cancellable(
        &mut self,
        request: &ImportRequest,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(LibraryVersionId, ConversionReport), ServiceError> {
        if cancelled() {
            return Err(ServiceError::new(
                ErrorKind::Cancelled,
                "Native import cancelled",
            ));
        }
        let bytes = read_bounded(
            &request.path,
            NativeFileLimits::default().maximum_file_bytes as u64,
        )?;
        let name = request
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let file = match NativeFile::parse(&bytes) {
            Ok(file) => file,
            Err(mut error) => {
                self.retain_rejected_native(name, &bytes, error.message.clone())?;
                error
                    .message
                    .push_str("; original bytes retained in local library source storage");
                return Err(error);
            }
        };
        if cancelled() {
            return Err(ServiceError::new(
                ErrorKind::Cancelled,
                "Native import cancelled before publication",
            ));
        }
        let mut snapshot = PresetSnapshot::from_raw(
            file.raw.clone(),
            SnapshotSource::Capture {
                capture_id: format!("native-prst:{}", digest(&bytes)),
            },
            None,
            Revision(0),
        );
        snapshot
            .evidence
            .push(EvidenceRef::new(RESEARCH, EvidenceLevel::StaticRecovered));
        let metadata = LibraryMetadata {
            name: snapshot.view.display_name().into(),
            ..LibraryMetadata::default()
        };
        let version = self.store_with_originals(
            Arc::new(snapshot),
            metadata,
            None,
            &[(name, FORMAT.into(), bytes)],
        )?;
        Ok((version, file.report.clone()))
    }
    pub fn export_native(
        &self,
        request: &NativeExportRequest,
    ) -> Result<ConversionReport, ServiceError> {
        self.export_native_cancellable(request, &|| false)
    }
    pub(crate) fn export_native_cancellable(
        &self,
        request: &NativeExportRequest,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ConversionReport, ServiceError> {
        if cancelled() {
            return Err(ServiceError::new(
                ErrorKind::Cancelled,
                "Native export cancelled",
            ));
        }
        let version = self.load_version(&request.version)?;
        require_encoded_projection(&version.snapshot)?;
        let source = self
            .original_sources(&request.version)?
            .iter()
            .rev()
            .find(|source| source.format == FORMAT);
        let original = source
            .map(|source| {
                self.original_bytes(source)
                    .and_then(|b| NativeFile::parse(&b))
            })
            .transpose()?;
        let bytes = match request.mode {
            NativeExportMode::PreserveOriginal => original.as_ref().ok_or_else(|| ServiceError::unsupported("This revision has no retained parsed native preset; choose preset-only export"))?.preserve_with_raw(&version.snapshot.raw)?,
            NativeExportMode::PresetOnly => NativeFile::preset_only(&version.snapshot, original.as_ref().map(NativeFile::header))?,
        };
        if cancelled() {
            return Err(ServiceError::new(
                ErrorKind::Cancelled,
                "Native export cancelled before publication",
            ));
        }
        let parent = request
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if !parent.is_dir() {
            return Err(invalid("Native export directory does not exist"));
        }
        atomic_write(parent, &request.path, &bytes)?;
        let mut report = NativeFile::parse(&bytes)?.report.clone();
        let metadata = &version.snapshot.view.metadata;
        let omitted = [
            ("author", metadata.author.is_some()),
            ("style", metadata.style.is_some()),
            ("notes", metadata.notes.is_some()),
        ]
        .into_iter()
        .filter_map(|(field, present)| present.then_some(field))
        .collect::<Vec<_>>();
        if !omitted.is_empty() {
            report.notes.push(format!("Local metadata projection fields omitted from native bytes: {}; preserved in .gp200bundle.", omitted.join(", ")));
        }
        match request.mode {
            NativeExportMode::PreserveOriginal => report.notes.push("Export preserves the source container and opaque embedded bytes; supported record changes are applied without changing surrounding data. Retained references do not supply or restore IR/SnapTone contents.".into()),
            NativeExportMode::PresetOnly => {
                report.notes.push("Preset-only export includes no IR/SnapTone asset bytes; dependent slot references require separate asset installation.".into());
                if original.is_none() { report.notes.push("No original native header was available; opaque header fields were initialized to zero. Official import of this generated header is untested.".into()); }
            }
        }
        Ok(report)
    }
}
