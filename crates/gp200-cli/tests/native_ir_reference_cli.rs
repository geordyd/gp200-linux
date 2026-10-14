//! Offline process checks. All mode 2 carriers here are synthetic, not native
//! interoperability goldens; no command constructs a hardware backend.
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

const MODE1: &[u8] = include_bytes!("../../../tests/fixtures/native/constructed-mode1.prst");
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "gp200-cli-ir-reference-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn inspect(&self, bytes: &[u8]) -> Value {
        let input = self.0.join("synthetic.prst");
        fs::write(&input, bytes).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_gp200-cli"))
            .args(["inspect", "prst", "--input"])
            .arg(&input)
            .current_dir(&self.0)
            .env_clear()
            .env("HOME", &self.0)
            .env("GP200_STUDIO_DATA", self.0.join("data"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["schema"], 1);
        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["hardware_access"], false);
        assert!(!self.0.join(".hardware-session.lock").exists());
        assert!(!self.0.join("data").exists());
        assert_eq!(fs::read(input).unwrap(), bytes);
        value["data"].clone()
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn synthetic_mode2(carrier: &[u8]) -> Vec<u8> {
    let mut bytes = MODE1[..32].to_vec();
    bytes[22..24].copy_from_slice(&2u16.to_le_bytes());
    let length = u32::try_from(carrier.len()).unwrap();
    let preset_offset = 56 + length;
    bytes.extend_from_slice(&48u32.to_le_bytes());
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(&preset_offset.to_le_bytes());
    bytes.extend_from_slice(&u32::try_from(MODE1.len() - 52).unwrap().to_le_bytes());
    bytes.extend_from_slice(b"RISU");
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(carrier);
    bytes.extend_from_slice(&MODE1[40..MODE1.len() - 4]);
    let final_word = bytes.len() - 4;
    bytes[final_word..].copy_from_slice(&u32::try_from(final_word).unwrap().to_le_bytes());
    let sum: u32 = bytes.iter().map(|byte| u32::from(*byte)).sum();
    bytes.extend_from_slice(&sum.to_be_bytes());
    bytes
}
fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}
fn inspect_path(temp: &Temp, path: &Path) -> Value {
    let bytes = fs::read(path).unwrap();
    let result = temp.inspect(&bytes);
    assert_eq!(result["sha256"], sha256(&bytes));
    result
}

#[test]
fn original_json_keys_stay_compatible_and_unknown_synthetic_risu_stays_opaque() {
    let temp = Temp::new();
    let mode1 = temp.inspect(MODE1);
    assert_eq!(mode1["details"]["mode"], 1);
    assert!(mode1["details"]["embedded_ir_bytes"].is_null());
    assert!(mode1["details"]["ir_reference_inspection"].is_null());
    for carrier in [vec![0xa5; 57], vec![0; 32]] {
        let bytes = synthetic_mode2(&carrier);
        let data = temp.inspect(&bytes);
        assert_eq!(data["sha256"], sha256(&bytes));
        assert_eq!(data["bytes"], bytes.len());
        assert_eq!(data["details"]["mode"], 2);
        assert_eq!(data["details"]["embedded_ir_bytes"], carrier.len());
        let description = &data["details"]["ir_reference_inspection"];
        assert_eq!(description["kind"], "opaque");
        assert_eq!(description["details"]["byte_count"], carrier.len());
        assert!(description["details"]["ir_coefficients_present"].is_null());
        assert!(
            data["details"]["embedded_ir_bytes_note"]
                .as_str()
                .unwrap()
                .contains("not a count of recovered IR audio")
        );
    }
}

#[test]
#[ignore = "requires a separately obtained mode 1 native export via GP200_NATIVE_INTEROP_FIXTURE"]
fn independent_mode1_export_stays_source_hash_identical() {
    let source = PathBuf::from(
        std::env::var_os("GP200_NATIVE_INTEROP_FIXTURE")
            .expect("Explicit interoperability test requires GP200_NATIVE_INTEROP_FIXTURE"),
    );
    let data = inspect_path(&Temp::new(), &source);
    assert_eq!(data["details"]["mode"], 1);
    assert!(data["details"]["ir_reference_inspection"].is_null());
}

#[test]
fn synthetic_metadata_json_separates_integrity_candidates_from_content_authority() {
    let temp = Temp::new();
    let mut carrier = [0; 32];
    carrier[..2].copy_from_slice(&0x1009u16.to_le_bytes());
    carrier[2..4].copy_from_slice(&24u16.to_le_bytes());
    carrier[4..6].copy_from_slice(&19u16.to_le_bytes());
    carrier[6..8].copy_from_slice(&u16::MAX.to_le_bytes());
    carrier[12..28].copy_from_slice(b"Raw~\0tail\0\0\0\0\0\0\0");
    for trailer in [0u32, gp200_library::native_blob_crc32(&carrier[..28])] {
        carrier[28..].copy_from_slice(&trailer.to_be_bytes());
        let bytes = synthetic_mode2(&carrier);
        let data = temp.inspect(&bytes);
        assert_eq!(data["details"]["embedded_ir_bytes"], 32);
        let description = &data["details"]["ir_reference_inspection"];
        assert_eq!(description["kind"], "metadata_reference");
        let reference = &description["details"];
        assert_eq!(reference["carrier_sha256"], sha256(&carrier));
        assert_eq!(reference["slot_word"], 19);
        assert_eq!(reference["flags_raw"], u16::MAX);
        assert_eq!(reference["checksum_raw"], 0);
        assert_eq!(
            reference["name_raw_hex"],
            "5261777e007461696c00000000000000"
        );
        assert_eq!(reference["name_native_ascii_display"], "Raw");
        assert_eq!(reference["physical_occupancy"], "unknown");
        assert_eq!(reference["metadata_only"], true);
        for field in [
            "ir_coefficients_present",
            "coefficient_identity_proven",
            "complete_asset_backup",
            "device_write_authority",
            "native_mode2_interoperability_verified",
        ] {
            assert_eq!(reference[field], false, "{field}");
        }
        assert_eq!(reference["integrity_candidates"]["trailer_be32"], trailer);
        assert_eq!(
            reference["integrity_candidates"]["matches_serializer_crc32"],
            trailer != 0
        );
        assert_eq!(
            reference["integrity_candidates"]["matches_multi_import_additive32"],
            false
        );
    }
}
