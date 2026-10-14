//! Genuine single-writer ownership transfer and observation-only firmware history.
use crate::{
    FirmwareHistoryCursor, FirmwareHistoryDetail, FirmwareHistoryExportReceipt,
    FirmwareHistoryPage, FirmwareHistoryPrefix, LibraryStore,
};
use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded};
use gp200_domain::{ErrorKind, ServiceError, ShutdownToken, SubmitError, WakeUi};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex, MutexGuard},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub const LIBRARY_FIRMWARE_COMMAND_CAPACITY: usize = 8;
pub const LIBRARY_FIRMWARE_EVENT_CAPACITY: usize = 8;
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryStoreRetirementState {
    Running,
    CheckingRetirement,
    BlockedPendingAcknowledgement,
    Retiring,
    Returned,
    Stopped,
    Unresolved,
    Unavailable,
}
#[derive(Debug)]
pub enum LibraryFirmwareAction {
    RelinquishStore,
    ListHistory {
        cursor: Option<FirmwareHistoryCursor>,
        limit: usize,
    },
    InspectAttempt {
        attempt: gp200_domain::FirmwareAttemptId,
    },
    ExportAttempt {
        attempt: gp200_domain::FirmwareAttemptId,
        expected_prefix: FirmwareHistoryPrefix,
        destination: PathBuf,
    },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryFirmwareActionAddress {
    RelinquishStore,
    ListHistory {
        cursor: Option<FirmwareHistoryCursor>,
        limit: usize,
    },
    InspectAttempt {
        attempt: gp200_domain::FirmwareAttemptId,
    },
    ExportAttempt {
        attempt: gp200_domain::FirmwareAttemptId,
        expected_prefix: FirmwareHistoryPrefix,
        destination: PathBuf,
    },
}
impl LibraryFirmwareAction {
    fn address(&self) -> LibraryFirmwareActionAddress {
        match self {
            Self::RelinquishStore => LibraryFirmwareActionAddress::RelinquishStore,
            Self::ListHistory { cursor, limit } => LibraryFirmwareActionAddress::ListHistory {
                cursor: cursor.clone(),
                limit: *limit,
            },
            Self::InspectAttempt { attempt } => LibraryFirmwareActionAddress::InspectAttempt {
                attempt: attempt.clone(),
            },
            Self::ExportAttempt {
                attempt,
                expected_prefix,
                destination,
            } => LibraryFirmwareActionAddress::ExportAttempt {
                attempt: attempt.clone(),
                expected_prefix: expected_prefix.clone(),
                destination: destination.clone(),
            },
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryFirmwareRequestAddress {
    pub run: String,
    pub sequence: u64,
    pub action: LibraryFirmwareActionAddress,
}
#[derive(Debug)]
pub enum LibraryFirmwareResult {
    Blocked { reasons: Vec<String> },
    Retirement(LibraryStoreRetirement),
    HistoryPage(FirmwareHistoryPage),
    HistoryDetail(Box<FirmwareHistoryDetail>),
    Exported(FirmwareHistoryExportReceipt),
}
#[derive(Debug)]
pub struct LibraryFirmwareEvent {
    pub address: LibraryFirmwareRequestAddress,
    pub result: Result<LibraryFirmwareResult, ServiceError>,
}

pub(crate) struct AdmissionState {
    pub(crate) phase: LibraryStoreRetirementState,
    pub(crate) queued: usize,
    pub(crate) active: bool,
    pub(crate) library_unread: usize,
    pub(crate) workflow_unread: usize,
    pub(crate) cancelled: usize,
    pending: Option<LibraryFirmwareRequestAddress>,
    runtime_stopped: bool,
    terminal: Option<String>,
    retirement_issued: bool,
}
impl AdmissionState {
    pub(crate) fn offers_allowed(&self) -> bool {
        self.phase == LibraryStoreRetirementState::Running
    }
    pub(crate) fn permanently_revoked(&self) -> bool {
        matches!(
            self.phase,
            LibraryStoreRetirementState::Retiring
                | LibraryStoreRetirementState::Returned
                | LibraryStoreRetirementState::Stopped
                | LibraryStoreRetirementState::Unresolved
        )
    }
}
struct Ownership {
    result: Option<Result<LibraryStore, ServiceError>>,
    taken: bool,
}
pub(crate) struct Admission {
    pub(crate) gate: Mutex<AdmissionState>,
    join: Mutex<Option<JoinHandle<LibraryStore>>>,
    ownership: Mutex<Ownership>,
    // Deliberate one-generation retention. Only the genuine ownership consumer breaks this anchor.
    // A lost/full event rail must not drop the sole store or release its writer lock.
    anchor: Mutex<Option<Arc<Admission>>>,
    run: String,
    pub(crate) revoked: AtomicBool,
    runtime_cancelled: AtomicBool,
    runtime_extracted: AtomicBool,
    writer_lost: AtomicBool,
    retirement_started: AtomicBool,
    #[cfg(test)]
    pub(crate) fail_helper: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    pub(crate) panic_before_return: AtomicBool,
    #[cfg(test)]
    fill_result_rail: AtomicBool,
    #[cfg(test)]
    pub(crate) pause_before_return: Mutex<Option<(Sender<()>, Receiver<()>)>>,
}
impl Admission {
    pub(crate) fn new() -> Result<Arc<Self>, ServiceError> {
        Ok(Arc::new(Self {
            gate: Mutex::new(AdmissionState {
                phase: LibraryStoreRetirementState::Running,
                queued: 0,
                active: false,
                library_unread: 0,
                workflow_unread: 0,
                cancelled: 0,
                pending: None,
                runtime_stopped: false,
                terminal: None,
                retirement_issued: false,
            }),
            join: Mutex::new(None),
            ownership: Mutex::new(Ownership {
                result: None,
                taken: false,
            }),
            anchor: Mutex::new(None),
            run: crate::store::nonce()?,
            revoked: AtomicBool::new(false),
            runtime_cancelled: AtomicBool::new(false),
            runtime_extracted: AtomicBool::new(false),
            writer_lost: AtomicBool::new(false),
            retirement_started: AtomicBool::new(false),
            #[cfg(test)]
            fail_helper: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            panic_before_return: AtomicBool::new(false),
            #[cfg(test)]
            fill_result_rail: AtomicBool::new(false),
            #[cfg(test)]
            pause_before_return: Mutex::new(None),
        }))
    }
    pub(crate) fn try_gate(&self) -> Option<MutexGuard<'_, AdmissionState>> {
        self.gate.try_lock().ok()
    }
    pub(crate) fn install_join(self: &Arc<Self>, join: JoinHandle<LibraryStore>) {
        *self.join.lock().unwrap_or_else(|p| p.into_inner()) = Some(join);
        // The actual writer may exit before spawn returns its JoinHandle to the caller.
        if self.writer_lost.load(Ordering::SeqCst) && self.runtime_extracted.load(Ordering::SeqCst)
        {
            *self.anchor.lock().unwrap_or_else(|p| p.into_inner()) = Some(self.clone());
        }
    }
    pub(crate) fn stop(&self, note: &str) {
        self.revoked.store(true, Ordering::SeqCst);
        if let Some(mut g) = self.try_gate() {
            if self.writer_lost.load(Ordering::SeqCst) {
                return;
            }
            g.phase = LibraryStoreRetirementState::Stopped;
            g.terminal = Some(note.chars().take(4096).collect());
        }
    }
    pub(crate) fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::SeqCst)
    }
    fn runtime_stopped(&self) -> bool {
        self.runtime_cancelled.load(Ordering::SeqCst)
    }
    pub(crate) fn runtime_extracted(self: &Arc<Self>) {
        self.runtime_extracted.store(true, Ordering::SeqCst);
        self.observe_writer_loss();
        if self.writer_lost.load(Ordering::SeqCst) {
            self.mark_writer_lost();
        }
    }
    fn mark_writer_lost(self: &Arc<Self>) {
        self.writer_lost.store(true, Ordering::SeqCst);
        self.revoked.store(true, Ordering::SeqCst);
        if let Some(mut g) = self.try_gate() {
            g.phase = LibraryStoreRetirementState::Unresolved;
            // Preserve prior failure/cancellation diagnostics and all actual work counters.
            if g.terminal.is_none() {
                g.terminal = Some(WRITER_LOSS_NOTE.into());
            }
        }
        if self.runtime_extracted.load(Ordering::SeqCst)
            && let Ok(join) = self.join.try_lock()
            && join.is_some()
            && let Ok(mut anchor) = self.anchor.try_lock()
        {
            *anchor = Some(self.clone());
        }
    }
    fn observe_writer_loss(self: &Arc<Self>) -> bool {
        if !self.retirement_started.load(Ordering::SeqCst)
            && self
                .join
                .try_lock()
                .ok()
                .is_some_and(|j| j.as_ref().is_some_and(JoinHandle::is_finished))
        {
            self.mark_writer_lost();
        }
        self.writer_lost.load(Ordering::SeqCst)
    }
}
const WRITER_LOSS_NOTE: &str =
    "Actual library writer exited before retirement; original join retained, no safe handoff";
/// Non-clone control endpoint extracted once from the actual writer.
/// ```compile_fail
/// fn duplicate(c:gp200_library::LibraryFirmwareRuntimeClient){let _=c.clone();}
/// ```
/// ```compile_fail
/// let _:gp200_library::LibraryFirmwareRuntimeClient=serde_json::from_str("{}").unwrap();
/// ```
pub struct LibraryFirmwareRuntimeClient {
    commands: Sender<RuntimeCommand>,
    events: Receiver<LibraryFirmwareEvent>,
    shared: Arc<Admission>,
    sequence: u64,
}
pub(crate) struct RuntimeRoute {
    commands: Receiver<RuntimeCommand>,
    events: Sender<LibraryFirmwareEvent>,
    shared: Arc<Admission>,
}
struct RuntimeCommand {
    address: LibraryFirmwareRequestAddress,
    action: LibraryFirmwareAction,
}
pub(crate) fn channel(shared: Arc<Admission>) -> (LibraryFirmwareRuntimeClient, RuntimeRoute) {
    let (commands, rx) = bounded(LIBRARY_FIRMWARE_COMMAND_CAPACITY);
    let (tx, events) = bounded(LIBRARY_FIRMWARE_EVENT_CAPACITY);
    (
        LibraryFirmwareRuntimeClient {
            commands,
            events,
            shared: shared.clone(),
            sequence: 0,
        },
        RuntimeRoute {
            commands: rx,
            events: tx,
            shared,
        },
    )
}
impl LibraryFirmwareRuntimeClient {
    pub fn run_id(&self) -> &str {
        &self.shared.run
    }
    pub fn try_request(
        &mut self,
        action: LibraryFirmwareAction,
    ) -> Result<LibraryFirmwareRequestAddress, SubmitError<LibraryFirmwareAction>> {
        if self.shared.observe_writer_loss() {
            return Err(SubmitError::Disconnected(action));
        }
        let Some(mut g) = self.shared.try_gate() else {
            return Err(SubmitError::Busy(action));
        };
        if self.shared.runtime_stopped()
            || self.shared.is_revoked()
            || g.runtime_stopped
            || g.permanently_revoked()
        {
            return Err(SubmitError::Disconnected(action));
        }
        if !g.offers_allowed() || g.pending.is_some() || !self.events.is_empty() {
            return Err(SubmitError::Busy(action));
        }
        if matches!(action, LibraryFirmwareAction::RelinquishStore)
            && (g.queued != 0
                || g.active
                || g.library_unread != 0
                || g.workflow_unread != 0
                || g.cancelled != 0)
        {
            return Err(SubmitError::Busy(action));
        }
        let Some(sequence) = self.sequence.checked_add(1) else {
            g.runtime_stopped = true;
            return Err(SubmitError::Disconnected(action));
        };
        let address = LibraryFirmwareRequestAddress {
            run: self.shared.run.clone(),
            sequence,
            action: action.address(),
        };
        let retiring = matches!(action, LibraryFirmwareAction::RelinquishStore);
        match self.commands.try_send(RuntimeCommand {
            address: address.clone(),
            action,
        }) {
            Ok(()) => {
                self.sequence = sequence;
                g.pending = Some(address.clone());
                if retiring {
                    g.phase = LibraryStoreRetirementState::CheckingRetirement;
                }
                Ok(address)
            }
            Err(TrySendError::Full(c)) => Err(SubmitError::Busy(c.action)),
            Err(TrySendError::Disconnected(c)) => {
                g.runtime_stopped = true;
                drop(g);
                self.shared.mark_writer_lost();
                Err(SubmitError::Disconnected(c.action))
            }
        }
    }
    pub fn try_next_event(&mut self) -> Option<LibraryFirmwareEvent> {
        self.shared.observe_writer_loss();
        let mut g = self.shared.try_gate()?;
        let mut event = match self.events.try_recv() {
            Ok(event) => event,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                drop(g);
                if !self.shared.retirement_started.load(Ordering::SeqCst) {
                    self.shared.mark_writer_lost();
                }
                return None;
            }
        };
        if g.pending.as_ref() != Some(&event.address) {
            g.runtime_stopped = true;
            g.terminal = Some("Runtime reply correlation differs".into());
            drop(g);
            drop(event);
            return None;
        }
        g.pending = None;
        if self.shared.runtime_stopped() || self.shared.is_revoked() || g.runtime_stopped {
            event.result = Err(ServiceError::new(
                ErrorKind::Indeterminate,
                "Runtime success revoked after consumer stop",
            ));
        }
        if matches!(event.result, Ok(LibraryFirmwareResult::Blocked { .. }))
            && !g.runtime_stopped
            && g.phase == LibraryStoreRetirementState::BlockedPendingAcknowledgement
        {
            g.phase = LibraryStoreRetirementState::Running;
        }
        if matches!(event.result, Ok(LibraryFirmwareResult::Retirement(_))) {
            g.retirement_issued = true;
        }
        Some(event)
    }
    pub fn terminal_failure(&self) -> Option<String> {
        let lost = self.shared.observe_writer_loss();
        let note = self.shared.try_gate().and_then(|g| g.terminal.clone());
        if lost {
            Some(match note {
                Some(note) if note != WRITER_LOSS_NOTE => format!("{WRITER_LOSS_NOTE}; {note}"),
                _ => WRITER_LOSS_NOTE.into(),
            })
        } else {
            note
        }
    }
    pub fn retirement_state(&self) -> LibraryStoreRetirementState {
        if self.shared.observe_writer_loss() {
            return LibraryStoreRetirementState::Unresolved;
        }
        if self.shared.runtime_stopped() {
            return LibraryStoreRetirementState::Stopped;
        }
        self.shared
            .try_gate()
            .map_or(LibraryStoreRetirementState::Unavailable, |g| {
                g.phase.clone()
            })
    }
    pub fn cancel(&self) {
        self.shared.runtime_cancelled.store(true, Ordering::SeqCst);
        if let Some(mut g) = self.shared.try_gate() {
            if self.shared.writer_lost.load(Ordering::SeqCst) {
                return;
            }
            g.runtime_stopped = true;
            if g.phase != LibraryStoreRetirementState::Running {
                g.phase = LibraryStoreRetirementState::Stopped;
            }
            g.terminal = Some("Runtime consumer stopped; ownership remains retained".into());
        }
    }
}
impl Drop for LibraryFirmwareRuntimeClient {
    fn drop(&mut self) {
        self.cancel();
    }
}
/// One genuine joined-store result, with immediate nonblocking polling.
/// ```compile_fail
/// fn duplicate(c:gp200_library::LibraryStoreRetirement){let _=c.clone();}
/// ```
/// ```compile_fail
/// let _:gp200_library::LibraryStoreRetirement=serde_json::from_str("{}").unwrap();
/// ```
pub struct LibraryStoreRetirement {
    shared: Arc<Admission>,
}
impl std::fmt::Debug for LibraryStoreRetirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibraryStoreRetirement")
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}
impl LibraryStoreRetirement {
    pub fn state(&self) -> LibraryStoreRetirementState {
        self.shared
            .try_gate()
            .map_or(LibraryStoreRetirementState::Unavailable, |g| {
                g.phase.clone()
            })
    }
    pub fn try_take_result(&mut self) -> Option<Result<LibraryStore, ServiceError>> {
        let mut slot = self.shared.ownership.try_lock().ok()?;
        if slot.taken {
            return None;
        }
        let result = slot.result.take()?;
        slot.taken = true;
        drop(slot);
        // Break the retained ownership anchor only after moving the genuine joined result.
        if let Ok(mut anchor) = self.shared.anchor.try_lock() {
            anchor.take();
        }
        Some(result)
    }
}
impl RuntimeRoute {
    fn history_cancelled(&self, shutdown: &ShutdownToken) -> bool {
        shutdown.cancelled()
            || self.shared.runtime_stopped()
            || self.shared.is_revoked()
            || self.shared.try_gate().is_some_and(|g| g.runtime_stopped)
    }
    pub(crate) fn tick(
        &mut self,
        store: &mut LibraryStore,
        backup: &mut crate::firmware_backup_worker::FirmwareBackupWorkerRoute,
        shutdown: &ShutdownToken,
        wake: &WakeUi,
    ) -> bool {
        let command = match self.commands.try_recv() {
            Ok(v) => v,
            Err(_) => return false,
        };
        let valid = {
            let g = self.shared.gate.lock().unwrap_or_else(|p| p.into_inner());
            !self.shared.runtime_stopped()
                && !g.runtime_stopped
                && g.pending.as_ref() == Some(&command.address)
        };
        if !valid {
            self.shared
                .stop("Runtime request revoked or correlation differs");
            return false;
        }
        let retiring = matches!(command.action, LibraryFirmwareAction::RelinquishStore);
        let result = match command.action {
            LibraryFirmwareAction::RelinquishStore => {
                let mut reasons = backup
                    .retirement_blockers()
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                if store.movement_publication_uncertain || store.firmware_publication_uncertain {
                    reasons.push("Genuine store publication health is uncertain".into());
                }
                if let Some(g) = self.shared.try_gate() {
                    if g.queued != 0
                        || g.active
                        || g.library_unread != 0
                        || g.workflow_unread != 0
                        || g.cancelled != 0
                    {
                        reasons.push("Actual ordinary work or acknowledgement remains".into());
                    }
                } else {
                    reasons.push("Admission gate unavailable".into());
                }
                if reasons.is_empty() {
                    match start_retirement(&self.shared, wake.clone()) {
                        Ok(handle) => {
                            backup.revoke_for_retirement();
                            Ok(LibraryFirmwareResult::Retirement(handle))
                        }
                        Err(error) => Ok(LibraryFirmwareResult::Blocked {
                            reasons: vec![error.message],
                        }),
                    }
                } else {
                    Ok(LibraryFirmwareResult::Blocked { reasons })
                }
            }
            LibraryFirmwareAction::ListHistory { cursor, limit } => {
                crate::firmware_attempt_history::list_history(store, cursor, limit)
                    .map(LibraryFirmwareResult::HistoryPage)
            }
            LibraryFirmwareAction::InspectAttempt { attempt } => {
                crate::firmware_attempt_history::inspect_attempt(store, &attempt)
                    .map(|v| LibraryFirmwareResult::HistoryDetail(Box::new(v)))
            }
            LibraryFirmwareAction::ExportAttempt {
                attempt,
                expected_prefix,
                destination,
            } => crate::firmware_attempt_history::export_attempt(
                store,
                &attempt,
                &expected_prefix,
                &destination,
                &|| self.history_cancelled(shutdown),
            )
            .map(LibraryFirmwareResult::Exported),
        };
        let accepted = matches!(result, Ok(LibraryFirmwareResult::Retirement(_)));
        if retiring && !accepted {
            let mut g = self.shared.gate.lock().unwrap_or_else(|p| p.into_inner());
            if !self.shared.runtime_stopped() && !self.shared.is_revoked() && !g.runtime_stopped {
                g.phase = LibraryStoreRetirementState::BlockedPendingAcknowledgement;
            }
        }
        #[cfg(test)]
        if self.shared.fill_result_rail.swap(false, Ordering::SeqCst) {
            for _ in 0..LIBRARY_FIRMWARE_EVENT_CAPACITY {
                self.events
                    .try_send(LibraryFirmwareEvent {
                        address: command.address.clone(),
                        result: Err(ServiceError::new(
                            ErrorKind::Indeterminate,
                            "Injected result-rail obstruction",
                        )),
                    })
                    .unwrap();
            }
        }
        self.publish(
            LibraryFirmwareEvent {
                address: command.address,
                result,
            },
            wake,
        );
        accepted
    }
    fn publish(&self, mut event: LibraryFirmwareEvent, wake: &WakeUi) {
        let deadline = Instant::now() + Duration::from_millis(250);
        loop {
            if let Some(mut g) = self.shared.try_gate() {
                if self.shared.runtime_stopped() || g.runtime_stopped {
                    g.terminal = Some("Runtime reply revoked; ownership retained".into());
                    return;
                }
                match self.events.try_send(event) {
                    Ok(()) => {
                        drop(g);
                        wake();
                        return;
                    }
                    Err(TrySendError::Full(v)) => event = v,
                    Err(TrySendError::Disconnected(_)) => {
                        g.runtime_stopped = true;
                        g.terminal =
                            Some("Runtime result consumer lost; ownership retained".into());
                        return;
                    }
                }
            }
            if Instant::now() >= deadline {
                self.shared
                    .stop("Runtime result delivery exceeded bounded deadline");
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for RuntimeRoute {
    fn drop(&mut self) {
        if !self.shared.retirement_started.load(Ordering::SeqCst) {
            // Actual command receiver/event sender destruction is a terminal loss, even if
            // stale queued/active/unread counts or a contended UI gate remain.
            self.shared.mark_writer_lost();
            // This is the exiting writer, never a UI poll. Ensure the genuine join/result
            // remains owned after all consumers disappear; install_join covers spawn races.
            if self.shared.runtime_extracted.load(Ordering::SeqCst)
                && self
                    .shared
                    .join
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .is_some()
            {
                *self.shared.anchor.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some(self.shared.clone());
            }
        }
    }
}
fn start_retirement(
    shared: &Arc<Admission>,
    wake: WakeUi,
) -> Result<LibraryStoreRetirement, ServiceError> {
    let join = shared
        .join
        .try_lock()
        .map_err(|_| ServiceError::new(ErrorKind::Busy, "Exact worker join unavailable"))?
        .take()
        .ok_or_else(|| {
            ServiceError::new(ErrorKind::Indeterminate, "Exact worker join already moved")
        })?;
    let cell = Arc::new(Mutex::new(Some(join)));
    let child = cell.clone();
    let target = shared.clone();
    let forced = {
        #[cfg(test)]
        {
            shared
                .fail_helper
                .swap(false, std::sync::atomic::Ordering::SeqCst)
        }
        #[cfg(not(test))]
        {
            false
        }
    };
    let closure = move || {
        let join = child
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .expect("one helper takes the actual join");
        let result = join.join().map_err(|_| {
            ServiceError::new(
                ErrorKind::Indeterminate,
                "Actual library writer panicked; no returned store",
            )
        });
        let failed = result.is_err();
        target
            .ownership
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .result = Some(result);
        {
            let mut g = target.gate.lock().unwrap_or_else(|p| p.into_inner());
            if !target.runtime_stopped() && !target.is_revoked() && !g.runtime_stopped {
                g.phase = if failed {
                    LibraryStoreRetirementState::Unresolved
                } else {
                    LibraryStoreRetirementState::Returned
                };
            }
            if failed {
                g.terminal = Some("Actual library writer panicked; ownership unresolved".into());
            }
        }
        wake();
    };
    let spawned = if forced {
        drop(closure);
        Err(std::io::Error::other(
            "Injected actual retirement helper spawn failure",
        ))
    } else {
        thread::Builder::new()
            .name("gp200-library-retirement".into())
            .spawn(closure)
    };
    if let Err(error) = spawned {
        let join = cell
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .expect("failed spawner never takes join");
        shared.install_join(join);
        return Err(ServiceError::new(
            ErrorKind::Io,
            format!("Cannot start retirement helper; original writer retained: {error}"),
        ));
    }
    shared.retirement_started.store(true, Ordering::SeqCst);
    *shared.anchor.lock().unwrap_or_else(|p| p.into_inner()) = Some(shared.clone());
    {
        let mut g = shared.gate.lock().unwrap_or_else(|p| p.into_inner());
        if !g.runtime_stopped {
            g.phase = LibraryStoreRetirementState::Retiring;
        }
    }
    Ok(LibraryStoreRetirement {
        shared: shared.clone(),
    })
}

/// Startup failure that retains the exact original store and its lock/health.
pub struct ReturnedLibraryStoreError {
    pub(crate) error: ServiceError,
    pub(crate) store: LibraryStore,
}
impl std::fmt::Debug for ReturnedLibraryStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReturnedLibraryStoreError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl ReturnedLibraryStoreError {
    pub fn error(&self) -> &ServiceError {
        &self.error
    }
    pub fn into_store(self) -> LibraryStore {
        self.store
    }
}
#[cfg(test)]
impl Admission {
    pub(crate) fn join_finished_for_test(&self) -> bool {
        self.join
            .lock()
            .unwrap()
            .as_ref()
            .is_none_or(JoinHandle::is_finished)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::LibraryWorker;
    use gp200_domain::LibraryService;
    use std::fs;
    fn temp() -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("fw05a0-runtime-{}", crate::store::nonce().unwrap()));
        fs::create_dir(&p).unwrap();
        p
    }
    fn event(c: &mut LibraryFirmwareRuntimeClient) -> LibraryFirmwareEvent {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(e) = c.try_next_event() {
                return e;
            }
            assert!(Instant::now() < end, "{:?}", c.terminal_failure());
            thread::sleep(Duration::from_millis(2));
        }
    }
    fn ownership_ready(s: &Admission) {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            if s.ownership.lock().unwrap().result.is_some() {
                return;
            }
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(2));
        }
    }
    #[test]
    fn cancel_and_drop_are_sticky_when_the_actual_admission_gate_is_contended() {
        for dropping in [false, true] {
            let shared = Admission::new().unwrap();
            let (c, _route) = channel(shared.clone());
            let mut c = Some(c);
            let held = shared.gate.lock().unwrap();
            let start = Instant::now();
            if dropping {
                drop(c.take());
            } else {
                c.as_ref().unwrap().cancel();
            }
            assert!(start.elapsed() < Duration::from_millis(100));
            assert!(shared.runtime_stopped());
            drop(held);
            if !dropping {
                assert!(matches!(
                    c.as_mut()
                        .unwrap()
                        .try_request(LibraryFirmwareAction::RelinquishStore),
                    Err(SubmitError::Disconnected(_))
                ));
            }
        }
    }
    #[test]
    fn actual_history_cancel_probe_waits_through_contention_but_observes_sticky_cancel() {
        let shared = Admission::new().unwrap();
        let (client, route) = channel(shared.clone());
        let shutdown = ShutdownToken::default();
        let held = shared.gate.lock().unwrap();
        assert!(!route.history_cancelled(&shutdown));
        client.cancel();
        assert!(route.history_cancelled(&shutdown));
        drop(held);
    }
    #[test]
    fn helper_spawn_failure_keeps_exact_writer_join_store_and_blocked_ack_barrier() {
        let path = temp();
        let mut store = LibraryStore::open(&path).unwrap();
        store.recovery_note = Some("Exact store survives helper failure".into());
        let mut worker = LibraryWorker::from_store(store, Arc::new(|| {})).unwrap();
        let shared = worker.test_admission();
        shared.fail_helper.store(true, Ordering::SeqCst);
        let mut c = worker.take_firmware_runtime_client().unwrap();
        c.try_request(LibraryFirmwareAction::RelinquishStore)
            .unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while c.retirement_state() != LibraryStoreRetirementState::BlockedPendingAcknowledgement {
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(2));
        }
        assert!(LibraryStore::open(&path).is_err());
        assert!(matches!(
            worker.try_submit(gp200_domain::LibraryCommand::Query {
                intent: gp200_domain::IntentId(1),
                query: gp200_domain::LibraryQuery::default()
            }),
            Err(SubmitError::Busy(_))
        ));
        assert!(matches!(
            event(&mut c).result,
            Ok(LibraryFirmwareResult::Blocked { .. })
        ));
        assert_eq!(c.retirement_state(), LibraryStoreRetirementState::Running);
        c.try_request(LibraryFirmwareAction::RelinquishStore)
            .unwrap();
        let mut handle = match event(&mut c).result.unwrap() {
            LibraryFirmwareResult::Retirement(h) => h,
            _ => panic!(),
        };
        ownership_ready(&shared);
        let store = handle.try_take_result().unwrap().unwrap();
        assert_eq!(
            store.recovery_note.as_deref(),
            Some("Exact store survives helper failure")
        );
        drop(store);
        drop(worker);
        drop(c);
        drop(shared);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn actual_writer_panic_returns_no_store_and_is_unresolved_after_genuine_join() {
        let path = temp();
        let mut worker = LibraryWorker::open(&path, Arc::new(|| {})).unwrap();
        let shared = worker.test_admission();
        shared.panic_before_return.store(true, Ordering::SeqCst);
        let mut c = worker.take_firmware_runtime_client().unwrap();
        c.try_request(LibraryFirmwareAction::RelinquishStore)
            .unwrap();
        let mut h = match event(&mut c).result.unwrap() {
            LibraryFirmwareResult::Retirement(h) => h,
            _ => panic!(),
        };
        ownership_ready(&shared);
        assert_eq!(h.state(), LibraryStoreRetirementState::Unresolved);
        assert!(h.try_take_result().unwrap().is_err());
        assert!(h.try_take_result().is_none());
        drop(worker);
        drop(c);
        drop(shared);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn ordinary_wake_panic_before_retirement_is_terminal_even_with_stale_work_and_gate_contention()
    {
        let path = temp();
        let (entered_tx, entered_rx) = bounded(1);
        let (release_tx, release_rx) = bounded(1);
        let mut worker = LibraryWorker::open(
            &path,
            Arc::new(move || {
                entered_tx.try_send(()).unwrap();
                release_rx.recv().unwrap();
                panic!("Actual ordinary result wake panicked before retirement");
            }),
        )
        .unwrap();
        let shared = worker.test_admission();
        let mut c = worker.take_firmware_runtime_client().unwrap();
        for intent in [1, 2] {
            worker
                .try_submit(gp200_domain::LibraryCommand::Query {
                    intent: gp200_domain::IntentId(intent),
                    query: gp200_domain::LibraryQuery::default(),
                })
                .unwrap();
            if intent == 1 {
                entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        }
        let held = shared.gate.lock().unwrap();
        assert!(held.active);
        assert_eq!(held.queued, 1);
        assert_eq!(held.library_unread, 1);
        release_tx.try_send(()).unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while !shared.join_finished_for_test() {
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(2));
        }
        let start = Instant::now();
        for _ in 0..100 {
            assert!(matches!(
                c.try_request(LibraryFirmwareAction::RelinquishStore),
                Err(SubmitError::Disconnected(_))
            ));
            assert_eq!(
                c.retirement_state(),
                LibraryStoreRetirementState::Unresolved
            );
            assert!(c.terminal_failure().unwrap().contains("no safe handoff"));
            assert!(c.try_next_event().is_none());
        }
        assert!(start.elapsed() < Duration::from_millis(100));
        assert!(held.active);
        assert_eq!(held.queued, 1);
        assert_eq!(held.library_unread, 1);
        drop(held);
        assert!(shared.ownership.lock().unwrap().result.is_none());
        drop(worker);
        drop(c);
        assert!(shared.anchor.lock().unwrap().is_some());
        // Private test cleanup joins the exact retained handle on another thread. It is
        // neither a retirement receipt nor public recovery authority.
        let join = shared.join.lock().unwrap().take().unwrap();
        assert!(thread::spawn(move || join.join()).join().unwrap().is_err());
        shared.anchor.lock().unwrap().take();
        drop(shared);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn stopped_writer_before_retirement_retains_its_actual_returned_store_without_safe_handoff() {
        let path = temp();
        let mut store = LibraryStore::open(&path).unwrap();
        store.recovery_note = Some("Exact pre-retirement stopped owner".into());
        store.firmware_publication_uncertain = true;
        let mut worker = LibraryWorker::from_store(store, Arc::new(|| {})).unwrap();
        let shared = worker.test_admission();
        let mut c = worker.take_firmware_runtime_client().unwrap();
        worker
            .try_submit(gp200_domain::LibraryCommand::Shutdown)
            .unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while !shared.join_finished_for_test() {
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(2));
        }
        assert!(shared.gate.lock().unwrap().active);
        assert!(matches!(
            c.try_request(LibraryFirmwareAction::RelinquishStore),
            Err(SubmitError::Disconnected(_))
        ));
        assert!(c.try_next_event().is_none());
        assert_eq!(
            c.retirement_state(),
            LibraryStoreRetirementState::Unresolved
        );
        assert!(c.terminal_failure().unwrap().contains("no safe handoff"));
        assert!(shared.ownership.lock().unwrap().result.is_none());
        assert!(LibraryStore::open(&path).is_err());
        drop(worker);
        drop(c);
        assert!(shared.anchor.lock().unwrap().is_some());
        assert!(LibraryStore::open(&path).is_err());
        let join = shared.join.lock().unwrap().take().unwrap();
        let store = thread::spawn(move || join.join()).join().unwrap().unwrap();
        assert_eq!(
            store.recovery_note.as_deref(),
            Some("Exact pre-retirement stopped owner")
        );
        assert!(store.firmware_publication_uncertain);
        assert!(shared.gate.lock().unwrap().active);
        shared.anchor.lock().unwrap().take();
        drop(store);
        drop(shared);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn full_and_lost_runtime_results_keep_the_actual_joined_store_in_its_retained_slot() {
        for full in [false, true] {
            let path = temp();
            let mut worker = LibraryWorker::open(&path, Arc::new(|| {})).unwrap();
            let shared = worker.test_admission();
            let mut c = worker.take_firmware_runtime_client().unwrap();
            if full {
                shared.fill_result_rail.store(true, Ordering::SeqCst);
            }
            c.try_request(LibraryFirmwareAction::RelinquishStore)
                .unwrap();
            let end = Instant::now() + Duration::from_secs(5);
            while !matches!(
                c.retirement_state(),
                LibraryStoreRetirementState::Retiring
                    | LibraryStoreRetirementState::Returned
                    | LibraryStoreRetirementState::Stopped
            ) {
                assert!(Instant::now() < end);
                thread::sleep(Duration::from_millis(2));
            }
            if !full {
                drop(c);
            } else {
                thread::sleep(Duration::from_millis(300));
                assert!(event(&mut c).result.is_err());
                drop(c);
            }
            ownership_ready(&shared);
            drop(worker);
            assert!(LibraryStore::open(&path).is_err());
            assert!(shared.anchor.lock().unwrap().is_some());
            let actual = shared
                .ownership
                .lock()
                .unwrap()
                .result
                .take()
                .unwrap()
                .unwrap();
            drop(actual);
            shared.anchor.lock().unwrap().take();
            drop(shared);
            assert!(LibraryStore::open(&path).is_ok());
            fs::remove_dir_all(path).unwrap();
        }
    }
    #[test]
    fn foreign_private_reply_cannot_replace_the_actual_pending_address() {
        let shared = Admission::new().unwrap();
        let (mut c, route) = channel(shared.clone());
        let address = c
            .try_request(LibraryFirmwareAction::ListHistory {
                cursor: None,
                limit: 1,
            })
            .unwrap();
        let mut foreign = address;
        foreign.sequence += 1;
        route
            .events
            .try_send(LibraryFirmwareEvent {
                address: foreign,
                result: Ok(LibraryFirmwareResult::Blocked { reasons: vec![] }),
            })
            .unwrap();
        assert!(c.try_next_event().is_none());
        assert!(c.terminal_failure().unwrap().contains("correlation"));
        assert!(matches!(
            c.try_request(LibraryFirmwareAction::RelinquishStore),
            Err(SubmitError::Disconnected(_))
        ));
    }
}

#[cfg(test)]
mod stalled_join_test {
    use super::*;
    use crate::LibraryWorker;
    #[test]
    fn actual_writer_and_join_stall_retains_store_while_ownership_polls_are_immediate() {
        let path =
            std::env::temp_dir().join(format!("fw05a0-stall-{}", crate::store::nonce().unwrap()));
        let mut store = LibraryStore::open(&path).unwrap();
        store.recovery_note = Some("Exact stalled owner".into());
        let mut worker = LibraryWorker::from_store(store, Arc::new(|| {})).unwrap();
        let shared = worker.test_admission();
        let (entered_tx, entered_rx) = bounded(1);
        let (release_tx, release_rx) = bounded(1);
        *shared.pause_before_return.lock().unwrap() = Some((entered_tx, release_rx));
        let mut client = worker.take_firmware_runtime_client().unwrap();
        client
            .try_request(LibraryFirmwareAction::RelinquishStore)
            .unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        let event = loop {
            if let Some(e) = client.try_next_event() {
                break e;
            }
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(2));
        };
        let mut handle = match event.result.unwrap() {
            LibraryFirmwareResult::Retirement(h) => h,
            _ => panic!(),
        };
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let now = Instant::now();
        for _ in 0..100 {
            assert!(handle.try_take_result().is_none());
            let _ = handle.state();
        }
        assert!(now.elapsed() < Duration::from_millis(100));
        assert!(LibraryStore::open(&path).is_err());
        release_tx.try_send(()).unwrap();
        let store = loop {
            if let Some(r) = handle.try_take_result() {
                break r.unwrap();
            }
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(store.recovery_note.as_deref(), Some("Exact stalled owner"));
        drop(store);
        drop(worker);
        drop(client);
        drop(shared);
        std::fs::remove_dir_all(path).unwrap();
    }
}
