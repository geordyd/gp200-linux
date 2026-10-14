//! NAM JSON diagnostics and the evidenced host WAV stage. Network kernels remain separate.
use crate::{MAX_SAMPLES, MAX_SOURCE_BYTES, WaveAudio, WaveEncoding, cancelled, invalid};
use gp200_domain::{ErrorKind, ServiceError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamInfo {
    pub version: String,
    pub architecture: String,
    pub sample_rate: Option<f64>,
    #[serde(default)]
    pub render_sample_rate: Option<u32>,
    pub weight_count: usize,
    #[serde(default)]
    pub operations_per_sample: Option<u64>,
    #[serde(default)]
    pub network_profile: Option<String>,
    #[serde(default)]
    pub size_plan: Option<NamSizePlan>,
    #[serde(default)]
    pub size_selection: Option<NamSizeSelection>,
    #[serde(default = "full_size")]
    pub requested_size: f64,
    pub config: Value,
    pub metadata: Option<Value>,
    pub supported_inference: bool,
    pub reason: String,
}
pub fn inspect_nam(bytes: &[u8]) -> Result<NamInfo, ServiceError> {
    inspect_nam_with_size(bytes, 1.0)
}
fn full_size() -> f64 {
    1.0
}
/// Inspection selects the same validated network used by the render worker.
pub fn inspect_nam_with_size(bytes: &[u8], size: f64) -> Result<NamInfo, ServiceError> {
    if !size.is_finite() || !(0.0..=1.0).contains(&size) {
        return Err(invalid("NAM size must be finite in 0–1"));
    }
    let value = parse_json(bytes)?;
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 128)
            .map(str::to_owned)
            .ok_or_else(|| {
                invalid(format!(
                    "NAM {key} must be a non-empty string of at most 128 bytes"
                ))
            })
    };
    let architecture = text("architecture")?;
    let version = text("version")?;
    let config = value
        .get("config")
        .filter(|v| v.is_object())
        .ok_or_else(|| invalid("NAM config must be an object"))?
        .clone();
    let weights = json_weights(
        &value,
        matches!(architecture.as_str(), "Sequential" | "SlimmableContainer"),
    )?;
    let sample_rate = match value.get("sample_rate") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_f64()
                .filter(|n| n.is_finite() && *n > 0.0 && *n <= 384000.0)
                .ok_or_else(|| invalid("NAM sample_rate must be finite in 1–384000 Hz"))?,
        ),
    };
    let mut budget = crate::wavenet::Budget::default();
    let loaded = load_value(&value, None, &mut budget, 0);
    let size_plan = loaded.as_ref().ok().and_then(NamModel::size_plan);
    let loaded = loaded.and_then(|mut model| {
        model.set_size(size)?;
        Ok(model)
    });
    let render_sample_rate = loaded.as_ref().ok().map(SampleProcessor::sample_rate);
    let size_selection = loaded.as_ref().ok().map(NamModel::size_selection);
    let operations_per_sample = loaded.as_ref().ok().map(NamModel::operations_per_sample);
    let network_profile = loaded
        .as_ref()
        .ok()
        .map(|model| model.architecture().to_owned());
    let (supported_inference, reason) = match loaded {
        Ok(_) => (
            true,
            format!(
                "{architecture} has bounded local mono inference against pinned NAM Core equations. The rendered WAV is not a calibrated SnapTone or an upload artifact. Native conversion equivalence remains unverified."
            ),
        ),
        Err(error) => (false, error.message),
    };
    Ok(NamInfo {
        version,
        architecture,
        sample_rate,
        render_sample_rate,
        weight_count: if supported_inference {
            budget.weights
        } else {
            weights.len()
        },
        operations_per_sample,
        network_profile,
        size_plan,
        size_selection,
        requested_size: size,
        config,
        metadata: value.get("metadata").cloned(),
        supported_inference,
        reason,
    })
}
fn parse_json(bytes: &[u8]) -> Result<Value, ServiceError> {
    if bytes.is_empty() || bytes.len() > MAX_SOURCE_BYTES {
        return Err(invalid(
            "NAM file is empty or exceeds the 64 MiB local limit",
        ));
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| invalid(format!("Invalid NAM JSON: {error}")))?;
    if !value.is_object() {
        return Err(invalid("NAM root must be a JSON object"));
    }
    Ok(value)
}
pub(crate) fn json_weights(value: &Value, allow_empty: bool) -> Result<Vec<f32>, ServiceError> {
    let weights = value
        .get("weights")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("NAM weights must be an array"))?;
    if (weights.is_empty() && !allow_empty) || weights.len() > 4194304 {
        return Err(invalid(
            "NAM weights exceed local bounds or are empty for a network",
        ));
    }
    weights
        .iter()
        .enumerate()
        .map(|(i, v)| {
            v.as_f64()
                .map(|n| n as f32)
                .filter(|n| n.is_finite())
                .ok_or_else(|| invalid(format!("NAM weight {i} must be finite float32")))
        })
        .collect()
}
pub(crate) fn json_rate(value: &Value) -> Result<Option<u32>, ServiceError> {
    match value.get("sample_rate") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 1.0 && *n <= 384000.0 && n.fract() == 0.0)
            .map(|n| Some(n as u32))
            .ok_or_else(|| {
                ServiceError::unsupported(
                    "Local NAM rendering requires an integer sample rate in 1–384000 Hz",
                )
            }),
    }
}
pub(crate) fn check_version(value: &Value) -> Result<(), ServiceError> {
    let version = value
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("NAM version is missing"))?;
    // The author-provided 0.5.2 LSTM benchmark was loaded unchanged by the
    // pinned Core oracle. This exception does not extend other architectures.
    let legacy_lstm = version == "0.5.2" && value["architecture"] == "LSTM";
    if !legacy_lstm && !["0.5.4", "0.6.0", "0.7.0"].contains(&version) {
        return Err(ServiceError::unsupported(format!(
            "NAM schema {version} has no pinned local numerical profile"
        )));
    }
    Ok(())
}
pub(crate) fn load_value(
    value: &Value,
    inherited_rate: Option<u32>,
    budget: &mut crate::wavenet::Budget,
    depth: usize,
) -> Result<NamModel, ServiceError> {
    if depth > 8 {
        return Err(invalid("NAM nested models exceed the local depth limit 8"));
    }
    check_version(value)?;
    let architecture = value
        .get("architecture")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("NAM architecture is missing"))?;
    let config = value
        .get("config")
        .filter(|v| v.is_object())
        .ok_or_else(|| invalid("NAM config must be an object"))?;
    let weights = json_weights(
        value,
        matches!(architecture, "Sequential" | "SlimmableContainer"),
    )?;
    let explicit_rate = json_rate(value)?;
    if inherited_rate
        .zip(explicit_rate)
        .is_some_and(|(a, b)| a != b)
    {
        return Err(invalid("NAM child model sample rate mismatch"));
    }
    let rate = explicit_rate.or(inherited_rate).unwrap_or(48000);
    match architecture {
        "ConvNet" => Ok(NamModel::ConvNet(crate::ConvNetModel::from_json(
            config, &weights, rate, budget,
        )?)),
        "Linear" => Ok(NamModel::Linear(crate::LinearModel::from_json(
            config, &weights, rate, budget,
        )?)),
        "LSTM" => {
            if !matches!(value["version"].as_str(), Some("0.5.2" | "0.5.4")) {
                return Err(ServiceError::unsupported(
                    "Local LSTM profile requires NAM version 0.5.2 or 0.5.4",
                ));
            }
            budget.weights = budget
                .weights
                .checked_add(weights.len())
                .ok_or_else(|| invalid("NAM total weights overflow"))?;
            if budget.weights > 4194304 {
                return Err(invalid("NAM total weights exceed local bounds"));
            }
            Ok(NamModel::Lstm(crate::LstmModel::from_json(
                config, &weights, rate,
            )?))
        }
        "WaveNet"
            if !config["layers"].as_array().is_some_and(|layers| {
                layers
                    .iter()
                    .any(|layer| layer.get("slimmable").is_some_and(|v| !v.is_null()))
            }) =>
        {
            let model = crate::WaveNetModel::from_json(config, &weights, rate, budget, depth)?;
            if model.output_channels() != 1 {
                return Err(ServiceError::unsupported(
                    "Local NAM WAV profile requires one output channel",
                ));
            }
            Ok(NamModel::WaveNet(Box::new(model)))
        }
        "SlimmableWavenet" | "WaveNet" => {
            let mut full = config.get("model").unwrap_or(config).clone();
            if config.get("model").is_some() && config.as_object().expect("object").len() != 1 {
                return Err(invalid(
                    "Wrapped SlimmableWavenet only supports model config",
                ));
            }
            if full.get("head").is_some_and(|v| !v.is_null()) {
                return Err(ServiceError::unsupported(
                    "SlimmableWavenet post-stack head is not supported by the pinned reference",
                ));
            }
            let layers = full
                .get_mut("layers")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| invalid("SlimmableWavenet requires layer arrays"))?;
            let mut any = false;
            let mut allowed_arrays = Vec::new();
            for layer in layers {
                let channels = crate::wavenet::integer(layer, "channels", None, 256)?;
                let mut allowed_channels = Vec::new();
                if let Some(slim) = layer.get("slimmable").filter(|v| !v.is_null()) {
                    allowed_channels = (1..=channels).collect();
                    if slim["method"] != "slice_channels_uniform" {
                        return Err(ServiceError::unsupported(
                            "Unsupported SlimmableWavenet slice method",
                        ));
                    }
                    if slim
                        .as_object()
                        .is_none_or(|o| o.keys().any(|k| k != "method" && k != "kwargs"))
                    {
                        return Err(invalid("Unknown SlimmableWavenet slice field"));
                    }
                    if let Some(kwargs) = slim.get("kwargs") {
                        if kwargs
                            .as_object()
                            .is_none_or(|o| o.keys().any(|k| k != "allowed_channels"))
                        {
                            return Err(invalid("Unknown SlimmableWavenet kwargs"));
                        }
                        if let Some(allowed) = kwargs.get("allowed_channels") {
                            let a = allowed.as_array().ok_or_else(|| {
                                invalid("SlimmableWavenet allowed_channels must be an array")
                            })?;
                            if a.is_empty() || a.len() > 256 {
                                return Err(invalid(
                                    "SlimmableWavenet allowed channels are empty/unbounded",
                                ));
                            }
                            let mut previous = 0;
                            allowed_channels.clear();
                            for n in a {
                                let n=n.as_u64().and_then(|n|usize::try_from(n).ok()).filter(|n|*n>previous&&*n<=channels).ok_or_else(||invalid("SlimmableWavenet allowed channels must strictly increase"))?;
                                previous = n;
                                allowed_channels.push(n);
                            }
                            if previous != channels {
                                return Err(invalid(
                                    "SlimmableWavenet final allowed channels differ from full channel count",
                                ));
                            }
                        }
                    }
                    any = true;
                }
                allowed_arrays.push(allowed_channels);
                layer
                    .as_object_mut()
                    .ok_or_else(|| invalid("SlimmableWavenet layer must be object"))?
                    .remove("slimmable");
            }
            if !any {
                return Err(invalid(
                    "SlimmableWavenet requires at least one slimmable array",
                ));
            }
            let model = crate::WaveNetModel::from_json(&full, &weights, rate, budget, depth)?;
            if model.output_channels() != 1 {
                return Err(ServiceError::unsupported(
                    "SlimmableWavenet local WAV rendering requires one output channel",
                ));
            }
            Ok(NamModel::SlimmableWaveNet(Box::new(
                crate::SlimmableWaveNetModel::new(model, allowed_arrays)?,
            )))
        }
        "SlimmableContainer" => {
            if !weights.is_empty() {
                return Err(invalid("SlimmableContainer top weights must be empty"));
            }
            if config
                .as_object()
                .expect("object")
                .keys()
                .any(|key| key != "submodels")
            {
                return Err(ServiceError::unsupported(
                    "SlimmableContainer config only supports submodels",
                ));
            }
            let children = config["submodels"]
                .as_array()
                .ok_or_else(|| invalid("SlimmableContainer requires submodels array"))?;
            if children.is_empty() || children.len() > 8 {
                return Err(invalid("SlimmableContainer requires 1–8 child models"));
            }
            let mut models = Vec::new();
            let mut thresholds = Vec::new();
            for child in children {
                if child
                    .as_object()
                    .is_none_or(|c| c.keys().any(|k| k != "max_value" && k != "model"))
                {
                    return Err(invalid(
                        "SlimmableContainer entry requires max_value and model only",
                    ));
                }
                let max = child["max_value"]
                    .as_f64()
                    .filter(|n| n.is_finite() && *n > 0.0)
                    .ok_or_else(|| {
                        invalid("SlimmableContainer max_value must be positive and finite")
                    })?;
                if thresholds.last().is_some_and(|old| *old >= max) {
                    return Err(invalid(
                        "SlimmableContainer thresholds must strictly increase",
                    ));
                }
                thresholds.push(max);
                models.push(load_value(&child["model"], Some(rate), budget, depth + 1)?);
            }
            if *thresholds.last().expect("nonempty") < 1.0 {
                return Err(invalid("SlimmableContainer final threshold must cover 1.0"));
            }
            let active = models.len() - 1;
            Ok(NamModel::Container(crate::ContainerModel {
                models,
                thresholds,
                active,
                rate,
            }))
        }
        "Sequential" => {
            if !weights.is_empty() {
                return Err(invalid(
                    "Sequential top-level weights must be empty; weights belong to complete child models",
                ));
            }
            if config
                .as_object()
                .expect("object")
                .keys()
                .any(|key| key != "models")
            {
                return Err(ServiceError::unsupported(
                    "Sequential config only supports the pinned models field",
                ));
            }
            let children = config["models"]
                .as_array()
                .ok_or_else(|| invalid("Sequential requires a models array"))?;
            if children.is_empty() || children.len() > 8 {
                return Err(invalid("Sequential requires 1–8 complete child models"));
            }
            let resolved = explicit_rate.or(inherited_rate).or(children
                .iter()
                .find_map(|child| json_rate(child).ok().flatten()));
            let mut models = Vec::new();
            for child in children {
                models.push(load_value(child, resolved, budget, depth + 1)?);
            }
            let rate = models[0].sample_rate();
            if models.iter().any(|model| model.sample_rate() != rate) {
                return Err(invalid("Sequential child sample rates differ"));
            }
            Ok(NamModel::Sequential(crate::SequentialModel {
                models,
                rate,
            }))
        }
        other => Err(ServiceError::unsupported(format!(
            "NAM architecture {other} has no implemented local kernel"
        ))),
    }
}
/// Native bin rules, carried as bounded metadata so GUI controls need no model loading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NamSizePlan {
    Channels {
        allowed: Vec<Vec<usize>>,
        full: Vec<usize>,
    },
    Children {
        thresholds: Vec<f64>,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NamSizeSelection {
    Channels(Vec<usize>),
    Child(usize),
    Fixed,
}
impl std::fmt::Display for NamSizeSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Channels(channels) => write!(
                f,
                "Channels {}",
                channels
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(" / ")
            ),
            Self::Child(index) => write!(f, "Child {}", index + 1),
            Self::Fixed => write!(f, "Full model"),
        }
    }
}
impl NamSizePlan {
    /// One midpoint per distinct native bin plus the exact full-default selection.
    pub fn choices(&self) -> Vec<(f64, NamSizeSelection)> {
        let valid = match self {
            Self::Channels { allowed, full } => {
                !allowed.is_empty()
                    && allowed.len() <= 16
                    && allowed.len() == full.len()
                    && allowed.iter().zip(full).all(|(allowed, full)| {
                        *full > 0
                            && *full <= 256
                            && allowed.len() <= 256
                            && allowed.iter().all(|value| *value > 0 && *value <= *full)
                            && allowed.windows(2).all(|pair| pair[0] < pair[1])
                            && allowed.last().is_none_or(|last| last == full)
                    })
            }
            Self::Children { thresholds } => {
                !thresholds.is_empty()
                    && thresholds.len() <= 16
                    && thresholds
                        .iter()
                        .all(|value| value.is_finite() && *value > 0.0)
                    && thresholds.windows(2).all(|pair| pair[0] < pair[1])
                    && thresholds.last().is_some_and(|value| *value >= 1.0)
            }
        };
        if !valid {
            return Vec::new();
        }
        let mut boundaries = vec![0.0, 1.0];
        match self {
            Self::Channels { allowed, .. } => {
                for channels in allowed {
                    for index in 1..channels.len() {
                        boundaries.push(index as f64 / channels.len() as f64);
                    }
                }
            }
            Self::Children { thresholds } => boundaries.extend(
                thresholds
                    .iter()
                    .copied()
                    .filter(|value| *value > 0.0 && *value < 1.0),
            ),
        }
        boundaries.sort_by(f64::total_cmp);
        boundaries.dedup();
        let mut choices: Vec<_> = boundaries
            .windows(2)
            .map(|range| {
                let size = (range[0] + range[1]) * 0.5;
                (size, self.selection(size))
            })
            .collect();
        choices.push((1.0, self.selection(1.0)));
        choices
    }
    fn selection(&self, size: f64) -> NamSizeSelection {
        match self {
            Self::Channels { allowed, full } => NamSizeSelection::Channels(
                allowed
                    .iter()
                    .zip(full)
                    .map(|(allowed, full)| {
                        if allowed.is_empty() {
                            *full
                        } else {
                            allowed[((size * allowed.len() as f64).floor() as usize)
                                .min(allowed.len() - 1)]
                        }
                    })
                    .collect(),
            ),
            Self::Children { thresholds } => NamSizeSelection::Child(
                thresholds
                    .iter()
                    .position(|max| size < *max)
                    .unwrap_or(thresholds.len() - 1),
            ),
        }
    }
}
/// Typed network-loading boundary: an unsupported network never becomes a different model.
#[derive(Debug, Clone)]
pub enum NamModel {
    Lstm(crate::LstmModel),
    ConvNet(crate::ConvNetModel),
    Linear(crate::LinearModel),
    WaveNet(Box<crate::WaveNetModel>),
    Sequential(crate::SequentialModel),
    Container(crate::ContainerModel),
    SlimmableWaveNet(Box<crate::SlimmableWaveNetModel>),
}
impl NamModel {
    pub fn load(bytes: &[u8]) -> Result<Self, ServiceError> {
        let value = parse_json(bytes)?;
        load_value(&value, None, &mut crate::wavenet::Budget::default(), 0)
            .map_err(|error| ServiceError::new(ErrorKind::Unverified, error.message))
    }
    pub fn set_activation(&mut self, mode: crate::ActivationMode) {
        match self {
            Self::Lstm(model) => model.set_activation(mode),
            Self::WaveNet(model) => model.set_activation(mode),
            Self::SlimmableWaveNet(model) => model.set_activation(mode),
            Self::ConvNet(model) => model.set_activation(mode),
            Self::Linear(_) => {}
            Self::Sequential(model) => model.set_activation(mode),
            Self::Container(model) => model.set_activation(mode),
        }
    }
    pub fn operations_per_sample(&self) -> u64 {
        match self {
            Self::Lstm(model) => model.operations_per_sample(),
            Self::WaveNet(model) => model.operations_per_sample(),
            Self::SlimmableWaveNet(model) => model.operations_per_sample(),
            Self::ConvNet(model) => model.operations_per_sample(),
            Self::Linear(model) => model.operations_per_sample(),
            Self::Sequential(model) => model.operations_per_sample(),
            Self::Container(model) => model.operations_per_sample(),
        }
    }
    /// Selects an evidenced slim model, resetting causal state only when the selected child changes.
    /// The caller must recompute the work budget after selection.
    pub fn set_size(&mut self, size: f64) -> Result<(), ServiceError> {
        if !size.is_finite() || !(0.0..=1.0).contains(&size) {
            return Err(invalid("NAM size must be finite in 0–1"));
        }
        match self {
            Self::SlimmableWaveNet(model) => model.set_size(size),
            Self::Container(model) => model.set_size(size),
            _ if size == 1.0 => Ok(()),
            _ => Err(ServiceError::unsupported(
                "This NAM architecture has no size-selection profile",
            )),
        }
    }
    pub fn size_plan(&self) -> Option<NamSizePlan> {
        match self {
            Self::SlimmableWaveNet(model) => Some(model.size_plan()),
            Self::Container(model) => Some(NamSizePlan::Children {
                thresholds: model.thresholds.clone(),
            }),
            _ => None,
        }
    }
    pub fn size_selection(&self) -> NamSizeSelection {
        match self {
            Self::SlimmableWaveNet(model) => {
                NamSizeSelection::Channels(model.selected_channels().to_vec())
            }
            Self::Container(model) => NamSizeSelection::Child(model.selected_index()),
            _ => NamSizeSelection::Fixed,
        }
    }
    pub fn estimated_operations(&self, frames: u64) -> Result<u64, ServiceError> {
        frames
            .checked_mul(self.operations_per_sample())
            .ok_or_else(|| invalid("NAM render operation estimate overflows u64"))
    }
    pub fn validate_render_budget(&self, frames: u64, limit: u64) -> Result<u64, ServiceError> {
        if limit == 0 || limit > MAX_NAM_RENDER_OPERATIONS {
            return Err(invalid("NAM max_operations must be 1–1000000000000"));
        }
        let estimate = self.estimated_operations(frames)?;
        if estimate > limit {
            return Err(ServiceError::unsupported(format!(
                "NAM render requires approximately {estimate} matrix multiply-adds, exceeding the selected {limit} limit. Shorten the input or explicitly increase max_operations up to {MAX_NAM_RENDER_OPERATIONS}."
            )));
        }
        Ok(estimate)
    }
    pub fn architecture(&self) -> &'static str {
        match self {
            Self::Lstm(_) => "LSTM",
            Self::WaveNet(_) => "WaveNet",
            Self::SlimmableWaveNet(model) if model.is_full_size() => "SlimmableWavenet (full-size)",
            Self::SlimmableWaveNet(_) => "SlimmableWavenet (selected channels)",
            Self::ConvNet(_) => "ConvNet",
            Self::Linear(_) => "Linear (direct causal FIR)",
            Self::Sequential(_) => "Sequential",
            Self::Container(_) => "SlimmableContainer",
        }
    }
}
impl SampleProcessor for NamModel {
    fn sample_rate(&self) -> u32 {
        match self {
            Self::Lstm(model) => model.sample_rate(),
            Self::WaveNet(model) => model.sample_rate(),
            Self::SlimmableWaveNet(model) => model.sample_rate(),
            Self::ConvNet(model) => model.sample_rate(),
            Self::Linear(model) => model.sample_rate(),
            Self::Sequential(model) => model.sample_rate(),
            Self::Container(model) => model.sample_rate(),
        }
    }
    fn reset(&mut self) -> Result<(), ServiceError> {
        match self {
            Self::Lstm(model) => model.reset(),
            Self::WaveNet(model) => model.reset(),
            Self::SlimmableWaveNet(model) => model.reset(),
            Self::ConvNet(model) => model.reset(),
            Self::Linear(model) => model.reset(),
            Self::Sequential(model) => model.reset(),
            Self::Container(model) => model.reset(),
        }
    }
    fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), ServiceError> {
        match self {
            Self::Lstm(model) => model.process(input, output),
            Self::WaveNet(model) => model.process(input, output),
            Self::SlimmableWaveNet(model) => model.process(input, output),
            Self::ConvNet(model) => model.process(input, output),
            Self::Linear(model) => model.process(input, output),
            Self::Sequential(model) => model.process(input, output),
            Self::Container(model) => model.process(input, output),
        }
    }
}
pub const DEFAULT_NAM_RENDER_OPERATIONS: u64 = 2_000_000_000;
pub const MAX_NAM_RENDER_OPERATIONS: u64 = 1_000_000_000_000;

/// A tested kernel can implement this after its NAM schema and weight layout are proved.
pub trait SampleProcessor {
    fn sample_rate(&self) -> u32;
    fn reset(&mut self) -> Result<(), ServiceError>;
    fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), ServiceError>;
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderReport {
    pub frames: u64,
    pub blocks: u64,
    pub clamped_samples: u64,
    pub source_rate: u32,
    pub output_rate: u32,
    pub host_profile: String,
}
/// Reproduces only the visible block/clamp/scale stage in native 0x494af0 and duplicated
/// stereo output in 0x4946f0. Input resampling, warmup, fast-tanh, calibration and SnapTone
/// serialization are explicitly outside this helper and are not inferred from this stage.
pub fn render_host_stage(
    input_mono: &WaveAudio,
    processor: &mut impl SampleProcessor,
    is_cancelled: impl Fn() -> bool,
    mut progress: impl FnMut(u64, u64),
) -> Result<(WaveAudio, RenderReport), ServiceError> {
    input_mono.validate()?;
    if input_mono.channels != 1 || processor.sample_rate() == 0 || processor.sample_rate() > 384_000
    {
        return Err(invalid(
            "Host render requires explicitly prepared mono input and a bounded processor sample rate",
        ));
    }
    if input_mono
        .samples
        .len()
        .checked_mul(2)
        .is_none_or(|n| n > MAX_SAMPLES)
    {
        return Err(invalid("Stereo host output exceeds the local sample limit"));
    }
    if input_mono.sample_rate != processor.sample_rate() {
        return Err(ServiceError::new(
            ErrorKind::Unverified,
            "Input/model sample rates differ. Original input resampling is not established; select an explicit local preparation before rendering.",
        ));
    }
    if is_cancelled() {
        return Err(cancelled());
    }
    processor.reset()?;
    let mut samples = Vec::with_capacity(input_mono.samples.len() * 2);
    let mut blocks = 0;
    let mut clamped = 0;
    let total = input_mono.samples.len() as u64;
    for input in input_mono.samples.chunks(1024) {
        if is_cancelled() {
            return Err(cancelled());
        }
        let mut output = [0.0_f32; 1024];
        processor.process(input, &mut output[..input.len()])?;
        for value in &output[..input.len()] {
            if !value.is_finite() {
                return Err(invalid(
                    "NAM processor produced a non-finite sample; conversion rejected",
                ));
            }
            if value.abs() > 1.0 {
                clamped += 1;
            }
            let value = value.clamp(-1.0, 1.0) * 0.31_f32;
            samples.extend_from_slice(&[value, value]);
        }
        blocks += 1;
        progress((samples.len() / 2) as u64, total);
    }
    let report = RenderReport {
        frames: total,
        blocks,
        clamped_samples: clamped,
        source_rate: input_mono.sample_rate,
        output_rate: processor.sample_rate(),
        host_profile: "gp200-1.8.1-static-host-stage-v1; not a SnapTone upload profile".into(),
    };
    Ok((
        WaveAudio {
            sample_rate: processor.sample_rate(),
            channels: 2,
            source_encoding: WaveEncoding::Pcm,
            source_bits: 16,
            samples,
        },
        report,
    ))
}
