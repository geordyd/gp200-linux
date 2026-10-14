# Contributing to GP200 Linux

Bug reports, documentation, UI improvements and focused code changes are welcome. For a substantial feature or protocol change, open an issue describing the intended behavior and how it can be verified before investing in a large patch.

## Development

Follow the [README](README.md) for build dependencies. Rustup selects the pinned toolchain. Use the simulator for development that does not require a pedal:

```sh
cargo run --locked -p gp200-gui -- --demo
```

Before submitting code, run the relevant focused checks and the standard workspace checks:

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
```

Keep changes small enough to review. Describe the problem, resulting behavior, checks performed and remaining limitations in your pull request. Use Conventional Commits, such as `fix(editor): preserve unknown preset records` or `docs: clarify backup limits`. Read [`AGENTS.md`](AGENTS.md) when using a coding agent.

The Cargo executables remain `gp200-gui` and `gp200-cli`. Existing `gp200-studio` configuration and data paths are retained for compatibility; do not rename or migrate user data as a cosmetic change.

## Device and protocol changes

Preserve unknown fields, record order and raw floating-point values when editing or exporting presets. Keep the GUI separate from physical MIDI ownership: device commands belong to the transport actor, not to individual widgets. Treat uncertain writes as uncertain; do not automatically resend them.

Synthetic fixtures, simulator tests and capture replay establish software behavior. They do not establish hardware behavior. State which kind of evidence a test uses, and identify the exact pedal model and firmware for hardware results. Use synthetic or clearly redistributable fixtures in public tests. Do not commit personal preset banks, raw USB captures, vendor programs or firmware images.

Hardware tests are manual and explicitly chosen. Do not add automatic pedal discovery, writes or firmware operations to tests or CI. Before a device mutation, record the starting state, use a disposable scratch slot where appropriate and verify readback and restoration. Share a minimal redacted result rather than private captures.

Firmware work can leave a pedal in update mode and can lose installed data. Discuss a concrete validation and recovery plan before enabling a new transfer path. A successful simulator run or previous recovery does not authorize a new physical flash or prove support for another firmware or pedal variant.

## Licensing and reports

By contributing, you agree that your project-authored contributions are available under the [MIT License](LICENSE). Preserve third-party license notices and document the origin and redistribution terms of new assets or dependencies. Do not copy vendor descriptions or artwork into the project.

For an ordinary bug report, include the application version, Linux distribution and desktop, reproduction steps, expected result and actual result. Include pedal model/firmware only when relevant. Inspect screenshots and logs for personal paths, preset names or device identifiers before posting. Follow [SECURITY.md](SECURITY.md) for vulnerabilities.
