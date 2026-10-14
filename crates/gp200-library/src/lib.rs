//! Durable, lossless offline preset library. This crate never opens MIDI.
//!
//! [`LibraryWorker`] implements the bounded asynchronous domain service.
//! [`LibraryStore`] is the synchronous bootstrap/test/native-file adapter API;
//! callers must keep it off the GUI thread after opening.

mod backup_archive;
mod bundle;
mod dependencies;
mod device_backup;
mod diff;
mod draft_recovery;
mod firmware_attempt_history;
mod firmware_attempt_ledger;
mod firmware_backup_capture_evidence;
mod firmware_backup_review;
mod firmware_backup_worker;
mod firmware_ledger_worker;
mod firmware_runtime_owner;
mod ir_attempt_history;
mod ir_attempt_ledger;
mod merge;
mod native;
mod native_ir_reference;
mod preset_movement;
mod preset_movement_journal;
mod preset_movement_review;
mod restore;
mod setlists;
mod store;
mod worker;
mod workflow_worker;
mod workflows;

pub use bundle::{BundleLimits, BundlePreview};
pub use dependencies::{AssetDependencyOwner, AssetDependencyUse, AssetReplacementImpact};
pub use device_backup::*;
pub use diff::{
    DependencyReport, KnownChange, ParameterChange, PresetDiff, RawChange, compare_presets,
    dependency_report,
};
pub use draft_recovery::*;
pub use firmware_attempt_history::*;
pub use firmware_attempt_ledger::*;
pub use firmware_backup_capture_evidence::*;
pub use firmware_backup_review::*;
pub use firmware_backup_worker::*;
pub use firmware_ledger_worker::*;
pub use firmware_runtime_owner::*;
pub use ir_attempt_history::*;
pub use ir_attempt_ledger::*;
pub use merge::*;
pub use native::{NativeFile, NativeFileLimits, RejectedNativeSource, native_blob_crc32};
pub use native_ir_reference::{
    NativeIrMetadataReference, NativeIrOpaqueReason, NativeIrReferenceInspection,
    NativeIrReferenceIntegrity, NativeIrReferenceOccupancy, inspect_native_ir_reference,
};
pub use preset_movement::*;
pub use preset_movement_journal::*;
pub use preset_movement_review::*;
pub use restore::*;
pub use setlists::*;
pub use store::{LibraryStore, OriginalSource, SCHEMA_VERSION, StoredAsset, data_dir};
pub use worker::{LibraryWorker, LibraryWorkflowClient};
pub use workflow_worker::{WorkflowAction, WorkflowCommand, WorkflowEvent, WorkflowResult};
pub use workflows::ContentHash;
#[cfg(test)]
extern crate self as gp200_library;
