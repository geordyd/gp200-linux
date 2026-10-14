//! Bounded descriptions of capture evidence. These values confer no I/O authority.
use crate::*;
use serde::{Deserialize, Serialize};

pub const FIRMWARE_CAPTURE_RECORD_LIMIT: usize = 600;
pub const FIRMWARE_CAPTURE_CHUNK_LIMIT: usize = 256;
pub const FIRMWARE_CAPTURE_RECORD_MATERIAL_LIMIT: usize = 1024 * 1024;
pub const FIRMWARE_CAPTURE_INDEX_LIMIT: usize = 2 * 1024 * 1024;
pub const FIRMWARE_CAPTURE_DIAGNOSTIC_LIMIT: usize = 512;

fn label(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareCaptureFence {
    pub allocated: u64,
    pub processed: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareCaptureReservation {
    pub actor_run: String,
    pub reservation: String,
    pub normal_owner: String,
    pub epoch: SessionEpoch,
    pub usb: FirmwareUsbObservation,
    pub clock_origin: String,
    pub prepared_monotonic_ns: u64,
    pub prepared_wall_millis: u64,
    pub initial_fence: FirmwareCaptureFence,
}
impl FirmwareCaptureReservation {
    pub fn validate(&self) -> Result<(), ServiceError> {
        if [
            &self.actor_run,
            &self.reservation,
            &self.normal_owner,
            &self.clock_origin,
        ]
        .iter()
        .any(|v| !label(v, 256))
            || self.epoch.0 == 0
            || self.initial_fence.allocated != self.initial_fence.processed
        {
            return Err(ServiceError::invalid(
                "Capture reservation is incomplete or unfenced",
            ));
        }
        self.usb.validate()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirmwareCaptureBoundaryKind {
    Opening,
    Middle,
    Final,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirmwareCaptureRead {
    Identity(FirmwareCaptureBoundaryKind),
    Object {
        object: FirmwareBackupObject,
        pass: FirmwareObservationPass,
    },
    Boundary {
        boundary: FirmwareCaptureBoundaryKind,
        object: FirmwareBackupObject,
    },
}
impl FirmwareCaptureRead {
    /// Exact canonical single-packet normal read description; constructing it opens no device.
    /// Asset metadata constructors remain candidates whose physical support is separate.
    pub fn request_bytes(&self) -> Result<Vec<u8>, ServiceError> {
        use gp200_protocol::*;
        self.validate()?;
        let object = match self {
            Self::Identity(_) => None,
            Self::Object { object, .. } | Self::Boundary { object, .. } => Some(*object),
        };
        let transfer = match object {
            None => identify_request(),
            Some(FirmwareBackupObject::Current) => read_current_request(),
            Some(FirmwareBackupObject::Stored(slot)) => {
                read_stored_request(slot).map_err(|e| ServiceError::invalid(e.to_string()))?
            }
            Some(FirmwareBackupObject::Globals) => {
                read_globals_request().map_err(|e| ServiceError::invalid(e.to_string()))?
            }
            Some(FirmwareBackupObject::IrMetadata(slot)) => asset_metadata_read_candidate(
                AssetSlot::new(AssetBank::Ir, u16::from(slot))
                    .map_err(|e| ServiceError::invalid(e.to_string()))?,
            ),
            Some(FirmwareBackupObject::SnapToneMetadata(slot)) => asset_metadata_read_candidate(
                AssetSlot::new(AssetBank::SnapTone, u16::from(slot))
                    .map_err(|e| ServiceError::invalid(e.to_string()))?,
            ),
        };
        let mut packets =
            encode_transfer(&transfer).map_err(|e| ServiceError::invalid(e.to_string()))?;
        if packets.len() != 1 {
            return Err(ServiceError::invalid(
                "Capture read requires exactly one bounded packet",
            ));
        }
        Ok(packets.remove(0))
    }
    pub fn validate(&self) -> Result<(), ServiceError> {
        match self {
            Self::Object { object, .. } if object.valid() => Ok(()),
            Self::Object { .. } => {
                Err(ServiceError::invalid("Capture object address exceeds bank"))
            }
            Self::Boundary {
                boundary: FirmwareCaptureBoundaryKind::Middle,
                object: FirmwareBackupObject::Current | FirmwareBackupObject::Globals,
            } => Ok(()),
            Self::Boundary { .. } => Err(ServiceError::invalid(
                "Capture boundary object is not middle current/globals",
            )),
            Self::Identity(_) => Ok(()),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareCaptureCallback {
    /// Allocated before bounds checking/copying, including rejected callbacks.
    pub ordinal: u64,
    /// Original callback session, retained even when rejected as stale.
    pub epoch: SessionEpoch,
    pub monotonic_ns: Option<u64>,
    pub original_len: u64,
    pub retained: Vec<u8>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareCaptureMissingExtent {
    pub first_ordinal: u64,
    pub last_ordinal: u64,
    pub reason: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareCaptureOfferInterval {
    pub started_ns: u64,
    pub returned_ns: u64,
    /// Host accepted byte extent only, never a device acknowledgement.
    pub accepted_bytes: u64,
    pub requested_bytes: u64,
}
/// Caller-reported raw observations remain descriptive, even with complete-looking fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareCaptureEvidence {
    pub read: FirmwareCaptureRead,
    pub sequence: u64,
    pub before: FirmwareCaptureFence,
    pub after: FirmwareCaptureFence,
    pub offer: Option<FirmwareCaptureOfferInterval>,
    /// Exact packet passed to the first backend write, empty when no offer was attempted.
    pub offered_read_bytes: Vec<u8>,
    pub completed_ns: Option<u64>,
    pub callbacks: Vec<FirmwareCaptureCallback>,
    pub missing: Vec<FirmwareCaptureMissingExtent>,
    /// Kind-prefixed original decoded transfer; metadata uses canonical RawRecord JSON
    /// preserving the exact command/body. Original framed wire chunks remain in callbacks.
    pub raw: Vec<u8>,
    pub complete: bool,
    pub diagnostics: Vec<String>,
}
impl FirmwareCaptureEvidence {
    pub fn validate(&self) -> Result<(), ServiceError> {
        self.read.validate()?;
        if self.sequence == 0
            || self.callbacks.len() > FIRMWARE_CAPTURE_CHUNK_LIMIT
            || self.missing.len() > FIRMWARE_CAPTURE_DIAGNOSTIC_LIMIT
            || self.diagnostics.len() > FIRMWARE_CAPTURE_DIAGNOSTIC_LIMIT
            || self.diagnostics.iter().any(|s| !label(s, 4096))
            || self.missing.iter().any(|m| {
                m.first_ordinal == 0 || m.last_ordinal < m.first_ordinal || !label(&m.reason, 4096)
            })
            || self.offered_read_bytes.len() > gp200_protocol::MAX_PACKET_LENGTH
            || self.raw.len() > 65_537
            || matches!(self.read, FirmwareCaptureRead::Identity(_)) && self.raw.len() > 2049
            || matches!(
                self.read,
                FirmwareCaptureRead::Object {
                    object: FirmwareBackupObject::IrMetadata(_)
                        | FirmwareBackupObject::SnapToneMetadata(_),
                    ..
                }
            ) && self.raw.len() > 8192
        {
            return Err(ServiceError::invalid("Capture evidence exceeds bounds"));
        }
        let material = self
            .callbacks
            .iter()
            .try_fold(self.raw.len() + self.offered_read_bytes.len(), |size, c| {
                if c.ordinal == 0
                    || c.retained.len() > gp200_protocol::MAX_PACKET_LENGTH
                    || c.original_len < c.retained.len() as u64
                {
                    return None;
                }
                size.checked_add(c.retained.len())
            })
            .ok_or_else(|| ServiceError::invalid("Capture callback extent differs"))?;
        if material > FIRMWARE_CAPTURE_RECORD_MATERIAL_LIMIT {
            return Err(ServiceError::invalid(
                "Capture record material exceeds budget",
            ));
        }
        if let Some(offer) = self.offer {
            if offer.requested_bytes != self.offered_read_bytes.len() as u64
                || offer.accepted_bytes > offer.requested_bytes
                || self.offered_read_bytes.is_empty()
            {
                return Err(ServiceError::invalid(
                    "Capture offered packet/extent differs",
                ));
            }
        } else if !self.offered_read_bytes.is_empty() {
            return Err(ServiceError::invalid(
                "Capture offered packet lacks an actual offer interval",
            ));
        }
        if !self.complete {
            return Ok(());
        }
        if self.offered_read_bytes != self.read.request_bytes()? {
            return Err(ServiceError::invalid(
                "Complete capture offered read/object/slot differs",
            ));
        }
        let offer = self
            .offer
            .ok_or_else(|| ServiceError::invalid("Complete evidence lacks offer interval"))?;
        let completed = self
            .completed_ns
            .ok_or_else(|| ServiceError::invalid("Complete evidence lacks completion time"))?;
        if self.raw.is_empty()
            || !self.missing.is_empty()
            || self.callbacks.is_empty()
            || self.before.allocated != self.before.processed
            || self.after.allocated != self.after.processed
            || offer.returned_ns < offer.started_ns
            || completed < offer.returned_ns
            || offer.requested_bytes == 0
            || offer.accepted_bytes != offer.requested_bytes
        {
            return Err(ServiceError::invalid(
                "Complete evidence has an unresolved offer/fence/extent",
            ));
        }
        let mut ordinal = self.before.allocated;
        let mut time = offer.started_ns;
        for callback in &self.callbacks {
            let t = callback
                .monotonic_ns
                .ok_or_else(|| ServiceError::invalid("Callback source time unavailable"))?;
            if callback.epoch.0 == 0
                || ordinal.checked_add(1) != Some(callback.ordinal)
                || t < time
                || t > completed
                || callback.retained.is_empty()
                || callback.original_len != callback.retained.len() as u64
            {
                return Err(ServiceError::invalid(
                    "Complete callback order/time/extent differs",
                ));
            }
            ordinal = callback.ordinal;
            time = t;
        }
        if ordinal != self.after.allocated {
            return Err(ServiceError::invalid(
                "Capture final callback fence differs",
            ));
        }
        Ok(())
    }
    /// Validates descriptive correlation with the actual prepared capture reservation.
    pub fn validate_for_reservation(
        &self,
        reservation: &FirmwareCaptureReservation,
    ) -> Result<(), ServiceError> {
        self.validate()?;
        if self.complete && self.callbacks.iter().any(|c| c.epoch != reservation.epoch) {
            return Err(ServiceError::invalid(
                "Complete capture callback session differs from reservation",
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirmwareCaptureReceiptKind {
    Prepared,
    Begun,
    Observation,
    Boundary,
    Failure,
    Closed,
    Published,
    Revalidated,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareCaptureReceiptDescription {
    pub worker_run: String,
    pub worker_sequence: u64,
    pub capture_id: String,
    pub acquisition: Option<String>,
    pub reservation: FirmwareCaptureReservation,
    pub kind: FirmwareCaptureReceiptKind,
    pub read: Option<FirmwareCaptureRead>,
    pub observation_sequence: Option<u64>,
    pub raw_sha256: String,
    pub evidence_sha256: String,
}
/// Reopening produces descriptions only; no receipt, live provenance or flash permission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareCaptureEvidenceSummary {
    pub capture_id: String,
    pub acquisition: Option<String>,
    pub records: usize,
    pub serialized_bytes: u64,
    pub index_sha256: String,
    pub archive_sha256: String,
    pub sidecar_sha256: String,
    pub closed: bool,
    pub cancelled: bool,
    pub limitations: String,
}
impl FirmwareCaptureEvidenceSummary {
    pub fn complete_restorable_backup(&self) -> bool {
        false
    }
    pub fn permits_flash(&self) -> bool {
        false
    }
}

/// Source labels are descriptive. Only the concrete guarded backend creates its
/// private system observation; imported/replay labels confer no physical grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirmwareCaptureOrigin {
    GuardedSystemNormal,
    Replay,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareCaptureDifference {
    pub record: usize,
    pub command: u16,
    pub body_offset: usize,
    pub baseline: u8,
    pub comparison: u8,
    pub admitted: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareCaptureConsistency {
    pub policy: String,
    pub strict_raw_equal: bool,
    pub admitted: bool,
    pub differences: Vec<FirmwareCaptureDifference>,
}
/// This support description cannot submit a receipt across the domain boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirmwareCaptureSupport {
    Unavailable,
    ConcreteClientRequired,
}

/// Canonical descriptive RawRecord bytes required by the unchanged archival
/// metadata representation. Original wire callback chunks remain separate.
/// This pure serializer grants no physical provenance, receipt or authority.
pub fn firmware_capture_metadata_record_bytes(
    record: &gp200_protocol::RawRecord,
) -> Result<Vec<u8>, ServiceError> {
    if !matches!(record.command, 0x1009 | 0x1012) || record.body.len() != 24 {
        return Err(ServiceError::invalid(
            "Capture metadata record shape differs",
        ));
    }
    let bytes =
        serde_json::to_vec(record).map_err(|error| ServiceError::invalid(error.to_string()))?;
    if bytes.len() > 8192 {
        return Err(ServiceError::invalid(
            "Capture metadata serialization exceeds bound",
        ));
    }
    Ok(bytes)
}
