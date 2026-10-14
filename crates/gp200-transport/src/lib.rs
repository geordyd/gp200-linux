//! Linux MIDI service and deterministic normal-mode device actor.
//!
//! The service owns the MIDI handles on a worker. Submission and event polling
//! never block the GUI. Normal framing has no transaction ID: an abandoned
//! request closes its session, advances the epoch, and cannot be replayed.

mod actor;
mod backend;
#[cfg(target_os = "linux")]
mod device_session_guard;
mod firmware_actor;
mod firmware_backend;
mod firmware_backup_capture;
#[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
mod firmware_entry_enumeration;
#[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
mod firmware_entry_study;
mod firmware_profile;
#[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
pub use firmware_entry_study::{
    EntryStudyAuthorization, EntryStudyDraft, EntryStudyFilePin, EntryStudyInputs, EntryStudyReview,
};
#[cfg(target_os = "linux")]
mod firmware_quarantine;
#[cfg(target_os = "linux")]
mod firmware_raw;
mod firmware_service;
#[cfg(all(test, target_os = "linux"))]
mod firmware_tests;
mod firmware_transition;
pub mod firmware_vendor_sequence;
mod model_profiles;
#[cfg(target_os = "linux")]
mod raw_backend;
mod service;
pub use backend::NormalReaderCloseReceipt;
pub use firmware_backend::FirmwareNormalHandoff;
pub use firmware_service::{
    FirmwareOwnerEvent, FirmwareOwnerSummary, FirmwareOwnerTicket, FirmwareOwnershipRequest,
    FirmwareStartupRetirementObservation,
};

pub use actor::{
    DeviceActor, TransportAction, TransportConfig, capabilities_for_mode,
    hardware_assignment_blocked_reason, hardware_control_verified, hardware_global_verified,
    hardware_model_blocked_reason, hardware_model_blocked_reason_with_globals,
    hardware_sync_verified, verified_capabilities,
};
pub use backend::{
    MidiPortDescriptor, MidiPortInventory, MidiSession, MidirBackend, ReceiveSink, TransportBackend,
};
pub use model_profiles::{CapturedModelProfile, captured_model_profile, captured_model_profiles};
#[cfg(target_os = "linux")]
pub use raw_backend::AlsaRawBackend;
pub use service::MidiDeviceService;

pub use firmware_backup_capture::{
    FirmwareBackupActionDescription, FirmwareBackupCaptureClient, FirmwareBackupCaptureCommand,
    FirmwareBackupCaptureEvent, FirmwareCaptureWriteResult, FirmwareNormalCaptureObservation,
};

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
mod firmware_linux_completion;
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
mod firmware_linux_completion_admission;
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
mod firmware_linux_completion_binding;
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
mod firmware_linux_completion_io;
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
mod firmware_linux_completion_journal;
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
mod firmware_linux_recovery;
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
mod firmware_linux_recovery_admission;
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
pub use firmware_linux_completion::{
    LinuxCompletionMonitor, LinuxCompletionOutcome, LinuxCompletionPhase, LinuxCompletionView,
};
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
pub use firmware_linux_completion_admission::{
    LinuxCompletionAuthorization, LinuxCompletionDraft, LinuxCompletionInputs,
    LinuxCompletionReview,
};
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
pub use firmware_linux_recovery::{
    LinuxFirmwareHistoryCatalogue, LinuxFirmwareHistoryRow, LinuxRecoveryHistoryReview,
    LinuxRecoveryOutcome, LinuxRecoveryRequest, review_linux_firmware_history,
    review_linux_firmware_history_at, review_linux_recovery_history,
};
