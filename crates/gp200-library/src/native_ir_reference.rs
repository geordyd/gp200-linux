//! Descriptive inspection of the recovered native IR metadata carrier.
//!
//! This does not validate native mode 2 interoperability or decode IR audio.
//! Unknown RISU payloads stay opaque; callers must retain their original bytes.
use crate::{native::native_blob_crc32, store::digest};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeIrOpaqueReason {
    UnexpectedLength,
    UnexpectedCommand,
    UnexpectedBodyLength,
    UnresolvedSlot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeIrReferenceOccupancy {
    Unknown,
}

/// The recovered producer uses CRC32, while an inspected multi-import reader
/// uses an additive32 check. Neither candidate grants compatibility authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NativeIrReferenceIntegrity {
    pub trailer_be32: u32,
    pub serializer_crc32_candidate: u32,
    pub matches_serializer_crc32: bool,
    pub multi_import_additive32_candidate: u32,
    pub matches_multi_import_additive32: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NativeIrMetadataReference {
    pub carrier_sha256: String,
    pub command: u16,
    pub body_bytes: u16,
    pub slot_word: u16,
    pub flags_raw: u16,
    /// Weak checksum metadata; it cannot identify or reconstruct coefficients.
    pub checksum_raw: u32,
    pub name_raw: [u8; 16],
    pub name_raw_hex: String,
    /// Derived native cache name: ASCII 0x20..0x7d before NUL, zero-padded.
    /// Raw bytes above remain unchanged and may differ from this projection.
    pub name_native_ascii_projection: [u8; 16],
    pub name_native_ascii_display: String,
    pub integrity_candidates: NativeIrReferenceIntegrity,
    pub metadata_only: bool,
    pub ir_coefficients_present: bool,
    pub physical_occupancy: NativeIrReferenceOccupancy,
    pub coefficient_identity_proven: bool,
    pub complete_asset_backup: bool,
    pub device_write_authority: bool,
    pub native_mode2_interoperability_verified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "details", rename_all = "snake_case")]
pub enum NativeIrReferenceInspection {
    Opaque {
        byte_count: usize,
        reason: NativeIrOpaqueReason,
    },
    MetadataReference(NativeIrMetadataReference),
}

/// Inspect only the exact 32-byte command1009/body24/slot0..19 carrier recovered
/// from the shipped editor. This is not a validator for the enclosing file.
/// Even mismatching trailer candidates are described without changing native
/// parse acceptance. No claims are made about contents of opaque carriers.
pub fn inspect_native_ir_reference(bytes: &[u8]) -> NativeIrReferenceInspection {
    let opaque = |reason| NativeIrReferenceInspection::Opaque {
        byte_count: bytes.len(),
        reason,
    };
    let Ok(carrier): Result<&[u8; 32], _> = bytes.try_into() else {
        return opaque(NativeIrOpaqueReason::UnexpectedLength);
    };
    let command = u16::from_le_bytes([carrier[0], carrier[1]]);
    if command != 0x1009 {
        return opaque(NativeIrOpaqueReason::UnexpectedCommand);
    }
    let body_bytes = u16::from_le_bytes([carrier[2], carrier[3]]);
    if body_bytes != 24 {
        return opaque(NativeIrOpaqueReason::UnexpectedBodyLength);
    }
    let slot_word = u16::from_le_bytes([carrier[4], carrier[5]]);
    if slot_word >= 20 {
        return opaque(NativeIrOpaqueReason::UnresolvedSlot);
    }
    let flags_raw = u16::from_le_bytes([carrier[6], carrier[7]]);
    let checksum_raw = u32::from_le_bytes(carrier[8..12].try_into().expect("four bytes"));
    let name_raw: [u8; 16] = carrier[12..28].try_into().expect("sixteen bytes");
    let mut name_native_ascii_projection = [0; 16];
    let mut length = 0;
    for byte in name_raw.iter().copied().take_while(|byte| *byte != 0) {
        if (0x20..=0x7d).contains(&byte) {
            name_native_ascii_projection[length] = byte;
            length += 1;
        }
    }
    let name_native_ascii_display =
        String::from_utf8(name_native_ascii_projection[..length].to_vec()).expect("ASCII bytes");
    let name_raw_hex = name_raw.iter().map(|byte| format!("{byte:02x}")).collect();
    let trailer_be32 = u32::from_be_bytes(carrier[28..32].try_into().expect("four bytes"));
    let serializer_crc32_candidate = native_blob_crc32(&carrier[..28]);
    let multi_import_additive32_candidate = carrier[..28]
        .iter()
        .fold(0u32, |sum, byte| sum.wrapping_add(u32::from(*byte)));
    NativeIrReferenceInspection::MetadataReference(NativeIrMetadataReference {
        carrier_sha256: digest(carrier),
        command,
        body_bytes,
        slot_word,
        flags_raw,
        checksum_raw,
        name_raw,
        name_raw_hex,
        name_native_ascii_projection,
        name_native_ascii_display,
        integrity_candidates: NativeIrReferenceIntegrity {
            trailer_be32,
            serializer_crc32_candidate,
            matches_serializer_crc32: trailer_be32 == serializer_crc32_candidate,
            multi_import_additive32_candidate,
            matches_multi_import_additive32: trailer_be32 == multi_import_additive32_candidate,
        },
        metadata_only: true,
        ir_coefficients_present: false,
        physical_occupancy: NativeIrReferenceOccupancy::Unknown,
        coefficient_identity_proven: false,
        complete_asset_backup: false,
        device_write_authority: false,
        native_mode2_interoperability_verified: false,
    })
}
