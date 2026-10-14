//! Root-only reversible Rust actor Sync proof; dry without --execute.
//! Profiles are the exact live-proven V-Roto and Tube pairs. No port opens in
//! dry mode. Root must wrap execution in `flock -n .hardware-session.lock`.
//! Before/current/stored captures are mandatory.
use gp200_domain::*;
use gp200_protocol::{F32Bits, RawTransfer};
use gp200_transport::{MidiDeviceService, TransportConfig};
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

struct Lab {
    output: PathBuf,
    epoch: SessionEpoch,
    current: Option<Arc<PresetSnapshot>>,
    stored: Option<Arc<PresetSnapshot>>,
    trace: File,
    tx: File,
    rx: File,
}
impl Lab {
    fn record(&mut self, event: &DeviceEvent) -> Result<(), String> {
        writeln!(self.trace, "{event:?}").map_err(|e| e.to_string())?;
        match event {
            DeviceEvent::ConnectionChanged { epoch, .. } => self.epoch = *epoch,
            DeviceEvent::SnapshotReceived {
                stamp,
                target,
                snapshot,
            } => {
                let label = match target {
                    PresetTarget::EditBuffer => "current",
                    PresetTarget::Stored(_) => "stored",
                };
                fs::write(
                    self.output
                        .join(format!("{}-{label}.payload.bin", stamp.intent.0)),
                    snapshot.raw.payload(),
                )
                .map_err(|e| e.to_string())?;
                match target {
                    PresetTarget::EditBuffer => self.current = Some(snapshot.clone()),
                    PresetTarget::Stored(_) => self.stored = Some(snapshot.clone()),
                }
            }
            DeviceEvent::Diagnostic(d) if matches!(d.category.as_str(), "midi-tx" | "midi-rx") => {
                if let Some(bytes) = &d.bytes {
                    let file = if d.category == "midi-tx" {
                        &mut self.tx
                    } else {
                        &mut self.rx
                    };
                    file.write_all(bytes).map_err(|e| e.to_string())?;
                }
            }
            _ => (),
        }
        Ok(())
    }
    fn stamp(&self, intent: u64) -> RequestStamp {
        RequestStamp {
            epoch: self.epoch,
            intent: IntentId(intent),
        }
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
        Err("Root probe event deadline exceeded".into())
    }
    fn sync(
        &mut self,
        service: &MidiDeviceService,
        intent: u64,
        profile: (u8, u32, u32, u8),
        enabled: bool,
        value: Option<f32>,
    ) -> Result<(), String> {
        let (module, code, index, power) = profile;
        let current = self.current.as_ref().ok_or("Missing current snapshot")?;
        submit(
            service,
            DeviceCommand::SubmitEdit {
                stamp: self.stamp(intent),
                target: PresetTarget::EditBuffer,
                base_revision: current.revision,
                patch: EditPatch::SetSync {
                    module: ModuleId::new(module).unwrap(),
                    model: ModelCode(code),
                    model_index: ModelIndex(index),
                    power_parameter: ParamId::new(power).unwrap(),
                    value_parameter: ParamId::new(1).unwrap(),
                    enabled,
                    ordinary_restore: value.map(|v| F32Bits(v.to_bits())),
                },
            },
        )?;
        self.wait(service, intent, |event| matches!(event, DeviceEvent::EditProgress { stamp, stage: EditStage::Verified } if stamp.intent == IntentId(intent)))
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
        self.wait(service, intent, |event| matches!(event, DeviceEvent::SnapshotReceived { stamp, target: PresetTarget::Stored(s), .. } if stamp.intent == IntentId(intent) && *s == slot))
    }
}
fn submit(service: &MidiDeviceService, command: DeviceCommand) -> Result<(), String> {
    service
        .try_submit(command)
        .map_err(|e| format!("Cannot submit: {e:?}"))
}
fn equivalent(a: &RawTransfer, b: &RawTransfer) -> bool {
    if a.records().len() != b.records().len() {
        return false;
    }
    a.records().iter().zip(b.records()).all(|(a, b)| {
        a.command == b.command
            && a.body.len() == b.body.len()
            && a.body
                .iter()
                .zip(&b.body)
                .enumerate()
                .all(|(offset, (a_byte, b_byte))| {
                    a_byte == b_byte || matches!((a.command, offset), (2 | 8, 15) | (0xf, 2 | 3))
                })
    })
}
fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let profile_name = args.next().unwrap_or_else(|| "mod-sync".into());
    let profile = match profile_name.as_str() {
        "mod-sync" => (7, 0x04000015, 9, 2),
        "dly-sync" => (8, 0x0b00000b, 12, 3),
        _ => return Err("Choose mod-sync or dly-sync".into()),
    };
    let mut execute = false;
    let mut output = None;
    let mut ordinary = if profile.0 == 7 { 0.7f32 } else { 501.0f32 };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--execute" => execute = true,
            "--output" => {
                output = Some(PathBuf::from(
                    args.next().ok_or("--output needs a fresh directory")?,
                ))
            }
            "--ordinary" => {
                ordinary = args
                    .next()
                    .ok_or("--ordinary needs a numeric value")?
                    .parse()
                    .map_err(|_| "Invalid ordinary value")?
            }
            _ => return Err(format!("Unknown argument {arg}")),
        }
    }
    if !execute {
        println!(
            "Dry run: {profile_name}; actor ordered Sync Off to explicit {ordinary}, then On to4, full restoration/readback. No MIDI opened."
        );
        return Ok(());
    }
    let output = output.ok_or("--execute requires --output")?;
    if output.exists() {
        return Err("Evidence directory must be fresh".into());
    }
    fs::create_dir_all(&output).map_err(|e| e.to_string())?;
    let mut lab = Lab {
        output: output.clone(),
        epoch: SessionEpoch(0),
        current: None,
        stored: None,
        trace: File::create(output.join("trace.txt")).map_err(|e| e.to_string())?,
        tx: File::create(output.join("tx.bin")).map_err(|e| e.to_string())?,
        rx: File::create(output.join("rx.bin")).map_err(|e| e.to_string())?,
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
                    .filter(|p| p.display_name.to_uppercase().contains("GP-200"))
                    .collect();
                if matches.len() == 1 {
                    selection = Some(matches[0].selection.clone());
                }
                true
            } else {
                false
            }
        })?;
        let selection = selection.ok_or("Exactly one GP-200 pair is required")?;
        submit(
            &service,
            DeviceCommand::Connect {
                intent: IntentId(1),
                selection: selection.clone(),
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
        let before = lab.current.clone().ok_or("Missing initial current read")?;
        let context = before
            .context
            .filter(|c| c.group == 0)
            .ok_or("Expected current context")?;
        let module = before
            .view
            .module(ModuleId::new(profile.0).unwrap())
            .ok_or("Module absent")?;
        if module.model != ModelCode(profile.1)
            || module.parameters[1] != F32Bits(4.0f32.to_bits())
            || module.parameters[usize::from(profile.3)] != F32Bits(1.0f32.to_bits())
        {
            return Err("Narrow proof requires current known model with Sync1/subdivision4".into());
        }
        let slot = SlotId::new(context.selector).map_err(|e| e.to_string())?;
        lab.read_stored(&service, 2, slot)?;
        let stored_before = lab.stored.clone().ok_or("Stored backup absent")?;
        let edit = lab.sync(&service, 3, profile, false, Some(ordinary));
        // After an uncertain pair, refresh through a new identified session;
        // never replay the failed write automatically.
        if edit.is_err() {
            submit(
                &service,
                DeviceCommand::Connect {
                    intent: IntentId(10),
                    selection,
                },
            )?;
            lab.wait(&service, 10, |event| {
                matches!(
                    event,
                    DeviceEvent::ConnectionChanged {
                        state: ConnectionState::Ready,
                        ..
                    }
                )
            })?;
        }
        let current = lab
            .current
            .as_ref()
            .ok_or("Current state absent before restoration")?;
        if current.context != Some(context)
            || current
                .view
                .module(ModuleId::new(profile.0).unwrap())
                .is_none_or(|m| m.model != ModelCode(profile.1))
        {
            return Err("Context/model changed; root must inspect before restoration".into());
        }
        let pair = current
            .view
            .module(ModuleId::new(profile.0).unwrap())
            .unwrap();
        let power_bits = pair.parameters[usize::from(profile.3)].0;
        let value_bits = pair.parameters[1].0;
        let fallback = if profile.0 == 7 { 0.5f32 } else { 500.0f32 };
        if !((power_bits == 0.0f32.to_bits()
            && [ordinary.to_bits(), fallback.to_bits()].contains(&value_bits))
            || (power_bits == 1.0f32.to_bits() && value_bits == 4.0f32.to_bits()))
        {
            return Err(
                "Control pair changed independently; root must inspect before restoration".into(),
            );
        }
        let mut comparison = current.raw.records().to_vec();
        let r = comparison
            .iter_mut()
            .find(|r| r.command == 0x14 && r.body.len() == 68 && r.body[0] == profile.0)
            .unwrap();
        let old = before
            .raw
            .records()
            .iter()
            .find(|r| r.command == 0x14 && r.body.len() == 68 && r.body[0] == profile.0)
            .unwrap();
        for id in [1, profile.3] {
            let offset = 8 + usize::from(id) * 4;
            r.body[offset..offset + 4].copy_from_slice(&old.body[offset..offset + 4]);
        }
        if !equivalent(
            &before.raw,
            &RawTransfer::from_records(current.raw.kind(), &comparison)
                .map_err(|e| e.to_string())?,
        ) {
            return Err(
                "Unrelated meaningful state changed; root must inspect before restoration".into(),
            );
        }
        let restore = lab.sync(&service, 11, profile, true, None);
        lab.read_stored(&service, 12, slot)?;
        submit(&service, DeviceCommand::ReadCurrent(lab.stamp(13)))?;
        lab.wait(&service, 13, |event| matches!(event, DeviceEvent::SnapshotReceived { stamp, target: PresetTarget::EditBuffer, .. } if stamp.intent == IntentId(13)))?;
        let restored = lab
            .current
            .as_ref()
            .is_some_and(|current| equivalent(&before.raw, &current.raw));
        let stored_unchanged = lab
            .stored
            .as_ref()
            .is_some_and(|stored| equivalent(&stored_before.raw, &stored.raw));
        let report = serde_json::json!({"profile": profile_name, "explicit_ordinary": ordinary,
            "edit_verified": edit.is_ok(), "restore_verified": restore.is_ok(), "restored": restored,
            "stored_unchanged": stored_unchanged, "edit_error": edit.err(), "restore_error": restore.err()});
        fs::write(
            output.join("result.json"),
            serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        println!("{report}");
        if report["edit_verified"] != true
            || report["restore_verified"] != true
            || !restored
            || !stored_unchanged
        {
            return Err("Rust actor control proof or complete restoration failed".into());
        }
        Ok(())
    })();
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
