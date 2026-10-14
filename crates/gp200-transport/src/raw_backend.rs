//! Direct ALSA raw MIDI alternative. It uses the same actor and validation
//! profiles as midir, with logical card IDs rather than numeric card positions.
use crate::device_session_guard::{EndpointIdentity, open_guarded};
use crate::{MidiSession, ReceiveSink, TransportBackend};
use alsa::{Ctl, Direction, card, rawmidi};
use crossbeam_channel::{RecvTimeoutError, bounded};
use gp200_domain::{
    BackendMode, DeviceFingerprint, DeviceSelection, ErrorKind, FirmwareUsbObservation,
    PortPairCandidate, ServiceError, SessionEpoch,
};
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const READER_STARTUP_TIMEOUT: Duration = Duration::from_secs(1);
const READER_POLL_INTERVAL: Duration = Duration::from_millis(1);

#[derive(Default)]
pub struct AlsaRawBackend;
impl AlsaRawBackend {
    pub fn new() -> Self {
        Self
    }
}
#[derive(Clone)]
struct Port {
    id: String,
    card_id: String,
    device: i32,
    subdevice: i32,
    direction: Direction,
    name: String,
    fingerprint: DeviceFingerprint,
    // Fresh backend observation, never the client's saved fingerprint hint.
    device_location: String,
}
impl Port {
    fn address(&self) -> String {
        format!("hw:{},{},{}", self.card_id, self.device, self.subdevice)
    }
    fn same_subdevice(&self, other: &Self) -> bool {
        self.card_id == other.card_id
            && self.device == other.device
            && self.subdevice == other.subdevice
    }
    fn ownership_identity(&self) -> EndpointIdentity {
        EndpointIdentity {
            location: self.device_location.clone(),
            card_id: self.card_id.clone(),
            device: self.device,
            subdevice: self.subdevice,
        }
    }
}
fn ports() -> Result<Vec<Port>, ServiceError> {
    let mut result = Vec::new();
    for card in card::Iter::new() {
        let card = card.map_err(alsa_error)?;
        let control = Ctl::from_card(&card, false).map_err(alsa_error)?;
        let info = control.card_info().map_err(alsa_error)?;
        let card_id = info.get_id().map_err(alsa_error)?.to_owned();
        let card_name = info.get_name().map_err(alsa_error)?.to_owned();
        let fingerprint = usb_fingerprint(card.get_index(), &card_name);
        let device_location =
            std::fs::canonicalize(format!("/sys/class/sound/card{}/device", card.get_index()))
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|_| format!("alsa-card:{}:{card_id}", card.get_index()));
        for raw in rawmidi::Iter::new(&control) {
            let raw = raw.map_err(alsa_error)?;
            let direction = raw.get_stream();
            let label = if direction == Direction::Capture {
                "in"
            } else {
                "out"
            };
            let name = raw.get_subdevice_name().map_err(alsa_error)?;
            result.push(Port {
                id: format!(
                    "alsa-raw:{label}:{card_id}:{}:{}",
                    raw.get_device(),
                    raw.get_subdevice()
                ),
                card_id: card_id.clone(),
                device: raw.get_device(),
                subdevice: raw.get_subdevice(),
                direction,
                name: if name.is_empty() {
                    card_name.clone()
                } else {
                    name
                },
                fingerprint: fingerprint.clone(),
                device_location: device_location.clone(),
            });
        }
    }
    result.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(result)
}
fn usb_fingerprint(card_index: i32, name: &str) -> DeviceFingerprint {
    let path = std::fs::canonicalize(format!("/sys/class/sound/card{card_index}/device")).ok();
    let mut fingerprint = DeviceFingerprint {
        vendor: None,
        product: None,
        serial: None,
        hint: name.into(),
    };
    if let Some(path) = path {
        for ancestor in path.ancestors() {
            let read_hex = |file| {
                std::fs::read_to_string(ancestor.join(file))
                    .ok()
                    .and_then(|value| u16::from_str_radix(value.trim(), 16).ok())
            };
            if let (Some(vendor), Some(product)) = (read_hex("idVendor"), read_hex("idProduct")) {
                fingerprint.vendor = Some(vendor);
                fingerprint.product = Some(product);
                fingerprint.serial = std::fs::read_to_string(ancestor.join("serial"))
                    .ok()
                    .map(|s| s.trim().into())
                    .filter(|s: &String| !s.is_empty());
                fingerprint.hint = ancestor.display().to_string();
                break;
            }
        }
    }
    fingerprint
}
fn rich_binding(port: &Port) -> Option<FirmwareUsbObservation> {
    let f = &port.fingerprint;
    if !f.hint.starts_with("/sys/") || !port.device_location.starts_with(&f.hint) {
        return None;
    }
    let binding = FirmwareUsbObservation {
        topology: f.hint.clone(),
        vendor: f.vendor?,
        product: f.product?,
        serial: f.serial.clone()?,
        interface: port.device_location.clone(),
        endpoint: port.address(),
    };
    binding.validate().ok()?;
    Some(binding)
}
impl TransportBackend for AlsaRawBackend {
    fn enumerate(&mut self) -> Result<Vec<PortPairCandidate>, ServiceError> {
        let ports = ports()?;
        let inputs = ports
            .iter()
            .filter(|port| port.direction == Direction::Capture);
        let outputs = ports
            .iter()
            .filter(|port| port.direction == Direction::Playback)
            .collect::<Vec<_>>();
        Ok(inputs
            .flat_map(|input| {
                outputs
                    .iter()
                    .filter(move |output| input.same_subdevice(output))
                    .map(move |output| PortPairCandidate {
                        selection: DeviceSelection {
                            input_id: input.id.clone(),
                            output_id: output.id.clone(),
                            fingerprint_hint: Some(input.fingerprint.clone()),
                        },
                        display_name: input.name.clone(),
                        mode_hint: Some(BackendMode::Hardware),
                        matching_confidence:
                            "Logical ALSA card/device/subdevice pair; USB hint; identity required"
                                .into(),
                    })
            })
            .collect())
    }
    fn selection_present(&mut self, selection: &DeviceSelection) -> Result<bool, ServiceError> {
        let ports = ports()?;
        Ok(ports.iter().any(|port| port.id == selection.input_id)
            && ports.iter().any(|port| port.id == selection.output_id))
    }
    fn connect(
        &mut self,
        selection: &DeviceSelection,
        receive: ReceiveSink,
    ) -> Result<Box<dyn MidiSession>, ServiceError> {
        let inventory = ports()?
            .into_iter()
            .map(|port| (port.id.clone(), port))
            .collect::<BTreeMap<_, _>>();
        let input_port = inventory
            .get(&selection.input_id)
            .filter(|port| port.direction == Direction::Capture)
            .ok_or_else(|| {
                ServiceError::new(
                    ErrorKind::Disconnected,
                    "Selected ALSA raw MIDI input disappeared",
                )
            })?;
        let output_port = inventory
            .get(&selection.output_id)
            .filter(|port| port.direction == Direction::Playback)
            .ok_or_else(|| {
                ServiceError::new(
                    ErrorKind::Disconnected,
                    "Selected ALSA raw MIDI output disappeared",
                )
            })?;
        if let Some(expected) = &selection.fingerprint_hint {
            let actual = &input_port.fingerprint;
            if expected
                .vendor
                .is_some_and(|value| actual.vendor != Some(value))
                || expected
                    .product
                    .is_some_and(|value| actual.product != Some(value))
                || expected
                    .serial
                    .as_ref()
                    .is_some_and(|value| actual.serial.as_ref() != Some(value))
            {
                return Err(ServiceError::new(
                    ErrorKind::Conflict,
                    "USB identity hint changed; select this pedal again",
                ));
            }
        }
        let binding = rich_binding(input_port);
        open_guarded(
            binding.as_ref(),
            &[
                input_port.ownership_identity(),
                output_port.ownership_identity(),
            ],
            || {
                let input = rawmidi::Rawmidi::new(&input_port.address(), Direction::Capture, true)
                    .map_err(alsa_error)?;
                let mut output =
                    rawmidi::Rawmidi::new(&output_port.address(), Direction::Playback, true)
                        .map_err(alsa_error)?;
                // Closing a research session must not inject ALSA-generated
                // active sensing bytes into a pedal operation.
                let mut params = output.params_current().map_err(alsa_error)?;
                params
                    .set_no_active_sensing(&output, true)
                    .map_err(alsa_error)?;
                output.params(&params).map_err(alsa_error)?;
                let reader = RawReader::start(input, receive, READER_STARTUP_TIMEOUT)?;
                Ok(Box::new(RawSession {
                    output: Some(output),
                    reader: Some(reader),
                    binding: binding.clone(),
                    address: input_port.address(),
                }))
            },
        )
    }
}

/// Implementations must use nonblocking I/O so cancellation can join the reader.
/// Kept private to inject deterministic startup failures without opening MIDI.
pub(crate) trait RawMidiInput: Send + 'static {
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, InputReadError>;
    fn check_status(&mut self) -> Result<(), ServiceError>;
}

pub(crate) enum InputReadError {
    Interrupted,
    WouldBlock,
    Failed(ServiceError),
}

impl RawMidiInput for rawmidi::Rawmidi {
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, InputReadError> {
        rawmidi::Rawmidi::read(self, buffer).map_err(|error| match error.errno() {
            4 => InputReadError::Interrupted,
            11 => InputReadError::WouldBlock,
            _ => InputReadError::Failed(alsa_error(error)),
        })
    }

    fn check_status(&mut self) -> Result<(), ServiceError> {
        let status = self.status().map_err(alsa_error)?;
        if status.get_xruns() > 0 {
            return Err(ServiceError::new(
                ErrorKind::Protocol,
                "ALSA raw MIDI input overrun; complete replies cannot be trusted",
            ));
        }
        Ok(())
    }
}

struct RawReader {
    stop: Arc<AtomicBool>,
    fault: Arc<Mutex<Option<ServiceError>>>,
    thread: Option<JoinHandle<()>>,
    source: Arc<crate::backend::NormalCallbackSource>,
    epoch: SessionEpoch,
}

impl RawReader {
    fn start(
        mut input: impl RawMidiInput,
        receive: ReceiveSink,
        timeout: Duration,
    ) -> Result<Self, ServiceError> {
        let source = receive.source.clone();
        let epoch = receive.epoch;
        let stop = Arc::new(AtomicBool::new(false));
        let fault = Arc::new(Mutex::new(None));
        let reader_stop = stop.clone();
        let reader_fault = fault.clone();
        let (ready_sender, ready_receiver) = bounded(1);
        let thread = thread::Builder::new()
            .name("gp200-alsa-raw-rx".into())
            .spawn(move || {
                let mut ready_sender = Some(ready_sender);
                let mut buffer = [0_u8; 4096];
                while !reader_stop.load(Ordering::Acquire) {
                    let mut idle = true;
                    let read = match input.read(&mut buffer) {
                        Ok(length) if length <= buffer.len() => {
                            if length > 0 {
                                idle = false;
                                receive.receive(&buffer[..length]);
                            }
                            Ok(true)
                        }
                        Ok(_) => Err(ServiceError::new(
                            ErrorKind::Protocol,
                            "ALSA raw MIDI reader returned more bytes than its buffer",
                        )),
                        Err(InputReadError::WouldBlock) => Ok(true),
                        Err(InputReadError::Interrupted) => Ok(false),
                        Err(InputReadError::Failed(error)) => Err(error),
                    };
                    let iteration = read.and_then(|primed| {
                        input.check_status()?;
                        if receive.overflow.load(Ordering::Acquire) {
                            return Err(ServiceError::new(
                                ErrorKind::Protocol,
                                "ALSA raw MIDI receive queue overflow; complete replies cannot be trusted",
                            ));
                        }
                        Ok(primed)
                    });
                    match iteration {
                        Ok(true) => {
                            // On Linux hw raw MIDI a completed read enables input
                            // delivery, including an empty nonblocking EAGAIN.
                            // EINTR alone does not establish that ordering.
                            if let Some(sender) = ready_sender.take() {
                                let _ = sender.send(Ok(()));
                            }
                        }
                        Ok(false) => {}
                        Err(error) => {
                            *reader_fault.lock().expect("raw MIDI fault mutex") =
                                Some(error.clone());
                            if let Some(sender) = ready_sender.take() {
                                let _ = sender.send(Err(error));
                            }
                            break;
                        }
                    }
                    if idle {
                        thread::sleep(READER_POLL_INTERVAL);
                    }
                }
            })
            .map_err(|error| {
                ServiceError::new(ErrorKind::Io, format!("Cannot start raw MIDI reader: {error}"))
            })?;
        let reader = Self {
            stop,
            fault,
            thread: Some(thread),
            source,
            epoch,
        };
        match ready_receiver.recv_timeout(timeout) {
            Ok(Ok(())) => Ok(reader),
            Ok(Err(error)) => Err(error),
            Err(RecvTimeoutError::Timeout) => Err(ServiceError::new(
                ErrorKind::Timeout,
                "ALSA raw MIDI input did not become ready before its startup deadline; session closed without sending",
            )),
            Err(RecvTimeoutError::Disconnected) => Err(ServiceError::new(
                ErrorKind::Io,
                "ALSA raw MIDI reader exited before input readiness; session closed without sending",
            )),
        }
        // On every error the local reader is dropped and joined before connect
        // returns. The input and output handles cannot escape a failed startup.
    }

    fn close_receipt(&mut self) -> crate::backend::NormalReaderCloseReceipt {
        let started = self.source.now_ns();
        self.stop.store(true, Ordering::Release);
        let join = match self.thread.take() {
            Some(thread) => match thread.join() {
                Ok(()) => gp200_domain::FirmwareNormalReaderJoinObservation::Returned,
                Err(_) => gp200_domain::FirmwareNormalReaderJoinObservation::Panicked,
            },
            None => gp200_domain::FirmwareNormalReaderJoinObservation::Unavailable,
        };
        crate::backend::NormalReaderCloseReceipt {
            source: self.source.clone(),
            epoch: self.epoch,
            started,
            finished: self.source.now_ns(),
            allocated: self.source.fence(0).allocated,
            join,
            fault: self.health().err(),
            overflow: self.source.failed(),
            output_destroyed: false,
        }
    }
    fn close(&mut self) -> Result<(), ServiceError> {
        let receipt = self.close_receipt();
        if receipt.join != gp200_domain::FirmwareNormalReaderJoinObservation::Returned {
            return Err(ServiceError::new(
                ErrorKind::Io,
                "Normal reader join unavailable or panicked",
            ));
        }
        if let Some(error) = receipt.fault {
            return Err(error);
        }
        if receipt.overflow {
            return Err(ServiceError::new(
                ErrorKind::Protocol,
                "Normal reader lost callback evidence",
            ));
        }
        Ok(())
    }
    fn health(&self) -> Result<(), ServiceError> {
        match self
            .fault
            .lock()
            .map_err(|_| {
                ServiceError::new(ErrorKind::Io, "Normal raw reader fault state poisoned")
            })?
            .as_ref()
        {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
}

impl Drop for RawReader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct RawSession {
    output: Option<rawmidi::Rawmidi>,
    reader: Option<RawReader>,
    binding: Option<FirmwareUsbObservation>,
    address: String,
}
impl MidiSession for RawSession {
    fn close_normal_capture(&mut self) -> Result<(), ServiceError> {
        let result = self
            .reader
            .as_mut()
            .ok_or_else(|| {
                ServiceError::new(ErrorKind::Disconnected, "Normal reader already retired")
            })?
            .close();
        drop(self.reader.take());
        drop(self.output.take());
        result
    }
    fn normal_capture_observation(
        &mut self,
    ) -> Result<crate::FirmwareNormalCaptureObservation, ServiceError> {
        self.health()?;
        let actual = self.binding.as_ref().ok_or_else(|| {
            ServiceError::new(ErrorKind::Unverified, "Normal raw USB binding unavailable")
        })?;
        let inventory = ports()?;
        let input = inventory
            .iter()
            .find(|p| p.direction == Direction::Capture && p.address() == self.address)
            .ok_or_else(|| {
                ServiceError::new(ErrorKind::Disconnected, "Fresh normal input absent")
            })?;
        let output = inventory
            .iter()
            .find(|p| p.direction == Direction::Playback && p.same_subdevice(input))
            .ok_or_else(|| {
                ServiceError::new(ErrorKind::Disconnected, "Fresh normal output absent")
            })?;
        if rich_binding(input).as_ref() != Some(actual)
            || rich_binding(output).as_ref() != Some(actual)
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Normal raw USB association changed",
            ));
        }
        Ok(crate::FirmwareNormalCaptureObservation {
            usb: actual.clone(),
            owner: format!("raw:{}", self.address),
            origin: gp200_domain::FirmwareCaptureOrigin::GuardedSystemNormal,
            guarded: false,
        })
    }
    fn capture_write_once(&mut self, bytes: &[u8]) -> crate::FirmwareCaptureWriteResult {
        // Rawmidi was opened O_NONBLOCK. Do not call health, retry, sleep or drain
        // under the genuine durability receipt's first-offer fence.
        match self.output.as_ref() {
            Some(output) => match output.write(bytes) {
                Ok(accepted) => crate::FirmwareCaptureWriteResult::Known {
                    accepted,
                    error: None,
                },
                Err(error) if matches!(error.errno(), 4 | 11) => {
                    crate::FirmwareCaptureWriteResult::Known {
                        accepted: 0,
                        error: Some(alsa_error(error)),
                    }
                }
                Err(error) => crate::FirmwareCaptureWriteResult::Unknown(alsa_error(error)),
            },
            None => crate::FirmwareCaptureWriteResult::Known {
                accepted: 0,
                error: Some(ServiceError::new(
                    ErrorKind::Disconnected,
                    "Normal raw output retired",
                )),
            },
        }
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), ServiceError> {
        self.reader
            .as_ref()
            .ok_or_else(|| ServiceError::new(ErrorKind::Disconnected, "Normal reader handed off"))?
            .health()?;
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut remaining = bytes;
        while !remaining.is_empty() {
            match self
                .output
                .as_ref()
                .expect("normal output")
                .write(remaining)
            {
                Ok(count) => remaining = &remaining[count..],
                Err(error) if matches!(error.errno(), 4 | 11) => {}
                Err(error) => return Err(alsa_error(error)),
            }
            if Instant::now() >= deadline {
                return Err(ServiceError::new(
                    ErrorKind::Timeout,
                    "ALSA raw MIDI send could not drain before its deadline",
                ));
            }
            if !remaining.is_empty() {
                thread::sleep(Duration::from_millis(1));
            }
        }
        loop {
            match self.output.as_ref().expect("normal output").drain() {
                Ok(()) => return Ok(()),
                Err(error) if matches!(error.errno(), 4 | 11) => {}
                Err(error) => return Err(alsa_error(error)),
            }
            if Instant::now() >= deadline {
                return Err(ServiceError::new(
                    ErrorKind::Timeout,
                    "ALSA raw MIDI output drain timed out after a possibly partial send",
                ));
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn take_firmware_handoff(&mut self) -> Result<crate::FirmwareNormalHandoff, ServiceError> {
        self.normal_capture_observation()?;
        let binding = self.binding.clone().ok_or_else(|| {
            ServiceError::new(
                ErrorKind::Unverified,
                "Fresh USB serial/topology is incomplete",
            )
        })?;
        // A failed join is still an actual retirement. Return its owning
        // evidence rather than pretending that the former session is Ready.
        let mut close = self
            .reader
            .as_mut()
            .ok_or_else(|| {
                ServiceError::new(ErrorKind::Disconnected, "Normal reader already retired")
            })?
            .close_receipt();
        drop(self.reader.take());
        drop(self.output.take());
        close.output_destroyed = true;
        Ok(crate::FirmwareNormalHandoff {
            binding,
            normal_epoch: close.epoch,
            normal_close: Some(close),
            retirement: None,
            factory: Some(crate::firmware_raw::FirmwareRawFactory {
                address: self.address.clone(),
            }),
            guard: None,
            #[cfg(test)]
            mock: None,
            transition: None,
        })
    }

    fn health(&mut self) -> Result<(), ServiceError> {
        self.reader
            .as_ref()
            .ok_or_else(|| ServiceError::new(ErrorKind::Disconnected, "Normal reader handed off"))?
            .health()
    }
}
impl Drop for RawSession {
    fn drop(&mut self) {
        drop(self.reader.take());
        if let Some(output) = self.output.take() {
            let _ = output.drop();
        }
    }
}

#[cfg(test)]
#[path = "raw_backend/readiness_tests.rs"]
mod readiness_tests;
fn alsa_error(error: alsa::Error) -> ServiceError {
    let kind = match error.errno() {
        19 | 2 => ErrorKind::Disconnected,
        16 => ErrorKind::Busy,
        13 | 1 => ErrorKind::Io,
        _ => ErrorKind::Io,
    };
    ServiceError::new(kind, format!("ALSA raw MIDI: {error}"))
}

#[cfg(feature = "firmware-linux-completion")]
pub(crate) fn completion_endpoints() -> Result<
    Vec<crate::firmware_linux_completion_binding::CompletionEndpointObservation>,
    ServiceError,
> {
    let inventory = ports()?;
    let mut observed = Vec::new();
    for input in inventory
        .iter()
        .filter(|port| port.direction == Direction::Capture)
    {
        for output in inventory
            .iter()
            .filter(|port| port.direction == Direction::Playback && port.same_subdevice(input))
        {
            if let (Some(binding), Some(other)) = (rich_binding(input), rich_binding(output))
                && binding == other
            {
                observed.push(
                    crate::firmware_linux_completion_binding::CompletionEndpointObservation {
                        binding,
                        selection: DeviceSelection {
                            input_id: input.id.clone(),
                            output_id: output.id.clone(),
                            fingerprint_hint: Some(input.fingerprint.clone()),
                        },
                    },
                );
            }
        }
    }
    Ok(observed)
}
#[cfg(feature = "firmware-linux-completion")]
pub(crate) fn completion_normal_reads(
    witness: &crate::firmware_linux_completion_binding::CompletionBindingWitness,
    normal: &FirmwareUsbObservation,
    guard: &crate::device_session_guard::DeviceSessionGuard,
    epoch: SessionEpoch,
    stop: &AtomicBool,
) -> Result<crate::firmware_linux_completion::CompletionNormalProof, ServiceError> {
    witness.revalidate(normal, guard)?;
    let binding = witness.observation().binding.clone();
    let selection = witness.observation().selection.clone();
    let origin = Instant::now();
    let source = crate::backend::NormalCallbackSource::new(origin, epoch);
    let overflow = Arc::new(AtomicBool::new(false));
    let (tx, received) = bounded(64);
    let sink = ReceiveSink::observed(tx, overflow.clone(), epoch, source.clone());
    let input =
        rawmidi::Rawmidi::new(&binding.endpoint, Direction::Capture, true).map_err(alsa_error)?;
    let reader = RawReader::start(input, sink, READER_STARTUP_TIMEOUT)?;
    let mut output =
        rawmidi::Rawmidi::new(&binding.endpoint, Direction::Playback, true).map_err(alsa_error)?;
    let mut params = output.params_current().map_err(alsa_error)?;
    params
        .set_no_active_sensing(&output, true)
        .map_err(alsa_error)?;
    output.params(&params).map_err(alsa_error)?;
    let session = Box::new(RawSession {
        output: Some(output),
        reader: Some(reader),
        binding: Some(binding.clone()),
        address: binding.endpoint.clone(),
    });
    let proof = crate::firmware_linux_completion::read_normal_owned(
        crate::firmware_linux_completion::CompletionNormalReadInput {
            session,
            source,
            received,
            overflow,
            epoch,
            selection,
            binding,
            mode: BackendMode::Hardware,
            config: crate::TransportConfig::default(),
        },
        stop,
    )?;
    witness.revalidate(normal, guard)?;
    Ok(proof)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn logical_raw_address_uses_card_id_not_card_number() {
        let port = Port {
            id: "alsa-raw:in:GP200:0:0".into(),
            card_id: "GP200".into(),
            device: 0,
            subdevice: 0,
            direction: Direction::Capture,
            name: "GP-200".into(),
            fingerprint: usb_fingerprint(-1, "GP-200"),
            device_location: "fixture-card".into(),
        };
        assert_eq!(port.address(), "hw:GP200,0,0");
        assert_eq!(port.fingerprint.vendor, None);
        assert_eq!(port.fingerprint.hint, "GP-200");
    }
    #[test]
    fn usb_path_without_device_metadata_is_only_a_name_hint() {
        assert!(!std::path::Path::new("/sys/class/sound/card-1/device").exists());
        assert_eq!(usb_fingerprint(-1, "No hardware").serial, None);
    }
}
