use crate::{MAGIC, MAX_PACKET_LENGTH, Packet, ProtocolError, decode_packet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    Packet(Packet),
    /// MIDI realtime bytes do not interrupt SysEx.
    Realtime(u8),
    /// Bytes outside SysEx, retained for a separate MIDI parser/diagnostic log.
    OtherMidi(u8),
    /// Foreign SysEx, legacy short GP-200 messages, or firmware signatures.
    /// Never feed these bytes into the normal record parser.
    UnrecognizedSysex(Vec<u8>),
    Error(ProtocolError),
}

/// Incremental stream splitting across arbitrary callback boundaries. The
/// retained SysEx buffer is limited to [`MAX_PACKET_LENGTH`]. Oversized messages
/// are discarded until their terminator (or a fresh start) without growing it.
#[derive(Debug, Default)]
pub struct StreamDecoder {
    current: Option<Vec<u8>>,
    discarding: bool,
}

impl StreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn buffered_len(&self) -> usize {
        self.current.as_ref().map_or(0, Vec::len)
    }
    pub fn is_incomplete(&self) -> bool {
        self.current.is_some() || self.discarding
    }
    pub fn reset(&mut self) {
        self.current = None;
        self.discarding = false;
    }
    /// Use at capture EOF or session shutdown; do not call after each callback.
    pub fn finish(&mut self) -> Result<(), ProtocolError> {
        let incomplete = self.is_incomplete();
        self.reset();
        if incomplete {
            Err(ProtocolError::IncompleteSysex)
        } else {
            Ok(())
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        for byte in bytes.iter().copied() {
            if byte >= 0xf8 {
                events.push(StreamEvent::Realtime(byte));
                continue;
            }
            if byte == 0xf0 {
                if self.is_incomplete() {
                    events.push(StreamEvent::Error(ProtocolError::NestedSysex));
                }
                self.current = Some(vec![0xf0]);
                self.discarding = false;
                continue;
            }
            if self.discarding {
                if byte == 0xf7 {
                    self.discarding = false;
                }
                continue;
            }
            let Some(current) = &mut self.current else {
                events.push(StreamEvent::OtherMidi(byte));
                continue;
            };
            if byte != 0xf7 && byte > 0x7f {
                self.current = None;
                events.push(StreamEvent::Error(ProtocolError::UnexpectedStatus(byte)));
                events.push(StreamEvent::OtherMidi(byte));
                continue;
            }
            if current.len() >= MAX_PACKET_LENGTH {
                self.current = None;
                self.discarding = byte != 0xf7;
                events.push(StreamEvent::Error(ProtocolError::PacketTooLarge));
                continue;
            }
            current.push(byte);
            if byte == 0xf7 {
                let frame = self.current.take().expect("terminated SysEx exists");
                // The known short address query has no nibble payload and must
                // remain outside the normal decoder. Foreign signatures likewise.
                if frame.len() < 16 || frame.get(1..8) != Some(MAGIC.as_slice()) {
                    events.push(StreamEvent::UnrecognizedSysex(frame));
                } else {
                    match decode_packet(&frame) {
                        Ok(packet) => events.push(StreamEvent::Packet(packet)),
                        Err(error) => events.push(StreamEvent::Error(error)),
                    }
                }
            }
        }
        events
    }
}
