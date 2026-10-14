//! Bounded local asset storage and audio preparation. No MIDI access or upload tickets.
mod audio;
mod calibration;
mod container;
mod convnet;
mod firmware;
mod ir_device;
mod linear;
mod lstm;
mod nam;
mod sequential;
mod snaptone;
mod store;
mod wavenet;
mod worker;

pub use audio::{
    AudioInfo, ChannelMode, PrepareOptions, WaveAudio, WaveEncoding, WaveformBin, inspect_wav,
    prepare_audio, write_float_wav, write_pcm16_wav,
};
pub use calibration::{
    CalibratedSnapTone, CalibrationOptions, CalibrationReport, GP200_STIMULUS_WAV_SHA256,
    calibrate_nam_to_clo, calibration_helper_available, calibration_helper_sha256,
};
pub use container::ContainerModel;
pub use convnet::ConvNetModel;
pub use firmware::{FirmwareImage, FirmwareInfo, FirmwareSection, inspect_firmware};
pub use ir_device::{
    DEVICE_IR_BYTES, DEVICE_IR_SAMPLE_RATE, DEVICE_IR_SAMPLES, IrCoefficientInfo, IrCoefficients,
    IrLengthPolicy, pack_device_ir,
};
pub use linear::LinearModel;
pub use lstm::{ActivationMode, LstmModel};
pub use nam::{
    DEFAULT_NAM_RENDER_OPERATIONS, MAX_NAM_RENDER_OPERATIONS, NamInfo, NamModel, NamSizePlan,
    NamSizeSelection, RenderReport, SampleProcessor, inspect_nam, inspect_nam_with_size,
    render_host_stage,
};
pub use sequential::SequentialModel;
pub use snaptone::{
    CLO_BYTES, CLO_COEFFICIENT_BYTES, CLO_COEFFICIENT_OFFSET, CloCoefficientRange,
    GP200_CLO_COEFFICIENT_BYTES, GP200_CLO_DECLARED_BYTES, SnapToneContainer, SnapToneInfo,
    clo_crc16, inspect_snaptone,
};
pub use store::{AssetManifest, AssetStore, ConversionRecipe, LocalAssetResult, data_dir};
pub use wavenet::{SlimmableWaveNetModel, WaveNetModel};
pub use worker::AssetWorker;

pub const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_SAMPLES: usize = 16 * 1024 * 1024;
pub const MAX_ASSETS: usize = 4096;

fn invalid(message: impl Into<String>) -> gp200_domain::ServiceError {
    gp200_domain::ServiceError::invalid(message)
}
fn cancelled() -> gp200_domain::ServiceError {
    gp200_domain::ServiceError::new(
        gp200_domain::ErrorKind::Cancelled,
        "Local preparation cancelled; no pedal transfer started",
    )
}
