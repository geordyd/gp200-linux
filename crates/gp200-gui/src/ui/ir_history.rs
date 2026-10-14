//! English local IR history view. No disk reads, actor commands, or device mutation.
use crate::{StudioApp, runtime::FilePurpose, theme::Palette};
use eframe::egui;
use gp200_domain::*;
use gp200_library::*;
use std::{path::PathBuf, sync::Arc};

#[derive(Debug, Default)]
pub(crate) struct IrHistoryUi {
    pub open: bool,
    page: Option<IrHistoryPage>,
    selected: Option<IrAttemptId>,
    detail: Option<Arc<IrHistoryDetail>>,
    list_pending: Option<IntentId>,
    detail_pending: Option<(IntentId, IrAttemptId)>,
    export_pending: Option<(IntentId, IrHistoryExportRequest)>,
    pub export_dialog: Option<IrHistoryExportRequest>,
    message: String,
}
impl IrHistoryUi {
    pub(crate) fn local_read_pending(&self) -> bool {
        self.list_pending.is_some() || self.detail_pending.is_some()
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(crate) fn local_work_pending(&self) -> bool {
        self.local_read_pending() || self.export_pending.is_some()
    }
    fn selected_id(&self) -> Option<&IrAttemptId> {
        self.selected.as_ref()
    }
}
impl StudioApp {
    fn submit_ir_history(&mut self, action: WorkflowAction) -> Option<IntentId> {
        let intent = self.local_intent();
        match self
            .services
            .submit_workflow(WorkflowCommand { intent, action })
        {
            Ok(()) => Some(intent),
            Err(error) => {
                self.ir_history_ui.message = error.message;
                None
            }
        }
    }
    pub(crate) fn refresh_ir_history(&mut self) {
        self.ir_history_ui.selected = None;
        self.ir_history_ui.detail = None;
        self.ir_history_ui.detail_pending = None;
        self.ir_history_ui.message.clear();
        self.ir_history_ui.list_pending = self.submit_ir_history(WorkflowAction::ListIrHistory(
            IrHistoryPageRequest::default(),
        ));
    }
    fn ir_history_page_request(&mut self, offset: usize) {
        let Some(page) = self.ir_history_ui.page.as_ref() else {
            return;
        };
        let request = IrHistoryPageRequest {
            offset,
            roster_sha256: Some(page.roster_sha256.clone()),
        };
        self.ir_history_ui.list_pending =
            self.submit_ir_history(WorkflowAction::ListIrHistory(request));
    }
    pub(crate) fn open_ir_history_attempt(&mut self, id: IrAttemptId) {
        self.ir_history_ui.open = true;
        self.asset_job_ui.open = false;
        self.selected_asset_kind = AssetKind::ImpulseResponse;
        self.ir_history_ui.selected = Some(id.clone());
        self.ir_history_ui.detail = None;
        self.ir_history_ui.message.clear();
        self.ir_history_ui.list_pending = None;
        self.ir_history_ui.detail_pending = None;
        if let Some(intent) = self.submit_ir_history(WorkflowAction::GetIrHistory(id.clone())) {
            self.ir_history_ui.detail_pending = Some((intent, id));
        }
    }
    pub(crate) fn observe_ir_history_event(&mut self, event: &WorkflowEvent) -> bool {
        if self.ir_history_ui.list_pending == Some(event.intent) {
            self.ir_history_ui.list_pending = None;
            match &event.result {
                Ok(WorkflowResult::IrHistoryPage(page)) => {
                    self.ir_history_ui.page = Some(page.clone())
                }
                Err(error) => self.ir_history_ui.message = error.message.clone(),
                _ => self.ir_history_ui.message = "Unexpected local IR history reply".into(),
            }
            self.screenshot_frames = 0;
            return true;
        }
        if self
            .ir_history_ui
            .detail_pending
            .as_ref()
            .is_some_and(|(intent, _)| *intent == event.intent)
        {
            let (_, expected) = self
                .ir_history_ui
                .detail_pending
                .take()
                .expect("matched intent");
            match &event.result {
                Ok(WorkflowResult::IrHistoryDetail(detail))
                    if detail.attempt == expected
                        && self.ir_history_ui.selected_id() == Some(&expected) =>
                {
                    self.ir_history_ui.detail = Some(detail.clone())
                }
                Err(error) => self.ir_history_ui.message = error.message.clone(),
                _ => {
                    self.ir_history_ui.message =
                        "Local IR detail reply differs from its selection".into()
                }
            }
            self.screenshot_frames = 0;
            return true;
        }
        if self
            .ir_history_ui
            .export_pending
            .as_ref()
            .is_some_and(|(intent, _)| *intent == event.intent)
        {
            let (_, expected) = self
                .ir_history_ui
                .export_pending
                .take()
                .expect("matched intent");
            self.ir_history_ui.message = match &event.result {
                Ok(WorkflowResult::IrHistoryExport(receipt))
                    if receipt.attempt == expected.attempt && receipt.kind == expected.kind
                    && receipt.destination == expected.destination
                    && receipt.source_snapshot_sha256 == expected.observed_snapshot_sha256 => format!(
                    "Saved local {} to {}. Pedal audio remains unverified.",
                    if receipt.kind == IrHistoryExportKind::CheckedEvidence {
                        "evidence"
                    } else {
                        "raw diagnostics"
                    },
                    receipt.destination.display()
                ),
                Err(error) => error.message.clone(),
                _ => "Local IR export reply differs from the requested attempt, mode, destination or snapshot; inspect the destination locally".into(),
            };
            return true;
        }
        false
    }
    pub(crate) fn export_ir_history_to(&mut self, destination: PathBuf) {
        let Some(mut request) = self.ir_history_ui.export_dialog.take() else {
            return;
        };
        request.destination = destination;
        if let Some(intent) =
            self.submit_ir_history(WorkflowAction::ExportIrHistory(request.clone()))
        {
            self.ir_history_ui.export_pending = Some((intent, request));
        }
    }
    fn start_ir_history_export_dialog(&mut self, detail: &IrHistoryDetail) {
        if self.ir_history_ui.export_dialog.is_some() || self.ir_history_ui.export_pending.is_some()
        {
            return;
        }
        if self.services.try_dialog(FilePurpose::ExportIrHistory) {
            self.ir_history_ui.export_dialog = Some(IrHistoryExportRequest {
                attempt: detail.attempt.clone(),
                observed_snapshot_sha256: detail.snapshot_sha256.clone(),
                kind: detail.export_kind,
                destination: PathBuf::new(),
            });
        }
    }
    pub(super) fn ir_history_workspace(&mut self, ui: &mut egui::Ui) {
        ui.heading("IR history");
        ui.label("Retained local observations and original source files. Pedal audio remains unverified.");
        if self.ir_history_ui.local_read_pending() {
            ui.label("Reading local history…");
        }
        if !self.ir_history_ui.message.is_empty() {
            ui.colored_label(
                Palette::for_context(ui.ctx()).warning,
                &self.ir_history_ui.message,
            );
        }
        if let Some(detail) = self.ir_history_ui.detail.clone() {
            ui.horizontal_wrapped(|ui| {
                if ui.button("Back to history").clicked() {
                    self.ir_history_ui.selected = None;
                    self.ir_history_ui.detail = None;
                    self.ir_history_ui.detail_pending = None;
                    if self.ir_history_ui.page.is_none() {
                        self.refresh_ir_history();
                    }
                }
                let label = if detail.export_kind == IrHistoryExportKind::CheckedEvidence {
                    "Export evidence…"
                } else {
                    "Export raw diagnostics…"
                };
                if ui
                    .add_enabled(
                        self.ir_history_ui.export_dialog.is_none()
                            && self.ir_history_ui.export_pending.is_none(),
                        egui::Button::new(label),
                    )
                    .clicked()
                {
                    self.start_ir_history_export_dialog(&detail);
                }
            });
            ui.add_space(8.0);
            render_detail(ui, &detail);
            return;
        }
        if self.ir_history_ui.selected.is_some()
            && self.ir_history_ui.detail_pending.is_none()
            && ui.button("Back to history").clicked()
        {
            self.ir_history_ui.selected = None;
        }
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    self.ir_history_ui.list_pending.is_none(),
                    egui::Button::new("Refresh history"),
                )
                .clicked()
            {
                self.refresh_ir_history();
            }
            if let Some(page) = self.ir_history_ui.page.clone() {
                if ui
                    .add_enabled(
                        page.offset >= IR_HISTORY_PAGE_SIZE
                            && self.ir_history_ui.list_pending.is_none(),
                        egui::Button::new("Previous page"),
                    )
                    .clicked()
                {
                    self.ir_history_page_request(page.offset - IR_HISTORY_PAGE_SIZE);
                }
                if ui
                    .add_enabled(
                        page.offset + page.rows.len() < page.total
                            && self.ir_history_ui.list_pending.is_none(),
                        egui::Button::new("Next page"),
                    )
                    .clicked()
                {
                    self.ir_history_page_request(page.offset + IR_HISTORY_PAGE_SIZE);
                }
                if page.total > 0 {
                    ui.small(format!(
                        "{}–{} of {} attempts",
                        page.offset + 1,
                        page.offset + page.rows.len(),
                        page.total
                    ));
                }
            }
        });
        let Some(page) = self.ir_history_ui.page.clone() else {
            if !self.ir_history_ui.local_read_pending() {
                ui.label("Open or refresh history to read the local library.");
            }
            return;
        };
        if page.total == 0 {
            ui.add_space(12.0);
            ui.label("No IR attempt history is retained in this library.");
            ui.label("Original source files remain available in Slots and sources.");
            return;
        }
        for row in page.rows {
            ui.add_space(7.0);
            let title = match (row.slot, row.name.as_deref()) {
                (Some(slot), Some(name)) => format!("User IR {slot}: {name}"),
                _ => format!("Unreadable attempt {}", row.attempt.as_str()),
            };
            if ui.selectable_label(false, title).clicked() {
                self.open_ir_history_attempt(row.attempt.clone());
            }
            if let Some(started) = row.started_at_ms {
                ui.small(start_label(started));
            }
            ui.label(ir_local_state_label(row.local_state));
            if let Some(error) = row.error {
                ui.colored_label(Palette::for_context(ui.ctx()).warning, error);
            }
            if let Some(recipe) = row.codec_revision {
                ui.small(format!("Local recipe: {recipe}"));
            }
            if let Some(hash) = row.source_sha256 {
                ui.small(format!("Source WAV SHA256: {hash}"));
            }
            ui.separator();
        }
    }
}

fn render_detail(ui: &mut egui::Ui, detail: &IrHistoryDetail) {
    ui.small(format!("Attempt {}", detail.attempt.as_str()));
    let Some(found) = detail.checked.as_ref() else {
        ui.label("Indeterminate");
        ui.label("This journal cannot be interpreted as a checked attempt.");
        if let Some(error) = &detail.load_error {
            ui.colored_label(Palette::for_context(ui.ctx()).warning, error);
        }
        ui.label(
            "Raw diagnostics preserve bytes and hashes only. A checked prefix is unavailable.",
        );
        render_files(ui, detail);
        return;
    };
    let binding = &found.recorded.binding;
    let end = binding.name_raw.iter().position(|b| *b == 0).unwrap_or(16);
    ui.strong(format!(
        "User IR {}: {}",
        binding.target.index() + 1,
        String::from_utf8_lossy(&binding.name_raw[..end])
    ));
    ui.small(start_label(found.recorded.started_at_ms));
    ui.colored_label(
        Palette::for_context(ui.ctx()).warning,
        ir_local_state_label(found.local_state),
    );
    ui.label("The old IR audio was not backed up. Names, checksums and status replies do not verify pedal audio.");
    ui.separator();
    ui.label(format!(
        "Recorded finish: {}",
        finish_label(found.recorded.finish)
    ));
    ui.label(format!(
        "Current interpretation: {}",
        finish_label(found.reconstructed.finish_reason())
    ));
    ui.small("The current interpretation is derived; it does not add a journal record or change the recorded finish.");
    for issue in &found.issues {
        ui.colored_label(
            Palette::for_context(ui.ctx()).warning,
            ir_journal_issue_label(issue),
        );
    }
    egui::CollapsingHeader::new("Journal and local durability")
        .default_open(true)
        .show(ui, |ui| {
            ui.label(format!("Original journal: {} bytes", detail.journal.bytes));
            ui.small(format!("SHA256: {}", detail.journal.sha256.as_str()));
            ui.label(format!(
                "Checked prefix: {} bytes, {} accepted records",
                found.valid_prefix_bytes,
                found.record_count()
            ));
            if let Some(hash) = &detail.checked_prefix_sha256 {
                ui.small(format!("Prefix SHA256: {}", hash.as_str()));
            }
            ui.small(format!(
                "Last accepted record SHA256: {}",
                found.record_sha256.as_str()
            ));
            if let Some(boundary) = &found.confirmed_local_fsync_boundary {
                ui.label(format!(
                    "Confirmed local fsync boundary: {} bytes, {} records",
                    boundary.prefix_bytes, boundary.record_count
                ));
                ui.small(format!(
                    "Boundary SHA256: {}",
                    boundary.prefix_sha256.as_str()
                ));
            } else {
                ui.label("No matching local fsync boundary was retained.");
            }
            ui.small(
                "A local fsync boundary describes filesystem publication, not pedal contents.",
            );
            ui.small(format!(
                "{} original checkpoint receipts retained",
                detail.checkpoints.len()
            ));
        });
    egui::CollapsingHeader::new("Retained source and recipe").show(ui, |ui| {
        ui.label(format!("Recipe: {}", binding.codec_revision));
        ui.label(format!(
            "Length policy: {}",
            match binding.length_policy {
                IrAttemptLengthPolicy::RejectLong => "Reject input longer than 1,024 samples",
                IrAttemptLengthPolicy::KeepFirst1024 => "Keep the first 1,024 samples",
            }
        ));
        ui.label(format!(
            "Intended payload: {} bytes; wire: {} bytes in {} packets",
            binding.payload_bytes, binding.wire_bytes, binding.packet_count
        ));
        ui.label(format!(
            "Raw additive checksum: {}",
            binding.additive_checksum
        ));
        ui.small(format!("Raw name bytes: {}", hex(&binding.name_raw)));
        for (label, hash) in [
            ("Source WAV", &binding.source_sha256),
            ("Coefficients", &binding.coefficients_sha256),
            ("Intended TLV", &binding.payload_sha256),
            ("Intended wire", &binding.wire_sha256),
            ("Baseline metadata", &binding.baseline_sha256),
            ("Identity", &binding.session.identity_sha256),
            ("Session", &binding.session.session_sha256),
            ("Original review", &binding.review_sha256),
        ] {
            ui.small(format!("{label} SHA256: {hash}"));
        }
        if !found
            .materials
            .iter()
            .any(|m| m.role == IrMaterialRole::ReviewPreimage)
        {
            ui.colored_label(
                Palette::for_context(ui.ctx()).warning,
                "Exact original review bytes were not retained",
            );
        }
        ui.small(format!(
            "Historical session epoch: {}; identity revision: {}; baseline revision: {}",
            binding.session.epoch.0,
            binding.session.identity_revision.0,
            binding.baseline_revision.0
        ));
        ui.small(format!(
            "Recorded input: {}",
            binding.session.selection.input_id
        ));
        ui.small(format!(
            "Recorded output: {}",
            binding.session.selection.output_id
        ));
    });
    egui::CollapsingHeader::new(format!(
        "Raw observations ({})",
        found.recorded.events.len()
    ))
    .show(ui, |ui| render_observations(ui, found));
    render_files(ui, detail);
}
fn render_observations(ui: &mut egui::Ui, found: &IrRetainedAttempt) {
    let mut status_index = 0;
    let mut metadata_index = 0;
    for (event_index, event) in found.recorded.events.iter().enumerate() {
        egui::CollapsingHeader::new(format!("Observation {}: {}", event_index + 1,
            match event { IrAttemptEventDto::Send { .. } => "process or transport observation",
                IrAttemptEventDto::Status { .. } => "raw status", IrAttemptEventDto::Metadata { .. } => "metadata" }))
            .id_salt(("ir-history-observation", event_index))
            .show(ui, |ui| {
            match event {
                IrAttemptEventDto::Send { context, at_ms, extent, wire_bytes, completion } => {
                    context_labels(ui, context, *at_ms);
                    ui.label(format!("Recorded extent: {extent:?}; bytes: {wire_bytes}; completion: {completion:?}"));
                    ui.small("A successful process exit does not establish pedal delivery.");
                }
                IrAttemptEventDto::Status { context, at_ms, raw } => {
                    context_labels(ui, context, *at_ms);
                    if let Some(observed) = found.reconstructed.statuses().get(status_index) {
                        if let Some(fields) = observed.fields() {
                            ui.label(format!("Raw slot: {}; flags: {}; value: {}",
                                fields.slot_raw, fields.flags_raw, fields.value_raw));
                        }
                        if let Some(issue) = observed.shape_issue() { ui.label(format!("Status shape: {issue:?}")); }
                        relation_labels(ui, observed.relation());
                        ui.label(if observed.after_accounted_send() { "Received after an accounted send observation" }
                            else { "Received before any matching accounted send observation" });
                        if let Some(previous) = observed.duplicate_of() { ui.label(format!("Duplicate of earlier status {previous}")); }
                    }
                    raw_labels(ui, raw);
                }
                IrAttemptEventDto::Metadata { context, at_ms, metadata } => {
                    context_labels(ui, context, *at_ms);
                    ui.label(format!("Raw metadata stamp: {:?}; revision: {}; source: {:?}; captured at: {} ms",
                        metadata.stamp, metadata.revision.0, metadata.source, metadata.captured_at_ms));
                    if let Some(observed) = found.reconstructed.metadata().get(metadata_index) {
                        relation_labels(ui, observed.relation());
                        let comparison = observed.comparison();
                        ui.label(format!("Fresh target metadata: {}; full name matches: {}; display name bytes match: {}; checksum matches: {}",
                            observed.fresh(), comparison.full_name_matches,
                            comparison.display_name_bytes_match, comparison.additive_checksum_matches));
                    }
                    raw_labels(ui, &metadata.raw);
                }
            }
        });
        // Count event positions even when an expanded body is closed.
        if matches!(event, IrAttemptEventDto::Status { .. }) {
            status_index += 1;
        }
        if matches!(event, IrAttemptEventDto::Metadata { .. }) {
            metadata_index += 1;
        }
    }
}
fn context_labels(ui: &mut egui::Ui, context: &IrObservationContextDto, at_ms: u64) {
    ui.label(format!(
        "Recorded time: {at_ms} ms; attempt: {}",
        context.attempt.as_str()
    ));
    ui.small(format!(
        "Session epoch: {}; identity revision: {}; session SHA256: {}",
        context.session.epoch.0,
        context.session.identity_revision.0,
        context.session.session_sha256
    ));
}
fn relation_labels(ui: &mut egui::Ui, relation: IrObservationRelation) {
    ui.label(format!(
        "Attempt matches: {}; session matches: {}; slot matches: {}; at or after start: {}",
        relation.attempt_matches,
        relation.session_matches,
        relation.slot_matches,
        relation.at_or_after_start
    ));
}
fn raw_labels(ui: &mut egui::Ui, raw: &gp200_protocol::RawTransfer) {
    ui.label(format!(
        "Raw transfer kind: {}; payload bytes: {}",
        raw.kind(),
        raw.payload().len()
    ));
    ui.small(hex(raw.payload()));
}
fn render_files(ui: &mut egui::Ui, detail: &IrHistoryDetail) {
    egui::CollapsingHeader::new(format!(
        "Exported byte descriptions ({})",
        detail.observed_files.len()
    ))
    .show(ui, |ui| {
        ui.small(
            "Original checkpoint bytes are retained observations, including invalid receipts.",
        );
        for file in &detail.observed_files {
            ui.label(format!("{}: {} bytes", file.path, file.bytes));
            ui.small(format!("SHA256: {}", file.sha256.as_str()));
        }
        ui.small(format!(
            "Displayed snapshot SHA256: {}",
            detail.snapshot_sha256.as_str()
        ));
    });
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn finish_label(reason: Option<IrAttemptFinishReason>) -> &'static str {
    match reason {
        None => "No finish was recorded",
        Some(IrAttemptFinishReason::ObservationComplete) => "Observation complete",
        Some(IrAttemptFinishReason::Timeout) => "Timed out",
        Some(IrAttemptFinishReason::Disconnected) => "Disconnected",
        Some(IrAttemptFinishReason::ProcessFailed) => "Process failed",
        Some(IrAttemptFinishReason::ObservationFailed) => "Observation failed",
        Some(IrAttemptFinishReason::InterruptedByRestart) => "Interrupted by restart",
    }
}
fn start_label(ms: u64) -> String {
    format!("Recorded start: {ms} ms; calendar date unavailable")
}

#[cfg(test)]
#[path = "ir_history_tests.rs"]
mod tests;

#[test]
#[cfg(all(test, feature = "firmware-linux-completion", target_os = "linux"))]
fn pending_ir_evidence_export_blocks_settled_recovery_host_close_until_its_matching_reply() {
    let (directory, mut app, log) = super::firmware_updater_tests::live_fixture::fixture();
    crate::firmware_update_coordinator::test_prepare_settled_recovery_host_close(
        &mut app,
        &directory.0,
    );
    // Accepted export-state/reply descriptions only. This regression observes
    // the omitted export producer; it neither starts device work nor fabricates
    // recovery ownership (the fixture uses the labeled typed-event seam above).
    let intent = IntentId(9001);
    app.ir_history_ui.export_pending = Some((
        intent,
        IrHistoryExportRequest {
            attempt: IrAttemptId::parse(&"a".repeat(32)).unwrap(),
            observed_snapshot_sha256: ContentHash::of(b"retained IR evidence export description"),
            kind: IrHistoryExportKind::RawDiagnostics,
            destination: directory.0.join("pending-original-ir-evidence.zip"),
        },
    ));
    assert!(!app.ir_history_ui.local_read_pending());
    assert!(app.ir_history_ui.local_work_pending());
    assert!(!app.can_close_settled_recovery_host());
    let ctx = egui::Context::default();
    let mut blocked = ctx.run_ui(Default::default(), |ui| {
        assert!(app.close_settled_recovery_host(ui.ctx()).is_err());
    });
    blocked.textures_delta.clear();
    assert!(blocked.viewport_output.values().any(|viewport| {
        viewport
            .commands
            .iter()
            .any(|command| matches!(command, egui::ViewportCommand::CancelClose))
    }));
    assert!(!app.observe_ir_history_event(&WorkflowEvent {
        intent: IntentId(9002),
        result: Err(ServiceError::invalid("unrelated retained reply")),
    }));
    assert!(!app.can_close_settled_recovery_host());
    assert!(app.observe_ir_history_event(&WorkflowEvent {
        intent,
        result: Err(ServiceError::invalid(
            "matching export failure; original request observed"
        )),
    }));
    assert!(!app.ir_history_ui.local_work_pending());
    assert!(app.can_close_settled_recovery_host());
    assert!(app.services.firmware_paused() && app.firmware_close_blocked());
    assert_eq!(log.lock().unwrap().offers, 0);
}
