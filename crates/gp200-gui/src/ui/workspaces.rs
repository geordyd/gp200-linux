use super::section;
use crate::{StudioApp, runtime::FilePurpose, theme::Palette};
use eframe::egui;
use gp200_domain::*;

impl StudioApp {
    pub(super) fn preset_library(&mut self, ui: &mut egui::Ui) {
        ui.heading(crate::localization::text("Preset library"));
        ui.label(crate::localization::text(
            "Keep versions locally, inspect changes, and carry the sounds you need.",
        ));
        ui.horizontal_wrapped(|ui| {
            if ui
                .add(
                    egui::TextEdit::singleline(&mut self.library_search)
                        .hint_text(crate::localization::text("Search names, tags and models"))
                        .desired_width(280.0),
                )
                .changed()
            {
                self.query_library();
            }
            if ui
                .checkbox(
                    &mut self.favorites_only,
                    crate::localization::text("Favorites"),
                )
                .changed()
            {
                self.query_library();
            }
            if ui
                .checkbox(&mut self.library_trash, crate::localization::text("Trash"))
                .changed()
            {
                self.selected_library = None;
                self.query_library();
            }
            if ui.button(crate::localization::text("Import…")).clicked() {
                self.services.dialog(FilePurpose::ImportPresets);
            }
            if ui.button(crate::localization::text("Export…")).clicked() {
                self.services.dialog(FilePurpose::ExportPresets);
            }
            if ui
                .add_enabled(
                    self.selected_native_version().is_some(),
                    egui::Button::new(crate::localization::text("Export native .prst…")),
                )
                .clicked()
            {
                self.native_export_review = true;
            }
            if ui
                .button(crate::localization::text("Back up library…"))
                .clicked()
            {
                self.services.dialog(FilePurpose::Backup);
            }
            if ui
                .button(crate::localization::text("Capture active draft"))
                .clicked()
            {
                self.capture_library();
            }
        });
        ui.add_space(15.0);
        let entries = self.controller.state().library.entries.clone();
        if entries.is_empty() {
            section(ui, crate::localization::text("Your collection starts here"));
            ui.label(crate::localization::text("Capture the active preset to make its first local version, or import an existing portable bundle."));
        }
        for entry in entries {
            let selected = self.selected_library.as_ref() == Some(&entry.id);
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(selected, &entry.metadata.name)
                    .clicked()
                {
                    self.selected_library = Some(entry.id.clone());
                    self.library_metadata = entry.metadata.clone();
                    self.library_tags = entry.metadata.tags.join(", ");
                    let intent = self.local_intent();
                    self.send(UiIntent::Library(LibraryCommand::ListVersions {
                        intent,
                        library: entry.id.clone(),
                    }));
                }
                ui.small(std::format!("{} versions", entry.version_count));
                ui.small(entry.metadata.tags.join(", "));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(crate::localization::text("Open draft")).clicked() {
                        self.request_switch(crate::PendingSwitch::Library(
                            entry.latest_version.clone(),
                        ));
                        self.send(UiIntent::Navigate(ViewId::Editor));
                    }
                });
            });
            ui.separator();
        }
        if let Some(library) = self.selected_library.clone() {
            section(ui, crate::localization::text("Local preset details"));
            ui.horizontal(|ui| {
                ui.label(crate::localization::text("Name"));
                ui.text_edit_singleline(&mut self.library_metadata.name);
                ui.checkbox(
                    &mut self.library_metadata.favorite,
                    crate::localization::text("Favorite"),
                );
            });
            ui.horizontal(|ui| {
                ui.label(crate::localization::text("Author"));
                ui.text_edit_singleline(&mut self.library_metadata.author);
                ui.label(crate::localization::text("Style"));
                ui.text_edit_singleline(&mut self.library_metadata.style);
            });
            ui.horizontal(|ui| {
                ui.label(crate::localization::text("Tags"));
                ui.text_edit_singleline(&mut self.library_tags);
            });
            ui.add(
                egui::TextEdit::multiline(&mut self.library_metadata.notes)
                    .hint_text(crate::localization::text("Notes"))
                    .desired_rows(2),
            );
            ui.horizontal(|ui| {
                if ui
                    .button(crate::localization::text("Save local details"))
                    .clicked()
                {
                    self.library_metadata.tags = self
                        .library_tags
                        .split(',')
                        .map(str::trim)
                        .filter(|tag| !tag.is_empty())
                        .map(str::to_owned)
                        .collect();
                    let intent = self.local_intent();
                    self.send(UiIntent::Library(LibraryCommand::UpdateMetadata {
                        intent,
                        library: library.clone(),
                        metadata: self.library_metadata.clone(),
                    }));
                }
                if !self.library_trash
                    && ui
                        .button(crate::localization::text("Move to trash"))
                        .clicked()
                {
                    let intent = self.local_intent();
                    self.send(UiIntent::Library(LibraryCommand::DeleteToTrash {
                        intent,
                        library: library.clone(),
                    }));
                    self.trashed_library = Some(library.clone());
                    self.selected_library = None;
                }
                if self.library_trash
                    && ui
                        .button(crate::localization::text("Restore preset"))
                        .clicked()
                {
                    let intent = self.local_intent();
                    self.send(UiIntent::Library(LibraryCommand::RestoreTrash {
                        intent,
                        library: library.clone(),
                    }));
                    self.selected_library = None;
                }
            });
        }
        if let Some(library) = self.trashed_library.clone()
            && ui
                .button(crate::localization::text("Restore last trashed preset"))
                .clicked()
        {
            let intent = self.local_intent();
            self.send(UiIntent::Library(LibraryCommand::RestoreTrash {
                intent,
                library,
            }));
            self.trashed_library = None;
        }
        if let Some(library) = &self.selected_library {
            let mut versions: Vec<_> = self
                .controller
                .state()
                .library
                .versions
                .values()
                .filter(|version| &version.library == library)
                .cloned()
                .collect();
            versions.sort_by(|a, b| a.created_at.cmp(&b.created_at));
            section(ui, crate::localization::text("Revision history"));
            for (index, version) in versions.iter().enumerate() {
                egui::CollapsingHeader::new(format!("{} · {}", index + 1, version.created_at))
                    .default_open(index + 1 == versions.len())
                    .show(ui, |ui| {
                        ui.label(&version.metadata.name);
                        ui.label(&version.metadata.notes);
                        ui.small(std::format!("Source: {:?}", version.snapshot.source));
                        if let Some(previous) =
                            index.checked_sub(1).and_then(|index| versions.get(index))
                        {
                            render_diff(
                                ui,
                                self.controller.catalog(),
                                &previous.snapshot,
                                &version.snapshot,
                            );
                        }
                        if ui
                            .button(crate::localization::text("Open this revision"))
                            .clicked()
                        {
                            self.request_switch(crate::PendingSwitch::Library(version.id.clone()));
                            self.send(UiIntent::Navigate(ViewId::Editor));
                        }
                        section(ui, crate::localization::text("Asset references"));
                        for reference in &version.snapshot.asset_refs {
                            if !matches!(reference, AssetRef::Factory { .. }) {
                                ui.label(format!("{reference:?}"));
                            }
                        }
                    });
            }
        }
        if let Some(report) = &self.controller.state().library.last_conversion {
            section(ui, crate::localization::text("Import / export report"));
            ui.label(std::format!("Compatibility: {:?}", report.level));
            for note in &report.notes {
                ui.label(crate::localization::message(note).as_ref());
            }
            for reference in &report.unresolved_assets {
                ui.label(std::format!("Missing source: {reference:?}"));
            }
        }
        if let Some(path) = &self.controller.state().library.last_export {
            ui.label(std::format!("Saved: {}", path.display()));
        }
    }
    pub(super) fn assets(&mut self, ui: &mut egui::Ui) {
        let history =
            self.selected_asset_kind == AssetKind::ImpulseResponse && self.ir_history_ui.open;
        let device_review = self.asset_job_ui.open
            && !history
            && matches!(
                self.selected_asset_kind,
                AssetKind::ImpulseResponse | AssetKind::SnapTone
            );
        if self.selected_asset_kind != AssetKind::Firmware && !device_review && !history {
            ui.heading(crate::localization::text("Sound assets"));
        }
        if !matches!(
            self.selected_asset_kind,
            AssetKind::Nam | AssetKind::Firmware
        ) && !device_review
            && !history
        {
            ui.label(crate::localization::text("Keep original sources with your presets and inspect every conversion before transfer."));
        }
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(
                &mut self.selected_asset_kind,
                AssetKind::ImpulseResponse,
                crate::localization::text("User IR"),
            );
            ui.selectable_value(
                &mut self.selected_asset_kind,
                AssetKind::SnapTone,
                crate::localization::text("SnapTone"),
            );
            ui.selectable_value(
                &mut self.selected_asset_kind,
                AssetKind::Nam,
                crate::localization::text("NAM conversion"),
            );
            ui.selectable_value(
                &mut self.selected_asset_kind,
                AssetKind::Firmware,
                crate::localization::text("Firmware"),
            );
        });
        ui.add_space(if self.selected_asset_kind == AssetKind::Nam {
            4.0
        } else {
            10.0
        });
        if self.selected_asset_kind == AssetKind::Firmware {
            self.firmware_workspace(ui);
            return;
        }
        if matches!(
            self.selected_asset_kind,
            AssetKind::ImpulseResponse | AssetKind::SnapTone
        ) {
            #[derive(Clone, Copy, PartialEq, Eq)]
            enum AssetSubview {
                Sources,
                DeviceReview,
                History,
            }
            let mut subview = if self.selected_asset_kind == AssetKind::ImpulseResponse
                && self.ir_history_ui.open
            {
                AssetSubview::History
            } else if self.asset_job_ui.open {
                AssetSubview::DeviceReview
            } else {
                AssetSubview::Sources
            };
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(
                    &mut subview,
                    AssetSubview::Sources,
                    crate::localization::text("Slots and sources"),
                );
                ui.selectable_value(
                    &mut subview,
                    AssetSubview::DeviceReview,
                    crate::localization::text("Device review"),
                );
                if self.selected_asset_kind == AssetKind::ImpulseResponse {
                    ui.selectable_value(&mut subview, AssetSubview::History, "IR history");
                }
            });
            if subview == AssetSubview::History {
                if !self.ir_history_ui.open {
                    self.refresh_ir_history();
                }
                self.ir_history_ui.open = true;
                self.asset_job_ui.open = false;
                self.ir_history_workspace(ui);
                return;
            }
            self.ir_history_ui.open = false;
            self.asset_job_ui.open = subview == AssetSubview::DeviceReview;
            if self.asset_job_ui.open {
                self.asset_job_review_workspace(ui);
                return;
            }
            self.asset_slots(ui);
        }
        ui.horizontal_wrapped(|ui| {
            if ui
                .button(crate::localization::text("Select source…"))
                .clicked()
            {
                self.services.dialog(match self.selected_asset_kind {
                    AssetKind::ImpulseResponse => FilePurpose::ImportIr,
                    AssetKind::SnapTone => FilePurpose::ImportSnapTone,
                    AssetKind::Nam => FilePurpose::ImportNam,
                    AssetKind::Firmware => FilePurpose::Firmware,
                });
            }
            ui.add(
                egui::TextEdit::singleline(&mut self.asset_path)
                    .hint_text(crate::localization::text("Source file path"))
                    .desired_width(360.0),
            );
            if ui
                .add_enabled(
                    !self.asset_path.is_empty(),
                    egui::Button::new(crate::localization::text("Inspect source")),
                )
                .clicked()
            {
                self.submit_asset(false);
            }
            if self.selected_asset_kind == AssetKind::ImpulseResponse
                && ui
                    .add_enabled(
                        !self.asset_path.is_empty(),
                        egui::Button::new(crate::localization::text("Prepare locally")),
                    )
                    .clicked()
            {
                self.submit_asset(true);
            }
        });
        self.asset_processing_options(ui);
        section(ui, crate::localization::text("Retained sources"));
        let assets = self.controller.state().assets.clone();
        if assets.is_empty() {
            ui.label(crate::localization::text("Select a source file to inspect its format. Local preparation does not change the pedal."));
        }
        for asset in assets {
            egui::CollapsingHeader::new(std::format!(
                "{} · {:?} · {} bytes",
                asset.name,
                asset.kind,
                asset.bytes
            ))
            .default_open(self.selected_asset_kind != AssetKind::Nam)
            .show(ui, |ui| {
                ui.small(asset.path.display().to_string());
                ui.small(std::format!("Content: {}", asset.id));
                if matches!(asset.kind, AssetKind::ImpulseResponse | AssetKind::SnapTone)
                    && ui
                        .button(crate::localization::text("Review for a pedal slot"))
                        .clicked()
                {
                    self.select_asset_for_review(asset.clone());
                }
                if asset.properties.contains_key("calibration_report_json") {
                    super::nam_assets::calibration_summary(ui, &asset);
                }
                self.nam_export_button(ui, &asset);
                super::nam_assets::conversion_history(ui, &asset);
                if let Some(json) = asset.properties.get("waveform_json") {
                    super::workflows::waveform(ui, json);
                }
                for (key, value) in &asset.properties {
                    if key.ends_with("_json") {
                        continue;
                    }
                    ui.label(std::format!("{key}: {value}"));
                }
                for warning in &asset.warnings {
                    ui.colored_label(
                        Palette::for_context(ui.ctx()).warning,
                        crate::localization::message(warning),
                    );
                }
            });
        }
        if self.selected_asset_kind == AssetKind::Firmware {
            section(ui, crate::localization::text("Firmware preflight"));
            ui.label(crate::localization::text("A firmware job needs a matching GP-200 image, a validated bootloader profile, and a complete preflight. File inspection alone does not authorize or start a flash."));
        }
        section(ui, crate::localization::text("Replacement impact"));
        let kind = self.selected_asset_kind;
        let slot = self.selected_asset_slot;
        let references: Vec<_> = self.controller.state().documents.values().filter(|document| document.baseline.asset_refs.iter().any(|reference| matches!(reference,AssetRef::UserSlot { kind:actual_kind,slot:actual_slot,.. } if *actual_kind == kind && *actual_slot == slot))).map(|document| document.draft.view.display_name().to_owned()).collect();
        if references.is_empty() {
            ui.label(crate::localization::text("No open document references this slot. Other stored presets may still depend on it until a complete bank scan is available."));
        } else {
            for name in references {
                ui.label(std::format!("Referenced by {name}"));
            }
        }
    }
    pub(crate) fn submit_asset(&mut self, prepare: bool) {
        if self.selected_asset_kind == AssetKind::Nam {
            if !prepare {
                self.inspect_nam_file(
                    std::path::PathBuf::from(&self.asset_path),
                    super::nam_assets::InspectionPurpose::Model,
                );
                return;
            }
            if self.nam_ui.busy() {
                self.services.error(crate::localization::text(
                    "Finish or cancel the current local NAM conversion first.",
                ));
                return;
            }
            if let Some(issue) = self
                .nam_ui
                .preflight(&self.asset_path, &self.asset_options)
                .issues
                .into_iter()
                .next()
            {
                self.services.error(issue);
                return;
            }
        }
        self.next_job += 1;
        let id = JobId(self.next_job);
        let source = std::path::PathBuf::from(&self.asset_path);
        if prepare {
            match self
                .asset_options
                .specification(self.selected_asset_kind, source, id)
            {
                Ok(spec) => self.asset_review = Some(spec),
                Err(message) => self.services.error(message),
            }
        } else {
            self.local_asset_jobs.insert(id);
            self.services
                .enqueue(vec![Effect::Assets(AssetCommand::Inspect(AssetJobSpec {
                    id,
                    kind: self.selected_asset_kind,
                    source,
                    output: None,
                    options: Default::default(),
                }))]);
        }
    }
    pub(super) fn diagnostics(&mut self, ui: &mut egui::Ui) {
        ui.heading(crate::localization::text("Operation diagnostics"));
        ui.small(crate::localization::text("Session epochs and readback keep responses associated with the correct pedal and preset."));
        ui.horizontal(|ui| {
            if ui
                .button(crate::localization::text("Clear displayed worker errors"))
                .clicked()
            {
                self.services.errors.clear();
            }
            ui.label(std::format!(
                "Connection epoch {}",
                self.controller.state().epoch
            ));
        });
        section(ui, crate::localization::text("Recent operations"));
        for message in self.controller.state().messages.iter().rev() {
            ui.label(format!(
                "{} {:?}: {}",
                message.sequence, message.severity, message.message
            ));
        }
        section(ui, crate::localization::text("Validated operations"));
        for (operation, support) in &self.controller.state().capabilities.operations {
            ui.label(std::format!("{operation:?}: {support:?}"));
        }
        section(ui, crate::localization::text("Transport events"));
        for event in self.controller.state().diagnostics.iter().rev() {
            egui::CollapsingHeader::new(format!("{}: {}", event.category, event.message)).show(
                ui,
                |ui| {
                    if let Some(bytes) = &event.bytes {
                        ui.monospace(
                            bytes
                                .iter()
                                .map(|byte| format!("{byte:02x}"))
                                .collect::<Vec<_>>()
                                .join(" "),
                        );
                    }
                },
            );
        }
        if let Some(document) = self.controller.active_document() {
            section(ui, crate::localization::text("Retained preset evidence"));
            ui.label(std::format!(
                "{} raw bytes · {} records · revision {}",
                document.baseline.raw.payload().len(),
                document.baseline.raw.records().len(),
                document.local_revision
            ));
            for module in &document.draft.view.modules {
                ui.small(std::format!(
                    "{} Code {:08x} state {}",
                    module.module.name(),
                    module.model.0,
                    module.state_raw
                ));
            }
        }
    }
    pub(super) fn settings(&mut self, ui: &mut egui::Ui) {
        ui.heading(crate::localization::text("Application settings"));
        let mut preferences = self.controller.state().preferences.clone();
        section(ui, crate::localization::text("Appearance"));
        ui.horizontal(|ui| {
            ui.selectable_value(
                &mut preferences.theme,
                ThemePreference::System,
                crate::localization::text("System"),
            );
            ui.selectable_value(
                &mut preferences.theme,
                ThemePreference::Light,
                crate::localization::text("Light"),
            );
            ui.selectable_value(
                &mut preferences.theme,
                ThemePreference::Dark,
                crate::localization::text("Dark"),
            );
        });
        ui.add(
            egui::Slider::new(&mut preferences.scale, 1.0..=2.0)
                .step_by(0.05)
                .text(crate::localization::text("UI scale")),
        );
        ui.checkbox(
            &mut preferences.compact,
            crate::localization::text("Prefer compact panels"),
        );
        if preferences != self.controller.state().preferences {
            self.send(UiIntent::SetPreferences(preferences));
        }

        section(ui, crate::localization::text("File dialog folders"));
        ui.small(crate::localization::text("Folders are remembered separately after you select a file. Removed or unmounted folders use the desktop default. Forgetting a folder removes only this preference."));
        let directories = self
            .controller
            .state()
            .preferences
            .dialog_directories
            .clone();
        for purpose in DialogDirectoryPurpose::ALL {
            ui.horizontal(|ui| {
                let label_width = (ui.available_width() * 0.32).min(245.0);
                ui.add_sized(
                    [label_width, 22.0],
                    egui::Label::new(
                        egui::RichText::new(crate::localization::text(purpose.label())).strong(),
                    )
                    .truncate(),
                );
                if let Some(path) = directories.get(&purpose) {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button(crate::localization::text("Forget"))
                            .clicked()
                        {
                            self.services.forget_dialog_directory(Some(purpose));
                        }
                        let full_path = path.to_string_lossy();
                        ui.add(egui::Label::new(full_path.as_ref()).truncate())
                            .on_hover_text(full_path.as_ref())
                            .context_menu(|ui| {
                                if ui
                                    .button(crate::localization::text("Copy full folder path"))
                                    .clicked()
                                {
                                    ui.ctx().copy_text(full_path.into_owned());
                                    ui.close();
                                }
                            });
                    });
                } else {
                    ui.weak(crate::localization::text("System default"));
                }
            });
        }
        if ui
            .add_enabled(
                !directories.is_empty(),
                egui::Button::new(crate::localization::text("Forget all remembered folders")),
            )
            .clicked()
        {
            self.services.forget_dialog_directory(None);
        }
        egui::CollapsingHeader::new(crate::localization::text("Local storage details")).show(ui, |ui| {
            ui.label(std::format!("Preferences: {}", crate::runtime::config_path().display()));
            ui.small(crate::localization::text("Preference changes persist on the file worker and keep active drafts and jobs intact."));
        });
        section(
            ui,
            crate::localization::text("Help and application information"),
        );
        ui.horizontal_wrapped(|ui| {
            if ui
                .button(crate::localization::text("Getting started"))
                .clicked()
            {
                self.open_help_topic(super::help::HelpTopic::Start);
            }
            if ui
                .button(crate::localization::text("About and release notes"))
                .clicked()
            {
                self.open_help_topic(super::help::HelpTopic::About);
            }
        });
    }
}
pub(super) fn render_diff(
    ui: &mut egui::Ui,
    catalog: &gp200_catalog::Catalog,
    before: &PresetSnapshot,
    after: &PresetSnapshot,
) {
    let mut changes = 0;
    for module in &after.view.modules {
        if let Some(previous) = before.view.module(module.module) {
            if module.model != previous.model {
                ui.label(std::format!(
                    "{} model {:08x} → {:08x}",
                    module.module.name(),
                    previous.model.0,
                    module.model.0
                ));
                changes += 1;
            }
            if module.state_raw != previous.state_raw {
                ui.label(std::format!(
                    "{} state {} → {}",
                    module.module.name(),
                    previous.state_raw,
                    module.state_raw
                ));
                changes += 1;
            }
            for (index, value) in module.parameters.iter().enumerate() {
                if let Some(previous) = previous.parameters.get(index)
                    && value != previous
                {
                    let definition = catalog
                        .model_by_id(module.module.get(), module.model.0)
                        .and_then(|model| model.parameter(index as u8));
                    let name = definition
                        .map(|parameter| parameter.name.clone())
                        .unwrap_or_else(|| std::format!("Retained parameter {index}"));
                    let previous_text = definition
                        .map(|parameter| {
                            parameter
                                .presentation(
                                    previous.as_f32(),
                                    &before
                                        .view
                                        .module(module.module)
                                        .unwrap()
                                        .parameters
                                        .iter()
                                        .map(|bits| bits.as_f32())
                                        .collect::<Vec<_>>(),
                                )
                                .text
                        })
                        .unwrap_or_else(|| previous.as_f32().to_string());
                    let value_text = definition
                        .map(|parameter| {
                            parameter
                                .presentation(
                                    value.as_f32(),
                                    &module
                                        .parameters
                                        .iter()
                                        .map(|bits| bits.as_f32())
                                        .collect::<Vec<_>>(),
                                )
                                .text
                        })
                        .unwrap_or_else(|| value.as_f32().to_string());
                    ui.label(std::format!(
                        "{} {name}: {previous_text} → {value_text}",
                        module.module.name()
                    ));
                    changes += 1;
                }
            }
        }
    }
    if changes == 0 {
        ui.label(crate::localization::text(
            "No module parameter changes from the preceding revision.",
        ));
    }
}
