//! Synthetic metadata carrier boundaries and lossless container retention.
//! These fixtures are not original-editor mode 2 compatibility evidence.
use gp200_domain::{CompatibilityLevel, NativeExportMode, NativeExportRequest};
use gp200_library::*;
use gp200_protocol::RawTransfer;

const MODE1: &[u8] = include_bytes!("../../../tests/fixtures/native/constructed-mode1.prst");
const OLD_SYNTHETIC_MODE2: &[u8] =
    include_bytes!("../../../tests/fixtures/native/constructed-mode2.prst");
const NAME: [u8; 16] = [
    0x20, b'A', 0x7d, 0x1f, 0x7e, 0xff, b'B', 0, b't', b'a', b'i', b'l', 0, 0, 0, 0,
];

fn synthetic_carrier() -> [u8; 32] {
    let mut bytes = [0; 32];
    bytes[..2].copy_from_slice(&0x1009u16.to_le_bytes());
    bytes[2..4].copy_from_slice(&24u16.to_le_bytes());
    bytes[6..8].copy_from_slice(&u16::MAX.to_le_bytes());
    bytes[12..28].copy_from_slice(&NAME);
    bytes
}
fn described(bytes: &[u8]) -> NativeIrMetadataReference {
    match inspect_native_ir_reference(bytes) {
        NativeIrReferenceInspection::MetadataReference(reference) => reference,
        other => panic!("Expected metadata description, got {other:?}"),
    }
}
fn assert_no_content_authority(reference: &NativeIrMetadataReference) {
    assert!(reference.metadata_only);
    assert!(!reference.ir_coefficients_present);
    assert_eq!(
        reference.physical_occupancy,
        NativeIrReferenceOccupancy::Unknown
    );
    assert!(!reference.coefficient_identity_proven);
    assert!(!reference.complete_asset_backup);
    assert!(!reference.device_write_authority);
    assert!(!reference.native_mode2_interoperability_verified);
}
fn synthetic_mode2(carrier: &[u8], user_ir: bool) -> Vec<u8> {
    let mut header = MODE1[..32].to_vec();
    header[22..24].copy_from_slice(&2u16.to_le_bytes());
    let ir_offset = 48u32;
    let ir_length = u32::try_from(carrier.len()).unwrap();
    let preset_offset = ir_offset + 8 + ir_length;
    let preset_length = u32::try_from(MODE1.len() - 52).unwrap();
    header.extend_from_slice(&ir_offset.to_le_bytes());
    header.extend_from_slice(&ir_length.to_le_bytes());
    header.extend_from_slice(&preset_offset.to_le_bytes());
    header.extend_from_slice(&preset_length.to_le_bytes());
    header.extend_from_slice(b"RISU");
    header.extend_from_slice(&ir_length.to_le_bytes());
    header.extend_from_slice(carrier);
    header.extend_from_slice(&MODE1[40..MODE1.len() - 4]);
    if user_ir {
        let offset = usize::try_from(preset_offset).unwrap() + 8;
        let end = header.len() - 4;
        let raw = RawTransfer::from_payload(0x12, header[offset..end].to_vec()).unwrap();
        let mut records = raw.records().to_vec();
        records
            .iter_mut()
            .find(|record| record.command == 0x14 && record.body[0] == 5)
            .unwrap()
            .body[4..8]
            .copy_from_slice(&0x0a10_0000u32.to_le_bytes());
        let raw = RawTransfer::from_records(0x12, &records).unwrap();
        header[offset..end].copy_from_slice(raw.payload());
    }
    let final_word = header.len() - 4;
    header[final_word..].copy_from_slice(&u32::try_from(final_word).unwrap().to_le_bytes());
    let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
    header.extend_from_slice(&sum.to_be_bytes());
    header
}

#[test]
fn synthetic_structural_boundaries_are_opaque_without_rejecting_outer_containers() {
    let original = synthetic_carrier();
    for length in [0, 1, 27, 28, 31, 33, 64] {
        let bytes = vec![0; length];
        assert_eq!(
            inspect_native_ir_reference(&bytes),
            NativeIrReferenceInspection::Opaque {
                byte_count: length,
                reason: NativeIrOpaqueReason::UnexpectedLength
            }
        );
    }
    for (range, value, reason) in [
        (0..2, 0x1012u16, NativeIrOpaqueReason::UnexpectedCommand),
        (2..4, 23, NativeIrOpaqueReason::UnexpectedBodyLength),
        (4..6, 20, NativeIrOpaqueReason::UnresolvedSlot),
        (4..6, u16::MAX, NativeIrOpaqueReason::UnresolvedSlot),
    ] {
        let mut bytes = original;
        bytes[range].copy_from_slice(&value.to_le_bytes());
        assert_eq!(
            inspect_native_ir_reference(&bytes),
            NativeIrReferenceInspection::Opaque {
                byte_count: 32,
                reason
            }
        );
        let outer = synthetic_mode2(&bytes, false);
        let file = NativeFile::parse(&outer).unwrap();
        assert_eq!(file.embedded_ir_bytes(), Some(bytes.as_slice()));
        assert_eq!(file.preserve_with_raw(file.raw()).unwrap(), outer);
    }
    for slot in [0u16, 19] {
        let mut bytes = original;
        bytes[4..6].copy_from_slice(&slot.to_le_bytes());
        assert_eq!(described(&bytes).slot_word, slot);
    }
    let file = NativeFile::parse(MODE1).unwrap();
    assert!(file.ir_reference_inspection().is_none());
    let old = NativeFile::parse(OLD_SYNTHETIC_MODE2).unwrap();
    assert!(matches!(
        old.ir_reference_inspection(),
        Some(NativeIrReferenceInspection::Opaque { .. })
    ));
    assert_eq!(
        old.preserve_with_raw(old.raw()).unwrap(),
        OLD_SYNTHETIC_MODE2
    );
}

#[test]
fn synthetic_trailers_report_separate_candidates_without_native_validity() {
    let original = synthetic_carrier();
    // Independently calculated with Python zlib.crc32 and sum over 28 bytes.
    let expected_crc = 0x926f_142eu32;
    let expected_sum = 1685u32;
    for (trailer, crc_matches, sum_matches) in [
        (expected_crc, true, false),
        (expected_sum, false, true),
        (0, false, false),
    ] {
        let mut bytes = original;
        bytes[28..].copy_from_slice(&trailer.to_be_bytes());
        let reference = described(&bytes);
        let integrity = &reference.integrity_candidates;
        assert_eq!(integrity.trailer_be32, trailer);
        assert_eq!(integrity.serializer_crc32_candidate, expected_crc);
        assert_eq!(integrity.multi_import_additive32_candidate, expected_sum);
        assert_eq!(integrity.matches_serializer_crc32, crc_matches);
        assert_eq!(integrity.matches_multi_import_additive32, sum_matches);
        assert_no_content_authority(&reference);
        let outer = synthetic_mode2(&bytes, false);
        let file = NativeFile::parse(&outer).unwrap();
        assert_eq!(file.report().level, CompatibilityLevel::Partial);
        assert_eq!(file.preserve_with_raw(file.raw()).unwrap(), outer);
    }
}

#[test]
fn synthetic_raw_names_flags_and_checksums_survive_lossy_native_projection() {
    let mut bytes = synthetic_carrier();
    let reference = described(&bytes);
    assert_eq!(reference.name_raw, NAME);
    assert_eq!(reference.name_raw_hex, "20417d1f7eff42007461696c00000000");
    assert_eq!(reference.name_native_ascii_display, " A}B");
    assert_eq!(&reference.name_native_ascii_projection[..4], b" A}B");
    assert_eq!(&reference.name_native_ascii_projection[4..], &[0; 12]);
    assert_eq!(reference.flags_raw, u16::MAX);
    assert_eq!(reference.checksum_raw, 0);
    assert_eq!(bytes[12..28], NAME);
    assert_no_content_authority(&reference);
    for name in [[b'A'; 16], [0; 16]] {
        bytes[12..28].copy_from_slice(&name);
        bytes[6..8].copy_from_slice(&0u16.to_le_bytes());
        bytes[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        let reference = described(&bytes);
        assert_eq!(reference.name_raw, name);
        assert_eq!(reference.name_native_ascii_projection, name);
        assert_eq!(reference.flags_raw, 0);
        assert_eq!(reference.checksum_raw, u32::MAX);
        assert_no_content_authority(&reference);
    }
}

#[test]
fn synthetic_same_sized_record_edit_preserves_all_ir_carrier_and_header_bytes() {
    for carrier in [synthetic_carrier().to_vec(), vec![0xa5; 57]] {
        let bytes = synthetic_mode2(&carrier, false);
        let file = NativeFile::parse(&bytes).unwrap();
        let before_inspection = file.ir_reference_inspection();
        let mut records = file.raw().records().to_vec();
        records
            .iter_mut()
            .find(|record| record.command == 0x14 && record.body[0] == 3)
            .unwrap()
            .body[8..12]
            .copy_from_slice(&0x7fc0_00a5u32.to_le_bytes());
        let raw = RawTransfer::from_records(0x12, &records).unwrap();
        let edited = file.preserve_with_raw(&raw).unwrap();
        let after = NativeFile::parse(&edited).unwrap();
        assert_eq!(after.header(), file.header());
        assert_eq!(after.embedded_ir_bytes(), Some(carrier.as_slice()));
        assert_eq!(after.ir_reference_inspection(), before_inspection);
        assert_eq!(after.raw().as_ref(), &raw);
        let start = 64 + carrier.len();
        let changed_field = start + 340..start + 344;
        assert!(
            bytes
                .iter()
                .zip(&edited)
                .enumerate()
                .filter(|(_, (left, right))| left != right)
                .all(|(index, _)| changed_field.contains(&index) || index >= edited.len() - 4)
        );
    }
}

#[test]
fn synthetic_metadata_reference_reports_separate_sources_without_claiming_backup() {
    let bytes = synthetic_mode2(&synthetic_carrier(), true);
    let file = NativeFile::parse(&bytes).unwrap();
    assert_eq!(file.report().unresolved_assets.len(), 1);
    assert!(
        file.report()
            .notes
            .iter()
            .any(|note| note.contains("metadata reference, not IR audio"))
    );
    assert!(
        file.report()
            .notes
            .iter()
            .any(|note| note.contains("CRC32 and additive32 trailer candidates separately"))
    );
    assert!(
        file.report()
            .notes
            .iter()
            .any(|note| note.contains("not a complete asset backup"))
    );
    assert!(
        file.report()
            .notes
            .iter()
            .any(|note| note.contains("missing IR files may leave the pedal muted"))
    );
    let temp = std::env::temp_dir().join(format!("gp200-ir-reference-{}", std::process::id()));
    std::fs::create_dir(&temp).unwrap();
    let mut store = LibraryStore::open(temp.join("library")).unwrap();
    let source = temp.join("synthetic-metadata-reference.prst");
    std::fs::write(&source, &bytes).unwrap();
    let (version, _) = store
        .import_native(&gp200_domain::ImportRequest { path: source })
        .unwrap();
    let output = temp.join("preserved.prst");
    let report = store
        .export_native(&NativeExportRequest {
            path: output.clone(),
            version,
            mode: NativeExportMode::PreserveOriginal,
        })
        .unwrap();
    assert_eq!(std::fs::read(output).unwrap(), bytes);
    assert!(report.notes.iter().any(|note| {
        note.contains("Retained references do not supply or restore IR/SnapTone contents")
    }));
    drop(store);
    std::fs::remove_dir_all(temp).unwrap();
}
