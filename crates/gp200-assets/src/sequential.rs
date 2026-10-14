//! Mono Sequential NAM dispatch, following the pinned Core child/rate/link contract.
use crate::{ActivationMode, NamModel, SampleProcessor, invalid};
use gp200_domain::ServiceError;
#[derive(Debug, Clone)]
pub struct SequentialModel {
    pub(crate) models: Vec<NamModel>,
    pub(crate) rate: u32,
}
impl SequentialModel {
    pub fn set_activation(&mut self, mode: ActivationMode) {
        for model in &mut self.models {
            model.set_activation(mode);
        }
    }
    pub fn operations_per_sample(&self) -> u64 {
        self.models
            .iter()
            .map(NamModel::operations_per_sample)
            .sum()
    }
}
impl SampleProcessor for SequentialModel {
    fn sample_rate(&self) -> u32 {
        self.rate
    }
    fn reset(&mut self) -> Result<(), ServiceError> {
        for model in &mut self.models {
            model.reset()?;
        }
        Ok(())
    }
    fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), ServiceError> {
        if input.len() != output.len() {
            return Err(invalid("Sequential input/output lengths differ"));
        }
        if input.iter().any(|x| !x.is_finite()) {
            return Err(invalid("Sequential input is non-finite"));
        }
        // Bound scratch to the same 1024-frame block used by the asset worker; callers can
        // submit longer buffers without causing unbounded intermediate allocations.
        for (input, output) in input.chunks(1024).zip(output.chunks_mut(1024)) {
            let mut a = [0.0_f32; 1024];
            let mut b = [0.0_f32; 1024];
            a[..input.len()].copy_from_slice(input);
            for model in &mut self.models {
                model.process(&a[..input.len()], &mut b[..input.len()])?;
                std::mem::swap(&mut a, &mut b);
            }
            output.copy_from_slice(&a[..input.len()]);
        }
        Ok(())
    }
}
