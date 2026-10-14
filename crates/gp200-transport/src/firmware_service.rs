//! Separate, bounded ownership rail; no normal command encodes firmware work.
use gp200_domain::*;
use gp200_library::LibraryStore;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
#[derive(Default)]
pub(crate) struct OwnerStop {
    pub(crate) requested: AtomicBool,
    pub(crate) gate: Mutex<()>,
}
/// Non-Clone owning cancellation ticket. Drop only signals; it never joins.
/// ```compile_fail
/// fn clone(t: gp200_transport::FirmwareOwnerTicket) { let _ = t.clone(); }
/// ```
pub struct FirmwareOwnerTicket {
    pub(crate) stop: Arc<OwnerStop>,
}
impl FirmwareOwnerTicket {
    pub fn request_stop(&self) {
        self.stop.requested.store(true, Ordering::SeqCst);
    }
}
impl Drop for FirmwareOwnerTicket {
    fn drop(&mut self) {
        self.request_stop();
    }
}
/// Local inputs are descriptive. A public request cannot issue a physical profile.
pub struct FirmwareOwnershipRequest {
    pub(crate) job: Arc<ReviewedFirmwareUpdate>,
    pub(crate) store: LibraryStore,
    pub(crate) stop: Arc<OwnerStop>,
    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
    pub(crate) entry_study: Option<crate::firmware_entry_study::EntryStudyAuthorization>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) linux_completion: Option<crate::LinuxCompletionAuthorization>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) linux_monitor: crate::LinuxCompletionMonitor,
    #[cfg(test)]
    pub(crate) mock: Option<(
        crate::firmware_profile::MockFirmwareAuthority,
        std::path::PathBuf,
    )>,
}
impl FirmwareOwnershipRequest {
    pub(crate) fn purpose(&self) -> crate::firmware_profile::FirmwarePurpose {
        #[cfg(test)]
        if let Some((authority, _)) = &self.mock {
            return authority.purpose();
        }
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        if self.linux_completion.is_some() {
            return crate::firmware_profile::FirmwarePurpose::NormalEntryObservation;
        }
        #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
        if self.entry_study.is_some() {
            return crate::firmware_profile::FirmwarePurpose::NormalEntryObservation;
        }
        crate::firmware_profile::FirmwarePurpose::BootTransfer
    }
    pub fn new(job: Arc<ReviewedFirmwareUpdate>, store: LibraryStore) -> Self {
        Self {
            job,
            store,
            stop: Arc::new(OwnerStop::default()),
            #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
            entry_study: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            linux_completion: None,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            linux_monitor: crate::LinuxCompletionMonitor::default(),
            #[cfg(test)]
            mock: None,
        }
    }
    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
    pub fn entry_study(
        authorization: crate::firmware_entry_study::EntryStudyAuthorization,
        store: LibraryStore,
    ) -> Self {
        let job = authorization.job.clone();
        let mut request = Self::new(job, store);
        request.entry_study = Some(authorization);
        request
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub fn linux_completion(
        authorization: crate::LinuxCompletionAuthorization,
        store: LibraryStore,
    ) -> (Self, crate::LinuxCompletionMonitor) {
        let job = authorization.job.clone();
        let mut request = Self::new(job, store);
        request.linux_completion = Some(authorization);
        let monitor = request.linux_monitor.clone();
        (request, monitor)
    }
    /// Consume an unoffered request and recover its exact original local owner.
    /// No replacement store is opened and no transfer ticket is reconstructed.
    pub fn into_store(self) -> LibraryStore {
        self.store
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareOwnerSummary {
    pub phase: String,
    pub simulated_source_bytes: u64,
    pub os_offered_bytes: u64,
    pub device_verified_bytes: u64,
    pub completion_verified: bool,
    pub quarantined: bool,
    pub diagnostic: Option<String>,
    /// Bounded observations not claimed durably journalled. Missing allocated chunks remain unknown.
    pub retained_raw_chunks: Vec<FirmwareLifecycleRawObservation>,
    pub allocated_raw_frontier: u64,
}
/// Actual startup teardown. Retained raw callbacks remain original observations,
/// not journal acknowledgements or proof of device completion.
#[derive(Debug)]
pub struct FirmwareStartupRetirementObservation {
    pub source: FirmwareLifecycleSourceRetirement,
    pub reader_fault: Option<ServiceError>,
    pub output_handle_destroyed: bool,
    pub retained_raw_chunks: Vec<FirmwareLifecycleRawObservation>,
}
/// Actual store ownership is returned after a real ledger join, or when startup
/// never transferred that store to an actual ledger thread.
pub enum FirmwareOwnerEvent {
    StartupFailed {
        error: ServiceError,
    },
    StartupFailedWithStore {
        error: ServiceError,
        store: LibraryStore,
        normal_retirement: Option<Box<FirmwareNormalRetirementObservation>>,
        updater_retirement: Option<Box<FirmwareStartupRetirementObservation>>,
    },
    Rejected {
        error: ServiceError,
        store: LibraryStore,
    },
    RejectedAfterNormalRetirement {
        error: ServiceError,
        store: LibraryStore,
        observation: Box<FirmwareNormalRetirementObservation>,
    },
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    LinuxCompletionReturned {
        summary: FirmwareOwnerSummary,
        store: LibraryStore,
        outcome: Box<crate::LinuxCompletionOutcome>,
    },
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    LinuxRecoveryReturned {
        summary: FirmwareOwnerSummary,
        store: LibraryStore,
        outcome: Box<crate::LinuxRecoveryOutcome>,
    },
    Retired {
        summary: FirmwareOwnerSummary,
        store: LibraryStore,
    },
}
