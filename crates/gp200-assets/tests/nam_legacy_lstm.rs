use gp200_assets::{ActivationMode, NamModel, SampleProcessor, inspect_nam, inspect_nam_with_size};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn reference() -> Value {
    serde_json::from_slice(include_bytes!(
        "../../../tests/fixtures/assets/legacy-lstm-core-reference.json"
    ))
    .unwrap()
}

fn synthetic(hidden: usize, layers: usize, version: &str) -> Value {
    let count = 4 * hidden * (1 + hidden)
        + 6 * hidden
        + (layers - 1) * (8 * hidden * hidden + 6 * hidden)
        + hidden
        + 1;
    let weights: Vec<f32> = (0..count)
        .map(|index| ((index * 13 % 31) as i32 - 15) as f32 / 128.0)
        .collect();
    json!({
        "version": version, "architecture": "LSTM", "sample_rate": 48000,
        "config": {"input_size": 1, "hidden_size": hidden, "num_layers": layers},
        "weights": weights
    })
}

fn floats(value: &Value) -> Vec<f32> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|sample| sample.as_f64().unwrap() as f32)
        .collect()
}

fn compare_core_outputs(name: &str, source: &[u8], profile: &Value, input: &[f32]) {
    let info = inspect_nam(source).unwrap();
    assert!(info.supported_inference, "{name}: {}", info.reason);
    assert_eq!(info.network_profile.as_deref(), Some("LSTM"));
    assert_eq!(info.render_sample_rate, Some(48000));
    for (mode, key) in [
        (ActivationMode::Standard, "standard"),
        (ActivationMode::Fast, "fast"),
    ] {
        let expected = floats(&profile[key]);
        assert_eq!(input.len(), expected.len());
        let mut model = NamModel::load(source).unwrap();
        model.set_activation(mode);
        model.reset().unwrap();
        let mut output = vec![0.0; input.len()];
        model.process(input, &mut output).unwrap();
        let mut max_absolute = 0.0_f32;
        let mut max_normalized = 0.0_f32;
        for (index, (actual, expected)) in output.iter().zip(&expected).enumerate() {
            let error = (actual - expected).abs();
            let limit = 1e-6 + 2e-5 * expected.abs();
            max_absolute = max_absolute.max(error);
            max_normalized = max_normalized.max(error / limit);
            assert!(
                actual.is_finite() && error <= limit,
                "{name} {key} sample {index}: {actual} vs {expected}; limit {limit}"
            );
        }
        eprintln!("{name} {key}: max_abs={max_absolute}, max_normalized={max_normalized}");
        model.reset().unwrap();
        let mut fragmented = vec![0.0; input.len()];
        let mut start = 0;
        for size in [1, 7, 1024, 3, 89, 511, 1024] {
            let end = (start + size).min(input.len());
            model
                .process(&input[start..end], &mut fragmented[start..end])
                .unwrap();
            start = end;
            if start == input.len() {
                break;
            }
        }
        if start < input.len() {
            model
                .process(&input[start..], &mut fragmented[start..])
                .unwrap();
        }
        assert_eq!(
            output, fragmented,
            "{name} {key} reset or fragmentation differs"
        );
    }
}

#[test]
fn legacy_lstm_synthetic_and_limit_profiles_match_exact_pinned_core() {
    let reference = reference();
    let input = floats(&reference["input"]);
    assert_eq!(input.len(), 2057);
    for profile in reference["synthetic_profiles"].as_array().unwrap() {
        let hidden = profile["hidden_size"].as_u64().unwrap() as usize;
        let layers = profile["num_layers"].as_u64().unwrap() as usize;
        assert!(profile["oracle_0.5.4_outputs_identical"].as_bool().unwrap());
        for version in ["0.5.2", "0.5.4"] {
            let source = serde_json::to_vec(&synthetic(hidden, layers, version)).unwrap();
            let info = inspect_nam(&source).unwrap();
            assert_eq!(info.version, version);
            assert_eq!(
                info.weight_count,
                profile["weight_count"].as_u64().unwrap() as usize
            );
            compare_core_outputs(profile["name"].as_str().unwrap(), &source, profile, &input);
        }
    }
}

#[test]
#[ignore = "requires ignored author download; run explicitly with GP200_NAM_LEGACY_MODEL if needed"]
fn unchanged_author_0_5_2_model_matches_exact_pinned_core() {
    let path = std::env::var_os("GP200_NAM_LEGACY_MODEL")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../analysis/downloads/nam-author-benchmark/LSTM-1-032.nam")
        });
    let source =
        std::fs::read(&path).expect("Retain the unmodified author model before this explicit test");
    let reference = reference();
    let profile = &reference["author_profile"];
    assert_eq!(
        format!("{:x}", Sha256::digest(&source)),
        profile["source_sha256"].as_str().unwrap()
    );
    let info = inspect_nam(&source).unwrap();
    assert_eq!(info.version, "0.5.2");
    assert_eq!(info.config, profile["config"]);
    assert_eq!(info.weight_count, 4449);
    assert_eq!(info.operations_per_sample, Some(4256));
    compare_core_outputs(
        "unchanged-author-LSTM-1-032",
        &source,
        profile,
        &floats(&reference["input"]),
    );
    assert_eq!(std::fs::read(path).unwrap(), source);
}

#[test]
fn legacy_version_exception_is_exact_and_lstm_only() {
    for version in [
        "0.5.0",
        "0.5.1",
        "0.5.3",
        "0.5.5",
        "0.6.0",
        "0.7.0",
        "0.5.2-beta",
        "00.5.2",
        "0.5.2 ",
        "99.0.0",
    ] {
        let bytes = serde_json::to_vec(&synthetic(1, 1, version)).unwrap();
        assert!(NamModel::load(&bytes).is_err(), "{version}");
        assert!(
            !inspect_nam(&bytes).unwrap().supported_inference,
            "{version}"
        );
    }
    for architecture in [
        "Linear",
        "ConvNet",
        "WaveNet",
        "Sequential",
        "SlimmableContainer",
        "SlimmableWavenet",
        "Unknown",
    ] {
        let mut model = synthetic(1, 1, "0.5.2");
        model["architecture"] = json!(architecture);
        let bytes = serde_json::to_vec(&model).unwrap();
        let info = inspect_nam(&bytes).unwrap();
        assert!(!info.supported_inference, "{architecture}");
        assert!(info.reason.contains("schema 0.5.2"), "{}", info.reason);
        assert!(NamModel::load(&bytes).is_err(), "{architecture}");
    }
}

#[test]
fn legacy_malformed_dimensions_channels_and_unknown_equations_are_rejected() {
    let mutations = [
        ("hidden_size", json!(0)),
        ("hidden_size", json!(65)),
        ("hidden_size", json!(-1)),
        ("hidden_size", json!(1.5)),
        ("hidden_size", json!(u64::MAX)),
        ("num_layers", json!(0)),
        ("num_layers", json!(5)),
        ("num_layers", json!("1")),
        ("input_size", json!(0)),
        ("input_size", json!(2)),
        ("in_channels", json!(2)),
        ("out_channels", json!(2)),
        ("out_channels", json!(null)),
        ("in_channels", json!(1.5)),
        ("future_transform", json!(true)),
        ("activation", json!("ReLU")),
    ];
    for version in ["0.5.2", "0.5.4"] {
        for (field, value) in &mutations {
            let mut model = synthetic(1, 1, version);
            model["config"][field] = value.clone();
            let source = serde_json::to_vec(&model).unwrap();
            assert!(
                NamModel::load(&source).is_err(),
                "{version} {field}={value}"
            );
            assert!(!inspect_nam(&source).unwrap().supported_inference);
        }
        for field in ["input_size", "hidden_size", "num_layers"] {
            let mut model = synthetic(1, 1, version);
            model["config"].as_object_mut().unwrap().remove(field);
            assert!(NamModel::load(&serde_json::to_vec(&model).unwrap()).is_err());
        }
    }
}

#[test]
fn legacy_weights_rate_size_and_render_budget_remain_bounded() {
    let original = synthetic(1, 1, "0.5.2");
    for count in [0, 15, 17] {
        let mut model = original.clone();
        model["weights"] = json!(vec![0.0; count]);
        let source = serde_json::to_vec(&model).unwrap();
        assert!(NamModel::load(&source).is_err());
    }
    for value in [json!(null), json!("1"), json!(1e100)] {
        let mut model = original.clone();
        model["weights"][0] = value;
        let source = serde_json::to_vec(&model).unwrap();
        assert!(NamModel::load(&source).is_err());
        assert!(inspect_nam(&source).is_err());
    }
    for rate in [0.0, -1.0, 48000.5, 384001.0] {
        let mut model = original.clone();
        model["sample_rate"] = json!(rate);
        assert!(NamModel::load(&serde_json::to_vec(&model).unwrap()).is_err());
    }
    let source = serde_json::to_vec(&original).unwrap();
    assert!(
        !inspect_nam_with_size(&source, 0.5)
            .unwrap()
            .supported_inference
    );
    let model = NamModel::load(&source).unwrap();
    assert_eq!(model.operations_per_sample(), 9);
    assert_eq!(model.validate_render_budget(17, 153).unwrap(), 153);
    assert!(model.validate_render_budget(17, 152).is_err());
    assert!(model.estimated_operations(u64::MAX).is_err());
}

#[test]
fn legacy_state_and_sample_validation_are_retained() {
    let source = serde_json::to_vec(&synthetic(3, 2, "0.5.2")).unwrap();
    let mut model = NamModel::load(&source).unwrap();
    assert!(model.process(&[0.0], &mut []).is_err());
    assert!(model.process(&[f32::NAN], &mut [0.0]).is_err());
    assert!(model.process(&[f32::INFINITY], &mut [0.0]).is_err());
    model.reset().unwrap();
    let mut first = [0.0; 16];
    model.process(&[0.3; 16], &mut first).unwrap();
    model.reset().unwrap();
    let mut second = [0.0; 16];
    model.process(&[0.3; 16], &mut second).unwrap();
    assert_eq!(first, second);
}
