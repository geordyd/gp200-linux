//! Local IR history adapter. All operations are local and run on LibraryWorker.
use crate::{
    ContentHash, IrAttemptLedger, IrJournalIssue, IrLocalAttemptState, IrRawCheckpointReceipt,
    IrRetainedAttempt, LibraryStore,
    store::{invalid, io, nonce, read_bounded, reject_symlink},
};
use gp200_domain::{ErrorKind, IrAttemptId, ServiceError};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

pub const IR_HISTORY_PAGE_SIZE: usize = 8;
const MAX_EXPORT_BYTES: usize = 72 * 1024 * 1024;
const MAX_EXPORT_FILES: usize = 280;

#[derive(Debug, Clone, Default)]
pub struct IrHistoryPageRequest {
    pub offset: usize,
    /// Descriptive roster hash, never a live session or append cursor.
    pub roster_sha256: Option<ContentHash>,
}
#[derive(Debug, Clone)]
pub struct IrHistoryRow {
    pub attempt: IrAttemptId,
    pub started_at_ms: Option<u64>,
    pub slot: Option<u16>,
    pub name: Option<String>,
    pub source_sha256: Option<String>,
    pub codec_revision: Option<String>,
    pub local_state: IrLocalAttemptState,
    pub error: Option<String>,
}
#[derive(Debug, Clone)]
pub struct IrHistoryPage {
    pub offset: usize,
    pub total: usize,
    pub roster_sha256: ContentHash,
    pub rows: Vec<IrHistoryRow>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IrEvidenceFilePin {
    pub path: String,
    pub bytes: u64,
    pub sha256: ContentHash,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum IrHistoryExportKind {
    CheckedEvidence,
    RawDiagnostics,
}
#[derive(Debug, Clone)]
pub struct IrHistoryDetail {
    pub attempt: IrAttemptId,
    pub checked: Option<Arc<IrRetainedAttempt>>,
    pub load_error: Option<String>,
    pub journal: IrEvidenceFilePin,
    pub checkpoints: Vec<IrEvidenceFilePin>,
    pub checked_prefix_sha256: Option<ContentHash>,
    pub observed_files: Vec<IrEvidenceFilePin>,
    /// Binds the bytes and mode displayed before the native save dialog.
    pub snapshot_sha256: ContentHash,
    pub export_kind: IrHistoryExportKind,
}
#[derive(Debug, Clone)]
pub struct IrHistoryExportRequest {
    pub attempt: IrAttemptId,
    pub observed_snapshot_sha256: ContentHash,
    pub kind: IrHistoryExportKind,
    pub destination: PathBuf,
}
#[derive(Debug, Clone)]
pub struct IrHistoryExportReceipt {
    pub destination: PathBuf,
    pub attempt: IrAttemptId,
    pub kind: IrHistoryExportKind,
    pub archive_sha256: ContentHash,
    pub archive_bytes: u64,
    pub source_snapshot_sha256: ContentHash,
    pub file_count: usize,
}
#[derive(Serialize)]
struct DerivedManifest<'a> {
    format: &'static str,
    schema: u16,
    attempt: &'a IrAttemptId,
    export_kind: IrHistoryExportKind,
    content_verified: bool,
    source_snapshot_sha256: &'a ContentHash,
    load_error: &'a Option<String>,
    files: &'a [IrEvidenceFilePin],
    notes: [&'static str; 4],
}
#[derive(Serialize)]
struct DerivedLocalChecks<'a> {
    schema: u16,
    content_verified: bool,
    local_state: &'static str,
    accepted_record_count: usize,
    checked_prefix_bytes: usize,
    checked_prefix_sha256: &'a Option<ContentHash>,
    last_accepted_record_sha256: &'a ContentHash,
    confirmed_local_fsync_boundary: &'a Option<crate::IrLocalFsyncBoundary>,
    issues: Vec<String>,
}
struct Collected {
    detail: IrHistoryDetail,
    files: BTreeMap<String, Vec<u8>>,
}

fn check_cancel(cancelled: &dyn Fn() -> bool) -> Result<(), ServiceError> {
    if cancelled() {
        Err(ServiceError::new(
            ErrorKind::Cancelled,
            "Local IR evidence operation cancelled; retained history is unchanged",
        ))
    } else {
        Ok(())
    }
}
fn file_pin(path: &str, bytes: &[u8]) -> IrEvidenceFilePin {
    IrEvidenceFilePin {
        path: path.into(),
        bytes: bytes.len() as u64,
        sha256: ContentHash::of(bytes),
    }
}
fn named_material(role: crate::IrMaterialRole) -> &'static str {
    use crate::IrMaterialRole::*;
    match role {
        SourceWav => "original/materials/source.wav",
        Coefficients => "original/materials/coefficients.bin",
        IntendedTlv => "original/materials/intended-tlv.bin",
        IntendedWire => "original/materials/intended-wire.bin",
        BaselinePayload => "original/materials/baseline-payload.bin",
        IdentityPayload => "original/materials/identity-payload.bin",
        SessionJson => "original/materials/session.json",
        ReviewPreimage => "original/materials/review-preimage.bin",
    }
}
fn name(raw: &[u8; 16]) -> String {
    let end = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).into_owned()
}

pub fn ir_history_page(
    store: &mut LibraryStore,
    request: IrHistoryPageRequest,
    cancelled: &dyn Fn() -> bool,
) -> Result<IrHistoryPage, ServiceError> {
    check_cancel(cancelled)?;
    let ledger = IrAttemptLedger::open(store)?;
    let ids = ledger.attempt_ids()?;
    let roster_sha256 = ContentHash::of(&serde_json::to_vec(&ids).map_err(invalid)?);
    if !request.offset.is_multiple_of(IR_HISTORY_PAGE_SIZE)
        || request.offset > ids.len()
        || (request.offset > 0 && request.roster_sha256.is_none())
    {
        return Err(invalid("IR history page is outside the bounded roster"));
    }
    if request
        .roster_sha256
        .as_ref()
        .is_some_and(|hash| hash != &roster_sha256)
    {
        return Err(ServiceError::new(
            ErrorKind::Conflict,
            "IR history changed. Refresh its first page before continuing",
        ));
    }
    let mut rows = Vec::new();
    for id in ids.iter().skip(request.offset).take(IR_HISTORY_PAGE_SIZE) {
        check_cancel(cancelled)?;
        let mut row = IrHistoryRow {
            attempt: id.clone(),
            started_at_ms: None,
            slot: None,
            name: None,
            source_sha256: None,
            codec_revision: None,
            local_state: IrLocalAttemptState::Indeterminate,
            error: None,
        };
        match ledger.load(id) {
            Ok(Some(found)) => {
                row.started_at_ms = Some(found.recorded.started_at_ms);
                row.slot = Some(found.recorded.binding.target.index() + 1);
                row.name = Some(name(&found.recorded.binding.name_raw));
                row.source_sha256 = Some(found.recorded.binding.source_sha256.clone());
                row.codec_revision = Some(found.recorded.binding.codec_revision.clone());
                row.local_state = found.local_state;
            }
            Ok(None) => {
                row.error = Some("No journal was retained for this reserved attempt".into())
            }
            Err(error) => row.error = Some(error.message),
        }
        rows.push(row);
    }
    Ok(IrHistoryPage {
        offset: request.offset,
        total: ids.len(),
        roster_sha256,
        rows,
    })
}

fn collect(
    store: &mut LibraryStore,
    id: &IrAttemptId,
    required_kind: Option<IrHistoryExportKind>,
    cancelled: &dyn Fn() -> bool,
) -> Result<Collected, ServiceError> {
    check_cancel(cancelled)?;
    let ledger = IrAttemptLedger::open(store)?;
    let (checked, load_error) = match ledger.load(id) {
        Ok(Some(value)) => (Some(Arc::new(value)), None),
        Ok(None) => (
            None,
            Some("No journal was retained for this reserved attempt".into()),
        ),
        Err(error) => (None, Some(error.message)),
    };
    let kind = required_kind.unwrap_or(if checked.is_some() {
        IrHistoryExportKind::CheckedEvidence
    } else {
        IrHistoryExportKind::RawDiagnostics
    });
    if kind == IrHistoryExportKind::CheckedEvidence && checked.is_none() {
        return Err(ServiceError::new(
            ErrorKind::Conflict,
            "Checked IR evidence is no longer readable. Open its diagnostics again",
        ));
    }
    let raw = ledger.export_raw_journal(id)?;
    let journal = file_pin("original/journal.bin", &raw);
    let mut files = BTreeMap::from([(journal.path.clone(), raw)]);
    let raw_checkpoints: Vec<IrRawCheckpointReceipt> = ledger.raw_checkpoint_receipts(id)?;
    let mut checkpoints = Vec::new();
    for checkpoint in raw_checkpoints {
        check_cancel(cancelled)?;
        let pin = file_pin(
            &format!("original/checkpoints/{:03}.json", checkpoint.sequence),
            &checkpoint.bytes,
        );
        if pin.sha256 != checkpoint.sha256 {
            return Err(invalid("IR checkpoint bytes changed"));
        }
        files.insert(pin.path.clone(), checkpoint.bytes);
        checkpoints.push(pin);
    }
    let mut checked_prefix_sha256 = None;
    if kind == IrHistoryExportKind::CheckedEvidence {
        let found = checked.as_ref().expect("checked kind validated");
        let raw = files.get(&journal.path).expect("journal inserted");
        if found.journal_sha256 != journal.sha256 || found.valid_prefix_bytes > raw.len() {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "IR journal changed during its checked read",
            ));
        }
        checked_prefix_sha256 = Some(ContentHash::of(&raw[..found.valid_prefix_bytes]));
        for material in &found.materials {
            check_cancel(cancelled)?;
            let bytes = ledger.material_bytes(material)?;
            let path = named_material(material.role).to_owned();
            if ContentHash::of(&bytes) != material.sha256
                || bytes.len() as u64 != material.bytes
                || files.insert(path, bytes).is_some()
            {
                return Err(invalid("IR material is changed or duplicate"));
            }
        }
        if let Some(imported) = &found.original_import {
            let bytes = ledger.original_import_bytes(imported)?;
            if ContentHash::of(&bytes) != imported.sha256 || bytes.len() as u64 != imported.bytes {
                return Err(invalid("Original imported IR bytes changed"));
            }
            files.insert("original/imported-receipt.json".into(), bytes);
        }
        // These serialized views are explicitly derived and never replace raw bytes.
        files.insert(
            "derived/accepted-recorded-prefix.json".into(),
            serde_json::to_vec_pretty(&found.recorded).map_err(invalid)?,
        );
        files.insert(
            "derived/current-interpretation.json".into(),
            serde_json::to_vec_pretty(&found.reconstructed.to_dto()).map_err(invalid)?,
        );
        files.insert(
            "derived/local-checks.json".into(),
            serde_json::to_vec_pretty(&DerivedLocalChecks {
                schema: 1,
                content_verified: false,
                local_state: ir_local_state_label(found.local_state),
                accepted_record_count: found.record_count(),
                checked_prefix_bytes: found.valid_prefix_bytes,
                checked_prefix_sha256: &checked_prefix_sha256,
                last_accepted_record_sha256: &found.record_sha256,
                confirmed_local_fsync_boundary: &found.confirmed_local_fsync_boundary,
                issues: found.issues.iter().map(ir_journal_issue_label).collect(),
            })
            .map_err(invalid)?,
        );
    }
    let total = files
        .values()
        .try_fold(0usize, |sum, bytes| sum.checked_add(bytes.len()));
    if files.len() > MAX_EXPORT_FILES || total.is_none_or(|bytes| bytes > MAX_EXPORT_BYTES) {
        return Err(invalid("Local IR evidence export exceeds its bounded size"));
    }
    let observed_files: Vec<_> = files
        .iter()
        .map(|(path, bytes)| file_pin(path, bytes))
        .collect();
    let snapshot_sha256 = ContentHash::of(
        &serde_json::to_vec(&(
            1u16,
            id,
            kind,
            &load_error,
            &checked_prefix_sha256,
            &observed_files,
        ))
        .map_err(invalid)?,
    );
    Ok(Collected {
        detail: IrHistoryDetail {
            attempt: id.clone(),
            checked,
            load_error,
            journal,
            checkpoints,
            checked_prefix_sha256,
            observed_files,
            snapshot_sha256,
            export_kind: kind,
        },
        files,
    })
}
pub fn ir_history_detail(
    store: &mut LibraryStore,
    id: &IrAttemptId,
    cancelled: &dyn Fn() -> bool,
) -> Result<Arc<IrHistoryDetail>, ServiceError> {
    Ok(Arc::new(collect(store, id, None, cancelled)?.detail))
}

/// Public worker adapter. Does not mutate history or return any writer/device authority.
pub fn export_ir_history(
    store: &mut LibraryStore,
    request: IrHistoryExportRequest,
    cancelled: &dyn Fn() -> bool,
) -> Result<IrHistoryExportReceipt, ServiceError> {
    export_with_before_publication(store, request, cancelled, &|| {})
}
fn export_with_before_publication(
    store: &mut LibraryStore,
    request: IrHistoryExportRequest,
    cancelled: &dyn Fn() -> bool,
    before_recheck: &dyn Fn(),
) -> Result<IrHistoryExportReceipt, ServiceError> {
    export_with_callbacks(store, request, cancelled, before_recheck, &|parent| {
        File::open(parent).and_then(|file| file.sync_all())
    })
}
fn export_with_callbacks(
    store: &mut LibraryStore,
    request: IrHistoryExportRequest,
    cancelled: &dyn Fn() -> bool,
    before_recheck: &dyn Fn(),
    sync_published_directory: &dyn Fn(&Path) -> std::io::Result<()>,
) -> Result<IrHistoryExportReceipt, ServiceError> {
    check_cancel(cancelled)?;
    if request
        .destination
        .extension()
        .is_none_or(|ext| ext != "zip")
    {
        return Err(invalid("Local IR evidence exports use .zip"));
    }
    let parent = request
        .destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    reject_symlink(parent)?;
    let parent = fs::canonicalize(parent).map_err(io)?;
    if !parent.is_dir() || parent.starts_with(fs::canonicalize(store.root()).map_err(io)?) {
        return Err(invalid(
            "Export IR evidence outside the local library folder",
        ));
    }
    let leaf = request
        .destination
        .file_name()
        .ok_or_else(|| invalid("IR export needs a file name"))?;
    let destination = parent.join(leaf);
    reject_symlink(&destination)?;
    if destination.exists() {
        return Err(ServiceError::new(
            ErrorKind::Conflict,
            "IR evidence destination already exists. Choose a new file",
        ));
    }
    let collected = collect(store, &request.attempt, Some(request.kind), cancelled)?;
    if collected.detail.snapshot_sha256 != request.observed_snapshot_sha256 {
        return Err(ServiceError::new(
            ErrorKind::Conflict,
            "IR evidence changed after opening the save dialog. Open its details again",
        ));
    }
    let metadata = DerivedManifest {
        format: "gp200-studio.local-ir-evidence",
        schema: 1,
        attempt: &request.attempt,
        export_kind: request.kind,
        content_verified: false,
        source_snapshot_sha256: &collected.detail.snapshot_sha256,
        load_error: &collected.detail.load_error,
        files: &collected.detail.observed_files,
        notes: [
            "Original bytes and derived views are separate.",
            "The old IR audio was not backed up.",
            "Names, checksums, status replies and local fsync receipts do not verify pedal audio.",
            "This archive cannot resume, retry or send a device job.",
        ],
    };
    let metadata_bytes = serde_json::to_vec_pretty(&metadata).map_err(invalid)?;
    let mut exported: BTreeMap<String, &[u8]> = collected
        .files
        .iter()
        .map(|(path, bytes)| (path.clone(), bytes.as_slice()))
        .collect();
    exported.insert("derived/export-manifest.json".into(), &metadata_bytes);
    if exported.len() > MAX_EXPORT_FILES + 1
        || exported
            .values()
            .try_fold(0usize, |sum, bytes| sum.checked_add(bytes.len()))
            .is_none_or(|bytes| bytes > MAX_EXPORT_BYTES)
    {
        return Err(invalid(
            "IR export metadata exceeds the bounded archive size",
        ));
    }
    let temporary = parent.join(format!(".ir-evidence-{}.tmp", nonce()?));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut writer = ZipWriter::new(options.open(&temporary).map_err(io)?);
        let entry_options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(0o600);
        for (path, bytes) in &exported {
            check_cancel(cancelled)?;
            writer.start_file(path, entry_options).map_err(invalid)?;
            writer.write_all(bytes).map_err(io)?;
        }
        let file = writer.finish().map_err(invalid)?;
        file.sync_all().map_err(io)?;
        if file.metadata().map_err(io)?.len() > (MAX_EXPORT_BYTES + 1024 * 1024) as u64 {
            return Err(invalid("IR evidence ZIP exceeds its compressed bound"));
        }
        let archive_raw = read_bounded(&temporary, (MAX_EXPORT_BYTES + 1024 * 1024) as u64)?;
        verify_written_zip(&archive_raw, &exported)?;
        let archive_sha256 = ContentHash::of(&archive_raw);
        before_recheck(); // Internal test fault seam only; not exposed by the worker.
        let latest = collect(store, &request.attempt, Some(request.kind), cancelled)?;
        if latest.detail.snapshot_sha256 != collected.detail.snapshot_sha256
            || latest.files != collected.files
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "IR evidence changed before publication. No archive was published",
            ));
        }
        check_cancel(cancelled)?;
        if ContentHash::of(&read_bounded(
            &temporary,
            (MAX_EXPORT_BYTES + 1024 * 1024) as u64,
        )?) != archive_sha256
        {
            return Err(invalid(
                "Temporary IR evidence ZIP changed before publication",
            ));
        }
        // Same-parent hard link is atomic create-new, so a raced existing file is never replaced.
        fs::hard_link(&temporary, &destination).map_err(io)?;
        fs::remove_file(&temporary)
            .and_then(|()| sync_published_directory(&parent))
            .map_err(|error| {
                ServiceError::new(
                    ErrorKind::Indeterminate,
                    format!("IR archive is visible but final local publication failed: {error}"),
                )
            })?;
        Ok(IrHistoryExportReceipt {
            destination,
            attempt: request.attempt.clone(),
            kind: request.kind,
            archive_sha256,
            archive_bytes: archive_raw.len() as u64,
            source_snapshot_sha256: collected.detail.snapshot_sha256,
            file_count: exported.len(),
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
fn verify_written_zip(raw: &[u8], expected: &BTreeMap<String, &[u8]>) -> Result<(), ServiceError> {
    let mut archive = ZipArchive::new(Cursor::new(raw)).map_err(invalid)?;
    if archive.len() != expected.len() {
        return Err(invalid("IR ZIP entry count differs"));
    }
    let mut names = BTreeSet::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(invalid)?;
        let name = entry.name().to_owned();
        let expected_bytes = expected
            .get(&name)
            .ok_or_else(|| invalid("Unexpected IR ZIP entry"))?;
        if entry.size() != expected_bytes.len() as u64 || entry.is_dir() {
            return Err(invalid("IR ZIP entry length differs"));
        }
        let mut bytes = Vec::new();
        entry
            .by_ref()
            .take(expected_bytes.len() as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        if bytes.as_slice() != *expected_bytes || !names.insert(name) {
            return Err(invalid("IR ZIP entry is duplicate or changed"));
        }
    }
    if names.len() != expected.len() {
        return Err(invalid("IR ZIP bytes differ"));
    }
    Ok(())
}

pub fn ir_local_state_label(state: IrLocalAttemptState) -> &'static str {
    match state {
        IrLocalAttemptState::LocalUnverified => "Local observations complete — audio unverified",
        IrLocalAttemptState::Indeterminate => "Indeterminate",
    }
}
pub fn ir_journal_issue_label(issue: &IrJournalIssue) -> String {
    match issue {
        IrJournalIssue::IncompleteTail { offset, bytes } => {
            format!("Incomplete journal tail: {bytes} bytes after byte {offset}")
        }
        IrJournalIssue::CorruptTail { offset, reason } => {
            format!("Corrupt journal tail after byte {offset}: {reason}")
        }
        IrJournalIssue::MissingReviewPreimage => {
            "Exact original review bytes were not retained".into()
        }
        IrJournalIssue::InvalidLocalFsyncCheckpoint { sequence, reason } => {
            format!("Invalid local fsync receipt {sequence}: {reason}")
        }
        IrJournalIssue::UnconfirmedLocalDurability {
            checked_prefix_bytes,
            confirmed_bytes,
        } => format!(
            "Local durability unconfirmed for bytes {confirmed_bytes}..{checked_prefix_bytes}"
        ),
    }
}

#[cfg(test)]
#[path = "ir_history_export_fault_tests.rs"]
mod tests;
