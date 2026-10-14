//! A single-consumer archival route on the existing library writer thread.
use crate::{
    BackupAcquisition, BackupOrigin, ContentHash, FirmwareArchiveBoundary,
    FirmwareArchiveObservation, FirmwareBackupReviewDraft, LibraryStore,
    PublishedFirmwareBackupReview, PublishedFirmwareCaptureEvidence,
    firmware_backup_capture_evidence::CaptureDraft,
    store::{invalid, nonce},
};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use gp200_domain::*;
use std::{
    sync::{Arc, Mutex, TryLockError},
    time::{Duration, Instant},
};
pub const FIRMWARE_BACKUP_COMMAND_CAPACITY: usize = 8;
pub const FIRMWARE_BACKUP_EVENT_CAPACITY: usize = 8;
#[derive(Debug)]
pub struct FirmwareBackupBegin {
    pub origin: BackupOrigin,
    pub context: FirmwareBackupReviewContext,
    pub opening: FirmwareArchiveBoundary,
}
#[derive(Debug)]
pub struct FirmwareCapturedBegin {
    pub begin: FirmwareBackupBegin,
    pub evidence: FirmwareCaptureEvidence,
}
#[derive(Debug)]
pub struct FirmwareCapturedObservation {
    pub observation: FirmwareArchiveObservation,
    pub evidence: FirmwareCaptureEvidence,
}
pub enum FirmwareBackupAction {
    PrepareCapture(FirmwareCaptureReservation),
    BeginCaptured(Box<FirmwareCapturedBegin>),
    StoreCaptured(Box<FirmwareCapturedObservation>),
    StoreCaptureBoundary(Box<FirmwareCaptureEvidence>),
    StoreCaptureFailure(Box<FirmwareCaptureEvidence>),
    CloseCaptured {
        boundary: FirmwareArchiveBoundary,
        outcome: BackupAcquisition,
    },
    PublishCaptured,
    RevalidateCaptured(Box<PublishedFirmwareCaptureEvidence>),
    Begin(Box<FirmwareBackupBegin>),
    Store(FirmwareArchiveObservation),
    Close {
        boundary: FirmwareArchiveBoundary,
        outcome: BackupAcquisition,
    },
    Publish,
    Revalidate(Box<PublishedFirmwareBackupReview>),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirmwareBackupActionAddress {
    Capture {
        kind: FirmwareCaptureReceiptKind,
        read: Option<FirmwareCaptureRead>,
        observation_sequence: Option<u64>,
    },
    Begin,
    Store {
        object: FirmwareBackupObject,
        pass: FirmwareObservationPass,
        observation_sequence: u64,
    },
    Close,
    Publish,
    Revalidate,
}
impl FirmwareBackupAction {
    fn address(&self) -> FirmwareBackupActionAddress {
        match self {
            Self::PrepareCapture(_) => capture_address(FirmwareCaptureReceiptKind::Prepared, None),
            Self::BeginCaptured(v) => {
                capture_address(FirmwareCaptureReceiptKind::Begun, Some(&v.evidence))
            }
            Self::StoreCaptured(v) => {
                capture_address(FirmwareCaptureReceiptKind::Observation, Some(&v.evidence))
            }
            Self::StoreCaptureBoundary(v) => {
                capture_address(FirmwareCaptureReceiptKind::Boundary, Some(v))
            }
            Self::StoreCaptureFailure(v) => {
                capture_address(FirmwareCaptureReceiptKind::Failure, Some(v))
            }
            Self::CloseCaptured { .. } => capture_address(FirmwareCaptureReceiptKind::Closed, None),
            Self::PublishCaptured => capture_address(FirmwareCaptureReceiptKind::Published, None),
            Self::RevalidateCaptured(_) => {
                capture_address(FirmwareCaptureReceiptKind::Revalidated, None)
            }
            Self::Begin(_) => FirmwareBackupActionAddress::Begin,
            Self::Store(o) => FirmwareBackupActionAddress::Store {
                object: o.object,
                pass: o.pass,
                observation_sequence: o.sequence,
            },
            Self::Close { .. } => FirmwareBackupActionAddress::Close,
            Self::Publish => FirmwareBackupActionAddress::Publish,
            Self::Revalidate(_) => FirmwareBackupActionAddress::Revalidate,
        }
    }
}
fn capture_address(
    kind: FirmwareCaptureReceiptKind,
    evidence: Option<&FirmwareCaptureEvidence>,
) -> FirmwareBackupActionAddress {
    FirmwareBackupActionAddress::Capture {
        kind,
        read: evidence.map(|e| e.read.clone()),
        observation_sequence: evidence.map(|e| e.sequence),
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareBackupRequestAddress {
    pub run: String,
    pub acquisition: Option<String>,
    pub sequence: u64,
    pub action: FirmwareBackupActionAddress,
}
pub struct FirmwareCaptureDurableResult {
    pub receipt: FirmwareBackupDurableReceipt,
    pub publication: Option<Box<PublishedFirmwareCaptureEvidence>>,
}
pub enum FirmwareBackupResult {
    CaptureDurable(Box<FirmwareCaptureDurableResult>),
    Begun { acquisition: String },
    ObjectStored { raw_hash: ContentHash },
    ComparisonStored { raw_hash: ContentHash },
    Closed,
    PackagePublished(Box<PublishedFirmwareBackupReview>),
    Revalidated(Box<PublishedFirmwareBackupReview>),
}
pub struct FirmwareBackupEvent {
    pub address: FirmwareBackupRequestAddress,
    pub result: Result<FirmwareBackupResult, ServiceError>,
}
struct Gate {
    stopped: bool,
    pending: Option<FirmwareBackupRequestAddress>,
    terminal: Option<String>,
    settled: bool,
    requires_cleanup: bool,
    receipt: Option<u64>,
    capture_prepared: bool,
    capture_aborted: bool,
    capture_closed: bool,
    capture_published: bool,
    capture_revalidated: bool,
    capture_revalidation_consumed: bool,
    cleanup_failed: bool,
    publication_failed: bool,
    archival_begun: bool,
    archival_closed: bool,
    archival_published: bool,
    archival_revalidated: bool,
    archival_revalidation_consumed: bool,
}
struct Shared {
    gate: Mutex<Gate>,
    admission: Option<Arc<crate::firmware_runtime_owner::Admission>>,
}
impl Shared {
    fn stop(&self, note: &str) {
        let mut gate = self.gate.lock().unwrap_or_else(|p| p.into_inner());
        gate.stopped = true;
        if gate.requires_cleanup {
            gate.settled = false;
        }
        if gate.terminal.is_none() {
            gate.terminal = Some(note.chars().take(4096).collect());
        }
    }
    fn stopped(&self) -> bool {
        self.gate.lock().map_or(true, |g| g.stopped)
    }
}
pub struct FirmwareBackupClient {
    commands: Sender<Command>,
    events: Receiver<FirmwareBackupEvent>,
    shared: Arc<Shared>,
    run: String,
    acquisition: Option<String>,
    sequence: u64,
}
struct Command {
    address: FirmwareBackupRequestAddress,
    action: FirmwareBackupAction,
}
pub(crate) struct FirmwareBackupWorkerRoute {
    commands: Receiver<Command>,
    events: Sender<FirmwareBackupEvent>,
    shared: Arc<Shared>,
    run: String,
    draft: Option<FirmwareBackupReviewDraft>,
    begun: bool,
    acquisition: Option<String>,
    expected_sequence: u64,
    terminal_woken: bool,
    capture: Option<CaptureDraft>,
    last_identity: Option<(FirmwareCaptureBoundaryKind, u64, String)>,
    capture_origin: Option<BackupOrigin>,
    capture_cleanup_attempted: bool,
}
#[cfg(test)]
pub(crate) fn channel() -> Result<(FirmwareBackupClient, FirmwareBackupWorkerRoute), ServiceError> {
    channel_with_admission(None)
}
pub(crate) fn channel_with_admission(
    admission: Option<Arc<crate::firmware_runtime_owner::Admission>>,
) -> Result<(FirmwareBackupClient, FirmwareBackupWorkerRoute), ServiceError> {
    let run = nonce()?;
    let (commands, rx) = bounded(FIRMWARE_BACKUP_COMMAND_CAPACITY);
    let (tx, events) = bounded(FIRMWARE_BACKUP_EVENT_CAPACITY);
    let shared = Arc::new(Shared {
        gate: Mutex::new(Gate {
            stopped: false,
            pending: None,
            terminal: None,
            settled: true,
            requires_cleanup: false,
            receipt: None,
            capture_prepared: false,
            capture_aborted: false,
            capture_closed: false,
            capture_published: false,
            capture_revalidated: false,
            capture_revalidation_consumed: false,
            cleanup_failed: false,
            publication_failed: false,
            archival_begun: false,
            archival_closed: false,
            archival_published: false,
            archival_revalidated: false,
            archival_revalidation_consumed: false,
        }),
        admission,
    });
    Ok((
        FirmwareBackupClient {
            commands,
            events,
            shared: shared.clone(),
            run: run.clone(),
            acquisition: None,
            sequence: 0,
        },
        FirmwareBackupWorkerRoute {
            commands: rx,
            events: tx,
            shared,
            run,
            draft: None,
            begun: false,
            acquisition: None,
            expected_sequence: 1,
            terminal_woken: false,
            capture: None,
            last_identity: None,
            capture_origin: None,
            capture_cleanup_attempted: false,
        },
    ))
}
impl FirmwareBackupClient {
    pub fn run_id(&self) -> &str {
        &self.run
    }
    pub fn try_submit(
        &mut self,
        action: FirmwareBackupAction,
    ) -> Result<FirmwareBackupRequestAddress, SubmitError<FirmwareBackupAction>> {
        let _admission = if let Some(a) = &self.shared.admission {
            let Some(g) = a.try_gate() else {
                return Err(SubmitError::Busy(action));
            };
            if a.is_revoked() || g.permanently_revoked() {
                return Err(SubmitError::Disconnected(action));
            }
            if !g.offers_allowed() {
                return Err(SubmitError::Busy(action));
            }
            Some(g)
        } else {
            None
        };
        let mut gate = match self.shared.gate.try_lock() {
            Ok(g) => g,
            Err(TryLockError::WouldBlock) => return Err(SubmitError::Busy(action)),
            Err(_) => return Err(SubmitError::Disconnected(action)),
        };
        if gate.stopped {
            return Err(SubmitError::Disconnected(action));
        }
        if gate.capture_aborted
            && !matches!(
                action,
                FirmwareBackupAction::StoreCaptureFailure(_)
                    | FirmwareBackupAction::CloseCaptured { .. }
                    | FirmwareBackupAction::PublishCaptured
                    | FirmwareBackupAction::RevalidateCaptured(_)
            )
        {
            return Err(SubmitError::Disconnected(action));
        }
        if gate.pending.is_some() || gate.receipt.is_some() {
            return Err(SubmitError::Busy(action));
        }
        let Some(sequence) = self.sequence.checked_add(1) else {
            gate.stopped = true;
            return Err(SubmitError::Disconnected(action));
        };
        let address = FirmwareBackupRequestAddress {
            run: self.run.clone(),
            acquisition: self.acquisition.clone(),
            sequence,
            action: action.address(),
        };
        match self.commands.try_send(Command {
            address: address.clone(),
            action,
        }) {
            Ok(()) => {
                self.sequence = sequence;
                gate.pending = Some(address.clone());
                gate.settled = false;
                Ok(address)
            }
            Err(TrySendError::Full(c)) => Err(SubmitError::Busy(c.action)),
            Err(TrySendError::Disconnected(c)) => {
                gate.stopped = true;
                Err(SubmitError::Disconnected(c.action))
            }
        }
    }
    pub fn try_next_event(&mut self) -> Option<FirmwareBackupEvent> {
        let _admission = if let Some(a) = &self.shared.admission {
            let g = a.try_gate()?;
            if a.is_revoked() || g.permanently_revoked() {
                return None;
            }
            Some(g)
        } else {
            None
        };
        let mut gate = self.shared.gate.try_lock().ok()?;
        let mut event = self.events.try_recv().ok()?;
        if gate.pending.as_ref() != Some(&event.address) {
            gate.stopped = true;
            gate.terminal = Some("Mismatched archival durability acknowledgement".into());
            drop(gate);
            drop(_admission);
            drop(event);
            return None;
        }
        let revoked = gate.stopped && event.result.is_ok();
        if !revoked {
            if matches!(event.result, Ok(FirmwareBackupResult::Revalidated(_))) {
                gate.archival_revalidation_consumed = true;
            }
            if let Ok(FirmwareBackupResult::Begun { acquisition }) = &event.result {
                self.acquisition = Some(acquisition.clone());
            } else if let Ok(FirmwareBackupResult::CaptureDurable(result)) = &event.result {
                self.acquisition = result.receipt.description.acquisition.clone();
            }
        }
        gate.pending = None;
        drop(gate);
        drop(_admission);
        // A receipt's Drop revokes its route; never drop it while holding that gate.
        if revoked {
            event.result = Err(ServiceError::new(
                ErrorKind::Indeterminate,
                "Archival success revoked after route stop",
            ));
        }
        Some(event)
    }
    pub fn cancel(&self) {
        self.shared
            .stop("Archival route cancelled; outstanding storage must settle");
    }
    pub fn terminal_failure(&self) -> Option<String> {
        self.shared
            .gate
            .try_lock()
            .ok()
            .and_then(|g| g.terminal.clone())
    }
    pub fn storage_settled(&self) -> bool {
        self.shared.gate.lock().is_ok_and(|g| g.settled)
    }
    /// Actual route state for the separately implemented store-retirement owner.
    /// This descriptive list never returns a store or grants updater permission.
    pub fn capture_retirement_blockers(&self) -> Vec<&'static str> {
        match self.shared.gate.try_lock() {
            Ok(g) => retirement_blockers(&g, self.events.len()),
            Err(_) => vec!["Capture route gate unavailable"],
        }
    }
    pub fn is_idle(&self) -> bool {
        self.shared
            .gate
            .lock()
            .is_ok_and(|g| !g.stopped && g.pending.is_none() && g.receipt.is_none())
    }
}
#[cfg(test)]
#[test]
fn firmware_backup_terminal_peek_does_not_wait_for_an_owned_gate() {
    let (client, _route) = channel_with_admission(None).unwrap();
    let mut held = client.shared.gate.lock().unwrap();
    held.terminal = Some("Actual retained terminal note".into());
    assert_eq!(client.terminal_failure(), None);
    drop(held);
    assert_eq!(
        client.terminal_failure().as_deref(),
        Some("Actual retained terminal note")
    );
}
impl Drop for FirmwareBackupClient {
    fn drop(&mut self) {
        self.shared
            .stop("Archival consumer dropped; publication acknowledgement revoked");
    }
}
impl FirmwareBackupWorkerRoute {
    pub(crate) fn retirement_blockers(&self) -> Vec<&'static str> {
        match self.shared.gate.try_lock() {
            Ok(g) => {
                let mut v = retirement_blockers(&g, self.events.len());
                if !self.commands.is_empty() {
                    v.push("Queued archival/capture action");
                }
                v
            }
            Err(_) => vec!["Capture route gate unavailable"],
        }
    }
    pub(crate) fn revoke_for_retirement(&mut self) {
        self.shared
            .stop("Genuine store retirement revoked the old archival generation");
    }
    pub(crate) fn tick(
        &mut self,
        store: &mut LibraryStore,
        shutdown: &ShutdownToken,
        wake: &WakeUi,
    ) {
        if shutdown.cancelled() {
            self.shared.stop("Library worker shutdown");
        }
        if self.shared.stopped() {
            self.abandon(store);
            self.wake_terminal(wake);
            return;
        }
        let command = match self.commands.try_recv() {
            Ok(c) => c,
            Err(crossbeam_channel::TryRecvError::Empty) => return,
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                self.shared.stop("Archival producer lost");
                self.abandon(store);
                self.wake_terminal(wake);
                return;
            }
        };
        if self.shared.stopped() {
            self.abandon(store);
            self.wake_terminal(wake);
            return;
        }
        let valid = command.address.run == self.run
            && command.address.sequence == self.expected_sequence
            && command.address.action == command.action.address()
            && command.address.acquisition == self.acquisition;
        let result = if valid {
            self.execute(store, command.action)
        } else {
            Err(invalid(
                "Archival worker run/action/acquisition/sequence differs",
            ))
        };
        self.expected_sequence = self.expected_sequence.saturating_add(1);
        let failed = result.is_err();
        if failed {
            if let Ok(mut g) = self.shared.gate.lock() {
                g.publication_failed = true;
            }
            self.abandon(store);
        }
        let mut event = FirmwareBackupEvent {
            address: command.address,
            result,
        };
        let deadline = Instant::now() + Duration::from_millis(250);
        loop {
            {
                let mut gate = match self.shared.gate.lock() {
                    Ok(g) => g,
                    Err(_) => {
                        self.shared.stop("Archival gate poisoned");
                        break;
                    }
                };
                if gate.stopped {
                    break;
                }
                match self.events.try_send(event) {
                    Ok(()) => {
                        gate.settled = true;
                        if failed {
                            gate.stopped = true;
                            gate.terminal = Some(
                                "Archival worker operation failed; no subsequent action accepted"
                                    .into(),
                            );
                        }
                        drop(gate);
                        wake();
                        return;
                    }
                    Err(TrySendError::Full(v)) => event = v,
                    Err(TrySendError::Disconnected(value)) => {
                        gate.stopped = true;
                        gate.terminal = Some("Archival consumer lost".into());
                        drop(gate);
                        drop(value);
                        break;
                    }
                }
            }
            if Instant::now() >= deadline {
                self.shared
                    .stop("Archival event queue exceeded bounded delivery deadline");
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        self.abandon(store);
        self.wake_terminal(wake);
    }
    fn wake_terminal(&mut self, wake: &WakeUi) {
        if !self.terminal_woken {
            self.terminal_woken = true;
            wake();
        }
    }
    fn execute(
        &mut self,
        store: &mut LibraryStore,
        action: FirmwareBackupAction,
    ) -> Result<FirmwareBackupResult, ServiceError> {
        if matches!(
            action,
            FirmwareBackupAction::PrepareCapture(_)
                | FirmwareBackupAction::BeginCaptured(_)
                | FirmwareBackupAction::StoreCaptured(_)
                | FirmwareBackupAction::StoreCaptureBoundary(_)
                | FirmwareBackupAction::StoreCaptureFailure(_)
                | FirmwareBackupAction::CloseCaptured { .. }
                | FirmwareBackupAction::PublishCaptured
                | FirmwareBackupAction::RevalidateCaptured(_)
        ) {
            let result = self.execute_capture(store, action);
            if result.is_err()
                && let Ok(mut g) = self.shared.gate.lock()
            {
                g.publication_failed = true;
            }
            return result;
        }
        if self.capture.is_some() {
            return Err(invalid(
                "Ordinary archival actions cannot bypass an active capture",
            ));
        }
        match action {
            FirmwareBackupAction::Begin(b) => {
                if self.begun {
                    return Err(invalid(
                        "An archival route cannot resume/restart its acquisition",
                    ));
                }
                self.begun = true;
                let draft = store.begin_firmware_backup_review(b.origin, b.context, b.opening)?;
                let acquisition = draft.acquisition().to_owned();
                self.acquisition = Some(acquisition.clone());
                self.draft = Some(draft);
                if let Ok(mut gate) = self.shared.gate.lock() {
                    gate.requires_cleanup = true;
                    gate.archival_begun = true;
                }
                Ok(FirmwareBackupResult::Begun { acquisition })
            }
            FirmwareBackupAction::Store(o) => {
                let pass = o.pass;
                let hash = store.store_firmware_backup_observation(
                    self.draft
                        .as_mut()
                        .ok_or_else(|| invalid("No archival acquisition"))?,
                    o,
                )?;
                Ok(if pass == FirmwareObservationPass::Baseline {
                    FirmwareBackupResult::ObjectStored { raw_hash: hash }
                } else {
                    FirmwareBackupResult::ComparisonStored { raw_hash: hash }
                })
            }
            FirmwareBackupAction::Close { boundary, outcome } => {
                store.close_firmware_backup_review(
                    self.draft
                        .as_mut()
                        .ok_or_else(|| invalid("No archival acquisition"))?,
                    boundary,
                    outcome,
                )?;
                if let Ok(mut gate) = self.shared.gate.lock() {
                    gate.requires_cleanup = false;
                    gate.archival_closed = true;
                }
                Ok(FirmwareBackupResult::Closed)
            }
            FirmwareBackupAction::Publish => {
                let draft = self
                    .draft
                    .take()
                    .ok_or_else(|| invalid("No closed archival acquisition"))?;
                let shared = self.shared.clone();
                let receipt = store.publish_firmware_backup_review_inner(
                    draft,
                    &|| shared.stopped(),
                    &|_| Ok(()),
                )?;
                if let Ok(mut g) = self.shared.gate.lock() {
                    g.archival_published = true;
                }
                Ok(FirmwareBackupResult::PackagePublished(Box::new(receipt)))
            }
            FirmwareBackupAction::Revalidate(receipt) => {
                if self.acquisition.as_deref() != Some(receipt.acquisition()) {
                    return Err(invalid("Local package belongs to another acquisition"));
                }
                receipt.revalidate()?;
                if let Ok(mut g) = self.shared.gate.lock() {
                    g.archival_revalidated = true;
                }
                Ok(FirmwareBackupResult::Revalidated(receipt))
            }
            _ => unreachable!("Capture actions dispatched above"),
        }
    }
    fn abandon(&mut self, store: &mut LibraryStore) {
        let mut cleanup_error = None;
        if let Some(draft) = self.draft.take()
            && !draft.is_closed()
            && let Err(e) =
                store.finish_device_backup(draft.acquisition(), BackupAcquisition::Cancelled)
        {
            cleanup_error = Some(e.message);
        }
        if !self.capture_cleanup_attempted
            && let Some(capture) = self.capture.as_mut()
        {
            self.capture_cleanup_attempted = true;
            let seq = self
                .expected_sequence
                .max(capture.last_worker_sequence().saturating_add(1));
            if let Err(e) = capture.abandon(&self.run, seq) {
                cleanup_error = Some(e.message);
            }
        }
        if let Ok(mut gate) = self.shared.gate.lock() {
            if let Some(e) = cleanup_error {
                gate.cleanup_failed = true;
                gate.terminal = Some(
                    format!("Capture/archival cancellation durability failed: {e}")
                        .chars()
                        .take(4096)
                        .collect(),
                );
            }
            gate.settled = true;
            gate.requires_cleanup = gate.cleanup_failed;
            if self.capture.as_ref().is_some_and(|c| c.closed) {
                gate.capture_closed = true;
            }
            gate.receipt = None;
        }
    }
    pub(crate) fn stop(&mut self, store: &mut LibraryStore) {
        self.shared
            .stop("Library worker stopped; archival reply revoked");
        self.abandon(store);
    }
}

fn retirement_blockers(g: &Gate, unread: usize) -> Vec<&'static str> {
    let mut result = vec![];
    if g.stopped {
        result.push("Archival/capture route revoked");
    }
    if g.archival_begun {
        if !g.archival_closed {
            result.push("Archival acquisition not closed");
        }
        if !g.archival_published {
            result.push("Archival package not published");
        }
        if !g.archival_revalidated {
            result.push("Archival package not revalidated");
        }
        if !g.archival_revalidation_consumed {
            result.push("Archival revalidation acknowledgement not consumed");
        }
    }
    if g.pending.is_some() {
        result.push("Outstanding archival/capture action");
    }
    if unread != 0 {
        result.push("Unread archival/capture result");
    }
    if g.receipt.is_some() {
        result.push("Unconsumed capture durability receipt");
    }
    if g.requires_cleanup {
        result.push("Capture/archival cleanup unresolved");
    }
    if g.cleanup_failed {
        result.push("Sticky capture/archival cleanup failure");
    }
    if g.publication_failed {
        result.push("Sticky capture publication failure");
    }
    if g.capture_aborted {
        result.push("Capture read progression irreversibly aborted");
    }
    if g.capture_prepared {
        if !g.capture_closed {
            result.push("Durable capture preparation not closed");
        }
        if !g.capture_published {
            result.push("Capture package not published");
        }
        if !g.capture_revalidated {
            result.push("Capture package not revalidated");
        }
        if !g.capture_revalidation_consumed {
            result.push("Capture revalidation receipt not consumed");
        }
    }
    result
}
/// Genuine same-writer local durability, never physical or flash authority.
///
/// ```compile_fail
/// use gp200_library::FirmwareBackupDurableReceipt;
/// fn duplicate(receipt: FirmwareBackupDurableReceipt) { let _ = receipt.clone(); }
/// ```
/// ```compile_fail
/// use gp200_library::FirmwareBackupDurableReceipt;
/// let receipt: FirmwareBackupDurableReceipt = serde_json::from_str("{}").unwrap();
/// ```
/// ```compile_fail
/// use gp200_library::FirmwareBackupDurableReceipt;
/// let receipt = FirmwareBackupDurableReceipt {};
/// ```
pub struct FirmwareBackupDurableReceipt {
    description: FirmwareCaptureReceiptDescription,
    shared: Arc<Shared>,
    token: u64,
    consumed: bool,
}
impl std::fmt::Debug for FirmwareBackupDurableReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FirmwareBackupDurableReceipt")
            .field("description", &self.description)
            .finish_non_exhaustive()
    }
}
#[derive(Debug)]
pub enum FirmwareBackupReceiptError {
    /// No operation was invoked; ownership of the same receipt is preserved.
    Busy(Box<FirmwareBackupDurableReceipt>),
    Rejected(ServiceError),
}
enum ReceiptUse {
    Offer,
    Review,
    Abort,
}
impl FirmwareBackupDurableReceipt {
    pub fn description(&self) -> &FirmwareCaptureReceiptDescription {
        &self.description
    }
    /// Linearizes one first offer against stop. `operation` MUST perform exactly one
    /// bounded NONBLOCKING backend write. No legacy send loop, retry, drain, disk,
    /// hash, wake or join belongs here. The later transport adapter enforces this.
    pub fn try_with_first_offer<T>(
        self,
        expected: &FirmwareCaptureReceiptDescription,
        operation: impl FnOnce() -> T,
    ) -> Result<T, FirmwareBackupReceiptError> {
        if !matches!(
            self.description.kind,
            FirmwareCaptureReceiptKind::Prepared
                | FirmwareCaptureReceiptKind::Begun
                | FirmwareCaptureReceiptKind::Observation
                | FirmwareCaptureReceiptKind::Boundary
        ) || matches!(
            self.description.read,
            Some(FirmwareCaptureRead::Identity(
                FirmwareCaptureBoundaryKind::Final
            ))
        ) {
            return Err(FirmwareBackupReceiptError::Rejected(invalid(
                "This capture receipt cannot offer another read",
            )));
        }
        self.consume(expected, ReceiptUse::Offer, operation)
    }
    /// Consumes Closed/Published/Revalidated/Failure or final-boundary evidence
    /// for read-only closure/review, with no backend operation or physical grant.
    pub fn try_consume_for_review(
        self,
        expected: &FirmwareCaptureReceiptDescription,
    ) -> Result<(), FirmwareBackupReceiptError> {
        if !matches!(
            self.description.kind,
            FirmwareCaptureReceiptKind::Closed
                | FirmwareCaptureReceiptKind::Published
                | FirmwareCaptureReceiptKind::Revalidated
                | FirmwareCaptureReceiptKind::Failure
        ) && !matches!(
            self.description.read,
            Some(FirmwareCaptureRead::Identity(
                FirmwareCaptureBoundaryKind::Final
            ))
        ) {
            return Err(FirmwareBackupReceiptError::Rejected(invalid(
                "A next-read capture receipt requires the first-offer fence",
            )));
        }
        self.consume(expected, ReceiptUse::Review, || ())
    }
    /// Irreversibly ends read progression using this genuine one-use receipt.
    /// No backend or storage operation occurs; bounded failure/close actions may follow.
    pub fn try_consume_for_capture_abort(
        self,
        expected: &FirmwareCaptureReceiptDescription,
    ) -> Result<(), FirmwareBackupReceiptError> {
        if !matches!(
            self.description.kind,
            FirmwareCaptureReceiptKind::Prepared
                | FirmwareCaptureReceiptKind::Begun
                | FirmwareCaptureReceiptKind::Observation
                | FirmwareCaptureReceiptKind::Boundary
        ) || matches!(
            self.description.read,
            Some(FirmwareCaptureRead::Identity(
                FirmwareCaptureBoundaryKind::Final
            ))
        ) {
            return Err(FirmwareBackupReceiptError::Rejected(invalid(
                "This receipt cannot abort next-read progression",
            )));
        }
        self.consume(expected, ReceiptUse::Abort, || ())
    }
    fn consume<T>(
        mut self,
        expected: &FirmwareCaptureReceiptDescription,
        usage: ReceiptUse,
        operation: impl FnOnce() -> T,
    ) -> Result<T, FirmwareBackupReceiptError> {
        let shared = self.shared.clone();
        let _admission = if let Some(a) = &shared.admission {
            match a.try_gate() {
                Some(g) if !a.is_revoked() && g.offers_allowed() => Some(g),
                Some(g) if a.is_revoked() || g.permanently_revoked() => {
                    self.consumed = true;
                    return Err(FirmwareBackupReceiptError::Rejected(invalid(
                        "Old writer generation revoked",
                    )));
                }
                _ => return Err(FirmwareBackupReceiptError::Busy(Box::new(self))),
            }
        } else {
            None
        };
        let mut gate = match shared.gate.try_lock() {
            Ok(g) => g,
            Err(TryLockError::WouldBlock) => {
                return Err(FirmwareBackupReceiptError::Busy(Box::new(self)));
            }
            Err(TryLockError::Poisoned(_)) => {
                self.consumed = true;
                return Err(FirmwareBackupReceiptError::Rejected(invalid(
                    "Capture receipt gate poisoned",
                )));
            }
        };
        if self.consumed
            || gate.stopped
            || gate.pending.is_some()
            || matches!(usage, ReceiptUse::Offer) && gate.capture_aborted
            || gate.receipt != Some(self.token)
            || &self.description != expected
        {
            gate.stopped = true;
            gate.terminal =
                Some("Capture durability receipt revoked or correlation differs".into());
            gate.receipt = None;
            self.consumed = true;
            return Err(FirmwareBackupReceiptError::Rejected(ServiceError::new(
                ErrorKind::Indeterminate,
                "Capture durability receipt revoked or correlation differs",
            )));
        }
        gate.receipt = None;
        if matches!(usage, ReceiptUse::Abort) {
            gate.capture_aborted = true;
        }
        self.consumed = true;
        if self.description.kind == FirmwareCaptureReceiptKind::Revalidated {
            gate.capture_revalidation_consumed = true;
        }
        // The consuming transition and exactly one bounded operation share stop's gate.
        let result = operation();
        drop(gate);
        Ok(result)
    }
}
impl Drop for FirmwareBackupDurableReceipt {
    fn drop(&mut self) {
        if !self.consumed {
            self.shared
                .stop("Capture durability receipt abandoned before consumption");
        }
    }
}
impl FirmwareBackupWorkerRoute {
    fn capture_receipt(
        &mut self,
        kind: FirmwareCaptureReceiptKind,
        evidence: Option<&FirmwareCaptureEvidence>,
        hash: ContentHash,
        publication: Option<Box<PublishedFirmwareCaptureEvidence>>,
    ) -> Result<FirmwareBackupResult, ServiceError> {
        let capture = self
            .capture
            .as_ref()
            .ok_or_else(|| invalid("No prepared capture"))?;
        let description = FirmwareCaptureReceiptDescription {
            worker_run: self.run.clone(),
            worker_sequence: self.expected_sequence,
            capture_id: capture.id.clone(),
            acquisition: self.acquisition.clone(),
            reservation: capture.reservation.clone(),
            kind,
            read: evidence.map(|e| e.read.clone()),
            observation_sequence: evidence.map(|e| e.sequence),
            raw_sha256: ContentHash::of(evidence.map_or(&[][..], |e| e.raw.as_slice()))
                .as_str()
                .into(),
            evidence_sha256: hash.as_str().into(),
        };
        let mut gate = self
            .shared
            .gate
            .lock()
            .map_err(|_| invalid("Capture receipt gate poisoned"))?;
        if gate.stopped || gate.receipt.is_some() {
            return Err(ServiceError::new(
                ErrorKind::Indeterminate,
                "Capture receipt revoked before issuance",
            ));
        }
        gate.receipt = Some(self.expected_sequence);
        drop(gate);
        Ok(FirmwareBackupResult::CaptureDurable(Box::new(
            FirmwareCaptureDurableResult {
                receipt: FirmwareBackupDurableReceipt {
                    description,
                    shared: self.shared.clone(),
                    token: self.expected_sequence,
                    consumed: false,
                },
                publication,
            },
        )))
    }
    fn execute_capture(
        &mut self,
        store: &mut LibraryStore,
        action: FirmwareBackupAction,
    ) -> Result<FirmwareBackupResult, ServiceError> {
        if self.capture.as_ref().is_some_and(|c| c.failed)
            && !matches!(
                action,
                FirmwareBackupAction::CloseCaptured { .. }
                    | FirmwareBackupAction::PublishCaptured
                    | FirmwareBackupAction::RevalidateCaptured(_)
            )
        {
            return Err(invalid("Retained capture failure closes read progression"));
        }
        if self
            .last_identity
            .as_ref()
            .is_some_and(|(kind, _, _)| *kind == FirmwareCaptureBoundaryKind::Final)
            && !matches!(
                action,
                FirmwareBackupAction::CloseCaptured { .. }
                    | FirmwareBackupAction::StoreCaptureFailure(_)
                    | FirmwareBackupAction::PublishCaptured
                    | FirmwareBackupAction::RevalidateCaptured(_)
            )
        {
            return Err(invalid(
                "No completed capture progression after Final identity",
            ));
        }
        match action {
            FirmwareBackupAction::PrepareCapture(reservation) => {
                if self.begun || self.capture.is_some() {
                    return Err(invalid("Capture preparation cannot restart or resume"));
                }
                self.capture = Some(CaptureDraft::prepare(
                    store,
                    &self.run,
                    self.expected_sequence,
                    reservation,
                )?);
                {
                    let mut g = self
                        .shared
                        .gate
                        .lock()
                        .map_err(|_| invalid("Capture gate poisoned"))?;
                    g.capture_prepared = true;
                    g.requires_cleanup = true;
                }
                let hash = self
                    .capture
                    .as_ref()
                    .expect("prepared above")
                    .last_record_hash()?;
                self.capture_receipt(FirmwareCaptureReceiptKind::Prepared, None, hash, None)
            }
            FirmwareBackupAction::BeginCaptured(b) => {
                let e = &b.evidence;
                e.validate()?;
                let c = self
                    .capture
                    .as_ref()
                    .ok_or_else(|| invalid("Capture must prepare before identity"))?;
                e.validate_for_reservation(&c.reservation)?;
                if self.begun
                    || !e.complete
                    || e.read != FirmwareCaptureRead::Identity(FirmwareCaptureBoundaryKind::Opening)
                    || e.raw != b.begin.opening.raw_identity
                    || e.sequence != b.begin.opening.sequence
                    || b.begin.origin.epoch != c.reservation.epoch
                {
                    return Err(invalid("Capture opening identity correlation differs"));
                }
                crate::firmware_backup_capture_evidence::validate_capture_boundary(
                    &b.begin.origin,
                    e,
                )?;
                self.begun = true;
                self.capture_origin = Some(b.begin.origin.clone());
                let draft = store.begin_firmware_backup_review(
                    b.begin.origin,
                    b.begin.context,
                    b.begin.opening,
                )?;
                self.acquisition = Some(draft.acquisition().to_owned());
                self.draft = Some(draft);
                let hash = self.capture.as_mut().expect("prepared above").append(
                    &self.run,
                    self.expected_sequence,
                    self.acquisition.as_deref(),
                    FirmwareCaptureReceiptKind::Begun,
                    Some(e.clone()),
                    false,
                )?;
                self.last_identity = Some((
                    FirmwareCaptureBoundaryKind::Opening,
                    e.sequence,
                    ContentHash::of(&e.raw).as_str().into(),
                ));
                self.capture_receipt(FirmwareCaptureReceiptKind::Begun, Some(e), hash, None)
            }
            FirmwareBackupAction::StoreCaptured(o) => {
                let e = &o.evidence;
                e.validate()?;
                let c = self
                    .capture
                    .as_ref()
                    .ok_or_else(|| invalid("No prepared capture"))?;
                e.validate_for_reservation(&c.reservation)?;
                if !e.complete
                    || e.read
                        != (FirmwareCaptureRead::Object {
                            object: o.observation.object,
                            pass: o.observation.pass,
                        })
                    || e.sequence != o.observation.sequence
                    || e.raw != o.observation.bytes
                    || o.observation.epoch != c.reservation.epoch
                {
                    return Err(invalid(
                        "Capture/archive object/pass/raw correlation differs",
                    ));
                }
                store.store_firmware_backup_observation(
                    self.draft
                        .as_mut()
                        .ok_or_else(|| invalid("No capture archive"))?,
                    o.observation,
                )?;
                let hash = self.capture.as_mut().expect("prepared above").append(
                    &self.run,
                    self.expected_sequence,
                    self.acquisition.as_deref(),
                    FirmwareCaptureReceiptKind::Observation,
                    Some(e.clone()),
                    false,
                )?;
                self.capture_receipt(FirmwareCaptureReceiptKind::Observation, Some(e), hash, None)
            }
            FirmwareBackupAction::StoreCaptureBoundary(e) => {
                e.validate()?;
                if !e.complete {
                    return Err(invalid(
                        "Capture boundary incomplete; preserve it as failure",
                    ));
                }
                let origin = self
                    .capture_origin
                    .as_ref()
                    .ok_or_else(|| invalid("Capture identity not begun"))?;
                crate::firmware_backup_capture_evidence::validate_capture_boundary(origin, &e)?;
                if e.read == FirmwareCaptureRead::Identity(FirmwareCaptureBoundaryKind::Opening) {
                    return Err(invalid("Capture opening identity cannot repeat"));
                }
                let hash = self
                    .capture
                    .as_mut()
                    .ok_or_else(|| invalid("No prepared capture"))?
                    .append(
                        &self.run,
                        self.expected_sequence,
                        self.acquisition.as_deref(),
                        FirmwareCaptureReceiptKind::Boundary,
                        Some((*e).clone()),
                        false,
                    )?;
                if let FirmwareCaptureRead::Identity(kind) = e.read {
                    self.last_identity =
                        Some((kind, e.sequence, ContentHash::of(&e.raw).as_str().into()));
                }
                self.capture_receipt(FirmwareCaptureReceiptKind::Boundary, Some(&e), hash, None)
            }
            FirmwareBackupAction::StoreCaptureFailure(e) => {
                e.validate()?;
                if e.complete {
                    return Err(invalid("Failure evidence cannot claim a completed read"));
                }
                let hash = self
                    .capture
                    .as_mut()
                    .ok_or_else(|| invalid("No prepared capture"))?
                    .append(
                        &self.run,
                        self.expected_sequence,
                        self.acquisition.as_deref(),
                        FirmwareCaptureReceiptKind::Failure,
                        Some((*e).clone()),
                        false,
                    )?;
                self.capture_receipt(FirmwareCaptureReceiptKind::Failure, Some(&e), hash, None)
            }
            FirmwareBackupAction::CloseCaptured { boundary, outcome } => {
                if outcome == BackupAcquisition::Finished
                    && self
                        .last_identity
                        .as_ref()
                        .is_none_or(|(kind, _, _)| *kind != FirmwareCaptureBoundaryKind::Final)
                {
                    return Err(invalid(
                        "Finished capture requires the exact durable Final identity",
                    ));
                }
                if self
                    .last_identity
                    .as_ref()
                    .is_none_or(|(_, sequence, hash)| {
                        *sequence != boundary.sequence
                            || hash != ContentHash::of(&boundary.raw_identity).as_str()
                    })
                {
                    return Err(invalid(
                        "Capture close requires the exact durably recorded identity boundary",
                    ));
                }
                let cancelled = outcome != BackupAcquisition::Finished;
                if !cancelled && self.capture.as_ref().is_some_and(|c| c.failed) {
                    return Err(invalid(
                        "Failed capture cannot finish as complete acquisition",
                    ));
                }
                store.close_firmware_backup_review(
                    self.draft
                        .as_mut()
                        .ok_or_else(|| invalid("No capture archive"))?,
                    boundary,
                    outcome,
                )?;
                let hash = self
                    .capture
                    .as_mut()
                    .ok_or_else(|| invalid("No prepared capture"))?
                    .append(
                        &self.run,
                        self.expected_sequence,
                        self.acquisition.as_deref(),
                        FirmwareCaptureReceiptKind::Closed,
                        None,
                        cancelled,
                    )?;
                {
                    let mut g = self
                        .shared
                        .gate
                        .lock()
                        .map_err(|_| invalid("Capture gate poisoned"))?;
                    g.capture_closed = true;
                    g.requires_cleanup = false;
                }
                self.capture_receipt(FirmwareCaptureReceiptKind::Closed, None, hash, None)
            }
            FirmwareBackupAction::PublishCaptured => {
                let result = (|| {
                    let draft = self
                        .draft
                        .take()
                        .ok_or_else(|| invalid("No closed capture archive"))?;
                    let shared = self.shared.clone();
                    let archive = store.publish_firmware_backup_review_inner(
                        draft,
                        &|| shared.stopped(),
                        &|_| Ok(()),
                    )?;
                    self.capture
                        .as_ref()
                        .ok_or_else(|| invalid("No prepared capture"))?
                        .publish(Box::new(archive))
                })();
                let publication = match result {
                    Ok(p) => p,
                    Err(e) => {
                        if let Ok(mut g) = self.shared.gate.lock() {
                            g.publication_failed = true;
                        }
                        return Err(e);
                    }
                };
                let hash = ContentHash::new(&publication.summary().index_sha256)?;
                {
                    let mut g = self
                        .shared
                        .gate
                        .lock()
                        .map_err(|_| invalid("Capture gate poisoned"))?;
                    g.capture_published = true;
                }
                self.capture_receipt(
                    FirmwareCaptureReceiptKind::Published,
                    None,
                    hash,
                    Some(Box::new(publication)),
                )
            }
            FirmwareBackupAction::RevalidateCaptured(publication) => {
                if self.acquisition.as_deref() != publication.summary().acquisition.as_deref()
                    || self
                        .capture
                        .as_ref()
                        .is_none_or(|c| c.id != publication.summary().capture_id)
                {
                    return Err(invalid("Capture publication belongs to another run"));
                }
                if let Err(e) = publication.revalidate() {
                    if let Ok(mut g) = self.shared.gate.lock() {
                        g.publication_failed = true;
                    }
                    return Err(e);
                }
                let hash = ContentHash::new(&publication.summary().index_sha256)?;
                {
                    let mut g = self
                        .shared
                        .gate
                        .lock()
                        .map_err(|_| invalid("Capture gate poisoned"))?;
                    g.capture_revalidated = true;
                    g.capture_revalidation_consumed = false;
                }
                self.capture_receipt(
                    FirmwareCaptureReceiptKind::Revalidated,
                    None,
                    hash,
                    Some(publication),
                )
            }
            _ => Err(invalid("Not a capture action")),
        }
    }
}

#[cfg(test)]
mod route_tests {
    use super::*;
    fn temp() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("fw04-route-{}", nonce().unwrap()));
        std::fs::create_dir(&p).unwrap();
        p
    }
    #[test]
    fn wake_callback_can_reenter_stop_gate() {
        let dir = temp();
        let mut store = LibraryStore::open(&dir).unwrap();
        let (mut client, mut route) = channel().unwrap();
        let shared = client.shared.clone();
        let wake: WakeUi = Arc::new(move || {
            let mut g = shared
                .gate
                .try_lock()
                .expect("wake must run outside publication gate");
            g.stopped = true;
        });
        client
            .try_submit(FirmwareBackupAction::Publish)
            .ok()
            .unwrap();
        route.tick(&mut store, &ShutdownToken::default(), &wake);
        assert!(client.terminal_failure().is_some());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn forged_run_and_sequence_are_rejected_by_actual_worker() {
        let dir = temp();
        let mut store = LibraryStore::open(&dir).unwrap();
        let (mut client, mut route) = channel().unwrap();
        client
            .try_submit(FirmwareBackupAction::Publish)
            .ok()
            .unwrap();
        let mut command = route.commands.try_recv().unwrap();
        command.address.run = "foreign".into();
        command.address.sequence = 77;
        client.commands.try_send(command).ok().unwrap();
        let wake: WakeUi = Arc::new(|| {});
        route.tick(&mut store, &ShutdownToken::default(), &wake);
        assert!(client.shared.stopped());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn full_event_rail_stops_with_terminal_cell() {
        let dir = temp();
        let mut store = LibraryStore::open(&dir).unwrap();
        let (mut client, mut route) = channel().unwrap();
        let address = client
            .try_submit(FirmwareBackupAction::Publish)
            .ok()
            .unwrap();
        for _ in 0..FIRMWARE_BACKUP_EVENT_CAPACITY {
            route
                .events
                .try_send(FirmwareBackupEvent {
                    address: address.clone(),
                    result: Ok(FirmwareBackupResult::Closed),
                })
                .ok()
                .unwrap();
        }
        let started = Instant::now();
        let wake: WakeUi = Arc::new(|| {});
        route.tick(&mut store, &ShutdownToken::default(), &wake);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(client.terminal_failure().unwrap().contains("deadline"));
        assert!(client.storage_settled());
        assert!(client.try_next_event().unwrap().result.is_err());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod capture_route_tests {
    use super::*;
    use std::{
        fs,
        sync::{
            Barrier,
            atomic::{AtomicUsize, Ordering},
        },
    };
    fn wake() -> WakeUi {
        Arc::new(|| {})
    }
    pub(super) fn reservation() -> FirmwareCaptureReservation {
        FirmwareCaptureReservation {
            actor_run: "actual route actor".into(),
            reservation: "actual route reservation".into(),
            normal_owner: "actual route guard".into(),
            epoch: SessionEpoch(1),
            usb: FirmwareUsbObservation {
                topology: "1-2".into(),
                vendor: 0x84ef,
                product: 0x2a,
                serial: "fixed serial".into(),
                interface: "midi".into(),
                endpoint: "raw".into(),
            },
            clock_origin: "actual route clock".into(),
            prepared_monotonic_ns: 1,
            prepared_wall_millis: 2,
            initial_fence: FirmwareCaptureFence {
                allocated: 0,
                processed: 0,
            },
        }
    }
    fn temp() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("fw04e-route-{}", nonce().unwrap()));
        fs::create_dir(&dir).unwrap();
        dir
    }
    fn prepare(
        store: &mut LibraryStore,
        c: &mut FirmwareBackupClient,
        r: &mut FirmwareBackupWorkerRoute,
    ) -> FirmwareBackupDurableReceipt {
        c.try_submit(FirmwareBackupAction::PrepareCapture(reservation()))
            .ok()
            .unwrap();
        r.tick(store, &ShutdownToken::default(), &wake());
        match c.try_next_event().unwrap().result {
            Ok(FirmwareBackupResult::CaptureDurable(value)) => value.receipt,
            _ => panic!("Missing actual route receipt"),
        }
    }
    #[test]
    fn busy_first_offer_fence_preserves_receipt_and_invokes_operation_once() {
        let dir = temp();
        let mut store = LibraryStore::open(&dir).unwrap();
        let (mut c, mut route) = channel().unwrap();
        let receipt = prepare(&mut store, &mut c, &mut route);
        let expected = receipt.description().clone();
        let shared = c.shared.clone();
        let gate = shared.gate.lock().unwrap();
        let count = AtomicUsize::new(0);
        let receipt = match receipt.try_with_first_offer(&expected, || {
            count.fetch_add(1, Ordering::SeqCst);
        }) {
            Err(FirmwareBackupReceiptError::Busy(r)) => *r,
            _ => panic!("Busy fence did not retain receipt"),
        };
        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(gate);
        receipt
            .try_with_first_offer(&expected, || {
                assert!(shared.gate.try_lock().is_err());
                count.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(c.shared.gate.lock().unwrap().receipt.is_none());
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn concurrent_stop_and_first_offer_have_one_gate_order() {
        for _ in 0..20 {
            let dir = temp();
            let mut store = LibraryStore::open(&dir).unwrap();
            let (mut c, mut route) = channel().unwrap();
            let receipt = prepare(&mut store, &mut c, &mut route);
            let expected = receipt.description().clone();
            let barrier = Arc::new(Barrier::new(2));
            let count = Arc::new(AtomicUsize::new(0));
            let offer_order = Arc::new(AtomicUsize::new(0));
            let cb = barrier.clone();
            let cc = count.clone();
            let co = offer_order.clone();
            let offer = std::thread::spawn(move || {
                cb.wait();
                receipt.try_with_first_offer(&expected, || {
                    co.store(cc.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                })
            });
            barrier.wait();
            c.cancel();
            let stopped_order = count.fetch_add(1, Ordering::SeqCst) + 1;
            let result = offer.join().unwrap();
            let actual_offer = offer_order.load(Ordering::SeqCst);
            if actual_offer != 0 {
                assert!(result.is_ok());
                assert!(actual_offer < stopped_order);
            } else {
                assert!(result.is_err());
            }
            route.tick(&mut store, &ShutdownToken::default(), &wake());
            drop(store);
            fs::remove_dir_all(dir).unwrap();
        }
    }
    #[test]
    fn private_test_replay_and_wrong_worker_receipt_cannot_offer() {
        let dir = temp();
        let mut store = LibraryStore::open(&dir).unwrap();
        let (mut c, mut route) = channel().unwrap();
        let receipt = prepare(&mut store, &mut c, &mut route);
        let expected = receipt.description().clone();
        let replay = FirmwareBackupDurableReceipt {
            description: expected.clone(),
            shared: receipt.shared.clone(),
            token: receipt.token,
            consumed: false,
        };
        receipt.try_with_first_offer(&expected, || ()).unwrap();
        let mut offered = false;
        assert!(
            replay
                .try_with_first_offer(&expected, || offered = true)
                .is_err()
        );
        assert!(!offered);
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn every_actual_capture_publication_fault_blocks_receipts_and_retirement() {
        for stage in [
            "directory-sync",
            "before-create",
            "after-write",
            "after-file-sync",
            "reread",
            "after-link",
            "before-cleanup",
        ] {
            let dir = temp();
            let mut store = LibraryStore::open(&dir).unwrap();
            let (mut c, mut route) = channel().unwrap();
            crate::firmware_backup_capture_evidence::set_fault(Some(stage));
            c.try_submit(FirmwareBackupAction::PrepareCapture(reservation()))
                .ok()
                .unwrap();
            route.tick(&mut store, &ShutdownToken::default(), &wake());
            crate::firmware_backup_capture_evidence::set_fault(None);
            assert!(c.try_next_event().unwrap().result.is_err(), "{stage}");
            assert!(
                route
                    .retirement_blockers()
                    .iter()
                    .any(|s| s.contains("publication failure")),
                "{stage}"
            );
            assert!(c.shared.gate.lock().unwrap().receipt.is_none());
            drop(store);
            fs::remove_dir_all(dir).unwrap();
        }
    }
    #[test]
    fn failed_actual_cleanup_remains_blocked_when_storage_settled() {
        let dir = temp();
        let mut store = LibraryStore::open(&dir).unwrap();
        let (mut c, mut route) = channel().unwrap();
        let receipt = prepare(&mut store, &mut c, &mut route);
        let expected = receipt.description().clone();
        receipt.try_with_first_offer(&expected, || ()).unwrap();
        crate::firmware_backup_capture_evidence::set_fault(Some("after-write"));
        c.cancel();
        route.tick(&mut store, &ShutdownToken::default(), &wake());
        crate::firmware_backup_capture_evidence::set_fault(None);
        assert!(c.storage_settled());
        assert!(
            route
                .retirement_blockers()
                .iter()
                .any(|s| s.contains("cleanup failure"))
        );
        assert!(
            route
                .retirement_blockers()
                .iter()
                .any(|s| s.contains("cleanup unresolved"))
        );
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn genuine_abort_preserves_busy_receipt_then_durably_records_failure_without_next_offer() {
        let dir = temp();
        let mut store = LibraryStore::open(&dir).unwrap();
        let (mut c, mut route) = channel().unwrap();
        let receipt = prepare(&mut store, &mut c, &mut route);
        let expected = receipt.description().clone();
        let shared = c.shared.clone();
        let held = shared.gate.lock().unwrap();
        let receipt = match receipt.try_consume_for_capture_abort(&expected) {
            Err(FirmwareBackupReceiptError::Busy(r)) => r,
            _ => panic!("Busy must retain the genuine receipt"),
        };
        drop(held);
        receipt.try_consume_for_capture_abort(&expected).unwrap();
        assert!(shared.gate.lock().unwrap().capture_aborted);
        assert!(matches!(
            c.try_submit(FirmwareBackupAction::PrepareCapture(reservation())),
            Err(SubmitError::Disconnected(_))
        ));
        let failure = FirmwareCaptureEvidence {
            read: FirmwareCaptureRead::Identity(FirmwareCaptureBoundaryKind::Opening),
            sequence: 1,
            before: FirmwareCaptureFence {
                allocated: 0,
                processed: 0,
            },
            after: FirmwareCaptureFence {
                allocated: 0,
                processed: 0,
            },
            offer: None,
            offered_read_bytes: vec![],
            completed_ns: None,
            callbacks: vec![],
            missing: vec![FirmwareCaptureMissingExtent {
                first_ordinal: 1,
                last_ordinal: 1,
                reason: "Aborted before any next read offer".into(),
            }],
            raw: vec![],
            complete: false,
            diagnostics: vec!["Actual abort retained without a fake offer".into()],
        };
        c.try_submit(FirmwareBackupAction::StoreCaptureFailure(Box::new(failure)))
            .ok()
            .unwrap();
        route.tick(&mut store, &ShutdownToken::default(), &wake());
        let receipt = match c.try_next_event().unwrap().result.unwrap() {
            FirmwareBackupResult::CaptureDurable(v) => v.receipt,
            _ => panic!(),
        };
        let expected = receipt.description().clone();
        receipt.try_consume_for_review(&expected).unwrap();
        assert!(route.capture.as_ref().unwrap().failed);
        assert!(
            route
                .retirement_blockers()
                .iter()
                .any(|s| s.contains("irreversibly aborted"))
        );
        assert!(!shared.gate.lock().unwrap().stopped);
        drop(c);
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod capture_mismatched_result_test {
    use super::*;
    #[test]
    fn genuine_mismatched_durable_result_drops_receipt_outside_gate() {
        let dir = std::env::temp_dir().join(format!("fw04e-mismatch-{}", nonce().unwrap()));
        std::fs::create_dir(&dir).unwrap();
        let mut store = LibraryStore::open(&dir).unwrap();
        let (mut c, mut route) = channel().unwrap();
        let reservation = FirmwareCaptureReservation {
            actor_run: "actor".into(),
            reservation: "reservation".into(),
            normal_owner: "guard".into(),
            epoch: SessionEpoch(1),
            usb: FirmwareUsbObservation {
                topology: "1-2".into(),
                vendor: 0x84ef,
                product: 0x2a,
                serial: "serial".into(),
                interface: "midi".into(),
                endpoint: "raw".into(),
            },
            clock_origin: "clock".into(),
            prepared_monotonic_ns: 1,
            prepared_wall_millis: 1,
            initial_fence: FirmwareCaptureFence {
                allocated: 0,
                processed: 0,
            },
        };
        c.try_submit(FirmwareBackupAction::PrepareCapture(reservation))
            .ok()
            .unwrap();
        let wake: WakeUi = Arc::new(|| {});
        route.tick(&mut store, &ShutdownToken::default(), &wake);
        let mut event = c.events.try_recv().unwrap();
        assert!(matches!(
            event.result,
            Ok(FirmwareBackupResult::CaptureDurable(_))
        ));
        event.address.sequence += 1;
        route.events.try_send(event).ok().unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            assert!(c.try_next_event().is_none());
            assert!(c.terminal_failure().unwrap().contains("Mismatched"));
            tx.send(()).unwrap();
            c
        });
        rx.recv_timeout(Duration::from_secs(1))
            .expect("Receipt-drop deadlock");
        let c = thread.join().unwrap();
        assert!(!c.capture_retirement_blockers().is_empty());
        route.tick(&mut store, &ShutdownToken::default(), &wake);
        drop(c);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod capture_publication_identity_test {
    use super::*;
    #[test]
    fn identical_unsynced_replacement_cannot_obtain_genuine_worker_receipt() {
        let dir = std::env::temp_dir().join(format!("fw04e-inode-{}", nonce().unwrap()));
        std::fs::create_dir(&dir).unwrap();
        let mut store = LibraryStore::open(&dir).unwrap();
        let (mut c, mut route) = channel().unwrap();
        let reservation = FirmwareCaptureReservation {
            actor_run: "actor".into(),
            reservation: "reservation".into(),
            normal_owner: "guard".into(),
            epoch: SessionEpoch(1),
            usb: FirmwareUsbObservation {
                topology: "1-2".into(),
                vendor: 0x84ef,
                product: 0x2a,
                serial: "serial".into(),
                interface: "midi".into(),
                endpoint: "raw".into(),
            },
            clock_origin: "clock".into(),
            prepared_monotonic_ns: 1,
            prepared_wall_millis: 1,
            initial_fence: FirmwareCaptureFence {
                allocated: 0,
                processed: 0,
            },
        };
        let copy = dir.clone();
        crate::firmware_backup_capture_evidence::set_mutation("after-file-sync", move || {
            let parent = std::fs::read_dir(copy.join("firmware-capture-evidence"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            let temp = std::fs::read_dir(&parent)
                .unwrap()
                .find_map(|e| {
                    let p = e.unwrap().path();
                    p.file_name()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .starts_with(".capture-")
                        .then_some(p)
                })
                .unwrap();
            let bytes = std::fs::read(&temp).unwrap();
            std::fs::rename(&temp, parent.join("original-synced-inode.retained")).unwrap();
            std::fs::write(&temp, bytes).unwrap();
        });
        c.try_submit(FirmwareBackupAction::PrepareCapture(reservation))
            .ok()
            .unwrap();
        let wake: WakeUi = Arc::new(|| {});
        route.tick(&mut store, &ShutdownToken::default(), &wake);
        let error = c.try_next_event().unwrap().result.err().unwrap();
        assert!(error.message.contains("inode identity changed"));
        assert!(
            route
                .retirement_blockers()
                .iter()
                .any(|b| b.contains("publication failure"))
        );
        assert!(c.shared.gate.lock().unwrap().receipt.is_none());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod capture_no_clobber_test {
    use super::*;
    #[test]
    fn actual_capture_writer_never_replaces_existing_final_record() {
        let dir = std::env::temp_dir().join(format!("fw04e-no-clobber-{}", nonce().unwrap()));
        std::fs::create_dir(&dir).unwrap();
        let mut store = LibraryStore::open(&dir).unwrap();
        let (mut c, mut route) = channel().unwrap();
        let copy = dir.clone();
        crate::firmware_backup_capture_evidence::set_mutation("after-file-sync", move || {
            let parent = std::fs::read_dir(copy.join("firmware-capture-evidence"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            std::fs::write(
                parent.join("record-0000.json"),
                b"existing independently supplied occupant",
            )
            .unwrap();
        });
        c.try_submit(FirmwareBackupAction::PrepareCapture(
            super::capture_route_tests::reservation(),
        ))
        .ok()
        .unwrap();
        let wake: WakeUi = Arc::new(|| {});
        route.tick(&mut store, &ShutdownToken::default(), &wake);
        assert!(c.try_next_event().unwrap().result.is_err());
        let parent = std::fs::read_dir(dir.join("firmware-capture-evidence"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            std::fs::read(parent.join("record-0000.json")).unwrap(),
            b"existing independently supplied occupant"
        );
        assert!(c.shared.gate.lock().unwrap().receipt.is_none());
        assert!(
            route
                .retirement_blockers()
                .iter()
                .any(|b| b.contains("publication failure"))
        );
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
