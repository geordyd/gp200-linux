# Changelog

Changes to GP200 Linux are recorded here.

## 0.1.1 — 2026-10-14 (preview)

- Fix pedal connection after reboot on filesystems such as Btrfs whose device numbers can change. Completed firmware recovery history now checks retained files against the currently opened registry filesystem, keeping inode, content, journal and complete-set validation. Live recovery identity checks and unresolved-attempt blocking remain enforced.

## 0.1.0 — 2026-10-14 (preview)

- Native Rust/egui Linux editor with an English interface, dark/light/system themes and adjustable scale.
- Lossless preset drafts, Undo/Redo, A/B comparison, preset diffs, selected settings merge and local draft recovery.
- Revisioned library, native `.prst` import/export, application bundles, device-bank archives and local setlists.
- Explicit USB-MIDI connection, read-only device queries and scoped live preset editing workflows.
- Expression, CTRL, quick-access, FX-loop, global settings and drum-pattern interfaces.
- Local WAV/IR preparation, supported NAM rendering and experimental independent NAM-to-CLO calibration.
- Headless CLI for inspection, local conversion, library/archive work, read-only queries and opt-in support archives.
- Experimental Linux firmware workflow, including a completed GP-200 V1.6.3 → V1.8.2 full-image recovery. Ordinary Linux builds include the updater and its embedded support materials; device operations remain explicitly reviewed.
- AppImage and portable archive release tooling.

Full official-editor parity, complete sound backup/restore, physical IR/SnapTone transfer, useful CLO tonal quality and broader device/version coverage remain open.
