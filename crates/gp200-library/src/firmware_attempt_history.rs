//! Bounded original-byte firmware history. No export is a resume or flash permit.
use crate::{
    ContentHash, FirmwareAttemptLedger, FirmwareRetainedAttempt, LibraryStore,
    store::{invalid, io, nonce},
};
use gp200_domain::{
    ErrorKind, FIRMWARE_JOURNAL_LIMIT, FirmwareAttemptId, ReviewedFirmwareUpdate, ServiceError,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(target_os = "linux")]
use std::os::{
    fd::AsRawFd,
    unix::fs::{MetadataExt, OpenOptionsExt},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};
const PAGE: usize = 32;
const FILES: usize = 1100;
const MANIFEST: u64 = 2 * 1024 * 1024;
const MATERIAL: u64 = 128 * 1024 * 1024;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareHistoryCursor {
    pub roster_sha256: ContentHash,
    pub offset: usize,
}
#[derive(Debug, Clone)]
pub struct FirmwareHistoryPage {
    pub attempts: Vec<FirmwareAttemptId>,
    pub total: usize,
    pub roster_sha256: ContentHash,
    pub next: Option<FirmwareHistoryCursor>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareHistoryFileIdentity {
    pub bytes: u64,
    pub device: u64,
    pub inode: u64,
    pub modified: i64,
    pub modified_ns: i64,
    pub changed: i64,
    pub changed_ns: i64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareHistoryFilePin {
    pub path: String,
    pub bytes: u64,
    pub sha256: ContentHash,
    pub identity: FirmwareHistoryFileIdentity,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareHistoryPrefix {
    pub accepted_records: u64,
    pub accepted_bytes: u64,
    pub prefix_sha256: Option<ContentHash>,
    pub source_snapshot_sha256: ContentHash,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirmwareHistoryExportKind {
    CheckedObservation,
    RawDiagnostics,
}
#[derive(Debug, Clone)]
pub struct FirmwareHistoryDetail {
    pub attempt: FirmwareAttemptId,
    pub checked: Option<Arc<FirmwareRetainedAttempt>>,
    pub load_error: Option<String>,
    pub files: Vec<FirmwareHistoryFilePin>,
    pub prefix: FirmwareHistoryPrefix,
    pub kind: FirmwareHistoryExportKind,
    pub issues: Vec<String>,
    pub omitted_issues: usize,
}
impl FirmwareHistoryDetail {
    pub fn can_resume(&self) -> bool {
        false
    }
    pub fn permits_flash(&self) -> bool {
        false
    }
}
#[derive(Debug, Clone)]
pub struct FirmwareHistoryExportReceipt {
    pub attempt: FirmwareAttemptId,
    pub destination: PathBuf,
    pub archive_sha256: ContentHash,
    pub archive_bytes: u64,
    pub source_snapshot_sha256: ContentHash,
    pub files: usize,
    pub kind: FirmwareHistoryExportKind,
}
impl FirmwareHistoryExportReceipt {
    pub fn can_resume(&self) -> bool {
        false
    }
    pub fn permits_flash(&self) -> bool {
        false
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ancestor {
    path: PathBuf,
    device: u64,
    inode: u64,
}
pub(crate) struct CheckedFile {
    pub(crate) file: File,
    path: PathBuf,
    identity: FirmwareHistoryFileIdentity,
    ancestors: Vec<Ancestor>,
    _directories: Vec<File>,
}
fn file_identity(m: &Metadata) -> Result<FirmwareHistoryFileIdentity, ServiceError> {
    if !m.is_file() || m.file_type().is_symlink() {
        return Err(invalid("History source must be a regular no-follow file"));
    }
    #[cfg(target_os = "linux")]
    {
        Ok(FirmwareHistoryFileIdentity {
            bytes: m.len(),
            device: m.dev(),
            inode: m.ino(),
            modified: m.mtime(),
            modified_ns: m.mtime_nsec(),
            changed: m.ctime(),
            changed_ns: m.ctime_nsec(),
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = m;
        Err(ServiceError::new(
            ErrorKind::Unsupported,
            "Checked no-follow history is unavailable on this platform",
        ))
    }
}
fn ancestors(path: &Path) -> Result<Vec<Ancestor>, ServiceError> {
    if !path.is_absolute() {
        return Err(invalid("History path must be absolute"));
    }
    let mut rows = vec![];
    let mut current = PathBuf::new();
    for c in path.components() {
        match c {
            Component::RootDir => current.push(c),
            Component::Normal(v) => current.push(v),
            _ => return Err(invalid("History path contains traversal")),
        };
        let m = fs::symlink_metadata(&current).map_err(io)?;
        if !m.is_dir() || m.file_type().is_symlink() {
            return Err(invalid("History ancestor must be a real directory"));
        }
        #[cfg(target_os = "linux")]
        rows.push(Ancestor {
            path: current.clone(),
            device: m.dev(),
            inode: m.ino(),
        });
        #[cfg(not(target_os = "linux"))]
        return Err(ServiceError::new(
            ErrorKind::Unsupported,
            "No-follow ancestors unavailable",
        ));
    }
    Ok(rows)
}
// Open every directory through the retained parent fd. O_NOFOLLOW on the final
// source alone would still follow a raced ancestor symlink.
fn directory_handles(path: &Path) -> Result<Vec<File>, ServiceError> {
    #[cfg(target_os = "linux")]
    {
        let rows = ancestors(path)?;
        let mut handles = vec![];
        for (index, row) in rows.iter().enumerate() {
            let target = if index == 0 {
                PathBuf::from("/")
            } else {
                PathBuf::from(format!(
                    "/proc/self/fd/{}/{}",
                    handles
                        .last()
                        .map(|f: &File| f.as_raw_fd())
                        .expect("retained parent"),
                    row.path
                        .file_name()
                        .expect("normal ancestor")
                        .to_string_lossy()
                ))
            };
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(0x10000 | 0x20000)
                .open(target)
                .map_err(io)?;
            let m = file.metadata().map_err(io)?;
            if !m.is_dir() || m.dev() != row.device || m.ino() != row.inode {
                return Err(invalid(
                    "History directory identity changed during anchored open",
                ));
            }
            handles.push(file);
        }
        Ok(handles)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Err(ServiceError::new(
            ErrorKind::Unsupported,
            "Anchored no-follow directories unavailable",
        ))
    }
}
pub(crate) fn checked_directory_entries(
    path: &Path,
    limit: usize,
) -> Result<Vec<PathBuf>, ServiceError> {
    #[cfg(target_os = "linux")]
    {
        let before = ancestors(path)?;
        let handles = directory_handles(path)?;
        let file = handles.last().expect("retained directory");
        let metadata = file.metadata().map_err(io)?;
        let target = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        let mut result = vec![];
        for row in fs::read_dir(target).map_err(io)? {
            if result.len() >= limit {
                return Err(invalid("History directory entry bound exceeded"));
            }
            result.push(path.join(row.map_err(io)?.file_name()));
        }
        let after = file.metadata().map_err(io)?;
        if (
            metadata.dev(),
            metadata.ino(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        ) != (
            after.dev(),
            after.ino(),
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec(),
        ) || ancestors(path)? != before
        {
            return Err(invalid("History directory changed during pinned listing"));
        }
        Ok(result)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (path, limit);
        Err(ServiceError::new(
            ErrorKind::Unsupported,
            "Pinned directory listing unavailable",
        ))
    }
}
fn child_path(handles: &[File], name: &std::ffi::OsStr) -> Result<PathBuf, ServiceError> {
    #[cfg(target_os = "linux")]
    {
        let fd = handles
            .last()
            .ok_or_else(|| invalid("Anchored parent absent"))?
            .as_raw_fd();
        Ok(PathBuf::from(format!("/proc/self/fd/{fd}")).join(name))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (handles, name);
        Err(ServiceError::new(
            ErrorKind::Unsupported,
            "Anchored paths unavailable",
        ))
    }
}
pub(crate) fn checked_open(path: &Path, limit: u64) -> Result<CheckedFile, ServiceError> {
    let parents = ancestors(
        path.parent()
            .ok_or_else(|| invalid("History parent missing"))?,
    )?;
    let before = file_identity(&fs::symlink_metadata(path).map_err(io)?)?;
    if before.bytes > limit {
        return Err(invalid("History source exceeds bounded length"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "linux")]
    options.custom_flags(0x20000); // Linux O_NOFOLLOW; safe standard-library open.
    #[cfg(not(target_os = "linux"))]
    return Err(ServiceError::new(
        ErrorKind::Unsupported,
        "No-follow reads unavailable",
    ));
    let directories = directory_handles(path.parent().expect("checked parent"))?;
    let file = options
        .open(child_path(
            &directories,
            path.file_name()
                .ok_or_else(|| invalid("Source filename absent"))?,
        )?)
        .map_err(io)?;
    if file_identity(&file.metadata().map_err(io)?)? != before
        || file_identity(&fs::symlink_metadata(path).map_err(io)?)? != before
        || ancestors(path.parent().expect("checked parent"))? != parents
    {
        return Err(invalid("History source identity changed at open"));
    }
    Ok(CheckedFile {
        file,
        path: path.into(),
        identity: before,
        ancestors: parents,
        _directories: directories,
    })
}
impl CheckedFile {
    pub(crate) fn recheck(&self) -> Result<(), ServiceError> {
        if file_identity(&self.file.metadata().map_err(io)?)? != self.identity
            || file_identity(&fs::symlink_metadata(&self.path).map_err(io)?)? != self.identity
            || ancestors(self.path.parent().expect("checked parent"))? != self.ancestors
        {
            return Err(invalid("History source changed during read"));
        }
        Ok(())
    }
}
pub(crate) fn checked_bytes(path: &Path, limit: u64) -> Result<Vec<u8>, ServiceError> {
    let mut source = checked_open(path, limit)?;
    let mut bytes = vec![];
    std::io::Read::by_ref(&mut source.file)
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() as u64 != source.identity.bytes {
        return Err(invalid("History bounded file length changed"));
    }
    source.recheck()?;
    Ok(bytes)
}
fn hash_file(
    path: &Path,
    limit: u64,
    cancelled: &dyn Fn() -> bool,
) -> Result<(FirmwareHistoryFileIdentity, ContentHash), ServiceError> {
    let mut source = checked_open(path, limit)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    let mut read = 0u64;
    loop {
        check_cancel(cancelled)?;
        let n = source.file.read(&mut buffer).map_err(io)?;
        if n == 0 {
            break;
        }
        read += n as u64;
        if read > limit {
            return Err(invalid("History stream grew beyond bound"));
        }
        hash.update(&buffer[..n]);
    }
    source.recheck()?;
    if read != source.identity.bytes {
        return Err(invalid("History source length differs"));
    }
    Ok((
        source.identity,
        ContentHash::new(format!("{:x}", hash.finalize()))?,
    ))
}
fn source_limit(path: &str) -> u64 {
    if path.ends_with(".journal") {
        FIRMWARE_JOURNAL_LIMIT
    } else if path.starts_with("blobs/") {
        64 * 1024 * 1024
    } else {
        16 * 1024
    }
}
fn check_cancel(cancelled: &dyn Fn() -> bool) -> Result<(), ServiceError> {
    if cancelled() {
        Err(ServiceError::new(
            ErrorKind::Cancelled,
            "History operation stopped; authority originals remain unchanged",
        ))
    } else {
        Ok(())
    }
}
pub(crate) fn list_history(
    store: &LibraryStore,
    cursor: Option<FirmwareHistoryCursor>,
    limit: usize,
) -> Result<FirmwareHistoryPage, ServiceError> {
    if limit == 0 || limit > PAGE {
        return Err(invalid("History page limit must be 1 through 32"));
    }
    let ids = FirmwareAttemptLedger::history_roster(store)?;
    let hash = ContentHash::of(&serde_json::to_vec(&ids).map_err(invalid)?);
    let offset = if let Some(c) = cursor {
        if c.roster_sha256 != hash || c.offset > ids.len() {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "History roster changed; request a fresh page",
            ));
        }
        c.offset
    } else {
        0
    };
    let end = offset.saturating_add(limit).min(ids.len());
    Ok(FirmwareHistoryPage {
        attempts: ids[offset..end].to_vec(),
        total: ids.len(),
        roster_sha256: hash.clone(),
        next: (end < ids.len()).then_some(FirmwareHistoryCursor {
            roster_sha256: hash,
            offset: end,
        }),
    })
}
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct Manifest {
    format: String,
    attempt: FirmwareAttemptId,
    kind: FirmwareHistoryExportKind,
    source_snapshot_sha256: ContentHash,
    files: Vec<FirmwareHistoryFilePin>,
    accepted_records: u64,
    accepted_bytes: u64,
    prefix_sha256: Option<ContentHash>,
    load_error: Option<String>,
    issues: Vec<String>,
    omitted_issues: usize,
    recorded_legacy_closure: Option<String>,
    derived_reopen_state: Option<String>,
    lifecycle_host_closure: Option<String>,
    origin: Option<String>,
    local_checkpoint_confirmed: bool,
    possible_device_update_mode: Option<bool>,
    can_resume: bool,
    permits_flash: bool,
    device_verified_bytes: u64,
    completion_verified: bool,
}
fn manifest(detail: &FirmwareHistoryDetail) -> Manifest {
    let checked = detail.checked.as_ref();
    let projection = checked.and_then(|c| c.projection.as_ref());
    let lifecycle = checked.and_then(|c| c.lifecycle.as_ref());
    Manifest {
        format: "gp200-firmware-history-observation-v1".into(),
        attempt: detail.attempt.clone(),
        kind: detail.kind.clone(),
        source_snapshot_sha256: detail.prefix.source_snapshot_sha256.clone(),
        files: detail.files.clone(),
        accepted_records: detail.prefix.accepted_records,
        accepted_bytes: detail.prefix.accepted_bytes,
        prefix_sha256: detail.prefix.prefix_sha256.clone(),
        load_error: detail.load_error.clone(),
        issues: detail.issues.clone(),
        omitted_issues: detail.omitted_issues,
        recorded_legacy_closure: projection
            .and_then(|p| p.recorded_closure())
            .map(|v| format!("{v:?}")),
        derived_reopen_state: projection.map(|p| format!("{:?}", p.state())),
        lifecycle_host_closure: lifecycle
            .and_then(|p| p.closure())
            .map(|v| format!("{v:?}")),
        origin: projection.map(|p| format!("{:?}", p.origin())),
        local_checkpoint_confirmed: checked.is_some_and(|c| c.local_checkpoint_confirmed),
        possible_device_update_mode: checked.map(|c| c.possible_device_update_mode),
        can_resume: false,
        permits_flash: false,
        device_verified_bytes: 0,
        completion_verified: false,
    }
}
pub(crate) fn inspect_attempt(
    store: &mut LibraryStore,
    id: &FirmwareAttemptId,
) -> Result<FirmwareHistoryDetail, ServiceError> {
    collect(store, id, &|| false)
}
fn collect(
    store: &mut LibraryStore,
    id: &FirmwareAttemptId,
    cancelled: &dyn Fn() -> bool,
) -> Result<FirmwareHistoryDetail, ServiceError> {
    check_cancel(cancelled)?;
    let mut paths = FirmwareAttemptLedger::history_paths(store, id)?;
    let journal = store
        .root
        .join("firmware-attempt-ledger")
        .join(format!("{}.journal", id.as_str()));
    let checked = FirmwareAttemptLedger::history_load(store, id);
    let (checked, load_error) = match checked {
        Ok(v) => (Some(Arc::new(v)), None),
        Err(e) => (None, Some(e.message.chars().take(4096).collect())),
    };
    let mut issues = vec![];
    if let Ok(Some(review)) = FirmwareAttemptLedger::history_first_review(&journal, id) {
        if crate::store::valid_hash(&review) {
            let preimage = store.root.join("blobs").join(&review);
            if preimage.exists() {
                paths.push(preimage.clone());
                match checked_bytes(&preimage, MANIFEST) {
                    Ok(raw) => {
                        if ContentHash::of(&raw).as_str() != review {
                            issues
                                .push("Original preimage hash differs from recorded claim".into());
                        }
                        match ReviewedFirmwareUpdate::retained_pins(&raw) {
                            Ok(pins) => {
                                for pin in pins {
                                    let path = store.root.join("blobs").join(&pin.sha256);
                                    if path.exists() {
                                        paths.push(path);
                                    } else {
                                        issues.push(format!(
                                            "Referenced material {} unavailable",
                                            pin.sha256
                                        ));
                                    }
                                }
                            }
                            Err(e) => issues.push(format!(
                                "Retained preimage cannot enumerate materials: {}",
                                e.message
                            )),
                        }
                    }
                    Err(e) => issues.push(e.message),
                }
            } else {
                issues.push("Recorded preimage is unavailable".into());
            }
        } else {
            issues.push("Recorded preimage hash is invalid".into());
        }
    } else {
        issues.push("Original first Prepared frame or review claim unavailable".into());
    }
    paths.sort();
    paths.dedup();
    if paths.len() + 1 > FILES {
        return Err(invalid("History original entry count exceeds 1100"));
    }
    let mut files = vec![];
    let mut material = 0u64;
    let mut metadata_bytes = 0u64;
    for path in paths {
        check_cancel(cancelled)?;
        let name = path
            .strip_prefix(&store.root)
            .map_err(invalid)?
            .to_str()
            .ok_or_else(|| invalid("History source path is not UTF-8"))?
            .to_owned();
        let (identity, hash) = hash_file(&path, source_limit(&name), cancelled)?;
        if name.starts_with("blobs/") {
            material = material
                .checked_add(identity.bytes)
                .ok_or_else(|| invalid("History material length overflow"))?;
            if material > MATERIAL {
                return Err(invalid("History material exceeds 128 MiB"));
            }
        }
        if name.starts_with("blobs/")
            && hash.as_str() != path.file_name().and_then(|n| n.to_str()).unwrap_or("")
        {
            issues.push(format!(
                "Original material {} disagrees with content-address claim",
                name
            ));
        }
        if !name.starts_with("blobs/") && !name.ends_with(".journal") {
            metadata_bytes = metadata_bytes
                .checked_add(identity.bytes)
                .ok_or_else(|| invalid("History metadata size overflow"))?;
            if metadata_bytes > 16 * 1024 * 1024 {
                return Err(invalid("History small metadata exceeds 16 MiB"));
            }
        }
        files.push(FirmwareHistoryFilePin {
            path: name,
            bytes: identity.bytes,
            sha256: hash,
            identity,
        });
    }
    let (records, bytes, prefix) = checked.as_ref().map_or((0, 0, None), |v| {
        (
            v.accepted_records,
            v.valid_prefix_bytes,
            ContentHash::new(&v.prefix_sha256).ok(),
        )
    });
    if let Some(c) = &checked {
        issues.extend(c.issues.iter().cloned());
    }
    let omitted_issues = issues.len().saturating_sub(64);
    issues.truncate(64);
    for issue in &mut issues {
        *issue = issue.chars().take(4096).collect();
    }
    let mut detail = FirmwareHistoryDetail {
        attempt: id.clone(),
        checked,
        load_error,
        files,
        prefix: FirmwareHistoryPrefix {
            accepted_records: records,
            accepted_bytes: bytes,
            prefix_sha256: prefix,
            source_snapshot_sha256: ContentHash::of(&[]),
        },
        kind: FirmwareHistoryExportKind::RawDiagnostics,
        issues,
        omitted_issues,
    };
    if detail
        .checked
        .as_ref()
        .is_some_and(|v| v.accepted_records != 0)
    {
        detail.kind = FirmwareHistoryExportKind::CheckedObservation;
    }
    // Snapshot binds the complete original hashes/identities, raw bad tail through whole journal,
    // sidecars/material, semantic error, closure/reopen provenance and observation mode.
    let raw = serde_json::to_vec(&manifest(&detail)).map_err(invalid)?;
    if raw.len() as u64 > MANIFEST {
        return Err(invalid("History snapshot manifest exceeds 2 MiB"));
    }
    detail.prefix.source_snapshot_sha256 = ContentHash::of(&raw);
    Ok(detail)
}
fn checked_destination(
    store: &LibraryStore,
    destination: &Path,
) -> Result<(PathBuf, Vec<Ancestor>), ServiceError> {
    let name = destination
        .file_name()
        .and_then(|v| v.to_str())
        .filter(|v| !v.is_empty() && v.len() <= 255 && *v != "." && *v != "..")
        .ok_or_else(|| invalid("History destination filename invalid"))?;
    let parent = destination
        .parent()
        .ok_or_else(|| invalid("History destination parent absent"))?;
    let rows = ancestors(parent)?;
    let root_rows = ancestors(&store.root)?;
    let root = root_rows.last().expect("canonical library root");
    if parent.starts_with(&store.root)
        || rows
            .iter()
            .any(|a| a.device == root.device && a.inode == root.inode)
    {
        return Err(invalid(
            "History export cannot target the library or its authority aliases",
        ));
    }
    let destination = parent.join(name);
    if fs::symlink_metadata(&destination).is_ok() {
        return Err(ServiceError::new(
            ErrorKind::Conflict,
            "History export is create-new; destination already exists",
        ));
    }
    Ok((destination, rows))
}
pub(crate) fn export_attempt(
    store: &mut LibraryStore,
    id: &FirmwareAttemptId,
    expected: &FirmwareHistoryPrefix,
    destination: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<FirmwareHistoryExportReceipt, ServiceError> {
    let detail = collect(store, id, cancelled)?;
    if &detail.prefix != expected {
        return Err(ServiceError::new(
            ErrorKind::Conflict,
            "History originals or rejected tail changed; review again",
        ));
    }
    let (destination, parent_identity) = checked_destination(store, destination)?;
    let parent = destination.parent().expect("checked parent");
    let manifest = serde_json::to_vec(&manifest(&detail)).map_err(invalid)?;
    if manifest.len() as u64 > MANIFEST {
        return Err(invalid("History manifest too large"));
    }
    let bytes = detail
        .files
        .iter()
        .try_fold(manifest.len() as u64, |n, p| n.checked_add(p.bytes + 4096))
        .ok_or_else(|| invalid("History export bound overflow"))?;
    boundary("before-space-check")?;
    if fs2::available_space(parent).map_err(io)? < bytes.saturating_add(1024 * 1024) {
        return Err(ServiceError::new(
            ErrorKind::Io,
            "Insufficient free space for all original evidence",
        ));
    }
    let partial = parent.join(format!(".gp200-history-{}.partial", nonce()?));
    let handles = directory_handles(parent)?;
    let anchored_partial = child_path(&handles, partial.file_name().expect("private filename"))?;
    let anchored_destination =
        child_path(&handles, destination.file_name().expect("checked filename"))?;
    let result = (|| {
        check_cancel(cancelled)?;
        boundary("before-create")?;
        let mut opts = OpenOptions::new();
        opts.write(true).read(true).create_new(true);
        #[cfg(target_os = "linux")]
        opts.mode(0o600).custom_flags(0x20000);
        let file = opts.open(&anchored_partial).map_err(io)?;
        let mut writer = ZipWriter::new(file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        for pin in &detail.files {
            check_cancel(cancelled)?;
            let mut source = checked_open(&store.root.join(&pin.path), source_limit(&pin.path))?;
            if source.identity != pin.identity {
                return Err(invalid("History original identity changed before export"));
            }
            writer
                .start_file(format!("original/{}", pin.path), options)
                .map_err(invalid)?;
            let mut hash = Sha256::new();
            let mut count = 0u64;
            let mut buffer = [0; 65536];
            loop {
                check_cancel(cancelled)?;
                let n = source.file.read(&mut buffer).map_err(io)?;
                if n == 0 {
                    break;
                }
                count += n as u64;
                if count > pin.bytes {
                    return Err(invalid("History original grew during export"));
                }
                writer.write_all(&buffer[..n]).map_err(io)?;
                hash.update(&buffer[..n]);
                boundary("during-write")?;
            }
            source.recheck()?;
            if count != pin.bytes || format!("{:x}", hash.finalize()) != pin.sha256.as_str() {
                return Err(invalid("History streamed original differs"));
            }
        }
        writer
            .start_file("observation.json", options)
            .map_err(invalid)?;
        writer.write_all(&manifest).map_err(io)?;
        let file = writer.finish().map_err(invalid)?;
        boundary("after-finish")?;
        file.sync_all().map_err(io)?;
        boundary("after-file-sync")?;
        let synced_identity = file_identity(&file.metadata().map_err(io)?)?;
        #[cfg(test)]
        substitute_synced_partial(&partial)?;
        let (hash, length) = verify_export(&partial, &detail, &manifest, cancelled, &file)?;
        boundary("after-private-reread")?;
        if ancestors(parent)? != parent_identity {
            return Err(invalid("History destination ancestor changed"));
        }
        check_cancel(cancelled)?;
        boundary("before-publication")?;
        fs::hard_link(&anchored_partial, &anchored_destination).map_err(io)?;
        boundary("after-publication")?;
        handles
            .last()
            .expect("retained parent")
            .sync_all()
            .map_err(io)?;
        boundary("after-directory-sync")?;
        let (final_hash, final_length) =
            verify_export(&destination, &detail, &manifest, cancelled, &file)?;
        let final_identity = file_identity(&file.metadata().map_err(io)?)?;
        if (
            final_identity.device,
            final_identity.inode,
            final_identity.bytes,
            final_identity.modified,
            final_identity.modified_ns,
        ) != (
            synced_identity.device,
            synced_identity.inode,
            synced_identity.bytes,
            synced_identity.modified,
            synced_identity.modified_ns,
        ) {
            return Err(invalid(
                "Synced output inode or data identity changed during publication",
            ));
        }
        boundary("after-final-reread")?;
        if hash != final_hash
            || length != final_length
            || ancestors(parent)? != parent_identity
            || collect(store, id, cancelled)?.prefix != detail.prefix
        {
            return Err(invalid("History final bytes or original snapshot changed"));
        }
        // Successful publication keeps its private exact copy as retained local evidence too.
        Ok(FirmwareHistoryExportReceipt {
            attempt: id.clone(),
            destination: destination.clone(),
            archive_sha256: final_hash,
            archive_bytes: final_length,
            source_snapshot_sha256: detail.prefix.source_snapshot_sha256.clone(),
            files: detail.files.len() + 1,
            kind: detail.kind.clone(),
        })
    })();
    result.map_err(|mut error| {
        error.message = format!(
            "{}; any visible output is unverified; retained partial: {}; destination: {}",
            error.message,
            partial.display(),
            destination.display()
        );
        error
    })
}
fn verify_export(
    path: &Path,
    detail: &FirmwareHistoryDetail,
    manifest: &[u8],
    cancelled: &dyn Fn() -> bool,
    synced: &File,
) -> Result<(ContentHash, u64), ServiceError> {
    let limit =
        FIRMWARE_JOURNAL_LIMIT + MATERIAL + 16 * 1024 * 1024 + MANIFEST + FILES as u64 * 8192;
    let mut checked = checked_open(path, limit)?;
    if checked.identity != file_identity(&synced.metadata().map_err(io)?)? {
        return Err(invalid(
            "Published alias differs from the actual synced output inode",
        ));
    }
    let mut digest = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0; 65536];
    loop {
        check_cancel(cancelled)?;
        let n = checked.file.read(&mut buffer).map_err(io)?;
        if n == 0 {
            break;
        }
        count += n as u64;
        if count > limit {
            return Err(invalid("History archive exceeds byte bound"));
        }
        digest.update(&buffer[..n]);
    }
    checked.recheck()?;
    if count != checked.identity.bytes {
        return Err(invalid("History archive length changed"));
    }
    let hash = ContentHash::new(format!("{:x}", digest.finalize()))?;
    checked.file.seek(SeekFrom::Start(0)).map_err(io)?;
    let mut archive = ZipArchive::new(checked.file.try_clone().map_err(io)?).map_err(invalid)?;
    if archive.len() != detail.files.len() + 1 || archive.len() > FILES {
        return Err(invalid("History archive entry count differs"));
    }
    let expected: BTreeMap<_, _> = detail
        .files
        .iter()
        .map(|p| (format!("original/{}", p.path), p))
        .collect();
    let mut seen = BTreeSet::new();
    for index in 0..archive.len() {
        check_cancel(cancelled)?;
        let mut entry = archive.by_index(index).map_err(invalid)?;
        let name = entry.name().to_owned();
        if !seen.insert(name.clone())
            || !entry.is_file()
            || entry.compression() != CompressionMethod::Stored
        {
            return Err(invalid("History archive entry shape differs"));
        }
        if name == "observation.json" {
            if entry.size() != manifest.len() as u64 {
                return Err(invalid("History manifest size differs"));
            }
            let mut raw = vec![];
            entry
                .by_ref()
                .take(MANIFEST + 1)
                .read_to_end(&mut raw)
                .map_err(io)?;
            if raw != manifest {
                return Err(invalid("History independently read manifest differs"));
            }
            let decoded: Manifest = serde_json::from_slice(&raw).map_err(invalid)?;
            if decoded.can_resume
                || decoded.permits_flash
                || decoded.completion_verified
                || decoded.device_verified_bytes != 0
            {
                return Err(invalid("History manifest contains physical authority"));
            }
        } else {
            let pin = expected
                .get(&name)
                .ok_or_else(|| invalid("History archive has unexpected original"))?;
            if entry.size() != pin.bytes {
                return Err(invalid("History archive original size differs"));
            }
            let mut h = Sha256::new();
            let mut count = 0u64;
            let mut buffer = [0; 65536];
            loop {
                let n = entry.read(&mut buffer).map_err(io)?;
                if n == 0 {
                    break;
                }
                count += n as u64;
                if count > pin.bytes {
                    return Err(invalid("History archive original exceeded bound"));
                }
                h.update(&buffer[..n]);
                check_cancel(cancelled)?;
            }
            if count != pin.bytes || format!("{:x}", h.finalize()) != pin.sha256.as_str() {
                return Err(invalid(
                    "History independently reread original hash differs",
                ));
            }
        }
    }
    checked.recheck()?;
    if checked.identity != file_identity(&synced.metadata().map_err(io)?)? {
        return Err(invalid(
            "Actual synced output identity changed during independent verification",
        ));
    }
    Ok((hash, checked.identity.bytes))
}
fn boundary(name: &str) -> Result<(), ServiceError> {
    #[cfg(test)]
    {
        FAULT.with(|f| {
            if f.borrow().as_deref() == Some(name) {
                Err(ServiceError::new(
                    ErrorKind::Io,
                    format!("Injected actual history boundary {name}"),
                ))
            } else {
                Ok(())
            }
        })
    }
    #[cfg(not(test))]
    {
        let _ = name;
        Ok(())
    }
}
#[cfg(test)]
thread_local! {static FAULT:std::cell::RefCell<Option<String>>=const{std::cell::RefCell::new(None)};}

#[cfg(test)]
thread_local! {static REPLACE_SYNCED:std::cell::Cell<bool>=const{std::cell::Cell::new(false)};}
#[cfg(test)]
fn substitute_synced_partial(path: &Path) -> Result<(), ServiceError> {
    if REPLACE_SYNCED.with(|f| f.replace(false)) {
        let raw = fs::read(path).map_err(io)?;
        fs::rename(path, path.with_extension("synced-retained")).map_err(io)?;
        fs::write(path, raw).map_err(io)?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (PathBuf, LibraryStore, FirmwareAttemptId) {
        let dir = std::env::temp_dir().join(format!("fw05a0-export-{}", nonce().unwrap()));
        fs::create_dir(&dir).unwrap();
        let store = LibraryStore::open(dir.join("library")).unwrap();
        let id = FirmwareAttemptId::parse("cccccccccccccccccccccccccccccccc").unwrap();
        let ledger = store.root.join("firmware-attempt-ledger");
        fs::create_dir(&ledger).unwrap();
        fs::write(
            ledger.join(format!("{}.journal", id.as_str())),
            b"original corrupt framing\0\xff\x7f",
        )
        .unwrap();
        (dir, store, id)
    }
    #[test]
    fn actual_write_sync_reread_and_publication_failures_retain_originals_and_never_issue_verified_receipts()
     {
        for stage in [
            "before-space-check",
            "before-create",
            "during-write",
            "after-finish",
            "after-file-sync",
            "after-private-reread",
            "before-publication",
            "after-publication",
            "after-directory-sync",
            "after-final-reread",
        ] {
            let (dir, mut store, id) = fixture();
            let detail = inspect_attempt(&mut store, &id).unwrap();
            let original = store.root.join(&detail.files[0].path);
            let before = fs::read(&original).unwrap();
            FAULT.with(|f| *f.borrow_mut() = Some(stage.into()));
            let output = dir.join("fault.zip");
            let result = export_attempt(&mut store, &id, &detail.prefix, &output, &|| false);
            FAULT.with(|f| f.borrow_mut().take());
            assert!(result.is_err(), "{stage}");
            assert_eq!(fs::read(&original).unwrap(), before);
            if matches!(
                stage,
                "after-publication" | "after-directory-sync" | "after-final-reread"
            ) {
                assert!(output.exists());
                assert!(result.unwrap_err().message.contains("unverified"));
            }
            drop(store);
            fs::remove_dir_all(dir).unwrap();
        }
    }
    #[test]
    fn byte_identical_unsynced_private_replacement_is_rejected_by_the_actual_retained_synced_inode()
    {
        let (dir, mut store, id) = fixture();
        let d = inspect_attempt(&mut store, &id).unwrap();
        REPLACE_SYNCED.with(|f| f.set(true));
        let output = dir.join("replacement.zip");
        let error = export_attempt(&mut store, &id, &d.prefix, &output, &|| false).unwrap_err();
        assert!(
            error.message.contains("actual synced output inode"),
            "{error:?}"
        );
        assert!(!output.exists());
        let retained = fs::read_dir(&dir)
            .unwrap()
            .map(|r| r.unwrap().path())
            .filter(|p| {
                p.extension()
                    .is_some_and(|v| v == "partial" || v == "synced-retained")
            })
            .collect::<Vec<_>>();
        assert_eq!(retained.len(), 2);
        assert_eq!(
            fs::read(&retained[0]).unwrap(),
            fs::read(&retained[1]).unwrap()
        );
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn cancellation_never_modifies_authority_and_reports_unverified_partial_if_writing_started() {
        let (dir, mut store, id) = fixture();
        let d = inspect_attempt(&mut store, &id).unwrap();
        let calls = std::cell::Cell::new(0);
        let output = dir.join("cancelled.zip");
        let error = export_attempt(&mut store, &id, &d.prefix, &output, &|| {
            let n = calls.get();
            calls.set(n + 1);
            n > 8
        })
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Cancelled);
        assert!(!output.exists());
        assert_eq!(inspect_attempt(&mut store, &id).unwrap().prefix, d.prefix);
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn oversized_sparse_journal_and_symlink_originals_fail_safe_without_reading_unbounded_content()
    {
        let (dir, mut store, id) = fixture();
        let path = store
            .root
            .join("firmware-attempt-ledger")
            .join(format!("{}.journal", id.as_str()));
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(FIRMWARE_JOURNAL_LIMIT + 1)
            .unwrap();
        assert!(inspect_attempt(&mut store, &id).is_err());
        fs::remove_file(&path).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", &path).unwrap();
            assert!(inspect_attempt(&mut store, &id).is_err());
        }
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn retained_directory_fd_prevents_a_swapped_ancestor_from_redirecting_the_read() {
        let dir = std::env::temp_dir().join(format!("fw05a0-anchored-{}", nonce().unwrap()));
        fs::create_dir(&dir).unwrap();
        let parent = dir.join("source");
        fs::create_dir(&parent).unwrap();
        fs::write(parent.join("raw"), b"original").unwrap();
        let mut checked = checked_open(&parent.join("raw"), 64).unwrap();
        fs::rename(&parent, dir.join("retained-source")).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::write(parent.join("raw"), b"replacement").unwrap();
        let mut raw = vec![];
        checked.file.read_to_end(&mut raw).unwrap();
        assert_eq!(raw, b"original");
        assert!(checked.recheck().is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
