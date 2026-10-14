//! Immutable local firmware preparation and observation. No live issuer exists.
use crate::*;
use gp200_protocol::{FirmwareRxEvidence, FirmwareSectionSelector, firmware_block_candidate};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub const FIRMWARE_SOURCE_LIMIT: u64 = 64 * 1024 * 1024;
pub const FIRMWARE_JOURNAL_LIMIT: u64 = 2 * 1024 * 1024 * 1024;
pub const FIRMWARE_FRAME_LIMIT: u64 = 1_000_000;
pub const FIRMWARE_TERMINAL_RESERVE: u64 = 128;
const KIB: u64 = 1024;
const HEADER: u64 = 44;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct FirmwareAttemptId(String);
impl FirmwareAttemptId {
    pub fn parse(value: &str) -> Result<Self, ServiceError> {
        if value.len() != 32
            || !value.bytes().all(|v| v.is_ascii_hexdigit())
            || value.bytes().all(|v| v == b'0')
        {
            return Err(ServiceError::invalid(
                "Firmware attempt ID requires 32 nonzero hexadecimal digits",
            ));
        }
        Ok(Self(value.to_ascii_lowercase()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for FirmwareAttemptId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FirmwareMaterialKind {
    Image,
    Profile,
    Evidence,
    BackupArchive,
    BackupInventory,
    RestorePlan,
    Recovery,
    ApprovalStatement,
    BackupObject(String),
}
#[derive(Debug, Clone)]
pub struct FirmwareMaterialInput {
    pub kind: FirmwareMaterialKind,
    pub bytes: Arc<[u8]>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareMaterialPin {
    pub kind: FirmwareMaterialKind,
    pub sha256: String,
    pub bytes: u64,
}

/// Every field is a recorded claim, including a supplied USB serial and version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareNormalBindingDto {
    pub epoch: SessionEpoch,
    pub revision: Revision,
    pub input: String,
    pub output: String,
    pub usb_binding: String,
    pub identity: RawTransfer,
    pub clock_origin: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirmwareVersionOrigin {
    HumanReported,
    ImageDerived,
    BootloaderReported,
    ClaimedAutomatic,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareVersionClaim {
    pub origin: FirmwareVersionOrigin,
    pub value: String,
    pub raw: Vec<u8>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirmwareBackupCoverage {
    MetadataOnly,
    LocalSource,
    ClaimedDeviceContent,
    Unavailable,
    ClaimedNotApplicable,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareBackupObjectDto {
    pub object: String,
    pub coverage: FirmwareBackupCoverage,
    pub content_sha256: Option<String>,
    pub restore_assessment: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareUpdateContextDto {
    pub profile_id: String,
    pub hardware_policy: String,
    pub update_mode: String,
    pub normal: Option<FirmwareNormalBindingDto>,
    pub versions: Vec<FirmwareVersionClaim>,
    pub backup_objects: Vec<FirmwareBackupObjectDto>,
    pub loss_items: Vec<String>,
    pub power_statement: String,
    pub connection_statement: String,
}
fn text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max
}
impl FirmwareUpdateContextDto {
    fn validate(
        &self,
        model: DeviceModel,
        pins: &[FirmwareMaterialPin],
    ) -> Result<(), ServiceError> {
        if !text(&self.profile_id, 256)
            || !text(&self.hardware_policy, 4096)
            || !text(&self.update_mode, 128)
            || !text(&self.power_statement, 4096)
            || !text(&self.connection_statement, 4096)
            || self.versions.len() > 8
            || self.backup_objects.len() > 288
            || self.loss_items.len() > 288
            || self.loss_items.iter().any(|v| !text(v, 512))
        {
            return Err(ServiceError::invalid(
                "Firmware descriptive context exceeds bounds",
            ));
        }
        if let Some(n) = &self.normal {
            if n.revision.0 == 0
                || !text(&n.input, 4096)
                || !text(&n.output, 4096)
                || !text(&n.usb_binding, 4096)
                || !text(&n.clock_origin, 128)
                || n.identity.payload().len() > 2048
                || n.identity.kind() != gp200_protocol::WRITE_KIND
                || n.identity.records().len() != 1
            {
                return Err(ServiceError::invalid(
                    "Firmware normal binding is missing or unbounded",
                ));
            }
            let parsed = gp200_protocol::Identity::from_transfer(&n.identity).ok_or_else(|| {
                ServiceError::invalid("Firmware binding requires complete raw identity")
            })?;
            let expected = match model {
                DeviceModel::Gp200 => gp200_protocol::WireDeviceModel::Gp200,
                DeviceModel::Gp200Lt => gp200_protocol::WireDeviceModel::Gp200Lt,
                DeviceModel::Gp200Jr => gp200_protocol::WireDeviceModel::Gp200Jr,
                DeviceModel::Unknown(_) => {
                    return Err(ServiceError::unsupported("Unknown firmware device model"));
                }
            };
            if parsed.state_raw != 1 || parsed.model() != expected {
                return Err(ServiceError::invalid(
                    "Firmware image and normal identity differ",
                ));
            }
        }
        if self
            .versions
            .iter()
            .any(|v| !text(&v.value, 256) || v.raw.len() > 512)
        {
            return Err(ServiceError::invalid(
                "Firmware version claims exceed bounds",
            ));
        }
        let mut objects = BTreeSet::new();
        for item in &self.backup_objects {
            if !text(&item.object, 64)
                || !objects.insert(&item.object)
                || !text(&item.restore_assessment, 512)
            {
                return Err(ServiceError::invalid(
                    "Firmware backup object is duplicated or unbounded",
                ));
            }
            if let Some(hash) = &item.content_sha256
                && (JobSha256::parse(hash)?.hex() != *hash
                    || !pins.iter().any(|p| {
                        p.sha256 == *hash
                            && p.kind == FirmwareMaterialKind::BackupObject(item.object.clone())
                    }))
            {
                return Err(ServiceError::invalid(
                    "Firmware backup content claim lacks exact retained material",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareJournalBudget {
    pub blocks: u64,
    pub actions: u64,
    pub callbacks: u64,
    pub frames: u64,
    pub bytes: u64,
}
impl FirmwareJournalBudget {
    pub fn for_blocks(blocks: u64) -> Result<Self, ServiceError> {
        if blocks == 0 || blocks > 1_000_000 {
            return Err(ServiceError::invalid(
                "Firmware plan exceeds the one-million-block application budget",
            ));
        }
        let actions = blocks
            .checked_add(32)
            .ok_or_else(|| ServiceError::invalid("Firmware action count overflow"))?;
        let callbacks = actions * 2 + 64;
        let frames = 1 + actions * 5 + 64 + FIRMWARE_TERMINAL_RESERVE;
        let bytes = 16 * KIB
            + HEADER
            + actions * (15 * KIB + 5 * HEADER)
            + 64 * (4 * KIB + HEADER)
            + FIRMWARE_TERMINAL_RESERVE * (8 * KIB + HEADER);
        if frames > FIRMWARE_FRAME_LIMIT || bytes > FIRMWARE_JOURNAL_LIMIT {
            return Err(ServiceError::invalid(
                "Firmware plan cannot fit its bounded full journal and fault reserve",
            ));
        }
        Ok(Self {
            blocks,
            actions,
            callbacks,
            frames,
            bytes,
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preparation {
    schema: u16,
    context: FirmwareUpdateContextDto,
    pins: Vec<FirmwareMaterialPin>,
    image_sha256: String,
    chunk: u16,
    budget: FirmwareJournalBudget,
}
/// Validates exact material, never physical profile/approval/backup acceptance.
#[derive(Debug, Clone)]
pub struct ReviewedFirmwareUpdate {
    review: FirmwareReview,
    prepared: Preparation,
    materials: BTreeMap<FirmwareMaterialKind, Arc<[u8]>>,
    preimage: Arc<[u8]>,
}
impl ReviewedFirmwareUpdate {
    pub fn prepare(
        source: RetainedJobBlob,
        context: FirmwareUpdateContextDto,
        supplied: Vec<FirmwareMaterialInput>,
        chunk: u16,
    ) -> Result<Self, ServiceError> {
        let source_bytes = source.retained_bytes();
        let review = FirmwareReview::inspect(source, None, &FirmwareBackupAssessment::default())?;
        review
            .summary()
            .image_provenance
            .announcement_candidate()
            .map_err(|e| ServiceError::invalid(e.to_string()))?;
        let counts = review.count_review(chunk)?;
        if counts.sections.iter().any(|s| s.count.discrepancy()) {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Native float and safe integer firmware counts disagree",
            ));
        }
        let blocks = counts
            .sections
            .iter()
            .map(|s| u64::from(s.count.safe_integer_blocks))
            .sum();
        let budget = FirmwareJournalBudget::for_blocks(blocks)?;
        if supplied.len() > 1023 {
            return Err(ServiceError::invalid(
                "Firmware material count exceeds 1024",
            ));
        }
        let mut materials = BTreeMap::from([(FirmwareMaterialKind::Image, source_bytes)]);
        for item in supplied {
            let document = !matches!(
                &item.kind,
                FirmwareMaterialKind::Image
                    | FirmwareMaterialKind::BackupArchive
                    | FirmwareMaterialKind::BackupObject(_)
            );
            if item.bytes.is_empty()
                || item.bytes.len() as u64 > FIRMWARE_SOURCE_LIMIT
                || (document && item.bytes.len() > 2 * 1024 * 1024)
                || matches!(&item.kind, FirmwareMaterialKind::BackupObject(k) if !text(k,64))
                || materials.insert(item.kind, item.bytes).is_some()
            {
                return Err(ServiceError::invalid(
                    "Firmware material is empty, duplicated or over budget",
                ));
            }
        }
        for kind in [
            FirmwareMaterialKind::Profile,
            FirmwareMaterialKind::Evidence,
            FirmwareMaterialKind::BackupArchive,
            FirmwareMaterialKind::BackupInventory,
            FirmwareMaterialKind::RestorePlan,
            FirmwareMaterialKind::Recovery,
            FirmwareMaterialKind::ApprovalStatement,
        ] {
            if !materials.contains_key(&kind) {
                return Err(ServiceError::invalid(
                    "Firmware preparation is missing descriptive review material",
                ));
            }
        }
        let mut unique = BTreeMap::new();
        let pins: Vec<_> = materials
            .iter()
            .map(|(kind, bytes)| {
                let hash = JobSha256::of(bytes).hex();
                unique.insert(hash.clone(), bytes.len() as u64);
                FirmwareMaterialPin {
                    kind: kind.clone(),
                    sha256: hash,
                    bytes: bytes.len() as u64,
                }
            })
            .collect();
        if unique.values().sum::<u64>() > 128 * 1024 * 1024 {
            return Err(ServiceError::invalid(
                "Firmware retained material exceeds 128 MiB",
            ));
        }
        context.validate(review.summary().model, &pins)?;
        for claim in &context.versions {
            if claim.origin == FirmwareVersionOrigin::ImageDerived
                && review.summary().image_version.as_ref() != Some(&claim.value)
            {
                return Err(ServiceError::invalid(
                    "Firmware image version claim differs from exact source",
                ));
            }
        }
        let prepared = Preparation {
            schema: 1,
            context,
            pins,
            image_sha256: review.summary().source_sha256.hex(),
            chunk,
            budget,
        };
        let preimage: Arc<[u8]> = serde_json::to_vec(&prepared)
            .map_err(|e| ServiceError::invalid(e.to_string()))?
            .into();
        if preimage.len() > 2 * 1024 * 1024 {
            return Err(ServiceError::invalid(
                "Firmware review preimage exceeds 2 MiB",
            ));
        }
        Ok(Self {
            review,
            prepared,
            materials,
            preimage,
        })
    }
    pub fn reconstruct(
        preimage: &[u8],
        supplied: Vec<FirmwareMaterialInput>,
    ) -> Result<Self, ServiceError> {
        if preimage.len() > 2 * 1024 * 1024 {
            return Err(ServiceError::invalid(
                "Firmware imported review exceeds bounds",
            ));
        }
        let parsed: Preparation =
            serde_json::from_slice(preimage).map_err(|e| ServiceError::invalid(e.to_string()))?;
        if parsed.schema != 1 || parsed.pins.len() > 1024 {
            return Err(ServiceError::invalid("Unsupported firmware preparation"));
        }
        let image = supplied
            .iter()
            .find(|m| m.kind == FirmwareMaterialKind::Image)
            .ok_or_else(|| ServiceError::invalid("Imported firmware image is absent"))?;
        let source = RetainedJobBlob::checked(
            AssetId("retained-firmware".into()),
            Arc::clone(&image.bytes),
            JobSha256::parse(&parsed.image_sha256)?,
        )?;
        let result = Self::prepare(
            source,
            parsed.context,
            supplied
                .into_iter()
                .filter(|m| m.kind != FirmwareMaterialKind::Image)
                .collect(),
            parsed.chunk,
        )?;
        if result.preimage() != preimage {
            return Err(ServiceError::invalid(
                "Imported firmware preimage differs from independently reconstructed material",
            ));
        }
        Ok(result)
    }
    pub fn retained_pins(preimage: &[u8]) -> Result<Vec<FirmwareMaterialPin>, ServiceError> {
        if preimage.len() > 2 * 1024 * 1024 {
            return Err(ServiceError::invalid("Firmware review exceeds bounds"));
        }
        let value: Preparation =
            serde_json::from_slice(preimage).map_err(|e| ServiceError::invalid(e.to_string()))?;
        if value.schema != 1 || value.pins.len() > 1024 {
            return Err(ServiceError::invalid(
                "Unsupported firmware review schema or material count",
            ));
        }
        // Validate allocation bounds before a loader reads any referenced blob.
        // Roles may alias content, but each hash has exactly one declared size.
        let mut kinds = BTreeSet::new();
        let mut unique = BTreeMap::new();
        for pin in &value.pins {
            let document = !matches!(
                &pin.kind,
                FirmwareMaterialKind::Image
                    | FirmwareMaterialKind::BackupArchive
                    | FirmwareMaterialKind::BackupObject(_)
            );
            if !kinds.insert(&pin.kind)
                || pin.bytes == 0
                || pin.bytes > FIRMWARE_SOURCE_LIMIT
                || (document && pin.bytes > 2 * 1024 * 1024)
                || matches!(&pin.kind, FirmwareMaterialKind::BackupObject(k) if !text(k, 64))
                || JobSha256::parse(&pin.sha256)?.hex() != pin.sha256
            {
                return Err(ServiceError::invalid(
                    "Firmware retained pin is duplicated, noncanonical or over budget",
                ));
            }
            if let Some(size) = unique.insert(&pin.sha256, pin.bytes)
                && size != pin.bytes
            {
                return Err(ServiceError::invalid(
                    "Firmware aliased material sizes differ",
                ));
            }
        }
        if unique.values().sum::<u64>() > 128 * 1024 * 1024 {
            return Err(ServiceError::invalid(
                "Firmware retained material exceeds 128 MiB",
            ));
        }
        Ok(value.pins)
    }
    pub fn review(&self) -> &FirmwareReview {
        &self.review
    }
    pub fn context(&self) -> &FirmwareUpdateContextDto {
        &self.prepared.context
    }
    pub fn pins(&self) -> &[FirmwareMaterialPin] {
        &self.prepared.pins
    }
    pub fn materials(&self) -> impl Iterator<Item = (&FirmwareMaterialKind, &[u8])> {
        self.materials.iter().map(|(k, v)| (k, v.as_ref()))
    }
    pub fn preimage(&self) -> &[u8] {
        &self.preimage
    }
    pub fn digest(&self) -> JobSha256 {
        JobSha256::of(&self.preimage)
    }
    pub fn budget(&self) -> &FirmwareJournalBudget {
        &self.prepared.budget
    }
    pub fn chunk(&self) -> u16 {
        self.prepared.chunk
    }
    pub fn permits_flash(&self) -> bool {
        false
    }
    pub fn complete_backup_verified(&self) -> bool {
        false
    }
    pub fn blocks(&self) -> FirmwareBlockIterator<'_> {
        FirmwareBlockIterator {
            job: self,
            section: 0,
            index: 0,
            ordinal: 0,
        }
    }
    pub fn block(&self, ordinal: u64) -> Result<FirmwareBlockDescription, ServiceError> {
        let mut preceding = 0;
        for (section, selector) in FirmwareSectionSelector::IMAGE_TRANSFER_ORDER
            .iter()
            .enumerate()
        {
            let slot = selector.image_table_slot().expect("image selector");
            let s = &self.review.summary().sections[slot];
            let count = (s.bytes as u64).div_ceil(u64::from(self.chunk()));
            if ordinal < preceding + count {
                return self.describe(section, (ordinal - preceding) as u32, ordinal);
            }
            preceding += count;
        }
        Err(ServiceError::invalid(
            "Firmware absolute block is outside the image",
        ))
    }
    fn describe(
        &self,
        section: usize,
        index: u32,
        ordinal: u64,
    ) -> Result<FirmwareBlockDescription, ServiceError> {
        let selector = FirmwareSectionSelector::IMAGE_TRANSFER_ORDER[section];
        let slot = selector.image_table_slot().expect("image selector");
        let s = &self.review.summary().sections[slot];
        let local = index as usize * usize::from(self.chunk());
        if local >= s.bytes {
            return Err(ServiceError::invalid("Firmware block exceeds section"));
        }
        let content = (s.bytes - local).min(usize::from(self.chunk()));
        let offset = s.file_offset + local;
        let bytes = &self.review.source()[offset..offset + content];
        let wire = firmware_block_candidate(index, self.chunk(), bytes)
            .map_err(|e| ServiceError::invalid(e.to_string()))?;
        Ok(FirmwareBlockDescription {
            ordinal,
            section: section as u8,
            selector: selector.raw(),
            block_in_section: index,
            index_mod128: (index % 128) as u8,
            file_offset: offset as u64,
            content_bytes: content as u16,
            padded_bytes: self.chunk() - content as u16,
            content_sha256: JobSha256::of(bytes).hex(),
            padding_sha256: JobSha256::of(&vec![255; usize::from(self.chunk()) - content]).hex(),
            wire_sha256: JobSha256::of(wire.raw()).hex(),
            wire_bytes: wire.raw().len() as u16,
        })
    }
    pub fn block_wire(
        &self,
        description: &FirmwareBlockDescription,
    ) -> Result<Vec<u8>, ServiceError> {
        if self.block(description.ordinal)? != *description {
            return Err(ServiceError::invalid(
                "Firmware block projection differs from exact image",
            ));
        }
        let start = description.file_offset as usize;
        firmware_block_candidate(
            description.block_in_section,
            self.chunk(),
            &self.review.source()[start..start + usize::from(description.content_bytes)],
        )
        .map(|b| b.raw().to_vec())
        .map_err(|e| ServiceError::invalid(e.to_string()))
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareBlockDescription {
    pub ordinal: u64,
    /// Zero-based native transfer phase, not a raw FRMW table slot.
    pub section: u8,
    pub selector: u8,
    pub block_in_section: u32,
    pub index_mod128: u8,
    pub file_offset: u64,
    pub content_bytes: u16,
    pub padded_bytes: u16,
    pub content_sha256: String,
    pub padding_sha256: String,
    pub wire_sha256: String,
    pub wire_bytes: u16,
}
pub struct FirmwareBlockIterator<'a> {
    job: &'a ReviewedFirmwareUpdate,
    section: usize,
    index: u32,
    ordinal: u64,
}
impl Iterator for FirmwareBlockIterator<'_> {
    type Item = FirmwareBlockDescription;
    fn next(&mut self) -> Option<Self::Item> {
        if self.section == 4 {
            return None;
        }
        let result = self
            .job
            .describe(self.section, self.index, self.ordinal)
            .expect("checked section iterator");
        self.index += 1;
        self.ordinal += 1;
        let slot = FirmwareSectionSelector::IMAGE_TRANSFER_ORDER[self.section]
            .image_table_slot()
            .expect("image selector");
        if self.index as usize * usize::from(self.job.chunk())
            >= self.job.review.summary().sections[slot].bytes
        {
            self.section += 1;
            self.index = 0;
        }
        Some(result)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirmwareObservationOrigin {
    LocalObservation,
    MockSimulation,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareCallbackDto {
    pub sequence: u64,
    pub at_ms: u64,
    pub clock_origin: String,
    pub epoch: SessionEpoch,
    pub binding_sha256: String,
    pub raw: Vec<u8>,
}
impl FirmwareCallbackDto {
    pub fn validate(&self) -> Result<FirmwareRxEvidence, ServiceError> {
        if !text(&self.clock_origin, 128)
            || JobSha256::parse(&self.binding_sha256)?.hex() != self.binding_sha256
        {
            return Err(ServiceError::invalid(
                "Firmware callback binding/clock is invalid",
            ));
        }
        FirmwareRxEvidence::retain(&self.raw).map_err(|e| ServiceError::invalid(e.to_string()))
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirmwareOsExtent {
    Known(u64),
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirmwareDrainObservation {
    NotAttempted,
    Returned,
    Failed,
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareSendObservation {
    pub action: u64,
    pub wire_sha256: String,
    pub attempted: u64,
    pub extent: FirmwareOsExtent,
    pub drain: FirmwareDrainObservation,
    pub started_sequence: u64,
    pub finished_sequence: u64,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
}
impl FirmwareSendObservation {
    pub fn complete_os_offer(&self) -> bool {
        self.extent == FirmwareOsExtent::Known(self.attempted)
            && self.drain == FirmwareDrainObservation::Returned
            && self.finished_sequence >= self.started_sequence
            && self.finished_at_ms >= self.started_at_ms
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirmwareHostClosure {
    CancelledBeforeEntry,
    ObservationFinished,
    HostStoppedIndeterminate,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirmwareDerivedLocalState {
    Prepared,
    Recording,
    AwaitingFinalizeEvidence,
    UnresolvedAction,
    ClosedUnverified,
    Indeterminate,
    RecoveredPaused,
}
/// Bounded streaming projection. Raw observations cannot manufacture an ACK key.
#[derive(Debug, Clone)]
pub struct FirmwareLocalProjection {
    origin: FirmwareObservationOrigin,
    state: FirmwareDerivedLocalState,
    pending: Option<FirmwareBlockDescription>,
    offered: u64,
    simulated: u64,
    next: u64,
    total_blocks: u64,
    send_observed: bool,
    expected: Option<FirmwareNormalBindingDto>,
    last_callback: Option<u64>,
    stopped: bool,
    recorded_closure: Option<FirmwareHostClosure>,
}
impl FirmwareLocalProjection {
    pub fn new(origin: FirmwareObservationOrigin) -> Self {
        Self {
            origin,
            state: FirmwareDerivedLocalState::Prepared,
            pending: None,
            offered: 0,
            simulated: 0,
            next: 0,
            total_blocks: 0,
            send_observed: false,
            expected: None,
            last_callback: None,
            stopped: false,
            recorded_closure: None,
        }
    }
    pub fn for_job(job: &ReviewedFirmwareUpdate, origin: FirmwareObservationOrigin) -> Self {
        let mut p = Self::new(origin);
        p.expected = job.context().normal.clone();
        p.total_blocks = job.budget().blocks;
        p
    }
    pub fn origin(&self) -> FirmwareObservationOrigin {
        self.origin
    }
    pub fn state(&self) -> &FirmwareDerivedLocalState {
        &self.state
    }
    pub fn pending(&self) -> Option<&FirmwareBlockDescription> {
        self.pending.as_ref()
    }
    pub fn next_block(&self) -> u64 {
        self.next
    }
    pub fn offered_source_bytes(&self) -> u64 {
        self.offered
    }
    pub fn simulated_source_bytes(&self) -> u64 {
        self.simulated
    }
    pub fn recorded_closure(&self) -> Option<&FirmwareHostClosure> {
        self.recorded_closure.as_ref()
    }
    pub fn device_verified_bytes(&self) -> u64 {
        0
    }
    pub fn simulated_awaiting_finalize(&self) -> bool {
        self.origin == FirmwareObservationOrigin::MockSimulation
            && self.total_blocks != 0
            && self.next == self.total_blocks
    }
    pub fn completion_verified(&self) -> bool {
        false
    }
    pub fn resume_supported(&self) -> bool {
        false
    }
    pub fn checkpoint(
        &mut self,
        job: &ReviewedFirmwareUpdate,
        d: FirmwareBlockDescription,
    ) -> Result<(), ServiceError> {
        if self.stopped
            || self.recorded_closure.is_some()
            || self.pending.is_some()
            || d.ordinal != self.next
            || job.block(d.ordinal)? != d
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Firmware local action is stale, unresolved or stopped",
            ));
        }
        self.pending = Some(d);
        self.send_observed = false;
        self.state = FirmwareDerivedLocalState::UnresolvedAction;
        Ok(())
    }
    pub fn observe_send(&mut self, s: &FirmwareSendObservation) -> Result<(), ServiceError> {
        let d = self.pending.as_ref().ok_or_else(|| {
            ServiceError::invalid("Firmware send lacks an exact local checkpoint")
        })?;
        if self.stopped
            || self.recorded_closure.is_some()
            || self.send_observed
            || s.action != d.ordinal
            || s.wire_sha256 != d.wire_sha256
            || s.attempted != u64::from(d.wire_bytes)
        {
            return Err(ServiceError::invalid(
                "Firmware send differs from local checkpoint",
            ));
        }
        self.send_observed = true;
        if s.complete_os_offer() {
            self.offered += u64::from(d.content_bytes);
        } else {
            self.stop();
        }
        Ok(())
    }
    pub fn observe_callback(&mut self, callback: &FirmwareCallbackDto) -> Result<(), ServiceError> {
        if self.recorded_closure.is_some() {
            return Err(ServiceError::invalid(
                "Firmware callback follows recorded closure",
            ));
        }
        let evidence = callback.validate()?;
        if self.expected.as_ref().is_some_and(|n| {
            callback.epoch != n.epoch
                || callback.clock_origin != n.clock_origin
                || callback.binding_sha256
                    != JobSha256::of(&serde_json::to_vec(n).expect("normal context serializes"))
                        .hex()
        }) {
            self.stop();
        }
        if self.last_callback.is_some_and(|s| callback.sequence <= s)
            || evidence.classification().is_err()
            || matches!(
                evidence.classification(),
                Ok(gp200_protocol::FirmwareRxCandidate::NativeGlobalError)
            )
        {
            self.stop();
        }
        self.last_callback = Some(callback.sequence);
        Ok(())
    }
    pub fn acknowledge_mock(&mut self, key: &FirmwareBlockDescription) -> Result<(), ServiceError> {
        if self.origin != FirmwareObservationOrigin::MockSimulation
            || !self.send_observed
            || self.stopped
            || self.recorded_closure.is_some()
            || self.pending.as_ref() != Some(key)
        {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Only an explicit matching mock key can advance simulated progress",
            ));
        }
        self.simulated += u64::from(key.content_bytes);
        self.next += 1;
        self.pending = None;
        self.state = if self.total_blocks != 0 && self.next == self.total_blocks {
            FirmwareDerivedLocalState::AwaitingFinalizeEvidence
        } else {
            FirmwareDerivedLocalState::Recording
        };
        Ok(())
    }
    pub fn stop(&mut self) {
        self.stopped = true;
        self.state = FirmwareDerivedLocalState::Indeterminate;
    }
    pub fn close(&mut self, closure: FirmwareHostClosure) -> Result<(), ServiceError> {
        if self.recorded_closure.is_some()
            || matches!(closure, FirmwareHostClosure::CancelledBeforeEntry)
                && (self.pending.is_some() || self.stopped || self.offered != 0 || self.next != 0)
        {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Firmware action cannot be safely cancelled or closed again",
            ));
        }
        let indeterminate = closure == FirmwareHostClosure::HostStoppedIndeterminate;
        self.recorded_closure = Some(closure);
        self.state = if indeterminate || self.pending.is_some() || self.stopped {
            FirmwareDerivedLocalState::Indeterminate
        } else {
            FirmwareDerivedLocalState::ClosedUnverified
        };
        self.stopped = true;
        Ok(())
    }
    pub fn derive_reopen(&mut self) {
        if self.recorded_closure.is_none() {
            self.state = if self.pending.is_some() || self.stopped {
                FirmwareDerivedLocalState::Indeterminate
            } else {
                FirmwareDerivedLocalState::RecoveredPaused
            };
        }
        self.stopped = true;
    }
}
