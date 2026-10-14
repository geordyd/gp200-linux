//! Executable-bound, compiled public updater materials. Loading is file-only;
//! existing live draft/owner admission remains responsible for device authority.
use gp200_domain::{JobSha256, ServiceError};
use gp200_transport::{EntryStudyFilePin, LinuxCompletionInputs};
use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const NOFOLLOW: i32 = 0x20_000;
const MATERIALS: [(&str, &[u8]); 4] = [
    (
        "loss.md",
        include_bytes!("../../../assets/firmware/loss.md"),
    ),
    (
        "recovery.md",
        include_bytes!("../../../assets/firmware/recovery.md"),
    ),
    (
        "vendor-guidance.md",
        include_bytes!("../../../assets/firmware/vendor-guidance.md"),
    ),
    (
        "protocol-contract.json",
        include_bytes!("../../../assets/firmware/protocol-contract.json"),
    ),
];

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LinuxFirmwareResources {
    schema: u16,
    executable_sha256: String,
    inputs: LinuxCompletionInputs,
}
fn bad(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::invalid(error.to_string())
}

pub(crate) fn load_completion_resources() -> Result<LinuxCompletionInputs, ServiceError> {
    let executable = fs::canonicalize("/proc/self/exe").map_err(bad)?;
    let executable_bytes = fs::read(&executable).map_err(bad)?;
    let executable_sha256 = JobSha256::of(&executable_bytes).hex();
    // An explicit developer override remains opt-in. Ordinary builds carry
    // their own materials and need no source checkout or adjacent manifest.
    let manifest = match std::env::var_os("GP200_FIRMWARE_RESOURCES") {
        Some(path) => PathBuf::from(path),
        None => builtin_manifest(&data_home()?, &executable_sha256)?,
    };
    load_manifest(&manifest, &executable_sha256)
}
fn data_home() -> Result<PathBuf, ServiceError> {
    let root = std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".local/share"))
        })
        .ok_or_else(|| bad("Firmware preparation needs XDG_DATA_HOME or HOME"))?;
    absolute_plain(&root)?;
    Ok(root)
}
fn absolute_plain(path: &Path) -> Result<(), ServiceError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
    {
        return Err(bad(
            "Firmware resources need an absolute path without dot components",
        ));
    }
    Ok(())
}
fn plain_ancestry(path: &Path) -> Result<(), ServiceError> {
    absolute_plain(path)?;
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).map_err(bad)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(bad("Firmware resource directory ancestry is not plain"));
        }
    }
    Ok(())
}
fn private_directory(path: &Path) -> Result<(), ServiceError> {
    plain_ancestry(path)?;
    let metadata = fs::symlink_metadata(path).map_err(bad)?;
    if metadata.mode() & 0o777 != 0o700 || metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(bad("App firmware directory must be private and owned"));
    }
    Ok(())
}
fn create_private_directory(path: &Path) -> Result<(), ServiceError> {
    let parent = path
        .parent()
        .ok_or_else(|| bad("Firmware directory parent missing"))?;
    plain_ancestry(parent)?;
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => File::open(parent)
            .and_then(|file| file.sync_all())
            .map_err(bad)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(bad(error)),
    }
    private_directory(path)
}
fn prepare_data_home(path: &Path) -> Result<(), ServiceError> {
    absolute_plain(path)?;
    // XDG's base may not yet exist. Inspect each existing ancestor before
    // creating descendants, and do not follow symlinks while materializing.
    for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err(bad("Firmware data directory ancestry is not plain")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                create_private_directory(ancestor)?;
            }
            Err(error) => return Err(bad(error)),
        }
    }
    Ok(())
}
fn pin(path: PathBuf, bytes: &[u8]) -> EntryStudyFilePin {
    EntryStudyFilePin {
        path,
        bytes: bytes.len() as u64,
        sha256: JobSha256::of(bytes).hex(),
    }
}
fn read_private_file(path: &Path, limit: u64) -> Result<Vec<u8>, ServiceError> {
    plain_ancestry(
        path.parent()
            .ok_or_else(|| bad("Firmware file parent missing"))?,
    )?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW)
        .open(path)
        .map_err(bad)?;
    let before = file.metadata().map_err(bad)?;
    if !before.is_file()
        || before.len() == 0
        || before.len() > limit
        || before.nlink() != 1
        || before.uid() != rustix::process::geteuid().as_raw()
        || before.mode() & 0o777 != 0o600
    {
        return Err(bad("Firmware resource is not a bounded private plain file"));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(bad)?;
    let after = file.metadata().map_err(bad)?;
    let named = fs::symlink_metadata(path).map_err(bad)?;
    let stamp = |metadata: &fs::Metadata| {
        (
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        )
    };
    if bytes.len() as u64 != before.len()
        || named.file_type().is_symlink()
        || stamp(&before) != stamp(&after)
        || stamp(&before) != stamp(&named)
    {
        return Err(bad("Firmware resources changed during review"));
    }
    Ok(bytes)
}
fn materialize(path: &Path, bytes: &[u8]) -> Result<EntryStudyFilePin, ServiceError> {
    private_directory(
        path.parent()
            .ok_or_else(|| bad("Firmware material parent missing"))?,
    )?;
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(NOFOLLOW)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(bytes).map_err(bad)?;
            file.sync_all().map_err(bad)?;
            File::open(path.parent().expect("checked parent"))
                .and_then(|file| file.sync_all())
                .map_err(bad)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(bad(error)),
    }
    let retained = read_private_file(path, bytes.len() as u64)?;
    if retained != bytes {
        return Err(bad(
            "Compiled firmware material changed; preparation stopped",
        ));
    }
    Ok(pin(path.into(), bytes))
}
fn builtin_manifest(data_root: &Path, executable_sha256: &str) -> Result<PathBuf, ServiceError> {
    JobSha256::parse(executable_sha256)?;
    if JobSha256::of(MATERIALS[3].1).hex()
        != gp200_transport::firmware_vendor_sequence::VENDOR_CONTRACT_SHA256
    {
        return Err(bad(
            "Compiled firmware contract differs from the current protocol",
        ));
    }
    prepare_data_home(data_root)?;
    let app = data_root.join("gp200-linux");
    create_private_directory(&app)?;
    let resources = app.join("firmware-resources");
    create_private_directory(&resources)?;
    let content = MATERIALS
        .iter()
        .flat_map(|(name, bytes)| {
            format!("{name}:{}:{}\n", bytes.len(), JobSha256::of(bytes).hex()).into_bytes()
        })
        .collect::<Vec<_>>();
    let bundle = resources.join(format!("v1-{}", JobSha256::of(&content).hex()));
    create_private_directory(&bundle)?;
    let pins = MATERIALS
        .iter()
        .map(|(name, bytes)| materialize(&bundle.join(name), bytes))
        .collect::<Result<Vec<_>, _>>()?;
    let runs = resources.join("reviews");
    create_private_directory(&runs)?;
    let manifests = bundle.join("executables");
    create_private_directory(&manifests)?;
    let manifest = manifests.join(format!("{executable_sha256}.json"));
    let resources = LinuxFirmwareResources {
        schema: 1,
        executable_sha256: executable_sha256.into(),
        inputs: LinuxCompletionInputs {
            // The support bytes originate in this running executable. Pin all
            // compiled materials plus this executable-bound manifest; the live
            // constructor independently pins/rechecks /proc/self/exe as before.
            sources: pins.clone(),
            loss: pins[0].clone(),
            recovery: pins[1].clone(),
            vendor_guidance: pins[2].clone(),
            contract: pins[3].clone(),
            run_directory: runs,
            approval_wait_seconds: 900,
            transition_seconds: 60,
            transfer_seconds: 3600,
        },
    };
    let bytes = serde_json::to_vec_pretty(&resources).map_err(bad)?;
    materialize(&manifest, &bytes)?;
    Ok(manifest)
}
fn load_manifest(
    path: &Path,
    executable_sha256: &str,
) -> Result<LinuxCompletionInputs, ServiceError> {
    let bytes = read_private_file(path, 16 * 1024 * 1024)?;
    let mut resources: LinuxFirmwareResources = serde_json::from_slice(&bytes).map_err(bad)?;
    if resources.schema != 1 || resources.executable_sha256 != executable_sha256 {
        return Err(bad(
            "Firmware resources were prepared for a different executable",
        ));
    }
    private_directory(&resources.inputs.run_directory)?;
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let unique = format!(
        "{}:{}:{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(bad)?
            .as_nanos()
    );
    let run = resources
        .inputs
        .run_directory
        .join(JobSha256::of(unique.as_bytes()).hex());
    // A review always gets a fresh directory; an existing one never resumes.
    DirBuilder::new().mode(0o700).create(&run).map_err(bad)?;
    private_directory(&run)?;
    File::open(&resources.inputs.run_directory)
        .and_then(|file| file.sync_all())
        .map_err(bad)?;
    resources.inputs.run_directory = run;
    resources
        .inputs
        .sources
        .push(pin(fs::canonicalize(path).map_err(bad)?, &bytes));
    Ok(resources.inputs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "gp200-public-resources-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            create_private_directory(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn compiled_resources_need_no_checkout_and_bind_private_inputs_to_current_executable() {
        let fixture = Fixture::new();
        let executable = fs::canonicalize("/proc/self/exe").unwrap();
        let digest = JobSha256::of(&fs::read(executable).unwrap()).hex();
        let manifest = builtin_manifest(&fixture.0.join("new-data-home"), &digest).unwrap();
        let inputs = load_manifest(&manifest, &digest).unwrap();
        assert_eq!(
            inputs.contract.sha256,
            gp200_transport::firmware_vendor_sequence::VENDOR_CONTRACT_SHA256
        );
        assert_eq!(
            (
                inputs.approval_wait_seconds,
                inputs.transition_seconds,
                inputs.transfer_seconds
            ),
            (900, 60, 3600)
        );
        assert_eq!(inputs.sources.len(), 5);
        assert_eq!(
            fs::metadata(&inputs.run_directory).unwrap().mode() & 0o777,
            0o700
        );
        for material in &inputs.sources {
            let bytes = read_private_file(&material.path, material.bytes).unwrap();
            assert_eq!(JobSha256::of(&bytes).hex(), material.sha256);
            assert_eq!(fs::metadata(&material.path).unwrap().mode() & 0o777, 0o600);
        }
        assert_eq!(
            builtin_manifest(&fixture.0.join("new-data-home"), &digest).unwrap(),
            manifest
        );
        let again = load_manifest(&manifest, &digest).unwrap();
        assert_ne!(inputs.run_directory, again.run_directory);
        assert!(fs::read_dir(inputs.run_directory).unwrap().next().is_none());
        assert!(load_manifest(&manifest, &"ee".repeat(32)).is_err());
    }
    #[test]
    fn relocating_support_root_recreates_paths_with_the_same_compiled_contract() {
        let fixture = Fixture::new();
        let digest = "ab".repeat(32);
        let first = builtin_manifest(&fixture.0.join("a"), &digest).unwrap();
        let original = load_manifest(&first, &digest).unwrap();
        fs::remove_dir_all(fixture.0.join("a")).unwrap();
        let moved = builtin_manifest(&fixture.0.join("b"), &digest).unwrap();
        let relocated = load_manifest(&moved, &digest).unwrap();
        assert_ne!(original.contract.path, relocated.contract.path);
        assert_eq!(original.contract.sha256, relocated.contract.sha256);
        assert!(
            relocated
                .sources
                .iter()
                .all(|material| material.path.starts_with(fixture.0.join("b")))
        );
    }
    #[test]
    fn material_tamper_links_and_shared_modes_stop_preparation_without_repair() {
        let fixture = Fixture::new();
        let digest = "ab".repeat(32);
        let manifest = builtin_manifest(&fixture.0, &digest).unwrap();
        let inputs = load_manifest(&manifest, &digest).unwrap();
        fs::write(&inputs.loss.path, b"changed").unwrap();
        assert!(builtin_manifest(&fixture.0, &digest).is_err());
        assert_eq!(fs::read(&inputs.loss.path).unwrap(), b"changed");
        fs::remove_file(&inputs.loss.path).unwrap();
        symlink(&inputs.recovery.path, &inputs.loss.path).unwrap();
        assert!(builtin_manifest(&fixture.0, &digest).is_err());
        fs::remove_file(&inputs.loss.path).unwrap();
        builtin_manifest(&fixture.0, &digest).unwrap();
        fs::set_permissions(&inputs.loss.path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(builtin_manifest(&fixture.0, &digest).is_err());
        let fixture = Fixture::new();
        symlink(std::env::temp_dir(), fixture.0.join("linked")).unwrap();
        assert!(builtin_manifest(&fixture.0.join("linked"), &digest).is_err());
        assert!(builtin_manifest(Path::new("relative"), &digest).is_err());
    }
}
