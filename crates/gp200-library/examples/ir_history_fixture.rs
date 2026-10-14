//! Synthetic local example for isolated native captures. It creates synthetic local evidence only.
#[path = "../tests/common/ir_history_fixture.rs"]
mod fixture;
use fixture::*;
use gp200_library::*;
use std::{fs, path::PathBuf};
fn main() {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().expect("fresh absolute local library directory"));
    let mode = args
        .next()
        .expect("empty|list|complete|corrupt-tail|unreadable");
    assert!(args.next().is_none());
    assert!(
        root.is_absolute() && !root.exists(),
        "Fixture directory must be fresh and absolute"
    );
    let mut store = LibraryStore::open(&root).unwrap();
    let _ = IrAttemptLedger::open(&mut store).unwrap();
    let count = if mode == "empty" {
        0
    } else if mode == "list" {
        9
    } else {
        1
    };
    let mut ids = vec![];
    for _ in 0..count {
        ids.push(populate(&mut store, true));
    }
    if let Some(id) = ids.first() {
        let journal = root
            .join("ir-attempt-ledger")
            .join(format!("{}.journal", id.as_str()));
        match mode.as_str() {
            "corrupt-tail" => {
                let mut raw = fs::read(&journal).unwrap();
                raw.extend_from_slice(b"retained bad tail");
                fs::write(journal, raw).unwrap();
            }
            "unreadable" => fs::write(journal, b"uninterpretable synthetic journal").unwrap(),
            "list" | "complete" => {}
            _ => panic!("Unknown synthetic fixture mode"),
        }
    } else {
        assert_eq!(mode, "empty");
    }
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "synthetic_fixture": true, "library_root": root, "mode": mode,
            "attempts": ids, "hardware_operations": 0, "new_human_write_authority": false,
        }))
        .unwrap()
    );
}
