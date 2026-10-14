//! One packet write, bounded drain polling, original raw reader and exact errno.
use super::{
    firmware_linux_completion_admission::bad,
    firmware_linux_completion_binding::CompletionBindingWitness,
};
use crate::{
    device_session_guard::DeviceSessionGuard,
    firmware_backend::{FirmwareReaderRetirement, FirmwareReceiveSink},
};
use alsa::{Direction, rawmidi};
use gp200_domain::*;
use serde::Serialize;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CompletionSyscall {
    pub(crate) operation: String,
    pub(crate) requested: usize,
    pub(crate) returned_count: Option<usize>,
    pub(crate) errno: Option<i32>,
    pub(crate) before_sequence: u64,
    pub(crate) after_sequence: Option<u64>,
    pub(crate) before_ns: u64,
    pub(crate) after_ns: Option<u64>,
    pub(crate) after_clock_error: Option<String>,
    pub(crate) diagnostic: Option<String>,
}
pub(crate) fn syscall_completion_ms(call: &CompletionSyscall) -> Result<u64, ServiceError> {
    if call.after_clock_error.is_some() {
        return Err(bad(
            "Original native call completion clock failed; retained syscall cannot be converted to an engine receipt",
        ));
    }
    call.after_ns
        .map(|ns| ns / 1_000_000)
        .ok_or_else(|| bad("Original native call completion timestamp missing"))
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CompletionReadFault {
    pub(crate) operation: String,
    pub(crate) errno: Option<i32>,
    pub(crate) sequence: u64,
    pub(crate) at_ns: u64,
    pub(crate) before_sequence: u64,
    pub(crate) after_sequence: u64,
    pub(crate) diagnostic: String,
}
pub(crate) struct CompletionInvocation<'a> {
    pub(crate) deadline: Instant,
    pub(crate) stop: &'a AtomicBool,
    pub(crate) processed: u64,
}
impl CompletionInvocation<'_> {
    pub(crate) fn admit(&self, sink: &FirmwareReceiveSink) -> Result<(u64, u64), ServiceError> {
        let before = sink.now()?;
        if before.0 != self.processed
            || sink.fault.load(Ordering::SeqCst)
            || sink.first_failure().is_some()
            || self.stop.load(Ordering::SeqCst)
            || Instant::now() >= self.deadline
        {
            return Err(bad(
                "Native single-write final authority/frontier fence denied before invocation",
            ));
        }
        Ok(before)
    }
}
pub(crate) trait CompletionIo: Send {
    fn write_once(
        &mut self,
        wire: &[u8],
        invocation: &CompletionInvocation<'_>,
        on_invoked: &mut dyn FnMut() -> Result<(), ServiceError>,
    ) -> Result<CompletionSyscall, ServiceError>;
    fn drain_once(&mut self) -> Result<CompletionSyscall, ServiceError>;
    fn healthy(&mut self) -> Result<(), ServiceError>;
    fn close(&mut self) -> FirmwareReaderRetirement;
    fn read_fault(&self) -> Option<CompletionReadFault>;
}
pub(crate) struct CompletionOpenFailure {
    pub(crate) error: ServiceError,
    pub(crate) io: Option<Box<dyn CompletionIo>>,
}
pub(crate) struct CompletionRawIo {
    output: Option<rawmidi::Rawmidi>,
    reader: Option<CompletionCaptureReader>,
    sink: FirmwareReceiveSink,
    retired: Option<FirmwareReaderRetirement>,
    retired_fault: Option<CompletionReadFault>,
}
impl CompletionRawIo {
    pub(crate) fn open_entry(
        normal: &FirmwareUsbObservation,
        guard: &DeviceSessionGuard,
        sink: FirmwareReceiveSink,
    ) -> Result<Box<dyn CompletionIo>, CompletionOpenFailure> {
        if !guard.owns_normal_binding(normal) {
            return Err(CompletionOpenFailure {
                error: bad("Entry opener lacks actual retained normal guard"),
                io: None,
            });
        }
        Self::open(normal.endpoint.as_str(), sink)
    }
    pub(crate) fn open_fresh(
        witness: &CompletionBindingWitness,
        normal: &FirmwareUsbObservation,
        guard: &DeviceSessionGuard,
        sink: FirmwareReceiveSink,
    ) -> Result<Box<dyn CompletionIo>, CompletionOpenFailure> {
        if let Err(error) = witness.revalidate(normal, guard) {
            return Err(CompletionOpenFailure { error, io: None });
        }
        Self::open(witness.endpoint(), sink)
    }
    fn open(
        address: &str,
        sink: FirmwareReceiveSink,
    ) -> Result<Box<dyn CompletionIo>, CompletionOpenFailure> {
        let input = rawmidi::Rawmidi::new(address, Direction::Capture, true).map_err(|e| {
            CompletionOpenFailure {
                error: bad(format!("Firmware capture open errno {}: {e}", e.errno())),
                io: None,
            }
        })?;
        let reader = match CompletionCaptureReader::start(input, sink.clone()) {
            Ok(reader) => reader,
            Err(failed) => {
                return Err(CompletionOpenFailure {
                    error: failed.error,
                    io: failed.reader.map(|reader| {
                        Box::new(Self {
                            output: None,
                            reader: Some(reader),
                            sink,
                            retired: None,
                            retired_fault: None,
                        }) as Box<dyn CompletionIo>
                    }),
                });
            }
        };
        let mut io = Self {
            output: None,
            reader: Some(reader),
            sink,
            retired: None,
            retired_fault: None,
        };
        let output = (|| {
            let mut output = rawmidi::Rawmidi::new(address, Direction::Playback, true)
                .map_err(|e| bad(format!("Firmware output open errno {}: {e}", e.errno())))?;
            let mut params = output.params_current().map_err(bad)?;
            params.set_no_active_sensing(&output, true).map_err(bad)?;
            output.params(&params).map_err(bad)?;
            Ok(output)
        })();
        match output {
            Ok(output) => {
                io.output = Some(output);
                Ok(Box::new(io))
            }
            Err(error) => Err(CompletionOpenFailure {
                error,
                io: Some(Box::new(io)),
            }),
        }
    }
    fn syscall(
        &self,
        operation: &str,
        requested: usize,
        call: impl FnOnce(&rawmidi::Rawmidi) -> Result<usize, alsa::Error>,
    ) -> Result<CompletionSyscall, ServiceError> {
        let (before_sequence, before_ns) = self.sink.now()?;
        let result = call(
            self.output
                .as_ref()
                .ok_or_else(|| bad("Firmware output already closed"))?,
        );
        let (after_sequence, after_ns, after_clock_error) = match self.sink.now() {
            Ok((sequence, ns)) => (Some(sequence), Some(ns), None),
            Err(error) => (None, None, Some(error.message)),
        };
        let (returned_count, errno, diagnostic) = match result {
            Ok(n) => (Some(n), None, None),
            Err(e) => (None, Some(e.errno()), Some(e.to_string())),
        };
        Ok(CompletionSyscall {
            operation: operation.into(),
            requested,
            returned_count,
            errno,
            before_sequence,
            after_sequence,
            before_ns,
            after_ns,
            after_clock_error,
            diagnostic,
        })
    }
}
impl CompletionIo for CompletionRawIo {
    fn write_once(
        &mut self,
        wire: &[u8],
        invocation: &CompletionInvocation<'_>,
        on_invoked: &mut dyn FnMut() -> Result<(), ServiceError>,
    ) -> Result<CompletionSyscall, ServiceError> {
        let output = self
            .output
            .as_ref()
            .ok_or_else(|| bad("Firmware output already closed"))?;
        let (before_sequence, before_ns) = invocation.admit(&self.sink)?;
        on_invoked()?;
        let result = output.write(wire);
        let (after_sequence, after_ns, after_clock_error) = match self.sink.now() {
            Ok((sequence, ns)) => (Some(sequence), Some(ns), None),
            Err(error) => (None, None, Some(error.message)),
        };
        let (returned_count, errno, diagnostic) = match result {
            Ok(n) => (Some(n), None, None),
            Err(e) => (None, Some(e.errno()), Some(e.to_string())),
        };
        Ok(CompletionSyscall {
            operation: "write".into(),
            requested: wire.len(),
            returned_count,
            errno,
            before_sequence,
            after_sequence,
            before_ns,
            after_ns,
            after_clock_error,
            diagnostic,
        })
    }
    fn drain_once(&mut self) -> Result<CompletionSyscall, ServiceError> {
        self.syscall("drain", 0, |output| output.drain().map(|()| 0))
    }
    fn healthy(&mut self) -> Result<(), ServiceError> {
        if self
            .reader
            .as_ref()
            .is_some_and(CompletionCaptureReader::healthy)
            && !self.sink.fault.load(std::sync::atomic::Ordering::SeqCst)
        {
            Ok(())
        } else {
            Err(ServiceError::new(
                ErrorKind::Disconnected,
                "Completion reader ended or lost evidence",
            ))
        }
    }
    fn read_fault(&self) -> Option<CompletionReadFault> {
        self.reader
            .as_ref()
            .and_then(|r| r.fault.lock().ok().and_then(|f| f.clone()))
            .or_else(|| self.retired_fault.clone())
    }
    fn close(&mut self) -> FirmwareReaderRetirement {
        if let Some(receipt) = &self.retired {
            return receipt.repeat();
        }
        self.retired_fault = self.read_fault();
        let mut receipt = self
            .reader
            .as_mut()
            .expect("first close has reader")
            .close();
        self.retired_fault = self.read_fault();
        drop(self.reader.take());
        drop(self.output.take());
        receipt.output_destroyed = true;
        self.retired = Some(receipt.repeat());
        receipt
    }
}
impl Drop for CompletionRawIo {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
struct ReaderStartFailure {
    error: ServiceError,
    reader: Option<CompletionCaptureReader>,
}
struct CompletionCaptureReader {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    sink: FirmwareReceiveSink,
    fault: Arc<Mutex<Option<CompletionReadFault>>>,
}
impl CompletionCaptureReader {
    fn start(
        input: rawmidi::Rawmidi,
        sink: FirmwareReceiveSink,
    ) -> Result<Self, ReaderStartFailure> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let retained = sink.clone();
        let fault = Arc::new(Mutex::new(None));
        let thread_fault = fault.clone();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let thread = thread::Builder::new()
            .name("gp200-linux-completion-rx".into())
            .spawn(move || {
                let mut ready = Some(tx);
                let mut buffer = [0u8; 1024];
                while !flag.load(Ordering::Acquire) {
                    let mut primed = false;
                    let result = match input.read(&mut buffer) {
                        Ok(n) if n <= buffer.len() => {
                            primed = true;
                            if n > 0 {
                                sink.receive(&buffer[..n]);
                            }
                            Ok(())
                        }
                        Err(e) if e.errno() == 11 => {
                            primed = true;
                            Ok(())
                        }
                        Err(e) if e.errno() == 4 => Ok(()),
                        Err(e) => Err(("read", Some(e.errno()), e.to_string())),
                        Ok(_) => Err(("read", None, "Impossible ALSA read extent".into())),
                    }
                    .and_then(|()| match input.status() {
                        Ok(status) if status.get_xruns() == 0 => Ok(()),
                        Ok(_) => Err(("status", None, "ALSA input overrun".into())),
                        Err(e) => Err(("status", Some(e.errno()), e.to_string())),
                    })
                    .and_then(|()| {
                        if sink.fault.load(Ordering::SeqCst) {
                            Err((
                                "publication",
                                None,
                                "Raw publication/frontier was lost".into(),
                            ))
                        } else {
                            Ok(())
                        }
                    });
                    if let Err((operation, errno, diagnostic)) = result {
                        let before = sink.now();
                        sink.reader_fault();
                        let after = sink.now();
                        let observation = match (before, after) {
                            (Ok((before_sequence, _)), Ok((after_sequence, at_ns))) => {
                                CompletionReadFault {
                                    operation: operation.into(),
                                    errno,
                                    sequence: after_sequence,
                                    at_ns,
                                    before_sequence,
                                    after_sequence,
                                    diagnostic: diagnostic.clone(),
                                }
                            }
                            _ => CompletionReadFault {
                                operation: "clock-fault".into(),
                                errno: None,
                                sequence: 0,
                                at_ns: 0,
                                before_sequence: 0,
                                after_sequence: 0,
                                diagnostic: diagnostic.clone(),
                            },
                        };
                        if let Ok(mut fault) = thread_fault.lock() {
                            *fault = Some(observation);
                        }
                        if let Some(tx) = ready.take() {
                            let _ = tx.try_send(Err(bad(diagnostic)));
                        }
                        break;
                    }
                    if primed && let Some(tx) = ready.take() {
                        let _ = tx.try_send(Ok(()));
                    }
                    thread::sleep(Duration::from_millis(1));
                }
            })
            .map_err(|e| ReaderStartFailure {
                error: bad(e),
                reader: None,
            })?;
        let reader = Self {
            stop,
            thread: Some(thread),
            sink: retained,
            fault,
        };
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Ok(())) => Ok(reader),
            Ok(Err(error)) => Err(ReaderStartFailure {
                error,
                reader: Some(reader),
            }),
            Err(_) => Err(ReaderStartFailure {
                error: bad("Actual completion reader readiness not observed; no writer exposed"),
                reader: Some(reader),
            }),
        }
    }
    fn healthy(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
    fn close(&mut self) -> FirmwareReaderRetirement {
        let start = self.sink.now().ok().map(|(_, ns)| ns);
        self.stop.store(true, Ordering::Release);
        let join = match self.thread.take() {
            Some(t) => match t.join() {
                Ok(()) => FirmwareNormalReaderJoinObservation::Returned,
                Err(_) => FirmwareNormalReaderJoinObservation::Panicked,
            },
            None => FirmwareNormalReaderJoinObservation::Unavailable,
        };
        let fault = self.fault.lock().ok().and_then(|f| f.clone()).map(|f| {
            bad(format!(
                "Original {} errno {:?} at {}: {}",
                f.operation, f.errno, f.at_ns, f.diagnostic
            ))
        });
        self.sink
            .retirement(start, self.sink.now().ok().map(|(_, ns)| ns), join, fault)
    }
}
impl Drop for CompletionCaptureReader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
/// Complete frames retain original chunk positions and timestamp. No wire ACK key.
pub(crate) struct CompletionFrame {
    pub(crate) sequence: u64,
    pub(crate) first_chunk: u64,
    pub(crate) last_chunk: u64,
    pub(crate) at_ns: u64,
    pub(crate) raw: Vec<u8>,
}
pub(crate) struct CompletionFramer {
    stream: Vec<u8>,
    first_chunk: u64,
    first_ns: u64,
    next: u64,
}
impl CompletionFramer {
    pub(crate) fn new() -> Self {
        Self {
            stream: Vec::new(),
            first_chunk: 0,
            first_ns: 0,
            next: 1,
        }
    }
    pub(crate) fn pending(&self) -> bool {
        !self.stream.is_empty()
    }
    pub(crate) fn feed(
        &mut self,
        chunk: &FirmwareLifecycleRawObservation,
    ) -> Result<Vec<CompletionFrame>, ServiceError> {
        let mut frames = Vec::new();
        for &b in &chunk.raw {
            if b == 0xf0 {
                if !self.stream.is_empty() {
                    return Err(bad("Nested completion SysEx frame"));
                }
                self.first_chunk = chunk.sequence;
                self.first_ns = chunk.at_ns;
                self.stream.push(b);
            } else if self.stream.is_empty() || b > 127 && b != 0xf7 {
                return Err(bad("Unframed or unsupported completion raw byte"));
            } else {
                self.stream.push(b);
                if self.stream.len() > 4096 {
                    return Err(bad("Completion frame exceeded finite receive-size bound"));
                }
                if b == 0xf7 {
                    let sequence = self.next;
                    self.next = self
                        .next
                        .checked_add(1)
                        .ok_or_else(|| bad("Completion frame frontier exhausted"))?;
                    frames.push(CompletionFrame {
                        sequence,
                        first_chunk: self.first_chunk,
                        last_chunk: chunk.sequence,
                        at_ns: self.first_ns,
                        raw: std::mem::take(&mut self.stream),
                    });
                }
            }
        }
        Ok(frames)
    }
}
