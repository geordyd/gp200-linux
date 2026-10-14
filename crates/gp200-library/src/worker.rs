use crate::{LibraryStore, WorkflowCommand, WorkflowEvent, data_dir};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use gp200_domain::*;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

pub const COMMAND_CAPACITY: usize = 64;
pub const EVENT_CAPACITY: usize = 256;

/// Bounded single-writer worker. Opening is a bootstrap operation; commands,
/// archive parsing, publication and hashing all execute off the egui thread.
pub struct LibraryWorker {
    /// Description of the original received store, never a second owner.
    root: PathBuf,
    commands: Sender<WorkerCommand>,
    events: Receiver<LibraryEvent>,
    workflow_events: Receiver<WorkflowEvent>,
    shutdown: ShutdownToken,
    cancelled: Arc<Mutex<BTreeSet<JobId>>>,
    recovery_note: Option<String>,
    firmware_backup_client: Option<crate::FirmwareBackupClient>,
    firmware_runtime_client: Option<crate::LibraryFirmwareRuntimeClient>,
    admission: Arc<crate::firmware_runtime_owner::Admission>,
}
enum WorkerCommand {
    Library(LibraryCommand),
    Workflow(WorkflowCommand),
}
/// Cloneable workflow endpoint for a concrete worker that is also boxed as `LibraryService`.
/// Poll from exactly one application consumer; clones share the same response queue.
#[derive(Clone)]
pub struct LibraryWorkflowClient {
    commands: Sender<WorkerCommand>,
    events: Receiver<WorkflowEvent>,
    shutdown: ShutdownToken,
    admission: Arc<crate::firmware_runtime_owner::Admission>,
}
impl LibraryWorkflowClient {
    pub fn try_submit(&self, command: WorkflowCommand) -> Result<(), SubmitError<WorkflowCommand>> {
        // A known stop is permanent even while another operation holds the gate.
        if self.shutdown.cancelled() || self.admission.is_revoked() {
            return Err(SubmitError::Disconnected(command));
        }
        let Some(mut gate) = self.admission.try_gate() else {
            return Err(SubmitError::Busy(command));
        };
        if self.shutdown.cancelled() || self.admission.is_revoked() || gate.permanently_revoked() {
            return Err(SubmitError::Disconnected(command));
        }
        if !gate.offers_allowed() {
            return Err(SubmitError::Busy(command));
        }
        match self.commands.try_send(WorkerCommand::Workflow(command)) {
            Ok(()) => {
                gate.queued += 1;
                Ok(())
            }
            Err(TrySendError::Full(WorkerCommand::Workflow(command))) => {
                Err(SubmitError::Busy(command))
            }
            Err(TrySendError::Disconnected(WorkerCommand::Workflow(command))) => {
                Err(SubmitError::Disconnected(command))
            }
            Err(_) => unreachable!("try_send returns the submitted variant"),
        }
    }
    pub fn try_next_event(&self) -> Option<WorkflowEvent> {
        let mut gate = self.admission.try_gate()?;
        if self.admission.is_revoked() || gate.permanently_revoked() {
            return None;
        }
        let event = self.events.try_recv().ok()?;
        gate.workflow_unread = gate.workflow_unread.saturating_sub(1);
        Some(event)
    }
}
impl LibraryWorker {
    pub fn open(path: impl AsRef<Path>, wake: WakeUi) -> Result<Self, ServiceError> {
        Self::from_store(LibraryStore::open(path)?, wake).map_err(|e| e.error)
    }
    /// Starts from this exact owner without opening a directory or resetting health.
    pub fn from_store(
        store: LibraryStore,
        wake: WakeUi,
    ) -> Result<Self, crate::ReturnedLibraryStoreError> {
        Self::from_store_inner(store, wake, StartupHooks::default())
    }
    fn from_store_inner(
        store: LibraryStore,
        wake: WakeUi,
        hooks: StartupHooks,
    ) -> Result<Self, crate::ReturnedLibraryStoreError> {
        let root = store.root().to_path_buf();
        let recovery_note = store.recovery_note.clone();
        let admission = match crate::firmware_runtime_owner::Admission::new() {
            Ok(v) => v,
            Err(error) => return Err(crate::ReturnedLibraryStoreError { error, store }),
        };
        let (commands, rx) = bounded(COMMAND_CAPACITY);
        let (tx, events) = bounded(EVENT_CAPACITY);
        let (workflow_tx, workflow_events) = bounded(EVENT_CAPACITY);
        let shutdown = ShutdownToken::default();
        let cancelled = Arc::new(Mutex::new(BTreeSet::new()));
        let (firmware_backup_client, firmware_backup_route) =
            match crate::firmware_backup_worker::channel_with_admission(Some(admission.clone())) {
                Ok(v) => v,
                Err(error) => return Err(crate::ReturnedLibraryStoreError { error, store }),
            };
        let (firmware_runtime_client, firmware_runtime_route) =
            crate::firmware_runtime_owner::channel(admission.clone());
        let cell = Arc::new(Mutex::new(Some(store)));
        let child = cell.clone();
        let thread_shutdown = shutdown.clone();
        let thread_cancelled = cancelled.clone();
        let thread_admission = admission.clone();
        let forced = {
            #[cfg(test)]
            {
                hooks.fail_start
            }
            #[cfg(not(test))]
            {
                let _ = hooks;
                false
            }
        };
        let closure = move || {
            let store = child
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
                .expect("one actual writer takes the startup owner");
            run(
                store,
                rx,
                tx,
                workflow_tx,
                thread_shutdown,
                thread_cancelled,
                wake,
                firmware_backup_route,
                firmware_runtime_route,
                thread_admission,
            )
        };
        let spawned = if forced {
            drop(closure);
            Err(std::io::Error::other(
                "Injected actual writer spawn failure",
            ))
        } else {
            thread::Builder::new()
                .name("gp200-library".into())
                .spawn(closure)
        };
        let join = match spawned {
            Ok(v) => v,
            Err(error) => {
                let store = cell
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .take()
                    .expect("failed spawner retains original owner");
                return Err(crate::ReturnedLibraryStoreError {
                    error: ServiceError::new(
                        ErrorKind::Io,
                        format!("Cannot start library writer: {error}"),
                    ),
                    store,
                });
            }
        };
        admission.install_join(join);
        Ok(Self {
            root,
            commands,
            events,
            workflow_events,
            shutdown,
            cancelled,
            recovery_note,
            firmware_backup_client: Some(firmware_backup_client),
            firmware_runtime_client: Some(firmware_runtime_client),
            admission,
        })
    }
    #[cfg(test)]
    pub(crate) fn test_admission(&self) -> Arc<crate::firmware_runtime_owner::Admission> {
        self.admission.clone()
    }
    /// Borrow the descriptive path captured from this exact received store.
    /// This neither clones nor reopens the original store or its writer.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Extract once before boxing. The endpoint owns no separate store.
    pub fn take_firmware_runtime_client(&mut self) -> Option<crate::LibraryFirmwareRuntimeClient> {
        let client = self.firmware_runtime_client.take()?;
        self.admission.runtime_extracted();
        Some(client)
    }
    /// Extract once before boxing the worker. The route shares this store and writer.
    pub fn take_firmware_backup_client(&mut self) -> Option<crate::FirmwareBackupClient> {
        self.firmware_backup_client.take()
    }
    pub fn open_xdg(wake: WakeUi) -> Result<Self, ServiceError> {
        Self::open(data_dir()?, wake)
    }
    /// Bootstrap warning when the last valid recovery manifest was used.
    pub fn recovery_note(&self) -> Option<&str> {
        self.recovery_note.as_deref()
    }
    /// Workflows share the same bounded queue, writer and serialization as ordinary library operations.
    pub fn try_submit_workflow(
        &self,
        command: WorkflowCommand,
    ) -> Result<(), SubmitError<WorkflowCommand>> {
        self.workflow_client().try_submit(command)
    }
    pub fn try_next_workflow_event(&self) -> Option<WorkflowEvent> {
        self.workflow_client().try_next_event()
    }
    pub fn workflow_client(&self) -> LibraryWorkflowClient {
        LibraryWorkflowClient {
            commands: self.commands.clone(),
            events: self.workflow_events.clone(),
            shutdown: self.shutdown.clone(),
            admission: self.admission.clone(),
        }
    }
}
impl LibraryService for LibraryWorker {
    fn try_submit(&self, command: LibraryCommand) -> Result<(), SubmitError<LibraryCommand>> {
        // A known stop is permanent even while another operation holds the gate.
        if self.shutdown.cancelled() || self.admission.is_revoked() {
            return Err(SubmitError::Disconnected(command));
        }
        let Some(mut gate) = self.admission.try_gate() else {
            return Err(SubmitError::Busy(command));
        };
        if self.shutdown.cancelled() || self.admission.is_revoked() || gate.permanently_revoked() {
            return Err(SubmitError::Disconnected(command));
        }
        if !gate.offers_allowed() {
            return Err(SubmitError::Busy(command));
        }
        if let LibraryCommand::Cancel(job) = command {
            if let Ok(mut cancelled) = self.cancelled.try_lock()
                && cancelled.len() < 256
            {
                cancelled.insert(job);
                gate.cancelled = cancelled.len();
                return Ok(());
            }
            return Err(SubmitError::Busy(LibraryCommand::Cancel(job)));
        }
        match self.commands.try_send(WorkerCommand::Library(command)) {
            Ok(()) => {
                gate.queued += 1;
                Ok(())
            }
            Err(TrySendError::Full(WorkerCommand::Library(command))) => {
                Err(SubmitError::Busy(command))
            }
            Err(TrySendError::Disconnected(WorkerCommand::Library(command))) => {
                Err(SubmitError::Disconnected(command))
            }
            Err(_) => unreachable!("try_send returns the submitted variant"),
        }
    }
    fn try_next_event(&self) -> Option<LibraryEvent> {
        let mut gate = self.admission.try_gate()?;
        if self.admission.is_revoked() || gate.permanently_revoked() {
            return None;
        }
        let event = self.events.try_recv().ok()?;
        gate.library_unread = gate.library_unread.saturating_sub(1);
        Some(event)
    }
    fn shutdown(&self) {
        self.admission.stop("Ordinary writer shutdown requested");
        self.shutdown.cancel();
    }
}
impl Drop for LibraryWorker {
    fn drop(&mut self) {
        self.admission.stop("Ordinary writer shutdown requested");
        self.shutdown.cancel();
    }
}

#[derive(Default)]
struct StartupHooks {
    #[cfg(test)]
    fail_start: bool,
}
fn publish<T>(
    tx: &Sender<T>,
    mut event: T,
    shutdown: &ShutdownToken,
    wake: &WakeUi,
    admission: &crate::firmware_runtime_owner::Admission,
    workflow: bool,
) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_millis(250);
    loop {
        if shutdown.cancelled() {
            return false;
        }
        if let Some(mut gate) = admission.try_gate() {
            if admission.is_revoked() || gate.permanently_revoked() {
                return false;
            }
            match tx.try_send(event) {
                Ok(()) => {
                    if workflow {
                        gate.workflow_unread += 1;
                    } else {
                        gate.library_unread += 1;
                    }
                    drop(gate);
                    wake();
                    return true;
                }
                Err(TrySendError::Full(v)) => event = v,
                Err(TrySendError::Disconnected(_)) => {
                    drop(gate);
                    admission.stop("Ordinary result consumer lost");
                    return false;
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            admission.stop("Ordinary result delivery exceeded bounded deadline");
            return false;
        }
        thread::sleep(Duration::from_millis(2));
    }
}
fn finish_operation(
    admission: &crate::firmware_runtime_owner::Admission,
    cancelled: &Mutex<BTreeSet<JobId>>,
) {
    let count = cancelled.lock().map_or(usize::MAX, |v| v.len());
    let mut gate = admission.gate.lock().unwrap_or_else(|p| p.into_inner());
    gate.active = false;
    gate.cancelled = count;
}
#[allow(clippy::too_many_arguments)] // Existing worker channels plus separate non-clone archival route.
fn run(
    mut store: LibraryStore,
    rx: Receiver<WorkerCommand>,
    tx: Sender<LibraryEvent>,
    workflow_tx: Sender<WorkflowEvent>,
    shutdown: ShutdownToken,
    cancelled: Arc<Mutex<BTreeSet<JobId>>>,
    wake: WakeUi,
    mut firmware_backup_route: crate::firmware_backup_worker::FirmwareBackupWorkerRoute,
    mut firmware_runtime_route: crate::firmware_runtime_owner::RuntimeRoute,
    admission: Arc<crate::firmware_runtime_owner::Admission>,
) -> LibraryStore {
    while !shutdown.cancelled() {
        if firmware_runtime_route.tick(&mut store, &mut firmware_backup_route, &shutdown, &wake) {
            #[cfg(test)]
            if admission
                .panic_before_return
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                panic!("Injected actual writer panic after helper owns join");
            }
            #[cfg(test)]
            if let Some((entered, release)) = admission.pause_before_return.lock().unwrap().take() {
                entered.try_send(()).unwrap();
                release.recv().unwrap();
            }
            return store;
        }
        firmware_backup_route.tick(&mut store, &shutdown, &wake);
        let command = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(command) => command,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };
        {
            let mut gate = admission.gate.lock().unwrap_or_else(|p| p.into_inner());
            gate.queued = gate.queued.saturating_sub(1);
            gate.active = true;
        }
        if matches!(command, WorkerCommand::Library(LibraryCommand::Shutdown)) {
            break;
        }
        let command = match command {
            WorkerCommand::Library(command) => command,
            WorkerCommand::Workflow(command) => {
                let job_id = JobId(command.intent.0);
                let is_cancelled = || {
                    shutdown.cancelled()
                        || cancelled.lock().map_or(true, |jobs| jobs.contains(&job_id))
                };
                let event = WorkflowEvent {
                    intent: command.intent,
                    result: if (store.movement_publication_uncertain
                        || store.firmware_publication_uncertain)
                        && !workflow_observation(&command.action)
                    {
                        Err(ServiceError::new(
                            ErrorKind::Indeterminate,
                            "Genuine store publication is uncertain; mutation is unavailable",
                        ))
                    } else {
                        crate::workflow_worker::execute_workflow(
                            &mut store,
                            command.action,
                            &is_cancelled,
                        )
                    },
                };
                if let Ok(mut jobs) = cancelled.lock() {
                    jobs.remove(&job_id);
                }
                if !publish(&workflow_tx, event, &shutdown, &wake, &admission, true) {
                    break;
                }
                finish_operation(&admission, &cancelled);
                continue;
            }
        };
        let intent = command_intent(&command);
        let job_id = JobId(intent.map_or(0, |id| id.0));
        let is_cancelled =
            || shutdown.cancelled() || cancelled.lock().map_or(true, |jobs| jobs.contains(&job_id));
        let is_archive = matches!(
            command,
            LibraryCommand::ImportBundle { .. }
                | LibraryCommand::ExportBundle { .. }
                | LibraryCommand::ImportNative { .. }
                | LibraryCommand::ExportNative { .. }
                | LibraryCommand::Backup { .. }
        );
        let is_native = matches!(
            command,
            LibraryCommand::ImportNative { .. } | LibraryCommand::ExportNative { .. }
        );
        if is_archive
            && !publish(
                &tx,
                LibraryEvent::JobProgress {
                    job_id,
                    progress: JobProgress {
                        completed: 0,
                        total: Some(1),
                        message: if is_native {
                            "Checking and processing native preset file"
                        } else {
                            "Checking and processing local preset bundle"
                        }
                        .into(),
                        cancellable: true,
                    },
                },
                &shutdown,
                &wake,
                &admission,
                false,
            )
        {
            break;
        }
        let result = execute(&mut store, command, &is_cancelled);
        let succeeded = result.is_ok();
        if let Ok(mut jobs) = cancelled.lock() {
            jobs.remove(&job_id);
        }
        let event = match result {
            Ok(Some(event)) => event,
            Ok(None) => {
                finish_operation(&admission, &cancelled);
                continue;
            }
            Err(error) => LibraryEvent::Failed {
                intent: intent.unwrap_or_default(),
                error,
            },
        };
        if !publish(&tx, event, &shutdown, &wake, &admission, false) {
            break;
        }
        if is_archive
            && !publish(
                &tx,
                LibraryEvent::JobProgress {
                    job_id,
                    progress: JobProgress {
                        completed: u64::from(succeeded),
                        total: Some(1),
                        message: if succeeded {
                            "Local file operation finished"
                        } else {
                            "Local file operation failed or was cancelled"
                        }
                        .into(),
                        cancellable: false,
                    },
                },
                &shutdown,
                &wake,
                &admission,
                false,
            )
        {
            break;
        }
        finish_operation(&admission, &cancelled);
    }
    admission.stop("Actual ordinary writer stopped");
    firmware_backup_route.stop(&mut store);
    store
}
fn command_intent(command: &LibraryCommand) -> Option<IntentId> {
    match command {
        LibraryCommand::Query { intent, .. }
        | LibraryCommand::LoadVersion { intent, .. }
        | LibraryCommand::StoreSnapshot { intent, .. }
        | LibraryCommand::ListVersions { intent, .. }
        | LibraryCommand::ImportBundle { intent, .. }
        | LibraryCommand::ExportBundle { intent, .. }
        | LibraryCommand::ImportNative { intent, .. }
        | LibraryCommand::ExportNative { intent, .. }
        | LibraryCommand::Backup { intent, .. }
        | LibraryCommand::DeleteToTrash { intent, .. }
        | LibraryCommand::RestoreTrash { intent, .. }
        | LibraryCommand::UpdateMetadata { intent, .. } => Some(*intent),
        LibraryCommand::Cancel(_) | LibraryCommand::Shutdown => None,
    }
}
fn workflow_observation(action: &crate::WorkflowAction) -> bool {
    matches!(
        action,
        crate::WorkflowAction::ListBackups
            | crate::WorkflowAction::GetBackup(_)
            | crate::WorkflowAction::ListRestores
            | crate::WorkflowAction::GetRestore(_)
            | crate::WorkflowAction::ListSetlists { .. }
            | crate::WorkflowAction::ResolveSetlist(_)
            | crate::WorkflowAction::CompareVersions { .. }
            | crate::WorkflowAction::AssetReplacementImpact(_)
            | crate::WorkflowAction::ListMovements
            | crate::WorkflowAction::GetMovement(_)
            | crate::WorkflowAction::PlanRestore(_)
            | crate::WorkflowAction::ExportBackup { .. }
    )
}
fn execute(
    store: &mut LibraryStore,
    command: LibraryCommand,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<LibraryEvent>, ServiceError> {
    if (store.movement_publication_uncertain || store.firmware_publication_uncertain)
        && !matches!(
            &command,
            LibraryCommand::Query { .. }
                | LibraryCommand::LoadVersion { .. }
                | LibraryCommand::ListVersions { .. }
                | LibraryCommand::ExportBundle { .. }
                | LibraryCommand::ExportNative { .. }
                | LibraryCommand::Backup { .. }
                | LibraryCommand::Cancel(_)
                | LibraryCommand::Shutdown
        )
    {
        return Err(ServiceError::new(
            ErrorKind::Indeterminate,
            "Genuine store publication is uncertain; mutation is unavailable",
        ));
    }
    let event = match command {
        LibraryCommand::Query { intent, query } => LibraryEvent::QueryResult {
            intent,
            entries: store.query(&query),
        },
        LibraryCommand::LoadVersion { intent, version } => LibraryEvent::VersionLoaded {
            intent,
            version: store.load_version(&version)?,
        },
        LibraryCommand::StoreSnapshot {
            intent,
            snapshot,
            metadata,
            parent,
        } => LibraryEvent::VersionStored {
            intent,
            version: store.store_snapshot(snapshot, metadata, parent)?,
        },
        LibraryCommand::ListVersions { intent, library } => LibraryEvent::VersionsListed {
            intent,
            versions: store.list_versions(&library)?,
        },
        LibraryCommand::ImportBundle { intent, request } => {
            let (versions, report) = store.import_bundle_cancellable(&request, cancelled)?;
            LibraryEvent::Imported {
                intent,
                versions,
                report,
            }
        }
        LibraryCommand::ExportBundle { intent, request } => {
            let report = store.export_bundle_cancellable(&request, false, cancelled)?;
            LibraryEvent::Exported {
                intent,
                path: request.path,
                report,
            }
        }
        LibraryCommand::ImportNative { intent, request } => {
            let (version, report) = store.import_native_cancellable(&request, cancelled)?;
            LibraryEvent::Imported {
                intent,
                versions: vec![version],
                report,
            }
        }
        LibraryCommand::ExportNative { intent, request } => {
            let report = store.export_native_cancellable(&request, cancelled)?;
            LibraryEvent::Exported {
                intent,
                path: request.path,
                report,
            }
        }
        LibraryCommand::Backup { intent, request } => {
            let report = store.export_bundle_cancellable(
                &ExportRequest {
                    path: request.path.clone(),
                    versions: request.versions,
                    include_assets: true,
                },
                true,
                cancelled,
            )?;
            LibraryEvent::Exported {
                intent,
                path: request.path,
                report,
            }
        }
        LibraryCommand::DeleteToTrash { intent, library } => {
            store.delete_to_trash(&library)?;
            LibraryEvent::EntryChanged { intent, library }
        }
        LibraryCommand::RestoreTrash { intent, library } => {
            store.restore_trash(&library)?;
            LibraryEvent::EntryChanged { intent, library }
        }
        LibraryCommand::UpdateMetadata {
            intent,
            library,
            metadata,
        } => {
            store.update_metadata(&library, metadata)?;
            LibraryEvent::EntryChanged { intent, library }
        }
        LibraryCommand::Cancel(_) | LibraryCommand::Shutdown => return Ok(None),
    };
    Ok(Some(event))
}

#[cfg(test)]
mod firmware_owner_worker_tests {
    use super::*;
    use crate::LibraryFirmwareAction;
    #[test]
    fn already_stopped_commands_retain_originals_without_waiting_for_the_admission_gate() {
        for stop_kind in [
            "actual worker shutdown",
            "shutdown token only",
            "admission revocation only",
        ] {
            let path = std::env::temp_dir().join(format!(
                "fw05a0-stopped-admission-{}",
                crate::store::nonce().unwrap()
            ));
            let worker = LibraryWorker::open(&path, Arc::new(|| {})).unwrap();
            let workflow = worker.workflow_client();
            let shared = worker.test_admission();
            let held = shared.gate.lock().unwrap();
            let before = (
                held.queued,
                held.active,
                held.library_unread,
                held.workflow_unread,
                held.cancelled,
            );
            match stop_kind {
                "actual worker shutdown" => worker.shutdown(),
                "shutdown token only" => worker.shutdown.cancel(),
                "admission revocation only" => shared.stop("Held-gate regression revocation"),
                _ => unreachable!(),
            }
            let text = format!("Original ordinary request: {stop_kind}");
            let original_text = text.as_ptr();
            let ordinary = worker.try_submit(LibraryCommand::Query {
                intent: IntentId(701),
                query: LibraryQuery {
                    text,
                    tags: vec!["original tag".into()],
                    favorites_only: true,
                    trashed_only: true,
                },
            });
            let name = format!("Original workflow request: {stop_kind}");
            let original_name = name.as_ptr();
            let workflow_result = workflow.try_submit(WorkflowCommand {
                intent: IntentId(702),
                action: crate::WorkflowAction::CreateSetlist(name),
            });
            // Observe both results while this exact gate is still held. A known
            // stopped writer cannot offer work or ask the caller to retry Busy.
            let ordinary_disconnected = matches!(&ordinary, Err(SubmitError::Disconnected(_)));
            let workflow_disconnected =
                matches!(&workflow_result, Err(SubmitError::Disconnected(_)));
            let after = (
                held.queued,
                held.active,
                held.library_unread,
                held.workflow_unread,
                held.cancelled,
            );
            let queue_empty = worker.commands.is_empty();
            drop(held);
            worker.shutdown();
            drop(workflow);
            drop(worker);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !shared.join_finished_for_test() {
                assert!(std::time::Instant::now() < deadline);
                thread::sleep(Duration::from_millis(2));
            }
            drop(shared);
            fs_remove(&path);
            assert_eq!(before, after, "{stop_kind}");
            assert!(queue_empty, "{stop_kind}: no command may be offered");
            match ordinary {
                Err(SubmitError::Disconnected(LibraryCommand::Query { intent, query }))
                | Err(SubmitError::Busy(LibraryCommand::Query { intent, query })) => {
                    assert_eq!(intent, IntentId(701));
                    assert_eq!(query.text.as_ptr(), original_text);
                    assert_eq!(
                        query.text,
                        format!("Original ordinary request: {stop_kind}")
                    );
                    assert_eq!(query.tags, ["original tag"]);
                    assert!(query.favorites_only && query.trashed_only);
                }
                other => panic!("Unexpected original ordinary result: {other:?}"),
            }
            match workflow_result {
                Err(SubmitError::Disconnected(WorkflowCommand {
                    intent,
                    action: crate::WorkflowAction::CreateSetlist(name),
                }))
                | Err(SubmitError::Busy(WorkflowCommand {
                    intent,
                    action: crate::WorkflowAction::CreateSetlist(name),
                })) => {
                    assert_eq!(intent, IntentId(702));
                    assert_eq!(name.as_ptr(), original_name);
                    assert_eq!(name, format!("Original workflow request: {stop_kind}"));
                }
                other => panic!("Unexpected original workflow result: {other:?}"),
            }
            assert!(
                ordinary_disconnected,
                "{stop_kind}: known stop returned Busy"
            );
            assert!(
                workflow_disconnected,
                "{stop_kind}: known stop returned Busy"
            );
        }
    }
    #[test]
    fn actual_startup_failure_returns_exact_health_and_owner_after_failed_closure_is_dropped() {
        let path =
            std::env::temp_dir().join(format!("fw05a0-spawn-{}", crate::store::nonce().unwrap()));
        let mut store = LibraryStore::open(&path).unwrap();
        store.recovery_note = Some("Unpublished exact owner".into());
        store.firmware_publication_uncertain = true;
        let failed = LibraryWorker::from_store_inner(
            store,
            Arc::new(|| {}),
            StartupHooks { fail_start: true },
        )
        .err()
        .unwrap();
        assert!(LibraryStore::open(&path).is_err());
        let returned = failed.into_store();
        assert_eq!(
            returned.recovery_note.as_deref(),
            Some("Unpublished exact owner")
        );
        assert!(returned.firmware_publication_uncertain);
        drop(returned);
        fs_remove(&path);
    }
    fn fs_remove(path: &std::path::Path) {
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn completion_accounting_waits_for_real_contention_instead_of_losing_active_false() {
        let shared = crate::firmware_runtime_owner::Admission::new().unwrap();
        shared.gate.lock().unwrap().active = true;
        let cancelled = Arc::new(Mutex::new(BTreeSet::new()));
        let locked = shared.gate.lock().unwrap();
        let child = shared.clone();
        let c = cancelled.clone();
        let (tx, rx) = bounded(1);
        let t = thread::spawn(move || {
            finish_operation(&child, &c);
            tx.send(()).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
        drop(locked);
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        t.join().unwrap();
        assert!(!shared.gate.lock().unwrap().active);
    }
    #[test]
    fn poisoned_genuine_store_is_inspectable_but_blocks_actual_retirement_and_mutation() {
        let path =
            std::env::temp_dir().join(format!("fw05a0-health-{}", crate::store::nonce().unwrap()));
        let mut store = LibraryStore::open(&path).unwrap();
        store.movement_publication_uncertain = true;
        let mut worker = LibraryWorker::from_store(store, Arc::new(|| {})).unwrap();
        let mut c = worker.take_firmware_runtime_client().unwrap();
        c.try_request(LibraryFirmwareAction::RelinquishStore)
            .unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(5);
        let e = loop {
            if let Some(e) = c.try_next_event() {
                break e;
            }
            assert!(std::time::Instant::now() < until);
            thread::sleep(Duration::from_millis(2));
        };
        assert!(matches!(
            e.result,
            Ok(crate::LibraryFirmwareResult::Blocked { .. })
        ));
        assert_eq!(
            c.retirement_state(),
            crate::LibraryStoreRetirementState::Running
        );
        worker
            .try_submit_workflow(WorkflowCommand {
                intent: IntentId(1),
                action: crate::WorkflowAction::CreateSetlist("blocked".into()),
            })
            .unwrap();
        let e = loop {
            if let Some(e) = worker.try_next_workflow_event() {
                break e;
            }
            assert!(std::time::Instant::now() < until);
            thread::sleep(Duration::from_millis(2));
        };
        assert!(e.result.is_err());
        worker
            .try_submit(LibraryCommand::Query {
                intent: IntentId(2),
                query: LibraryQuery::default(),
            })
            .unwrap();
        loop {
            if worker.try_next_event().is_some() {
                break;
            }
            assert!(std::time::Instant::now() < until);
            thread::sleep(Duration::from_millis(2));
        }
        let shared = worker.admission.clone();
        drop(c);
        drop(worker);
        let until = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if shared.join_finished_for_test() {
                break;
            }
            assert!(std::time::Instant::now() < until);
            thread::sleep(Duration::from_millis(2));
        }
        drop(shared);
        fs_remove(&path);
    }
}
