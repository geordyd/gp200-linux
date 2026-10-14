//! Same-run experimental admission. Persisted descriptions cannot resume an owner.
use crate::firmware_entry_study::{EntryStudyFilePin, read_pin};
use crate::firmware_vendor_sequence::current_firmware_scope;
use gp200_domain::*;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Instant,
};
const NOFOLLOW: i32 = 0x20_000;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxCompletionInputs {
    pub sources: Vec<EntryStudyFilePin>,
    pub loss: EntryStudyFilePin,
    pub recovery: EntryStudyFilePin,
    pub vendor_guidance: EntryStudyFilePin,
    pub contract: EntryStudyFilePin,
    pub run_directory: PathBuf,
    pub approval_wait_seconds: u64,
    pub transition_seconds: u64,
    pub transfer_seconds: u64,
}
#[derive(Debug, Clone, Serialize)]
pub struct LinuxCompletionReview {
    pub schema: u16,
    pub purpose: String,
    pub job_sha256: String,
    pub reservation: FirmwareCaptureReservation,
    pub acquisition: String,
    pub evidence_index_sha256: String,
    pub genuine_preparation: serde_json::Value,
    pub captured_materials: Vec<EntryStudyFilePin>,
    pub executable: EntryStudyFilePin,
    pub inputs: LinuxCompletionInputs,
    pub outgoing: serde_json::Value,
    pub evidence: serde_json::Value,
    pub limitations: String,
}
/// A genuine coordinator draft; not cloneable, deserializable, or executable.
/// ```compile_fail
/// fn clone(x:gp200_transport::LinuxCompletionDraft){let _=x.clone();}
/// ```
pub struct LinuxCompletionDraft {
    review: LinuxCompletionReview,
    job: Arc<ReviewedFirmwareUpdate>,
    created: Instant,
    run_inode: u64,
}
/// Consumed only by the actual MIDI owner after independent capture matching.
/// ```compile_fail
/// let _:gp200_transport::LinuxCompletionAuthorization=serde_json::from_str("{}").unwrap();
/// ```
pub struct LinuxCompletionAuthorization {
    pub(crate) review: LinuxCompletionReview,
    pub(crate) job: Arc<ReviewedFirmwareUpdate>,
    digest: String,
    run_inode: u64,
    granted: Instant,
}
pub(crate) fn bad(s: impl std::fmt::Display) -> ServiceError {
    ServiceError::invalid(s.to_string())
}
pub(crate) fn private_directory(path: &Path) -> Result<u64, ServiceError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
    {
        return Err(bad("Completion directory must be an absolute plain path"));
    }
    for p in path.ancestors() {
        let m = fs::symlink_metadata(p).map_err(bad)?;
        if !m.is_dir() || m.file_type().is_symlink() {
            return Err(bad("Completion directory ancestry is not plain"));
        }
    }
    let m = fs::symlink_metadata(path).map_err(bad)?;
    if m.mode() & 0o777 != 0o700 || m.uid() != rustix::process::geteuid().as_raw() {
        return Err(bad("Completion directory must be private and owned"));
    }
    Ok(m.ino())
}
pub(crate) fn executable_pin() -> Result<EntryStudyFilePin, ServiceError> {
    let path = fs::canonicalize("/proc/self/exe").map_err(bad)?;
    let bytes = fs::read(&path).map_err(bad)?;
    Ok(EntryStudyFilePin {
        path,
        bytes: bytes.len() as u64,
        sha256: JobSha256::of(&bytes).hex(),
    })
}
pub(crate) fn publish(path: &Path, value: &impl Serialize) -> Result<String, ServiceError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(bad)?;
    if bytes.len() > 32 * 1024 * 1024 {
        return Err(bad("Completion review exceeds bounded publication"));
    }
    let parent = path.parent().ok_or_else(|| bad("Review parent missing"))?;
    private_directory(parent)?;
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(NOFOLLOW)
        .open(path)
        .map_err(bad)?;
    f.write_all(&bytes).map_err(bad)?;
    f.sync_all().map_err(bad)?;
    std::fs::File::open(parent)
        .and_then(|d| d.sync_all())
        .map_err(bad)?;
    let digest = JobSha256::of(&bytes).hex();
    read_pin(&EntryStudyFilePin {
        path: path.into(),
        bytes: bytes.len() as u64,
        sha256: digest.clone(),
    })?;
    Ok(digest)
}
impl LinuxCompletionDraft {
    /// Descriptive inputs cannot bypass the service's actual accepted-capture match.
    pub fn prepare(
        job: Arc<ReviewedFirmwareUpdate>,
        reservation: FirmwareCaptureReservation,
        acquisition: String,
        index: String,
        genuine: serde_json::Value,
        captured: Vec<EntryStudyFilePin>,
        inputs: LinuxCompletionInputs,
    ) -> Result<Self, ServiceError> {
        reservation.validate()?;
        JobSha256::parse(&index)?;
        if acquisition.is_empty()
            || acquisition.len() > 256
            || !(30..=900).contains(&inputs.approval_wait_seconds)
            || !(5..=120).contains(&inputs.transition_seconds)
            || !(60..=3600).contains(&inputs.transfer_seconds)
            || inputs.sources.is_empty()
            || inputs.sources.len() > 3000
            || captured.len() != 3
            || !current_firmware_scope(
                &job.review().summary().source_sha256.hex(),
                &inputs.contract.sha256,
            )
        {
            return Err(bad(
                "Completion scope, exact image/contract or finite bounds differ",
            ));
        }
        let mut paths = std::collections::BTreeSet::new();
        for pin in &inputs.sources {
            if !paths.insert(&pin.path) {
                return Err(bad("Duplicate completion source pin"));
            }
            read_pin(pin)?;
        }
        for pin in [
            &inputs.loss,
            &inputs.recovery,
            &inputs.vendor_guidance,
            &inputs.contract,
        ]
        .into_iter()
        .chain(captured.iter())
        {
            read_pin(pin)?;
        }
        let run_inode = private_directory(&inputs.run_directory)?;
        let review=LinuxCompletionReview{schema:1,purpose:"normal entry only; boot query and transfer require later exact same-run reviews".into(),job_sha256:job.digest().hex(),reservation,acquisition,evidence_index_sha256:index,genuine_preparation:genuine,captured_materials:captured,executable:executable_pin()?,inputs,outgoing:serde_json::json!({"entry_candidates":crate::firmware_profile::entry_candidates(),"offers_each":1,"query_permitted":false,"transfer_permitted":false}),evidence:serde_json::json!({"boot_binding":"unobserved","negotiated_chunk":"unobserved","normal_return":"unobserved"}),limitations:"Experimental updater tested on one GP-200 from V1.6.3 to V1.8.2, including full-image recovery after interruption. Other devices and firmware paths remain unverified. Entry may require a completed update to exit. All installed IR audio is unavailable; local source and metadata are separate. Host write/drain, raw unkeyed statuses and terminal09 prove no device commit. No retry, imported-journal resume, reset, power cycle or speculative clearance is authorized. Boot-only Linux recovery requires a separate checked local predecessor lineage, a fresh separately approved boot query and a new complete-image review; retained normal captures and prior approvals cannot authorize it. Fresh normal reads, separate manual application-version confirmation and this live owner's durable closure are required to return to editing; covered content remains separate.".into()};
        Ok(Self {
            review,
            job,
            created: Instant::now(),
            run_inode,
        })
    }
    pub fn review(&self) -> &LinuxCompletionReview {
        &self.review
    }
    pub fn digest(&self) -> String {
        JobSha256::of(&serde_json::to_vec(&self.review).expect("review serializes")).hex()
    }
    pub fn expired(&self) -> bool {
        self.created.elapsed().as_secs() > self.review.inputs.approval_wait_seconds
    }
    pub fn approve_exact(self, signal: &str) -> Result<LinuxCompletionAuthorization, ServiceError> {
        if self.expired() || signal != format!("execute-entry:{}", self.digest()) {
            return Err(bad(
                "Completion entry signal does not match this finite live draft",
            ));
        }
        let authorization = LinuxCompletionAuthorization {
            digest: self.digest(),
            review: self.review,
            job: self.job,
            run_inode: self.run_inode,
            granted: Instant::now(),
        };
        authorization.revalidate()?;
        Ok(authorization)
    }
}
impl LinuxCompletionAuthorization {
    pub(crate) fn revalidate(&self) -> Result<(), ServiceError> {
        if private_directory(&self.review.inputs.run_directory)? != self.run_inode
            || self.review.job_sha256 != self.job.digest().hex()
            || self.review.executable.sha256 != executable_pin()?.sha256
        {
            return Err(bad("Completion live owner or executable changed"));
        }
        for pin in self
            .review
            .inputs
            .sources
            .iter()
            .chain(self.review.captured_materials.iter())
            .chain([
                &self.review.inputs.loss,
                &self.review.inputs.recovery,
                &self.review.inputs.vendor_guidance,
                &self.review.inputs.contract,
            ])
        {
            read_pin(pin)?;
        }
        Ok(())
    }
    pub(crate) fn entry_deadline(&self) -> Instant {
        self.granted + std::time::Duration::from_secs(120)
    }
    pub(crate) fn entry_fresh(&self) -> Result<(), ServiceError> {
        if Instant::now() >= self.entry_deadline() {
            return Err(bad("Entry admission expired"));
        }
        self.revalidate()?;
        if Instant::now() >= self.entry_deadline() {
            return Err(bad("Entry admission expired during source revalidation"));
        }
        Ok(())
    }
    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }
    pub(crate) fn read_signal(&self, name: &str, expected: &str) -> Result<bool, ServiceError> {
        read_phase_signal(&self.review.inputs, self.run_inode, name, expected)
    }
}

pub(crate) fn read_phase_signal(
    inputs: &LinuxCompletionInputs,
    run_inode: u64,
    name: &str,
    expected: &str,
) -> Result<bool, ServiceError> {
    if !matches!(
        name,
        "approve-query" | "approve-transfer" | "confirm-application-version"
    ) {
        return Err(bad("Unknown completion signal"));
    }
    if private_directory(&inputs.run_directory)? != run_inode {
        return Err(bad("Completion signal directory changed"));
    }
    let path = inputs.run_directory.join(name);
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(bad(e)),
    };
    let before = file.metadata().map_err(bad)?;
    if !before.is_file()
        || before.len() > 512
        || before.nlink() != 1
        || before.uid() != rustix::process::geteuid().as_raw()
        || before.mode() & 0o777 != 0o600
    {
        return Err(bad("Completion signal is not a private plain file"));
    }
    let mut text = String::new();
    (&mut file)
        .take(513)
        .read_to_string(&mut text)
        .map_err(bad)?;
    let after = file.metadata().map_err(bad)?;
    let named = fs::symlink_metadata(&path).map_err(bad)?;
    if named.file_type().is_symlink()
        || named.ino() != before.ino()
        || named.dev() != before.dev()
        || after.len() != before.len()
        || after.mtime() != before.mtime()
        || after.mtime_nsec() != before.mtime_nsec()
        || after.ctime() != before.ctime()
        || after.ctime_nsec() != before.ctime_nsec()
    {
        return Err(bad("Completion phase signal changed while reading"));
    }
    if text.trim() != expected {
        return Err(bad("Completion signal is not this exact live phase review"));
    }
    Ok(true)
}
