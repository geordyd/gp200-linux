use gp200_assets::*;
use gp200_domain::*;
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "gp200-assets-test-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn mono(samples: Vec<f32>) -> WaveAudio {
    WaveAudio {
        sample_rate: 48_000,
        channels: 1,
        source_encoding: WaveEncoding::Float,
        source_bits: 32,
        samples,
    }
}
fn spec(id: u64, source: PathBuf) -> AssetJobSpec {
    AssetJobSpec {
        id: JobId(id),
        kind: AssetKind::ImpulseResponse,
        source,
        output: None,
        options: BTreeMap::new(),
    }
}
fn source(scratch: &Scratch, audio: &WaveAudio) -> PathBuf {
    let path = scratch.0.join("source.wav");
    fs::write(&path, write_float_wav(audio).unwrap()).unwrap();
    path
}

#[test]
fn float_wave_roundtrip_preserves_sample_bits_and_stereo() {
    let mut audio = mono(vec![-0.0, 0.25, -0.5, 1.5, 1.0, -1.0]);
    audio.channels = 2;
    let decoded = inspect_wav(&write_float_wav(&audio).unwrap()).unwrap();
    assert_eq!(decoded, audio);
    assert_eq!(decoded.frames(), 3);
    let info = decoded.inspect().unwrap();
    assert_eq!(info.clipped_samples, 3);
    assert_eq!(info.waveform.len(), 3);
    assert_eq!(info.peak, 1.5);
}
#[test]
fn pcm16_detects_both_full_scale_endpoints() {
    let decoded = inspect_wav(&write_pcm16_wav(&mono(vec![-1.0, 1.0, 0.5])).unwrap()).unwrap();
    assert_eq!(decoded.source_bits, 16);
    assert_eq!(decoded.samples, [-1.0, 32767.0 / 32768.0, 0.5]);
    assert_eq!(decoded.inspect().unwrap().clipped_samples, 2);
}
fn raw_wave(tag: u16, bits: u16, channels: u16, data: &[u8]) -> Vec<u8> {
    let padded = data.len() + data.len() % 2;
    let align = channels * (bits / 8);
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + padded as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&tag.to_le_bytes());
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&48000_u32.to_le_bytes());
    bytes.extend_from_slice(&(48000_u32 * u32::from(align)).to_le_bytes());
    bytes.extend_from_slice(&align.to_le_bytes());
    bytes.extend_from_slice(&bits.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(data);
    if data.len() % 2 == 1 {
        bytes.push(0);
    }
    bytes
}
#[test]
fn pcm8_pcm24_pcm32_and_float64_formats_decode_correctly() {
    assert_eq!(
        inspect_wav(&raw_wave(1, 8, 1, &[0, 128, 255]))
            .unwrap()
            .samples,
        [-1.0, 0.0, 127.0 / 128.0]
    );
    assert_eq!(
        inspect_wav(&raw_wave(1, 24, 1, &[0, 0, 128, 0, 0, 0, 255, 255, 127]))
            .unwrap()
            .samples,
        [-1.0, 0.0, 8388607.0 / 8388608.0]
    );
    let mut pcm32 = Vec::new();
    for value in [i32::MIN, 0, 1073741824] {
        pcm32.extend_from_slice(&value.to_le_bytes());
    }
    assert_eq!(
        inspect_wav(&raw_wave(1, 32, 1, &pcm32)).unwrap().samples,
        [-1.0, 0.0, 0.5]
    );
    let mut float64 = Vec::new();
    for value in [0.5_f64, -0.25] {
        float64.extend_from_slice(&value.to_le_bytes());
    }
    assert_eq!(
        inspect_wav(&raw_wave(3, 64, 1, &float64)).unwrap().samples,
        [0.5, -0.25]
    );
}
#[test]
fn odd_unknown_chunks_are_skipped_and_supported_extensible_guid_is_checked() {
    let original = raw_wave(1, 16, 1, &[0, 0]);
    let mut bytes = original[..12].to_vec();
    bytes.extend_from_slice(b"JUNK\x01\0\0\0x\0");
    bytes.extend_from_slice(&original[12..]);
    let size = bytes.len() as u32 - 8;
    bytes[4..8].copy_from_slice(&size.to_le_bytes());
    assert_eq!(inspect_wav(&bytes).unwrap().samples, [0.0]);
    let mut extensible = original[..36].to_vec();
    extensible[16..20].copy_from_slice(&40_u32.to_le_bytes());
    extensible[20..22].copy_from_slice(&0xfffe_u16.to_le_bytes());
    extensible.extend_from_slice(&22_u16.to_le_bytes());
    extensible.extend_from_slice(&16_u16.to_le_bytes());
    extensible.extend_from_slice(&1_u32.to_le_bytes());
    extensible.extend_from_slice(&[
        1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71,
    ]);
    extensible.extend_from_slice(&original[36..]);
    let size = extensible.len() as u32 - 8;
    extensible[4..8].copy_from_slice(&size.to_le_bytes());
    assert_eq!(inspect_wav(&extensible).unwrap().samples, [0.0]);
    extensible[50] = 255;
    assert_eq!(
        inspect_wav(&extensible).unwrap_err().kind,
        ErrorKind::Unsupported
    );
}
#[test]
fn riff_rejects_truncation_oversized_chunks_duplicate_format_and_partial_frames() {
    let original = write_float_wav(&mono(vec![0.0, 1.0])).unwrap();
    for length in 0..original.len() {
        assert!(inspect_wav(&original[..length]).is_err());
    }
    let mut bad = original.clone();
    bad[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(inspect_wav(&bad).is_err());
    let mut duplicate = original.clone();
    duplicate.extend_from_slice(&original[12..36]);
    let size = duplicate.len() as u32 - 8;
    duplicate[4..8].copy_from_slice(&size.to_le_bytes());
    assert!(inspect_wav(&duplicate).is_err());
    let mut partial = original;
    partial.push(0);
    let size = partial.len() as u32 - 8;
    partial[4..8].copy_from_slice(&size.to_le_bytes());
    partial[40..44].copy_from_slice(&9_u32.to_le_bytes());
    assert!(inspect_wav(&partial).is_err());
}
#[test]
fn decoder_rejects_nan_infinity_and_compressed_wav() {
    let mut bytes = write_float_wav(&mono(vec![0.0])).unwrap();
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        bytes[44..48].copy_from_slice(&value.to_le_bytes());
        assert!(inspect_wav(&bytes).is_err());
    }
    bytes[20..22].copy_from_slice(&17_u16.to_le_bytes());
    assert_eq!(
        inspect_wav(&bytes).unwrap_err().kind,
        ErrorKind::Unsupported
    );
}
#[test]
fn explicit_channel_selection_normalization_truncation_leave_original_unchanged() {
    let audio = WaveAudio {
        channels: 2,
        ..mono(vec![0.2, -0.4, 0.1, -0.2, 0.05, -0.1])
    };
    let original = audio.clone();
    let prepared = prepare_audio(
        &audio,
        &PrepareOptions {
            channel: ChannelMode::Select(1),
            normalize_peak: Some(0.5),
            truncate_frames: Some(2),
            ..Default::default()
        },
        || false,
    )
    .unwrap();
    assert_eq!(prepared.channels, 1);
    assert_eq!(prepared.samples, [-0.5, -0.25]);
    assert_eq!(audio, original);
    let average = prepare_audio(
        &audio,
        &PrepareOptions {
            channel: ChannelMode::Average,
            ..Default::default()
        },
        || false,
    )
    .unwrap();
    assert!((average.samples[0] + 0.1).abs() < 1e-6);
    assert!(
        prepare_audio(
            &audio,
            &PrepareOptions {
                channel: ChannelMode::Select(2),
                ..Default::default()
            },
            || false
        )
        .is_err()
    );
}
#[test]
fn zero_signal_normalization_and_invalid_options() {
    let audio = mono(vec![0.0; 16]);
    assert_eq!(
        prepare_audio(
            &audio,
            &PrepareOptions {
                normalize_peak: Some(0.8),
                ..Default::default()
            },
            || false
        )
        .unwrap()
        .samples,
        audio.samples
    );
    for peak in [0.0, -1.0, f32::NAN, f32::INFINITY, 1.1] {
        assert!(
            prepare_audio(
                &audio,
                &PrepareOptions {
                    normalize_peak: Some(peak),
                    ..Default::default()
                },
                || false
            )
            .is_err()
        );
    }
    assert!(
        PrepareOptions::from_options(&BTreeMap::from([("unsupported".into(), "1".into())]))
            .is_err()
    );
}
#[test]
fn resampling_keeps_duration_constant_and_suppresses_aliasing() {
    let constant = mono(vec![0.25; 2048]);
    let low = prepare_audio(
        &constant,
        &PrepareOptions {
            sample_rate: Some(16_000),
            ..Default::default()
        },
        || false,
    )
    .unwrap();
    assert_eq!(low.frames(), 683);
    assert_eq!(low.sample_rate, 16_000);
    assert!(low.samples.iter().all(|value| (*value - 0.25).abs() < 1e-6));
    let tone = mono(
        (0..4096)
            .map(|sample| (2.0 * std::f32::consts::PI * 10_000.0 * sample as f32 / 48_000.0).sin())
            .collect(),
    );
    let low = prepare_audio(
        &tone,
        &PrepareOptions {
            sample_rate: Some(16_000),
            ..Default::default()
        },
        || false,
    )
    .unwrap();
    let rms = (low.samples[64..low.samples.len() - 64]
        .iter()
        .map(|value| value.powi(2))
        .sum::<f32>()
        / (low.samples.len() - 128) as f32)
        .sqrt();
    assert!(rms < 0.005, "10kHz alias RMS={rms}");
}
#[test]
fn preparation_can_cancel_and_rejects_memory_expansion() {
    assert_eq!(
        prepare_audio(&mono(vec![0.0; 8192]), &PrepareOptions::default(), || true)
            .unwrap_err()
            .kind,
        ErrorKind::Cancelled
    );
    let mut audio = mono(vec![0.0; 1024]);
    audio.sample_rate = 1;
    assert!(
        prepare_audio(
            &audio,
            &PrepareOptions {
                sample_rate: Some(384_000),
                ..Default::default()
            },
            || false
        )
        .is_err()
    );
}
#[test]
fn store_deduplicates_retains_original_and_persists_derived_provenance() {
    let scratch = Scratch::new();
    let audio = mono(vec![0.0, 0.25, -0.25]);
    let path = source(&scratch, &audio);
    let input = fs::read(&path).unwrap();
    let root = scratch.0.join("store");
    let mut store = AssetStore::open(&root).unwrap();
    let original = store.inspect(&spec(1, path.clone())).unwrap();
    assert_eq!(
        store.inspect(&spec(2, path.clone())).unwrap().id,
        original.id
    );
    assert_eq!(store.inventory().len(), 1);
    let mut request = spec(3, path.clone());
    request
        .options
        .insert("normalize_peak".into(), "0.5".into());
    let result = store.prepare(&request, || false).unwrap();
    let derived = result.derived.unwrap();
    assert_ne!(original.id, derived.id);
    assert_eq!(fs::read(&path).unwrap(), input);
    assert_eq!(store.original_bytes(&derived.id).unwrap(), input);
    assert_eq!(
        store.manifests()[&derived.id].conversion_profile.as_deref(),
        Some("local-wav-f32-sinc-v1")
    );
    drop(store);
    let store = AssetStore::open(root).unwrap();
    assert_eq!(store.inventory().len(), 2);
    assert_eq!(
        inspect_wav(&store.content_bytes(&derived.id).unwrap())
            .unwrap()
            .samples,
        [0.0, 0.5, -0.5]
    );
}
#[test]
fn store_single_writer_and_corrupt_content_are_rejected() {
    let scratch = Scratch::new();
    let path = source(&scratch, &mono(vec![0.25]));
    let root = scratch.0.join("store");
    let mut store = AssetStore::open(&root).unwrap();
    assert!(matches!(
        AssetStore::open(&root),
        Err(ServiceError {
            kind: ErrorKind::Busy,
            ..
        })
    ));
    let asset = store.inspect(&spec(1, path)).unwrap();
    fs::write(&asset.path, [0, 1, 2]).unwrap();
    assert!(store.original_bytes(&asset.id).is_err());
}
#[test]
fn invalid_source_is_retained_for_diagnosis_and_never_prepared() {
    let scratch = Scratch::new();
    let path = scratch.0.join("broken.wav");
    fs::write(&path, b"not wave").unwrap();
    let mut store = AssetStore::open(scratch.0.join("store")).unwrap();
    assert!(store.inspect(&spec(1, path.clone())).is_err());
    assert_eq!(store.inventory().len(), 1);
    assert_eq!(store.inventory()[0].properties["validation"], "failed");
    assert!(store.prepare(&spec(2, path), || false).is_err());
    assert_eq!(store.inventory().len(), 1);
}
#[test]
fn output_export_cannot_replace_original_or_existing_file() {
    let scratch = Scratch::new();
    let path = source(&scratch, &mono(vec![0.25]));
    let bytes = fs::read(&path).unwrap();
    let mut store = AssetStore::open(scratch.0.join("store")).unwrap();
    let mut request = spec(1, path.clone());
    request.output = Some(path.clone());
    assert!(store.prepare(&request, || false).is_err());
    assert_eq!(fs::read(&path).unwrap(), bytes);
    let output = scratch.0.join("prepared.wav");
    request.output = Some(output.clone());
    store.prepare(&request, || false).unwrap();
    let exported = fs::read(&output).unwrap();
    assert!(store.prepare(&request, || false).is_err());
    assert_eq!(fs::read(output).unwrap(), exported);
}
#[test]
fn nam_diagnosis_never_substitutes_an_unvalidated_network() {
    let bytes = br#"{"version":"0.5.4","architecture":"WaveNet","config":{"layers":[]},"sample_rate":48000,"weights":[0.1,0.2]}"#;
    let info = inspect_nam(bytes).unwrap();
    assert_eq!(info.weight_count, 2);
    assert!(!info.supported_inference);
    assert_eq!(
        NamModel::load(bytes).unwrap_err().kind,
        ErrorKind::Unverified
    );
    for bytes in [
        b"{}".as_slice(),
        br#"{"version":"0.5.4","architecture":"WaveNet","config":{},"weights":[null]}"#,
        br#"{"version":"0.5.4","architecture":"WaveNet","config":{},"weights":[],"sample_rate":0}"#,
    ] {
        assert!(inspect_nam(bytes).is_err());
    }
}
#[test]
fn upstream_lstm_sample_matches_independent_eigen_reference() {
    let model_bytes = include_bytes!("../../../tests/fixtures/assets/upstream-lstm-0.5.4.nam");
    let reference: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../tests/fixtures/assets/upstream-lstm-reference.json"
    ))
    .unwrap();
    let input: Vec<f32> = reference["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_f64().unwrap() as f32)
        .collect();
    assert!(inspect_nam(model_bytes).unwrap().supported_inference);
    for (mode, key) in [
        (ActivationMode::Standard, "standard"),
        (ActivationMode::Fast, "fast"),
    ] {
        let mut model = NamModel::load(model_bytes).unwrap();
        model.set_activation(mode);
        model.reset().unwrap();
        let mut output = vec![0.0; input.len()];
        model.process(&input, &mut output).unwrap();
        for (index, (actual, expected)) in output
            .iter()
            .zip(reference[key].as_array().unwrap())
            .enumerate()
        {
            let expected = expected.as_f64().unwrap() as f32;
            assert!(
                (actual - expected).abs() <= 1e-6,
                "{key} sample{index}: {actual} vs {expected}"
            );
        }
        model.reset().unwrap();
        let mut second = vec![0.0; input.len()];
        model.process(&input, &mut second).unwrap();
        assert_eq!(output, second);
    }
}
#[test]
fn analytic_lstm_fixture_proves_gate_order_and_exported_state_layout() {
    let mut weights = vec![0.0_f32; 16];
    weights[4] = 1.0;
    weights[14] = 1.0;
    let model = serde_json::json!({"version":"0.5.4","architecture":"LSTM","config":{"input_size":1,"hidden_size":1,"num_layers":1},"weights":weights,"sample_rate":48000});
    let mut model = NamModel::load(&serde_json::to_vec(&model).unwrap()).unwrap();
    model.reset().unwrap();
    let mut output = [0.0];
    model.process(&[0.4], &mut output).unwrap();
    let expected = 0.5 * (0.5 * 0.4_f32.tanh()).tanh();
    assert!((output[0] - expected).abs() < 1e-7);
}
#[test]
fn nam_local_render_retains_model_and_input_but_emits_no_snaptone() {
    let scratch = Scratch::new();
    let path = scratch.0.join("model.nam");
    let nam = include_bytes!("../../../tests/fixtures/assets/upstream-lstm-0.5.4.nam");
    fs::write(&path, nam).unwrap();
    let input = source(&scratch, &mono(vec![0.0, 0.1, -0.1, 0.5]));
    let original_input = fs::read(&input).unwrap();
    let mut request = spec(1, path.clone());
    request.kind = AssetKind::Nam;
    request
        .options
        .insert("input_wav".into(), input.to_string_lossy().into_owned());
    let mut store = AssetStore::open(scratch.0.join("store")).unwrap();
    let result = store.prepare(&request, || false).unwrap();
    let output = result.derived.unwrap();
    assert_eq!(output.kind, AssetKind::Nam);
    assert_eq!(output.properties["device_compatible"], "unknown");
    assert_eq!(store.original_bytes(&output.id).unwrap(), nam);
    assert_eq!(
        store.dependency_bytes(&output.id, "input_wav").unwrap(),
        original_input
    );
    assert_eq!(fs::read(path).unwrap(), nam);
    let audio = inspect_wav(&store.content_bytes(&output.id).unwrap()).unwrap();
    assert_eq!(audio.channels, 2);
    assert_eq!(audio.source_bits, 16);
    assert_eq!(audio.frames(), 4);
    assert!(audio.samples.iter().all(|value| value.abs() <= 0.31));
}
#[test]
fn valid_modern_sequential_and_fractional_rates_are_inspected_but_unsupported() {
    let sequential=br#"{"version":"0.7.0","architecture":"Sequential","config":{"models":[]},"weights":[],"sample_rate":48000}"#;
    assert!(!inspect_nam(sequential).unwrap().supported_inference);
    let fractional=br#"{"version":"0.5.4","architecture":"LSTM","config":{"input_size":1,"hidden_size":1,"num_layers":1},"weights":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"sample_rate":48000.5}"#;
    assert_eq!(inspect_nam(fractional).unwrap().sample_rate, Some(48000.5));
    assert_eq!(
        NamModel::load(fractional).unwrap_err().kind,
        ErrorKind::Unverified
    );
}
struct GainProcessor {
    rate: u32,
    calls: Vec<usize>,
    reset: bool,
    gain: f32,
}
impl SampleProcessor for GainProcessor {
    fn sample_rate(&self) -> u32 {
        self.rate
    }
    fn reset(&mut self) -> Result<(), ServiceError> {
        self.reset = true;
        Ok(())
    }
    fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), ServiceError> {
        assert!(self.reset);
        self.calls.push(input.len());
        for (input, output) in input.iter().zip(output) {
            *output = *input * self.gain;
        }
        Ok(())
    }
}
#[test]
fn evidenced_host_stage_uses_1024_blocks_clamps_scales_and_duplicates_stereo() {
    let mut processor = GainProcessor {
        rate: 48_000,
        calls: vec![],
        reset: false,
        gain: 2.0,
    };
    let audio = mono(vec![0.75; 2051]);
    let mut progress = vec![];
    let (output, report) = render_host_stage(
        &audio,
        &mut processor,
        || false,
        |done, total| progress.push((done, total)),
    )
    .unwrap();
    assert_eq!(processor.calls, [1024, 1024, 3]);
    assert_eq!(report.clamped_samples, 2051);
    assert_eq!(output.channels, 2);
    assert!(
        output
            .samples
            .iter()
            .all(|value| value.to_bits() == 0.31_f32.to_bits())
    );
    assert_eq!(progress.last(), Some(&(2051, 2051)));
    let decoded = inspect_wav(&write_pcm16_wav(&output).unwrap()).unwrap();
    assert_eq!(decoded.channels, 2);
    assert_eq!(decoded.frames(), 2051);
    assert!((decoded.samples[0] - 0.31).abs() <= 1.0 / 32768.0);
}
#[test]
fn host_stage_rejects_rate_mismatch_nonfinite_processor_and_cancellation() {
    let mut processor = GainProcessor {
        rate: 44_100,
        calls: vec![],
        reset: false,
        gain: 1.0,
    };
    assert_eq!(
        render_host_stage(&mono(vec![1.0]), &mut processor, || false, |_, _| {})
            .unwrap_err()
            .kind,
        ErrorKind::Unverified
    );
    processor.rate = 48_000;
    processor.gain = f32::NAN;
    assert!(render_host_stage(&mono(vec![1.0]), &mut processor, || false, |_, _| {}).is_err());
    assert_eq!(
        render_host_stage(&mono(vec![1.0]), &mut processor, || true, |_, _| {})
            .unwrap_err()
            .kind,
        ErrorKind::Cancelled
    );
}
fn await_terminal(worker: &AssetWorker, job: JobId) -> Vec<AssetEvent> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut events = vec![];
    while Instant::now() < deadline {
        if let Some(event) = worker.try_next_event() {
            let terminal = matches!(&event,AssetEvent::Inspected{job:id,..}|AssetEvent::Prepared{job:id,..}|AssetEvent::Failed{job:id,..} if *id==job);
            events.push(event);
            if terminal {
                return events;
            }
        } else {
            thread::sleep(Duration::from_millis(2));
        }
    }
    panic!("No asset terminal event before deadline")
}
#[test]
fn worker_prepares_and_never_emits_device_ticket() {
    let scratch = Scratch::new();
    let path = source(&scratch, &mono(vec![0.0, 0.25]));
    let worker = AssetWorker::open(scratch.0.join("store"), no_wake()).unwrap();
    worker
        .try_submit(AssetCommand::Prepare(spec(7, path)))
        .unwrap();
    let events = await_terminal(&worker, JobId(7));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AssetEvent::Prepared { .. }))
    );
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, AssetEvent::DeviceJobPrepared(_)))
    );
}
#[test]
fn cancelled_job_and_shutdown_return_explicit_outcomes() {
    let scratch = Scratch::new();
    let path = source(&scratch, &mono(vec![0.0; 4096]));
    let worker = AssetWorker::open(scratch.0.join("store"), no_wake()).unwrap();
    worker.try_submit(AssetCommand::Cancel(JobId(9))).unwrap();
    worker
        .try_submit(AssetCommand::Prepare(spec(9, path)))
        .unwrap();
    let events = await_terminal(&worker, JobId(9));
    assert!(events.iter().any(
        |event| matches!(event,AssetEvent::Failed{error,..} if error.kind==ErrorKind::Cancelled)
    ));
    worker.shutdown();
    assert!(matches!(
        worker.try_submit(AssetCommand::List),
        Err(SubmitError::Disconnected(_))
    ));
}
#[test]
fn interrupted_journal_is_marked_and_never_resumed_automatically() {
    let scratch = Scratch::new();
    let root = scratch.0.join("store");
    let store = AssetStore::open(&root).unwrap();
    drop(store);
    let journal = root.join("jobs/interrupted.json");
    fs::write(
        &journal,
        br#"{"schema_version":1,"state":"converting_locally","pedal_transfer_started":false}"#,
    )
    .unwrap();
    let store = AssetStore::open(root).unwrap();
    assert!(store.inventory().is_empty());
    let value: serde_json::Value = serde_json::from_slice(&fs::read(journal).unwrap()).unwrap();
    assert_eq!(value["state"], "interrupted_local");
    assert_eq!(value["pedal_transfer_started"], false);
}
#[cfg(unix)]
#[test]
fn retained_content_symlinks_are_rejected() {
    let scratch = Scratch::new();
    let path = source(&scratch, &mono(vec![0.0]));
    let mut store = AssetStore::open(scratch.0.join("store")).unwrap();
    let info = store.inspect(&spec(1, path.clone())).unwrap();
    fs::remove_file(&info.path).unwrap();
    std::os::unix::fs::symlink(path, &info.path).unwrap();
    assert!(store.content_bytes(&info.id).is_err());
}
