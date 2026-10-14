//! NAM SlimmableContainer selects the final/full child by default, as NAM Core does.
use crate::{ActivationMode, NamModel, SampleProcessor, invalid};
use gp200_domain::ServiceError;
#[derive(Debug, Clone)]
pub struct ContainerModel {
    pub(crate) models: Vec<NamModel>,
    pub(crate) thresholds: Vec<f64>,
    pub(crate) active: usize,
    pub(crate) rate: u32,
}
impl ContainerModel {
    pub fn set_activation(&mut self, mode: ActivationMode) {
        for model in &mut self.models {
            model.set_activation(mode);
        }
    }
    /// An explicit local size selection, never an audio-dependent model replacement.
    pub fn set_size(&mut self, size: f64) -> Result<(), ServiceError> {
        if !size.is_finite() || !(0.0..=1.0).contains(&size) {
            return Err(invalid("NAM container size must be finite in 0–1"));
        }
        let index = self
            .thresholds
            .iter()
            .position(|max| size < *max)
            .unwrap_or(self.models.len() - 1);
        if index != self.active {
            self.models[index].reset()?;
            self.active = index;
        }
        Ok(())
    }
    pub fn operations_per_sample(&self) -> u64 {
        self.models[self.active].operations_per_sample()
    }
    pub fn selected_index(&self) -> usize {
        self.active
    }
}
impl SampleProcessor for ContainerModel {
    fn sample_rate(&self) -> u32 {
        self.rate
    }
    fn reset(&mut self) -> Result<(), ServiceError> {
        self.models[self.active].reset()
    }
    fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), ServiceError> {
        self.models[self.active].process(input, output)
    }
}
