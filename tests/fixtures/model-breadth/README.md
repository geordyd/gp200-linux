# Native GP-200 factory model captures

These 35 payloads are exact copies of independently captured normal-mode
GP-200 results from 2026-10-10. The installed firmware was not established.
`manifest.json` pins every original source/report hash and identifies positive
versus ignored-negative evidence. Source originals remain under `analysis/`.

Eleven positive pairs each retain before, after and direct-return payloads.
Gate3 retains before and ignored after payloads and is never promoted.
`cases.rs` supplies the positive payloads to actor and reducer regression
tests; those tests establish offline behavior, not new physical evidence.

Validate provenance without MIDI:

```sh
python3 tools/lab/test_rust_model_transitions.py
```
