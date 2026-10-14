//! Research CLI. Read operations are the default. `edit-gain --execute` changes
//! one verified control, reads it back, then restores and verifies the baseline.
use gp200_domain::*;
use gp200_protocol::F32Bits;
use gp200_transport::{MidiDeviceService, MidirBackend, TransportBackend, TransportConfig};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

struct Options {
    command: String,
    pair: Option<usize>,
    slot: u16,
    value: f32,
    execute: bool,
    output: Option<PathBuf>,
    request_gap_ms: Option<u64>,
    active_discovery: bool,
    backend: String,
}
fn options() -> Result<Options, String> {
    let mut args = std::env::args().skip(1);
    let mut result = Options {
        command: args.next().unwrap_or_else(|| "current".into()),
        pair: None,
        slot: 23,
        value: 51.0,
        execute: false,
        output: None,
        request_gap_ms: None,
        active_discovery: true,
        backend: if cfg!(target_os = "linux") {
            "raw".into()
        } else {
            "midir".into()
        },
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--pair" => {
                result.pair = Some(
                    args.next()
                        .ok_or("--pair needs an index")?
                        .parse()
                        .map_err(|_| "Invalid pair index")?,
                )
            }
            "--slot" => {
                result.slot = args
                    .next()
                    .ok_or("--slot needs 0..255")?
                    .parse()
                    .map_err(|_| "Invalid slot")?
            }
            "--value" => {
                result.value = args
                    .next()
                    .ok_or("--value needs 0..100")?
                    .parse()
                    .map_err(|_| "Invalid value")?
            }
            "--execute" => result.execute = true,
            "--request-gap-ms" => {
                result.request_gap_ms = Some(
                    args.next()
                        .ok_or("--request-gap-ms needs milliseconds")?
                        .parse()
                        .map_err(|_| "Invalid request gap")?,
                )
            }
            "--no-active-discovery" => result.active_discovery = false,
            "--backend" => result.backend = args.next().ok_or("--backend needs midir or raw")?,
            "--output" => {
                result.output = Some(PathBuf::from(
                    args.next()
                        .ok_or("--output needs a new evidence directory")?,
                ))
            }
            _ => return Err(format!("Unknown option: {arg}")),
        }
    }
    if result.slot > 255 {
        return Err("Slot must be 0..255".into());
    }
    Ok(result)
}
struct Capture {
    output: Option<PathBuf>,
    trace: Option<File>,
    tx: Option<File>,
    rx: Option<File>,
    epoch: SessionEpoch,
    identity: Option<DeviceIdentity>,
    snapshots: BTreeMap<PresetTarget, Arc<PresetSnapshot>>,
}
impl Capture {
    fn new(output: Option<PathBuf>) -> Result<Self, String> {
        let mut result = Self {
            output,
            trace: None,
            tx: None,
            rx: None,
            epoch: SessionEpoch(0),
            identity: None,
            snapshots: BTreeMap::new(),
        };
        if let Some(path) = &result.output {
            fs::create_dir_all(path).map_err(|e| e.to_string())?;
            let create = |name| {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path.join(name))
                    .map_err(|e| format!("Evidence output must be fresh: {e}"))
            };
            result.trace = Some(create("trace.jsonl")?);
            result.tx = Some(create("tx.bin")?);
            result.rx = Some(create("rx.bin")?);
        }
        Ok(result)
    }
    fn record(&mut self, event: &DeviceEvent) -> Result<(), String> {
        match event {
            DeviceEvent::ConnectionChanged { epoch, state } => {
                self.epoch = *epoch;
                println!("Session {}: {state:?}", epoch.0);
            }
            DeviceEvent::SessionChanged {
                epoch, identity, ..
            } => {
                self.epoch = *epoch;
                self.identity = Some(identity.clone());
                println!("Detected {}; firmware unknown", identity.display_name);
            }
            DeviceEvent::SnapshotReceived {
                stamp,
                target,
                snapshot,
            } => {
                self.snapshots.insert(*target, snapshot.clone());
                println!(
                    "{target:?} revision {}: {}",
                    snapshot.revision.0,
                    snapshot.view.display_name()
                );
                if let Some(path) = &self.output {
                    let target_name = match target {
                        PresetTarget::EditBuffer => "current".into(),
                        PresetTarget::Stored(slot) => format!("stored-{:03}", slot.get()),
                    };
                    let name = format!("{}-{target_name}", stamp.intent.0);
                    fs::write(
                        path.join(format!("{name}.payload.bin")),
                        snapshot.raw.payload(),
                    )
                    .map_err(|e| e.to_string())?;
                    fs::write(
                        path.join(format!("{name}.json")),
                        serde_json::to_vec_pretty(snapshot).map_err(|e| e.to_string())?,
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
            DeviceEvent::Diagnostic(event)
                if event.category == "midi-tx" || event.category == "midi-rx" =>
            {
                if let Some(bytes) = &event.bytes {
                    let target = if event.category == "midi-tx" {
                        &mut self.tx
                    } else {
                        &mut self.rx
                    };
                    if let Some(file) = target {
                        file.write_all(bytes).map_err(|e| e.to_string())?;
                        file.flush().map_err(|e| e.to_string())?;
                    }
                }
            }
            DeviceEvent::EditProgress { stamp, stage } => {
                println!("Edit {}: {stage:?}", stamp.intent.0)
            }
            DeviceEvent::OperationFailed { stamp, error } => eprintln!(
                "Operation {}: {:?}: {}",
                stamp.intent.0, error.kind, error.message
            ),
            _ => {}
        }
        if let Some(file) = &mut self.trace {
            let line = match event {
                DeviceEvent::Diagnostic(event) => serde_json::json!({"diagnostic": event}),
                DeviceEvent::SnapshotReceived {
                    stamp,
                    target,
                    snapshot,
                } => {
                    serde_json::json!({"snapshot": {"stamp": stamp, "target": target, "revision": snapshot.revision}})
                }
                _ => serde_json::json!({"event": format!("{event:?}")}),
            };
            writeln!(file, "{line}").map_err(|e| e.to_string())?;
            file.flush().map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    fn wait(
        &mut self,
        service: &MidiDeviceService,
        intent: Option<IntentId>,
        predicate: impl Fn(&DeviceEvent) -> bool,
    ) -> Result<DeviceEvent, String> {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if let Some(event) = service.try_next_event() {
                self.record(&event)?;
                if let DeviceEvent::OperationFailed { stamp, error } = &event
                    && intent == Some(stamp.intent)
                {
                    return Err(format!("{:?}: {}", error.kind, error.message));
                }
                if predicate(&event) {
                    return Ok(event);
                }
            } else {
                thread::sleep(Duration::from_millis(2));
            }
        }
        Err("CLI event deadline exceeded".into())
    }
    fn stamp(&self, intent: u64) -> RequestStamp {
        RequestStamp {
            epoch: self.epoch,
            intent: IntentId(intent),
        }
    }
    fn result(&self, value: serde_json::Value) -> Result<(), String> {
        println!("{value}");
        if let Some(path) = &self.output {
            fs::write(
                path.join("result.json"),
                serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}
fn submit(service: &MidiDeviceService, command: DeviceCommand) -> Result<(), String> {
    service
        .try_submit(command)
        .map_err(|error| format!("Cannot submit command: {error:?}"))
}
fn connect(
    service: &MidiDeviceService,
    capture: &mut Capture,
    selection: DeviceSelection,
    intent: u64,
) -> Result<(), String> {
    submit(
        service,
        DeviceCommand::Connect {
            intent: IntentId(intent),
            selection,
        },
    )?;
    capture.wait(service, Some(IntentId(intent)), |event| {
        matches!(
            event,
            DeviceEvent::ConnectionChanged {
                state: ConnectionState::Ready,
                ..
            }
        )
    })?;
    Ok(())
}
fn set_gain(
    service: &MidiDeviceService,
    capture: &mut Capture,
    intent: u64,
    bits: F32Bits,
) -> Result<(), String> {
    let baseline = capture
        .snapshots
        .get(&PresetTarget::EditBuffer)
        .ok_or("Missing current buffer")?;
    submit(
        service,
        DeviceCommand::SubmitEdit {
            stamp: capture.stamp(intent),
            target: PresetTarget::EditBuffer,
            base_revision: baseline.revision,
            patch: EditPatch::SetParameter {
                module: ModuleId::new(3).unwrap(),
                model: ModelCode(0x0700_0015),
                model_index: ModelIndex(13),
                parameter: ParamId::new(0).unwrap(),
                value: bits,
            },
        },
    )?;
    capture.wait(service, Some(IntentId(intent)), |event| matches!(event, DeviceEvent::EditProgress { stamp, stage: EditStage::Verified } if stamp.intent == IntentId(intent)))?;
    Ok(())
}
fn current_gain(capture: &Capture) -> Result<F32Bits, String> {
    let snapshot = capture
        .snapshots
        .get(&PresetTarget::EditBuffer)
        .ok_or("No current snapshot")?;
    let amp = snapshot
        .view
        .module(ModuleId::new(3).unwrap())
        .ok_or("No AMP module")?;
    if amp.model != ModelCode(0x0700_0015) {
        return Err("This test is restricted to the currently selected Match CL amplifier".into());
    }
    amp.parameter(ParamId::new(0).unwrap())
        .ok_or_else(|| "No AMP Gain value".into())
}
fn run(options: Options) -> Result<(), String> {
    if matches!(options.command.as_str(), "help" | "--help" | "-h") {
        println!(
            "probe [list|identify|current|stored|bank|edit-gain] [--pair INDEX] [--slot 0..255] [--output NEW_DIR]\nLinux default: raw MIDI, 100 ms request spacing. Diagnostic options: --backend raw|midir, --request-gap-ms N, --no-active-discovery.\nedit-gain additionally requires --execute and accepts --value 0..100; it always restores the original Gain after readback."
        );
        return Ok(());
    }
    if !matches!(
        options.command.as_str(),
        "list" | "identify" | "current" | "stored" | "bank" | "edit-gain"
    ) {
        return Err("Unknown command; use help".into());
    }
    if options.command == "edit-gain" && !options.execute {
        return Err("No hardware change made: edit-gain requires --execute after reviewing the Gain profile; it will restore the baseline".into());
    }
    let mut capture = Capture::new(options.output)?;
    let profile = if options.backend == "raw" {
        TransportConfig::alsa_raw()
    } else {
        TransportConfig::default()
    };
    let config = TransportConfig {
        trace_bytes: capture.output.is_some(),
        minimum_request_spacing: options
            .request_gap_ms
            .map(Duration::from_millis)
            .unwrap_or(profile.minimum_request_spacing),
        poll_ports_while_connected: options.active_discovery,
        ..profile
    };
    let backend: Box<dyn TransportBackend> = match options.backend.as_str() {
        "midir" => Box::new(MidirBackend::new()),
        #[cfg(target_os = "linux")]
        "raw" => Box::new(gp200_transport::AlsaRawBackend::new()),
        _ => return Err("Unknown backend; choose midir or raw".into()),
    };
    let service = MidiDeviceService::with_backend(backend, config, no_wake());
    submit(&service, DeviceCommand::Discover)?;
    let DeviceEvent::PortsChanged(ports) = capture.wait(&service, None, |event| {
        matches!(event, DeviceEvent::PortsChanged(_))
    })?
    else {
        unreachable!()
    };
    for (index, pair) in ports.iter().enumerate() {
        println!(
            "[{index}] {} ({})\n  input: {}\n  output: {}",
            pair.display_name,
            pair.matching_confidence,
            pair.selection.input_id,
            pair.selection.output_id
        );
    }
    if options.command == "list" {
        service.shutdown();
        service.wait_stopped(Duration::from_secs(1));
        return Ok(());
    }
    let index = if let Some(index) = options.pair {
        index
    } else {
        let matches = ports
            .iter()
            .enumerate()
            .filter(|(_, pair)| pair.display_name.to_ascii_uppercase().contains("GP-200"))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(format!(
                "Found {} GP-200 candidate pairs; select one explicitly with --pair INDEX",
                matches.len()
            ));
        }
        matches[0]
    };
    let selection = ports
        .get(index)
        .ok_or("Pair index out of range")?
        .selection
        .clone();
    connect(&service, &mut capture, selection.clone(), 1)?;
    let result = match options.command.as_str() {
        "identify" | "current" => capture.result(serde_json::json!({"operation": options.command, "identity": capture.identity, "connected": true})),
        "stored" => {
            let slot = SlotId::new(options.slot).unwrap();
            submit(&service, DeviceCommand::ReadStored { stamp: capture.stamp(2), slot })?;
            capture.wait(&service, Some(IntentId(2)), |event| matches!(event, DeviceEvent::SnapshotReceived { target: PresetTarget::Stored(s), stamp, .. } if *s == slot && stamp.intent == IntentId(2)))?;
            capture.result(serde_json::json!({"operation": "stored", "slot": slot, "read_only": true}))
        }
        "bank" => {
            let start = Instant::now();
            for number in 0..256_u16 {
                let slot = SlotId::new(number).unwrap(); let intent = IntentId(number as u64 + 2);
                submit(&service, DeviceCommand::ReadStored { stamp: capture.stamp(intent.0), slot })?;
                capture.wait(&service, Some(intent), |event| matches!(event, DeviceEvent::SnapshotReceived { target: PresetTarget::Stored(s), stamp, .. } if *s == slot && stamp.intent == intent))?;
            }
            capture.result(serde_json::json!({"operation": "bank", "slots_read": 256, "elapsed_ms": start.elapsed().as_millis(), "read_only": true}))
        }
        "edit-gain" => {
            let original = current_gain(&capture)?;
            let original_context = capture.snapshots[&PresetTarget::EditBuffer].context;
            let requested = F32Bits::from_f32(options.value);
            let edited = set_gain(&service, &mut capture, 2, requested);
            // Even after an uncertain result, reconnect/read first. Restoration
            // is allowed only if the same buffer/model contains our exact value.
            let restored = if edited.is_ok() {
                set_gain(&service, &mut capture, 3, original)
            } else {
                connect(&service, &mut capture, selection, 10).and_then(|()| {
                    if capture.snapshots[&PresetTarget::EditBuffer].context != original_context {
                        return Err("Original buffer context changed; automatic restoration refused".into());
                    }
                    match current_gain(&capture)? {
                        value if value == original => Ok(()),
                        value if value == requested => set_gain(&service, &mut capture, 11, original),
                        _ => Err("Gain changed independently; automatic restoration refused".into()),
                    }
                })
            };
            capture.result(serde_json::json!({"operation": "edit-gain", "original_bits": original.0, "requested_bits": requested.0, "edit_verified": edited.is_ok(), "restored": restored.is_ok(), "edit_error": edited.as_ref().err(), "restore_error": restored.as_ref().err()}))?;
            edited.and(restored)
        }
        _ => unreachable!(),
    };
    service.shutdown();
    service.wait_stopped(Duration::from_secs(1));
    result
}
fn main() -> std::process::ExitCode {
    match options().and_then(run) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
