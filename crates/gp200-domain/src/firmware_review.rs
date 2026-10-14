//! Offline firmware inspection, preparation and interruption rehearsal. No
//! bootloader entry, erase, finalize, retry, reconnect or flash authorization.
use crate::*;
use gp200_protocol::{BootloaderIdentity, FirmwareSectionSelector, firmware_block_candidate};
// Assets depends on domain already; these pure observational types add no actor
// or transport capability and avoid a new dependency or duplicated normalization.
pub use gp200_protocol::{FirmwareAnnouncement, FirmwareImageProvenance};
use gp200_protocol::{FirmwareRxCandidate, FirmwareRxEvidence};
use serde::Serialize;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewedFirmwareSection {
    /// Original FRMW table slot, independent of native wire transfer order.
    pub index: u8,
    pub file_offset: usize,
    pub bytes: usize,
    pub sha256: JobSha256,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum FirmwareReviewBlocker {
    NoActiveSession,
    InstalledFirmwareUnknown,
    CompleteContentBackupMissing,
    BootloaderTransitionAndReconnectUnverified,
    TransferAcknowledgementsUnverified,
    FinalizationAndRecoveryUnverified,
}
impl FirmwareReviewBlocker {
    pub fn explanation(&self) -> &'static str {
        match self {
            Self::NoActiveSession => {
                "Connect and identify the exact target device before update review"
            }
            Self::InstalledFirmwareUnknown => {
                "The installed firmware version has not been read through a verified version query"
            }
            Self::CompleteContentBackupMissing => {
                "A complete, restorable preset/global/IR/SnapTone content backup has not been verified"
            }
            Self::BootloaderTransitionAndReconnectUnverified => {
                "Normal-mode entry and bootloader USB/MIDI reconnection need a verified transcript"
            }
            Self::TransferAcknowledgementsUnverified => {
                "Bootloader section/block acknowledgement, deadlines and retry boundaries need a verified transcript"
            }
            Self::FinalizationAndRecoveryUnverified => {
                "Commit, reboot, final identity, restore and interruption recovery need a verified transcript"
            }
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FirmwareRecoveryPrerequisites {
    pub safe_cancel_before_first_send: bool,
    pub verified_cancel_after_entry: bool,
    pub verified_resume_position: bool,
    pub verified_restart_or_reentry: bool,
    pub verified_finalize_and_post_update_identity: bool,
    pub instructions: Option<String>,
}
impl Default for FirmwareRecoveryPrerequisites {
    fn default() -> Self {
        Self {
            safe_cancel_before_first_send: true,
            verified_cancel_after_entry: false,
            verified_resume_position: false,
            verified_restart_or_reentry: false,
            verified_finalize_and_post_update_identity: false,
            instructions: None,
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FirmwareBackupAssessment {
    pub stored_presets: usize,
    pub current_buffer: bool,
    pub globals: bool,
    pub verified_ir_contents: usize,
    pub verified_snaptone_contents: usize,
    // No current physical full-content/restore profile can issue this proof.
    // Public counters remain a display assessment, including imported claims.
    complete_content_backup_verified: bool,
}
impl FirmwareBackupAssessment {
    /// A display assessment, never authority from an imported manifest.
    pub fn complete(&self) -> bool {
        self.complete_content_backup_verified
            && self.stored_presets == 256
            && self.current_buffer
            && self.globals
            && self.verified_ir_contents == 20
            && self.verified_snaptone_contents == 10
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct FirmwareReviewSummary {
    pub model: DeviceModel,
    pub image_version: Option<String>,
    pub installed_version: Option<String>,
    pub file_bytes: usize,
    pub source_sha256: JobSha256,
    /// Original FRMW table order; selectors map to these slots explicitly.
    pub sections: [ReviewedFirmwareSection; 4],
    pub checksum_is_signature: bool,
    pub blockers: Vec<FirmwareReviewBlocker>,
    pub recovery: FirmwareRecoveryPrerequisites,
    pub image_provenance: FirmwareImageProvenance,
}
/// Exact immutable file, independently parsed instead of trusting an inspector
/// metadata DTO or filename. This object has no conversion to DeviceCommand.
#[derive(Debug, Clone)]
pub struct FirmwareReview {
    source: RetainedJobBlob,
    summary: FirmwareReviewSummary,
    session: Option<DeviceJobSession>,
}
fn fw_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("bounded FRMW"))
}
impl FirmwareReview {
    pub fn inspect(
        source: RetainedJobBlob,
        session: Option<DeviceJobSession>,
        backup: &FirmwareBackupAssessment,
    ) -> Result<Self, ServiceError> {
        let bytes = source.bytes();
        if bytes.len() < 92
            || &bytes[..4] != b"FRMW"
            || fw_u32(bytes, 4) as usize != bytes.len() - 48
        {
            return Err(ServiceError::invalid(
                "Firmware requires a complete FRMW image with an exact declared length",
            ));
        }
        let product = &bytes[16..32];
        let expected = |name: &[u8]| {
            let mut result = [0; 16];
            result[..name.len()].copy_from_slice(name);
            result
        };
        let model = if product == expected(b"GP-200") {
            DeviceModel::Gp200
        } else if product == expected(b"GP-200LT") {
            DeviceModel::Gp200Lt
        } else if product == expected(b"GP-200JR") {
            DeviceModel::Gp200Jr
        } else {
            return Err(ServiceError::unsupported(
                "FRMW product is not a known exact GP-200 variant",
            ));
        };
        if bytes[48..]
            .iter()
            .fold(0_u8, |sum, byte| sum.wrapping_add(*byte))
            != bytes[14]
        {
            return Err(ServiceError::invalid(
                "FRMW additive payload checksum mismatch",
            ));
        }
        let mut sections: Vec<ReviewedFirmwareSection> = Vec::with_capacity(4);
        for index in 0..4 {
            let relative = fw_u32(bytes, 48 + index * 8) as usize;
            let length = fw_u32(bytes, 52 + index * 8) as usize;
            let offset = relative
                .checked_add(48)
                .ok_or_else(|| ServiceError::invalid("FRMW section offset overflow"))?;
            let end = offset
                .checked_add(length)
                .ok_or_else(|| ServiceError::invalid("FRMW section size overflow"))?;
            if relative < 32
                || length == 0
                || end > bytes.len()
                || sections.iter().any(|other| {
                    offset < other.file_offset + other.bytes && other.file_offset < end
                })
            {
                return Err(ServiceError::invalid(
                    "FRMW section is empty, overlaps the table/another section or exceeds the exact file",
                ));
            }
            sections.push(ReviewedFirmwareSection {
                index: index as u8,
                file_offset: offset,
                bytes: length,
                sha256: JobSha256::of(&bytes[offset..end]),
            });
        }
        if session
            .as_ref()
            .is_some_and(|value| value.identity().model != model)
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Firmware image variant does not match the observed device",
            ));
        }
        let tag = &bytes[88..92];
        let image_version = (matches!(tag[0], b'B' | b'b' | b'V' | b'v')
            && tag[1..].iter().all(u8::is_ascii_digit))
        .then(|| format!("{}.{}.{}", tag[1] - b'0', tag[2] - b'0', tag[3] - b'0'));
        let installed_version = session.as_ref().and_then(|value| {
            value
                .identity()
                .firmware
                .as_ref()
                .map(|f| f.version.clone())
        });
        let mut blockers = vec![
            FirmwareReviewBlocker::BootloaderTransitionAndReconnectUnverified,
            FirmwareReviewBlocker::TransferAcknowledgementsUnverified,
            FirmwareReviewBlocker::FinalizationAndRecoveryUnverified,
        ];
        if session.is_none() {
            blockers.push(FirmwareReviewBlocker::NoActiveSession);
        }
        if installed_version.is_none() {
            blockers.push(FirmwareReviewBlocker::InstalledFirmwareUnknown);
        }
        if !backup.complete() {
            blockers.push(FirmwareReviewBlocker::CompleteContentBackupMissing);
        }
        let summary = FirmwareReviewSummary {
            model,
            image_version,
            installed_version,
            file_bytes: bytes.len(),
            source_sha256: source.sha256(),
            sections: sections.try_into().expect("four sections"),
            checksum_is_signature: false,
            blockers,
            recovery: FirmwareRecoveryPrerequisites::default(),
            image_provenance: FirmwareImageProvenance::from_header(&bytes[..92])
                .map_err(|error| ServiceError::invalid(error.to_string()))?,
        };
        Ok(Self {
            source,
            summary,
            session,
        })
    }
    pub fn summary(&self) -> &FirmwareReviewSummary {
        &self.summary
    }
    pub fn source(&self) -> &[u8] {
        self.source.bytes()
    }
    pub fn session(&self) -> Option<&DeviceJobSession> {
        self.session.as_ref()
    }
    /// Exact bytes for a raw FRMW table slot.
    pub fn section(&self, index: usize) -> Option<&[u8]> {
        let section = self.summary.sections.get(index)?;
        Some(&self.source.bytes()[section.file_offset..section.file_offset + section.bytes])
    }
    pub fn rehearse(
        &self,
        identity: &BootloaderIdentity,
    ) -> Result<FirmwareRehearsal, ServiceError> {
        // Public display projections must not replace the decoder-owned bytes.
        let identity = BootloaderIdentity::decode(identity.raw())
            .map_err(|error| ServiceError::invalid(error.to_string()))?;
        let mut product = [0; 16];
        let name = self.summary.model.name();
        product[..name.len()].copy_from_slice(name.as_bytes());
        if identity.product_raw != product {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Offline bootloader identity fixture does not match firmware variant",
            ));
        }
        let count_review = self.count_review(identity.chunk_bytes)?;
        let blocks = count_review
            .sections
            .iter()
            .map(|section| u64::from(section.count.safe_integer_blocks))
            .sum();
        Ok(FirmwareRehearsal {
            review: self.clone(),
            chunk_bytes: identity.chunk_bytes,
            phase: FirmwareRehearsalPhase::Prepared,
            section: 0,
            block: 0,
            acknowledged_bytes: 0,
            total_blocks: blocks,
            pending: None,
            count_review,
        })
    }
    /// An explicit offline chunk fixture. Agreement is not device negotiation,
    /// a transfer profile or acknowledgement proof.
    pub fn count_review(&self, chunk_bytes: u16) -> Result<FirmwareCountReview, ServiceError> {
        let sections = self
            .summary
            .sections
            .iter()
            .map(|section| {
                Ok(FirmwareSectionCountReview {
                    image_table_slot: section.index,
                    selector_raw: FirmwareSectionSelector::for_image_table_slot(usize::from(
                        section.index,
                    ))
                    .expect("four raw table slots")
                    .raw(),
                    source_sha256: section.sha256,
                    count: firmware_block_count_comparison(section.bytes as u32, chunk_bytes)?,
                })
            })
            .collect::<Result<Vec<_>, ServiceError>>()?;
        let discrepancy = sections.iter().any(|section| section.count.discrepancy());
        let mut blockers = vec![FirmwareCountReviewBlocker::OperationalTransferUnverified];
        if discrepancy {
            blockers.push(FirmwareCountReviewBlocker::IntegerNativeCountDiscrepancy);
        }
        Ok(FirmwareCountReview {
            source_sha256: self.summary.source_sha256,
            sections: sections.try_into().expect("four reviewed sections"),
            blockers,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum FirmwareRehearsalPhase {
    Prepared,
    Transferring,
    AwaitingBlockAcknowledgement,
    AwaitingSectionAcknowledgement,
    AwaitingFinalizeEvidence,
    Cancelled,
    Indeterminate,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FirmwareChunkPreview {
    pub source_sha256: JobSha256,
    /// Zero-based native transfer phase, not a raw FRMW table slot.
    pub section: u8,
    pub block_in_section: u32,
    pub index_mod128: u8,
    pub file_offset: usize,
    pub content_bytes: usize,
    pub padded_bytes: usize,
    pub wire_bytes: usize,
    pub wire_sha256: JobSha256,
}
/// Offline rehearsal consumes explicit simulated ACKs. It cannot produce
/// device commands or a successful-update claim, and never retries an unknown
/// send. The final state remains blocked at missing finalization evidence.
#[derive(Debug, Clone)]
pub struct FirmwareRehearsal {
    review: FirmwareReview,
    chunk_bytes: u16,
    phase: FirmwareRehearsalPhase,
    section: usize,
    block: u32,
    acknowledged_bytes: u64,
    total_blocks: u64,
    pending: Option<FirmwareChunkPreview>,
    count_review: FirmwareCountReview,
}
impl FirmwareRehearsal {
    pub fn phase(&self) -> &FirmwareRehearsalPhase {
        &self.phase
    }
    pub fn total_blocks(&self) -> u64 {
        self.total_blocks
    }
    pub fn count_review(&self) -> &FirmwareCountReview {
        &self.count_review
    }
    pub fn progress(&self) -> JobProgress {
        JobProgress {
            completed: self.acknowledged_bytes,
            total: Some(
                self.review
                    .summary
                    .sections
                    .iter()
                    .map(|v| v.bytes as u64)
                    .sum(),
            ),
            message: format!("Offline firmware rehearsal: {:?}", self.phase),
            cancellable: self.phase == FirmwareRehearsalPhase::Prepared,
        }
    }
    pub fn next_chunk(&mut self) -> Result<FirmwareChunkPreview, ServiceError> {
        if self
            .count_review
            .blockers
            .contains(&FirmwareCountReviewBlocker::IntegerNativeCountDiscrepancy)
        {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Offline firmware counts differ from native float32 planning; resolve the discrepancy before rehearsal",
            ));
        }
        if !matches!(
            self.phase,
            FirmwareRehearsalPhase::Prepared | FirmwareRehearsalPhase::Transferring
        ) {
            return Err(ServiceError::new(
                ErrorKind::Busy,
                "Rehearsal needs the outstanding acknowledgement/evidence; no automatic resend",
            ));
        }
        let slot = FirmwareSectionSelector::IMAGE_TRANSFER_ORDER[self.section]
            .image_table_slot()
            .expect("image selector");
        let section = &self.review.summary.sections[slot];
        let local_offset = self.block as usize * usize::from(self.chunk_bytes);
        let content_bytes = (section.bytes - local_offset).min(usize::from(self.chunk_bytes));
        let file_offset = section.file_offset + local_offset;
        let block = firmware_block_candidate(
            self.block,
            self.chunk_bytes,
            &self.review.source()[file_offset..file_offset + content_bytes],
        )
        .map_err(|e| ServiceError::invalid(e.to_string()))?;
        let preview = FirmwareChunkPreview {
            source_sha256: self.review.summary.source_sha256,
            section: self.section as u8,
            block_in_section: self.block,
            index_mod128: block.block_index_mod128,
            file_offset,
            content_bytes,
            padded_bytes: usize::from(self.chunk_bytes) - content_bytes,
            wire_bytes: block.raw().len(),
            wire_sha256: JobSha256::of(block.raw()),
        };
        self.pending = Some(preview.clone());
        self.phase = FirmwareRehearsalPhase::AwaitingBlockAcknowledgement;
        Ok(preview)
    }
    pub fn acknowledge_simulated_chunk(
        &mut self,
        preview: &FirmwareChunkPreview,
    ) -> Result<(), ServiceError> {
        if self.phase != FirmwareRehearsalPhase::AwaitingBlockAcknowledgement
            || self.pending.as_ref() != Some(preview)
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Offline ACK does not match the pending file, section and full block digest",
            ));
        }
        self.acknowledged_bytes += preview.content_bytes as u64;
        self.block += 1;
        self.pending = None;
        let slot = FirmwareSectionSelector::IMAGE_TRANSFER_ORDER[self.section]
            .image_table_slot()
            .expect("image selector");
        self.phase = if self.block as usize * usize::from(self.chunk_bytes)
            >= self.review.summary.sections[slot].bytes
        {
            FirmwareRehearsalPhase::AwaitingSectionAcknowledgement
        } else {
            FirmwareRehearsalPhase::Transferring
        };
        Ok(())
    }
    pub fn acknowledge_simulated_section(
        &mut self,
        source_sha256: JobSha256,
        section: u8,
    ) -> Result<(), ServiceError> {
        if self.phase != FirmwareRehearsalPhase::AwaitingSectionAcknowledgement
            || source_sha256 != self.review.summary.source_sha256
            || usize::from(section) != self.section
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Offline section ACK does not match the pending firmware section",
            ));
        }
        self.section += 1;
        self.block = 0;
        self.phase = if self.section == 4 {
            FirmwareRehearsalPhase::AwaitingFinalizeEvidence
        } else {
            FirmwareRehearsalPhase::Transferring
        };
        Ok(())
    }
    pub fn cancel(&mut self) -> Result<(), ServiceError> {
        if self.phase != FirmwareRehearsalPhase::Prepared {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "No hardware cancellation boundary is verified after firmware preparation",
            ));
        }
        self.phase = FirmwareRehearsalPhase::Cancelled;
        Ok(())
    }
    pub fn acknowledgement_missing(&mut self) {
        if matches!(
            self.phase,
            FirmwareRehearsalPhase::AwaitingBlockAcknowledgement
                | FirmwareRehearsalPhase::AwaitingSectionAcknowledgement
                | FirmwareRehearsalPhase::AwaitingFinalizeEvidence
        ) {
            self.phase = FirmwareRehearsalPhase::Indeterminate;
        }
    }
    /// The same immutable source is retained for diagnosis. This does not
    /// establish a bootloader resume position or permit retry/erase/restart.
    pub fn retained_source(&self) -> Arc<[u8]> {
        self.review.source.retained_bytes()
    }
}

/// Comparison scope matches the local 64MiB source bound and excludes native
/// signed-overflow/NaN/conversion-error behavior. Integer planning remains primary.
pub const MAX_FIRMWARE_COUNT_SECTION_BYTES: u32 = 64 * 1024 * 1024;
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FirmwareBlockCountComparison {
    pub section_bytes: u32,
    pub chunk_bytes: u16,
    pub safe_integer_blocks: u32,
    pub native_float32_blocks: u32,
}
impl FirmwareBlockCountComparison {
    pub fn discrepancy(&self) -> bool {
        self.safe_integer_blocks != self.native_float32_blocks
    }
}
pub fn firmware_block_count_comparison(
    section_bytes: u32,
    chunk_bytes: u16,
) -> Result<FirmwareBlockCountComparison, ServiceError> {
    if section_bytes > MAX_FIRMWARE_COUNT_SECTION_BYTES || !(1..=255).contains(&chunk_bytes) {
        return Err(ServiceError::invalid(
            "Firmware count comparison requires 0..64MiB section bytes and chunk1..255; native full error domain is not emulated",
        ));
    }
    let safe_integer_blocks = section_bytes.div_ceil(u32::from(chunk_bytes));
    // Original sender converts nibble length and twice chunk to float32, divides
    // in float32, then calls ceil. Do not silently substitute the integer count.
    let native_float32_blocks =
        ((section_bytes * 2) as f32 / (u32::from(chunk_bytes) * 2) as f32).ceil() as u32;
    Ok(FirmwareBlockCountComparison {
        section_bytes,
        chunk_bytes,
        safe_integer_blocks,
        native_float32_blocks,
    })
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum FirmwareCountReviewBlocker {
    IntegerNativeCountDiscrepancy,
    OperationalTransferUnverified,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FirmwareSectionCountReview {
    pub image_table_slot: u8,
    pub selector_raw: u8,
    pub source_sha256: JobSha256,
    pub count: FirmwareBlockCountComparison,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FirmwareCountReview {
    pub source_sha256: JobSha256,
    /// Original raw FRMW table order, with an explicit selector on each entry.
    pub sections: [FirmwareSectionCountReview; 4],
    pub blockers: Vec<FirmwareCountReviewBlocker>,
}

pub const MAX_FIRMWARE_OFFLINE_RX_RECORDS: usize = 256;
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum FirmwareOfflineRxState {
    Recording,
    StoppedNativeError,
    StoppedRejectedPacket,
    StoppedOversizedPacket,
    StoppedDisconnect,
    StoppedCapacity,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FirmwareOfflineRxRecord {
    /// File-local record order only; never sent or matched as a device ACK key.
    pub ordinal: usize,
    pub evidence: FirmwareRxEvidence,
}
/// Bounded offline diagnosis. No status, duplicate, terminal data, ordinal or
/// identity packet can grant transfer progress, completion, resume or authority.
/// This is separate from FirmwareRehearsal's explicit simulated-ACK API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FirmwareOfflineRxTranscript {
    state: FirmwareOfflineRxState,
    records: Vec<FirmwareOfflineRxRecord>,
}
impl Default for FirmwareOfflineRxTranscript {
    fn default() -> Self {
        Self {
            state: FirmwareOfflineRxState::Recording,
            records: Vec::new(),
        }
    }
}
impl FirmwareOfflineRxTranscript {
    pub fn state(&self) -> &FirmwareOfflineRxState {
        &self.state
    }
    pub fn records(&self) -> &[FirmwareOfflineRxRecord] {
        &self.records
    }
    pub fn device_progress_verified(&self) -> bool {
        false
    }
    pub fn completion_verified(&self) -> bool {
        false
    }
    pub fn resume_supported(&self) -> bool {
        false
    }
    pub fn record_candidate(&mut self, packet: &[u8]) -> Result<(), ServiceError> {
        if self.state != FirmwareOfflineRxState::Recording {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Offline firmware transcript stopped; resume/retry is unsupported",
            ));
        }
        if self.records.len() == MAX_FIRMWARE_OFFLINE_RX_RECORDS {
            self.state = FirmwareOfflineRxState::StoppedCapacity;
            return Err(ServiceError::new(
                ErrorKind::Busy,
                "Offline firmware transcript capacity reached; additional input was not copied",
            ));
        }
        let evidence = match FirmwareRxEvidence::retain(packet) {
            Ok(evidence) => evidence,
            Err(error) => {
                self.state = FirmwareOfflineRxState::StoppedOversizedPacket;
                return Err(ServiceError::invalid(error.to_string()));
            }
        };
        self.state = match evidence.classification() {
            Ok(FirmwareRxCandidate::NativeGlobalError) => {
                FirmwareOfflineRxState::StoppedNativeError
            }
            Err(_) => FirmwareOfflineRxState::StoppedRejectedPacket,
            _ => FirmwareOfflineRxState::Recording,
        };
        self.records.push(FirmwareOfflineRxRecord {
            ordinal: self.records.len(),
            evidence,
        });
        Ok(())
    }
    pub fn disconnect(&mut self) {
        if self.state == FirmwareOfflineRxState::Recording {
            self.state = FirmwareOfflineRxState::StoppedDisconnect;
        }
    }
}
