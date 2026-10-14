//! Explicit selected-field merging between immutable library revisions and
//! existing offline drafts. Only the reviewed local reducer intent is emitted.
use super::{diff_view, library_workflows::LibraryTab, section};
use crate::{StudioApp, theme::Palette};
use eframe::egui;
use gp200_domain::*;
use gp200_library::{OfflineMergeReview, offline_merge_target_sha256, review_offline_merge};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub(crate) struct SelectiveMergeUi {
    source_library: Option<LibraryId>,
    source_version: Option<LibraryVersionId>,
    pending_source: Option<(IntentId, LibraryVersionId)>,
    source: Option<LibraryVersion>,
    target: Option<DocumentId>,
    fields: BTreeSet<OfflineMergeField>,
    review: Option<OfflineMergeReview>,
    message: String,
    pub(crate) field_rects: BTreeMap<OfflineMergeField, egui::Rect>,
    pub(crate) review_rect: Option<egui::Rect>,
    pub(crate) apply_rect: Option<egui::Rect>,
    pub(crate) back_rect: Option<egui::Rect>,
}

impl StudioApp {
    /// Open a retained immutable library version for local selected merging.
    /// Uncached versions are resolved asynchronously without opening an editor
    /// document or acquiring any device authority.
    pub fn open_selective_merge_source(&mut self, version: LibraryVersionId) {
        self.selective_merge.source_version = Some(version.clone());
        self.selective_merge.source = None;
        self.selective_merge.pending_source = None;
        self.selective_merge.fields.clear();
        self.selective_merge.review = None;
        self.selective_merge.message.clear();
        if let Some(source) = self
            .controller
            .state()
            .library
            .versions
            .get(&version)
            .cloned()
        {
            self.selective_merge.source_library = Some(source.library.clone());
            self.selective_merge.source = Some(source);
        } else if self.services.library.is_some() {
            let intent = self.local_intent();
            self.selective_merge.pending_source = Some((intent, version.clone()));
            self.send(UiIntent::Library(LibraryCommand::LoadVersion {
                intent,
                version,
            }));
        } else {
            self.selective_merge.message =
                "Local library unavailable; load an existing retained source first".into();
        }
        if self.selective_merge.target.is_none() {
            self.selective_merge.target = self.controller.active_document().and_then(|document| {
                matches!(&document.target, EditTarget::Offline(id) if id == &document.id)
                    .then(|| document.id.clone())
            });
        }
        self.library_workflow.tab = LibraryTab::Merge;
        self.send(UiIntent::Navigate(ViewId::Library));
    }

    pub(crate) fn observe_selective_merge_source(&mut self, event: &LibraryEvent) {
        let Some((expected, version_id)) = self.selective_merge.pending_source.clone() else {
            return;
        };
        match event {
            LibraryEvent::VersionLoaded { intent, version } if *intent == expected => {
                self.selective_merge.pending_source = None;
                if version.id == version_id
                    && self.selective_merge.source_version.as_ref() == Some(&version_id)
                {
                    self.selective_merge.source_library = Some(version.library.clone());
                    self.selective_merge.source = Some(version.clone());
                } else {
                    self.selective_merge.message =
                        "Source revision changed while loading; choose it again".into();
                }
                self.selective_merge.review = None;
            }
            LibraryEvent::Failed { intent, error } if *intent == expected => {
                self.selective_merge.pending_source = None;
                self.selective_merge.message = error.message.clone();
                self.selective_merge.review = None;
            }
            _ => {}
        }
    }

    fn expire_selective_merge(&mut self) {
        let source_changed = self.selective_merge.source.as_ref().is_some_and(|source| {
            self.selective_merge.source_version.as_ref() != Some(&source.id)
                || self
                    .controller
                    .state()
                    .library
                    .versions
                    .get(&source.id)
                    .is_some_and(|current| current != source)
        });
        if source_changed {
            self.selective_merge.source = None;
            self.selective_merge.review = None;
            self.selective_merge.message =
                "Source revision changed; choose it and review again".into();
        }
        let Some(review) = &self.selective_merge.review else {
            return;
        };
        let request = review.request();
        let valid = self.selective_merge.target.as_ref() == Some(&request.document)
            && self
                .selective_merge
                .fields
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                == request.fields
            && self
                .selective_merge
                .source
                .as_ref()
                .is_some_and(|source| source.snapshot == request.source)
            && self
                .controller
                .state()
                .documents
                .get(&request.document)
                .and_then(|document| offline_merge_target_sha256(document).ok())
                == Some(request.target_sha256);
        if !valid {
            self.selective_merge.review = None;
            self.selective_merge.message =
                "Review expired because the target or selection changed. Create a new review."
                    .into();
        }
    }

    /// Prepare an exact local review. A caller cannot turn it into a device job.
    pub fn review_selective_merge(
        &mut self,
        mut fields: Vec<OfflineMergeField>,
    ) -> Result<(), ServiceError> {
        self.expire_selective_merge();
        self.selective_merge.review = None;
        self.selective_merge.message.clear();
        fields.sort();
        // Preserve duplicate-field rejection rather than silently deduplicating
        // an incoming API selection. The UI naturally supplies a sorted set.
        let result = self
            .selective_merge
            .source
            .as_ref()
            .ok_or_else(|| ServiceError::invalid("Choose a source revision first"))
            .and_then(|source| {
                self.selective_merge
                    .target
                    .as_ref()
                    .and_then(|id| self.controller.state().documents.get(id))
                    .ok_or_else(|| ServiceError::invalid("Choose an existing offline target first"))
                    .and_then(|document| {
                        review_offline_merge(
                            document,
                            source.snapshot.clone(),
                            fields.clone(),
                            self.controller.catalog(),
                        )
                    })
            });
        match result {
            Ok(review) => {
                self.selective_merge.fields = fields.into_iter().collect();
                self.selective_merge.review = Some(review);
                Ok(())
            }
            Err(error) => {
                self.selective_merge.message = error.message.clone();
                Err(error)
            }
        }
    }

    fn apply_selective_merge(&mut self) {
        self.expire_selective_merge();
        let Some(review) = self.selective_merge.review.take() else {
            return;
        };
        let request = review.request();
        let id = request.document.clone();
        let revision = request.expected_revision;
        self.send(UiIntent::ApplyOfflineMerge(request));
        if self
            .controller
            .state()
            .documents
            .get(&id)
            .is_some_and(|document| document.local_revision.0 == revision.0 + 1)
        {
            // Global preset Undo/Redo acts on the active document. A merge may
            // explicitly target another offline draft, so its successful local
            // history step must become the active history context as well.
            self.controller.activate_document(&id);
            self.selective_merge.message =
                "Selected settings applied as one local undo step".into();
        } else {
            self.selective_merge.message =
                "Merge was rejected; review the current target again".into();
        }
    }

    /// Return to checkboxes while retaining the explicit field selection.
    pub fn show_selective_merge_selection(&mut self) {
        self.selective_merge.review = None;
    }

    /// Compact inspection metadata for local automation/native QA. Full hashes
    /// bind the exact review; these do not authorize transmission or asset writes.
    pub fn selective_merge_report(&self) -> Option<serde_json::Value> {
        let review = self.selective_merge.review.as_ref()?;
        let request = review.request();
        Some(serde_json::json!({
            "source_version": self.selective_merge.source_version,
            "target_document": request.document,
            "target_revision": request.expected_revision,
            "source_sha256": request.source_sha256,
            "target_sha256": request.target_sha256,
            "result_sha256": request.result_sha256,
            "review_sha256": request.review_sha256,
            "selected_fields": request.fields,
            "setting_changes": review.diff().known.len(),
            "raw_record_changes": review.diff().raw.len(),
            "before_raw_sha256": JobSha256::of(review.before().raw.payload()),
            "after_raw_sha256": JobSha256::of(review.after().raw.payload()),
            "operation": "offline-selected-merge", "device_authority": false,
        }))
    }

    pub(super) fn selective_merge_workspace(&mut self, ui: &mut egui::Ui) {
        self.expire_selective_merge();
        self.selective_merge.field_rects.clear();
        self.selective_merge.review_rect = None;
        self.selective_merge.apply_rect = None;
        self.selective_merge.back_rect = None;
        let palette = Palette::for_context(ui.ctx());
        ui.heading(crate::localization::text("Merge selected settings"));
        ui.label(crate::localization::text("Keep the target's unselected settings and raw records. Review model dependencies before applying."));
        self.merge_source_and_target(ui);
        if !self.selective_merge.message.is_empty() {
            ui.colored_label(
                palette.warning,
                crate::localization::message(&self.selective_merge.message),
            );
        }
        ui.add_space(8.0);
        if let Some(review) = self.selective_merge.review.clone() {
            ui.horizontal_wrapped(|ui| {
                let apply = ui.button(crate::localization::text("Apply locally"));
                self.selective_merge.apply_rect = Some(apply.rect);
                if apply.clicked() {
                    self.apply_selective_merge();
                }
                let back = ui.button(crate::localization::text("Back to selection"));
                self.selective_merge.back_rect = Some(back.rect);
                if back.clicked() {
                    self.show_selective_merge_selection();
                }
                ui.small(crate::localization::text(
                    "One local undo step. The pedal is unaffected.",
                ));
            });
            section(ui, crate::localization::text("Reviewed changes"));
            ui.small(std::format!("{} selected fields", review.fields().len()));
            diff_view::render(ui, review.diff());
            egui::CollapsingHeader::new(crate::localization::text("Exact review fingerprints"))
                .show(ui, |ui| {
                    for (label, hash) in [
                        ("Source SHA-256", review.source_sha256()),
                        ("Target SHA-256", review.target_sha256()),
                        ("Review SHA-256", review.request().review_sha256),
                    ] {
                        ui.label(crate::localization::text(label));
                        ui.monospace(hex(&hash));
                    }
                });
            return;
        }
        let source = self.selective_merge.source.clone();
        let target = self
            .selective_merge
            .target
            .as_ref()
            .and_then(|id| self.controller.state().documents.get(id))
            .cloned();
        ui.horizontal_wrapped(|ui| {
            let enabled =
                source.is_some() && target.is_some() && !self.selective_merge.fields.is_empty();
            let response = ui.add_enabled(
                enabled,
                egui::Button::new(crate::localization::text("Review selected changes")),
            );
            self.selective_merge.review_rect = Some(response.rect);
            if response.clicked() {
                let fields = self.selective_merge.fields.iter().cloned().collect();
                let _ = self.review_selective_merge(fields);
            }
            if ui
                .button(crate::localization::text("Clear selection"))
                .clicked()
            {
                self.selective_merge.fields.clear();
                self.selective_merge.message.clear();
            }
            ui.small(std::format!(
                "{} selected fields",
                self.selective_merge.fields.len()
            ));
        });
        if let (Some(source), Some(target)) = (&source, &target) {
            self.merge_checkboxes(ui, &source.snapshot, target);
        } else if target.is_none() {
            ui.label(crate::localization::text(
                "No offline document is open. Open a local revision in the editor first.",
            ));
        } else {
            ui.label(crate::localization::text("Choose a source revision first"));
        }
    }

    fn merge_source_and_target(&mut self, ui: &mut egui::Ui) {
        let entries = self.controller.state().library.entries.clone();
        let previous = self.selective_merge.source_library.clone();
        ui.horizontal_wrapped(|ui| {
            let label = ui.label(crate::localization::text("Source preset"));
            let name = self
                .selective_merge
                .source_library
                .as_ref()
                .and_then(|id| entries.iter().find(|entry| &entry.id == id))
                .map(|entry| entry.metadata.name.clone())
                .unwrap_or_else(|| crate::localization::text("Choose source").into());
            egui::ComboBox::from_id_salt("merge-source-preset")
                .selected_text(name)
                .wrap()
                .width(ui.available_width().min(320.0))
                .show_ui(ui, |ui| {
                    for entry in &entries {
                        ui.selectable_value(
                            &mut self.selective_merge.source_library,
                            Some(entry.id.clone()),
                            &entry.metadata.name,
                        );
                    }
                })
                .response
                .labelled_by(label.id);
        });
        if self.selective_merge.source_library != previous {
            self.selective_merge.source = None;
            self.selective_merge.source_version = None;
            self.selective_merge.pending_source = None;
            self.selective_merge.fields.clear();
            self.selective_merge.review = None;
            self.selective_merge.message.clear();
            if let Some(library) = self.selective_merge.source_library.clone() {
                let intent = self.local_intent();
                self.send(UiIntent::Library(LibraryCommand::ListVersions {
                    intent,
                    library,
                }));
            }
        }
        let mut versions: Vec<_> = self
            .controller
            .state()
            .library
            .versions
            .values()
            .filter(|version| {
                Some(&version.library) == self.selective_merge.source_library.as_ref()
            })
            .cloned()
            .collect();
        // The latest ID can be chosen immediately while ListVersions populates
        // older retained revisions. Its snapshot is loaded by the library worker.
        let latest = entries
            .iter()
            .find(|entry| Some(&entry.id) == self.selective_merge.source_library.as_ref())
            .map(|entry| entry.latest_version.clone());
        versions.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
        let old_version = self.selective_merge.source_version.clone();
        ui.horizontal_wrapped(|ui| {
            let label = ui.label(crate::localization::text("Source revision"));
            egui::ComboBox::from_id_salt("merge-source-version")
                .selected_text(
                    self.selective_merge
                        .source_version
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| crate::localization::text("Choose revision").into()),
                )
                .wrap()
                .width(ui.available_width().min(320.0))
                .show_ui(ui, |ui| {
                    for version in &versions {
                        ui.selectable_value(
                            &mut self.selective_merge.source_version,
                            Some(version.id.clone()),
                            format!("{}  {}", version.created_at, version.id),
                        );
                    }
                    if let Some(latest) = &latest
                        && !versions.iter().any(|version| &version.id == latest)
                    {
                        ui.selectable_value(
                            &mut self.selective_merge.source_version,
                            Some(latest.clone()),
                            latest.to_string(),
                        );
                    }
                })
                .response
                .labelled_by(label.id);
        });
        if old_version != self.selective_merge.source_version
            && let Some(version) = self.selective_merge.source_version.clone()
        {
            self.open_selective_merge_source(version);
        }
        if self.selective_merge.pending_source.is_some() {
            ui.label(crate::localization::text(
                "Loading immutable source revision…",
            ));
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(50));
        }
        let old_target = self.selective_merge.target.clone();
        ui.horizontal_wrapped(|ui| {
            let label = ui.label(crate::localization::text("Target offline document"));
            let name = self
                .selective_merge
                .target
                .as_ref()
                .and_then(|id| self.controller.state().documents.get(id))
                .map(|document| {
                    document
                        .draft
                        .metadata
                        .name
                        .clone()
                        .unwrap_or_else(|| document.id.to_string())
                })
                .unwrap_or_else(|| crate::localization::text("Choose offline target").into());
            egui::ComboBox::from_id_salt("merge-target-document")
                .selected_text(name)
                .wrap()
                .width(ui.available_width().min(320.0))
                .show_ui(ui, |ui| {
                    for document in self.controller.state().documents.values() {
                        if matches!(&document.target, EditTarget::Offline(id) if id == &document.id)
                        {
                            let name = document.draft.metadata.name.as_deref().unwrap_or("");
                            ui.selectable_value(
                                &mut self.selective_merge.target,
                                Some(document.id.clone()),
                                format!("{name}  {}", document.id),
                            );
                        }
                    }
                })
                .response
                .labelled_by(label.id);
        });
        if self.selective_merge.target != old_target {
            self.selective_merge.review = None;
            self.selective_merge.fields.clear();
            self.selective_merge.message.clear();
        }
        // Combobox changes must retire a review within this same frame, before
        // the Apply button can be rendered or accept its input.
        self.expire_selective_merge();
    }

    fn merge_checkbox(
        &mut self,
        ui: &mut egui::Ui,
        field: OfflineMergeField,
        label: &str,
        enabled: bool,
    ) {
        let mut selected = self.selective_merge.fields.contains(&field);
        let response = ui.add_enabled(enabled, egui::Checkbox::new(&mut selected, label));
        self.selective_merge
            .field_rects
            .insert(field.clone(), response.rect);
        if response.changed() {
            self.selective_merge.review = None;
            self.selective_merge.message.clear();
            if selected {
                match &field {
                    OfflineMergeField::Module(module) => self.selective_merge.fields.retain(|existing| {
                        !matches!(existing, OfflineMergeField::Enabled(id) if id == module)
                            && !matches!(existing, OfflineMergeField::Parameter { module: id, .. } if id == module)
                    }),
                    OfflineMergeField::Enabled(module) | OfflineMergeField::Parameter { module, .. } => {
                        self.selective_merge.fields.remove(&OfflineMergeField::Module(*module));
                    }
                    _ => {},
                }
                self.selective_merge.fields.insert(field);
            } else {
                self.selective_merge.fields.remove(&field);
            }
        }
    }

    fn merge_checkboxes(
        &mut self,
        ui: &mut egui::Ui,
        source: &PresetSnapshot,
        target: &EditorDocument,
    ) {
        // Saved snapshots may predate the assignment projection while retaining
        // the exact native records. Match core review migration for display;
        // the immutable source snapshot and its review hash remain untouched.
        let source = PresetDraft::from_snapshot(source);
        egui::CollapsingHeader::new(crate::localization::text("Patch metadata and annotations"))
            .default_open(true)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (field, label) in [
                        (MergeMetadataField::Name, "Name"),
                        (MergeMetadataField::Tempo, "Tempo"),
                        (MergeMetadataField::Level, "Patch level"),
                        (MergeMetadataField::NativePan, "Native Pan"),
                        (MergeMetadataField::NativeStyle, "Native style"),
                        (MergeMetadataField::Author, "Author"),
                        (MergeMetadataField::Style, "Style annotation"),
                        (MergeMetadataField::Notes, "Notes"),
                        (MergeMetadataField::PanAnnotation, "Pan annotation"),
                        (MergeMetadataField::FxSendLevel, "FX send level"),
                        (MergeMetadataField::FxReturnLevel, "FX return level"),
                        (MergeMetadataField::FxLoopMode, "FX-loop mode"),
                    ] {
                        self.merge_checkbox(
                            ui,
                            OfflineMergeField::Metadata(field),
                            crate::localization::text(label),
                            true,
                        );
                    }
                });
            });
        self.merge_checkbox(
            ui,
            OfflineMergeField::Chain,
            crate::localization::text("Signal-chain order"),
            true,
        );
        section(ui, crate::localization::text("Effect settings"));
        for module in &source.view.modules {
            let target_module = target.draft.view.module(module.module);
            let model = self
                .controller
                .catalog()
                .model_by_id(module.module.get(), module.model.0)
                .cloned();
            let name = model
                .as_ref()
                .map(|model| model.name.as_str())
                .unwrap_or("?");
            let label = format!("{}  {name}", module.module.name());
            egui::CollapsingHeader::new(label).id_salt(("merge-module", module.module.get()))
                .default_open(self.selective_merge.fields.iter().any(|field| matches!(field, OfflineMergeField::Parameter { module: id, .. } if id == &module.module)))
                .show(ui, |ui| {
                    self.merge_checkbox(ui, OfflineMergeField::Module(module.module), crate::localization::text("All retained module settings"), target_module.is_some());
                    self.merge_checkbox(ui, OfflineMergeField::Enabled(module.module), crate::localization::text("Enabled state"), target_module.is_some());
                    let same_model = target_module.is_some_and(|target| target.model == module.model);
                    if !same_model {
                        ui.small(crate::localization::text("Different models: select the complete module and every affected assignment."));
                    }
                    if let Some(model) = &model {
                        for parameter in &model.parameters {
                            let Ok(id) = ParamId::new(parameter.parameter_id) else { continue };
                            let label = crate::localization::text(&parameter.name);
                            self.merge_checkbox(ui, OfflineMergeField::Parameter { module: module.module, expected_model: module.model, parameter: id }, label, same_model);
                        }
                    }
                });
        }
        egui::CollapsingHeader::new(crate::localization::text("Assignments and FX routing")).show(ui, |ui| {
            ui.small(crate::localization::text("Model changes require every affected target EXP and quick assignment. Unresolved references are rejected at review."));
            for assignment in &source.view.patch_assignments.expressions {
                let slot = assignment.record.slot;
                let label = std::format!("EXP group {} / slot {}", slot.group()+1, slot.slot()+1);
                self.merge_checkbox(ui, OfflineMergeField::Expression(slot), &label, true);
            }
            for assignment in &source.view.patch_assignments.ctrls {
                let label = format!("CTRL {}", assignment.record.slot+1);
                self.merge_checkbox(ui, OfflineMergeField::Ctrl(assignment.record.slot), &label, true);
            }
            for assignment in &source.view.patch_assignments.quicks {
                let label = std::format!("Quick control {}", assignment.record.index+1);
                self.merge_checkbox(ui, OfflineMergeField::Quick(assignment.record.index), &label, true);
            }
            self.merge_checkbox(ui, OfflineMergeField::FxLoopPositions, crate::localization::text("FX-loop positions"), source.view.patch_assignments.routing.is_some());
            for (field, label) in [
                (OfflineMergeField::LegacyExpressions, "Legacy expression assignments"),
                (OfflineMergeField::LegacyFootswitches, "Legacy footswitch assignments"),
                (OfflineMergeField::LegacyQuickControls, "Legacy quick controls"),
            ] { self.merge_checkbox(ui, field, crate::localization::text(label), true); }
        });
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
#[path = "selective_merge_tests.rs"]
mod tests;
