//! Build an explicitly offline UI fixture. No MIDI service is constructed.
use gp200_domain::*;
use gp200_library::*;
use gp200_protocol::{RawRecord, RawTransfer, full_stored_write_request};
use std::{collections::BTreeMap, sync::Arc};
fn fixture(target: PresetTarget, revision: u64) -> Arc<PresetSnapshot> {
    let captured = gp200_sim::captured_demo_snapshot();
    let raw = match target {
        PresetTarget::EditBuffer => captured.raw.as_ref().clone(),
        PresetTarget::Stored(slot) => {
            let wire = full_stored_write_request(slot.get(), &captured.raw).unwrap();
            let mut records = vec![wire.records()[0].clone()];
            records.extend_from_slice(&wire.records()[2..]);
            RawTransfer::from_records(0x12, &records).unwrap()
        }
    };
    Arc::new(PresetSnapshot::from_raw(
        Arc::new(raw),
        SnapshotSource::Simulator,
        Some(target),
        Revision(revision),
    ))
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("Usage: workflow_fixture ABSOLUTE_LIBRARY_PATH")?;
    let mut store = LibraryStore::open(path)?;
    let origin = BackupOrigin {
        identity: DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Simulator,
            firmware: None,
            display_name: "Offline UI fixture".into(),
        },
        epoch: SessionEpoch(1),
        device_key: "offline-qa-fixture".into(),
        evidence: vec![EvidenceRef::new(
            "assets/demo/read-current-payload.bin",
            EvidenceLevel::Proposed,
        )],
    };
    let capture = store.begin_device_backup(origin.clone())?;
    store.capture_backup_preset(&capture.id, fixture(PresetTarget::EditBuffer, 1))?;
    for n in [0, 1, 2, 23, 254, 255] {
        store.capture_backup_preset(
            &capture.id,
            fixture(PresetTarget::Stored(SlotId::new(n)?), u64::from(n) + 2),
        )?;
    }
    let capture = store.finish_device_backup(&capture.id, BackupAcquisition::Cancelled)?;
    let version = store.store_snapshot(
        fixture(PresetTarget::EditBuffer, 1),
        LibraryMetadata {
            name: "Match clean / church".into(),
            notes: "Offline QA capture; source floats and assignments retained".into(),
            ..Default::default()
        },
        None,
    )?;
    let mut changed = fixture(PresetTarget::EditBuffer, 2).raw.records().to_vec();
    let record = changed
        .iter_mut()
        .find(|r| r.command == 0x14 && r.body[0] == 3)
        .unwrap();
    record.body[8..12].copy_from_slice(&63.25_f32.to_le_bytes());
    // A separate revision with a real supported parameter difference and an opaque byte difference.
    changed.push(RawRecord {
        command: 0x7abc,
        body: vec![0xde, 0xad, 0x01],
    });
    let raw = RawTransfer::from_records(0x12, &changed)?;
    let second = Arc::new(PresetSnapshot::from_raw(
        Arc::new(raw),
        SnapshotSource::Simulator,
        Some(PresetTarget::EditBuffer),
        Revision(2),
    ));
    let _second = store.store_snapshot(
        second,
        LibraryMetadata {
            name: "Match clean / solo".into(),
            ..Default::default()
        },
        Some(version.clone()),
    )?;
    let mut list = store.create_setlist("Friday rehearsal")?;
    for (song, section, notes, tempo, slot) in [
        (
            "Northern lights",
            "Intro / verse",
            "Clean opening. Bring in the church reverb for the final chord.",
            112,
            23,
        ),
        (
            "Wide open",
            "Chorus",
            "Keep the clean sound pinned. The pedal slot is a local proposal.",
            124,
            254,
        ),
        (
            "After the rain",
            "Solo",
            "Use the level control after unlocking the stage controls.",
            96,
            255,
        ),
    ] {
        let mut entry = SetlistEntry::new(version.clone(), song)?;
        entry.section = section.into();
        entry.notes = notes.into();
        entry.tempo_fallback = Some(tempo);
        entry.proposed_slot = Some(SlotId::new(slot)?);
        list.entries.push(entry);
    }
    let list = store.save_setlist(list, Revision(0))?;
    let target = SlotId::new(254)?;
    let plan = store.plan_restore(&RestoreRequest {
        target: origin,
        current_buffer: fixture(PresetTarget::EditBuffer, 1),
        selections: vec![RestoreSelection {
            source: RestoreSource::BackupStored {
                backup: capture.id.clone(),
                slot: SlotId::new(1)?,
            },
            target,
        }],
        observed_targets: BTreeMap::from([(target, fixture(PresetTarget::Stored(target), 1))]),
        asset_bindings: vec![],
        target_assets: vec![],
    })?;
    store.begin_restore(
        plan.clone(),
        RestoreReview {
            plan_hash: plan.review_hash()?,
            accept_unknown_firmware: true,
        },
    )?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"provenance":"Offline simulator fixture; no MIDI opened","backup":capture.id,"setlist":list.id,"stored_presets":capture.presets.len(),"missing_asset_content":capture.completeness().missing_asset_content.len()})
        )?
    );
    Ok(())
}
