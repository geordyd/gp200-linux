use serde::{Deserialize, Serialize};

/// A wire float stored as its exact IEEE-754 bit pattern, including NaN payloads.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct F32Bits(pub u32);

impl F32Bits {
    pub const fn from_le_bytes(bytes: [u8; 4]) -> Self {
        Self(u32::from_le_bytes(bytes))
    }
    pub const fn to_le_bytes(self) -> [u8; 4] {
        self.0.to_le_bytes()
    }
    pub fn from_f32(value: f32) -> Self {
        Self(value.to_bits())
    }
    pub fn as_f32(self) -> f32 {
        f32::from_bits(self.0)
    }
}

impl From<f32> for F32Bits {
    fn from(value: f32) -> Self {
        Self::from_f32(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawRecord {
    pub command: u16,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RawError {
    #[error("truncated record header at byte {0}")]
    TruncatedHeader(usize),
    #[error("truncated record body at byte {0}")]
    TruncatedBody(usize),
    #[error("record body exceeds the u16 wire length")]
    RecordTooLarge,
    #[error("kind must fit a MIDI data byte")]
    InvalidKind,
}

/// Raw bytes are authoritative. Ordered records are always derived from them.
/// Invalid typed records remain legal bodies; truncated framing is rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RawTransfer {
    kind: u8,
    payload: Vec<u8>,
    #[serde(skip)]
    records: Vec<RawRecord>,
}

impl RawTransfer {
    pub fn from_payload(kind: u8, payload: Vec<u8>) -> Result<Self, RawError> {
        if kind > 0x7f {
            return Err(RawError::InvalidKind);
        }
        let mut records = Vec::new();
        let mut offset = 0;
        while offset < payload.len() {
            if payload.len() - offset < 4 {
                return Err(RawError::TruncatedHeader(offset));
            }
            let command = u16::from_le_bytes([payload[offset], payload[offset + 1]]);
            let size = u16::from_le_bytes([payload[offset + 2], payload[offset + 3]]) as usize;
            let end = offset + 4 + size;
            if end > payload.len() {
                return Err(RawError::TruncatedBody(offset));
            }
            records.push(RawRecord {
                command,
                body: payload[offset + 4..end].to_vec(),
            });
            offset = end;
        }
        Ok(Self {
            kind,
            payload,
            records,
        })
    }
    pub fn from_records(kind: u8, records: &[RawRecord]) -> Result<Self, RawError> {
        let mut payload = Vec::new();
        for record in records {
            let size = u16::try_from(record.body.len()).map_err(|_| RawError::RecordTooLarge)?;
            payload.extend_from_slice(&record.command.to_le_bytes());
            payload.extend_from_slice(&size.to_le_bytes());
            payload.extend_from_slice(&record.body);
        }
        Self::from_payload(kind, payload)
    }
    pub fn kind(&self) -> u8 {
        self.kind
    }
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
    pub fn records(&self) -> &[RawRecord] {
        &self.records
    }
}

impl<'de> Deserialize<'de> for RawTransfer {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            kind: u8,
            payload: Vec<u8>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::from_payload(wire.kind, wire.payload).map_err(serde::de::Error::custom)
    }
}
