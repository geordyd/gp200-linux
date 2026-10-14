use super::section;
use crate::{StudioApp, SwitchWait, runtime::FilePurpose, theme::Palette};
use eframe::egui::{self, RichText};
use gp200_domain::*;

impl StudioApp {
    pub(super) fn bank_controls(&mut self, ui: &mut egui::Ui) {
        let available = self
            .controller
            .operation_availability(OperationKind::ReadStoredPreset)
            == OperationAvailability::Available;
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            if self.bank_scan.running {
                if ui
                    .button(crate::localization::text("Pause bank scan"))
                    .clicked()
                {
                    self.bank_scan.cancel();
                }
            } else {
                if ui
                    .add_enabled(
                        available && self.bank_scan.in_flight.is_none(),
                        egui::Button::new(crate::localization::text("Read all 256 slots")),
                    )
                    .clicked()
                {
                    self.bank_scan.start(self.controller.state().epoch, false);
                }
                if !self.bank_scan.completed.is_empty()
                    && self.bank_scan.completed.len() < 256
                    && ui
                        .add_enabled(
                            available
                                && self.bank_scan.epoch == Some(self.controller.state().epoch),
                            egui::Button::new(crate::localization::text("Resume scan")),
                        )
                        .clicked()
                {
                    self.bank_scan.start(self.controller.state().epoch, true);
                }
            }
        });
        if self.bank_scan.epoch.is_some() {
            ui.add(
                egui::ProgressBar::new(self.bank_scan.completed.len() as f32 / 256.0).text(
                    std::format!("{} / 256 stored presets", self.bank_scan.completed.len()),
                ),
            );
            ui.small(crate::localization::message(&self.bank_scan.message));
        }
        if let Some(cached) = self
            .controller
            .state()
            .bank
            .slots
            .get(&SlotId::new(self.slot_preview).expect("bounded slot"))
            .cloned()
        {
            ui.label(RichText::new(cached.snapshot.view.display_name()).strong());
            ui.small(crate::localization::text(
                "Stored preview. Reading it does not change your sound.",
            ));
            if ui
                .button(crate::localization::text("Open preview as offline draft"))
                .clicked()
            {
                self.request_switch(crate::PendingSwitch::Snapshot(cached.snapshot));
            }
            if ui
                .add_enabled(
                    self.controller
                        .operation_availability(OperationKind::SelectStoredPreset)
                        == OperationAvailability::Available,
                    egui::Button::new(crate::localization::text("Audition this slot on pedal")),
                )
                .clicked()
            {
                self.request_switch(crate::PendingSwitch::Audition(
                    SlotId::new(self.slot_preview).expect("bounded slot"),
                ));
            }
            if ui
                .add_enabled(
                    self.controller
                        .operation_availability(OperationKind::PresetImport)
                        == OperationAvailability::Available
                        && self.stored_write_wait.is_none()
                        && self.bank_scan.in_flight.is_none(),
                    egui::Button::new(crate::localization::text("Copy active draft to this slot…")),
                )
                .clicked()
            {
                self.review_stored_write(false);
            }
            if ui
                .add_enabled(
                    self.controller
                        .operation_availability(OperationKind::ClearPreset)
                        == OperationAvailability::Available
                        && self.stored_write_wait.is_none()
                        && self.bank_scan.in_flight.is_none(),
                    egui::Button::new(crate::localization::text("Clear this slot…")),
                )
                .clicked()
            {
                self.review_stored_write(true);
            }
        }
    }
    pub(super) fn workflow_dialogs(&mut self, ctx: &egui::Context) {
        if let Some(review) = self.stored_write_review.clone() {
            let mut open = true;
            egui::Window::new(crate::localization::text("Review stored preset replacement")).open(&mut open).collapsible(false).default_width(540.0).show(ctx, |ui| {
                ui.label(std::format!("Destination: slot {} — {}", review.slot, review.before.view.display_name()));
                ui.label(if review.clear { crate::localization::text("Replace with the pedal's cleared preset: It's GP-200, tempo 120, level 50.").into() } else { std::format!("Replacement: {}", review.source.view.display_name()) });
                section(ui, crate::localization::text("Changes"));
                if !review.clear { egui::ScrollArea::vertical().max_height(210.0).show(ui, |ui| super::workspaces::render_diff(ui, self.controller.catalog(), &review.before, &review.source)); }
                for reference in &review.source.asset_refs { if !matches!(reference, AssetRef::Factory { .. }) { ui.colored_label(Palette::for_context(ui.ctx()).warning, std::format!("Sound dependency: {reference:?}")); } }
                ui.label(if review.clear { crate::localization::text("The destination will be saved as a local library revision before sending. Clearing replaces one stored slot; it does not audition that slot.") } else { crate::localization::text("The destination and source will be saved as local library revisions before sending. Writing replaces one stored slot; it does not audition that slot.") });
                ui.horizontal_wrapped(|ui| {
                    if ui.button(if review.clear { crate::localization::text("Back up and clear stored slot") } else { crate::localization::text("Back up and replace stored slot") }).clicked() {
                        let intent = self.preserve_write_snapshot(review.before.clone(), std::format!("Before replacement, slot {}: {}", review.slot, review.before.view.display_name()));
                        self.stored_write_wait = Some(crate::StoredWriteWait::Backup { intent, review:review.clone() });
                        self.stored_write_review = None;
                    }
                    if ui.button(crate::localization::text("Cancel")).clicked() { self.stored_write_review = None; }
                });
            });
            if !open {
                self.stored_write_review = None;
            }
        }
        if self.stored_write_wait.is_some() {
            egui::Window::new(crate::localization::text("Replacing stored preset")).collapsible(false).default_width(450.0).show(ctx, |ui| {
                ui.spinner();
                match self.stored_write_wait {
                    Some(crate::StoredWriteWait::Writing { slot, .. }) => { ui.label(std::format!("Writing slot {slot} and checking stored readback. A sent write cannot be withdrawn.")); }
                    _ => { ui.label(crate::localization::text("Saving the destination backup and replacement source locally…")); if ui.button(crate::localization::text("Cancel before sending")).clicked() { self.stored_write_wait = None; } }
                }
            });
        }
        if self.native_export_review {
            let mut open = true;
            egui::Window::new(crate::localization::text("Export GP-200 preset")).open(&mut open).collapsible(false).default_width(470.0).show(ctx, |ui| {
                ui.label(crate::localization::text("Export the selected saved library revision as a native .prst file. Capture your active edited draft first to export its new changes."));
                ui.radio_value(&mut self.native_export_mode, NativeExportMode::PreserveOriginal, crate::localization::text("Preserve imported source and embedded bytes"));
                ui.radio_value(&mut self.native_export_mode, NativeExportMode::PresetOnly, crate::localization::text("Preset only, without embedded sound assets"));
                ui.add_space(10.0);
                ui.label(crate::localization::text("Preserve mode needs an imported native source. Unchanged source bytes are retained exactly; supported edits replace only their recorded fields."));
                ui.label(crate::localization::text("Tags, favorites and library notes belong in a portable .gp200bundle. User IR and SnapTone dependencies may need separate files."));
                ui.small(crate::localization::text("Recovered container compatibility is partial until the original editor has imported the output. The export report lists retained and omitted data."));
                ui.horizontal(|ui| {
                    if ui.add_enabled(self.selected_native_version().is_some(), egui::Button::new(crate::localization::text("Choose destination…"))).clicked() { self.native_export_pending = self.selected_native_version().map(|version| (version, self.native_export_mode)); self.services.dialog(FilePurpose::ExportNative); self.native_export_review = false; }
                    if ui.button(crate::localization::text("Cancel")).clicked() { self.native_export_review = false; }
                });
            });
            if !open {
                self.native_export_review = false;
            }
        }
        if let Some(spec) = self.asset_review.clone() {
            let clo = spec
                .options
                .get("output_format")
                .is_some_and(|format| format == "clo");
            let mut open = true;
            egui::Window::new(if clo { crate::localization::text("Review experimental CLO calibration") } else { crate::localization::text("Review local sound preparation") }).open(&mut open).collapsible(false).default_width(510.0).default_pos(ctx.content_rect().center() - egui::vec2(255.0, 235.0)).show(ctx, |ui| {
                ui.label(spec.source.display().to_string());
                if clo {
                    ui.label(crate::localization::text("Run the independent NAM-to-CLO recipe and retain the original model, selected stimulus, intermediate and final CLO files."));
                    ui.colored_label(Palette::for_context(ui.ctx()).warning, crate::localization::text("Hardware compatibility and original converter equivalence are unverified. This produces a local file only."));
                }
                else if spec.kind == AssetKind::Nam { ui.label(crate::localization::text("Render the inspected NAM model to a local stereo WAV. This output is not a SnapTone upload file.")); }
                else { ui.label(crate::localization::text("Create a local float WAV. Original source bytes remain retained.")); }
                section(ui, crate::localization::text("Selected processing"));
                for (key, value) in &spec.options { ui.label(std::format!("{}: {value}", option_label(key))); }
                if spec.kind == AssetKind::ImpulseResponse && !spec.options.contains_key("sample_rate") { ui.label(crate::localization::text("Sample rate: preserve source")); }
                if spec.kind == AssetKind::ImpulseResponse {
                    if !spec.options.contains_key("normalize_peak") { ui.label(crate::localization::text("Normalization: off")); }
                    if !spec.options.contains_key("truncate_frames") { ui.label(crate::localization::text("Truncation: off")); }
                }
                if spec.kind == AssetKind::Nam {
                    ui.small(crate::localization::text("The worker rechecks the selected source and processing limits. You can cancel the local job after it starts."));
                }
                ui.horizontal(|ui| {
                    if ui.button(if clo { crate::localization::text("Create experimental CLO locally") } else if spec.kind == AssetKind::Nam { crate::localization::text("Render local WAV") } else { crate::localization::text("Prepare local WAV") }).clicked() {
                        self.start_local_asset_job(spec.clone());
                    }
                    if ui.button(crate::localization::text("Cancel")).clicked() { self.asset_review = None; }
                });
            });
            if !open {
                self.asset_review = None;
            }
        }
        if let Some(action) = self.pending_switch.clone() {
            let mut open = true;
            egui::Window::new(crate::localization::text("Keep your preset changes")).open(&mut open).collapsible(false).default_width(470.0).show(ctx, |ui| {
                let name = self.controller.active_document().map(|doc| doc.draft.view.display_name()).unwrap_or(crate::localization::text("This preset"));
                ui.label(std::format!("{name} has unsaved changes."));
                ui.label(crate::localization::text("Save a local revision before switching, explicitly discard the draft, or stay here. Local saving does not replace a stored pedal slot."));
                ui.horizontal_wrapped(|ui| {
                    if ui.button(crate::localization::text("Save to library and continue")).clicked() && let Some(intent) = self.capture_library() { self.switch_wait = Some(SwitchWait::Saving { intent, action:action.clone() }); self.pending_switch = None; }
                    if ui.button(crate::localization::text("Discard and continue")).clicked() { self.discard_for_switch(action.clone()); self.pending_switch = None; }
                    if ui.button(crate::localization::text("Cancel")).clicked() { self.pending_switch = None; }
                });
                if self.controller.active_document().is_some_and(|doc| doc.target == EditTarget::DeviceBuffer) { ui.small(crate::localization::text("Discarding a connected draft sends its supported reverse edits and waits for readback before auditioning another slot.")); }
            });
            if !open {
                self.pending_switch = None;
            }
        }
        if self.switch_wait.is_some() {
            egui::Window::new(crate::localization::text("Finishing preset changes"))
                .collapsible(false)
                .default_width(430.0)
                .show(ctx, |ui| {
                    ui.spinner();
                    ui.label(match self.switch_wait {
                        Some(SwitchWait::Saving { .. }) => crate::localization::text(
                            "Waiting for the local revision to be durably saved…",
                        ),
                        _ => crate::localization::text(
                            "Waiting for the discarded draft to be verified…",
                        ),
                    });
                    if ui
                        .button(crate::localization::text("Stay in this preset"))
                        .clicked()
                    {
                        self.switch_wait = None;
                    }
                });
        }
    }
    pub(super) fn asset_processing_options(&mut self, ui: &mut egui::Ui) {
        if self.selected_asset_kind == AssetKind::Nam {
            self.nam_processing_options(ui);
            return;
        }
        if !matches!(
            self.selected_asset_kind,
            AssetKind::ImpulseResponse | AssetKind::Nam
        ) {
            return;
        }
        section(
            ui,
            if self.selected_asset_kind == AssetKind::Nam {
                crate::localization::text("Local NAM render")
            } else {
                crate::localization::text("Local WAV preparation")
            },
        );
        {
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(
                    &mut self.asset_options.change_rate,
                    crate::localization::text("Change sample rate"),
                );
                ui.add_enabled(
                    self.asset_options.change_rate,
                    egui::DragValue::new(&mut self.asset_options.sample_rate)
                        .range(1..=384000)
                        .suffix(crate::localization::text(" Hz")),
                );
                ui.checkbox(
                    &mut self.asset_options.normalize,
                    crate::localization::text("Normalize peak"),
                );
                ui.add_enabled(
                    self.asset_options.normalize,
                    egui::DragValue::new(&mut self.asset_options.normalize_peak)
                        .range(0.01..=1.0)
                        .speed(0.01),
                );
            });
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(
                    &mut self.asset_options.truncate,
                    crate::localization::text("Truncate after frames"),
                );
                ui.add_enabled(
                    self.asset_options.truncate,
                    egui::DragValue::new(&mut self.asset_options.truncate_frames)
                        .range(1..=16777216),
                );
            });
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(crate::localization::text("Channels"));
            ui.selectable_value(
                &mut self.asset_options.channel_mode,
                0,
                if self.selected_asset_kind == AssetKind::Nam {
                    crate::localization::text("Require mono")
                } else {
                    crate::localization::text("Preserve")
                },
            );
            ui.selectable_value(
                &mut self.asset_options.channel_mode,
                1,
                crate::localization::text("Select channel"),
            );
            ui.selectable_value(
                &mut self.asset_options.channel_mode,
                2,
                crate::localization::text("Average to mono"),
            );
            if self.asset_options.channel_mode == 1 {
                ui.add(
                    egui::DragValue::new(&mut self.asset_options.channel_index)
                        .range(0..=255)
                        .prefix(crate::localization::text("Index ")),
                );
            }
        });
        ui.small(crate::localization::text("These are local processing choices. Pedal upload limits are checked by a separate validated transfer profile."));
    }
    pub(crate) fn start_local_asset_job(&mut self, spec: AssetJobSpec) {
        self.local_asset_jobs.insert(spec.id);
        if spec.kind == AssetKind::Nam {
            self.nam_ui.start_conversion(&spec);
        }
        self.services
            .enqueue(vec![Effect::Assets(if spec.kind == AssetKind::Nam {
                AssetCommand::Convert(spec)
            } else {
                AssetCommand::Prepare(spec)
            })]);
        self.asset_review = None;
    }
}
fn option_label(key: &str) -> &str {
    match key {
        "sample_rate" => crate::localization::text("Sample rate (Hz)"),
        "channel" | "input_channel" => crate::localization::text("Channel selection"),
        "normalize_peak" => crate::localization::text("Target peak"),
        "truncate_frames" => crate::localization::text("Maximum frames"),
        "input_wav" => crate::localization::text("Input WAV"),
        "resample_input" => crate::localization::text("Resample to model rate"),
        "activation" => crate::localization::text("Activation"),
        "max_operations" => crate::localization::text("Rendering budget (operations)"),
        "output_format" => crate::localization::text("Output format"),
        "calibration_timeout_seconds" => crate::localization::text("Calibration timeout (seconds)"),
        _ => key,
    }
}
pub(super) fn waveform(ui: &mut egui::Ui, json: &str) {
    #[derive(serde::Deserialize)]
    struct Bin {
        min: f32,
        max: f32,
    }
    let Ok(bins) = serde_json::from_str::<Vec<Bin>>(json) else {
        return;
    };
    if bins.is_empty()
        || bins.len() > 256
        || bins
            .iter()
            .any(|bin| !bin.min.is_finite() || !bin.max.is_finite())
    {
        return;
    }
    let palette = Palette::for_context(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().min(660.0), 94.0),
        egui::Sense::hover(),
    );
    ui.painter().rect_filled(rect, 3.0, palette.surface);
    let middle = rect.center().y;
    ui.painter().line_segment(
        [
            egui::pos2(rect.left(), middle),
            egui::pos2(rect.right(), middle),
        ],
        egui::Stroke::new(1.0, palette.muted.gamma_multiply(0.35)),
    );
    let scale = bins
        .iter()
        .map(|bin| bin.min.abs().max(bin.max.abs()))
        .fold(1.0_f32, f32::max);
    for (i, bin) in bins.iter().enumerate() {
        let x = rect.left() + (i as f32 + 0.5) * rect.width() / bins.len() as f32;
        ui.painter().line_segment(
            [
                egui::pos2(x, middle - bin.max / scale * 40.0),
                egui::pos2(x, middle - bin.min / scale * 40.0),
            ],
            egui::Stroke::new(
                (rect.width() / bins.len() as f32).clamp(1.0, 4.0),
                palette.accent,
            ),
        );
    }
    ui.small(crate::localization::text(
        "Waveform overview; the original sample values are retained.",
    ));
}
