//! Local archive inspection/export only. This crate has no MIDI service.
use gp200_library::LibraryStore;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let source = PathBuf::from(
        args.next()
            .ok_or("Usage: backup_archive_probe STORE DEST.gp200backup VERIFY_STORE")?,
    );
    let destination = PathBuf::from(args.next().ok_or("Missing destination")?);
    let verify = PathBuf::from(args.next().ok_or("Missing fresh verification store")?);
    if args.next().is_some() || destination.exists() || verify.exists() {
        return Err("Require exact arguments and fresh destination/verification store".into());
    }
    let store = LibraryStore::open(&source)?;
    let backups = store.device_backups()?;
    if backups.len() != 1 {
        return Err("Require exactly one source acquisition".into());
    }
    let original = &backups[0];
    let completeness = store.export_device_backup(&original.id, &destination)?;
    let mut imported = LibraryStore::open(&verify)?;
    let restored = imported.import_device_backup(&destination)?;
    // Import allocates an ID but preserves every captured descriptor/hash.
    if original.presets != restored.presets
        || original.edit_buffer != restored.edit_buffer
        || original.globals != restored.globals
        || original.assets != restored.assets
        || original.read_errors != restored.read_errors
        || original.acquisition != restored.acquisition
    {
        return Err("Imported archive differs from captured acquisition".into());
    }
    println!(
        "{}",
        serde_json::json!({
            "stored_presets": restored.presets.len(),
            "current_captured": restored.edit_buffer.is_some(),
            "globals_captured": restored.globals.is_some(),
            "asset_metadata": restored.assets.len(),
            "completeness": completeness,
            "roundtrip_descriptors_identical": true,
            "midi_operations": 0,
        })
    );
    Ok(())
}
