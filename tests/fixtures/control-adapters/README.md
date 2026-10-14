# Control adapter byte oracles

These immutable fixtures come from root-operated, reversible GP-200 tests.
`manifest.json` records each source path and SHA256. The requests were encoded
by the independent Python reference client, sent to the physical GP-200, and
accepted only after all unrelated meaningful preset bytes and exact restoration
were checked. They are not captures from an executed Windows application.

The 1176-byte before snapshots retain every TLV and all fifteen module float
slots, including undefined tails. The request files contain the complete SysEx
wire packets. Firmware identity was unavailable; no GP-200LT/JR claim follows.

V-Roto and Tube initially have Sync=1/subdivision4. Their first negative runs
proved that Power Off alone automatically sets the corresponding ordinary value
to the XML default (0.5 Hz and 500 ms). Corrected paired runs retain explicit
Power/value ordering and full restoration. The negative evidence remains in
`analysis/implementation/control-mod-sync` and `control-dly-sync`.

Regenerate copies only from existing accepted captures, without opening MIDI:

```sh
python3 analysis/implementation/research-control-adapters/extract_evidence.py
```
