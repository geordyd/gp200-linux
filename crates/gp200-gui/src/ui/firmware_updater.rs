//! Native updater presentation. Descriptions and preview scenes never carry authority.
use crate::{
    FirmwareCoordinatorPhase as Phase, FirmwareCoordinatorView, StudioApp, theme::Palette,
};
use eframe::egui::{self, RichText};
use gp200_domain::*;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(crate) enum FirmwareSubview {
    #[default]
    FileReview,
    Prepare,
    Update,
    History,
}
impl FirmwareSubview {
    pub(crate) const ALL: [(Self, &'static str); 4] = [
        (Self::FileReview, "File review"),
        (Self::Prepare, "Prepare update"),
        (Self::Update, "Update"),
        (Self::History, "History & recovery"),
    ];
}
#[derive(Debug, Default)]
pub(crate) struct FirmwareUpdaterUi {
    pub page: FirmwareSubview,
    pub message: String,
    pub manual_version: String,
    pub manual_date: String,
    pub manual_report_binding: Option<DeviceJobSession>,
    pub power_confirmed: bool,
    pub usb_confirmed: bool,
    pub export_sequence: u64,
    pub preparation_dialog: Option<(u64, u64, crate::LocalFirmwarePreparation)>,
    pub preparation_export: Option<FirmwarePreparationExportIntent>,
    pub failed_preparation_export: Option<FirmwarePreparationExportIntent>,
    pub attempt_dialog: Option<(
        u64,
        u64,
        FirmwareAttemptId,
        gp200_library::FirmwareHistoryPrefix,
    )>,
    pub preview: Option<FirmwareRenderPreview>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub completion_confirmation: bool,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(super) completion_phase_key: Option<CompletionConfirmationKey>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub completion_display_version: String,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub completion_version_read_now: bool,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(super) completion_cache: Option<CompletionPresentationCache>,
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(super) completion_presentation: Option<CompletionPresentationFrame>,
    #[cfg(all(test, feature = "firmware-linux-completion", target_os = "linux"))]
    pub(super) completion_presentation_now: Option<std::time::Instant>,
    #[cfg(test)]
    pub widgets: Vec<(String, egui::Rect, bool)>,
    #[cfg(test)]
    pub detail_scroll: Option<(FirmwareSubview, egui::Id, egui::Vec2, egui::Rect)>,
}
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
pub(super) const COMPLETION_PRESENTATION_TTL: std::time::Duration =
    std::time::Duration::from_secs(1);
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CompletionConfirmationKey {
    Entry {
        generation: u64,
        digest: String,
    },
    Live {
        owner: crate::firmware_update_coordinator::CompletionPresentationOwner,
        run: String,
        phase: gp200_transport::LinuxCompletionPhase,
        digest: Option<String>,
    },
}
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
#[derive(Debug)]
pub(super) struct CompletionPresentationCache {
    owner: crate::firmware_update_coordinator::CompletionPresentationOwner,
    view: gp200_transport::LinuxCompletionView,
    observed_at: std::time::Instant,
}
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
#[derive(Debug, Clone)]
pub(super) struct CompletionPresentationFrame {
    pub(super) view: gp200_transport::LinuxCompletionView,
    pub(super) fresh: bool,
}
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
impl FirmwareUpdaterUi {
    /// Keep one short-lived display projection through a failed try_lock.
    /// Confirmation identity includes the owner, run and phase, not just the
    /// digest. A cached frame is always disabled and never reaches approval.
    pub(super) fn project_completion(
        &mut self,
        owner: Option<crate::firmware_update_coordinator::CompletionPresentationOwner>,
        view: Option<gp200_transport::LinuxCompletionView>,
        entry: Option<(u64, String)>,
        now: std::time::Instant,
    ) {
        let presentation = match (owner, view) {
            (Some(owner), Some(view)) => {
                self.completion_cache = Some(CompletionPresentationCache {
                    owner,
                    view: view.clone(),
                    observed_at: now,
                });
                Some(CompletionPresentationFrame { view, fresh: true })
            }
            (Some(owner), None) => {
                if self.completion_cache.as_ref().is_some_and(|cached| {
                    cached.owner == owner
                        && now.saturating_duration_since(cached.observed_at)
                            < COMPLETION_PRESENTATION_TTL
                }) {
                    self.completion_cache
                        .as_ref()
                        .map(|cached| CompletionPresentationFrame {
                            view: cached.view.clone(),
                            fresh: false,
                        })
                } else {
                    self.completion_cache = None;
                    None
                }
            }
            (None, view) => {
                self.completion_cache = None;
                view.map(|view| CompletionPresentationFrame { view, fresh: false })
            }
        };
        let key = self
            .completion_cache
            .as_ref()
            .map(|cached| CompletionConfirmationKey::Live {
                owner: cached.owner.clone(),
                run: cached.view.run.clone(),
                phase: cached.view.phase,
                digest: cached.view.review_digest.clone(),
            })
            .or_else(|| {
                entry.map(|(generation, digest)| CompletionConfirmationKey::Entry {
                    generation,
                    digest,
                })
            });
        if self.completion_phase_key != key {
            self.completion_phase_key = key;
            self.completion_confirmation = false;
            self.completion_display_version.clear();
            self.completion_version_read_now = false;
        }
        self.completion_presentation = presentation;
    }
}
#[derive(Debug)]
pub(crate) struct FirmwarePreparationExportIntent {
    pub id: u64,
    pub generation: u64,
    pub report: crate::LocalFirmwarePreparation,
    pub destination: std::path::PathBuf,
}
/// This is deliberately only a presentation fixture. It cannot be submitted to
/// the coordinator and contains no store, ticket, receipt, lease, or permit.
#[derive(Debug, Clone)]
pub(crate) struct FirmwareRenderPreview {
    pub scene: String,
    pub page: FirmwareSubview,
    pub title: String,
    pub phase: Phase,
    pub status: String,
    pub rows: Vec<(String, String)>,
    pub diagnostics: Vec<String>,
    pub simulated_source_bytes: u64,
    pub os_offered_bytes: u64,
    pub device_verified_bytes: u64,
}
impl FirmwareRenderPreview {
    fn new(scene: &str) -> Result<Self, String> {
        let (page, title, phase, status): (_, _, _, &str) = match scene {
            "image-reviewed" => (
                FirmwareSubview::FileReview,
                "Reviewed image",
                Phase::Idle,
                "Local image inspection complete; no pedal changes",
            ),
            "source-replaced" => (
                FirmwareSubview::FileReview,
                "Image source changed",
                Phase::Idle,
                "Source replacement invalidated the local review",
            ),
            "editor-resolution" => (
                FirmwareSubview::Prepare,
                "Resolve local work",
                Phase::Preparing,
                "Keep the local draft separate from the pedal edit buffer",
            ),
            "backup-coverage" => (
                FirmwareSubview::Prepare,
                "Review captured data",
                Phase::ReadOnlyReview,
                "Stored presets, current buffer and globals captured separately",
            ),
            "partial-acquisition" => (
                FirmwareSubview::Prepare,
                "Acquisition incomplete",
                Phase::RecoveryRequired,
                "Capture cancelled; the changed archive cannot be accepted",
            ),
            "proposed-loss" => (
                FirmwareSubview::Prepare,
                "Review proposed loss",
                Phase::ReadOnlyReview,
                "Physical update and Linux recovery profiles are unavailable",
            ),
            "waiting-for-store" => (
                FirmwareSubview::Update,
                "Waiting for the library",
                Phase::WaitingForStore,
                "Original local library worker is retiring; its store has not returned",
            ),
            "ownership-rejected" => (
                FirmwareSubview::Update,
                "Ownership request rejected",
                Phase::RestoringLibrary,
                "The genuine returned local store is being restored off the UI thread",
            ),
            "simulated-transfer" => (
                FirmwareSubview::Update,
                "Simulated owner transfer",
                Phase::Owned,
                "Simulated observations; zero device-verified bytes",
            ),
            "host-stop" => (
                FirmwareSubview::Update,
                "Host stop requested",
                Phase::StopRequested,
                "Stopping host observation does not cancel or roll back the pedal",
            ),
            "owner-retiring" => (
                FirmwareSubview::Update,
                "Owner retirement pending",
                Phase::ReturningNormal,
                "Reader and ledger joins are pending; the store is not yet returned",
            ),
            "startup-no-store" => (
                FirmwareSubview::Update,
                "Startup failed without a store",
                Phase::RecoveryRequired,
                "Actual store ownership is unresolved; reopening a replacement is unavailable",
            ),
            "restart-quarantine" => (
                FirmwareSubview::History,
                "Interrupted attempt",
                Phase::Idle,
                "Device mode unknown; no resume or retry is available",
            ),
            "finished-corrupt-tail" => (
                FirmwareSubview::History,
                "Recorded finish with retained tail",
                Phase::Idle,
                "Recorded host finish and derived interrupted state remain separate",
            ),
            _ => return Err(format!("Unknown native updater scene: {scene}")),
        };
        let mut rows = vec![
            (
                "Target image".into(),
                "GP-200 • 1.8.2 • 6,452,728 bytes (descriptive fixture)".into(),
            ),
            (
                "Reported by you".into(),
                "1.6.3 (fixture report; never an automatic read)".into(),
            ),
            ("Automatic application version".into(), "Unavailable".into()),
            ("Bootloader version".into(), "Not observed".into()),
        ];
        match page {
            FirmwareSubview::FileReview => {
                rows.push((
                    "Retained source SHA-256".into(),
                    "c368618773097b07c556c951eb10792f9d57be929323366648eb6e083d0d23af".into(),
                ));
                rows.push((
                    "Integrity".into(),
                    "Additive checksum passed; signature/authenticity unavailable".into(),
                ));
                rows.push(("Count comparison".into(), if scene == "source-replaced" { "Selected source changed; no retained sections/counts accepted" } else { "Four image sections; native and integer packet counts shown only after inspection" }.into()));
            }
            FirmwareSubview::Prepare => {
                rows.push((
                    "Stored presets".into(),
                    if scene == "partial-acquisition" {
                        "128 / 256 distinct slots observed; comparison incomplete"
                    } else {
                        "256 / 256 distinct slots; destination restoration acceptance missing"
                    }
                    .into(),
                ));
                rows.push((
                    "Current edit buffer".into(),
                    "Fart Voice (Wah), captured separately; not a saved-slot claim".into(),
                ));
                rows.push(("Local draft".into(), "Retained local work and Undo state; excluded only after a genuine Saved acknowledgement".into()));
                rows.push((
                    "Globals and mappings".into(),
                    "Raw globals plus three footswitch tables; persistence after update unverified"
                        .into(),
                ));
                rows.push((
                    "User IR".into(),
                    "20 metadata slots; installed audio unavailable in every slot, including IR20"
                        .into(),
                ));
                rows.push(("IR20 local source".into(), "Linux IR test source retained; equality to installed audio and reconstruction unverified".into()));
                rows.push(("SnapTone".into(), "Conditionally not applicable if the manual 1.6.3 report is correct; automatic version unavailable".into()));
                rows.push(("Preset references".into(), "Fixture: factory cabinet references; unavailable unreferenced user content remains possible loss".into()));
                rows.push(("Restoration and migration".into(), "No accepted 1.8.2 restore/profile; unknown-byte preservation remains unverified".into()));
                rows.push(("Backup package".into(), if scene == "partial-acquisition" { "Partial/cancelled acquisition; archive or sidecar changed; revalidation rejected" } else { "Independently checked local package; this is not a complete restorable device backup" }.into()));
            }
            FirmwareSubview::Update => {
                rows.push((
                    "Normal editor".into(),
                    "Device changes paused until actual retirement and fresh normal reads".into(),
                ));
                rows.push((
                    "Completion".into(),
                    "Device completion, reboot and fresh normal verification remain unverified"
                        .into(),
                ));
            }
            FirmwareSubview::History => {
                rows.push((
                    "Attempt".into(),
                    "native-preview-0001 (render fixture)".into(),
                ));
                rows.push(("Origin".into(), "Simulator / descriptive preview".into()));
                rows.push((
                    "Recorded status".into(),
                    if scene == "finished-corrupt-tail" {
                        "Host finish recorded; device completion unverified"
                    } else {
                        "Host process interrupted"
                    }
                    .into(),
                ));
                rows.push((
                    "Derived reopen state".into(),
                    "Interrupted / retained corrupt tail; device mode unknown".into(),
                ));
                rows.push((
                    "Accepted prefix".into(),
                    "24 records / 16,384 bytes; tail retained separately".into(),
                ));
                rows.push((
                    "Recent records".into(),
                    "Window of 8 / 24 records; 16 older records remain in the full report".into(),
                ));
                rows.push(("Checked export".into(), "Fixture export result: retained original journal/material bundle; no resume authority".into()));
            }
        }
        // Put the decision facts ahead of version origins in the bounded viewport.
        if page == FirmwareSubview::Prepare {
            let priority = [
                "Backup package",
                "Stored presets",
                "User IR",
                "SnapTone",
                "Restoration and migration",
                "Globals and mappings",
                "Current edit buffer",
                "Local draft",
                "IR20 local source",
                "Preset references",
            ];
            rows.sort_by_key(|(name, _)| {
                priority
                    .iter()
                    .position(|label| name == label)
                    .unwrap_or(priority.len())
            });
        } else if page != FirmwareSubview::FileReview {
            rows.rotate_left(4);
        }
        Ok(Self {
            scene: scene.into(), page, title: title.into(), phase, status: status.into(), rows,
            diagnostics: vec!["Native render fixture only. No hardware backend or firmware request is constructed.".into(), "Recovery requires contingent access to the vendor Windows updater; that recovery has not been exercised.".into()],
            simulated_source_bytes: if scene == "simulated-transfer" { 32_768 } else { 0 },
            os_offered_bytes: if scene == "simulated-transfer" || scene == "host-stop" { 65_840 } else { 0 },
            device_verified_bytes: 0,
        })
    }
}
impl StudioApp {
    pub(crate) fn invalidate_changed_firmware_report_session(&mut self) {
        if self
            .firmware_ui
            .updater
            .manual_report_binding
            .as_ref()
            .is_some_and(|bound| Some(bound) != self.controller.state().device_job_session.as_ref())
        {
            self.firmware_ui.updater.manual_report_binding = None;
            self.firmware_ui.updater.message = "The normal device session changed. Read the pedal display again and reconfirm the manual version/date for this GP-200 before capture.".into();
        }
    }
    pub(crate) fn associate_firmware_manual_report(&mut self) -> Result<(), ServiceError> {
        self.firmware_ui.updater.manual_report_binding = None;
        let session = self
            .controller
            .state()
            .device_job_session
            .clone()
            .ok_or_else(|| {
                ServiceError::invalid(
                    "Connect and identify this GP-200 before associating a manual report",
                )
            })?;
        if session.identity().mode != BackendMode::Hardware
            || session.identity().model != DeviceModel::Gp200
        {
            return Err(ServiceError::invalid(
                "Manual pedal reports require an observed GP-200 hardware session",
            ));
        }
        let association = firmware_report_association(&session)?;
        firmware_manual_report(
            &self.firmware_ui.updater.manual_version,
            &self.firmware_ui.updater.manual_date,
            association,
        )?;
        self.firmware_ui.updater.manual_report_binding = Some(session);
        Ok(())
    }
    pub(crate) fn firmware_escape(&mut self) {
        match self.firmware_coordinator_view().phase {
            Phase::Preparing => {
                let result = self.return_from_firmware_preparation();
                self.firmware_action_result(result, "Local cancellation requested; outstanding original work must settle.");
            }
            Phase::Idle => {}
            _ => self.firmware_ui.updater.message = "Escape does not stop the pedal. Use the updater's explicit Return to editing or Stop host observation action and wait for original retirement observations.".into(),
        }
    }
    pub(crate) fn firmware_ownership_strip(&mut self, ui: &mut egui::Ui) {
        let view = self.firmware_coordinator_view();
        if view.phase == Phase::Idle {
            return;
        }
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(
                Palette::for_context(ui.ctx()).warning,
                format!("Updater: {}", phase_label(view.phase)),
            );
            ui.label("Normal controls paused");
            if ui.button("Open updater").clicked() {
                self.send(UiIntent::Navigate(ViewId::Assets));
                self.selected_asset_kind = AssetKind::Firmware;
                self.firmware_ui.updater.page = if matches!(
                    view.phase,
                    Phase::Preparing | Phase::ReadOnlyReview | Phase::PreparingLocalReport
                ) {
                    FirmwareSubview::Prepare
                } else {
                    FirmwareSubview::Update
                };
            }
        });
        self.firmware_boundary_actions(ui, &view);
    }
    /// Programmatic QA close uses the same production close policy.
    pub fn request_firmware_preview_close(&mut self, ctx: &egui::Context) {
        self.request_application_close(ctx);
    }
    /// Native renderer QA only. This accepts a scene name, not owner material.
    /// All update/capture/history mutations are disabled in a preview scene.
    pub fn open_firmware_updater_preview(&mut self, scene: &str) -> Result<(), String> {
        let preview = FirmwareRenderPreview::new(scene)?;
        self.send(UiIntent::Navigate(ViewId::Assets));
        self.selected_asset_kind = AssetKind::Firmware;
        self.firmware_ui.updater.page = preview.page;
        self.firmware_ui.updater.preview = Some(preview);
        Ok(())
    }
    /// Descriptive report for native QA; it cannot be used as update material.
    pub fn firmware_updater_preview_report(&self) -> Option<serde_json::Value> {
        self.firmware_ui.updater.preview.as_ref().map(|preview| serde_json::json!({
            "scene": preview.scene, "injected_render_description": true,
            "phase_label": phase_label(preview.phase), "simulated_source_bytes": preview.simulated_source_bytes,
            "os_offered_bytes": preview.os_offered_bytes, "device_verified_bytes": preview.device_verified_bytes,
            "hardware_backend_constructed": false, "permits_flash": false, "completion_verified": false,
        }))
    }
    pub(crate) fn firmware_navigation(&mut self, ui: &mut egui::Ui) {
        #[cfg(test)]
        self.firmware_ui.updater.widgets.clear();
        ui.horizontal_wrapped(|ui| {
            for (page, label) in FirmwareSubview::ALL {
                let response = ui.selectable_label(self.firmware_ui.updater.page == page, label);
                self.firmware_record_widget(label, &response);
                if response.clicked() {
                    self.firmware_ui.updater.page = page;
                }
            }
        });
        let preview = self.firmware_ui.updater.preview.clone();
        if let Some(preview) = preview {
            ui.colored_label(
                Palette::for_context(ui.ctx()).warning,
                "Native preview • injected description • all device actions disabled",
            );
            ui.label(RichText::new(&preview.status).strong());
            ui.horizontal_wrapped(|ui| {
                let labels: &[&str] = match preview.page {
                    FirmwareSubview::FileReview => {
                        &["Select firmware…", "Inspect image", "Export review report…"]
                    }
                    FirmwareSubview::Prepare => {
                        if preview.phase == Phase::Preparing {
                            &["Cancel preparation", "Start update"]
                        } else {
                            &["Return to editing", "Start update"]
                        }
                    }
                    FirmwareSubview::Update => {
                        if preview.phase == Phase::StopRequested {
                            &["Stop requested"]
                        } else {
                            &["Stop host observation"]
                        }
                    }
                    FirmwareSubview::History => &["Export checked attempt…"],
                };
                for label in labels {
                    let response = ui.add_enabled(false, egui::Button::new(*label));
                    self.firmware_record_widget(label, &response);
                }
            });
        } else {
            let view = self.firmware_coordinator_view();
            ui.label(RichText::new(format!("Updater: {}", phase_label(view.phase))).strong());
            if view.phase != Phase::Idle {
                ui.small("Normal pedal and library changes are paused. Read-only navigation, local review, and appearance settings remain available.");
            }
            self.firmware_boundary_actions(ui, &view);
            if self.firmware_ui.updater.page == FirmwareSubview::Prepare {
                ui.horizontal_wrapped(|ui| {
                    blocked_update_button(ui, "Physical update unavailable.")
                });
            }
        }
        if !self.firmware_ui.updater.message.is_empty() {
            ui.colored_label(
                Palette::for_context(ui.ctx()).warning,
                &self.firmware_ui.updater.message,
            );
        }
        self.firmware_held_file_choices(ui);
        ui.separator();
    }
    pub(crate) fn firmware_record_widget(&mut self, label: &str, response: &egui::Response) {
        #[cfg(test)]
        self.firmware_ui
            .updater
            .widgets
            .push((label.into(), response.rect, response.enabled()));
        #[cfg(not(test))]
        let _ = (label, response);
    }
    fn firmware_action_result(&mut self, result: Result<(), ServiceError>, success: &str) {
        self.firmware_ui.updater.message = match result {
            Ok(()) => success.into(),
            Err(error) => error.message,
        };
    }
    pub(crate) fn begin_firmware_preparation_export(&mut self) {
        if self.firmware_ui.updater.preparation_dialog.is_some()
            || self.firmware_ui.updater.preparation_export.is_some()
        {
            return;
        }
        let view = self.firmware_coordinator_view();
        let Some(preparation) = view.preparation else {
            return;
        };
        self.firmware_ui.updater.export_sequence += 1;
        let id = self.firmware_ui.updater.export_sequence;
        if self
            .services
            .try_dialog(crate::runtime::FilePurpose::ExportFirmwarePreparation(id))
        {
            self.firmware_ui.updater.preparation_dialog = Some((id, view.generation, preparation));
        }
    }
    pub(crate) fn finish_firmware_preparation_dialog(
        &mut self,
        id: u64,
        destination: Option<std::path::PathBuf>,
    ) {
        if self
            .firmware_ui
            .updater
            .preparation_dialog
            .as_ref()
            .map(|d| d.0)
            != Some(id)
        {
            self.firmware_ui.updater.message = "An old preparation report dialog reply was ignored. Select a destination for the current review.".into();
            return;
        }
        let (_, generation, report) = self.firmware_ui.updater.preparation_dialog.take().unwrap();
        let Some(destination) = destination else {
            return;
        };
        let view = self.firmware_coordinator_view();
        let current = view.preparation.as_ref();
        if view.generation != generation
            || current.is_none_or(|p| {
                p.digest != report.digest
                    || p.image_sha256 != report.image_sha256
                    || p.archive_sha256 != report.archive_sha256
                    || p.sidecar_sha256 != report.sidecar_sha256
                    || p.evidence_index_sha256 != report.evidence_index_sha256
            })
        {
            self.firmware_ui.updater.message = "The preparation review changed while its destination dialog was open. No report was written; review the current material and export again.".into();
            return;
        }
        let intent = FirmwarePreparationExportIntent {
            id,
            generation,
            report,
            destination,
        };
        let result = self.services.export_firmware_preparation(
            id,
            intent.report.clone(),
            intent.destination.clone(),
        );
        if result.is_ok() {
            self.firmware_ui.updater.preparation_export = Some(intent);
        } else {
            self.firmware_ui.updater.failed_preparation_export = Some(intent);
        }
        self.firmware_action_result(
            result,
            "Saving the exact preparation and proposed loss review off the UI thread…",
        );
    }
    pub(crate) fn poll_firmware_preparation_export(&mut self) {
        if let Some((id, result)) = self.services.firmware_preparation_export.take()
            && self
                .firmware_ui
                .updater
                .preparation_export
                .as_ref()
                .is_some_and(|p| p.id == id)
        {
            let intent = self.firmware_ui.updater.preparation_export.take().unwrap();
            self.firmware_ui.updater.message = match result {
                Ok((path, hash)) => {
                    self.firmware_ui.updater.failed_preparation_export = None;
                    format!(
                        "Preparation report saved: {}. Review digest {}; report SHA-256 {hash}. Local evidence only; device completion remains unverified.",
                        path.display(),
                        intent.report.digest
                    )
                }
                Err(error) => {
                    self.firmware_ui.updater.failed_preparation_export = Some(intent);
                    format!(
                        "Preparation export did not complete: {error}. The original review and destination are retained below."
                    )
                }
            };
        }
    }
    pub(crate) fn firmware_close_notice(&mut self, ctx: &egui::Context) {
        if !self.firmware_close_request_pending() {
            return;
        }
        let view = self.firmware_coordinator_view();
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        let title = if view.can_close_settled_recovery_host {
            "Recovery is still required"
        } else {
            "Updater work is still active"
        };
        #[cfg(not(all(feature = "firmware-linux-completion", target_os = "linux")))]
        let title = "Updater work is still active";
        egui::Window::new(title)
            .id(egui::Id::new("firmware-close-notice"))
            .collapsible(false).resizable(false).default_width(420.0)
            .show(ctx, |ui| {
                #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
                if view.can_close_settled_recovery_host {
                    ui.label("The original firmware owner returned and the local library and saved documents are settled. You can close the app. The pedal still requires recovery; keep its power and USB connected for a new review.");
                    let response = ui.button("Close app and keep recovery required");
                    self.firmware_record_widget("Close app and keep recovery required", &response);
                    if response.clicked() {
                        let result = self.close_settled_recovery_host(ui.ctx());
                        self.firmware_action_result(result, "Closing the settled host. The original recovery history and pedal recovery hold are retained.");
                    }
                } else {
                    ui.label("Keep the app open while the original work and retirement observations settle. A forced process or OS shutdown can leave the pedal state unknown.");
                }
                #[cfg(not(all(feature = "firmware-linux-completion", target_os = "linux")))]
                ui.label("Keep the app open while the original work and retirement observations settle. A forced process or OS shutdown can leave the pedal state unknown.");
                ui.label(phase_label(view.phase));
                let response = ui.button("Continue waiting");
                self.firmware_record_widget("Continue waiting", &response);
                if response.clicked() { self.dismiss_firmware_close_request(); }
                self.firmware_boundary_actions(ui, &view);
            });
    }
    pub(crate) fn firmware_held_file_choices(&mut self, ui: &mut egui::Ui) {
        let count = self.pending_firmware_file_choices();
        if count == 0 && self.omitted_file_selections == 0 {
            return;
        }
        ui.label(format!(
            "{count} original file choices held locally. They are never applied automatically."
        ));
        if self.omitted_file_selections != 0 {
            ui.colored_label(
                Palette::for_context(ui.ctx()).warning,
                format!(
                    "{} choices exceeded the 32-choice limit. Dismiss them and select files again.",
                    self.omitted_file_selections
                ),
            );
        }
        ui.horizontal_wrapped(|ui| {
            let response = ui.add_enabled(
                !self.services.firmware_paused() && self.omitted_file_selections == 0,
                egui::Button::new("Apply held file choices"),
            );
            self.firmware_record_widget("Apply held file choices", &response);
            if response.clicked() {
                let result = self.apply_pending_firmware_file_choices();
                self.firmware_action_result(
                    result,
                    "The original held file choices were applied deliberately.",
                );
            }
            let response = ui.button("Dismiss held file choices");
            self.firmware_record_widget("Dismiss held file choices", &response);
            if response.clicked() {
                self.dismiss_pending_firmware_file_choices();
            }
        });
    }
    pub(crate) fn firmware_boundary_actions(
        &mut self,
        ui: &mut egui::Ui,
        view: &FirmwareCoordinatorView,
    ) {
        if view.can_retry_library_restore {
            let response = ui.button("Retry local library restoration");
            self.firmware_record_widget("Retry local library restoration", &response);
            if response.clicked() {
                let result = self.retry_firmware_library_restore();
                self.firmware_action_result(result, "Restoring the same returned library store. Normal editing waits for its actual return and fresh normal reads.");
            }
            ui.small("This retries local library restoration only. It never retries firmware or releases the updater owner.");
        }
        if view.phase == Phase::Idle {
            return;
        }
        if view.can_return_to_editing {
            let label = if view.phase == Phase::Preparing {
                "Cancel preparation"
            } else {
                "Return to editing"
            };
            let response = ui.button(label);
            self.firmware_record_widget(label, &response);
            if response.clicked() {
                let result = self.return_from_firmware_preparation();
                self.firmware_action_result(result, "Return requested; waiting for the original owner and fresh normal observations.");
            }
            ui.small(if view.phase == Phase::Preparing { "Cancellation waits for outstanding local work; no firmware was offered." } else { "Editing resumes only after the actual library return and fresh identity, current-buffer, and globals reads." });
        } else if view.phase != Phase::StopRequested {
            let response = ui.button("Stop host observation");
            self.firmware_record_widget("Stop host observation", &response);
            if response.clicked() {
                self.stop_firmware_preparation();
            }
            ui.small("This stops the host observer. It does not cancel or roll back the pedal, prove a safe power-off, or release the owner immediately.");
        } else {
            ui.label(
                "Stop requested. Keep the app open while original retirement observations settle.",
            );
        }
    }
    pub(crate) fn firmware_preview_file_review(
        &mut self,
        ui: &mut egui::Ui,
        preview: &FirmwareRenderPreview,
    ) {
        ui.heading(&preview.title);
        render_rows(ui, &preview.rows);
        section(ui, "Offline packet rehearsal");
        ui.label("Synthetic packet generation is separate from device update. No bootloader identity, finalization, or recovery is accepted by this scene.");
        ui.add_enabled(false, egui::Button::new("Run offline rehearsal"));
        diagnostics(ui, "preview-diagnostics", &preview.diagnostics, 0);
    }
    pub(crate) fn firmware_prepare_screen(&mut self, ui: &mut egui::Ui) {
        ui.heading("Prepare a GP-200 update");
        if let Some(preview) = self.firmware_ui.updater.preview.clone() {
            ui.label(RichText::new(&preview.title).strong());
            render_rows(ui, &preview.rows);
            self.firmware_preparation_checklist(ui, false);
            diagnostics(ui, "preview-diagnostics", &preview.diagnostics, 0);
            return;
        }
        let view = self.firmware_coordinator_view();
        ui.label("Review the actual pedal data, your retained local draft, and unavailable content before considering firmware entry.");
        if ui.button("Open local backup reviews").clicked() {
            self.send(UiIntent::Navigate(ViewId::Library));
            self.library_workflow.tab = super::library_workflows::LibraryTab::Backups;
            self.refresh_library_workflows();
        }
        ui.small("Imported and reopened archives remain descriptive local reviews. They cannot replace the genuine current-device capture required here.");
        if ui.available_width() >= 800.0 {
            ui.columns(2, |columns| {
                render_coverage(&mut columns[0], &view);
                render_material(&mut columns[1], &view);
            });
        } else {
            render_coverage(ui, &view);
            render_material(ui, &view);
        }
        let globals_current = crate::firmware_update_coordinator::normal_firmware_globals_current(
            self.controller.state(),
        );
        let global_read_reason = super::globals::read_blocked_reason(
            self.controller.state(),
            self.controller.device_operation_pending(),
        );
        ui.horizontal_wrapped(|ui| {
            let read_available = view.phase == Phase::Idle
                && !self.firmware_ui.busy()
                && global_read_reason.is_none();
            let read = ui.add_enabled(read_available, egui::Button::new("Read global settings"));
            self.firmware_record_widget("Read global settings", &read);
            if read.clicked() {
                self.send(UiIntent::RefreshDeviceGlobals);
            }
            if let Some(reason) = &global_read_reason {
                read.on_hover_text(reason);
            }
            let begin_available = view.phase == Phase::Idle
                && globals_current
                && !self.firmware_ui.busy()
                && self.controller.state().connection == ConnectionState::Ready
                && self
                    .controller
                    .state()
                    .identity
                    .as_ref()
                    .is_some_and(|i| i.mode == BackendMode::Hardware)
                && !view.blockers.iter().any(|s| {
                    s.contains("concrete MIDI") || s.contains("original local library owner")
                });
            let begin = ui.add_enabled(begin_available, egui::Button::new("Begin preparation"));
            self.firmware_record_widget("Begin preparation", &begin);
            if begin.clicked() {
                let result = self.begin_firmware_preparation();
                self.firmware_action_result(
                    result,
                    "Preparation requested; normal work is settling.",
                );
            }
            let retain = ui.add_enabled(
                view.phase == Phase::Preparing,
                egui::Button::new("Keep local draft for read-only capture"),
            );
            self.firmware_record_widget("Keep local draft for read-only capture", &retain);
            if retain.clicked() {
                let result = self.retain_local_draft_for_firmware_capture();
                self.firmware_action_result(
                    result,
                    "The exact saved local draft was retained separately from the pedal buffer.",
                );
            }
            let refresh = ui.add_enabled(
                view.phase == Phase::Preparing,
                egui::Button::new("Refresh preparation source"),
            );
            self.firmware_record_widget("Refresh preparation source", &refresh);
            if refresh.clicked() {
                let result = self.refresh_firmware_preparation();
                self.firmware_action_result(
                    result,
                    "Source refreshed; previous local resolution choices must be checked again.",
                );
            }
        });
        if view.phase == Phase::Idle {
            ui.small("Connect the normal-mode GP-200 using the concrete MIDI service to begin a read-only capture. Local file inspection remains available.");
            if !globals_current {
                ui.small("Read global settings in this connection before beginning preparation. A previous connection's baseline cannot be used.");
            }
        }
        self.firmware_source_facts(ui);
        section(ui, "Current buffer, saved presets and local draft");
        ui.label("The current pedal edit buffer is a separate object. A local document, its Undo history, and a saved preset slot can contain different work.");
        if let Some(document) = self.controller.active_document() {
            ui.label(format!(
                "Local document: {} • {}",
                document.draft.view.display_name(),
                if document.dirty() {
                    "unsaved local changes"
                } else {
                    "unchanged from its local baseline"
                }
            ));
        }
        ui.small("Keep local draft succeeds only after the genuine draft-recovery Saved acknowledgement. It does not write a preset or replace the pedal current buffer.");
        let context = self.firmware_capture_context();
        let capture_available = view.phase == Phase::Preparing && context.is_ok();
        let capture = ui.add_enabled(
            capture_available,
            egui::Button::new("Capture read-only backup"),
        );
        self.firmware_record_widget("Capture read-only backup", &capture);
        if capture.clicked() {
            let result = context.and_then(|context| self.capture_firmware_backup(context));
            self.firmware_action_result(
                result,
                "Read-only capture requested; both original observation passes must settle.",
            );
        } else if let Err(error) = context {
            ui.small(error.message);
        }
        let prepare = ui.add_enabled(
            view.phase == Phase::ReadOnlyReview && self.firmware_ui.image.is_some(),
            egui::Button::new("Check local review material"),
        );
        self.firmware_record_widget("Check local review material", &prepare);
        if prepare.clicked() {
            let result = self.prepare_local_firmware_review();
            self.firmware_action_result(
                result,
                "Checking exact retained image, archive, sidecar and evidence off the UI thread.",
            );
        }
        self.firmware_preparation_checklist(ui, true);
        let export = ui.add_enabled(
            view.preparation.is_some()
                && self.firmware_ui.updater.preparation_dialog.is_none()
                && self.firmware_ui.updater.preparation_export.is_none(),
            egui::Button::new("Export preparation and loss review…"),
        );
        self.firmware_record_widget("Export preparation and loss review…", &export);
        if export.clicked() {
            self.begin_firmware_preparation_export();
        }
        if let Some(intent) = &self.firmware_ui.updater.failed_preparation_export {
            section(ui, "Retained failed export");
            row(ui, "Original generation", &intent.generation.to_string());
            copy_value(ui, "Original preparation digest", &intent.report.digest);
            row(
                ui,
                "Original destination",
                &intent.destination.display().to_string(),
            );
            ui.small("The last export did not complete. Review the current material and choose a new report filename to export deliberately.");
        }
        ui.colored_label(Palette::for_context(ui.ctx()).warning, "Physical profile, finalization, Linux recovery, and destination restoration acceptance are unavailable.");
        diagnostics(ui, "coordinator-blockers", &view.blockers, 0);
        diagnostics(
            ui,
            "coordinator-diagnostics",
            &view.diagnostics,
            view.omitted_diagnostics,
        );
    }
    fn firmware_source_facts(&mut self, ui: &mut egui::Ui) {
        section(ui, "Device and version origins");
        let state = self.controller.state();
        ui.label(format!("Normal observation epoch: {}", state.epoch.0));
        if let Some(session) = &state.device_job_session {
            row(
                ui,
                "Identity source",
                if session.identity().mode == BackendMode::Hardware {
                    "Observed normal MIDI identity; application version not established"
                } else {
                    "Simulator identity; not an installed pedal version"
                },
            );
            copy_value(ui, "Input endpoint", &session.selection().input_id);
            copy_value(ui, "Output endpoint", &session.selection().output_id);
            copy_value(
                ui,
                "Observed identity SHA-256",
                &session.identity_sha256().hex(),
            );
        } else {
            row(
                ui,
                "Identity source",
                "No current associated normal session",
            );
        }
        row(
            ui,
            "Automatic application version",
            "Unavailable; normal identity does not establish an application version",
        );
        row(ui, "Bootloader observation", "Not observed");
        row(
            ui,
            "Physical USB binding",
            "No accepted bootloader binding is available. Normal endpoint names alone do not establish an update-mode binding.",
        );
        if let Some(image) = &self.firmware_ui.image {
            row(
                ui,
                "Target image version",
                image
                    .review
                    .summary()
                    .image_version
                    .as_deref()
                    .unwrap_or("Unrecognized"),
            );
        }
        ui.label("Optional version read from the pedal display:");
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            changed |= ui
                .add_enabled(
                    self.firmware_coordinator_view().phase == Phase::Idle,
                    egui::TextEdit::singleline(&mut self.firmware_ui.updater.manual_version)
                        .hint_text("Reported version, e.g. 1.6.3")
                        .desired_width(180.0),
                )
                .changed();
            changed |= ui
                .add_enabled(
                    self.firmware_coordinator_view().phase == Phase::Idle,
                    egui::TextEdit::singleline(&mut self.firmware_ui.updater.manual_date)
                        .hint_text("Report date YYYY-MM-DD")
                        .desired_width(180.0),
                )
                .changed();
        });
        if changed {
            self.firmware_ui.updater.manual_report_binding = None;
            if !self.firmware_ui.updater.manual_version.trim().is_empty()
                && !self.firmware_ui.updater.manual_date.trim().is_empty()
            {
                let result = self.associate_firmware_manual_report();
                self.firmware_action_result(result, "Manual report associated with the current normal GP-200 session; automatic application version remains unavailable.");
            }
        }
        if self.firmware_ui.updater.manual_version.trim().is_empty() {
            ui.small("No manual report entered. SnapTone applicability remains unknown.");
        } else if self.firmware_ui.updater.manual_report_binding.as_ref()
            == self.controller.state().device_job_session.as_ref()
            && self.firmware_ui.updater.manual_report_binding.is_some()
        {
            ui.label(format!(
                "Reported by you: {} • associated with this normal session",
                self.firmware_ui.updater.manual_version.trim()
            ));
        } else {
            ui.colored_label(Palette::for_context(ui.ctx()).warning, "This report is unassociated or its device session changed. It will not be used for capture applicability.");
            if ui
                .add_enabled(
                    self.firmware_coordinator_view().phase == Phase::Idle,
                    egui::Button::new("Reconfirm report for this GP-200"),
                )
                .clicked()
            {
                let result = self.associate_firmware_manual_report();
                self.firmware_action_result(
                    result,
                    "Manual display report reconfirmed for this normal GP-200 session.",
                );
            }
        }
        ui.small("A manual report is conditional descriptive evidence. Leave it empty when unknown; it cannot establish update compatibility.");
    }
    pub(super) fn firmware_capture_context(
        &self,
    ) -> Result<FirmwareBackupReviewContext, ServiceError> {
        let session = self
            .controller
            .state()
            .device_job_session
            .as_ref()
            .ok_or_else(|| ServiceError::invalid("A current normal device session is required"))?;
        if session.identity().mode != BackendMode::Hardware
            || session.identity().model != DeviceModel::Gp200
        {
            return Err(ServiceError::unsupported(
                "Read-only firmware acquisition requires the GP-200 hardware session",
            ));
        }
        let image = self.firmware_ui.image.as_ref().ok_or_else(|| {
            ServiceError::invalid("Inspect the selected GP-200 firmware image first")
        })?;
        let summary = image.review.summary();
        let version: FirmwareVersion = summary
            .image_version
            .as_deref()
            .ok_or_else(|| ServiceError::invalid("Image target version is unknown"))?
            .parse()
            .map_err(|e: FirmwareDescriptionError| ServiceError::invalid(e.to_string()))?;
        let device = firmware_report_association(session)?;
        let manual_version = if self.firmware_ui.updater.manual_version.trim().is_empty() {
            None
        } else {
            if self.firmware_ui.updater.manual_report_binding.as_ref() != Some(session) {
                return Err(ServiceError::invalid(
                    "The manual report belongs to a different or unconfirmed device session. Reconfirm it for this GP-200, or clear the optional report.",
                ));
            }
            Some(firmware_manual_report(
                &self.firmware_ui.updater.manual_version,
                &self.firmware_ui.updater.manual_date,
                device.clone(),
            )?)
        };
        Ok(FirmwareBackupReviewContext { device, manual_version, destination: FirmwareBackupDestination { model: summary.model.name(), version, image_sha256: summary.source_sha256.hex(), restoration_profile: None }, local_material: vec![], preservation_uncertainty: "Firmware preservation of preset/global/user-content data and unknown bytes remains unverified; destination migration and restoration acceptance are missing".into(), recovery_limitations: "No accepted Linux update completion or recovery profile. Vendor Windows recovery requires contingent machine access and has not been exercised".into() })
    }
    fn firmware_preparation_checklist(&mut self, ui: &mut egui::Ui, enabled: bool) {
        section(ui, "Before any physical update");
        ui.add_enabled_ui(enabled, |ui| {
            ui.checkbox(
                &mut self.firmware_ui.updater.power_confirmed,
                "I have stable pedal power",
            );
            ui.checkbox(
                &mut self.firmware_ui.updater.usb_confirmed,
                "I have a direct, stable USB connection",
            );
        });
        ui.small("These are your statements. They do not approve a flash, validate the physical profile, or make unavailable content recoverable.");
    }
    pub(crate) fn firmware_update_screen(&mut self, ui: &mut egui::Ui) {
        #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
        if self.firmware_ui.updater.preview.is_none() {
            self.firmware_linux_update_screen(ui);
            return;
        }
        ui.heading("Update observation");
        if let Some(preview) = self.firmware_ui.updater.preview.clone() {
            ui.label(RichText::new(&preview.title).strong());
            ui.label(&preview.status);
            render_counters(
                ui,
                true,
                preview.simulated_source_bytes,
                preview.os_offered_bytes,
                preview.device_verified_bytes,
            );
            render_rows(ui, &preview.rows);
            diagnostics(ui, "preview-diagnostics", &preview.diagnostics, 0);
        } else {
            let view = self.firmware_coordinator_view();
            ui.label(RichText::new(phase_label(view.phase)).strong());
            if let Some(owner) = &view.owner {
                ui.label(format!("Owner observation: {}", owner.phase));
                render_counters(
                    ui,
                    owner.simulated_source_bytes > 0,
                    owner.simulated_source_bytes,
                    owner.os_offered_bytes,
                    owner.device_verified_bytes,
                );
                row(
                    ui,
                    "Completion",
                    if owner.completion_verified {
                        "Owner reported completion; physical acceptance still requires the matched profile"
                    } else {
                        "Device completion unverified"
                    },
                );
                if let Some(message) = &owner.diagnostic {
                    ui.label(message);
                }
                if owner.quarantined {
                    ui.colored_label(Palette::for_context(ui.ctx()).warning, "Recovery held. Device mode remains unknown without accepted fresh evidence.");
                }
                ui.label(format!("{} retained raw chunks; allocated callback frontier {}. Raw observations are not accepted block acknowledgements.", owner.retained_raw_chunks.len(), owner.allocated_raw_frontier));
            } else {
                row(
                    ui,
                    "Device-verified bytes",
                    "0 bytes; no accepted physical transfer",
                );
                if view.phase != Phase::Idle {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Waiting for named original ownership observations");
                    });
                }
            }
            blocked_update_button(
                ui,
                "The concrete physical update route is unavailable; no flash request can be granted.",
            );
            diagnostics(
                ui,
                "coordinator-diagnostics",
                &view.diagnostics,
                view.omitted_diagnostics,
            );
        }
        section(ui, "Expected physical sequence");
        ui.label("Entry → bootloader reconnect → transfer → section verification → finalization → reboot → fresh normal verification → separate restore review");
        ui.small("This sequence describes work still requiring a matched physical profile. Last send, 100% data offered, same-name USB reappearance, and host retirement do not prove completion.");
        section(ui, "Closing and stopping");
        ui.label("An active owner blocks normal app close while original retirement observations are pending. A forced process or OS shutdown can leave the pedal state indeterminate.");
        ui.small("No Resume, Retry, reset, unplug, or recovery write is offered by this screen.");
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(super) fn refresh_linux_completion_presentation(&mut self, context: &egui::Context) {
        let (owner, view) = self.firmware_coordinator.linux_completion_presentation();
        let coordinator = self.firmware_coordinator_view();
        let entry = (coordinator.phase == Phase::ReadOnlyReview)
            .then(|| self.firmware_coordinator.linux_completion_digest())
            .flatten()
            .map(|digest| (coordinator.generation, digest));
        let now = std::time::Instant::now();
        #[cfg(test)]
        let now = self
            .firmware_ui
            .updater
            .completion_presentation_now
            .unwrap_or(now);
        self.firmware_ui
            .updater
            .project_completion(owner, view, entry, now);
        if self
            .firmware_ui
            .updater
            .completion_presentation
            .as_ref()
            .is_some_and(|frame| !frame.fresh)
            && self.firmware_ui.updater.completion_cache.is_some()
        {
            context.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    pub(super) fn firmware_linux_live_progress(
        &mut self,
        ui: &mut egui::Ui,
        view: gp200_transport::LinuxCompletionView,
        coordinator: &crate::FirmwareCoordinatorView,
        pending: bool,
        fresh: bool,
    ) {
        use gp200_transport::LinuxCompletionPhase as CompletionPhase;
        ui.label(RichText::new(completion_phase_label(view.phase)).strong());
        if let Some(total) = view.total_blocks {
            let fraction = if total == 0 {
                0.0
            } else {
                view.host_accepted_blocks as f32 / total as f32
            };
            ui.add(egui::ProgressBar::new(fraction).text(format!(
                "{} / {total} data blocks • host protocol progress",
                view.host_accepted_blocks
            )));
        }
        ui.small(format!("{} host write invocations • {} wire bytes offered. Progress reflects host protocol responses; it does not measure device flash commits.",view.host_write_invocations,view.os_offered_bytes));
        render_rows(
            ui,
            &[
                (
                    "Host finished".into(),
                    yes_no(view.host_finished_unverified).into(),
                ),
                (
                    "Fresh normal identity, current and globals".into(),
                    yes_no(view.normal_mode_verified).into(),
                ),
                (
                    "V1.8.2 freshly reported from pedal display".into(),
                    yes_no(view.target_version_verified).into(),
                ),
                (
                    "Covered content comparison".into(),
                    if view.contents_verified {
                        "Verified"
                    } else {
                        "Not performed; installed IR audio unavailable"
                    }
                    .into(),
                ),
            ],
        );
        if let Some(review) = &view.review {
            let mut facts = vec![(
                "Firmware image".into(),
                "GP-200 V1.8.2; complete image from section zero".into(),
            )];
            for (label, pointer) in [
                ("Current USB location", "/candidate/binding/topology"),
                ("Current ALSA endpoint", "/candidate/binding/endpoint"),
                ("Live negotiated chunk", "/actual_chunk"),
            ] {
                if let Some(value) = review.pointer(pointer) {
                    let text = value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string());
                    if text.len() <= 2048 {
                        facts.push((label.into(), text));
                    }
                }
            }
            render_rows(ui, &facts);
        }
        match view.phase {
            CompletionPhase::ReviewBootQuery => {
                section(ui, "Check the connected update receiver");
                ui.label("The app found a fresh connection associated with this pedal. USB descriptors alone do not identify update mode. This step sends one identity query and observes the actual GP-200 reply and negotiated transfer size.");
                let confirmation = ui.checkbox(
                    &mut self.firmware_ui.updater.completion_confirmation,
                    "I reviewed this connection and approve one update-receiver identity query",
                );
                self.firmware_record_widget(
                    "Approve update-receiver identity query",
                    &confirmation,
                );
                let response = ui.add_enabled(
                    coordinator.phase == Phase::Owned
                        && fresh
                        && !pending
                        && self.firmware_ui.updater.completion_confirmation,
                    egui::Button::new("Query update receiver"),
                );
                self.firmware_record_widget("Query update receiver", &response);
                if response.clicked() {
                    let result = self.approve_linux_completion_phase(
                        view.phase,
                        view.review_digest.clone().unwrap_or_default(),
                        None,
                    );
                    self.firmware_action_result(result, "One reviewed identity query requested.");
                }
            }
            CompletionPhase::ReviewTransfer => {
                section(ui, "Confirm firmware transfer");
                ui.label("The receiver identity and chunk size were observed in this run. Every section starts at zero. This transfers the retained GP-200 1.8.2 image once, using the vendor sequence, with no automatic retry. Existing data may change and user IR audio remains unavailable.");
                let mut timing = Vec::new();
                for (label, key, allowed, start) in [
                    (
                        "Transfer timeout",
                        "transfer_seconds",
                        60..=3600,
                        "after approval",
                    ),
                    (
                        "Approval window",
                        "approval_wait_seconds",
                        30..=900,
                        "from review publication",
                    ),
                ] {
                    let text = view
                        .review
                        .as_ref()
                        .and_then(|review| review.get(key))
                        .and_then(serde_json::Value::as_u64)
                        .filter(|seconds| allowed.contains(seconds))
                        .map(|seconds| {
                            if seconds % 60 == 0 {
                                let minutes = seconds / 60;
                                let unit = if minutes == 1 { "minute" } else { "minutes" };
                                format!("{minutes} {unit} ({seconds} seconds) {start}")
                            } else {
                                format!("{seconds} seconds {start}")
                            }
                        })
                        .unwrap_or_else(|| "Unavailable in this review".into());
                    timing.push((label.into(), text));
                }
                render_rows(ui, &timing);
                ui.small("Complete hardware duration is unmeasured. The transfer timeout is a finite host deadline; completion time also depends on the pedal's responses and return to normal mode.");
                ui.checkbox(&mut self.firmware_ui.updater.completion_confirmation,"I reviewed the firmware image, retained historical data, unavailable IR audio and recovery limits, and approve this whole-image transfer");
                let response = ui.add_enabled(
                    coordinator.phase == Phase::Owned
                        && fresh
                        && !pending
                        && self.firmware_ui.updater.completion_confirmation,
                    egui::Button::new("Transfer reviewed firmware"),
                );
                self.firmware_record_widget("Transfer reviewed firmware", &response);
                if response.clicked() {
                    let result = self.approve_linux_completion_phase(
                        view.phase,
                        view.review_digest.clone().unwrap_or_default(),
                        None,
                    );
                    self.firmware_action_result(
                        result,
                        "One reviewed full firmware transfer requested.",
                    );
                }
            }
            CompletionPhase::ConfirmApplicationVersion => {
                section(ui, "Read the version on the pedal");
                ui.label("Fresh normal reads succeeded. Open the pedal's system version display and enter the application firmware version shown now. The USB identity reply does not provide that version.");
                ui.horizontal(|ui| {
                    ui.label("Pedal display version");
                    ui.text_edit_singleline(
                        &mut self.firmware_ui.updater.completion_display_version,
                    );
                });
                ui.checkbox(
                    &mut self.firmware_ui.updater.completion_version_read_now,
                    "I read this version on the pedal display after this update",
                );
                let response = ui.add_enabled(
                    coordinator.phase == Phase::Owned
                        && fresh
                        && !pending
                        && self.firmware_ui.updater.completion_version_read_now
                        && !self
                            .firmware_ui
                            .updater
                            .completion_display_version
                            .trim()
                            .is_empty(),
                    egui::Button::new("Confirm displayed version"),
                );
                self.firmware_record_widget("Confirm displayed version", &response);
                if response.clicked() {
                    let result = self.approve_linux_completion_phase(
                        view.phase,
                        view.review_digest.clone().unwrap_or_default(),
                        Some(self.firmware_ui.updater.completion_display_version.clone()),
                    );
                    self.firmware_action_result(
                        result,
                        "Fresh pedal display confirmation submitted.",
                    );
                }
            }
            CompletionPhase::ReturningEditor => {
                ui.label(if coordinator.phase == Phase::Idle {
                    "The original library and editor are available again."
                } else {
                    "Restoring the original library and obtaining fresh editor reads…"
                });
            }
            CompletionPhase::RecoveryRequired => {
                ui.colored_label(Palette::for_context(ui.ctx()).warning,"Update held. Keep the recorded attempt for recovery review. A new full-image recovery requires a fresh process, checked local lineage and separate live query and transfer reviews. Vendor recovery requires access to a Windows or macOS machine.");
            }
            _ => {}
        }
        if let Some(message) = view.diagnostic.as_ref() {
            ui.add(egui::Label::new(message).wrap());
        }
        if let Some(review) = view.review.as_ref() {
            completion_review_details(ui, review);
        }
    }
    #[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
    fn firmware_linux_update_screen(&mut self, ui: &mut egui::Ui) {
        ui.heading("Update your GP-200");
        let coordinator = self.firmware_coordinator_view();
        let pending = self.firmware_coordinator.linux_completion_helper_pending();
        let live = self.firmware_ui.updater.completion_presentation.clone();
        let draft = self.firmware_coordinator.linux_completion_display();
        if self.firmware_coordinator.linux_recovery_active() {
            ui.label("This is a new full-image recovery from update mode. Historical backup material and the new live receiver review are available in History and recovery.");
            ui.colored_label(Palette::for_context(ui.ctx()).warning, "Full-image recovery was tested on one GP-200. This update-mode connection cannot provide a fresh normal backup; all 20 installed IR audio slots are unavailable.");
        } else {
            ui.label("Select and inspect firmware 1.8.2, then capture the pedal data in Prepare update. Each step below uses that same live capture and keeps the editor held until fresh normal reads finish.");
            ui.colored_label(Palette::for_context(ui.ctx()).warning,"Experimental Linux update path. Hardware completion and recovery still need testing. Entry can require a complete update to leave update mode.");
            ui.small("Captured presets, the current buffer and globals are retained. Installed user IR audio cannot be backed up. Content preservation and restoration remain separate from firmware completion.");
            self.firmware_preparation_checklist(ui, !pending && live.is_none());
        }
        if pending {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Preparing or recording your reviewed action…");
            });
        }
        if let Some(frame) = live {
            self.firmware_linux_live_progress(ui, frame.view, &coordinator, pending, frame.fresh);
        } else if let Some((topology, review)) = draft {
            section(ui, "Review entering update mode");
            ui.label("The firmware image and this run's captured data are bound to the connected normal GP-200. Entering update mode sends at most two fixed one-use commands. The pedal may require a complete firmware transfer to return to normal mode.");
            render_rows(
                ui,
                &[
                    ("Firmware image".into(), "GP-200 1.8.2".into()),
                    ("Normal USB connection".into(), topology),
                    (
                        "Installed user IR audio".into(),
                        "Unavailable; cannot be backed up".into(),
                    ),
                ],
            );
            ui.checkbox(&mut self.firmware_ui.updater.completion_confirmation,"I reviewed the image, capture and recovery limits, and approve entering update mode");
            let response = ui.add_enabled(
                !pending
                    && self.firmware_ui.updater.completion_confirmation
                    && self.firmware_ui.updater.power_confirmed
                    && self.firmware_ui.updater.usb_confirmed,
                egui::Button::new("Enter update mode"),
            );
            self.firmware_record_widget("Enter update mode", &response);
            if response.clicked()
                && let Some(digest) = self.firmware_coordinator.linux_completion_digest()
            {
                let result = self.execute_linux_completion(format!("execute-entry:{digest}"));
                self.firmware_action_result(
                    result,
                    "Reviewed entry requested; the editor remains held.",
                );
            }
            completion_review_details(ui, &review);
        } else {
            let response = ui.add_enabled(
                coordinator.phase == Phase::ReadOnlyReview
                    && coordinator.preparation.is_some()
                    && !pending,
                egui::Button::new("Prepare update review"),
            );
            self.firmware_record_widget("Prepare update review", &response);
            if response.clicked() {
                let result = self.prepare_linux_completion_from_resources();
                self.firmware_action_result(
                    result,
                    "Preparing the live update review from this app's firmware resources.",
                );
            }
            ui.small("This experimental build requires its app-provided, build-matched firmware resources. If they are missing, preparation reports the limitation before entering update mode.");
        }
        if !matches!(
            coordinator.phase,
            Phase::Idle | Phase::ReadOnlyReview | Phase::Preparing
        ) && ui.button("Stop update host").clicked()
        {
            self.stop_firmware_preparation();
        }
        ui.small("Stop ends host work and retains the attempt. It cannot cancel or roll back the pedal. History offers a separate review for a new boot-only full-image recovery. No automatic resume, reset or retry is offered.");
        diagnostics(
            ui,
            "linux-update-diagnostics",
            &coordinator.diagnostics,
            coordinator.omitted_diagnostics,
        );
    }
}
pub(crate) fn phase_label(phase: Phase) -> &'static str {
    match phase {
        Phase::Idle => "Local review available",
        Phase::Preparing => "Resolving local work",
        Phase::Capturing => "Acquiring read-only observations",
        Phase::Publishing => "Publishing captured material",
        Phase::Revalidating => "Independently checking the package",
        Phase::ReadOnlyReview => "Reviewing captured data",
        Phase::PreparingLocalReport => "Checking retained local material",
        Phase::WaitingForStore => "Waiting for the original library store",
        Phase::RestoringLibrary => "Restoring the genuine returned store",
        Phase::ReturningNormal => "Retiring the capture owner",
        Phase::RefreshingNormal => "Waiting for fresh normal reads",
        Phase::Owned => "Observing the updater owner",
        Phase::StopRequested => "Stop requested; retirement pending",
        Phase::RecoveryRequired => "Recovery observations required",
    }
}
pub(crate) fn section(ui: &mut egui::Ui, title: &str) {
    super::section(ui, title);
}
pub(crate) fn row(ui: &mut egui::Ui, name: &str, value: &str) {
    ui.label(RichText::new(name).strong());
    ui.add(egui::Label::new(value).wrap());
}
pub(crate) fn copy_value(ui: &mut egui::Ui, name: &str, value: &str) {
    row(ui, name, value);
    if ui.small_button(format!("Copy {name}")).clicked() {
        ui.ctx().copy_text(value.into());
    }
}
pub(crate) fn render_rows(ui: &mut egui::Ui, rows: &[(String, String)]) {
    if ui.available_width() >= 800.0 && rows.len() > 1 {
        let split = rows.len().div_ceil(2);
        ui.columns(2, |columns| {
            for (column, part) in columns.iter_mut().zip(rows.chunks(split)) {
                column.set_max_width(column.available_width().min(600.0));
                for (name, value) in part {
                    row(column, name, value);
                }
            }
        });
    } else {
        for (name, value) in rows {
            row(ui, name, value);
        }
    }
}
pub(crate) fn diagnostics(
    ui: &mut egui::Ui,
    salt: &str,
    messages: &[String],
    omitted: usize,
) -> Option<egui::Response> {
    if messages.is_empty() && omitted == 0 {
        return None;
    }
    let response = egui::CollapsingHeader::new("Limitations and diagnostics").id_salt(("firmware-diagnostics", salt)).default_open(true).show(ui, |ui| {
        for message in messages.iter().take(32) { ui.add(egui::Label::new(message).wrap()); }
        let omitted = omitted + messages.len().saturating_sub(32);
        if omitted > 0 { ui.small(format!("{omitted} older diagnostics omitted from this bounded view; retain the full report.")); }
    });
    Some(response.header_response)
}
fn blocked_update_button(ui: &mut egui::Ui, reason: &str) {
    ui.add_enabled(false, egui::Button::new("Start update"));
    ui.colored_label(Palette::for_context(ui.ctx()).warning, reason);
}
fn render_counters(ui: &mut egui::Ui, simulated: bool, source: u64, offered: u64, verified: u64) {
    if simulated {
        ui.colored_label(
            Palette::for_context(ui.ctx()).warning,
            "Simulated owner counters • no device completion claim",
        );
    }
    row(
        ui,
        "Locally simulated source",
        &format!("{source} bytes; packet generation only"),
    );
    row(
        ui,
        "OS-offered wire",
        &format!("{offered} bytes; offering bytes is not acknowledgement"),
    );
    row(
        ui,
        "Device-verified source",
        &format!("{verified} bytes; physical completion remains separate"),
    );
}
fn render_coverage(ui: &mut egui::Ui, view: &FirmwareCoordinatorView) {
    section(ui, "Data coverage and proposed loss");
    let backup = view
        .preparation
        .as_ref()
        .map(|p| &p.backup)
        .or(view.backup.as_ref());
    let Some(backup) = backup else {
        for (name, value) in [
            ("Stored presets", "0 / 256 captured distinct slots"),
            (
                "Current edit buffer",
                "Not captured; separate from saved presets and local drafts",
            ),
            ("Globals and mappings", "Not captured"),
            (
                "User IR",
                "20 installed audio contents unavailable; metadata capture pending",
            ),
            (
                "SnapTone",
                "Applicability unknown without an associated version report",
            ),
            (
                "Restoration and migration",
                "Destination restoration and unknown-byte preservation acceptance missing",
            ),
        ] {
            row(ui, name, value);
        }
        return;
    };
    row(
        ui,
        "Stored presets",
        &format!(
            "{} / 256 distinct slots captured; comparison {}",
            backup.stored_presets,
            if backup.comparison_complete {
                "checked descriptively"
            } else {
                "incomplete"
            }
        ),
    );
    row(
        ui,
        "Current edit buffer",
        if backup.current_buffer {
            "Captured as its own object; saved-slot relationship remains separate"
        } else {
            "Missing"
        },
    );
    row(
        ui,
        "Globals and mapping tables",
        if backup.globals_and_mappings {
            "Captured raw globals and mappings; post-update persistence unverified"
        } else {
            "Missing"
        },
    );
    row(
        ui,
        "User IR",
        "Installed audio unavailable in all 20 slots. Names, checksums, unused slots, or no prior uploads do not prove empty/default content.",
    );
    row(
        ui,
        "SnapTone applicability",
        &format!("{:?}", backup.context.snaptone_applicability()),
    );
    row(
        ui,
        "Restoration and migration",
        "Captured data does not establish a destination restore route. Hidden/unknown byte preservation and model mapping remain unverified.",
    );
    ui.label(&backup.label);
    ui.label(&backup.non_atomic_limitation);
    egui::CollapsingHeader::new("Every slot and restoration status").show(ui, |ui| {
        for item in backup.coverage.iter().take(FIRMWARE_BACKUP_OBJECT_LIMIT) {
            row(
                ui,
                &format!("{:?}", item.object),
                &format!(
                    "Baseline {}; comparison {}; installed content {}; restoration {:?}",
                    if item.baseline_sha256.is_some() {
                        "captured"
                    } else {
                        "missing"
                    },
                    if item.exact_equal {
                        "raw equal"
                    } else {
                        "different / unavailable"
                    },
                    if item.installed_content_available {
                        "available"
                    } else {
                        "unavailable"
                    },
                    item.restoration
                ),
            );
        }
    });
    egui::CollapsingHeader::new("Affected references and local reconstruction sources").show(
        ui,
        |ui| {
            for reference in backup.references.iter().take(64) {
                ui.label(format!(
                    "{:?}, module {}: {} ({})",
                    reference.object, reference.module, reference.code, reference.classification
                ));
            }
            if backup.references.len() > 64 {
                ui.small(format!(
                    "{} additional references retained in local review export",
                    backup.references.len() - 64
                ));
            }
            for material in &backup.context.local_material {
                row(
                    ui,
                    &format!("{:?} local material", material.object),
                    &format!(
                        "{}; {} bytes; {}. This source is not a backup of installed content.",
                        material.path, material.bytes, material.provenance
                    ),
                );
            }
        },
    );
    row(
        ui,
        "Preservation uncertainty",
        &backup.context.preservation_uncertainty,
    );
    row(
        ui,
        "Recovery limitations",
        &backup.context.recovery_limitations,
    );
}
fn render_material(ui: &mut egui::Ui, view: &FirmwareCoordinatorView) {
    section(ui, "Retained package and freshness");
    if let Some(capture) = &view.capture {
        row(
            ui,
            "Acquisition",
            capture.acquisition.as_deref().unwrap_or("Unknown"),
        );
        row(
            ui,
            "Closing boundary",
            &format!(
                "Closed: {}; cancelled/partial: {}; {} retained records",
                capture.closed, capture.cancelled, capture.records
            ),
        );
        copy_value(ui, "Archive SHA-256", &capture.archive_sha256);
        copy_value(ui, "Sidecar SHA-256", &capture.sidecar_sha256);
        copy_value(ui, "Evidence index SHA-256", &capture.index_sha256);
        ui.label(&capture.limitations);
    } else {
        ui.label("No genuine closed and published acquisition is available in this session.");
    }
    if let Some(preparation) = &view.preparation {
        copy_value(ui, "Local review digest", &preparation.digest);
        copy_value(ui, "Bound image SHA-256", &preparation.image_sha256);
        row(
            ui,
            "Source generation",
            &format!(
                "Normal epoch {}; local draft excluded {}",
                preparation.source_epoch.0, preparation.local_draft_excluded
            ),
        );
        diagnostics(ui, "preparation-blockers", &preparation.blockers, 0);
    }
    ui.small("Independent archive validity and current-device freshness are separate facts. Reopened archival material cannot become a live capture receipt.");
}

fn firmware_report_association(
    session: &DeviceJobSession,
) -> Result<FirmwareDeviceAssociation, ServiceError> {
    FirmwareDeviceAssociation::named_local_record(
        "GP-200",
        format!(
            "normal-{}-{}",
            session.epoch().0,
            session.identity_sha256().hex()
        ),
    )
    .map_err(|e| ServiceError::invalid(e.to_string()))
}
fn firmware_manual_report(
    version: &str,
    date: &str,
    association: FirmwareDeviceAssociation,
) -> Result<UserReportedFirmware, ServiceError> {
    let parts: Vec<_> = date.split('-').collect();
    if parts.len() != 3 {
        return Err(ServiceError::invalid(
            "A manual version needs its report date as YYYY-MM-DD",
        ));
    }
    let date = FirmwareReportDate::new(
        parts[0]
            .parse()
            .map_err(|_| ServiceError::invalid("Invalid report year"))?,
        parts[1]
            .parse()
            .map_err(|_| ServiceError::invalid("Invalid report month"))?,
        parts[2]
            .parse()
            .map_err(|_| ServiceError::invalid("Invalid report day"))?,
    )
    .map_err(|e| ServiceError::invalid(e.to_string()))?;
    UserReportedFirmware::new(version.trim(), association, date)
        .map_err(|e| ServiceError::invalid(e.to_string()))
}

#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn completion_phase_label(phase: gp200_transport::LinuxCompletionPhase) -> &'static str {
    use gp200_transport::LinuxCompletionPhase::*;
    match phase {
        Entry => "Entering update mode",
        FindingBoot => "Finding a fresh pedal connection",
        ReviewBootQuery => "Review the update receiver query",
        QueryingBoot => "Reading the update receiver identity",
        ReviewTransfer => "Review the full firmware transfer",
        Transferring => "Transferring firmware",
        HostFinishedUnverified => "Host sequence finished; checking the pedal",
        FindingNormal => "Waiting for normal mode",
        ReadingNormal => "Reading fresh identity, current buffer and globals",
        ConfirmApplicationVersion => "Confirm the application version on the pedal",
        ClosingAttempt => "Recording the verified return",
        ReturningEditor => "Returning to the editor",
        RecoveryRequired => "Recovery review required",
    }
}
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
fn yes_no(value: bool) -> &'static str {
    if value {
        "Observed"
    } else {
        "Not yet observed"
    }
}
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
pub(super) fn completion_review_details(ui: &mut egui::Ui, review: &serde_json::Value) {
    egui::CollapsingHeader::new("Review details").id_salt("linux-completion-live-review").show(ui,|ui|{
        render_rows(ui,&completion_review_summary(review));
        ui.small("The complete review and original raw capture stay in the published review files and journal. This pane shows bounded review facts and source pins.");
        if review.get("source_pins_truncated").and_then(|v|v.as_bool())==Some(true){ui.small("This view shows the first 16 source pins. All retained pins remain in the complete review file.");}
        if let Some(path)=review.pointer("/inputs/run_directory").and_then(|value|value.as_str()){let path=path.to_string();row(ui,"Complete review directory",&path);if ui.small_button("Copy complete review file path").clicked(){ui.ctx().copy_text(path);}}
        egui::CollapsingHeader::new("Technical source pins").id_salt("linux-completion-source-pins").show(ui,|ui|{
            let mut pins=Vec::new();for pointer in ["/executable","/inputs/contract","/inputs/loss","/inputs/recovery","/inputs/vendor_guidance"]{if let Some(value)=review.pointer(pointer){pins.push(value);}}
            for pointer in ["/captured_materials","/backup","/inputs/sources"]{if let Some(values)=review.pointer(pointer).and_then(|value|value.as_array()){ui.small(format!("{pointer}: {} retained pins",values.len()));pins.extend(values.iter().take(16));}}
            for pin in pins.iter().take(32){if let Some(path)=pin.get("path").and_then(|value|value.as_str()){row(ui,"Pinned file",&path.chars().take(1024).collect::<String>());if let Some(hash)=pin.get("sha256").and_then(|value|value.as_str()){ui.monospace(hash);}}}
            if pins.len()>32{ui.small("Additional pins remain in the complete published review.");}
        });
    });
}
#[cfg(all(feature = "firmware-linux-completion", target_os = "linux"))]
pub(super) fn completion_review_summary(review: &serde_json::Value) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    for (name, pointer) in [
        ("Run", "/run"),
        ("Review purpose", "/purpose"),
        ("Retained job", "/job_sha256"),
        ("Source review", "/source_review"),
        ("Firmware image SHA-256", "/image_sha256"),
        ("Vendor contract SHA-256", "/contract_sha256"),
        ("Actual negotiated chunk", "/actual_chunk"),
        ("Normal source epoch", "/reservation/epoch"),
        ("Fresh normal epoch", "/normal/epoch"),
        ("Normal topology", "/reservation/usb/topology"),
        ("Fresh connection topology", "/candidate/binding/topology"),
        ("Fresh receiver topology", "/fresh_boot/binding/topology"),
        ("Manual application version", "/manual_application_version"),
        ("Terminal control outcome", "/terminal09"),
        ("Limitations", "/limitations"),
        ("Recovery selected predecessor", "/history/selected"),
        (
            "Recovery selected predecessor",
            "/recovery_history/selected",
        ),
        ("Recovery starts at section", "/history/starts_at_section"),
        ("Recovery starts at block", "/history/starts_at_block"),
        ("Current endpoint", "/candidate/binding/endpoint"),
    ] {
        if let Some(value) = review.pointer(pointer) {
            let text = match value {
                serde_json::Value::String(value) => value.chars().take(2048).collect(),
                serde_json::Value::Number(value) => value.to_string(),
                serde_json::Value::Bool(value) => value.to_string(),
                _ => continue,
            };
            rows.push((name.into(), text));
        }
    }
    if let Some(pins) = review
        .pointer("/inputs/sources")
        .and_then(|value| value.as_array())
    {
        rows.push(("Retained source files".into(), pins.len().to_string()));
    }
    if let Some(records) = review
        .pointer("/genuine_preparation/capture_evidence/records")
        .and_then(|value| value.as_u64())
    {
        rows.push(("Captured observation records".into(), records.to_string()));
    }
    rows
}
