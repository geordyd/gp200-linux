//! Pure editor admission for updater coordination. These descriptions and the
//! private controller lease cannot acquire MIDI, a library store, or flash authority.
use crate::{AppController, BufferPersistence};
use gp200_domain::*;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FirmwareEditorPhase {
    #[default]
    Normal,
    Preparing,
    Sealed,
    RefreshingNormal,
    RecoveryRequired,
}

/// Render-only state. Admission always uses the controller's private barrier.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FirmwareEditorStatus {
    pub phase: FirmwareEditorPhase,
    pub generation: u64,
    pub source_changed: bool,
    /// Only the exact locally retained session is excluded from a read-only
    /// pedal backup. This is never a claim of installed or restorable content.
    pub local_draft_excluded: bool,
    pub reason: Option<String>,
}

/// A non-clone, non-serializable editor coordination handle, not a service permit.
///
/// ```compile_fail
/// fn duplicate(lease: gp200_app::FirmwareEditorLease) { let _ = lease.clone(); }
/// ```
/// ```compile_fail
/// let _ = gp200_app::FirmwareEditorLease { owner: std::sync::Arc::new(()), generation: 1 };
/// ```
/// ```compile_fail
/// let _: gp200_app::FirmwareEditorLease = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug)]
pub struct FirmwareEditorLease {
    owner: Arc<()>,
    generation: u64,
}
impl FirmwareEditorLease {
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// Exact local descriptions retained when preparing. Firmware origin remains
/// whatever the actual identity reported; no user report is promoted into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareEditorSource {
    pub epoch: SessionEpoch,
    pub identity: Option<DeviceIdentity>,
    pub buffer: Option<Arc<PresetSnapshot>>,
    pub globals: Option<Arc<DeviceGlobalState>>,
    pub persistence: BufferPersistence,
    pub recovery: RecoverySession,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirmwareEditorBlocker {
    SourceChanged,
    ConnectionNotReady,
    IdentityMissing,
    BufferMissing,
    GlobalsMissing,
    PendingDeviceRequests(usize),
    PendingReadAssociations(usize),
    PendingConnect,
    PendingLibraryEffects(usize),
    PendingAssetEffects(usize),
    PendingVersionLoads(usize),
    RetainedDocumentOpenings(usize),
    DeferredEdits(usize),
    DocumentPending { document: DocumentId, count: usize },
    DocumentConflicts { document: DocumentId, count: usize },
    OpenGesture(DocumentId),
    RecoveredUncertainty(usize),
    DeviceUncertainty,
    ExternalRefresh,
    ExternalReviews(usize),
    HistoryContinuation,
    RunningJobs(usize),
    PreparedDeviceJobs(usize),
    DirtyDeviceDraft,
    BufferPersistenceUnresolved,
    RecoveryRequired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareEditorReadiness {
    pub generation: u64,
    pub blockers: Vec<FirmwareEditorBlocker>,
}
impl FirmwareEditorReadiness {
    pub fn locally_settled(&self) -> bool {
        self.blockers.is_empty()
    }
}

#[derive(Debug)]
pub(crate) struct ActiveFirmwareEditorBarrier {
    pub(crate) owner: Arc<()>,
    pub(crate) generation: u64,
    pub(crate) phase: FirmwareEditorPhase,
    pub(crate) source: FirmwareEditorSource,
    pub(crate) source_changed: bool,
    recovery_purpose: bool,
    excluded_session: Option<RecoverySession>,
    pub(crate) normal_refresh: Option<FirmwareEditorNormalRefresh>,
}

#[derive(Debug)]
pub(crate) struct FirmwareEditorNormalRefresh {
    pub(crate) source_epoch: SessionEpoch,
    pub(crate) minimum_epoch: SessionEpoch,
    pub(crate) connect: IntentId,
    pub(crate) epoch: Option<SessionEpoch>,
    identity: Option<DeviceIdentity>,
    ready: bool,
    pub(crate) reads: Option<(RequestStamp, RequestStamp)>,
    current: Option<Arc<PresetSnapshot>>,
    globals: Option<Arc<DeviceGlobalState>>,
    failed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LibraryCompletion {
    Entry,
    Query,
    Load,
    Store,
    List,
    Import,
    Export,
}

impl AppController {
    fn firmware_editor_source(&self) -> FirmwareEditorSource {
        FirmwareEditorSource {
            epoch: self.state.epoch,
            identity: self.state.identity.clone(),
            buffer: self.state.confirmed_buffer.clone(),
            globals: self.state.device_globals.clone(),
            persistence: self.state.buffer_persistence.clone(),
            recovery: self.recovery_session(),
        }
    }

    /// Freeze new normal actions immediately. Already offered work continues
    /// receiving its original results and may invalidate this source binding.
    pub fn begin_firmware_editor_preparation(
        &mut self,
    ) -> Result<FirmwareEditorLease, ServiceError> {
        if self.firmware_barrier.is_some() {
            return Err(Self::firmware_editor_busy());
        }
        let generation = self
            .next_firmware_generation
            .checked_add(1)
            .ok_or_else(|| {
                ServiceError::new(
                    ErrorKind::Indeterminate,
                    "Editor preparation generation exhausted",
                )
            })?;
        let source = self.firmware_editor_source();
        source.recovery.validate()?;
        let owner = Arc::new(());
        self.next_firmware_generation = generation;
        self.firmware_barrier = Some(ActiveFirmwareEditorBarrier {
            owner: owner.clone(),
            generation,
            phase: FirmwareEditorPhase::Preparing,
            source,
            source_changed: false,
            recovery_purpose: false,
            excluded_session: None,
            normal_refresh: None,
        });
        self.sync_firmware_editor_status(None);
        Ok(FirmwareEditorLease { owner, generation })
    }

    /// Freeze a local editor for a new boot-only recovery review. This source
    /// epoch describes only the editor; no normal device capture or retirement
    /// is implied. Every local-work blocker remains active.
    pub fn begin_firmware_recovery_preparation(
        &mut self,
    ) -> Result<FirmwareEditorLease, ServiceError> {
        let lease = self.begin_firmware_editor_preparation()?;
        self.firmware_barrier
            .as_mut()
            .expect("new local barrier")
            .recovery_purpose = true;
        Ok(lease)
    }

    fn firmware_lease_matches(&self, lease: &FirmwareEditorLease) -> bool {
        self.firmware_barrier.as_ref().is_some_and(|barrier| {
            Arc::ptr_eq(&barrier.owner, &lease.owner) && barrier.generation == lease.generation
        })
    }

    /// Explicitly bind a new generation after original results have settled.
    /// This does not discard queued work or acknowledge durable storage.
    pub fn refresh_firmware_editor_preparation(
        &mut self,
        lease: &mut FirmwareEditorLease,
    ) -> Result<FirmwareEditorSource, ServiceError> {
        if !self.firmware_lease_matches(lease)
            || self
                .firmware_barrier
                .as_ref()
                .is_none_or(|b| b.phase != FirmwareEditorPhase::Preparing)
        {
            return Err(Self::firmware_editor_busy());
        }
        let generation = self
            .next_firmware_generation
            .checked_add(1)
            .ok_or_else(|| {
                ServiceError::new(
                    ErrorKind::Indeterminate,
                    "Editor preparation generation exhausted",
                )
            })?;
        let source = self.firmware_editor_source();
        source.recovery.validate()?;
        self.next_firmware_generation = generation;
        let barrier = self.firmware_barrier.as_mut().expect("validated barrier");
        barrier.generation = generation;
        barrier.source = source.clone();
        barrier.source_changed = false;
        barrier.excluded_session = None;
        lease.generation = generation;
        self.sync_firmware_editor_status(None);
        Ok(source)
    }

    pub fn firmware_editor_bound_source(
        &self,
        lease: &FirmwareEditorLease,
    ) -> Result<&FirmwareEditorSource, ServiceError> {
        if !self.firmware_lease_matches(lease) {
            return Err(Self::firmware_editor_busy());
        }
        Ok(&self
            .firmware_barrier
            .as_ref()
            .expect("validated barrier")
            .source)
    }

    /// Reducer readiness only. The runtime must separately prove its real
    /// queues, genuine saved recovery reply, joined store, and MIDI ownership.
    pub fn firmware_editor_readiness(
        &self,
        lease: &FirmwareEditorLease,
    ) -> Result<FirmwareEditorReadiness, ServiceError> {
        if !self.firmware_lease_matches(lease) {
            return Err(Self::firmware_editor_busy());
        }
        let barrier = self.firmware_barrier.as_ref().expect("validated barrier");
        let mut blockers = Vec::new();
        if barrier.source_changed || barrier.source != self.firmware_editor_source() {
            blockers.push(FirmwareEditorBlocker::SourceChanged);
        }
        if barrier.phase == FirmwareEditorPhase::RecoveryRequired {
            blockers.push(FirmwareEditorBlocker::RecoveryRequired);
        }
        if !barrier.recovery_purpose && self.state.connection != ConnectionState::Ready {
            blockers.push(FirmwareEditorBlocker::ConnectionNotReady);
        }
        if !barrier.recovery_purpose && self.state.identity.is_none() {
            blockers.push(FirmwareEditorBlocker::IdentityMissing);
        }
        if !barrier.recovery_purpose && self.state.confirmed_buffer.is_none() {
            blockers.push(FirmwareEditorBlocker::BufferMissing);
        }
        if !barrier.recovery_purpose && self.state.device_globals.is_none() {
            blockers.push(FirmwareEditorBlocker::GlobalsMissing);
        }
        if !self.requests.is_empty() {
            blockers.push(FirmwareEditorBlocker::PendingDeviceRequests(
                self.requests.len(),
            ));
        }
        // latest_reads is an ordering history too; only entries with a live
        // request represent outstanding work and must remain until settled.
        let reads = self
            .latest_reads
            .values()
            .filter(|id| self.requests.contains_key(id))
            .count();
        if reads != 0 {
            blockers.push(FirmwareEditorBlocker::PendingReadAssociations(reads));
        }
        if self.connect_intent.is_some() {
            blockers.push(FirmwareEditorBlocker::PendingConnect);
        }
        let local: usize = self.pending_library_effects.values().sum();
        if local != 0 {
            blockers.push(FirmwareEditorBlocker::PendingLibraryEffects(local));
        }
        let assets: usize = self.pending_asset_effects.values().sum();
        if assets != 0 {
            blockers.push(FirmwareEditorBlocker::PendingAssetEffects(assets));
        }
        if !self.loading_versions.is_empty() {
            blockers.push(FirmwareEditorBlocker::PendingVersionLoads(
                self.loading_versions.len(),
            ));
        }
        if !self.retained_document_openings.is_empty() {
            blockers.push(FirmwareEditorBlocker::RetainedDocumentOpenings(
                self.retained_document_openings.len(),
            ));
        }
        let deferred = self.deferred.values().map(Vec::len).sum();
        if deferred != 0 {
            blockers.push(FirmwareEditorBlocker::DeferredEdits(deferred));
        }
        for document in self.state.documents.values() {
            let count = document
                .pending
                .values()
                .filter(|p| !matches!(p.stage, EditStage::Verified))
                .count();
            if count != 0 {
                blockers.push(FirmwareEditorBlocker::DocumentPending {
                    document: document.id.clone(),
                    count,
                });
            }
            if !document.conflicts.is_empty() {
                blockers.push(FirmwareEditorBlocker::DocumentConflicts {
                    document: document.id.clone(),
                    count: document.conflicts.len(),
                });
            }
            if document.undo.gesture.is_some() {
                blockers.push(FirmwareEditorBlocker::OpenGesture(document.id.clone()));
            }
        }
        let recovered = self.state.recovered_session_uncertainty.len()
            + self
                .state
                .recovered_uncertainty
                .values()
                .map(Vec::len)
                .sum::<usize>();
        if recovered != 0 {
            blockers.push(FirmwareEditorBlocker::RecoveredUncertainty(recovered));
        }
        if self.blocked_after_uncertainty {
            blockers.push(FirmwareEditorBlocker::DeviceUncertainty);
        }
        if self.external_refresh.invalidated() {
            blockers.push(FirmwareEditorBlocker::ExternalRefresh);
        }
        if !self.external_reviews.is_empty() {
            blockers.push(FirmwareEditorBlocker::ExternalReviews(
                self.external_reviews.len(),
            ));
        }
        if self.history_continuation.is_some() {
            blockers.push(FirmwareEditorBlocker::HistoryContinuation);
        }
        let jobs = self
            .state
            .jobs
            .values()
            .filter(|job| job.total.is_none_or(|total| job.completed < total))
            .count();
        if jobs != 0 {
            blockers.push(FirmwareEditorBlocker::RunningJobs(jobs));
        }
        if !self.state.prepared_jobs.is_empty() {
            blockers.push(FirmwareEditorBlocker::PreparedDeviceJobs(
                self.state.prepared_jobs.len(),
            ));
        }
        if barrier.excluded_session.as_ref() != Some(&self.recovery_session())
            && self.state.documents.values().any(|doc| {
                doc.target == EditTarget::DeviceBuffer
                    && self.state.confirmed_buffer.as_ref().is_none_or(|buffer| {
                        !doc.draft
                            .semantically_equal(&PresetDraft::from_snapshot(buffer))
                    })
            })
        {
            blockers.push(FirmwareEditorBlocker::DirtyDeviceDraft);
        }
        if matches!(
            self.state.buffer_persistence,
            BufferPersistence::Saving { .. } | BufferPersistence::Indeterminate { .. }
        ) {
            blockers.push(FirmwareEditorBlocker::BufferPersistenceUnresolved);
        }
        Ok(FirmwareEditorReadiness {
            generation: barrier.generation,
            blockers,
        })
    }

    /// Record the explicit choice to keep the exact local work without applying
    /// it to the pedal. The runtime must still consume a genuine Saved event for
    /// this exact session. Every pending, uncertain, conflict and gesture blocker
    /// remains; this choice removes only DirtyDeviceDraft for read-only review.
    pub fn retain_firmware_local_draft_excluded(
        &mut self,
        lease: &FirmwareEditorLease,
        described_session: &RecoverySession,
    ) -> Result<(), ServiceError> {
        if !self.firmware_lease_matches(lease)
            || self.firmware_barrier.as_ref().is_none_or(|b| {
                b.phase != FirmwareEditorPhase::Preparing || b.normal_refresh.is_some()
            })
        {
            return Err(Self::firmware_editor_busy());
        }
        described_session.validate()?;
        if *described_session != self.recovery_session()
            || self
                .firmware_barrier
                .as_ref()
                .is_none_or(|b| b.source_changed || b.source != self.firmware_editor_source())
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Retaining local work requires the exact unchanged editing session",
            ));
        }
        self.firmware_barrier
            .as_mut()
            .expect("validated barrier")
            .excluded_session = Some(described_session.clone());
        self.sync_firmware_editor_status(None);
        Ok(())
    }

    /// Only the real coordinator may interpret a transport ReturnNormal event.
    /// These arguments describe that observation; they confer no transport or
    /// physical authority. New normal receipts are required before local release.
    pub fn begin_firmware_editor_normal_refresh(
        &mut self,
        lease: &FirmwareEditorLease,
        retired_epoch: SessionEpoch,
        minimum_epoch: SessionEpoch,
        selection: DeviceSelection,
    ) -> Result<Vec<Effect>, ServiceError> {
        if self
            .firmware_barrier
            .as_ref()
            .is_none_or(|b| b.recovery_purpose)
        {
            return Err(Self::firmware_editor_busy());
        }
        self.begin_firmware_editor_refresh(lease, retired_epoch, minimum_epoch, selection)
    }

    /// A recovery owner has returned its original store and verified normal
    /// endpoint. Still require a new ordinary Connect, identity and two reads.
    /// editor_source_epoch is a local binding, never retired normal proof.
    pub fn begin_firmware_recovery_normal_refresh(
        &mut self,
        lease: &FirmwareEditorLease,
        editor_source_epoch: SessionEpoch,
        minimum_epoch: SessionEpoch,
        selection: DeviceSelection,
    ) -> Result<Vec<Effect>, ServiceError> {
        if self
            .firmware_barrier
            .as_ref()
            .is_none_or(|b| !b.recovery_purpose)
        {
            return Err(Self::firmware_editor_busy());
        }
        self.begin_firmware_editor_refresh(lease, editor_source_epoch, minimum_epoch, selection)
    }

    fn begin_firmware_editor_refresh(
        &mut self,
        lease: &FirmwareEditorLease,
        source_epoch: SessionEpoch,
        minimum_epoch: SessionEpoch,
        selection: DeviceSelection,
    ) -> Result<Vec<Effect>, ServiceError> {
        if !self.firmware_lease_matches(lease)
            || self.firmware_barrier.as_ref().is_none_or(|b| {
                !matches!(
                    b.phase,
                    FirmwareEditorPhase::Preparing
                        | FirmwareEditorPhase::Sealed
                        | FirmwareEditorPhase::RecoveryRequired
                ) || b.normal_refresh.is_some()
                    || b.source.epoch != source_epoch
            })
            || minimum_epoch <= source_epoch
            || !self.requests.is_empty()
            || self.connect_intent.is_some()
            || selection.input_id.is_empty()
            || selection.output_id.is_empty()
            || selection.input_id.len() > 4096
            || selection.output_id.len() > 4096
            || selection.input_id.chars().any(char::is_control)
            || selection.output_id.chars().any(char::is_control)
        {
            return Err(Self::firmware_editor_busy());
        }
        let (connect, effects) = self.register_firmware_refresh_connect(selection);
        let barrier = self.firmware_barrier.as_mut().expect("validated barrier");
        barrier.phase = FirmwareEditorPhase::RefreshingNormal;
        barrier.source_changed = true;
        barrier.normal_refresh = Some(FirmwareEditorNormalRefresh {
            source_epoch,
            minimum_epoch,
            connect,
            epoch: None,
            identity: None,
            ready: false,
            reads: None,
            current: None,
            globals: None,
            failed: false,
        });
        self.sync_firmware_editor_status(None);
        Ok(effects)
    }

    /// Register two new requests after the new connection's original initial
    /// result has settled. Initial Connect readback and cached values cannot
    /// substitute for these explicitly bound requests.
    pub fn request_firmware_editor_normal_refresh_reads(
        &mut self,
        lease: &FirmwareEditorLease,
    ) -> Result<Vec<Effect>, ServiceError> {
        if !self.firmware_lease_matches(lease) {
            return Err(Self::firmware_editor_busy());
        }
        if self
            .firmware_barrier
            .as_ref()
            .is_none_or(|b| b.phase != FirmwareEditorPhase::RefreshingNormal)
        {
            return Err(Self::firmware_editor_busy());
        }
        let refresh = self
            .firmware_barrier
            .as_ref()
            .and_then(|b| b.normal_refresh.as_ref())
            .ok_or_else(Self::firmware_editor_busy)?;
        if refresh.failed {
            return Err(ServiceError::new(
                ErrorKind::Indeterminate,
                "The original fresh normal session failed; ownership remains held",
            ));
        }
        if refresh.failed
            || !refresh.ready
            || refresh.reads.is_some()
            || refresh.epoch != Some(self.state.epoch)
            || refresh.identity.as_ref() != self.state.identity.as_ref()
            || refresh.identity.is_none()
            || self.state.connection != ConnectionState::Ready
            || !self.requests.is_empty()
            || self.connect_intent.is_some()
        {
            return Err(Self::firmware_editor_busy());
        }
        let (current, globals, effects) = self.register_firmware_refresh_reads();
        self.firmware_barrier
            .as_mut()
            .expect("validated barrier")
            .normal_refresh
            .as_mut()
            .expect("validated refresh")
            .reads = Some((current, globals));
        Ok(effects)
    }

    /// Correlated original current read progress, descriptive and lease bound.
    pub fn firmware_editor_normal_refresh_current_received(
        &self,
        lease: &FirmwareEditorLease,
    ) -> Result<bool, ServiceError> {
        if !self.firmware_lease_matches(lease) {
            return Err(Self::firmware_editor_busy());
        }
        let refresh = self
            .firmware_barrier
            .as_ref()
            .and_then(|b| b.normal_refresh.as_ref())
            .ok_or_else(Self::firmware_editor_busy)?;
        if refresh.failed {
            return Err(ServiceError::new(
                ErrorKind::Indeterminate,
                "The original fresh normal session failed; ownership remains held",
            ));
        }
        Ok(refresh.current.is_some())
    }
    /// Explicit local release after all three newly observed sources agree with
    /// the reducer's accepted results. The runtime separately validates the real
    /// same-service result rail and must never call this from a preview DTO.
    pub fn finish_firmware_editor_normal_refresh(
        &mut self,
        lease: &FirmwareEditorLease,
    ) -> Result<(), ServiceError> {
        if !self.firmware_lease_matches(lease) {
            return Err(Self::firmware_editor_busy());
        }
        if self
            .firmware_barrier
            .as_ref()
            .and_then(|b| b.normal_refresh.as_ref())
            .is_some_and(|r| r.failed)
        {
            return Err(ServiceError::new(
                ErrorKind::Indeterminate,
                "The original fresh normal session failed; ownership remains held",
            ));
        }
        if self
            .firmware_barrier
            .as_ref()
            .is_none_or(|b| b.phase != FirmwareEditorPhase::RefreshingNormal)
            || self
                .firmware_editor_readiness(lease)?
                .blockers
                .iter()
                .any(|b| !matches!(b, FirmwareEditorBlocker::SourceChanged))
        {
            return Err(Self::firmware_editor_busy());
        }
        let refresh = self
            .firmware_barrier
            .as_ref()
            .and_then(|b| b.normal_refresh.as_ref())
            .ok_or_else(Self::firmware_editor_busy)?;
        if refresh.failed
            || !refresh.ready
            || refresh.epoch != Some(self.state.epoch)
            || refresh.identity.is_none()
            || refresh.current.is_none()
            || refresh.globals.is_none()
            || refresh.identity.as_ref() != self.state.identity.as_ref()
            || refresh.current.as_ref() != self.state.confirmed_buffer.as_ref()
            || refresh.globals.as_ref() != self.state.device_globals.as_ref()
            || self.state.connection != ConnectionState::Ready
            || !self.requests.is_empty()
            || self.connect_intent.is_some()
            || !self.pending_library_effects.is_empty()
            || !self.pending_asset_effects.is_empty()
        {
            return Err(Self::firmware_editor_busy());
        }
        self.firmware_barrier = None;
        self.sync_firmware_editor_status(None);
        Ok(())
    }

    /// Observe only after the ordinary reducer has accepted the original result.
    pub(crate) fn observe_firmware_editor_refresh_event(&mut self, event: &DeviceEvent) {
        let Some(barrier) = self.firmware_barrier.as_mut() else {
            return;
        };
        let Some(refresh) = barrier.normal_refresh.as_mut() else {
            return;
        };
        match event {
            DeviceEvent::ConnectionChanged { epoch, state }
                if *epoch >= refresh.minimum_epoch && *epoch > refresh.source_epoch =>
            {
                if refresh.epoch.is_some_and(|old| old != *epoch) {
                    refresh.failed = true;
                }
                refresh.epoch = Some(*epoch);
                refresh.ready = *state == ConnectionState::Ready;
                if matches!(
                    state,
                    ConnectionState::Recovering
                        | ConnectionState::Disconnected
                        | ConnectionState::Offline
                ) {
                    refresh.failed = true;
                }
            }
            DeviceEvent::SessionChanged {
                epoch, identity, ..
            } if Some(*epoch) == refresh.epoch => {
                let association_matches =
                    if barrier.recovery_purpose {
                        identity.model == DeviceModel::Gp200
                    } else {
                        barrier.source.identity.as_ref().is_some_and(|old| {
                            old.model == identity.model && old.mode == identity.mode
                        })
                    };
                if !association_matches || self.state.identity.as_ref() != Some(identity) {
                    refresh.failed = true;
                } else {
                    refresh.identity = Some(identity.clone());
                }
            }
            DeviceEvent::SnapshotReceived {
                stamp,
                target: PresetTarget::EditBuffer,
                snapshot,
            } if refresh.reads.is_some_and(|(current, _)| current == *stamp)
                && Some(stamp.epoch) == refresh.epoch =>
            {
                let source_matches = refresh.identity.as_ref().is_some_and(|identity| {
                    match (&snapshot.source, identity.mode) {
                        (SnapshotSource::Hardware { epoch }, BackendMode::Hardware) => {
                            *epoch == stamp.epoch
                        }
                        (SnapshotSource::Simulator, BackendMode::Simulator)
                        | (SnapshotSource::Capture { .. }, BackendMode::CaptureReplay) => true,
                        _ => false,
                    }
                });
                if source_matches && self.state.confirmed_buffer.as_ref() == Some(snapshot) {
                    refresh.current = Some(snapshot.clone());
                } else {
                    refresh.failed = true;
                }
            }
            DeviceEvent::GlobalStateReceived { stamp, state }
                if refresh.reads.is_some_and(|(_, globals)| globals == *stamp)
                    && Some(stamp.epoch) == refresh.epoch =>
            {
                if self.state.device_globals.as_ref() == Some(state) {
                    refresh.globals = Some(state.clone());
                } else {
                    refresh.failed = true;
                }
            }
            DeviceEvent::OperationFailed { stamp, .. }
                if stamp.intent == refresh.connect
                    || refresh.reads.is_some_and(|(current, globals)| {
                        *stamp == current || *stamp == globals
                    }) =>
            {
                refresh.failed = true;
            }
            _ => {}
        }
    }

    /// Compare exact recovery bytes represented by the DTO and seal local
    /// editing. A caller-provided session is descriptive; only the future
    /// coordinator's genuine Saved event can establish durable retention.
    pub fn seal_firmware_editor_preparation(
        &mut self,
        lease: &FirmwareEditorLease,
        described_retained_session: &RecoverySession,
    ) -> Result<(), ServiceError> {
        if self
            .firmware_barrier
            .as_ref()
            .is_none_or(|barrier| barrier.recovery_purpose)
        {
            return Err(Self::firmware_editor_busy());
        }
        self.seal_firmware_editor_barrier(lease, described_retained_session)
    }

    /// Seal the distinct local recovery purpose after exact retained-session
    /// comparison. This seal carries no normal capture or physical permission.
    pub fn seal_firmware_recovery_preparation(
        &mut self,
        lease: &FirmwareEditorLease,
        described_retained_session: &RecoverySession,
    ) -> Result<(), ServiceError> {
        if self
            .firmware_barrier
            .as_ref()
            .is_none_or(|barrier| !barrier.recovery_purpose)
        {
            return Err(Self::firmware_editor_busy());
        }
        self.seal_firmware_editor_barrier(lease, described_retained_session)
    }

    fn seal_firmware_editor_barrier(
        &mut self,
        lease: &FirmwareEditorLease,
        described_retained_session: &RecoverySession,
    ) -> Result<(), ServiceError> {
        let readiness = self.firmware_editor_readiness(lease)?;
        if !readiness.locally_settled()
            || self
                .firmware_barrier
                .as_ref()
                .is_none_or(|b| b.phase != FirmwareEditorPhase::Preparing)
        {
            return Err(Self::firmware_editor_busy());
        }
        described_retained_session.validate()?;
        if *described_retained_session != self.recovery_session() {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "The retained editing session differs from the current exact draft, history, comparison, or uncertainty",
            ));
        }
        self.firmware_barrier
            .as_mut()
            .expect("validated barrier")
            .phase = FirmwareEditorPhase::Sealed;
        self.sync_firmware_editor_status(None);
        Ok(())
    }

    /// Explicit cancellation is available before sealing only and emits no
    /// continuation. It cannot release a sealed/retired owner or resume a device.
    pub fn cancel_firmware_editor_preparation(
        &mut self,
        lease: &FirmwareEditorLease,
    ) -> Result<(), ServiceError> {
        if !self.firmware_lease_matches(lease)
            || self
                .firmware_barrier
                .as_ref()
                .is_none_or(|b| b.phase != FirmwareEditorPhase::Preparing)
        {
            return Err(Self::firmware_editor_busy());
        }
        self.firmware_barrier = None;
        self.sync_firmware_editor_status(None);
        Ok(())
    }

    /// Hold local admission after an uncertain owner transition. Only a later
    /// explicit fresh normal check with the same lease may attempt local release.
    /// This is a conservative app state; it proves no hardware transition.
    pub fn hold_firmware_editor_for_recovery(
        &mut self,
        lease: &FirmwareEditorLease,
        reason: String,
    ) -> Result<(), ServiceError> {
        if !self.firmware_lease_matches(lease) {
            return Err(Self::firmware_editor_busy());
        }
        let barrier = self.firmware_barrier.as_mut().expect("validated barrier");
        barrier.phase = FirmwareEditorPhase::RecoveryRequired;
        barrier.source_changed = true;
        self.state.connection = ConnectionState::Recovering;
        self.state.identity = None;
        self.state.capabilities = Capabilities::empty(DeviceModel::Gp200);
        self.state.confirmed_buffer = None;
        self.state.globals = None;
        self.state.device_globals = None;
        self.state.device_job_session = None;
        self.state.device_assets.invalidate(self.state.epoch);
        for cached in self.state.bank.slots.values_mut() {
            cached.fresh = false;
        }
        self.state.buffer_persistence = BufferPersistence::Unknown;
        self.sync_firmware_editor_status(Some(reason));
        Ok(())
    }

    pub fn firmware_editor_blocks_normal_actions(&self) -> bool {
        self.firmware_barrier.is_some()
    }

    pub(crate) fn firmware_editor_busy() -> ServiceError {
        ServiceError::new(
            ErrorKind::Busy,
            "Normal editing is paused for firmware preparation or recovery",
        )
    }
    pub(crate) fn admit_normal_mutation(&mut self) -> bool {
        if self.firmware_editor_blocks_normal_actions() {
            self.state.error(Self::firmware_editor_busy());
            false
        } else {
            true
        }
    }
    pub(crate) fn note_firmware_source_event(&mut self) {
        if let Some(barrier) = &mut self.firmware_barrier {
            barrier.source_changed = true;
        }
        self.sync_firmware_editor_status(self.state.firmware_editor.reason.clone());
    }
    fn sync_firmware_editor_status(&mut self, reason: Option<String>) {
        self.state.firmware_editor =
            self.firmware_barrier
                .as_ref()
                .map_or_else(FirmwareEditorStatus::default, |b| FirmwareEditorStatus {
                    phase: b.phase,
                    generation: b.generation,
                    source_changed: b.source_changed,
                    local_draft_excluded: b.excluded_session.is_some(),
                    reason,
                });
    }

    /// A delayed requested open is retained without switching source or changing
    /// documents. Resuming it later is an explicit normal action.
    pub fn resume_retained_document_opening(&mut self, intent: IntentId) -> Vec<Effect> {
        if !self.admit_normal_mutation() {
            return Vec::new();
        }
        if let Some(snapshot) = self.retained_document_openings.remove(&intent) {
            self.open_offline(snapshot);
        }
        Vec::new()
    }

    pub(crate) fn track_local_effects(&mut self, effects: &[Effect]) {
        for effect in effects {
            match effect {
                Effect::Library(command) => {
                    let pair = match command {
                        LibraryCommand::ImportNative { intent, .. }
                        | LibraryCommand::ImportBundle { intent, .. } => {
                            Some((*intent, LibraryCompletion::Import))
                        }
                        LibraryCommand::ExportNative { intent, .. }
                        | LibraryCommand::ExportBundle { intent, .. }
                        | LibraryCommand::Backup { intent, .. } => {
                            Some((*intent, LibraryCompletion::Export))
                        }
                        LibraryCommand::DeleteToTrash { intent, .. }
                        | LibraryCommand::RestoreTrash { intent, .. }
                        | LibraryCommand::UpdateMetadata { intent, .. } => {
                            Some((*intent, LibraryCompletion::Entry))
                        }
                        LibraryCommand::Query { intent, .. } => {
                            Some((*intent, LibraryCompletion::Query))
                        }
                        LibraryCommand::LoadVersion { intent, .. } => {
                            Some((*intent, LibraryCompletion::Load))
                        }
                        LibraryCommand::StoreSnapshot { intent, .. } => {
                            Some((*intent, LibraryCompletion::Store))
                        }
                        LibraryCommand::ListVersions { intent, .. } => {
                            Some((*intent, LibraryCompletion::List))
                        }
                        LibraryCommand::Cancel(_) | LibraryCommand::Shutdown => None,
                    };
                    if let Some(pair) = pair {
                        *self.pending_library_effects.entry(pair).or_default() += 1;
                    }
                }
                Effect::Assets(
                    AssetCommand::Inspect(spec)
                    | AssetCommand::Prepare(spec)
                    | AssetCommand::Convert(spec),
                ) => {
                    *self.pending_asset_effects.entry(spec.id).or_default() += 1;
                }
                _ => {}
            }
        }
    }
    pub(crate) fn settle_library_effect(&mut self, event: &LibraryEvent) {
        let pair = match event {
            LibraryEvent::EntryChanged { intent, .. } => {
                Some((*intent, Some(LibraryCompletion::Entry)))
            }
            LibraryEvent::QueryResult { intent, .. } => {
                Some((*intent, Some(LibraryCompletion::Query)))
            }
            LibraryEvent::VersionLoaded { intent, .. } => {
                Some((*intent, Some(LibraryCompletion::Load)))
            }
            LibraryEvent::VersionStored { intent, .. } => {
                Some((*intent, Some(LibraryCompletion::Store)))
            }
            LibraryEvent::VersionsListed { intent, .. } => {
                Some((*intent, Some(LibraryCompletion::List)))
            }
            LibraryEvent::Imported { intent, .. } => {
                Some((*intent, Some(LibraryCompletion::Import)))
            }
            LibraryEvent::Exported { intent, .. } => {
                Some((*intent, Some(LibraryCompletion::Export)))
            }
            LibraryEvent::Failed { intent, .. } => Some((*intent, None)),
            LibraryEvent::JobProgress { .. } => None,
        };
        if let Some((intent, kind)) = pair {
            let key = self
                .pending_library_effects
                .keys()
                .find(|(id, k)| *id == intent && kind.is_none_or(|expected| expected == *k))
                .copied();
            if let Some(key) = key {
                decrement(&mut self.pending_library_effects, key);
            }
        }
    }
    pub(crate) fn settle_asset_effect(&mut self, event: &AssetEvent) {
        let job = match event {
            AssetEvent::Inspected { job, .. }
            | AssetEvent::Prepared { job, .. }
            | AssetEvent::Failed { job, .. } => Some(*job),
            AssetEvent::DeviceJobPrepared(job) => Some(job.id()),
            _ => None,
        };
        if let Some(job) = job {
            decrement(&mut self.pending_asset_effects, job);
        }
    }
}
fn decrement<K: Ord>(map: &mut BTreeMap<K, usize>, key: K) {
    if let Some(count) = map.get_mut(&key) {
        *count -= 1;
        if *count == 0 {
            map.remove(&key);
        }
    }
}
