use crate::{StudioApp, theme::Palette};
use eframe::egui::{self, RichText};
use gp200_domain::*;

impl StudioApp {
    pub(crate) fn draft_recovery_banner(&mut self, ui: &mut egui::Ui) {
        if self.recovery_ui.found.is_none() && self.recovery_ui.error.is_none() {
            return;
        }
        let palette = Palette::for_context(ui.ctx());
        egui::Panel::top("draft-recovery-banner")
            .frame(egui::Frame::new().fill(palette.surface).inner_margin(egui::Margin::symmetric(18, 8)))
            .show(ui, |ui| {
                if let Some(error) = &self.recovery_ui.error { ui.colored_label(palette.warning, crate::localization::message(error)); }
                if let Some(hash) = &self.recovery_ui.visible_uncertain_hash {
                    ui.small(crate::localization::text("New snapshot read back; crash durability is unconfirmed")).on_hover_text(std::format!("Visible journal SHA-256: {hash}"));
                }
                if let Some(found) = &self.recovery_ui.found {
                    let count = found.session.documents.len();
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(std::format!("Previous session: {count} open document{}", if count == 1 { "" } else { "s" })).strong());
                        ui.label(crate::localization::text("Review and reopen your work offline."));
                        if ui.button(crate::localization::text("Review recovery")).clicked() { self.recovery_ui.review_open = true; }
                    });
                    ui.small(crate::localization::text("Autosaving this session waits until you recover or discard the previous session."));
                }
            });
    }
    pub(crate) fn draft_recovery_dialog(&mut self, ctx: &egui::Context) {
        if !self.recovery_ui.review_open {
            return;
        }
        let Some(found) = self.recovery_ui.found.clone() else {
            return;
        };
        let palette = Palette::for_context(ctx);
        let mut open = true;
        let mut recover = false;
        let mut discard = false;
        let mut close_review = false;
        egui::Window::new(crate::localization::text("Recover previous session"))
            .open(&mut open).collapsible(false).resizable(true)
            .default_width(600.0).default_height(390.0)
            .show(ctx, |ui| {
                ui.label(RichText::new(crate::localization::text("Your edits are kept locally")).size(19.0).strong());
                ui.label(crate::localization::text("Recover opens new offline documents with their exact edits, history and A/B captures. Read your pedal manually before deciding what to apply."));
                ui.add_space(8.0);
                if let Some(id) = &self.recovery_ui.startup_document {
                    let pristine = self.controller.can_replace_pristine_offline(id);
                    ui.add_enabled(pristine, egui::Checkbox::new(&mut self.recovery_ui.replace_startup, crate::localization::text("Replace the untouched offline startup capture")));
                    if !pristine {
                        self.recovery_ui.replace_startup = false;
                        ui.small(crate::localization::text("The startup document contains work and will be kept."));
                    }
                }
                if !found.session.session_uncertainty.is_empty() {
                    ui.colored_label(palette.warning, std::format!("{} uncertain session operation(s)", found.session.session_uncertainty.len()));
                    egui::CollapsingHeader::new(crate::localization::text("Show retained session uncertainty")).show(ui, |ui| {
                        for item in &found.session.session_uncertainty { ui.label(crate::localization::message(item)); }
                    });
                }
                egui::ScrollArea::vertical().max_height(230.0).show(ui, |ui| {
                    for document in &found.session.documents {
                        ui.group(|ui| {
                            ui.label(RichText::new(document.draft.view.display_name()).strong());
                            let dirty = document.draft != PresetDraft::from_snapshot(&document.baseline);
                            ui.small(std::format!("{} · {} · {} undo / {} redo steps · A {} / B {}",
                                if document.original_target == EditTarget::DeviceBuffer { crate::localization::text("Pedal draft, reopens offline") } else { crate::localization::text("Offline document") },
                                if dirty { crate::localization::text("Local changes") } else { crate::localization::text("Baseline unchanged") },
                                document.undo.past.len(), document.undo.future.len(),
                                if document.comparison.a.is_some() { crate::localization::text("kept") } else { crate::localization::text("empty") },
                                if document.comparison.b.is_some() { crate::localization::text("kept") } else { crate::localization::text("empty") }));
                            if document.undo.gesture.is_some() { ui.small(crate::localization::text("An interrupted drag becomes one complete undo step.")); }
                            if !document.uncertainty.is_empty() {
                                ui.colored_label(palette.warning, std::format!("{} uncertain device operation(s)", document.uncertainty.len()));
                                egui::CollapsingHeader::new(crate::localization::text("Show retained uncertainty")).id_salt(&document.original_id).show(ui, |ui| {
                                    for item in &document.uncertainty { ui.label(crate::localization::message(&item.description)); }
                                });
                            }
                        });
                    }
                });
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    let response = ui.add_enabled(!self.recovery_ui.busy, egui::Button::new(crate::localization::text("Recover all offline")));
                    #[cfg(test)] { self.recovery_ui.recover_rect = Some(response.rect); }
                    if response.clicked() { recover = true; }
                    if ui.add_enabled(!self.recovery_ui.busy, egui::Button::new(crate::localization::text("Discard previous session"))).clicked() { discard = true; }
                    if ui.button(crate::localization::text("Keep for later")).clicked() { close_review = true; }
                });
                ui.small(crate::localization::text("Discard removes only the reviewed recovery file. Open documents and the pedal stay as they are."));
                egui::CollapsingHeader::new(crate::localization::text("Recovery receipt")).show(ui, |ui| {
                    ui.small(std::format!("App {} · schema {}", found.session.app_version, found.session.schema_version));
                    ui.label(egui::RichText::new(&found.sha256).monospace().small());
                    if ui.small_button(crate::localization::text("Copy journal hash")).clicked() { ui.ctx().copy_text(found.sha256.clone()); }
                });
            });
        self.recovery_ui.review_open = open && !close_review;
        if recover {
            self.recover_previous_offline();
        }
        if discard {
            self.discard_previous_recovery();
        }
    }
    pub(crate) fn observed_pedal_change(&mut self, ui: &mut egui::Ui) {
        let state = self.controller.state();
        let observed = state
            .device_globals
            .as_ref()
            .filter(|globals| globals.epoch == state.epoch)
            .and_then(|globals| globals.records().runtime.as_ref())
            .and_then(|runtime| runtime.preset_changed());
        let text = match observed {
            Some(true) => crate::localization::text("Last observed pedal change: changed"),
            Some(false) => {
                crate::localization::text("Last observed pedal change: no change reported")
            }
            None => crate::localization::text("Pedal change state has not been read"),
        };
        if state.connection == ConnectionState::Ready {
            ui.horizontal_wrapped(|ui| {
            ui.small(text).on_hover_text(crate::localization::text("The pedal's preset-changed latch is separate from local draft changes and verified persistent Save. A zero latch does not confirm a Save, and this observation can become stale after an edit."));
            if ui.add_enabled(!self.controller.device_operation_pending(), egui::Button::new(crate::localization::text("Read change state")).small()).clicked() {
                self.send(UiIntent::RefreshDeviceGlobals);
            }
        });
        }
        if let Some(id) = self.controller.active_document_id()
            && let Some(uncertainty) = self.controller.state().recovered_uncertainty.get(id)
            && !uncertainty.is_empty()
        {
            ui.small(std::format!("Recovered offline with {} uncertain prior operation(s). Read the pedal manually to reconcile.", uncertainty.len()));
        }
        if !self
            .controller
            .state()
            .recovered_session_uncertainty
            .is_empty()
        {
            ui.small(std::format!("{} prior session operation(s) have unknown results. Read the pedal manually to reconcile.", self.controller.state().recovered_session_uncertainty.len()));
        }
    }
}
