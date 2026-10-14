//! Bounded retained-attempt presentation. No read or export is a resume permit.
use super::firmware_updater::{copy_value, diagnostics, render_rows, row, section};
use crate::{FirmwareCoordinatorPhase as Phase, StudioApp, theme::Palette};
use eframe::egui::{self, RichText};

impl StudioApp {
    pub(crate) fn begin_firmware_attempt_export(
        &mut self,
        detail: &gp200_library::FirmwareHistoryDetail,
    ) {
        if self.firmware_ui.updater.attempt_dialog.is_some() {
            return;
        }
        let view = self.firmware_coordinator_view();
        self.firmware_ui.updater.export_sequence += 1;
        let id = self.firmware_ui.updater.export_sequence;
        if self
            .services
            .try_dialog(crate::runtime::FilePurpose::ExportFirmwareAttempt(id))
        {
            self.firmware_ui.updater.attempt_dialog = Some((
                id,
                view.generation,
                detail.attempt.clone(),
                detail.prefix.clone(),
            ));
        }
    }
    pub(crate) fn finish_firmware_attempt_dialog(
        &mut self,
        id: u64,
        destination: Option<std::path::PathBuf>,
    ) {
        if self
            .firmware_ui
            .updater
            .attempt_dialog
            .as_ref()
            .map(|d| d.0)
            != Some(id)
        {
            self.firmware_ui.updater.message = "An old attempt export dialog reply was ignored. Select a destination for the current checked attempt.".into();
            return;
        }
        let (_, generation, attempt, prefix) =
            self.firmware_ui.updater.attempt_dialog.take().unwrap();
        let Some(destination) = destination else {
            return;
        };
        let view = self.firmware_coordinator_view();
        if view.generation != generation
            || view
                .detail
                .as_ref()
                .is_none_or(|d| d.attempt != attempt || d.prefix != prefix)
        {
            self.firmware_ui.updater.message = "The checked attempt or its prefix changed while the export dialog was open. No export was requested; inspect the current attempt and export again.".into();
            return;
        }
        let result = self.export_firmware_attempt(attempt, prefix, destination);
        self.firmware_ui.updater.message = match result {
            Ok(()) => {
                "Independently checking and exporting the exact original attempt and prefix…".into()
            }
            Err(error) => error.message,
        };
    }
    pub(crate) fn firmware_history_screen(&mut self, ui: &mut egui::Ui) {
        ui.heading("History and recovery");
        if let Some(preview) = self.firmware_ui.updater.preview.clone() {
            ui.label(RichText::new(&preview.title).strong());
            ui.label(&preview.status);
            render_rows(ui, &preview.rows);
            diagnostics(ui, "history-preview", &preview.diagnostics, 0);
            self.firmware_recovery_guidance(ui);
            return;
        }
        let view = self.firmware_coordinator_view();
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        self.firmware_linux_recovery_history(ui, &view);
        let can_read = matches!(
            view.phase,
            Phase::Idle | Phase::Preparing | Phase::ReadOnlyReview
        );
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(can_read, egui::Button::new("Refresh attempt history"))
                .clicked()
            {
                let result = self.request_firmware_history(None);
                self.firmware_ui.updater.message = match result {
                    Ok(()) => "Checking a bounded page of original retained attempts…".into(),
                    Err(error) => error.message,
                };
            }
            if let Some(cursor) = view.history.as_ref().and_then(|page| page.next.clone())
                && ui
                    .add_enabled(can_read, egui::Button::new("Next 32 attempts"))
                    .clicked()
            {
                let result = self.request_firmware_history(Some(cursor));
                self.firmware_ui.updater.message = match result {
                    Ok(()) => "Checking the next correlated roster page…".into(),
                    Err(error) => error.message,
                };
            }
        });
        if !can_read {
            ui.colored_label(Palette::for_context(ui.ctx()).warning, "The original library store belongs to preparation, retirement, or recovery. Historical reads and exports wait for its genuine return.");
        }
        if let Some(page) = &view.history {
            ui.small(format!(
                "{} shown / {} retained attempts; each checked page is bounded to 32",
                page.attempts.len(),
                page.total
            ));
            copy_value(ui, "Roster SHA-256", page.roster_sha256.as_str());
            for attempt in page.attempts.iter().take(32) {
                let selected = view
                    .detail
                    .as_ref()
                    .is_some_and(|detail| &detail.attempt == attempt);
                if ui
                    .add_enabled(
                        can_read,
                        egui::Button::selectable(selected, attempt.as_str()),
                    )
                    .clicked()
                {
                    let result = self.inspect_firmware_attempt(attempt.clone());
                    self.firmware_ui.updater.message = match result {
                        Ok(()) => "Checking the selected attempt and its retained raw tail…".into(),
                        Err(error) => error.message,
                    };
                }
            }
        } else {
            ui.label("No checked history page loaded. Refresh history to inspect the local original journals.");
        }
        if let Some(detail) = &view.detail {
            section(ui, "Selected original attempt");
            copy_value(ui, "Attempt identity", detail.attempt.as_str());
            row(
                ui,
                "Accepted prefix",
                &format!(
                    "{} records / {} bytes",
                    detail.prefix.accepted_records, detail.prefix.accepted_bytes
                ),
            );
            copy_value(
                ui,
                "Source snapshot SHA-256",
                detail.prefix.source_snapshot_sha256.as_str(),
            );
            if let Some(hash) = &detail.prefix.prefix_sha256 {
                copy_value(ui, "Accepted prefix SHA-256", hash.as_str());
            }
            row(
                ui,
                "Export kind",
                &format!("{:?}; local evidence only", detail.kind),
            );
            if let Some(error) = &detail.load_error {
                ui.colored_label(Palette::for_context(ui.ctx()).warning, error);
            }
            if let Some(checked) = &detail.checked {
                row(
                    ui,
                    "Local checkpoint",
                    if checked.local_checkpoint_confirmed {
                        "Confirmed local durable prefix; not a pedal checkpoint"
                    } else {
                        "Local checkpoint unconfirmed"
                    },
                );
                if let Some(projection) = &checked.projection {
                    row(
                        ui,
                        "Observation origin",
                        &format!("{:?}", projection.origin()),
                    );
                    row(
                        ui,
                        "Recorded closure",
                        &format!(
                            "{:?}; host closure does not establish device completion",
                            projection.recorded_closure()
                        ),
                    );
                    row(
                        ui,
                        "Derived reopen state",
                        &format!("{:?}", projection.state()),
                    );
                    row(
                        ui,
                        "Source offered / simulated",
                        &format!(
                            "{} / {} bytes; not device-verified source bytes",
                            projection.offered_source_bytes(),
                            projection.simulated_source_bytes()
                        ),
                    );
                } else {
                    row(
                        ui,
                        "Derived reopen state",
                        "Missing or malformed projection; device mode unknown",
                    );
                }
                row(
                    ui,
                    "Device mode",
                    if checked.possible_device_update_mode {
                        "May remain in update mode; no accepted fresh normal verification"
                    } else {
                        "Unknown; absence of an update-mode flag does not prove normal mode"
                    },
                );
                if let Some(tail) = &checked.raw_tail {
                    row(
                        ui,
                        "Retained original tail",
                        &format!(
                            "{} bytes at offset {}; preserved separately from the accepted prefix",
                            tail.bytes, tail.offset
                        ),
                    );
                    copy_value(ui, "Tail SHA-256", &tail.sha256);
                }
                section(ui, "Recent record window");
                ui.small(format!(
                    "{} shown / {} accepted records; older records remain in the checked export",
                    checked.recent.len(),
                    checked.accepted_records
                ));
                for record in checked.recent.iter().take(32) {
                    row(
                        ui,
                        &format!("Record {}", record.sequence),
                        &format!(
                            "{}; offset {}; {} bytes; {}",
                            record.label, record.offset, record.bytes, record.sha256
                        ),
                    );
                }
            }
            diagnostics(ui, "history-detail", &detail.issues, detail.omitted_issues);
            egui::CollapsingHeader::new("Pinned original files").show(ui, |ui| {
                for file in detail.files.iter().take(32) {
                    row(
                        ui,
                        &file.path,
                        &format!("{} bytes; {}", file.bytes, file.sha256.as_str()),
                    );
                }
                if detail.files.len() > 32 {
                    ui.small(format!(
                        "{} additional source files retained in export",
                        detail.files.len() - 32
                    ));
                }
            });
            let response = ui.add_enabled(
                can_read && self.firmware_ui.updater.attempt_dialog.is_none(),
                egui::Button::new("Export checked attempt…"),
            );
            self.firmware_record_widget("Export checked attempt…", &response);
            if response.clicked() {
                self.begin_firmware_attempt_export(detail);
            }
            ui.small("Export independently checks the retained originals. It never permits Resume, Retry, or a device write.");
        }
        if let Some(exported) = &view.exported {
            section(ui, "Checked local export");
            row(ui, "Attempt", exported.attempt.as_str());
            row(
                ui,
                "Saved report",
                &exported.destination.display().to_string(),
            );
            copy_value(ui, "Archive SHA-256", exported.archive_sha256.as_str());
            row(
                ui,
                "Exported material",
                &format!(
                    "{} bytes / {} original files / {:?}",
                    exported.archive_bytes, exported.files, exported.kind
                ),
            );
            ui.small(
                "Checked report export is a local outcome; device completion remains unverified.",
            );
        }
        diagnostics(
            ui,
            "history-coordinator",
            &view.diagnostics,
            view.omitted_diagnostics,
        );
        self.firmware_recovery_guidance(ui);
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    fn firmware_linux_recovery_history(
        &mut self,
        ui: &mut egui::Ui,
        view: &crate::FirmwareCoordinatorView,
    ) {
        self.firmware_coordinator.ensure_linux_history();
        self.firmware_linux_retained_history(ui, view);
        section(ui, "New full-image recovery");
        ui.label("A disconnected or update-mode GP-200 can be reviewed here. The app checks the original local unresolved lineage and the current connection after taking ownership. Missing, imported, corrupt or ambiguous history blocks the review.");
        ui.label("Select and inspect GP-200 firmware 1.8.2 first. This review retains your exact saved local draft, excluded from pedal content. No fresh normal backup is possible; historical material keeps its original age, all 20 installed IR audio slots are unavailable, and restoration remains unknown.");
        if let Some(blocker) = self.firmware_recovery_draft_blocker() {
            ui.label(egui::RichText::new(blocker).color(ui.visuals().warn_fg_color));
            if self.recovery_ui.found.is_some() {
                let response = ui.button("Review previous offline documents");
                self.firmware_record_widget("Review previous offline documents", &response);
                if response.clicked() {
                    self.open_recovery_review();
                }
            }
        }
        let pending = self.firmware_coordinator.linux_completion_helper_pending();
        let active = self.firmware_coordinator.linux_recovery_active();
        let response = ui.add_enabled(
            view.can_review_linux_recovery && self.firmware_ui.image.is_some() && !pending,
            egui::Button::new("Review a new full-image recovery"),
        );
        self.firmware_record_widget("Review a new full-image recovery", &response);
        if response.clicked() {
            let result = self.review_linux_recovery();
            self.firmware_ui.updater.message = match result {
                Ok(()) => "Freezing local work and checking a new recovery review; no device query has been approved.".into(),
                Err(error) => error.message,
            };
        }
        if active || self.linux_recovery_outcome().is_some() {
            if pending {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Preparing or recording this exact recovery review…");
                });
            }
            if view.phase == Phase::Preparing {
                ui.label("Waiting for the exact local draft's durable Saved result and every original editor/runtime blocker to settle.");
                diagnostics(ui, "recovery-local-blockers", &view.blockers, 0);
                if !pending && ui.button("Rebind settled local work").clicked() {
                    let result = self.refresh_firmware_preparation();
                    self.firmware_ui.updater.message = match result {
                        Ok(()) => "Bound the settled local editor generation; its exact Saved draft is still required.".into(),
                        Err(error) => error.message,
                    };
                }
            }
            let history = self
                .firmware_coordinator
                .linux_recovery_history()
                .or_else(|| {
                    self.linux_recovery_outcome()
                        .and_then(|outcome| outcome.history)
                });
            if let Some(history) = &history {
                recovery_history_summary(ui, &serde_json::to_value(history).unwrap_or_default());
            }
            if let Some(frame) = self.firmware_ui.updater.completion_presentation.clone() {
                let live = frame.view;
                if history.is_none()
                    && let Some(history) = live.review.as_ref().and_then(|review| {
                        review
                            .get("history")
                            .or_else(|| review.get("recovery_history"))
                    })
                {
                    recovery_history_summary(ui, history);
                }
                self.firmware_linux_live_progress(ui, live, view, pending, frame.fresh);
            }
            if active {
                let response = ui.button("Stop recovery host");
                self.firmware_record_widget("Stop recovery host", &response);
                if response.clicked() {
                    self.stop_firmware_preparation();
                }
                ui.small("Stop ends bounded host work and retains the attempt. A new recovery transfers the entire image from section zero; it provides no Resume, automatic Retry, reset, power or cable action.");
                if view.phase == Phase::StopRequested && pending {
                    ui.small("Waiting for the original local review helper to finish. An unsealed preparation can be cancelled after its helper and runtime work settle; device recovery remains unresolved.");
                }
            }
        }
    }

    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    fn firmware_linux_retained_history(
        &mut self,
        ui: &mut egui::Ui,
        view: &crate::FirmwareCoordinatorView,
    ) {
        section(ui, "Retained Linux firmware history");
        let (pending, error, selected) = self.firmware_coordinator.linux_history_status();
        let idle = view.phase == Phase::Idle;
        let response = ui.add_enabled(
            idle && !pending,
            egui::Button::new("Refresh Linux firmware history"),
        );
        self.firmware_record_widget("Refresh Linux firmware history", &response);
        if response.clicked() {
            let result = self.refresh_linux_firmware_history();
            self.firmware_ui.updater.message = result.map_or_else(
                |error| error.message,
                |_| "Checking retained local Linux journals…".into(),
            );
        }
        if pending {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Checking the original library root and retained local journal prefixes…");
            });
        }
        if let Some(error) = error {
            diagnostics(ui, "linux-catalogue-error", &[error], 0);
        }
        let Some(catalogue) = self.firmware_coordinator.linux_history() else {
            return;
        };
        ui.small("Descriptive local history only. Completed jobs remain visible after restart. Every new recovery rechecks the actual original store, full lineage and current connection; these rows provide no Resume or transfer approval.");
        let offset = self.firmware_coordinator.linux_history_page_offset();
        ui.horizontal_wrapped(|ui| {
            ui.small(format!(
                "{}–{} / {} local attempts; at most 32 rows per page",
                if catalogue.rows.is_empty() {
                    0
                } else {
                    offset + 1
                },
                (offset + 32).min(catalogue.rows.len()),
                catalogue.rows.len()
            ));
            if ui
                .add_enabled(offset > 0, egui::Button::new("Previous Linux history page"))
                .clicked()
            {
                self.firmware_coordinator.change_linux_history_page(false);
            }
            if ui
                .add_enabled(
                    offset + 32 < catalogue.rows.len(),
                    egui::Button::new("Next Linux history page"),
                )
                .clicked()
            {
                self.firmware_coordinator.change_linux_history_page(true);
            }
        });
        if catalogue.rows.is_empty() {
            ui.label("No checked local Linux firmware journal is retained for this original library root.");
        }
        let selectable = idle && !pending && catalogue.diagnostics.is_empty();
        if selected.is_some()
            && ui
                .add_enabled(
                    selectable,
                    egui::Button::new("Use the unique unresolved lineage"),
                )
                .clicked()
        {
            let _ = self.firmware_coordinator.select_linux_history(None);
        }
        for attempt in catalogue.rows.iter().skip(offset).take(32) {
            let state = linux_history_state_label(attempt);
            let title = format!("{} · {}", state, attempt.attempt.as_str());
            egui::CollapsingHeader::new(title).id_salt(("linux-history",attempt.attempt.as_str())).show(ui, |ui| {
                row(ui,"Job", &attempt.purpose.chars().take(160).collect::<String>());
                row(ui,"Local closure", &attempt.closure_status.chars().take(256).collect::<String>());
                row(ui,"Original association", &format!("{} · {}",attempt.original_association.topology,attempt.original_association.serial).chars().take(512).collect::<String>());
                row(ui,"Linked predecessors", &attempt.predecessors.len().to_string());
                if let Some(capture) = &attempt.historical_capture {
                    row(ui,"Historical capture", &format!("{} milliseconds since Unix epoch; original age, never fresh",capture.prepared_wall_millis));
                    ui.small(capture.status.chars().take(512).collect::<String>());
                }
                row(ui,"Installed IR audio", "All 20 slots unavailable; restoration unknown");
                egui::CollapsingHeader::new("Checked local prefix details").id_salt(("linux-prefix",attempt.attempt.as_str())).show(ui, |ui| {
                    for (name,prefix) in [("Registry",&attempt.registry),("Journal",&attempt.journal)] {
                        row(ui,name,&prefix.path.display().to_string().chars().take(1024).collect::<String>());
                        row(ui,"Bytes",&prefix.bytes.to_string());
                        copy_value(ui,"SHA-256",&prefix.sha256);
                    }
                    copy_value(ui,"Source review SHA-256",&attempt.source_review_sha256);
                    ui.small("Complete original JSON remains in the retained journal on disk.");
                });
                if !attempt.resolved {
                    let response = ui.add_enabled(selectable, egui::Button::selectable(selected.as_ref()==Some(&attempt.attempt), "Select this unresolved local attempt"));
                    self.firmware_record_widget("Select this unresolved local attempt", &response);
                    if response.clicked() {
                        let result = self.firmware_coordinator.select_linux_history(Some(attempt.attempt.clone()));
                        self.firmware_ui.updater.message = result.map_or_else(|error|error.message, |_|"Selected a descriptive local row; new recovery still rechecks it after original store retirement.".into());
                    }
                }
            });
        }
        diagnostics(ui, "linux-catalogue-diagnostics", &catalogue.diagnostics, 0);
        if !catalogue.diagnostics.is_empty() {
            ui.colored_label(Palette::for_context(ui.ctx()).warning,"Incomplete or corrupt local history blocks row selection. A new recovery owner must independently validate the full lineage.");
        }
    }

    fn firmware_recovery_guidance(&mut self, ui: &mut egui::Ui) {
        section(ui, "Recovery guidance");
        ui.label("Interrupted or quarantined attempts require a new reviewed whole-image job; retained reports never authorize Resume or Retry. Preserve the original report and check Valeton’s official recovery guidance.");
        ui.label("Only Linux is currently available; access to a Windows computer may be arranged if needed. Vendor recovery has not been exercised, and entering update mode may require a completed update to return to normal.");
        ui.small("Opening guidance is a local navigation action. It does not approve a download, bootloader entry, firmware flash, reset, or recovery write.");
        if ui
            .button("Open official GP-200 recovery guidance")
            .clicked()
        {
            self.services
                .open_official_link(super::help::OfficialLink::Faq);
        }
    }
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
pub(super) fn linux_history_state_label(
    row: &gp200_transport::LinuxFirmwareHistoryRow,
) -> &'static str {
    match (row.local_state, row.resolved) {
        (2, true) => "Closed without firmware writes",
        (5, true) => "Completed local update",
        (6, true) => "Resolved by linked recovery",
        (7, true) => "Completed full-image recovery",
        (_, false) => "Unresolved / held",
        _ => "Closed local attempt",
    }
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn recovery_history_summary(ui: &mut egui::Ui, history: &serde_json::Value) {
    section(ui, "Checked original lineage");
    if let Some(selected) = history.get("selected").and_then(|value| value.as_str()) {
        row(ui, "Original selected attempt", selected);
    }
    if let Some(predecessors) = history
        .get("predecessors")
        .and_then(|value| value.as_array())
    {
        row(
            ui,
            "Checked unresolved predecessors",
            &predecessors.len().to_string(),
        );
        egui::CollapsingHeader::new("Original retained prefix details").show(ui, |ui| {
            for predecessor in predecessors.iter().take(16) {
                super::firmware_updater::render_rows(
                    ui,
                    &super::firmware_updater::completion_review_summary(predecessor),
                );
                for name in ["attempt", "image_sha256", "source_job_sha256"] {
                    if let Some(value) = predecessor.get(name) {
                        let text = value
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| value.to_string());
                        row(ui, name, &text.chars().take(1024).collect::<String>());
                    }
                }
                if let Some(journal) = predecessor.get("journal") {
                    for name in ["path", "bytes", "sha256"] {
                        if let Some(value) = journal.get(name) {
                            let text = value
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| value.to_string());
                            row(ui, name, &text.chars().take(1024).collect::<String>());
                        }
                    }
                }
            }
            if predecessors.len() > 16 {
                ui.small("Additional original prefixes remain in the complete review on disk.");
            }
        });
    }
    if let Some(captures) = history
        .get("historical_captures")
        .and_then(|value| value.as_array())
    {
        for capture in captures.iter().take(4) {
            let recorded = capture
                .get("prepared_wall_millis")
                .and_then(|value| value.as_u64());
            let status = capture
                .get("status")
                .and_then(|value| value.as_str())
                .unwrap_or("Unknown");
            row(
                ui,
                "Original backup status",
                &status.chars().take(512).collect::<String>(),
            );
            if let Some(recorded) = recorded {
                row(
                    ui,
                    "Original capture time",
                    &format!("{recorded} milliseconds since Unix epoch; historical, never fresh"),
                );
            }
            if let Some(acquisition) = capture.get("acquisition").and_then(|value| value.as_str()) {
                row(
                    ui,
                    "Original acquisition",
                    &acquisition.chars().take(256).collect::<String>(),
                );
            }
        }
        if captures.len() > 4 {
            ui.small("Additional historical capture metadata remains in the complete review.");
        }
    }
    row(
        ui,
        "New whole-image job",
        "Section 0 / block 0; fresh query and separate transfer approval required",
    );
    row(
        ui,
        "Fresh normal backup",
        "Unavailable in update mode; original capture is historical",
    );
    row(
        ui,
        "Installed IR audio",
        "All 20 slots unavailable; restoration unknown",
    );
}
