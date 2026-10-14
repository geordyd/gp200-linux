//! There is deliberately no accepted physical profile in this binary.
use gp200_domain::{ErrorKind, ServiceError};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FirmwarePurpose {
    NormalEntryObservation,
    BootTransfer,
}
enum PhysicalSeal {}
enum ExperimentalEntrySeal {
    #[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
    ReviewedStudy(crate::firmware_entry_study::EntryStudyAuthorization),
}
enum AcceptedBootSeal {}
pub(crate) struct ExperimentalEntryPermit {
    _seal: ExperimentalEntrySeal,
}
#[cfg(all(feature = "firmware-entry-study", target_os = "linux"))]
impl ExperimentalEntryPermit {
    pub(crate) fn from_study(
        authorization: crate::firmware_entry_study::EntryStudyAuthorization,
        job: &gp200_domain::ReviewedFirmwareUpdate,
        handoff: &crate::FirmwareNormalHandoff,
        stop: std::sync::Arc<crate::firmware_service::OwnerStop>,
    ) -> Result<Self, ServiceError> {
        authorization.revalidate()?;
        if !authorization.matches_job(job)
            || authorization.review.reservation.epoch != handoff.normal_epoch
            || authorization.review.reservation.usb != handoff.binding
        {
            return Err(ServiceError::invalid(
                "Actual entry owner differs from exact same-run study",
            ));
        }
        authorization.bind_native_owner(stop)?;
        Ok(Self {
            _seal: ExperimentalEntrySeal::ReviewedStudy(authorization),
        })
    }
    pub(crate) fn study(&self) -> &crate::firmware_entry_study::EntryStudyAuthorization {
        match &self._seal {
            ExperimentalEntrySeal::ReviewedStudy(study) => study,
        }
    }
}
pub(crate) struct AcceptedBootBinding {
    _seal: AcceptedBootSeal,
    pub(crate) binding: gp200_domain::FirmwareUsbObservation,
    pub(crate) epoch: gp200_domain::SessionEpoch,
    pub(crate) generation: u64,
    pub(crate) source_instance: String,
}
pub(crate) fn issue_entry() -> Result<ExperimentalEntryPermit, ServiceError> {
    Err(ServiceError::new(
        ErrorKind::Unverified,
        "Experimental normal entry, exact study approval, backup/loss and recovery are not accepted",
    ))
}
pub(crate) fn issue_boot_binding() -> Result<AcceptedBootBinding, ServiceError> {
    Err(ServiceError::new(
        ErrorKind::Unverified,
        "A fresh accepted boot observation and held boot endpoint are unavailable",
    ))
}
pub(crate) fn admit_purpose(purpose: FirmwarePurpose) -> Result<(), ServiceError> {
    match purpose {
        FirmwarePurpose::NormalEntryObservation => issue_entry().map(|_| ()),
        FirmwarePurpose::BootTransfer => issue_physical().map(|_| ()),
    }
}
// Complete retained raw literals. The short form is deliberately not decoded
// as a generic normal command; its device semantics remain unresolved.
pub(crate) fn entry_candidates() -> [Vec<u8>; 2] {
    [
        vec![
            0xf0, 0x21, 0x25, 0x7e, 0x47, 0x50, 0x2d, 0x32, 0x12, 0x04, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf7,
        ],
        vec![
            0xf0, 0x21, 0x25, 0x7e, 0x47, 0x50, 0x2d, 0x32, 0x12, 0x12, 0x00, 0x20, 0x00, 0xf7,
        ],
    ]
}
pub(crate) fn entry_plan(
    plan: gp200_domain::FirmwareLifecyclePlanDto,
) -> gp200_domain::FirmwareEntryObservationPlanDto {
    let candidates = entry_candidates();
    let study_sha256 =
        gp200_domain::FirmwareEntryObservationPlanDto::digest_for(&plan, &candidates);
    gp200_domain::FirmwareEntryObservationPlanDto {
        plan,
        candidates,
        study_sha256,
    }
}
pub(crate) fn derive_entry(
    entry: &gp200_domain::FirmwareEntryObservationPlanDto,
    ordinal: u64,
) -> Result<gp200_domain::FirmwareLifecycleActionDescription, ServiceError> {
    let wire = usize::try_from(ordinal)
        .ok()
        .and_then(|i| entry.candidates.get(i))
        .ok_or_else(|| ServiceError::invalid("Entry ordinal is outside the two-candidate study"))?
        .clone();
    if entry.candidates != entry_candidates()
        || entry.study_sha256
            != gp200_domain::FirmwareEntryObservationPlanDto::digest_for(
                &entry.plan,
                &entry.candidates,
            )
    {
        return Err(ServiceError::invalid(
            "Private entry literals/order/review changed",
        ));
    }
    Ok(gp200_domain::FirmwareLifecycleActionDescription {
        ordinal,
        kind: gp200_domain::FirmwareLifecycleKind::EntryTransition,
        wire_sha256: gp200_domain::JobSha256::of(&wire).hex(),
        wire,
        block: None,
        epoch: entry.plan.epoch,
        binding_sha256: entry.plan.binding.digest(),
        generation: entry.plan.generation,
        rule_sha256: entry.study_sha256.clone(),
    })
}
pub(crate) struct PhysicalFirmwarePermit {
    _seal: PhysicalSeal,
}
pub(crate) fn issue_physical() -> Result<PhysicalFirmwarePermit, ServiceError> {
    Err(ServiceError::new(
        ErrorKind::Unverified,
        "Firmware entry, boot USB identity, ACK correlation, pacing, finalization, recovery and backup/loss review are not yet accepted",
    ))
}
#[cfg(test)]
pub(crate) struct MockFirmwareAuthority {
    _private: (),
    purpose: FirmwarePurpose,
}
#[cfg(test)]
impl MockFirmwareAuthority {
    pub(crate) fn entry_observation() -> Self {
        Self {
            _private: (),
            purpose: FirmwarePurpose::NormalEntryObservation,
        }
    }
    pub(crate) fn purpose(&self) -> FirmwarePurpose {
        self.purpose
    }
    pub(crate) fn synthetic() -> Self {
        Self {
            _private: (),
            purpose: FirmwarePurpose::BootTransfer,
        }
    }
}

pub(crate) fn derive_physical(
    _permit: &PhysicalFirmwarePermit,
) -> Result<gp200_domain::FirmwareLifecycleActionDescription, ServiceError> {
    Err(ServiceError::new(
        ErrorKind::Unverified,
        "Accepted physical action grammar is unavailable",
    ))
}
#[cfg(test)]
impl MockFirmwareAuthority {
    pub(crate) fn check_rule(&self, digest: &str) -> Result<(), ServiceError> {
        if digest
            == gp200_domain::JobSha256::of(b"private synthetic FW03 absolute action correlation v1")
                .hex()
            || digest
                == gp200_domain::JobSha256::of(b"private synthetic FW03 three-role transition v1")
                    .hex()
        {
            Ok(())
        } else {
            Err(ServiceError::invalid("Private mock profile digest differs"))
        }
    }
}
