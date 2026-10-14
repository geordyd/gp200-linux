//! Shared persistence for bank captures, reviewed restore journals and setlists.
//! All mutations use the existing library writer lock and atomic manifest commit.
use crate::{
    LibraryStore,
    store::{Manifest, digest, invalid, nonce, valid_hash},
};
use gp200_domain::ServiceError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(crate) const EXTENSION: &str = "gp200-studio.workflows";

/// A checked SHA-256 of exact bytes, not a display name or pedal checksum.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ContentHash(String);
impl ContentHash {
    pub fn new(value: impl Into<String>) -> Result<Self, ServiceError> {
        let value = value.into();
        if !valid_hash(&value) {
            return Err(invalid("Expected a lowercase SHA-256 content hash"));
        }
        Ok(Self(value))
    }
    pub fn of(bytes: &[u8]) -> Self {
        Self(digest(bytes))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D: serde::Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(decoder)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Workflows {
    pub schema_version: u32,
    pub backups: BTreeMap<String, crate::DeviceBackup>,
    pub restores: BTreeMap<String, crate::RestoreJournal>,
    pub setlists: BTreeMap<String, crate::Setlist>,
}
impl Default for Workflows {
    fn default() -> Self {
        Self {
            schema_version: 1,
            backups: BTreeMap::new(),
            restores: BTreeMap::new(),
            setlists: BTreeMap::new(),
        }
    }
}
pub(crate) fn valid_workflow_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}
pub(crate) fn new_id(prefix: &str) -> Result<String, ServiceError> {
    Ok(format!("{prefix}-{}", nonce()?))
}
pub(crate) fn decode_workflows(manifest: &Manifest) -> Result<Workflows, ServiceError> {
    let Some(value) = manifest.extensions.get(EXTENSION) else {
        return Ok(Workflows::default());
    };
    let workflows: Workflows = serde_json::from_value(value.clone()).map_err(invalid)?;
    if workflows.schema_version != 1 {
        return Err(ServiceError::unsupported(
            "Unsupported device workflows schema; no migration or rollback was attempted",
        ));
    }
    if workflows.backups.len() > 64
        || workflows.restores.len() > 128
        || workflows.setlists.len() > 256
    {
        return Err(invalid("Device workflows item limit exceeded"));
    }
    for (id, backup) in &workflows.backups {
        if id != &backup.id || !valid_workflow_id(id) {
            return Err(invalid("Invalid backup identity"));
        }
        backup.validate()?;
    }
    for (id, journal) in &workflows.restores {
        if id != &journal.id || !valid_workflow_id(id) {
            return Err(invalid("Invalid restore journal identity"));
        }
        journal.validate()?;
    }
    for (id, setlist) in &workflows.setlists {
        if id != &setlist.id || !valid_workflow_id(id) {
            return Err(invalid("Invalid setlist identity"));
        }
        setlist.validate()?;
    }
    Ok(workflows)
}
impl LibraryStore {
    pub(crate) fn workflows(&self) -> Result<Workflows, ServiceError> {
        decode_workflows(&self.manifest)
    }
    pub(crate) fn commit_workflows(&mut self, workflows: Workflows) -> Result<(), ServiceError> {
        let mut next = self.manifest.clone();
        next.extensions.insert(
            EXTENSION.into(),
            serde_json::to_value(workflows).map_err(invalid)?,
        );
        decode_workflows(&next)?;
        self.commit(next)
    }
    pub(crate) fn validate_workflow_blobs(&self, manifest: &Manifest) -> Result<(), ServiceError> {
        let workflows = decode_workflows(manifest)?;
        for backup in workflows.backups.values() {
            self.validate_backup_blobs(backup)?;
        }
        for journal in workflows.restores.values() {
            self.validate_restore_blobs(journal)?;
        }
        Ok(())
    }
    /// Opening a new writer cannot prove whether an interrupted write reached the device.
    pub(crate) fn recover_interrupted_restores(&mut self) -> Result<(), ServiceError> {
        let mut workflows = self.workflows()?;
        let mut changed = false;
        for journal in workflows.restores.values_mut() {
            for entry in &mut journal.entries {
                if entry.state == crate::RestoreEntryState::InFlight {
                    entry.state = crate::RestoreEntryState::Uncertain {
                        reason: "Application stopped while a device write was in flight".into(),
                    };
                    journal.state = crate::RestoreRunState::Paused { reason: "Reconnect and read every target before continuing; no write was replayed".into() };
                    changed = true;
                }
            }
        }
        if changed {
            self.commit_workflows(workflows)?;
            self.recovery_note = Some(match self.recovery_note.take() {
                Some(note) => format!("{note}; interrupted restore retained for readback reconciliation"),
                None => "Interrupted restore retained for readback reconciliation; no writes were replayed".into(),
            });
        }
        Ok(())
    }
}
