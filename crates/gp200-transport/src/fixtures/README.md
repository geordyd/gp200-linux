# Observed device clear preset

`device-cleared.payload.bin` is the complete decoded 1176-byte stored-slot 255
reply after a real GP-200 Normal-mode `2009` clear. SHA256:
`8a5b83f40a4e6fa86342d5e0eefaf93d0fe65d9e5a5137051542a36c1b9236af`.

Source: `analysis/implementation/clear-changed-scratch-roundtrip/after-clear.payload.bin`.
The experiment first changed the scratch name to `GP200ClearTest` using the
separately verified full-preset write, verified that change, cleared it, compared
the complete reply against an independent prior clear reply, and restored the
original scratch preset and current buffer. Both stored slots remained restored.

This is the device's observed default: name `It's GP-200`, tempo 120, level 50,
chain 0..10, all modules disabled except Volume. It is distinct from the original
editor's local empty-name/level 80 initialization. The actor retargets only the
established context, metadata slot and chain slot fields, preserves the captured
opaque bytes, and compares every other byte except the precisely documented
volatile offsets. A mismatching reset on another firmware is a conflict with
actual state retained, never a fabricated success.

Hardware clear currently requires an inactive stored slot and a matching
reviewed target revision. This fixture does not authorize clearing the active
buffer, factory reset, asset erasure or firmware operations.
