use eframe::egui;
use gp200_library::{KnownChange, PresetDiff};

fn value<T: std::fmt::Display>(value: &Option<T>) -> String {
    value
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| crate::localization::text("Unknown").into())
}

fn chain(value: &Option<Vec<gp200_domain::ModuleId>>) -> String {
    value
        .as_ref()
        .map(|modules| {
            modules
                .iter()
                .map(|module| module.name())
                .collect::<Vec<_>>()
                .join(" · ")
        })
        .unwrap_or_else(|| crate::localization::text("Unknown").into())
}

pub(super) fn render(ui: &mut egui::Ui, diff: &PresetDiff) {
    ui.label(std::format!(
        "{} setting changes · {} raw record changes",
        diff.known.len(),
        diff.raw.len()
    ));
    if diff.transfer_kind_changed {
        ui.label(crate::localization::text("Transfer kind changed"));
    }
    for change in &diff.known {
        let label = match change {
            KnownChange::Name { before, after } => {
                let before = value(before);
                let after = value(after);
                std::format!("Name: {before} → {after}")
            }
            KnownChange::Tempo { before, after } => {
                let before = value(before);
                let after = value(after);
                std::format!("Tempo: {before} → {after} BPM")
            }
            KnownChange::Level { before, after } => {
                let before = value(before);
                let after = value(after);
                std::format!("Patch level: {before} → {after}")
            }
            KnownChange::Chain { before, after } => {
                let before = chain(before);
                let after = chain(after);
                std::format!("Signal chain: {before} → {after}")
            }
            KnownChange::ModuleAdded(m) => {
                std::format!("{} added", m.name())
            }
            KnownChange::ModuleRemoved(m) => {
                std::format!("{} removed", m.name())
            }
            KnownChange::Model {
                module,
                before,
                after,
                before_name,
                after_name,
            } => std::format!(
                "{} model: {} ({:08x}) → {} ({:08x})",
                module.name(),
                before_name
                    .as_deref()
                    .unwrap_or(crate::localization::text("Unknown")),
                before.0,
                after_name
                    .as_deref()
                    .unwrap_or(crate::localization::text("Unknown")),
                after.0
            ),
            KnownChange::State {
                module,
                before,
                after,
            } => std::format!("{} enable state: {before} → {after}", module.name()),
            KnownChange::Parameter(c) => std::format!(
                "{} {}: {} → {}",
                c.module.name(),
                crate::localization::text(&c.name),
                c.before.as_f32(),
                c.after.as_f32()
            ),
            KnownChange::LocalMetadata => {
                crate::localization::text("Local metadata changed").into()
            }
            KnownChange::Expressions => {
                crate::localization::text("Expression assignments changed").into()
            }
            KnownChange::Footswitches => {
                crate::localization::text("CTRL assignments changed").into()
            }
            KnownChange::QuickControls => {
                crate::localization::text("Quick controls changed").into()
            }
            KnownChange::FxLoopPositions => {
                crate::localization::text("FX-loop send/return positions changed").into()
            }
            KnownChange::Dependencies => {
                crate::localization::text("Asset references changed").into()
            }
        };
        let response = ui.label(label);
        if let KnownChange::Parameter(change) = change {
            response.on_hover_text(std::format!(
                "Exact parameter bits: {:08x} → {:08x}",
                change.before.0,
                change.after.0
            ));
        }
    }
    if diff.known.is_empty() {
        ui.small(crate::localization::text(
            "No changes to decoded settings. Opaque byte differences remain visible below.",
        ));
    }
    egui::CollapsingHeader::new(crate::localization::text(
        "Exact record bytes and unknown fields",
    ))
    .show(ui, |ui| {
        for change in &diff.raw {
            egui::CollapsingHeader::new(std::format!(
                "Record {} · {:04x?} → {:04x?} · {} changed bytes · {}",
                change.record_index,
                change.before_command,
                change.after_command,
                change.changed_offsets.len(),
                change.classification
            ))
            .id_salt(change.record_index)
            .show(ui, |ui| {
                ui.small(std::format!(
                    "Changed offsets: {:?}",
                    change.changed_offsets
                ));
                for (name, bytes) in [
                    (crate::localization::text("Before"), &change.before),
                    (crate::localization::text("After"), &change.after),
                ] {
                    ui.label(name);
                    match bytes {
                        None => {
                            ui.small(crate::localization::text("Record absent"));
                        }
                        Some(bytes) => {
                            for (chunk, data) in bytes.chunks(24).enumerate() {
                                ui.monospace(std::format!(
                                    "{:04x}: {}",
                                    chunk * 24,
                                    data.iter()
                                        .map(|byte| format!("{byte:02x}"))
                                        .collect::<Vec<_>>()
                                        .join(" ")
                                ));
                            }
                        }
                    }
                }
            });
        }
    });
}
