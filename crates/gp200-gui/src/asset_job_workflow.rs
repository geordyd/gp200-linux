//! Read-only local preparation worker. It owns files, never a device service.
use gp200_domain::*;
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SourceSelection {
    pub path: PathBuf,
    pub expected_sha256: Option<JobSha256>,
    pub bank: AssetBank,
    pub slot: u16,
    pub name: String,
    pub truncate: bool,
    /// Descriptive retained conversion history, never device authority.
    pub provenance: BTreeMap<String, String>,
}

#[cfg(test)]
#[path = "asset_job_workflow_tests.rs"]
mod tests;
impl SourceSelection {
    pub fn target(&self) -> Result<AssetSlot, String> {
        AssetSlot::new(self.bank, self.slot).map_err(|e| e.to_string())
    }
}
#[derive(Debug, Clone)]
pub(crate) struct LocalAssetReview {
    pub selection: SourceSelection,
    pub source: RetainedJobBlob,
    pub recipe: NormalAssetRecipe,
    pub conversion: JobConversion,
    pub technical: String,
}
#[derive(Debug, Clone)]
pub(crate) struct LiveBinding {
    pub session: DeviceJobSession,
    pub baseline: Arc<DeviceAssetMetadata>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct AffectedPreset {
    pub location: String,
    pub name: String,
    pub sha256: JobSha256,
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ReviewReport {
    pub schema: u32,
    pub operation: &'static str,
    pub source: SourceSelection,
    pub source_sha256: JobSha256,
    pub source_bytes: usize,
    pub derived_sha256: JobSha256,
    pub derived_bytes: usize,
    pub codec_revision: String,
    pub options_sha256: JobSha256,
    pub technical: String,
    pub session: Option<DeviceJobSession>,
    pub prepared: Option<DeviceJobReview>,
    pub prerequisites: Vec<String>,
    pub affected_presets: Vec<AffectedPreset>,
    pub fresh_stored_presets_checked: usize,
    pub full_content_backup_verified: bool,
    pub occupancy_known: bool,
    pub hardware_commands: u64,
    pub permits_transfer: bool,
}
impl ReviewReport {
    pub fn sha256(&self) -> JobSha256 {
        let value = serde_json::to_value(self).expect("Bounded asset review JSON");
        JobSha256::of(&serde_json::to_vec(&value).expect("Canonical review JSON"))
    }
    pub fn sealed_json(&self) -> serde_json::Value {
        serde_json::json!({"report_sha256":self.sha256(), "review":self})
    }
}
pub(crate) enum Work {
    Inspect(SourceSelection),
    Prepare {
        local: Box<LocalAssetReview>,
        binding: Box<LiveBinding>,
        id: JobId,
    },
    Export {
        report: Box<ReviewReport>,
        path: PathBuf,
    },
}
pub(crate) enum Completed {
    Inspected(Box<LocalAssetReview>),
    Prepared(PreparedDeviceJob),
    Exported(PathBuf),
}
pub(crate) struct Reply {
    pub id: u64,
    pub result: Result<Completed, String>,
}
struct Request {
    id: u64,
    cancel: Arc<AtomicBool>,
    work: Work,
}
pub(crate) struct AssetJobWorker {
    send: SyncSender<Request>,
    replies: Receiver<Reply>,
}
impl Default for AssetJobWorker {
    fn default() -> Self {
        let (send, receive) = mpsc::sync_channel::<Request>(2);
        let (reply, replies) = mpsc::sync_channel(4);
        std::thread::spawn(move || {
            while let Ok(request) = receive.recv() {
                let result = match request.work {
                    Work::Inspect(selection) => inspect(selection, &request.cancel)
                        .map(Box::new)
                        .map(Completed::Inspected),
                    Work::Prepare { local, binding, id } => {
                        prepare(&local, *binding, id, &request.cancel).map(Completed::Prepared)
                    }
                    Work::Export { report, path } => {
                        export(&report, &path, &request.cancel).map(|()| Completed::Exported(path))
                    }
                };
                if reply
                    .send(Reply {
                        id: request.id,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self { send, replies }
    }
}
impl AssetJobWorker {
    pub fn submit(&self, id: u64, cancel: Arc<AtomicBool>, work: Work) -> Result<(), String> {
        self.send
            .try_send(Request { id, cancel, work })
            .map_err(|_| {
                crate::localization::text("Asset review worker is busy; wait and try again").into()
            })
    }
    pub fn poll(&self) -> impl Iterator<Item = Reply> + '_ {
        self.replies.try_iter()
    }
}
fn cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err(crate::localization::text("Local asset review cancelled; no transfer started").into())
    } else {
        Ok(())
    }
}
fn read_source(path: &Path, cancel: &AtomicBool) -> Result<Vec<u8>, String> {
    cancelled(cancel)?;
    let before = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !before.is_file()
        || before.len() == 0
        || before.len() > gp200_assets::MAX_SOURCE_BYTES as u64
    {
        return Err(crate::localization::text(
            "Select a regular source file of 1..64 MiB; symbolic links are not accepted",
        )
        .into());
    }
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let opened = file.metadata().map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != opened.dev() || before.ino() != opened.ino() {
            return Err(crate::localization::text(
                "Source was replaced before reading; inspect it again",
            )
            .into());
        }
    }
    if !opened.is_file() || before.len() != opened.len() {
        return Err(
            crate::localization::text("Source changed before reading; inspect it again").into(),
        );
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    let mut block = [0; 65536];
    loop {
        cancelled(cancel)?;
        let count = file.read(&mut block).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        if bytes.len() + count > gp200_assets::MAX_SOURCE_BYTES {
            return Err(crate::localization::text("Source grew beyond 64 MiB").into());
        }
        bytes.extend_from_slice(&block[..count]);
    }
    let after = file.metadata().map_err(|e| e.to_string())?;
    let current = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !current.is_file()
        || after.len() != bytes.len() as u64
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
    {
        return Err(
            crate::localization::text("Source changed while reading; inspect it again").into(),
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != current.dev()
            || before.ino() != current.ino()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
        {
            return Err(crate::localization::text(
                "Source changed while reading; inspect it again",
            )
            .into());
        }
    }
    cancelled(cancel)?;
    Ok(bytes)
}
fn blob(bytes: Vec<u8>) -> Result<RetainedJobBlob, String> {
    let hash = JobSha256::of(&bytes);
    RetainedJobBlob::checked(
        AssetId(format!("sha256:{}", hash.hex())),
        bytes.into(),
        hash,
    )
    .map_err(|e| e.message)
}
fn inspect(selection: SourceSelection, cancel: &AtomicBool) -> Result<LocalAssetReview, String> {
    let target = selection.target()?;
    if selection.provenance.len() > 64
        || selection
            .provenance
            .iter()
            .any(|(k, v)| k.len() > 128 || v.len() > 32768)
    {
        return Err(crate::localization::text(
            "Retained source provenance exceeds the local review bound",
        )
        .into());
    }
    let bytes = read_source(&selection.path, cancel)?;
    let source = blob(bytes)?;
    if selection
        .expected_sha256
        .is_some_and(|expected| expected != source.sha256())
    {
        return Err(crate::localization::text(
            "Retained source hash differs from its inventory identity; inspect a fresh source",
        )
        .into());
    }
    let (recipe, technical) = match target.bank() {
        AssetBank::Ir => {
            let length = if selection.truncate {
                gp200_assets::IrLengthPolicy::KeepFirst1024
            } else {
                gp200_assets::IrLengthPolicy::RejectTruncation
            };
            let packed =
                gp200_assets::pack_device_ir(source.bytes(), length).map_err(|e| e.message)?;
            let info = packed.info();
            let technical = std::format!(
                "Mono PCM24 · 44100 Hz · {} samples · {} retained · {} padded · {} truncated",
                info.source_samples,
                info.retained_samples,
                info.padded_samples,
                info.truncated_samples
            );
            let recipe = NormalAssetRecipe::ir(
                target,
                selection.name.clone(),
                source.clone(),
                if selection.truncate {
                    IrJobLengthPolicy::KeepFirst1024
                } else {
                    IrJobLengthPolicy::RejectLong
                },
                blob(packed.raw().to_vec())?,
            )
            .map_err(|e| e.message)?;
            (recipe, technical)
        }
        AssetBank::SnapTone => {
            let recipe = NormalAssetRecipe::native_clo(
                target,
                selection.name.clone(),
                source.clone(),
                blob(
                    source
                        .bytes()
                        .get(..8192)
                        .ok_or(crate::localization::text(
                            "CLO source is shorter than its native prefix",
                        ))?
                        .to_vec(),
                )?,
            )
            .map_err(|e| e.message)?;
            let technical = std::format!(
                "VTSI container · {} bytes · {:?} · exact 8192-byte prefix",
                source.bytes().len(),
                recipe.clo_profile().expect("CLO recipe")
            );
            (recipe, technical)
        }
    };
    // The direct codec starts from this exact WAV/container. Historical NAM
    // properties are explicitly descriptive; they never grant missing inputs.
    let options: Arc<[u8]> = serde_json::to_vec(&serde_json::json!({
        "source_input":"exact_selected_wav_or_clo", "name":selection.name,
        "ir_length_policy":recipe.ir_length_policy(), "clo_profile":recipe.clo_profile(),
    }))
    .map_err(|e| e.to_string())?
    .into();
    let conversion = JobConversion::checked(
        recipe.codec_revision().into(),
        options.clone(),
        JobSha256::of(&options),
        BTreeMap::new(),
    )
    .map_err(|e| e.message)?;
    cancelled(cancel)?;
    Ok(LocalAssetReview {
        selection,
        source,
        recipe,
        conversion,
        technical,
    })
}
fn prepare(
    local: &LocalAssetReview,
    binding: LiveBinding,
    id: JobId,
    cancel: &AtomicBool,
) -> Result<PreparedDeviceJob, String> {
    let fresh = read_source(&local.selection.path, cancel)?;
    if JobSha256::of(&fresh) != local.source.sha256() || fresh.as_slice() != local.source.bytes() {
        return Err(crate::localization::text(
            "Source was replaced after inspection; inspect and review it again",
        )
        .into());
    }
    cancelled(cancel)?;
    PreparedDeviceJob::prepare(id, binding.session, local.source.clone(), local.recipe.clone(), local.conversion.clone(), binding.baseline,
        AssetBackupReview::Unavailable { reason:crate::localization::text("Only slot metadata is readable; current audio contents and exact restoration have not been proved").into() })
        .map_err(|e| e.message)
}
fn export(report: &ReviewReport, path: &Path, cancel: &AtomicBool) -> Result<(), String> {
    cancelled(cancel)?;
    if !path.is_absolute() || path == report.source.path {
        return Err(crate::localization::text(
            "Choose a fresh absolute report path distinct from the source",
        )
        .into());
    }
    let parent = path
        .parent()
        .ok_or(crate::localization::text("Report has no parent folder"))?;
    let source_parent = report.source.path.parent().unwrap_or(Path::new(""));
    if source_parent
        .file_name()
        .is_some_and(|name| name == "originals" || name == "derived")
        && fs::canonicalize(parent).ok() == fs::canonicalize(source_parent).ok()
    {
        return Err(crate::localization::text(
            "Reports cannot be written into immutable asset storage",
        )
        .into());
    }
    let mut bytes = serde_json::to_vec_pretty(&report.sealed_json()).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    if bytes.len() > 128 * 1024 {
        return Err(crate::localization::text(
            "Review report exceeds 128 KiB; reduce descriptive provenance",
        )
        .into());
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    static STAGE: AtomicU64 = AtomicU64::new(0);
    let stage = parent.join(format!(
        ".gp200-asset-review-{}-{}.tmp",
        std::process::id(),
        STAGE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = options
        .open(&stage)
        .map_err(|e| std::format!("Report was not created: {e}"))?;
    let staged = (|| {
        file.write_all(&bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        cancelled(cancel)?;
        // Link publishes complete bytes while refusing an existing destination.
        fs::hard_link(&stage, path).map_err(|e| std::format!("Report was not published: {e}"))
    })();
    let _ = fs::remove_file(&stage);
    staged?;
    let result = File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|e| e.to_string());
    match result {
        Ok(()) => Ok(()),
        Err(error) => Err(std::format!(
            "Report publication is indeterminate; inspect the visible file before retrying: {error}"
        )),
    }
}
