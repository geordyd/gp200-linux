//! Populate a fresh, isolated library for native README screenshots.
//! Synthetic names and local metadata; no MIDI service or physical device.
use gp200_domain::*;
use gp200_library::LibraryStore;
use std::{path::PathBuf, sync::Arc};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("Usage: readme_library_fixture ABSOLUTE_EMPTY_LIBRARY_PATH")?,
    );
    if !root.is_absolute() || root.exists() {
        return Err("Select a fresh absolute library path".into());
    }
    let mut store = LibraryStore::open(&root)?;
    let baseline = gp200_sim::public_demo_snapshot();
    for (name, style, tags, favorite) in [
        (
            "Clean Shimmer",
            "Ambient",
            vec!["clean", "ambient", "demo"],
            true,
        ),
        ("Singing Lead", "Rock", vec!["lead", "rock", "demo"], true),
        (
            "Warm Rhythm",
            "Blues",
            vec!["rhythm", "blues", "demo"],
            false,
        ),
    ] {
        let mut draft = PresetDraft::from_snapshot(&baseline);
        draft.apply(&EditPatch::SetMetadata(MetadataPatch {
            name: Some(name.into()),
            ..Default::default()
        }))?;
        let snapshot =
            Arc::new(draft.snapshot(&baseline, SnapshotSource::Simulator, Revision(0))?);
        store.store_snapshot(
            snapshot,
            LibraryMetadata {
                name: name.into(),
                author: "GP200 Linux demo".into(),
                style: style.into(),
                notes: "Synthetic screenshot example based on the retained simulator configuration; sound is not independently verified.".into(),
                tags: tags.into_iter().map(str::to_owned).collect(),
                favorite,
            },
            None,
        )?;
    }
    println!("Created 3 synthetic presets; no hardware access");
    Ok(())
}
