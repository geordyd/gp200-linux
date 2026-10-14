//! Fresh whole-image recovery purpose, separate from captured-normal entry.
//!
//! No recovered journal, offset, prior chunk, historic approval or normal handoff
//! is used as execution authority. A fresh owner runs the shared completion
//! driver only after checking the actual local predecessor set and new source.
use crate::firmware_linux_recovery_admission::{
    CheckedRecoveryLineage, RecoveryHistoricalCapture, RecoveryPredecessorReference,
};
use crate::{
    LinuxCompletionInputs, LinuxCompletionMonitor, LinuxCompletionView,
    firmware_entry_study::{EntryStudyFilePin, read_pin},
    firmware_linux_completion_admission::{
        bad, executable_pin, private_directory, read_phase_signal,
    },
    firmware_service::OwnerStop,
    firmware_vendor_sequence::{VENDOR_IMAGE_SHA256, current_firmware_scope},
};
use gp200_domain::{
    DeviceSelection, FirmwareAttemptId, FirmwareReview, FirmwareUsbObservation, JobSha256,
    ServiceError, SessionEpoch,
};
use gp200_library::LibraryStore;
use serde::Serialize;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

pub(crate) const FRESH_RECOVERY_PURPOSE: &str = "fresh-full-image-recovery-v1";

/// Descriptive startup with the actual moved original store. No historical
/// approval, capture DTO or offset can authorize a physical write.
/// ```compile_fail
/// fn copy(x:gp200_transport::LinuxRecoveryRequest){let _=x.clone();}
/// ```
pub struct LinuxRecoveryRequest {
    pub(crate) image: FirmwareReview,
    pub(crate) inputs: LinuxCompletionInputs,
    pub(crate) selected: Option<FirmwareAttemptId>,
    pub(crate) editor_source_epoch: SessionEpoch,
    pub(crate) store: LibraryStore,
    pub(crate) monitor: LinuxCompletionMonitor,
    pub(crate) stop: Arc<OwnerStop>,
}
impl LinuxRecoveryRequest {
    pub fn new(
        image: FirmwareReview,
        inputs: LinuxCompletionInputs,
        selected: Option<FirmwareAttemptId>,
        editor_source_epoch: SessionEpoch,
        store: LibraryStore,
    ) -> (Self, LinuxCompletionMonitor) {
        let monitor = LinuxCompletionMonitor::default();
        (
            Self {
                image,
                inputs,
                selected,
                editor_source_epoch,
                store,
                monitor: monitor.clone(),
                stop: Arc::new(OwnerStop::default()),
            },
            monitor,
        )
    }
    pub fn into_store(self) -> LibraryStore {
        self.store
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LinuxRecoveryOutcome {
    pub view: LinuxCompletionView,
    pub selection: Option<DeviceSelection>,
    /// The genuine GUI local source epoch; never a normal MIDI retirement claim.
    pub editor_source_epoch: SessionEpoch,
    pub verified_normal_epoch: Option<SessionEpoch>,
    pub application_version: Option<String>,
    pub journal_path: String,
    pub editor_return_allowed: bool,
    pub history: Option<LinuxRecoveryHistoryReview>,
}

/// Produced only inside the actual fresh owner after checked local lineage,
/// current passive binding and retained canonical guards. It cannot be imported.
pub(crate) struct LinuxRecoveryAuthorization {
    pub(crate) image: FirmwareReview,
    pub(crate) inputs: LinuxCompletionInputs,
    pub(crate) lineage: CheckedRecoveryLineage,
    pub(crate) history: LinuxRecoveryHistoryReview,
    executable: EntryStudyFilePin,
    run_inode: u64,
    digest: String,
    deadline: Instant,
    live_successor: Option<FirmwareAttemptId>,
}
impl LinuxRecoveryAuthorization {
    pub(crate) fn from_fresh_owner(
        image: FirmwareReview,
        inputs: LinuxCompletionInputs,
        lineage: CheckedRecoveryLineage,
        anchor: &crate::firmware_linux_completion_binding::CompletionOwnedAnchor,
    ) -> Result<Self, ServiceError> {
        if !current_firmware_scope(
            &image.summary().source_sha256.hex(),
            &inputs.contract.sha256,
        ) || inputs.sources.is_empty()
            || inputs.sources.len() > 3000
            || !(30..=900).contains(&inputs.approval_wait_seconds)
            || !(5..=120).contains(&inputs.transition_seconds)
            || !(60..=3600).contains(&inputs.transfer_seconds)
        {
            return Err(bad(
                "Recovery exact image/contract/source or finite policy differs",
            ));
        }
        let original = lineage.original_binding();
        if !crate::firmware_linux_completion_binding::associated_identity(original, &anchor.binding)
            || !anchor
                .guard
                .as_ref()
                .is_some_and(|g| g.owns_normal_binding(&anchor.binding))
        {
            return Err(bad(
                "Recovery authorization lacks its fresh actual physical guard",
            ));
        }
        lineage.revalidate()?;
        let history = RecoveryPreparation::from_checked_lineage(lineage);
        let review = history.review().clone();
        let lineage = history.into_lineage();
        let executable = executable_pin()?;
        let run_inode = private_directory(&inputs.run_directory)?;
        let digest=JobSha256::of(&serde_json::to_vec(&serde_json::json!({"schema":1,"purpose":FRESH_RECOVERY_PURPOSE,"image_sha256":VENDOR_IMAGE_SHA256,"history":review,"anchor":anchor.binding,"inputs":inputs,"executable":executable})).map_err(bad)?).hex();
        let deadline = Instant::now() + Duration::from_secs(inputs.approval_wait_seconds);
        let result = Self {
            image,
            inputs,
            lineage,
            history: review,
            executable,
            run_inode,
            digest,
            deadline,
            live_successor: None,
        };
        result.revalidate()?;
        Ok(result)
    }
    pub(crate) fn retain_live_successor(&mut self, attempt: FirmwareAttemptId) {
        self.live_successor = Some(attempt);
    }
    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }
    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }
    pub(crate) fn revalidate(&self) -> Result<(), ServiceError> {
        if private_directory(&self.inputs.run_directory)? != self.run_inode
            || executable_pin()?.sha256 != self.executable.sha256
        {
            return Err(bad("Recovery executable/run directory differs"));
        }
        let mut paths = std::collections::BTreeSet::new();
        for pin in &self.inputs.sources {
            if !paths.insert(&pin.path) {
                return Err(bad("Duplicate recovery source path"));
            }
            read_pin(pin)?;
        }
        for pin in [
            &self.inputs.loss,
            &self.inputs.recovery,
            &self.inputs.vendor_guidance,
            &self.inputs.contract,
        ] {
            read_pin(pin)?;
        }
        self.lineage
            .revalidate_except_live(self.live_successor.as_ref())
    }
    pub(crate) fn fresh(&self) -> Result<(), ServiceError> {
        if Instant::now() >= self.deadline {
            return Err(bad("Fresh recovery preparation expired"));
        }
        self.revalidate()?;
        if Instant::now() >= self.deadline {
            return Err(bad(
                "Fresh recovery preparation expired during revalidation",
            ));
        }
        Ok(())
    }
    pub(crate) fn read_signal(&self, name: &str, expected: &str) -> Result<bool, ServiceError> {
        read_phase_signal(&self.inputs, self.run_inode, name, expected)
    }
}

/// Review projection only. It deliberately reports the absence of a fresh normal capture.
#[derive(Debug, Clone, Serialize)]
pub struct LinuxRecoveryHistoryReview {
    pub schema: u16,
    pub purpose: String,
    pub selected: FirmwareAttemptId,
    pub original_association: FirmwareUsbObservation,
    pub predecessors: Vec<RecoveryPredecessorReference>,
    pub source_reviews: Vec<String>,
    pub historical_captures: Vec<RecoveryHistoricalCapture>,
    pub starts_at_section: u8,
    pub starts_at_block: u64,
    pub requires_fresh_live_query: bool,
    pub requires_new_full_image_approval: bool,
    pub resume_supported: bool,
    pub physical_authority: bool,
    pub fresh_normal_backup_available: bool,
    pub installed_ir_audio_available: bool,
    pub limitations: String,
}

impl LinuxRecoveryHistoryReview {
    pub fn can_resume(&self) -> bool {
        false
    }
    pub fn permits_flash(&self) -> bool {
        false
    }
}

/// This private value records checked local history only. It owns no MIDI endpoint,
/// no store, no fresh reader and no authorization. It cannot create a normal handoff.
pub(crate) struct RecoveryPreparation {
    lineage: CheckedRecoveryLineage,
    review: LinuxRecoveryHistoryReview,
}

impl RecoveryPreparation {
    pub(crate) fn from_checked_lineage(lineage: CheckedRecoveryLineage) -> Self {
        let review = LinuxRecoveryHistoryReview {
            schema: 1,
            purpose: FRESH_RECOVERY_PURPOSE.into(),
            selected: lineage.selected().clone(),
            original_association: lineage.original_binding().clone(),
            predecessors: lineage.references(),
            source_reviews: lineage.source_reviews(),
            historical_captures: lineage.historical_captures(),
            starts_at_section: 0,
            starts_at_block: 0,
            requires_fresh_live_query: true,
            requires_new_full_image_approval: true,
            resume_supported: false,
            physical_authority: false,
            fresh_normal_backup_available: false,
            installed_ir_audio_available: false,
            limitations:
                "This is checked local history, not a live boot identity or permission to write. \
                A new process must acquire current canonical endpoint and physical guards, then \
                reobserve the current USB/ALSA binding. A fresh single identity query and a new \
                complete-image approval are required. Every section starts from zero; the old \
                offset, negotiated chunk and consumed approvals are ignored. No fresh normal \
                backup can be captured in boot-only mode; all installed IR audio remains \
                unavailable. Full-image recovery from the interrupted V1.6.3 to V1.8.2 path was tested on one GP-200; other devices and interruption cases remain unverified. \
                Host completion, fresh normal reads, a new pedal-display application-version \
                confirmation, linked durable quarantine closure and actual store/editor return \
                are distinct requirements. No retry, resume, reset or automatic clearance is \
                provided by this history review."
                    .into(),
        };
        Self { lineage, review }
    }
    pub(crate) fn review(&self) -> &LinuxRecoveryHistoryReview {
        &self.review
    }
    #[cfg(test)]
    pub(crate) fn revalidate(&self) -> Result<(), ServiceError> {
        self.lineage.revalidate()
    }
    pub(crate) fn into_lineage(self) -> CheckedRecoveryLineage {
        self.lineage
    }
}

#[cfg(test)]
#[path = "firmware_linux_recovery_tests.rs"]
mod tests;

/// Checks retained local lineage for display only. Every execution rechecks the
/// actual moved library store and acquires a new current boot binding separately.
pub fn review_linux_recovery_history(
    root: &std::path::Path,
    selected: Option<&FirmwareAttemptId>,
) -> Result<LinuxRecoveryHistoryReview, ServiceError> {
    let registry = crate::firmware_quarantine::QuarantineRegistry::system()?;
    let lineage = registry.checked_recovery(root, selected)?;
    Ok(RecoveryPreparation::from_checked_lineage(lineage)
        .review()
        .clone())
}

/// Retained local history projection. It contains no live executable capability.
#[derive(Debug, Clone, Serialize)]
pub struct LinuxFirmwareHistoryRow {
    pub attempt: FirmwareAttemptId,
    pub purpose: String,
    pub registry: crate::firmware_linux_recovery_admission::RecoveryFilePrefix,
    pub journal: crate::firmware_linux_recovery_admission::RecoveryFilePrefix,
    pub local_state: u8,
    pub resolved: bool,
    pub source_review_sha256: String,
    pub original_association: FirmwareUsbObservation,
    pub predecessors: Vec<RecoveryPredecessorReference>,
    pub historical_capture: Option<RecoveryHistoricalCapture>,
    pub closure_status: String,
    pub resume_supported: bool,
    pub physical_authority: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct LinuxFirmwareHistoryCatalogue {
    pub schema: u16,
    pub rows: Vec<LinuxFirmwareHistoryRow>,
    pub diagnostics: Vec<String>,
    pub resume_supported: bool,
    pub physical_authority: bool,
}
/// A bounded, secure local GP2LCJ01 history scan for display. Partial, missing or
/// corrupt history is reported conservatively and cannot issue a recovery permit.
pub fn review_linux_firmware_history(
    root: &std::path::Path,
) -> Result<LinuxFirmwareHistoryCatalogue, ServiceError> {
    crate::firmware_quarantine::QuarantineRegistry::system()?.history_catalogue(root)
}

/// Capless display of a securely checked local catalogue at an explicit private
/// registry directory. The physical executor always uses its canonical registry.
pub fn review_linux_firmware_history_at(
    root: &std::path::Path,
    registry_directory: &std::path::Path,
) -> Result<LinuxFirmwareHistoryCatalogue, ServiceError> {
    crate::firmware_linux_recovery_admission::catalogue_at(root, registry_directory)
}
