//! Canonical read-only acquisition on the sole normal worker. Original bytes
//! and host order are evidence, never device causality, atomicity or flash proof.
use crate::backend::{NormalCallbackSource, ObservedCallback};
use crate::{DeviceActor, MidiSession, TransportConfig};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use gp200_domain::*;
use gp200_library::{
    BackupAcquisition, BackupOrigin, ContentHash, FIRMWARE_CAPTURE_EVIDENCE_SCHEMA,
    FirmwareArchiveBoundary, FirmwareArchiveObservation, FirmwareBackupAction, FirmwareBackupBegin,
    FirmwareBackupDurableReceipt, FirmwareBackupReceiptError, FirmwareCaptureDurableResult,
    FirmwareCaptureRecordInspection, FirmwareCapturedBegin, FirmwareCapturedObservation,
    firmware_capture_record_hash,
};
use gp200_protocol::{
    AssetBank, AssetSlotMetadata, GlobalRecords, Identity, RawTransfer, Reassembler, StreamDecoder,
    StreamEvent, WireContext,
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const FIRMWARE_CAPTURE_CONTROL_CAPACITY: usize = 8;
pub const FIRMWARE_CAPTURE_EVENT_CAPACITY: usize = 8;
const SOFT_DEADLINE: Duration = Duration::from_secs(600);
static RUN: AtomicU64 = AtomicU64::new(1);
fn invalid(message: impl Into<String>) -> ServiceError {
    ServiceError::invalid(message)
}
fn time_ns(origin: Instant) -> Result<u64, ServiceError> {
    u64::try_from(origin.elapsed().as_nanos()).map_err(|_| invalid("Normal source clock exhausted"))
}
fn wall_millis() -> Result<u64, ServiceError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid("Wall clock precedes epoch"))?;
    u64::try_from(elapsed.as_millis()).map_err(|_| invalid("Wall clock exhausted"))
}

/// Private system provenance can only be created by the concrete raw backend
/// and completed by its actual guard. A custom backend may declare replay only.
/// ```compile_fail
/// use gp200_transport::FirmwareNormalCaptureObservation;
/// use gp200_domain::{FirmwareUsbObservation,FirmwareCaptureOrigin};
/// fn forge(usb: FirmwareUsbObservation) {
///     let _ = FirmwareNormalCaptureObservation { usb, owner: "fake".into(), origin: FirmwareCaptureOrigin::GuardedSystemNormal, guarded: true };
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareNormalCaptureObservation {
    pub(crate) usb: FirmwareUsbObservation,
    pub(crate) owner: String,
    pub(crate) origin: FirmwareCaptureOrigin,
    pub(crate) guarded: bool,
}
impl FirmwareNormalCaptureObservation {
    pub fn replay(
        usb: FirmwareUsbObservation,
        owner: impl Into<String>,
    ) -> Result<Self, ServiceError> {
        usb.validate()?;
        let owner = owner.into();
        if owner.is_empty() || owner.len() > 256 || owner.chars().any(char::is_control) {
            return Err(invalid("Replay owner label exceeds bounds"));
        }
        Ok(Self {
            usb,
            owner,
            origin: FirmwareCaptureOrigin::Replay,
            guarded: false,
        })
    }
    pub fn source(&self) -> FirmwareCaptureOrigin {
        self.origin
    }
    pub fn usb(&self) -> &FirmwareUsbObservation {
        &self.usb
    }
    pub(crate) fn admit(&self, mode: BackendMode) -> Result<(), ServiceError> {
        self.usb.validate()?;
        match (self.origin, self.guarded, mode) {
            (FirmwareCaptureOrigin::GuardedSystemNormal, true, BackendMode::Hardware)
            | (
                FirmwareCaptureOrigin::Replay,
                false,
                BackendMode::CaptureReplay | BackendMode::Simulator,
            ) => Ok(()),
            _ => Err(ServiceError::new(
                ErrorKind::Unverified,
                "Capture requires actual guarded normal provenance or explicitly labelled replay",
            )),
        }
    }
}
#[derive(Debug)]
pub enum FirmwareCaptureWriteResult {
    /// The single OS call reported this exact byte extent, never a device ACK.
    Known {
        accepted: usize,
        error: Option<ServiceError>,
    },
    Unknown(ServiceError),
}

pub enum FirmwareBackupCaptureCommand {
    Begin(FirmwareBackupReviewContext),
    ContinueAfterDurableObservation(FirmwareBackupDurableReceipt),
    CloseAfterBoundary(FirmwareBackupDurableReceipt),
    AcceptRevalidatedReview(FirmwareCaptureDurableResult),
    ReturnToNormal,
}
impl std::fmt::Debug for FirmwareBackupCaptureCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Begin(_) => f.write_str("Begin"),
            Self::ContinueAfterDurableObservation(r) => f
                .debug_tuple("ContinueAfterDurableObservation")
                .field(r)
                .finish(),
            Self::CloseAfterBoundary(r) => f.debug_tuple("CloseAfterBoundary").field(r).finish(),
            Self::AcceptRevalidatedReview(r) => f
                .debug_tuple("AcceptRevalidatedReview")
                .field(&r.receipt)
                .finish(),
            Self::ReturnToNormal => f.write_str("ReturnToNormal"),
        }
    }
}
#[derive(Debug)]
pub enum FirmwareBackupCaptureEvent {
    Prepared {
        reservation: FirmwareCaptureReservation,
        origin: FirmwareCaptureOrigin,
    },
    Observation {
        evidence: FirmwareCaptureEvidence,
        action: Box<FirmwareBackupActionDescription>,
        consistency: Option<FirmwareCaptureConsistency>,
    },
    Failed {
        error: ServiceError,
        evidence: Option<Box<FirmwareCaptureEvidence>>,
    },
    CloseReady {
        boundary: FirmwareArchiveBoundary,
        outcome: BackupAcquisition,
    },
    Closed {
        reservation: FirmwareCaptureReservation,
    },
    Rejected(ServiceError),
    /// Passive revocation preserves the entire bounded original event.
    Revoked {
        error: ServiceError,
        original: Box<FirmwareBackupCaptureEvent>,
    },
    /// Stopped-only recovery. At most the one outstanding action and its abort
    /// record; no successful action/receipt or complete late-reader claim.
    RetainedAfterStop {
        error: ServiceError,
        reservation: FirmwareCaptureReservation,
        originals: Vec<FirmwareCaptureEvidence>,
        observed_frontier: FirmwareCaptureFence,
        unretained_suffix: Option<FirmwareCaptureMissingExtent>,
        limitation: String,
    },
    /// Late original callback after failure publication; passive, never another archive action.
    RejectedCallback {
        callback: FirmwareCaptureCallback,
        after: FirmwareCaptureFence,
        error: ServiceError,
    },
    ReviewAccepted {
        reservation: FirmwareCaptureReservation,
    },
    NormalRefreshRequired {
        retired_epoch: SessionEpoch,
        next_epoch: SessionEpoch,
        selection: Option<DeviceSelection>,
    },
}
/// Descriptions used to submit the already existing sole library writer.
#[derive(Debug)]
pub enum FirmwareBackupActionDescription {
    Opening {
        origin: BackupOrigin,
        context: Box<FirmwareBackupReviewContext>,
        boundary: Box<FirmwareArchiveBoundary>,
    },
    Object(FirmwareArchiveObservation),
    Boundary,
}
impl FirmwareBackupCaptureEvent {
    pub fn into_library_action(self) -> Result<FirmwareBackupAction, ServiceError> {
        match self {
            Self::Prepared { reservation, .. } => {
                Ok(FirmwareBackupAction::PrepareCapture(reservation))
            }
            Self::Observation {
                evidence, action, ..
            } => match *action {
                FirmwareBackupActionDescription::Opening {
                    origin,
                    context,
                    boundary,
                } => Ok(FirmwareBackupAction::BeginCaptured(Box::new(
                    FirmwareCapturedBegin {
                        begin: FirmwareBackupBegin {
                            origin,
                            context: *context,
                            opening: *boundary,
                        },
                        evidence,
                    },
                ))),
                FirmwareBackupActionDescription::Object(observation) => Ok(
                    FirmwareBackupAction::StoreCaptured(Box::new(FirmwareCapturedObservation {
                        observation,
                        evidence,
                    })),
                ),
                FirmwareBackupActionDescription::Boundary => Ok(
                    FirmwareBackupAction::StoreCaptureBoundary(Box::new(evidence)),
                ),
            },
            Self::Failed {
                evidence: Some(evidence),
                ..
            } => Ok(FirmwareBackupAction::StoreCaptureFailure(evidence)),
            Self::CloseReady { boundary, outcome } => {
                Ok(FirmwareBackupAction::CloseCaptured { boundary, outcome })
            }
            _ => Err(invalid("Capture event contains no archival action")),
        }
    }
}
pub(crate) struct CaptureShared {
    pub(crate) active: AtomicBool,
    pub(crate) closed: AtomicBool,
    stopped: AtomicBool,
    cancelled: AtomicBool,
    /// Only short admission/one nonblocking offer is held here, never disk/join/wake.
    offer_gate: Mutex<()>,
    terminal: Mutex<Option<ServiceError>>,
    retained: Mutex<Option<FirmwareBackupCaptureEvent>>,
    retained_source: Mutex<Option<Arc<NormalCallbackSource>>>,
    processed: AtomicU64,
}
impl CaptureShared {
    pub(crate) fn fail(&self, error: ServiceError) {
        self.stopped.store(true, Ordering::Release);
        if let Ok(mut terminal) = self.terminal.try_lock()
            && terminal.is_none()
        {
            *terminal = Some(error);
        }
    }
    fn retain(
        &self,
        event: FirmwareBackupCaptureEvent,
    ) -> Result<(), Box<FirmwareBackupCaptureEvent>> {
        let Ok(mut retained) = self.retained.try_lock() else {
            return Err(Box::new(event));
        };
        if retained.is_some() {
            return Err(Box::new(event));
        }
        *retained = Some(event);
        Ok(())
    }
    pub(crate) fn take_retained(&self) -> Option<FirmwareBackupCaptureEvent> {
        if !self.stopped.load(Ordering::Acquire) {
            return None;
        }
        let mut event = self.retained.try_lock().ok()?.take()?;
        if let FirmwareBackupCaptureEvent::RetainedAfterStop {
            observed_frontier,
            unretained_suffix,
            ..
        } = &mut event
        {
            if let Ok(source) = self.retained_source.try_lock()
                && let Some(source) = source.as_ref()
            {
                let now = source.fence(self.processed.load(Ordering::Acquire));
                if now.allocated > observed_frontier.allocated {
                    *unretained_suffix=Some(FirmwareCaptureMissingExtent{first_ordinal:observed_frontier.allocated.saturating_add(1),last_ordinal:now.allocated,reason:"Original callbacks allocated after this passive snapshot are outside its retained material; no final joined-reader completeness claim".into()});
                }
            }
            return Some(event);
        }
        Some(FirmwareBackupCaptureEvent::Revoked {
            error: self
                .terminal
                .try_lock()
                .ok()
                .and_then(|v| v.clone())
                .unwrap_or_else(|| {
                    ServiceError::new(
                        ErrorKind::Indeterminate,
                        "Stopped publication retained passively",
                    )
                }),
            original: Box::new(event),
        })
    }
    pub(crate) fn blocks_normal(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
    pub(crate) fn blocks_firmware(&self) -> bool {
        self.blocks_normal() && !self.closed.load(Ordering::Acquire)
    }
    pub(crate) fn stop(&self) {
        self.fail(ServiceError::new(
            ErrorKind::Cancelled,
            "Capture consumer/normal service stopped",
        ));
    }
}
/// Once-extracted sole capture consumer. Dropping it revokes pending work.
/// ```compile_fail
/// use gp200_transport::FirmwareBackupCaptureClient;
/// fn duplicate(c: FirmwareBackupCaptureClient) { let _ = c.clone(); }
/// ```
pub struct FirmwareBackupCaptureClient {
    commands: Sender<FirmwareBackupCaptureCommand>,
    events: Receiver<FirmwareBackupCaptureEvent>,
    shared: Arc<CaptureShared>,
}
impl FirmwareBackupCaptureClient {
    pub fn try_submit(
        &self,
        command: FirmwareBackupCaptureCommand,
    ) -> Result<(), SubmitError<FirmwareBackupCaptureCommand>> {
        if self.shared.stopped.load(Ordering::Acquire) {
            return Err(SubmitError::Disconnected(command));
        }
        let Ok(_gate) = self.shared.offer_gate.try_lock() else {
            return Err(SubmitError::Busy(command));
        };
        let begin = matches!(&command, FirmwareBackupCaptureCommand::Begin(_));
        if begin && self.shared.active.swap(true, Ordering::AcqRel) {
            return Err(SubmitError::Busy(command));
        }
        if !begin && !self.shared.blocks_normal() {
            return Err(SubmitError::Busy(command));
        }
        match self.commands.try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(c)) => {
                if begin {
                    self.shared.active.store(false, Ordering::Release);
                }
                Err(SubmitError::Busy(c))
            }
            Err(TrySendError::Disconnected(c)) => {
                self.shared.stop();
                Err(SubmitError::Disconnected(c))
            }
        }
    }
    pub fn try_next_event(&self) -> Option<FirmwareBackupCaptureEvent> {
        let Some(event) = self.events.try_recv().ok() else {
            return self.shared.take_retained();
        };
        if self.shared.stopped.load(Ordering::Acquire)
            && matches!(
                &event,
                FirmwareBackupCaptureEvent::Prepared { .. }
                    | FirmwareBackupCaptureEvent::Observation { .. }
                    | FirmwareBackupCaptureEvent::CloseReady { .. }
                    | FirmwareBackupCaptureEvent::Closed { .. }
                    | FirmwareBackupCaptureEvent::ReviewAccepted { .. }
                    | FirmwareBackupCaptureEvent::Failed { .. }
            )
        {
            return Some(FirmwareBackupCaptureEvent::Revoked {
                error: self.terminal_failure().unwrap_or_else(|| {
                    ServiceError::new(ErrorKind::Indeterminate, "Capture reply revoked")
                }),
                original: Box::new(event),
            });
        }
        Some(event)
    }
    /// Revokes future offers at the worker's final atomic check. Cancellation
    /// racing an offer already past that check may follow one attempted write;
    /// original evidence remains uncertain and no retry follows.
    pub fn cancel(&self) {
        self.shared.cancelled.store(true, Ordering::Release);
        // No wait on a worker, filesystem, reader or provider. Cancel is observed
        // again under the actual first-offer gate on the normal worker.
    }
    pub fn terminal_failure(&self) -> Option<ServiceError> {
        self.shared.terminal.try_lock().ok().and_then(|v| v.clone())
    }
}
impl Drop for FirmwareBackupCaptureClient {
    fn drop(&mut self) {
        self.shared.stop();
    }
}
pub(crate) struct CaptureWorkerRail {
    pub(crate) commands: Receiver<FirmwareBackupCaptureCommand>,
    events: Sender<FirmwareBackupCaptureEvent>,
    pub(crate) shared: Arc<CaptureShared>,
    blocked_event: Option<(FirmwareBackupCaptureEvent, Instant)>,
}
pub(crate) fn channel() -> (FirmwareBackupCaptureClient, CaptureWorkerRail) {
    let (commands, rx) = bounded(FIRMWARE_CAPTURE_CONTROL_CAPACITY);
    let (tx, events) = bounded(FIRMWARE_CAPTURE_EVENT_CAPACITY);
    let shared = Arc::new(CaptureShared {
        active: AtomicBool::new(false),
        closed: AtomicBool::new(false),
        stopped: AtomicBool::new(false),
        cancelled: AtomicBool::new(false),
        offer_gate: Mutex::new(()),
        terminal: Mutex::new(None),
        retained: Mutex::new(None),
        retained_source: Mutex::new(None),
        processed: AtomicU64::new(0),
    });
    (
        FirmwareBackupCaptureClient {
            commands,
            events,
            shared: shared.clone(),
        },
        CaptureWorkerRail {
            commands: rx,
            events: tx,
            shared,
            blocked_event: None,
        },
    )
}
impl CaptureWorkerRail {
    pub(crate) fn reply(&mut self, event: FirmwareBackupCaptureEvent, wake: &WakeUi) {
        if self.blocked_event.is_some() {
            self.shared.fail(invalid("Multiple unresolved capture result publications; actual run retains outstanding original evidence"));
            return;
        }
        if self.shared.stopped.load(Ordering::Acquire) {
            if let Err(event) = self.shared.retain(event) {
                self.blocked_event = Some((*event, Instant::now()));
            }
            return;
        }
        match self.events.try_send(event) {
            Ok(()) => wake(),
            Err(TrySendError::Full(e)) => self.blocked_event = Some((e, Instant::now())),
            Err(TrySendError::Disconnected(event)) => {
                self.shared.stop();
                self.blocked_event = Some((event, Instant::now()));
            }
        }
    }
    pub(crate) fn flush(&mut self, actor: &mut DeviceActor, wake: &WakeUi) {
        if self.shared.stopped.load(Ordering::Acquire) {
            actor.capture_abort(ServiceError::new(
                ErrorKind::Cancelled,
                "Capture route stopped; originals retained passively",
            ));
            if let Some((event, started)) = self.blocked_event.take()
                && let Err(event) = self.shared.retain(event)
            {
                self.blocked_event = Some((*event, started));
            }
            if self.blocked_event.is_none()
                && let Some(run) = actor.capture.as_mut()
                && !run.terminal_snapshot_retained
            {
                let snapshot = run.terminal_snapshot();
                if self.shared.retain(snapshot).is_ok() {
                    run.terminal_snapshot_retained = true;
                }
            }
            return;
        }
        if let Some((event, started)) = self.blocked_event.take() {
            match self.events.try_send(event) {
                Ok(()) => wake(),
                Err(TrySendError::Full(event))
                    if started.elapsed() < Duration::from_millis(250) =>
                {
                    self.blocked_event = Some((event, started))
                }
                Err(TrySendError::Full(event) | TrySendError::Disconnected(event)) => {
                    self.shared.fail(ServiceError::new(ErrorKind::Indeterminate,"Capture result rail lost/full past bounded publication deadline; original retained"));
                    self.blocked_event = Some((event, started));
                }
            }
        }
        if self.shared.stopped.load(Ordering::Acquire) {
            actor.capture_abort(ServiceError::new(
                ErrorKind::Cancelled,
                "Capture route stopped",
            ));
        }
        if self.blocked_event.is_none()
            && let Some(run) = actor.capture.as_mut()
            && let Some(event) = run.events.pop_front()
        {
            self.reply(event, wake);
        }
    }
    pub(crate) fn quiescent(&self) -> bool {
        self.commands.is_empty() && self.events.is_empty() && self.blocked_event.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Prepared,
    WaitingDurable,
    Scheduled,
    Reading,
    AwaitClosed,
    Closed,
    Failed,
}
pub(crate) struct CaptureRun {
    reservation: FirmwareCaptureReservation,
    pub(crate) binding: FirmwareNormalCaptureObservation,
    context: FirmwareBackupReviewContext,
    mode: BackendMode,
    strict_equal: bool,
    conditional_163_profile: bool,
    source: Arc<NormalCallbackSource>,
    shared: Arc<CaptureShared>,
    schedule: Vec<FirmwareCaptureRead>,
    index: usize,
    phase: Phase,
    origin: Instant,
    started: Instant,
    next_allowed: Duration,
    timeout: Duration,
    spacing: Duration,
    decoder: StreamDecoder,
    assembler: Reassembler,
    next_offset: usize,
    total: Option<usize>,
    evidence: Option<FirmwareCaptureEvidence>,
    waiting_evidence: Option<FirmwareCaptureEvidence>,
    waiting_kind: FirmwareCaptureReceiptKind,
    receipt: Option<FirmwareBackupDurableReceipt>,
    receipt_description: Option<FirmwareCaptureReceiptDescription>,
    worker_run: Option<String>,
    capture_id: Option<String>,
    acquisition: Option<String>,
    worker_sequence: u64,
    prior_hash: String,
    baseline: BTreeMap<FirmwareBackupObject, Vec<u8>>,
    opening_identity: Option<Vec<u8>>,
    last_identity: Option<FirmwareArchiveBoundary>,
    deadline: Option<Duration>,
    raw_completed: Option<RawTransfer>,
    pub(crate) events: VecDeque<FirmwareBackupCaptureEvent>,
    failed: bool,
    queued_offer: bool,
    last_processed: u64,
    unexpected: Vec<FirmwareCaptureCallback>,
    pending_failure: Option<(ServiceError, FirmwareCaptureEvidence)>,
    return_request: Option<FirmwareCaptureDurableResult>,
    return_ready: bool,
    accepted_review: bool,
    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
    entry_review_index: Option<String>,
    terminal_snapshot_retained: bool,
}
fn schedule(nam: bool) -> Vec<FirmwareCaptureRead> {
    use FirmwareBackupObject::*;
    use FirmwareCaptureBoundaryKind::*;
    use FirmwareObservationPass::*;
    let obj = |object, pass| FirmwareCaptureRead::Object { object, pass };
    let mut out = vec![
        FirmwareCaptureRead::Identity(Opening),
        obj(Current, Baseline),
        obj(Globals, Baseline),
    ];
    for pass in [Baseline, Comparison] {
        for i in 0..256 {
            out.push(obj(Stored(i), pass));
        }
        for i in 0..20 {
            out.push(obj(IrMetadata(i), pass));
        }
        if nam {
            for i in 0..10 {
                out.push(obj(SnapToneMetadata(i), pass));
            }
        }
        if pass == Baseline {
            out.push(FirmwareCaptureRead::Boundary {
                boundary: Middle,
                object: Current,
            });
            out.push(FirmwareCaptureRead::Boundary {
                boundary: Middle,
                object: Globals,
            });
            out.push(FirmwareCaptureRead::Identity(Middle));
        }
    }
    out.push(obj(Current, Comparison));
    out.push(obj(Globals, Comparison));
    out.push(FirmwareCaptureRead::Identity(Final));
    out
}
pub(crate) struct CaptureAdmission<'a> {
    pub(crate) epoch: SessionEpoch,
    pub(crate) mode: BackendMode,
    pub(crate) config: &'a TransportConfig,
    pub(crate) binding: FirmwareNormalCaptureObservation,
    pub(crate) context: FirmwareBackupReviewContext,
    pub(crate) source: Arc<NormalCallbackSource>,
    pub(crate) processed: u64,
    pub(crate) shared: Arc<CaptureShared>,
    pub(crate) now: Duration,
}
impl CaptureRun {
    pub(crate) fn new(admission: CaptureAdmission<'_>) -> Result<Self, ServiceError> {
        let CaptureAdmission {
            epoch,
            mode,
            config,
            binding,
            context,
            source,
            processed,
            shared,
            now,
        } = admission;
        context.validate()?;
        binding.admit(mode)?;
        if context
            .device
            .usb_identity()
            .is_some_and(|(vendor, product, serial)| {
                vendor != binding.usb.vendor
                    || product != binding.usb.product
                    || serial != binding.usb.serial
            })
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Reported USB device association differs from actual normal capture binding",
            ));
        }
        let conditional_163_profile = context.manual_version.as_ref().is_some_and(|report| {
            report.suggested_metadata_profile(&context.device)
                == Some(EditorMetadataProfile::Gp200Editor160)
        });
        let nam = !matches!(
            context.snaptone_applicability(),
            FirmwareAssetApplicability::ConditionalNotApplicable { .. }
        );
        if nam && binding.origin == FirmwareCaptureOrigin::GuardedSystemNormal {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Automatic NAM applicability/profile unavailable; a reported pre-1.7.4 version supports conditional absence only",
            ));
        }
        if let Ok(mut retained) = shared.retained_source.try_lock() {
            *retained = Some(source.clone());
        } else {
            return Err(ServiceError::new(
                ErrorKind::Busy,
                "Capture source retention cell busy",
            ));
        }
        shared.processed.store(processed, Ordering::Release);
        let fence = source.fence(processed);
        if fence.allocated != fence.processed || source.failed() {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Normal callback frontier is unresolved",
            ));
        }
        let id = RUN.fetch_add(1, Ordering::AcqRel);
        if id == u64::MAX {
            return Err(invalid("Capture actor run exhausted"));
        }
        let origin = source.origin();
        let reservation = FirmwareCaptureReservation {
            actor_run: format!("normal-{}-{id}", std::process::id()),
            reservation: format!("capture-{}-{id}", std::process::id()),
            normal_owner: binding.owner.clone(),
            epoch,
            usb: binding.usb.clone(),
            clock_origin: source.clock_origin().into(),
            prepared_monotonic_ns: time_ns(origin)?,
            prepared_wall_millis: wall_millis()?,
            initial_fence: fence,
        };
        reservation.validate()?;
        let mut events = VecDeque::new();
        events.push_back(FirmwareBackupCaptureEvent::Prepared {
            reservation: reservation.clone(),
            origin: binding.origin,
        });
        Ok(Self {
            reservation,
            binding,
            context,
            mode,
            strict_equal: true,
            conditional_163_profile,
            source,
            shared,
            schedule: schedule(nam),
            index: 0,
            phase: Phase::Prepared,
            origin,
            started: Instant::now(),
            next_allowed: now,
            timeout: config.request_timeout,
            spacing: config.minimum_request_spacing,
            decoder: StreamDecoder::new(),
            assembler: Reassembler::new(config.reassembly_timeout),
            next_offset: 0,
            total: None,
            evidence: None,
            waiting_evidence: None,
            waiting_kind: FirmwareCaptureReceiptKind::Prepared,
            receipt: None,
            receipt_description: None,
            worker_run: None,
            capture_id: None,
            acquisition: None,
            worker_sequence: 0,
            prior_hash: ContentHash::of(&[]).as_str().into(),
            baseline: BTreeMap::new(),
            opening_identity: None,
            last_identity: None,
            deadline: None,
            raw_completed: None,
            events,
            failed: false,
            queued_offer: false,
            last_processed: processed,
            unexpected: vec![],
            pending_failure: None,
            return_request: None,
            return_ready: false,
            accepted_review: false,
            #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
            entry_review_index: None,
            terminal_snapshot_retained: false,
        })
    }
    pub(crate) fn closed(&self) -> bool {
        self.phase == Phase::Closed
            && self.accepted_review
            && self.events.is_empty()
            && self.receipt.is_none()
            && self.evidence.is_none()
            && !self.source.failed()
            && self.source.fence(self.last_processed).allocated == self.last_processed
    }
    fn expected(
        &mut self,
        actual: &FirmwareCaptureReceiptDescription,
        kind: FirmwareCaptureReceiptKind,
        cancelled: bool,
    ) -> Result<FirmwareCaptureReceiptDescription, ServiceError> {
        if actual.reservation != self.reservation || actual.kind != kind {
            return Err(invalid("Capture durability reservation/kind differs"));
        }
        if self.worker_run.is_none() && kind == FirmwareCaptureReceiptKind::Prepared {
            self.worker_run = Some(actual.worker_run.clone());
            self.capture_id = Some(actual.capture_id.clone());
            self.worker_sequence = actual
                .worker_sequence
                .checked_sub(1)
                .ok_or_else(|| invalid("Capture worker sequence starts at zero"))?;
        }
        if kind == FirmwareCaptureReceiptKind::Begun && self.acquisition.is_none() {
            self.acquisition = actual.acquisition.clone();
            if self.acquisition.is_none() {
                return Err(invalid("Begun capture lacks actual acquisition"));
            }
        }
        let sequence = self
            .worker_sequence
            .checked_add(1)
            .ok_or_else(|| invalid("Worker sequence exhausted"))?;
        let record = FirmwareCaptureRecordInspection {
            schema: FIRMWARE_CAPTURE_EVIDENCE_SCHEMA.into(),
            capture_id: self
                .capture_id
                .clone()
                .ok_or_else(|| invalid("Capture id not anchored"))?,
            worker_run: self
                .worker_run
                .clone()
                .ok_or_else(|| invalid("Worker run not anchored"))?,
            worker_sequence: sequence,
            acquisition: self.acquisition.clone(),
            kind,
            previous_sha256: self.prior_hash.clone(),
            preparation: (kind == FirmwareCaptureReceiptKind::Prepared)
                .then(|| self.reservation.clone()),
            evidence: self.waiting_evidence.clone(),
            cancelled,
        };
        let hash = firmware_capture_record_hash(&record)?;
        let expected = FirmwareCaptureReceiptDescription {
            worker_run: record.worker_run,
            worker_sequence: sequence,
            capture_id: record.capture_id,
            acquisition: record.acquisition,
            reservation: self.reservation.clone(),
            kind,
            read: self.waiting_evidence.as_ref().map(|e| e.read.clone()),
            observation_sequence: self.waiting_evidence.as_ref().map(|e| e.sequence),
            raw_sha256: ContentHash::of(
                self.waiting_evidence
                    .as_ref()
                    .map_or(&[][..], |e| e.raw.as_slice()),
            )
            .as_str()
            .into(),
            evidence_sha256: hash.as_str().into(),
        };
        if &expected != actual {
            return Err(invalid(
                "Capture genuine receipt differs from independently retained action/original evidence/hash",
            ));
        }
        Ok(expected)
    }
    pub(crate) fn continue_receipt(
        &mut self,
        receipt: FirmwareBackupDurableReceipt,
    ) -> Result<(), ServiceError> {
        if !matches!(
            self.phase,
            Phase::Prepared | Phase::WaitingDurable | Phase::Failed
        ) || self.receipt.is_some()
        {
            return Err(invalid("No one outstanding capture durability action"));
        }
        let expected = self.expected(receipt.description(), self.waiting_kind, false)?;
        self.receipt_description = Some(expected);
        self.receipt = Some(receipt);
        if self.pending_failure.is_some() {
            return self.consume_abort();
        }
        if self.waiting_kind == FirmwareCaptureReceiptKind::Failure
            || self.waiting_evidence.as_ref().is_some_and(|e| {
                e.read == FirmwareCaptureRead::Identity(FirmwareCaptureBoundaryKind::Final)
            })
        {
            self.consume_review(false)?;
        } else {
            self.phase = Phase::Scheduled;
        }
        Ok(())
    }
    fn accept_receipt(&mut self) {
        let d = self.receipt_description.take().expect("validated receipt");
        self.worker_sequence = d.worker_sequence;
        self.prior_hash = d.evidence_sha256;
        self.waiting_evidence = None;
    }
    fn consume_review(&mut self, closed: bool) -> Result<(), ServiceError> {
        let receipt = self
            .receipt
            .take()
            .ok_or_else(|| invalid("Missing genuine capture receipt"))?;
        let expected = self
            .receipt_description
            .as_ref()
            .expect("validated receipt");
        match receipt.try_consume_for_review(expected) {
            Ok(()) => {
                self.accept_receipt();
                if closed {
                    self.phase = Phase::Closed;
                    self.events.push_back(FirmwareBackupCaptureEvent::Closed {
                        reservation: self.reservation.clone(),
                    });
                } else {
                    self.phase = Phase::AwaitClosed;
                    if let Some(boundary) = self.last_identity.clone() {
                        self.events.push_back(FirmwareBackupCaptureEvent::CloseReady{boundary,outcome:if self.failed {BackupAcquisition::Cancelled}else if !self.strict_equal {BackupAcquisition::Failed{reason:"Both raw passes observed; exact archival equality differs; separate source-backed consistency admitted originals".into()}}else{BackupAcquisition::Finished}});
                    } else {
                        self.events.push_back(FirmwareBackupCaptureEvent::Rejected(ServiceError::new(ErrorKind::Unverified,"No durably observed identity boundary; retain partial evidence/cleanup without inventing a publishable archive")));
                    }
                }
                Ok(())
            }
            Err(FirmwareBackupReceiptError::Busy(r)) => {
                self.receipt = Some(*r);
                Ok(())
            }
            Err(FirmwareBackupReceiptError::Rejected(e)) => {
                // The one-use receipt is gone. Stop this route and keep the
                // actual run-held originals available through passive recovery;
                // waiting for a replacement proof would strand the capture.
                self.shared.fail(e.clone());
                Err(e)
            }
        }
    }
    fn consume_abort(&mut self) -> Result<(), ServiceError> {
        let receipt = self
            .receipt
            .take()
            .ok_or_else(|| invalid("Missing genuine abort receipt"))?;
        let expected = self
            .receipt_description
            .as_ref()
            .expect("retained expected preceding evidence");
        // A durably recorded Final identity already forbids another completed
        // read. Its genuine receipt is review-only, including when cancellation
        // arrived before the caller returned that receipt. Earlier read receipts
        // require A0's sticky capture-abort consumption instead.
        let final_identity = self.waiting_evidence.as_ref().is_some_and(|e| {
            e.read == FirmwareCaptureRead::Identity(FirmwareCaptureBoundaryKind::Final)
        });
        let result = if final_identity {
            receipt.try_consume_for_review(expected)
        } else {
            receipt.try_consume_for_capture_abort(expected)
        };
        match result {
            Ok(()) => {
                self.accept_receipt();
                let (error, evidence) = self
                    .pending_failure
                    .take()
                    .expect("actual pending original failure");
                if final_identity {
                    // Keep post-final originals passively. Final is the last
                    // archival observation; no subsequent failure observation
                    // receives library durability or substitutes a boundary.
                    self.evidence = Some(evidence);
                    self.phase = Phase::AwaitClosed;
                    let boundary = self.last_identity.clone().ok_or_else(|| {
                        invalid("Final cancellation lacks its original identity boundary")
                    })?;
                    self.events
                        .push_back(FirmwareBackupCaptureEvent::CloseReady {
                            boundary,
                            outcome: BackupAcquisition::Cancelled,
                        });
                } else {
                    self.waiting_kind = FirmwareCaptureReceiptKind::Failure;
                    self.waiting_evidence = Some(evidence.clone());
                    self.events.push_back(FirmwareBackupCaptureEvent::Failed {
                        error,
                        evidence: Some(Box::new(evidence)),
                    });
                }
                Ok(())
            }
            Err(FirmwareBackupReceiptError::Busy(r)) => {
                self.receipt = Some(*r);
                Ok(())
            }
            Err(FirmwareBackupReceiptError::Rejected(e)) => {
                // The one-use receipt is gone. Stop this route and keep the
                // actual run-held originals available through passive recovery;
                // waiting for a replacement proof would strand the capture.
                self.shared.fail(e.clone());
                Err(e)
            }
        }
    }
    pub(crate) fn tick_durability(&mut self) -> Result<(), ServiceError> {
        self.try_normal_return()?;
        if self.receipt.is_none() {
            return Ok(());
        }
        if self.pending_failure.is_some() {
            return self.consume_abort();
        }
        if self.phase == Phase::AwaitClosed {
            return self.consume_review(true);
        }
        if self.waiting_kind == FirmwareCaptureReceiptKind::Failure
            || self.waiting_evidence.as_ref().is_some_and(|e| {
                e.read == FirmwareCaptureRead::Identity(FirmwareCaptureBoundaryKind::Final)
            })
        {
            return self.consume_review(false);
        }
        Ok(())
    }
    pub(crate) fn close_receipt(
        &mut self,
        receipt: FirmwareBackupDurableReceipt,
    ) -> Result<(), ServiceError> {
        if self.phase != Phase::AwaitClosed || self.receipt.is_some() {
            return Err(invalid(
                "Capture is not waiting for genuine Closed durability",
            ));
        }
        self.waiting_kind = FirmwareCaptureReceiptKind::Closed;
        let expected = self.expected(
            receipt.description(),
            FirmwareCaptureReceiptKind::Closed,
            self.failed || !self.strict_equal,
        )?;
        self.receipt_description = Some(expected);
        self.receipt = Some(receipt);
        self.consume_review(true)
    }
    pub(crate) fn accept_revalidated_review(
        &mut self,
        result: FirmwareCaptureDurableResult,
    ) -> Result<(), ServiceError> {
        if self.phase != Phase::Closed
            || self.failed
            || self.accepted_review
            || self.return_request.is_some()
        {
            return Err(invalid(
                "Return requires actual successful Closed capture plus fresh published/revalidated evidence",
            ));
        }
        self.return_request = Some(result);
        self.try_normal_return()
    }
    pub(crate) fn try_normal_return(&mut self) -> Result<(), ServiceError> {
        let Some(result) = self.return_request.take() else {
            return Ok(());
        };
        let publication = result
            .publication
            .as_ref()
            .ok_or_else(|| invalid("Revalidated return lacks genuine sealed publication"))?;
        let summary = publication.summary();
        if Some(&summary.capture_id) != self.capture_id.as_ref()
            || summary.acquisition != self.acquisition
            || !summary.closed
            || summary.cancelled != (self.failed || !self.strict_equal)
            || publication.archive().summary().context != self.context
        {
            return Err(invalid(
                "Revalidated capture publication/run/context differs",
            ));
        }
        let expected = FirmwareCaptureReceiptDescription {
            worker_run: self
                .worker_run
                .clone()
                .ok_or_else(|| invalid("Actual worker not anchored"))?,
            worker_sequence: self
                .worker_sequence
                .checked_add(2)
                .ok_or_else(|| invalid("Closed/Published/Revalidated sequence exhausted"))?,
            capture_id: self.capture_id.clone().expect("anchored capture"),
            acquisition: self.acquisition.clone(),
            reservation: self.reservation.clone(),
            kind: FirmwareCaptureReceiptKind::Revalidated,
            read: None,
            observation_sequence: None,
            raw_sha256: ContentHash::of(&[]).as_str().into(),
            evidence_sha256: summary.index_sha256.clone(),
        };
        if result.receipt.description() != &expected {
            return Err(invalid(
                "Return receipt is not exact Closed + Published + Revalidated same-writer sequence/index",
            ));
        }
        match result.receipt.try_consume_for_review(&expected) {
            Ok(()) => {
                self.accepted_review = true;
                #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
                {
                    self.entry_review_index = Some(summary.index_sha256.clone());
                }
                self.shared.closed.store(true, Ordering::Release);
                self.events
                    .push_back(FirmwareBackupCaptureEvent::ReviewAccepted {
                        reservation: self.reservation.clone(),
                    });
                Ok(())
            }
            Err(FirmwareBackupReceiptError::Busy(receipt)) => {
                self.return_request = Some(FirmwareCaptureDurableResult {
                    receipt: *receipt,
                    publication: result.publication,
                });
                Ok(())
            }
            Err(FirmwareBackupReceiptError::Rejected(error)) => Err(error),
        }
    }
    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
    pub(crate) fn entry_study_matches(
        &self,
        reservation: &FirmwareCaptureReservation,
        acquisition: &str,
        index: &str,
    ) -> bool {
        self.phase == Phase::Closed
            && self.accepted_review
            && !self.failed
            && !self.shared.cancelled.load(Ordering::SeqCst)
            && !self.shared.stopped.load(Ordering::SeqCst)
            && self.return_request.is_none()
            && !self.return_ready
            && &self.reservation == reservation
            && self.acquisition.as_deref() == Some(acquisition)
            && self.entry_review_index.as_deref() == Some(index)
            && !self.source.failed()
            && self.source.fence(self.last_processed).allocated == self.last_processed
    }
    pub(crate) fn request_normal_return(&mut self) -> Result<(), ServiceError> {
        if !self.accepted_review
            || self.phase != Phase::Closed
            || self.failed
            || self.return_request.is_some()
        {
            return Err(invalid(
                "Return requires privately accepted current-run Revalidated review",
            ));
        }
        self.return_ready = true;
        Ok(())
    }
    pub(crate) fn return_ready(&self) -> bool {
        self.return_ready
    }
    pub(crate) fn return_preflight(&self, processed: u64) -> Result<(), ServiceError> {
        if !self.return_ready
            || self.source.failed()
            || self.source.fence(processed).allocated != processed
        {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Normal return has an unresolved original callback frontier/close",
            ));
        }
        Ok(())
    }
    pub(crate) fn processed(&self) -> u64 {
        self.last_processed
    }
    pub(crate) fn is_failed(&self) -> bool {
        self.failed
    }
    pub(crate) fn queue_offer(&mut self, now: Duration) -> Option<(String, u64)> {
        if !self.due(now) || self.queued_offer {
            return None;
        }
        self.queued_offer = true;
        Some((
            self.reservation.reservation.clone(),
            (self.index + 1) as u64,
        ))
    }
    pub(crate) fn matches_offer(&mut self, reservation: &str, sequence: u64) -> bool {
        self.queued_offer = false;
        self.phase == Phase::Scheduled
            && reservation == self.reservation.reservation
            && sequence == (self.index + 1) as u64
    }
    pub(crate) fn due(&self, now: Duration) -> bool {
        self.phase == Phase::Scheduled && now >= self.next_allowed
    }
    pub(crate) fn offer(
        &mut self,
        opened: &mut dyn MidiSession,
        processed: u64,
        now: Duration,
        shutdown: &ShutdownToken,
    ) -> Result<(), ServiceError> {
        if !self.due(now) {
            return Ok(());
        }
        if self.started.elapsed() > SOFT_DEADLINE {
            return Err(ServiceError::new(
                ErrorKind::Timeout,
                "Capture soft deadline stopped new reads",
            ));
        }
        if opened.normal_capture_observation()? != self.binding {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Fresh guarded owner/USB association changed",
            ));
        }
        opened.health()?;
        let before = self.source.fence(processed);
        if self.source.failed()
            || before.allocated != before.processed
            || self.decoder.is_incomplete()
            || self.assembler.progress().is_some()
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Unexpected/unaccounted callback before actual capture offer",
            ));
        }
        let read = self.schedule[self.index].clone();
        let packet = read.request_bytes()?;
        let shared = self.shared.clone();
        let Ok(_gate) = shared.offer_gate.try_lock() else {
            return Ok(());
        };
        if shared.cancelled.load(Ordering::Acquire)
            || shared.stopped.load(Ordering::Acquire)
            || shutdown.cancelled()
        {
            return Err(ServiceError::new(
                ErrorKind::Cancelled,
                "Capture cancelled before actual offer",
            ));
        }
        let receipt = self
            .receipt
            .take()
            .ok_or_else(|| invalid("Read lacks genuine durability receipt"))?;
        let expected = self
            .receipt_description
            .as_ref()
            .expect("validated receipt");
        let source = self.source.clone();
        let origin = self.origin;
        let attempted = receipt.try_with_first_offer(expected, || {
            // Bounded final source check and exactly one nonblocking raw write.
            if source.failed() || source.fence(processed) != before {
                return Err(ServiceError::new(
                    ErrorKind::Conflict,
                    "Callback allocation changed at actual first-offer boundary",
                ));
            }
            let started = time_ns(origin)?;
            let result = opened.capture_write_once(&packet);
            let returned = time_ns(origin)?;
            Ok((started, returned, result))
        });
        match attempted {
            Err(FirmwareBackupReceiptError::Busy(receipt)) => {
                self.receipt = Some(*receipt);
                return Ok(());
            }
            Err(FirmwareBackupReceiptError::Rejected(error)) => {
                self.phase = Phase::Reading;
                return Err(error);
            }
            Ok(result) => {
                self.accept_receipt();
                self.phase = Phase::Reading;
                let (started, returned, result) = result?;
                let (accepted, diagnostic, known) = match result {
                    FirmwareCaptureWriteResult::Known { accepted, error }
                        if accepted <= packet.len() =>
                    {
                        (accepted, error, true)
                    }
                    FirmwareCaptureWriteResult::Known { accepted, .. } => (
                        0,
                        Some(invalid(format!(
                            "Host offer extent INVALID; accepted_bytes=0 is not a known extent; backend reported impossible first-write count {accepted}"
                        ))),
                        false,
                    ),
                    FirmwareCaptureWriteResult::Unknown(e) => (
                        0,
                        Some(ServiceError::new(
                            e.kind,
                            format!(
                                "Host offer extent UNKNOWN; accepted_bytes=0 is not a known byte-count claim: {}",
                                e.message
                            ),
                        )),
                        false,
                    ),
                };
                self.evidence = Some(FirmwareCaptureEvidence {
                    read,
                    sequence: (self.index + 1) as u64,
                    before,
                    after: before,
                    offer: Some(FirmwareCaptureOfferInterval {
                        started_ns: started,
                        returned_ns: returned,
                        accepted_bytes: accepted as u64,
                        requested_bytes: packet.len() as u64,
                    }),
                    offered_read_bytes: packet,
                    completed_ns: None,
                    callbacks: vec![],
                    missing: vec![],
                    raw: vec![],
                    complete: false,
                    diagnostics: vec![],
                });
                self.next_allowed = Duration::from_nanos(started) + self.spacing;
                if !known
                    || diagnostic.is_some()
                    || accepted
                        != self
                            .evidence
                            .as_ref()
                            .expect("offer above")
                            .offered_read_bytes
                            .len()
                {
                    return Err(diagnostic.unwrap_or_else(|| {
                        ServiceError::new(
                            ErrorKind::Indeterminate,
                            format!("First read offer accepted only {accepted} bytes; no retry"),
                        )
                    }));
                }
                self.phase = Phase::Reading;
                self.deadline = Some(Duration::from_nanos(returned) + self.timeout);
                self.next_offset = 0;
                self.total = None;
            }
        }
        Ok(())
    }
    pub(crate) fn receive(
        &mut self,
        callback: ObservedCallback,
        processed: u64,
        now: Duration,
    ) -> Result<(), ServiceError> {
        let c = callback.callback;
        self.last_processed = processed;
        self.shared.processed.store(processed, Ordering::Release);
        if self.phase != Phase::Reading {
            if self.failed {
                if let Some((_, e)) = self.pending_failure.as_mut() {
                    retain_callback(e, c.clone());
                    e.after = self.source.fence(processed);
                } else if self
                    .events
                    .iter()
                    .any(|event| matches!(event, FirmwareBackupCaptureEvent::Failed { .. }))
                {
                    if let Some(e) = self.waiting_evidence.as_mut()
                        && self.waiting_kind == FirmwareCaptureReceiptKind::Failure
                    {
                        retain_callback(e, c.clone());
                        e.after = self.source.fence(processed);
                        for event in &mut self.events {
                            if let FirmwareBackupCaptureEvent::Failed {
                                evidence: Some(retained),
                                ..
                            } = event
                            {
                                **retained = e.clone();
                            }
                        }
                    }
                } else {
                    // The published action is immutable. Retain a separate
                    // bounded passive copy in the actual run before publishing
                    // its diagnostic event, so receiver loss cannot destroy the
                    // sole copy of this original callback.
                    self.retain_passive_callback(
                        ObservedCallback {
                            callback: c.clone(),
                            clock_origin: callback.clock_origin.clone(),
                        },
                        processed,
                    );
                    if self.events.len() < FIRMWARE_CAPTURE_EVENT_CAPACITY {
                        self.events.push_back(FirmwareBackupCaptureEvent::RejectedCallback{callback:c,after:self.source.fence(processed),error:ServiceError::new(ErrorKind::Conflict,"Original callback arrived after failure action publication; retained passively and not appended to its immutable action")});
                    } else {
                        self.shared.fail(ServiceError::new(
                            ErrorKind::Indeterminate,
                            "Late callback rail exhausted; source frontier is unresolved",
                        ));
                    }
                }
            } else if self.unexpected.len() < FIRMWARE_CAPTURE_CHUNK_LIMIT {
                self.unexpected.push(c);
            }
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                format!(
                    "Unsolicited/stale callback outside one offered capture read; original_clock={} expected_clock={}",
                    callback.clock_origin, self.reservation.clock_origin
                ),
            ));
        }
        let evidence = self.evidence.as_mut().expect("Reading evidence");
        if evidence.callbacks.len() >= FIRMWARE_CAPTURE_CHUNK_LIMIT {
            retain_callback(evidence, c);
            return Err(invalid("Capture raw callback bound exceeded"));
        }
        evidence.callbacks.push(c.clone());
        evidence.after = self.source.fence(processed);
        if callback.clock_origin != self.reservation.clock_origin
            || c.epoch != self.reservation.epoch
            || c.monotonic_ns.is_none()
            || c.retained.is_empty()
            || c.original_len != c.retained.len() as u64
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                format!(
                    "Callback source/epoch/time/extent differs; original_clock={} expected_clock={}",
                    callback.clock_origin, self.reservation.clock_origin
                ),
            ));
        }
        let expected = evidence.before.allocated + evidence.callbacks.len() as u64;
        if c.ordinal != expected
            || c.monotonic_ns.unwrap() < evidence.offer.expect("offered").started_ns
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Callback ordinal/source time precedes actual read offer",
            ));
        }
        for event in self.decoder.feed(&c.retained) {
            let StreamEvent::Packet(packet) = event else {
                return Err(ServiceError::new(
                    ErrorKind::Protocol,
                    "Unexpected MIDI/framing bytes during capture read",
                ));
            };
            if self.raw_completed.is_some()
                || packet.offset != self.next_offset
                || self.total.is_some_and(|t| t != packet.total)
            {
                return Err(ServiceError::new(
                    ErrorKind::Protocol,
                    "Duplicate/overlapping/out-of-order or unrelated capture packet",
                ));
            }
            self.total = Some(packet.total);
            self.next_offset += packet.payload.len();
            if let Some(raw) = self
                .assembler
                .add(&packet, now)
                .map_err(|e| ServiceError::new(ErrorKind::Protocol, e.to_string()))?
            {
                evidence.raw = kind_prefixed(&raw);
                self.raw_completed = Some(raw);
            }
        }
        Ok(())
    }
    pub(crate) fn finish_callbacks(&mut self, processed: u64) -> Result<(), ServiceError> {
        let Some(raw) = self.raw_completed.take() else {
            return Ok(());
        };
        self.evidence.as_mut().expect("completed read").raw = kind_prefixed(&raw);
        if self.decoder.is_incomplete()
            || self.assembler.progress().is_some()
            || self.source.failed()
            || self.source.fence(processed).allocated != processed
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Read completion has partial framing or unresolved callback frontier",
            ));
        }
        // Retain every original callback and decoded transfer before fallible
        // validation/comparison/clock work; fail() must not invent empty bytes.
        self.evidence.as_mut().expect("completed read").raw = kind_prefixed(&raw);
        let mut evidence = self.evidence.as_ref().expect("completed read").clone();
        evidence.raw = validate_raw(&raw, &evidence.read)?;
        evidence.after = self.source.fence(processed);
        evidence.completed_ns = Some(time_ns(self.origin)?);
        evidence.complete = true;
        evidence.validate_for_reservation(&self.reservation)?;
        let consistency = self.compare(&raw, &evidence.read)?;
        if let Some(c) = &consistency {
            self.strict_equal &= c.strict_raw_equal;
            for d in &c.differences {
                evidence.diagnostics.push(format!(
                    "{} record={} cmd={:04x} offset={} before={:02x} after={:02x} admitted={}",
                    c.policy,
                    d.record,
                    d.command,
                    d.body_offset,
                    d.baseline,
                    d.comparison,
                    d.admitted
                ));
            }
            if !c.admitted {
                evidence.complete = false;
                self.evidence = Some(evidence);
                return Err(ServiceError::new(
                    ErrorKind::Conflict,
                    "Second/boundary original bytes differ outside accepted source-backed consistency offsets",
                ));
            }
        }
        let sequence = evidence.sequence;
        let t = evidence.completed_ns.expect("checked completion");
        let observed_wall = wall_millis()?;
        let boundary = || FirmwareArchiveBoundary {
            epoch: self.reservation.epoch,
            identity: self.identity(),
            device_key: self.reservation.usb.digest(),
            sequence,
            monotonic_millis: t / 1_000_000,
            wall_clock_millis: observed_wall,
            raw_identity: evidence.raw.clone(),
        };
        let action = match &evidence.read {
            FirmwareCaptureRead::Identity(kind) => {
                let b = boundary();
                self.last_identity = Some(b.clone());
                if *kind == FirmwareCaptureBoundaryKind::Opening {
                    self.opening_identity = Some(evidence.raw.clone());
                    FirmwareBackupActionDescription::Opening {
                        origin: BackupOrigin {
                            identity: b.identity.clone(),
                            epoch: b.epoch,
                            device_key: b.device_key.clone(),
                            evidence: vec![],
                        },
                        context: Box::new(self.context.clone()),
                        boundary: Box::new(b),
                    }
                } else {
                    FirmwareBackupActionDescription::Boundary
                }
            }
            FirmwareCaptureRead::Object { object, pass } => {
                if *pass == FirmwareObservationPass::Baseline {
                    self.baseline.insert(*object, evidence.raw.clone());
                }
                FirmwareBackupActionDescription::Object(FirmwareArchiveObservation {
                    object: *object,
                    pass: *pass,
                    epoch: self.reservation.epoch,
                    revision: Revision(sequence),
                    sequence,
                    monotonic_millis: t / 1_000_000,
                    wall_clock_millis: observed_wall,
                    bytes: evidence.raw.clone(),
                })
            }
            FirmwareCaptureRead::Boundary { .. } => FirmwareBackupActionDescription::Boundary,
        };
        self.waiting_kind = match action {
            FirmwareBackupActionDescription::Opening { .. } => FirmwareCaptureReceiptKind::Begun,
            FirmwareBackupActionDescription::Object(_) => FirmwareCaptureReceiptKind::Observation,
            FirmwareBackupActionDescription::Boundary => FirmwareCaptureReceiptKind::Boundary,
        };
        self.evidence = None;
        self.waiting_evidence = Some(evidence.clone());
        self.events
            .push_back(FirmwareBackupCaptureEvent::Observation {
                evidence,
                action: Box::new(action),
                consistency,
            });
        self.index += 1;
        self.phase = Phase::WaitingDurable;
        self.deadline = None;
        Ok(())
    }
    fn identity(&self) -> DeviceIdentity {
        DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: self.mode,
            firmware: None,
            display_name: "GP-200".into(),
        }
    }
    fn compare(
        &self,
        raw: &RawTransfer,
        read: &FirmwareCaptureRead,
    ) -> Result<Option<FirmwareCaptureConsistency>, ServiceError> {
        let baseline = match read {
            FirmwareCaptureRead::Identity(
                FirmwareCaptureBoundaryKind::Middle | FirmwareCaptureBoundaryKind::Final,
            ) => self.opening_identity.as_ref(),
            FirmwareCaptureRead::Object {
                object,
                pass: FirmwareObservationPass::Comparison,
            }
            | FirmwareCaptureRead::Boundary { object, .. } => self.baseline.get(object),
            _ => None,
        };
        let Some(first) = baseline else {
            return Ok(None);
        };
        if matches!(read, FirmwareCaptureRead::Identity(_)) {
            let equal = first == &kind_prefixed(raw);
            return Ok(Some(FirmwareCaptureConsistency {
                policy: "gp200-capture-identity-exact-v1".into(),
                strict_raw_equal: equal,
                admitted: equal,
                differences: vec![],
            }));
        }
        let object = match read {
            FirmwareCaptureRead::Object { object, .. }
            | FirmwareCaptureRead::Boundary { object, .. } => *object,
            _ => unreachable!(),
        };
        let base = if matches!(
            object,
            FirmwareBackupObject::IrMetadata(_) | FirmwareBackupObject::SnapToneMetadata(_)
        ) {
            let equal = first == &validate_raw(raw, read)?;
            return Ok(Some(FirmwareCaptureConsistency {
                policy: "gp200-capture-metadata-exact-v1".into(),
                strict_raw_equal: equal,
                admitted: equal,
                differences: vec![],
            }));
        } else {
            RawTransfer::from_payload(first[0], first[1..].to_vec())
                .map_err(|e| invalid(e.to_string()))?
        };
        compare_transfers(&base, raw, object, self.conditional_163_profile)
    }
    pub(crate) fn check_timeout(&mut self, now: Duration) -> Result<(), ServiceError> {
        if self.failed || self.phase == Phase::Closed || self.phase == Phase::AwaitClosed {
            return Ok(());
        }
        if self.shared.cancelled.load(Ordering::Acquire)
            || self.shared.stopped.load(Ordering::Acquire)
        {
            return Err(ServiceError::new(
                ErrorKind::Cancelled,
                "Capture cancellation observed; no future reads",
            ));
        }
        if self.deadline.is_some_and(|d| now >= d) || self.started.elapsed() > SOFT_DEADLINE {
            return Err(ServiceError::new(
                ErrorKind::Timeout,
                "Capture read/soft deadline expired; no retry",
            ));
        }
        Ok(())
    }
    pub(crate) fn retain_passive_callback(&mut self, callback: ObservedCallback, processed: u64) {
        // Late failure diagnostics and shutdown retain a bounded queue prefix.
        // This cannot complete a read, mutate the immutable outstanding action,
        // or issue an archive observation. Original epoch/clock/extent stay intact.
        self.last_processed = processed;
        self.shared.processed.store(processed, Ordering::Release);
        let read = self.schedule[self.index.min(self.schedule.len() - 1)].clone();
        let fence = self.source.fence(processed);
        let e = self.evidence.get_or_insert_with(|| FirmwareCaptureEvidence {
            read,
            sequence: (self.index + 1) as u64,
            before: fence,
            after: fence,
            offer: None,
            offered_read_bytes: vec![],
            completed_ns: None,
            callbacks: vec![],
            missing: vec![],
            raw: vec![],
            complete: false,
            diagnostics: vec!["Original passive callback prefix after failure/stop; no joined-reader close or durable authority".into()],
        });
        e.complete = false;
        e.after = fence;
        if callback.clock_origin != self.reservation.clock_origin
            && e.diagnostics.len() < FIRMWARE_CAPTURE_DIAGNOSTIC_LIMIT
        {
            e.diagnostics.push(format!(
                "Stopped callback original clock {} differs from {}",
                callback.clock_origin, self.reservation.clock_origin
            ));
        }
        retain_callback(e, callback.callback);
    }
    fn terminal_snapshot(&self) -> FirmwareBackupCaptureEvent {
        let mut originals = Vec::with_capacity(2);
        if let Some(e) = &self.waiting_evidence {
            originals.push(e.clone());
        }
        if let Some((_, e)) = &self.pending_failure {
            originals.push(e.clone());
        } else if let Some(e) = &self.evidence {
            originals.push(e.clone());
        }
        let mut extra = self
            .pending_failure
            .as_ref()
            .map(|(_, e)| e.clone())
            .or_else(|| self.evidence.clone())
            .unwrap_or_else(|| FirmwareCaptureEvidence {
                read: self.schedule[self.index.min(self.schedule.len() - 1)].clone(),
                sequence: (self.index + 1) as u64,
                before: self.source.fence(self.last_processed),
                after: self.source.fence(self.last_processed),
                offer: None,
                offered_read_bytes: vec![],
                completed_ns: None,
                callbacks: vec![],
                missing: vec![],
                raw: vec![],
                complete: false,
                diagnostics: vec![],
            });
        for c in &self.unexpected {
            retain_callback(&mut extra, c.clone());
        }
        for event in &self.events {
            if let FirmwareBackupCaptureEvent::RejectedCallback { callback, .. } = event {
                retain_callback(&mut extra, callback.clone());
            }
        }
        extra.complete = false;
        extra.after = self.source.fence(self.last_processed);
        self.source.add_failure_evidence(&mut extra);
        if extra.after.allocated > extra.after.processed
            && extra.missing.len() < FIRMWARE_CAPTURE_DIAGNOSTIC_LIMIT
        {
            extra.missing.push(FirmwareCaptureMissingExtent {
                first_ordinal: extra.after.processed.saturating_add(1),
                last_ordinal: extra.after.allocated,
                reason: "Original source callbacks allocated but unprocessed at this passive snapshot; available stopped drain is bounded, remaining queued/late extents are unretained".into(),
            });
        }
        if !extra.callbacks.is_empty() || !extra.missing.is_empty() {
            if originals.len() == 2 {
                originals[1] = extra;
            } else {
                originals.push(extra);
            }
        }
        FirmwareBackupCaptureEvent::RetainedAfterStop{error:self.shared.terminal.try_lock().ok().and_then(|v|v.clone()).unwrap_or_else(||ServiceError::new(ErrorKind::Indeterminate,"Stopped capture")),reservation:self.reservation.clone(),originals,observed_frontier:self.source.fence(self.last_processed),unretained_suffix:None,limitation:"Bounded passive originals at the stopped route. One outstanding archival action plus incomplete abort/prefix evidence only; missing extents remain explicit. Later callbacks and final joined-reader accounting are unaccepted. No durable receipt, progression, restoration or flash authority.".into()}
    }
    pub(crate) fn fail(&mut self, error: ServiceError, processed: u64) {
        if self.phase == Phase::Closed {
            self.failed = true;
            self.accepted_review = false;
            self.return_ready = false;
            self.shared.closed.store(false, Ordering::Release);
            self.shared.fail(error.clone());
            self.events
                .push_back(FirmwareBackupCaptureEvent::Rejected(error));
            return;
        }
        if self.failed {
            return;
        }
        if let Ok(mut terminal) = self.shared.terminal.try_lock()
            && terminal.is_none()
        {
            *terminal = Some(error.clone());
        }
        let previous_pending = matches!(
            self.phase,
            Phase::Prepared | Phase::WaitingDurable | Phase::Scheduled
        );
        self.failed = true;
        self.phase = Phase::Failed;
        self.deadline = None;
        self.queued_offer = false;
        self.raw_completed = None;
        let mut evidence = self
            .evidence
            .take()
            .unwrap_or_else(|| FirmwareCaptureEvidence {
                read: self.schedule[self.index.min(self.schedule.len() - 1)].clone(),
                sequence: (self.index + 1) as u64,
                before: self.source.fence(processed),
                after: self.source.fence(processed),
                offer: None,
                offered_read_bytes: vec![],
                completed_ns: None,
                callbacks: vec![],
                missing: vec![],
                raw: vec![],
                complete: false,
                diagnostics: vec![],
            });
        evidence.complete = false;
        evidence.after = self.source.fence(processed);
        evidence
            .diagnostics
            .push(error.message.chars().take(4096).collect());
        for c in self.unexpected.drain(..) {
            if evidence.callbacks.len() < FIRMWARE_CAPTURE_CHUNK_LIMIT {
                evidence.callbacks.push(c);
            }
        }
        self.source.add_failure_evidence(&mut evidence);
        let raw_limit = match evidence.read {
            FirmwareCaptureRead::Identity(_) => 2049,
            FirmwareCaptureRead::Object {
                object:
                    FirmwareBackupObject::IrMetadata(_) | FirmwareBackupObject::SnapToneMetadata(_),
                ..
            } => 8192,
            _ => 65537,
        };
        if evidence.raw.len() > raw_limit {
            let original = evidence.raw.len();
            evidence.raw.truncate(raw_limit);
            evidence.diagnostics.push(format!("Decoded failed transfer original extent={original}; exact prefix retained={raw_limit}; original callbacks retain available wire bytes"));
        }
        // Bound actual retained material, including original failure prefixes;
        // discarded later extents are explicit and never complete callbacks.
        let mut material = evidence.raw.len() + evidence.offered_read_bytes.len();
        let mut omitted = None;
        evidence.callbacks.retain(|c| {
            let keep =
                material.saturating_add(c.retained.len()) <= FIRMWARE_CAPTURE_RECORD_MATERIAL_LIMIT;
            if keep {
                material += c.retained.len();
            } else {
                omitted = Some(
                    omitted.map_or((c.ordinal, c.ordinal), |(a, b): (u64, u64)| {
                        (a.min(c.ordinal), b.max(c.ordinal))
                    }),
                );
            }
            keep
        });
        if let Some((first, last)) = omitted {
            evidence.missing.push(FirmwareCaptureMissingExtent {
                first_ordinal: first,
                last_ordinal: last,
                reason: "Original callback material exceeded record budget; retained prefix only"
                    .into(),
            });
        }
        if previous_pending {
            self.pending_failure = Some((error, evidence));
            return;
        }
        self.waiting_kind = FirmwareCaptureReceiptKind::Failure;
        self.waiting_evidence = Some(evidence.clone());
        self.events.push_back(FirmwareBackupCaptureEvent::Failed {
            error,
            evidence: Some(Box::new(evidence)),
        });
    }
}
fn retain_callback(e: &mut FirmwareCaptureEvidence, c: FirmwareCaptureCallback) {
    if e.callbacks.iter().any(|r| r == &c) {
        return;
    }
    let material = e.raw.len()
        + e.offered_read_bytes.len()
        + e.callbacks.iter().map(|r| r.retained.len()).sum::<usize>();
    if e.callbacks.len() < FIRMWARE_CAPTURE_CHUNK_LIMIT
        && material.saturating_add(c.retained.len()) <= FIRMWARE_CAPTURE_RECORD_MATERIAL_LIMIT
    {
        e.callbacks.push(c);
    } else if e.missing.len() < FIRMWARE_CAPTURE_DIAGNOSTIC_LIMIT {
        e.missing.push(FirmwareCaptureMissingExtent{first_ordinal:c.ordinal,last_ordinal:c.ordinal,reason:"Original late callback exceeded retained count/material bound; exact extent unavailable in this record".into()});
    }
}

fn kind_prefixed(raw: &RawTransfer) -> Vec<u8> {
    let mut out = vec![raw.kind()];
    out.extend_from_slice(raw.payload());
    out
}
fn validate_raw(raw: &RawTransfer, read: &FirmwareCaptureRead) -> Result<Vec<u8>, ServiceError> {
    if raw.kind() != 0x12 {
        return Err(invalid("Capture response kind differs"));
    }
    match read {
        FirmwareCaptureRead::Identity(_) => {
            let identity = Identity::from_transfer(raw)
                .ok_or_else(|| invalid("Capture identity absent/malformed"))?;
            if raw.records().len() != 1
                || identity.model() != gp200_protocol::WireDeviceModel::Gp200
                || identity.state_raw != 1
            {
                return Err(invalid(
                    "Capture identity is not unique normal GP-200 state1",
                ));
            }
        }
        FirmwareCaptureRead::Object {
            object: FirmwareBackupObject::Current | FirmwareBackupObject::Stored(_),
            ..
        }
        | FirmwareCaptureRead::Boundary {
            object: FirmwareBackupObject::Current,
            ..
        } => {
            let context = WireContext::from_transfer(raw)
                .ok_or_else(|| invalid("Capture preset context absent"))?;
            let object = match read {
                FirmwareCaptureRead::Object { object, .. }
                | FirmwareCaptureRead::Boundary { object, .. } => *object,
                _ => unreachable!(),
            };
            if raw.records().iter().filter(|r| r.command == 0).count() != 1
                || context.selector > 255
                || match object {
                    FirmwareBackupObject::Current => context.group != 0,
                    FirmwareBackupObject::Stored(slot) => {
                        context.group != 1 || context.selector != slot
                    }
                    _ => true,
                }
            {
                return Err(invalid("Capture preset context/slot differs"));
            }
            for (command, length) in [(2, 88), (8, 16)] {
                let records = raw
                    .records()
                    .iter()
                    .filter(|r| r.command == command)
                    .collect::<Vec<_>>();
                if records.len() != 1
                    || records[0].body.len() != length
                    || records[0].body[..2] != context.selector.to_le_bytes()
                {
                    return Err(invalid(
                        "Capture preset header/chain incomplete or wrongly addressed",
                    ));
                }
            }
            let modules = raw
                .records()
                .iter()
                .filter(|r| r.command == 0x14)
                .collect::<Vec<_>>();
            let ids = modules
                .iter()
                .filter(|r| r.body.len() == 68 && u16::from_le_bytes([r.body[2], r.body[3]]) == 15)
                .map(|r| r.body[0])
                .collect::<std::collections::BTreeSet<_>>();
            let assignments = gp200_domain::PatchAssignments::from_raw(raw);
            if modules.len() != 11
                || ids != (0..11).collect()
                || assignments.expressions.len() != 9
                || assignments.ctrls.len() != 8
                || assignments.quicks.len() != 3
                || !assignments.diagnostics.is_empty()
            {
                return Err(invalid(
                    "Capture preset complete modules/assignments absent",
                ));
            }
        }
        FirmwareCaptureRead::Object {
            object: FirmwareBackupObject::Globals,
            ..
        }
        | FirmwareCaptureRead::Boundary {
            object: FirmwareBackupObject::Globals,
            ..
        } => {
            let parsed =
                GlobalRecords::from_transfer(raw.clone()).map_err(|e| invalid(e.to_string()))?;
            let tables = parsed
                .footswitch_tables
                .iter()
                .map(|t| t.table_raw())
                .collect::<std::collections::BTreeSet<_>>();
            if !parsed.is_complete()
                || parsed.footswitch_tables.len() != 3
                || tables != [0, 1, 2].into_iter().collect()
                || raw.records().iter().any(|r| r.command == 0)
            {
                return Err(invalid("Capture global records/mapping0/1/2 incomplete"));
            }
        }
        FirmwareCaptureRead::Object {
            object:
                FirmwareBackupObject::IrMetadata(slot) | FirmwareBackupObject::SnapToneMetadata(slot),
            ..
        } => {
            let bank = if matches!(
                read,
                FirmwareCaptureRead::Object {
                    object: FirmwareBackupObject::IrMetadata(_),
                    ..
                }
            ) {
                AssetBank::Ir
            } else {
                AssetBank::SnapTone
            };
            if raw.records().len() != 1 {
                return Err(invalid("Capture asset metadata is not unique"));
            }
            let metadata = AssetSlotMetadata::decode(bank, &raw.records()[0])
                .map_err(|e| invalid(e.to_string()))?;
            if metadata.slot.index() != u16::from(*slot) {
                return Err(invalid("Capture metadata slot differs"));
            }
            return firmware_capture_metadata_record_bytes(&raw.records()[0]);
        }
        _ => return Err(invalid("Unsupported capture boundary")),
    }
    let bytes = kind_prefixed(raw);
    if bytes.len() > 65537 {
        return Err(invalid("Capture decoded raw bound exceeded"));
    }
    Ok(bytes)
}
fn compare_transfers(
    first: &RawTransfer,
    second: &RawTransfer,
    object: FirmwareBackupObject,
    conditional_163_profile: bool,
) -> Result<Option<FirmwareCaptureConsistency>, ServiceError> {
    let mut result=FirmwareCaptureConsistency{policy:"gp200-capture-conditional-reported163-consistency-v1; source-note-sha256=be69aea7171cd239f2414aa645d46d39af9a5c2ae1e5a115feee825fb971966a; manual-associated-V1.6.3-conditional; automatic-version-unknown; preset=cmd2[15],cmd8[15],cmdF[2,3]; globals=cmd1007[54,55]=observed-variable-opaque,[56]=native-skipped; cmd1002[6,7],cmdA[2,3]/strictly-after-same-first-NUL-from4; originals-retained; strict-archive-partial-on-any-raw-change; non-atomic".into(),strict_raw_equal:first==second,admitted:true,differences:vec![]};
    if !conditional_163_profile {
        result.policy="gp200-capture-strict-raw-v1; conditional accepted V1.6.3 report/profile absent; automatic-version-unknown".into();
    }
    if first.kind() != second.kind() || first.records().len() != second.records().len() {
        result.admitted = false;
        return Ok(Some(result));
    }
    for (index, (a, b)) in first.records().iter().zip(second.records()).enumerate() {
        if a.command != b.command || a.body.len() != b.body.len() {
            result.admitted = false;
            continue;
        }
        for (offset, (&before, &after)) in a.body.iter().zip(&b.body).enumerate() {
            if before == after {
                continue;
            }
            let admitted = conditional_163_profile
                && if object == FirmwareBackupObject::Globals {
                    match (a.command, a.body.len()) {
                        (0x1007, 72) => (54..=56).contains(&offset),
                        (0x1002, 8) => matches!(offset, 6 | 7),
                        (0xA, 20) => {
                            matches!(offset, 2 | 3)
                                || a.body[4..]
                                    .iter()
                                    .position(|v| *v == 0)
                                    .zip(b.body[4..].iter().position(|v| *v == 0))
                                    .is_some_and(|(x, y)| x == y && offset > 4 + x)
                        }
                        _ => false,
                    }
                } else {
                    match (a.command, a.body.len()) {
                        (2, 88) | (8, 16) => offset == 15,
                        (0xF, 8) => matches!(offset, 2 | 3),
                        _ => false,
                    }
                };
            result.admitted &= admitted;
            result.differences.push(FirmwareCaptureDifference {
                record: index,
                command: a.command,
                body_offset: offset,
                baseline: before,
                comparison: after,
                admitted,
            });
            if result.differences.len() > FIRMWARE_CAPTURE_DIAGNOSTIC_LIMIT {
                return Err(invalid(
                    "Capture exact difference description bound exceeded",
                ));
            }
        }
    }
    Ok(Some(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gp200_protocol::RawRecord;
    fn transfer(command: u16, body: Vec<u8>) -> RawTransfer {
        RawTransfer::from_records(0x12, &[RawRecord { command, body }]).unwrap()
    }
    #[test]
    fn schedule_has_exact_independent_fixed_boundaries_and_all_addresses() {
        for nam in [false, true] {
            let reads = schedule(nam);
            assert_eq!(reads.len(), if nam { 581 } else { 561 });
            assert_eq!(
                reads[0],
                FirmwareCaptureRead::Identity(FirmwareCaptureBoundaryKind::Opening)
            );
            assert_eq!(
                reads.last(),
                Some(&FirmwareCaptureRead::Identity(
                    FirmwareCaptureBoundaryKind::Final
                ))
            );
            for pass in [
                FirmwareObservationPass::Baseline,
                FirmwareObservationPass::Comparison,
            ] {
                for slot in 0..256 {
                    assert_eq!(
                        reads
                            .iter()
                            .filter(|r| **r
                                == FirmwareCaptureRead::Object {
                                    object: FirmwareBackupObject::Stored(slot),
                                    pass
                                })
                            .count(),
                        1
                    );
                }
                for slot in 0..20 {
                    assert_eq!(
                        reads
                            .iter()
                            .filter(|r| **r
                                == FirmwareCaptureRead::Object {
                                    object: FirmwareBackupObject::IrMetadata(slot),
                                    pass
                                })
                            .count(),
                        1
                    );
                }
                for slot in 0..10 {
                    assert_eq!(
                        reads
                            .iter()
                            .filter(|r| **r
                                == FirmwareCaptureRead::Object {
                                    object: FirmwareBackupObject::SnapToneMetadata(slot),
                                    pass
                                })
                            .count(),
                        usize::from(nam)
                    );
                }
            }
            let mid = if nam { 289 } else { 279 };
            assert_eq!(
                reads[mid],
                FirmwareCaptureRead::Boundary {
                    boundary: FirmwareCaptureBoundaryKind::Middle,
                    object: FirmwareBackupObject::Current
                }
            );
            assert_eq!(
                reads[mid + 1],
                FirmwareCaptureRead::Boundary {
                    boundary: FirmwareCaptureBoundaryKind::Middle,
                    object: FirmwareBackupObject::Globals
                }
            );
            assert_eq!(
                reads[mid + 2],
                FirmwareCaptureRead::Identity(FirmwareCaptureBoundaryKind::Middle)
            );
        }
    }
    #[test]
    fn conditional_profile_masks_exact_individual_offsets_and_keeps_adjacent_bytes_strict() {
        for (object, command, len, allowed) in [
            (FirmwareBackupObject::Current, 2, 88, vec![15]),
            (FirmwareBackupObject::Stored(255), 8, 16, vec![15]),
            (FirmwareBackupObject::Current, 0xf, 8, vec![2, 3]),
            (FirmwareBackupObject::Globals, 0x1007, 72, vec![54, 55, 56]),
            (FirmwareBackupObject::Globals, 0x1002, 8, vec![6, 7]),
            (
                FirmwareBackupObject::Globals,
                0xa,
                20,
                vec![2, 3, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19],
            ),
        ] {
            let mut original = vec![0x55; len];
            if command == 0xa {
                original[7] = 0;
            }
            let first = transfer(command, original.clone());
            for offset in 0..len {
                let mut changed = original.clone();
                changed[offset] ^= 1;
                let second = transfer(command, changed);
                let result = compare_transfers(&first, &second, object, true)
                    .unwrap()
                    .unwrap();
                assert!(!result.strict_raw_equal);
                assert_eq!(
                    result.admitted,
                    allowed.contains(&offset),
                    "cmd={command:x}/offset={offset}"
                );
                assert_eq!(result.differences.len(), 1);
                let result = compare_transfers(&first, &second, object, false)
                    .unwrap()
                    .unwrap();
                assert!(!result.admitted);
                assert_eq!(first, transfer(command, original.clone()));
            }
        }
    }
    #[test]
    fn missing_changed_nul_order_length_kind_and_unknown_records_are_strict() {
        let first = transfer(0xa, vec![0x55; 20]);
        let mut changed = vec![0x55; 20];
        changed[19] ^= 1;
        assert!(
            !compare_transfers(
                &first,
                &transfer(0xa, changed),
                FirmwareBackupObject::Globals,
                true
            )
            .unwrap()
            .unwrap()
            .admitted
        );
        let first = transfer(0xeef1, vec![0; 8]);
        assert!(
            !compare_transfers(
                &first,
                &transfer(0xeef1, vec![1; 8]),
                FirmwareBackupObject::Current,
                true
            )
            .unwrap()
            .unwrap()
            .admitted
        );
        let first = transfer(2, vec![0; 88]);
        assert!(
            !compare_transfers(
                &first,
                &transfer(2, vec![0; 87]),
                FirmwareBackupObject::Current,
                true
            )
            .unwrap()
            .unwrap()
            .admitted
        );
        let second = RawTransfer::from_records(0x11, first.records()).unwrap();
        assert!(
            !compare_transfers(&first, &second, FirmwareBackupObject::Current, true)
                .unwrap()
                .unwrap()
                .admitted
        );
    }
    fn original_failed_event(ordinal: u64) -> FirmwareBackupCaptureEvent {
        let fence = FirmwareCaptureFence {
            allocated: ordinal,
            processed: ordinal,
        };
        FirmwareBackupCaptureEvent::Failed {
            error: ServiceError::new(ErrorKind::Indeterminate, "original failure"),
            evidence: Some(Box::new(FirmwareCaptureEvidence {
                read: FirmwareCaptureRead::Identity(FirmwareCaptureBoundaryKind::Opening),
                sequence: 1,
                before: FirmwareCaptureFence {
                    allocated: 0,
                    processed: 0,
                },
                after: fence,
                offer: None,
                offered_read_bytes: vec![],
                completed_ns: None,
                callbacks: vec![FirmwareCaptureCallback {
                    ordinal,
                    epoch: SessionEpoch(77),
                    monotonic_ns: Some(ordinal),
                    original_len: 3,
                    retained: vec![0xf0, ordinal as u8, 0xf7],
                }],
                missing: vec![],
                raw: vec![],
                complete: false,
                diagnostics: vec![],
            })),
        }
    }
    fn assert_revoked_original(event: FirmwareBackupCaptureEvent, ordinal: u64) {
        let FirmwareBackupCaptureEvent::Revoked { original, .. } = event else {
            panic!("Stopped event lost its original {event:?}");
        };
        let FirmwareBackupCaptureEvent::Failed {
            evidence: Some(e), ..
        } = original.as_ref()
        else {
            panic!("Wrong passive original {original:?}");
        };
        assert_eq!(e.callbacks[0].ordinal, ordinal);
        assert_eq!(e.callbacks[0].epoch, SessionEpoch(77));
        assert_eq!(e.callbacks[0].retained, vec![0xf0, ordinal as u8, 0xf7]);
        assert!(
            FirmwareBackupCaptureEvent::Revoked {
                error: ServiceError::new(ErrorKind::Cancelled, "stopped"),
                original,
            }
            .into_library_action()
            .is_err()
        );
    }
    #[test]
    fn full_result_rail_stops_and_recovers_original_queued_and_blocked_events() {
        let (client, mut worker) = channel();
        let wake: WakeUi = Arc::new(|| {});
        for ordinal in 1..=FIRMWARE_CAPTURE_EVENT_CAPACITY as u64 {
            worker.reply(original_failed_event(ordinal), &wake);
        }
        let last = FIRMWARE_CAPTURE_EVENT_CAPACITY as u64 + 1;
        worker.reply(original_failed_event(last), &wake);
        worker.blocked_event.as_mut().unwrap().1 = Instant::now() - Duration::from_millis(300);
        let mut actor = DeviceActor::new(TransportConfig::default());
        worker.flush(&mut actor, &wake);
        assert!(worker.shared.stopped.load(Ordering::Acquire));
        // Queue revocation retains exact raw-bearing events, not just an error.
        for ordinal in 1..=FIRMWARE_CAPTURE_EVENT_CAPACITY as u64 {
            assert_revoked_original(client.try_next_event().unwrap(), ordinal);
        }
        worker.flush(&mut actor, &wake);
        assert_revoked_original(client.try_next_event().unwrap(), last);
        assert!(client.try_next_event().is_none());
        assert!(matches!(
            client.try_submit(FirmwareBackupCaptureCommand::ReturnToNormal),
            Err(SubmitError::Disconnected(_))
        ));
    }
    #[test]
    fn disconnected_result_consumer_keeps_original_without_publication_loop() {
        let (client, mut worker) = channel();
        let shared = worker.shared.clone();
        drop(client);
        let wake: WakeUi = Arc::new(|| {});
        worker.reply(original_failed_event(1), &wake);
        worker.reply(original_failed_event(2), &wake);
        let mut actor = DeviceActor::new(TransportConfig::default());
        for _ in 0..3 {
            worker.flush(&mut actor, &wake);
        }
        assert_revoked_original(shared.take_retained().unwrap(), 1);
        worker.flush(&mut actor, &wake);
        assert_revoked_original(shared.take_retained().unwrap(), 2);
        assert!(worker.blocked_event.is_none());
    }
    #[test]
    fn passive_retention_keeps_colliding_ordinals_from_distinct_original_epochs() {
        let FirmwareBackupCaptureEvent::Failed {
            evidence: Some(mut e),
            ..
        } = original_failed_event(1)
        else {
            unreachable!()
        };
        let mut stale = e.callbacks[0].clone();
        stale.epoch = SessionEpoch(76);
        retain_callback(&mut e, stale.clone());
        retain_callback(&mut e, stale);
        assert_eq!(e.callbacks.len(), 2);
        assert_eq!(e.callbacks[0].epoch, SessionEpoch(77));
        assert_eq!(e.callbacks[1].epoch, SessionEpoch(76));
    }
    #[test]
    fn bounded_control_rail_returns_original_commands_and_drop_revokes_route() {
        let (client, worker) = channel();
        worker.shared.active.store(true, Ordering::Release);
        for _ in 0..FIRMWARE_CAPTURE_CONTROL_CAPACITY {
            client
                .try_submit(FirmwareBackupCaptureCommand::ReturnToNormal)
                .unwrap();
        }
        assert!(matches!(
            client.try_submit(FirmwareBackupCaptureCommand::ReturnToNormal),
            Err(SubmitError::Busy(
                FirmwareBackupCaptureCommand::ReturnToNormal
            ))
        ));
        drop(client);
        assert!(worker.shared.stopped.load(Ordering::Acquire));
        assert!(worker.shared.blocks_firmware());
    }
}
