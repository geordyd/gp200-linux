use crate::{MAX_SAMPLES, MAX_SOURCE_BYTES, cancelled, invalid};
use gp200_domain::ServiceError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WaveEncoding {
    Pcm,
    Float,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaveAudio {
    pub sample_rate: u32,
    pub channels: u16,
    pub source_encoding: WaveEncoding,
    pub source_bits: u16,
    pub samples: Vec<f32>,
}
impl WaveAudio {
    pub fn frames(&self) -> usize {
        self.samples.len() / usize::from(self.channels)
    }
    pub fn validate(&self) -> Result<(), ServiceError> {
        if self.channels == 0
            || self.channels > 8
            || self.sample_rate == 0
            || self.sample_rate > 384_000
            || self.samples.len() > MAX_SAMPLES
            || self.samples.is_empty()
            || !self
                .samples
                .len()
                .is_multiple_of(usize::from(self.channels))
        {
            return Err(invalid(
                "Audio must contain complete frames, 1–8 channels, a rate up to 384000 Hz, and at most 16777216 samples (local processing limits)",
            ));
        }
        if self.samples.iter().any(|x| !x.is_finite()) {
            return Err(invalid(
                "Audio contains NaN or infinity; original retained, preparation rejected",
            ));
        }
        Ok(())
    }
    pub fn inspect(&self) -> Result<AudioInfo, ServiceError> {
        self.validate()?;
        let mut peak = 0.0_f32;
        let mut sum_squares = 0.0_f64;
        let mut sum = 0.0_f64;
        let mut clipped = 0_u64;
        let positive_limit = if self.source_encoding == WaveEncoding::Pcm {
            1.0_f64 - 2.0_f64.powi(1 - i32::from(self.source_bits))
        } else {
            1.0
        };
        for value in &self.samples {
            peak = peak.max(value.abs());
            sum += f64::from(*value);
            sum_squares += f64::from(*value).powi(2);
            if f64::from(*value) >= positive_limit || *value <= -1.0 {
                clipped += 1;
            }
        }
        let frames = self.frames();
        let count = frames.min(256);
        let mut waveform = Vec::with_capacity(count);
        for bin in 0..count {
            let first = bin * frames / count;
            let last = ((bin + 1) * frames / count).max(first + 1);
            let chunk = &self.samples
                [first * usize::from(self.channels)..last * usize::from(self.channels)];
            let min = chunk.iter().copied().fold(f32::INFINITY, f32::min);
            let max = chunk.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            waveform.push(WaveformBin { min, max });
        }
        Ok(AudioInfo {
            sample_rate: self.sample_rate,
            channels: self.channels,
            encoding: self.source_encoding,
            bits: self.source_bits,
            frames: frames as u64,
            seconds: frames as f64 / f64::from(self.sample_rate),
            peak,
            rms: (sum_squares / self.samples.len() as f64).sqrt(),
            dc: sum / self.samples.len() as f64,
            clipped_samples: clipped,
            waveform,
        })
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaveformBin {
    pub min: f32,
    pub max: f32,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub encoding: WaveEncoding,
    pub bits: u16,
    pub frames: u64,
    pub seconds: f64,
    pub peak: f32,
    pub rms: f64,
    pub dc: f64,
    pub clipped_samples: u64,
    pub waveform: Vec<WaveformBin>,
}
fn u16_le(bytes: &[u8], offset: usize) -> Result<u16, ServiceError> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .ok_or_else(|| invalid("Truncated WAV field"))?
            .try_into()
            .expect("length checked"),
    ))
}
fn u32_le(bytes: &[u8], offset: usize) -> Result<u32, ServiceError> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or_else(|| invalid("Truncated WAV field"))?
            .try_into()
            .expect("length checked"),
    ))
}
/// RIFF/WAVE decoding with validated chunk extents; local limits are not pedal constraints.
pub fn inspect_wav(bytes: &[u8]) -> Result<WaveAudio, ServiceError> {
    if bytes.len() > MAX_SOURCE_BYTES
        || bytes.len() < 12
        || &bytes[..4] != b"RIFF"
        || &bytes[8..12] != b"WAVE"
    {
        return Err(invalid("Expected a bounded little-endian RIFF/WAVE file"));
    }
    let end = (u32_le(bytes, 4)? as usize)
        .checked_add(8)
        .ok_or_else(|| invalid("WAV length overflow"))?;
    if end != bytes.len() {
        return Err(invalid("RIFF declared length differs from file length"));
    }
    let mut pos = 12;
    let mut fmt = None;
    let mut data = None;
    while pos < end {
        let header = bytes
            .get(pos..pos + 8)
            .ok_or_else(|| invalid("Truncated WAV chunk header"))?;
        let length = u32_le(header, 4)? as usize;
        let start = pos + 8;
        let last = start
            .checked_add(length)
            .ok_or_else(|| invalid("WAV chunk length overflow"))?;
        let chunk = bytes
            .get(start..last)
            .ok_or_else(|| invalid("WAV chunk exceeds RIFF bounds"))?;
        match &header[..4] {
            b"fmt " => {
                if fmt.replace(chunk).is_some() {
                    return Err(invalid("Duplicate WAV format chunk"));
                }
            }
            b"data" if data.replace(chunk).is_some() => {
                return Err(invalid("Multiple WAV data chunks are unsupported"));
            }
            _ => {}
        }
        pos = last
            .checked_add(length & 1)
            .ok_or_else(|| invalid("WAV padding overflow"))?;
        if pos > end {
            return Err(invalid("Missing odd WAV chunk padding"));
        }
    }
    let fmt = fmt.ok_or_else(|| invalid("Missing WAV format chunk"))?;
    if fmt.len() < 16 {
        return Err(invalid("Short WAV format chunk"));
    }
    let mut tag = u16_le(fmt, 0)?;
    let channels = u16_le(fmt, 2)?;
    let sample_rate = u32_le(fmt, 4)?;
    let byte_rate = u32_le(fmt, 8)?;
    let block_align = u16_le(fmt, 12)?;
    let bits = u16_le(fmt, 14)?;
    if tag == 0xfffe {
        if fmt.len() < 40
            || u16_le(fmt, 16)? < 22
            || u16_le(fmt, 18)? != bits
            || fmt[26..40] != [0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71]
        {
            return Err(ServiceError::unsupported(
                "Unsupported WAVE extensible subtype or packed valid-bit layout",
            ));
        }
        tag = u16_le(fmt, 24)?;
    }
    let encoding = match (tag, bits) {
        (1, 8 | 16 | 24 | 32) => WaveEncoding::Pcm,
        (3, 32 | 64) => WaveEncoding::Float,
        _ => {
            return Err(ServiceError::unsupported(format!(
                "Unsupported WAV format {tag}, {bits} bits; PCM 8/16/24/32 and float 32/64 are supported locally"
            )));
        }
    };
    if channels == 0 || channels > 8 || sample_rate == 0 || sample_rate > 384_000 {
        return Err(invalid(
            "WAV channels or sample rate exceed local processing bounds",
        ));
    }
    let width = usize::from(bits / 8);
    if usize::from(block_align) != width * usize::from(channels)
        || byte_rate
            != sample_rate
                .checked_mul(u32::from(block_align))
                .ok_or_else(|| invalid("WAV byte rate overflow"))?
    {
        return Err(invalid("WAV block alignment or byte rate is inconsistent"));
    }
    let data = data.ok_or_else(|| invalid("Missing WAV data chunk"))?;
    if data.is_empty()
        || data.len() % usize::from(block_align) != 0
        || data.len() / width > MAX_SAMPLES
    {
        return Err(invalid(
            "WAV data is empty, exceeds local bounds, or ends in a partial frame",
        ));
    }
    let mut samples = Vec::with_capacity(data.len() / width);
    for value in data.chunks_exact(width) {
        let sample = match (encoding, bits) {
            (WaveEncoding::Pcm, 8) => (f32::from(value[0]) - 128.0) / 128.0,
            (WaveEncoding::Pcm, 16) => {
                f32::from(i16::from_le_bytes(value.try_into().expect("width"))) / 32768.0
            }
            (WaveEncoding::Pcm, 24) => {
                let packed =
                    (i32::from(value[0]) | i32::from(value[1]) << 8 | i32::from(value[2]) << 16)
                        << 8
                        >> 8;
                packed as f32 / 8_388_608.0
            }
            (WaveEncoding::Pcm, 32) => {
                i32::from_le_bytes(value.try_into().expect("width")) as f32 / 2_147_483_648.0
            }
            (WaveEncoding::Float, 32) => f32::from_le_bytes(value.try_into().expect("width")),
            (WaveEncoding::Float, 64) => {
                f64::from_le_bytes(value.try_into().expect("width")) as f32
            }
            _ => unreachable!(),
        };
        if !sample.is_finite() {
            return Err(invalid("WAV contains non-finite float samples"));
        }
        samples.push(sample);
    }
    Ok(WaveAudio {
        sample_rate,
        channels,
        source_encoding: encoding,
        source_bits: bits,
        samples,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ChannelMode {
    #[default]
    Preserve,
    Select(u16),
    Average,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PrepareOptions {
    pub sample_rate: Option<u32>,
    pub channel: ChannelMode,
    pub normalize_peak: Option<f32>,
    pub truncate_frames: Option<u64>,
}
impl PrepareOptions {
    pub fn from_options(options: &BTreeMap<String, String>) -> Result<Self, ServiceError> {
        let mut result = Self::default();
        for (key, value) in options {
            match key.as_str() {
                "target_slot" => {
                    value
                        .parse::<u8>()
                        .map_err(|_| invalid("Invalid target slot"))?;
                }
                "sample_rate" => {
                    result.sample_rate =
                        Some(value.parse().map_err(|_| invalid("Invalid sample rate"))?)
                }
                "channel" => {
                    result.channel = match value.as_str() {
                        "preserve" => ChannelMode::Preserve,
                        "average" => ChannelMode::Average,
                        _ => ChannelMode::Select(value.parse().map_err(|_| {
                            invalid("Channel is preserve, average, or a zero-based channel index")
                        })?),
                    }
                }
                "normalize_peak" => {
                    result.normalize_peak = Some(
                        value
                            .parse()
                            .map_err(|_| invalid("Invalid normalization peak"))?,
                    )
                }
                "truncate_frames" => {
                    result.truncate_frames = Some(
                        value
                            .parse()
                            .map_err(|_| invalid("Invalid truncation frame count"))?,
                    )
                }
                _ => return Err(invalid(format!("Unknown preparation option: {key}"))),
            }
        }
        if result
            .sample_rate
            .is_some_and(|rate| rate == 0 || rate > 384_000)
            || result
                .normalize_peak
                .is_some_and(|peak| !peak.is_finite() || peak <= 0.0 || peak > 1.0)
            || result.truncate_frames == Some(0)
        {
            return Err(invalid("Invalid local preparation limits"));
        }
        Ok(result)
    }
}
/// Explicit channel selection, finite windowed-sinc resampling, truncation and normalization.
/// These are local tools, not a reverse-engineered GP-200 coefficient profile.
pub fn prepare_audio(
    source: &WaveAudio,
    options: &PrepareOptions,
    is_cancelled: impl Fn() -> bool,
) -> Result<WaveAudio, ServiceError> {
    source.validate()?;
    if options
        .sample_rate
        .is_some_and(|rate| rate == 0 || rate > 384_000)
        || options
            .normalize_peak
            .is_some_and(|peak| !peak.is_finite() || peak <= 0.0 || peak > 1.0)
        || options.truncate_frames == Some(0)
    {
        return Err(invalid("Invalid preparation options"));
    }
    if let ChannelMode::Select(channel) = options.channel
        && channel >= source.channels
    {
        return Err(invalid("Selected channel does not exist"));
    }
    let channels = if options.channel == ChannelMode::Preserve {
        source.channels
    } else {
        1
    };
    let mut samples = Vec::with_capacity(source.frames() * usize::from(channels));
    for (index, frame) in source
        .samples
        .chunks_exact(usize::from(source.channels))
        .enumerate()
    {
        if index % 1024 == 0 && is_cancelled() {
            return Err(cancelled());
        }
        match options.channel {
            ChannelMode::Preserve => samples.extend_from_slice(frame),
            ChannelMode::Select(channel) => samples.push(frame[usize::from(channel)]),
            ChannelMode::Average => samples.push(
                (frame.iter().map(|value| f64::from(*value)).sum::<f64>() / frame.len() as f64)
                    as f32,
            ),
        }
    }
    let mut audio = WaveAudio {
        sample_rate: source.sample_rate,
        channels,
        source_encoding: WaveEncoding::Float,
        source_bits: 32,
        samples,
    };
    if let Some(rate) = options
        .sample_rate
        .filter(|rate| *rate != audio.sample_rate)
    {
        audio = resample(&audio, rate, &is_cancelled)?;
    }
    if let Some(frames) = options.truncate_frames {
        audio.samples.truncate(
            usize::try_from(frames)
                .unwrap_or(usize::MAX)
                .saturating_mul(usize::from(channels)),
        );
    }
    if let Some(target) = options.normalize_peak {
        let peak = audio
            .samples
            .iter()
            .map(|value| value.abs())
            .fold(0.0_f32, f32::max);
        if peak > 0.0 {
            let scale = f64::from(target) / f64::from(peak);
            for (index, value) in audio.samples.iter_mut().enumerate() {
                if index % 1024 == 0 && is_cancelled() {
                    return Err(cancelled());
                }
                *value = (f64::from(*value) * scale) as f32;
            }
        }
    }
    audio.validate()?;
    Ok(audio)
}
fn resample(
    source: &WaveAudio,
    rate: u32,
    is_cancelled: &impl Fn() -> bool,
) -> Result<WaveAudio, ServiceError> {
    let frames = (source.frames() as u64 * u64::from(rate)).div_ceil(u64::from(source.sample_rate));
    let count = usize::try_from(frames)
        .ok()
        .and_then(|n| n.checked_mul(usize::from(source.channels)))
        .filter(|n| *n <= MAX_SAMPLES)
        .ok_or_else(|| invalid("Resampled audio exceeds local processing bounds"))?;
    let mut output = vec![0.0; count];
    let ratio = f64::from(source.sample_rate) / f64::from(rate);
    let cutoff = (f64::from(rate) / f64::from(source.sample_rate)).min(1.0);
    // 32 taps per side; cutoff follows downsampling to suppress aliasing.
    let radius = (32.0 / cutoff).ceil().min(512.0) as isize;
    for frame in 0..frames as usize {
        if frame % 256 == 0 && is_cancelled() {
            return Err(cancelled());
        }
        let position = frame as f64 * ratio;
        let center = position.floor() as isize;
        for channel in 0..usize::from(source.channels) {
            let mut sum = 0.0;
            let mut weights = 0.0;
            for index in center - radius..=center + radius {
                if index < 0 || index >= source.frames() as isize {
                    continue;
                }
                let distance = position - index as f64;
                let angle = std::f64::consts::PI * distance * cutoff;
                let sinc = if angle.abs() < 1e-12 {
                    1.0
                } else {
                    angle.sin() / angle
                };
                let window = 0.5 * (1.0 + (std::f64::consts::PI * distance / radius as f64).cos());
                let weight = cutoff * sinc * window;
                weights += weight;
                sum += weight
                    * f64::from(
                        source.samples[index as usize * usize::from(source.channels) + channel],
                    );
            }
            output[frame * usize::from(source.channels) + channel] = if weights.abs() < 1e-12 {
                0.0
            } else {
                (sum / weights) as f32
            };
        }
    }
    Ok(WaveAudio {
        sample_rate: rate,
        channels: source.channels,
        source_encoding: WaveEncoding::Float,
        source_bits: 32,
        samples: output,
    })
}
fn wav_header(audio: &WaveAudio, tag: u16, bits: u16) -> Result<Vec<u8>, ServiceError> {
    audio.validate()?;
    let bytes = audio
        .samples
        .len()
        .checked_mul(usize::from(bits / 8))
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| invalid("WAV output size overflow"))?;
    if bytes as usize + 44 > MAX_SOURCE_BYTES {
        return Err(invalid("WAV output exceeds local byte limit"));
    }
    let align = audio.channels * (bits / 8);
    let mut output = Vec::with_capacity(bytes as usize + 44);
    output.extend_from_slice(b"RIFF");
    output.extend_from_slice(&(bytes + 36).to_le_bytes());
    output.extend_from_slice(b"WAVEfmt ");
    output.extend_from_slice(&16_u32.to_le_bytes());
    output.extend_from_slice(&tag.to_le_bytes());
    output.extend_from_slice(&audio.channels.to_le_bytes());
    output.extend_from_slice(&audio.sample_rate.to_le_bytes());
    output.extend_from_slice(&(audio.sample_rate * u32::from(align)).to_le_bytes());
    output.extend_from_slice(&align.to_le_bytes());
    output.extend_from_slice(&bits.to_le_bytes());
    output.extend_from_slice(b"data");
    output.extend_from_slice(&bytes.to_le_bytes());
    Ok(output)
}
pub fn write_float_wav(audio: &WaveAudio) -> Result<Vec<u8>, ServiceError> {
    let mut output = wav_header(audio, 3, 32)?;
    for sample in &audio.samples {
        output.extend_from_slice(&sample.to_le_bytes());
    }
    Ok(output)
}
pub fn write_pcm16_wav(audio: &WaveAudio) -> Result<Vec<u8>, ServiceError> {
    let mut output = wav_header(audio, 1, 16)?;
    for sample in &audio.samples {
        let quantized = (sample.clamp(-1.0, 1.0) * 32768.0)
            .round()
            .clamp(-32768.0, 32767.0) as i16;
        output.extend_from_slice(&quantized.to_le_bytes());
    }
    Ok(output)
}
