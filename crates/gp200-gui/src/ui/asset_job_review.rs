use crate::{StudioApp, asset_job_workflow::*, runtime::FilePurpose, theme::Palette};
use eframe::egui::{self, RichText};
use gp200_app::AppController;
use gp200_domain::*;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

#[derive(Clone)]
pub struct AssetJobWorkspaceLaunch {
    pub source: PathBuf,
    pub bank: AssetBank,
    pub slot: u16,
    pub name: String,
}

#[cfg(test)]
#[path = "asset_job_review_tests.rs"]
mod tests;
#[derive(Clone)]
struct ReadContext {
    binding: Option<LiveBinding>,
    prerequisites: Vec<String>,
    references: Vec<AffectedPreset>,
    checked: usize,
    digest: JobSha256,
}
fn context(controller: &AppController, slot: Result<AssetSlot, String>) -> ReadContext {
    let state = controller.state();
    let mut prerequisites = vec![];
    let mut binding = None;
    let mut references = vec![];
    let mut checked = 0;
    if let Ok(slot) = slot {
        let entry = state.device_assets.entry(slot);
        if state.connection != ConnectionState::Ready {
            prerequisites.push(
                "Connect in Normal USB mode to bind this review to a current device session".into(),
            );
        }
        if controller.device_operation_pending() {
            prerequisites
                .push("Wait for the pending device operation before preparing a review".into());
        }
        let session = state.device_job_session.as_ref().filter(|session| {
            session.epoch() == state.epoch
                && state.identity.as_ref().is_some_and(|identity| {
                    session.matches_capabilities(identity, &state.capabilities)
                })
        });
        if session.is_none() {
            prerequisites.push(
                "A complete identity observation with exact current endpoints is required".into(),
            );
        }
        let baseline = entry.snapshot.as_ref().filter(|baseline| {
            entry.status == AssetMetadataReadStatus::Ready && baseline.stamp().epoch == state.epoch
        });
        if baseline.is_none() {
            prerequisites.push("Refresh the selected slot metadata in this session; its contents and occupancy remain unknown".into());
        }
        if prerequisites.is_empty()
            && let (Some(session), Some(baseline)) = (session, baseline)
        {
            binding = Some(LiveBinding {
                session: session.clone(),
                baseline: baseline.clone(),
            });
        }
        let refers = |modules: &[ModuleSnapshot]| {
            modules.iter().any(|module| match slot.bank() {
                AssetBank::Ir => {
                    module.module == ModuleId::new(5).expect("CAB")
                        && module.model.0 == 0x0a100000 + u32::from(slot.index())
                }
                AssetBank::SnapTone => {
                    [2, 3].contains(&module.module.get())
                        && module.model.0 == 0x0f000000 + u32::from(slot.index())
                }
            })
        };
        if state.bank.epoch == state.epoch {
            for (stored, cached) in &state.bank.slots {
                if !cached.fresh {
                    continue;
                }
                checked += 1;
                if refers(&cached.snapshot.view.modules) {
                    references.push(AffectedPreset {
                        location: format!("Stored slot {}", stored.get()),
                        name: cached.snapshot.view.display_name().into(),
                        sha256: JobSha256::of(cached.snapshot.raw.payload()),
                    });
                }
            }
        }
        for (id, document) in &state.documents {
            if refers(&document.draft.view.modules)
                && let Ok(snapshot) = controller.document_snapshot(id)
            {
                references.push(AffectedPreset {
                    location: format!("Open draft {}", id.0),
                    name: snapshot.view.display_name().into(),
                    sha256: JobSha256::of(snapshot.raw.payload()),
                });
            }
        }
    } else {
        prerequisites.push("Choose a supported slot in this asset bank".into());
    }
    let bytes = serde_json::to_vec(&serde_json::json!({
        "session":binding.as_ref().map(|b| &b.session),
        "baseline":binding.as_ref().map(|b| (b.baseline.slot(), b.baseline.stamp(), b.baseline.revision(), JobSha256::of(b.baseline.raw().payload()))),
        "prerequisites":prerequisites, "references":references, "checked":checked,
    })).expect("Read-only review context JSON");
    ReadContext {
        binding,
        prerequisites,
        references,
        checked,
        digest: JobSha256::of(&bytes),
    }
}

pub(crate) struct AssetJobUi {
    pub open: bool,
    pub path: String,
    pub name: String,
    pub truncate: bool,
    inventory_source: Option<AssetInfo>,
    worker: AssetJobWorker,
    local: Option<LocalAssetReview>,
    prepared: Option<PreparedDeviceJob>,
    prepared_context: Option<JobSha256>,
    inspected_selection: Option<SourceSelection>,
    pending: Option<(u64, Arc<AtomicBool>, Option<JobSha256>)>,
    sequence: u64,
    pub message: String,
    pub settled: Option<Instant>,
    pub export_pending: Option<ReviewReport>,
    #[cfg(test)]
    inspect_rect: Option<egui::Rect>,
    #[cfg(test)]
    prepare_rect: Option<egui::Rect>,
}
impl Default for AssetJobUi {
    fn default() -> Self {
        Self {
            open: false,
            path: String::new(),
            name: crate::localization::text("My asset").into(),
            truncate: false,
            inventory_source: None,
            worker: Default::default(),
            local: None,
            prepared: None,
            prepared_context: None,
            inspected_selection: None,
            pending: None,
            sequence: 0,
            message: String::new(),
            settled: None,
            export_pending: None,
            #[cfg(test)]
            inspect_rect: None,
            #[cfg(test)]
            prepare_rect: None,
        }
    }
}
impl Drop for AssetJobUi {
    fn drop(&mut self) {
        self.cancel();
    }
}
impl AssetJobUi {
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }
    fn cancel(&mut self) {
        if let Some((_, cancel, _)) = self.pending.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.settled = Some(Instant::now());
    }
    fn invalidate(&mut self, reason: &str) {
        self.cancel();
        self.local = None;
        self.prepared = None;
        self.prepared_context = None;
        self.inspected_selection = None;
        self.export_pending = None;
        self.message = reason.into();
    }
    fn submit(&mut self, work: Work, digest: Option<JobSha256>, message: &str) {
        self.cancel();
        self.sequence += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        match self.worker.submit(self.sequence, cancel.clone(), work) {
            Ok(()) => {
                self.pending = Some((self.sequence, cancel, digest));
                self.message = message.into();
                self.settled = None;
            }
            Err(error) => self.message = error,
        }
    }
    fn selection(&self, bank: AssetBank, slot: u16) -> SourceSelection {
        let selected = self
            .inventory_source
            .as_ref()
            .filter(|source| source.path.as_path() == std::path::Path::new(&self.path));
        SourceSelection {
            path: PathBuf::from(&self.path),
            expected_sha256: selected.and_then(|source| {
                source
                    .id
                    .0
                    .strip_prefix("sha256:")
                    .and_then(|hex| JobSha256::parse(hex).ok())
            }),
            bank,
            slot,
            name: self.name.clone(),
            truncate: self.truncate,
            provenance: selected
                .map(|source| source.properties.clone())
                .unwrap_or_default(),
        }
    }
    fn inspect(&mut self, selection: SourceSelection) {
        self.local = None;
        self.prepared = None;
        self.prepared_context = None;
        self.export_pending = None;
        self.inspected_selection = Some(selection.clone());
        self.submit(
            Work::Inspect(selection),
            None,
            crate::localization::text(
                "Inspecting immutable source bytes and reconstructing the exact local recipe…",
            ),
        );
    }
    fn prepare(&mut self, read: &ReadContext, id: JobId) {
        if let (Some(local), Some(binding)) = (self.local.clone(), read.binding.clone()) {
            self.prepared = None;
            self.export_pending = None;
            self.submit(
                Work::Prepare {
                    local: Box::new(local),
                    binding: Box::new(binding),
                    id,
                },
                Some(read.digest),
                crate::localization::text(
                    "Checking source bytes again and sealing the selected slot review…",
                ),
            );
        }
    }
    fn accept(
        &mut self,
        reply: Reply,
        selection: &SourceSelection,
        read: &ReadContext,
        capabilities: &Capabilities,
    ) {
        let Some((id, _, captured)) = self.pending.as_ref() else {
            return;
        };
        if *id != reply.id {
            return;
        }
        let captured = *captured;
        self.pending = None;
        self.settled = Some(Instant::now());
        if self.inspected_selection.as_ref() != Some(selection)
            || captured.is_some_and(|digest| digest != read.digest)
        {
            self.prepared = None;
            self.prepared_context = None;
            self.export_pending = None;
            self.message=crate::localization::text("Review discarded because its source, target or device context changed; inspect again").into();
            return;
        }
        match reply.result {
            Ok(Completed::Inspected(local)) => {
                self.local = Some(*local);
                self.message=crate::localization::text("Exact source and recipe inspected locally. Prepare binds it to refreshed slot metadata.").into();
            }
            Ok(Completed::Prepared(job)) => {
                if let Some(binding) = &read.binding {
                    match job.recheck_active(
                        &binding.session,
                        binding.session.identity(),
                        capabilities,
                        &binding.baseline,
                    ) {
                        Ok(()) => {
                            self.prepared = Some(job);
                            self.prepared_context = Some(read.digest);
                            self.message=crate::localization::text("Immutable preparation sealed. Transfer remains blocked until acceptance, backup and recovery are proved.").into();
                        }
                        Err(error) => self.message = error.message,
                    }
                }
            }
            Ok(Completed::Exported(path)) => {
                self.message = std::format!("Review report saved to {}", path.display())
            }
            Err(error) => {
                self.prepared = None;
                self.prepared_context = None;
                self.message = error;
            }
        }
    }
    fn report(&self, read: &ReadContext, capabilities: &Capabilities) -> Option<ReviewReport> {
        let local = self.local.as_ref()?;
        Some(ReviewReport {
            schema: 1,
            operation: "local_normal_asset_review",
            source: local.selection.clone(),
            source_sha256: local.source.sha256(),
            source_bytes: local.source.bytes().len(),
            derived_sha256: local.recipe.derived_sha256(),
            derived_bytes: local.recipe.derived().len(),
            codec_revision: local.conversion.converter_revision().into(),
            options_sha256: local.conversion.options_sha256(),
            technical: local.technical.clone(),
            session: read.binding.as_ref().map(|binding| binding.session.clone()),
            prepared: self.prepared.as_ref().map(|job| job.review(capabilities)),
            prerequisites: read.prerequisites.clone(),
            affected_presets: read.references.clone(),
            fresh_stored_presets_checked: read.checked,
            full_content_backup_verified: false,
            occupancy_known: false,
            hardware_commands: 0,
            permits_transfer: false,
        })
    }
    pub fn export_to(&mut self, path: PathBuf) {
        if let Some(report) = self.export_pending.take() {
            self.submit(
                Work::Export {
                    report: Box::new(report),
                    path,
                },
                self.prepared_context,
                crate::localization::text("Saving the immutable local review report…"),
            );
        }
    }
}
impl StudioApp {
    pub fn open_asset_job_review(&mut self, launch: AssetJobWorkspaceLaunch) {
        self.selected_asset_kind = if launch.bank == AssetBank::Ir {
            AssetKind::ImpulseResponse
        } else {
            AssetKind::SnapTone
        };
        // Reject unknown indices through the typed selection instead of wrapping.
        if launch.slot >= launch.bank.slots() {
            self.asset_job_ui
                .invalidate(crate::localization::text("Unsupported asset slot"));
            self.asset_job_ui.message = crate::localization::text("Unsupported asset slot").into();
            return;
        }
        self.selected_asset_slot = launch.slot as u8;
        self.asset_job_ui.open = true;
        self.asset_job_ui.path = launch.source.display().to_string();
        self.asset_job_ui.name = launch.name;
        self.send(UiIntent::Navigate(ViewId::Assets));
        let selection = self.asset_job_selection();
        self.asset_job_ui.inspect(selection);
    }
    pub fn asset_job_review_report(&self) -> Option<serde_json::Value> {
        let selection = self.asset_job_selection();
        let read = context(&self.controller, selection.target());
        self.asset_job_ui
            .report(&read, &self.controller.state().capabilities)
            .map(|report| report.sealed_json())
    }
    pub fn prepare_asset_job_review(&mut self) {
        let read = context(&self.controller, self.asset_job_selection().target());
        self.next_job += 1;
        self.asset_job_ui.prepare(&read, JobId(self.next_job));
    }
    fn asset_job_selection(&self) -> SourceSelection {
        self.asset_job_ui.selection(
            if self.selected_asset_kind == AssetKind::ImpulseResponse {
                AssetBank::Ir
            } else {
                AssetBank::SnapTone
            },
            u16::from(self.selected_asset_slot),
        )
    }
    pub(crate) fn advance_asset_job_review(&mut self) {
        let selection = self.asset_job_selection();
        let read = context(&self.controller, selection.target());
        if self
            .asset_job_ui
            .inspected_selection
            .as_ref()
            .is_some_and(|previous| previous != &selection)
        {
            self.asset_job_ui.invalidate(crate::localization::text(
                "Source or target changed. Inspect and prepare a fresh review.",
            ));
        } else if self
            .asset_job_ui
            .prepared_context
            .is_some_and(|previous| previous != read.digest)
        {
            self.asset_job_ui.prepared = None;
            self.asset_job_ui.prepared_context = None;
            self.asset_job_ui.export_pending = None;
            self.asset_job_ui.message = crate::localization::text(
                "Device metadata, session or affected presets changed. Prepare a fresh review.",
            )
            .into();
        }
        let replies: Vec<_> = self.asset_job_ui.worker.poll().collect();
        for reply in replies {
            self.asset_job_ui.accept(
                reply,
                &selection,
                &read,
                &self.controller.state().capabilities,
            );
        }
    }
    pub(crate) fn select_asset_for_review(&mut self, asset: AssetInfo) {
        self.selected_asset_kind = asset.kind;
        self.asset_job_ui.invalidate("");
        self.asset_job_ui.open = true;
        self.asset_job_ui.path = asset.path.display().to_string();
        self.asset_job_ui.inventory_source = Some(asset);
    }
    pub(crate) fn asset_job_source_selected(&mut self, path: PathBuf) {
        self.asset_job_ui.invalidate("");
        self.asset_job_ui.open = true;
        self.asset_job_ui.path = path.display().to_string();
        self.asset_job_ui.inventory_source = None;
    }
    pub(super) fn asset_job_review_workspace(&mut self, ui: &mut egui::Ui) {
        let palette = Palette::for_context(ui.ctx());
        let bank = if self.selected_asset_kind == AssetKind::ImpulseResponse {
            AssetBank::Ir
        } else {
            AssetBank::SnapTone
        };
        ui.heading(crate::localization::text("Review a device asset"));
        ui.label(crate::localization::text("Inspect exact local bytes, then bind a preparation to current slot metadata. No upload is available yet."));
        ui.horizontal_wrapped(|ui| {
            if ui
                .button(crate::localization::text("Select source…"))
                .clicked()
            {
                self.services.dialog(if bank == AssetBank::Ir {
                    FilePurpose::ReviewIr
                } else {
                    FilePurpose::ReviewSnapTone
                });
            }
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.asset_job_ui.path)
                    .hint_text(crate::localization::text("WAV or CLO source path"))
                    .desired_width(ui.available_width().min(470.0)),
            );
            if response.changed() {
                self.asset_job_ui.inventory_source = None;
            }
            egui::ComboBox::from_id_salt("device-review-inventory")
                .selected_text(crate::localization::text("Retained sources"))
                .show_ui(ui, |ui| {
                    for asset in self.controller.state().assets.clone() {
                        if asset.kind
                            == if bank == AssetBank::Ir {
                                AssetKind::ImpulseResponse
                            } else {
                                AssetKind::SnapTone
                            }
                            && ui.selectable_label(false, &asset.name).clicked()
                        {
                            self.select_asset_for_review(asset);
                        }
                    }
                });
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(crate::localization::text("Target slot"));
            egui::ComboBox::from_id_salt("device-review-target")
                .selected_text(self.selected_asset_slot.to_string())
                .show_ui(ui, |ui| {
                    for index in 0..bank.slots() {
                        ui.selectable_value(
                            &mut self.selected_asset_slot,
                            index as u8,
                            index.to_string(),
                        );
                    }
                });
            ui.label(crate::localization::text("Slot name"));
            ui.add(
                egui::TextEdit::singleline(&mut self.asset_job_ui.name)
                    .char_limit(16)
                    .desired_width(170.0),
            );
            if bank == AssetBank::Ir {
                ui.checkbox(
                    &mut self.asset_job_ui.truncate,
                    crate::localization::text("Keep first 1024 samples"),
                );
            }
        });
        self.advance_asset_job_review();
        let selection = self.asset_job_selection();
        let read = context(&self.controller, selection.target());
        ui.horizontal_wrapped(|ui| {
            let availability = self
                .controller
                .operation_availability(if bank == AssetBank::Ir {
                    OperationKind::IrRead
                } else {
                    OperationKind::SnapToneRead
                });
            if ui
                .add_enabled(
                    availability == OperationAvailability::Available
                        && !self.controller.device_operation_pending(),
                    egui::Button::new(crate::localization::text("Refresh selected metadata")),
                )
                .clicked()
                && let Ok(slot) = selection.target()
            {
                self.send(UiIntent::RefreshAssetMetadata(slot));
            }
            let inspect = ui.add_enabled(
                !self.asset_job_ui.path.is_empty() && !self.asset_job_ui.busy(),
                egui::Button::new(crate::localization::text("Inspect exact recipe")),
            );
            #[cfg(test)]
            {
                self.asset_job_ui.inspect_rect = Some(inspect.rect);
            }
            if inspect.clicked() {
                self.asset_job_ui.inspect(selection.clone());
            }
            let prepare = ui.add_enabled(
                self.asset_job_ui.local.is_some()
                    && read.binding.is_some()
                    && !self.asset_job_ui.busy(),
                egui::Button::new(crate::localization::text("Prepare slot review")),
            );
            #[cfg(test)]
            {
                self.asset_job_ui.prepare_rect = Some(prepare.rect);
            }
            if prepare.clicked() {
                self.prepare_asset_job_review();
            }
            if self.asset_job_ui.busy()
                && ui
                    .button(crate::localization::text("Cancel local work"))
                    .clicked()
            {
                self.asset_job_ui.cancel();
                self.asset_job_ui.message = crate::localization::text(
                    "Local work cancelled. No device command was submitted.",
                )
                .into();
            }
            if self.asset_job_ui.local.is_some()
                && !self.asset_job_ui.busy()
                && ui
                    .button(crate::localization::text("Export review…"))
                    .clicked()
            {
                self.asset_job_ui.export_pending = self
                    .asset_job_ui
                    .report(&read, &self.controller.state().capabilities);
                self.services.dialog(FilePurpose::ExportAssetJobReport);
            }
        });
        if !self.asset_job_ui.message.is_empty() {
            ui.label(crate::localization::message(&self.asset_job_ui.message));
        }
        ui.separator();
        ui.columns(2, |columns| {
            columns[0].label(RichText::new(crate::localization::text("Local material")).strong());
            if let Some(local)=&self.asset_job_ui.local {
                columns[0].label(crate::localization::message(&local.technical));
                columns[0].label(std::format!("{} source bytes → {} recipe bytes", local.source.bytes().len(), local.recipe.derived().len()));
                columns[0].small(std::format!("Codec: {}", local.conversion.converter_revision()));
                columns[0].collapsing(crate::localization::text("Exact hashes and provenance"), |ui| {
                    ui.label(std::format!("Source SHA-256: {}", local.source.sha256().hex()));
                    ui.label(std::format!("Derived SHA-256: {}", local.recipe.derived_sha256().hex()));
                    ui.label(std::format!("Options SHA-256: {}", local.conversion.options_sha256().hex()));
                    ui.label(crate::localization::text("The direct recipe uses this exact WAV or CLO. Retained conversion history describes earlier local processing; it grants no device-content proof."));
                    for (key,value) in &local.selection.provenance { if key.ends_with("_json") { ui.collapsing(key, |ui| { ui.label(value); }); } else { ui.label(std::format!("{key}: {value}")); } }
                });
            } else { columns[0].label(crate::localization::text("Choose a mono PCM24/44100 Hz WAV or a supported full VTSI CLO container.")); }
            columns[1].label(RichText::new(crate::localization::text("Device prerequisites")).strong());
            for prerequisite in &read.prerequisites { columns[1].label(crate::localization::message(prerequisite)); }
            if let Some(binding)=&read.binding {
                columns[1].label(std::format!("{:?} · session {} · identity revision {}", binding.session.identity().mode, binding.session.epoch().0, binding.session.identity_revision().0));
                columns[1].label(std::format!("Slot {}: {} · metadata revision {}", binding.baseline.slot().index(), binding.baseline.metadata().display_name(), binding.baseline.revision().0));
            }
            columns[1].label(crate::localization::text("Occupancy: unknown. Current audio backup: unavailable."));
            columns[1].colored_label(palette.warning, crate::localization::text("Transfer, readback and exact recovery remain unverified."));
        });
        if let Some(job) = &self.asset_job_ui.prepared {
            ui.separator();
            ui.label(
                RichText::new(crate::localization::text("Immutable prepared review")).strong(),
            );
            ui.label(std::format!(
                "Review SHA-256: {}",
                job.review_sha256().hex()
            ));
            if let Some(report) = self
                .asset_job_ui
                .report(&read, &self.controller.state().capabilities)
            {
                ui.small(std::format!(
                    "Full report SHA-256: {}",
                    report.sha256().hex()
                ));
            }
            for blocker in job.review(&self.controller.state().capabilities).blockers {
                ui.label(crate::localization::text(blocker.explanation()));
            }
        }
        ui.collapsing(std::format!("Replacement impact · {} known references", read.references.len()), |ui| {
            ui.label(std::format!("{} / 256 stored presets checked in the current session. Open drafts are checked separately; unscanned presets may still reference this slot.", read.checked));
            for reference in &read.references { ui.label(format!("{} · {}", crate::localization::message(&reference.location), reference.name)); }
        });
        if self.asset_job_ui.busy() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(16));
        }
    }
}
