//! Mono causal ConvNet from pinned NAM Core 0b3d3c97; MIT notice in assets/licenses.
use crate::wavenet::{Activation, Budget, Conv, Cursor, boolean, integer, known_keys};
use crate::{ActivationMode, SampleProcessor, invalid};
use gp200_domain::ServiceError;
use serde_json::Value;

#[derive(Debug, Clone)]
struct Block {
    convolution: Conv,
    affine: Option<(Vec<f32>, Vec<f32>)>,
    activation: Activation,
}

/// Validated kernel-2 dilated convolution blocks, optional inference BatchNorm and affine head.
#[derive(Debug, Clone)]
pub struct ConvNetModel {
    blocks: Vec<Block>,
    head: Vec<f32>,
    bias: f32,
    rate: u32,
    activation: ActivationMode,
    operations: u64,
}
impl ConvNetModel {
    pub(crate) fn from_json(
        config: &Value,
        weights: &[f32],
        rate: u32,
        budget: &mut Budget,
    ) -> Result<Self, ServiceError> {
        known_keys(
            config,
            &[
                "channels",
                "dilations",
                "batchnorm",
                "activation",
                "groups",
                "in_channels",
                "out_channels",
            ],
        )?;
        if integer(config, "in_channels", Some(1), 256)? != 1
            || integer(config, "out_channels", Some(1), 256)? != 1
        {
            return Err(ServiceError::unsupported(
                "Local ConvNet input/output must be mono",
            ));
        }
        let channels = integer(config, "channels", None, 256)?;
        let groups = integer(config, "groups", Some(1), 256)?;
        // The first convolution has one input channel, so the global group count can only be 1.
        if groups != 1 {
            return Err(invalid("Mono ConvNet groups must equal 1"));
        }
        let batchnorm = boolean(config, "batchnorm", false)?;
        if config.get("batchnorm").is_none() {
            return Err(invalid("ConvNet requires boolean batchnorm"));
        }
        let activation = Activation::parse(&config["activation"], channels)?;
        let dilations = config["dilations"]
            .as_array()
            .ok_or_else(|| invalid("ConvNet dilations must be an array"))?;
        if dilations.is_empty() || dilations.len() > 32 {
            return Err(invalid("ConvNet requires 1–32 dilation blocks"));
        }
        budget.weights = budget
            .weights
            .checked_add(weights.len())
            .ok_or_else(|| invalid("NAM total weights overflow"))?;
        if budget.weights > 4_194_304 {
            return Err(invalid("NAM total weights exceed the local limit"));
        }
        let before_ops = budget.operations;
        let mut cursor = Cursor { weights, pos: 0 };
        let mut blocks = Vec::new();
        for (index, dilation) in dilations.iter().enumerate() {
            let dilation = dilation
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| (1..=65_536).contains(n))
                .ok_or_else(|| invalid("ConvNet dilation must be 1–65536"))?;
            let convolution = Conv::new(
                if index == 0 { 1 } else { channels },
                channels,
                2,
                dilation,
                groups,
                !batchnorm,
                &mut cursor,
                budget,
            )?;
            let affine = if batchnorm {
                // Exported order: running_mean, running_var, affine weight, affine bias, eps.
                let means = cursor.take(channels)?;
                let variances = cursor.take(channels)?;
                let scales = cursor.take(channels)?;
                let biases = cursor.take(channels)?;
                let eps = cursor.take(1)?[0];
                if eps < 0.0
                    || variances
                        .iter()
                        .any(|v| *v < 0.0 || !(*v + eps).is_finite() || *v + eps <= 0.0)
                {
                    return Err(invalid(
                        "ConvNet BatchNorm requires nonnegative variance/epsilon and positive finite variance + epsilon",
                    ));
                }
                let scale: Vec<f32> = scales
                    .iter()
                    .zip(&variances)
                    .map(|(w, v)| *w / (*v + eps).sqrt())
                    .collect();
                let loc: Vec<f32> = biases
                    .iter()
                    .zip(scale.iter().zip(&means))
                    .map(|(b, (s, m))| *b - *s * *m)
                    .collect();
                if scale.iter().chain(&loc).any(|v| !v.is_finite()) {
                    return Err(invalid("ConvNet BatchNorm coefficients overflow float32"));
                }
                budget.operations += channels as u64 * 2;
                Some((scale, loc))
            } else {
                None
            };
            blocks.push(Block {
                convolution,
                affine,
                activation: activation.clone(),
            });
        }
        let head = cursor.take(channels)?;
        let bias = cursor.take(1)?[0];
        budget.operations += channels as u64;
        if cursor.pos != weights.len() {
            return Err(invalid("ConvNet has excess weights"));
        }
        Ok(Self {
            blocks,
            head,
            bias,
            rate,
            activation: ActivationMode::Standard,
            operations: budget.operations - before_ops,
        })
    }
    pub fn set_activation(&mut self, mode: ActivationMode) {
        self.activation = mode;
    }
    pub fn operations_per_sample(&self) -> u64 {
        self.operations
    }
}
impl SampleProcessor for ConvNetModel {
    fn sample_rate(&self) -> u32 {
        self.rate
    }
    fn reset(&mut self) -> Result<(), ServiceError> {
        for block in &mut self.blocks {
            block.convolution.reset();
        }
        Ok(())
    }
    fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), ServiceError> {
        if input.len() != output.len() {
            return Err(invalid("ConvNet input/output lengths differ"));
        }
        for (sample, result) in input.iter().zip(output) {
            if !sample.is_finite() {
                return Err(invalid("NAM input is non-finite"));
            }
            let mut values = vec![*sample];
            for block in &mut self.blocks {
                values = block.convolution.sample(&values);
                if values.iter().any(|v| !v.is_finite()) {
                    return Err(invalid(
                        "ConvNet convolution produced a non-finite intermediate",
                    ));
                }
                for (i, value) in values.iter_mut().enumerate() {
                    if let Some((scale, loc)) = &block.affine {
                        *value = *value * scale[i] + loc[i];
                    }
                    if !value.is_finite() {
                        return Err(invalid(
                            "ConvNet BatchNorm produced a non-finite intermediate",
                        ));
                    }
                    *value = block.activation.apply(*value, i, self.activation);
                    if !value.is_finite() {
                        return Err(invalid(
                            "ConvNet activation produced a non-finite intermediate",
                        ));
                    }
                }
            }
            // Native Eigen head computes dot first, then adds bias.
            let mut value = 0.0_f32;
            for (weight, input) in self.head.iter().zip(values) {
                value += *weight * input;
            }
            *result = value + self.bias;
            if !result.is_finite() {
                return Err(invalid("ConvNet head produced a non-finite output"));
            }
        }
        Ok(())
    }
}
