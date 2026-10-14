//! The pedal's recovered desktop drum workflow.
use super::{
    globals::{self, GlobalUiState, WidgetOutput},
    section,
};
use crate::{StudioApp, theme::Palette};
use eframe::egui::{self, RichText};
use gp200_app::AppState;
use gp200_domain::*;
use gp200_protocol::DrumSetting;

impl StudioApp {
    pub(super) fn practice(&mut self, ui: &mut egui::Ui) {
        ui.heading(crate::localization::text("Practice"));
        ui.label(crate::localization::text(
            "Play with the GP-200's rhythm patterns and metronome.",
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
        section(ui, crate::localization::text("Audio device"));
        ui.label(crate::localization::text("Select GP-200 in your Linux recording application. Rhythm playback and guitar effects run on the pedal."));
        if let Some(document) = self.controller.active_document() {
            section(ui, crate::localization::text("Session notes"));
            ui.label(document.draft.metadata.notes.clone().unwrap_or_else(|| {
                crate::localization::text("Add practice notes in the Editor patch details.").into()
            }));
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
    globals::header(ui, app, pending, &mut output);
    let Some(baseline) = app
        .device_globals
        .as_ref()
        .filter(|baseline| baseline.epoch == app.epoch && app.connection == ConnectionState::Ready)
    else {
        section(ui, crate::localization::text("Pedal rhythm controls"));
        ui.label(crate::localization::text(
            "Read global settings to load the pedal's pattern, tempo, volume and playback state.",
        ));
        return output;
    };
    let Some(drums) = &baseline.records().drum else {
        return output;
    };
    let playing = baseline
        .records()
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.drum_playing());
    let palette = Palette::for_context(ui.ctx());
    section(ui, crate::localization::text("Pedal rhythm"));
    let selected_pattern = drums.value(DrumSetting::Pattern);
    ui.label(
        RichText::new(
            DRUM_PATTERN_NAMES
                .get(selected_pattern as usize)
                .map_or_else(
                    || std::format!("Unknown pattern ({selected_pattern})"),
                    |name| (*name).into(),
                ),
        )
        .size(25.0)
        .strong(),
    );
    ui.horizontal_wrapped(|ui| {
        ui.label(std::format!("{} BPM", drums.value(DrumSetting::Tempo)));
        ui.label(std::format!("Volume {}%", drums.value(DrumSetting::Volume)));
        ui.colored_label(
            palette.accent,
            match playing {
                Some(true) => crate::localization::text("Playing"),
                Some(false) => crate::localization::text("Stopped"),
                None => crate::localization::text("Playback state unknown"),
            },
        );
        let edit = GlobalControlEdit::DrumPlay(!playing.unwrap_or(false));
        let reason = globals::blocked_reason(app, pending, &edit);
        let response = ui.add_enabled(
            playing.is_some() && reason.is_none() && output.action.is_none(),
            egui::Button::new(if playing == Some(true) {
                crate::localization::text("Stop drums")
            } else {
                crate::localization::text("Play drums")
            })
            .min_size(egui::vec2(130.0, 38.0)),
        );
        output.play_rect = Some(response.rect);
        if response.clicked() {
            output.action = Some(UiIntent::ApplyGlobalControlEdit(edit));
        }
        if let Some(reason) = reason {
            response.on_hover_text(crate::localization::message(&reason));
        }
    });
    if playing == Some(true)
        && app
            .identity
            .as_ref()
            .is_some_and(|identity| identity.mode == BackendMode::Hardware)
    {
        ui.small(crate::localization::text("Stop drums before applying a different pattern, tempo or volume. Stop works at the current pedal volume."));
    }
    section(ui, crate::localization::text("Tempo and volume"));
    for (field, index, label, minimum, maximum, suffix) in [
        (
            DrumSetting::Tempo,
            2,
            crate::localization::text("Tempo"),
            40,
            250,
            crate::localization::text(" BPM"),
        ),
        (
            DrumSetting::Volume,
            1,
            crate::localization::text("Volume"),
            0,
            100,
            " %",
        ),
    ] {
        ui.push_id(label, |ui| {
            ui.horizontal_wrapped(|ui| {
                globals::field_label(ui, 100.0, label);
                ui.add(
                    egui::DragValue::new(&mut state.drums[index])
                        .range(minimum..=maximum)
                        .clamp_existing_to_range(false)
                        .suffix(suffix),
                );
                drum_apply(
                    ui,
                    app,
                    pending,
                    field,
                    state.drums[index],
                    drums.value(field),
                    &mut output,
                );
            });
        });
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(std::format!(
            "Selected: {}",
            DRUM_PATTERN_NAMES.get(state.drums[0] as usize).map_or_else(
                || std::format!("Unknown ({})", state.drums[0]),
                |name| (*name).into()
            )
        ));
        drum_apply(
            ui,
            app,
            pending,
            DrumSetting::Pattern,
            state.drums[0],
            selected_pattern,
            &mut output,
        );
    });
    ui.add_space(15.0);
    section(ui, crate::localization::text("Pattern library"));
    ui.horizontal_wrapped(|ui| {
        for (index, group) in DRUM_PATTERN_GROUPS.iter().enumerate() {
            ui.selectable_value(
                &mut state.drum_group,
                index,
                crate::localization::text(group.name),
            );
        }
    });
    let group = DRUM_PATTERN_GROUPS[state.drum_group];
    egui::Frame::new()
        .fill(palette.surface)
        .inner_margin(12)
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                for pattern in group.first..group.first + group.count {
                    ui.selectable_value(
                        &mut state.drums[0],
                        pattern,
                        DRUM_PATTERN_NAMES[pattern as usize],
                    );
                }
            });
        });
    output
}

fn drum_apply(
    ui: &mut egui::Ui,
    app: &AppState,
    pending: bool,
    field: DrumSetting,
    value: u16,
    original: u16,
    output: &mut WidgetOutput,
) {
    let edit = GlobalControlEdit::Drum { field, value };
    let proposal = if value == original {
        Ok(None)
    } else {
        field
            .validate(value)
            .map(|_| Some(edit.clone()))
            .map_err(|error| error.to_string())
    };
    globals::apply(
        ui,
        app,
        pending,
        match field {
            DrumSetting::Pattern => crate::localization::text("Pattern"),
            DrumSetting::Tempo => crate::localization::text("Tempo"),
            DrumSetting::Volume => crate::localization::text("Volume"),
        },
        &edit,
        proposal,
        output,
    );
}
