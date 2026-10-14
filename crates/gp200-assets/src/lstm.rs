// NAM LSTM weight layout and gate equations derived from NeuralAmpModelerCore,
// Copyright (c) 2023 Steven Atkinson, MIT. Full notice: assets/licenses/neural-amp-modeler-core-MIT.txt
use crate::{SampleProcessor, invalid};
use gp200_domain::ServiceError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ActivationMode {
    #[default]
    Standard,
    Fast,
}
#[derive(Debug, Clone)]
struct Cell {
    input_size: usize,
    hidden_size: usize,
    weights: Vec<f32>,
    bias: Vec<f32>,
    initial_hidden: Vec<f32>,
    initial_cell: Vec<f32>,
    hidden: Vec<f32>,
    cell: Vec<f32>,
    xh: Vec<f32>,
    gates: Vec<f32>,
}
impl Cell {
    fn process(&mut self, input: &[f32], mode: ActivationMode) {
        self.xh[..self.input_size].copy_from_slice(input);
        self.xh[self.input_size..].copy_from_slice(&self.hidden);
        let width = self.xh.len();
        for (row, gate) in self.gates.iter_mut().enumerate() {
            // Bias is added after the matrix-vector sum, matching Core's operation order.
            let mut sum = 0.0_f32;
            for (weight, input) in self.weights[row * width..(row + 1) * width]
                .iter()
                .zip(&self.xh)
            {
                sum += weight * input;
            }
            *gate = sum + self.bias[row];
        }
        for index in 0..self.hidden_size {
            self.cell[index] = sigmoid(self.gates[self.hidden_size + index], mode)
                * self.cell[index]
                + sigmoid(self.gates[index], mode)
                    * tanh(self.gates[2 * self.hidden_size + index], mode);
        }
        for index in 0..self.hidden_size {
            self.hidden[index] = sigmoid(self.gates[3 * self.hidden_size + index], mode)
                * tanh(self.cell[index], mode);
        }
    }
    fn reset(&mut self) {
        self.hidden.copy_from_slice(&self.initial_hidden);
        self.cell.copy_from_slice(&self.initial_cell);
    }
}
pub(crate) fn tanh(x: f32, mode: ActivationMode) -> f32 {
    match mode {
        ActivationMode::Standard => x.tanh(),
        ActivationMode::Fast => {
            let ax = x.abs();
            let x2 = x * x;
            x * (2.4555075 + 2.4555075 * ax + (0.89322984 + 0.82122666 * ax) * x2)
                / (2.4450665 + (2.4450665 + x2) * (x + 0.8146427 * x * ax).abs())
        }
    }
}
pub(crate) fn sigmoid(x: f32, mode: ActivationMode) -> f32 {
    match mode {
        ActivationMode::Standard => 1.0 / (1.0 + (-x).exp()),
        ActivationMode::Fast => 0.5 * (tanh(x * 0.5, mode) + 1.0),
    }
}
#[derive(Debug, Clone)]
pub struct LstmModel {
    rate: u32,
    layers: Vec<Cell>,
    head: Vec<f32>,
    bias: f32,
    activation: ActivationMode,
    operations_per_sample: u64,
}
pub(crate) fn config_dimensions(config: &Value) -> Result<(usize, usize), ServiceError> {
    crate::wavenet::known_keys(
        config,
        &[
            "input_size",
            "hidden_size",
            "num_layers",
            "in_channels",
            "out_channels",
        ],
    )?;
    let field = |name: &str| {
        config
            .get(name)
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| invalid(format!("NAM LSTM config requires integer {name}")))
    };
    let channels = |name: &str| match config.get(name) {
        None => Ok(1),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| invalid(format!("NAM LSTM {name} must be an integer"))),
    };
    if field("input_size")? != 1 || channels("in_channels")? != 1 || channels("out_channels")? != 1
    {
        return Err(ServiceError::unsupported(
            "Local LSTM rendering currently supports one input and one output channel only",
        ));
    }
    let hidden = field("hidden_size")?;
    let layers = field("num_layers")?;
    if !(1..=64).contains(&hidden) || !(1..=4).contains(&layers) {
        return Err(ServiceError::unsupported(
            "LSTM exceeds local processing limits: hidden_size 1–64, num_layers 1–4",
        ));
    }
    Ok((hidden, layers))
}
pub(crate) fn expected_weights(config: &Value) -> Result<usize, ServiceError> {
    let (h, layers) = config_dimensions(config)?;
    Ok(4 * h * (1 + h) + 6 * h + (layers - 1) * (4 * h * (h + h) + 6 * h) + h + 1)
}
impl LstmModel {
    pub(crate) fn from_json(
        config: &Value,
        weights: &[f32],
        rate: u32,
    ) -> Result<Self, ServiceError> {
        let (hidden, layers) = config_dimensions(config)?;
        if weights.len() != expected_weights(config)? {
            return Err(invalid(format!(
                "NAM LSTM weight length {} differs from required {}",
                weights.len(),
                expected_weights(config)?
            )));
        }
        let mut offset = 0;
        let mut take = |count: usize| {
            let values = weights[offset..offset + count].to_vec();
            offset += count;
            values
        };
        let mut cells = Vec::with_capacity(layers);
        let mut operations_per_sample = 0;
        for layer in 0..layers {
            let input_size = if layer == 0 { 1 } else { hidden };
            let width = input_size + hidden;
            let matrix = take(4 * hidden * width);
            let bias = take(4 * hidden);
            let initial_hidden = take(hidden);
            let initial_cell = take(hidden);
            operations_per_sample += (4 * hidden * width) as u64;
            cells.push(Cell {
                input_size,
                hidden_size: hidden,
                weights: matrix,
                bias,
                hidden: initial_hidden.clone(),
                cell: initial_cell.clone(),
                initial_hidden,
                initial_cell,
                xh: vec![0.0; width],
                gates: vec![0.0; 4 * hidden],
            });
        }
        let head = take(hidden);
        let bias = take(1)[0];
        Ok(Self {
            rate,
            layers: cells,
            head,
            bias,
            activation: ActivationMode::Standard,
            operations_per_sample: operations_per_sample + hidden as u64,
        })
    }
    pub fn set_activation(&mut self, mode: ActivationMode) {
        self.activation = mode;
    }
    pub fn operations_per_sample(&self) -> u64 {
        self.operations_per_sample
    }
    fn sample(&mut self, input: f32) -> f32 {
        self.layers[0].process(&[input], self.activation);
        for index in 1..self.layers.len() {
            let (previous, current) = self.layers.split_at_mut(index);
            current[0].process(&previous[index - 1].hidden, self.activation);
        }
        let hidden = &self.layers.last().expect("validated layers").hidden;
        let mut value = 0.0;
        for (weight, hidden) in self.head.iter().zip(hidden) {
            value += weight * hidden;
        }
        value + self.bias
    }
}
impl SampleProcessor for LstmModel {
    fn sample_rate(&self) -> u32 {
        self.rate
    }
    fn reset(&mut self) -> Result<(), ServiceError> {
        for layer in &mut self.layers {
            layer.reset();
        }
        Ok(())
    }
    fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), ServiceError> {
        if input.len() != output.len() {
            return Err(invalid("LSTM input/output lengths differ"));
        }
        for (input, output) in input.iter().zip(output) {
            if !input.is_finite() {
                return Err(invalid("LSTM input is non-finite"));
            }
            *output = self.sample(*input);
            if !output.is_finite() {
                return Err(invalid("LSTM produced non-finite output"));
            }
        }
        Ok(())
    }
}
