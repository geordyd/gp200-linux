//! Private exact editing journal. This module has no transport dependency.
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use fs2::FileExt;
use gp200_domain::*;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAGIC: &[u8; 8] = b"GP2REC01";
const HEADER: usize = 60;
static NEXT_STAGE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct RetainedDraftRecovery {
    pub sha256: String,
    pub written_at_unix_ms: u64,
    pub session: Arc<RecoverySession>,
}
pub struct DraftRecoveryStore {
    root: PathBuf,
    _lock: File,
}
fn io(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::new(ErrorKind::Io, format!("Draft recovery: {error}"))
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn regular_if_present(path: &Path) -> Result<(), ServiceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(io("a journal or lock path is not a regular file")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io(e)),
    }
}
fn private_directory(path: &Path) -> Result<(), ServiceError> {
    if !path.is_absolute() {
        return Err(io("recovery directory must be absolute"));
    }
    let mut component_path = PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::ParentDir | Component::CurDir) {
            return Err(io("recovery path must be normalized"));
        }
        component_path.push(component.as_os_str());
        match fs::symlink_metadata(&component_path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(io(
                    "recovery directory has a symlink or non-directory component",
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                builder.create(&component_path).map_err(io)?;
            }
            Err(e) => return Err(io(e)),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io)?;
    }
    Ok(())
}
fn open_checked(path: &Path) -> Result<File, ServiceError> {
    regular_if_present(path)?;
    let file = File::open(path).map_err(io)?;
    let opened = file.metadata().map_err(io)?;
    let named = fs::symlink_metadata(path).map_err(io)?;
    if !opened.is_file() || named.file_type().is_symlink() {
        return Err(io("journal changed while opening"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (opened.dev(), opened.ino()) != (named.dev(), named.ino()) {
            return Err(io("journal changed while opening"));
        }
    }
    Ok(file)
}
impl DraftRecoveryStore {
    /// Synchronous adapter for worker/bootstrap/tests only. No render-thread IO.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ServiceError> {
        let root = root.as_ref().to_owned();
        private_directory(&root)?;
        let lock_path = root.join("writer.lock");
        regular_if_present(&lock_path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(&lock_path).map_err(io)?;
        let named = fs::symlink_metadata(&lock_path).map_err(io)?;
        if !named.is_file() || named.file_type().is_symlink() {
            return Err(io("lock path changed while opening"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let opened = lock.metadata().map_err(io)?;
            if (opened.dev(), opened.ino()) != (named.dev(), named.ino()) {
                return Err(io("lock path changed while opening"));
            }
        }
        lock.try_lock_exclusive().map_err(|e| {
            ServiceError::new(
                ErrorKind::Busy,
                format!("Draft recovery is open by another application: {e}"),
            )
        })?;
        Ok(Self { root, _lock: lock })
    }
    pub fn path(&self) -> PathBuf {
        self.root.join("session.gp200recovery")
    }
    pub fn discover(&self) -> Result<Option<RetainedDraftRecovery>, ServiceError> {
        let path = self.path();
        regular_if_present(&path)?;
        if !path.exists() {
            return Ok(None);
        }
        let mut file = open_checked(&path)?;
        let size = file.metadata().map_err(io)?.len();
        if size < HEADER as u64 || size > (HEADER + MAX_RECOVERY_JSON) as u64 {
            return Err(ServiceError::invalid(
                "Draft recovery is partial or exceeds its 16 MiB bound; the file was retained",
            ));
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take((HEADER + MAX_RECOVERY_JSON + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        if bytes.len() < HEADER || &bytes[..8] != MAGIC {
            return Err(ServiceError::invalid(
                "Draft recovery header is invalid; the file was retained",
            ));
        }
        let schema = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if schema != DRAFT_RECOVERY_SCHEMA {
            return Err(ServiceError::unsupported(
                "Draft recovery schema is unsupported; keep the file for a compatible application",
            ));
        }
        let written_at_unix_ms = u64::from_le_bytes(bytes[12..20].try_into().unwrap());
        let declared = u64::from_le_bytes(bytes[20..28].try_into().unwrap());
        let mut digest = Sha256::new();
        digest.update(&bytes[..28]);
        digest.update(&bytes[HEADER..]);
        if declared != (bytes.len() - HEADER) as u64
            || digest.finalize().as_slice() != &bytes[28..HEADER]
        {
            return Err(ServiceError::invalid(
                "Draft recovery integrity check failed; the file was retained",
            ));
        }
        let session: RecoverySession = serde_json::from_slice(&bytes[HEADER..])
            .map_err(|e| ServiceError::invalid(format!("Draft recovery JSON rejected: {e}")))?;
        session.validate()?;
        Ok(Some(RetainedDraftRecovery {
            sha256: hash(&bytes),
            written_at_unix_ms,
            session: Arc::new(session),
        }))
    }
    pub fn save(&self, session: &RecoverySession) -> Result<RetainedDraftRecovery, ServiceError> {
        self.save_with_directory_sync(session, |root| File::open(root).and_then(|f| f.sync_all()))
    }
    fn save_with_directory_sync(
        &self,
        session: &RecoverySession,
        directory_sync: impl FnOnce(&Path) -> std::io::Result<()>,
    ) -> Result<RetainedDraftRecovery, ServiceError> {
        session.validate()?;
        let json = serde_json::to_vec(session).map_err(io)?;
        let written_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io)?
            .as_millis()
            .min(u64::MAX as u128) as u64;
        let mut bytes = Vec::with_capacity(HEADER + json.len());
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&DRAFT_RECOVERY_SCHEMA.to_le_bytes());
        bytes.extend_from_slice(&written_at_unix_ms.to_le_bytes());
        bytes.extend_from_slice(&(json.len() as u64).to_le_bytes());
        let mut digest = Sha256::new();
        digest.update(&bytes);
        digest.update(&json);
        bytes.extend_from_slice(&digest.finalize());
        bytes.extend_from_slice(&json);
        let stage = self.root.join(format!(
            ".session-{}-{}.tmp",
            std::process::id(),
            NEXT_STAGE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut stage_created = false;
        let result = (|| {
            regular_if_present(&self.path())?;
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&stage).map_err(io)?;
            stage_created = true;
            file.write_all(&bytes).map_err(io)?;
            file.sync_all().map_err(io)?;
            regular_if_present(&self.path())?;
            fs::rename(&stage, self.path()).map_err(io)?;
            directory_sync(&self.root).map_err(|error| ServiceError::new(ErrorKind::Indeterminate,
                format!("The new recovery snapshot is visible, but directory sync failed ({error}); its durability after a crash is unconfirmed")))?;
            Ok(RetainedDraftRecovery {
                sha256: hash(&bytes),
                written_at_unix_ms,
                session: Arc::new(session.clone()),
            })
        })();
        if result.is_err() && stage_created {
            let _ = fs::remove_file(stage);
        }
        result
    }
    /// Compare the reviewed complete-file hash before deleting. A stale review
    /// cannot discard a newer journal, even if the preset names are identical.
    pub fn discard(&self, reviewed_sha256: &str) -> Result<(), ServiceError> {
        let Some(retained) = self.discover()? else {
            return Err(ServiceError::invalid(
                "The reviewed recovery journal is absent",
            ));
        };
        if retained.sha256 != reviewed_sha256 {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Recovery changed after review; reopen the review before discarding",
            ));
        }
        fs::remove_file(self.path()).map_err(io)?;
        File::open(&self.root)
            .and_then(|f| f.sync_all())
            .map_err(|error| ServiceError::new(ErrorKind::Indeterminate, format!("The reviewed recovery file was removed, but directory sync failed ({error}); removal durability after a crash is unconfirmed")))
    }
}

#[derive(Debug)]
pub enum DraftRecoveryCommand {
    Save(Arc<RecoverySession>),
    Discard {
        reviewed_sha256: String,
    },
    Flush {
        session: Arc<RecoverySession>,
        reply: std::sync::mpsc::Sender<Result<(), ServiceError>>,
    },
}
#[derive(Debug)]
pub enum DraftRecoveryEvent {
    Discovered(Option<RetainedDraftRecovery>),
    Saved(RetainedDraftRecovery),
    Discarded,
    PublicationUncertain {
        visible: Option<RetainedDraftRecovery>,
        error: ServiceError,
    },
    DiscardUncertain(ServiceError),
    Failed(ServiceError),
}
pub struct DraftRecoveryWorker {
    commands: Sender<DraftRecoveryCommand>,
    events: Receiver<DraftRecoveryEvent>,
}
impl DraftRecoveryWorker {
    /// Directory creation, locking and discovery also run on the worker.
    pub fn spawn(root: PathBuf, wake: WakeUi) -> Result<Self, ServiceError> {
        let (commands, rx) = bounded(4);
        let (tx, events) = bounded(8);
        thread::Builder::new()
            .name("gp200-draft-recovery".into())
            .spawn(move || {
                let store = match DraftRecoveryStore::open(root) {
                    Ok(store) => store,
                    Err(error) => {
                        let _ = tx.send(DraftRecoveryEvent::Failed(error));
                        wake();
                        return;
                    }
                };
                let first = match store.discover() {
                    Ok(found) => DraftRecoveryEvent::Discovered(found),
                    Err(error) => DraftRecoveryEvent::Failed(error),
                };
                if tx.send(first).is_err() {
                    return;
                }
                wake();
                while let Ok(command) = rx.recv() {
                    let event = match command {
                        DraftRecoveryCommand::Save(session) => match store.save(&session) {
                            Ok(saved) => DraftRecoveryEvent::Saved(saved),
                            Err(error) if error.kind == ErrorKind::Indeterminate => {
                                DraftRecoveryEvent::PublicationUncertain {
                                    visible: store.discover().ok().flatten(),
                                    error,
                                }
                            }
                            Err(e) => DraftRecoveryEvent::Failed(e),
                        },
                        DraftRecoveryCommand::Discard { reviewed_sha256 } => {
                            match store.discard(&reviewed_sha256) {
                                Ok(()) => DraftRecoveryEvent::Discarded,
                                Err(error) if error.kind == ErrorKind::Indeterminate => {
                                    DraftRecoveryEvent::DiscardUncertain(error)
                                }
                                Err(e) => DraftRecoveryEvent::Failed(e),
                            }
                        }
                        DraftRecoveryCommand::Flush { session, reply } => {
                            let _ = reply.send(store.save(&session).map(|_| ()));
                            continue;
                        }
                    };
                    if tx.send(event).is_err() {
                        break;
                    }
                    wake();
                }
            })
            .map_err(io)?;
        Ok(Self { commands, events })
    }
    pub fn try_submit(
        &self,
        command: DraftRecoveryCommand,
    ) -> Result<(), SubmitError<DraftRecoveryCommand>> {
        self.commands.try_send(command).map_err(|e| match e {
            TrySendError::Full(c) => SubmitError::Busy(c),
            TrySendError::Disconnected(c) => SubmitError::Disconnected(c),
        })
    }
    pub fn try_next_event(&self) -> Option<DraftRecoveryEvent> {
        self.events.try_recv().ok()
    }
    /// Called only on orderly app exit, never during render. IO stays on the
    /// worker; the bounded wait makes the last completed editing frame durable.
    pub fn flush_on_exit(&self, session: Arc<RecoverySession>) -> Result<(), ServiceError> {
        let (reply, rx) = std::sync::mpsc::channel();
        self.try_submit(DraftRecoveryCommand::Flush { session, reply })
            .map_err(|_| {
                ServiceError::new(
                    ErrorKind::Busy,
                    "Recovery worker could not accept the final snapshot",
                )
            })?;
        rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            ServiceError::new(
                ErrorKind::Timeout,
                "Recovery final snapshot did not finish within two seconds",
            )
        })?
    }
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    #[test]
    fn directory_sync_failure_after_rename_reports_visible_new_snapshot_as_indeterminate() {
        let root = std::env::temp_dir().join(format!(
            "gp200-recovery-publish-{}-{}",
            std::process::id(),
            NEXT_STAGE.fetch_add(1, Ordering::Relaxed)
        ));
        let store = DraftRecoveryStore::open(&root).unwrap();
        let original = RecoverySession {
            schema_version: 1,
            app_version: "0.1.0".into(),
            active_document: None,
            documents: vec![],
            session_uncertainty: vec!["Earlier snapshot".into()],
        };
        let earlier = store.save(&original).unwrap();
        let mut new = original.clone();
        new.session_uncertainty = vec!["New exact snapshot".into()];
        let error = store
            .save_with_directory_sync(&new, |_| {
                Err(std::io::Error::other("injected directory sync failure"))
            })
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Indeterminate);
        assert!(error.message.contains("new recovery snapshot is visible"));
        let visible = store.discover().unwrap().unwrap();
        assert_eq!(*visible.session, new);
        assert_ne!(visible.sha256, earlier.sha256);
        assert!(!error.message.contains("previous complete"));
        assert_eq!(*store.save(&new).unwrap().session, new);
        drop(store);
        fs::remove_dir_all(root).unwrap();
    }
}
