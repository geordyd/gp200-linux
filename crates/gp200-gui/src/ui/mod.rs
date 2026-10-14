pub(crate) mod asset_job_review;
pub(crate) mod asset_slots;
pub(crate) mod assignments;
mod backups;
pub(crate) mod controls;
mod diff_view;
mod editor;
pub(crate) mod firmware;
pub(crate) mod firmware_history;
#[cfg(test)]
mod firmware_history_tests;
pub(crate) mod firmware_updater;
#[cfg(test)]
mod firmware_updater_tests;
pub(crate) mod globals;
pub(crate) mod help;
pub(crate) mod ir_history;
pub(crate) mod library_workflows;
mod model_picker;
pub(crate) mod nam_assets;
mod performance;
mod practice;
mod recovery;
pub(crate) mod selective_merge;
mod workflows;
mod workspaces;
use crate::{StudioApp, theme::Palette};
use eframe::egui::{self, RichText};
use gp200_app::ComparisonSlot;
use gp200_domain::*;

const NAVIGATION: [(ViewId, &str); 10] = [
    (ViewId::Editor, "Editor"),
    (ViewId::Library, "Library"),
    (ViewId::Assets, "Assets"),
    (ViewId::Assignments, "Assignments"),
    (ViewId::GlobalSettings, "Device"),
    (ViewId::Performance, "Performance"),
    (ViewId::Practice, "Practice"),
    (ViewId::Diagnostics, "Diagnostics"),
    (ViewId::AppSettings, "Settings"),
    (ViewId::Help, "Help"),
];
impl StudioApp {
    pub(crate) fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.firmware_close_blocked() && !ctx.text_edit_focused() {
            ctx.input_mut(|input| {
                for shortcut in [
                    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Z),
                    egui::KeyboardShortcut::new(
                        egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                        egui::Key::Z,
                    ),
                    egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::PageUp),
                    egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::PageDown),
                ] {
                    input.consume_shortcut(&shortcut);
                }
            });
        }
        // TextEdit also backs focused DragValue entry. Let that widget consume
        // its own history shortcut before considering preset history.
        let action = if ctx.text_edit_focused() {
            None
        } else {
            ctx.input_mut(|input| {
                if input.consume_shortcut(&egui::KeyboardShortcut::new(
                    egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                    egui::Key::Z,
                )) {
                    Some(true)
                } else if input.consume_shortcut(&egui::KeyboardShortcut::new(
                    egui::Modifiers::COMMAND,
                    egui::Key::Z,
                )) {
                    Some(false)
                } else {
                    None
                }
            })
        };
        if let Some(redo) = action
            && let Some(document) = self.controller.active_document_id().cloned()
        {
            self.send(if redo {
                UiIntent::Redo(document)
            } else {
                UiIntent::Undo(document)
            });
        }
        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.firmware_escape();
            self.model_picker = false;
            self.save_review = false;
            self.connect_dialog = false;
            self.asset_review = None;
            self.native_export_review = false;
            self.pending_switch = None;
        }
        if self.controller.state().view == ViewId::Performance {
            if ctx.input(|input| input.key_pressed(egui::Key::PageDown)) {
                self.step_setlist(1);
            }
            if ctx.input(|input| input.key_pressed(egui::Key::PageUp)) {
                self.step_setlist(-1);
            }
        }
    }
    pub(crate) fn render(&mut self, ui: &mut egui::Ui) {
        let palette = Palette::for_context(ui.ctx());
        egui::Panel::top("application-toolbar")
            .frame(
                egui::Frame::new()
                    .fill(palette.surface)
                    .inner_margin(egui::Margin::symmetric(18, 12)),
            )
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(crate::localization::text("GP200 Linux"))
                            .size(22.0)
                            .strong(),
                    );
                    ui.add_space(16.0);
                    let state = self.controller.state();
                    ui.label(
                        RichText::new(connection_label(state.connection)).color(palette.muted),
                    );
                    if let Some(identity) = &state.identity {
                        ui.small(format!(
                            "{}{}",
                            identity.model.name(),
                            if identity.mode == BackendMode::Simulator {
                                crate::localization::text(" simulator")
                            } else {
                                ""
                            }
                        ));
                    }
                    ui.horizontal_wrapped(|ui| {
                        let paused = self.firmware_close_blocked();
                        if self.controller.state().connection == ConnectionState::Ready {
                            if ui
                                .add_enabled(
                                    !paused,
                                    egui::Button::new(crate::localization::text("Disconnect")),
                                )
                                .clicked()
                            {
                                self.send(UiIntent::Disconnect);
                            }
                            if ui
                                .add_enabled(
                                    !paused,
                                    egui::Button::new(crate::localization::text("Read pedal")),
                                )
                                .clicked()
                            {
                                self.send(UiIntent::Refresh(PresetTarget::EditBuffer));
                            }
                        } else if ui
                            .add_enabled(
                                !paused,
                                egui::Button::new(crate::localization::text("Connect pedal")),
                            )
                            .clicked()
                        {
                            self.connect_dialog = true;
                            self.send(UiIntent::DiscoverDevices);
                        }
                        let mut theme = self.controller.state().preferences.theme;
                        egui::ComboBox::from_id_salt("toolbar-theme")
                            .selected_text(match theme {
                                ThemePreference::Dark => crate::localization::text("Dark"),
                                ThemePreference::Light => crate::localization::text("Light"),
                                ThemePreference::System => crate::localization::text("System"),
                            })
                            .width(82.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut theme,
                                    ThemePreference::System,
                                    crate::localization::text("System"),
                                );
                                ui.selectable_value(
                                    &mut theme,
                                    ThemePreference::Light,
                                    crate::localization::text("Light"),
                                );
                                ui.selectable_value(
                                    &mut theme,
                                    ThemePreference::Dark,
                                    crate::localization::text("Dark"),
                                );
                            });
                        if theme != self.controller.state().preferences.theme {
                            let mut preferences = self.controller.state().preferences.clone();
                            preferences.theme = theme;
                            self.send(UiIntent::SetPreferences(preferences));
                        }
                    });
                });
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    for (view, label) in NAVIGATION {
                        if ui
                            .selectable_label(
                                self.controller.state().view == view,
                                crate::localization::text(label),
                            )
                            .clicked()
                        {
                            self.send(UiIntent::Navigate(view));
                            if view == ViewId::Library {
                                self.query_library();
                                self.refresh_library_workflows();
                            }
                            if view == ViewId::Performance {
                                self.refresh_library_workflows();
                            }
                            if view == ViewId::Assets {
                                self.services
                                    .enqueue(vec![Effect::Assets(AssetCommand::List)]);
                            }
                        }
                    }
                });
                self.observed_pedal_change(ui);
                self.firmware_ownership_strip(ui);
            });
        self.draft_recovery_banner(ui);
        egui::Panel::bottom("operation-footer")
            .frame(
                egui::Frame::new()
                    .fill(palette.surface)
                    .inner_margin(egui::Margin::symmetric(18, 8)),
            )
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if let Some(error) = self.services.errors.back() {
                        ui.colored_label(palette.warning, crate::localization::message(error));
                    } else {
                        ui.label(
                            RichText::new(crate::localization::message(&self.notice))
                                .small()
                                .color(palette.muted),
                        );
                    }
                    let jobs: Vec<_> = self
                        .controller
                        .state()
                        .jobs
                        .iter()
                        .map(|(id, job)| (*id, job.clone()))
                        .collect();
                    for (id, job) in jobs {
                        if job.total.is_none_or(|total| job.completed < total) {
                            ui.label(crate::localization::message(&job.message));
                            if let Some(total) = job.total
                                && total > 0
                            {
                                ui.add(
                                    egui::ProgressBar::new(job.completed as f32 / total as f32)
                                        .desired_width(100.0),
                                );
                            }
                            if job.cancellable
                                && ui
                                    .small_button(crate::localization::text("Cancel"))
                                    .clicked()
                            {
                                self.cancel_scoped_job(id);
                            }
                        }
                    }
                });
            });
        if matches!(
            self.controller.state().view,
            ViewId::Editor | ViewId::Performance
        ) {
            egui::Panel::left("preset-browser")
                .resizable(true)
                .default_size(200.0)
                .size_range(160.0..=320.0)
                .frame(egui::Frame::new().fill(palette.surface).inner_margin(14))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("preset-browser-scroll")
                        .show(ui, |ui| self.browser(ui));
                });
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(palette.canvas).inner_margin(20))
            .show(ui, |ui| {
                if self.controller.state().view == ViewId::Assets
                    && self.selected_asset_kind == AssetKind::Firmware
                {
                    self.assets(ui);
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt("workspace-scroll")
                    .show(ui, |ui| match self.controller.state().view {
                        ViewId::Editor => self.editor(ui),
                        ViewId::Library => self.library(ui),
                        ViewId::Assets => self.assets(ui),
                        ViewId::Assignments => self.assignments(ui),
                        ViewId::GlobalSettings => self.globals(ui),
                        ViewId::Performance => self.performance(ui),
                        ViewId::Practice => self.practice(ui),
                        ViewId::Diagnostics => self.diagnostics(ui),
                        ViewId::AppSettings => self.settings(ui),
                        ViewId::Help => self.help(ui),
                    });
            });
        self.dialogs(ui.ctx());
        self.firmware_close_notice(ui.ctx());
    }
    fn browser(&mut self, ui: &mut egui::Ui) {
        if self.controller.state().view == ViewId::Performance {
            self.performance_browser(ui);
            return;
        }
        let palette = Palette::for_context(ui.ctx());
        ui.label(
            RichText::new(if self.controller.state().view == ViewId::Performance {
                crate::localization::text("Setlist")
            } else {
                crate::localization::text("Presets")
            })
            .size(17.0)
            .strong(),
        );
        ui.add(
            egui::TextEdit::singleline(&mut self.browser_search)
                .hint_text(crate::localization::text("Find a preset"))
                .desired_width(f32::INFINITY),
        );
        ui.add_space(12.0);
        ui.small(crate::localization::text("Open documents"));
        let documents: Vec<_> = self
            .controller
            .state()
            .documents
            .values()
            .filter(|doc| {
                doc.draft
                    .view
                    .display_name()
                    .to_lowercase()
                    .contains(&self.browser_search.to_lowercase())
            })
            .map(|doc| {
                (
                    doc.id.clone(),
                    doc.draft.view.display_name().to_owned(),
                    doc.target.clone(),
                    doc.dirty(),
                )
            })
            .collect();
        for (id, name, target, dirty) in documents {
            let response = ui.selectable_label(
                self.controller.active_document_id() == Some(&id),
                format!("{}{}", name, if dirty { " *" } else { "" }),
            );
            if response.clicked() {
                self.request_switch(crate::PendingSwitch::Document(id.clone()));
            }
            ui.label(
                RichText::new(match target {
                    EditTarget::DeviceBuffer => crate::localization::text("Pedal edit buffer"),
                    EditTarget::Offline(_) => crate::localization::text("Offline document"),
                })
                .small()
                .color(palette.muted),
            );
            ui.add_space(6.0);
        }
        ui.separator();
        if ui
            .button(crate::localization::text("New from capture"))
            .clicked()
        {
            self.request_switch(crate::PendingSwitch::Snapshot(
                gp200_sim::captured_demo_snapshot(),
            ));
        }
        ui.add_space(12.0);
        ui.small(crate::localization::text("Stored pedal slot"));
        ui.add(
            egui::DragValue::new(&mut self.slot_preview)
                .range(0..=255)
                .prefix(crate::localization::text("Slot ")),
        );
        let available = self
            .controller
            .operation_availability(OperationKind::ReadStoredPreset)
            == OperationAvailability::Available;
        if ui
            .add_enabled(
                available,
                egui::Button::new(crate::localization::text("Read stored preview")),
            )
            .clicked()
        {
            self.send(UiIntent::Refresh(PresetTarget::Stored(
                SlotId::new(self.slot_preview).expect("bounded slot"),
            )));
        }
        if let Some(slot) = self.controller.state().preview_slot {
            ui.colored_label(palette.accent, std::format!("Preview slot {slot}"));
        }
        self.bank_controls(ui);
        ui.add_space(8.0);
        for (slot, cached) in self.controller.state().bank.slots.clone() {
            if !cached
                .snapshot
                .view
                .display_name()
                .to_lowercase()
                .contains(&self.browser_search.to_lowercase())
            {
                continue;
            }
            if ui
                .selectable_label(
                    self.slot_preview == slot.get(),
                    std::format!("{slot}: {}", cached.snapshot.view.display_name()),
                )
                .clicked()
            {
                self.slot_preview = slot.get();
            }
        }
        ui.add_space(15.0);
        ui.separator();
        ui.label(
            RichText::new(crate::localization::text(
                "322 models\n1,400 controls\n11 effect modules",
            ))
            .small()
            .color(palette.muted),
        );
    }
    fn dialogs(&mut self, ctx: &egui::Context) {
        self.draft_recovery_dialog(ctx);
        self.workflow_dialogs(ctx);
        if self.model_picker {
            self.model_dialog(ctx);
        }
        if self.connect_dialog {
            let mut open = true;
            egui::Window::new(crate::localization::text("Connect GP-200")).open(&mut open).default_width(460.0).collapsible(false).show(ctx, |ui| {
                ui.label(crate::localization::text("Connect the pedal by USB in Normal mode. USB audio access is separate from MIDI editing.")); ui.add_space(8.0);
                let ports = self.controller.state().ports.clone();
                if ports.is_empty() { ui.label(crate::localization::text("No matching MIDI port yet. Check the USB cable and Normal USB mode.")); }
                for (index,port) in ports.iter().enumerate() { ui.radio_value(&mut self.selected_port,index,&port.display_name); ui.small(&port.matching_confidence); }
                ui.horizontal(|ui| { if ui.button(crate::localization::text("Refresh ports")).clicked() { self.send(UiIntent::DiscoverDevices); }
if ui.add_enabled(!ports.is_empty(),egui::Button::new(crate::localization::text("Connect selected"))).clicked() && let Some(port) = ports.get(self.selected_port) { self.send(UiIntent::Connect(port.selection.clone())); self.connect_dialog = false; } });
            });
            if !open {
                self.connect_dialog = false;
            }
        }
        if self.save_review {
            let mut open = true;
            egui::Window::new(crate::localization::text("Save to pedal slot")).open(&mut open).default_width(460.0).collapsible(false).show(ctx, |ui| {
                ui.label(crate::localization::text("Saving replaces one stored preset. Capture to library keeps a separate local version."));
                ui.add(egui::DragValue::new(&mut self.save_slot).range(0..=255).prefix(crate::localization::text("Destination slot ")));
                if let Some(document) = self.controller.active_document() { ui.label(std::format!("Preset: {}",document.draft.view.display_name())); ui.label(std::format!("{} modules, {} local history steps",document.draft.view.modules.len(),document.undo.past.len())); }
                let availability = self.controller.operation_availability(OperationKind::SavePreset);
                let live_document = self.controller.active_document().is_some_and(|document| document.target == EditTarget::DeviceBuffer);
                match &availability { OperationAvailability::Available => { if !live_document { ui.label(crate::localization::text("Open the pedal edit-buffer document before saving. This offline draft has not been sent to the pedal.")); } }, OperationAvailability::OfflineOnly => { ui.label(crate::localization::text("Connect to save a verified buffer.")); }, OperationAvailability::Blocked { reason } => { ui.label(crate::localization::message(reason)); } }
                if ui.add_enabled(availability == OperationAvailability::Available && live_document,egui::Button::new(crate::localization::text("Save verified buffer to slot"))).clicked() && let Some(snapshot) = &self.controller.state().confirmed_buffer { self.send(UiIntent::SaveBufferTo { slot: SlotId::new(self.save_slot).expect("bounded"), expected_baseline: snapshot.revision }); self.save_review = false; }
            });
            if !open {
                self.save_review = false;
            }
        }
    }
    fn history_toolbar(&mut self, ui: &mut egui::Ui) {
        let Some(document) = self.controller.active_document().cloned() else {
            return;
        };
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    !document.undo.past.is_empty(),
                    egui::Button::new(crate::localization::text("Undo")),
                )
                .clicked()
            {
                self.send(UiIntent::Undo(document.id.clone()));
            }
            if ui
                .add_enabled(
                    !document.undo.future.is_empty(),
                    egui::Button::new(crate::localization::text("Redo")),
                )
                .clicked()
            {
                self.send(UiIntent::Redo(document.id.clone()));
            }
            if ui.button(crate::localization::text("Capture A")).clicked() {
                self.compare_capture(ComparisonSlot::A);
            }
            if ui.button(crate::localization::text("Capture B")).clicked() {
                self.compare_capture(ComparisonSlot::B);
            }
            let comparison = self
                .controller
                .state()
                .comparisons
                .get(&document.id)
                .cloned()
                .unwrap_or_default();
            for slot in [ComparisonSlot::A, ComparisonSlot::B] {
                if ui
                    .add_enabled(
                        comparison.snapshots.contains_key(&slot),
                        egui::Button::new(std::format!("Recall {slot:?}")),
                    )
                    .clicked()
                {
                    self.compare_recall(slot);
                }
            }
        });
    }
}
fn connection_label(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::Offline | ConnectionState::Disconnected => {
            crate::localization::text("Offline")
        }
        ConnectionState::Ready => crate::localization::text("Connected"),
        ConnectionState::Connecting
        | ConnectionState::Identifying
        | ConnectionState::Synchronizing => crate::localization::text("Connecting…"),
        ConnectionState::Discovering => crate::localization::text("Finding MIDI ports…"),
        ConnectionState::Recovering => crate::localization::text("Recovering connection…"),
        ConnectionState::Unsupported => crate::localization::text("Unsupported device"),
    }
}
pub(super) fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(8.0);
    ui.label(
        RichText::new(crate::localization::text(title))
            .size(17.0)
            .strong(),
    );
    ui.add_space(3.0);
}
