//! Root-only reversible proof through the production Rust actor. Dry by default.
//! The probe owns `.hardware-session.lock`; do not wrap it in another flock.
use gp200_domain::*;
use gp200_protocol::{EqParameter, GeneralSetting, GlobalRecords, RawTransfer};
use gp200_transport::{MidiDeviceService, TransportConfig};
use std::{
    fs::{self, File},
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
    trace: File,
    tx: File,
    rx: File,
    phase: &'static str,
    write_attempted: bool,
}
impl Lab {
    fn stamp(&self, intent: u64) -> RequestStamp {
        RequestStamp {
            epoch: self.epoch,
            intent: IntentId(intent),
        }
    }
    fn record(&mut self, event: &DeviceEvent) -> Result<(), String> {
        writeln!(self.trace, "{event:?}").map_err(|e| e.to_string())?;
        match event {
            DeviceEvent::ConnectionChanged { epoch, .. } => self.epoch = *epoch,
            DeviceEvent::EditProgress {
                stage: EditStage::Sent | EditStage::Indeterminate(_),
                ..
            } => self.write_attempted = true,
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
            DeviceEvent::GlobalStateReceived { stamp, state } => {
                fs::write(
                    self.output
                        .join(format!("{}-globals.payload.bin", stamp.intent.0)),
                    state.records().raw().payload(),
                )
                .map_err(|e| e.to_string())?;
                self.globals = Some(state.clone());
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
        Err("Root global probe event deadline exceeded".into())
    }
    fn read_globals(&mut self, service: &MidiDeviceService, intent: u64) -> Result<(), String> {
        submit(service, DeviceCommand::ReadGlobals(self.stamp(intent)))?;
        self.wait(service,intent,|e|matches!(e,DeviceEvent::GlobalStateReceived{stamp,..} if stamp.intent==IntentId(intent)))
    }
    fn read_current(&mut self, service: &MidiDeviceService, intent: u64) -> Result<(), String> {
        submit(service, DeviceCommand::ReadCurrent(self.stamp(intent)))?;
        self.wait(service,intent,|e|matches!(e,DeviceEvent::SnapshotReceived{stamp,target:PresetTarget::EditBuffer,..} if stamp.intent==IntentId(intent)))
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
        self.wait(service,intent,|e|matches!(e,DeviceEvent::SnapshotReceived{stamp,target:PresetTarget::Stored(s),..} if stamp.intent==IntentId(intent)&&*s==slot))
    }
    fn edit(
        &mut self,
        service: &MidiDeviceService,
        intent: u64,
        patch: GlobalControlEdit,
    ) -> Result<(), String> {
        let revision = self
            .globals
            .as_ref()
            .ok_or("Missing global baseline")?
            .revision;
        submit(
            service,
            DeviceCommand::SubmitGlobalControlEdit {
                stamp: self.stamp(intent),
                base_revision: revision,
                patch,
            },
        )?;
        self.wait(service,intent,|e|matches!(e,DeviceEvent::EditProgress{stamp,stage:EditStage::Verified} if stamp.intent==IntentId(intent)))
    }
}
fn submit(service: &MidiDeviceService, command: DeviceCommand) -> Result<(), String> {
    service
        .try_submit(command)
        .map_err(|e| format!("Cannot submit: {e:?}"))
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
/// Named, recovered selectors only; never accepts a raw command/value pair.
fn general_field(profile: &str) -> Option<GeneralSetting> {
    Some(match profile {
        "general-input-level" => GeneralSetting::InputLevel,
        "general-input-mode" => GeneralSetting::InputMode,
        "general-no-cab-left" => GeneralSetting::NoCabLeft,
        "general-no-cab-right" => GeneralSetting::NoCabRight,
        "general-recording-level" => GeneralSetting::RecordingLevel,
        "general-recording-mode-left" => GeneralSetting::RecordingModeLeft,
        "general-recording-mode-right" => GeneralSetting::RecordingModeRight,
        "general-aux-to-usb" => GeneralSetting::AuxToUsb,
        "general-monitor-level" => GeneralSetting::MonitorLevel,
        "general-external-mode" => GeneralSetting::ExternalMode,
        "general-midi-clock-source" => GeneralSetting::MidiClockSource,
        "general-midi-input-usb" => GeneralSetting::MidiInputUsb,
        "general-midi-input-din" => GeneralSetting::MidiInputDin,
        "general-midi-output-usb" => GeneralSetting::MidiOutputUsb,
        "general-midi-output-din" => GeneralSetting::MidiOutputDin,
        "general-midi-input-source" => GeneralSetting::MidiInputSource,
        "general-midi-clock-out-usb" => GeneralSetting::MidiClockOutUsb,
        "general-midi-clock-out-din" => GeneralSetting::MidiClockOutDin,
        "general-footswitch-template" => GeneralSetting::FootswitchTemplate,
        "general-external-footswitch1" => GeneralSetting::ExternalFootswitch1,
        "general-external-footswitch2" => GeneralSetting::ExternalFootswitch2,
        "general-footswitch-mode" => GeneralSetting::FootswitchMode,
        "general-auto-cab-match" => GeneralSetting::AutoCabMatch,
        "general-display-mode" => GeneralSetting::DisplayMode,
        _ => return None,
    })
}
fn eq_field(profile: &str) -> Option<EqParameter> {
    EqParameter::from_index(profile.strip_prefix("eq-")?.parse().ok()?).ok()
}
fn edits(
    profile: &str,
    state: &DeviceGlobalState,
) -> Result<(GlobalControlEdit, GlobalControlEdit), String> {
    if let Some(field) = general_field(profile) {
        let old = state
            .records()
            .general
            .as_ref()
            .ok_or("No general baseline")?
            .value(field);
        field.validate(old).map_err(|e| e.to_string())?;
        // One adjacent native menu/level value; both endpoints must validate.
        let new = if field.validate(old + 1).is_ok() {
            old + 1
        } else {
            old - 1
        };
        field.validate(new).map_err(|e| e.to_string())?;
        return Ok((
            GlobalControlEdit::General { field, value: new },
            GlobalControlEdit::General { field, value: old },
        ));
    }
    if let Some(parameter) = eq_field(profile) {
        let old =
            state.records().eq.as_ref().ok_or("No EQ baseline")?.values[parameter.index() as usize];
        parameter
            .validate(old.as_f32())
            .map_err(|e| e.to_string())?;
        let step = if matches!(
            parameter,
            EqParameter::LowCutQ
                | EqParameter::LowShelfQ
                | EqParameter::Band1Q
                | EqParameter::Band2Q
                | EqParameter::Band3Q
                | EqParameter::Band4Q
                | EqParameter::HighShelfQ
                | EqParameter::HighCutQ
                | EqParameter::LowShelfGain
                | EqParameter::Band1Gain
                | EqParameter::Band2Gain
                | EqParameter::Band3Gain
                | EqParameter::Band4Gain
                | EqParameter::HighShelfGain
        ) {
            0.1
        } else {
            1.0
        };
        let new = if parameter.is_switch() {
            1.0 - old.as_f32()
        } else if parameter.validate(old.as_f32() + step).is_ok() {
            old.as_f32() + step
        } else {
            old.as_f32() - step
        };
        parameter.validate(new).map_err(|e| e.to_string())?;
        return Ok((
            GlobalControlEdit::Eq {
                parameter,
                value: F32Bits::from_f32(new),
            },
            GlobalControlEdit::Eq {
                parameter,
                value: old,
            },
        ));
    }
    Err("Choose baseline, general-<recovered field>, or eq-0..30".into())
}
fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let profile = args.next().unwrap_or_else(|| "baseline".into());
    if profile != "baseline" && general_field(&profile).is_none() && eq_field(&profile).is_none() {
        return Err("Choose baseline, general-<recovered field>, or eq-0..30".into());
    }
    let mut execute = false;
    let mut output = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--execute" => execute = true,
            "--output" => {
                output = Some(PathBuf::from(
                    args.next().ok_or("--output needs a fresh directory")?,
                ))
            }
            _ => return Err(format!("Unknown argument {arg}")),
        }
    }
    if !execute {
        println!(
            "Dry run: {profile}; production actor global preflight, exact field edit/readback/restoration, current/stored checks. No MIDI opened."
        );
        return Ok(());
    }
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(".hardware-session.lock")
        .map_err(|e| e.to_string())?;
    lock.try_lock()
        .map_err(|_| "Another root hardware session owns the pedal".to_owned())?;
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
        globals: None,
        trace: File::create(output.join("trace.txt")).map_err(|e| e.to_string())?,
        tx: File::create(output.join("tx.bin")).map_err(|e| e.to_string())?,
        rx: File::create(output.join("rx.bin")).map_err(|e| e.to_string())?,
        phase: "discovery",
        write_attempted: false,
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
        let selection = selection.ok_or("Exactly one GP-200 pair required")?;
        lab.phase = "identify";
        submit(
            &service,
            DeviceCommand::Connect {
                intent: IntentId(1),
                selection: selection.clone(),
            },
        )?;
        lab.wait(&service, 1, |e| {
            matches!(
                e,
                DeviceEvent::ConnectionChanged {
                    state: ConnectionState::Ready,
                    ..
                }
            )
        })?;
        let current_before = lab.current.clone().ok_or("Initial current missing")?;
        let context = current_before
            .context
            .filter(|c| c.group == 0)
            .ok_or("Current context required")?;
        let slot = SlotId::new(context.selector).map_err(|e| e.to_string())?;
        lab.phase = "stored-baseline";
        lab.read_stored(&service, 2, slot)?;
        let stored_before = lab.stored.clone().ok_or("Stored backup missing")?;
        lab.phase = "global-baseline";
        lab.read_globals(&service, 3)?;
        let before = lab.globals.clone().ok_or("Global baseline missing")?;
        if profile == "baseline" {
            fs::write(
                output.join("result.json"),
                b"{\"read_globals\":true,\"write_records_sent\":0}",
            )
            .map_err(|e| e.to_string())?;
            return Ok(());
        }
        let (patch, restore) = edits(&profile, &before)?;
        lab.phase = "global-edit";
        let edit_result = lab.edit(&service, 4, patch.clone());
        if edit_result.is_err() {
            lab.phase = "identify-for-restoration";
            submit(
                &service,
                DeviceCommand::Connect {
                    intent: IntentId(10),
                    selection,
                },
            )?;
            lab.wait(&service, 10, |e| {
                matches!(
                    e,
                    DeviceEvent::ConnectionChanged {
                        state: ConnectionState::Ready,
                        ..
                    }
                )
            })?;
            lab.read_globals(&service, 11)?;
        }
        let after = lab.globals.clone().ok_or("Global edit state missing")?;
        // The lab may reconnect after an uncertain write. Compare captured
        // complete records explicitly across epochs, never reuse an old stamp.
        let expected =
            GlobalRecords::from_transfer(before.expected_after(&patch).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?
                .control_state_equal(after.records());
        if !expected && !before.records().control_state_equal(after.records()) {
            return Err("Unexpected global changes; exact restoration requires inspection".into());
        }
        lab.phase = "pre-restoration-current";
        lab.read_current(&service, 12)?;
        if !lab.current.as_ref().is_some_and(|s| {
            s.context == Some(context) && preset_equal(&current_before.raw, &s.raw)
        }) {
            return Err(
                "Current preset changed before restoration; root inspection required".into(),
            );
        }
        lab.phase = "restore-global-field";
        let restore_result = lab.edit(&service, 13, restore);
        lab.phase = "restoration-readback";
        lab.read_stored(&service, 14, slot)?;
        lab.read_current(&service, 15)?;
        let restored = lab
            .globals
            .as_ref()
            .is_some_and(|s| before.records().control_state_equal(s.records()));
        let exact_raw_equal = lab
            .globals
            .as_ref()
            .is_some_and(|s| before.records().raw() == s.records().raw());
        let current_unchanged = lab
            .current
            .as_ref()
            .is_some_and(|s| preset_equal(&current_before.raw, &s.raw));
        let stored_unchanged = lab
            .stored
            .as_ref()
            .is_some_and(|s| preset_equal(&stored_before.raw, &s.raw));
        let report = serde_json::json!({"profile":profile,"edit_verified":edit_result.is_ok()&&expected,"restore_verified":restore_result.is_ok(),"control_state_restored":restored,"exact_raw_bundle_equal_after_restore":exact_raw_equal,"current_unchanged":current_unchanged,"stored_unchanged":stored_unchanged,"edit_error":edit_result.err(),"restore_error":restore_result.err(),"comparison_policy":"only captured GP-200 varying opaque1007:54/55;native-skipped1007:56,1002:6/7;cmdA:2/3 and after firstNUL"});
        fs::write(
            output.join("result.json"),
            serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        println!("{report}");
        if report["edit_verified"] != true
            || report["restore_verified"] != true
            || !restored
            || !current_unchanged
            || !stored_unchanged
        {
            return Err("Rust actor global proof or restoration failed".into());
        }
        Ok(())
    })();
    if let Err(error) = &result
        && !output.join("result.json").exists()
    {
        let report = serde_json::json!({"profile":profile,"terminal_failure":true,"phase":lab.phase,
            "write_attempted":lab.write_attempted,"error":error,
            "have_current_baseline":lab.current.is_some(),"have_stored_baseline":lab.stored.is_some(),
            "have_global_baseline":lab.globals.is_some()});
        fs::write(
            output.join("result.json"),
            serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        println!("{report}");
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn named_recipes_validate_and_restore_exact_bits() {
        let state = DeviceGlobalState::from_transfer(
            Revision(1),
            SessionEpoch(1),
            RawTransfer::from_payload(
                gp200_protocol::WRITE_KIND,
                include_bytes!("../../../tests/fixtures/globals/baseline.payload.bin").to_vec(),
            )
            .unwrap(),
            vec![],
        )
        .unwrap();
        let manifest: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/globals/breadth/manifest.json"
        ))
        .unwrap();
        for entry in manifest["entries"].as_array().unwrap() {
            let profile = if entry["kind"] == "general" {
                format!("general-{}", entry["name"].as_str().unwrap())
            } else {
                entry["name"].as_str().unwrap().to_owned()
            };
            let (patch, restore) = edits(&profile, &state).unwrap();
            assert!(gp200_transport::hardware_global_verified(&patch, &state));
            let after = DeviceGlobalState::from_transfer(
                Revision(2),
                state.epoch,
                state.expected_after(&patch).unwrap(),
                vec![],
            )
            .unwrap();
            let restored =
                GlobalRecords::from_transfer(after.expected_after(&restore).unwrap()).unwrap();
            assert!(state.records().control_state_equal(&restored));
        }
        assert!(eq_field("eq-31").is_none());
        assert!(general_field("general-reset").is_none());
    }
}
