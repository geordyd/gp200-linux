# Real hardware wire fixtures

These are unmodified copies of the manifest's source captures. The SHA-256,
executable identity and observation scope are recorded in `manifest.json`.
Firmware version remains unknown. They are not simulator data.

Original current and stored-slot-23 replies contain seven 185/66-byte decoded
chunks and 1176-byte payloads. Their contexts differ (`[0,23]` versus `[1,23]`).
The new identity capture contains a separate unsolicited command-5 transfer
before command 0x2001; it tests response matching, not outgoing write semantics.

The edit/restore request fixtures are from the controlled Match CL Gain
50→51→50 experiment, with XML Index 13, command-5 zero padding and fresh context
`[0,23]`. These do not validate other controls, conversions or stored writes.
