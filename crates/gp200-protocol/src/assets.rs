//! Device-asset records recovered from the shipped editor. These pure codecs
//! do not authorize a hardware actor, select a target, or establish readback.
use crate::{READ_KIND, RawError, RawRecord, RawTransfer, WRITE_KIND};
use serde::{Deserialize, Serialize};

pub const IR_SLOT_COUNT: u16 = 20;
pub const SNAPTONE_SLOT_COUNT: u16 = 10;
pub const IR_TRANSFER_BYTES: usize = 4096;
pub const SNAPTONE_TRANSFER_BYTES: usize = 8192;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssetBank {
    Ir,
    SnapTone,
}
impl AssetBank {
    pub const fn slots(self) -> u16 {
        match self {
            Self::Ir => IR_SLOT_COUNT,
            Self::SnapTone => SNAPTONE_SLOT_COUNT,
        }
    }
    pub const fn metadata_command(self) -> u16 {
        match self {
            Self::Ir => 0x1009,
            Self::SnapTone => 0x1012,
        }
    }
    pub const fn upload_command(self) -> u16 {
        match self {
            Self::Ir => 0x100a,
            Self::SnapTone => 0x1013,
        }
    }
    pub const fn status_command(self) -> u16 {
        match self {
            Self::Ir => 0x100b,
            Self::SnapTone => 0x1014,
        }
    }
    pub const fn rename_command(self) -> u16 {
        match self {
            Self::Ir => 0x100c,
            Self::SnapTone => 0x1015,
        }
    }
    pub const fn transfer_bytes(self) -> usize {
        match self {
            Self::Ir => IR_TRANSFER_BYTES,
            Self::SnapTone => SNAPTONE_TRANSFER_BYTES,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AssetRecordError {
    #[error("asset slot {slot} is outside {bank:?}'s recovered storage range")]
    InvalidSlot { bank: AssetBank, slot: u16 },
    #[error("asset record command or body length does not match its type")]
    InvalidShape,
    #[error(
        "asset name needs 1–16 printable ASCII bytes from space through '}}', with a non-space character"
    )]
    InvalidName,
    #[error("asset upload has {actual} bytes; {expected} coefficient/container bytes are required")]
    InvalidPayload { actual: usize, expected: usize },
    #[error(transparent)]
    Raw(#[from] RawError),
}
/// Private index; deserialization runs the same storage-bound check as creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AssetSlot {
    bank: AssetBank,
    index: u16,
}
impl AssetSlot {
    pub fn new(bank: AssetBank, index: u16) -> Result<Self, AssetRecordError> {
        if index >= bank.slots() {
            return Err(AssetRecordError::InvalidSlot { bank, slot: index });
        }
        Ok(Self { bank, index })
    }
    pub const fn bank(self) -> AssetBank {
        self.bank
    }
    pub const fn index(self) -> u16 {
        self.index
    }
}
impl<'de> Deserialize<'de> for AssetSlot {
    fn deserialize<D: serde::Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            bank: AssetBank,
            index: u16,
        }
        let wire = Wire::deserialize(decoder)?;
        Self::new(wire.bank, wire.index).map_err(serde::de::Error::custom)
    }
}
fn name_bytes(name: &str) -> Result<[u8; 16], AssetRecordError> {
    if name.is_empty()
        || name.len() > 16
        || !name.bytes().all(|byte| (0x20..=0x7d).contains(&byte))
        || name.bytes().all(|byte| byte == b' ')
    {
        return Err(AssetRecordError::InvalidName);
    }
    let mut raw = [0_u8; 16];
    raw[..name.len()].copy_from_slice(name.as_bytes());
    Ok(raw)
}
fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}
fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("bounded record"),
    )
}

/// Native callbacks 0x499ef0/0x499de0/0x49a040 copy 24/20/8 body bytes.
/// The second word and status value are deliberately raw, not invented enums.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AssetSlotMetadata {
    pub slot: AssetSlot,
    pub flags_raw: u16,
    pub checksum_raw: u32,
    pub name_raw: [u8; 16],
    raw: RawRecord,
}
impl AssetSlotMetadata {
    pub fn decode(bank: AssetBank, record: &RawRecord) -> Result<Self, AssetRecordError> {
        if record.command != bank.metadata_command() || record.body.len() != 24 {
            return Err(AssetRecordError::InvalidShape);
        }
        Ok(Self {
            slot: AssetSlot::new(bank, read_u16(&record.body, 0))?,
            flags_raw: read_u16(&record.body, 2),
            checksum_raw: read_u32(&record.body, 4),
            name_raw: record.body[8..24].try_into().expect("16 bytes"),
            raw: record.clone(),
        })
    }
    pub fn raw(&self) -> &RawRecord {
        &self.raw
    }
    pub fn display_name(&self) -> String {
        let end = self
            .name_raw
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(16);
        String::from_utf8_lossy(&self.name_raw[..end]).into_owned()
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AssetRenameReply {
    pub slot: AssetSlot,
    pub flags_raw: u16,
    pub name_raw: [u8; 16],
    raw: RawRecord,
}
impl AssetRenameReply {
    pub fn decode(bank: AssetBank, record: &RawRecord) -> Result<Self, AssetRecordError> {
        if record.command != bank.rename_command() || record.body.len() != 20 {
            return Err(AssetRecordError::InvalidShape);
        }
        Ok(Self {
            slot: AssetSlot::new(bank, read_u16(&record.body, 0))?,
            flags_raw: read_u16(&record.body, 2),
            name_raw: record.body[4..20].try_into().expect("16 bytes"),
            raw: record.clone(),
        })
    }
    pub fn raw(&self) -> &RawRecord {
        &self.raw
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AssetStatusReply {
    pub slot: AssetSlot,
    pub flags_raw: u16,
    pub value_raw: u32,
    raw: RawRecord,
}
impl AssetStatusReply {
    pub fn decode(bank: AssetBank, record: &RawRecord) -> Result<Self, AssetRecordError> {
        if record.command != bank.status_command() || record.body.len() != 8 {
            return Err(AssetRecordError::InvalidShape);
        }
        Ok(Self {
            slot: AssetSlot::new(bank, read_u16(&record.body, 0))?,
            flags_raw: read_u16(&record.body, 2),
            value_raw: read_u32(&record.body, 4),
            raw: record.clone(),
        })
    }
    pub fn raw(&self) -> &RawRecord {
        &self.raw
    }
}
/// Native read constructors 0x5d6ab0 (SnapTone) and 0x5d6bf0 (IR) initialize
/// slot and flags=1, then send 24 bytes at kind 0x11. Unused stack bytes are
/// canonicalized to zero here; this candidate still requires captured proof.
pub fn asset_metadata_read_candidate(slot: AssetSlot) -> RawTransfer {
    let mut body = vec![0_u8; 24];
    body[..2].copy_from_slice(&slot.index.to_le_bytes());
    body[2..4].copy_from_slice(&1_u16.to_le_bytes());
    RawTransfer::from_records(
        READ_KIND,
        &[RawRecord {
            command: slot.bank.metadata_command(),
            body,
        }],
    )
    .expect("bounded metadata request")
}
/// Overlay only the name of an authoritative metadata baseline. Native writes
/// use 20-byte rename records; keep the observed second word unchanged.
pub fn asset_rename_candidate(
    baseline: &AssetSlotMetadata,
    name: &str,
) -> Result<RawTransfer, AssetRecordError> {
    // Read the private authoritative record again, so mutations of a public
    // display projection cannot redirect a write or change opaque flags.
    let bank = match baseline.raw.command {
        0x1009 => AssetBank::Ir,
        0x1012 => AssetBank::SnapTone,
        _ => return Err(AssetRecordError::InvalidShape),
    };
    let baseline = AssetSlotMetadata::decode(bank, &baseline.raw)?;
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&baseline.slot.index.to_le_bytes());
    body.extend_from_slice(&baseline.flags_raw.to_le_bytes());
    body.extend_from_slice(&name_bytes(name)?);
    Ok(RawTransfer::from_records(
        WRITE_KIND,
        &[RawRecord {
            command: baseline.slot.bank.rename_command(),
            body,
        }],
    )?)
}
/// Native bulk records have a zero second word, byte-sum checksum, 16-byte
/// name, then 4096 IR bytes or 8192 SnapTone bytes. For SnapTone the caller
/// must explicitly supply the native 8192-byte prefix, not an entire .clo.
pub fn asset_upload_candidate(
    slot: AssetSlot,
    name: &str,
    payload: &[u8],
) -> Result<RawTransfer, AssetRecordError> {
    let expected = slot.bank.transfer_bytes();
    if payload.len() != expected {
        return Err(AssetRecordError::InvalidPayload {
            actual: payload.len(),
            expected,
        });
    }
    let mut body = Vec::with_capacity(24 + expected);
    body.extend_from_slice(&slot.index.to_le_bytes());
    body.extend_from_slice(&0_u16.to_le_bytes());
    let sum: u32 = payload.iter().map(|byte| u32::from(*byte)).sum();
    body.extend_from_slice(&sum.to_le_bytes());
    body.extend_from_slice(&name_bytes(name)?);
    body.extend_from_slice(payload);
    Ok(RawTransfer::from_records(
        WRITE_KIND,
        &[RawRecord {
            command: slot.bank.upload_command(),
            body,
        }],
    )?)
}
