//! Root-only reversible proof through the production Rust actor. Dry by default.
//! Root must wrap --execute in `flock -n .hardware-session.lock`.
use gp200_domain::*;
use gp200_protocol::{
    DrumSetting, EqParameter, GeneralSetting, GlobalFootswitchEvent, GlobalFootswitchFunction,
    GlobalFootswitchTableId, GlobalRecords, RawTransfer,
};
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
fn edits(
    profile: &str,
    state: &DeviceGlobalState,
) -> Result<(GlobalControlEdit, GlobalControlEdit), String> {
    let general = state
        .records()
        .general
        .as_ref()
        .ok_or("No general baseline")?;
    match profile {
        "display-mode" | "input-level" => {
            let field = if profile == "display-mode" {
                GeneralSetting::DisplayMode
            } else {
                GeneralSetting::InputLevel
            };
            let old = general.value(field);
            field.validate(old).map_err(|e| e.to_string())?;
            let new = if field == GeneralSetting::DisplayMode {
                (old + 1) % 3
            } else if old < 20 {
                old + 1
            } else {
                old - 1
            };
            Ok((
                GlobalControlEdit::General { field, value: new },
                GlobalControlEdit::General { field, value: old },
            ))
        }
        "eq-band-gain" => {
            let old = state.records().eq.as_ref().ok_or("No EQ baseline")?.values[18];
            if old.as_f32() != 0.0 {
                return Err("Narrow EQ proof requires Band3Gain0dB".into());
            }
            Ok((
                GlobalControlEdit::Eq {
                    parameter: EqParameter::Band3Gain,
                    value: F32Bits::from_f32(0.1),
                },
                GlobalControlEdit::Eq {
                    parameter: EqParameter::Band3Gain,
                    value: old,
                },
            ))
        }
        "stopped-drum-tempo" | "stopped-drum-volume" | "stopped-drum-pattern" => {
            if state
                .records()
                .runtime
                .as_ref()
                .and_then(|r| r.drum_playing())
                != Some(false)
            {
                return Err("Drum must be stopped".into());
            }
            let field = match profile {
                "stopped-drum-tempo" => DrumSetting::Tempo,
                "stopped-drum-volume" => DrumSetting::Volume,
                _ => DrumSetting::Pattern,
            };
            let old = state
                .records()
                .drum
                .as_ref()
                .ok_or("No drum baseline")?
                .value(field);
            field.validate(old).map_err(|e| e.to_string())?;
            if field == DrumSetting::Tempo && old != 120 {
                return Err("Narrow tempo proof requires120BPM".into());
            }
            let max = if field == DrumSetting::Pattern {
                99
            } else {
                100
            };
            let new = if field == DrumSetting::Tempo || old < max {
                old + 1
            } else {
                old - 1
            };
            Ok((
                GlobalControlEdit::Drum { field, value: new },
                GlobalControlEdit::Drum { field, value: old },
            ))
        }
        "inactive-footswitch" => {
            let table = GlobalFootswitchTableId::new(2).map_err(|e| e.to_string())?;
            let event = GlobalFootswitchEvent::new(19).map_err(|e| e.to_string())?;
            let mapping = state
                .records()
                .footswitch_table(table)
                .ok_or("User table missing")?;
            if !matches!(general.value(GeneralSetting::FootswitchMode), 0 | 1)
                || mapping.function_raw(event) != 0
            {
                return Err("Inactive User table2 event19 initially None required".into());
            }
            Ok((
                GlobalControlEdit::Footswitch {
                    table,
                    event,
                    function: GlobalFootswitchFunction::new(1).map_err(|e| e.to_string())?,
                },
                GlobalControlEdit::Footswitch {
                    table,
                    event,
                    function: GlobalFootswitchFunction::new(0).map_err(|e| e.to_string())?,
                },
            ))
        }
        _ => Err("Unknown mutation profile".into()),
    }
}
fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let profile = args.next().unwrap_or_else(|| "baseline".into());
    if ![
        "baseline",
        "display-mode",
        "input-level",
        "eq-band-gain",
        "stopped-drum-tempo",
        "stopped-drum-volume",
        "stopped-drum-pattern",
        "inactive-footswitch",
    ]
    .contains(&profile.as_str())
    {
        return Err("Choose a named global profile".into());
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
