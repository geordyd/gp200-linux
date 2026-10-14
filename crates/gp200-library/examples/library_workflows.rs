//! Offline workflow adapter. This executable has no MIDI/transport dependency.
use gp200_domain::*;
use gp200_library::*;
use gp200_protocol::{RawTransfer, full_stored_write_request};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

fn fixture(
    target: PresetTarget,
    revision: u64,
) -> Result<Arc<PresetSnapshot>, Box<dyn std::error::Error>> {
    let current = RawTransfer::from_payload(
        0x12,
        include_bytes!("../../../assets/demo/read-current-payload.bin").to_vec(),
    )?;
    let raw = match target {
        PresetTarget::EditBuffer => current,
        PresetTarget::Stored(slot) => {
            let wire = full_stored_write_request(slot.get(), &current)?;
            let mut records = vec![wire.records()[0].clone()];
            records.extend_from_slice(&wire.records()[2..]);
            RawTransfer::from_records(0x12, &records)?
        }
    };
    Ok(Arc::new(PresetSnapshot::from_raw(
        Arc::new(raw),
        SnapshotSource::Simulator,
        Some(target),
        Revision(revision),
    )))
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments.len() < 2 {
        return Err("Usage: library_workflows <demo|inspect|export-backup|import-backup> <absolute-store-directory> [backup-id/archive-path] [archive-path]".into());
    }
    let root = PathBuf::from(&arguments[1]);
    if !root.is_absolute() {
        return Err("Use an absolute store directory".into());
    }
    let mut store = LibraryStore::open(root)?;
    match arguments[0].as_str() {
        "demo" => {
            let origin = BackupOrigin {
                identity: DeviceIdentity {
                    model: DeviceModel::Gp200,
                    mode: BackendMode::Simulator,
                    firmware: None,
                    display_name: "Offline fixture simulator".into(),
                },
                epoch: SessionEpoch(1),
                device_key: "offline-example-fixture".into(),
                evidence: vec![EvidenceRef::new(
                    "assets/demo/read-current-payload.bin; synthetic stored addressing",
                    EvidenceLevel::Proposed,
                )],
            };
            let backup = store.begin_device_backup(origin.clone())?;
            store.capture_backup_preset(&backup.id, fixture(PresetTarget::EditBuffer, 1)?)?;
            for index in 0..3 {
                store.capture_backup_preset(
                    &backup.id,
                    fixture(
                        PresetTarget::Stored(SlotId::new(index)?),
                        u64::from(index) + 1,
                    )?,
                )?;
            }
            let backup = store.finish_device_backup(&backup.id, BackupAcquisition::Cancelled)?;
            let version = store.store_snapshot(
                fixture(PresetTarget::EditBuffer, 1)?,
                LibraryMetadata {
                    name: "Offline workflow example".into(),
                    notes: "Simulator fixture only; no hardware was accessed".into(),
                    ..Default::default()
                },
                None,
            )?;
            let mut setlist = store.create_setlist("Rehearsal example")?;
            let mut song = SetlistEntry::new(version, "Opening song")?;
            song.section = "Intro / verse".into();
            song.notes = "Pinned exact revision; this is only a local slot proposal".into();
            song.tempo_fallback = Some(120);
            song.proposed_slot = Some(SlotId::new(254)?);
            setlist.entries.push(song);
            let setlist = store.save_setlist(setlist, Revision(0))?;
            let target = SlotId::new(254)?;
            let plan = store.plan_restore(&RestoreRequest {
                target: origin,
                current_buffer: fixture(PresetTarget::EditBuffer, 1)?,
                selections: vec![RestoreSelection {
                    source: RestoreSource::BackupStored {
                        backup: backup.id.clone(),
                        slot: SlotId::new(1)?,
                    },
                    target,
                }],
                observed_targets: BTreeMap::from([(
                    target,
                    fixture(PresetTarget::Stored(target), 1)?,
                )]),
                asset_bindings: vec![],
                target_assets: vec![],
            })?;
            let review = RestoreReview {
                plan_hash: plan.review_hash()?,
                accept_unknown_firmware: false,
            };
            let journal = store.begin_restore(plan, review)?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"mode":"offline simulator fixture; no MIDI opened","backup":backup,"completeness":backup.completeness(),"setlist":setlist,"reviewed_plan_journal":journal})
                )?
            );
        }
        "inspect" => println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"backups":store.device_backups()?,"setlists":store.setlists(true)?,"restore_journals":store.restore_journals()?})
            )?
        ),
        "export-backup" if arguments.len() == 4 => println!(
            "{}",
            serde_json::to_string_pretty(
                &store.export_device_backup(&arguments[2], &arguments[3])?
            )?
        ),
        "import-backup" if arguments.len() == 3 => println!(
            "{}",
            serde_json::to_string_pretty(&store.import_device_backup(&arguments[2])?)?
        ),
        _ => return Err("Invalid action/arguments".into()),
    }
    Ok(())
}
