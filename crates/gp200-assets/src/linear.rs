//! Exact causal FIR equations from NAM Core 0b3d3c97; scalar offline backend.
use crate::wavenet::{Budget, boolean, integer, known_keys};
use crate::{SampleProcessor, invalid};
use gp200_domain::ServiceError;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct LinearModel {
    coefficients: Vec<f32>,
    bias: f32,
    history: Vec<f32>,
    position: usize,
    rate: u32,
}
impl LinearModel {
    pub(crate) fn from_json(
        config: &Value,
        weights: &[f32],
        rate: u32,
        budget: &mut Budget,
    ) -> Result<Self, ServiceError> {
        known_keys(
            config,
            &[
                "receptive_field",
                "bias",
                "in_channels",
                "out_channels",
                "implementation",
            ],
        )?;
        if integer(config, "in_channels", Some(1), 256)? != 1
            || integer(config, "out_channels", Some(1), 256)? != 1
        {
            return Err(ServiceError::unsupported(
                "Local Linear input/output must be mono",
            ));
        }
        let length = integer(config, "receptive_field", None, 65_536)?;
        let bias = boolean(config, "bias", false)?;
        if config.get("bias").is_none() {
            return Err(invalid("Linear requires boolean bias"));
        }
        let implementation = config.get("implementation").map_or(Ok("auto"), |v| {
            v.as_str()
                .ok_or_else(|| invalid("Linear implementation must be a string"))
        })?;
        match implementation.to_ascii_lowercase().as_str() {
            "auto" | "direct" | "legacy" | "old" => {}
            "fft" | "partitioned_fft" | "partitioned-fft" => {
                return Err(ServiceError::unsupported(
                    "Explicit Linear FFT backend has no implemented local kernel; use an explicitly selected direct profile",
                ));
            }
            _ => return Err(ServiceError::unsupported("Unknown Linear implementation")),
        }
        if weights.len() != length + usize::from(bias) {
            return Err(invalid(
                "Linear parameter count differs from FIR length and bias",
            ));
        }
        budget.weights = budget
            .weights
            .checked_add(weights.len())
            .ok_or_else(|| invalid("NAM total weights overflow"))?;
        budget.state = budget
            .state
            .checked_add(length)
            .ok_or_else(|| invalid("NAM total state overflow"))?;
        if budget.weights > 4_194_304 || budget.state > 16_777_216 {
            return Err(invalid("NAM aggregate weights/state exceed local bounds"));
        }
        budget.operations += length as u64;
        Ok(Self {
            coefficients: weights[..length].to_vec(),
            bias: if bias { weights[length] } else { 0.0 },
            history: vec![0.0; length],
            position: 0,
            rate,
        })
    }
    pub fn operations_per_sample(&self) -> u64 {
        self.coefficients.len() as u64
    }
}
impl SampleProcessor for LinearModel {
    fn sample_rate(&self) -> u32 {
        self.rate
    }
    fn reset(&mut self) -> Result<(), ServiceError> {
        self.history.fill(0.0);
        self.position = 0;
        Ok(())
    }
    fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), ServiceError> {
        if input.len() != output.len() {
            return Err(invalid("Linear input/output lengths differ"));
        }
        let length = self.coefficients.len();
        for (sample, result) in input.iter().zip(output) {
            if !sample.is_finite() {
                return Err(invalid("NAM input is non-finite"));
            }
            self.history[self.position] = *sample;
            let mut value = 0.0_f32;
            // Native direct dot uses oldest-to-current input and reversed exported impulse.
            for age in (0..length).rev() {
                value +=
                    self.coefficients[age] * self.history[(self.position + length - age) % length];
            }
            *result = self.bias + value;
            if !result.is_finite() {
                return Err(invalid("Linear FIR produced a non-finite output"));
            }
            self.position = (self.position + 1) % length;
        }
        Ok(())
    }
}
