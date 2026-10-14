//! Optional typed worker surface for workflows whose domain/UI integration is owned by the app.
use crate::*;
use gp200_domain::*;
use gp200_protocol::AssetSlotMetadata;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};

#[derive(Debug, Clone)]
pub struct WorkflowCommand {
    pub intent: IntentId,
    pub action: WorkflowAction,
}
#[derive(Debug, Clone)]
pub enum WorkflowAction {
    BeginBackup(BackupOrigin),
    ListBackups,
    GetBackup(String),
    CapturePreset {
        backup: String,
        snapshot: Arc<PresetSnapshot>,
    },
    CaptureGlobals {
        backup: String,
        globals: Arc<DeviceGlobalState>,
    },
    CaptureAssetMetadata {
        backup: String,
        epoch: SessionEpoch,
        metadata: AssetSlotMetadata,
    },
    CaptureAssetContent {
        backup: String,
        epoch: SessionEpoch,
        slot: DeviceAssetSlot,
        bytes: Arc<Vec<u8>>,
        format: String,
        origin: AssetContentOrigin,
    },
    RecordReadError {
        backup: String,
        object: String,
        message: String,
    },
    FinishBackup {
        backup: String,
        outcome: BackupAcquisition,
    },
    ExportBackup {
        backup: String,
        path: PathBuf,
    },
    ImportBackup(PathBuf),
    PlanRestore(RestoreRequest),
    BeginRestore {
        plan: RestorePlan,
        review: RestoreReview,
    },
    ListRestores,
    GetRestore(String),
    DispatchRestore {
        journal: String,
        target: SlotId,
        observed: Arc<PresetSnapshot>,
        current_buffer: Arc<PresetSnapshot>,
    },
    ConfirmRestore {
        journal: String,
        actual: Arc<PresetSnapshot>,
    },
    PauseRestore {
        journal: String,
        reason: String,
    },
    ReconcileRestore {
        journal: String,
        origin: BackupOrigin,
        readbacks: BTreeMap<SlotId, Arc<PresetSnapshot>>,
    },
    ApproveRetry {
        journal: String,
        target: SlotId,
        observed_hash: ContentHash,
    },
    CreateSetlist(String),
    ListSetlists {
        include_trash: bool,
    },
    SaveSetlist {
        setlist: Setlist,
        expected_revision: Revision,
    },
    TrashSetlist {
        id: String,
        expected_revision: Revision,
        trashed: bool,
    },
    ResolveSetlist(String),
    CompareVersions {
        before: LibraryVersionId,
        after: LibraryVersionId,
    },
    AssetReplacementImpact(DeviceAssetSlot),
    ListIrHistory(IrHistoryPageRequest),
    GetIrHistory(IrAttemptId),
    ExportIrHistory(IrHistoryExportRequest),
    PrepareMovement(Box<PresetMovementRequest>),
    BindMovement {
        prepared: Box<PresetMovementReview>,
        reviewed: ContentHash,
    },
    BeginMovement {
        execution: Box<MovementExecutionReview>,
        reviewed: ContentHash,
        fresh: Box<PresetMovementObserved>,
    },
    ListMovements,
    GetMovement(String),
    DispatchMovement {
        journal: String,
        sequence: u64,
        fresh: Box<PresetMovementObserved>,
    },
    ConfirmMovement {
        journal: String,
        sequence: u64,
        actor_verified_target_readback: Arc<PresetSnapshot>,
        post: Box<PresetMovementObserved>,
    },
    PauseMovement {
        journal: String,
        sequence: u64,
        reason: String,
    },
    ReconcileMovement {
        journal: String,
        sequence: u64,
        fresh: Box<PresetMovementObserved>,
    },
    ContinueMovement {
        journal: String,
        reviewed_report: ContentHash,
        explicitly_reviewed_retry_targets: BTreeSet<SlotId>,
        fresh: Box<PresetMovementObserved>,
    },
}
#[derive(Debug, Clone)]
pub enum WorkflowResult {
    Backup(DeviceBackup),
    Backups(Vec<DeviceBackup>),
    Completeness(BackupCompleteness),
    RestorePlan(RestorePlan),
    RestoreJournal(RestoreJournal),
    RestoreJournals(Vec<RestoreJournal>),
    RestoreDispatch(RestoreDispatch),
    Setlist(Setlist),
    Setlists(Vec<Setlist>),
    ResolvedSetlist(ResolvedSetlist),
    Differences(PresetDiff),
    AssetReplacementImpact(AssetReplacementImpact),
    IrHistoryPage(IrHistoryPage),
    IrHistoryDetail(Arc<IrHistoryDetail>),
    IrHistoryExport(IrHistoryExportReceipt),
    Stored,
    MovementPreparation(Box<PresetMovementPreparation>),
    MovementExecutionReview(Box<MovementExecutionReview>),
    MovementJournal(Box<MovementJournal>),
    MovementJournals(Vec<MovementJournal>),
    MovementDispatch(Box<MovementDispatch>),
    MovementReconciliation(Box<MovementReconciliation>),
}
#[derive(Debug, Clone)]
pub struct WorkflowEvent {
    pub intent: IntentId,
    pub result: Result<WorkflowResult, ServiceError>,
}

pub(crate) fn execute_workflow(
    store: &mut LibraryStore,
    action: WorkflowAction,
    cancelled: &dyn Fn() -> bool,
) -> Result<WorkflowResult, ServiceError> {
    if cancelled() {
        return Err(ServiceError::new(
            ErrorKind::Cancelled,
            "Library workflow cancelled; previously captured items remain durable",
        ));
    }
    let movement_mutation = matches!(
        &action,
        WorkflowAction::BeginMovement { .. }
            | WorkflowAction::DispatchMovement { .. }
            | WorkflowAction::ConfirmMovement { .. }
            | WorkflowAction::PauseMovement { .. }
            | WorkflowAction::ReconcileMovement { .. }
            | WorkflowAction::ContinueMovement { .. }
    );
    let result = match action {
        WorkflowAction::BeginBackup(origin) => {
            WorkflowResult::Backup(store.begin_device_backup(origin)?)
        }
        WorkflowAction::ListBackups => WorkflowResult::Backups(store.device_backups()?),
        WorkflowAction::GetBackup(id) => WorkflowResult::Backup(store.device_backup(&id)?),
        WorkflowAction::CapturePreset { backup, snapshot } => {
            WorkflowResult::Completeness(store.capture_backup_preset(&backup, snapshot)?)
        }
        WorkflowAction::CaptureGlobals { backup, globals } => {
            store.capture_backup_globals(&backup, &globals)?;
            WorkflowResult::Stored
        }
        WorkflowAction::CaptureAssetMetadata {
            backup,
            epoch,
            metadata,
        } => {
            store.capture_backup_asset_metadata(&backup, epoch, &metadata)?;
            WorkflowResult::Stored
        }
        WorkflowAction::CaptureAssetContent {
            backup,
            epoch,
            slot,
            bytes,
            format,
            origin,
        } => {
            store.capture_backup_asset_content(&backup, epoch, slot, &bytes, &format, origin)?;
            WorkflowResult::Stored
        }
        WorkflowAction::RecordReadError {
            backup,
            object,
            message,
        } => {
            store.record_backup_read_error(&backup, &object, &message)?;
            WorkflowResult::Stored
        }
        WorkflowAction::FinishBackup { backup, outcome } => {
            WorkflowResult::Backup(store.finish_device_backup(&backup, outcome)?)
        }
        WorkflowAction::ExportBackup { backup, path } => WorkflowResult::Completeness(
            store.export_device_backup_cancellable(&backup, &path, cancelled)?,
        ),
        WorkflowAction::ImportBackup(path) => {
            WorkflowResult::Backup(store.import_device_backup_cancellable(&path, cancelled)?)
        }
        WorkflowAction::PlanRestore(request) => {
            WorkflowResult::RestorePlan(store.plan_restore(&request)?)
        }
        WorkflowAction::BeginRestore { plan, review } => {
            WorkflowResult::RestoreJournal(store.begin_restore(plan, review)?)
        }
        WorkflowAction::ListRestores => WorkflowResult::RestoreJournals(store.restore_journals()?),
        WorkflowAction::GetRestore(id) => {
            WorkflowResult::RestoreJournal(store.restore_journal(&id)?)
        }
        WorkflowAction::DispatchRestore {
            journal,
            target,
            observed,
            current_buffer,
        } => WorkflowResult::RestoreDispatch(store.begin_restore_entry(
            &journal,
            target,
            observed,
            current_buffer,
        )?),
        WorkflowAction::ConfirmRestore { journal, actual } => {
            WorkflowResult::RestoreJournal(store.confirm_restore_readback(&journal, actual)?)
        }
        WorkflowAction::PauseRestore { journal, reason } => {
            WorkflowResult::RestoreJournal(store.pause_restore(&journal, &reason)?)
        }
        WorkflowAction::ReconcileRestore {
            journal,
            origin,
            readbacks,
        } => WorkflowResult::RestoreJournal(store.reconcile_restore(&journal, origin, readbacks)?),
        WorkflowAction::ApproveRetry {
            journal,
            target,
            observed_hash,
        } => WorkflowResult::RestoreJournal(store.approve_restore_retry(
            &journal,
            target,
            observed_hash,
        )?),
        WorkflowAction::CreateSetlist(name) => WorkflowResult::Setlist(store.create_setlist(name)?),
        WorkflowAction::ListSetlists { include_trash } => {
            WorkflowResult::Setlists(store.setlists(include_trash)?)
        }
        WorkflowAction::SaveSetlist {
            setlist,
            expected_revision,
        } => WorkflowResult::Setlist(store.save_setlist(setlist, expected_revision)?),
        WorkflowAction::TrashSetlist {
            id,
            expected_revision,
            trashed,
        } => WorkflowResult::Setlist(store.setlist_trash(&id, expected_revision, trashed)?),
        WorkflowAction::ResolveSetlist(id) => {
            WorkflowResult::ResolvedSetlist(store.resolve_setlist(&id)?)
        }
        WorkflowAction::CompareVersions { before, after } => {
            let before = store.load_version(&before)?;
            let after = store.load_version(&after)?;
            let catalog = gp200_catalog::Catalog::embedded().map_err(crate::store::invalid)?;
            WorkflowResult::Differences(compare_presets(
                &before.snapshot,
                &after.snapshot,
                &catalog,
            ))
        }
        WorkflowAction::AssetReplacementImpact(slot) => {
            WorkflowResult::AssetReplacementImpact(store.asset_replacement_impact(slot)?)
        }
        WorkflowAction::ListIrHistory(request) => {
            WorkflowResult::IrHistoryPage(ir_history_page(store, request, cancelled)?)
        }
        WorkflowAction::GetIrHistory(id) => {
            WorkflowResult::IrHistoryDetail(ir_history_detail(store, &id, cancelled)?)
        }
        WorkflowAction::ExportIrHistory(request) => {
            WorkflowResult::IrHistoryExport(export_ir_history(store, request, cancelled)?)
        }
        WorkflowAction::PrepareMovement(request) => {
            WorkflowResult::MovementPreparation(Box::new(store.prepare_preset_movement(&request)?))
        }
        WorkflowAction::BindMovement { prepared, reviewed } => {
            WorkflowResult::MovementExecutionReview(Box::new(
                store.bind_preset_movement_execution(&prepared, reviewed)?,
            ))
        }
        WorkflowAction::BeginMovement {
            execution,
            reviewed,
            fresh,
        } => WorkflowResult::MovementJournal(Box::new(
            store.begin_preset_movement(&execution, reviewed, *fresh)?,
        )),
        WorkflowAction::ListMovements => {
            WorkflowResult::MovementJournals(store.movement_journals()?)
        }
        WorkflowAction::GetMovement(id) => {
            WorkflowResult::MovementJournal(Box::new(store.movement_journal(&id)?))
        }
        WorkflowAction::DispatchMovement {
            journal,
            sequence,
            fresh,
        } => WorkflowResult::MovementDispatch(Box::new(
            store.begin_preset_movement_entry(&journal, sequence, *fresh)?,
        )),
        WorkflowAction::ConfirmMovement {
            journal,
            sequence,
            actor_verified_target_readback,
            post,
        } => WorkflowResult::MovementJournal(Box::new(store.confirm_preset_movement_entry(
            &journal,
            sequence,
            actor_verified_target_readback,
            *post,
        )?)),
        WorkflowAction::PauseMovement {
            journal,
            sequence,
            reason,
        } => WorkflowResult::MovementJournal(Box::new(
            store.pause_preset_movement(&journal, sequence, &reason)?,
        )),
        WorkflowAction::ReconcileMovement {
            journal,
            sequence,
            fresh,
        } => WorkflowResult::MovementReconciliation(Box::new(
            store.reconcile_preset_movement(&journal, sequence, *fresh)?,
        )),
        WorkflowAction::ContinueMovement {
            journal,
            reviewed_report,
            explicitly_reviewed_retry_targets,
            fresh,
        } => WorkflowResult::MovementJournal(Box::new(store.continue_preset_movement(
            &journal,
            reviewed_report,
            explicitly_reviewed_retry_targets,
            *fresh,
        )?)),
    };
    // A movement acknowledgement cannot escape after its durable action was cancelled.
    // The GUI still independently owns its run serial and late-result handling.
    if movement_mutation && cancelled() {
        let movement = match &result {
            WorkflowResult::MovementJournal(journal) if !journal.is_completed() => {
                Some((journal.id.clone(), journal.sequence))
            }
            WorkflowResult::MovementDispatch(dispatch) => {
                Some((dispatch.journal().to_owned(), dispatch.sequence()))
            }
            WorkflowResult::MovementReconciliation(report) => {
                Some((report.journal.clone(), report.sequence))
            }
            _ => None,
        };
        if let Some((id, sequence)) = movement {
            if !store.movement_journal(&id)?.is_completed() {
                store.pause_preset_movement(&id, sequence,
                    "Movement worker action was cancelled after publication; reconcile before continuing")?;
            }
            return Err(ServiceError::new(
                ErrorKind::Cancelled,
                "Movement action cancelled; no dispatch acknowledgement returned",
            ));
        }
    }
    Ok(result)
}
