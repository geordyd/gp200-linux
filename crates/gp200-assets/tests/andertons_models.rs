//! Optional real-input verification. Public weights and oracle arrays remain ignored.
use gp200_assets::{ActivationMode, NamModel, SampleProcessor, inspect_nam};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, io::Write, path::PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn floats(values: &Value) -> Vec<f32> {
    values
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_f64().unwrap() as f32)
        .collect()
}

#[test]
#[ignore = "requires private downloaded models and pinned Core oracle; run explicitly in release mode"]
fn unchanged_public_models_match_core_and_reset_with_arbitrary_block_boundaries() {
    let profile_bytes = fs::read(root().join("tests/fixtures/assets/andertons-profiles.json"))
        .expect("Explicit real-input test requires the separately retained corpus profile");
    let profiles: Value = serde_json::from_slice(&profile_bytes).unwrap();
    let configured = std::env::var_os("GP200_ANDERTONS_CORPUS");
    let corpus = configured.as_ref().map(PathBuf::from).unwrap_or_else(root);
    let reference_path = root().join(profiles["reference"]["path"].as_str().unwrap());
    if !reference_path.exists()
        || profiles["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|profile| !corpus.join(profile["path"].as_str().unwrap()).exists())
    {
        panic!(
            "Explicit real-input test requires complete Andertons models and pinned Core arrays; retain them before running this ignored test."
        );
    }
    let reference_bytes = fs::read(reference_path).unwrap();
    assert_eq!(
        digest(&reference_bytes),
        profiles["reference"]["sha256"].as_str().unwrap()
    );
    let reference: Value = serde_json::from_slice(&reference_bytes).unwrap();
    let input = floats(&reference["input"]);
    assert_eq!(input.len(), 6151);
    assert_eq!(profiles["models"].as_array().unwrap().len(), 7);
    let mut cases = Vec::new();
    for (index, profile) in profiles["models"].as_array().unwrap().iter().enumerate() {
        let path = corpus.join(profile["path"].as_str().unwrap());
        let source = fs::read(&path).unwrap();
        assert_eq!(source.len() as u64, profile["bytes"].as_u64().unwrap());
        assert_eq!(digest(&source), profile["sha256"].as_str().unwrap());
        let info = inspect_nam(&source).unwrap();
        assert!(info.supported_inference, "{}", info.reason);
        assert_eq!(info.version, "0.5.4");
        assert_eq!(info.network_profile.as_deref(), Some("WaveNet"));
        assert_eq!(info.config, profile["config"]);
        assert_eq!(info.weight_count, 13802);
        assert_eq!(info.operations_per_sample, Some(13320));
        assert_eq!(info.render_sample_rate, Some(48000));
        assert_eq!(
            reference["models"][index]["source_sha256"],
            profile["sha256"]
        );
        for (mode, key) in [
            (ActivationMode::Standard, "standard"),
            (ActivationMode::Fast, "fast"),
        ] {
            let expected = floats(&reference["models"][index][key]);
            assert_eq!(input.len(), expected.len());
            let mut model = NamModel::load(&source).unwrap();
            model.set_activation(mode);
            model.reset().unwrap();
            let mut output = vec![0.0; input.len()];
            model.process(&input, &mut output).unwrap();
            let mut max_absolute = 0.0_f32;
            let mut max_normalized = 0.0_f32;
            let mut sum_squared = 0.0_f64;
            for (sample, (actual, expected)) in output.iter().zip(&expected).enumerate() {
                let error = (actual - expected).abs();
                let limit = 1e-6 + 2e-5 * expected.abs();
                max_absolute = max_absolute.max(error);
                max_normalized = max_normalized.max(error / limit);
                sum_squared += f64::from(error).powi(2);
                assert!(
                    actual.is_finite() && error <= limit,
                    "{} {key} sample {sample}: {actual} vs {expected}, limit {limit}",
                    path.display()
                );
            }
            // Reuse the same model after driving its delay buffers to test real reset.
            model.reset().unwrap();
            let mut fragmented = vec![0.0; input.len()];
            let mut start = 0;
            for size in [1, 7, 1023, 2, 89, 511, 1024, 5, 2049] {
                let end = (start + size).min(input.len());
                model
                    .process(&input[start..end], &mut fragmented[start..end])
                    .unwrap();
                start = end;
            }
            if start < input.len() {
                model
                    .process(&input[start..], &mut fragmented[start..])
                    .unwrap();
            }
            assert_eq!(output, fragmented, "reset/fragmentation differs for {key}");
            let row = json!({"path": profile["path"], "sha256": profile["sha256"],
                "mode": key, "samples": input.len(), "max_absolute_error": max_absolute,
                "max_normalized_error": max_normalized,
                "rms_error": (sum_squared / input.len() as f64).sqrt(),
                "tolerance": "1e-6 + 2e-5 * abs(Core sample)",
                "fragmentation_bit_exact": true, "reset_bit_exact": true,
                "input_unchanged": fs::read(&path).unwrap() == source});
            eprintln!("{row}");
            cases.push(row);
        }
        assert_eq!(fs::read(path).unwrap(), source);
    }
    if let Some(path) = std::env::var_os("GP200_ANDERTONS_NAM_REPORT") {
        let report = json!({"schema": 1, "profiles_sha256": digest(&profile_bytes), "midi_opened": false,
            "public_weights_redistributed": false, "cases": cases});
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap()
            .write_all(&serde_json::to_vec_pretty(&report).unwrap())
            .unwrap();
    }
}
