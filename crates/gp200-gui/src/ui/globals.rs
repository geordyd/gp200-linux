//! Device-wide controls staged independently of the preset editor.
use super::section;
use crate::{StudioApp, theme::Palette};
use eframe::egui::{self, RichText};
use gp200_app::AppState;
use gp200_domain::*;
use gp200_protocol::{
    DrumSetting, EqParameter, GLOBAL_FOOTSWITCH_EVENT_NAMES, GLOBAL_FOOTSWITCH_FUNCTION_NAMES,
    GeneralSetting, GlobalFootswitchEvent, GlobalFootswitchFunction, GlobalFootswitchTableId,
    gp200_footswitch_choices,
};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GlobalTab {
    #[default]
    Audio,
    Midi,
    External,
    Footswitch,
    Eq,
    Display,
}
impl GlobalTab {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "audio" => Some(Self::Audio),
            "midi" => Some(Self::Midi),
            "external" => Some(Self::External),
            "footswitch" => Some(Self::Footswitch),
            "eq" => Some(Self::Eq),
            "display" => Some(Self::Display),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct EqDraft {
    text: String,
    original: String,
    bits: F32Bits,
}
impl EqDraft {
    fn new(bits: F32Bits) -> Self {
        let text = bits.as_f32().to_string();
        Self {
            original: text.clone(),
            text,
            bits,
        }
    }
    fn proposal(&self, parameter: EqParameter) -> Result<Option<GlobalControlEdit>, String> {
        // Formatting a retained NaN, negative zero or off-grid float never
        // rewrites it. Parsing happens only after the user edits the text.
        if self.text == self.original {
            return Ok(None);
        }
        let value = self
            .text
            .trim()
            .parse::<f32>()
            .map_err(|_| crate::localization::text("Enter a finite number.").to_owned())?;
        parameter.validate(value).map_err(|_| {
            let (minimum, maximum) = parameter.bounds();
            std::format!("Enter a finite value from {minimum} to {maximum}.")
        })?;
        let value = F32Bits::from_f32(value);
        Ok((value != self.bits).then_some(GlobalControlEdit::Eq { parameter, value }))
    }
}

pub(crate) struct GlobalUiState {
    pub(crate) tab: GlobalTab,
    baseline: Option<Arc<DeviceGlobalState>>,
    general: BTreeMap<GeneralSetting, i16>,
    eq: BTreeMap<EqParameter, EqDraft>,
    switches: BTreeMap<(u16, u8), u8>,
    pub(super) drums: [u16; 3],
    pub(super) drum_group: usize,
    footswitch_table: u16,
    eq_section: usize,
}
impl Default for GlobalUiState {
    fn default() -> Self {
        Self {
            tab: GlobalTab::Audio,
            baseline: None,
            general: BTreeMap::new(),
            eq: BTreeMap::new(),
            switches: BTreeMap::new(),
            drums: [0, 0, 120],
            drum_group: 0,
            footswitch_table: 0,
            eq_section: 0,
        }
    }
}
impl GlobalUiState {
    pub(super) fn observe(&mut self, app: &AppState) {
        let baseline = app
            .device_globals
            .clone()
            .filter(|state| app.connection == ConnectionState::Ready && state.epoch == app.epoch);
        if self.baseline == baseline {
            return;
        }
        self.baseline = baseline.clone();
        self.general.clear();
        self.eq.clear();
        self.switches.clear();
        self.drums = [0, 0, 120];
        self.drum_group = 0;
        let Some(baseline) = baseline else {
            return;
        };
        if let Some(general) = &baseline.records().general {
            for field in GeneralSetting::ALL {
                self.general.insert(field, general.value(field));
            }
        }
        if let Some(eq) = &baseline.records().eq {
            for parameter in EqParameter::ALL {
                self.eq.insert(
                    parameter,
                    EqDraft::new(eq.values[parameter.index() as usize]),
                );
            }
        }
        for table in &baseline.records().footswitch_tables {
            for event in 0..20 {
                self.switches.insert(
                    (table.table_raw(), event),
                    table.function_raw(GlobalFootswitchEvent::new(event).unwrap()),
                );
            }
        }
        if let Some(drums) = &baseline.records().drum {
            self.drums = [
                drums.value(DrumSetting::Pattern),
                drums.value(DrumSetting::Volume),
                drums.value(DrumSetting::Tempo),
            ];
            self.drum_group = DRUM_PATTERN_GROUPS
                .iter()
                .position(|group| (group.first..group.first + group.count).contains(&self.drums[0]))
                .unwrap_or(0);
        }
    }
}

#[derive(Default)]
pub(super) struct WidgetOutput {
    pub(super) action: Option<UiIntent>,
    pub(super) apply_rects: Vec<(String, egui::Rect)>,
    pub(super) refresh_rect: Option<egui::Rect>,
    pub(super) play_rect: Option<egui::Rect>,
}

impl StudioApp {
    pub(super) fn globals(&mut self, ui: &mut egui::Ui) {
        ui.heading(crate::localization::text("Device settings"));
        ui.label(crate::localization::text(
            "Audio, MIDI and switch settings shared by every preset.",
        ));
        let output = render(
            ui,
            &mut self.global_state,
            self.controller.state(),
            self.controller.device_operation_pending(),
        );
        if let Some(action) = output.action {
            self.send(action);
        }
        section(ui, crate::localization::text("USB connection"));
        ui.label(crate::localization::text("Use Normal USB mode for MIDI editing. Select the separate GP-200 audio device in your Linux recording application."));
        if ui
            .button(crate::localization::text("Open firmware workspace"))
            .clicked()
        {
            self.selected_asset_kind = AssetKind::Firmware;
            self.send(UiIntent::Navigate(ViewId::Assets));
        }
    }
}

pub(super) fn render(
    ui: &mut egui::Ui,
    state: &mut GlobalUiState,
    app: &AppState,
    pending: bool,
) -> WidgetOutput {
    state.observe(app);
    let mut output = WidgetOutput::default();
    header(ui, app, pending, &mut output);
    ui.add_space(12.0);
    ui.horizontal_wrapped(|ui| {
        for (tab, label) in [
            (GlobalTab::Audio, crate::localization::text("Audio routing")),
            (GlobalTab::Midi, crate::localization::text("MIDI")),
            (
                GlobalTab::External,
                crate::localization::text("External controls"),
            ),
            (
                GlobalTab::Footswitch,
                crate::localization::text("Footswitches"),
            ),
            (GlobalTab::Eq, crate::localization::text("Global EQ")),
            (GlobalTab::Display, crate::localization::text("Display")),
        ] {
            ui.selectable_value(&mut state.tab, tab, crate::localization::text(label));
        }
    });
    ui.separator();
    let Some(baseline) = state.baseline.clone() else {
        ui.add_space(12.0);
        ui.label(crate::localization::text(
            "Connect the pedal, then read its global settings to inspect or change a field.",
        ));
        return output;
    };
    match state.tab {
        GlobalTab::Audio => {
            section(ui, crate::localization::text("Input and cabinet routing"));
            general_rows(
                ui,
                state,
                app,
                pending,
                &baseline,
                &[
                    GeneralSetting::InputLevel,
                    GeneralSetting::InputMode,
                    GeneralSetting::NoCabLeft,
                    GeneralSetting::NoCabRight,
                    GeneralSetting::AutoCabMatch,
                ],
                &mut output,
            );
            section(ui, crate::localization::text("USB recording and monitor"));
            general_rows(
                ui,
                state,
                app,
                pending,
                &baseline,
                &[
                    GeneralSetting::RecordingLevel,
                    GeneralSetting::RecordingModeLeft,
                    GeneralSetting::RecordingModeRight,
                    GeneralSetting::AuxToUsb,
                    GeneralSetting::MonitorLevel,
                ],
                &mut output,
            );
        }
        GlobalTab::Midi => {
            section(ui, crate::localization::text("Input and output channels"));
            general_rows(
                ui,
                state,
                app,
                pending,
                &baseline,
                &[
                    GeneralSetting::MidiInputSource,
                    GeneralSetting::MidiInputUsb,
                    GeneralSetting::MidiInputDin,
                    GeneralSetting::MidiOutputUsb,
                    GeneralSetting::MidiOutputDin,
                ],
                &mut output,
            );
            section(ui, crate::localization::text("Clock routing"));
            general_rows(
                ui,
                state,
                app,
                pending,
                &baseline,
                &[
                    GeneralSetting::MidiClockSource,
                    GeneralSetting::MidiClockOutUsb,
                    GeneralSetting::MidiClockOutDin,
                ],
                &mut output,
            );
        }
        GlobalTab::External => {
            section(ui, crate::localization::text("EXP2 and external switches"));
            general_rows(
                ui,
                state,
                app,
                pending,
                &baseline,
                &[
                    GeneralSetting::ExternalMode,
                    GeneralSetting::ExternalFootswitch1,
                    GeneralSetting::ExternalFootswitch2,
                ],
                &mut output,
            );
            ui.small(crate::localization::text("Expression targets and heel/toe ranges belong to each patch. Open Assignments to edit them."));
        }
        GlobalTab::Footswitch => footswitches(ui, state, app, pending, &baseline, &mut output),
        GlobalTab::Eq => equalizer(ui, state, app, pending, &mut output),
        GlobalTab::Display => {
            section(ui, crate::localization::text("Pedal display"));
            general_rows(
                ui,
                state,
                app,
                pending,
                &baseline,
                &[GeneralSetting::DisplayMode],
                &mut output,
            );
            if let Some(identity) = &app.identity {
                section(ui, crate::localization::text("Connected device"));
                ui.label(&identity.display_name);
                ui.small(identity.firmware.as_ref().map_or_else(
                    || {
                        crate::localization::text(
                            "Firmware version has not been read from the pedal.",
                        )
                        .into()
                    },
                    |firmware| std::format!("Firmware {}", firmware.version),
                ));
            }
        }
    }
    egui::CollapsingHeader::new(crate::localization::text("Retained device records")).show(
        ui,
        |ui| {
            ui.small(std::format!(
                "{} original records; unknown bytes stay in the read baseline.",
                baseline.records().raw().records().len()
            ));
            for record in baseline.records().raw().records() {
                ui.small(std::format!(
                    "Command {:04x}: {} bytes",
                    record.command,
                    record.body.len()
                ));
            }
        },
    );
    output
}

pub(super) fn header(ui: &mut egui::Ui, app: &AppState, pending: bool, output: &mut WidgetOutput) {
    let palette = Palette::for_context(ui.ctx());
    ui.horizontal_wrapped(|ui| {
        let reason = read_blocked_reason(app, pending);
        let response = ui.add_enabled(
            reason.is_none(),
            egui::Button::new(crate::localization::text("Read global settings")),
        );
        output.refresh_rect = Some(response.rect);
        if response.clicked() {
            output.action = Some(UiIntent::RefreshDeviceGlobals);
        }
        if let Some(reason) = reason {
            response.on_hover_text(crate::localization::message(&reason));
        }
        if pending {
            ui.spinner();
            ui.label(crate::localization::text("Waiting for device readback…"));
        } else if let Some(baseline) = &app.device_globals
            && baseline.epoch == app.epoch
            && app.connection == ConnectionState::Ready
        {
            ui.label(
                RichText::new(std::format!("Read baseline {}", baseline.revision))
                    .color(palette.accent),
            );
        }
    });
    if app
        .identity
        .as_ref()
        .is_some_and(|identity| identity.mode == BackendMode::Simulator)
    {
        ui.small(crate::localization::text(
            "Simulator settings use the same wire codec. Changes stay in the simulated pedal.",
        ));
    } else {
        ui.small(crate::localization::text("Apply changes one field, then reads the complete device state. General and EQ controls have verified GP-200 profiles; switch mappings use a separate profile."));
    }
    if let Some(message) = app
        .messages
        .back()
        .filter(|message| message.severity == gp200_app::MessageSeverity::Error)
    {
        ui.colored_label(
            palette.warning,
            crate::localization::message(&message.message),
        );
    }
}

pub(super) fn read_blocked_reason(app: &AppState, pending: bool) -> Option<String> {
    if pending {
        return Some(crate::localization::text("Wait for the pending device request.").into());
    }
    if app.connection != ConnectionState::Ready {
        return Some(crate::localization::text("Connect a pedal or simulator first.").into());
    }
    let Some(identity) = app.identity.as_ref() else {
        return Some(crate::localization::text("Read the connected device identity first.").into());
    };
    if !app
        .capabilities
        .allows(OperationKind::ReadGlobals, identity)
    {
        return Some(
            crate::localization::text("This connection has no validated global read profile.")
                .into(),
        );
    }
    None
}

pub(super) fn blocked_reason(
    app: &AppState,
    pending: bool,
    edit: &GlobalControlEdit,
) -> Option<String> {
    if pending {
        return Some(
            crate::localization::text(
                "Wait for the pending device readback before applying another field.",
            )
            .into(),
        );
    }
    if app.connection != ConnectionState::Ready {
        return Some(
            crate::localization::text("Connect a pedal or simulator before applying settings.")
                .into(),
        );
    }
    let Some(identity) = &app.identity else {
        return Some(crate::localization::text("Read the connected device identity first.").into());
    };
    if !app
        .capabilities
        .allows(OperationKind::WriteGlobals, identity)
    {
        return Some(
            crate::localization::text("This connection has no validated global write profile.")
                .into(),
        );
    }
    let Some(baseline) = app
        .device_globals
        .as_ref()
        .filter(|state| state.epoch == app.epoch)
    else {
        return Some(
            crate::localization::text(
                "Read global settings in this connection before applying a change.",
            )
            .into(),
        );
    };
    if let Err(error) = baseline.expected_after(edit) {
        return Some(error.to_string());
    }
    match identity.mode {
        BackendMode::Simulator => None,
        BackendMode::CaptureReplay => {
            Some(crate::localization::text("Capture replay is read only.").into())
        }
        BackendMode::Hardware if identity.model != DeviceModel::Gp200 => Some(
            crate::localization::text(
                "The physical global-settings profile is currently validated for GP-200.",
            )
            .into(),
        ),
        BackendMode::Hardware if gp200_transport::hardware_global_verified(edit, baseline) => None,
        BackendMode::Hardware => Some(match edit {
            GlobalControlEdit::General { field, .. } => std::format!(
                "{} has a recovered protocol but still needs a hardware roundtrip.",
                field.label()
            ),
            GlobalControlEdit::Eq { parameter, .. } => {
                std::format!("{} has no matching hardware profile.", eq_label(*parameter))
            }
            GlobalControlEdit::Drum { .. } => crate::localization::text(
                "Stop the pedal drums before changing pattern, tempo or volume.",
            )
            .into(),
            GlobalControlEdit::DrumPlay(_) => crate::localization::text(
                "Read a known drum playback state before starting or stopping.",
            )
            .into(),
            GlobalControlEdit::Footswitch { .. } => {
                ValidatedInactiveFootswitchEdit::validate(edit, baseline)
                    .err()
                    .map_or_else(
                        || {
                            crate::localization::text(
                                "This mapping has no matching storage profile.",
                            )
                            .into()
                        },
                        |error| error.to_string(),
                    )
            }
        }),
    }
}

fn general_rows(
    ui: &mut egui::Ui,
    state: &mut GlobalUiState,
    app: &AppState,
    pending: bool,
    baseline: &DeviceGlobalState,
    fields: &[GeneralSetting],
    output: &mut WidgetOutput,
) {
    let Some(general) = &baseline.records().general else {
        return;
    };
    for &field in fields {
        ui.push_id(field, |ui| {
            let Some(value) = state.general.get_mut(&field) else {
                return;
            };
            ui.horizontal_wrapped(|ui| {
                field_label(ui, 225.0, field.label());
                general_widget(ui, field, value);
                let edit = GlobalControlEdit::General {
                    field,
                    value: *value,
                };
                let proposal = if *value == general.value(field) {
                    Ok(None)
                } else {
                    field
                        .validate(*value)
                        .map(|_| Some(edit.clone()))
                        .map_err(|error| error.to_string())
                };
                apply(ui, app, pending, field.label(), &edit, proposal, output);
            });
        });
    }
}

pub(super) fn field_label(ui: &mut egui::Ui, width: f32, label: &str) {
    let width = width.min(ui.available_width());
    ui.allocate_ui_with_layout(
        egui::vec2(width, 29.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_width(width);
            ui.label(crate::localization::text(label));
        },
    );
}

fn general_widget(ui: &mut egui::Ui, field: GeneralSetting, value: &mut i16) {
    if matches!(
        field,
        GeneralSetting::InputLevel | GeneralSetting::RecordingLevel | GeneralSetting::MonitorLevel
    ) {
        ui.add(
            egui::DragValue::new(value)
                .range(-20..=20)
                .clamp_existing_to_range(false)
                .suffix(crate::localization::text(" dB")),
        );
    } else {
        egui::ComboBox::from_id_salt("value")
            .width(185.0)
            .selected_text(general_value_label(field, *value))
            .show_ui(ui, |ui| {
                if !field.choices().is_empty() {
                    for (index, label) in field.choices().iter().enumerate() {
                        ui.selectable_value(value, index as i16, crate::localization::text(label));
                    }
                } else if matches!(
                    field,
                    GeneralSetting::MidiInputUsb
                        | GeneralSetting::MidiInputDin
                        | GeneralSetting::MidiOutputUsb
                        | GeneralSetting::MidiOutputDin
                ) {
                    let minimum = if matches!(
                        field,
                        GeneralSetting::MidiOutputUsb | GeneralSetting::MidiOutputDin
                    ) {
                        -1
                    } else {
                        0
                    };
                    for channel in minimum..=16 {
                        ui.selectable_value(value, channel, general_value_label(field, channel));
                    }
                } else {
                    ui.selectable_value(value, 0, crate::localization::text("Off"));
                    ui.selectable_value(value, 1, crate::localization::text("On"));
                }
            });
    }
}
fn general_value_label(field: GeneralSetting, value: i16) -> String {
    if !field.choices().is_empty() {
        return field.choices().get(value as usize).map_or_else(
            || std::format!("Unknown ({value})"),
            |label| crate::localization::text(label).into(),
        );
    }
    if matches!(
        field,
        GeneralSetting::MidiInputUsb
            | GeneralSetting::MidiInputDin
            | GeneralSetting::MidiOutputUsb
            | GeneralSetting::MidiOutputDin
    ) {
        return match value {
            -1 => crate::localization::text("Off").into(),
            0 => crate::localization::text("Omni").into(),
            1..=16 => std::format!("Channel {value}"),
            _ => std::format!("Unknown ({value})"),
        };
    }
    match value {
        0 => crate::localization::text("Off").into(),
        1 => crate::localization::text("On").into(),
        _ => std::format!("Unknown ({value})"),
    }
}

fn equalizer(
    ui: &mut egui::Ui,
    state: &mut GlobalUiState,
    app: &AppState,
    pending: bool,
    output: &mut WidgetOutput,
) {
    section(ui, crate::localization::text("Output EQ"));
    ui.label(crate::localization::text("This equalizer applies across patches and is separate from the EQ block in the signal chain."));
    ui.horizontal_wrapped(|ui| {
        for (index, label) in [
            crate::localization::text("Filters / level"),
            crate::localization::text("Band 1"),
            crate::localization::text("Band 2"),
            crate::localization::text("Band 3"),
            crate::localization::text("Band 4"),
            crate::localization::text("Low shelf (extra)"),
            crate::localization::text("High shelf (extra)"),
        ]
        .into_iter()
        .enumerate()
        {
            ui.selectable_value(
                &mut state.eq_section,
                index,
                crate::localization::text(label),
            );
        }
    });
    let parameters: &[EqParameter] = match state.eq_section {
        1 => &[
            EqParameter::Band1Enabled,
            EqParameter::Band1Frequency,
            EqParameter::Band1Q,
            EqParameter::Band1Gain,
        ],
        2 => &[
            EqParameter::Band2Enabled,
            EqParameter::Band2Frequency,
            EqParameter::Band2Q,
            EqParameter::Band2Gain,
        ],
        3 => &[
            EqParameter::Band3Enabled,
            EqParameter::Band3Frequency,
            EqParameter::Band3Q,
            EqParameter::Band3Gain,
        ],
        4 => &[
            EqParameter::Band4Enabled,
            EqParameter::Band4Frequency,
            EqParameter::Band4Q,
            EqParameter::Band4Gain,
        ],
        5 => &[
            EqParameter::LowShelfEnabled,
            EqParameter::LowShelfFrequency,
            EqParameter::LowShelfQ,
            EqParameter::LowShelfGain,
        ],
        6 => &[
            EqParameter::HighShelfEnabled,
            EqParameter::HighShelfFrequency,
            EqParameter::HighShelfQ,
            EqParameter::HighShelfGain,
        ],
        _ => &[
            EqParameter::LowCutEnabled,
            EqParameter::LowCutFrequency,
            EqParameter::LowCutQ,
            EqParameter::HighCutEnabled,
            EqParameter::HighCutFrequency,
            EqParameter::HighCutQ,
            EqParameter::Level,
        ],
    };
    if matches!(state.eq_section, 5 | 6) {
        ui.small(crate::localization::text("Extra controls: the pedal stores these shelf fields and verified edits work through the same EQ protocol. The original desktop page does not expose them."));
    }
    ui.add_space(12.0);
    for &parameter in parameters {
        eq_row(ui, state, app, pending, parameter, output);
    }
}

fn eq_row(
    ui: &mut egui::Ui,
    state: &mut GlobalUiState,
    app: &AppState,
    pending: bool,
    parameter: EqParameter,
    output: &mut WidgetOutput,
) {
    let Some(value) = state.eq.get_mut(&parameter) else {
        return;
    };
    ui.push_id(parameter, |ui| {
        ui.horizontal_wrapped(|ui| {
            field_label(ui, 200.0, eq_label(parameter));
            if parameter.is_switch() {
                let current = value.text.parse::<f32>().ok();
                egui::ComboBox::from_id_salt("switch")
                    .width(100.0)
                    .selected_text(match current {
                        Some(0.0) => crate::localization::text("Off").into(),
                        Some(1.0) => crate::localization::text("On").into(),
                        _ => std::format!("Unknown ({})", value.text),
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut value.text,
                            "0".into(),
                            crate::localization::text("Off"),
                        );
                        ui.selectable_value(
                            &mut value.text,
                            "1".into(),
                            crate::localization::text("On"),
                        );
                    });
            } else {
                ui.add(egui::TextEdit::singleline(&mut value.text).desired_width(100.0))
                    .on_hover_text(std::format!("Retained float bits {:08x}", value.bits.0));
                let unit = eq_unit(parameter);
                if !unit.is_empty() {
                    ui.small(unit);
                }
                if matches!(
                    parameter,
                    EqParameter::LowCutFrequency | EqParameter::HighCutFrequency
                ) {
                    if ui.small_button(crate::localization::text("Off")).clicked() {
                        value.text = if parameter == EqParameter::LowCutFrequency {
                            "19"
                        } else {
                            "20001"
                        }
                        .into();
                    }
                    if (parameter == EqParameter::LowCutFrequency && value.text == "19")
                        || (parameter == EqParameter::HighCutFrequency && value.text == "20001")
                    {
                        ui.small(crate::localization::text("Off"));
                    }
                }
            }
            let inspection = GlobalControlEdit::Eq {
                parameter,
                value: value.bits,
            };
            apply(
                ui,
                app,
                pending,
                eq_label(parameter),
                &inspection,
                value.proposal(parameter),
                output,
            );
        });
    });
}
fn eq_label(parameter: EqParameter) -> &'static str {
    match parameter {
        EqParameter::LowCutEnabled => crate::localization::text("Low cut enabled"),
        EqParameter::LowCutFrequency => crate::localization::text("Low cut frequency"),
        EqParameter::LowCutQ => crate::localization::text("Low cut Q"),
        EqParameter::LowShelfEnabled => crate::localization::text("Low shelf enabled"),
        EqParameter::LowShelfFrequency => crate::localization::text("Low shelf frequency"),
        EqParameter::LowShelfQ => crate::localization::text("Low shelf Q"),
        EqParameter::LowShelfGain => crate::localization::text("Low shelf gain"),
        EqParameter::Band1Enabled => crate::localization::text("Band 1 enabled"),
        EqParameter::Band1Frequency => crate::localization::text("Band 1 frequency"),
        EqParameter::Band1Q => crate::localization::text("Band 1 Q"),
        EqParameter::Band1Gain => crate::localization::text("Band 1 gain"),
        EqParameter::Band2Enabled => crate::localization::text("Band 2 enabled"),
        EqParameter::Band2Frequency => crate::localization::text("Band 2 frequency"),
        EqParameter::Band2Q => crate::localization::text("Band 2 Q"),
        EqParameter::Band2Gain => crate::localization::text("Band 2 gain"),
        EqParameter::Band3Enabled => crate::localization::text("Band 3 enabled"),
        EqParameter::Band3Frequency => crate::localization::text("Band 3 frequency"),
        EqParameter::Band3Q => crate::localization::text("Band 3 Q"),
        EqParameter::Band3Gain => crate::localization::text("Band 3 gain"),
        EqParameter::Band4Enabled => crate::localization::text("Band 4 enabled"),
        EqParameter::Band4Frequency => crate::localization::text("Band 4 frequency"),
        EqParameter::Band4Q => crate::localization::text("Band 4 Q"),
        EqParameter::Band4Gain => crate::localization::text("Band 4 gain"),
        EqParameter::HighShelfEnabled => crate::localization::text("High shelf enabled"),
        EqParameter::HighShelfFrequency => crate::localization::text("High shelf frequency"),
        EqParameter::HighShelfQ => crate::localization::text("High shelf Q"),
        EqParameter::HighShelfGain => crate::localization::text("High shelf gain"),
        EqParameter::HighCutEnabled => crate::localization::text("High cut enabled"),
        EqParameter::HighCutFrequency => crate::localization::text("High cut frequency"),
        EqParameter::HighCutQ => crate::localization::text("High cut Q"),
        EqParameter::Level => crate::localization::text("Output level"),
    }
}
fn eq_unit(parameter: EqParameter) -> &'static str {
    match parameter {
        EqParameter::LowCutFrequency
        | EqParameter::LowShelfFrequency
        | EqParameter::Band1Frequency
        | EqParameter::Band2Frequency
        | EqParameter::Band3Frequency
        | EqParameter::Band4Frequency
        | EqParameter::HighShelfFrequency
        | EqParameter::HighCutFrequency => "Hz",
        EqParameter::LowShelfGain
        | EqParameter::Band1Gain
        | EqParameter::Band2Gain
        | EqParameter::Band3Gain
        | EqParameter::Band4Gain
        | EqParameter::HighShelfGain => "dB",
        EqParameter::Level => "%",
        _ => "",
    }
}

fn footswitches(
    ui: &mut egui::Ui,
    state: &mut GlobalUiState,
    app: &AppState,
    pending: bool,
    baseline: &DeviceGlobalState,
    output: &mut WidgetOutput,
) {
    section(ui, crate::localization::text("Active switch mode"));
    general_rows(
        ui,
        state,
        app,
        pending,
        baseline,
        &[
            GeneralSetting::FootswitchMode,
            GeneralSetting::FootswitchTemplate,
        ],
        output,
    );
    section(ui, crate::localization::text("Template mappings"));
    ui.horizontal_wrapped(|ui| {
        for (table, label) in [
            (0, crate::localization::text("Preset")),
            (1, crate::localization::text("Stomp")),
            (2, crate::localization::text("User")),
        ] {
            ui.selectable_value(
                &mut state.footswitch_table,
                table,
                crate::localization::text(label),
            );
        }
    });
    let table = GlobalFootswitchTableId::new(state.footswitch_table).unwrap();
    let active_mode = baseline
        .records()
        .general
        .as_ref()
        .map(|general| general.value(GeneralSetting::FootswitchMode));
    let mode_name = match active_mode {
        Some(0) => crate::localization::text("Preset"),
        Some(1) => crate::localization::text("Stomp"),
        Some(2) => crate::localization::text("User"),
        _ => crate::localization::text("Unknown"),
    };
    ui.label(std::format!("Pedal's current switch mode: {mode_name}"));
    if app
        .identity
        .as_ref()
        .is_some_and(|identity| identity.mode == BackendMode::Hardware)
    {
        ui.label(crate::localization::text("Apply is available for inactive tables. Each change reads all settings again and verifies the stored mapping."));
        ui.small(crate::localization::text("Storage has been tested; switch actions and power-cycle persistence are still unmeasured."));
    }
    let Some(baseline_table) = baseline.records().footswitch_table(table) else {
        ui.label(crate::localization::text(
            "This template was absent from the retained global response.",
        ));
        return;
    };
    for event_value in 0..20 {
        let event = GlobalFootswitchEvent::new(event_value).unwrap();
        let Some(value) = state.switches.get_mut(&(table.value(), event_value)) else {
            continue;
        };
        ui.push_id((table.value(), event_value), |ui| {
            ui.horizontal_wrapped(|ui| {
                field_label(
                    ui,
                    150.0,
                    GLOBAL_FOOTSWITCH_EVENT_NAMES[event_value as usize],
                );
                egui::ComboBox::from_id_salt("function")
                    .width(155.0)
                    .selected_text(
                        GLOBAL_FOOTSWITCH_FUNCTION_NAMES
                            .get(*value as usize)
                            .map_or_else(
                                || std::format!("Unknown ({value})"),
                                |name| crate::localization::text(name).into(),
                            ),
                    )
                    .show_ui(ui, |ui| {
                        for &function in gp200_footswitch_choices(event) {
                            ui.selectable_value(
                                value,
                                function,
                                crate::localization::text(
                                    GLOBAL_FOOTSWITCH_FUNCTION_NAMES[function as usize],
                                ),
                            );
                        }
                    });
                let proposal = if *value == baseline_table.function_raw(event) {
                    Ok(None)
                } else {
                    GlobalFootswitchFunction::new(*value)
                        .map(|function| {
                            Some(GlobalControlEdit::Footswitch {
                                table,
                                event,
                                function,
                            })
                        })
                        .map_err(|error| error.to_string())
                };
                let inspection = GlobalControlEdit::Footswitch {
                    table,
                    event,
                    function: GlobalFootswitchFunction::new(*value)
                        .unwrap_or_else(|_| GlobalFootswitchFunction::new(0).unwrap()),
                };
                apply(
                    ui,
                    app,
                    pending,
                    GLOBAL_FOOTSWITCH_EVENT_NAMES[event_value as usize],
                    &inspection,
                    proposal,
                    output,
                );
            });
        });
    }
}

pub(super) fn apply(
    ui: &mut egui::Ui,
    app: &AppState,
    pending: bool,
    label: &str,
    inspection: &GlobalControlEdit,
    proposal: Result<Option<GlobalControlEdit>, String>,
    output: &mut WidgetOutput,
) {
    let (edit, reason) = match proposal {
        Ok(Some(edit)) => {
            let reason = blocked_reason(app, pending, &edit);
            (Some(edit), reason)
        }
        Ok(None) => (None, blocked_reason(app, pending, inspection)),
        Err(reason) => (None, Some(reason)),
    };
    let response = ui.add_enabled(
        edit.is_some() && reason.is_none() && output.action.is_none(),
        egui::Button::new(crate::localization::text("Apply")),
    );
    output.apply_rects.push((label.into(), response.rect));
    if response.clicked()
        && let Some(edit) = edit
    {
        output.action = Some(UiIntent::ApplyGlobalControlEdit(edit));
    }
    if let Some(reason) = reason {
        response.on_hover_text(crate::localization::message(&reason));
        ui.label(
            RichText::new(crate::localization::text("Read only"))
                .small()
                .color(Palette::for_context(ui.ctx()).muted),
        )
        .on_hover_text(crate::localization::message(&reason));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gp200_protocol::{RawTransfer, WRITE_KIND};

    fn baseline() -> Arc<DeviceGlobalState> {
        Arc::new(
            DeviceGlobalState::from_transfer(
                Revision(10),
                SessionEpoch(4),
                RawTransfer::from_payload(
                    WRITE_KIND,
                    include_bytes!("../../../../tests/fixtures/globals/baseline.payload.bin")
                        .to_vec(),
                )
                .unwrap(),
                vec![],
            )
            .unwrap(),
        )
    }
    fn app(mode: BackendMode) -> AppState {
        let mut app = gp200_app::AppController::with_demo(
            gp200_sim::captured_demo_snapshot(),
            AppPreferences::default(),
        )
        .unwrap()
        .state()
        .clone();
        app.connection = ConnectionState::Ready;
        app.epoch = SessionEpoch(4);
        app.identity = Some(DeviceIdentity {
            model: DeviceModel::Gp200,
            mode,
            firmware: None,
            display_name: "GP-200 test".into(),
        });
        app.capabilities = gp200_transport::capabilities_for_mode(mode);
        app.device_globals = Some(baseline());
        app
    }
    fn frame(
        ctx: &egui::Context,
        state: &mut GlobalUiState,
        app: &AppState,
        practice: bool,
        pending: bool,
        events: Vec<egui::Event>,
        size: [f32; 2],
    ) -> WidgetOutput {
        let mut output = WidgetOutput::default();
        let mut frame = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(size[0], size[1]),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        output = if practice {
                            super::super::practice::render(ui, state, app, pending)
                        } else {
                            render(ui, state, app, pending)
                        };
                    });
                });
            },
        );
        frame.textures_delta.clear();
        output
    }
    fn click(
        ctx: &egui::Context,
        state: &mut GlobalUiState,
        app: &AppState,
        practice: bool,
        pending: bool,
        rect: egui::Rect,
    ) -> WidgetOutput {
        let point = rect.center();
        for pressed in [true, false] {
            let output = frame(
                ctx,
                state,
                app,
                practice,
                pending,
                vec![
                    egui::Event::PointerMoved(point),
                    egui::Event::PointerButton {
                        pos: point,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                [1280.0, 900.0],
            );
            if !pressed {
                return output;
            }
        }
        unreachable!()
    }
    #[test]
    fn every_typed_page_renders_in_both_themes_without_passive_writes() {
        let app = app(BackendMode::Hardware);
        let original = app.device_globals.as_ref().unwrap().records().raw().clone();
        for size in [[1280.0, 800.0], [640.0, 480.0]] {
            for theme in [ThemePreference::Dark, ThemePreference::Light] {
                let ctx = egui::Context::default();
                crate::theme::apply(&ctx, theme, 1.0);
                let mut state = GlobalUiState::default();
                for tab in [
                    GlobalTab::Audio,
                    GlobalTab::Midi,
                    GlobalTab::External,
                    GlobalTab::Footswitch,
                    GlobalTab::Eq,
                    GlobalTab::Display,
                ] {
                    state.tab = tab;
                    for _ in 0..3 {
                        assert!(
                            frame(&ctx, &mut state, &app, false, false, vec![], size)
                                .action
                                .is_none()
                        );
                    }
                }
                for group in 0..8 {
                    state.drum_group = group;
                    assert!(
                        frame(&ctx, &mut state, &app, true, false, vec![], size)
                            .action
                            .is_none()
                    );
                }
            }
        }
        assert_eq!(
            app.device_globals.as_ref().unwrap().records().raw(),
            &original
        );
    }
    #[test]
    fn unknown_scalar_switch_and_drum_values_survive_passive_rendering() {
        let mut app = app(BackendMode::Hardware);
        let mut records = app
            .device_globals
            .as_ref()
            .unwrap()
            .records()
            .raw()
            .records()
            .to_vec();
        for record in &mut records {
            match record.command {
                0x1007 => record.body[GeneralSetting::InputMode.body_offset()
                    ..GeneralSetting::InputMode.body_offset() + 2]
                    .copy_from_slice(&99_i16.to_le_bytes()),
                0x1004 => record.body[72..76].copy_from_slice(&0x7fc0_1234_u32.to_le_bytes()),
                0x1002 => {
                    record.body[..2].copy_from_slice(&u16::MAX.to_le_bytes());
                    record.body[2..4].copy_from_slice(&u16::MAX.to_le_bytes());
                }
                0x100e => record.body[20] = 99,
                _ => {}
            }
        }
        let raw = RawTransfer::from_records(WRITE_KIND, &records).unwrap();
        app.device_globals = Some(Arc::new(
            DeviceGlobalState::from_transfer(Revision(11), app.epoch, raw.clone(), vec![]).unwrap(),
        ));
        let ctx = egui::Context::default();
        let mut state = GlobalUiState::default();
        for tab in [GlobalTab::Audio, GlobalTab::Footswitch, GlobalTab::Eq] {
            state.tab = tab;
            assert!(
                frame(
                    &ctx,
                    &mut state,
                    &app,
                    false,
                    false,
                    vec![],
                    [1280.0, 900.0]
                )
                .action
                .is_none()
            );
        }
        for section in 0..5 {
            state.eq_section = section;
            assert!(
                frame(
                    &ctx,
                    &mut state,
                    &app,
                    false,
                    false,
                    vec![],
                    [1280.0, 900.0]
                )
                .action
                .is_none()
            );
        }
        assert!(
            frame(&ctx, &mut state, &app, true, false, vec![], [1280.0, 900.0])
                .action
                .is_none()
        );
        assert_eq!(state.general[&GeneralSetting::InputMode], 99);
        assert_eq!(state.drums[0], u16::MAX);
        assert_eq!(state.drums[2], u16::MAX);
        assert_eq!(state.eq[&EqParameter::Band3Gain].bits.0, 0x7fc0_1234);
        assert_eq!(app.device_globals.as_ref().unwrap().records().raw(), &raw);
    }
    #[test]
    fn an_actual_apply_click_emits_only_the_selected_typed_delta() {
        let app = app(BackendMode::Hardware);
        let ctx = egui::Context::default();
        let mut state = GlobalUiState::default();
        frame(
            &ctx,
            &mut state,
            &app,
            false,
            false,
            vec![],
            [1280.0, 900.0],
        );
        state.general.insert(GeneralSetting::InputLevel, 1);
        let output = frame(
            &ctx,
            &mut state,
            &app,
            false,
            false,
            vec![],
            [1280.0, 900.0],
        );
        assert!(output.action.is_none());
        let rect = output
            .apply_rects
            .iter()
            .find(|(label, _)| label == GeneralSetting::InputLevel.label())
            .unwrap()
            .1;
        let output = click(&ctx, &mut state, &app, false, false, rect);
        assert!(matches!(
            output.action,
            Some(UiIntent::ApplyGlobalControlEdit(
                GlobalControlEdit::General {
                    field: GeneralSetting::InputLevel,
                    value: 1
                }
            ))
        ));
        assert_eq!(
            app.device_globals
                .as_ref()
                .unwrap()
                .records()
                .general
                .as_ref()
                .unwrap()
                .value(GeneralSetting::InputLevel),
            0
        );
    }
    #[test]
    fn pending_or_invalid_apply_clicks_emit_no_edit() {
        let app = app(BackendMode::Hardware);
        let ctx = egui::Context::default();
        let mut state = GlobalUiState::default();
        frame(
            &ctx,
            &mut state,
            &app,
            false,
            false,
            vec![],
            [1280.0, 900.0],
        );
        state.general.insert(GeneralSetting::InputLevel, 1);
        state.general.insert(GeneralSetting::InputMode, 99);
        let output = frame(&ctx, &mut state, &app, false, true, vec![], [1280.0, 900.0]);
        let rect = output
            .apply_rects
            .iter()
            .find(|(label, _)| label == GeneralSetting::InputLevel.label())
            .unwrap()
            .1;
        assert!(
            click(&ctx, &mut state, &app, false, true, rect)
                .action
                .is_none()
        );
        let output = frame(
            &ctx,
            &mut state,
            &app,
            false,
            false,
            vec![],
            [1280.0, 900.0],
        );
        let rect = output
            .apply_rects
            .iter()
            .find(|(label, _)| label == GeneralSetting::InputMode.label())
            .unwrap()
            .1;
        assert!(
            click(&ctx, &mut state, &app, false, false, rect)
                .action
                .is_none()
        );
        assert!(
            blocked_reason(
                &app,
                false,
                &GlobalControlEdit::General {
                    field: GeneralSetting::InputMode,
                    value: 99
                }
            )
            .unwrap()
            .contains("value")
        );
    }
    #[test]
    fn float_text_preserves_nonfinite_offgrid_and_negative_zero_bits_until_changed() {
        for bits in [
            0x8000_0000,
            0x7fc0_1234,
            0x7f80_0000,
            0x0000_0001,
            0x3dcc_cccd,
        ] {
            let draft = EqDraft::new(F32Bits(bits));
            assert!(draft.proposal(EqParameter::Band3Gain).unwrap().is_none());
            assert_eq!(draft.bits.0, bits);
        }
        let mut draft = EqDraft::new(F32Bits(0x7fc0_1234));
        draft.text = "0.12345679".into();
        assert!(
            matches!(draft.proposal(EqParameter::Band3Gain).unwrap(), Some(GlobalControlEdit::Eq { value, .. }) if value == F32Bits::from_f32(0.12345679))
        );
        let mut draft = EqDraft::new(F32Bits::from_f32(0.0));
        for text in ["NaN", "inf", "21", "bad"] {
            draft.text = text.into();
            assert!(draft.proposal(EqParameter::Band3Gain).is_err());
        }
    }
    #[test]
    fn refresh_revision_disconnect_and_new_epoch_retire_staged_values() {
        let mut app = app(BackendMode::Simulator);
        let mut state = GlobalUiState::default();
        state.observe(&app);
        state.general.insert(GeneralSetting::InputLevel, 8);
        state.observe(&app);
        assert_eq!(state.general[&GeneralSetting::InputLevel], 8);
        let baseline = app.device_globals.as_ref().unwrap();
        app.device_globals = Some(Arc::new(
            DeviceGlobalState::from_transfer(
                Revision(11),
                app.epoch,
                baseline.records().raw().clone(),
                vec![],
            )
            .unwrap(),
        ));
        state.observe(&app);
        assert_eq!(state.general[&GeneralSetting::InputLevel], 0);
        state.general.insert(GeneralSetting::InputLevel, 8);
        app.epoch = SessionEpoch(5);
        state.observe(&app);
        assert!(state.general.is_empty());
        app.epoch = SessionEpoch(4);
        app.connection = ConnectionState::Disconnected;
        state.observe(&app);
        assert!(state.general.is_empty());
    }
    #[test]
    fn stop_at_ordinary_volume_is_enabled_but_live_pattern_edit_is_blocked() {
        let mut app = app(BackendMode::Hardware);
        let before = app.device_globals.as_ref().unwrap();
        let playing = before
            .expected_after(&GlobalControlEdit::DrumPlay(true))
            .unwrap();
        app.device_globals = Some(Arc::new(
            DeviceGlobalState::from_transfer(Revision(11), app.epoch, playing, vec![]).unwrap(),
        ));
        assert_eq!(
            app.device_globals
                .as_ref()
                .unwrap()
                .records()
                .drum
                .as_ref()
                .unwrap()
                .value(DrumSetting::Volume),
            80
        );
        assert!(blocked_reason(&app, false, &GlobalControlEdit::DrumPlay(false)).is_none());
        assert!(
            blocked_reason(
                &app,
                false,
                &GlobalControlEdit::Drum {
                    field: DrumSetting::Pattern,
                    value: 1
                }
            )
            .unwrap()
            .starts_with("Stop")
        );
        let ctx = egui::Context::default();
        let mut state = GlobalUiState::default();
        let output = frame(&ctx, &mut state, &app, true, false, vec![], [1280.0, 900.0]);
        let output = click(
            &ctx,
            &mut state,
            &app,
            true,
            false,
            output.play_rect.unwrap(),
        );
        assert!(matches!(
            output.action,
            Some(UiIntent::ApplyGlobalControlEdit(
                GlobalControlEdit::DrumPlay(false)
            ))
        ));
    }
    #[test]
    fn hardware_profile_and_capability_gates_are_shared_with_the_actor() {
        let mut hardware = app(BackendMode::Hardware);
        let unverified = GlobalControlEdit::Footswitch {
            table: GlobalFootswitchTableId::new(0).unwrap(),
            event: GlobalFootswitchEvent::new(18).unwrap(),
            function: GlobalFootswitchFunction::new(2).unwrap(),
        };
        assert!(blocked_reason(&hardware, false, &unverified).is_some());
        assert!(blocked_reason(&app(BackendMode::Simulator), false, &unverified).is_none());
        let verified = GlobalControlEdit::General {
            field: GeneralSetting::DisplayMode,
            value: 1,
        };
        assert!(blocked_reason(&hardware, false, &verified).is_none());
        hardware
            .capabilities
            .operations
            .remove(&OperationKind::WriteGlobals);
        assert!(
            blocked_reason(&hardware, false, &verified)
                .unwrap()
                .contains("write profile")
        );
        hardware.identity = None;
        assert!(
            read_blocked_reason(&hardware, false)
                .unwrap()
                .contains("identity")
        );
    }
    #[test]
    fn inactive_switch_tables_share_storage_gate_and_stage_only_the_selected_native_event() {
        let hardware = app(BackendMode::Hardware);
        for table in [1, 2] {
            for (event, function) in [(0, 17), (1, 5), (16, 23)] {
                let patch = GlobalControlEdit::Footswitch {
                    table: GlobalFootswitchTableId::new(table).unwrap(),
                    event: GlobalFootswitchEvent::new(event).unwrap(),
                    function: GlobalFootswitchFunction::new(function).unwrap(),
                };
                assert!(blocked_reason(&hardware, false, &patch).is_none());
                assert!(
                    blocked_reason(&hardware, true, &patch)
                        .unwrap()
                        .contains("pending")
                );
                let ctx = egui::Context::default();
                let mut state = GlobalUiState::default();
                state.observe(&hardware);
                state.tab = GlobalTab::Footswitch;
                state.footswitch_table = table;
                state.switches.insert((table, event), function);
                // Compact rendering is passive and preserves all raw values.
                assert!(
                    frame(
                        &ctx,
                        &mut state,
                        &hardware,
                        false,
                        false,
                        vec![],
                        [960.0, 640.0]
                    )
                    .action
                    .is_none()
                );
                let output = frame(
                    &ctx,
                    &mut state,
                    &hardware,
                    false,
                    false,
                    vec![],
                    [1280.0, 1800.0],
                );
                let label = GLOBAL_FOOTSWITCH_EVENT_NAMES[event as usize];
                let rect = output
                    .apply_rects
                    .iter()
                    .find(|(name, _)| name == label)
                    .unwrap()
                    .1;
                // Use the same large viewport for the real press/release so lower combination rows are reachable.
                let mut clicked = None;
                for pressed in [true, false] {
                    clicked = frame(
                        &ctx,
                        &mut state,
                        &hardware,
                        false,
                        false,
                        vec![
                            egui::Event::PointerMoved(rect.center()),
                            egui::Event::PointerButton {
                                pos: rect.center(),
                                button: egui::PointerButton::Primary,
                                pressed,
                                modifiers: egui::Modifiers::NONE,
                            },
                        ],
                        [1280.0, 1800.0],
                    )
                    .action;
                }
                assert!(
                    matches!(clicked,Some(UiIntent::ApplyGlobalControlEdit(actual)) if actual == patch)
                );
            }
        }
        let active = GlobalControlEdit::Footswitch {
            table: GlobalFootswitchTableId::new(0).unwrap(),
            event: GlobalFootswitchEvent::new(0).unwrap(),
            function: GlobalFootswitchFunction::new(17).unwrap(),
        };
        assert!(
            blocked_reason(&hardware, false, &active)
                .unwrap()
                .contains("inactive")
        );
        let excluded = GlobalControlEdit::Footswitch {
            table: GlobalFootswitchTableId::new(2).unwrap(),
            event: GlobalFootswitchEvent::new(1).unwrap(),
            function: GlobalFootswitchFunction::new(1).unwrap(),
        };
        assert!(
            blocked_reason(&hardware, false, &excluded)
                .unwrap()
                .contains("native menu")
        );
    }
    #[test]
    fn all_general_and_eq_fields_share_validated_hardware_gates_and_render_once() {
        let hardware = app(BackendMode::Hardware);
        let baseline = hardware.device_globals.as_ref().unwrap();
        let ctx = egui::Context::default();
        let mut state = GlobalUiState::default();
        let mut general_labels = std::collections::BTreeSet::new();
        for tab in [
            GlobalTab::Audio,
            GlobalTab::Midi,
            GlobalTab::External,
            GlobalTab::Footswitch,
            GlobalTab::Display,
        ] {
            state.tab = tab;
            let output = frame(
                &ctx,
                &mut state,
                &hardware,
                false,
                false,
                vec![],
                [960.0, 640.0],
            );
            assert!(output.action.is_none());
            for (label, _) in output.apply_rects {
                if GeneralSetting::ALL
                    .iter()
                    .any(|field| field.label() == label)
                {
                    assert!(general_labels.insert(label));
                }
            }
        }
        assert_eq!(general_labels.len(), GeneralSetting::ALL.len());
        for field in GeneralSetting::ALL {
            let value = baseline.records().general.as_ref().unwrap().value(field);
            let edit = GlobalControlEdit::General { field, value };
            assert!(
                blocked_reason(&hardware, false, &edit).is_none(),
                "{field:?}"
            );
            assert!(blocked_reason(&app(BackendMode::CaptureReplay), false, &edit).is_some());
        }
        state.tab = GlobalTab::Eq;
        let mut eq_labels = std::collections::BTreeSet::new();
        for section in 0..7 {
            state.eq_section = section;
            let output = frame(
                &ctx,
                &mut state,
                &hardware,
                false,
                false,
                vec![],
                [960.0, 640.0],
            );
            assert!(output.action.is_none());
            for (label, _) in output.apply_rects {
                assert!(eq_labels.insert(label));
            }
        }
        assert_eq!(eq_labels.len(), EqParameter::ALL.len());
        for parameter in EqParameter::ALL {
            let edit = GlobalControlEdit::Eq {
                parameter,
                value: baseline.records().eq.as_ref().unwrap().values[parameter.index() as usize],
            };
            assert!(
                blocked_reason(&hardware, false, &edit).is_none(),
                "{parameter:?}"
            );
            assert!(blocked_reason(&hardware, true, &edit).is_some());
            let mut wrong_variant = hardware.clone();
            wrong_variant.identity.as_mut().unwrap().model = DeviceModel::Gp200Jr;
            assert!(blocked_reason(&wrong_variant, false, &edit).is_some());
        }
    }
    #[test]
    fn representative_breadth_widgets_emit_the_exact_typed_edit_only_after_apply() {
        let app = app(BackendMode::Hardware);
        for (tab, field, value) in [
            (GlobalTab::Midi, GeneralSetting::MidiInputUsb, 1),
            (GlobalTab::Midi, GeneralSetting::MidiInputSource, 1),
            (GlobalTab::Audio, GeneralSetting::RecordingLevel, 1),
            (GlobalTab::Audio, GeneralSetting::NoCabLeft, 0),
            (GlobalTab::External, GeneralSetting::ExternalFootswitch1, 1),
        ] {
            let ctx = egui::Context::default();
            let mut state = GlobalUiState {
                tab,
                ..Default::default()
            };
            state.observe(&app);
            state.general.insert(field, value);
            let output = frame(
                &ctx,
                &mut state,
                &app,
                false,
                false,
                vec![],
                [1280.0, 900.0],
            );
            assert!(output.action.is_none());
            let rect = output
                .apply_rects
                .iter()
                .find(|(label, _)| label == field.label())
                .unwrap()
                .1;
            let action = click(&ctx, &mut state, &app, false, false, rect).action;
            assert!(
                matches!(action,
                Some(UiIntent::ApplyGlobalControlEdit(GlobalControlEdit::General { field: actual_field, value: actual_value })) if actual_field == field && actual_value == value),
                "{field:?}: {action:?} at {rect:?}"
            );
        }
        for (section, parameter, value) in [
            (3, EqParameter::Band3Frequency, 1001.0),
            (3, EqParameter::Band3Q, 1.1),
            (3, EqParameter::Band3Enabled, 0.0),
            (0, EqParameter::Level, 99.0),
            (5, EqParameter::LowShelfGain, 0.1),
        ] {
            let ctx = egui::Context::default();
            let mut state = GlobalUiState {
                tab: GlobalTab::Eq,
                eq_section: section,
                ..Default::default()
            };
            state.observe(&app);
            state.eq.get_mut(&parameter).unwrap().text = value.to_string();
            let output = frame(
                &ctx,
                &mut state,
                &app,
                false,
                false,
                vec![],
                [1280.0, 900.0],
            );
            assert!(output.action.is_none());
            let rect = output
                .apply_rects
                .iter()
                .find(|(label, _)| label == eq_label(parameter))
                .unwrap()
                .1;
            assert!(
                matches!(click(&ctx, &mut state, &app, false, false, rect).action,
                Some(UiIntent::ApplyGlobalControlEdit(GlobalControlEdit::Eq { parameter: actual_parameter, value: actual_value })) if actual_parameter == parameter && actual_value == F32Bits::from_f32(value))
            );
        }
    }
    #[test]
    fn the_eight_pattern_groups_cover_every_wire_pattern_once() {
        let patterns: Vec<_> = DRUM_PATTERN_GROUPS
            .iter()
            .flat_map(|group| group.first..group.first + group.count)
            .collect();
        assert_eq!(patterns, (0..100).collect::<Vec<_>>());
        assert_eq!(DRUM_PATTERN_NAMES.len(), patterns.len());
        for pattern in patterns {
            DrumSetting::Pattern.validate(pattern).unwrap();
        }
    }
}
