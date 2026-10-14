use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvidenceLevel {
    LiveVerified,
    StaticRecovered,
    Inferred,
    Proposed,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub path: String,
    pub locator: Option<String>,
    pub sha256: Option<String>,
    pub level: EvidenceLevel,
}
impl EvidenceRef {
    pub fn new(path: impl Into<String>, level: EvidenceLevel) -> Self {
        Self {
            path: path.into(),
            locator: None,
            sha256: None,
            level,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Recovered<T> {
    Unknown,
    Candidate {
        value: T,
        evidence: Vec<EvidenceRef>,
    },
    Confirmed {
        value: T,
        evidence: Vec<EvidenceRef>,
    },
}
impl<T> Recovered<T> {
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Unknown => None,
            Self::Candidate { value, .. } | Self::Confirmed { value, .. } => Some(value),
        }
    }
    pub fn is_confirmed(&self) -> bool {
        matches!(self, Self::Confirmed { .. })
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum DeviceModel {
    Gp200,
    Gp200Lt,
    Gp200Jr,
    Unknown(u16),
}
impl DeviceModel {
    pub fn name(self) -> String {
        match self {
            Self::Gp200 => "GP-200".into(),
            Self::Gp200Lt => "GP-200LT".into(),
            Self::Gp200Jr => "GP-200JR".into(),
            Self::Unknown(id) => format!("Unknown model {id}"),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum BackendMode {
    Hardware,
    Simulator,
    CaptureReplay,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum OperationKind {
    Identify,
    ReadEditBuffer,
    ReadStoredPreset,
    SelectStoredPreset,
    SetParameter,
    SetModuleEnabled,
    SetModel,
    SetChain,
    SetMetadata,
    SetExpression,
    SetFootswitch,
    SetQuickControl,
    ReadGlobals,
    WriteGlobals,
    SavePreset,
    ClearPreset,
    PresetImport,
    PresetExport,
    IrRead,
    IrWrite,
    SnapToneRead,
    SnapToneWrite,
    FirmwareUpdate,
    FactoryReset,
}
impl OperationKind {
    pub const ALL: [Self; 24] = [
        Self::Identify,
        Self::ReadEditBuffer,
        Self::ReadStoredPreset,
        Self::SelectStoredPreset,
        Self::SetParameter,
        Self::SetModuleEnabled,
        Self::SetModel,
        Self::SetChain,
        Self::SetMetadata,
        Self::SetExpression,
        Self::SetFootswitch,
        Self::SetQuickControl,
        Self::ReadGlobals,
        Self::WriteGlobals,
        Self::SavePreset,
        Self::ClearPreset,
        Self::PresetImport,
        Self::PresetExport,
        Self::IrRead,
        Self::IrWrite,
        Self::SnapToneRead,
        Self::SnapToneWrite,
        Self::FirmwareUpdate,
        Self::FactoryReset,
    ];
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareIdentity {
    pub version: String,
    pub raw: Vec<u8>,
    pub evidence: Vec<EvidenceRef>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceIdentity {
    pub model: DeviceModel,
    pub mode: BackendMode,
    pub firmware: Option<FirmwareIdentity>,
    pub display_name: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FeatureSupport {
    Unsupported {
        reason: String,
    },
    Unverified {
        evidence: Vec<EvidenceRef>,
        research_id: String,
    },
    Verified {
        profile: ValidationProfileId,
        mode: BackendMode,
        firmware: Option<String>,
        evidence: Vec<EvidenceRef>,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub model: DeviceModel,
    pub operations: BTreeMap<OperationKind, FeatureSupport>,
}
impl Capabilities {
    pub fn empty(model: DeviceModel) -> Self {
        Self {
            model,
            operations: BTreeMap::new(),
        }
    }
    pub fn allows(&self, operation: OperationKind, identity: &DeviceIdentity) -> bool {
        if self.model != identity.model {
            return false;
        }
        matches!(self.operations.get(&operation), Some(FeatureSupport::Verified { mode, firmware, .. })
            if *mode == identity.mode && firmware.as_ref().is_none_or(|v| identity.firmware.as_ref().is_some_and(|f| &f.version == v)))
    }
    pub fn gp200_read_only() -> Self {
        let mut capabilities = Self::empty(DeviceModel::Gp200);
        for op in [
            OperationKind::Identify,
            OperationKind::ReadEditBuffer,
            OperationKind::ReadStoredPreset,
        ] {
            capabilities.operations.insert(
                op,
                FeatureSupport::Verified {
                    profile: ValidationProfileId(format!("gp200-capture-{op:?}")),
                    mode: BackendMode::Hardware,
                    firmware: None,
                    evidence: vec![EvidenceRef::new(
                        "analysis/PROTOCOL.txt",
                        EvidenceLevel::LiveVerified,
                    )],
                },
            );
        }
        capabilities
    }
    pub fn simulator() -> Self {
        let mut capabilities = Self::empty(DeviceModel::Gp200);
        for op in OperationKind::ALL {
            if matches!(
                op,
                OperationKind::FirmwareUpdate
                    | OperationKind::FactoryReset
                    | OperationKind::IrWrite
                    | OperationKind::SnapToneWrite
            ) {
                continue;
            }
            capabilities.operations.insert(
                op,
                FeatureSupport::Verified {
                    profile: ValidationProfileId(format!("simulator-{op:?}")),
                    mode: BackendMode::Simulator,
                    firmware: None,
                    evidence: vec![EvidenceRef::new(
                        "crates/gp200-sim",
                        EvidenceLevel::Proposed,
                    )],
                },
            );
        }
        capabilities
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationAvailability {
    Available,
    OfflineOnly,
    Blocked { reason: String },
}
