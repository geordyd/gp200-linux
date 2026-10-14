//! UI workflow state; no file I/O, MIDI access, or unbounded device command batches.
use gp200_domain::*;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[derive(Debug, Default)]
pub(crate) struct BankAcquisition {
    pub epoch: Option<SessionEpoch>,
    pub completed: BTreeSet<SlotId>,
    pub in_flight: Option<(RequestStamp, SlotId)>,
    pub running: bool,
    pub message: String,
}
impl BankAcquisition {
    pub fn start(&mut self, epoch: SessionEpoch, resume: bool) {
        if !resume || self.epoch != Some(epoch) {
            self.completed.clear();
        }
        self.epoch = Some(epoch);
        self.running = true;
        self.message = "Reading stored presets. Your edit buffer stays open.".into();
    }
    pub fn next_slot(&self, epoch: SessionEpoch) -> Option<SlotId> {
        if !self.running || self.epoch != Some(epoch) || self.in_flight.is_some() {
            return None;
        }
        (0..256)
            .filter_map(|n| SlotId::new(n).ok())
            .find(|slot| !self.completed.contains(slot))
    }
    pub fn submitted(&mut self, stamp: RequestStamp, slot: SlotId) {
        self.in_flight = Some((stamp, slot));
    }
    pub fn cancel(&mut self) {
        self.running = false;
        self.message =
            "Scan paused. Completed presets are retained; the current read may finish.".into();
    }
    pub fn observe(&mut self, event: &DeviceEvent) {
        match event {
            DeviceEvent::ConnectionChanged { epoch, state }
                if self.epoch.is_some()
                    && (self.epoch != Some(*epoch) || *state != ConnectionState::Ready) =>
            {
                self.running = false;
                self.in_flight = None;
                self.message = "Connection changed. Partial scan retained; start a fresh scan after reconnecting.".into();
            }
            DeviceEvent::SnapshotReceived {
                stamp,
                target: PresetTarget::Stored(slot),
                snapshot,
            } if self.in_flight == Some((*stamp, *slot))
                && snapshot.target == Some(PresetTarget::Stored(*slot))
                && snapshot.context
                    == Some(WireContext {
                        group: 1,
                        selector: slot.get(),
                    })
                && !matches!(snapshot.source, SnapshotSource::Hardware { epoch } if epoch != stamp.epoch) =>
            {
                self.completed.insert(*slot);
                self.in_flight = None;
                if self.completed.len() == 256 {
                    self.running = false;
                    self.message =
                        "All 256 stored presets read. Assets and global settings are separate."
                            .into();
                }
            }
            DeviceEvent::OperationFailed { stamp, error }
                if self
                    .in_flight
                    .is_some_and(|(expected, _)| expected == *stamp) =>
            {
                self.in_flight = None;
                self.running = false;
                self.message = format!(
                    "Scan paused: {}. Completed presets are retained.",
                    error.message
                );
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AssetOptions {
    pub change_rate: bool,
    pub sample_rate: u32,
    pub channel_mode: u8,
    pub channel_index: u16,
    pub normalize: bool,
    pub normalize_peak: f32,
    pub truncate: bool,
    pub truncate_frames: u64,
    pub input_wav: String,
    pub resample_input: bool,
    pub fast_activation: bool,
    pub nam_output: NamOutput,
    pub nam_size: f64,
    pub larger_budget: bool,
    pub max_operations: u64,
    pub calibration_channel: u8,
    pub calibration_fast_activation: bool,
    pub calibration_timeout_seconds: u64,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NamOutput {
    #[default]
    Wav,
    ExperimentalClo,
}
impl Default for AssetOptions {
    fn default() -> Self {
        Self {
            change_rate: false,
            sample_rate: 48_000,
            channel_mode: 0,
            channel_index: 0,
            normalize: false,
            normalize_peak: 0.9,
            truncate: false,
            truncate_frames: 2048,
            input_wav: String::new(),
            resample_input: false,
            fast_activation: false,
            nam_output: NamOutput::Wav,
            nam_size: 1.0,
            larger_budget: false,
            max_operations: gp200_assets::DEFAULT_NAM_RENDER_OPERATIONS,
            calibration_channel: 0,
            calibration_fast_activation: true,
            calibration_timeout_seconds: 900,
        }
    }
}
impl AssetOptions {
    pub fn operation_limit(&self) -> u64 {
        if self.larger_budget {
            self.max_operations
        } else {
            gp200_assets::DEFAULT_NAM_RENDER_OPERATIONS
        }
    }
    pub fn specification(
        &self,
        kind: AssetKind,
        source: PathBuf,
        id: JobId,
    ) -> Result<AssetJobSpec, String> {
        let mut options = BTreeMap::new();
        if kind == AssetKind::Nam {
            if self.input_wav.trim().is_empty() {
                return Err("Select an input WAV before rendering a NAM model.".into());
            }
            options.insert("input_wav".into(), self.input_wav.clone());
            let limit = self.operation_limit();
            if limit == 0 || limit > gp200_assets::MAX_NAM_RENDER_OPERATIONS {
                return Err(
                    "Rendering budget must be between 1 and 1,000,000,000,000 operations.".into(),
                );
            }
            options.insert("max_operations".into(), limit.to_string());
            if self.nam_output == NamOutput::ExperimentalClo {
                if self.calibration_channel > 1 {
                    return Err("Select calibration channel 0 or 1.".into());
                }
                if !(1..=900).contains(&self.calibration_timeout_seconds) {
                    return Err("Calibration timeout must be between 1 and 900 seconds.".into());
                }
                options.insert("output_format".into(), "clo".into());
                options.insert("input_channel".into(), self.calibration_channel.to_string());
                options.insert(
                    "calibration_timeout_seconds".into(),
                    self.calibration_timeout_seconds.to_string(),
                );
            } else if self.channel_mode == 1 {
                options.insert("input_channel".into(), self.channel_index.to_string());
            }
            if self.nam_output == NamOutput::Wav && self.channel_mode == 2 {
                options.insert("input_channel".into(), "average".into());
            }
            if self.nam_output == NamOutput::Wav {
                if !self.nam_size.is_finite() || !(0.0..=1.0).contains(&self.nam_size) {
                    return Err("NAM size must be finite in 0–1.".into());
                }
                if self.nam_size != 1.0 {
                    options.insert("nam_size".into(), self.nam_size.to_string());
                }
                options.insert("resample_input".into(), self.resample_input.to_string());
            }
            options.insert(
                "activation".into(),
                if match self.nam_output {
                    NamOutput::Wav => self.fast_activation,
                    NamOutput::ExperimentalClo => self.calibration_fast_activation,
                } {
                    "fast"
                } else {
                    "standard"
                }
                .into(),
            );
        } else if kind == AssetKind::ImpulseResponse {
            if self.change_rate {
                options.insert("sample_rate".into(), self.sample_rate.to_string());
            }
            options.insert(
                "channel".into(),
                match self.channel_mode {
                    1 => self.channel_index.to_string(),
                    2 => "average".into(),
                    _ => "preserve".into(),
                },
            );
            if self.normalize {
                options.insert("normalize_peak".into(), self.normalize_peak.to_string());
            }
            if self.truncate {
                options.insert("truncate_frames".into(), self.truncate_frames.to_string());
            }
        } else {
            return Err("Local preparation supports WAV impulse responses and the supported NAM subset. Inspect other files first.".into());
        }
        Ok(AssetJobSpec {
            id,
            kind,
            source,
            output: None,
            options,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_size_option_is_wav_only_and_full_default_keeps_legacy_recipe() {
        let mut options = AssetOptions {
            input_wav: "input.wav".into(),
            ..Default::default()
        };
        assert!(
            !options
                .specification(AssetKind::Nam, "model.nam".into(), JobId(1))
                .unwrap()
                .options
                .contains_key("nam_size")
        );
        options.nam_size = 0.125;
        assert_eq!(
            options
                .specification(AssetKind::Nam, "model.nam".into(), JobId(1))
                .unwrap()
                .options["nam_size"],
            "0.125"
        );
        options.nam_output = NamOutput::ExperimentalClo;
        assert!(
            !options
                .specification(AssetKind::Nam, "model.nam".into(), JobId(1))
                .unwrap()
                .options
                .contains_key("nam_size")
        );
        options.nam_output = NamOutput::Wav;
        for size in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
            options.nam_size = size;
            assert!(
                options
                    .specification(AssetKind::Nam, "model.nam".into(), JobId(1))
                    .is_err()
            );
        }
    }
    #[test]
    fn bank_scan_requires_exact_slot_intent_epoch_and_stops_between_reads() {
        let mut scan = BankAcquisition::default();
        scan.start(SessionEpoch(3), false);
        let slot = scan.next_slot(SessionEpoch(3)).unwrap();
        let stamp = RequestStamp {
            epoch: SessionEpoch(3),
            intent: IntentId(91),
        };
        scan.submitted(stamp, slot);
        assert!(scan.next_slot(SessionEpoch(3)).is_none());
        let mut snap = (*gp200_sim::captured_demo_snapshot()).clone();
        snap.target = Some(PresetTarget::Stored(slot));
        snap.context = Some(WireContext {
            group: 1,
            selector: slot.get(),
        });
        let snap = std::sync::Arc::new(snap);
        scan.observe(&DeviceEvent::SnapshotReceived {
            stamp: RequestStamp {
                intent: IntentId(90),
                ..stamp
            },
            target: PresetTarget::Stored(slot),
            snapshot: snap.clone(),
        });
        assert!(scan.completed.is_empty());
        scan.cancel();
        scan.observe(&DeviceEvent::SnapshotReceived {
            stamp,
            target: PresetTarget::Stored(slot),
            snapshot: snap,
        });
        assert_eq!(scan.completed.len(), 1);
        assert!(scan.next_slot(SessionEpoch(3)).is_none());
        scan.start(SessionEpoch(3), true);
        assert_eq!(scan.next_slot(SessionEpoch(3)).unwrap().get(), 1);
        scan.observe(&DeviceEvent::ConnectionChanged {
            epoch: SessionEpoch(4),
            state: ConnectionState::Ready,
        });
        assert!(!scan.running);
        scan.start(SessionEpoch(4), true);
        assert!(scan.completed.is_empty());
    }
    #[test]
    fn local_asset_options_are_explicit_and_nam_requires_a_stimulus() {
        let mut options = AssetOptions::default();
        let wav = options
            .specification(AssetKind::ImpulseResponse, "a.wav".into(), JobId(2))
            .unwrap();
        assert_eq!(
            wav.options,
            BTreeMap::from([("channel".into(), "preserve".into())])
        );
        assert!(
            options
                .specification(AssetKind::Nam, "a.nam".into(), JobId(2))
                .is_err()
        );
        options.input_wav = "stimulus.wav".into();
        options.channel_mode = 1;
        options.channel_index = 2;
        let nam = options
            .specification(AssetKind::Nam, "a.nam".into(), JobId(3))
            .unwrap();
        assert_eq!(nam.options["input_channel"], "2");
        assert_eq!(nam.options["resample_input"], "false");
        assert!(!nam.options.contains_key("normalize_peak"));
    }
    #[test]
    fn clo_options_keep_channel_activation_budget_and_timeout_explicit() {
        let mut options = AssetOptions {
            input_wav: "selected-original.wav".into(),
            nam_output: NamOutput::ExperimentalClo,
            channel_mode: 2,
            resample_input: true,
            calibration_channel: 1,
            calibration_timeout_seconds: 120,
            larger_budget: true,
            max_operations: 100_000_000_000,
            ..Default::default()
        };
        let spec = options
            .specification(AssetKind::Nam, "model.nam".into(), JobId(7))
            .unwrap();
        assert_eq!(
            spec.options,
            BTreeMap::from([
                ("input_wav".into(), "selected-original.wav".into()),
                ("input_channel".into(), "1".into()),
                ("output_format".into(), "clo".into()),
                ("activation".into(), "fast".into()),
                ("max_operations".into(), "100000000000".into()),
                ("calibration_timeout_seconds".into(), "120".into()),
            ])
        );
        options.larger_budget = false;
        assert_eq!(
            options.operation_limit(),
            gp200_assets::DEFAULT_NAM_RENDER_OPERATIONS
        );
        options.calibration_timeout_seconds = 901;
        assert!(
            options
                .specification(AssetKind::Nam, "model.nam".into(), JobId(8))
                .is_err()
        );
        options.calibration_timeout_seconds = 1;
        options.calibration_channel = 2;
        assert!(
            options
                .specification(AssetKind::Nam, "model.nam".into(), JobId(8))
                .is_err()
        );
        options.calibration_channel = 0;
        options.larger_budget = true;
        options.max_operations = gp200_assets::MAX_NAM_RENDER_OPERATIONS + 1;
        assert!(
            options
                .specification(AssetKind::Nam, "model.nam".into(), JobId(8))
                .is_err()
        );
    }
}
