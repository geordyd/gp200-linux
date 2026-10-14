//! Descriptions of a local updater lifecycle. Deserializing these grants no I/O.
use crate::*;
use serde::{Deserialize, Serialize};

pub const FIRMWARE_NONBLOCK_ACTION_LIMIT: u64 = 32;
pub const FIRMWARE_RAW_CHUNK_LIMIT: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirmwareLifecycleKind {
    EntryTransition,
    IdentityQuery,
    ImageAnnouncement,
    SectionRequest,
    Descriptor,
    ImageBlock,
    SectionTerminator,
    TerminalQuery,
    TerminalControl,
    ProfiledReboot,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareUsbObservation {
    pub topology: String,
    pub vendor: u16,
    pub product: u16,
    pub serial: String,
    pub interface: String,
    pub endpoint: String,
}
impl FirmwareUsbObservation {
    pub fn validate(&self) -> Result<(), ServiceError> {
        for field in [
            &self.topology,
            &self.serial,
            &self.interface,
            &self.endpoint,
        ] {
            if field.is_empty() || field.len() > 256 || field.chars().any(char::is_control) {
                return Err(ServiceError::invalid(
                    "Firmware USB observation is incomplete or exceeds bounds",
                ));
            }
        }
        Ok(())
    }
    pub fn digest(&self) -> String {
        JobSha256::of(&serde_json::to_vec(self).expect("USB observation serializes")).hex()
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareLifecycleActionDescription {
    pub ordinal: u64,
    pub kind: FirmwareLifecycleKind,
    pub wire: Vec<u8>,
    pub wire_sha256: String,
    pub block: Option<FirmwareBlockDescription>,
    pub epoch: SessionEpoch,
    pub binding_sha256: String,
    pub generation: u64,
    pub rule_sha256: String,
}
impl FirmwareLifecycleActionDescription {
    pub fn validate(&self, job: &ReviewedFirmwareUpdate) -> Result<(), ServiceError> {
        if self.wire.is_empty()
            || self.wire.len() > 518
            || self.wire_sha256 != JobSha256::of(&self.wire).hex()
            || self.generation == 0
            || JobSha256::parse(&self.binding_sha256)?.hex() != self.binding_sha256
            || JobSha256::parse(&self.rule_sha256)?.hex() != self.rule_sha256
        {
            return Err(ServiceError::invalid(
                "Firmware action wire/binding/rule differs",
            ));
        }
        match (&self.block, self.kind) {
            (Some(b), FirmwareLifecycleKind::ImageBlock) if job.block_wire(b)? == self.wire => {
                Ok(())
            }
            (None, k) if k != FirmwareLifecycleKind::ImageBlock => Ok(()),
            _ => Err(ServiceError::invalid(
                "Firmware action source coordinates differ",
            )),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareLifecyclePlanDto {
    pub review_sha256: String,
    pub profile_sha256: String,
    pub origin: FirmwareObservationOrigin,
    pub nonblock_actions: u64,
    pub epoch: SessionEpoch,
    pub binding: FirmwareUsbObservation,
    pub generation: u64,
    pub clock_origin: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareLifecycleBudget {
    pub actions: u64,
    pub raw_records: u64,
    pub frames: u64,
    pub bytes: u64,
}
impl FirmwareLifecyclePlanDto {
    pub fn validate(
        &self,
        job: &ReviewedFirmwareUpdate,
    ) -> Result<FirmwareLifecycleBudget, ServiceError> {
        self.binding.validate()?;
        if self.review_sha256 != job.digest().hex()
            || JobSha256::parse(&self.profile_sha256)?.hex() != self.profile_sha256
            || self.nonblock_actions > FIRMWARE_NONBLOCK_ACTION_LIMIT
            || self.generation == 0
            || self.clock_origin.is_empty()
            || self.clock_origin.len() > 128
        {
            return Err(ServiceError::invalid(
                "Firmware lifecycle plan differs or exceeds bounds",
            ));
        }
        FirmwareLifecycleBudget::checked(job.budget().blocks, self.nonblock_actions)
    }
}
impl FirmwareLifecycleBudget {
    pub fn checked(blocks: u64, nonblock: u64) -> Result<Self, ServiceError> {
        let bad =
            || ServiceError::invalid("Firmware lifecycle evidence exceeds finite journal budget");
        if nonblock > 32 {
            return Err(bad());
        }
        let a = blocks.checked_add(nonblock).ok_or_else(bad)?;
        let c = a
            .checked_mul(2)
            .and_then(|v| v.checked_add(64))
            .ok_or_else(bad)?;
        let frames = a
            .checked_mul(3)
            .and_then(|v| v.checked_add(c + 1 + 64 + 128))
            .ok_or_else(bad)?;
        let bytes = a
            .checked_mul(4096 + 1024 + 2048 + 3 * 44)
            .and_then(|v| c.checked_mul(8192 + 44).and_then(|r| v.checked_add(r)))
            .and_then(|v| v.checked_add(16384 + 44 + (64 + 128) * (8192 + 44)))
            .ok_or_else(bad)?;
        if frames > FIRMWARE_FRAME_LIMIT || bytes > FIRMWARE_JOURNAL_LIMIT {
            return Err(bad());
        }
        Ok(Self {
            actions: a,
            raw_records: c,
            frames,
            bytes,
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareLifecycleRawObservation {
    pub sequence: u64,
    pub at_ns: u64,
    pub epoch: SessionEpoch,
    pub generation: u64,
    pub binding_sha256: String,
    pub clock_origin: String,
    pub raw: Vec<u8>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareLifecycleSendObservation {
    pub action: u64,
    pub wire_sha256: String,
    pub attempted: u64,
    pub extent: FirmwareOsExtent,
    pub drain: FirmwareDrainObservation,
    pub started_sequence: u64,
    pub finished_sequence: Option<u64>,
    pub started_at_ns: u64,
    pub finished_at_ns: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareLifecycleDecision {
    pub action: u64,
    pub rule_sha256: String,
    pub first_raw: u64,
    pub last_raw: u64,
    pub observed_frontier: u64,
    pub mock_matched: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirmwareLifecycleHostClosure {
    ClosedBeforeAnyOffer,
    HostStoppedUnresolved,
    MockFinished,
}
/// Three descriptive source roles. Imported records cannot issue a live
/// reader-close receipt, continuation, opener, or physical permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirmwareLifecycleEpochRole {
    NormalEntry,
    Bootloader,
    PostUpdateNormal,
}

pub const FIRMWARE_LIFECYCLE_EPOCH_LIMIT: usize = 3;
pub const FIRMWARE_LIFECYCLE_EPOCH_PAYLOAD_LIMIT: usize = 8 * 1024;
pub const FIRMWARE_LIFECYCLE_IDENTITY_LIMIT: usize = 512;
pub const FIRMWARE_NORMAL_RETIREMENT_RAW_LIMIT: usize = 64;
pub const FIRMWARE_NORMAL_RETIREMENT_CHUNK_LIMIT: usize = 4096;
pub const FIRMWARE_NORMAL_RETIREMENT_GAP_LIMIT: usize = 64;
pub const FIRMWARE_NORMAL_RETIREMENT_ENCODED_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirmwareNormalReaderJoinObservation {
    Returned,
    Panicked,
    Unavailable,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareNormalRetirementRawObservation {
    pub sequence: u64,
    pub epoch: SessionEpoch,
    pub at_ns: Option<u64>,
    pub original_length: u64,
    pub raw: Vec<u8>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareNormalRetirementGap {
    pub source_instance: String,
    pub first_sequence: u64,
    pub last_sequence: u64,
    pub missing_length: Option<u64>,
    pub reason: String,
}
/// Bounded originals and unknown extents from an actually retired normal
/// source. This public description is deliberately not retirement authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareNormalRetirementObservation {
    pub normal_epoch: SessionEpoch,
    pub owner_generation: u64,
    pub source_instance: String,
    pub clock_origin: String,
    pub binding: FirmwareUsbObservation,
    pub close_started_at_ns: Option<u64>,
    pub close_finished_at_ns: Option<u64>,
    pub allocated_before: u64,
    pub processed_before: u64,
    pub allocated_after: u64,
    pub processed_after: u64,
    pub reader_join: FirmwareNormalReaderJoinObservation,
    pub output_handle_destroyed: bool,
    pub reader_fault: Option<ServiceError>,
    pub overflow: bool,
    pub retained_raw: Vec<FirmwareNormalRetirementRawObservation>,
    pub missing: Vec<FirmwareNormalRetirementGap>,
    pub first_failure: Option<FirmwareNormalRetirementRawObservation>,
}
impl FirmwareNormalRetirementObservation {
    pub fn retained_json(&self) -> Result<Vec<u8>, ServiceError> {
        self.binding.validate()?;
        bounded_source(&self.source_instance)?;
        bounded_source(&self.clock_origin)?;
        if self.owner_generation == 0
            || self.retained_raw.len() > FIRMWARE_NORMAL_RETIREMENT_RAW_LIMIT
            || self.missing.len() > FIRMWARE_NORMAL_RETIREMENT_GAP_LIMIT
            || self.processed_before > self.allocated_before
            || self.processed_after > self.allocated_after
            || self.allocated_after < self.allocated_before
            || self.processed_after < self.processed_before
            || matches!((self.close_started_at_ns, self.close_finished_at_ns), (Some(a), Some(b)) if b < a)
        {
            return Err(ServiceError::invalid(
                "Normal retirement description exceeds bounds or reverses its frontier",
            ));
        }
        for r in self.retained_raw.iter().chain(self.first_failure.iter()) {
            if r.sequence == 0
                || r.raw.len() > FIRMWARE_NORMAL_RETIREMENT_CHUNK_LIMIT
                || r.raw.len() as u64 > r.original_length
            {
                return Err(ServiceError::invalid(
                    "Retired normal original prefix exceeds bounds",
                ));
            }
        }
        for g in &self.missing {
            bounded_source(&g.source_instance)?;
            if g.first_sequence == 0
                || g.first_sequence > g.last_sequence
                || g.reason.is_empty()
                || g.reason.len() > 512
            {
                return Err(ServiceError::invalid(
                    "Retired normal missing extent exceeds bounds",
                ));
            }
        }
        if let Some(e) = &self.reader_fault
            && (e.message.is_empty()
                || e.message.len() > 512
                || e.evidence.len() > 8
                || serde_json::to_vec(e)
                    .map_err(|e| ServiceError::invalid(e.to_string()))?
                    .len()
                    > 1024)
        {
            return Err(ServiceError::invalid(
                "Retired normal reader fault exceeds bounds",
            ));
        }
        let bytes = serde_json::to_vec(self).map_err(|e| ServiceError::invalid(e.to_string()))?;
        if bytes.len() > FIRMWARE_NORMAL_RETIREMENT_ENCODED_LIMIT {
            return Err(ServiceError::invalid(
                "Encoded normal retirement exceeds bound",
            ));
        }
        Ok(bytes)
    }
}
/// A source's final descriptive frontier. Separate source namespaces are never
/// compared as if their callback ordinals shared an allocator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareLifecycleSourceRetirement {
    pub source_instance: String,
    pub clock_origin: String,
    pub epoch: SessionEpoch,
    pub generation: u64,
    pub allocated: u64,
    pub processed: u64,
    pub close_started_at_ns: u64,
    pub close_finished_at_ns: u64,
    pub reader_join: FirmwareNormalReaderJoinObservation,
    pub fault_or_missing: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirmwareLifecycleTransitionKind {
    Reenumerated,
    SameEnumerationProtocolChange,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareLifecycleTransitionObservation {
    pub kind: FirmwareLifecycleTransitionKind,
    pub previous_enumeration_generation: u64,
    pub enumeration_generation: u64,
    pub old_source_disappeared: bool,
    pub old_and_new_coexisted: bool,
    pub candidate_count: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwarePostNormalRefreshObservation {
    pub epoch: SessionEpoch,
    pub source_instance: String,
    pub clock_origin: String,
    pub allocated: u64,
    pub processed: u64,
    pub identity_sha256: String,
    pub current_sha256: String,
    pub globals_sha256: String,
    pub stored_sha256: String,
    pub current_revision: u64,
    pub globals_revision: u64,
    pub stored_revision: u64,
    pub stored_slot: u16,
    /// None means the application version was not actually observed. A boot
    /// version, USB bcdDevice, or manual label cannot populate this field.
    pub application_version: Option<String>,
    pub assessment: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareLifecycleEpochObservation {
    pub role: FirmwareLifecycleEpochRole,
    pub epoch: SessionEpoch,
    pub generation: u64,
    pub binding: FirmwareUsbObservation,
    pub previous_binding_sha256: Option<String>,
    pub previous_epoch: Option<SessionEpoch>,
    pub source_instance: String,
    pub clock_origin: String,
    pub retired_source: Option<FirmwareLifecycleSourceRetirement>,
    pub raw_identity: Vec<u8>,
    pub negotiation: Vec<u8>,
    pub transition: Option<FirmwareLifecycleTransitionObservation>,
    pub post_normal_refresh: Option<FirmwarePostNormalRefreshObservation>,
}
fn bounded_source(value: &str) -> Result<(), ServiceError> {
    if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
        Err(ServiceError::invalid(
            "Firmware source namespace is missing or exceeds bounds",
        ))
    } else {
        Ok(())
    }
}
fn checked_digest(value: &str) -> Result<(), ServiceError> {
    if JobSha256::parse(value)?.hex() != value {
        return Err(ServiceError::invalid(
            "Firmware source digest is not canonical",
        ));
    }
    Ok(())
}
impl FirmwareLifecycleEpochObservation {
    /// Validate descriptive bounds only. No serialized field creates a live
    /// continuation, a joined-reader seal, or physical authority.
    pub fn retained_json(&self) -> Result<Vec<u8>, ServiceError> {
        self.binding.validate()?;
        bounded_source(&self.source_instance)?;
        bounded_source(&self.clock_origin)?;
        if self.generation == 0
            || self.raw_identity.len() > FIRMWARE_LIFECYCLE_IDENTITY_LIMIT
            || self.negotiation.len() > FIRMWARE_LIFECYCLE_IDENTITY_LIMIT
        {
            return Err(ServiceError::invalid(
                "Firmware epoch identity or generation exceeds bounds",
            ));
        }
        if let Some(d) = &self.previous_binding_sha256 {
            checked_digest(d)?;
        }
        if let Some(r) = &self.retired_source {
            bounded_source(&r.source_instance)?;
            bounded_source(&r.clock_origin)?;
            if r.generation == 0
                || r.processed > r.allocated
                || r.close_finished_at_ns < r.close_started_at_ns
            {
                return Err(ServiceError::invalid(
                    "Firmware retired-source frontier differs",
                ));
            }
        }
        if let Some(t) = &self.transition
            && (t.previous_enumeration_generation == 0
                || t.enumeration_generation == 0
                || t.candidate_count != 1
                || t.old_and_new_coexisted
                || match t.kind {
                    FirmwareLifecycleTransitionKind::Reenumerated => {
                        !t.old_source_disappeared
                            || t.enumeration_generation <= t.previous_enumeration_generation
                    }
                    FirmwareLifecycleTransitionKind::SameEnumerationProtocolChange => {
                        t.enumeration_generation != t.previous_enumeration_generation
                    }
                })
        {
            return Err(ServiceError::invalid(
                "Firmware transition observation is ambiguous or stale",
            ));
        }
        if let Some(r) = &self.post_normal_refresh {
            bounded_source(&r.source_instance)?;
            bounded_source(&r.clock_origin)?;
            for d in [
                &r.identity_sha256,
                &r.current_sha256,
                &r.globals_sha256,
                &r.stored_sha256,
            ] {
                checked_digest(d)?;
            }
            if r.epoch != self.epoch
                || r.source_instance != self.source_instance
                || r.clock_origin != self.clock_origin
                || r.allocated != r.processed
                || r.current_revision == 0
                || r.globals_revision == 0
                || r.stored_revision == 0
                || r.stored_slot != 0
                || r.assessment.is_empty()
                || r.assessment.len() > 512
                || r.application_version.as_ref().is_some_and(|v| {
                    v.is_empty() || v.len() > 128 || v.chars().any(char::is_control)
                })
            {
                return Err(ServiceError::invalid(
                    "Firmware post-normal refresh is incomplete or differs",
                ));
            }
        }
        let bytes = serde_json::to_vec(self).map_err(|e| ServiceError::invalid(e.to_string()))?;
        // Leave space for the real record identity/hash envelope. The ledger
        // also checks the entire encoded payload against the exact 8 KiB cap.
        if bytes.len() > FIRMWARE_LIFECYCLE_EPOCH_PAYLOAD_LIMIT - 512 {
            return Err(ServiceError::invalid(
                "Encoded firmware epoch exceeds frame bound",
            ));
        }
        Ok(bytes)
    }
}

/// Immutable, descriptive entry study. Importing this record grants no I/O.
/// Its digest binds the exact two raw candidates, their order and the original
/// job review/binding. It supplies no updater receiver or recovery authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareEntryObservationPlanDto {
    pub plan: FirmwareLifecyclePlanDto,
    pub candidates: [Vec<u8>; 2],
    pub study_sha256: String,
}
impl FirmwareEntryObservationPlanDto {
    pub fn digest_for(plan: &FirmwareLifecyclePlanDto, candidates: &[Vec<u8>; 2]) -> String {
        JobSha256::of(&serde_json::to_vec(&(
            "normal-entry-observation-v1:single-write;host-evidence-only;no-transfer;quarantine",
            plan,
            candidates,
        )).expect("entry description serializes")).hex()
    }
    pub fn validate(&self, job: &ReviewedFirmwareUpdate) -> Result<(), ServiceError> {
        self.plan.validate(job)?;
        if self.plan.nonblock_actions != 2
            || self.study_sha256 != Self::digest_for(&self.plan, &self.candidates)
            || self
                .candidates
                .iter()
                .any(|wire| wire.is_empty() || wire.len() > 518)
        {
            return Err(ServiceError::invalid(
                "Entry study order/bytes/scope differs",
            ));
        }
        Ok(())
    }
}
/// A settled host offer is not an ACK, entry success, transfer cursor or clearance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareEntryHostSettlement {
    pub action: u64,
    pub wire_sha256: String,
    pub study_sha256: String,
}

/// Bounded observation projection; physical claims never advance verified bytes.
#[derive(Debug, Clone)]
pub struct FirmwareLifecycleProjection {
    plan: FirmwareLifecyclePlanDto,
    budget: FirmwareLifecycleBudget,
    next: u64,
    pending: Option<FirmwareLifecycleActionDescription>,
    send: bool,
    last_raw: Option<u64>,
    last_ns: u64,
    offered: bool,
    stopped: bool,
    simulated: u64,
    closure: Option<FirmwareLifecycleHostClosure>,
    epochs: Vec<FirmwareLifecycleEpochObservation>,
    entry: Option<FirmwareEntryObservationPlanDto>,
}
impl FirmwareLifecycleProjection {
    pub fn new(
        job: &ReviewedFirmwareUpdate,
        plan: FirmwareLifecyclePlanDto,
    ) -> Result<Self, ServiceError> {
        let budget = plan.validate(job)?;
        Ok(Self {
            plan,
            budget,
            next: 0,
            pending: None,
            send: false,
            last_raw: None,
            last_ns: 0,
            offered: false,
            stopped: false,
            simulated: 0,
            closure: None,
            epochs: Vec::new(),
            entry: None,
        })
    }
    pub fn new_entry(
        job: &ReviewedFirmwareUpdate,
        entry: FirmwareEntryObservationPlanDto,
    ) -> Result<Self, ServiceError> {
        entry.validate(job)?;
        let mut projection = Self::new(job, entry.plan.clone())?;
        projection.budget = FirmwareLifecycleBudget::checked(0, 2)?;
        projection.entry = Some(entry);
        Ok(projection)
    }
    pub fn entry_observation(&self) -> Option<&FirmwareEntryObservationPlanDto> {
        self.entry.as_ref()
    }
    pub fn settle_entry_host(
        &mut self,
        settled: &FirmwareEntryHostSettlement,
    ) -> Result<(), ServiceError> {
        let entry = self
            .entry
            .as_ref()
            .ok_or_else(|| ServiceError::invalid("Transfer has no entry host settlement"))?;
        let action = self
            .pending
            .as_ref()
            .ok_or_else(|| ServiceError::invalid("Entry host settlement lacks action"))?;
        if self.stopped
            || self.closure.is_some()
            || !self.send
            || settled.action != action.ordinal
            || settled.wire_sha256 != action.wire_sha256
            || settled.study_sha256 != entry.study_sha256
        {
            return Err(ServiceError::invalid(
                "Entry host offer is incomplete or differs",
            ));
        }
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| ServiceError::invalid("Entry ordinal exhausted"))?;
        self.pending = None;
        self.send = false;
        Ok(())
    }
    pub fn epochs(&self) -> &[FirmwareLifecycleEpochObservation] {
        &self.epochs
    }
    pub fn active_epoch(&self) -> SessionEpoch {
        self.epochs.last().map_or(self.plan.epoch, |e| e.epoch)
    }
    pub fn active_generation(&self) -> u64 {
        self.epochs
            .last()
            .map_or(self.plan.generation, |e| e.generation)
    }
    pub fn active_binding(&self) -> &FirmwareUsbObservation {
        self.epochs
            .last()
            .map_or(&self.plan.binding, |e| &e.binding)
    }
    pub fn active_clock_origin(&self) -> &str {
        self.epochs
            .last()
            .map_or(&self.plan.clock_origin, |e| &e.clock_origin)
    }
    pub fn observe_epoch(
        &mut self,
        e: FirmwareLifecycleEpochObservation,
    ) -> Result<(), ServiceError> {
        e.retained_json()?;
        let bad = || {
            ServiceError::invalid("Firmware epoch order/source/frontier is unresolved or differs")
        };
        if self.stopped
            || self.closure.is_some()
            || self.pending.is_some()
            || self.send
            || self.epochs.len() >= FIRMWARE_LIFECYCLE_EPOCH_LIMIT
        {
            return Err(bad());
        }
        if self.entry.is_some() && !self.epochs.is_empty() {
            return Err(bad());
        }
        if let Some(previous) = self.epochs.last() {
            let expected_role = match previous.role {
                FirmwareLifecycleEpochRole::NormalEntry => FirmwareLifecycleEpochRole::Bootloader,
                FirmwareLifecycleEpochRole::Bootloader => {
                    FirmwareLifecycleEpochRole::PostUpdateNormal
                }
                FirmwareLifecycleEpochRole::PostUpdateNormal => return Err(bad()),
            };
            let r = e.retired_source.as_ref().ok_or_else(bad)?;
            if e.role != expected_role
                || e.previous_epoch != Some(previous.epoch)
                || e.previous_binding_sha256.as_ref() != Some(&previous.binding.digest())
                || previous.epoch.0.checked_add(1) != Some(e.epoch.0)
                || previous.generation.checked_add(1) != Some(e.generation)
                || previous.source_instance == e.source_instance
                || previous.transition.as_ref().is_some_and(|t| {
                    e.transition.as_ref().is_none_or(|next| {
                        next.previous_enumeration_generation != t.enumeration_generation
                    })
                })
                || e.transition.is_none()
                || e.raw_identity.is_empty()
                || e.negotiation.is_empty()
                || r.source_instance != previous.source_instance
                || r.clock_origin != previous.clock_origin
                || r.epoch != previous.epoch
                || r.generation != previous.generation
                || r.reader_join != FirmwareNormalReaderJoinObservation::Returned
                || r.fault_or_missing
                || r.allocated != r.processed
                || r.processed != self.last_raw.unwrap_or(0)
                || r.close_started_at_ns < self.last_ns
                || (e.role == FirmwareLifecycleEpochRole::Bootloader
                    && (e.clock_origin != previous.clock_origin || e.post_normal_refresh.is_some()))
                || (e.role == FirmwareLifecycleEpochRole::PostUpdateNormal
                    && (self.next != self.budget.actions || e.post_normal_refresh.is_none()))
            {
                return Err(bad());
            }
            // The updater clock and global allocator continue across entry and
            // boot source replacement. Fresh normal reads have a separately
            // named clock/allocator recorded in the refresh description.
            if e.role == FirmwareLifecycleEpochRole::Bootloader {
                self.last_ns = r.close_finished_at_ns;
            }
        } else if e.role != FirmwareLifecycleEpochRole::NormalEntry
            || self.next != 0
            || self.offered
            || self.last_raw.is_some()
            || e.epoch != self.plan.epoch
            || e.generation != self.plan.generation
            || e.binding != self.plan.binding
            || e.clock_origin != self.plan.clock_origin
            || e.previous_epoch.is_some()
            || e.previous_binding_sha256.is_some()
            || e.retired_source.is_some()
            || e.transition.is_some()
            || e.post_normal_refresh.is_some()
        {
            return Err(bad());
        }
        self.epochs.push(e);
        Ok(())
    }
    pub fn plan(&self) -> &FirmwareLifecyclePlanDto {
        &self.plan
    }
    pub fn budget(&self) -> &FirmwareLifecycleBudget {
        &self.budget
    }
    pub fn pending(&self) -> Option<&FirmwareLifecycleActionDescription> {
        self.pending.as_ref()
    }
    pub fn next_action(&self) -> u64 {
        self.next
    }
    pub fn simulated_source_bytes(&self) -> u64 {
        self.simulated
    }
    pub fn device_verified_bytes(&self) -> u64 {
        0
    }
    pub fn completion_verified(&self) -> bool {
        false
    }
    pub fn resume_supported(&self) -> bool {
        false
    }
    pub fn stopped(&self) -> bool {
        self.stopped
    }
    pub fn closure(&self) -> Option<FirmwareLifecycleHostClosure> {
        self.closure
    }
    pub fn checkpoint(
        &mut self,
        job: &ReviewedFirmwareUpdate,
        a: FirmwareLifecycleActionDescription,
    ) -> Result<(), ServiceError> {
        a.validate(job)?;
        if let Some(entry) = &self.entry {
            let candidate = usize::try_from(a.ordinal)
                .ok()
                .and_then(|i| entry.candidates.get(i));
            if candidate != Some(&a.wire)
                || a.kind != FirmwareLifecycleKind::EntryTransition
                || a.block.is_some()
                || a.rule_sha256 != entry.study_sha256
            {
                return Err(ServiceError::invalid(
                    "Entry action differs from immutable ordered study",
                ));
            }
        }
        if self.stopped
            || self.closure.is_some()
            || self.pending.is_some()
            || a.ordinal != self.next
            || a.ordinal >= self.budget.actions
            || self
                .epochs
                .last()
                .is_some_and(|e| e.role == FirmwareLifecycleEpochRole::PostUpdateNormal)
            || a.epoch != self.active_epoch()
            || a.generation != self.active_generation()
            || a.binding_sha256 != self.active_binding().digest()
        {
            return Err(ServiceError::invalid(
                "Firmware lifecycle action is stale or unresolved",
            ));
        }
        self.pending = Some(a);
        self.send = false;
        Ok(())
    }
    pub fn observe_send(
        &mut self,
        s: &FirmwareLifecycleSendObservation,
    ) -> Result<(), ServiceError> {
        let a = self
            .pending
            .as_ref()
            .ok_or_else(|| ServiceError::invalid("Firmware send has no checkpoint"))?;
        if self.stopped
            || self.closure.is_some()
            || self.send
            || s.action != a.ordinal
            || s.wire_sha256 != a.wire_sha256
            || s.attempted != a.wire.len() as u64
            || s.finished_sequence.is_some_and(|v| v < s.started_sequence)
            || s.finished_at_ns.is_some_and(|v| v < s.started_at_ns)
        {
            return Err(ServiceError::invalid("Firmware send observation differs"));
        }
        self.send = true;
        self.offered = true;
        if s.extent != FirmwareOsExtent::Known(s.attempted)
            || s.finished_sequence.is_none()
            || s.finished_at_ns.is_none()
            || s.drain != FirmwareDrainObservation::Returned
        {
            self.stopped = true;
        }
        Ok(())
    }
    pub fn raw(&mut self, r: &FirmwareLifecycleRawObservation) -> Result<(), ServiceError> {
        if self.closure.is_some()
            || r.raw.is_empty()
            || r.raw.len() > FIRMWARE_RAW_CHUNK_LIMIT
            || r.sequence != self.last_raw.map_or(1, |v| v.saturating_add(1))
            || self.last_raw == Some(u64::MAX)
            || r.at_ns < self.last_ns
            || self
                .epochs
                .last()
                .is_some_and(|e| e.role == FirmwareLifecycleEpochRole::PostUpdateNormal)
            || r.epoch != self.active_epoch()
            || r.generation != self.active_generation()
            || r.binding_sha256 != self.active_binding().digest()
            || r.clock_origin != self.active_clock_origin()
        {
            return Err(ServiceError::invalid(
                "Firmware raw source/order/frontier differs",
            ));
        }
        self.last_raw = Some(r.sequence);
        self.last_ns = r.at_ns;
        Ok(())
    }
    pub fn decide(&mut self, d: &FirmwareLifecycleDecision) -> Result<(), ServiceError> {
        let a = self
            .pending
            .as_ref()
            .ok_or_else(|| ServiceError::invalid("Firmware decision lacks action"))?;
        if self.entry.is_some()
            || self.stopped
            || !self.send
            || self.plan.origin != FirmwareObservationOrigin::MockSimulation
            || !d.mock_matched
            || d.action != a.ordinal
            || d.rule_sha256 != a.rule_sha256
            || d.first_raw == 0
            || d.first_raw > d.last_raw
            || self.last_raw != Some(d.last_raw)
            || d.observed_frontier != d.last_raw
        {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Firmware decision has no accepted private physical authority",
            ));
        }
        self.simulated += a.block.as_ref().map_or(0, |b| u64::from(b.content_bytes));
        self.next += 1;
        self.pending = None;
        self.send = false;
        Ok(())
    }
    pub fn close(&mut self, c: FirmwareLifecycleHostClosure) -> Result<(), ServiceError> {
        if self.closure.is_some()
            || (c == FirmwareLifecycleHostClosure::ClosedBeforeAnyOffer && self.offered)
            || (c == FirmwareLifecycleHostClosure::MockFinished
                && (self.entry.is_some()
                    || self.plan.origin != FirmwareObservationOrigin::MockSimulation
                    || self.stopped
                    || self.pending.is_some()
                    || self.next != self.budget.actions
                    || (!self.epochs.is_empty()
                        && self.epochs.last().is_none_or(|e| {
                            e.role != FirmwareLifecycleEpochRole::PostUpdateNormal
                                || e.post_normal_refresh
                                    .as_ref()
                                    .is_none_or(|r| r.application_version.is_none())
                        }))))
        {
            return Err(ServiceError::invalid(
                "Firmware lifecycle closure is not established",
            ));
        }
        self.closure = Some(c);
        self.stopped = true;
        Ok(())
    }
    pub fn stop(&mut self) {
        self.stopped = true;
    }
}
/// Exact original chunk retained with an explicit failed interpretation. It is
/// never an ACK or a usable binding, including when source/order is invalid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareLifecycleNegativeObservation {
    pub raw: FirmwareLifecycleRawObservation,
    pub note: String,
}
impl FirmwareLifecycleNegativeObservation {
    pub fn retained_json(&self) -> Result<Vec<u8>, ServiceError> {
        if self.raw.raw.len() > 1024 || self.note.is_empty() || self.note.len() > 512 {
            return Err(ServiceError::invalid(
                "Firmware negative observation exceeds bound",
            ));
        }
        let bytes = serde_json::to_vec(self).map_err(|e| ServiceError::invalid(e.to_string()))?;
        if bytes.len() > 8 * 1024 - 256 {
            return Err(ServiceError::invalid(
                "Encoded negative observation exceeds frame bound",
            ));
        }
        Ok(bytes)
    }
}
