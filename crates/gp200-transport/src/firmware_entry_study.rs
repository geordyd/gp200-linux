//! Nondefault source-bound entry study. Descriptive exports are never transfer permits.
use crate::{
    firmware_entry_enumeration::{self, EnumerationReceipt},
    firmware_profile::entry_candidates,
};
use gp200_domain::*;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
const NOFOLLOW: i32 = 0x20_000;
const UNKNOWN: &str = "Experimental entry may leave this GP-200 requiring a full update to exit. V1.6.3 is a dated human report, automatic version and revision applicability are unknown. Installed IR audio is unavailable; destination restoration/migration and Linux recovery are unaccepted. Windows access is contingent and unexercised. No boot query, transfer, reset, power cycle or recovery is authorized by this study. Host offers/drain/teardown prove no device success or safe Normal state.";
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryStudyFilePin {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryStudyInputs {
    pub sources: Vec<EntryStudyFilePin>,
    pub loss: EntryStudyFilePin,
    pub recovery: EntryStudyFilePin,
    pub vendor_guidance: EntryStudyFilePin,
    pub audit_directory: PathBuf,
    pub observation_seconds: u64,
    pub approval_wait_seconds: u64,
}
#[derive(Debug, Clone, Serialize)]
pub struct EntryStudyReview {
    pub schema: u16,
    pub job_sha256: String,
    pub reservation: FirmwareCaptureReservation,
    pub acquisition: String,
    pub evidence_index_sha256: String,
    pub genuine_preparation: serde_json::Value,
    pub captured_materials: Vec<EntryStudyFilePin>,
    pub executable: EntryStudyFilePin,
    pub inputs: EntryStudyInputs,
    pub candidates: [Vec<u8>; 2],
    pub timing: String,
    pub unknowns: String,
}
/// Exact same-run review plus its privately retained job. No Clone/Deserialize.
/// ```compile_fail
/// fn clone(x: gp200_transport::EntryStudyDraft){let _=x.clone();}
/// ```
pub struct EntryStudyDraft {
    review: EntryStudyReview,
    job: Arc<ReviewedFirmwareUpdate>,
    created: Instant,
    audit_inode: u64,
}
/// A consuming entry-study capability, compiled only with this experiment feature.
/// ```compile_fail
/// let _:gp200_transport::EntryStudyAuthorization=serde_json::from_str("{}").unwrap();
/// ```
pub struct EntryStudyAuthorization {
    pub(crate) review: EntryStudyReview,
    pub(crate) job: Arc<ReviewedFirmwareUpdate>,
    sha256: String,
    audit_inode: u64,
    approved: Instant,
    native: Arc<EntryNativeFence>,
}
struct EntryNativeFence {
    review: EntryStudyReview,
    approved: Instant,
    deadline: Duration,
    audit_inode: u64,
    owner_stop: OnceLock<Arc<crate::firmware_service::OwnerStop>>,
    processed: AtomicU64,
}
impl EntryNativeFence {
    fn reread(&self) -> Result<(), ServiceError> {
        if self.approved.elapsed() >= self.deadline
            || audit_inode(&self.review.inputs.audit_directory)? != self.audit_inode
            || executable_pin()?.sha256 != self.review.executable.sha256
        {
            return Err(bad("Final native entry authority expired or changed"));
        }
        for pin in self
            .review
            .inputs
            .sources
            .iter()
            .chain(&self.review.captured_materials)
            .chain([
                &self.review.inputs.loss,
                &self.review.inputs.recovery,
                &self.review.inputs.vendor_guidance,
            ])
        {
            read_pin(pin)?;
        }
        #[cfg(test)]
        tests::after_native_reread();
        Ok(())
    }
    fn immediate(
        &self,
        sink: &crate::firmware_backend::FirmwareReceiveSink,
        reader_live: bool,
    ) -> Result<(), ServiceError> {
        #[cfg(test)]
        tests::before_native_immediate();
        let stop = self
            .owner_stop
            .get()
            .ok_or_else(|| bad("Actual entry owner stop binding absent"))?;
        let processed = self.processed.load(Ordering::SeqCst);
        if self.approved.elapsed() >= self.deadline
            || stop.requested.load(Ordering::SeqCst)
            || !reader_live
            || sink.fault.load(Ordering::SeqCst)
            || processed == u64::MAX
            || sink.allocated.load(Ordering::SeqCst) != processed
        {
            return Err(bad(
                "Final native entry deadline, stop, reader fault or original frontier revoked invocation",
            ));
        }
        Ok(())
    }
}
fn bad(s: impl std::fmt::Display) -> ServiceError {
    ServiceError::invalid(s.to_string())
}
fn plain(path: &Path) -> Result<(), ServiceError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(bad("Study path must be absolute without dot components"));
    }
    for p in path.ancestors().skip(1) {
        let m = fs::symlink_metadata(p).map_err(bad)?;
        if !m.is_dir() || m.file_type().is_symlink() {
            return Err(bad("Study path ancestry changed or contains a symlink"));
        }
    }
    Ok(())
}
pub(crate) fn read_pin(pin: &EntryStudyFilePin) -> Result<Vec<u8>, ServiceError> {
    plain(&pin.path)?;
    if pin.bytes == 0
        || pin.bytes > 1024 * 1024 * 1024
        || JobSha256::parse(&pin.sha256)?.hex() != pin.sha256
    {
        return Err(bad("Study input pin invalid"));
    }
    let mut f = OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW)
        .open(&pin.path)
        .map_err(bad)?;
    let before = f.metadata().map_err(bad)?;
    if !before.is_file() || before.len() != pin.bytes {
        return Err(bad("Study input extent changed"));
    }
    let mut bytes = Vec::new();
    (&mut f)
        .take(pin.bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(bad)?;
    let after = f.metadata().map_err(bad)?;
    if bytes.len() as u64 != pin.bytes
        || JobSha256::of(&bytes).hex() != pin.sha256
        || before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
    {
        return Err(bad("Study input bytes or inode changed"));
    }
    Ok(bytes)
}
fn audit_inode(path: &Path) -> Result<u64, ServiceError> {
    plain(&path.join("leaf"))?;
    let m = fs::symlink_metadata(path).map_err(bad)?;
    if !m.is_dir()
        || m.file_type().is_symlink()
        || m.mode() & 0o777 != 0o700
        || m.uid() != rustix::process::geteuid().as_raw()
    {
        return Err(bad(
            "Study audit directory must be owned, private and nonsymlink",
        ));
    }
    Ok(m.ino())
}
fn executable_pin() -> Result<EntryStudyFilePin, ServiceError> {
    let path = fs::canonicalize("/proc/self/exe").map_err(bad)?;
    let bytes = fs::read(&path).map_err(bad)?;
    Ok(EntryStudyFilePin {
        path,
        bytes: bytes.len() as u64,
        sha256: JobSha256::of(&bytes).hex(),
    })
}
impl EntryStudyDraft {
    /// The actual coordinator supplies its private current capture/job. This DTO
    /// alone still cannot pass the service's independent accepted-capture check.
    pub fn prepare(
        job: Arc<ReviewedFirmwareUpdate>,
        reservation: FirmwareCaptureReservation,
        acquisition: String,
        index: String,
        genuine_preparation: serde_json::Value,
        captured_materials: Vec<EntryStudyFilePin>,
        inputs: EntryStudyInputs,
    ) -> Result<Self, ServiceError> {
        reservation.validate()?;
        JobSha256::parse(&index)?;
        if acquisition.is_empty()
            || acquisition.len() > 256
            || !(1..=60).contains(&inputs.observation_seconds)
            || !(30..=900).contains(&inputs.approval_wait_seconds)
            || captured_materials.len() > 4
            || inputs.sources.is_empty()
            || inputs.sources.len() > 3000
        {
            return Err(bad("Entry study bounds or acquisition invalid"));
        }
        let mut paths = std::collections::BTreeSet::new();
        for pin in &inputs.sources {
            if !paths.insert(&pin.path) {
                return Err(bad("Study source path duplicated"));
            }
            read_pin(pin)?;
        }
        for pin in [&inputs.loss, &inputs.recovery, &inputs.vendor_guidance] {
            read_pin(pin)?;
        }
        for pin in &captured_materials {
            read_pin(pin)?;
        }
        let inode = audit_inode(&inputs.audit_directory)?;
        let review = EntryStudyReview {
            schema: 1,
            job_sha256: job.digest().hex(),
            reservation,
            acquisition,
            evidence_index_sha256: index,
            genuine_preparation,
            captured_materials,
            executable: executable_pin()?,
            inputs,
            candidates: entry_candidates(),
            timing: "Each candidate is offered once. Candidate 2 requires complete known candidate-1 host count and Returned drain, actual unchanged live binding/reader/frontier and no observed transition or stop. No explicit inter-offer delay or response wait is established; actual host scheduling includes durable checkpoint/fsync work. Every offer rereads review/source/captured/executable pins after durable prework and checks the deadline/stop/reader/frontier immediately before native invocation; exact execution authority expires after 120 seconds. Passive observation lasts the stated finite window after the last actual write callback returns, including partial/unknown first returns; raw bytes are not ACKs.".into(),
            unknowns: UNKNOWN.into(),
        };
        Ok(Self {
            review,
            job,
            created: Instant::now(),
            audit_inode: inode,
        })
    }
    pub fn review(&self) -> &EntryStudyReview {
        &self.review
    }
    pub fn digest(&self) -> String {
        JobSha256::of(&serde_json::to_vec(&self.review).expect("study review serializes")).hex()
    }
    pub fn expired(&self) -> bool {
        self.created.elapsed() >= Duration::from_secs(self.review.inputs.approval_wait_seconds)
    }
    pub fn approve_exact(self, signal: &str) -> Result<EntryStudyAuthorization, ServiceError> {
        let sha256 = self.digest();
        if self.expired() || signal.trim() != format!("EXECUTE ENTRY ONCE {sha256}") {
            return Err(bad(
                "Exact same-run entry execution signal missing, changed or expired",
            ));
        }
        let approved = Instant::now();
        let native = Arc::new(EntryNativeFence {
            review: self.review.clone(),
            approved,
            deadline: Duration::from_secs(120),
            audit_inode: self.audit_inode,
            owner_stop: OnceLock::new(),
            processed: AtomicU64::new(u64::MAX),
        });
        let authorization = EntryStudyAuthorization {
            review: self.review,
            job: self.job,
            sha256,
            audit_inode: self.audit_inode,
            approved,
            native,
        };
        authorization.revalidate()?;
        Ok(authorization)
    }
}
impl EntryStudyAuthorization {
    pub(crate) fn bind_native_owner(
        &self,
        stop: Arc<crate::firmware_service::OwnerStop>,
    ) -> Result<(), ServiceError> {
        self.native
            .owner_stop
            .set(stop)
            .map_err(|_| bad("Entry native owner already bound"))
    }
    pub(crate) fn arm_native_frontier(&self, processed: u64) {
        self.native.processed.store(processed, Ordering::SeqCst);
    }
    pub(crate) fn revalidate(&self) -> Result<(), ServiceError> {
        self.revalidate_with_deadline(Duration::from_secs(120))
    }
    fn revalidate_with_deadline(&self, deadline: Duration) -> Result<(), ServiceError> {
        if deadline > Duration::from_secs(120)
            || self.approved.elapsed() >= deadline
            || self.review.job_sha256 != self.job.digest().hex()
            || self.review.candidates != entry_candidates()
            || self.sha256 != JobSha256::of(&serde_json::to_vec(&self.review).map_err(bad)?).hex()
            || audit_inode(&self.review.inputs.audit_directory)? != self.audit_inode
        {
            return Err(bad("Entry study authority expired or changed"));
        }
        if executable_pin()?.sha256 != self.review.executable.sha256 {
            return Err(bad("Actual executable changed since exact review"));
        }
        for pin in self
            .review
            .inputs
            .sources
            .iter()
            .chain(&self.review.captured_materials)
            .chain([
                &self.review.inputs.loss,
                &self.review.inputs.recovery,
                &self.review.inputs.vendor_guidance,
            ])
        {
            read_pin(pin)?;
        }
        #[cfg(test)]
        tests::after_authorization_reread();
        if self.approved.elapsed() >= deadline {
            return Err(bad(
                "Entry study authority expired while rereading exact inputs",
            ));
        }
        Ok(())
    }
    pub(crate) fn matches_job(&self, job: &ReviewedFirmwareUpdate) -> bool {
        self.review.job_sha256 == job.digest().hex()
    }
    pub(crate) fn profile_sha256(&self) -> String {
        self.sha256.clone()
    }
    pub(crate) fn open_audit(
        &self,
        binding: &FirmwareUsbObservation,
    ) -> Result<EntryAudit, ServiceError> {
        self.revalidate()?;
        if binding != &self.review.reservation.usb {
            return Err(bad(
                "Actual owned entry binding differs from reviewed fresh capture",
            ));
        }
        let mut audit = EntryAudit::start(
            binding.clone(),
            self.review.inputs.audit_directory.clone(),
            self.review.inputs.observation_seconds,
        )?;
        audit.native = Some(self.native.clone());
        Ok(audit)
    }
}
struct AuditState {
    baseline: EnumerationReceipt,
    transition: bool,
    fault: Option<String>,
    samples: u64,
}
#[derive(Clone)]
struct PassivePaths {
    usb: PathBuf,
    sound: PathBuf,
}
impl PassivePaths {
    fn system() -> Self {
        Self {
            usb: PathBuf::from("/sys/bus/usb/devices"),
            sound: PathBuf::from("/sys/class/sound"),
        }
    }
    fn capture(
        &self,
        binding: &FirmwareUsbObservation,
    ) -> Result<EnumerationReceipt, ServiceError> {
        firmware_entry_enumeration::capture_at(&self.usb, &self.sound, binding)
    }
}
pub(crate) struct EntryAudit {
    native: Option<Arc<EntryNativeFence>>,
    state: Arc<Mutex<AuditState>>,
    file: Arc<Mutex<File>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    binding: FirmwareUsbObservation,
    paths: PassivePaths,
    origin: Instant,
    observation: Duration,
    last_attempt: Option<Instant>,
}
fn append(file: &Mutex<File>, value: serde_json::Value) -> Result<(), ServiceError> {
    let mut f = file.lock().map_err(|_| bad("Study audit mutex poisoned"))?;
    let mut bytes = serde_json::to_vec(&value).map_err(bad)?;
    bytes.push(b'\n');
    f.write_all(&bytes).and_then(|_| f.sync_all()).map_err(bad)
}
impl EntryAudit {
    fn start(
        binding: FirmwareUsbObservation,
        path: PathBuf,
        observation_seconds: u64,
    ) -> Result<Self, ServiceError> {
        Self::start_at(binding, path, observation_seconds, PassivePaths::system())
    }
    fn start_at(
        binding: FirmwareUsbObservation,
        path: PathBuf,
        observation_seconds: u64,
        paths: PassivePaths,
    ) -> Result<Self, ServiceError> {
        let baseline = paths.capture(&binding)?;
        if !baseline.errors.is_empty() {
            return Err(bad(
                "Initial passive USB enumeration contains unresolved errors",
            ));
        }
        let file = Arc::new(Mutex::new(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(NOFOLLOW)
                .open(path.join("entry-host-observations.jsonl"))
                .map_err(bad)?,
        ));
        File::open(&path).and_then(|f| f.sync_all()).map_err(bad)?;
        let state = Arc::new(Mutex::new(AuditState {
            baseline: baseline.clone(),
            transition: false,
            fault: None,
            samples: 0,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let origin = Instant::now();
        append(
            &file,
            serde_json::json!({"kind":"passive-before-open","at_ns":0,"receipt":baseline}),
        )?;
        let s = state.clone();
        let f = file.clone();
        let stopping = stop.clone();
        let b = binding.clone();
        let observed_paths = paths.clone();
        let thread = thread::Builder::new()
            .name("gp200-entry-passive-sysfs".into())
            .spawn(move || {
                while !stopping.load(Ordering::Acquire) {
                    let captured = observed_paths.capture(&b);
                    let mut state = match s.lock() {
                        Ok(value) => value,
                        Err(_) => return,
                    };
                    state.samples += 1;
                    if state.samples > 3600 {
                        state.fault = Some("Passive inventory sample bound exhausted".into());
                        return;
                    }
                    let event = match captured {
                        Ok(receipt) => {
                            if receipt.owned != state.baseline.owned
                                || receipt.normal_interface != state.baseline.normal_interface
                                || receipt.interface_inode != state.baseline.interface_inode
                                || receipt.sound_device != state.baseline.sound_device
                            {
                                state.transition = true;
                            }
                            if !receipt.errors.is_empty() {
                                state.fault = Some("Passive inventory has unresolved errors".into());
                            }
                            serde_json::json!({"kind":"passive-enumeration","at_ns":origin.elapsed().as_nanos().to_string(),"receipt":receipt,"transition":state.transition})
                        }
                        Err(error) => {
                            state.transition = true;
                            state.fault = Some(error.message.clone());
                            serde_json::json!({"kind":"passive-binding-disappeared-or-fault","at_ns":origin.elapsed().as_nanos().to_string(),"error":error.message,"fresh_inventory":firmware_entry_enumeration::inventory_at(&observed_paths.usb).map_err(|e|e.message)})
                        }
                    };
                    if let Err(error) = append(&f, event) {
                        state.fault = Some(error.message);
                        return;
                    }
                    drop(state);
                    thread::sleep(Duration::from_millis(50));
                }
            })
            .map_err(bad)?;
        Ok(Self {
            native: None,
            state,
            file,
            stop,
            thread: Some(thread),
            binding,
            paths,
            origin,
            observation: Duration::from_secs(observation_seconds),
            last_attempt: None,
        })
    }
    pub(crate) fn fresh_before_offer(&self, ordinal: u64) -> Result<(), ServiceError> {
        let receipt = self.paths.capture(&self.binding)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| bad("Passive state poisoned"))?;
        if receipt.owned != state.baseline.owned
            || receipt.normal_interface != state.baseline.normal_interface
            || receipt.interface_inode != state.baseline.interface_inode
            || receipt.sound_device != state.baseline.sound_device
        {
            state.transition = true;
        }
        append(
            &self.file,
            serde_json::json!({"kind":"fresh-per-offer-enumeration","ordinal":ordinal,"at_ns":self.origin.elapsed().as_nanos().to_string(),"receipt":receipt,"sticky_transition":state.transition}),
        )?;
        if state.transition
            || state.fault.is_some()
            || !receipt.errors.is_empty()
            || self.thread.as_ref().is_none_or(|t| t.is_finished())
        {
            return Err(bad(
                "Observed transition, passive fault or closed monitor prohibits entry offer",
            ));
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn mark_attempt(&mut self) {
        self.last_attempt = Some(Instant::now());
    }
    /// This is the only native write boundary, after all durable prework.
    /// Tests use this same helper with an actual callback counter and raw reader.
    pub(crate) fn invoke_native<T>(
        &mut self,
        ordinal: u64,
        sink: &crate::firmware_backend::FirmwareReceiveSink,
        reader_live: impl FnOnce() -> bool,
        invoke: impl FnOnce() -> T,
    ) -> Result<(T, u128, u128), ServiceError> {
        let checked = (|| {
            let native = self
                .native
                .as_ref()
                .ok_or_else(|| bad("Private native entry fence absent"))?;
            native.reread()?;
            // The passive worker can hold this lock across fsync. Never wait
            // after the final reread, and never place disk work after the cheap fence.
            let state = self
                .state
                .try_lock()
                .map_err(|_| bad("Final passive state unavailable"))?;
            if state.transition
                || state.fault.is_some()
                || self.thread.as_ref().is_none_or(|t| t.is_finished())
            {
                return Err(bad(
                    "Final passive transition or monitor fault revoked invocation",
                ));
            }
            native.immediate(sink, reader_live())?;
            let start = self.now();
            self.last_attempt = Some(Instant::now());
            let result = invoke();
            let end = self.now();
            self.last_attempt = Some(Instant::now());
            drop(state);
            Ok((result, start, end))
        })();
        if let Err(error) = &checked {
            self.write_record("entry-native-invocation-denied", 0,
                serde_json::json!({"ordinal":ordinal,"actual_native_invocations":0,"error":error.message,"actor_offer_accounting_may_be_conservative":true}), self.now(), self.now())?;
        }
        checked
    }
    pub(crate) fn write_record(
        &self,
        kind: &str,
        requested: usize,
        result: serde_json::Value,
        started: u128,
        finished: u128,
    ) -> Result<(), ServiceError> {
        append(
            &self.file,
            serde_json::json!({"kind":kind,"requested":requested,"result":result,"started_ns":started.to_string(),"finished_ns":finished.to_string(),"host_only":true}),
        )
    }
    pub(crate) fn now(&self) -> u128 {
        self.origin.elapsed().as_nanos()
    }
    pub(crate) fn healthy(&self) -> Result<(), ServiceError> {
        let state = self
            .state
            .lock()
            .map_err(|_| bad("Passive state poisoned"))?;
        if state.fault.is_some()
            || state.transition
            || self.thread.as_ref().is_none_or(|t| t.is_finished())
        {
            Err(bad("Passive observed binding transition or monitor fault"))
        } else {
            Ok(())
        }
    }
    pub(crate) fn close(&mut self) -> Result<(), ServiceError> {
        if let Some(attempt) = self.last_attempt
            && self.thread.is_some()
        {
            while attempt.elapsed() < self.observation {
                thread::sleep(Duration::from_millis(10));
            }
        }
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.join().map_err(|_| bad("Passive monitor join failed"))?;
            append(
                &self.file,
                serde_json::json!({"kind":"passive-monitor-joined","at_ns":self.now().to_string(),"normal_safe":false}),
            )?;
        }
        Ok(())
    }
}
impl Drop for EntryAudit {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
#[cfg(test)]
#[path = "firmware_entry_study_tests.rs"]
mod tests;
