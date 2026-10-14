use gp200_domain::{ErrorKind, ServiceError};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
static NONCE: AtomicU64 = AtomicU64::new(0);
pub(crate) fn invalid(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::invalid(error.to_string())
}
pub(crate) fn io(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::new(ErrorKind::Io, error.to_string())
}
pub(crate) fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub(crate) fn reject_links(path: &Path) -> Result<(), ServiceError> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().map_err(io)?.join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        if matches!(component, Component::ParentDir) {
            return Err(ServiceError::invalid(
                "Paths with '..' are not accepted; select the actual path",
            ));
        }
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ServiceError::invalid(
                    "Symbolic-link paths are not accepted",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(error)),
        }
    }
    Ok(())
}
pub(crate) fn read_bounded(path: &Path, maximum: usize) -> Result<Vec<u8>, ServiceError> {
    reject_links(path)?;
    let before = fs::metadata(path).map_err(io)?;
    if !before.is_file() || before.len() > maximum as u64 {
        return Err(ServiceError::invalid(
            "Input must be a regular file within the command's size limit",
        ));
    }
    let file = File::open(path).map_err(io)?;
    let metadata = file.metadata().map_err(io)?;
    if !metadata.is_file() || metadata.len() > maximum as u64 {
        return Err(ServiceError::invalid(
            "Input must be a regular file within the command's size limit",
        ));
    }
    let mut bytes = Vec::new();
    file.take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() > maximum {
        return Err(ServiceError::invalid("Input grew beyond its size limit"));
    }
    Ok(bytes)
}
pub(crate) fn fresh_output(path: &Path) -> Result<(), ServiceError> {
    reject_links(path)?;
    if path.exists() {
        return Err(ServiceError::new(
            ErrorKind::Conflict,
            "Output already exists; choose a new file",
        ));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() || path.file_name().is_none() {
        return Err(ServiceError::invalid(
            "Output needs an existing directory and a file name",
        ));
    }
    Ok(())
}
pub(crate) struct AtomicOutput {
    pub path: PathBuf,
    destination: PathBuf,
}
impl AtomicOutput {
    pub fn new(destination: &Path) -> Result<Self, ServiceError> {
        fresh_output(destination)?;
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let sequence = NONCE.fetch_add(1, Ordering::Relaxed);
        let extension = destination
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("tmp");
        let path = parent.join(format!(
            ".gp200-cli-{}-{sequence}.{extension}",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&path).map_err(io)?;
        Ok(Self {
            path,
            destination: destination.to_owned(),
        })
    }
    pub fn publish(self) -> Result<(), ServiceError> {
        reject_links(&self.path)?;
        reject_links(&self.destination)?;
        File::open(&self.path)
            .and_then(|f| f.sync_all())
            .map_err(io)?;
        fs::hard_link(&self.path, &self.destination).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                ServiceError::new(
                    ErrorKind::Conflict,
                    "Output appeared while preparing; existing bytes preserved",
                )
            } else {
                io(error)
            }
        })?;
        let parent = self
            .destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent).and_then(|f| f.sync_all()).map_err(io)?;
        Ok(())
    }
}
impl Drop for AtomicOutput {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
pub(crate) fn write_new(path: &Path, bytes: &[u8]) -> Result<(), ServiceError> {
    let output = AtomicOutput::new(path)?;
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&output.path)
        .map_err(io)?;
    file.write_all(bytes).map_err(io)?;
    file.sync_all().map_err(io)?;
    drop(file);
    output.publish()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn destination_race_preserves_existing_bytes_and_cleans_stage() {
        let root =
            std::env::temp_dir().join(format!("gp200-cli-atomic-test-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let destination = root.join("output.bin");
        let output = AtomicOutput::new(&destination).unwrap();
        fs::write(&output.path, b"prepared").unwrap();
        let stage = output.path.clone();
        fs::write(&destination, b"another writer").unwrap();
        assert_eq!(output.publish().unwrap_err().kind, ErrorKind::Conflict);
        assert_eq!(fs::read(&destination).unwrap(), b"another writer");
        assert!(!stage.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
