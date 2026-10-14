# GP-200 normal wire codec

`gp200-protocol` is a pure Rust codec. It does not open MIDI ports or decide
which hardware writes are authorized. `RawTransfer` and `F32Bits` preserve
record ordering, unknown bodies and all IEEE-754 bits.

Typical actor flow:

1. Use `identify_request()`, `read_current_request()` or
   `read_stored_request(slot)` and `encode_transfer()`. The sender emits complete
   183-byte decoded chunks; the actor applies its own measured pacing.
2. Pass MIDI callback bytes to `StreamDecoder::feed()`. Forward `Packet` events
   to `Reassembler::add(&packet, monotonic_elapsed)`. Log other MIDI bytes and
   foreign/short SysEx separately. Realtime bytes do not interrupt SysEx.
3. Match the completed `RawTransfer` to the requested command and immutable
   target context. `Identity::from_transfer()` searches command 0x2001 rather
   than assuming every completed transfer is the requested identity reply.
4. Call `expire(now)` periodically and surface its incomplete-transfer
   diagnostics. Reset streaming and reassembly state on a session change.

`Reassembler` rejects conflicting overlap, accepts identical duplicates and
out-of-order chunks, and retains one transfer bounded by the 14-bit length.
Its timeout is a hard deadline from the first chunk. `add()` reports expiry but
does not silently consume a new packet as a new transaction: call `expire()`
or `reset()` explicitly. An offset-zero chunk does not itself prove a fresh
transaction, and the wire header has no known transaction ID. Completed
transfers reset state, so a repeated complete reply must be handled by the
actor's request/session guards. A different complete unsolicited transfer
arriving while another transfer is incomplete requires actor routing or
diagnostics; the codec never silently replaces the incomplete transfer.

Normal parsing accepts the observed 185-byte pedal chunks; 183 is only the TX
profile. Firmware signatures, short legacy address messages and foreign
SysEx remain separate `UnrecognizedSysex` events. No firmware protocol is
implemented by this normal codec.

Command-5 construction uses `WireModelIndex`, deliberately distinct from model
Code. The Match CL AMP Gain edit/restore fixture validates Index 13, padding 0,
and current context `[0,23]` for that control only. The codec does not claim
other model, parameter, transform or persistence operations were validated.

Run `cargo test -p gp200-protocol` for real capture, callback boundary,
realtime, overlap, deadline, length, bit-preservation and malformed-input tests.
Fixture provenance is under `tests/fixtures/manifest.json`.
