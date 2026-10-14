use crate::{Packet, ProtocolError, RawTransfer};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncompleteTransfer {
    pub kind: u8,
    pub total: usize,
    pub received: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbandonedTransfer {
    pub incomplete: IncompleteTransfer,
    pub age: Duration,
}

#[derive(Debug)]
struct Assembly {
    kind: u8,
    data: Vec<u8>,
    present: Vec<bool>,
    received: usize,
    started: Duration,
    last_timestamp: Duration,
}

impl Assembly {
    fn progress(&self) -> IncompleteTransfer {
        IncompleteTransfer {
            kind: self.kind,
            total: self.data.len(),
            received: self.received,
        }
    }
}

/// Bounded single-transfer reassembly with a hard deadline from the first chunk.
/// Callers provide monotonically increasing timestamps from the same origin.
/// Identical duplicates do not extend the deadline. An offset-zero packet never
/// implicitly resets an incomplete transfer; the header has no transaction ID.
#[derive(Debug)]
pub struct Reassembler {
    timeout: Duration,
    pending: Option<Assembly>,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new(Duration::from_secs(2))
    }
}

impl Reassembler {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            pending: None,
        }
    }
    pub fn progress(&self) -> Option<IncompleteTransfer> {
        self.pending.as_ref().map(Assembly::progress)
    }
    /// Explicit abandonment, for session changes or cancellation.
    pub fn reset(&mut self) -> Option<IncompleteTransfer> {
        self.pending.take().map(|assembly| assembly.progress())
    }
    pub fn expire(&mut self, now: Duration) -> Option<AbandonedTransfer> {
        let assembly = self.pending.as_ref()?;
        let age = now.checked_sub(assembly.started)?;
        if age < self.timeout {
            return None;
        }
        Some(AbandonedTransfer {
            incomplete: self.reset()?,
            age,
        })
    }
    /// Parse the complete payload into ordered raw records, then reset. A record
    /// framing error also clears the completed assembly; unknown bodies remain legal.
    pub fn add(
        &mut self,
        packet: &Packet,
        now: Duration,
    ) -> Result<Option<RawTransfer>, ProtocolError> {
        packet.validate()?;
        if let Some(assembly) = &self.pending {
            if now < assembly.last_timestamp {
                return Err(ProtocolError::TimestampWentBackwards);
            }
            if now - assembly.started >= self.timeout {
                return Err(ProtocolError::TransferExpired);
            }
            if packet.kind != assembly.kind || packet.total != assembly.data.len() {
                return Err(ProtocolError::InterleavedTransfer {
                    expected_kind: assembly.kind,
                    expected_total: assembly.data.len(),
                    actual_kind: packet.kind,
                    actual_total: packet.total,
                });
            }
        }
        let assembly = self.pending.get_or_insert_with(|| Assembly {
            kind: packet.kind,
            data: vec![0; packet.total],
            present: vec![false; packet.total],
            received: 0,
            started: now,
            last_timestamp: now,
        });
        // Validate the entire overlap first. An error never partly changes state.
        for (relative, byte) in packet.payload.iter().enumerate() {
            let offset = packet.offset + relative;
            if assembly.present[offset] && assembly.data[offset] != *byte {
                return Err(ProtocolError::ConflictingOverlap(offset));
            }
        }
        for (relative, byte) in packet.payload.iter().enumerate() {
            let offset = packet.offset + relative;
            if !assembly.present[offset] {
                assembly.data[offset] = *byte;
                assembly.present[offset] = true;
                assembly.received += 1;
            }
        }
        assembly.last_timestamp = now;
        if assembly.received != assembly.data.len() {
            return Ok(None);
        }
        let complete = self.pending.take().expect("complete assembly exists");
        Ok(Some(RawTransfer::from_payload(
            complete.kind,
            complete.data,
        )?))
    }
}
