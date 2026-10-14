//! Private nonblocking Linux I/O, gated by an unissuable physical permit.
use crate::{
    firmware_backend::*,
    firmware_profile::{AcceptedBootBinding, ExperimentalEntryPermit, PhysicalFirmwarePermit},
    raw_backend::{InputReadError, RawMidiInput},
};
use alsa::{Direction, rawmidi};
use gp200_domain::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
pub(crate) struct FirmwareRawFactory {
    pub(crate) address: String,
}
impl FirmwareRawFactory {
    pub(crate) fn open_entry(
        self,
        permit: &ExperimentalEntryPermit,
        binding: &NormalEntryBinding,
        sink: FirmwareReceiveSink,
    ) -> Result<Box<dyn FirmwareIo>, ServiceError> {
        if self.address != binding.endpoint() {
            return Err(ServiceError::invalid(
                "Entry factory is not the retained normal endpoint",
            ));
        }
        #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
        let audit = Some(
            permit
                .study()
                .open_audit(&permit.study().review.reservation.usb)?,
        );
        #[cfg(not(feature = "firmware-entry-study"))]
        let _ = permit;
        Self::open_bound(
            &self.address,
            sink,
            #[cfg(feature = "firmware-entry-study")]
            audit,
        )
    }
    pub(crate) fn open_transfer(
        _permit: &PhysicalFirmwarePermit,
        boot: &AcceptedBootBinding,
        sink: FirmwareReceiveSink,
    ) -> Result<Box<dyn FirmwareIo>, ServiceError> {
        // No old normal address is accepted here. The private accepted boot
        // witness is itself uninhabited until fresh boot/rebind review closes.
        if !sink.matches_boot(boot) {
            return Err(ServiceError::invalid(
                "Transfer sink lacks exact accepted fresh boot source",
            ));
        }
        Self::open_bound(
            &boot.binding.endpoint,
            sink,
            #[cfg(feature = "firmware-entry-study")]
            None,
        )
    }
    fn open_bound(
        address: &str,
        sink: FirmwareReceiveSink,
        #[cfg(feature = "firmware-entry-study")] entry_audit: Option<
            crate::firmware_entry_study::EntryAudit,
        >,
    ) -> Result<Box<dyn FirmwareIo>, ServiceError> {
        let input = rawmidi::Rawmidi::new(address, Direction::Capture, true).map_err(err)?;
        let reader = FirmwareRawReader::start(input, sink, Duration::from_secs(1))?;
        // Capture readiness precedes construction/exposure of writable output.
        let mut output = rawmidi::Rawmidi::new(address, Direction::Playback, true).map_err(err)?;
        let mut params = output.params_current().map_err(err)?;
        params.set_no_active_sensing(&output, true).map_err(err)?;
        output.params(&params).map_err(err)?;
        Ok(Box::new(FirmwareRawIo {
            output: Some(output),
            reader: Some(reader),
            retired: None,
            #[cfg(feature = "firmware-entry-study")]
            entry_audit,
            #[cfg(feature = "firmware-entry-study")]
            entry_calls: 0,
        }))
    }
}
pub(crate) struct FirmwareRawReader {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    sink: FirmwareReceiveSink,
    fault: Arc<std::sync::Mutex<Option<ServiceError>>>,
    retired: Option<FirmwareReaderRetirement>,
}
impl FirmwareRawReader {
    pub(crate) fn start(
        mut input: impl RawMidiInput,
        sink: FirmwareReceiveSink,
        timeout: Duration,
    ) -> Result<Self, ServiceError> {
        let retained_sink = sink.clone();
        let fault = Arc::new(std::sync::Mutex::new(None));
        let reader_fault = fault.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let thread = thread::Builder::new()
            .name("gp200-fw-raw-rx".into())
            .spawn(move || {
                let mut ready = Some(tx);
                let mut buffer = [0u8; 1024];
                while !flag.load(Ordering::Acquire) {
                    let mut primed = false;
                    let observed = match input.read(&mut buffer) {
                        Ok(n) if n <= buffer.len() => {
                            primed = true;
                            if n > 0 {
                                sink.receive(&buffer[..n]);
                            }
                            Ok(())
                        }
                        Err(InputReadError::WouldBlock) => {
                            primed = true;
                            Ok(())
                        }
                        Err(InputReadError::Interrupted) => Ok(()),
                        Err(InputReadError::Failed(e)) => Err(e),
                        Ok(_) => Err(ServiceError::new(
                            ErrorKind::Protocol,
                            "Firmware raw reader returned impossible length",
                        )),
                    }
                    .and_then(|()| input.check_status())
                    .and_then(|()| {
                        if sink.fault.load(Ordering::SeqCst) {
                            Err(ServiceError::new(
                                ErrorKind::Protocol,
                                "Firmware raw publication lost evidence",
                            ))
                        } else {
                            Ok(())
                        }
                    });
                    if let Err(error) = observed {
                        sink.reader_fault();
                        if let Ok(mut fault) = reader_fault.lock() {
                            *fault = Some(error.clone());
                        }
                        if let Some(tx) = ready.take() {
                            let _ = tx.try_send(Err(error));
                        }
                        break;
                    }
                    if primed && let Some(tx) = ready.take() {
                        let _ = tx.try_send(Ok(()));
                    }
                    thread::sleep(Duration::from_millis(1));
                }
            })
            .map_err(|e| ServiceError::new(ErrorKind::Io, e.to_string()))?;
        let reader = Self {
            stop,
            thread: Some(thread),
            sink: retained_sink,
            fault,
            retired: None,
        };
        match rx.recv_timeout(timeout) {
            Ok(Ok(())) => Ok(reader),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(ServiceError::new(
                ErrorKind::Timeout,
                "Firmware capture readiness not observed; no updater writer exposed",
            )),
        }
        // On error reader's destructor really joins before the factory returns.
    }
    pub(crate) fn close(&mut self) -> FirmwareReaderRetirement {
        if let Some(retired) = &self.retired {
            return retired.repeat();
        }
        let started = self.sink.now().ok().map(|(_, n)| n);
        self.stop.store(true, Ordering::Release);
        let join = match self.thread.take() {
            Some(t) => match t.join() {
                Ok(()) => FirmwareNormalReaderJoinObservation::Returned,
                Err(_) => FirmwareNormalReaderJoinObservation::Panicked,
            },
            None => FirmwareNormalReaderJoinObservation::Unavailable,
        };
        let fault = match self.fault.lock() {
            Ok(f) => f.clone(),
            Err(_) => Some(ServiceError::invalid("Updater reader fault state poisoned")),
        };
        let receipt =
            self.sink
                .retirement(started, self.sink.now().ok().map(|(_, n)| n), join, fault);
        self.retired = Some(receipt.repeat());
        receipt
    }
    fn healthy(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
    #[cfg(all(test, feature = "firmware-entry-study"))]
    pub(crate) fn live_for_entry_fixture(&self) -> bool {
        self.healthy()
    }
}
impl Drop for FirmwareRawReader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
struct FirmwareRawIo {
    #[cfg(feature = "firmware-entry-study")]
    entry_audit: Option<crate::firmware_entry_study::EntryAudit>,
    #[cfg(feature = "firmware-entry-study")]
    entry_calls: u64,
    output: Option<rawmidi::Rawmidi>,
    reader: Option<FirmwareRawReader>,
    retired: Option<FirmwareReaderRetirement>,
}
impl FirmwareIo for FirmwareRawIo {
    fn write_once(&mut self, b: &[u8]) -> WriteStep {
        #[cfg(feature = "firmware-entry-study")]
        let (result, timing) = if let Some(audit) = self.entry_audit.as_mut() {
            if self.entry_calls >= 2 {
                return WriteStep::Unknown(ServiceError::invalid("Entry syscall budget exhausted"));
            }
            if let Err(e) = audit.fresh_before_offer(self.entry_calls) {
                return WriteStep::Unknown(e);
            }
            let preflight = audit.now();
            if let Err(e) = audit.write_record(
                "entry-write-intent-before-syscall", b.len(),
                serde_json::json!({"ordinal":self.entry_calls,"wire":b,"actual_native_invocations":0}),
                preflight, preflight,
            ) {
                return WriteStep::Unknown(e);
            }
            let reader = self.reader.as_ref().expect("open firmware reader");
            let output = self.output.as_ref().expect("open firmware output");
            match audit.invoke_native(
                self.entry_calls,
                &reader.sink,
                || reader.healthy(),
                || output.write(b),
            ) {
                Ok((result, start, end)) => {
                    self.entry_calls += 1;
                    (result, Some((start, end)))
                }
                Err(error) => return WriteStep::Unknown(error),
            }
        } else {
            (
                self.output.as_ref().expect("open firmware output").write(b),
                None,
            )
        };
        #[cfg(not(feature = "firmware-entry-study"))]
        let result = self.output.as_ref().expect("open firmware output").write(b);
        #[cfg(feature = "firmware-entry-study")]
        if let (Some(audit), Some((start, end))) = (&self.entry_audit, timing) {
            let observation = match &result {
                Ok(n) => {
                    serde_json::json!({"returned_count":n,"errno":null,"actual_native_invocations":1})
                }
                Err(e) => {
                    serde_json::json!({"returned_count":null,"errno":e.errno(),"error":e.to_string(),"actual_native_invocations":1})
                }
            };
            if let Err(e) =
                audit.write_record("entry-write-returned", b.len(), observation, start, end)
            {
                return WriteStep::Unknown(e);
            }
        }
        match result {
            Ok(n) => WriteStep::Returned(n),
            Err(e) if e.errno() == 11 => WriteStep::WouldBlock,
            Err(e) if e.errno() == 4 => WriteStep::Interrupted,
            Err(e) => WriteStep::Unknown(err(e)),
        }
    }
    fn drain_once(&mut self) -> DrainStep {
        #[cfg(feature = "firmware-entry-study")]
        let start = self.entry_audit.as_ref().map(|audit| audit.now());
        let result = self.output.as_ref().expect("open firmware output").drain();
        #[cfg(feature = "firmware-entry-study")]
        if let (Some(audit), Some(start)) = (&self.entry_audit, start) {
            let observation = match &result {
                Ok(()) => serde_json::json!({"returned":true,"errno":null}),
                Err(e) => {
                    serde_json::json!({"returned":false,"errno":e.errno(),"error":e.to_string()})
                }
            };
            if let Err(e) =
                audit.write_record("entry-drain-returned", 0, observation, start, audit.now())
            {
                return DrainStep::Failed(e);
            }
        }
        match result {
            Ok(()) => DrainStep::Returned,
            Err(e) if e.errno() == 11 => DrainStep::WouldBlock,
            Err(e) => DrainStep::Failed(err(e)),
        }
    }
    fn health(&mut self) -> Result<(), ServiceError> {
        #[cfg(feature = "firmware-entry-study")]
        if let Some(audit) = &self.entry_audit {
            audit.healthy()?;
        }
        if self.reader.as_ref().is_some_and(FirmwareRawReader::healthy) {
            Ok(())
        } else {
            Err(ServiceError::new(
                ErrorKind::Disconnected,
                "Firmware raw reader stopped",
            ))
        }
    }
    fn ready_for(&self, sink: &FirmwareReceiveSink) -> bool {
        self.reader
            .as_ref()
            .is_some_and(|reader| reader.healthy() && reader.sink.same_source(sink))
    }
    fn close(&mut self) -> FirmwareReaderRetirement {
        if let Some(retired) = &self.retired {
            return retired.repeat();
        }
        let mut receipt = self
            .reader
            .as_mut()
            .expect("first updater close has reader")
            .close();
        drop(self.reader.take());
        drop(self.output.take());
        receipt.output_destroyed = true;
        #[cfg(feature = "firmware-entry-study")]
        if let Some(audit) = &mut self.entry_audit
            && let Err(e) = audit.close()
        {
            receipt.fault = Some(e);
        }
        self.retired = Some(receipt.repeat());
        receipt
    }
}
impl Drop for FirmwareRawIo {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
fn err(e: alsa::Error) -> ServiceError {
    ServiceError::new(ErrorKind::Io, format!("Firmware ALSA observation: {e}"))
}
