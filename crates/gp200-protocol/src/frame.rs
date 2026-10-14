use crate::{RawError, RawTransfer};

/// Normal editor-message signature after `F0`. Firmware uses a different signature.
pub const MAGIC: [u8; 7] = [0x21, 0x25, 0x7e, 0x47, 0x50, 0x2d, 0x32];
pub const READ_KIND: u8 = 0x11;
pub const WRITE_KIND: u8 = 0x12;
pub const MAX_TRANSFER_LENGTH: usize = 0x3fff;
/// Windows editor's TX chunk size. Pedal replies use 185 bytes instead.
pub const TX_CHUNK_BYTES: usize = 183;
/// Absolute allocation bound for one normal packet, including `F0` and `F7`.
pub const MAX_PACKET_LENGTH: usize = 14 + MAX_TRANSFER_LENGTH * 2;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("not a normal GP-200 packet")]
    NotNormalPacket,
    #[error("kind {0:#x} is not a MIDI data byte")]
    InvalidKind(u8),
    #[error("transfer length {0} is outside 1..=16383")]
    InvalidTotal(usize),
    #[error("MIDI status byte at packet offset {0}")]
    StatusInPacket(usize),
    #[error("payload has an odd number of nibbles")]
    OddNibbleCount,
    #[error("payload nibble at packet offset {0} exceeds 15")]
    InvalidNibble(usize),
    #[error("empty decoded chunk")]
    EmptyChunk,
    #[error("chunk offset {offset} plus length {length} exceeds total {total}")]
    ChunkOutOfBounds {
        offset: usize,
        length: usize,
        total: usize,
    },
    #[error(
        "interleaved transfer: expected kind {expected_kind:#x}/total {expected_total}, received kind {actual_kind:#x}/total {actual_total}"
    )]
    InterleavedTransfer {
        expected_kind: u8,
        expected_total: usize,
        actual_kind: u8,
        actual_total: usize,
    },
    #[error("conflicting overlapping byte at decoded offset {0}")]
    ConflictingOverlap(usize),
    #[error("caller timestamp moved backwards")]
    TimestampWentBackwards,
    #[error("incomplete transfer expired; reset before accepting another packet")]
    TransferExpired,
    #[error("SysEx exceeded the bounded packet buffer")]
    PacketTooLarge,
    #[error("new SysEx start before the previous message ended")]
    NestedSysex,
    #[error("unexpected MIDI status {0:#x} inside SysEx")]
    UnexpectedStatus(u8),
    #[error("incomplete SysEx message")]
    IncompleteSysex,
    #[error("stored slot {0} is outside 0..=255")]
    InvalidSlot(u16),
    #[error("module ID {0} is outside 0..=10")]
    InvalidModule(u8),
    #[error("XML parameter ID {0} is outside 0..=14")]
    InvalidParameter(u8),
    #[error(transparent)]
    Records(#[from] RawError),
}

/// A decoded normal packet. Public fields are revalidated before reassembly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub kind: u8,
    pub total: usize,
    pub offset: usize,
    pub payload: Vec<u8>,
}

impl Packet {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.kind > 0x7f {
            return Err(ProtocolError::InvalidKind(self.kind));
        }
        if !(1..=MAX_TRANSFER_LENGTH).contains(&self.total) {
            return Err(ProtocolError::InvalidTotal(self.total));
        }
        if self.payload.is_empty() {
            return Err(ProtocolError::EmptyChunk);
        }
        if self.offset >= self.total || self.payload.len() > self.total - self.offset {
            return Err(ProtocolError::ChunkOutOfBounds {
                offset: self.offset,
                length: self.payload.len(),
                total: self.total,
            });
        }
        Ok(())
    }
}

/// Decode only normal length/offset/nibble packets. Realtime bytes must first be
/// removed by [`crate::StreamDecoder`]. Legacy short queries are not accepted.
pub fn decode_packet(raw: &[u8]) -> Result<Packet, ProtocolError> {
    if raw.len() < 16
        || raw.len() > MAX_PACKET_LENGTH
        || raw.first() != Some(&0xf0)
        || raw.get(1..8) != Some(MAGIC.as_slice())
        || raw.last() != Some(&0xf7)
    {
        return Err(ProtocolError::NotNormalPacket);
    }
    if let Some(index) = raw[1..raw.len() - 1].iter().position(|byte| *byte > 0x7f) {
        return Err(ProtocolError::StatusInPacket(index + 1));
    }
    let nibbles = &raw[13..raw.len() - 1];
    if !nibbles.len().is_multiple_of(2) {
        return Err(ProtocolError::OddNibbleCount);
    }
    if let Some(index) = nibbles.iter().position(|nibble| *nibble > 15) {
        return Err(ProtocolError::InvalidNibble(index + 13));
    }
    let packet = Packet {
        kind: raw[8],
        total: usize::from(raw[9]) | (usize::from(raw[10]) << 7),
        offset: usize::from(raw[11]) | (usize::from(raw[12]) << 7),
        payload: nibbles
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| (pair[0] << 4) | pair[1])
            .collect(),
    };
    packet.validate()?;
    Ok(packet)
}

/// Encode using the recovered 183-byte application TX profile. This returns
/// complete packets; the transport actor owns pacing between them.
pub fn encode_packets(payload: &[u8], kind: u8) -> Result<Vec<Vec<u8>>, ProtocolError> {
    encode_packets_with_chunk_size(payload, kind, TX_CHUNK_BYTES)
}

/// Device replies use 185 decoded bytes per packet; original host requests
/// use 183. The explicit size supports fixture-backed simulator replies.
pub fn encode_packets_with_chunk_size(
    payload: &[u8],
    kind: u8,
    chunk_size: usize,
) -> Result<Vec<Vec<u8>>, ProtocolError> {
    if kind > 0x7f {
        return Err(ProtocolError::InvalidKind(kind));
    }
    if !(1..=MAX_TRANSFER_LENGTH).contains(&payload.len()) {
        return Err(ProtocolError::InvalidTotal(payload.len()));
    }
    if !(1..=MAX_TRANSFER_LENGTH).contains(&chunk_size) {
        return Err(ProtocolError::InvalidTotal(chunk_size));
    }
    let mut packets = Vec::with_capacity(payload.len().div_ceil(chunk_size));
    for (index, chunk) in payload.chunks(chunk_size).enumerate() {
        let offset = index * chunk_size;
        let mut packet = Vec::with_capacity(14 + chunk.len() * 2);
        packet.push(0xf0);
        packet.extend_from_slice(&MAGIC);
        packet.extend_from_slice(&[
            kind,
            (payload.len() & 0x7f) as u8,
            (payload.len() >> 7) as u8,
            (offset & 0x7f) as u8,
            (offset >> 7) as u8,
        ]);
        for byte in chunk {
            packet.extend_from_slice(&[byte >> 4, byte & 15]);
        }
        packet.push(0xf7);
        packets.push(packet);
    }
    Ok(packets)
}

pub fn encode_transfer(transfer: &RawTransfer) -> Result<Vec<Vec<u8>>, ProtocolError> {
    encode_packets(transfer.payload(), transfer.kind())
}
