use crate::{
    DependencyReport, LibraryStore, dependency_report,
    store::invalid,
    workflows::{new_id, valid_workflow_id},
};
use gp200_domain::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetlistEntry {
    pub id: String,
    /// A pinned immutable revision. A new library revision never substitutes for it.
    pub version: LibraryVersionId,
    pub song: String,
    pub section: String,
    pub notes: String,
    pub tempo_fallback: Option<u16>,
    /// Local proposal only. This does not select or overwrite the pedal slot.
    pub proposed_slot: Option<SlotId>,
}
impl SetlistEntry {
    pub fn new(version: LibraryVersionId, song: impl Into<String>) -> Result<Self, ServiceError> {
        Ok(Self {
            id: new_id("song")?,
            version,
            song: song.into(),
            section: String::new(),
            notes: String::new(),
            tempo_fallback: None,
            proposed_slot: None,
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setlist {
    pub id: String,
    pub revision: Revision,
    pub name: String,
    pub notes: String,
    pub entries: Vec<SetlistEntry>,
    pub trashed: bool,
}
impl Setlist {
    pub(crate) fn validate(&self) -> Result<(), ServiceError> {
        if !valid_workflow_id(&self.id)
            || self.name.is_empty()
            || self.name.len() > 1024
            || self.notes.len() > 65536
            || self.entries.len() > 1024
        {
            return Err(invalid("Setlist name, notes or entry count exceeds bounds"));
        }
        let mut ids = BTreeSet::new();
        for entry in &self.entries {
            if !valid_workflow_id(&entry.id)
                || !ids.insert(&entry.id)
                || !valid_workflow_id(&entry.version.0)
                || entry.song.len() > 1024
                || entry.section.len() > 1024
                || entry.notes.len() > 16384
                || entry
                    .tempo_fallback
                    .is_some_and(|tempo| !(40..=250).contains(&tempo))
            {
                return Err(invalid(
                    "Invalid, duplicate or oversized setlist entry; tempo fallback must be 40–250 BPM",
                ));
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetlistReferenceState {
    Available {
        library: LibraryId,
        newer_revision: Option<LibraryVersionId>,
        dependencies: DependencyReport,
    },
    Trashed {
        library: LibraryId,
    },
    Missing,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSetlistEntry {
    pub entry: SetlistEntry,
    pub state: SetlistReferenceState,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSetlist {
    pub setlist: Setlist,
    pub entries: Vec<ResolvedSetlistEntry>,
}

impl LibraryStore {
    pub fn create_setlist(&mut self, name: impl Into<String>) -> Result<Setlist, ServiceError> {
        let setlist = Setlist {
            id: new_id("setlist")?,
            revision: Revision(0),
            name: name.into(),
            notes: String::new(),
            entries: Vec::new(),
            trashed: false,
        };
        setlist.validate()?;
        let mut workflows = self.workflows()?;
        workflows
            .setlists
            .insert(setlist.id.clone(), setlist.clone());
        self.commit_workflows(workflows)?;
        Ok(setlist)
    }
    pub fn setlists(&self, include_trash: bool) -> Result<Vec<Setlist>, ServiceError> {
        Ok(self
            .workflows()?
            .setlists
            .into_values()
            .filter(|setlist| include_trash || !setlist.trashed)
            .collect())
    }
    pub fn setlist(&self, id: &str) -> Result<Setlist, ServiceError> {
        self.workflows()?
            .setlists
            .remove(id)
            .ok_or_else(|| invalid("Setlist does not exist"))
    }
    /// Save preserves explicit ordering and pins; `expected_revision` rejects stale UI edits.
    pub fn save_setlist(
        &mut self,
        mut proposed: Setlist,
        expected_revision: Revision,
    ) -> Result<Setlist, ServiceError> {
        proposed.validate()?;
        let mut workflows = self.workflows()?;
        let current = workflows
            .setlists
            .get(&proposed.id)
            .ok_or_else(|| invalid("Setlist does not exist"))?;
        if current.revision != expected_revision || proposed.revision != expected_revision {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Setlist changed since it was loaded; refresh before saving",
            ));
        }
        if current.trashed != proposed.trashed {
            return Err(invalid("Use the dedicated trash/restore operation"));
        }
        if current.trashed {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Restore this setlist from trash before editing it",
            ));
        }
        proposed.revision = Revision(
            expected_revision
                .0
                .checked_add(1)
                .ok_or_else(|| invalid("Setlist revision exhausted"))?,
        );
        workflows
            .setlists
            .insert(proposed.id.clone(), proposed.clone());
        self.commit_workflows(workflows)?;
        Ok(proposed)
    }
    pub fn setlist_trash(
        &mut self,
        id: &str,
        expected_revision: Revision,
        trashed: bool,
    ) -> Result<Setlist, ServiceError> {
        let mut workflows = self.workflows()?;
        let setlist = workflows
            .setlists
            .get_mut(id)
            .ok_or_else(|| invalid("Setlist does not exist"))?;
        if setlist.revision != expected_revision {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Setlist changed since it was loaded",
            ));
        }
        setlist.trashed = trashed;
        setlist.revision = Revision(
            expected_revision
                .0
                .checked_add(1)
                .ok_or_else(|| invalid("Setlist revision exhausted"))?,
        );
        let result = setlist.clone();
        self.commit_workflows(workflows)?;
        Ok(result)
    }
    pub fn resolve_setlist(&self, id: &str) -> Result<ResolvedSetlist, ServiceError> {
        let setlist = self.setlist(id)?;
        let mut entries = Vec::new();
        for entry in &setlist.entries {
            let state = match self.manifest.versions.get(&entry.version) {
                None => SetlistReferenceState::Missing,
                Some(version) => {
                    let preset = self.manifest.entries.get(&version.library).ok_or_else(|| {
                        invalid("Setlist revision refers to an invalid library entry")
                    })?;
                    if preset.trashed {
                        SetlistReferenceState::Trashed {
                            library: preset.id.clone(),
                        }
                    } else {
                        let loaded = self.load_version(&entry.version)?;
                        SetlistReferenceState::Available {
                            library: loaded.library,
                            newer_revision: (preset.latest != entry.version)
                                .then(|| preset.latest.clone()),
                            dependencies: dependency_report(&loaded.snapshot, self.assets()),
                        }
                    }
                }
            };
            entries.push(ResolvedSetlistEntry {
                entry: entry.clone(),
                state,
            });
        }
        Ok(ResolvedSetlist { setlist, entries })
    }
}
