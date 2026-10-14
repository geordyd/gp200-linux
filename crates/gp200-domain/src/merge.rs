//! Typed selections for an explicit, reviewed OFFLINE merge. These carry no
//! device target, session authority, or hardware edit commands.
use crate::{DocumentId, ExpressionSlotId, ModelCode, ModuleId, ParamId, PresetSnapshot, Revision};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MergeMetadataField {
    Name,
    Tempo,
    Level,
    Author,
    Style,
    Notes,
    PanAnnotation,
    NativePan,
    NativeStyle,
    FxSendLevel,
    FxReturnLevel,
    FxLoopMode,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum OfflineMergeField {
    Metadata(MergeMetadataField),
    Chain,
    /// State, model and every retained parameter slot, including undeclared
    /// float bits. Opaque record bytes and target record positions stay local.
    Module(ModuleId),
    Enabled(ModuleId),
    Parameter {
        module: ModuleId,
        expected_model: ModelCode,
        parameter: ParamId,
    },
    Expression(ExpressionSlotId),
    Ctrl(u8),
    Quick(u8),
    FxLoopPositions,
    LegacyExpressions,
    LegacyFootswitches,
    LegacyQuickControls,
}

/// Produced by the library review. The reducer verifies all hashes and
/// regenerates the preview against the current document before applying.
#[derive(Debug, Clone)]
pub struct OfflineMergeRequest {
    pub document: DocumentId,
    pub expected_revision: Revision,
    pub target_sha256: [u8; 32],
    pub source: Arc<PresetSnapshot>,
    pub source_sha256: [u8; 32],
    pub fields: Vec<OfflineMergeField>,
    pub result_sha256: [u8; 32],
    pub review_sha256: [u8; 32],
}
