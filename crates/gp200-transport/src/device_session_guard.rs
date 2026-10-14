//! Cooperative Linux ownership, separate from any application-data/lab locks.
//!
//! Only a freshly enumerated backend endpoint may create an identity. Guarded
//! construction happens before opening MIDI and the guard outlives the inner
//! session's destructor (including its reader join). Enumeration is passive.
use crate::MidiSession;
use gp200_domain::{ErrorKind, ServiceError};
use rustix::fs::{self, FileType, Mode, OFlags};
use rustix::io::Errno;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::File;
use std::path::{Component, Path, PathBuf};

const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// This is deliberately private, not a client-provided selection fingerprint.
pub(crate) struct EndpointIdentity {
    pub(crate) location: String,
    pub(crate) card_id: String,
    pub(crate) device: i32,
    pub(crate) subdevice: i32,
}

impl EndpointIdentity {
    fn key(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(b"gp200-studio-alsa-raw-duplex-v1\0");
        for field in [self.location.as_bytes(), self.card_id.as_bytes()] {
            hash.update((field.len() as u64).to_le_bytes());
            hash.update(field);
        }
        hash.update(self.device.to_le_bytes());
        hash.update(self.subdevice.to_le_bytes());
        // Capture/playback share one duplex endpoint. A crossed route must
        // acquire every distinct endpoint and therefore cannot evade overlap.
        format!("{:x}.lock", hash.finalize())
    }
}

struct GuardRoot {
    directory: File,
    uid: u32,
}

impl GuardRoot {
    fn system() -> Result<Self, ServiceError> {
        let uid = rustix::process::geteuid().as_raw();
        let canonical_runtime = PathBuf::from(format!("/run/user/{uid}"));
        let runtime = match std::fs::symlink_metadata(&canonical_runtime) {
            Ok(_) => canonical_runtime,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Headless sessions have no logind runtime. This UID-scoped
                // fallback is independent of cwd, HOME and XDG_DATA_HOME.
                PathBuf::from(format!("/tmp/gp200-studio-runtime-{uid}"))
            }
            Err(_) => return Err(unsafe_path()),
        };
        if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR") {
            let xdg = PathBuf::from(xdg);
            if xdg != runtime {
                // Never split this user's lock namespace across app-specific
                // environment values. Do not follow even an alias symlink.
                return Err(ServiceError::new(
                    ErrorKind::Io,
                    "Device ownership requires the canonical private user runtime directory; XDG_RUNTIME_DIR does not match",
                ));
            }
        }
        let runtime = Self::open_runtime(&runtime, uid, true)?;
        let app = private_child(&runtime.directory, "gp200-studio", uid)?;
        let directory = private_child(&app, "device-sessions", uid)?;
        Ok(Self { directory, uid })
    }

    fn open_runtime(path: &Path, uid: u32, create_leaf: bool) -> Result<Self, ServiceError> {
        if !path.is_absolute() {
            return Err(unsafe_path());
        }
        let mut directory: File = fs::open("/", DIRECTORY_FLAGS, Mode::empty())
            .map_err(|_| unsafe_path())?
            .into();
        let components = path.components().collect::<Vec<_>>();
        for (index, component) in components.iter().enumerate() {
            let name = match component {
                Component::RootDir => continue,
                Component::Normal(name) => *name,
                _ => return Err(unsafe_path()),
            };
            let leaf = index + 1 == components.len();
            if leaf && create_leaf {
                match fs::mkdirat(&directory, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(_) => return Err(unsafe_path()),
                }
            }
            let next: File = fs::openat(&directory, name, DIRECTORY_FLAGS, Mode::empty())
                .map_err(|_| unsafe_path())?
                .into();
            let stat = fs::fstat(&next).map_err(|_| unsafe_path())?;
            if leaf {
                require_private(&stat, uid, FileType::Directory, 0o700)?;
            } else {
                // A root-owned sticky temporary directory is a valid ancestor;
                // a writable unprotected parent could replace the namespace.
                let writable = stat.st_mode & 0o022 != 0;
                let protected_temporary = stat.st_uid == 0 && stat.st_mode & 0o1000 != 0;
                if (stat.st_uid != 0 && stat.st_uid != uid) || (writable && !protected_temporary) {
                    return Err(unsafe_path());
                }
            }
            directory = next;
        }
        // A bare '/' cannot be a user's private runtime.
        require_private(
            &fs::fstat(&directory).map_err(|_| unsafe_path())?,
            uid,
            FileType::Directory,
            0o700,
        )?;
        Ok(Self { directory, uid })
    }
}

fn require_private(
    stat: &fs::Stat,
    uid: u32,
    file_type: FileType,
    permissions: u32,
) -> Result<(), ServiceError> {
    if FileType::from_raw_mode(stat.st_mode) != file_type
        || stat.st_uid != uid
        || stat.st_mode & 0o7777 != permissions
        || (file_type == FileType::RegularFile && stat.st_nlink != 1)
    {
        return Err(unsafe_path());
    }
    Ok(())
}

fn private_child(parent: &File, name: &str, uid: u32) -> Result<File, ServiceError> {
    match fs::mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
        Ok(()) | Err(Errno::EXIST) => {}
        Err(_) => return Err(unsafe_path()),
    }
    let directory: File = fs::openat(parent, name, DIRECTORY_FLAGS, Mode::empty())
        .map_err(|_| unsafe_path())?
        .into();
    require_private(
        &fs::fstat(&directory).map_err(|_| unsafe_path())?,
        uid,
        FileType::Directory,
        0o700,
    )?;
    Ok(directory)
}

fn unsafe_path() -> ServiceError {
    ServiceError::new(
        ErrorKind::Io,
        "Device ownership runtime path is unsafe or unavailable; use a private user runtime directory",
    )
}

pub(crate) struct DeviceSessionGuard {
    _locks: Vec<File>,
    _physical: Option<crate::firmware_quarantine::PhysicalLease>,
    physical_binding: Option<String>,
    keys: BTreeSet<String>,
    root: GuardRoot,
}

impl DeviceSessionGuard {
    fn acquire(root: &GuardRoot, endpoints: &[EndpointIdentity]) -> Result<Self, ServiceError> {
        let keys = endpoints
            .iter()
            .map(EndpointIdentity::key)
            .collect::<BTreeSet<_>>();
        if keys.is_empty() {
            return Err(ServiceError::invalid("No hardware endpoint selected"));
        }
        let mut guard = Self {
            _locks: Vec::with_capacity(keys.len()),
            _physical: None,
            physical_binding: None,
            keys: BTreeSet::new(),
            root: GuardRoot {
                directory: root.directory.try_clone().map_err(|_| unsafe_path())?,
                uid: root.uid,
            },
        };
        for key in keys {
            let lock: File = fs::openat(
                &root.directory,
                key.as_str(),
                OFlags::RDWR
                    | OFlags::CREATE
                    | OFlags::NOFOLLOW
                    | OFlags::CLOEXEC
                    | OFlags::NONBLOCK,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(|_| unsafe_path())?
            .into();
            require_private(
                &fs::fstat(&lock).map_err(|_| unsafe_path())?,
                root.uid,
                FileType::RegularFile,
                0o600,
            )?;
            lock.try_lock().map_err(|error| match error {
                std::fs::TryLockError::WouldBlock => ServiceError::new(
                    ErrorKind::Busy,
                    "Another GP200 Studio client owns this pedal connection; disconnect it before reconnecting",
                ),
                std::fs::TryLockError::Error(_) => unsafe_path(),
            })?;
            // Never remove lock files: replacing a locked inode would permit
            // another client to acquire a different inode for the same pedal.
            guard.keys.insert(key);
            guard._locks.push(lock);
        }
        Ok(guard)
    }
    #[cfg(test)]
    pub(crate) fn fixture(
        root: &Path,
        endpoints: &[EndpointIdentity],
    ) -> Result<Self, ServiceError> {
        Self::acquire(
            &GuardRoot::open_runtime(root, rustix::process::geteuid().as_raw(), false)?,
            endpoints,
        )
    }
    pub(crate) fn owns_normal_binding(
        &self,
        binding: &gp200_domain::FirmwareUsbObservation,
    ) -> bool {
        let Some(address) = binding.endpoint.strip_prefix("hw:") else {
            return false;
        };
        let fields: Vec<_> = address.split(',').collect();
        if fields.len() != 3 {
            return false;
        }
        let (Ok(device), Ok(subdevice)) = (fields[1].parse(), fields[2].parse()) else {
            return false;
        };
        let endpoint = EndpointIdentity {
            location: binding.interface.clone(),
            card_id: fields[0].into(),
            device,
            subdevice,
        };
        self._physical.is_some()
            && self.physical_binding.as_ref() == Some(&binding.digest())
            && self.keys.contains(&endpoint.key())
    }
    /// Distinct fresh recovery acquisition. Historical descriptions are checked
    /// against the held registry; this does not admit ordinary normal sessions.
    #[cfg(feature = "firmware-linux-completion")]
    pub(crate) fn recovery(
        endpoints: &[EndpointIdentity],
        binding: &gp200_domain::FirmwareUsbObservation,
        registry: &crate::firmware_quarantine::QuarantineRegistry,
        lineage: &crate::firmware_linux_recovery_admission::CheckedRecoveryLineage,
    ) -> Result<Self, ServiceError> {
        lineage.revalidate()?;
        let original = lineage.original_binding();
        if !crate::firmware_linux_completion_binding::associated_identity(original, binding) {
            return Err(ServiceError::invalid(
                "Fresh recovery anchor is unrelated to checked history",
            ));
        }
        registry.check_recovery_lineage(lineage)?;
        let root = GuardRoot::system()?;
        let mut guard = Self::acquire(&root, endpoints)?;
        guard._physical = Some(registry.physical_lease(binding)?);
        guard.physical_binding = Some(binding.digest());
        lineage.revalidate()?;
        Ok(guard)
    }
    #[cfg(test)]
    pub(crate) fn fixture_physical(
        root: &Path,
        endpoints: &[EndpointIdentity],
        binding: &gp200_domain::FirmwareUsbObservation,
        registry: &crate::firmware_quarantine::QuarantineRegistry,
    ) -> Result<Self, ServiceError> {
        let mut guard = Self::fixture(root, endpoints)?;
        guard._physical = Some(registry.physical_lease(binding)?);
        guard.physical_binding = Some(binding.digest());
        Ok(guard)
    }
    pub(crate) fn retain_aliases(
        &mut self,
        endpoints: &[EndpointIdentity],
    ) -> Result<(), ServiceError> {
        if endpoints.is_empty() {
            return Err(ServiceError::invalid(
                "Fresh alias observation contains no endpoint",
            ));
        }
        for endpoint in endpoints {
            let key = endpoint.key();
            if self.keys.contains(&key) {
                continue;
            }
            let mut acquired = Self::acquire(&self.root, std::slice::from_ref(endpoint))?;
            self._locks.append(&mut acquired._locks);
            self.keys.append(&mut acquired.keys);
        }
        Ok(())
    }
}

impl Drop for DeviceSessionGuard {
    fn drop(&mut self) {
        // Explicit unlock also handles a concurrent fork's short pre-exec
        // descriptor inheritance. Every file is CLOEXEC and no descriptor is
        // exposed to callers; children never become session owners.
        for lock in &self._locks {
            let _ = lock.unlock();
        }
    }
}

struct GuardedSession {
    inner: Option<Box<dyn MidiSession>>,
    _guard: Option<DeviceSessionGuard>,
    capture_owner: String,
}

impl MidiSession for GuardedSession {
    fn close_normal_capture(&mut self) -> Result<(), ServiceError> {
        // Existing endpoint/physical ownership stays held through actual join
        // and inner/output destruction, including failure.
        let result = self
            .inner
            .as_mut()
            .ok_or_else(|| {
                ServiceError::new(ErrorKind::Disconnected, "Normal owner already retired")
            })?
            .close_normal_capture();
        drop(self.inner.take());
        result
    }
    fn normal_capture_observation(
        &mut self,
    ) -> Result<crate::FirmwareNormalCaptureObservation, ServiceError> {
        if self._guard.is_none() {
            return Err(ServiceError::new(
                ErrorKind::Disconnected,
                "Normal guard already retired",
            ));
        }
        crate::firmware_quarantine::QuarantineRegistry::system()?.admit_normal()?;
        let mut observation = self
            .inner
            .as_mut()
            .ok_or_else(|| {
                ServiceError::new(ErrorKind::Disconnected, "Normal inner owner retired")
            })?
            .normal_capture_observation()?;
        if observation.origin != gp200_domain::FirmwareCaptureOrigin::GuardedSystemNormal {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Guard cannot upgrade custom/replay provenance",
            ));
        }
        observation.owner = self.capture_owner.clone();
        observation.guarded = true;
        Ok(observation)
    }
    fn capture_write_once(&mut self, bytes: &[u8]) -> crate::FirmwareCaptureWriteResult {
        match self.inner.as_mut() {
            Some(inner) => inner.capture_write_once(bytes),
            None => crate::FirmwareCaptureWriteResult::Known {
                accepted: 0,
                error: Some(ServiceError::new(
                    ErrorKind::Disconnected,
                    "Guarded normal owner retired",
                )),
            },
        }
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), ServiceError> {
        self.inner
            .as_mut()
            .ok_or_else(|| {
                ServiceError::new(ErrorKind::Disconnected, "Guarded normal owner retired")
            })?
            .send(bytes)
    }

    fn take_firmware_handoff(&mut self) -> Result<crate::FirmwareNormalHandoff, ServiceError> {
        let mut handoff = self
            .inner
            .as_mut()
            .ok_or_else(|| {
                ServiceError::new(ErrorKind::Disconnected, "Guarded normal owner retired")
            })?
            .take_firmware_handoff()?;
        handoff.guard = self._guard.take();
        drop(self.inner.take());
        Ok(handoff)
    }
    fn health(&mut self) -> Result<(), ServiceError> {
        self.inner
            .as_mut()
            .ok_or_else(|| {
                ServiceError::new(ErrorKind::Disconnected, "Guarded normal owner retired")
            })?
            .health()
    }
}

impl Drop for GuardedSession {
    fn drop(&mut self) {
        // RawSession joins its reader and closes output in its destructor.
        // Keep the ownership files locked through that entire destructor.
        drop(self.inner.take());
    }
}

pub(crate) fn open_guarded(
    binding: Option<&gp200_domain::FirmwareUsbObservation>,
    endpoints: &[EndpointIdentity],
    open: impl FnOnce() -> Result<Box<dyn MidiSession>, ServiceError>,
) -> Result<Box<dyn MidiSession>, ServiceError> {
    let root = GuardRoot::system()?;
    let registry = crate::firmware_quarantine::QuarantineRegistry::system()?;
    let physical = binding.map(|b| registry.physical_lease(b)).transpose()?;
    registry.admit_normal()?;
    let mut guard = DeviceSessionGuard::acquire(&root, endpoints)?;
    guard._physical = physical;
    guard.physical_binding = binding.map(gp200_domain::FirmwareUsbObservation::digest);
    let inner = open()?;
    Ok(Box::new(GuardedSession {
        inner: Some(inner),
        _guard: Some(guard),
        capture_owner: format!(
            "guard-{}-{:?}",
            std::process::id(),
            std::time::Instant::now()
        ),
    }))
}

#[cfg(test)]
fn open_guarded_at(
    root: &GuardRoot,
    endpoints: &[EndpointIdentity],
    open: impl FnOnce() -> Result<Box<dyn MidiSession>, ServiceError>,
) -> Result<Box<dyn MidiSession>, ServiceError> {
    let guard = DeviceSessionGuard::acquire(root, endpoints)?;
    let inner = open()?;
    Ok(Box::new(GuardedSession {
        inner: Some(inner),
        _guard: Some(guard),
        capture_owner: format!(
            "guard-{}-{:?}",
            std::process::id(),
            std::time::Instant::now()
        ),
    }))
}

#[cfg(test)]
#[path = "device_session_guard/tests.rs"]
mod tests;
