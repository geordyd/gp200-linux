//! Offline independent NAM→CLO recipe. The embedded helper contains only the
//! pinned, licensed mathematical kernel. It has no device or network API.
use crate::{
    NamModel, SampleProcessor, SnapToneContainer, WaveAudio, WaveEncoding, cancelled,
    inspect_snaptone, invalid, store::digest,
};
use gp200_domain::{ErrorKind, ServiceError};
use serde::Serialize;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/gp200-calibration"));
const SOURCE_FRAMES: usize = 70 * 44100;
/// Decoded channel 0/1 of the explicitly selected shipped editor stimulus.
pub const GP200_STIMULUS_WAV_SHA256: &str =
    "9bb6c1b136dfbeb7538a6060499d98c89342b76ec568b76836e36ab98b29aa1a";
const STIMULUS_MONO_SHA256: &str =
    "301399b1faba968f9e9e5d63056bea9bafddcbc1700f3f68bc9ca9c8f39a453a";
const MAX_LOG_BYTES: u64 = 32 * 1024;
const PROFILE: &str = "independent-namtoclo-42c07456-rust-nam-v1";
static NEXT_JOB: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct CalibrationOptions {
    pub activation: crate::ActivationMode,
    pub max_operations: u64,
    pub timeout: Duration,
}
impl Default for CalibrationOptions {
    fn default() -> Self {
        Self {
            activation: crate::ActivationMode::Fast,
            max_operations: crate::DEFAULT_NAM_RENDER_OPERATIONS,
            timeout: Duration::from_secs(900),
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct CalibrationReport {
    pub conversion_profile: String,
    pub model_sha256: String,
    pub source_mono_sha256: String,
    pub trainer_input_sha256: String,
    pub nam_response_sha256: String,
    pub helper_sha256: String,
    pub trainer_rate: u32,
    pub frames: u64,
    pub estimated_nam_operations: u64,
    pub activation: String,
    pub latency_samples: u32,
    pub final_spectral_rmse_db: f64,
    pub analysis_windows: u32,
    pub intermediate_sha256: String,
    pub final_sha256: String,
    pub hardware_compatible_verified: bool,
}
pub struct CalibratedSnapTone {
    pub container: SnapToneContainer,
    pub intermediate: SnapToneContainer,
    /// Exact little-endian float32 stage buffers, retained by the asset store.
    pub trainer_input_f32: Vec<u8>,
    pub nam_response_f32: Vec<u8>,
    pub report: CalibrationReport,
}
pub fn calibration_helper_available() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64")) && HELPER.starts_with(b"\x7fELF")
}
/// Hash of the exact helper embedded in this build, for release provenance.
pub fn calibration_helper_sha256() -> Option<String> {
    calibration_helper_available().then(|| digest(HELPER))
}
fn io(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::new(ErrorKind::Io, format!("Offline calibration: {error}"))
}
fn raw_samples(samples: &[f32]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}
fn write_private(path: &Path, bytes: &[u8], executable: bool) -> Result<(), ServiceError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(if executable { 0o500 } else { 0o600 });
    let mut file = options.open(path).map_err(io)?;
    file.write_all(bytes).map_err(io)?;
    file.sync_all().map_err(io)
}
fn read_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>, ServiceError> {
    let metadata = fs::symlink_metadata(path).map_err(io)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > maximum {
        return Err(invalid("Invalid or oversized calibration helper output"));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(io)?
        .take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() as u64 > maximum {
        return Err(invalid("Calibration output grew beyond its bound"));
    }
    Ok(bytes)
}
struct WorkDirectory(PathBuf);
impl WorkDirectory {
    fn new(root: &Path) -> Result<Self, ServiceError> {
        if !root.is_absolute() {
            return Err(invalid("Calibration work root must be absolute"));
        }
        let metadata = fs::symlink_metadata(root).map_err(io)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(invalid(
                "Calibration root must be an existing directory, not a symbolic link",
            ));
        }
        let parent = root.join("calibration-work");
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        match builder.create(&parent) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io(error)),
        }
        let metadata = fs::symlink_metadata(&parent).map_err(io)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(invalid("Calibration directory is not private storage"));
        }
        #[cfg(unix)]
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(invalid(
                "Calibration directory must have private 0700 permissions",
            ));
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io)?
            .as_nanos();
        let path = parent.join(format!(
            "{}-{now}-{}",
            std::process::id(),
            NEXT_JOB.fetch_add(1, Ordering::Relaxed)
        ));
        builder.create(&path).map_err(io)?;
        Ok(Self(path))
    }
}
impl Drop for WorkDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct RunningHelper(Child);
impl Drop for RunningHelper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn run_helper(
    work: &WorkDirectory,
    mode: &str,
    rate: u32,
    deadline: Instant,
    is_cancelled: &impl Fn() -> bool,
) -> Result<(), ServiceError> {
    if is_cancelled() {
        return Err(cancelled());
    }
    let executable = work.0.join("calibration-helper");
    // Every launch checks exact embedded bytes, file type and private ownership
    // permissions. There is no shell invocation or runtime PATH lookup.
    let actual = read_bounded(&executable, HELPER.len() as u64)?;
    if actual != HELPER {
        return Err(invalid("Calibration helper integrity check failed"));
    }
    #[cfg(unix)]
    if fs::metadata(&executable).map_err(io)?.permissions().mode() & 0o777 != 0o500 {
        return Err(invalid("Calibration helper permissions changed"));
    }
    let log = work.0.join(format!("{mode}.log"));
    let mut log_options = OpenOptions::new();
    log_options.write(true).create_new(true);
    #[cfg(unix)]
    log_options.mode(0o600);
    let log_file = log_options.open(&log).map_err(io)?;
    let child = Command::new(&executable)
        .arg(mode)
        .arg(&work.0)
        .arg(rate.to_string())
        .arg(std::process::id().to_string())
        .current_dir(&work.0)
        .env_clear()
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file))
        .spawn()
        .map_err(io)?;
    let mut child = RunningHelper(child);
    loop {
        if is_cancelled() {
            return Err(cancelled());
        }
        if Instant::now() >= deadline {
            return Err(ServiceError::new(
                ErrorKind::Timeout,
                "Offline calibration timed out; helper stopped and no pedal transfer started",
            ));
        }
        if fs::metadata(&log).map_err(io)?.len() > MAX_LOG_BYTES {
            return Err(invalid("Calibration helper exceeded its log bound"));
        }
        if let Some(status) = child.0.try_wait().map_err(io)? {
            if status.success() {
                return Ok(());
            }
            let bytes = read_bounded(&log, MAX_LOG_BYTES)?;
            return Err(invalid(format!(
                "Offline calibration helper failed ({status}): {}",
                String::from_utf8_lossy(&bytes)
            )));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Select the exact 70-second stimulus explicitly. This routine never substitutes
/// bundled audio and never constructs an upload ticket. PCM16 channel selection
/// occurs before this function, through the existing explicit audio preparation.
pub fn calibrate_nam_to_clo(
    nam_bytes: &[u8],
    stimulus: &WaveAudio,
    options: &CalibrationOptions,
    work_root: &Path,
    is_cancelled: impl Fn() -> bool,
    mut progress: impl FnMut(u64, u64),
) -> Result<CalibratedSnapTone, ServiceError> {
    if !calibration_helper_available() {
        return Err(ServiceError::unsupported(
            "Offline calibration helper is unavailable on this platform",
        ));
    }
    stimulus.validate()?;
    if stimulus.channels != 1
        || stimulus.sample_rate != 44100
        || ![SOURCE_FRAMES, SOURCE_FRAMES + 600].contains(&stimulus.frames())
        || stimulus.source_encoding != WaveEncoding::Pcm
        || stimulus.source_bits != 16
    {
        return Err(invalid(
            "Independent calibration requires explicitly selected mono PCM16/44100 70-second conversion stimulus (optional 600 guard frames)",
        ));
    }
    if options.timeout.is_zero() || options.timeout > Duration::from_secs(900) {
        return Err(invalid(
            "Calibration timeout must be positive and at most 900 seconds",
        ));
    }
    let source = raw_samples(&stimulus.samples[..SOURCE_FRAMES]);
    if digest(&source) != STIMULUS_MONO_SHA256 {
        return Err(invalid(
            "Calibration requires the verified GP-200 V1.8.1 structured stimulus; arbitrary 70-second audio is not a calibration signal",
        ));
    }
    let mut model = NamModel::load(nam_bytes)?;
    model.set_activation(options.activation);
    let rate = model.sample_rate();
    if ![44100, 48000, 96000].contains(&rate) {
        return Err(ServiceError::unsupported(
            "Independent calibration has verified conditioning tables only at 44100, 48000 and 96000 Hz",
        ));
    }
    let frames = 70 * rate as usize;
    let estimated = model.validate_render_budget(frames as u64, options.max_operations)?;
    let deadline = Instant::now() + options.timeout;
    if is_cancelled() {
        return Err(cancelled());
    }
    let work = WorkDirectory::new(work_root)?;
    write_private(&work.0.join("calibration-helper"), HELPER, true)?;
    write_private(&work.0.join("source.f32"), &source, false)?;
    progress(0, frames as u64);
    run_helper(&work, "resample", rate, deadline, &is_cancelled)?;
    let input_bytes = read_bounded(&work.0.join("input.f32"), (frames * 4) as u64)?;
    if input_bytes.len() != frames * 4 {
        return Err(invalid(
            "Calibration resampler returned an unexpected sample count",
        ));
    }
    let input: Vec<f32> = input_bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    if input.iter().any(|x| !x.is_finite()) {
        return Err(invalid("Non-finite calibration resampler output"));
    }
    model.reset()?;
    let mut target = vec![0.0_f32; frames];
    for (index, block) in input.chunks(1024).enumerate() {
        if is_cancelled() {
            return Err(cancelled());
        }
        if Instant::now() >= deadline {
            return Err(ServiceError::new(
                ErrorKind::Timeout,
                "Offline NAM calibration rendering timed out",
            ));
        }
        let position = index * 1024;
        let output = &mut target[position..position + block.len()];
        model.process(block, output)?;
        // Explicit upstream independent recipe: unquantized float ×0.31.
        // This differs from the earlier observed clamp→PCM16 host WAV stage.
        for value in output {
            *value *= 0.31;
            if !value.is_finite() {
                return Err(invalid("Non-finite NAM calibration response"));
            }
        }
        progress((position + block.len()) as u64, frames as u64);
    }
    let target_bytes = raw_samples(&target);
    write_private(&work.0.join("target.f32"), &target_bytes, false)?;
    run_helper(&work, "calibrate", rate, deadline, &is_cancelled)?;
    let intermediate = inspect_snaptone(&read_bounded(
        &work.0.join("intermediate.clo"),
        crate::CLO_BYTES as u64,
    )?)?;
    let container = inspect_snaptone(&read_bounded(
        &work.0.join("final.clo"),
        crate::CLO_BYTES as u64,
    )?)?;
    if container.info().declared_bytes != crate::GP200_CLO_DECLARED_BYTES
        || intermediate.info().declared_bytes != crate::CLO_BYTES
    {
        return Err(invalid(
            "Calibration helper returned an unexpected CLO profile",
        ));
    }
    if container
        .info()
        .calibration_words
        .iter()
        .any(|x| !x.as_f32().is_finite())
        || container
            .coefficients(0)
            .into_iter()
            .flatten()
            .chain(container.coefficients(1).into_iter().flatten())
            .any(|x| !x.as_f32().is_finite())
    {
        return Err(invalid("Calibration produced non-finite coefficients"));
    }
    let metrics = read_bounded(&work.0.join("metrics.txt"), 256)?;
    let text = std::str::from_utf8(&metrics).map_err(io)?;
    let values: Vec<_> = text.lines().collect();
    if values.len() != 3 {
        return Err(invalid("Invalid calibration metrics"));
    }
    let latency_samples = values[0].parse::<u32>().map_err(io)?;
    let final_spectral_rmse_db = values[1].parse::<f64>().map_err(io)?;
    let analysis_windows = values[2].parse::<u32>().map_err(io)?;
    if latency_samples > 600
        || !final_spectral_rmse_db.is_finite()
        || final_spectral_rmse_db < 0.0
        || analysis_windows == 0
        || analysis_windows > 300
    {
        return Err(invalid("Out-of-range calibration metrics"));
    }
    if is_cancelled() {
        return Err(cancelled());
    }
    let report = CalibrationReport {
        conversion_profile: PROFILE.into(),
        model_sha256: digest(nam_bytes),
        source_mono_sha256: digest(&source),
        trainer_input_sha256: digest(&input_bytes),
        nam_response_sha256: digest(&target_bytes),
        helper_sha256: digest(HELPER),
        trainer_rate: rate,
        frames: frames as u64,
        estimated_nam_operations: estimated,
        activation: format!("{:?}", options.activation),
        latency_samples,
        final_spectral_rmse_db,
        analysis_windows,
        intermediate_sha256: intermediate.info().sha256.clone(),
        final_sha256: container.info().sha256.clone(),
        hardware_compatible_verified: false,
    };
    Ok(CalibratedSnapTone {
        container,
        intermediate,
        trainer_input_f32: input_bytes,
        nam_response_f32: target_bytes,
        report,
    })
}

#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    struct TestRoot(PathBuf);
    impl TestRoot {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "gp200-calibration-tests-{}-{}",
                std::process::id(),
                NEXT_JOB.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn prepared_work(root: &TestRoot) -> WorkDirectory {
        let work = WorkDirectory::new(&root.0).unwrap();
        write_private(&work.0.join("calibration-helper"), HELPER, true).unwrap();
        write_private(
            &work.0.join("source.f32"),
            &vec![0; SOURCE_FRAMES * 4],
            false,
        )
        .unwrap();
        work
    }
    #[test]
    fn running_helper_is_cancelled_and_reaped_and_work_files_are_removed() {
        let root = TestRoot::new();
        let work = prepared_work(&root);
        let path = work.0.clone();
        let calls = AtomicUsize::new(0);
        let error = run_helper(
            &work,
            "resample",
            96000,
            Instant::now() + Duration::from_secs(10),
            &|| calls.fetch_add(1, Ordering::SeqCst) >= 2,
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Cancelled);
        assert!(calls.load(Ordering::SeqCst) >= 3);
        drop(work);
        assert!(!path.exists());
        assert!(
            fs::read_dir(root.0.join("calibration-work"))
                .unwrap()
                .next()
                .is_none()
        );
    }
    #[test]
    fn helper_timeout_and_corruption_fail_before_publishing_an_artifact() {
        let root = TestRoot::new();
        let work = prepared_work(&root);
        let error = run_helper(&work, "resample", 96000, Instant::now(), &|| false).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Timeout);
        assert!(!work.0.join("final.clo").exists());
        let executable = work.0.join("calibration-helper");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&executable, b"untrusted executable").unwrap();
        let error = run_helper(
            &work,
            "resample",
            44100,
            Instant::now() + Duration::from_secs(1),
            &|| false,
        )
        .unwrap_err();
        assert!(error.message.contains("integrity"));
        assert!(!work.0.join("input.f32").exists());
    }
    #[test]
    fn helper_error_preserves_diagnostic_and_rejects_malformed_sample_file() {
        let root = TestRoot::new();
        let work = WorkDirectory::new(&root.0).unwrap();
        write_private(&work.0.join("calibration-helper"), HELPER, true).unwrap();
        write_private(&work.0.join("source.f32"), &[0; 4], false).unwrap();
        let error = run_helper(
            &work,
            "resample",
            44100,
            Instant::now() + Duration::from_secs(5),
            &|| false,
        )
        .unwrap_err();
        assert!(error.message.contains("sample input length"));
        assert!(!work.0.join("final.clo").exists());
    }
    #[test]
    fn private_work_rejects_symlink_and_world_readable_directory() {
        let root = TestRoot::new();
        let parent = root.0.join("calibration-work");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(WorkDirectory::new(&root.0).is_err());
        fs::remove_dir(&parent).unwrap();
        std::os::unix::fs::symlink(&root.0, &parent).unwrap();
        assert!(WorkDirectory::new(&root.0).is_err());
    }
}
