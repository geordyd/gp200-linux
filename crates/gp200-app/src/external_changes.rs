//! Context-free receive data is an invalidation hint, never write authority.
use crate::{
    AppController, BoundModelPlan, BufferPersistence, MessageSeverity, PendingRequest,
    device_document_id,
};
use gp200_domain::*;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefreshKind {
    Current,
    Globals,
}

#[cfg(all(test, gp200_private_evidence))]
mod history_isolation_tests {
    use super::*;
    use gp200_catalog::Catalog;

    #[test]
    fn a_second_representable_buffer_document_cannot_consume_another_history_chain() {
        // Synthetic AppState arrangement. Ordinary UI constructors create one
        // canonical buffer, but the public state model can represent a second.
        let before = RawTransfer::from_payload(0x12, include_bytes!(
            "../../../analysis/implementation/dependent-model-normal-root/amp-auto-cab-off/rust/own-fresh-current-before.payload.bin"
        ).to_vec()).unwrap();
        let reverse = RawTransfer::from_payload(0x12, include_bytes!(
            "../../../analysis/implementation/dependent-model-normal-root/amp-auto-cab-off/rust/undo-60-6.preset.payload.bin"
        ).to_vec()).unwrap();
        let global_raw = RawTransfer::from_payload(0x12, include_bytes!(
            "../../../analysis/implementation/dependent-model-normal-root/amp-auto-cab-off/rust/undo-63-6.globals.payload.bin"
        ).to_vec()).unwrap();
        let snapshot = |raw, revision| {
            Arc::new(PresetSnapshot::from_raw(
                Arc::new(raw),
                SnapshotSource::Hardware {
                    epoch: SessionEpoch(1),
                },
                Some(PresetTarget::EditBuffer),
                Revision(revision),
            ))
        };
        let before = snapshot(before, 1);
        let reverse = snapshot(reverse, 5);
        let globals = Arc::new(
            DeviceGlobalState::from_transfer(Revision(3), SessionEpoch(1), global_raw, vec![])
                .unwrap(),
        );
        let id = device_document_id();
        let other = DocumentId::from("another-buffer-target");
        let mut app = AppController::new(Catalog::embedded().unwrap(), AppPreferences::default());
        app.state.epoch = SessionEpoch(1);
        app.state.connection = ConnectionState::Ready;
        app.state.identity = Some(DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Hardware,
            firmware: None,
            display_name: "Synthetic second-buffer state".into(),
        });
        app.state.capabilities = gp200_transport::verified_capabilities();
        app.state.confirmed_buffer = Some(reverse.clone());
        app.state.device_globals = Some(globals.clone());
        let document = EditorDocument::new(id.clone(), EditTarget::DeviceBuffer, before.clone());
        let desired = document.draft.clone();
        let plan = DependentModelPlan {
            profile: DependentModelProfile::AmpAutoCabOff,
            module: ModuleId::new(3).unwrap(),
            previous: ModelCode(0x07000001),
            target: ModelCode(0x07000015),
            auto_cab: false,
            paired_cabinet: None,
        };
        let remaining = app.delta(&desired, &reverse);
        assert_eq!(remaining.len(), 4);
        app.history_continuation = Some(HistoryContinuation {
            epoch: SessionEpoch(1),
            document: id.clone(),
            local_revision: document.local_revision,
            baseline: before,
            desired,
            plan: BoundModelPlan::Existing(plan),
            owned: None,
            remaining: remaining.clone(),
            confirmed: Some(reverse.clone()),
            globals: Some(globals),
        });
        app.state.documents.insert(id, document);
        app.state.documents.insert(
            other.clone(),
            EditorDocument::new(other.clone(), EditTarget::DeviceBuffer, reverse),
        );
        let effects = app.dispatch(AppInput::Ui(UiIntent::ApplyEdit {
            document: other,
            patch: EditPatch::SetEnabled {
                module: ModuleId::new(0).unwrap(),
                enabled: true,
            },
        }));
        assert!(matches!(
            effects.as_slice(),
            [Effect::Device(DeviceCommand::SubmitEdit { .. })]
        ));
        let continuation = app.history_continuation.as_ref().unwrap();
        assert_eq!(continuation.document, device_document_id());
        assert_eq!(continuation.owned, None);
        assert_eq!(continuation.remaining, remaining);
    }
}

#[derive(Debug, Default)]
pub(crate) struct ExternalRefresh {
    epoch: SessionEpoch,
    pending: u8,
    active: Option<(RequestStamp, RefreshKind)>,
    not_before_ms: u64,
    minimum_next_ms: u64,
}

impl ExternalRefresh {
    pub(crate) fn reset(&mut self, epoch: SessionEpoch) {
        *self = Self {
            epoch,
            ..Self::default()
        };
    }

    pub(crate) fn note(&mut self, epoch: SessionEpoch, raw: &RawTransfer, now_ms: u64) -> bool {
        if epoch != self.epoch {
            return false;
        }
        let hints = notification_hints(raw);
        if hints == 0 {
            return false;
        }
        if self.pending == 0 {
            self.not_before_ms = now_ms.saturating_add(100).max(self.minimum_next_ms);
        }
        self.pending |= hints;
        true
    }

    pub(crate) fn due(&self, now_ms: u64) -> Option<RefreshKind> {
        if self.active.is_some() || now_ms < self.not_before_ms.max(self.minimum_next_ms) {
            return None;
        }
        if self.pending & 1 != 0 {
            Some(RefreshKind::Current)
        } else if self.pending & 2 != 0 {
            Some(RefreshKind::Globals)
        } else {
            None
        }
    }

    pub(crate) fn started(&mut self, stamp: RequestStamp, kind: RefreshKind, now_ms: u64) {
        self.pending &= !(if kind == RefreshKind::Current { 1 } else { 2 });
        self.active = Some((stamp, kind));
        self.minimum_next_ms = now_ms.saturating_add(500);
    }

    pub(crate) fn finished(&mut self, stamp: RequestStamp, success: bool) {
        if self.active.is_some_and(|(active, _)| active == stamp) {
            self.active = None;
            if !success {
                self.pending = 0;
            }
        }
    }

    pub(crate) fn invalidated(&self) -> bool {
        self.pending != 0 || self.active.is_some()
    }

    pub(crate) fn abandon(&mut self, kind: RefreshKind) {
        self.pending &= !(if kind == RefreshKind::Current { 1 } else { 2 });
    }
}

fn notification_hints(raw: &RawTransfer) -> u8 {
    if raw.kind() != 0x12 {
        return 0;
    }
    raw.records().iter().fold(0, |hints, record| {
        let current = match (record.command, record.body.len()) {
            (2, 88) | (8, 16) => u16::from_le_bytes([record.body[0], record.body[1]]) < 256,
            (0x0c, 12) | (0x0f, 8) | (0x10, 4) => true,
            (5, 12) => record.body[0] < 11 && record.body[1] < 15,
            (0x14, 68) | (0x15, 4) | (0x16, 8) => record.body[0] < 11,
            (6, 4) => u16::from_le_bytes([record.body[0], record.body[1]]) <= 6,
            (7 | 0x2003, 20) | (0x2009, 4) => {
                u16::from_le_bytes([record.body[0], record.body[1]]) < 256
            }
            (0x1008, 4) => record.body[..2] == [0, 0],
            _ => false,
        };
        let globals = matches!(
            (record.command, record.body.len()),
            (0x1000, 6)
                | (0x1001, 4)
                | (0x1002, 8)
                | (0x1004, 124)
                | (0x1005, 8)
                | (0x1007, 72)
                | (0x1008, 4)
                | (0x100e, 40)
                | (0x100f, 4)
                | (0x1010, 4)
                | (0x1011, 4)
        ) || (record.command == 0x000a
            && record.body.len() == 20
            && u16::from_le_bytes([record.body[0], record.body[1]]) < 20);
        hints | u8::from(current) | (u8::from(globals) << 1)
    })
}

pub(crate) fn mutation_operation(operation: OperationKind) -> bool {
    !matches!(
        operation,
        OperationKind::Identify
            | OperationKind::ReadEditBuffer
            | OperationKind::ReadStoredPreset
            | OperationKind::ReadGlobals
            | OperationKind::PresetExport
            | OperationKind::IrRead
            | OperationKind::SnapToneRead
    )
}

#[derive(Debug, Clone)]
pub(crate) struct ExternalReview {
    epoch: SessionEpoch,
    document: DocumentId,
    local_revision: Revision,
    baseline: Arc<PresetSnapshot>,
    actual: Arc<PresetSnapshot>,
}

/// A submitted dependent-model history action can need original parameter bits
/// after its verified model reversal. This ephemeral token owns that one chain;
/// ordinary unsent edits and failed/uncertain writes never acquire it.
#[derive(Debug, Clone)]
pub(crate) struct HistoryContinuation {
    epoch: SessionEpoch,
    pub(crate) document: DocumentId,
    local_revision: Revision,
    baseline: Arc<PresetSnapshot>,
    desired: PresetDraft,
    plan: BoundModelPlan,
    pub(crate) owned: Option<IntentId>,
    pub(crate) remaining: Vec<EditPatch>,
    confirmed: Option<Arc<PresetSnapshot>>,
    globals: Option<Arc<DeviceGlobalState>>,
}

// Exact raw comparison, with only the already accepted volatile-byte exclusions.
fn same_complete_state(a: &PresetSnapshot, b: &PresetSnapshot) -> bool {
    a.context == b.context
        && a.raw.kind() == b.raw.kind()
        && a.raw.records().len() == b.raw.records().len()
        && a.raw.records().iter().zip(b.raw.records()).all(|(a, b)| {
            a.command == b.command
                && a.body.len() == b.body.len()
                && a.body.iter().zip(&b.body).enumerate().all(|(i, (x, y))| {
                    x == y
                        || match (a.command, a.body.len()) {
                            (2, 88) | (8, 16) => i == 15,
                            (0x0f, 8) => matches!(i, 2 | 3),
                            _ => false,
                        }
                })
        })
}

fn has_retained_work(document: &EditorDocument) -> bool {
    document.dirty()
        || !document.undo.past.is_empty()
        || !document.undo.future.is_empty()
        || document.undo.gesture.is_some()
        || !document.conflicts.is_empty()
        || document
            .pending
            .values()
            .any(|pending| !matches!(pending.stage, EditStage::Verified))
}

impl AppController {
    /// A factory history barrier requires a newly observed owned current read.
    /// Equal/older revisions leave the pending read and invalidation intact.
    pub(crate) fn factory_history_read_is_stale(&self, incoming: &PresetSnapshot) -> bool {
        self.history_continuation
            .as_ref()
            .is_some_and(|continuation| {
                matches!(continuation.plan, BoundModelPlan::Factory { .. })
                    && continuation.epoch == self.state.epoch
                    && self
                        .state
                        .confirmed_buffer
                        .as_ref()
                        .is_some_and(|current| incoming.revision <= current.revision)
            })
    }

    pub(crate) fn begin_history_continuation(&mut self, document: &DocumentId, effects: &[Effect]) {
        let [Effect::Device(command)] = effects else {
            return;
        };
        let stamp = match command {
            DeviceCommand::SubmitDependentModel { stamp, .. }
            | DeviceCommand::SubmitFactoryAmpModel { stamp, .. } => stamp,
            _ => return,
        };
        let Some(PendingRequest::Edit {
            dependent: Some(dependent),
            ..
        }) = self.requests.get(&stamp.intent)
        else {
            return;
        };
        let Some(document) = self.state.documents.get(document) else {
            return;
        };
        self.history_continuation = Some(HistoryContinuation {
            epoch: stamp.epoch,
            document: document.id.clone(),
            local_revision: document.local_revision,
            baseline: document.baseline.clone(),
            desired: document.draft.clone(),
            plan: dependent.plan.clone(),
            owned: Some(stamp.intent),
            remaining: Vec::new(),
            confirmed: None,
            globals: None,
        });
    }

    fn history_binding_valid(&self, continuation: &HistoryContinuation) -> bool {
        self.state.epoch == continuation.epoch
            && self.state.connection == ConnectionState::Ready
            && self
                .state
                .documents
                .get(&continuation.document)
                .is_some_and(|document| {
                    document.target == EditTarget::DeviceBuffer
                        && document.local_revision == continuation.local_revision
                        && Arc::ptr_eq(&document.baseline, &continuation.baseline)
                        && document.draft.semantically_equal(&continuation.desired)
                        && document.conflicts.is_empty()
                })
    }

    /// Returns true for the owned history step even when its binding expired,
    /// preventing the ordinary model followup path from reviving old deltas.
    pub(crate) fn history_step_verified(&mut self, stamp: RequestStamp) -> bool {
        let Some(mut continuation) = self.history_continuation.take() else {
            return false;
        };
        if continuation.owned != Some(stamp.intent) {
            self.history_continuation = Some(continuation);
            return false;
        }
        // The offered step can settle, but its remaining chain and original
        // binding stay visible while updater admission is frozen.
        if self.firmware_editor_blocks_normal_actions() {
            if self.history_binding_valid(&continuation)
                && let (Some(confirmed), Some(globals)) =
                    (&self.state.confirmed_buffer, &self.state.device_globals)
            {
                if continuation.confirmed.is_none() {
                    continuation.remaining = self.delta(&continuation.desired, confirmed);
                }
                continuation.confirmed = Some(confirmed.clone());
                continuation.globals = Some(globals.clone());
            }
            continuation.owned = None;
            self.history_continuation = Some(continuation);
            return true;
        }
        self.deferred.remove(&continuation.document);
        if !self.history_binding_valid(&continuation) {
            return true;
        }
        let (Some(confirmed), Some(globals)) =
            (&self.state.confirmed_buffer, &self.state.device_globals)
        else {
            return true;
        };
        if continuation.confirmed.is_none() {
            let remaining = self.delta(&continuation.desired, confirmed);
            // One model history entry can restore only its affected declared
            // controls. Any other local/model/assignment delta cancels the token.
            if !remaining.iter().all(|patch| {
                matches!(patch,
                EditPatch::SetParameter { module, model, .. }
                if (*module == continuation.plan.module() && *model == continuation.plan.target())
                    || (module.get() == 5 && continuation.plan.paired_cabinet() == Some(*model)))
            }) {
                return true;
            }
            continuation.remaining = remaining;
        }
        continuation.owned = None;
        continuation.confirmed = Some(confirmed.clone());
        continuation.globals = Some(globals.clone());
        if !continuation.remaining.is_empty() {
            self.history_continuation = Some(continuation);
        }
        true
    }

    pub(crate) fn resume_history_continuation(&mut self) -> Vec<Effect> {
        if self.firmware_editor_blocks_normal_actions() {
            return Vec::new();
        }
        let Some(continuation) = self.history_continuation.as_ref().cloned() else {
            return Vec::new();
        };
        if !self.history_binding_valid(&continuation) {
            self.history_continuation = None;
            self.deferred.remove(&continuation.document);
            return Vec::new();
        }
        if continuation.owned.is_some()
            || self.external_write_barrier()
            || self.device_operation_pending()
        {
            return Vec::new();
        }
        let current_matches = continuation
            .confirmed
            .as_ref()
            .zip(self.state.confirmed_buffer.as_ref())
            .is_some_and(|(expected, actual)| same_complete_state(expected, actual));
        let globals_match = continuation
            .globals
            .as_ref()
            .zip(self.state.device_globals.as_ref())
            .is_some_and(|(expected, actual)| {
                dependent_globals_match(expected.records().raw(), actual.records().raw(), None)
                    .unwrap_or(false)
            });
        if !current_matches || !globals_match || self.blocked_after_uncertainty {
            self.history_continuation = None;
            self.deferred.remove(&continuation.document);
            return Vec::new();
        }
        let Some(patch) = continuation.remaining.first() else {
            return Vec::new();
        };
        // Recreate one never-submitted declared step only after the complete
        // unchanged recovery has retired every hint. No pending edit is replayed.
        self.deferred
            .insert(continuation.document.clone(), vec![patch.clone()]);
        self.flush(&continuation.document)
    }

    /// Host scheduler status, not evidence that this firmware emits push updates.
    pub fn external_reconciliation_pending(&self) -> bool {
        self.external_refresh.invalidated()
    }

    pub fn external_conflict(&self, intent: IntentId) -> bool {
        self.external_reviews.contains_key(&intent)
    }

    pub fn external_conflict_ready(&self, intent: IntentId) -> bool {
        !self.firmware_editor_blocks_normal_actions()
            && self
                .external_reviews
                .get(&intent)
                .is_some_and(|review| self.external_review_valid(review))
    }

    fn external_review_valid(&self, review: &ExternalReview) -> bool {
        self.state.connection == ConnectionState::Ready
            && self.state.epoch == review.epoch
            && !self.external_refresh.invalidated()
            && self
                .state
                .documents
                .get(&review.document)
                .is_some_and(|document| {
                    document.target == EditTarget::DeviceBuffer
                        && document.local_revision == review.local_revision
                        && Arc::ptr_eq(&document.baseline, &review.baseline)
                })
            && self.state.confirmed_buffer.as_ref().is_some_and(|actual| {
                Arc::ptr_eq(actual, &review.actual)
                    && actual.revision == review.actual.revision
                    && actual.context == review.actual.context
            })
    }

    pub(crate) fn external_write_barrier(&self) -> bool {
        self.external_refresh.invalidated() || !self.external_reviews.is_empty()
    }

    fn expire_external_reviews(&mut self) {
        if self.firmware_editor_blocks_normal_actions() {
            return;
        }
        let expired = std::mem::take(&mut self.external_reviews);
        for (intent, review) in expired {
            if let Some(document) = self.state.documents.get_mut(&review.document) {
                document
                    .conflicts
                    .retain(|conflict| conflict.intent != intent);
            }
        }
    }

    pub(crate) fn reset_external_reconciliation(&mut self, epoch: SessionEpoch) {
        if !self.firmware_editor_blocks_normal_actions() {
            self.history_continuation = None;
        }
        if !self.firmware_editor_blocks_normal_actions() {
            self.external_refresh.reset(epoch);
            self.expire_external_reviews();
        }
    }

    fn detach_buffer(&mut self, reason: &str) -> Option<DocumentId> {
        let old_id = device_document_id();
        let mut document = self.state.documents.remove(&old_id)?;
        let id = DocumentId(format!(
            "external-local-{}-{}",
            self.state.epoch.0, self.next_document
        ));
        self.next_document += 1;
        document.id = id.clone();
        document.target = EditTarget::Offline(id.clone());
        for pending in document.pending.values_mut() {
            if matches!(pending.stage, EditStage::Queued) {
                pending.stage = EditStage::Failed(ServiceError::new(ErrorKind::Cancelled, reason));
            } else if matches!(pending.stage, EditStage::Sent | EditStage::AwaitingReadback) {
                pending.stage =
                    EditStage::Indeterminate(ServiceError::new(ErrorKind::Indeterminate, reason));
            }
        }
        for request in self.requests.values_mut() {
            if let PendingRequest::Edit { document, .. } = request
                && *document == old_id
            {
                *document = id.clone();
            }
        }
        let fields = std::mem::take(&mut self.conflict_fields);
        self.conflict_fields = fields
            .into_iter()
            .map(|((document, intent), fields)| {
                (
                    (
                        if document == old_id {
                            id.clone()
                        } else {
                            document
                        },
                        intent,
                    ),
                    fields,
                )
            })
            .collect();
        if let Some(comparison) = self.state.comparisons.remove(&old_id) {
            self.state.comparisons.insert(id.clone(), comparison);
        }
        if let Some(uncertainty) = self.state.recovered_uncertainty.remove(&old_id) {
            self.state
                .recovered_uncertainty
                .insert(id.clone(), uncertainty);
        }
        if self.firmware_editor_blocks_normal_actions() {
            if let Some(queue) = self.deferred.remove(&old_id) {
                self.deferred.insert(id.clone(), queue);
            }
        } else {
            self.deferred.remove(&old_id);
        }
        self.state.documents.insert(id.clone(), document);
        Some(id)
    }

    fn install_actual_buffer(&mut self, actual: Arc<PresetSnapshot>) {
        let id = device_document_id();
        self.state.documents.insert(
            id.clone(),
            EditorDocument::new(id, EditTarget::DeviceBuffer, actual.clone()),
        );
        self.state.confirmed_buffer = Some(actual);
        self.state.buffer_persistence = BufferPersistence::Unknown;
    }

    fn retire_unowned_buffer_edits(&mut self) {
        if let Some(document) = self.state.documents.get_mut(&device_document_id()) {
            for (intent, pending) in &mut document.pending {
                if self.requests.contains_key(intent) {
                    continue;
                }
                let error = match &pending.stage {
                    EditStage::Queued => Some(ServiceError::new(
                        ErrorKind::Cancelled,
                        "Complete state was read; this unsent edit was retired without replay",
                    )),
                    EditStage::Sent | EditStage::AwaitingReadback => Some(ServiceError::new(
                        ErrorKind::Indeterminate,
                        "Complete state was read; earlier delivery remains unknown and was not retried",
                    )),
                    EditStage::Indeterminate(error) => {
                        let mut error = error.clone();
                        error.message = format!(
                            "Complete state was read; earlier delivery remains unknown and was not retried: {}",
                            error.message
                        );
                        Some(error)
                    }
                    _ => None,
                };
                if let Some(error) = error {
                    pending.stage = EditStage::Failed(error);
                }
            }
        }
        if !self.requests.values().any(|request| {
            matches!(
                request,
                PendingRequest::Edit { .. }
                    | PendingRequest::Save { .. }
                    | PendingRequest::WriteStored { .. }
                    | PendingRequest::Select { .. }
                    | PendingRequest::Global
                    | PendingRequest::GlobalControl { .. }
                    | PendingRequest::DeviceJob(_)
            )
        }) {
            self.blocked_after_uncertainty = false;
        }
    }

    /// Called for owned complete reads only, before ordinary field merge. Returns
    /// true when this no-write path has handled the snapshot.
    pub(crate) fn reconcile_external_buffer(
        &mut self,
        stamp: RequestStamp,
        actual: Arc<PresetSnapshot>,
    ) -> bool {
        let request = self.requests.get(&stamp.intent).cloned();
        let explicit_read = matches!(
            request,
            Some(PendingRequest::Read(PresetTarget::EditBuffer))
        );
        let previous = self.state.confirmed_buffer.clone();
        let context_changed = previous
            .as_ref()
            .is_some_and(|previous| previous.context != actual.context);
        if !explicit_read
            && !(context_changed && matches!(request, Some(PendingRequest::Edit { .. })))
        {
            return false;
        }
        if explicit_read {
            self.requests.remove(&stamp.intent);
        }
        self.external_refresh.finished(stamp, true);
        if !self.firmware_editor_blocks_normal_actions() {
            self.deferred.remove(&device_document_id());
            self.retire_unowned_buffer_edits();
        }
        let had_review = !self.external_reviews.is_empty();
        self.expire_external_reviews();
        self.state.confirmed_buffer = Some(actual.clone());
        let id = device_document_id();
        if context_changed {
            let was_active = self.state.active_document == Some(id.clone());
            let retain_active = self.state.documents.get(&id).is_some_and(has_retained_work);
            let detached = self.detach_buffer(
                "Pedal context changed; old pending edits remain local and will not be replayed",
            );
            self.install_actual_buffer(actual);
            if was_active {
                self.state.active_document = if retain_active { detached } else { Some(id) };
            }
            self.state.message(MessageSeverity::Warning, "The pedal selected another preset. Previous work and history were retained as a local document.", None);
            return true;
        }
        let unchanged = previous
            .as_ref()
            .is_some_and(|previous| same_complete_state(previous, &actual));
        if (!unchanged || had_review) && !self.firmware_editor_blocks_normal_actions() {
            self.history_continuation = None;
        }
        let Some(document) = self.state.documents.get_mut(&id) else {
            self.install_actual_buffer(actual);
            return true;
        };
        if unchanged && !had_review {
            // Refresh never replays an unsent delta. An unchanged read retains
            // the entire existing document and its already bound history.
            return true;
        }
        if !has_retained_work(document)
            && document.baseline.context == previous.as_ref().and_then(|value| value.context)
        {
            document.baseline = actual.clone();
            document.draft = PresetDraft::from_snapshot(&actual);
            document.local_revision.0 += 1;
            self.state.buffer_persistence = BufferPersistence::Unknown;
        } else {
            let review = ExternalReview {
                epoch: self.state.epoch,
                document: id.clone(),
                local_revision: document.local_revision,
                baseline: document.baseline.clone(),
                actual: actual.clone(),
            };
            document.conflicts.push(EditConflict { intent: stamp.intent,
                description: "The pedal changed. Your entire local draft and history are preserved; review the complete pedal state before continuing.".into(), actual });
            self.external_reviews.insert(stamp.intent, review);
        }
        true
    }

    pub(crate) fn resolve_external_change(
        &mut self,
        resolution: &ConflictResolution,
    ) -> Option<Vec<Effect>> {
        let intent = match resolution {
            ConflictResolution::AcceptDevice(intent) | ConflictResolution::KeepLocal(intent) => {
                *intent
            }
        };
        let review = self.external_reviews.get(&intent)?.clone();
        if !self.external_review_valid(&review) {
            self.state.error(ServiceError::new(
                ErrorKind::Conflict,
                "Pedal change review expired. Read the current preset and review again.",
            ));
            return Some(Vec::new());
        }
        let was_active = self.state.active_document == Some(review.document.clone());
        self.expire_external_reviews();
        let detached =
            self.detach_buffer("External change review kept old work locally; no edit was sent");
        self.install_actual_buffer(review.actual);
        if was_active {
            self.state.active_document = match resolution {
                ConflictResolution::AcceptDevice(_) => Some(device_document_id()),
                ConflictResolution::KeepLocal(_) => detached,
            };
        }
        self.blocked_after_uncertainty = false;
        self.state.message(MessageSeverity::Info, "Previous work and history were retained in a local document. The pedal buffer was refreshed without sending an edit.", None);
        Some(Vec::new())
    }
}

#[cfg(all(test, gp200_private_evidence))]
mod firmware_barrier_history_tests {
    use super::*;
    use gp200_catalog::Catalog;

    #[test]
    fn original_verified_model_history_step_settles_without_losing_its_remaining_chain() {
        let before_raw = RawTransfer::from_payload(0x12, include_bytes!("../../../analysis/implementation/dependent-model-normal-root/amp-auto-cab-off/rust/own-fresh-current-before.payload.bin").to_vec()).unwrap();
        let reverse_raw = RawTransfer::from_payload(0x12, include_bytes!("../../../analysis/implementation/dependent-model-normal-root/amp-auto-cab-off/rust/undo-60-6.preset.payload.bin").to_vec()).unwrap();
        let global_raw = RawTransfer::from_payload(0x12, include_bytes!("../../../analysis/implementation/dependent-model-normal-root/amp-auto-cab-off/rust/undo-63-6.globals.payload.bin").to_vec()).unwrap();
        let snapshot = |raw, revision| {
            Arc::new(PresetSnapshot::from_raw(
                Arc::new(raw),
                SnapshotSource::Hardware {
                    epoch: SessionEpoch(1),
                },
                Some(PresetTarget::EditBuffer),
                Revision(revision),
            ))
        };
        let before = snapshot(before_raw, 1);
        let actual = snapshot(reverse_raw, 5);
        let globals = Arc::new(
            DeviceGlobalState::from_transfer(Revision(3), SessionEpoch(1), global_raw, vec![])
                .unwrap(),
        );
        let id = device_document_id();
        let owned = RequestStamp {
            epoch: SessionEpoch(1),
            intent: IntentId(123),
        };
        let plan = DependentModelPlan {
            profile: DependentModelProfile::AmpAutoCabOff,
            module: ModuleId::new(3).unwrap(),
            previous: ModelCode(0x07000001),
            target: ModelCode(0x07000015),
            auto_cab: false,
            paired_cabinet: None,
        };
        let patch = EditPatch::SetModel {
            module: plan.module,
            model: plan.target,
        };
        let mut app = AppController::new(Catalog::embedded().unwrap(), AppPreferences::default());
        app.state.epoch = owned.epoch;
        app.state.connection = ConnectionState::Ready;
        app.state.identity = Some(DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Hardware,
            firmware: None,
            display_name: "Synthetic owned history".into(),
        });
        app.state.confirmed_buffer = Some(actual.clone());
        app.state.device_globals = Some(globals.clone());
        let mut document =
            EditorDocument::new(id.clone(), EditTarget::DeviceBuffer, before.clone());
        let desired = document.draft.clone();
        document.pending.insert(
            owned.intent,
            PendingEdit {
                patch: patch.clone(),
                stage: EditStage::AwaitingReadback,
                revision: document.local_revision,
            },
        );
        app.requests.insert(
            owned.intent,
            PendingRequest::Edit {
                document: id.clone(),
                patch,
                saw_readback: true,
                verified: false,
                dependent: Some(crate::PendingDependentModel {
                    plan: BoundModelPlan::Existing(plan.clone()),
                    before_globals: globals.clone(),
                    saw_globals: true,
                }),
            },
        );
        let remaining = app.delta(&desired, &actual);
        assert_eq!(remaining.len(), 4);
        app.deferred.insert(id.clone(), remaining.clone());
        app.history_continuation = Some(HistoryContinuation {
            epoch: owned.epoch,
            document: id.clone(),
            local_revision: document.local_revision,
            baseline: before,
            desired,
            plan: BoundModelPlan::Existing(plan),
            owned: Some(owned.intent),
            remaining: vec![],
            confirmed: None,
            globals: None,
        });
        app.state.documents.insert(id.clone(), document);
        let lease = app.begin_firmware_editor_preparation().unwrap();
        assert!(
            app.dispatch(AppInput::Device(DeviceEvent::EditProgress {
                stamp: owned,
                stage: EditStage::Verified
            }))
            .is_empty()
        );
        assert!(!app.requests.contains_key(&owned.intent));
        assert!(matches!(
            app.state.documents[&id].pending[&owned.intent].stage,
            EditStage::Verified
        ));
        assert_eq!(app.deferred[&id], remaining);
        let chain = app.history_continuation.as_ref().unwrap();
        assert_eq!(chain.owned, None);
        assert_eq!(chain.remaining, remaining);
        let bound = chain.confirmed.clone();
        assert!(app.resume_history_continuation().is_empty());
        assert_eq!(app.deferred[&id], remaining);
        assert_eq!(app.history_continuation.as_ref().unwrap().confirmed, bound);
        let readiness = app.firmware_editor_readiness(&lease).unwrap();
        assert!(
            readiness
                .blockers
                .contains(&crate::FirmwareEditorBlocker::HistoryContinuation)
        );
        assert!(
            readiness
                .blockers
                .contains(&crate::FirmwareEditorBlocker::DeferredEdits(4))
        );
    }
}
