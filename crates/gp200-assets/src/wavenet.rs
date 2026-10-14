//! Causal NAM WaveNet equations from the pinned MIT NAM Core reference.
//! This is an offline renderer, not a calibrated SnapTone encoder.
//! Reference: sdatkinson/NeuralAmpModelerCore 0b3d3c97b0859a3a8c92a8628c4dd89a25eb5842.
//! Upstream copyright and MIT permission: assets/licenses/neural-amp-modeler-core-MIT.txt.
use crate::{ActivationMode, SampleProcessor, invalid};
use gp200_domain::ServiceError;
use serde_json::Value;

const MAX_CHANNELS: usize = 256;
const MAX_STATE_FLOATS: usize = 16 * 1024 * 1024;
const MAX_WEIGHTS: usize = 4_194_304;

pub(crate) fn integer(
    v: &Value,
    key: &str,
    default: Option<usize>,
    max: usize,
) -> Result<usize, ServiceError> {
    let n = match v.get(key) {
        None => default.ok_or_else(|| invalid(format!("NAM requires integer {key}")))?,
        Some(n) => n
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| invalid(format!("NAM {key} must be a positive integer")))?,
    };
    if n == 0 || n > max {
        return Err(invalid(format!(
            "NAM {key} is outside the local limit 1–{max}"
        )));
    }
    Ok(n)
}
pub(crate) fn boolean(v: &Value, key: &str, default: bool) -> Result<bool, ServiceError> {
    v.get(key).map_or(Ok(default), |v| {
        v.as_bool()
            .ok_or_else(|| invalid(format!("NAM {key} must be boolean")))
    })
}
fn number(v: &Value, key: &str, default: Option<f32>) -> Result<f32, ServiceError> {
    let n = v
        .get(key)
        .map_or(default, |n| n.as_f64().map(|n| n as f32))
        .ok_or_else(|| invalid(format!("NAM requires finite {key}")))?;
    if !n.is_finite() {
        return Err(invalid(format!("NAM {key} must be finite float32")));
    }
    Ok(n)
}
pub(crate) fn known_keys(v: &Value, keys: &[&str]) -> Result<(), ServiceError> {
    let object = v
        .as_object()
        .ok_or_else(|| invalid("NAM configuration must be an object"))?;
    if let Some(key) = object.keys().find(|key| !keys.contains(&key.as_str())) {
        return Err(ServiceError::unsupported(format!(
            "NAM configuration field {key} has no implemented numerical contract"
        )));
    }
    Ok(())
}
fn per_layer<'a>(
    v: &'a Value,
    key: &str,
    index: usize,
    count: usize,
) -> Result<Option<&'a Value>, ServiceError> {
    match v.get(key) {
        Some(Value::Array(a)) => {
            if a.len() != count {
                return Err(invalid(format!(
                    "NAM {key} array length differs from dilations"
                )));
            }
            Ok(Some(&a[index]))
        }
        other => Ok(other),
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Activation {
    name: String,
    slopes: Vec<f32>,
    lo: f32,
    hi: f32,
    lo_slope: f32,
    hi_slope: f32,
}
impl Activation {
    pub(crate) fn parse(value: &Value, channels: usize) -> Result<Self, ServiceError> {
        let name = value
            .as_str()
            .or_else(|| value.get("type").and_then(Value::as_str))
            .ok_or_else(|| invalid("NAM activation requires a name or typed object"))?;
        if ![
            "Tanh",
            "Fasttanh",
            "Hardtanh",
            "ReLU",
            "LeakyReLU",
            "PReLU",
            "Sigmoid",
            "SiLU",
            "Hardswish",
            "LeakyHardtanh",
            "LeakyHardTanh",
            "Softsign",
        ]
        .contains(&name)
        {
            return Err(ServiceError::unsupported(format!(
                "Unsupported NAM activation {name}"
            )));
        }
        if value.is_object() {
            known_keys(
                value,
                &[
                    "type",
                    "negative_slope",
                    "negative_slopes",
                    "min_val",
                    "max_val",
                    "min_slope",
                    "max_slope",
                ],
            )?;
        }
        let slopes = if let Some(values) = value.get("negative_slopes") {
            let values = values
                .as_array()
                .ok_or_else(|| invalid("PReLU negative_slopes must be an array"))?;
            if values.len() != channels {
                return Err(invalid(
                    "PReLU negative_slopes must match activated channels",
                ));
            }
            values
                .iter()
                .map(|n| {
                    n.as_f64()
                        .map(|n| n as f32)
                        .filter(|n| n.is_finite())
                        .ok_or_else(|| invalid("PReLU slopes must be finite"))
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            vec![number(value, "negative_slope", Some(0.01))?]
        };
        let lo = number(value, "min_val", Some(-1.0))?;
        let hi = number(value, "max_val", Some(1.0))?;
        if lo > hi {
            return Err(invalid("LeakyHardtanh min_val exceeds max_val"));
        }
        Ok(Self {
            name: name.into(),
            slopes,
            lo,
            hi,
            lo_slope: number(value, "min_slope", Some(0.01))?,
            hi_slope: number(value, "max_slope", Some(0.01))?,
        })
    }
    pub(crate) fn apply(&self, x: f32, channel: usize, mode: ActivationMode) -> f32 {
        match self.name.as_str() {
            "Tanh" => crate::lstm::tanh(x, mode),
            "Fasttanh" => crate::lstm::tanh(x, ActivationMode::Fast),
            "Sigmoid" => crate::lstm::sigmoid(x, ActivationMode::Standard),
            "ReLU" => x.max(0.0),
            "Hardtanh" => x.clamp(-1.0, 1.0),
            "LeakyReLU" | "PReLU" => {
                if x < 0.0 {
                    x * self.slopes[if self.slopes.len() == 1 { 0 } else { channel }]
                } else {
                    x
                }
            }
            "SiLU" => x / (1.0 + (-x).exp()),
            "Hardswish" => x * (x + 3.0).clamp(0.0, 6.0) / 6.0,
            "Softsign" => x / (1.0 + x.abs()),
            "LeakyHardtanh" | "LeakyHardTanh" => {
                if x < self.lo {
                    self.lo + self.lo_slope * (x - self.lo)
                } else if x > self.hi {
                    self.hi + self.hi_slope * (x - self.hi)
                } else {
                    x
                }
            }
            _ => unreachable!("validated activation"),
        }
    }
}
#[derive(Default)]
pub(crate) struct Budget {
    pub weights: usize,
    pub state: usize,
    pub operations: u64,
}
pub(crate) struct Cursor<'a> {
    pub weights: &'a [f32],
    pub pos: usize,
}
impl Cursor<'_> {
    pub(crate) fn take(&mut self, n: usize) -> Result<Vec<f32>, ServiceError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| invalid("NAM weight dimensions overflow"))?;
        let v = self
            .weights
            .get(self.pos..end)
            .ok_or_else(|| {
                invalid(format!(
                    "NAM weights truncated at {}, need {n} more",
                    self.pos
                ))
            })?
            .to_vec();
        self.pos = end;
        Ok(v)
    }
}
#[derive(Debug, Clone)]
pub(crate) struct Conv {
    input: usize,
    output: usize,
    kernel: usize,
    dilation: usize,
    groups: usize,
    weights: Vec<f32>,
    bias: Vec<f32>,
    history: Vec<f32>,
    ring: usize,
    position: usize,
}
impl Conv {
    fn sliced(
        &self,
        input: usize,
        output: usize,
        budget: &mut Budget,
    ) -> Result<Self, ServiceError> {
        if self.groups != 1
            || input == 0
            || output == 0
            || input > self.input
            || output > self.output
        {
            return Err(ServiceError::unsupported(
                "Slimmable WaveNet slicing requires groups=1 and bounded leading channel subsets",
            ));
        }
        let mut weights = Vec::with_capacity(input * output * self.kernel);
        for o in 0..output {
            for i in 0..input {
                let start = (o * self.input + i) * self.kernel;
                weights.extend_from_slice(&self.weights[start..start + self.kernel]);
            }
        }
        let state = self.ring * input;
        budget.state = budget
            .state
            .checked_add(state)
            .ok_or_else(|| invalid("NAM sliced state overflow"))?;
        if budget.state > MAX_STATE_FLOATS {
            return Err(invalid("NAM sliced delay exceeds 64 MiB"));
        }
        budget.operations += weights.len() as u64;
        Ok(Self {
            input,
            output,
            kernel: self.kernel,
            dilation: self.dilation,
            groups: 1,
            weights,
            bias: self.bias[..output].to_vec(),
            history: vec![0.0; state],
            ring: self.ring,
            position: 0,
        })
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        input: usize,
        output: usize,
        kernel: usize,
        dilation: usize,
        groups: usize,
        bias: bool,
        cursor: &mut Cursor<'_>,
        budget: &mut Budget,
    ) -> Result<Self, ServiceError> {
        if input == 0
            || output == 0
            || input > MAX_CHANNELS * 2
            || output > MAX_CHANNELS * 2
            || kernel == 0
            || kernel > 32
            || dilation == 0
            || dilation > 65536
            || groups == 0
            || !input.is_multiple_of(groups)
            || !output.is_multiple_of(groups)
        {
            return Err(invalid(
                "NAM convolution has invalid or unbounded dimensions/groups",
            ));
        }
        let count = output * (input / groups) * kernel;
        let ring = (kernel - 1) * dilation + 1;
        let state = ring * input;
        budget.state = budget
            .state
            .checked_add(state)
            .ok_or_else(|| invalid("NAM state size overflow"))?;
        budget.operations += count as u64;
        if budget.state > MAX_STATE_FLOATS {
            return Err(ServiceError::unsupported(
                "NAM delay state exceeds the local 64 MiB budget",
            ));
        }
        let weights = cursor.take(count)?;
        let bias = if bias {
            cursor.take(output)?
        } else {
            vec![0.0; output]
        };
        Ok(Self {
            input,
            output,
            kernel,
            dilation,
            groups,
            weights,
            bias,
            history: vec![0.0; state],
            ring,
            position: 0,
        })
    }
    pub(crate) fn sample(&mut self, input: &[f32]) -> Vec<f32> {
        debug_assert_eq!(input.len(), self.input);
        self.history[self.position * self.input..(self.position + 1) * self.input]
            .copy_from_slice(input);
        let mut out = vec![0.0; self.output];
        let inputs = self.input / self.groups;
        let outputs = self.output / self.groups;
        // Export flattening: group, output-channel, input-channel, oldest-to-current tap.
        for (o, value) in out.iter_mut().enumerate() {
            let group = o / outputs;
            for k in 0..self.kernel {
                let age = (self.kernel - 1 - k) * self.dilation;
                let row = (self.position + self.ring - age) % self.ring;
                let mut tap = 0.0;
                for i in 0..inputs {
                    let weight = (o * inputs + i) * self.kernel + k;
                    tap +=
                        self.weights[weight] * self.history[row * self.input + group * inputs + i];
                }
                *value += tap;
            }
            *value += self.bias[o];
        }
        self.position = (self.position + 1) % self.ring;
        out
    }
    pub(crate) fn reset(&mut self) {
        self.history.fill(0.0);
        self.position = 0;
    }
}
#[derive(Debug, Clone)]
struct Film {
    conv: Conv,
    shift: bool,
}
impl Film {
    fn parse(
        v: &Value,
        key: &str,
        condition: usize,
        input: usize,
        cursor: &mut Cursor<'_>,
        budget: &mut Budget,
    ) -> Result<Option<Self>, ServiceError> {
        let Some(config) = v.get(key) else {
            return Ok(None);
        };
        if config == &Value::Bool(false) {
            return Ok(None);
        };
        known_keys(config, &["active", "shift", "groups"])?;
        if !boolean(config, "active", true)? {
            return Ok(None);
        };
        let shift = boolean(config, "shift", true)?;
        let groups = integer(config, "groups", Some(1), MAX_CHANNELS)?;
        Ok(Some(Self {
            conv: Conv::new(
                condition,
                input * if shift { 2 } else { 1 },
                1,
                1,
                groups,
                true,
                cursor,
                budget,
            )?,
            shift,
        }))
    }
    fn apply(&mut self, input: &mut [f32], condition: &[f32]) {
        let scale = self.conv.sample(condition);
        let n = input.len();
        for (i, x) in input.iter_mut().enumerate() {
            *x = *x * scale[i] + if self.shift { scale[n + i] } else { 0.0 };
        }
    }
}
fn film_apply(film: &mut Option<Film>, input: &mut [f32], condition: &[f32]) {
    if let Some(film) = film {
        film.apply(input, condition)
    }
}
#[derive(Debug, Clone, Copy)]
enum Gate {
    None,
    Gated,
    Blended,
}
#[derive(Debug, Clone)]
struct Layer {
    conv: Conv,
    mixin: Conv,
    residual: Option<Conv>,
    head: Option<Conv>,
    films: Vec<Option<Film>>,
    activation: Activation,
    secondary: Activation,
    gate: Gate,
    bottleneck: usize,
}
impl Layer {
    #[allow(clippy::too_many_arguments)]
    fn new(
        v: &Value,
        index: usize,
        count: usize,
        channels: usize,
        condition: usize,
        kernel: usize,
        dilation: usize,
        cursor: &mut Cursor<'_>,
        budget: &mut Budget,
    ) -> Result<Self, ServiceError> {
        let bottleneck = integer(v, "bottleneck", Some(channels), MAX_CHANNELS)?;
        let gate = match per_layer(v, "gating_mode", index, count)? {
            Some(g) => match g.as_str() {
                Some("none") => Gate::None,
                Some("gated") => Gate::Gated,
                Some("blended") => Gate::Blended,
                _ => return Err(invalid("NAM gating_mode requires none, gated or blended")),
            },
            None => {
                if boolean(v, "gated", false)? {
                    Gate::Gated
                } else {
                    Gate::None
                }
            }
        };
        let z = bottleneck * if matches!(gate, Gate::None) { 1 } else { 2 };
        let activation = Activation::parse(
            per_layer(v, "activation", index, count)?
                .ok_or_else(|| invalid("NAM WaveNet activation is missing"))?,
            bottleneck,
        )?;
        let secondary = if matches!(gate, Gate::None) {
            Activation::parse(&Value::String("Sigmoid".into()), bottleneck)?
        } else {
            Activation::parse(
                per_layer(v, "secondary_activation", index, count)?
                    .unwrap_or(&Value::String("Sigmoid".into())),
                bottleneck,
            )?
        };
        let conv = Conv::new(
            channels,
            z,
            kernel,
            dilation,
            integer(v, "groups_input", Some(1), MAX_CHANNELS)?,
            true,
            cursor,
            budget,
        )?;
        let mixin = Conv::new(
            condition,
            z,
            1,
            1,
            integer(v, "groups_input_mixin", Some(1), MAX_CHANNELS)?,
            false,
            cursor,
            budget,
        )?;
        let residual = match v.get("layer1x1") {
            Some(config) => {
                known_keys(config, &["active", "groups"])?;
                if boolean(config, "active", true)? {
                    Some(Conv::new(
                        bottleneck,
                        channels,
                        1,
                        1,
                        integer(config, "groups", Some(1), MAX_CHANNELS)?,
                        true,
                        cursor,
                        budget,
                    )?)
                } else {
                    if bottleneck != channels {
                        return Err(invalid(
                            "Inactive layer1x1 requires bottleneck equal to channels",
                        ));
                    }
                    None
                }
            }
            None => Some(Conv::new(
                bottleneck, channels, 1, 1, 1, true, cursor, budget,
            )?),
        };
        let head = match v.get("head1x1") {
            Some(config) => {
                known_keys(config, &["active", "out_channels", "groups"])?;
                if boolean(config, "active", false)? {
                    Some(Conv::new(
                        bottleneck,
                        integer(config, "out_channels", Some(channels), MAX_CHANNELS)?,
                        1,
                        1,
                        integer(config, "groups", Some(1), MAX_CHANNELS)?,
                        true,
                        cursor,
                        budget,
                    )?)
                } else {
                    None
                }
            }
            None => None,
        };
        let head_size = head.as_ref().map_or(bottleneck, |conv| conv.output);
        let specs = [
            ("conv_pre_film", channels),
            ("conv_post_film", z),
            ("input_mixin_pre_film", condition),
            ("input_mixin_post_film", z),
            ("activation_pre_film", z),
            ("activation_post_film", bottleneck),
            ("layer1x1_post_film", channels),
            ("head1x1_post_film", head_size),
        ];
        let mut films = Vec::new();
        for (key, size) in specs {
            films.push(Film::parse(v, key, condition, size, cursor, budget)?);
        }
        if (residual.is_none() && films[6].is_some()) || (head.is_none() && films[7].is_some()) {
            return Err(invalid(
                "NAM post-1x1 FiLM requires the corresponding active convolution",
            ));
        }
        Ok(Self {
            conv,
            mixin,
            residual,
            head,
            films,
            activation,
            secondary,
            gate,
            bottleneck,
        })
    }
    fn sample(
        &mut self,
        input: &[f32],
        condition: &[f32],
        mode: ActivationMode,
    ) -> Result<(Vec<f32>, Vec<f32>), ServiceError> {
        let mut conv_input = input.to_vec();
        film_apply(&mut self.films[0], &mut conv_input, condition);
        let mut z = self.conv.sample(&conv_input);
        film_apply(&mut self.films[1], &mut z, condition);
        let mut mix_input = condition.to_vec();
        film_apply(&mut self.films[2], &mut mix_input, condition);
        let mut mix = self.mixin.sample(&mix_input);
        film_apply(&mut self.films[3], &mut mix, condition);
        for (z, mix) in z.iter_mut().zip(mix) {
            *z += mix;
        }
        film_apply(&mut self.films[4], &mut z, condition);
        if z.iter().any(|x| !x.is_finite()) {
            return Err(invalid(
                "NAM WaveNet produced a non-finite pre-activation value",
            ));
        }
        for i in 0..self.bottleneck {
            let old = z[i];
            let active = self.activation.apply(old, i, mode);
            z[i] = match self.gate {
                Gate::None => active,
                Gate::Gated => active * self.secondary.apply(z[i + self.bottleneck], i, mode),
                Gate::Blended => {
                    let alpha = self.secondary.apply(z[i + self.bottleneck], i, mode);
                    alpha * active + (1.0 - alpha) * old
                }
            };
        }
        z.truncate(self.bottleneck);
        film_apply(&mut self.films[5], &mut z, condition);
        let residual = if let Some(conv) = &mut self.residual {
            let mut out = conv.sample(&z);
            film_apply(&mut self.films[6], &mut out, condition);
            for (out, input) in out.iter_mut().zip(input) {
                *out += input;
            }
            out
        } else {
            input.to_vec()
        };
        let mut head = if let Some(conv) = &mut self.head {
            conv.sample(&z)
        } else {
            z
        };
        film_apply(&mut self.films[7], &mut head, condition);
        // Keep finite checks at every boundary so overflow cannot be hidden by an activation.
        if residual.iter().chain(&head).any(|x| !x.is_finite()) {
            return Err(invalid("NAM WaveNet produced a non-finite layer output"));
        }
        Ok((residual, head))
    }
    fn reset(&mut self) {
        self.conv.reset();
        self.mixin.reset();
        for conv in [&mut self.residual, &mut self.head].into_iter().flatten() {
            conv.reset();
        }
        for film in self.films.iter_mut().flatten() {
            film.conv.reset();
        }
    }
}
#[derive(Debug, Clone)]
struct Stack {
    rechannel: Conv,
    layers: Vec<Layer>,
    head: Conv,
    skip_channels: usize,
}
#[derive(Debug, Clone)]
enum ConditionModel {
    WaveNet(Box<WaveNetModel>),
    Mono(Box<crate::NamModel>),
}
impl ConditionModel {
    fn outputs(&self) -> usize {
        match self {
            Self::WaveNet(m) => m.outputs,
            Self::Mono(_) => 1,
        }
    }
    fn operations(&self) -> u64 {
        match self {
            Self::WaveNet(m) => m.operations,
            Self::Mono(m) => m.operations_per_sample(),
        }
    }
    fn reset(&mut self) -> Result<(), ServiceError> {
        match self {
            Self::WaveNet(m) => m.reset(),
            Self::Mono(m) => m.reset(),
        }
    }
    fn set_activation(&mut self, mode: ActivationMode) {
        match self {
            Self::WaveNet(m) => m.set_activation(mode),
            Self::Mono(m) => m.set_activation(mode),
        }
    }
    fn sample(&mut self, input: f32) -> Result<Vec<f32>, ServiceError> {
        match self {
            Self::WaveNet(m) => m.sample_channels(input),
            Self::Mono(m) => {
                let mut output = [0.0];
                m.process(&[input], &mut output)?;
                Ok(output.to_vec())
            }
        }
    }
}
#[derive(Debug, Clone)]
pub struct WaveNetModel {
    rate: u32,
    stacks: Vec<Stack>,
    condition: Option<ConditionModel>,
    head: Vec<(Activation, Conv)>,
    scale: f32,
    activation: ActivationMode,
    outputs: usize,
    operations: u64,
}
impl WaveNetModel {
    fn slimmed(&self, channels: &[usize]) -> Result<Self, ServiceError> {
        if channels.len() != self.stacks.len() || !self.head.is_empty() {
            return Err(invalid("Slimmable WaveNet array count/post-head mismatch"));
        }
        let mut result = self.clone();
        let mut budget = Budget::default();
        for (index, stack) in result.stacks.iter_mut().enumerate() {
            let original = &self.stacks[index];
            if original.head.kernel != 1 {
                return Err(ServiceError::unsupported(
                    "Slimmable WaveNet head rechannel kernel must equal 1",
                ));
            }
            let old_channels = original.rechannel.output;
            let new_channels = channels[index];
            if new_channels == 0 || new_channels > old_channels {
                return Err(invalid(
                    "Slimmable WaveNet target channels exceed validated full model",
                ));
            }
            let input = if index == 0 {
                original.rechannel.input
            } else {
                channels[index - 1]
            };
            stack.rechannel = original
                .rechannel
                .sliced(input, new_channels, &mut budget)?;
            for (layer, full) in stack.layers.iter_mut().zip(&original.layers) {
                let bottleneck = if full.residual.is_none() {
                    new_channels
                } else {
                    (full.bottleneck * new_channels / old_channels).max(1)
                };
                let z = bottleneck
                    * if matches!(full.gate, Gate::None) {
                        1
                    } else {
                        2
                    };
                layer.conv = full.conv.sliced(new_channels, z, &mut budget)?;
                layer.mixin = full.mixin.sliced(full.mixin.input, z, &mut budget)?;
                layer.residual = full
                    .residual
                    .as_ref()
                    .map(|c| c.sliced(bottleneck, new_channels, &mut budget))
                    .transpose()?;
                layer.head = full
                    .head
                    .as_ref()
                    .map(|c| c.sliced(bottleneck, c.output, &mut budget))
                    .transpose()?;
                // Upstream retains activation configurations rather than slicing learned slopes.
                // A full-width PReLU vector cannot be silently assigned to fewer channels.
                if (full.activation.slopes.len() > 1 || full.secondary.slopes.len() > 1)
                    && bottleneck != full.bottleneck
                {
                    return Err(ServiceError::unsupported(
                        "Pruned channel-specific PReLU has no verified pinned slim profile",
                    ));
                }
                let sizes = [
                    new_channels,
                    z,
                    full.mixin.input,
                    z,
                    z,
                    bottleneck,
                    new_channels,
                    full.head.as_ref().map_or(bottleneck, |h| h.output),
                ];
                for (index, film) in layer.films.iter_mut().enumerate() {
                    if let Some(film) = film {
                        let full_film = full.films[index].as_ref().expect("cloned film");
                        film.conv = full_film.conv.sliced(
                            full_film.conv.input,
                            sizes[index] * if film.shift { 2 } else { 1 },
                            &mut budget,
                        )?;
                    }
                }
                layer.bottleneck = bottleneck;
            }
            stack.skip_channels = stack.layers[0]
                .head
                .as_ref()
                .map_or(stack.layers[0].bottleneck, |h| h.output);
            let head_output = if index + 1 < channels.len() {
                channels[index + 1]
            } else {
                original.head.output
            };
            stack.head = original
                .head
                .sliced(stack.skip_channels, head_output, &mut budget)?;
        }
        if result
            .stacks
            .windows(2)
            .any(|arrays| arrays[0].head.output != arrays[1].skip_channels)
        {
            return Err(ServiceError::unsupported(
                "Selected Slimmable WaveNet head/skip channel links differ; this profile has no valid mono slicing contract",
            ));
        }
        result.operations = budget.operations
            + result
                .condition
                .as_ref()
                .map_or(0, ConditionModel::operations);
        result.reset()?;
        Ok(result)
    }
    pub(crate) fn from_json(
        config: &Value,
        weights: &[f32],
        rate: u32,
        budget: &mut Budget,
        depth: usize,
    ) -> Result<Self, ServiceError> {
        if depth > 8 {
            return Err(invalid("NAM nested models exceed the local depth limit 8"));
        }
        known_keys(
            config,
            &[
                "layers",
                "head",
                "head_scale",
                "condition_dsp",
                "in_channels",
            ],
        )?;
        if integer(config, "in_channels", Some(1), MAX_CHANNELS)? != 1 {
            return Err(ServiceError::unsupported(
                "Local WaveNet input must be mono",
            ));
        }
        budget.weights = budget
            .weights
            .checked_add(weights.len())
            .ok_or_else(|| invalid("NAM weight count overflow"))?;
        if budget.weights > MAX_WEIGHTS {
            return Err(invalid("NAM nested weights exceed the local limit"));
        }
        let condition = match config.get("condition_dsp").filter(|v| !v.is_null()) {
            None => None,
            Some(child) => {
                crate::nam::check_version(child)?;
                let child_rate = crate::nam::json_rate(child)?.unwrap_or(rate);
                if child_rate != rate {
                    return Err(invalid("NAM conditioning model sample rate mismatch"));
                }
                if child["architecture"] == "WaveNet"
                    && !child["config"]["layers"].as_array().is_some_and(|layers| {
                        layers
                            .iter()
                            .any(|l| l.get("slimmable").is_some_and(|v| !v.is_null()))
                    })
                {
                    let child_weights = crate::nam::json_weights(child, false)?;
                    Some(ConditionModel::WaveNet(Box::new(Self::from_json(
                        &child["config"],
                        &child_weights,
                        rate,
                        budget,
                        depth + 1,
                    )?)))
                } else {
                    Some(ConditionModel::Mono(Box::new(crate::nam::load_value(
                        child,
                        Some(rate),
                        budget,
                        depth + 1,
                    )?)))
                }
            }
        };
        let condition_channels = condition.as_ref().map_or(1, ConditionModel::outputs);
        let configs = config["layers"]
            .as_array()
            .ok_or_else(|| invalid("NAM WaveNet layers must be an array"))?;
        if configs.is_empty() || configs.len() > 8 {
            return Err(invalid("NAM WaveNet requires 1–8 layer arrays"));
        }
        let mut cursor = Cursor { weights, pos: 0 };
        let mut stacks = Vec::new();
        let mut previous_channels = 1;
        let mut previous_head = None;
        let before_ops = budget.operations;
        for v in configs {
            known_keys(
                v,
                &[
                    "input_size",
                    "condition_size",
                    "head_size",
                    "head_bias",
                    "head",
                    "channels",
                    "bottleneck",
                    "kernel_size",
                    "kernel_sizes",
                    "dilations",
                    "activation",
                    "gated",
                    "gating_mode",
                    "secondary_activation",
                    "groups_input",
                    "groups_input_mixin",
                    "layer1x1",
                    "head1x1",
                    "conv_pre_film",
                    "conv_post_film",
                    "input_mixin_pre_film",
                    "input_mixin_post_film",
                    "activation_pre_film",
                    "activation_post_film",
                    "layer1x1_post_film",
                    "head1x1_post_film",
                    "slimmable",
                ],
            )?;
            if v.get("slimmable").is_some_and(|v| !v.is_null()) {
                return Err(ServiceError::unsupported(
                    "Slimmable WaveNet requires a separate validated architecture",
                ));
            }
            let input = integer(v, "input_size", None, MAX_CHANNELS)?;
            let channels = integer(v, "channels", None, MAX_CHANNELS)?;
            if input != previous_channels
                || integer(v, "condition_size", None, MAX_CHANNELS)? != condition_channels
            {
                return Err(invalid(
                    "NAM WaveNet input/condition channel links mismatch",
                ));
            }
            let ds = v["dilations"]
                .as_array()
                .ok_or_else(|| invalid("NAM dilations must be an array"))?;
            if ds.is_empty() || ds.len() > 32 {
                return Err(invalid("NAM requires 1–32 dilations per stack"));
            }
            if v.get("kernel_size").is_some() && v.get("kernel_sizes").is_some() {
                return Err(invalid("NAM specifies both kernel_size and kernel_sizes"));
            }
            let kernels = if let Some(k) = v.get("kernel_sizes") {
                let ks = k
                    .as_array()
                    .ok_or_else(|| invalid("kernel_sizes must be an array"))?;
                if ks.len() != ds.len() {
                    return Err(invalid("kernel_sizes length differs from dilations"));
                }
                ks.iter()
                    .map(|x| {
                        x.as_u64()
                            .and_then(|x| usize::try_from(x).ok())
                            .filter(|x| (1..=32).contains(x))
                            .ok_or_else(|| invalid("NAM kernel_size must be 1–32"))
                    })
                    .collect::<Result<Vec<_>, _>>()?
            } else {
                vec![integer(v, "kernel_size", None, 32)?; ds.len()]
            };
            let rechannel = Conv::new(input, channels, 1, 1, 1, false, &mut cursor, budget)?;
            let mut layers = Vec::new();
            for (i, d) in ds.iter().enumerate() {
                let d = d
                    .as_u64()
                    .and_then(|x| usize::try_from(x).ok())
                    .filter(|x| (1..=65536).contains(x))
                    .ok_or_else(|| invalid("NAM dilation must be 1–65536"))?;
                layers.push(Layer::new(
                    v,
                    i,
                    ds.len(),
                    channels,
                    condition_channels,
                    kernels[i],
                    d,
                    &mut cursor,
                    budget,
                )?);
            }
            let skip_channels = layers[0]
                .head
                .as_ref()
                .map_or(layers[0].bottleneck, |c| c.output);
            if layers
                .iter()
                .any(|l| l.head.as_ref().map_or(l.bottleneck, |c| c.output) != skip_channels)
                || previous_head.is_some_and(|n| n != skip_channels)
            {
                return Err(invalid("NAM WaveNet skip/head channels mismatch"));
            }
            let (outputs, kernel, dilation, bias) = match v.get("head").filter(|v| !v.is_null()) {
                Some(h) => {
                    known_keys(h, &["out_channels", "kernel_size", "head_dilation", "bias"])?;
                    (
                        integer(h, "out_channels", None, MAX_CHANNELS)?,
                        integer(h, "kernel_size", None, 32)?,
                        integer(h, "head_dilation", Some(1), 65536)?,
                        boolean(h, "bias", false)?,
                    )
                }
                None => (
                    integer(v, "head_size", None, MAX_CHANNELS)?,
                    1,
                    1,
                    boolean(v, "head_bias", false)?,
                ),
            };
            let head = Conv::new(
                skip_channels,
                outputs,
                kernel,
                dilation,
                1,
                bias,
                &mut cursor,
                budget,
            )?;
            stacks.push(Stack {
                rechannel,
                layers,
                head,
                skip_channels,
            });
            previous_channels = channels;
            previous_head = Some(outputs);
        }
        let mut outputs = previous_head.expect("nonempty stacks");
        let mut head = Vec::new();
        if let Some(h) = config.get("head").filter(|v| !v.is_null()) {
            known_keys(
                h,
                &[
                    "in_channels",
                    "out_channels",
                    "channels",
                    "kernel_sizes",
                    "activation",
                ],
            )?;
            if integer(h, "in_channels", None, MAX_CHANNELS)? != outputs {
                return Err(invalid("NAM post-stack head input channels mismatch"));
            }
            let hidden = integer(h, "channels", None, MAX_CHANNELS)?;
            let final_out = integer(h, "out_channels", None, MAX_CHANNELS)?;
            let ks = h["kernel_sizes"]
                .as_array()
                .ok_or_else(|| invalid("NAM head kernel_sizes must be an array"))?;
            if ks.is_empty() || ks.len() > 8 {
                return Err(invalid("NAM post-stack head requires 1–8 kernels"));
            }
            for (i, k) in ks.iter().enumerate() {
                let k = k
                    .as_u64()
                    .and_then(|k| usize::try_from(k).ok())
                    .filter(|k| (1..=32).contains(k))
                    .ok_or_else(|| invalid("NAM head kernel must be 1–32"))?;
                let out = if i + 1 == ks.len() { final_out } else { hidden };
                let act = Activation::parse(&h["activation"], outputs)?;
                let conv = Conv::new(outputs, out, k, 1, 1, true, &mut cursor, budget)?;
                head.push((act, conv));
                outputs = out;
            }
        }
        let config_scale = number(config, "head_scale", None)?;
        let scale = cursor.take(1)?[0];
        if !config_scale.is_finite() || !scale.is_finite() {
            return Err(invalid("NAM head scale is not finite"));
        }
        if cursor.pos != weights.len() {
            return Err(invalid(format!(
                "NAM WaveNet has {} excess weights",
                weights.len() - cursor.pos
            )));
        }
        let operations = budget.operations - before_ops
            + condition.as_ref().map_or(0, ConditionModel::operations);
        Ok(Self {
            rate,
            stacks,
            condition,
            head,
            scale,
            activation: ActivationMode::Standard,
            outputs,
            operations,
        })
    }
    pub fn output_channels(&self) -> usize {
        self.outputs
    }
    pub fn operations_per_sample(&self) -> u64 {
        self.operations
    }
    pub fn set_activation(&mut self, mode: ActivationMode) {
        self.activation = mode;
        if let Some(model) = &mut self.condition {
            model.set_activation(mode);
        }
    }
    fn sample_channels(&mut self, input: f32) -> Result<Vec<f32>, ServiceError> {
        let condition = if let Some(model) = &mut self.condition {
            model.sample(input)?
        } else {
            vec![input]
        };
        let mut residual = vec![input];
        let mut head = None;
        for stack in &mut self.stacks {
            residual = stack.rechannel.sample(&residual);
            let mut sum = head
                .take()
                .unwrap_or_else(|| vec![0.0; stack.skip_channels]);
            for layer in &mut stack.layers {
                let (r, h) = layer.sample(&residual, &condition, self.activation)?;
                residual = r;
                for (x, h) in sum.iter_mut().zip(h) {
                    *x += h;
                }
            }
            head = Some(stack.head.sample(&sum));
        }
        let mut out = head.expect("validated stacks");
        for x in &mut out {
            *x *= self.scale;
        }
        for (activation, conv) in &mut self.head {
            for (i, x) in out.iter_mut().enumerate() {
                *x = activation.apply(*x, i, self.activation);
            }
            out = conv.sample(&out);
        }
        if out.iter().chain(&residual).any(|x| !x.is_finite()) {
            return Err(invalid(
                "NAM WaveNet produced a non-finite intermediate/output",
            ));
        }
        Ok(out)
    }
}

/// Size-selectable upstream channel slicing with a validated immutable full-model template.
/// It keeps at most the full template and one selected child; selection changes reset history.
#[derive(Debug, Clone)]
pub struct SlimmableWaveNetModel {
    full: Box<WaveNetModel>,
    selected: Option<Box<WaveNetModel>>,
    allowed: Vec<Vec<usize>>,
    channels: Vec<usize>,
}
impl SlimmableWaveNetModel {
    pub(crate) fn new(full: WaveNetModel, allowed: Vec<Vec<usize>>) -> Result<Self, ServiceError> {
        if allowed.len() != full.stacks.len() {
            return Err(invalid("Slimmable WaveNet allowed channel arrays mismatch"));
        }
        let channels = full.stacks.iter().map(|s| s.rechannel.output).collect();
        Ok(Self {
            full: Box::new(full),
            selected: None,
            allowed,
            channels,
        })
    }
    pub fn set_size(&mut self, size: f64) -> Result<(), ServiceError> {
        if !size.is_finite() || !(0.0..=1.0).contains(&size) {
            return Err(invalid("NAM slimmable size must be finite in 0–1"));
        }
        let channels: Vec<usize> = self
            .allowed
            .iter()
            .zip(&self.full.stacks)
            .map(|(allowed, stack)| {
                if allowed.is_empty() {
                    stack.rechannel.output
                } else {
                    allowed[((size * allowed.len() as f64).floor() as usize).min(allowed.len() - 1)]
                }
            })
            .collect();
        if channels != self.channels {
            let selected = if channels
                .iter()
                .zip(&self.full.stacks)
                .all(|(c, s)| *c == s.rechannel.output)
            {
                self.full.reset()?;
                None
            } else {
                Some(Box::new(self.full.slimmed(&channels)?))
            };
            self.selected = selected;
            self.channels = channels;
        }
        Ok(())
    }
    pub(crate) fn size_plan(&self) -> crate::NamSizePlan {
        crate::NamSizePlan::Channels {
            allowed: self.allowed.clone(),
            full: self
                .full
                .stacks
                .iter()
                .map(|stack| stack.rechannel.output)
                .collect(),
        }
    }
    pub fn selected_channels(&self) -> &[usize] {
        &self.channels
    }
    pub fn is_full_size(&self) -> bool {
        self.selected.is_none()
    }
    fn active(&self) -> &WaveNetModel {
        self.selected.as_deref().unwrap_or(&self.full)
    }
    fn active_mut(&mut self) -> &mut WaveNetModel {
        self.selected.as_deref_mut().unwrap_or(&mut self.full)
    }
    pub fn operations_per_sample(&self) -> u64 {
        self.active().operations_per_sample()
    }
    pub fn set_activation(&mut self, mode: ActivationMode) {
        self.full.set_activation(mode);
        if let Some(selected) = &mut self.selected {
            selected.set_activation(mode);
        }
    }
}
impl SampleProcessor for SlimmableWaveNetModel {
    fn sample_rate(&self) -> u32 {
        self.full.sample_rate()
    }
    fn reset(&mut self) -> Result<(), ServiceError> {
        self.active_mut().reset()
    }
    fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), ServiceError> {
        self.active_mut().process(input, output)
    }
}
impl SampleProcessor for WaveNetModel {
    fn sample_rate(&self) -> u32 {
        self.rate
    }
    fn reset(&mut self) -> Result<(), ServiceError> {
        for stack in &mut self.stacks {
            stack.rechannel.reset();
            stack.head.reset();
            for layer in &mut stack.layers {
                layer.reset();
            }
        }
        for (_, conv) in &mut self.head {
            conv.reset();
        }
        if let Some(model) = &mut self.condition {
            model.reset()?;
        }
        Ok(())
    }
    fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), ServiceError> {
        if self.outputs != 1 {
            return Err(ServiceError::unsupported(
                "Mono WAV renderer requires one WaveNet output channel",
            ));
        }
        if input.len() != output.len() {
            return Err(invalid("WaveNet input/output lengths differ"));
        }
        for (x, y) in input.iter().zip(output) {
            if !x.is_finite() {
                return Err(invalid("NAM input is non-finite"));
            }
            *y = self.sample_channels(*x)?[0];
        }
        Ok(())
    }
}
