//! Widgets use catalog request plans; rendering never normalizes retained bits.
use crate::theme::Palette;
use eframe::egui::{self, RichText};
use gp200_app::AppState;
use gp200_catalog::{
    ControlRequest, ControlSemantics, ModelDefinition, OrdinaryRestore, ParameterDefinition,
    SYNC_SUBDIVISIONS, SentinelKind, control_semantics, plan_control_request, recognized_sentinel,
    sentinel_value, subdivision_index, sync_value_parameter,
};
use gp200_domain::*;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SyncKey {
    module: u8,
    model: u32,
    power: u8,
    value: u8,
}
#[derive(Clone)]
pub(super) struct EditContext {
    document: DocumentId,
    target: EditTarget,
    epoch: SessionEpoch,
    baseline: Arc<PresetSnapshot>,
    wire: Option<WireContext>,
}
impl EditContext {
    pub(super) fn new(
        document: &EditorDocument,
        epoch: SessionEpoch,
        wire: Option<WireContext>,
    ) -> Self {
        Self {
            document: document.id.clone(),
            target: document.target.clone(),
            epoch,
            baseline: document.baseline.clone(),
            wire,
        }
    }
    fn matches(&self, other: &Self) -> bool {
        self.document == other.document
            && self.target == other.target
            && self.epoch == other.epoch
            && self.wire == other.wire
            && Arc::ptr_eq(&self.baseline, &other.baseline)
    }
}
#[derive(Clone)]
struct OffReview {
    key: SyncKey,
    ordinary: String,
}
/// Cache only values actually seen with Sync Off in this document/context.
/// Selecting a preset, changing model or session, and saving retire the cache.
#[derive(Default)]
pub(crate) struct ControlState {
    context: Option<EditContext>,
    models: BTreeMap<u8, u32>,
    ordinary: BTreeMap<SyncKey, F32Bits>,
    review: Option<OffReview>,
}
impl ControlState {
    pub(super) fn observe(
        &mut self,
        document: &EditorDocument,
        epoch: SessionEpoch,
        wire: Option<WireContext>,
        module: &ModuleSnapshot,
        model: &ModelDefinition,
        source: Option<&ModuleSnapshot>,
    ) {
        let next = EditContext::new(document, epoch, wire);
        let same = self
            .context
            .as_ref()
            .is_some_and(|context| context.matches(&next));
        if !same {
            self.context = Some(next);
            self.models.clear();
            self.ordinary.clear();
            self.review = None;
        }
        if self.models.insert(module.module.get(), module.model.0) != Some(module.model.0) {
            self.ordinary
                .retain(|key, _| key.module != module.module.get());
            if self
                .review
                .as_ref()
                .is_some_and(|review| review.key.module == module.module.get())
            {
                self.review = None;
            }
        }
        let Some(source) = source.filter(|source| source.model == module.model) else {
            return;
        };
        for parameter in &model.parameters {
            let ControlSemantics::SyncNumeric { power_parameter } = control_semantics(parameter)
            else {
                continue;
            };
            if source
                .parameters
                .get(usize::from(power_parameter))
                .map(|bits| bits.as_f32())
                != Some(0.0)
            {
                continue;
            }
            let Some(value) = source
                .parameters
                .get(usize::from(parameter.parameter_id))
                .copied()
            else {
                continue;
            };
            if let Some((minimum, maximum, _)) = parameter.numeric_bounds()
                && value.as_f32().is_finite()
                && (minimum..=maximum).contains(&value.as_f32())
            {
                self.ordinary.insert(
                    SyncKey {
                        module: module.module.get(),
                        model: model.code,
                        power: power_parameter,
                        value: parameter.parameter_id,
                    },
                    value,
                );
            }
        }
    }
}

/// An offline document is always local, even while a physical pedal is attached.
pub(super) fn blocked_reason(
    state: &AppState,
    target: &EditTarget,
    module: &ModuleSnapshot,
    model: &ModelDefinition,
    parameter: &ParameterDefinition,
    allowed: bool,
) -> Option<String> {
    if !allowed {
        return Some(crate::localization::text("Unlock the performance controls to edit.").into());
    }
    if matches!(target, EditTarget::Offline(_)) {
        return None;
    }
    if state.connection != ConnectionState::Ready {
        return Some(
            crate::localization::text("Reconnect and read the pedal buffer before editing it.")
                .into(),
        );
    }
    let Some(identity) = &state.identity else {
        return Some(
            crate::localization::text("Read the pedal identity before editing its buffer.").into(),
        );
    };
    if !state
        .capabilities
        .allows(OperationKind::SetParameter, identity)
    {
        return Some(
            crate::localization::text("Effect edits are unavailable for this device session.")
                .into(),
        );
    }
    if let OperationAvailability::Blocked { reason } =
        state.operation_availability(OperationKind::SetParameter)
    {
        return Some(reason);
    }
    match identity.mode {
        BackendMode::Simulator => None,
        BackendMode::CaptureReplay => {
            Some(crate::localization::text("Capture replay is read only.").into())
        }
        BackendMode::Hardware => {
            let verified = if identity.model != DeviceModel::Gp200 {
                false
            } else if matches!(
                control_semantics(parameter),
                ControlSemantics::SyncPower { .. }
            ) {
                sync_value_parameter(model, parameter.parameter_id).is_ok_and(|value| {
                    gp200_transport::hardware_sync_verified(
                        module.module.get(),
                        model.code,
                        parameter.parameter_id,
                        value,
                    )
                })
            } else {
                gp200_transport::hardware_control_verified(
                    module.module.get(),
                    model.code,
                    parameter,
                )
            };
            (!verified).then(|| crate::localization::text("This control has no verified GP-200 hardware profile. Edit an offline copy to prepare it locally.").into())
        }
    }
}

pub(super) fn patch_for_request(
    module: &ModuleSnapshot,
    model: &ModelDefinition,
    parameter: &ParameterDefinition,
    request: ControlRequest,
) -> Result<EditPatch, String> {
    let values = module
        .parameters
        .iter()
        .map(|bits| bits.0)
        .collect::<Vec<_>>();
    let plan = plan_control_request(model, parameter.parameter_id, &values, request)
        .map_err(|error| error.to_string())?;
    if let ControlRequest::Sync {
        enabled,
        ordinary_restore,
    } = request
    {
        return Ok(EditPatch::SetSync {
            module: module.module,
            model: module.model,
            model_index: ModelIndex(u32::from(model.xml_index)),
            power_parameter: ParamId::new(parameter.parameter_id)
                .map_err(|error| error.to_string())?,
            value_parameter: ParamId::new(
                sync_value_parameter(model, parameter.parameter_id)
                    .map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?,
            enabled,
            ordinary_restore: ordinary_restore.map(|restore| F32Bits(restore.bits())),
        });
    }
    let step = plan.steps.first().ok_or(crate::localization::text(
        "Control request did not produce a value",
    ))?;
    Ok(EditPatch::SetParameter {
        module: module.module,
        model: module.model,
        model_index: ModelIndex(u32::from(model.xml_index)),
        parameter: ParamId::new(step.parameter_id).map_err(|error| error.to_string())?,
        value: F32Bits(step.raw_bits),
    })
}

#[derive(Default)]
pub(super) struct WidgetEdit {
    pub patch: Option<EditPatch>,
    pub error: Option<String>,
    pub started: bool,
    pub stopped: bool,
    #[cfg(test)]
    checkbox_rect: Option<egui::Rect>,
    #[cfg(test)]
    review_apply_rect: Option<egui::Rect>,
}

#[derive(Clone, Default)]
struct NumericEntryGeneration {
    context: Option<EditContext>,
    generation: u64,
}

/// Retire only this widget's text buffer after Escape. In pinned egui 0.36.2,
/// DragValue skips parsing on Escape but retains its private EditState, then
/// parses that cancelled text on the next lost-focus frame. A fresh scoped ID
/// prevents that deferred parse without clearing other widgets' egui memory.
/// A document switch or capture replacement retires the old buffer before
/// rendering, so losing focus cannot parse outgoing text into the new document.
pub(super) fn committed_numeric(
    ui: &mut egui::Ui,
    context: Option<&EditContext>,
    render: impl FnOnce(&mut egui::Ui) -> egui::Response,
) -> egui::Response {
    let key = ui.next_auto_id().with("committed-numeric");
    let mut state = ui
        .data(|data| data.get_temp::<NumericEntryGeneration>(key))
        .unwrap_or_default();
    let same = match (&state.context, context) {
        (Some(previous), Some(current)) => previous.matches(current),
        (None, None) => true,
        _ => false,
    };
    if !same {
        state.generation = state.generation.wrapping_add(1);
        state.context = context.cloned();
    }
    let response = ui.push_id(state.generation, render).inner;
    if (response.has_focus() || response.lost_focus())
        && ui.input(|input| input.key_pressed(egui::Key::Escape))
    {
        state.generation = state.generation.wrapping_add(1);
    }
    ui.data_mut(|data| data.insert_temp(key, state));
    response
}

pub(super) fn render(
    ui: &mut egui::Ui,
    state: &mut ControlState,
    module: &ModuleSnapshot,
    model: &ModelDefinition,
    parameter: &ParameterDefinition,
    blocked: Option<&str>,
) -> WidgetEdit {
    let mut output = WidgetEdit::default();
    let Some(bits) = module
        .parameters
        .get(usize::from(parameter.parameter_id))
        .copied()
    else {
        ui.label(std::format!(
            "{}: missing raw slot",
            crate::localization::text(&parameter.name)
        ));
        return output;
    };
    let values = module
        .parameters
        .iter()
        .map(|bits| bits.as_f32())
        .collect::<Vec<_>>();
    let presentation = parameter.presentation(bits.as_f32(), &values);
    let palette = Palette::for_context(ui.ctx());
    let semantics = control_semantics(parameter);
    let mut request = None;
    ui.push_id((module.module.get(), model.code, parameter.parameter_id), |ui| {
        ui.horizontal(|ui| {
            let label = ui.add_sized([105.0, 28.0], egui::Label::new(crate::localization::text(&parameter.name)));
            if !presentation.editable {
                let response = ui.label(RichText::new(crate::localization::value(&presentation.text)).color(palette.muted));
                if let Some(explanation) = &presentation.explanation { response.on_hover_text(crate::localization::message(explanation)); }
                return;
            }
            let response = ui.add_enabled_ui(blocked.is_none(), |ui| {
                match semantics {
                    ControlSemantics::SyncPower { .. } => {
                        let Ok(value_parameter) = sync_value_parameter(model, parameter.parameter_id) else {
                            ui.label(crate::localization::text("Sync binding unavailable"));
                            return;
                        };
                        let key = SyncKey { module: module.module.get(), model: model.code, power: parameter.parameter_id, value: value_parameter };
                        let mut enabled = bits.as_f32() == 1.0;
                        let checkbox = ui.checkbox(&mut enabled, crate::localization::text("On"));
                        #[cfg(test)]
                        { output.checkbox_rect = Some(checkbox.rect); }
                        if checkbox.changed() {
                            if enabled {
                                state.review = None;
                                request = Some(ControlRequest::Sync { enabled: true, ordinary_restore: None });
                            } else if let Some(restore) = state.ordinary.get(&key) {
                                state.review = None;
                                request = Some(ControlRequest::Sync { enabled: false, ordinary_restore: Some(OrdinaryRestore::ObservedInSameContext(restore.0)) });
                            } else {
                                state.review = Some(OffReview { key, ordinary: String::new() });
                            }
                        }
                        if bits.as_f32() == 1.0 {
                            if let Some(restore) = state.ordinary.get(&key)
                                && let Some(bound) = model.parameter(value_parameter)
                            {
                                ui.small(std::format!("Off restores {}", bound.presentation(restore.as_f32(), &[]).text));
                            } else {
                                ui.small(crate::localization::text("Choose a rate or time when turning off"));
                            }
                        }
                    }
                    ControlSemantics::SyncNumeric { power_parameter }
                        if values.get(usize::from(power_parameter)) == Some(&1.0) =>
                    {
                        let current = subdivision_index(bits.as_f32());
                        egui::ComboBox::from_id_salt("subdivision").selected_text(crate::localization::value(&presentation.text))
                            .width(140.0).show_ui(ui, |ui| {
                            for (index, label) in SYNC_SUBDIVISIONS.iter().enumerate() {
                                if ui.selectable_label(current == Some(index as u8), crate::localization::text(label)).clicked() {
                                    request = Some(ControlRequest::Subdivision(index as u8));
                                }
                            }
                        });
                        ui.small(crate::localization::text("Beat subdivision"));
                    }
                    ControlSemantics::Numeric | ControlSemantics::SyncNumeric { .. } => {
                        if let Some((minimum, maximum, step)) = parameter.numeric_bounds() {
                            let mut value = bits.as_f32();
                            let width = (ui.available_width() - 135.0).clamp(45.0, 350.0);
                            let slider = ui.scope(|ui| {
                                ui.visuals_mut().widgets.inactive.bg_fill = palette.rule;
                                ui.visuals_mut().selection.bg_fill = palette.accent;
                                ui.spacing_mut().slider_rail_height = 5.0;
                                ui.add_sized([width, 28.0], egui::Slider::new(&mut value, minimum..=maximum)
                                    .clamping(egui::SliderClamping::Edits)
                                    .step_by(f64::from(step)).show_value(false).trailing_fill(true))
                            }).inner;
                            let slider = slider.labelled_by(label.id);
                            let mut changed = slider.changed();
                            output.started |= slider.drag_started();
                            output.stopped |= slider.drag_stopped();
                            let numeric = committed_numeric(ui, state.context.as_ref(), |ui| ui.add(egui::DragValue::new(&mut value)
                                .update_while_editing(false)
                                .range(minimum..=maximum).clamp_existing_to_range(false)
                                .speed(f64::from(step)).max_decimals(6)
                                .custom_formatter(|value, decimals| {
                                    match recognized_sentinel(parameter, value as f32) {
                                        Some(SentinelKind::Off) => crate::localization::text("OFF").into(),
                                        Some(SentinelKind::Mute) => crate::localization::text("Mute").into(),
                                        None => egui::emath::format_with_decimals_in_range(value, decimals),
                                    }
                                })
                                .custom_parser(|text| parse_numeric(parameter, text).map(f64::from))))
                                .on_hover_text(std::format!("{}; raw bits 0x{:08x}", presentation.text, bits.0))
                                .labelled_by(label.id);
                            changed |= numeric.changed();
                            output.started |= numeric.drag_started();
                            output.stopped |= numeric.drag_stopped();
                            if recognized_sentinel(parameter, value).is_none()
                                && let Some(unit) = &parameter.suffix { ui.small(unit); }
                            // These buttons use exact catalog endpoints. Keep them
                            // beside the value so all CAB controls fit compact views.
                            for kind in [SentinelKind::Off, SentinelKind::Mute] {
                                if sentinel_value(parameter, kind).is_some()
                                    && recognized_sentinel(parameter, bits.as_f32()) != Some(kind)
                                {
                                    let label = match kind { SentinelKind::Off => crate::localization::text("Set OFF"), SentinelKind::Mute => crate::localization::text("Mute") };
                                    if ui.small_button(label).clicked() {
                                        request = Some(ControlRequest::Sentinel(kind));
                                    }
                                }
                            }
                            if changed && value.to_bits() != bits.0 {
                                request = Some(ControlRequest::RawBits(value.to_bits()));
                            }
                        }
                    }
                    ControlSemantics::Choice | ControlSemantics::Switch => {
                        egui::ComboBox::from_id_salt("choice").selected_text(crate::localization::value(&presentation.text))
                            .width(140.0).show_ui(ui, |ui| {
                            for choice in &parameter.choices {
                                if ui.selectable_label(bits.as_f32() == choice.id as f32, crate::localization::choice(&choice.name)).clicked() {
                                    request = Some(ControlRequest::Choice(choice.id));
                                }
                            }
                        });
                    }
                    ControlSemantics::Unsupported => { ui.label(crate::localization::value(&presentation.text)); }
                }
            }).response;
            if let Some(reason) = blocked {
                response.on_hover_text(crate::localization::message(reason));
                ui.label(RichText::new(crate::localization::text("Read only")).small().color(palette.muted)).on_hover_text(crate::localization::message(reason));
            }
        });
        if let Some(explanation) = &presentation.explanation {
            ui.label(RichText::new(crate::localization::message(explanation)).small().color(palette.muted));
        }
        if let Some(mut review) = state.review.clone()
            && review.key.module == module.module.get() && review.key.model == model.code
            && review.key.power == parameter.parameter_id
        {
            if bits.as_f32() != 1.0 {
                state.review = None;
            } else if let Some(bound) = model.parameter(review.key.value) {
                let mut cancel = false;
                let mut apply = false;
                egui::Frame::new().fill(palette.raised).inner_margin(12).show(ui, |ui| {
                    ui.label(std::format!("Choose {} with Sync off", crate::localization::text(&bound.name)));
                    if let Some((minimum, maximum, _)) = bound.numeric_bounds() {
                        ui.small(std::format!("Enter {minimum}–{maximum} {}. The synced patch does not contain its previous ordinary value.", bound.suffix.as_deref().unwrap_or("")));
                    }
                    let mut parsed = None;
                    ui.horizontal_wrapped(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut review.ordinary).hint_text(crate::localization::text("Enter a value")).desired_width(140.0));
                        parsed = parse_ordinary(bound, &review.ordinary);
                        if let Some(unit) = &bound.suffix { ui.label(unit); }
                        let response = ui.add_enabled(blocked.is_none() && parsed.is_some(), egui::Button::new(crate::localization::text("Turn Sync off")));
                        #[cfg(test)]
                        { output.review_apply_rect = Some(response.rect); }
                        apply = response.clicked();
                        cancel = ui.button(crate::localization::text("Cancel")).clicked();
                    });
                    if apply && let Some(value) = parsed {
                        request = Some(ControlRequest::Sync { enabled: false, ordinary_restore: Some(OrdinaryRestore::ExplicitUserValue(value.0)) });
                    }
                });
                state.review = if cancel || apply { None } else { Some(review) };
            }
        }
    });
    if let Some(request) = request {
        match patch_for_request(module, model, parameter, request) {
            Ok(patch) => output.patch = Some(patch),
            Err(error) => output.error = Some(error),
        }
    }
    output
}

fn parse_numeric(parameter: &ParameterDefinition, text: &str) -> Option<f32> {
    match text.trim().to_ascii_lowercase().as_str() {
        "off" => sentinel_value(parameter, SentinelKind::Off),
        "mute" => sentinel_value(parameter, SentinelKind::Mute),
        _ => text
            .trim()
            .parse::<f32>()
            .ok()
            .filter(|value| value.is_finite()),
    }
}
fn parse_ordinary(parameter: &ParameterDefinition, text: &str) -> Option<F32Bits> {
    let value = text.trim().parse::<f32>().ok()?;
    let (minimum, maximum, _) = parameter.numeric_bounds()?;
    (value.is_finite() && (minimum..=maximum).contains(&value)).then(|| F32Bits(value.to_bits()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gp200_catalog::Catalog;

    fn module(model: &ModelDefinition) -> ModuleSnapshot {
        let module = gp200_catalog::MODULE_NAMES
            .iter()
            .position(|name| *name == model.module)
            .unwrap();
        let mut values = vec![F32Bits(0); 15];
        for parameter in &model.parameters {
            if let Some(value) = parameter.default {
                values[usize::from(parameter.parameter_id)] = F32Bits(value.to_bits());
            }
        }
        ModuleSnapshot {
            module: ModuleId::new(module as u8).unwrap(),
            state_raw: 1,
            slot_count_raw: 15,
            model: ModelCode(model.code),
            parameters: values,
            raw_record_index: 0,
        }
    }
    fn render_once(
        state: &mut ControlState,
        module: &ModuleSnapshot,
        model: &ModelDefinition,
        parameter: &ParameterDefinition,
    ) -> WidgetEdit {
        let ctx = egui::Context::default();
        render_frame(&ctx, state, module, model, parameter, Vec::new())
    }
    fn render_frame(
        ctx: &egui::Context,
        state: &mut ControlState,
        module: &ModuleSnapshot,
        model: &ModelDefinition,
        parameter: &ParameterDefinition,
        events: Vec<egui::Event>,
    ) -> WidgetEdit {
        let mut result = WidgetEdit::default();
        let mut frame = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 500.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    result = render(ui, state, module, model, parameter, None);
                });
            },
        );
        frame.textures_delta.clear();
        result
    }
    fn click(
        ctx: &egui::Context,
        state: &mut ControlState,
        module: &ModuleSnapshot,
        model: &ModelDefinition,
        parameter: &ParameterDefinition,
        rect: egui::Rect,
    ) -> WidgetEdit {
        let point = rect.center();
        for pressed in [true, false] {
            let result = render_frame(
                ctx,
                state,
                module,
                model,
                parameter,
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
            if !pressed {
                return result;
            }
        }
        unreachable!()
    }
    #[test]
    fn synced_checkbox_opens_review_and_sends_only_after_explicit_apply() {
        let catalog = Catalog::embedded().unwrap();
        let model = catalog.model_by_id(8, 0x0b00000b).unwrap();
        let parameter = model.parameter(3).unwrap();
        let mut module = module(model);
        module.parameters[3] = F32Bits(1.0_f32.to_bits());
        module.parameters[1] = F32Bits(4.0_f32.to_bits());
        let ctx = egui::Context::default();
        let mut state = ControlState::default();
        let first = render_frame(&ctx, &mut state, &module, model, parameter, Vec::new());
        let clicked = click(
            &ctx,
            &mut state,
            &module,
            model,
            parameter,
            first.checkbox_rect.unwrap(),
        );
        assert!(clicked.patch.is_none());
        assert!(state.review.as_ref().unwrap().ordinary.is_empty());
        assert!(state.ordinary.is_empty());
        state.review.as_mut().unwrap().ordinary = "501.12345".into();
        let frame = render_frame(&ctx, &mut state, &module, model, parameter, Vec::new());
        let applied = click(
            &ctx,
            &mut state,
            &module,
            model,
            parameter,
            frame.review_apply_rect.unwrap(),
        );
        assert!(
            matches!(applied.patch, Some(EditPatch::SetSync { enabled: false, value_parameter, power_parameter, ordinary_restore: Some(bits), .. })
            if value_parameter.get() == 1 && power_parameter.get() == 3 && bits.0 == 501.12345_f32.to_bits())
        );
        assert!(state.review.is_none());
    }
    #[test]
    fn sync_off_restores_observed_float_bits_without_rounding() {
        let catalog = Catalog::embedded().unwrap();
        let model = catalog.model_by_id(7, 0x04000015).unwrap();
        let parameter = model.parameter(2).unwrap();
        let mut module = module(model);
        module.parameters[2] = F32Bits(1.0_f32.to_bits());
        module.parameters[1] = F32Bits(4.0_f32.to_bits());
        let original = F32Bits(0.713579_f32.to_bits());
        let mut state = ControlState::default();
        state.ordinary.insert(
            SyncKey {
                module: 7,
                model: model.code,
                power: 2,
                value: 1,
            },
            original,
        );
        let ctx = egui::Context::default();
        let first = render_frame(&ctx, &mut state, &module, model, parameter, Vec::new());
        let clicked = click(
            &ctx,
            &mut state,
            &module,
            model,
            parameter,
            first.checkbox_rect.unwrap(),
        );
        assert!(
            matches!(clicked.patch, Some(EditPatch::SetSync { enabled: false, ordinary_restore: Some(bits), .. }) if bits == original)
        );
        assert!(state.review.is_none());
    }
    #[test]
    fn all_declared_controls_render_without_generating_edits() {
        let catalog = Catalog::embedded().unwrap();
        let mut count = 0;
        for family in catalog.modules() {
            for model in &family.models {
                let module = module(model);
                for parameter in model.parameters_in_display_order() {
                    let result =
                        render_once(&mut ControlState::default(), &module, model, parameter);
                    assert!(
                        result.patch.is_none(),
                        "{} {} {} generated a passive edit",
                        family.name,
                        model.name,
                        parameter.name
                    );
                    assert!(result.error.is_none());
                    assert!(!result.started && !result.stopped);
                    count += 1;
                }
            }
        }
        assert_eq!(count, 1400);
    }
    #[test]
    fn physical_float_and_unknown_bits_survive_passive_rendering() {
        let catalog = Catalog::embedded().unwrap();
        let model = catalog.model_by_id(1, 0x0500000a).unwrap();
        let parameter = model.parameter(3).unwrap();
        let mut module = module(model);
        for bits in [
            0x42c6b5c9,
            99.355_05_f32.to_bits(),
            (-0.0_f32).to_bits(),
            f32::NAN.to_bits(),
            f32::INFINITY.to_bits(),
        ] {
            module.parameters[3] = F32Bits(bits);
            let original = module.parameters.clone();
            let result = render_once(&mut ControlState::default(), &module, model, parameter);
            assert!(
                result.patch.is_none(),
                "passive f32 bits {bits:08x} were normalized"
            );
            assert_eq!(module.parameters, original);
        }
    }
    #[test]
    fn sync_pairs_resolve_reverse_ids_and_reject_isolated_power() {
        let catalog = Catalog::embedded().unwrap();
        let mut pairs = 0;
        let mut shipped_defects = 0;
        for family in catalog.modules() {
            for model in &family.models {
                let mut module = module(model);
                for power in &model.parameters {
                    let ControlSemantics::SyncPower {
                        value_parameter: hint,
                    } = control_semantics(power)
                    else {
                        continue;
                    };
                    module.parameters[usize::from(power.parameter_id)] = F32Bits(1.0_f32.to_bits());
                    let value_id = sync_value_parameter(model, power.parameter_id).unwrap();
                    if hint != value_id {
                        shipped_defects += 1;
                    }
                    let numeric = model.parameter(value_id).unwrap();
                    let restore = numeric.default.or(numeric.minimum).unwrap();
                    let patch = patch_for_request(
                        &module,
                        model,
                        power,
                        ControlRequest::Sync {
                            enabled: false,
                            ordinary_restore: Some(OrdinaryRestore::ExplicitUserValue(
                                restore.to_bits(),
                            )),
                        },
                    )
                    .unwrap();
                    assert!(
                        matches!(patch, EditPatch::SetSync { power_parameter, value_parameter, ordinary_restore: Some(bits), .. }
                        if power_parameter.get() == power.parameter_id && value_parameter.get() == value_id && bits.0 == restore.to_bits())
                    );
                    assert!(
                        patch_for_request(
                            &module,
                            model,
                            power,
                            ControlRequest::RawBits(0.0_f32.to_bits())
                        )
                        .is_err()
                    );
                    assert!(
                        patch_for_request(
                            &module,
                            model,
                            power,
                            ControlRequest::Sync {
                                enabled: false,
                                ordinary_restore: None
                            }
                        )
                        .is_err()
                    );
                    pairs += 1;
                }
            }
        }
        assert_eq!(pairs, 49);
        assert_eq!(shipped_defects, 3);
    }
    #[test]
    fn sentinel_requests_emit_exact_catalog_bits_with_sparse_ids() {
        let catalog = Catalog::embedded().unwrap();
        let model = catalog.model_by_id(5, 0x0a000010).unwrap();
        let module = module(model);
        for (id, expected) in [(5, 19.0_f32), (6, 20001.0_f32)] {
            let parameter = model.parameter(id).unwrap();
            let patch = patch_for_request(
                &module,
                model,
                parameter,
                ControlRequest::Sentinel(SentinelKind::Off),
            )
            .unwrap();
            assert!(
                matches!(patch, EditPatch::SetParameter { parameter, value, .. } if parameter.get() == id && value.0 == expected.to_bits())
            );
            assert_eq!(
                parse_numeric(parameter, "OFF").unwrap().to_bits(),
                expected.to_bits()
            );
        }
    }
    #[test]
    fn ordinary_review_requires_a_finite_explicit_value_without_step_rounding() {
        let catalog = Catalog::embedded().unwrap();
        let model = catalog.model_by_id(7, 0x04000015).unwrap();
        let parameter = model.parameter(1).unwrap();
        for input in ["", "1/4", "OFF", "NaN", "inf", "-1", "10000"] {
            assert!(
                parse_ordinary(parameter, input).is_none(),
                "accepted {input}"
            );
        }
        let bits = parse_ordinary(parameter, "0.713579").unwrap();
        assert_eq!(bits.0, 0.713579_f32.to_bits());
    }
    #[test]
    fn sync_cache_never_guesses_initial_value_and_retires_on_context_changes() {
        let catalog = Catalog::embedded().unwrap();
        let model = catalog.model_by_id(7, 0x04000015).unwrap();
        let snapshot = gp200_sim::captured_demo_snapshot();
        let mut document = EditorDocument::new(
            DocumentId("control-test".into()),
            EditTarget::Offline(DocumentId("control-test".into())),
            snapshot,
        );
        let synced = document
            .draft
            .view
            .module(ModuleId::new(7).unwrap())
            .unwrap()
            .clone();
        let mut state = ControlState::default();
        state.observe(
            &document,
            SessionEpoch(1),
            document.baseline.context,
            &synced,
            model,
            Some(&synced),
        );
        assert!(state.ordinary.is_empty());
        let mut unsynced = synced.clone();
        unsynced.parameters[2] = F32Bits(0.0_f32.to_bits());
        unsynced.parameters[1] = F32Bits(0.713579_f32.to_bits());
        state.observe(
            &document,
            SessionEpoch(1),
            document.baseline.context,
            &unsynced,
            model,
            Some(&unsynced),
        );
        assert_eq!(
            state.ordinary.values().next().unwrap().0,
            0.713579_f32.to_bits()
        );
        state.observe(
            &document,
            SessionEpoch(1),
            document.baseline.context,
            &synced,
            model,
            Some(&synced),
        );
        assert_eq!(state.ordinary.len(), 1);
        state.observe(
            &document,
            SessionEpoch(2),
            document.baseline.context,
            &synced,
            model,
            Some(&synced),
        );
        assert!(state.ordinary.is_empty());
        state.observe(
            &document,
            SessionEpoch(2),
            document.baseline.context,
            &unsynced,
            model,
            Some(&unsynced),
        );
        document.baseline = Arc::new((*document.baseline).clone());
        state.observe(
            &document,
            SessionEpoch(2),
            document.baseline.context,
            &synced,
            model,
            Some(&synced),
        );
        assert!(state.ordinary.is_empty());
        state.observe(
            &document,
            SessionEpoch(2),
            document.baseline.context,
            &unsynced,
            model,
            Some(&unsynced),
        );
        let changed = Some(WireContext {
            group: 0,
            selector: 255,
        });
        state.observe(
            &document,
            SessionEpoch(2),
            changed,
            &synced,
            model,
            Some(&synced),
        );
        assert!(state.ordinary.is_empty());
    }
    #[test]
    fn hardware_profiles_are_precise_while_offline_and_simulator_are_local() {
        let catalog = Catalog::embedded().unwrap();
        let mut state = AppState::new(AppPreferences::default());
        state.connection = ConnectionState::Ready;
        state.identity = Some(DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Hardware,
            firmware: None,
            display_name: "GP-200".into(),
        });
        state.capabilities = gp200_transport::verified_capabilities();
        for (id, code, parameter_id) in [
            (7, 0x04000015, 2),
            (8, 0x0b00000b, 1),
            (5, 0x0a000010, 6),
            (1, 0x0500000a, 4),
        ] {
            let model = catalog.model_by_id(id, code).unwrap();
            let module = module(model);
            assert!(
                blocked_reason(
                    &state,
                    &EditTarget::DeviceBuffer,
                    &module,
                    model,
                    model.parameter(parameter_id).unwrap(),
                    true
                )
                .is_none()
            );
        }
        let model = catalog
            .modules()
            .iter()
            .flat_map(|module| &module.models)
            .find(|model| {
                model
                    .parameters
                    .iter()
                    .any(|parameter| control_semantics(parameter) == ControlSemantics::Choice)
            })
            .unwrap();
        let module = module(model);
        let parameter = model
            .parameters
            .iter()
            .find(|parameter| control_semantics(parameter) == ControlSemantics::Choice)
            .unwrap();
        assert!(
            blocked_reason(
                &state,
                &EditTarget::DeviceBuffer,
                &module,
                model,
                parameter,
                true
            )
            .is_some()
        );
        assert!(
            blocked_reason(
                &state,
                &EditTarget::Offline(DocumentId("offline".into())),
                &module,
                model,
                parameter,
                true
            )
            .is_none()
        );
        state.identity.as_mut().unwrap().mode = BackendMode::Simulator;
        state.capabilities = Capabilities::simulator();
        assert!(
            blocked_reason(
                &state,
                &EditTarget::DeviceBuffer,
                &module,
                model,
                parameter,
                true
            )
            .is_none()
        );
        assert!(
            blocked_reason(
                &state,
                &EditTarget::DeviceBuffer,
                &module,
                model,
                parameter,
                false
            )
            .is_some()
        );
        state.identity.as_mut().unwrap().model = DeviceModel::Gp200Lt;
        assert!(
            blocked_reason(
                &state,
                &EditTarget::DeviceBuffer,
                &module,
                model,
                parameter,
                true
            )
            .is_some()
        );
    }
}
