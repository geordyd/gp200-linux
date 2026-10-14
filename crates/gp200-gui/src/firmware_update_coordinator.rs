//! Genuine asynchronous coordinator. Render data never carries a service permit.
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
use crate::firmware_device_bridge::ReturnedLinuxOwner;
use crate::{
    firmware_device_bridge::{
        FirmwareLibraryEndpoint, FirmwareServiceEndpoint, RuntimeFirmwareLease,
    },
    firmware_workflow::{
        CoordinatorPreparationWorker, InspectedFirmware, LocalFirmwarePreparation,
        PreparationInput, PreparedLocalReview,
    },
    runtime::{RuntimeServices, StoreRestoration, StoreRestorationResult},
};
use gp200_app::{AppController, FirmwareEditorLease, FirmwareEditorSource};
use gp200_domain::*;
use gp200_library::*;
use gp200_transport::*;
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FirmwareCoordinatorPhase {
    #[default]
    Idle,
    Preparing,
    Capturing,
    Publishing,
    Revalidating,
    ReadOnlyReview,
    PreparingLocalReport,
    WaitingForStore,
    RestoringLibrary,
    ReturningNormal,
    RefreshingNormal,
    Owned,
    StopRequested,
    RecoveryRequired,
}
/// Normal-mode preparation must retain globals read in this connection.
/// Boot-only recovery deliberately does not use this admission condition.
pub(crate) fn normal_firmware_globals_current(state: &gp200_app::AppState) -> bool {
    state
        .device_globals
        .as_ref()
        .is_some_and(|globals| globals.epoch == state.epoch)
}

/// A bounded projection. Changing this value cannot acquire or release an owner.
#[derive(Debug, Clone, Default)]
pub struct FirmwareCoordinatorView {
    pub phase: FirmwareCoordinatorPhase,
    pub generation: u64,
    pub records: usize,
    pub blockers: Vec<String>,
    pub diagnostics: Vec<String>,
    pub omitted_diagnostics: usize,
    pub preparation: Option<LocalFirmwarePreparation>,
    pub capture: Option<FirmwareCaptureEvidenceSummary>,
    pub backup: Option<FirmwareBackupReviewSummary>,
    pub owner: Option<FirmwareOwnerSummary>,
    pub history: Option<FirmwareHistoryPage>,
    pub detail: Option<FirmwareHistoryDetail>,
    pub exported: Option<FirmwareHistoryExportReceipt>,
    pub can_return_to_editing: bool,
    pub can_retry_library_restore: bool,
    pub close_blocked: bool,
    pub permits_flash: bool,
    pub completion_verified: bool,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub can_review_linux_recovery: bool,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub can_close_settled_recovery_host: bool,
}
#[derive(Clone)]
struct PublishedFacts {
    proposal: ProposedFirmwareLossAllowance,
    evidence: FirmwareCaptureEvidenceSummary,
    archive: PathBuf,
    sidecar: PathBuf,
    index: PathBuf,
}
enum StorePurpose {
    ReturnNormal,
    Update,
    Recovery,
    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
    EntryStudy,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    LinuxCompletion,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    LinuxCompletionReturn,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    LinuxRecovery,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    LinuxRecoveryReturn,
}
/// Original startup/retirement observations are retained, not reclassified as success.
enum OwnerOriginal {
    Normal(Box<FirmwareNormalRetirementObservation>),
    Startup(Box<FirmwareStartupRetirementObservation>),
    Retired(FirmwareOwnerSummary),
}
#[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
enum EntryPreparationReply {
    Draft(Result<EntryStudyDraft, ServiceError>),
    Authorized(Result<EntryStudyAuthorization, ServiceError>),
}
#[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
struct EntryPreparation {
    received: std::sync::mpsc::Receiver<EntryPreparationReply>,
    thread: Option<std::thread::JoinHandle<()>>,
    generation: u64,
}
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
enum CompletionPreparationReply {
    Draft(Result<LinuxCompletionDraft, ServiceError>),
    Authorized(Result<LinuxCompletionAuthorization, ServiceError>),
    Signal(Result<(), ServiceError>),
    RecoveryInputs(
        Result<
            Box<(
                FirmwareReview,
                LinuxCompletionInputs,
                Option<FirmwareAttemptId>,
            )>,
            ServiceError,
        >,
    ),
    RecoveryHistory(Result<LinuxRecoveryHistoryReview, ServiceError>),
}
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
struct CompletionPreparation {
    received: std::sync::mpsc::Receiver<CompletionPreparationReply>,
    thread: Option<std::thread::JoinHandle<()>>,
    generation: u64,
}
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
struct LinuxHistoryRead {
    received: std::sync::mpsc::Receiver<Result<LinuxFirmwareHistoryCatalogue, ServiceError>>,
    thread: Option<std::thread::JoinHandle<()>>,
    root: PathBuf,
}
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
struct RecoveryHostReturn {
    owner: ReturnedLinuxOwner,
    generation: u64,
    source_epoch: SessionEpoch,
}
/// Presentation identity only. The allocation pins one installed monitor even
/// when a nonblocking read of its live projection is temporarily unavailable.
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
#[derive(Debug, Clone)]
pub(crate) struct CompletionPresentationOwner {
    pub(crate) generation: u64,
    pub(crate) run_directory: PathBuf,
    instance: Arc<()>,
}
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
impl PartialEq for CompletionPresentationOwner {
    fn eq(&self, other: &Self) -> bool {
        self.generation == other.generation
            && self.run_directory == other.run_directory
            && Arc::ptr_eq(&self.instance, &other.instance)
    }
}
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
impl Eq for CompletionPresentationOwner {}
pub(crate) struct FirmwareUpdateCoordinator {
    device: Option<FirmwareServiceEndpoint>,
    library: Option<FirmwareLibraryEndpoint>,
    editor: Option<FirmwareEditorLease>,
    runtime: Option<RuntimeFirmwareLease>,
    source: Option<FirmwareEditorSource>,
    selection: Option<DeviceSelection>,
    phase: FirmwareCoordinatorPhase,
    generation: u64,
    reservation: Option<FirmwareCaptureReservation>,
    control: Option<FirmwareBackupCaptureCommand>,
    action: Option<FirmwareBackupAction>,
    backup_request: Option<FirmwareBackupRequestAddress>,
    closed: Option<FirmwareCaptureReceiptDescription>,
    acquisition: Option<String>,
    receipt_capture_id: Option<String>,
    published: Option<Box<FirmwareCaptureDurableResult>>,
    facts: Option<PublishedFacts>,
    outcome: Option<BackupAcquisition>,
    conditional_consistency_observed: bool,
    capture_io_failed: bool,
    runtime_request: Option<LibraryFirmwareRequestAddress>,
    retirement: Option<LibraryStoreRetirement>,
    restoration: Option<StoreRestoration>,
    retained_store: Option<LibraryStore>,
    purpose: Option<StorePurpose>,
    return_boundary: Option<(SessionEpoch, SessionEpoch, DeviceSelection)>,
    library_restored: bool,
    owner_return_observed: bool,
    capture_store_returned: bool,
    capture_return_submitted: bool,
    capture_restore_retry: bool,
    refresh: VecDeque<DeviceCommand>,
    refresh_reads_requested: bool,
    worker: CoordinatorPreparationWorker,
    worker_cancel: Arc<AtomicBool>,
    prepared: Option<Arc<PreparedLocalReview>>,
    reviewed: Option<Arc<ReviewedFirmwareUpdate>>,
    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
    entry_preparation: Option<EntryPreparation>,
    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
    entry_draft: Option<EntryStudyDraft>,
    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
    entry_authorization: Option<EntryStudyAuthorization>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    completion_preparation: Option<CompletionPreparation>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    completion_draft: Option<LinuxCompletionDraft>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    completion_authorization: Option<LinuxCompletionAuthorization>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    completion_monitor: Option<LinuxCompletionMonitor>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    completion_monitor_instance: Option<Arc<()>>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    completion_run_directory: Option<PathBuf>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    completion_outcome: Option<LinuxCompletionOutcome>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    recovery_pending: Option<(
        FirmwareReview,
        LinuxCompletionInputs,
        Option<FirmwareAttemptId>,
    )>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    recovery_outcome: Option<LinuxRecoveryOutcome>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    recovery_history: Option<LinuxRecoveryHistoryReview>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    recovery_editor: bool,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    recovery_preparation_cancel_requested: Option<std::time::Instant>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    recovery_store_root: Option<PathBuf>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    recovery_host_return: Option<RecoveryHostReturn>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    linux_history_read: Option<LinuxHistoryRead>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    linux_history: Option<Arc<LinuxFirmwareHistoryCatalogue>>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    linux_history_attempted: bool,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    linux_history_error: Option<String>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    linux_history_selected: Option<FirmwareAttemptId>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    linux_history_offset: usize,
    // Parser-directory seam only; production always uses the canonical registry.
    #[cfg(all(test, feature = "firmware-linux-completion", target_os = "linux"))]
    linux_history_test_registry: Option<PathBuf>,
    // An admitted entry study cannot use the ordinary recovery Return path.
    entry_gate_held: bool,
    records: usize,
    admission_stopped: bool,
    offer_started: Option<std::time::Instant>,
    diagnostics: VecDeque<String>,
    omitted: usize,
    originals: VecDeque<FirmwareBackupCaptureEvent>,
    owner_originals: VecDeque<OwnerOriginal>,
    history: Option<FirmwareHistoryPage>,
    detail: Option<FirmwareHistoryDetail>,
    exported: Option<FirmwareHistoryExportReceipt>,
}
impl FirmwareUpdateCoordinator {
    pub(crate) fn new(services: &mut RuntimeServices) -> Self {
        Self {
            device: services.firmware_device.take(),
            library: services.firmware_library.take(),
            editor: None,
            runtime: None,
            source: None,
            selection: None,
            phase: FirmwareCoordinatorPhase::Idle,
            generation: 0,
            reservation: None,
            control: None,
            action: None,
            backup_request: None,
            closed: None,
            acquisition: None,
            receipt_capture_id: None,
            published: None,
            facts: None,
            outcome: None,
            conditional_consistency_observed: false,
            capture_io_failed: false,
            runtime_request: None,
            retirement: None,
            restoration: None,
            retained_store: None,
            purpose: None,
            return_boundary: None,
            library_restored: false,
            owner_return_observed: false,
            capture_store_returned: false,
            capture_return_submitted: false,
            capture_restore_retry: false,
            refresh: VecDeque::new(),
            refresh_reads_requested: false,
            worker: Default::default(),
            worker_cancel: Arc::new(AtomicBool::new(false)),
            prepared: None,
            reviewed: None,
            #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
            entry_preparation: None,
            #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
            entry_draft: None,
            #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
            entry_authorization: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            completion_preparation: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            completion_draft: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            completion_authorization: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            completion_monitor: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            completion_monitor_instance: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            completion_run_directory: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            completion_outcome: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            recovery_pending: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            recovery_outcome: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            recovery_history: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            recovery_editor: false,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            recovery_preparation_cancel_requested: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            recovery_store_root: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            recovery_host_return: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            linux_history_read: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            linux_history: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            linux_history_attempted: false,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            linux_history_error: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            linux_history_selected: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            linux_history_offset: 0,
            #[cfg(all(test, feature = "firmware-linux-completion", target_os = "linux"))]
            linux_history_test_registry: None,
            entry_gate_held: false,
            records: 0,
            admission_stopped: false,
            offer_started: None,
            diagnostics: VecDeque::new(),
            omitted: 0,
            originals: VecDeque::new(),
            owner_originals: VecDeque::new(),
            history: None,
            detail: None,
            exported: None,
        }
    }
    pub(crate) fn blocks_close(&self) -> bool {
        self.phase != FirmwareCoordinatorPhase::Idle
    }
    /// This permits explicit host exit only. It never releases normal admission,
    /// closes a device quarantine or upgrades a failed recovery into completion.
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn can_close_settled_recovery_host(
        &self,
        controller: &AppController,
        services: &RuntimeServices,
    ) -> bool {
        let (Some(returned), Some(source), Some(editor), Some(runtime), Some(root)) = (
            &self.recovery_host_return,
            &self.source,
            &self.editor,
            &self.runtime,
            &self.recovery_store_root,
        ) else {
            return false;
        };
        // Failed Linux owners deliberately invalidate the live source and keep
        // RecoveryRequired. Only these expected hold-state blockers are exempt;
        // unchanged exact documents and all pending local work are checked below.
        let held_readiness = controller
            .firmware_editor_readiness(editor)
            .is_ok_and(|ready| {
                ready.generation == self.generation
                    && ready
                        .blockers
                        .contains(&gp200_app::FirmwareEditorBlocker::RecoveryRequired)
                    && ready.blockers.iter().all(|blocker| {
                        matches!(
                            blocker,
                            gp200_app::FirmwareEditorBlocker::SourceChanged
                                | gp200_app::FirmwareEditorBlocker::RecoveryRequired
                        ) || (!self.recovery_editor
                            && matches!(
                                blocker,
                                gp200_app::FirmwareEditorBlocker::ConnectionNotReady
                                    | gp200_app::FirmwareEditorBlocker::IdentityMissing
                                    | gp200_app::FirmwareEditorBlocker::BufferMissing
                                    | gp200_app::FirmwareEditorBlocker::GlobalsMissing
                                    // Hold cleared the confirmed buffer. Exact
                                    // source documents and Saved are still required.
                                    | gp200_app::FirmwareEditorBlocker::DirtyDeviceDraft
                            ))
                    })
            });
        let exact_failed_outcome = if self.recovery_editor {
            self.recovery_outcome.as_ref().is_some_and(|outcome| {
                source.epoch == outcome.editor_source_epoch
                    && !outcome.editor_return_allowed
                    && outcome.view.phase == LinuxCompletionPhase::RecoveryRequired
                    && outcome.view.quarantined
                    && returned.owner.matches_recovery(outcome, root)
                    && self
                        .recovery_history
                        .as_ref()
                        .zip(outcome.history.as_ref())
                        .is_some_and(|(prepared, returned)| {
                            serde_json::to_value(prepared)
                                .ok()
                                .zip(serde_json::to_value(returned).ok())
                                .is_some_and(|(prepared, returned)| prepared == returned)
                        })
            }) && self.completion_draft.is_none()
                && self.completion_outcome.is_none()
                && self.reservation.is_none()
                && self.closed.is_none()
                && self.acquisition.is_none()
                && self.receipt_capture_id.is_none()
                && self.published.is_none()
                && self.facts.is_none()
                && self.outcome.is_none()
                && self.prepared.is_none()
                && self.reviewed.is_none()
                && self.originals.is_empty()
                && self.records == 0
        } else {
            // Captured evidence and reviewed inputs are retained immutable
            // records, not outstanding commands. Host exit must not erase them.
            self.completion_outcome.as_ref().is_some_and(|outcome| {
                source.epoch == outcome.original_normal_epoch
                    && !outcome.editor_return_allowed
                    && outcome.view.phase == LinuxCompletionPhase::RecoveryRequired
                    && outcome.view.quarantined
                    && returned.owner.matches_completion(outcome, root)
            }) && self.recovery_outcome.is_none()
                && self.recovery_history.is_none()
                && self
                    .prepared
                    .as_ref()
                    .is_none_or(|prepared| &prepared.source == source)
        };
        exact_failed_outcome
            && matches!(self.phase, FirmwareCoordinatorPhase::RecoveryRequired | FirmwareCoordinatorPhase::StopRequested)
            && self.entry_gate_held && self.admission_stopped && self.owner_return_observed && self.library_restored
            && matches!(self.purpose, Some(StorePurpose::Recovery))
            && returned.generation == self.generation && editor.generation() == self.generation
            && returned.source_epoch == source.epoch && returned.owner.root() == root
            && self.owner_originals.iter().any(|original| {
                matches!(original, OwnerOriginal::Retired(summary) if summary == &returned.owner.summary)
            })
            && controller.firmware_editor_bound_source(editor).is_ok_and(|bound| bound == source)
            && held_readiness && controller.state().epoch == source.epoch
            && controller.state().firmware_editor.phase == gp200_app::FirmwareEditorPhase::RecoveryRequired
            && controller.state().firmware_editor.source_changed
            && matches!(controller.state().connection, ConnectionState::Recovering | ConnectionState::Disconnected | ConnectionState::Offline)
            && controller.state().identity.is_none() && controller.state().confirmed_buffer.is_none()
            && controller.state().device_globals.is_none() && controller.state().device_job_session.is_none()
            && controller.recovery_session() == source.recovery
            && services.firmware_lease_matches(runtime) && services.firmware_queues_settled()
            && services.file_worker_settled()
            && self.device.as_ref().is_some_and(|device| !device.has_owner_ticket())
            && self.library.as_ref().is_some_and(|library| {
                &library.root == root && library.runtime.retirement_state() == LibraryStoreRetirementState::Running
            })
            && self.runtime_request.is_none() && self.retirement.is_none() && self.restoration.is_none()
            && self.retained_store.is_none() && self.completion_preparation.is_none() && self.recovery_pending.is_none()
            && self.recovery_preparation_cancel_requested.is_none() && self.linux_history_read.is_none()
            && self.no_entry_preparation() && self.completion_authorization.is_none() && self.return_boundary.is_none()
            && self.refresh.is_empty() && !self.refresh_reads_requested
            && self.control.is_none() && self.action.is_none() && self.backup_request.is_none()
            && !self.capture_store_returned && !self.capture_return_submitted && !self.capture_restore_retry
    }
    pub(crate) fn view(
        &self,
        controller: &AppController,
        services: &RuntimeServices,
    ) -> FirmwareCoordinatorView {
        let mut blockers = Vec::new();
        if self.device.is_none() {
            blockers.push("Live firmware capture requires the concrete MIDI service".into());
        }
        if self.library.is_none() {
            blockers.push("The original local library owner is unavailable".into());
        }
        if let Some(lease) = &self.editor {
            match controller.firmware_editor_readiness(lease) {
                Ok(ready) => blockers.extend(ready.blockers.iter().map(|v| format!("{v:?}"))),
                Err(e) => blockers.push(e.message),
            }
        }
        if !services.firmware_queues_settled() {
            blockers.push("Original queued runtime work is still settling".into());
        }
        blockers.push(
            self.device
                .as_ref()
                .map_or("Physical update is unavailable", |d| d.unavailable())
                .into(),
        );
        blockers.push("The Linux recovery procedure has not yet been validated".into());
        if self.runtime_request.is_some() {
            blockers.push("The original library request is awaiting its result".into());
        }
        if self.phase == FirmwareCoordinatorPhase::RecoveryRequired {
            blockers
                .push("The owner remains held for recovery; no automatic resume is allowed".into());
        }
        FirmwareCoordinatorView {
            phase: self.phase,
            generation: self.generation,
            records: self.records,
            blockers,
            diagnostics: self.diagnostics.iter().cloned().collect(),
            omitted_diagnostics: self.omitted,
            preparation: self.prepared.as_ref().map(|p| p.view.clone()),
            capture: self.facts.as_ref().map(|p| p.evidence.clone()),
            backup: self.facts.as_ref().map(|p| p.proposal.review.clone()),
            owner: self.device.as_ref().and_then(|d| d.summary()),
            history: self.history.clone(),
            detail: self.detail.clone(),
            exported: self.exported.clone(),
            can_return_to_editing: !self.entry_gate_held
                && !self.is_linux_recovery()
                && matches!(
                    self.phase,
                    FirmwareCoordinatorPhase::Preparing | FirmwareCoordinatorPhase::ReadOnlyReview
                )
                || self.can_continue_return(),
            can_retry_library_restore: self.retained_store.is_some()
                && self.restoration.is_none()
                && self.phase == FirmwareCoordinatorPhase::RecoveryRequired,
            close_blocked: self.phase != FirmwareCoordinatorPhase::Idle,
            permits_flash: false,
            completion_verified: false,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            can_review_linux_recovery: self.phase == FirmwareCoordinatorPhase::Idle
                && !matches!(
                    controller.state().connection,
                    ConnectionState::Ready | ConnectionState::Connecting
                )
                && self.device.is_some()
                && self.library.is_some()
                && self.runtime_request.is_none()
                && self.linux_history_read.is_none(),
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            can_close_settled_recovery_host: self
                .can_close_settled_recovery_host(controller, services),
        }
    }
    fn is_linux_recovery(&self) -> bool {
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        {
            self.recovery_editor
        }
        #[cfg(not(all(feature = "firmware-linux-completion", target_os = "linux")))]
        {
            false
        }
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn ensure_linux_history(&mut self) {
        if !self.linux_history_attempted
            && self.phase == FirmwareCoordinatorPhase::Idle
            && self.library.is_some()
        {
            let _ = self.refresh_linux_history();
        }
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn refresh_linux_history(&mut self) -> Result<(), ServiceError> {
        if self.phase != FirmwareCoordinatorPhase::Idle || self.linux_history_read.is_some() {
            return Err(busy("Wait for the current firmware owner or history read"));
        }
        let root = self
            .library
            .as_ref()
            .ok_or_else(|| invalid("The original library root is unavailable"))?
            .root
            .clone();
        self.linux_history = None;
        self.linux_history_error = None;
        self.linux_history_selected = None;
        self.linux_history_offset = 0;
        self.linux_history_attempted = true;
        let thread_root = root.clone();
        #[cfg(test)]
        let registry = self.linux_history_test_registry.clone();
        let (send, received) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("gp200-linux-history".into())
            .spawn(move || {
                #[cfg(test)]
                let result = match registry {
                    Some(registry) => review_linux_firmware_history_at(&thread_root, &registry),
                    None => review_linux_firmware_history(&thread_root),
                };
                #[cfg(not(test))]
                let result = review_linux_firmware_history(&thread_root);
                let _ = send.send(result);
            })
            .map_err(|error| invalid(error.to_string()))?;
        self.linux_history_read = Some(LinuxHistoryRead {
            received,
            thread: Some(thread),
            root,
        });
        Ok(())
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    fn poll_linux_history(&mut self) {
        let Some(read) = &self.linux_history_read else {
            return;
        };
        let result = match read.received.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err(invalid("Local firmware history reader stopped"))
            }
        };
        let mut read = self.linux_history_read.take().unwrap();
        if let Some(thread) = read.thread.take() {
            let _ = thread.join();
        }
        if self
            .library
            .as_ref()
            .is_none_or(|library| library.root != read.root)
        {
            self.linux_history_error = Some("The original library owner changed during the descriptive read; refresh history after its return".into());
            return;
        }
        match result {
            Ok(catalogue)
                if catalogue.schema == 1
                    && !catalogue.resume_supported
                    && !catalogue.physical_authority
                    && catalogue
                        .rows
                        .iter()
                        .all(|row| !row.resume_supported && !row.physical_authority) =>
            {
                self.linux_history = Some(Arc::new(catalogue));
            }
            Ok(_) => {
                self.linux_history_error =
                    Some("Unsupported local firmware history projection".into())
            }
            Err(error) => self.linux_history_error = Some(error.message),
        }
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn linux_history(&self) -> Option<Arc<LinuxFirmwareHistoryCatalogue>> {
        self.linux_history.clone()
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn linux_history_page_offset(&self) -> usize {
        self.linux_history_offset
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn change_linux_history_page(&mut self, forward: bool) {
        let total = self
            .linux_history
            .as_ref()
            .map_or(0, |catalogue| catalogue.rows.len());
        self.linux_history_offset = if forward && self.linux_history_offset + 32 < total {
            self.linux_history_offset + 32
        } else if !forward {
            self.linux_history_offset.saturating_sub(32)
        } else {
            self.linux_history_offset
        };
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn linux_history_status(&self) -> (bool, Option<String>, Option<FirmwareAttemptId>) {
        (
            self.linux_history_read.is_some(),
            self.linux_history_error.clone(),
            self.linux_history_selected.clone(),
        )
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn select_linux_history(
        &mut self,
        selected: Option<FirmwareAttemptId>,
    ) -> Result<(), ServiceError> {
        if self.phase != FirmwareCoordinatorPhase::Idle || self.linux_history_read.is_some() {
            return Err(busy(
                "Finish original firmware work before selecting local history",
            ));
        }
        if let Some(attempt) = &selected {
            let checked = self.linux_history.as_ref().is_some_and(|catalogue| {
                catalogue.diagnostics.is_empty()
                    && catalogue
                        .rows
                        .iter()
                        .any(|row| &row.attempt == attempt && !row.resolved)
            });
            if !checked {
                return Err(invalid(
                    "Only a checked unresolved local row can describe a new recovery",
                ));
            }
        }
        self.linux_history_selected = selected;
        Ok(())
    }
    #[cfg(all(test, feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn test_linux_history_registry(&mut self, registry: PathBuf) {
        self.linux_history_test_registry = Some(registry);
    }
    #[cfg(all(test, feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn test_completed_linux_history_fixture(
        &mut self,
        registry: &std::path::Path,
    ) -> Vec<FirmwareAttemptId> {
        // Disk-parser/display fixture only; no returned event, ticket, editor
        // lease, native factory or transfer approval is constructed here.
        let root = &self.library.as_ref().unwrap().root;
        firmware_update_coordinator_tests::write_completed_linux_history_fixture(root, registry)
    }
    #[cfg(all(test, feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn test_zero_call_linux_history_fixture(
        &mut self,
        registry: &std::path::Path,
    ) -> FirmwareAttemptId {
        let root = &self.library.as_ref().unwrap().root;
        firmware_update_coordinator_tests::write_zero_call_linux_history_fixture(root, registry)
    }
    pub(crate) fn begin(
        &mut self,
        controller: &mut AppController,
        services: &mut RuntimeServices,
        producers_settled: bool,
    ) -> Result<(), ServiceError> {
        if self.phase != FirmwareCoordinatorPhase::Idle || !producers_settled {
            return Err(busy("Finish original GUI work before preparing firmware"));
        }
        if self.device.is_none() || self.library.is_none() {
            return Err(ServiceError::unsupported(
                "Concrete MIDI and the original library worker are required",
            ));
        }
        if !normal_firmware_globals_current(controller.state()) {
            return Err(busy(
                "Read global settings in this connection before beginning firmware preparation",
            ));
        }
        let runtime = services.freeze_firmware()?;
        let editor = match controller.begin_firmware_editor_preparation() {
            Ok(v) => v,
            Err(e) => {
                let _ = services.release_firmware(&runtime);
                return Err(e);
            }
        };
        self.generation = editor.generation();
        self.source = Some(controller.firmware_editor_bound_source(&editor)?.clone());
        self.selection = controller
            .state()
            .device_job_session
            .as_ref()
            .map(|s| s.selection().clone());
        self.editor = Some(editor);
        self.runtime = Some(runtime);
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        {
            self.recovery_store_root = self.library.as_ref().map(|library| library.root.clone());
            self.recovery_host_return = None;
        }
        self.admission_stopped = false;
        self.offer_started = None;
        self.phase = FirmwareCoordinatorPhase::Preparing;
        Ok(())
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn begin_linux_recovery(
        &mut self,
        controller: &mut AppController,
        services: &mut RuntimeServices,
        producers_settled: bool,
        image: FirmwareReview,
        inputs: Option<LinuxCompletionInputs>,
        selected: Option<FirmwareAttemptId>,
    ) -> Result<(), ServiceError> {
        if self.phase != FirmwareCoordinatorPhase::Idle
            || !producers_settled
            || self.runtime_request.is_some()
            || self.linux_history_read.is_some()
            || self.device.is_none()
            || self.library.is_none()
        {
            return Err(busy(
                "Finish original editor, history and library work before reviewing recovery",
            ));
        }
        if matches!(
            controller.state().connection,
            ConnectionState::Ready | ConnectionState::Connecting
        ) {
            return Err(busy(
                "Recovery review requires a disconnected editor; an ordinary normal connection is still active",
            ));
        }
        if image.summary().model != DeviceModel::Gp200
            || image.summary().source_sha256.hex()
                != gp200_transport::firmware_vendor_sequence::VENDOR_IMAGE_SHA256
        {
            return Err(invalid(
                "Recovery requires the exact reviewed GP-200 V1.8.2 whole image",
            ));
        }
        let runtime = services.freeze_firmware()?;
        let editor = match controller.begin_firmware_recovery_preparation() {
            Ok(editor) => editor,
            Err(error) => {
                let _ = services.release_firmware(&runtime);
                return Err(error);
            }
        };
        self.generation = editor.generation();
        self.source = Some(controller.firmware_editor_bound_source(&editor)?.clone());
        self.editor = Some(editor);
        self.runtime = Some(runtime);
        self.recovery_editor = true;
        self.recovery_preparation_cancel_requested = None;
        self.recovery_store_root = self.library.as_ref().map(|library| library.root.clone());
        self.recovery_host_return = None;
        self.entry_gate_held = true;
        self.admission_stopped = false;
        self.completion_outcome = None;
        self.recovery_outcome = None;
        self.completion_monitor = None;
        self.completion_monitor_instance = None;
        self.phase = FirmwareCoordinatorPhase::Preparing;
        let (send, received) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("gp200-linux-recovery-review".into())
            .spawn(move || {
                let result = inputs
                    .map(Ok)
                    .unwrap_or_else(load_completion_resources)
                    .map(|inputs| Box::new((image, inputs, selected)));
                let _ = send.send(CompletionPreparationReply::RecoveryInputs(result));
            });
        match thread {
            Ok(thread) => {
                self.completion_preparation = Some(CompletionPreparation {
                    received,
                    thread: Some(thread),
                    generation: self.generation,
                })
            }
            Err(error) => {
                self.fail(controller, error.to_string());
                return Err(invalid(error.to_string()));
            }
        }
        Ok(())
    }

    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    fn poll_recovery_preparation(
        &mut self,
        controller: &mut AppController,
        services: &RuntimeServices,
        saved: Option<&RecoverySession>,
        producers_settled: bool,
    ) {
        if !self.recovery_editor
            || self.recovery_preparation_cancel_requested.is_some()
            || self.phase != FirmwareCoordinatorPhase::Preparing
            || self.recovery_pending.is_none()
            || !producers_settled
            || !services.firmware_queues_settled()
            || self.runtime_request.is_some()
        {
            return;
        }
        let (Some(source), Some(editor), Some(runtime)) =
            (&self.source, &self.editor, &self.runtime)
        else {
            return;
        };
        if saved != Some(&source.recovery) || !services.firmware_lease_matches(runtime) {
            return;
        }
        // The user's recovery-review action retains the exact local work excluded
        // from historical pedal material. Saved is the actual runtime reply.
        if controller
            .retain_firmware_local_draft_excluded(editor, &source.recovery)
            .is_err()
            || !controller
                .firmware_editor_readiness(editor)
                .is_ok_and(|ready| ready.locally_settled())
        {
            return;
        }
        if let Err(error) = controller.seal_firmware_recovery_preparation(editor, &source.recovery)
        {
            self.fail(controller, error.message);
            return;
        }
        if let Err(error) = self.begin_retirement(StorePurpose::LinuxRecovery) {
            self.fail(controller, error.message);
        }
    }

    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn linux_recovery_outcome(&self) -> Option<LinuxRecoveryOutcome> {
        self.recovery_outcome.clone()
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn linux_recovery_active(&self) -> bool {
        self.recovery_editor
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn linux_recovery_history(&self) -> Option<LinuxRecoveryHistoryReview> {
        self.recovery_history.clone()
    }

    pub(crate) fn refresh_source(
        &mut self,
        controller: &mut AppController,
        services: &RuntimeServices,
    ) -> Result<(), ServiceError> {
        if self.phase != FirmwareCoordinatorPhase::Preparing || !services.firmware_queues_settled()
        {
            return Err(busy("Original work has not settled"));
        }
        let lease = self
            .editor
            .as_mut()
            .ok_or_else(|| invalid("No original editor lease"))?;
        self.source = Some(controller.refresh_firmware_editor_preparation(lease)?);
        self.generation = lease.generation();
        self.selection = controller
            .state()
            .device_job_session
            .as_ref()
            .map(|s| s.selection().clone());
        Ok(())
    }
    pub(crate) fn retain_local_draft(
        &mut self,
        controller: &mut AppController,
        saved: Option<&RecoverySession>,
    ) -> Result<(), ServiceError> {
        let source = self
            .source
            .as_ref()
            .ok_or_else(|| invalid("No bound source"))?;
        if saved != Some(&source.recovery) {
            return Err(busy(
                "The exact local session has not received a genuine Saved acknowledgement",
            ));
        }
        controller.retain_firmware_local_draft_excluded(
            self.editor
                .as_ref()
                .ok_or_else(|| invalid("No editor lease"))?,
            &source.recovery,
        )
    }
    pub(crate) fn capture(
        &mut self,
        controller: &mut AppController,
        services: &RuntimeServices,
        saved: Option<&RecoverySession>,
        producers_settled: bool,
        context: FirmwareBackupReviewContext,
    ) -> Result<(), ServiceError> {
        if self.phase != FirmwareCoordinatorPhase::Preparing
            || self.is_linux_recovery()
            || self.runtime_request.is_some()
            || !producers_settled
            || !services.firmware_queues_settled()
        {
            return Err(busy("Original producers are still settling"));
        }
        context.validate()?;
        let lease = self
            .editor
            .as_ref()
            .ok_or_else(|| invalid("No editor lease"))?;
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| invalid("No runtime lease"))?;
        let source = self
            .source
            .as_ref()
            .ok_or_else(|| invalid("No bound source"))?;
        if !services.firmware_lease_matches(runtime)
            || controller.firmware_editor_bound_source(lease)? != source
            || saved != Some(&source.recovery)
            || !controller
                .firmware_editor_readiness(lease)?
                .locally_settled()
        {
            return Err(busy(
                "Exact source, runtime queues and genuine Saved recovery must settle before capture",
            ));
        }
        if context.device.model()
            != source
                .identity
                .as_ref()
                .map_or(String::new(), |v| v.model.name())
        {
            return Err(invalid(
                "Capture device association differs from the original source",
            ));
        }
        controller.seal_firmware_editor_preparation(lease, &source.recovery)?;
        self.control = Some(FirmwareBackupCaptureCommand::Begin(context));
        self.phase = FirmwareCoordinatorPhase::Capturing;
        self.submit_control(controller);
        Ok(())
    }
    pub(crate) fn prepare(
        &mut self,
        controller: &AppController,
        image: InspectedFirmware,
    ) -> Result<(), ServiceError> {
        if self.phase != FirmwareCoordinatorPhase::ReadOnlyReview {
            return Err(busy("A genuine accepted read-only capture is required"));
        }
        let facts = self
            .facts
            .as_ref()
            .ok_or_else(|| invalid("No genuine publication facts"))?;
        if image.review.summary().source_sha256.hex()
            != facts.proposal.review.context.destination.image_sha256
            || image.review.summary().model.name()
                != facts.proposal.review.context.destination.model
            || image
                .review
                .summary()
                .image_version
                .as_deref()
                .and_then(|v| v.parse::<FirmwareVersion>().ok())
                != Some(facts.proposal.review.context.destination.version)
        {
            return Err(invalid("Image differs from the captured destination"));
        }
        let source = self
            .source
            .clone()
            .ok_or_else(|| invalid("No original source"))?;
        let lease = self
            .editor
            .as_ref()
            .ok_or_else(|| invalid("No editor lease"))?;
        if controller.firmware_editor_bound_source(lease)? != &source {
            return Err(invalid("Source generation changed"));
        }
        self.worker_cancel = Arc::new(AtomicBool::new(false));
        let input = PreparationInput {
            generation: self.generation,
            image,
            source,
            local_draft_excluded: controller.state().firmware_editor.local_draft_excluded,
            proposal: facts.proposal.clone(),
            evidence: facts.evidence.clone(),
            archive: facts.archive.clone(),
            sidecar: facts.sidecar.clone(),
            index: facts.index.clone(),
        };
        self.worker
            .submit(input, self.worker_cancel.clone())
            .map_err(invalid)?;
        self.prepared = None;
        self.phase = FirmwareCoordinatorPhase::PreparingLocalReport;
        Ok(())
    }
    pub(crate) fn request_update(&mut self) -> Result<(), ServiceError> {
        let device = self
            .device
            .as_ref()
            .ok_or_else(|| ServiceError::unsupported("Concrete MIDI service unavailable"))?;
        // This availability check precedes relinquishment and every ownership request.
        let reason = device.unavailable();
        if !reason.is_empty() {
            return Err(ServiceError::new(ErrorKind::Unverified, reason));
        }
        if self.phase != FirmwareCoordinatorPhase::ReadOnlyReview || self.reviewed.is_none() {
            return Err(invalid("No fully reviewed genuine update is available"));
        }
        self.begin_retirement(StorePurpose::Update)
    }
    pub(crate) fn return_normal(
        &mut self,
        controller: &mut AppController,
        services: &mut RuntimeServices,
    ) -> Result<(), ServiceError> {
        if self.entry_gate_held {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Entry study keeps Normal controls held; local library return is not safe Normal/recovery evidence",
            ));
        }
        if self.can_continue_return() {
            if matches!(self.purpose, Some(StorePurpose::ReturnNormal)) {
                // This choice continues the original restoration failure once.
                // A later failed Connect/read cannot reuse the consumed return
                // boundary or present the same choice as a fresh continuation.
                self.capture_restore_retry = false;
                // Continue only the original return control after the same store
                // was joined and restored. A fresh boundary must still come from
                // its actual NormalRefreshRequired receipt, including a receipt
                // that arrived while the restoration failure held admission.
                self.admission_stopped = false;
                self.offer_started = None;
                self.phase = FirmwareCoordinatorPhase::ReturningNormal;
                if let Some(index) = self.originals.iter().position(|event| {
                    matches!(
                        event,
                        FirmwareBackupCaptureEvent::NormalRefreshRequired { .. }
                    )
                }) {
                    let original = self
                        .originals
                        .remove(index)
                        .expect("observed original receipt");
                    self.capture_event(original, controller, None, false, services);
                }
                return Ok(());
            }
            let source = self
                .source
                .as_ref()
                .ok_or_else(|| invalid("No original source lease"))?;
            let selection = self
                .selection
                .clone()
                .ok_or_else(|| invalid("The original owner selection is unavailable"))?;
            let minimum = controller
                .state()
                .epoch
                .0
                .max(source.epoch.0)
                .checked_add(1)
                .map(SessionEpoch)
                .ok_or_else(|| invalid("Fresh epoch is exhausted"))?;
            self.return_boundary = Some((source.epoch, minimum, selection));
            self.admission_stopped = false;
            self.offer_started = None;
            self.phase = FirmwareCoordinatorPhase::ReturningNormal;
            return Ok(());
        }
        if self.phase == FirmwareCoordinatorPhase::Preparing {
            if !services.firmware_queues_settled() {
                return Err(busy("Original results must settle first"));
            }
            let lease = self
                .editor
                .as_ref()
                .ok_or_else(|| invalid("No editor lease"))?;
            services.release_firmware_with(
                self.runtime
                    .as_ref()
                    .ok_or_else(|| invalid("No runtime lease"))?,
                || controller.cancel_firmware_editor_preparation(lease),
            )?;
            self.clear_after_return();
            return Ok(());
        }
        if self.phase != FirmwareCoordinatorPhase::ReadOnlyReview {
            return Err(busy(
                "Return requires an accepted capture review; failed capture remains held",
            ));
        }
        self.begin_retirement(StorePurpose::ReturnNormal)
    }
    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
    pub(crate) fn prepare_entry_study(
        &mut self,
        inputs: EntryStudyInputs,
    ) -> Result<(), ServiceError> {
        if self.phase != FirmwareCoordinatorPhase::ReadOnlyReview
            || self.entry_preparation.is_some()
            || self.entry_draft.is_some()
            || self.entry_gate_held
        {
            return Err(busy(
                "Entry preparation requires this same-run settled capture review",
            ));
        }
        let prepared = self
            .prepared
            .clone()
            .ok_or_else(|| invalid("Prepare the genuine local firmware report first"))?;
        let facts = self
            .facts
            .clone()
            .ok_or_else(|| invalid("Actual captured publication absent"))?;
        let reservation = self
            .reservation
            .clone()
            .ok_or_else(|| invalid("Actual normal capture reservation absent"))?;
        // ReadOnlyReview requires the actual transport's private acceptance.
        // Its conditional consistency does not make the strict archive equal.
        if !facts.evidence.closed || prepared.source.epoch != reservation.epoch {
            return Err(invalid("Closed current source capture boundary differs"));
        }
        let (send, received) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("gp200-entry-review".into())
            .spawn(move || {
                let _ = send.send(EntryPreparationReply::Draft(prepare_entry_draft(
                    prepared,
                    facts,
                    reservation,
                    inputs,
                )));
            })
            .map_err(|e| invalid(e.to_string()))?;
        self.entry_preparation = Some(EntryPreparation {
            received,
            thread: Some(thread),
            generation: self.generation,
        });
        Ok(())
    }
    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
    pub(crate) fn entry_study_review(&self) -> Option<EntryStudyReview> {
        self.entry_draft.as_ref().map(|d| d.review().clone())
    }
    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
    pub(crate) fn execute_entry_study(&mut self, signal: String) -> Result<(), ServiceError> {
        if self.phase != FirmwareCoordinatorPhase::ReadOnlyReview
            || self.entry_preparation.is_some()
            || self.entry_gate_held
        {
            return Err(busy("Same-run entry draft is not settled"));
        }
        let draft = self
            .entry_draft
            .take()
            .ok_or_else(|| invalid("No exact same-run entry review"))?;
        let (send, received) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("gp200-entry-exact-signal".into())
            .spawn(move || {
                let _ = send.send(EntryPreparationReply::Authorized(
                    draft.approve_exact(&signal),
                ));
            })
            .map_err(|e| invalid(e.to_string()))?;
        self.entry_preparation = Some(EntryPreparation {
            received,
            thread: Some(thread),
            generation: self.generation,
        });
        Ok(())
    }
    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
    fn poll_entry_preparation(&mut self, controller: &mut AppController) {
        let Some(worker) = self.entry_preparation.as_mut() else {
            return;
        };
        if worker.thread.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        let result = worker.received.try_recv();
        let generation = worker.generation;
        if let Some(thread) = worker.thread.take() {
            let _ = thread.join();
        }
        self.entry_preparation = None;
        if generation != self.generation
            || self.phase != FirmwareCoordinatorPhase::ReadOnlyReview
            || self.entry_gate_held
        {
            self.note("Stale entry helper result discarded without an offer".into());
            return;
        }
        match result {
            Ok(EntryPreparationReply::Draft(Ok(draft))) => self.entry_draft = Some(draft),
            Ok(EntryPreparationReply::Authorized(Ok(authorization))) => {
                self.entry_authorization = Some(authorization);
                if let Err(e) = self.begin_retirement(StorePurpose::EntryStudy) {
                    self.entry_authorization = None;
                    self.note(e.message);
                }
            }
            Ok(
                EntryPreparationReply::Draft(Err(e)) | EntryPreparationReply::Authorized(Err(e)),
            ) => self.note(e.message),
            Err(_) => self.fail(
                controller,
                "Entry preparation worker ended without its original result".into(),
            ),
        }
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn prepare_linux_completion(
        &mut self,
        inputs: Option<LinuxCompletionInputs>,
    ) -> Result<(), ServiceError> {
        if self.phase != FirmwareCoordinatorPhase::ReadOnlyReview
            || self.completion_preparation.is_some()
            || self.completion_draft.is_some()
            || self.entry_preparation.is_some()
            || self.entry_draft.is_some()
            || self.entry_gate_held
        {
            return Err(busy(
                "Completion preparation requires this same-run settled captured review",
            ));
        }
        let prepared = self
            .prepared
            .clone()
            .ok_or_else(|| invalid("Prepare the local firmware report first"))?;
        let facts = self
            .facts
            .clone()
            .ok_or_else(|| invalid("Genuine captured publication absent"))?;
        let reservation = self
            .reservation
            .clone()
            .ok_or_else(|| invalid("Genuine normal reservation absent"))?;
        if !facts.evidence.closed || prepared.source.epoch != reservation.epoch {
            return Err(invalid("Closed capture differs from current editor source"));
        }
        self.completion_outcome = None;
        self.completion_monitor = None;
        self.completion_monitor_instance = None;
        let (send, received) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("gp200-linux-update-review".into())
            .spawn(move || {
                let result = (|| {
                    let inputs = match inputs {
                        Some(inputs) => inputs,
                        None => load_completion_resources()?,
                    };
                    let draft = prepare_completion_draft(prepared, facts, reservation, inputs)?;
                    publish_completion_review(&draft)?;
                    Ok(draft)
                })();
                let _ = send.send(CompletionPreparationReply::Draft(result));
            })
            .map_err(|e| invalid(e.to_string()))?;
        self.completion_preparation = Some(CompletionPreparation {
            received,
            thread: Some(thread),
            generation: self.generation,
        });
        Ok(())
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn linux_completion_review(&self) -> Option<LinuxCompletionReview> {
        self.completion_draft
            .as_ref()
            .map(|draft| draft.review().clone())
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn linux_completion_display(&self) -> Option<(String, serde_json::Value)> {
        let review = self.completion_draft.as_ref()?.review();
        Some((
            review.reservation.usb.topology.clone(),
            completion_display_facts(review),
        ))
    }

    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn linux_completion_digest(&self) -> Option<String> {
        self.completion_draft.as_ref().map(|draft| draft.digest())
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn linux_completion_view(&self) -> Option<LinuxCompletionView> {
        self.completion_monitor
            .as_ref()
            .and_then(|m| m.view())
            .or_else(|| {
                self.recovery_outcome
                    .as_ref()
                    .map(|outcome| outcome.view.clone())
            })
            .or_else(|| {
                self.completion_outcome
                    .as_ref()
                    .map(|outcome| outcome.view.clone())
            })
    }
    /// Read at most once per UI frame. Returned outcomes supersede the last
    /// monitor projection; only a current owned monitor receives an identity.
    /// Neither this metadata nor a retained UI projection grants an action.
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn linux_completion_presentation(
        &self,
    ) -> (
        Option<CompletionPresentationOwner>,
        Option<LinuxCompletionView>,
    ) {
        if let Some(outcome) = &self.recovery_outcome {
            return (None, Some(outcome.view.clone()));
        }
        if let Some(outcome) = &self.completion_outcome {
            return (None, Some(outcome.view.clone()));
        }
        let monitor = self.completion_monitor.as_ref();
        let owner = if self.phase == FirmwareCoordinatorPhase::Owned && monitor.is_some() {
            self.completion_monitor_instance
                .as_ref()
                .zip(self.completion_run_directory.as_ref())
                .map(|(instance, directory)| CompletionPresentationOwner {
                    generation: self.generation,
                    run_directory: directory.clone(),
                    instance: instance.clone(),
                })
        } else {
            None
        };
        (owner, monitor.and_then(LinuxCompletionMonitor::view))
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    fn install_completion_monitor(&mut self, monitor: LinuxCompletionMonitor) {
        self.completion_monitor = Some(monitor);
        self.completion_monitor_instance = Some(Arc::new(()));
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn linux_completion_outcome(&self) -> Option<LinuxCompletionOutcome> {
        self.completion_outcome.clone()
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn linux_completion_helper_pending(&self) -> bool {
        self.completion_preparation.is_some()
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn execute_linux_completion(&mut self, signal: String) -> Result<(), ServiceError> {
        if self.phase != FirmwareCoordinatorPhase::ReadOnlyReview
            || self.completion_preparation.is_some()
            || self.entry_gate_held
        {
            return Err(busy("Same-run completion review is not settled"));
        }
        let draft = self
            .completion_draft
            .take()
            .ok_or_else(|| invalid("No exact completion review"))?;
        self.completion_run_directory = Some(draft.review().inputs.run_directory.clone());
        let (send, received) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("gp200-linux-entry-confirmation".into())
            .spawn(move || {
                let _ = send.send(CompletionPreparationReply::Authorized(
                    draft.approve_exact(&signal),
                ));
            })
            .map_err(|e| invalid(e.to_string()))?;
        self.completion_preparation = Some(CompletionPreparation {
            received,
            thread: Some(thread),
            generation: self.generation,
        });
        Ok(())
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn approve_linux_completion_phase(
        &mut self,
        phase: LinuxCompletionPhase,
        digest: String,
        manual: Option<String>,
    ) -> Result<(), ServiceError> {
        if self.completion_preparation.is_some() || self.phase != FirmwareCoordinatorPhase::Owned {
            return Err(busy(
                "The live completion owner is not ready for a phase confirmation",
            ));
        }
        let view = self
            .completion_monitor
            .as_ref()
            .and_then(|m| m.view())
            .ok_or_else(|| invalid("Actual completion owner absent"))?;
        if view.phase != phase || view.review_digest.as_deref() != Some(digest.as_str()) {
            return Err(invalid(
                "Displayed review differs from this live owner phase",
            ));
        }
        let (name, signal) = match phase {
            LinuxCompletionPhase::ReviewBootQuery => {
                ("approve-query", format!("execute-query:{digest}"))
            }
            LinuxCompletionPhase::ReviewTransfer => {
                ("approve-transfer", format!("execute-transfer:{digest}"))
            }
            LinuxCompletionPhase::ConfirmApplicationVersion => {
                let text =
                    manual.ok_or_else(|| invalid("Read the version on the pedal display now"))?;
                let version: FirmwareVersion = text
                    .trim()
                    .parse()
                    .map_err(|e: FirmwareDescriptionError| invalid(e.to_string()))?;
                if version.to_string() != "1.8.2" {
                    return Err(invalid(
                        "The pedal display must freshly report V1.8.2; no automatic version inference is available",
                    ));
                }
                (
                    "confirm-application-version",
                    format!("confirm-application:{digest}:V1.8.2"),
                )
            }
            _ => return Err(invalid("This live phase has no user confirmation")),
        };
        let directory = self
            .completion_run_directory
            .clone()
            .ok_or_else(|| invalid("Live internal review directory absent"))?;
        let (send, received) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("gp200-linux-phase-confirmation".into())
            .spawn(move || {
                let _ = send.send(CompletionPreparationReply::Signal(
                    publish_completion_signal(&directory, name, &signal),
                ));
            })
            .map_err(|e| invalid(e.to_string()))?;
        self.completion_preparation = Some(CompletionPreparation {
            received,
            thread: Some(thread),
            generation: self.generation,
        });
        Ok(())
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    fn poll_completion_preparation(
        &mut self,
        controller: &mut AppController,
        services: &RuntimeServices,
    ) {
        let Some(worker) = self.completion_preparation.as_mut() else {
            return;
        };
        if worker
            .thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
        {
            return;
        }
        let reply = worker.received.try_recv();
        let generation = worker.generation;
        if let Some(thread) = worker.thread.take() {
            let _ = thread.join();
        }
        self.completion_preparation = None;
        if generation != self.generation {
            self.note("Stale Linux update helper discarded without authority".into());
            return;
        }
        if self.recovery_preparation_cancel_requested.is_some() {
            // The helper only loads local resources before ownership. Its
            // original thread has now joined; neither a result nor an error
            // may turn a cancelled preparation into retirement or authority.
            self.note("Original recovery resource helper joined after cancellation; its result carries no authority".into());
            return;
        }
        match reply {
            Ok(CompletionPreparationReply::Draft(Ok(draft)))
                if self.phase == FirmwareCoordinatorPhase::ReadOnlyReview
                    && !self.entry_gate_held =>
            {
                self.completion_draft = Some(draft)
            }
            Ok(CompletionPreparationReply::Authorized(Ok(auth)))
                if self.phase == FirmwareCoordinatorPhase::ReadOnlyReview
                    && !self.entry_gate_held =>
            {
                self.completion_authorization = Some(auth);
                if let Err(e) = self.begin_retirement(StorePurpose::LinuxCompletion) {
                    self.completion_authorization = None;
                    self.note(e.message);
                }
            }
            Ok(CompletionPreparationReply::RecoveryInputs(Ok(payload)))
                if self.recovery_editor && self.phase == FirmwareCoordinatorPhase::Preparing =>
            {
                let (image, inputs, selected) = *payload;
                self.completion_run_directory = Some(inputs.run_directory.clone());
                self.recovery_pending = Some((image, inputs, selected));
            }
            Ok(CompletionPreparationReply::RecoveryInputs(Err(error))) => {
                self.fail(controller, error.message)
            }
            Ok(CompletionPreparationReply::RecoveryHistory(Ok(history)))
                if self.recovery_editor
                    && matches!(self.purpose, Some(StorePurpose::LinuxRecovery))
                    && !self.admission_stopped =>
            {
                self.recovery_history = Some(history);
                self.submit_checked_recovery(controller, services);
            }
            Ok(CompletionPreparationReply::RecoveryHistory(Err(error))) => {
                if let Some(store) = self.retained_store.take() {
                    self.restoration = Some(services.restore_library(store));
                }
                self.purpose = Some(StorePurpose::Recovery);
                self.fail(controller, error.message);
            }
            Ok(CompletionPreparationReply::RecoveryHistory(Ok(_))) => {
                if let Some(store) = self.retained_store.take() {
                    self.restoration = Some(services.restore_library(store));
                }
                self.purpose = Some(StorePurpose::Recovery);
                self.fail(controller, "Recovery history check finished after Stop; exact store restored without submission".into());
            }
            Ok(CompletionPreparationReply::Signal(Ok(()))) => {}
            Ok(
                CompletionPreparationReply::Draft(Err(e))
                | CompletionPreparationReply::Authorized(Err(e))
                | CompletionPreparationReply::Signal(Err(e)),
            ) => self.note(e.message),
            Ok(_) => self.note("Linux helper result rejected after Stop or phase change".into()),
            Err(_) => self.fail(
                controller,
                "Linux update helper lost its original result".into(),
            ),
        }
    }
    fn can_continue_return(&self) -> bool {
        !self.entry_gate_held
            && self.phase == FirmwareCoordinatorPhase::RecoveryRequired
            && self.library_restored
            && self.restoration.is_none()
            && (matches!(self.purpose, Some(StorePurpose::Recovery)) && self.owner_return_observed
                || matches!(self.purpose, Some(StorePurpose::ReturnNormal))
                    && self.capture_restore_retry
                    && self.capture_store_returned
                    && (self.capture_return_submitted
                        || matches!(
                            self.control,
                            Some(FirmwareBackupCaptureCommand::ReturnToNormal)
                        )))
    }
    fn begin_retirement(&mut self, purpose: StorePurpose) -> Result<(), ServiceError> {
        if self.runtime_request.is_some()
            || self.backup_request.is_some()
            || self.action.is_some()
            || self.control.is_some()
            || self.published.is_some()
        {
            return Err(busy(
                "Original durable or history results are still pending",
            ));
        }
        let library = self
            .library
            .as_mut()
            .ok_or_else(|| invalid("No actual library runtime client"))?;
        let address = library
            .runtime
            .try_request(LibraryFirmwareAction::RelinquishStore)
            .map_err(|_| busy("Library did not accept relinquishment; ownership is unchanged"))?;
        self.runtime_request = Some(address);
        self.purpose = Some(purpose);
        self.phase = FirmwareCoordinatorPhase::WaitingForStore;
        Ok(())
    }
    pub(crate) fn history(&mut self, action: LibraryFirmwareAction) -> Result<(), ServiceError> {
        if matches!(action, LibraryFirmwareAction::RelinquishStore) {
            return Err(invalid("History cannot retire a store"));
        }
        if !(matches!(
            self.phase,
            FirmwareCoordinatorPhase::Idle
                | FirmwareCoordinatorPhase::Preparing
                | FirmwareCoordinatorPhase::ReadOnlyReview
        ) || self.entry_gate_held
            && self.library_restored
            && self.phase == FirmwareCoordinatorPhase::RecoveryRequired)
            || self.runtime_request.is_some()
        {
            return Err(busy(
                "Original library history request is pending or ownership is changing",
            ));
        }
        if let LibraryFirmwareAction::ListHistory { limit, .. } = &action
            && (*limit == 0 || *limit > 32)
        {
            return Err(invalid("History page limit must be 1–32"));
        }
        self.runtime_request = Some(
            self.library
                .as_mut()
                .ok_or_else(|| invalid("Original library runtime unavailable"))?
                .runtime
                .try_request(action)
                .map_err(|_| busy("History was not offered; retry explicitly"))?,
        );
        Ok(())
    }
    pub(crate) fn stop(&mut self, controller: &mut AppController) {
        if self.phase == FirmwareCoordinatorPhase::Idle {
            return;
        }
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        if self.recovery_preparation_cancel_requested.is_some() {
            // Repeated Stop must not convert this same unsealed barrier into
            // RecoveryRequired while its original local helper is finishing.
            return;
        } else if self.recovery_pre_owner_preparation(controller) {
            self.recovery_preparation_cancel_requested = Some(std::time::Instant::now());
            self.admission_stopped = true;
            self.phase = FirmwareCoordinatorPhase::StopRequested;
            self.note("Cancellation requested before sealing or ownership; waiting for the original local helper and runtime work".into());
            return;
        }
        self.worker_cancel.store(true, Ordering::Release);
        if let Some(d) = &self.device {
            if !self.is_linux_recovery() {
                d.capture.cancel();
            }
            d.stop();
        }
        if self.phase != FirmwareCoordinatorPhase::Idle {
            self.fail(controller, "Stop requested; reader, output and library retirement still require original observations".into());
            self.phase = FirmwareCoordinatorPhase::StopRequested;
        }
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    fn recovery_pre_owner_preparation(&self, controller: &AppController) -> bool {
        self.recovery_editor
            && matches!(
                self.phase,
                FirmwareCoordinatorPhase::Preparing | FirmwareCoordinatorPhase::StopRequested
            )
            && matches!(
                controller.state().connection,
                ConnectionState::Disconnected | ConnectionState::Offline
            )
            && controller.state().identity.is_none()
            && controller.state().device_job_session.is_none()
            && controller.state().firmware_editor.phase == gp200_app::FirmwareEditorPhase::Preparing
            && self
                .editor
                .as_ref()
                .is_some_and(|lease| controller.firmware_editor_bound_source(lease).is_ok())
            && self.runtime.is_some()
            && self.source.is_some()
            && self.selection.is_none()
            && self
                .device
                .as_ref()
                .is_some_and(|device| !device.has_owner_ticket() && device.summary().is_none())
            && self.library.as_ref().is_some_and(|library| {
                // Unavailable is only a contended observation, never proof of
                // an intact writer. It permits waiting; release below still
                // requires an actual Running observation within the bound.
                matches!(
                    library.runtime.retirement_state(),
                    LibraryStoreRetirementState::Running | LibraryStoreRetirementState::Unavailable
                )
            })
            && self.runtime_request.is_none()
            && self.retirement.is_none()
            && self.restoration.is_none()
            && self.retained_store.is_none()
            && self.purpose.is_none()
            && self.reservation.is_none()
            && self.control.is_none()
            && self.action.is_none()
            && self.backup_request.is_none()
            && self.closed.is_none()
            && self.acquisition.is_none()
            && self.receipt_capture_id.is_none()
            && self.published.is_none()
            && self.facts.is_none()
            && self.outcome.is_none()
            && self.prepared.is_none()
            && self.reviewed.is_none()
            && self.originals.is_empty()
            && self.owner_originals.is_empty()
            && self.records == 0
            && self.return_boundary.is_none()
            && self.refresh.is_empty()
            && !self.refresh_reads_requested
            && !self.library_restored
            && !self.owner_return_observed
            && !self.capture_store_returned
            && !self.capture_return_submitted
            && !self.capture_restore_retry
            && self.completion_draft.is_none()
            && self.completion_authorization.is_none()
            && self.completion_monitor.is_none()
            && self.completion_outcome.is_none()
            && self.recovery_outcome.is_none()
            && self.recovery_history.is_none()
            && self.linux_history_read.is_none()
            && self.no_entry_preparation()
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    fn no_entry_preparation(&self) -> bool {
        #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
        {
            self.entry_preparation.is_none()
                && self.entry_draft.is_none()
                && self.entry_authorization.is_none()
        }
        #[cfg(not(all(feature = "firmware-entry-study", target_os = "linux")))]
        {
            true
        }
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    fn poll_recovery_preparation_cancel(
        &mut self,
        controller: &mut AppController,
        services: &RuntimeServices,
        producers_settled: bool,
    ) {
        let Some(started) = self.recovery_preparation_cancel_requested else {
            return;
        };
        if started.elapsed() >= std::time::Duration::from_secs(10) {
            self.recovery_preparation_cancel_requested = None;
            self.fail(controller, "Local recovery cancellation could not observe every original helper and runtime boundary within 10 seconds; admission remains held".into());
            return;
        }
        if !self.recovery_pre_owner_preparation(controller) {
            self.recovery_preparation_cancel_requested = None;
            self.fail(controller, "Recovery preparation cancellation remains held because an original ownership or editor boundary changed".into());
            return;
        }
        let settled = self.completion_preparation.is_none()
            && producers_settled
            && services.firmware_queues_settled()
            && self.library.as_ref().is_some_and(|library| {
                library.runtime.retirement_state() == LibraryStoreRetirementState::Running
            })
            && self
                .runtime
                .as_ref()
                .is_some_and(|lease| services.firmware_lease_matches(lease));
        if settled {
            let result = services.release_firmware_with(
                self.runtime
                    .as_ref()
                    .expect("checked original runtime lease"),
                || {
                    controller.cancel_firmware_editor_preparation(
                        self.editor.as_ref().expect("checked unsealed editor lease"),
                    )
                },
            );
            if result.is_ok() {
                // This gate belonged only to the cancelled local preparation.
                // No quarantine, journal, device state or normal connection is
                // changed; physical recovery remains unresolved.
                self.entry_gate_held = false;
                self.completion_run_directory = None;
                self.admission_stopped = false;
                self.clear_after_return();
                self.note("Unsealed local recovery preparation cancelled after the original helper joined and runtime work settled; the disconnected device remains unresolved".into());
            }
        }
    }
    pub(crate) fn retry_store_restore(
        &mut self,
        services: &RuntimeServices,
    ) -> Result<(), ServiceError> {
        if self.restoration.is_some() || self.phase != FirmwareCoordinatorPhase::RecoveryRequired {
            return Err(busy(
                "Original ownership preparation or restoration is still pending",
            ));
        }
        let store = self
            .retained_store
            .take()
            .ok_or_else(|| invalid("No genuine returned store is retained"))?;
        self.restoration = Some(services.restore_library(store));
        self.phase = FirmwareCoordinatorPhase::RestoringLibrary;
        Ok(())
    }
    pub(crate) fn poll(
        &mut self,
        controller: &mut AppController,
        services: &mut RuntimeServices,
        saved: Option<&RecoverySession>,
        producers_settled: bool,
    ) {
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        self.poll_linux_history();
        #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
        self.poll_entry_preparation(controller);
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        self.poll_completion_preparation(controller, services);
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        self.poll_recovery_preparation_cancel(controller, services, producers_settled);
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        self.poll_recovery_preparation(controller, services, saved, producers_settled);
        if let Some(reply) = self.worker.poll() {
            if reply.generation != self.generation
                || self.phase != FirmwareCoordinatorPhase::PreparingLocalReport
            {
                self.note("Stale original local material reply retained as a diagnostic".into());
            } else {
                match reply.result {
                    Ok(prepared)
                        if Some(&prepared.source) == self.source.as_ref()
                            && prepared.image.review.summary().source_sha256.hex()
                                == self.facts.as_ref().map_or(String::new(), |f| {
                                    f.proposal.review.context.destination.image_sha256.clone()
                                }) =>
                    {
                        self.prepared = Some(prepared);
                        self.phase = FirmwareCoordinatorPhase::ReadOnlyReview;
                    }
                    Ok(_) => self.fail(
                        controller,
                        "Prepared source or immutable image differs from the bound generation"
                            .into(),
                    ),
                    Err(e) => {
                        self.note(e);
                        self.phase = FirmwareCoordinatorPhase::ReadOnlyReview;
                    }
                }
            }
        }
        self.poll_owner(controller, services);
        self.poll_runtime(controller);
        self.poll_retirement(controller, services);
        self.poll_restore(controller, services);
        self.poll_backup(controller);
        self.consume_publication(controller);
        self.submit_action(controller);
        self.submit_control(controller);
        if self.admission_stopped
            && let Some(original) = self
                .device
                .as_ref()
                .and_then(|d| d.capture.try_next_event())
        {
            self.retain_original(original);
        }
        // One capture event per frame; each action must receive its genuine receipt.
        if !self.admission_stopped
            && self.action.is_none()
            && self.backup_request.is_none()
            && self.control.is_none()
            && self.published.is_none()
            && let Some(event) = self
                .device
                .as_ref()
                .and_then(|d| d.capture.try_next_event())
        {
            self.capture_event(event, controller, saved, producers_settled, services);
        }
        self.submit_action(controller);
        self.submit_control(controller);
        self.observe_terminal_rails(controller);
        self.poll_normal_refresh(controller, services, producers_settled);
    }
    fn capture_event(
        &mut self,
        event: FirmwareBackupCaptureEvent,
        controller: &mut AppController,
        saved: Option<&RecoverySession>,
        producers_settled: bool,
        services: &RuntimeServices,
    ) {
        match &event {
            FirmwareBackupCaptureEvent::Prepared { reservation, .. } => {
                if self.phase != FirmwareCoordinatorPhase::Capturing
                    || self.reservation.is_some()
                    || self
                        .source
                        .as_ref()
                        .is_none_or(|s| s.epoch != reservation.epoch)
                {
                    self.retain_original(event);
                    self.fail(
                        controller,
                        "Original capture reservation does not match this source epoch".into(),
                    );
                    return;
                }
                self.reservation = Some(reservation.clone());
            }
            FirmwareBackupCaptureEvent::Observation {
                evidence,
                consistency,
                ..
            } => {
                if evidence.validate().is_err()
                    || self.reservation.is_none()
                    || self.records >= FIRMWARE_CAPTURE_RECORD_LIMIT
                {
                    self.retain_original(event);
                    self.fail(
                        controller,
                        "Original capture record is invalid or exceeds its bound".into(),
                    );
                    return;
                }
                self.records += 1;
                if consistency
                    .as_ref()
                    .is_some_and(|c| c.admitted && !c.strict_raw_equal)
                {
                    self.conditional_consistency_observed = true;
                }
            }
            FirmwareBackupCaptureEvent::Failed {
                error,
                evidence: Some(_),
            } => {
                self.capture_io_failed = true;
                self.note(error.message.clone());
            }
            FirmwareBackupCaptureEvent::CloseReady { outcome, .. } => {
                self.outcome = Some(outcome.clone())
            }
            FirmwareBackupCaptureEvent::Closed { reservation } => {
                if self.reservation.as_ref() != Some(reservation) || self.closed.is_none() {
                    self.retain_original(event);
                    self.fail(
                        controller,
                        "Close does not match the actual durable boundary".into(),
                    );
                    return;
                }
                self.action = Some(FirmwareBackupAction::PublishCaptured);
                self.phase = FirmwareCoordinatorPhase::Publishing;
                return;
            }
            FirmwareBackupCaptureEvent::ReviewAccepted { reservation } => {
                let valid = self.reservation.as_ref() == Some(reservation)
                    && self.facts.is_some()
                    && self.reviewable_capture_outcome()
                    && self
                        .source
                        .as_ref()
                        .is_some_and(|s| saved == Some(&s.recovery))
                    && producers_settled
                    && services.firmware_queues_settled();
                if !valid {
                    self.retain_original(event);
                    self.fail(controller, "Accepted capture has no matching original durable review or local Saved session".into());
                    return;
                }
                self.phase = FirmwareCoordinatorPhase::ReadOnlyReview;
                if self.conditional_consistency_observed {
                    self.note("Original admitted conditional consistency retained for review; strict archival Failed/cancelled/partial status remains unchanged".into());
                }
                return;
            }
            FirmwareBackupCaptureEvent::NormalRefreshRequired {
                retired_epoch,
                next_epoch,
                selection,
            } => {
                if !matches!(self.purpose, Some(StorePurpose::ReturnNormal))
                    || !self.capture_store_returned
                    || !self.capture_return_submitted
                    || self
                        .source
                        .as_ref()
                        .is_none_or(|s| s.epoch != *retired_epoch)
                {
                    self.retain_original(event);
                    self.fail(
                        controller,
                        "Original normal retirement differs from this lease".into(),
                    );
                    return;
                }
                let Some(selection) = selection.clone() else {
                    self.retain_original(event);
                    self.fail(
                        controller,
                        "The retired owner did not return its actual selection".into(),
                    );
                    return;
                };
                let Some(fresh_minimum) = next_epoch.0.checked_add(1).map(SessionEpoch) else {
                    self.retain_original(event);
                    self.fail(controller, "Fresh normal epoch is exhausted".into());
                    return;
                };
                self.return_boundary = Some((*retired_epoch, fresh_minimum, selection));
                return;
            }
            _ => {
                self.retain_original(event);
                self.fail(
                    controller,
                    "Original capture stopped, was rejected, revoked or became uncertain".into(),
                );
                return;
            }
        }
        match event.into_library_action() {
            Ok(action) => self.action = Some(action),
            Err(e) => self.fail(controller, e.message),
        }
    }
    fn submit_action(&mut self, controller: &mut AppController) {
        if self.admission_stopped || !self.offer_within_bound(controller) {
            return;
        }
        let Some(action) = self.action.take() else {
            return;
        };
        let Some(library) = &mut self.library else {
            self.action = Some(action);
            self.fail(
                controller,
                "The genuine archival client is unavailable".into(),
            );
            return;
        };
        match library.backup.try_submit(action) {
            Ok(address) => {
                self.offer_started = None;
                self.backup_request = Some(address);
            }
            Err(SubmitError::Busy(action)) => self.action = Some(action),
            Err(SubmitError::Disconnected(action)) => {
                self.action = Some(action);
                self.fail(
                    controller,
                    "Original archival action was not accepted; writer stopped".into(),
                );
            }
        }
    }
    fn submit_control(&mut self, controller: &mut AppController) {
        if self.admission_stopped || !self.offer_within_bound(controller) {
            return;
        }
        let Some(command) = self.control.take() else {
            return;
        };
        let Some(device) = &self.device else {
            self.control = Some(command);
            self.fail(
                controller,
                "The genuine capture client is unavailable".into(),
            );
            return;
        };
        let returning = matches!(command, FirmwareBackupCaptureCommand::ReturnToNormal);
        match device.capture.try_submit(command) {
            Ok(()) => {
                self.offer_started = None;
                if returning {
                    self.capture_return_submitted = true;
                }
            }
            Err(SubmitError::Busy(command)) => self.control = Some(command),
            Err(SubmitError::Disconnected(command)) => {
                self.control = Some(command);
                self.fail(
                    controller,
                    "Original capture control was not accepted; actor stopped".into(),
                );
            }
        }
    }
    fn observe_terminal_rails(&mut self, controller: &mut AppController) {
        if self.admission_stopped || self.phase == FirmwareCoordinatorPhase::Idle {
            return;
        }
        let failure = self
            .library
            .as_ref()
            .and_then(|l| {
                let state = l.runtime.retirement_state();
                let correlated_retirement = self.purpose.is_some()
                    && (self.retirement.is_some()
                        || self.runtime_request.as_ref().is_some_and(|r| {
                            matches!(r.action, LibraryFirmwareActionAddress::RelinquishStore)
                        }))
                    && matches!(
                        state,
                        LibraryStoreRetirementState::Retiring
                            | LibraryStoreRetirementState::Returned
                    );
                l.runtime
                    .terminal_failure()
                    .or_else(|| {
                        (!correlated_retirement)
                            .then(|| l.backup.terminal_failure())
                            .flatten()
                    })
                    .or_else(|| {
                        matches!(
                            state,
                            LibraryStoreRetirementState::Unresolved
                                | LibraryStoreRetirementState::Stopped
                        )
                        .then(|| {
                            "The genuine writer exited without a returnable owner/result".into()
                        })
                    })
            })
            .or_else(|| {
                self.device
                    .as_ref()
                    .and_then(|d| {
                        d.capture.terminal_failure().map(|e| e.message).or_else(|| {
                            d.stopped().then(|| {
                                "The original MIDI worker terminated; accepted work and ownership remain unresolved".into()
                            })
                        })
                    })
            });
        if let Some(reason) = failure {
            self.fail(controller, reason);
        }
    }
    fn poll_backup(&mut self, controller: &mut AppController) {
        let Some(event) = self
            .library
            .as_mut()
            .and_then(|l| l.backup.try_next_event())
        else {
            return;
        };
        if self.backup_request.take().as_ref() != Some(&event.address) {
            self.fail(
                controller,
                "Original archival address differs from the sole pending action".into(),
            );
            return;
        }
        let result = match event.result {
            Ok(FirmwareBackupResult::CaptureDurable(v)) => v,
            Ok(_) => {
                self.fail(
                    controller,
                    "Archival result is not a genuine capture receipt".into(),
                );
                return;
            }
            Err(e) => {
                self.fail(controller, e.message);
                return;
            }
        };
        let description = result.receipt.description();
        let expected_action = FirmwareBackupActionAddress::Capture {
            kind: description.kind,
            read: description.read.clone(),
            observation_sequence: description.observation_sequence,
        };
        if description.worker_run != event.address.run
            || description.worker_sequence != event.address.sequence
            || !(description.acquisition == event.address.acquisition
                && description.acquisition == self.acquisition
                || description.kind == FirmwareCaptureReceiptKind::Begun
                    && event.address.acquisition.is_none()
                    && self.acquisition.is_none()
                    && description
                        .acquisition
                        .as_ref()
                        .is_some_and(|v| !v.is_empty() && v.len() <= 256))
            || self
                .receipt_capture_id
                .as_ref()
                .is_some_and(|id| id != &description.capture_id)
            || expected_action != event.address.action
            || self.reservation.as_ref() != Some(&description.reservation)
        {
            self.fail(
                controller,
                "Genuine receipt run, sequence, original action or reservation differs".into(),
            );
            return;
        }
        self.acquisition = description.acquisition.clone();
        self.receipt_capture_id = Some(description.capture_id.clone());
        match description.kind {
            FirmwareCaptureReceiptKind::Closed => {
                self.closed = Some(description.clone());
                self.control = Some(FirmwareBackupCaptureCommand::CloseAfterBoundary(
                    result.receipt,
                ));
            }
            FirmwareCaptureReceiptKind::Published => {
                self.published = Some(result);
            }
            FirmwareCaptureReceiptKind::Revalidated => {
                if !self.publication_matches(
                    description,
                    FirmwareCaptureReceiptKind::Revalidated,
                    2,
                ) || result.publication.is_none()
                {
                    self.fail(
                        controller,
                        "Revalidated receipt differs from original Closed + 2 publication".into(),
                    );
                    return;
                }
                let publication = result
                    .publication
                    .as_ref()
                    .expect("checked original publication");
                let facts = PublishedFacts {
                    proposal: publication.archive().proposal(),
                    evidence: publication.summary().clone(),
                    archive: publication.archive().archive_path().to_owned(),
                    sidecar: publication.archive().sidecar_path().to_owned(),
                    index: publication.index_path().to_owned(),
                };
                self.facts = Some(facts);
                if !self.reviewable_capture_outcome() {
                    self.fail(controller, "Original failed or cancelled acquisition was published for recovery; it cannot be accepted or resumed".into());
                    return;
                }
                self.control = Some(FirmwareBackupCaptureCommand::AcceptRevalidatedReview(
                    *result,
                ));
            }
            _ => {
                self.control = Some(
                    FirmwareBackupCaptureCommand::ContinueAfterDurableObservation(result.receipt),
                )
            }
        }
    }
    fn reviewable_capture_outcome(&self) -> bool {
        !self.capture_io_failed
            && !self.admission_stopped
            && (self.outcome == Some(BackupAcquisition::Finished)
                || self.conditional_consistency_observed
                    && matches!(self.outcome, Some(BackupAcquisition::Failed { .. })))
    }
    fn publication_matches(
        &self,
        description: &FirmwareCaptureReceiptDescription,
        kind: FirmwareCaptureReceiptKind,
        delta: u64,
    ) -> bool {
        let Some(closed) = &self.closed else {
            return false;
        };
        description.kind == kind
            && closed.worker_sequence.checked_add(delta) == Some(description.worker_sequence)
            && description.worker_run == closed.worker_run
            && description.capture_id == closed.capture_id
            && description.acquisition == closed.acquisition
            && description.reservation == closed.reservation
            && description.read.is_none()
            && description.observation_sequence.is_none()
            && description.raw_sha256 == JobSha256::of(&[]).hex()
            && self
                .facts
                .as_ref()
                .is_none_or(|f| description.evidence_sha256 == f.evidence.index_sha256)
    }
    fn consume_publication(&mut self, controller: &mut AppController) {
        let Some(result) = self.published.take() else {
            return;
        };
        if !self.publication_matches(
            result.receipt.description(),
            FirmwareCaptureReceiptKind::Published,
            1,
        ) || result.publication.is_none()
        {
            self.fail(
                controller,
                "Published receipt differs from original Closed + 1 publication".into(),
            );
            return;
        }
        let mut expected = self.closed.as_ref().expect("checked closed").clone();
        expected.worker_sequence += 1;
        expected.kind = FirmwareCaptureReceiptKind::Published;
        expected.raw_sha256 = JobSha256::of(&[]).hex();
        expected.evidence_sha256 = result
            .publication
            .as_ref()
            .expect("checked publication")
            .summary()
            .index_sha256
            .clone();
        let publication = result
            .publication
            .as_ref()
            .expect("checked genuine publication");
        self.facts = Some(PublishedFacts {
            proposal: publication.archive().proposal(),
            evidence: publication.summary().clone(),
            archive: publication.archive().archive_path().to_owned(),
            sidecar: publication.archive().sidecar_path().to_owned(),
            index: publication.index_path().to_owned(),
        });
        match result.receipt.try_consume_for_review(&expected) {
            Ok(()) => {
                self.action = Some(FirmwareBackupAction::RevalidateCaptured(
                    result.publication.expect("checked publication"),
                ));
                self.phase = FirmwareCoordinatorPhase::Revalidating;
            }
            Err(FirmwareBackupReceiptError::Busy(original)) => {
                self.published = Some(Box::new(FirmwareCaptureDurableResult {
                    receipt: *original,
                    publication: result.publication,
                }));
            }
            Err(FirmwareBackupReceiptError::Rejected(e)) => self.fail(controller, e.message),
        }
    }
    fn poll_runtime(&mut self, controller: &mut AppController) {
        let Some(event) = self
            .library
            .as_mut()
            .and_then(|l| l.runtime.try_next_event())
        else {
            return;
        };
        if self.runtime_request.take().as_ref() != Some(&event.address) {
            self.fail(
                controller,
                "Original library runtime address differs from its pending request".into(),
            );
            return;
        }
        match event.result {
            Ok(LibraryFirmwareResult::Retirement(handle))
                if matches!(
                    event.address.action,
                    LibraryFirmwareActionAddress::RelinquishStore
                ) =>
            {
                self.retirement = Some(handle)
            }
            Ok(LibraryFirmwareResult::Blocked { reasons })
                if matches!(
                    event.address.action,
                    LibraryFirmwareActionAddress::RelinquishStore
                ) =>
            {
                for reason in reasons.into_iter().take(32) {
                    self.note(reason);
                }
                self.purpose = None;
                self.phase = FirmwareCoordinatorPhase::ReadOnlyReview;
            }
            Ok(LibraryFirmwareResult::HistoryPage(page))
                if matches!(
                    event.address.action,
                    LibraryFirmwareActionAddress::ListHistory { .. }
                ) =>
            {
                self.history = Some(page)
            }
            Ok(LibraryFirmwareResult::HistoryDetail(detail))
                if matches!(
                    event.address.action,
                    LibraryFirmwareActionAddress::InspectAttempt { .. }
                ) =>
            {
                self.detail = Some(*detail)
            }
            Ok(LibraryFirmwareResult::Exported(receipt))
                if matches!(
                    event.address.action,
                    LibraryFirmwareActionAddress::ExportAttempt { .. }
                ) =>
            {
                self.exported = Some(receipt)
            }
            Ok(_) => self.fail(
                controller,
                "Original runtime result does not match the requested action".into(),
            ),
            Err(e) => {
                self.note(e.message);
                if self.purpose.is_some() {
                    self.fail(controller, "The genuine library retirement failed".into());
                }
            }
        }
    }
    fn poll_retirement(&mut self, controller: &mut AppController, services: &mut RuntimeServices) {
        let Some(result) = self.retirement.as_mut().and_then(|r| r.try_take_result()) else {
            return;
        };
        self.retirement = None;
        let store = match result {
            Ok(s) => s,
            Err(e) => {
                self.fail(controller, e.message);
                return;
            }
        };
        // The real writer has joined. Drop the revoked ordinary clients only now.
        services.retire_library_clients();
        self.library = None;
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        if matches!(self.purpose, Some(StorePurpose::LinuxRecovery)) {
            let root = store.root().to_path_buf();
            let selected = self
                .recovery_pending
                .as_ref()
                .and_then(|pending| pending.2.clone());
            self.retained_store = Some(store);
            if self.admission_stopped {
                self.restoration = self
                    .retained_store
                    .take()
                    .map(|store| services.restore_library(store));
                self.purpose = Some(StorePurpose::Recovery);
                self.fail(
                    controller,
                    "Recovery stopped before owner submission; returning the exact unoffered store"
                        .into(),
                );
                return;
            }
            let (send, received) = std::sync::mpsc::sync_channel(1);
            match std::thread::Builder::new()
                .name("gp200-linux-recovery-history".into())
                .spawn(move || {
                    let result = review_linux_recovery_history(&root, selected.as_ref());
                    let _ = send.send(CompletionPreparationReply::RecoveryHistory(result));
                }) {
                Ok(thread) => {
                    self.completion_preparation = Some(CompletionPreparation {
                        received,
                        thread: Some(thread),
                        generation: self.generation,
                    })
                }
                Err(error) => {
                    self.restoration = self
                        .retained_store
                        .take()
                        .map(|store| services.restore_library(store));
                    self.purpose = Some(StorePurpose::Recovery);
                    self.fail(controller, error.to_string());
                }
            }
            return;
        }
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        if matches!(self.purpose, Some(StorePurpose::LinuxCompletion)) {
            let Some(authorization) = self.completion_authorization.take() else {
                self.retained_store = Some(store);
                self.fail(controller, "Exact completion authorization lost".into());
                return;
            };
            let (request, monitor) =
                FirmwareOwnershipRequest::linux_completion(authorization, store);
            match self
                .device
                .as_mut()
                .expect("concrete owner")
                .submit_completion(request)
            {
                Ok(()) => {
                    self.install_completion_monitor(monitor);
                    self.entry_gate_held = true;
                    self.phase = FirmwareCoordinatorPhase::Owned;
                }
                Err(error) => {
                    self.retained_store = Some(error.into_command().into_store());
                    self.owner_return_observed = true;
                    self.purpose = Some(StorePurpose::Recovery);
                    self.fail(controller,"Completion request was not accepted; exact store retained without automatic normal return".into());
                }
            }
            return;
        }
        #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
        if matches!(self.purpose, Some(StorePurpose::EntryStudy)) {
            let Some(authorization) = self.entry_authorization.take() else {
                self.retained_store = Some(store);
                self.fail(
                    controller,
                    "Exact original entry authorization was lost".into(),
                );
                return;
            };
            let request = FirmwareOwnershipRequest::entry_study(authorization, store);
            match self
                .device
                .as_mut()
                .expect("concrete owner")
                .submit(request)
            {
                Ok(()) => {
                    self.entry_gate_held = true;
                    self.phase = FirmwareCoordinatorPhase::Owned;
                }
                Err(error) => {
                    self.retained_store = Some(error.into_command().into_store());
                    self.owner_return_observed = true;
                    self.purpose = Some(StorePurpose::Recovery);
                    self.fail(controller,"Entry request was not accepted; exact store retained and no automatic Normal resume".into());
                }
            }
            return;
        }
        if matches!(self.purpose, Some(StorePurpose::Update)) {
            let Some(job) = self.reviewed.clone() else {
                self.retained_store = Some(store);
                self.fail(
                    controller,
                    "No genuine reviewed update remains after retirement".into(),
                );
                return;
            };
            let request = FirmwareOwnershipRequest::new(job, store);
            match self
                .device
                .as_mut()
                .expect("concrete owner checked before retirement")
                .submit(request)
            {
                Ok(()) => self.phase = FirmwareCoordinatorPhase::Owned,
                Err(error) => {
                    self.retained_store = Some(error.into_command().into_store());
                    self.owner_return_observed = true;
                    self.purpose = Some(StorePurpose::Recovery);
                    self.fail(
                        controller,
                        "Original firmware request was not accepted; the exact store is retained"
                            .into(),
                    );
                }
            }
        } else {
            if matches!(self.purpose, Some(StorePurpose::ReturnNormal)) {
                self.capture_store_returned = true;
            }
            self.restoration = Some(services.restore_library(store));
            self.phase = FirmwareCoordinatorPhase::RestoringLibrary;
            if matches!(self.purpose, Some(StorePurpose::ReturnNormal)) {
                self.control = Some(FirmwareBackupCaptureCommand::ReturnToNormal);
            }
        }
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    fn submit_checked_recovery(
        &mut self,
        controller: &mut AppController,
        services: &RuntimeServices,
    ) {
        let Some(store) = self.retained_store.take() else {
            self.fail(controller, "Original retired recovery store lost".into());
            return;
        };
        let Some((image, inputs, selected)) = self.recovery_pending.take() else {
            self.restoration = Some(services.restore_library(store));
            self.purpose = Some(StorePurpose::Recovery);
            self.fail(controller, "Original recovery preparation lost".into());
            return;
        };
        let Some(source_epoch) = self.source.as_ref().map(|source| source.epoch) else {
            self.restoration = Some(services.restore_library(store));
            self.purpose = Some(StorePurpose::Recovery);
            self.fail(controller, "Original editor binding lost".into());
            return;
        };
        let (request, monitor) =
            LinuxRecoveryRequest::new(image, inputs, selected, source_epoch, store);
        match self
            .device
            .as_mut()
            .expect("concrete recovery endpoint")
            .submit_recovery(request)
        {
            Ok(()) => {
                self.install_completion_monitor(monitor);
                self.phase = FirmwareCoordinatorPhase::Owned;
            }
            Err(error) => {
                self.restoration =
                    Some(services.restore_library(error.into_command().into_store()));
                self.owner_return_observed = true;
                self.purpose = Some(StorePurpose::Recovery);
                self.fail(controller, "Recovery review was not accepted; restoring the exact unoffered store while the editor remains held".into());
            }
        }
    }
    fn poll_restore(&mut self, controller: &mut AppController, services: &mut RuntimeServices) {
        let Some(result) = self.restoration.as_mut().and_then(|r| r.try_take()) else {
            return;
        };
        self.restoration = None;
        match result {
            StoreRestorationResult::Restored(bundle) => {
                services.install_library_bundle(bundle);
                self.library = services.firmware_library.take();
                self.library_restored = true;
                if self.normal_return_purpose() && !self.admission_stopped {
                    self.phase = FirmwareCoordinatorPhase::ReturningNormal;
                } else if self.admission_stopped
                    || matches!(self.purpose, Some(StorePurpose::Recovery))
                {
                    self.phase = FirmwareCoordinatorPhase::RecoveryRequired;
                }
            }
            StoreRestorationResult::RetainedFailure { error, store } => {
                self.retained_store = Some(store);
                if matches!(self.purpose, Some(StorePurpose::ReturnNormal))
                    && self.capture_store_returned
                {
                    self.capture_restore_retry = true;
                }
                self.fail(controller, error.message);
            }
            StoreRestorationResult::Unresolved(error) => self.fail(controller, error.message),
        }
    }
    fn poll_owner(&mut self, controller: &mut AppController, services: &RuntimeServices) {
        let Some(event) = self.device.as_mut().and_then(|d| d.poll()) else {
            return;
        };
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        let event =
            match event {
                FirmwareOwnerEvent::LinuxRecoveryReturned {
                    summary,
                    store,
                    outcome,
                } => {
                    let proof = self
                        .device
                        .as_mut()
                        .and_then(|device| device.take_linux_return());
                    let returned = proof
                        .filter(|proof| {
                            self.recovery_editor
                                && matches!(self.purpose, Some(StorePurpose::LinuxRecovery))
                                && self.source.as_ref().is_some_and(|source| {
                                    source.epoch == outcome.editor_source_epoch
                                })
                                && self.editor.as_ref().is_some_and(|editor| {
                                    editor.generation() == self.generation
                                        && controller
                                            .firmware_editor_bound_source(editor)
                                            .is_ok_and(|bound| Some(bound) == self.source.as_ref())
                                })
                                && self
                                    .runtime
                                    .as_ref()
                                    .is_some_and(|lease| services.firmware_lease_matches(lease))
                                && self.recovery_store_root.as_deref() == Some(store.root())
                                && proof.summary == summary
                                && proof.matches_recovery(&outcome, store.root())
                        })
                        .map(|owner| RecoveryHostReturn {
                            owner,
                            generation: self.generation,
                            source_epoch: outcome.editor_source_epoch,
                        });
                    self.accept_recovery_return(summary, store, *outcome, controller, services);
                    self.recovery_host_return = returned;
                    return;
                }
                FirmwareOwnerEvent::LinuxCompletionReturned {
                    summary,
                    store,
                    outcome,
                } => {
                    let proof = self
                        .device
                        .as_mut()
                        .and_then(|device| device.take_linux_return());
                    let returned = proof
                        .filter(|proof| {
                            !self.recovery_editor
                                && matches!(self.purpose, Some(StorePurpose::LinuxCompletion))
                                && self.source.as_ref().is_some_and(|source| {
                                    source.epoch == outcome.original_normal_epoch
                                })
                                && self.editor.as_ref().is_some_and(|editor| {
                                    editor.generation() == self.generation
                                        && controller
                                            .firmware_editor_bound_source(editor)
                                            .is_ok_and(|bound| Some(bound) == self.source.as_ref())
                                })
                                && self
                                    .runtime
                                    .as_ref()
                                    .is_some_and(|lease| services.firmware_lease_matches(lease))
                                && self.recovery_store_root.as_deref() == Some(store.root())
                                && proof.summary == summary
                                && proof.matches_completion(&outcome, store.root())
                        })
                        .map(|owner| RecoveryHostReturn {
                            owner,
                            generation: self.generation,
                            source_epoch: outcome.original_normal_epoch,
                        });
                    self.accept_completion_return(summary, store, *outcome, controller, services);
                    self.recovery_host_return = returned;
                    return;
                }
                event => event,
            };
        let (store, reason) = match event {
            FirmwareOwnerEvent::StartupFailed { error } => {
                self.fail(
                    controller,
                    format!("Actual startup has no returnable store: {}", error.message),
                );
                return;
            }
            FirmwareOwnerEvent::StartupFailedWithStore {
                error,
                store,
                normal_retirement,
                updater_retirement,
            } => {
                if let Some(v) = normal_retirement {
                    self.retain_owner(OwnerOriginal::Normal(v));
                }
                if let Some(v) = updater_retirement {
                    self.retain_owner(OwnerOriginal::Startup(v));
                }
                (store, error.message)
            }
            FirmwareOwnerEvent::Rejected { error, store } => (store, error.message),
            FirmwareOwnerEvent::RejectedAfterNormalRetirement {
                error,
                store,
                observation,
            } => {
                self.retain_owner(OwnerOriginal::Normal(observation));
                (store, error.message)
            }
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            FirmwareOwnerEvent::LinuxRecoveryReturned { .. }
            | FirmwareOwnerEvent::LinuxCompletionReturned { .. } => {
                unreachable!("live completion event consumed above")
            }
            FirmwareOwnerEvent::Retired { summary, store } => {
                self.retain_owner(OwnerOriginal::Retired(summary));
                (store, "Actual firmware owner retired; completion and live normal service are not established".into())
            }
        };
        self.owner_return_observed = true;
        self.library_restored = false;
        self.purpose = Some(StorePurpose::Recovery);
        self.restoration = Some(services.restore_library(store));
        self.fail(controller, reason);
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    fn accept_completion_return(
        &mut self,
        summary: FirmwareOwnerSummary,
        store: LibraryStore,
        outcome: LinuxCompletionOutcome,
        controller: &mut AppController,
        services: &RuntimeServices,
    ) {
        // Only the concrete bridge event branch can install a close proof.
        self.recovery_host_return = None;
        self.retain_owner(OwnerOriginal::Retired(summary));
        self.owner_return_observed = true;
        self.library_restored = false;
        let boundary = if matches!(self.purpose, Some(StorePurpose::LinuxCompletion))
            && !self.recovery_editor
            && outcome.editor_return_allowed
            && !outcome.view.quarantined
            && outcome.view.normal_mode_verified
            && outcome
                .verified_normal_epoch
                .is_some_and(|epoch| epoch > outcome.original_normal_epoch)
            && self
                .source
                .as_ref()
                .is_some_and(|source| source.epoch == outcome.original_normal_epoch)
        {
            outcome
                .verified_normal_epoch
                .and_then(|epoch| epoch.0.checked_add(1))
                .map(SessionEpoch)
                .zip(outcome.selection.clone())
                .map(|(next, selection)| (outcome.original_normal_epoch, next, selection))
        } else {
            None
        };
        let reason=outcome.view.diagnostic.clone().unwrap_or_else(||"Update interrupted; normal editor remains held. Preserve this attempt for a new full-image recovery review.".into());
        self.completion_outcome = Some(outcome);
        self.completion_monitor_instance = None;
        self.restoration = Some(services.restore_library(store));
        if let Some(boundary) = boundary {
            self.return_boundary = Some(boundary);
            self.purpose = Some(StorePurpose::LinuxCompletionReturn);
            self.admission_stopped = false;
            self.offer_started = None;
            self.refresh_reads_requested = false;
            self.phase = FirmwareCoordinatorPhase::RestoringLibrary;
        } else {
            self.purpose = Some(StorePurpose::Recovery);
            self.fail(controller, reason);
        }
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    fn accept_recovery_return(
        &mut self,
        summary: FirmwareOwnerSummary,
        store: LibraryStore,
        outcome: LinuxRecoveryOutcome,
        controller: &mut AppController,
        services: &RuntimeServices,
    ) {
        // Only the concrete bridge event branch above can install a close proof.
        // Direct typed/presentation seams retain the ordinary held-close policy.
        self.recovery_host_return = None;
        let active = self.recovery_editor
            && matches!(self.purpose, Some(StorePurpose::LinuxRecovery))
            && self
                .source
                .as_ref()
                .is_some_and(|source| source.epoch == outcome.editor_source_epoch)
            && self.editor.is_some()
            && self
                .runtime
                .as_ref()
                .is_some_and(|lease| services.firmware_lease_matches(lease));
        let boundary = if active
            && outcome.editor_return_allowed
            && !outcome.view.quarantined
            && outcome.view.normal_mode_verified
            && outcome.view.target_version_verified
            && outcome.application_version.as_deref() == Some("V1.8.2")
            && outcome.history.as_ref().is_some_and(|history| {
                !history.predecessors.is_empty()
                    && !history.permits_flash()
                    && !history.can_resume()
            }) {
            outcome
                .verified_normal_epoch
                .and_then(|epoch| {
                    epoch
                        .0
                        .max(outcome.editor_source_epoch.0)
                        .checked_add(1)
                        .map(SessionEpoch)
                })
                .zip(outcome.selection.clone())
                .map(|(minimum, selection)| (outcome.editor_source_epoch, minimum, selection))
        } else {
            None
        };
        let reason = outcome.view.diagnostic.clone().unwrap_or_else(||
            "Recovery owner returned without the complete linked closure and fresh normal/version proof; editor remains held".into());
        self.retain_owner(OwnerOriginal::Retired(summary));
        self.owner_return_observed = true;
        self.library_restored = false;
        self.recovery_outcome = Some(outcome);
        self.completion_monitor_instance = None;
        self.restoration = Some(services.restore_library(store));
        if let Some(boundary) = boundary {
            self.return_boundary = Some(boundary);
            self.purpose = Some(StorePurpose::LinuxRecoveryReturn);
            self.admission_stopped = false;
            self.offer_started = None;
            self.refresh_reads_requested = false;
            self.phase = FirmwareCoordinatorPhase::RestoringLibrary;
        } else {
            self.purpose = Some(StorePurpose::Recovery);
            self.fail(controller, reason);
        }
    }

    /// Presentation fixture only. This cannot mint a monitor, store, ticket or live return event.
    #[cfg(all(test, feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn test_completion_projection(&mut self, outcome: LinuxCompletionOutcome) {
        self.completion_outcome = Some(outcome);
    }
    /// Unreadable real monitor plus descriptive metadata only. No owner,
    /// permit, store or attached owner view is produced by this test fixture.
    #[cfg(all(test, feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn test_unreadable_completion_monitor(&mut self, directory: PathBuf) {
        self.install_completion_monitor(LinuxCompletionMonitor::default());
        self.completion_run_directory = Some(directory);
        self.phase = FirmwareCoordinatorPhase::Owned;
        self.recovery_editor = true;
    }
    /// Presentation only: no ticket, monitor, moved store, editor lease or live
    /// recovery authorization can be produced by this cfg(test) description.
    #[cfg(all(test, feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn test_recovery_projection(&mut self, view: LinuxCompletionView, active: bool) {
        self.recovery_editor = active;
        self.recovery_outcome = Some(LinuxRecoveryOutcome {
            view,
            selection: None,
            editor_source_epoch: SessionEpoch(0),
            verified_normal_epoch: None,
            application_version: None,
            journal_path: "cfg(test) presentation only".into(),
            editor_return_allowed: false,
            history: None,
        });
    }
    fn poll_normal_refresh(
        &mut self,
        controller: &mut AppController,
        services: &mut RuntimeServices,
        producers_settled: bool,
    ) {
        if !self.library_restored
            || !(self.normal_return_purpose()
                || matches!(self.purpose, Some(StorePurpose::Recovery)))
        {
            return;
        }
        if self.phase == FirmwareCoordinatorPhase::ReturningNormal
            && let Some((retired, next, selection)) = self.return_boundary.take()
        {
            self.capture_return_submitted = false;
            let Some(lease) = &self.editor else {
                return;
            };
            let effects = if self.is_linux_recovery() {
                controller.begin_firmware_recovery_normal_refresh(lease, retired, next, selection)
            } else {
                controller.begin_firmware_editor_normal_refresh(lease, retired, next, selection)
            };
            match effects {
                Ok(effects) => {
                    self.refresh.extend(effects.into_iter().filter_map(|e| {
                        if let Effect::Device(c) = e {
                            Some(c)
                        } else {
                            None
                        }
                    }));
                    self.phase = FirmwareCoordinatorPhase::RefreshingNormal;
                }
                Err(e) => self.fail(controller, e.message),
            }
        }
        if self.phase != FirmwareCoordinatorPhase::RefreshingNormal {
            return;
        }
        if matches!(self.refresh.front(), Some(DeviceCommand::ReadGlobals(_))) {
            match controller.firmware_editor_normal_refresh_current_received(
                self.editor.as_ref().expect("same original lease"),
            ) {
                Ok(true) => {}
                Ok(false) => {
                    self.offer_started = None;
                    return;
                }
                Err(e) => {
                    self.fail(controller, e.message);
                    return;
                }
            }
        }
        if !self.offer_within_bound(controller) {
            return;
        }
        if let Some(command) = self.refresh.pop_front() {
            match self
                .device
                .as_ref()
                .expect("genuine refresh endpoint")
                .refresh_submit(command)
            {
                Ok(()) => {}
                Err(SubmitError::Busy(original)) => self.refresh.push_front(original),
                Err(SubmitError::Disconnected(original)) => {
                    self.refresh.push_front(original);
                    self.fail(
                        controller,
                        "Fresh normal command was not offered; actor stopped".into(),
                    );
                    return;
                }
            }
        }
        let Some(lease) = &self.editor else {
            return;
        };
        if self.refresh.is_empty() && !self.refresh_reads_requested {
            match controller.request_firmware_editor_normal_refresh_reads(lease) {
                Ok(effects) => {
                    self.refresh.extend(effects.into_iter().filter_map(|e| {
                        if let Effect::Device(c) = e {
                            Some(c)
                        } else {
                            None
                        }
                    }));
                    self.refresh_reads_requested = true;
                }
                Err(e) if e.kind == ErrorKind::Busy => {}
                Err(e) => {
                    self.fail(controller, e.message);
                    return;
                }
            }
        }
        if self.refresh_reads_requested
            && self.refresh.is_empty()
            && producers_settled
            && services.firmware_queues_settled()
            && let Some(runtime) = &self.runtime
            && services.firmware_lease_matches(runtime)
        {
            match services.release_firmware_with(runtime, || {
                controller.finish_firmware_editor_normal_refresh(lease)
            }) {
                Ok(()) => {
                    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
                    if matches!(
                        self.purpose,
                        Some(
                            StorePurpose::LinuxCompletionReturn | StorePurpose::LinuxRecoveryReturn
                        )
                    ) {
                        self.entry_gate_held = false;
                    }
                    self.clear_after_return()
                }
                Err(e) if e.kind == ErrorKind::Busy => {}
                Err(e) => self.fail(controller, e.message),
            }
        }
    }
    fn offer_within_bound(&mut self, controller: &mut AppController) -> bool {
        if self.action.is_none() && self.control.is_none() && self.refresh.is_empty() {
            self.offer_started = None;
            return true;
        }
        let started = self
            .offer_started
            .get_or_insert_with(std::time::Instant::now);
        if started.elapsed() > std::time::Duration::from_secs(10) {
            self.fail(controller, "An original unoffered command stayed Busy for 10 seconds; it remains retained without automatic retry".into());
            return false;
        }
        true
    }
    fn normal_return_purpose(&self) -> bool {
        if matches!(self.purpose, Some(StorePurpose::ReturnNormal)) {
            return true;
        }
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        if matches!(
            self.purpose,
            Some(StorePurpose::LinuxCompletionReturn | StorePurpose::LinuxRecoveryReturn)
        ) {
            return true;
        }
        false
    }
    fn clear_after_return(&mut self) {
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        {
            self.recovery_editor = false;
            self.recovery_pending = None;
            self.recovery_preparation_cancel_requested = None;
            self.recovery_store_root = None;
            self.recovery_host_return = None;
        }
        self.editor = None;
        self.runtime = None;
        self.source = None;
        self.selection = None;
        self.reservation = None;
        self.closed = None;
        self.acquisition = None;
        self.receipt_capture_id = None;
        self.facts = None;
        self.outcome = None;
        self.conditional_consistency_observed = false;
        self.capture_io_failed = false;
        self.prepared = None;
        self.reviewed = None;
        #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
        {
            self.entry_draft = None;
            self.entry_authorization = None;
        }
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        {
            self.completion_draft = None;
            self.completion_authorization = None;
        }
        // entry_gate_held is never cleared by an ordinary Return helper.
        self.purpose = None;
        self.library_restored = false;
        self.owner_return_observed = false;
        self.capture_store_returned = false;
        self.capture_return_submitted = false;
        self.capture_restore_retry = false;
        self.return_boundary = None;
        self.refresh_reads_requested = false;
        self.records = 0;
        self.phase = FirmwareCoordinatorPhase::Idle;
    }
    fn note(&mut self, message: String) {
        if self.diagnostics.len() < 32 {
            self.diagnostics
                .push_back(message.chars().take(4096).collect());
        } else {
            self.omitted = self.omitted.saturating_add(1);
        }
    }
    fn fail(&mut self, controller: &mut AppController, reason: String) {
        self.note(reason.clone());
        self.admission_stopped = true;
        self.phase = FirmwareCoordinatorPhase::RecoveryRequired;
        if let Some(lease) = &self.editor {
            let _ = controller.hold_firmware_editor_for_recovery(lease, reason);
        }
    }
    fn retain_original(&mut self, event: FirmwareBackupCaptureEvent) {
        if self.originals.len() < 8 {
            self.originals.push_back(event);
        } else {
            self.omitted = self.omitted.saturating_add(1);
        }
    }
    fn retain_owner(&mut self, original: OwnerOriginal) {
        let label = match &original {
            OwnerOriginal::Normal(v) => format!(
                "Original normal retirement: epoch {}; raw records {}; missing extents {}; output destroyed {}",
                v.normal_epoch.0,
                v.retained_raw.len(),
                v.missing.len(),
                v.output_handle_destroyed
            ),
            OwnerOriginal::Startup(v) => format!(
                "Original updater startup retirement: raw records {}; output destroyed {}",
                v.retained_raw_chunks.len(),
                v.output_handle_destroyed
            ),
            OwnerOriginal::Retired(v) => format!(
                "Original owner phase: {}; verified bytes {}; completion {}",
                v.phase, v.device_verified_bytes, v.completion_verified
            ),
        };
        self.note(label);
        if self.owner_originals.len() < 8 {
            self.owner_originals.push_back(original);
        } else {
            self.omitted = self.omitted.saturating_add(1);
        }
    }
}
impl Drop for FirmwareUpdateCoordinator {
    fn drop(&mut self) {
        self.worker_cancel.store(true, Ordering::Release);
        if let Some(d) = &self.device {
            if !self.is_linux_recovery() {
                d.capture.cancel();
            }
            d.stop();
        }
    }
}
fn invalid(message: impl Into<String>) -> ServiceError {
    ServiceError::invalid(message)
}
fn busy(message: impl Into<String>) -> ServiceError {
    ServiceError::new(ErrorKind::Busy, message)
}

#[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
fn prepare_entry_draft(
    prepared: Arc<PreparedLocalReview>,
    facts: PublishedFacts,
    reservation: FirmwareCaptureReservation,
    inputs: EntryStudyInputs,
) -> Result<EntryStudyDraft, ServiceError> {
    fn json<T: serde::Serialize>(value: &T) -> Result<Arc<[u8]>, ServiceError> {
        serde_json::to_vec(value)
            .map(Into::into)
            .map_err(|e| ServiceError::invalid(e.to_string()))
    }
    fn read(path: &std::path::Path, expected: &str) -> Result<Arc<[u8]>, ServiceError> {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .and_then(|f| f.take(128 * 1024 * 1024 + 1).read_to_end(&mut bytes))
            .map_err(|e| ServiceError::invalid(e.to_string()))?;
        if bytes.len() > 128 * 1024 * 1024 || JobSha256::of(&bytes).hex() != expected {
            return Err(ServiceError::invalid("Original captured material changed"));
        }
        Ok(bytes.into())
    }
    let reopened = reopen_firmware_backup_review(&facts.archive, &facts.sidecar)?;
    let evidence = reopen_firmware_capture_evidence(&facts.index)?;
    if reopened != facts.proposal.review
        || evidence != facts.evidence
        || prepared.view.archive_sha256 != facts.proposal.archive_sha256
        || prepared.view.acquisition != facts.proposal.acquisition
        || reservation.epoch != prepared.source.epoch
    {
        return Err(ServiceError::invalid(
            "Entry review is not this exact genuine same-run acquisition",
        ));
    }
    let captured_materials = [&facts.archive, &facts.sidecar, &facts.index]
        .into_iter()
        .map(|path| {
            let bytes = std::fs::read(path).map_err(|e| ServiceError::invalid(e.to_string()))?;
            Ok(EntryStudyFilePin {
                path: path.clone(),
                bytes: bytes.len() as u64,
                sha256: JobSha256::of(&bytes).hex(),
            })
        })
        .collect::<Result<Vec<_>, ServiceError>>()?;
    let archive = read(&facts.archive, &facts.proposal.archive_sha256)?;
    let inventory = read(&facts.sidecar, &facts.proposal.sidecar_sha256)?;
    // Record genuine cached identity separately; no fabricated raw identity DTO.
    let genuine = serde_json::json!({"local_preparation":{"digest":prepared.view.digest,"image_sha256":prepared.view.image_sha256,"archive_sha256":prepared.view.archive_sha256,"sidecar_sha256":prepared.view.sidecar_sha256,"evidence_index_sha256":prepared.view.evidence_index_sha256,"acquisition":prepared.view.acquisition,"source_epoch":prepared.view.source_epoch,"local_draft_excluded":prepared.view.local_draft_excluded,"permits_flash":false},"proposal":facts.proposal,"capture_evidence":evidence,"reservation":reservation,"source_identity":prepared.source.identity,"source_epoch":prepared.source.epoch});
    let recovery =
        std::fs::read(&inputs.recovery.path).map_err(|e| ServiceError::invalid(e.to_string()))?;
    let profile = serde_json::json!({"purpose":"normal-entry-observation-only","candidates":"two fixed literals; one syscall each; no transfer","receiver":"unaccepted","safe_return":"unaccepted"});
    let restore = serde_json::json!({"destination_restoration":"unaccepted","installed_IR_audio":"unavailable","source":"genuine retained capture; not full restorable content"});
    let mut versions = vec![FirmwareVersionClaim {
        origin: FirmwareVersionOrigin::ImageDerived,
        value: prepared
            .image
            .review
            .summary()
            .image_version
            .clone()
            .unwrap_or_default(),
        raw: vec![],
    }];
    if let Some(manual) = facts.proposal.review.context.manual_version.as_ref() {
        versions.push(FirmwareVersionClaim {
            origin: FirmwareVersionOrigin::HumanReported,
            value: manual.raw_input().into(),
            raw: vec![],
        });
    }
    let context=FirmwareUpdateContextDto{profile_id:"FW06-B experimental normal entry only".into(),hardware_policy:"No accepted physical transfer/recovery profile; genuine same-run reservation bound separately".into(),update_mode:"entry observation only".into(),normal:None,versions,backup_objects:vec![],loss_items:facts.proposal.review.diagnostics.clone(),power_statement:"Power/connection review remains explicit pinned study material".into(),connection_statement:format!("Retained normal USB binding {}",reservation.usb.digest())};
    let image: Arc<[u8]> = prepared.image.review.source().to_vec().into();
    let source = RetainedJobBlob::checked(
        AssetId("entry-study-image-reference-only".into()),
        image.clone(),
        JobSha256::of(&image),
    )?;
    let material = |kind, bytes| FirmwareMaterialInput { kind, bytes };
    let job = Arc::new(ReviewedFirmwareUpdate::prepare(
        source,
        context,
        vec![
            material(FirmwareMaterialKind::Profile, json(&profile)?),
            material(FirmwareMaterialKind::Evidence, json(&genuine)?),
            material(FirmwareMaterialKind::BackupArchive, archive),
            material(FirmwareMaterialKind::BackupInventory, inventory),
            material(FirmwareMaterialKind::RestorePlan, json(&restore)?),
            material(FirmwareMaterialKind::Recovery, recovery.into()),
            material(
                FirmwareMaterialKind::ApprovalStatement,
                Arc::from(
                    &b"Proposed entry study only; no execution signal yet, no transfer permission"
                        [..],
                ),
            ),
        ],
        128,
    )?);
    EntryStudyDraft::prepare(
        job,
        reservation,
        facts.proposal.acquisition,
        facts.evidence.index_sha256,
        genuine,
        captured_materials,
        inputs,
    )
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn prepare_completion_draft(
    prepared: Arc<PreparedLocalReview>,
    facts: PublishedFacts,
    reservation: FirmwareCaptureReservation,
    inputs: LinuxCompletionInputs,
) -> Result<LinuxCompletionDraft, ServiceError> {
    fn json<T: serde::Serialize>(value: &T) -> Result<Arc<[u8]>, ServiceError> {
        serde_json::to_vec(value)
            .map(Into::into)
            .map_err(|e| ServiceError::invalid(e.to_string()))
    }
    fn read(path: &std::path::Path, expected: &str) -> Result<Arc<[u8]>, ServiceError> {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .and_then(|f| f.take(128 * 1024 * 1024 + 1).read_to_end(&mut bytes))
            .map_err(|e| ServiceError::invalid(e.to_string()))?;
        if bytes.len() > 128 * 1024 * 1024 || JobSha256::of(&bytes).hex() != expected {
            return Err(ServiceError::invalid("Original captured material changed"));
        }
        Ok(bytes.into())
    }
    let reopened = reopen_firmware_backup_review(&facts.archive, &facts.sidecar)?;
    let evidence = reopen_firmware_capture_evidence(&facts.index)?;
    if reopened != facts.proposal.review
        || evidence != facts.evidence
        || prepared.view.archive_sha256 != facts.proposal.archive_sha256
        || prepared.view.acquisition != facts.proposal.acquisition
        || reservation.epoch != prepared.source.epoch
    {
        return Err(ServiceError::invalid(
            "Entry review is not this exact genuine same-run acquisition",
        ));
    }
    let captured_materials = [&facts.archive, &facts.sidecar, &facts.index]
        .into_iter()
        .map(|path| {
            let bytes = std::fs::read(path).map_err(|e| ServiceError::invalid(e.to_string()))?;
            Ok(EntryStudyFilePin {
                path: path.clone(),
                bytes: bytes.len() as u64,
                sha256: JobSha256::of(&bytes).hex(),
            })
        })
        .collect::<Result<Vec<_>, ServiceError>>()?;
    let archive = read(&facts.archive, &facts.proposal.archive_sha256)?;
    let inventory = read(&facts.sidecar, &facts.proposal.sidecar_sha256)?;
    // Record genuine cached identity separately; no fabricated raw identity DTO.
    let genuine = serde_json::json!({"local_preparation":{"digest":prepared.view.digest,"image_sha256":prepared.view.image_sha256,"archive_sha256":prepared.view.archive_sha256,"sidecar_sha256":prepared.view.sidecar_sha256,"evidence_index_sha256":prepared.view.evidence_index_sha256,"acquisition":prepared.view.acquisition,"source_epoch":prepared.view.source_epoch,"local_draft_excluded":prepared.view.local_draft_excluded,"permits_flash":false},"proposal":facts.proposal,"capture_evidence":evidence,"reservation":reservation,"source_identity":prepared.source.identity,"source_epoch":prepared.source.epoch});
    let recovery =
        std::fs::read(&inputs.recovery.path).map_err(|e| ServiceError::invalid(e.to_string()))?;
    let profile = serde_json::json!({"purpose":"experimental-linux-completion-separate-reviewed-phases","candidates":"two fixed literals; one syscall each; query and transfer require later live reviews","receiver":"unaccepted","safe_return":"unaccepted"});
    let restore = serde_json::json!({"destination_restoration":"unaccepted","installed_IR_audio":"unavailable","source":"genuine retained capture; not full restorable content"});
    let mut versions = vec![FirmwareVersionClaim {
        origin: FirmwareVersionOrigin::ImageDerived,
        value: prepared
            .image
            .review
            .summary()
            .image_version
            .clone()
            .unwrap_or_default(),
        raw: vec![],
    }];
    if let Some(manual) = facts.proposal.review.context.manual_version.as_ref() {
        versions.push(FirmwareVersionClaim {
            origin: FirmwareVersionOrigin::HumanReported,
            value: manual.raw_input().into(),
            raw: vec![],
        });
    }
    let context=FirmwareUpdateContextDto{profile_id:"FW08 experimental source-bound Linux completion".into(),hardware_policy:"No accepted physical transfer/recovery profile; genuine same-run reservation bound separately".into(),update_mode:"entry then separately reviewed boot query, transfer and normal return".into(),normal:None,versions,backup_objects:vec![],loss_items:facts.proposal.review.diagnostics.clone(),power_statement:"Power/connection review remains explicit pinned study material".into(),connection_statement:format!("Retained normal USB binding {}",reservation.usb.digest())};
    let image: Arc<[u8]> = prepared.image.review.source().to_vec().into();
    let source = RetainedJobBlob::checked(
        AssetId("linux-completion-image".into()),
        image.clone(),
        JobSha256::of(&image),
    )?;
    let material = |kind, bytes| FirmwareMaterialInput { kind, bytes };
    let job = Arc::new(ReviewedFirmwareUpdate::prepare(
        source,
        context,
        vec![
            material(FirmwareMaterialKind::Profile, json(&profile)?),
            material(FirmwareMaterialKind::Evidence, json(&genuine)?),
            material(FirmwareMaterialKind::BackupArchive, archive),
            material(FirmwareMaterialKind::BackupInventory, inventory),
            material(FirmwareMaterialKind::RestorePlan, json(&restore)?),
            material(FirmwareMaterialKind::Recovery, recovery.into()),
            material(
                FirmwareMaterialKind::ApprovalStatement,
                Arc::from(
                    &b"Proposed Linux entry review only; no execution signal yet; later boot query and transfer each require fresh review"
                        [..],
                ),
            ),
        ],
        128,
    )?);
    LinuxCompletionDraft::prepare(
        job,
        reservation,
        facts.proposal.acquisition,
        facts.evidence.index_sha256,
        genuine,
        captured_materials,
        inputs,
    )
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn load_completion_resources() -> Result<LinuxCompletionInputs, ServiceError> {
    crate::firmware_resources::load_completion_resources()
}
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn publish_completion_signal(
    directory: &std::path::Path,
    name: &str,
    signal: &str,
) -> Result<(), ServiceError> {
    use std::{
        io::Write,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };
    if !matches!(
        name,
        "approve-query" | "approve-transfer" | "confirm-application-version"
    ) || signal.len() > 512
    {
        return Err(invalid("Unknown live firmware confirmation"));
    }
    let metadata = std::fs::symlink_metadata(directory).map_err(|e| invalid(e.to_string()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.mode() & 0o777 != 0o700 {
        return Err(invalid("Live confirmation directory changed"));
    }
    let path = directory.join(name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(0x20_000)
        .open(&path)
        .map_err(|e| invalid(e.to_string()))?;
    file.write_all(signal.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|e| invalid(e.to_string()))?;
    std::fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|e| invalid(e.to_string()))?;
    if std::fs::read(&path).map_err(|e| invalid(e.to_string()))? != signal.as_bytes() {
        return Err(invalid("Live confirmation reread differs"));
    }
    Ok(())
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn publish_completion_review(draft: &LinuxCompletionDraft) -> Result<(), ServiceError> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let directory = &draft.review().inputs.run_directory;
    let path = directory.join("entry-review.json");
    let bytes =
        serde_json::to_vec_pretty(draft.review()).map_err(|error| invalid(error.to_string()))?;
    if bytes.len() > 32 * 1024 * 1024 {
        return Err(invalid("Entry review exceeds bounded publication"));
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(0x20_000)
        .open(&path)
        .map_err(|error| invalid(error.to_string()))?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| invalid(error.to_string()))?;
    std::fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|error| invalid(error.to_string()))?;
    if std::fs::read(&path).map_err(|error| invalid(error.to_string()))? != bytes {
        return Err(invalid("Published entry review reread differs"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "firmware_update_coordinator_tests.rs"]
mod firmware_update_coordinator_tests;

/// cfg(test) typed-event substitution on a genuinely returned simulator ticket
/// and original store; never a physical Linux recovery owner or display permit.
#[cfg(all(test, feature = "firmware-linux-completion", target_os = "linux"))]
pub(crate) fn test_prepare_settled_recovery_host_close(
    app: &mut crate::StudioApp,
    directory: &std::path::Path,
) {
    firmware_update_coordinator_tests::prepare_settled_recovery_host_close_event_seam(
        app, directory,
    );
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
pub(crate) fn completion_display_facts(review: &LinuxCompletionReview) -> serde_json::Value {
    // Display projection only. Full capture and source bytes stay in the opaque review.
    let records = review
        .genuine_preparation
        .pointer("/capture_evidence/records")
        .and_then(|v| v.as_u64());
    let facts = serde_json::json!({"purpose":review.purpose,"job_sha256":review.job_sha256,"image_sha256":gp200_transport::firmware_vendor_sequence::VENDOR_IMAGE_SHA256,"contract_sha256":gp200_transport::firmware_vendor_sequence::VENDOR_CONTRACT_SHA256,"reservation":review.reservation,"executable":review.executable,"captured_materials":review.captured_materials,"genuine_preparation":{"capture_evidence":{"records":records}},"retained_source_files":review.inputs.sources.len(),"source_pins_truncated":review.inputs.sources.len()>16,"inputs":{"run_directory":review.inputs.run_directory,"loss":review.inputs.loss,"recovery":review.inputs.recovery,"vendor_guidance":review.inputs.vendor_guidance,"contract":review.inputs.contract,"sources":review.inputs.sources.iter().take(16).collect::<Vec<_>>()},"limitations":review.limitations});
    facts
}
