//! One privately derived action, one genuine journal intent, one unresolved offer.
use crate::{firmware_backend::*, firmware_service::*};
use gp200_domain::*;
use gp200_library::*;
#[cfg(test)]
use gp200_protocol::{FirmwareDescriptor, FirmwareFixedPacket};
use std::{
    collections::VecDeque,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};
#[derive(Clone, Copy, PartialEq)]
enum Wait {
    Begin,
    Plan,
    Checkpoint,
    Send,
    Raw,
    Decision,
    Close,
    Failure,
    Epoch,
    EntrySettled,
}
enum OwnerAuthority {
    Physical(crate::firmware_profile::PhysicalFirmwarePermit),
    Entry(Box<crate::firmware_profile::ExperimentalEntryPermit>),
    #[cfg(test)]
    Mock(crate::firmware_profile::MockFirmwareAuthority),
}
struct FirmwareDispatchTicket {
    checkpoint: FirmwareLocalActionCheckpoint,
    action: FirmwareLifecycleActionDescription,
    generation: u64,
    review: String,
}
pub(crate) struct FirmwareUpdateOwner {
    job: Arc<ReviewedFirmwareUpdate>,
    ledger: Option<FirmwareLedgerWorker>,
    run: String,
    journal_root: String,
    plan: FirmwareLifecyclePlanDto,
    io: Box<dyn FirmwareIo>,
    receiver: FirmwareReceiver,
    stop: Arc<OwnerStop>,
    attempt: Option<FirmwareAttemptId>,
    intent: u64,
    sequence: u64,
    waiting: Option<Wait>,
    deadline: Instant,
    action: Option<FirmwareLifecycleActionDescription>,
    checkpoint: Option<FirmwareLocalActionCheckpoint>,
    ordinal: u64,
    offset: usize,
    tries: u32,
    begun: Option<(u64, u64)>,
    send: Option<FirmwareLifecycleSendObservation>,
    raw: VecDeque<FirmwareLifecycleRawObservation>,
    // Original chunks already journalled for the unresolved action remain available
    // for a negative interpretation without substituting reconstructed frames.
    action_raw: Vec<FirmwareLifecycleRawObservation>,
    frames: VecDeque<(u64, u64, Vec<u8>)>,
    stream: Vec<u8>,
    frame_start: u64,
    received: u64,
    last_ns: u64,
    first_raw: u64,
    summary: FirmwareOwnerSummary,
    finished: bool,
    calls: u64,
    zero_cancel: bool,
    fault_note: Option<String>,
    failure_records: u64,
    #[cfg(target_os = "linux")]
    quarantine: Option<crate::firmware_quarantine::QuarantineAttempt>,
    // Locks outlive close and the real ledger join, including fsync stalls.
    _handoff: FirmwareNormalHandoff,
    authority: OwnerAuthority,
    entry: Option<FirmwareEntryObservationPlanDto>,
    normal_entry: Option<NormalEntryBinding>,
    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
    entry_observation_started: Option<Instant>,
    transition: Option<Box<dyn crate::firmware_transition::TransitionDriver>>,
    epochs: Vec<FirmwareLifecycleEpochObservation>,
    pending_epoch: Option<FirmwareLifecycleEpochObservation>,
    enumeration: u64,
    #[cfg(test)]
    mock_registry: Option<std::path::PathBuf>,
}
pub(crate) struct FirmwareOwnerStartFailure {
    pub(crate) error: ServiceError,
    pub(crate) store: LibraryStore,
    pub(crate) updater_retirement: Option<FirmwareStartupRetirementObservation>,
}
struct FirmwareOwnerStartPreparation {
    plan: FirmwareLifecyclePlanDto,
    receiver: FirmwareReceiver,
    io: Box<dyn FirmwareIo>,
    authority: OwnerAuthority,
    journal_root: String,
    normal_entry: Option<NormalEntryBinding>,
    #[cfg(test)]
    mock_path: Option<std::path::PathBuf>,
}
impl FirmwareUpdateOwner {
    pub(crate) fn start(
        request: FirmwareOwnershipRequest,
        mut handoff: FirmwareNormalHandoff,
    ) -> Result<Self, Box<FirmwareOwnerStartFailure>> {
        let purpose = request.purpose();
        #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
        let entry_profile = request.entry_study.as_ref().map(|v| v.profile_sha256());
        let FirmwareOwnershipRequest {
            job,
            store,
            stop,
            #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
            entry_study,
            #[cfg(test)]
            mock,
            ..
        } = request;
        let prepared = (|| -> Result<FirmwareOwnerStartPreparation, ServiceError> {
            let binding = handoff.binding.clone();
            let profile = handoff.transition.as_ref().map_or_else(
                || crate::firmware_transition::TransitionProfile {
                    sha256: JobSha256::of(b"private synthetic FW03 absolute action correlation v1"),
                    nonblock_actions: 17,
                },
                |driver| driver.profile(),
            );
            let plan = FirmwareLifecyclePlanDto {
                review_sha256: job.digest().hex(),
                profile_sha256: if purpose
                    == crate::firmware_profile::FirmwarePurpose::NormalEntryObservation
                {
                    JobSha256::of(b"private normal entry two-candidate single-offer observation v1")
                        .hex()
                } else {
                    profile.sha256.hex()
                },
                origin: FirmwareObservationOrigin::LocalObservation,
                nonblock_actions: if purpose
                    == crate::firmware_profile::FirmwarePurpose::NormalEntryObservation
                {
                    2
                } else {
                    profile.nonblock_actions
                },
                epoch: SessionEpoch(
                    handoff
                        .normal_epoch
                        .0
                        .checked_add(1)
                        .ok_or_else(|| ServiceError::invalid("Firmware epoch exhausted"))?,
                ),
                binding,
                generation: 1,
                clock_origin: "fw03-owner-monotonic".into(),
            };
            #[cfg(test)]
            let mut plan = plan;
            #[cfg(test)]
            let (mock_authority, mock_path) = match mock {
                Some((a, p)) => (Some(a), Some(p)),
                None => (None, None),
            };
            #[cfg(test)]
            if mock_path.is_some() {
                plan.origin = FirmwareObservationOrigin::MockSimulation;
            }
            #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
            let plan = {
                let mut plan = plan;
                if let Some(profile) = &entry_profile {
                    plan.profile_sha256 = profile.clone();
                }
                plan
            };
            plan.validate(&job)?;
            let journal_path = store.root().join("firmware-attempt-ledger");
            let journal_root = journal_path
                .to_str()
                .ok_or_else(|| {
                    ServiceError::invalid("Journal reference must retain exact UTF-8 path")
                })?
                .to_owned();
            if journal_root.len() > 4096 || journal_root.chars().any(char::is_control) {
                return Err(ServiceError::invalid(
                    "Journal reference cannot be retained exactly",
                ));
            }
            let normal_entry =
                if purpose == crate::firmware_profile::FirmwarePurpose::NormalEntryObservation {
                    Some(NormalEntryBinding::from_handoff(&handoff)?)
                } else {
                    None
                };
            let receiver = FirmwareReceiver::new(&plan);
            let io: Box<dyn FirmwareIo>;
            let authority: OwnerAuthority;
            #[cfg(test)]
            if mock_path.is_some() {
                authority = OwnerAuthority::Mock(mock_authority.expect("private authority"));
                io = match handoff.mock.take() {
                    Some(io) => io,
                    None => {
                        return Err(ServiceError::invalid("Mock handoff has no private backend"));
                    }
                };
            } else {
                let observed = Self::physical_io(
                    &mut handoff,
                    &receiver,
                    purpose,
                    normal_entry.as_ref(),
                    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
                    entry_study,
                    &job,
                    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
                    &stop,
                )?;
                io = observed.0;
                authority = observed.1;
            }
            #[cfg(not(test))]
            {
                let observed = Self::physical_io(
                    &mut handoff,
                    &receiver,
                    purpose,
                    normal_entry.as_ref(),
                    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
                    entry_study,
                    &job,
                    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
                    &stop,
                )?;
                io = observed.0;
                authority = observed.1;
            }
            #[cfg(test)]
            let io = {
                let mut io = io;
                io.attach(receiver.sink.clone());
                io
            };
            Ok(FirmwareOwnerStartPreparation {
                plan,
                receiver,
                io,
                authority,
                journal_root,
                normal_entry,
                #[cfg(test)]
                mock_path,
            })
        })();
        let FirmwareOwnerStartPreparation {
            plan,
            receiver,
            mut io,
            authority,
            journal_root,
            normal_entry,
            #[cfg(test)]
            mock_path,
        } = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                return Err(Box::new(FirmwareOwnerStartFailure {
                    error,
                    store,
                    updater_retirement: None,
                }));
            }
        };
        #[cfg(test)]
        let started = match handoff
            .transition
            .as_ref()
            .and_then(|driver| driver.ledger_stack_size())
        {
            Some(size) => FirmwareLedgerWorker::spawn_retaining_store_with_builder(
                store,
                std::thread::Builder::new()
                    .name("gp200-firmware-ledger".into())
                    .stack_size(size),
            ),
            None => FirmwareLedgerWorker::spawn_retaining_store(store),
        };
        #[cfg(not(test))]
        let started = FirmwareLedgerWorker::spawn_retaining_store(store);
        let ledger = match started {
            Ok(ledger) => ledger,
            Err(failed) => {
                let (error, store) = failed.into_parts();
                let closed = io.close();
                let mut retained_raw_chunks: Vec<_> =
                    receiver.received.try_iter().take(64).collect();
                if let Some(first) = receiver.sink.first_failure()
                    && !retained_raw_chunks.contains(&first)
                {
                    retained_raw_chunks.push(first);
                }
                return Err(Box::new(FirmwareOwnerStartFailure {
                    error,
                    store,
                    updater_retirement: Some(FirmwareStartupRetirementObservation {
                        source: closed.observation().clone(),
                        reader_fault: closed.fault.clone(),
                        output_handle_destroyed: closed.output_destroyed,
                        retained_raw_chunks,
                    }),
                }));
            }
        };
        let run = ledger.run_id().to_owned();
        let transition = handoff.transition.take();
        let enumeration = transition.as_ref().map_or(0, |d| d.initial_enumeration());
        let entry = (purpose == crate::firmware_profile::FirmwarePurpose::NormalEntryObservation)
            .then(|| crate::firmware_profile::entry_plan(plan.clone()));
        let mut owner = Self {
            job,
            ledger: Some(ledger),
            run,
            journal_root,
            plan,
            io,
            receiver,
            stop,
            attempt: None,
            intent: 0,
            sequence: 0,
            waiting: None,
            deadline: Instant::now(),
            action: None,
            checkpoint: None,
            ordinal: 0,
            offset: 0,
            tries: 0,
            begun: None,
            send: None,
            raw: VecDeque::new(),
            action_raw: Vec::new(),
            frames: VecDeque::new(),
            stream: Vec::new(),
            frame_start: 0,
            received: 0,
            last_ns: 0,
            first_raw: 0,
            summary: FirmwareOwnerSummary {
                phase: "Preparing local evidence".into(),
                simulated_source_bytes: 0,
                os_offered_bytes: 0,
                device_verified_bytes: 0,
                completion_verified: false,
                quarantined: true,
                diagnostic: None,
                retained_raw_chunks: Vec::new(),
                allocated_raw_frontier: 0,
            },
            finished: false,
            calls: 0,
            zero_cancel: false,
            fault_note: None,
            failure_records: 0,
            #[cfg(target_os = "linux")]
            quarantine: None,
            _handoff: handoff,
            authority,
            entry,
            normal_entry,
            #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
            entry_observation_started: None,
            transition,
            epochs: Vec::new(),
            pending_epoch: None,
            enumeration,
            #[cfg(test)]
            mock_registry: None,
        };
        #[cfg(test)]
        if let Some(path) = mock_path {
            owner.mock_registry = Some(path);
        }
        owner.submit(
            Wait::Begin,
            FirmwareLedgerAction::Begin {
                job: owner.job.clone(),
                origin: owner.plan.origin,
            },
        );
        Ok(owner)
    }
    fn physical_io(
        handoff: &mut FirmwareNormalHandoff,
        receiver: &FirmwareReceiver,
        purpose: crate::firmware_profile::FirmwarePurpose,
        normal: Option<&NormalEntryBinding>,
        #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))] entry_study: Option<
            crate::firmware_entry_study::EntryStudyAuthorization,
        >,
        job: &ReviewedFirmwareUpdate,
        #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))] stop: &Arc<OwnerStop>,
    ) -> Result<(Box<dyn FirmwareIo>, OwnerAuthority), ServiceError> {
        #[cfg(not(all(feature = "firmware-entry-study", target_os = "linux")))]
        let _ = job;
        #[cfg(target_os = "linux")]
        {
            #[cfg(not(all(feature = "firmware-entry-study", target_os = "linux")))]
            crate::firmware_profile::admit_purpose(purpose)?;
            #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
            if entry_study.is_none() {
                crate::firmware_profile::admit_purpose(purpose)?;
            }
            if handoff.guard.is_none() {
                return Err(ServiceError::invalid("Missing retained physical ownership"));
            }
            match purpose {
                crate::firmware_profile::FirmwarePurpose::NormalEntryObservation => {
                    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
                    let permit = match entry_study {
                        Some(study) => {
                            crate::firmware_profile::ExperimentalEntryPermit::from_study(
                                study,
                                job,
                                handoff,
                                stop.clone(),
                            )?
                        }
                        None => crate::firmware_profile::issue_entry()?,
                    };
                    #[cfg(not(all(feature = "firmware-entry-study", target_os = "linux")))]
                    let permit = crate::firmware_profile::issue_entry()?;
                    let binding = normal
                        .ok_or_else(|| ServiceError::invalid("Entry normal witness absent"))?;
                    let io = handoff
                        .factory
                        .take()
                        .ok_or_else(|| ServiceError::invalid("No normal entry factory"))?
                        .open_entry(&permit, binding, receiver.sink.clone())?;
                    Ok((io, OwnerAuthority::Entry(Box::new(permit))))
                }
                crate::firmware_profile::FirmwarePurpose::BootTransfer => {
                    let permit = crate::firmware_profile::issue_physical()?;
                    let boot = crate::firmware_profile::issue_boot_binding()?;
                    let io = crate::firmware_raw::FirmwareRawFactory::open_transfer(
                        &permit,
                        &boot,
                        receiver.sink.clone(),
                    )?;
                    Ok((io, OwnerAuthority::Physical(permit)))
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (handoff, receiver, purpose, normal, job);
            Err(ServiceError::unsupported("Updater Linux binding required"))
        }
    }
    fn epoch_record(&mut self, record: FirmwareLifecycleEpochObservation) {
        self.pending_epoch = Some(record.clone());
        self.submit(
            Wait::Epoch,
            FirmwareLedgerAction::LifecycleEpoch(Box::new(record)),
        );
    }
    fn initial_epoch(&self) -> Result<FirmwareLifecycleEpochObservation, ServiceError> {
        let (raw_identity, negotiation) = self
            .transition
            .as_ref()
            .ok_or_else(|| {
                ServiceError::invalid("Initial role lacks its private transition driver")
            })?
            .initial_identity();
        let observation = FirmwareLifecycleEpochObservation {
            role: FirmwareLifecycleEpochRole::NormalEntry,
            epoch: self.plan.epoch,
            generation: self.plan.generation,
            binding: self.plan.binding.clone(),
            previous_binding_sha256: None,
            previous_epoch: None,
            source_instance: self.receiver.sink.source().into(),
            clock_origin: self.plan.clock_origin.clone(),
            retired_source: None,
            raw_identity,
            negotiation,
            transition: None,
            post_normal_refresh: None,
        };
        observation.retained_json()?;
        Ok(observation)
    }
    fn transition_correlation(&self) -> crate::firmware_transition::TransitionCorrelation {
        crate::firmware_transition::TransitionCorrelation {
            run: self.run.clone(),
            review: self.job.digest().hex(),
            profile: self.plan.profile_sha256.clone(),
            image: self.job.review().summary().source_sha256.hex(),
            sequence: self.sequence,
            epoch: self.plan.epoch,
            generation: self.plan.generation,
            source: self.receiver.sink.source().into(),
            binding: self.plan.binding.digest(),
        }
    }
    fn transition_source(&mut self, role: FirmwareLifecycleEpochRole) -> Result<(), ServiceError> {
        if self.waiting.is_some()
            || self.action.is_some()
            || self.checkpoint.is_some()
            || self.send.is_some()
            || !self.raw.is_empty()
            || !self.frames.is_empty()
            || !self.stream.is_empty()
        {
            return Err(ServiceError::invalid(
                "Source replacement requires fully settled action/framing boundary",
            ));
        }
        let mut retirement = self.io.close();
        // The actual joined source must remain at the already accounted fence;
        // late originals are retained and cannot become new-source replies.
        for _ in 0..64 {
            let Ok(raw) = self.receiver.received.try_recv() else {
                break;
            };
            self.raw.push_back(raw);
        }
        if !retirement.accounts(&self.receiver.sink, self.received)
            || !self.raw.is_empty()
            || !self.receiver.received.is_empty()
        {
            return Err(ServiceError::invalid(
                "Joined updater source changed its original frontier",
            ));
        }
        let previous = self
            .epochs
            .last()
            .ok_or_else(|| ServiceError::invalid("Transition has no durable preceding role"))?
            .clone();
        let correlation = self.transition_correlation();
        let driver = self
            .transition
            .as_mut()
            .ok_or_else(|| ServiceError::invalid("No privately owned transition driver"))?;
        let policy = driver.policy().into();
        let expected = driver.expected(role);
        let candidates = driver.candidates(role)?;
        let continuation = crate::firmware_transition::prepare_transition(
            crate::firmware_transition::TransitionRequest {
                previous: &previous,
                previous_enumeration: self.enumeration,
                role,
                retirement,
                processed: self.received,
                last_ns: self.last_ns,
                policy,
                candidates,
                expected,
                correlation,
            },
        )?;
        let (candidate, mut record) = continuation.consume(
            &self.transition_correlation(),
            self.stop.requested.load(Ordering::SeqCst),
        )?;
        #[cfg(target_os = "linux")]
        if let Some(guard) = self._handoff.guard.as_mut() {
            let endpoints = self
                .transition
                .as_ref()
                .expect("owned driver")
                .endpoints(&candidate);
            guard.retain_aliases(&endpoints)?;
        }
        if self.stop.requested.load(Ordering::SeqCst) {
            return Err(ServiceError::new(
                ErrorKind::Cancelled,
                "Transition revoked before fresh opener",
            ));
        }
        self.enumeration = candidate.enumeration;
        if role == FirmwareLifecycleEpochRole::Bootloader {
            let mut next_plan = self.plan.clone();
            next_plan.epoch = candidate.epoch;
            next_plan.generation = candidate.generation;
            next_plan.binding = candidate.binding.clone();
            let receiver = self
                .receiver
                .sink
                .replacement(&next_plan, candidate.source.clone());
            let io = self
                .transition
                .as_mut()
                .expect("owned driver")
                .open_updater(&candidate, receiver.sink.clone())?;
            if !io.ready_for(&receiver.sink)
                || receiver.sink.fault.load(Ordering::SeqCst)
                || receiver.sink.allocated.load(Ordering::SeqCst) != self.received
                || !receiver.received.is_empty()
            {
                return Err(ServiceError::invalid(
                    "Fresh updater readiness changed source before durable role",
                ));
            }
            self.io = io;
            self.receiver = receiver;
            self.plan = next_plan;
            self.last_ns = record
                .retired_source
                .as_ref()
                .expect("retirement")
                .close_finished_at_ns;
        } else {
            let refresh = crate::firmware_transition::refresh_normal(
                self.transition.as_mut().expect("owned driver").as_mut(),
                &candidate,
                &self.stop.requested,
            )?;
            record.source_instance = refresh.source_instance.clone();
            record.clock_origin = refresh.clock_origin.clone();
            record.post_normal_refresh = Some(refresh);
        }
        self.epoch_record(record);
        Ok(())
    }
    fn derive_next(&self) -> Result<FirmwareLifecycleActionDescription, ServiceError> {
        if let Some(entry) = &self.entry {
            #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
            if let OwnerAuthority::Entry(permit) = &self.authority {
                permit.study().revalidate()?;
            }
            if self
                .normal_entry
                .as_ref()
                .is_none_or(|b| !b.matches(&self._handoff, &self.plan))
                || !self.io.ready_for(&self.receiver.sink)
            {
                return Err(ServiceError::invalid("Entry owned normal binding changed"));
            }
            return crate::firmware_profile::derive_entry(entry, self.ordinal);
        }
        match &self.authority {
            OwnerAuthority::Physical(permit) => crate::firmware_profile::derive_physical(permit),
            OwnerAuthority::Entry(_permit) => Err(ServiceError::invalid(
                "Entry authority lacks its immutable study",
            )),
            #[cfg(test)]
            OwnerAuthority::Mock(authority) => {
                authority.check_rule(&self.plan.profile_sha256)?;
                let mut action = derive_action(&self.job, &self.plan, self.ordinal)?;
                if self.transition.is_some() && self.ordinal == 0 {
                    action.wire = vec![0xf0, 0x7d, 0x45, 0x01, 0xf7];
                    action.wire_sha256 = JobSha256::of(&action.wire).hex();
                }
                Ok(action)
            }
        }
    }
    fn submit(&mut self, wait: Wait, action: FirmwareLedgerAction) {
        if self.finished {
            return;
        }
        let Some(intent) = self.intent.checked_add(1) else {
            self.fail("Updater intent exhausted");
            return;
        };
        self.intent = intent;
        let r = FirmwareLedgerRequest {
            intent,
            expected_sequence: self.sequence,
            review_sha256: self.job.digest().hex(),
            action,
        };
        match self.ledger.as_ref().expect("owned ledger").try_submit(r) {
            Ok(()) => {
                self.waiting = Some(wait);
                self.deadline = Instant::now() + Duration::from_secs(5);
            }
            Err(e) => self.fail(&e.message),
        }
    }
    pub(crate) fn summary(&self) -> &FirmwareOwnerSummary {
        &self.summary
    }
    #[cfg(test)]
    pub(crate) fn observe_raw_for_test(&self, raw: &[u8]) {
        self.receiver.sink.receive(raw);
    }
    #[cfg(test)]
    pub(crate) fn fail_clock_for_test(&self) {
        self.receiver
            .sink
            .clock_failed
            .store(true, Ordering::SeqCst);
    }
    #[cfg(test)]
    pub(crate) fn stop_between_partial_for_test(&self) -> bool {
        if self.offset > 0 && self.send.is_none() {
            self.stop.requested.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }
    #[cfg(test)]
    pub(crate) fn request_user_stop_for_test(&self) {
        self.stop.requested.store(true, Ordering::SeqCst);
    }
    pub(crate) fn request_host_stop(&mut self) {
        self.fail("Service shutdown retains unresolved ownership history");
    }
    pub(crate) fn stopped(&self) -> bool {
        self.finished
    }
    fn fail(&mut self, why: &str) {
        if self.finished {
            return;
        }
        if self.fault_note.is_some() {
            return;
        }
        self.summary.phase = "Recording failure evidence; ownership retained".into();
        self.summary.diagnostic = Some(why.to_owned());
        self.fault_note = Some(bounded_note(why));
        self.stop.requested.store(true, Ordering::SeqCst);
        // Off GUI/callback threads. Stop the producer before taking its final raw frontier.
        self.io.close();
    }
    fn finish_failure(&mut self) {
        if let Some(w) = &self.ledger {
            w.request_stop();
        }
        #[cfg(target_os = "linux")]
        if let Some(q) = &mut self.quarantine {
            let mut pending = self.action_raw.clone();
            pending.extend(self.raw.iter().cloned());
            if let Err(error) = q.retain_failure(
                &pending,
                self.send.as_ref(),
                self.receiver.sink.allocated.load(Ordering::SeqCst),
                self.fault_note
                    .as_deref()
                    .unwrap_or("Unknown ownership failure"),
            ) {
                self.summary.diagnostic = Some(format!(
                    "{}; failure evidence publication also failed: {}; prior quarantine remains unresolved",
                    self.fault_note
                        .as_deref()
                        .unwrap_or("Unknown ownership failure"),
                    error.message
                ));
            }
        }
        self.summary.phase = "Quarantined; host evidence incomplete".into();
        self.finished = true;
    }
    fn flush_failure(&mut self) {
        for _ in 0..64 {
            if self.raw.len() >= 128 {
                break;
            }
            let Ok(r) = self.receiver.received.try_recv() else {
                break;
            };
            self.raw.push_back(r);
        }
        let ledger = self.ledger.as_ref().expect("owned ledger");
        if self.waiting.is_some() {
            if let Some(e) = ledger.try_next() {
                self.reply(e);
                return;
            }
            if ledger.is_stopped() || Instant::now() > self.deadline {
                self.finish_failure();
            }
            return;
        }
        if ledger.is_stopped() {
            self.finish_failure();
            return;
        }
        let send = if self.calls != 0 && self.send.is_none() {
            self.make_send(
                FirmwareOsExtent::Known(self.offset as u64),
                FirmwareDrainObservation::NotAttempted,
            )
        } else {
            None
        };
        if let Some(s) = &send {
            self.send = Some(s.clone());
        }
        let mut raw = self.action_raw.clone();
        raw.extend(self.raw.iter().cloned());
        self.failure_records = raw.len() as u64 + u64::from(send.is_some()) + 1;
        self.submit(
            Wait::Failure,
            FirmwareLedgerAction::LifecycleFailure {
                raw,
                send,
                note: self.fault_note.clone().expect("failure reason"),
            },
        );
    }
    pub(crate) fn tick(&mut self) {
        if self.finished {
            return;
        }
        if self.fault_note.is_some() {
            self.flush_failure();
            return;
        }
        if self.stop.requested.load(Ordering::SeqCst) {
            if self.calls == 0 && self.waiting.is_none() {
                self.checkpoint.take();
                self.zero_cancel = true;
                self.submit(
                    Wait::Close,
                    FirmwareLedgerAction::LifecycleClosed(
                        FirmwareLifecycleHostClosure::ClosedBeforeAnyOffer,
                    ),
                );
                return;
            }
            if self.calls != 0 && self.waiting != Some(Wait::Send) {
                self.fail("Owner stopped after an offer; no retry/reset/rollback issued");
                return;
            }
        }
        if let Err(e) = self.io.health() {
            self.fail(&e.message);
            return;
        }
        if self.receiver.sink.fault.load(Ordering::SeqCst) {
            self.fail("Firmware reader lost evidence");
            return;
        }
        for _ in 0..64 {
            if self.raw.len() >= 64 {
                self.fail("Firmware retained raw backlog exhausted");
                return;
            }
            let Ok(r) = self.receiver.received.try_recv() else {
                break;
            };
            // Retain before any source/order/framing validation can reject it.
            self.raw.push_back(r.clone());
            if r.sequence != self.received.saturating_add(1)
                || self.received == u64::MAX
                || r.at_ns < self.last_ns
                || r.epoch != self.plan.epoch
                || r.generation != self.plan.generation
                || r.binding_sha256 != self.plan.binding.digest()
                || r.clock_origin != self.plan.clock_origin
            {
                self.fail("Firmware receive source/order/backlog differs");
                return;
            }
            self.received = r.sequence;
            self.last_ns = r.at_ns;
            if self.entry.is_some() {
                continue;
            }
            for &b in &r.raw {
                if b == 0xf0 {
                    if !self.stream.is_empty() {
                        self.fail("Nested firmware framing");
                        return;
                    }
                    self.frame_start = r.sequence;
                    self.stream.push(b);
                } else if self.stream.is_empty() || (b > 127 && b != 0xf7) {
                    self.fail("Unsupported or unframed firmware byte");
                    return;
                } else {
                    self.stream.push(b);
                    if self.stream.len() > 518 {
                        self.fail("Oversize firmware frame");
                        return;
                    }
                    if b == 0xf7 {
                        if self.frames.len() >= 64 {
                            self.fail("Firmware frame backlog exhausted");
                            return;
                        }
                        self.frames.push_back((
                            self.frame_start,
                            r.sequence,
                            std::mem::take(&mut self.stream),
                        ));
                    }
                }
            }
        }
        if self.receiver.sink.allocated.load(Ordering::SeqCst) != self.received {
            self.fail("Allocated raw observation unpublished or concurrent at fence");
            return;
        }
        if self.waiting.is_some() {
            if Instant::now() > self.deadline {
                self.fail("Local critical acknowledgement exceeded host bound");
                return;
            }
            if let Some(e) = self.ledger.as_ref().expect("ledger").try_next() {
                self.reply(e);
            }
            return;
        }
        if self.checkpoint.is_some() || self.begun.is_some() {
            self.offer();
            return;
        }
        if self.send.is_some() {
            if Instant::now() > self.deadline {
                self.fail("No accepted action decision before host bound");
                return;
            }
            if let Some(raw) = self.raw.pop_front() {
                if self.action_raw.len() >= 64 {
                    self.raw.push_front(raw);
                    self.fail("Unresolved action raw evidence exceeds bound");
                    return;
                }
                self.action_raw.push(raw.clone());
                self.submit(Wait::Raw, FirmwareLedgerAction::LifecycleRaw(raw));
                return;
            }
            if let Some(entry) = &self.entry {
                let Some(action) = &self.action else {
                    self.fail("Entry send lacks action");
                    return;
                };
                self.submit(
                    Wait::EntrySettled,
                    FirmwareLedgerAction::LifecycleEntryHostSettled(FirmwareEntryHostSettlement {
                        action: action.ordinal,
                        wire_sha256: action.wire_sha256.clone(),
                        study_sha256: entry.study_sha256.clone(),
                    }),
                );
                return;
            }
            if !self.stream.is_empty() {
                self.fail("Unfinished frame at decision boundary");
                return;
            }
            let Some(a) = self.action.as_ref() else {
                self.fail("Missing private action");
                return;
            };
            let Some((first, last, frame)) = self.frames.pop_front() else {
                return;
            };
            if !self.frames.is_empty()
                || first < self.first_raw
                || frame != synthetic_reply(a.ordinal)
            {
                self.fail("No exact private absolute-action mock correlation; opaque status cannot advance");
                return;
            }
            if self.plan.origin != FirmwareObservationOrigin::MockSimulation {
                self.fail("Physical decision issuer unavailable");
                return;
            }
            let d = FirmwareLifecycleDecision {
                action: a.ordinal,
                rule_sha256: a.rule_sha256.clone(),
                first_raw: first,
                last_raw: last,
                observed_frontier: self.received,
                mock_matched: true,
            };
            self.submit(Wait::Decision, FirmwareLedgerAction::LifecycleDecision(d));
            return;
        }
        if self.entry.is_none()
            && self.transition.is_some()
            && self.ordinal == 1
            && self.epochs.len() == 1
        {
            if let Err(e) = self.transition_source(FirmwareLifecycleEpochRole::Bootloader) {
                self.fail(&e.message);
            }
            return;
        }
        if self.entry.is_none()
            && self.transition.is_some()
            && self.ordinal == self.job.budget().blocks + 17
            && self.epochs.len() == 2
        {
            if let Err(e) = self.transition_source(FirmwareLifecycleEpochRole::PostUpdateNormal) {
                self.fail(&e.message);
            }
            return;
        }
        if self.entry.is_some() && self.ordinal == 2 {
            #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
            if let OwnerAuthority::Entry(permit) = &self.authority {
                let started = self
                    .entry_observation_started
                    .get_or_insert_with(Instant::now);
                if let Some(raw) = self.raw.pop_front() {
                    self.submit(Wait::Raw, FirmwareLedgerAction::LifecycleRaw(raw));
                    return;
                }
                if started.elapsed()
                    < Duration::from_secs(permit.study().review.inputs.observation_seconds)
                {
                    return;
                }
            }
            self.submit(
                Wait::Close,
                FirmwareLedgerAction::LifecycleClosed(
                    FirmwareLifecycleHostClosure::HostStoppedUnresolved,
                ),
            );
            return;
        }
        if self.entry.is_none() && self.ordinal == self.job.budget().blocks + 17 {
            self.submit(
                Wait::Close,
                FirmwareLedgerAction::LifecycleClosed(FirmwareLifecycleHostClosure::MockFinished),
            );
            return;
        }
        if self.entry.is_some()
            && let Some(raw) = self.raw.pop_front()
        {
            self.submit(Wait::Raw, FirmwareLedgerAction::LifecycleRaw(raw));
            return;
        }
        if !self.raw.is_empty() || !self.frames.is_empty() || !self.stream.is_empty() {
            self.fail("Stale raw data before next action");
            return;
        }
        match self.derive_next() {
            Ok(a) => {
                self.first_raw = self.received + 1;
                self.action = Some(a.clone());
                self.submit(
                    Wait::Checkpoint,
                    FirmwareLedgerAction::LifecycleCheckpoint(a),
                );
            }
            Err(e) => self.fail(&e.message),
        }
    }
    fn reply(&mut self, e: FirmwareLedgerEvent) {
        if e.run != self.run || e.intent != self.intent {
            self.fail("Wrong actual worker run/intent");
            return;
        }
        let Some(wait) = self.waiting.take() else {
            self.fail("Duplicate critical reply");
            return;
        };
        let reply = match e.result {
            Ok(r) => r,
            Err(e) => {
                self.fail(&e.message);
                return;
            }
        };
        if self.fault_note.is_some() && wait == Wait::Close {
            self.finish_failure();
            return;
        }
        match (wait, reply) {
            (Wait::Failure, FirmwareLedgerReply::Recorded { sequence })
                if sequence == self.sequence + self.failure_records =>
            {
                self.sequence = sequence;
                self.finish_failure();
            }

            (
                Wait::Begin,
                FirmwareLedgerReply::Prepared {
                    attempt,
                    checkpoint,
                },
            ) if checkpoint.sequence() == 0 && checkpoint.attempt() == &attempt => {
                self.attempt = Some(attempt.clone());
                #[cfg(target_os = "linux")]
                {
                    #[cfg(test)]
                    let registry = self
                        .mock_registry
                        .as_ref()
                        .map(|p| crate::firmware_quarantine::QuarantineRegistry::mock(p))
                        .unwrap_or_else(crate::firmware_quarantine::QuarantineRegistry::system);
                    #[cfg(not(test))]
                    let registry = crate::firmware_quarantine::QuarantineRegistry::system();
                    match registry
                        .and_then(|r| {
                            r.reserve(
                                &attempt,
                                &self.plan,
                                &self.job.review().summary().source_sha256.hex(),
                                &self.journal_root,
                            )
                        })
                        .and_then(|mut q| {
                            q.arm()?;
                            Ok(q)
                        }) {
                        Ok(q) => self.quarantine = Some(q),
                        Err(e) => {
                            self.fail(&e.message);
                            return;
                        }
                    }
                }
                self.submit(
                    Wait::Plan,
                    match &self.entry {
                        Some(entry) => FirmwareLedgerAction::LifecycleEntryPrepared(entry.clone()),
                        None => FirmwareLedgerAction::LifecyclePrepared(self.plan.clone()),
                    },
                );
            }
            (Wait::Checkpoint, FirmwareLedgerReply::ActionCheckpoint(c))
                if c.sequence() == self.sequence + 1
                    && Some(c.attempt()) == self.attempt.as_ref()
                    && c.run_id() == self.run
                    && c.intent() == self.intent
                    && Some(c.action()) == self.action.as_ref()
                    && c.prefix_sha256().len() == 64 =>
            {
                self.sequence = c.sequence();
                self.checkpoint = Some(c);
                self.offset = 0;
                self.tries = 0;
            }
            (Wait::Close, FirmwareLedgerReply::Observation(r))
                if r.lifecycle.as_ref().is_some_and(|p| {
                    p.closure()
                        == Some(if self.zero_cancel {
                            FirmwareLifecycleHostClosure::ClosedBeforeAnyOffer
                        } else if self.entry.is_some() {
                            FirmwareLifecycleHostClosure::HostStoppedUnresolved
                        } else {
                            FirmwareLifecycleHostClosure::MockFinished
                        })
                }) =>
            {
                #[cfg(target_os = "linux")]
                if let Some(q) = &mut self.quarantine {
                    let result = if self.zero_cancel {
                        q.zero_offer()
                    } else if self.entry.is_some() {
                        Ok(())
                    } else {
                        q.mock_finish()
                    };
                    if let Err(e) = result {
                        self.fail(&e.message);
                        return;
                    }
                }
                self.summary.phase = if self.zero_cancel {
                    "Cancelled before any driver offer"
                } else if self.entry.is_some() {
                    "Entry host offers recorded; device mode unresolved"
                } else {
                    "Mock finished; physical update remains unverified"
                }
                .into();
                self.summary.quarantined = self.entry.is_some() && !self.zero_cancel;
                self.finished = true;
            }
            (wait, FirmwareLedgerReply::Recorded { sequence }) if sequence == self.sequence + 1 => {
                self.sequence = sequence;
                match wait {
                    Wait::Plan => {
                        if self.transition.is_some() {
                            match self.initial_epoch() {
                                Ok(record) => self.epoch_record(record),
                                Err(error) => self.fail(&error.message),
                            }
                        }
                    }
                    Wait::Epoch => {
                        let Some(record) = self.pending_epoch.take() else {
                            self.fail("Epoch reply has no original role");
                            return;
                        };
                        self.epochs.push(record);
                    }
                    Wait::Send => {
                        let complete = self.send.as_ref().is_some_and(|s| {
                            s.extent == FirmwareOsExtent::Known(s.attempted)
                                && s.drain == FirmwareDrainObservation::Returned
                                && s.finished_sequence.is_some()
                                && s.finished_at_ns.is_some()
                        });
                        if self.fault_note.is_none()
                            && (!complete || self.stop.requested.load(Ordering::SeqCst))
                        {
                            self.fail(
                                "Offered extent/drain unresolved; exact send observation retained",
                            );
                        }
                    }
                    Wait::Raw => {}
                    Wait::Decision | Wait::EntrySettled => {
                        if let Some(a) = self.action.take() {
                            self.summary.simulated_source_bytes +=
                                a.block.map_or(0, |b| u64::from(b.content_bytes));
                        }
                        self.ordinal += 1;
                        self.send = None;
                        self.checkpoint = None;
                        self.action_raw.clear();
                    }
                    _ => self.fail("Recorded reply kind differs"),
                }
            }
            _ => self.fail("Critical journal reply action/sequence differs"),
        }
    }
    fn offer(&mut self) {
        let Some(a) = self.action.clone() else {
            self.fail("Dispatch lacks privately derived action");
            return;
        };
        if self.derive_next().as_ref() != Ok(&a) {
            self.fail("Private action changed before offer");
            return;
        }
        if self.tries >= 32 {
            self.record_send(
                FirmwareOsExtent::Known(self.offset as u64),
                FirmwareDrainObservation::Unknown,
            );
            return;
        }
        self.tries += 1;
        if self.stop.requested.load(Ordering::SeqCst)
            || self.receiver.sink.fault.load(Ordering::SeqCst)
        {
            self.fail("Stop before next nonblocking offer");
            return;
        }
        let step;
        if let Some(c) = self.checkpoint.take() {
            let ticket = FirmwareDispatchTicket {
                checkpoint: c,
                action: a.clone(),
                generation: self.plan.generation,
                review: self.job.digest().hex(),
            };
            if ticket.action != a
                || ticket.generation != self.plan.generation
                || ticket.review != self.job.digest().hex()
            {
                self.fail("Dispatch ticket owner scope differs");
                return;
            }
            let c = ticket.checkpoint;
            #[cfg(target_os = "linux")]
            if let Some(entry) = &self.entry {
                let Some(q) = self.quarantine.as_mut() else {
                    self.fail("Entry registry absent");
                    return;
                };
                if let Err(error) = q.entry_checkpoint(&c, &entry.study_sha256) {
                    self.fail(&error.message);
                    return;
                }
            }
            if self.received != 0 && !self.raw.is_empty() {
                self.fail("Raw data arrived before offer");
                return;
            }
            // Produce an owned single-call observation before handling errors;
            // no borrowed journal fence can survive into failure retirement.
            let offered = (|| -> Result<WriteStep, ServiceError> {
                let w = self.ledger.as_ref().expect("ledger");
                let _fence = w.try_local_offer_fence(c)?;
                let stop = self.stop.clone();
                let _gate = stop.gate.lock().unwrap_or_else(|e| e.into_inner());
                if stop.requested.load(Ordering::SeqCst)
                    || w.is_stopped()
                    || self.receiver.sink.fault.load(Ordering::SeqCst)
                {
                    return Err(ServiceError::new(
                        ErrorKind::Cancelled,
                        "Stop revoked first syscall",
                    ));
                }
                self.begun = Some(self.receiver.sink.now()?);
                #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
                if let OwnerAuthority::Entry(permit) = &self.authority {
                    permit.study().arm_native_frontier(self.received);
                }
                #[cfg(target_os = "linux")]
                if let Some(q) = &mut self.quarantine {
                    q.offered();
                }
                self.calls += 1;
                Ok(self.io.write_once(&a.wire))
            })();
            step = match offered {
                Ok(step) => step,
                Err(error) => {
                    self.fail(&error.message);
                    return;
                }
            };
        } else if self.offset < a.wire.len() {
            self.calls += 1;
            step = self.io.write_once(&a.wire[self.offset..]);
        } else {
            match self.io.drain_once() {
                DrainStep::Returned => self.record_send(
                    FirmwareOsExtent::Known(self.offset as u64),
                    FirmwareDrainObservation::Returned,
                ),
                DrainStep::WouldBlock if self.entry.is_some() => self.record_send(
                    FirmwareOsExtent::Known(self.offset as u64),
                    FirmwareDrainObservation::Unknown,
                ),
                DrainStep::WouldBlock => {}
                DrainStep::Failed(e) => {
                    self.summary.diagnostic = Some(e.message);
                    self.record_send(
                        FirmwareOsExtent::Known(self.offset as u64),
                        FirmwareDrainObservation::Failed,
                    );
                }
            }
            return;
        }
        if self.entry.is_some() {
            match step {
                WriteStep::Returned(n) if n == a.wire.len() => {
                    self.offset = n;
                    self.summary.os_offered_bytes += n as u64;
                }
                WriteStep::Returned(n) if n < a.wire.len() => {
                    self.offset = n;
                    self.summary.os_offered_bytes += n as u64;
                    self.record_send(
                        FirmwareOsExtent::Known(n as u64),
                        FirmwareDrainObservation::NotAttempted,
                    );
                }
                WriteStep::WouldBlock | WriteStep::Interrupted => {
                    self.record_send(
                        FirmwareOsExtent::Known(0),
                        FirmwareDrainObservation::NotAttempted,
                    );
                }
                WriteStep::Unknown(e) => {
                    self.summary.diagnostic = Some(e.message);
                    self.record_send(
                        FirmwareOsExtent::Unknown,
                        FirmwareDrainObservation::NotAttempted,
                    );
                }
                WriteStep::Returned(_) => self.record_send(
                    FirmwareOsExtent::Unknown,
                    FirmwareDrainObservation::NotAttempted,
                ),
            }
            return;
        }
        match step {
            WriteStep::Returned(n) if n > 0 && n <= a.wire.len() - self.offset => {
                self.offset += n;
                self.summary.os_offered_bytes += n as u64;
            }
            WriteStep::WouldBlock | WriteStep::Interrupted => {}
            WriteStep::Unknown(e) => {
                self.summary.diagnostic = Some(e.message);
                self.record_send(
                    FirmwareOsExtent::Unknown,
                    FirmwareDrainObservation::NotAttempted,
                );
            }
            WriteStep::Returned(0) => self.record_send(
                FirmwareOsExtent::Known(self.offset as u64),
                FirmwareDrainObservation::NotAttempted,
            ),
            WriteStep::Returned(_) => self.record_send(
                FirmwareOsExtent::Unknown,
                FirmwareDrainObservation::NotAttempted,
            ),
        }
        if self.stop.requested.load(Ordering::SeqCst) && self.send.is_none() {
            self.record_send(
                FirmwareOsExtent::Known(self.offset as u64),
                FirmwareDrainObservation::NotAttempted,
            );
        }
    }
    fn make_send(
        &self,
        extent: FirmwareOsExtent,
        drain: FirmwareDrainObservation,
    ) -> Option<FirmwareLifecycleSendObservation> {
        let a = self.action.as_ref()?;
        let (started_sequence, started_at_ns) = self.begun?;
        let finish = self.receiver.sink.now().ok();
        Some(FirmwareLifecycleSendObservation {
            action: a.ordinal,
            wire_sha256: a.wire_sha256.clone(),
            attempted: a.wire.len() as u64,
            extent,
            drain,
            started_sequence,
            finished_sequence: finish.map(|v| v.0),
            started_at_ns,
            finished_at_ns: finish.map(|v| v.1),
        })
    }
    fn record_send(&mut self, extent: FirmwareOsExtent, drain: FirmwareDrainObservation) {
        let Some(s) = self.make_send(extent, drain) else {
            self.fail("Missing actual first-offer clock; send evidence remains unknown");
            return;
        };
        self.begun = None;
        self.send = Some(s.clone());
        self.submit(Wait::Send, FirmwareLedgerAction::LifecycleSend(s));
    }
    pub(crate) fn retire(mut self) -> Result<(FirmwareOwnerSummary, LibraryStore), ServiceError> {
        // Only the service worker calls this. Shutdown still records the known
        // extent and original raw evidence before requesting the actual ledger join.
        if !self.finished {
            if self.fault_note.is_none() {
                self.fail("Owner retired before an accepted host closure");
            }
            while !self.finished {
                self.tick();
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let mut close = self.io.close();
        let entry_frontier_changed =
            self.entry.is_some() && !close.accounts(&self.receiver.sink, self.received);
        if entry_frontier_changed
            || close.observation().reader_join != FirmwareNormalReaderJoinObservation::Returned
            || close.observation().fault_or_missing
        {
            self.summary.diagnostic =
                Some("Actual updater retirement join/frontier remains unresolved".into());
        }
        self.summary.allocated_raw_frontier = self.receiver.sink.allocated.load(Ordering::SeqCst);
        for _ in 0..64 {
            if self.raw.len() >= 128 {
                break;
            }
            let Ok(r) = self.receiver.received.try_recv() else {
                break;
            };
            self.raw.push_back(r);
        }
        self.summary.retained_raw_chunks = std::mem::take(&mut self.action_raw);
        self.summary.retained_raw_chunks.extend(self.raw.drain(..));
        if let Some(first) = self.receiver.sink.first_failure()
            && !self.summary.retained_raw_chunks.iter().any(|r| r == &first)
        {
            self.summary.retained_raw_chunks.push(first);
        }
        let store = self.ledger.take().expect("one journal owner").join()?;
        Ok((self.summary.clone(), store))
    }
}
// Absolute synthetic ACK grammar is deliberately different from device op04/7f.
pub(crate) fn synthetic_reply(ordinal: u64) -> Vec<u8> {
    let mut p = vec![0xf0, 0x7d];
    for b in ordinal.to_be_bytes() {
        p.extend([b >> 4, b & 15]);
    }
    p.push(0xf7);
    p
}
#[cfg(test)]
pub(crate) fn derive_action(
    job: &ReviewedFirmwareUpdate,
    plan: &FirmwareLifecyclePlanDto,
    ordinal: u64,
) -> Result<FirmwareLifecycleActionDescription, ServiceError> {
    let mut at = 0;
    let mut block_at = 0;
    fn pick(
        at: &mut u64,
        ordinal: u64,
        plan: &FirmwareLifecyclePlanDto,
        kind: FirmwareLifecycleKind,
        wire: Vec<u8>,
        block: Option<FirmwareBlockDescription>,
    ) -> Option<FirmwareLifecycleActionDescription> {
        let matched = *at == ordinal;
        *at += 1;
        matched.then(|| FirmwareLifecycleActionDescription {
            ordinal,
            kind,
            wire_sha256: JobSha256::of(&wire).hex(),
            wire,
            block,
            epoch: plan.epoch,
            binding_sha256: plan.binding.digest(),
            generation: plan.generation,
            rule_sha256: plan.profile_sha256.clone(),
        })
    }
    if let Some(a) = pick(
        &mut at,
        ordinal,
        plan,
        FirmwareLifecycleKind::IdentityQuery,
        FirmwareFixedPacket::IdentityQuery.raw().to_vec(),
        None,
    ) {
        return Ok(a);
    }
    let announce = job
        .review()
        .summary()
        .image_provenance
        .announcement_candidate()
        .map_err(|e| ServiceError::invalid(e.to_string()))?;
    if let Some(a) = pick(
        &mut at,
        ordinal,
        plan,
        FirmwareLifecycleKind::ImageAnnouncement,
        announce.raw().to_vec(),
        None,
    ) {
        return Ok(a);
    }
    for selector in gp200_protocol::FirmwareSectionSelector::IMAGE_TRANSFER_ORDER {
        let slot = selector.image_table_slot().expect("image selector");
        let s = &job.review().summary().sections[slot];
        if let Some(a) = pick(
            &mut at,
            ordinal,
            plan,
            FirmwareLifecycleKind::SectionRequest,
            selector.request_candidate().to_vec(),
            None,
        ) {
            return Ok(a);
        }
        let count = (s.bytes as u64).div_ceil(u64::from(job.chunk()));
        if let Some(a) = pick(
            &mut at,
            ordinal,
            plan,
            FirmwareLifecycleKind::Descriptor,
            FirmwareDescriptor::candidate(s.bytes as u32, count as u32)
                .raw()
                .to_vec(),
            None,
        ) {
            return Ok(a);
        }
        // Avoid allocating all blocks and avoid replaying previous block bytes.
        if ordinal >= at && ordinal < at + count {
            let b = job.block(block_at + ordinal - at)?;
            let wire = job.block_wire(&b)?;
            return Ok(FirmwareLifecycleActionDescription {
                ordinal,
                kind: FirmwareLifecycleKind::ImageBlock,
                wire_sha256: JobSha256::of(&wire).hex(),
                wire,
                block: Some(b),
                epoch: plan.epoch,
                binding_sha256: plan.binding.digest(),
                generation: plan.generation,
                rule_sha256: plan.profile_sha256.clone(),
            });
        }
        at += count;
        block_at += count;
        if let Some(a) = pick(
            &mut at,
            ordinal,
            plan,
            FirmwareLifecycleKind::SectionTerminator,
            FirmwareFixedPacket::SectionTerminator.raw().to_vec(),
            None,
        ) {
            return Ok(a);
        }
    }
    if let Some(a) = pick(
        &mut at,
        ordinal,
        plan,
        FirmwareLifecycleKind::SectionRequest,
        gp200_protocol::FirmwareSectionSelector::Terminal
            .request_candidate()
            .to_vec(),
        None,
    ) {
        return Ok(a);
    }
    if let Some(a) = pick(
        &mut at,
        ordinal,
        plan,
        FirmwareLifecycleKind::TerminalQuery,
        FirmwareFixedPacket::TerminalQuery.raw().to_vec(),
        None,
    ) {
        return Ok(a);
    }
    if let Some(a) = pick(
        &mut at,
        ordinal,
        plan,
        FirmwareLifecycleKind::TerminalControl,
        FirmwareFixedPacket::TerminalControl.raw().to_vec(),
        None,
    ) {
        return Ok(a);
    }
    Err(ServiceError::invalid(
        "Private lifecycle ordinal outside exact image schedule",
    ))
}

fn bounded_note(why: &str) -> String {
    let mut end = why.len().min(512);
    while !why.is_char_boundary(end) {
        end -= 1;
    }
    why[..end].to_owned()
}
