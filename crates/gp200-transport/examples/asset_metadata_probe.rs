//! Root-operated read-only production actor proof. Dry by default; never sends
//! asset writes. It takes the shared hardware lock before creating a service.
use gp200_domain::*;
use gp200_transport::{MidiDeviceService, TransportConfig};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

struct Lab {
    output: PathBuf,
    epoch: SessionEpoch,
    current: Option<Arc<PresetSnapshot>>,
    stored: Option<Arc<PresetSnapshot>>,
    globals: Option<Arc<DeviceGlobalState>>,
    records: Vec<Arc<DeviceAssetMetadata>>,
    trace: File,
    tx: File,
    rx: File,
}
impl Lab {
    fn stamp(&self, intent: u64) -> RequestStamp {
        RequestStamp {
            epoch: self.epoch,
            intent: IntentId(intent),
        }
    }
    fn record(&mut self, event: &DeviceEvent) -> Result<(), String> {
        writeln!(self.trace, "{event:?}").map_err(|error| error.to_string())?;
        match event {
            DeviceEvent::ConnectionChanged { epoch, .. } => self.epoch = *epoch,
            DeviceEvent::SnapshotReceived {
                stamp,
                target,
                snapshot,
            } => {
                let label = if *target == PresetTarget::EditBuffer {
                    "current"
                } else {
                    "stored"
                };
                fs::write(
                    self.output
                        .join(format!("{}-{label}.payload.bin", stamp.intent.0)),
                    snapshot.raw.payload(),
                )
                .map_err(|error| error.to_string())?;
                if *target == PresetTarget::EditBuffer {
                    self.current = Some(snapshot.clone());
                } else {
                    self.stored = Some(snapshot.clone());
                }
            }
            DeviceEvent::GlobalStateReceived { stamp, state } => {
                fs::write(
                    self.output
                        .join(format!("{}-globals.payload.bin", stamp.intent.0)),
                    state.records().raw().payload(),
                )
                .map_err(|error| error.to_string())?;
                self.globals = Some(state.clone());
            }
            DeviceEvent::AssetMetadataReceived { stamp, metadata } => {
                fs::write(
                    self.output.join(format!(
                        "{}-ir-slot-{}.payload.bin",
                        stamp.intent.0,
                        metadata.slot().index()
                    )),
                    metadata.raw().payload(),
                )
                .map_err(|error| error.to_string())?;
                self.records.push(metadata.clone());
            }
            DeviceEvent::Diagnostic(event)
                if matches!(event.category.as_str(), "midi-tx" | "midi-rx") =>
            {
                if let Some(bytes) = &event.bytes {
                    (if event.category == "midi-tx" {
                        &mut self.tx
                    } else {
                        &mut self.rx
                    })
                    .write_all(bytes)
                    .map_err(|error| error.to_string())?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn wait(
        &mut self,
        service: &MidiDeviceService,
        intent: u64,
        mut predicate: impl FnMut(&DeviceEvent) -> bool,
    ) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if let Some(event) = service.try_next_event() {
                self.record(&event)?;
                if let DeviceEvent::OperationFailed { stamp, error } = &event
                    && stamp.intent == IntentId(intent)
                {
                    return Err(format!("{:?}: {}", error.kind, error.message));
                }
                if predicate(&event) {
                    return Ok(());
                }
            } else {
                thread::sleep(Duration::from_millis(2));
            }
        }
        Err("Root metadata probe event deadline exceeded".into())
    }
    fn read_current(&mut self, service: &MidiDeviceService, intent: u64) -> Result<(), String> {
        submit(service, DeviceCommand::ReadCurrent(self.stamp(intent)))?;
        self.wait(service, intent, |event| matches!(event, DeviceEvent::SnapshotReceived { stamp, target: PresetTarget::EditBuffer, .. } if stamp.intent == IntentId(intent)))
    }
    fn read_stored(
        &mut self,
        service: &MidiDeviceService,
        intent: u64,
        slot: SlotId,
    ) -> Result<(), String> {
        submit(
            service,
            DeviceCommand::ReadStored {
                stamp: self.stamp(intent),
                slot,
            },
        )?;
        self.wait(service, intent, |event| matches!(event, DeviceEvent::SnapshotReceived { stamp, target: PresetTarget::Stored(actual), .. } if stamp.intent == IntentId(intent) && *actual == slot))
    }
    fn read_globals(&mut self, service: &MidiDeviceService, intent: u64) -> Result<(), String> {
        submit(service, DeviceCommand::ReadGlobals(self.stamp(intent)))?;
        self.wait(service, intent, |event| matches!(event, DeviceEvent::GlobalStateReceived { stamp, .. } if stamp.intent == IntentId(intent)))
    }
}
fn submit(service: &MidiDeviceService, command: DeviceCommand) -> Result<(), String> {
    service
        .try_submit(command)
        .map_err(|error| format!("Cannot submit: {error:?}"))
}
fn preset_equal(a: &RawTransfer, b: &RawTransfer) -> bool {
    a.records().len() == b.records().len()
        && a.records().iter().zip(b.records()).all(|(a, b)| {
            a.command == b.command
                && a.body.len() == b.body.len()
                && a.body
                    .iter()
                    .zip(&b.body)
                    .enumerate()
                    .all(|(i, (a_byte, b_byte))| {
                        a_byte == b_byte || matches!((a.command, i), (2 | 8, 15) | (0x0f, 2 | 3))
                    })
        })
}
fn run() -> Result<(), String> {
    let mut execute = false;
    let mut all = false;
    let mut index = 0;
    let mut output = None;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--execute-read-only" => execute = true,
            "--all" => all = true,
            "--slot" => {
                index = args
                    .next()
                    .ok_or("--slot requires 0..19")?
                    .parse::<u16>()
                    .map_err(|_| "--slot requires 0..19")?
            }
            "--output" => {
                output = Some(PathBuf::from(
                    args.next().ok_or("--output needs a fresh directory")?,
                ))
            }
            _ => return Err(format!("Unknown argument {argument}")),
        }
    }
    AssetSlot::new(AssetBank::Ir, index).map_err(|error| error.to_string())?;
    if !execute {
        println!(
            "Dry run: read-only IR metadata {}, exact slot matching, current/stored/global before/after checks; no MIDI opened. Root only: --execute-read-only --output FRESH_FOLDER.",
            if all {
                "bank".into()
            } else {
                format!("slot {index}")
            }
        );
        return Ok(());
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(".hardware-session.lock")
        .map_err(|error| error.to_string())?;
    lock.try_lock()
        .map_err(|error| format!("Another process owns the hardware session: {error}"))?;
    let output = output.ok_or("--execute-read-only requires --output")?;
    if output.exists() {
        return Err("Evidence directory must be fresh".into());
    }
    fs::create_dir_all(&output).map_err(|error| error.to_string())?;
    let mut lab = Lab {
        output: output.clone(),
        epoch: SessionEpoch(0),
        current: None,
        stored: None,
        globals: None,
        records: Vec::new(),
        trace: File::create(output.join("trace.txt")).map_err(|error| error.to_string())?,
        tx: File::create(output.join("tx.bin")).map_err(|error| error.to_string())?,
        rx: File::create(output.join("rx.bin")).map_err(|error| error.to_string())?,
    };
    let service = MidiDeviceService::with_backend(
        Box::new(gp200_transport::AlsaRawBackend::new()),
        TransportConfig {
            trace_bytes: true,
            ..TransportConfig::alsa_raw()
        },
        no_wake(),
    );
    let result = (|| {
        submit(&service, DeviceCommand::Discover)?;
        let mut selection = None;
        lab.wait(&service, 0, |event| {
            if let DeviceEvent::PortsChanged(ports) = event {
                let matches: Vec<_> = ports
                    .iter()
                    .filter(|port| port.display_name.to_uppercase().contains("GP-200"))
                    .collect();
                if matches.len() == 1 {
                    selection = Some(matches[0].selection.clone());
                }
                true
            } else {
                false
            }
        })?;
        submit(
            &service,
            DeviceCommand::Connect {
                intent: IntentId(1),
                selection: selection.ok_or("Exactly one GP-200 MIDI pair required")?,
            },
        )?;
        lab.wait(&service, 1, |event| {
            matches!(
                event,
                DeviceEvent::ConnectionChanged {
                    state: ConnectionState::Ready,
                    ..
                }
            )
        })?;
        let before = lab
            .current
            .clone()
            .ok_or("No synchronized current-buffer baseline")?;
        let context = before
            .context
            .filter(|context| context.group == 0 && context.selector <= 255)
            .ok_or("Invalid current-buffer context")?;
        let stored_slot = SlotId::new(context.selector).map_err(|error| error.to_string())?;
        lab.read_stored(&service, 2, stored_slot)?;
        let stored = lab.stored.clone().ok_or("No stored baseline")?;
        lab.read_globals(&service, 3)?;
        let globals = lab.globals.clone().ok_or("No global baseline")?;
        let indices: Vec<_> = if all { (0..20).collect() } else { vec![index] };
        for index in indices {
            let intent = 100 + u64::from(index);
            let slot = AssetSlot::new(AssetBank::Ir, index).map_err(|error| error.to_string())?;
            submit(
                &service,
                DeviceCommand::ReadAssetMetadata {
                    stamp: lab.stamp(intent),
                    slot,
                },
            )?;
            lab.wait(&service, intent, |event| matches!(event, DeviceEvent::AssetMetadataReceived { stamp, metadata } if stamp.intent == IntentId(intent) && metadata.slot() == slot))?;
        }
        lab.read_stored(&service, 4, stored_slot)?;
        lab.read_current(&service, 5)?;
        lab.read_globals(&service, 6)?;
        let current_unchanged = lab.current.as_ref().is_some_and(|actual| {
            actual.context == Some(context) && preset_equal(&before.raw, &actual.raw)
        });
        let stored_unchanged = lab
            .stored
            .as_ref()
            .is_some_and(|actual| preset_equal(&stored.raw, &actual.raw));
        let globals_unchanged = lab
            .globals
            .as_ref()
            .is_some_and(|actual| globals.records().control_state_equal(actual.records()));
        let report = serde_json::json!({"classification":"production-rust-actor-read-only-hardware-observation","bank":"ir","records_read":lab.records.len(),"write_records_sent":0,"current_unchanged":current_unchanged,"stored_unchanged":stored_unchanged,"global_control_state_unchanged":globals_unchanged,"metadata_only":true,"occupancy":"unknown","complete_content_backup":"unavailable","records":lab.records.iter().map(|metadata| serde_json::json!({"stamp":metadata.stamp(),"source":metadata.source(),"metadata":metadata.metadata()})).collect::<Vec<_>>()});
        fs::write(
            output.join("result.json"),
            serde_json::to_vec_pretty(&report).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        println!("{report}");
        if !current_unchanged || !stored_unchanged || !globals_unchanged {
            return Err(
                "Read-only probe found unexpected device state changes; root inspection required"
                    .into(),
            );
        }
        Ok(())
    })();
    if let Err(error) = &result
        && !output.join("result.json").exists()
    {
        fs::write(output.join("result.json"), serde_json::to_vec_pretty(&serde_json::json!({"classification":"production-rust-actor-read-only-hardware-observation","terminal_failure":true,"error":error,"write_records_sent":0,"records_read":lab.records.len()})).map_err(|error| error.to_string())?).map_err(|error| error.to_string())?;
    }
    service.shutdown();
    service.wait_stopped(Duration::from_secs(1));
    result
}
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
