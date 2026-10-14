//! Native firmware file review. All actions here are local worker operations.
pub use crate::firmware_workflow::FirmwareSimulation;
use crate::{
    StudioApp,
    firmware_workflow::{
        Completed, FirmwareWorker, InspectedFirmware, RehearsalReport, ReportSnapshot, Work,
    },
    runtime::FilePurpose,
    theme::Palette,
};
use eframe::egui::{self, RichText};
use gp200_domain::*;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

#[derive(Clone)]
pub struct FirmwareWorkspaceLaunch {
    pub source: PathBuf,
    pub identity_model: DeviceModel,
    pub chunk_bytes: u16,
    pub simulation: Option<FirmwareSimulation>,
}
pub(crate) struct FirmwareUiState {
    pub updater: super::firmware_updater::FirmwareUpdaterUi,
    worker: FirmwareWorker,
    pub path: String,
    pub image: Option<InspectedFirmware>,
    pub rehearsal: Option<RehearsalReport>,
    pub model: DeviceModel,
    pub chunk_bytes: u16,
    pub simulation: FirmwareSimulation,
    pub message: String,
    show_message: bool,
    sequence: u64,
    pending: Option<(u64, Arc<AtomicBool>)>,
    pub export_pending: Option<ReportSnapshot>,
    launch_simulation: Option<FirmwareSimulation>,
    pub settled: Option<Instant>,
    #[cfg(test)]
    run_rect: Option<egui::Rect>,
}
impl Default for FirmwareUiState {
    fn default() -> Self {
        Self {
            updater: Default::default(),
            worker: Default::default(),
            path: String::new(),
            image: None,
            rehearsal: None,
            model: DeviceModel::Gp200,
            chunk_bytes: 128,
            simulation: FirmwareSimulation::Preview,
            message: String::new(),
            show_message: true,
            sequence: 0,
            pending: None,
            export_pending: None,
            launch_simulation: None,
            settled: None,
            #[cfg(test)]
            run_rect: None,
        }
    }
}
impl Drop for FirmwareUiState {
    fn drop(&mut self) {
        if let Some((_, cancel)) = &self.pending {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}
impl FirmwareUiState {
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }
    fn submit(&mut self, work: Work, message: &str) {
        self.show_message = true;
        self.cancel();
        self.sequence += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        match self.worker.submit(self.sequence, cancel.clone(), work) {
            Ok(()) => {
                self.pending = Some((self.sequence, cancel));
                self.message = message.into();
                self.settled = None;
            }
            Err(error) => {
                self.message = error;
                self.settled = Some(Instant::now());
            }
        }
    }
    pub fn cancel(&mut self) {
        if let Some((_, cancel)) = self.pending.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.launch_simulation = None;
        self.settled = Some(Instant::now());
    }
    fn request_cancel(&mut self) {
        self.show_message = true;
        if let Some((_, cancel)) = &self.pending {
            cancel.store(true, Ordering::Relaxed);
            self.message = crate::localization::text(
                "Cancellation requested; waiting for the local worker to finish.",
            )
            .into();
        }
        self.launch_simulation = None;
    }
    pub fn inspect(&mut self, path: PathBuf, session: Option<DeviceJobSession>) {
        self.path = path.display().to_string();
        self.image = None;
        self.rehearsal = None;
        self.export_pending = None;
        self.submit(
            Work::Inspect {
                path,
                session: session.map(Box::new),
            },
            crate::localization::text("Inspecting the selected firmware file…"),
        );
    }
    fn run_rehearsal(&mut self) {
        let Some(image) = self.image.clone() else {
            return;
        };
        self.rehearsal = None;
        self.submit(
            Work::Rehearse {
                image: Box::new(image),
                model: self.model,
                chunk: self.chunk_bytes,
                simulation: self.simulation,
            },
            crate::localization::text("Running the selected offline simulation…"),
        );
    }
    pub fn export_to(&mut self, path: PathBuf) {
        if let Some(report) = self.export_pending.take() {
            self.submit(
                Work::Export {
                    report: Box::new(report),
                    path,
                },
                crate::localization::text("Saving the reviewed report…"),
            );
        }
    }
    pub fn poll(&mut self) {
        let replies: Vec<_> = self.worker.poll().collect();
        for reply in replies {
            if self.pending.as_ref().map(|v| v.0) != Some(reply.id) {
                continue;
            }
            self.pending = None;
            self.settled = Some(Instant::now());
            match reply.result {
                Ok(Completed::Inspected(image)) => {
                    self.show_message = false;
                    self.image = Some(*image);
                    self.message = crate::localization::text(
                        "Image reviewed from retained bytes. The pedal has not been changed.",
                    )
                    .into();
                    if let Some(simulation) = self.launch_simulation.take() {
                        self.simulation = simulation;
                        self.run_rehearsal();
                    }
                }
                Ok(Completed::Rehearsed(report)) => {
                    self.show_message = false;
                    self.message = match report.phase {
                        FirmwareRehearsalPhase::AwaitingFinalizeEvidence => crate::localization::text("All simulated block and section acknowledgements completed. Finalization and recovery remain unverified."),
                        FirmwareRehearsalPhase::Indeterminate => crate::localization::text("The simulated acknowledgement is missing. State is indeterminate; there is no automatic retry."),
                        _ => crate::localization::text("First block preview prepared. No acknowledgement was assumed."),
                    }.into();
                    self.rehearsal = Some(*report);
                }
                Ok(Completed::Exported(path)) => {
                    self.show_message = true;
                    self.message = std::format!("Report saved: {}", path.display())
                }
                Err(error) => {
                    self.show_message = true;
                    self.message = error;
                }
            }
        }
    }
    fn report(&self) -> Option<ReportSnapshot> {
        self.image
            .as_ref()
            .map(|image| ReportSnapshot::new(image, self.rehearsal.clone()))
    }
}
impl StudioApp {
    /// Diagnostic projection of the immutable local review. This provides no
    /// transport or update authority, including when the simulation completed.
    pub fn firmware_review_report(&self) -> Option<(serde_json::Value, String)> {
        if self.firmware_ui.busy() {
            return None;
        }
        self.firmware_ui
            .report()
            .and_then(|report| serde_json::to_value(report).ok())
            .map(|report| (report, self.firmware_ui.message.clone()))
    }
    pub fn open_firmware_workspace(&mut self, launch: FirmwareWorkspaceLaunch) {
        if self.firmware_close_blocked() {
            self.firmware_ui.updater.message =
                "Return to editing before replacing the bound image or offline rehearsal settings."
                    .into();
            return;
        }
        self.send(UiIntent::Navigate(ViewId::Assets));
        self.selected_asset_kind = AssetKind::Firmware;
        self.firmware_ui.model = launch.identity_model;
        self.firmware_ui.chunk_bytes = launch.chunk_bytes;
        self.inspect_firmware(launch.source);
        self.firmware_ui.launch_simulation = launch.simulation;
    }
    pub(crate) fn inspect_firmware(&mut self, path: PathBuf) {
        if self.firmware_close_blocked() {
            self.firmware_ui.updater.message =
                "The selected image is frozen while firmware preparation owns the editor. Return to editing before selecting another file.".into();
            return;
        }
        // Simulator/capture identity must not be presented as the installed pedal version.
        let session = self
            .controller
            .state()
            .device_job_session
            .clone()
            .filter(|session| session.identity().mode == BackendMode::Hardware);
        self.firmware_ui.inspect(path, session);
    }
    pub(super) fn firmware_workspace(&mut self, ui: &mut egui::Ui) {
        self.invalidate_changed_firmware_report_session();
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        self.refresh_linux_completion_presentation(ui.ctx());
        if self.controller.state().preferences.compact {
            ui.spacing_mut().item_spacing.y = 5.0;
        }
        self.firmware_navigation(ui);
        let page = self.firmware_ui.updater.page;
        let detail = egui::ScrollArea::vertical()
            .id_salt(("firmware-detail-scroll", page))
            .auto_shrink([false, false])
            .show(ui, |ui| match page {
                super::firmware_updater::FirmwareSubview::FileReview => {
                    self.firmware_file_review(ui)
                }
                super::firmware_updater::FirmwareSubview::Prepare => {
                    self.firmware_prepare_screen(ui)
                }
                super::firmware_updater::FirmwareSubview::Update => self.firmware_update_screen(ui),
                super::firmware_updater::FirmwareSubview::History => {
                    self.firmware_history_screen(ui)
                }
            });
        #[cfg(test)]
        {
            self.firmware_ui.updater.detail_scroll =
                Some((page, detail.id, detail.state.offset, detail.inner_rect));
        }
        #[cfg(not(test))]
        let _ = detail;
    }
    fn firmware_file_review(&mut self, ui: &mut egui::Ui) {
        let palette = Palette::for_context(ui.ctx());
        if let Some(preview) = self.firmware_ui.updater.preview.clone() {
            self.firmware_preview_file_review(ui, &preview);
            return;
        }
        ui.heading(crate::localization::text("Firmware file review"));
        ui.label(crate::localization::text("Inspect a saved image and rehearse its packets locally before any update is considered."));
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    !self.firmware_ui.busy() && !self.firmware_close_blocked(),
                    egui::Button::new(crate::localization::text("Select firmware…")),
                )
                .clicked()
            {
                self.services.dialog(FilePurpose::Firmware);
            }
            let response = ui.add_enabled(
                !self.firmware_ui.busy() && !self.firmware_close_blocked(),
                egui::TextEdit::singleline(&mut self.firmware_ui.path)
                    .hint_text(crate::localization::text("Firmware .bin path"))
                    .desired_width(ui.available_width().min(330.0)),
            );
            if response.changed() {
                self.firmware_ui.cancel();
                self.firmware_ui.image = None;
                self.firmware_ui.rehearsal = None;
                self.firmware_ui.export_pending = None;
                self.firmware_ui.message = crate::localization::text(
                    "Source changed; inspect the selected file to create a new review.",
                )
                .into();
                self.firmware_ui.show_message = true;
            }
            if ui
                .add_enabled(
                    !self.firmware_ui.path.is_empty()
                        && !self.firmware_ui.busy()
                        && !self.firmware_close_blocked(),
                    egui::Button::new(crate::localization::text("Inspect image")),
                )
                .clicked()
            {
                self.inspect_firmware(PathBuf::from(&self.firmware_ui.path));
            }
        });
        ui.horizontal_wrapped(|ui| {
            let installed = self
                .controller
                .state()
                .identity
                .as_ref()
                .filter(|v| v.mode == BackendMode::Hardware)
                .and_then(|v| v.firmware.as_ref())
                .map(|v| v.version.as_str());
            ui.label(std::format!(
                "Normal identity version claim: {}",
                installed.unwrap_or(crate::localization::text(
                    "unavailable — no verified application version query"
                ))
            ));
            if self.firmware_ui.busy() {
                ui.spinner();
                if ui
                    .button(crate::localization::text("Cancel local operation"))
                    .clicked()
                {
                    self.firmware_ui.request_cancel();
                }
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(50));
            }
        });
        if let Some(report) = &self.firmware_ui.rehearsal {
            ui.colored_label(
                palette.warning,
                std::format!(
                    "Simulation: {}. Physical update remains blocked.",
                    rehearsal_phase_label(&report.phase)
                ),
            )
            .on_hover_text(crate::localization::message(&self.firmware_ui.message));
        }
        if self.firmware_ui.show_message && !self.firmware_ui.message.is_empty() {
            ui.label(crate::localization::message(&self.firmware_ui.message));
        }
        let Some(image) = self.firmware_ui.image.clone() else {
            super::section(ui, crate::localization::text("Select an image"));
            ui.label(crate::localization::text("The review reads the image's product, version, checksum and four sections. It keeps the exact inspected bytes for the rehearsal and report."));
            ui.label(crate::localization::text("A complete restorable backup and verified update/recovery protocol are required before a physical update can be enabled."));
            return;
        };
        if ui.available_width() >= 800.0 {
            ui.columns(2, |columns| {
                image_summary(
                    &mut columns[0],
                    &image,
                    palette,
                    self.firmware_ui.chunk_bytes,
                );
                self.firmware_rehearsal_controls(&mut columns[1], &image);
            });
        } else {
            image_summary(ui, &image, palette, self.firmware_ui.chunk_bytes);
            self.firmware_rehearsal_controls(ui, &image);
        }
    }
    fn firmware_rehearsal_controls(&mut self, ui: &mut egui::Ui, image: &InspectedFirmware) {
        let palette = Palette::for_context(ui.ctx());
        let summary = image.review.summary();
        super::section(ui, crate::localization::text("Offline packet rehearsal"));
        ui.label(crate::localization::text("The identity below is synthetic. Chunk size is a test setting, not a value negotiated with the pedal."));
        ui.add_enabled_ui(
            !self.firmware_ui.busy() && !self.firmware_close_blocked(),
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    egui::ComboBox::from_id_salt("firmware-fixture-model")
                        .selected_text(self.firmware_ui.model.name())
                        .show_ui(ui, |ui| {
                            for model in [
                                DeviceModel::Gp200,
                                DeviceModel::Gp200Lt,
                                DeviceModel::Gp200Jr,
                            ] {
                                ui.selectable_value(
                                    &mut self.firmware_ui.model,
                                    model,
                                    std::format!("Synthetic {} identity", model.name()),
                                );
                            }
                        });
                    ui.label(crate::localization::text("Chunk bytes"));
                    ui.add(egui::DragValue::new(&mut self.firmware_ui.chunk_bytes).range(1..=255));
                });
                egui::ComboBox::from_id_salt("firmware-simulation")
                    .selected_text(self.firmware_ui.simulation.label())
                    .width(ui.available_width().min(370.0))
                    .show_ui(ui, |ui| {
                        for simulation in [
                            FirmwareSimulation::Preview,
                            FirmwareSimulation::AllAcknowledged,
                            FirmwareSimulation::MissingBlockAcknowledgement,
                            FirmwareSimulation::MissingSectionAcknowledgement,
                        ] {
                            ui.selectable_value(
                                &mut self.firmware_ui.simulation,
                                simulation,
                                simulation.label(),
                            );
                        }
                    });
                ui.horizontal_wrapped(|ui| {
                    let run = ui.button(crate::localization::text("Run offline rehearsal"));
                    #[cfg(test)]
                    {
                        self.firmware_ui.run_rect = Some(run.rect);
                    }
                    if run.clicked() {
                        self.firmware_ui.run_rehearsal();
                    }
                    if ui
                        .button(crate::localization::text("Export review report…"))
                        .clicked()
                    {
                        self.firmware_ui.export_pending = self.firmware_ui.report();
                        if !self.services.try_dialog(FilePurpose::ExportFirmwareReport) {
                            self.firmware_ui.export_pending = None;
                        }
                    }
                });
            },
        );
        if let Some(report) = &self.firmware_ui.rehearsal {
            ui.small(std::format!(
                "Last run: {} bytes per chunk; {}",
                report.negotiated_chunk_bytes,
                report.simulation.label()
            ));
            ui.small(&report.identity_fixture);
            ui.label(
                RichText::new(std::format!(
                    "Simulation state: {}",
                    rehearsal_phase_label(&report.phase)
                ))
                .strong(),
            );
            ui.label(std::format!(
                "{} blocks planned; {} block and {} section acknowledgements simulated",
                report.total_blocks,
                report.simulated_block_acknowledgements,
                report.simulated_section_acknowledgements
            ));
            egui::CollapsingHeader::new(crate::localization::text("Representative block hashes")).show(ui, |ui| {
                for preview in &report.representative_chunks {
                    ui.label(std::format!("Section {}, block {}, index {}, {} source + {} padding → {} wire bytes",
                        preview.section + 1,
                        preview.block_in_section,
                        preview.index_mod128,
                        preview.content_bytes,
                        preview.padded_bytes,
                        preview.wire_bytes
                    ));
                    compact_path(ui, crate::localization::text("Wire SHA-256"), &preview.wire_sha256.hex());
                }
            });
        }
        egui::CollapsingHeader::new(crate::localization::text("Physical update remains blocked")).default_open(false).show(ui, |ui| {
            for blocker in &summary.blockers { ui.label(crate::localization::text(blocker.explanation())); }
            ui.label(crate::localization::text("Cancelling locally before any send is safe. Resume, restart, finalization and post-update restore have no verified physical recovery profile."));
        });
        ui.colored_label(
            palette.warning,
            crate::localization::text(
                "Offline review only. No bootloader entry, transfer or flash is enabled.",
            ),
        );
    }
}
fn rehearsal_phase_label(phase: &FirmwareRehearsalPhase) -> &'static str {
    crate::localization::text(match phase {
        FirmwareRehearsalPhase::Prepared => "Prepared",
        FirmwareRehearsalPhase::Transferring => "Preparing transfer packets",
        FirmwareRehearsalPhase::AwaitingBlockAcknowledgement => "Waiting for block acknowledgement",
        FirmwareRehearsalPhase::AwaitingSectionAcknowledgement => {
            "Waiting for section acknowledgement"
        }
        FirmwareRehearsalPhase::AwaitingFinalizeEvidence => "Waiting for finalization evidence",
        FirmwareRehearsalPhase::Cancelled => "Cancelled",
        FirmwareRehearsalPhase::Indeterminate => "Outcome uncertain",
    })
}
fn image_summary(ui: &mut egui::Ui, image: &InspectedFirmware, palette: Palette, chunk: u16) {
    let summary = image.review.summary();
    super::section(ui, crate::localization::text("Inspected image"));
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(summary.model.name()).strong());
        ui.label(std::format!(
            "Image version {}",
            summary
                .image_version
                .as_deref()
                .unwrap_or(crate::localization::text("unrecognized"))
        ));
        ui.label(std::format!("{} bytes", summary.file_bytes));
    });
    ui.colored_label(
        palette.accent,
        crate::localization::text("Checksum passed; authenticity unverified"),
    );
    compact_path(
        ui,
        crate::localization::text("SHA-256"),
        &summary.source_sha256.hex(),
    );
    let provenance = &summary.image_provenance;
    ui.label(format!(
        "Image tag {:?} at file byte {}; recognized {}, decimal digits {}",
        provenance.version_tag_raw(),
        provenance.version_tag_file_offset(),
        provenance.tag_recognized(),
        provenance.tag_digits_valid()
    ));
    ui.label(format!(
        "Header words {:?}; native normalized words {:?}",
        provenance.raw_words(),
        provenance.native_normalized_words()
    ));
    ui.small("These are image-derived header facts. Official provenance and a verified target hardware/profile are not established by a filename or checksum.");
    egui::CollapsingHeader::new("Native and integer packet counts").show(ui, |ui| {
        ui.small(format!(
            "Local test chunk: {chunk} bytes. This size was not negotiated with the pedal."
        ));
        for part in &summary.sections {
            match firmware_block_count_comparison(part.bytes as u32, chunk) {
                Ok(count) => {
                    ui.label(format!(
                        "Section {}: integer {} blocks; native float32 {} blocks; {}",
                        part.index + 1,
                        count.safe_integer_blocks,
                        count.native_float32_blocks,
                        if count.discrepancy() {
                            "count mismatch blocks update planning"
                        } else {
                            "counts agree locally"
                        }
                    ));
                }
                Err(error) => {
                    ui.colored_label(palette.warning, error.message);
                }
            }
        }
    });
    egui::CollapsingHeader::new(crate::localization::text("Source and four image sections")).show(ui, |ui| {
        ui.label(crate::localization::text("This additive checksum checks file consistency; it is not a signature or proof of authenticity."));
        compact_path(ui, crate::localization::text("Retained source"), &image.path.display().to_string());
        for part in &summary.sections {
            ui.label(std::format!("Section {}: offset {}, {} bytes", part.index + 1, part.file_offset, part.bytes));
            compact_path(ui, crate::localization::text("SHA-256"), &part.sha256.hex());
        }
    });
}
fn compact_path(ui: &mut egui::Ui, label: &str, text: &str) {
    ui.vertical(|ui| {
        ui.small(label);
        ui.add(egui::Label::new(RichText::new(text).monospace().small()).wrap());
        if ui.small_button(format!("Copy {label}")).clicked() {
            ui.ctx().copy_text(text.to_owned());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    const IMAGE: &[u8] =
        include_bytes!("../../../../tests/fixtures/device-assets/synthetic-firmware.bin");
    fn directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "gp200-firmware-ui-state-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }
    fn finish(state: &mut FirmwareUiState) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while state.busy() {
            state.poll();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    #[test]
    fn stale_source_result_cannot_replace_a_new_selection() {
        let dir = directory();
        let first = dir.join("first.bin");
        let second = dir.join("second.bin");
        fs::write(&first, IMAGE).unwrap();
        fs::write(&second, IMAGE).unwrap();
        let mut state = FirmwareUiState::default();
        state.inspect(first, None);
        state.inspect(second.clone(), None);
        finish(&mut state);
        assert_eq!(
            state.image.as_ref().unwrap().path,
            second.canonicalize().unwrap()
        );
        assert!(state.rehearsal.is_none());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn cancel_waits_for_the_worker_and_preserves_the_review() {
        let dir = directory();
        let source = dir.join("source.bin");
        fs::write(&source, IMAGE).unwrap();
        let mut state = FirmwareUiState::default();
        state.inspect(source, None);
        finish(&mut state);
        state.simulation = FirmwareSimulation::AllAcknowledged;
        state.run_rehearsal();
        state.request_cancel();
        assert!(state.busy());
        finish(&mut state);
        assert!(state.image.is_some());
        assert!(!state.busy());
        assert!(
            state.message.contains("cancelled")
                || state.rehearsal.as_ref().is_some_and(
                    |report| report.phase == FirmwareRehearsalPhase::AwaitingFinalizeEvidence
                )
        );
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn export_dialog_snapshot_survives_source_file_changes_and_cancellation_drops_it() {
        let dir = directory();
        let source = dir.join("source.bin");
        fs::write(&source, IMAGE).unwrap();
        let mut state = FirmwareUiState::default();
        state.inspect(source.clone(), None);
        finish(&mut state);
        state.export_pending = state.report();
        fs::write(source, b"replaced source").unwrap();
        let output = dir.join("review.json");
        state.export_to(output.clone());
        finish(&mut state);
        let json: serde_json::Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
        assert_eq!(json["source_sha256_hex"], JobSha256::of(IMAGE).hex());
        assert!(state.export_pending.is_none());
        state.export_pending = state.report();
        state.export_pending = None;
        let cancelled = dir.join("cancelled.json");
        state.export_to(cancelled.clone());
        assert!(!state.busy());
        assert!(!cancelled.exists());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn actual_rehearsal_button_clicks_never_submit_device_commands() {
        use std::sync::atomic::AtomicUsize;
        struct CountDevice(Arc<AtomicUsize>);
        impl DeviceService for CountDevice {
            fn try_submit(&self, command: DeviceCommand) -> Result<(), SubmitError<DeviceCommand>> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(SubmitError::Disconnected(command))
            }
            fn try_next_event(&self) -> Option<DeviceEvent> {
                None
            }
            fn shutdown(&self) {}
        }
        fn render(app: &mut StudioApp, ctx: &egui::Context, events: Vec<egui::Event>) {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(960.0, 800.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| app.firmware_workspace(ui),
            );
            assert!(!output.shapes.is_empty());
            output.textures_delta.clear();
        }
        let dir = directory();
        let source = dir.join("source.bin");
        fs::write(&source, IMAGE).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let mut app = StudioApp::with_services(
            gp200_app::AppController::with_demo(
                gp200_sim::captured_demo_snapshot(),
                AppPreferences::default(),
            )
            .unwrap(),
            crate::runtime::RuntimeServices::new(
                Box::new(CountDevice(count.clone())),
                gp200_domain::no_wake(),
            ),
        );
        let before = app.controller.active_document().unwrap().draft.clone();
        app.inspect_firmware(source);
        finish(&mut app.firmware_ui);
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        for (simulation, phase) in [
            (
                FirmwareSimulation::Preview,
                FirmwareRehearsalPhase::AwaitingBlockAcknowledgement,
            ),
            (
                FirmwareSimulation::AllAcknowledged,
                FirmwareRehearsalPhase::AwaitingFinalizeEvidence,
            ),
            (
                FirmwareSimulation::MissingBlockAcknowledgement,
                FirmwareRehearsalPhase::Indeterminate,
            ),
            (
                FirmwareSimulation::MissingSectionAcknowledgement,
                FirmwareRehearsalPhase::Indeterminate,
            ),
        ] {
            app.firmware_ui.simulation = simulation;
            render(&mut app, &ctx, vec![]);
            render(&mut app, &ctx, vec![]);
            let point = app.firmware_ui.run_rect.unwrap().center();
            for pressed in [true, false] {
                render(
                    &mut app,
                    &ctx,
                    vec![
                        egui::Event::PointerMoved(point),
                        egui::Event::PointerButton {
                            pos: point,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
            finish(&mut app.firmware_ui);
            assert_eq!(app.firmware_ui.rehearsal.as_ref().unwrap().phase, phase);
            assert_eq!(count.load(Ordering::SeqCst), 0);
            assert_eq!(app.controller.active_document().unwrap().draft, before);
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
