use gp200_assets::{ActivationMode, NamModel, SampleProcessor, inspect_nam};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
fn bytes(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/../../tests/fixtures/assets/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}
fn fixture(name: &str) -> Value {
    serde_json::from_slice(&bytes(name)).unwrap()
}
#[test]
fn store_render_budget_is_explicit_preserves_sources_and_records_cost() {
    use gp200_assets::{AssetStore, WaveAudio, WaveEncoding, inspect_wav, write_float_wav};
    use gp200_domain::{AssetJobSpec, AssetKind, JobId};
    use std::{collections::BTreeMap, fs};
    let path = std::env::temp_dir().join(format!("gp200-nam-budget-{}", std::process::id()));
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fs::create_dir_all(&path).unwrap();
    let _cleanup = Cleanup(path.clone());
    let source = bytes("upstream-wavenet.nam");
    let source_path = path.join("source.nam");
    fs::write(&source_path, &source).unwrap();
    let input = write_float_wav(&WaveAudio {
        sample_rate: 48000,
        channels: 1,
        source_encoding: WaveEncoding::Float,
        source_bits: 32,
        samples: vec![0.1; 1031],
    })
    .unwrap();
    let input_path = path.join("input.wav");
    fs::write(&input_path, &input).unwrap();
    let mut store = AssetStore::open(path.join("store")).unwrap();
    let mut request = AssetJobSpec {
        id: JobId(70),
        kind: AssetKind::Nam,
        source: source_path.clone(),
        output: None,
        options: BTreeMap::from([(
            "input_wav".into(),
            input_path.to_string_lossy().into_owned(),
        )]),
    };
    for invalid_limit in ["1", "0", "1000000000001", "-1", "1.5", "huge"] {
        request
            .options
            .insert("max_operations".into(), invalid_limit.into());
        assert!(
            store.prepare(&request, || false).is_err(),
            "{invalid_limit}"
        );
    }
    request
        .options
        .insert("max_operations".into(), "1000000000000".into());
    let output = store.prepare(&request, || false).unwrap().derived.unwrap();
    let model = NamModel::load(&source).unwrap();
    assert_eq!(output.properties["max_operations"], "1000000000000");
    assert_eq!(
        output.properties["estimated_operations"],
        model.estimated_operations(1031).unwrap().to_string()
    );
    assert_eq!(
        output.properties["operations_per_sample"],
        model.operations_per_sample().to_string()
    );
    assert_eq!(output.properties["network_profile"], "WaveNet");
    assert_eq!(
        output.properties["conversion_profile"],
        "local-nam-core-0b3d3c97-host-wav-v2"
    );
    assert_eq!(store.original_bytes(&output.id).unwrap(), source);
    assert_eq!(
        store.dependency_bytes(&output.id, "input_wav").unwrap(),
        input
    );
    assert_eq!(fs::read(source_path).unwrap(), source);
    assert_eq!(fs::read(input_path).unwrap(), input);
    assert_eq!(
        inspect_wav(&store.content_bytes(&output.id).unwrap())
            .unwrap()
            .frames(),
        1031
    );
    request.options.remove("max_operations");
    let mut default_store = AssetStore::open(path.join("default-store")).unwrap();
    let default = default_store
        .prepare(&request, || false)
        .unwrap()
        .derived
        .unwrap();
    assert_eq!(default.properties["max_operations"], "2000000000");
}
#[test]
fn wavenet_and_sequential_match_independent_pinned_upstream_in_both_modes() {
    for name in [
        "upstream-wavenet",
        "upstream-wavenet_a2_feature_test",
        "gated-wavenet",
        "post-head-wavenet",
        "sequential-wavenet-lstm",
        "upstream-A2",
        "upstream-slimmable_wavenet",
        "upstream-slimmable_container",
        "lstm-conditioned-wavenet",
    ] {
        let original = bytes(&format!("{name}.nam"));
        let reference = fixture(&format!("{name}-reference.json"));
        assert_eq!(
            format!("{:x}", Sha256::digest(&original)),
            reference["source_sha256"].as_str().unwrap()
        );
        assert!(
            inspect_nam(&original).unwrap().supported_inference,
            "{name}"
        );
        let input: Vec<f32> = reference["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap() as f32)
            .collect();
        for (mode, key) in [
            (ActivationMode::Standard, "standard"),
            (ActivationMode::Fast, "fast"),
        ] {
            let expected: Vec<f32> = reference[key]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_f64().unwrap() as f32)
                .collect();
            let mut model = NamModel::load(&original).unwrap();
            model.set_activation(mode);
            model.reset().unwrap();
            let mut output = vec![0.; input.len()];
            model.process(&input, &mut output).unwrap();
            let mut max_normalized = 0.0_f32;
            for (i, (a, b)) in output.iter().zip(&expected).enumerate() {
                let limit = 1e-6 + 2e-5 * b.abs();
                max_normalized = max_normalized.max((a - b).abs() / limit);
                assert!(
                    (a - b).abs() <= limit,
                    "{name} {key} sample {i}: {a} vs {b}, limit {limit}"
                );
            }
            eprintln!("{name} {key} normalized maximum error {max_normalized}");
            model.reset().unwrap();
            let mut fragmented = vec![0.; input.len()];
            let mut start = 0;
            for size in [1, 7, 1024, 3, 89, 511, 1024] {
                if start == input.len() {
                    break;
                }
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
            assert_eq!(
                output, fragmented,
                "{name} output depends on block size or reset"
            );
        }
    }
}
#[test]
fn malformed_dimensions_weights_groups_and_future_fields_are_explicitly_rejected() {
    let original = fixture("upstream-wavenet.nam");
    let mutations: [(&str, Value); 10] = [
        ("channels", json!(0)),
        ("channels", json!(257)),
        ("groups_input", json!(0)),
        ("groups_input", json!(2)),
        ("bottleneck", json!(512)),
        ("dilations", json!([u64::MAX])),
        ("kernel_size", json!(33)),
        ("kernel_sizes", json!([2, 2])),
        ("activation", json!("UnknownActivation")),
        ("future_transform", json!(true)),
    ];
    for (field, value) in mutations {
        let mut v = original.clone();
        v["config"]["layers"][0][field] = value;
        assert!(
            NamModel::load(&serde_json::to_vec(&v).unwrap()).is_err(),
            "{field}"
        );
    }
    for count in [0, 130, 132] {
        let mut v = original.clone();
        v["weights"] = json!(vec![0.0; count]);
        assert!(NamModel::load(&serde_json::to_vec(&v).unwrap()).is_err());
    }
    let mut v = original.clone();
    v["weights"][0] = json!(1e100);
    assert!(inspect_nam(&serde_json::to_vec(&v).unwrap()).is_err());
    let mut v = original.clone();
    v["config"]["layers"][1]["input_size"] = json!(4);
    assert!(NamModel::load(&serde_json::to_vec(&v).unwrap()).is_err());
    let mut v = original.clone();
    v["config"]["layers"][0]["dilations"] = json!(vec![65536; 32]);
    v["config"]["layers"][0]["channels"] = json!(256);
    v["config"]["layers"][0]["kernel_size"] = json!(32);
    assert!(NamModel::load(&serde_json::to_vec(&v).unwrap()).is_err());
}
#[test]
fn sequential_enforces_child_rates_versions_empty_top_weights_and_bounded_depth() {
    let original = fixture("sequential-wavenet-lstm.nam");
    for (field, value) in [
        ("sample_rate", json!(44100)),
        ("version", json!("99.0.0")),
        ("architecture", json!("NotWaveNet")),
    ] {
        let mut v = original.clone();
        v["config"]["models"][0][field] = value;
        assert!(NamModel::load(&serde_json::to_vec(&v).unwrap()).is_err());
    }
    let mut v = original.clone();
    v["weights"] = json!([0]);
    assert!(NamModel::load(&serde_json::to_vec(&v).unwrap()).is_err());
    let mut v = original.clone();
    v["config"]["models"] = json!([]);
    assert!(NamModel::load(&serde_json::to_vec(&v).unwrap()).is_err());
    let mut v = original;
    for _ in 0..10 {
        v = json!({"version":"0.7.0","architecture":"Sequential","config":{"models":[v]},"weights":[],"sample_rate":48000});
    }
    assert!(NamModel::load(&serde_json::to_vec(&v).unwrap()).is_err());
}
#[test]
fn sample_validation_fails_and_reset_repeats_the_initial_state() {
    for name in [
        "upstream-wavenet.nam",
        "upstream-wavenet_a2_feature_test.nam",
        "sequential-wavenet-lstm.nam",
    ] {
        let mut model = NamModel::load(&bytes(name)).unwrap();
        assert!(model.process(&[0.], &mut []).is_err());
        assert!(model.process(&[f32::NAN], &mut [0.]).is_err());
        assert!(model.process(&[f32::INFINITY], &mut [0.]).is_err());
        model.reset().unwrap();
        let mut a = [0.; 8];
        model.process(&[0.3; 8], &mut a).unwrap();
        model.reset().unwrap();
        let mut b = [0.; 8];
        model.process(&[0.3; 8], &mut b).unwrap();
        assert_eq!(a, b);
    }
}
#[test]
fn container_selection_uses_strict_thresholds_and_restarts_selected_child() {
    let original = fixture("upstream-A2.nam");
    let mut model = NamModel::load(&bytes("upstream-A2.nam")).unwrap();
    let NamModel::Container(container) = &mut model else {
        panic!("A2 is an explicit container")
    };
    assert_eq!(container.selected_index(), 1);
    container.set_size(0.499).unwrap();
    assert_eq!(container.selected_index(), 0);
    let input = [0.2; 16];
    let mut actual = [0.; 16];
    container.process(&input, &mut actual).unwrap();
    let child = serde_json::to_vec(&original["config"]["submodels"][0]["model"]).unwrap();
    let mut reference = NamModel::load(&child).unwrap();
    let mut expected = [0.; 16];
    reference.reset().unwrap();
    reference.process(&input, &mut expected).unwrap();
    assert_eq!(actual, expected);
    container.set_size(0.5).unwrap();
    assert_eq!(container.selected_index(), 1);
    container.set_size(0.).unwrap();
    container.process(&input, &mut actual).unwrap();
    assert_eq!(actual, expected);
    for size in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
        assert!(container.set_size(size).is_err());
    }
    for thresholds in [
        json!([0.5, 0.4]),
        json!([0.5, 0.9]),
        json!([0.5, 0.5]),
        json!([0.0, 1.0]),
    ] {
        let mut bad = original.clone();
        for i in 0..2 {
            bad["config"]["submodels"][i]["max_value"] = thresholds[i].clone();
        }
        assert!(NamModel::load(&serde_json::to_vec(&bad).unwrap()).is_err());
    }
}
#[test]
fn operation_estimates_are_checked_and_long_render_requires_bounded_opt_in() {
    let original = bytes("upstream-A2.nam");
    let model = NamModel::load(&original).unwrap();
    let info = inspect_nam(&original).unwrap();
    assert_eq!(
        info.operations_per_sample,
        Some(model.operations_per_sample())
    );
    assert_eq!(info.network_profile.as_deref(), Some("SlimmableContainer"));
    assert_eq!(info.weight_count, 14017);
    let frames = 70 * 48000;
    let estimate = model.estimated_operations(frames).unwrap();
    assert!(estimate > gp200_assets::DEFAULT_NAM_RENDER_OPERATIONS);
    assert!(
        model
            .validate_render_budget(frames, gp200_assets::DEFAULT_NAM_RENDER_OPERATIONS)
            .is_err()
    );
    assert_eq!(
        model
            .validate_render_budget(frames, gp200_assets::MAX_NAM_RENDER_OPERATIONS)
            .unwrap(),
        estimate
    );
    assert!(model.validate_render_budget(1, 0).is_err());
    assert!(
        model
            .validate_render_budget(1, gp200_assets::MAX_NAM_RENDER_OPERATIONS + 1)
            .is_err()
    );
    assert!(model.estimated_operations(u64::MAX).is_err());
}
#[test]
fn wavenet_host_render_cancels_at_the_next_1024_frame_boundary() {
    use std::cell::Cell;
    let progressed = Cell::new(0);
    let mut model = NamModel::load(&bytes("upstream-wavenet.nam")).unwrap();
    let input = gp200_assets::WaveAudio {
        sample_rate: 48000,
        channels: 1,
        source_encoding: gp200_assets::WaveEncoding::Float,
        source_bits: 32,
        samples: vec![0.2; 2057],
    };
    let error = gp200_assets::render_host_stage(
        &input,
        &mut model,
        || progressed.get() >= 1024,
        |done, _| progressed.set(done),
    )
    .unwrap_err();
    assert_eq!(progressed.get(), 1024);
    assert_eq!(error.kind, gp200_domain::ErrorKind::Cancelled);
}
#[test]
fn finite_weights_cannot_hide_pre_activation_overflow() {
    let mut value = fixture("gated-wavenet.nam");
    for weight in value["weights"].as_array_mut().unwrap() {
        *weight = json!(1e38);
    }
    let mut model = NamModel::load(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(model.process(&[10.], &mut [0.]).is_err());
}
