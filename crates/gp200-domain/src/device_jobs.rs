//! Reviewed normal-mode asset preparations. Neither metadata, an imported
//! manifest, a hash string nor a simulated send can create write authority.
//! Firmware is deliberately a different review type.
use crate::*;
use gp200_protocol::{Identity, WireDeviceModel, asset_upload_candidate};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::Arc};

const MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;
const IR_CODEC: &str = "gp200-ir-44100-pcm24-v1";
const CLO_CODEC: &str = "gp200-vtsi-prefix8192-v1";

/// Fixed-size digest, computed from authoritative immutable bytes. Parsing a
/// claim does not verify the bytes; `RetainedJobBlob::checked` does that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct JobSha256([u8; 32]);
impl JobSha256 {
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
    pub fn parse(hex: &str) -> Result<Self, ServiceError> {
        if hex.len() != 64 || !hex.bytes().all(|v| v.is_ascii_hexdigit()) {
            return Err(ServiceError::invalid(
                "SHA-256 must contain exactly 64 hexadecimal digits",
            ));
        }
        let mut digest = [0; 32];
        for (index, byte) in digest.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
                .map_err(|_| ServiceError::invalid("Invalid SHA-256"))?;
        }
        Ok(Self(digest))
    }
    pub fn hex(self) -> String {
        self.0.iter().map(|v| format!("{v:02x}")).collect()
    }
}

#[derive(Debug, Clone)]
pub struct RetainedJobBlob {
    id: AssetId,
    bytes: Arc<[u8]>,
    sha256: JobSha256,
}
impl RetainedJobBlob {
    pub fn checked(
        id: AssetId,
        bytes: Arc<[u8]>,
        claimed: JobSha256,
    ) -> Result<Self, ServiceError> {
        if id.0.trim().is_empty() || bytes.is_empty() || bytes.len() > MAX_BLOB_BYTES {
            return Err(ServiceError::invalid(
                "Retained job source needs an identity and 1..64 MiB of bytes",
            ));
        }
        if JobSha256::of(&bytes) != claimed {
            return Err(ServiceError::invalid(
                "Retained job source SHA-256 does not match its bytes",
            ));
        }
        Ok(Self {
            id,
            bytes,
            sha256: claimed,
        })
    }
    pub fn id(&self) -> &AssetId {
        &self.id
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn sha256(&self) -> JobSha256 {
        self.sha256
    }
    pub(crate) fn retained_bytes(&self) -> Arc<[u8]> {
        Arc::clone(&self.bytes)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ObservedDeviceJobMode {
    Normal,
}

/// Produced by the transport actor after a valid normal identity reply. The
/// constructor checks bytes; consumers must still compare it with their own
/// current actor observation. This is not deserializable authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceJobSession {
    epoch: SessionEpoch,
    selection: DeviceSelection,
    identity_revision: Revision,
    identity_sha256: JobSha256,
    identity: DeviceIdentity,
    capabilities_sha256: JobSha256,
    native_mode: ObservedDeviceJobMode,
}
impl DeviceJobSession {
    pub fn observe_normal(
        epoch: SessionEpoch,
        selection: DeviceSelection,
        revision: Revision,
        raw: &RawTransfer,
        identity: &DeviceIdentity,
        capabilities: &Capabilities,
    ) -> Result<Self, ServiceError> {
        let parsed = Identity::from_transfer(raw).ok_or_else(|| {
            ServiceError::invalid("Device job session requires the complete identity reply")
        })?;
        let model = match parsed.model() {
            WireDeviceModel::Gp200 => DeviceModel::Gp200,
            WireDeviceModel::Gp200Lt => DeviceModel::Gp200Lt,
            WireDeviceModel::Gp200Jr => DeviceModel::Gp200Jr,
            WireDeviceModel::Unknown(_) => {
                return Err(ServiceError::unsupported(
                    "Unknown device model cannot prepare an asset job",
                ));
            }
        };
        if parsed.state_raw != 1
            || identity.model != model
            || capabilities.model != model
            || raw.kind() != gp200_protocol::WRITE_KIND
            || raw.records().len() != 1
            || revision.0 == 0
            || selection.input_id.trim().is_empty()
            || selection.output_id.trim().is_empty()
        {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Device job session needs matching normal-mode identity, exact endpoints and a live observation revision",
            ));
        }
        if identity.firmware.is_some() {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "The normal identity reply does not establish an installed firmware version",
            ));
        }
        let bytes = serde_json::to_vec(capabilities)
            .map_err(|error| ServiceError::invalid(error.to_string()))?;
        Ok(Self {
            epoch,
            selection,
            identity_revision: revision,
            identity_sha256: JobSha256::of(raw.payload()),
            identity: identity.clone(),
            capabilities_sha256: JobSha256::of(&bytes),
            native_mode: ObservedDeviceJobMode::Normal,
        })
    }
    pub fn epoch(&self) -> SessionEpoch {
        self.epoch
    }
    pub fn selection(&self) -> &DeviceSelection {
        &self.selection
    }
    pub fn identity_revision(&self) -> Revision {
        self.identity_revision
    }
    pub fn identity_sha256(&self) -> JobSha256 {
        self.identity_sha256
    }
    pub fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }
    pub fn native_mode(&self) -> ObservedDeviceJobMode {
        self.native_mode
    }
    pub fn matches_capabilities(
        &self,
        identity: &DeviceIdentity,
        capabilities: &Capabilities,
    ) -> bool {
        self.identity == *identity
            && serde_json::to_vec(capabilities)
                .is_ok_and(|bytes| JobSha256::of(&bytes) == self.capabilities_sha256)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum IrJobLengthPolicy {
    RejectLong,
    KeepFirst1024,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum NativeCloProfile {
    NativeA128B2048Layout,
    IndependentA128B1024Padded,
}

#[derive(Debug, Clone)]
enum AssetRecipeData {
    Ir {
        wav: RetainedJobBlob,
        length: IrJobLengthPolicy,
    },
    Clo {
        container: RetainedJobBlob,
        profile: NativeCloProfile,
    },
}
/// Constructors re-inspect the exact WAV/CLO and independently reconstruct the
/// only allowed derived content. No raw record, opcode or transfer plan input.
#[derive(Debug, Clone)]
pub struct NormalAssetRecipe {
    target: AssetSlot,
    name: String,
    data: AssetRecipeData,
    derived: Arc<[u8]>,
    derived_sha256: JobSha256,
}
impl NormalAssetRecipe {
    pub fn ir(
        target: AssetSlot,
        name: String,
        wav: RetainedJobBlob,
        length: IrJobLengthPolicy,
        claimed_coefficients: RetainedJobBlob,
    ) -> Result<Self, ServiceError> {
        if target.bank() != AssetBank::Ir {
            return Err(ServiceError::invalid(
                "IR recipe cannot target the SnapTone bank",
            ));
        }
        let derived = repack_ir(wav.bytes(), length)?;
        if derived.as_slice() != claimed_coefficients.bytes() {
            return Err(ServiceError::invalid(
                "IR coefficients differ from independent signed PCM24 reconstruction",
            ));
        }
        Self::finish(target, name, AssetRecipeData::Ir { wav, length }, derived)
    }
    pub fn native_clo(
        target: AssetSlot,
        name: String,
        container: RetainedJobBlob,
        claimed_prefix: RetainedJobBlob,
    ) -> Result<Self, ServiceError> {
        if target.bank() != AssetBank::SnapTone {
            return Err(ServiceError::invalid(
                "CLO recipe cannot target the IR bank",
            ));
        }
        let profile = inspect_job_clo(container.bytes())?;
        let derived = container.bytes()[..8192].to_vec();
        if derived.as_slice() != claimed_prefix.bytes() {
            return Err(ServiceError::invalid(
                "SnapTone native prefix must equal the first 8192 bytes of the inspected complete CLO",
            ));
        }
        Self::finish(
            target,
            name,
            AssetRecipeData::Clo { container, profile },
            derived,
        )
    }
    fn finish(
        target: AssetSlot,
        name: String,
        data: AssetRecipeData,
        derived: Vec<u8>,
    ) -> Result<Self, ServiceError> {
        // The pure codec checks exactly the native ASCII name constraints. Its
        // candidate is discarded; a recipe never contains wire authorization.
        asset_upload_candidate(target, &name, &derived)
            .map_err(|e| ServiceError::invalid(e.to_string()))?;
        Ok(Self {
            target,
            name,
            data,
            derived_sha256: JobSha256::of(&derived),
            derived: derived.into(),
        })
    }
    pub fn target(&self) -> AssetSlot {
        self.target
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn derived(&self) -> &[u8] {
        &self.derived
    }
    pub fn derived_sha256(&self) -> JobSha256 {
        self.derived_sha256
    }
    pub fn operation(&self) -> OperationKind {
        match self.target.bank() {
            AssetBank::Ir => OperationKind::IrWrite,
            AssetBank::SnapTone => OperationKind::SnapToneWrite,
        }
    }
    pub fn codec_revision(&self) -> &'static str {
        match self.data {
            AssetRecipeData::Ir { .. } => IR_CODEC,
            AssetRecipeData::Clo { .. } => CLO_CODEC,
        }
    }
    pub fn clo_profile(&self) -> Option<NativeCloProfile> {
        match self.data {
            AssetRecipeData::Clo { profile, .. } => Some(profile),
            _ => None,
        }
    }
    pub fn ir_length_policy(&self) -> Option<IrJobLengthPolicy> {
        match self.data {
            AssetRecipeData::Ir { length, .. } => Some(length),
            _ => None,
        }
    }
    pub fn input_sha256(&self) -> JobSha256 {
        match &self.data {
            AssetRecipeData::Ir { wav, .. } => wav.sha256,
            AssetRecipeData::Clo { container, .. } => container.sha256,
        }
    }
    fn recheck(&self) -> Result<(), ServiceError> {
        let content = match &self.data {
            AssetRecipeData::Ir { wav, length } => repack_ir(wav.bytes(), *length)?,
            AssetRecipeData::Clo { container, profile } => {
                if inspect_job_clo(container.bytes())? != *profile {
                    return Err(ServiceError::invalid("CLO profile changed"));
                }
                container.bytes()[..8192].to_vec()
            }
        };
        if content.as_slice() != self.derived() || JobSha256::of(&content) != self.derived_sha256 {
            return Err(ServiceError::invalid("Derived asset bytes or hash changed"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum JobConversionInput {
    NamModel,
    CalibrationInputWav,
    CalibrationOutputWav,
}
#[derive(Debug, Clone)]
pub struct JobConversion {
    converter_revision: String,
    options: Arc<[u8]>,
    options_sha256: JobSha256,
    dependencies: BTreeMap<JobConversionInput, RetainedJobBlob>,
}
impl JobConversion {
    pub fn checked(
        converter_revision: String,
        options: Arc<[u8]>,
        claimed_options: JobSha256,
        dependencies: BTreeMap<JobConversionInput, RetainedJobBlob>,
    ) -> Result<Self, ServiceError> {
        if converter_revision.trim().is_empty()
            || converter_revision.len() > 256
            || options.len() > 65536
            || JobSha256::of(&options) != claimed_options
        {
            return Err(ServiceError::invalid(
                "Conversion revision or exact option bytes/hash are invalid",
            ));
        }
        Ok(Self {
            converter_revision,
            options,
            options_sha256: claimed_options,
            dependencies,
        })
    }
    pub fn converter_revision(&self) -> &str {
        &self.converter_revision
    }
    pub fn options_sha256(&self) -> JobSha256 {
        self.options_sha256
    }
    pub fn dependencies(&self) -> &BTreeMap<JobConversionInput, RetainedJobBlob> {
        &self.dependencies
    }
}

/// Retained local files are useful recovery material, but their presence does
/// not prove that they are the bytes currently stored in a device slot.
#[derive(Debug, Clone)]
pub enum AssetBackupReview {
    Unavailable {
        reason: String,
    },
    RetainedOriginal {
        content: RetainedJobBlob,
        baseline_sha256: JobSha256,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum DeviceJobBlocker {
    NoVerifiedWriteCapability,
    TransferReadbackRecoveryProfileMissing,
    FullContentBackupUnverified,
    IndependentCalibrationHasNoHardwareProof,
}
impl DeviceJobBlocker {
    pub fn explanation(&self) -> &'static str {
        match self {
            Self::NoVerifiedWriteCapability => {
                "This connected device has no verified asset write capability"
            }
            Self::TransferReadbackRecoveryProfileMissing => {
                "Upload acceptance, readback, cancellation and recovery still need a complete verified profile"
            }
            Self::FullContentBackupUnverified => {
                "Slot metadata and retained local files cannot establish a full backup of the current device content"
            }
            Self::IndependentCalibrationHasNoHardwareProof => {
                "This independent NAM calibration profile has not been compared on hardware"
            }
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct DeviceJobReview {
    pub id: JobId,
    pub target: AssetSlot,
    pub name: String,
    pub source_sha256: JobSha256,
    pub derived_sha256: JobSha256,
    pub derived_bytes: usize,
    pub baseline_sha256: JobSha256,
    pub baseline_revision: Revision,
    pub review_sha256: JobSha256,
    pub blockers: Vec<DeviceJobBlocker>,
}

#[derive(Debug, Clone)]
pub struct PreparedDeviceJob {
    inner: Arc<PreparedDeviceJobData>,
}
#[derive(Debug)]
struct PreparedDeviceJobData {
    id: JobId,
    session: DeviceJobSession,
    source: RetainedJobBlob,
    recipe: NormalAssetRecipe,
    conversion: JobConversion,
    baseline: Arc<DeviceAssetMetadata>,
    backup: AssetBackupReview,
    review_sha256: JobSha256,
}
impl PreparedDeviceJob {
    pub fn prepare(
        id: JobId,
        session: DeviceJobSession,
        source: RetainedJobBlob,
        recipe: NormalAssetRecipe,
        conversion: JobConversion,
        baseline: Arc<DeviceAssetMetadata>,
        backup: AssetBackupReview,
    ) -> Result<Self, ServiceError> {
        if baseline.slot() != recipe.target
            || baseline.stamp().epoch != session.epoch
            || baseline.revision().0 == 0
            || !provenance_matches(baseline.source(), &session)
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Asset baseline bank, slot, revision or session provenance does not match preparation",
            ));
        }
        recipe.recheck()?;
        if conversion.converter_revision != recipe.codec_revision() {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Conversion codec revision has no matching typed preparation profile",
            ));
        }
        if JobSha256::of(&conversion.options) != conversion.options_sha256 {
            return Err(ServiceError::invalid("Conversion options hash changed"));
        }
        if source.sha256 != recipe.input_sha256()
            && (recipe.target.bank() != AssetBank::SnapTone
                || conversion
                    .dependencies
                    .get(&JobConversionInput::NamModel)
                    .is_none_or(|blob| blob.sha256 != source.sha256)
                || [
                    JobConversionInput::CalibrationInputWav,
                    JobConversionInput::CalibrationOutputWav,
                ]
                .iter()
                .any(|role| !conversion.dependencies.contains_key(role)))
        {
            return Err(ServiceError::invalid(
                "Original source does not match the recipe input or complete retained NAM calibration dependencies",
            ));
        }
        if let AssetBackupReview::RetainedOriginal {
            content,
            baseline_sha256,
        } = &backup
            && (*baseline_sha256 != JobSha256::of(baseline.raw().payload())
                || content.bytes.len() != recipe.target.bank().transfer_bytes())
        {
            return Err(ServiceError::invalid(
                "Retained original is not bound to this metadata baseline or bank payload length",
            ));
        }
        let review_sha256 = review_digest(
            id,
            &session,
            &source,
            &recipe,
            &conversion,
            &baseline,
            &backup,
        )?;
        Ok(Self {
            inner: Arc::new(PreparedDeviceJobData {
                id,
                session,
                source,
                recipe,
                conversion,
                baseline,
                backup,
                review_sha256,
            }),
        })
    }
    pub fn id(&self) -> JobId {
        self.inner.id
    }
    pub fn review_sha256(&self) -> JobSha256 {
        self.inner.review_sha256
    }
    /// Exact immutable review-hash preimage. Retaining these bytes does not
    /// authorize submission or recreate a live job from an imported receipt.
    pub fn review_preimage(&self) -> Result<Vec<u8>, ServiceError> {
        review_preimage(
            self.inner.id,
            &self.inner.session,
            &self.inner.source,
            &self.inner.recipe,
            &self.inner.conversion,
            &self.inner.baseline,
            &self.inner.backup,
        )
    }
    pub fn operation(&self) -> OperationKind {
        self.inner.recipe.operation()
    }
    pub fn session(&self) -> &DeviceJobSession {
        &self.inner.session
    }
    pub fn target(&self) -> AssetSlot {
        self.inner.recipe.target
    }
    pub fn recipe(&self) -> &NormalAssetRecipe {
        &self.inner.recipe
    }
    pub fn baseline(&self) -> &Arc<DeviceAssetMetadata> {
        &self.inner.baseline
    }
    pub fn review(&self, capabilities: &Capabilities) -> DeviceJobReview {
        let mut blockers = vec![
            DeviceJobBlocker::TransferReadbackRecoveryProfileMissing,
            DeviceJobBlocker::FullContentBackupUnverified,
        ];
        if !capabilities.allows(self.operation(), self.inner.session.identity()) {
            blockers.insert(0, DeviceJobBlocker::NoVerifiedWriteCapability);
        }
        if self.inner.recipe.clo_profile() == Some(NativeCloProfile::IndependentA128B1024Padded) {
            blockers.push(DeviceJobBlocker::IndependentCalibrationHasNoHardwareProof);
        }
        DeviceJobReview {
            id: self.inner.id,
            target: self.target(),
            name: self.inner.recipe.name.clone(),
            source_sha256: self.inner.source.sha256,
            derived_sha256: self.inner.recipe.derived_sha256,
            derived_bytes: self.inner.recipe.derived.len(),
            baseline_sha256: JobSha256::of(self.inner.baseline.raw().payload()),
            baseline_revision: self.inner.baseline.revision(),
            review_sha256: self.inner.review_sha256,
            blockers,
        }
    }
    pub fn recheck_active(
        &self,
        active: &DeviceJobSession,
        identity: &DeviceIdentity,
        capabilities: &Capabilities,
        baseline: &DeviceAssetMetadata,
    ) -> Result<(), ServiceError> {
        if self.inner.session != *active || !active.matches_capabilities(identity, capabilities) {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Device job epoch, endpoints, identity revision or capabilities changed; prepare and review again",
            ));
        }
        if baseline.slot() != self.target()
            || baseline.revision() != self.inner.baseline.revision()
            || baseline.stamp().epoch != active.epoch
            || baseline.raw() != self.inner.baseline.raw()
            || !provenance_matches(baseline.source(), active)
        {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Device asset baseline changed; refresh, prepare and review again",
            ));
        }
        self.inner.recipe.recheck()?;
        if JobSha256::of(self.inner.source.bytes()) != self.inner.source.sha256
            || review_digest(
                self.inner.id,
                &self.inner.session,
                &self.inner.source,
                &self.inner.recipe,
                &self.inner.conversion,
                &self.inner.baseline,
                &self.inner.backup,
            )? != self.inner.review_sha256
        {
            return Err(ServiceError::invalid(
                "Prepared source, conversion or review digest changed",
            ));
        }
        Ok(())
    }
}

/// Sealed, non-deserializable command type. There is intentionally no issuing
/// profile today: upload ACK, full-content backup/restore, cancellation and
/// recovery are incomplete. Capability claims cannot bypass that registry.
///
/// Imported JSON cannot construct write authority:
/// ```compile_fail
/// let _: gp200_domain::ValidatedDeviceJob = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug, Clone)]
pub struct ValidatedDeviceJob {
    prepared: PreparedDeviceJob,
}
impl ValidatedDeviceJob {
    pub fn validate(
        prepared: PreparedDeviceJob,
        reviewed_sha256: JobSha256,
        active: &DeviceJobSession,
        identity: &DeviceIdentity,
        capabilities: &Capabilities,
        baseline: &DeviceAssetMetadata,
    ) -> Result<Self, ServiceError> {
        if prepared.review_sha256() != reviewed_sha256 {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Prepared job differs from the reviewed summary; review it again",
            ));
        }
        prepared.recheck_active(active, identity, capabilities, baseline)?;
        let review = prepared.review(capabilities);
        Err(ServiceError::new(
            ErrorKind::Unverified,
            format!(
                "Asset preparation is reviewable but cannot execute: {}",
                review
                    .blockers
                    .iter()
                    .map(DeviceJobBlocker::explanation)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        ))
    }
    pub fn prepared(&self) -> &PreparedDeviceJob {
        &self.prepared
    }
    pub fn recheck_before_submit(
        &self,
        active: &DeviceJobSession,
        identity: &DeviceIdentity,
        capabilities: &Capabilities,
        baseline: &DeviceAssetMetadata,
    ) -> Result<(), ServiceError> {
        self.prepared
            .recheck_active(active, identity, capabilities, baseline)?;
        Err(ServiceError::new(
            ErrorKind::Unverified,
            "No complete normal asset transfer/restore profile is registered",
        ))
    }
}

fn provenance_matches(source: &SnapshotSource, session: &DeviceJobSession) -> bool {
    match (session.identity.mode, source) {
        (BackendMode::Hardware, SnapshotSource::Hardware { epoch }) => *epoch == session.epoch,
        (BackendMode::Simulator, SnapshotSource::Simulator) => true,
        // Replay has no live target content or write authority.
        _ => false,
    }
}
fn review_digest(
    id: JobId,
    session: &DeviceJobSession,
    source: &RetainedJobBlob,
    recipe: &NormalAssetRecipe,
    conversion: &JobConversion,
    baseline: &DeviceAssetMetadata,
    backup: &AssetBackupReview,
) -> Result<JobSha256, ServiceError> {
    Ok(JobSha256::of(&review_preimage(
        id, session, source, recipe, conversion, baseline, backup,
    )?))
}
fn review_preimage(
    id: JobId,
    session: &DeviceJobSession,
    source: &RetainedJobBlob,
    recipe: &NormalAssetRecipe,
    conversion: &JobConversion,
    baseline: &DeviceAssetMetadata,
    backup: &AssetBackupReview,
) -> Result<Vec<u8>, ServiceError> {
    let dependencies: Vec<_> = conversion
        .dependencies
        .iter()
        .map(|(role, blob)| (role, blob.id(), blob.sha256(), blob.bytes().len()))
        .collect();
    let original = match backup {
        AssetBackupReview::Unavailable { reason } => (None, Some(reason.as_str())),
        AssetBackupReview::RetainedOriginal {
            content,
            baseline_sha256,
        } => (
            Some((
                content.id(),
                content.sha256(),
                content.bytes().len(),
                *baseline_sha256,
            )),
            None,
        ),
    };
    let bytes = serde_json::to_vec(&(
        (
            "gp200-reviewed-asset-v1",
            id,
            session,
            source.id(),
            source.sha256,
            source.bytes().len(),
            recipe.target,
            &recipe.name,
            recipe.codec_revision(),
            recipe.clo_profile(),
            recipe.ir_length_policy(),
            recipe.input_sha256(),
            recipe.derived_sha256,
            recipe.derived.len(),
        ),
        (
            &conversion.converter_revision,
            conversion.options_sha256,
            dependencies,
            baseline.revision(),
            baseline.source(),
            baseline.captured_at_ms(),
            JobSha256::of(baseline.raw().payload()),
            original,
        ),
    ))
    .map_err(|e| ServiceError::invalid(e.to_string()))?;
    Ok(bytes)
}

fn le32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("bounded input"))
}
fn repack_ir(wav: &[u8], policy: IrJobLengthPolicy) -> Result<Vec<u8>, ServiceError> {
    if wav.len() < 12
        || &wav[..4] != b"RIFF"
        || &wav[8..12] != b"WAVE"
        || le32(wav, 4) as usize != wav.len() - 8
    {
        return Err(ServiceError::invalid(
            "IR job requires an exact RIFF/WAVE file",
        ));
    }
    let (mut fmt, mut data) = (None, None);
    let mut cursor = 12;
    while cursor < wav.len() {
        if wav.len() - cursor < 8 {
            return Err(ServiceError::invalid("Truncated WAV chunk"));
        }
        let length = le32(wav, cursor + 4) as usize;
        let end = cursor
            .checked_add(8)
            .and_then(|v| v.checked_add(length))
            .filter(|v| *v <= wav.len())
            .ok_or_else(|| ServiceError::invalid("WAV chunk extends beyond file"))?;
        match &wav[cursor..cursor + 4] {
            b"fmt " if fmt.is_none() => fmt = Some(&wav[cursor + 8..end]),
            b"data" if data.is_none() => data = Some(&wav[cursor + 8..end]),
            b"fmt " | b"data" => {
                return Err(ServiceError::invalid("Duplicate WAV fmt/data chunks"));
            }
            _ => {}
        }
        cursor = end + (length & 1);
        if cursor > wav.len() {
            return Err(ServiceError::invalid("Missing WAV chunk padding"));
        }
    }
    let fmt = fmt.ok_or_else(|| ServiceError::invalid("WAV fmt chunk is missing"))?;
    let data = data.ok_or_else(|| ServiceError::invalid("WAV data chunk is missing"))?;
    if fmt.len() != 16
        || fmt[..4] != [1, 0, 1, 0]
        || le32(fmt, 4) != 44100
        || le32(fmt, 8) != 132300
        || fmt[12..16] != [3, 0, 24, 0]
        || data.is_empty()
        || !data.len().is_multiple_of(3)
    {
        return Err(ServiceError::new(
            ErrorKind::Unverified,
            "IR preparation profile requires mono PCM24 at 44100 Hz with complete sample words",
        ));
    }
    if data.len() / 3 > 1024 && policy == IrJobLengthPolicy::RejectLong {
        return Err(ServiceError::invalid(
            "IR has more than 1024 samples; explicit KeepFirst1024 is required",
        ));
    }
    let mut output = vec![0; 4096];
    for (index, sample) in data.as_chunks::<3>().0.iter().take(1024).enumerate() {
        let sign = if sample[2] & 0x80 == 0 { 0 } else { 0xff };
        output[index * 4..index * 4 + 4].copy_from_slice(&[sample[0], sample[1], sample[2], sign]);
    }
    Ok(output)
}
fn inspect_job_clo(bytes: &[u8]) -> Result<NativeCloProfile, ServiceError> {
    if bytes.len() != 8840 || &bytes[..4] != b"VTSI" {
        return Err(ServiceError::invalid(
            "CLO preparation requires complete 8840-byte VTSI container",
        ));
    }
    let declared = le32(bytes, 4) as usize;
    let coefficients = le32(bytes, 20) as usize;
    let profile = match (declared, coefficients) {
        (8840, 8704)
            if le32(bytes, 120) == 0
                && le32(bytes, 124) == 128
                && le32(bytes, 128) == 128
                && le32(bytes, 132) == 2048 =>
        {
            NativeCloProfile::NativeA128B2048Layout
        }
        (4744, 4608)
            if le32(bytes, 120) == 0
                && le32(bytes, 124) == 128
                && le32(bytes, 128) == 128
                && le32(bytes, 132) == 1024
                && bytes[4744..].iter().all(|v| *v == 0) =>
        {
            NativeCloProfile::IndependentA128B1024Padded
        }
        _ => {
            return Err(ServiceError::new(
                ErrorKind::Unverified,
                "Unknown CLO length, coefficient or padding profile",
            ));
        }
    };
    let mut crc = 0xffff_u16;
    for byte in &bytes[12..declared] {
        crc ^= u16::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xa001
            } else {
                crc >> 1
            };
        }
    }
    if crc != u16::from_be_bytes([bytes[8], bytes[9]]) {
        return Err(ServiceError::invalid("CLO CRC16 mismatch"));
    }
    let ranges = [
        (le32(bytes, 120), le32(bytes, 124)),
        (le32(bytes, 128), le32(bytes, 132)),
    ];
    if ranges
        .iter()
        .any(|(offset, count)| u64::from(*offset) + u64::from(*count) > (coefficients / 4) as u64)
        || (ranges[0].1 != 0
            && ranges[1].1 != 0
            && u64::from(ranges[0].0) < u64::from(ranges[1].0) + u64::from(ranges[1].1)
            && u64::from(ranges[1].0) < u64::from(ranges[0].0) + u64::from(ranges[0].1))
    {
        return Err(ServiceError::invalid(
            "CLO coefficient ranges overlap or exceed the payload",
        ));
    }
    Ok(profile)
}
