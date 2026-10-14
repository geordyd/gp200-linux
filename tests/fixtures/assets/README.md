# Asset fixtures

`upstream-lstm-0.5.4.nam` is the 70 weight mono LSTM example from `sdatkinson/NeuralAmpModelerCore/example_models/lstm.nam` at commit `0b3d3c97b0859a3a8c92a8628c4dd89a25eb5842`, retrieved through the GitHub plugin. SHA-256 `df73653fb73735d7fed584627c657fe3c5a44db62b66a563073c1ac8621d42e4`. MIT license attribution is in [the notice](../../../assets/licenses/neural-amp-modeler-core-MIT.txt).

`upstream-lstm-reference.json` contains 64 input samples and standard/fast output from an independent Eigen matrix-vector reference. Regenerate with `python3 tools/assets/generate_lstm_reference.py`; the production application does not require Python, Eigen or a C++ compiler. This fixture establishes the local upstream LSTM weight layout and numerical behavior; it is not a GP-200 SnapTone golden.

Other WAV, corrupt-input and analytic LSTM fixtures are generated in Rust tests to keep them small, inspectable and independent of vendor assets. The shipped70second stimulus and official firmware are referenced only in local investigation evidence; they are not redistributed as these test fixtures.
