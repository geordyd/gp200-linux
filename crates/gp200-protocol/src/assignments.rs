//! Retained per-patch assignments. The native editor's full-record paths emit
//! 0x000c (EXP), 0x000f (CTRL), and 0x0010 (quick access), in the patch context.
//! These records contain parameter references, not XML model Codes or Indices.
use crate::{
    F32Bits, MutationError, RawRecord, RawTransfer, WireContext, contextual_write_request,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AssignmentCodecError {
    #[error("expected command {command:#06x} with {length} body bytes")]
    Structure { command: u16, length: usize },
    #[error("EXP identity must contain group 0..2 and slot 0..2")]
    ExpressionIdentity,
    #[error("CTRL identity must be 0..7")]
    CtrlIdentity,
    #[error("quick access identity must be 0..2")]
    QuickIdentity,
    #[error("module bit must be 0..10")]
    ModuleBit,
}

/// The high nibble selects EXP1 A, EXP1 B, or external EXP2; the low nibble
/// selects its sparse assignment slot. Neither nibble is a module number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ExpressionSlotId(u8);
impl ExpressionSlotId {
    pub fn new(group: u8, slot: u8) -> Result<Self, AssignmentCodecError> {
        if group > 2 || slot > 2 {
            return Err(AssignmentCodecError::ExpressionIdentity);
        }
        Ok(Self((group << 4) | slot))
    }
    pub fn from_packed(packed: u8) -> Result<Self, AssignmentCodecError> {
        Self::new(packed >> 4, packed & 0x0f)
    }
    pub const fn packed(self) -> u8 {
        self.0
    }
    pub const fn group(self) -> u8 {
        self.0 >> 4
    }
    pub const fn slot(self) -> u8 {
        self.0 & 0x0f
    }
}
impl<'de> Deserialize<'de> for ExpressionSlotId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::from_packed(u8::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpressionAssignmentRecord {
    pub slot: ExpressionSlotId,
    /// 0xff means OFF. Other unknown references remain readable and lossless.
    pub module_raw: u8,
    pub parameter_raw: u16,
    /// The wire stores MAX before MIN. Reversed ranges are intentional.
    pub maximum: F32Bits,
    pub minimum: F32Bits,
}
impl ExpressionAssignmentRecord {
    pub fn decode(record: &RawRecord) -> Result<Self, AssignmentCodecError> {
        require(record, 0x0c, 12)?;
        let b = &record.body;
        Ok(Self {
            slot: ExpressionSlotId::from_packed(b[0])?,
            module_raw: b[1],
            parameter_raw: u16::from_le_bytes([b[2], b[3]]),
            maximum: F32Bits::from_le_bytes(b[4..8].try_into().unwrap()),
            minimum: F32Bits::from_le_bytes(b[8..12].try_into().unwrap()),
        })
    }
    pub fn to_record(&self) -> RawRecord {
        let mut body = vec![self.slot.packed(), self.module_raw];
        body.extend_from_slice(&self.parameter_raw.to_le_bytes());
        body.extend_from_slice(&self.maximum.to_le_bytes());
        body.extend_from_slice(&self.minimum.to_le_bytes());
        RawRecord {
            command: 0x0c,
            body,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CtrlAssignmentRecord {
    pub slot: u8,
    pub state_raw: u8,
    /// The native copy routine skips these two bytes. Preserve captured values.
    pub retained_padding: [u8; 2],
    /// Low eleven bits are module membership; unnamed high bits are retained.
    pub module_mask_raw: u32,
}
impl CtrlAssignmentRecord {
    pub const MODULE_MASK: u32 = 0x7ff;
    pub fn decode(record: &RawRecord) -> Result<Self, AssignmentCodecError> {
        require(record, 0x0f, 8)?;
        let b = &record.body;
        if b[0] > 7 {
            return Err(AssignmentCodecError::CtrlIdentity);
        }
        Ok(Self {
            slot: b[0],
            state_raw: b[1],
            retained_padding: [b[2], b[3]],
            module_mask_raw: u32::from_le_bytes(b[4..8].try_into().unwrap()),
        })
    }
    pub fn to_record(&self) -> RawRecord {
        let mut body = vec![self.slot, self.state_raw];
        body.extend_from_slice(&self.retained_padding);
        body.extend_from_slice(&self.module_mask_raw.to_le_bytes());
        RawRecord {
            command: 0x0f,
            body,
        }
    }
    pub fn contains_module(&self, module: u8) -> Result<bool, AssignmentCodecError> {
        if module > 10 {
            return Err(AssignmentCodecError::ModuleBit);
        }
        Ok(self.module_mask_raw & (1 << module) != 0)
    }
    pub fn set_module(&mut self, module: u8, present: bool) -> Result<(), AssignmentCodecError> {
        if module > 10 {
            return Err(AssignmentCodecError::ModuleBit);
        }
        let bit = 1 << module;
        self.module_mask_raw = (self.module_mask_raw & !bit) | if present { bit } else { 0 };
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickAssignmentRecord {
    pub index: u8,
    /// 0..10: module parameter, 11: patch volume, 12: patch tempo, 0xff: OFF.
    pub module_raw: u8,
    /// Special references may retain 0 or 0xff; do not normalize on decode.
    pub parameter_raw: u16,
}
impl QuickAssignmentRecord {
    pub fn decode(record: &RawRecord) -> Result<Self, AssignmentCodecError> {
        require(record, 0x10, 4)?;
        if record.body[0] > 2 {
            return Err(AssignmentCodecError::QuickIdentity);
        }
        Ok(Self {
            index: record.body[0],
            module_raw: record.body[1],
            parameter_raw: u16::from_le_bytes([record.body[2], record.body[3]]),
        })
    }
    pub fn to_record(&self) -> RawRecord {
        let mut body = vec![self.index, self.module_raw];
        body.extend_from_slice(&self.parameter_raw.to_le_bytes());
        RawRecord {
            command: 0x10,
            body,
        }
    }
}

fn require(record: &RawRecord, command: u16, length: usize) -> Result<(), AssignmentCodecError> {
    if record.command != command || record.body.len() != length {
        Err(AssignmentCodecError::Structure { command, length })
    } else {
        Ok(())
    }
}

/// Native EXP sliders emit cmd0x000e, rather than overwriting cmd0x000c.
/// Manager0x5b8890 also propagates an endpoint to matching references in a bank;
/// callers must account for that coupled mutation in their expected readback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExpressionEndpoint {
    Minimum,
    Maximum,
}
pub fn expression_endpoint_record(
    slot: ExpressionSlotId,
    endpoint: ExpressionEndpoint,
    value: F32Bits,
    retained_padding: [u8; 2],
) -> Result<RawRecord, MutationError> {
    if !value.as_f32().is_finite() {
        return Err(MutationError::InvalidPreset);
    }
    let mut body = vec![
        match endpoint {
            ExpressionEndpoint::Minimum => 0,
            ExpressionEndpoint::Maximum => 1,
        },
        slot.packed(),
    ];
    body.extend_from_slice(&retained_padding);
    body.extend_from_slice(&value.to_le_bytes());
    Ok(RawRecord {
        command: 0x0e,
        body,
    })
}
pub fn expression_endpoint_write_request(
    context: WireContext,
    slot: ExpressionSlotId,
    endpoint: ExpressionEndpoint,
    value: F32Bits,
    retained_padding: [u8; 2],
) -> Result<RawTransfer, MutationError> {
    contextual_write_request(
        context,
        &[expression_endpoint_record(
            slot,
            endpoint,
            value,
            retained_padding,
        )?],
    )
}

/// Send/Return occupy boundaries around the eleven modules. The original drag
/// callback couples the other node when crossed, so the resulting pair must be
/// ordered. Equality bypasses the loop span without deleting either node.
pub fn fx_loop_positions_record(
    retained: &RawRecord,
    send_boundary: u8,
    return_boundary: u8,
) -> Result<RawRecord, MutationError> {
    if retained.command != 8
        || retained.body.len() != 16
        || send_boundary > return_boundary
        || return_boundary > 11
    {
        return Err(MutationError::InvalidChain);
    }
    let mut result = retained.clone();
    result.body[2] = send_boundary;
    result.body[3] = return_boundary;
    Ok(result)
}

/// Full-record writes established by native EXP/CTRL/quick callbacks. Read-side
/// unknown references remain lossless, but cannot be submitted as new writes.
pub fn patch_assignment_write_request(
    context: WireContext,
    record: &RawRecord,
) -> Result<RawTransfer, MutationError> {
    let valid = match record.command {
        0x0c => ExpressionAssignmentRecord::decode(record)
            .is_ok_and(|r| r.module_raw == 0xff || (r.module_raw <= 10 && r.parameter_raw <= 14)),
        0x0f => CtrlAssignmentRecord::decode(record).is_ok_and(|r| r.state_raw <= 1),
        0x10 => QuickAssignmentRecord::decode(record).is_ok_and(|r| {
            matches!(r.module_raw, 11 | 12 | 0xff) || (r.module_raw <= 10 && r.parameter_raw <= 14)
        }),
        _ => false,
    };
    if !valid {
        return Err(MutationError::InvalidPreset);
    }
    contextual_write_request(context, std::slice::from_ref(record))
}
