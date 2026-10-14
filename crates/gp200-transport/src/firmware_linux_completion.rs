//! Nondefault concrete Linux execution. Hardware acceptance is a separate ROOT action.
use super::{
    firmware_linux_completion_admission::{LinuxCompletionAuthorization, bad, publish},
    firmware_linux_completion_binding::{
        self as binding, CompletionBindingWitness, CompletionOwnedAnchor,
    },
    firmware_linux_completion_io::{
        CompletionFramer, CompletionIo, CompletionRawIo, CompletionSyscall,
    },
    firmware_linux_completion_journal::{
        CompletionJournal, JournalReceipt, JournalReply, MAX_BATCH_BYTES, MAX_BATCH_RECORDS,
    },
};
use crate::{
    firmware_backend::{FirmwareNormalHandoff, FirmwareReceiver},
    firmware_service::{FirmwareOwnerSummary, OwnerStop},
    firmware_vendor_sequence::*,
};
use gp200_domain::*;
use gp200_library::LibraryStore;
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, atomic::Ordering},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum LinuxCompletionPhase {
    Entry,
    FindingBoot,
    ReviewBootQuery,
    QueryingBoot,
    ReviewTransfer,
    Transferring,
    HostFinishedUnverified,
    FindingNormal,
    ReadingNormal,
    ConfirmApplicationVersion,
    ClosingAttempt,
    ReturningEditor,
    RecoveryRequired,
}
#[derive(Debug, Clone, Serialize)]
pub struct LinuxCompletionView {
    pub phase: LinuxCompletionPhase,
    pub run: String,
    pub review_digest: Option<String>,
    pub review: Option<serde_json::Value>,
    pub os_offered_bytes: u64,
    pub host_write_invocations: u64,
    pub host_accepted_blocks: u64,
    pub total_blocks: Option<u64>,
    pub device_verified_bytes: u64,
    pub host_finished_unverified: bool,
    pub normal_mode_verified: bool,
    pub target_version_verified: bool,
    pub contents_verified: bool,
    pub quarantined: bool,
    pub diagnostic: Option<String>,
    pub retained_raw_chunks: Vec<FirmwareLifecycleRawObservation>,
    pub allocated_raw_frontier: u64,
    /// Original RAM evidence retained when its durable outcome did not settle.
    pub retained_block_outcome: Option<serde_json::Value>,
    pub reader_retirement_diagnostics: Vec<serde_json::Value>,
}
impl LinuxCompletionView {
    fn initial(run: String) -> Self {
        Self {
            phase: LinuxCompletionPhase::Entry,
            run,
            review_digest: None,
            review: None,
            os_offered_bytes: 0,
            host_write_invocations: 0,
            host_accepted_blocks: 0,
            total_blocks: None,
            device_verified_bytes: 0,
            host_finished_unverified: false,
            normal_mode_verified: false,
            target_version_verified: false,
            contents_verified: false,
            quarantined: true,
            diagnostic: None,
            retained_raw_chunks: vec![],
            allocated_raw_frontier: 0,
            retained_block_outcome: None,
            reader_retirement_diagnostics: vec![],
        }
    }
    pub(crate) fn summary(&self) -> FirmwareOwnerSummary {
        FirmwareOwnerSummary {
            phase: format!("Linux completion: {:?}", self.phase),
            simulated_source_bytes: 0,
            os_offered_bytes: self.os_offered_bytes,
            device_verified_bytes: 0,
            completion_verified: false,
            quarantined: self.quarantined,
            diagnostic: self.diagnostic.clone(),
            retained_raw_chunks: self.retained_raw_chunks.clone(),
            allocated_raw_frontier: self.allocated_raw_frontier,
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct LinuxCompletionOutcome {
    pub view: LinuxCompletionView,
    pub selection: Option<DeviceSelection>,
    pub original_normal_epoch: SessionEpoch,
    pub verified_normal_epoch: Option<SessionEpoch>,
    pub application_version: Option<String>,
    pub journal_path: String,
    pub editor_return_allowed: bool,
}
/// Read-only live projection. It cannot submit work, approve a phase, or clear a hold.
#[derive(Clone, Default)]
pub struct LinuxCompletionMonitor {
    view: Arc<Mutex<Option<Arc<Mutex<LinuxCompletionView>>>>>,
}
impl LinuxCompletionMonitor {
    pub fn view(&self) -> Option<LinuxCompletionView> {
        self.view
            .try_lock()
            .ok()?
            .as_ref()?
            .try_lock()
            .ok()
            .map(|v| v.clone())
    }
    pub(crate) fn attach(&self, owner: &LinuxCompletionOwner) {
        *self.view.lock().unwrap_or_else(|p| p.into_inner()) = Some(owner.view.clone());
    }
}
enum OwnedOutcome {
    Normal(Box<LinuxCompletionOutcome>),
    Recovery(Box<crate::LinuxRecoveryOutcome>),
}
struct RunResult {
    store: LibraryStore,
    outcome: OwnedOutcome,
}

enum ExecutionAuthorization {
    Normal(Box<LinuxCompletionAuthorization>),
    Recovery(Box<crate::firmware_linux_recovery::LinuxRecoveryAuthorization>),
}
impl ExecutionAuthorization {
    fn original_normal_binding(&self) -> &FirmwareUsbObservation {
        match self {
            Self::Normal(a) => &a.review.reservation.usb,
            Self::Recovery(a) => a.lineage.original_binding(),
        }
    }
    fn inputs(&self) -> &crate::LinuxCompletionInputs {
        match self {
            Self::Normal(a) => &a.review.inputs,
            Self::Recovery(a) => &a.inputs,
        }
    }
    fn digest(&self) -> &str {
        match self {
            Self::Normal(a) => a.digest(),
            Self::Recovery(a) => a.digest(),
        }
    }
    fn entry_fresh(&self) -> Result<(), ServiceError> {
        match self {
            Self::Normal(a) => a.entry_fresh(),
            Self::Recovery(a) => a.fresh(),
        }
    }
    fn revalidate(&self) -> Result<(), ServiceError> {
        match self {
            Self::Normal(a) => a.revalidate(),
            Self::Recovery(a) => a.revalidate(),
        }
    }
    fn read_signal(&self, name: &str, expected: &str) -> Result<bool, ServiceError> {
        match self {
            Self::Normal(a) => a.read_signal(name, expected),
            Self::Recovery(a) => a.read_signal(name, expected),
        }
    }
    fn firmware_review(&self) -> &FirmwareReview {
        match self {
            Self::Normal(a) => a.job.review(),
            Self::Recovery(a) => &a.image,
        }
    }
    fn job_digest(&self) -> String {
        match self {
            Self::Normal(a) => a.job.digest().hex(),
            Self::Recovery(a) => a.digest().to_owned(),
        }
    }
    fn backup(&self) -> serde_json::Value {
        match self {
            Self::Normal(a) => serde_json::json!(a.review.captured_materials),
            Self::Recovery(a) => {
                serde_json::json!({"historical_captures":a.history.historical_captures,"fresh_normal_backup_available":false,"installed_ir_audio_available":false})
            }
        }
    }
    fn limitations(&self) -> &str {
        match self {
            Self::Normal(a) => &a.review.limitations,
            Self::Recovery(a) => &a.history.limitations,
        }
    }
    fn history(&self) -> Option<&crate::LinuxRecoveryHistoryReview> {
        match self {
            Self::Normal(_) => None,
            Self::Recovery(a) => Some(&a.history),
        }
    }
    fn lineage(&self) -> Option<&crate::firmware_linux_recovery_admission::CheckedRecoveryLineage> {
        match self {
            Self::Normal(_) => None,
            Self::Recovery(a) => Some(&a.lineage),
        }
    }
    fn is_recovery(&self) -> bool {
        matches!(self, Self::Recovery(_))
    }
}
pub(crate) struct LinuxCompletionStartFailure {
    pub(crate) error: ServiceError,
    pub(crate) store: LibraryStore,
}
pub(crate) struct LinuxCompletionOwner {
    thread: Option<JoinHandle<Result<RunResult, ServiceError>>>,
    view: Arc<Mutex<LinuxCompletionView>>,
    last: FirmwareOwnerSummary,
    stop: Arc<OwnerStop>,
}
struct ExecutorStartFailure {
    error: ServiceError,
    input: StartInput,
}
struct StartInput {
    authorization: LinuxCompletionAuthorization,
    store: LibraryStore,
    handoff: FirmwareNormalHandoff,
    stop: Arc<OwnerStop>,
    #[cfg(test)]
    fixture: Option<tests::FixturePlatform>,
}
impl LinuxCompletionOwner {
    pub(crate) fn start(
        authorization: LinuxCompletionAuthorization,
        store: LibraryStore,
        handoff: FirmwareNormalHandoff,
        stop: Arc<OwnerStop>,
    ) -> Result<Self, Box<LinuxCompletionStartFailure>> {
        Self::start_input(StartInput {
            authorization,
            store,
            handoff,
            stop,
            #[cfg(test)]
            fixture: None,
        })
    }
    #[cfg(test)]
    fn start_fixture(
        authorization: LinuxCompletionAuthorization,
        store: LibraryStore,
        handoff: FirmwareNormalHandoff,
        stop: Arc<OwnerStop>,
        fixture: tests::FixturePlatform,
    ) -> Result<Self, Box<LinuxCompletionStartFailure>> {
        Self::start_input(StartInput {
            authorization,
            store,
            handoff,
            stop,
            fixture: Some(fixture),
        })
    }
    fn start_input(input: StartInput) -> Result<Self, Box<LinuxCompletionStartFailure>> {
        let StartInput {
            authorization,
            handoff,
            ..
        } = &input;
        let check = (|| {
            authorization.entry_fresh()?;
            crate::firmware_backend::NormalEntryBinding::from_handoff(handoff)?;
            if handoff.binding != authorization.review.reservation.usb
                || handoff.normal_epoch != authorization.review.reservation.epoch
            {
                return Err(bad("Completion handoff is not this captured normal owner"));
            }
            Ok(())
        })();
        if let Err(error) = check {
            return Err(Box::new(LinuxCompletionStartFailure {
                error,
                store: input.store,
            }));
        }
        let run = JobSha256::of(
            format!(
                "completion:{}:{}:{:?}",
                std::process::id(),
                authorization.digest(),
                Instant::now()
            )
            .as_bytes(),
        )
        .hex()[..32]
            .to_owned();
        let view = Arc::new(Mutex::new(LinuxCompletionView::initial(run.clone())));
        let thread_view = view.clone();
        let owner_stop = input.stop.clone();
        let retained = Arc::new(Mutex::new(Some(input)));
        let thread_input = retained.clone();
        let thread = thread::Builder::new()
            .name("gp200-linux-completion-owner".into())
            .spawn(move || {
                let input = thread_input
                    .lock()
                    .map_err(|_| bad("Completion startup owner poisoned"))?
                    .take()
                    .ok_or_else(|| bad("Completion startup owner missing"))?;
                match Executor::start(input, run, thread_view.clone()) {
                    Ok(executor) => executor.execute(),
                    Err(failed) => return_startup_without_call(*failed, thread_view),
                }
            });
        match thread {
            Ok(thread) => {
                let last = view.lock().expect("new view").summary();
                Ok(Self {
                    thread: Some(thread),
                    view,
                    last,
                    stop: owner_stop,
                })
            }
            Err(error) => {
                let input = retained
                    .lock()
                    .expect("startup lock")
                    .take()
                    .expect("unstarted owner");
                Err(Box::new(LinuxCompletionStartFailure {
                    error: bad(error),
                    store: input.store,
                }))
            }
        }
    }
    pub(crate) fn start_recovery(
        request: crate::LinuxRecoveryRequest,
        epoch: SessionEpoch,
    ) -> Result<Self, Box<LinuxCompletionStartFailure>> {
        let run = JobSha256::of(
            format!("fresh-recovery:{}:{:?}", std::process::id(), Instant::now()).as_bytes(),
        )
        .hex()[..32]
            .to_owned();
        let mut initial = LinuxCompletionView::initial(run.clone());
        initial.phase = LinuxCompletionPhase::FindingBoot;
        let view = Arc::new(Mutex::new(initial));
        let thread_view = view.clone();
        let stop = request.stop.clone();
        let retained = Arc::new(Mutex::new(Some(request)));
        let thread_input = retained.clone();
        let thread = thread::Builder::new()
            .name("gp200-linux-recovery-owner".into())
            .spawn(move || {
                let request = thread_input
                    .lock()
                    .map_err(|_| bad("Recovery startup owner poisoned"))?
                    .take()
                    .ok_or_else(|| bad("Recovery startup owner missing"))?;
                Executor::start_fresh_recovery(
                    request,
                    epoch,
                    run,
                    thread_view,
                    #[cfg(test)]
                    None,
                )
            });
        match thread {
            Ok(thread) => {
                let last = view.lock().expect("new recovery view").summary();
                Ok(Self {
                    thread: Some(thread),
                    view,
                    last,
                    stop,
                })
            }
            Err(error) => {
                let request = retained
                    .lock()
                    .expect("unstarted recovery owner")
                    .take()
                    .expect("one request");
                Err(Box::new(LinuxCompletionStartFailure {
                    error: bad(error),
                    store: request.store,
                }))
            }
        }
    }
    #[cfg(test)]
    fn start_recovery_fixture(
        request: crate::LinuxRecoveryRequest,
        epoch: SessionEpoch,
        anchor: CompletionOwnedAnchor,
        fixture: tests::FixturePlatform,
    ) -> Self {
        let run = JobSha256::of(
            format!(
                "mock-fresh-recovery:{}:{:?}",
                std::process::id(),
                Instant::now()
            )
            .as_bytes(),
        )
        .hex()[..32]
            .to_owned();
        let mut initial = LinuxCompletionView::initial(run.clone());
        initial.phase = LinuxCompletionPhase::FindingBoot;
        let view = Arc::new(Mutex::new(initial));
        let thread_view = view.clone();
        let stop = request.stop.clone();
        let thread = thread::Builder::new()
            .name("gp200-mock-recovery-owner".into())
            .spawn(move || {
                Executor::start_fresh_recovery(
                    request,
                    epoch,
                    run,
                    thread_view,
                    Some((fixture, anchor)),
                )
            })
            .unwrap();
        let last = view.lock().unwrap().summary();
        Self {
            thread: Some(thread),
            view,
            last,
            stop,
        }
    }
    pub(crate) fn retire_recovery(
        mut self,
    ) -> Result<
        (
            FirmwareOwnerSummary,
            LibraryStore,
            crate::LinuxRecoveryOutcome,
        ),
        ServiceError,
    > {
        let result = self
            .thread
            .take()
            .expect("one recovery owner")
            .join()
            .map_err(|_| bad("Recovery owner panicked; original store unavailable"))??;
        match result.outcome {
            OwnedOutcome::Recovery(outcome) => Ok((outcome.view.summary(), result.store, *outcome)),
            OwnedOutcome::Normal(_) => Err(bad("Normal completion cannot retire as recovery")),
        }
    }
    pub(crate) fn tick(&mut self) {
        if let Ok(view) = self.view.try_lock() {
            self.last = view.summary();
        }
    }
    pub(crate) fn summary(&self) -> &FirmwareOwnerSummary {
        &self.last
    }
    #[cfg(test)]
    pub(crate) fn view(&self) -> LinuxCompletionView {
        self.view.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    pub(crate) fn request_host_stop(&mut self) {
        self.stop.requested.store(true, Ordering::SeqCst);
    }
    pub(crate) fn stopped(&self) -> bool {
        self.thread.as_ref().is_some_and(JoinHandle::is_finished)
    }
    pub(crate) fn retire(
        mut self,
    ) -> Result<(FirmwareOwnerSummary, LibraryStore, LinuxCompletionOutcome), ServiceError> {
        let result = self
            .thread
            .take()
            .expect("one completion owner")
            .join()
            .map_err(|_| bad("Completion owner panicked; original store unavailable"))??;
        match result.outcome {
            OwnedOutcome::Normal(outcome) => Ok((outcome.view.summary(), result.store, *outcome)),
            OwnedOutcome::Recovery(_) => {
                Err(bad("Recovery owner cannot retire as a normal completion"))
            }
        }
    }
}
impl Drop for LinuxCompletionOwner {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
/// Only the live executor can construct this consuming clearance witness. No DTO constructor.
pub(crate) struct LiveCompletionClearance {
    attempt: FirmwareAttemptId,
    run: String,
    journal: JournalReceipt,
    guard: crate::device_session_guard::DeviceSessionGuard,
    original_binding: FirmwareUsbObservation,
    record: serde_json::Value,
}
impl LiveCompletionClearance {
    pub(crate) fn consume(
        self,
        attempt: &FirmwareAttemptId,
    ) -> Result<
        (
            crate::device_session_guard::DeviceSessionGuard,
            serde_json::Value,
        ),
        ServiceError,
    > {
        if &self.attempt != attempt
            || self.run != self.journal.run
            || self.journal.sequence == 0
            || JobSha256::parse(&self.journal.prefix_sha256)?.hex() != self.journal.prefix_sha256
        {
            return Err(bad(
                "Live completion clearance differs from this owner/attempt",
            ));
        }
        if !self.guard.owns_normal_binding(&self.original_binding) {
            return Err(bad(
                "Live completion original physical/alias ownership absent",
            ));
        }
        self.journal.revalidate()?;
        Ok((self.guard, self.record))
    }
}
/// Consuming proof produced only after this owner's actual whole-image sequence,
/// fresh normal actor reads, reader retirement and new application confirmation.
pub(crate) struct LiveRecoveryCompletionClearance<'a> {
    attempt: FirmwareAttemptId,
    run: String,
    journal: JournalReceipt,
    guard: crate::device_session_guard::DeviceSessionGuard,
    anchor_binding: FirmwareUsbObservation,
    lineage: &'a crate::firmware_linux_recovery_admission::CheckedRecoveryLineage,
    record: serde_json::Value,
    #[cfg(test)]
    mock_origin: bool,
}
impl LiveRecoveryCompletionClearance<'_> {
    #[cfg(test)]
    pub(crate) fn is_mock_origin(&self) -> bool {
        self.mock_origin
    }
    pub(crate) fn consume(
        self,
        attempt: &FirmwareAttemptId,
        directory: &std::fs::File,
    ) -> Result<
        (
            crate::device_session_guard::DeviceSessionGuard,
            Vec<crate::firmware_linux_recovery_admission::RecoveryPredecessorReference>,
            serde_json::Value,
            JournalReceipt,
        ),
        ServiceError,
    > {
        if &self.attempt != attempt
            || self.run != self.journal.run
            || self.journal.sequence == 0
            || !self.guard.owns_normal_binding(&self.anchor_binding)
        {
            return Err(bad(
                "Live recovery clearance differs from actual owner/physical guard",
            ));
        }
        self.lineage.matches_registry(directory)?;
        self.lineage.revalidate_except_live(Some(attempt))?;
        self.journal.revalidate()?;
        Ok((
            self.guard,
            self.lineage.references(),
            self.record,
            self.journal,
        ))
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum RecordingPolicy {
    Immediate,
    BlockOutcome,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum JournalWaitMode {
    Operation,
    EvidenceSettlement,
}
struct PendingJournalWait {
    intent: u64,
    deadline: Instant,
}
struct RetainedSyscall {
    kind: &'static str,
    ordinal: u64,
    observation: CompletionSyscall,
    delivery_at_ms: Option<u64>,
    intent: Option<u64>,
}
impl RetainedSyscall {
    fn record(&self) -> serde_json::Value {
        serde_json::json!({"kind":self.kind,"ordinal":self.ordinal,
            "observation":self.observation,"journal_delivery_at_ms":self.delivery_at_ms})
    }
}
enum OutcomeSubmission {
    Staged,
    Submitted(u64),
    Failed(ServiceError),
}
/// Only this original Executor owns the tentative Block observations. Clearing
/// a prefix requires its exact live acknowledgement; disk replay grants no I/O.
struct PendingBlockOutcome {
    ordinal: u64,
    syscalls: Vec<RetainedSyscall>,
    raw: Vec<serde_json::Value>,
    pair: Option<serde_json::Value>,
    immediate_raw: bool,
    immediate_drain: bool,
    submission: OutcomeSubmission,
    acknowledged_prefix: Option<u64>,
    submitted_intent: Option<u64>,
    frame_spans: Vec<serde_json::Value>,
    // Engine retirement is provisional until the exact original journal reply.
    completed: Option<VendorActionReceipt>,
    combined_checkpoint: Option<(u64, String, Vec<u8>)>,
}
impl PendingBlockOutcome {
    fn new(ordinal: u64) -> Self {
        Self {
            ordinal,
            syscalls: Vec::with_capacity(2),
            raw: Vec::with_capacity(64),
            pair: None,
            immediate_raw: false,
            immediate_drain: false,
            submission: OutcomeSubmission::Staged,
            acknowledged_prefix: None,
            submitted_intent: None,
            frame_spans: Vec::new(),
            completed: None,
            combined_checkpoint: None,
        }
    }
    fn records(&self) -> Vec<serde_json::Value> {
        self.syscalls
            .iter()
            .map(RetainedSyscall::record)
            .chain(self.raw.iter().cloned())
            .chain(self.pair.iter().cloned())
            .collect()
    }
    fn diagnostic(&self) -> serde_json::Value {
        serde_json::json!({"ordinal":self.ordinal,"original_records":self.records(),
            "original_intent":self.submitted_intent,"original_frame_spans":self.frame_spans,
            "acknowledged_prefix":self.acknowledged_prefix,"uncertain":true,"no_retry":true,
            "provisional_completed_receipt":self.completed.as_ref().map(receipt_json),
            "original_combined_checkpoint":self.combined_checkpoint.as_ref().map(|(ordinal,purpose,wire)|
                serde_json::json!({"ordinal":ordinal,"purpose":purpose,"wire":wire,
                    "wire_sha256":JobSha256::of(wire).hex()}))})
    }
}
struct Executor {
    authorization: ExecutionAuthorization,
    anchor: CompletionOwnedAnchor,
    stop: Arc<OwnerStop>,
    view: Arc<Mutex<LinuxCompletionView>>,
    journal: Option<CompletionJournal>,
    quarantine: Option<crate::firmware_quarantine::QuarantineAttempt>,
    attempt: FirmwareAttemptId,
    origin: Instant,
    receiver: Option<FirmwareReceiver>,
    io: Option<Box<dyn CompletionIo>>,
    raw_frontier: u64,
    last_raw_ns: u64,
    framer: CompletionFramer,
    raw_pending: VecDeque<FirmwareLifecycleRawObservation>,
    frames: VecDeque<super::firmware_linux_completion_io::CompletionFrame>,
    generation: u64,
    epoch: SessionEpoch,
    offer_ordinal: u64,
    calls: u64,
    zero_call_teardown: bool,
    evidence_teardown: bool,
    last_receipt: Option<JournalReceipt>,
    journal_wait: Option<PendingJournalWait>,
    pending_block: Option<PendingBlockOutcome>,
    pending_syscall: Option<RetainedSyscall>,
    raw_pending_intent: Option<u64>,
    selection: Option<DeviceSelection>,
    normal_proof: Option<CompletionNormalProof>,
    target_version: Option<String>,
    original_epoch: Option<SessionEpoch>,
    editor_source_epoch: Option<SessionEpoch>,
    active_binding: FirmwareUsbObservation,
    last_offer_sequence: u64,
    last_offer_ns: u64,
    raw_fault: Option<ServiceError>,
    active_receipt: Option<crate::firmware_entry_enumeration::EnumerationReceipt>,
    authority_deadline: Instant,
    #[cfg(test)]
    fixture: Option<tests::FixturePlatform>,
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CompletionNormalProof {
    pub(crate) epoch: SessionEpoch,
    pub(crate) binding: FirmwareUsbObservation,
    pub(crate) identity_sha256: String,
    pub(crate) current_sha256: String,
    pub(crate) globals_sha256: String,
    pub(crate) allocated: u64,
    pub(crate) processed: u64,
    pub(crate) reader_joined: bool,
    pub(crate) output_destroyed: bool,
    pub(crate) retained_raw: Vec<serde_json::Value>,
}
impl Executor {
    fn start(
        input: StartInput,
        run: String,
        view: Arc<Mutex<LinuxCompletionView>>,
    ) -> Result<Self, Box<ExecutorStartFailure>> {
        let StartInput {
            authorization,
            store,
            handoff,
            stop,
            #[cfg(test)]
            fixture,
        } = input;
        let original_epoch = handoff.normal_epoch;
        let epoch = match original_epoch.0.checked_add(1) {
            Some(v) => SessionEpoch(v),
            None => {
                return Err(Box::new(ExecutorStartFailure {
                    error: bad("Completion epoch exhausted"),
                    input: StartInput {
                        authorization,
                        store,
                        handoff,
                        stop,
                        #[cfg(test)]
                        fixture,
                    },
                }));
            }
        };
        let journal = match CompletionJournal::start(
            store,
            run.clone(),
            serde_json::json!({"review":authorization.review,"review_digest":authorization.digest(),"original_normal_epoch":original_epoch}),
        ) {
            Ok(j) => j,
            Err(failed) => {
                return Err(Box::new(ExecutorStartFailure {
                    error: failed.error,
                    input: StartInput {
                        authorization,
                        store: failed.store,
                        handoff,
                        stop,
                        #[cfg(test)]
                        fixture,
                    },
                }));
            }
        };
        let attempt = FirmwareAttemptId::parse(&run).expect("generated canonical run ID");
        #[cfg(test)]
        if let Some(fixture) = &fixture {
            match fixture.fault {
                tests::FixtureFault::StopPendingCheckpoint => {
                    journal.test_stop_before_reply(true, stop.clone())
                }
                tests::FixtureFault::StopPendingRecord => {
                    journal.test_stop_before_reply(false, stop.clone())
                }
                _ => {}
            }
        }
        let active_binding = handoff.binding.clone();
        let authority_deadline = authorization.entry_deadline();
        let anchor = CompletionOwnedAnchor::from_captured_normal(handoff)
            .expect("validated actual normal handoff");
        Ok(Self {
            authorization: ExecutionAuthorization::Normal(Box::new(authorization)),
            anchor,
            stop,
            view,
            journal: Some(journal),
            quarantine: None,
            attempt,
            origin: Instant::now(),
            receiver: None,
            io: None,
            raw_frontier: 0,
            last_raw_ns: 0,
            framer: CompletionFramer::new(),
            raw_pending: VecDeque::new(),
            frames: VecDeque::new(),
            generation: 1,
            epoch,
            offer_ordinal: 0,
            calls: 0,
            zero_call_teardown: false,
            evidence_teardown: false,
            last_receipt: None,
            journal_wait: None,
            pending_block: None,
            pending_syscall: None,
            raw_pending_intent: None,
            selection: None,
            normal_proof: None,
            target_version: None,
            original_epoch: Some(original_epoch),
            editor_source_epoch: None,
            active_binding,
            last_offer_sequence: 0,
            last_offer_ns: 0,
            raw_fault: None,
            active_receipt: None,
            authority_deadline,
            #[cfg(test)]
            fixture,
        })
    }
    fn start_fresh_recovery(
        request: crate::LinuxRecoveryRequest,
        epoch: SessionEpoch,
        run: String,
        view: Arc<Mutex<LinuxCompletionView>>,
        #[cfg(test)] fixture_input: Option<(tests::FixturePlatform, CompletionOwnedAnchor)>,
    ) -> Result<RunResult, ServiceError> {
        let crate::LinuxRecoveryRequest {
            image,
            inputs,
            selected,
            editor_source_epoch,
            store,
            monitor: _,
            stop,
        } = request;
        #[cfg(test)]
        let mut fixture = None;
        let preparation = (|| {
            if stop.requested.load(Ordering::SeqCst) {
                return Err(bad("Recovery stopped before checked preparation"));
            }
            if epoch.0 == 0 {
                return Err(bad("Actual new recovery owner epoch must be nonzero"));
            }
            #[cfg(test)]
            let (lineage, anchor) = if let Some((platform, anchor)) = fixture_input {
                let registry =
                    crate::firmware_quarantine::QuarantineRegistry::mock(&platform.registry)?;
                let lineage = registry.checked_recovery_fixture(
                    store.root(),
                    selected
                        .as_ref()
                        .ok_or_else(|| bad("Mock recovery selected predecessor required"))?,
                )?;
                fixture = Some(platform);
                (lineage, anchor)
            } else {
                let registry = crate::firmware_quarantine::QuarantineRegistry::system()?;
                let lineage = registry.checked_recovery(store.root(), selected.as_ref())?;
                let anchor = CompletionOwnedAnchor::fresh_recovery(&lineage, &registry)?;
                (lineage, anchor)
            };
            #[cfg(not(test))]
            let (lineage, anchor) = {
                let registry = crate::firmware_quarantine::QuarantineRegistry::system()?;
                let lineage = registry.checked_recovery(store.root(), selected.as_ref())?;
                let anchor = CompletionOwnedAnchor::fresh_recovery(&lineage, &registry)?;
                (lineage, anchor)
            };
            // Passive association cannot prove boot mode; only the newly reviewed
            // exact39-byte live query response admits a fresh transfer below.
            let authorization =
                crate::firmware_linux_recovery::LinuxRecoveryAuthorization::from_fresh_owner(
                    image, inputs, lineage, &anchor,
                )?;
            if stop.requested.load(Ordering::SeqCst) {
                return Err(bad("Recovery stopped during checked preparation"));
            }
            Ok((authorization, anchor))
        })();
        let (authorization, anchor) = match preparation {
            Ok(v) => v,
            Err(error) => {
                return Ok(recovery_startup_held(
                    store,
                    view,
                    editor_source_epoch,
                    None,
                    error,
                ));
            }
        };
        let history = authorization.history.clone();
        let metadata = serde_json::json!({"purpose":crate::firmware_linux_recovery::FRESH_RECOVERY_PURPOSE,
            "review":{"schema":1,"job_sha256":authorization.digest(),"history":history,"inputs":authorization.inputs},
            "review_digest":authorization.digest(),"original_binding":history.original_association,
            "anchor_binding":anchor.binding,"predecessors":history.predecessors,
            "fresh_normal_backup_available":false,"editor_source_epoch":editor_source_epoch});
        let journal = match CompletionJournal::start(store, run.clone(), metadata) {
            Ok(j) => j,
            Err(failed) => {
                return Ok(recovery_startup_held(
                    failed.store,
                    view,
                    editor_source_epoch,
                    Some(history),
                    failed.error,
                ));
            }
        };
        let active_binding = anchor.binding.clone();
        let authority_deadline = authorization.deadline();
        let executor = Self {
            authorization: ExecutionAuthorization::Recovery(Box::new(authorization)),
            anchor,
            stop,
            view,
            journal: Some(journal),
            quarantine: None,
            attempt: FirmwareAttemptId::parse(&run).expect("generated canonical recovery run"),
            origin: Instant::now(),
            receiver: None,
            io: None,
            raw_frontier: 0,
            last_raw_ns: 0,
            framer: CompletionFramer::new(),
            raw_pending: VecDeque::new(),
            frames: VecDeque::new(),
            generation: 1,
            epoch,
            offer_ordinal: 0,
            calls: 0,
            zero_call_teardown: false,
            evidence_teardown: false,
            last_receipt: None,
            journal_wait: None,
            pending_block: None,
            pending_syscall: None,
            raw_pending_intent: None,
            selection: None,
            normal_proof: None,
            target_version: None,
            original_epoch: None,
            editor_source_epoch: Some(editor_source_epoch),
            active_binding,
            last_offer_sequence: 0,
            last_offer_ns: 0,
            raw_fault: None,
            active_receipt: None,
            authority_deadline,
            #[cfg(test)]
            fixture,
        };
        executor.execute()
    }
    fn execute(mut self) -> Result<RunResult, ServiceError> {
        let result = self.run();
        let mut zero_return = false;
        let mut diagnostics = Vec::new();
        if let Err(error) = &result {
            diagnostics.push(error.message.clone());
        }
        if result.is_err() && self.calls == 0 {
            self.zero_call_teardown = true;
            match self.return_after_zero_calls() {
                Ok(()) => zero_return = true,
                Err(error) => diagnostics.push(format!("zero-call settlement: {}", error.message)),
            }
        }
        if result.is_err() {
            self.evidence_teardown = true;
            if !zero_return {
                self.set_phase(LinuxCompletionPhase::RecoveryRequired);
            }
            if let Err(error) = self.settle_evidence() {
                diagnostics.push(format!("original evidence settlement: {}", error.message));
            }
            // Never replace an unresolved/poisoned original command with a failed record.
            if self
                .journal
                .as_ref()
                .is_some_and(|journal| journal.healthy() && journal.pending_intent().is_none())
                && !self
                    .pending_block
                    .as_ref()
                    .is_some_and(|block| !matches!(block.submission, OutcomeSubmission::Staged))
                && let Err(error) = self.record(
                    serde_json::json!({"kind":"failed","error":diagnostics.first(),
                    "calls":self.calls,"raw_frontier":self.raw_frontier,"no_retry":true}),
                )
            {
                diagnostics.push(format!("failure evidence settlement: {}", error.message));
            }
        }
        if let Err(error) = self.close_io(false) {
            diagnostics.push(format!("actual I/O retirement: {}", error.message));
        }
        if let Some(block) = &self.pending_block {
            let retained = block.diagnostic();
            self.update(|view| view.retained_block_outcome = Some(retained));
        }
        if let Some(call) = &self.pending_syscall {
            diagnostics.push(format!(
                "retained native observation without durable acknowledgement: {}",
                call.record()
            ));
        }
        if !self.raw_pending.is_empty() {
            let raw = self.raw_pending.iter().cloned().collect();
            self.update(|view| view.retained_raw_chunks = raw);
            diagnostics.push(
                "Original raw evidence remains undurable; no retry or completion clearance".into(),
            );
        }
        if !diagnostics.is_empty() {
            self.update(|view| view.diagnostic = Some(diagnostics.join("; ")));
        }
        let journal_path = self
            .journal
            .as_ref()
            .map(|j| j.path().display().to_string())
            .unwrap_or_default();
        let journal = self
            .journal
            .take()
            .ok_or_else(|| bad("Original completion journal absent"))?;
        let store = journal.join()?;
        let view = self.view.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let allowed = !view.quarantined
            && view.normal_mode_verified
            && (view.target_version_verified || zero_return);
        let outcome = if self.authorization.is_recovery() {
            OwnedOutcome::Recovery(Box::new(crate::LinuxRecoveryOutcome {
                editor_return_allowed: allowed,
                selection: self.selection.clone(),
                editor_source_epoch: self.editor_source_epoch.expect("recovery GUI source epoch"),
                verified_normal_epoch: self.normal_proof.as_ref().map(|p| p.epoch),
                application_version: self.target_version.clone(),
                journal_path,
                view,
                history: self.authorization.history().cloned(),
            }))
        } else {
            OwnedOutcome::Normal(Box::new(LinuxCompletionOutcome {
                editor_return_allowed: !view.quarantined
                    && view.normal_mode_verified
                    && (view.target_version_verified || zero_return),
                selection: self.selection.clone(),
                original_normal_epoch: self.original_epoch.expect("normal completion epoch"),
                verified_normal_epoch: self.normal_proof.as_ref().map(|p| p.epoch),
                application_version: self.target_version.clone(),
                journal_path,
                view,
            }))
        };
        Ok(RunResult { store, outcome })
    }
    fn baseline_now(
        &self,
    ) -> Result<crate::firmware_entry_enumeration::EnumerationReceipt, ServiceError> {
        #[cfg(test)]
        if let Some(fixture) = &self.fixture {
            return fixture.baseline(&self.anchor.binding);
        }
        binding::baseline(&self.anchor.binding)
    }
    fn transition_now(
        &self,
        previous: &crate::firmware_entry_enumeration::EnumerationReceipt,
    ) -> Result<bool, ServiceError> {
        #[cfg(test)]
        if let Some(fixture) = &self.fixture {
            return fixture.transition(previous);
        }
        binding::observed_transition(previous)
    }
    fn accept_now(
        &mut self,
        generation: u64,
        normal_return_only: bool,
    ) -> Result<CompletionBindingWitness, ServiceError> {
        if normal_return_only {
            let original = self.authorization.original_normal_binding();
            #[cfg(test)]
            if let Some(fixture) = &self.fixture {
                return binding::accept_association_fixture(
                    &self.anchor.binding,
                    original,
                    generation,
                    self.anchor
                        .guard
                        .as_mut()
                        .ok_or_else(|| bad("Fixture original guard missing"))?,
                    &fixture.usb,
                    &fixture.sound,
                    fixture.endpoints.clone(),
                    true,
                );
            }
            return binding::accept_normal_return(
                &self.anchor.binding,
                original,
                generation,
                self.anchor
                    .guard
                    .as_mut()
                    .ok_or_else(|| bad("Original guard missing"))?,
            );
        }
        #[cfg(test)]
        if let Some(fixture) = &self.fixture {
            return fixture.accept(
                &self.anchor.binding,
                generation,
                self.anchor
                    .guard
                    .as_mut()
                    .ok_or_else(|| bad("Fixture original guard missing"))?,
            );
        }
        binding::accept(
            &self.anchor.binding,
            generation,
            self.anchor
                .guard
                .as_mut()
                .ok_or_else(|| bad("Original guard missing"))?,
        )
    }
    fn normal_reads_now(
        &self,
        witness: &CompletionBindingWitness,
        epoch: SessionEpoch,
        stop: &std::sync::atomic::AtomicBool,
    ) -> Result<CompletionNormalProof, ServiceError> {
        #[cfg(test)]
        if let Some(fixture) = &self.fixture {
            return fixture.normal_reads(witness, epoch, stop);
        }
        crate::raw_backend::completion_normal_reads(
            witness,
            &self.anchor.binding,
            self.anchor.guard.as_ref().expect("guard"),
            epoch,
            stop,
        )
    }
    fn registry_now(&self) -> Result<crate::firmware_quarantine::QuarantineRegistry, ServiceError> {
        #[cfg(test)]
        if let Some(fixture) = &self.fixture {
            return crate::firmware_quarantine::QuarantineRegistry::mock(&fixture.registry);
        }
        crate::firmware_quarantine::QuarantineRegistry::system()
    }
    fn return_after_zero_calls(&mut self) -> Result<(), ServiceError> {
        if self.calls != 0 {
            return Err(bad(
                "Zero-call return cannot reinterpret an attempted write",
            ));
        }
        let pending = self.journal.as_ref().expect("journal").pending_intent();
        if let Some(intent) = pending {
            match self.wait_journal(intent)? {
                JournalReply::Recorded(receipt) => self.last_receipt = Some(receipt),
                JournalReply::Checkpoint(_) => {}
            }
        }
        let retired = self
            .journal
            .as_ref()
            .expect("journal")
            .retire_zero_call_offer()?;
        self.record(serde_json::json!({"kind":"creating-owner-uninvoked-journal-retirement","calls":0,"settled_original_intent":pending,"retired_unused_checkpoint_intent":retired,"no_future_firmware_offer":true}))?;
        self.close_io(true)?;
        if let Some(quarantine) = &mut self.quarantine {
            quarantine.zero_offer()?;
        }
        if self.authorization.is_recovery() {
            self.set_phase(LinuxCompletionPhase::RecoveryRequired);
            return Ok(()); // Only the new zero-call reservation closed; predecessor holds remain.
        }
        // A zero-call editor return still requires the original normal association.
        let normal = self.accept_now(
            self.generation
                .checked_add(1)
                .ok_or_else(|| bad("Fresh generation exhausted"))?,
            true,
        )?;
        let epoch = SessionEpoch(
            self.epoch
                .0
                .checked_add(1)
                .ok_or_else(|| bad("Fresh zero-call normal epoch exhausted"))?,
        );
        let proof =
            self.normal_reads_now(&normal, epoch, &std::sync::atomic::AtomicBool::new(false))?;
        if !proof.reader_joined || !proof.output_destroyed || proof.allocated != proof.processed {
            return Err(bad("Zero-call normal refresh/close is unresolved"));
        }
        let closure = serde_json::json!({"schema":1,"kind":"creating-owner-zero-invocations","run":self.attempt,"source_review":self.authorization.digest(),"original_normal_epoch":self.original_epoch,"normal":proof,"calls":self.calls,"offered":false,"target_version_verified":false,"completion_verified":false,"device_modified_by_this_owner":false});
        if let Err(error) = self.record(closure.clone()) {
            let fallback = self
                .authorization
                .inputs()
                .run_directory
                .join("zero-invocations-return.json");
            publish(
                &fallback,
                &serde_json::json!({"closure":closure,"primary_journal_error":error.message}),
            )?;
        }
        self.selection = Some(normal.observation().selection.clone());
        self.normal_proof = Some(proof);
        self.update(|v| {
            v.normal_mode_verified = true;
            v.quarantined = false;
            v.phase = LinuxCompletionPhase::ReturningEditor;
        });
        Ok(())
    }
    fn update(&self, f: impl FnOnce(&mut LinuxCompletionView)) {
        f(&mut self.view.lock().unwrap_or_else(|p| p.into_inner()));
    }
    fn set_phase(&self, phase: LinuxCompletionPhase) {
        self.update(|v| v.phase = phase);
    }
    fn check_authority(&self) -> Result<(), ServiceError> {
        if Instant::now() >= self.authority_deadline {
            Err(bad(
                "Finite phase authority expired before this invocation; elapsed time grants no offer",
            ))
        } else {
            Ok(())
        }
    }
    fn check_stop(&self) -> Result<(), ServiceError> {
        if self.stop.requested.load(Ordering::SeqCst) {
            Err(ServiceError::new(
                ErrorKind::Cancelled,
                "Completion host stop requested; no reset/rollback or retry",
            ))
        } else {
            Ok(())
        }
    }
    fn millis(&self) -> Result<u64, ServiceError> {
        self.receiver.as_ref().map_or_else(
            || u64::try_from(self.origin.elapsed().as_millis()).map_err(bad),
            |receiver| receiver.sink.now().map(|(_, ns)| ns / 1_000_000),
        )
    }
    fn wait_journal(&mut self, intent: u64) -> Result<JournalReply, ServiceError> {
        let mode = if self.zero_call_teardown || self.evidence_teardown {
            JournalWaitMode::EvidenceSettlement
        } else {
            JournalWaitMode::Operation
        };
        self.wait_journal_mode(intent, mode)
    }
    fn wait_journal_mode(
        &mut self,
        intent: u64,
        mode: JournalWaitMode,
    ) -> Result<JournalReply, ServiceError> {
        // Cancellation changes the wait mode, never the original absolute budget.
        let budget = Duration::from_secs(15);
        #[cfg(test)]
        let budget = if self.pending_block.is_some()
            && self.fixture.as_ref().is_some_and(|fixture| {
                matches!(
                    fixture.fault,
                    tests::FixtureFault::BlockStopStall | tests::FixtureFault::BlockLostReply
                )
            }) {
            Duration::from_millis(50)
        } else {
            budget
        };
        let pending = self.journal_wait.get_or_insert_with(|| PendingJournalWait {
            intent,
            deadline: Instant::now() + budget,
        });
        if pending.intent != intent {
            return Err(bad(
                "Completion journal wait would replace an original intent",
            ));
        }
        let deadline = pending.deadline;
        #[cfg(test)]
        if let Some(fixture) = &self.fixture
            && matches!(
                fixture.fault,
                tests::FixtureFault::BlockStopStall | tests::FixtureFault::BlockLostReply
            )
        {
            fixture
                .trace
                .lock()
                .unwrap()
                .journal_waits
                .push((intent, deadline));
        }
        let mut received = None;
        loop {
            let _ = self.collect_raw();
            if Instant::now() >= deadline {
                return Err(bad(
                    "Original durable journal operation exceeded host budget; join/ownership remains required",
                ));
            }
            if let Some(event) = received
                .take()
                .or_else(|| self.journal.as_ref().expect("journal").poll())
            {
                if event.intent != intent {
                    return Err(bad(
                        "Completion journal result belongs to a different intent",
                    ));
                }
                self.journal_wait.take();
                return event.result;
            }
            if let Err(error) = self.check_stop() {
                #[cfg(test)]
                self.journal
                    .as_ref()
                    .expect("journal")
                    .test_release_cancelled_reply();
                if mode == JournalWaitMode::Operation {
                    return Err(error);
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(bad(
                    "Original durable journal operation exceeded host budget; join/ownership remains required",
                ));
            }
            received = self
                .journal
                .as_ref()
                .expect("journal")
                .wait_event((deadline - now).min(Duration::from_millis(1)));
        }
    }
    fn record(&mut self, value: serde_json::Value) -> Result<(), ServiceError> {
        let intent = self.journal.as_mut().expect("journal").record(value)?;
        self.finish_record(intent, JournalWaitMode::Operation)
    }
    fn finish_record(&mut self, intent: u64, mode: JournalWaitMode) -> Result<(), ServiceError> {
        let mode = if self.zero_call_teardown || self.evidence_teardown {
            JournalWaitMode::EvidenceSettlement
        } else {
            mode
        };
        match self.wait_journal_mode(intent, mode)? {
            JournalReply::Recorded(receipt) => {
                self.last_receipt = Some(receipt);
                Ok(())
            }
            JournalReply::Checkpoint(_) => Err(bad("Unexpected journal checkpoint")),
        }
    }
    fn settle_original_journal(&mut self) -> Result<(), ServiceError> {
        let Some(intent) = self.journal.as_ref().expect("journal").pending_intent() else {
            return Ok(());
        };
        if !self.journal.as_ref().expect("journal").healthy() {
            return Err(bad(format!(
                "Original journal intent {intent} is poisoned and unresolved; original join remains required"
            )));
        }
        match self.wait_journal_mode(intent, JournalWaitMode::EvidenceSettlement)? {
            JournalReply::Recorded(receipt) => self.last_receipt = Some(receipt),
            JournalReply::Checkpoint(_) => {}
        }
        if self
            .pending_syscall
            .as_ref()
            .is_some_and(|call| call.intent == Some(intent))
        {
            self.pending_syscall.take();
        }
        if self.raw_pending_intent == Some(intent) {
            self.raw_pending.pop_front();
            self.raw_pending_intent = None;
        }
        Ok(())
    }
    fn settle_evidence(&mut self) -> Result<(), ServiceError> {
        if let Some(block) = &self.pending_block
            && let OutcomeSubmission::Failed(error) = &block.submission
        {
            return Err(error.clone());
        }
        if self
            .pending_block
            .as_ref()
            .is_some_and(|block| matches!(block.submission, OutcomeSubmission::Submitted(_)))
        {
            self.flush_block_outcome()?;
        } else {
            self.settle_original_journal()?;
        }
        self.persist_syscall(JournalWaitMode::EvidenceSettlement)?;
        let collected = self.collect_raw();
        self.persist_action_raw()?;
        self.flush_block_outcome()?;
        collected
    }
    fn retain_syscall(&mut self, kind: &'static str, call: CompletionSyscall) -> CompletionSyscall {
        // Store the native return BEFORE clocks, serialization, health, or engine work.
        let retained = RetainedSyscall {
            kind,
            ordinal: self.offer_ordinal,
            observation: call.clone(),
            delivery_at_ms: None,
            intent: None,
        };
        if let Some(block) = &mut self.pending_block
            && (kind == "write-syscall" || !block.immediate_drain)
        {
            block.syscalls.push(retained);
        } else {
            self.pending_syscall = Some(retained);
        }
        call
    }
    fn deliver_syscall(&mut self) -> Result<(), ServiceError> {
        let delivery = self.millis()?;
        if self.pending_syscall.is_none()
            && let Some(block) = &mut self.pending_block
        {
            block
                .syscalls
                .last_mut()
                .expect("retained native call")
                .delivery_at_ms = Some(delivery);
        } else {
            self.pending_syscall
                .as_mut()
                .expect("retained native call")
                .delivery_at_ms = Some(delivery);
            self.persist_syscall(JournalWaitMode::Operation)?;
        }
        Ok(())
    }
    fn persist_syscall(&mut self, mode: JournalWaitMode) -> Result<(), ServiceError> {
        let Some(call) = &self.pending_syscall else {
            return Ok(());
        };
        let intent = if let Some(intent) = call.intent {
            intent
        } else {
            let value = call.record();
            let intent = self.journal.as_mut().expect("journal").record(value)?;
            self.pending_syscall.as_mut().expect("retained call").intent = Some(intent);
            intent
        };
        self.finish_record(intent, mode)?;
        self.pending_syscall.take();
        Ok(())
    }
    fn flush_block_outcome(&mut self) -> Result<(), ServiceError> {
        let Some(block) = &self.pending_block else {
            return Ok(());
        };
        let intent =
            match &block.submission {
                OutcomeSubmission::Failed(error) => return Err(error.clone()),
                OutcomeSubmission::Submitted(intent) => *intent,
                OutcomeSubmission::Staged => {
                    let values = block.records();
                    if values.is_empty() {
                        return Ok(());
                    }
                    #[cfg(test)]
                    if self.fixture.as_ref().is_some_and(|fixture| {
                        fixture.fault == tests::FixtureFault::BlockBatchSubmit
                    }) && values
                        .iter()
                        .any(|value| value["kind"] == "vendor-action-pair")
                    {
                        self.journal.as_ref().expect("journal").stop();
                    }
                    match self.journal.as_mut().expect("journal").records(values) {
                        Ok(intent) => {
                            self.pending_block
                                .as_mut()
                                .expect("pending block")
                                .submission = OutcomeSubmission::Submitted(intent);
                            self.pending_block
                                .as_mut()
                                .expect("pending block")
                                .submitted_intent = Some(intent);
                            intent
                        }
                        Err(error) => {
                            self.pending_block
                                .as_mut()
                                .expect("pending block")
                                .submission = OutcomeSubmission::Failed(error.clone());
                            return Err(error);
                        }
                    }
                }
            };
        self.acknowledge_block_submission(intent, JournalWaitMode::EvidenceSettlement)?;
        Ok(())
    }
    fn acknowledge_block_submission(
        &mut self,
        intent: u64,
        mode: JournalWaitMode,
    ) -> Result<Option<super::firmware_linux_completion_journal::CompletionCheckpoint>, ServiceError>
    {
        let combined = self
            .pending_block
            .as_ref()
            .expect("pending block")
            .combined_checkpoint
            .is_some();
        let result = self
            .wait_journal_mode(intent, mode)
            .and_then(|reply| match reply {
                JournalReply::Recorded(receipt) if !combined => {
                    self.last_receipt = Some(receipt);
                    Ok(None)
                }
                JournalReply::Checkpoint(mut checkpoint) if combined => {
                    let (ordinal, _, wire) = self
                        .pending_block
                        .as_ref()
                        .expect("pending block")
                        .combined_checkpoint
                        .as_ref()
                        .expect("original combined checkpoint");
                    if !checkpoint.matches_original(*ordinal, wire) {
                        return Err(bad(
                            "Combined checkpoint differs from its original outcome intent",
                        ));
                    }
                    self.last_receipt =
                        Some(checkpoint.outcome_receipt.take().ok_or_else(|| {
                            bad("Combined checkpoint lacks its actual durable outcome receipt")
                        })?);
                    Ok(Some(checkpoint))
                }
                _ => Err(bad("Original Block journal acknowledgement kind differs")),
            });
        match result {
            Ok(checkpoint) => {
                let block = self.pending_block.as_mut().expect("pending block");
                block.acknowledged_prefix =
                    self.last_receipt.as_ref().map(|receipt| receipt.sequence);
                block.syscalls.clear();
                block.raw.clear();
                block.pair = None;
                block.submission = OutcomeSubmission::Staged;
                block.submitted_intent = None;
                block.combined_checkpoint = None;
                if let Some(receipt) = block.completed.take() {
                    self.update(|view| {
                        view.host_accepted_blocks = receipt
                            .action
                            .block()
                            .expect("completed Block source")
                            .absolute_block_ordinal
                            + 1
                    });
                    #[cfg(test)]
                    if let Some(fixture) = &self.fixture {
                        fixture.trace.lock().unwrap().pair_retirements += 1;
                    }
                    self.pending_block.take();
                }
                Ok(checkpoint)
            }
            Err(error) => {
                // Keep the original combined specification and records even if
                // its frames reached disk: absence of a live ack grants no offer.
                if !self.journal.as_ref().expect("journal").healthy()
                    || self.journal.as_ref().expect("journal").pending_intent() != Some(intent)
                {
                    self.pending_block
                        .as_mut()
                        .expect("pending block")
                        .submission = OutcomeSubmission::Failed(error.clone());
                }
                Err(error)
            }
        }
    }
    fn can_fuse_block(&self, next: &super::firmware_vendor_sequence::VendorAction) -> bool {
        self.pending_block.as_ref().is_some_and(|block| {
            let Some(previous) = &block.completed else {
                return false;
            };
            previous.action.phase() == VendorPhase::Block
                && next.phase() == VendorPhase::Block
                && previous.action.section().is_some()
                && previous.action.section() == next.section()
                // Journal/native ordinals include optional entry offers; engine
                // ordinals begin with its fresh identity. Bind each frontier in
                // its own domain instead of equating those different counters.
                && block.ordinal.checked_add(1) == Some(self.offer_ordinal)
                && previous.action.ordinal().checked_add(1) == Some(next.ordinal())
                && previous
                    .action
                    .block()
                    .zip(next.block())
                    .is_some_and(|(previous, next)| {
                        previous.block_in_section.checked_add(1) == Some(next.block_in_section)
                            && previous.absolute_block_ordinal.checked_add(1)
                                == Some(next.absolute_block_ordinal)
                    })
                && matches!(block.submission, OutcomeSubmission::Staged)
                && block.pair.is_some()
                && !block.immediate_raw
                && !block.immediate_drain
                && block.records().len() < MAX_BATCH_RECORDS
                && block
                    .records()
                    .iter()
                    .chain(std::iter::once(&serde_json::json!({
                    "kind":"checkpoint","ordinal":self.offer_ordinal,
                    "purpose":format!("vendor {:?}",next.phase()),"wire":next.wire(),
                    "wire_sha256":JobSha256::of(next.wire()).hex()})))
                    .try_fold(0usize, |bytes, value| {
                        serde_json::to_vec(value)
                            .ok()
                            .and_then(|body| bytes.checked_add(44 + body.len()))
                    })
                    .is_some_and(|bytes| bytes <= MAX_BATCH_BYTES)
        })
    }
    fn fused_block_checkpoint(
        &mut self,
        wire: &[u8],
        purpose: &str,
    ) -> Result<super::firmware_linux_completion_journal::CompletionCheckpoint, ServiceError> {
        let block = self.pending_block.as_ref().expect("completed Block");
        if !matches!(block.submission, OutcomeSubmission::Staged) || block.completed.is_none() {
            return Err(bad(
                "Combined checkpoint cannot replace an unresolved outcome",
            ));
        }
        let values = block.records();
        #[cfg(test)]
        if self
            .fixture
            .as_ref()
            .is_some_and(|fixture| fixture.fault == tests::FixtureFault::BlockBatchSubmit)
        {
            self.journal.as_ref().expect("journal").stop();
        }
        // Retain the exact proposed checkpoint before queue submission. A failed
        // submission or lost reply cannot be replaced with standalone records.
        self.pending_block
            .as_mut()
            .expect("completed Block")
            .combined_checkpoint = Some((self.offer_ordinal, purpose.into(), wire.to_vec()));
        let intent = match self.journal.as_mut().expect("journal").outcome_checkpoint(
            values,
            self.offer_ordinal,
            purpose.into(),
            wire.to_vec(),
        ) {
            Ok(intent) => intent,
            Err(error) => {
                self.pending_block
                    .as_mut()
                    .expect("completed Block")
                    .submission = OutcomeSubmission::Failed(error.clone());
                return Err(error);
            }
        };
        let block = self.pending_block.as_mut().expect("completed Block");
        block.submission = OutcomeSubmission::Submitted(intent);
        block.submitted_intent = Some(intent);
        self.acknowledge_block_submission(intent, JournalWaitMode::Operation)?
            .ok_or_else(|| bad("Combined outcome issued no consuming checkpoint"))
    }
    fn persist_action_raw(&mut self) -> Result<(), ServiceError> {
        if !self.raw_pending.is_empty()
            && self
                .pending_block
                .as_ref()
                .is_some_and(|block| block.completed.is_some())
        {
            self.flush_block_outcome()?;
        }
        while let Some(raw) = self.raw_pending.front() {
            let Some(block) = &self.pending_block else {
                return self.persist_raw();
            };
            if block.immediate_raw {
                return self.persist_raw();
            }
            if !matches!(block.submission, OutcomeSubmission::Staged) {
                return Err(bad(
                    "Original Block outcome is unresolved; raw cannot replace it",
                ));
            }
            let value = serde_json::json!({"kind":"raw-chunk","observation":raw});
            let mut values = block.records();
            values.push(value.clone());
            // Reserve one ordinary pair record; every raw stays queued until admitted.
            let bytes = values.iter().try_fold(0usize, |bytes, value| {
                let length = serde_json::to_vec(value).map_err(bad)?.len();
                bytes
                    .checked_add(44)
                    .and_then(|bytes| bytes.checked_add(length))
                    .ok_or_else(|| bad("Block outcome aggregate length overflow"))
            })?;
            if values.len() >= MAX_BATCH_RECORDS || bytes > MAX_BATCH_BYTES - 16 * 1024 {
                self.flush_block_outcome()?;
                self.pending_block.as_mut().expect("block").immediate_raw = true;
                return self.persist_raw();
            }
            self.pending_block.as_mut().expect("block").raw.push(value);
            self.raw_pending.pop_front();
        }
        Ok(())
    }
    fn checkpoint(
        &mut self,
        wire: &[u8],
        purpose: &str,
    ) -> Result<super::firmware_linux_completion_journal::CompletionCheckpoint, ServiceError> {
        let intent = self.journal.as_mut().expect("journal").checkpoint(
            self.offer_ordinal,
            purpose.into(),
            wire.to_vec(),
        )?;
        match self.wait_journal(intent)? {
            JournalReply::Checkpoint(c) => Ok(c),
            JournalReply::Recorded(_) => Err(bad("Missing consuming durable checkpoint")),
        }
    }
    fn receiver_plan(&self, binding: FirmwareUsbObservation) -> FirmwareLifecyclePlanDto {
        FirmwareLifecyclePlanDto {
            review_sha256: self.authorization.job_digest(),
            profile_sha256: VENDOR_CONTRACT_SHA256.into(),
            origin: FirmwareObservationOrigin::LocalObservation,
            nonblock_actions: 20,
            epoch: self.epoch,
            binding,
            generation: self.generation,
            clock_origin: format!(
                "linux-completion-{}-{}",
                self.attempt.as_str(),
                self.generation
            ),
        }
    }
    fn retain_open(
        &mut self,
        result: Result<
            Box<dyn CompletionIo>,
            super::firmware_linux_completion_io::CompletionOpenFailure,
        >,
    ) -> Result<(), ServiceError> {
        match result {
            Ok(io) => {
                self.io = Some(io);
                Ok(())
            }
            Err(failed) => {
                self.io = failed.io;
                let _=self.record(serde_json::json!({"kind":"native-open-failed-before-invocation","calls":self.calls,"error":failed.error.message,"partial_reader_retained":self.io.is_some()}));
                Err(failed.error)
            }
        }
    }
    fn open_entry(&mut self) -> Result<(), ServiceError> {
        self.active_receipt = Some(self.baseline_now()?);
        let plan = self.receiver_plan(self.anchor.binding.clone());
        let receiver = FirmwareReceiver::new(&plan);
        let guard = self
            .anchor
            .guard
            .as_ref()
            .ok_or_else(|| bad("Completion actual physical guard absent"))?;
        #[cfg(test)]
        let mock = self
            .fixture
            .as_ref()
            .map(|fixture| fixture.open(receiver.sink.clone(), false));
        #[cfg(test)]
        let result = match mock {
            Some(result) => result,
            None => CompletionRawIo::open_entry(&self.anchor.binding, guard, receiver.sink.clone()),
        };
        #[cfg(not(test))]
        let result =
            CompletionRawIo::open_entry(&self.anchor.binding, guard, receiver.sink.clone());
        self.receiver = Some(receiver);
        self.retain_open(result)
    }
    fn open_boot(&mut self, witness: &CompletionBindingWitness) -> Result<(), ServiceError> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| bad("Completion generation exhausted"))?;
        self.epoch = SessionEpoch(
            self.epoch
                .0
                .checked_add(1)
                .ok_or_else(|| bad("Completion epoch exhausted"))?,
        );
        self.active_binding = witness.observation().binding.clone();
        self.active_receipt = Some(witness.observation().usb.clone());
        let receiver = FirmwareReceiver::new(&self.receiver_plan(self.active_binding.clone()));
        #[cfg(test)]
        let mock = self
            .fixture
            .as_ref()
            .map(|fixture| fixture.open(receiver.sink.clone(), true));
        #[cfg(test)]
        let result = match mock {
            Some(result) => result,
            None => CompletionRawIo::open_fresh(
                witness,
                &self.anchor.binding,
                self.anchor.guard.as_ref().expect("guard"),
                receiver.sink.clone(),
            ),
        };
        #[cfg(not(test))]
        let result = CompletionRawIo::open_fresh(
            witness,
            &self.anchor.binding,
            self.anchor.guard.as_ref().expect("guard"),
            receiver.sink.clone(),
        );
        self.receiver = Some(receiver);
        self.raw_frontier = 0;
        self.last_raw_ns = 0;
        self.framer = CompletionFramer::new();
        self.retain_open(result)
    }
    fn collect_raw(&mut self) -> Result<(), ServiceError> {
        if let Some(error) = &self.raw_fault {
            return Err(error.clone());
        }
        let Some(receiver) = &self.receiver else {
            return Ok(());
        };
        for _ in 0..64 {
            if self.raw_pending.len() >= 64 {
                if receiver.received.is_empty() {
                    break;
                }
                let error = bad("Completion raw journal backlog exceeded finite capacity");
                self.raw_fault = Some(error.clone());
                return Err(error);
            }
            let Ok(raw) = receiver.received.try_recv() else {
                break;
            };
            #[cfg(test)]
            let raw = if self.pending_block.is_some()
                && self
                    .fixture
                    .as_ref()
                    .is_some_and(|fixture| fixture.fault == tests::FixtureFault::BlockStale)
            {
                let mut raw = raw;
                raw.generation -= 1;
                raw
            } else {
                raw
            };
            self.update(|v| {
                if v.retained_raw_chunks.len() < 64 {
                    v.retained_raw_chunks.push(raw.clone());
                }
                v.allocated_raw_frontier = receiver.sink.allocated.load(Ordering::SeqCst);
            });
            self.raw_pending.push_back(raw.clone());
            if raw.sequence
                != self
                    .raw_frontier
                    .checked_add(1)
                    .ok_or_else(|| bad("Raw sequence exhausted"))?
                || raw.at_ns < self.last_raw_ns
                || raw.epoch != self.epoch
                || raw.generation != self.generation
                || raw.binding_sha256 != self.active_binding.digest()
                || raw.clock_origin
                    != format!(
                        "linux-completion-{}-{}",
                        self.attempt.as_str(),
                        self.generation
                    )
            {
                let error = bad("Completion original raw source/frontier/time differs");
                self.raw_fault = Some(error.clone());
                return Err(error);
            }
            self.raw_frontier = raw.sequence;
            self.last_raw_ns = raw.at_ns;
            // Entry observations are raw evidence only; framing begins at the fresh boot reader.
            if self.generation > 1 {
                match self.framer.feed(&raw) {
                    Ok(frames) => self.frames.extend(frames),
                    Err(error) => {
                        self.raw_fault = Some(error.clone());
                        return Err(error);
                    }
                }
            }
        }
        if receiver.sink.fault.load(Ordering::SeqCst) {
            let error = bad("Completion reader publication/fault left an unexplained raw frontier");
            self.raw_fault = Some(error.clone());
            return Err(error);
        }
        Ok(())
    }
    fn collect_retired_raw(&mut self) {
        let Some(receiver) = &self.receiver else {
            return;
        };
        // The actual reader is already joined. Retain available originals even
        // after a sticky validation fault, without advancing the processed frontier.
        while self.raw_pending.len() < 64 {
            let Ok(raw) = receiver.received.try_recv() else {
                break;
            };
            self.raw_pending.push_back(raw);
        }
    }
    fn persist_raw(&mut self) -> Result<(), ServiceError> {
        self.persist_raw_mode(JournalWaitMode::Operation)
    }
    fn persist_raw_mode(&mut self, mode: JournalWaitMode) -> Result<(), ServiceError> {
        while let Some(raw) = self.raw_pending.front() {
            let intent = if let Some(intent) = self.raw_pending_intent {
                intent
            } else {
                let value = serde_json::json!({"kind":"raw-chunk","observation":raw});
                let intent = self.journal.as_mut().expect("journal").record(value)?;
                self.raw_pending_intent = Some(intent);
                intent
            };
            self.finish_record(intent, mode)?;
            // Submission alone does not transfer or discard our original observation.
            self.raw_pending.pop_front();
            self.raw_pending_intent = None;
        }
        Ok(())
    }
    fn expected_disconnect_now(&self) -> bool {
        let Some(receiver) = &self.receiver else {
            return false;
        };
        let fault = self.io.as_ref().and_then(|io| io.read_fault());
        let transition = self
            .active_receipt
            .as_ref()
            .is_some_and(|previous| self.transition_now(previous) == Ok(true));
        !self.framer.pending()
            && expected_transition_fault(
                fault.as_ref(),
                self.raw_frontier,
                receiver.sink.allocated.load(Ordering::SeqCst),
                receiver.sink.first_failure().is_none(),
                self.raw_fault.as_ref(),
                transition,
            )
    }
    fn close_io(&mut self, required: bool) -> Result<(), ServiceError> {
        let Some(mut io) = self.io.take() else {
            return Ok(());
        };
        // close() joins the original reader and destroys the original output.
        // No journal failure can skip retaining or accounting this actual return.
        let mut receipt = io.close();
        let collect = self.collect_raw();
        if collect.is_err() {
            self.collect_retired_raw();
        }
        let persisted = self
            .persist_action_raw()
            .and_then(|()| self.flush_block_outcome());
        let receiver = self.receiver.as_ref().expect("reader sink");
        let fault = io.read_fault();
        let passive_transition = self
            .active_receipt
            .as_ref()
            .is_some_and(|previous| self.transition_now(previous) == Ok(true));
        let expected_fault = expected_transition_fault(
            fault.as_ref(),
            self.raw_frontier,
            receiver.sink.allocated.load(Ordering::SeqCst),
            receiver.sink.first_failure().is_none(),
            self.raw_fault.as_ref(),
            passive_transition,
        );
        let accounted_frontier = if expected_fault {
            self.raw_frontier
                .checked_add(1)
                .unwrap_or(self.raw_frontier)
        } else {
            self.raw_frontier
        };
        let accounted = receipt.accounts(&receiver.sink, accounted_frontier);
        let joined = receipt.observation().reader_join
            == FirmwareNormalReaderJoinObservation::Returned
            && receipt.output_destroyed;
        let valid = accounted
            && joined
            && (receipt.is_fresh() || expected_fault)
            && !self.framer.pending()
            && self.frames.is_empty()
            && (collect.is_ok() || expected_fault);
        let retirement = serde_json::json!({"kind":"firmware-reader-retired","source":receipt.observation(),
            "fault":receipt.fault.as_ref().map(|e|&e.message),"output_destroyed":receipt.output_destroyed,
            "accounted":accounted,"required":required,"original_read_fault":fault,
            "passive_transition":passive_transition,"expected_single_fault_accounted":expected_fault,
            "valid":valid,"first_publication_failure":receiver.sink.first_failure(),"raw_persistence_error":persisted.as_ref().err().map(|error|&error.message)});
        // The in-memory retirement survives an unavailable or uncertain journal.
        self.update(|view| {
            view.reader_retirement_diagnostics.push(retirement.clone());
            view.allocated_raw_frontier = receiver.sink.allocated.load(Ordering::SeqCst);
        });
        self.receiver.take();
        self.raw_fault = None;
        let recorded = if persisted.is_ok() {
            self.record(retirement)
        } else {
            Err(bad(
                "Reader retirement remains undurable after original evidence settlement failure",
            ))
        };
        let mut errors = Vec::new();
        if let Err(error) = persisted {
            errors.push(format!("raw/outcome settlement: {}", error.message));
        }
        if let Err(error) = recorded {
            errors.push(format!("reader retirement settlement: {}", error.message));
        }
        if let Err(error) = collect
            && !expected_fault
        {
            errors.push(format!("reader collection: {}", error.message));
        }
        if !valid {
            errors.push("Actual firmware source close/join/frontier is unresolved".into());
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(bad(errors.join("; ")))
        }
    }
    fn offer(
        &mut self,
        wire: &[u8],
        purpose: &str,
        policy: RecordingPolicy,
        before: impl FnOnce(u64) -> Result<(), ServiceError>,
    ) -> Result<VendorWriteReceipt, ServiceError> {
        if self.zero_call_teardown || self.evidence_teardown {
            return Err(bad(
                "Terminal zero-call cleanup cannot offer firmware bytes",
            ));
        }
        if self.pending_block.as_ref().is_some_and(|block| {
            block.completed.is_none()
                || !matches!(block.submission, OutcomeSubmission::Staged)
                || policy != RecordingPolicy::BlockOutcome
        }) || self.pending_syscall.is_some()
            || self.journal_wait.is_some()
        {
            return Err(bad("Previous original outcome is unresolved before offer"));
        }
        self.check_stop()?;
        self.collect_raw()?;
        if self.generation > 1 && (!self.frames.is_empty() || self.framer.pending()) {
            return Err(bad(
                "Queued traffic before completion offer cannot become its reply",
            ));
        }
        self.persist_raw()?;
        let checkpoint = if self.pending_block.is_some() {
            self.fused_block_checkpoint(wire, purpose)?
        } else {
            self.checkpoint(wire, purpose)?
        };
        self.check_stop()?;
        let start = self.millis()?;
        if policy == RecordingPolicy::BlockOutcome {
            self.pending_block = Some(PendingBlockOutcome::new(self.offer_ordinal));
            #[cfg(test)]
            self.test_prepare_block_outcome();
        }
        let stop = self.stop.clone();
        let call = {
            let _stop = stop
                .gate
                .lock()
                .map_err(|_| bad("Completion stop fence poisoned"))?;
            let _fence = self.journal.as_ref().expect("journal").offer_fence(
                checkpoint,
                self.offer_ordinal,
                wire,
            )?;
            self.check_stop()?;
            self.check_authority()?;
            self.io.as_mut().expect("open writer").healthy()?;
            let receiver = self.receiver.as_ref().expect("actual original receiver");
            if receiver.sink.fault.load(Ordering::SeqCst)
                || receiver.sink.allocated.load(Ordering::SeqCst) != self.raw_frontier
                || receiver.sink.first_failure().is_some()
                || !self.raw_pending.is_empty()
                || !self.frames.is_empty()
                || self.framer.pending()
            {
                return Err(bad(
                    "Actual receive frontier changed after the durable offer checkpoint",
                ));
            }
            before(self.millis()?)?;
            self.check_stop()?;
            self.check_authority()?;
            let invocation = super::firmware_linux_completion_io::CompletionInvocation {
                deadline: self.authority_deadline,
                stop: &stop.requested,
                processed: self.raw_frontier,
            };
            let calls = &mut self.calls;
            let quarantine = self.quarantine.as_mut().expect("risk before offer");
            self.io
                .as_mut()
                .expect("open writer")
                .write_once(wire, &invocation, &mut || {
                    *calls = calls
                        .checked_add(1)
                        .ok_or_else(|| bad("Completion syscall count exhausted"))?;
                    quarantine.offered();
                    Ok(())
                })?
        };
        let call = self.retain_syscall("write-syscall", call);
        self.last_offer_sequence = call.before_sequence;
        self.last_offer_ns = call.before_ns;
        self.update(|v| {
            v.os_offered_bytes = v
                .os_offered_bytes
                .saturating_add(call.returned_count.unwrap_or(0) as u64);
            v.host_write_invocations = self.calls;
        });
        self.deliver_syscall()?;
        let mut receipt = VendorWriteReceipt {
            at_ms: super::firmware_linux_completion_io::syscall_completion_ms(&call)?,
            extent: call
                .returned_count
                .map_or(VendorWriteExtent::Unknown, VendorWriteExtent::Known),
            drain: VendorDrain::NotAttempted,
        };
        if call.errno.is_some() || call.returned_count != Some(wire.len()) {
            return Ok(receipt);
        }
        loop {
            self.check_stop()?;
            self.io.as_mut().expect("writer").healthy()?;
            if self
                .pending_block
                .as_ref()
                .is_some_and(|block| !block.immediate_drain && block.syscalls.len() >= 2)
            {
                return Err(bad("Block native outcome capacity exceeded before drain"));
            }
            let call = self.io.as_mut().expect("writer").drain_once()?;
            let call = self.retain_syscall("drain-syscall", call);
            self.deliver_syscall()?;
            receipt.at_ms = super::firmware_linux_completion_io::syscall_completion_ms(&call)?;
            if call.errno.is_none() && call.returned_count == Some(0) {
                receipt.drain = VendorDrain::Returned;
                break;
            }
            if call.errno != Some(11)
                || receipt.at_ms.saturating_sub(start) >= VENDOR_ACTION_BUDGET_MS
            {
                receipt.drain = VendorDrain::Failed;
                break;
            }
            if self.pending_block.is_some() {
                self.flush_block_outcome()?;
                // All later EAGAIN observations retain the old independently
                // durable drain path, with this original action clock unchanged.
                self.pending_block.as_mut().expect("block").immediate_drain = true;
            }
            thread::sleep(Duration::from_millis(1));
        }
        self.offer_ordinal = self
            .offer_ordinal
            .checked_add(1)
            .ok_or_else(|| bad("Completion action frontier exhausted"))?;
        Ok(receipt)
    }
    #[cfg(test)]
    fn test_prepare_block_outcome(&self) {
        use super::firmware_linux_completion_journal::TestBatchFault;
        let Some(fixture) = &self.fixture else {
            return;
        };
        let journal = self.journal.as_ref().expect("journal");
        match fixture.fault {
            tests::FixtureFault::BlockBatchSync
            | tests::FixtureFault::BlockCloseQueuedJournalFailure => {
                journal.test_fail_next_batch(TestBatchFault::Sync)
            }
            tests::FixtureFault::BlockBatchReread => {
                journal.test_fail_next_batch(TestBatchFault::Reread)
            }
            tests::FixtureFault::BlockWrongIntent => {
                journal.test_fail_next_batch(TestBatchFault::WrongIntent)
            }
            tests::FixtureFault::BlockRevoked => {
                journal.test_fail_next_batch(TestBatchFault::Revoked)
            }
            tests::FixtureFault::BlockStopBatch => {
                journal.test_stop_before_reply(true, self.stop.clone())
            }
            tests::FixtureFault::BlockLostReply => {
                journal.test_fail_next_batch(TestBatchFault::LostReply)
            }
            tests::FixtureFault::BlockStopBeforeFusionCommit => {
                let stop = self.stop.clone();
                let view = self.view.clone();
                let trace = fixture.trace.clone();
                journal.test_before_next_batch_sync(Box::new(move || {
                    trace.lock().unwrap().before_fusion_sync_accepted =
                        Some(view.lock().unwrap().host_accepted_blocks);
                    stop.requested.store(true, Ordering::SeqCst);
                }));
            }
            tests::FixtureFault::BlockStopBeforeFusionSubmit => {}
            tests::FixtureFault::BlockStopStall => {
                journal.test_stall_before_reply(self.stop.clone(), Duration::from_millis(150))
            }
            _ => {}
        }
        if matches!(
            fixture.fault,
            tests::FixtureFault::BlockRawDuringSync
                | tests::FixtureFault::BlockCloseQueuedJournalFailure
        ) {
            let sink = self.receiver.as_ref().expect("original sink").sink.clone();
            journal.test_before_next_batch_sync(Box::new(move || {
                sink.receive(&[240, 126, 72, 3, 127, 247])
            }));
        }
    }
    fn phase_review(
        &mut self,
        name: &str,
        value: serde_json::Value,
    ) -> Result<String, ServiceError> {
        let path = self
            .authorization
            .inputs()
            .run_directory
            .join(format!("{name}-review.json"));
        let digest = publish(&path, &value)?;
        self.record(serde_json::json!({"kind":"phase-review","phase":name,"digest":digest,"path":path,"review":value}))?;
        self.update(|v| {
            v.review_digest = Some(digest.clone());
            v.review = Some(value);
        });
        Ok(digest)
    }
    fn wait_signal(
        &mut self,
        name: &str,
        expected: &str,
        timeout: u64,
    ) -> Result<(), ServiceError> {
        let deadline = Instant::now() + Duration::from_secs(timeout);
        #[cfg(test)]
        let deadline = if self
            .fixture
            .as_ref()
            .is_some_and(|f| f.phase_wait_hook.is_some())
        {
            // Private boundary tests must first reach real source hashing and
            // durable journal work while fresh, then cross this same finite
            // original clock. This never changes a production input timeout.
            Instant::now() + Duration::from_secs(30)
        } else {
            deadline
        };
        // This phase retains its original finite wait budget through signal
        // inspection, source rereads and the actual durable journal reply.
        self.authority_deadline = deadline;
        loop {
            self.check_stop()?;
            #[cfg(test)]
            self.test_phase_wait_boundary(tests::PhaseWaitBoundary::BeforeRead, deadline);
            check_phase_wait_deadline(deadline)?;
            self.collect_raw()?;
            check_phase_wait_deadline(deadline)?;
            let approved = self.authorization.read_signal(name, expected)?;
            check_phase_wait_deadline(deadline)?;
            if approved {
                self.authorization.revalidate()?;
                #[cfg(test)]
                self.test_phase_wait_boundary(
                    tests::PhaseWaitBoundary::AfterRevalidation,
                    deadline,
                );
                check_phase_wait_deadline(deadline)?;
                self.record(serde_json::json!({"kind":"exact-phase-signal","name":name,"expected":expected}))?;
                #[cfg(test)]
                self.test_phase_wait_boundary(tests::PhaseWaitBoundary::AfterJournal, deadline);
                check_phase_wait_deadline(deadline)?;
                self.check_stop()?;
                let granted_at = Instant::now();
                check_phase_wait_deadline_at(deadline, granted_at)?;
                self.authority_deadline = granted_at
                    + Duration::from_secs(if name == "approve-transfer" {
                        self.authorization.inputs().transfer_seconds
                    } else {
                        120
                    });
                self.check_authority()?;
                self.update(|v| {
                    v.review = None;
                    v.review_digest = None;
                });
                return Ok(());
            }
            check_phase_wait_deadline(deadline)?;
            thread::sleep(Duration::from_millis(10));
        }
    }
    #[cfg(test)]
    fn test_phase_wait_boundary(&self, boundary: tests::PhaseWaitBoundary, deadline: Instant) {
        if let Some(hook) = self
            .fixture
            .as_ref()
            .and_then(|f| f.phase_wait_hook.as_ref())
        {
            hook.pause(boundary, deadline);
        }
    }
    fn find_binding(
        &mut self,
        normal_return_only: bool,
    ) -> Result<CompletionBindingWitness, ServiceError> {
        let deadline =
            Instant::now() + Duration::from_secs(self.authorization.inputs().transition_seconds);
        loop {
            self.check_stop()?;
            let generation = self
                .generation
                .checked_add(1)
                .ok_or_else(|| bad("Binding generation exhausted"))?;
            match self.accept_now(generation, normal_return_only) {
                Ok(w) => return Ok(w),
                Err(error) => {
                    if Instant::now() >= deadline {
                        return Err(error);
                    }
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn run(&mut self) -> Result<(), ServiceError> {
        self.authorization.entry_fresh()?;
        if !self.authorization.is_recovery() {
            let baseline = self.baseline_now()?;
            let plan = self.receiver_plan(self.anchor.binding.clone());
            let registry = self.registry_now()?;
            self.quarantine = Some(
                registry.reserve(
                    &self.attempt,
                    &plan,
                    VENDOR_IMAGE_SHA256,
                    self.journal
                        .as_ref()
                        .expect("journal")
                        .path()
                        .parent()
                        .expect("journal parent")
                        .to_str()
                        .ok_or_else(|| bad("Journal path is not UTF8"))?,
                )?,
            );
            self.quarantine.as_mut().expect("risk").arm()?;
            self.open_entry()?;
            for (index, wire) in crate::firmware_profile::entry_candidates()
                .into_iter()
                .enumerate()
            {
                self.authorization.entry_fresh()?;
                let current = self.baseline_now();
                if !current.as_ref().is_ok_and(|receipt| receipt == &baseline) {
                    if index > 0 && self.transition_now(&baseline)? {
                        self.record(serde_json::json!({"kind":"entry-candidate-omitted","ordinal":index,"reason":"Observed passive normal transition after complete candidate1 write/drain; retain owner for fresh separately reviewed query","device_success":false}))?;
                        break;
                    }
                    return Err(bad(
                        "Original normal binding changed or became ambiguous before entry offer",
                    ));
                }
                self.io.as_mut().expect("entry reader").healthy()?;
                let receipt = self.offer(
                    &wire,
                    "fixed normal entry candidate",
                    RecordingPolicy::Immediate,
                    |_| Ok(()),
                )?;
                self.record(serde_json::json!({"kind":"entry-host-settlement","extent":format!("{:?}",receipt.extent),"drain":format!("{:?}",receipt.drain),"no_device_ack":true}))?;
                if receipt.extent != VendorWriteExtent::Known(wire.len())
                    || receipt.drain != VendorDrain::Returned
                {
                    return Err(bad(
                        "Entry offer unknown/partial/zero or drain failed; no retry",
                    ));
                }
            }
            self.close_io(true)?;
        } else {
            let registry = self.registry_now()?;
            let plan = self.receiver_plan(self.anchor.binding.clone());
            let root = self
                .journal
                .as_ref()
                .expect("journal")
                .path()
                .parent()
                .expect("journal parent")
                .to_str()
                .ok_or_else(|| bad("Recovery journal path is not UTF8"))?;
            let lineage = self
                .authorization
                .lineage()
                .expect("checked recovery lineage");
            lineage
                .validate_linked_closure_budget(self.journal.as_ref().expect("journal").path())?;
            #[cfg(test)]
            let quarantine = if self.fixture.is_some() {
                registry.reserve_recovery_fixture(
                    &self.attempt,
                    &plan,
                    VENDOR_IMAGE_SHA256,
                    root,
                    lineage,
                )?
            } else {
                registry.reserve_recovery(
                    &self.attempt,
                    &plan,
                    VENDOR_IMAGE_SHA256,
                    root,
                    lineage,
                )?
            };
            #[cfg(not(test))]
            let quarantine = registry.reserve_recovery(
                &self.attempt,
                &plan,
                VENDOR_IMAGE_SHA256,
                root,
                lineage,
            )?;
            self.quarantine = Some(quarantine);
            if let ExecutionAuthorization::Recovery(a) = &mut self.authorization {
                a.retain_live_successor(self.attempt.clone());
            }
            self.authorization.revalidate()?;
            self.record(serde_json::json!({"kind":"recovery-predecessors-checked","purpose":crate::firmware_linux_recovery::FRESH_RECOVERY_PURPOSE,"history":self.authorization.history(),"new_query_required":true,"starts_at_section":0,"starts_at_block":0}))?;
        }
        self.set_phase(LinuxCompletionPhase::FindingBoot);
        let boot = self.find_binding(false)?;
        self.set_phase(LinuxCompletionPhase::ReviewBootQuery);
        let query_digest=self.phase_review("boot-query",serde_json::json!({"run":self.attempt,"source_review":self.authorization.digest(),"candidate":boot.observation(),"recovery_history":self.authorization.history(),"wire":[240,126,72,1,247],"offers":1,"transfer_permitted":false,"backup":self.authorization.backup(),"limitations":self.authorization.limitations()}))?;
        self.wait_signal(
            "approve-query",
            &format!("execute-query:{query_digest}"),
            self.authorization.inputs().approval_wait_seconds,
        )?;
        if self.authorization.is_recovery() {
            self.authorization.revalidate()?;
            self.check_stop()?;
            self.quarantine
                .as_mut()
                .expect("new recovery reservation")
                .arm()?;
        }
        self.open_boot(&boot)?;
        self.set_phase(LinuxCompletionPhase::QueryingBoot);
        let mut engine = VendorSequence::new(
            self.authorization.firmware_review().clone(),
            self.generation,
            self.millis()?,
        )
        .map_err(|e| bad(format!("Vendor engine admission: {e:?}")))?;
        self.drive_engine_action(&mut engine, &boot)?;
        let chunk = engine
            .negotiated_chunk()
            .ok_or_else(|| bad("Actual boot query yielded no negotiated chunk"))?;
        let budget = FirmwareJournalBudget::for_blocks(
            self.authorization
                .firmware_review()
                .count_review(chunk)?
                .sections
                .iter()
                .map(|s| u64::from(s.count.safe_integer_blocks))
                .sum(),
        )?;
        self.update(|v| v.total_blocks = Some(budget.blocks));
        self.set_phase(LinuxCompletionPhase::ReviewTransfer);
        let digest=self.phase_review("transfer",serde_json::json!({"run":self.attempt,"source_review":self.authorization.digest(),"image_sha256":VENDOR_IMAGE_SHA256,"contract_sha256":VENDOR_CONTRACT_SHA256,"fresh_boot":boot.observation(),"actual_chunk":chunk,"budget":budget,"transfer_seconds":self.authorization.inputs().transfer_seconds,"approval_wait_seconds":self.authorization.inputs().approval_wait_seconds,"actual_identity_receipt":self.last_receipt.as_ref().map(|r|serde_json::json!({"sequence":r.sequence,"prefix_sha256":r.prefix_sha256,"path":r.path,"bytes":r.bytes})),"offers_each":1,"automatic_retry":false,"unkeyed_status_limit":"A delayed old status arriving during a later offer cannot be distinguished from its valid reply.","terminal09":"HostFinishedUnverified; later normal reads and manual application version required","backup":self.authorization.backup(),"limitations":self.authorization.limitations()}))?;
        self.wait_signal(
            "approve-transfer",
            &format!("execute-transfer:{digest}"),
            self.authorization.inputs().approval_wait_seconds,
        )?;
        boot.revalidate(
            &self.anchor.binding,
            self.anchor.guard.as_ref().expect("guard"),
        )?;
        self.set_phase(LinuxCompletionPhase::Transferring);
        let deadline = self.authority_deadline;
        while engine.state() != VendorState::HostFinishedUnverified {
            self.check_stop()?;
            if Instant::now() >= deadline {
                return Err(bad("Total transfer host budget exceeded; no resume/retry"));
            }
            self.drive_engine_action(&mut engine, &boot)?;
            // Block progress is published only by its actual journal acknowledgement.
        }
        self.update(|v| {
            v.host_finished_unverified = true;
            v.phase = LinuxCompletionPhase::HostFinishedUnverified;
        });
        self.close_io(true)?;
        self.set_phase(LinuxCompletionPhase::FindingNormal);
        let normal = self.find_binding(true)?;
        self.set_phase(LinuxCompletionPhase::ReadingNormal);
        let normal_epoch = SessionEpoch(
            self.epoch
                .0
                .checked_add(1)
                .ok_or_else(|| bad("Fresh normal epoch exhausted"))?,
        );
        let proof = self.normal_reads_now(&normal, normal_epoch, &self.stop.requested)?;
        if !proof.reader_joined || !proof.output_destroyed || proof.allocated != proof.processed {
            return Err(bad("Fresh normal reader/frontier is not retired"));
        }
        self.record(serde_json::json!({"kind":"normal-mode-observed","proof":proof,"target_application_version":"not observed"}))?;
        self.selection = Some(normal.observation().selection.clone());
        self.normal_proof = Some(proof.clone());
        self.update(|v| v.normal_mode_verified = true);
        self.set_phase(LinuxCompletionPhase::ConfirmApplicationVersion);
        let manual_digest=self.phase_review("application-version",serde_json::json!({"run":self.attempt,"normal":proof,"application_version_required":"Read V1.8.2 on the pedal display now; image/boot version cannot substitute","contents_verified":false,"installed_IR_audio":"unavailable","journal_prefix":self.last_receipt.as_ref().map(|r|&r.prefix_sha256)}))?;
        self.wait_signal(
            "confirm-application-version",
            &format!("confirm-application:{manual_digest}:V1.8.2"),
            self.authorization.inputs().approval_wait_seconds,
        )?;
        normal.revalidate(
            &self.anchor.binding,
            self.anchor.guard.as_ref().expect("guard"),
        )?;
        self.target_version = Some("V1.8.2".into());
        self.update(|v| v.target_version_verified = true);
        self.set_phase(LinuxCompletionPhase::ClosingAttempt);
        self.record(serde_json::json!({"kind":"live-completion-verified","normal":proof,"manual_version":"V1.8.2","manual_phase_review":manual_digest,"image_sha256":VENDOR_IMAGE_SHA256,"contents_verified":false,"installed_IR_audio":"unavailable"}))?;
        let journal = self
            .last_receipt
            .take()
            .ok_or_else(|| bad("Completion durable pair missing"))?;
        let record = serde_json::json!({"schema":1,"attempt":self.attempt,"run":journal.run,"sequence":journal.sequence,"prefix_sha256":journal.prefix_sha256,"journal":journal.path,"normal":{"epoch":proof.epoch,"binding":proof.binding,"identity_sha256":proof.identity_sha256,"current_sha256":proof.current_sha256,"globals_sha256":proof.globals_sha256,"allocated":proof.allocated,"processed":proof.processed,"reader_joined":proof.reader_joined,"output_destroyed":proof.output_destroyed},"manual_phase_review":manual_digest,"manual_application_version":"V1.8.2","source_review":self.authorization.digest(),"image_sha256":VENDOR_IMAGE_SHA256,"contents_verified":false});
        self.authorization.revalidate()?;
        self.check_stop()?;
        let guard = if let ExecutionAuthorization::Recovery(a) = &self.authorization {
            let witness = LiveRecoveryCompletionClearance {
                attempt: self.attempt.clone(),
                run: self.journal.as_ref().expect("journal").run().into(),
                journal,
                guard: self
                    .anchor
                    .guard
                    .take()
                    .ok_or_else(|| bad("Recovery physical guard missing"))?,
                anchor_binding: self.anchor.binding.clone(),
                lineage: &a.lineage,
                record,
                #[cfg(test)]
                mock_origin: self.fixture.is_some(),
            };
            self.quarantine
                .as_mut()
                .expect("risk")
                .complete_recovery(witness)?
        } else {
            let witness = LiveCompletionClearance {
                original_binding: self.anchor.binding.clone(),
                attempt: self.attempt.clone(),
                run: self.journal.as_ref().expect("journal").run().into(),
                record,
                journal,
                guard: self
                    .anchor
                    .guard
                    .take()
                    .ok_or_else(|| bad("Live completion guard missing"))?,
            };
            self.quarantine
                .as_mut()
                .expect("risk")
                .complete_linux(witness)?
        };
        self.anchor.guard = Some(guard);
        self.update(|v| {
            v.quarantined = false;
            v.phase = LinuxCompletionPhase::ReturningEditor;
        });
        Ok(())
    }
    fn drive_engine_action(
        &mut self,
        engine: &mut VendorSequence,
        witness: &CompletionBindingWitness,
    ) -> Result<(), ServiceError> {
        loop {
            self.check_stop()?;
            self.collect_raw()?;
            if self
                .pending_block
                .as_ref()
                .is_some_and(|block| block.completed.is_some())
                && (!self.raw_pending.is_empty()
                    || !self.frames.is_empty()
                    || self.framer.pending())
            {
                // New unsolicited traffic cannot be inserted ahead of the older
                // staged pair, nor become the next action's reply.
                self.flush_block_outcome()?;
            }
            self.persist_raw()?;
            if let Some(action) = engine
                .poll_action(self.millis()?)
                .map_err(|e| bad(format!("Vendor next action: {e:?}")))?
                .cloned()
            {
                if self
                    .pending_block
                    .as_ref()
                    .is_some_and(|block| block.completed.is_some())
                    && !self.can_fuse_block(&action)
                {
                    self.flush_block_outcome()?;
                }
                witness.revalidate(
                    &self.anchor.binding,
                    self.anchor.guard.as_ref().expect("guard"),
                )?;
                self.collect_raw()?;
                if !self.frames.is_empty() || self.framer.pending() {
                    return Err(bad("Unsolicited raw traffic before engine action"));
                }
                let receipt = self.offer(
                    action.wire(),
                    &format!("vendor {:?}", action.phase()),
                    if action.phase() == VendorPhase::Block {
                        RecordingPolicy::BlockOutcome
                    } else {
                        RecordingPolicy::Immediate
                    },
                    |at_ms| {
                        engine
                            .begin_offer(action.ordinal(), at_ms)
                            .map_err(|e| bad(format!("Vendor offer: {e:?}")))
                    },
                )?;
                // Consume original RX captured during write/drain before the current host deadline check.
                let after_offer = self.collect_raw();
                self.persist_action_raw()?;
                while let Some(frame) = self.frames.pop_front() {
                    if let Some(block) = &mut self.pending_block {
                        if block.frame_spans.len() >= 64 {
                            return Err(bad("Block frame provenance capacity exceeded"));
                        }
                        block.frame_spans.push(serde_json::json!({"sequence":frame.sequence,
                            "first_chunk":frame.first_chunk,"last_chunk":frame.last_chunk,"at_ns":frame.at_ns}));
                    }
                    if frame.last_chunk < frame.first_chunk
                        || frame.last_chunk > self.raw_frontier
                        || frame.first_chunk <= self.last_offer_sequence
                        || frame.at_ns < self.last_offer_ns
                    {
                        return Err(bad("Actual raw frame began before this write invocation"));
                    }
                    engine
                        .receive(VendorFrameObservation {
                            generation: self.generation,
                            sequence: frame.sequence,
                            at_ms: frame.at_ns / 1_000_000,
                            raw: frame.raw,
                        })
                        .map_err(|e| bad(format!("Vendor raw frame: {e}")))?;
                }
                let finish_transition = action.phase() == VendorPhase::FinishControl
                    && receipt.extent == VendorWriteExtent::Known(action.wire().len())
                    && receipt.drain == VendorDrain::Returned
                    && self.expected_disconnect_now();
                if let Err(error) = after_offer
                    && !finish_transition
                {
                    return Err(error);
                }
                engine
                    .finish_write(action.ordinal(), receipt)
                    .map_err(|e| bad(format!("Vendor write/drain: {e}")))?;
                loop {
                    self.check_stop()?;
                    let collected = self.collect_raw();
                    self.persist_action_raw()?;
                    while let Some(frame) = self.frames.pop_front() {
                        if let Some(block) = &mut self.pending_block {
                            if block.frame_spans.len() >= 64 {
                                return Err(bad("Block frame provenance capacity exceeded"));
                            }
                            block.frame_spans.push(serde_json::json!({"sequence":frame.sequence,
                                "first_chunk":frame.first_chunk,"last_chunk":frame.last_chunk,"at_ns":frame.at_ns}));
                        }
                        if frame.last_chunk < frame.first_chunk
                            || frame.last_chunk > self.raw_frontier
                            || frame.first_chunk <= self.last_offer_sequence
                            || frame.at_ns < self.last_offer_ns
                        {
                            return Err(bad("Actual raw frame began before this write invocation"));
                        }
                        engine
                            .receive(VendorFrameObservation {
                                generation: self.generation,
                                sequence: frame.sequence,
                                at_ms: frame.at_ns / 1_000_000,
                                raw: frame.raw,
                            })
                            .map_err(|e| bad(format!("Vendor raw status: {e}")))?;
                    }
                    if let Err(error) = collected
                        && (engine.state() != VendorState::HostFinishedUnverified
                            || !self.expected_disconnect_now())
                    {
                        return Err(error);
                    }
                    if let Some(receipt) = engine.completed_receipt() {
                        let json = receipt_json(receipt);
                        let mut pair = serde_json::json!({"kind":"vendor-action-pair","receipt":json,"raw_frontier":self.raw_frontier});
                        if let Some(block) = &self.pending_block {
                            pair["original_frame_spans"] = serde_json::json!(block.frame_spans);
                        }
                        if let Some(block) = &mut self.pending_block {
                            block.pair = Some(pair);
                            // Only the owner may use this local retirement to
                            // describe a successor. It is not durable progress.
                            block.completed = Some(engine.take_completed().ok_or_else(||
                                bad("Original completed Block changed before provisional retirement"))?);
                            #[cfg(test)]
                            if self.fixture.as_ref().is_some_and(|fixture| {
                                matches!(
                                    fixture.fault,
                                    tests::FixtureFault::BlockStopBeforeFusionSubmit
                                        | tests::FixtureFault::BlockEagain
                                        | tests::FixtureFault::BlockRepeatedEagain
                                        | tests::FixtureFault::BlockSplit
                                        | tests::FixtureFault::BlockDuringDrain
                                )
                            }) {
                                self.stop.requested.store(true, Ordering::SeqCst);
                            }
                            if block.immediate_raw || block.immediate_drain {
                                self.flush_block_outcome()?;
                            }
                        } else {
                            self.record(pair)?;
                            engine.take_completed().ok_or_else(|| {
                                bad("Durable action receipt changed before retirement")
                            })?;
                        }
                        return Ok(());
                    }
                    self.io.as_mut().expect("boot reader").healthy()?;
                    engine
                        .tick(self.millis()?)
                        .map_err(|e| bad(format!("Vendor phase timeout/failure: {e:?}")))?;
                    thread::sleep(Duration::from_millis(1));
                }
            }
            // A prepared delay or boundary must not leave completed RAM-only
            // evidence waiting for an unrelated future action.
            self.flush_block_outcome()?;
            thread::sleep(Duration::from_millis(1));
        }
    }
}
pub(crate) fn expected_transition_fault(
    fault: Option<&super::firmware_linux_completion_io::CompletionReadFault>,
    processed: u64,
    allocated: u64,
    no_publication_loss: bool,
    raw_error: Option<&ServiceError>,
    passive_transition: bool,
) -> bool {
    fault.is_some_and(|f| {
        matches!(f.errno, Some(5 | 19))
            && f.operation != "publication"
            && f.before_sequence == processed
            && f.before_sequence.checked_add(1) == Some(f.sequence)
            && f.after_sequence == f.sequence
            && allocated == f.sequence
            && no_publication_loss
            && raw_error.is_none_or(|e| {
                e.message == "Completion reader publication/fault left an unexplained raw frontier"
            })
            && passive_transition
    })
}
fn check_phase_wait_deadline_at(deadline: Instant, now: Instant) -> Result<(), ServiceError> {
    if now >= deadline {
        Err(bad(
            "Finite exact phase approval expired; elapsed time supplies no approval",
        ))
    } else {
        Ok(())
    }
}
fn check_phase_wait_deadline(deadline: Instant) -> Result<(), ServiceError> {
    check_phase_wait_deadline_at(deadline, Instant::now())
}

fn recovery_startup_held(
    store: LibraryStore,
    view: Arc<Mutex<LinuxCompletionView>>,
    editor_source_epoch: SessionEpoch,
    history: Option<crate::LinuxRecoveryHistoryReview>,
    error: ServiceError,
) -> RunResult {
    let mut v = view.lock().unwrap_or_else(|p| p.into_inner());
    v.phase = LinuxCompletionPhase::RecoveryRequired;
    v.quarantined = true;
    v.diagnostic = Some(error.message);
    RunResult {
        store,
        outcome: OwnedOutcome::Recovery(Box::new(crate::LinuxRecoveryOutcome {
            view: v.clone(),
            selection: None,
            editor_source_epoch,
            verified_normal_epoch: None,
            application_version: None,
            journal_path: String::new(),
            editor_return_allowed: false,
            history,
        })),
    }
}

// This branch exists only before the first invocation and owns the original handoff.
// Its independently durable closure never represents target firmware or an update success.
fn return_startup_without_call(
    mut failed: ExecutorStartFailure,
    view: Arc<Mutex<LinuxCompletionView>>,
) -> Result<RunResult, ServiceError> {
    let epoch = failed.input.handoff.normal_epoch;
    let mut selection = None;
    let mut verified = None;
    let mut closure_path = String::new();
    let result = (|| {
        let next = SessionEpoch(
            epoch
                .0
                .checked_add(1)
                .ok_or_else(|| bad("Zero-call normal epoch exhausted"))?,
        );
        let guard = failed
            .input
            .handoff
            .guard
            .as_mut()
            .ok_or_else(|| bad("Original zero-call handoff guard missing"))?;
        #[cfg(test)]
        let witness = match &failed.input.fixture {
            Some(fixture) => binding::accept_association_fixture(
                &failed.input.handoff.binding,
                &failed.input.handoff.binding,
                1,
                guard,
                &fixture.usb,
                &fixture.sound,
                fixture.endpoints.clone(),
                true,
            )?,
            None => binding::accept_normal_return(
                &failed.input.handoff.binding,
                &failed.input.handoff.binding,
                1,
                guard,
            )?,
        };
        #[cfg(not(test))]
        let witness = binding::accept_normal_return(
            &failed.input.handoff.binding,
            &failed.input.handoff.binding,
            1,
            guard,
        )?;
        #[cfg(test)]
        let proof = match &failed.input.fixture {
            Some(fixture) => {
                fixture.normal_reads(&witness, next, &std::sync::atomic::AtomicBool::new(false))?
            }
            None => crate::raw_backend::completion_normal_reads(
                &witness,
                &failed.input.handoff.binding,
                guard,
                next,
                &std::sync::atomic::AtomicBool::new(false),
            )?,
        };
        #[cfg(not(test))]
        let proof = crate::raw_backend::completion_normal_reads(
            &witness,
            &failed.input.handoff.binding,
            guard,
            next,
            &std::sync::atomic::AtomicBool::new(false),
        )?;
        if !proof.reader_joined || !proof.output_destroyed || proof.allocated != proof.processed {
            return Err(bad("Startup zero-call normal close/frontier is unresolved"));
        }
        let path = failed
            .input
            .authorization
            .review
            .inputs
            .run_directory
            .join("startup-zero-invocations.json");
        let closure = serde_json::json!({"schema":1,"kind":"live-creating-owner-startup-zero-invocations","run":view.lock().unwrap_or_else(|p|p.into_inner()).run,"source_review":failed.input.authorization.digest(),"original_normal_epoch":epoch,"primary_journal_error":failed.error.message,"normal":proof,"calls":0,"offered":false,"target_version_verified":false,"completion_verified":false});
        publish(&path, &closure)?;
        selection = Some(witness.observation().selection.clone());
        verified = Some(proof.epoch);
        closure_path = path.display().to_string();
        Ok(())
    })();
    let mut final_view = view.lock().unwrap_or_else(|p| p.into_inner());
    let allowed = result.is_ok();
    final_view.normal_mode_verified = allowed;
    final_view.quarantined = !allowed;
    final_view.phase = if allowed {
        LinuxCompletionPhase::ReturningEditor
    } else {
        LinuxCompletionPhase::RecoveryRequired
    };
    final_view.diagnostic = Some(match result {
        Ok(()) => failed.error.message,
        Err(e) => format!(
            "{}; zero-invocation return remained held: {}",
            failed.error.message, e.message
        ),
    });
    let outcome = LinuxCompletionOutcome {
        view: final_view.clone(),
        selection,
        original_normal_epoch: epoch,
        verified_normal_epoch: verified,
        application_version: None,
        journal_path: closure_path,
        editor_return_allowed: allowed,
    };
    Ok(RunResult {
        store: failed.input.store,
        outcome: OwnedOutcome::Normal(Box::new(outcome)),
    })
}

fn receipt_json(receipt: &VendorActionReceipt) -> serde_json::Value {
    serde_json::json!({"ordinal":receipt.action.ordinal(),"phase":format!("{:?}",receipt.action.phase()),"wire":receipt.action.wire(),"wire_sha256":receipt.action.wire_sha256(),"offered_at_ms":receipt.offered_at_ms,"receive_frontier_at_offer":receipt.receive_frontier_at_offer,"write":receipt.write.as_ref().map(|w|serde_json::json!({"at_ms":w.at_ms,"extent":format!("{:?}",w.extent),"drain":format!("{:?}",w.drain)})),"frames":receipt.frames.iter().map(|f|serde_json::json!({"generation":f.generation,"sequence":f.sequence,"at_ms":f.at_ms,"raw":f.raw})).collect::<Vec<_>>(),"terminal_data":receipt.terminal_data.iter().map(|d|serde_json::json!({"frame_sequence":d.frame_sequence,"at_ms":d.at_ms,"opcode":d.opcode,"decoded":d.decoded})).collect::<Vec<_>>(),"source_block":receipt.action.block().map(|b|serde_json::json!({"absolute_block_ordinal":b.absolute_block_ordinal,"block_in_section":b.block_in_section,"file_offset":b.file_offset,"content_bytes":b.content_bytes,"padded_bytes":b.padded_bytes,"content_sha256":b.content_sha256,"padding_sha256":b.padding_sha256})),"decision":format!("{:?}",receipt.decision)})
}

/// The concrete opener supplies an owned session and its actual callback source.
/// Closing extracts the original reader receipt; DTOs cannot substitute it.
pub(crate) struct CompletionNormalReadInput {
    pub(crate) session: Box<dyn crate::MidiSession>,
    pub(crate) source: Arc<crate::backend::NormalCallbackSource>,
    pub(crate) received: crossbeam_channel::Receiver<crate::backend::ObservedCallback>,
    pub(crate) overflow: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) epoch: SessionEpoch,
    pub(crate) selection: DeviceSelection,
    pub(crate) binding: FirmwareUsbObservation,
    pub(crate) mode: BackendMode,
    pub(crate) config: crate::TransportConfig,
}
pub(crate) fn read_normal_owned(
    mut input: CompletionNormalReadInput,
    stop: &std::sync::atomic::AtomicBool,
) -> Result<CompletionNormalProof, ServiceError> {
    use crate::{DeviceActor, TransportAction};
    let origin = Instant::now();
    let deadline = origin + Duration::from_secs(15);
    let mut actor = DeviceActor::with_mode(input.config, input.mode);
    actor.set_fresh_epoch_before_connect(input.epoch)?;
    actor.command(
        DeviceCommand::Connect {
            intent: IntentId(1),
            selection: input.selection.clone(),
        },
        Duration::ZERO,
    );
    let mut decoder = gp200_protocol::StreamDecoder::new();
    let mut assembler = gp200_protocol::Reassembler::default();
    let mut identity = None;
    let mut processed = 0;
    let mut read = 0u8;
    let mut opened = false;
    let mut retained = Vec::new();
    let result = (|| {
        loop {
            if stop.load(Ordering::SeqCst) || Instant::now() >= deadline {
                return Err(bad(
                    "Fresh normal identity/current/globals read was cancelled or exceeded the finite host budget",
                ));
            }
            input.session.health()?;
            while let Some(action) = actor.next_action() {
                match action {
                    TransportAction::Open { .. } => {
                        opened = true;
                        actor.opened(Ok(()), origin.elapsed());
                    }
                    TransportAction::Send { packets } => {
                        for packet in packets {
                            input.session.send(&packet)?;
                        }
                        actor.sent(Ok(()), origin.elapsed());
                    }
                    TransportAction::Event(DeviceEvent::ConnectionChanged {
                        state: ConnectionState::Ready,
                        ..
                    }) if read == 0 => {
                        read = 1;
                        actor.command(
                            DeviceCommand::ReadCurrent(RequestStamp {
                                epoch: actor.epoch(),
                                intent: IntentId(2),
                            }),
                            origin.elapsed(),
                        );
                    }
                    TransportAction::Event(DeviceEvent::SnapshotReceived {
                        target: PresetTarget::EditBuffer,
                        stamp,
                        ..
                    }) if read == 1 && stamp.intent == IntentId(2) => {
                        read = 2;
                        actor.command(
                            DeviceCommand::ReadGlobals(RequestStamp {
                                epoch: actor.epoch(),
                                intent: IntentId(3),
                            }),
                            origin.elapsed(),
                        );
                    }
                    TransportAction::Event(DeviceEvent::GlobalStateReceived { stamp, .. })
                        if read == 2 && stamp.intent == IntentId(3) =>
                    {
                        read = 3;
                    }
                    TransportAction::Event(DeviceEvent::OperationFailed { error, .. }) => {
                        return Err(error);
                    }
                    TransportAction::Close if !opened => {}
                    TransportAction::Close => {
                        return Err(bad(
                            "Fresh normal actor closed before the complete read set",
                        ));
                    }
                    _ => {}
                }
            }
            for _ in 0..64 {
                let Ok(original) = input.received.try_recv() else {
                    break;
                };
                let callback = original.callback;
                if callback.epoch != input.epoch
                    || callback.monotonic_ns.is_none()
                    || callback.ordinal != processed + 1
                    || original.clock_origin != input.source.clock_origin()
                    || callback.original_len != callback.retained.len() as u64
                {
                    return Err(bad(
                        "Fresh normal original callback source/order/extent differs",
                    ));
                }
                if retained.len() >= 256 {
                    return Err(bad("Fresh normal retained callback budget exceeded"));
                }
                retained.push(serde_json::json!({"sequence":callback.ordinal,"epoch":callback.epoch,"at_ns":callback.monotonic_ns,"clock_origin":original.clock_origin,"raw":callback.retained}));
                for event in decoder.feed(&callback.retained) {
                    match event {
                        gp200_protocol::StreamEvent::Packet(packet) => {
                            if let Some(raw) =
                                assembler.add(&packet, origin.elapsed()).map_err(bad)?
                                && raw.records().iter().any(|record| record.command == 0x2001)
                            {
                                if identity.is_some() || raw.payload().len() > 2048 {
                                    return Err(bad(
                                        "Fresh normal identity duplicate or oversized",
                                    ));
                                }
                                let parsed = gp200_protocol::Identity::from_transfer(&raw)
                                    .ok_or_else(|| bad("Fresh normal identity is malformed"))?;
                                if parsed.state_raw != 1
                                    || parsed.model() != gp200_protocol::WireDeviceModel::Gp200
                                {
                                    return Err(bad(
                                        "Fresh normal receiver is not observed GP-200 normal state",
                                    ));
                                }
                                identity = Some(raw.payload().to_vec());
                            }
                        }
                        _ => {
                            return Err(bad(
                                "Fresh normal decoder retained a malformed stream observation",
                            ));
                        }
                    }
                }
                processed = callback.ordinal;
                actor.receive(callback.epoch, &callback.retained, origin.elapsed());
            }
            if input.source.failed() || input.overflow.load(Ordering::SeqCst) {
                return Err(bad(
                    "Fresh normal original callback publication lost evidence",
                ));
            }
            if read == 3 {
                break;
            }
            actor.tick(origin.elapsed());
            thread::sleep(Duration::from_millis(1));
        }
        if input.source.fence(processed).allocated != processed
            || !input.received.is_empty()
            || actor.is_busy()
        {
            return Err(bad("Fresh normal read frontier is unresolved"));
        }
        let identity = identity.ok_or_else(|| bad("Fresh normal identity is absent"))?;
        let current = actor
            .snapshot(PresetTarget::EditBuffer)
            .ok_or_else(|| bad("Fresh normal current is absent"))?;
        let globals = actor
            .global_state()
            .ok_or_else(|| bad("Fresh normal globals are absent"))?;
        Ok((
            JobSha256::of(&identity).hex(),
            JobSha256::of(current.raw.payload()).hex(),
            JobSha256::of(globals.records().raw().payload()).hex(),
        ))
    })();
    // Always retire the original session, including read/decoder/timeout failures.
    let handoff = input.session.take_firmware_handoff();
    let close = if handoff.is_err() {
        input.session.close_normal_capture()
    } else {
        Ok(())
    };
    drop(input.session);
    let receipt = handoff
        .ok()
        .and_then(|mut handoff| handoff.normal_close.take());
    let settled = receipt.as_ref().is_some_and(|receipt| {
        Arc::ptr_eq(&receipt.source, &input.source)
            && receipt.epoch == input.epoch
            && receipt.allocated == processed
            && receipt.join == FirmwareNormalReaderJoinObservation::Returned
            && receipt.started.is_some()
            && receipt.finished.is_some()
            && receipt.fault.is_none()
            && !receipt.overflow
            && receipt.output_destroyed
            && !input.source.failed()
            && input.received.is_empty()
            && input.source.fence(processed).allocated == processed
    });
    let (identity_sha256, current_sha256, globals_sha256) = result?;
    close?;
    if !settled {
        return Err(bad(
            "Fresh normal actual close/join/output/frontier receipt is unresolved",
        ));
    }
    Ok(CompletionNormalProof {
        epoch: input.epoch,
        binding: input.binding,
        identity_sha256,
        current_sha256,
        globals_sha256,
        allocated: processed,
        processed,
        reader_joined: true,
        output_destroyed: true,
        retained_raw: retained,
    })
}
#[cfg(test)]
#[path = "firmware_linux_completion_tests.rs"]
mod tests;
