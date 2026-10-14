//! Default-off factory candidate probe; root alone operates MIDI.
//! Dry by default; root must review a recovery supervisor before execution.
use gp200_app::{AppController, device_document_id};
use gp200_catalog::Catalog;
use gp200_domain::*;
use gp200_transport::{MidiDeviceService, TransportConfig};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

fn profile(name: &str) -> Option<FactoryAmpProfileId> {
    FACTORY_AMP_CANDIDATES
        .iter()
        .flat_map(|descriptor| {
            [false, true].map(|mode| {
                FactoryAmpProfileId::new(descriptor.model, mode).expect("static descriptor")
            })
        })
        .find(|profile| profile.name() == name)
}
fn check_hash(bytes: &[u8], review: &serde_json::Value, field: &str) -> Result<(), String> {
    if review[field].as_str() != Some(JobSha256::of(bytes).hex().as_str()) {
        return Err(format!(
            "Reviewed {field} differs from immutable actual bytes"
        ));
    }
    Ok(())
}
fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| error.to_string())
}
fn durable_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())?;
    sync_directory(path.parent().ok_or("Missing output parent")?)
}
fn complete_preset_equal(before: &RawTransfer, after: &RawTransfer) -> bool {
    dependent_preset_shape(before).is_ok()
        && dependent_preset_shape(after).is_ok()
        && before.kind() == after.kind()
        && before
            .records()
            .iter()
            .zip(after.records())
            .all(|(old, new)| {
                old.command == new.command
                    && old.body.len() == new.body.len()
                    && old
                        .body
                        .iter()
                        .zip(&new.body)
                        .enumerate()
                        .all(|(index, (a, b))| {
                            a == b
                                || match old.command {
                                    2 | 8 => index == 15,
                                    0x0f => matches!(index, 2 | 3),
                                    _ => false,
                                }
                        })
            })
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
    profile: FactoryAmpProfileId,
    reviewed_selection: DeviceSelection,
    reviewed_before: Arc<PresetSnapshot>,
    submitted: Vec<serde_json::Value>,
    model_readbacks: Vec<serde_json::Value>,
    started: Instant,
}
impl Lab {
    fn effects(&mut self, effects: Vec<Effect>) -> Result<(), String> {
        for effect in effects {
            let Effect::Device(command) = effect else {
                return Err("Unexpected non-device effect in candidate model proof".into());
            };
            let summary = match &command {
                DeviceCommand::SubmitFactoryAmpModel {
                    stamp,
                    model,
                    profile,
                    expected_auto_cab,
                    before_globals,
                    ..
                } => {
                    if self
                        .submitted
                        .iter()
                        .filter(|command| command["kind"] == "factory-amp-model")
                        .count()
                        >= 2
                    {
                        return Err("Reviewed maximum of two model attempts exhausted".into());
                    }
                    let target = match self.phase {
                        "forward" => self.profile.candidate(),
                        "undo" => FACTORY_AMP_BASELINE,
                        _ => {
                            return Err("Model command outside reviewed forward/Undo phases".into());
                        }
                    };
                    if *profile != self.profile
                        || *model != target
                        || *expected_auto_cab != self.profile.auto_cab()
                    {
                        return Err("Exact candidate profile, target or Auto CAB differs from reviewed probe".into());
                    }
                    serde_json::json!({"phase":self.phase,"intent":stamp.intent.0,"kind":"factory-amp-model","module":3,"target":model.0,"profile":profile.name(),"expected_auto_cab":expected_auto_cab,"bound_global_revision":before_globals.revision.0})
                }
                DeviceCommand::SubmitEdit {
                    stamp,
                    patch:
                        EditPatch::SetParameter {
                            module,
                            model,
                            parameter,
                            value,
                            ..
                        },
                    ..
                } if self.phase == "undo" => {
                    let allowed = (module.get() == 3
                        && *model == FACTORY_AMP_BASELINE
                        && parameter.get() <= 5)
                        || (self.profile.auto_cab()
                            && module.get() == 5
                            && *model == FACTORY_AMP_BASELINE_CAB
                            && matches!(parameter.get(), 1 | 5 | 6));
                    if !allowed
                        || self
                            .reviewed_before
                            .view
                            .module(*module)
                            .and_then(|source| source.parameters.get(usize::from(parameter.get())))
                            .copied()
                            != Some(*value)
                    {
                        return Err("Undo control differs from exact reviewed original bits and allowed source modules".into());
                    }
                    if self
                        .app
                        .catalog()
                        .model_by_id(module.get(), model.0)
                        .and_then(|definition| definition.parameter(parameter.get()))
                        .is_none()
                    {
                        return Err("Undo requested an undeclared control".into());
                    }
                    serde_json::json!({"phase":self.phase,"intent":stamp.intent.0,"kind":"declared-control","module":module.get(),"model":model.0,"parameter":parameter.get(),"raw_bits":value.0})
                }
                DeviceCommand::Discover
                | DeviceCommand::Connect { .. }
                | DeviceCommand::ReadGlobals(_)
                | DeviceCommand::RefreshTarget { .. } => {
                    serde_json::json!({"phase":self.phase,"kind":"read-only","debug":format!("{command:?}")})
                }
                _ => {
                    return Err(format!(
                        "Unexpected write/command in candidate proof: {command:?}"
                    ));
                }
            };
            let index = self.submitted.len();
            durable_file(
                &self.output.join(format!("command-{index}.json")),
                &serde_json::to_vec_pretty(&summary).map_err(|error| error.to_string())?,
            )?;
            self.submitted.push(summary);
            self.service
                .try_submit(command)
                .map_err(|error| format!("Cannot submit: {error:?}"))?;
        }
        Ok(())
    }
    fn input(&mut self, input: AppInput) -> Result<(), String> {
        let effects = self.app.dispatch(input);
        self.effects(effects)
    }
    fn event(&mut self, event: DeviceEvent) -> Result<(), String> {
        writeln!(
            self.trace,
            "{} {} {event:?}",
            self.started.elapsed().as_nanos(),
            self.phase
        )
        .map_err(|error| error.to_string())?;
        self.trace
            .flush()
            .and_then(|()| self.trace.sync_all())
            .map_err(|error| error.to_string())?;
        self.sequence += 1;
        match &event {
            DeviceEvent::PortsChanged(ports) => {
                let pairs: Vec<_> = ports
                    .iter()
                    .filter(|port| port.display_name.to_uppercase().contains("GP-200"))
                    .collect();
                if pairs.len() != 1 {
                    return Err("Exactly one normal GP-200 endpoint is required".into());
                }
                if pairs[0].selection.input_id != self.reviewed_selection.input_id
                    || pairs[0].selection.output_id != self.reviewed_selection.output_id
                {
                    return Err(
                        "Discovered endpoints differ from root's reviewed canonical pair".into(),
                    );
                }
                self.selection = Some(pairs[0].selection.clone());
            }
            DeviceEvent::SnapshotReceived {
                stamp,
                target: PresetTarget::EditBuffer,
                snapshot,
            } => {
                let name = format!(
                    "{}-{}-{}.preset.payload.bin",
                    self.phase, self.sequence, stamp.intent.0
                );
                durable_file(&self.output.join(&name), snapshot.raw.payload())?;
                if matches!(self.phase, "forward" | "undo") {
                    self.model_readbacks.push(serde_json::json!({"phase":self.phase,"intent":stamp.intent.0,"file":name,"revision":snapshot.revision.0}));
                }
            }
            DeviceEvent::GlobalStateReceived { stamp, state } => {
                durable_file(
                    &self.output.join(format!(
                        "{}-{}-{}.globals.payload.bin",
                        self.phase, self.sequence, stamp.intent.0
                    )),
                    state.records().raw().payload(),
                )?;
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
                    file.sync_all().map_err(|error| error.to_string())?;
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
            DeviceEvent::ConnectionChanged {
                state: ConnectionState::Recovering,
                ..
            } => Some("Physical session entered recovery".into()),
            _ => None,
        };
        self.input(AppInput::Device(event))?;
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(())
    }
    fn wait(&mut self, predicate: impl Fn(&Self) -> bool) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(25);
        while Instant::now() < deadline {
            self.input(AppInput::Tick(self.started.elapsed().as_millis() as u64))?;
            if let Some(event) = self.service.try_next_event() {
                self.event(event)?;
            } else {
                thread::sleep(Duration::from_millis(2));
            }
            if predicate(self) {
                return Ok(());
            }
        }
        Err(format!(
            "{} exceeded the bounded event deadline",
            self.phase
        ))
    }
    fn settled(&self, module: ModuleId, model: ModelCode) -> bool {
        !self.app.device_operation_pending()
            && !self.app.external_reconciliation_pending()
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
                .is_some_and(|actual| actual.model == model)
    }
}
fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut name = None;
    let mut execute = false;
    let mut output = None;
    let mut before_preset = None;
    let mut before_globals = None;
    let mut review_path = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--execute" => execute = true,
            "--review-manifest" => {
                review_path = Some(PathBuf::from(
                    args.next().ok_or("--review-manifest requires a file")?,
                ))
            }
            "--profile" => name = Some(args.next().ok_or("--profile requires a name")?),
            "--output" => {
                output = Some(PathBuf::from(
                    args.next().ok_or("--output requires a fresh directory")?,
                ))
            }
            "--before-preset" => {
                before_preset = Some(PathBuf::from(
                    args.next().ok_or("--before-preset requires a file")?,
                ))
            }
            "--before-globals" => {
                before_globals = Some(PathBuf::from(
                    args.next().ok_or("--before-globals requires a file")?,
                ))
            }
            _ => return Err(format!("Unknown argument {argument}")),
        }
    }
    let name = name.ok_or("--profile required")?;
    let candidate = profile(&name).ok_or("Unknown exact factory AMP profile")?;
    let id = 3;
    let baseline = FACTORY_AMP_BASELINE.0;
    let target = candidate.candidate().0;
    if !execute {
        println!(
            "Dry: {name}; typed controller forward + Undo. No MIDI opened. Normal eligibility false. Candidate feature {}.",
            cfg!(feature = "factory-amp-trials")
        );
        return Ok(());
    }
    if !cfg!(feature = "factory-amp-trials") || cfg!(feature = "dependent-model-trials") {
        return Err("Execute requires factory-amp-trials without dependent-model-trials".into());
    }
    let output = output.ok_or("--execute requires --output")?;
    let review_bytes = fs::read(review_path.ok_or("Root-reviewed --review-manifest required")?)
        .map_err(|error| error.to_string())?;
    if review_bytes.len() > 1024 * 1024 {
        return Err("Probe review exceeds 1 MiB".into());
    }
    let review: serde_json::Value =
        serde_json::from_slice(&review_bytes).map_err(|error| error.to_string())?;
    if review["schema"] != 1
        || review["profile"].as_str() != Some(name.as_str())
        || review["target_code"].as_u64() != Some(u64::from(target))
        || review["auto_cab"].as_bool() != Some(candidate.auto_cab())
        || review["maximum_model_attempts"] != 2
        || review["normal_promotion"] != false
    {
        return Err(
            "Root probe review differs from the exact candidate/context/attempt scope".into(),
        );
    }
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    check_hash(
        &fs::read(executable).map_err(|error| error.to_string())?,
        &review,
        "elf_sha256",
    )?;
    let closure_path = PathBuf::from(
        review["source_closure_path"]
            .as_str()
            .ok_or("Reviewed source closure path required")?,
    );
    check_hash(
        &fs::read(closure_path).map_err(|error| error.to_string())?,
        &review,
        "source_closure_sha256",
    )?;
    let reviewed_selection = DeviceSelection {
        input_id: review["input_id"]
            .as_str()
            .ok_or("Reviewed input endpoint required")?
            .into(),
        output_id: review["output_id"]
            .as_str()
            .ok_or("Reviewed output endpoint required")?
            .into(),
        fingerprint_hint: None,
    };
    if !reviewed_selection.input_id.starts_with("alsa-raw:in:")
        || !reviewed_selection.output_id.starts_with("alsa-raw:out:")
    {
        return Err("Reviewed canonical ALSA raw endpoint pair required".into());
    }
    let output_parent = fs::canonicalize(output.parent().ok_or("Fresh output parent required")?)
        .map_err(|error| error.to_string())?;
    let output = output_parent.join(output.file_name().ok_or("Fresh output basename required")?);
    if review["output_directory"].as_str() != output.to_str() {
        return Err("Output directory differs from root's exact reviewed attempt".into());
    }
    let parent_preset = RawTransfer::from_payload(
        0x12,
        fs::read(before_preset.ok_or("Parent raw before-preset required")?)
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let parent_globals = RawTransfer::from_payload(
        0x12,
        fs::read(before_globals.ok_or("Parent raw before-globals required")?)
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    check_hash(parent_preset.payload(), &review, "before_preset_sha256")?;
    check_hash(parent_globals.payload(), &review, "before_globals_sha256")?;
    let reviewed_before = PresetSnapshot::from_raw(
        Arc::new(parent_preset.clone()),
        SnapshotSource::Hardware {
            epoch: SessionEpoch(1),
        },
        Some(PresetTarget::EditBuffer),
        Revision(1),
    );
    let reviewed_globals = DeviceGlobalState::from_transfer(
        Revision(1),
        SessionEpoch(1),
        parent_globals.clone(),
        vec![],
    )
    .map_err(|error| error.to_string())?;
    let reviewed_plan =
        FactoryAmpPlan::from_snapshot(&reviewed_before, &reviewed_globals, ModelCode(target))
            .map_err(|error| error.message)?;
    if reviewed_plan.profile != candidate
        || reviewed_before.context
            != Some(WireContext {
                group: 0,
                selector: 23,
            })
    {
        return Err(
            "Reviewed before-image differs from Match CL/current23 candidate context".into(),
        );
    }
    dependent_preset_shape(&parent_preset).map_err(|error| error.message)?;
    dependent_globals_shape(&parent_globals).map_err(|error| error.message)?;
    if output.exists() {
        return Err("Evidence directory must be fresh".into());
    }
    fs::create_dir_all(&output).map_err(|error| error.to_string())?;
    sync_directory(output.parent().ok_or("Output parent missing")?)?;
    let mut lab = Lab {
        trace: File::create(output.join("trace.txt")).map_err(|error| error.to_string())?,
        tx: File::create(output.join("tx.bin")).map_err(|error| error.to_string())?,
        rx: File::create(output.join("rx.bin")).map_err(|error| error.to_string())?,
        output: output.clone(),
        app: AppController::new(
            Catalog::embedded().map_err(|error| error.to_string())?,
            AppPreferences::default(),
        ),
        service: MidiDeviceService::try_with_backend(
            Box::new(gp200_transport::AlsaRawBackend::new()),
            TransportConfig {
                trace_bytes: true,
                ..TransportConfig::alsa_raw()
            },
            no_wake(),
        )
        .map_err(|error| error.message)?,
        phase: "discover",
        sequence: 0,
        selection: None,
        profile: candidate,
        reviewed_selection,
        reviewed_before: Arc::new(reviewed_before),
        submitted: vec![],
        model_readbacks: vec![],
        started: Instant::now(),
    };
    let mut report = serde_json::json!({"schema":1,"profile":name,"candidate_feature":true,"normal_build_gates_open":false,"accepted":false,"root_physical_acceptance":false,"forward_verified":false,"undo_verified":false,"complete_before_images_durably_published_before_model":false,"internal_actor_preflight_fsynced_before_model":false,"internal_actor_preflight_evidence":"Asynchronous RX diagnostics are fsynced when retained; their flush does not precede the immediate worker model send. Published parent/child startup authorities support recovery.","whole_current_restored":false,"globals_restored_including_third_word":false,"service_stopped":false});
    let result = (|| {
        lab.input(AppInput::Ui(UiIntent::DiscoverDevices))?;
        lab.wait(|lab| lab.selection.is_some())?;
        lab.phase = "baseline";
        let selection = lab
            .selection
            .clone()
            .ok_or("Exactly one GP-200 MIDI pair required")?;
        report["selection"] =
            serde_json::json!({"input_id":selection.input_id,"output_id":selection.output_id});
        lab.input(AppInput::Ui(UiIntent::Connect(selection)))?;
        lab.wait(|lab| {
            lab.app.state().connection == ConnectionState::Ready
                && lab.app.state().confirmed_buffer.is_some()
                && !lab.app.device_operation_pending()
                && !lab.app.external_reconciliation_pending()
        })?;
        if lab.app.state().identity.as_ref().is_none_or(|identity| {
            identity.mode != BackendMode::Hardware || identity.model != DeviceModel::Gp200
        }) {
            return Err("Normal GP-200 physical identity required".into());
        }
        lab.input(AppInput::Ui(UiIntent::RefreshDeviceGlobals))?;
        lab.wait(|lab| {
            !lab.app.device_operation_pending()
                && !lab.app.external_reconciliation_pending()
                && lab.app.state().device_globals.is_some()
        })?;
        // Acquire current again after globals; the production actor independently
        // rechecks this exact bound authority before its sole model command.
        let old_revision = lab
            .app
            .state()
            .confirmed_buffer
            .as_ref()
            .ok_or("Current baseline missing")?
            .revision;
        lab.input(AppInput::Ui(UiIntent::Refresh(PresetTarget::EditBuffer)))?;
        lab.wait(|lab| {
            !lab.app.device_operation_pending()
                && !lab.app.external_reconciliation_pending()
                && lab
                    .app
                    .state()
                    .confirmed_buffer
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.revision > old_revision)
        })?;
        let before = lab
            .app
            .state()
            .confirmed_buffer
            .clone()
            .ok_or("Own fresh current absent")?;
        let globals = lab
            .app
            .state()
            .device_globals
            .clone()
            .ok_or("Own fresh globals absent")?;
        if !complete_preset_equal(&parent_preset, &before.raw)
            || !dependent_globals_match(&parent_globals, globals.records().raw(), None)
                .map_err(|error| error.message)?
        {
            return Err("Fresh child authorities differ from the parent's published baseline; no model send".into());
        }
        let module = ModuleId::new(id).map_err(|error| error.to_string())?;
        let plan = FactoryAmpPlan::from_snapshot(&before, &globals, ModelCode(target))
            .map_err(|error| error.message)?;
        if plan.profile != candidate
            || plan.previous.0 != baseline
            || before.context
                != Some(WireContext {
                    group: 0,
                    selector: 23,
                })
        {
            return Err(
                "Fresh source/profile differs from the exact root-reviewed experiment".into(),
            );
        }
        durable_file(
            &output.join("own-fresh-current-before.payload.bin"),
            before.raw.payload(),
        )?;
        durable_file(
            &output.join("own-fresh-globals-before.payload.bin"),
            globals.records().raw().payload(),
        )?;
        durable_file(&output.join("before-images-journal.json"), &serde_json::to_vec_pretty(&serde_json::json!({"schema":1,"current_revision":before.revision.0,"global_revision":globals.revision.0,"epoch":globals.epoch.0,"profile":candidate.name(),"expected_auto_cab":plan.auto_cab,"file_fsync":true,"directory_fsync_completed":true})).map_err(|error| error.to_string())?)?;
        sync_directory(&output)?;
        report["complete_before_images_durably_published_before_model"] = true.into();
        lab.wait(|lab| {
            !lab.app.device_operation_pending()
                && !lab.app.external_reconciliation_pending()
                && matches!(
                    lab.app.operation_availability(OperationKind::SetModel),
                    OperationAvailability::Available
                )
        })?;
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
        let forward = lab
            .app
            .state()
            .confirmed_buffer
            .clone()
            .ok_or("Forward current missing")?;
        durable_file(
            &output.join("after-forward.payload.bin"),
            forward.raw.payload(),
        )?;
        durable_file(
            &output.join("after-forward-globals.payload.bin"),
            lab.app
                .state()
                .device_globals
                .as_ref()
                .ok_or("Forward globals missing")?
                .records()
                .raw()
                .payload(),
        )?;
        lab.wait(|lab| {
            !lab.app.device_operation_pending()
                && !lab.app.external_reconciliation_pending()
                && matches!(
                    lab.app.operation_availability(OperationKind::SetModel),
                    OperationAvailability::Available
                )
        })?;
        lab.phase = "undo";
        lab.input(AppInput::Ui(UiIntent::Undo(device_document_id())))?;
        lab.wait(|lab| lab.settled(module, ModelCode(baseline)))?;
        report["undo_verified"] = true.into();
        let restored = lab
            .app
            .state()
            .confirmed_buffer
            .clone()
            .ok_or("Final Undo current missing")?;
        let restored_globals = lab
            .app
            .state()
            .device_globals
            .clone()
            .ok_or("Final Undo globals missing")?;
        durable_file(
            &output.join("after-undo.payload.bin"),
            restored.raw.payload(),
        )?;
        durable_file(
            &output.join("after-undo-globals.payload.bin"),
            restored_globals.records().raw().payload(),
        )?;
        let whole = complete_preset_equal(&before.raw, &restored.raw);
        let whole_globals = dependent_globals_match(
            globals.records().raw(),
            restored_globals.records().raw(),
            Some(1),
        )
        .map_err(|error| error.message)?;
        report["whole_current_restored"] = whole.into();
        report["globals_restored_including_third_word"] = whole_globals.into();
        let dependent: Vec<_> = lab
            .submitted
            .iter()
            .filter(|command| command["kind"] == "factory-amp-model")
            .collect();
        report["model_command_count"] = dependent.len().into();
        if dependent.len() != 2 || !whole || !whole_globals {
            return Err("Production forward/Undo did not restore the full retained current/global state with exactly two model commands".into());
        }
        Ok(())
    })();
    lab.phase = "shutdown";
    lab.service.shutdown();
    let stopped = lab.service.wait_stopped(Duration::from_secs(3));
    // Drain final diagnostic packets after the worker has released the endpoint.
    while let Some(event) = lab.service.try_next_event() {
        if let DeviceEvent::Diagnostic(diagnostic) = event
            && let Some(bytes) = diagnostic.bytes
        {
            match diagnostic.category.as_str() {
                "midi-tx" => lab.tx.write_all(&bytes),
                "midi-rx" => lab.rx.write_all(&bytes),
                _ => Ok(()),
            }
            .map_err(|error| error.to_string())?;
        }
    }
    lab.trace
        .flush()
        .and_then(|()| lab.trace.sync_all())
        .map_err(|error| error.to_string())?;
    lab.tx
        .sync_all()
        .and_then(|()| lab.rx.sync_all())
        .map_err(|error| error.to_string())?;
    report["service_stopped"] = stopped.into();
    report["error"] = serde_json::json!(result.as_ref().err());
    report["submitted_commands"] = serde_json::json!(lab.submitted);
    report["complete_preset_readbacks"] = serde_json::json!(lab.model_readbacks);
    report["probe_completed"] = (result.is_ok() && stopped).into();
    report["accepted"] = false.into(); // Root independently reviews physical captures and restoration.
    durable_file(
        &output.join("result.json"),
        &serde_json::to_vec_pretty(&report).map_err(|error| error.to_string())?,
    )?;
    println!("{report}");
    if !stopped {
        return Err(
            "MIDI service did not stop within deadline; parent must reap child before recovery"
                .into(),
        );
    }
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

#[cfg(all(test, gp200_private_evidence))]
mod tests {
    use super::*;
    #[test]
    fn dry_profile_ids_are_exact_and_never_normal_accepted() {
        for descriptor in FACTORY_AMP_CANDIDATES {
            for mode in [false, true] {
                let id = FactoryAmpProfileId::new(descriptor.model, mode).unwrap();
                assert_eq!(profile(&id.name()), Some(id));
                assert!(!id.normal_build_accepted());
            }
        }
        for name in [
            "amp-auto-cab-on",
            "wah-assigned-preserve",
            "amp-0700004a-auto-cab-1",
            "amp-07000003-auto-cab-2",
            "amp-07100000-auto-cab-1",
        ] {
            assert!(profile(name).is_none());
        }
    }
    #[test]
    fn complete_before_image_comparison_keeps_all_opaque_module_words_strict() {
        let raw = RawTransfer::from_payload(0x12, include_bytes!("../../../analysis/implementation/dependent-model-root/amp-auto-cab-on/current-before.payload.bin").to_vec()).unwrap();
        let mut records = raw.records().to_vec();
        records[6].body[64] ^= 1;
        assert!(!complete_preset_equal(
            &raw,
            &RawTransfer::from_records(0x12, &records).unwrap()
        ));
        records = raw.records().to_vec();
        records[1].body[15] ^= 1;
        records[26].body[2] ^= 1;
        assert!(complete_preset_equal(
            &raw,
            &RawTransfer::from_records(0x12, &records).unwrap()
        ));
    }
}
