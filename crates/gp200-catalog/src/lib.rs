//! The shipped editor's effect/control metadata, independent of MIDI or egui.
//!
//! XML parameter IDs are the sparse raw value keys. Display indices are only
//! order; callers must never index live values by a control's display index.
//! Formatting describes local catalog semantics and grants no device write
//! capability. Unknown conversions remain raw and read-only.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

pub mod adapters;
mod firmware_metadata;
pub use firmware_metadata::*;
mod model_defaults;
pub use adapters::{
    ControlError, ControlRequest, ControlSemantics, ControlWritePlan, OrdinaryRestore,
    ParameterWriteStep, SYNC_SUBDIVISIONS, SentinelKind, control_semantics, plan_control_request,
    recognized_sentinel, sentinel_value, subdivision_index, sync_value_parameter,
};
pub use model_defaults::ModelDefaultPlan;

pub const MODULE_NAMES: [&str; 11] = [
    "PRE", "WAH", "DST", "AMP", "NR", "CAB", "EQ", "MOD", "DLY", "RVB", "VOL",
];
pub const EMBEDDED_CATALOG_JSON: &str = include_str!("../../../assets/catalog/gp200.json");

#[derive(Debug)]
pub enum CatalogError {
    Json(serde_json::Error),
    Invalid(String),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => write!(f, "Catalog JSON: {error}"),
            Self::Invalid(message) => write!(f, "Invalid catalog: {message}"),
        }
    }
}

impl Error for CatalogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Json(error) => Some(error),
            Self::Invalid(_) => None,
        }
    }
}

impl From<serde_json::Error> for CatalogError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceRef {
    /// Portable evidence path, relative to the project root.
    pub file: String,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(flatten)]
    pub additional: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogData {
    pub schema_version: u32,
    pub variant: String,
    pub scope: String,
    pub provenance: serde_json::Value,
    pub model_definition_count: usize,
    pub parameter_control_count: usize,
    pub modules: Vec<ModuleDefinition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleDefinition {
    pub name: String,
    pub catalog_position: u8,
    pub source: SourceRef,
    pub models: Vec<ModelDefinition>,
}

impl ModuleDefinition {
    /// Recovered Normal-mode module identity, independent of XML ordering.
    pub fn module_id(&self) -> Option<u8> {
        module_id(&self.name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDefinition {
    pub name: String,
    pub module: String,
    pub code: u32,
    pub display_index: u16,
    /// XML `Index`, traced to cmd0005's model identifier; distinct from Code.
    /// Derived during catalog validation from the retained Index attribute.
    #[serde(skip)]
    pub xml_index: u16,
    pub raw_attributes: BTreeMap<String, String>,
    pub source: SourceRef,
    pub parameters: Vec<ParameterDefinition>,
    #[serde(default)]
    pub english_descriptions: Vec<DescriptionDefinition>,
    #[serde(flatten)]
    pub additional: BTreeMap<String, serde_json::Value>,
}

impl ModelDefinition {
    pub fn key(&self) -> ModelKey {
        // Catalog validation establishes this before exposing a definition.
        ModelKey {
            module: module_id(&self.module).unwrap_or(u8::MAX),
            code: self.code,
        }
    }

    pub fn parameter(&self, xml_id: u8) -> Option<&ParameterDefinition> {
        self.parameters
            .iter()
            .find(|parameter| parameter.parameter_id == xml_id)
    }

    pub fn parameters_in_display_order(&self) -> Vec<&ParameterDefinition> {
        let mut parameters: Vec<_> = self.parameters.iter().collect();
        parameters.sort_by_key(|parameter| parameter.display_index);
        parameters
    }

    pub fn description(&self) -> Option<&str> {
        self.english_descriptions
            .first()
            .map(|description| description.text.as_str())
    }

    /// Sparse defaults: no invented zeros for undefined float slots and no
    /// guessed value for malformed defaults such as Detune "-25 Cents".
    pub fn default_values(&self) -> BTreeMap<u8, f32> {
        self.parameters
            .iter()
            .filter_map(|parameter| {
                parameter
                    .default
                    .map(|value| (parameter.parameter_id, value))
            })
            .collect()
    }

    /// Applying declared defaults retains every unspecified raw float bit.
    pub fn apply_declared_defaults(&self, parameters: &mut [u32]) {
        for (id, value) in self.default_values() {
            if let Some(destination) = parameters.get_mut(usize::from(id)) {
                *destination = value.to_bits();
            }
        }
    }

    pub fn slot_kind(&self) -> SlotKind {
        match (self.module.as_str(), self.code) {
            ("CAB", code @ 0x0a10_0000..=0x0a10_0013) => SlotKind::UserIr {
                slot: (code - 0x0a10_0000) as u8,
            },
            ("AMP" | "DST", code @ 0x0f00_0000..=0x0f00_0009) => SlotKind::SnapTone {
                slot: (code - 0x0f00_0000) as u8,
            },
            _ => SlotKind::Factory,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlotKind {
    Factory,
    UserIr { slot: u8 },
    SnapTone { slot: u8 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParameterDefinition {
    pub ui_kind: String,
    pub name: String,
    pub display_index: u8,
    pub parameter_id: u8,
    pub default: Option<f32>,
    #[serde(default)]
    pub minimum: Option<f32>,
    #[serde(default)]
    pub maximum: Option<f32>,
    #[serde(default)]
    pub step: Option<f32>,
    #[serde(default)]
    pub type_code: Option<u8>,
    #[serde(default)]
    pub value_type_code: Option<u8>,
    #[serde(default)]
    pub bound_parameter_id: Option<u8>,
    #[serde(default)]
    pub suffix: Option<String>,
    #[serde(default)]
    pub minimum_is_off_raw: Option<String>,
    #[serde(default)]
    pub sub_type_raw: Option<String>,
    #[serde(default)]
    pub choices: Vec<ChoiceDefinition>,
    pub raw_attributes: BTreeMap<String, String>,
    pub source: SourceRef,
    #[serde(flatten)]
    pub additional: BTreeMap<String, serde_json::Value>,
}

impl ParameterDefinition {
    pub fn control_kind(&self) -> ControlKind {
        match self.ui_kind.as_str() {
            "Knob" | "Slider" => ControlKind::Numeric,
            "Combox" => ControlKind::Choice,
            "Switch" => ControlKind::Switch,
            _ => ControlKind::Unknown,
        }
    }

    pub fn raw_default(&self) -> Option<&str> {
        self.raw_attributes.get("default").map(String::as_str)
    }

    pub fn choice(&self, value: f32) -> Option<&ChoiceDefinition> {
        self.choices.iter().find(|choice| value == choice.id as f32)
    }

    pub fn numeric_bounds(&self) -> Option<(f32, f32, f32)> {
        Some((self.minimum?, self.maximum?, self.step.unwrap_or(1.0)))
    }

    /// This is a local editing policy, never proof of a verified write format.
    pub fn presentation(&self, raw: f32, module_values: &[f32]) -> ParameterPresentation {
        format_value(self, raw, module_values)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlKind {
    Numeric,
    Choice,
    Switch,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChoiceDefinition {
    pub name: String,
    pub id: i32,
    pub raw_attributes: BTreeMap<String, String>,
    pub source: SourceRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescriptionDefinition {
    pub text: String,
    pub name: String,
    pub index: u16,
    pub raw_attributes: BTreeMap<String, String>,
    pub source: SourceRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ModelKey {
    pub module: u8,
    pub code: u32,
}

/// Persistent favorites use module plus code, never name or display index.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Favorites {
    keys: BTreeSet<ModelKey>,
}

impl Favorites {
    pub fn contains(&self, key: ModelKey) -> bool {
        self.keys.contains(&key)
    }
    pub fn set(&mut self, key: ModelKey, favorite: bool) {
        if favorite {
            self.keys.insert(key);
        } else {
            self.keys.remove(&key);
        }
    }
    pub fn toggle(&mut self, key: ModelKey) -> bool {
        let selected = !self.contains(key);
        self.set(key, selected);
        selected
    }
    pub fn keys(&self) -> impl Iterator<Item = ModelKey> + '_ {
        self.keys.iter().copied()
    }
}

#[derive(Debug, Clone)]
pub struct Catalog {
    data: CatalogData,
    module_index: BTreeMap<String, usize>,
    model_index: BTreeMap<ModelKey, (usize, usize)>,
}

impl Catalog {
    pub fn embedded() -> Result<Self, CatalogError> {
        Self::from_json(EMBEDDED_CATALOG_JSON)
    }

    pub fn from_json(json: &str) -> Result<Self, CatalogError> {
        let data: CatalogData = serde_json::from_str(json)?;
        Self::from_data(data)
    }

    pub fn from_data(mut data: CatalogData) -> Result<Self, CatalogError> {
        if data.schema_version != 1 || data.variant != "GP-200" {
            return invalid("Unsupported catalog schema or device variant");
        }
        let mut module_index = BTreeMap::new();
        let mut model_index = BTreeMap::new();
        let mut control_count = 0;
        for (module_position, module) in data.modules.iter_mut().enumerate() {
            let Some(id) = module_id(&module.name) else {
                return invalid(format!("Unknown module {}", module.name));
            };
            if module_index
                .insert(module.name.clone(), module_position)
                .is_some()
            {
                return invalid(format!("Duplicate module {}", module.name));
            }
            for (model_position, model) in module.models.iter_mut().enumerate() {
                if model.module != module.name {
                    return invalid(format!("Model {} belongs to another module", model.name));
                }
                let key = ModelKey {
                    module: id,
                    code: model.code,
                };
                if model_index
                    .insert(key, (module_position, model_position))
                    .is_some()
                {
                    return invalid(format!(
                        "Duplicate model key {} 0x{:08x}",
                        model.module, model.code
                    ));
                }
                model.xml_index = model.display_index;
                validate_model(model)?;
                control_count += model.parameters.len();
            }
        }
        if module_index.len() != MODULE_NAMES.len() {
            return invalid("The GP-200 catalog must describe all eleven modules");
        }
        if model_index.len() != data.model_definition_count
            || control_count != data.parameter_control_count
        {
            return invalid("Declared catalog counts differ from parsed definitions");
        }
        Ok(Self {
            data,
            module_index,
            model_index,
        })
    }

    pub fn data(&self) -> &CatalogData {
        &self.data
    }
    pub fn modules(&self) -> &[ModuleDefinition] {
        &self.data.modules
    }
    pub fn model_count(&self) -> usize {
        self.model_index.len()
    }
    pub fn parameter_count(&self) -> usize {
        self.data.parameter_control_count
    }
    pub fn variant(&self) -> &str {
        &self.data.variant
    }

    pub fn module(&self, name: &str) -> Option<&ModuleDefinition> {
        self.module_index
            .get(name)
            .map(|position| &self.data.modules[*position])
    }

    pub fn module_by_id(&self, id: u8) -> Option<&ModuleDefinition> {
        self.module(module_name(id)?)
    }

    pub fn model(&self, module: &str, code: u32) -> Option<&ModelDefinition> {
        self.model_by_id(module_id(module)?, code)
    }

    pub fn model_by_id(&self, module: u8, code: u32) -> Option<&ModelDefinition> {
        self.model_by_key(ModelKey { module, code })
    }

    pub fn model_by_key(&self, key: ModelKey) -> Option<&ModelDefinition> {
        self.model_index
            .get(&key)
            .map(|(module, model)| &self.data.modules[*module].models[*model])
    }

    /// Case-insensitive, all-words search across names, module, descriptions,
    /// parameter names and the hexadecimal code. Stable catalog ordering.
    pub fn search_models(&self, module: Option<u8>, query: &str) -> Vec<&ModelDefinition> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        self.data
            .modules
            .iter()
            .filter(|candidate| module.is_none_or(|id| candidate.module_id() == Some(id)))
            .flat_map(|candidate| candidate.models.iter())
            .filter(|model| {
                let text = format!(
                    "{} {} {:08x} {} {}",
                    model.module,
                    model.name,
                    model.code,
                    model
                        .english_descriptions
                        .iter()
                        .map(|description| description.text.as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                    model
                        .parameters
                        .iter()
                        .map(|parameter| parameter.name.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                )
                .to_lowercase();
                words.iter().all(|word| text.contains(word))
            })
            .collect()
    }

    pub fn favorite_models(&self, favorites: &Favorites) -> Vec<&ModelDefinition> {
        favorites
            .keys()
            .filter_map(|key| self.model_by_key(key))
            .collect()
    }

    /// Standalone formatting without the referenced module context remains
    /// read-only for bound controls. Use `format_value` for a full module view.
    pub fn format_value(&self, parameter: &ParameterDefinition, raw: f32) -> ParameterPresentation {
        format_value(parameter, raw, &[])
    }
}

pub fn module_id(name: &str) -> Option<u8> {
    MODULE_NAMES
        .iter()
        .position(|candidate| *candidate == name)
        .map(|id| id as u8)
}

pub fn module_name(id: u8) -> Option<&'static str> {
    MODULE_NAMES.get(usize::from(id)).copied()
}

fn invalid<T>(message: impl Into<String>) -> Result<T, CatalogError> {
    Err(CatalogError::Invalid(message.into()))
}

fn parse_integral_attribute(text: &str) -> Option<u32> {
    let number: f64 = text.parse().ok()?;
    if number.is_finite() && number >= 0.0 && number <= f64::from(u32::MAX) && number.fract() == 0.0
    {
        Some(number as u32)
    } else {
        None
    }
}

fn validate_model(model: &ModelDefinition) -> Result<(), CatalogError> {
    if model
        .raw_attributes
        .get("Code")
        .and_then(|value| parse_integral_attribute(value))
        != Some(model.code)
    {
        return invalid(format!("{} has inconsistent Code attribute", model.name));
    }
    if model
        .raw_attributes
        .get("Index")
        .and_then(|value| parse_integral_attribute(value))
        != Some(u32::from(model.xml_index))
    {
        return invalid(format!("{} has inconsistent XML Index", model.name));
    }
    let mut ids = BTreeSet::new();
    let mut indices = BTreeSet::new();
    for parameter in &model.parameters {
        if parameter.parameter_id >= 15 || !ids.insert(parameter.parameter_id) {
            return invalid(format!(
                "{} has an invalid/duplicate parameter ID",
                model.name
            ));
        }
        if !indices.insert(parameter.display_index) {
            return invalid(format!("{} has duplicate UI order indices", model.name));
        }
        if parameter
            .raw_attributes
            .get("ID")
            .and_then(|value| parse_integral_attribute(value))
            != Some(u32::from(parameter.parameter_id))
        {
            return invalid(format!(
                "{} {} has inconsistent ID attribute",
                model.name, parameter.name
            ));
        }
        if let (Some(minimum), Some(maximum)) = (parameter.minimum, parameter.maximum) {
            if !minimum.is_finite() || !maximum.is_finite() || minimum > maximum {
                return invalid(format!(
                    "{} {} has invalid bounds",
                    model.name, parameter.name
                ));
            }
            if parameter
                .default
                .is_some_and(|value| !value.is_finite() || value < minimum || value > maximum)
            {
                return invalid(format!(
                    "{} {} default is outside its bounds",
                    model.name, parameter.name
                ));
            }
        }
        if parameter
            .step
            .is_some_and(|step| !step.is_finite() || step <= 0.0)
        {
            return invalid(format!(
                "{} {} has invalid step",
                model.name, parameter.name
            ));
        }
        let mut choice_ids = BTreeSet::new();
        for choice in &parameter.choices {
            if !choice_ids.insert(choice.id) {
                return invalid(format!(
                    "{} {} has duplicate choice IDs",
                    model.name, parameter.name
                ));
            }
        }
        if !parameter.choices.is_empty()
            && parameter
                .default
                .is_some_and(|default| parameter.choice(default).is_none())
        {
            return invalid(format!(
                "{} {} default has no declared choice",
                model.name, parameter.name
            ));
        }
    }
    for description in &model.english_descriptions {
        if description
            .raw_attributes
            .get("ID")
            .and_then(|id| parse_integral_attribute(id))
            != Some(model.code)
        {
            return invalid(format!(
                "{} description is joined to another model code",
                model.name
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentationState {
    Number,
    Choice,
    SyncedRaw,
    SyncSubdivision,
    SentinelRaw,
    Sentinel,
    UnverifiedTransform,
    MissingBinding,
    UnknownBinding,
    UnknownChoice,
    OutOfRange,
    NonFinite,
    UnsupportedControl,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParameterPresentation {
    pub text: String,
    /// Retains NaN payloads and signed zero; do not round-trip through text.
    pub raw_bits: u32,
    pub state: PresentationState,
    /// Local draft editing only. The transport capability gate is independent.
    pub editable: bool,
    pub explanation: Option<String>,
}

fn raw_presentation(
    raw: f32,
    state: PresentationState,
    explanation: &str,
) -> ParameterPresentation {
    ParameterPresentation {
        text: if raw.is_finite() {
            format!("Raw {raw}")
        } else {
            format!("Raw bits 0x{:08x}", raw.to_bits())
        },
        raw_bits: raw.to_bits(),
        state,
        editable: false,
        explanation: Some(explanation.to_owned()),
    }
}

/// Format using XML IDs into the full raw module parameter array.
/// Static recovered presentation. It never grants hardware write authority;
/// raw bits remain authoritative even when a label truncates decimals.
pub fn format_value(
    parameter: &ParameterDefinition,
    raw: f32,
    module_values: &[f32],
) -> ParameterPresentation {
    if !raw.is_finite() {
        return raw_presentation(
            raw,
            PresentationState::NonFinite,
            "Non-finite raw value is preserved exactly.",
        );
    }
    if parameter.value_type_code == Some(1) {
        let Some(binding) = parameter.bound_parameter_id else {
            return raw_presentation(
                raw,
                PresentationState::MissingBinding,
                "Sync binding is absent; conversion remains unverified.",
            );
        };
        match module_values.get(usize::from(binding)).copied() {
            Some(1.0) => {
                return match subdivision_index(raw) {
                    Some(index) => ParameterPresentation {
                        text: SYNC_SUBDIVISIONS[usize::from(index)].to_owned(),
                        raw_bits: raw.to_bits(),
                        state: PresentationState::SyncSubdivision,
                        editable: true,
                        explanation: None,
                    },
                    None => raw_presentation(
                        raw,
                        PresentationState::OutOfRange,
                        "Synced raw value is outside 0..10; bits are preserved without clamping.",
                    ),
                };
            }
            Some(0.0) => {}
            Some(_) => {
                return raw_presentation(
                    raw,
                    PresentationState::UnknownBinding,
                    "The bound Sync value is unknown; it is preserved without guessing its mode.",
                );
            }
            None => {
                return raw_presentation(
                    raw,
                    PresentationState::MissingBinding,
                    "The referenced Sync parameter is missing; conversion needs the complete module context.",
                );
            }
        }
    }
    // Mode +1a4's piecewise frequency display has no established catalog
    // applicability. XML valueType=2 uses the direct raw number after its
    // maximum Off check; it is not that separate formatter mode.
    if parameter.value_type_code.is_some_and(|kind| kind > 3) {
        return raw_presentation(
            raw,
            PresentationState::UnverifiedTransform,
            "This control declares an unresolved value transform or sentinel; its raw value is preserved.",
        );
    }
    if let Some(kind) = recognized_sentinel(parameter, raw) {
        return ParameterPresentation {
            text: match kind {
                SentinelKind::Off => "OFF",
                SentinelKind::Mute => "Mute",
            }
            .to_owned(),
            raw_bits: raw.to_bits(),
            state: PresentationState::Sentinel,
            editable: true,
            explanation: None,
        };
    }
    if matches!(
        parameter.control_kind(),
        ControlKind::Choice | ControlKind::Switch
    ) {
        return match parameter.choice(raw) {
            Some(choice) => ParameterPresentation {
                text: choice.name.clone(),
                raw_bits: raw.to_bits(),
                state: PresentationState::Choice,
                editable: true,
                explanation: None,
            },
            None => raw_presentation(
                raw,
                PresentationState::UnknownChoice,
                "No declared menu choice matches this raw value.",
            ),
        };
    }
    if parameter.control_kind() != ControlKind::Numeric {
        return raw_presentation(
            raw,
            PresentationState::UnsupportedControl,
            "The catalog control type is unknown.",
        );
    }
    let Some((minimum, maximum, _)) = parameter.numeric_bounds() else {
        return raw_presentation(
            raw,
            PresentationState::UnsupportedControl,
            "The numeric control has no complete declared bounds.",
        );
    };
    if raw < minimum || raw > maximum {
        return raw_presentation(
            raw,
            PresentationState::OutOfRange,
            "The raw value is outside the declared range and is retained without clamping.",
        );
    }
    let display = match parameter.value_type_code {
        Some(2) => f64::from(raw),
        Some(3) => (f64::from(raw) * 10.0).trunc() / 10.0,
        _ => (f64::from(raw) * 100.0).trunc() / 100.0,
    };
    let text = match parameter.suffix.as_deref() {
        Some(unit) if !unit.is_empty() => format!("{display} {unit}"),
        _ => display.to_string(),
    };
    ParameterPresentation {
        text,
        raw_bits: raw.to_bits(),
        state: PresentationState::Number,
        editable: true,
        explanation: None,
    }
}
