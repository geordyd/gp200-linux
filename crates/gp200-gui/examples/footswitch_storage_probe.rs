//! Root-only reversible inactive-table storage proof through AppController and
//! the actual ALSA actor. Dry by default; this process owns the hardware lock.
//! Never trigger a footswitch while the probe is running. No mode changes,
//! preset writes, reset, flashing, or raw command/value arguments are accepted.
use gp200_app::AppController;
use gp200_catalog::Catalog;
use gp200_domain::*;
use gp200_protocol::{
    GlobalFootswitchEvent, GlobalFootswitchFunction, GlobalFootswitchTableId, GlobalRecords,
};
use gp200_transport::{MidiDeviceService, TransportConfig};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write,
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

fn profile(name: &str) -> Option<(u16, u8, u8)> {
    Some(match name {
        "table1-tap" => (1, 0, 17),
        "table1-hold" => (1, 1, 5),
        "table1-combined" => (1, 16, 23),
        "table2-tap" => (2, 0, 17),
        "table2-hold" => (2, 1, 5),
        "table2-combined" => (2, 16, 23),
        _ => return None,
    })
}
fn recipe(
    name: &str,
    before: &DeviceGlobalState,
) -> Result<(GlobalControlEdit, GlobalControlEdit), String> {
    let (table, event, representative) =
        profile(name).ok_or("Choose table1|table2-tap|hold|combined")?;
    let table = GlobalFootswitchTableId::new(table).map_err(|e| e.to_string())?;
    let event = GlobalFootswitchEvent::new(event).map_err(|e| e.to_string())?;
    let original = before
        .records()
        .footswitch_table(table)
        .ok_or("Complete table baseline required")?
        .function_raw(event);
    let function = if original == representative {
        0
    } else {
        representative
    };
    let edit = GlobalControlEdit::Footswitch {
        table,
        event,
        function: GlobalFootswitchFunction::new(function).map_err(|e| e.to_string())?,
    };
    ValidatedInactiveFootswitchEdit::validate(&edit, before).map_err(|e| e.to_string())?;
    let restore = GlobalControlEdit::Footswitch {
        table,
        event,
        function: GlobalFootswitchFunction::new(original).map_err(|e| e.to_string())?,
    };
    Ok((edit, restore))
}
fn preset_equal(a: &PresetSnapshot, b: &PresetSnapshot) -> bool {
    a.context == b.context
        && a.raw.records().len() == b.raw.records().len()
        && a.raw.records().iter().zip(b.raw.records()).all(|(a, b)| {
            a.command == b.command
                && a.body.len() == b.body.len()
                && a.body
                    .iter()
                    .zip(&b.body)
                    .enumerate()
                    .all(|(index, (left, right))| {
                        left == right || matches!((a.command, index), (2 | 8, 15) | (0x0f, 2 | 3))
                    })
        })
}
struct Lab {
    app: AppController,
    service: MidiDeviceService,
    output: PathBuf,
    phase: &'static str,
    sequence: u64,
    selection: Option<DeviceSelection>,
    snapshots: BTreeMap<PresetTarget, (u64, Arc<PresetSnapshot>)>,
    globals: Option<Arc<DeviceGlobalState>>,
    verified: bool,
    sent: bool,
    trace: File,
    tx: File,
    rx: File,
}
impl Lab {
    fn input(&mut self, input: AppInput) -> Result<(), String> {
        for effect in self.app.dispatch(input) {
            if let Effect::Device(command) = effect {
                self.service
                    .try_submit(command)
                    .map_err(|e| format!("Cannot submit: {e:?}"))?;
            } else {
                return Err("Unexpected non-device effect in mapping proof".into());
            }
        }
        Ok(())
    }
    fn wait(&mut self, predicate: impl Fn(&Self) -> bool) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Some(event) = self.service.try_next_event() {
                self.sequence += 1;
                writeln!(self.trace, "{} {event:?}", self.phase).map_err(|e| e.to_string())?;
                match &event {
                    DeviceEvent::PortsChanged(ports) => {
                        let ports: Vec<_> = ports
                            .iter()
                            .filter(|p| p.display_name.to_uppercase().contains("GP-200"))
                            .collect();
                        if ports.len() == 1 {
                            self.selection = Some(ports[0].selection.clone());
                        }
                    }
                    DeviceEvent::SnapshotReceived {
                        target, snapshot, ..
                    } => {
                        fs::write(
                            self.output.join(format!(
                                "{}-{}-preset.payload.bin",
                                self.phase, self.sequence
                            )),
                            snapshot.raw.payload(),
                        )
                        .map_err(|e| e.to_string())?;
                        self.snapshots
                            .insert(*target, (self.sequence, Arc::clone(snapshot)));
                    }
                    DeviceEvent::GlobalStateReceived { state, .. } => {
                        fs::write(
                            self.output.join(format!(
                                "{}-{}-globals.payload.bin",
                                self.phase, self.sequence
                            )),
                            state.records().raw().payload(),
                        )
                        .map_err(|e| e.to_string())?;
                        self.globals = Some(Arc::clone(state));
                    }
                    DeviceEvent::EditProgress {
                        stage: EditStage::Sent,
                        ..
                    } => self.sent = true,
                    DeviceEvent::EditProgress {
                        stage: EditStage::Verified,
                        ..
                    } => self.verified = true,
                    DeviceEvent::Diagnostic(d)
                        if matches!(d.category.as_str(), "midi-tx" | "midi-rx") =>
                    {
                        if let Some(bytes) = &d.bytes {
                            let file = if d.category == "midi-tx" {
                                &mut self.tx
                            } else {
                                &mut self.rx
                            };
                            file.write_all(bytes).map_err(|e| e.to_string())?;
                        }
                    }
                    _ => {}
                }
                let failure = match &event {
                    DeviceEvent::OperationFailed { error, .. }
                    | DeviceEvent::EditProgress {
                        stage: EditStage::Failed(error) | EditStage::Indeterminate(error),
                        ..
                    } => Some(format!("{:?}: {}", error.kind, error.message)),
                    _ => None,
                };
                self.input(AppInput::Device(event))?;
                if let Some(failure) = failure {
                    return Err(failure);
                }
                if predicate(self) {
                    return Ok(());
                }
            } else {
                thread::sleep(Duration::from_millis(2));
            }
        }
        Err(format!("{} event deadline exceeded", self.phase))
    }
    fn read_globals(&mut self) -> Result<Arc<DeviceGlobalState>, String> {
        let revision = self
            .app
            .state()
            .device_globals
            .as_ref()
            .map_or(Revision(0), |s| s.revision);
        self.input(AppInput::Ui(UiIntent::RefreshDeviceGlobals))?;
        self.wait(|lab| {
            !lab.app.device_operation_pending()
                && lab
                    .app
                    .state()
                    .device_globals
                    .as_ref()
                    .is_some_and(|s| s.revision > revision)
        })?;
        self.app
            .state()
            .device_globals
            .clone()
            .ok_or("Global readback missing".into())
    }
    fn read_preset(&mut self, target: PresetTarget) -> Result<Arc<PresetSnapshot>, String> {
        let sequence = self.sequence;
        self.input(AppInput::Ui(UiIntent::Refresh(target)))?;
        self.wait(|lab| {
            !lab.app.device_operation_pending()
                && lab
                    .snapshots
                    .get(&target)
                    .is_some_and(|(s, _)| *s > sequence)
        })?;
        self.snapshots
            .get(&target)
            .map(|(_, s)| Arc::clone(s))
            .ok_or("Preset readback missing".into())
    }
    fn edit(&mut self, patch: GlobalControlEdit) -> Result<(), String> {
        self.verified = false;
        self.sent = false;
        self.input(AppInput::Ui(UiIntent::ApplyGlobalControlEdit(patch)))?;
        self.wait(|lab| lab.verified && !lab.app.device_operation_pending())
    }
}
fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let name = args.next().unwrap_or_else(|| "table2-tap".into());
    profile(&name).ok_or(
        "Choose table1-tap|table1-hold|table1-combined|table2-tap|table2-hold|table2-combined",
    )?;
    let mut execute = false;
    let mut output = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--execute" => execute = true,
            "--output" => {
                output = Some(PathBuf::from(
                    args.next().ok_or("--output requires a fresh directory")?,
                ))
            }
            _ => return Err(format!("Unknown argument {arg}")),
        }
    }
    if !execute {
        println!(
            "Dry: {name}; inactive table only, complete fresh preflight, reducer + ALSA write/readback and exact baseline restoration. No MIDI opened."
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
        .map_err(|_| "Another hardware session owns the pedal")?;
    let output = output.ok_or("--execute requires --output")?;
    if output.exists() {
        return Err("Evidence directory must be fresh".into());
    }
    fs::create_dir_all(&output).map_err(|e| e.to_string())?;
    let mut lab = Lab {
        app: AppController::new(
            Catalog::embedded().map_err(|e| e.to_string())?,
            AppPreferences::default(),
        ),
        service: MidiDeviceService::with_backend(
            Box::new(gp200_transport::AlsaRawBackend::new()),
            TransportConfig {
                trace_bytes: true,
                ..TransportConfig::alsa_raw()
            },
            no_wake(),
        ),
        trace: File::create(output.join("trace.txt")).map_err(|e| e.to_string())?,
        tx: File::create(output.join("tx.bin")).map_err(|e| e.to_string())?,
        rx: File::create(output.join("rx.bin")).map_err(|e| e.to_string())?,
        output: output.clone(),
        phase: "discover",
        sequence: 0,
        selection: None,
        snapshots: BTreeMap::new(),
        globals: None,
        verified: false,
        sent: false,
    };
    let mut report = serde_json::json!({"profile":name,"edit_verified":false,"restore_verified":false,"control_state_restored":false,"current_unchanged":false,"stored_unchanged":false,"physical_switch_triggers":0,"write_attempted":false});
    let result = (|| {
        lab.input(AppInput::Ui(UiIntent::DiscoverDevices))?;
        lab.wait(|lab| lab.selection.is_some())?;
        let selection = lab
            .selection
            .clone()
            .ok_or("Exactly one GP-200 MIDI pair required")?;
        lab.phase = "connect";
        lab.input(AppInput::Ui(UiIntent::Connect(selection.clone())))?;
        lab.wait(|lab| {
            lab.app.state().connection == ConnectionState::Ready
                && lab.app.state().confirmed_buffer.is_some()
        })?;
        let current = lab
            .app
            .state()
            .confirmed_buffer
            .clone()
            .ok_or("Current baseline missing")?;
        let context = current
            .context
            .filter(|c| c.group == 0)
            .ok_or("Current source slot required")?;
        let slot = SlotId::new(context.selector).map_err(|e| e.to_string())?;
        lab.phase = "stored-baseline";
        let stored = lab.read_preset(PresetTarget::Stored(slot))?;
        lab.phase = "globals-baseline";
        let before = lab.read_globals()?;
        let (patch, restore) = recipe(&name, &before)?;
        report["edit"] = serde_json::to_value(&patch).map_err(|e| e.to_string())?;
        report["restore"] = serde_json::to_value(&restore).map_err(|e| e.to_string())?;
        lab.phase = "mapping-edit";
        let edit_result = lab.edit(patch.clone());
        report["write_attempted"] = lab.sent.into();
        // On uncertainty, reconnect only to obtain fresh state for the exact
        // captured restoration. Never replay the requested edit.
        if edit_result.is_err() {
            lab.phase = "identify-for-restoration";
            lab.input(AppInput::Ui(UiIntent::Connect(selection)))?;
            lab.wait(|lab| lab.app.state().connection == ConnectionState::Ready)?;
            lab.phase = "fresh-restoration-globals";
            lab.read_globals()?;
        }
        let after = lab
            .app
            .state()
            .device_globals
            .clone()
            .ok_or("Actual global readback missing")?;
        let expected =
            GlobalRecords::from_transfer(before.expected_after(&patch).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        let expected_changed = expected.control_state_equal(after.records());
        if !expected_changed && !before.records().control_state_equal(after.records()) {
            return Err("Unexpected global changes; exact restoration requires inspection".into());
        }
        lab.phase = "current-before-restoration";
        let pre_restore = lab.read_preset(PresetTarget::EditBuffer)?;
        if !preset_equal(&current, &pre_restore) {
            return Err(
                "Current preset changed; root inspection required before restoration".into(),
            );
        }
        lab.phase = "mapping-restoration";
        let restore_result = lab.edit(restore);
        lab.phase = "globals-after-restoration";
        let restored = lab.read_globals()?;
        lab.phase = "stored-after-restoration";
        let stored_after = lab.read_preset(PresetTarget::Stored(slot))?;
        lab.phase = "current-after-restoration";
        let current_after = lab.read_preset(PresetTarget::EditBuffer)?;
        report["edit_verified"] = (edit_result.is_ok() && expected_changed).into();
        report["restore_verified"] = restore_result.is_ok().into();
        report["control_state_restored"] = before
            .records()
            .control_state_equal(restored.records())
            .into();
        report["exact_raw_bundle_equal_after_restore"] =
            (before.records().raw() == restored.records().raw()).into();
        report["current_unchanged"] = preset_equal(&current, &current_after).into();
        report["stored_unchanged"] = preset_equal(&stored, &stored_after).into();
        report["edit_error"] = serde_json::json!(edit_result.err());
        report["restore_error"] = serde_json::json!(restore_result.err());
        report["comparison_policy"] = serde_json::json!(
            "Complete ordered stable global state; retained volatile1007:54/55, skipped1007:56,1002:6/7 and style alignment/name tail excluded"
        );
        for key in [
            "edit_verified",
            "restore_verified",
            "control_state_restored",
            "current_unchanged",
            "stored_unchanged",
        ] {
            if report[key] != true {
                return Err("Storage proof or restoration did not pass".into());
            }
        }
        Ok(())
    })();
    report["error"] = serde_json::json!(result.as_ref().err());
    report["terminal_phase"] = lab.phase.into();
    fs::write(
        output.join("result.json"),
        serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    println!("{report}");
    lab.service.shutdown();
    lab.service.wait_stopped(Duration::from_secs(1));
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
    use gp200_protocol::RawTransfer;
    #[test]
    fn fixed_representatives_are_meaningful_and_restore_exact_native_bytes() {
        let raw = RawTransfer::from_payload(
            gp200_protocol::WRITE_KIND,
            include_bytes!("../../../tests/fixtures/globals/baseline.payload.bin").to_vec(),
        )
        .unwrap();
        let state =
            DeviceGlobalState::from_transfer(Revision(1), SessionEpoch(1), raw, vec![]).unwrap();
        for name in [
            "table1-tap",
            "table1-hold",
            "table1-combined",
            "table2-tap",
            "table2-hold",
            "table2-combined",
        ] {
            let (patch, restore) = recipe(name, &state).unwrap();
            assert_ne!(patch, restore);
            let after = DeviceGlobalState::from_transfer(
                Revision(2),
                state.epoch,
                state.expected_after(&patch).unwrap(),
                vec![],
            )
            .unwrap();
            assert!(ValidatedInactiveFootswitchEdit::validate(&restore, &after).is_ok());
            assert!(state.records().control_state_equal(
                &GlobalRecords::from_transfer(after.expected_after(&restore).unwrap()).unwrap()
            ));
        }
        assert!(profile("table0-tap").is_none());
        assert!(profile("raw-100f").is_none());
    }
}
