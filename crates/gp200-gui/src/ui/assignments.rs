//! Native assignment projection and explicit, lossless staged edits.
use super::section;
use crate::{StudioApp, theme::Palette};
use eframe::egui::{self, RichText};
use gp200_app::AppState;
use gp200_catalog::Catalog;
use gp200_domain::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AssignmentTab {
    #[default]
    Expression,
    Ctrl,
    Quick,
    Routing,
}
impl AssignmentTab {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "expression" => Some(Self::Expression),
            "ctrl" => Some(Self::Ctrl),
            "quick" => Some(Self::Quick),
            "routing" => Some(Self::Routing),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum TargetChoice {
    Off,
    Parameter(ParameterAssignmentTarget),
    PatchVolume,
    PatchTempo,
    Unresolved { module: u8, parameter: u16 },
}
impl TargetChoice {
    fn from_reference(reference: ResolvedAssignmentReference, catalog: &Catalog) -> Self {
        match reference {
            ResolvedAssignmentReference::Off => Self::Off,
            ResolvedAssignmentReference::PatchVolume => Self::PatchVolume,
            ResolvedAssignmentReference::PatchTempo => Self::PatchTempo,
            ResolvedAssignmentReference::Parameter(target)
                if catalog
                    .model_by_id(target.module.get(), target.expected_model.0)
                    .and_then(|model| model.parameter(target.parameter.get()))
                    .is_some() =>
            {
                Self::Parameter(target)
            }
            ResolvedAssignmentReference::Parameter(target) => Self::Unresolved {
                module: target.module.get(),
                parameter: u16::from(target.parameter.get()),
            },
            ResolvedAssignmentReference::Unresolved {
                module_raw,
                parameter_raw,
            } => Self::Unresolved {
                module: module_raw,
                parameter: parameter_raw,
            },
        }
    }
}
#[derive(Clone, PartialEq, Eq)]
struct SelectionContext {
    document: DocumentId,
    revision: Revision,
    epoch: SessionEpoch,
    assignments: PatchAssignments,
    models: Vec<(ModuleId, ModelCode)>,
    tab: AssignmentTab,
    expression: ExpressionSlotId,
    ctrl: u8,
    quick: u8,
}
pub(crate) struct AssignmentState {
    pub(crate) tab: AssignmentTab,
    expression: ExpressionSlotId,
    ctrl: u8,
    quick: u8,
    context: Option<SelectionContext>,
    target: TargetChoice,
    minimum: String,
    maximum: String,
    modules: Vec<ModuleId>,
    send_boundary: u8,
    return_boundary: u8,
}
impl Default for AssignmentState {
    fn default() -> Self {
        Self {
            tab: AssignmentTab::Expression,
            expression: ExpressionSlotId::new(0, 0).unwrap(),
            ctrl: 0,
            quick: 0,
            context: None,
            target: TargetChoice::Off,
            minimum: String::new(),
            maximum: String::new(),
            modules: Vec::new(),
            send_boundary: 0,
            return_boundary: 0,
        }
    }
}
impl AssignmentState {
    fn observe(&mut self, document: &EditorDocument, epoch: SessionEpoch, catalog: &Catalog) {
        let context = SelectionContext {
            document: document.id.clone(),
            revision: document.local_revision,
            epoch,
            assignments: document.draft.view.patch_assignments.clone(),
            models: document
                .draft
                .view
                .modules
                .iter()
                .map(|module| (module.module, module.model))
                .collect(),
            tab: self.tab,
            expression: self.expression,
            ctrl: self.ctrl,
            quick: self.quick,
        };
        if self.context.as_ref() == Some(&context) {
            return;
        }
        self.context = Some(context);
        let assignments = &document.draft.view.patch_assignments;
        let modules = &document.draft.view.modules;
        self.target = TargetChoice::Off;
        self.minimum.clear();
        self.maximum.clear();
        match self.tab {
            AssignmentTab::Expression => {
                if let Some(entry) = unique_expression(assignments, self.expression) {
                    self.target = TargetChoice::from_reference(
                        PatchAssignments::resolve_expression(entry, modules),
                        catalog,
                    );
                    self.minimum = entry.minimum.as_f32().to_string();
                    self.maximum = entry.maximum.as_f32().to_string();
                }
            }
            AssignmentTab::Quick => {
                if let Some(entry) = unique_quick(assignments, self.quick) {
                    self.target = TargetChoice::from_reference(
                        PatchAssignments::resolve_quick(entry, modules),
                        catalog,
                    );
                }
            }
            AssignmentTab::Ctrl => {
                self.modules = unique_ctrl(assignments, self.ctrl)
                    .map(|entry| {
                        ModuleId::all()
                            .filter(|module| entry.module_mask_raw & (1 << module.get()) != 0)
                            .collect()
                    })
                    .unwrap_or_default();
            }
            AssignmentTab::Routing => {
                if let Some(routing) = &assignments.routing {
                    self.send_boundary = routing.record.send_boundary;
                    self.return_boundary = routing.record.return_boundary;
                }
            }
        }
    }
    fn select_target(&mut self, target: TargetChoice, catalog: &Catalog) {
        if self.target == target {
            return;
        }
        match &target {
            TargetChoice::Parameter(target) => {
                if let Some((minimum, maximum, _)) = target_bounds(catalog, target) {
                    self.minimum = minimum.to_string();
                    self.maximum = maximum.to_string();
                }
            }
            TargetChoice::Off => {
                self.minimum = "0".into();
                self.maximum = "0".into();
            }
            _ => {}
        }
        self.target = target;
    }
}

impl StudioApp {
    pub(super) fn assignments(&mut self, ui: &mut egui::Ui) {
        ui.heading(crate::localization::text("Patch assignments"));
        let Some(document) = self.controller.active_document().cloned() else {
            ui.label(crate::localization::text(
                "Open a preset to inspect its expression, CTRL and quick mappings.",
            ));
            return;
        };
        if let Some(edit) = render(
            ui,
            &mut self.assignment_state,
            &document,
            self.controller.catalog(),
            self.controller.state(),
        ) {
            self.edit(EditPatch::SetPatchAssignment(edit));
        }
        self.history_toolbar(ui);
    }
}

#[derive(Default)]
struct WidgetOutput {
    edit: Option<AssignmentEdit>,
    apply_rect: Option<egui::Rect>,
    module_rects: Vec<(ModuleId, egui::Rect)>,
    slot_rects: Vec<(ExpressionSlotId, egui::Rect)>,
}
fn render(
    ui: &mut egui::Ui,
    state: &mut AssignmentState,
    document: &EditorDocument,
    catalog: &Catalog,
    app: &AppState,
) -> Option<AssignmentEdit> {
    render_widgets(ui, state, document, catalog, app).edit
}
fn render_widgets(
    ui: &mut egui::Ui,
    state: &mut AssignmentState,
    document: &EditorDocument,
    catalog: &Catalog,
    app: &AppState,
) -> WidgetOutput {
    let previous_spacing = ui.spacing().clone();
    if ui.ctx().content_rect().height() < 700.0 {
        let spacing = ui.spacing_mut();
        spacing.item_spacing.y = 5.0;
        spacing.interact_size.y = 25.0;
        spacing.button_padding.y = 3.0;
    }
    let palette = Palette::for_context(ui.ctx());
    let mut output = WidgetOutput::default();
    ui.label(
        RichText::new(format!(
            "{} · {}",
            document.draft.view.display_name(),
            editing_context(app, document)
        ))
        .color(palette.muted),
    );
    ui.horizontal_wrapped(|ui| {
        for (tab, label) in [
            (
                AssignmentTab::Expression,
                crate::localization::text("Expression"),
            ),
            (
                AssignmentTab::Ctrl,
                crate::localization::text("CTRL switches"),
            ),
            (
                AssignmentTab::Quick,
                crate::localization::text("Quick controls"),
            ),
            (AssignmentTab::Routing, crate::localization::text("FX loop")),
        ] {
            ui.selectable_value(&mut state.tab, tab, crate::localization::text(label));
        }
    });
    ui.add_space(8.0);
    match state.tab {
        AssignmentTab::Expression => {
            expression_widgets(ui, state, document, catalog, app, &mut output)
        }
        AssignmentTab::Ctrl => ctrl_widgets(ui, state, document, catalog, app, &mut output),
        AssignmentTab::Quick => quick_widgets(ui, state, document, catalog, app, &mut output),
        AssignmentTab::Routing => routing_widgets(ui, state, document, catalog, app, &mut output),
    }
    if !document.draft.view.patch_assignments.diagnostics.is_empty() {
        egui::CollapsingHeader::new(crate::localization::text("Retained mapping diagnostics"))
            .show(ui, |ui| {
                for diagnostic in &document.draft.view.patch_assignments.diagnostics {
                    ui.colored_label(palette.warning, &diagnostic.description);
                }
            });
    }
    *ui.spacing_mut() = previous_spacing;
    output
}

fn expression_widgets(
    ui: &mut egui::Ui,
    state: &mut AssignmentState,
    document: &EditorDocument,
    catalog: &Catalog,
    app: &AppState,
    output: &mut WidgetOutput,
) {
    let assignments = &document.draft.view.patch_assignments;
    ui.columns(3, |columns| {
        for (group, label) in [
            crate::localization::text("EXP1 A"),
            crate::localization::text("EXP1 B"),
            crate::localization::text("EXP2 external"),
        ]
        .into_iter()
        .enumerate()
        {
            let ui = &mut columns[group];
            ui.label(RichText::new(label).strong());
            for position in 0..3 {
                let slot = ExpressionSlotId::new(group as u8, position).unwrap();
                let entry = unique_expression(assignments, slot);
                let label = entry.map_or_else(
                    || crate::localization::text("Missing or ambiguous").into(),
                    |entry| {
                        target_label(
                            catalog,
                            &TargetChoice::from_reference(
                                PatchAssignments::resolve_expression(
                                    entry,
                                    &document.draft.view.modules,
                                ),
                                catalog,
                            ),
                        )
                    },
                );
                let response = ui.add(
                    egui::Button::new(std::format!("{}: {label}", position + 1))
                        .selected(state.expression == slot)
                        .wrap(),
                );
                output.slot_rects.push((slot, response.rect));
                if response.clicked() {
                    state.expression = slot;
                }
            }
        }
    });
    state.observe(document, app.epoch, catalog);
    let Some(entry) = unique_expression(assignments, state.expression) else {
        ui.label(crate::localization::text("This retained expression slot is missing or duplicated. Import/read its original records before editing."));
        return;
    };
    section(
        ui,
        &std::format!(
            "{} / mapping {}",
            [
                crate::localization::text("EXP1 A"),
                crate::localization::text("EXP1 B"),
                crate::localization::text("EXP2 external")
            ][usize::from(state.expression.group())],
            state.expression.slot() + 1
        ),
    );
    target_picker(ui, state, document, catalog, false);
    let original = TargetChoice::from_reference(
        PatchAssignments::resolve_expression(entry, &document.draft.view.modules),
        catalog,
    );
    let parameter = match &state.target {
        TargetChoice::Parameter(target) => Some(target),
        _ => None,
    };
    let bounds = parameter.and_then(|target| target_bounds(catalog, target));
    if let Some((minimum, maximum, _)) = bounds {
        let suffix = parameter
            .and_then(|target| catalog.model_by_id(target.module.get(), target.expected_model.0))
            .and_then(|model| match &state.target {
                TargetChoice::Parameter(target) => model.parameter(target.parameter.get()),
                _ => None,
            })
            .and_then(|p| p.suffix.as_deref())
            .unwrap_or("");
        ui.small(std::format!(
            "Parameter range: {minimum} to {maximum} {suffix}"
        ));
        ui.horizontal_wrapped(|ui| {
            ui.label(crate::localization::text("Heel / minimum"));
            ui.add(egui::TextEdit::singleline(&mut state.minimum).desired_width(105.0));
            ui.label(crate::localization::text("Toe / maximum"));
            ui.add(egui::TextEdit::singleline(&mut state.maximum).desired_width(105.0));
            if ui
                .small_button(crate::localization::text("Full range"))
                .clicked()
            {
                state.minimum = minimum.to_string();
                state.maximum = maximum.to_string();
            }
            if ui
                .small_button(crate::localization::text("Reverse"))
                .clicked()
            {
                std::mem::swap(&mut state.minimum, &mut state.maximum);
            }
        });
        if let (Some(minimum), Some(maximum)) = (
            parse_endpoint(&state.minimum),
            parse_endpoint(&state.maximum),
        ) {
            ui.small(if minimum.as_f32() > maximum.as_f32() {
                crate::localization::text("Reversed: the value decreases from heel to toe.")
            } else {
                crate::localization::text("The value moves from heel to toe using these endpoints.")
            });
        }
    } else if matches!(state.target, TargetChoice::Off) {
        ui.label(crate::localization::text(
            "Off keeps this mapping inactive.",
        ));
        ui.small(std::format!("Retained endpoints: {} to {}", entry.minimum.as_f32(), entry.maximum.as_f32()))
            .on_hover_text(std::format!("Inactive reference module {:#04x}, parameter {}. Minimum bits {:08x}; maximum bits {:08x}. Imported inactive bounds stay unchanged until you explicitly edit the target.", entry.module_raw, entry.parameter_raw, entry.minimum.0, entry.maximum.0));
    } else {
        ui.label(crate::localization::text("The current reference has no recovered numeric endpoint range. Its original bytes are retained."));
    }
    let proposal = expression_edit(state, entry, &original, catalog);
    if matches!(&proposal, Ok(None)) && matches!(&state.target, TargetChoice::Parameter(_)) {
        let inspection = AssignmentEdit::ExpressionRange {
            slot: state.expression,
            minimum: entry.minimum,
            maximum: entry.maximum,
        };
        if let Some(reason) = blocked_reason(app, document, &inspection) {
            ui.colored_label(Palette::for_context(ui.ctx()).warning, reason);
        }
    }
    apply_widget(ui, document, app, proposal, output);
}

fn ctrl_widgets(
    ui: &mut egui::Ui,
    state: &mut AssignmentState,
    document: &EditorDocument,
    catalog: &Catalog,
    app: &AppState,
    output: &mut WidgetOutput,
) {
    let assignments = &document.draft.view.patch_assignments;
    ui.horizontal_wrapped(|ui| {
        for slot in 0..8 {
            ui.selectable_value(&mut state.ctrl, slot, std::format!("CTRL {}", slot + 1));
        }
    });
    state.observe(document, app.epoch, catalog);
    let Some(entry) = unique_ctrl(assignments, state.ctrl) else {
        ui.label(crate::localization::text(
            "The selected CTRL record is missing or duplicated; its membership cannot be changed.",
        ));
        return;
    };
    section(
        ui,
        &std::format!("CTRL {} module membership", state.ctrl + 1),
    );
    ui.label(crate::localization::text(
        "Choose the effect modules included in this patch's CTRL mapping.",
    ));
    ui.horizontal_wrapped(|ui| {
        for module in ModuleId::all() {
            let mut selected = state.modules.contains(&module);
            let response = ui.checkbox(&mut selected, module.name());
            output.module_rects.push((module, response.rect));
            if response.changed() {
                state.modules.retain(|actual| *actual != module);
                if selected {
                    state.modules.push(module);
                }
                state.modules.sort();
            }
        }
    });
    ui.small(std::format!(
        "Retained CTRL state: {}. State activation still needs a separate pedal profile.",
        entry.state_raw
    ));
    if entry.module_mask_raw & !gp200_protocol::CtrlAssignmentRecord::MODULE_MASK != 0 {
        ui.small(std::format!(
            "Unknown membership bits retained: {:08x}",
            entry.module_mask_raw & !gp200_protocol::CtrlAssignmentRecord::MODULE_MASK
        ));
    }
    let unchanged = ModuleId::all()
        .filter(|module| entry.module_mask_raw & (1 << module.get()) != 0)
        .collect::<Vec<_>>()
        == state.modules;
    let proposal = if unchanged {
        Ok(None)
    } else {
        Ok(Some(AssignmentEdit::CtrlModules {
            slot: state.ctrl,
            modules: state.modules.clone(),
        }))
    };
    apply_widget(ui, document, app, proposal, output);
    section(ui, crate::localization::text("All eight patch mappings"));
    for slot in 0..8 {
        if let Some(record) = unique_ctrl(assignments, slot) {
            let modules = ModuleId::all()
                .filter(|module| record.module_mask_raw & (1 << module.get()) != 0)
                .map(|module| module.name())
                .collect::<Vec<_>>();
            ui.label(std::format!(
                "CTRL {}: {}",
                slot + 1,
                if modules.is_empty() {
                    crate::localization::text("No modules").into()
                } else {
                    modules.join(", ")
                }
            ));
        }
    }
}

fn quick_widgets(
    ui: &mut egui::Ui,
    state: &mut AssignmentState,
    document: &EditorDocument,
    catalog: &Catalog,
    app: &AppState,
    output: &mut WidgetOutput,
) {
    let assignments = &document.draft.view.patch_assignments;
    for index in 0..3 {
        let label = unique_quick(assignments, index).map_or_else(
            || crate::localization::text("Missing or ambiguous").into(),
            |entry| {
                target_label(
                    catalog,
                    &TargetChoice::from_reference(
                        PatchAssignments::resolve_quick(entry, &document.draft.view.modules),
                        catalog,
                    ),
                )
            },
        );
        ui.selectable_value(
            &mut state.quick,
            index,
            std::format!("Quick {}: {label}", index + 1),
        );
    }
    state.observe(document, app.epoch, catalog);
    let Some(entry) = unique_quick(assignments, state.quick) else {
        ui.label(crate::localization::text(
            "Read the missing quick-control record before changing its target.",
        ));
        return;
    };
    section(ui, &std::format!("Quick {} target", state.quick + 1));
    target_picker(ui, state, document, catalog, true);
    let original = TargetChoice::from_reference(
        PatchAssignments::resolve_quick(entry, &document.draft.view.modules),
        catalog,
    );
    let proposal = if state.target == original {
        Ok(None)
    } else {
        let target = match &state.target {
            TargetChoice::Off => Some(QuickAssignmentTarget::Off),
            TargetChoice::Parameter(target) => {
                Some(QuickAssignmentTarget::Parameter(target.clone()))
            }
            TargetChoice::PatchVolume => Some(QuickAssignmentTarget::PatchVolume),
            TargetChoice::PatchTempo => Some(QuickAssignmentTarget::PatchTempo),
            TargetChoice::Unresolved { .. } => None,
        };
        target
            .map(|target| {
                Some(AssignmentEdit::QuickTarget {
                    index: state.quick,
                    target,
                })
            })
            .ok_or_else(|| {
                crate::localization::text(
                    "Choose a known target to replace this unresolved reference.",
                )
                .into()
            })
    };
    apply_widget(ui, document, app, proposal, output);
    ui.small(crate::localization::text("A quick control references the selected effect's current model and parameter; it stores no separate knob value."));
}

fn routing_widgets(
    ui: &mut egui::Ui,
    state: &mut AssignmentState,
    document: &EditorDocument,
    catalog: &Catalog,
    app: &AppState,
    output: &mut WidgetOutput,
) {
    state.observe(document, app.epoch, catalog);
    let Some(routing) = &document.draft.view.patch_assignments.routing else {
        ui.label(crate::localization::text(
            "This patch has no unique retained FX loop routing record.",
        ));
        return;
    };
    let Some(chain) = document.draft.view.chain.value() else {
        ui.label(crate::localization::text(
            "The signal chain is unresolved; its FX positions remain readable in the raw capture.",
        ));
        return;
    };
    section(
        ui,
        crate::localization::text("FX Send and Return placement"),
    );
    ui.label(crate::localization::text(
        "Place the loop around a span of the signal chain. Matching positions make an empty span.",
    ));
    ui.horizontal_wrapped(|ui| {
        for (position, module) in chain.iter().enumerate() {
            if position as u8 == state.send_boundary {
                ui.colored_label(
                    Palette::for_context(ui.ctx()).accent,
                    crate::localization::text("Send"),
                );
            }
            if position as u8 == state.return_boundary {
                ui.colored_label(
                    Palette::for_context(ui.ctx()).accent,
                    crate::localization::text("Return"),
                );
            }
            ui.label(module.name());
        }
        if state.send_boundary == 11 {
            ui.colored_label(
                Palette::for_context(ui.ctx()).accent,
                crate::localization::text("Send"),
            );
        }
        if state.return_boundary == 11 {
            ui.colored_label(
                Palette::for_context(ui.ctx()).accent,
                crate::localization::text("Return"),
            );
        }
    });
    let before_send = state.send_boundary;
    let before_return = state.return_boundary;
    ui.horizontal_wrapped(|ui| {
        boundary_picker(
            ui,
            crate::localization::text("Send"),
            &mut state.send_boundary,
            chain,
        );
        if state.send_boundary != before_send && state.send_boundary > state.return_boundary {
            state.return_boundary = state.send_boundary;
        }
        boundary_picker(
            ui,
            crate::localization::text("Return"),
            &mut state.return_boundary,
            chain,
        );
        if state.return_boundary != before_return && state.return_boundary < state.send_boundary {
            state.send_boundary = state.return_boundary;
        }
    });
    let unchanged = state.send_boundary == routing.record.send_boundary
        && state.return_boundary == routing.record.return_boundary;
    let proposal = if unchanged {
        Ok(None)
    } else {
        Ok(Some(AssignmentEdit::FxLoopPositions {
            send_boundary: state.send_boundary,
            return_boundary: state.return_boundary,
        }))
    };
    apply_widget(ui, document, app, proposal, output);
    ui.small(crate::localization::text("Crossing the other position moves both together. Send/Return levels and Series/Parallel mode are separate patch settings."));
}

fn boundary_picker(ui: &mut egui::Ui, label: &str, boundary: &mut u8, chain: &[ModuleId]) {
    ui.label(label);
    egui::ComboBox::from_id_salt(("fx-position", label))
        .selected_text(boundary_label(*boundary, chain))
        .width(170.0)
        .show_ui(ui, |ui| {
            for position in 0..=11 {
                ui.selectable_value(boundary, position, boundary_label(position, chain));
            }
        });
}
fn boundary_label(boundary: u8, chain: &[ModuleId]) -> String {
    match boundary {
        0 => crate::localization::text("Before the chain").into(),
        11 => crate::localization::text("After the chain").into(),
        n => chain.get(usize::from(n)).map_or_else(
            || std::format!("Unknown position {n}"),
            |module| std::format!("Before {}", module.name()),
        ),
    }
}
fn target_picker(
    ui: &mut egui::Ui,
    state: &mut AssignmentState,
    document: &EditorDocument,
    catalog: &Catalog,
    quick: bool,
) {
    let mut target = state.target.clone();
    ui.label(crate::localization::text("Target"));
    egui::ComboBox::from_id_salt(("native-assignment-target", quick))
        .selected_text(target_label(catalog, &target))
        .width(ui.available_width().min(430.0))
        .show_ui(ui, |ui| {
            ui.selectable_value(
                &mut target,
                TargetChoice::Off,
                crate::localization::text("Off"),
            );
            if quick {
                ui.selectable_value(
                    &mut target,
                    TargetChoice::PatchVolume,
                    crate::localization::text("Patch volume"),
                );
                ui.selectable_value(
                    &mut target,
                    TargetChoice::PatchTempo,
                    crate::localization::text("Patch tempo"),
                );
            }
            for module in &document.draft.view.modules {
                let Some(model) = catalog.model_by_id(module.module.get(), module.model.0) else {
                    continue;
                };
                ui.separator();
                ui.label(
                    RichText::new(format!("{} / {}", module.module.name(), model.name)).strong(),
                );
                for parameter in model.parameters_in_display_order() {
                    if !quick && parameter.numeric_bounds().is_none() {
                        continue;
                    }
                    let Ok(parameter_id) = ParamId::new(parameter.parameter_id) else {
                        continue;
                    };
                    ui.selectable_value(
                        &mut target,
                        TargetChoice::Parameter(ParameterAssignmentTarget {
                            module: module.module,
                            expected_model: module.model,
                            parameter: parameter_id,
                        }),
                        crate::localization::text(&parameter.name),
                    );
                }
            }
        });
    state.select_target(target, catalog);
}
fn expression_edit(
    state: &AssignmentState,
    record: &gp200_protocol::ExpressionAssignmentRecord,
    original: &TargetChoice,
    catalog: &Catalog,
) -> Result<Option<AssignmentEdit>, String> {
    let target_changed = state.target != *original;
    match &state.target {
        TargetChoice::Off if target_changed => Ok(Some(AssignmentEdit::ExpressionTargetAndRange {
            slot: state.expression,
            target: ExpressionAssignmentTarget::Off,
            minimum: F32Bits::from_f32(0.0),
            maximum: F32Bits::from_f32(0.0),
        })),
        TargetChoice::Off => Ok(None),
        TargetChoice::Parameter(target) => {
            let minimum = parse_endpoint(&state.minimum).ok_or(crate::localization::text(
                "Enter a finite minimum endpoint.",
            ))?;
            let maximum = parse_endpoint(&state.maximum).ok_or(crate::localization::text(
                "Enter a finite maximum endpoint.",
            ))?;
            let (low, high, _) = target_bounds(catalog, target).ok_or(
                crate::localization::text("This parameter has no recovered endpoint range."),
            )?;
            if [minimum.as_f32(), maximum.as_f32()]
                .into_iter()
                .any(|value| value < low || value > high)
            {
                return Err(std::format!(
                    "Both endpoints must be within {low} to {high}. Reversed ranges are allowed."
                ));
            }
            if target_changed {
                Ok(Some(AssignmentEdit::ExpressionTargetAndRange {
                    slot: state.expression,
                    target: ExpressionAssignmentTarget::Parameter(target.clone()),
                    minimum,
                    maximum,
                }))
            } else if minimum != record.minimum || maximum != record.maximum {
                Ok(Some(AssignmentEdit::ExpressionRange {
                    slot: state.expression,
                    minimum,
                    maximum,
                }))
            } else {
                Ok(None)
            }
        }
        _ => Err(crate::localization::text(
            "Choose a known numeric parameter to replace this unresolved reference.",
        )
        .into()),
    }
}
fn parse_endpoint(text: &str) -> Option<F32Bits> {
    text.trim()
        .parse::<f32>()
        .ok()
        .filter(|value| value.is_finite())
        .map(F32Bits::from_f32)
}
fn target_bounds(catalog: &Catalog, target: &ParameterAssignmentTarget) -> Option<(f32, f32, f32)> {
    catalog
        .model_by_id(target.module.get(), target.expected_model.0)?
        .parameter(target.parameter.get())?
        .numeric_bounds()
}
fn target_label(catalog: &Catalog, choice: &TargetChoice) -> String {
    match choice {
        TargetChoice::Off => crate::localization::text("Off").into(),
        TargetChoice::PatchVolume => crate::localization::text("Patch volume").into(),
        TargetChoice::PatchTempo => crate::localization::text("Patch tempo").into(),
        TargetChoice::Parameter(target) => catalog
            .model_by_id(target.module.get(), target.expected_model.0)
            .and_then(|model| {
                model.parameter(target.parameter.get()).map(|parameter| {
                    format!(
                        "{} / {} / {}",
                        target.module.name(),
                        model.name,
                        crate::localization::text(&parameter.name)
                    )
                })
            })
            .unwrap_or_else(|| {
                std::format!(
                    "Unresolved {} parameter {}",
                    target.module.name(),
                    target.parameter.get()
                )
            }),
        TargetChoice::Unresolved { module, parameter } => {
            std::format!("Unresolved module {module:#04x}, parameter {parameter}")
        }
    }
}
fn editing_context(app: &AppState, document: &EditorDocument) -> &'static str {
    if matches!(document.target, EditTarget::Offline(_)) {
        crate::localization::text("Offline draft")
    } else if app
        .identity
        .as_ref()
        .is_some_and(|identity| identity.mode == BackendMode::Simulator)
    {
        crate::localization::text("Simulator edit buffer")
    } else {
        crate::localization::text("Pedal edit buffer")
    }
}
fn blocked_reason(
    app: &AppState,
    document: &EditorDocument,
    edit: &AssignmentEdit,
) -> Option<String> {
    if matches!(document.target, EditTarget::Offline(_)) {
        return None;
    }
    if app.connection != ConnectionState::Ready {
        return Some(
            crate::localization::text(
                "Reconnect and read the pedal buffer before applying this mapping.",
            )
            .into(),
        );
    }
    let Some(identity) = &app.identity else {
        return Some(
            crate::localization::text("Read the pedal identity before editing its mappings.")
                .into(),
        );
    };
    let operation = EditPatch::SetPatchAssignment(edit.clone()).operation();
    if let OperationAvailability::Blocked { reason } = app.operation_availability(operation) {
        return Some(reason);
    }
    if !app.capabilities.allows(operation, identity) {
        return Some(
            crate::localization::text(
                "This mapping operation has no profile for this device session.",
            )
            .into(),
        );
    }
    if !document.conflicts.is_empty() {
        return Some(
            crate::localization::text(
                "Resolve the pending patch conflict before changing its mappings.",
            )
            .into(),
        );
    }
    if document.pending.values().any(|pending| {
        matches!(
            pending.stage,
            EditStage::Queued | EditStage::Sent | EditStage::AwaitingReadback
        )
    }) {
        return Some(
            crate::localization::text(
                "Wait for the pending edit readback before applying another mapping.",
            )
            .into(),
        );
    }
    match identity.mode {
        BackendMode::Simulator => None,
        BackendMode::CaptureReplay => {
            Some(crate::localization::text("Capture replay is read only.").into())
        }
        BackendMode::Hardware if identity.model != DeviceModel::Gp200 => Some(
            crate::localization::text(
                "The physical assignment profile is currently validated for GP-200.",
            )
            .into(),
        ),
        BackendMode::Hardware => {
            let Some(snapshot) = &app.confirmed_buffer else {
                return Some(
                    crate::localization::text(
                        "Read a fresh current patch before changing its mappings.",
                    )
                    .into(),
                );
            };
            gp200_transport::hardware_assignment_blocked_reason(snapshot, edit)
        }
    }
}
fn apply_widget(
    ui: &mut egui::Ui,
    document: &EditorDocument,
    app: &AppState,
    proposal: Result<Option<AssignmentEdit>, String>,
    output: &mut WidgetOutput,
) {
    ui.add_space(7.0);
    let (edit, reason) = match proposal {
        Ok(Some(edit)) => {
            let reason = blocked_reason(app, document, &edit);
            (Some(edit), reason)
        }
        Ok(None) => (None, None),
        Err(reason) => (None, Some(reason)),
    };
    let label = if matches!(document.target, EditTarget::Offline(_)) {
        crate::localization::text("Apply to draft")
    } else if app
        .identity
        .as_ref()
        .is_some_and(|identity| identity.mode == BackendMode::Simulator)
    {
        crate::localization::text("Apply in simulator")
    } else {
        crate::localization::text("Apply to pedal buffer")
    };
    let response = ui.add_enabled(edit.is_some() && reason.is_none(), egui::Button::new(label));
    output.apply_rect = Some(response.rect);
    if response.clicked() {
        output.edit = edit.clone();
    }
    if let Some(reason) = reason {
        ui.colored_label(Palette::for_context(ui.ctx()).warning, reason);
    } else if edit.is_none() {
        ui.small(crate::localization::text(
            "Choose a change above, then apply it explicitly.",
        ));
    } else if matches!(document.target, EditTarget::Offline(_)) {
        ui.small(crate::localization::text(
            "Updates this local draft. The pedal is unchanged.",
        ));
    } else {
        ui.small(crate::localization::text(
            "Applies this mapping to the edit buffer. Saving to a stored preset is separate.",
        ));
    }
}
fn unique_expression(
    assignments: &PatchAssignments,
    slot: ExpressionSlotId,
) -> Option<&gp200_protocol::ExpressionAssignmentRecord> {
    let mut entries = assignments
        .expressions
        .iter()
        .filter(|entry| entry.record.slot == slot);
    let entry = entries.next()?;
    entries.next().is_none().then_some(&entry.record)
}
fn unique_ctrl(
    assignments: &PatchAssignments,
    slot: u8,
) -> Option<&gp200_protocol::CtrlAssignmentRecord> {
    let mut entries = assignments
        .ctrls
        .iter()
        .filter(|entry| entry.record.slot == slot);
    let entry = entries.next()?;
    entries.next().is_none().then_some(&entry.record)
}
fn unique_quick(
    assignments: &PatchAssignments,
    index: u8,
) -> Option<&gp200_protocol::QuickAssignmentRecord> {
    let mut entries = assignments
        .quicks
        .iter()
        .filter(|entry| entry.record.index == index);
    let entry = entries.next()?;
    entries.next().is_none().then_some(&entry.record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn document() -> EditorDocument {
        let id = DocumentId("assignment-ui".into());
        EditorDocument::new(
            id.clone(),
            EditTarget::Offline(id),
            gp200_sim::captured_demo_snapshot(),
        )
    }
    fn frame(
        ctx: &egui::Context,
        state: &mut AssignmentState,
        document: &EditorDocument,
        catalog: &Catalog,
        app: &AppState,
        events: Vec<egui::Event>,
    ) -> WidgetOutput {
        let mut output = WidgetOutput::default();
        let mut frame = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 900.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    output = render_widgets(ui, state, document, catalog, app);
                });
            },
        );
        frame.textures_delta.clear();
        output
    }
    fn click(
        ctx: &egui::Context,
        state: &mut AssignmentState,
        document: &EditorDocument,
        catalog: &Catalog,
        app: &AppState,
        rect: egui::Rect,
    ) -> WidgetOutput {
        let point = rect.center();
        frame(
            ctx,
            state,
            document,
            catalog,
            app,
            vec![
                egui::Event::PointerMoved(point),
                egui::Event::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        frame(
            ctx,
            state,
            document,
            catalog,
            app,
            vec![
                egui::Event::PointerMoved(point),
                egui::Event::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        )
    }
    #[test]
    fn every_sparse_slot_and_workspace_renders_without_passive_edits() {
        let catalog = Catalog::embedded().unwrap();
        let document = document();
        let original = document.draft.clone();
        let app = AppState::new(AppPreferences::default());
        let ctx = egui::Context::default();
        let assignments = &document.draft.view.patch_assignments;
        assert_eq!(assignments.expressions.len(), 9);
        assert_eq!(assignments.ctrls.len(), 8);
        assert_eq!(assignments.quicks.len(), 3);
        for tab in [
            AssignmentTab::Expression,
            AssignmentTab::Ctrl,
            AssignmentTab::Quick,
            AssignmentTab::Routing,
        ] {
            for group in 0..3 {
                for position in 0..3 {
                    let mut state = AssignmentState {
                        tab,
                        expression: ExpressionSlotId::new(group, position).unwrap(),
                        ctrl: group * 2 + position,
                        quick: position,
                        ..Default::default()
                    };
                    let result = frame(&ctx, &mut state, &document, &catalog, &app, Vec::new());
                    assert!(result.edit.is_none());
                    if tab == AssignmentTab::Expression {
                        assert_eq!(result.slot_rects.len(), 9);
                    }
                }
            }
        }
        assert_eq!(document.draft, original);
        assert!(document.undo.past.is_empty());
    }
    #[test]
    fn expression_selection_loads_exact_reference_without_editing_it() {
        let catalog = Catalog::embedded().unwrap();
        let document = document();
        let original = document.draft.clone();
        let app = AppState::new(AppPreferences::default());
        let ctx = egui::Context::default();
        let mut state = AssignmentState::default();
        let first = frame(&ctx, &mut state, &document, &catalog, &app, Vec::new());
        let slot = ExpressionSlotId::new(1, 0).unwrap();
        let rect = first
            .slot_rects
            .iter()
            .find(|(id, _)| *id == slot)
            .unwrap()
            .1;
        let result = click(&ctx, &mut state, &document, &catalog, &app, rect);
        assert!(result.edit.is_none());
        assert_eq!(state.expression.packed(), 0x10);
        assert!(
            matches!(state.target, TargetChoice::Parameter(ref t) if t.module.get() == 1 && t.expected_model.0 == 0x0500000a && t.parameter.get() == 3)
        );
        assert_eq!(state.minimum, "0");
        assert_eq!(state.maximum, "100");
        assert_eq!(document.draft, original);
    }
    #[test]
    fn ctrl_checkbox_stages_then_explicit_apply_creates_one_lossless_undo_step() {
        let catalog = Catalog::embedded().unwrap();
        let mut document = document();
        let original = document.draft.clone();
        let app = AppState::new(AppPreferences::default());
        let ctx = egui::Context::default();
        let mut state = AssignmentState {
            tab: AssignmentTab::Ctrl,
            ctrl: 7,
            ..Default::default()
        };
        let first = frame(&ctx, &mut state, &document, &catalog, &app, Vec::new());
        let vol = ModuleId::new(10).unwrap();
        let rect = first
            .module_rects
            .iter()
            .find(|(id, _)| *id == vol)
            .unwrap()
            .1;
        let selected = click(&ctx, &mut state, &document, &catalog, &app, rect);
        assert!(selected.edit.is_none());
        assert!(state.modules.contains(&vol));
        assert_eq!(document.draft, original);
        let applied = click(
            &ctx,
            &mut state,
            &document,
            &catalog,
            &app,
            selected.apply_rect.unwrap(),
        );
        let edit = applied.edit.unwrap();
        assert_eq!(
            edit,
            AssignmentEdit::CtrlModules {
                slot: 7,
                modules: vec![vol]
            }
        );
        document
            .apply(&EditPatch::SetPatchAssignment(edit))
            .unwrap();
        let encoded = document
            .draft
            .snapshot(
                &document.baseline,
                SnapshotSource::Simulator,
                document.local_revision,
            )
            .unwrap();
        for (before, after) in document
            .baseline
            .raw
            .records()
            .iter()
            .zip(encoded.raw.records())
        {
            if before.command == 15 && before.body[0] == 7 {
                assert_eq!(before.body[..4], after.body[..4]);
                assert_eq!(
                    u32::from_le_bytes(after.body[4..8].try_into().unwrap()),
                    1 << 10
                );
            } else {
                assert_eq!(before, after);
            }
        }
        assert_eq!(document.undo.past.len(), 1);
        assert!(document.undo());
        assert_eq!(document.draft, original);
        assert!(document.redo());
        assert_ne!(document.draft, original);
    }
    #[test]
    fn explicit_reversed_endpoints_preserve_float_bits_and_unrelated_records() {
        let catalog = Catalog::embedded().unwrap();
        let mut document = document();
        let app = AppState::new(AppPreferences::default());
        let ctx = egui::Context::default();
        let slot = ExpressionSlotId::new(1, 0).unwrap();
        let mut state = AssignmentState {
            expression: slot,
            ..Default::default()
        };
        frame(&ctx, &mut state, &document, &catalog, &app, Vec::new());
        state.minimum = "99.35505".into();
        state.maximum = "0.713579".into();
        let staged = frame(&ctx, &mut state, &document, &catalog, &app, Vec::new());
        assert!(staged.edit.is_none());
        let result = click(
            &ctx,
            &mut state,
            &document,
            &catalog,
            &app,
            staged.apply_rect.unwrap(),
        );
        let edit = result.edit.unwrap();
        assert!(
            matches!(edit, AssignmentEdit::ExpressionRange { minimum, maximum, .. } if minimum.0 == 99.35505_f32.to_bits() && maximum.0 == 0.713579_f32.to_bits())
        );
        document
            .apply(&EditPatch::SetPatchAssignment(edit))
            .unwrap();
        let encoded = document
            .draft
            .snapshot(
                &document.baseline,
                SnapshotSource::Simulator,
                document.local_revision,
            )
            .unwrap();
        for (before, after) in document
            .baseline
            .raw
            .records()
            .iter()
            .zip(encoded.raw.records())
        {
            if before.command == 12 && before.body[0] == 0x10 {
                assert_eq!(before.body[..4], after.body[..4]);
            } else {
                assert_eq!(before, after);
            }
        }
    }
    #[test]
    fn sparse_catalog_id_and_target_model_survive_selection() {
        let catalog = Catalog::embedded().unwrap();
        let document = document();
        let mut state = AssignmentState::default();
        state.observe(&document, SessionEpoch(1), &catalog);
        let module = document
            .draft
            .view
            .module(ModuleId::new(5).unwrap())
            .unwrap();
        let model = catalog.model_by_id(5, module.model.0).unwrap();
        let parameter = model.parameter(5).unwrap();
        assert_ne!(parameter.display_index, parameter.parameter_id);
        let target = ParameterAssignmentTarget {
            module: module.module,
            expected_model: module.model,
            parameter: ParamId::new(5).unwrap(),
        };
        state.select_target(TargetChoice::Parameter(target.clone()), &catalog);
        let record =
            unique_expression(&document.draft.view.patch_assignments, state.expression).unwrap();
        let original = TargetChoice::from_reference(
            PatchAssignments::resolve_expression(record, &document.draft.view.modules),
            &catalog,
        );
        let edit = expression_edit(&state, record, &original, &catalog)
            .unwrap()
            .unwrap();
        assert!(
            matches!(edit, AssignmentEdit::ExpressionTargetAndRange { target: ExpressionAssignmentTarget::Parameter(ref t), .. } if *t == target)
        );
    }
    #[test]
    fn quick_specials_and_sparse_parameter_targets_require_explicit_apply() {
        let catalog = Catalog::embedded().unwrap();
        let app = AppState::new(AppPreferences::default());
        let cab = document()
            .draft
            .view
            .module(ModuleId::new(5).unwrap())
            .unwrap()
            .clone();
        for target in [
            TargetChoice::Off,
            TargetChoice::PatchVolume,
            TargetChoice::PatchTempo,
            TargetChoice::Parameter(ParameterAssignmentTarget {
                module: cab.module,
                expected_model: cab.model,
                parameter: ParamId::new(5).unwrap(),
            }),
        ] {
            let mut document = document();
            let original = document.draft.clone();
            let ctx = egui::Context::default();
            let mut state = AssignmentState {
                tab: AssignmentTab::Quick,
                quick: 0,
                ..Default::default()
            };
            frame(&ctx, &mut state, &document, &catalog, &app, Vec::new());
            state.select_target(target.clone(), &catalog);
            let staged = frame(&ctx, &mut state, &document, &catalog, &app, Vec::new());
            assert!(staged.edit.is_none());
            assert_eq!(document.draft, original);
            let applied = click(
                &ctx,
                &mut state,
                &document,
                &catalog,
                &app,
                staged.apply_rect.unwrap(),
            );
            let edit = applied.edit.unwrap();
            assert!(matches!(edit, AssignmentEdit::QuickTarget { index: 0, .. }));
            document
                .apply(&EditPatch::SetPatchAssignment(edit))
                .unwrap();
            let encoded = document
                .draft
                .snapshot(
                    &document.baseline,
                    SnapshotSource::Simulator,
                    document.local_revision,
                )
                .unwrap();
            for (before, after) in document
                .baseline
                .raw
                .records()
                .iter()
                .zip(encoded.raw.records())
            {
                if before.command == 16 && before.body[0] == 0 {
                    let expected = match target {
                        TargetChoice::Off => [0, 255, 0, 0],
                        TargetChoice::PatchVolume => [0, 11, 0, 0],
                        TargetChoice::PatchTempo => [0, 12, 0, 0],
                        TargetChoice::Parameter(_) => [0, 5, 5, 0],
                        _ => unreachable!(),
                    };
                    assert_eq!(after.body, expected);
                } else {
                    assert_eq!(before, after);
                }
            }
            assert_eq!(document.undo.past.len(), 1);
            assert!(document.undo());
            assert_eq!(document.draft, original);
        }
    }
    #[test]
    fn routing_apply_updates_the_pair_and_preserves_chain_and_padding() {
        let catalog = Catalog::embedded().unwrap();
        let mut document = document();
        let original = document.draft.clone();
        let app = AppState::new(AppPreferences::default());
        let ctx = egui::Context::default();
        let mut state = AssignmentState {
            tab: AssignmentTab::Routing,
            ..Default::default()
        };
        frame(&ctx, &mut state, &document, &catalog, &app, Vec::new());
        state.send_boundary = 2;
        state.return_boundary = 8;
        let staged = frame(&ctx, &mut state, &document, &catalog, &app, Vec::new());
        assert!(staged.edit.is_none());
        let applied = click(
            &ctx,
            &mut state,
            &document,
            &catalog,
            &app,
            staged.apply_rect.unwrap(),
        );
        let edit = applied.edit.unwrap();
        assert_eq!(
            edit,
            AssignmentEdit::FxLoopPositions {
                send_boundary: 2,
                return_boundary: 8
            }
        );
        document
            .apply(&EditPatch::SetPatchAssignment(edit))
            .unwrap();
        let encoded = document
            .draft
            .snapshot(
                &document.baseline,
                SnapshotSource::Simulator,
                document.local_revision,
            )
            .unwrap();
        for (before, after) in document
            .baseline
            .raw
            .records()
            .iter()
            .zip(encoded.raw.records())
        {
            if before.command == 8 {
                assert_eq!(after.body[2..4], [2, 8]);
                assert_eq!(before.body[..2], after.body[..2]);
                assert_eq!(before.body[4..], after.body[4..]);
            } else {
                assert_eq!(before, after);
            }
        }
        assert_eq!(document.undo.past.len(), 1);
        assert!(document.undo());
        assert_eq!(document.draft, original);
    }
    #[test]
    fn selection_model_revision_and_epoch_changes_retire_staged_values() {
        let catalog = Catalog::embedded().unwrap();
        let mut document = document();
        let mut state = AssignmentState {
            expression: ExpressionSlotId::new(1, 0).unwrap(),
            ..Default::default()
        };
        state.observe(&document, SessionEpoch(1), &catalog);
        state.maximum = "99".into();
        state.observe(&document, SessionEpoch(1), &catalog);
        assert_eq!(state.maximum, "99");
        state.observe(&document, SessionEpoch(2), &catalog);
        assert_eq!(state.maximum, "100");
        state.maximum = "98".into();
        document.local_revision.0 += 1;
        state.observe(&document, SessionEpoch(2), &catalog);
        assert_eq!(state.maximum, "100");
        state.maximum = "97".into();
        document
            .draft
            .view
            .modules
            .iter_mut()
            .find(|module| module.module.get() == 1)
            .unwrap()
            .model = ModelCode(0x0500000b);
        state.observe(&document, SessionEpoch(2), &catalog);
        assert_eq!(state.maximum, "100");
        state.expression = ExpressionSlotId::new(2, 2).unwrap();
        state.observe(&document, SessionEpoch(2), &catalog);
        assert_eq!(state.target, TargetChoice::Off);
    }
    #[test]
    fn unresolved_nan_inactive_and_unknown_membership_values_are_read_only_on_load() {
        let catalog = Catalog::embedded().unwrap();
        let mut document = document();
        let assignments = &mut document.draft.view.patch_assignments;
        assignments.expressions[0].record.parameter_raw = 65535;
        assignments.expressions[0].record.minimum = F32Bits(0x7fc12345);
        assignments.expressions[0].record.maximum = F32Bits(0x80000000);
        assignments.ctrls[0].record.module_mask_raw = 0xa5000000;
        assignments.quicks[0].record.module_raw = 0xfa;
        assignments.quicks[0].record.parameter_raw = 60000;
        let original = document.draft.clone();
        let app = AppState::new(AppPreferences::default());
        let ctx = egui::Context::default();
        for tab in [
            AssignmentTab::Expression,
            AssignmentTab::Ctrl,
            AssignmentTab::Quick,
        ] {
            let mut state = AssignmentState {
                tab,
                ..Default::default()
            };
            let output = frame(&ctx, &mut state, &document, &catalog, &app, Vec::new());
            assert!(output.edit.is_none());
        }
        assert_eq!(document.draft, original);
        assert!(
            unique_expression(
                &document.draft.view.patch_assignments,
                ExpressionSlotId::new(0, 0).unwrap()
            )
            .unwrap()
            .minimum
            .0 == 0x7fc12345
        );
    }
    #[test]
    fn hardware_gate_is_precise_and_does_not_certify_a_staged_target() {
        let mut document = document();
        document.target = EditTarget::DeviceBuffer;
        let mut app = AppState::new(AppPreferences::default());
        app.connection = ConnectionState::Ready;
        app.epoch = SessionEpoch(1);
        app.identity = Some(DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Hardware,
            firmware: None,
            display_name: "GP-200".into(),
        });
        app.capabilities = gp200_transport::verified_capabilities();
        app.confirmed_buffer = Some(document.baseline.clone());
        let slot = ExpressionSlotId::new(1, 0).unwrap();
        let maximum = AssignmentEdit::ExpressionRange {
            slot,
            minimum: F32Bits::from_f32(0.),
            maximum: F32Bits::from_f32(99.),
        };
        assert!(blocked_reason(&app, &document, &maximum).is_none());
        let minimum = AssignmentEdit::ExpressionRange {
            slot,
            minimum: F32Bits::from_f32(1.),
            maximum: F32Bits::from_f32(100.),
        };
        assert!(blocked_reason(&app, &document, &minimum).is_none());
        assert!(
            blocked_reason(
                &app,
                &document,
                &AssignmentEdit::CtrlState {
                    slot: 7,
                    enabled: true
                }
            )
            .is_some()
        );
        let unproved = AssignmentEdit::ExpressionRange {
            slot: ExpressionSlotId::new(2, 2).unwrap(),
            minimum: F32Bits::from_f32(0.),
            maximum: F32Bits::from_f32(99.),
        };
        document
            .draft
            .view
            .patch_assignments
            .expressions
            .iter_mut()
            .find(|entry| entry.record.slot.packed() == 0x22)
            .unwrap()
            .record
            .module_raw = 1;
        assert!(blocked_reason(&app, &document, &unproved).is_some());
        document.target = EditTarget::Offline(DocumentId("offline".into()));
        assert!(blocked_reason(&app, &document, &minimum).is_none());
        document.target = EditTarget::DeviceBuffer;
        app.identity.as_mut().unwrap().mode = BackendMode::Simulator;
        app.capabilities = Capabilities::simulator();
        assert!(blocked_reason(&app, &document, &minimum).is_none());
        app.identity.as_mut().unwrap().mode = BackendMode::Hardware;
        app.identity.as_mut().unwrap().model = DeviceModel::Gp200Lt;
        assert!(blocked_reason(&app, &document, &maximum).is_some());
        app.confirmed_buffer = Some(Arc::new((*document.baseline).clone()));
    }
}
