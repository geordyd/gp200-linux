use crate::*;
use gp200_protocol::RawTransfer;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorKind {
    InvalidData,
    Unsupported,
    Unverified,
    Busy,
    Disconnected,
    Timeout,
    Io,
    Protocol,
    Conflict,
    Cancelled,
    Indeterminate,
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[error("{message}")]
pub struct ServiceError {
    pub kind: ErrorKind,
    pub message: String,
    pub evidence: Vec<EvidenceRef>,
}
impl ServiceError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            evidence: Vec::new(),
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidData, message)
    }
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, message)
    }
}
#[derive(Debug)]
pub enum SubmitError<T> {
    Busy(T),
    Disconnected(T),
}
impl<T> SubmitError<T> {
    pub fn into_command(self) -> T {
        match self {
            Self::Busy(command) | Self::Disconnected(command) => command,
        }
    }
}
pub type WakeUi = Arc<dyn Fn() + Send + Sync>;
pub fn no_wake() -> WakeUi {
    Arc::new(|| {})
}
#[derive(Debug, Clone, Default)]
pub struct ShutdownToken(Arc<AtomicBool>);
impl ShutdownToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceFingerprint {
    pub vendor: Option<u16>,
    pub product: Option<u16>,
    pub serial: Option<String>,
    pub hint: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSelection {
    pub input_id: String,
    pub output_id: String,
    pub fingerprint_hint: Option<DeviceFingerprint>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortPairCandidate {
    pub selection: DeviceSelection,
    pub display_name: String,
    pub mode_hint: Option<BackendMode>,
    pub matching_confidence: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionState {
    Disconnected,
    Offline,
    Discovering,
    Connecting,
    Identifying,
    Synchronizing,
    Ready,
    Recovering,
    Unsupported,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalPatch {
    pub section: String,
    pub key: String,
    pub value: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalSnapshot {
    pub revision: Revision,
    pub fields: BTreeMap<String, Recovered<String>>,
    pub raw: Option<Arc<RawTransfer>>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobProgress {
    pub completed: u64,
    pub total: Option<u64>,
    pub message: String,
    pub cancellable: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticEvent {
    pub category: String,
    pub message: String,
    pub bytes: Option<Vec<u8>>,
    pub epoch: Option<SessionEpoch>,
}
#[derive(Debug, Clone)]
pub enum EditStage {
    Queued,
    Sent,
    AwaitingReadback,
    Verified,
    Failed(ServiceError),
    Indeterminate(ServiceError),
}

#[derive(Debug, Clone)]
pub enum DeviceCommand {
    Discover,
    Connect {
        intent: IntentId,
        selection: DeviceSelection,
    },
    Disconnect {
        intent: IntentId,
    },
    ReadIdentity(RequestStamp),
    ReadCurrent(RequestStamp),
    ReadGlobals(RequestStamp),
    ReadAssetMetadata {
        stamp: RequestStamp,
        slot: AssetSlot,
    },
    ReadStored {
        stamp: RequestStamp,
        slot: SlotId,
    },
    RefreshTarget {
        stamp: RequestStamp,
        target: PresetTarget,
    },
    SelectStored {
        stamp: RequestStamp,
        slot: SlotId,
    },
    SubmitEdit {
        stamp: RequestStamp,
        target: PresetTarget,
        base_revision: Revision,
        patch: EditPatch,
    },
    /// Explicit global binding for the fixed dependent-model profiles. This
    /// descriptor is not transferable write authority; the actor preflights it.
    SubmitDependentModel {
        stamp: RequestStamp,
        base_revision: Revision,
        module: ModuleId,
        model: ModelCode,
        before_globals: Arc<DeviceGlobalState>,
        expected_auto_cab: bool,
    },
    /// An exact descriptive factory candidate, independently preflighted by the actor.
    SubmitFactoryAmpModel {
        stamp: RequestStamp,
        base_revision: Revision,
        model: ModelCode,
        profile: crate::FactoryAmpProfileId,
        before_globals: Arc<DeviceGlobalState>,
        expected_auto_cab: bool,
    },
    SubmitGlobalEdit {
        stamp: RequestStamp,
        base_revision: Revision,
        patch: GlobalPatch,
    },
    SubmitGlobalControlEdit {
        stamp: RequestStamp,
        base_revision: Revision,
        patch: GlobalControlEdit,
    },
    SaveBufferTo {
        stamp: RequestStamp,
        slot: SlotId,
        expected_buffer_revision: Revision,
    },
    WriteStoredPreset {
        stamp: RequestStamp,
        slot: SlotId,
        expected_target_revision: Revision,
        snapshot: Arc<PresetSnapshot>,
    },
    ClearStoredPreset {
        stamp: RequestStamp,
        slot: SlotId,
        expected_target_revision: Revision,
    },
    ExecuteDeviceJob {
        stamp: RequestStamp,
        job: ValidatedDeviceJob,
    },
    CancelJob {
        stamp: RequestStamp,
        job_id: JobId,
    },
    Shutdown,
}
#[derive(Debug, Clone)]
pub enum DeviceEvent {
    PortsChanged(Vec<PortPairCandidate>),
    ConnectionChanged {
        epoch: SessionEpoch,
        state: ConnectionState,
    },
    SessionChanged {
        epoch: SessionEpoch,
        identity: DeviceIdentity,
        capabilities: Capabilities,
    },
    /// Immutable actor observation, used for future reviewed asset preparation.
    /// This observation alone grants no write capability.
    DeviceJobSessionObserved(DeviceJobSession),
    SnapshotReceived {
        stamp: RequestStamp,
        target: PresetTarget,
        snapshot: Arc<PresetSnapshot>,
    },
    GlobalsReceived {
        stamp: RequestStamp,
        globals: GlobalSnapshot,
    },
    GlobalStateReceived {
        stamp: RequestStamp,
        state: Arc<DeviceGlobalState>,
    },
    AssetMetadataReceived {
        stamp: RequestStamp,
        metadata: Arc<DeviceAssetMetadata>,
    },
    EditProgress {
        stamp: RequestStamp,
        stage: EditStage,
    },
    IncomingNotification {
        epoch: SessionEpoch,
        raw: Arc<RawTransfer>,
    },
    JobProgress {
        epoch: SessionEpoch,
        job_id: JobId,
        progress: JobProgress,
    },
    OperationFailed {
        stamp: RequestStamp,
        error: ServiceError,
    },
    Diagnostic(DiagnosticEvent),
}
pub trait DeviceService: Send {
    /// Execution uses the once-extracted concrete transport client; this method
    /// is descriptive and grants neither durability nor device authority.
    fn firmware_backup_capture_support(&self) -> FirmwareCaptureSupport {
        FirmwareCaptureSupport::Unavailable
    }

    fn try_submit(&self, command: DeviceCommand) -> Result<(), SubmitError<DeviceCommand>>;
    fn try_next_event(&self) -> Option<DeviceEvent>;
    fn shutdown(&self);
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryMetadata {
    pub name: String,
    pub author: String,
    pub style: String,
    pub notes: String,
    pub tags: Vec<String>,
    pub favorite: bool,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LibraryQuery {
    pub text: String,
    pub tags: Vec<String>,
    pub favorites_only: bool,
    #[serde(default)]
    pub trashed_only: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryEntry {
    pub id: LibraryId,
    pub latest_version: LibraryVersionId,
    pub metadata: LibraryMetadata,
    pub version_count: u32,
    pub modified_at: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryVersion {
    pub id: LibraryVersionId,
    pub library: LibraryId,
    pub parent: Option<LibraryVersionId>,
    pub snapshot: Arc<PresetSnapshot>,
    pub metadata: LibraryMetadata,
    pub created_at: String,
}
#[derive(Debug, Clone)]
pub struct ImportRequest {
    pub path: PathBuf,
}
#[derive(Debug, Clone)]
pub struct ExportRequest {
    pub path: PathBuf,
    pub versions: Vec<LibraryVersionId>,
    pub include_assets: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeExportMode {
    PreserveOriginal,
    PresetOnly,
}
#[derive(Debug, Clone)]
pub struct NativeExportRequest {
    pub path: PathBuf,
    pub version: LibraryVersionId,
    pub mode: NativeExportMode,
}
#[derive(Debug, Clone)]
pub struct BackupRequest {
    pub path: PathBuf,
    pub versions: Vec<LibraryVersionId>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompatibilityLevel {
    Lossless,
    Partial,
    Unsupported,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversionReport {
    pub level: CompatibilityLevel,
    pub notes: Vec<String>,
    pub unresolved_assets: Vec<AssetRef>,
}
#[derive(Debug, Clone)]
pub enum LibraryCommand {
    ImportNative {
        intent: IntentId,
        request: ImportRequest,
    },
    ExportNative {
        intent: IntentId,
        request: NativeExportRequest,
    },
    DeleteToTrash {
        intent: IntentId,
        library: LibraryId,
    },
    RestoreTrash {
        intent: IntentId,
        library: LibraryId,
    },
    UpdateMetadata {
        intent: IntentId,
        library: LibraryId,
        metadata: LibraryMetadata,
    },
    Query {
        intent: IntentId,
        query: LibraryQuery,
    },
    LoadVersion {
        intent: IntentId,
        version: LibraryVersionId,
    },
    StoreSnapshot {
        intent: IntentId,
        snapshot: Arc<PresetSnapshot>,
        metadata: LibraryMetadata,
        parent: Option<LibraryVersionId>,
    },
    ListVersions {
        intent: IntentId,
        library: LibraryId,
    },
    ImportBundle {
        intent: IntentId,
        request: ImportRequest,
    },
    ExportBundle {
        intent: IntentId,
        request: ExportRequest,
    },
    Backup {
        intent: IntentId,
        request: BackupRequest,
    },
    Cancel(JobId),
    Shutdown,
}
#[derive(Debug, Clone)]
pub enum LibraryEvent {
    EntryChanged {
        intent: IntentId,
        library: LibraryId,
    },
    QueryResult {
        intent: IntentId,
        entries: Vec<LibraryEntry>,
    },
    VersionLoaded {
        intent: IntentId,
        version: LibraryVersion,
    },
    VersionStored {
        intent: IntentId,
        version: LibraryVersionId,
    },
    VersionsListed {
        intent: IntentId,
        versions: Vec<LibraryVersion>,
    },
    Imported {
        intent: IntentId,
        versions: Vec<LibraryVersionId>,
        report: ConversionReport,
    },
    Exported {
        intent: IntentId,
        path: PathBuf,
        report: ConversionReport,
    },
    JobProgress {
        job_id: JobId,
        progress: JobProgress,
    },
    Failed {
        intent: IntentId,
        error: ServiceError,
    },
}
pub trait LibraryService: Send {
    fn try_submit(&self, command: LibraryCommand) -> Result<(), SubmitError<LibraryCommand>>;
    fn try_next_event(&self) -> Option<LibraryEvent>;
    fn shutdown(&self);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetJobSpec {
    pub id: JobId,
    pub kind: AssetKind,
    pub source: PathBuf,
    pub output: Option<PathBuf>,
    pub options: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirmwareJobSpec {
    pub id: JobId,
    pub source: PathBuf,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetInfo {
    pub id: AssetId,
    pub kind: AssetKind,
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub properties: BTreeMap<String, String>,
    pub warnings: Vec<String>,
}
#[derive(Debug, Clone)]
pub enum AssetCommand {
    Inspect(AssetJobSpec),
    Prepare(AssetJobSpec),
    Convert(AssetJobSpec),
    List,
    Cancel(JobId),
    Shutdown,
}
#[derive(Debug, Clone)]
pub enum AssetEvent {
    Inventory(Vec<AssetInfo>),
    Inspected {
        job: JobId,
        info: AssetInfo,
    },
    Prepared {
        job: JobId,
        info: AssetInfo,
    },
    DeviceJobPrepared(PreparedDeviceJob),
    JobProgress {
        job_id: JobId,
        progress: JobProgress,
    },
    Failed {
        job: JobId,
        error: ServiceError,
    },
}
pub trait AssetService: Send {
    fn try_submit(&self, command: AssetCommand) -> Result<(), SubmitError<AssetCommand>>;
    fn try_next_event(&self) -> Option<AssetEvent>;
    fn shutdown(&self);
}
