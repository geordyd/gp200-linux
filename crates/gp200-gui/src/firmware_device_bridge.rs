//! Private shared concrete services. Public render state never owns these rails.
use gp200_domain::*;
use gp200_library::{FirmwareBackupClient, LibraryFirmwareRuntimeClient, LibraryWorker};
use gp200_transport::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

#[derive(Default)]
pub(crate) struct RuntimeAdmission {
    pub(crate) frozen: AtomicBool,
    generation: AtomicU64,
    offers: Mutex<()>,
}
pub(crate) struct RuntimeFirmwareLease {
    owner: Arc<RuntimeAdmission>,
    pub(crate) generation: u64,
}
impl RuntimeAdmission {
    pub(crate) fn freeze(self: &Arc<Self>) -> Result<RuntimeFirmwareLease, ServiceError> {
        let _gate = self.offers.try_lock().map_err(|_| paused())?;
        if self.frozen.load(Ordering::Acquire) {
            return Err(paused());
        }
        let generation = self
            .generation
            .load(Ordering::Acquire)
            .checked_add(1)
            .ok_or_else(|| {
                ServiceError::new(ErrorKind::Indeterminate, "Runtime generation exhausted")
            })?;
        self.generation.store(generation, Ordering::Release);
        self.frozen.store(true, Ordering::Release);
        Ok(RuntimeFirmwareLease {
            owner: self.clone(),
            generation,
        })
    }
    pub(crate) fn matches(self: &Arc<Self>, lease: &RuntimeFirmwareLease) -> bool {
        Arc::ptr_eq(self, &lease.owner)
            && self.frozen.load(Ordering::Acquire)
            && self.generation.load(Ordering::Acquire) == lease.generation
    }
    pub(crate) fn release(
        self: &Arc<Self>,
        lease: &RuntimeFirmwareLease,
    ) -> Result<(), ServiceError> {
        self.release_with(lease, || Ok(()))
    }
    pub(crate) fn release_with(
        self: &Arc<Self>,
        lease: &RuntimeFirmwareLease,
        finish: impl FnOnce() -> Result<(), ServiceError>,
    ) -> Result<(), ServiceError> {
        let _gate = self.offers.try_lock().map_err(|_| paused())?;
        if !self.matches(lease) {
            return Err(paused());
        }
        finish()?;
        self.frozen.store(false, Ordering::Release);
        Ok(())
    }
}
pub(crate) fn paused() -> ServiceError {
    ServiceError::new(
        ErrorKind::Busy,
        "NotOffered: normal work is paused for firmware preparation or recovery",
    )
}
struct NormalMidiProxy {
    service: Arc<MidiDeviceService>,
    gate: Arc<RuntimeAdmission>,
}
impl DeviceService for NormalMidiProxy {
    fn firmware_backup_capture_support(&self) -> FirmwareCaptureSupport {
        FirmwareCaptureSupport::ConcreteClientRequired
    }
    fn try_submit(&self, command: DeviceCommand) -> Result<(), SubmitError<DeviceCommand>> {
        let Ok(_offer) = self.gate.offers.try_lock() else {
            return Err(SubmitError::Busy(command));
        };
        if self.gate.frozen.load(Ordering::Acquire) {
            return Err(SubmitError::Busy(command));
        }
        self.service.try_submit(command)
    }
    fn try_next_event(&self) -> Option<DeviceEvent> {
        self.service.try_next_event()
    }
    fn shutdown(&self) {
        self.service.shutdown();
    }
}
pub(crate) struct InjectedNormalProxy {
    pub(crate) service: Box<dyn DeviceService>,
    pub(crate) gate: Arc<RuntimeAdmission>,
}
impl DeviceService for InjectedNormalProxy {
    fn firmware_backup_capture_support(&self) -> FirmwareCaptureSupport {
        FirmwareCaptureSupport::Unavailable
    }
    fn try_submit(&self, command: DeviceCommand) -> Result<(), SubmitError<DeviceCommand>> {
        let Ok(_offer) = self.gate.offers.try_lock() else {
            return Err(SubmitError::Busy(command));
        };
        if self.gate.frozen.load(Ordering::Acquire) {
            return Err(SubmitError::Busy(command));
        }
        self.service.try_submit(command)
    }
    fn try_next_event(&self) -> Option<DeviceEvent> {
        self.service.try_next_event()
    }
    fn shutdown(&self) {
        self.service.shutdown();
    }
}
pub(crate) struct FirmwareServiceEndpoint {
    service: Arc<MidiDeviceService>,
    pub(crate) capture: FirmwareBackupCaptureClient,
    ticket: Option<FirmwareOwnerTicket>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    linux_ticket: Option<ReturnedLinuxOwnerKind>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    linux_return: Option<ReturnedLinuxOwner>,
    #[cfg(all(test, feature = "firmware-linux-completion", target_os = "linux"))]
    linux_return_test_event: Option<ReturnedLinuxTestEvent>,
}
/// Private evidence of a consumed concrete Linux owner ticket. Render
/// descriptions and direct coordinator seams cannot construct this proof.
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReturnedLinuxOwnerKind {
    Recovery,
    Completion,
}
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
pub(crate) struct ReturnedLinuxOwner {
    pub(crate) summary: FirmwareOwnerSummary,
    kind: ReturnedLinuxOwnerKind,
    outcome_sha256: String,
    root: std::path::PathBuf,
}
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
impl ReturnedLinuxOwner {
    pub(crate) fn root(&self) -> &std::path::Path {
        &self.root
    }
    pub(crate) fn matches_recovery(
        &self,
        outcome: &LinuxRecoveryOutcome,
        root: &std::path::Path,
    ) -> bool {
        self.kind == ReturnedLinuxOwnerKind::Recovery
            && self.root == root
            && serde_json::to_vec(outcome)
                .is_ok_and(|bytes| JobSha256::of(&bytes).hex() == self.outcome_sha256)
    }
    pub(crate) fn matches_completion(
        &self,
        outcome: &LinuxCompletionOutcome,
        root: &std::path::Path,
    ) -> bool {
        self.kind == ReturnedLinuxOwnerKind::Completion
            && self.root == root
            && serde_json::to_vec(outcome)
                .is_ok_and(|bytes| JobSha256::of(&bytes).hex() == self.outcome_sha256)
    }
}
#[cfg(all(test, feature = "firmware-linux-completion", target_os = "linux"))]
enum ReturnedLinuxTestEvent {
    Recovery(FirmwareOwnerSummary, Box<LinuxRecoveryOutcome>),
    Completion(FirmwareOwnerSummary, Box<LinuxCompletionOutcome>),
}
impl FirmwareServiceEndpoint {
    pub(crate) fn split(
        mut service: MidiDeviceService,
        gate: Arc<RuntimeAdmission>,
    ) -> (Box<dyn DeviceService>, Self) {
        let capture = service
            .take_firmware_backup_capture_client()
            .expect("fresh concrete service has sole capture client");
        let service = Arc::new(service);
        (
            Box::new(NormalMidiProxy {
                service: service.clone(),
                gate,
            }),
            Self {
                service,
                capture,
                ticket: None,
                #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
                linux_ticket: None,
                #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
                linux_return: None,
                #[cfg(all(test, feature = "firmware-linux-completion", target_os = "linux"))]
                linux_return_test_event: None,
            },
        )
    }
    pub(crate) fn unavailable(&self) -> &'static str {
        self.service.firmware_update_unavailable_reason()
    }
    pub(crate) fn stopped(&self) -> bool {
        self.service.stopped()
    }
    pub(crate) fn submit(
        &mut self,
        request: FirmwareOwnershipRequest,
    ) -> Result<(), SubmitError<FirmwareOwnershipRequest>> {
        if self.ticket.is_some() {
            return Err(SubmitError::Busy(request));
        }
        self.ticket = Some(self.service.try_start_firmware(request)?);
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        {
            self.linux_ticket = None;
            self.linux_return = None;
        }
        Ok(())
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn submit_completion(
        &mut self,
        request: FirmwareOwnershipRequest,
    ) -> Result<(), SubmitError<FirmwareOwnershipRequest>> {
        self.submit(request)?;
        self.linux_ticket = Some(ReturnedLinuxOwnerKind::Completion);
        Ok(())
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn submit_recovery(
        &mut self,
        request: LinuxRecoveryRequest,
    ) -> Result<(), SubmitError<LinuxRecoveryRequest>> {
        if self.ticket.is_some() {
            return Err(SubmitError::Busy(request));
        }
        self.ticket = Some(self.service.try_start_linux_recovery(request)?);
        self.linux_ticket = Some(ReturnedLinuxOwnerKind::Recovery);
        self.linux_return = None;
        Ok(())
    }
    pub(crate) fn poll(&mut self) -> Option<FirmwareOwnerEvent> {
        let event = self.service.try_next_firmware_event()?;
        #[cfg(all(test, feature = "firmware-linux-completion", target_os = "linux"))]
        let event = match (event, self.linux_return_test_event.take()) {
            (
                FirmwareOwnerEvent::Rejected { store, .. },
                Some(ReturnedLinuxTestEvent::Recovery(summary, outcome)),
            ) if self.linux_ticket == Some(ReturnedLinuxOwnerKind::Recovery)
                && self.ticket.is_some() =>
            {
                // Only the typed event is substituted. A real simulator ticket
                // has returned the genuinely retired original store.
                FirmwareOwnerEvent::LinuxRecoveryReturned {
                    summary,
                    store,
                    outcome,
                }
            }
            (
                FirmwareOwnerEvent::Rejected { store, .. },
                Some(ReturnedLinuxTestEvent::Completion(summary, outcome)),
            ) if self.linux_ticket == Some(ReturnedLinuxOwnerKind::Completion)
                && self.ticket.is_some() =>
            {
                FirmwareOwnerEvent::LinuxCompletionReturned {
                    summary,
                    store,
                    outcome,
                }
            }
            (event, _) => event,
        };
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        {
            self.linux_return = match &event {
                FirmwareOwnerEvent::LinuxRecoveryReturned {
                    summary,
                    store,
                    outcome,
                } if self.linux_ticket == Some(ReturnedLinuxOwnerKind::Recovery)
                    && self.ticket.is_some() =>
                {
                    serde_json::to_vec(outcome)
                        .ok()
                        .map(|bytes| ReturnedLinuxOwner {
                            summary: summary.clone(),
                            kind: ReturnedLinuxOwnerKind::Recovery,
                            outcome_sha256: JobSha256::of(&bytes).hex(),
                            root: store.root().to_path_buf(),
                        })
                }
                FirmwareOwnerEvent::LinuxCompletionReturned {
                    summary,
                    store,
                    outcome,
                } if self.linux_ticket == Some(ReturnedLinuxOwnerKind::Completion)
                    && self.ticket.is_some() =>
                {
                    serde_json::to_vec(outcome)
                        .ok()
                        .map(|bytes| ReturnedLinuxOwner {
                            summary: summary.clone(),
                            kind: ReturnedLinuxOwnerKind::Completion,
                            outcome_sha256: JobSha256::of(&bytes).hex(),
                            root: store.root().to_path_buf(),
                        })
                }
                _ => None,
            };
            self.linux_ticket = None;
        }
        self.ticket.take();
        Some(event)
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn take_linux_return(&mut self) -> Option<ReturnedLinuxOwner> {
        self.linux_return.take()
    }
    #[cfg(all(test, feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn test_substitute_rejected_recovery_return(
        &mut self,
        summary: FirmwareOwnerSummary,
        outcome: LinuxRecoveryOutcome,
    ) {
        assert!(
            self.linux_ticket == Some(ReturnedLinuxOwnerKind::Recovery) && self.ticket.is_some()
        );
        assert!(self.linux_return_test_event.is_none());
        self.linux_return_test_event =
            Some(ReturnedLinuxTestEvent::Recovery(summary, Box::new(outcome)));
    }
    #[cfg(all(test, feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn test_substitute_rejected_completion_return(
        &mut self,
        summary: FirmwareOwnerSummary,
        outcome: LinuxCompletionOutcome,
    ) {
        assert!(
            self.linux_ticket == Some(ReturnedLinuxOwnerKind::Completion) && self.ticket.is_some()
        );
        assert!(self.linux_return_test_event.is_none());
        self.linux_return_test_event = Some(ReturnedLinuxTestEvent::Completion(
            summary,
            Box::new(outcome),
        ));
    }
    pub(crate) fn summary(&self) -> Option<FirmwareOwnerSummary> {
        self.service.firmware_summary()
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn has_owner_ticket(&self) -> bool {
        self.ticket.is_some()
    }
    pub(crate) fn stop(&self) {
        if let Some(ticket) = &self.ticket {
            ticket.request_stop();
        }
    }
    pub(crate) fn refresh_submit(
        &self,
        command: DeviceCommand,
    ) -> Result<(), SubmitError<DeviceCommand>> {
        self.service.try_submit(command)
    }
}
impl Drop for FirmwareServiceEndpoint {
    fn drop(&mut self) {
        self.stop();
    }
}

struct NormalLibraryProxy {
    worker: Arc<LibraryWorker>,
    gate: Arc<RuntimeAdmission>,
}
impl LibraryService for NormalLibraryProxy {
    fn try_submit(&self, command: LibraryCommand) -> Result<(), SubmitError<LibraryCommand>> {
        let Ok(_offer) = self.gate.offers.try_lock() else {
            return Err(SubmitError::Busy(command));
        };
        if self.gate.frozen.load(Ordering::Acquire) {
            return Err(SubmitError::Busy(command));
        }
        self.worker.try_submit(command)
    }
    fn try_next_event(&self) -> Option<LibraryEvent> {
        self.worker.try_next_event()
    }
    fn shutdown(&self) {
        self.worker.shutdown();
    }
}
pub(crate) struct NormalWorkflowProxy {
    client: gp200_library::LibraryWorkflowClient,
    gate: Arc<RuntimeAdmission>,
}
impl NormalWorkflowProxy {
    pub(crate) fn try_submit(
        &self,
        command: gp200_library::WorkflowCommand,
    ) -> Result<(), SubmitError<gp200_library::WorkflowCommand>> {
        let Ok(_offer) = self.gate.offers.try_lock() else {
            return Err(SubmitError::Busy(command));
        };
        if self.gate.frozen.load(Ordering::Acquire) {
            return Err(SubmitError::Busy(command));
        }
        self.client.try_submit(command)
    }
    pub(crate) fn try_next_event(&self) -> Option<gp200_library::WorkflowEvent> {
        self.client.try_next_event()
    }
}
pub(crate) struct FirmwareLibraryEndpoint {
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) root: std::path::PathBuf,
    pub(crate) backup: FirmwareBackupClient,
    pub(crate) runtime: LibraryFirmwareRuntimeClient,
}
pub(crate) struct LibraryBundle {
    pub(crate) service: Box<dyn LibraryService>,
    pub(crate) workflows: NormalWorkflowProxy,
    pub(crate) endpoint: FirmwareLibraryEndpoint,
}
impl LibraryBundle {
    pub(crate) fn from_worker(mut worker: LibraryWorker, gate: Arc<RuntimeAdmission>) -> Self {
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        let root = worker.root().to_path_buf();
        let backup = worker
            .take_firmware_backup_client()
            .expect("new actual writer has its sole archival client");
        let runtime = worker
            .take_firmware_runtime_client()
            .expect("new actual writer has its sole runtime client");
        let workflows = NormalWorkflowProxy {
            client: worker.workflow_client(),
            gate: gate.clone(),
        };
        let service = Box::new(NormalLibraryProxy {
            worker: Arc::new(worker),
            gate,
        });
        Self {
            service,
            workflows,
            endpoint: FirmwareLibraryEndpoint {
                #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
                root,
                backup,
                runtime,
            },
        }
    }
}
