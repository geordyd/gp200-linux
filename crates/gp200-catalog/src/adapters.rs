//! Pure display and requested-write semantics recovered from the editor.
//!
//! Plans contain no device authorization. A transport profile independently
//! validates backend, model, epoch, context and proof before executing them.

use crate::{ControlKind, ModelDefinition, ModelKey, ParameterDefinition};
use std::error::Error;
use std::fmt;

pub const SYNC_SUBDIVISIONS: [&str; 11] = [
    "1/1", "1/2", "1/2D", "1/2T", "1/4", "1/4D", "1/4T", "1/8", "1/8D", "1/8T", "1/16",
];
/// Strict comparison threshold in the original double formatter.
pub const SENTINEL_EPSILON: f64 = 0.00003;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentinelKind {
    Off,
    Mute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlSemantics {
    Numeric,
    Choice,
    Switch,
    /// Ordinary numeric units when Power=0; eleven subdivisions when Power=1.
    SyncNumeric {
        power_parameter: u8,
    },
    /// Power's own declared binding is retained as a hint. Three shipped DLY
    /// entries use display idx here; the native callback resolves the actual
    /// value control by reverse lookup of the numeric knob's bind/XML ID.
    SyncPower {
        value_parameter: u8,
    },
    Unsupported,
}

pub fn control_semantics(parameter: &ParameterDefinition) -> ControlSemantics {
    if parameter.sub_type_raw.as_deref() == Some("Power") {
        return parameter
            .bound_parameter_id
            .map_or(ControlSemantics::Unsupported, |value_parameter| {
                ControlSemantics::SyncPower { value_parameter }
            });
    }
    if parameter.value_type_code == Some(1) {
        return parameter
            .bound_parameter_id
            .map_or(ControlSemantics::Unsupported, |power_parameter| {
                ControlSemantics::SyncNumeric { power_parameter }
            });
    }
    if parameter.value_type_code.is_some_and(|kind| kind > 3) {
        return ControlSemantics::Unsupported;
    }
    match parameter.control_kind() {
        ControlKind::Numeric => ControlSemantics::Numeric,
        ControlKind::Choice => ControlSemantics::Choice,
        ControlKind::Switch => ControlSemantics::Switch,
        ControlKind::Unknown => ControlSemantics::Unsupported,
    }
}

/// The integer callback scans knobs for knob.bind == changed Power XML ID.
/// It does not trust Power.bind. Ambiguous or missing reverse links fail.
pub fn sync_value_parameter(model: &ModelDefinition, power_id: u8) -> Result<u8, ControlError> {
    let mut values = model.parameters.iter().filter(|parameter| {
        control_semantics(parameter)
            == ControlSemantics::SyncNumeric {
                power_parameter: power_id,
            }
    });
    let id = values
        .next()
        .ok_or(ControlError::MissingBinding)?
        .parameter_id;
    if values.next().is_some() {
        return Err(ControlError::MissingBinding);
    }
    Ok(id)
}

fn enabled_attribute(parameter: &ParameterDefinition, name: &str) -> bool {
    parameter
        .raw_attributes
        .get(name)
        .and_then(|s| s.parse::<f64>().ok())
        .is_some_and(|value| value.is_finite() && value != 0.0)
}

pub fn sentinel_value(parameter: &ParameterDefinition, kind: SentinelKind) -> Option<f32> {
    match kind {
        SentinelKind::Off if enabled_attribute(parameter, "MinISOff") => {
            if parameter.value_type_code == Some(2) {
                parameter.maximum
            } else {
                parameter.minimum
            }
        }
        SentinelKind::Mute if enabled_attribute(parameter, "MinISMute") => parameter.minimum,
        _ => None,
    }
}

pub fn recognized_sentinel(parameter: &ParameterDefinition, raw: f32) -> Option<SentinelKind> {
    if !raw.is_finite() {
        return None;
    }
    // The original ordinary Off test precedes Mute, then type=2 maximum Off.
    let order = if parameter.value_type_code == Some(2) {
        [SentinelKind::Mute, SentinelKind::Off]
    } else {
        [SentinelKind::Off, SentinelKind::Mute]
    };
    order.into_iter().find(|kind| {
        sentinel_value(parameter, *kind)
            .is_some_and(|value| (f64::from(raw) - f64::from(value)).abs() < SENTINEL_EPSILON)
    })
}

/// Native midpoint buckets, including exact ties in the preceding bucket.
/// The active sync range is exactly 0..10, not the ordinary XML range.
pub fn subdivision_index(raw: f32) -> Option<u8> {
    if !raw.is_finite() || !(0.0..=10.0).contains(&raw) {
        return None;
    }
    Some((f64::from(raw) - 0.5).ceil().clamp(0.0, 10.0) as u8)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlRequest {
    /// Already in device units; float bits are never regenerated from text.
    RawBits(u32),
    Choice(i32),
    Subdivision(u8),
    Sentinel(SentinelKind),
    /// Power controls use this explicit action instead of an isolated raw edit.
    Sync {
        enabled: bool,
        ordinary_restore: Option<OrdinaryRestore>,
    },
}

/// The value supplied on Sync Off must have an explicit origin. Initially
/// synced snapshots contain a subdivision and cannot reveal a prior Hz/ms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrdinaryRestore {
    ObservedInSameContext(u32),
    ExplicitUserValue(u32),
}
impl OrdinaryRestore {
    pub fn bits(self) -> u32 {
        match self {
            Self::ObservedInSameContext(bits) | Self::ExplicitUserValue(bits) => bits,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParameterWriteStep {
    pub parameter_id: u8,
    pub raw_bits: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlWritePlan {
    pub model: ModelKey,
    /// cmd0005 uses this Index; cmd0014/model selection use model.code.
    pub model_xml_index: u16,
    pub before_values: Vec<u32>,
    /// Separate ordered cmd0005 writes. Do not reorder/coalesce a sync pair.
    pub steps: Vec<ParameterWriteStep>,
    /// Requested-write projections after each step, including tails. Device
    /// automatic paired changes may differ at an intermediate step; validate
    /// the complete final snapshot after the ordered sequence.
    pub expected_after_steps: Vec<Vec<u32>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlError {
    UnknownParameter(u8),
    MissingValue(u8),
    Unsupported,
    InvalidValue,
    UnknownChoice,
    MissingBinding,
    UnknownSyncState,
    ExplicitSyncActionRequired,
    MissingOrdinaryRestore,
}
impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnknownParameter(_) => "Control ID is absent from this model",
            Self::MissingValue(_) => "Raw module context is incomplete",
            Self::Unsupported => "Control semantics are not recovered",
            Self::InvalidValue => "Value is non-finite or outside the declared mode range",
            Self::UnknownChoice => "Value is not a declared menu choice",
            Self::MissingBinding => "Sync binding is absent or not reciprocal",
            Self::UnknownSyncState => "Sync state is not a declared OFF/ON value",
            Self::ExplicitSyncActionRequired => "Sync changes require an explicit paired action",
            Self::MissingOrdinaryRestore => {
                "Choose an ordinary value or supply an observed value from this context"
            }
        })
    }
}
impl Error for ControlError {}

fn ordinary_valid(parameter: &ParameterDefinition, bits: u32) -> Result<(), ControlError> {
    let value = f32::from_bits(bits);
    let (minimum, maximum, _) = parameter
        .numeric_bounds()
        .ok_or(ControlError::Unsupported)?;
    if value.is_finite() && (minimum..=maximum).contains(&value) {
        Ok(())
    } else {
        Err(ControlError::InvalidValue)
    }
}

/// Validate a user request using a complete immutable module context. Read-only
/// projection never calls this function and never produces writes.
pub fn plan_control_request(
    model: &ModelDefinition,
    parameter_id: u8,
    module_bits: &[u32],
    request: ControlRequest,
) -> Result<ControlWritePlan, ControlError> {
    let parameter = model
        .parameter(parameter_id)
        .ok_or(ControlError::UnknownParameter(parameter_id))?;
    if module_bits.get(usize::from(parameter_id)).is_none() {
        return Err(ControlError::MissingValue(parameter_id));
    }
    let mut steps = Vec::new();
    match control_semantics(parameter) {
        ControlSemantics::SyncPower { .. } => {
            let value_parameter = sync_value_parameter(model, parameter_id)?;
            let bound = model
                .parameter(value_parameter)
                .ok_or(ControlError::MissingBinding)?;
            if control_semantics(bound)
                != (ControlSemantics::SyncNumeric {
                    power_parameter: parameter_id,
                })
            {
                return Err(ControlError::MissingBinding);
            }
            if module_bits.get(usize::from(value_parameter)).is_none() {
                return Err(ControlError::MissingValue(value_parameter));
            }
            let old = f32::from_bits(module_bits[usize::from(parameter_id)]);
            if old != 0.0 && old != 1.0 {
                return Err(ControlError::UnknownSyncState);
            }
            let ControlRequest::Sync {
                enabled,
                ordinary_restore,
            } = request
            else {
                return Err(ControlError::ExplicitSyncActionRequired);
            };
            // Every explicit action follows the native pair, including a
            // redundant request. Enable always selects subdivision4.
            {
                let value = if enabled {
                    4.0f32.to_bits()
                } else {
                    let restore = ordinary_restore
                        .ok_or(ControlError::MissingOrdinaryRestore)?
                        .bits();
                    ordinary_valid(bound, restore)?;
                    restore
                };
                steps.push(ParameterWriteStep {
                    parameter_id,
                    raw_bits: if enabled {
                        1.0f32.to_bits()
                    } else {
                        0.0f32.to_bits()
                    },
                });
                steps.push(ParameterWriteStep {
                    parameter_id: value_parameter,
                    raw_bits: value,
                });
            }
        }
        semantics => {
            let bits = match request {
                ControlRequest::RawBits(bits) => bits,
                ControlRequest::Choice(value)
                    if matches!(
                        semantics,
                        ControlSemantics::Choice | ControlSemantics::Switch
                    ) =>
                {
                    (value as f32).to_bits()
                }
                ControlRequest::Subdivision(index)
                    if matches!(semantics, ControlSemantics::SyncNumeric { .. }) && index <= 10 =>
                {
                    f32::from(index).to_bits()
                }
                ControlRequest::Sentinel(kind) if semantics == ControlSemantics::Numeric => {
                    sentinel_value(parameter, kind)
                        .ok_or(ControlError::Unsupported)?
                        .to_bits()
                }
                _ => return Err(ControlError::Unsupported),
            };
            let raw = f32::from_bits(bits);
            match semantics {
                ControlSemantics::Numeric => ordinary_valid(parameter, bits)?,
                ControlSemantics::SyncNumeric { power_parameter } => {
                    let power = model
                        .parameter(power_parameter)
                        .ok_or(ControlError::MissingBinding)?;
                    if !matches!(control_semantics(power), ControlSemantics::SyncPower { .. })
                        || sync_value_parameter(model, power_parameter)? != parameter_id
                    {
                        return Err(ControlError::MissingBinding);
                    }
                    let power_bits = *module_bits
                        .get(usize::from(power_parameter))
                        .ok_or(ControlError::MissingValue(power_parameter))?;
                    match f32::from_bits(power_bits) {
                        0.0 => {
                            if matches!(request, ControlRequest::Subdivision(_)) {
                                return Err(ControlError::UnknownSyncState);
                            }
                            ordinary_valid(parameter, bits)?;
                        }
                        1.0 if raw.is_finite() && (0.0..=10.0).contains(&raw) => (),
                        1.0 => return Err(ControlError::InvalidValue),
                        _ => return Err(ControlError::UnknownSyncState),
                    }
                }
                ControlSemantics::Choice | ControlSemantics::Switch => {
                    if parameter.choice(raw).is_none() {
                        return Err(ControlError::UnknownChoice);
                    }
                }
                _ => return Err(ControlError::Unsupported),
            }
            steps.push(ParameterWriteStep {
                parameter_id,
                raw_bits: bits,
            });
        }
    }
    let mut expected = module_bits.to_vec();
    let mut expected_after_steps = Vec::new();
    for step in &steps {
        expected[usize::from(step.parameter_id)] = step.raw_bits;
        expected_after_steps.push(expected.clone());
    }
    Ok(ControlWritePlan {
        model: model.key(),
        model_xml_index: model.xml_index,
        before_values: module_bits.to_vec(),
        steps,
        expected_after_steps,
    })
}
