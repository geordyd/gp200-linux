//! Native workflow coordination. All durable operations use the bounded library worker;
//! every pedal operation is dispatched through AppController and a typed UiIntent.
use crate::StudioApp;
use gp200_domain::*;
use gp200_library::*;
use std::{collections::BTreeMap, sync::Arc, time::Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LibraryTab {
    #[default]
    Presets,
    Backups,
    Compare,
    Merge,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureObject {
    Current,
    Stored(SlotId),
    Globals,
    Asset(AssetSlot),
}
impl CaptureObject {
    fn label(self) -> String {
        match self {
            Self::Current => crate::localization::text("current edit buffer").into(),
            Self::Stored(slot) => {
                std::format!("stored slot {slot}")
            }
            Self::Globals => crate::localization::text("global settings").into(),
            Self::Asset(slot) => std::format!("{:?} metadata {}", slot.bank(), slot.index() + 1),
        }
    }
    fn intent(self) -> UiIntent {
        match self {
            Self::Current => UiIntent::Refresh(PresetTarget::EditBuffer),
            Self::Stored(slot) => UiIntent::Refresh(PresetTarget::Stored(slot)),
            Self::Globals => UiIntent::RefreshDeviceGlobals,
            Self::Asset(slot) => UiIntent::RefreshAssetMetadata(slot),
        }
    }
    fn matches(self, command: &DeviceCommand) -> Option<RequestStamp> {
        match (self, command) {
            (
                Self::Current,
                DeviceCommand::RefreshTarget {
                    stamp,
                    target: PresetTarget::EditBuffer,
                },
            ) => Some(*stamp),
            (
                Self::Stored(slot),
                DeviceCommand::RefreshTarget {
                    stamp,
                    target: PresetTarget::Stored(actual),
                },
            ) if slot == *actual => Some(*stamp),
            (Self::Globals, DeviceCommand::ReadGlobals(stamp)) => Some(*stamp),
            (
                Self::Asset(slot),
                DeviceCommand::ReadAssetMetadata {
                    stamp,
                    slot: actual,
                },
            ) if slot == *actual => Some(*stamp),
            _ => None,
        }
    }
}
#[derive(Debug)]
pub(crate) struct CaptureRun {
    pub origin: BackupOrigin,
    pub id: Option<String>,
    pub cursor: u16,
    pub waiting: Option<(RequestStamp, CaptureObject, Instant)>,
    pub durable: Option<IntentId>,
    pub stop: Option<BackupAcquisition>,
    pub completed: usize,
}
impl CaptureRun {
    fn object(&self) -> Option<CaptureObject> {
        match self.cursor {
            0 => Some(CaptureObject::Current),
            1..=256 => Some(CaptureObject::Stored(
                SlotId::new(self.cursor - 1).expect("bounded"),
            )),
            257 => Some(CaptureObject::Globals),
            258..=277 => Some(CaptureObject::Asset(
                AssetSlot::new(AssetBank::Ir, self.cursor - 258).expect("bounded"),
            )),
            278..=287 => Some(CaptureObject::Asset(
                AssetSlot::new(AssetBank::SnapTone, self.cursor - 278).expect("bounded"),
            )),
            _ => None,
        }
    }
}
#[derive(Debug, Clone)]
pub(crate) enum Purpose {
    Backups,
    Setlists,
    Restores,
    Setlist,
    Resolve,
    Compare,
    Export,
    BackupBegin,
    BackupCapture,
    BackupFinish,
    Plan,
    RestoreBegin,
    RestoreDispatch,
    RestoreConfirm,
    RestorePause,
    RestoreReconcile,
    RestoreRetry,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestorePhase {
    Planning,
    Dispatching,
    Writing,
    Confirming,
    Reconcile,
}
#[derive(Debug)]
pub(crate) struct RestoreRun {
    pub origin: BackupOrigin,
    pub phase: RestorePhase,
    pub selections: Vec<RestoreSelection>,
    pub current: Option<Arc<PresetSnapshot>>,
    pub observed: BTreeMap<SlotId, Arc<PresetSnapshot>>,
    pub waiting: Option<(RequestStamp, CaptureObject, Instant)>,
    pub durable: Option<IntentId>,
    pub write: Option<RequestStamp>,
    pub write_started: Option<Instant>,
    pub write_readback: Option<Arc<PresetSnapshot>>,
    pub write_verified: bool,
    pub journal: Option<String>,
    pub target: Option<SlotId>,
}
#[derive(Debug, Default)]
pub(crate) struct LibraryWorkflowUi {
    pub tab: LibraryTab,
    pub pending: BTreeMap<IntentId, Purpose>,
    pub backups: Vec<DeviceBackup>,
    pub selected_backup: Option<String>,
    pub setlists: Vec<Setlist>,
    pub setlist: Option<Setlist>,
    pub resolved: Option<ResolvedSetlist>,
    pub selected_entry: Option<String>,
    pub setlist_dirty: bool,
    pub show_setlist_trash: bool,
    pub arranging: bool,
    pub new_setlist_name: String,
    pub pin_version: Option<LibraryVersionId>,
    pub capture: Option<CaptureRun>,
    pub restore: Option<RestoreRun>,
    pub journals: Vec<RestoreJournal>,
    pub selected_journal: Option<String>,
    pub plan: Option<RestorePlan>,
    pub accept_unknown_firmware: bool,
    pub restore_sources: Vec<RestoreSelection>,
    pub target_offset: u16,
    pub source_start: u16,
    pub source_end: u16,
    pub compare_before: Option<LibraryVersionId>,
    pub compare_after: Option<LibraryVersionId>,
    pub comparison: Option<PresetDiff>,
    pub export_backup: Option<String>,
    pub message: String,
    pub device_key: Option<String>,
}
impl LibraryWorkflowUi {
    pub fn busy(&self) -> bool {
        self.capture.is_some()
            || self.restore.is_some()
            || self.pending.values().any(|p| {
                matches!(
                    p,
                    Purpose::RestoreBegin
                        | Purpose::RestoreDispatch
                        | Purpose::RestoreConfirm
                        | Purpose::RestorePause
                        | Purpose::RestoreReconcile
                        | Purpose::RestoreRetry
                )
            })
    }
    pub fn pending_mutation(&self) -> bool {
        self.pending
            .values()
            .any(|p| matches!(p, Purpose::Setlist | Purpose::Resolve))
    }
    fn accepts_capture(&self, stamp: RequestStamp, object: CaptureObject) -> bool {
        self.capture.as_ref().is_some_and(|r| {
            r.origin.epoch == stamp.epoch
                && r.waiting
                    .as_ref()
                    .is_some_and(|(s, o, _)| *s == stamp && *o == object)
        })
    }
    fn accepts_restore(&self, stamp: RequestStamp, object: CaptureObject) -> bool {
        self.restore.as_ref().is_some_and(|r| {
            r.origin.epoch == stamp.epoch
                && r.waiting
                    .as_ref()
                    .is_some_and(|(s, o, _)| *s == stamp && *o == object)
        })
    }
}
impl StudioApp {
    pub(crate) fn workflow_submit(
        &mut self,
        action: WorkflowAction,
        purpose: Purpose,
    ) -> Option<IntentId> {
        let intent = self.local_intent();
        match self
            .services
            .submit_workflow(WorkflowCommand { intent, action })
        {
            Ok(()) => {
                self.library_workflow.pending.insert(intent, purpose);
                Some(intent)
            }
            Err(error) => {
                self.services.error(error.message);
                None
            }
        }
    }
    pub(crate) fn refresh_library_workflows(&mut self) {
        if !self.services.workflow_available() {
            return;
        }
        self.workflow_submit(WorkflowAction::ListBackups, Purpose::Backups);
        self.workflow_submit(
            WorkflowAction::ListSetlists {
                include_trash: self.library_workflow.show_setlist_trash,
            },
            Purpose::Setlists,
        );
        self.workflow_submit(WorkflowAction::ListRestores, Purpose::Restores);
    }
    pub(crate) fn backup_origin(&self) -> Option<BackupOrigin> {
        let state = self.controller.state();
        if state.connection != ConnectionState::Ready {
            return None;
        }
        Some(BackupOrigin {
            identity: state.identity.clone()?,
            epoch: state.epoch,
            device_key: self.library_workflow.device_key.clone()?,
            evidence: Vec::new(),
        })
    }
    pub(crate) fn start_device_backup(&mut self) {
        if self.library_workflow.busy()
            || self.controller.device_operation_pending()
            || self.asset_slot_ui.is_running()
        {
            return;
        }
        let Some(origin) = self.backup_origin() else {
            self.services.error(crate::localization::text(
                "Connect the pedal before capturing a device bank.",
            ));
            return;
        };
        self.bank_scan.cancel();
        let Some(intent) = self.workflow_submit(
            WorkflowAction::BeginBackup(origin.clone()),
            Purpose::BackupBegin,
        ) else {
            return;
        };
        self.library_workflow.capture = Some(CaptureRun {
            origin,
            id: None,
            cursor: 0,
            waiting: None,
            durable: Some(intent),
            stop: None,
            completed: 0,
        });
        self.library_workflow.message = crate::localization::text(
            "Creating a durable bank capture. Every read is saved before the next begins.",
        )
        .into();
    }
    pub(crate) fn stop_device_backup(&mut self) {
        if let Some(run) = &mut self.library_workflow.capture {
            run.stop = Some(BackupAcquisition::Cancelled);
        }
        self.library_workflow.message = crate::localization::text(
            "Stopping after the current read and durable save. Captured items remain available.",
        )
        .into();
    }
    pub(crate) fn plan_restore_sources(&mut self, selections: Vec<RestoreSelection>) {
        if selections.is_empty()
            || self.library_workflow.busy()
            || self.controller.device_operation_pending()
        {
            return;
        }
        let Some(origin) = self.backup_origin() else {
            self.services.error(crate::localization::text(
                "Connect the pedal to read destinations and prepare a concrete restore review.",
            ));
            return;
        };
        self.bank_scan.cancel();
        self.library_workflow.plan = None;
        self.library_workflow.restore = Some(RestoreRun {
            origin,
            phase: RestorePhase::Planning,
            selections,
            current: None,
            observed: BTreeMap::new(),
            waiting: None,
            durable: None,
            write: None,
            write_started: None,
            write_readback: None,
            write_verified: false,
            journal: None,
            target: None,
        });
        self.library_workflow.message = crate::localization::text(
            "Reading the current buffer and every selected destination before planning.",
        )
        .into();
    }
    pub(crate) fn apply_restore_plan(&mut self) {
        if self.library_workflow.busy() {
            return;
        }
        let Some(plan) = self.library_workflow.plan.clone() else {
            return;
        };
        if self.backup_origin().as_ref() != Some(&plan.target) {
            self.services.error(crate::localization::text(
                "Connection changed. Prepare the restore plan again.",
            ));
            return;
        }
        let Ok(hash) = plan.review_hash() else {
            return;
        };
        let origin = plan.target.clone();
        let selections = plan.entries.iter().map(|e| e.selection.clone()).collect();
        let intent = self.workflow_submit(
            WorkflowAction::BeginRestore {
                plan,
                review: RestoreReview {
                    plan_hash: hash,
                    accept_unknown_firmware: self.library_workflow.accept_unknown_firmware,
                },
            },
            Purpose::RestoreBegin,
        );
        if let Some(intent) = intent {
            self.library_workflow.restore = Some(RestoreRun {
                origin,
                phase: RestorePhase::Confirming,
                selections,
                current: None,
                observed: BTreeMap::new(),
                waiting: None,
                durable: Some(intent),
                write: None,
                write_started: None,
                write_readback: None,
                write_verified: false,
                journal: None,
                target: None,
            });
        }
    }
    pub(crate) fn run_restore_journal(&mut self, journal: RestoreJournal, phase: RestorePhase) {
        let Some(origin) = self.backup_origin() else {
            return;
        };
        if journal.target.device_key != origin.device_key
            || journal.target.identity != origin.identity
        {
            self.services.error(crate::localization::text("Restore journal belongs to a different device, backend or firmware. Create a new review."));
            return;
        }
        if phase != RestorePhase::Reconcile
            && (journal.target.epoch != origin.epoch || journal.state != RestoreRunState::Ready)
        {
            return;
        }
        let selections = journal
            .entries
            .iter()
            .filter(|e| phase == RestorePhase::Reconcile || e.state == RestoreEntryState::Planned)
            .map(|e| e.plan.selection.clone())
            .collect();
        self.library_workflow.selected_journal = Some(journal.id.clone());
        self.library_workflow.restore = Some(RestoreRun {
            origin,
            phase,
            selections,
            current: None,
            observed: BTreeMap::new(),
            waiting: None,
            durable: None,
            write: None,
            write_started: None,
            write_readback: None,
            write_verified: false,
            journal: Some(journal.id),
            target: None,
        });
    }
    pub(crate) fn pause_batch_restore(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        let journal = self.library_workflow.restore.take().and_then(|r| r.journal);
        if let Some(journal) = journal {
            self.workflow_submit(
                WorkflowAction::PauseRestore {
                    journal,
                    reason: reason.clone(),
                },
                Purpose::RestorePause,
            );
        }
        self.library_workflow.message = reason;
    }
    pub(crate) fn observe_library_device(&mut self, event: &DeviceEvent) {
        match event {
            DeviceEvent::ConnectionChanged { epoch, state }
                if *state != ConnectionState::Ready || *epoch != self.controller.state().epoch =>
            {
                if let Some(run) = &mut self.library_workflow.capture {
                    run.stop = Some(BackupAcquisition::Failed {
                        reason: crate::localization::text("Connection changed during acquisition")
                            .into(),
                    });
                    run.waiting = None;
                }
                if self.library_workflow.restore.is_some() {
                    self.pause_batch_restore(crate::localization::text("Connection changed. Restore paused; uncertain writes require fresh reconciliation."));
                }
            }
            DeviceEvent::SnapshotReceived {
                stamp,
                target,
                snapshot,
            } => {
                let object = match target {
                    PresetTarget::EditBuffer => CaptureObject::Current,
                    PresetTarget::Stored(s) => CaptureObject::Stored(*s),
                };
                if self.library_workflow.accepts_capture(*stamp, object) {
                    let backup = self
                        .library_workflow
                        .capture
                        .as_ref()
                        .and_then(|r| r.id.clone())
                        .expect("begun backup");
                    let intent = self.workflow_submit(
                        WorkflowAction::CapturePreset {
                            backup,
                            snapshot: snapshot.clone(),
                        },
                        Purpose::BackupCapture,
                    );
                    if let Some(r) = &mut self.library_workflow.capture {
                        r.waiting = None;
                        r.durable = intent;
                        if intent.is_none() {
                            r.stop = Some(BackupAcquisition::Failed {
                                reason: crate::localization::text(
                                    "Library capture could not be submitted",
                                )
                                .into(),
                            });
                        }
                    }
                }
                if self.library_workflow.accepts_restore(*stamp, object)
                    && let Some(r) = &mut self.library_workflow.restore
                {
                    match target {
                        PresetTarget::EditBuffer => r.current = Some(snapshot.clone()),
                        PresetTarget::Stored(s) => {
                            r.observed.insert(*s, snapshot.clone());
                        }
                    };
                    r.waiting = None;
                }
                if let Some(r) = &mut self.library_workflow.restore
                    && r.write == Some(*stamp)
                    && *target == PresetTarget::Stored(r.target.expect("writing target"))
                {
                    r.write_readback = Some(snapshot.clone());
                }
            }
            DeviceEvent::GlobalStateReceived { stamp, state }
                if self
                    .library_workflow
                    .accepts_capture(*stamp, CaptureObject::Globals) =>
            {
                let backup = self
                    .library_workflow
                    .capture
                    .as_ref()
                    .and_then(|r| r.id.clone())
                    .expect("begun");
                let intent = self.workflow_submit(
                    WorkflowAction::CaptureGlobals {
                        backup,
                        globals: state.clone(),
                    },
                    Purpose::BackupCapture,
                );
                if let Some(r) = &mut self.library_workflow.capture {
                    r.waiting = None;
                    r.durable = intent;
                    if intent.is_none() {
                        r.stop = Some(BackupAcquisition::Failed {
                            reason: crate::localization::text(
                                "Global capture could not be submitted",
                            )
                            .into(),
                        });
                    }
                }
            }
            DeviceEvent::AssetMetadataReceived { stamp, metadata }
                if self
                    .library_workflow
                    .accepts_capture(*stamp, CaptureObject::Asset(metadata.slot())) =>
            {
                let backup = self
                    .library_workflow
                    .capture
                    .as_ref()
                    .and_then(|r| r.id.clone())
                    .expect("begun");
                let intent = self.workflow_submit(
                    WorkflowAction::CaptureAssetMetadata {
                        backup,
                        epoch: stamp.epoch,
                        metadata: metadata.metadata().clone(),
                    },
                    Purpose::BackupCapture,
                );
                if let Some(r) = &mut self.library_workflow.capture {
                    r.waiting = None;
                    r.durable = intent;
                    if intent.is_none() {
                        r.stop = Some(BackupAcquisition::Failed {
                            reason: crate::localization::text(
                                "Asset metadata capture could not be submitted",
                            )
                            .into(),
                        });
                    }
                }
            }
            DeviceEvent::OperationFailed { stamp, error } => {
                if self
                    .library_workflow
                    .capture
                    .as_ref()
                    .is_some_and(|r| r.waiting.as_ref().is_some_and(|(s, _, _)| s == stamp))
                {
                    self.capture_read_error(error.message.clone(), true);
                }
                if self.library_workflow.restore.as_ref().is_some_and(|r| {
                    r.write == Some(*stamp)
                        || r.waiting.as_ref().is_some_and(|(s, _, _)| s == stamp)
                }) {
                    self.pause_batch_restore(std::format!(
                        "Restore paused: {}. Read back before any retry.",
                        error.message
                    ));
                }
            }
            DeviceEvent::EditProgress { stamp, stage }
                if self
                    .library_workflow
                    .restore
                    .as_ref()
                    .is_some_and(|r| r.write == Some(*stamp)) =>
            {
                match stage {
                    EditStage::Verified => {
                        if let Some(r) = &mut self.library_workflow.restore {
                            r.write_verified = true;
                        }
                    }
                    EditStage::Failed(e) | EditStage::Indeterminate(e) => {
                        self.pause_batch_restore(std::format!(
                            "Restore paused: {}. No write will replay automatically.",
                            e.message
                        ))
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    fn capture_read_error(&mut self, message: String, stop: bool) {
        let Some(run) = &mut self.library_workflow.capture else {
            return;
        };
        let object = run
            .waiting
            .take()
            .map(|(_, o, _)| o)
            .or_else(|| run.object());
        if stop {
            run.stop = Some(BackupAcquisition::Failed {
                reason: message.clone(),
            });
        }
        let Some(backup) = run.id.clone() else {
            return;
        };
        let intent = self.workflow_submit(
            WorkflowAction::RecordReadError {
                backup,
                object: object.map_or_else(|| "capture".into(), CaptureObject::label),
                message,
            },
            Purpose::BackupCapture,
        );
        if let Some(r) = &mut self.library_workflow.capture {
            r.durable = intent;
        }
    }
    pub(crate) fn observe_workflow_event(&mut self, event: WorkflowEvent) {
        if self.startup_import == Some(event.intent) {
            self.startup_import = None;
            self.screenshot_frames = 0;
        }
        let Some(purpose) = self.library_workflow.pending.remove(&event.intent) else {
            return;
        };
        let result = match event.result {
            Ok(r) => r,
            Err(error) => {
                self.services.error(error.message.clone());
                self.library_workflow.message = error.message.clone();
                if matches!(
                    purpose,
                    Purpose::BackupBegin | Purpose::BackupCapture | Purpose::BackupFinish
                ) {
                    if matches!(purpose, Purpose::BackupBegin | Purpose::BackupFinish) {
                        self.fail_backup_qa(error.message.clone());
                        self.library_workflow.capture = None;
                    } else if let Some(r) = &mut self.library_workflow.capture {
                        r.durable = None;
                        r.stop = Some(BackupAcquisition::Failed {
                            reason: error.message,
                        });
                    }
                }
                if matches!(
                    purpose,
                    Purpose::Plan
                        | Purpose::RestoreBegin
                        | Purpose::RestoreDispatch
                        | Purpose::RestoreConfirm
                        | Purpose::RestoreReconcile
                        | Purpose::RestoreRetry
                ) {
                    self.pause_batch_restore(crate::localization::text("Library checkpoint failed. Restore paused; retained before-images remain available."));
                }
                return;
            }
        };
        match (purpose, result) {
            (Purpose::Backups, WorkflowResult::Backups(b)) => {
                if self.library_workflow.selected_backup.is_none() {
                    self.library_workflow.selected_backup = b.first().map(|b| b.id.clone());
                }
                self.library_workflow.backups = b;
            }
            (Purpose::Setlists, WorkflowResult::Setlists(s)) => {
                if self.library_workflow.setlist.is_none()
                    && let Some(first) = s.iter().find(|s| !s.trashed)
                {
                    self.library_workflow.selected_entry =
                        first.entries.first().map(|e| e.id.clone());
                    self.library_workflow.setlist = Some(first.clone());
                    self.workflow_submit(
                        WorkflowAction::ResolveSetlist(first.id.clone()),
                        Purpose::Resolve,
                    );
                }
                self.library_workflow.setlists = s;
            }
            (Purpose::Restores, WorkflowResult::RestoreJournals(j)) => {
                self.library_workflow.journals = j
            }
            (Purpose::Setlist, WorkflowResult::Setlist(s)) => {
                self.library_workflow.setlist_dirty = false;
                self.library_workflow.setlist = Some(s.clone());
                self.workflow_submit(WorkflowAction::ResolveSetlist(s.id), Purpose::Resolve);
                self.workflow_submit(
                    WorkflowAction::ListSetlists {
                        include_trash: self.library_workflow.show_setlist_trash,
                    },
                    Purpose::Setlists,
                );
            }
            (Purpose::Resolve, WorkflowResult::ResolvedSetlist(s)) => {
                if self
                    .library_workflow
                    .setlist
                    .as_ref()
                    .is_none_or(|current| current.id == s.setlist.id)
                    && !self.library_workflow.setlist_dirty
                {
                    if self
                        .library_workflow
                        .selected_entry
                        .as_ref()
                        .is_none_or(|id| !s.setlist.entries.iter().any(|e| &e.id == id))
                    {
                        self.library_workflow.selected_entry =
                            s.setlist.entries.first().map(|e| e.id.clone());
                    }
                    self.library_workflow.setlist = Some(s.setlist.clone());
                    self.library_workflow.resolved = Some(s);
                }
            }
            (Purpose::Compare, WorkflowResult::Differences(d)) => {
                self.library_workflow.comparison = Some(d)
            }
            (Purpose::BackupBegin, WorkflowResult::Backup(b)) => {
                if let Some(r) = &mut self.library_workflow.capture
                    && r.durable == Some(event.intent)
                {
                    r.id = Some(b.id.clone());
                    r.durable = None;
                }
                self.library_workflow.selected_backup = Some(b.id.clone());
                self.upsert_backup(b);
            }
            (Purpose::BackupCapture, _) => {
                if let Some(r) = &mut self.library_workflow.capture
                    && r.durable == Some(event.intent)
                {
                    r.durable = None;
                    r.cursor += 1;
                    r.completed += 1;
                }
            }
            (Purpose::BackupFinish, WorkflowResult::Backup(b)) => {
                if self
                    .capture_backup_qa
                    .as_ref()
                    .is_some_and(|qa| qa.started.is_some() && !qa.ready && !qa.failed)
                {
                    if b.acquisition == BackupAcquisition::Finished {
                        if let Some(qa) = &mut self.capture_backup_qa {
                            qa.ready = true;
                        }
                        self.screenshot_frames = 0;
                    } else {
                        self.fail_backup_qa(std::format!(
                            "Device capture ended as {:?}; {} stored presets remain retained",
                            b.acquisition,
                            b.presets.len()
                        ));
                    }
                }
                self.library_workflow.capture = None;
                self.library_workflow.message = b.completeness().label.clone();
                self.upsert_backup(b);
            }
            (Purpose::Export, WorkflowResult::Backup(b)) => {
                self.library_workflow.selected_backup = Some(b.id.clone());
                self.upsert_backup(b);
                self.library_workflow.message=crate::localization::text("Device archive imported. Its original provenance is retained; hardware state must be revalidated.").into();
            }
            (Purpose::Export, WorkflowResult::Completeness(c)) => {
                self.library_workflow.message = std::format!(
                    "Archive exported: {}. {} asset contents remain missing.",
                    c.label,
                    c.missing_asset_content.len()
                )
            }
            (Purpose::Plan, WorkflowResult::RestorePlan(p)) => {
                self.library_workflow.restore = None;
                self.library_workflow.accept_unknown_firmware = false;
                self.library_workflow.message = crate::localization::text(
                    "Review every destination, difference and exclusion before applying.",
                )
                .into();
                self.library_workflow.plan = Some(p);
            }
            (Purpose::RestoreBegin, WorkflowResult::RestoreJournal(j)) => {
                self.upsert_journal(j.clone());
                if self
                    .library_workflow
                    .restore
                    .as_ref()
                    .is_some_and(|r| r.durable == Some(event.intent))
                {
                    self.run_restore_journal(j, RestorePhase::Dispatching);
                } else {
                    self.workflow_submit(
                        WorkflowAction::PauseRestore {
                            journal: j.id,
                            reason: crate::localization::text(
                                "Restore was stopped before its journal creation completed",
                            )
                            .into(),
                        },
                        Purpose::RestorePause,
                    );
                }
            }
            (Purpose::RestoreDispatch, WorkflowResult::RestoreDispatch(d)) => {
                if self.library_workflow.restore.as_ref().is_some_and(|r| {
                    r.durable == Some(event.intent)
                        && r.origin.epoch == d.epoch
                        && r.journal.as_ref() == Some(&d.journal)
                }) {
                    let effects =
                        self.controller
                            .dispatch(AppInput::Ui(UiIntent::WriteStoredPreset {
                                slot: d.target,
                                expected_target_revision: d.expected_target_revision,
                                snapshot: d.snapshot,
                            }));
                    let stamp = effects.iter().find_map(|e| match e {
                        Effect::Device(DeviceCommand::WriteStoredPreset {
                            stamp, slot, ..
                        }) if *slot == d.target => Some(*stamp),
                        _ => None,
                    });
                    if let Some(r) = &mut self.library_workflow.restore {
                        r.durable = None;
                        r.target = Some(d.target);
                        r.write = stamp;
                        r.write_started = stamp.map(|_| Instant::now());
                        r.phase = RestorePhase::Writing;
                    }
                    self.services.enqueue(effects);
                    if stamp.is_none() {
                        self.pause_batch_restore(crate::localization::text("The device did not accept the reviewed stored-slot write. Reconcile this durable InFlight checkpoint before retrying."));
                    }
                }
            }
            (Purpose::RestoreConfirm, WorkflowResult::RestoreJournal(j)) => {
                self.upsert_journal(j.clone());
                let owned = self
                    .library_workflow
                    .restore
                    .as_ref()
                    .is_some_and(|r| r.durable == Some(event.intent));
                if owned && j.state == RestoreRunState::Ready {
                    self.run_restore_journal(j, RestorePhase::Dispatching);
                } else if owned {
                    self.library_workflow.restore = None;
                    self.library_workflow.message = std::format!(
                        "Restore {:?}: {} verified destinations.",
                        j.state,
                        j.verified_count()
                    );
                }
            }
            (
                Purpose::RestorePause | Purpose::RestoreReconcile | Purpose::RestoreRetry,
                WorkflowResult::RestoreJournal(j),
            ) => {
                self.library_workflow.restore = None;
                self.library_workflow.message = std::format!(
                    "Restore {:?}. Review its checkpoints before continuing.",
                    j.state
                );
                self.upsert_journal(j);
            }
            _ => {}
        }
    }
    fn upsert_backup(&mut self, b: DeviceBackup) {
        self.library_workflow.backups.retain(|old| old.id != b.id);
        self.library_workflow.backups.push(b);
    }
    fn upsert_journal(&mut self, j: RestoreJournal) {
        self.library_workflow.journals.retain(|old| old.id != j.id);
        self.library_workflow.journals.push(j);
    }
    fn workflow_read(&mut self, object: CaptureObject) -> Option<RequestStamp> {
        let effects = self.controller.dispatch(AppInput::Ui(object.intent()));
        let stamp = effects.iter().find_map(|e| match e {
            Effect::Device(c) => object.matches(c),
            _ => None,
        });
        self.services.enqueue(effects);
        stamp
    }
    fn fail_backup_qa(&mut self, message: String) {
        if let Some(qa) = &mut self.capture_backup_qa {
            qa.failed = true;
            self.qa_result
                .store(3, std::sync::atomic::Ordering::Relaxed);
            eprintln!("--capture-device-backup failed: {message}");
            self.services.error(message);
        }
    }
    pub(crate) fn advance_library_workflows(&mut self) {
        if self
            .capture_backup_qa
            .as_ref()
            .is_some_and(|qa| qa.started.is_none() && !qa.failed)
            && self.controller.state().connection == ConnectionState::Ready
            && self.controller.state().confirmed_buffer.is_some()
            && !self.controller.device_operation_pending()
            && self.global_qa.as_ref().is_none_or(|qa| qa.ready)
            && self.ir_metadata_qa.as_ref().is_none_or(|qa| qa.ready)
            && self
                .bank_qa
                .as_ref()
                .is_none_or(|qa| !qa.failed && self.bank_scan.completed.len() == 256)
            && !self.bank_scan.running
            && !self.asset_slot_ui.is_running()
        {
            if let Some(qa) = &mut self.capture_backup_qa {
                qa.started = Some(Instant::now());
            }
            self.start_device_backup();
            if self.library_workflow.capture.is_none() {
                self.fail_backup_qa(
                    crate::localization::text("The durable bank capture could not start").into(),
                );
            }
        }
        if self.capture_backup_qa.as_ref().is_some_and(|qa| {
            !qa.ready
                && !qa.failed
                && qa
                    .started
                    .is_some_and(|started| started.elapsed().as_secs() >= 90)
        }) {
            if let Some(r) = &mut self.library_workflow.capture {
                r.stop = Some(BackupAcquisition::Failed {
                    reason: crate::localization::text("Startup capture exceeded 90 seconds").into(),
                });
            }
            self.fail_backup_qa(
                crate::localization::text(
                    "Capture exceeded 90 seconds; previously accepted objects remain durable",
                )
                .into(),
            );
        }
        // Library acknowledgements are a durable barrier, not a cosmetic progress event.
        let capture_ready = self
            .library_workflow
            .capture
            .as_ref()
            .is_some_and(|r| r.durable.is_none() && r.waiting.is_none() && r.id.is_some());
        if capture_ready {
            let r = self.library_workflow.capture.as_ref().expect("checked");
            if let Some(outcome) = r
                .stop
                .clone()
                .or_else(|| r.object().is_none().then_some(BackupAcquisition::Finished))
            {
                let backup = r.id.clone().expect("begun");
                let intent = self.workflow_submit(
                    WorkflowAction::FinishBackup { backup, outcome },
                    Purpose::BackupFinish,
                );
                if let Some(r) = &mut self.library_workflow.capture {
                    r.durable = intent;
                }
                if intent.is_none() {
                    self.library_workflow.capture = None;
                }
            } else if !self.controller.device_operation_pending() {
                let object = r.object().expect("object");
                let operation = match object {
                    CaptureObject::Current => OperationKind::ReadEditBuffer,
                    CaptureObject::Stored(_) => OperationKind::ReadStoredPreset,
                    CaptureObject::Globals => OperationKind::ReadGlobals,
                    CaptureObject::Asset(s) => {
                        if s.bank() == AssetBank::Ir {
                            OperationKind::IrRead
                        } else {
                            OperationKind::SnapToneRead
                        }
                    }
                };
                let allowed = self
                    .controller
                    .state()
                    .identity
                    .as_ref()
                    .is_some_and(|i| self.controller.state().capabilities.allows(operation, i));
                if !allowed {
                    self.capture_read_error(
                        std::format!(
                            "Verified {:?} profile unavailable; this object remains missing.",
                            operation
                        ),
                        false,
                    );
                } else {
                    let stamp = self.workflow_read(object);
                    if let Some(r) = &mut self.library_workflow.capture {
                        r.waiting = stamp.map(|s| (s, object, Instant::now()));
                    }
                    if stamp.is_none() {
                        self.capture_read_error(
                            crate::localization::text("The device could not start this read")
                                .into(),
                            true,
                        );
                    }
                }
            }
        }
        if self.library_workflow.capture.as_ref().is_some_and(|r| {
            r.waiting
                .as_ref()
                .is_some_and(|(_, _, started)| started.elapsed().as_secs() >= 12)
        }) {
            self.capture_read_error(
                crate::localization::text("Capture read timed out; partial backup retained").into(),
                true,
            );
        }
        if self.library_workflow.restore.as_ref().is_some_and(|r| {
            r.waiting
                .as_ref()
                .is_some_and(|(_, _, started)| started.elapsed().as_secs() >= 12)
        }) {
            self.pause_batch_restore(crate::localization::text(
                "Destination read timed out. Restore paused; no writes replay automatically.",
            ));
            return;
        }
        if self.library_workflow.restore.as_ref().is_some_and(|r| {
            r.phase == RestorePhase::Writing
                && r.write_started
                    .is_some_and(|started| started.elapsed().as_secs() >= 12)
        }) {
            self.pause_batch_restore(crate::localization::text("Stored-slot acknowledgement timed out. Read back and reconcile; this write will not replay automatically."));
            return;
        }
        let Some(r) = self.library_workflow.restore.as_ref() else {
            return;
        };
        if r.origin.epoch != self.controller.state().epoch
            || self.controller.state().connection != ConnectionState::Ready
        {
            self.pause_batch_restore(crate::localization::text(
                "Session changed. Read back all targets before retrying.",
            ));
            return;
        }
        if r.waiting.is_some() || r.durable.is_some() || self.controller.device_operation_pending()
        {
            return;
        }
        if r.phase == RestorePhase::Writing {
            if r.write_verified
                && let Some(actual) = r.write_readback.clone()
            {
                let journal = r.journal.clone().expect("writing journal");
                let intent = self.workflow_submit(
                    WorkflowAction::ConfirmRestore { journal, actual },
                    Purpose::RestoreConfirm,
                );
                if let Some(r) = &mut self.library_workflow.restore {
                    r.durable = intent;
                    r.phase = RestorePhase::Confirming;
                }
                if intent.is_none() {
                    self.pause_batch_restore(crate::localization::text(
                        "Readback confirmation could not be saved. Reconcile before retrying.",
                    ));
                }
            }
            return;
        }
        if r.phase == RestorePhase::Confirming {
            return;
        }
        let target = r
            .selections
            .iter()
            .map(|s| s.target)
            .find(|s| !r.observed.contains_key(s));
        let object = if r.current.is_none() {
            Some(CaptureObject::Current)
        } else {
            target.map(CaptureObject::Stored)
        };
        if let Some(object) = object {
            let stamp = self.workflow_read(object);
            if let Some(r) = &mut self.library_workflow.restore {
                r.waiting = stamp.map(|s| (s, object, Instant::now()));
            }
            if stamp.is_none() {
                self.pause_batch_restore(crate::localization::text(
                    "The device could not start the destination read. Reconnect and review again.",
                ));
            }
            return;
        }
        let r = self.library_workflow.restore.as_ref().expect("run");
        let action = match r.phase {
            RestorePhase::Planning => WorkflowAction::PlanRestore(RestoreRequest {
                target: r.origin.clone(),
                current_buffer: r.current.clone().expect("read"),
                selections: r.selections.clone(),
                observed_targets: r.observed.clone(),
                asset_bindings: Vec::new(),
                target_assets: Vec::new(),
            }),
            RestorePhase::Reconcile => WorkflowAction::ReconcileRestore {
                journal: r.journal.clone().expect("journal"),
                origin: r.origin.clone(),
                readbacks: r.observed.clone(),
            },
            RestorePhase::Dispatching => {
                let Some(target) = r.selections.first().map(|s| s.target) else {
                    self.library_workflow.restore = None;
                    return;
                };
                WorkflowAction::DispatchRestore {
                    journal: r.journal.clone().expect("journal"),
                    target,
                    observed: r.observed[&target].clone(),
                    current_buffer: r.current.clone().expect("current"),
                }
            }
            _ => return,
        };
        let purpose = match r.phase {
            RestorePhase::Planning => Purpose::Plan,
            RestorePhase::Reconcile => Purpose::RestoreReconcile,
            _ => Purpose::RestoreDispatch,
        };
        let intent = self.workflow_submit(action, purpose);
        if let Some(r) = &mut self.library_workflow.restore {
            r.durable = intent;
        }
        if intent.is_none() {
            self.pause_batch_restore(crate::localization::text(
                "Library checkpoint queue unavailable. Review again before any dispatch.",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimeServices;
    use gp200_app::AppController;
    use std::{path::PathBuf, sync::Mutex, time::Duration};

    struct RecordingSimulator {
        inner: Box<dyn DeviceService>,
        commands: Arc<Mutex<Vec<DeviceCommand>>>,
    }
    impl DeviceService for RecordingSimulator {
        fn try_submit(&self, c: DeviceCommand) -> Result<(), SubmitError<DeviceCommand>> {
            let result = self.inner.try_submit(c.clone());
            if result.is_ok() {
                self.commands.lock().unwrap().push(c);
            }
            result
        }
        fn try_next_event(&self) -> Option<DeviceEvent> {
            self.inner.try_next_event()
        }
        fn shutdown(&self) {
            self.inner.shutdown();
        }
    }
    fn app() -> (StudioApp, PathBuf, Arc<Mutex<Vec<DeviceCommand>>>) {
        let root = std::env::temp_dir().join(format!(
            "gp200-gui-workflow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let commands = Arc::new(Mutex::new(Vec::new()));
        let wake: WakeUi = Arc::new(|| {});
        let device = RecordingSimulator {
            inner: Box::new(gp200_sim::simulator_service(wake.clone())),
            commands: commands.clone(),
        };
        let mut services = RuntimeServices::new(Box::new(device), wake.clone());
        let worker = LibraryWorker::open(&root, wake).unwrap();
        services.workflows = Some(worker.workflow_client());
        services.library = Some(Box::new(worker));
        let controller = AppController::with_demo(
            gp200_sim::captured_demo_snapshot(),
            AppPreferences::default(),
        )
        .unwrap();
        (
            StudioApp::with_services(controller, services),
            root,
            commands,
        )
    }
    fn pump(app: &mut StudioApp) {
        let (inputs, _) = app.services.pump();
        for input in inputs {
            if let AppInput::Device(event) = &input {
                app.observe_library_device(event);
            }
            let refresh = matches!(
                &input,
                AppInput::Library(
                    LibraryEvent::VersionStored { .. }
                        | LibraryEvent::Imported { .. }
                        | LibraryEvent::EntryChanged { .. }
                )
            );
            let effects = app.controller.dispatch(input);
            app.services.enqueue(effects);
            if refresh {
                app.query_library();
            }
        }
        for event in app.services.pump_workflows() {
            app.observe_workflow_event(event);
        }
        let effects = app
            .controller
            .dispatch(AppInput::Tick(app.started.elapsed().as_millis() as u64));
        app.services.enqueue(effects);
        app.advance_library_workflows();
    }
    fn until(app: &mut StudioApp, ready: impl Fn(&StudioApp) -> bool) {
        until_with_budget(app, Duration::from_secs(20), ready);
    }
    fn until_with_budget(
        app: &mut StudioApp,
        budget: Duration,
        ready: impl Fn(&StudioApp) -> bool,
    ) {
        let deadline = Instant::now() + budget;
        while !ready(app) {
            pump(app);
            assert!(
                Instant::now() < deadline,
                "timed out: {:?} / {:?}",
                app.library_workflow.message,
                app.services.errors
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn reopen(root: &std::path::Path) -> LibraryStore {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match LibraryStore::open(root) {
                Ok(store) => return store,
                Err(e) if e.kind == ErrorKind::Busy && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) => panic!("reopen failed: {e}"),
            }
        }
    }
    fn connect(app: &mut StudioApp) {
        app.send(UiIntent::Connect(gp200_sim::simulator_port().selection));
        until(app, |a| {
            a.controller.state().connection == ConnectionState::Ready
                && a.controller.state().confirmed_buffer.is_some()
        });
    }
    #[test]
    fn full_capture_uses_real_worker_durable_barriers_and_never_claims_asset_contents() {
        let (mut app, root, commands) = app();
        connect(&mut app);
        app.start_device_backup();
        // Until BeginBackup has completed there is no pedal read at all.
        let baseline = commands.lock().unwrap().len();
        app.advance_library_workflows();
        app.advance_library_workflows();
        assert_eq!(commands.lock().unwrap().len(), baseline);
        // This performs 256 real durable writes. Parallel tests can share a
        // slow CI filesystem; keep their observation budget separate from
        // production device deadlines.
        until_with_budget(&mut app, Duration::from_secs(90), |a| {
            a.library_workflow.capture.is_none()
        });
        assert_eq!(app.library_workflow.backups.len(), 1);
        let backup = &app.library_workflow.backups[0];
        let complete = backup.completeness();
        assert_eq!(complete.stored_presets, 256);
        assert!(complete.edit_buffer_captured);
        assert!(complete.globals_captured);
        assert!(complete.global_mappings_captured);
        assert_eq!(complete.missing_asset_content.len(), 30);
        assert!(!complete.complete_device_state);
        assert!(complete.requires_hardware_revalidation);
        let commands = commands.lock().unwrap();
        assert_eq!(
            commands
                .iter()
                .filter(|c| matches!(
                    c,
                    DeviceCommand::RefreshTarget {
                        target: PresetTarget::Stored(_),
                        ..
                    }
                ))
                .count(),
            256
        );
        assert!(!commands.iter().any(|c| matches!(
            c,
            DeviceCommand::WriteStoredPreset { .. }
                | DeviceCommand::SaveBufferTo { .. }
                | DeviceCommand::ExecuteDeviceJob { .. }
        )));
        drop(commands);
        drop(app);
        let reopened = reopen(&root);
        assert_eq!(reopened.device_backups().unwrap()[0].presets.len(), 256);
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cancel_during_acquisition_keeps_partial_capture_and_does_not_resume_after_epoch_change() {
        let (mut app, root, commands) = app();
        connect(&mut app);
        app.start_device_backup();
        until(&mut app, |a| {
            a.library_workflow
                .capture
                .as_ref()
                .is_some_and(|r| r.cursor >= 4)
        });
        app.stop_device_backup();
        until(&mut app, |a| a.library_workflow.capture.is_none());
        let backup = &app.library_workflow.backups[0];
        assert_eq!(backup.acquisition, BackupAcquisition::Cancelled);
        assert!(!backup.presets.is_empty());
        assert!(backup.presets.len() < 256);
        let before = commands.lock().unwrap().len();
        app.observe_library_device(&DeviceEvent::ConnectionChanged {
            epoch: SessionEpoch(99),
            state: ConnectionState::Disconnected,
        });
        for _ in 0..4 {
            pump(&mut app);
        }
        assert_eq!(commands.lock().unwrap().len(), before);
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn stale_or_unowned_capture_reply_does_not_advance_or_publish_a_snapshot() {
        let (mut app, root, _) = app();
        connect(&mut app);
        app.start_device_backup();
        until(&mut app, |a| {
            a.library_workflow
                .capture
                .as_ref()
                .is_some_and(|r| r.id.is_some())
        });
        let run = app.library_workflow.capture.as_ref().unwrap();
        let before = run.cursor;
        let epoch = run.origin.epoch;
        app.observe_library_device(&DeviceEvent::SnapshotReceived {
            stamp: RequestStamp {
                epoch,
                intent: IntentId(0),
            },
            target: PresetTarget::Stored(SlotId::new(9).unwrap()),
            snapshot: gp200_sim::captured_demo_snapshot(),
        });
        assert_eq!(
            app.library_workflow.capture.as_ref().unwrap().cursor,
            before
        );
        app.stop_device_backup();
        until(&mut app, |a| a.library_workflow.capture.is_none());
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn bounded_workflow_adapter_retains_intents_and_reports_retired_worker() {
        let (mut app, root, _) = app();
        for intent in 1..=64 {
            assert!(
                app.services
                    .submit_workflow(WorkflowCommand {
                        intent: IntentId(intent),
                        action: WorkflowAction::ListBackups
                    })
                    .is_ok()
            );
        }
        assert_eq!(
            app.services
                .submit_workflow(WorkflowCommand {
                    intent: IntentId(65),
                    action: WorkflowAction::ListBackups
                })
                .unwrap_err()
                .kind,
            ErrorKind::Busy
        );
        app.services.library.as_ref().unwrap().shutdown();
        let events = app.services.pump_workflows();
        assert_eq!(events.len(), 64);
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event.intent, IntentId(index as u64 + 1));
            assert_eq!(
                event.result.as_ref().unwrap_err().kind,
                ErrorKind::Disconnected
            );
        }
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn setlist_save_reload_keeps_pinned_revision_and_navigation_sends_no_device_command() {
        let (mut app, root, commands) = app();
        let intent = app.capture_library().unwrap();
        until(&mut app, |a| {
            !a.controller.state().library.entries.is_empty()
        });
        let version = app.controller.state().library.entries[0]
            .latest_version
            .clone();
        assert!(intent.0 > 0);
        app.workflow_submit(
            WorkflowAction::CreateSetlist("Live set".into()),
            Purpose::Setlist,
        );
        until(&mut app, |a| {
            a.library_workflow.setlist.is_some() && a.library_workflow.pending.is_empty()
        });
        let mut setlist = app.library_workflow.setlist.clone().unwrap();
        setlist.entries.push(SetlistEntry {
            id: "song-one".into(),
            version: version.clone(),
            song: "First song".into(),
            section: "Verse".into(),
            notes: "Clean opening".into(),
            tempo_fallback: Some(96),
            proposed_slot: Some(SlotId::new(254).unwrap()),
        });
        let mut second = setlist.entries[0].clone();
        second.id = "song-two".into();
        second.song = "Second song".into();
        setlist.entries.push(second);
        app.workflow_submit(
            WorkflowAction::SaveSetlist {
                expected_revision: setlist.revision,
                setlist,
            },
            Purpose::Setlist,
        );
        until(&mut app, |a| a.library_workflow.pending.is_empty());
        app.library_workflow.selected_entry = Some("song-one".into());
        let before = commands.lock().unwrap().len();
        app.step_setlist(1);
        assert_eq!(
            app.library_workflow.selected_entry.as_deref(),
            Some("song-two")
        );
        assert_eq!(commands.lock().unwrap().len(), before);
        let id = app.library_workflow.setlist.as_ref().unwrap().id.clone();
        drop(app);
        let store = reopen(&root);
        let persisted = store.resolve_setlist(&id).unwrap();
        assert_eq!(persisted.entries[0].entry.version, version);
        assert_eq!(persisted.entries[0].entry.notes, "Clean opening");
        assert_eq!(persisted.entries[0].entry.tempo_fallback, Some(96));
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn concrete_plan_excludes_active_slot_and_restore_waits_for_actor_verified_readback() {
        let (mut app, root, commands) = app();
        connect(&mut app);
        let stored = app.capture_library().unwrap();
        until(&mut app, |a| {
            !a.controller.state().library.entries.is_empty()
        });
        assert!(stored.0 > 0);
        let version = app.controller.state().library.entries[0]
            .latest_version
            .clone();
        app.plan_restore_sources(vec![
            RestoreSelection {
                source: RestoreSource::LibraryVersion(version.clone()),
                target: SlotId::new(23).unwrap(),
            },
            RestoreSelection {
                source: RestoreSource::LibraryVersion(version),
                target: SlotId::new(254).unwrap(),
            },
        ]);
        until(&mut app, |a| a.library_workflow.plan.is_some());
        let plan = app.library_workflow.plan.as_ref().unwrap();
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].selection.target, SlotId::new(254).unwrap());
        assert_eq!(plan.excluded.len(), 1);
        assert!(
            !commands
                .lock()
                .unwrap()
                .iter()
                .any(|c| matches!(c, DeviceCommand::WriteStoredPreset { .. }))
        );
        app.library_workflow.accept_unknown_firmware = true;
        app.apply_restore_plan();
        until(&mut app, |a| {
            a.library_workflow
                .journals
                .iter()
                .any(|j| j.state == RestoreRunState::Completed)
        });
        let journal = &app.library_workflow.journals[0];
        assert_eq!(journal.verified_count(), 1);
        assert_eq!(journal.entries[0].attempts, 1);
        assert_eq!(
            commands
                .lock()
                .unwrap()
                .iter()
                .filter(|c| matches!(c, DeviceCommand::WriteStoredPreset { .. }))
                .count(),
            1
        );
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn native_workspaces_render_dark_light_compact_without_passive_mutation() {
        let (mut app, root, commands) = app();
        let context = eframe::egui::Context::default();
        for theme in [ThemePreference::Dark, ThemePreference::Light] {
            crate::theme::apply(&context, theme, 1.0);
            for size in [
                eframe::egui::vec2(960.0, 640.0),
                eframe::egui::vec2(1280.0, 800.0),
            ] {
                for tab in [LibraryTab::Backups, LibraryTab::Compare] {
                    app.library_workflow.tab = tab;
                    let mut output = context.run_ui(
                        eframe::egui::RawInput {
                            screen_rect: Some(eframe::egui::Rect::from_min_size(
                                eframe::egui::Pos2::ZERO,
                                size,
                            )),
                            ..Default::default()
                        },
                        |ui| app.library(ui),
                    );
                    assert!(!output.shapes.is_empty());
                    output.textures_delta.clear();
                }
                for arranging in [false, true] {
                    app.library_workflow.arranging = arranging;
                    let mut output = context.run_ui(
                        eframe::egui::RawInput {
                            screen_rect: Some(eframe::egui::Rect::from_min_size(
                                eframe::egui::Pos2::ZERO,
                                size,
                            )),
                            ..Default::default()
                        },
                        |ui| app.performance(ui),
                    );
                    assert!(!output.shapes.is_empty());
                    output.textures_delta.clear();
                }
            }
        }
        assert!(commands.lock().unwrap().is_empty());
        assert!(app.library_workflow.pending.is_empty());
        assert!(!app.services.has_pending());
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    struct SuppressVerified(Box<dyn DeviceService>);
    impl DeviceService for SuppressVerified {
        fn try_submit(&self, c: DeviceCommand) -> Result<(), SubmitError<DeviceCommand>> {
            self.0.try_submit(c)
        }
        fn try_next_event(&self) -> Option<DeviceEvent> {
            loop {
                let event = self.0.try_next_event()?;
                if !matches!(
                    event,
                    DeviceEvent::EditProgress {
                        stage: EditStage::Verified,
                        ..
                    }
                ) {
                    return Some(event);
                }
            }
        }
        fn shutdown(&self) {
            self.0.shutdown();
        }
    }
    #[test]
    fn lost_write_acknowledgement_pauses_durable_journal_without_replay() {
        let (mut app, root, commands) = app();
        app.services.device.shutdown();
        app.services.device = Box::new(RecordingSimulator {
            inner: Box::new(SuppressVerified(Box::new(gp200_sim::simulator_service(
                Arc::new(|| {}),
            )))),
            commands: commands.clone(),
        });
        connect(&mut app);
        app.capture_library();
        until(&mut app, |a| {
            !a.controller.state().library.entries.is_empty()
        });
        let version = app.controller.state().library.entries[0]
            .latest_version
            .clone();
        app.plan_restore_sources(vec![RestoreSelection {
            source: RestoreSource::LibraryVersion(version),
            target: SlotId::new(254).unwrap(),
        }]);
        until(&mut app, |a| a.library_workflow.plan.is_some());
        app.library_workflow.accept_unknown_firmware = true;
        app.apply_restore_plan();
        until(&mut app, |a| {
            a.library_workflow
                .restore
                .as_ref()
                .is_some_and(|r| r.write_readback.is_some())
        });
        let run = app.library_workflow.restore.as_mut().unwrap();
        assert!(!run.write_verified);
        run.write_started = Some(Instant::now() - Duration::from_secs(13));
        app.advance_library_workflows();
        until(&mut app, |a| {
            !a.library_workflow
                .pending
                .values()
                .any(|p| matches!(p, Purpose::RestorePause))
        });
        assert!(app.library_workflow.restore.is_none());
        let journal = app.library_workflow.journals[0].clone();
        assert!(matches!(journal.state, RestoreRunState::Paused { .. }));
        assert!(matches!(
            journal.entries[0].state,
            RestoreEntryState::Uncertain { .. }
        ));
        assert_eq!(
            commands
                .lock()
                .unwrap()
                .iter()
                .filter(|c| matches!(c, DeviceCommand::WriteStoredPreset { .. }))
                .count(),
            1
        );
        for _ in 0..8 {
            pump(&mut app);
        }
        assert_eq!(
            commands
                .lock()
                .unwrap()
                .iter()
                .filter(|c| matches!(c, DeviceCommand::WriteStoredPreset { .. }))
                .count(),
            1
        );
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn stopping_before_begin_journal_ack_prevents_late_automatic_dispatch() {
        let (mut app, root, commands) = app();
        connect(&mut app);
        app.capture_library();
        until(&mut app, |a| {
            !a.controller.state().library.entries.is_empty()
        });
        let version = app.controller.state().library.entries[0]
            .latest_version
            .clone();
        app.plan_restore_sources(vec![RestoreSelection {
            source: RestoreSource::LibraryVersion(version),
            target: SlotId::new(254).unwrap(),
        }]);
        until(&mut app, |a| a.library_workflow.plan.is_some());
        app.library_workflow.accept_unknown_firmware = true;
        app.apply_restore_plan();
        assert!(app.library_workflow.restore.is_some());
        app.pause_batch_restore("Stop before begin acknowledged");
        until(&mut app, |a| a.library_workflow.pending.is_empty());
        assert!(
            !commands
                .lock()
                .unwrap()
                .iter()
                .any(|c| matches!(c, DeviceCommand::WriteStoredPreset { .. }))
        );
        assert!(app.library_workflow.restore.is_none());
        assert!(matches!(
            app.library_workflow.journals[0].state,
            RestoreRunState::Paused { .. }
        ));
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn portable_archive_open_route_uses_workflow_worker_and_keeps_missing_content() {
        let (mut source, root, commands) = app();
        connect(&mut source);
        source.start_device_backup();
        until(&mut source, |a| {
            a.library_workflow
                .capture
                .as_ref()
                .is_some_and(|r| r.cursor >= 3)
        });
        source.stop_device_backup();
        until(&mut source, |a| a.library_workflow.capture.is_none());
        let id = source.library_workflow.backups[0].id.clone();
        let archive = root.with_extension("gp200backup");
        source.library_workflow.export_backup = Some(id);
        source.handle_file(
            crate::runtime::FilePurpose::ExportDeviceBackup,
            archive.clone(),
        );
        until(&mut source, |a| a.library_workflow.pending.is_empty());
        assert!(archive.exists());
        let (mut target, other, other_commands) = app();
        target.handle_file(crate::runtime::FilePurpose::ImportPresets, archive.clone());
        target.startup_import = Some(IntentId(target.next_local_intent));
        until(&mut target, |a| {
            a.library_workflow.backups.len() == 1 && a.startup_import.is_none()
        });
        let backup = &target.library_workflow.backups[0];
        assert!(!backup.presets.is_empty());
        assert_eq!(backup.completeness().missing_asset_content.len(), 30);
        assert!(!backup.completeness().complete_device_state);
        assert!(other_commands.lock().unwrap().is_empty());
        assert!(
            !commands
                .lock()
                .unwrap()
                .iter()
                .any(|c| matches!(c, DeviceCommand::WriteStoredPreset { .. }))
        );
        drop(source);
        drop(target);
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(other).unwrap();
        std::fs::remove_file(archive).unwrap();
    }
    #[test]
    fn real_egui_next_song_click_changes_only_stable_local_entry_selection() {
        use eframe::egui::{self, epaint::Shape};
        let (mut app, root, commands) = app();
        let entries = [("first", "Intro"), ("second", "Chorus")].map(|(id, song)| SetlistEntry {
            id: id.into(),
            version: LibraryVersionId("pinned-revision".into()),
            song: song.into(),
            section: String::new(),
            notes: String::new(),
            tempo_fallback: None,
            proposed_slot: None,
        });
        app.library_workflow.setlist = Some(Setlist {
            id: "setlist-click-test".into(),
            revision: Revision(0),
            name: "Keyboard rehearsal".into(),
            notes: String::new(),
            entries: entries.to_vec(),
            trashed: false,
        });
        app.library_workflow.selected_entry = Some("first".into());
        let ctx = egui::Context::default();
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 800.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input(), |ui| app.performance(ui));
        let pos = output
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                Shape::Text(text) if text.galley.job.text == "Next song" => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                _ => None,
            })
            .expect("actual Next song widget");
        output.textures_delta.clear();
        let mut click = input();
        click.events = vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ];
        let mut output = ctx.run_ui(click, |ui| app.performance(ui));
        output.textures_delta.clear();
        assert_eq!(
            app.library_workflow.selected_entry.as_deref(),
            Some("second")
        );
        assert!(commands.lock().unwrap().is_empty());
        assert!(app.library_workflow.pending.is_empty());
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn startup_capture_requires_explicit_backend_and_runs_exact_coordinator_once() {
        assert!(
            crate::LaunchOptions::parse(["--capture-device-backup"].map(str::to_owned)).is_err()
        );
        assert!(
            crate::LaunchOptions::parse(["--demo", "--capture-device-backup"].map(str::to_owned))
                .unwrap()
                .capture_device_backup
        );
        let (mut app, root, commands) = app();
        app.capture_backup_qa = Some(crate::GlobalQa {
            started: None,
            ready: false,
            failed: false,
        });
        app.send(UiIntent::Connect(gp200_sim::simulator_port().selection));
        until_with_budget(&mut app, Duration::from_secs(90), |a| {
            a.capture_backup_qa.as_ref().is_some_and(|qa| qa.ready)
        });
        assert!(!app.capture_backup_qa.as_ref().unwrap().failed);
        assert_eq!(app.library_workflow.backups.len(), 1);
        assert_eq!(app.library_workflow.backups[0].presets.len(), 256);
        assert_eq!(
            app.library_workflow.backups[0]
                .completeness()
                .missing_asset_content
                .len(),
            30
        );
        for _ in 0..8 {
            pump(&mut app);
        }
        assert_eq!(app.library_workflow.backups.len(), 1);
        assert!(!commands.lock().unwrap().iter().any(|c| matches!(
            c,
            DeviceCommand::WriteStoredPreset { .. } | DeviceCommand::ExecuteDeviceJob { .. }
        )));
        drop(app);
        std::fs::remove_dir_all(root).unwrap();
    }
}
