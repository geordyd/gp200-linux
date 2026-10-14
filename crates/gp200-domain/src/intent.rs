use crate::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemePreference {
    Dark,
    Light,
    #[default]
    System,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppPreferences {
    pub theme: ThemePreference,
    pub scale: f32,
    pub language: LanguagePreference,
    pub compact: bool,
    pub last_input: Option<String>,
    pub last_output: Option<String>,
    pub dialog_directories: DialogDirectories,
}
impl Default for AppPreferences {
    fn default() -> Self {
        Self {
            theme: ThemePreference::System,
            scale: 1.0,
            language: LanguagePreference::English,
            compact: false,
            last_input: None,
            last_output: None,
            dialog_directories: DialogDirectories::new(),
        }
    }
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViewId {
    #[default]
    Editor,
    Library,
    Assets,
    Assignments,
    GlobalSettings,
    Performance,
    Practice,
    Diagnostics,
    AppSettings,
    Help,
}
#[derive(Debug, Clone)]
pub enum ConflictResolution {
    AcceptDevice(IntentId),
    KeepLocal(IntentId),
}
#[derive(Debug, Clone)]
pub enum UiIntent {
    Navigate(ViewId),
    DiscoverDevices,
    Connect(DeviceSelection),
    Disconnect,
    EnterOfflineMode,
    Refresh(PresetTarget),
    PreviewStored(SlotId),
    AuditionStored(SlotId),
    OpenLibraryVersion(LibraryVersionId),
    NewOfflineFrom(Arc<PresetSnapshot>),
    BeginGesture {
        document: DocumentId,
        key: GestureKey,
    },
    ApplyEdit {
        document: DocumentId,
        patch: EditPatch,
    },
    ApplyOfflineMerge(OfflineMergeRequest),
    EndGesture {
        document: DocumentId,
        key: GestureKey,
    },
    Undo(DocumentId),
    Redo(DocumentId),
    DiscardDraft(DocumentId),
    SaveBufferTo {
        slot: SlotId,
        expected_baseline: Revision,
    },
    WriteStoredPreset {
        slot: SlotId,
        expected_target_revision: Revision,
        snapshot: Arc<PresetSnapshot>,
    },
    ClearStoredPreset {
        slot: SlotId,
        expected_target_revision: Revision,
    },
    ApplyGlobalEdit(GlobalPatch),
    RefreshDeviceGlobals,
    RefreshAssetMetadata(AssetSlot),
    ApplyGlobalControlEdit(GlobalControlEdit),
    Library(LibraryCommand),
    StartAssetJob(AssetJobSpec),
    StartFirmwareJob(FirmwareJobSpec),
    CancelJob(JobId),
    ResolveConflict(ConflictResolution),
    SetPreferences(AppPreferences),
}
#[derive(Debug, Clone)]
pub enum AppInput {
    Ui(UiIntent),
    Device(DeviceEvent),
    Library(LibraryEvent),
    Assets(AssetEvent),
    Tick(u64),
}
#[derive(Debug, Clone)]
pub enum Effect {
    Device(DeviceCommand),
    Library(LibraryCommand),
    Assets(AssetCommand),
    PersistSettings(AppPreferences),
}
