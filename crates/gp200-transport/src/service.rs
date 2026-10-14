use crate::{
    DeviceActor, MidiSession, ReceiveSink, TransportAction, TransportBackend, TransportConfig,
    backend::{NormalCallbackSource, ObservedCallback},
    firmware_actor::FirmwareUpdateOwner,
    firmware_backup_capture::{
        CaptureShared, CaptureWorkerRail, FirmwareBackupCaptureClient,
        FirmwareBackupCaptureCommand, FirmwareBackupCaptureEvent,
    },
    firmware_service::*,
};
use crossbeam_channel::{Receiver, SendTimeoutError, Sender, TrySendError, bounded};
use gp200_domain::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

/// Nonblocking service handle. Dropping/shutting it down signals the worker;
/// it never joins a MIDI thread on the GUI thread. `wait_stopped` is for CLIs.
#[cfg(test)]
pub(crate) struct FirmwareAdmissionTestGates {
    pub(crate) submission: Arc<std::sync::Mutex<()>>,
    pub(crate) summary: Arc<std::sync::Mutex<Option<FirmwareOwnerSummary>>>,
    pub(crate) active: Arc<AtomicBool>,
}
pub struct MidiDeviceService {
    commands: Sender<DeviceCommand>,
    events: Receiver<DeviceEvent>,
    shutdown: ShutdownToken,
    finished: Arc<AtomicBool>,
    firmware_commands: Sender<FirmwareOwnershipRequest>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    recovery_commands: Sender<crate::LinuxRecoveryRequest>,
    firmware_events: Receiver<FirmwareOwnerEvent>,
    firmware_active: Arc<AtomicBool>,
    submission_gate: Arc<std::sync::Mutex<()>>,
    firmware_summary: Arc<std::sync::Mutex<Option<FirmwareOwnerSummary>>>,
    capture_client: Option<FirmwareBackupCaptureClient>,
    capture_shared: Arc<CaptureShared>,
}
/// The original worker's lifetime, including unwinding after an accepted command.
/// This signal reports termination only; it never manufactures an owning result.
struct WorkerTermination {
    finished: Arc<AtomicBool>,
    capture: Arc<CaptureShared>,
}
impl Drop for WorkerTermination {
    fn drop(&mut self) {
        if thread::panicking() {
            self.capture.fail(ServiceError::new(
                ErrorKind::Indeterminate,
                "The original MIDI worker panicked; accepted capture and ownership remain unresolved",
            ));
        }
        self.finished.store(true, Ordering::Release);
    }
}
impl MidiDeviceService {
    pub fn new(wake: WakeUi) -> Self {
        #[cfg(target_os = "linux")]
        let backend: Box<dyn TransportBackend> = Box::new(crate::AlsaRawBackend::new());
        #[cfg(not(target_os = "linux"))]
        let backend: Box<dyn TransportBackend> = Box::new(crate::MidirBackend::new());
        #[cfg(target_os = "linux")]
        let config = TransportConfig::alsa_raw();
        #[cfg(not(target_os = "linux"))]
        let config = TransportConfig::default();
        Self::with_backend(backend, config, wake)
    }
    pub fn with_backend(
        backend: Box<dyn TransportBackend>,
        config: TransportConfig,
        wake: WakeUi,
    ) -> Self {
        Self::try_with_backend(backend, config, wake).expect("Cannot spawn MIDI service worker")
    }
    pub fn try_with_backend(
        backend: Box<dyn TransportBackend>,
        config: TransportConfig,
        wake: WakeUi,
    ) -> Result<Self, ServiceError> {
        let (commands, command_rx) = bounded(config.command_capacity.max(1));
        let (event_tx, events) = bounded(config.event_capacity.max(1));
        let shutdown = ShutdownToken::default();
        let finished = Arc::new(AtomicBool::new(false));
        let (firmware_commands, fw_rx) = bounded(1);
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        let (recovery_commands, recovery_rx) = bounded(1);
        let (fw_tx, firmware_events) = bounded(1);
        let firmware_active = Arc::new(AtomicBool::new(false));
        let submission_gate = Arc::new(std::sync::Mutex::new(()));
        let firmware_summary = Arc::new(std::sync::Mutex::new(None));
        let (capture_client, capture_rail) = crate::firmware_backup_capture::channel();
        let capture_shared = capture_rail.shared.clone();
        let worker_summary = firmware_summary.clone();
        let worker_shutdown = shutdown.clone();
        let worker_finished = finished.clone();
        let worker_capture = capture_shared.clone();
        thread::Builder::new()
            .name("gp200-midi".into())
            .spawn(move || {
                let _termination = WorkerTermination {
                    finished: worker_finished,
                    capture: worker_capture,
                };
                run_worker(
                    backend,
                    config,
                    command_rx,
                    event_tx,
                    worker_shutdown,
                    wake,
                    (
                        FirmwareWorkerRail {
                            commands: fw_rx,
                            #[cfg(all(
                                feature = "firmware-linux-completion",
                                target_os = "linux"
                            ))]
                            recovery: recovery_rx,
                            events: fw_tx,
                            summary: worker_summary,
                        },
                        capture_rail,
                    ),
                );
            })
            .map_err(|error| {
                ServiceError::new(ErrorKind::Io, format!("Cannot start MIDI worker: {error}"))
            })?;
        Ok(Self {
            commands,
            events,
            shutdown,
            finished,
            firmware_commands,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            recovery_commands,
            firmware_events,
            firmware_active,
            submission_gate,
            firmware_summary,
            capture_client: Some(capture_client),
            capture_shared,
        })
    }
    pub fn take_firmware_backup_capture_client(&mut self) -> Option<FirmwareBackupCaptureClient> {
        self.capture_client.take()
    }
    /// Stopped-only passive originals, including recovery after capture-client
    /// loss. This does not extract another client or grant archival progression.
    pub fn try_next_retained_capture_event(&self) -> Option<FirmwareBackupCaptureEvent> {
        self.capture_shared.take_retained()
    }
    pub fn try_start_firmware(
        &self,
        request: FirmwareOwnershipRequest,
    ) -> Result<FirmwareOwnerTicket, SubmitError<FirmwareOwnershipRequest>> {
        let Ok(_gate) = self.submission_gate.try_lock() else {
            return Err(SubmitError::Busy(request));
        };
        if self.shutdown.cancelled() || self.stopped() {
            return Err(SubmitError::Disconnected(request));
        }
        if self.capture_shared.blocks_firmware() {
            return Err(SubmitError::Busy(request));
        }
        let Ok(mut summary) = self.firmware_summary.try_lock() else {
            return Err(SubmitError::Busy(request));
        };
        if self.firmware_active.swap(true, Ordering::SeqCst) {
            return Err(SubmitError::Busy(request));
        }
        let stop = request.stop.clone();
        match self.firmware_commands.try_send(request) {
            Ok(()) => {
                *summary = None;
                Ok(FirmwareOwnerTicket { stop })
            }
            Err(e) => {
                self.firmware_active.store(false, Ordering::SeqCst);
                Err(match e {
                    TrySendError::Full(r) => SubmitError::Busy(r),
                    TrySendError::Disconnected(r) => SubmitError::Disconnected(r),
                })
            }
        }
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub fn try_start_linux_recovery(
        &self,
        request: crate::LinuxRecoveryRequest,
    ) -> Result<FirmwareOwnerTicket, SubmitError<crate::LinuxRecoveryRequest>> {
        let Ok(_gate) = self.submission_gate.try_lock() else {
            return Err(SubmitError::Busy(request));
        };
        if self.shutdown.cancelled() || self.stopped() {
            return Err(SubmitError::Disconnected(request));
        }
        if self.capture_shared.blocks_firmware() {
            return Err(SubmitError::Busy(request));
        }
        let Ok(mut summary) = self.firmware_summary.try_lock() else {
            return Err(SubmitError::Busy(request));
        };
        if self.firmware_active.swap(true, Ordering::SeqCst) {
            return Err(SubmitError::Busy(request));
        }
        let stop = request.stop.clone();
        match self.recovery_commands.try_send(request) {
            Ok(()) => {
                *summary = None;
                Ok(FirmwareOwnerTicket { stop })
            }
            Err(error) => {
                self.firmware_active.store(false, Ordering::SeqCst);
                Err(match error {
                    TrySendError::Full(r) => SubmitError::Busy(r),
                    TrySendError::Disconnected(r) => SubmitError::Disconnected(r),
                })
            }
        }
    }
    pub fn try_next_firmware_event(&self) -> Option<FirmwareOwnerEvent> {
        let event = self.firmware_events.try_recv().ok()?;
        self.firmware_active.store(false, Ordering::SeqCst);
        Some(event)
    }
    pub fn firmware_summary(&self) -> Option<FirmwareOwnerSummary> {
        self.firmware_summary
            .try_lock()
            .ok()
            .and_then(|s| s.clone())
    }
    /// Descriptive availability only. This does not call the private issuer.
    pub fn firmware_update_unavailable_reason(&self) -> &'static str {
        "The Linux update and recovery protocol still needs hardware validation"
    }
    #[cfg(test)]
    pub(crate) fn test_firmware_admission_gates(&self) -> FirmwareAdmissionTestGates {
        FirmwareAdmissionTestGates {
            submission: self.submission_gate.clone(),
            summary: self.firmware_summary.clone(),
            active: self.firmware_active.clone(),
        }
    }
    pub fn stopped(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
    /// Explicitly blocking utility for a CLI or test, never needed by egui.
    pub fn wait_stopped(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !self.stopped() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        self.stopped()
    }
}
impl DeviceService for MidiDeviceService {
    fn firmware_backup_capture_support(&self) -> FirmwareCaptureSupport {
        FirmwareCaptureSupport::ConcreteClientRequired
    }
    fn try_submit(&self, command: DeviceCommand) -> Result<(), SubmitError<DeviceCommand>> {
        let Ok(_gate) = self.submission_gate.try_lock() else {
            return Err(if self.shutdown.cancelled() || self.stopped() {
                SubmitError::Disconnected(command)
            } else {
                SubmitError::Busy(command)
            });
        };
        if self.firmware_active.load(Ordering::SeqCst)
            || self.capture_shared.blocks_normal()
                && !matches!(&command, DeviceCommand::Discover | DeviceCommand::Shutdown)
        {
            return Err(SubmitError::Busy(command));
        }
        if self.shutdown.cancelled() || self.stopped() {
            return Err(SubmitError::Disconnected(command));
        }
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                TrySendError::Full(command) => SubmitError::Busy(command),
                TrySendError::Disconnected(command) => SubmitError::Disconnected(command),
            })
    }
    fn try_next_event(&self) -> Option<DeviceEvent> {
        self.events.try_recv().ok()
    }
    fn shutdown(&self) {
        self.capture_shared.stop();
        self.shutdown.cancel();
        let _ = self.commands.try_send(DeviceCommand::Shutdown);
    }
}
impl Drop for MidiDeviceService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct FirmwareWorkerRail {
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    recovery: Receiver<crate::LinuxRecoveryRequest>,
    commands: Receiver<FirmwareOwnershipRequest>,
    events: Sender<FirmwareOwnerEvent>,
    summary: Arc<std::sync::Mutex<Option<FirmwareOwnerSummary>>>,
}
enum ExclusiveFirmwareOwner {
    Legacy(Box<FirmwareUpdateOwner>),
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    Linux(crate::firmware_linux_completion::LinuxCompletionOwner),
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    Recovery(crate::firmware_linux_completion::LinuxCompletionOwner),
}
impl ExclusiveFirmwareOwner {
    fn tick(&mut self) {
        match self {
            Self::Legacy(o) => o.tick(),
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            Self::Linux(o) | Self::Recovery(o) => o.tick(),
        }
    }
    fn summary(&self) -> &FirmwareOwnerSummary {
        match self {
            Self::Legacy(o) => o.summary(),
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            Self::Linux(o) | Self::Recovery(o) => o.summary(),
        }
    }
    fn stopped(&self) -> bool {
        match self {
            Self::Legacy(o) => o.stopped(),
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            Self::Linux(o) | Self::Recovery(o) => o.stopped(),
        }
    }
    fn request_host_stop(&mut self) {
        match self {
            Self::Legacy(o) => o.request_host_stop(),
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            Self::Linux(o) | Self::Recovery(o) => o.request_host_stop(),
        }
    }
    fn retire(self) -> FirmwareOwnerEvent {
        match self {
            Self::Legacy(o) => match o.retire() {
                Ok((summary, store)) => FirmwareOwnerEvent::Retired { summary, store },
                Err(error) => FirmwareOwnerEvent::StartupFailed { error },
            },
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            Self::Linux(o) => match o.retire() {
                Ok((summary, store, outcome)) => FirmwareOwnerEvent::LinuxCompletionReturned {
                    summary,
                    store,
                    outcome: Box::new(outcome),
                },
                Err(error) => FirmwareOwnerEvent::StartupFailed { error },
            },
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            Self::Recovery(o) => match o.retire_recovery() {
                Ok((summary, store, outcome)) => FirmwareOwnerEvent::LinuxRecoveryReturned {
                    summary,
                    store,
                    outcome: Box::new(outcome),
                },
                Err(error) => FirmwareOwnerEvent::StartupFailed { error },
            },
        }
    }
}
fn run_worker(
    mut backend: Box<dyn TransportBackend>,
    config: TransportConfig,
    commands: Receiver<DeviceCommand>,
    events: Sender<DeviceEvent>,
    shutdown: ShutdownToken,
    wake: WakeUi,
    rails: (FirmwareWorkerRail, CaptureWorkerRail),
) {
    let (firmware, mut capture) = rails;
    let FirmwareWorkerRail {
        commands: firmware_commands,
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            recovery: recovery_commands,
        events: firmware_events,
        summary: firmware_summary,
    } = firmware;
    let (rx_sender, received) = bounded::<ObservedCallback>(config.callback_capacity.max(1));
    let overflow = Arc::new(AtomicBool::new(false));
    let mut actor = DeviceActor::with_mode(config.clone(), backend.mode());
    let mut session: Option<Box<dyn MidiSession>> = None;
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    let mut normal_source_retired = true; // Fresh service has never opened a normal reader.
    let mut selection: Option<DeviceSelection> = None;
    let origin = Instant::now();
    let mut source = NormalCallbackSource::new(origin, SessionEpoch(0));
    let mut processed = 0_u64;
    let mut next_discovery = Duration::ZERO;
    let mut discover_active = false;
    let mut last_ports: Option<Vec<PortPairCandidate>> = None;
    let mut dropped_notifications = 0_u64;
    let mut updater: Option<ExclusiveFirmwareOwner> = None;
    while !shutdown.cancelled() {
        if let Some(owner) = &mut updater {
            owner.tick();
            if let Ok(mut summary) = firmware_summary.lock() {
                *summary = Some(owner.summary().clone());
            }
            if owner.stopped() {
                let owner = updater.take().expect("one exclusive owner");
                #[allow(unused_mut)]
                let mut event = owner.retire();
                #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
                if let FirmwareOwnerEvent::LinuxCompletionReturned { outcome, .. } = &mut event
                    && outcome.editor_return_allowed
                {
                    let next = outcome
                        .verified_normal_epoch
                        .and_then(|epoch| epoch.0.checked_add(1))
                        .map(SessionEpoch);
                    let mut fresh = DeviceActor::with_mode(config.clone(), backend.mode());
                    if let Some(next) = next {
                        if fresh.set_fresh_epoch_before_connect(next).is_ok() {
                            actor = fresh;
                        } else {
                            outcome.editor_return_allowed = false;
                        }
                    } else {
                        outcome.editor_return_allowed = false;
                    }
                }
                #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
                if let FirmwareOwnerEvent::LinuxRecoveryReturned { outcome, .. } = &mut event
                    && outcome.editor_return_allowed
                {
                    let next = outcome
                        .verified_normal_epoch
                        .and_then(|e| e.0.max(outcome.editor_source_epoch.0).checked_add(1))
                        .map(SessionEpoch);
                    let mut fresh = DeviceActor::with_mode(config.clone(), backend.mode());
                    if let Some(next) = next {
                        if fresh.set_fresh_epoch_before_connect(next).is_ok() {
                            actor = fresh;
                        } else {
                            outcome.editor_return_allowed = false;
                        }
                    } else {
                        outcome.editor_return_allowed = false;
                    }
                }
                let _ = firmware_events.try_send(event);
                wake();
            }
            thread::sleep(Duration::from_millis(1));
            continue;
        }
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        if let Ok(request) = recovery_commands.try_recv() {
            let check = (|| {
                if backend.mode() != BackendMode::Hardware {
                    return Err(ServiceError::new(
                        ErrorKind::Unverified,
                        "Concrete Linux recovery cannot use simulator or replay transport",
                    ));
                }
                actor.firmware_recovery_quiescence()?;
                if shutdown.cancelled() || request.stop.requested.load(Ordering::SeqCst) {
                    return Err(ServiceError::new(
                        ErrorKind::Cancelled,
                        "Fresh recovery cancelled before ownership",
                    ));
                }
                if session.is_some()
                    || !normal_source_retired
                    || capture.shared.blocks_firmware()
                    || !capture.quiescent()
                    || !commands.is_empty()
                    || !firmware_commands.is_empty()
                    || !firmware_events.is_empty()
                    || !received.is_empty()
                    || !events.is_empty()
                    || overflow.load(Ordering::Acquire)
                    || source.failed()
                    || source.fence(processed).allocated != processed
                {
                    return Err(ServiceError::new(
                        ErrorKind::Busy,
                        "Fresh recovery requires an absent or genuinely retired normal reader, accounted original callback frontier and empty result/request rails",
                    ));
                }
                actor
                    .epoch()
                    .0
                    .checked_add(1)
                    .map(SessionEpoch)
                    .ok_or_else(|| ServiceError::invalid("New recovery epoch exhausted"))
            })();
            match check {
                Err(error) => {
                    let _ = firmware_events.try_send(FirmwareOwnerEvent::Rejected {
                        error,
                        store: request.store,
                    });
                    wake();
                }
                Ok(epoch) => {
                    discover_active = false;
                    let monitor = request.monitor.clone();
                    match crate::firmware_linux_completion::LinuxCompletionOwner::start_recovery(
                        request, epoch,
                    ) {
                        Ok(owner) => {
                            monitor.attach(&owner);
                            updater = Some(ExclusiveFirmwareOwner::Recovery(owner));
                        }
                        Err(failed) => {
                            let _ = firmware_events.try_send(FirmwareOwnerEvent::Rejected {
                                error: failed.error,
                                store: failed.store,
                            });
                            wake();
                        }
                    }
                }
            }
            continue;
        }
        #[allow(unused_mut)]
        if let Ok(mut request) = firmware_commands.try_recv() {
            let stop = request.stop.clone();
            #[cfg(test)]
            let mock = request.mock.is_some();
            #[cfg(not(test))]
            let mock = false;
            let check = (|| {
                if !mock {
                    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
                    let completion_admitted = if let Some(authorization) = &request.linux_completion
                    {
                        authorization.entry_fresh()?;
                        if authorization.job.digest() != request.job.digest()
                            || actor.capture.as_ref().is_none_or(|run| {
                                !run.entry_study_matches(
                                    &authorization.review.reservation,
                                    &authorization.review.acquisition,
                                    &authorization.review.evidence_index_sha256,
                                )
                            })
                            || shutdown.cancelled()
                        {
                            return Err(ServiceError::new(
                                ErrorKind::Unverified,
                                "Linux completion requires this owner's genuine current accepted and unstopped capture",
                            ));
                        }
                        true
                    } else {
                        false
                    };
                    #[cfg(all(
                        feature = "firmware-entry-study",
                        target_os = "linux",
                        not(feature = "firmware-linux-completion")
                    ))]
                    let completion_admitted = false;
                    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
                    if completion_admitted {
                    } else if let Some(study) = &request.entry_study {
                        study.revalidate()?;
                        if !study.matches_job(&request.job)
                            || actor.capture.as_ref().is_none_or(|capture| {
                                !capture.entry_study_matches(
                                    &study.review.reservation,
                                    &study.review.acquisition,
                                    &study.review.evidence_index_sha256,
                                )
                            })
                        {
                            return Err(ServiceError::new(
                                ErrorKind::Unverified,
                                "Entry study lacks this owner's genuine current accepted capture",
                            ));
                        }
                    } else {
                        crate::firmware_profile::admit_purpose(request.purpose())?;
                    }
                    #[cfg(not(all(feature = "firmware-entry-study", target_os = "linux")))]
                    crate::firmware_profile::admit_purpose(request.purpose())?;
                }
                if stop.requested.load(Ordering::SeqCst) {
                    return Err(ServiceError::new(
                        ErrorKind::Cancelled,
                        "Ownership cancelled before handoff",
                    ));
                }
                actor
                    .epoch()
                    .0
                    .checked_add(1)
                    .ok_or_else(|| ServiceError::invalid("Normal retirement epoch exhausted"))?;
                actor.firmware_quiescence()?;
                if capture.shared.blocks_firmware()
                    || !capture.quiescent()
                    || !commands.is_empty()
                    || !received.is_empty()
                    || overflow.load(Ordering::Acquire)
                    || source.failed()
                    || source.fence(processed).allocated != processed
                    || !events.is_empty()
                {
                    return Err(ServiceError::new(
                        ErrorKind::Busy,
                        "Normal queues/framing/results must be empty before updater ownership",
                    ));
                }
                session
                    .as_mut()
                    .ok_or_else(|| {
                        ServiceError::new(ErrorKind::Disconnected, "No normal MIDI owner")
                    })?
                    .health()?;
                Ok(())
            })();
            if let Err(error) = check {
                let _ = firmware_events.try_send(FirmwareOwnerEvent::Rejected {
                    error,
                    store: request.store,
                });
                wake();
            } else {
                #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
                if request
                    .linux_completion
                    .as_ref()
                    .is_some_and(|auth| Instant::now() >= auth.entry_deadline())
                    || stop.requested.load(Ordering::SeqCst)
                    || shutdown.cancelled()
                {
                    let _ = firmware_events.try_send(FirmwareOwnerEvent::Rejected {
                        error: ServiceError::new(
                            ErrorKind::Cancelled,
                            "Completion admission expired or stopped before normal retirement",
                        ),
                        store: request.store,
                    });
                    wake();
                    continue;
                }
                let before = source.fence(processed);
                match session
                    .as_mut()
                    .expect("checked normal owner")
                    .take_firmware_handoff()
                {
                    Err(error) => {
                        let _ = firmware_events.try_send(FirmwareOwnerEvent::Rejected {
                            error,
                            store: request.store,
                        });
                        wake();
                    }
                    Ok(mut handoff) => {
                        let closed = handoff.normal_close.take();
                        let retirement = closed.map(|receipt| {
                            crate::firmware_transition::normal_retirement(
                                receipt,
                                &source,
                                actor.epoch(),
                                handoff.binding.clone(),
                                before,
                                &received,
                                overflow.load(Ordering::Acquire),
                            )
                        });
                        drop(session.take());
                        selection = None;
                        discover_active = false;
                        actor.firmware_handed_off();
                        // The old accepted review cannot keep an already retired
                        // actor reserved. A later connection must create a fresh
                        // normal epoch and refresh; this does not resume editing.
                        actor.capture = None;
                        capture.shared.active.store(false, Ordering::Release);
                        capture.shared.closed.store(false, Ordering::Release);
                        match retirement {
                            Some((observation, Err(error))) => {
                                let _ = firmware_events.try_send(
                                    FirmwareOwnerEvent::RejectedAfterNormalRetirement {
                                        error,
                                        store: request.store,
                                        observation: Box::new(observation),
                                    },
                                );
                                wake();
                            }
                            Some((observation, Ok(seal))) => {
                                #[cfg(all(
                                    feature = "firmware-linux-completion",
                                    target_os = "linux"
                                ))]
                                {
                                    normal_source_retired = true;
                                }
                                handoff.retirement = Some(seal);
                                #[cfg(all(
                                    feature = "firmware-linux-completion",
                                    target_os = "linux"
                                ))]
                                if let Some(authorization) = request.linux_completion.take() {
                                    match crate::firmware_linux_completion::LinuxCompletionOwner::start(authorization,request.store,handoff,stop){
                                        Ok(owner)=>{request.linux_monitor.attach(&owner);updater=Some(ExclusiveFirmwareOwner::Linux(owner));}
                                        Err(failed)=>{let _=firmware_events.try_send(FirmwareOwnerEvent::StartupFailedWithStore{error:failed.error,store:failed.store,normal_retirement:Some(Box::new(observation)),updater_retirement:None});wake();}
                                    }
                                    continue;
                                }
                                match FirmwareUpdateOwner::start(request, handoff) {
                                    Ok(owner) => {
                                        updater =
                                            Some(ExclusiveFirmwareOwner::Legacy(Box::new(owner)))
                                    }
                                    Err(failed) => {
                                        let _ = firmware_events.try_send(
                                            FirmwareOwnerEvent::StartupFailedWithStore {
                                                error: failed.error,
                                                store: failed.store,
                                                normal_retirement: Some(Box::new(observation)),
                                                updater_retirement: failed
                                                    .updater_retirement
                                                    .map(Box::new),
                                            },
                                        );
                                        wake();
                                    }
                                }
                            }
                            None => {
                                // Public/custom handoffs cannot occur, but retain a
                                // conservative startup failure if an internal seam omitted proof.
                                let _ = firmware_events.try_send(FirmwareOwnerEvent::StartupFailedWithStore {
                                    error: ServiceError::invalid("Retired normal handoff omitted its concrete reader receipt"),
                                    store: request.store,
                                    normal_retirement: None,
                                    updater_retirement: None,
                                });
                                wake();
                            }
                        }
                    }
                }
            }
            continue;
        }

        capture.flush(&mut actor, &wake);
        if let Ok(command) = capture.commands.try_recv() {
            let review_control = matches!(
                &command,
                FirmwareBackupCaptureCommand::AcceptRevalidatedReview(_)
                    | FirmwareBackupCaptureCommand::ReturnToNormal
            );
            let result = match command {
                FirmwareBackupCaptureCommand::Begin(context) => {
                    let result = (|| {
                        if !commands.is_empty()
                            || !events.is_empty()
                            || !received.is_empty()
                            || overflow.load(Ordering::Acquire)
                        {
                            return Err(ServiceError::new(
                                ErrorKind::Busy,
                                "Existing normal command/result/callback rail prevents capture admission",
                            ));
                        }
                        let binding = session
                            .as_mut()
                            .ok_or_else(|| {
                                ServiceError::new(
                                    ErrorKind::Disconnected,
                                    "No normal owner for capture",
                                )
                            })?
                            .normal_capture_observation()?;
                        actor.capture_begin(
                            binding,
                            context,
                            source.clone(),
                            processed,
                            capture.shared.clone(),
                            origin.elapsed(),
                        )
                    })();
                    if let Err(error) = result {
                        capture.shared.active.store(false, Ordering::Release);
                        capture.reply(FirmwareBackupCaptureEvent::Rejected(error), &wake);
                    }
                    Ok(())
                }
                FirmwareBackupCaptureCommand::ContinueAfterDurableObservation(receipt) => {
                    match actor.capture.as_mut() {
                        Some(run) => run.continue_receipt(receipt),
                        None => Err(ServiceError::new(
                            ErrorKind::Busy,
                            "No actual capture reservation",
                        )),
                    }
                }
                FirmwareBackupCaptureCommand::CloseAfterBoundary(receipt) => {
                    match actor.capture.as_mut() {
                        Some(run) => run.close_receipt(receipt),
                        None => Err(ServiceError::new(
                            ErrorKind::Busy,
                            "No actual capture reservation",
                        )),
                    }
                }
                FirmwareBackupCaptureCommand::AcceptRevalidatedReview(result) => {
                    match actor.capture.as_mut() {
                        Some(run) => run.accept_revalidated_review(result),
                        None => Err(ServiceError::new(
                            ErrorKind::Busy,
                            "No actual capture reservation",
                        )),
                    }
                }
                FirmwareBackupCaptureCommand::ReturnToNormal => match actor.capture.as_mut() {
                    Some(run) => run.request_normal_return(),
                    None => Err(ServiceError::new(
                        ErrorKind::Busy,
                        "No privately accepted capture review",
                    )),
                },
            };
            if let Err(error) = result {
                if review_control {
                    capture.reply(FirmwareBackupCaptureEvent::Rejected(error), &wake);
                } else {
                    actor.lost(error);
                }
            }
        }
        if let Some(run) = actor.capture.as_mut()
            && let Err(error) = run.tick_durability()
        {
            actor.lost(error);
        }
        if actor.capture.as_ref().is_some_and(|r| r.return_ready()) {
            let retired_epoch = actor.epoch();
            let old_selection = selection.clone();
            let result = (|| {
                if !firmware_commands.is_empty()
                    || !firmware_events.is_empty()
                    || !received.is_empty()
                    || !commands.is_empty()
                    || !events.is_empty()
                    || !capture.quiescent()
                {
                    return Err(ServiceError::new(
                        ErrorKind::Busy,
                        "Normal return waits for actual result/control/callback rails",
                    ));
                }
                actor
                    .capture
                    .as_ref()
                    .expect("active capture")
                    .return_preflight(processed)?;
                let opened = session.as_mut().ok_or_else(|| {
                    ServiceError::new(ErrorKind::Disconnected, "Normal return owner disappeared")
                })?;
                opened.health()?;
                opened.close_normal_capture()?;
                actor
                    .capture
                    .as_ref()
                    .expect("active capture")
                    .return_preflight(processed)?;
                Ok(())
            })();
            match result {
                Ok(()) => {
                    drop(session.take());
                    selection = None;
                    discover_active = false;
                    actor.firmware_handed_off();
                    actor.capture = None;
                    capture.shared.active.store(false, Ordering::Release);
                    capture.shared.closed.store(false, Ordering::Release);
                    capture.reply(
                        FirmwareBackupCaptureEvent::NormalRefreshRequired {
                            retired_epoch,
                            next_epoch: actor.epoch(),
                            selection: old_selection,
                        },
                        &wake,
                    );
                }
                Err(error) if error.kind == ErrorKind::Busy => {}
                Err(error) => {
                    actor.lost(error.clone());
                    capture.shared.fail(error.clone());
                    capture.reply(FirmwareBackupCaptureEvent::Rejected(error), &wake);
                }
            }
        }
        if let Some(run) = actor.capture.as_mut()
            && let Err(error) = run.check_timeout(origin.elapsed())
        {
            actor.lost(error);
        }
        if let Some(opened) = &mut session
            && let Err(error) = opened.health()
        {
            actor.lost(error);
        }
        if overflow.swap(false, Ordering::AcqRel) {
            actor.lost(ServiceError::new(
                ErrorKind::Protocol,
                "MIDI callback queue overflowed; session is no longer trustworthy",
            ));
        }
        for _ in 0..32 {
            match commands.try_recv() {
                Ok(DeviceCommand::Shutdown) => {
                    shutdown.cancel();
                    break;
                }
                Ok(command) => actor.command(command, origin.elapsed()),
                Err(_) => break,
            }
        }
        for _ in 0..config.callback_capacity.max(1) {
            let Ok(callback) = received.try_recv() else {
                break;
            };
            if callback.clock_origin == source.clock_origin() {
                processed = callback.callback.ordinal;
            }
            if let Some(run) = actor.capture.as_mut() {
                let already_failed = run.is_failed();
                if let Err(error) = run.receive(callback, processed, origin.elapsed())
                    && !already_failed
                {
                    actor.lost(error);
                }
            } else {
                actor.receive(
                    callback.callback.epoch,
                    &callback.callback.retained,
                    origin.elapsed(),
                );
            }
        }
        if let Some(run) = actor.capture.as_mut()
            && let Err(error) = run.finish_callbacks(processed)
        {
            actor.lost(error);
        }
        capture.flush(&mut actor, &wake);
        actor.tick(origin.elapsed());
        if discover_active
            && origin.elapsed() >= next_discovery
            && (selection.is_none() || config.poll_ports_while_connected)
        {
            let result = backend.enumerate();
            if let Ok(ports) = &result {
                if last_ports.as_ref() != Some(ports) {
                    last_ports = Some(ports.clone());
                    actor.enumerated(result);
                }
            } else if selection.is_none() {
                actor.enumerated(result);
            }
            if let Some(selected) = &selection {
                match backend.selection_present(selected) {
                    Ok(false) => actor.lost(ServiceError::new(
                        ErrorKind::Disconnected,
                        "Selected MIDI ports disappeared; reconnect after Normal USB mode returns",
                    )),
                    Err(error) => actor.lost(error),
                    Ok(true) => {}
                }
            }
            next_discovery =
                origin.elapsed() + config.discovery_interval.max(Duration::from_millis(100));
        }
        while let Some(action) = actor.next_action() {
            if shutdown.cancelled() {
                break;
            }
            match action {
                TransportAction::Enumerate => {
                    discover_active = true;
                    let result = backend.enumerate();
                    if let Ok(ports) = &result {
                        last_ports = Some(ports.clone());
                    }
                    actor.enumerated(result);
                    next_discovery = origin.elapsed() + config.discovery_interval;
                }
                TransportAction::Open {
                    selection: selected,
                    epoch,
                } => {
                    discover_active = true;
                    overflow.store(false, Ordering::Release);
                    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
                    {
                        normal_source_retired = false;
                    }
                    source = NormalCallbackSource::new(origin, epoch);
                    processed = 0;
                    let sink = ReceiveSink::observed(
                        rx_sender.clone(),
                        overflow.clone(),
                        epoch,
                        source.clone(),
                    );
                    match backend.connect(&selected, sink) {
                        Ok(opened) => {
                            session = Some(opened);
                            selection = Some(selected);
                            actor.opened(Ok(()), origin.elapsed());
                        }
                        Err(error) => actor.opened(Err(error), origin.elapsed()),
                    }
                    next_discovery = origin.elapsed() + config.discovery_interval;
                }
                TransportAction::Close => {
                    session = None;
                    selection = None;
                }
                TransportAction::Send { packets } => {
                    let result = match &mut session {
                        None => Err(ServiceError::new(
                            ErrorKind::Disconnected,
                            "MIDI connection is closed",
                        )),
                        Some(opened) => {
                            let mut result = Ok(());
                            for (index, packet) in packets.iter().enumerate() {
                                if shutdown.cancelled() {
                                    result = Err(ServiceError::new(
                                        ErrorKind::Cancelled,
                                        "Shutdown during MIDI send",
                                    ));
                                    break;
                                }
                                if index > 0 {
                                    thread::sleep(
                                        config.packet_spacing.min(Duration::from_millis(100)),
                                    );
                                }
                                actor.trace_wire("midi-tx", packet, origin.elapsed());
                                if let Err(error) = opened.send(packet) {
                                    result = Err(error);
                                    break;
                                }
                            }
                            result
                        }
                    };
                    actor.sent(result, origin.elapsed());
                }
                TransportAction::CaptureRead {
                    reservation,
                    sequence,
                } => {
                    let result = match (actor.capture.as_mut(), session.as_mut()) {
                        (Some(run), Some(opened)) => {
                            if run.matches_offer(&reservation, sequence) {
                                run.offer(opened.as_mut(), processed, origin.elapsed(), &shutdown)
                            } else {
                                Err(ServiceError::new(
                                    ErrorKind::Conflict,
                                    "Tagged capture reservation/read differs",
                                ))
                            }
                        }
                        _ => Err(ServiceError::new(
                            ErrorKind::Conflict,
                            "Actual tagged capture reservation/read no longer owns normal session",
                        )),
                    };
                    if let Err(error) = result {
                        actor.lost(error);
                    }
                    capture.flush(&mut actor, &wake);
                }
                TransportAction::Event(event) => {
                    if matches!(&event, DeviceEvent::OperationFailed { error, .. } if matches!(error.kind, ErrorKind::Timeout | ErrorKind::Indeterminate))
                        && let Ok(clients) = std::fs::read("/proc/asound/seq/clients")
                    {
                        let _ = events.try_send(DeviceEvent::Diagnostic(DiagnosticEvent {
                            category: "alsa-sequencer-clients".into(),
                            message: String::from_utf8_lossy(&clients).into_owned(),
                            bytes: None,
                            epoch: Some(actor.epoch()),
                        }));
                        wake();
                    }
                    let noncritical = matches!(
                        event,
                        DeviceEvent::Diagnostic(_) | DeviceEvent::IncomingNotification { .. }
                    );
                    if noncritical {
                        match events.try_send(event) {
                            Ok(()) => wake(),
                            Err(TrySendError::Full(_)) => dropped_notifications += 1,
                            Err(TrySendError::Disconnected(_)) => {
                                shutdown.cancel();
                                break;
                            }
                        }
                    } else {
                        let mut pending_event = event;
                        loop {
                            match events.send_timeout(pending_event, Duration::from_millis(25)) {
                                Ok(()) => {
                                    wake();
                                    break;
                                }
                                Err(SendTimeoutError::Timeout(event)) => {
                                    if shutdown.cancelled() {
                                        break;
                                    }
                                    if let Ok(request) = firmware_commands.try_recv() {
                                        let error = ServiceError::new(
                                            ErrorKind::Busy,
                                            "An undelivered normal result prevents firmware handoff",
                                        );
                                        let _ = firmware_events.try_send(
                                            FirmwareOwnerEvent::Rejected {
                                                error,
                                                store: request.store,
                                            },
                                        );
                                        wake();
                                    }
                                    capture.flush(&mut actor, &wake);
                                    if actor.capture.is_none()
                                        && let Ok(FirmwareBackupCaptureCommand::Begin(_)) =
                                            capture.commands.try_recv()
                                    {
                                        capture.shared.active.store(false, Ordering::Release);
                                        capture.reply(FirmwareBackupCaptureEvent::Rejected(ServiceError::new(ErrorKind::Busy,"Undelivered normal result prevents capture admission")),&wake);
                                    }
                                    wake();
                                    pending_event = event;
                                }
                                Err(SendTimeoutError::Disconnected(_)) => {
                                    shutdown.cancel();
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        if dropped_notifications > 0 && !events.is_full() {
            let event = DeviceEvent::Diagnostic(DiagnosticEvent {
                category: "event-backpressure".into(),
                message: format!(
                    "Suppressed {dropped_notifications} unsolicited/diagnostic events while the bounded UI queue was full; transaction results retained"
                ),
                bytes: None,
                epoch: Some(actor.epoch()),
            });
            if events.try_send(event).is_ok() {
                dropped_notifications = 0;
                wake();
            }
        }
        // Only the worker sleeps. GUI submissions and callbacks use try_send.
        thread::sleep(Duration::from_millis(3));
    }
    if let Some(mut owner) = updater {
        owner.request_host_stop();
        let _ = owner.retire();
    }
    capture.shared.stop();
    // Retain a fixed available callback prefix before queue/actor destruction.
    // The source may still allocate afterward; snapshots expose that suffix and
    // make no joined-reader result or final-frontier completeness claim.
    if let Some(run) = actor.capture.as_mut() {
        for _ in 0..config
            .callback_capacity
            .clamp(1, gp200_domain::FIRMWARE_CAPTURE_CHUNK_LIMIT)
        {
            let Ok(callback) = received.try_recv() else {
                break;
            };
            if callback.clock_origin == source.clock_origin() {
                processed = callback.callback.ordinal;
            }
            run.retain_passive_callback(callback, processed);
        }
    }
    // Preserve bounded run-held originals before dropping the actual actor and
    // source on shutdown, including passive post-Final cancellation evidence.
    capture.flush(&mut actor, &wake);
    drop(session);
}
