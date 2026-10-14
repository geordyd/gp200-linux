use crate::StudioApp;
use eframe::egui::{self, RichText};
use gp200_app::AppState;
use gp200_domain::*;

#[derive(Debug)]
struct BankRead {
    epoch: SessionEpoch,
    bank: AssetBank,
    next: u16,
    waiting: Option<RequestStamp>,
}
#[derive(Default)]
pub(crate) struct AssetSlotUi {
    scan: Option<BankRead>,
    notice: Option<String>,
}
impl AssetSlotUi {
    pub(crate) fn start(&mut self, bank: AssetBank, epoch: SessionEpoch) {
        self.scan = Some(BankRead {
            epoch,
            bank,
            next: 0,
            waiting: None,
        });
        self.notice = None;
    }
    pub(crate) fn is_running(&self) -> bool {
        self.scan.is_some()
    }
    fn stop(&mut self, message: impl Into<String>) {
        self.scan = None;
        self.notice = Some(message.into());
    }
    fn next_read(
        &mut self,
        inventory: &DeviceAssetInventory,
        ready: bool,
        busy: bool,
    ) -> Option<AssetSlot> {
        let scan = self.scan.as_mut()?;
        if scan.epoch != inventory.epoch() || !ready {
            self.stop(crate::localization::text("Bank read stopped because the device session changed. Refresh explicitly after reconnecting."));
            return None;
        }
        if let Some(stamp) = scan.waiting {
            let slot = AssetSlot::new(scan.bank, scan.next).expect("bounded scan");
            match &inventory.entry(slot).status {
                AssetMetadataReadStatus::Ready
                    if inventory
                        .entry(slot)
                        .snapshot
                        .as_ref()
                        .is_some_and(|snapshot| snapshot.stamp() == stamp) =>
                {
                    scan.next += 1;
                    scan.waiting = None;
                }
                AssetMetadataReadStatus::Reading { stamp: actual } if *actual == stamp => {
                    return None;
                }
                AssetMetadataReadStatus::Failed {
                    stamp: actual,
                    error,
                } if *actual == stamp => {
                    let message = std::format!(
                        "Bank read stopped at slot {}: {}",
                        slot.index(),
                        error.message
                    );
                    self.stop(message);
                    return None;
                }
                _ => {
                    self.stop(crate::localization::text(
                        "Bank read stopped because a slot reply did not match the owned request.",
                    ));
                    return None;
                }
            }
        }
        // Read remaining slots only. Previously completed metadata is retained.
        while scan.next < scan.bank.slots()
            && inventory
                .entry(AssetSlot::new(scan.bank, scan.next).expect("bounded scan"))
                .status
                == AssetMetadataReadStatus::Ready
        {
            scan.next += 1;
        }
        if scan.next == scan.bank.slots() {
            self.stop(crate::localization::text(
                "Metadata bank read completed. Slot contents and occupancy remain unknown.",
            ));
            return None;
        }
        if busy {
            return None;
        }
        Some(AssetSlot::new(scan.bank, scan.next).expect("bounded scan"))
    }
    fn submitted(&mut self, inventory: &DeviceAssetInventory, slot: AssetSlot) {
        let Some(scan) = self.scan.as_mut() else {
            return;
        };
        if slot.bank() != scan.bank || slot.index() != scan.next {
            return;
        }
        if let AssetMetadataReadStatus::Reading { stamp } = inventory.entry(slot).status {
            scan.waiting = Some(stamp);
        } else {
            self.stop(crate::localization::text(
                "Bank read could not start; wait for the current operation and retry explicitly.",
            ));
        }
    }
}

impl StudioApp {
    pub(crate) fn advance_asset_metadata_reads(&mut self) {
        if let Some(slot) = self.asset_slot_ui.next_read(
            &self.controller.state().device_assets,
            self.controller.state().connection == ConnectionState::Ready,
            self.controller.device_operation_pending(),
        ) {
            self.send(UiIntent::RefreshAssetMetadata(slot));
            self.asset_slot_ui
                .submitted(&self.controller.state().device_assets, slot);
        }
    }
    pub(super) fn asset_slots(&mut self, ui: &mut egui::Ui) {
        let bank = if self.selected_asset_kind == AssetKind::ImpulseResponse {
            AssetBank::Ir
        } else {
            AssetBank::SnapTone
        };
        if self
            .asset_slot_ui
            .scan
            .as_ref()
            .is_some_and(|scan| scan.bank != bank)
        {
            self.asset_slot_ui.stop(crate::localization::text(
                "Bank read stopped after changing the asset bank.",
            ));
        }
        let operation = if bank == AssetBank::Ir {
            OperationKind::IrRead
        } else {
            OperationKind::SnapToneRead
        };
        let availability = self.controller.operation_availability(operation);
        let busy = self.controller.device_operation_pending();
        let scan_active = self.asset_slot_ui.scan.is_some();
        let allowed = availability == OperationAvailability::Available && !busy && !scan_active;
        let mut refresh = false;
        let mut start = false;
        let mut stop = false;
        ui.group(|ui| {
            ui.label(RichText::new(if bank == AssetBank::Ir { crate::localization::text("Pedal IR slots") } else { crate::localization::text("Pedal SnapTone slots") }).strong().size(17.0));
            ui.label(crate::localization::text("Read names and raw metadata. Audio contents, occupancy and backups require separate proof."));
            ui.horizontal_wrapped(|ui| {
                refresh = ui.add_enabled(allowed, egui::Button::new(crate::localization::text("Refresh selected slot"))).clicked();
                start = ui.add_enabled(allowed, egui::Button::new(crate::localization::text("Read remaining slots"))).clicked();
                if scan_active {
                    stop = ui.button(crate::localization::text("Stop after current read")).clicked();
                }
                ui.label(std::format!("{} / {} metadata records read", self.controller.state().device_assets.complete_count(bank), bank.slots()));
            });
            if let OperationAvailability::Blocked { reason } = &availability {
                ui.label(RichText::new(reason).color(ui.visuals().weak_text_color()));
            }
            if bank == AssetBank::SnapTone && self.controller.state().identity.as_ref().is_some_and(|identity| identity.mode == BackendMode::Hardware) {
                ui.label(crate::localization::text("The captured slot 0 query had no reply within 1 second. This does not establish that SnapTone is unsupported."));
            }
            ui.horizontal_wrapped(|ui| {
                for entry in self.controller.state().device_assets.entries(bank) {
                    let marker = match entry.status { AssetMetadataReadStatus::Ready => "read", AssetMetadataReadStatus::Reading { .. } => "...", AssetMetadataReadStatus::Failed { .. } => "!", AssetMetadataReadStatus::Stale => "old", AssetMetadataReadStatus::NeverRead => "?" };
                    if ui.selectable_label(self.selected_asset_slot == entry.slot.index() as u8, std::format!("{} {marker}", entry.slot.index())).clicked() {
                        self.selected_asset_slot = entry.slot.index() as u8;
                    }
                }
            });
            let selected = AssetSlot::new(bank, u16::from(self.selected_asset_slot)).unwrap_or_else(|_| AssetSlot::new(bank, 0).expect("slot zero"));
            slot_detail(ui, self.controller.state(), selected);
            if let Some(message) = &self.asset_slot_ui.notice { ui.label(crate::localization::message(message)); }
        });
        if stop {
            self.asset_slot_ui.stop(crate::localization::text(
                "Bank read stopped. The current read can finish; it sends no writes.",
            ));
        }
        if start {
            self.asset_slot_ui
                .start(bank, self.controller.state().epoch);
        }
        if refresh {
            let selected = AssetSlot::new(bank, u16::from(self.selected_asset_slot))
                .unwrap_or_else(|_| AssetSlot::new(bank, 0).expect("slot zero"));
            self.send(UiIntent::RefreshAssetMetadata(selected));
        }
        if self.asset_slot_ui.scan.is_some() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(50));
        }
    }
}

fn slot_detail(ui: &mut egui::Ui, state: &AppState, slot: AssetSlot) {
    let entry = state.device_assets.entry(slot);
    let status = match &entry.status {
        AssetMetadataReadStatus::NeverRead => crate::localization::text("Not read").to_owned(),
        AssetMetadataReadStatus::Reading { .. } => {
            crate::localization::text("Reading metadata…").to_owned()
        }
        AssetMetadataReadStatus::Ready => crate::localization::text("Metadata read").to_owned(),
        AssetMetadataReadStatus::Failed { error, .. } => {
            std::format!("Read failed: {}", error.message)
        }
        AssetMetadataReadStatus::Stale => {
            crate::localization::text("Previous session metadata; refresh required").to_owned()
        }
    };
    ui.label(RichText::new(std::format!("Slot {} · {status}", slot.index())).strong());
    if let Some(snapshot) = &entry.snapshot {
        let provenance = match snapshot.source() {
            SnapshotSource::Hardware { epoch } => std::format!("Pedal read · session {}", epoch.0),
            SnapshotSource::Simulator => {
                crate::localization::text("Simulator · fixture metadata").into()
            }
            SnapshotSource::Capture { capture_id } => std::format!("Capture replay · {capture_id}"),
            SnapshotSource::Library { version } => {
                std::format!("Local library · {version}")
            }
        };
        ui.label(std::format!("Name: {}", snapshot.metadata().display_name()));
        ui.label(provenance);
        ui.label(crate::localization::text(
            "Contents and occupancy: unknown. Complete content backup: unavailable.",
        ));
        ui.collapsing(crate::localization::text("Retained raw metadata"), |ui| {
            ui.monospace(std::format!(
                "Flags: 0x{:04x}   Checksum: 0x{:08x}",
                snapshot.metadata().flags_raw,
                snapshot.metadata().checksum_raw
            ));
            ui.monospace(
                snapshot
                    .raw()
                    .payload()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            ui.label(std::format!(
                "Revision {} · captured at {} ms · request {}",
                snapshot.revision().0,
                snapshot.captured_at_ms(),
                snapshot.stamp().intent.0
            ));
        });
    } else {
        ui.label(crate::localization::text(
            "No slot metadata has been read in this session.",
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    struct NoDevice;
    impl DeviceService for NoDevice {
        fn try_submit(&self, command: DeviceCommand) -> Result<(), SubmitError<DeviceCommand>> {
            panic!("Passive slot rendering attempted a device command: {command:?}")
        }
        fn try_next_event(&self) -> Option<DeviceEvent> {
            None
        }
        fn shutdown(&self) {}
    }
    #[test]
    fn rendering_ready_asset_slots_in_both_themes_enqueues_no_read_or_write() {
        let mut controller = gp200_app::AppController::new(
            gp200_catalog::Catalog::embedded().unwrap(),
            AppPreferences::default(),
        );
        controller.dispatch(AppInput::Device(DeviceEvent::SessionChanged {
            epoch: SessionEpoch(1),
            identity: DeviceIdentity {
                model: DeviceModel::Gp200,
                mode: BackendMode::Simulator,
                firmware: None,
                display_name: "Simulator".into(),
            },
            capabilities: Capabilities::simulator(),
        }));
        controller.dispatch(AppInput::Device(DeviceEvent::ConnectionChanged {
            epoch: SessionEpoch(1),
            state: ConnectionState::Ready,
        }));
        let mut app = StudioApp::with_services(
            controller,
            crate::runtime::RuntimeServices::new(Box::new(NoDevice), no_wake()),
        );
        let context = egui::Context::default();
        for theme in [ThemePreference::Dark, ThemePreference::Light] {
            crate::theme::apply(&context, theme, 1.0);
            for kind in [AssetKind::ImpulseResponse, AssetKind::SnapTone] {
                app.selected_asset_kind = kind;
                let mut output = context.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(960.0, 640.0),
                        )),
                        ..Default::default()
                    },
                    |ui| app.asset_slots(ui),
                );
                assert!(!output.shapes.is_empty());
                assert!(!app.services.has_pending());
                assert!(!app.controller.device_operation_pending());
                assert_eq!(
                    app.controller
                        .state()
                        .device_assets
                        .complete_count(AssetBank::Ir),
                    0
                );
                assert_eq!(
                    app.controller
                        .state()
                        .device_assets
                        .complete_count(AssetBank::SnapTone),
                    0
                );
                output.textures_delta.clear();
            }
        }
    }
    fn slot(index: u16) -> AssetSlot {
        AssetSlot::new(AssetBank::Ir, index).unwrap()
    }
    fn stamp(intent: u64) -> RequestStamp {
        RequestStamp {
            epoch: SessionEpoch(1),
            intent: IntentId(intent),
        }
    }
    fn complete(cache: &mut DeviceAssetInventory, index: u16, intent: u64) {
        let bank = RawTransfer::from_payload(
            gp200_protocol::WRITE_KIND,
            include_bytes!(
                "../../../../tests/fixtures/device-assets/connected-ir-metadata.payload.bin"
            )
            .to_vec(),
        )
        .unwrap();
        let raw = RawTransfer::from_records(
            gp200_protocol::WRITE_KIND,
            &bank.records()[usize::from(index)..usize::from(index) + 1],
        )
        .unwrap();
        cache.begin(slot(index), stamp(intent));
        assert!(
            cache.complete(Arc::new(
                DeviceAssetMetadata::from_transfer(
                    slot(index),
                    stamp(intent),
                    Revision(intent),
                    SnapshotSource::Simulator,
                    0,
                    raw
                )
                .unwrap()
            ))
        );
    }
    #[test]
    fn sequential_bank_read_waits_for_owned_reply_and_skips_completed_slots() {
        let mut cache = DeviceAssetInventory::new(SessionEpoch(1));
        complete(&mut cache, 0, 1);
        let mut ui = AssetSlotUi::default();
        assert!(ui.next_read(&cache, true, false).is_none());
        ui.start(AssetBank::Ir, SessionEpoch(1));
        assert!(ui.next_read(&cache, true, true).is_none());
        assert_eq!(ui.next_read(&cache, true, false), Some(slot(1)));
        cache.begin(slot(1), stamp(2));
        ui.submitted(&cache, slot(1));
        assert!(ui.next_read(&cache, true, false).is_none());
        complete(&mut cache, 1, 2);
        assert_eq!(ui.next_read(&cache, true, false), Some(slot(2)));
    }
    #[test]
    fn failed_or_disconnected_scan_stops_without_replay() {
        let mut cache = DeviceAssetInventory::new(SessionEpoch(1));
        let mut ui = AssetSlotUi::default();
        ui.start(AssetBank::Ir, SessionEpoch(1));
        cache.begin(slot(0), stamp(2));
        ui.submitted(&cache, slot(0));
        cache.fail(
            slot(0),
            stamp(2),
            ServiceError::new(ErrorKind::Timeout, "No reply"),
        );
        assert!(ui.next_read(&cache, true, false).is_none());
        assert!(ui.scan.is_none());
        assert!(ui.notice.as_ref().unwrap().contains("No reply"));
        ui.start(AssetBank::Ir, SessionEpoch(1));
        cache.invalidate(SessionEpoch(2));
        assert!(ui.next_read(&cache, true, false).is_none());
        assert!(ui.scan.is_none());
        assert!(ui.next_read(&cache, true, false).is_none());
    }
    #[test]
    fn successful_bank_completion_does_not_restart_and_metadata_is_never_occupancy() {
        let mut cache = DeviceAssetInventory::new(SessionEpoch(1));
        for index in 0..20 {
            complete(&mut cache, index, u64::from(index) + 1);
        }
        let mut ui = AssetSlotUi::default();
        ui.start(AssetBank::Ir, SessionEpoch(1));
        assert!(ui.next_read(&cache, true, false).is_none());
        assert!(ui.scan.is_none());
        assert!(
            ui.notice
                .as_ref()
                .unwrap()
                .contains("occupancy remain unknown")
        );
    }
}
