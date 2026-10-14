//! Offline FRMW container inspector. No bootloader commands or flashing.
//!
//! Layout: original editor 0x485a50/0x485ec0, corroborated by the official
//! GP-200 1.8.0 image. The additive checksum is not a digital signature.
use crate::{MAX_SOURCE_BYTES, invalid, store::digest};
use gp200_domain::{DeviceModel, FirmwareImageProvenance, ServiceError};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FirmwareSection {
    /// Original FRMW table slot, independent of native wire transfer order.
    pub index: u8,
    pub relative_offset: u32,
    pub file_offset: usize,
    pub bytes: u32,
    pub sha256: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FirmwareInfo {
    pub model: DeviceModel,
    pub bytes: usize,
    pub sha256: String,
    pub payload_bytes: u32,
    pub image_family_raw: u16,
    pub version_word_raw: u16,
    pub header_word_raw: u16,
    pub checksum_word_raw: u16,
    pub additive_checksum: u8,
    pub product_raw: [u8; 16],
    pub metadata_raw: [u8; 16],
    pub version_tag_raw: [u8; 4],
    pub image_version: Option<String>,
    /// Additive projection; raw FRMW words/metadata and the file stay unchanged.
    pub image_provenance: FirmwareImageProvenance,
    /// Original FRMW table order; no native transfer permutation is applied.
    pub sections: [FirmwareSection; 4],
}
/// Parsing owns an immutable exact copy; metadata cannot replace authoritative bytes.
#[derive(Debug, Clone)]
pub struct FirmwareImage {
    raw: Vec<u8>,
    info: FirmwareInfo,
}
impl FirmwareImage {
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }
    pub fn info(&self) -> &FirmwareInfo {
        &self.info
    }
    /// Exact bytes for a raw FRMW table slot.
    pub fn section(&self, index: usize) -> Option<&[u8]> {
        let section = self.info.sections.get(index)?;
        Some(&self.raw[section.file_offset..section.file_offset + section.bytes as usize])
    }
}
fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("bounded header"),
    )
}
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("bounded header"),
    )
}
pub fn inspect_firmware(bytes: &[u8]) -> Result<FirmwareImage, ServiceError> {
    if bytes.len() < 92 || bytes.len() > MAX_SOURCE_BYTES {
        return Err(invalid(
            "FRMW image must contain its complete header/table/version tag within the 64 MiB local bound",
        ));
    }
    if &bytes[..4] != b"FRMW" {
        return Err(invalid("Firmware has no FRMW magic"));
    }
    let payload_bytes = u32_at(bytes, 4);
    if payload_bytes as usize != bytes.len() - 48 {
        return Err(invalid(
            "FRMW declared payload length does not match the complete file",
        ));
    }
    let product_raw: [u8; 16] = bytes[16..32].try_into().expect("bounded header");
    let product = |name: &[u8]| {
        let mut value = [0_u8; 16];
        value[..name.len()].copy_from_slice(name);
        value
    };
    let model = if product_raw == product(b"GP-200") {
        DeviceModel::Gp200
    } else if product_raw == product(b"GP-200LT") {
        DeviceModel::Gp200Lt
    } else if product_raw == product(b"GP-200JR") {
        DeviceModel::Gp200Jr
    } else {
        return Err(invalid(
            "FRMW product field is not an exact known GP-200 variant",
        ));
    };
    let additive_checksum = bytes[48..]
        .iter()
        .fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
    let checksum_word_raw = u16_at(bytes, 14);
    // The original validator compares only the low byte. Preserve the high byte.
    if additive_checksum != checksum_word_raw as u8 {
        return Err(invalid("FRMW additive payload checksum mismatch"));
    }
    let mut sections = Vec::with_capacity(4);
    for index in 0..4 {
        let relative_offset = u32_at(bytes, 48 + index * 8);
        let length = u32_at(bytes, 52 + index * 8);
        let file_offset = 48_usize
            .checked_add(relative_offset as usize)
            .ok_or_else(|| invalid("FRMW section offset overflow"))?;
        let end = file_offset
            .checked_add(length as usize)
            .ok_or_else(|| invalid("FRMW section length overflow"))?;
        if relative_offset < 32 || length == 0 || end > bytes.len() {
            return Err(invalid(
                "FRMW section overlaps its table, is empty, or extends beyond the file",
            ));
        }
        for other in &sections {
            let other: &FirmwareSection = other;
            if file_offset < other.file_offset + other.bytes as usize && other.file_offset < end {
                return Err(invalid("FRMW sections overlap"));
            }
        }
        sections.push(FirmwareSection {
            index: index as u8,
            relative_offset,
            file_offset,
            bytes: length,
            sha256: digest(&bytes[file_offset..end]),
        });
    }
    let version_tag_raw: [u8; 4] = bytes[88..92].try_into().expect("bounded version tag");
    let image_version = (matches!(version_tag_raw[0], b'B' | b'b' | b'V' | b'v')
        && version_tag_raw[1..].iter().all(u8::is_ascii_digit))
    .then(|| {
        format!(
            "{}.{}.{}",
            version_tag_raw[1] - b'0',
            version_tag_raw[2] - b'0',
            version_tag_raw[3] - b'0'
        )
    });
    let info = FirmwareInfo {
        model,
        bytes: bytes.len(),
        sha256: digest(bytes),
        payload_bytes,
        image_family_raw: u16_at(bytes, 8),
        version_word_raw: u16_at(bytes, 10),
        header_word_raw: u16_at(bytes, 12),
        checksum_word_raw,
        additive_checksum,
        product_raw,
        metadata_raw: bytes[32..48].try_into().expect("bounded header"),
        version_tag_raw,
        image_version,
        image_provenance: FirmwareImageProvenance::from_header(&bytes[..92])
            .map_err(|error| invalid(error.to_string()))?,
        sections: sections.try_into().expect("four sections"),
    };
    Ok(FirmwareImage {
        raw: bytes.to_vec(),
        info,
    })
}
