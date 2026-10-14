//! Checked descriptions of local predecessors. Disk history never issues I/O authority.
//!
//! This module deliberately has no native opener, firmware offer, approval issuer,
//! quarantine clear, or normal-handoff constructor. Its private checked result must
//! be consumed by a freshly guarded recovery owner before any live review exists.
use crate::firmware_entry_study::EntryStudyFilePin;
use gp200_domain::{
    FIRMWARE_FRAME_LIMIT, FIRMWARE_JOURNAL_LIMIT, FirmwareAttemptId, FirmwareCaptureReservation,
    FirmwareUsbObservation, JobSha256, ServiceError,
};
use rustix::fs::{self, FileType, Mode, OFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    os::{
        fd::AsRawFd,
        unix::fs::{FileExt, MetadataExt},
    },
    path::{Component, Path, PathBuf},
};

const REGISTRY_FILE_LIMIT: u64 = 1024 * 1024;
const REGISTRY_RECORD_LIMIT: usize = 16 * 1024;
const JOURNAL_RECORD_LIMIT: usize = 512 * 1024;
// Completion's genuine 561-record source-bound begin is larger than later observations.
// The distinct begin cap does not enlarge the limit for every subsequent record.
const JOURNAL_BEGIN_LIMIT: usize = 32 * 1024 * 1024;
const MAX_ATTEMPTS: usize = 1024;
const MAX_PREDECESSORS: usize = 64;
const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const READ: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const RECOVERY_PURPOSE: &str = "fresh-full-image-recovery-v1";

fn bad(message: impl std::fmt::Display) -> ServiceError {
    ServiceError::invalid(format!(
        "Checked local firmware recovery unavailable: {message}"
    ))
}

/// An exported prefix description. Deserializing it does not authorize any action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryFilePrefix {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    pub device: u64,
    pub inode: u64,
}

/// References are descriptive history; approvals and boot witnesses are never stored here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPredecessorReference {
    pub attempt: FirmwareAttemptId,
    pub registry: RecoveryFilePrefix,
    pub journal: RecoveryFilePrefix,
    pub original_binding_sha256: String,
    pub reservation_binding_sha256: String,
    pub image_sha256: String,
    pub source_job_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    device: u64,
    inode: u64,
    bytes: u64,
    links: u64,
    uid: u32,
    mode: u32,
    mtime: i64,
    mtime_ns: i64,
    ctime: i64,
    ctime_ns: i64,
}
impl Stamp {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            bytes: metadata.len(),
            links: metadata.nlink(),
            uid: metadata.uid(),
            mode: metadata.mode(),
            mtime: metadata.mtime(),
            mtime_ns: metadata.mtime_nsec(),
            ctime: metadata.ctime(),
            ctime_ns: metadata.ctime_nsec(),
        }
    }
}

#[derive(Debug)]
struct CheckedFile {
    description: RecoveryFilePrefix,
    stamp: Stamp,
}

/// A noncloneable in-process snapshot of the entire current unresolved local chain.
/// A checked history cannot be deserialized, cannot resume, and cannot open MIDI.
#[derive(Debug)]
pub(crate) struct CheckedRecoveryLineage {
    registry_directory: PathBuf,
    library_root: PathBuf,
    selected: FirmwareAttemptId,
    original_binding: FirmwareUsbObservation,
    predecessors: Vec<CheckedPredecessor>,
}

#[derive(Debug)]
struct CheckedPredecessor {
    attempt: FirmwareAttemptId,
    original_binding: FirmwareUsbObservation,
    registry: CheckedFile,
    journal: CheckedFile,
    image_sha256: String,
    source_job_sha256: String,
    source_review_sha256: String,
    reservation_binding_sha256: String,
    registry_records: u64,
    ancestors: Vec<RecoveryPredecessorReference>,
    historical_capture: Option<RecoveryHistoricalCapture>,
}

/// Age and original file references remain historical descriptions, never a fresh capture.
#[derive(Debug, Clone, Serialize)]
pub struct RecoveryHistoricalCapture {
    pub prepared_wall_millis: u64,
    pub acquisition: String,
    pub evidence_index_sha256: String,
    pub captured_materials: Vec<EntryStudyFilePin>,
    pub status: String,
}

#[derive(Debug)]
struct Reservation {
    attempt: FirmwareAttemptId,
    job_sha256: String,
    profile_sha256: String,
    binding_sha256: String,
    image_sha256: String,
    journal: PathBuf,
    generation: u64,
    updater_epoch: u64,
}

struct RegistryHistory {
    reservation: Reservation,
    file: CheckedFile,
    last_state: u8,
    records: u64,
}

impl CheckedRecoveryLineage {
    pub(crate) fn matches_registry(&self, directory: &File) -> Result<(), ServiceError> {
        check_directory_identity(
            directory,
            &self.registry_directory,
            rustix::process::geteuid().as_raw(),
        )
    }
    pub(crate) fn selected(&self) -> &FirmwareAttemptId {
        &self.selected
    }
    pub(crate) fn original_binding(&self) -> &FirmwareUsbObservation {
        &self.original_binding
    }
    pub(crate) fn references(&self) -> Vec<RecoveryPredecessorReference> {
        self.predecessors
            .iter()
            .map(CheckedPredecessor::reference)
            .collect()
    }
    pub(crate) fn source_reviews(&self) -> Vec<String> {
        self.predecessors
            .iter()
            .map(|p| p.source_review_sha256.clone())
            .collect()
    }
    pub(crate) fn historical_captures(&self) -> Vec<RecoveryHistoricalCapture> {
        self.predecessors
            .iter()
            .filter_map(|p| p.historical_capture.clone())
            .collect()
    }
    pub(crate) fn validate_linked_closure_budget(
        &self,
        journal: &Path,
    ) -> Result<(), ServiceError> {
        let mut maximum = 3000usize
            .checked_add(journal.as_os_str().len())
            .ok_or_else(|| bad("Linked closure path budget overflows"))?;
        for p in &self.predecessors {
            let r = &p.registry.description;
            // The new successor identifier and set digest have fixed lengths.
            let pending = format!(
                "schema=1\nkind=linked-recovery-predecessor\nsuccessor={}\nset={}\noriginal_bytes={}\noriginal_sha256={}\noriginal_device={}\noriginal_inode={}\nno_physical_authority=true",
                "a".repeat(32),
                "0".repeat(64),
                r.bytes,
                r.sha256,
                r.device,
                r.inode
            );
            if p.registry_records >= 256
                || r.bytes
                    .checked_add(45 + pending.len() as u64)
                    .is_none_or(|n| n > REGISTRY_FILE_LIMIT)
            {
                return Err(bad(
                    "Original predecessor lacks durable linked append capacity",
                ));
            }
            maximum = maximum
                .checked_add(
                    format!(
                        "predecessor={},{},{},{},{},{},{}\n",
                        p.attempt.as_str(),
                        r.bytes,
                        r.sha256,
                        r.device,
                        r.inode,
                        REGISTRY_FILE_LIMIT,
                        "0".repeat(64)
                    )
                    .len(),
                )
                .ok_or_else(|| bad("Complete linked set budget overflows"))?;
        }
        if maximum > REGISTRY_RECORD_LIMIT - 1 {
            return Err(bad(
                "Complete linked closure cannot fit its bounded registry record before any query",
            ));
        }
        Ok(())
    }
    pub(crate) fn revalidate(&self) -> Result<(), ServiceError> {
        self.revalidate_except_live(None)
    }
    pub(crate) fn revalidate_except_live(
        &self,
        live: Option<&FirmwareAttemptId>,
    ) -> Result<(), ServiceError> {
        let uid = rustix::process::geteuid().as_raw();
        let directory = open_private_directory(&self.registry_directory, uid)?;
        let current = load_lineage_except(
            &directory,
            &self.registry_directory,
            &self.library_root,
            &self.selected,
            live,
        )?;
        if current.references() != self.references()
            || current.original_binding != self.original_binding
            || current.source_reviews() != self.source_reviews()
            || current.predecessors.len() != self.predecessors.len()
            || current
                .predecessors
                .iter()
                .zip(&self.predecessors)
                .any(|(a, b)| {
                    a.registry.stamp != b.registry.stamp || a.journal.stamp != b.journal.stamp
                })
        {
            return Err(bad(
                "Selected local lineage or exact file identities changed",
            ));
        }
        Ok(())
    }
}

impl CheckedPredecessor {
    fn reference(&self) -> RecoveryPredecessorReference {
        RecoveryPredecessorReference {
            attempt: self.attempt.clone(),
            registry: self.registry.description.clone(),
            journal: self.journal.description.clone(),
            original_binding_sha256: self.original_binding.digest(),
            reservation_binding_sha256: self.reservation_binding_sha256.clone(),
            image_sha256: self.image_sha256.clone(),
            source_job_sha256: self.source_job_sha256.clone(),
        }
    }
}

/// Called only through the canonical registry while its actual coordination lock is held.
/// The directory FD is checked against the exact plain private named directory.
pub(crate) fn load_local_lineage(
    directory: &File,
    registry_directory: &Path,
    library_root: &Path,
    selected: &FirmwareAttemptId,
) -> Result<CheckedRecoveryLineage, ServiceError> {
    load_lineage_except(directory, registry_directory, library_root, selected, None)
}
fn load_lineage_except(
    directory: &File,
    registry_directory: &Path,
    library_root: &Path,
    selected: &FirmwareAttemptId,
    live: Option<&FirmwareAttemptId>,
) -> Result<CheckedRecoveryLineage, ServiceError> {
    let uid = rustix::process::geteuid().as_raw();
    check_directory_identity(directory, registry_directory, uid)?;
    let _library = open_history_directory(library_root, uid, false)?;
    let mut unresolved = BTreeMap::<FirmwareAttemptId, CheckedPredecessor>::new();
    let names =
        std::fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd())).map_err(bad)?;
    let mut entries = 0usize;
    for entry in names {
        entries += 1;
        if entries > MAX_ATTEMPTS {
            return Err(bad("Registry entry count exceeds recovery budget"));
        }
        let name = entry.map_err(bad)?.file_name();
        let name = name.to_str().ok_or_else(|| bad("Non-UTF8 registry name"))?;
        if name == "registry.lock" || valid_physical_lock_name(name) {
            let file = open_at_private(directory, name, uid, REGISTRY_FILE_LIMIT, true)?;
            // Locks are permanently named regular files. Their content is never an attempt.
            settle_named(
                &file,
                directory,
                name,
                Stamp::of(&file.metadata().map_err(bad)?),
                uid,
            )?;
            continue;
        }
        let token = name
            .strip_suffix(".quarantine")
            .ok_or_else(|| bad("Unknown/pending local registry entry"))?;
        let attempt = canonical_attempt(token)?;
        let history = read_registry(directory, registry_directory, name, uid)?;
        if history.reservation.attempt != attempt {
            return Err(bad("Registry filename and reservation attempt differ"));
        }
        if live == Some(&attempt) {
            continue;
        }
        if matches!(history.last_state, 6 | 7)
            && crate::firmware_quarantine::linked_attempt_closed(directory, uid, &attempt, false)?
        {
            continue;
        }
        if matches!(history.last_state, 2 | 5) {
            continue;
        }
        if history.last_state == 3 {
            return Err(bad("Mock closure cannot enter physical recovery lineage"));
        }
        if !matches!(history.last_state, 0 | 1 | 4 | 6) {
            return Err(bad("Unrecognized unresolved registry state"));
        }
        let predecessor = read_predecessor(history, library_root, uid)?;
        if unresolved.insert(attempt, predecessor).is_some() {
            return Err(bad("Duplicate unresolved local attempt"));
        }
        if unresolved.len() > MAX_PREDECESSORS {
            return Err(bad("Recovery lineage exceeds bounded predecessor budget"));
        }
        let total = unresolved
            .values()
            .try_fold(0u64, |sum, p| {
                sum.checked_add(p.registry.description.bytes)
                    .and_then(|n| n.checked_add(p.journal.description.bytes))
            })
            .ok_or_else(|| bad("Recovery combined history extent overflows"))?;
        if total > FIRMWARE_JOURNAL_LIMIT {
            return Err(bad(
                "Combined original lineage exceeds the bounded history budget",
            ));
        }
    }
    check_directory_identity(directory, registry_directory, uid)?;
    validate_chain(&unresolved, selected)?;
    let original_binding = unresolved
        .get(selected)
        .ok_or_else(|| bad("Selected attempt is not unresolved local history"))?
        .original_binding
        .clone();
    Ok(CheckedRecoveryLineage {
        registry_directory: registry_directory.into(),
        library_root: library_root.into(),
        selected: selected.clone(),
        original_binding,
        predecessors: unresolved.into_values().collect(),
    })
}

fn valid_physical_lock_name(name: &str) -> bool {
    name.strip_prefix("physical-")
        .and_then(|v| v.strip_suffix(".lock"))
        .is_some_and(|v| {
            v.len() == 64
                && v.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
}

fn canonical_attempt(value: &str) -> Result<FirmwareAttemptId, ServiceError> {
    let attempt = FirmwareAttemptId::parse(value)?;
    if attempt.as_str() != value {
        return Err(bad("Attempt identifier is not canonical lowercase"));
    }
    Ok(attempt)
}
fn canonical_digest(value: &str) -> Result<String, ServiceError> {
    if JobSha256::parse(value)?.hex() != value {
        return Err(bad("Noncanonical retained digest"));
    }
    Ok(value.into())
}

fn read_registry(
    directory: &File,
    root: &Path,
    name: &str,
    uid: u32,
) -> Result<RegistryHistory, ServiceError> {
    let file = open_at_private(directory, name, uid, REGISTRY_FILE_LIMIT, false)?;
    let before = Stamp::of(&file.metadata().map_err(bad)?);
    let mut reservation = None;
    let mut last_state = 255;
    let (sha256, records) = scan_frames(
        &file,
        before.bytes,
        b"GP2QUAR1",
        REGISTRY_RECORD_LIMIT,
        REGISTRY_RECORD_LIMIT,
        256,
        |index, body| {
            let (&state, payload) = body
                .split_first()
                .ok_or_else(|| bad("Empty registry state"))?;
            if state > 7 || (index > 0 && state == 0) || matches!(last_state, 2 | 3 | 5 | 7) {
                return Err(bad(
                    "Unknown registry state, repeated begin, or bytes after closure",
                ));
            }
            if index == 0 {
                if state != 0 {
                    return Err(bad("Registry lacks immutable reservation"));
                }
                reservation = Some(parse_reservation(payload)?);
            }
            if state == 2 && payload != b"creating owner observed zero driver calls" {
                return Err(bad("Unknown creating-owner zero-call closure"));
            }
            if matches!(state, 6 | 7) {
                crate::firmware_quarantine::validate_linked_record(state, payload)?;
            }
            if state == 5 {
                validate_completion_closure(
                    payload,
                    reservation
                        .as_ref()
                        .ok_or_else(|| bad("Closure reservation absent"))?,
                )?;
            }
            last_state = state;
            Ok(())
        },
    )?;
    if records == 0 {
        return Err(bad("Empty registry attempt"));
    }
    settle_named(&file, directory, name, before, uid)?;
    Ok(RegistryHistory {
        reservation: reservation.ok_or_else(|| bad("Reservation absent"))?,
        file: CheckedFile {
            description: RecoveryFilePrefix {
                path: root.join(name),
                bytes: before.bytes,
                sha256,
                device: before.device,
                inode: before.inode,
            },
            stamp: before,
        },
        last_state,
        records,
    })
}

fn parse_reservation(bytes: &[u8]) -> Result<Reservation, ServiceError> {
    let text = std::str::from_utf8(bytes).map_err(bad)?;
    let keys = [
        "schema=",
        "origin=",
        "attempt=",
        "review=",
        "profile=",
        "binding=",
        "generation=",
        "clock=",
        "image=",
        "journal=",
        "updater_epoch=",
        "no_physical_authority=",
    ];
    let lines = text.lines().collect::<Vec<_>>();
    if lines.len() != keys.len() || text.chars().any(|c| c.is_control() && c != '\n') {
        return Err(bad("Unknown reservation schema or control bytes"));
    }
    let values = lines
        .iter()
        .zip(keys)
        .map(|(line, key)| {
            line.strip_prefix(key)
                .ok_or_else(|| bad("Reservation key/order differs"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if values[0] != "1"
        || values[1] != "LocalObservation"
        || values[11] != "true"
        || values[7].is_empty()
        || values[7].len() > 128
        || values[9].len() > 4096
    {
        return Err(bad(
            "Reservation schema, physical origin, authority claim or clock differs",
        ));
    }
    let generation = values[6].parse::<u64>().map_err(bad)?;
    let updater_epoch = values[10].parse::<u64>().map_err(bad)?;
    if generation == 0 || updater_epoch == 0 {
        return Err(bad("Reservation epoch/generation is zero"));
    }
    let journal = PathBuf::from(values[9]);
    plain_absolute(&journal)?;
    Ok(Reservation {
        attempt: canonical_attempt(values[2])?,
        job_sha256: canonical_digest(values[3])?,
        profile_sha256: canonical_digest(values[4])?,
        binding_sha256: canonical_digest(values[5])?,
        image_sha256: canonical_digest(values[8])?,
        journal,
        generation,
        updater_epoch,
    })
}

fn read_predecessor(
    history: RegistryHistory,
    library_root: &Path,
    uid: u32,
) -> Result<CheckedPredecessor, ServiceError> {
    let registry_records = history.records;
    let reservation = history.reservation;
    if reservation.image_sha256 != crate::firmware_vendor_sequence::VENDOR_IMAGE_SHA256
        || !matches!(
            reservation.profile_sha256.as_str(),
            crate::firmware_vendor_sequence::VENDOR_CONTRACT_SHA256
                | crate::firmware_vendor_sequence::HISTORICAL_CORRECTED_VENDOR_CONTRACT_SHA256
                | crate::firmware_vendor_sequence::LEGACY_VENDOR_CONTRACT_SHA256
        )
    {
        return Err(bad(
            "Predecessor is not this supported local Linux vendor-image route",
        ));
    }
    let directory_path = library_root.join("linux-firmware-completion");
    let expected = directory_path.join(format!("{}.journal", reservation.attempt.as_str()));
    if reservation.journal != expected {
        return Err(bad(
            "Original journal is outside this original local library",
        ));
    }
    let directory = open_private_directory(&directory_path, uid)?;
    let name = expected
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| bad("Journal name invalid"))?;
    let file = open_at_private(&directory, name, uid, FIRMWARE_JOURNAL_LIMIT, false)?;
    let before = Stamp::of(&file.metadata().map_err(bad)?);
    let mut begin = None;
    let (sha256, _) = scan_frames(
        &file,
        before.bytes,
        b"GP2LCJ01",
        JOURNAL_BEGIN_LIMIT,
        JOURNAL_RECORD_LIMIT,
        FIRMWARE_FRAME_LIMIT,
        |index, body| {
            let value: Value = serde_json::from_slice(body).map_err(bad)?;
            let kind = value
                .get("kind")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("Journal record lacks kind"))?;
            if index == 0 {
                if kind != "begin" {
                    return Err(bad("Original completion journal lacks begin"));
                }
                begin = Some(parse_begin(&value, &reservation)?);
            } else if !known_journal_record(kind) || kind == "begin" {
                return Err(bad("Unsupported journal record or repeated begin"));
            }
            Ok(())
        },
    )?;
    settle_named(&file, &directory, name, before, uid)?;
    check_directory_identity(&directory, &directory_path, uid)?;
    let (original_binding, source_review_sha256, ancestors, historical_capture) =
        begin.ok_or_else(|| bad("Journal begin missing"))?;
    Ok(CheckedPredecessor {
        attempt: reservation.attempt,
        original_binding,
        registry: history.file,
        journal: CheckedFile {
            description: RecoveryFilePrefix {
                path: expected,
                bytes: before.bytes,
                sha256,
                device: before.device,
                inode: before.inode,
            },
            stamp: before,
        },
        image_sha256: reservation.image_sha256,
        source_job_sha256: reservation.job_sha256,
        reservation_binding_sha256: reservation.binding_sha256,
        source_review_sha256,
        registry_records,
        ancestors,
        historical_capture,
    })
}

fn known_journal_record(kind: &str) -> bool {
    matches!(
        kind,
        "checkpoint"
            | "raw-chunk"
            | "write-syscall"
            | "drain-syscall"
            | "entry-host-settlement"
            | "entry-candidate-omitted"
            | "firmware-reader-retired"
            | "phase-review"
            | "exact-phase-signal"
            | "vendor-action-pair"
            | "normal-mode-observed"
            | "live-completion-verified"
            | "failed"
            | "creating-owner-zero-invocations"
            | "live-creating-owner-startup-zero-invocations"
            | "creating-owner-uninvoked-journal-retirement"
            | "native-open-failed-before-invocation"
            | "recovery-predecessors-checked"
            | "live-recovery-completion-verified"
    )
}

fn parse_begin(
    value: &Value,
    reservation: &Reservation,
) -> Result<
    (
        FirmwareUsbObservation,
        String,
        Vec<RecoveryPredecessorReference>,
        Option<RecoveryHistoricalCapture>,
    ),
    ServiceError,
> {
    if value.get("schema").and_then(Value::as_u64) != Some(1)
        || value.get("run").and_then(Value::as_str) != Some(reservation.attempt.as_str())
        || value.get("resume_supported").and_then(Value::as_bool) != Some(false)
        || value.get("physical_authority").and_then(Value::as_bool) != Some(false)
    {
        return Err(bad("Journal begin run/schema/authority differs"));
    }
    let metadata = value
        .get("metadata")
        .ok_or_else(|| bad("Journal begin metadata absent"))?;
    let review = metadata
        .get("review")
        .ok_or_else(|| bad("Original live review absent"))?;
    let source_review = canonical_digest(string(metadata, "review_digest")?)?;
    if review.get("schema").and_then(Value::as_u64) != Some(1)
        || string(review, "job_sha256")? != reservation.job_sha256
    {
        return Err(bad("Original source job and registry differ"));
    }
    let purpose = metadata.get("purpose").and_then(Value::as_str);
    let (binding_value, anchor_value, ancestors, historical_capture) = if purpose
        == Some(RECOVERY_PURPOSE)
    {
        let references: Vec<RecoveryPredecessorReference> = serde_json::from_value(
            metadata
                .get("predecessors")
                .cloned()
                .ok_or_else(|| bad("Recovery begin lacks predecessor refs"))?,
        )
        .map_err(bad)?;
        if references.is_empty() || references.len() > MAX_PREDECESSORS {
            return Err(bad("Recovery predecessor count invalid"));
        }
        (
            metadata
                .get("original_binding")
                .ok_or_else(|| bad("Recovery original association absent"))?,
            metadata
                .get("anchor_binding")
                .ok_or_else(|| bad("Recovery current anchor snapshot absent"))?,
            references,
            None,
        )
    } else {
        if purpose.is_some() {
            return Err(bad("Unknown local completion purpose"));
        }
        let capture = review
            .get("reservation")
            .ok_or_else(|| bad("Initial normal capture reservation absent"))?;
        let capture_description: FirmwareCaptureReservation =
            serde_json::from_value(capture.clone()).map_err(bad)?;
        capture_description.validate()?;
        let epoch = capture_description.epoch.0;
        if metadata
            .get("original_normal_epoch")
            .and_then(Value::as_u64)
            != Some(epoch)
            || epoch.checked_add(1) != Some(reservation.updater_epoch)
            || reservation.generation != 1
        {
            return Err(bad(
                "Original captured-normal and updater epoch/generation differ",
            ));
        }
        let original = capture
            .get("usb")
            .ok_or_else(|| bad("Original normal snapshot absent"))?;
        let acquisition = string(review, "acquisition")?;
        if acquisition.is_empty()
            || acquisition.len() > 256
            || acquisition.chars().any(char::is_control)
        {
            return Err(bad("Original capture acquisition label invalid"));
        }
        let evidence_index_sha256 = canonical_digest(string(review, "evidence_index_sha256")?)?;
        let captured_materials: Vec<EntryStudyFilePin> = serde_json::from_value(
            review
                .get("captured_materials")
                .cloned()
                .ok_or_else(|| bad("Original capture file references absent"))?,
        )
        .map_err(bad)?;
        if captured_materials.len() != 3 {
            return Err(bad(
                "Original capture file count differs from local completion schema",
            ));
        }
        let mut paths = BTreeSet::new();
        for pin in &captured_materials {
            plain_absolute(&pin.path)?;
            canonical_digest(&pin.sha256)?;
            if pin.bytes == 0 || pin.bytes > FIRMWARE_JOURNAL_LIMIT || !paths.insert(&pin.path) {
                return Err(bad("Original capture file reference extent/path differs"));
            }
        }
        let historical=RecoveryHistoricalCapture { prepared_wall_millis:capture_description.prepared_wall_millis,
            acquisition:acquisition.into(),evidence_index_sha256,captured_materials,
            status:"Historical original capture references only. Original strict/conditional coverage remains in the retained source journal; these references are not freshly captured, reread material, a restoration proof or I/O authority.".into() };
        (original, original, vec![], Some(historical))
    };
    let binding: FirmwareUsbObservation =
        serde_json::from_value(binding_value.clone()).map_err(bad)?;
    binding.validate()?;
    let anchor: FirmwareUsbObservation =
        serde_json::from_value(anchor_value.clone()).map_err(bad)?;
    anchor.validate()?;
    if anchor.digest() != reservation.binding_sha256 {
        return Err(bad(
            "Original run anchor snapshot and registry binding digest differ",
        ));
    }
    if !crate::firmware_linux_completion_binding::associated_identity(&binding, &anchor) {
        return Err(bad(
            "Recovery anchor is unrelated to the checked original association",
        ));
    }
    if binding.vendor != 0x84ef || binding.product != 0x002a {
        return Err(bad("Original association is not a normal GP-200 device"));
    }
    Ok((binding, source_review, ancestors, historical_capture))
}
fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str, ServiceError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| bad(format!("Missing/invalid {field}")))
}

fn validate_completion_closure(
    payload: &[u8],
    reservation: &Reservation,
) -> Result<(), ServiceError> {
    let value: Value = serde_json::from_slice(payload).map_err(bad)?;
    if value.get("schema").and_then(Value::as_u64) != Some(1)
        || string(&value, "attempt")? != reservation.attempt.as_str()
        || string(&value, "run")? != reservation.attempt.as_str()
        || string(&value, "journal")?
            != reservation
                .journal
                .to_str()
                .ok_or_else(|| bad("Journal path invalid"))?
        || string(&value, "image_sha256")? != reservation.image_sha256
        || value
            .get("sequence")
            .and_then(Value::as_u64)
            .is_none_or(|n| n == 0)
        || string(&value, "manual_application_version")? != "V1.8.2"
        || value.get("contents_verified").and_then(Value::as_bool) != Some(false)
    {
        return Err(bad("Unsupported local Linux completion closure"));
    }
    for key in ["prefix_sha256", "manual_phase_review", "source_review"] {
        canonical_digest(string(&value, key)?)?;
    }
    let normal = value
        .get("normal")
        .ok_or_else(|| bad("Closure normal proof absent"))?;
    let binding: FirmwareUsbObservation = serde_json::from_value(
        normal
            .get("binding")
            .cloned()
            .ok_or_else(|| bad("Closure normal binding absent"))?,
    )
    .map_err(bad)?;
    binding.validate()?;
    if binding.topology.is_empty()
        || binding.vendor != 0x84ef
        || normal
            .get("epoch")
            .and_then(Value::as_u64)
            .is_none_or(|n| n == 0)
        || normal.get("allocated").and_then(Value::as_u64)
            != normal.get("processed").and_then(Value::as_u64)
        || normal.get("allocated").and_then(Value::as_u64).is_none()
        || normal.get("reader_joined").and_then(Value::as_bool) != Some(true)
        || normal.get("output_destroyed").and_then(Value::as_bool) != Some(true)
    {
        return Err(bad(
            "Closure normal epoch, ownership/frontier evidence differs",
        ));
    }
    for key in ["identity_sha256", "current_sha256", "globals_sha256"] {
        canonical_digest(string(normal, key)?)?;
    }
    Ok(())
}

fn validate_chain(
    predecessors: &BTreeMap<FirmwareAttemptId, CheckedPredecessor>,
    selected: &FirmwareAttemptId,
) -> Result<(), ServiceError> {
    let selected_value = predecessors
        .get(selected)
        .ok_or_else(|| bad("Selected unresolved attempt absent"))?;
    let roots = predecessors
        .values()
        .filter(|p| p.ancestors.is_empty())
        .collect::<Vec<_>>();
    if roots.len() != 1 {
        return Err(bad(
            "Unresolved attempts do not have one checked local initial root",
        ));
    }
    let association = &roots[0].original_binding;
    if selected_value.original_binding != *association
        || predecessors
            .values()
            .any(|p| p.original_binding != *association)
    {
        return Err(bad(
            "Unrelated unresolved physical associations cannot share a recovery job",
        ));
    }
    let mut verified = BTreeSet::from([roots[0].attempt.clone()]);
    while verified.len() < predecessors.len() {
        let next = predecessors
            .values()
            .filter(|p| {
                !verified.contains(&p.attempt)
                    && p.ancestors.iter().all(|a| verified.contains(&a.attempt))
            })
            .collect::<Vec<_>>();
        if next.len() != 1 {
            return Err(bad(
                "Ambiguous branches, cycle or absent recovery ancestors",
            ));
        }
        let predecessor = next[0];
        let names = predecessor
            .ancestors
            .iter()
            .map(|a| a.attempt.clone())
            .collect::<BTreeSet<_>>();
        if names.len() != predecessor.ancestors.len() || names != verified {
            return Err(bad(
                "Recovery begin did not bind the entire preceding unresolved local lineage",
            ));
        }
        for reference in &predecessor.ancestors {
            let actual = predecessors
                .get(&reference.attempt)
                .ok_or_else(|| bad("Recovery ancestor missing"))?;
            if reference.original_binding_sha256 != actual.original_binding.digest()
                || reference.reservation_binding_sha256 != actual.reservation_binding_sha256
                || reference.image_sha256 != actual.image_sha256
                || reference.source_job_sha256 != actual.source_job_sha256
                || reference.registry.path != actual.registry.description.path
                || reference.journal.path != actual.journal.description.path
            {
                return Err(bad(
                    "Recovery ancestor binding, image, job or exact local path differs",
                ));
            }
            validate_retained_prefix(&reference.registry, &actual.registry.description)?;
            validate_retained_prefix(&reference.journal, &actual.journal.description)?;
        }
        verified.insert(predecessor.attempt.clone());
    }
    Ok(())
}

fn validate_retained_prefix(
    reference: &RecoveryFilePrefix,
    current: &RecoveryFilePrefix,
) -> Result<(), ServiceError> {
    canonical_digest(&reference.sha256)?;
    if reference.path != current.path
        || reference.device != current.device
        || reference.inode != current.inode
        || reference.bytes == 0
        || reference.bytes > current.bytes
    {
        return Err(bad("Original predecessor prefix/file identity differs"));
    }
    if reference.bytes == current.bytes {
        if reference.sha256 != current.sha256 {
            return Err(bad("Original predecessor prefix digest differs"));
        }
        return Ok(());
    }
    let uid = rustix::process::geteuid().as_raw();
    let parent = current
        .path
        .parent()
        .ok_or_else(|| bad("History parent absent"))?;
    let directory = open_private_directory(parent, uid)?;
    let name = current
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| bad("History name invalid"))?;
    let file = open_at_private(&directory, name, uid, FIRMWARE_JOURNAL_LIMIT, false)?;
    let before = Stamp::of(&file.metadata().map_err(bad)?);
    if before.device != current.device
        || before.inode != current.inode
        || before.bytes != current.bytes
    {
        return Err(bad("Current history changed during prefix inspection"));
    }
    let digest = hash_prefix(&file, reference.bytes)?;
    settle_named(&file, &directory, name, before, uid)?;
    if digest != reference.sha256 {
        return Err(bad("Retained original predecessor prefix was modified"));
    }
    Ok(())
}

fn scan_frames(
    file: &File,
    bytes: u64,
    magic: &[u8; 8],
    first_record_limit: usize,
    record_limit: usize,
    frame_limit: u64,
    mut consume: impl FnMut(u64, &[u8]) -> Result<(), ServiceError>,
) -> Result<(String, u64), ServiceError> {
    let mut offset = 0u64;
    let mut frames = 0u64;
    let mut hash = Sha256::new();
    while offset < bytes {
        let mut header = [0u8; 44];
        if frames >= frame_limit || bytes - offset < 44 {
            return Err(bad("Partial or excessive history frame header"));
        }
        file.read_exact_at(&mut header, offset).map_err(bad)?;
        let length =
            u32::from_le_bytes(header[8..12].try_into().expect("four-byte frame extent")) as usize;
        let allowed = if frames == 0 {
            first_record_limit
        } else {
            record_limit
        };
        if &header[..8] != magic
            || length == 0
            || length > allowed
            || 44u64
                .checked_add(length as u64)
                .is_none_or(|n| n > bytes - offset)
        {
            return Err(bad("History magic, extent or record budget differs"));
        }
        let mut body = vec![0; length];
        file.read_exact_at(&mut body, offset + 44).map_err(bad)?;
        if Sha256::digest(&body).as_slice() != &header[12..44] {
            return Err(bad("History frame checksum differs"));
        }
        consume(frames, &body)?;
        hash.update(header);
        hash.update(&body);
        offset += 44 + length as u64;
        frames += 1;
    }
    if frames == 0 {
        return Err(bad("History has no complete frames"));
    }
    Ok((format!("{:x}", hash.finalize()), frames))
}
fn hash_prefix(file: &File, bytes: u64) -> Result<String, ServiceError> {
    let mut hash = Sha256::new();
    let mut offset = 0u64;
    let mut buffer = [0u8; 65536];
    while offset < bytes {
        let length = (bytes - offset).min(buffer.len() as u64) as usize;
        file.read_exact_at(&mut buffer[..length], offset)
            .map_err(bad)?;
        hash.update(&buffer[..length]);
        offset += length as u64;
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn plain_absolute(path: &Path) -> Result<(), ServiceError> {
    if !path.is_absolute()
        || path == Path::new("/")
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(bad("History path must be a plain absolute local path"));
    }
    Ok(())
}
fn open_private_directory(path: &Path, uid: u32) -> Result<File, ServiceError> {
    open_history_directory(path, uid, true)
}
fn open_history_directory(path: &Path, uid: u32, private: bool) -> Result<File, ServiceError> {
    plain_absolute(path)?;
    let mut directory: File = fs::open("/", DIRECTORY, Mode::empty()).map_err(bad)?.into();
    for component in path.components() {
        let name = match component {
            Component::RootDir => continue,
            Component::Normal(name) => name,
            _ => return Err(bad("Invalid history ancestor")),
        };
        directory = fs::openat(&directory, name, DIRECTORY, Mode::empty())
            .map_err(bad)?
            .into();
        let stat = fs::fstat(&directory).map_err(bad)?;
        let protected_temporary = stat.st_uid == 0 && stat.st_mode & 0o1000 != 0;
        if FileType::from_raw_mode(stat.st_mode) != FileType::Directory
            || (stat.st_uid != uid && stat.st_uid != 0)
            || (stat.st_mode & 0o022 != 0 && !protected_temporary)
        {
            return Err(bad("History ancestor owner, permissions or type is unsafe"));
        }
    }
    let stat = fs::fstat(&directory).map_err(bad)?;
    if stat.st_uid != uid
        || (private && stat.st_mode & 0o7777 != 0o700)
        || stat.st_mode & 0o022 != 0
    {
        return Err(bad("History directory must be private and owned"));
    }
    Ok(directory)
}
fn check_directory_identity(directory: &File, path: &Path, uid: u32) -> Result<(), ServiceError> {
    let named = open_private_directory(path, uid)?;
    let actual = fs::fstat(directory).map_err(bad)?;
    let current = fs::fstat(&named).map_err(bad)?;
    if actual.st_dev != current.st_dev || actual.st_ino != current.st_ino {
        return Err(bad("Named history directory was replaced"));
    }
    Ok(())
}
fn open_at_private(
    directory: &File,
    name: &str,
    uid: u32,
    limit: u64,
    allow_empty: bool,
) -> Result<File, ServiceError> {
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return Err(bad("Invalid local history child"));
    }
    let file: File = fs::openat(directory, name, READ, Mode::empty())
        .map_err(bad)?
        .into();
    let stat = fs::fstat(&file).map_err(bad)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_uid != uid
        || stat.st_mode & 0o7777 != 0o600
        || stat.st_nlink != 1
        || stat.st_size < 0
        || stat.st_size as u64 > limit
        || (!allow_empty && stat.st_size == 0)
    {
        return Err(bad(
            "History file owner, mode, link count, type or extent differs",
        ));
    }
    Ok(file)
}
fn settle_named(
    file: &File,
    directory: &File,
    name: &str,
    before: Stamp,
    uid: u32,
) -> Result<(), ServiceError> {
    let after = Stamp::of(&file.metadata().map_err(bad)?);
    let named = open_at_private(directory, name, uid, FIRMWARE_JOURNAL_LIMIT, true)?;
    if after != before || Stamp::of(&named.metadata().map_err(bad)?) != before {
        return Err(bad("History changed or was replaced while reading"));
    }
    Ok(())
}

pub(crate) fn catalogue_registry(
    directory: &File,
    registry_directory: &Path,
    library_root: &Path,
) -> Result<crate::LinuxFirmwareHistoryCatalogue, ServiceError> {
    use crate::{LinuxFirmwareHistoryCatalogue, LinuxFirmwareHistoryRow};
    let uid = rustix::process::geteuid().as_raw();
    check_directory_identity(directory, registry_directory, uid)?;
    let _root = open_history_directory(library_root, uid, false)?;
    let mut catalogue = LinuxFirmwareHistoryCatalogue {
        schema: 1,
        rows: vec![],
        diagnostics: vec![],
        resume_supported: false,
        physical_authority: false,
    };
    let mut count = 0;
    let mut total = 0u64;
    for item in
        std::fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd())).map_err(bad)?
    {
        count += 1;
        if count > MAX_ATTEMPTS {
            return Err(bad("History catalogue entry cap"));
        }
        let name = item.map_err(bad)?.file_name();
        let name = name.to_str().ok_or_else(|| bad("Non-UTF8 history entry"))?;
        let result = (|| {
            if name == "registry.lock" || valid_physical_lock_name(name) {
                let file = open_at_private(directory, name, uid, REGISTRY_FILE_LIMIT, true)?;
                settle_named(
                    &file,
                    directory,
                    name,
                    Stamp::of(&file.metadata().map_err(bad)?),
                    uid,
                )?;
                return Ok(None);
            }
            let token = name
                .strip_suffix(".quarantine")
                .ok_or_else(|| bad("Unknown or pending retained registry entry"))?;
            let attempt = canonical_attempt(token)?;
            let history = read_registry(directory, registry_directory, name, uid)?;
            let state = history.last_state;
            if history.reservation.attempt != attempt {
                return Err(bad("History filename differs from reservation"));
            }
            // Read only this canonical local library child, never metadata paths.
            let journal = library_root
                .join("linux-firmware-completion")
                .join(format!("{}.journal", attempt.as_str()));
            let extent = std::fs::symlink_metadata(&journal).map_err(bad)?.len();
            total = total
                .checked_add(history.file.description.bytes)
                .and_then(|n| n.checked_add(extent))
                .ok_or_else(|| bad("History catalogue extent overflows"))?;
            if total > FIRMWARE_JOURNAL_LIMIT {
                return Err(bad("History catalogue combined extent cap"));
            }
            let resolved = match state {
                2 | 5 => true,
                6 | 7 => crate::firmware_quarantine::linked_attempt_closed(
                    directory, uid, &attempt, false,
                )?,
                _ => false,
            };
            let p = read_predecessor(history, library_root, uid)?;
            let purpose = if p.ancestors.is_empty() {
                "captured-normal-linux-update"
            } else {
                RECOVERY_PURPOSE
            };
            let closure_status = match (state, resolved) {
                (2, true) => "creating owner zero calls",
                (5, true) => "completed local Linux update",
                (6 | 7, true) => "complete linked local recovery",
                (6 | 7, false) => "incomplete linked closure; held",
                _ => "unresolved local attempt; held",
            };
            Ok(Some(LinuxFirmwareHistoryRow {
                attempt: p.attempt.clone(),
                purpose: purpose.into(),
                registry: p.registry.description,
                journal: p.journal.description,
                local_state: state,
                resolved,
                source_review_sha256: p.source_review_sha256,
                original_association: p.original_binding,
                predecessors: p.ancestors,
                historical_capture: p.historical_capture,
                closure_status: closure_status.into(),
                resume_supported: false,
                physical_authority: false,
            }))
        })();
        match result {
            Ok(Some(row)) => catalogue.rows.push(row),
            Ok(None) => {}
            Err(error) => catalogue
                .diagnostics
                .push(format!("{name}: {}", error.message)),
        }
    }
    check_directory_identity(directory, registry_directory, uid)?;
    catalogue.rows.sort_by(|a, b| a.attempt.cmp(&b.attempt));
    Ok(catalogue)
}
pub(crate) fn catalogue_at(
    root: &Path,
    registry_directory: &Path,
) -> Result<crate::LinuxFirmwareHistoryCatalogue, ServiceError> {
    let directory =
        open_private_directory(registry_directory, rustix::process::geteuid().as_raw())?;
    catalogue_registry(&directory, registry_directory, root)
}
