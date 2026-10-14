use super::{library_workflows::Purpose, section};
use crate::{PendingSwitch, StudioApp, theme::Palette};
use eframe::egui::{self, RichText};
use gp200_domain::*;
use gp200_library::*;

impl StudioApp {
    pub(super) fn performance_browser(&mut self, ui: &mut egui::Ui) {
        section(ui, crate::localization::text("Setlists"));
        if ui
            .checkbox(
                &mut self.library_workflow.show_setlist_trash,
                crate::localization::text("Show trash"),
            )
            .changed()
        {
            self.workflow_submit(
                WorkflowAction::ListSetlists {
                    include_trash: self.library_workflow.show_setlist_trash,
                },
                Purpose::Setlists,
            );
        }
        for setlist in self.library_workflow.setlists.clone() {
            let selected = self
                .library_workflow
                .setlist
                .as_ref()
                .is_some_and(|s| s.id == setlist.id);
            if ui
                .selectable_label(
                    selected,
                    format!(
                        "{}{}",
                        setlist.name,
                        if setlist.trashed {
                            crate::localization::text(" (trash)")
                        } else {
                            ""
                        }
                    ),
                )
                .clicked()
            {
                if self.library_workflow.setlist_dirty || self.library_workflow.pending_mutation() {
                    self.services.error(crate::localization::text(
                        "Save or discard the setlist changes before opening another setlist.",
                    ));
                } else {
                    self.library_workflow.setlist = Some(setlist.clone());
                    self.library_workflow.selected_entry = None;
                    self.workflow_submit(
                        WorkflowAction::ResolveSetlist(setlist.id),
                        Purpose::Resolve,
                    );
                }
            }
        }
        ui.add_space(12.0);
        ui.add(
            egui::TextEdit::singleline(&mut self.library_workflow.new_setlist_name)
                .hint_text(crate::localization::text("Setlist name"))
                .desired_width(f32::INFINITY),
        );
        if ui
            .add_enabled(
                !self.library_workflow.pending_mutation(),
                egui::Button::new(crate::localization::text("Create setlist")),
            )
            .clicked()
        {
            let name = self.library_workflow.new_setlist_name.trim().to_owned();
            if name.is_empty() {
                self.services
                    .error(crate::localization::text("Enter a name for the setlist."));
            } else {
                self.workflow_submit(WorkflowAction::CreateSetlist(name), Purpose::Setlist);
            }
        }
        ui.add_space(16.0);
        section(ui, crate::localization::text("Songs"));
        if let Some(setlist) = self.library_workflow.setlist.clone() {
            for (index, entry) in setlist.entries.iter().enumerate() {
                let selected = self.library_workflow.selected_entry.as_ref() == Some(&entry.id);
                if ui
                    .selectable_label(
                        selected,
                        format!(
                            "{}. {}{}",
                            index + 1,
                            entry.song,
                            if entry.section.is_empty() {
                                String::new()
                            } else {
                                format!(" / {}", entry.section)
                            }
                        ),
                    )
                    .clicked()
                {
                    self.library_workflow.selected_entry = Some(entry.id.clone());
                }
            }
        }
        ui.small(crate::localization::text(
            "Selecting a song stays local. Open sound and Audition slot are separate actions.",
        ));
    }
    pub(super) fn performance(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.heading(crate::localization::text("Performance"));
            ui.selectable_value(
                &mut self.library_workflow.arranging,
                false,
                crate::localization::text("Stage"),
            );
            ui.selectable_value(
                &mut self.library_workflow.arranging,
                true,
                crate::localization::text("Arrange setlist"),
            );
            ui.checkbox(
                &mut self.performance_compact,
                crate::localization::text("Compact editor"),
            );
            if ui
                .button(if self.performance_locked {
                    crate::localization::text("Unlock controls")
                } else {
                    crate::localization::text("Lock controls")
                })
                .clicked()
            {
                self.performance_locked = !self.performance_locked;
            }
            if ui
                .small_button(crate::localization::text("Refresh setlists"))
                .clicked()
            {
                self.refresh_library_workflows();
            }
        });
        if self.library_workflow.arranging {
            self.arrange_setlist(ui);
            return;
        }
        let palette = Palette::for_context(ui.ctx());
        let selected = self
            .library_workflow
            .setlist
            .as_ref()
            .and_then(|s| {
                s.entries
                    .iter()
                    .find(|e| Some(&e.id) == self.library_workflow.selected_entry.as_ref())
            })
            .cloned();
        if let Some(entry) = selected {
            ui.add_space(16.0);
            ui.label(
                RichText::new(&entry.song)
                    .size(if ui.available_width() < 800.0 {
                        40.0
                    } else {
                        56.0
                    })
                    .strong(),
            );
            if !entry.section.is_empty() {
                ui.label(
                    RichText::new(&entry.section)
                        .size(24.0)
                        .color(palette.accent),
                );
            }
            if !entry.notes.is_empty() {
                ui.label(&entry.notes);
            }
            let state = self
                .library_workflow
                .resolved
                .as_ref()
                .and_then(|r| r.entries.iter().find(|e| e.entry.id == entry.id))
                .map(|r| r.state.clone());
            let available = matches!(state, Some(SetlistReferenceState::Available { .. }));
            match state {
                Some(SetlistReferenceState::Available {
                    newer_revision,
                    dependencies,
                    ..
                }) => {
                    ui.small(crate::localization::text("Exact sound revision pinned"))
                        .on_hover_text(std::format!("Revision {}", entry.version.0));
                    if newer_revision.is_some() {
                        ui.colored_label(
                            palette.warning,
                            crate::localization::text("A newer sound revision exists. This song retains its pinned version."),
                        );
                    }
                    if !dependencies.missing.is_empty() {
                        ui.colored_label(
                            palette.warning,
                            std::format!(
                                "{} unresolved asset references",
                                dependencies.missing.len()
                            ),
                        );
                    }
                }
                Some(SetlistReferenceState::Trashed { .. }) => {
                    ui.colored_label(
                        palette.warning,
                        crate::localization::text(
                            "This pinned sound is in library trash. Restore it before opening.",
                        ),
                    );
                }
                _ => {
                    ui.colored_label(palette.warning,crate::localization::text("This pinned sound is missing or has not been resolved. Refresh the setlist."));
                }
            }
            if let Some(tempo) = entry.tempo_fallback {
                ui.label(std::format!("Song tempo {tempo} BPM (local fallback)"));
            }
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(
                        available,
                        egui::Button::new(crate::localization::text("Open pinned sound locally")),
                    )
                    .clicked()
                {
                    self.request_switch(PendingSwitch::Library(entry.version.clone()));
                }
                if let Some(slot) = entry.proposed_slot
                    && ui
                        .add_enabled(
                            self.controller.state().connection == ConnectionState::Ready,
                            egui::Button::new(std::format!("Audition pedal slot {slot}")),
                        )
                        .clicked()
                {
                    self.request_switch(PendingSwitch::Audition(slot));
                }
                if ui
                    .button(crate::localization::text("Previous song"))
                    .clicked()
                {
                    self.step_setlist(-1);
                }
                if ui.button(crate::localization::text("Next song")).clicked() {
                    self.step_setlist(1);
                }
            });
            ui.small(crate::localization::text("Page Up / Page Down choose a song. Pedal slot audition selects the existing slot; it does not install the pinned sound."));
        } else {
            section(ui, crate::localization::text("Choose a song"));
            ui.label(crate::localization::text("Create a setlist and pin library sounds in Arrange setlist, then select a song at the left."));
        }
        ui.add_space(12.0);
        ui.separator();
        if let Some(document) = self.controller.active_document().cloned() {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(document.draft.view.display_name())
                        .size(23.0)
                        .strong(),
                );
                ui.small(match document.target {
                    EditTarget::DeviceBuffer => crate::localization::text("Pedal edit buffer"),
                    EditTarget::Offline(_) => crate::localization::text("Offline document"),
                });
            });
            if self.performance_compact {
                ui.add_enabled_ui(!self.performance_locked, |ui| {
                    self.signal_chain(ui);
                    self.module_editor(ui);
                });
            } else {
                egui::Grid::new("stage-parameters")
                    .min_col_width(40.0)
                    .spacing(egui::vec2(14.0, 4.0))
                    .show(ui, |ui| {
                        ui.label(crate::localization::text("Amp"));
                        self.stage_parameter(ui, 3, 0);
                        ui.end_row();
                        ui.label(crate::localization::text("Reverb"));
                        self.stage_parameter(ui, 9, 0);
                        ui.end_row();
                        ui.label(crate::localization::text("Output"));
                        self.stage_parameter(ui, 10, 0);
                        ui.end_row();
                    });
            }
            self.history_toolbar(ui);
        }
    }
    fn arrange_setlist(&mut self, ui: &mut egui::Ui) {
        let Some(mut setlist) = self.library_workflow.setlist.clone() else {
            ui.label(crate::localization::text(
                "Create or choose a setlist at the left.",
            ));
            return;
        };
        let busy = self.library_workflow.pending_mutation();
        let mut changed = false;
        let mut save = false;
        let mut discard = false;
        let mut trash = false;
        ui.add_enabled_ui(!busy && !setlist.trashed, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(crate::localization::text("Name"));
                changed |= ui.text_edit_singleline(&mut setlist.name).changed();
                if ui
                    .add_enabled(
                        self.library_workflow.setlist_dirty,
                        egui::Button::new(crate::localization::text("Save setlist")),
                    )
                    .clicked()
                {
                    save = true;
                }
                if ui
                    .add_enabled(
                        self.library_workflow.setlist_dirty,
                        egui::Button::new(crate::localization::text("Discard local changes")),
                    )
                    .clicked()
                {
                    discard = true;
                }
            });
            changed |= ui
                .add(
                    egui::TextEdit::multiline(&mut setlist.notes)
                        .hint_text(crate::localization::text("Setlist notes"))
                        .desired_rows(2),
                )
                .changed();
            section(ui, crate::localization::text("Pin a library sound"));
            let choices = self.library_pin_choices();
            egui::ComboBox::from_id_salt("setlist-pin-version")
                .selected_text(
                    self.library_workflow
                        .pin_version
                        .as_ref()
                        .and_then(|id| choices.iter().find(|(v, _)| v == id))
                        .map_or(
                            crate::localization::text("Choose an exact revision"),
                            |(_, label)| label.as_str(),
                        ),
                )
                .width(ui.available_width().min(420.0))
                .show_ui(ui, |ui| {
                    for (version, label) in choices {
                        ui.selectable_value(
                            &mut self.library_workflow.pin_version,
                            Some(version),
                            label,
                        );
                    }
                });
            if ui
                .add_enabled(
                    self.library_workflow.pin_version.is_some(),
                    egui::Button::new(crate::localization::text("Add song with this sound")),
                )
                .clicked()
                && let Some(version) = self.library_workflow.pin_version.clone()
            {
                let title = self
                    .controller
                    .state()
                    .library
                    .entries
                    .iter()
                    .find(|e| e.latest_version == version)
                    .map_or(crate::localization::text("New song"), |e| {
                        e.metadata.name.as_str()
                    });
                let entry = self.new_setlist_entry(version, title.to_owned());
                self.library_workflow.selected_entry = Some(entry.id.clone());
                setlist.entries.push(entry);
                changed = true;
            }
            section(ui, crate::localization::text("Song order and notes"));
            let mut reorder = None;
            let mut remove = None;
            let mut duplicate = None;
            for (index, entry) in setlist.entries.iter_mut().enumerate() {
                egui::CollapsingHeader::new(format!(
                    "{}. {} / {}",
                    index + 1,
                    entry.song,
                    entry.section
                ))
                .id_salt(&entry.id)
                .default_open(self.library_workflow.selected_entry.as_ref() == Some(&entry.id))
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(crate::localization::text("Song"));
                        changed |= ui.text_edit_singleline(&mut entry.song).changed();
                        ui.label(crate::localization::text("Section"));
                        changed |= ui.text_edit_singleline(&mut entry.section).changed();
                    });
                    changed |= ui
                        .add(
                            egui::TextEdit::multiline(&mut entry.notes)
                                .hint_text(crate::localization::text("Song and section notes"))
                                .desired_rows(2),
                        )
                        .changed();
                    ui.small(std::format!("Pinned revision {}", entry.version.0));
                    ui.horizontal_wrapped(|ui| {
                        let mut tempo = entry.tempo_fallback.is_some();
                        if ui
                            .checkbox(&mut tempo, crate::localization::text("Fallback tempo"))
                            .changed()
                        {
                            entry.tempo_fallback = tempo.then_some(120);
                            changed = true;
                        }
                        if let Some(value) = &mut entry.tempo_fallback {
                            changed |= ui
                                .add(
                                    egui::DragValue::new(value)
                                        .range(40..=250)
                                        .suffix(crate::localization::text(" BPM")),
                                )
                                .changed();
                        }
                        let mut proposed = entry.proposed_slot.is_some();
                        if ui
                            .checkbox(
                                &mut proposed,
                                crate::localization::text("Pedal slot proposal"),
                            )
                            .changed()
                        {
                            entry.proposed_slot =
                                proposed.then(|| SlotId::new(0).expect("bounded"));
                            changed = true;
                        }
                        if let Some(slot) = entry.proposed_slot {
                            let mut n = slot.get();
                            if ui
                                .add(egui::DragValue::new(&mut n).range(0..=255))
                                .changed()
                            {
                                entry.proposed_slot = SlotId::new(n).ok();
                                changed = true;
                            }
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add_enabled(
                                index > 0,
                                egui::Button::new(crate::localization::text("Move up")),
                            )
                            .clicked()
                        {
                            reorder = Some((index, index - 1));
                        }
                        if ui.button(crate::localization::text("Move down")).clicked() {
                            reorder = Some((index, index + 1));
                        }
                        if ui
                            .button(crate::localization::text("Duplicate song"))
                            .clicked()
                        {
                            duplicate = Some(index);
                        }
                        if ui
                            .button(crate::localization::text("Remove song"))
                            .clicked()
                        {
                            remove = Some(index);
                        }
                    });
                });
            }
            if let Some((a, b)) = reorder
                && b < setlist.entries.len()
            {
                setlist.entries.swap(a, b);
                changed = true;
            }
            if let Some(index) = remove {
                setlist.entries.remove(index);
                changed = true;
            }
            if let Some(index) = duplicate {
                let source = &setlist.entries[index];
                let mut copy = self.new_setlist_entry(source.version.clone(), source.song.clone());
                copy.section = source.section.clone();
                copy.notes = source.notes.clone();
                copy.tempo_fallback = source.tempo_fallback;
                copy.proposed_slot = source.proposed_slot;
                setlist.entries.insert(index + 1, copy);
                changed = true;
            }
        });
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    !busy && !self.library_workflow.setlist_dirty,
                    egui::Button::new(if setlist.trashed {
                        crate::localization::text("Restore setlist from trash")
                    } else {
                        crate::localization::text("Move setlist to trash")
                    }),
                )
                .clicked()
            {
                trash = true;
            }
            let selections = setlist
                .entries
                .iter()
                .filter_map(|entry| {
                    entry.proposed_slot.map(|target| RestoreSelection {
                        source: RestoreSource::LibraryVersion(entry.version.clone()),
                        target,
                    })
                })
                .collect::<Vec<_>>();
            if ui
                .add_enabled(
                    !selections.is_empty()
                        && !self.library_workflow.busy()
                        && !self.library_workflow.setlist_dirty,
                    egui::Button::new(crate::localization::text("Review proposed pedal layout")),
                )
                .clicked()
            {
                self.plan_restore_sources(selections);
                self.library_workflow.tab = super::library_workflows::LibraryTab::Backups;
                self.send(UiIntent::Navigate(ViewId::Library));
            }
        });
        if changed {
            self.library_workflow.setlist_dirty = true;
            self.library_workflow.setlist = Some(setlist.clone());
        }
        if save {
            self.workflow_submit(
                WorkflowAction::SaveSetlist {
                    expected_revision: setlist.revision,
                    setlist: setlist.clone(),
                },
                Purpose::Setlist,
            );
        }
        if discard {
            self.library_workflow.setlist_dirty = false;
            self.workflow_submit(
                WorkflowAction::ResolveSetlist(setlist.id.clone()),
                Purpose::Resolve,
            );
        }
        if trash {
            self.workflow_submit(
                WorkflowAction::TrashSetlist {
                    id: setlist.id,
                    expected_revision: setlist.revision,
                    trashed: !setlist.trashed,
                },
                Purpose::Setlist,
            );
        }
    }
    // IDs are created without opening /dev/urandom or touching disk in an egui frame.
    fn new_setlist_entry(&mut self, version: LibraryVersionId, song: String) -> SetlistEntry {
        let intent = self.local_intent();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        SetlistEntry {
            id: format!("song-gui-{nanos:x}-{:x}-{:x}", std::process::id(), intent.0),
            version,
            song,
            section: String::new(),
            notes: String::new(),
            tempo_fallback: None,
            proposed_slot: None,
        }
    }
    pub(crate) fn library_pin_choices(&self) -> Vec<(LibraryVersionId, String)> {
        let mut values = self
            .controller
            .state()
            .library
            .entries
            .iter()
            .map(|e| {
                (
                    e.latest_version.clone(),
                    std::format!("{} · revision {}", e.metadata.name, e.latest_version.0),
                )
            })
            .collect::<Vec<_>>();
        for version in self.controller.state().library.versions.values() {
            if !values.iter().any(|(id, _)| id == &version.id) {
                values.push((
                    version.id.clone(),
                    std::format!("{} · revision {}", version.metadata.name, version.id.0),
                ));
            }
        }
        values
    }
    pub(super) fn step_setlist(&mut self, delta: i32) {
        let Some(setlist) = &self.library_workflow.setlist else {
            return;
        };
        if setlist.entries.is_empty() {
            return;
        }
        let current = setlist
            .entries
            .iter()
            .position(|e| Some(&e.id) == self.library_workflow.selected_entry.as_ref())
            .unwrap_or(0);
        let next = (current as i32 + delta).clamp(0, setlist.entries.len().saturating_sub(1) as i32)
            as usize;
        self.library_workflow.selected_entry = Some(setlist.entries[next].id.clone());
    }
}
