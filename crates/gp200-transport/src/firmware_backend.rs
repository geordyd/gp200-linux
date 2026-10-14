//! Updater I/O uses distinct observations, never normal aggregate send().
use crossbeam_channel::{Receiver, Sender, bounded};
use gp200_domain::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Instant;

pub(crate) enum WriteStep {
    Returned(usize),
    WouldBlock,
    Interrupted,
    Unknown(ServiceError),
}
pub(crate) enum DrainStep {
    Returned,
    WouldBlock,
    Failed(ServiceError),
}
pub(crate) trait FirmwareIo: Send {
    #[cfg(test)]
    fn attach(&mut self, _sink: FirmwareReceiveSink) {}
    fn write_once(&mut self, bytes: &[u8]) -> WriteStep;
    fn drain_once(&mut self) -> DrainStep;
    fn health(&mut self) -> Result<(), ServiceError>;
    fn ready_for(&self, _sink: &FirmwareReceiveSink) -> bool {
        false
    }
    fn close(&mut self) -> FirmwareReaderRetirement;
}
pub(crate) struct FirmwareReceiver {
    pub(crate) received: Receiver<FirmwareLifecycleRawObservation>,
    pub(crate) sink: FirmwareReceiveSink,
}
#[derive(Clone)]
pub(crate) struct FirmwareReceiveSink {
    tx: Sender<FirmwareLifecycleRawObservation>,
    pub(crate) fault: Arc<AtomicBool>,
    pub(crate) allocated: Arc<AtomicU64>,
    #[cfg(test)]
    pub(crate) clock_failed: Arc<AtomicBool>,
    origin: Instant,
    clock: String,
    binding: String,
    epoch: SessionEpoch,
    generation: u64,
    source: String,
    first_failure: Arc<std::sync::Mutex<Option<FirmwareLifecycleRawObservation>>>,
}
impl FirmwareReceiver {
    pub(crate) fn new(plan: &FirmwareLifecyclePlanDto) -> Self {
        let (tx, received) = bounded(64);
        Self {
            received,
            sink: FirmwareReceiveSink {
                tx,
                fault: Arc::new(AtomicBool::new(false)),
                allocated: Arc::new(AtomicU64::new(0)),
                #[cfg(test)]
                clock_failed: Arc::new(AtomicBool::new(false)),
                origin: Instant::now(),
                clock: plan.clock_origin.clone(),
                binding: plan.binding.digest(),
                epoch: plan.epoch,
                generation: plan.generation,
                source: format!("updater-source-{}-{}", plan.epoch.0, plan.generation),
                first_failure: Arc::new(std::sync::Mutex::new(None)),
            },
        }
    }
}
impl FirmwareReceiveSink {
    pub(crate) fn receive(&self, raw: &[u8]) {
        // Allocation and original time precede the copy, including failed publication.
        let sequence = self
            .allocated
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_add(1));
        let at = u64::try_from(self.origin.elapsed().as_nanos());
        match (sequence, at) {
            (Ok(previous), Ok(at_ns)) if !raw.is_empty() && raw.len() <= 1024 => {
                let r = FirmwareLifecycleRawObservation {
                    sequence: previous + 1,
                    at_ns,
                    epoch: self.epoch,
                    generation: self.generation,
                    binding_sha256: self.binding.clone(),
                    clock_origin: self.clock.clone(),
                    raw: raw.to_vec(),
                };
                if let Err(e) = self.tx.try_send(r) {
                    if let Ok(mut first) = self.first_failure.try_lock()
                        && first.is_none()
                    {
                        *first = Some(e.into_inner());
                    }
                    self.fault.store(true, Ordering::SeqCst);
                }
            }
            _ => self.fault.store(true, Ordering::SeqCst),
        }
    }
    /// A reader fault has an original allocated position even when no bytes
    /// exist or its bounded publication cannot succeed. The sticky frontier gap
    /// forbids attributing a later callback to that missing observation.
    pub(crate) fn reader_fault(&self) {
        self.receive(&[]);
        self.fault.store(true, Ordering::SeqCst);
    }
    pub(crate) fn now(&self) -> Result<(u64, u64), ServiceError> {
        #[cfg(test)]
        if self.clock_failed.load(Ordering::SeqCst) {
            return Err(ServiceError::invalid("Injected unavailable owner clock"));
        }
        let ns = u64::try_from(self.origin.elapsed().as_nanos())
            .map_err(|_| ServiceError::invalid("Firmware monotonic clock exhausted"))?;
        Ok((self.allocated.load(Ordering::SeqCst), ns))
    }
}
/// One consuming close observation. Repeated close returns the retained
/// description but cannot yield another fresh continuation.
pub(crate) struct FirmwareReaderRetirement {
    observation: FirmwareLifecycleSourceRetirement,
    pub(crate) fault: Option<ServiceError>,
    pub(crate) output_destroyed: bool,
    fresh: bool,
    allocator: Arc<AtomicU64>,
}
impl FirmwareReaderRetirement {
    pub(crate) fn observation(&self) -> &FirmwareLifecycleSourceRetirement {
        &self.observation
    }
    pub(crate) fn is_fresh(&self) -> bool {
        self.fresh && self.fault.is_none() && self.output_destroyed
    }
    pub(crate) fn accounts(&mut self, sink: &FirmwareReceiveSink, processed: u64) -> bool {
        self.observation.processed = processed;
        Arc::ptr_eq(&self.allocator, &sink.allocated)
            && self.observation.source_instance == sink.source
            && self.observation.epoch == sink.epoch
            && self.observation.generation == sink.generation
            && self.observation.clock_origin == sink.clock
            && self.observation.allocated == processed
    }
    pub(crate) fn repeat(&self) -> Self {
        Self {
            observation: self.observation.clone(),
            fault: self.fault.clone(),
            output_destroyed: self.output_destroyed,
            fresh: false,
            allocator: self.allocator.clone(),
        }
    }
}
impl FirmwareReceiveSink {
    pub(crate) fn first_failure(&self) -> Option<FirmwareLifecycleRawObservation> {
        self.first_failure.lock().ok().and_then(|f| f.clone())
    }
    pub(crate) fn same_source(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.allocated, &other.allocated)
            && self.source == other.source
            && self.epoch == other.epoch
            && self.generation == other.generation
            && self.clock == other.clock
            && self.binding == other.binding
    }
    pub(crate) fn matches_boot(&self, boot: &crate::firmware_profile::AcceptedBootBinding) -> bool {
        self.binding == boot.binding.digest()
            && self.epoch == boot.epoch
            && self.generation == boot.generation
            && self.source == boot.source_instance
    }
    pub(crate) fn source(&self) -> &str {
        &self.source
    }
    pub(crate) fn replacement(
        &self,
        plan: &FirmwareLifecyclePlanDto,
        source: String,
    ) -> FirmwareReceiver {
        let (tx, received) = bounded(64);
        let mut sink = self.clone();
        sink.tx = tx;
        sink.epoch = plan.epoch;
        sink.generation = plan.generation;
        sink.binding = plan.binding.digest();
        sink.source = source;
        FirmwareReceiver { received, sink }
    }
    pub(crate) fn retirement(
        &self,
        started: Option<u64>,
        finished: Option<u64>,
        join: FirmwareNormalReaderJoinObservation,
        fault: Option<ServiceError>,
    ) -> FirmwareReaderRetirement {
        let clock_missing = started.is_none() || finished.is_none();
        FirmwareReaderRetirement {
            observation: FirmwareLifecycleSourceRetirement {
                source_instance: self.source.clone(),
                clock_origin: self.clock.clone(),
                epoch: self.epoch,
                generation: self.generation,
                allocated: self.allocated.load(Ordering::SeqCst),
                processed: 0,
                close_started_at_ns: started.unwrap_or(0),
                close_finished_at_ns: finished.unwrap_or(0),
                reader_join: join,
                fault_or_missing: clock_missing
                    || self.fault.load(Ordering::SeqCst)
                    || fault.is_some(),
            },
            fault,
            output_destroyed: false,
            fresh: true,
            allocator: self.allocated.clone(),
        }
    }
    #[cfg(test)]
    pub(crate) fn synthetic_close(&self) -> FirmwareReaderRetirement {
        let start = self.now().ok().map(|(_, n)| n);
        let mut receipt = self.retirement(
            start,
            self.now().ok().map(|(_, n)| n),
            FirmwareNormalReaderJoinObservation::Returned,
            None,
        );
        receipt.output_destroyed = true;
        receipt
    }
}
/// Opaque session ownership bridge. External backend implementations cannot
/// construct a system or mock handoff.
pub struct FirmwareNormalHandoff {
    pub(crate) binding: FirmwareUsbObservation,
    pub(crate) normal_epoch: SessionEpoch,
    pub(crate) normal_close: Option<crate::backend::NormalReaderCloseReceipt>,
    pub(crate) retirement: Option<crate::firmware_transition::NormalRetirementSeal>,
    pub(crate) transition: Option<Box<dyn crate::firmware_transition::TransitionDriver>>,
    #[cfg(target_os = "linux")]
    pub(crate) factory: Option<crate::firmware_raw::FirmwareRawFactory>,
    #[cfg(target_os = "linux")]
    pub(crate) guard: Option<crate::device_session_guard::DeviceSessionGuard>,
    #[cfg(test)]
    pub(crate) mock: Option<Box<dyn FirmwareIo>>,
}

/// Actual normal retirement and retained physical/original alias ownership.
/// Only private owning handoffs can construct this witness.
pub(crate) struct NormalEntryBinding {
    binding: FirmwareUsbObservation,
    normal_epoch: SessionEpoch,
}
impl NormalEntryBinding {
    pub(crate) fn from_handoff(handoff: &FirmwareNormalHandoff) -> Result<Self, ServiceError> {
        if handoff
            .retirement
            .as_ref()
            .is_none_or(|r| !r.matches(&handoff.binding, handoff.normal_epoch))
        {
            return Err(ServiceError::invalid(
                "Entry requires actual unchanged normal retirement",
            ));
        }
        #[cfg(target_os = "linux")]
        if handoff
            .guard
            .as_ref()
            .is_none_or(|g| !g.owns_normal_binding(&handoff.binding))
        {
            return Err(ServiceError::invalid(
                "Entry lacks physical/original endpoint ownership",
            ));
        }
        Ok(Self {
            binding: handoff.binding.clone(),
            normal_epoch: handoff.normal_epoch,
        })
    }
    pub(crate) fn matches(
        &self,
        handoff: &FirmwareNormalHandoff,
        plan: &FirmwareLifecyclePlanDto,
    ) -> bool {
        self.binding == handoff.binding
            && self.binding == plan.binding
            && self.normal_epoch == handoff.normal_epoch
            && self.normal_epoch.0.checked_add(1) == Some(plan.epoch.0)
            && handoff
                .retirement
                .as_ref()
                .is_some_and(|r| r.matches(&self.binding, self.normal_epoch))
            && {
                #[cfg(target_os = "linux")]
                {
                    handoff
                        .guard
                        .as_ref()
                        .is_some_and(|g| g.owns_normal_binding(&self.binding))
                }
                #[cfg(not(target_os = "linux"))]
                {
                    false
                }
            }
    }
    pub(crate) fn endpoint(&self) -> &str {
        &self.binding.endpoint
    }
}
