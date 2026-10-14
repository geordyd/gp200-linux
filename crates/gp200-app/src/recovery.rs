use crate::{
    AppController, BufferPersistence, ComparisonSlot, ComparisonState, MessageSeverity,
    PendingRequest,
};
use gp200_domain::*;
use std::sync::Arc;

impl AppController {
    /// Snapshot only editing data. Ports, capabilities, prepared jobs, pending
    /// request tables, globals and connection authority are deliberately absent.
    pub fn recovery_session(&self) -> RecoverySession {
        RecoverySession {
            schema_version: DRAFT_RECOVERY_SCHEMA,
            app_version: env!("CARGO_PKG_VERSION").into(),
            active_document: self.state.active_document.clone(),
            session_uncertainty: {
                let mut retained = self.state.recovered_session_uncertainty.clone();
                for (intent, request) in &self.requests {
                    let operation = match request {
                        PendingRequest::Save { slot, .. } => format!("Save to slot {slot}"),
                        PendingRequest::WriteStored { slot, .. } => format!("Stored write to slot {slot}"),
                        PendingRequest::GlobalControl { .. } => "Global settings write".into(),
                        PendingRequest::Global => "Global settings write (legacy projection)".into(),
                        PendingRequest::Select { slot, .. } => format!("Select slot {slot}"),
                        PendingRequest::DeviceJob(job) => format!("Device asset job {}", job.0),
                        _ => continue,
                    };
                    retained.push(format!("{operation} was pending at snapshot (epoch {}, intent {}). Result is unknown after restart; read manually.", self.state.epoch.0, intent.0));
                }
                if let BufferPersistence::Indeterminate { slot, reason } = &self.state.buffer_persistence {
                    retained.push(format!("Persistent slot {slot} result is indeterminate: {reason}"));
                }
                retained
            },
            documents: self.state.documents.values().map(|document| {
                let comparison = self.state.comparisons.get(&document.id);
                let mut uncertainty: Vec<_> = document.pending.iter().filter_map(|(intent, pending)| {
                    let description = match &pending.stage {
                        EditStage::Queued => "Queued at snapshot; delivery is unknown after restart".into(),
                        EditStage::Sent => "Sent at snapshot; pedal result is unknown after restart".into(),
                        EditStage::AwaitingReadback => "Awaiting readback at snapshot; pedal result is unknown after restart".into(),
                        EditStage::Indeterminate(error) => format!("Indeterminate: {}", error.message),
                        EditStage::Failed(error) => format!("Failed: {}", error.message),
                        EditStage::Verified => return None,
                    };
                    Some(RecoveryUncertainty { original_intent: intent.0, revision: pending.revision, description, patch: Some(pending.patch.clone()), actual: None })
                }).collect();
                uncertainty.extend(document.conflicts.iter().map(|conflict| RecoveryUncertainty {
                    original_intent: conflict.intent.0, revision: document.local_revision,
                    description: format!("Unresolved conflict: {}", conflict.description), patch: None,
                    actual: Some(conflict.actual.clone()),
                }));
                if let Some(retained) = self.state.recovered_uncertainty.get(&document.id) {
                    uncertainty.extend(retained.iter().cloned());
                }
                RecoveryDocument {
                    original_id: document.id.clone(), original_target: document.target.clone(),
                    baseline: document.baseline.clone(), draft: document.draft.clone(),
                    local_revision: document.local_revision,
                    undo: RecoveryUndo { past: document.undo.past.clone(), future: document.undo.future.clone(), gesture: document.undo.gesture.clone() },
                    comparison: RecoveryComparison {
                        a: comparison.and_then(|c| c.snapshots.get(&ComparisonSlot::A)).cloned(),
                        b: comparison.and_then(|c| c.snapshots.get(&ComparisonSlot::B)).cloned(),
                        last_recalled: comparison.and_then(|c| c.last_recalled).map(|s| if s == ComparisonSlot::A { 'A' } else { 'B' }),
                    }, uncertainty,
                }
            }).collect(),
        }
    }
    /// Explicitly recover into new OFFLINE documents. This emits no service
    /// effects and never imports live session, target, intent or Save authority.
    pub fn recover_offline(
        &mut self,
        session: &RecoverySession,
    ) -> Result<Vec<DocumentId>, ServiceError> {
        self.recover_offline_replacing_pristine(session, None)
    }
    pub fn can_replace_pristine_offline(&self, id: &DocumentId) -> bool {
        self.state.documents.get(id).is_some_and(|document| {
            matches!(document.target, EditTarget::Offline(_))
                && !document.dirty()
                && document.undo.past.is_empty()
                && document.undo.future.is_empty()
                && document.undo.gesture.is_none()
                && document.pending.is_empty()
                && document.conflicts.is_empty()
        }) && self
            .state
            .comparisons
            .get(id)
            .is_none_or(|comparison| comparison.snapshots.is_empty())
            && self
                .state
                .recovered_uncertainty
                .get(id)
                .is_none_or(Vec::is_empty)
    }
    /// The review may explicitly replace its untouched bootstrap capture.
    /// Validate again here so an edit made while the review was open is kept.
    pub fn recover_offline_replacing_pristine(
        &mut self,
        session: &RecoverySession,
        replace: Option<&DocumentId>,
    ) -> Result<Vec<DocumentId>, ServiceError> {
        if self.firmware_editor_blocks_normal_actions() {
            return Err(Self::firmware_editor_busy());
        }
        session.validate()?;
        if replace.is_some_and(|id| !self.can_replace_pristine_offline(id)) {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "The startup document gained work while recovery was reviewed. Uncheck replacement to keep it.",
            ));
        }
        if self.state.documents.len() + session.documents.len() - usize::from(replace.is_some())
            > MAX_RECOVERY_DOCUMENTS
        {
            return Err(ServiceError::invalid(
                "Close documents before recovering this session; the 32-document bound would be exceeded",
            ));
        }
        if self.state.recovered_session_uncertainty.len() + session.session_uncertainty.len() > 256
        {
            return Err(ServiceError::invalid(
                "Recovery session uncertainty limit would be exceeded; existing work was kept",
            ));
        }
        if let Some(id) = replace {
            self.state.documents.remove(id);
            self.state.comparisons.remove(id);
            self.state.recovered_uncertainty.remove(id);
            if self.state.active_document.as_ref() == Some(id) {
                self.state.active_document = None;
            }
        }
        let mut ids = Vec::new();
        let mut active = None;
        self.state
            .recovered_session_uncertainty
            .extend(session.session_uncertainty.iter().cloned());
        for recovered in &session.documents {
            let id = loop {
                let candidate = DocumentId(format!("recovered-{}", self.next_document));
                self.next_document += 1;
                if !self.state.documents.contains_key(&candidate) {
                    break candidate;
                }
            };
            let mut baseline = (*recovered.baseline).clone();
            baseline.source = SnapshotSource::Capture {
                capture_id: format!("offline-recovery:{}", recovered.original_id.0),
            };
            baseline.target = None;
            baseline.context = None;
            let mut document = EditorDocument::new(
                id.clone(),
                EditTarget::Offline(id.clone()),
                Arc::new(baseline),
            );
            document.draft = recovered.draft.clone();
            document.local_revision = recovered.local_revision;
            document.undo = UndoHistory {
                past: recovered.undo.past.clone(),
                future: recovered.undo.future.clone(),
                gesture: None,
            };
            // A crash can interrupt a drag. Seal its exact before image into an
            // undo step rather than leaving a gesture tied to a lost widget.
            if let Some((_, before)) = &recovered.undo.gesture
                && before != &document.draft
            {
                document.undo.past.push(before.clone());
            }
            let mut comparison = ComparisonState::default();
            if let Some(a) = &recovered.comparison.a {
                comparison.snapshots.insert(ComparisonSlot::A, a.clone());
            }
            if let Some(b) = &recovered.comparison.b {
                comparison.snapshots.insert(ComparisonSlot::B, b.clone());
            }
            comparison.last_recalled = recovered.comparison.last_recalled.map(|c| {
                if c == 'A' {
                    ComparisonSlot::A
                } else {
                    ComparisonSlot::B
                }
            });
            self.state.comparisons.insert(id.clone(), comparison);
            self.state
                .recovered_uncertainty
                .insert(id.clone(), recovered.uncertainty.clone());
            if !recovered.uncertainty.is_empty() {
                self.state.message(MessageSeverity::Warning,
                    format!("Recovered {} offline with {} uncertain operation(s). Read the pedal manually before deciding whether any device change is needed.", document.draft.view.display_name(), recovered.uncertainty.len()), None);
            }
            if session.active_document.as_ref() == Some(&recovered.original_id) {
                active = Some(id.clone());
            }
            self.state.documents.insert(id.clone(), document);
            ids.push(id);
        }
        if let Some(id) = active.or_else(|| ids.first().cloned()) {
            self.state.active_document = Some(id);
        }
        self.state.message(
            MessageSeverity::Info,
            format!(
                "Recovered {} document(s) offline; no pedal operation was submitted",
                ids.len()
            ),
            None,
        );
        Ok(ids)
    }
}
