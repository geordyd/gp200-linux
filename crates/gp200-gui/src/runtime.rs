//! Service adapter and file-dialog/preferences workers. No I/O occurs in widgets.
use crate::firmware_device_bridge::{
    FirmwareLibraryEndpoint, FirmwareServiceEndpoint, InjectedNormalProxy, LibraryBundle,
    RuntimeAdmission, RuntimeFirmwareLease, paused,
};
pub use crate::preferences::{config_path, load_preferences};
use gp200_domain::*;
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilePurpose {
    ImportPresets,
    ExportPresets,
    ExportNative,
    NamInput,
    NamCalibrationInput,
    ExportNamWav,
    ExportNamClo,
    Backup,
    ImportIr,
    ImportNam,
    ImportSnapTone,
    Firmware,
    ExportFirmwareReport,
    ExportFirmwarePreparation(u64),
    ExportFirmwareAttempt(u64),
    ReviewIr,
    ReviewSnapTone,
    ExportAssetJobReport,
    ExportIrHistory,
    ImportDeviceBackup,
    ExportDeviceBackup,
}
impl FilePurpose {
    pub fn directory_purpose(self) -> DialogDirectoryPurpose {
        match self {
            Self::ImportPresets => DialogDirectoryPurpose::PresetImport,
            Self::ExportPresets | Self::ExportNative => DialogDirectoryPurpose::PresetExport,
            Self::ImportIr | Self::ReviewIr => DialogDirectoryPurpose::IrImport,
            Self::ImportNam => DialogDirectoryPurpose::NamImport,
            Self::ImportSnapTone | Self::ReviewSnapTone => DialogDirectoryPurpose::SnapToneImport,
            Self::ExportNamWav
            | Self::ExportNamClo
            | Self::ExportAssetJobReport
            | Self::ExportIrHistory => DialogDirectoryPurpose::AssetExport,
            Self::NamInput => DialogDirectoryPurpose::NamInput,
            Self::NamCalibrationInput => DialogDirectoryPurpose::CalibrationStimulus,
            Self::Backup => DialogDirectoryPurpose::LibraryBackup,
            Self::ImportDeviceBackup => DialogDirectoryPurpose::DeviceBackupImport,
            Self::ExportDeviceBackup => DialogDirectoryPurpose::DeviceBackupExport,
            Self::Firmware
            | Self::ExportFirmwareReport
            | Self::ExportFirmwarePreparation(_)
            | Self::ExportFirmwareAttempt(_) => DialogDirectoryPurpose::Firmware,
        }
    }
}
#[derive(Debug)]
pub struct FileSelection {
    pub purpose: FilePurpose,
    pub path: Option<PathBuf>,
}
enum WorkerCommand {
    Preferences(AppPreferences),
    Dialog(FilePurpose),
    OpenLink(crate::ui::help::OfficialLink),
    ForgetDirectory(Option<DialogDirectoryPurpose>),
    Screenshot(PathBuf, std::sync::Arc<eframe::egui::ColorImage>),
    ExportAsset(AssetInfo, PathBuf),
    ExportFirmwarePreparation(u64, Box<crate::LocalFirmwarePreparation>, PathBuf),
    #[cfg(test)]
    TestBarrier(SyncSender<()>, Receiver<()>),
    #[cfg(test)]
    TestFileSelection(FileSelection),
    #[cfg(test)]
    TestPanic,
    Shutdown,
}

/// One count spans queued/running commands and their unconsumed replies. A reply
/// acquires its count before publication and before the command count can drop,
/// so worker completion can never transiently appear settled before UI handling.
struct FileWorkerWork {
    outstanding: AtomicUsize,
    panicked: AtomicBool,
    wake: WakeUi,
}
impl FileWorkerWork {
    fn acquire(self: &Arc<Self>) -> FileWorkAcknowledgement {
        self.outstanding.fetch_add(1, Ordering::SeqCst);
        FileWorkAcknowledgement(self.clone())
    }
    fn settled(&self) -> bool {
        !self.panicked.load(Ordering::SeqCst) && self.outstanding.load(Ordering::SeqCst) == 0
    }
}
/// Dropping a command or reply acknowledges only its own accepted work. Rejected
/// offers and unwinding paths release their token without leaking the counter.
pub(crate) struct FileWorkAcknowledgement(Arc<FileWorkerWork>);
impl Drop for FileWorkAcknowledgement {
    fn drop(&mut self) {
        if thread::panicking() {
            self.0.panicked.store(true, Ordering::SeqCst);
        }
        let previous = self.0.outstanding.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(previous > 0);
        (self.0.wake)();
    }
}
struct TrackedWorkerCommand {
    command: WorkerCommand,
    _acknowledgement: FileWorkAcknowledgement,
}
struct FileWorkerCommands {
    sender: SyncSender<TrackedWorkerCommand>,
    work: Arc<FileWorkerWork>,
}
impl FileWorkerCommands {
    fn try_send(&self, command: WorkerCommand) -> Result<(), mpsc::TrySendError<WorkerCommand>> {
        let envelope = TrackedWorkerCommand {
            command,
            _acknowledgement: self.work.acquire(),
        };
        self.sender.try_send(envelope).map_err(|error| match error {
            mpsc::TrySendError::Full(envelope) => mpsc::TrySendError::Full(envelope.command),
            mpsc::TrySendError::Disconnected(envelope) => {
                mpsc::TrySendError::Disconnected(envelope.command)
            }
        })
    }
    #[cfg(test)]
    fn send(&self, command: WorkerCommand) -> Result<(), mpsc::SendError<WorkerCommand>> {
        self.sender
            .send(TrackedWorkerCommand {
                command,
                _acknowledgement: self.work.acquire(),
            })
            .map_err(|error| mpsc::SendError(error.0.command))
    }
}
struct TrackedWorkerEvent {
    event: WorkerEvent,
    acknowledgement: FileWorkAcknowledgement,
}
struct FileWorkerEvents {
    sender: SyncSender<TrackedWorkerEvent>,
    work: Arc<FileWorkerWork>,
}
impl FileWorkerEvents {
    fn send(&self, event: WorkerEvent) -> Result<(), mpsc::SendError<TrackedWorkerEvent>> {
        self.sender.send(TrackedWorkerEvent {
            event,
            acknowledgement: self.work.acquire(),
        })
    }
}
struct FileWorkerPanicGuard(Arc<FileWorkerWork>);
impl Drop for FileWorkerPanicGuard {
    fn drop(&mut self) {
        if thread::panicking() {
            // A panicking worker cannot acknowledge the result of its partial
            // work. Keep the explicit firmware-held close fail-closed.
            self.0.panicked.store(true, Ordering::SeqCst);
            (self.0.wake)();
        }
    }
}
struct PendingFileSelectionAcknowledgement {
    purpose: FilePurpose,
    path: Option<PathBuf>,
    acknowledgement: FileWorkAcknowledgement,
}
pub(crate) type FirmwarePreparationExportResult = (u64, Result<(PathBuf, String), String>);
enum WorkerEvent {
    File(FileSelection),
    Directories(DialogDirectories),
    LinkOpened(String),
    Error(String),
    ScreenshotSaved(PathBuf),
    AssetExported(AssetId, PathBuf),
    FirmwarePreparationExported(u64, Result<(PathBuf, String), String>),
}

pub struct RuntimeServices {
    pub device: Box<dyn DeviceService>,
    pub library: Option<Box<dyn LibraryService>>,
    pub assets: Option<Box<dyn AssetService>>,
    /// Injected normal-only fixtures. Concrete bootstrap keeps its client private.
    pub workflows: Option<gp200_library::LibraryWorkflowClient>,
    actual_workflows: Option<crate::firmware_device_bridge::NormalWorkflowProxy>,
    pub draft_recovery: Option<gp200_library::DraftRecoveryWorker>,
    pending_workflows: VecDeque<gp200_library::WorkflowCommand>,
    pending: VecDeque<Effect>,
    admission: std::sync::Arc<RuntimeAdmission>,
    pub(crate) firmware_device: Option<FirmwareServiceEndpoint>,
    pub(crate) firmware_library: Option<FirmwareLibraryEndpoint>,
    pending_rejections: VecDeque<AppInput>,
    pending_rejected_controls: VecDeque<DeviceCommand>,
    pub(crate) lost_rejections: usize,
    wake: WakeUi,
    #[cfg(test)]
    restoration_failure_gate:
        std::sync::Mutex<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>>,
    commands: FileWorkerCommands,
    events: Receiver<TrackedWorkerEvent>,
    pending_file_acknowledgements: VecDeque<PendingFileSelectionAcknowledgement>,
    pub errors: VecDeque<String>,
    pub screenshot_saved: bool,
    directory_update: Option<DialogDirectories>,
    pub last_link_notice: Option<String>,
    pub last_asset_export: Option<(AssetId, PathBuf)>,
    pub(crate) firmware_preparation_export: Option<FirmwarePreparationExportResult>,
    #[cfg(test)]
    pub(crate) dialog_requests: Option<Vec<FilePurpose>>,
}
impl RuntimeServices {
    pub fn new(device: Box<dyn DeviceService>, wake: WakeUi) -> Self {
        Self::with_preferences(device, wake, AppPreferences::default())
    }
    pub fn with_preferences(
        device: Box<dyn DeviceService>,
        wake: WakeUi,
        preferences: AppPreferences,
    ) -> Self {
        Self::with_preferences_path(device, wake, preferences, config_path())
    }
    pub(crate) fn with_preferences_path(
        device: Box<dyn DeviceService>,
        wake: WakeUi,
        preferences: AppPreferences,
        preference_path: PathBuf,
    ) -> Self {
        let admission = std::sync::Arc::new(RuntimeAdmission::default());
        let device = Box::new(InjectedNormalProxy {
            service: device,
            gate: admission.clone(),
        });
        Self::with_gate(device, wake, preferences, preference_path, admission)
    }
    pub(crate) fn with_midi_preferences(
        service: gp200_transport::MidiDeviceService,
        wake: WakeUi,
        preferences: AppPreferences,
    ) -> Self {
        let admission = std::sync::Arc::new(RuntimeAdmission::default());
        let (device, endpoint) = FirmwareServiceEndpoint::split(service, admission.clone());
        let mut runtime = Self::with_gate(device, wake, preferences, config_path(), admission);
        runtime.firmware_device = Some(endpoint);
        runtime
    }
    /// Same concrete MIDI endpoint and worker, with only the preference file
    /// destination changed to an isolated test directory.
    #[cfg(test)]
    pub(crate) fn with_midi_preferences_path(
        service: gp200_transport::MidiDeviceService,
        wake: WakeUi,
        preferences: AppPreferences,
        preference_path: PathBuf,
    ) -> Self {
        let admission = Arc::new(RuntimeAdmission::default());
        let (device, endpoint) = FirmwareServiceEndpoint::split(service, admission.clone());
        let mut runtime = Self::with_gate(device, wake, preferences, preference_path, admission);
        runtime.firmware_device = Some(endpoint);
        runtime
    }
    fn with_gate(
        device: Box<dyn DeviceService>,
        wake: WakeUi,
        mut preferences: AppPreferences,
        preference_path: PathBuf,
        admission: std::sync::Arc<RuntimeAdmission>,
    ) -> Self {
        let runtime_wake = wake.clone();
        let work = Arc::new(FileWorkerWork {
            outstanding: AtomicUsize::new(0),
            panicked: AtomicBool::new(false),
            wake: runtime_wake.clone(),
        });
        let (sender, rx) = mpsc::sync_channel(32);
        let commands = FileWorkerCommands {
            sender,
            work: work.clone(),
        };
        let (sender, events) = mpsc::sync_channel(32);
        let tx = FileWorkerEvents {
            sender,
            work: work.clone(),
        };
        thread::Builder::new()
            .name("gp200-ui-files".into())
            .spawn(move || {
                let _panic_guard = FileWorkerPanicGuard(work);
                while let Ok(envelope) = rx.recv() {
                    let TrackedWorkerCommand { command, _acknowledgement } = envelope;
                    let event = match command {
                        WorkerCommand::Shutdown => break,
                        #[cfg(test)]
                        WorkerCommand::TestBarrier(entered, release) => {
                            let _ = entered.send(());
                            let _ = release.recv();
                            None
                        }
                        #[cfg(test)]
                        WorkerCommand::TestFileSelection(selection) => Some(WorkerEvent::File(selection)),
                        #[cfg(test)]
                        WorkerCommand::TestPanic => panic!("Injected file-worker panic"),
                        WorkerCommand::ExportFirmwarePreparation(id, report, destination) => {
                            Some(WorkerEvent::FirmwarePreparationExported(id,
                                export_firmware_preparation(&report, &destination)
                                    .map(|hash| (destination, hash))))
                        }
                        WorkerCommand::ExportAsset(info, destination) => {
                            Some(match export_retained_asset(&info, &destination) {
                                Ok(()) => WorkerEvent::AssetExported(info.id, destination),
                                Err(error) => WorkerEvent::Error(std::format!("Local asset export failed: {error}"
                                )),
                            })
                        }
                        WorkerCommand::Preferences(mut changed) => {
                            // The worker owns directory history. Appearance events queued
                            // while a dialog is open cannot overwrite its later selection.
                            changed.dialog_directories = preferences.dialog_directories.clone();
                            preferences = changed;
                            crate::preferences::save_preferences_at(&preference_path, &preferences)
                                .err().map(|error| WorkerEvent::Error(std::format!("Preferences were not saved: {error}")))
                        }
                        WorkerCommand::OpenLink(link) => Some(match open_browser(link.url()) {
                            Ok(()) => WorkerEvent::LinkOpened(std::format!("The desktop accepted the browser request for {}.", link.label())),
                            Err(error) => WorkerEvent::Error(std::format!("Could not open {}. Copy this address into your browser: {} ({error})", link.label(), link.url())),
                        }),
                        WorkerCommand::ForgetDirectory(purpose) => {
                            if let Some(purpose) = purpose { preferences.dialog_directories.remove(&purpose); }
                            else { preferences.dialog_directories.clear(); }
                            if let Err(error) = crate::preferences::save_preferences_at(&preference_path, &preferences)
                                && tx.send(WorkerEvent::Error(std::format!("Remembered folders were reset for this session but could not be saved: {error}"))).is_err() { break; }
                            Some(WorkerEvent::Directories(preferences.dialog_directories.clone()))
                        }
                        WorkerCommand::Screenshot(path, pixels) => {
                            let bytes: Vec<u8> = pixels
                                .pixels
                                .iter()
                                .flat_map(|pixel| pixel.to_array())
                                .collect();
                            match image::save_buffer_with_format(
                                &path,
                                &bytes,
                                pixels.size[0] as u32,
                                pixels.size[1] as u32,
                                image::ColorType::Rgba8,
                                image::ImageFormat::Png,
                            ) {
                                Ok(()) => Some(WorkerEvent::ScreenshotSaved(path)),
                                Err(error) => Some(WorkerEvent::Error(std::format!("Screenshot save failed: {error}"
                                ))),
                            }
                        }
                        WorkerCommand::Dialog(purpose) => {
                            let before = preferences.dialog_directories.clone();
                            let directory = crate::preferences::initial_directory(&mut preferences.dialog_directories, purpose.directory_purpose());
                            let mut dialog = rfd::FileDialog::new().set_title(crate::localization::text_for(preferences.language, match purpose {
                                FilePurpose::ImportPresets => crate::localization::text("Import presets or portable bundle"),
                                FilePurpose::ExportPresets => crate::localization::text("Export portable preset bundle"),
                                FilePurpose::ExportNative => crate::localization::text("Export native GP-200 preset"),
                                FilePurpose::NamInput => crate::localization::text("Select NAM rendering input WAV"),
                                FilePurpose::ExportNamWav => crate::localization::text("Export retained NAM render WAV"),
                                FilePurpose::ExportNamClo => crate::localization::text("Export retained experimental CLO"),
                                FilePurpose::NamCalibrationInput => {
                                    crate::localization::text("Select original GP-200 V1.8.1 calibration stimulus WAV")
                                }
                                FilePurpose::Backup => crate::localization::text("Save library backup"),
                                FilePurpose::ImportIr => crate::localization::text("Select an impulse response WAV"),
                                FilePurpose::ImportNam => crate::localization::text("Select a NAM model"),
                                FilePurpose::ImportSnapTone => crate::localization::text("Select a SnapTone source"),
                                FilePurpose::Firmware => crate::localization::text("Select GP-200 firmware"),
                                FilePurpose::ExportFirmwareReport => crate::localization::text("Export offline firmware review"),
                                FilePurpose::ExportFirmwarePreparation(_) => "Export preparation and proposed loss review",
                                FilePurpose::ExportFirmwareAttempt(_) => "Export checked firmware attempt",
                                FilePurpose::ReviewIr => crate::localization::text("Review an exact IR source"),
                                FilePurpose::ReviewSnapTone => crate::localization::text("Review an exact SnapTone container"),
                                FilePurpose::ExportAssetJobReport => crate::localization::text("Export asset device review"),
                                FilePurpose::ExportIrHistory => crate::localization::text("Export local IR evidence"),
                                FilePurpose::ImportDeviceBackup => crate::localization::text("Import a device bank archive"),
                                FilePurpose::ExportDeviceBackup => crate::localization::text("Export a device bank archive"),
                            }));
                            if let Some(directory) = directory { dialog = dialog.set_directory(directory); }
                            let path = match purpose {
                                FilePurpose::ExportFirmwarePreparation(_) => dialog
                                    .set_file_name("gp200-firmware-preparation.json")
                                    .add_filter("Preparation and loss review", &["json"])
                                    .save_file(),
                                FilePurpose::ExportFirmwareAttempt(_) => dialog
                                    .set_file_name("gp200-firmware-attempt.zip")
                                    .add_filter("Original firmware evidence", &["zip"])
                                    .save_file(),
                                FilePurpose::ExportIrHistory => dialog
                                    .set_file_name("gp200-ir-evidence.zip")
                                    .add_filter(crate::localization::text_for(preferences.language, "Local IR evidence ZIP"), &["zip"])
                                    .save_file(),
                                FilePurpose::ExportAssetJobReport => dialog
                                    .set_file_name("gp200-asset-review.json")
                                    .add_filter(crate::localization::text_for(preferences.language, "Asset review report"), &["json"])
                                    .save_file(),
                                FilePurpose::ExportFirmwareReport => dialog
                                    .set_file_name("gp200-firmware-review.json")
                                    .add_filter(crate::localization::text_for(preferences.language, "Firmware review report"), &["json"])
                                    .save_file(),
                                FilePurpose::ExportNamWav => dialog
                                    .set_file_name("nam-render.wav")
                                    .add_filter(crate::localization::text_for(preferences.language, "Wave audio"), &["wav"])
                                    .save_file(),
                                FilePurpose::ExportNamClo => dialog
                                    .set_file_name("nam-calibrated.clo")
                                    .add_filter(crate::localization::text_for(preferences.language, "SnapTone CLO"), &["clo"])
                                    .save_file(),
                                FilePurpose::ExportDeviceBackup => dialog
                                    .set_file_name("gp200-bank.gp200backup")
                                    .add_filter(crate::localization::text_for(preferences.language, "GP200 Linux device archive"), &["gp200backup"])
                                    .save_file(),
                                FilePurpose::ImportDeviceBackup => dialog
                                    .add_filter(crate::localization::text_for(preferences.language, "GP200 Linux device archive"), &["gp200backup"])
                                    .pick_file(),
                                FilePurpose::ExportNative => dialog
                                    .set_file_name("preset.prst")
                                    .add_filter(crate::localization::text_for(preferences.language, "GP-200 preset"), &["prst"])
                                    .save_file(),
                                FilePurpose::ExportPresets | FilePurpose::Backup => dialog
                                    .set_file_name("gp200-library.gp200bundle")
                                    .save_file(),
                                FilePurpose::ImportIr
                                | FilePurpose::ReviewIr
                                | FilePurpose::NamInput
                                | FilePurpose::NamCalibrationInput => {
                                    dialog.add_filter(crate::localization::text_for(preferences.language, "Wave audio"), &["wav"]).pick_file()
                                }
                                FilePurpose::ImportPresets => dialog
                                    .add_filter(
                                        crate::localization::text("GP-200 presets or portable bundles"),
                                        &["prst", "gp200bundle"],
                                    )
                                    .pick_file(),
                                FilePurpose::ImportNam => {
                                    dialog.add_filter(crate::localization::text_for(preferences.language, "Neural amp model"), &["nam"]).pick_file()
                                }
                                FilePurpose::ReviewSnapTone => dialog.add_filter(crate::localization::text_for(preferences.language, "SnapTone CLO"), &["clo"]).pick_file(),
                                _ => dialog.pick_file(),
                            };
                            crate::preferences::remember_selection(&mut preferences.dialog_directories, purpose.directory_purpose(), path.as_deref());
                            if preferences.dialog_directories != before {
                                if let Err(error) = crate::preferences::save_preferences_at(&preference_path, &preferences)
                                    && tx.send(WorkerEvent::Error(std::format!("The selected folder is remembered for this session but could not be saved: {error}"))).is_err() { break; }
                                if tx.send(WorkerEvent::Directories(preferences.dialog_directories.clone())).is_err() { break; }
                            }
                            Some(WorkerEvent::File(FileSelection { purpose, path }))
                        }
                    };
                    if let Some(event) = event {
                        if tx.send(event).is_err() {
                            break;
                        }
                        wake();
                    }
                }
            })
            .expect("UI file worker starts");
        Self {
            device,
            library: None,
            assets: None,
            workflows: None,
            actual_workflows: None,
            draft_recovery: None,
            pending_workflows: VecDeque::new(),
            pending: VecDeque::new(),
            admission,
            firmware_device: None,
            firmware_library: None,
            pending_rejections: VecDeque::new(),
            pending_rejected_controls: VecDeque::new(),
            lost_rejections: 0,
            wake: runtime_wake,
            #[cfg(test)]
            restoration_failure_gate: std::sync::Mutex::new(None),
            commands,
            events,
            pending_file_acknowledgements: VecDeque::new(),
            errors: VecDeque::new(),
            screenshot_saved: false,
            directory_update: None,
            last_asset_export: None,
            firmware_preparation_export: None,
            #[cfg(test)]
            dialog_requests: None,
            last_link_notice: None,
        }
    }
    pub fn enqueue(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            if self.firmware_paused() && !matches!(&effect, Effect::PersistSettings(_)) {
                self.reject_unoffered(effect);
                continue;
            }
            if self.pending.len() < 256 {
                self.pending.push_back(effect);
            } else {
                self.error(crate::localization::text(
                    "Service queue is full. Wait for running work before retrying.",
                ));
            }
        }
    }
    pub(crate) fn firmware_paused(&self) -> bool {
        self.admission
            .frozen
            .load(std::sync::atomic::Ordering::Acquire)
    }
    pub(crate) fn freeze_firmware(&mut self) -> Result<RuntimeFirmwareLease, ServiceError> {
        if self
            .pending
            .iter()
            .any(|e| matches!(e, Effect::Library(_) | Effect::Assets(_)))
            || !self.pending_workflows.is_empty()
            || self.lost_rejections != 0
        {
            return Err(ServiceError::new(
                ErrorKind::Busy,
                "Finish queued local work before firmware preparation",
            ));
        }
        let lease = self.admission.freeze()?;
        let old = std::mem::take(&mut self.pending);
        for effect in old {
            if matches!(&effect, Effect::Device(_)) {
                self.reject_unoffered(effect);
            } else {
                self.pending.push_back(effect);
            }
        }
        Ok(lease)
    }
    pub(crate) fn release_firmware(
        &mut self,
        lease: &RuntimeFirmwareLease,
    ) -> Result<(), ServiceError> {
        if !self.pending_rejections.is_empty()
            || !self.pending_rejected_controls.is_empty()
            || self.lost_rejections != 0
        {
            return Err(paused());
        }
        self.admission.release(lease)
    }
    pub(crate) fn firmware_lease_matches(&self, lease: &RuntimeFirmwareLease) -> bool {
        self.admission.matches(lease)
    }
    pub(crate) fn release_firmware_with(
        &self,
        lease: &RuntimeFirmwareLease,
        finish: impl FnOnce() -> Result<(), ServiceError>,
    ) -> Result<(), ServiceError> {
        if !self.firmware_queues_settled() {
            return Err(paused());
        }
        self.admission.release_with(lease, finish)
    }
    pub(crate) fn firmware_queues_settled(&self) -> bool {
        self.pending.is_empty()
            && self.pending_workflows.is_empty()
            && self.pending_rejections.is_empty()
            && self.pending_rejected_controls.is_empty()
            && self.lost_rejections == 0
    }
    fn reject_unoffered(&mut self, effect: Effect) {
        if let Effect::Device(
            command @ (DeviceCommand::Connect { .. }
            | DeviceCommand::Discover
            | DeviceCommand::Disconnect { .. }
            | DeviceCommand::Shutdown),
        ) = effect
        {
            if self.pending_rejected_controls.len() < 256 {
                self.pending_rejected_controls.push_back(command);
            } else {
                self.lost_rejections = self.lost_rejections.saturating_add(1);
            }
            return;
        }
        let input = match effect {
            Effect::Device(command) => device_stamp(&command).map(|stamp| {
                AppInput::Device(DeviceEvent::OperationFailed {
                    stamp,
                    error: paused(),
                })
            }),
            Effect::Library(command) => library_intent(&command).map(|intent| {
                AppInput::Library(LibraryEvent::Failed {
                    intent,
                    error: paused(),
                })
            }),
            Effect::Assets(
                AssetCommand::Inspect(spec)
                | AssetCommand::Prepare(spec)
                | AssetCommand::Convert(spec),
            ) => Some(AppInput::Assets(AssetEvent::Failed {
                job: spec.id,
                error: paused(),
            })),
            _ => None,
        };
        if let Some(input) = input {
            if self.pending_rejections.len() < 1024 {
                self.pending_rejections.push_back(input);
            } else {
                self.lost_rejections = self.lost_rejections.saturating_add(1);
            }
        } else {
            self.error(paused().message);
        }
    }
    pub(crate) fn take_unoffered_controls(&mut self) -> impl Iterator<Item = DeviceCommand> + '_ {
        self.pending_rejected_controls.drain(..)
    }
    pub(crate) fn install_library_worker(&mut self, worker: gp200_library::LibraryWorker) {
        self.install_library_bundle(LibraryBundle::from_worker(worker, self.admission.clone()));
    }
    pub(crate) fn install_library_bundle(&mut self, bundle: LibraryBundle) {
        self.library = Some(bundle.service);
        self.workflows = None;
        self.actual_workflows = Some(bundle.workflows);
        self.firmware_library = Some(bundle.endpoint);
    }
    pub(crate) fn retire_library_clients(&mut self) {
        self.library = None;
        self.actual_workflows = None;
        self.workflows = None;
    }
    pub fn workflow_available(&self) -> bool {
        self.actual_workflows.is_some() || self.workflows.is_some()
    }
    fn workflow_submit(
        &self,
        command: gp200_library::WorkflowCommand,
    ) -> Result<(), SubmitError<gp200_library::WorkflowCommand>> {
        if let Some(actual) = &self.actual_workflows {
            actual.try_submit(command)
        } else if let Some(injected) = &self.workflows {
            injected.try_submit(command)
        } else {
            Err(SubmitError::Disconnected(command))
        }
    }
    fn workflow_event(&self) -> Option<gp200_library::WorkflowEvent> {
        if let Some(actual) = &self.actual_workflows {
            actual.try_next_event()
        } else {
            self.workflows.as_ref().and_then(|c| c.try_next_event())
        }
    }
    pub(crate) fn restore_library(&self, store: gp200_library::LibraryStore) -> StoreRestoration {
        #[cfg(test)]
        if let Some(gate) = self.restoration_failure_gate.lock().unwrap().take() {
            return StoreRestoration::start_inner(
                store,
                self.admission.clone(),
                self.wake.clone(),
                Some(gate),
            );
        }
        StoreRestoration::start(store, self.admission.clone(), self.wake.clone())
    }
    #[cfg(test)]
    pub(crate) fn fail_next_library_restoration_before_worker_offer(
        &self,
        gate: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        *self.restoration_failure_gate.lock().unwrap() = Some(gate);
    }
    pub(crate) fn open_official_link(&mut self, link: crate::ui::help::OfficialLink) {
        self.last_link_notice =
            Some(crate::localization::text("Opening the official page in your browser…").into());
        if self
            .commands
            .try_send(WorkerCommand::OpenLink(link))
            .is_err()
        {
            self.last_link_notice = None;
            self.error(crate::localization::text("The file worker is busy. Retry opening the official page after running work finishes."));
        }
    }
    pub fn forget_dialog_directory(&mut self, purpose: Option<DialogDirectoryPurpose>) {
        if self
            .commands
            .try_send(WorkerCommand::ForgetDirectory(purpose))
            .is_err()
        {
            self.error(crate::localization::text("The file worker is busy. Retry resetting remembered folders after running work finishes."));
        }
    }
    pub fn take_directory_update(&mut self) -> Option<DialogDirectories> {
        self.directory_update.take()
    }
    pub fn dialog(&mut self, purpose: FilePurpose) {
        self.try_dialog(purpose);
    }
    pub fn try_dialog(&mut self, purpose: FilePurpose) -> bool {
        #[cfg(test)]
        if let Some(requests) = &mut self.dialog_requests {
            requests.push(purpose);
            return true;
        }
        if self
            .commands
            .try_send(WorkerCommand::Dialog(purpose))
            .is_err()
        {
            self.error(crate::localization::text(
                "A file dialog is already pending. Close it before opening another.",
            ));
            return false;
        }
        true
    }
    pub fn export_asset(&mut self, info: AssetInfo, destination: PathBuf) {
        if self
            .commands
            .try_send(WorkerCommand::ExportAsset(info, destination))
            .is_err()
        {
            self.error(crate::localization::text(
                "The file worker is busy. Retry the local export after running work finishes.",
            ));
        }
    }
    pub(crate) fn export_firmware_preparation(
        &mut self,
        id: u64,
        report: crate::LocalFirmwarePreparation,
        destination: PathBuf,
    ) -> Result<(), ServiceError> {
        self.commands
            .try_send(WorkerCommand::ExportFirmwarePreparation(id, Box::new(report), destination))
            .map_err(|_| ServiceError::new(ErrorKind::Busy, "The file worker is busy; select the report destination again after local work settles"))
    }
    pub fn pump(&mut self) -> (Vec<AppInput>, Vec<FileSelection>) {
        let mut inputs = Vec::new();
        for _ in 0..64 {
            let Some(input) = self.pending_rejections.pop_front() else {
                break;
            };
            inputs.push(input);
        }
        for _ in 0..64 {
            let Some(effect) = self.pending.pop_front() else {
                break;
            };
            let retry = match effect {
                Effect::Device(command) => match self.device.try_submit(command) {
                    Ok(()) => None,
                    Err(SubmitError::Busy(command)) => Some(Effect::Device(command)),
                    Err(SubmitError::Disconnected(command)) => {
                        let error = ServiceError::new(
                            ErrorKind::Disconnected,
                            crate::localization::text(
                                "Device worker stopped; draft preserved. Reconnect before retrying.",
                            ),
                        );
                        if let Some(stamp) = device_stamp(&command) {
                            inputs.push(AppInput::Device(DeviceEvent::OperationFailed {
                                stamp,
                                error,
                            }));
                        } else {
                            self.error(error.message);
                        }
                        None
                    }
                },
                Effect::Library(command) => match &self.library {
                    Some(service) => match service.try_submit(command) {
                        Ok(()) => None,
                        Err(SubmitError::Busy(command)) => Some(Effect::Library(command)),
                        Err(SubmitError::Disconnected(command)) => {
                            if let Some(intent) = library_intent(&command) {
                                inputs.push(AppInput::Library(LibraryEvent::Failed {
                                    intent,
                                    error: ServiceError::new(
                                        ErrorKind::Disconnected,
                                        crate::localization::text("Library worker stopped"),
                                    ),
                                }));
                            }
                            None
                        }
                    },
                    None => {
                        if let Some(intent) = library_intent(&command) {
                            inputs.push(AppInput::Library(LibraryEvent::Failed {
                                intent,
                                error: ServiceError::unsupported(crate::localization::text(
                                    "Local library worker is not installed",
                                )),
                            }));
                        }
                        None
                    }
                },
                Effect::Assets(command) => match &self.assets {
                    Some(service) => match service.try_submit(command) {
                        Ok(()) => None,
                        Err(SubmitError::Busy(command)) => Some(Effect::Assets(command)),
                        Err(SubmitError::Disconnected(_command)) => {
                            self.error(crate::localization::text(
                                "Asset worker stopped; retained sources are unchanged",
                            ));
                            None
                        }
                    },
                    None => {
                        self.error(crate::localization::text("Asset worker is not installed"));
                        None
                    }
                },
                Effect::PersistSettings(preferences) => match self
                    .commands
                    .try_send(WorkerCommand::Preferences(preferences))
                {
                    Ok(()) => None,
                    Err(mpsc::TrySendError::Full(WorkerCommand::Preferences(preferences))) => {
                        Some(Effect::PersistSettings(preferences))
                    }
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        self.error(crate::localization::text("Preference worker stopped; current theme stays applied but was not persisted"));
                        None
                    }
                    Err(mpsc::TrySendError::Full(_)) => unreachable!(),
                },
            };
            if let Some(effect) = retry {
                self.pending.push_front(effect);
                break;
            }
        }
        for _ in 0..64 {
            let Some(event) = self.device.try_next_event() else {
                break;
            };
            inputs.push(AppInput::Device(event));
        }
        if let Some(service) = &self.library {
            for _ in 0..64 {
                let Some(event) = service.try_next_event() else {
                    break;
                };
                inputs.push(AppInput::Library(event));
            }
        }
        if let Some(service) = &self.assets {
            for _ in 0..64 {
                let Some(event) = service.try_next_event() else {
                    break;
                };
                inputs.push(AppInput::Assets(event));
            }
        }
        let mut selections = Vec::new();
        for _ in 0..32 {
            let Ok(TrackedWorkerEvent {
                event,
                acknowledgement,
            }) = self.events.try_recv()
            else {
                break;
            };
            match event {
                WorkerEvent::File(file) => {
                    // Keep the reply counted after pump returns, through actual
                    // selection handling (or recording the original held choice).
                    self.pending_file_acknowledgements.push_back(
                        PendingFileSelectionAcknowledgement {
                            purpose: file.purpose,
                            path: file.path.clone(),
                            acknowledgement,
                        },
                    );
                    selections.push(file);
                    continue;
                }
                WorkerEvent::Directories(directories) => self.directory_update = Some(directories),
                WorkerEvent::LinkOpened(notice) => self.last_link_notice = Some(notice),
                WorkerEvent::Error(error) => {
                    self.last_link_notice = None;
                    self.error(error);
                }
                WorkerEvent::ScreenshotSaved(path) => {
                    self.screenshot_saved = true;
                    eprintln!("Native screenshot saved: {}", path.display());
                }
                WorkerEvent::AssetExported(id, path) => self.last_asset_export = Some((id, path)),
                WorkerEvent::FirmwarePreparationExported(id, result) => {
                    self.firmware_preparation_export = Some((id, result));
                }
            }
            drop(acknowledgement);
        }
        (inputs, selections)
    }
    /// One GUI consumer owns the cloned client. A full writer queue retains the exact
    /// command and intent; the bounded adapter never drops or regenerates a mutation.
    pub fn submit_workflow(
        &mut self,
        command: gp200_library::WorkflowCommand,
    ) -> Result<(), ServiceError> {
        if self.firmware_paused() {
            return Err(paused());
        }
        if !self.workflow_available() {
            return Err(ServiceError::unsupported(crate::localization::text(
                "Local library workflow worker is unavailable",
            )));
        }
        if self.pending_workflows.len() >= 64 {
            return Err(ServiceError::new(
                ErrorKind::Busy,
                crate::localization::text("Library workflow queue is full. Wait before retrying."),
            ));
        }
        self.pending_workflows.push_back(command);
        Ok(())
    }
    pub fn pump_workflows(&mut self) -> Vec<gp200_library::WorkflowEvent> {
        let mut events = Vec::new();
        for _ in 0..64 {
            let Some(command) = self.pending_workflows.pop_front() else {
                break;
            };
            let failure = |intent| gp200_library::WorkflowEvent {
                intent,
                result: Err(ServiceError::new(
                    ErrorKind::Disconnected,
                    crate::localization::text(
                        "Library workflow worker stopped. Retained captures and before-images remain available.",
                    ),
                )),
            };
            if self.firmware_paused() {
                events.push(gp200_library::WorkflowEvent {
                    intent: command.intent,
                    result: Err(paused()),
                });
                continue;
            }
            match self.workflow_submit(command) {
                Ok(()) => {}
                Err(SubmitError::Busy(command)) => {
                    self.pending_workflows.push_front(command);
                    break;
                }
                Err(SubmitError::Disconnected(command)) => events.push(failure(command.intent)),
            }
        }
        for _ in 0..64 {
            let Some(event) = self.workflow_event() else {
                break;
            };
            events.push(event);
        }
        events
    }
    pub fn screenshot(&mut self, path: PathBuf, pixels: std::sync::Arc<eframe::egui::ColorImage>) {
        if self
            .commands
            .try_send(WorkerCommand::Screenshot(path, pixels))
            .is_err()
        {
            self.error(crate::localization::text(
                "File worker is busy; screenshot could not be queued",
            ));
        }
    }
    /// This observes accepted file/preferences work, not a cached render state.
    /// Preferences retries are still original queued effects; staged replies
    /// remain outstanding until the app applies or records their consequences.
    pub(crate) fn file_worker_settled(&self) -> bool {
        self.commands.work.settled()
            && self.pending_file_acknowledgements.is_empty()
            && self.directory_update.is_none()
            && self.firmware_preparation_export.is_none()
            && !self
                .pending
                .iter()
                .any(|effect| matches!(effect, Effect::PersistSettings(_)))
    }
    pub(crate) fn take_file_selection_acknowledgement(
        &mut self,
        selection: &FileSelection,
    ) -> Option<FileWorkAcknowledgement> {
        let pending = self.pending_file_acknowledgements.front()?;
        if pending.purpose != selection.purpose || pending.path != selection.path {
            // An out-of-order or synthetic selection cannot acknowledge a
            // different accepted worker reply.
            return None;
        }
        self.pending_file_acknowledgements
            .pop_front()
            .map(|pending| pending.acknowledgement)
    }
    /// Deterministic actual-worker queue/running barrier. It substitutes no
    /// device authority and opens no native file dialog.
    #[cfg(test)]
    pub(crate) fn test_queue_file_work_barrier(&self) -> (Receiver<()>, SyncSender<()>) {
        let (entered, observed) = mpsc::sync_channel(1);
        let (release, wait) = mpsc::sync_channel(1);
        self.commands
            .try_send(WorkerCommand::TestBarrier(entered, wait))
            .unwrap();
        (observed, release)
    }
    /// Actual tracked worker publication with only the native dialog result
    /// substituted. The original reply follows the production pump/ack path.
    #[cfg(test)]
    pub(crate) fn test_queue_file_selection(&self, purpose: FilePurpose, path: Option<PathBuf>) {
        self.commands
            .try_send(WorkerCommand::TestFileSelection(FileSelection {
                purpose,
                path,
            }))
            .unwrap();
    }
    pub fn has_pending(&self) -> bool {
        !self.file_worker_settled()
            || !self.pending.is_empty()
            || !self.pending_workflows.is_empty()
            || !self.pending_rejections.is_empty()
    }
    pub fn error(&mut self, error: impl Into<String>) {
        if self.errors.len() >= 32 {
            self.errors.pop_front();
        }
        self.errors.push_back(error.into());
    }
}
impl Drop for RuntimeServices {
    fn drop(&mut self) {
        self.device.shutdown();
        if let Some(service) = &self.library {
            service.shutdown();
        }
        if let Some(service) = &self.assets {
            service.shutdown();
        }
        let _ = self.commands.try_send(WorkerCommand::Shutdown);
    }
}

/// One bounded owning result slot. Losing the UI consumer intentionally retains
/// the actual store/writer owner; it never opens a replacement or joins on UI.
pub(crate) enum StoreRestorationResult {
    Restored(LibraryBundle),
    RetainedFailure {
        error: ServiceError,
        store: gp200_library::LibraryStore,
    },
    Unresolved(ServiceError),
}
struct RestorationSlot {
    store: Option<gp200_library::LibraryStore>,
    result: Option<StoreRestorationResult>,
    anchor: Option<std::sync::Arc<std::sync::Mutex<RestorationSlot>>>,
}
pub(crate) struct StoreRestoration {
    slot: std::sync::Arc<std::sync::Mutex<RestorationSlot>>,
}
impl StoreRestoration {
    fn start(
        store: gp200_library::LibraryStore,
        gate: std::sync::Arc<RuntimeAdmission>,
        wake: WakeUi,
    ) -> Self {
        Self::start_inner(
            store,
            gate,
            wake,
            #[cfg(test)]
            None,
        )
    }
    fn start_inner(
        store: gp200_library::LibraryStore,
        gate: std::sync::Arc<RuntimeAdmission>,
        wake: WakeUi,
        #[cfg(test)] failure_gate: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Self {
        let slot = std::sync::Arc::new(std::sync::Mutex::new(RestorationSlot {
            store: Some(store),
            result: None,
            anchor: None,
        }));
        slot.lock().unwrap_or_else(|e| e.into_inner()).anchor = Some(slot.clone());
        let worker_slot = slot.clone();
        let worker_wake = wake.clone();
        let spawned = thread::Builder::new()
            .name("gp200-library-return".into())
            .spawn(move || {
                let store = worker_slot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .store
                    .take()
                    .expect("sole actual returned store");
                #[cfg(test)]
                if let Some(failure_gate) = failure_gate {
                    // Fault injection occurs on this actual owning helper before
                    // it offers the original store to a new library worker.
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    while !failure_gate.load(std::sync::atomic::Ordering::Acquire)
                        && std::time::Instant::now() < deadline
                    {
                        thread::sleep(std::time::Duration::from_millis(1));
                    }
                    worker_slot.lock().unwrap_or_else(|e| e.into_inner()).result =
                        Some(StoreRestorationResult::RetainedFailure {
                            error: ServiceError::new(
                                ErrorKind::Io,
                                "Injected actual restoration helper failure before worker offer",
                            ),
                            store,
                        });
                    worker_wake();
                    return;
                }
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    gp200_library::LibraryWorker::from_store(store, worker_wake.clone())
                }));
                let result = match result {
                    Ok(Ok(worker)) => {
                        StoreRestorationResult::Restored(LibraryBundle::from_worker(worker, gate))
                    }
                    Ok(Err(failure)) => {
                        let error = failure.error().clone();
                        StoreRestorationResult::RetainedFailure {
                            error,
                            store: failure.into_store(),
                        }
                    }
                    Err(_) => StoreRestorationResult::Unresolved(ServiceError::new(
                        ErrorKind::Indeterminate,
                        "Local writer restoration panicked; actual store ownership is unresolved",
                    )),
                };
                worker_slot.lock().unwrap_or_else(|e| e.into_inner()).result = Some(result);
                worker_wake();
            });
        if let Err(error) = spawned {
            let mut state = slot.lock().unwrap_or_else(|e| e.into_inner());
            let store = state
                .store
                .take()
                .expect("spawn failure retains the original owner in the slot");
            state.result = Some(StoreRestorationResult::RetainedFailure {
                error: ServiceError::new(
                    ErrorKind::Io,
                    format!("Cannot start local restoration helper: {error}"),
                ),
                store,
            });
            wake();
        }
        Self { slot }
    }
    pub(crate) fn try_take(&mut self) -> Option<StoreRestorationResult> {
        let mut slot = self.slot.try_lock().ok()?;
        let result = slot.result.take()?;
        slot.anchor.take();
        Some(result)
    }
}

/// Save the existing preparation projection on the file worker. This serializes
/// retained descriptive facts; it never creates an allowance or rereads a source.
fn export_firmware_preparation(
    report: &crate::LocalFirmwarePreparation,
    destination: &std::path::Path,
) -> Result<String, String> {
    use std::{
        fs::{self, File, OpenOptions},
        io::Write,
        sync::atomic::{AtomicU64, Ordering},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if !destination.is_absolute() {
        return Err("Choose an absolute preparation report destination".into());
    }
    let bytes = serde_json::to_vec_pretty(report).map_err(|e| e.to_string())?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("Preparation review exceeds the 8 MiB local report limit".into());
    }
    let parent = destination
        .parent()
        .ok_or("Choose a report folder")?
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let destination = parent.join(destination.file_name().ok_or("Choose a report filename")?);
    let stage = parent.join(format!(
        ".gp200-preparation-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    struct Stage(PathBuf);
    impl Drop for Stage {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&stage).map_err(|e| e.to_string())?;
    let _cleanup = Stage(stage.clone());
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| e.to_string())?;
    // A report cannot overwrite captured material or another existing file.
    fs::hard_link(&stage, &destination)
        .map_err(|e| format!("Choose a new report filename: {e}"))?;
    File::open(&parent)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(JobSha256::of(&bytes).hex())
}

/// Export exactly one immutable artifact on the file worker. The dialog-selected
/// destination may replace a user file, but never another retained store object.
fn export_retained_asset(info: &AssetInfo, destination: &std::path::Path) -> Result<(), String> {
    use std::{
        fs::{self, File, OpenOptions},
        io::{Read, Write},
        sync::atomic::{AtomicU64, Ordering},
    };
    const MAX_BYTES: u64 = 64 * 1024 * 1024;
    static NEXT_EXPORT: AtomicU64 = AtomicU64::new(0);
    if info.bytes == 0 || info.bytes > MAX_BYTES {
        return Err(crate::localization::text(
            "Retained asset is empty or exceeds the 64 MiB export limit",
        )
        .into());
    }
    let metadata = fs::symlink_metadata(&info.path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() != info.bytes {
        return Err(
            crate::localization::text("Retained source file changed or is unavailable").into(),
        );
    }
    let expected = JobSha256::parse(info.properties.get("sha256").ok_or(
        crate::localization::text("Retained asset lacks its content hash"),
    )?)
    .map_err(|error| error.message)?;
    let store = info
        .path
        .parent()
        .and_then(std::path::Path::parent)
        .ok_or(crate::localization::text("Invalid retained source path"))?
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let directory = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."))
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if directory.starts_with(&store) {
        return Err(crate::localization::text(
            "Choose a destination outside the retained asset store",
        )
        .into());
    }
    let name = destination.file_name().ok_or(crate::localization::text(
        "Export destination needs a filename",
    ))?;
    let destination = directory.join(name);
    let mut bytes = Vec::with_capacity(info.bytes as usize);
    File::open(&info.path)
        .map_err(|error| error.to_string())?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 != info.bytes || JobSha256::of(&bytes) != expected {
        return Err(crate::localization::text(
            "Retained asset hash changed; no destination was replaced",
        )
        .into());
    }
    let stage = directory.join(format!(
        ".gp200-asset-export-{}-{}.tmp",
        std::process::id(),
        NEXT_EXPORT.fetch_add(1, Ordering::Relaxed)
    ));
    struct Stage(std::path::PathBuf);
    impl Drop for Stage {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&stage).map_err(|error| error.to_string())?;
    let _cleanup = Stage(stage.clone());
    file.write_all(&bytes).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    drop(file);
    fs::rename(&stage, destination).map_err(|error| error.to_string())?;
    File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn library_intent(command: &LibraryCommand) -> Option<IntentId> {
    match command {
        LibraryCommand::ImportNative { intent, .. }
        | LibraryCommand::ExportNative { intent, .. } => Some(*intent),
        LibraryCommand::DeleteToTrash { intent, .. }
        | LibraryCommand::RestoreTrash { intent, .. }
        | LibraryCommand::UpdateMetadata { intent, .. }
        | LibraryCommand::Query { intent, .. }
        | LibraryCommand::LoadVersion { intent, .. }
        | LibraryCommand::StoreSnapshot { intent, .. }
        | LibraryCommand::ListVersions { intent, .. }
        | LibraryCommand::ImportBundle { intent, .. }
        | LibraryCommand::ExportBundle { intent, .. }
        | LibraryCommand::Backup { intent, .. } => Some(*intent),
        _ => None,
    }
}

fn device_stamp(command: &DeviceCommand) -> Option<RequestStamp> {
    match command {
        DeviceCommand::ReadIdentity(stamp)
        | DeviceCommand::ReadCurrent(stamp)
        | DeviceCommand::ReadGlobals(stamp)
        | DeviceCommand::ReadAssetMetadata { stamp, .. }
        | DeviceCommand::ReadStored { stamp, .. }
        | DeviceCommand::RefreshTarget { stamp, .. }
        | DeviceCommand::SelectStored { stamp, .. }
        | DeviceCommand::SubmitEdit { stamp, .. }
        | DeviceCommand::SubmitDependentModel { stamp, .. }
        | DeviceCommand::SubmitFactoryAmpModel { stamp, .. }
        | DeviceCommand::SubmitGlobalEdit { stamp, .. }
        | DeviceCommand::SubmitGlobalControlEdit { stamp, .. }
        | DeviceCommand::SaveBufferTo { stamp, .. }
        | DeviceCommand::WriteStoredPreset { stamp, .. }
        | DeviceCommand::ClearStoredPreset { stamp, .. }
        | DeviceCommand::ExecuteDeviceJob { stamp, .. }
        | DeviceCommand::CancelJob { stamp, .. } => Some(*stamp),
        _ => None,
    }
}

#[cfg(test)]
mod asset_export_tests {
    use super::*;
    use std::{
        collections::BTreeMap,
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct TestDirectory(PathBuf);
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn fixture() -> (TestDirectory, AssetInfo, Vec<u8>) {
        let root = TestDirectory(std::env::temp_dir().join(format!(
            "gp200-asset-export-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        fs::create_dir_all(root.0.join("assets/derived")).unwrap();
        let bytes = b"exact retained conversion bytes\0\xff".to_vec();
        let path = root.0.join("assets/derived/immutable-blob");
        fs::write(&path, &bytes).unwrap();
        let info = AssetInfo {
            id: AssetId("retained-result".into()),
            kind: AssetKind::Nam,
            name: "render.wav".into(),
            path,
            bytes: bytes.len() as u64,
            properties: BTreeMap::from([("sha256".into(), JobSha256::of(&bytes).hex())]),
            warnings: vec![],
        };
        (root, info, bytes)
    }
    #[test]
    fn exact_export_replaces_only_the_selected_external_file_and_cleans_staging() {
        let (root, info, bytes) = fixture();
        let destination = root.0.join("new render % $ é.wav");
        fs::write(&destination, b"previous external user file").unwrap();
        export_retained_asset(&info, &destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), bytes);
        assert_eq!(fs::read(&info.path).unwrap(), bytes);
        assert!(fs::read_dir(&root.0).unwrap().all(|item| {
            !item
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".gp200-asset-export")
        }));
    }
    #[test]
    fn changed_content_and_store_destinations_leave_existing_bytes_untouched() {
        let (root, info, bytes) = fixture();
        let destination = root.0.join("existing.clo");
        fs::write(&destination, b"keep this destination").unwrap();
        assert!(
            export_retained_asset(&info, &root.0.join("assets/manifest.json"))
                .unwrap_err()
                .contains("outside")
        );
        fs::write(&info.path, vec![0; bytes.len()]).unwrap();
        assert!(
            export_retained_asset(&info, &destination)
                .unwrap_err()
                .contains("hash changed")
        );
        assert_eq!(fs::read(&destination).unwrap(), b"keep this destination");
        assert!(!root.0.join("assets/manifest.json").exists());
    }
    #[cfg(unix)]
    #[test]
    fn symlinked_destination_directory_cannot_reach_the_retained_store() {
        let (root, info, bytes) = fixture();
        std::os::unix::fs::symlink(root.0.join("assets"), root.0.join("alias")).unwrap();
        assert!(
            export_retained_asset(&info, &root.0.join("alias/derived/immutable-blob")).is_err()
        );
        assert_eq!(fs::read(&info.path).unwrap(), bytes);
    }
    #[test]
    fn file_worker_acknowledges_only_after_exact_durable_export() {
        let (root, info, bytes) = fixture();
        let destination = root.0.join("worker-render.wav");
        let mut services = RuntimeServices::new(
            Box::new(gp200_sim::simulator_service(std::sync::Arc::new(|| {}))),
            std::sync::Arc::new(|| {}),
        );
        services.export_asset(info.clone(), destination.clone());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while services.last_asset_export.is_none() {
            services.pump();
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            services.last_asset_export,
            Some((info.id, destination.clone()))
        );
        assert_eq!(fs::read(destination).unwrap(), bytes);
        assert!(services.errors.is_empty());
    }
}

fn open_browser(url: &str) -> Result<(), String> {
    let mut command = std::process::Command::new("xdg-open");
    command.arg(url);
    run_browser_command(command, std::time::Duration::from_secs(5))
}
fn run_browser_command(
    mut command: std::process::Command,
    timeout: std::time::Duration,
) -> Result<(), String> {
    use std::{process::Stdio, time::Instant};
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(std::format!(
                    "desktop browser launcher exited with {status}"
                ));
            }
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(match result {
                    Err(error) => error.to_string(),
                    _ => crate::localization::text(
                        "desktop browser launcher did not complete within five seconds",
                    )
                    .into(),
                });
            }
        }
    }
}

#[cfg(test)]
mod help_preferences_tests {
    use super::*;
    use std::{
        fs,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::{Duration, Instant},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct NoDevice;
    impl DeviceService for NoDevice {
        fn try_submit(&self, command: DeviceCommand) -> Result<(), SubmitError<DeviceCommand>> {
            panic!("Preferences must not submit a device command: {command:?}");
        }
        fn try_next_event(&self) -> Option<DeviceEvent> {
            None
        }
        fn shutdown(&self) {}
    }
    fn wait_until(service: &mut RuntimeServices, condition: impl Fn(&RuntimeServices) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let (inputs, selections) = service.pump();
            assert!(inputs.is_empty() && selections.is_empty());
            if condition(service) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "File worker did not acknowledge preferences"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn worker_preserves_directory_history_across_appearance_edits_and_reset() {
        let root = std::env::temp_dir().join(format!(
            "gp200-worker-preferences-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("preferences.json");
        let mut preferences = AppPreferences::default();
        preferences
            .dialog_directories
            .insert(DialogDirectoryPurpose::PresetImport, root.clone());
        let mut service = RuntimeServices::with_preferences_path(
            Box::new(NoDevice),
            Arc::new(|| {}),
            preferences.clone(),
            path.clone(),
        );
        let stale = AppPreferences {
            theme: ThemePreference::Dark,
            ..Default::default()
        };
        service
            .commands
            .send(WorkerCommand::Preferences(stale))
            .unwrap();
        service
            .commands
            .send(WorkerCommand::ForgetDirectory(Some(
                DialogDirectoryPurpose::Firmware,
            )))
            .unwrap();
        wait_until(&mut service, |service| service.directory_update.is_some());
        assert_eq!(
            service.take_directory_update().unwrap(),
            preferences.dialog_directories
        );
        let saved: AppPreferences = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.theme, ThemePreference::Dark);
        assert_eq!(saved.dialog_directories, preferences.dialog_directories);
        service
            .commands
            .send(WorkerCommand::ForgetDirectory(None))
            .unwrap();
        preferences.theme = ThemePreference::Light;
        service
            .commands
            .send(WorkerCommand::Preferences(preferences))
            .unwrap();
        // A later acknowledgment acts as a barrier for the queued appearance save.
        service
            .commands
            .send(WorkerCommand::ForgetDirectory(Some(
                DialogDirectoryPurpose::Firmware,
            )))
            .unwrap();
        wait_until(&mut service, |service| {
            service.directory_update.is_some()
                && fs::read(&path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<AppPreferences>(&bytes).ok())
                    .is_some_and(|saved| {
                        saved.theme == ThemePreference::Light && saved.dialog_directories.is_empty()
                    })
        });
        let saved: AppPreferences = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert!(saved.dialog_directories.is_empty());
        assert_eq!(saved.theme, ThemePreference::Light);
        drop(service);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn preference_write_failure_is_visible_and_reset_deletes_no_data() {
        let root = std::env::temp_dir().join(format!(
            "gp200-worker-preferences-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let obstacle = root.join("not-a-file");
        fs::create_dir(&obstacle).unwrap();
        let data = root.join("library-data");
        fs::write(&data, "keep exact data").unwrap();
        let mut preferences = AppPreferences::default();
        preferences
            .dialog_directories
            .insert(DialogDirectoryPurpose::PresetImport, root.clone());
        let mut service = RuntimeServices::with_preferences_path(
            Box::new(NoDevice),
            Arc::new(|| {}),
            preferences,
            obstacle.clone(),
        );
        service.forget_dialog_directory(None);
        wait_until(&mut service, |service| service.directory_update.is_some());
        assert!(service.take_directory_update().unwrap().is_empty());
        assert!(
            service
                .errors
                .back()
                .unwrap()
                .contains("could not be saved")
        );
        assert!(obstacle.is_dir());
        assert_eq!(fs::read_to_string(data).unwrap(), "keep exact data");
        drop(service);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn browser_launch_errors_and_timeout_do_not_start_other_services() {
        let timeout = Duration::from_millis(30);
        assert!(run_browser_command(std::process::Command::new("/bin/true"), timeout).is_ok());
        assert!(run_browser_command(std::process::Command::new("/bin/false"), timeout).is_err());
        assert!(
            run_browser_command(
                std::process::Command::new("/nonexistent-gp200-browser"),
                timeout
            )
            .is_err()
        );
        let mut sleeping = std::process::Command::new("/bin/sleep");
        sleeping.arg("1");
        let before = Instant::now();
        assert!(run_browser_command(sleeping, timeout).is_err());
        assert!(before.elapsed() < Duration::from_millis(300));
    }
    #[test]
    fn dialogs_have_distinct_import_export_ir_nam_and_calibration_folders() {
        assert_ne!(
            FilePurpose::ImportIr.directory_purpose(),
            FilePurpose::ImportNam.directory_purpose()
        );
        assert_ne!(
            FilePurpose::ImportNam.directory_purpose(),
            FilePurpose::ImportSnapTone.directory_purpose()
        );
        assert_ne!(
            FilePurpose::ImportPresets.directory_purpose(),
            FilePurpose::ExportPresets.directory_purpose()
        );
        assert_ne!(
            FilePurpose::NamInput.directory_purpose(),
            FilePurpose::NamCalibrationInput.directory_purpose()
        );
        assert_eq!(
            FilePurpose::ExportNative.directory_purpose(),
            FilePurpose::ExportPresets.directory_purpose()
        );
    }
}

#[cfg(test)]
mod file_worker_settlement_tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::AtomicU64,
        time::{Duration, Instant},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct NoDevice;
    impl DeviceService for NoDevice {
        fn try_submit(&self, command: DeviceCommand) -> Result<(), SubmitError<DeviceCommand>> {
            panic!("File settlement must not offer device work: {command:?}");
        }
        fn try_next_event(&self) -> Option<DeviceEvent> {
            None
        }
        fn shutdown(&self) {}
    }
    struct Directory(PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn runtime() -> (Directory, RuntimeServices) {
        let directory = Directory(std::env::temp_dir().join(format!(
            "gp200-file-settlement-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        fs::create_dir(&directory.0).unwrap();
        let runtime = RuntimeServices::with_preferences_path(
            Box::new(NoDevice),
            Arc::new(|| {}),
            AppPreferences::default(),
            directory.0.join("preferences.json"),
        );
        (directory, runtime)
    }
    fn wait(condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "Tracked worker did not reach the requested boundary"
            );
            thread::yield_now();
        }
    }
    fn work() -> Arc<FileWorkerWork> {
        Arc::new(FileWorkerWork {
            outstanding: AtomicUsize::new(0),
            panicked: AtomicBool::new(false),
            wake: Arc::new(|| {}),
        })
    }
    #[test]
    fn accepted_commands_count_before_queue_and_rejected_offers_roll_back() {
        let work = work();
        let (sender, receiver) = mpsc::sync_channel(1);
        let commands = FileWorkerCommands {
            sender,
            work: work.clone(),
        };
        commands
            .try_send(WorkerCommand::Dialog(FilePurpose::ImportPresets))
            .unwrap();
        assert_eq!(work.outstanding.load(Ordering::SeqCst), 1);
        assert!(!work.settled());
        assert!(matches!(
            commands.try_send(WorkerCommand::Preferences(AppPreferences::default())),
            Err(mpsc::TrySendError::Full(WorkerCommand::Preferences(_)))
        ));
        assert_eq!(
            work.outstanding.load(Ordering::SeqCst),
            1,
            "Rejected offer leaked accepted work"
        );
        let running = receiver.recv().unwrap();
        assert!(
            !work.settled(),
            "Taking a queued command must not acknowledge running work"
        );
        drop(running);
        assert!(work.settled());
        drop(receiver);
        assert!(matches!(
            commands.try_send(WorkerCommand::ForgetDirectory(None)),
            Err(mpsc::TrySendError::Disconnected(
                WorkerCommand::ForgetDirectory(None)
            ))
        ));
        assert!(
            work.settled(),
            "Disconnected offer must roll back its token"
        );
        assert!(commands.send(WorkerCommand::Shutdown).is_err());
        assert!(
            work.settled(),
            "Rejected blocking offer must also roll back"
        );
    }
    #[test]
    fn reply_tokens_overlap_commands_and_failed_or_dropped_publications_release_only_their_work() {
        let work = work();
        let (sender, receiver) = mpsc::sync_channel(1);
        let events = FileWorkerEvents {
            sender,
            work: work.clone(),
        };
        let command = work.acquire();
        events
            .send(WorkerEvent::Error("original failed operation".into()))
            .unwrap();
        assert_eq!(work.outstanding.load(Ordering::SeqCst), 2);
        drop(command);
        assert!(
            !work.settled(),
            "Command completion cannot discard the unconsumed reply"
        );
        let reply = receiver.recv().unwrap();
        assert!(
            !work.settled(),
            "Receiving the envelope does not acknowledge UI handling"
        );
        drop(reply);
        assert!(work.settled());
        drop(receiver);
        assert!(
            events
                .send(WorkerEvent::Directories(Default::default()))
                .is_err()
        );
        assert!(
            work.settled(),
            "Failed reply publication leaked its own token"
        );
        let (sender, receiver) = mpsc::sync_channel(1);
        let events = FileWorkerEvents {
            sender,
            work: work.clone(),
        };
        events
            .send(WorkerEvent::LinkOpened("accepted desktop request".into()))
            .unwrap();
        assert!(!work.settled());
        drop(receiver);
        assert!(
            work.settled(),
            "Dropping a queued result must drop its acknowledgement too"
        );
    }
    #[test]
    fn actual_worker_keeps_completed_dialog_reply_counted_through_selection_handling() {
        let (directory, mut runtime) = runtime();
        let (running, release) = runtime.test_queue_file_work_barrier();
        running.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(!runtime.file_worker_settled());
        let choice = directory.0.join("original-import.prst");
        runtime.test_queue_file_selection(FilePurpose::ImportPresets, Some(choice.clone()));
        let preferences = AppPreferences {
            theme: ThemePreference::Light,
            ..Default::default()
        };
        runtime.enqueue(vec![Effect::PersistSettings(preferences)]);
        assert!(runtime.pump().1.is_empty());
        let (finished_prior, finish_last) = runtime.test_queue_file_work_barrier();
        release.send(()).unwrap();
        finished_prior.recv_timeout(Duration::from_secs(3)).unwrap();
        finish_last.send(()).unwrap();
        wait(|| runtime.commands.work.outstanding.load(Ordering::SeqCst) == 1);
        assert!(
            !runtime.file_worker_settled(),
            "Completed but unconsumed dialog reply was lost"
        );
        let (inputs, selections) = runtime.pump();
        assert!(inputs.is_empty());
        assert_eq!(selections.len(), 1);
        assert!(
            !runtime.file_worker_settled(),
            "Returning a selection from pump is not handling it"
        );
        let unrelated = FileSelection {
            purpose: FilePurpose::ExportPresets,
            path: Some(choice),
        };
        assert!(
            runtime
                .take_file_selection_acknowledgement(&unrelated)
                .is_none()
        );
        assert!(!runtime.file_worker_settled());
        let acknowledgement = runtime
            .take_file_selection_acknowledgement(&selections[0])
            .unwrap();
        assert!(
            !runtime.file_worker_settled(),
            "Acknowledgement must survive actual handler execution"
        );
        drop(acknowledgement);
        assert!(runtime.file_worker_settled());
        let saved: AppPreferences =
            serde_json::from_slice(&fs::read(directory.0.join("preferences.json")).unwrap())
                .unwrap();
        assert_eq!(saved.theme, ThemePreference::Light);
    }
    #[test]
    fn full_worker_queue_retains_exact_preferences_retry_and_acknowledges_durable_finish() {
        let (directory, mut runtime) = runtime();
        let (running, release) = runtime.test_queue_file_work_barrier();
        running.recv_timeout(Duration::from_secs(3)).unwrap();
        for _ in 0..32 {
            runtime
                .commands
                .try_send(WorkerCommand::Preferences(AppPreferences::default()))
                .unwrap();
        }
        let changed = AppPreferences {
            theme: ThemePreference::Light,
            scale: 1.25,
            ..Default::default()
        };
        runtime.enqueue(vec![Effect::PersistSettings(changed.clone())]);
        assert!(!runtime.file_worker_settled());
        assert!(runtime.pump().1.is_empty());
        assert_eq!(runtime.commands.work.outstanding.load(Ordering::SeqCst), 33);
        assert!(
            matches!(runtime.pending.front(), Some(Effect::PersistSettings(actual)) if actual == &changed)
        );
        assert!(!runtime.file_worker_settled());
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !runtime.file_worker_settled() {
            let (inputs, selections) = runtime.pump();
            assert!(inputs.is_empty() && selections.is_empty());
            assert!(
                Instant::now() < deadline,
                "Original preference retry did not finish"
            );
            thread::yield_now();
        }
        let saved: AppPreferences =
            serde_json::from_slice(&fs::read(directory.0.join("preferences.json")).unwrap())
                .unwrap();
        assert_eq!(saved.theme, changed.theme);
        assert_eq!(saved.scale, changed.scale);
        assert!(runtime.pending.is_empty());
    }
    #[test]
    fn staged_directory_result_remains_unsettled_until_applied() {
        let (_directory, mut runtime) = runtime();
        runtime.forget_dialog_directory(None);
        let (processed, release) = runtime.test_queue_file_work_barrier();
        processed.recv_timeout(Duration::from_secs(3)).unwrap();
        release.send(()).unwrap();
        wait(|| runtime.commands.work.outstanding.load(Ordering::SeqCst) == 1);
        assert!(!runtime.file_worker_settled());
        assert!(runtime.pump().1.is_empty());
        assert_eq!(runtime.commands.work.outstanding.load(Ordering::SeqCst), 0);
        assert!(
            !runtime.file_worker_settled(),
            "Pumped folder reply still needs app preference dispatch"
        );
        assert!(runtime.take_directory_update().unwrap().is_empty());
        assert!(runtime.file_worker_settled());
    }
    #[test]
    fn worker_panic_releases_command_tokens_but_cannot_claim_settlement() {
        let (_directory, runtime) = runtime();
        let (running, release) = runtime.test_queue_file_work_barrier();
        running.recv_timeout(Duration::from_secs(3)).unwrap();
        runtime.commands.try_send(WorkerCommand::TestPanic).unwrap();
        runtime
            .commands
            .try_send(WorkerCommand::Preferences(AppPreferences::default()))
            .unwrap();
        release.send(()).unwrap();
        wait(|| {
            runtime.commands.work.panicked.load(Ordering::SeqCst)
                && runtime.commands.work.outstanding.load(Ordering::SeqCst) == 0
        });
        assert!(
            !runtime.file_worker_settled(),
            "A partial panicking command has no acknowledged result"
        );
        assert!(
            runtime
                .commands
                .try_send(WorkerCommand::Preferences(AppPreferences::default()))
                .is_err()
        );
        assert_eq!(runtime.commands.work.outstanding.load(Ordering::SeqCst), 0);
        assert!(!runtime.file_worker_settled());
    }
}
