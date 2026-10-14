//! Root-run, finite normal-mode acceptance of the actual native app coordinator.
//! Default/help exits before any backend, file, window, or service construction.
//! Demo checks normal simulator GUI and its deliberately unavailable capture path.
#![forbid(unsafe_code)]

use eframe::{App, egui};
use gp200_domain::*;
use gp200_gui::{FirmwareCoordinatorPhase, FirmwareWorkspaceLaunch, LaunchOptions, StudioApp};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    error::Error,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const PROFILE_SHA: &str = "be69aea7171cd239f2414aa645d46d39af9a5c2ae1e5a115feee825fb971966a";
const IMAGE_SHA: &str = "c368618773097b07c556c951eb10792f9d57be929323366648eb6e083d0d23af";
const NOFOLLOW: i32 = 0x20_000;
const HELP: &str = "Native GP-200 firmware coordinator NORMAL-READ-ONLY acceptance.\n\
Default/help constructs no backend and performs no file or device operations.\n\
Offline: --demo --run-root ABSOLUTE_PRIVATE_DIRECTORY --tested-commit FULL_SHA\n\
Root-only hardware: --hardware --run-root ABSOLUTE_PRIVATE_DIRECTORY\n\
  --tested-commit FULL_SHA --input alsa-raw:in:GP200:0:0\n\
  --output alsa-raw:out:GP200:0:0 --context ABSOLUTE_JSON --context-sha256 SHA256\n\
  --canonical-profile ABSOLUTE_JSON --canonical-profile-sha256 SHA256\n\
  --image ABSOLUTE_BIN --image-sha256 SHA256\n\
Optional: --timeout-seconds 30..360 (default 360).\n\
Runner supplies private XDG_{DATA,CONFIG,STATE,CACHE}_HOME under run-root;\n\
XDG_RUNTIME_DIR keeps the canonical device lock namespace. No HOME override.\n\
Context requires the dated, associated user pedal-display V1.6.3 assertion;\n\
automatic firmware remains Unknown. The accepted profile/image hashes are fixed.\n\
No updater entry, firmware query/transfer, save/select/write/upload/restore,\n\
quarantine clear, automatic retry, or restoration/completion/flash authority.\n\
Demo verifies real normal simulator GUI and Unsupported capture admission only.";

fn error(message: impl Into<String>) -> Box<dyn Error> {
    io::Error::other(message.into()).into()
}
fn hash(bytes: &[u8]) -> String {
    JobSha256::of(bytes).hex()
}
fn sha_valid(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn absolute_plain(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
    {
        return Err(error("Require an absolute path without dot components"));
    }
    Ok(())
}
fn uid() -> Result<u32> {
    let status = fs::read_to_string("/proc/self/status")?;
    Ok(status
        .lines()
        .find(|line| line.starts_with("Uid:"))
        .and_then(|line| line.split_whitespace().nth(2))
        .ok_or_else(|| error("Effective UID unavailable"))?
        .parse()?)
}
fn private_directory(path: &Path, owner: u32) -> Result<()> {
    absolute_plain(path)?;
    for ancestor in path.ancestors() {
        let meta = fs::symlink_metadata(ancestor)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(error(
                "Directory ancestry contains a non-directory or symlink",
            ));
        }
    }
    let meta = fs::symlink_metadata(path)?;
    if meta.uid() != owner || meta.mode() & 0o777 != 0o700 {
        return Err(error(
            "Run and private XDG directories must be owned and mode 0700",
        ));
    }
    Ok(())
}
fn checked_read(path: &Path, bound: u64) -> Result<Vec<u8>> {
    absolute_plain(path)?;
    for ancestor in path.ancestors().skip(1) {
        let meta = fs::symlink_metadata(ancestor)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(error("Input ancestry contains a non-directory or symlink"));
        }
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW)
        .open(path)?;
    let before = file.metadata()?;
    if !before.is_file() || before.len() > bound {
        return Err(error("Input is nonregular or exceeds its bound"));
    }
    let mut bytes = Vec::new();
    (&mut file).take(bound + 1).read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() as u64 != before.len()
        || before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
    {
        return Err(error("Input changed during its bounded read"));
    }
    Ok(bytes)
}

#[derive(Debug)]
struct Options {
    hardware: bool,
    root: PathBuf,
    commit: String,
    timeout: Duration,
    values: BTreeMap<String, String>,
}
impl Options {
    fn parse(args: Vec<String>) -> Result<Option<Self>> {
        if args.is_empty() || args.iter().any(|a| matches!(a.as_str(), "--help" | "-h")) {
            return Ok(None);
        }
        let mut mode = None;
        let mut values = BTreeMap::new();
        let mut args = args.into_iter();
        while let Some(key) = args.next() {
            if matches!(key.as_str(), "--demo" | "--hardware") {
                if mode.replace(key == "--hardware").is_some() {
                    return Err(error("Choose exactly one explicit mode"));
                }
                continue;
            }
            if !matches!(
                key.as_str(),
                "--run-root"
                    | "--tested-commit"
                    | "--timeout-seconds"
                    | "--input"
                    | "--output"
                    | "--context"
                    | "--context-sha256"
                    | "--canonical-profile"
                    | "--canonical-profile-sha256"
                    | "--image"
                    | "--image-sha256"
            ) {
                return Err(error(format!("Unknown option {key}")));
            }
            let value = args.next().ok_or_else(|| error("Missing option value"))?;
            if value.starts_with("--") || values.insert(key, value).is_some() {
                return Err(error("Missing or duplicate option value"));
            }
        }
        let hardware =
            mode.ok_or_else(|| error("Choose --demo or explicit root-only --hardware"))?;
        let root = PathBuf::from(
            values
                .remove("--run-root")
                .ok_or_else(|| error("Missing --run-root"))?,
        );
        absolute_plain(&root)?;
        let commit = values
            .remove("--tested-commit")
            .ok_or_else(|| error("Missing --tested-commit"))?;
        if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(error("Require the full root-supplied baseline commit"));
        }
        let seconds: u64 = values
            .remove("--timeout-seconds")
            .map_or(Ok(360), |v| v.parse())?;
        if !(30..=360).contains(&seconds) {
            return Err(error("Timeout must be from 30 to 360 seconds"));
        }
        if hardware {
            for key in [
                "--input",
                "--output",
                "--context",
                "--context-sha256",
                "--canonical-profile",
                "--canonical-profile-sha256",
                "--image",
                "--image-sha256",
            ] {
                if !values.contains_key(key) {
                    return Err(error(format!("Missing {key}")));
                }
            }
            if !values["--input"].starts_with("alsa-raw:in:")
                || !values["--output"].starts_with("alsa-raw:out:")
                || values["--input"].len() > 128
                || values["--output"].len() > 128
            {
                return Err(error("Require exact ALSA raw input and output IDs"));
            }
            for key in [
                "--context-sha256",
                "--canonical-profile-sha256",
                "--image-sha256",
            ] {
                if !sha_valid(&values[key]) {
                    return Err(error("Invalid lowercase SHA-256 pin"));
                }
            }
        } else if !values.is_empty() {
            return Err(error(
                "Hardware inputs cannot be passed to the offline demo",
            ));
        }
        Ok(Some(Self {
            hardware,
            root,
            commit,
            timeout: Duration::from_secs(seconds),
            values,
        }))
    }
    fn prepare(&self) -> Result<Option<FirmwareBackupReviewContext>> {
        let owner = uid()?;
        private_directory(&self.root, owner)?;
        if std::env::var_os("GP200_STUDIO_DATA").is_some()
            || std::env::var_os("GP200_STUDIO_CONFIG").is_some()
        {
            return Err(error(
                "Reject application data/config overrides; private XDG paths are required",
            ));
        }
        for (variable, leaf) in [
            ("XDG_DATA_HOME", "data"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
        ] {
            let supplied = std::env::var_os(variable).map(PathBuf::from);
            let expected = self.root.join(leaf);
            if supplied.as_ref() != Some(&expected) {
                return Err(error(format!(
                    "{variable} must exactly equal {}",
                    expected.display()
                )));
            }
            private_directory(&expected, owner)?;
            // Xvfb or the graphics driver can populate the private shader cache
            // before main; app data, configuration and state must still be new.
            if leaf != "cache" && fs::read_dir(&expected)?.next().is_some() {
                return Err(error(
                    "Initial private XDG data/config/state directories must be empty",
                ));
            }
        }
        if !self.hardware {
            return Ok(None);
        }
        let profile = checked_read(Path::new(&self.values["--canonical-profile"]), 16 * 1024)?;
        if profile.len() != 5824
            || hash(&profile) != PROFILE_SHA
            || self.values["--canonical-profile-sha256"] != PROFILE_SHA
        {
            return Err(error(
                "Canonical profile differs from the exact accepted conditional source note",
            ));
        }
        let image = checked_read(Path::new(&self.values["--image"]), 6_452_728)?;
        if image.len() != 6_452_728
            || hash(&image) != IMAGE_SHA
            || self.values["--image-sha256"] != IMAGE_SHA
        {
            return Err(error(
                "Local destination image differs from the exact accepted V1.8.2 file",
            ));
        }
        let bytes = checked_read(Path::new(&self.values["--context"]), 128 * 1024)?;
        if hash(&bytes) != self.values["--context-sha256"] {
            return Err(error(
                "Dated manual capture context differs from its explicit pin",
            ));
        }
        let context: FirmwareBackupReviewContext = serde_json::from_slice(&bytes)?;
        context.validate()?;
        if context.manual_version.as_ref().is_none_or(|report| {
            report.version().components() != [1, 6, 3]
                || report.device() != &context.device
                || report.provenance() != FirmwareReportProvenance::UserEnteredPedalDisplay
        }) || context.device.usb_identity().is_none()
            || context.destination.version.components() != [1, 8, 2]
            || context.destination.image_sha256 != IMAGE_SHA
        {
            return Err(error(
                "Require the dated associated pedal-display V1.6.3 assertion, USB serial association and accepted V1.8.2 destination",
            ));
        }
        write_new_json(
            &self.root.join("context-input.json"),
            &serde_json::from_slice::<Value>(&bytes)?,
        )?;
        write_new(&self.root.join("canonical-profile-input.json"), &profile)?;
        Ok(Some(context))
    }
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(error("Output exceeds its bounded size"));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(NOFOLLOW)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    File::open(
        path.parent()
            .ok_or_else(|| error("Missing output parent"))?,
    )?
    .sync_all()?;
    Ok(())
}
fn write_new_json(path: &Path, value: &Value) -> Result<()> {
    write_new(path, &serde_json::to_vec_pretty(value)?)
}
struct Events {
    file: File,
    count: usize,
    bytes: usize,
    started: Instant,
}
impl Events {
    fn new(root: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(NOFOLLOW)
            .open(root.join("events.jsonl"))?;
        file.sync_all()?;
        File::open(root)?.sync_all()?;
        Ok(Self {
            file,
            count: 0,
            bytes: 0,
            started: Instant::now(),
        })
    }
    fn record(&mut self, kind: &str, payload: Value) -> Result<()> {
        let bytes = serde_json::to_vec(
            &json!({"sequence": self.count + 1, "elapsed_ms": self.started.elapsed().as_millis(), "kind":kind, "payload":payload}),
        )?;
        if self.count >= 4096 || self.bytes + bytes.len() > 32 * 1024 * 1024 {
            return Err(error("Acceptance event output exhausted its finite bound"));
        }
        self.file.write_all(&bytes)?;
        self.file.write_all(b"\n")?;
        self.file.sync_all()?;
        self.count += 1;
        self.bytes += bytes.len() + 1;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Discover,
    Connecting,
    Globals,
    Preparing,
    Capturing,
    LocalReport,
    Returning,
    Complete,
}
struct Acceptance {
    studio: StudioApp,
    options: Options,
    context: Option<FirmwareBackupReviewContext>,
    events: Events,
    stage: Stage,
    stage_started: Instant,
    last_attempt: Instant,
    last_projection: (FirmwareCoordinatorPhase, usize, usize, SessionEpoch),
    initial_epoch: Option<SessionEpoch>,
    initial_selection: Option<DeviceSelection>,
    before: Option<Value>,
    result: Arc<Mutex<Option<Value>>>,
    complete: bool,
    logic_frames: usize,
    ui_frames: usize,
}
impl Acceptance {
    fn send(&mut self, intent: UiIntent) -> Result<()> {
        if !matches!(
            &intent,
            UiIntent::DiscoverDevices | UiIntent::Connect(_) | UiIntent::RefreshDeviceGlobals
        ) {
            return Err(error("Harness rejected a non-read-only intent"));
        }
        let effects = self.studio.controller.dispatch(AppInput::Ui(intent));
        if effects.iter().any(|effect| {
            !matches!(
                effect,
                Effect::Device(
                    DeviceCommand::Discover
                        | DeviceCommand::Connect { .. }
                        | DeviceCommand::ReadIdentity(_)
                        | DeviceCommand::ReadCurrent(_)
                        | DeviceCommand::ReadGlobals(_)
                )
            )
        }) {
            return Err(error("Harness rejected an unexpected dispatched effect"));
        }
        self.studio.services.enqueue(effects);
        Ok(())
    }
    fn change(&mut self, stage: Stage) -> Result<()> {
        self.stage = stage;
        self.stage_started = Instant::now();
        self.events
            .record("harness-stage", json!({"stage": format!("{stage:?}")}))
    }
    fn snapshot(&self) -> Value {
        let state = self.studio.controller.state();
        let view = self.studio.firmware_coordinator_view();
        json!({
            "phase":format!("{:?}",view.phase), "generation":view.generation,
            "records":view.records, "blockers":view.blockers, "diagnostics":view.diagnostics,
            "omitted_diagnostics":view.omitted_diagnostics, "capture":view.capture,
            "backup":view.backup, "preparation":view.preparation,
            "owner_description":view.owner.map(|v|format!("{v:?}")),
            "permits_flash":view.permits_flash, "completion_verified":view.completion_verified,
            "complete_restorable_backup":false, "close_blocked":view.close_blocked,
            "can_return_to_editing":view.can_return_to_editing,
            "can_retry_library_restore":view.can_retry_library_restore,
            "connection":state.connection, "epoch":state.epoch,
            "identity":state.identity, "current":state.confirmed_buffer,
            "globals":state.device_globals, "session":state.device_job_session.as_ref().map(|s|json!({"selection":s.selection(),"identity":s.identity()})),
            "library_service_present":self.studio.services.library.is_some(),
            "workflow_service_available":self.studio.services.workflow_available(),
            "normal_runtime_pending":self.studio.services.has_pending(),
            "runtime_errors":self.studio.services.errors,
            "app_messages":state.messages.iter().map(|m|json!({"sequence":m.sequence,"severity":format!("{:?}",m.severity),"message":m.message,"error":m.error})).collect::<Vec<_>>(),
            "app_diagnostics":state.diagnostics,
            "actual_native_logic_frames":self.logic_frames,
            "actual_native_ui_frames":self.ui_frames,
        })
    }
    fn observe(&mut self) -> Result<()> {
        let view = self.studio.firmware_coordinator_view();
        let key = (
            view.phase,
            view.records / 25,
            view.diagnostics.len(),
            self.studio.controller.state().epoch,
        );
        if key != self.last_projection {
            self.events.record("actual-coordinator-projection", json!({
                "phase":format!("{:?}",view.phase), "generation":view.generation,
                "original_record_count":view.records, "blockers":view.blockers,
                "diagnostics":view.diagnostics,"omitted_diagnostics":view.omitted_diagnostics,
                "capture":view.capture,"can_return_to_editing":view.can_return_to_editing,
                "can_retry_library_restore":view.can_retry_library_restore,
                "close_blocked":view.close_blocked,"epoch":key.3,
                "permits_flash":view.permits_flash,"completion_verified":view.completion_verified,
            }))?;
            self.last_projection = key;
        }
        if view.permits_flash || view.completion_verified {
            return Err(error("Unexpected physical authority/completion projection"));
        }
        if view.phase == FirmwareCoordinatorPhase::RecoveryRequired {
            return Err(error(
                "Actual coordinator requires recovery; original ownership remains held",
            ));
        }
        Ok(())
    }
    fn drive(&mut self, ctx: &egui::Context) -> Result<()> {
        self.observe()?;
        if self.events.started.elapsed() >= self.options.timeout {
            return Err(error(
                "Overall acceptance deadline exhausted; no retry or safe-normal claim",
            ));
        }
        let phase = self.studio.firmware_coordinator_view().phase;
        match self.stage {
            Stage::Discover => {
                if !self.options.hardware {
                    self.change(Stage::Connecting)?;
                } else {
                    let state = self.studio.controller.state();
                    let matches: Vec<_> = state
                        .ports
                        .iter()
                        .filter(|p| {
                            p.selection.input_id == self.options.values["--input"]
                                && p.selection.output_id == self.options.values["--output"]
                        })
                        .collect();
                    if matches.len() > 1 {
                        return Err(error("Selected exact endpoint is ambiguous"));
                    }
                    if let Some(port) = matches.first() {
                        let selection = port.selection.clone();
                        self.events
                            .record("actual-selected-endpoint", json!({"candidate":port}))?;
                        self.initial_selection = Some(selection.clone());
                        self.send(UiIntent::Connect(selection))?;
                        self.change(Stage::Connecting)?;
                    } else if self.stage_started.elapsed() > Duration::from_secs(15) {
                        return Err(error(
                            "Exact selected endpoint was not discovered; no alternate selection",
                        ));
                    }
                }
            }
            Stage::Connecting => {
                let state = self.studio.controller.state();
                if state.connection == ConnectionState::Ready
                    && state.confirmed_buffer.is_some()
                    && let Some(identity) = state.identity.as_ref()
                {
                    let mode = identity.mode;
                    if mode
                        != if self.options.hardware {
                            BackendMode::Hardware
                        } else {
                            BackendMode::Simulator
                        }
                    {
                        return Err(error(
                            "Actual initial identity backend differs from explicit run mode",
                        ));
                    }
                    if identity.model != DeviceModel::Gp200 {
                        return Err(error("Expected the original GP-200 model"));
                    }
                    self.initial_epoch = Some(state.epoch);
                    self.send(UiIntent::RefreshDeviceGlobals)?;
                    self.change(Stage::Globals)?;
                } else if self.stage_started.elapsed() > Duration::from_secs(20) {
                    return Err(error("Fresh normal identity/current did not become ready"));
                }
            }
            Stage::Globals => {
                let state = self.studio.controller.state();
                if state
                    .device_globals
                    .as_ref()
                    .is_some_and(|g| g.epoch == state.epoch)
                    && !self.studio.services.has_pending()
                {
                    self.before = Some(self.snapshot());
                    write_new_json(
                        &self.options.root.join("normal-before.json"),
                        self.before.as_ref().expect("just recorded"),
                    )?;
                    self.change(Stage::Preparing)?;
                } else if self.stage_started.elapsed() > Duration::from_secs(20) {
                    return Err(error("Fresh normal globals did not settle"));
                }
            }
            Stage::Preparing => {
                // Busy returns mean this exact action was not accepted. Only original
                // queue/recovery settlement is awaited; no accepted operation is retried.
                if self.last_attempt.elapsed() >= Duration::from_millis(100) {
                    self.last_attempt = Instant::now();
                    match self.studio.begin_firmware_preparation() {
                        Ok(()) if self.options.hardware => self.change(Stage::Capturing)?,
                        Err(e) if !self.options.hardware && e.kind == ErrorKind::Unsupported => {
                            self.events.record("actual-demo-unsupported-capture-admission", json!({"error":e,"capture_endpoint_fabricated":false,"hardware_backend_constructed":false}))?;
                            if self.studio.firmware_coordinator_view().phase
                                != FirmwareCoordinatorPhase::Idle
                                || self.studio.firmware_close_blocked()
                            {
                                return Err(error(
                                    "Unsupported demo admission changed original ownership",
                                ));
                            }
                            self.finish(ctx, "offline_normal_gui_and_unsupported_capture", true)?;
                        }
                        Err(e)
                            if e.kind == ErrorKind::Busy
                                && self.stage_started.elapsed() < Duration::from_secs(20) => {}
                        Err(e) => return Err(e.into()),
                        Ok(()) => {
                            return Err(error(
                                "Demo unexpectedly acquired a concrete capture endpoint",
                            ));
                        }
                    }
                }
            }
            Stage::Capturing => {
                if phase == FirmwareCoordinatorPhase::Preparing
                    && self.last_attempt.elapsed() >= Duration::from_millis(100)
                {
                    self.last_attempt = Instant::now();
                    match self.studio.capture_firmware_backup(self.context.clone().ok_or_else(||error("Hardware context absent"))?) {
                        Ok(()) => self.events.record("capture-admitted", json!({"genuine_coordinator_action":true,"command_retries_after_acceptance":0}))?,
                        Err(e) if e.kind == ErrorKind::Busy && self.stage_started.elapsed() < Duration::from_secs(20) => {}
                        Err(e) => return Err(e.into()),
                    }
                } else if phase == FirmwareCoordinatorPhase::ReadOnlyReview {
                    let view = self.studio.firmware_coordinator_view();
                    let capture = view
                        .capture
                        .as_ref()
                        .ok_or_else(|| error("Genuine published evidence summary absent"))?;
                    let backup = view
                        .backup
                        .as_ref()
                        .ok_or_else(|| error("Genuine revalidated archival summary absent"))?;
                    if view.records != 561
                        || capture.records != 563
                        || !capture.closed
                        || backup.stored_presets != 256
                        || !backup.current_buffer
                        || !backup.globals_and_mappings
                        || backup
                            .coverage
                            .iter()
                            .filter(|v| matches!(v.object, FirmwareBackupObject::IrMetadata(_)))
                            .count()
                            != 20
                        || backup
                            .coverage
                            .iter()
                            .filter(|v| matches!(v.object, FirmwareBackupObject::IrMetadata(_)))
                            .any(|v| {
                                v.installed_content_available
                                    || v.restoration != FirmwareRestorationStatus::UnavailableSource
                            })
                    {
                        return Err(error(
                            "Original read-only acquisition/publication summary is incomplete or misstates IR content",
                        ));
                    }
                    write_new_json(
                        &self.options.root.join("capture-review.json"),
                        &self.snapshot(),
                    )?;
                    self.studio.prepare_local_firmware_review()?;
                    self.change(Stage::LocalReport)?;
                }
            }
            Stage::LocalReport => {
                if phase == FirmwareCoordinatorPhase::ReadOnlyReview
                    && let Some(report) = self.studio.firmware_coordinator_view().preparation
                {
                    if report.permits_flash
                        || report.completion_verified
                        || report.image_sha256 != IMAGE_SHA
                    {
                        return Err(error("Invalid local descriptive preparation projection"));
                    }
                    write_new_json(
                        &self.options.root.join("local-preparation.json"),
                        &serde_json::to_value(&report)?,
                    )?;
                    self.studio.return_from_firmware_preparation()?;
                    self.change(Stage::Returning)?;
                } else if self.stage_started.elapsed() > Duration::from_secs(30) {
                    return Err(error(
                        "Genuine local preparation did not complete; no return request was fabricated",
                    ));
                }
            }
            Stage::Returning => {
                if phase == FirmwareCoordinatorPhase::Idle {
                    let state = self.studio.controller.state();
                    let initial = self
                        .initial_epoch
                        .ok_or_else(|| error("Original source epoch absent"))?;
                    if state.connection != ConnectionState::Ready || state.epoch <= initial
                        || state.identity.as_ref().is_none_or(|i|i.mode != BackendMode::Hardware || i.model != DeviceModel::Gp200)
                        || state.confirmed_buffer.as_ref().is_none_or(|p|!matches!(p.source,SnapshotSource::Hardware{epoch} if epoch == state.epoch))
                        || state.device_globals.as_ref().is_none_or(|g|g.epoch != state.epoch)
                        || state.device_job_session.as_ref().is_none_or(|s|Some(s.selection()) != self.initial_selection.as_ref())
                        || !self.studio.services.workflow_available() || self.studio.services.library.is_none()
                        || self.studio.firmware_close_blocked()
                    { return Err(error("Actual same-store return/fresh normal identity-current-globals boundary is not ready")); }
                    self.finish(ctx, "hardware_normal_capture_and_same_store_return", true)?;
                } else if self.stage_started.elapsed() > Duration::from_secs(30) {
                    return Err(error(
                        "Original return did not reach fresh normal readiness",
                    ));
                }
            }
            Stage::Complete => {}
        }
        ctx.request_repaint_after(Duration::from_millis(16));
        Ok(())
    }
    fn finish(&mut self, ctx: &egui::Context, outcome: &str, passed: bool) -> Result<()> {
        if self.logic_frames == 0 || self.ui_frames == 0 {
            return Err(error("Native app logic and UI have not both actually run"));
        }
        let result = json!({"schema":1,"outcome":outcome,"passed":passed,"hardware_mode":self.options.hardware,
            "offline_scope":"Actual normal simulator GUI and Unsupported capture admission only; full concrete replay capture already covered by separate core tests",
            "actual_native_frames_delegated":{"logic":self.logic_frames,"ui":self.ui_frames},"baseline_commit_supplied_by_runner":self.options.commit, "commit_is_informational":"Exact original source/build input pins are authoritative; an overlaid working tree is not certified by this CLI string",
            "elapsed_ms":self.events.started.elapsed().as_millis(),"source_epoch":self.initial_epoch,
            "normal_before":self.before,"final":self.snapshot(),"firmware_entry_offers":0,"firmware_transfer_offers":0,
            "harness_device_mutation_intents":0,"complete_restorable_backup":false,"permits_flash":false,"completion_verified":false,
            "original_archive_policy":"Original strict failure/cancelled status retained; conditional descriptive consistency does not promote restoration readiness",
            "receipt_visibility":"Coordinator view is descriptive; original durable receipt descriptions and raw evidence remain in the private XDG capture publication, preserved by the runner"});
        write_new_json(&self.options.root.join("result.json"), &result)?;
        self.events.record(
            "acceptance-result",
            json!({"passed":passed,"outcome":outcome}),
        )?;
        *self
            .result
            .lock()
            .map_err(|_| error("Result cell poisoned"))? = Some(result);
        self.change(Stage::Complete)?;
        self.complete = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        Ok(())
    }
    fn fail(&mut self, cause: Box<dyn Error>) -> ! {
        // Stop admission while retaining the actual coordinator's owner. Do not
        // claim that process termination closes an uncertain owner safely.
        let original = self.snapshot();
        self.studio.stop_firmware_preparation();
        let observation = json!({"schema":1,"passed":false,"error":cause.to_string(),"stage":format!("{:?}",self.stage),"original":original,"after_admission_stop":self.snapshot(),"safe_normal_claimed":false,"automatic_retry":false,"process_termination":"Finite diagnostic ends with admission stopped; OS teardown is not a genuine normal-return receipt"});
        let _ = self
            .events
            .record("original-failed-boundary", observation.clone());
        let _ = write_new_json(&self.options.root.join("result.json"), &observation);
        eprintln!("{observation}");
        std::process::exit(2)
    }
}
impl App for Acceptance {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // This acceptance window cannot admit edits through keyboard shortcuts.
        ctx.input_mut(|input| {
            input.events.clear();
            input.keys_down.clear();
            input.pointer = Default::default();
        });
        self.logic_frames += 1;
        self.studio.logic(ctx, frame);
        if !self.complete
            && let Err(cause) = self.drive(ctx)
        {
            self.fail(cause);
        }
    }
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.ui_frames += 1;
        ui.disable();
        self.studio.ui(ui, frame);
    }
    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        self.studio.on_exit(gl);
    }
}

fn main() -> Result<()> {
    let Some(options) = Options::parse(std::env::args().skip(1).collect())? else {
        println!("{HELP}");
        return Ok(());
    };
    let context = options.prepare()?;
    let executable = std::env::current_exe()?;
    let bytes = checked_read(&executable, 768 * 1024 * 1024)?;
    let process = fs::read_to_string("/proc/self/stat")?;
    let start = process
        .rsplit_once(')')
        .and_then(|(_, tail)| tail.split_whitespace().nth(19))
        .ok_or_else(|| error("Process start identity unavailable"))?;
    let mut events = Events::new(&options.root)?;
    events.record("original-run-provenance", json!({"pid":std::process::id(),"process_start_ticks":start,"exe":executable,"exe_bytes":bytes.len(),"exe_sha256":hash(&bytes),"example_source_sha256":hash(include_bytes!("firmware_coordinator_readonly_acceptance.rs")),"gui_manifest_sha256":hash(include_bytes!("../Cargo.toml")),"options":format!("{options:?}"),"hardware_mode":options.hardware,"actual_gui_bootstrap":"StudioApp::new; real eframe App::logic/ui","no_home_override_by_harness":true,"canonical_device_runtime_preserved":std::env::var_os("XDG_RUNTIME_DIR"),"manual_version_is_an_assertion":true,"physical_update_authority":false}))?;
    drop(bytes);
    let mut launch = LaunchOptions {
        demo: !options.hardware,
        theme: Some(ThemePreference::Dark),
        view: Some(ViewId::Diagnostics),
        ..Default::default()
    };
    if options.hardware {
        launch.firmware_workspace = Some(FirmwareWorkspaceLaunch {
            source: PathBuf::from(&options.values["--image"]),
            identity_model: DeviceModel::Gp200,
            chunk_bytes: 128,
            simulation: None,
        });
    }
    let result = Arc::new(Mutex::new(None));
    let returned = result.clone();
    eframe::run_native(
        "GP-200 · bounded read-only coordinator acceptance",
        eframe::NativeOptions {
            persist_window: false,
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1280.0, 800.0])
                .with_min_inner_size([640.0, 480.0]),
            glow_options: eframe::egui_glow::GlowConfiguration {
                vsync: false,
                ..Default::default()
            },
            ..Default::default()
        },
        Box::new(move |cc| {
            let mut studio = StudioApp::new(cc, launch)?;
            if options.hardware {
                let effects = studio
                    .controller
                    .dispatch(AppInput::Ui(UiIntent::DiscoverDevices));
                if !effects
                    .iter()
                    .all(|e| matches!(e, Effect::Device(DeviceCommand::Discover)))
                {
                    return Err("Unexpected bootstrap discovery effects".into());
                }
                studio.services.enqueue(effects);
            }
            Ok(Box::new(Acceptance {
                studio,
                options,
                context,
                events,
                stage: Stage::Discover,
                stage_started: Instant::now(),
                last_attempt: Instant::now(),
                last_projection: (FirmwareCoordinatorPhase::Idle, 0, 0, SessionEpoch(0)),
                initial_epoch: None,
                initial_selection: None,
                before: None,
                result,
                complete: false,
                logic_frames: 0,
                ui_frames: 0,
            }))
        }),
    )?;
    let result = returned.lock().map_err(|_| error("Result cell poisoned"))?;
    let value = result
        .as_ref()
        .ok_or_else(|| error("Native window ended before a genuine terminal acceptance result"))?;
    println!(
        "{}",
        json!({"passed":value["passed"],"outcome":value["outcome"],"firmware_entry_offers":0,"firmware_transfer_offers":0})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_help_and_missing_explicit_modes_cannot_construct_a_backend() {
        assert!(Options::parse(Vec::new()).unwrap().is_none());
        assert!(Options::parse(vec!["--help".into()]).unwrap().is_none());
        assert!(Options::parse(vec!["--run-root".into(), "/unused".into()]).is_err());
        assert!(Options::parse(vec!["--demo".into(), "--hardware".into()]).is_err());
    }

    #[test]
    fn hardware_requires_exact_endpoint_and_all_explicit_material_pins() {
        let base = vec![
            "--hardware".into(),
            "--run-root".into(),
            "/unused".into(),
            "--tested-commit".into(),
            "0".repeat(40),
        ];
        assert!(Options::parse(base).is_err());
        let demo = vec![
            "--demo".into(),
            "--run-root".into(),
            "/unused".into(),
            "--tested-commit".into(),
            "0".repeat(40),
            "--input".into(),
            "alsa-raw:in:GP200:0:0".into(),
        ];
        assert!(Options::parse(demo).is_err());
    }

    #[test]
    fn overall_deadline_and_plain_output_paths_are_bounded_before_bootstrap() {
        for seconds in ["0", "29", "361", "18446744073709551615"] {
            assert!(
                Options::parse(vec![
                    "--demo".into(),
                    "--run-root".into(),
                    "/unused".into(),
                    "--tested-commit".into(),
                    "0".repeat(40),
                    "--timeout-seconds".into(),
                    seconds.into(),
                ])
                .is_err()
            );
        }
        assert!(absolute_plain(Path::new("relative")).is_err());
        assert!(absolute_plain(Path::new("/tmp/../elsewhere")).is_err());
    }
}
