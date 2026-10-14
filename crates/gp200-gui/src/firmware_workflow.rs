//! Immutable, bounded firmware review and rehearsal worker. This module has no
//! device service, transport handle, network client, or flash command.
use gp200_domain::*;
use gp200_protocol::BootloaderIdentity;
use serde::Serialize;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    time::{Duration, Instant},
};

const MAX_SOURCE: u64 = 64 * 1024 * 1024;
const MAX_REHEARSAL_BLOCKS: u64 = 1_000_000;
const MAX_REPORT: usize = 128 * 1024;
static STAGE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub(crate) struct InspectedFirmware {
    pub path: PathBuf,
    pub review: FirmwareReview,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum FirmwareSimulation {
    Preview,
    AllAcknowledged,
    MissingBlockAcknowledgement,
    MissingSectionAcknowledgement,
}
impl FirmwareSimulation {
    pub fn label(self) -> &'static str {
        match self {
            Self::Preview => crate::localization::text("Preview first block"),
            Self::AllAcknowledged => crate::localization::text("Simulate every acknowledgement"),
            Self::MissingBlockAcknowledgement => {
                crate::localization::text("Simulate a missing block acknowledgement")
            }
            Self::MissingSectionAcknowledgement => {
                crate::localization::text("Simulate a missing section acknowledgement")
            }
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct RehearsalReport {
    pub source_sha256_hex: String,
    pub identity_fixture: String,
    pub identity_sha256_hex: String,
    pub negotiated_chunk_bytes: u16,
    pub simulation: FirmwareSimulation,
    pub total_blocks: u64,
    pub previewed_blocks: u64,
    pub preview_transcript_sha256_hex: String,
    pub preview_transcript_hash_recipe: &'static str,
    pub simulated_block_acknowledgements: u64,
    pub simulated_section_acknowledgements: u8,
    pub phase: FirmwareRehearsalPhase,
    pub progress: JobProgress,
    pub representative_chunks: Vec<FirmwareChunkPreview>,
    pub representative_wire_sha256_hex: Vec<String>,
    pub hardware_commands: u64,
    pub permits_flash: bool,
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ReportSnapshot {
    schema: u32,
    operation: &'static str,
    source_path: PathBuf,
    source_sha256_hex: String,
    captured_session: Option<DeviceJobSession>,
    review: FirmwareReviewSummary,
    rehearsal: Option<RehearsalReport>,
    hardware_commands: u64,
    permits_flash: bool,
}
impl ReportSnapshot {
    pub fn new(image: &InspectedFirmware, rehearsal: Option<RehearsalReport>) -> Self {
        Self {
            schema: 1,
            operation: "offline_firmware_review",
            source_path: image.path.clone(),
            source_sha256_hex: image.review.summary().source_sha256.hex(),
            captured_session: image.review.session().cloned(),
            review: image.review.summary().clone(),
            rehearsal,
            hardware_commands: 0,
            permits_flash: false,
        }
    }
}

pub(crate) enum Work {
    Inspect {
        path: PathBuf,
        session: Option<Box<DeviceJobSession>>,
    },
    Rehearse {
        image: Box<InspectedFirmware>,
        model: DeviceModel,
        chunk: u16,
        simulation: FirmwareSimulation,
    },
    Export {
        report: Box<ReportSnapshot>,
        path: PathBuf,
    },
}
pub(crate) enum Completed {
    Inspected(Box<InspectedFirmware>),
    Rehearsed(Box<RehearsalReport>),
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

/// Descriptive immutable material returned by the off-UI preparation worker.
/// No required physical profile, approval or recovery material is fabricated.
#[derive(Debug, Clone, Serialize)]
pub struct LocalFirmwarePreparation {
    pub digest: String,
    pub image_sha256: String,
    pub archive_sha256: String,
    pub sidecar_sha256: String,
    pub evidence_index_sha256: String,
    pub acquisition: String,
    pub source_epoch: SessionEpoch,
    pub local_draft_excluded: bool,
    pub backup: FirmwareBackupReviewSummary,
    pub blockers: Vec<String>,
    pub permits_flash: bool,
    pub completion_verified: bool,
}
pub(crate) struct PreparationInput {
    pub generation: u64,
    pub image: InspectedFirmware,
    pub source: gp200_app::FirmwareEditorSource,
    pub local_draft_excluded: bool,
    pub proposal: ProposedFirmwareLossAllowance,
    pub evidence: FirmwareCaptureEvidenceSummary,
    pub archive: PathBuf,
    pub sidecar: PathBuf,
    pub index: PathBuf,
}
pub(crate) struct PreparedLocalReview {
    pub view: LocalFirmwarePreparation,
    pub image: InspectedFirmware,
    pub source: gp200_app::FirmwareEditorSource,
}
pub(crate) struct PreparationReply {
    pub generation: u64,
    pub result: Result<Arc<PreparedLocalReview>, String>,
}
pub(crate) struct CoordinatorPreparationWorker {
    commands: SyncSender<(PreparationInput, Arc<AtomicBool>)>,
    events: Receiver<PreparationReply>,
}
impl Default for CoordinatorPreparationWorker {
    fn default() -> Self {
        let (commands, receive) = mpsc::sync_channel::<(PreparationInput, Arc<AtomicBool>)>(1);
        let (events_send, events) = mpsc::sync_channel(2);
        std::thread::spawn(move || {
            while let Ok((input, cancel)) = receive.recv() {
                let generation = input.generation;
                let result = prepare_local_review(input, &cancel).map(Arc::new);
                if events_send
                    .send(PreparationReply { generation, result })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self { commands, events }
    }
}
impl CoordinatorPreparationWorker {
    pub(crate) fn submit(
        &self,
        input: PreparationInput,
        cancel: Arc<AtomicBool>,
    ) -> Result<(), String> {
        self.commands
            .try_send((input, cancel))
            .map_err(|_| "Local material preparation is busy; no request was accepted".into())
    }
    pub(crate) fn poll(&self) -> Option<PreparationReply> {
        self.events.try_recv().ok()
    }
}
fn prepare_local_review(
    input: PreparationInput,
    cancel: &AtomicBool,
) -> Result<PreparedLocalReview, String> {
    check_cancel(cancel)?;
    input.source.recovery.validate().map_err(|e| e.message)?;
    let reopened = gp200_library::reopen_firmware_backup_review(&input.archive, &input.sidecar)
        .map_err(|e| e.message)?;
    if reopened != input.proposal.review
        || reopened.context.destination.image_sha256
            != input.image.review.summary().source_sha256.hex()
    {
        return Err(
            "The exact retained archive, sidecar, destination or proposed loss changed".into(),
        );
    }
    fn material_hash(path: &Path) -> Result<String, String> {
        let mut bytes = Vec::new();
        File::open(path)
            .map_err(|e| e.to_string())?
            .take(FIRMWARE_BACKUP_MATERIAL_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > FIRMWARE_BACKUP_MATERIAL_LIMIT {
            return Err("Retained archive material exceeds its bound".into());
        }
        Ok(JobSha256::of(&bytes).hex())
    }
    if material_hash(&input.archive)? != input.proposal.archive_sha256
        || material_hash(&input.sidecar)? != input.proposal.sidecar_sha256
    {
        return Err("The exact original archive or sidecar hash changed".into());
    }
    let evidence =
        gp200_library::reopen_firmware_capture_evidence(&input.index).map_err(|e| e.message)?;
    if evidence != input.evidence
        || evidence.acquisition.as_deref() != Some(input.proposal.acquisition.as_str())
        || evidence.archive_sha256 != input.proposal.archive_sha256
        || evidence.sidecar_sha256 != input.proposal.sidecar_sha256
    {
        return Err("The exact retained capture evidence changed".into());
    }
    check_cancel(cancel)?;
    // Bounded serialization on this helper, never on a frame or MIDI worker.
    struct Bounded(Vec<u8>);
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.0.len().saturating_add(bytes.len()) > 32 * 1024 * 1024 {
                return Err(std::io::Error::other(
                    "Exact retained local review exceeds 32 MiB",
                ));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let blockers = vec![
        "The Linux update and recovery protocol still needs hardware validation".into(),
        "All 20 installed IR audio payloads are unavailable, including slots with known local sources".into(),
        "Preset, settings and user asset restoration to the destination firmware still needs validation".into(),
        "Automatic installed firmware version is unavailable; human and image versions retain separate origins".into(),
        "No update or proposed loss has been approved; exported material cannot resume an update".into(),
    ];
    let mut bytes = Bounded(Vec::new());
    serde_json::to_writer(
        &mut bytes,
        &(
            input.generation,
            &input.image.path,
            input.image.review.summary(),
            input.source.epoch,
            &input.source.identity,
            &input.source.buffer,
            &input.source.globals,
            format!("{:?}", input.source.persistence),
            &input.source.recovery,
            input.local_draft_excluded,
            &input.proposal,
            &input.evidence,
            &blockers,
        ),
    )
    .map_err(|e| e.to_string())?;
    let view = LocalFirmwarePreparation {
        digest: JobSha256::of(&bytes.0).hex(),
        image_sha256: input.image.review.summary().source_sha256.hex(),
        archive_sha256: input.proposal.archive_sha256,
        sidecar_sha256: input.proposal.sidecar_sha256,
        evidence_index_sha256: input.evidence.index_sha256,
        acquisition: input.proposal.acquisition,
        source_epoch: input.source.epoch,
        local_draft_excluded: input.local_draft_excluded,
        backup: input.proposal.review,
        blockers,
        permits_flash: false,
        completion_verified: false,
    };
    Ok(PreparedLocalReview {
        view,
        image: input.image,
        source: input.source,
    })
}
pub(crate) struct FirmwareWorker {
    send: SyncSender<Request>,
    replies: Receiver<Reply>,
}
impl Default for FirmwareWorker {
    fn default() -> Self {
        let (send, receive) = mpsc::sync_channel::<Request>(2);
        let (reply, replies) = mpsc::sync_channel(4);
        std::thread::spawn(move || {
            while let Ok(request) = receive.recv() {
                let result = match request.work {
                    Work::Inspect { path, session } => {
                        inspect(&path, session.map(|session| *session), &request.cancel)
                            .map(Box::new)
                            .map(Completed::Inspected)
                    }
                    Work::Rehearse {
                        image,
                        model,
                        chunk,
                        simulation,
                    } => rehearse(&image, model, chunk, simulation, &request.cancel)
                        .map(Box::new)
                        .map(Completed::Rehearsed),
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
impl FirmwareWorker {
    pub fn submit(&self, id: u64, cancel: Arc<AtomicBool>, work: Work) -> Result<(), String> {
        self.send
            .try_send(Request { id, cancel, work })
            .map_err(|_| {
                crate::localization::text(
                    "Firmware worker is busy; wait for cancellation to finish",
                )
                .into()
            })
    }
    pub fn poll(&self) -> impl Iterator<Item = Reply> + '_ {
        self.replies.try_iter()
    }
}
fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err(crate::localization::text("Offline operation cancelled").into())
    } else {
        Ok(())
    }
}
fn inspect(
    path: &Path,
    session: Option<DeviceJobSession>,
    cancel: &AtomicBool,
) -> Result<InspectedFirmware, String> {
    check_cancel(cancel)?;
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.file_type().is_file() || meta.len() == 0 || meta.len() > MAX_SOURCE {
        return Err(crate::localization::text(
            "Select a regular FRMW file of 1..64 MiB; symbolic links are not inspected",
        )
        .into());
    }
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let opened = file.metadata().map_err(|e| e.to_string())?;
    if !opened.file_type().is_file() || opened.len() != meta.len() {
        return Err(
            crate::localization::text("Firmware changed before reading; select it again").into(),
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.dev() != meta.dev() || opened.ino() != meta.ino() {
            return Err(crate::localization::text(
                "Firmware changed before reading; select it again",
            )
            .into());
        }
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    let mut block = [0; 64 * 1024];
    loop {
        check_cancel(cancel)?;
        let count = file.read(&mut block).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        if bytes.len() + count > MAX_SOURCE as usize {
            return Err(crate::localization::text("Firmware changed or exceeds 64 MiB").into());
        }
        bytes.extend_from_slice(&block[..count]);
    }
    let after = file.metadata().map_err(|e| e.to_string())?;
    if after.len() != bytes.len() as u64
        || opened.len() != bytes.len() as u64
        || after.modified().ok() != opened.modified().ok()
    {
        return Err(crate::localization::text(
            "Firmware length changed while reading; select it again",
        )
        .into());
    }
    let sha = JobSha256::of(&bytes);
    let source = RetainedJobBlob::checked(
        AssetId(format!("firmware-{}", sha.hex())),
        bytes.into(),
        sha,
    )
    .map_err(|e| e.to_string())?;
    let review = FirmwareReview::inspect(source, session, &FirmwareBackupAssessment::default())
        .map_err(|e| e.to_string())?;
    check_cancel(cancel)?;
    Ok(InspectedFirmware {
        path: path.canonicalize().map_err(|e| e.to_string())?,
        review,
    })
}
fn fixture(model: DeviceModel, chunk: u16) -> Result<BootloaderIdentity, String> {
    if !(1..=255).contains(&chunk) {
        return Err(crate::localization::text("Offline chunk size must be 1..255 bytes").into());
    }
    let mut raw = [0; 39];
    raw[..4].copy_from_slice(&[0xf0, 0x7e, 0x48, 1]);
    let name_string = model.name();
    let name = name_string.as_bytes();
    if name.len() > 16 || matches!(model, DeviceModel::Unknown(_)) {
        return Err(crate::localization::text(
            "Select a known GP-200 variant for the synthetic identity",
        )
        .into());
    }
    raw[4..4 + name.len()].copy_from_slice(name);
    raw[20..27].copy_from_slice(b"OFFLINE");
    raw[36] = (chunk >> 4) as u8;
    raw[37] = (chunk & 15) as u8;
    raw[38] = 0xf7;
    BootloaderIdentity::decode(&raw).map_err(|e| e.to_string())
}
fn rehearse(
    image: &InspectedFirmware,
    model: DeviceModel,
    chunk: u16,
    simulation: FirmwareSimulation,
    cancel: &AtomicBool,
) -> Result<RehearsalReport, String> {
    check_cancel(cancel)?;
    let identity = fixture(model, chunk)?;
    let mut rehearsal = image
        .review
        .rehearse(&identity)
        .map_err(|e| e.to_string())?;
    if rehearsal.total_blocks() > MAX_REHEARSAL_BLOCKS && simulation != FirmwareSimulation::Preview
    {
        return Err(crate::localization::text("Rehearsal exceeds one million blocks; increase the offline chunk size or preview the first block").into());
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut samples = Vec::new();
    let mut blocks = 0;
    let mut previewed_blocks = 0;
    let mut transcript = JobSha256::of(b"");
    let mut sections = 0;
    loop {
        check_cancel(cancel)?;
        if Instant::now() >= deadline {
            return Err(crate::localization::text(
                "Offline rehearsal exceeded its 30 second limit; increase the chunk size",
            )
            .into());
        }
        let preview = rehearsal.next_chunk().map_err(|e| e.to_string())?;
        let link = format!(
            "{}:{}:{}:{}:{}",
            transcript.hex(),
            preview.section,
            preview.block_in_section,
            preview.file_offset,
            preview.wire_sha256.hex()
        );
        transcript = JobSha256::of(link.as_bytes());
        previewed_blocks += 1;
        if samples.len() < 3 || preview.padded_bytes != 0 || preview.block_in_section == 0 {
            samples.push(preview.clone());
        }
        if simulation == FirmwareSimulation::Preview {
            break;
        }
        if simulation == FirmwareSimulation::MissingBlockAcknowledgement && blocks == 2 {
            rehearsal.acknowledgement_missing();
            break;
        }
        rehearsal
            .acknowledge_simulated_chunk(&preview)
            .map_err(|e| e.to_string())?;
        blocks += 1;
        if *rehearsal.phase() == FirmwareRehearsalPhase::AwaitingSectionAcknowledgement {
            if simulation == FirmwareSimulation::MissingSectionAcknowledgement {
                rehearsal.acknowledgement_missing();
                break;
            }
            rehearsal
                .acknowledge_simulated_section(
                    image.review.summary().source_sha256,
                    preview.section,
                )
                .map_err(|e| e.to_string())?;
            sections += 1;
        }
        if *rehearsal.phase() == FirmwareRehearsalPhase::AwaitingFinalizeEvidence {
            break;
        }
    }
    Ok(RehearsalReport {
        source_sha256_hex: image.review.summary().source_sha256.hex(),
        identity_fixture: std::format!("Synthetic {} identity; version OFFLINE", model.name()),
        identity_sha256_hex: JobSha256::of(identity.raw()).hex(),
        negotiated_chunk_bytes: chunk,
        simulation,
        total_blocks: rehearsal.total_blocks(),
        previewed_blocks,
        preview_transcript_sha256_hex: transcript.hex(),
        preview_transcript_hash_recipe: crate::localization::text(
            "SHA256(previous_hex:section:block:file_offset:wire_sha256_hex), initial SHA256(empty), ASCII decimal numbers",
        ),
        simulated_block_acknowledgements: blocks,
        simulated_section_acknowledgements: sections,
        phase: rehearsal.phase().clone(),
        progress: rehearsal.progress(),
        representative_wire_sha256_hex: samples.iter().map(|v| v.wire_sha256.hex()).collect(),
        representative_chunks: samples,
        hardware_commands: 0,
        permits_flash: false,
    })
}
fn export(report: &ReportSnapshot, destination: &Path, cancel: &AtomicBool) -> Result<(), String> {
    check_cancel(cancel)?;
    let bytes = serde_json::to_vec_pretty(report).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_REPORT {
        return Err(crate::localization::text("Firmware report exceeds 128 KiB").into());
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or(crate::localization::text("Choose a report folder"))?
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let name = destination
        .file_name()
        .ok_or(crate::localization::text("Choose a report filename"))?;
    let path = parent.join(name);
    if path == report.source_path || path.canonicalize().ok().as_ref() == Some(&report.source_path)
    {
        return Err(crate::localization::text(
            "The firmware source cannot be replaced by a report",
        )
        .into());
    }
    if let Ok(meta) = fs::symlink_metadata(&path)
        && !meta.file_type().is_file()
    {
        return Err(crate::localization::text(
            "Report destination must be a regular file; symbolic links are not replaced",
        )
        .into());
    }
    let stage = parent.join(format!(
        ".gp200-firmware-report-{}-{}.tmp",
        std::process::id(),
        STAGE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&stage).map_err(|e| e.to_string())?;
    let result = (|| {
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| e.to_string())?;
        check_cancel(cancel)?;
        fs::rename(&stage, &path).map_err(|e| e.to_string())?;
        File::open(&parent)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(stage);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    const IMAGE: &[u8] =
        include_bytes!("../../../tests/fixtures/device-assets/synthetic-firmware.bin");
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "gp200-firmware-ui-{}-{}",
                std::process::id(),
                STAGE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn image(&self) -> InspectedFirmware {
            let path = self.0.join("source.bin");
            fs::write(&path, IMAGE).unwrap();
            inspect(&path, None, &AtomicBool::new(false)).unwrap()
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn retained_review_and_rehearsal_survive_source_replacement_without_trusting_it() {
        let temp = Temp::new();
        let image = temp.image();
        fs::write(&image.path, b"different bytes").unwrap();
        let report = rehearse(
            &image,
            DeviceModel::Gp200,
            7,
            FirmwareSimulation::AllAcknowledged,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(image.review.source(), IMAGE);
        assert_eq!(report.source_sha256_hex, JobSha256::of(IMAGE).hex());
        assert_eq!(
            report.phase,
            FirmwareRehearsalPhase::AwaitingFinalizeEvidence
        );
        assert_eq!(report.simulated_block_acknowledgements, report.total_blocks);
        assert_eq!(report.previewed_blocks, report.total_blocks);
        assert_eq!(report.preview_transcript_sha256_hex.len(), 64);
        assert_eq!(report.simulated_section_acknowledgements, 4);
        assert_eq!(report.hardware_commands, 0);
        assert!(!report.permits_flash);
        assert!(image.review.summary().installed_version.is_none());
    }
    #[test]
    fn missing_acknowledgements_are_indeterminate_and_preview_assumes_none() {
        let temp = Temp::new();
        let image = temp.image();
        for simulation in [
            FirmwareSimulation::MissingBlockAcknowledgement,
            FirmwareSimulation::MissingSectionAcknowledgement,
        ] {
            let result = rehearse(
                &image,
                DeviceModel::Gp200,
                7,
                simulation,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(result.phase, FirmwareRehearsalPhase::Indeterminate);
            assert!(result.simulated_block_acknowledgements < result.total_blocks);
            assert_eq!(result.simulated_section_acknowledgements, 0);
        }
        let preview = rehearse(
            &image,
            DeviceModel::Gp200,
            255,
            FirmwareSimulation::Preview,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            preview.phase,
            FirmwareRehearsalPhase::AwaitingBlockAcknowledgement
        );
        assert_eq!(preview.simulated_block_acknowledgements, 0);
        assert_eq!(preview.representative_chunks.len(), 1);
        assert!(preview.representative_chunks[0].padded_bytes > 0);
    }
    #[test]
    fn synthetic_identity_enforces_variant_and_chunk_bounds() {
        let temp = Temp::new();
        let image = temp.image();
        for model in [
            DeviceModel::Gp200Lt,
            DeviceModel::Gp200Jr,
            DeviceModel::Unknown(65535),
        ] {
            assert!(
                rehearse(
                    &image,
                    model,
                    128,
                    FirmwareSimulation::Preview,
                    &AtomicBool::new(false)
                )
                .is_err()
            );
        }
        for chunk in [0, 256, u16::MAX] {
            assert!(
                rehearse(
                    &image,
                    DeviceModel::Gp200,
                    chunk,
                    FirmwareSimulation::Preview,
                    &AtomicBool::new(false)
                )
                .is_err()
            );
        }
        for chunk in [1, 255] {
            assert_eq!(
                fixture(DeviceModel::Gp200, chunk).unwrap().chunk_bytes,
                chunk
            );
        }
    }
    #[test]
    fn cancellation_prevents_read_rehearsal_and_export_commit() {
        let temp = Temp::new();
        let image = temp.image();
        let cancelled = AtomicBool::new(true);
        assert!(inspect(&image.path, None, &cancelled).is_err());
        assert!(
            rehearse(
                &image,
                DeviceModel::Gp200,
                1,
                FirmwareSimulation::AllAcknowledged,
                &cancelled
            )
            .is_err()
        );
        let output = temp.0.join("report.json");
        fs::write(&output, b"keep").unwrap();
        assert!(export(&ReportSnapshot::new(&image, None), &output, &cancelled).is_err());
        assert_eq!(fs::read(output).unwrap(), b"keep");
    }
    #[test]
    fn review_rejects_truncated_checksum_invalid_and_oversized_sources() {
        let temp = Temp::new();
        let path = temp.0.join("image.bin");
        let mut corrupt = IMAGE.to_vec();
        corrupt[100] ^= 1;
        for bytes in [&IMAGE[..100], corrupt.as_slice()] {
            fs::write(&path, bytes).unwrap();
            assert!(inspect(&path, None, &AtomicBool::new(false)).is_err());
        }
        let file = File::create(&path).unwrap();
        file.set_len(MAX_SOURCE + 1).unwrap();
        assert!(
            inspect(&path, None, &AtomicBool::new(false))
                .unwrap_err()
                .contains("64 MiB")
        );
    }
    #[test]
    fn tiny_chunk_full_simulation_is_bounded_but_a_preview_is_available() {
        let temp = Temp::new();
        let lengths = [1_000_001_usize, 1, 1, 1];
        let mut bytes = vec![0; 80 + lengths.iter().sum::<usize>()];
        bytes[..4].copy_from_slice(b"FRMW");
        let declared = (bytes.len() - 48) as u32;
        bytes[4..8].copy_from_slice(&declared.to_le_bytes());
        bytes[16..22].copy_from_slice(b"GP-200");
        let mut relative = 32_usize;
        for (index, length) in lengths.into_iter().enumerate() {
            bytes[48 + index * 8..52 + index * 8].copy_from_slice(&(relative as u32).to_le_bytes());
            bytes[52 + index * 8..56 + index * 8].copy_from_slice(&(length as u32).to_le_bytes());
            relative += length;
        }
        bytes[88..92].copy_from_slice(b"V180");
        bytes[14] = bytes[48..]
            .iter()
            .fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
        let source = temp.0.join("large.bin");
        fs::write(&source, bytes).unwrap();
        let image = inspect(&source, None, &AtomicBool::new(false)).unwrap();
        assert!(
            rehearse(
                &image,
                DeviceModel::Gp200,
                1,
                FirmwareSimulation::AllAcknowledged,
                &AtomicBool::new(false)
            )
            .unwrap_err()
            .contains("one million")
        );
        let report = rehearse(
            &image,
            DeviceModel::Gp200,
            1,
            FirmwareSimulation::Preview,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(report.total_blocks, 1_000_004);
        assert_eq!(report.previewed_blocks, 1);
    }
    #[test]
    fn atomic_private_report_retains_exact_hashes_and_cannot_replace_the_source() {
        let temp = Temp::new();
        let image = temp.image();
        let report = ReportSnapshot::new(&image, None);
        assert!(export(&report, &image.path, &AtomicBool::new(false)).is_err());
        let output = temp.0.join("report.json");
        fs::write(&output, b"old report").unwrap();
        export(&report, &output, &AtomicBool::new(false)).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
        assert_eq!(json["source_sha256_hex"], JobSha256::of(IMAGE).hex());
        assert_eq!(json["hardware_commands"], 0);
        assert_eq!(json["permits_flash"], false);
        assert_eq!(json["review"]["sections"].as_array().unwrap().len(), 4);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(output).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(!fs::read_dir(&temp.0).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
        assert_eq!(fs::read(&image.path).unwrap(), IMAGE);
    }
    #[cfg(unix)]
    #[test]
    fn symbolic_link_sources_and_report_destinations_are_rejected() {
        let temp = Temp::new();
        let image = temp.image();
        let alias = temp.0.join("alias.bin");
        std::os::unix::fs::symlink(&image.path, &alias).unwrap();
        assert!(inspect(&alias, None, &AtomicBool::new(false)).is_err());
        assert!(
            export(
                &ReportSnapshot::new(&image, None),
                &alias,
                &AtomicBool::new(false)
            )
            .is_err()
        );
        assert!(
            fs::symlink_metadata(alias)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(image.path).unwrap(), IMAGE);
    }
}
