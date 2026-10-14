# Simulator and replay fixtures

`gp200-bank.bin` concatenates the 256 captured 1176-byte stored preset payloads, ordered by zero-based slot. `manifest.json` retains each original source path and SHA-256 plus the concatenated hash. These are actual pedal preset bytes. They contain no globals, user asset contents, firmware version or DSP code. Runtime simulation always labels its results `Simulator` regardless of the fixture's physical origin.

`device-cleared.payload.bin` is copied from the independently observed device clear result, with its source and hash in the manifest. Its name is `It's GP-200`, tempo 120 and level 50. It differs from the Windows host's initialization defaults.

`replay-identity-current.json` embeds the unchanged original incoming SysEx captures from the protocol fixtures, their expected outgoing requests, response SHA-256, provenance and an explicit synthetic-timing flag. The identity response includes a real unsolicited parameter notification before the identity record. No per-packet source timestamps were available, so zero timing does not claim physical timing accuracy.

`fragmented-notification-scenario.json` is a proposed fault scenario: current read, 185-byte reply chunks, seven-byte callbacks, synthetic latency, duplicated chunk, injected MIDI clock, and interleaved notification. Its notification timing/placement are synthetic coverage rather than a recorded live exchange.

Native export acceptance uses the independently generated fixtures under `../native/`; their names identify them as constructed files. No constructed file is described as an official vendor export.
