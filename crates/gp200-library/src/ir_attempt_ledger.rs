//! Durable local IR observations. No transport, device proof, or job authority.
//! Keep synchronous ledger operations on a worker or bootstrap path.
use crate::{
    ContentHash, LibraryStore,
    store::{digest, invalid, io, nonce, read_bounded, reject_symlink},
};
use gp200_domain::*;
use gp200_protocol::{RawTransfer, WRITE_KIND, asset_upload_candidate, encode_transfer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

const MAGIC: &[u8; 8] = b"GP2IRL01";
const HEADER: usize = 44;
const MAX_RECORD: usize = 64 * 1024;
const MAX_JOURNAL: u64 = 2 * 1024 * 1024;
const MAX_ATTEMPTS: usize = 1024;
const MAX_RECORDS: usize = 258; // start, at most 256 raw events, finish

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum IrMaterialRole {
    SourceWav,
    Coefficients,
    IntendedTlv,
    IntendedWire,
    BaselinePayload,
    IdentityPayload,
    SessionJson,
    ReviewPreimage,
}
impl IrMaterialRole {
    fn maximum(self) -> usize {
        match self {
            Self::SourceWav => 64 * 1024 * 1024,
            Self::Coefficients => 4096,
            Self::IntendedTlv => 4124,
            Self::IntendedWire => 8570,
            Self::BaselinePayload | Self::IdentityPayload => 4096,
            Self::SessionJson | Self::ReviewPreimage => 64 * 1024,
        }
    }
    fn expected(self, binding: &IrAttemptBindingDto) -> &str {
        match self {
            Self::SourceWav => &binding.source_sha256,
            Self::Coefficients => &binding.coefficients_sha256,
            Self::IntendedTlv => &binding.payload_sha256,
            Self::IntendedWire => &binding.wire_sha256,
            Self::BaselinePayload => &binding.baseline_sha256,
            Self::IdentityPayload => &binding.session.identity_sha256,
            Self::SessionJson => &binding.session.session_sha256,
            Self::ReviewPreimage => &binding.review_sha256,
        }
    }
}
const ROLES: [IrMaterialRole; 8] = [
    IrMaterialRole::SourceWav,
    IrMaterialRole::Coefficients,
    IrMaterialRole::IntendedTlv,
    IrMaterialRole::IntendedWire,
    IrMaterialRole::BaselinePayload,
    IrMaterialRole::IdentityPayload,
    IrMaterialRole::SessionJson,
    IrMaterialRole::ReviewPreimage,
];

/// Replacement/source materials only. This cannot describe the prior IR audio.
#[derive(Debug, Clone)]
pub struct IrAttemptMaterials {
    bytes: std::collections::BTreeMap<IrMaterialRole, Vec<u8>>,
}
impl IrAttemptMaterials {
    /// Calls only pure checked recipe/encoding and review-preimage APIs.
    pub fn from_prepared(
        job: &PreparedDeviceJob,
        source_wav: &[u8],
        identity_payload: &[u8],
    ) -> Result<Self, ServiceError> {
        let recipe = job.recipe();
        if recipe.ir_length_policy().is_none() {
            return Err(invalid("IR ledger needs a checked IR preparation"));
        }
        let transfer = asset_upload_candidate(job.target(), recipe.name(), recipe.derived())
            .map_err(invalid)?;
        let wire = encode_transfer(&transfer)
            .map_err(invalid)?
            .into_iter()
            .flatten()
            .collect();
        let bytes = std::collections::BTreeMap::from([
            (IrMaterialRole::SourceWav, source_wav.to_vec()),
            (IrMaterialRole::Coefficients, recipe.derived().to_vec()),
            (IrMaterialRole::IntendedTlv, transfer.payload().to_vec()),
            (IrMaterialRole::IntendedWire, wire),
            (
                IrMaterialRole::BaselinePayload,
                job.baseline().raw().payload().to_vec(),
            ),
            (IrMaterialRole::IdentityPayload, identity_payload.to_vec()),
            (
                IrMaterialRole::SessionJson,
                serde_json::to_vec(job.session()).map_err(invalid)?,
            ),
            (IrMaterialRole::ReviewPreimage, job.review_preimage()?),
        ]);
        Ok(Self { bytes })
    }
    /// Import of descriptive material. No PreparedDeviceJob or session is returned.
    /// None for an old missing review preimage remains an explicit evidence gap.
    pub fn from_retained(
        bytes: std::collections::BTreeMap<IrMaterialRole, Vec<u8>>,
    ) -> Result<Self, ServiceError> {
        for (role, data) in &bytes {
            if data.is_empty() || data.len() > role.maximum() {
                return Err(invalid(
                    "IR retained material is empty or exceeds its bound",
                ));
            }
        }
        Ok(Self { bytes })
    }
    pub fn bytes(&self, role: IrMaterialRole) -> Option<&[u8]> {
        self.bytes.get(&role).map(Vec::as_slice)
    }
    fn check(&self, binding: &IrAttemptBinding, exact_review: bool) -> Result<(), ServiceError> {
        let pins = binding.pins();
        for role in ROLES {
            let Some(data) = self.bytes(role) else {
                if role == IrMaterialRole::ReviewPreimage && !exact_review {
                    continue;
                }
                return Err(invalid(format!(
                    "IR retained material is missing: {role:?}"
                )));
            };
            if data.is_empty() || data.len() > role.maximum() || digest(data) != role.expected(pins)
            {
                return Err(invalid(format!(
                    "IR material length/hash mismatch: {role:?}"
                )));
            }
        }
        let coefficients = self
            .bytes(IrMaterialRole::Coefficients)
            .expect("checked above");
        if coefficients.len() != 4096 {
            return Err(invalid("IR coefficients must have exactly 4096 bytes"));
        }
        let source = self
            .bytes(IrMaterialRole::SourceWav)
            .expect("checked above");
        let retained = |data: &[u8]| {
            RetainedJobBlob::checked(
                AssetId(format!("sha256:{}", digest(data))),
                Arc::from(data),
                JobSha256::of(data),
            )
        };
        let name_end = pins
            .name_raw
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(16);
        let name = std::str::from_utf8(&pins.name_raw[..name_end]).map_err(invalid)?;
        let recipe = NormalAssetRecipe::ir(
            pins.target,
            name.to_owned(),
            retained(source)?,
            match pins.length_policy {
                IrAttemptLengthPolicy::RejectLong => IrJobLengthPolicy::RejectLong,
                IrAttemptLengthPolicy::KeepFirst1024 => IrJobLengthPolicy::KeepFirst1024,
            },
            retained(coefficients)?,
        )?;
        let tlv = asset_upload_candidate(pins.target, recipe.name(), recipe.derived())
            .map_err(invalid)?;
        let wire: Vec<u8> = encode_transfer(&tlv)
            .map_err(invalid)?
            .into_iter()
            .flatten()
            .collect();
        if self.bytes(IrMaterialRole::IntendedTlv) != Some(tlv.payload())
            || self.bytes(IrMaterialRole::IntendedWire) != Some(wire.as_slice())
            || tlv.payload().len() as u64 != pins.payload_bytes
            || wire.len() as u64 != pins.wire_bytes
            || u32::from_le_bytes(
                tlv.records()[0].body[4..8]
                    .try_into()
                    .expect("typed recipe"),
            ) != pins.additive_checksum
        {
            return Err(invalid(
                "IR materials do not reproduce the exact checked recipe/TLV/wire",
            ));
        }
        // Raw payloads are retained, not reconstructed into live authority.
        let baseline = RawTransfer::from_payload(
            WRITE_KIND,
            self.bytes(IrMaterialRole::BaselinePayload)
                .expect("checked above")
                .to_vec(),
        )
        .map_err(invalid)?;
        if baseline.records().len() != 1
            || gp200_protocol::AssetSlotMetadata::decode(AssetBank::Ir, &baseline.records()[0])
                .map_err(invalid)?
                .slot
                != pins.target
        {
            return Err(invalid(
                "IR baseline material must retain the exact target metadata",
            ));
        }
        let identity = RawTransfer::from_payload(
            WRITE_KIND,
            self.bytes(IrMaterialRole::IdentityPayload)
                .expect("checked above")
                .to_vec(),
        )
        .map_err(invalid)?;
        if gp200_protocol::Identity::from_transfer(&identity)
            .is_none_or(|value| value.state_raw != 1)
        {
            return Err(invalid(
                "IR identity material must retain a complete normal identity reply",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IrRetainedMaterial {
    pub role: IrMaterialRole,
    pub sha256: ContentHash,
    pub bytes: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IrRetainedImport {
    pub sha256: ContentHash,
    pub bytes: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrJournalIssue {
    IncompleteTail {
        offset: usize,
        bytes: usize,
    },
    CorruptTail {
        offset: usize,
        reason: String,
    },
    MissingReviewPreimage,
    InvalidLocalFsyncCheckpoint {
        sequence: usize,
        reason: String,
    },
    UnconfirmedLocalDurability {
        checked_prefix_bytes: usize,
        confirmed_bytes: usize,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrLocalAttemptState {
    LocalUnverified,
    Indeterminate,
}

/// Local writer receipt emitted only after journal sync_all returned success.
/// It describes local filesystem publication; it says nothing about the pedal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IrLocalFsyncBoundary {
    schema: u16,
    attempt: IrAttemptId,
    pub record_count: u16,
    pub prefix_bytes: u64,
    pub prefix_sha256: ContentHash,
    pub last_record_sha256: ContentHash,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FsyncEnvelope {
    sha256: ContentHash,
    boundary: IrLocalFsyncBoundary,
}

/// Exact bounded local checkpoint bytes, including invalid receipts. Hashing
/// these bytes does not establish that they confirm a journal fsync boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrRawCheckpointReceipt {
    pub sequence: u16,
    pub sha256: ContentHash,
    pub bytes: Vec<u8>,
}

/// A cursor permits only local ledger append in the creating process handle.
/// It is neither serializable nor a device/session/job grant.
#[derive(Debug)]
pub struct IrLedgerCursor {
    attempt: IrAttemptId,
    record_count: usize,
    last_record_sha256: ContentHash,
}
impl IrLedgerCursor {
    pub fn attempt(&self) -> &IrAttemptId {
        &self.attempt
    }
}
#[derive(Debug)]
pub struct IrRetainedAttempt {
    pub recorded: IrAttemptEvidenceDto,
    pub reconstructed: IrAttemptEvidence,
    pub materials: Vec<IrRetainedMaterial>,
    pub original_import: Option<IrRetainedImport>,
    pub journal_sha256: ContentHash,
    pub record_sha256: ContentHash,
    pub valid_prefix_bytes: usize,
    pub issues: Vec<IrJournalIssue>,
    pub confirmed_local_fsync_boundary: Option<IrLocalFsyncBoundary>,
    pub local_state: IrLocalAttemptState,
    record_count: usize,
}
impl IrRetainedAttempt {
    /// Number of accepted frames in the independently checked journal prefix.
    pub fn record_count(&self) -> usize {
        self.record_count
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LedgerRecord {
    schema: u16,
    attempt: IrAttemptId,
    sequence: u16,
    previous: Option<ContentHash>,
    entry: LedgerEntry,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum LedgerEntry {
    Started {
        binding: IrAttemptBindingDto,
        started_at_ms: u64,
        materials: Vec<IrRetainedMaterial>,
        original_import: Option<IrRetainedImport>,
    },
    Observed {
        event: IrAttemptEventDto,
    },
    Closed {
        reason: IrAttemptFinishReason,
    },
}

/// Borrows the existing LibraryStore writer lock. Never opens MIDI.
/// No reopened/imported attempt is inserted into this process's active set.
pub struct IrAttemptLedger<'a> {
    store: &'a mut LibraryStore,
    root: PathBuf,
    active: BTreeSet<String>,
    allocated_here: BTreeSet<String>,
}
impl<'a> IrAttemptLedger<'a> {
    pub fn open(store: &'a mut LibraryStore) -> Result<Self, ServiceError> {
        let root = store.root().join("ir-attempt-ledger");
        reject_symlink(&root)?;
        if !root.exists() {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&root).map_err(io)?;
            sync_dir(store.root())?;
        }
        let metadata = fs::symlink_metadata(&root).map_err(io)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(invalid("IR ledger root must be a real directory"));
        }
        let result = Self {
            store,
            root,
            active: BTreeSet::new(),
            allocated_here: BTreeSet::new(),
        };
        result.attempt_ids()?;
        Ok(result)
    }
    fn path(&self, id: &IrAttemptId, suffix: &str) -> PathBuf {
        self.root.join(format!("{}.{suffix}", id.as_str()))
    }
    fn check_roots(&self) -> Result<(), ServiceError> {
        for path in [
            self.store.root().to_owned(),
            self.store.root().join("blobs"),
            self.root.clone(),
        ] {
            let metadata = fs::symlink_metadata(&path).map_err(io)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(invalid(
                    "IR library/blob/ledger root changed to a symlink or non-directory",
                ));
            }
        }
        Ok(())
    }
    pub fn attempt_ids(&self) -> Result<Vec<IrAttemptId>, ServiceError> {
        self.check_roots()?;
        let mut ids = BTreeSet::new();
        let mut seen = 0;
        for entry in fs::read_dir(&self.root).map_err(io)? {
            seen += 1;
            if seen > MAX_ATTEMPTS * (MAX_RECORDS + 2) {
                return Err(invalid("IR ledger exceeds its retained-file bound"));
            }
            let entry = entry.map_err(io)?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| invalid("Non-UTF8 IR ledger entry"))?;
            let Some((text, suffix)) = name.rsplit_once('.') else {
                return Err(invalid("Unexpected entry in IR attempt ledger"));
            };
            let id = IrAttemptId::parse(text)?;
            let checkpoint = suffix.strip_prefix("checkpoint-").and_then(|value| {
                if value.len() != 3 || !value.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                value
                    .parse::<usize>()
                    .ok()
                    .filter(|sequence| *sequence < MAX_RECORDS)
            });
            if id.as_str() != text
                || (!matches!(suffix, "reserved" | "journal") && checkpoint.is_none())
            {
                return Err(invalid("Unexpected or noncanonical IR ledger file"));
            }
            regular(&entry.path())?;
            if suffix == "reserved" {
                ids.insert(id.as_str().to_owned());
            }
            if suffix != "reserved" && !self.path(&id, "reserved").exists() {
                return Err(invalid(
                    "IR journal is missing its permanent ID reservation",
                ));
            }
        }
        if ids.len() > MAX_ATTEMPTS {
            return Err(invalid("IR attempt limit reached"));
        }
        ids.iter().map(|text| IrAttemptId::parse(text)).collect()
    }
    fn reserve_exact(&mut self, id: &IrAttemptId) -> Result<(), ServiceError> {
        if self.attempt_ids()?.len() >= MAX_ATTEMPTS {
            return Err(invalid(
                "IR attempt limit reached; retained IDs cannot be recycled",
            ));
        }
        let path = self.path(id, "reserved");
        if path.exists() || self.path(id, "journal").exists() {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "IR attempt ID is permanently reserved",
            ));
        }
        let marker = format!("gp200-ir-observation-id-v1:{}\n", id.as_str());
        create_synced(&self.root, &path, marker.as_bytes())
    }
    /// Allocation describes local evidence only. No deletion/reuse/resume API.
    pub fn reserve_id(&mut self) -> Result<IrAttemptId, ServiceError> {
        for _ in 0..16 {
            let id = IrAttemptId::parse(&nonce()?)?;
            match self.reserve_exact(&id) {
                Ok(()) => {
                    self.allocated_here.insert(id.as_str().to_owned());
                    return Ok(id);
                }
                Err(error) if error.kind == ErrorKind::Conflict => continue,
                Err(error) => return Err(error),
            }
        }
        Err(ServiceError::new(
            ErrorKind::Conflict,
            "Unique IR observation ID allocation failed",
        ))
    }
    fn check_reserved(&self, id: &IrAttemptId) -> Result<(), ServiceError> {
        self.check_roots()?;
        let expected = format!("gp200-ir-observation-id-v1:{}\n", id.as_str());
        if read_bounded(&self.path(id, "reserved"), 128)? != expected.as_bytes() {
            return Err(invalid(
                "IR ID reservation is partial or corrupt; its ID remains consumed",
            ));
        }
        Ok(())
    }
    fn publish_materials(
        &self,
        binding: &IrAttemptBinding,
        material: &IrAttemptMaterials,
        exact_review: bool,
    ) -> Result<Vec<IrRetainedMaterial>, ServiceError> {
        self.check_roots()?;
        material.check(binding, exact_review)?;
        let mut refs = vec![];
        for (role, bytes) in &material.bytes {
            let hash = self.store.publish_blob(bytes).map_err(|error| uncertain(format!(
                "IR immutable material publication did not complete; local visibility may have changed: {error}")))?;
            if self.store.read_blob(&hash)? != *bytes {
                return Err(invalid("Newly retained IR material failed reread"));
            }
            refs.push(IrRetainedMaterial {
                role: *role,
                sha256: ContentHash::new(hash)?,
                bytes: bytes.len() as u64,
            });
        }
        Ok(refs)
    }
    fn publish_fsync_boundary(
        &self,
        id: &IrAttemptId,
        record_count: usize,
        bytes: &[u8],
        last_record: ContentHash,
    ) -> Result<(), ServiceError> {
        let boundary = IrLocalFsyncBoundary {
            schema: 1,
            attempt: id.clone(),
            record_count: record_count as u16,
            prefix_bytes: bytes.len() as u64,
            prefix_sha256: ContentHash::of(bytes),
            last_record_sha256: last_record,
        };
        let sha256 = ContentHash::of(&serde_json::to_vec(&boundary).map_err(invalid)?);
        let bytes = serde_json::to_vec(&FsyncEnvelope { sha256, boundary }).map_err(invalid)?;
        create_synced(
            &self.root,
            &self.path(id, &format!("checkpoint-{:03}", record_count - 1)),
            &bytes,
        )
    }
    pub fn begin_reserved(
        &mut self,
        id: &IrAttemptId,
        binding: IrAttemptBinding,
        materials: &IrAttemptMaterials,
        started_at_ms: u64,
    ) -> Result<IrLedgerCursor, ServiceError> {
        if !self.allocated_here.contains(id.as_str()) {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "A reopened/imported IR ID cannot start or resume local observation",
            ));
        }
        if binding.pins().attempt != *id {
            return Err(invalid("IR reservation/binding IDs differ"));
        }
        self.check_reserved(id)?;
        materials.check(&binding, true)?;
        let refs: Vec<_> = materials
            .bytes
            .iter()
            .map(|(role, bytes)| IrRetainedMaterial {
                role: *role,
                sha256: ContentHash::of(bytes),
                bytes: bytes.len() as u64,
            })
            .collect();
        let record = LedgerRecord {
            schema: 1,
            attempt: id.clone(),
            sequence: 0,
            previous: None,
            entry: LedgerEntry::Started {
                binding: binding.to_dto(),
                started_at_ms,
                materials: refs.clone(),
                original_import: None,
            },
        };
        let bytes = encode_record(&record)?;
        // Pure validation is complete. Consume start eligibility before the
        // first write, including immutable material publication that can fail
        // after earlier blobs become visible. It cannot be retried in this handle.
        self.allocated_here.remove(id.as_str());
        if self.publish_materials(&binding, materials, true)? != refs {
            return Err(uncertain(
                "IR retained material publication differs from checked descriptors",
            ));
        }
        create_synced(&self.root, &self.path(id, "journal"), &bytes)?;
        self.publish_fsync_boundary(id, 1, &bytes, ContentHash::of(&bytes))?;
        let loaded = self
            .load(id)
            .map_err(|error| uncertain(format!("IR start publication reread failed: {error}")))?
            .ok_or_else(|| invalid("Published IR start is missing"))?;
        confirm_publication(&loaded, 1, &bytes, &ContentHash::of(&bytes))?;
        self.active.insert(id.as_str().to_owned());
        Ok(IrLedgerCursor {
            attempt: id.clone(),
            record_count: 1,
            last_record_sha256: loaded.record_sha256,
        })
    }
    /// Closed imported history, including old missing-preimage evidence gaps.
    /// Import never creates an append cursor or live job/session/proof.
    pub fn import_closed(
        &mut self,
        dto: IrAttemptEvidenceDto,
        materials: &IrAttemptMaterials,
    ) -> Result<IrRetainedAttempt, ServiceError> {
        let bytes = serde_json::to_vec(&dto).map_err(invalid)?;
        self.import_closed_json(&bytes, materials)
    }
    /// Retains the original bounded JSON bytes separately from the normalized,
    /// closed domain projection. Unsupported/corrupt inputs create no journal.
    pub fn import_closed_json(
        &mut self,
        original: &[u8],
        materials: &IrAttemptMaterials,
    ) -> Result<IrRetainedAttempt, ServiceError> {
        if original.len() > 2 * 1024 * 1024 {
            return Err(invalid("Imported IR JSON exceeds 2 MiB"));
        }
        let dto: IrAttemptEvidenceDto = serde_json::from_slice(original).map_err(invalid)?;
        let reconstructed = IrAttemptEvidence::reconstruct(dto.clone())?;
        let closed = reconstructed.to_dto();
        let record_count = closed.events.len() + 2;
        let binding = reconstructed.binding();
        materials.check(binding, false)?;
        let id = binding.pins().attempt.clone();
        self.reserve_exact(&id)?;
        let refs = self.publish_materials(binding, materials, false)?;
        let original_hash = self.store.publish_blob(original).map_err(|error| {
            uncertain(format!(
                "IR imported receipt publication is unconfirmed: {error}"
            ))
        })?;
        if self.store.read_blob(&original_hash)? != original {
            return Err(invalid("IR imported receipt failed exact reread"));
        }
        let original_import = Some(IrRetainedImport {
            sha256: ContentHash::new(original_hash)?,
            bytes: original.len() as u64,
        });
        let mut bytes = vec![];
        let mut previous = None;
        let entries = std::iter::once(LedgerEntry::Started {
            binding: closed.binding,
            started_at_ms: closed.started_at_ms,
            materials: refs,
            original_import,
        })
        .chain(
            closed
                .events
                .into_iter()
                .map(|event| LedgerEntry::Observed { event }),
        )
        .chain(std::iter::once(LedgerEntry::Closed {
            reason: closed.finish.expect("reconstruct closes"),
        }));
        for (sequence, entry) in entries.enumerate() {
            let frame = encode_record(&LedgerRecord {
                schema: 1,
                attempt: id.clone(),
                sequence: sequence as u16,
                previous,
                entry,
            })?;
            previous = Some(ContentHash::of(&frame));
            bytes.extend_from_slice(&frame);
        }
        if bytes.len() as u64 > MAX_JOURNAL {
            return Err(invalid("Imported IR journal exceeds 2 MiB"));
        }
        create_synced(&self.root, &self.path(&id, "journal"), &bytes)?;
        self.publish_fsync_boundary(
            &id,
            record_count,
            &bytes,
            previous.expect("closed import has records"),
        )?;
        self.load(&id)?
            .ok_or_else(|| invalid("Imported IR journal is missing"))
    }
    fn require_append(&self, cursor: &IrLedgerCursor) -> Result<IrRetainedAttempt, ServiceError> {
        if !self.active.contains(cursor.attempt.as_str()) {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "IR observations cannot resume after reopen/import/uncertain publication",
            ));
        }
        let found = self
            .load(&cursor.attempt)?
            .ok_or_else(|| invalid("IR journal is absent"))?;
        if found.recorded.finish.is_some()
            || !found.issues.is_empty()
            || found.record_count != cursor.record_count
            || found.record_sha256 != cursor.last_record_sha256
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "IR observation cursor is stale or its journal is closed/changed",
            ));
        }
        Ok(found)
    }
    /// Append exactly one raw observation. Input order is kept even for stale or
    /// mismatched contexts/timestamps; the domain reconstruction classifies it.
    pub fn append_observation(
        &mut self,
        cursor: &IrLedgerCursor,
        event: IrAttemptEventDto,
    ) -> Result<IrLedgerCursor, ServiceError> {
        let mut found = self.require_append(cursor)?;
        found.recorded.events.push(event.clone());
        IrAttemptEvidence::reconstruct(found.recorded)?;
        self.append_record(cursor, LedgerEntry::Observed { event })
    }
    pub fn close(
        &mut self,
        cursor: &IrLedgerCursor,
        reason: IrAttemptFinishReason,
    ) -> Result<IrRetainedAttempt, ServiceError> {
        self.require_append(cursor)?;
        self.append_record(cursor, LedgerEntry::Closed { reason })?;
        self.active.remove(cursor.attempt.as_str());
        self.load(&cursor.attempt)?
            .ok_or_else(|| invalid("Closed IR journal is missing"))
    }
    fn append_record(
        &mut self,
        cursor: &IrLedgerCursor,
        entry: LedgerEntry,
    ) -> Result<IrLedgerCursor, ServiceError> {
        self.append_record_with_sync(cursor, entry, |file| file.sync_all())
    }
    fn append_record_with_sync(
        &mut self,
        cursor: &IrLedgerCursor,
        entry: LedgerEntry,
        file_sync: impl FnOnce(&File) -> std::io::Result<()>,
    ) -> Result<IrLedgerCursor, ServiceError> {
        let found = self.require_append(cursor)?;
        if cursor.record_count >= MAX_RECORDS {
            return Err(invalid("IR journal record limit reached"));
        }
        let frame = encode_record(&LedgerRecord {
            schema: 1,
            attempt: cursor.attempt.clone(),
            sequence: cursor.record_count as u16,
            previous: Some(cursor.last_record_sha256.clone()),
            entry,
        })?;
        let path = self.path(&cursor.attempt, "journal");
        if found.valid_prefix_bytes + frame.len() > MAX_JOURNAL as usize {
            return Err(invalid("IR journal exceeds 2 MiB"));
        }
        // Remove local append eligibility before a write that may be partial.
        // Only successful fsync + exact reread restores it in this same handle.
        self.active.remove(cursor.attempt.as_str());
        let mut file = open_append_checked(&path)?;
        file.write_all(&frame)
            .and_then(|()| file_sync(&file))
            .map_err(|error| {
                uncertain(format!(
                    "IR append may be visible but is not confirmed durable: {error}"
                ))
            })?;
        let raw = read_bounded(&path, MAX_JOURNAL)?;
        if raw.len() != found.valid_prefix_bytes + frame.len()
            || ContentHash::of(&raw[..found.valid_prefix_bytes]) != found.journal_sha256
            || raw[found.valid_prefix_bytes..] != frame
        {
            return Err(uncertain(
                "IR append reread did not confirm exact prior bytes and new frame",
            ));
        }
        self.publish_fsync_boundary(
            &cursor.attempt,
            cursor.record_count + 1,
            &raw,
            ContentHash::of(&frame),
        )
        .map_err(|error| {
            uncertain(format!(
                "IR append fsync checkpoint publication failed: {error}"
            ))
        })?;
        let loaded = self
            .load(&cursor.attempt)
            .map_err(|error| uncertain(format!("IR append publication reread failed: {error}")))?
            .ok_or_else(|| invalid("IR append disappeared"))?;
        confirm_publication(
            &loaded,
            cursor.record_count + 1,
            &raw,
            &ContentHash::of(&frame),
        )?;
        self.active.insert(cursor.attempt.as_str().to_owned());
        Ok(IrLedgerCursor {
            attempt: cursor.attempt.clone(),
            record_count: loaded.record_count,
            last_record_sha256: loaded.record_sha256,
        })
    }
    /// Recompute bounded raw evidence. Never repairs, truncates, replays device
    /// commands, or recreates this process's append eligibility.
    pub fn load(&self, id: &IrAttemptId) -> Result<Option<IrRetainedAttempt>, ServiceError> {
        self.check_reserved(id)?;
        let path = self.path(id, "journal");
        reject_symlink(&path)?;
        if !path.exists() {
            return Ok(None);
        }
        let bytes = read_bounded(&path, MAX_JOURNAL)?;
        let mut offset = 0;
        let mut previous = None;
        let mut count = 0;
        let mut dto: Option<IrAttemptEvidenceDto> = None;
        let mut projection: Option<IrAttemptEvidence> = None;
        let mut materials = vec![];
        let mut original_import = None;
        let mut issues = vec![];
        let mut boundaries: Vec<(usize, ContentHash, ContentHash)> = vec![];
        let mut prefix_hash = Sha256::new();
        while offset < bytes.len() {
            let start = offset;
            let parsed = decode_record(&bytes, offset);
            let (record, frame_len) = match parsed {
                Ok(value) => value,
                Err(issue) => {
                    issues.push(issue);
                    break;
                }
            };
            // A rejected complete frame must not leak its tentative event or
            // finish mutation into the independently retained valid prefix.
            let before_dto = dto.clone();
            let before_materials = materials.clone();
            let before_import = original_import.clone();
            let accept = (|| {
                if record.schema != 1
                    || record.attempt != *id
                    || usize::from(record.sequence) != count
                    || record.previous != previous
                    || count >= MAX_RECORDS
                {
                    return Err(invalid(
                        "IR journal schema/identity/sequence/hash chain differs",
                    ));
                }
                match record.entry {
                    LedgerEntry::Started {
                        binding,
                        started_at_ms,
                        materials: refs,
                        original_import: imported,
                    } => {
                        if count != 0 || binding.attempt != *id {
                            return Err(invalid("Duplicate or cross-attempt IR start"));
                        }
                        let checked = IrAttemptBinding::from_dto(binding.clone())?;
                        if checked.to_dto() != binding {
                            return Err(invalid("IR journal start is not canonically checked"));
                        }
                        materials = refs;
                        original_import = imported;
                        dto = Some(IrAttemptEvidenceDto {
                            schema: 1,
                            binding: checked.to_dto(),
                            started_at_ms,
                            events: vec![],
                            finish: None,
                        });
                        projection = Some(IrAttemptEvidence::start(checked, started_at_ms));
                    }
                    LedgerEntry::Observed { event } => {
                        let state = dto
                            .as_mut()
                            .ok_or_else(|| invalid("IR event precedes start"))?;
                        if state.finish.is_some() {
                            return Err(invalid("IR event follows closure"));
                        }
                        apply_observation(
                            projection
                                .as_mut()
                                .ok_or_else(|| invalid("IR projection precedes start"))?,
                            event.clone(),
                        )?;
                        state.events.push(event);
                    }
                    LedgerEntry::Closed { reason } => {
                        let state = dto
                            .as_mut()
                            .ok_or_else(|| invalid("IR finish precedes start"))?;
                        if state.finish.replace(reason).is_some() {
                            return Err(invalid("Duplicate IR closure"));
                        }
                        projection
                            .as_mut()
                            .ok_or_else(|| invalid("IR projection precedes start"))?
                            .finish(reason)?;
                    }
                }
                Ok(())
            })();
            if let Err(error) = accept {
                // Rebuild only this exceptional rejected-frame path from the
                // retained prefix. The domain evidence type is not Clone, and
                // tentative observer state must never leak into the prefix.
                projection = before_dto
                    .as_ref()
                    .map(|prefix| IrAttemptEvidence::reconstruct(prefix.clone()))
                    .transpose()?;
                dto = before_dto;
                materials = before_materials;
                original_import = before_import;
                issues.push(IrJournalIssue::CorruptTail {
                    offset: start,
                    reason: error.to_string(),
                });
                break;
            }
            offset += frame_len;
            previous = Some(ContentHash::of(&bytes[start..offset]));
            prefix_hash.update(&bytes[start..offset]);
            boundaries.push((
                offset,
                previous.clone().expect("record hash"),
                ContentHash::new(format!("{:x}", prefix_hash.clone().finalize()))?,
            ));
            count += 1;
        }
        let Some(recorded) = dto else {
            return Err(invalid(
                "IR journal has no complete checked start; raw file and reserved ID were retained",
            ));
        };
        // Check the whole accepted prefix independently once, rather than
        // replaying every preceding event for every frame. Incremental parsing
        // uses the same domain observer APIs and rolls back a rejected frame.
        let checked_prefix = IrAttemptEvidence::reconstruct(recorded.clone())?;
        let mut incremental =
            projection.ok_or_else(|| invalid("IR checked prefix projection is missing"))?;
        if incremental.finish_reason().is_none() {
            incremental.finish(IrAttemptFinishReason::InterruptedByRestart)?;
        }
        if incremental.to_dto() != checked_prefix.to_dto() {
            return Err(invalid(
                "IR incremental projection differs from independent accepted-prefix reconstruction",
            ));
        }
        let tail_interrupted = !issues.is_empty();
        let mut confirmed_local_fsync_boundary = None;
        for sequence in 0..MAX_RECORDS {
            let path = self.path(id, &format!("checkpoint-{sequence:03}"));
            reject_symlink(&path)?;
            if !path.exists() {
                continue;
            }
            let check = (|| {
                let envelope: FsyncEnvelope =
                    serde_json::from_slice(&read_bounded(&path, 2048)?).map_err(invalid)?;
                let boundary = envelope.boundary;
                if envelope.sha256
                    != ContentHash::of(&serde_json::to_vec(&boundary).map_err(invalid)?)
                    || boundary.schema != 1
                    || boundary.attempt != *id
                    || usize::from(boundary.record_count) != sequence + 1
                {
                    return Err(invalid(
                        "Local IR fsync checkpoint schema/identity/hash differs",
                    ));
                }
                let Some((end, last, prefix)) = boundaries.get(sequence) else {
                    return Err(invalid(
                        "Local IR fsync checkpoint exceeds the checked journal prefix",
                    ));
                };
                if boundary.prefix_bytes != *end as u64
                    || boundary.last_record_sha256 != *last
                    || boundary.prefix_sha256 != *prefix
                {
                    return Err(invalid(
                        "Local IR fsync checkpoint does not match exact prefix bytes",
                    ));
                }
                Ok(boundary)
            })();
            match check {
                Ok(boundary) => confirmed_local_fsync_boundary = Some(boundary),
                Err(error) => issues.push(IrJournalIssue::InvalidLocalFsyncCheckpoint {
                    sequence,
                    reason: error.to_string(),
                }),
            }
        }
        let confirmed_bytes = confirmed_local_fsync_boundary
            .as_ref()
            .map_or(0, |b| b.prefix_bytes as usize);
        if confirmed_bytes != offset {
            issues.push(IrJournalIssue::UnconfirmedLocalDurability {
                checked_prefix_bytes: offset,
                confirmed_bytes,
            });
        }
        let mut material_bytes = std::collections::BTreeMap::new();
        for retained in &materials {
            if retained.bytes == 0
                || retained.bytes > retained.role.maximum() as u64
                || retained.sha256.as_str() != retained.role.expected(&recorded.binding)
                || material_bytes.contains_key(&retained.role)
            {
                return Err(invalid(
                    "IR material descriptor is duplicate or differs from binding",
                ));
            }
            let data = self.store.read_blob(retained.sha256.as_str())?;
            if data.len() as u64 != retained.bytes {
                return Err(invalid("IR retained material byte length differs"));
            }
            material_bytes.insert(retained.role, data);
        }
        let checked = IrAttemptBinding::from_dto(recorded.binding.clone())?;
        let material = IrAttemptMaterials::from_retained(material_bytes)?;
        material.check(&checked, false)?;
        if material.bytes(IrMaterialRole::ReviewPreimage).is_none() {
            issues.push(IrJournalIssue::MissingReviewPreimage);
        }
        if let Some(imported) = &original_import {
            if imported.bytes == 0 || imported.bytes > 2 * 1024 * 1024 {
                return Err(invalid("IR original imported receipt length is invalid"));
            }
            let raw = self.store.read_blob(imported.sha256.as_str())?;
            if raw.len() as u64 != imported.bytes {
                return Err(invalid("IR original imported receipt length differs"));
            }
            let original = IrAttemptEvidence::from_json(&raw)?.to_dto();
            if original.binding != recorded.binding
                || original.started_at_ms != recorded.started_at_ms
                || !original.events.starts_with(&recorded.events)
                || (recorded.finish.is_some() && original.finish != recorded.finish)
            {
                return Err(invalid(
                    "IR imported journal differs from its retained original projection",
                ));
            }
        }
        // Preserve recorded.finish exactly, including a real finish in a clean
        // prefix followed by bad trailing bytes. Only the separate projection
        // receives synthetic interruption; it is not an appended record.
        let mut reconstructed_dto = recorded.clone();
        if tail_interrupted {
            reconstructed_dto.finish = Some(IrAttemptFinishReason::InterruptedByRestart);
        }
        let reconstructed = IrAttemptEvidence::reconstruct(reconstructed_dto)?;
        let local_state = if issues.is_empty()
            && reconstructed
                .summary()
                .expect("reconstruct closes")
                .final_state
                == IrAttemptFinalState::ContentUnverified
        {
            IrLocalAttemptState::LocalUnverified
        } else {
            IrLocalAttemptState::Indeterminate
        };
        Ok(Some(IrRetainedAttempt {
            recorded,
            reconstructed,
            materials,
            original_import,
            journal_sha256: ContentHash::of(&bytes),
            record_sha256: previous.expect("complete checked start"),
            valid_prefix_bytes: offset,
            issues,
            confirmed_local_fsync_boundary,
            local_state,
            record_count: count,
        }))
    }
    /// Exact journal bytes (including any retained tail), after checked read.
    /// A future English read-only UI can export this with material blobs.
    pub fn export_journal(&self, id: &IrAttemptId) -> Result<Vec<u8>, ServiceError> {
        self.load(id)?
            .ok_or_else(|| invalid("IR journal has not started"))?;
        read_bounded(&self.path(id, "journal"), MAX_JOURNAL)
    }
    /// Diagnostic bytes only, even when the start or materials cannot load.
    /// No prefix, observation, local durability or device content is certified.
    pub fn export_raw_journal(&self, id: &IrAttemptId) -> Result<Vec<u8>, ServiceError> {
        self.check_reserved(id)?;
        read_bounded(&self.path(id, "journal"), MAX_JOURNAL)
    }
    /// Retain exact checkpoint bytes for local evidence export. Invalid JSON or
    /// hashes remain descriptive bytes; this does not reconstruct a boundary.
    pub fn raw_checkpoint_receipts(
        &self,
        id: &IrAttemptId,
    ) -> Result<Vec<IrRawCheckpointReceipt>, ServiceError> {
        self.check_reserved(id)?;
        let mut receipts = Vec::new();
        for sequence in 0..MAX_RECORDS {
            let path = self.path(id, &format!("checkpoint-{sequence:03}"));
            reject_symlink(&path)?;
            if !path.exists() {
                continue;
            }
            let bytes = read_bounded(&path, 2048)?;
            receipts.push(IrRawCheckpointReceipt {
                sequence: sequence as u16,
                sha256: ContentHash::of(&bytes),
                bytes,
            });
        }
        Ok(receipts)
    }
    pub fn material_bytes(&self, retained: &IrRetainedMaterial) -> Result<Vec<u8>, ServiceError> {
        self.check_roots()?;
        let bytes = self.store.read_blob(retained.sha256.as_str())?;
        if bytes.len() as u64 != retained.bytes || bytes.len() > retained.role.maximum() {
            return Err(invalid("IR material export length mismatch"));
        }
        Ok(bytes)
    }
    pub fn original_import_bytes(
        &self,
        retained: &IrRetainedImport,
    ) -> Result<Vec<u8>, ServiceError> {
        self.check_roots()?;
        if retained.bytes == 0 || retained.bytes > 2 * 1024 * 1024 {
            return Err(invalid("IR import export length is invalid"));
        }
        let bytes = self.store.read_blob(retained.sha256.as_str())?;
        if bytes.len() as u64 != retained.bytes {
            return Err(invalid("IR import export length differs"));
        }
        Ok(bytes)
    }
}

fn apply_observation(
    evidence: &mut IrAttemptEvidence,
    event: IrAttemptEventDto,
) -> Result<(), ServiceError> {
    match event {
        IrAttemptEventDto::Send {
            context,
            at_ms,
            extent,
            wire_bytes,
            completion,
        } => evidence.observe_send(
            IrObservationContext::from_dto(context)?,
            at_ms,
            extent,
            wire_bytes,
            completion,
        ),
        IrAttemptEventDto::Status {
            context,
            at_ms,
            raw,
        } => evidence.observe_status(IrObservationContext::from_dto(context)?, at_ms, raw),
        IrAttemptEventDto::Metadata {
            context,
            at_ms,
            metadata,
        } => {
            let snapshot = DeviceAssetMetadata::from_transfer(
                metadata.slot,
                metadata.stamp,
                metadata.revision,
                metadata.source,
                metadata.captured_at_ms,
                metadata.raw,
            )?;
            evidence.observe_metadata(IrObservationContext::from_dto(context)?, at_ms, snapshot)
        }
    }
}

fn confirm_publication(
    loaded: &IrRetainedAttempt,
    record_count: usize,
    raw: &[u8],
    last_record: &ContentHash,
) -> Result<(), ServiceError> {
    let hash = ContentHash::of(raw);
    if !loaded.issues.is_empty()
        || loaded.valid_prefix_bytes != raw.len()
        || loaded.record_count != record_count
        || &loaded.record_sha256 != last_record
        || loaded.journal_sha256 != hash
        || loaded
            .confirmed_local_fsync_boundary
            .as_ref()
            .is_none_or(|boundary| {
                usize::from(boundary.record_count) != record_count
                    || boundary.prefix_bytes != raw.len() as u64
                    || boundary.prefix_sha256 != hash
                    || &boundary.last_record_sha256 != last_record
            })
    {
        return Err(uncertain(
            "IR publication reread did not confirm an issue-free exact prefix and local fsync boundary",
        ));
    }
    Ok(())
}

fn encode_record(record: &LedgerRecord) -> Result<Vec<u8>, ServiceError> {
    let json = serde_json::to_vec(record).map_err(invalid)?;
    if json.len() > MAX_RECORD {
        return Err(invalid("IR record exceeds 64 KiB"));
    }
    let mut frame = Vec::with_capacity(HEADER + json.len());
    frame.extend_from_slice(MAGIC);
    frame.extend_from_slice(&(json.len() as u32).to_le_bytes());
    frame.extend_from_slice(&Sha256::digest(&json));
    frame.extend_from_slice(&json);
    Ok(frame)
}
fn decode_record(bytes: &[u8], offset: usize) -> Result<(LedgerRecord, usize), IrJournalIssue> {
    let remaining = &bytes[offset..];
    let incomplete = || IrJournalIssue::IncompleteTail {
        offset,
        bytes: remaining.len(),
    };
    let corrupt = |reason: &str| IrJournalIssue::CorruptTail {
        offset,
        reason: reason.to_owned(),
    };
    if remaining.len() < HEADER {
        return Err(incomplete());
    }
    if &remaining[..8] != MAGIC {
        return Err(corrupt("IR frame magic differs"));
    }
    let length = u32::from_le_bytes(remaining[8..12].try_into().expect("bounded header")) as usize;
    if length > MAX_RECORD {
        return Err(corrupt("IR frame exceeds 64 KiB"));
    }
    if remaining.len() < HEADER + length {
        return Err(incomplete());
    }
    let json = &remaining[HEADER..HEADER + length];
    if Sha256::digest(json)[..] != remaining[12..44] {
        return Err(corrupt("IR frame JSON hash differs"));
    }
    let record = serde_json::from_slice(json).map_err(|_| corrupt("IR frame JSON rejected"))?;
    Ok((record, HEADER + length))
}
fn uncertain(message: impl Into<String>) -> ServiceError {
    ServiceError::new(ErrorKind::Indeterminate, message)
}
fn sync_dir(path: &Path) -> Result<(), ServiceError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| {
            uncertain(format!(
                "IR directory publication may be visible but fsync failed: {error}"
            ))
        })
}
fn regular(path: &Path) -> Result<(), ServiceError> {
    let named = fs::symlink_metadata(path).map_err(io)?;
    if !named.is_file() || named.file_type().is_symlink() {
        return Err(invalid("IR journal/reservation must be a regular file"));
    }
    Ok(())
}
fn create_synced(root: &Path, path: &Path, bytes: &[u8]) -> Result<(), ServiceError> {
    create_synced_with_sync(
        root,
        path,
        bytes,
        |file| file.sync_all(),
        |dir| File::open(dir).and_then(|file| file.sync_all()),
    )
}
fn create_synced_with_sync(
    root: &Path,
    path: &Path,
    bytes: &[u8],
    file_sync: impl FnOnce(&File) -> std::io::Result<()>,
    directory_sync: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<(), ServiceError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            ServiceError::new(
                ErrorKind::Conflict,
                "IR ID or journal already exists; it cannot be reused",
            )
        } else {
            io(error)
        }
    })?;
    // Keep any partial file as evidence and permanent ID occupancy on failure.
    file.write_all(bytes)
        .and_then(|()| file_sync(&file))
        .map_err(|error| {
            uncertain(format!(
                "IR publication may be partial/visible; its file is retained: {error}"
            ))
        })?;
    directory_sync(root).map_err(|error| {
        uncertain(format!(
            "IR directory publication may be visible but fsync failed: {error}"
        ))
    })
}

fn open_append_checked(path: &Path) -> Result<File, ServiceError> {
    regular(path)?;
    let file = OpenOptions::new().append(true).open(path).map_err(io)?;
    let opened = file.metadata().map_err(io)?;
    let named = fs::symlink_metadata(path).map_err(io)?;
    if !opened.is_file() || !named.is_file() || named.file_type().is_symlink() {
        return Err(invalid("IR journal changed while opening"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (opened.dev(), opened.ino()) != (named.dev(), named.ino()) {
            return Err(invalid("IR journal inode changed while opening"));
        }
    }
    Ok(file)
}

#[cfg(test)]
mod local_publication_tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("gp200-ir-sync-{}", nonce().unwrap()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    const WAV: &[u8] =
        include_bytes!("../../../tests/fixtures/device-assets/synthetic-ir-pcm24.wav");
    fn fixture() -> (PreparedDeviceJob, RawTransfer) {
        let source = WAV;
        let coefficients =
            include_bytes!("../../../tests/fixtures/device-assets/synthetic-ir-coefficients.bin");
        let retained = |data: &[u8]| {
            RetainedJobBlob::checked(AssetId(digest(data)), Arc::from(data), JobSha256::of(data))
                .unwrap()
        };
        let identity = RawTransfer::from_records(
            WRITE_KIND,
            &[gp200_protocol::RawRecord {
                command: 0x2001,
                body: vec![1, 0, 2, 0],
            }],
        )
        .unwrap();
        let session = DeviceJobSession::observe_normal(
            SessionEpoch(7),
            DeviceSelection {
                input_id: "fixture:in".into(),
                output_id: "fixture:out".into(),
                fingerprint_hint: None,
            },
            Revision(1),
            &identity,
            &DeviceIdentity {
                model: DeviceModel::Gp200,
                mode: BackendMode::Hardware,
                firmware: None,
                display_name: "Offline fixture".into(),
            },
            &Capabilities::gp200_read_only(),
        )
        .unwrap();
        let target = AssetSlot::new(AssetBank::Ir, 19).unwrap();
        let mut body = 19u16.to_le_bytes().to_vec();
        body.extend_from_slice(&[0; 22]);
        let baseline = DeviceAssetMetadata::from_transfer(
            target,
            RequestStamp {
                epoch: SessionEpoch(7),
                intent: IntentId(1),
            },
            Revision(2),
            SnapshotSource::Hardware {
                epoch: SessionEpoch(7),
            },
            99,
            RawTransfer::from_records(
                WRITE_KIND,
                &[gp200_protocol::RawRecord {
                    command: 0x1009,
                    body,
                }],
            )
            .unwrap(),
        )
        .unwrap();
        let recipe = NormalAssetRecipe::ir(
            target,
            "Linux IR test".into(),
            retained(source),
            IrJobLengthPolicy::RejectLong,
            retained(coefficients),
        )
        .unwrap();
        let options: Arc<[u8]> = Arc::from(b"{}".as_slice());
        let job = PreparedDeviceJob::prepare(
            JobId(4),
            session,
            retained(source),
            recipe,
            JobConversion::checked(
                "gp200-ir-44100-pcm24-v1".into(),
                options.clone(),
                JobSha256::of(&options),
                std::collections::BTreeMap::new(),
            )
            .unwrap(),
            Arc::new(baseline),
            AssetBackupReview::Unavailable {
                reason: "Only metadata retained".into(),
            },
        )
        .unwrap();
        (job, identity)
    }
    #[test]
    fn reservation_file_and_directory_sync_failures_keep_visible_id_consumed() {
        let root = Temp::new();
        for (name, fail_file) in [("file", true), ("directory", false)] {
            let path = root.0.join(name);
            let result = create_synced_with_sync(
                &root.0,
                &path,
                b"reserved",
                |file| {
                    if fail_file {
                        Err(std::io::Error::other("injected file fsync failure"))
                    } else {
                        file.sync_all()
                    }
                },
                |_| {
                    if !fail_file {
                        Err(std::io::Error::other("injected directory fsync failure"))
                    } else {
                        Ok(())
                    }
                },
            );
            assert_eq!(result.unwrap_err().kind, ErrorKind::Indeterminate);
            assert_eq!(fs::read(&path).unwrap(), b"reserved");
            assert_eq!(
                create_synced(&root.0, &path, b"replacement")
                    .unwrap_err()
                    .kind,
                ErrorKind::Conflict
            );
            assert_eq!(fs::read(&path).unwrap(), b"reserved");
        }
    }
    #[test]
    fn append_fsync_failure_keeps_complete_unconfirmed_frame_and_poisoned_cursor() {
        let root = Temp::new();
        let mut store = LibraryStore::open(&root.0).unwrap();
        let (job, identity) = fixture();
        let source = WAV;
        let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
        let id = ledger.reserve_id().unwrap();
        let binding = IrAttemptBinding::from_prepared(id.clone(), &job).unwrap();
        let cursor = ledger
            .begin_reserved(
                &id,
                binding.clone(),
                &IrAttemptMaterials::from_prepared(&job, source, identity.payload()).unwrap(),
                100,
            )
            .unwrap();
        let old = ledger.load(&id).unwrap().unwrap();
        let event = IrAttemptEventDto::Send {
            context: binding.context().to_dto(),
            at_ms: 101,
            extent: IrTransportExtent::OfferedToProcess,
            wire_bytes: 8570,
            completion: IrSendCompletion::InProgress,
        };
        let error = ledger
            .append_record_with_sync(&cursor, LedgerEntry::Observed { event }, |_| {
                Err(std::io::Error::other("injected append fsync failure"))
            })
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Indeterminate);
        let found = ledger.load(&id).unwrap().unwrap();
        assert_eq!(found.recorded.events.len(), 1);
        assert!(found.valid_prefix_bytes > old.valid_prefix_bytes);
        assert_eq!(
            found.confirmed_local_fsync_boundary,
            old.confirmed_local_fsync_boundary
        );
        assert!(
            found
                .issues
                .iter()
                .any(|issue| matches!(issue, IrJournalIssue::UnconfirmedLocalDurability { .. }))
        );
        assert_eq!(found.local_state, IrLocalAttemptState::Indeterminate);
        assert_eq!(
            ledger
                .close(&cursor, IrAttemptFinishReason::ObservationComplete)
                .unwrap_err()
                .kind,
            ErrorKind::Conflict
        );
    }
    #[test]
    fn partial_material_publication_consumes_start_before_journal_creation() {
        let root = Temp::new();
        let mut store = LibraryStore::open(&root.0).unwrap();
        let (job, identity) = fixture();
        let materials = IrAttemptMaterials::from_prepared(&job, WAV, identity.payload()).unwrap();
        // Source publication succeeds first; the following coefficient blob
        // cannot be read or published because its reserved hash path is a directory.
        let blocker = root.0.join("blobs").join(digest(
            materials.bytes(IrMaterialRole::Coefficients).unwrap(),
        ));
        fs::create_dir(&blocker).unwrap();
        let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
        let id = ledger.reserve_id().unwrap();
        let binding = IrAttemptBinding::from_prepared(id.clone(), &job).unwrap();
        assert_eq!(
            ledger
                .begin_reserved(&id, binding.clone(), &materials, 100)
                .unwrap_err()
                .kind,
            ErrorKind::Indeterminate
        );
        assert!(root.0.join("blobs").join(digest(WAV)).is_file());
        assert!(!ledger.path(&id, "journal").exists());
        assert_eq!(ledger.attempt_ids().unwrap(), vec![id.clone()]);
        fs::remove_dir(blocker).unwrap();
        // Even after the local obstruction is gone, no second publication can
        // start under this consumed ID. A reopened handle also cannot start it.
        assert_eq!(
            ledger
                .begin_reserved(&id, binding.clone(), &materials, 100)
                .unwrap_err()
                .kind,
            ErrorKind::Conflict
        );
        drop(ledger);
        let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
        assert_eq!(
            ledger
                .begin_reserved(&id, binding, &materials, 100)
                .unwrap_err()
                .kind,
            ErrorKind::Conflict
        );
        assert!(!ledger.path(&id, "journal").exists());
    }
    #[test]
    fn final_reread_issue_consumes_cursor_even_with_exact_new_frame_and_full_boundary() {
        let root = Temp::new();
        let mut store = LibraryStore::open(&root.0).unwrap();
        let (job, identity) = fixture();
        let mut ledger = IrAttemptLedger::open(&mut store).unwrap();
        let id = ledger.reserve_id().unwrap();
        let binding = IrAttemptBinding::from_prepared(id.clone(), &job).unwrap();
        let cursor = ledger
            .begin_reserved(
                &id,
                binding.clone(),
                &IrAttemptMaterials::from_prepared(&job, WAV, identity.payload()).unwrap(),
                100,
            )
            .unwrap();
        let start_checkpoint = ledger.path(&id, "checkpoint-000");
        let original_checkpoint = fs::read(&start_checkpoint).unwrap();
        let event = IrAttemptEventDto::Send {
            context: binding.context().to_dto(),
            at_ms: 101,
            extent: IrTransportExtent::OfferedToProcess,
            wire_bytes: 8570,
            completion: IrSendCompletion::InProgress,
        };
        let error = ledger
            .append_record_with_sync(&cursor, LedgerEntry::Observed { event }, |file| {
                file.sync_all()?;
                // A file changes between the prior checked read and final reread.
                // The new frame and checkpoint will still be exact and complete.
                fs::write(&start_checkpoint, b"corrupt prior checkpoint")
            })
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Indeterminate);
        let found = ledger.load(&id).unwrap().unwrap();
        assert_eq!(found.record_count, 2);
        assert_eq!(
            found
                .confirmed_local_fsync_boundary
                .as_ref()
                .unwrap()
                .record_count,
            2
        );
        assert!(found.issues.iter().any(|issue| matches!(
            issue,
            IrJournalIssue::InvalidLocalFsyncCheckpoint { sequence: 0, .. }
        )));
        let raw = ledger.export_journal(&id).unwrap();
        fs::write(start_checkpoint, original_checkpoint).unwrap();
        assert!(ledger.load(&id).unwrap().unwrap().issues.is_empty());
        // Fixing a local file cannot revive publication eligibility after the
        // final read failed; the raw journal stays unchanged.
        assert_eq!(
            ledger
                .close(&cursor, IrAttemptFinishReason::ObservationComplete)
                .unwrap_err()
                .kind,
            ErrorKind::Conflict
        );
        assert_eq!(ledger.export_journal(&id).unwrap(), raw);
    }
}
