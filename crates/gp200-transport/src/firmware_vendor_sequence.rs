//! Pure offline model of the recovered vendor firmware sequence.
//!
//! This module owns no handles, worker, journal, permission or physical permit.
//! Its public constructor accepts only the separately pinned GP-200 V1.8.2 image.
//! Complete framed receives and host receipts are caller observations, not proof
//! of device origin. A future driver must preserve original chunks and durably
//! record each returned action receipt before making another physical offer.
//!
//! Statuses have no transaction/section/block key. A delayed old status arriving
//! during a later offer is indistinguishable from its legitimate response. Local
//! generation/frontier checks cannot solve that wire limitation, and identical
//! statuses across successive blocks are deliberately allowed.
use gp200_domain::{DeviceModel, FirmwareReview, JobSha256};
use gp200_protocol::{
    BootloaderIdentity, FirmwareDescriptor, FirmwareFixedPacket, FirmwareSectionSelector,
    firmware_block_candidate,
};

/// Public corrected-order runtime contract required by fresh reviews; not an approval.
pub const VENDOR_CONTRACT_SHA256: &str =
    "eb5885f1b3290edebd9efbac89ceae09993117b0dc7031c20a6217aa70bc7823";
/// Prior corrected-order contract; accepted only when inspecting original local history.
pub const HISTORICAL_CORRECTED_VENDOR_CONTRACT_SHA256: &str =
    "b4ac1998426e8510d469beb57c309a3c1201bb60b7604df9696239f50cd63ac8";
/// Immutable predecessor contract; accepted only when inspecting original local history.
pub const LEGACY_VENDOR_CONTRACT_SHA256: &str =
    "b1997f6e0028083e5dbc4530a15a1dd789563328d85c61edb2d1bac8fdfc7d40";
pub const VENDOR_IMAGE_SHA256: &str =
    "c368618773097b07c556c951eb10792f9d57be929323366648eb6e083d0d23af";
/// Fresh completion and recovery share this exact current scope gate. Known
/// historical contracts can describe lineage but cannot approve a new draft.
#[cfg(any(test, all(feature = "firmware-linux-completion", target_os = "linux")))]
pub(crate) fn current_firmware_scope(image_sha256: &str, contract_sha256: &str) -> bool {
    image_sha256 == VENDOR_IMAGE_SHA256 && contract_sha256 == VENDOR_CONTRACT_SHA256
}
/// Finite host policy, not measured device timing or the native unbounded drain.
pub const VENDOR_ACTION_BUDGET_MS: u64 = 10_000;
pub const VENDOR_ANNOUNCEMENT_GAP_MS: u64 = 30;
const MAX_FRAME_BYTES: usize = 4096;
const MAX_ACTION_FRAMES: usize = 64;
const SELECTORS: [FirmwareSectionSelector; 4] = FirmwareSectionSelector::IMAGE_TRANSFER_ORDER;
// Geometry stays in raw FRMW table order; selectors apply the native permutation.
const EXACT_GEOMETRY: [(usize, usize); 4] = [
    (80, 4_192_632),
    (4_192_712, 1_834_032),
    (6_026_744, 294_912),
    (6_321_656, 131_072),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VendorPhase {
    Identity,
    Announcement,
    SectionRequest,
    Descriptor,
    Block,
    SectionTerminator,
    TerminalQuery,
    FinishControl,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VendorFailure {
    UnsupportedImage,
    InvalidClock,
    DeadlineOverflow,
    DeadlineExpired,
    WrongAction,
    WrongGeneration,
    ReceiveSequenceGap,
    ReceiveBeforeOffer,
    UnexpectedTraffic,
    /// A complete descriptor response failed the unchanged native status predicate.
    /// Section numbers here are one-based; action section indices remain zero-based.
    DescriptorStatusRejected {
        section_one_based: u8,
        selector: u8,
        status: u8,
    },
    ExtraTraffic,
    MalformedFrame,
    NativeGlobalError,
    InvalidIdentity,
    ProductMismatch,
    InvalidCountOrOffset,
    PartialOrUnknownWrite,
    DrainNotReturned,
    EvidenceCapacity,
    Cancelled,
    ReaderOrBindingLost,
    AlreadyStopped,
    AlreadyFinished,
}
impl std::fmt::Display for VendorFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DescriptorStatusRejected {
                section_one_based,
                selector,
                status,
            } => write!(
                f,
                "Descriptor response rejected for section {section_one_based} (selector 0x{selector:02x}): received status 0x{status:02x}, expected 0x7f"
            ),
            _ => write!(f, "{self:?}"),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VendorState {
    Ready {
        phase: VendorPhase,
        not_before_ms: u64,
    },
    Writing {
        phase: VendorPhase,
        ordinal: u64,
        deadline_ms: u64,
    },
    AwaitingReply {
        phase: VendorPhase,
        ordinal: u64,
        deadline_ms: u64,
    },
    /// Only the last host 09 offer and drain finished; commit/return are unknown.
    HostFinishedUnverified,
    Stopped {
        reason: VendorFailure,
        any_offer_attempted: bool,
    },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorBlockSource {
    pub absolute_block_ordinal: u64,
    pub block_in_section: u32,
    pub index_mod128: u8,
    pub file_offset: usize,
    pub content_bytes: usize,
    pub padded_bytes: usize,
    pub content_sha256: String,
    pub padding_sha256: String,
}
/// A description of bytes, never a transport capability. Fields are immutable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorAction {
    ordinal: u64,
    phase: VendorPhase,
    wire: Vec<u8>,
    wire_sha256: String,
    image_sha256: String,
    section: Option<u8>,
    selector: Option<u8>,
    block: Option<VendorBlockSource>,
}
impl VendorAction {
    pub fn ordinal(&self) -> u64 {
        self.ordinal
    }
    pub fn phase(&self) -> VendorPhase {
        self.phase
    }
    pub fn wire(&self) -> &[u8] {
        &self.wire
    }
    pub fn wire_sha256(&self) -> &str {
        &self.wire_sha256
    }
    pub fn image_sha256(&self) -> &str {
        &self.image_sha256
    }
    /// Zero-based native transfer phase, not a raw FRMW table slot.
    pub fn section(&self) -> Option<u8> {
        self.section
    }
    pub fn selector(&self) -> Option<u8> {
        self.selector
    }
    pub fn block(&self) -> Option<&VendorBlockSource> {
        self.block.as_ref()
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VendorWriteExtent {
    Known(usize),
    Unknown,
    Failed { known_bytes: Option<usize> },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VendorDrain {
    Returned,
    NotAttempted,
    Failed,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorWriteReceipt {
    pub at_ms: u64,
    pub extent: VendorWriteExtent,
    pub drain: VendorDrain,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorFrameObservation {
    pub generation: u64,
    /// Driver-local complete-frame sequence, never an echoed device ordinal.
    pub sequence: u64,
    pub at_ms: u64,
    pub raw: Vec<u8>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VendorDecision {
    BootIdentity {
        raw: Vec<u8>,
        chunk_bytes: u16,
    },
    AnnouncementWritten,
    /// Only the phase-relative native predicate matched an unkeyed raw status.
    OpaqueStatusAccepted,
    TerminalDataAccepted {
        decoded: Vec<u8>,
    },
    HostFinishedUnverified,
    Stopped(VendorFailure),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorTerminalDataObservation {
    pub frame_sequence: u64,
    pub at_ms: u64,
    pub opcode: u8,
    pub decoded: Vec<u8>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorActionReceipt {
    pub action: VendorAction,
    pub offered_at_ms: u64,
    pub receive_frontier_at_offer: u64,
    pub write: Option<VendorWriteReceipt>,
    pub frames: Vec<VendorFrameObservation>,
    pub terminal_data: Vec<VendorTerminalDataObservation>,
    pub decision: VendorDecision,
}
/// The first rejected callback is retained separately, including pre-offer input.
/// Oversized original data stays with the driver; only its byte count is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorRejectedFrame {
    pub generation: u64,
    pub sequence: u64,
    pub at_ms: u64,
    pub original_bytes: usize,
    pub raw: Option<Vec<u8>>,
}
struct Pending {
    action: VendorAction,
    offered_at_ms: u64,
    frontier: u64,
    deadline_ms: u64,
    write: Option<VendorWriteReceipt>,
    frames: Vec<VendorFrameObservation>,
}
/// Deterministic one-outstanding-action model. Construction grants no I/O.
pub struct VendorSequence {
    review: FirmwareReview,
    generation: u64,
    clock_ms: u64,
    last_frame_sequence: u64,
    last_frame_at_ms: Option<u64>,
    phase: VendorPhase,
    section: usize,
    block_cursor: u32,
    absolute_block_ordinal: u64,
    counts: [u32; 4],
    chunk: Option<u16>,
    ordinal: u64,
    not_before_ms: u64,
    prepared: Option<VendorAction>,
    pending: Option<Pending>,
    completed: Option<VendorActionReceipt>,
    failure: Option<VendorFailure>,
    rejected_frame: Option<VendorRejectedFrame>,
    any_offer_attempted: bool,
    finished: bool,
}
impl VendorSequence {
    /// Accepts the exact retained image geometry/product/tag/hash only.
    pub fn new(review: FirmwareReview, generation: u64, at_ms: u64) -> Result<Self, VendorFailure> {
        let summary = review.summary();
        if summary.model != DeviceModel::Gp200
            || summary.file_bytes != 6_452_728
            || summary.source_sha256.hex() != VENDOR_IMAGE_SHA256
            || summary.image_provenance.version_tag_raw() != b"V182"
            || summary
                .sections
                .iter()
                .zip(EXACT_GEOMETRY)
                .any(|(s, (offset, bytes))| s.file_offset != offset || s.bytes != bytes)
        {
            return Err(VendorFailure::UnsupportedImage);
        }
        Self::from_checked_review(review, generation, at_ms)
    }
    fn from_checked_review(
        review: FirmwareReview,
        generation: u64,
        at_ms: u64,
    ) -> Result<Self, VendorFailure> {
        if generation == 0
            || review.summary().model != DeviceModel::Gp200
            || review
                .summary()
                .sections
                .iter()
                .any(|s| s.bytes == 0 || s.bytes > u32::MAX as usize)
            || review
                .summary()
                .image_provenance
                .announcement_candidate()
                .is_err()
        {
            return Err(VendorFailure::UnsupportedImage);
        }
        Ok(Self {
            review,
            generation,
            clock_ms: at_ms,
            last_frame_sequence: 0,
            last_frame_at_ms: None,
            phase: VendorPhase::Identity,
            section: 0,
            block_cursor: 0,
            absolute_block_ordinal: 0,
            counts: [0; 4],
            chunk: None,
            ordinal: 0,
            not_before_ms: at_ms,
            prepared: None,
            pending: None,
            completed: None,
            failure: None,
            rejected_frame: None,
            any_offer_attempted: false,
            finished: false,
        })
    }
    pub fn state(&self) -> VendorState {
        if let Some(reason) = &self.failure {
            return VendorState::Stopped {
                reason: reason.clone(),
                any_offer_attempted: self.any_offer_attempted,
            };
        }
        if self.finished {
            return VendorState::HostFinishedUnverified;
        }
        if let Some(p) = &self.pending {
            return if p.write.is_some() {
                VendorState::AwaitingReply {
                    phase: p.action.phase,
                    ordinal: p.action.ordinal,
                    deadline_ms: p.deadline_ms,
                }
            } else {
                VendorState::Writing {
                    phase: p.action.phase,
                    ordinal: p.action.ordinal,
                    deadline_ms: p.deadline_ms,
                }
            };
        }
        VendorState::Ready {
            phase: self.phase,
            not_before_ms: self.not_before_ms,
        }
    }
    pub fn accepted_blocks(&self) -> u64 {
        self.absolute_block_ordinal
    }
    pub fn negotiated_chunk(&self) -> Option<u16> {
        self.chunk
    }
    pub fn rejected_frame(&self) -> Option<&VendorRejectedFrame> {
        self.rejected_frame.as_ref()
    }
    /// Taking a receipt is local retirement, not a durable-journal attestation.
    /// No subsequent action is available until the preceding receipt is taken.
    pub fn completed_receipt(&self) -> Option<&VendorActionReceipt> {
        self.completed.as_ref()
    }
    pub fn take_completed(&mut self) -> Option<VendorActionReceipt> {
        self.completed.take()
    }
    pub fn poll_action(&mut self, at_ms: u64) -> Result<Option<&VendorAction>, VendorFailure> {
        self.clock(at_ms)?;
        if self.finished
            || self.pending.is_some()
            || self.completed.is_some()
            || at_ms < self.not_before_ms
        {
            return Ok(None);
        }
        if self.prepared.is_none() {
            match self.describe() {
                Ok(action) => self.prepared = Some(action),
                Err(reason) => return self.stop(reason),
            }
        }
        Ok(self.prepared.as_ref())
    }
    /// Record one invocation. Never permits a resend of the current ordinal.
    pub fn begin_offer(&mut self, ordinal: u64, at_ms: u64) -> Result<(), VendorFailure> {
        self.clock(at_ms)?;
        if self.finished {
            return Err(VendorFailure::AlreadyFinished);
        }
        if self.pending.is_some()
            || self.completed.is_some()
            || at_ms < self.not_before_ms
            || self.prepared.as_ref().is_none_or(|a| a.ordinal != ordinal)
        {
            return self.stop(VendorFailure::WrongAction);
        }
        let deadline_ms = match at_ms.checked_add(VENDOR_ACTION_BUDGET_MS) {
            Some(value) => value,
            None => return self.stop(VendorFailure::DeadlineOverflow),
        };
        self.pending = Some(Pending {
            action: self.prepared.take().expect("checked action"),
            offered_at_ms: at_ms,
            frontier: self.last_frame_sequence,
            deadline_ms,
            write: None,
            frames: Vec::new(),
        });
        self.any_offer_attempted = true;
        Ok(())
    }
    /// Complete host extent and drain are prerequisites even if a reply arrived.
    pub fn finish_write(
        &mut self,
        ordinal: u64,
        receipt: VendorWriteReceipt,
    ) -> Result<(), VendorFailure> {
        let receipt_at_ms = receipt.at_ms;
        let Some(p) = &mut self.pending else {
            return self.stop(VendorFailure::WrongAction);
        };
        if p.action.ordinal != ordinal || p.write.is_some() {
            return self.stop(VendorFailure::WrongAction);
        }
        let full = receipt.extent == VendorWriteExtent::Known(p.action.wire.len());
        let drained = receipt.drain == VendorDrain::Returned;
        let before_offer = receipt_at_ms < p.offered_at_ms;
        p.write = Some(receipt.clone());
        if before_offer {
            return self.stop(VendorFailure::InvalidClock);
        }
        self.clock(self.clock_ms.max(receipt_at_ms))?;
        if !full {
            return self.stop(VendorFailure::PartialOrUnknownWrite);
        }
        if !drained {
            return self.stop(VendorFailure::DrainNotReturned);
        }
        self.pending.as_mut().expect("current action").deadline_ms =
            match receipt.at_ms.checked_add(VENDOR_ACTION_BUDGET_MS) {
                Some(value) => value,
                None => return self.stop(VendorFailure::DeadlineOverflow),
            };
        match self.phase {
            VendorPhase::Announcement => {
                self.not_before_ms = match receipt.at_ms.checked_add(VENDOR_ANNOUNCEMENT_GAP_MS) {
                    Some(value) => value,
                    None => return self.stop(VendorFailure::DeadlineOverflow),
                };
                self.phase = VendorPhase::SectionRequest;
                self.complete(VendorDecision::AnnouncementWritten);
                Ok(())
            }
            VendorPhase::FinishControl => {
                self.finished = true;
                self.complete(VendorDecision::HostFinishedUnverified);
                Ok(())
            }
            _ => self.decide_buffered(),
        }
    }
    /// All input is complete-frame evidence for this caller-described generation.
    /// Original chunks/framing/frontier must also be retained by a future driver.
    /// Queued capture timestamps may precede a later host poll or drain receipt.
    /// Process queued original frames before asserting timeout; an already stopped
    /// deadline cannot be undone by a late-delivered observation.
    pub fn receive(&mut self, frame: VendorFrameObservation) -> Result<(), VendorFailure> {
        if let Err(reason) = self.clock(self.clock_ms.max(frame.at_ms)) {
            self.retain_rejected(frame);
            return Err(reason);
        }
        if self.finished {
            return Err(VendorFailure::AlreadyFinished);
        }
        if frame.generation != self.generation {
            return self.reject_frame(frame, VendorFailure::WrongGeneration);
        }
        if self
            .last_frame_at_ms
            .is_some_and(|previous| frame.at_ms < previous)
        {
            return self.reject_frame(frame, VendorFailure::InvalidClock);
        }
        if self.last_frame_sequence.checked_add(1) != Some(frame.sequence) {
            return self.reject_frame(frame, VendorFailure::ReceiveSequenceGap);
        }
        self.last_frame_sequence = frame.sequence;
        self.last_frame_at_ms = Some(frame.at_ms);
        let Some(p) = &mut self.pending else {
            return self.reject_frame(frame, VendorFailure::ReceiveBeforeOffer);
        };
        if frame.sequence <= p.frontier || frame.at_ms < p.offered_at_ms {
            return self.reject_frame(frame, VendorFailure::ReceiveBeforeOffer);
        }
        if p.frames.len() >= MAX_ACTION_FRAMES {
            return self.reject_frame(frame, VendorFailure::EvidenceCapacity);
        }
        if frame.raw.len() > MAX_FRAME_BYTES {
            return self.reject_frame(frame, VendorFailure::MalformedFrame);
        }
        let safe = safe_frame(&frame.raw);
        let error = frame.raw == FirmwareFixedPacket::NativeGlobalErrorCandidate.raw();
        p.frames.push(frame);
        if !safe {
            return self.stop(VendorFailure::MalformedFrame);
        }
        if error {
            return self.stop(VendorFailure::NativeGlobalError);
        }
        if self.phase == VendorPhase::Announcement {
            return self.stop(VendorFailure::UnexpectedTraffic);
        }
        if self.phase == VendorPhase::FinishControl {
            return Ok(());
        }
        if self
            .pending
            .as_ref()
            .expect("current action")
            .write
            .is_some()
        {
            self.decide_buffered()
        } else {
            Ok(())
        }
    }
    pub fn tick(&mut self, at_ms: u64) -> Result<(), VendorFailure> {
        self.clock(at_ms)
    }
    pub fn cancel(&mut self) -> Result<(), VendorFailure> {
        if self.finished {
            return Err(VendorFailure::AlreadyFinished);
        }
        self.stop(VendorFailure::Cancelled)
    }
    pub fn reader_or_binding_lost(&mut self) -> Result<(), VendorFailure> {
        self.stop(VendorFailure::ReaderOrBindingLost)
    }
    fn retain_rejected(&mut self, frame: VendorFrameObservation) {
        if self.rejected_frame.is_none() {
            self.rejected_frame = Some(VendorRejectedFrame {
                generation: frame.generation,
                sequence: frame.sequence,
                at_ms: frame.at_ms,
                original_bytes: frame.raw.len(),
                raw: (frame.raw.len() <= MAX_FRAME_BYTES).then_some(frame.raw),
            });
        }
    }
    fn reject_frame(
        &mut self,
        frame: VendorFrameObservation,
        reason: VendorFailure,
    ) -> Result<(), VendorFailure> {
        self.retain_rejected(frame);
        self.stop(reason)
    }
    fn clock(&mut self, at_ms: u64) -> Result<(), VendorFailure> {
        if self.failure.is_some() {
            return Err(VendorFailure::AlreadyStopped);
        }
        if at_ms < self.clock_ms {
            return self.stop(VendorFailure::InvalidClock);
        }
        self.clock_ms = at_ms;
        if self
            .pending
            .as_ref()
            .is_some_and(|p| at_ms >= p.deadline_ms)
        {
            return self.stop(VendorFailure::DeadlineExpired);
        }
        Ok(())
    }
    fn stop<T>(&mut self, reason: VendorFailure) -> Result<T, VendorFailure> {
        if self.failure.is_some() {
            return Err(VendorFailure::AlreadyStopped);
        }
        if self.pending.is_some() {
            self.complete(VendorDecision::Stopped(reason.clone()));
        }
        self.prepared = None;
        self.failure = Some(reason.clone());
        Err(reason)
    }
    fn complete(&mut self, decision: VendorDecision) {
        let p = self.pending.take().expect("one outstanding action");
        debug_assert!(self.completed.is_none());
        let terminal_observations = if p.action.phase == VendorPhase::TerminalQuery {
            p.frames
                .iter()
                .filter_map(|frame| {
                    if !safe_frame(&frame.raw) {
                        return None;
                    }
                    terminal_data(&frame.raw).map(|decoded| VendorTerminalDataObservation {
                        frame_sequence: frame.sequence,
                        at_ms: frame.at_ms,
                        opcode: frame.raw[3],
                        decoded,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        self.completed = Some(VendorActionReceipt {
            action: p.action,
            offered_at_ms: p.offered_at_ms,
            receive_frontier_at_offer: p.frontier,
            write: p.write,
            frames: p.frames,
            terminal_data: terminal_observations,
            decision,
        });
        self.ordinal += 1;
    }
    fn describe(&self) -> Result<VendorAction, VendorFailure> {
        let mut block = None;
        let wire = match self.phase {
            VendorPhase::Identity => FirmwareFixedPacket::IdentityQuery.raw().to_vec(),
            VendorPhase::Announcement => self
                .review
                .summary()
                .image_provenance
                .announcement_candidate()
                .map_err(|_| VendorFailure::UnsupportedImage)?
                .raw()
                .to_vec(),
            VendorPhase::SectionRequest => SELECTORS[self.section].request_candidate().to_vec(),
            VendorPhase::Descriptor => FirmwareDescriptor::candidate(
                self.review.summary().sections[SELECTORS[self.section]
                    .image_table_slot()
                    .expect("image selector")]
                .bytes as u32,
                self.counts[self.section],
            )
            .raw()
            .to_vec(),
            VendorPhase::Block => {
                let slot = SELECTORS[self.section]
                    .image_table_slot()
                    .expect("image selector");
                let section = &self.review.summary().sections[slot];
                let chunk = usize::from(self.chunk.ok_or(VendorFailure::InvalidIdentity)?);
                if self.block_cursor >= self.counts[self.section] {
                    return Err(VendorFailure::InvalidCountOrOffset);
                }
                let local = (self.block_cursor as usize)
                    .checked_mul(chunk)
                    .ok_or(VendorFailure::InvalidCountOrOffset)?;
                let remaining = section
                    .bytes
                    .checked_sub(local)
                    .ok_or(VendorFailure::InvalidCountOrOffset)?;
                let content_bytes = remaining.min(chunk);
                let start = section
                    .file_offset
                    .checked_add(local)
                    .ok_or(VendorFailure::InvalidCountOrOffset)?;
                let end = start
                    .checked_add(content_bytes)
                    .ok_or(VendorFailure::InvalidCountOrOffset)?;
                let content = self
                    .review
                    .source()
                    .get(start..end)
                    .ok_or(VendorFailure::InvalidCountOrOffset)?;
                let padding = vec![0xff; chunk - content_bytes];
                block = Some(VendorBlockSource {
                    absolute_block_ordinal: self.absolute_block_ordinal,
                    block_in_section: self.block_cursor,
                    index_mod128: (self.block_cursor % 128) as u8,
                    file_offset: start,
                    content_bytes,
                    padded_bytes: padding.len(),
                    content_sha256: JobSha256::of(content).hex(),
                    padding_sha256: JobSha256::of(&padding).hex(),
                });
                firmware_block_candidate(self.block_cursor, chunk as u16, content)
                    .map_err(|_| VendorFailure::InvalidCountOrOffset)?
                    .raw()
                    .to_vec()
            }
            VendorPhase::SectionTerminator => FirmwareFixedPacket::SectionTerminator.raw().to_vec(),
            VendorPhase::TerminalQuery => FirmwareFixedPacket::TerminalQuery.raw().to_vec(),
            VendorPhase::FinishControl => FirmwareFixedPacket::TerminalControl.raw().to_vec(),
        };
        let is_section = matches!(
            self.phase,
            VendorPhase::Announcement
                | VendorPhase::SectionRequest
                | VendorPhase::Descriptor
                | VendorPhase::Block
                | VendorPhase::SectionTerminator
        );
        Ok(VendorAction {
            ordinal: self.ordinal,
            phase: self.phase,
            wire_sha256: JobSha256::of(&wire).hex(),
            wire,
            image_sha256: self.review.summary().source_sha256.hex(),
            section: is_section.then_some(self.section as u8),
            selector: is_section.then_some(SELECTORS[self.section].raw()),
            block,
        })
    }
    fn decide_buffered(&mut self) -> Result<(), VendorFailure> {
        let frames = &self.pending.as_ref().expect("current action").frames;
        if frames.is_empty() {
            return Ok(());
        }
        if self.phase == VendorPhase::TerminalQuery {
            let mut accepted = None;
            for frame in frames {
                let Some(decoded) = terminal_data(&frame.raw) else {
                    return self.stop(VendorFailure::UnexpectedTraffic);
                };
                if accepted.is_some() {
                    return self.stop(VendorFailure::ExtraTraffic);
                }
                if frame.raw[3] == 7 {
                    accepted = Some(decoded);
                }
            }
            if let Some(decoded) = accepted {
                self.phase = VendorPhase::FinishControl;
                self.complete(VendorDecision::TerminalDataAccepted { decoded });
            }
            return Ok(());
        }
        if frames.len() != 1 {
            return self.stop(VendorFailure::ExtraTraffic);
        }
        let raw = &frames[0].raw;
        let decision = match self.phase {
            VendorPhase::Identity => {
                let identity = match BootloaderIdentity::decode(raw) {
                    Ok(value) => value,
                    Err(_) => return self.stop(VendorFailure::InvalidIdentity),
                };
                if identity.product_raw != *self.review.summary().image_provenance.product_raw() {
                    return self.stop(VendorFailure::ProductMismatch);
                }
                for (index, selector) in SELECTORS.iter().enumerate() {
                    let slot = selector.image_table_slot().expect("image selector");
                    let section = &self.review.summary().sections[slot];
                    let count = (section.bytes as u64).div_ceil(u64::from(identity.chunk_bytes));
                    self.counts[index] = match u32::try_from(count) {
                        Ok(value) if value > 0 => value,
                        _ => return self.stop(VendorFailure::InvalidCountOrOffset),
                    };
                }
                self.chunk = Some(identity.chunk_bytes);
                self.phase = VendorPhase::Announcement;
                VendorDecision::BootIdentity {
                    raw: identity.raw().to_vec(),
                    chunk_bytes: identity.chunk_bytes,
                }
            }
            VendorPhase::SectionRequest if raw.len() >= 6 && raw[4] != 0x7e => {
                self.block_cursor = 0;
                self.phase = VendorPhase::Descriptor;
                VendorDecision::OpaqueStatusAccepted
            }
            VendorPhase::Descriptor if raw.len() >= 6 && raw[4] == 0x7f => {
                self.phase = VendorPhase::Block;
                VendorDecision::OpaqueStatusAccepted
            }
            VendorPhase::Descriptor if raw.len() >= 6 => {
                let reason = VendorFailure::DescriptorStatusRejected {
                    section_one_based: self.section as u8 + 1,
                    selector: SELECTORS[self.section].raw(),
                    status: raw[4],
                };
                let rejected = frames[0].clone();
                self.retain_rejected(rejected);
                return self.stop(reason);
            }
            VendorPhase::Block if raw.len() >= 6 && raw[4] == 0x7f => {
                self.block_cursor += 1;
                self.absolute_block_ordinal += 1;
                if self.block_cursor == self.counts[self.section] {
                    self.phase = VendorPhase::SectionTerminator;
                }
                VendorDecision::OpaqueStatusAccepted
            }
            VendorPhase::SectionTerminator if raw.len() >= 6 && raw[4] == 0x7f => {
                self.block_cursor = 0;
                if self.section == 3 {
                    self.phase = VendorPhase::TerminalQuery;
                } else {
                    self.section += 1;
                    self.phase = VendorPhase::Announcement;
                }
                VendorDecision::OpaqueStatusAccepted
            }
            _ => return self.stop(VendorFailure::UnexpectedTraffic),
        };
        self.complete(decision);
        Ok(())
    }
}
fn safe_frame(raw: &[u8]) -> bool {
    (5..=MAX_FRAME_BYTES).contains(&raw.len())
        && raw[..3] == [0xf0, 0x7e, 0x48]
        && raw.last() == Some(&0xf7)
        && raw[1..raw.len() - 1].iter().all(|b| *b <= 0x7f)
}
fn terminal_data(raw: &[u8]) -> Option<Vec<u8>> {
    if !matches!(raw[3], 3 | 7) {
        return None;
    }
    let body = &raw[4..raw.len() - 1];
    if !body.len().is_multiple_of(2) || body.len() > 510 || body.iter().any(|b| *b > 15) {
        return None;
    }
    Some(
        body.as_chunks::<2>()
            .0
            .iter()
            .map(|pair| (pair[0] << 4) | pair[1])
            .collect(),
    )
}
#[cfg(test)]
#[path = "firmware_vendor_sequence_tests.rs"]
mod tests;

#[cfg(test)]
mod public_contract_tests {
    use super::*;

    #[test]
    fn fresh_scope_requires_the_compiled_public_contract_and_exact_image() {
        let contract = include_bytes!("../../../assets/firmware/protocol-contract.json");
        assert_eq!(JobSha256::of(contract).hex(), VENDOR_CONTRACT_SHA256);
        assert!(current_firmware_scope(
            VENDOR_IMAGE_SHA256,
            VENDOR_CONTRACT_SHA256
        ));
        for historical in [
            LEGACY_VENDOR_CONTRACT_SHA256,
            HISTORICAL_CORRECTED_VENDOR_CONTRACT_SHA256,
        ] {
            assert!(!current_firmware_scope(VENDOR_IMAGE_SHA256, historical));
        }
        assert!(!current_firmware_scope(
            &"dd".repeat(32),
            VENDOR_CONTRACT_SHA256
        ));
        assert!(!current_firmware_scope(
            VENDOR_IMAGE_SHA256,
            &"dd".repeat(32)
        ));
    }
}
