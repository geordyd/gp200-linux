use super::{
    diff_view,
    library_workflows::{LibraryTab, Purpose, RestorePhase},
    section,
};
use crate::{StudioApp, runtime::FilePurpose, theme::Palette};
use eframe::egui::{self, RichText};
use gp200_domain::*;
use gp200_library::*;

impl StudioApp {
    pub(super) fn library(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(
                &mut self.library_workflow.tab,
                LibraryTab::Presets,
                crate::localization::text("Presets"),
            );
            ui.selectable_value(
                &mut self.library_workflow.tab,
                LibraryTab::Backups,
                crate::localization::text("Device backups and restore"),
            );
            ui.selectable_value(
                &mut self.library_workflow.tab,
                LibraryTab::Compare,
                crate::localization::text("Compare revisions"),
            );
            ui.selectable_value(
                &mut self.library_workflow.tab,
                LibraryTab::Merge,
                crate::localization::text("Merge selected settings"),
            );
        });
        ui.add_space(12.0);
        match self.library_workflow.tab {
            LibraryTab::Presets => self.preset_library(ui),
            LibraryTab::Backups => self.device_backups(ui),
            LibraryTab::Compare => self.compare_library_revisions(ui),
            LibraryTab::Merge => self.selective_merge_workspace(ui),
        }
    }
    fn device_backups(&mut self, ui: &mut egui::Ui) {
        let palette = Palette::for_context(ui.ctx());
        ui.heading(crate::localization::text("Device bank archives"));
        ui.label(crate::localization::text("Capture stored presets, the current edit buffer, settings and readable asset metadata. Every accepted item is saved before the next read."));
        ui.horizontal_wrapped(|ui| {
            let can_capture = self.services.workflow_available()
                && self.backup_origin().is_some()
                && !self.library_workflow.busy()
                && !self.controller.device_operation_pending()
                && !self.asset_slot_ui.is_running();
            if ui
                .add_enabled(
                    can_capture,
                    egui::Button::new(crate::localization::text("Capture device bank")),
                )
                .clicked()
            {
                self.start_device_backup();
            }
            if ui
                .add_enabled(
                    self.library_workflow.capture.is_some(),
                    egui::Button::new(crate::localization::text("Stop capture")),
                )
                .clicked()
            {
                self.stop_device_backup();
            }
            if ui
                .button(crate::localization::text("Import archive…"))
                .clicked()
            {
                self.services.dialog(FilePurpose::ImportDeviceBackup);
            }
            if ui
                .button(crate::localization::text("Refresh archives"))
                .clicked()
            {
                self.refresh_library_workflows();
            }
        });
        if let Some(run) = &self.library_workflow.capture {
            ui.label(std::format!(
                "{} capture: {} durable objects",
                run.origin.identity.model.name(),
                run.completed
            ));
            ui.add(
                egui::ProgressBar::new(f32::from(run.cursor) / 288.0)
                    .text(std::format!("{} / 288 planned objects", run.cursor))
                    .desired_width(ui.available_width().min(500.0)),
            );
            if run.stop.is_some() {
                ui.small(crate::localization::text("Stopping after the current save"));
            } else if run.durable.is_some() {
                ui.small(crate::localization::text(
                    "Saving the previous object to the archive",
                ));
            } else if let Some((_, object, _)) = &run.waiting {
                ui.small(std::format!("Reading {object:?}"));
            }
        }
        if !self.library_workflow.message.is_empty() {
            ui.label(crate::localization::message(&self.library_workflow.message));
        }
        ui.colored_label(palette.warning,crate::localization::text("IR/SnapTone metadata does not contain their sound data. Device asset contents remain missing from these captures."));
        ui.add_space(12.0);
        if self.library_workflow.backups.is_empty() {
            ui.label(crate::localization::text(
                "Capture your connected pedal or import a .gp200backup archive.",
            ));
        }
        for backup in self.library_workflow.backups.clone() {
            let c = backup.completeness();
            let selected = self.library_workflow.selected_backup.as_ref() == Some(&backup.id);
            let status = if backup.acquisition == BackupAcquisition::Acquiring
                && !self
                    .library_workflow
                    .capture
                    .as_ref()
                    .is_some_and(|r| r.id.as_ref() == Some(&backup.id))
            {
                crate::localization::text("Interrupted acquisition; not resumed").to_owned()
            } else {
                format!("{:?}", backup.acquisition)
            };
            if ui
                .selectable_label(
                    selected,
                    std::format!(
                        "{} · {:?} · {} / 256 stored presets · {}",
                        backup.origin.identity.model.name(),
                        backup.origin.identity.mode,
                        c.stored_presets,
                        status
                    ),
                )
                .clicked()
            {
                self.library_workflow.selected_backup = Some(backup.id.clone());
                self.library_workflow.source_start = 0;
                self.library_workflow.source_end = 255;
                self.library_workflow.restore_sources.clear();
            }
        }
        if let Some(backup) = self
            .library_workflow
            .backups
            .iter()
            .find(|b| Some(&b.id) == self.library_workflow.selected_backup.as_ref())
            .cloned()
        {
            section(ui, crate::localization::text("Archive contents"));
            let c = backup.completeness();
            ui.label(RichText::new(&c.label).strong());
            ui.label(std::format!("Stored presets {} / 256; current buffer {}; globals {}; footswitch tables {}; metadata {} / 30; missing sound contents {} / 30",c.stored_presets,yes(c.edit_buffer_captured),yes(c.globals_captured),yes(c.global_mappings_captured),c.asset_metadata,c.missing_asset_content.len()));
            ui.small(std::format!("Archive {}", backup.id));
            ui.small(std::format!(
                "Device: {}. Firmware: {}",
                backup.origin.identity.display_name,
                backup
                    .origin
                    .identity
                    .firmware
                    .as_ref()
                    .map_or(crate::localization::text("Unknown"), |f| f.version.as_str())
            ));
            ui.collapsing(crate::localization::text("Connection details"), |ui| {
                ui.add(egui::Label::new(&backup.origin.device_key).wrap());
            });
            if backup.acquisition == BackupAcquisition::Acquiring && !self.library_workflow.busy() {
                ui.colored_label(palette.warning,crate::localization::text("This capture was interrupted. Previously saved objects remain available; hardware acquisition has not resumed."));
                if ui
                    .button(crate::localization::text("Close interrupted capture"))
                    .clicked()
                {
                    self.workflow_submit(WorkflowAction::FinishBackup{backup:backup.id.clone(),outcome:BackupAcquisition::Failed{reason:crate::localization::text("Previous acquisition was interrupted before this application session").into()}},Purpose::BackupFinish);
                }
            }
            if ui
                .button(crate::localization::text("Export this archive…"))
                .clicked()
            {
                self.library_workflow.export_backup = Some(backup.id.clone());
                self.services.dialog(FilePurpose::ExportDeviceBackup);
            }
            egui::CollapsingHeader::new(crate::localization::text("Missing items and read errors"))
                .show(ui, |ui| {
                    ui.label(std::format!(
                        "Missing stored slots: {}",
                        c.missing_presets
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                    ui.label(std::format!(
                        "Missing asset metadata: {:?}",
                        c.missing_asset_metadata
                    ));
                    ui.label(std::format!(
                        "Missing sound contents: {:?}",
                        c.missing_asset_content
                    ));
                    for (object, error) in &backup.read_errors {
                        ui.label(std::format!("{object}: {error}"));
                    }
                });
            section(
                ui,
                crate::localization::text("Prepare a stored-slot restore"),
            );
            ui.horizontal_wrapped(|ui| {
                ui.label(crate::localization::text("Source slots"));
                ui.add(
                    egui::DragValue::new(&mut self.library_workflow.source_start).range(0..=255),
                );
                ui.label(crate::localization::text("through"));
                ui.add(egui::DragValue::new(&mut self.library_workflow.source_end).range(0..=255));
                ui.label(crate::localization::text("First target slot"));
                ui.add(
                    egui::DragValue::new(&mut self.library_workflow.target_offset).range(0..=255),
                );
            });
            let start = self.library_workflow.source_start;
            let end = self.library_workflow.source_end;
            let offset = self.library_workflow.target_offset;
            let selections = backup
                .presets
                .keys()
                .filter(|slot| slot.get() >= start && slot.get() <= end)
                .filter_map(|slot| {
                    SlotId::new(slot.get() - start + offset)
                        .ok()
                        .map(|target| RestoreSelection {
                            source: RestoreSource::BackupStored {
                                backup: backup.id.clone(),
                                slot: *slot,
                            },
                            target,
                        })
                })
                .collect::<Vec<_>>();
            ui.small(crate::localization::text("Only captured source slots within the range are included. Targets beyond 255 are omitted. The active pedal slot and unresolved assets are excluded by the planner."));
            ui.label(std::format!("{} explicit slot mappings", selections.len()));
            egui::CollapsingHeader::new(crate::localization::text(
                "All proposed source → target mappings",
            ))
            .show(ui, |ui| {
                for selection in &selections {
                    ui.label(std::format!(
                        "{:?} → stored slot {}",
                        selection.source,
                        selection.target
                    ));
                }
            });
            if ui
                .add_enabled(
                    !selections.is_empty()
                        && !self.library_workflow.busy()
                        && self.backup_origin().is_some(),
                    egui::Button::new(crate::localization::text(
                        "Read destinations and prepare review",
                    )),
                )
                .clicked()
            {
                self.plan_restore_sources(selections);
            }
        }
        self.restore_review(ui);
    }
    fn restore_review(&mut self, ui: &mut egui::Ui) {
        let palette = Palette::for_context(ui.ctx());
        if let Some(run) = &self.library_workflow.restore {
            section(ui, crate::localization::text("Restore progress"));
            ui.label(std::format!(
                "{:?}; {} fresh destination snapshots",
                run.phase,
                run.observed.len()
            ));
            if ui
                .button(crate::localization::text("Pause restore"))
                .clicked()
            {
                self.pause_batch_restore(crate::localization::text("Paused by you. Readback reconciliation is required before continuing an attempted write."));
            }
        }
        if let Some(plan) = self.library_workflow.plan.clone() {
            section(ui, crate::localization::text("Concrete restore review"));
            ui.label(std::format!(
                "{} applicable destinations; {} excluded selections",
                plan.entries.len(),
                plan.excluded.len()
            ));
            for note in &plan.notes {
                ui.small(note);
            }
            for entry in &plan.entries {
                egui::CollapsingHeader::new(std::format!(
                    "{:?} → stored slot {}",
                    entry.selection.source,
                    entry.selection.target
                ))
                .id_salt(("plan-entry", entry.selection.target.get()))
                .show(ui, |ui| {
                    for note in &entry.notes {
                        ui.label(note);
                    }
                    if entry.compatibility_review_required {
                        ui.colored_label(
                            palette.warning,
                            crate::localization::text(
                                "Source/target firmware compatibility requires explicit review.",
                            ),
                        );
                    }
                    diff_view::render(ui, &entry.differences);
                    ui.small(std::format!(
                        "Before raw SHA-256 {}",
                        entry.before_raw_hash.as_str()
                    ));
                    ui.small(std::format!(
                        "Expected raw SHA-256 {}",
                        entry.expected_raw_hash.as_str()
                    ));
                });
            }
            for excluded in &plan.excluded {
                ui.colored_label(
                    palette.warning,
                    std::format!(
                        "Excluded {:?} → {}: {}",
                        excluded.selection.source,
                        excluded.selection.target,
                        excluded.reasons.join("; ")
                    ),
                );
            }
            let unknown = plan.entries.iter().any(|e| e.compatibility_review_required);
            if unknown {
                ui.checkbox(
                    &mut self.library_workflow.accept_unknown_firmware,
                    crate::localization::text(
                        "I reviewed the source/target firmware uncertainty for these destinations",
                    ),
                );
            }
            let identity = self.controller.state().identity.as_ref();
            let capable = identity.is_some_and(|i| {
                self.controller
                    .state()
                    .capabilities
                    .allows(OperationKind::PresetImport, i)
            });
            if !capable {
                ui.colored_label(palette.warning,crate::localization::text("A verified stored-preset write profile is unavailable for this session. The plan remains available to inspect."));
            }
            if ui
                .add_enabled(
                    !plan.entries.is_empty()
                        && capable
                        && (!unknown || self.library_workflow.accept_unknown_firmware)
                        && !self.library_workflow.busy()
                        && !self
                            .library_workflow
                            .pending
                            .values()
                            .any(|p| matches!(p, Purpose::RestoreBegin)),
                    egui::Button::new(crate::localization::text("Apply reviewed stored-slot plan")),
                )
                .clicked()
            {
                self.apply_restore_plan();
            }
            ui.small(crate::localization::text("Before-images and an InFlight checkpoint are saved before each write. Lost acknowledgements pause the restore; writes never replay automatically."));
        }
        if !self.library_workflow.journals.is_empty() {
            section(ui, crate::localization::text("Restore checkpoints"));
        }
        for journal in self.library_workflow.journals.clone() {
            egui::CollapsingHeader::new(std::format!("{} · {:?} · {} / {} verified",journal.id,journal.state,journal.verified_count(),journal.entries.len())).id_salt(&journal.id).show(ui,|ui|{
                for entry in &journal.entries{ui.label(std::format!("Slot {}: {:?} ({} attempts)",entry.plan.selection.target,entry.state,entry.attempts));}
                ui.horizontal_wrapped(|ui|{
                    if ui.add_enabled(!self.library_workflow.busy()&&self.backup_origin().is_some(),egui::Button::new(crate::localization::text("Read back and reconcile"))).clicked(){self.run_restore_journal(journal.clone(),RestorePhase::Reconcile);}
                    if ui.add_enabled(!self.library_workflow.busy()&&journal.state==RestoreRunState::Ready&&journal.target.epoch==self.controller.state().epoch,egui::Button::new(crate::localization::text("Continue reviewed plan"))).clicked(){self.run_restore_journal(journal.clone(),RestorePhase::Dispatching);}
                });
                for entry in &journal.entries{if entry.state==RestoreEntryState::NeedsRetryReview{
                    ui.colored_label(palette.warning,std::format!("Slot {} still matches its before-image. Retry requires explicit fresh review.",entry.plan.selection.target));
                    if ui.add_enabled(!self.library_workflow.busy(),egui::Button::new(std::format!("Approve retry of slot {}",entry.plan.selection.target))).clicked(){self.workflow_submit(WorkflowAction::ApproveRetry{journal:journal.id.clone(),target:entry.plan.selection.target,observed_hash:entry.observed_raw_hash.clone()},Purpose::RestoreRetry);}
                }}
            });
        }
    }
    fn compare_library_revisions(&mut self, ui: &mut egui::Ui) {
        ui.heading(crate::localization::text("Compare sound revisions"));
        ui.label(crate::localization::text("Compare decoded settings alongside every changed record byte, including assignments and unknown fields."));
        ui.horizontal_wrapped(|ui| {
            ui.label(crate::localization::text("Load revision history:"));
            for entry in self.controller.state().library.entries.clone() {
                if ui.button(&entry.metadata.name).clicked() {
                    let intent = self.local_intent();
                    self.send(UiIntent::Library(LibraryCommand::ListVersions {
                        intent,
                        library: entry.id,
                    }));
                }
            }
        });
        let choices = self.library_pin_choices();
        for (label, before) in [
            (crate::localization::text("Before"), true),
            (crate::localization::text("After"), false),
        ] {
            let selected = if before {
                &mut self.library_workflow.compare_before
            } else {
                &mut self.library_workflow.compare_after
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(label);
                egui::ComboBox::from_id_salt(("compare-revision", before))
                    .selected_text(
                        selected
                            .as_ref()
                            .and_then(|v| choices.iter().find(|(id, _)| id == v))
                            .map_or(
                                crate::localization::text("Choose a revision"),
                                |(_, label)| label.as_str(),
                            ),
                    )
                    .width(ui.available_width().min(500.0))
                    .show_ui(ui, |ui| {
                        for (id, title) in &choices {
                            ui.selectable_value(selected, Some(id.clone()), title);
                        }
                    });
            });
        }
        if ui
            .add_enabled(
                self.library_workflow.compare_before.is_some()
                    && self.library_workflow.compare_after.is_some(),
                egui::Button::new(crate::localization::text("Compare exact revisions")),
            )
            .clicked()
        {
            self.workflow_submit(
                WorkflowAction::CompareVersions {
                    before: self
                        .library_workflow
                        .compare_before
                        .clone()
                        .expect("selected"),
                    after: self
                        .library_workflow
                        .compare_after
                        .clone()
                        .expect("selected"),
                },
                Purpose::Compare,
            );
        }
        if let Some(diff) = &self.library_workflow.comparison {
            diff_view::render(ui, diff);
        }
    }
}
fn yes(value: bool) -> &'static str {
    if value { "captured" } else { "missing" }
}
