use crate::{Delivery, SimulatedPedal, simulator_port};
use crossbeam_channel::{Sender, bounded};
use gp200_domain::*;
use gp200_transport::{MidiSession, ReceiveSink, TransportBackend};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

struct Shared {
    pedal: SimulatedPedal,
    active: Option<(u64, Sender<Delivery>)>,
    generation: u64,
}

/// Test/developer control only: it cannot address physical ALSA devices.
#[derive(Clone)]
pub struct SimulatorHandle {
    shared: Arc<Mutex<Shared>>,
    present: Arc<AtomicBool>,
    origin: Instant,
}
impl SimulatorHandle {
    pub fn unplug(&self) {
        self.present.store(false, Ordering::Release);
    }
    pub fn plug_in(&self) {
        self.present.store(true, Ordering::Release);
    }
    pub fn with_pedal<T>(&self, inspect: impl FnOnce(&mut SimulatedPedal) -> T) -> T {
        inspect(&mut self.shared.lock().expect("simulator mutex").pedal)
    }
    pub fn external_parameter_edit(
        &self,
        module: u8,
        parameter: u8,
        value: F32Bits,
    ) -> Result<(), ServiceError> {
        let mut shared = self
            .shared
            .lock()
            .map_err(|_| ServiceError::new(ErrorKind::Io, "Simulator state lock failed"))?;
        let deliveries = shared.pedal.external_parameter_edit(
            module,
            parameter,
            value,
            self.origin.elapsed(),
        )?;
        if let Some((_, sender)) = &shared.active {
            for delivery in deliveries {
                sender.try_send(delivery).map_err(|_| {
                    ServiceError::new(
                        ErrorKind::Busy,
                        "Simulator delivery queue is full or closed",
                    )
                })?;
            }
        }
        Ok(())
    }
}

pub struct SimulatorBackend {
    handle: SimulatorHandle,
}
impl Default for SimulatorBackend {
    fn default() -> Self {
        Self::new()
    }
}
impl SimulatorBackend {
    pub fn new() -> Self {
        Self::with_pedal(SimulatedPedal::captured())
    }
    pub fn with_pedal(pedal: SimulatedPedal) -> Self {
        Self {
            handle: SimulatorHandle {
                shared: Arc::new(Mutex::new(Shared {
                    pedal,
                    active: None,
                    generation: 0,
                })),
                present: Arc::new(AtomicBool::new(true)),
                origin: Instant::now(),
            },
        }
    }
    pub fn handle(&self) -> SimulatorHandle {
        self.handle.clone()
    }
}
impl TransportBackend for SimulatorBackend {
    fn mode(&self) -> BackendMode {
        BackendMode::Simulator
    }
    fn enumerate(&mut self) -> Result<Vec<PortPairCandidate>, ServiceError> {
        Ok(if self.handle.present.load(Ordering::Acquire) {
            vec![simulator_port()]
        } else {
            Vec::new()
        })
    }
    fn connect(
        &mut self,
        selection: &DeviceSelection,
        receive: ReceiveSink,
    ) -> Result<Box<dyn MidiSession>, ServiceError> {
        if selection != &simulator_port().selection {
            return Err(ServiceError::invalid(
                "Simulator opens only explicit simulator ports",
            ));
        }
        if !self.handle.present.load(Ordering::Acquire) {
            return Err(ServiceError::new(
                ErrorKind::Disconnected,
                "Simulator is unplugged",
            ));
        }
        let (sender, cancelled) = delivery_worker(
            receive.clone(),
            self.handle.origin,
            self.handle.present.clone(),
        )?;
        let mut shared = self
            .handle
            .shared
            .lock()
            .map_err(|_| ServiceError::new(ErrorKind::Io, "Simulator state lock failed"))?;
        shared.generation += 1;
        let generation = shared.generation;
        shared.pedal.reset_stream();
        shared.active = Some((generation, sender.clone()));
        Ok(Box::new(SimulatorSession {
            handle: self.handle.clone(),
            generation,
            sender,
            receive,
            cancelled,
        }))
    }
}
struct SimulatorSession {
    handle: SimulatorHandle,
    generation: u64,
    sender: Sender<Delivery>,
    receive: ReceiveSink,
    cancelled: ShutdownToken,
}
impl MidiSession for SimulatorSession {
    fn send(&mut self, bytes: &[u8]) -> Result<(), ServiceError> {
        self.health()?;
        let now = self.handle.origin.elapsed();
        let deliveries = self
            .handle
            .shared
            .lock()
            .map_err(|_| ServiceError::new(ErrorKind::Io, "Simulator state lock failed"))?
            .pedal
            .receive(bytes, now)?;
        for delivery in deliveries {
            match delivery {
                Delivery::Bytes { at, bytes } if at <= now => self.receive.receive(&bytes),
                Delivery::Disconnected { at } if at <= now => {
                    self.handle.present.store(false, Ordering::Release)
                }
                delivery => self.sender.try_send(delivery).map_err(|_| {
                    ServiceError::new(
                        ErrorKind::Busy,
                        "Simulator delivery queue is full or closed",
                    )
                })?,
            }
        }
        Ok(())
    }
    fn health(&mut self) -> Result<(), ServiceError> {
        if !self.handle.present.load(Ordering::Acquire) || self.cancelled.cancelled() {
            Err(ServiceError::new(
                ErrorKind::Disconnected,
                "Simulator disconnected",
            ))
        } else {
            Ok(())
        }
    }
}
impl Drop for SimulatorSession {
    fn drop(&mut self) {
        self.cancelled.cancel();
        if let Ok(mut shared) = self.handle.shared.lock()
            && shared
                .active
                .as_ref()
                .is_some_and(|(generation, _)| *generation == self.generation)
        {
            shared.active = None;
            shared.pedal.reset_stream();
        }
    }
}

/// Production actor with a real-codec simulated backend. The older
/// DemoDeviceService is retained only as an explicitly named domain test stub.
pub fn simulator_service(wake: WakeUi) -> gp200_transport::MidiDeviceService {
    let config = gp200_transport::TransportConfig {
        packet_spacing: Duration::ZERO,
        minimum_request_spacing: Duration::ZERO,
        write_settle: Duration::from_millis(5),
        ..Default::default()
    };
    gp200_transport::MidiDeviceService::with_backend(
        Box::new(SimulatorBackend::with_pedal(SimulatedPedal::public_demo())),
        config,
        wake,
    )
}

pub(crate) fn delivery_worker(
    receive: ReceiveSink,
    origin: Instant,
    present: Arc<AtomicBool>,
) -> Result<(Sender<Delivery>, ShutdownToken), ServiceError> {
    let (sender, receiver) = bounded::<Delivery>(4096);
    let cancelled = ShutdownToken::default();
    let worker_cancel = cancelled.clone();
    std::thread::Builder::new()
        .name("gp200-offline-rx".into())
        .spawn(move || {
            let mut scheduled = Vec::<Delivery>::new();
            while !worker_cancel.cancelled() {
                while scheduled.len() < 4096 {
                    let Ok(delivery) = receiver.try_recv() else {
                        break;
                    };
                    scheduled.push(delivery);
                }
                scheduled.sort_by_key(Delivery::at);
                let due = scheduled.partition_point(|delivery| delivery.at() <= origin.elapsed());
                for delivery in scheduled.drain(..due) {
                    if worker_cancel.cancelled() {
                        return;
                    }
                    match delivery {
                        Delivery::Bytes { bytes, .. } => receive.receive(&bytes),
                        Delivery::Disconnected { .. } => {
                            present.store(false, Ordering::Release);
                            return;
                        }
                    }
                }
                let wait = scheduled
                    .first()
                    .map_or(Duration::from_millis(5), |delivery| {
                        delivery
                            .at()
                            .saturating_sub(origin.elapsed())
                            .min(Duration::from_millis(5))
                    });
                if scheduled.len() < 4096 {
                    if let Ok(delivery) = receiver.recv_timeout(wait) {
                        scheduled.push(delivery);
                    }
                } else {
                    std::thread::sleep(wait);
                }
            }
        })
        .map_err(|error| {
            ServiceError::new(
                ErrorKind::Io,
                format!("Cannot start offline receiver: {error}"),
            )
        })?;
    Ok((sender, cancelled))
}
