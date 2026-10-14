# GP200 Linux user guide

Build and launch instructions are in the [README](../README.md). Run `cargo run --locked -p gp200-gui -- --help` or `cargo run --locked -p gp200-cli -- --help` for the current command options.

## Start offline

The GUI starts in an offline workspace. You can inspect and edit local presets without connecting a pedal. For a labelled simulator session:

```sh
cargo run --locked -p gp200-gui -- --demo --theme dark
```

Settings retains the dark, light or system theme and display scale. `--theme light` selects the light theme; `--scale 1.5` sets the scale for a launch. The interface is English.

## Connect a GP-200

Connect the pedal by USB in normal mode, close other editors and choose **Connect**. The application identifies the pedal and reads its current buffer. You can also request connection explicitly at launch:

```sh
cargo run --locked -p gp200-gui -- --connect
```

Use one editor or CLI device session at a time. A busy or ambiguous device result needs an explicit choice or reconnect; the application does not bypass another session's ownership. GP-200LT, GP-200JR and two identical connected pedals are not verified configurations.

After a disconnected or uncertain write, reconnect and read the device before deciding what to do next. Do not assume a failed confirmation means that a save never reached the pedal.

## Edit and keep presets

Select an effect module to edit its controls. Numeric text commits on Enter or when focus leaves the field; Escape cancels it. Undo/Redo and A/B help compare changes. Unknown preset records are retained rather than discarded.

The Library keeps immutable revisions and imports/exports native `.prst` files. **Merge selected settings** reviews settings from a chosen revision before applying them to an offline document as one Undo step. Unselected settings and unknown records are preserved. Check the target slot and review the operation before saving to a connected pedal.

Application bundles and device-bank archives use project-specific formats. A local round trip does not guarantee every official-editor file mode or embedded-asset relationship is supported. Keep original files when trying an import.

Crash recovery opens recovered drafts as new offline documents. It does not repeat pending pedal operations.

## Read and archive a bank

This explicit read-only acquisition saves presets, current buffer, globals and supported asset metadata into the local library:

```sh
cargo run --locked -p gp200-gui -- --connect --capture-device-backup --view library
```

Missing IR audio and SnapTone contents remain recorded as missing. **This is not a complete sound backup**, and it cannot promise that every installed sound can be restored.

The CLI also offers explicit read-only commands:

```sh
cargo run --locked -p gp200-cli -- device ports
cargo run --locked -p gp200-cli -- device current --unique
cargo run --locked -p gp200-cli -- device globals --unique
```

`--unique` refuses zero or multiple GP-200 candidates. Local inspection, conversion, library/archive and support commands do not require a device. Consult `--help` for exact file and command options.

## Assets and firmware

Assets supports local WAV/IR preparation and NAM rendering for supported profiles. NAM-to-CLO calibration is experimental: a generated container does not establish good guitar tone or a verified pedal upload. Physical IR/SnapTone transfer and complete content recovery remain unfinished.

The experimental Linux updater is included in ordinary builds. It successfully recovered one GP-200 from V1.6.3 to V1.8.2 after interrupted attempts. Each physical operation requires the application’s explicit review; the supported official firmware file must be supplied by the user. See [firmware instructions](firmware.md). That result does not establish data preservation, every firmware version or other pedal variants.

## Data and troubleshooting

For compatibility with existing installations, configuration remains under `$XDG_CONFIG_HOME/gp200-studio` (default `~/.config/gp200-studio`). Library, assets and journals remain under `$XDG_DATA_HOME/gp200-studio` (default `~/.local/share/gp200-studio`). The public application name is GP200 Linux.

If a file dialog fails, check that `xdg-desktop-portal` and a backend for your desktop are running. If connection fails, confirm normal USB mode and that another editor has not opened the MIDI endpoint. Use the Diagnostics view or the CLI's `device doctor --unique` for a read-only connection check.

Support archives are created locally and are not sent automatically. Review their contents before sharing, especially if you explicitly included presets, MIDI or audio. See [SECURITY.md](../SECURITY.md) before posting sensitive material.
