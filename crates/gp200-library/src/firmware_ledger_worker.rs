//! Dedicated bounded local journal owner. Existing GUI handoff is not wired yet.
use crate::*;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use gp200_domain::*;
use std::{
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
const CAPACITY: usize = 8;
const RAW_CAPACITY: usize = 64;
/// All stop transitions, successful enqueue and delivery share this brief gate.
/// No filesystem operation or blocking channel operation holds it.
#[derive(Debug, Default)]
struct StopGate {
    stopped: AtomicBool,
    requested: AtomicBool,
    delivery: Mutex<()>,
}
impl StopGate {
    fn lock(&self) -> MutexGuard<'_, ()> {
        self.delivery.lock().unwrap_or_else(|poison| {
            self.stopped.store(true, Ordering::SeqCst);
            poison.into_inner()
        })
    }
    fn stop(&self) {
        let _gate = self.lock();
        self.stopped.store(true, Ordering::SeqCst);
    }
    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst) || self.requested.load(Ordering::SeqCst)
    }
}
fn revoked_reply() -> ServiceError {
    ServiceError::new(
        ErrorKind::Indeterminate,
        "Firmware local acknowledgement revoked by stopped owner",
    )
}
#[cfg(test)]
struct ReplyBoundaryHook {
    intent: u64,
    after_enqueue: bool,
    reached: Sender<()>,
    release: Receiver<()>,
}
#[cfg(test)]
fn reply_boundary(hook: &Option<ReplyBoundaryHook>, intent: u64, after_enqueue: bool) {
    if let Some(hook) = hook
        && hook.intent == intent
        && hook.after_enqueue == after_enqueue
    {
        hook.reached.send(()).expect("test barrier receiver");
        hook.release
            .recv_timeout(Duration::from_secs(5))
            .expect("test barrier release");
    }
}
#[derive(Debug, Clone)]
pub enum FirmwareLedgerAction {
    Begin {
        job: Arc<ReviewedFirmwareUpdate>,
        origin: FirmwareObservationOrigin,
    },
    Checkpoint {
        ordinal: u64,
    },
    Send(FirmwareSendObservation),
    MockDecision(FirmwareBlockDescription),
    ModeBoundary {
        raw: Vec<u8>,
        note: String,
    },
    Close(FirmwareHostClosure),
    LifecyclePrepared(FirmwareLifecyclePlanDto),
    LifecycleEntryPrepared(FirmwareEntryObservationPlanDto),
    LifecycleEntryHostSettled(FirmwareEntryHostSettlement),
    LifecycleEpoch(Box<FirmwareLifecycleEpochObservation>),
    LifecycleCheckpoint(FirmwareLifecycleActionDescription),
    LifecycleRaw(FirmwareLifecycleRawObservation),
    LifecycleSend(FirmwareLifecycleSendObservation),
    LifecycleDecision(FirmwareLifecycleDecision),
    LifecycleClosed(FirmwareLifecycleHostClosure),
    LifecycleFailure {
        raw: Vec<FirmwareLifecycleRawObservation>,
        send: Option<FirmwareLifecycleSendObservation>,
        note: String,
    },
    Inspect(FirmwareAttemptId),
}
#[derive(Debug, Clone)]
pub struct FirmwareLedgerRequest {
    pub intent: u64,
    pub expected_sequence: u64,
    pub review_sha256: String,
    pub action: FirmwareLedgerAction,
}
#[derive(Debug)]
pub enum FirmwareLedgerReply {
    Prepared {
        attempt: FirmwareAttemptId,
        checkpoint: FirmwareLocalCheckpoint,
    },
    Checkpoint(FirmwareLocalCheckpoint),
    ActionCheckpoint(FirmwareLocalActionCheckpoint),
    Recorded {
        sequence: u64,
    },
    Observation(Box<FirmwareRetainedAttempt>),
}
#[derive(Debug)]
pub struct FirmwareLedgerEvent {
    pub run: String,
    pub intent: u64,
    pub result: Result<FirmwareLedgerReply, ServiceError>,
}
/// A durable local receipt bound to the concrete issuing worker, not a flash permit.
/// ```compile_fail
/// fn clone(c: gp200_library::FirmwareLocalActionCheckpoint) { let _ = c.clone(); }
/// ```
#[derive(Debug)]
pub struct FirmwareLocalActionCheckpoint {
    checkpoint: FirmwareLocalCheckpoint,
    action: Box<FirmwareLifecycleActionDescription>,
    run: String,
    intent: u64,
    issuer: Arc<StopGate>,
}
impl FirmwareLocalActionCheckpoint {
    pub fn action(&self) -> &FirmwareLifecycleActionDescription {
        &self.action
    }
    pub fn sequence(&self) -> u64 {
        self.checkpoint.sequence()
    }
    pub fn attempt(&self) -> &FirmwareAttemptId {
        self.checkpoint.attempt()
    }
    pub fn prefix_sha256(&self) -> &str {
        self.checkpoint.prefix_sha256()
    }
    pub fn run_id(&self) -> &str {
        &self.run
    }
    pub fn intent(&self) -> u64 {
        self.intent
    }
    pub fn permits_flash(&self) -> bool {
        false
    }
}
/// Brief revocation fence. It orders one nonblocking offer against local stop.
/// It does not certify physical origin, USB delivery or flash completion.
/// ```compile_fail
/// fn clone(c: gp200_library::FirmwareLocalOfferFence<'_>) { let _ = c.clone(); }
/// ```
pub struct FirmwareLocalOfferFence<'a> {
    _gate: MutexGuard<'a, ()>,
}
/// Owns a moved LibraryStore; it does not open/reopen a directory or a device.
/// No cloneable GUI/workflow handle can manufacture its private ledger cursor.
pub struct FirmwareLedgerWorker {
    run: String,
    commands: Sender<FirmwareLedgerRequest>,
    raw: Sender<FirmwareCallbackDto>,
    events: Receiver<FirmwareLedgerEvent>,
    stopped: Arc<StopGate>,
    pending: Arc<AtomicU64>,
    offer: Arc<AtomicU64>,
    join: Option<JoinHandle<LibraryStore>>,
}
/// Owns the original store when no actual ledger thread accepted it.
/// It is neither cloneable nor reconstructed from a path or DTO.
/// ```compile_fail
/// fn clone(f: gp200_library::FirmwareLedgerStartFailure) { let _ = f.clone(); }
/// ```
/// ```compile_fail
/// let _: gp200_library::FirmwareLedgerStartFailure = serde_json::from_str("{}").unwrap();
/// ```
pub struct FirmwareLedgerStartFailure {
    error: ServiceError,
    store: Box<LibraryStore>,
}
impl FirmwareLedgerStartFailure {
    pub fn into_parts(self) -> (ServiceError, LibraryStore) {
        (self.error, *self.store)
    }
}
impl std::fmt::Debug for FirmwareLedgerStartFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FirmwareLedgerStartFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl FirmwareLedgerWorker {
    pub fn spawn(store: LibraryStore) -> Result<Self, ServiceError> {
        Self::spawn_inner(
            store,
            #[cfg(test)]
            None,
        )
    }
    /// Returns the exact moved store if preflight or OS thread creation fails.
    /// No store is reopened or reconstructed from its root path.
    pub fn spawn_retaining_store(store: LibraryStore) -> Result<Self, FirmwareLedgerStartFailure> {
        Self::spawn_retaining_store_with_builder(
            store,
            thread::Builder::new().name("gp200-firmware-ledger".into()),
        )
    }
    /// Owning start with caller-selected OS thread attributes. This grants no
    /// journal cursor or firmware dispatch authority.
    pub fn spawn_retaining_store_with_builder(
        store: LibraryStore,
        builder: thread::Builder,
    ) -> Result<Self, FirmwareLedgerStartFailure> {
        Self::spawn_owned_inner(
            store,
            builder,
            #[cfg(test)]
            None,
        )
    }
    fn spawn_inner(
        store: LibraryStore,
        #[cfg(test)] hook: Option<ReplyBoundaryHook>,
    ) -> Result<Self, ServiceError> {
        Self::spawn_owned_inner(
            store,
            thread::Builder::new().name("gp200-firmware-ledger".into()),
            #[cfg(test)]
            hook,
        )
        .map_err(|failed| failed.error)
    }
    fn spawn_owned_inner(
        store: LibraryStore,
        builder: thread::Builder,
        #[cfg(test)] hook: Option<ReplyBoundaryHook>,
    ) -> Result<Self, FirmwareLedgerStartFailure> {
        let run = match crate::store::nonce() {
            Ok(run) => run,
            Err(error) => {
                return Err(FirmwareLedgerStartFailure {
                    error,
                    store: Box::new(store),
                });
            }
        };
        let (commands, rx) = bounded(CAPACITY);
        let (raw, raw_rx) = bounded(RAW_CAPACITY);
        let (tx, events) = bounded(CAPACITY);
        let stopped = Arc::new(StopGate::default());
        let flag = stopped.clone();
        let thread_run = run.clone();
        let offer = Arc::new(AtomicU64::new(0));
        let worker_offer = offer.clone();
        // Builder::spawn drops its closure on OS failure. A separate owning
        // cell keeps the actual store returnable until a real thread starts.
        let retained = Arc::new(Mutex::new(Some(store)));
        let thread_store = retained.clone();
        let join = match builder.spawn(move || {
            let store = thread_store
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
                .expect("one actual ledger thread takes the moved store");
            run_worker(
                store,
                rx,
                raw_rx,
                tx,
                FirmwareWorkerBookkeeping {
                    stopped: flag,
                    offer: worker_offer,
                },
                thread_run,
                #[cfg(test)]
                hook,
            )
        }) {
            Ok(join) => join,
            Err(error) => {
                let store = retained
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .take()
                    .expect("OS spawn failure never started the ledger owner");
                return Err(FirmwareLedgerStartFailure {
                    error: crate::store::io(error),
                    store: Box::new(store),
                });
            }
        };
        Ok(Self {
            run,
            commands,
            raw,
            events,
            stopped,
            pending: Arc::new(AtomicU64::new(0)),
            offer,
            join: Some(join),
        })
    }
    pub fn run_id(&self) -> &str {
        &self.run
    }
    pub fn try_submit(&self, request: FirmwareLedgerRequest) -> Result<(), ServiceError> {
        let _gate = self.stopped.lock();
        if self.stopped.is_stopped() {
            return Err(ServiceError::new(
                ErrorKind::Indeterminate,
                "Firmware local evidence owner stopped",
            ));
        }
        if request.intent == 0 {
            return Err(ServiceError::invalid(
                "Firmware request intent must be nonzero",
            ));
        }
        if !matches!(request.action, FirmwareLedgerAction::Inspect(_))
            && self
                .pending
                .compare_exchange(0, request.intent, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return Err(ServiceError::new(
                ErrorKind::Busy,
                "Firmware local mutating request is still outstanding",
            ));
        }
        if !matches!(request.action, FirmwareLedgerAction::Inspect(_)) {
            self.offer.store(0, Ordering::SeqCst);
        }
        match self.commands.try_send(request) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.stopped.stopped.store(true, Ordering::SeqCst);
                Err(ServiceError::new(
                    ErrorKind::Busy,
                    "Firmware critical command rail overflow; local action stopped",
                ))
            }
            Err(TrySendError::Disconnected(_)) => {
                self.stopped.stopped.store(true, Ordering::SeqCst);
                Err(ServiceError::new(
                    ErrorKind::Indeterminate,
                    "Firmware critical command rail disconnected",
                ))
            }
        }
    }
    pub fn try_record_callback(&self, callback: FirmwareCallbackDto) -> Result<(), ServiceError> {
        let _gate = self.stopped.lock();
        if self.stopped.is_stopped() {
            return Err(ServiceError::new(
                ErrorKind::Indeterminate,
                "Firmware raw evidence owner stopped",
            ));
        }
        if let Err(e) = callback.validate() {
            self.stopped.stopped.store(true, Ordering::SeqCst);
            return Err(e);
        }
        match self.raw.try_send(callback) {
            Ok(()) => Ok(()),
            Err(_) => {
                self.stopped.stopped.store(true, Ordering::SeqCst);
                Err(ServiceError::new(
                    ErrorKind::Indeterminate,
                    "Firmware critical raw rail lost evidence",
                ))
            }
        }
    }
    pub fn try_next(&self) -> Option<FirmwareLedgerEvent> {
        let _gate = self.stopped.lock();
        let mut event = self.events.try_recv().ok()?;
        if self.stopped.is_stopped() && event.result.is_ok() {
            event.result = Err(revoked_reply());
        }
        let _ = self
            .pending
            .compare_exchange(event.intent, 0, Ordering::SeqCst, Ordering::SeqCst);
        Some(event)
    }
    /// Returns immediately; successful delivery and offer fences also inspect it.
    pub fn request_stop(&self) {
        self.stopped.requested.store(true, Ordering::SeqCst);
    }
    pub fn try_local_offer_fence(
        &self,
        checkpoint: FirmwareLocalActionCheckpoint,
    ) -> Result<FirmwareLocalOfferFence<'_>, ServiceError> {
        let gate = self.stopped.lock();
        if self.stopped.is_stopped()
            || self.pending.load(Ordering::SeqCst) != 0
            || !Arc::ptr_eq(&self.stopped, &checkpoint.issuer)
            || self.run != checkpoint.run
            || self
                .offer
                .compare_exchange(checkpoint.intent, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return Err(revoked_reply());
        }
        Ok(FirmwareLocalOfferFence { _gate: gate })
    }
    pub fn stop(&self) {
        self.stopped.stop();
    }
    pub fn is_stopped(&self) -> bool {
        self.stopped.is_stopped()
    }
    /// Blocking local ownership join; keep off GUI/callback threads. It is not
    /// safe normal-mode admission. A kernel fsync may delay this join.
    pub fn join(mut self) -> Result<LibraryStore, ServiceError> {
        self.stop();
        self.join
            .take()
            .expect("one join owner")
            .join()
            .map_err(|_| {
                ServiceError::new(
                    ErrorKind::Indeterminate,
                    "Firmware ledger thread panicked; reopen local observation",
                )
            })
    }
}
impl Drop for FirmwareLedgerWorker {
    fn drop(&mut self) {
        self.stop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
struct FirmwareWorkerBookkeeping {
    stopped: Arc<StopGate>,
    offer: Arc<AtomicU64>,
}
fn run_worker(
    mut store: LibraryStore,
    commands: Receiver<FirmwareLedgerRequest>,
    raw: Receiver<FirmwareCallbackDto>,
    events: Sender<FirmwareLedgerEvent>,
    bookkeeping: FirmwareWorkerBookkeeping,
    run: String,
    #[cfg(test)] hook: Option<ReplyBoundaryHook>,
) -> LibraryStore {
    let FirmwareWorkerBookkeeping { stopped, offer } = bookkeeping;
    {
        let ledger = FirmwareAttemptLedger::open(&mut store);
        if let Ok(mut ledger) = ledger {
            let mut cursor = None;
            // Cleanup identity survives removal/consumption of the action cursor.
            let mut active_attempt = None;
            let mut job_digest = None;
            let mut sequence = 0;
            let mut last_intent = None;
            while !stopped.is_stopped() {
                match raw.try_recv() {
                    Ok(callback) => {
                        if let Some(c) = cursor.take() {
                            match ledger.observe_callback(c, callback) {
                                Ok(c) => {
                                    sequence += 1;
                                    cursor = Some(c)
                                }
                                Err(_) => {
                                    stopped.stop();
                                }
                            }
                        } else {
                            stopped.stop();
                        }
                        continue;
                    }
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        stopped.stop();
                        break;
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => {}
                }
                let request = match commands.recv_timeout(Duration::from_millis(20)) {
                    Ok(c) => c,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                    Err(_) => break,
                };
                if stopped.is_stopped() {
                    break;
                }
                let intent = request.intent;
                let result = (|| {
                    if last_intent.is_some_and(|v| intent <= v) {
                        return Err(ServiceError::new(
                            ErrorKind::Conflict,
                            "Firmware local request intent is stale",
                        ));
                    }
                    if !matches!(
                        &request.action,
                        FirmwareLedgerAction::Begin { .. } | FirmwareLedgerAction::Inspect(_)
                    ) && (request.expected_sequence != sequence
                        || job_digest.as_ref() != Some(&request.review_sha256))
                    {
                        return Err(ServiceError::new(
                            ErrorKind::Conflict,
                            "Firmware local request sequence/review is stale",
                        ));
                    }
                    if !matches!(&request.action, FirmwareLedgerAction::Inspect(_)) {
                        offer.store(0, Ordering::SeqCst);
                    }
                    last_intent = Some(intent);
                    match request.action {
                        FirmwareLedgerAction::Begin { job, origin } => {
                            if cursor.is_some()
                                || job_digest.is_some()
                                || request.expected_sequence != 0
                                || request.review_sha256 != job.digest().hex()
                            {
                                return Err(ServiceError::new(
                                    ErrorKind::Conflict,
                                    "Firmware local begin is stale or already consumed",
                                ));
                            }
                            let id = ledger.reserve_id()?;
                            active_attempt = Some(id.clone());
                            job_digest = Some(job.digest().hex());
                            let (c, checkpoint) = ledger.begin_reserved(&id, job, origin)?;
                            cursor = Some(c);
                            sequence = checkpoint.sequence();
                            Ok(FirmwareLedgerReply::Prepared {
                                attempt: id,
                                checkpoint,
                            })
                        }
                        FirmwareLedgerAction::Inspect(id) => Ok(FirmwareLedgerReply::Observation(
                            Box::new(ledger.load(&id)?),
                        )),
                        action => {
                            let c = cursor.take().ok_or_else(|| {
                                ServiceError::new(
                                    ErrorKind::Conflict,
                                    "Firmware local cursor is absent",
                                )
                            })?;
                            match action {
                                FirmwareLedgerAction::Checkpoint { ordinal } => {
                                    let (c, checkpoint) = ledger.checkpoint(c, ordinal)?;
                                    sequence = checkpoint.sequence();
                                    cursor = Some(c);
                                    Ok(FirmwareLedgerReply::Checkpoint(checkpoint))
                                }
                                FirmwareLedgerAction::LifecyclePrepared(p) => {
                                    cursor = Some(ledger.prepare_lifecycle(c, p)?);
                                    sequence += 1;
                                    Ok(FirmwareLedgerReply::Recorded { sequence })
                                }
                                FirmwareLedgerAction::LifecycleEntryPrepared(p) => {
                                    cursor = Some(ledger.prepare_entry_lifecycle(c, p)?);
                                    sequence += 1;
                                    Ok(FirmwareLedgerReply::Recorded { sequence })
                                }
                                FirmwareLedgerAction::LifecycleEntryHostSettled(s) => {
                                    cursor = Some(ledger.lifecycle_entry_host_settled(c, s)?);
                                    sequence += 1;
                                    Ok(FirmwareLedgerReply::Recorded { sequence })
                                }
                                FirmwareLedgerAction::LifecycleEpoch(epoch) => {
                                    let (c, checkpoint) = ledger.lifecycle_epoch(c, *epoch)?;
                                    // Use the actual independently reread durable
                                    // checkpoint rather than guessing an epoch's
                                    // sequence after a source-side mutation.
                                    sequence = checkpoint.sequence();
                                    cursor = Some(c);
                                    Ok(FirmwareLedgerReply::Recorded { sequence })
                                }
                                FirmwareLedgerAction::LifecycleCheckpoint(action) => {
                                    let (c, checkpoint) =
                                        ledger.lifecycle_checkpoint(c, action.clone())?;
                                    sequence = checkpoint.sequence();
                                    cursor = Some(c);
                                    offer.store(intent, Ordering::SeqCst);
                                    Ok(FirmwareLedgerReply::ActionCheckpoint(
                                        FirmwareLocalActionCheckpoint {
                                            checkpoint,
                                            action: Box::new(action),
                                            run: run.clone(),
                                            intent,
                                            issuer: stopped.clone(),
                                        },
                                    ))
                                }
                                FirmwareLedgerAction::LifecycleRaw(raw) => {
                                    cursor = Some(ledger.lifecycle_raw(c, raw)?);
                                    sequence += 1;
                                    Ok(FirmwareLedgerReply::Recorded { sequence })
                                }
                                FirmwareLedgerAction::LifecycleSend(send) => {
                                    cursor = Some(ledger.lifecycle_send(c, send)?);
                                    sequence += 1;
                                    Ok(FirmwareLedgerReply::Recorded { sequence })
                                }
                                FirmwareLedgerAction::LifecycleDecision(d) => {
                                    cursor = Some(ledger.lifecycle_decision(c, d)?);
                                    sequence += 1;
                                    Ok(FirmwareLedgerReply::Recorded { sequence })
                                }
                                FirmwareLedgerAction::LifecycleFailure { raw, send, note } => {
                                    if raw.len() > 192 || note.is_empty() || note.len() > 512 {
                                        return Err(ServiceError::invalid(
                                            "Firmware failure evidence exceeds bound",
                                        ));
                                    }
                                    let mut current = c;
                                    if let Some(s) = send {
                                        current = ledger.lifecycle_send(current, s)?;
                                        sequence += 1;
                                    }
                                    for r in raw {
                                        current = ledger.lifecycle_negative(
                                            current,
                                            FirmwareLifecycleNegativeObservation {
                                                raw: r,
                                                note: note.clone(),
                                            },
                                        )?;
                                        sequence += 1;
                                    }
                                    ledger.evidence_fault(current, note)?;
                                    sequence += 1;
                                    cursor = None;
                                    Ok(FirmwareLedgerReply::Recorded { sequence })
                                }
                                FirmwareLedgerAction::LifecycleClosed(reason) => {
                                    let closed = ledger.lifecycle_close(c, reason)?;
                                    active_attempt = None;
                                    Ok(FirmwareLedgerReply::Observation(Box::new(closed)))
                                }
                                FirmwareLedgerAction::Send(s) => {
                                    cursor = Some(ledger.observe_send(c, s)?);
                                    sequence += 1;
                                    Ok(FirmwareLedgerReply::Recorded { sequence })
                                }
                                FirmwareLedgerAction::MockDecision(d) => {
                                    cursor = Some(ledger.acknowledge_mock(c, d)?);
                                    sequence += 1;
                                    Ok(FirmwareLedgerReply::Recorded { sequence })
                                }
                                FirmwareLedgerAction::ModeBoundary { raw, note } => {
                                    cursor = Some(ledger.record_mode_boundary(c, raw, note)?);
                                    sequence += 1;
                                    Ok(FirmwareLedgerReply::Recorded { sequence })
                                }
                                FirmwareLedgerAction::Close(reason) => {
                                    let closed = ledger.close(c, reason)?;
                                    active_attempt = None;
                                    Ok(FirmwareLedgerReply::Observation(Box::new(closed)))
                                }
                                _ => unreachable!("outer match covers begin/inspect"),
                            }
                        }
                    }
                })();
                if result.is_err() {
                    stopped.stop();
                }
                #[cfg(test)]
                reply_boundary(&hook, intent, false);
                let queued = {
                    let _gate = stopped.lock();
                    let result = if stopped.is_stopped() && result.is_ok() {
                        Err(revoked_reply())
                    } else {
                        result
                    };
                    let queued = events.try_send(FirmwareLedgerEvent {
                        run: run.clone(),
                        intent,
                        result,
                    });
                    if queued.is_err() {
                        stopped.stopped.store(true, Ordering::SeqCst);
                    }
                    queued
                };
                #[cfg(test)]
                reply_boundary(&hook, intent, true);
                if queued.is_err() {
                    break;
                }
            }
            // Drain already queued bounded raw records. No action acknowledgement
            // escapes once the sticky flag is set. Never synthesize send evidence.
            while let Ok(callback) = raw.try_recv() {
                if let Some(c) = cursor.take()
                    && let Ok(c) = ledger.observe_callback(c, callback)
                {
                    cursor = Some(c);
                }
            }
            drop(cursor.take());
            if let Some(id) = active_attempt {
                ledger.abandon_owner(
                    &id,
                    "Firmware local owner stopped or lost a critical acknowledgement".into(),
                );
            }
        } else {
            stopped.stop();
        }
    }
    store
}
#[cfg(test)]
#[path = "firmware_ledger_worker_tests.rs"]
mod tests;
