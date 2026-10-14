//! Local application preferences, never pedal settings or protocol authority.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

/// Compatibility field for existing preference files. The interface uses English.
/// Former language tags migrate to English without discarding other preferences.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LanguagePreference {
    #[default]
    #[serde(
        rename = "en",
        alias = "en-US",
        alias = "en-GB",
        alias = "English",
        alias = "zh-CN",
        alias = "zh",
        alias = "zh_cn",
        alias = "zh-Hans",
        alias = "Chinese"
    )]
    English,
}

/// Related dialogs share one folder; imports and exports remain independent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DialogDirectoryPurpose {
    PresetImport,
    PresetExport,
    IrImport,
    NamImport,
    SnapToneImport,
    AssetExport,
    NamInput,
    CalibrationStimulus,
    LibraryBackup,
    DeviceBackupImport,
    DeviceBackupExport,
    Firmware,
}
impl DialogDirectoryPurpose {
    pub const ALL: [Self; 12] = [
        Self::PresetImport,
        Self::PresetExport,
        Self::IrImport,
        Self::NamImport,
        Self::SnapToneImport,
        Self::AssetExport,
        Self::NamInput,
        Self::CalibrationStimulus,
        Self::LibraryBackup,
        Self::DeviceBackupImport,
        Self::DeviceBackupExport,
        Self::Firmware,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::PresetImport => "Preset and bundle imports",
            Self::PresetExport => "Preset and bundle exports",
            Self::IrImport => "Impulse response sources",
            Self::NamImport => "NAM models",
            Self::SnapToneImport => "SnapTone sources",
            Self::AssetExport => "Rendered WAV and CLO exports",
            Self::NamInput => "NAM rendering input WAV",
            Self::CalibrationStimulus => "Original calibration stimulus",
            Self::LibraryBackup => "Local library backups",
            Self::DeviceBackupImport => "Device bank archive imports",
            Self::DeviceBackupExport => "Device bank archive exports",
            Self::Firmware => "Firmware inspection",
        }
    }
}

pub type DialogDirectories = BTreeMap<DialogDirectoryPurpose, PathBuf>;
