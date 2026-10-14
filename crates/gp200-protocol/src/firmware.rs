//! Bounded offline bootloader codecs recovered from the original host program.
//! No transport integration, entry/reset commands, or flash executor.
//! A complete updater additionally needs captured handshake, target binding,
//! deadlines, retry/recovery, and reconnect evidence before any hardware use.
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
pub enum FirmwareWireError {
    #[error("packet does not have the recovered bootloader signature/shape")]
    InvalidShape,
    #[error("bootloader nibble is outside 0..15")]
    InvalidNibble,
    #[error("negotiated firmware chunk length must be 1..255 bytes")]
    InvalidChunkLength,
    #[error("firmware block input must have 1..negotiated chunk bytes")]
    InvalidBlockLength,
    #[error("firmware block nibble checksum mismatch")]
    ChecksumMismatch,
    #[error("terminal body must contain an even number of nibbles")]
    OddNibbleCount,
    #[error("terminal decoded count exceeds the native u8 field; narrowing is rejected")]
    TerminalCountOverflow,
    #[error(
        "firmware packet exceeds the 518-byte offline evidence bound; no raw copy was retained"
    )]
    PacketTooLarge,
    #[error("section selector is not in the recovered set")]
    InvalidSectionSelector,
    #[error("image provenance requires an exact 92-byte FRMW header/tag snapshot")]
    InvalidImageHeader,
    #[error("strict image announcement requires a recognized B/b/V/v source tag")]
    UnrecognizedImageTag,
    #[error(
        "strict image announcement requires three ASCII source digits and normalized digits 0..9"
    )]
    InvalidImageVersion,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BootloaderIdentity {
    pub product_raw: [u8; 16],
    pub version_raw: [u8; 16],
    pub chunk_bytes: u16,
    raw: Vec<u8>,
}
impl BootloaderIdentity {
    pub fn decode(packet: &[u8]) -> Result<Self, FirmwareWireError> {
        if packet.len() != 39
            || packet[..4] != [0xf0, 0x7e, 0x48, 1]
            || packet[38] != 0xf7
            || packet[1..38].iter().any(|byte| *byte > 127)
        {
            return Err(FirmwareWireError::InvalidShape);
        }
        if packet[36] > 15 || packet[37] > 15 {
            return Err(FirmwareWireError::InvalidNibble);
        }
        let chunk_bytes = u16::from(packet[36]) * 16 + u16::from(packet[37]);
        if chunk_bytes == 0 {
            return Err(FirmwareWireError::InvalidChunkLength);
        }
        Ok(Self {
            product_raw: packet[4..20].try_into().expect("16 bytes"),
            version_raw: packet[20..36].try_into().expect("16 bytes"),
            chunk_bytes,
            raw: packet.to_vec(),
        })
    }
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }
}
/// Wire block index wraps modulo128 in the native sender; it is not a unique
/// transaction or offset. `data_padded` retains native FF padding verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FirmwareBlock {
    pub block_index_mod128: u8,
    pub checksum: u8,
    pub data_padded: Vec<u8>,
    raw: Vec<u8>,
}
impl FirmwareBlock {
    pub fn decode(packet: &[u8], chunk_bytes: u16) -> Result<Self, FirmwareWireError> {
        valid_chunk(chunk_bytes)?;
        let nibbles = usize::from(chunk_bytes) * 2;
        if packet.len() != nibbles + 8
            || packet[..4] != [0xf0, 0x7e, 0x48, 4]
            || packet[4] > 127
            || packet[packet.len() - 1] != 0xf7
        {
            return Err(FirmwareWireError::InvalidShape);
        }
        let content = &packet[5..5 + nibbles];
        let checksum_nibbles = &packet[5 + nibbles..7 + nibbles];
        if content
            .iter()
            .chain(checksum_nibbles)
            .any(|nibble| *nibble > 15)
        {
            return Err(FirmwareWireError::InvalidNibble);
        }
        let checksum = content
            .iter()
            .fold(0_u8, |sum, nibble| sum.wrapping_add(*nibble));
        if checksum != ((checksum_nibbles[0] << 4) | checksum_nibbles[1]) {
            return Err(FirmwareWireError::ChecksumMismatch);
        }
        Ok(Self {
            block_index_mod128: packet[4],
            checksum,
            data_padded: content
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| (pair[0] << 4) | pair[1])
                .collect(),
            raw: packet.to_vec(),
        })
    }
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }
}
fn valid_chunk(chunk_bytes: u16) -> Result<(), FirmwareWireError> {
    if !(1..=255).contains(&chunk_bytes) {
        return Err(FirmwareWireError::InvalidChunkLength);
    }
    Ok(())
}
/// Build exactly one offline candidate. Its output must not be treated as an
/// updater plan or sent by the normal-message actor (different protocol).
pub fn firmware_block_candidate(
    block_index: u32,
    chunk_bytes: u16,
    data: &[u8],
) -> Result<FirmwareBlock, FirmwareWireError> {
    valid_chunk(chunk_bytes)?;
    if data.is_empty() || data.len() > usize::from(chunk_bytes) {
        return Err(FirmwareWireError::InvalidBlockLength);
    }
    let mut raw = Vec::with_capacity(usize::from(chunk_bytes) * 2 + 8);
    raw.extend_from_slice(&[0xf0, 0x7e, 0x48, 4, (block_index % 128) as u8]);
    for index in 0..usize::from(chunk_bytes) {
        let byte = data.get(index).copied().unwrap_or(0xff);
        raw.extend_from_slice(&[byte >> 4, byte & 15]);
    }
    let sum = raw[5..]
        .iter()
        .fold(0_u8, |sum, nibble| sum.wrapping_add(*nibble));
    raw.extend_from_slice(&[sum >> 4, sum & 15, 0xf7]);
    FirmwareBlock::decode(&raw, chunk_bytes)
}

/// Largest recovered complete block (chunk255). Oversized input is rejected
/// before copying; callers retain their own original oversized bytes.
pub const MAX_FIRMWARE_PACKET_BYTES: usize = 518;
pub const FIRMWARE_VERSION_TAG_OFFSET: usize = 88;

fn valid_frame(packet: &[u8]) -> Result<(), FirmwareWireError> {
    if packet.len() < 5
        || packet[..3] != [0xf0, 0x7e, 0x48]
        || packet[packet.len() - 1] != 0xf7
        || packet[1..packet.len() - 1].iter().any(|byte| *byte > 127)
    {
        return Err(FirmwareWireError::InvalidShape);
    }
    Ok(())
}
fn decode_nibbles(nibbles: &[u8]) -> Result<Vec<u8>, FirmwareWireError> {
    if !nibbles.len().is_multiple_of(2) {
        return Err(FirmwareWireError::OddNibbleCount);
    }
    if nibbles.iter().any(|nibble| *nibble > 15) {
        return Err(FirmwareWireError::InvalidNibble);
    }
    Ok(nibbles
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| (pair[0] << 4) | pair[1])
        .collect())
}
fn append_nibbles(output: &mut Vec<u8>, bytes: &[u8]) {
    for byte in bytes {
        output.extend_from_slice(&[byte >> 4, byte & 15]);
    }
}

/// Exact fixed shapes. Marker/error meanings, commit and reboot remain unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum FirmwareFixedPacket {
    IdentityQuery,
    TerminalQuery,
    TerminalControl,
    SectionTerminator,
    NativeGlobalErrorCandidate,
}
impl FirmwareFixedPacket {
    pub fn raw(self) -> &'static [u8] {
        match self {
            Self::IdentityQuery => &[0xf0, 0x7e, 0x48, 1, 0xf7],
            Self::TerminalQuery => &[0xf0, 0x7e, 0x48, 7, 0xf7],
            Self::TerminalControl => &[0xf0, 0x7e, 0x48, 9, 0xf7],
            Self::SectionTerminator => &[0xf0, 0x7e, 0x48, 5, 0x7b, 0xf7],
            Self::NativeGlobalErrorCandidate => &[0xf0, 0x7e, 0x48, 6, 0xf7],
        }
    }
    pub fn decode(packet: &[u8]) -> Result<Self, FirmwareWireError> {
        for kind in [
            Self::IdentityQuery,
            Self::TerminalQuery,
            Self::TerminalControl,
            Self::SectionTerminator,
            Self::NativeGlobalErrorCandidate,
        ] {
            if packet == kind.raw() {
                return Ok(kind);
            }
        }
        Err(FirmwareWireError::InvalidShape)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum FirmwareSectionSelector {
    Section0,
    Section1,
    Section2,
    Section3,
    Terminal,
}
impl FirmwareSectionSelector {
    /// Native transfer order; enum section numbers describe wire phases, not
    /// raw FRMW table slots. The parser permutes slots to [0, 2, 3, 1].
    pub const IMAGE_TRANSFER_ORDER: [Self; 4] = [
        Self::Section0,
        Self::Section1,
        Self::Section2,
        Self::Section3,
    ];
    pub fn from_raw(value: u8) -> Result<Self, FirmwareWireError> {
        match value {
            0 => Ok(Self::Section0),
            1 => Ok(Self::Section1),
            2 => Ok(Self::Section2),
            0x10 => Ok(Self::Section3),
            0x11 => Ok(Self::Terminal),
            _ => Err(FirmwareWireError::InvalidSectionSelector),
        }
    }
    pub fn raw(self) -> u8 {
        match self {
            Self::Section0 => 0,
            Self::Section1 => 1,
            Self::Section2 => 2,
            Self::Section3 => 0x10,
            Self::Terminal => 0x11,
        }
    }
    /// Raw FRMW table slot selected by this wire phase. Terminal is a control
    /// selector, not a fifth image section.
    pub fn image_table_slot(self) -> Option<usize> {
        match self {
            Self::Section0 => Some(0),
            Self::Section1 => Some(2),
            Self::Section2 => Some(3),
            Self::Section3 => Some(1),
            Self::Terminal => None,
        }
    }
    /// Inverse mapping for metadata retained in raw FRMW table order.
    pub fn for_image_table_slot(slot: usize) -> Option<Self> {
        match slot {
            0 => Some(Self::Section0),
            1 => Some(Self::Section3),
            2 => Some(Self::Section1),
            3 => Some(Self::Section2),
            _ => None,
        }
    }
    pub fn request_candidate(self) -> [u8; 6] {
        [0xf0, 0x7e, 0x48, 0x7f, self.raw(), 0xf7]
    }
    pub fn decode_request(packet: &[u8]) -> Result<Self, FirmwareWireError> {
        if packet.len() != 6 || packet[..4] != [0xf0, 0x7e, 0x48, 0x7f] || packet[5] != 0xf7 {
            return Err(FirmwareWireError::InvalidShape);
        }
        Self::from_raw(packet[4])
    }
}

/// Descriptor has no section identifier, checksum or ACK correlation field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FirmwareDescriptor {
    section_bytes: u32,
    block_count: u32,
    raw: Vec<u8>,
}
impl FirmwareDescriptor {
    pub fn candidate(section_bytes: u32, block_count: u32) -> Self {
        let mut raw = Vec::with_capacity(21);
        raw.extend_from_slice(&[0xf0, 0x7e, 0x48, 2]);
        append_nibbles(&mut raw, &section_bytes.to_be_bytes());
        append_nibbles(&mut raw, &block_count.to_be_bytes());
        raw.push(0xf7);
        Self {
            section_bytes,
            block_count,
            raw,
        }
    }
    pub fn decode(packet: &[u8]) -> Result<Self, FirmwareWireError> {
        if packet.len() != 21 || packet[..4] != [0xf0, 0x7e, 0x48, 2] || packet[20] != 0xf7 {
            return Err(FirmwareWireError::InvalidShape);
        }
        let bytes = decode_nibbles(&packet[4..20])?;
        Ok(Self {
            section_bytes: u32::from_be_bytes(bytes[..4].try_into().expect("four decoded bytes")),
            block_count: u32::from_be_bytes(bytes[4..].try_into().expect("four decoded bytes")),
            raw: packet.to_vec(),
        })
    }
    pub fn section_bytes(&self) -> u32 {
        self.section_bytes
    }
    pub fn block_count(&self) -> u32 {
        self.block_count
    }
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }
}

/// Exact raw header snapshot and a separate observation of native normalization.
/// This is image provenance only, without installed-device or compatibility proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FirmwareImageProvenance {
    raw_header: Vec<u8>,
    raw_words: [u16; 4],
    native_normalized_words: [u16; 4],
    product_raw: [u8; 16],
    metadata_raw: [u8; 16],
    native_normalized_metadata: [u8; 16],
    version_tag_raw: [u8; 4],
    version_tag_file_offset: usize,
    tag_recognized: bool,
    tag_digits_valid: bool,
}
impl FirmwareImageProvenance {
    pub fn from_header(header: &[u8]) -> Result<Self, FirmwareWireError> {
        if header.len() != 92 || &header[..4] != b"FRMW" {
            return Err(FirmwareWireError::InvalidImageHeader);
        }
        let raw_words = std::array::from_fn(|index| {
            u16::from_le_bytes(
                header[8 + index * 2..10 + index * 2]
                    .try_into()
                    .expect("bounded raw word"),
            )
        });
        let metadata_raw: [u8; 16] = header[32..48].try_into().expect("bounded metadata");
        let version_tag_raw: [u8; 4] = header[88..92].try_into().expect("bounded tag");
        let tag_recognized = matches!(version_tag_raw[0], b'B' | b'b' | b'V' | b'v');
        let tag_digits_valid = version_tag_raw[1..].iter().all(u8::is_ascii_digit);
        let mut native_normalized_words = raw_words;
        let mut native_normalized_metadata = metadata_raw;
        if tag_recognized {
            native_normalized_words[0] = if version_tag_raw[0] == b'B' { 1 } else { 3 };
            // Original instructions subtract in an 8-bit register, then sign extend.
            let major = i32::from(version_tag_raw[1].wrapping_sub(b'0') as i8);
            let minor = i32::from(version_tag_raw[2].wrapping_sub(b'0') as i8);
            native_normalized_words[1] = (major * 256 + minor) as u16;
            native_normalized_metadata[1] = version_tag_raw[3].wrapping_sub(b'0');
        }
        Ok(Self {
            raw_header: header.to_vec(),
            raw_words,
            native_normalized_words,
            product_raw: header[16..32].try_into().expect("bounded product"),
            metadata_raw,
            native_normalized_metadata,
            version_tag_raw,
            version_tag_file_offset: FIRMWARE_VERSION_TAG_OFFSET,
            tag_recognized,
            tag_digits_valid,
        })
    }
    pub fn raw_header(&self) -> &[u8] {
        &self.raw_header
    }
    pub fn raw_words(&self) -> [u16; 4] {
        self.raw_words
    }
    pub fn native_normalized_words(&self) -> [u16; 4] {
        self.native_normalized_words
    }
    pub fn product_raw(&self) -> &[u8; 16] {
        &self.product_raw
    }
    pub fn metadata_raw(&self) -> &[u8; 16] {
        &self.metadata_raw
    }
    pub fn native_normalized_metadata(&self) -> &[u8; 16] {
        &self.native_normalized_metadata
    }
    pub fn version_tag_raw(&self) -> &[u8; 4] {
        &self.version_tag_raw
    }
    pub fn version_tag_file_offset(&self) -> usize {
        self.version_tag_file_offset
    }
    pub fn tag_recognized(&self) -> bool {
        self.tag_recognized
    }
    pub fn tag_digits_valid(&self) -> bool {
        self.tag_digits_valid
    }
    pub fn announcement_candidate(&self) -> Result<FirmwareAnnouncement, FirmwareWireError> {
        if !self.tag_recognized {
            return Err(FirmwareWireError::UnrecognizedImageTag);
        }
        let version = self.native_normalized_words[1];
        let digits = [
            (version >> 8) as u8,
            version as u8,
            self.native_normalized_metadata[1],
        ];
        if !self.tag_digits_valid || digits.iter().any(|digit| *digit > 9) {
            return Err(FirmwareWireError::InvalidImageVersion);
        }
        let family = if self.native_normalized_words[0] == 3 {
            b'V'
        } else {
            b'B'
        };
        let raw = vec![
            0xf0,
            0x7e,
            0x48,
            0x0b,
            1,
            family,
            digits[0] + b'0',
            digits[1] + b'0',
            digits[2] + b'0',
            0xf7,
        ];
        Ok(FirmwareAnnouncement {
            raw,
            image_provenance: Some(self.clone()),
        })
    }
}
/// Decoded wire announcements have no image provenance; only the strict header
/// constructor supplies it. Neither source is an installed-version attestation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FirmwareAnnouncement {
    raw: Vec<u8>,
    image_provenance: Option<FirmwareImageProvenance>,
}
impl FirmwareAnnouncement {
    pub fn decode(packet: &[u8]) -> Result<Self, FirmwareWireError> {
        if packet.len() != 10
            || packet[..5] != [0xf0, 0x7e, 0x48, 0x0b, 1]
            || !matches!(packet[5], b'B' | b'V')
            || !packet[6..9].iter().all(u8::is_ascii_digit)
            || packet[9] != 0xf7
        {
            return Err(FirmwareWireError::InvalidShape);
        }
        Ok(Self {
            raw: packet.to_vec(),
            image_provenance: None,
        })
    }
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }
    pub fn image_provenance(&self) -> Option<&FirmwareImageProvenance> {
        self.image_provenance.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum FirmwareRxCandidate {
    BootloaderIdentity(BootloaderIdentity),
    NativeGlobalError,
    TerminalData { decoded: Vec<u8> },
    OpaqueStatus,
}
/// An exact bounded receive candidate, including rejected bytes/reason. Direction
/// and actual device origin are not proven by construction. No status is an ACK.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FirmwareRxEvidence {
    raw: Vec<u8>,
    opcode_raw: Option<u8>,
    offset4_raw: Option<u8>,
    payload_raw: Vec<u8>,
    classification: Result<FirmwareRxCandidate, FirmwareWireError>,
}
impl FirmwareRxEvidence {
    pub fn retain(packet: &[u8]) -> Result<Self, FirmwareWireError> {
        if packet.len() > MAX_FIRMWARE_PACKET_BYTES {
            return Err(FirmwareWireError::PacketTooLarge);
        }
        let classification = Self::classify(packet);
        let payload_end = if packet.last() == Some(&0xf7) {
            packet.len().saturating_sub(1)
        } else {
            packet.len()
        };
        let payload_raw = packet.get(4..payload_end).unwrap_or_default().to_vec();
        Ok(Self {
            raw: packet.to_vec(),
            opcode_raw: packet.get(3).copied(),
            offset4_raw: packet.get(4).copied(),
            payload_raw,
            classification,
        })
    }
    fn classify(packet: &[u8]) -> Result<FirmwareRxCandidate, FirmwareWireError> {
        valid_frame(packet)?;
        match packet[3] {
            1 => BootloaderIdentity::decode(packet).map(FirmwareRxCandidate::BootloaderIdentity),
            6 => {
                if packet == FirmwareFixedPacket::NativeGlobalErrorCandidate.raw() {
                    Ok(FirmwareRxCandidate::NativeGlobalError)
                } else {
                    Err(FirmwareWireError::InvalidShape)
                }
            }
            3 | 7 => {
                let decoded = decode_nibbles(&packet[4..packet.len() - 1])?;
                if decoded.len() > usize::from(u8::MAX) {
                    return Err(FirmwareWireError::TerminalCountOverflow);
                }
                Ok(FirmwareRxCandidate::TerminalData { decoded })
            }
            _ => Ok(FirmwareRxCandidate::OpaqueStatus),
        }
    }
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }
    pub fn opcode_raw(&self) -> Option<u8> {
        self.opcode_raw
    }
    pub fn offset4_raw(&self) -> Option<u8> {
        self.offset4_raw
    }
    pub fn payload_raw(&self) -> &[u8] {
        &self.payload_raw
    }
    pub fn classification(&self) -> &Result<FirmwareRxCandidate, FirmwareWireError> {
        &self.classification
    }
}
