//! Original-editor VTSI `.clo` calibration output inspector.
//!
//! This is a local file format, not a claim that the NAM calibration algorithm
//! or pedal transfer has been verified. Unknown calibration words remain raw.
use crate::{invalid, store::digest};
use gp200_domain::{F32Bits, ServiceError};
use serde::Serialize;

pub const CLO_BYTES: usize = 0x2288;
pub const CLO_COEFFICIENT_BYTES: usize = 0x2200;
pub const CLO_COEFFICIENT_OFFSET: usize = 0x88;
pub const GP200_CLO_DECLARED_BYTES: usize = 0x1288;
pub const GP200_CLO_COEFFICIENT_BYTES: usize = 0x1200;

/// CRC16/Modbus, with the result stored big endian in the native container.
/// The shipped 256-entry table at 0x797370/0x797470 matches polynomial 0xa001.
pub fn clo_crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0xffff_u16;
    for byte in bytes {
        crc ^= u16::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xa001
            } else {
                crc >> 1
            };
        }
    }
    crc
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CloCoefficientRange {
    pub offset: u32,
    pub count: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapToneInfo {
    pub bytes: usize,
    pub declared_bytes: usize,
    pub coefficient_bytes: usize,
    pub container_profile: String,
    pub sha256: String,
    pub crc16: u16,
    pub reserved_raw: [u8; 10],
    pub metadata_raw: Vec<u8>,
    pub calibration_words: [F32Bits; 4],
    pub ranges: [CloCoefficientRange; 2],
    pub coefficient_sha256: String,
}
#[derive(Debug, Clone)]
pub struct SnapToneContainer {
    raw: Vec<u8>,
    info: SnapToneInfo,
}
impl SnapToneContainer {
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }
    pub fn info(&self) -> &SnapToneInfo {
        &self.info
    }
    /// Exact float words, including NaN payloads and signed zero. No interpretation
    /// is assigned to calibration coefficients by inspection.
    pub fn coefficients(&self, range: usize) -> Option<Vec<F32Bits>> {
        let range = self.info.ranges.get(range)?;
        let offset = CLO_COEFFICIENT_OFFSET + range.offset as usize * 4;
        Some(
            self.raw[offset..offset + range.count as usize * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|word| F32Bits::from_le_bytes(*word))
                .collect(),
        )
    }
}
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("bounded container"),
    )
}
pub fn inspect_snaptone(bytes: &[u8]) -> Result<SnapToneContainer, ServiceError> {
    if bytes.len() != CLO_BYTES || &bytes[..4] != b"VTSI" {
        return Err(invalid(
            "SnapTone .clo requires VTSI magic and exactly 8840 bytes",
        ));
    }
    let declared_bytes = u32_at(bytes, 4) as usize;
    let coefficient_bytes = u32_at(bytes, 20) as usize;
    let profile = match (declared_bytes, coefficient_bytes) {
        (CLO_BYTES, CLO_COEFFICIENT_BYTES) => "vtsi-large-native-layout",
        (GP200_CLO_DECLARED_BYTES, GP200_CLO_COEFFICIENT_BYTES)
            if u32_at(bytes, 120) == 0
                && u32_at(bytes, 124) == 128
                && u32_at(bytes, 128) == 128
                && u32_at(bytes, 132) == 1024
                && bytes[GP200_CLO_DECLARED_BYTES..]
                    .iter()
                    .all(|byte| *byte == 0) =>
        {
            "vtsi-a128-b1024-namtoclo-42c07456-padded"
        }
        _ => {
            return Err(invalid(
                "Unsupported VTSI declared length, coefficient count or padding profile",
            ));
        }
    };
    if declared_bytes != CLO_COEFFICIENT_OFFSET + coefficient_bytes {
        return Err(invalid("VTSI container or coefficient length is invalid"));
    }
    let crc16 = u16::from_be_bytes([bytes[8], bytes[9]]);
    if crc16 != clo_crc16(&bytes[12..declared_bytes]) {
        return Err(invalid("VTSI CRC16 mismatch"));
    }
    let ranges = [
        CloCoefficientRange {
            offset: u32_at(bytes, 120),
            count: u32_at(bytes, 124),
        },
        CloCoefficientRange {
            offset: u32_at(bytes, 128),
            count: u32_at(bytes, 132),
        },
    ];
    for range in &ranges {
        let end = u64::from(range.offset) + u64::from(range.count);
        if end > (coefficient_bytes / 4) as u64 {
            return Err(invalid("VTSI coefficient range exceeds its fixed payload"));
        }
    }
    if ranges[0].count > 0
        && ranges[1].count > 0
        && u64::from(ranges[0].offset) < u64::from(ranges[1].offset) + u64::from(ranges[1].count)
        && u64::from(ranges[1].offset) < u64::from(ranges[0].offset) + u64::from(ranges[0].count)
    {
        return Err(invalid("VTSI coefficient ranges overlap"));
    }
    let info = SnapToneInfo {
        bytes: bytes.len(),
        declared_bytes,
        coefficient_bytes,
        container_profile: profile.into(),
        sha256: digest(bytes),
        crc16,
        reserved_raw: bytes[10..20].try_into().expect("bounded header"),
        metadata_raw: bytes[24..104].to_vec(),
        calibration_words: std::array::from_fn(|index| F32Bits(u32_at(bytes, 104 + index * 4))),
        ranges,
        coefficient_sha256: digest(&bytes[CLO_COEFFICIENT_OFFSET..declared_bytes]),
    };
    Ok(SnapToneContainer {
        raw: bytes.to_vec(),
        info,
    })
}
