//! Explicit immutable metadata views. An observed XML Index is diagnostic;
//! it has no conversion to an existing wire model index or control plan.

use crate::{
    Catalog, CatalogError, ChoiceDefinition, ControlKind, ModelDefinition, ModelKey,
    ParameterDefinition, ParameterPresentation, SlotKind, SourceRef,
};
use gp200_domain::{
    EditorMetadataProfile, FirmwareDeviceAssociation, FirmwareVersion, UserReportedFirmware,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const OLD_JSON: &str = include_str!("../../../assets/catalog/gp200-editor-1.6.0.json");
const EVIDENCE_JSON: &str = include_str!("../../../assets/catalog/firmware-metadata-evidence.json");
const OLD_JSON_SHA: &str = "39bee495f4bd09468767cecc8b957d941372cd77881e9ae81eac9afeb2a58977";
const EVIDENCE_SHA: &str = "ab0e5dec788d1a4765569a4e361eed3b26229c018a4c50abca96461896356670";
const CURRENT_JSON_SHA: &str = "a73e5fcc27818bb7dc4917d139a91916f2d8ec71a1ad2894ed5894964b67b6bb";
const OLD_ALGORITHM_SHA: &str = "a874128d894fb2826616413419e2b4701dbb11c5e779fecf2f0ee97db2bfc8cb";
const NEW_ALGORITHM_SHA: &str = "d95d6e019d602e4dafd5b2b0aec7c1494dd52546b0e67f0ee6663d8fa6cb8e0e";

fn invalid<T>(message: &str) -> Result<T, CatalogError> {
    Err(CatalogError::Invalid(message.to_owned()))
}
fn hash_matches(bytes: &str, expected: &str) -> bool {
    format!("{:x}", Sha256::digest(bytes.as_bytes())) == expected
}

#[derive(Debug)]
pub struct EmbeddedMetadataSourceIdentity {
    profile: EditorMetadataProfile,
    catalog_sha256: &'static str,
    algorithm_sha256: &'static str,
    algorithm_file: &'static str,
}
impl EmbeddedMetadataSourceIdentity {
    pub const fn profile(&self) -> EditorMetadataProfile {
        self.profile
    }
    pub const fn catalog_sha256(&self) -> &str {
        self.catalog_sha256
    }
    pub const fn algorithm_sha256(&self) -> &str {
        self.algorithm_sha256
    }
    pub const fn algorithm_file(&self) -> &str {
        self.algorithm_file
    }
}

#[derive(Debug, Deserialize)]
struct PathLocator {
    path: String,
    line: u32,
}
#[derive(Debug, Deserialize)]
struct ModelLocator {
    module: String,
    code: u32,
    name: String,
    xml_index: u16,
    source: PathLocator,
}
#[derive(Debug, Deserialize)]
struct IndexChange {
    key: ModelKey,
    old: ModelLocator,
    new: ModelLocator,
}

/// Exact retained XML, including spelling, line locators and opaque children.
#[derive(Debug, Deserialize)]
pub struct RawXmlMetadata {
    tag: String,
    attributes: BTreeMap<String, String>,
    source: SourceRef,
    children: Vec<RawXmlMetadata>,
    #[serde(default)]
    text: Option<String>,
}
impl RawXmlMetadata {
    pub fn tag(&self) -> &str {
        &self.tag
    }
    pub fn attributes(&self) -> &BTreeMap<String, String> {
        &self.attributes
    }
    pub fn source(&self) -> &SourceRef {
        &self.source
    }
    pub fn children(&self) -> &[Self] {
        &self.children
    }
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }
}

#[derive(Debug, Deserialize)]
pub struct MetadataParameterChange {
    key: ModelKey,
    parameter_id: u8,
    old_definition: RawXmlMetadata,
    new_definition: RawXmlMetadata,
    old_default_bits: Option<u32>,
    new_default_bits: Option<u32>,
}
impl MetadataParameterChange {
    pub const fn key(&self) -> ModelKey {
        self.key
    }
    pub const fn parameter_id(&self) -> u8 {
        self.parameter_id
    }
    pub fn old_definition(&self) -> &RawXmlMetadata {
        &self.old_definition
    }
    pub fn new_definition(&self) -> &RawXmlMetadata {
        &self.new_definition
    }
    pub const fn default_bits(&self) -> (Option<u32>, Option<u32>) {
        (self.old_default_bits, self.new_default_bits)
    }
}

#[derive(Debug, Deserialize)]
struct DefaultEvidence {
    parameter_id: u8,
    bits: Option<u32>,
}
#[derive(Debug, Deserialize)]
pub struct SharedAmpMetadataEvidence {
    key: ModelKey,
    paired_cab_code: u32,
    declared_default_bits: Vec<DefaultEvidence>,
    old_source: SourceRef,
    new_source: SourceRef,
}
impl SharedAmpMetadataEvidence {
    pub const fn key(&self) -> ModelKey {
        self.key
    }
    pub const fn paired_cab_code(&self) -> u32 {
        self.paired_cab_code
    }
    pub fn declared_defaults(&self) -> impl Iterator<Item = (u8, Option<u32>)> + '_ {
        self.declared_default_bits
            .iter()
            .map(|d| (d.parameter_id, d.bits))
    }
    pub fn sources(&self) -> (&SourceRef, &SourceRef) {
        (&self.old_source, &self.new_source)
    }
}

#[derive(Debug, Deserialize)]
pub struct VendorMinimumMetadataFact {
    key: ModelKey,
    minimum: FirmwareVersion,
    release_note_location: String,
    metadata_source: SourceRef,
}
impl VendorMinimumMetadataFact {
    pub const fn key(&self) -> ModelKey {
        self.key
    }
    pub const fn minimum(&self) -> FirmwareVersion {
        self.minimum
    }
    pub fn release_note_location(&self) -> &str {
        &self.release_note_location
    }
    pub fn metadata_source(&self) -> &SourceRef {
        &self.metadata_source
    }
}
#[derive(Debug, Deserialize)]
pub struct MetadataReleaseNoteSource {
    file: String,
    bytes: usize,
    sha256: String,
}
impl MetadataReleaseNoteSource {
    pub fn file(&self) -> &str {
        &self.file
    }
    pub const fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

#[derive(Debug, Deserialize)]
struct Evidence {
    schema: u32,
    variant: String,
    old_algorithm_sha256: String,
    new_algorithm_sha256: String,
    current_catalog_sha256: String,
    old_module_counts: BTreeMap<String, usize>,
    new_module_counts: BTreeMap<String, usize>,
    old_membership: Vec<ModelKey>,
    new_membership: Vec<ModelKey>,
    index_changes: Vec<IndexChange>,
    parameter_changes: Vec<MetadataParameterChange>,
    shared_amps: Vec<SharedAmpMetadataEvidence>,
    vendor_minima: Vec<VendorMinimumMetadataFact>,
    release_note_source: MetadataReleaseNoteSource,
    grants_transport_authority: bool,
}

/// Only the pinned embedded bytes can construct this owner. Public source
/// labels, imported JSON and standalone evidence rows cannot manufacture it.
///
/// ```compile_fail
/// let forged: gp200_catalog::CheckedEmbeddedMetadata =
///     serde_json::from_str("{\"profile\":\"Gp200Editor160\"}").unwrap();
/// ```
#[derive(Debug)]
pub struct CheckedEmbeddedMetadata {
    old: Catalog,
    current: Catalog,
    old_identity: EmbeddedMetadataSourceIdentity,
    current_identity: EmbeddedMetadataSourceIdentity,
    evidence: Evidence,
}
impl CheckedEmbeddedMetadata {
    pub fn embedded() -> Result<Self, CatalogError> {
        Self::checked_bytes(OLD_JSON, crate::EMBEDDED_CATALOG_JSON, EVIDENCE_JSON)
    }
    fn checked_bytes(old: &str, current: &str, evidence: &str) -> Result<Self, CatalogError> {
        if !hash_matches(old, OLD_JSON_SHA)
            || !hash_matches(current, CURRENT_JSON_SHA)
            || !hash_matches(evidence, EVIDENCE_SHA)
        {
            return invalid("Embedded firmware metadata bytes differ from their pinned identities");
        }
        let result = Self {
            old: Catalog::from_json(old)?,
            current: Catalog::from_json(current)?,
            old_identity: EmbeddedMetadataSourceIdentity {
                profile: EditorMetadataProfile::Gp200Editor160,
                catalog_sha256: OLD_JSON_SHA,
                algorithm_sha256: OLD_ALGORITHM_SHA,
                algorithm_file: "analysis/downloads/official-editor-1.6.0/resources/Resource/GP-200/File/algorithm.xml",
            },
            current_identity: EmbeddedMetadataSourceIdentity {
                profile: EditorMetadataProfile::Gp200Editor181,
                catalog_sha256: CURRENT_JSON_SHA,
                algorithm_sha256: NEW_ALGORITHM_SHA,
                algorithm_file: "analysis/extracted/Resource/GP-200/File/algorithm.xml",
            },
            evidence: serde_json::from_str(evidence)?,
        };
        result.validate()?;
        Ok(result)
    }
    fn validate(&self) -> Result<(), CatalogError> {
        let e = &self.evidence;
        if e.schema != 1
            || e.variant != "GP-200"
            || e.grants_transport_authority
            || e.old_algorithm_sha256 != OLD_ALGORITHM_SHA
            || e.new_algorithm_sha256 != NEW_ALGORITHM_SHA
            || e.current_catalog_sha256 != CURRENT_JSON_SHA
            || (
                self.old.model_count(),
                self.old.parameter_count(),
                self.current.model_count(),
                self.current.parameter_count(),
            ) != (308, 1329, 322, 1400)
        {
            return invalid("Embedded firmware metadata schema, sources or counts differ");
        }
        let keys = |catalog: &Catalog| -> BTreeSet<ModelKey> {
            catalog
                .modules()
                .iter()
                .flat_map(|m| m.models.iter().map(ModelDefinition::key))
                .collect()
        };
        let old_keys = keys(&self.old);
        let new_keys = keys(&self.current);
        for (catalog, identity, membership, counts) in [
            (
                &self.old,
                &self.old_identity,
                &e.old_membership,
                &e.old_module_counts,
            ),
            (
                &self.current,
                &self.current_identity,
                &e.new_membership,
                &e.new_module_counts,
            ),
        ] {
            if membership.len() != catalog.model_count()
                || membership.iter().copied().collect::<BTreeSet<_>>() != keys(catalog)
                || counts.len() != 11
            {
                return invalid("Firmware metadata membership differs");
            }
            if catalog.data().provenance["source"]["sha256"] != identity.algorithm_sha256 {
                return invalid("Firmware metadata algorithm identity differs");
            }
            for module in catalog.modules() {
                if counts.get(&module.name) != Some(&module.models.len())
                    || module.source.file != identity.algorithm_file
                {
                    return invalid("Firmware metadata module counts/source differ");
                }
                let mut indices = BTreeSet::new();
                for model in &module.models {
                    if !indices.insert(model.xml_index)
                        || model.source.file != identity.algorithm_file
                        || model.source.line.is_none()
                        || model.parameters.iter().any(|p| {
                            p.source.file != identity.algorithm_file || p.source.line.is_none()
                        })
                    {
                        return invalid("Firmware metadata source/index relations differ");
                    }
                }
            }
        }
        if !old_keys.is_subset(&new_keys) || new_keys.difference(&old_keys).count() != 14 {
            return invalid("Firmware metadata additions differ");
        }
        let expected_indices: BTreeSet<_> = old_keys
            .iter()
            .copied()
            .filter(|k| {
                self.old.model_by_key(*k).unwrap().xml_index
                    != self.current.model_by_key(*k).unwrap().xml_index
            })
            .collect();
        if e.index_changes.len() != 56
            || e.index_changes
                .iter()
                .map(|r| r.key)
                .collect::<BTreeSet<_>>()
                != expected_indices
        {
            return invalid("Firmware metadata Index delta set differs");
        }
        for row in &e.index_changes {
            for (locator, catalog) in [(&row.old, &self.old), (&row.new, &self.current)] {
                let Some(model) = catalog.model_by_key(row.key) else {
                    return invalid("Index evidence references an unknown model");
                };
                if locator.code != model.code
                    || locator.module != model.module
                    || locator.name != model.name
                    || locator.xml_index != model.xml_index
                    || locator.source.path != model.source.file
                    || Some(locator.source.line) != model.source.line
                {
                    return invalid("Index evidence locator differs");
                }
            }
        }
        let expected_changes: BTreeSet<_> = [
            (0x0b00001d, 3),
            (0x0b00001f, 3),
            (0x0b000021, 3),
            (0x0b000033, 0),
        ]
        .into_iter()
        .map(|(code, pid)| (ModelKey { module: 8, code }, pid))
        .collect();
        if e.parameter_changes.len() != 4
            || e.parameter_changes
                .iter()
                .map(|r| (r.key, r.parameter_id))
                .collect::<BTreeSet<_>>()
                != expected_changes
        {
            return invalid("Parameter delta identity differs");
        }
        for row in &e.parameter_changes {
            for (node, bits, catalog) in [
                (&row.old_definition, row.old_default_bits, &self.old),
                (&row.new_definition, row.new_default_bits, &self.current),
            ] {
                let Some(parameter) = catalog
                    .model_by_key(row.key)
                    .and_then(|m| m.parameter(row.parameter_id))
                else {
                    return invalid("Parameter evidence references an unknown control");
                };
                if node.tag != parameter.ui_kind
                    || node.attributes != parameter.raw_attributes
                    || node.source != parameter.source
                    || bits != parameter.default.map(f32::to_bits)
                {
                    return invalid("Parameter evidence definition differs");
                }
            }
        }
        let expected_amps: BTreeSet<_> =
            old_keys.iter().copied().filter(|k| k.module == 3).collect();
        if e.shared_amps.len() != 71
            || e.shared_amps.iter().map(|r| r.key).collect::<BTreeSet<_>>() != expected_amps
        {
            return invalid("Shared AMP evidence identity differs");
        }
        for row in &e.shared_amps {
            let a = self.old.model_by_key(row.key).unwrap();
            let b = self.current.model_by_key(row.key).unwrap();
            let defaults = |model: &ModelDefinition| -> BTreeMap<_, _> {
                model
                    .parameters
                    .iter()
                    .map(|p| (p.parameter_id, p.default.map(f32::to_bits)))
                    .collect()
            };
            let cab = row.paired_cab_code.to_string();
            if row.old_source != a.source
                || row.new_source != b.source
                || a.raw_attributes.get("CABCode") != Some(&cab)
                || b.raw_attributes.get("CABCode") != Some(&cab)
                || defaults(a) != defaults(b)
                || row.declared_default_bits.len() != a.parameters.len()
                || row
                    .declared_default_bits
                    .iter()
                    .map(|d| (d.parameter_id, d.bits))
                    .collect::<BTreeMap<_, _>>()
                    != defaults(a)
                || self.old.model_by_id(5, row.paired_cab_code).is_none()
                || self.current.model_by_id(5, row.paired_cab_code).is_none()
            {
                return invalid("Shared AMP defaults/CAB evidence differs");
            }
        }
        if e.vendor_minima.len() != 14
            || e.vendor_minima
                .iter()
                .map(|f| f.key)
                .collect::<BTreeSet<_>>()
                != new_keys.difference(&old_keys).copied().collect()
        {
            return invalid("Vendor minimum membership differs");
        }
        for fact in &e.vendor_minima {
            let expected = if (0x0f000000..=0x0f000009).contains(&fact.key.code) {
                [1, 7, 4]
            } else {
                [1, 8, 0]
            };
            if fact.minimum.components() != expected
                || self
                    .current
                    .model_by_key(fact.key)
                    .is_none_or(|m| m.source != fact.metadata_source)
                || fact.release_note_location.is_empty()
            {
                return invalid("Vendor minimum evidence differs");
            }
        }
        if e.release_note_source.file
            != "analysis/downloads/official-firmware-1.8.2/GP-200 Firmware Release Note.pdf"
            || e.release_note_source.bytes != 73918
            || e.release_note_source.sha256
                != "05cac58eb06ff5c0de3a5393925ca1e47c11813b7f7676f5e78d22c7c48eb053"
        {
            return invalid("Vendor minimum release-note identity differs");
        }
        Ok(())
    }
    pub fn view(&self, profile: EditorMetadataProfile) -> MetadataView<'_> {
        let (catalog, identity) = match profile {
            EditorMetadataProfile::Gp200Editor160 => (&self.old, &self.old_identity),
            EditorMetadataProfile::Gp200Editor181 => (&self.current, &self.current_identity),
        };
        MetadataView {
            owner: self,
            catalog,
            identity,
        }
    }
    pub fn index_changes(&self) -> impl Iterator<Item = MetadataIndexPair<'_>> + '_ {
        self.evidence
            .index_changes
            .iter()
            .map(|r| MetadataIndexPair {
                key: r.key,
                old: self
                    .view(EditorMetadataProfile::Gp200Editor160)
                    .model(r.key)
                    .unwrap()
                    .observed_index(),
                current: self
                    .view(EditorMetadataProfile::Gp200Editor181)
                    .model(r.key)
                    .unwrap()
                    .observed_index(),
            })
    }
    pub fn parameter_changes(&self) -> &[MetadataParameterChange] {
        &self.evidence.parameter_changes
    }
    pub fn shared_amp_evidence(&self) -> &[SharedAmpMetadataEvidence] {
        &self.evidence.shared_amps
    }
    pub fn vendor_minima(&self) -> &[VendorMinimumMetadataFact] {
        &self.evidence.vendor_minima
    }
    pub fn release_note_source(&self) -> &MetadataReleaseNoteSource {
        &self.evidence.release_note_source
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MetadataView<'a> {
    owner: &'a CheckedEmbeddedMetadata,
    catalog: &'a Catalog,
    identity: &'a EmbeddedMetadataSourceIdentity,
}
impl<'a> MetadataView<'a> {
    pub fn identity(self) -> &'a EmbeddedMetadataSourceIdentity {
        self.identity
    }
    pub fn model_count(self) -> usize {
        self.catalog.model_count()
    }
    pub fn parameter_count(self) -> usize {
        self.catalog.parameter_count()
    }
    pub fn module_model_count(self, module: u8) -> Option<usize> {
        self.catalog.module_by_id(module).map(|m| m.models.len())
    }
    pub fn model(self, key: ModelKey) -> Option<MetadataModelRef<'a>> {
        self.catalog
            .model_by_key(key)
            .map(|model| MetadataModelRef {
                model,
                identity: self.identity,
            })
    }
    pub fn models(self) -> impl Iterator<Item = MetadataModelRef<'a>> + 'a {
        self.catalog
            .modules()
            .iter()
            .flat_map(|m| m.models.iter())
            .map(move |model| MetadataModelRef {
                model,
                identity: self.identity,
            })
    }
    pub fn search(self, module: Option<u8>, text: &str) -> Vec<MetadataModelRef<'a>> {
        self.catalog
            .search_models(module, text)
            .into_iter()
            .map(|model| MetadataModelRef {
                model,
                identity: self.identity,
            })
            .collect()
    }
    pub fn inspect_any(self, key: ModelKey) -> MetadataInspection<'a> {
        if let Some(model) = self.model(key) {
            return MetadataInspection::SelectedProfile(model);
        }
        match self
            .owner
            .view(EditorMetadataProfile::Gp200Editor181)
            .model(key)
        {
            Some(model) => MetadataInspection::OutsideSelectedProfile {
                selected_profile: self.identity.profile,
                model,
            },
            None => MetadataInspection::UnknownModel(key),
        }
    }
    pub fn assess(
        self,
        key: ModelKey,
        report: Option<&UserReportedFirmware>,
        associated_device: Option<&FirmwareDeviceAssociation>,
    ) -> MetadataAvailability {
        if self.owner.current.model_by_key(key).is_none() {
            return MetadataAvailability::UnknownModel;
        }
        let Some(report) = report.filter(|report| {
            associated_device == Some(report.device()) && report.device().model() == "GP-200"
        }) else {
            return MetadataAvailability::UnknownFirmwareOrProfile;
        };
        if let Some(fact) = self
            .owner
            .evidence
            .vendor_minima
            .iter()
            .find(|f| f.key == key)
        {
            return if report.version() < fact.minimum {
                MetadataAvailability::VendorMinimumNotMet {
                    minimum: fact.minimum,
                }
            } else {
                MetadataAvailability::VendorMinimumMetMetadataUnconfirmed {
                    minimum: fact.minimum,
                }
            };
        }
        if report.version().components() == [1, 6, 3]
            && self.identity.profile == EditorMetadataProfile::Gp200Editor160
        {
            if self.model(key).is_some() {
                MetadataAvailability::PresentInEditor160Metadata
            } else {
                MetadataAvailability::AbsentFromEditor160Metadata
            }
        } else {
            MetadataAvailability::UnknownFirmwareOrProfile
        }
    }
}

#[derive(Debug)]
pub enum MetadataInspection<'a> {
    SelectedProfile(MetadataModelRef<'a>),
    OutsideSelectedProfile {
        selected_profile: EditorMetadataProfile,
        model: MetadataModelRef<'a>,
    },
    UnknownModel(ModelKey),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataAvailability {
    VendorMinimumNotMet { minimum: FirmwareVersion },
    PresentInEditor160Metadata,
    AbsentFromEditor160Metadata,
    VendorMinimumMetMetadataUnconfirmed { minimum: FirmwareVersion },
    UnknownFirmwareOrProfile,
    UnknownModel,
}

/// A borrowed descriptive view has no `Deref` or conversion to the existing
/// public definition accepted by control-plan generation.
///
/// ```compile_fail
/// use gp200_catalog::{CheckedEmbeddedMetadata, ModelKey, ModelDefinition};
/// use gp200_domain::EditorMetadataProfile;
/// let owner = CheckedEmbeddedMetadata::embedded().unwrap();
/// let model = owner.view(EditorMetadataProfile::Gp200Editor160)
///     .model(ModelKey { module: 2, code: 10 }).unwrap();
/// let live_definition: &ModelDefinition = model;
/// ```
#[derive(Debug, Clone, Copy)]
pub struct MetadataModelRef<'a> {
    model: &'a ModelDefinition,
    identity: &'a EmbeddedMetadataSourceIdentity,
}
impl<'a> MetadataModelRef<'a> {
    pub fn key(self) -> ModelKey {
        self.model.key()
    }
    pub fn name(self) -> &'a str {
        &self.model.name
    }
    pub fn module_name(self) -> &'a str {
        &self.model.module
    }
    pub fn identity(self) -> &'a EmbeddedMetadataSourceIdentity {
        self.identity
    }
    pub fn display_order(self) -> u16 {
        self.model.display_index
    }
    pub fn slot_kind(self) -> SlotKind {
        self.model.slot_kind()
    }
    pub fn description(self) -> Option<&'a str> {
        self.model.description()
    }
    pub fn raw_attributes(self) -> &'a BTreeMap<String, String> {
        &self.model.raw_attributes
    }
    pub fn source(self) -> &'a SourceRef {
        &self.model.source
    }
    pub fn raw_xml(self) -> Option<&'a serde_json::Value> {
        self.model.additional.get("raw_xml")
    }
    pub fn parameter(self, id: u8) -> Option<MetadataParameterRef<'a>> {
        self.model
            .parameter(id)
            .map(|parameter| MetadataParameterRef {
                parameter,
                identity: self.identity,
            })
    }
    pub fn parameters(self) -> impl Iterator<Item = MetadataParameterRef<'a>> + 'a {
        self.model
            .parameters
            .iter()
            .map(move |parameter| MetadataParameterRef {
                parameter,
                identity: self.identity,
            })
    }
    pub fn observed_index(self) -> ObservedMetadataModelIndex<'a> {
        ObservedMetadataModelIndex {
            value: self.model.xml_index,
            key: self.key(),
            identity: self.identity,
            source: &self.model.source,
        }
    }
    pub fn local_defaults(self) -> ScopedLocalModelDefaults {
        ScopedLocalModelDefaults {
            profile: self.identity.profile,
            catalog_sha256: self.identity.catalog_sha256,
            algorithm_sha256: self.identity.algorithm_sha256,
            key: self.key(),
            declared: self
                .model
                .default_values()
                .into_iter()
                .map(|(id, value)| (id, value.to_bits()))
                .collect(),
            unresolved: self
                .model
                .parameters
                .iter()
                .filter(|p| p.default.is_none())
                .map(|p| p.parameter_id)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MetadataParameterRef<'a> {
    parameter: &'a ParameterDefinition,
    identity: &'a EmbeddedMetadataSourceIdentity,
}
impl<'a> MetadataParameterRef<'a> {
    pub fn identity(self) -> &'a EmbeddedMetadataSourceIdentity {
        self.identity
    }
    pub fn parameter_id(self) -> u8 {
        self.parameter.parameter_id
    }
    pub fn control_order(self) -> u8 {
        self.parameter.display_index
    }
    pub fn name(self) -> &'a str {
        &self.parameter.name
    }
    pub fn ui_kind(self) -> &'a str {
        &self.parameter.ui_kind
    }
    pub fn control_kind(self) -> ControlKind {
        self.parameter.control_kind()
    }
    pub fn default_bits(self) -> Option<u32> {
        self.parameter.default.map(f32::to_bits)
    }
    pub fn numeric_bounds(self) -> Option<(f32, f32, f32)> {
        self.parameter.numeric_bounds()
    }
    pub fn raw_attributes(self) -> &'a BTreeMap<String, String> {
        &self.parameter.raw_attributes
    }
    pub fn source(self) -> &'a SourceRef {
        &self.parameter.source
    }
    pub fn raw_xml(self) -> Option<&'a serde_json::Value> {
        self.parameter.additional.get("raw_xml")
    }
    pub fn choices(self) -> impl Iterator<Item = MetadataChoiceRef<'a>> + 'a {
        self.parameter
            .choices
            .iter()
            .map(|choice| MetadataChoiceRef { choice })
    }
    pub fn presentation(self, raw: f32, module_values: &[f32]) -> ParameterPresentation {
        self.parameter.presentation(raw, module_values)
    }
}
#[derive(Debug, Clone, Copy)]
pub struct MetadataChoiceRef<'a> {
    choice: &'a ChoiceDefinition,
}
impl<'a> MetadataChoiceRef<'a> {
    pub fn id(self) -> i32 {
        self.choice.id
    }
    pub fn name(self) -> &'a str {
        &self.choice.name
    }
    pub fn raw_attributes(self) -> &'a BTreeMap<String, String> {
        &self.choice.raw_attributes
    }
    pub fn source(self) -> &'a SourceRef {
        &self.choice.source
    }
}

/// XML Index evidence has no implicit outgoing interpretation.
///
/// ```compile_fail
/// use gp200_catalog::{CheckedEmbeddedMetadata, ModelKey};
/// use gp200_domain::{EditorMetadataProfile, ModelIndex};
/// let owner = CheckedEmbeddedMetadata::embedded().unwrap();
/// let index = owner.view(EditorMetadataProfile::Gp200Editor160)
///     .model(ModelKey { module: 2, code: 10 }).unwrap().observed_index();
/// let outgoing: ModelIndex = index.into();
/// ```
#[derive(Debug, Clone, Copy)]
pub struct ObservedMetadataModelIndex<'a> {
    value: u16,
    key: ModelKey,
    identity: &'a EmbeddedMetadataSourceIdentity,
    source: &'a SourceRef,
}
impl<'a> ObservedMetadataModelIndex<'a> {
    pub fn observed_value(self) -> u16 {
        self.value
    }
    pub fn key(self) -> ModelKey {
        self.key
    }
    pub fn identity(self) -> &'a EmbeddedMetadataSourceIdentity {
        self.identity
    }
    pub fn source(self) -> &'a SourceRef {
        self.source
    }
}
#[derive(Debug)]
pub struct MetadataIndexPair<'a> {
    key: ModelKey,
    old: ObservedMetadataModelIndex<'a>,
    current: ObservedMetadataModelIndex<'a>,
}
impl<'a> MetadataIndexPair<'a> {
    pub fn key(&self) -> ModelKey {
        self.key
    }
    pub fn old(&self) -> ObservedMetadataModelIndex<'a> {
        self.old
    }
    pub fn current(&self) -> ObservedMetadataModelIndex<'a> {
        self.current
    }
}

/// Explicit local preview only. No conversion to a live model plan/command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedLocalModelDefaults {
    profile: EditorMetadataProfile,
    catalog_sha256: &'static str,
    algorithm_sha256: &'static str,
    key: ModelKey,
    declared: Vec<(u8, u32)>,
    unresolved: Vec<u8>,
}
impl ScopedLocalModelDefaults {
    pub fn profile(&self) -> EditorMetadataProfile {
        self.profile
    }
    pub fn catalog_sha256(&self) -> &str {
        self.catalog_sha256
    }
    pub fn algorithm_sha256(&self) -> &str {
        self.algorithm_sha256
    }
    pub fn key(&self) -> ModelKey {
        self.key
    }
    pub fn declared(&self) -> &[(u8, u32)] {
        &self.declared
    }
    pub fn unresolved_ids(&self) -> &[u8] {
        &self.unresolved
    }
    pub fn apply_to_local_bits(&self, parameters: &mut [u32]) -> Result<(), &'static str> {
        if parameters.len() != 15 || self.declared.iter().any(|(id, _)| *id >= 15) {
            return Err("Local preview requires exactly fifteen raw float slots");
        }
        for &(id, bits) in &self.declared {
            parameters[usize::from(id)] = bits;
        }
        Ok(())
    }
    pub fn apply_to_local_module_body(&self, body: &mut [u8]) -> Result<(), &'static str> {
        if body.len() != 68
            || body[0] != self.key.module
            || u16::from_le_bytes([body[2], body[3]]) != 15
            || self.declared.iter().any(|(id, _)| *id >= 15)
        {
            return Err("Local preview requires the matching native fifteen-slot module shape");
        }
        body[4..8].copy_from_slice(&self.key.code.to_le_bytes());
        for &(id, bits) in &self.declared {
            let start = 8 + usize::from(id) * 4;
            body[start..start + 4].copy_from_slice(&bits.to_le_bytes());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn forged_bytes_or_matching_public_source_labels_cannot_issue_checked_owner() {
        for (old, current, evidence) in [
            (
                format!("{OLD_JSON} "),
                crate::EMBEDDED_CATALOG_JSON.to_owned(),
                EVIDENCE_JSON.to_owned(),
            ),
            (
                OLD_JSON.to_owned(),
                format!("{} ", crate::EMBEDDED_CATALOG_JSON),
                EVIDENCE_JSON.to_owned(),
            ),
            (
                OLD_JSON.to_owned(),
                crate::EMBEDDED_CATALOG_JSON.to_owned(),
                EVIDENCE_JSON.replace("\"schema\":1", "\"schema\":2"),
            ),
        ] {
            assert!(CheckedEmbeddedMetadata::checked_bytes(&old, &current, &evidence).is_err());
        }
    }
}
