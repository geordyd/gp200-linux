//! Root-operated default-GUI-service ownership proof. Dry by default. It issues
//! only production typed reads and holds a Ready session for 1..30 seconds.
use gp200_domain::*;
use gp200_transport::MidiDeviceService;
use serde_json::json;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

struct Probe {
    output: PathBuf,
    trace: File,
    current: Option<Arc<PresetSnapshot>>,
    stored: Option<Arc<PresetSnapshot>>,
    globals: Option<Arc<DeviceGlobalState>>,
    epoch: SessionEpoch,
    candidates: Vec<PortPairCandidate>,
}
impl Probe {
    fn record(&mut self, event: &DeviceEvent) -> Result<(), ServiceError> {
        writeln!(self.trace, "{event:?}").map_err(io)?;
        match event {
            DeviceEvent::PortsChanged(ports) => self.candidates = ports.clone(),
            DeviceEvent::ConnectionChanged { epoch, .. } => self.epoch = *epoch,
            DeviceEvent::SnapshotReceived {
                target: PresetTarget::EditBuffer,
                snapshot,
                ..
            } => self.current = Some(snapshot.clone()),
            DeviceEvent::SnapshotReceived {
                target: PresetTarget::Stored(_),
                snapshot,
                ..
            } => self.stored = Some(snapshot.clone()),
            DeviceEvent::GlobalStateReceived { state, .. } => self.globals = Some(state.clone()),
            _ => {}
        }
        Ok(())
    }
    fn wait(
        &mut self,
        service: &MidiDeviceService,
        accepts: impl Fn(&DeviceEvent) -> bool,
    ) -> Result<(), ServiceError> {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Some(event) = service.try_next_event() {
                self.record(&event)?;
                if let DeviceEvent::OperationFailed { error, .. } = &event {
                    return Err(error.clone());
                }
                if accepts(&event) {
                    return Ok(());
                }
            } else {
                thread::sleep(Duration::from_millis(2));
            }
        }
        Err(ServiceError::new(
            ErrorKind::Timeout,
            "Read-only ownership probe event deadline exceeded",
        ))
    }
    fn read(
        &mut self,
        service: &MidiDeviceService,
        phase: &str,
        intent: u64,
    ) -> Result<(), ServiceError> {
        let epoch = self.epoch;
        let stamp = |number| RequestStamp {
            epoch,
            intent: IntentId(number),
        };
        submit(service, DeviceCommand::ReadCurrent(stamp(intent)))?;
        self.wait(service, |event| matches!(event, DeviceEvent::SnapshotReceived { stamp, target: PresetTarget::EditBuffer, .. } if stamp.intent == IntentId(intent)))?;
        write_new(
            &self.output.join(format!("{phase}-current.payload.bin")),
            self.current.as_ref().expect("read current").raw.payload(),
        )?;
        submit(
            service,
            DeviceCommand::ReadStored {
                stamp: stamp(intent + 1),
                slot: SlotId::new(23).expect("fixed source"),
            },
        )?;
        self.wait(service, |event| matches!(event, DeviceEvent::SnapshotReceived { stamp, target: PresetTarget::Stored(_), .. } if stamp.intent == IntentId(intent + 1)))?;
        write_new(
            &self.output.join(format!("{phase}-stored-023.payload.bin")),
            self.stored.as_ref().expect("read stored").raw.payload(),
        )?;
        submit(service, DeviceCommand::ReadGlobals(stamp(intent + 2)))?;
        self.wait(service, |event| matches!(event, DeviceEvent::GlobalStateReceived { stamp, .. } if stamp.intent == IntentId(intent + 2)))?;
        write_new(
            &self.output.join(format!("{phase}-globals.payload.bin")),
            self.globals
                .as_ref()
                .expect("read globals")
                .records()
                .raw()
                .payload(),
        )?;
        Ok(())
    }
}
fn io(error: std::io::Error) -> ServiceError {
    ServiceError::new(ErrorKind::Io, error.to_string())
}
fn submit(service: &MidiDeviceService, command: DeviceCommand) -> Result<(), ServiceError> {
    service
        .try_submit(command)
        .map_err(|_| ServiceError::new(ErrorKind::Busy, "Probe service rejected command"))
}
fn write_new(path: &PathBuf, bytes: &[u8]) -> Result<(), ServiceError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(io)?;
    file.write_all(bytes).map_err(io)?;
    file.sync_all().map_err(io)
}
fn run() -> Result<(), ServiceError> {
    let mut execute = false;
    let mut output = None;
    let mut hold = 15_u64;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--execute-read-only") => execute = true,
            Some("--output") => {
                output =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        ServiceError::invalid("Missing output directory")
                    })?))
            }
            Some("--hold-seconds") => {
                hold = args
                    .next()
                    .and_then(|arg| arg.to_str().and_then(|text| text.parse().ok()))
                    .ok_or_else(|| ServiceError::invalid("Invalid hold duration"))?
            }
            _ => {
                return Err(ServiceError::invalid(
                    "Use --execute-read-only --output FRESH_ABSOLUTE_DIRECTORY --hold-seconds 1..30",
                ));
            }
        }
    }
    if !(1..=30).contains(&hold) {
        return Err(ServiceError::invalid("Hold duration must be 1..30 seconds"));
    }
    if !execute {
        println!(
            "Dry run: default GUI MidiDeviceService, typed current/stored-023/global reads, bounded ownership hold and clean shutdown. No MIDI opened. Root only: --execute-read-only --output FRESH_ABSOLUTE_DIRECTORY --hold-seconds 1..30."
        );
        return Ok(());
    }
    let output = output.filter(|path| path.is_absolute()).ok_or_else(|| {
        ServiceError::invalid("Execution requires a fresh absolute output directory")
    })?;
    // The new backend guard intentionally never acquires this lab lock. This
    // probe owns its cwd lock; do not wrap it in an outer flock on that inode.
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(".hardware-session.lock")
        .map_err(io)?;
    if !lock.metadata().map_err(io)?.is_file() {
        return Err(ServiceError::invalid("Unsafe lab lock"));
    }
    lock.try_lock().map_err(|_| {
        ServiceError::new(
            ErrorKind::Busy,
            "Another process owns the workspace lab session",
        )
    })?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&output)
        .map_err(io)?;
    let trace = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(output.join("trace.txt"))
        .map_err(io)?;
    let mut probe = Probe {
        output: output.clone(),
        trace,
        current: None,
        stored: None,
        globals: None,
        epoch: SessionEpoch(0),
        candidates: vec![],
    };
    // Exactly the ordinary production GUI constructor, with the same backend
    // and session lifetime. No simulator or probe-specific backend is inserted.
    let service = MidiDeviceService::new(no_wake());
    let result = (|| {
        submit(&service, DeviceCommand::Discover)?;
        probe.wait(&service, |event| {
            matches!(event, DeviceEvent::PortsChanged(_))
        })?;
        let mut candidates = probe
            .candidates
            .iter()
            .filter(|port| port.display_name.to_uppercase().contains("GP-200"));
        let selected = candidates
            .next()
            .ok_or_else(|| ServiceError::new(ErrorKind::Disconnected, "No GP-200 pair found"))?
            .selection
            .clone();
        if candidates.next().is_some() {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Exactly one GP-200 pair is required",
            ));
        }
        submit(
            &service,
            DeviceCommand::Connect {
                intent: IntentId(1),
                selection: selected,
            },
        )?;
        probe.wait(&service, |event| {
            matches!(
                event,
                DeviceEvent::ConnectionChanged {
                    state: ConnectionState::Ready,
                    ..
                }
            )
        })?;
        probe.read(&service, "before", 2)?;
        write_new(&output.join("ready.json"), &serde_json::to_vec_pretty(&json!({"state":"Ready","read_only":true,"hold_seconds":hold,"pid":std::process::id()})).expect("JSON"))?;
        let deadline = Instant::now() + Duration::from_secs(hold);
        while Instant::now() < deadline {
            while let Some(event) = service.try_next_event() {
                probe.record(&event)?;
                if let DeviceEvent::OperationFailed { error, .. } = event {
                    return Err(error);
                }
            }
            thread::sleep(Duration::from_millis(3));
        }
        probe.read(&service, "after", 10)?;
        submit(
            &service,
            DeviceCommand::Disconnect {
                intent: IntentId(20),
            },
        )?;
        probe.wait(&service, |event| {
            matches!(
                event,
                DeviceEvent::ConnectionChanged {
                    state: ConnectionState::Disconnected,
                    ..
                }
            )
        })?;
        Ok(())
    })();
    service.shutdown();
    let stopped = service.wait_stopped(Duration::from_secs(5));
    let report = json!({"schema":1,"read_only":true,"default_gui_service":true,"worker_stopped":stopped,"ok":result.is_ok(),"error":result.as_ref().err().map(|error| json!({"kind":format!("{:?}",error.kind),"message":error.message}))});
    write_new(
        &output.join("result.json"),
        &serde_json::to_vec_pretty(&report).expect("JSON"),
    )?;
    result?;
    if !stopped {
        return Err(ServiceError::new(
            ErrorKind::Timeout,
            "Ownership probe worker did not stop",
        ));
    }
    println!("{report}");
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!(
            "{}",
            json!({"schema":1,"ok":false,"error":{"kind":format!("{:?}",error.kind),"message":error.message}})
        );
        std::process::exit(if error.kind == ErrorKind::Busy { 4 } else { 1 });
    }
}
