mod asset_job_workflow;
mod firmware_device_bridge;
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
mod firmware_resources;
mod firmware_update_coordinator;
mod firmware_workflow;
pub use firmware_update_coordinator::{FirmwareCoordinatorPhase, FirmwareCoordinatorView};
pub use firmware_workflow::LocalFirmwarePreparation;
pub mod localization;
pub mod runtime;
pub mod theme;
mod ui;
mod workflow;
pub use ui::asset_job_review::AssetJobWorkspaceLaunch;
pub use ui::assignments::AssignmentTab;
pub use ui::firmware::{FirmwareSimulation, FirmwareWorkspaceLaunch};
pub use ui::globals::GlobalTab;
pub use ui::help::HelpTopic;
pub use ui::library_workflows::LibraryTab;
pub use ui::nam_assets::{NamOutput, NamWorkspaceLaunch};
#[cfg(test)]
mod external_changes_tests;
#[cfg(test)]
mod keyboard_accessibility_tests;
#[cfg(test)]
mod localization_tests;
#[cfg(test)]
mod numeric_document_isolation_tests;
#[cfg(test)]
mod numeric_entry_tests;
mod preferences;
mod recovery;

use eframe::egui;
use gp200_app::{AppController, ComparisonSlot};
use gp200_domain::*;
use runtime::{FilePurpose, RuntimeServices};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Default, Clone)]
pub struct LaunchOptions {
    pub qa_result: Arc<std::sync::atomic::AtomicI32>,
    pub demo: bool,
    pub connect: bool,
    pub open: Option<std::path::PathBuf>,
    pub scan_bank: bool,
    pub capture_device_backup: bool,
    pub read_globals: bool,
    pub read_ir_metadata: bool,
    pub qa_global_tab: Option<GlobalTab>,
    pub qa_library_tab: Option<LibraryTab>,
    pub qa_ir_history: Option<Option<IrAttemptId>>,
    pub qa_performance_arrange: bool,
    pub qa_selected_module: Option<ModuleId>,
    pub qa_model_picker: bool,
    pub qa_assignment_tab: Option<AssignmentTab>,
    pub qa_patch_inspector: bool,
    pub theme: Option<ThemePreference>,
    pub scale: Option<f32>,
    pub view: Option<ViewId>,
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub screenshot: Option<std::path::PathBuf>,
    pub nam_workspace: Option<NamWorkspaceLaunch>,
    pub firmware_workspace: Option<FirmwareWorkspaceLaunch>,
}
impl LaunchOptions {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut options = Self::default();
        let mut args = args.into_iter();
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--demo" => options.demo = true,
                "--connect" => options.connect = true,
                "--scan-bank" => options.scan_bank = true,
                "--capture-device-backup" => options.capture_device_backup = true,
                "--qa-model-picker" => options.qa_model_picker = true,
                "--qa-patch-inspector" => options.qa_patch_inspector = true,
                "--read-globals" => options.read_globals = true,
                "--read-ir-metadata" => options.read_ir_metadata = true,
                "--qa-library-tab" => {
                    options.qa_library_tab = Some(match args.next().as_deref() {
                        Some("presets") => LibraryTab::Presets,
                        Some("backups") => LibraryTab::Backups,
                        Some("compare") => LibraryTab::Compare,
                        Some("merge") => LibraryTab::Merge,
                        _ => {
                            return Err(
                                "--qa-library-tab requires presets|backups|compare|merge".into()
                            );
                        }
                    });
                }
                "--qa-performance-arrange" => options.qa_performance_arrange = true,
                "--qa-ir-history" => {
                    let value = args.next().ok_or(
                        "--qa-ir-history requires list or a 32-digit hexadecimal attempt ID",
                    )?;
                    options.qa_ir_history = Some(if value == "list" {
                        None
                    } else {
                        Some(IrAttemptId::parse(&value).map_err(|error| error.message)?)
                    });
                }
                "--qa-global-tab" => {
                    let value = args.next().ok_or(
                        "--qa-global-tab requires audio|midi|external|footswitch|eq|display",
                    )?;
                    options.qa_global_tab = Some(GlobalTab::parse(&value).ok_or(
                        "--qa-global-tab requires audio|midi|external|footswitch|eq|display",
                    )?);
                }
                "--qa-selected-module" => {
                    let value = args
                        .next()
                        .ok_or("--qa-selected-module requires a module name or ID 0..10")?;
                    let id = ModuleId::all().find(|module| module.name().eq_ignore_ascii_case(&value))
                        .or_else(|| value.parse::<u8>().ok().and_then(|id| ModuleId::new(id).ok()))
                        .ok_or("--qa-selected-module requires PRE, WAH, DST, AMP, NR, CAB, EQ, MOD, DLY, RVB, VOL or 0..10")?;
                    options.qa_selected_module = Some(id);
                }
                "--qa-assignment-tab" => {
                    let value = args
                        .next()
                        .ok_or("--qa-assignment-tab requires expression|ctrl|quick|routing")?;
                    options.qa_assignment_tab = Some(
                        ui::assignments::AssignmentTab::parse(&value)
                            .ok_or("--qa-assignment-tab requires expression|ctrl|quick|routing")?,
                    );
                }
                "--open" => {
                    options.open = Some(std::path::PathBuf::from(
                        args.next()
                            .ok_or("--open requires a preset or portable bundle path")?,
                    ))
                }
                "--screenshot" => {
                    options.screenshot = Some(std::path::PathBuf::from(
                        args.next()
                            .ok_or("--screenshot requires a PNG output path")?,
                    ))
                }
                "--scale" => {
                    let value: f32 = args
                        .next()
                        .ok_or("--scale requires 1..2")?
                        .parse()
                        .map_err(|_| "--scale requires 1..2")?;
                    if !value.is_finite() || !(1.0..=2.0).contains(&value) {
                        return Err("--scale requires a finite value from 1 to 2".into());
                    }
                    options.scale = Some(value);
                }
                "--theme" => {
                    options.theme = Some(match args.next().as_deref() {
                        Some("dark") => ThemePreference::Dark,
                        Some("light") => ThemePreference::Light,
                        Some("system") => ThemePreference::System,
                        _ => return Err("--theme requires dark, light or system".into()),
                    })
                }
                "--view" => {
                    options.view = Some(match args.next().as_deref() {
                        Some("editor") => ViewId::Editor,
                        Some("library") => ViewId::Library,
                        Some("assets") => ViewId::Assets,
                        Some("assignments") => ViewId::Assignments,
                        Some("device") => ViewId::GlobalSettings,
                        Some("performance") => ViewId::Performance,
                        Some("practice") => ViewId::Practice,
                        Some("diagnostics") => ViewId::Diagnostics,
                        Some("settings") => ViewId::AppSettings,
                        Some("help") => ViewId::Help,
                        _ => return Err(crate::localization::text("Unknown --view").into()),
                    })
                }
                "--size" => {
                    let size = args.next().ok_or("--size requires WIDTHxHEIGHT")?;
                    let (width, height) =
                        size.split_once('x').ok_or("--size requires WIDTHxHEIGHT")?;
                    options.width = Some(
                        width
                            .parse()
                            .map_err(|_| crate::localization::text("Invalid width"))?,
                    );
                    options.height = Some(
                        height
                            .parse()
                            .map_err(|_| crate::localization::text("Invalid height"))?,
                    );
                    if options.width.is_some_and(|w| !w.is_finite() || w < 640.0)
                        || options.height.is_some_and(|h| !h.is_finite() || h < 480.0)
                    {
                        return Err(
                            crate::localization::text("Minimum window size is 640x480").into()
                        );
                    }
                }
                _ => {
                    return Err(std::format!("Unknown argument {argument}"));
                }
            }
        }
        if options.demo && options.connect {
            return Err(crate::localization::text(
                "Choose --demo or --connect. The demo never opens a hardware port.",
            )
            .into());
        }
        if options.scan_bank && !options.demo && !options.connect {
            return Err("--scan-bank requires --demo or --connect".into());
        }
        if options.capture_device_backup && !options.demo && !options.connect {
            return Err("--capture-device-backup requires --demo or --connect".into());
        }
        if options.read_globals && !options.demo && !options.connect {
            return Err("--read-globals requires --demo or --connect".into());
        }
        if options.read_ir_metadata && !options.demo && !options.connect {
            return Err("--read-ir-metadata requires --demo or --connect".into());
        }
        Ok(options)
    }
}

pub struct StudioApp {
    pub controller: AppController,
    pub services: RuntimeServices,
    recovery_ui: recovery::RecoveryUi,
    firmware_coordinator: firmware_update_coordinator::FirmwareUpdateCoordinator,
    firmware_close_request_pending: bool,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    firmware_settled_recovery_close_requested: bool,
    deferred_file_selections: std::collections::VecDeque<runtime::FileSelection>,
    omitted_file_selections: u64,
    help_topic: HelpTopic,
    selected_module: ModuleId,
    qa_patch_inspector: bool,
    control_state: ui::controls::ControlState,
    browser_search: String,
    catalog_search: String,
    model_picker: bool,
    save_review: bool,
    save_slot: u16,
    connect_dialog: bool,
    selected_port: usize,
    slot_preview: u16,
    selected_library: Option<LibraryId>,
    library_search: String,
    favorites_only: bool,
    library_trash: bool,
    library_metadata: LibraryMetadata,
    library_tags: String,
    trashed_library: Option<LibraryId>,
    selected_asset_kind: AssetKind,
    selected_asset_slot: u8,
    asset_slot_ui: ui::asset_slots::AssetSlotUi,
    asset_path: String,
    asset_options: workflow::AssetOptions,
    nam_ui: ui::nam_assets::NamUiState,
    firmware_ui: ui::firmware::FirmwareUiState,
    asset_job_ui: ui::asset_job_review::AssetJobUi,
    ir_history_ui: ui::ir_history::IrHistoryUi,
    asset_review: Option<AssetJobSpec>,
    asset_export_pending: Option<AssetInfo>,
    local_asset_jobs: std::collections::BTreeSet<JobId>,
    local_library_jobs: std::collections::BTreeSet<JobId>,
    native_export_mode: NativeExportMode,
    native_export_review: bool,
    native_export_pending: Option<(LibraryVersionId, NativeExportMode)>,
    bank_scan: workflow::BankAcquisition,
    stored_write_review: Option<StoredWriteReview>,
    stored_write_wait: Option<StoredWriteWait>,
    pending_switch: Option<PendingSwitch>,
    switch_wait: Option<SwitchWait>,
    connect_qa: Option<ConnectQa>,
    startup_import: Option<IntentId>,
    bank_qa: Option<BankQa>,
    capture_backup_qa: Option<GlobalQa>,
    qa_result: Arc<std::sync::atomic::AtomicI32>,
    assignment_state: ui::assignments::AssignmentState,
    global_state: ui::globals::GlobalUiState,
    global_qa: Option<GlobalQa>,
    ir_metadata_qa: Option<GlobalQa>,
    library_workflow: ui::library_workflows::LibraryWorkflowUi,
    selective_merge: ui::selective_merge::SelectiveMergeUi,
    performance_locked: bool,
    performance_compact: bool,
    started: Instant,
    next_local_intent: u64,
    next_job: u64,
    last_message: u64,
    notice: String,
    screenshot_path: Option<std::path::PathBuf>,
    screenshot_frames: usize,
    screenshot_asset_settled: Option<Instant>,
    screenshot_requested: bool,
}
#[derive(Clone)]
enum PendingSwitch {
    Document(DocumentId),
    Library(LibraryVersionId),
    Snapshot(Arc<PresetSnapshot>),
    Audition(SlotId),
}
enum SwitchWait {
    Saving {
        intent: IntentId,
        action: PendingSwitch,
    },
    Discarding {
        document: DocumentId,
        action: PendingSwitch,
    },
}
struct ConnectQa {
    started: Instant,
    attempted: bool,
    ready: bool,
    failed: bool,
}
struct GlobalQa {
    started: Option<Instant>,
    ready: bool,
    failed: bool,
}
struct BankQa {
    started: Option<Instant>,
    failed: bool,
}
#[derive(Clone)]
struct StoredWriteReview {
    clear: bool,
    slot: SlotId,
    expected: Revision,
    source: Arc<PresetSnapshot>,
    before: Arc<PresetSnapshot>,
}
enum StoredWriteWait {
    Backup {
        intent: IntentId,
        review: StoredWriteReview,
    },
    Source {
        intent: IntentId,
        review: StoredWriteReview,
    },
    Writing {
        stamp: RequestStamp,
        slot: SlotId,
    },
}
impl StudioApp {
    pub fn new(cc: &eframe::CreationContext<'_>, options: LaunchOptions) -> Result<Self, String> {
        theme::install_fonts(&cc.egui_ctx);
        let (mut preferences, preference_notice) = preferences::load_preferences_report();
        if let Some(theme) = options.theme {
            preferences.theme = theme;
        }
        if let Some(scale) = options.scale {
            preferences.scale = scale;
        }
        let initial_snapshot = if options.demo {
            gp200_sim::public_demo_snapshot()
        } else {
            gp200_sim::captured_demo_snapshot()
        };
        let controller = AppController::with_demo(initial_snapshot, preferences.clone())
            .map_err(|error| error.to_string())?;
        let context = cc.egui_ctx.clone();
        let wake: WakeUi = Arc::new(move || context.request_repaint());
        let mut services = if options.demo {
            RuntimeServices::with_preferences(
                Box::new(gp200_sim::simulator_service(wake.clone())),
                wake.clone(),
                preferences,
            )
        } else {
            RuntimeServices::with_midi_preferences(
                gp200_transport::MidiDeviceService::new(wake.clone()),
                wake.clone(),
                preferences,
            )
        };
        if let Some(notice) = preference_notice {
            services.error(notice);
        }
        match gp200_assets::AssetWorker::open_xdg(wake.clone()) {
            Ok(worker) => services.assets = Some(Box::new(worker)),
            Err(error) => services.error(std::format!("Asset store unavailable: {error}")),
        }
        match gp200_library::data_dir().and_then(|root| {
            gp200_library::DraftRecoveryWorker::spawn(root.join("draft-recovery"), wake.clone())
        }) {
            Ok(worker) => services.draft_recovery = Some(worker),
            Err(error) => services.error(std::format!(
                "Draft recovery unavailable: {}",
                error.message
            )),
        }
        match gp200_library::LibraryWorker::open_xdg(wake) {
            Ok(library) => {
                services.install_library_worker(library);
            }
            Err(error) => {
                services.error(std::format!("Cannot open local library: {}", error.message))
            }
        }
        let mut app = Self::with_services(controller, services);
        app.model_picker = options.qa_model_picker;
        app.qa_patch_inspector = options.qa_patch_inspector;
        if let Some(tab) = options.qa_library_tab {
            app.library_workflow.tab = tab;
        }
        app.library_workflow.arranging = options.qa_performance_arrange;
        if let Some(module) = options.qa_selected_module {
            app.selected_module = module;
        }
        if let Some(tab) = options.qa_assignment_tab {
            app.assignment_state.tab = tab;
        }
        if let Some(tab) = options.qa_global_tab {
            app.global_state.tab = tab;
        }
        if options.capture_device_backup {
            app.capture_backup_qa = Some(GlobalQa {
                started: None,
                ready: false,
                failed: false,
            });
        }
        if options.read_globals {
            app.global_qa = Some(GlobalQa {
                started: None,
                ready: false,
                failed: false,
            });
        }
        if options.read_ir_metadata {
            app.ir_metadata_qa = Some(GlobalQa {
                started: None,
                ready: false,
                failed: false,
            });
        }
        app.qa_result = options.qa_result;
        app.screenshot_path = options.screenshot;
        app.query_library();
        app.refresh_library_workflows();
        app.services
            .enqueue(vec![Effect::Assets(AssetCommand::List)]);
        if let Some(view) = options.view {
            app.send(UiIntent::Navigate(view));
        }
        if let Some(selected) = options.qa_ir_history {
            app.send(UiIntent::Navigate(ViewId::Assets));
            app.ir_history_ui.open = true;
            if let Some(id) = selected {
                app.open_ir_history_attempt(id);
            } else {
                app.refresh_ir_history();
            }
        }
        if let Some(path) = options.open {
            let backup = path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("gp200backup"));
            app.handle_file(
                if backup {
                    FilePurpose::ImportDeviceBackup
                } else {
                    FilePurpose::ImportPresets
                },
                path,
            );
            let intent = IntentId(app.next_local_intent);
            app.startup_import =
                (!backup || app.library_workflow.pending.contains_key(&intent)).then_some(intent);
        }
        if options.demo {
            app.send(UiIntent::Connect(gp200_sim::simulator_port().selection));
            app.connect_qa = Some(ConnectQa {
                started: Instant::now(),
                attempted: true,
                ready: false,
                failed: false,
            });
        }
        if options.connect {
            app.connect_qa = Some(ConnectQa {
                started: Instant::now(),
                attempted: false,
                ready: false,
                failed: false,
            });
            app.send(UiIntent::DiscoverDevices);
        }
        if options.scan_bank {
            app.bank_qa = Some(BankQa {
                started: None,
                failed: false,
            });
        }
        if let Some(launch) = options.nam_workspace {
            app.open_nam_workspace(launch);
        }
        if let Some(launch) = options.firmware_workspace {
            app.open_firmware_workspace(launch);
        }
        Ok(app)
    }
    pub fn with_services(controller: AppController, mut services: RuntimeServices) -> Self {
        let firmware_coordinator =
            firmware_update_coordinator::FirmwareUpdateCoordinator::new(&mut services);
        let recovery_ui =
            recovery::RecoveryUi::with_startup(controller.active_document_id().cloned());
        Self {
            controller,
            services,
            recovery_ui,
            firmware_coordinator,
            firmware_close_request_pending: false,
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            firmware_settled_recovery_close_requested: false,
            deferred_file_selections: Default::default(),
            omitted_file_selections: 0,
            help_topic: HelpTopic::default(),
            selected_module: ModuleId::new(3).expect("AMP"),
            qa_patch_inspector: false,
            control_state: Default::default(),
            browser_search: String::new(),
            catalog_search: String::new(),
            model_picker: false,
            save_review: false,
            save_slot: 23,
            connect_dialog: false,
            selected_port: 0,
            slot_preview: 23,
            selected_library: None,
            library_search: String::new(),
            favorites_only: false,
            library_trash: false,
            library_metadata: LibraryMetadata::default(),
            library_tags: String::new(),
            trashed_library: None,
            selected_asset_kind: AssetKind::ImpulseResponse,
            selected_asset_slot: 0,
            asset_slot_ui: Default::default(),
            asset_path: String::new(),
            asset_options: workflow::AssetOptions::default(),
            nam_ui: Default::default(),
            firmware_ui: Default::default(),
            asset_job_ui: Default::default(),
            ir_history_ui: Default::default(),
            asset_review: None,
            asset_export_pending: None,
            local_asset_jobs: Default::default(),
            local_library_jobs: Default::default(),
            native_export_mode: NativeExportMode::PreserveOriginal,
            native_export_review: false,
            native_export_pending: None,
            bank_scan: workflow::BankAcquisition::default(),
            stored_write_review: None,
            stored_write_wait: None,
            pending_switch: None,
            switch_wait: None,
            connect_qa: None,
            startup_import: None,
            bank_qa: None,
            capture_backup_qa: None,
            qa_result: Arc::new(std::sync::atomic::AtomicI32::new(0)),
            assignment_state: ui::assignments::AssignmentState::default(),
            global_state: ui::globals::GlobalUiState::default(),
            global_qa: None,
            ir_metadata_qa: None,
            library_workflow: Default::default(),
            selective_merge: Default::default(),
            performance_locked: true,
            performance_compact: false,
            started: Instant::now(),
            next_local_intent: 10_000_000,
            next_job: 1,
            last_message: 0,
            notice: crate::localization::text(
                "Captured preset opened offline. Connect to read your pedal.",
            )
            .into(),
            screenshot_path: None,
            screenshot_frames: 0,
            screenshot_asset_settled: None,
            screenshot_requested: false,
        }
    }
    pub fn firmware_coordinator_view(&self) -> FirmwareCoordinatorView {
        let mut view = self
            .firmware_coordinator
            .view(&self.controller, &self.services);
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        {
            view.can_review_linux_recovery &= self.firmware_producers_settled();
            view.can_close_settled_recovery_host = self.can_close_settled_recovery_host();
            if let Some(blocker) = self.firmware_recovery_draft_blocker() {
                view.can_review_linux_recovery = false;
                view.blockers.push(blocker.into());
            }
        }
        if !self.deferred_file_selections.is_empty() || self.omitted_file_selections != 0 {
            view.blockers.push(format!("{} original file choices are held locally; explicitly apply them after return or dismiss them", self.deferred_file_selections.len()));
        }
        view
    }
    fn firmware_producers_settled(&self) -> bool {
        !self.bank_scan.running
            && !self.asset_slot_ui.is_running()
            && !self.asset_job_ui.busy()
            && !self.nam_ui.busy()
            && !self.library_workflow.busy()
            && self.library_workflow.pending.is_empty()
            && self.local_asset_jobs.is_empty()
            && self.local_library_jobs.is_empty()
            && self.pending_switch.is_none()
            && self.switch_wait.is_none()
            && self.stored_write_wait.is_none()
            && self.startup_import.is_none()
            && !self.recovery_ui.busy
            && self.recovery_ui.visible_uncertain_hash.is_none()
            && self.deferred_file_selections.is_empty()
            && self.omitted_file_selections == 0
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    fn firmware_recovery_draft_blocker(&self) -> Option<&'static str> {
        if self.services.draft_recovery.is_none() {
            return Some(
                "Draft recovery storage is unavailable; restore it before reviewing firmware recovery.",
            );
        }
        if self.recovery_ui.found.is_some() {
            return Some(
                "Open the previous draft review and choose Recover all offline before reviewing firmware recovery. The previous journal is kept until the recovered documents are saved.",
            );
        }
        if self.recovery_ui.error.is_some() || self.recovery_ui.visible_uncertain_hash.is_some() {
            return Some(
                "Resolve the draft recovery error and wait for a durable Saved result before reviewing firmware recovery. Any existing journal is kept.",
            );
        }
        if !self.recovery_ui.ready {
            return Some(
                "Wait for previous draft discovery to finish before reviewing firmware recovery.",
            );
        }
        if self.recovery_ui.busy
            || self.recovery_ui.last_saved.as_deref() != Some(&self.controller.recovery_session())
        {
            return Some(
                "Wait for the current local documents' durable Saved result before reviewing firmware recovery.",
            );
        }
        None
    }
    pub fn begin_firmware_preparation(&mut self) -> Result<(), ServiceError> {
        let settled = self.firmware_producers_settled();
        self.firmware_coordinator
            .begin(&mut self.controller, &mut self.services, settled)
    }
    pub fn refresh_firmware_preparation(&mut self) -> Result<(), ServiceError> {
        self.firmware_coordinator
            .refresh_source(&mut self.controller, &self.services)
    }
    pub fn retain_local_draft_for_firmware_capture(&mut self) -> Result<(), ServiceError> {
        self.firmware_coordinator
            .retain_local_draft(&mut self.controller, self.recovery_ui.last_saved.as_deref())
    }
    pub fn capture_firmware_backup(
        &mut self,
        context: FirmwareBackupReviewContext,
    ) -> Result<(), ServiceError> {
        let settled = self.firmware_producers_settled();
        self.firmware_coordinator.capture(
            &mut self.controller,
            &self.services,
            self.recovery_ui.last_saved.as_deref(),
            settled,
            context,
        )
    }
    pub fn prepare_local_firmware_review(&mut self) -> Result<(), ServiceError> {
        let image = self
            .firmware_ui
            .image
            .clone()
            .ok_or_else(|| ServiceError::invalid("Inspect a local firmware image first"))?;
        self.firmware_coordinator.prepare(&self.controller, image)
    }
    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
    pub fn prepare_entry_study(
        &mut self,
        inputs: gp200_transport::EntryStudyInputs,
    ) -> Result<(), ServiceError> {
        self.firmware_coordinator.prepare_entry_study(inputs)
    }
    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
    pub fn entry_study_review(&self) -> Option<gp200_transport::EntryStudyReview> {
        self.firmware_coordinator.entry_study_review()
    }
    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
    pub fn execute_entry_study(&mut self, exact_signal: String) -> Result<(), ServiceError> {
        self.firmware_coordinator.execute_entry_study(exact_signal)
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub fn review_linux_recovery(&mut self) -> Result<(), ServiceError> {
        if let Some(blocker) = self.firmware_recovery_draft_blocker() {
            return Err(ServiceError::new(ErrorKind::Busy, blocker));
        }
        let image = self
            .firmware_ui
            .image
            .as_ref()
            .ok_or_else(|| {
                ServiceError::invalid("Select and inspect the GP-200 1.8.2 firmware image first")
            })?
            .review
            .clone();
        let settled = self.firmware_producers_settled();
        let selected = self.firmware_coordinator.linux_history_status().2;
        self.firmware_coordinator.begin_linux_recovery(
            &mut self.controller,
            &mut self.services,
            settled,
            image,
            None,
            selected,
        )
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub fn refresh_linux_firmware_history(&mut self) -> Result<(), ServiceError> {
        self.firmware_coordinator.refresh_linux_history()
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub fn linux_recovery_outcome(&self) -> Option<gp200_transport::LinuxRecoveryOutcome> {
        self.firmware_coordinator.linux_recovery_outcome()
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub fn prepare_linux_completion(
        &mut self,
        inputs: gp200_transport::LinuxCompletionInputs,
    ) -> Result<(), ServiceError> {
        self.firmware_coordinator
            .prepare_linux_completion(Some(inputs))
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub fn prepare_linux_completion_from_resources(&mut self) -> Result<(), ServiceError> {
        self.firmware_coordinator.prepare_linux_completion(None)
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub fn linux_completion_review(&self) -> Option<gp200_transport::LinuxCompletionReview> {
        self.firmware_coordinator.linux_completion_review()
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub fn linux_completion_view(&self) -> Option<gp200_transport::LinuxCompletionView> {
        self.firmware_coordinator.linux_completion_view()
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub fn linux_completion_outcome(&self) -> Option<gp200_transport::LinuxCompletionOutcome> {
        self.firmware_coordinator.linux_completion_outcome()
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub fn execute_linux_completion(&mut self, exact_signal: String) -> Result<(), ServiceError> {
        self.firmware_coordinator
            .execute_linux_completion(exact_signal)
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub fn approve_linux_completion_phase(
        &mut self,
        phase: gp200_transport::LinuxCompletionPhase,
        digest: String,
        manual: Option<String>,
    ) -> Result<(), ServiceError> {
        self.firmware_coordinator
            .approve_linux_completion_phase(phase, digest, manual)
    }
    pub fn request_firmware_update(&mut self) -> Result<(), ServiceError> {
        self.firmware_coordinator.request_update()
    }
    pub fn return_from_firmware_preparation(&mut self) -> Result<(), ServiceError> {
        self.firmware_coordinator
            .return_normal(&mut self.controller, &mut self.services)
    }
    pub fn request_firmware_history(
        &mut self,
        cursor: Option<gp200_library::FirmwareHistoryCursor>,
    ) -> Result<(), ServiceError> {
        self.firmware_coordinator
            .history(gp200_library::LibraryFirmwareAction::ListHistory { cursor, limit: 32 })
    }
    pub fn inspect_firmware_attempt(
        &mut self,
        attempt: FirmwareAttemptId,
    ) -> Result<(), ServiceError> {
        self.firmware_coordinator
            .history(gp200_library::LibraryFirmwareAction::InspectAttempt { attempt })
    }
    pub fn export_firmware_attempt(
        &mut self,
        attempt: FirmwareAttemptId,
        expected_prefix: gp200_library::FirmwareHistoryPrefix,
        destination: std::path::PathBuf,
    ) -> Result<(), ServiceError> {
        self.firmware_coordinator
            .history(gp200_library::LibraryFirmwareAction::ExportAttempt {
                attempt,
                expected_prefix,
                destination,
            })
    }
    pub fn retry_firmware_library_restore(&mut self) -> Result<(), ServiceError> {
        self.firmware_coordinator
            .retry_store_restore(&self.services)
    }
    pub fn stop_firmware_preparation(&mut self) {
        self.firmware_coordinator.stop(&mut self.controller);
    }
    pub fn firmware_close_blocked(&self) -> bool {
        self.firmware_coordinator.blocks_close()
    }
    pub fn firmware_close_request_pending(&self) -> bool {
        self.firmware_close_request_pending
    }
    pub fn dismiss_firmware_close_request(&mut self) {
        self.firmware_close_request_pending = false;
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub fn can_close_settled_recovery_host(&self) -> bool {
        self.services.file_worker_settled()
            && self.firmware_producers_settled()
            && self.firmware_recovery_draft_blocker().is_none()
            && !self.firmware_ui.busy()
            && self.firmware_ui.export_pending.is_none()
            && self.firmware_ui.updater.preparation_export.is_none()
            && self.firmware_ui.updater.preparation_dialog.is_none()
            && self.firmware_ui.updater.attempt_dialog.is_none()
            && !self.ir_history_ui.local_work_pending()
            && self.ir_history_ui.export_dialog.is_none()
            && self.asset_export_pending.is_none()
            && self.native_export_pending.is_none()
            && self
                .connect_qa
                .as_ref()
                .is_none_or(|qa| qa.ready || qa.failed)
            && self
                .bank_qa
                .as_ref()
                .is_none_or(|qa| qa.failed || self.bank_scan.completed.len() == 256)
            && self
                .capture_backup_qa
                .as_ref()
                .is_none_or(|qa| qa.ready || qa.failed)
            && self
                .global_qa
                .as_ref()
                .is_none_or(|qa| qa.ready || qa.failed)
            && self
                .ir_metadata_qa
                .as_ref()
                .is_none_or(|qa| qa.ready || qa.failed)
            && self
                .firmware_coordinator
                .can_close_settled_recovery_host(&self.controller, &self.services)
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn close_settled_recovery_host(
        &mut self,
        ctx: &egui::Context,
    ) -> Result<(), ServiceError> {
        if !self.can_close_settled_recovery_host() {
            self.firmware_settled_recovery_close_requested = false;
            self.request_application_close(ctx);
            return Err(ServiceError::new(
                ErrorKind::Busy,
                "Wait for the original recovery owner, library restoration, saved documents and all local work before closing",
            ));
        }
        self.firmware_settled_recovery_close_requested = true;
        self.firmware_close_request_pending = false;
        self.request_application_close(ctx);
        Ok(())
    }
    fn firmware_application_close_allowed(&self) -> bool {
        if !self.firmware_close_blocked() {
            return true;
        }
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        {
            self.firmware_settled_recovery_close_requested && self.can_close_settled_recovery_host()
        }
        #[cfg(not(all(feature = "firmware-linux-completion", target_os = "linux")))]
        {
            false
        }
    }
    fn intercept_firmware_application_close(&mut self, ctx: &egui::Context) {
        if !self.firmware_application_close_allowed() {
            #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
            {
                self.firmware_settled_recovery_close_requested = false;
            }
            self.firmware_close_request_pending = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.notice = "Return to editing after actual retirement, or close a settled recovery host while keeping recovery required".into();
        }
    }
    fn request_application_close(&mut self, ctx: &egui::Context) {
        let allowed = self.firmware_application_close_allowed();
        if !allowed {
            self.firmware_close_request_pending = true;
        }
        ctx.send_viewport_cmd(if !allowed {
            egui::ViewportCommand::CancelClose
        } else {
            egui::ViewportCommand::Close
        });
    }
    pub fn open_help_topic(&mut self, topic: HelpTopic) {
        self.help_topic = topic;
        self.send(UiIntent::Navigate(ViewId::Help));
    }
    fn send(&mut self, intent: UiIntent) {
        if self.services.firmware_paused()
            && !matches!(&intent, UiIntent::Navigate(_) | UiIntent::SetPreferences(_))
        {
            let effects = self.controller.dispatch(AppInput::Ui(intent));
            self.services.enqueue(effects);
            return;
        }
        if let UiIntent::Connect(selection) = &intent {
            self.library_workflow.device_key = Some(format!(
                "endpoint:{}:{}:{:?}",
                selection.input_id, selection.output_id, selection.fingerprint_hint
            ));
        }
        if let UiIntent::Library(
            LibraryCommand::ImportNative { intent, .. }
            | LibraryCommand::ExportNative { intent, .. }
            | LibraryCommand::ImportBundle { intent, .. }
            | LibraryCommand::ExportBundle { intent, .. }
            | LibraryCommand::Backup { intent, .. },
        ) = &intent
        {
            self.local_library_jobs.insert(JobId(intent.0));
        }
        let effects = self.controller.dispatch(AppInput::Ui(intent));
        self.services.enqueue(effects);
    }
    fn edit(&mut self, patch: EditPatch) {
        if let Some(document) = self.controller.active_document_id().cloned() {
            self.send(UiIntent::ApplyEdit { document, patch });
        }
    }
    fn schedule_external_reconciliation_repaint(&self, ctx: &egui::Context) {
        if self.controller.external_reconciliation_pending() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }
    fn local_intent(&mut self) -> IntentId {
        self.next_local_intent += 1;
        IntentId(self.next_local_intent)
    }
    fn query_library(&mut self) {
        let intent = self.local_intent();
        self.send(UiIntent::Library(LibraryCommand::Query {
            intent,
            query: LibraryQuery {
                text: self.library_search.clone(),
                favorites_only: self.favorites_only,
                trashed_only: self.library_trash,
                tags: Vec::new(),
            },
        }));
    }
    fn capture_library(&mut self) -> Option<IntentId> {
        let document = self.controller.active_document_id().cloned()?;
        match self.controller.document_snapshot(&document) {
            Ok(snapshot) => {
                let intent = self.local_intent();
                let metadata = LibraryMetadata {
                    name: snapshot.view.display_name().into(),
                    author: snapshot.view.metadata.author.clone().unwrap_or_default(),
                    style: snapshot.view.metadata.style.clone().unwrap_or_default(),
                    notes: snapshot.view.metadata.notes.clone().unwrap_or_default(),
                    ..Default::default()
                };
                self.send(UiIntent::Library(LibraryCommand::StoreSnapshot {
                    intent,
                    snapshot,
                    metadata,
                    parent: self
                        .controller
                        .active_document()
                        .and_then(|document| match &document.baseline.source {
                            SnapshotSource::Library { version } => Some(version.clone()),
                            _ => None,
                        }),
                }));
                return Some(intent);
            }
            Err(error) => self.services.error(error.message),
        }
        None
    }
    fn compare_capture(&mut self, slot: ComparisonSlot) {
        if let Some(document) = self.controller.active_document_id().cloned() {
            match self.controller.capture_comparison(&document, slot) {
                Ok(()) => self.notice = std::format!("Captured comparison {slot:?}"),
                Err(error) => self.services.error(error.message),
            }
        }
    }
    fn compare_recall(&mut self, slot: ComparisonSlot) {
        if let Some(document) = self.controller.active_document_id().cloned() {
            let effects = self.controller.recall_comparison(&document, slot);
            self.services.enqueue(effects);
        }
    }
    fn receive_file_selection(&mut self, selection: runtime::FileSelection) {
        // Retain the actual worker reply acknowledgement until its original
        // import/export/save intent has been handled or held locally.
        let _reply_acknowledgement = self
            .services
            .take_file_selection_acknowledgement(&selection);
        if !self.services.firmware_paused()
            || matches!(
                selection.purpose,
                FilePurpose::Firmware
                    | FilePurpose::ExportFirmwareReport
                    | FilePurpose::ExportFirmwarePreparation(_)
                    | FilePurpose::ExportFirmwareAttempt(_)
            )
        {
            self.handle_file_selection(selection);
            return;
        }
        if self.deferred_file_selections.len() < 32 {
            self.deferred_file_selections.push_back(selection);
        } else {
            self.omitted_file_selections = self.omitted_file_selections.saturating_add(1);
        }
        self.notice =
            "File choice is held locally. Return to editing, then explicitly apply or dismiss it."
                .into();
    }
    /// Original file-dialog results are never automatically replayed after a firmware hold.
    pub fn pending_firmware_file_choices(&self) -> usize {
        self.deferred_file_selections.len()
    }
    pub fn apply_pending_firmware_file_choices(&mut self) -> Result<(), ServiceError> {
        if self.services.firmware_paused() {
            return Err(ServiceError::new(
                ErrorKind::Busy,
                "Return to normal editing before applying held file choices",
            ));
        }
        if self.omitted_file_selections != 0 {
            return Err(ServiceError::new(
                ErrorKind::Indeterminate,
                "The held file-choice limit was exceeded; dismiss choices and select files again",
            ));
        }
        let choices = std::mem::take(&mut self.deferred_file_selections);
        for original in choices {
            self.handle_file_selection(original);
        }
        Ok(())
    }
    pub fn dismiss_pending_firmware_file_choices(&mut self) {
        self.deferred_file_selections.clear();
        self.omitted_file_selections = 0;
    }
    fn handle_file_selection(&mut self, selection: runtime::FileSelection) {
        match selection.purpose {
            FilePurpose::ExportFirmwarePreparation(id) => {
                self.finish_firmware_preparation_dialog(id, selection.path);
                return;
            }
            FilePurpose::ExportFirmwareAttempt(id) => {
                self.finish_firmware_attempt_dialog(id, selection.path);
                return;
            }
            _ => {}
        }
        if let Some(path) = selection.path {
            self.handle_file(selection.purpose, path);
        } else {
            match selection.purpose {
                FilePurpose::ExportNative => self.native_export_pending = None,
                FilePurpose::ExportDeviceBackup => self.library_workflow.export_backup = None,
                FilePurpose::ExportFirmwareReport => self.firmware_ui.export_pending = None,
                FilePurpose::ExportAssetJobReport => self.asset_job_ui.export_pending = None,
                FilePurpose::ExportIrHistory => self.ir_history_ui.export_dialog = None,
                FilePurpose::ExportNamWav | FilePurpose::ExportNamClo => {
                    self.asset_export_pending = None
                }
                _ => {}
            }
        }
    }
    fn handle_file(&mut self, purpose: FilePurpose, path: std::path::PathBuf) {
        match purpose {
            FilePurpose::ExportFirmwarePreparation(id) => {
                self.finish_firmware_preparation_dialog(id, Some(path))
            }
            FilePurpose::ExportFirmwareAttempt(id) => {
                self.finish_firmware_attempt_dialog(id, Some(path))
            }
            FilePurpose::Firmware => self.inspect_firmware(path),
            FilePurpose::ReviewIr | FilePurpose::ReviewSnapTone => {
                self.asset_job_source_selected(path)
            }
            FilePurpose::ExportAssetJobReport => self.asset_job_ui.export_to(path),
            FilePurpose::ExportIrHistory => self.export_ir_history_to(path),
            FilePurpose::ExportFirmwareReport => self.firmware_ui.export_to(path),
            FilePurpose::ImportDeviceBackup => {
                self.workflow_submit(
                    gp200_library::WorkflowAction::ImportBackup(path),
                    ui::library_workflows::Purpose::Export,
                );
            }
            FilePurpose::ExportDeviceBackup => {
                if let Some(backup) = self.library_workflow.export_backup.take() {
                    self.workflow_submit(
                        gp200_library::WorkflowAction::ExportBackup { backup, path },
                        ui::library_workflows::Purpose::Export,
                    );
                }
            }
            FilePurpose::ImportPresets
                if path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("gp200backup")) =>
            {
                self.workflow_submit(
                    gp200_library::WorkflowAction::ImportBackup(path),
                    ui::library_workflows::Purpose::Export,
                );
            }
            FilePurpose::ImportPresets => {
                let intent = self.local_intent();
                self.send(UiIntent::Library(import_command(intent, path)));
            }
            FilePurpose::ExportNative => {
                if let Some((version, mode)) = self.native_export_pending.take() {
                    let intent = self.local_intent();
                    self.send(UiIntent::Library(LibraryCommand::ExportNative {
                        intent,
                        request: NativeExportRequest {
                            path,
                            version,
                            mode,
                        },
                    }));
                } else {
                    self.services.error(crate::localization::text(
                        "Select a library preset before exporting a native file.",
                    ));
                }
            }
            FilePurpose::NamInput | FilePurpose::NamCalibrationInput => {
                self.asset_options.input_wav = path.to_string_lossy().into_owned();
                self.inspect_nam_file(path, ui::nam_assets::InspectionPurpose::Input);
            }
            FilePurpose::ImportNam => {
                self.selected_asset_kind = AssetKind::Nam;
                self.asset_path = path.to_string_lossy().into_owned();
                self.inspect_nam_file(path, ui::nam_assets::InspectionPurpose::Model);
            }
            FilePurpose::ExportNamWav | FilePurpose::ExportNamClo => {
                if let Some(info) = self.asset_export_pending.take() {
                    self.services.export_asset(info, path);
                } else {
                    self.services.error(crate::localization::text(
                        "Select a retained conversion result before exporting.",
                    ));
                }
            }
            FilePurpose::ExportPresets => {
                let intent = self.local_intent();
                let versions = self
                    .controller
                    .state()
                    .library
                    .entries
                    .iter()
                    .map(|entry| entry.latest_version.clone())
                    .collect();
                self.send(UiIntent::Library(LibraryCommand::ExportBundle {
                    intent,
                    request: ExportRequest {
                        path,
                        versions,
                        include_assets: true,
                    },
                }));
            }
            FilePurpose::Backup => {
                let intent = self.local_intent();
                let versions = self
                    .controller
                    .state()
                    .library
                    .entries
                    .iter()
                    .map(|entry| entry.latest_version.clone())
                    .collect();
                self.send(UiIntent::Library(LibraryCommand::Backup {
                    intent,
                    request: BackupRequest { path, versions },
                }));
            }
            _ => {
                self.asset_path = path.to_string_lossy().into_owned();
                self.next_job += 1;
                self.local_asset_jobs.insert(JobId(self.next_job));
                let kind = match purpose {
                    FilePurpose::ImportIr => AssetKind::ImpulseResponse,
                    FilePurpose::ImportNam => AssetKind::Nam,
                    FilePurpose::ImportSnapTone => AssetKind::SnapTone,
                    _ => AssetKind::Firmware,
                };
                self.services
                    .enqueue(vec![Effect::Assets(AssetCommand::Inspect(AssetJobSpec {
                        id: JobId(self.next_job),
                        kind,
                        source: path,
                        output: None,
                        options: Default::default(),
                    }))]);
            }
        }
    }
    fn selected_native_version(&self) -> Option<LibraryVersionId> {
        self.selected_library
            .as_ref()
            .and_then(|id| {
                self.controller
                    .state()
                    .library
                    .entries
                    .iter()
                    .find(|entry| &entry.id == id)
            })
            .map(|entry| entry.latest_version.clone())
    }
    fn cancel_scoped_job(&mut self, id: JobId) {
        if self.local_asset_jobs.contains(&id) {
            self.services
                .enqueue(vec![Effect::Assets(AssetCommand::Cancel(id))]);
        } else if self.local_library_jobs.contains(&id) {
            self.services
                .enqueue(vec![Effect::Library(LibraryCommand::Cancel(id))]);
        } else {
            self.send(UiIntent::CancelJob(id));
        }
    }
    fn review_stored_write(&mut self, clear: bool) {
        self.bank_scan.cancel();
        let slot = SlotId::new(self.slot_preview).expect("bounded");
        let Some(before) = self
            .controller
            .state()
            .bank
            .slots
            .get(&slot)
            .filter(|cached| cached.fresh)
            .map(|cached| cached.snapshot.clone())
        else {
            self.services.error(crate::localization::text(
                "Read the destination slot before reviewing a replacement.",
            ));
            return;
        };
        if clear {
            self.stored_write_review = Some(StoredWriteReview {
                clear: true,
                slot,
                expected: before.revision,
                source: before.clone(),
                before,
            });
            return;
        }
        let Some(document) = self.controller.active_document_id() else {
            return;
        };
        match self.controller.document_snapshot(document) {
            Ok(source) => {
                self.stored_write_review = Some(StoredWriteReview {
                    clear: false,
                    slot,
                    expected: before.revision,
                    source,
                    before,
                })
            }
            Err(error) => self.services.error(error.message),
        }
    }
    fn preserve_write_snapshot(&mut self, snapshot: Arc<PresetSnapshot>, name: String) -> IntentId {
        let intent = self.local_intent();
        self.send(UiIntent::Library(LibraryCommand::StoreSnapshot {
            intent,
            snapshot,
            metadata: LibraryMetadata {
                name,
                notes: crate::localization::text(
                    "Retained for a reviewed stored-preset replacement.",
                )
                .into(),
                ..Default::default()
            },
            parent: None,
        }));
        intent
    }
    fn observe_stored_write(&mut self, input: &AppInput) {
        match input {
            AppInput::Library(LibraryEvent::VersionStored { intent, .. }) => {
                if matches!(&self.stored_write_wait, Some(StoredWriteWait::Backup { intent:expected, .. }) if expected == intent)
                {
                    if let Some(StoredWriteWait::Backup { review, .. }) =
                        self.stored_write_wait.take()
                    {
                        if review.clear {
                            self.submit_reviewed_stored_write(review);
                            return;
                        }
                        let intent = self.preserve_write_snapshot(
                            review.source.clone(),
                            std::format!(
                                "Write source for slot {}: {}",
                                review.slot,
                                review.source.view.display_name()
                            ),
                        );
                        self.stored_write_wait = Some(StoredWriteWait::Source { intent, review });
                    }
                } else if matches!(&self.stored_write_wait, Some(StoredWriteWait::Source { intent:expected, .. }) if expected == intent)
                    && let Some(StoredWriteWait::Source { review, .. }) =
                        self.stored_write_wait.take()
                {
                    self.submit_reviewed_stored_write(review);
                }
            }
            AppInput::Library(LibraryEvent::Failed { intent, .. }) if matches!(&self.stored_write_wait, Some(StoredWriteWait::Backup { intent:expected, .. } | StoredWriteWait::Source { intent:expected, .. }) if expected == intent) =>
            {
                self.stored_write_wait = None;
            }
            AppInput::Device(
                DeviceEvent::OperationFailed { stamp, .. }
                | DeviceEvent::EditProgress {
                    stamp,
                    stage: EditStage::Failed(_) | EditStage::Indeterminate(_),
                },
            ) if matches!(&self.stored_write_wait, Some(StoredWriteWait::Writing { stamp:expected, .. }) if expected == stamp) =>
            {
                self.stored_write_wait = None;
            }
            AppInput::Device(DeviceEvent::EditProgress {
                stamp,
                stage: EditStage::Verified,
            }) if matches!(&self.stored_write_wait, Some(StoredWriteWait::Writing { stamp:expected, .. }) if expected == stamp) =>
            {
                self.stored_write_wait = None;
            }
            _ => {}
        }
    }
    fn submit_reviewed_stored_write(&mut self, review: StoredWriteReview) {
        let intent = if review.clear {
            UiIntent::ClearStoredPreset {
                slot: review.slot,
                expected_target_revision: review.expected,
            }
        } else {
            UiIntent::WriteStoredPreset {
                slot: review.slot,
                expected_target_revision: review.expected,
                snapshot: review.source,
            }
        };
        let effects = self.controller.dispatch(AppInput::Ui(intent));
        if let Some(stamp) = effects.iter().find_map(|effect| match effect {
            Effect::Device(
                DeviceCommand::WriteStoredPreset { stamp, .. }
                | DeviceCommand::ClearStoredPreset { stamp, .. },
            ) => Some(*stamp),
            _ => None,
        }) {
            self.stored_write_wait = Some(StoredWriteWait::Writing {
                stamp,
                slot: review.slot,
            });
        }
        self.services.enqueue(effects);
    }
    fn request_switch(&mut self, action: PendingSwitch) {
        let buffer_dirty = matches!(action, PendingSwitch::Audition(_))
            && self
                .controller
                .state()
                .documents
                .get(&gp200_app::device_document_id())
                .is_some_and(|doc| doc.dirty());
        if buffer_dirty {
            self.controller
                .activate_document(&gp200_app::device_document_id());
            self.pending_switch = Some(action);
        } else if self
            .controller
            .active_document()
            .is_some_and(|doc| doc.dirty())
        {
            self.pending_switch = Some(action);
        } else {
            self.execute_switch(action);
        }
    }
    fn execute_switch(&mut self, action: PendingSwitch) {
        match action {
            PendingSwitch::Document(id) => {
                self.controller.activate_document(&id);
            }
            PendingSwitch::Library(id) => self.send(UiIntent::OpenLibraryVersion(id)),
            PendingSwitch::Snapshot(snapshot) => self.send(UiIntent::NewOfflineFrom(snapshot)),
            PendingSwitch::Audition(slot) => self.send(UiIntent::AuditionStored(slot)),
        }
    }
    fn discard_for_switch(&mut self, action: PendingSwitch) {
        if let Some(document) = self.controller.active_document_id().cloned() {
            self.send(UiIntent::DiscardDraft(document.clone()));
            self.switch_wait = Some(SwitchWait::Discarding { document, action });
        }
    }
    fn advance_workflows(&mut self) {
        if self
            .ir_metadata_qa
            .as_ref()
            .is_some_and(|qa| qa.started.is_none() && !qa.failed)
            && self.controller.state().connection == ConnectionState::Ready
            && !self.controller.device_operation_pending()
            && self
                .bank_qa
                .as_ref()
                .is_none_or(|_| self.bank_scan.completed.len() == 256)
            && self.global_qa.as_ref().is_none_or(|qa| qa.ready)
        {
            if let Some(qa) = &mut self.ir_metadata_qa {
                qa.started = Some(Instant::now());
            }
            self.asset_slot_ui
                .start(AssetBank::Ir, self.controller.state().epoch);
        }
        self.advance_asset_metadata_reads();
        self.advance_asset_job_review();
        if self
            .ir_metadata_qa
            .as_ref()
            .is_some_and(|qa| qa.started.is_some() && !qa.ready && !qa.failed)
            && self
                .controller
                .state()
                .device_assets
                .complete_count(AssetBank::Ir)
                == 20
            && self.controller.state().connection == ConnectionState::Ready
            && !self.controller.device_operation_pending()
        {
            if let Some(qa) = &mut self.ir_metadata_qa {
                qa.ready = true;
            }
            self.screenshot_frames = 0;
        }
        if self.ir_metadata_qa.as_ref().is_some_and(|qa| {
            !qa.ready
                && !qa.failed
                && qa.started.is_some_and(|started| {
                    started.elapsed() >= Duration::from_secs(15) || !self.asset_slot_ui.is_running()
                })
        }) {
            if let Some(qa) = &mut self.ir_metadata_qa {
                qa.failed = true;
            }
            self.qa_result
                .store(3, std::sync::atomic::Ordering::Relaxed);
            self.services.error(crate::localization::text(
                "IR metadata bank read did not complete. Reconnect and refresh explicitly.",
            ));
            eprintln!("--read-ir-metadata failed: IR metadata bank read did not complete");
        }
        if self
            .global_qa
            .as_ref()
            .is_some_and(|qa| qa.started.is_none() && !qa.failed)
            && self.controller.state().connection == ConnectionState::Ready
            && !self.controller.device_operation_pending()
            && !self.bank_scan.running
            && self
                .bank_qa
                .as_ref()
                .is_none_or(|_| self.bank_scan.completed.len() == 256)
        {
            if let Some(qa) = &mut self.global_qa {
                qa.started = Some(Instant::now());
            }
            self.send(UiIntent::RefreshDeviceGlobals);
        }
        if self
            .global_qa
            .as_ref()
            .is_some_and(|qa| qa.started.is_some() && !qa.failed && !qa.ready)
            && self
                .controller
                .state()
                .device_globals
                .as_ref()
                .is_some_and(|state| state.epoch == self.controller.state().epoch)
            && !self.controller.device_operation_pending()
        {
            if let Some(qa) = &mut self.global_qa {
                qa.ready = true;
            }
            self.screenshot_frames = 0;
        }
        if self.global_qa.as_ref().is_some_and(|qa| {
            !qa.ready
                && !qa.failed
                && qa
                    .started
                    .is_some_and(|started| started.elapsed() >= Duration::from_secs(10))
        }) {
            if let Some(qa) = &mut self.global_qa {
                qa.failed = true;
            }
            self.qa_result
                .store(3, std::sync::atomic::Ordering::Relaxed);
            self.services.error(crate::localization::text("Global settings did not complete within 10 seconds. Reconnect and read settings again."));
            eprintln!("--read-globals failed: global settings did not complete within 10 seconds");
        }

        if self
            .bank_qa
            .as_ref()
            .is_some_and(|qa| qa.started.is_none() && !qa.failed)
            && self.controller.state().connection == ConnectionState::Ready
            && self.controller.state().confirmed_buffer.is_some()
            && !self.controller.device_operation_pending()
        {
            if let Some(qa) = &mut self.bank_qa {
                qa.started = Some(Instant::now());
            }
            self.bank_scan.start(self.controller.state().epoch, false);
        }
        if self.bank_qa.as_ref().is_some_and(|qa| {
            !qa.failed
                && qa.started.is_some_and(|started| {
                    started.elapsed() >= Duration::from_secs(45) || !self.bank_scan.running
                })
                && self.bank_scan.completed.len() != 256
        }) {
            let reason = self.bank_scan.message.clone();
            self.bank_scan.cancel();
            if let Some(qa) = &mut self.bank_qa {
                qa.failed = true;
            }
            let message = std::format!(
                "Bank scan did not complete: {} / 256 slots. {reason}",
                self.bank_scan.completed.len()
            );
            eprintln!("--scan-bank failed: {message}");
            self.qa_result
                .store(3, std::sync::atomic::Ordering::Relaxed);
            self.services.error(message);
        }

        if let Some(slot) = self.bank_scan.next_slot(self.controller.state().epoch) {
            let effects = self
                .controller
                .dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::Stored(slot))));
            if let Some(stamp) = effects.iter().find_map(|effect| match effect {
                Effect::Device(DeviceCommand::RefreshTarget {
                    stamp,
                    target: PresetTarget::Stored(actual),
                }) if *actual == slot => Some(*stamp),
                _ => None,
            }) {
                self.bank_scan.submitted(stamp, slot);
                self.services.enqueue(effects);
            } else {
                self.bank_scan.cancel();
            }
        }
        if let Some(SwitchWait::Discarding { document, .. }) = &self.switch_wait {
            let status = self.controller.state().document_status(document);
            if status
                .as_ref()
                .is_some_and(|status| !status.changed_since_baseline && status.pending == 0)
            {
                if let Some(SwitchWait::Discarding { action, .. }) = self.switch_wait.take() {
                    self.execute_switch(action);
                }
            } else if status
                .as_ref()
                .is_some_and(|status| status.pending == 0 && status.conflicts > 0)
            {
                self.switch_wait = None;
                self.services.error(crate::localization::text(
                    "Resolve the pedal conflict before changing presets. Your draft is retained.",
                ));
            }
        }
        if self.connect_qa.as_ref().is_some_and(|qa| {
            !qa.ready
                && !qa.failed
                && self.controller.state().connection == ConnectionState::Ready
                && self.controller.state().confirmed_buffer.is_some()
        }) {
            if let Some(qa) = &mut self.connect_qa {
                qa.ready = true;
            }
            self.controller
                .activate_document(&gp200_app::device_document_id());
            self.notice = if self
                .controller
                .state()
                .identity
                .as_ref()
                .is_some_and(|identity| identity.mode == BackendMode::Simulator)
            {
                crate::localization::text(
                    "Simulator ready. Changes stay in its simulated edit buffer.",
                )
            } else {
                crate::localization::text(
                    "GP-200 connected. The displayed edit buffer was read from the pedal.",
                )
            }
            .into();
        }
        if self.connect_qa.as_ref().is_some_and(|qa| {
            !qa.attempted && !qa.failed && !self.controller.state().ports.is_empty()
        }) {
            let candidates: Vec<_> = self
                .controller
                .state()
                .ports
                .iter()
                .filter(|port| {
                    port.display_name
                        .to_ascii_lowercase()
                        .chars()
                        .filter(char::is_ascii_alphanumeric)
                        .collect::<String>()
                        .contains("gp200")
                })
                .cloned()
                .collect();
            if candidates.len() == 1 {
                if let Some(qa) = &mut self.connect_qa {
                    qa.attempted = true;
                }
                self.send(UiIntent::Connect(candidates[0].selection.clone()));
            } else if candidates.len() > 1 {
                self.fail_connect_qa(crate::localization::text(
                    "More than one GP-200 candidate is available. Connect selected ports manually.",
                ));
            }
        }
        if self.connect_qa.as_ref().is_some_and(|qa| {
            !qa.ready && !qa.failed && qa.started.elapsed() >= Duration::from_secs(10)
        }) {
            self.fail_connect_qa(crate::localization::text("The GP-200 did not become ready within 10 seconds. Check Normal USB mode and the connection."));
        }
    }
    fn apply_frame_style(&self, ctx: &egui::Context) {
        let preferences = &self.controller.state().preferences;
        theme::apply(ctx, preferences.theme, preferences.scale);
        if self.screenshot_path.is_some() {
            // Capture settled widgets independently of the renderer's frame timing.
            // Set both themes each frame so a theme change cannot resume animations.
            ctx.all_styles_mut(|style| style.animation_time = 0.0);
        }
    }
    fn fail_connect_qa(&mut self, message: &str) {
        if let Some(qa) = &mut self.connect_qa {
            qa.failed = true;
        }
        eprintln!("--connect failed: {message}");
        self.qa_result
            .store(3, std::sync::atomic::Ordering::Relaxed);
        self.services.error(message);
    }
}
fn import_command(intent: IntentId, path: std::path::PathBuf) -> LibraryCommand {
    if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("prst"))
    {
        LibraryCommand::ImportNative {
            intent,
            request: ImportRequest { path },
        }
    } else {
        LibraryCommand::ImportBundle {
            intent,
            request: ImportRequest { path },
        }
    }
}
impl eframe::App for StudioApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.firmware_coordinator.stop(&mut self.controller);
        if self.recovery_ui.ready
            && self.recovery_ui.found.is_none()
            && let Some(worker) = &self.services.draft_recovery
            && let Err(error) = worker.flush_on_exit(Arc::new(self.controller.recovery_session()))
        {
            eprintln!("Final draft recovery snapshot failed: {}", error.message);
        }
    }
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.viewport().close_requested()) {
            self.intercept_firmware_application_close(ctx);
        }
        self.firmware_ui.poll();
        if self.firmware_ui.busy() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        let (inputs, selections) = self.services.pump();
        self.poll_firmware_preparation_export();
        for original in self.services.take_unoffered_controls() {
            self.controller.settle_unoffered_device_control(original);
        }
        if let Some(directories) = self.services.take_directory_update() {
            let mut preferences = self.controller.state().preferences.clone();
            preferences.dialog_directories = directories;
            self.send(UiIntent::SetPreferences(preferences));
        }
        for input in inputs {
            if let AppInput::Library(event) = &input {
                self.observe_selective_merge_source(event);
            }
            if matches!(&input, AppInput::Library(LibraryEvent::Imported { intent, .. } | LibraryEvent::Failed { intent, .. }) if self.startup_import == Some(*intent))
            {
                self.startup_import = None;
                self.screenshot_frames = 0;
            }
            self.observe_stored_write(&input);
            if let AppInput::Assets(event) = &input {
                self.nam_ui.observe(event);
            }
            match &input {
                AppInput::Assets(
                    AssetEvent::Prepared { job, .. }
                    | AssetEvent::Inspected { job, .. }
                    | AssetEvent::Failed { job, .. },
                ) => {
                    if self.local_asset_jobs.remove(job) {
                        self.screenshot_frames = 0;
                        self.screenshot_asset_settled = Some(Instant::now());
                    }
                }
                AppInput::Library(
                    LibraryEvent::Imported { intent, .. }
                    | LibraryEvent::Exported { intent, .. }
                    | LibraryEvent::Failed { intent, .. },
                ) => {
                    self.local_library_jobs.remove(&JobId(intent.0));
                }
                _ => {}
            }
            if let AppInput::Device(event) = &input {
                self.bank_scan.observe(event);
                self.observe_library_device(event);
            }
            if !self.services.firmware_paused()
                && let AppInput::Library(event) = &input
                && let Some(SwitchWait::Saving { intent, .. }) = &self.switch_wait
            {
                if matches!(event, LibraryEvent::VersionStored { intent:actual, .. } if actual == intent)
                {
                    if let Some(SwitchWait::Saving { action, .. }) = self.switch_wait.take() {
                        self.discard_for_switch(action);
                    }
                } else if matches!(event, LibraryEvent::Failed { intent:actual, .. } if actual == intent)
                {
                    self.switch_wait = None;
                }
            }
            let refresh = matches!(
                &input,
                AppInput::Library(
                    LibraryEvent::VersionStored { .. }
                        | LibraryEvent::Imported { .. }
                        | LibraryEvent::EntryChanged { .. }
                )
            );
            let effects = self.controller.dispatch(input);
            self.services.enqueue(effects);
            if refresh && !self.services.firmware_paused() {
                self.query_library();
            }
        }
        for event in self.services.pump_workflows() {
            if !self.observe_ir_history_event(&event) {
                self.observe_workflow_event(event);
            }
        }
        for selection in selections {
            self.receive_file_selection(selection);
        }
        let effects = self
            .controller
            .dispatch(AppInput::Tick(self.started.elapsed().as_millis() as u64));
        self.services.enqueue(effects);
        self.schedule_external_reconciliation_repaint(ctx);
        if !self.services.firmware_paused() {
            self.advance_workflows();
            self.advance_nam_workspace();
            self.advance_library_workflows();
        }
        self.advance_draft_recovery();
        let settled = self.firmware_producers_settled();
        self.firmware_coordinator.poll(
            &mut self.controller,
            &mut self.services,
            self.recovery_ui.last_saved.as_deref(),
            settled,
        );
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        if self.firmware_settled_recovery_close_requested {
            // Pumping may settle or introduce original work after the earlier
            // close event. Recheck the live bound proof before host exit too.
            self.intercept_firmware_application_close(ctx);
        }
        if self.firmware_close_blocked() {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
        if self.services.draft_recovery.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        if let Some(message) = self.controller.state().messages.back()
            && message.sequence != self.last_message
        {
            self.last_message = message.sequence;
            self.notice = message.message.clone();
        }
        if self.services.has_pending()
            || self.library_workflow.busy()
            || !self.library_workflow.pending.is_empty()
            || self.bank_scan.running
            || self.asset_slot_ui.is_running()
            || self.asset_job_ui.busy()
            || self.switch_wait.is_some()
            || self.stored_write_wait.is_some()
            || self
                .connect_qa
                .as_ref()
                .is_some_and(|qa| !qa.ready && !qa.failed)
        {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.apply_frame_style(ui.ctx());
        self.shortcuts(ui.ctx());
        self.render(ui);
        if self.screenshot_path.is_some() {
            self.screenshot_frames += 1;
            let connection_ready = self.connect_qa.as_ref().is_none_or(|qa| qa.ready);
            let globals_ready = self.global_qa.as_ref().is_none_or(|qa| qa.ready);
            let ir_metadata_ready = self.ir_metadata_qa.as_ref().is_none_or(|qa| qa.ready);
            let bank_ready = self
                .bank_qa
                .as_ref()
                .is_none_or(|qa| !qa.failed && self.bank_scan.completed.len() == 256);
            if self.screenshot_frames >= 8
                && !self.screenshot_requested
                && (self.services.draft_recovery.is_none()
                    || self.recovery_ui.ready
                    || self.recovery_ui.error.is_some())
                && connection_ready
                && self.startup_import.is_none()
                && bank_ready
                && globals_ready
                && ir_metadata_ready
                && self.capture_backup_qa.as_ref().is_none_or(|qa| qa.ready)
                && self.library_workflow.pending.is_empty()
                && !self.library_workflow.busy()
                && self.local_asset_jobs.is_empty()
                && !self.firmware_ui.busy()
                && !self.asset_job_ui.busy()
                && !self.ir_history_ui.local_read_pending()
                && self
                    .firmware_ui
                    .settled
                    .as_ref()
                    .is_none_or(|settled| settled.elapsed() >= Duration::from_millis(300))
                && self
                    .screenshot_asset_settled
                    .as_ref()
                    .is_none_or(|settled| settled.elapsed() >= Duration::from_millis(300))
            {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
                self.screenshot_requested = true;
            }
            let pixels = ui.ctx().input(|input| {
                input.events.iter().find_map(|event| {
                    if let egui::Event::Screenshot { image, .. } = event {
                        Some(image.clone())
                    } else {
                        None
                    }
                })
            });
            if let Some(pixels) = pixels
                && let Some(path) = self.screenshot_path.take()
            {
                self.services.screenshot(path, pixels);
            }
            ui.ctx().request_repaint_after(Duration::from_millis(30));
        }
        if self.connect_qa.as_ref().is_some_and(|qa| qa.failed) && self.screenshot_path.is_some() {
            self.request_application_close(ui.ctx());
        }
        if self.bank_qa.as_ref().is_some_and(|qa| qa.failed) && self.screenshot_path.is_some() {
            self.request_application_close(ui.ctx());
        }
        if self.global_qa.as_ref().is_some_and(|qa| qa.failed) && self.screenshot_path.is_some() {
            self.request_application_close(ui.ctx());
        }
        if self.capture_backup_qa.as_ref().is_some_and(|qa| qa.failed)
            && self.screenshot_path.is_some()
        {
            self.request_application_close(ui.ctx());
        }
        if self.ir_metadata_qa.as_ref().is_some_and(|qa| qa.failed)
            && self.screenshot_path.is_some()
        {
            self.request_application_close(ui.ctx());
        }
        if self.screenshot_requested {
            if self.services.screenshot_saved {
                self.request_application_close(ui.ctx());
            } else {
                ui.ctx().request_repaint_after(Duration::from_millis(30));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct NoDevice;
    impl DeviceService for NoDevice {
        fn try_submit(&self, command: DeviceCommand) -> Result<(), SubmitError<DeviceCommand>> {
            Err(SubmitError::Disconnected(command))
        }
        fn try_next_event(&self) -> Option<DeviceEvent> {
            None
        }
        fn shutdown(&self) {}
    }
    fn app() -> StudioApp {
        StudioApp::with_services(
            AppController::with_demo(
                gp200_sim::captured_demo_snapshot(),
                AppPreferences::default(),
            )
            .expect("catalog"),
            RuntimeServices::new(Box::new(NoDevice), no_wake()),
        )
    }
    #[test]
    fn paused_file_worker_replies_hold_original_import_and_export_intents_before_acknowledgement() {
        for purpose in [
            FilePurpose::ImportPresets,
            FilePurpose::ExportPresets,
            FilePurpose::ExportNative,
        ] {
            let mut app = app();
            let original = app.controller.recovery_session();
            let _lease = app.services.freeze_firmware().unwrap();
            let (running, release) = app.services.test_queue_file_work_barrier();
            running.recv_timeout(Duration::from_secs(3)).unwrap();
            app.services
                .test_queue_file_selection(purpose, Some("original-file-choice.prst".into()));
            let (published, finish) = app.services.test_queue_file_work_barrier();
            release.send(()).unwrap();
            published.recv_timeout(Duration::from_secs(3)).unwrap();
            let (inputs, mut selections) = app.services.pump();
            assert!(inputs.is_empty());
            assert_eq!(selections.len(), 1);
            assert!(!app.services.file_worker_settled());
            app.receive_file_selection(selections.remove(0));
            assert_eq!(app.pending_firmware_file_choices(), 1);
            assert!(!app.firmware_producers_settled());
            assert_eq!(app.controller.recovery_session(), original);
            assert!(app.apply_pending_firmware_file_choices().is_err());
            assert_eq!(app.controller.recovery_session(), original);
            finish.send(()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !app.services.file_worker_settled() {
                assert!(
                    Instant::now() < deadline,
                    "Handled reply did not acknowledge its worker token"
                );
                std::thread::yield_now();
            }
            assert!(
                !app.firmware_producers_settled(),
                "Held original intent must still deny firmware close"
            );
            app.dismiss_pending_firmware_file_choices();
            assert!(app.firmware_producers_settled());
            assert_eq!(app.controller.recovery_session(), original);
            assert!(app.services.firmware_paused());
        }
    }
    #[test]
    fn cancelled_dialog_reply_settles_only_after_the_app_handles_the_original_result() {
        let mut app = app();
        let original = app.controller.recovery_session();
        app.services
            .test_queue_file_selection(FilePurpose::ImportPresets, None);
        let (published, finish) = app.services.test_queue_file_work_barrier();
        published.recv_timeout(Duration::from_secs(3)).unwrap();
        finish.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let selections = loop {
            let (inputs, selections) = app.services.pump();
            assert!(inputs.is_empty());
            if !selections.is_empty() {
                break selections;
            }
            assert!(
                Instant::now() < deadline,
                "Original cancellation reply was not published"
            );
            std::thread::yield_now();
        };
        assert!(!app.services.file_worker_settled());
        for selection in selections {
            app.receive_file_selection(selection);
        }
        while !app.services.file_worker_settled() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(app.controller.recovery_session(), original);
        assert!(app.services.errors.is_empty());
        assert_eq!(app.pending_firmware_file_choices(), 0);
    }
    #[test]
    fn interactive_frames_preserve_animation_settings_across_themes() {
        let mut app = app();
        let context = egui::Context::default();
        context.all_styles_mut(|style| style.animation_time = 0.37);
        for preference in [ThemePreference::Light, ThemePreference::Dark] {
            let mut preferences = app.controller.state().preferences.clone();
            preferences.theme = preference;
            let _ = app
                .controller
                .dispatch(AppInput::Ui(UiIntent::SetPreferences(preferences)));
            app.apply_frame_style(&context);
            for theme in [egui::Theme::Light, egui::Theme::Dark] {
                assert_eq!(context.style_of(theme).animation_time, 0.37);
            }
        }
    }
    #[test]
    fn screenshot_frames_settle_animated_widgets_at_different_frame_times() {
        for interval in [0.001, 0.017, 0.043] {
            let mut app = app();
            app.screenshot_path = Some("/unused-native-qa.png".into());
            let context = egui::Context::default();
            let animation = egui::Id::new("floating-scrollbar");
            for (index, target) in [false, true, false, true].into_iter().enumerate() {
                let preference = if index % 2 == 0 {
                    ThemePreference::Light
                } else {
                    ThemePreference::Dark
                };
                let mut preferences = app.controller.state().preferences.clone();
                preferences.theme = preference;
                let _ = app
                    .controller
                    .dispatch(AppInput::Ui(UiIntent::SetPreferences(preferences)));
                // Simulate a theme resetting its style before a later frame.
                context.all_styles_mut(|style| style.animation_time = 0.37);
                app.apply_frame_style(&context);
                for theme in [egui::Theme::Light, egui::Theme::Dark] {
                    assert_eq!(context.style_of(theme).animation_time, 0.0);
                }
                let mut output = context.run_ui(
                    egui::RawInput {
                        time: Some(index as f64 * interval),
                        ..Default::default()
                    },
                    |ui| {
                        assert_eq!(
                            ui.ctx().animate_bool_responsive(animation, target),
                            if target { 1.0 } else { 0.0 }
                        );
                    },
                );
                output.textures_delta.clear();
            }
            assert!(!app.services.has_pending());
        }
    }
    #[test]
    fn all_help_topics_render_passively_without_changing_drafts_or_services() {
        let mut app = app();
        let draft = app.controller.active_document().unwrap().draft.clone();
        let preferences = app.controller.state().preferences.clone();
        let context = egui::Context::default();
        theme::install_fonts(&context);
        for (topic, _) in HelpTopic::ALL {
            app.open_help_topic(topic);
            let mut output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(960.0, 640.0),
                    )),
                    ..Default::default()
                },
                |ui| app.render(ui),
            );
            assert!(!output.shapes.is_empty());
            output.textures_delta.clear();
            assert_eq!(app.controller.active_document().unwrap().draft, draft);
            assert_eq!(app.controller.state().preferences, preferences);
            assert!(!app.services.has_pending());
            let (inputs, selections) = app.services.pump();
            assert!(inputs.is_empty() && selections.is_empty());
            assert!(app.services.errors.is_empty());
            assert!(app.services.last_link_notice.is_none());
        }
    }
    #[test]
    fn selected_module_qa_is_bounded_and_does_not_connect_or_edit() {
        for (name, expected) in [("dly", 8), ("CAB", 5), ("10", 10)] {
            let options =
                LaunchOptions::parse(["--qa-selected-module".into(), name.into()]).unwrap();
            assert_eq!(options.qa_selected_module.unwrap().get(), expected);
            assert!(!options.connect && !options.demo);
        }
        for name in ["11", "255", "-1", "unknown"] {
            assert!(LaunchOptions::parse(["--qa-selected-module".into(), name.into()]).is_err());
        }
    }
    #[test]
    fn global_read_qa_requires_an_explicit_backend_and_tab_selection_never_connects() {
        assert!(LaunchOptions::parse(["--read-globals".into()]).is_err());
        let options = LaunchOptions::parse(["--demo".into(), "--read-globals".into()]).unwrap();
        assert!(options.demo && options.read_globals && !options.connect);
        for (name, expected) in [
            ("audio", GlobalTab::Audio),
            ("midi", GlobalTab::Midi),
            ("external", GlobalTab::External),
            ("footswitch", GlobalTab::Footswitch),
            ("eq", GlobalTab::Eq),
            ("display", GlobalTab::Display),
        ] {
            let options = LaunchOptions::parse(["--qa-global-tab".into(), name.into()]).unwrap();
            assert_eq!(options.qa_global_tab, Some(expected));
            assert!(!options.demo && !options.connect && !options.read_globals);
        }
        assert!(LaunchOptions::parse(["--qa-global-tab".into(), "raw".into()]).is_err());
    }
    #[test]
    fn every_workspace_renders_in_both_themes_and_compact_size() {
        for size in [[1280.0, 800.0], [960.0, 640.0]] {
            for theme_preference in [ThemePreference::Dark, ThemePreference::Light] {
                let mut app = app();
                let context = egui::Context::default();
                theme::install_fonts(&context);
                theme::apply(&context, theme_preference, 1.0);
                for view in [
                    ViewId::Editor,
                    ViewId::Library,
                    ViewId::Assets,
                    ViewId::Assignments,
                    ViewId::GlobalSettings,
                    ViewId::Performance,
                    ViewId::Practice,
                    ViewId::Diagnostics,
                    ViewId::AppSettings,
                    ViewId::Help,
                ] {
                    app.send(UiIntent::Navigate(view));
                    let mut output = context.run_ui(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(size[0], size[1]),
                            )),
                            ..Default::default()
                        },
                        |ui| app.render(ui),
                    );
                    assert!(!output.shapes.is_empty(), "{view:?} rendered no shapes");
                    output.textures_delta.clear();
                    assert_eq!(
                        app.controller
                            .active_document()
                            .expect("offline draft")
                            .draft
                            .view
                            .modules
                            .len(),
                        11
                    );
                }
            }
        }
    }
    #[test]
    fn theme_change_preserves_exact_parameter_draft() {
        let mut app = app();
        let id = app
            .controller
            .active_document_id()
            .expect("document")
            .clone();
        app.send(UiIntent::ApplyEdit {
            document: id,
            patch: EditPatch::SetParameter {
                module: ModuleId::new(3).expect("AMP"),
                model: ModelCode(0x0700_0015),
                model_index: ModelIndex(13),
                parameter: ParamId::new(0).expect("gain"),
                value: F32Bits(51.0_f32.to_bits()),
            },
        });
        let draft = app.controller.active_document().expect("doc").draft.clone();
        for preference in [
            ThemePreference::Dark,
            ThemePreference::Light,
            ThemePreference::System,
        ] {
            let mut preferences = app.controller.state().preferences.clone();
            preferences.theme = preference;
            app.send(UiIntent::SetPreferences(preferences));
            theme::apply(&egui::Context::default(), preference, 1.0);
            assert_eq!(app.controller.active_document().expect("doc").draft, draft);
        }
        // No pump: this test creates no preference file or physical MIDI session.
    }
    #[test]
    fn stopped_device_worker_fails_request_without_infinite_retry() {
        let mut services = RuntimeServices::new(Box::new(NoDevice), no_wake());
        services.enqueue(vec![Effect::Device(DeviceCommand::ReadCurrent(
            RequestStamp {
                epoch: SessionEpoch(9),
                intent: IntentId(14),
            },
        ))]);
        let (inputs, _) = services.pump();
        assert!(!services.has_pending());
        assert!(matches!(
            inputs.as_slice(),
            [AppInput::Device(DeviceEvent::OperationFailed {
                stamp: RequestStamp {
                    epoch: SessionEpoch(9),
                    intent: IntentId(14)
                },
                error: ServiceError {
                    kind: ErrorKind::Disconnected,
                    ..
                }
            })]
        ));
    }
    struct BusyOnce(Arc<AtomicUsize>);
    impl DeviceService for BusyOnce {
        fn try_submit(&self, command: DeviceCommand) -> Result<(), SubmitError<DeviceCommand>> {
            if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
                Err(SubmitError::Busy(command))
            } else {
                Ok(())
            }
        }
        fn try_next_event(&self) -> Option<DeviceEvent> {
            None
        }
        fn shutdown(&self) {}
    }
    #[test]
    fn busy_queue_retains_request_for_later_pump() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut services = RuntimeServices::new(Box::new(BusyOnce(attempts.clone())), no_wake());
        services.enqueue(vec![Effect::Device(DeviceCommand::Discover)]);
        services.pump();
        assert!(services.has_pending());
        services.pump();
        assert!(!services.has_pending());
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
    }
    #[test]
    fn cli_rejects_unbounded_or_unknown_screenshot_setup() {
        assert!(LaunchOptions::parse(["--scan-bank"].map(str::to_owned)).is_err());
        assert!(
            LaunchOptions::parse(["--demo", "--scan-bank"].map(str::to_owned))
                .unwrap()
                .scan_bank
        );
        assert!(LaunchOptions::parse(["--demo", "--connect"].map(str::to_owned)).is_err());
        assert!(LaunchOptions::parse(["--size", "200x80"].map(str::to_owned)).is_err());
        assert!(LaunchOptions::parse(["--size", "NaNx800"].map(str::to_owned)).is_err());
        assert!(LaunchOptions::parse(["--theme", "neon"].map(str::to_owned)).is_err());
        let options = LaunchOptions::parse(
            [
                "--demo",
                "--theme",
                "dark",
                "--view",
                "performance",
                "--screenshot",
                "/tmp/gp200.png",
            ]
            .map(str::to_owned),
        )
        .expect("supported CLI");
        assert!(options.demo);
        assert_eq!(options.theme, Some(ThemePreference::Dark));
        assert_eq!(options.view, Some(ViewId::Performance));
    }
    struct RecordingLibrary(Arc<std::sync::Mutex<Vec<LibraryCommand>>>);
    struct RecordingDevice(Arc<std::sync::Mutex<Vec<DeviceCommand>>>);
    impl DeviceService for RecordingDevice {
        fn try_submit(&self, command: DeviceCommand) -> Result<(), SubmitError<DeviceCommand>> {
            self.0.lock().unwrap().push(command);
            Ok(())
        }
        fn try_next_event(&self) -> Option<DeviceEvent> {
            None
        }
        fn shutdown(&self) {}
    }
    impl LibraryService for RecordingLibrary {
        fn try_submit(&self, command: LibraryCommand) -> Result<(), SubmitError<LibraryCommand>> {
            self.0.lock().unwrap().push(command);
            Ok(())
        }
        fn try_next_event(&self) -> Option<LibraryEvent> {
            None
        }
        fn shutdown(&self) {}
    }
    #[test]
    fn native_import_is_distinct_and_export_uses_immutable_reviewed_revision() {
        assert!(matches!(
            import_command(IntentId(1), "/tmp/Lead.PRST".into()),
            LibraryCommand::ImportNative { .. }
        ));
        assert!(matches!(
            import_command(IntentId(1), "/tmp/Lead.gp200bundle".into()),
            LibraryCommand::ImportBundle { .. }
        ));
        let mut app = app();
        let commands = Arc::new(std::sync::Mutex::new(Vec::new()));
        app.services.library = Some(Box::new(RecordingLibrary(commands.clone())));
        app.native_export_pending = Some((
            LibraryVersionId("reviewed-version".into()),
            NativeExportMode::PreserveOriginal,
        ));
        app.native_export_mode = NativeExportMode::PresetOnly;
        app.handle_file(FilePurpose::ExportNative, "/tmp/lead.prst".into());
        app.services.pump();
        assert!(
            matches!(commands.lock().unwrap().as_slice(), [LibraryCommand::ExportNative { request:NativeExportRequest { version, mode:NativeExportMode::PreserveOriginal, .. }, .. }] if version.0 == "reviewed-version")
        );
    }
    #[test]
    fn dirty_navigation_cancel_and_asset_review_enqueue_no_mutations() {
        let mut app = app();
        let original = app.controller.active_document_id().unwrap().clone();
        app.edit(EditPatch::SetParameter {
            module: ModuleId::new(3).unwrap(),
            model: ModelCode(0x07000015),
            model_index: ModelIndex(13),
            parameter: ParamId::new(0).unwrap(),
            value: F32Bits(51.0_f32.to_bits()),
        });
        app.request_switch(PendingSwitch::Snapshot(gp200_sim::captured_demo_snapshot()));
        assert!(app.pending_switch.is_some());
        app.pending_switch = None;
        assert_eq!(app.controller.active_document_id(), Some(&original));
        assert!(app.controller.active_document().unwrap().dirty());
        app.asset_review = Some(
            app.asset_options
                .specification(
                    AssetKind::ImpulseResponse,
                    "/tmp/input.wav".into(),
                    JobId(3),
                )
                .unwrap(),
        );
        app.asset_review = None;
        assert!(!app.services.has_pending());
        assert!(app.local_asset_jobs.is_empty());
    }
    #[test]
    fn copy_and_clear_wait_for_durable_backup_before_any_device_write() {
        for clear in [false, true] {
            let mut app = app();
            let device = Arc::new(std::sync::Mutex::new(Vec::new()));
            let library = Arc::new(std::sync::Mutex::new(Vec::new()));
            app.services.device = Box::new(RecordingDevice(device.clone()));
            app.services.library = Some(Box::new(RecordingLibrary(library.clone())));
            let slot = SlotId::new(23).unwrap();
            let connect =
                app.controller
                    .dispatch(AppInput::Ui(UiIntent::Connect(DeviceSelection {
                        input_id: "test-in".into(),
                        output_id: "test-out".into(),
                        fingerprint_hint: None,
                    })));
            let Effect::Device(DeviceCommand::Connect { intent, .. }) = connect[0] else {
                panic!("connect");
            };
            app.controller
                .dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
                    epoch: SessionEpoch(1),
                    state: ConnectionState::Ready,
                }));
            app.controller
                .dispatch(AppInput::Device(DeviceEvent::SessionChanged {
                    epoch: SessionEpoch(1),
                    identity: DeviceIdentity {
                        model: DeviceModel::Gp200,
                        mode: BackendMode::Simulator,
                        firmware: None,
                        display_name: "Simulator".into(),
                    },
                    capabilities: Capabilities::simulator(),
                }));
            app.controller
                .dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
                    stamp: RequestStamp {
                        epoch: SessionEpoch(1),
                        intent,
                    },
                    target: PresetTarget::EditBuffer,
                    snapshot: gp200_sim::captured_demo_snapshot(),
                }));
            let read = app
                .controller
                .dispatch(AppInput::Ui(UiIntent::Refresh(PresetTarget::Stored(slot))));
            let Effect::Device(DeviceCommand::RefreshTarget { stamp, .. }) = read[0] else {
                panic!("read");
            };
            let mut raw = gp200_sim::captured_demo_snapshot().raw.payload().to_vec();
            raw[4..6].copy_from_slice(&1_u16.to_le_bytes());
            let before = Arc::new(PresetSnapshot::from_raw(
                Arc::new(RawTransfer::from_payload(0x12, raw).unwrap()),
                SnapshotSource::Simulator,
                Some(PresetTarget::Stored(slot)),
                Revision(7),
            ));
            app.controller
                .dispatch(AppInput::Device(DeviceEvent::SnapshotReceived {
                    stamp,
                    target: PresetTarget::Stored(slot),
                    snapshot: before,
                }));
            app.review_stored_write(clear);
            let review = app.stored_write_review.take().unwrap();
            let intent =
                app.preserve_write_snapshot(review.before.clone(), "Destination backup".into());
            app.stored_write_wait = Some(StoredWriteWait::Backup { intent, review });
            app.services.pump();
            assert_eq!(library.lock().unwrap().len(), 1);
            assert!(device.lock().unwrap().is_empty());
            app.observe_stored_write(&AppInput::Library(LibraryEvent::VersionStored {
                intent,
                version: LibraryVersionId("durable-backup".into()),
            }));
            app.services.pump();
            if !clear {
                assert_eq!(library.lock().unwrap().len(), 2);
                assert!(device.lock().unwrap().is_empty());
                let Some(StoredWriteWait::Source { intent, .. }) = app.stored_write_wait.as_ref()
                else {
                    panic!("source save");
                };
                app.observe_stored_write(&AppInput::Library(LibraryEvent::VersionStored {
                    intent: *intent,
                    version: LibraryVersionId("durable-source".into()),
                }));
                app.services.pump();
            }
            let commands = device.lock().unwrap();
            assert!(
                matches!(commands.as_slice(), [DeviceCommand::ClearStoredPreset { slot:actual, expected_target_revision:Revision(7), .. }] if clear && *actual == slot)
                    || matches!(commands.as_slice(), [DeviceCommand::WriteStoredPreset { slot:actual, expected_target_revision:Revision(7), .. }] if !clear && *actual == slot)
            );
        }
    }
}
