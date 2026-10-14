//! Exact final IR packing stage recovered at native 0x5cca90/0x5cc660.
//! Resampling/mixing must happen separately; this function accepts only the
//! native conversion stage's mono PCM24/44100 input, without altering it.
use crate::{WaveEncoding, inspect_wav, invalid};
use gp200_domain::ServiceError;
use serde::Serialize;

pub const DEVICE_IR_SAMPLE_RATE: u32 = 44_100;
pub const DEVICE_IR_SAMPLES: usize = 1024;
pub const DEVICE_IR_BYTES: usize = DEVICE_IR_SAMPLES * 4;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrLengthPolicy {
    RejectTruncation,
    KeepFirst1024,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IrCoefficientInfo {
    pub source_samples: usize,
    pub retained_samples: usize,
    pub padded_samples: usize,
    pub truncated_samples: usize,
    pub byte_sum: u32,
}
#[derive(Debug, Clone)]
pub struct IrCoefficients {
    raw: [u8; DEVICE_IR_BYTES],
    info: IrCoefficientInfo,
}
impl IrCoefficients {
    pub fn raw(&self) -> &[u8; DEVICE_IR_BYTES] {
        &self.raw
    }
    pub fn info(&self) -> &IrCoefficientInfo {
        &self.info
    }
}
pub fn pack_device_ir(wav: &[u8], policy: IrLengthPolicy) -> Result<IrCoefficients, ServiceError> {
    let audio = inspect_wav(wav)?;
    if audio.channels != 1
        || audio.source_encoding != WaveEncoding::Pcm
        || audio.source_bits != 24
        || audio.sample_rate != DEVICE_IR_SAMPLE_RATE
    {
        return Err(invalid(
            "Exact GP-200 IR packing needs mono PCM24 WAV at 44100 Hz; prepare resampling and channel selection first",
        ));
    }
    if audio.samples.len() > DEVICE_IR_SAMPLES && policy == IrLengthPolicy::RejectTruncation {
        return Err(invalid(
            "IR exceeds 1024 samples; explicitly choose KeepFirst1024 to reproduce native truncation",
        ));
    }
    let retained_samples = audio.samples.len().min(DEVICE_IR_SAMPLES);
    let mut raw = [0_u8; DEVICE_IR_BYTES];
    // Every signed PCM24 integer is exactly representable as f32; the WAV
    // decoder's power-of-two scaling therefore loses no source sample bits.
    for (index, sample) in audio.samples[..retained_samples].iter().enumerate() {
        let integer = (*sample * 8_388_608.0) as i32;
        raw[index * 4..index * 4 + 4].copy_from_slice(&integer.to_le_bytes());
    }
    let info = IrCoefficientInfo {
        source_samples: audio.samples.len(),
        retained_samples,
        padded_samples: DEVICE_IR_SAMPLES - retained_samples,
        truncated_samples: audio.samples.len() - retained_samples,
        byte_sum: raw.iter().map(|value| u32::from(*value)).sum(),
    };
    Ok(IrCoefficients { raw, info })
}
