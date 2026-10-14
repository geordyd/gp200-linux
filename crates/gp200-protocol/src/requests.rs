use crate::{F32Bits, ProtocolError, READ_KIND, RawRecord, RawTransfer, WRITE_KIND};

/// Protocol-local target to avoid a dependency cycle with gp200-domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadTarget {
    Current,
    Stored(u16),
}

/// Object context from command 0. It is not a transaction identifier. Keep each
/// snapshot's context immutable rather than reusing the most recently seen one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireContext {
    pub group: u16,
    pub selector: u16,
}

impl WireContext {
    pub fn to_record(self) -> RawRecord {
        let mut body = Vec::with_capacity(4);
        body.extend_from_slice(&self.group.to_le_bytes());
        body.extend_from_slice(&self.selector.to_le_bytes());
        RawRecord { command: 0, body }
    }
    pub fn from_transfer(transfer: &RawTransfer) -> Option<Self> {
        let record = transfer
            .records()
            .iter()
            .find(|record| record.command == 0 && record.body.len() == 4)?;
        Some(Self {
            group: u16::from_le_bytes([record.body[0], record.body[1]]),
            selector: u16::from_le_bytes([record.body[2], record.body[3]]),
        })
    }
}

/// Confirmed identity query (kind 0x11, command 0x2001, empty body).
pub fn identify_request() -> RawTransfer {
    RawTransfer::from_records(
        READ_KIND,
        &[RawRecord {
            command: 0x2001,
            body: vec![],
        }],
    )
    .expect("constant identity request is valid")
}

/// Confirmed whole edit-buffer read. This is deliberately not the original
/// editor's unresolved selector-specific current-state builder.
pub fn read_current_request() -> RawTransfer {
    read_request(ReadTarget::Current).expect("constant current request is valid")
}

/// Confirmed stored-slot read. This does not select the preset on the pedal.
pub fn read_stored_request(slot: u16) -> Result<RawTransfer, ProtocolError> {
    read_request(ReadTarget::Stored(slot))
}

pub fn read_request(target: ReadTarget) -> Result<RawTransfer, ProtocolError> {
    let (context, range) = match target {
        ReadTarget::Current => (
            WireContext {
                group: 0,
                selector: 0,
            },
            [0xff; 4],
        ),
        ReadTarget::Stored(slot) => {
            if slot > 255 {
                return Err(ProtocolError::InvalidSlot(slot));
            }
            let [low, high] = slot.to_le_bytes();
            (
                WireContext {
                    group: 1,
                    selector: slot,
                },
                [low, high, low, high],
            )
        }
    };
    Ok(RawTransfer::from_records(
        READ_KIND,
        &[
            context.to_record(),
            RawRecord {
                command: 1,
                body: range.to_vec(),
            },
        ],
    )?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireDeviceModel {
    Gp200,
    Gp200Lt,
    Gp200Jr,
    Unknown(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub state_raw: u16,
    pub model_raw: u16,
}

impl Identity {
    /// Search for the requested response, never mistake an unsolicited parameter
    /// notification for identity. Malformed identity bodies are ignored, retained
    /// in RawTransfer for diagnostics, and cannot satisfy identification.
    pub fn from_transfer(transfer: &RawTransfer) -> Option<Self> {
        let record = transfer
            .records()
            .iter()
            .find(|record| record.command == 0x2001 && record.body.len() == 4)?;
        Some(Self {
            state_raw: u16::from_le_bytes([record.body[0], record.body[1]]),
            model_raw: u16::from_le_bytes([record.body[2], record.body[3]]),
        })
    }
    pub fn model(self) -> WireDeviceModel {
        match self.model_raw {
            2 => WireDeviceModel::Gp200,
            3 => WireDeviceModel::Gp200Lt,
            4 => WireDeviceModel::Gp200Jr,
            other => WireDeviceModel::Unknown(other),
        }
    }
}

/// Distinct wire type: command 5 uses XML model **Index**, whereas module replies
/// (command 14) and model changes (command 16) use XML model **Code**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireModelIndex(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParameterWrite {
    pub module_id: u8,
    pub xml_param_id: u8,
    pub model_index: WireModelIndex,
    pub value: F32Bits,
}

impl ParameterWrite {
    /// Build command 5 with zero padding. Static evidence establishes layout;
    /// the controlled Match CL Gain 50→51→50 experiment validates this one path.
    /// Semantic ranges, target epoch and capability authorization belong to the
    /// actor/reducer; constructing bytes is not proof a control works on hardware.
    pub fn to_record(self) -> Result<RawRecord, ProtocolError> {
        if self.module_id > 10 {
            return Err(ProtocolError::InvalidModule(self.module_id));
        }
        if self.xml_param_id > 14 {
            return Err(ProtocolError::InvalidParameter(self.xml_param_id));
        }
        let mut body = vec![self.module_id, self.xml_param_id, 0, 0];
        body.extend_from_slice(&self.model_index.0.to_le_bytes());
        body.extend_from_slice(&self.value.to_le_bytes());
        Ok(RawRecord { command: 5, body })
    }
}

pub fn parameter_write_request(
    context: WireContext,
    parameter: ParameterWrite,
) -> Result<RawTransfer, ProtocolError> {
    Ok(RawTransfer::from_records(
        WRITE_KIND,
        &[context.to_record(), parameter.to_record()?],
    )?)
}
