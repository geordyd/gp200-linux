//! Bounded IR observations, never executable jobs or proof of device contents.
//! Imported pins describe claims; they do not establish files or a live session.
use crate::*;
use gp200_protocol::{WRITE_KIND, asset_upload_candidate, encode_transfer};
use serde::{Deserialize, Serialize};

const MAX_EVENTS: usize = 256;
const MAX_STATUS_BYTES: usize = 4096;
const MAX_JSON_BYTES: usize = 2 * 1024 * 1024;

/// A descriptive identifier. The persistent ledger must enforce uniqueness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct IrAttemptId(String);
impl IrAttemptId {
    pub fn parse(value: &str) -> Result<Self, ServiceError> {
        if value.len() != 32
            || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
            || value.bytes().all(|byte| byte == b'0')
        {
            return Err(ServiceError::invalid(
                "IR attempt identifier needs 32 nonzero hexadecimal digits",
            ));
        }
        Ok(Self(value.to_ascii_lowercase()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for IrAttemptId {
    fn deserialize<D: serde::Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(decoder)?).map_err(serde::de::Error::custom)
    }
}

/// Persisted descriptive claims. No session authority is reconstructed here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrSessionBindingDto {
    pub epoch: SessionEpoch,
    pub selection: DeviceSelection,
    pub identity_revision: Revision,
    pub identity_sha256: String,
    pub session_sha256: String,
}
impl IrSessionBindingDto {
    fn from_session(session: &DeviceJobSession) -> Self {
        Self {
            epoch: session.epoch(),
            selection: session.selection().clone(),
            identity_revision: session.identity_revision(),
            identity_sha256: session.identity_sha256().hex(),
            session_sha256: JobSha256::of(
                &serde_json::to_vec(session).expect("session serializes"),
            )
            .hex(),
        }
    }
    fn check(&mut self) -> Result<(), ServiceError> {
        if self.identity_revision.0 == 0
            || [&self.selection.input_id, &self.selection.output_id]
                .iter()
                .any(|value| value.trim().is_empty() || value.len() > 4096)
            || serde_json::to_vec(&self.selection).map_or(true, |bytes| bytes.len() > 16 * 1024)
        {
            return Err(ServiceError::invalid(
                "IR observation session needs bounded endpoints and an identity revision",
            ));
        }
        self.identity_sha256 = JobSha256::parse(&self.identity_sha256)?.hex();
        self.session_sha256 = JobSha256::parse(&self.session_sha256)?.hex();
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrAttemptBindingDto {
    pub attempt: IrAttemptId,
    pub job: JobId,
    pub session: IrSessionBindingDto,
    pub target: AssetSlot,
    pub review_sha256: String,
    pub baseline_sha256: String,
    pub baseline_revision: Revision,
    pub source_sha256: String,
    pub coefficients_sha256: String,
    pub payload_sha256: String,
    pub wire_sha256: String,
    pub codec_revision: String,
    pub length_policy: IrAttemptLengthPolicy,
    pub name_raw: [u8; 16],
    pub additive_checksum: u32,
    pub payload_bytes: u64,
    pub wire_bytes: u64,
    pub packet_count: u16,
}
/// Persisted recipe description; importing this does not prepare a conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IrAttemptLengthPolicy {
    RejectLong,
    KeepFirst1024,
}

/// Private checked shape. Importing it verifies shape, not source/session truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrAttemptBinding(IrAttemptBindingDto);
impl IrAttemptBinding {
    pub fn from_prepared(
        attempt: IrAttemptId,
        job: &PreparedDeviceJob,
    ) -> Result<Self, ServiceError> {
        let recipe = job.recipe();
        let length_policy = recipe
            .ir_length_policy()
            .ok_or_else(|| ServiceError::invalid("IR observations require an IR preparation"))?;
        let transfer = asset_upload_candidate(job.target(), recipe.name(), recipe.derived())
            .map_err(|error| ServiceError::invalid(error.to_string()))?;
        let packets =
            encode_transfer(&transfer).map_err(|error| ServiceError::invalid(error.to_string()))?;
        let wire: Vec<u8> = packets.iter().flatten().copied().collect();
        let body = &transfer.records()[0].body;
        Self::from_dto(IrAttemptBindingDto {
            attempt,
            job: job.id(),
            session: IrSessionBindingDto::from_session(job.session()),
            target: job.target(),
            review_sha256: job.review_sha256().hex(),
            baseline_sha256: JobSha256::of(job.baseline().raw().payload()).hex(),
            baseline_revision: job.baseline().revision(),
            source_sha256: recipe.input_sha256().hex(),
            coefficients_sha256: recipe.derived_sha256().hex(),
            payload_sha256: JobSha256::of(transfer.payload()).hex(),
            wire_sha256: JobSha256::of(&wire).hex(),
            codec_revision: recipe.codec_revision().into(),
            length_policy: match length_policy {
                IrJobLengthPolicy::RejectLong => IrAttemptLengthPolicy::RejectLong,
                IrJobLengthPolicy::KeepFirst1024 => IrAttemptLengthPolicy::KeepFirst1024,
            },
            name_raw: body[8..24].try_into().expect("typed IR name"),
            additive_checksum: u32::from_le_bytes(body[4..8].try_into().expect("typed IR sum")),
            payload_bytes: transfer.payload().len() as u64,
            wire_bytes: wire.len() as u64,
            packet_count: packets.len() as u16,
        })
    }
    pub fn from_dto(mut dto: IrAttemptBindingDto) -> Result<Self, ServiceError> {
        dto.session.check()?;
        if dto.target.bank() != AssetBank::Ir
            || dto.baseline_revision.0 == 0
            || dto.codec_revision != "gp200-ir-44100-pcm24-v1"
            || dto.payload_bytes != 4124
            || dto.wire_bytes != 8570
            || dto.packet_count != 23
            || dto.additive_checksum > 4096 * 255
        {
            return Err(ServiceError::invalid(
                "IR attempt pins do not match the bounded current IR recipe",
            ));
        }
        // The checked recipe uses this exact pure codec. Reuse its constraints
        // and require canonical padding instead of inferring a separate rule.
        let end = dto
            .name_raw
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(16);
        let name = std::str::from_utf8(&dto.name_raw[..end])
            .map_err(|_| ServiceError::invalid("IR intended name is not ASCII"))?;
        let name_check = asset_upload_candidate(dto.target, name, &[0; 4096])
            .map_err(|error| ServiceError::invalid(error.to_string()))?;
        if name_check.records()[0].body[8..24] != dto.name_raw {
            return Err(ServiceError::invalid(
                "IR intended name is not a checked zero-padded ASCII field",
            ));
        }
        for digest in [
            &mut dto.review_sha256,
            &mut dto.baseline_sha256,
            &mut dto.source_sha256,
            &mut dto.coefficients_sha256,
            &mut dto.payload_sha256,
            &mut dto.wire_sha256,
        ] {
            *digest = JobSha256::parse(digest)?.hex();
        }
        Ok(Self(dto))
    }
    pub fn pins(&self) -> &IrAttemptBindingDto {
        &self.0
    }
    pub fn to_dto(&self) -> IrAttemptBindingDto {
        self.0.clone()
    }
    pub fn context(&self) -> IrObservationContext {
        IrObservationContext(IrObservationContextDto {
            attempt: self.0.attempt.clone(),
            session: self.0.session.clone(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrObservationContextDto {
    pub attempt: IrAttemptId,
    pub session: IrSessionBindingDto,
}
/// Local attribution only: the recovered status has no transaction identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrObservationContext(IrObservationContextDto);
impl IrObservationContext {
    pub fn new(attempt: IrAttemptId, session: &DeviceJobSession) -> Result<Self, ServiceError> {
        Self::from_dto(IrObservationContextDto {
            attempt,
            session: IrSessionBindingDto::from_session(session),
        })
    }
    pub fn from_dto(mut dto: IrObservationContextDto) -> Result<Self, ServiceError> {
        dto.session.check()?;
        Ok(Self(dto))
    }
    pub fn to_dto(&self) -> IrObservationContextDto {
        self.0.clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IrTransportExtent {
    OfferedToProcess,
    TransportSubmitted,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IrSendCompletion {
    Process(IrProcessObservation),
    TransportReturned,
    TransportFailed,
    InProgress,
}
impl IrSendCompletion {
    fn completed(self, extent: IrTransportExtent) -> bool {
        match self {
            Self::Process(process) => process.completed(),
            Self::TransportReturned => extent == IrTransportExtent::TransportSubmitted,
            _ => false,
        }
    }
    fn failed(self) -> bool {
        matches!(self, Self::TransportFailed)
            || matches!(self, Self::Process(process) if process.timed_out || process.exit_code.is_some_and(|code| code != 0))
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrProcessObservation {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub reaped: bool,
}
impl IrProcessObservation {
    fn completed(self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && self.reaped
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrObservationRelation {
    pub attempt_matches: bool,
    pub session_matches: bool,
    pub slot_matches: bool,
    pub at_or_after_start: bool,
}
impl IrObservationRelation {
    pub fn matches(self) -> bool {
        self.attempt_matches && self.session_matches && self.slot_matches && self.at_or_after_start
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrStatusFields {
    pub slot_raw: u16,
    pub flags_raw: u16,
    pub value_raw: u32,
}
impl IrStatusFields {
    /// Static raw equality only; no native adapter/runtime or acceptance claim.
    pub fn raw_flags_equal_one(self) -> bool {
        self.flags_raw == 1
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrStatusShapeIssue {
    WrongKind,
    ExpectedSingleRecord,
    WrongCommand,
    InvalidBodyLength,
    SlotOutOfRange,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrMetadataComparison {
    pub full_name_matches: bool,
    pub display_name_bytes_match: bool,
    pub additive_checksum_matches: bool,
}
impl IrMetadataComparison {
    pub fn all_match(self) -> bool {
        self.full_name_matches && self.display_name_bytes_match && self.additive_checksum_matches
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrMetadataObservationDto {
    pub slot: AssetSlot,
    pub stamp: RequestStamp,
    pub revision: Revision,
    pub source: SnapshotSource,
    pub captured_at_ms: u64,
    pub raw: RawTransfer,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IrAttemptEventDto {
    Send {
        context: IrObservationContextDto,
        at_ms: u64,
        extent: IrTransportExtent,
        wire_bytes: u64,
        completion: IrSendCompletion,
    },
    Status {
        context: IrObservationContextDto,
        at_ms: u64,
        raw: RawTransfer,
    },
    Metadata {
        context: IrObservationContextDto,
        at_ms: u64,
        metadata: IrMetadataObservationDto,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IrAttemptFinishReason {
    ObservationComplete,
    Timeout,
    Disconnected,
    ProcessFailed,
    ObservationFailed,
    InterruptedByRestart,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrAttemptFinalState {
    ContentUnverified,
    Indeterminate,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrIndeterminateReason {
    Interrupted,
    SendMissing,
    SendPartialOrFailed,
    MatchingStatusMissing,
    FreshTargetMetadataMissing,
    MetadataMismatch,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrAttemptSummary {
    pub final_state: IrAttemptFinalState,
    pub reasons: Vec<IrIndeterminateReason>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrAttemptEvidenceDto {
    pub schema: u16,
    pub binding: IrAttemptBindingDto,
    pub started_at_ms: u64,
    pub events: Vec<IrAttemptEventDto>,
    pub finish: Option<IrAttemptFinishReason>,
}

#[derive(Debug, Clone)]
pub struct IrStatusObservation {
    raw: RawTransfer,
    fields: Option<IrStatusFields>,
    shape_issue: Option<IrStatusShapeIssue>,
    relation: IrObservationRelation,
    after_accounted_send: bool,
    duplicate_of: Option<usize>,
}
impl IrStatusObservation {
    pub fn raw(&self) -> &RawTransfer {
        &self.raw
    }
    pub fn fields(&self) -> Option<IrStatusFields> {
        self.fields
    }
    pub fn shape_issue(&self) -> Option<IrStatusShapeIssue> {
        self.shape_issue
    }
    pub fn relation(&self) -> IrObservationRelation {
        self.relation
    }
    pub fn after_accounted_send(&self) -> bool {
        self.after_accounted_send
    }
    pub fn duplicate_of(&self) -> Option<usize> {
        self.duplicate_of
    }
}
#[derive(Debug, Clone)]
pub struct IrMetadataObservation {
    snapshot: DeviceAssetMetadata,
    relation: IrObservationRelation,
    fresh: bool,
    comparison: IrMetadataComparison,
}
impl IrMetadataObservation {
    pub fn snapshot(&self) -> &DeviceAssetMetadata {
        &self.snapshot
    }
    pub fn relation(&self) -> IrObservationRelation {
        self.relation
    }
    pub fn fresh(&self) -> bool {
        self.fresh
    }
    pub fn comparison(&self) -> IrMetadataComparison {
        self.comparison
    }
}

/// Closed imports cannot be resumed. This collection contains no send command.
#[derive(Debug)]
pub struct IrAttemptEvidence {
    binding: IrAttemptBinding,
    started_at_ms: u64,
    events: Vec<IrAttemptEventDto>,
    statuses: Vec<IrStatusObservation>,
    metadata: Vec<IrMetadataObservation>,
    finish: Option<IrAttemptFinishReason>,
}
impl IrAttemptEvidence {
    pub fn start(binding: IrAttemptBinding, started_at_ms: u64) -> Self {
        Self {
            binding,
            started_at_ms,
            events: vec![],
            statuses: vec![],
            metadata: vec![],
            finish: None,
        }
    }
    pub fn binding(&self) -> &IrAttemptBinding {
        &self.binding
    }
    pub fn statuses(&self) -> &[IrStatusObservation] {
        &self.statuses
    }
    pub fn metadata(&self) -> &[IrMetadataObservation] {
        &self.metadata
    }
    pub fn events(&self) -> &[IrAttemptEventDto] {
        &self.events
    }
    pub fn finish_reason(&self) -> Option<IrAttemptFinishReason> {
        self.finish
    }
    fn can_append(&self) -> Result<(), ServiceError> {
        if self.finish.is_some() {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Finished IR observations cannot resume",
            ));
        }
        if self.events.len() >= MAX_EVENTS {
            return Err(ServiceError::invalid("IR observations exceed 256 events"));
        }
        Ok(())
    }
    fn relation(
        &self,
        context: &IrObservationContext,
        at_ms: u64,
        slot: Option<u16>,
    ) -> IrObservationRelation {
        IrObservationRelation {
            attempt_matches: context.0.attempt == self.binding.0.attempt,
            session_matches: context.0.session == self.binding.0.session,
            slot_matches: slot.is_some_and(|value| value == self.binding.0.target.index()),
            at_or_after_start: at_ms >= self.started_at_ms,
        }
    }
    fn completed_send_at(&self, at_ms: u64) -> Option<u64> {
        self.events
            .iter()
            .filter_map(|event| {
                if let IrAttemptEventDto::Send {
                    context,
                    at_ms: sent_at,
                    extent,
                    wire_bytes,
                    completion,
                } = event
                    && context.attempt == self.binding.0.attempt
                    && context.session == self.binding.0.session
                    && *sent_at >= self.started_at_ms
                    && *sent_at <= at_ms
                    && *wire_bytes == self.binding.0.wire_bytes
                    && completion.completed(*extent)
                {
                    Some(*sent_at)
                } else {
                    None
                }
            })
            .max()
    }
    pub fn observe_send(
        &mut self,
        context: IrObservationContext,
        at_ms: u64,
        extent: IrTransportExtent,
        wire_bytes: u64,
        completion: IrSendCompletion,
    ) -> Result<(), ServiceError> {
        self.can_append()?;
        if wire_bytes > self.binding.0.wire_bytes {
            return Err(ServiceError::invalid(
                "IR send accounting exceeds the intended wire bytes",
            ));
        }
        self.events.push(IrAttemptEventDto::Send {
            context: context.0,
            at_ms,
            extent,
            wire_bytes,
            completion,
        });
        Ok(())
    }
    pub fn observe_status(
        &mut self,
        context: IrObservationContext,
        at_ms: u64,
        raw: RawTransfer,
    ) -> Result<(), ServiceError> {
        self.can_append()?;
        if raw.payload().len() > MAX_STATUS_BYTES {
            return Err(ServiceError::invalid(
                "IR status observation exceeds 4096 payload bytes",
            ));
        }
        let mut fields = None;
        let shape_issue = if raw.kind() != WRITE_KIND {
            Some(IrStatusShapeIssue::WrongKind)
        } else if raw.records().len() != 1 {
            Some(IrStatusShapeIssue::ExpectedSingleRecord)
        } else {
            let record = &raw.records()[0];
            if record.command != 0x100b {
                Some(IrStatusShapeIssue::WrongCommand)
            } else if record.body.len() != 8 {
                Some(IrStatusShapeIssue::InvalidBodyLength)
            } else {
                let value = IrStatusFields {
                    slot_raw: u16::from_le_bytes(
                        record.body[..2].try_into().expect("bounded status"),
                    ),
                    flags_raw: u16::from_le_bytes(
                        record.body[2..4].try_into().expect("bounded status"),
                    ),
                    value_raw: u32::from_le_bytes(
                        record.body[4..8].try_into().expect("bounded status"),
                    ),
                };
                fields = Some(value);
                (value.slot_raw >= 20).then_some(IrStatusShapeIssue::SlotOutOfRange)
            }
        };
        let relation = self.relation(&context, at_ms, fields.map(|value| value.slot_raw));
        // Fixed at receive time. A later send cannot retroactively promote a
        // buffered/pre-send reply; imported events replay the same order.
        let after_accounted_send = self.completed_send_at(at_ms).is_some();
        let duplicate_of = self.events.iter().position(|event| matches!(event,
            IrAttemptEventDto::Status { context: old, raw: previous, .. } if *old == context.0 && *previous == raw));
        self.statuses.push(IrStatusObservation {
            raw: raw.clone(),
            fields,
            shape_issue,
            relation,
            after_accounted_send,
            duplicate_of,
        });
        self.events.push(IrAttemptEventDto::Status {
            context: context.0,
            at_ms,
            raw,
        });
        Ok(())
    }
    pub fn observe_metadata(
        &mut self,
        context: IrObservationContext,
        at_ms: u64,
        snapshot: DeviceAssetMetadata,
    ) -> Result<(), ServiceError> {
        self.can_append()?;
        if snapshot.slot().bank() != AssetBank::Ir {
            return Err(ServiceError::invalid(
                "IR evidence cannot contain SnapTone metadata",
            ));
        }
        let relation = self.relation(&context, at_ms, Some(snapshot.slot().index()));
        let fresh = relation.matches()
            && self.completed_send_at(snapshot.captured_at_ms()).is_some()
            && snapshot.stamp().epoch == self.binding.0.session.epoch
            && matches!(snapshot.source(), SnapshotSource::Hardware { epoch } if *epoch == self.binding.0.session.epoch)
            && snapshot.revision() > self.binding.0.baseline_revision
            && snapshot.captured_at_ms() >= self.started_at_ms
            && snapshot.captured_at_ms() <= at_ms;
        let actual = snapshot.metadata();
        let comparison = IrMetadataComparison {
            full_name_matches: actual.name_raw == self.binding.0.name_raw,
            display_name_bytes_match: display_bytes(&actual.name_raw)
                == display_bytes(&self.binding.0.name_raw),
            additive_checksum_matches: actual.checksum_raw == self.binding.0.additive_checksum,
        };
        let metadata = IrMetadataObservationDto {
            slot: snapshot.slot(),
            stamp: snapshot.stamp(),
            revision: snapshot.revision(),
            source: snapshot.source().clone(),
            captured_at_ms: snapshot.captured_at_ms(),
            raw: (**snapshot.raw()).clone(),
        };
        self.metadata.push(IrMetadataObservation {
            snapshot,
            relation,
            fresh,
            comparison,
        });
        self.events.push(IrAttemptEventDto::Metadata {
            context: context.0,
            at_ms,
            metadata,
        });
        Ok(())
    }
    pub fn finish(
        &mut self,
        reason: IrAttemptFinishReason,
    ) -> Result<IrAttemptSummary, ServiceError> {
        if self.finish.is_some() {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "IR attempt observation is already finished",
            ));
        }
        self.finish = Some(reason);
        Ok(self.summary().expect("just finished"))
    }
    pub fn summary(&self) -> Option<IrAttemptSummary> {
        let finish = self.finish?;
        let mut reasons = vec![];
        if finish != IrAttemptFinishReason::ObservationComplete {
            reasons.push(IrIndeterminateReason::Interrupted);
        }
        let sends: Vec<_> = self
            .events
            .iter()
            .filter_map(|event| {
                if let IrAttemptEventDto::Send {
                    context,
                    at_ms,
                    extent,
                    wire_bytes,
                    completion,
                } = event
                    && context.attempt == self.binding.0.attempt
                    && context.session == self.binding.0.session
                    && *at_ms >= self.started_at_ms
                {
                    Some((*extent, *wire_bytes, *completion))
                } else {
                    None
                }
            })
            .collect();
        if sends.is_empty() {
            reasons.push(IrIndeterminateReason::SendMissing);
        } else if !sends.iter().any(|(extent, count, completion)| {
            *count == self.binding.0.wire_bytes && completion.completed(*extent)
        }) || sends.iter().any(|(_, _, completion)| completion.failed())
            || sends.last().is_some_and(|(extent, count, completion)| {
                *count != self.binding.0.wire_bytes || !completion.completed(*extent)
            })
        {
            reasons.push(IrIndeterminateReason::SendPartialOrFailed);
        }
        if !self.statuses.iter().any(|status| {
            status.shape_issue.is_none()
                && status.relation.matches()
                && status.after_accounted_send
                && status.duplicate_of.is_none()
        }) {
            reasons.push(IrIndeterminateReason::MatchingStatusMissing);
        }
        if let Some(metadata) = self.metadata.iter().rev().find(|metadata| metadata.fresh) {
            if !metadata.comparison.all_match() {
                reasons.push(IrIndeterminateReason::MetadataMismatch);
            }
        } else {
            reasons.push(IrIndeterminateReason::FreshTargetMetadataMissing);
        }
        Some(IrAttemptSummary {
            final_state: if reasons.is_empty() {
                IrAttemptFinalState::ContentUnverified
            } else {
                IrAttemptFinalState::Indeterminate
            },
            reasons,
        })
    }
    pub fn to_dto(&self) -> IrAttemptEvidenceDto {
        IrAttemptEvidenceDto {
            schema: 1,
            binding: self.binding.to_dto(),
            started_at_ms: self.started_at_ms,
            events: self.events.clone(),
            finish: self.finish,
        }
    }
    /// Validates shape and recomputes projections; does not verify claimed pins.
    /// An unfinished import is closed as interrupted, never silently resumed.
    pub fn reconstruct(dto: IrAttemptEvidenceDto) -> Result<Self, ServiceError> {
        if dto.schema != 1 || dto.events.len() > MAX_EVENTS {
            return Err(ServiceError::invalid(
                "Unsupported or oversized IR evidence receipt",
            ));
        }
        let mut result = Self::start(IrAttemptBinding::from_dto(dto.binding)?, dto.started_at_ms);
        for event in dto.events {
            match event {
                IrAttemptEventDto::Send {
                    context,
                    at_ms,
                    extent,
                    wire_bytes,
                    completion,
                } => result.observe_send(
                    IrObservationContext::from_dto(context)?,
                    at_ms,
                    extent,
                    wire_bytes,
                    completion,
                )?,
                IrAttemptEventDto::Status {
                    context,
                    at_ms,
                    raw,
                } => result.observe_status(IrObservationContext::from_dto(context)?, at_ms, raw)?,
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
                    result.observe_metadata(
                        IrObservationContext::from_dto(context)?,
                        at_ms,
                        snapshot,
                    )?;
                }
            }
        }
        result.finish(
            dto.finish
                .unwrap_or(IrAttemptFinishReason::InterruptedByRestart),
        )?;
        Ok(result)
    }
    pub fn from_json(bytes: &[u8]) -> Result<Self, ServiceError> {
        if bytes.len() > MAX_JSON_BYTES {
            return Err(ServiceError::invalid("IR evidence JSON exceeds 2 MiB"));
        }
        let dto = serde_json::from_slice(bytes)
            .map_err(|error| ServiceError::invalid(error.to_string()))?;
        Self::reconstruct(dto)
    }
}
fn display_bytes(name: &[u8; 16]) -> &[u8] {
    &name[..name.iter().position(|byte| *byte == 0).unwrap_or(16)]
}
