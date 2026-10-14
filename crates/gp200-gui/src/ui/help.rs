//! Offline help is passive. Only deliberate navigation and external-link clicks act.
use super::section;
use crate::{StudioApp, theme::Palette};
use eframe::egui::{self, RichText};
use gp200_domain::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HelpTopic {
    #[default]
    Start,
    Editing,
    Backups,
    Assets,
    Troubleshooting,
    About,
}
impl HelpTopic {
    pub const ALL: [(Self, &'static str); 6] = [
        (Self::Start, "Getting started"),
        (Self::Editing, "Editing"),
        (Self::Backups, "Backups"),
        (Self::Assets, "IR and NAM"),
        (Self::Troubleshooting, "Troubleshooting"),
        (Self::About, "About"),
    ];
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "start" => Some(Self::Start),
            "editing" => Some(Self::Editing),
            "backups" => Some(Self::Backups),
            "assets" => Some(Self::Assets),
            "troubleshooting" => Some(Self::Troubleshooting),
            "about" => Some(Self::About),
            _ => None,
        }
    }
}

/// Fixed official pages. No user-derived URLs or background update checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OfficialLink {
    Product,
    Manual,
    Manuals,
    Firmware,
    Downloads,
    Faq,
}
impl OfficialLink {
    pub const ALL: [Self; 6] = [
        Self::Manual,
        Self::Manuals,
        Self::Firmware,
        Self::Downloads,
        Self::Faq,
        Self::Product,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Product => crate::localization::text("GP-200 product and software"),
            Self::Manual => crate::localization::text("GP-200 English manual (1.8.0)"),
            Self::Manuals => crate::localization::text("All official manuals"),
            Self::Firmware => crate::localization::text("Official firmware downloads"),
            Self::Downloads => crate::localization::text("Official software downloads"),
            Self::Faq => crate::localization::text("GP-200 questions and MIDI guidance"),
        }
    }
    pub fn url(self) -> &'static str {
        match self {
            Self::Product => "https://www.valeton.net/product/gp-200/",
            Self::Manual => {
                "https://www.valeton.net/wpfd_file/gp-200_online-manual_en_firmware-v1-8-0-pdf/"
            }
            Self::Manuals => "https://www.valeton.net/user-manual/",
            Self::Firmware => "https://www.valeton.net/firmware/",
            Self::Downloads => "https://www.valeton.net/download/",
            Self::Faq => "https://www.valeton.net/gp-200-faq/",
        }
    }
}

impl StudioApp {
    pub(super) fn help(&mut self, ui: &mut egui::Ui) {
        ui.heading(crate::localization::text(
            "Help and application information",
        ));
        ui.horizontal_wrapped(|ui| {
            for (topic, label) in HelpTopic::ALL {
                ui.selectable_value(
                    &mut self.help_topic,
                    topic,
                    crate::localization::text(label),
                );
            }
        });
        ui.add_space(12.0);
        let width = ui.available_width().min(860.0);
        ui.scope(|ui| {
            ui.set_max_width(width);
            match self.help_topic {
                HelpTopic::Start => self.help_start(ui),
                HelpTopic::Editing => self.help_editing(ui),
                HelpTopic::Backups => self.help_backups(ui),
                HelpTopic::Assets => self.help_assets(ui),
                HelpTopic::Troubleshooting => self.help_troubleshooting(ui),
                HelpTopic::About => self.help_about(ui),
            }
        });
    }
    fn help_start(&mut self, ui: &mut egui::Ui) {
        section(ui, crate::localization::text("Connect your GP-200"));
        ui.label(crate::localization::text("1. On the pedal, open GLOBAL > USB Audio, go to the second page and set USB Mode to Normal."));
        ui.label(crate::localization::text("2. Connect a USB data cable. Linux exposes the pedal through ALSA MIDI; an audio connection alone does not confirm the editor connection."));
        ui.label(crate::localization::text("3. Choose Connect pedal in the toolbar. Select the matching input/output pair when prompted. Use one editor session at a time."));
        ui.label(crate::localization::text("4. Wait for Ready and the current preset read. The toolbar identifies a simulator explicitly when you launch the demo."));
        section(ui, crate::localization::text("Work offline too"));
        ui.label(crate::localization::text("The initial preset is a retained capture, opened as an offline document. It is separate from the preset currently on your pedal. You can inspect effects, make local edits, import presets, maintain your library and prepare assets without connecting."));
        self.help_navigation(
            ui,
            &[
                (ViewId::Library, crate::localization::text("Open Library")),
                (
                    ViewId::AppSettings,
                    crate::localization::text("Appearance and folders"),
                ),
            ],
        );
        section(ui, crate::localization::text("Start with a backup"));
        ui.label(crate::localization::text("Library > Device banks captures all 256 preset slots, the current buffer, settings and supported asset metadata. Check the completeness report before using it for recovery: sound contents for IR and SnapTone slots are not captured yet."));
        self.official_resources(ui);
    }
    fn help_editing(&mut self, ui: &mut egui::Ui) {
        section(ui, crate::localization::text("Edit, capture and save"));
        ui.label(crate::localization::text("Select a module in the Editor signal chain to view its controls. The model picker lists the recovered pedal effect catalogue. Factory effects run on the pedal; this editor does not emulate their DSP on your computer."));
        ui.label(crate::localization::text("A local draft, an acknowledged edit and a stored pedal preset have separate states. Hardware edits need a fresh readback. If an operation fails or becomes uncertain, read the pedal again before retrying."));
        ui.label(crate::localization::text("Capture to library keeps the selected draft as a local revision. Save to pedal uses the reviewed target slot and name. Previewing a stored slot reads it; auditioning selects it on the pedal."));
        section(ui, crate::localization::text("Undo and compare"));
        ui.label(crate::localization::text("Ctrl+Z undoes an edit; Ctrl+Shift+Z redoes it. Verified hardware undo also waits for readback. Compare captures preserve their exact values for review. Check the target before recalling an audition or writing a stored slot."));
        section(ui, crate::localization::text("Changing documents"));
        ui.label(crate::localization::text("When a draft is dirty, document navigation asks you to capture it, discard it or cancel the switch. Disconnects keep the draft in this running session. Reconnecting uses fresh device state and does not automatically replay old writes."));
        self.help_navigation(
            ui,
            &[
                (ViewId::Editor, crate::localization::text("Open Editor")),
                (ViewId::Library, crate::localization::text("Open Library")),
            ],
        );
    }
    fn help_backups(&mut self, ui: &mut egui::Ui) {
        section(ui, crate::localization::text("Two kinds of archive"));
        ui.label(crate::localization::text("A .gp200bundle stores local library revisions and included registered assets. A .gp200backup stores a device acquisition: preset slots, the current buffer, global settings and separately reported asset records. A .prst is a native GP-200 preset file."));
        section(
            ui,
            crate::localization::text("Check the report before recovery"),
        );
        ui.label(crate::localization::text("Device bank captures retain each completed read durably before requesting the next one. A cancelled or interrupted capture keeps those records and reports what is missing. Export the archive from Library > Device banks after reviewing its completeness."));
        ui.label(crate::localization::text("IR metadata describes names and checksums; it does not contain the impulse response. SnapTone metadata and all asset sound contents may be missing. A bank archive with those gaps cannot restore the whole pedal's sound assets."));
        section(ui, crate::localization::text("Restore deliberately"));
        ui.label(crate::localization::text("Import an archive, choose the target preset slots and inspect differences. Stored-preset restore captures a durable before-image and checks the target again before writing. Uncertain results pause the queue for reconciliation. Asset replacement and firmware recovery are unavailable."));
        ui.label(crate::localization::text("Library history and trash allow local recovery. Export a portable copy to another folder for protection against disk failure; resetting application folder preferences does not remove any library or archive."));
        self.help_navigation(
            ui,
            &[(ViewId::Library, crate::localization::text("Open Library"))],
        );
    }
    fn help_assets(&mut self, ui: &mut egui::Ui) {
        section(ui, crate::localization::text("Impulse responses"));
        ui.label(crate::localization::text("Select a WAV in Assets for inspection. The exact local IR preparation path accepts mono PCM24 at 44.1 kHz and prepares 1,024 samples. Longer input needs an explicit truncation choice. Other formats need separately verified processing."));
        section(
            ui,
            crate::localization::text("NAM renders and independent CLO calibration"),
        );
        ui.label(crate::localization::text("Select a NAM model and an input WAV. Supported model profiles include LSTM, WaveNet, A2, Sequential, supported containers, ConvNet, Linear and Slimmable WaveNet. Inspection reports the actual schema and any unsupported features."));
        ui.label(crate::localization::text("Ordinary rendering produces a retained WAV. Review the estimated operation count; the default budget is two billion operations. Larger jobs need an explicit higher limit. Rendered audio is separate from a SnapTone coefficient file."));
        ui.label(crate::localization::text("Experimental CLO calibration needs the original GP-200 Windows editor 1.8.1 calibration stimulus, selected explicitly. The worker checks its exact hash and decoded channel, renders the NAM response and fits an independent coefficient recipe. Its report describes the local fit; original converter equivalence and pedal sound acceptance remain unverified."));
        ui.label(crate::localization::text("Choose Export retained result to write the existing WAV or CLO to a selected folder. Export verifies the retained bytes and does not rerun conversion. Cancellation targets the selected job, including during coefficient fitting."));
        section(ui, crate::localization::text("Device transfers"));
        ui.label(crate::localization::text("IR and SnapTone uploads, erasure and firmware flashing remain disabled while transfer acknowledgments, complete before-images and recovery behavior are unresolved. Local inspection and conversion do not upload anything."));
        self.help_navigation(
            ui,
            &[(ViewId::Assets, crate::localization::text("Open Assets"))],
        );
    }
    fn help_troubleshooting(&mut self, ui: &mut egui::Ui) {
        section(
            ui,
            crate::localization::text("No pedal or no identity reply"),
        );
        ui.label(crate::localization::text("Check Normal USB mode, a data-capable cable and the matching MIDI input/output pair. Close other editors or MIDI probes. Audio playback working does not establish a functioning SysEx editor session. Disconnect and reconnect after those checks."));
        section(
            ui,
            crate::localization::text("Disconnected or uncertain edit"),
        );
        ui.label(crate::localization::text("Keep the app open to retain the current draft. Reconnect, read the current preset and review the difference. Do not infer that an edit succeeded from a button click or a missing error. Diagnostics retains the operation result and transport messages."));
        section(ui, crate::localization::text("Missing files or folders"));
        ui.label(crate::localization::text("A remembered folder that was removed or unmounted falls back to the desktop's default. Settings lets you forget individual folders or all remembered folders. A damaged preferences file is kept and defaults are used; the library and assets have their own stores."));
        section(ui, crate::localization::text("A feature is unavailable"));
        ui.label(crate::localization::text("A disabled operation shows its missing validation or protocol support. A zeroed metadata record does not establish an empty asset slot. The connected firmware version is not yet identified by the normal-mode protocol; check the pedal's System/About screen when comparing official documentation."));
        self.help_navigation(
            ui,
            &[
                (
                    ViewId::Diagnostics,
                    crate::localization::text("Open Diagnostics"),
                ),
                (
                    ViewId::AppSettings,
                    crate::localization::text("Open Settings"),
                ),
            ],
        );
        self.official_resources(ui);
    }
    fn help_about(&mut self, ui: &mut egui::Ui) {
        section(ui, crate::localization::text("GP200 Linux"));
        ui.label(RichText::new(std::format!("Version {}", env!("CARGO_PKG_VERSION"))).strong());
        ui.label(std::format!(
            "Native Rust / egui 0.36.2 · {} / {} · {} build",
            std::env::consts::OS,
            std::env::consts::ARCH,
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "optimized"
            }
        ));
        ui.small(crate::localization::text(
            "Independent Linux editor. GP200 Linux is not a Valeton product.",
        ));
        section(ui, crate::localization::text("Current session"));
        if let Some(identity) = &self.controller.state().identity {
            ui.label(format!(
                "{} · {:?} · {:?}",
                identity.model.name(),
                identity.mode,
                self.controller.state().connection
            ));
            ui.label(firmware_label(Some(identity)));
        } else {
            ui.label(crate::localization::text(
                "Offline document; no active pedal identity.",
            ));
            ui.label(firmware_label(None));
        }
        section(ui, crate::localization::text("Compatibility"));
        ui.label(crate::localization::text("Recovered from the GP-200 Windows editor 1.8.1. Physical protocol profiles were checked on a connected GP-200 in Normal mode. GP-200LT and GP-200JR need separate physical validation. Supported operations depend on the observed session and their own readback profiles."));
        ui.small(crate::localization::text("ALSA MIDI transport; OpenGL renderer via eframe/glow. Independent calibration helper availability is specific to this build."));
        ui.label(if gp200_assets::calibration_helper_available() {
            crate::localization::text("Independent CLO helper: included")
        } else {
            crate::localization::text("Independent CLO helper: unavailable for this build target")
        });
        egui::CollapsingHeader::new(crate::localization::text("Linux application changes")).default_open(true).show(ui, |ui| {
            ui.label(crate::localization::text("0.1.0 development: native preset editor and catalogue, exact local revisions and comparisons, durable device bank acquisition, offline file inspection and NAM processing, explicit hardware profiles, persistent dark/light/system themes and help."));
            ui.small(crate::localization::text("Original-app parity and integrated release acceptance remain in progress. No automatic update check is enabled."));
        });
        egui::CollapsingHeader::new(crate::localization::text("Original editor baseline notes")).show(ui, |ui| {
            ui.label(crate::localization::text("The supplied vendor release note identifies GP-200 Software 1.8.1, dated 27 July 2026, and adds NAM A2 file loading. This is the Windows editor version; it does not identify the connected pedal's firmware."));
            ui.small(crate::localization::text("Summary of the supplied GP-200 Software Release Note.rtf. Official downloads provide vendor material separately from Linux application changes."));
        });
        egui::CollapsingHeader::new(crate::localization::text("Licensing and notices")).show(ui, |ui| {
            ui.label(crate::localization::text("Application source: MIT. Cantarell: SIL Open Font License. Default egui font notices accompany the dependency licenses. NAM Core, NamtoClo and r8brain components: MIT. Ooura FFT retains its permissive notice."));
            ui.small(crate::localization::text("The portable package includes LICENSE-app.txt, a licenses folder and dependency-licenses.json. In the source checkout, font notices are in assets/ui/fonts and conversion notices are in assets/licenses."));
            egui::CollapsingHeader::new(crate::localization::text("Application license")).show(ui, |ui| { ui.small(include_str!("../../../../packaging/LICENSE-app.txt")); });
        });
        self.official_resources(ui);
    }
    fn help_navigation(&mut self, ui: &mut egui::Ui, destinations: &[(ViewId, &str)]) {
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            for (view, label) in destinations {
                if ui.button(crate::localization::text(label)).clicked() {
                    self.send(UiIntent::Navigate(*view));
                }
            }
        });
    }
    fn official_resources(&mut self, ui: &mut egui::Ui) {
        section(ui, crate::localization::text("Official resources"));
        ui.small(crate::localization::text("These buttons open your browser on an explicit click. They do not download files or change the pedal. Choose documentation for your variant and firmware."));
        ui.horizontal_wrapped(|ui| {
            for link in OfficialLink::ALL {
                if ui.button(link.label()).on_hover_text(link.url()).clicked() {
                    self.services.open_official_link(link);
                }
            }
        });
        if let Some(notice) = &self.services.last_link_notice {
            ui.colored_label(Palette::for_context(ui.ctx()).muted, notice);
        }
    }
}

fn firmware_label(identity: Option<&DeviceIdentity>) -> String {
    match identity {
        Some(identity) if identity.mode == BackendMode::Hardware => identity
            .firmware
            .as_ref()
            .map(|firmware| std::format!("Observed pedal firmware: {}", firmware.version))
            .unwrap_or_else(|| {
                crate::localization::text(
                    "Pedal firmware: unknown; check the pedal's System/About screen.",
                )
                .into()
            }),
        Some(_) => crate::localization::text(
            "Pedal firmware: not established by this simulator or capture session.",
        )
        .into(),
        None => crate::localization::text("Pedal firmware: unknown while offline.").into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn firmware_never_comes_from_editor_or_simulator_version() {
        let mut identity = DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Simulator,
            firmware: Some(FirmwareIdentity {
                version: "invented-simulator-value".into(),
                raw: vec![],
                evidence: vec![],
            }),
            display_name: "Test".into(),
        };
        assert!(!firmware_label(Some(&identity)).contains("invented"));
        identity.mode = BackendMode::Hardware;
        identity.firmware = None;
        assert!(firmware_label(Some(&identity)).contains("unknown"));
        identity.firmware = Some(FirmwareIdentity {
            version: "observed-version".into(),
            raw: vec![1],
            evidence: vec![],
        });
        assert!(firmware_label(Some(&identity)).contains("observed-version"));
        assert!(firmware_label(None).contains("offline"));
    }
    #[test]
    fn official_resources_have_fixed_https_valeton_destinations() {
        for link in OfficialLink::ALL {
            assert!(link.url().starts_with("https://www.valeton.net/"));
            assert!(!link.label().is_empty());
        }
        assert!(OfficialLink::Manual.url().contains("firmware-v1-8-0"));
        assert!(HelpTopic::parse("download-everything").is_none());
    }
}
