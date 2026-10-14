//! Root-only reversible Patch Pan/style proof through AppController and ALSA.
//! Dry by default. This process owns .hardware-session.lock. Only fixed sample
//! profiles are accepted; no raw packets, arbitrary values or stored writes.
use gp200_app::AppController;
use gp200_catalog::Catalog;
use gp200_domain::*;
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
fn recipe(name: &str, before: &PresetSnapshot) -> Result<(MetadataPatch, MetadataPatch), String> {
    if before.context
        != Some(WireContext {
            group: 0,
            selector: 23,
        })
    {
        return Err("This root fixture requires current source slot23".into());
    }
    let original = &before.view.metadata;
    let (patch, restore) = match name {
        "pan" => {
            let old = original.native_pan.ok_or("Pan missing")?;
            if ![0, 1].contains(&old.raw()) {
                return Err("Pan baseline must be 0/1".into());
            }
            (
                MetadataPatch {
                    native_pan: Some(PatchPan::new(if old.raw() == 0 { 1 } else { 0 }).unwrap()),
                    ..Default::default()
                },
                MetadataPatch {
                    native_pan: Some(old),
                    ..Default::default()
                },
            )
        }
        "style" => {
            let old = original.native_style.ok_or("Style missing")?;
            if ![0, 5].contains(&old.raw()) {
                return Err("Style baseline must be 0/5".into());
            }
            (
                MetadataPatch {
                    native_style: Some(
                        NativeStyleId::new(if old.raw() == 0 { 5 } else { 0 }).unwrap(),
                    ),
                    ..Default::default()
                },
                MetadataPatch {
                    native_style: Some(old),
                    ..Default::default()
                },
            )
        }
        _ => return Err("Choose pan or style".into()),
    };
    patch
        .hardware_profile(original)
        .map_err(|e| e.to_string())?;
    Ok((patch, restore))
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
    fn undo(&mut self) -> Result<(), String> {
        self.verified = false;
        self.sent = false;
        self.input(AppInput::Ui(
            UiIntent::Undo(gp200_app::device_document_id()),
        ))?;
        self.wait(|lab| lab.verified && !lab.app.device_operation_pending())
    }
    fn edit(&mut self, patch: MetadataPatch) -> Result<(), String> {
        self.verified = false;
        self.sent = false;
        self.input(AppInput::Ui(UiIntent::ApplyEdit {
            document: gp200_app::device_document_id(),
            patch: EditPatch::SetMetadata(patch),
        }))?;
        self.wait(|lab| lab.verified && !lab.app.device_operation_pending())
    }
}
fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let name = args.next().unwrap_or_else(|| "pan".into());
    if !["pan", "style"].contains(&name.as_str()) {
        return Err("Choose pan or style".into());
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
            "Dry: {name}; current source23, complete fresh preflight, one typed sample edit, production Undo, source23/scratch255/globals restoration checks. No MIDI opened."
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
    let mut report = serde_json::json!({"profile":name,"edit_verified":false,"undo_verified":false,"current_restored":false,"stored_source_unchanged":false,"stored_scratch_unchanged":false,"globals_unchanged":false,"write_attempted":false,"stored_writes":0});
    let result = (|| {
        lab.input(AppInput::Ui(UiIntent::DiscoverDevices))?;
        lab.wait(|lab| lab.selection.is_some())?;
        let selection = lab
            .selection
            .clone()
            .ok_or("Exactly one GP-200 pair required")?;
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
            .ok_or("Current baseline required")?;
        let (patch, restore) = recipe(&name, &current)?;
        report["edit"] = serde_json::to_value(&patch).map_err(|e| e.to_string())?;
        report["restore"] = serde_json::to_value(&restore).map_err(|e| e.to_string())?;
        lab.phase = "stored-source-before";
        let source = lab.read_preset(PresetTarget::Stored(SlotId::new(23).unwrap()))?;
        lab.phase = "stored-scratch-before";
        let scratch = lab.read_preset(PresetTarget::Stored(SlotId::new(255).unwrap()))?;
        lab.phase = "globals-before";
        let globals = lab.read_globals()?;
        let mut draft = PresetDraft::from_snapshot(&current);
        draft
            .apply(&EditPatch::SetMetadata(patch.clone()))
            .map_err(|e| e.to_string())?;
        let expected = draft
            .snapshot(&current, SnapshotSource::Simulator, Revision(0))
            .map_err(|e| e.to_string())?;
        lab.phase = "native-edit";
        let edit_result = lab.edit(patch);
        report["write_attempted"] = lab.sent.into();
        if edit_result.is_err() {
            lab.phase = "read-only-reconnect-for-restoration";
            lab.input(AppInput::Ui(UiIntent::Connect(selection)))?;
            lab.wait(|lab| lab.app.state().connection == ConnectionState::Ready)?;
        }
        lab.phase = "current-before-undo";
        let actual = lab.read_preset(PresetTarget::EditBuffer)?;
        let edited = preset_equal(&expected, &actual);
        let baseline = preset_equal(&current, &actual);
        if !edited && !baseline {
            return Err("Unexpected current changes; root must inspect before restoring".into());
        }
        let undo_attempted = edited && edit_result.is_ok();
        let restored_result = if edited {
            lab.phase = "production-undo";
            if edit_result.is_ok() {
                lab.undo()
            } else {
                lab.edit(restore)
            }
        } else {
            Ok(())
        };
        let undo_verified = undo_attempted && restored_result.is_ok();
        let undo_error = if undo_attempted {
            restored_result.as_ref().err().cloned()
        } else {
            None
        };
        let recovery_attempted = edited && !undo_verified;
        // A failed Undo is never silently replayed. Read fresh state and attempt only
        // the captured baseline field if the actual full patch still equals our edit.
        let restored_result = if restored_result.is_err() {
            lab.phase = "refresh-after-undo-failure";
            let actual = lab.read_preset(PresetTarget::EditBuffer)?;
            if preset_equal(&current, &actual) {
                Ok(())
            } else if preset_equal(&expected, &actual) {
                lab.edit(recipe(&name, &current)?.1)
            } else {
                return Err("Unexpected state after Undo; root inspection required".into());
            }
        } else {
            restored_result
        };
        lab.phase = "current-after";
        let current_after = lab.read_preset(PresetTarget::EditBuffer)?;
        lab.phase = "stored-source-after";
        let source_after = lab.read_preset(PresetTarget::Stored(SlotId::new(23).unwrap()))?;
        lab.phase = "stored-scratch-after";
        let scratch_after = lab.read_preset(PresetTarget::Stored(SlotId::new(255).unwrap()))?;
        lab.phase = "globals-after";
        let globals_after = lab.read_globals()?;
        report["edit_verified"] = (edit_result.is_ok() && edited).into();
        report["undo_attempted"] = undo_attempted.into();
        report["undo_verified"] = undo_verified.into();
        report["recovery_attempted"] = recovery_attempted.into();
        report["recovery_verified"] = (recovery_attempted
            && restored_result.is_ok()
            && preset_equal(&current, &current_after))
        .into();
        report["current_restored"] = preset_equal(&current, &current_after).into();
        report["stored_source_unchanged"] = preset_equal(&source, &source_after).into();
        report["stored_scratch_unchanged"] = preset_equal(&scratch, &scratch_after).into();
        report["globals_unchanged"] = globals
            .records()
            .control_state_equal(globals_after.records())
            .into();
        report["edit_error"] = serde_json::json!(edit_result.err());
        report["undo_error"] = serde_json::json!(undo_error);
        report["recovery_error"] = serde_json::json!(restored_result.err());
        for key in [
            "edit_verified",
            "undo_verified",
            "current_restored",
            "stored_source_unchanged",
            "stored_scratch_unchanged",
            "globals_unchanged",
        ] {
            if report[key] != true {
                return Err("Metadata proof or restoration did not pass".into());
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
    #[test]
    fn fixed_profiles_have_only_expected_offsets_and_exact_restoration() {
        let raw = RawTransfer::from_payload(
            0x12,
            include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
        )
        .unwrap();
        let before = PresetSnapshot::from_raw(
            Arc::new(raw),
            SnapshotSource::Simulator,
            Some(PresetTarget::EditBuffer),
            Revision(0),
        );
        for name in ["pan", "style"] {
            let (patch, restore) = recipe(name, &before).unwrap();
            let mut draft = PresetDraft::from_snapshot(&before);
            draft.apply(&EditPatch::SetMetadata(patch)).unwrap();
            let after = draft
                .snapshot(&before, SnapshotSource::Simulator, Revision(1))
                .unwrap();
            assert!(!preset_equal(&before, &after));
            draft.apply(&EditPatch::SetMetadata(restore)).unwrap();
            assert!(preset_equal(
                &before,
                &draft
                    .snapshot(&after, SnapshotSource::Simulator, Revision(2))
                    .unwrap()
            ));
        }
    }
}
