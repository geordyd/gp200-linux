use gp200_assets::*;
use gp200_domain::{AssetJobSpec, AssetKind, JobId};
use std::{collections::BTreeMap, time::Duration};
const ORIGINAL: &[u8] =
    include_bytes!("../../../tests/fixtures/assets/clo/upstream-source2048.clo");
const FINAL: &[u8] = include_bytes!("../../../tests/fixtures/assets/clo/upstream-final1024.clo");
#[test]
fn independently_generated_destination_golden_preserves_core_and_uses_declared_crc() {
    let original = inspect_snaptone(ORIGINAL).unwrap();
    let final_clo = inspect_snaptone(FINAL).unwrap();
    assert_eq!(original.info().declared_bytes, CLO_BYTES);
    assert_eq!(final_clo.info().declared_bytes, GP200_CLO_DECLARED_BYTES);
    assert_eq!(
        final_clo.info().coefficient_bytes,
        GP200_CLO_COEFFICIENT_BYTES
    );
    assert_eq!(
        final_clo.coefficients(0).unwrap(),
        original.coefficients(0).unwrap()
    );
    assert_eq!(final_clo.info().metadata_raw, original.info().metadata_raw);
    assert_eq!(
        final_clo.info().calibration_words,
        original.info().calibration_words
    );
    assert_eq!(final_clo.coefficients(1).unwrap().len(), 1024);
    assert_eq!(
        clo_crc16(&FINAL[12..GP200_CLO_DECLARED_BYTES]),
        final_clo.info().crc16
    );
    assert!(
        FINAL[GP200_CLO_DECLARED_BYTES..]
            .iter()
            .all(|byte| *byte == 0)
    );
    assert!(
        final_clo
            .coefficients(1)
            .unwrap()
            .iter()
            .all(|x| x.as_f32().is_finite())
    );
}
#[test]
fn padded_profile_rejects_corruption_padding_guessed_lengths_and_ranges() {
    for offset in [0, 4, 20, 120, 124, 128, 132, 136, 4743, 4744, 8839] {
        let mut bytes = FINAL.to_vec();
        bytes[offset] ^= 1;
        assert!(inspect_snaptone(&bytes).is_err(), "offset {offset}");
    }
    let mut wrong = FINAL.to_vec();
    wrong[132..136].copy_from_slice(&512_u32.to_le_bytes());
    let crc = clo_crc16(&wrong[12..GP200_CLO_DECLARED_BYTES]);
    wrong[8..10].copy_from_slice(&crc.to_be_bytes());
    assert!(inspect_snaptone(&wrong).is_err());
    assert!(inspect_snaptone(&FINAL[..GP200_CLO_DECLARED_BYTES]).is_err());
}
#[test]
fn unknown_audio_and_unbounded_timeout_are_explicit_errors_without_helper_work() {
    let stimulus = WaveAudio {
        sample_rate: 44100,
        channels: 1,
        source_encoding: WaveEncoding::Pcm,
        source_bits: 16,
        samples: vec![0.0; 70 * 44100],
    };
    let root = std::env::temp_dir();
    let error = calibrate_nam_to_clo(
        b"{}",
        &stimulus,
        &CalibrationOptions::default(),
        &root,
        || false,
        |_, _| {},
    )
    .err()
    .unwrap();
    assert!(error.message.contains("verified GP-200"));
    let options = CalibrationOptions {
        timeout: Duration::from_secs(901),
        ..Default::default()
    };
    let error = calibrate_nam_to_clo(b"{}", &stimulus, &options, &root, || false, |_, _| {})
        .err()
        .unwrap();
    assert!(error.message.contains("900"));
}
#[test]
fn store_rejects_unknown_stimulus_but_retains_selected_inputs() {
    let root = std::env::temp_dir().join(format!("gp200-calibration-store-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let source = root.join("model.nam");
    let stimulus = root.join("unknown.wav");
    std::fs::write(
        &source,
        include_bytes!("../../../tests/fixtures/assets/upstream-lstm-0.5.4.nam"),
    )
    .unwrap();
    std::fs::write(&stimulus, b"unknown").unwrap();
    let mut store = AssetStore::open(root.join("store")).unwrap();
    let spec = AssetJobSpec {
        id: JobId(1),
        kind: AssetKind::Nam,
        source,
        output: None,
        options: BTreeMap::from([
            ("output_format".into(), "clo".into()),
            ("input_wav".into(), stimulus.to_string_lossy().into_owned()),
            ("input_channel".into(), "0".into()),
        ]),
    };
    let error = store.prepare(&spec, || false).unwrap_err();
    assert!(error.message.contains("does not match"));
    assert_eq!(store.inventory().len(), 1);
    assert!(root.join("store/originals").is_dir());
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}
