//! Durable local firmware observations. This module never opens a device.
use crate::{
    LibraryStore,
    store::{digest, invalid, io, nonce, read_bounded, reject_symlink},
};
use gp200_domain::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

const MAGIC: &[u8; 8] = b"GP2FWL01";
const HEADER: usize = 44;
const MAX_FRAME: usize = 16 * 1024;
const MAX_ATTEMPTS: usize = 1024;
const MAX_RECENT: usize = 32;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: u16,
    attempt: FirmwareAttemptId,
    sequence: u64,
    previous: Option<String>,
    entry: Entry,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
enum Entry {
    Prepared {
        review: String,
        origin: FirmwareObservationOrigin,
    },
    Checkpoint(FirmwareBlockDescription),
    Send(FirmwareSendObservation),
    Callback(FirmwareCallbackDto),
    MockDecision(FirmwareBlockDescription),
    ModeBoundary {
        raw: Vec<u8>,
        note: String,
    },
    LifecyclePrepared(FirmwareLifecyclePlanDto),
    LifecycleEntryPrepared(FirmwareEntryObservationPlanDto),
    LifecycleEntryHostSettled(FirmwareEntryHostSettlement),
    LifecycleEpoch(Box<FirmwareLifecycleEpochObservation>),
    LifecycleCheckpoint(FirmwareLifecycleActionDescription),
    LifecycleRaw(FirmwareLifecycleRawObservation),
    LifecycleNegative(FirmwareLifecycleNegativeObservation),
    LifecycleSend(FirmwareLifecycleSendObservation),
    LifecycleDecision(FirmwareLifecycleDecision),
    LifecycleClosed(FirmwareLifecycleHostClosure),
    Fault(String),
    Closed(FirmwareHostClosure),
}
impl Entry {
    fn cap(&self) -> usize {
        match self {
            Self::Prepared { .. }
            | Self::LifecyclePrepared(_)
            | Self::LifecycleEntryPrepared(_) => 16 * 1024,
            Self::LifecycleEpoch(_) => FIRMWARE_LIFECYCLE_EPOCH_PAYLOAD_LIMIT,
            Self::LifecycleCheckpoint(_) => 4 * 1024,
            Self::LifecycleRaw(_) | Self::LifecycleNegative(_) => 8 * 1024,
            Self::LifecycleSend(_) => 1024,
            Self::LifecycleDecision(_) => 2 * 1024,
            Self::Checkpoint(_) | Self::Callback(_) => 4 * 1024,
            Self::Send(_) => 1024,
            Self::MockDecision(_) => 2 * 1024,
            _ => 8 * 1024,
        }
    }
    fn terminal(&self) -> bool {
        matches!(
            self,
            Self::Fault(_) | Self::Closed(_) | Self::LifecycleClosed(_) | Self::ModeBoundary { .. }
        )
    }
    fn label(&self) -> &'static str {
        match self {
            Self::Prepared { .. } => "Prepared",
            Self::Checkpoint(_) => "Local durability checkpoint",
            Self::Send(_) => "OS write observation",
            Self::Callback(_) => "Raw callback",
            Self::MockDecision(_) => "Explicit mock decision",
            Self::ModeBoundary { .. } => "Possible mode transition observation",
            Self::LifecyclePrepared(_) => "Lifecycle preparation",
            Self::LifecycleEntryPrepared(_) => "Entry observation preparation",
            Self::LifecycleEntryHostSettled(_) => {
                "Entry host offer settled; device effect unverified"
            }
            Self::LifecycleEpoch(_) => "Lifecycle source epoch observation",
            Self::LifecycleCheckpoint(_) => "Lifecycle action checkpoint",
            Self::LifecycleRaw(_) => "Lifecycle raw driver chunk",
            Self::LifecycleNegative(_) => "Lifecycle rejected raw interpretation",
            Self::LifecycleSend(_) => "Lifecycle OS offer",
            Self::LifecycleDecision(_) => "Lifecycle mock decision",
            Self::LifecycleClosed(_) => "Lifecycle host closure",
            Self::Fault(_) => "Evidence fault",
            Self::Closed(_) => "Recorded host closure",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FsyncReceipt {
    schema: u16,
    attempt: FirmwareAttemptId,
    records: u64,
    bytes: u64,
    prefix: String,
    last_frame: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    sha256: String,
    receipt: FsyncReceipt,
}
/// Local-only cursor. Private and neither Clone nor Deserialize.
/// ```compile_fail
/// fn duplicate(c: gp200_library::FirmwareLedgerCursor) { let _ = c.clone(); }
/// ```
/// ```compile_fail
/// let _: gp200_library::FirmwareLedgerCursor = serde_json::from_str("{}").unwrap();
/// ```
pub struct FirmwareLedgerCursor {
    attempt: FirmwareAttemptId,
    owner: String,
    records: u64,
    last: String,
}
impl FirmwareLedgerCursor {
    pub fn attempt(&self) -> &FirmwareAttemptId {
        &self.attempt
    }
}
/// Checked local filesystem boundary; never a live continuation or device ACK.
/// ```compile_fail
/// fn duplicate(c: gp200_library::FirmwareLocalCheckpoint) { let _ = c.clone(); }
/// ```
#[derive(Debug)]
pub struct FirmwareLocalCheckpoint {
    attempt: FirmwareAttemptId,
    sequence: u64,
    prefix: String,
    bytes: u64,
    action: Option<FirmwareBlockDescription>,
}
impl FirmwareLocalCheckpoint {
    pub fn attempt(&self) -> &FirmwareAttemptId {
        &self.attempt
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn prefix_sha256(&self) -> &str {
        &self.prefix
    }
    pub fn prefix_bytes(&self) -> u64 {
        self.bytes
    }
    pub fn action(&self) -> Option<&FirmwareBlockDescription> {
        self.action.as_ref()
    }
    pub fn permits_flash(&self) -> bool {
        false
    }
}
#[derive(Debug, Clone)]
pub struct FirmwareRecordSummary {
    pub sequence: u64,
    pub offset: u64,
    pub bytes: u32,
    pub label: String,
    pub sha256: String,
}
#[derive(Debug, Clone)]
pub struct FirmwareBadTail {
    pub offset: u64,
    pub bytes: u64,
    pub sha256: String,
}
#[derive(Debug)]
pub struct FirmwareRetainedAttempt {
    pub attempt: FirmwareAttemptId,
    pub accepted_records: u64,
    pub valid_prefix_bytes: u64,
    pub prefix_sha256: String,
    pub projection: Option<FirmwareLocalProjection>,
    pub lifecycle: Option<FirmwareLifecycleProjection>,
    pub recorded_review: Option<String>,
    pub issues: Vec<String>,
    pub raw_tail: Option<FirmwareBadTail>,
    pub recent: Vec<FirmwareRecordSummary>,
    pub possible_device_update_mode: bool,
    pub local_checkpoint_confirmed: bool,
}
impl FirmwareRetainedAttempt {
    pub fn permits_flash(&self) -> bool {
        false
    }
    pub fn can_resume(&self) -> bool {
        false
    }
}
#[derive(Clone, PartialEq, Eq)]
struct FileIdentity {
    length: u64,
    device: u64,
    inode: u64,
    modified: i64,
    modified_ns: i64,
    changed: i64,
    changed_ns: i64,
}
fn identity(path: &Path) -> Result<FileIdentity, ServiceError> {
    let m = fs::symlink_metadata(path).map_err(io)?;
    if !m.is_file() || m.file_type().is_symlink() {
        return Err(invalid("Firmware retained file must be regular"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(FileIdentity {
            length: m.len(),
            device: m.dev(),
            inode: m.ino(),
            modified: m.mtime(),
            modified_ns: m.mtime_nsec(),
            changed: m.ctime(),
            changed_ns: m.ctime_nsec(),
        })
    }
    #[cfg(not(unix))]
    {
        Ok(FileIdentity {
            length: m.len(),
            device: 0,
            inode: 0,
            modified: 0,
            modified_ns: 0,
            changed: 0,
            changed_ns: 0,
        })
    }
}
struct Active {
    job: Arc<ReviewedFirmwareUpdate>,
    projection: FirmwareLocalProjection,
    lifecycle: Option<FirmwareLifecycleProjection>,
    records: u64,
    bytes: u64,
    last: String,
    prefix: Sha256,
    journal_identity: FileIdentity,
    materials: BTreeMap<PathBuf, FileIdentity>,
    callbacks: u64,
    actions: u64,
    sends: u64,
    decisions: u64,
    terminal: u64,
    possible_mode: bool,
}
/// Borrows the existing exclusive library writer. Reopen never issues a cursor.
pub struct FirmwareAttemptLedger<'a> {
    store: &'a mut LibraryStore,
    root: PathBuf,
    owner: String,
    allocated: BTreeSet<FirmwareAttemptId>,
    active: BTreeMap<FirmwareAttemptId, Active>,
}
impl<'a> FirmwareAttemptLedger<'a> {
    pub fn open(store: &'a mut LibraryStore) -> Result<Self, ServiceError> {
        let root = store.root().join("firmware-attempt-ledger");
        reject_symlink(&root)?;
        if !root.exists() {
            store.require_firmware_publication_certain()?;
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&root).map_err(io)?;
            if let Err(e) = sync_dir(store.root()) {
                store.firmware_publication_uncertain = true;
                return Err(e);
            }
        }
        if !fs::symlink_metadata(&root).map_err(io)?.is_dir() {
            return Err(invalid("Firmware ledger root must be a directory"));
        }
        let result = Self {
            store,
            root,
            owner: nonce()?,
            allocated: BTreeSet::new(),
            active: BTreeMap::new(),
        };
        result.attempt_ids()?;
        Ok(result)
    }
    fn path(&self, id: &FirmwareAttemptId, suffix: &str) -> PathBuf {
        self.root.join(format!("{}.{}", id.as_str(), suffix))
    }
    fn roots(&self) -> Result<(), ServiceError> {
        for p in [
            self.store.root().to_path_buf(),
            self.store.root().join("blobs"),
            self.root.clone(),
        ] {
            let m = fs::symlink_metadata(p).map_err(io)?;
            if !m.is_dir() || m.file_type().is_symlink() {
                return Err(invalid("Firmware root became a symlink or non-directory"));
            }
        }
        Ok(())
    }
    pub fn attempt_ids(&self) -> Result<Vec<FirmwareAttemptId>, ServiceError> {
        self.roots()?;
        let mut ids = BTreeSet::new();
        let mut count = 0;
        for row in fs::read_dir(&self.root).map_err(io)? {
            count += 1;
            if count > MAX_ATTEMPTS * 16 {
                return Err(invalid("Firmware retained file count exceeds bound"));
            }
            let row = row.map_err(io)?;
            let name = row
                .file_name()
                .into_string()
                .map_err(|_| invalid("Firmware ledger filename is not UTF-8"))?;
            let (id, suffix) = name
                .split_once('.')
                .ok_or_else(|| invalid("Invalid firmware ledger filename"))?;
            let id = FirmwareAttemptId::parse(id)?;
            identity(&row.path())?;
            if !matches!(suffix, "reserved" | "journal" | "checkpoint" | "previous")
                && !suffix.starts_with("failed-")
            {
                return Err(invalid("Unknown firmware ledger file"));
            }
            if suffix == "reserved" {
                ids.insert(id.clone());
            } else if !self.path(&id, "reserved").exists() {
                return Err(invalid("Firmware attempt lacks permanent reservation"));
            }
        }
        if ids.len() > MAX_ATTEMPTS {
            return Err(invalid("Firmware attempt capacity exceeded"));
        }
        Ok(ids.into_iter().collect())
    }
    fn reserve_exact(&mut self, id: &FirmwareAttemptId) -> Result<(), ServiceError> {
        self.store.require_firmware_publication_certain()?;
        self.roots()?;
        if self.attempt_ids()?.len() >= MAX_ATTEMPTS {
            return Err(invalid(
                "Firmware IDs cannot be recycled; attempt limit reached",
            ));
        }
        let path = self.path(id, "reserved");
        if path.exists() {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Firmware attempt is permanently reserved",
            ));
        }
        let bytes = format!("gp200-firmware-local-attempt-v1:{}\n", id.as_str());
        if let Err(e) = create_synced(&self.root, &path, bytes.as_bytes()) {
            self.store.firmware_publication_uncertain = true;
            return Err(e);
        }
        let reread = fault("reservation-before-reread")
            .and_then(|_| read_bounded(&path, 128))
            .and_then(|actual| {
                if actual != bytes.as_bytes() {
                    return Err(uncertain("Firmware reservation reread differs"));
                }
                Ok(())
            });
        if let Err(e) = reread {
            self.store.firmware_publication_uncertain = true;
            return Err(uncertain(format!(
                "Firmware reservation verification is uncertain: {e}"
            )));
        }
        Ok(())
    }
    pub fn reserve_id(&mut self) -> Result<FirmwareAttemptId, ServiceError> {
        for _ in 0..16 {
            let id = FirmwareAttemptId::parse(&nonce()?)?;
            match self.reserve_exact(&id) {
                Ok(()) => {
                    self.allocated.insert(id.clone());
                    return Ok(id);
                }
                Err(e) if e.kind == ErrorKind::Conflict => continue,
                Err(e) => return Err(e),
            }
        }
        Err(ServiceError::new(
            ErrorKind::Conflict,
            "Firmware ID allocation failed",
        ))
    }
    fn check_reserved(&self, id: &FirmwareAttemptId) -> Result<(), ServiceError> {
        if read_bounded(&self.path(id, "reserved"), 128)?
            != format!("gp200-firmware-local-attempt-v1:{}\n", id.as_str()).as_bytes()
        {
            return Err(invalid(
                "Firmware reservation is corrupt; ID remains occupied",
            ));
        }
        Ok(())
    }
    fn material(&self, bytes: &[u8]) -> Result<String, ServiceError> {
        if bytes.is_empty() || bytes.len() > 64 * 1024 * 1024 {
            return Err(invalid("Firmware material exceeds blob bound"));
        }
        let hash = digest(bytes);
        let path = self.store.root().join("blobs").join(&hash);
        if path.exists() {
            if self.store.read_blob(&hash)? != bytes {
                return Err(invalid("Firmware retained blob differs"));
            }
        } else {
            create_synced(&self.store.root().join("blobs"), &path, bytes)?;
        }
        if self.store.read_blob(&hash)? != bytes {
            return Err(uncertain("Firmware material exact reread differs"));
        }
        Ok(hash)
    }
    pub fn begin_reserved(
        &mut self,
        id: &FirmwareAttemptId,
        job: Arc<ReviewedFirmwareUpdate>,
        origin: FirmwareObservationOrigin,
    ) -> Result<(FirmwareLedgerCursor, FirmwareLocalCheckpoint), ServiceError> {
        self.store.require_firmware_publication_certain()?;
        self.check_reserved(id)?;
        if !self.active.is_empty() || !self.allocated.contains(id) {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Only a newly allocated creating-handle firmware ID can start",
            ));
        }
        let frame = encode(&Record {
            schema: 1,
            attempt: id.clone(),
            sequence: 0,
            previous: None,
            entry: Entry::Prepared {
                review: job.digest().hex(),
                origin,
            },
        })?;
        self.allocated.remove(id);
        self.store.firmware_publication_uncertain = true;
        let publication = (|| {
            let mut materials = BTreeMap::new();
            for (_, bytes) in job.materials() {
                let hash = self.material(bytes)?;
                let path = self.store.root().join("blobs").join(hash);
                materials.insert(path.clone(), identity(&path)?);
            }
            let review = self.material(job.preimage())?;
            let path = self.store.root().join("blobs").join(review);
            materials.insert(path.clone(), identity(&path)?);
            let journal = self.path(id, "journal");
            create_synced(&self.root, &journal, &frame)?;
            let mut prefix = Sha256::new();
            prefix.update(&frame);
            let receipt = FsyncReceipt {
                schema: 1,
                attempt: id.clone(),
                records: 1,
                bytes: frame.len() as u64,
                prefix: format!("{:x}", prefix.clone().finalize()),
                last_frame: digest(&frame),
            };
            self.publish_receipt(id, &receipt)?;
            self.verify_new_frame(id, 0, &frame, &receipt)?;
            Ok::<_, ServiceError>(Active {
                projection: FirmwareLocalProjection::for_job(&job, origin),
                lifecycle: None,
                job,
                records: 1,
                bytes: frame.len() as u64,
                last: digest(&frame),
                prefix,
                journal_identity: identity(&journal)?,
                materials,
                callbacks: 0,
                actions: 0,
                sends: 0,
                decisions: 0,
                terminal: 0,
                possible_mode: false,
            })
        })();
        match publication {
            Ok(active) => {
                self.store.firmware_publication_uncertain = false;
                let cursor = self.cursor(id, &active);
                let checkpoint = self.local_checkpoint(id, &active, None);
                self.active.insert(id.clone(), active);
                Ok((cursor, checkpoint))
            }
            Err(e) => Err(uncertain(format!(
                "Firmware start publication is uncertain: {e}"
            ))),
        }
    }
    fn cursor(&self, id: &FirmwareAttemptId, a: &Active) -> FirmwareLedgerCursor {
        FirmwareLedgerCursor {
            attempt: id.clone(),
            owner: self.owner.clone(),
            records: a.records,
            last: a.last.clone(),
        }
    }
    fn local_checkpoint(
        &self,
        id: &FirmwareAttemptId,
        a: &Active,
        action: Option<FirmwareBlockDescription>,
    ) -> FirmwareLocalCheckpoint {
        FirmwareLocalCheckpoint {
            attempt: id.clone(),
            sequence: a.records - 1,
            prefix: format!("{:x}", a.prefix.clone().finalize()),
            bytes: a.bytes,
            action,
        }
    }
    fn publish_receipt(
        &self,
        id: &FirmwareAttemptId,
        receipt: &FsyncReceipt,
    ) -> Result<(), ServiceError> {
        let bytes = serde_json::to_vec(receipt).map_err(invalid)?;
        let envelope = serde_json::to_vec(&Envelope {
            sha256: digest(&bytes),
            receipt: receipt.clone(),
        })
        .map_err(invalid)?;
        let current = self.path(id, "checkpoint");
        if current.exists() {
            let previous = read_bounded(&current, 4096)?;
            self.replace_retaining_failure(id, "previous", &previous)?;
        }
        self.replace_retaining_failure(id, "checkpoint", &envelope)
    }
    fn replace_retaining_failure(
        &self,
        id: &FirmwareAttemptId,
        suffix: &str,
        bytes: &[u8],
    ) -> Result<(), ServiceError> {
        let tmp = self.path(id, &format!("failed-{}", nonce()?));
        create_synced(&self.root, &tmp, bytes)?;
        fault("before-checkpoint-rename")?;
        let path = self.path(id, suffix);
        reject_symlink(&path)?;
        fs::rename(&tmp, &path).map_err(io)?;
        fault("after-checkpoint-rename")?;
        sync_dir(&self.root)?;
        fault("after-checkpoint-directory-sync")?;
        Ok(())
    }
    fn verify_new_frame(
        &self,
        id: &FirmwareAttemptId,
        offset: u64,
        expected: &[u8],
        receipt: &FsyncReceipt,
    ) -> Result<(), ServiceError> {
        fault("before-reread")?;
        let path = self.path(id, "journal");
        let mut f = File::open(&path).map_err(io)?;
        if f.metadata().map_err(io)?.len() != receipt.bytes {
            return Err(uncertain("Firmware journal length changed"));
        }
        f.seek(SeekFrom::Start(offset)).map_err(io)?;
        let mut actual = vec![0; expected.len()];
        f.read_exact(&mut actual).map_err(io)?;
        if actual != expected {
            return Err(uncertain("Firmware appended frame exact reread differs"));
        }
        let raw = read_bounded(&self.path(id, "checkpoint"), 4096)?;
        let checked: Envelope = serde_json::from_slice(&raw).map_err(invalid)?;
        if checked.sha256 != digest(&serde_json::to_vec(&checked.receipt).map_err(invalid)?)
            || checked.receipt != *receipt
        {
            return Err(uncertain("Firmware local sync receipt differs"));
        }
        fault("after-reread")?;
        Ok(())
    }
    pub fn checkpoint(
        &mut self,
        c: FirmwareLedgerCursor,
        ordinal: u64,
    ) -> Result<(FirmwareLedgerCursor, FirmwareLocalCheckpoint), ServiceError> {
        let job = self
            .active
            .get(&c.attempt)
            .ok_or_else(|| invalid("Firmware local writer is absent"))?
            .job
            .clone();
        let d = job.block(ordinal)?;
        self.append(c, Entry::Checkpoint(d.clone()), Some(d))
    }
    pub fn prepare_lifecycle(
        &mut self,
        c: FirmwareLedgerCursor,
        plan: FirmwareLifecyclePlanDto,
    ) -> Result<FirmwareLedgerCursor, ServiceError> {
        self.append(c, Entry::LifecyclePrepared(plan), None)
            .map(|v| v.0)
    }
    /// Append a bounded descriptive role through the same actual durable
    /// writer. The returned checkpoint is local durability, never a source
    /// continuation, reader close, or physical permission. Epoch entries use
    /// three slots from the existing owner/terminal bookkeeping reserve.
    pub fn prepare_entry_lifecycle(
        &mut self,
        c: FirmwareLedgerCursor,
        plan: FirmwareEntryObservationPlanDto,
    ) -> Result<FirmwareLedgerCursor, ServiceError> {
        self.append(c, Entry::LifecycleEntryPrepared(plan), None)
            .map(|v| v.0)
    }
    pub fn lifecycle_entry_host_settled(
        &mut self,
        c: FirmwareLedgerCursor,
        settled: FirmwareEntryHostSettlement,
    ) -> Result<FirmwareLedgerCursor, ServiceError> {
        self.append(c, Entry::LifecycleEntryHostSettled(settled), None)
            .map(|v| v.0)
    }
    pub fn lifecycle_epoch(
        &mut self,
        c: FirmwareLedgerCursor,
        epoch: FirmwareLifecycleEpochObservation,
    ) -> Result<(FirmwareLedgerCursor, FirmwareLocalCheckpoint), ServiceError> {
        self.append(c, Entry::LifecycleEpoch(Box::new(epoch)), None)
    }
    pub fn lifecycle_checkpoint(
        &mut self,
        c: FirmwareLedgerCursor,
        action: FirmwareLifecycleActionDescription,
    ) -> Result<(FirmwareLedgerCursor, FirmwareLocalCheckpoint), ServiceError> {
        self.append(c, Entry::LifecycleCheckpoint(action), None)
    }
    pub fn lifecycle_raw(
        &mut self,
        c: FirmwareLedgerCursor,
        raw: FirmwareLifecycleRawObservation,
    ) -> Result<FirmwareLedgerCursor, ServiceError> {
        self.append(c, Entry::LifecycleRaw(raw), None).map(|v| v.0)
    }
    pub fn lifecycle_negative(
        &mut self,
        c: FirmwareLedgerCursor,
        raw: FirmwareLifecycleNegativeObservation,
    ) -> Result<FirmwareLedgerCursor, ServiceError> {
        self.append(c, Entry::LifecycleNegative(raw), None)
            .map(|v| v.0)
    }
    pub fn lifecycle_send(
        &mut self,
        c: FirmwareLedgerCursor,
        send: FirmwareLifecycleSendObservation,
    ) -> Result<FirmwareLedgerCursor, ServiceError> {
        self.append(c, Entry::LifecycleSend(send), None)
            .map(|v| v.0)
    }
    pub fn lifecycle_decision(
        &mut self,
        c: FirmwareLedgerCursor,
        decision: FirmwareLifecycleDecision,
    ) -> Result<FirmwareLedgerCursor, ServiceError> {
        self.append(c, Entry::LifecycleDecision(decision), None)
            .map(|v| v.0)
    }
    pub fn lifecycle_close(
        &mut self,
        c: FirmwareLedgerCursor,
        closure: FirmwareLifecycleHostClosure,
    ) -> Result<FirmwareRetainedAttempt, ServiceError> {
        let id = c.attempt.clone();
        self.append(c, Entry::LifecycleClosed(closure), None)?;
        self.active.remove(&id);
        self.load(&id)
    }
    pub fn observe_send(
        &mut self,
        c: FirmwareLedgerCursor,
        s: FirmwareSendObservation,
    ) -> Result<FirmwareLedgerCursor, ServiceError> {
        self.append(c, Entry::Send(s), None).map(|v| v.0)
    }
    pub fn observe_callback(
        &mut self,
        c: FirmwareLedgerCursor,
        callback: FirmwareCallbackDto,
    ) -> Result<FirmwareLedgerCursor, ServiceError> {
        if let Err(e) = callback.validate() {
            self.poison(&c.attempt);
            return Err(e);
        }
        self.append(c, Entry::Callback(callback), None).map(|v| v.0)
    }
    pub fn acknowledge_mock(
        &mut self,
        c: FirmwareLedgerCursor,
        key: FirmwareBlockDescription,
    ) -> Result<FirmwareLedgerCursor, ServiceError> {
        self.append(c, Entry::MockDecision(key), None).map(|v| v.0)
    }
    pub fn record_mode_boundary(
        &mut self,
        c: FirmwareLedgerCursor,
        raw: Vec<u8>,
        note: String,
    ) -> Result<FirmwareLedgerCursor, ServiceError> {
        if raw.is_empty() || raw.len() > 518 || note.is_empty() || note.len() > 512 {
            return Err(invalid("Foreign mode observation exceeds diagnostic bound"));
        }
        self.append(c, Entry::ModeBoundary { raw, note }, None)
            .map(|v| v.0)
    }
    pub fn close(
        &mut self,
        c: FirmwareLedgerCursor,
        reason: FirmwareHostClosure,
    ) -> Result<FirmwareRetainedAttempt, ServiceError> {
        let id = c.attempt.clone();
        self.append(c, Entry::Closed(reason), None)?;
        self.active.remove(&id);
        self.load(&id)
    }
    pub fn evidence_fault(
        &mut self,
        c: FirmwareLedgerCursor,
        note: String,
    ) -> Result<(), ServiceError> {
        let id = c.attempt.clone();
        let result = self.append(c, Entry::Fault(note), None);
        self.poison(&id);
        result.map(|_| ())
    }
    /// Private stopped-owner cleanup, independent of the consumed action cursor.
    /// This can record only a terminal fault and never constructs a new cursor.
    pub(crate) fn abandon_owner(&mut self, id: &FirmwareAttemptId, note: String) {
        if self.active.contains_key(id) && !self.store.firmware_publication_uncertain {
            let _ = self.append_record(id, Entry::Fault(note), None);
        }
        self.poison(id);
    }
    fn poison(&mut self, id: &FirmwareAttemptId) {
        self.active.remove(id);
        self.allocated.remove(id);
        self.store.firmware_publication_uncertain = true;
    }
    fn append(
        &mut self,
        c: FirmwareLedgerCursor,
        entry: Entry,
        action: Option<FirmwareBlockDescription>,
    ) -> Result<(FirmwareLedgerCursor, FirmwareLocalCheckpoint), ServiceError> {
        let a = self.active.get(&c.attempt).ok_or_else(|| {
            ServiceError::new(
                ErrorKind::Conflict,
                "Reopened/imported/stopped firmware attempt cannot append",
            )
        })?;
        if c.owner != self.owner || c.records != a.records || c.last != a.last {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Firmware local cursor is stale or from another owner",
            ));
        }
        let receipt = self.append_record(&c.attempt, entry, action)?;
        let a = self
            .active
            .get(&c.attempt)
            .expect("checked append remains active");
        Ok((self.cursor(&c.attempt, a), receipt))
    }
    fn append_record(
        &mut self,
        id: &FirmwareAttemptId,
        entry: Entry,
        action: Option<FirmwareBlockDescription>,
    ) -> Result<FirmwareLocalCheckpoint, ServiceError> {
        self.store.require_firmware_publication_certain()?;
        self.roots()?;
        let a = self.active.get(id).ok_or_else(|| {
            ServiceError::new(
                ErrorKind::Conflict,
                "Firmware stopped owner has no active projection",
            )
        })?;
        let mut projected = a.projection.clone();
        let mut lifecycle = a.lifecycle.clone();
        apply_all(&a.job, &mut projected, &mut lifecycle, &entry)?;
        let legacy = a.job.budget();
        let budget = lifecycle.as_ref().map(|p| p.budget());
        let (actions, callbacks, frames, bytes) = budget.map_or(
            (
                legacy.actions,
                legacy.callbacks,
                legacy.frames,
                legacy.bytes,
            ),
            |b| (b.actions, b.raw_records, b.frames, b.bytes),
        );
        let count_ok = match &entry {
            Entry::Checkpoint(_) | Entry::LifecycleCheckpoint(_) => a.actions < actions,
            Entry::Send(_) | Entry::LifecycleSend(_) => a.sends < actions,
            Entry::MockDecision(_)
            | Entry::LifecycleDecision(_)
            | Entry::LifecycleEntryHostSettled(_) => a.decisions < actions,
            Entry::Callback(_) | Entry::LifecycleRaw(_) | Entry::LifecycleNegative(_) => {
                a.callbacks < callbacks
            }
            _ => a.terminal < FIRMWARE_TERMINAL_RESERVE,
        };
        let frame = encode(&Record {
            schema: 1,
            attempt: id.clone(),
            sequence: a.records,
            previous: Some(a.last.clone()),
            entry: entry.clone(),
        })?;
        let usable_frames = if entry.terminal() {
            frames
        } else {
            frames - FIRMWARE_TERMINAL_RESERVE
        };
        let usable_bytes = if entry.terminal() {
            bytes
        } else {
            bytes - FIRMWARE_TERMINAL_RESERVE * (8 * 1024 + HEADER as u64)
        };
        if !count_ok || a.records >= usable_frames || a.bytes + frame.len() as u64 > usable_bytes {
            if entry.terminal() {
                self.poison(id);
                return Err(uncertain(
                    "Firmware terminal bookkeeping capacity exhausted",
                ));
            }
            let failure = self.append_record(
                id,
                Entry::Fault("Firmware evidence capacity exhausted".into()),
                None,
            );
            self.poison(id);
            return Err(uncertain(format!(
                "Firmware evidence capacity exhausted; fault retained: {}",
                failure.is_ok()
            )));
        }
        for (path, pin) in &a.materials {
            if !identity(path).is_ok_and(|actual| actual == *pin) {
                self.poison(id);
                return Err(uncertain(
                    "Firmware immutable material changed during local observation",
                ));
            }
        }
        let path = self.path(id, "journal");
        if !identity(&path).is_ok_and(|actual| actual == a.journal_identity) {
            self.poison(id);
            return Err(uncertain("Firmware journal was externally changed"));
        }
        // Keep the accepted projection available for terminal owner cleanup if
        // pure validation above fails. Publication consumes it before visibility.
        let mut a = self.active.remove(id).expect("validated active projection");
        self.store.firmware_publication_uncertain = true;
        let result = (|| {
            let mut f = OpenOptions::new().append(true).open(&path).map_err(io)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let opened = f.metadata().map_err(io)?;
                if opened.dev() != a.journal_identity.device
                    || opened.ino() != a.journal_identity.inode
                {
                    return Err(uncertain("Firmware journal changed while opening"));
                }
            }
            let half = frame.len() / 2;
            f.write_all(&frame[..half]).map_err(io)?;
            fault("append-partial-write")?;
            f.write_all(&frame[half..]).map_err(io)?;
            fault("append-write")?;
            f.sync_all().map_err(io)?;
            fault("append-fsync")?;
            a.prefix.update(&frame);
            let receipt = FsyncReceipt {
                schema: 1,
                attempt: id.clone(),
                records: a.records + 1,
                bytes: a.bytes + frame.len() as u64,
                prefix: format!("{:x}", a.prefix.clone().finalize()),
                last_frame: digest(&frame),
            };
            self.publish_receipt(id, &receipt)?;
            self.verify_new_frame(id, a.bytes, &frame, &receipt)?;
            a.records += 1;
            a.bytes = receipt.bytes;
            a.last = receipt.last_frame;
            a.projection = projected;
            a.lifecycle = lifecycle;
            a.journal_identity = identity(&path)?;
            match &entry {
                Entry::Checkpoint(_) | Entry::LifecycleCheckpoint(_) => a.actions += 1,
                Entry::Send(_) | Entry::LifecycleSend(_) => a.sends += 1,
                Entry::MockDecision(_)
                | Entry::LifecycleDecision(_)
                | Entry::LifecycleEntryHostSettled(_) => a.decisions += 1,
                Entry::Callback(_) | Entry::LifecycleRaw(_) | Entry::LifecycleNegative(_) => {
                    a.callbacks += 1
                }
                _ => a.terminal += 1,
            }
            if matches!(entry, Entry::ModeBoundary { .. })
                && a.projection.origin() == FirmwareObservationOrigin::LocalObservation
            {
                a.possible_mode = true;
            }
            Ok::<(), ServiceError>(())
        })();
        if let Err(e) = result {
            return Err(uncertain(format!(
                "Firmware append publication is uncertain: {e}"
            )));
        }
        self.store.firmware_publication_uncertain = false;
        let receipt = self.local_checkpoint(id, &a, action);
        self.active.insert(id.clone(), a);
        Ok(receipt)
    }
    fn read_job(&self, review: &str) -> Result<Arc<ReviewedFirmwareUpdate>, ServiceError> {
        let preimage = self.store.read_blob(review)?;
        let pins = ReviewedFirmwareUpdate::retained_pins(&preimage)?;
        let mut materials = vec![];
        let mut cache: BTreeMap<String, Arc<[u8]>> = BTreeMap::new();
        for pin in pins {
            let raw = if let Some(raw) = cache.get(&pin.sha256) {
                Arc::clone(raw)
            } else {
                let raw: Arc<[u8]> = self.store.read_blob(&pin.sha256)?.into();
                if raw.len() as u64 != pin.bytes {
                    return Err(invalid("Firmware imported material length differs"));
                }
                cache.insert(pin.sha256, Arc::clone(&raw));
                raw
            };
            materials.push(FirmwareMaterialInput {
                kind: pin.kind,
                bytes: raw,
            });
        }
        let job = ReviewedFirmwareUpdate::reconstruct(&preimage, materials)?;
        if job.digest().hex() != review {
            return Err(invalid("Firmware review digest differs"));
        }
        Ok(Arc::new(job))
    }
    /// Streaming observation-only recovery. Does not truncate, repair or resume.
    pub fn load(&self, id: &FirmwareAttemptId) -> Result<FirmwareRetainedAttempt, ServiceError> {
        self.load_inner(id, false)
    }
    fn load_inner(
        &self,
        id: &FirmwareAttemptId,
        observed: bool,
    ) -> Result<FirmwareRetainedAttempt, ServiceError> {
        self.roots()?;
        if observed {
            if crate::firmware_attempt_history::checked_bytes(&self.path(id, "reserved"), 128)?
                != format!("gp200-firmware-local-attempt-v1:{}\n", id.as_str()).as_bytes()
            {
                return Err(invalid(
                    "Firmware reservation is corrupt; raw observation remains available",
                ));
            }
        } else {
            self.check_reserved(id)?;
        }
        let path = self.path(id, "journal");
        let meta = identity(&path)?;
        if meta.length > FIRMWARE_JOURNAL_LIMIT {
            return Err(invalid("Firmware journal exceeds 2 GiB streaming bound"));
        }
        let checked = if observed {
            Some(crate::firmware_attempt_history::checked_open(
                &path,
                FIRMWARE_JOURNAL_LIMIT,
            )?)
        } else {
            None
        };
        let mut file = if let Some(c) = &checked {
            c.file.try_clone().map_err(io)?
        } else {
            File::open(&path).map_err(io)?
        };
        let mut offset = 0;
        let mut count = 0;
        let mut previous = None;
        let mut prefix = Sha256::new();
        let mut projection = None;
        let mut lifecycle = None;
        let mut job = None;
        let mut review = None;
        let mut issues = vec![];
        let mut recent = VecDeque::new();
        let mut possible_mode = false;
        let mut callbacks = 0;
        let mut actions = 0;
        let mut sends = 0;
        let mut decisions = 0;
        let mut terminal = 0;
        while offset < meta.length {
            let parsed = read_frame(&mut file, offset, meta.length);
            let (record, frame) = match parsed {
                Ok(v) => v,
                Err(e) => {
                    issues.push(e.message);
                    break;
                }
            };
            let accept = (|| {
                if record.schema != 1
                    || record.attempt != *id
                    || record.sequence != count
                    || record.previous != previous
                {
                    return Err(invalid(
                        "Firmware frame identity/sequence/hash chain differs",
                    ));
                }
                if count == 0 {
                    let Entry::Prepared { review: r, origin } = &record.entry else {
                        return Err(invalid("Firmware first record is not Prepared"));
                    };
                    let rebuilt = if observed {
                        self.history_read_job(r)?
                    } else {
                        self.read_job(r)?
                    };
                    projection = Some(FirmwareLocalProjection::for_job(&rebuilt, *origin));
                    review = Some(r.clone());
                    job = Some(rebuilt);
                } else {
                    if matches!(record.entry, Entry::Prepared { .. }) {
                        return Err(invalid("Firmware journal repeats Prepared"));
                    }
                    let j = job.as_ref().expect("Prepared accepted");
                    let p = projection.as_ref().expect("Prepared accepted");
                    let mut tentative = p.clone();
                    let mut lifecycle_tentative = lifecycle.clone();
                    apply_all(j, &mut tentative, &mut lifecycle_tentative, &record.entry)?;
                    let legacy = j.budget();
                    let budget = lifecycle_tentative.as_ref().map(|p| p.budget());
                    let (ba, bc, bf, bb) = budget.map_or(
                        (
                            legacy.actions,
                            legacy.callbacks,
                            legacy.frames,
                            legacy.bytes,
                        ),
                        |b| (b.actions, b.raw_records, b.frames, b.bytes),
                    );
                    let (c, a, s, d, t) = match &record.entry {
                        Entry::Callback(_)
                        | Entry::LifecycleRaw(_)
                        | Entry::LifecycleNegative(_) => {
                            (callbacks + 1, actions, sends, decisions, terminal)
                        }
                        Entry::Checkpoint(_) | Entry::LifecycleCheckpoint(_) => {
                            (callbacks, actions + 1, sends, decisions, terminal)
                        }
                        Entry::Send(_) | Entry::LifecycleSend(_) => {
                            (callbacks, actions, sends + 1, decisions, terminal)
                        }
                        Entry::MockDecision(_)
                        | Entry::LifecycleDecision(_)
                        | Entry::LifecycleEntryHostSettled(_) => {
                            (callbacks, actions, sends, decisions + 1, terminal)
                        }
                        _ => (callbacks, actions, sends, decisions, terminal + 1),
                    };
                    if c > bc
                        || a > ba
                        || s > ba
                        || d > ba
                        || t > FIRMWARE_TERMINAL_RESERVE
                        || count + 1 > bf
                        || offset + frame.len() as u64 > bb
                    {
                        return Err(invalid(
                            "Firmware retained traffic exceeds prepared admission budget",
                        ));
                    }
                    projection = Some(tentative);
                    lifecycle = lifecycle_tentative;
                    callbacks = c;
                    actions = a;
                    sends = s;
                    decisions = d;
                    terminal = t;
                }
                Ok::<(), ServiceError>(())
            })();
            if let Err(e) = accept {
                issues.push(e.message);
                break;
            }
            if matches!(record.entry, Entry::ModeBoundary { .. })
                && projection
                    .as_ref()
                    .is_some_and(|p| p.origin() == FirmwareObservationOrigin::LocalObservation)
            {
                possible_mode = true;
            }
            let hash = digest(&frame);
            recent.push_back(FirmwareRecordSummary {
                sequence: count,
                offset,
                bytes: frame.len() as u32,
                label: record.entry.label().into(),
                sha256: hash.clone(),
            });
            if recent.len() > MAX_RECENT {
                recent.pop_front();
            }
            prefix.update(&frame);
            offset += frame.len() as u64;
            count += 1;
            previous = Some(hash);
        }
        let prefix_sha256 = format!("{:x}", prefix.finalize());
        let raw_tail = if offset < meta.length {
            file.seek(SeekFrom::Start(offset)).map_err(io)?;
            let mut tail = Sha256::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = file.read(&mut buf).map_err(io)?;
                if n == 0 {
                    break;
                }
                tail.update(&buf[..n]);
            }
            Some(FirmwareBadTail {
                offset,
                bytes: meta.length - offset,
                sha256: format!("{:x}", tail.finalize()),
            })
        } else {
            None
        };
        let expected = FsyncReceipt {
            schema: 1,
            attempt: id.clone(),
            records: count,
            bytes: offset,
            prefix: prefix_sha256.clone(),
            last_frame: previous.unwrap_or_default(),
        };
        let receipt = if observed {
            crate::firmware_attempt_history::checked_bytes(&self.path(id, "checkpoint"), 4096)
                .and_then(|raw| {
                    let e: Envelope = serde_json::from_slice(&raw).map_err(invalid)?;
                    if e.sha256 != digest(&serde_json::to_vec(&e.receipt).map_err(invalid)?) {
                        return Err(invalid("Firmware checkpoint hash differs"));
                    }
                    Ok(e.receipt)
                })
        } else {
            self.read_receipt(id)
        };
        let local_checkpoint_confirmed = receipt.is_ok_and(|r| r == expected);
        if !local_checkpoint_confirmed {
            issues.push(
                "Latest local filesystem checkpoint does not confirm exact accepted prefix".into(),
            );
        }
        if let Some(c) = checked {
            c.recheck()?;
        }
        if identity(&path)? != meta {
            issues.push("Firmware journal changed during independent streaming read".into());
        }
        if let Some(p) = &mut projection {
            if !issues.is_empty() {
                p.stop();
                if p.origin() == FirmwareObservationOrigin::LocalObservation {
                    possible_mode = true;
                }
            }
            p.derive_reopen();
        }
        if let Some(p) = &mut lifecycle {
            p.stop(); // Recovery is always observation only.
            if p.plan().origin == FirmwareObservationOrigin::LocalObservation {
                possible_mode = true;
            }
        }
        Ok(FirmwareRetainedAttempt {
            attempt: id.clone(),
            accepted_records: count,
            valid_prefix_bytes: offset,
            prefix_sha256,
            projection,
            lifecycle,
            recorded_review: review,
            issues,
            raw_tail,
            recent: recent.into_iter().collect(),
            possible_device_update_mode: possible_mode,
            local_checkpoint_confirmed,
        })
    }
    pub(crate) fn history_roster(
        store: &LibraryStore,
    ) -> Result<Vec<FirmwareAttemptId>, ServiceError> {
        let root = store.root.join("firmware-attempt-ledger");
        match fs::symlink_metadata(&root) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(io(e)),
            Ok(m) if m.file_type().is_symlink() => {
                return Err(invalid("History ledger root is a symlink"));
            }
            Ok(_) => {}
        }
        let m = fs::symlink_metadata(&root).map_err(io)?;
        if !m.is_dir() || m.file_type().is_symlink() {
            return Err(invalid("History ledger root invalid"));
        }
        let mut ids = BTreeSet::new();
        let mut count = 0;
        for path in
            crate::firmware_attempt_history::checked_directory_entries(&root, MAX_ATTEMPTS * 16)?
        {
            count += 1;
            if count > MAX_ATTEMPTS * 16 {
                return Err(invalid("History retained file count exceeds bound"));
            }
            let name = path
                .file_name()
                .ok_or_else(|| invalid("History filename absent"))?
                .to_os_string()
                .into_string()
                .map_err(|_| invalid("History filename is not UTF-8"))?;
            let (id, suffix) = name
                .split_once('.')
                .ok_or_else(|| invalid("History filename invalid"))?;
            let id = FirmwareAttemptId::parse(id)?;
            if !matches!(suffix, "reserved" | "journal" | "checkpoint" | "previous")
                && !suffix.starts_with("failed-")
            {
                return Err(invalid("Unknown history file suffix"));
            }
            let checked = crate::firmware_attempt_history::checked_open(
                &path,
                if suffix == "journal" {
                    FIRMWARE_JOURNAL_LIMIT
                } else {
                    16 * 1024
                },
            )?;
            checked.recheck()?;
            ids.insert(id);
            if ids.len() > MAX_ATTEMPTS {
                return Err(invalid("History attempt bound exceeded"));
            }
        }
        Ok(ids.into_iter().collect())
    }
    pub(crate) fn history_paths(
        store: &LibraryStore,
        id: &FirmwareAttemptId,
    ) -> Result<Vec<PathBuf>, ServiceError> {
        if !Self::history_roster(store)?.contains(id) {
            return Err(invalid("History attempt absent"));
        }
        let prefix = format!("{}.", id.as_str());
        let mut paths = vec![];
        for path in crate::firmware_attempt_history::checked_directory_entries(
            &store.root.join("firmware-attempt-ledger"),
            MAX_ATTEMPTS * 16,
        )? {
            if path
                .file_name()
                .ok_or_else(|| invalid("History filename absent"))?
                .to_str()
                .is_some_and(|n| n.starts_with(&prefix))
            {
                paths.push(path);
            }
        }
        paths.sort();
        Ok(paths)
    }
    pub(crate) fn history_first_review(
        path: &Path,
        id: &FirmwareAttemptId,
    ) -> Result<Option<String>, ServiceError> {
        let mut checked =
            crate::firmware_attempt_history::checked_open(path, FIRMWARE_JOURNAL_LIMIT)?;
        let total = checked.file.metadata().map_err(io)?.len();
        let (record, _) = read_frame(&mut checked.file, 0, total)?;
        checked.recheck()?;
        if record.schema != 1
            || record.attempt != *id
            || record.sequence != 0
            || record.previous.is_some()
        {
            return Err(invalid("Original Prepared frame identity differs"));
        }
        if let Entry::Prepared { review, .. } = record.entry {
            Ok(Some(review))
        } else {
            Ok(None)
        }
    }
    pub(crate) fn history_load(
        store: &'a mut LibraryStore,
        id: &FirmwareAttemptId,
    ) -> Result<FirmwareRetainedAttempt, ServiceError> {
        let root = store.root.join("firmware-attempt-ledger");
        let ledger = Self {
            store,
            root,
            owner: "observation-only-history".into(),
            allocated: BTreeSet::new(),
            active: BTreeMap::new(),
        };
        ledger.load_inner(id, true)
    }
    fn history_read_job(&self, review: &str) -> Result<Arc<ReviewedFirmwareUpdate>, ServiceError> {
        if !crate::store::valid_hash(review) {
            return Err(invalid("History review hash invalid"));
        }
        let preimage = crate::firmware_attempt_history::checked_bytes(
            &self.store.root.join("blobs").join(review),
            2 * 1024 * 1024,
        )?;
        if digest(&preimage) != review {
            return Err(invalid("History preimage digest differs"));
        }
        let pins = ReviewedFirmwareUpdate::retained_pins(&preimage)?;
        let mut materials = vec![];
        let mut cache: BTreeMap<String, Arc<[u8]>> = BTreeMap::new();
        for pin in pins {
            let raw = if let Some(v) = cache.get(&pin.sha256) {
                v.clone()
            } else {
                let raw: Arc<[u8]> = crate::firmware_attempt_history::checked_bytes(
                    &self.store.root.join("blobs").join(&pin.sha256),
                    64 * 1024 * 1024,
                )?
                .into();
                if raw.len() as u64 != pin.bytes || digest(&raw) != pin.sha256 {
                    return Err(invalid("History retained material differs"));
                }
                cache.insert(pin.sha256, raw.clone());
                raw
            };
            materials.push(FirmwareMaterialInput {
                kind: pin.kind,
                bytes: raw,
            });
        }
        Ok(Arc::new(ReviewedFirmwareUpdate::reconstruct(
            &preimage, materials,
        )?))
    }
    fn read_receipt(&self, id: &FirmwareAttemptId) -> Result<FsyncReceipt, ServiceError> {
        let raw = read_bounded(&self.path(id, "checkpoint"), 4096)?;
        let e: Envelope = serde_json::from_slice(&raw).map_err(invalid)?;
        if e.sha256 != digest(&serde_json::to_vec(&e.receipt).map_err(invalid)?) {
            return Err(invalid("Firmware checkpoint hash differs"));
        }
        Ok(e.receipt)
    }
    /// Import retains exactly the supplied framed observation; no local cursor.
    pub fn import_observation(
        &mut self,
        id: FirmwareAttemptId,
        job: Arc<ReviewedFirmwareUpdate>,
        raw: &[u8],
    ) -> Result<FirmwareRetainedAttempt, ServiceError> {
        if raw.len() as u64 > job.budget().bytes {
            return Err(invalid(
                "Firmware imported observation exceeds prepared journal bound",
            ));
        }
        self.reserve_exact(&id)?;
        self.store.firmware_publication_uncertain = true;
        let result = (|| {
            for (_, b) in job.materials() {
                self.material(b)?;
            }
            self.material(job.preimage())?;
            create_synced(&self.root, &self.path(&id, "journal"), raw)?;
            Ok::<(), ServiceError>(())
        })();
        if let Err(e) = result {
            return Err(uncertain(format!(
                "Firmware imported evidence publication is uncertain: {e}"
            )));
        }
        self.store.firmware_publication_uncertain = false;
        self.load(&id)
    }
    pub fn stream_bad_tail(
        &self,
        id: &FirmwareAttemptId,
        tail: &FirmwareBadTail,
        destination: &mut dyn Write,
    ) -> Result<(), ServiceError> {
        let found = self.load(id)?;
        let actual = found
            .raw_tail
            .ok_or_else(|| invalid("Firmware bad tail is absent"))?;
        if actual.offset != tail.offset
            || actual.bytes != tail.bytes
            || actual.sha256 != tail.sha256
        {
            return Err(invalid("Firmware requested bad tail is stale"));
        }
        let path = self.path(id, "journal");
        let before = identity(&path)?;
        let mut file = File::open(&path).map_err(io)?;
        file.seek(SeekFrom::Start(tail.offset)).map_err(io)?;
        let mut bounded = file.take(tail.bytes);
        let mut hash = Sha256::new();
        let mut copied = 0;
        let mut buffer = [0; 8192];
        loop {
            let n = bounded.read(&mut buffer).map_err(io)?;
            if n == 0 {
                break;
            }
            destination.write_all(&buffer[..n]).map_err(io)?;
            hash.update(&buffer[..n]);
            copied += n as u64;
        }
        if copied != tail.bytes
            || format!("{:x}", hash.finalize()) != tail.sha256
            || identity(&path)? != before
        {
            return Err(uncertain(
                "Firmware bad tail changed during export; destination is unverified",
            ));
        }
        Ok(())
    }
}
fn apply_all(
    job: &ReviewedFirmwareUpdate,
    legacy: &mut FirmwareLocalProjection,
    lifecycle: &mut Option<FirmwareLifecycleProjection>,
    e: &Entry,
) -> Result<(), ServiceError> {
    let preparation = match e {
        Entry::LifecyclePrepared(plan) => Some((plan, None)),
        Entry::LifecycleEntryPrepared(entry) => Some((&entry.plan, Some(entry))),
        _ => None,
    };
    if let Some((plan, entry)) = preparation {
        if lifecycle.is_some()
            || legacy.pending().is_some()
            || legacy.next_block() != 0
            || legacy.recorded_closure().is_some()
            || plan.origin != legacy.origin()
        {
            return Err(invalid(
                "Lifecycle preparation follows legacy actions or repeats",
            ));
        }
        *lifecycle = Some(match entry {
            Some(entry) => FirmwareLifecycleProjection::new_entry(job, entry.clone())?,
            None => FirmwareLifecycleProjection::new(job, plan.clone())?,
        });
        return Ok(());
    }
    if let Some(p) = lifecycle {
        if p.closure().is_some() {
            return Err(invalid("Lifecycle record follows closure"));
        }
        return match e {
            Entry::LifecycleEpoch(e) => p.observe_epoch(e.as_ref().clone()),
            Entry::LifecycleCheckpoint(a) => p.checkpoint(job, a.clone()),
            Entry::LifecycleRaw(r) => p.raw(r),
            Entry::LifecycleNegative(r) => {
                r.retained_json()?;
                p.stop();
                Ok(())
            }
            Entry::LifecycleSend(s) => p.observe_send(s),
            Entry::LifecycleDecision(d) => p.decide(d),
            Entry::LifecycleEntryHostSettled(s) => p.settle_entry_host(s),
            Entry::LifecycleClosed(c) => p.close(*c),
            Entry::Fault(note) if !note.is_empty() && note.len() <= 512 => {
                p.stop();
                Ok(())
            }
            _ => Err(invalid("Legacy and lifecycle records cannot be mixed")),
        };
    }
    apply(job, legacy, e)
}
fn apply(
    job: &ReviewedFirmwareUpdate,
    p: &mut FirmwareLocalProjection,
    e: &Entry,
) -> Result<(), ServiceError> {
    if p.recorded_closure().is_some() {
        return Err(invalid("Firmware record follows closed observation"));
    }
    match e {
        Entry::Prepared { .. } => Err(invalid("Prepared cannot be appended")),
        Entry::Checkpoint(d) => p.checkpoint(job, d.clone()),
        Entry::Send(s) => p.observe_send(s),
        Entry::Callback(c) => p.observe_callback(c),
        Entry::MockDecision(d) => p.acknowledge_mock(d),
        Entry::ModeBoundary { raw, note } => {
            if raw.is_empty() || raw.len() > 518 || note.is_empty() || note.len() > 512 {
                return Err(invalid("Foreign mode observation exceeds bounds"));
            }
            p.stop();
            Ok(())
        }
        Entry::Fault(note) => {
            if note.is_empty() || note.len() > 512 {
                return Err(invalid("Firmware fault text exceeds bound"));
            }
            p.stop();
            Ok(())
        }
        Entry::Closed(reason) => p.close(reason.clone()),
        _ => Err(invalid("Lifecycle record lacks lifecycle preparation")),
    }
}
fn encode(r: &Record) -> Result<Vec<u8>, ServiceError> {
    let body = serde_json::to_vec(r).map_err(invalid)?;
    if body.len() > r.entry.cap() || body.len() > MAX_FRAME {
        return Err(invalid(
            "Firmware encoded frame exceeds prepared per-kind bound",
        ));
    }
    let mut frame = Vec::with_capacity(HEADER + body.len());
    frame.extend_from_slice(MAGIC);
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&Sha256::digest(&body));
    frame.extend_from_slice(&body);
    Ok(frame)
}
fn read_frame(file: &mut File, offset: u64, total: u64) -> Result<(Record, Vec<u8>), ServiceError> {
    if total - offset < HEADER as u64 {
        return Err(invalid("Firmware journal has incomplete frame header"));
    }
    let mut header = [0; HEADER];
    file.read_exact(&mut header).map_err(io)?;
    if &header[..8] != MAGIC {
        return Err(invalid("Firmware frame magic differs"));
    }
    let len = u32::from_le_bytes(header[8..12].try_into().expect("four bytes")) as usize;
    if len == 0 || len > MAX_FRAME || total - offset < HEADER as u64 + len as u64 {
        return Err(invalid("Firmware journal has invalid or incomplete body"));
    }
    let mut body = vec![0; len];
    file.read_exact(&mut body).map_err(io)?;
    if Sha256::digest(&body)[..] != header[12..44] {
        return Err(invalid("Firmware frame payload hash differs"));
    }
    let record: Record = serde_json::from_slice(&body).map_err(invalid)?;
    if body.len() > record.entry.cap() {
        return Err(invalid("Firmware frame exceeds per-kind preparation cap"));
    }
    let mut frame = header.to_vec();
    frame.extend_from_slice(&body);
    Ok((record, frame))
}
fn uncertain(s: impl Into<String>) -> ServiceError {
    ServiceError::new(ErrorKind::Indeterminate, s)
}
fn sync_dir(path: &Path) -> Result<(), ServiceError> {
    File::open(path).and_then(|f| f.sync_all()).map_err(io)
}
fn create_synced(root: &Path, path: &Path, bytes: &[u8]) -> Result<(), ServiceError> {
    reject_symlink(path)?;
    fault("new-before-create")?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(io)?;
    let half = bytes.len() / 2;
    file.write_all(&bytes[..half]).map_err(io)?;
    fault("new-partial-write")?;
    file.write_all(&bytes[half..]).map_err(io)?;
    fault("new-write")?;
    file.sync_all().map_err(io)?;
    fault("new-fsync")?;
    sync_dir(root)?;
    fault("new-directory-sync")?;
    Ok(())
}
fn fault(name: &str) -> Result<(), ServiceError> {
    #[cfg(test)]
    tests::boundary(name)?;
    #[cfg(not(test))]
    let _ = name;
    Ok(())
}
#[cfg(test)]
#[path = "firmware_attempt_ledger_tests.rs"]
pub(crate) mod tests;
