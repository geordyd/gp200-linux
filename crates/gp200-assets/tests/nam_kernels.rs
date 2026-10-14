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
fn values(value: &Value) -> Vec<f32> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap() as f32)
        .collect()
}
fn render(model: &mut NamModel, input: &[f32]) -> Vec<f32> {
    let mut output = vec![0.0; input.len()];
    model.process(input, &mut output).unwrap();
    output
}
#[test]
fn convnet_linear_and_slimmed_sizes_match_independent_upstream() {
    for name in [
        "convnet-tanh",
        "convnet-batchnorm",
        "convnet-prelu",
        "linear-one",
        "linear-biased",
        "linear-long-direct",
        "linear-auto-fft-reference",
        "upstream-slimmable_wavenet",
        "slim-gated",
        "slim-multi-array",
        "slim-mixed-fixed-0",
        "slim-mixed-fixed-1",
        "slim-film-bottleneck",
    ] {
        let source = bytes(&format!("{name}.nam"));
        let reference: Value =
            serde_json::from_slice(&bytes(&format!("{name}-kernel-reference.json"))).unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(&source)),
            reference["source_sha256"].as_str().unwrap()
        );
        let info = inspect_nam(&source).unwrap();
        assert!(info.supported_inference, "{name}: {}", info.reason);
        let input = values(&reference["input"]);
        for (size, modes) in reference["sizes"].as_object().unwrap() {
            for (mode, key) in [
                (ActivationMode::Standard, "standard"),
                (ActivationMode::Fast, "fast"),
            ] {
                let expected = values(&modes[key]);
                let mut model = NamModel::load(&source).unwrap();
                model.set_activation(mode);
                if size != "default" {
                    model.set_size(size.parse().unwrap()).unwrap();
                }
                model.reset().unwrap();
                let actual = render(&mut model, &input);
                let mut max_normalized = 0.0_f32;
                for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
                    let limit = 1e-6 + 2e-5 * expected.abs();
                    max_normalized = max_normalized.max((actual - expected).abs() / limit);
                    assert!(
                        (actual - expected).abs() <= limit,
                        "{name}, {size}, {key}, {index}: {actual} versus {expected}"
                    );
                }
                eprintln!("{name} {size} {key} maximum normalized error {max_normalized}");
                model.reset().unwrap();
                let mut fragmented = Vec::new();
                for chunk in input.chunks(37) {
                    fragmented.extend(render(&mut model, chunk));
                }
                assert_eq!(
                    actual, fragmented,
                    "{name} {size}: fragmentation changes state"
                );
                model.reset().unwrap();
                assert_eq!(actual, render(&mut model, &input));
                assert_eq!(
                    model.estimated_operations(input.len() as u64).unwrap(),
                    model.operations_per_sample() * input.len() as u64
                );
            }
        }
    }
}
#[test]
fn slim_bins_preserve_history_and_changed_selection_resets() {
    let source = bytes("upstream-slimmable_wavenet.nam");
    let reference: Value =
        serde_json::from_slice(&bytes("upstream-slimmable_wavenet-kernel-reference.json")).unwrap();
    let input = values(&reference["input"]);
    let mut model = NamModel::load(&source).unwrap();
    let full_cost = model.operations_per_sample();
    model.set_size(0.0).unwrap();
    assert!(model.operations_per_sample() < full_cost);
    let whole = render(&mut model, &input);
    model.reset().unwrap();
    let mut split = render(&mut model, &input[..713]);
    model.set_size(0.2).unwrap(); // same 1-channel bin: preserve state
    split.extend(render(&mut model, &input[713..]));
    assert_eq!(whole, split);
    model.set_size(1.0 / 3.0).unwrap();
    let second = render(&mut model, &input);
    let mut fresh = NamModel::load(&source).unwrap();
    fresh.set_size(1.0 / 3.0).unwrap();
    assert_eq!(second, render(&mut fresh, &input));
    model.set_size(1.0).unwrap();
    assert_eq!(full_cost, model.operations_per_sample());
    let mut full = NamModel::load(&source).unwrap();
    assert_eq!(render(&mut model, &input), render(&mut full, &input));
    for size in [f64::NAN, f64::INFINITY, -0.01, 1.01] {
        assert!(model.set_size(size).is_err());
    }
}
#[test]
fn new_kernel_shapes_versions_and_numeric_state_are_bounded() {
    for name in ["convnet-tanh", "convnet-batchnorm", "linear-biased"] {
        let source: Value = serde_json::from_slice(&bytes(&format!("{name}.nam"))).unwrap();
        for version in ["0.5.4", "0.6.0", "0.7.0"] {
            let mut valid = source.clone();
            valid["version"] = json!(version);
            assert!(NamModel::load(&serde_json::to_vec(&valid).unwrap()).is_ok());
        }
        let mut invalid = source.clone();
        invalid["version"] = json!("0.8.0");
        assert!(NamModel::load(&serde_json::to_vec(&invalid).unwrap()).is_err());
        let mut invalid = source.clone();
        invalid["weights"].as_array_mut().unwrap().pop();
        assert!(NamModel::load(&serde_json::to_vec(&invalid).unwrap()).is_err());
        let mut invalid = source.clone();
        invalid["weights"].as_array_mut().unwrap().push(json!(0.0));
        assert!(NamModel::load(&serde_json::to_vec(&invalid).unwrap()).is_err());
        let mut invalid = source.clone();
        invalid["config"]["guess"] = json!(1);
        assert!(NamModel::load(&serde_json::to_vec(&invalid).unwrap()).is_err());
        for key in ["in_channels", "out_channels"] {
            let mut invalid = source.clone();
            invalid["config"][key] = json!(2);
            assert!(NamModel::load(&serde_json::to_vec(&invalid).unwrap()).is_err());
        }
        let mut model = NamModel::load(&serde_json::to_vec(&source).unwrap()).unwrap();
        assert!(model.process(&[f32::NAN], &mut [0.0]).is_err());
        model.reset().unwrap();
        assert!(model.process(&[1.0], &mut []).is_err());
        assert!(model.estimated_operations(u64::MAX).is_err());
        assert!(model.validate_render_budget(1024, 1).is_err());
    }
    let mut conv: Value = serde_json::from_slice(&bytes("convnet-tanh.nam")).unwrap();
    for dilations in [
        json!([]),
        json!([0]),
        json!([65537]),
        json!([-1]),
        json!(vec![1; 33]),
    ] {
        conv["config"]["dilations"] = dilations;
        assert!(NamModel::load(&serde_json::to_vec(&conv).unwrap()).is_err());
    }
    let mut linear: Value = serde_json::from_slice(&bytes("linear-biased.nam")).unwrap();
    for length in [0, 65537, -1] {
        linear["config"]["receptive_field"] = json!(length);
        assert!(NamModel::load(&serde_json::to_vec(&linear).unwrap()).is_err());
    }
    linear["config"]["receptive_field"] = json!(7);
    linear["config"]["implementation"] = json!("fft");
    let inspection = inspect_nam(&serde_json::to_vec(&linear).unwrap()).unwrap();
    assert!(!inspection.supported_inference);
    assert!(inspection.reason.contains("Explicit Linear FFT"));
}
#[test]
fn batchnorm_invalid_variance_and_overflow_reject_before_saturation() {
    let source: Value = serde_json::from_slice(&bytes("convnet-batchnorm.nam")).unwrap();
    // First block has 4*1*2 convolution weights; running variance starts after four means.
    for variance in [-1.0, -0.00001] {
        let mut invalid = source.clone();
        invalid["weights"][12] = json!(variance);
        assert!(NamModel::load(&serde_json::to_vec(&invalid).unwrap()).is_err());
    }
    let huge = json!({"version":"0.7.0", "architecture":"ConvNet", "sample_rate":48000,
        "config":{"channels":1,"dilations":[1],"batchnorm":false,"activation":"Tanh"},
        "weights":[0.0,f32::MAX,0.0,1.0,0.0]});
    let mut model = NamModel::load(&serde_json::to_vec(&huge).unwrap()).unwrap();
    assert!(model.process(&[2.0], &mut [0.0]).is_err());
    let huge = json!({"version":"0.7.0", "architecture":"Linear", "sample_rate":48000,
        "config":{"receptive_field":1,"bias":false},"weights":[f32::MAX]});
    let mut model = NamModel::load(&serde_json::to_vec(&huge).unwrap()).unwrap();
    assert!(model.process(&[2.0], &mut [0.0]).is_err());
}
#[test]
fn new_kernels_share_host_render_cancellation_and_exact_frame_accounting() {
    use gp200_assets::{WaveAudio, WaveEncoding, render_host_stage};
    use gp200_domain::ErrorKind;
    for name in ["convnet-tanh", "linear-biased"] {
        let mut model = NamModel::load(&bytes(&format!("{name}.nam"))).unwrap();
        let input = WaveAudio {
            sample_rate: 48000,
            channels: 1,
            source_encoding: WaveEncoding::Float,
            source_bits: 32,
            samples: vec![0.1; 2057],
        };
        let mut checks = 0;
        let result = render_host_stage(&input, &mut model, || false, |_, _| {}).unwrap();
        assert_eq!(result.1.frames, 2057);
        assert_eq!(result.1.blocks, 3);
        assert_eq!(result.0.samples.len(), 4114);
        let cancellation = std::cell::Cell::new(0);
        let result = render_host_stage(
            &input,
            &mut model,
            || {
                cancellation.set(cancellation.get() + 1);
                cancellation.get() > 2
            },
            |_, _| {
                checks += 1;
            },
        );
        assert_eq!(result.unwrap_err().kind, ErrorKind::Cancelled);
        assert_eq!(checks, 1);
    }
}

#[test]
fn aggregate_convnet_delay_and_bad_slim_selection_are_rejected_without_mutating_history() {
    let weight_count = 256 * 2 + 256 + 256 * 256 * 2 + 256 + 256 + 1;
    let excessive = json!({"version":"0.7.0","architecture":"ConvNet","sample_rate":48000,
        "config":{"channels":256,"dilations":[65536,65536],"batchnorm":false,"activation":"Tanh"},
        "weights":vec![0.0;weight_count]});
    let error = NamModel::load(&serde_json::to_vec(&excessive).unwrap()).unwrap_err();
    assert!(error.message.contains("64 MiB"), "{}", error.message);
    let source = bytes("upstream-slimmable_wavenet.nam");
    let mut control = NamModel::load(&source).unwrap();
    let mut tested = control.clone();
    tested.set_size(0.0).unwrap();
    control.set_size(0.0).unwrap();
    assert_eq!(
        render(&mut tested, &[0.1; 19]),
        render(&mut control, &[0.1; 19])
    );
    assert!(tested.set_size(f64::NAN).is_err());
    assert!(tested.set_size(-1.0).is_err());
    assert_eq!(
        render(&mut tested, &[0.2; 31]),
        render(&mut control, &[0.2; 31])
    );
    let mut fixed = NamModel::load(&bytes("linear-biased.nam")).unwrap();
    assert!(fixed.set_size(0.5).is_err());
    assert!(fixed.set_size(1.0).is_ok());
}

#[test]
fn unverified_pruned_groups_prelu_and_skip_links_are_explicit_and_atomic() {
    let mut grouped: Value =
        serde_json::from_slice(&bytes("upstream-wavenet_a2_feature_test.nam")).unwrap();
    grouped["config"]["layers"][0]["slimmable"] =
        json!({"method":"slice_channels_uniform","kwargs":{"allowed_channels":[1,2,4]}});
    let mut prelu: Value = serde_json::from_slice(&bytes("slim-film-bottleneck.nam")).unwrap();
    prelu["config"]["layers"][0]["activation"] =
        json!({"type":"PReLU","negative_slopes":[0.03,0.07]});
    let mut linked: Value = serde_json::from_slice(&bytes("slim-multi-array.nam")).unwrap();
    linked["config"]["layers"][1]["head1x1"] = json!({"active":true,"out_channels":2,"groups":1});
    let weights = linked["weights"].as_array_mut().unwrap();
    let offset = weights.len() - 4; // second head's two coefficients, bias, learned final scale
    weights.splice(
        offset..offset,
        [
            json!(1.0),
            json!(0.0),
            json!(0.0),
            json!(1.0),
            json!(0.0),
            json!(0.0),
        ],
    );
    for (source, reason) in [
        (grouped, "groups=1"),
        (prelu, "PReLU"),
        (linked, "head/skip"),
    ] {
        let encoded = serde_json::to_vec(&source).unwrap();
        let mut tested = NamModel::load(&encoded).unwrap();
        let mut control = tested.clone();
        assert_eq!(
            render(&mut tested, &[0.1; 19]),
            render(&mut control, &[0.1; 19])
        );
        let cost = tested.operations_per_sample();
        let error = tested.set_size(0.0).unwrap_err();
        assert!(error.message.contains(reason), "{}", error.message);
        assert_eq!(tested.operations_per_sample(), cost);
        assert_eq!(
            render(&mut tested, &[0.2; 31]),
            render(&mut control, &[0.2; 31])
        );
    }
}
