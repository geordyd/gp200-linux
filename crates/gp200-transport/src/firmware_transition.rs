//! Private transition mechanics shared by production owners and synthetic tests.
//! None of the descriptive fields below issues a physical opener or clearance.
use crate::backend::{NormalCallbackSource, NormalReaderCloseReceipt, ObservedCallback};
use crate::firmware_backend::{FirmwareIo, FirmwareReaderRetirement, FirmwareReceiveSink};
use crate::{DeviceActor, MidiSession, ReceiveSink, TransportAction, TransportConfig};
use crossbeam_channel::Receiver;
use gp200_domain::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

pub(crate) struct NormalRetirementSeal {
    _receipt: NormalReaderCloseReceipt,
    binding_sha256: String,
}
impl NormalRetirementSeal {
    pub(crate) fn matches(&self, binding: &FirmwareUsbObservation, epoch: SessionEpoch) -> bool {
        self.binding_sha256 == binding.digest() && self._receipt.epoch == epoch
    }
}
pub(crate) fn normal_retirement(
    receipt: NormalReaderCloseReceipt,
    source: &Arc<NormalCallbackSource>,
    epoch: SessionEpoch,
    binding: FirmwareUsbObservation,
    before: FirmwareCaptureFence,
    received: &Receiver<ObservedCallback>,
    overflow: bool,
) -> (
    FirmwareNormalRetirementObservation,
    Result<NormalRetirementSeal, ServiceError>,
) {
    let mut observation = FirmwareNormalRetirementObservation {
        normal_epoch: receipt.epoch,
        owner_generation: epoch.0,
        source_instance: receipt.source.clock_origin().to_owned(),
        clock_origin: receipt.source.clock_origin().to_owned(),
        binding,
        close_started_at_ns: receipt.started,
        close_finished_at_ns: receipt.finished,
        allocated_before: before.allocated,
        processed_before: before.processed,
        allocated_after: receipt.allocated,
        processed_after: before.processed,
        reader_join: receipt.join,
        output_handle_destroyed: receipt.output_destroyed,
        reader_fault: receipt.fault.clone(),
        overflow: overflow || receipt.overflow,
        retained_raw: Vec::new(),
        missing: Vec::new(),
        first_failure: None,
    };
    let mut ordered = true;
    for _ in 0..FIRMWARE_NORMAL_RETIREMENT_RAW_LIMIT {
        let Ok(original) = received.try_recv() else {
            break;
        };
        let r = original.callback;
        ordered &= original.clock_origin == observation.clock_origin
            && r.epoch == epoch
            && observation.processed_after.checked_add(1) == Some(r.ordinal);
        if ordered {
            observation.processed_after = r.ordinal;
        }
        observation
            .retained_raw
            .push(FirmwareNormalRetirementRawObservation {
                sequence: r.ordinal,
                epoch: r.epoch,
                at_ns: r.monotonic_ns,
                original_length: r.original_len,
                raw: r.retained[..r.retained.len().min(FIRMWARE_NORMAL_RETIREMENT_CHUNK_LIMIT)]
                    .to_vec(),
            });
    }
    if let Some(r) = receipt.source.first_failure() {
        observation.first_failure = Some(FirmwareNormalRetirementRawObservation {
            sequence: r.ordinal,
            epoch: r.epoch,
            at_ns: r.monotonic_ns,
            original_length: r.original_len,
            raw: r.retained[..r.retained.len().min(FIRMWARE_NORMAL_RETIREMENT_CHUNK_LIMIT)]
                .to_vec(),
        });
    }
    if observation.processed_after < observation.allocated_after {
        observation.missing.push(FirmwareNormalRetirementGap {
            source_instance: observation.source_instance.clone(),
            first_sequence: observation.processed_after.saturating_add(1),
            last_sequence: observation.allocated_after,
            missing_length: None,
            reason: "Allocated callbacks outside the retained ordered prefix remain unaccounted"
                .into(),
        });
    }
    let valid = Arc::ptr_eq(source, &receipt.source)
        && receipt.epoch == epoch
        && before.allocated == before.processed
        && receipt.allocated == before.allocated
        && receipt.join == FirmwareNormalReaderJoinObservation::Returned
        && receipt.output_destroyed
        && receipt.fault.is_none()
        && !observation.overflow
        && !source.failed()
        && receipt.started.is_some()
        && receipt.finished.is_some()
        && ordered
        && observation.retained_raw.is_empty()
        && received.is_empty();
    let result = observation.retained_json().and_then(|_| {
        if valid { Ok(NormalRetirementSeal { binding_sha256: observation.binding.digest(), _receipt: receipt }) }
        else { Err(ServiceError::new(ErrorKind::Unverified,
            "Normal reader retired with changed source, late callbacks, missing frontier or failed join; fresh guarded connection required")) }
    });
    (observation, result)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransitionPolicy {
    Reenumerated,
    SameEnumerationProtocolChange,
}
impl From<FirmwareLifecycleTransitionKind> for TransitionPolicy {
    fn from(value: FirmwareLifecycleTransitionKind) -> Self {
        match value {
            FirmwareLifecycleTransitionKind::Reenumerated => Self::Reenumerated,
            FirmwareLifecycleTransitionKind::SameEnumerationProtocolChange => {
                Self::SameEnumerationProtocolChange
            }
        }
    }
}
pub(crate) struct TransitionCandidate {
    pub(crate) binding: FirmwareUsbObservation,
    pub(crate) enumeration: u64,
    pub(crate) epoch: SessionEpoch,
    pub(crate) generation: u64,
    pub(crate) source: String,
    pub(crate) identity: Vec<u8>,
    pub(crate) negotiation: Vec<u8>,
    pub(crate) ready: bool,
}
pub(crate) struct TransitionExpected {
    pub(crate) binding: FirmwareUsbObservation,
    pub(crate) identity: Vec<u8>,
    pub(crate) negotiation: Vec<u8>,
}
pub(crate) struct TransitionCandidates {
    pub(crate) candidates: Vec<TransitionCandidate>,
    pub(crate) old_disappeared: bool,
    pub(crate) coexist: bool,
}
pub(crate) struct TransitionProfile {
    pub(crate) sha256: JobSha256,
    pub(crate) nonblock_actions: u64,
}
/// Private driver ownership, unavailable from public requests or DTOs. A future
/// physical issuer must supply its own separately accepted opener.
pub(crate) trait TransitionDriver: Send {
    fn profile(&self) -> TransitionProfile;
    fn initial_identity(&self) -> (Vec<u8>, Vec<u8>);
    fn normal_mode(&self) -> BackendMode;
    fn normal_config(&self) -> TransportConfig;
    #[cfg(test)]
    fn ledger_stack_size(&self) -> Option<usize> {
        None
    }
    fn policy(&self) -> FirmwareLifecycleTransitionKind;
    fn expected(&self, role: FirmwareLifecycleEpochRole) -> TransitionExpected;
    #[cfg(target_os = "linux")]
    fn endpoints(
        &self,
        candidate: &TransitionCandidate,
    ) -> Vec<crate::device_session_guard::EndpointIdentity>;
    fn initial_enumeration(&self) -> u64;
    fn candidates(
        &mut self,
        role: FirmwareLifecycleEpochRole,
    ) -> Result<TransitionCandidates, ServiceError>;
    fn open_updater(
        &mut self,
        candidate: &TransitionCandidate,
        sink: FirmwareReceiveSink,
    ) -> Result<Box<dyn FirmwareIo>, ServiceError>;
    fn selection(&self) -> DeviceSelection;
    fn application_version(
        &self,
        _original_identity: &[u8],
    ) -> Result<Option<String>, ServiceError> {
        Ok(None)
    }
    fn open_normal(
        &mut self,
        candidate: &TransitionCandidate,
        sink: ReceiveSink,
    ) -> Result<Box<dyn MidiSession>, ServiceError>;
}
pub(crate) struct TransitionCorrelation {
    pub(crate) run: String,
    pub(crate) review: String,
    pub(crate) profile: String,
    pub(crate) image: String,
    pub(crate) sequence: u64,
    pub(crate) epoch: SessionEpoch,
    pub(crate) generation: u64,
    pub(crate) source: String,
    pub(crate) binding: String,
}
pub(crate) struct TransitionContinuation {
    correlation: TransitionCorrelation,
    previous: FirmwareLifecycleEpochObservation,
    pub(crate) candidate: TransitionCandidate,
    pub(crate) observation: FirmwareLifecycleEpochObservation,
    _retirement: FirmwareReaderRetirement,
}
impl TransitionContinuation {
    pub(crate) fn consume(
        self,
        correlation: &TransitionCorrelation,
        stopped: bool,
    ) -> Result<(TransitionCandidate, FirmwareLifecycleEpochObservation), ServiceError> {
        if stopped
            || self.correlation.run != correlation.run
            || self.correlation.review != correlation.review
            || self.correlation.profile != correlation.profile
            || self.correlation.image != correlation.image
            || self.correlation.sequence != correlation.sequence
            || self.correlation.epoch != correlation.epoch
            || self.correlation.generation != correlation.generation
            || self.correlation.source != correlation.source
            || self.correlation.binding != correlation.binding
            || self.observation.previous_epoch != Some(self.previous.epoch)
            || self.observation.previous_binding_sha256.as_ref()
                != Some(&self.previous.binding.digest())
        {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Transition continuation cancelled or privately correlated owner changed",
            ));
        }
        Ok((self.candidate, self.observation))
    }
}
pub(crate) struct TransitionRequest<'a> {
    pub(crate) previous: &'a FirmwareLifecycleEpochObservation,
    pub(crate) previous_enumeration: u64,
    pub(crate) role: FirmwareLifecycleEpochRole,
    pub(crate) retirement: FirmwareReaderRetirement,
    pub(crate) processed: u64,
    pub(crate) last_ns: u64,
    pub(crate) policy: TransitionPolicy,
    pub(crate) candidates: TransitionCandidates,
    pub(crate) expected: TransitionExpected,
    pub(crate) correlation: TransitionCorrelation,
}
pub(crate) fn prepare_transition(
    request: TransitionRequest<'_>,
) -> Result<TransitionContinuation, ServiceError> {
    let TransitionRequest {
        previous,
        previous_enumeration,
        role,
        retirement,
        processed,
        last_ns,
        policy,
        candidates,
        expected,
        correlation,
    } = request;
    let r = retirement.observation();
    if !retirement.is_fresh()
        || r.epoch != previous.epoch
        || r.generation != previous.generation
        || r.source_instance != previous.source_instance
        || r.clock_origin != previous.clock_origin
        || r.allocated != processed
        || r.processed != processed
        || r.close_started_at_ns < last_ns
        || r.reader_join != FirmwareNormalReaderJoinObservation::Returned
        || r.fault_or_missing
        || candidates.candidates.len() != 1
        || candidates.coexist
        || previous_enumeration == 0
    {
        return Err(ServiceError::new(
            ErrorKind::Unverified,
            "Actual source retirement or unique fresh candidate is unresolved",
        ));
    }
    let candidate = candidates
        .candidates
        .into_iter()
        .next()
        .expect("exact unique candidate");
    candidate.binding.validate()?;
    if candidate.binding != expected.binding
        || candidate.identity != expected.identity
        || candidate.negotiation != expected.negotiation
    {
        return Err(ServiceError::new(
            ErrorKind::Unverified,
            "Privately specified model/interface/protocol mode relation differs",
        ));
    }
    let kind = match policy {
        TransitionPolicy::Reenumerated
            if candidates.old_disappeared && candidate.enumeration > previous_enumeration =>
        {
            FirmwareLifecycleTransitionKind::Reenumerated
        }
        TransitionPolicy::SameEnumerationProtocolChange
            if candidate.enumeration == previous_enumeration =>
        {
            FirmwareLifecycleTransitionKind::SameEnumerationProtocolChange
        }
        _ => {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Transition disappearance/enumeration policy differs",
            ));
        }
    };
    if previous.epoch.0.checked_add(1) != Some(candidate.epoch.0)
        || previous.generation.checked_add(1) != Some(candidate.generation)
        || candidate.source == previous.source_instance
        || candidate.source.is_empty()
        || candidate.binding.topology != previous.binding.topology
        || candidate.binding.serial != previous.binding.serial
        || !candidate.ready
        || candidate.identity.is_empty()
        || candidate.negotiation.is_empty()
        || candidate.identity == previous.raw_identity
        || candidate.negotiation == previous.negotiation
    {
        return Err(ServiceError::new(
            ErrorKind::Unverified,
            "Fresh source, readiness, physical relation or protocol-mode identity differs",
        ));
    }
    let observation = FirmwareLifecycleEpochObservation {
        role,
        epoch: candidate.epoch,
        generation: candidate.generation,
        binding: candidate.binding.clone(),
        previous_binding_sha256: Some(previous.binding.digest()),
        previous_epoch: Some(previous.epoch),
        source_instance: candidate.source.clone(),
        clock_origin: previous.clock_origin.clone(),
        retired_source: Some(r.clone()),
        raw_identity: candidate.identity.clone(),
        negotiation: candidate.negotiation.clone(),
        transition: Some(FirmwareLifecycleTransitionObservation {
            kind,
            previous_enumeration_generation: previous_enumeration,
            enumeration_generation: candidate.enumeration,
            old_source_disappeared: candidates.old_disappeared,
            old_and_new_coexisted: candidates.coexist,
            candidate_count: 1,
        }),
        post_normal_refresh: None,
    };
    observation.retained_json()?;
    Ok(TransitionContinuation {
        correlation,
        previous: previous.clone(),
        candidate,
        observation,
        _retirement: retirement,
    })
}

/// Run fresh normal reads through the real actor/parser, with its own explicit
/// source namespace. These read offers never use the updater allocator.
pub(crate) fn refresh_normal(
    driver: &mut dyn TransitionDriver,
    candidate: &TransitionCandidate,
    stop: &AtomicBool,
) -> Result<FirmwarePostNormalRefreshObservation, ServiceError> {
    let origin = Instant::now();
    let source = NormalCallbackSource::new(origin, candidate.epoch);
    let overflow = Arc::new(AtomicBool::new(false));
    let (tx, received) = crossbeam_channel::bounded(64);
    let sink = ReceiveSink::observed(tx, overflow.clone(), candidate.epoch, source.clone());
    let mut session = driver.open_normal(candidate, sink)?;
    let mut actor = DeviceActor::with_mode(driver.normal_config(), driver.normal_mode());
    actor.set_fresh_epoch_before_connect(candidate.epoch)?;
    actor.command(
        DeviceCommand::Connect {
            intent: IntentId(1),
            selection: driver.selection(),
        },
        Duration::ZERO,
    );
    let mut processed = 0;
    let mut identity = Vec::new();
    let mut decoder = gp200_protocol::StreamDecoder::new();
    let mut assembler = gp200_protocol::Reassembler::default();
    let mut reads = 0;
    let mut opened = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if stop.load(Ordering::SeqCst) || Instant::now() >= deadline {
            return Err(ServiceError::new(
                ErrorKind::Cancelled,
                "Fresh post-update normal reads cancelled or exceeded bound",
            ));
        }
        session.health()?;
        while let Some(action) = actor.next_action() {
            match action {
                TransportAction::Open { .. } => {
                    opened = true;
                    actor.opened(Ok(()), origin.elapsed());
                }
                TransportAction::Send { packets } => {
                    for packet in packets {
                        session.send(&packet)?;
                    }
                    actor.sent(Ok(()), origin.elapsed());
                }
                TransportAction::Event(DeviceEvent::ConnectionChanged {
                    state: ConnectionState::Ready,
                    ..
                }) if reads == 0 => {
                    reads = 1;
                    actor.command(
                        DeviceCommand::ReadGlobals(RequestStamp {
                            epoch: actor.epoch(),
                            intent: IntentId(2),
                        }),
                        origin.elapsed(),
                    );
                }
                TransportAction::Event(DeviceEvent::GlobalStateReceived { .. }) if reads == 1 => {
                    reads = 2;
                    actor.command(
                        DeviceCommand::ReadStored {
                            stamp: RequestStamp {
                                epoch: actor.epoch(),
                                intent: IntentId(3),
                            },
                            slot: SlotId::new(0).expect("stored assessment slot"),
                        },
                        origin.elapsed(),
                    );
                }
                TransportAction::Event(DeviceEvent::SnapshotReceived {
                    target: PresetTarget::Stored(slot),
                    ..
                }) if reads == 2 && slot == SlotId::new(0).expect("stored assessment slot") => {
                    reads = 3;
                }
                TransportAction::Event(DeviceEvent::OperationFailed { error, .. }) => {
                    return Err(error);
                }
                TransportAction::Close if !opened => {}
                TransportAction::Close => {
                    return Err(ServiceError::new(
                        ErrorKind::Disconnected,
                        "Fresh normal actor closed before complete read set",
                    ));
                }
                _ => {}
            }
        }
        for _ in 0..64 {
            let Ok(original) = received.try_recv() else {
                break;
            };
            let r = original.callback;
            if r.epoch != candidate.epoch
                || r.monotonic_ns.is_none()
                || r.ordinal != processed + 1
                || original.clock_origin != source.clock_origin()
                || r.original_len != r.retained.len() as u64
            {
                return Err(ServiceError::invalid(
                    "Fresh normal callback source/order/extent differs",
                ));
            }
            for event in decoder.feed(&r.retained) {
                if let gp200_protocol::StreamEvent::Packet(packet) = event
                    && let Some(raw) = assembler
                        .add(&packet, origin.elapsed())
                        .map_err(|e| ServiceError::invalid(e.to_string()))?
                    && raw.records().iter().any(|r| r.command == 0x2001)
                {
                    if !identity.is_empty()
                        || raw.payload().len() > FIRMWARE_LIFECYCLE_IDENTITY_LIMIT
                    {
                        return Err(ServiceError::invalid(
                            "Fresh identity duplicate or oversized",
                        ));
                    }
                    identity = raw.payload().to_vec();
                }
            }
            processed = r.ordinal;
            actor.receive(r.epoch, &r.retained, origin.elapsed());
        }
        if source.failed() || overflow.load(Ordering::Acquire) {
            return Err(ServiceError::invalid("Fresh normal source lost originals"));
        }
        if reads == 3 {
            break;
        }
        actor.tick(origin.elapsed());
        std::thread::sleep(Duration::from_millis(1));
    }
    if source.fence(processed).allocated != processed || !received.is_empty() || actor.is_busy() {
        return Err(ServiceError::invalid(
            "Fresh normal read frontier unresolved",
        ));
    }
    let current = actor
        .snapshot(PresetTarget::EditBuffer)
        .ok_or_else(|| ServiceError::invalid("Fresh current snapshot absent"))?;
    let globals = actor
        .global_state()
        .ok_or_else(|| ServiceError::invalid("Fresh globals absent"))?;
    let stored = actor
        .snapshot(PresetTarget::Stored(
            SlotId::new(0).expect("stored assessment slot"),
        ))
        .ok_or_else(|| ServiceError::invalid("Fresh stored assessment absent"))?;
    let application_version = driver.application_version(&identity)?;
    let assessment = if application_version.is_some() {
        "Fresh actor/parser reads retain application version evidence from the original normal identity; no physical completion evidence"
    } else {
        "Fresh actor/parser identity, current, globals and stored slot 0; application version not observed"
    };
    let result = FirmwarePostNormalRefreshObservation {
        epoch: candidate.epoch,
        source_instance: source.clock_origin().into(),
        clock_origin: source.clock_origin().into(),
        allocated: processed,
        processed,
        identity_sha256: JobSha256::of(&identity).hex(),
        current_sha256: JobSha256::of(current.raw.payload()).hex(),
        globals_sha256: JobSha256::of(globals.records().raw().payload()).hex(),
        stored_sha256: JobSha256::of(stored.raw.payload()).hex(),
        current_revision: current.revision.0,
        globals_revision: globals.revision.0,
        stored_revision: stored.revision.0,
        stored_slot: 0,
        application_version,
        assessment: assessment.into(),
    };
    session.close_normal_capture()?;
    if source.fence(processed).allocated != processed || source.failed() || !received.is_empty() {
        return Err(ServiceError::invalid(
            "Fresh normal close changed its source frontier",
        ));
    }
    Ok(result)
}
