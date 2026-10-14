//! Independent immutable capture transcript; publication attests local bytes only.
use crate::{
    ContentHash, LibraryStore, PublishedFirmwareBackupReview,
    store::{self, invalid, io},
};
use gp200_domain::*;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub const FIRMWARE_CAPTURE_EVIDENCE_SCHEMA: &str = "gp200-capture-evidence-v1";
const SCHEMA: &str = FIRMWARE_CAPTURE_EVIDENCE_SCHEMA;
const POLICY: &str = "gp200-fw04-a-archival-exact-bytes-v1";
const RECORD_BYTES: usize = 8 * 1024 * 1024;
const LIMITATIONS: &str = "Caller-reported observations; host fences do not prove device causality or an atomic bank. All 20 installed IR audio contents unavailable. Restoration and migration unaccepted. No complete backup or flash authority.";
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareCaptureRecordInspection {
    pub schema: String,
    pub capture_id: String,
    pub worker_run: String,
    pub worker_sequence: u64,
    pub acquisition: Option<String>,
    pub kind: FirmwareCaptureReceiptKind,
    pub previous_sha256: String,
    pub preparation: Option<FirmwareCaptureReservation>,
    pub evidence: Option<FirmwareCaptureEvidence>,
    pub cancelled: bool,
}
/// Pure bounded canonical description of one record. It opens no files and issues no receipt.
/// Callers independently assemble the record from their retained original pending evidence.
/// Chain/order/reservation provenance requires the actual writer and independent index reread.
pub fn canonical_firmware_capture_record_bytes(
    record: &FirmwareCaptureRecordInspection,
) -> Result<Vec<u8>, ServiceError> {
    let bounded_label =
        |v: &str| !v.is_empty() && v.len() <= 256 && !v.chars().any(char::is_control);
    if record.schema != SCHEMA
        || record.worker_sequence == 0
        || !bounded_label(&record.worker_run)
        || !bounded_label(&record.capture_id)
        || record
            .acquisition
            .as_ref()
            .is_some_and(|v| !bounded_label(v))
        || !firmware_backup_hash_valid(&record.previous_sha256)
    {
        return Err(invalid(
            "Unsupported or unbounded capture record description",
        ));
    }
    if let Some(p) = &record.preparation {
        p.validate()?;
    }
    if let Some(e) = &record.evidence {
        e.validate()?;
    }
    let shape = match record.kind {
        FirmwareCaptureReceiptKind::Prepared => {
            record.preparation.is_some()
                && record.evidence.is_none()
                && record.acquisition.is_none()
                && !record.cancelled
        }
        FirmwareCaptureReceiptKind::Begun => {
            record.preparation.is_none()
                && record.acquisition.is_some()
                && !record.cancelled
                && record.evidence.as_ref().is_some_and(|e| {
                    e.complete
                        && e.read
                            == FirmwareCaptureRead::Identity(FirmwareCaptureBoundaryKind::Opening)
                })
        }
        FirmwareCaptureReceiptKind::Observation => {
            record.preparation.is_none()
                && record.acquisition.is_some()
                && !record.cancelled
                && record.evidence.as_ref().is_some_and(|e| {
                    e.complete && matches!(e.read, FirmwareCaptureRead::Object { .. })
                })
        }
        FirmwareCaptureReceiptKind::Boundary => {
            record.preparation.is_none()
                && record.acquisition.is_some()
                && !record.cancelled
                && record.evidence.as_ref().is_some_and(|e| {
                    e.complete
                        && matches!(
                            e.read,
                            FirmwareCaptureRead::Identity(
                                FirmwareCaptureBoundaryKind::Middle
                                    | FirmwareCaptureBoundaryKind::Final
                            ) | FirmwareCaptureRead::Boundary { .. }
                        )
                })
        }
        FirmwareCaptureReceiptKind::Failure => {
            record.preparation.is_none()
                && !record.cancelled
                && record.evidence.as_ref().is_some_and(|e| !e.complete)
        }
        FirmwareCaptureReceiptKind::Closed => {
            record.preparation.is_none() && record.evidence.is_none()
        }
        FirmwareCaptureReceiptKind::Published | FirmwareCaptureReceiptKind::Revalidated => false,
    };
    if !shape {
        return Err(invalid("Capture record kind/evidence shape differs"));
    }
    let bytes = serde_json::to_vec(record).map_err(invalid)?;
    if bytes.len() > RECORD_BYTES {
        return Err(invalid("Capture record serialized extent exceeds bounds"));
    }
    Ok(bytes)
}
/// Pure hash of exactly the bytes returned by canonical_firmware_capture_record_bytes.
/// Imported descriptions and matching hashes never recreate a current-run durability receipt.
pub fn firmware_capture_record_hash(
    record: &FirmwareCaptureRecordInspection,
) -> Result<ContentHash, ServiceError> {
    Ok(ContentHash::of(&canonical_firmware_capture_record_bytes(
        record,
    )?))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pin {
    name: String,
    bytes: u64,
    sha256: String,
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    schema: String,
    strict_archive_policy: String,
    capture_id: String,
    acquisition: Option<String>,
    records: Vec<Pin>,
    record_bytes: u64,
    archive_sha256: String,
    sidecar_sha256: String,
    closed: bool,
    cancelled: bool,
}
pub(crate) struct CaptureDraft {
    pub(crate) id: String,
    pub(crate) reservation: FirmwareCaptureReservation,
    directory: PathBuf,
    records: Vec<Pin>,
    bytes: usize,
    pub(crate) closed: bool,
    cancelled: bool,
    last_evidence_sequence: u64,
    last_callback: u64,
    last_worker_sequence: u64,
    acquisition: Option<String>,
    last_completed_ns: u64,
    pub(crate) failed: bool,
    final_seen: bool,
}
impl CaptureDraft {
    pub(crate) fn prepare(
        store: &LibraryStore,
        run: &str,
        sequence: u64,
        reservation: FirmwareCaptureReservation,
    ) -> Result<Self, ServiceError> {
        reservation.validate()?;
        store.require_firmware_publication_certain()?;
        let parent = store.root.join("firmware-capture-evidence");
        ensure_directory(&parent, false)?;
        sync_directory(&store.root)?;
        let id = store::nonce()?;
        let directory = parent.join(&id);
        ensure_directory(&directory, true)?;
        sync_directory(&parent)?;
        let prepared_ns = reservation.prepared_monotonic_ns;
        let mut result = Self {
            id,
            reservation,
            directory,
            records: vec![],
            bytes: 0,
            closed: false,
            cancelled: false,
            last_evidence_sequence: 0,
            last_callback: 0,
            last_worker_sequence: 0,
            acquisition: None,
            last_completed_ns: prepared_ns,
            failed: false,
            final_seen: false,
        };
        result.append(
            run,
            sequence,
            None,
            FirmwareCaptureReceiptKind::Prepared,
            None,
            false,
        )?;
        Ok(result)
    }
    pub(crate) fn append(
        &mut self,
        run: &str,
        sequence: u64,
        acquisition: Option<&str>,
        kind: FirmwareCaptureReceiptKind,
        evidence: Option<FirmwareCaptureEvidence>,
        cancelled: bool,
    ) -> Result<ContentHash, ServiceError> {
        if self.closed
            || self.records.len() >= FIRMWARE_CAPTURE_RECORD_LIMIT
            || sequence <= self.last_worker_sequence
            || self.failed && kind != FirmwareCaptureReceiptKind::Closed
        {
            return Err(invalid("Capture transcript closed, full or non-monotonic"));
        }
        if let Some(a) = &self.acquisition
            && acquisition != Some(a.as_str())
        {
            return Err(invalid("Capture acquisition differs"));
        }
        if let Some(e) = &evidence {
            e.validate_for_reservation(&self.reservation)?;
            if self.final_seen && e.complete {
                return Err(invalid("Completed capture evidence follows Final identity"));
            }
            if e.sequence <= self.last_evidence_sequence {
                return Err(invalid("Capture observation sequence was reused"));
            }
            if e.complete
                && e.offer
                    .is_none_or(|v| v.started_ns < self.last_completed_ns)
            {
                return Err(invalid(
                    "Capture monotonic interval precedes prior durable completion",
                ));
            }
            if e.complete
                && (e.before.allocated != self.last_callback
                    || e.before.processed != self.last_callback)
            {
                return Err(invalid(
                    "Capture callback frontier differs from prior durable record",
                ));
            }
        }
        let record = FirmwareCaptureRecordInspection {
            schema: SCHEMA.into(),
            capture_id: self.id.clone(),
            worker_run: run.into(),
            worker_sequence: sequence,
            acquisition: acquisition.map(str::to_owned),
            kind,
            previous_sha256: self.records.last().map_or_else(
                || ContentHash::of(&[]).as_str().to_owned(),
                |p| p.sha256.clone(),
            ),
            preparation: self.records.is_empty().then(|| self.reservation.clone()),
            evidence,
            cancelled,
        };
        let bytes = canonical_firmware_capture_record_bytes(&record)?;
        let cumulative = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(|| invalid("Capture size overflow"))?;
        if bytes.len() > RECORD_BYTES
            || cumulative > FIRMWARE_BACKUP_MATERIAL_LIMIT - FIRMWARE_CAPTURE_INDEX_LIMIT
        {
            return Err(invalid(
                "Capture transcript serialized material exceeds budget",
            ));
        }
        let name = format!("record-{:04}.json", self.records.len());
        let path = self.directory.join(&name);
        durable_file(&self.directory, &path, &bytes)?;
        let reopened = checked_read(&path, RECORD_BYTES)?;
        let decoded: FirmwareCaptureRecordInspection =
            serde_json::from_slice(&reopened).map_err(invalid)?;
        if reopened != bytes || decoded != record {
            return Err(invalid("Capture record reread differs"));
        }
        let hash = ContentHash::of(&reopened);
        self.records.push(Pin {
            name,
            bytes: bytes.len() as u64,
            sha256: hash.as_str().into(),
        });
        self.bytes = cumulative;
        self.last_worker_sequence = sequence;
        if let Some(a) = acquisition {
            self.acquisition = Some(a.into());
        }
        if let Some(e) = record.evidence {
            if e.read == FirmwareCaptureRead::Identity(FirmwareCaptureBoundaryKind::Final)
                && e.complete
            {
                self.final_seen = true;
            }
            self.last_evidence_sequence = e.sequence;
            self.last_callback = e.after.allocated;
            if let Some(t) = e.completed_ns {
                self.last_completed_ns = self.last_completed_ns.max(t);
            }
        }
        if kind == FirmwareCaptureReceiptKind::Failure {
            self.failed = true;
        }
        if kind == FirmwareCaptureReceiptKind::Prepared {
            self.last_callback = self.reservation.initial_fence.allocated;
        }
        if kind == FirmwareCaptureReceiptKind::Closed {
            self.closed = true;
            self.cancelled = cancelled;
        }
        Ok(hash)
    }
    pub(crate) fn last_record_hash(&self) -> Result<ContentHash, ServiceError> {
        ContentHash::new(
            &self
                .records
                .last()
                .ok_or_else(|| invalid("Capture preparation missing"))?
                .sha256,
        )
    }
    pub(crate) fn last_worker_sequence(&self) -> u64 {
        self.last_worker_sequence
    }
    pub(crate) fn publish(
        &self,
        archive: Box<PublishedFirmwareBackupReview>,
    ) -> Result<PublishedFirmwareCaptureEvidence, ServiceError> {
        if !self.closed || self.acquisition.as_deref() != Some(archive.acquisition()) {
            return Err(invalid(
                "Capture publication requires matching closed archive",
            ));
        }
        archive.revalidate()?;
        let proposal = archive.proposal();
        let index = Index {
            schema: SCHEMA.into(),
            strict_archive_policy: POLICY.into(),
            capture_id: self.id.clone(),
            acquisition: self.acquisition.clone(),
            records: self.records.clone(),
            record_bytes: self.bytes as u64,
            archive_sha256: proposal.archive_sha256,
            sidecar_sha256: proposal.sidecar_sha256,
            closed: self.closed,
            cancelled: self.cancelled,
        };
        let bytes = serde_json::to_vec(&index).map_err(invalid)?;
        if bytes.len() > FIRMWARE_CAPTURE_INDEX_LIMIT
            || self.bytes + bytes.len() > FIRMWARE_BACKUP_MATERIAL_LIMIT
        {
            return Err(invalid("Capture index/material exceeds budget"));
        }
        let index_path = self.directory.join("index.json");
        durable_file(&self.directory, &index_path, &bytes)?;
        let summary = reopen_firmware_capture_evidence(&index_path)?;
        if summary.index_sha256 != ContentHash::of(&bytes).as_str() {
            return Err(invalid("Capture published index differs"));
        }
        Ok(PublishedFirmwareCaptureEvidence {
            index: index_path,
            summary,
            archive,
        })
    }
    pub(crate) fn abandon(&mut self, run: &str, sequence: u64) -> Result<(), ServiceError> {
        if !self.closed {
            self.append(
                run,
                sequence,
                self.acquisition.clone().as_deref(),
                FirmwareCaptureReceiptKind::Closed,
                None,
                true,
            )?;
        }
        Ok(())
    }
}
/// Private-issued, nonclone local publication. Reopening never recreates this object.
pub struct PublishedFirmwareCaptureEvidence {
    index: PathBuf,
    summary: FirmwareCaptureEvidenceSummary,
    archive: Box<PublishedFirmwareBackupReview>,
}
impl PublishedFirmwareCaptureEvidence {
    pub fn index_path(&self) -> &Path {
        &self.index
    }
    pub fn summary(&self) -> &FirmwareCaptureEvidenceSummary {
        &self.summary
    }
    pub fn archive(&self) -> &PublishedFirmwareBackupReview {
        &self.archive
    }
    pub fn revalidate(&self) -> Result<(), ServiceError> {
        self.archive.revalidate()?;
        if reopen_firmware_capture_evidence(&self.index)? != self.summary {
            return Err(invalid("Published capture evidence changed"));
        }
        let proposal = self.archive.proposal();
        if proposal.archive_sha256 != self.summary.archive_sha256
            || proposal.sidecar_sha256 != self.summary.sidecar_sha256
        {
            return Err(invalid("Capture archive linkage differs"));
        }
        Ok(())
    }
}
/// Bounded inspection of one original record, including partial/failure evidence.
/// It does not certify the surrounding chain or reconstruct any current-run receipt.
pub fn reopen_firmware_capture_record(
    path: &Path,
) -> Result<FirmwareCaptureRecordInspection, ServiceError> {
    let raw = checked_read(path, RECORD_BYTES)?;
    let record: FirmwareCaptureRecordInspection = serde_json::from_slice(&raw).map_err(invalid)?;
    if canonical_firmware_capture_record_bytes(&record)? != raw {
        return Err(invalid("Unsupported or noncanonical capture record"));
    }
    Ok(record)
}
/// Independently reread descriptive inspection, never a current-run durability receipt.
pub fn reopen_firmware_capture_evidence(
    index_path: &Path,
) -> Result<FirmwareCaptureEvidenceSummary, ServiceError> {
    let bytes = checked_read(index_path, FIRMWARE_CAPTURE_INDEX_LIMIT)?;
    let index: Index = serde_json::from_slice(&bytes).map_err(invalid)?;
    if serde_json::to_vec(&index).map_err(invalid)? != bytes
        || index.schema != SCHEMA
        || index.strict_archive_policy != POLICY
        || index.records.is_empty()
        || index.records.len() > FIRMWARE_CAPTURE_RECORD_LIMIT
        || !index.closed
        || !firmware_backup_hash_valid(&index.archive_sha256)
        || !firmware_backup_hash_valid(&index.sidecar_sha256)
    {
        return Err(invalid(
            "Unsupported, noncanonical or incomplete capture index",
        ));
    }
    let parent = index_path
        .parent()
        .ok_or_else(|| invalid("Capture index parent absent"))?;
    check_directory(parent)?;
    let mut cumulative = 0usize;
    let mut previous = ContentHash::of(&[]).as_str().to_owned();
    let mut reservation = None;
    let mut worker_run = None;
    let mut worker_sequence = 0;
    let mut evidence_sequence = 0;
    let mut callback = 0;
    let mut acquisition = None;
    let mut completed_ns = 0;
    let mut failed = false;
    let mut final_seen = false;
    for (i, pin) in index.records.iter().enumerate() {
        if pin.name != format!("record-{i:04}.json")
            || pin.bytes > RECORD_BYTES as u64
            || !firmware_backup_hash_valid(&pin.sha256)
        {
            return Err(invalid("Capture record pin/name exceeds bounds"));
        }
        let raw = checked_read(&parent.join(&pin.name), RECORD_BYTES)?;
        cumulative = cumulative
            .checked_add(raw.len())
            .ok_or_else(|| invalid("Capture material overflow"))?;
        if cumulative > FIRMWARE_BACKUP_MATERIAL_LIMIT - FIRMWARE_CAPTURE_INDEX_LIMIT
            || raw.len() as u64 != pin.bytes
            || ContentHash::of(&raw).as_str() != pin.sha256
        {
            return Err(invalid("Capture record hash/length/material differs"));
        }
        let r: FirmwareCaptureRecordInspection = serde_json::from_slice(&raw).map_err(invalid)?;
        if canonical_firmware_capture_record_bytes(&r)? != raw
            || r.schema != SCHEMA
            || r.capture_id != index.capture_id
            || r.previous_sha256 != previous
            || r.worker_sequence <= worker_sequence
        {
            return Err(invalid("Capture record chain/order differs"));
        }
        if i == 0 {
            if r.kind != FirmwareCaptureReceiptKind::Prepared
                || r.evidence.is_some()
                || r.acquisition.is_some()
            {
                return Err(invalid("Capture first record is not preparation"));
            }
            let p = r
                .preparation
                .clone()
                .ok_or_else(|| invalid("Capture preparation absent"))?;
            p.validate()?;
            callback = p.initial_fence.allocated;
            completed_ns = p.prepared_monotonic_ns;
            reservation = Some(p);
            worker_run = Some(r.worker_run.clone());
        } else if r.preparation.is_some() || worker_run.as_deref() != Some(r.worker_run.as_str()) {
            return Err(invalid("Capture worker/reservation changed"));
        }
        if let Some(a) = &acquisition
            && r.acquisition.as_ref() != Some(a)
        {
            return Err(invalid("Capture acquisition changed"));
        }
        if let Some(a) = &r.acquisition {
            acquisition = Some(a.clone());
        }
        if failed && r.kind != FirmwareCaptureReceiptKind::Closed {
            return Err(invalid("Capture progression follows retained failure"));
        }
        if let Some(e) = &r.evidence {
            e.validate_for_reservation(
                reservation
                    .as_ref()
                    .ok_or_else(|| invalid("Capture reservation absent"))?,
            )?;
            if final_seen && e.complete {
                return Err(invalid("Completed capture record follows Final identity"));
            }
            if e.read == FirmwareCaptureRead::Identity(FirmwareCaptureBoundaryKind::Final)
                && e.complete
            {
                final_seen = true;
            }
            if e.complete && e.offer.is_none_or(|v| v.started_ns < completed_ns) {
                return Err(invalid("Capture monotonic interval moved backwards"));
            }
            if e.sequence <= evidence_sequence
                || e.complete && (e.before.allocated != callback || e.before.processed != callback)
            {
                return Err(invalid("Capture evidence order/frontier differs"));
            }
            evidence_sequence = e.sequence;
            callback = e.after.allocated;
            if let Some(t) = e.completed_ns {
                completed_ns = completed_ns.max(t);
            }
        }
        if r.kind == FirmwareCaptureReceiptKind::Failure {
            failed = true;
        }
        if i + 1 == index.records.len() {
            if r.kind != FirmwareCaptureReceiptKind::Closed
                || r.cancelled != index.cancelled
                || !r.cancelled && (!final_seen || failed)
            {
                return Err(invalid("Capture final closed record differs"));
            }
        } else if r.kind == FirmwareCaptureReceiptKind::Closed {
            return Err(invalid("Capture records follow close"));
        }
        previous = pin.sha256.clone();
        worker_sequence = r.worker_sequence;
    }
    if reservation.is_none()
        || acquisition != index.acquisition
        || cumulative as u64 != index.record_bytes
        || cumulative + bytes.len() > FIRMWARE_BACKUP_MATERIAL_LIMIT
    {
        return Err(invalid("Capture index totals/acquisition differ"));
    }
    Ok(FirmwareCaptureEvidenceSummary {
        capture_id: index.capture_id,
        acquisition: index.acquisition,
        records: index.records.len(),
        serialized_bytes: (cumulative + bytes.len()) as u64,
        index_sha256: ContentHash::of(&bytes).as_str().into(),
        archive_sha256: index.archive_sha256,
        sidecar_sha256: index.sidecar_sha256,
        closed: index.closed,
        cancelled: index.cancelled,
        limitations: LIMITATIONS.into(),
    })
}
fn check_ancestors(path: &Path) -> Result<(), ServiceError> {
    for p in path.ancestors() {
        if !p.as_os_str().is_empty() {
            store::reject_symlink(p)?;
        }
    }
    Ok(())
}
fn check_directory(path: &Path) -> Result<(), ServiceError> {
    check_ancestors(path)?;
    if !fs::symlink_metadata(path).map_err(io)?.is_dir() {
        return Err(invalid("Capture authority parent is not a directory"));
    }
    Ok(())
}
fn ensure_directory(path: &Path, new: bool) -> Result<(), ServiceError> {
    check_ancestors(path)?;
    match fs::create_dir(path) {
        Ok(()) => {
            #[cfg(unix)]
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io)?;
        }
        Err(e) if !new && e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(io(e)),
    }
    check_directory(path)
}
fn sync_directory(path: &Path) -> Result<(), ServiceError> {
    check_directory(path)?;
    fault("directory-sync")?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "linux")]
    options.custom_flags(0x20000);
    options.open(path).and_then(|f| f.sync_all()).map_err(io)
}
fn checked_read(path: &Path, max: usize) -> Result<Vec<u8>, ServiceError> {
    check_ancestors(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "linux")]
    options.custom_flags(0x20000);
    let file = options.open(path).map_err(io)?;
    let before = file.metadata().map_err(io)?;
    if !before.is_file() || before.len() > max as u64 {
        return Err(invalid("Capture file is nonregular or oversized"));
    }
    let mut raw = Vec::new();
    file.take(max as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(io)?;
    let after = fs::symlink_metadata(path).map_err(io)?;
    #[cfg(unix)]
    if before.dev() != after.dev() || before.ino() != after.ino() {
        return Err(invalid("Capture file identity changed"));
    }
    if after.file_type().is_symlink()
        || raw.len() > max
        || raw.len() as u64 != before.len()
        || before.len() != after.len()
    {
        return Err(invalid("Capture file changed during reread"));
    }
    fault("reread")?;
    Ok(raw)
}
fn check_identity(expected: &fs::Metadata, path: &Path) -> Result<(), ServiceError> {
    let current = fs::symlink_metadata(path).map_err(io)?;
    #[cfg(unix)]
    if current.dev() != expected.dev() || current.ino() != expected.ino() {
        return Err(invalid("Capture publication inode identity changed"));
    }
    if current.file_type().is_symlink()
        || current.is_file() != expected.is_file()
        || current.is_dir() != expected.is_dir()
        || current.is_file() && current.len() != expected.len()
    {
        return Err(invalid("Capture publication identity/extent changed"));
    }
    Ok(())
}
fn durable_file(parent: &Path, path: &Path, bytes: &[u8]) -> Result<(), ServiceError> {
    check_directory(parent)?;
    store::reject_symlink(path)?;
    let mut parent_options = OpenOptions::new();
    parent_options.read(true);
    #[cfg(target_os = "linux")]
    parent_options.custom_flags(0x20000);
    let parent_file = parent_options.open(parent).map_err(io)?;
    let parent_identity = parent_file.metadata().map_err(io)?;
    check_identity(&parent_identity, parent)?;
    let sync_parent = || -> Result<(), ServiceError> {
        check_identity(&parent_identity, parent)?;
        fault("directory-sync")?;
        parent_file.sync_all().map_err(io)
    };
    let temp = parent.join(format!(".capture-{}.tmp", store::nonce()?));
    fault("before-create")?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    #[cfg(target_os = "linux")]
    options.custom_flags(0x20000);
    let mut file = options.open(&temp).map_err(io)?;
    file.write_all(bytes).map_err(io)?;
    fault("after-write")?;
    file.sync_all().map_err(io)?;
    let synced_identity = file.metadata().map_err(io)?;
    fault("after-file-sync")?;
    if checked_read(&temp, RECORD_BYTES)? != bytes {
        return Err(invalid("Capture temporary reread differs"));
    }
    check_identity(&synced_identity, &temp)?;
    check_identity(&parent_identity, parent)?;
    fs::hard_link(&temp, path).map_err(io)?;
    fault("after-link")?;
    check_identity(&synced_identity, path)?;
    sync_parent()?;
    fault("before-cleanup")?;
    check_identity(&synced_identity, &temp)?;
    fs::remove_file(&temp).map_err(io)?;
    sync_parent()?;
    if checked_read(path, RECORD_BYTES)? != bytes {
        return Err(invalid("Capture final reread differs"));
    }
    check_identity(&synced_identity, path)?;
    check_identity(&parent_identity, parent)?;
    Ok(())
}

#[cfg(not(test))]
fn fault(_stage: &str) -> Result<(), ServiceError> {
    Ok(())
}
#[cfg(test)]
thread_local! { static FAILURE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
pub(crate) fn set_fault(stage: Option<&str>) {
    FAILURE.with(|f| *f.borrow_mut() = stage.map(str::to_owned));
}
#[cfg(test)]
type CaptureMutation = (String, Box<dyn FnOnce()>);
#[cfg(test)]
thread_local! {static MUTATION: std::cell::RefCell<Option<CaptureMutation>>=const{std::cell::RefCell::new(None)};}
#[cfg(test)]
pub(crate) fn set_mutation(stage: &str, action: impl FnOnce() + 'static) {
    MUTATION.with(|m| *m.borrow_mut() = Some((stage.into(), Box::new(action))));
}
#[cfg(test)]
fn fault(stage: &str) -> Result<(), ServiceError> {
    let action = MUTATION.with(|m| {
        let matched = m.borrow().as_ref().is_some_and(|(name, _)| name == stage);
        if matched {
            m.borrow_mut().take().map(|(_, f)| f)
        } else {
            None
        }
    });
    if let Some(action) = action {
        action();
    }
    FAILURE.with(|f| {
        if f.borrow().as_deref() == Some(stage) {
            Err(io(format!("Injected capture fault at {stage}")))
        } else {
            Ok(())
        }
    })
}

pub(crate) fn validate_capture_boundary(
    origin: &crate::BackupOrigin,
    e: &FirmwareCaptureEvidence,
) -> Result<(), ServiceError> {
    use crate::device_backup::{check_snapshot, parse_raw};
    use gp200_protocol::{GlobalRecords, Identity};
    use std::{collections::BTreeSet, sync::Arc};
    let raw = parse_raw(&e.raw)?;
    if raw.kind() != 0x12 {
        return Err(invalid("Capture boundary reply kind differs"));
    }
    match e.read {
        FirmwareCaptureRead::Identity(_) => {
            if raw.records().iter().filter(|r| r.command == 0x2001).count() != 1
                || Identity::from_transfer(&raw)
                    .is_none_or(|v| v.state_raw != 1 || v.model_raw != 2)
            {
                return Err(invalid("Capture identity is not unique normal GP-200"));
            }
        }
        FirmwareCaptureRead::Boundary {
            object: FirmwareBackupObject::Current,
            ..
        } => {
            let source = match origin.identity.mode {
                BackendMode::Simulator => SnapshotSource::Simulator,
                _ => SnapshotSource::Hardware {
                    epoch: origin.epoch,
                },
            };
            let snapshot = PresetSnapshot::from_raw(
                Arc::new(raw),
                source,
                Some(PresetTarget::EditBuffer),
                Revision(e.sequence),
            );
            check_snapshot(origin, &snapshot, PresetTarget::EditBuffer)?;
        }
        FirmwareCaptureRead::Boundary {
            object: FirmwareBackupObject::Globals,
            ..
        } => {
            let records = GlobalRecords::from_transfer(raw).map_err(invalid)?;
            if !records.is_complete()
                || records.footswitch_tables.len() != 3
                || records
                    .footswitch_tables
                    .iter()
                    .map(|t| t.table_raw())
                    .collect::<BTreeSet<_>>()
                    != BTreeSet::from([0, 1, 2])
            {
                return Err(invalid("Capture middle globals missing mapping tables"));
            }
        }
        _ => return Err(invalid("Capture boundary read kind differs")),
    }
    Ok(())
}
