use crossbeam_channel::Sender;
use gp200_domain::{
    BackendMode, DeviceSelection, ErrorKind, PortPairCandidate, ServiceError, SessionEpoch,
};
use midir::{Ignore, MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Original source observation, allocated before copying/rejection.
#[derive(Debug)]
pub(crate) struct ObservedCallback {
    pub(crate) callback: gp200_domain::FirmwareCaptureCallback,
    pub(crate) clock_origin: String,
}
pub(crate) struct NormalCallbackSource {
    origin: std::time::Instant,
    clock_origin: String,
    allocated: std::sync::atomic::AtomicU64,
    failed: AtomicBool,
    first_failure: std::sync::Mutex<Option<gp200_domain::FirmwareCaptureCallback>>,
    first_missing: std::sync::atomic::AtomicU64,
    last_missing: std::sync::atomic::AtomicU64,
}
impl NormalCallbackSource {
    pub(crate) fn new(origin: std::time::Instant, epoch: SessionEpoch) -> Arc<Self> {
        Arc::new(Self {
            origin,
            clock_origin: format!(
                "normal-source-{}-{}-{:?}",
                std::process::id(),
                epoch.0,
                origin
            ),
            allocated: std::sync::atomic::AtomicU64::new(0),
            failed: AtomicBool::new(false),
            first_failure: std::sync::Mutex::new(None),
            first_missing: std::sync::atomic::AtomicU64::new(0),
            last_missing: std::sync::atomic::AtomicU64::new(0),
        })
    }
    pub(crate) fn origin(&self) -> std::time::Instant {
        self.origin
    }
    pub(crate) fn clock_origin(&self) -> &str {
        &self.clock_origin
    }
    pub(crate) fn fence(&self, processed: u64) -> gp200_domain::FirmwareCaptureFence {
        gp200_domain::FirmwareCaptureFence {
            allocated: self.allocated.load(Ordering::Acquire),
            processed,
        }
    }
    pub(crate) fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
    fn reject(&self, callback: gp200_domain::FirmwareCaptureCallback) {
        self.failed.store(true, Ordering::Release);
        let _ = self.first_missing.compare_exchange(
            0,
            callback.ordinal,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.last_missing
            .fetch_max(callback.ordinal, Ordering::AcqRel);
        if let Ok(mut first) = self.first_failure.try_lock()
            && first.is_none()
        {
            *first = Some(callback);
        }
    }
    pub(crate) fn add_failure_evidence(&self, e: &mut gp200_domain::FirmwareCaptureEvidence) {
        if let Ok(first) = self.first_failure.try_lock()
            && let Some(c) = first.as_ref()
            && e.callbacks.len() < gp200_domain::FIRMWARE_CAPTURE_CHUNK_LIMIT
            && e.raw.len()
                + e.offered_read_bytes.len()
                + e.callbacks.iter().map(|r| r.retained.len()).sum::<usize>()
                + c.retained.len()
                <= gp200_domain::FIRMWARE_CAPTURE_RECORD_MATERIAL_LIMIT
            && !e.callbacks.iter().any(|r| r == c)
        {
            e.callbacks.push(c.clone());
        }
        let first = self.first_missing.load(Ordering::Acquire);
        let last = self.last_missing.load(Ordering::Acquire);
        if first != 0 && e.missing.len() < gp200_domain::FIRMWARE_CAPTURE_DIAGNOSTIC_LIMIT {
            e.missing.push(gp200_domain::FirmwareCaptureMissingExtent{first_ordinal:first,last_ordinal:last.max(first),reason:"Rejected callback positions within this range; exact first original prefix retained, other rejected extents unavailable".into()});
        }
        if e.after.allocated > e.after.processed && e.missing.is_empty() {
            e.missing.push(gp200_domain::FirmwareCaptureMissingExtent{first_ordinal:e.after.processed.saturating_add(1),last_ordinal:e.after.allocated,reason:"Allocated source callbacks not yet processed; acquisition cannot claim a completed frontier".into()});
        }
    }
}
/// Owning observation produced by the concrete reader's explicit join. Public
/// DTOs cannot construct or clone this receipt.
/// ```compile_fail
/// fn duplicate(r: gp200_transport::NormalReaderCloseReceipt) { let _ = r.clone(); }
/// ```
/// ```compile_fail
/// fn imported(bytes: &[u8]) { let _: gp200_transport::NormalReaderCloseReceipt = serde_json::from_slice(bytes).unwrap(); }
/// ```
pub struct NormalReaderCloseReceipt {
    pub(crate) source: Arc<NormalCallbackSource>,
    pub(crate) epoch: SessionEpoch,
    pub(crate) started: Option<u64>,
    pub(crate) finished: Option<u64>,
    pub(crate) allocated: u64,
    pub(crate) join: gp200_domain::FirmwareNormalReaderJoinObservation,
    pub(crate) fault: Option<ServiceError>,
    pub(crate) overflow: bool,
    pub(crate) output_destroyed: bool,
}
impl NormalCallbackSource {
    pub(crate) fn now_ns(&self) -> Option<u64> {
        u64::try_from(self.origin.elapsed().as_nanos()).ok()
    }
    pub(crate) fn first_failure(&self) -> Option<gp200_domain::FirmwareCaptureCallback> {
        self.first_failure.lock().ok().and_then(|r| r.clone())
    }
}
#[derive(Clone)]
enum ReceiveSender {
    #[cfg(test)]
    Legacy(Sender<(SessionEpoch, Vec<u8>)>),
    Observed(Sender<ObservedCallback>),
}
/// MIDI callbacks never wait for a queue, disk or mutex. Both rejected and
/// accepted callbacks allocate their ordinal/time before bounds/copy.
#[derive(Clone)]
pub struct ReceiveSink {
    sender: ReceiveSender,
    pub(crate) overflow: Arc<AtomicBool>,
    pub(crate) epoch: SessionEpoch,
    pub(crate) source: Arc<NormalCallbackSource>,
}
impl ReceiveSink {
    #[cfg(test)]
    pub(crate) fn new(
        sender: Sender<(SessionEpoch, Vec<u8>)>,
        overflow: Arc<AtomicBool>,
        epoch: SessionEpoch,
    ) -> Self {
        Self {
            sender: ReceiveSender::Legacy(sender),
            overflow,
            epoch,
            source: NormalCallbackSource::new(std::time::Instant::now(), epoch),
        }
    }
    pub(crate) fn observed(
        sender: Sender<ObservedCallback>,
        overflow: Arc<AtomicBool>,
        epoch: SessionEpoch,
        source: Arc<NormalCallbackSource>,
    ) -> Self {
        Self {
            sender: ReceiveSender::Observed(sender),
            overflow,
            epoch,
            source,
        }
    }
    pub fn receive(&self, bytes: &[u8]) {
        let allocated =
            self.source
                .allocated
                .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1));
        let ordinal = match allocated {
            Ok(n) => n + 1,
            Err(_) => {
                self.source.failed.store(true, Ordering::Release);
                self.overflow.store(true, Ordering::Release);
                return;
            }
        };
        let monotonic_ns = u64::try_from(self.source.origin.elapsed().as_nanos()).ok();
        let callback = gp200_domain::FirmwareCaptureCallback {
            ordinal,
            epoch: self.epoch,
            monotonic_ns,
            original_len: bytes.len() as u64,
            retained: bytes[..bytes.len().min(gp200_protocol::MAX_PACKET_LENGTH)].to_vec(),
        };
        if bytes.is_empty()
            || bytes.len() > gp200_protocol::MAX_PACKET_LENGTH
            || monotonic_ns.is_none()
        {
            self.source.reject(callback);
            self.overflow.store(true, Ordering::Release);
            return;
        }
        let rejected = match &self.sender {
            #[cfg(test)]
            ReceiveSender::Legacy(sender) => sender
                .try_send((self.epoch, callback.retained.clone()))
                .is_err(),
            ReceiveSender::Observed(sender) => match sender.try_send(ObservedCallback {
                callback: callback.clone(),
                clock_origin: self.source.clock_origin.clone(),
            }) {
                Ok(()) => false,
                Err(_) => true,
            },
        };
        if rejected {
            self.source.reject(callback);
            self.overflow.store(true, Ordering::Release);
        }
    }
}

pub trait MidiSession: Send {
    /// Fresh observation outside the receipt gate. Default/custom backends
    /// cannot mint private guarded system provenance.
    fn normal_capture_observation(
        &mut self,
    ) -> Result<crate::FirmwareNormalCaptureObservation, ServiceError> {
        Err(ServiceError::new(
            ErrorKind::Unverified,
            "Backend has no accepted normal capture binding",
        ))
    }
    /// Exactly one bounded nonblocking first OS write. No legacy send/drain.
    fn capture_write_once(&mut self, _bytes: &[u8]) -> crate::FirmwareCaptureWriteResult {
        crate::FirmwareCaptureWriteResult::Unknown(ServiceError::new(
            ErrorKind::Unsupported,
            "Backend has no bounded capture first write",
        ))
    }

    /// Explicit editor return closes the existing reader/output on the worker;
    /// default unsupported is not a fabricated successful join receipt.
    fn close_normal_capture(&mut self) -> Result<(), ServiceError> {
        Err(ServiceError::new(
            ErrorKind::Unverified,
            "Backend has no checked normal capture close",
        ))
    }
    /// Only the concrete system backend can construct this opaque ownership.
    fn take_firmware_handoff(&mut self) -> Result<crate::FirmwareNormalHandoff, ServiceError> {
        Err(ServiceError::new(
            ErrorKind::Unverified,
            "This MIDI backend has no accepted updater ownership binding",
        ))
    }
    /// A successful OS send is not pedal acceptance. The actor verifies reads.
    fn send(&mut self, bytes: &[u8]) -> Result<(), ServiceError>;
    /// Reader-thread faults are reported to the owning actor, never hidden in a
    /// background callback. Most callback backends have no explicit health API.
    fn health(&mut self) -> Result<(), ServiceError> {
        Ok(())
    }
}
pub trait TransportBackend: Send {
    /// Backend provenance is supplied by its owner, never inferred from a
    /// simulated identity packet that happens to name the physical device.
    fn mode(&self) -> BackendMode {
        BackendMode::Hardware
    }
    fn enumerate(&mut self) -> Result<Vec<PortPairCandidate>, ServiceError>;
    fn selection_present(&mut self, selection: &DeviceSelection) -> Result<bool, ServiceError> {
        Ok(self
            .enumerate()?
            .iter()
            .any(|pair| &pair.selection == selection))
    }
    fn connect(
        &mut self,
        selection: &DeviceSelection,
        receive: ReceiveSink,
    ) -> Result<Box<dyn MidiSession>, ServiceError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiPortDescriptor {
    /// Unique names survive ALSA client/card renumbering. Duplicate names include
    /// their current backend address and must be selected again after hotplug.
    pub id: String,
    pub name: String,
    pub backend_id: String,
    pub ambiguous: bool,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MidiPortInventory {
    pub inputs: Vec<MidiPortDescriptor>,
    pub outputs: Vec<MidiPortDescriptor>,
}
impl MidiPortInventory {
    pub fn candidates(&self) -> Vec<PortPairCandidate> {
        let mut pairs = Vec::new();
        for input in &self.inputs {
            for output in &self.outputs {
                if input.name == output.name {
                    pairs.push(PortPairCandidate {
                        selection: DeviceSelection {
                            input_id: input.id.clone(),
                            output_id: output.id.clone(),
                            fingerprint_hint: None,
                        },
                        display_name: input.name.clone(),
                        mode_hint: Some(BackendMode::Hardware),
                        matching_confidence: if input.ambiguous || output.ambiguous {
                            "Duplicate names: explicitly select an address; identity required"
                                .into()
                        } else {
                            "Unique name pair; identity still required".into()
                        },
                    });
                }
            }
        }
        pairs
    }
}

#[derive(Default)]
pub struct MidirBackend;
impl MidirBackend {
    pub fn new() -> Self {
        Self
    }

    /// Enumerating does not open the pedal. All ports remain available for an
    /// explicit pair, even if their input/output names differ.
    pub fn inventory(&mut self) -> Result<MidiPortInventory, ServiceError> {
        let input = MidiInput::new("GP-200 Linux discovery input").map_err(midi_error)?;
        let output = MidiOutput::new("GP-200 Linux discovery output").map_err(midi_error)?;
        let inputs = input
            .ports()
            .iter()
            .map(|port| {
                input
                    .port_name(port)
                    .map(|name| (name, port.id()))
                    .map_err(midi_error)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let outputs = output
            .ports()
            .iter()
            .map(|port| {
                output
                    .port_name(port)
                    .map(|name| (name, port.id()))
                    .map_err(midi_error)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(MidiPortInventory {
            inputs: descriptors("in", inputs),
            outputs: descriptors("out", outputs),
        })
    }
}

fn normalized_name(name: &str) -> String {
    // midir's ALSA implementation appends " client:port" to the port name.
    // Strip only a numeric suffix, leaving the actual device/port name intact.
    let Some((prefix, suffix)) = name.rsplit_once(' ') else {
        return name.into();
    };
    let Some((client, port)) = suffix.split_once(':') else {
        return name.into();
    };
    if !client.is_empty()
        && !port.is_empty()
        && client.bytes().all(|b| b.is_ascii_digit())
        && port.bytes().all(|b| b.is_ascii_digit())
    {
        prefix.into()
    } else {
        name.into()
    }
}
fn descriptors(direction: &str, ports: Vec<(String, String)>) -> Vec<MidiPortDescriptor> {
    let mut counts = BTreeMap::new();
    for (name, _) in &ports {
        *counts.entry(normalized_name(name)).or_insert(0_usize) += 1;
    }
    let mut result = ports
        .into_iter()
        .map(|(name, backend_id)| {
            let name = normalized_name(&name);
            let ambiguous = counts[&name] > 1;
            let encoded = name.bytes().map(|b| format!("{b:02x}")).collect::<String>();
            let id = if ambiguous {
                format!("midir:{direction}:{encoded}@{backend_id}")
            } else {
                format!("midir:{direction}:{encoded}")
            };
            MidiPortDescriptor {
                id,
                name,
                backend_id,
                ambiguous,
            }
        })
        .collect::<Vec<_>>();
    result.sort_by(|a, b| a.id.cmp(&b.id));
    result
}

impl TransportBackend for MidirBackend {
    fn enumerate(&mut self) -> Result<Vec<PortPairCandidate>, ServiceError> {
        Ok(self.inventory()?.candidates())
    }
    fn selection_present(&mut self, selection: &DeviceSelection) -> Result<bool, ServiceError> {
        let inventory = self.inventory()?;
        Ok(inventory
            .inputs
            .iter()
            .any(|port| port.id == selection.input_id)
            && inventory
                .outputs
                .iter()
                .any(|port| port.id == selection.output_id))
    }
    fn connect(
        &mut self,
        selection: &DeviceSelection,
        receive: ReceiveSink,
    ) -> Result<Box<dyn MidiSession>, ServiceError> {
        let mut input = MidiInput::new("GP-200 Linux input").map_err(midi_error)?;
        input.ignore(Ignore::None);
        let output = MidiOutput::new("GP-200 Linux output").map_err(midi_error)?;
        let in_ports = input.ports();
        let in_descriptors = descriptors(
            "in",
            in_ports
                .iter()
                .map(|port| {
                    input
                        .port_name(port)
                        .map(|name| (name, port.id()))
                        .map_err(midi_error)
                })
                .collect::<Result<Vec<_>, _>>()?,
        );
        let out_ports = output.ports();
        let out_descriptors = descriptors(
            "out",
            out_ports
                .iter()
                .map(|port| {
                    output
                        .port_name(port)
                        .map(|name| (name, port.id()))
                        .map_err(midi_error)
                })
                .collect::<Result<Vec<_>, _>>()?,
        );
        let input_id = in_descriptors
            .iter()
            .find(|p| p.id == selection.input_id)
            .ok_or_else(|| {
                ServiceError::new(ErrorKind::Disconnected, "Selected MIDI input disappeared")
            })?;
        let output_id = out_descriptors
            .iter()
            .find(|p| p.id == selection.output_id)
            .ok_or_else(|| {
                ServiceError::new(ErrorKind::Disconnected, "Selected MIDI output disappeared")
            })?;
        let in_port = in_ports
            .iter()
            .find(|p| p.id() == input_id.backend_id)
            .ok_or_else(|| {
                ServiceError::new(
                    ErrorKind::Disconnected,
                    "MIDI input changed during discovery",
                )
            })?;
        let out_port = out_ports
            .iter()
            .find(|p| p.id() == output_id.backend_id)
            .ok_or_else(|| {
                ServiceError::new(
                    ErrorKind::Disconnected,
                    "MIDI output changed during discovery",
                )
            })?;
        let input_connection = input
            .connect(
                in_port,
                "GP-200 receive",
                move |_, bytes, sink| {
                    sink.receive(bytes);
                },
                receive,
            )
            .map_err(midi_error)?;
        let output_connection = output
            .connect(out_port, "GP-200 send")
            .map_err(midi_error)?;
        Ok(Box::new(MidirSession {
            _input: input_connection,
            output: output_connection,
        }))
    }
}
struct MidirSession {
    _input: MidiInputConnection<ReceiveSink>,
    output: MidiOutputConnection,
}
impl MidiSession for MidirSession {
    fn send(&mut self, bytes: &[u8]) -> Result<(), ServiceError> {
        self.output.send(bytes).map_err(midi_error)
    }
}
fn midi_error(error: impl std::fmt::Display) -> ServiceError {
    let message = error.to_string();
    let lower = message.to_ascii_lowercase();
    let kind = if lower.contains("permission") || lower.contains("denied") {
        ErrorKind::Io
    } else if lower.contains("busy") {
        ErrorKind::Busy
    } else if lower.contains("not found")
        || lower.contains("no such")
        || lower.contains("invalid port")
    {
        ErrorKind::Disconnected
    } else {
        ErrorKind::Io
    };
    ServiceError::new(kind, format!("ALSA MIDI: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unique_descriptors_survive_alsa_renumbering() {
        let a = descriptors(
            "in",
            vec![("GP-200:GP-200 MIDI 1 20:0".into(), "20:0".into())],
        );
        let b = descriptors(
            "in",
            vec![("GP-200:GP-200 MIDI 1 31:0".into(), "31:0".into())],
        );
        assert_eq!(a[0].id, b[0].id);
        assert_ne!(a[0].backend_id, b[0].backend_id);
    }
    #[test]
    fn duplicates_are_explicit_and_not_implicitly_paired() {
        let entries = descriptors(
            "in",
            vec![
                ("GP-200 20:0".into(), "20:0".into()),
                ("GP-200 21:0".into(), "21:0".into()),
            ],
        );
        assert!(entries.iter().all(|entry| entry.ambiguous));
        assert_ne!(entries[0].id, entries[1].id);
    }
    #[test]
    fn callback_overflow_is_visible_and_bounded() {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        let overflow = Arc::new(AtomicBool::new(false));
        let sink = ReceiveSink::new(sender, overflow.clone(), SessionEpoch(7));
        sink.receive(&[1]);
        sink.receive(&[2]);
        assert!(overflow.load(Ordering::Acquire));
        assert_eq!(receiver.len(), 1);
        assert_eq!(receiver.try_recv().unwrap(), (SessionEpoch(7), vec![1]));
    }
}

#[cfg(test)]
mod capture_source_tests {
    use super::*;
    #[test]
    fn original_ordinals_time_epoch_and_first_rejected_prefix_precede_queue_or_size_rejection() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let overflow = Arc::new(AtomicBool::new(false));
        let source = NormalCallbackSource::new(std::time::Instant::now(), SessionEpoch(44));
        let sink = ReceiveSink::observed(tx, overflow.clone(), SessionEpoch(44), source.clone());
        sink.receive(&[1, 2, 3]);
        sink.receive(&[4, 5, 6]);
        sink.receive(&[]);
        sink.receive(&vec![7; gp200_protocol::MAX_PACKET_LENGTH + 1]);
        let accepted = rx.try_recv().unwrap();
        assert_eq!(accepted.callback.ordinal, 1);
        assert_eq!(accepted.callback.epoch, SessionEpoch(44));
        assert_eq!(accepted.callback.retained, [1, 2, 3]);
        assert_eq!(source.fence(1).allocated, 4);
        assert!(source.failed());
        assert!(overflow.load(Ordering::Acquire));
        let rejected = source.first_failure.lock().unwrap();
        let rejected = rejected.as_ref().unwrap();
        assert_eq!(rejected.ordinal, 2);
        assert_eq!(rejected.retained, [4, 5, 6]);
        assert!(accepted.callback.monotonic_ns <= rejected.monotonic_ns);
        assert_eq!(source.first_missing.load(Ordering::Acquire), 2);
        assert_eq!(source.last_missing.load(Ordering::Acquire), 4);
    }
    #[test]
    fn oversized_callback_retains_original_length_and_exact_bounded_prefix() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let source = NormalCallbackSource::new(std::time::Instant::now(), SessionEpoch(9));
        let sink = ReceiveSink::observed(
            tx,
            Arc::new(AtomicBool::new(false)),
            SessionEpoch(9),
            source.clone(),
        );
        let bytes = vec![0x9e; gp200_protocol::MAX_PACKET_LENGTH + 31];
        sink.receive(&bytes);
        assert!(rx.is_empty());
        let first = source.first_failure.lock().unwrap();
        let c = first.as_ref().unwrap();
        assert_eq!(c.ordinal, 1);
        assert_eq!(c.original_len, bytes.len() as u64);
        assert_eq!(c.retained, &bytes[..gp200_protocol::MAX_PACKET_LENGTH]);
        assert_eq!(c.epoch, SessionEpoch(9));
        assert!(c.monotonic_ns.is_some());
    }
}
