//! Recovery data is an offline description, never a device command or session ticket.
use crate::*;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::Arc};

pub const DRAFT_RECOVERY_SCHEMA: u32 = 1;
pub const MAX_RECOVERY_DOCUMENTS: usize = 32;
pub const MAX_RECOVERY_HISTORY: usize = 256;
pub const MAX_RECOVERY_JSON: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryUndo {
    pub past: Vec<PresetDraft>,
    pub future: Vec<PresetDraft>,
    pub gesture: Option<(GestureKey, PresetDraft)>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryUncertainty {
    /// Original intent is descriptive only and is never submitted after recovery.
    pub original_intent: u64,
    pub revision: Revision,
    pub description: String,
    pub patch: Option<EditPatch>,
    pub actual: Option<Arc<PresetSnapshot>>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryComparison {
    pub a: Option<PresetDraft>,
    pub b: Option<PresetDraft>,
    pub last_recalled: Option<char>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryDocument {
    pub original_id: DocumentId,
    /// This records where the work came from. It does not authorize restoring that target.
    pub original_target: EditTarget,
    pub baseline: Arc<PresetSnapshot>,
    pub draft: PresetDraft,
    pub local_revision: Revision,
    pub undo: RecoveryUndo,
    pub comparison: RecoveryComparison,
    pub uncertainty: Vec<RecoveryUncertainty>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoverySession {
    pub schema_version: u32,
    pub app_version: String,
    pub active_document: Option<DocumentId>,
    pub session_uncertainty: Vec<String>,
    pub documents: Vec<RecoveryDocument>,
}
impl RecoverySession {
    /// Reject excessive work instead of silently truncating exact history.
    pub fn validate(&self) -> Result<(), ServiceError> {
        if self.schema_version != DRAFT_RECOVERY_SCHEMA {
            return Err(ServiceError::unsupported(
                "Recovery schema is newer or unsupported; keep the file for a compatible version",
            ));
        }
        if self.app_version.is_empty()
            || self.app_version.len() > 64
            || self.documents.len() > MAX_RECOVERY_DOCUMENTS
            || self.session_uncertainty.len() > 256
            || self
                .session_uncertainty
                .iter()
                .any(|item| item.len() > 64 * 1024)
        {
            return Err(ServiceError::invalid(
                "Recovery document/version limit exceeded",
            ));
        }
        let mut ids = BTreeSet::new();
        for document in &self.documents {
            if document.original_id.0.is_empty()
                || document.original_id.0.len() > 256
                || !ids.insert(&document.original_id)
                || document.undo.past.len()
                    + document.undo.future.len()
                    + usize::from(document.undo.gesture.is_some())
                    > MAX_RECOVERY_HISTORY
                || document.uncertainty.len() > 256
                || !matches!(document.comparison.last_recalled, None | Some('A' | 'B'))
            {
                return Err(ServiceError::invalid(
                    "Recovery document/history structure is invalid or exceeds its limit",
                ));
            }
            let bytes =
                serde_json::to_vec(document).map_err(|e| ServiceError::invalid(e.to_string()))?;
            if bytes.len() > 4 * 1024 * 1024 {
                return Err(ServiceError::invalid(
                    "A recovery document exceeds 4 MiB; exact history was not truncated",
                ));
            }
        }
        if self
            .active_document
            .as_ref()
            .is_some_and(|id| !ids.contains(id))
        {
            return Err(ServiceError::invalid("Recovery active document is absent"));
        }
        if serde_json::to_vec(self)
            .map_err(|e| ServiceError::invalid(e.to_string()))?
            .len()
            > MAX_RECOVERY_JSON
        {
            return Err(ServiceError::invalid(
                "Recovery exceeds 16 MiB; exact drafts were not truncated",
            ));
        }
        Ok(())
    }
}
