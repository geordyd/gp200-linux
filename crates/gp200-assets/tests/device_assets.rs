use gp200_assets::*;
use gp200_domain::{AssetJobSpec, AssetKind, DeviceModel, F32Bits, JobId};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

const FIRMWARE: &[u8] =
    include_bytes!("../../../tests/fixtures/device-assets/synthetic-firmware.bin");
const CLO: &[u8] =
    include_bytes!("../../../tests/fixtures/device-assets/synthetic-calibration.clo");
const IR_WAV: &[u8] =
    include_bytes!("../../../tests/fixtures/device-assets/synthetic-ir-pcm24.wav");
const IR_PACKED: &[u8] =
    include_bytes!("../../../tests/fixtures/device-assets/synthetic-ir-coefficients.bin");
fn checksum_firmware(bytes: &mut [u8]) {
    bytes[14] = bytes[48..]
        .iter()
        .fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
}
fn checksum_clo(bytes: &mut [u8]) {
    let checksum = clo_crc16(&bytes[12..]);
    bytes[8..10].copy_from_slice(&checksum.to_be_bytes());
}
#[test]
fn firmware_preserves_unknown_header_bytes_and_each_section() {
    let image = inspect_firmware(FIRMWARE).unwrap();
    assert_eq!(image.raw(), FIRMWARE);
    let info = image.info();
    assert_eq!(info.model, DeviceModel::Gp200);
    assert_eq!(info.image_version.as_deref(), Some("1.8.0"));
    assert_eq!(info.header_word_raw, 0xcafe);
    assert_eq!(info.checksum_word_raw >> 8, 0xab);
    assert_eq!(info.metadata_raw, std::array::from_fn(|index| index as u8));
    assert_eq!(info.sections[0].file_offset, 80);
    assert_eq!(image.section(3).unwrap(), &FIRMWARE[144..176]);
    assert!(image.section(4).is_none());
}
#[test]
fn firmware_rejects_corruption_length_wrong_product_and_bad_ranges() {
    for offset in [0, 4, 16, 48, 100] {
        let mut broken = FIRMWARE.to_vec();
        broken[offset] ^= 1;
        assert!(inspect_firmware(&broken).is_err(), "offset {offset}");
    }
    for (offset, value) in [(48, 0_u32), (52, u32::MAX), (56, 32_u32), (60, 0_u32)] {
        let mut broken = FIRMWARE.to_vec();
        broken[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        checksum_firmware(&mut broken);
        assert!(inspect_firmware(&broken).is_err(), "range offset {offset}");
    }
    for length in 0..92 {
        assert!(inspect_firmware(&FIRMWARE[..length]).is_err());
    }
}
#[test]
fn firmware_product_model_is_exact_and_version_tag_is_not_device_version() {
    for (name, model) in [
        (b"GP-200LT".as_slice(), DeviceModel::Gp200Lt),
        (b"GP-200JR".as_slice(), DeviceModel::Gp200Jr),
    ] {
        let mut raw = FIRMWARE.to_vec();
        raw[16..32].fill(0);
        raw[16..16 + name.len()].copy_from_slice(name);
        assert_eq!(inspect_firmware(&raw).unwrap().info().model, model);
        raw[16 + name.len()] = b'X';
        assert!(inspect_firmware(&raw).is_err());
    }
    let mut unknown_version = FIRMWARE.to_vec();
    unknown_version[88..92].copy_from_slice(b"V1x0");
    checksum_firmware(&mut unknown_version);
    assert_eq!(
        inspect_firmware(&unknown_version)
            .unwrap()
            .info()
            .image_version,
        None
    );
}
#[test]
fn native_crc_tables_and_reference_fixture_match_bitwise_crc() {
    let tables = include_bytes!("../../../tests/fixtures/device-assets/clo-crc-tables.bin");
    for byte in 0..256_u16 {
        let mut crc = byte;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xa001
            } else {
                crc >> 1
            };
        }
        assert_eq!(tables[byte as usize], crc as u8);
        assert_eq!(tables[256 + byte as usize], (crc >> 8) as u8);
    }
    assert_eq!(clo_crc16(b"123456789"), 0x4b37);
    assert_eq!(clo_crc16(&CLO[12..]), u16::from_be_bytes([CLO[8], CLO[9]]));
}
#[test]
fn clo_preserves_nan_payloads_signed_zero_unknowns_and_ranges() {
    let container = inspect_snaptone(CLO).unwrap();
    assert_eq!(container.raw(), CLO);
    assert_eq!(
        container.info().calibration_words,
        [
            F32Bits(0x7fc12345),
            F32Bits(0x80000000),
            F32Bits(1),
            F32Bits(0x3f800000)
        ]
    );
    assert_eq!(
        container.coefficients(0).unwrap(),
        [
            F32Bits(0x7fc98765),
            F32Bits(0x80000000),
            F32Bits(0x3f800000)
        ]
    );
    assert_eq!(
        container.coefficients(1).unwrap(),
        [F32Bits(1), F32Bits(0xbf800000)]
    );
    assert!(container.coefficients(2).is_none());
}
#[test]
fn clo_rejects_crc_length_overflow_and_overlap_without_panics() {
    for offset in [0, 4, 8, 20, 24, 8839] {
        let mut broken = CLO.to_vec();
        broken[offset] ^= 1;
        assert!(inspect_snaptone(&broken).is_err());
    }
    for (offset, value) in [
        (120, u32::MAX),
        (124, 2177_u32),
        (128, 2_u32),
        (132, u32::MAX),
    ] {
        let mut broken = CLO.to_vec();
        broken[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        checksum_clo(&mut broken);
        assert!(inspect_snaptone(&broken).is_err(), "offset {offset}");
    }
    for length in [0, 1, 4, 136, 8839] {
        assert!(inspect_snaptone(&CLO[..length]).is_err());
    }
}
#[test]
fn ir_packing_matches_independent_signed24_oracle_without_resampling() {
    let packed = pack_device_ir(IR_WAV, IrLengthPolicy::RejectTruncation).unwrap();
    assert_eq!(packed.raw().as_slice(), IR_PACKED);
    assert_eq!(packed.info().source_samples, 5);
    assert_eq!(packed.info().padded_samples, 1019);
    assert_eq!(packed.info().truncated_samples, 0);
    assert_eq!(
        packed.info().byte_sum,
        IR_PACKED.iter().map(|byte| u32::from(*byte)).sum::<u32>()
    );
    let mut wrong_rate = IR_WAV.to_vec();
    wrong_rate[24..28].copy_from_slice(&48000_u32.to_le_bytes());
    wrong_rate[28..32].copy_from_slice(&(48000_u32 * 3).to_le_bytes());
    assert!(pack_device_ir(&wrong_rate, IrLengthPolicy::RejectTruncation).is_err());
}
#[test]
fn ir_truncation_requires_explicit_choice() {
    let mut long = IR_WAV[..44].to_vec();
    long.extend(std::iter::repeat_n(1_u8, 1025 * 3));
    long.push(0);
    let riff_length = (long.len() - 8) as u32;
    long[4..8].copy_from_slice(&riff_length.to_le_bytes());
    long[40..44].copy_from_slice(&(1025_u32 * 3).to_le_bytes());
    assert!(pack_device_ir(&long, IrLengthPolicy::RejectTruncation).is_err());
    let packed = pack_device_ir(&long, IrLengthPolicy::KeepFirst1024).unwrap();
    assert_eq!(packed.info().truncated_samples, 1);
    assert_eq!(packed.info().retained_samples, 1024);
    assert_eq!(packed.info().padded_samples, 0);
}

#[test]
fn store_native_inspectors_emit_real_metadata_and_keep_corrupt_original() {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let scratch = std::env::temp_dir().join(format!(
        "gp200-device-assets-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&scratch).unwrap();
    let mut store = AssetStore::open(scratch.join("store")).unwrap();
    let firmware_path = scratch.join("image.bin");
    std::fs::write(&firmware_path, FIRMWARE).unwrap();
    let mut spec = AssetJobSpec {
        id: JobId(1),
        kind: AssetKind::Firmware,
        source: firmware_path.clone(),
        output: None,
        options: BTreeMap::new(),
    };
    let inspected = store.inspect(&spec).unwrap();
    assert_eq!(inspected.properties["container_format"], "FRMW-native");
    assert_eq!(inspected.properties["firmware_image_version"], "1.8.0");
    assert_eq!(inspected.properties["device_compatible"], "unknown");
    assert_eq!(store.original_bytes(&inspected.id).unwrap(), FIRMWARE);
    spec.kind = AssetKind::SnapTone;
    spec.source = scratch.join("tone.clo");
    std::fs::write(&spec.source, CLO).unwrap();
    let inspected = store.inspect(&spec).unwrap();
    assert_eq!(inspected.properties["container_format"], "VTSI-clo-native");
    assert_eq!(inspected.properties["integrity"], "crc16-modbus-be-valid");
    assert_eq!(store.original_bytes(&inspected.id).unwrap(), CLO);
    spec.kind = AssetKind::Firmware;
    spec.source = firmware_path;
    let mut corrupt = FIRMWARE.to_vec();
    corrupt[0] = b'X';
    std::fs::write(&spec.source, &corrupt).unwrap();
    assert!(store.inspect(&spec).is_err());
    let retained = store
        .inventory()
        .into_iter()
        .find(|info| {
            info.properties
                .get("validation")
                .is_some_and(|value| value == "failed")
        })
        .unwrap();
    assert_eq!(store.original_bytes(&retained.id).unwrap(), corrupt);
    assert_eq!(retained.properties["source_retained"], "true");
    drop(store);
    std::fs::remove_dir_all(scratch).unwrap();
}
