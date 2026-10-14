use crate::Delivery;
use crossbeam_channel::Sender;
use gp200_domain::{
    BackendMode, DeviceSelection, ErrorKind, PortPairCandidate, ServiceError, ShutdownToken,
};
use gp200_protocol::{RawTransfer, Reassembler, StreamDecoder, StreamEvent};
use gp200_transport::{MidiSession, ReceiveSink, TransportBackend};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

/// Recorded byte chunks retain their original byte authority. A fixture must
/// explicitly label synthetic timestamps when source timing was unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureChunk {
    pub offset: Duration,
    pub bytes: Vec<u8>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedExchange {
    pub expected_request: RawTransfer,
    pub response: Vec<CaptureChunk>,
    pub provenance: String,
    pub synthetic_timing: bool,
}
impl CapturedExchange {
    pub fn from_bytes(
        expected_request: RawTransfer,
        response: Vec<u8>,
        provenance: impl Into<String>,
    ) -> Self {
        Self {
            expected_request,
            response: vec![CaptureChunk {
                offset: Duration::ZERO,
                bytes: response,
            }],
            provenance: provenance.into(),
            synthetic_timing: true,
        }
    }
    fn validate(&self) -> Result<(), ServiceError> {
        if self.provenance.is_empty()
            || self.response.len() > 4096
            || self
                .response
                .iter()
                .map(|chunk| chunk.bytes.len())
                .sum::<usize>()
                > 4 * 1024 * 1024
            || self
                .response
                .windows(2)
                .any(|pair| pair[0].offset > pair[1].offset)
            || self
                .response
                .last()
                .is_some_and(|chunk| chunk.offset > Duration::from_secs(3600))
        {
            return Err(ServiceError::invalid(
                "Replay exchange has missing provenance or exceeds its bounds",
            ));
        }
        Ok(())
    }
}

/// Deterministic expectation replay: an edit can succeed only if this fixture
/// explicitly expects its bytes. Replay still cannot validate hardware writes.
pub struct ExpectationReplay {
    exchanges: VecDeque<CapturedExchange>,
    decoder: StreamDecoder,
    assembler: Reassembler,
    now: Duration,
}
impl ExpectationReplay {
    pub fn new(exchanges: Vec<CapturedExchange>) -> Result<Self, ServiceError> {
        if exchanges.len() > 1024 {
            return Err(ServiceError::invalid(
                "Replay exchange count exceeds its bound",
            ));
        }
        for exchange in &exchanges {
            exchange.validate()?;
        }
        Ok(Self {
            exchanges: exchanges.into(),
            decoder: StreamDecoder::new(),
            assembler: Reassembler::default(),
            now: Duration::ZERO,
        })
    }
    pub fn mode(&self) -> BackendMode {
        BackendMode::CaptureReplay
    }
    pub fn remaining(&self) -> usize {
        self.exchanges.len()
    }
    pub fn reset_stream(&mut self) {
        self.decoder.reset();
        self.assembler.reset();
    }
    pub fn receive(&mut self, bytes: &[u8], now: Duration) -> Result<Vec<Delivery>, ServiceError> {
        if now < self.now {
            return Err(ServiceError::invalid("Replay timestamp moved backwards"));
        }
        self.now = now;
        let mut deliveries = Vec::new();
        for event in self.decoder.feed(bytes) {
            match event {
                StreamEvent::Packet(packet) => {
                    if let Some(request) = self.assembler.add(&packet, now).map_err(|error| {
                        ServiceError::new(ErrorKind::Protocol, error.to_string())
                    })? {
                        let expected = self.exchanges.front().ok_or_else(|| {
                            ServiceError::unsupported("Replay has no remaining expected request")
                        })?;
                        if request != expected.expected_request {
                            let actual = request.payload();
                            let wanted = expected.expected_request.payload();
                            let differing = actual
                                .iter()
                                .zip(wanted)
                                .position(|(left, right)| left != right)
                                .unwrap_or(actual.len().min(wanted.len()));
                            return Err(ServiceError::new(
                                ErrorKind::Conflict,
                                format!(
                                    "Replay outgoing request mismatch: kind {:#x}/{:#x}, first payload difference at byte {differing}; fixture {}",
                                    request.kind(),
                                    expected.expected_request.kind(),
                                    expected.provenance
                                ),
                            ));
                        }
                        let expected = self.exchanges.pop_front().expect("front exists");
                        deliveries.extend(expected.response.into_iter().map(|chunk| {
                            Delivery::Bytes {
                                at: now + chunk.offset,
                                bytes: chunk.bytes,
                            }
                        }));
                    }
                }
                StreamEvent::Error(error) => {
                    return Err(ServiceError::new(ErrorKind::Protocol, error.to_string()));
                }
                StreamEvent::UnrecognizedSysex(_) => {
                    return Err(ServiceError::new(
                        ErrorKind::Protocol,
                        "Replay received unsupported SysEx",
                    ));
                }
                StreamEvent::Realtime(_) | StreamEvent::OtherMidi(_) => {}
            }
        }
        Ok(deliveries)
    }
}

/// Passive inspection releases incoming capture data according to caller time.
/// Every outgoing request, including edits, is rejected without consuming data.
pub struct PassiveReplay {
    chunks: VecDeque<CaptureChunk>,
}

pub fn replay_port() -> PortPairCandidate {
    PortPairCandidate {
        selection: DeviceSelection {
            input_id: "replay:input".into(),
            output_id: "replay:output".into(),
            fingerprint_hint: None,
        },
        display_name: "GP-200 capture replay".into(),
        mode_hint: Some(BackendMode::CaptureReplay),
        matching_confidence: "Fixture expectation replay, no physical MIDI".into(),
    }
}

/// Expectation replay through the production actor/backend boundary. Its
/// capability profile stays CaptureReplay and permits fixture-backed reads.
pub struct ExpectationReplayBackend {
    replay: Arc<Mutex<ExpectationReplay>>,
    origin: Instant,
    connected: Arc<AtomicBool>,
}
impl ExpectationReplayBackend {
    pub fn new(exchanges: Vec<CapturedExchange>) -> Result<Self, ServiceError> {
        Ok(Self {
            replay: Arc::new(Mutex::new(ExpectationReplay::new(exchanges)?)),
            origin: Instant::now(),
            connected: Arc::new(AtomicBool::new(false)),
        })
    }
}
impl TransportBackend for ExpectationReplayBackend {
    fn mode(&self) -> BackendMode {
        BackendMode::CaptureReplay
    }
    fn enumerate(&mut self) -> Result<Vec<PortPairCandidate>, ServiceError> {
        Ok(vec![replay_port()])
    }
    fn connect(
        &mut self,
        selection: &DeviceSelection,
        receive: ReceiveSink,
    ) -> Result<Box<dyn MidiSession>, ServiceError> {
        if selection != &replay_port().selection {
            return Err(ServiceError::invalid(
                "Replay opens only explicit replay ports",
            ));
        }
        if self.connected.swap(true, Ordering::AcqRel) {
            return Err(ServiceError::new(
                ErrorKind::Busy,
                "Replay session is already connected",
            ));
        }
        self.replay
            .lock()
            .map_err(|_| ServiceError::new(ErrorKind::Io, "Replay lock failed"))?
            .reset_stream();
        let (sender, cancelled) =
            crate::backend::delivery_worker(receive.clone(), self.origin, self.connected.clone())?;
        Ok(Box::new(ReplaySession {
            replay: self.replay.clone(),
            origin: self.origin,
            connected: self.connected.clone(),
            sender,
            cancelled,
            receive,
        }))
    }
}
struct ReplaySession {
    replay: Arc<Mutex<ExpectationReplay>>,
    origin: Instant,
    connected: Arc<AtomicBool>,
    sender: Sender<Delivery>,
    cancelled: ShutdownToken,
    receive: ReceiveSink,
}
impl MidiSession for ReplaySession {
    fn send(&mut self, bytes: &[u8]) -> Result<(), ServiceError> {
        self.health()?;
        let now = self.origin.elapsed();
        let deliveries = self
            .replay
            .lock()
            .map_err(|_| ServiceError::new(ErrorKind::Io, "Replay lock failed"))?
            .receive(bytes, now)?;
        for delivery in deliveries {
            match delivery {
                Delivery::Bytes { at, bytes } if at <= now => self.receive.receive(&bytes),
                delivery => self.sender.try_send(delivery).map_err(|_| {
                    ServiceError::new(ErrorKind::Busy, "Replay delivery queue is full or closed")
                })?,
            }
        }
        Ok(())
    }
    fn health(&mut self) -> Result<(), ServiceError> {
        if self.cancelled.cancelled() || !self.connected.load(Ordering::Acquire) {
            Err(ServiceError::new(
                ErrorKind::Disconnected,
                "Replay disconnected",
            ))
        } else {
            Ok(())
        }
    }
}
impl Drop for ReplaySession {
    fn drop(&mut self) {
        self.cancelled.cancel();
        self.connected.store(false, Ordering::Release);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayManifest {
    pub schema_version: u16,
    pub backend_mode: BackendMode,
    pub exchanges: Vec<ReplayExchangeManifest>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayExchangeManifest {
    pub request_kind: u8,
    pub request_payload_hex: String,
    pub response: Vec<ReplayChunkManifest>,
    pub response_sha256: String,
    pub source: String,
    pub synthetic_timing: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayChunkManifest {
    pub at_ms: u64,
    pub bytes_hex: String,
}
fn unhex(value: &str, maximum: usize) -> Result<Vec<u8>, ServiceError> {
    if value.len() > maximum * 2
        || !value.len().is_multiple_of(2)
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ServiceError::invalid(
            "Replay hex byte field is malformed or exceeds its bound",
        ));
    }
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let text = std::str::from_utf8(pair).expect("ASCII hex");
            u8::from_str_radix(text, 16).map_err(|error| ServiceError::invalid(error.to_string()))
        })
        .collect()
}
impl ReplayManifest {
    /// Bound bytes before JSON allocation; reject missing hashes and undeclared
    /// mode rather than interpreting a replay as a physical-device profile.
    pub fn parse(bytes: &[u8]) -> Result<Self, ServiceError> {
        if bytes.len() > 8 * 1024 * 1024 {
            return Err(ServiceError::invalid(
                "Replay manifest exceeds its 8 MiB limit",
            ));
        }
        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|error| ServiceError::invalid(error.to_string()))?;
        manifest.exchanges()?;
        Ok(manifest)
    }
    pub fn exchanges(&self) -> Result<Vec<CapturedExchange>, ServiceError> {
        if self.schema_version != 1
            || self.backend_mode != BackendMode::CaptureReplay
            || self.exchanges.len() > 1024
        {
            return Err(ServiceError::invalid(
                "Replay schema, mode or exchange count is unsupported",
            ));
        }
        let mut result = Vec::new();
        let mut total = 0usize;
        for exchange in &self.exchanges {
            if exchange.response.len() > 4096 {
                return Err(ServiceError::invalid(
                    "Replay callback count exceeds its bound",
                ));
            }
            let payload = unhex(
                &exchange.request_payload_hex,
                gp200_protocol::MAX_TRANSFER_LENGTH,
            )?;
            let request = RawTransfer::from_payload(exchange.request_kind, payload)
                .map_err(|error| ServiceError::invalid(error.to_string()))?;
            let mut response = Vec::new();
            let mut hash = Sha256::new();
            for chunk in &exchange.response {
                let bytes = unhex(&chunk.bytes_hex, 4 * 1024 * 1024)?;
                total = total
                    .checked_add(bytes.len())
                    .ok_or_else(|| ServiceError::invalid("Replay byte count overflow"))?;
                if total > 4 * 1024 * 1024 {
                    return Err(ServiceError::invalid(
                        "Replay responses exceed their combined 4 MiB limit",
                    ));
                }
                hash.update(&bytes);
                response.push(CaptureChunk {
                    offset: Duration::from_millis(chunk.at_ms),
                    bytes,
                });
            }
            if exchange.response_sha256 != format!("{:x}", hash.finalize()) {
                return Err(ServiceError::invalid("Replay response SHA-256 mismatch"));
            }
            let captured = CapturedExchange {
                expected_request: request,
                response,
                provenance: exchange.source.clone(),
                synthetic_timing: exchange.synthetic_timing,
            };
            captured.validate()?;
            result.push(captured);
        }
        Ok(result)
    }
}
impl PassiveReplay {
    pub fn new(chunks: Vec<CaptureChunk>) -> Result<Self, ServiceError> {
        CapturedExchange {
            expected_request: gp200_protocol::identify_request(),
            response: chunks.clone(),
            provenance: "passive capture".into(),
            synthetic_timing: false,
        }
        .validate()?;
        Ok(Self {
            chunks: chunks.into(),
        })
    }
    pub fn mode(&self) -> BackendMode {
        BackendMode::CaptureReplay
    }
    pub fn send(&self, _bytes: &[u8]) -> Result<(), ServiceError> {
        Err(ServiceError::new(
            ErrorKind::Unverified,
            "Passive capture replay does not transmit or manufacture edit success",
        ))
    }
    pub fn advance(&mut self, elapsed: Duration) -> Vec<Delivery> {
        let mut deliveries = Vec::new();
        while self
            .chunks
            .front()
            .is_some_and(|chunk| chunk.offset <= elapsed)
        {
            let chunk = self.chunks.pop_front().expect("due front exists");
            deliveries.push(Delivery::Bytes {
                at: chunk.offset,
                bytes: chunk.bytes,
            });
        }
        deliveries
    }
}
