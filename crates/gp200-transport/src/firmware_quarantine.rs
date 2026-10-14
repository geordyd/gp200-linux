//! Persistent admission history. No disk record issues a continuation permit.
use gp200_domain::*;
use rustix::fs::{self, FileType, Mode, OFlags};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::fd::AsRawFd,
    path::{Component, Path},
};
const DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const FILE: OFlags = OFlags::RDWR.union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);
const MAX_ENTRIES: usize = 1024;
const MAX_FILE: u64 = 1024 * 1024;
fn bad(s: impl std::fmt::Display) -> ServiceError {
    ServiceError::new(
        ErrorKind::Indeterminate,
        format!("Persistent firmware admission unavailable: {s}"),
    )
}
fn valid(f: &File, uid: u32, private: bool, directory: bool) -> Result<(), ServiceError> {
    let s = fs::fstat(f).map_err(bad)?;
    if FileType::from_raw_mode(s.st_mode)
        != if directory {
            FileType::Directory
        } else {
            FileType::RegularFile
        }
        || (s.st_uid != uid && s.st_uid != 0)
        || s.st_mode & 0o022 != 0
        || (private
            && (s.st_uid != uid || s.st_mode & 0o7777 != if directory { 0o700 } else { 0o600 }))
        || (!directory && s.st_nlink != 1)
    {
        return Err(bad("unsafe owner, permissions or file type"));
    }
    Ok(())
}
fn open_absolute(path: &Path, uid: u32) -> Result<File, ServiceError> {
    if !path.is_absolute() {
        return Err(bad("account home is not absolute"));
    }
    let mut d: File = fs::open("/", DIR, Mode::empty()).map_err(bad)?.into();
    valid(&d, uid, false, true)?;
    for part in path.components() {
        let name = match part {
            Component::RootDir => continue,
            Component::Normal(n) => n,
            _ => return Err(bad("invalid account component")),
        };
        d = fs::openat(&d, name, DIR, Mode::empty())
            .map_err(bad)?
            .into();
        valid(&d, uid, false, true)?;
    }
    let stat = fs::fstat(&d).map_err(bad)?;
    if stat.st_uid != uid {
        return Err(bad("account home ownership differs"));
    }
    Ok(d)
}
fn passwd_home(bytes: &[u8], uid: u32) -> Result<std::path::PathBuf, ServiceError> {
    if bytes.len() > 1024 * 1024 {
        return Err(bad("passwd too large"));
    }
    let text = std::str::from_utf8(bytes).map_err(bad)?;
    let mut found = None;
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<_> = line.split(':').collect();
        if fields.len() != 7 || fields[0].is_empty() {
            return Err(bad("malformed local passwd"));
        }
        let id = fields[2].parse::<u32>().map_err(bad)?;
        fields[3].parse::<u32>().map_err(bad)?;
        if id == uid {
            let p = Path::new(fields[5]);
            if found.is_some() || !p.is_absolute() || p == Path::new("/") {
                return Err(bad("duplicate or invalid local account"));
            }
            found = Some(p.to_owned());
        }
    }
    found.ok_or_else(|| bad("UID absent from local passwd; NSS-only accounts are unsupported"))
}
fn child(d: &File, name: &str, uid: u32, private: bool) -> Result<File, ServiceError> {
    child_with_parent_sync(d, name, uid, private, File::sync_all)
}
fn child_with_parent_sync(
    d: &File,
    name: &str,
    uid: u32,
    private: bool,
    sync: impl FnOnce(&File) -> std::io::Result<()>,
) -> Result<File, ServiceError> {
    match fs::mkdirat(d, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(e) => return Err(bad(e)),
    }
    let f: File = fs::openat(d, name, DIR, Mode::empty()).map_err(bad)?.into();
    valid(&f, uid, private, true)?;
    // Flush the actual parent link before returning any child that can publish
    // an armed attempt. Also flush existing links: an earlier failed mkdir sync
    // can leave a visible entry whose durability has not been established.
    sync(d).map_err(bad)?;
    Ok(f)
}
/// Held outside replaceable I/O. Files remain permanently named.
pub(crate) struct PhysicalLease {
    _locks: Vec<File>,
}
impl Drop for PhysicalLease {
    fn drop(&mut self) {
        for f in &self._locks {
            let _ = f.unlock();
        }
    }
}
pub(crate) struct QuarantineRegistry {
    directory: File,
    uid: u32,
    _coord: File,
    mock_root: bool,
}
impl QuarantineRegistry {
    pub(crate) fn system() -> Result<Self, ServiceError> {
        let uid = rustix::process::geteuid().as_raw();
        let root: File = fs::open("/", DIR, Mode::empty()).map_err(bad)?.into();
        valid(&root, 0, false, true)?;
        let etc: File = fs::openat(&root, "etc", DIR, Mode::empty())
            .map_err(bad)?
            .into();
        valid(&etc, 0, false, true)?;
        let mut passwd: File = fs::openat(
            &etc,
            "passwd",
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(bad)?
        .into();
        valid(&passwd, 0, false, false)?;
        let mut bytes = Vec::new();
        (&mut passwd)
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(bad)?;
        let home = open_absolute(&passwd_home(&bytes, uid)?, uid)?;
        let local = child(&home, ".local", uid, false)?;
        let state = child(&local, "state", uid, false)?;
        let app = child(&state, "gp200-studio", uid, true)?;
        Self::at(child(&app, "device-quarantine", uid, true)?, uid, false)
    }
    fn at(directory: File, uid: u32, mock_root: bool) -> Result<Self, ServiceError> {
        valid(&directory, uid, true, true)?;
        let lock: File = fs::openat(
            &directory,
            "registry.lock",
            FILE | OFlags::CREATE,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(bad)?
        .into();
        valid(&lock, uid, true, false)?;
        lock.try_lock().map_err(bad)?;
        Ok(Self {
            directory,
            uid,
            _coord: lock,
            mock_root,
        })
    }
    #[cfg(test)]
    pub(crate) fn mock(path: &Path) -> Result<Self, ServiceError> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path).map_err(bad)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(bad)?;
        let f = fs::open(path, DIR, Mode::empty()).map_err(bad)?.into();
        Self::at(f, rustix::process::geteuid().as_raw(), true)
    }
    pub(crate) fn physical_lease(
        &self,
        binding: &FirmwareUsbObservation,
    ) -> Result<PhysicalLease, ServiceError> {
        binding.validate()?;
        let topology =
            JobSha256::of(format!("gp200-physical-topology-v1\0{}", binding.topology).as_bytes())
                .hex();
        let serial = JobSha256::of(
            format!(
                "gp200-physical-serial-v1\0{}\0{}",
                binding.vendor, binding.serial
            )
            .as_bytes(),
        )
        .hex();
        let mut keys = vec![
            format!("physical-{topology}.lock"),
            format!("physical-{serial}.lock"),
        ];
        keys.sort();
        keys.dedup();
        let mut locks = Vec::new();
        for key in keys {
            let f: File = fs::openat(
                &self.directory,
                key.as_str(),
                FILE | OFlags::CREATE,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(bad)?
            .into();
            valid(&f, self.uid, true, false)?;
            f.try_lock().map_err(|_| {
                ServiceError::new(ErrorKind::Busy, "Another client owns this physical pedal")
            })?;
            locks.push(f);
        }
        Ok(PhysicalLease { _locks: locks })
    }
    pub(crate) fn admit_normal(&self) -> Result<(), ServiceError> {
        let entries = std::fs::read_dir(format!("/proc/self/fd/{}", self.directory.as_raw_fd()))
            .map_err(bad)?;
        let mut count = 0;
        for e in entries {
            let name = e.map_err(bad)?.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| bad("non-UTF8 registry entry"))?;
            let physical_key = name
                .strip_prefix("physical-")
                .and_then(|n| n.strip_suffix(".lock"));
            if name == "registry.lock"
                || physical_key.is_some_and(|n| {
                    n.len() == 64
                        && n.bytes()
                            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                })
            {
                let file: File = fs::openat(&self.directory, name, FILE, Mode::empty())
                    .map_err(bad)?
                    .into();
                valid(&file, self.uid, true, false)?;
                continue;
            }
            count += 1;
            if count > MAX_ENTRIES || !name.ends_with(".quarantine") {
                return Err(bad("unrecognized or excessive registry entries"));
            }
            let mut file: File = fs::openat(&self.directory, name, FILE, Mode::empty())
                .map_err(bad)?
                .into();
            valid(&file, self.uid, true, false)?;
            let (last, _) = read_history(&mut file, self.mock_root)?;
            // Conservative family-wide block; identity attribution is not guessed.
            let linked = if matches!(last, 6 | 7) {
                linked_attempt_closed(
                    &self.directory,
                    self.uid,
                    &FirmwareAttemptId::parse(
                        name.strip_suffix(".quarantine").expect("checked suffix"),
                    )?,
                    self.mock_root,
                )?
            } else {
                false
            };
            if last != 2 && last != 5 && !linked && !(self.mock_root && last == 3) {
                return Err(bad(
                    "unresolved firmware attempt; inspect retained history before reconnecting",
                ));
            }
        }
        Ok(())
    }
    pub(crate) fn reserve(
        &self,
        attempt: &FirmwareAttemptId,
        plan: &FirmwareLifecyclePlanDto,
        image_sha256: &str,
        journal_root: &str,
    ) -> Result<QuarantineAttempt, ServiceError> {
        if plan.origin == FirmwareObservationOrigin::MockSimulation && !self.mock_root {
            return Err(bad("mock origin cannot publish into physical namespace"));
        }
        self.admit_normal()?;
        self.reserve_checked(attempt, plan, image_sha256, journal_root)
    }
    #[cfg(feature = "firmware-linux-completion")]
    pub(crate) fn reserve_recovery(
        &self,
        attempt: &FirmwareAttemptId,
        plan: &FirmwareLifecyclePlanDto,
        image_sha256: &str,
        journal_root: &str,
        lineage: &crate::firmware_linux_recovery_admission::CheckedRecoveryLineage,
    ) -> Result<QuarantineAttempt, ServiceError> {
        self.check_recovery_lineage(lineage)?;
        if plan.origin != FirmwareObservationOrigin::LocalObservation
            || image_sha256 != crate::firmware_vendor_sequence::VENDOR_IMAGE_SHA256
            || !crate::firmware_linux_completion_binding::associated_identity(
                lineage.original_binding(),
                &plan.binding,
            )
            || lineage.references().iter().any(|p| &p.attempt == attempt)
        {
            return Err(bad(
                "Fresh recovery reservation differs from this checked lineage",
            ));
        }
        self.reserve_checked(attempt, plan, image_sha256, journal_root)
    }
    fn reserve_checked(
        &self,
        attempt: &FirmwareAttemptId,
        plan: &FirmwareLifecyclePlanDto,
        image_sha256: &str,
        journal_root: &str,
    ) -> Result<QuarantineAttempt, ServiceError> {
        let name = format!("{}.quarantine", attempt.as_str());
        let temporary = format!("{}.pending", attempt.as_str());
        JobSha256::parse(image_sha256)?;
        let file: File = fs::openat(
            &self.directory,
            temporary.as_str(),
            FILE | OFlags::CREATE | OFlags::EXCL,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(bad)?
        .into();
        valid(&file, self.uid, true, false)?;
        let mut q = QuarantineAttempt {
            file,
            directory: self.directory.try_clone().map_err(bad)?,
            records: 0,
            bytes: 0,
            mock: self.mock_root,
            offered: false,
            attempt: attempt.clone(),
            binding: plan.binding.digest(),
            epoch: plan.epoch,
            generation: plan.generation,
            entry_next: 0,
            entry_run: None,
        };
        // Immutable review/source binding is retained before an armed boundary.
        let body = format!(
            "schema=1\norigin={:?}\nattempt={}\nreview={}\nprofile={}\nbinding={}\ngeneration={}\nclock={}\nimage={}\njournal={}/{}.journal\nupdater_epoch={}\nno_physical_authority=true",
            plan.origin,
            attempt.as_str(),
            plan.review_sha256,
            plan.profile_sha256,
            plan.binding.digest(),
            plan.generation,
            plan.clock_origin,
            image_sha256,
            journal_root,
            attempt.as_str(),
            plan.epoch.0,
        );
        q.publish(0, body.as_bytes())?;
        // A failed publication leaves the exact create-new pending bytes named
        // in this namespace. Unknown pending entries fail normal admission closed.
        fs::renameat_with(
            &self.directory,
            temporary.as_str(),
            &self.directory,
            name.as_str(),
            fs::RenameFlags::NOREPLACE,
        )
        .map_err(bad)?;
        self.directory.sync_all().map_err(bad)?;
        let mut reread: File = fs::openat(&self.directory, name.as_str(), FILE, Mode::empty())
            .map_err(bad)?
            .into();
        valid(&reread, self.uid, true, false)?;
        if read_history(&mut reread, self.mock_root)? != (0, 1) {
            return Err(bad("Published quarantine reservation differs"));
        }
        Ok(q)
    }
    #[cfg(feature = "firmware-linux-completion")]
    pub(crate) fn history_catalogue(
        &self,
        root: &Path,
    ) -> Result<crate::LinuxFirmwareHistoryCatalogue, ServiceError> {
        if self.mock_root {
            return Err(bad("Mock history cannot enter production catalogue"));
        }
        let directory = std::fs::read_link(format!("/proc/self/fd/{}", self.directory.as_raw_fd()))
            .map_err(bad)?;
        crate::firmware_linux_recovery_admission::catalogue_registry(
            &self.directory,
            &directory,
            root,
        )
    }
    #[cfg(feature = "firmware-linux-completion")]
    pub(crate) fn checked_recovery(
        &self,
        library_root: &Path,
        selected: Option<&FirmwareAttemptId>,
    ) -> Result<crate::firmware_linux_recovery_admission::CheckedRecoveryLineage, ServiceError>
    {
        if self.mock_root {
            return Err(bad(
                "Mock registry cannot mint a physical recovery description",
            ));
        }
        let directory = std::fs::read_link(format!("/proc/self/fd/{}", self.directory.as_raw_fd()))
            .map_err(bad)?;
        let selected = self.select_recovery_attempt(selected)?;
        crate::firmware_linux_recovery_admission::load_local_lineage(
            &self.directory,
            &directory,
            library_root,
            &selected,
        )
    }
    #[cfg(feature = "firmware-linux-completion")]
    fn select_recovery_attempt(
        &self,
        selected: Option<&FirmwareAttemptId>,
    ) -> Result<FirmwareAttemptId, ServiceError> {
        Ok(match selected {
            Some(id) => id.clone(),
            None => {
                let mut ids = Vec::new();
                let mut entries = 0_u32;
                for item in
                    std::fs::read_dir(format!("/proc/self/fd/{}", self.directory.as_raw_fd()))
                        .map_err(bad)?
                {
                    entries += 1;
                    if entries > 1024 {
                        return Err(bad("Recovery registry entry budget exceeded"));
                    }
                    let name = item.map_err(bad)?.file_name();
                    let name = name
                        .to_str()
                        .ok_or_else(|| bad("Registry name is not UTF8"))?;
                    let Some(value) = name.strip_suffix(".quarantine") else {
                        continue;
                    };
                    let mut file: File = fs::openat(&self.directory, name, FILE, Mode::empty())
                        .map_err(bad)?
                        .into();
                    valid(&file, self.uid, true, false)?;
                    let attempt = FirmwareAttemptId::parse(value)?;
                    let state = read_history(&mut file, false)?.0;
                    match state {
                        2 | 5 => continue,
                        6 | 7
                            if linked_attempt_closed(
                                &self.directory,
                                self.uid,
                                &attempt,
                                self.mock_root,
                            )? =>
                        {
                            continue;
                        }
                        7 => {
                            return Err(bad(
                                "Linked successor lacks a complete validated predecessor closure",
                            ));
                        }
                        _ => ids.push(attempt),
                    }
                    if ids.len() > 64 {
                        return Err(bad("Recovery lineage count exceeds budget"));
                    }
                }
                ids.sort();
                ids.into_iter()
                    .next()
                    .ok_or_else(|| bad("No unresolved local Linux completion attempt exists"))?
            }
        })
    }
    #[cfg(all(test, feature = "firmware-linux-completion"))]
    pub(crate) fn checked_recovery_fixture_default(
        &self,
        root: &Path,
    ) -> Result<crate::firmware_linux_recovery_admission::CheckedRecoveryLineage, ServiceError>
    {
        if !self.mock_root {
            return Err(bad("Test default selection requires isolated registry"));
        }
        let selected = self.select_recovery_attempt(None)?;
        self.checked_recovery_fixture(root, &selected)
    }
    #[cfg(all(test, feature = "firmware-linux-completion"))]
    pub(crate) fn checked_recovery_fixture(
        &self,
        root: &Path,
        selected: &FirmwareAttemptId,
    ) -> Result<crate::firmware_linux_recovery_admission::CheckedRecoveryLineage, ServiceError>
    {
        if !self.mock_root {
            return Err(bad("Mock-only recovery checker requires isolated registry"));
        }
        let directory = std::fs::read_link(format!("/proc/self/fd/{}", self.directory.as_raw_fd()))
            .map_err(bad)?;
        crate::firmware_linux_recovery_admission::load_local_lineage(
            &self.directory,
            &directory,
            root,
            selected,
        )
    }
    #[cfg(all(test, feature = "firmware-linux-completion"))]
    pub(crate) fn reserve_recovery_fixture(
        &self,
        attempt: &FirmwareAttemptId,
        plan: &FirmwareLifecyclePlanDto,
        image: &str,
        root: &str,
        lineage: &crate::firmware_linux_recovery_admission::CheckedRecoveryLineage,
    ) -> Result<QuarantineAttempt, ServiceError> {
        if !self.mock_root {
            return Err(bad(
                "Mock recovery reservation cannot enter physical namespace",
            ));
        }
        lineage.matches_registry(&self.directory)?;
        lineage.revalidate()?;
        self.reserve_checked(attempt, plan, image, root)
    }
    #[cfg(feature = "firmware-linux-completion")]
    pub(crate) fn check_recovery_lineage(
        &self,
        lineage: &crate::firmware_linux_recovery_admission::CheckedRecoveryLineage,
    ) -> Result<(), ServiceError> {
        if self.mock_root {
            return Err(bad("Mock registry cannot admit physical recovery"));
        }
        lineage.matches_registry(&self.directory)?;
        lineage.revalidate()
    }
}
pub(crate) struct QuarantineAttempt {
    file: File,
    directory: File,
    records: u32,
    bytes: u64,
    mock: bool,
    offered: bool,
    attempt: FirmwareAttemptId,
    binding: String,
    epoch: SessionEpoch,
    generation: u64,
    entry_next: u64,
    entry_run: Option<String>,
}
impl QuarantineAttempt {
    fn publish(&mut self, state: u8, body: &[u8]) -> Result<(), ServiceError> {
        let mut payload = vec![state];
        payload.extend_from_slice(body);
        if payload.len() > 16 * 1024 || self.records >= 256 {
            return Err(bad("registry local record cap"));
        }
        let mut frame = b"GP2QUAR1".to_vec();
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&sha2::Sha256::digest(&payload));
        frame.extend_from_slice(&payload);
        if self.bytes + frame.len() as u64 > MAX_FILE {
            return Err(bad("registry attempt cap"));
        }
        self.file.seek(SeekFrom::End(0)).map_err(bad)?;
        self.file.write_all(&frame).map_err(bad)?;
        self.file.sync_all().map_err(bad)?;
        self.directory.sync_all().map_err(bad)?;
        self.file.seek(SeekFrom::Start(self.bytes)).map_err(bad)?;
        let mut actual = vec![0; frame.len()];
        self.file.read_exact(&mut actual).map_err(bad)?;
        if actual != frame
            || self.file.metadata().map_err(bad)?.len() != self.bytes + frame.len() as u64
        {
            return Err(bad("registry exact reread differs"));
        }
        self.bytes += frame.len() as u64;
        self.records += 1;
        Ok(())
    }
    pub(crate) fn arm(&mut self) -> Result<(), ServiceError> {
        self.publish(
            1,
            b"entry armed; a missing send record cannot prove zero offers",
        )
    }
    pub(crate) fn entry_checkpoint(
        &mut self,
        checkpoint: &gp200_library::FirmwareLocalActionCheckpoint,
        study_sha256: &str,
    ) -> Result<(), ServiceError> {
        let action = checkpoint.action();
        let expected = crate::firmware_profile::entry_candidates();
        let wire = usize::try_from(action.ordinal)
            .ok()
            .and_then(|i| expected.get(i));
        if checkpoint.attempt() != &self.attempt
            || action.ordinal != self.entry_next
            || wire != Some(&action.wire)
            || action.kind != FirmwareLifecycleKind::EntryTransition
            || action.block.is_some()
            || action.wire_sha256 != JobSha256::of(&action.wire).hex()
            || action.binding_sha256 != self.binding
            || action.epoch != self.epoch
            || action.generation != self.generation
            || action.rule_sha256 != study_sha256
            || self
                .entry_run
                .as_ref()
                .is_some_and(|run| run != checkpoint.run_id())
            || checkpoint.run_id().is_empty()
            || checkpoint.run_id().len() > 256
            || checkpoint.intent() == 0
            || checkpoint.sequence() == 0
            || JobSha256::parse(checkpoint.prefix_sha256())?.hex() != checkpoint.prefix_sha256()
            || JobSha256::parse(study_sha256)?.hex() != study_sha256
        {
            return Err(bad(
                "Entry registry checkpoint run/action/binding/review differs",
            ));
        }
        let body = format!(
            "entry-action-v1\nattempt={}\nrun={}\nintent={}\nsequence={}\nprefix={}\nordinal={}\nwire={}\nbinding={}\nepoch={}\ngeneration={}\nstudy={}\nno_device_receipt=true",
            self.attempt.as_str(),
            checkpoint.run_id(),
            checkpoint.intent(),
            checkpoint.sequence(),
            checkpoint.prefix_sha256(),
            action.ordinal,
            action.wire_sha256,
            self.binding,
            self.epoch.0,
            self.generation,
            study_sha256,
        );
        // publish fsyncs both file and directory and independently rereads exact
        // bytes before the genuine worker offer fence can be consumed.
        self.publish(1, body.as_bytes())?;
        self.entry_run = Some(checkpoint.run_id().into());
        self.entry_next += 1;
        Ok(())
    }
    pub(crate) fn offered(&mut self) {
        self.offered = true;
    }
    pub(crate) fn retain_failure(
        &mut self,
        raw: &[FirmwareLifecycleRawObservation],
        send: Option<&FirmwareLifecycleSendObservation>,
        frontier: u64,
        note: &str,
    ) -> Result<(), ServiceError> {
        if raw.len() > 192 {
            return Err(bad("failure raw retention cap"));
        }
        for r in raw {
            let bytes = FirmwareLifecycleNegativeObservation {
                raw: r.clone(),
                note: note.to_owned(),
            }
            .retained_json()?;
            self.publish(4, &bytes)?;
        }
        if let Some(s) = send {
            self.publish(4, format!("exact host send observation: {s:?}").as_bytes())?;
        }
        self.publish(4,format!("host failure: {note}; allocated raw frontier {frontier}; no physical clear or resume").as_bytes())
    }
    pub(crate) fn zero_offer(&mut self) -> Result<(), ServiceError> {
        if self.offered {
            return Err(bad("a driver call occurred"));
        }
        self.publish(2, b"creating owner observed zero driver calls")
    }
    #[cfg(feature = "firmware-linux-completion")]
    pub(crate) fn complete_linux(
        &mut self,
        witness: crate::firmware_linux_completion::LiveCompletionClearance,
    ) -> Result<crate::device_session_guard::DeviceSessionGuard, ServiceError> {
        if !self.offered {
            return Err(bad(
                "Linux completion cannot reinterpret a zero-offer owner",
            ));
        }
        let (guard, record) = witness.consume(&self.attempt)?;
        let bytes = serde_json::to_vec(&record).map_err(bad)?;
        self.publish(5, &bytes)?;
        Ok(guard)
    }
    #[cfg(feature = "firmware-linux-completion")]
    pub(crate) fn complete_recovery(
        &mut self,
        witness: crate::firmware_linux_completion::LiveRecoveryCompletionClearance<'_>,
    ) -> Result<crate::device_session_guard::DeviceSessionGuard, ServiceError> {
        use std::os::unix::fs::MetadataExt;
        #[cfg(test)]
        let mock_allowed = self.mock && witness.is_mock_origin();
        #[cfg(not(test))]
        let mock_allowed = false;
        if (self.mock && !mock_allowed) || !self.offered {
            return Err(bad(
                "Physical linked recovery closure needs this offered actual owner",
            ));
        }
        let uid = rustix::process::geteuid().as_raw();
        let registry =
            QuarantineRegistry::at(self.directory.try_clone().map_err(bad)?, uid, self.mock)?;
        let (guard, refs, record, journal) = witness.consume(&self.attempt, &registry.directory)?;
        let mut predecessors = refs
            .iter()
            .map(|r| LinkedPredecessor {
                attempt: r.attempt.clone(),
                before_bytes: r.registry.bytes,
                before_sha: r.registry.sha256.clone(),
                device: r.registry.device,
                inode: r.registry.inode,
                after_bytes: 0,
                after_sha: String::new(),
            })
            .collect::<Vec<_>>();
        if predecessors.is_empty() || predecessors.len() > 64 {
            return Err(bad("Live recovery predecessor set missing/excessive"));
        }
        predecessors.sort_by(|a, b| a.attempt.cmp(&b.attempt));
        let set = JobSha256::of(set_description(&predecessors).as_bytes()).hex();
        // Every append preserves the original named regular file. A crash or any
        // failed sync/reread leaves state6 pending and ordinary admission blocked.
        for p in &mut predecessors {
            let name = format!("{}.quarantine", p.attempt.as_str());
            let mut file: File =
                fs::openat(&registry.directory, name.as_str(), FILE, Mode::empty())
                    .map_err(bad)?
                    .into();
            valid(&file, uid, true, false)?;
            stable_prefix(
                &mut file,
                p.before_bytes,
                &p.before_sha,
                p.device,
                p.inode,
                true,
            )?;
            let (state, records) = read_history(&mut file, false)?;
            if !matches!(state, 0 | 1 | 4 | 6) {
                return Err(bad("Live recovery predecessor is no longer unresolved"));
            }
            let mut member = QuarantineAttempt {
                file,
                directory: registry.directory.try_clone().map_err(bad)?,
                records,
                bytes: p.before_bytes,
                mock: false,
                offered: true,
                attempt: p.attempt.clone(),
                binding: String::new(),
                epoch: SessionEpoch(1),
                generation: 1,
                entry_next: 0,
                entry_run: None,
            };
            let body = format!(
                "schema=1\nkind=linked-recovery-predecessor\nsuccessor={}\nset={}\noriginal_bytes={}\noriginal_sha256={}\noriginal_device={}\noriginal_inode={}\nno_physical_authority=true",
                self.attempt.as_str(),
                set,
                p.before_bytes,
                p.before_sha,
                p.device,
                p.inode
            );
            validate_linked_record(6, body.as_bytes())?;
            member.publish(6, body.as_bytes())?;
            let mut named: File =
                fs::openat(&registry.directory, name.as_str(), FILE, Mode::empty())
                    .map_err(bad)?
                    .into();
            valid(&named, uid, true, false)?;
            let metadata = named.metadata().map_err(bad)?;
            if metadata.dev() != p.device
                || metadata.ino() != p.inode
                || metadata.len() != member.bytes
            {
                return Err(bad("Linked predecessor name changed after durable append"));
            }
            named.seek(SeekFrom::Start(0)).map_err(bad)?;
            let mut bytes = Vec::new();
            named.read_to_end(&mut bytes).map_err(bad)?;
            p.after_bytes = metadata.len();
            p.after_sha = JobSha256::of(&bytes).hex();
            stable_prefix(
                &mut named,
                p.before_bytes,
                &p.before_sha,
                p.device,
                p.inode,
                false,
            )?;
        }
        journal.revalidate()?;
        let normal = record
            .get("normal")
            .ok_or_else(|| bad("Live normal proof absent"))?;
        let binding: FirmwareUsbObservation = serde_json::from_value(
            normal
                .get("binding")
                .cloned()
                .ok_or_else(|| bad("Live normal binding absent"))?,
        )
        .map_err(bad)?;
        binding.validate()?;
        let string = |v: &serde_json::Value, k: &str| -> Result<String, ServiceError> {
            Ok(v.get(k)
                .and_then(|n| n.as_str())
                .ok_or_else(|| bad(format!("Live closure lacks {k}")))?
                .to_owned())
        };
        let number = |v: &serde_json::Value, k: &str| -> Result<u64, ServiceError> {
            v.get(k)
                .and_then(|n| n.as_u64())
                .ok_or_else(|| bad(format!("Live closure lacks {k}")))
        };
        let mut lines = vec![
            "schema=1".into(),
            format!(
                "kind={}",
                if self.mock {
                    "linked-recovery-mock-commit"
                } else {
                    "linked-recovery-commit"
                }
            ),
            format!("successor={}", self.attempt.as_str()),
            format!("set={set}"),
            format!("source_review={}", string(&record, "source_review")?),
            format!("image={}", string(&record, "image_sha256")?),
            format!("journal={}", journal.path.display()),
            format!("journal_bytes={}", journal.bytes),
            format!("journal_prefix_sha256={}", journal.prefix_sha256),
            format!("journal_sequence={}", journal.sequence),
            format!("normal_epoch={}", number(normal, "epoch")?),
            format!("normal_binding_sha256={}", binding.digest()),
            format!("identity_sha256={}", string(normal, "identity_sha256")?),
            format!("current_sha256={}", string(normal, "current_sha256")?),
            format!("globals_sha256={}", string(normal, "globals_sha256")?),
            format!("raw_allocated={}", number(normal, "allocated")?),
            format!("raw_processed={}", number(normal, "processed")?),
            "reader_joined=true".into(),
            "output_destroyed=true".into(),
            format!(
                "manual_phase_review={}",
                string(&record, "manual_phase_review")?
            ),
            "manual_application_version=V1.8.2".into(),
            "contents_verified=false".into(),
            format!("count={}", predecessors.len()),
        ];
        for p in &predecessors {
            lines.push(format!(
                "predecessor={},{},{},{},{},{},{}",
                p.attempt.as_str(),
                p.before_bytes,
                p.before_sha,
                p.device,
                p.inode,
                p.after_bytes,
                p.after_sha
            ));
        }
        let body = lines.join("\n");
        validate_linked_record(7, body.as_bytes())?;
        self.publish(7, body.as_bytes())?;
        if !linked_attempt_closed(&registry.directory, uid, &self.attempt, self.mock)? {
            return Err(bad("Linked live recovery closure incomplete"));
        }
        registry.admit_normal()?;
        Ok(guard)
    }
    pub(crate) fn mock_finish(&mut self) -> Result<(), ServiceError> {
        if !self.mock {
            return Err(bad("physical clear issuer unavailable"));
        }
        self.publish(3, b"mock only; no verified flash or physical clearance")
    }
}
use sha2::Digest;
fn read_history(file: &mut File, allow_mock: bool) -> Result<(u8, u32), ServiceError> {
    file.seek(SeekFrom::Start(0)).map_err(bad)?;
    let length = file.metadata().map_err(bad)?.len();
    if length == 0 || length > MAX_FILE {
        return Err(bad("missing or excessive quarantine history"));
    }
    let mut offset = 0;
    let mut count = 0;
    let mut state = 255;
    while offset < length {
        let mut h = [0u8; 44];
        file.read_exact(&mut h).map_err(bad)?;
        let n = u32::from_le_bytes(h[8..12].try_into().expect("four bytes")) as usize;
        if &h[..8] != b"GP2QUAR1"
            || n == 0
            || n > 16 * 1024
            || offset + 44 + n as u64 > length
            || count >= 256
        {
            return Err(bad("invalid/partial quarantine frame"));
        }
        let mut body = vec![0; n];
        file.read_exact(&mut body).map_err(bad)?;
        if sha2::Sha256::digest(&body).as_slice() != &h[12..] || body[0] > 7 {
            return Err(bad("quarantine checksum/schema differs"));
        }
        if count == 0 {
            if body[0] != 0 {
                return Err(bad("quarantine lacks reservation"));
            }
            validate_reservation(&body[1..], allow_mock)?;
        }
        if count != 0 && body[0] == 0 {
            return Err(bad("quarantine repeats reservation"));
        }
        if matches!(state, 2 | 3 | 5 | 7) {
            return Err(bad("quarantine history follows closure"));
        }
        if matches!(body[0], 6 | 7) {
            validate_linked_record(body[0], &body[1..])?;
        }
        state = body[0];
        count += 1;
        offset += 44 + n as u64;
    }
    Ok((state, count))
}
fn validate_reservation(body: &[u8], allow_mock: bool) -> Result<(), ServiceError> {
    let text = std::str::from_utf8(body).map_err(bad)?;
    let lines = text.lines().collect::<Vec<_>>();
    let keys = [
        "schema=",
        "origin=",
        "attempt=",
        "review=",
        "profile=",
        "binding=",
        "generation=",
        "clock=",
        "image=",
        "journal=",
        "updater_epoch=",
        "no_physical_authority=",
    ];
    if lines.len() != keys.len() {
        return Err(bad("unknown reservation schema"));
    }
    let values = lines
        .iter()
        .zip(keys)
        .map(|(line, key)| {
            line.strip_prefix(key)
                .ok_or_else(|| bad("unknown reservation field"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if values[0] != "1"
        || !matches!(values[1], "LocalObservation" | "MockSimulation")
        || values[11] != "true"
    {
        return Err(bad("unknown reservation schema/origin/authority claim"));
    }
    if values[1] == "MockSimulation" && !allow_mock {
        return Err(bad(
            "Mock reservation cannot enter the physical admission namespace",
        ));
    }
    FirmwareAttemptId::parse(values[2])?;
    for value in [values[3], values[4], values[5], values[8]] {
        if JobSha256::parse(value)?.hex() != value {
            return Err(bad("noncanonical reservation digest"));
        }
    }
    if values[6].parse::<u64>().map_err(bad)? == 0
        || values[10].parse::<u64>().map_err(bad)? == 0
        || values[7].is_empty()
        || values[7].len() > 128
        || !Path::new(values[9]).is_absolute()
        || values[9].len() > 4096
        || text.chars().any(|c| c.is_control() && c != '\n')
    {
        return Err(bad("invalid reservation source/reference"));
    }
    Ok(())
}
// Linked closures are readable in the default build. No imported record can
// construct the consuming live owner witness required to publish them.
#[derive(Clone)]
struct LinkedPredecessor {
    attempt: FirmwareAttemptId,
    before_bytes: u64,
    before_sha: String,
    device: u64,
    inode: u64,
    after_bytes: u64,
    after_sha: String,
}
struct LinkedCommit {
    mock: bool,
    successor: FirmwareAttemptId,
    set: String,
    source_review: String,
    journal: std::path::PathBuf,
    journal_bytes: u64,
    journal_sha: String,
    journal_sequence: u64,
    predecessors: Vec<LinkedPredecessor>,
}
fn canonical_u64(value: &str) -> Result<u64, ServiceError> {
    let n = value.parse::<u64>().map_err(bad)?;
    if n.to_string() != value {
        return Err(bad("Noncanonical linked integer"));
    }
    Ok(n)
}
fn linked_sha(value: &str) -> Result<String, ServiceError> {
    if JobSha256::parse(value)?.hex() != value {
        return Err(bad("Noncanonical linked digest"));
    }
    Ok(value.into())
}
fn linked_lines<'a>(body: &'a [u8], keys: &[&str]) -> Result<Vec<&'a str>, ServiceError> {
    let text = std::str::from_utf8(body).map_err(bad)?;
    if text.chars().any(|c| c.is_control() && c != '\n') || text.ends_with('\n') {
        return Err(bad("Invalid linked closure control bytes"));
    }
    let lines = text.split('\n').collect::<Vec<_>>();
    if lines.len() < keys.len() {
        return Err(bad("Missing linked field"));
    }
    let mut values = Vec::new();
    for (line, key) in lines.iter().zip(keys) {
        values.push(
            line.strip_prefix(key)
                .ok_or_else(|| bad("Unknown/reordered linked field"))?,
        );
    }
    Ok(values)
}
fn parse_link_predecessor(
    body: &[u8],
) -> Result<(FirmwareAttemptId, String, u64, String, u64, u64), ServiceError> {
    let keys = [
        "schema=",
        "kind=",
        "successor=",
        "set=",
        "original_bytes=",
        "original_sha256=",
        "original_device=",
        "original_inode=",
        "no_physical_authority=",
    ];
    let v = linked_lines(body, &keys)?;
    if body.split(|b| *b == b'\n').count() != keys.len()
        || v[0] != "1"
        || v[1] != "linked-recovery-predecessor"
        || v[8] != "true"
    {
        return Err(bad("Unknown predecessor closure schema"));
    }
    let id = FirmwareAttemptId::parse(v[2])?;
    if id.as_str() != v[2] {
        return Err(bad("Noncanonical linked successor"));
    }
    let bytes = canonical_u64(v[4])?;
    let device = canonical_u64(v[6])?;
    let inode = canonical_u64(v[7])?;
    if bytes == 0 || bytes > MAX_FILE || inode == 0 {
        return Err(bad("Linked predecessor extent/identity invalid"));
    }
    Ok((
        id,
        linked_sha(v[3])?,
        bytes,
        linked_sha(v[5])?,
        device,
        inode,
    ))
}
const COMMIT_KEYS: [&str; 23] = [
    "schema=",
    "kind=",
    "successor=",
    "set=",
    "source_review=",
    "image=",
    "journal=",
    "journal_bytes=",
    "journal_prefix_sha256=",
    "journal_sequence=",
    "normal_epoch=",
    "normal_binding_sha256=",
    "identity_sha256=",
    "current_sha256=",
    "globals_sha256=",
    "raw_allocated=",
    "raw_processed=",
    "reader_joined=",
    "output_destroyed=",
    "manual_phase_review=",
    "manual_application_version=",
    "contents_verified=",
    "count=",
];
fn set_description(predecessors: &[LinkedPredecessor]) -> String {
    predecessors
        .iter()
        .map(|p| {
            format!(
                "{},{},{},{},{}",
                p.attempt.as_str(),
                p.before_bytes,
                p.before_sha,
                p.device,
                p.inode
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn parse_link_commit(body: &[u8]) -> Result<LinkedCommit, ServiceError> {
    let v = linked_lines(body, &COMMIT_KEYS)?;
    let count = usize::try_from(canonical_u64(v[22])?).map_err(bad)?;
    if v[0] != "1"
        || !matches!(
            v[1],
            "linked-recovery-commit" | "linked-recovery-mock-commit"
        )
        || v[5] != "c368618773097b07c556c951eb10792f9d57be929323366648eb6e083d0d23af"
        || v[17] != "true"
        || v[18] != "true"
        || v[20] != "V1.8.2"
        || v[21] != "false"
        || count == 0
        || count > 64
        || body.split(|b| *b == b'\n').count() != COMMIT_KEYS.len() + count
    {
        return Err(bad("Linked commit schema/image/verification differs"));
    }
    // The image literal below is checked against the producer constant whenever
    // the feature is enabled; default admission uses the same exact image pin.
    let successor = FirmwareAttemptId::parse(v[2])?;
    if successor.as_str() != v[2] {
        return Err(bad("Noncanonical committed successor"));
    }
    let set = linked_sha(v[3])?;
    for index in [4, 8, 11, 12, 13, 14, 19] {
        linked_sha(v[index])?;
    }
    let journal = Path::new(v[6]);
    if !journal.is_absolute()
        || v[6].len() > 4096
        || journal
            .components()
            .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
        || journal.file_name().and_then(|n| n.to_str())
            != Some(&format!("{}.journal", successor.as_str()))
    {
        return Err(bad("Linked journal path differs"));
    }
    let journal_bytes = canonical_u64(v[7])?;
    let journal_sequence = canonical_u64(v[9])?;
    if journal_bytes == 0
        || journal_bytes > FIRMWARE_JOURNAL_LIMIT
        || !(2..=FIRMWARE_FRAME_LIMIT).contains(&journal_sequence)
        || canonical_u64(v[10])? == 0
        || canonical_u64(v[15])? != canonical_u64(v[16])?
    {
        return Err(bad("Linked normal/frontier/journal limits differ"));
    }
    let text = std::str::from_utf8(body).map_err(bad)?;
    let mut predecessors = Vec::new();
    for line in text.lines().skip(COMMIT_KEYS.len()) {
        let fields = line
            .strip_prefix("predecessor=")
            .ok_or_else(|| bad("Unknown linked member"))?
            .split(',')
            .collect::<Vec<_>>();
        if fields.len() != 7 {
            return Err(bad("Linked member extent differs"));
        }
        let attempt = FirmwareAttemptId::parse(fields[0])?;
        if attempt.as_str() != fields[0]
            || attempt == successor
            || predecessors
                .last()
                .is_some_and(|p: &LinkedPredecessor| p.attempt >= attempt)
        {
            return Err(bad("Linked members repeated, unsorted or self-referential"));
        }
        let p = LinkedPredecessor {
            attempt,
            before_bytes: canonical_u64(fields[1])?,
            before_sha: linked_sha(fields[2])?,
            device: canonical_u64(fields[3])?,
            inode: canonical_u64(fields[4])?,
            after_bytes: canonical_u64(fields[5])?,
            after_sha: linked_sha(fields[6])?,
        };
        if p.before_bytes == 0
            || p.before_bytes >= p.after_bytes
            || p.after_bytes > MAX_FILE
            || p.inode == 0
        {
            return Err(bad(
                "Linked member does not preserve a proper original prefix",
            ));
        }
        predecessors.push(p);
    }
    if predecessors.len() != count
        || JobSha256::of(set_description(&predecessors).as_bytes()).hex() != set
    {
        return Err(bad("Linked complete set digest differs"));
    }
    Ok(LinkedCommit {
        mock: v[1] == "linked-recovery-mock-commit",
        successor,
        set,
        source_review: v[4].into(),
        journal: journal.into(),
        journal_bytes,
        journal_sha: v[8].into(),
        journal_sequence,
        predecessors,
    })
}
pub(crate) fn validate_linked_record(state: u8, body: &[u8]) -> Result<(), ServiceError> {
    match state {
        6 => {
            parse_link_predecessor(body)?;
        }
        7 => {
            parse_link_commit(body)?;
        }
        _ => return Err(bad("Unknown linked state")),
    }
    Ok(())
}
fn last_payload(file: &mut File) -> Result<(u8, Vec<u8>), ServiceError> {
    file.seek(SeekFrom::Start(0)).map_err(bad)?;
    read_history(file, false)?;
    file.seek(SeekFrom::Start(0)).map_err(bad)?;
    let mut last = None;
    let length = file.metadata().map_err(bad)?.len();
    let mut offset = 0;
    while offset < length {
        let mut h = [0; 44];
        file.read_exact(&mut h).map_err(bad)?;
        let n = u32::from_le_bytes(h[8..12].try_into().expect("length")) as usize;
        let mut payload = vec![0; n];
        file.read_exact(&mut payload).map_err(bad)?;
        last = Some((payload[0], payload[1..].to_vec()));
        offset += 44 + n as u64;
    }
    last.ok_or_else(|| bad("Linked attempt empty"))
}
fn stable_prefix(
    file: &mut File,
    bytes: u64,
    sha: &str,
    device: u64,
    inode: u64,
    exact: bool,
) -> Result<(), ServiceError> {
    use std::os::unix::fs::MetadataExt;
    let before = file.metadata().map_err(bad)?;
    if before.dev() != device
        || before.ino() != inode
        || before.len() < bytes
        || (exact && before.len() != bytes)
    {
        return Err(bad("Linked exact file identity/extent differs"));
    }
    file.seek(SeekFrom::Start(0)).map_err(bad)?;
    let mut h = sha2::Sha256::new();
    let mut left = bytes;
    let mut buffer = [0; 65536];
    while left > 0 {
        let n = left.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..n]).map_err(bad)?;
        h.update(&buffer[..n]);
        left -= n as u64;
    }
    let after = file.metadata().map_err(bad)?;
    if format!("{:x}", h.finalize()) != sha
        || before.len() != after.len()
        || before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
    {
        return Err(bad("Linked retained prefix changed during checking"));
    }
    Ok(())
}
fn open_linked_directory(path: &Path, uid: u32) -> Result<File, ServiceError> {
    if !path.is_absolute() {
        return Err(bad("Linked path is relative"));
    }
    let mut directory: File = fs::open("/", DIR, Mode::empty()).map_err(bad)?.into();
    for part in path.components() {
        let name = match part {
            Component::RootDir => continue,
            Component::Normal(n) => n,
            _ => return Err(bad("Linked path components differ")),
        };
        directory = fs::openat(&directory, name, DIR, Mode::empty())
            .map_err(bad)?
            .into();
        let stat = fs::fstat(&directory).map_err(bad)?;
        if !(stat.st_uid == 0 && stat.st_mode & 0o1777 == 0o1777) {
            valid(&directory, uid, false, true)?;
        }
    }
    valid(&directory, uid, true, true)?;
    Ok(directory)
}
fn check_linked_journal(commit: &LinkedCommit, uid: u32) -> Result<(), ServiceError> {
    use std::os::unix::fs::MetadataExt;
    let parent = commit
        .journal
        .parent()
        .ok_or_else(|| bad("Linked journal parent absent"))?;
    let directory = open_linked_directory(parent, uid)?;
    let name = commit
        .journal
        .file_name()
        .ok_or_else(|| bad("Linked journal name absent"))?;
    let mut file: File = fs::openat(&directory, name, FILE, Mode::empty())
        .map_err(bad)?
        .into();
    valid(&file, uid, true, false)?;
    let before = file.metadata().map_err(bad)?;
    stable_prefix(
        &mut file,
        commit.journal_bytes,
        &commit.journal_sha,
        before.dev(),
        before.ino(),
        true,
    )?;
    file.seek(SeekFrom::Start(0)).map_err(bad)?;
    let mut offset = 0;
    let mut count = 0;
    while offset < commit.journal_bytes {
        let mut header = [0; 44];
        file.read_exact(&mut header).map_err(bad)?;
        let n = u32::from_le_bytes(header[8..12].try_into().expect("length")) as usize;
        let bound = if count == 0 {
            32 * 1024 * 1024
        } else {
            512 * 1024
        };
        if &header[..8] != b"GP2LCJ01"
            || n == 0
            || n > bound
            || offset + 44 + n as u64 > commit.journal_bytes
            || count >= FIRMWARE_FRAME_LIMIT
        {
            return Err(bad("Linked journal framing/budget differs"));
        }
        let mut payload = vec![0; n];
        file.read_exact(&mut payload).map_err(bad)?;
        if sha2::Sha256::digest(&payload).as_slice() != &header[12..] {
            return Err(bad("Linked journal checksum differs"));
        }
        offset += 44 + n as u64;
        count += 1;
    }
    if count != commit.journal_sequence {
        return Err(bad("Linked durable journal sequence differs"));
    }
    let mut named: File = fs::openat(&directory, name, FILE, Mode::empty())
        .map_err(bad)?
        .into();
    valid(&named, uid, true, false)?;
    stable_prefix(
        &mut named,
        commit.journal_bytes,
        &commit.journal_sha,
        before.dev(),
        before.ino(),
        true,
    )
}
fn linked_reservation(file: &mut File) -> Result<Vec<String>, ServiceError> {
    file.seek(SeekFrom::Start(0)).map_err(bad)?;
    read_history(file, false)?;
    file.seek(SeekFrom::Start(0)).map_err(bad)?;
    let mut header = [0; 44];
    file.read_exact(&mut header).map_err(bad)?;
    let n = u32::from_le_bytes(header[8..12].try_into().expect("length")) as usize;
    let mut body = vec![0; n];
    file.read_exact(&mut body).map_err(bad)?;
    validate_reservation(&body[1..], false)?;
    Ok(std::str::from_utf8(&body[1..])
        .map_err(bad)?
        .lines()
        .map(|l| {
            l.split_once('=')
                .expect("validated reservation")
                .1
                .to_owned()
        })
        .collect())
}
pub(crate) fn linked_attempt_closed(
    directory: &File,
    uid: u32,
    attempt: &FirmwareAttemptId,
    allow_mock: bool,
) -> Result<bool, ServiceError> {
    // st_dev is a kernel mount identifier, not a persistent filesystem ID.
    // Btrfs can renumber it on reboot. For already committed history, bind each
    // retained inode to the currently opened registry filesystem instead. Keep
    // the recorded device values in the complete-set digest and cross-record
    // checks below, and still check exact bytes, hashes, permissions and inode.
    // Live recovery publication continues to require its original dev/ino pins.
    let current_device = fs::fstat(directory).map_err(bad)?.st_dev;
    let mut file: File = fs::openat(
        directory,
        format!("{}.quarantine", attempt.as_str()).as_str(),
        FILE,
        Mode::empty(),
    )
    .map_err(bad)?
    .into();
    valid(&file, uid, true, false)?;
    let (state, payload) = last_payload(&mut file)?;
    let reservation = linked_reservation(&mut file)?;
    if reservation[2] != attempt.as_str() {
        return Err(bad("Linked filename and reservation differ"));
    }
    let successor = match state {
        6 => parse_link_predecessor(&payload)?.0,
        7 => parse_link_commit(&payload)?.successor,
        _ => return Ok(false),
    };
    let mut owner: File = fs::openat(
        directory,
        format!("{}.quarantine", successor.as_str()).as_str(),
        FILE,
        Mode::empty(),
    )
    .map_err(bad)?
    .into();
    valid(&owner, uid, true, false)?;
    let (owner_state, body) = last_payload(&mut owner)?;
    if owner_state != 7 {
        return Ok(false);
    }
    let commit = parse_link_commit(&body)?;
    if commit.mock && !allow_mock {
        return Err(bad("Mock linked closure cannot enter physical admission"));
    }
    let owner_reservation = linked_reservation(&mut owner)?;
    if owner_reservation[2] != successor.as_str()
        || owner_reservation[3] != commit.source_review
        || owner_reservation[8]
            != "c368618773097b07c556c951eb10792f9d57be929323366648eb6e083d0d23af"
        || Path::new(&owner_reservation[9]) != commit.journal
    {
        return Err(bad("Linked commit and creating reservation differ"));
    }
    if commit.successor != successor
        || (state == 7 && &successor != attempt)
        || (state == 6 && !commit.predecessors.iter().any(|p| &p.attempt == attempt))
    {
        return Err(bad(
            "Linked predecessor is absent from committed complete set",
        ));
    }
    for p in &commit.predecessors {
        let mut member: File = fs::openat(
            directory,
            format!("{}.quarantine", p.attempt.as_str()).as_str(),
            FILE,
            Mode::empty(),
        )
        .map_err(bad)?
        .into();
        valid(&member, uid, true, false)?;
        let original_reservation = linked_reservation(&mut member)?;
        if original_reservation[2] != p.attempt.as_str() {
            return Err(bad("Linked original filename/reservation differs"));
        }
        stable_prefix(
            &mut member,
            p.after_bytes,
            &p.after_sha,
            current_device,
            p.inode,
            true,
        )?;
        stable_prefix(
            &mut member,
            p.before_bytes,
            &p.before_sha,
            current_device,
            p.inode,
            false,
        )?;
        let (state, body) = last_payload(&mut member)?;
        if state != 6 {
            return Err(bad("Linked member lacks pending closure"));
        }
        let (id, set, bytes, sha, device, inode) = parse_link_predecessor(&body)?;
        if id != successor
            || set != commit.set
            || bytes != p.before_bytes
            || sha != p.before_sha
            || device != p.device
            || inode != p.inode
        {
            return Err(bad("Linked member original prefix/set differs"));
        }
    }
    check_linked_journal(&commit, uid)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_account_parser_rejects_nss_duplicate_malformed_and_relative() {
        assert!(passwd_home(b"root:x:0:0::/root:/bin/sh\n", 1000).is_err());
        assert!(
            passwd_home(
                b"a:x:1000:1000::/home/a:/bin/sh\nb:x:1000:1000::/home/b:/bin/sh\n",
                1000
            )
            .is_err()
        );
        assert!(passwd_home(b"a:x:1000:1000::relative:/bin/sh\n", 1000).is_err());
        assert!(passwd_home(b"invalid", 1000).is_err());
        assert_eq!(
            passwd_home(b"a:x:1000:1000::/home/a:/bin/sh\n", 1000).unwrap(),
            Path::new("/home/a")
        );
    }
}

#[cfg(test)]
mod persistent_tests {
    use super::*;
    use std::{
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicU64, Ordering},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "gp200-fw03-registry-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir(&p).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self(p)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn binding() -> FirmwareUsbObservation {
        FirmwareUsbObservation {
            topology: "private topology".into(),
            vendor: 0x84ef,
            product: 0x29,
            serial: "fixture serial".into(),
            interface: "fixture interface".into(),
            endpoint: "fixture endpoint".into(),
        }
    }
    fn plan() -> FirmwareLifecyclePlanDto {
        FirmwareLifecyclePlanDto {
            review_sha256: "00".repeat(32),
            profile_sha256: "11".repeat(32),
            origin: FirmwareObservationOrigin::MockSimulation,
            nonblock_actions: 17,
            epoch: SessionEpoch(21),
            binding: binding(),
            generation: 1,
            clock_origin: "private registry clock".into(),
        }
    }
    fn reserve(r: &QuarantineRegistry) -> QuarantineAttempt {
        r.reserve(
            &FirmwareAttemptId::parse(&"a".repeat(32)).unwrap(),
            &plan(),
            &"22".repeat(32),
            "/private/test-only-library/firmware-attempt-ledger",
        )
        .unwrap()
    }
    #[test]
    fn live_prefix_validation_still_requires_the_original_device_and_inode() {
        use std::os::unix::fs::MetadataExt;
        let t = Temp::new();
        let path = t.0.join("live-prefix");
        let bytes = b"live recovery evidence";
        std::fs::write(&path, bytes).unwrap();
        let mut file = File::open(path).unwrap();
        let meta = file.metadata().unwrap();
        let sha = JobSha256::of(bytes).hex();
        stable_prefix(
            &mut file,
            bytes.len() as u64,
            &sha,
            meta.dev(),
            meta.ino(),
            true,
        )
        .unwrap();
        assert!(
            stable_prefix(
                &mut file,
                bytes.len() as u64,
                &sha,
                meta.dev() ^ 1,
                meta.ino(),
                true
            )
            .is_err()
        );
        assert!(
            stable_prefix(
                &mut file,
                bytes.len() as u64,
                &sha,
                meta.dev(),
                meta.ino() ^ 1,
                true
            )
            .is_err()
        );
    }
    #[test]
    fn permanent_physical_keys_bind_topology_and_serial_across_endpoint_aliases() {
        let t = Temp::new();
        let r = QuarantineRegistry::mock(&t.0).unwrap();
        let held = r.physical_lease(&binding()).unwrap();
        let mut alias = binding();
        alias.endpoint = "renumbered endpoint".into();
        alias.interface = "boot test interface".into();
        alias.product = 0x2a;
        assert!(r.physical_lease(&alias).is_err());
        alias.topology = "different topology".into();
        assert!(r.physical_lease(&alias).is_err());
        alias.serial = "different serial".into();
        let other = r.physical_lease(&alias).unwrap();
        drop(other);
        drop(held);
        r.physical_lease(&binding()).unwrap();
        r.admit_normal().unwrap();
    }
    #[test]
    fn unresolved_reservation_survives_owner_drop_and_only_live_zero_offer_can_close() {
        let t = Temp::new();
        {
            let r = QuarantineRegistry::mock(&t.0).unwrap();
            let mut q = reserve(&r);
            q.arm().unwrap();
            assert!(r.admit_normal().is_err());
            q.offered();
            assert!(q.zero_offer().is_err());
        }
        let r = QuarantineRegistry::mock(&t.0).unwrap();
        assert!(r.admit_normal().is_err());
        drop(r);
        let t = Temp::new();
        let r = QuarantineRegistry::mock(&t.0).unwrap();
        let mut q = reserve(&r);
        q.arm().unwrap();
        q.zero_offer().unwrap();
        r.admit_normal().unwrap();
    }
    #[test]
    fn partial_checksum_unknown_tail_symlink_and_hardlink_all_block_admission() {
        for mutation in 0..5 {
            let t = Temp::new();
            let r = QuarantineRegistry::mock(&t.0).unwrap();
            let mut q = reserve(&r);
            q.zero_offer().unwrap();
            drop(q);
            let p = t.0.join(format!("{}.quarantine", "a".repeat(32)));
            match mutation {
                0 => {
                    let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
                    f.write_all(b"partial").unwrap();
                }
                1 => {
                    let mut bytes = std::fs::read(&p).unwrap();
                    bytes[12] ^= 1;
                    std::fs::write(&p, bytes).unwrap();
                }
                2 => {
                    std::fs::write(t.0.join("unrecognized.pending"), b"retained partial").unwrap();
                }
                3 => {
                    std::fs::remove_file(&p).unwrap();
                    std::os::unix::fs::symlink("missing", &p).unwrap();
                }
                _ => {
                    std::fs::hard_link(&p, t.0.join("another.quarantine")).unwrap();
                }
            }
            assert!(
                r.admit_normal().is_err(),
                "unsafe mutation {mutation} admitted"
            );
        }
    }
    #[test]
    fn physical_lock_lookalikes_and_unsafe_real_lock_files_are_not_skipped() {
        for mutation in 0..3 {
            let t = Temp::new();
            let r = QuarantineRegistry::mock(&t.0).unwrap();
            let name = if mutation == 0 {
                "physical-forged.lock".into()
            } else {
                format!("physical-{}.lock", "a".repeat(64))
            };
            let path = t.0.join(name);
            if mutation == 1 {
                std::os::unix::fs::symlink("registry.lock", &path).unwrap();
            } else {
                std::fs::write(&path, []).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
            assert!(r.admit_normal().is_err());
        }
    }
    #[test]
    fn maximum_terminal_evidence_respects_existing_byte_and_frame_caps() {
        let mut raw = FirmwareLifecycleRawObservation {
            sequence: u64::MAX,
            at_ns: u64::MAX,
            epoch: SessionEpoch(u64::MAX),
            generation: u64::MAX,
            binding_sha256: "f".repeat(64),
            clock_origin: "\\".repeat(128),
            raw: vec![255; 1024],
        };
        let note = "\\".repeat(512);
        let encoded = FirmwareLifecycleNegativeObservation {
            raw: raw.clone(),
            note: note.clone(),
        }
        .retained_json()
        .unwrap();
        assert!(encoded.len() + 256 <= 8192);
        let t = Temp::new();
        let r = QuarantineRegistry::mock(&t.0).unwrap();
        let mut q = reserve(&r);
        q.arm().unwrap();
        q.offered();
        let batch = vec![raw.clone(); 192];
        let bytes_needed = (encoded.len() + 45) * 192;
        assert!(
            bytes_needed > MAX_FILE as usize,
            "fixture must exercise fallback cap"
        );
        assert!(q.retain_failure(&batch, None, u64::MAX, &note).is_err());
        assert!(q.file.metadata().unwrap().len() <= MAX_FILE);
        assert!(q.records <= 256);
        assert!(r.admit_normal().is_err());
        // A larger serialized record is refused before publication. No claimed
        // exact retention can be inferred merely from its declared field limits.
        raw.clock_origin = "\0".repeat(128);
        assert!(
            FirmwareLifecycleNegativeObservation {
                raw,
                note: "\0".repeat(512)
            }
            .retained_json()
            .is_err()
        );
        let t = Temp::new();
        let r = QuarantineRegistry::mock(&t.0).unwrap();
        let mut q = reserve(&r);
        q.arm().unwrap();
        for _ in 0..254 {
            q.publish(4, b"unresolved").unwrap();
        }
        assert_eq!(q.records, 256);
        assert!(q.publish(4, b"overflow").is_err());
        assert!(r.admit_normal().is_err());
    }
    #[test]
    fn actual_parent_directory_publication_failure_retains_entry_and_blocks_progression() {
        let t = Temp::new();
        let parent: File = fs::open(&t.0, DIR, Mode::empty()).unwrap().into();
        let uid = rustix::process::geteuid().as_raw();
        let mut observed_parent = None;
        let result = child_with_parent_sync(&parent, "published-child", uid, true, |actual| {
            observed_parent = Some(fs::fstat(actual).unwrap().st_ino);
            Err(std::io::Error::other("injected actual parent sync failure"))
        });
        assert!(result.is_err());
        assert_eq!(observed_parent, Some(fs::fstat(&parent).unwrap().st_ino));
        assert!(t.0.join("published-child").is_dir());
        let mut syncs = 0;
        let child = child_with_parent_sync(&parent, "published-child", uid, true, |actual| {
            syncs += 1;
            actual.sync_all()
        })
        .unwrap();
        assert_eq!(syncs, 1);
        // Each further level's real parent must be synced before a usable final
        // registry is returned, rather than relying on its deepest file sync.
        let state = child_with_parent_sync(&child, "state", uid, true, |actual| {
            assert_eq!(
                fs::fstat(actual).unwrap().st_ino,
                fs::fstat(&child).unwrap().st_ino
            );
            actual.sync_all()
        })
        .unwrap();
        let registry = QuarantineRegistry::at(state, uid, true).unwrap();
        registry.admit_normal().unwrap();
        assert!(
            QuarantineRegistry::mock(&t.0)
                .unwrap()
                .admit_normal()
                .is_err()
        );
    }
    #[test]
    fn unknown_reservation_schema_and_origin_cannot_become_normal_admission() {
        for body in [
            b"schema=77\norigin=MockSimulation".as_slice(),
            b"caller supplied descriptive clearance".as_slice(),
        ] {
            let t = Temp::new();
            let r = QuarantineRegistry::mock(&t.0).unwrap();
            let mut q = reserve(&r);
            q.file.set_len(0).unwrap();
            q.bytes = 0;
            q.records = 0;
            q.publish(0, body).unwrap();
            q.publish(2, b"untrusted zero claim").unwrap();
            assert!(r.admit_normal().is_err());
        }
    }
    #[test]
    fn mock_zero_offer_record_is_not_an_issuer_for_the_physical_namespace() {
        let t = Temp::new();
        let mut r = QuarantineRegistry::mock(&t.0).unwrap();
        let mut q = reserve(&r);
        q.zero_offer().unwrap();
        r.admit_normal().unwrap();
        r.mock_root = false;
        assert!(r.admit_normal().is_err());
    }
}
