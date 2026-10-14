use super::section;
use crate::{StudioApp, theme::Palette};
use eframe::egui::{self, RichText};
use gp200_catalog::{ModelDefinition, ParameterDefinition};
use gp200_domain::*;
impl StudioApp {
    pub(super) fn editor(&mut self, ui: &mut egui::Ui) {
        if self.qa_patch_inspector {
            self.patch_inspector(ui);
            self.history_toolbar(ui);
            return;
        }
        let palette = Palette::for_context(ui.ctx());
        let Some(document) = self.controller.active_document().cloned() else {
            ui.heading(crate::localization::text("Open a preset"));
            ui.label(crate::localization::text(
                "Choose a library version or read the pedal edit buffer.",
            ));
            return;
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(document.draft.view.display_name())
                    .size(27.0)
                    .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(crate::localization::text("Capture to library"))
                    .clicked()
                {
                    self.capture_library();
                }
                if ui
                    .button(crate::localization::text("Save to pedal…"))
                    .clicked()
                {
                    self.save_review = true;
                }
            });
        });
        if document.target != EditTarget::DeviceBuffer
            && self.controller.state().confirmed_buffer.is_some()
        {
            ui.horizontal_wrapped(|ui| {
                if ui
                    .button(crate::localization::text("Open pedal buffer"))
                    .clicked()
                {
                    self.request_switch(crate::PendingSwitch::Document(
                        gp200_app::device_document_id(),
                    ));
                }
                if ui
                    .button(crate::localization::text("Apply draft to pedal buffer"))
                    .clicked()
                {
                    let effects = self.controller.stage_document_in_buffer(&document.id);
                    self.services.enqueue(effects);
                }
            });
        }
        let status = self.controller.state().document_status(&document.id);
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(match document.target {
                    EditTarget::DeviceBuffer => crate::localization::text("Pedal edit buffer"),
                    EditTarget::Offline(_) => crate::localization::text("Offline draft"),
                })
                .color(palette.muted),
            );
            if let Some(status) = status {
                if status.changed_since_baseline {
                    ui.colored_label(palette.warning, crate::localization::text("Local changes"));
                } else {
                    ui.label(crate::localization::text("Unchanged capture"));
                }
                if status.pending > 0 {
                    ui.label(std::format!("{} pending", status.pending));
                }
                if status.conflicts > 0 {
                    ui.colored_label(
                        palette.warning,
                        std::format!("{} conflicts", status.conflicts),
                    );
                }
                if status.verified_in_buffer {
                    ui.label(crate::localization::text("Readback verified in buffer"));
                }
                if let Some(persistence) = status.persistence {
                    ui.label(match persistence {
                        gp200_app::BufferPersistence::Unknown => {
                            crate::localization::text("Stored state not compared").into()
                        }
                        gp200_app::BufferPersistence::Selected {
                            slot,
                            stored_matches_buffer,
                            ..
                        } => match stored_matches_buffer {
                            Some(true) => {
                                std::format!("Loaded slot {slot}; matches stored settings")
                            }
                            Some(false) => {
                                std::format!(
                                    "Loaded slot {slot}; pedal controls changed runtime values"
                                )
                            }
                            None => {
                                std::format!("Loaded slot {slot}; stored comparison unavailable")
                            }
                        },
                        gp200_app::BufferPersistence::Unsaved => {
                            crate::localization::text("Buffer changes are not saved to a slot")
                                .into()
                        }
                        gp200_app::BufferPersistence::Saving { slot, .. } => {
                            std::format!("Saving slot {slot}…")
                        }
                        gp200_app::BufferPersistence::Saved { slot, .. } => {
                            std::format!("Stored slot {slot} verified")
                        }
                        gp200_app::BufferPersistence::Indeterminate { slot, .. } => {
                            std::format!("Save to slot {slot} needs reconciliation")
                        }
                    });
                }
            }
        });
        ui.add_space(15.0);
        self.signal_chain(ui);
        ui.add_space(18.0);
        if ui.available_width() >= 800.0 {
            let total_width = ui.available_width();
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(total_width - 260.0, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| self.module_editor(ui),
                );
                ui.add_space(14.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(230.0, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| self.patch_inspector(ui),
                );
            });
        } else {
            self.module_editor(ui);
            ui.add_space(12.0);
            self.patch_inspector(ui);
        }
        ui.add_space(14.0);
        self.history_toolbar(ui);
        if !document.conflicts.is_empty() {
            section(ui, crate::localization::text("Resolve pedal changes"));
            for conflict in document.conflicts {
                ui.label(crate::localization::message(&conflict.description));
                if self.controller.external_conflict(conflict.intent) {
                    ui.small(crate::localization::text("Both choices preserve your previous work as a local document and send no edit."));
                }
                let review_ready = !self.controller.external_conflict(conflict.intent)
                    || self.controller.external_conflict_ready(conflict.intent);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            review_ready,
                            egui::Button::new(crate::localization::text("Accept pedal")),
                        )
                        .clicked()
                    {
                        self.send(UiIntent::ResolveConflict(ConflictResolution::AcceptDevice(
                            conflict.intent,
                        )));
                    }
                    if ui
                        .add_enabled(
                            review_ready,
                            egui::Button::new(crate::localization::text("Keep local draft")),
                        )
                        .clicked()
                    {
                        self.send(UiIntent::ResolveConflict(ConflictResolution::KeepLocal(
                            conflict.intent,
                        )));
                    }
                });
            }
        }
    }
    pub(super) fn signal_chain(&mut self, ui: &mut egui::Ui) {
        let Some(document) = self.controller.active_document().cloned() else {
            return;
        };
        let palette = Palette::for_context(ui.ctx());
        let chain = document
            .draft
            .view
            .chain
            .value()
            .cloned()
            .unwrap_or_else(|| ModuleId::all().collect());
        egui::ScrollArea::horizontal()
            .id_salt("signal-chain")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for module_id in chain {
                        let Some(module) = document.draft.view.module(module_id) else {
                            continue;
                        };
                        let model_name = self
                            .controller
                            .catalog()
                            .model_by_id(module_id.get(), module.model.0)
                            .map_or(crate::localization::text("Unknown"), |model| model.name.as_str());
                        let selected = self.selected_module == module_id;
                        let text = format!(
                            "{}\n{}\n{}",
                            module_id.name(),
                            model_name,
                            match module.state_raw {
                                0 => crate::localization::text(crate::localization::text("Bypassed")),
                                1 => crate::localization::text(crate::localization::text("On")),
                                _ => crate::localization::text(crate::localization::text("Unknown state")),
                            }
                        );
                        let button =
                            egui::Button::new(RichText::new(text).size(12.0).color(palette.ink))
                                .selected(selected)
                                .fill(if selected {
                                    palette.selected
                                } else {
                                    palette.surface
                                })
                                .stroke(egui::Stroke::new(
                                    if selected { 2.0 } else { 1.0 },
                                    if selected {
                                        palette.accent
                                    } else {
                                        palette.rule
                                    },
                                ))
                                .min_size(egui::vec2(72.0, 75.0));
                        if ui.add(button).clicked() {
                            self.selected_module = module_id;
                        }
                    }
                });
            });
    }
    pub(super) fn module_editor(&mut self, ui: &mut egui::Ui) {
        let Some(document) = self.controller.active_document().cloned() else {
            return;
        };
        let Some(module) = document.draft.view.module(self.selected_module).cloned() else {
            return;
        };
        let definition = self
            .controller
            .catalog()
            .model_by_id(module.module.get(), module.model.0)
            .cloned();
        let palette = Palette::for_context(ui.ctx());
        egui::Frame::new().fill(palette.surface).stroke(egui::Stroke::new(1.0,palette.rule)).inner_margin(18).show(ui,|ui| {
            ui.set_min_width((ui.available_width()-2.0).max(280.0));
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(module.module.name()).color(palette.muted));
                ui.label(RichText::new(definition.as_ref().map_or(crate::localization::text("Unknown model"),|model| model.name.as_str())).size(22.0).strong());
                if ui.button(crate::localization::text("Choose model…")).clicked() { self.model_picker = true; self.catalog_search.clear(); }
            });
            let mut enabled = module.state_raw == 1;
            if ui.add_enabled(module.state_raw <= 1,egui::Checkbox::new(&mut enabled,crate::localization::text("Effect enabled"))).changed() { self.edit(EditPatch::SetEnabled { module: module.module,enabled }); }
            if let Some(definition) = &definition {
                if let Some(description) = definition.description() { ui.label(RichText::new(crate::localization::description(&definition.module, definition.code, description)).small().color(palette.muted)); }
                ui.add_space(16.0);
                for parameter in definition.parameters_in_display_order() { self.parameter(ui,&document.id,&module,definition,parameter,true); }
            } else { ui.label(crate::localization::text("This model has no catalog definition. Its original parameter bytes are retained.")); }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let chain = document.draft.view.chain.value().cloned().unwrap_or_else(|| ModuleId::all().collect());
                let index = chain.iter().position(|candidate| *candidate == module.module).unwrap_or(0);
                if ui.add_enabled(index > 0,egui::Button::new(crate::localization::text("Move earlier"))).clicked() { self.edit(EditPatch::MoveModule { module:module.module,before:chain.get(index.saturating_sub(1)).copied() }); }
                if ui.add_enabled(index+1 < chain.len(),egui::Button::new(crate::localization::text("Move later"))).clicked() { self.edit(EditPatch::MoveModule { module:module.module,before:chain.get(index+2).copied() }); }
            });
        });
    }
    fn parameter(
        &mut self,
        ui: &mut egui::Ui,
        document: &DocumentId,
        module: &ModuleSnapshot,
        model: &ModelDefinition,
        parameter: &ParameterDefinition,
        allowed: bool,
    ) {
        let Some(editor_document) = self.controller.state().documents.get(document).cloned() else {
            return;
        };
        let state = self.controller.state();
        let confirmed = state.confirmed_buffer.as_ref();
        let wire = if editor_document.target == EditTarget::DeviceBuffer {
            confirmed.and_then(|snapshot| snapshot.context)
        } else {
            editor_document.baseline.context
        };
        let source = if editor_document.target == EditTarget::DeviceBuffer {
            confirmed.and_then(|snapshot| snapshot.view.module(module.module))
        } else {
            Some(module)
        };
        self.control_state
            .observe(&editor_document, state.epoch, wire, module, model, source);
        let blocked = super::controls::blocked_reason(
            state,
            &editor_document.target,
            module,
            model,
            parameter,
            allowed,
        );
        let result = super::controls::render(
            ui,
            &mut self.control_state,
            module,
            model,
            parameter,
            blocked.as_deref(),
        );
        let Ok(id) = ParamId::new(parameter.parameter_id) else {
            return;
        };
        let key = GestureKey {
            module: module.module,
            model: module.model,
            parameter: id,
        };
        if result.started {
            self.send(UiIntent::BeginGesture {
                document: document.clone(),
                key: key.clone(),
            });
        }
        if let Some(patch) = result.patch {
            self.send(UiIntent::ApplyEdit {
                document: document.clone(),
                patch,
            });
        }
        if result.stopped {
            self.send(UiIntent::EndGesture {
                document: document.clone(),
                key,
            });
        }
        if let Some(error) = result.error {
            self.services.error(error);
        }
    }
    fn patch_inspector(&mut self, ui: &mut egui::Ui) {
        let Some(document) = self.controller.active_document().cloned() else {
            return;
        };
        let state = self.controller.state();
        let wire = if document.target == EditTarget::DeviceBuffer {
            state
                .confirmed_buffer
                .as_ref()
                .and_then(|snapshot| snapshot.context)
        } else {
            document.baseline.context
        };
        let numeric_context = super::controls::EditContext::new(&document, state.epoch, wire);
        let palette = Palette::for_context(ui.ctx());
        section(ui, crate::localization::text("Patch details"));
        let mut name = document.draft.view.display_name().to_owned();
        ui.label(crate::localization::text("Name"));
        if ui
            .add(
                egui::TextEdit::singleline(&mut name)
                    .char_limit(16)
                    .desired_width(f32::INFINITY),
            )
            .changed()
        {
            self.edit(EditPatch::SetMetadata(MetadataPatch {
                name: Some(name),
                ..Default::default()
            }));
        }
        let mut tempo = document.draft.view.tempo.value().copied().unwrap_or(120);
        let mut level = document.draft.view.level.value().copied().unwrap_or(50);
        ui.horizontal(|ui| {
            ui.label(crate::localization::text("Tempo"));
            if super::controls::committed_numeric(ui, Some(&numeric_context), |ui| {
                ui.add(
                    egui::DragValue::new(&mut tempo)
                        .update_while_editing(false)
                        .range(30..=300)
                        .suffix(crate::localization::text(" BPM")),
                )
            })
            .changed()
                && tempo != document.draft.view.tempo.value().copied().unwrap_or(120)
            {
                self.edit(EditPatch::SetMetadata(MetadataPatch {
                    tempo: Some(tempo),
                    ..Default::default()
                }));
            }
        });
        ui.horizontal(|ui| {
            ui.label(crate::localization::text("Patch level"));
            if super::controls::committed_numeric(ui, Some(&numeric_context), |ui| {
                ui.add(
                    egui::DragValue::new(&mut level)
                        .update_while_editing(false)
                        .range(0..=100),
                )
            })
            .changed()
                && level != document.draft.view.level.value().copied().unwrap_or(50)
            {
                self.edit(EditPatch::SetMetadata(MetadataPatch {
                    level: Some(level),
                    ..Default::default()
                }));
            }
        });
        if let Some(pan) = document.draft.metadata.native_pan {
            ui.label(crate::localization::text("Patch Pan"));
            let patch = MetadataPatch {
                native_pan: Some(pan),
                ..Default::default()
            };
            let hardware = document.target == EditTarget::DeviceBuffer
                && self
                    .controller
                    .state()
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.mode == BackendMode::Hardware);
            let baseline = &document.baseline.view.metadata;
            let allowed = !hardware || patch.hardware_profile(baseline).is_ok();
            if pan.validate().is_ok() {
                let mut value = i16::from(pan.raw());
                let range = if hardware { 0..=1 } else { -100..=100 };
                if super::controls::committed_numeric(ui, Some(&numeric_context), |ui| {
                    ui.add_enabled(
                        allowed,
                        egui::Slider::new(&mut value, range).update_while_editing(false),
                    )
                })
                .changed()
                    && value != i16::from(pan.raw())
                {
                    self.edit(EditPatch::SetMetadata(MetadataPatch {
                        native_pan: PatchPan::new(value).ok(),
                        ..Default::default()
                    }));
                }
                ui.small(if hardware {
                    crate::localization::text("Hardware test currently covers 0 and 1")
                } else {
                    crate::localization::text("Native range −100..100 · Step 1")
                });
            } else {
                ui.small(std::format!(
                    "Retained value {} (outside native slider)",
                    pan.raw()
                ));
            }
            if !allowed {
                ui.small(crate::localization::text(
                    "This Pan baseline has no verified hardware edit profile.",
                ));
            }
        }
        if let Some(style) = document.draft.metadata.native_style {
            let hardware = document.target == EditTarget::DeviceBuffer
                && self
                    .controller
                    .state()
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.mode == BackendMode::Hardware);
            let globals = self
                .controller
                .state()
                .device_globals
                .as_ref()
                .map(|state| state.records().raw());
            let mut selected = style;
            ui.label(crate::localization::text("Pedal style"));
            egui::ComboBox::from_id_salt("native-patch-style")
                .selected_text(crate::localization::style(style, globals))
                .width(ui.available_width().min(210.0))
                .show_ui(ui, |ui| {
                    for id in 0..20 {
                        let value = NativeStyleId::new(id).expect("bounded style");
                        let patch = MetadataPatch {
                            native_style: Some(value),
                            ..Default::default()
                        };
                        let allowed = !hardware
                            || patch
                                .hardware_profile(&document.baseline.view.metadata)
                                .is_ok();
                        ui.add_enabled_ui(allowed, |ui| {
                            ui.selectable_value(
                                &mut selected,
                                value,
                                crate::localization::style(value, globals),
                            );
                        });
                    }
                });
            if selected != style {
                self.edit(EditPatch::SetMetadata(MetadataPatch {
                    native_style: Some(selected),
                    ..Default::default()
                }));
            }
            if hardware {
                ui.small(crate::localization::text(
                    "Hardware editing currently covers Rock and “-”.",
                ));
            }
        }
        ui.collapsing(crate::localization::text("FX loop levels & mode"), |ui| {
            let hardware = document.target == EditTarget::DeviceBuffer
                && self
                    .controller
                    .state()
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.mode == BackendMode::Hardware);
            for (label, original, send) in [
                (
                    crate::localization::text("Send level"),
                    document.draft.metadata.fx_send_level,
                    true,
                ),
                (
                    crate::localization::text("Return level"),
                    document.draft.metadata.fx_return_level,
                    false,
                ),
            ] {
                if let Some(mut value) = original {
                    if value <= 100 {
                        if ui
                            .add_enabled(
                                !hardware,
                                egui::Slider::new(&mut value, 0..=100).text(label),
                            )
                            .changed()
                        {
                            self.edit(EditPatch::SetMetadata(if send {
                                MetadataPatch {
                                    fx_send_level: Some(value),
                                    ..Default::default()
                                }
                            } else {
                                MetadataPatch {
                                    fx_return_level: Some(value),
                                    ..Default::default()
                                }
                            }));
                        }
                    } else {
                        ui.small(std::format!("{label}: retained value {value}"));
                    }
                }
            }
            if let Some(mode) = document.draft.metadata.fx_loop_mode {
                let mut selected = mode;
                ui.add_enabled_ui(!hardware, |ui| {
                    egui::ComboBox::from_id_salt("patch-fx-mode")
                        .selected_text(crate::localization::text(mode.label()))
                        .show_ui(ui, |ui| {
                            for raw in 0..=1 {
                                let value = PatchFxLoopMode::new(raw).expect("bounded mode");
                                ui.selectable_value(
                                    &mut selected,
                                    value,
                                    crate::localization::text(value.label()),
                                );
                            }
                        });
                });
                if selected != mode {
                    self.edit(EditPatch::SetMetadata(MetadataPatch {
                        fx_loop_mode: Some(selected),
                        ..Default::default()
                    }));
                }
                if mode.raw() > 1 {
                    ui.small(std::format!("Retained mode byte {}", mode.raw()));
                }
            }
            if hardware {
                ui.small(crate::localization::text(
                    "Read-only until reversible hardware tests cover these controls.",
                ));
            }
        });
        ui.collapsing(crate::localization::text("Pedal author & note"), |ui| {
            if let Some(record) = document.baseline.raw.records().iter().find(|record| record.command == 2 && record.body.len() == 88) {
                for (label, bytes) in [(crate::localization::text("Author"), &record.body[32..48]), (crate::localization::text("Note"), &record.body[48..88])] {
                    let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
                    ui.label(std::format!("{label}: {}", if end == 0 { "—".into() } else { String::from_utf8_lossy(&bytes[..end]) }));
                }
                ui.small(crate::localization::text("Original bytes are retained. Device text editing awaits a verified transfer profile."));
            }
        });
        ui.collapsing(
            crate::localization::text("Local author & style tag"),
            |ui| {
                for (label, original, author) in [
                    (
                        crate::localization::text("Local author"),
                        document.draft.metadata.author.clone(),
                        true,
                    ),
                    (
                        crate::localization::text("Library style tag"),
                        document.draft.metadata.style.clone(),
                        false,
                    ),
                ] {
                    let mut value = original.unwrap_or_default();
                    ui.label(label);
                    if ui
                        .add(egui::TextEdit::singleline(&mut value).desired_width(f32::INFINITY))
                        .changed()
                    {
                        self.edit(EditPatch::SetMetadata(if author {
                            MetadataPatch {
                                author: Some(value),
                                ..Default::default()
                            }
                        } else {
                            MetadataPatch {
                                style: Some(value),
                                ..Default::default()
                            }
                        }));
                    }
                }
            },
        );
        let mut notes = document.draft.metadata.notes.clone().unwrap_or_default();
        ui.label(crate::localization::text("Local notes"));
        if ui
            .add(
                egui::TextEdit::multiline(&mut notes)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY),
            )
            .changed()
        {
            self.edit(EditPatch::SetMetadata(MetadataPatch {
                notes: Some(notes),
                ..Default::default()
            }));
        }
        ui.label(
            RichText::new(crate::localization::text(
                "Notes are stored in your local library.",
            ))
            .small()
            .color(palette.muted),
        );
        section(ui, crate::localization::text("Assignments"));
        let mappings = &document.draft.view.patch_assignments;
        ui.label(std::format!(
            "{} expression slots\n{} CTRL mappings\n{} quick controls",
            mappings.expressions.len(),
            mappings.ctrls.len(),
            mappings.quicks.len()
        ));
        if let Some(routing) = &mappings.routing {
            ui.small(std::format!(
                "FX loop positions: Send {}, Return {}",
                routing.record.send_boundary,
                routing.record.return_boundary
            ));
        }
        if !mappings.diagnostics.is_empty() {
            ui.colored_label(
                palette.warning,
                std::format!(
                    "{} retained mapping diagnostics",
                    mappings.diagnostics.len()
                ),
            );
        }
        if ui
            .button(crate::localization::text("Edit assignments"))
            .clicked()
        {
            self.send(UiIntent::Navigate(ViewId::Assignments));
        }
        section(ui, crate::localization::text("Source"));
        ui.small(format!("{:?}", document.baseline.source));
        if let Some(context) = document.baseline.context {
            ui.small(std::format!("Captured internal slot {}", context.selector));
        }
    }
    pub(super) fn stage_parameter(&mut self, ui: &mut egui::Ui, module_id: u8, param_id: u8) {
        let Some(document) = self.controller.active_document().cloned() else {
            return;
        };
        let Ok(module_id) = ModuleId::new(module_id) else {
            return;
        };
        let Some(module) = document.draft.view.module(module_id).cloned() else {
            return;
        };
        let Some(model) = self
            .controller
            .catalog()
            .model_by_id(module_id.get(), module.model.0)
            .cloned()
        else {
            return;
        };
        let Some(parameter) = model.parameter(param_id) else {
            return;
        };
        self.parameter(
            ui,
            &document.id,
            &module,
            &model,
            parameter,
            !self.performance_locked,
        );
    }
}
