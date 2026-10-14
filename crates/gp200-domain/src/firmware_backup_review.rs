//! Descriptive archival review. No value in this module grants device access.
use crate::{
    ErrorKind, FirmwareDeviceAssociation, FirmwareVersion, ServiceError, UserReportedFirmware,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const FIRMWARE_BACKUP_OBJECT_LIMIT: usize = 288;
pub const FIRMWARE_BACKUP_MATERIAL_LIMIT: usize = 128 * 1024 * 1024;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FirmwareBackupObject {
    Stored(u16),
    Current,
    Globals,
    IrMetadata(u8),
    SnapToneMetadata(u8),
}
impl FirmwareBackupObject {
    pub fn valid(self) -> bool {
        match self {
            Self::Stored(i) => i < 256,
            Self::IrMetadata(i) => i < 20,
            Self::SnapToneMetadata(i) => i < 10,
            _ => true,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FirmwareObservationPass {
    Baseline,
    Comparison,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareBackupDestination {
    pub model: String,
    pub version: FirmwareVersion,
    pub image_sha256: String,
    pub restoration_profile: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareBackupReviewContext {
    pub device: FirmwareDeviceAssociation,
    pub manual_version: Option<UserReportedFirmware>,
    pub destination: FirmwareBackupDestination,
    /// Local reconstruction only, never installed content. Fixed format/path/hash pins.
    pub local_material: Vec<FirmwareBackupLocalMaterial>,
    pub preservation_uncertainty: String,
    pub recovery_limitations: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareBackupLocalMaterial {
    pub object: FirmwareBackupObject,
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub format: String,
    pub provenance: String,
}
fn label(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control)
}
pub fn firmware_backup_hash_valid(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl FirmwareBackupReviewContext {
    pub fn validate(&self) -> Result<(), ServiceError> {
        let valid = self.device.model() == "GP-200"
            && self.destination.model == "GP-200"
            && firmware_backup_hash_valid(&self.destination.image_sha256)
            && self
                .manual_version
                .as_ref()
                .is_none_or(|v| v.device() == &self.device)
            && self
                .destination
                .restoration_profile
                .as_ref()
                .is_none_or(|s| label(s, 512))
            && self.local_material.len() <= 30
            && self.local_material.iter().all(|v| {
                v.object.valid()
                    && label(&v.path, 4096)
                    && firmware_backup_hash_valid(&v.sha256)
                    && v.bytes > 0
                    && v.bytes <= 64 * 1024 * 1024
                    && label(&v.format, 256)
                    && label(&v.provenance, 4096)
            })
            && label(&self.preservation_uncertainty, 4096)
            && label(&self.recovery_limitations, 4096);
        if valid {
            Ok(())
        } else {
            Err(ServiceError::new(
                ErrorKind::InvalidData,
                "Invalid bounded firmware backup review context",
            ))
        }
    }
    pub fn snaptone_applicability(&self) -> FirmwareAssetApplicability {
        if self
            .manual_version
            .as_ref()
            .is_some_and(|v| v.version() < FirmwareVersion::new(1, 7, 4).expect("fixed version"))
        {
            FirmwareAssetApplicability::ConditionalNotApplicable {
                explanation: "Not applicable if the reported version is correct; automatic version read unavailable".into(),
                source: "GP-200 NAM/SnapTone introduction in vendor 1.7.4 notes; descriptive metadata only".into(),
            }
        } else {
            FirmwareAssetApplicability::Unknown
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirmwareAssetApplicability {
    Applicable,
    ConditionalNotApplicable { explanation: String, source: String },
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirmwareRestorationStatus {
    MissingRestorationAcceptance,
    DestinationMigrationRequired,
    UnavailableSource,
    ConditionalNotApplicable,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareBackupCoverageItem {
    pub object: FirmwareBackupObject,
    pub baseline_sha256: Option<String>,
    pub comparison_sha256: Option<String>,
    pub exact_equal: bool,
    /// Installed IR/SnapTone payload provenance only. Always false in FW04-A.
    /// For stored/current/global objects, baseline_sha256 indicates captured raw data.
    /// Neither field establishes an accepted destination restoration route.
    pub installed_content_available: bool,
    pub applicability: FirmwareAssetApplicability,
    pub restoration: FirmwareRestorationStatus,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareBackupAssetReference {
    pub object: FirmwareBackupObject,
    pub module: u8,
    pub code: u32,
    pub classification: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareBackupReviewSummary {
    pub context: FirmwareBackupReviewContext,
    pub coverage: Vec<FirmwareBackupCoverageItem>,
    pub references: Vec<FirmwareBackupAssetReference>,
    pub diagnostics: Vec<String>,
    pub stored_presets: usize,
    pub current_buffer: bool,
    pub globals_and_mappings: bool,
    pub comparison_complete: bool,
    pub label: String,
    pub non_atomic_limitation: String,
}
impl FirmwareBackupReviewSummary {
    /// Even complete-looking serialized summaries cannot acquire private proof.
    pub const fn complete_restorable_backup(&self) -> bool {
        false
    }
    pub const fn permits_flash(&self) -> bool {
        false
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposedFirmwareLossAllowance {
    pub review: FirmwareBackupReviewSummary,
    pub archive_sha256: String,
    pub sidecar_sha256: String,
    pub acquisition: String,
    pub backup_origin_sha256: String,
}
impl ProposedFirmwareLossAllowance {
    pub fn digest(&self) -> Result<String, ServiceError> {
        let bytes = serde_json::to_vec(self)
            .map_err(|e| ServiceError::new(ErrorKind::InvalidData, e.to_string()))?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }
    pub const fn grants_approval(&self) -> bool {
        false
    }
}
