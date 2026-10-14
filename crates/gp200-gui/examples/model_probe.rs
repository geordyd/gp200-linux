//! Root-only model selection + Undo through the actual reducer and MIDI actor.
//! Dry by default. Execute only through tools/lab/verify_rust_model_transitions.py,
//! which owns the hardware lock, captures stored baselines and restores hidden
//! slots via the separately proved source reload recovery.
use gp200_app::{AppController, device_document_id};
use gp200_catalog::Catalog;
use gp200_domain::*;
use gp200_transport::{MidiDeviceService, TransportConfig};
use std::{
    fs::{self, File},
    io::Write,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

fn profile(name: &str) -> Option<(u8, u32, u32)> {
    gp200_transport::captured_model_profile(name)
        .map(|profile| (profile.module, profile.baseline, profile.target))
}
struct Lab {
    output: PathBuf,
    app: AppController,
    service: MidiDeviceService,
    trace: File,
    tx: File,
    rx: File,
    phase: &'static str,
    sequence: u64,
    selection: Option<DeviceSelection>,
}
impl Lab {
    fn effects(&self, effects: Vec<Effect>) -> Result<(), String> {
        for effect in effects {
            if let Effect::Device(command) = effect {
                self.service
                    .try_submit(command)
                    .map_err(|error| format!("Cannot submit: {error:?}"))?;
            } else {
                return Err("Unexpected non-device effect in model proof".into());
            }
        }
        Ok(())
    }
    fn input(&mut self, input: AppInput) -> Result<(), String> {
        let effects = self.app.dispatch(input);
        self.effects(effects)
    }
    fn wait(&mut self, predicate: impl Fn(&Self) -> bool) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Some(event) = self.service.try_next_event() {
                writeln!(self.trace, "{} {event:?}", self.phase)
                    .map_err(|error| error.to_string())?;
                self.sequence += 1;
                match &event {
                    DeviceEvent::PortsChanged(ports) => {
                        let pairs: Vec<_> = ports
                            .iter()
                            .filter(|port| port.display_name.to_uppercase().contains("GP-200"))
                            .collect();
                        if pairs.len() == 1 {
                            self.selection = Some(pairs[0].selection.clone());
                        }
                    }
                    DeviceEvent::SnapshotReceived {
                        stamp,
                        target: PresetTarget::EditBuffer,
                        snapshot,
                    } => {
                        fs::write(
                            self.output.join(format!(
                                "{}-{}-{}.payload.bin",
                                self.phase, self.sequence, stamp.intent.0
                            )),
                            snapshot.raw.payload(),
                        )
                        .map_err(|error| error.to_string())?;
                    }
                    DeviceEvent::Diagnostic(diagnostic)
                        if matches!(diagnostic.category.as_str(), "midi-tx" | "midi-rx") =>
                    {
                        if let Some(bytes) = &diagnostic.bytes {
                            let file = if diagnostic.category == "midi-tx" {
                                &mut self.tx
                            } else {
                                &mut self.rx
                            };
                            file.write_all(bytes).map_err(|error| error.to_string())?;
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
                if let Some(error) = failure {
                    return Err(error);
                }
                if predicate(self) {
                    return Ok(());
                }
            } else {
                thread::sleep(Duration::from_millis(2));
            }
        }
        Err(format!(
            "{} exceeded its bounded event deadline",
            self.phase
        ))
    }
    fn settled(&self, module: ModuleId, code: ModelCode) -> bool {
        !self.app.device_operation_pending()
            && self
                .app
                .state()
                .document_status(&device_document_id())
                .is_some_and(|status| status.verified_in_buffer)
            && self
                .app
                .state()
                .confirmed_buffer
                .as_ref()
                .and_then(|snapshot| snapshot.view.module(module))
                .is_some_and(|actual| actual.model == code)
    }
}
fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let name = args.next().unwrap_or_else(|| "dly".into());
    let (id, old_code, target) = profile(&name).ok_or_else(|| {
        format!(
            "Choose one captured profile: {}",
            gp200_transport::captured_model_profiles()
                .iter()
                .map(|profile| profile.name)
                .collect::<Vec<_>>()
                .join("|")
        )
    })?;
    let mut execute = false;
    let mut output = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--execute" => execute = true,
            "--output" => {
                output = Some(PathBuf::from(
                    args.next().ok_or("--output requires a fresh directory")?,
                ))
            }
            _ => return Err(format!("Unknown argument {argument}")),
        }
    }
    if !execute {
        println!(
            "Dry: {name} module{id}, {old_code:#010x} → {target:#010x}, reducer Undo and declared controls readback. No MIDI opened. Execute through the root recovery wrapper."
        );
        return Ok(());
    }
    let output = output.ok_or("--execute requires --output")?;
    if output.exists() {
        return Err("Evidence directory must be fresh".into());
    }
    fs::create_dir_all(&output).map_err(|error| error.to_string())?;
    let mut lab = Lab {
        trace: File::create(output.join("trace.txt")).map_err(|error| error.to_string())?,
        tx: File::create(output.join("tx.bin")).map_err(|error| error.to_string())?,
        rx: File::create(output.join("rx.bin")).map_err(|error| error.to_string())?,
        output: output.clone(),
        app: AppController::new(
            Catalog::embedded().map_err(|error| error.to_string())?,
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
        phase: "discover",
        sequence: 0,
        selection: None,
    };
    let mut report = serde_json::json!({"profile": name, "module": id, "old_code": old_code, "target": target, "forward_verified": false, "undo_verified": false, "defined_controls_restored": false});
    let result = (|| {
        lab.input(AppInput::Ui(UiIntent::DiscoverDevices))?;
        lab.wait(|lab| lab.selection.is_some())?;
        lab.phase = "baseline";
        lab.input(AppInput::Ui(UiIntent::Connect(
            lab.selection
                .clone()
                .ok_or("Exactly one GP-200 MIDI pair is required")?,
        )))?;
        let module = ModuleId::new(id).map_err(|error| error.to_string())?;
        lab.wait(|lab| {
            lab.app.state().connection == ConnectionState::Ready
                && lab.app.state().confirmed_buffer.is_some()
        })?;
        let before = lab
            .app
            .state()
            .confirmed_buffer
            .clone()
            .ok_or("Current baseline absent")?;
        if before.context
            != Some(WireContext {
                group: 0,
                selector: 23,
            })
            || before
                .view
                .module(module)
                .is_none_or(|actual| actual.model != ModelCode(old_code))
        {
            return Err(
                "Expected the independently backed-up source23 and exact original model".into(),
            );
        }
        fs::write(output.join("before.payload.bin"), before.raw.payload())
            .map_err(|error| error.to_string())?;
        if let Some(reason) =
            gp200_transport::hardware_model_blocked_reason(&before, module, ModelCode(target))
        {
            return Err(reason);
        }
        lab.phase = "forward";
        lab.input(AppInput::Ui(UiIntent::ApplyEdit {
            document: device_document_id(),
            patch: EditPatch::SetModel {
                module,
                model: ModelCode(target),
            },
        }))?;
        lab.wait(|lab| lab.settled(module, ModelCode(target)))?;
        report["forward_verified"] = true.into();
        lab.phase = "undo";
        lab.input(AppInput::Ui(UiIntent::Undo(device_document_id())))?;
        lab.wait(|lab| lab.settled(module, ModelCode(old_code)))?;
        report["undo_verified"] = true.into();
        let after = lab
            .app
            .state()
            .confirmed_buffer
            .clone()
            .ok_or("Undo readback absent")?;
        fs::write(output.join("after-undo.payload.bin"), after.raw.payload())
            .map_err(|error| error.to_string())?;
        let original = before.view.module(module).ok_or("Original module absent")?;
        let restored = after.view.module(module).ok_or("Restored module absent")?;
        let definition = lab
            .app
            .catalog()
            .model_by_id(id, old_code)
            .ok_or("Original model absent")?;
        let defined = definition.parameters.iter().all(|control| {
            original.parameters.get(usize::from(control.parameter_id))
                == restored.parameters.get(usize::from(control.parameter_id))
        });
        report["defined_controls_restored"] = defined.into();
        report["hidden_slots_changed"] = serde_json::json!(
            (0..15)
                .filter(|index| definition.parameter(*index as u8).is_none()
                    && original.parameters[*index] != restored.parameters[*index])
                .collect::<Vec<_>>()
        );
        if !defined {
            return Err("Undo did not restore every declared original control bit".into());
        }
        Ok(())
    })();
    report["error"] = serde_json::json!(result.as_ref().err());
    fs::write(
        output.join("result.json"),
        serde_json::to_vec_pretty(&report).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
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

    #[test]
    fn probe_targets_are_exact_compiled_profiles_and_exclude_ignored_models() {
        assert_eq!(gp200_transport::captured_model_profiles().len(), 17);
        for captured in gp200_transport::captured_model_profiles() {
            assert_eq!(
                profile(captured.name),
                Some((captured.module, captured.baseline, captured.target))
            );
        }
        for unknown in [
            "nr-gate3",
            "mod-achorus",
            "amp",
            "cab-user-ir",
            "cab-twdcp-to-j120",
        ] {
            assert!(profile(unknown).is_none());
        }
    }
}
