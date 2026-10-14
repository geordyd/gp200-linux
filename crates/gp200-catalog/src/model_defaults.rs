//! Sparse model selection defaults. The editor metadata defines values only
//! for declared parameter IDs; a model change does not erase other float slots.

use crate::ModelDefinition;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelDefaultPlan {
    pub code: u32,
    /// XML parameter IDs and exact IEEE-754 bits, independent of display order.
    pub declared: Vec<(u8, u32)>,
    /// Declared controls whose shipped default could not be decoded.
    pub unknown_defaults: Vec<u8>,
}

impl ModelDefaultPlan {
    pub fn from_definition(model: &ModelDefinition) -> Self {
        Self {
            code: model.code,
            declared: model
                .default_values()
                .into_iter()
                .map(|(id, value)| (id, value.to_bits()))
                .collect(),
            unknown_defaults: model
                .parameters
                .iter()
                .filter(|parameter| parameter.default.is_none())
                .map(|parameter| parameter.parameter_id)
                .collect(),
        }
    }

    /// Validate the complete plan before changing any value. Missing and
    /// undeclared defaults retain their original bits, including NaN payloads.
    pub fn apply_to_bits(&self, parameters: &mut [u32]) -> Result<(), &'static str> {
        if self
            .declared
            .iter()
            .any(|(id, _)| usize::from(*id) >= parameters.len())
        {
            return Err("Model defaults require a complete parameter array");
        }
        for &(id, bits) in &self.declared {
            parameters[usize::from(id)] = bits;
        }
        Ok(())
    }

    /// Overlay only Code and recovered defaults in a native 68-byte module
    /// body. Header, bypass state, hidden controls and opaque bits stay intact.
    pub fn apply_to_module_body(&self, body: &mut [u8]) -> Result<(), &'static str> {
        if body.len() != 68
            || u16::from_le_bytes([body[2], body[3]]) != 15
            || self.declared.iter().any(|(id, _)| *id >= 15)
        {
            return Err("Model defaults require the recovered 15-slot module shape");
        }
        body[4..8].copy_from_slice(&self.code.to_le_bytes());
        for &(id, bits) in &self.declared {
            let start = 8 + usize::from(id) * 4;
            body[start..start + 4].copy_from_slice(&bits.to_le_bytes());
        }
        Ok(())
    }
}
