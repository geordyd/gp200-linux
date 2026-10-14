//! NAM preflight and conversion status. Inspection and inference run on the asset worker.
use crate::{StudioApp, runtime::FilePurpose, theme::Palette, workflow::AssetOptions};
use eframe::egui::{self, RichText};
use gp200_domain::*;
use std::{collections::BTreeMap, path::PathBuf};

pub use crate::workflow::NamOutput;

/// Opens real local files for the offline native preview runner and future file associations.
/// Opening this workspace only inspects files; conversion still needs its review action.
#[derive(Debug, Clone)]
pub struct NamWorkspaceLaunch {
    pub model: PathBuf,
    pub input: PathBuf,
    pub output: NamOutput,
    pub operation_limit: Option<u64>,
    pub review_when_ready: bool,
    pub input_channel: Option<u8>,
    pub resample_input: bool,
    pub nam_size: Option<f64>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InspectionPurpose {
    Model,
    Input,
}
#[derive(Debug, Clone)]
struct Inspection {
    path: PathBuf,
    info: AssetInfo,
}
#[derive(Debug, Clone)]
struct Conversion {
    id: JobId,
    output: NamOutput,
    progress: Option<JobProgress>,
    terminal: Option<Result<AssetInfo, String>>,
}
#[derive(Debug, Default)]
pub(crate) struct NamUiState {
    pending: BTreeMap<JobId, (PathBuf, InspectionPurpose)>,
    latest_model: Option<JobId>,
    latest_input: Option<JobId>,
    model: Option<Inspection>,
    input: Option<Inspection>,
    conversion: Option<Conversion>,
    pub review_when_ready: bool,
}
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct NamPreflight {
    pub estimate: Option<u64>,
    pub frames: Option<u64>,
    pub issues: Vec<String>,
}
impl NamUiState {
    pub fn inspect(&mut self, job: JobId, path: PathBuf, purpose: InspectionPurpose) -> bool {
        if self.pending.len() >= 8 {
            return false;
        }
        match purpose {
            InspectionPurpose::Model => self.latest_model = Some(job),
            InspectionPurpose::Input => self.latest_input = Some(job),
        }
        self.pending.insert(job, (path, purpose));
        true
    }
    pub fn inspecting(&self) -> bool {
        !self.pending.is_empty()
    }
    pub fn observe(&mut self, event: &AssetEvent) {
        if let AssetEvent::Inspected { job, info } = event {
            if let Some((path, purpose)) = self.pending.remove(job) {
                let cache = Inspection {
                    path,
                    info: info.clone(),
                };
                match purpose {
                    InspectionPurpose::Model if self.latest_model == Some(*job) => {
                        self.model = Some(cache)
                    }
                    InspectionPurpose::Input if self.latest_input == Some(*job) => {
                        self.input = Some(cache)
                    }
                    _ => {}
                }
            }
        } else if let AssetEvent::Failed { job, .. } = event {
            self.pending.remove(job);
        }
        let Some(conversion) = &mut self.conversion else {
            return;
        };
        match event {
            AssetEvent::JobProgress { job_id, progress } if *job_id == conversion.id => {
                conversion.progress = Some(progress.clone())
            }
            AssetEvent::Prepared { job, info } if *job == conversion.id => {
                conversion.terminal = Some(Ok(info.clone()))
            }
            AssetEvent::Failed { job, error } if *job == conversion.id => {
                conversion.terminal = Some(Err(error.message.clone()))
            }
            _ => {}
        }
    }
    pub fn start_conversion(&mut self, spec: &AssetJobSpec) {
        self.conversion = Some(Conversion {
            id: spec.id,
            output: if spec
                .options
                .get("output_format")
                .is_some_and(|format| format == "clo")
            {
                NamOutput::ExperimentalClo
            } else {
                NamOutput::Wav
            },
            progress: None,
            terminal: None,
        });
    }
    fn current_model(&self, path: &str) -> Option<&AssetInfo> {
        self.model
            .as_ref()
            .filter(|inspection| inspection.path == std::path::Path::new(path))
            .map(|inspection| &inspection.info)
    }
    fn current_input(&self, path: &str) -> Option<&AssetInfo> {
        self.input
            .as_ref()
            .filter(|inspection| inspection.path == std::path::Path::new(path))
            .map(|inspection| &inspection.info)
    }
    pub fn busy(&self) -> bool {
        self.conversion
            .as_ref()
            .is_some_and(|conversion| conversion.terminal.is_none())
    }
    pub fn preflight(&self, path: &str, options: &AssetOptions) -> NamPreflight {
        let mut result = NamPreflight::default();
        let Some(model) = self.current_model(path) else {
            result.issues.push(
                crate::localization::text(
                    "Inspect the selected NAM source to check its architecture and rendering cost.",
                )
                .into(),
            );
            return result;
        };
        if model
            .properties
            .get("inference_available")
            .map(String::as_str)
            != Some("true")
        {
            result
                .issues
                .push(model.warnings.first().cloned().unwrap_or_else(|| {
                    crate::localization::text(
                        "This model has no supported local inference profile.",
                    )
                    .into()
                }));
            return result;
        }
        let size = if options.nam_output == NamOutput::ExperimentalClo {
            1.0
        } else {
            options.nam_size
        };
        let inspected_size = model
            .properties
            .get("nam_size")
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(1.0);
        if !size.is_finite() || !(0.0..=1.0).contains(&size) || size != inspected_size {
            result.issues.push(
                crate::localization::text(
                    "Inspect the selected size to verify its native bin and actual rendering cost.",
                )
                .into(),
            );
            return result;
        }
        let rate = model
            .properties
            .get("sample_rate")
            .and_then(|rate| rate.parse::<u64>().ok())
            .unwrap_or(48_000);
        let operations = model
            .properties
            .get("operations_per_sample")
            .and_then(|value| value.parse::<u64>().ok());
        let input = self.current_input(&options.input_wav);
        let frames = if options.nam_output == NamOutput::ExperimentalClo {
            if ![44_100, 48_000, 96_000].contains(&rate) {
                result.issues.push(
                    crate::localization::text("Experimental calibration supports model rates of 44,100, 48,000 or 96,000 Hz.")
                        .into(),
                );
            }
            rate.checked_mul(70)
        } else {
            input.and_then(|input| {
                let frames = input.properties.get("frames")?.parse::<u64>().ok()?;
                if options.resample_input {
                    let input_rate = input.properties.get("sample_rate")?.parse::<u64>().ok()?;
                    if input_rate == 0 {
                        return None;
                    }
                    frames
                        .checked_mul(rate)
                        .map(|frames| frames.div_ceil(input_rate))
                } else {
                    Some(frames)
                }
            })
        };
        result.frames = frames;
        result.estimate = frames
            .zip(operations)
            .and_then(|(frames, operations)| frames.checked_mul(operations));
        if options.operation_limit() == 0
            || options.operation_limit() > gp200_assets::MAX_NAM_RENDER_OPERATIONS
        {
            result.issues.push(
                crate::localization::text(
                    "Select a rendering budget between 1 and 1,000,000,000,000 operations.",
                )
                .into(),
            );
        }
        if let Some(estimate) = result.estimate {
            if estimate > options.operation_limit() {
                result.issues.push(std::format!("This render needs about {} operations. Shorten the WAV or explicitly allow a larger budget.", grouped(estimate)));
            }
        } else {
            result.issues.push(
                crate::localization::text(
                    "Inspect the selected input WAV to calculate the rendering cost.",
                )
                .into(),
            );
        }
        let Some(input) = input else {
            result.issues.push(
                crate::localization::text(
                    "Select and inspect an input WAV. No stimulus is automatically substituted.",
                )
                .into(),
            );
            return result;
        };
        if input.properties.get("validation").map(String::as_str) != Some("inspected") {
            result.issues.push(
                crate::localization::text("The selected input WAV did not pass inspection.").into(),
            );
            return result;
        }
        let channels = input
            .properties
            .get("channels")
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(0);
        if options.nam_output == NamOutput::ExperimentalClo {
            if input.properties.get("sha256").map(String::as_str)
                != Some(gp200_assets::GP200_STIMULUS_WAV_SHA256)
            {
                result.issues.push(crate::localization::text("This WAV is not the verified GP-200 V1.8.1 calibration stimulus. Select the retained original WAV.").into());
            }
            if options.calibration_channel > 1 || u16::from(options.calibration_channel) >= channels
            {
                result.issues.push(
                    crate::localization::text("Select available calibration channel 0 or 1.")
                        .into(),
                );
            }
            if !(1..=900).contains(&options.calibration_timeout_seconds) {
                result.issues.push(
                    crate::localization::text(
                        "Calibration timeout must be between 1 and 900 seconds.",
                    )
                    .into(),
                );
            }
            if !gp200_assets::calibration_helper_available() {
                result.issues.push(
                    crate::localization::text(
                        "The offline calibration helper is unavailable in this build.",
                    )
                    .into(),
                );
            }
        } else {
            if (options.channel_mode == 0 && channels != 1)
                || (options.channel_mode == 1 && options.channel_index >= channels)
                || options.channel_mode > 2
            {
                result.issues.push(
                    crate::localization::text(
                        "Select an available input channel or average the channels to mono.",
                    )
                    .into(),
                );
            }
            if !options.resample_input
                && input
                    .properties
                    .get("sample_rate")
                    .and_then(|value| value.parse::<u64>().ok())
                    != Some(rate)
            {
                result.issues.push(std::format!("Input rate differs from the model's {} Hz. Enable input resampling or choose matching audio.", grouped(rate)));
            }
        }
        result
    }
}
impl StudioApp {
    pub fn open_nam_workspace(&mut self, launch: NamWorkspaceLaunch) {
        self.selected_asset_kind = AssetKind::Nam;
        self.asset_options.nam_output = launch.output;
        if let Some(size) = launch.nam_size {
            if !size.is_finite() || !(0.0..=1.0).contains(&size) {
                self.services
                    .error(crate::localization::text("NAM size must be finite in 0–1."));
                return;
            }
            self.asset_options.nam_size = size;
        }
        if let Some(limit) = launch.operation_limit {
            if !(gp200_assets::DEFAULT_NAM_RENDER_OPERATIONS
                ..=gp200_assets::MAX_NAM_RENDER_OPERATIONS)
                .contains(&limit)
            {
                self.services.error(crate::localization::text("The workspace budget must be between the default 2 billion and maximum 1 trillion operations."));
                return;
            }
            self.asset_options.larger_budget = limit > gp200_assets::DEFAULT_NAM_RENDER_OPERATIONS;
            self.asset_options.max_operations = limit;
        }
        self.nam_ui.review_when_ready = launch.review_when_ready;
        self.asset_options.resample_input = launch.resample_input;
        if let Some(channel) = launch.input_channel {
            self.asset_options.channel_mode = 1;
            self.asset_options.channel_index = u16::from(channel);
            self.asset_options.calibration_channel = channel;
        }
        self.handle_file(FilePurpose::ImportNam, launch.model);
        self.handle_file(FilePurpose::NamInput, launch.input);
        self.send(UiIntent::Navigate(ViewId::Assets));
    }
    pub(crate) fn inspect_nam_file(&mut self, path: PathBuf, purpose: InspectionPurpose) {
        if self.services.assets.is_none() {
            self.services.error(crate::localization::text("The local asset worker is unavailable. Reopen the app after checking its storage path."));
            return;
        }
        self.next_job += 1;
        let id = JobId(self.next_job);
        if !self.nam_ui.inspect(id, path.clone(), purpose) {
            self.services.error(crate::localization::text(
                "Finish the pending NAM inspections before adding more files.",
            ));
            return;
        }
        self.local_asset_jobs.insert(id);
        self.services
            .enqueue(vec![Effect::Assets(AssetCommand::Inspect(AssetJobSpec {
                id,
                kind: if purpose == InspectionPurpose::Model {
                    AssetKind::Nam
                } else {
                    AssetKind::ImpulseResponse
                },
                source: path,
                output: None,
                options: if purpose == InspectionPurpose::Model {
                    BTreeMap::from([(
                        "nam_size".into(),
                        if self.asset_options.nam_output == NamOutput::ExperimentalClo {
                            1.0
                        } else {
                            self.asset_options.nam_size
                        }
                        .to_string(),
                    )])
                } else {
                    BTreeMap::new()
                },
            }))]);
    }
    pub(crate) fn nam_processing_options(&mut self, ui: &mut egui::Ui) {
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            self.nam_processing_fields(ui);
        });
    }
    fn nam_processing_fields(&mut self, ui: &mut egui::Ui) {
        let palette = Palette::for_context(ui.ctx());
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(crate::localization::text("Output"));
            ui.selectable_value(
                &mut self.asset_options.nam_output,
                NamOutput::Wav,
                crate::localization::text("Render WAV"),
            );
            ui.selectable_value(
                &mut self.asset_options.nam_output,
                NamOutput::ExperimentalClo,
                crate::localization::text("Experimental CLO"),
            );
        });
        if let Some(model) = self.nam_ui.current_model(&self.asset_path) {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&model.name).strong());
                ui.label(
                    RichText::new(
                        model
                            .properties
                            .get("network_profile")
                            .or_else(|| model.properties.get("architecture"))
                            .map(String::as_str)
                            .unwrap_or(crate::localization::text("Unrecognized model")),
                    )
                    .strong(),
                );
                ui.label(std::format!(
                    "NAM {}",
                    model
                        .properties
                        .get("nam_version")
                        .map(String::as_str)
                        .unwrap_or("unknown")
                ));
                ui.label(std::format!(
                    "{} Hz",
                    model
                        .properties
                        .get("sample_rate")
                        .map(String::as_str)
                        .filter(|rate| *rate != "unspecified")
                        .unwrap_or(crate::localization::text("48000 (default)"))
                ));
                ui.label(
                    if model
                        .properties
                        .get("inference_available")
                        .map(String::as_str)
                        == Some("true")
                    {
                        crate::localization::text("Local inference available")
                    } else {
                        crate::localization::text("Inspection only")
                    },
                );
            });
        } else {
            ui.small(crate::localization::text(
                "Inspect a NAM model to see its supported architecture, sample rate and cost.",
            ));
        }
        let clo = self.asset_options.nam_output == NamOutput::ExperimentalClo;
        if !clo {
            let plan = self
                .nam_ui
                .current_model(&self.asset_path)
                .and_then(|model| model.properties.get("size_plan_json"))
                .filter(|json| json.len() <= 32768)
                .and_then(|json| serde_json::from_str::<gp200_assets::NamSizePlan>(json).ok());
            ui.horizontal_wrapped(|ui| {
                ui.label(crate::localization::text("Model size"));
                if let Some(plan) = plan {
                    egui::ComboBox::from_id_salt("nam-size-bin")
                        .selected_text(std::format!(
                            "{:.4} · selected fraction",
                            self.asset_options.nam_size
                        ))
                        .height(240.0)
                        .show_ui(ui, |ui| {
                            for (size, selection) in plan.choices() {
                                ui.selectable_value(
                                    &mut self.asset_options.nam_size,
                                    size,
                                    if size == 1.0 {
                                        std::format!("{selection} · full default (1.0)")
                                    } else {
                                        std::format!("{selection} · {size:.4}")
                                    },
                                );
                            }
                        });
                    ui.add(
                        egui::DragValue::new(&mut self.asset_options.nam_size)
                            .range(0.0..=1.0)
                            .speed(0.01)
                            .max_decimals(6),
                    );
                } else {
                    ui.small(crate::localization::text(
                        "Full default · this model has no inspected size-selection profile",
                    ));
                    if self.asset_options.nam_size != 1.0
                        && ui
                            .button(crate::localization::text("Use full size"))
                            .clicked()
                    {
                        self.asset_options.nam_size = 1.0;
                    }
                }
                if ui
                    .add_enabled(
                        !self.asset_path.is_empty() && !self.nam_ui.inspecting(),
                        egui::Button::new(crate::localization::text("Inspect size")),
                    )
                    .clicked()
                {
                    self.inspect_nam_file(
                        PathBuf::from(&self.asset_path),
                        InspectionPurpose::Model,
                    );
                }
            });
            if let Some(model) = self.nam_ui.current_model(&self.asset_path) {
                ui.small(std::format!(
                    "Inspected bin: {} · {} operations per sample",
                    model
                        .properties
                        .get("size_selection")
                        .map(String::as_str)
                        .unwrap_or(crate::localization::text("Full model")),
                    model
                        .properties
                        .get("operations_per_sample")
                        .map(String::as_str)
                        .unwrap_or("unavailable")
                ));
            }
        } else {
            ui.small(crate::localization::text("Calibration keeps its independently checked full-size profile. Size selection applies to WAV rendering."));
            if self
                .nam_ui
                .current_model(&self.asset_path)
                .is_some_and(|model| {
                    model
                        .properties
                        .get("nam_size")
                        .is_some_and(|value| value != "1")
                })
                && ui
                    .button(crate::localization::text("Inspect full model for CLO"))
                    .clicked()
            {
                self.inspect_nam_file(PathBuf::from(&self.asset_path), InspectionPurpose::Model);
            }
        }
        if clo {
            ui.label(crate::localization::text("Independent NAM-to-CLO calibration. Keep the model, stimulus and both coefficient stages locally."));
            ui.colored_label(palette.warning, crate::localization::text("Pedal acceptance and original converter equivalence are unverified. Upload stays unavailable."));
        } else {
            ui.small(crate::localization::text("Render the inspected model into a stereo PCM16 WAV. This audio is not a SnapTone upload file."));
        }
        self.nam_conversion_status(ui);
        ui.horizontal_wrapped(|ui| {
            if ui
                .button(if clo {
                    crate::localization::text("Select calibration WAV…")
                } else {
                    crate::localization::text("Select input WAV…")
                })
                .clicked()
            {
                self.services.dialog(if clo {
                    FilePurpose::NamCalibrationInput
                } else {
                    FilePurpose::NamInput
                });
            }
            ui.add(
                egui::TextEdit::singleline(&mut self.asset_options.input_wav)
                    .hint_text(if clo {
                        crate::localization::text("Retained GP-200 V1.8.1 stimulus WAV")
                    } else {
                        crate::localization::text("Explicit input WAV")
                    })
                    .desired_width((ui.available_width() - 115.0).clamp(180.0, 540.0)),
            );
            if ui
                .add_enabled(
                    !self.asset_options.input_wav.trim().is_empty() && !self.nam_ui.inspecting(),
                    egui::Button::new(crate::localization::text("Inspect WAV")),
                )
                .clicked()
            {
                self.inspect_nam_file(
                    PathBuf::from(&self.asset_options.input_wav),
                    InspectionPurpose::Input,
                );
            }
        });
        if let Some(input) = self.nam_ui.current_input(&self.asset_options.input_wav) {
            ui.horizontal_wrapped(|ui| {
                ui.small(std::format!(
                    "{} channels, {} Hz, {} seconds",
                    input
                        .properties
                        .get("channels")
                        .map(String::as_str)
                        .unwrap_or("?"),
                    input
                        .properties
                        .get("sample_rate")
                        .map(String::as_str)
                        .unwrap_or("?"),
                    input
                        .properties
                        .get("seconds")
                        .map(String::as_str)
                        .unwrap_or("?")
                ));
                if clo {
                    let verified = input.properties.get("sha256").map(String::as_str)
                        == Some(gp200_assets::GP200_STIMULUS_WAV_SHA256);
                    ui.colored_label(
                        if verified {
                            palette.accent
                        } else {
                            palette.warning
                        },
                        if verified {
                            crate::localization::text("Stimulus hash matches")
                        } else {
                            crate::localization::text("Stimulus hash does not match")
                        },
                    )
                    .on_hover_text(gp200_assets::GP200_STIMULUS_WAV_SHA256);
                }
            });
        }
        if clo {
            ui.horizontal_wrapped(|ui| {
                ui.label(crate::localization::text("Stimulus channel"));
                ui.selectable_value(
                    &mut self.asset_options.calibration_channel,
                    0,
                    crate::localization::text("0 · left"),
                );
                ui.selectable_value(
                    &mut self.asset_options.calibration_channel,
                    1,
                    crate::localization::text("1 · right"),
                );
                ui.checkbox(
                    &mut self.asset_options.calibration_fast_activation,
                    crate::localization::text("Fast activation"),
                );
                ui.label(crate::localization::text("Timeout"));
                ui.add(
                    egui::DragValue::new(&mut self.asset_options.calibration_timeout_seconds)
                        .range(1..=900)
                        .suffix(crate::localization::text(" seconds")),
                );
            });
            ui.small(crate::localization::text("The selected original must match the shipped 70-second PCM16 stimulus. No download or replacement occurs here."));
        } else {
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(
                    &mut self.asset_options.resample_input,
                    crate::localization::text("Resample input to model rate"),
                );
                ui.checkbox(
                    &mut self.asset_options.fast_activation,
                    crate::localization::text("Fast activation approximation"),
                );
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(crate::localization::text("Input channels"));
                ui.selectable_value(
                    &mut self.asset_options.channel_mode,
                    0,
                    crate::localization::text("Require mono"),
                );
                ui.selectable_value(
                    &mut self.asset_options.channel_mode,
                    1,
                    crate::localization::text("Select channel"),
                );
                ui.selectable_value(
                    &mut self.asset_options.channel_mode,
                    2,
                    crate::localization::text("Average to mono"),
                );
                if self.asset_options.channel_mode == 1 {
                    ui.add(
                        egui::DragValue::new(&mut self.asset_options.channel_index)
                            .range(0..=255)
                            .prefix(crate::localization::text("Index ")),
                    );
                }
            });
        }
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(
                &mut self.asset_options.larger_budget,
                crate::localization::text("Allow a larger rendering budget"),
            );
            if self.asset_options.larger_budget {
                ui.add(
                    egui::DragValue::new(&mut self.asset_options.max_operations)
                        .range(
                            gp200_assets::DEFAULT_NAM_RENDER_OPERATIONS
                                ..=gp200_assets::MAX_NAM_RENDER_OPERATIONS,
                        )
                        .speed(1_000_000)
                        .custom_formatter(|value, _| grouped(value as u64))
                        .custom_parser(parse_budget),
                );
                ui.label(crate::localization::text("operations"));
            } else {
                ui.small(crate::localization::text("Default: 2 billion operations"));
            }
        });
        let preflight = self.nam_ui.preflight(&self.asset_path, &self.asset_options);
        if let Some(estimate) = preflight.estimate {
            ui.label(
                RichText::new(std::format!("Estimated work: {} operations   /   Limit: {}",
                    grouped(estimate),
                    grouped(self.asset_options.operation_limit())
                ))
                .strong(),
            ).on_hover_text(crate::localization::text("Cost estimates describe kernel work, not elapsed time. Cancel stays available during rendering and calibration."));
        }
        for issue in &preflight.issues {
            ui.colored_label(palette.warning, issue);
        }
        if self.nam_ui.inspecting() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(crate::localization::text(
                    "Inspecting selected local files…",
                ));
            });
        }
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    preflight.issues.is_empty() && !self.nam_ui.busy(),
                    egui::Button::new(if clo {
                        crate::localization::text("Review experimental CLO…")
                    } else {
                        crate::localization::text("Review WAV render…")
                    }),
                )
                .clicked()
            {
                self.submit_asset(true);
            }
        });
    }
    fn nam_conversion_status(&mut self, ui: &mut egui::Ui) {
        let Some(conversion) = self.nam_ui.conversion.clone() else {
            let model_hash = self
                .nam_ui
                .current_model(&self.asset_path)
                .and_then(|model| model.properties.get("sha256"));
            if let Some(asset) = model_hash
                .and_then(|hash| {
                    self.controller.state().assets.iter().find(|asset| {
                        asset.properties.get("original_sha256") == Some(hash)
                            && if self.asset_options.nam_output == NamOutput::Wav {
                                asset.kind == AssetKind::Nam
                                    && asset.properties.get("format").map(String::as_str)
                                        == Some(crate::localization::text("Stereo PCM16 WAV"))
                            } else {
                                asset.properties.get("declared_bytes").map(String::as_str)
                                    == Some("4744")
                                    && asset.properties.contains_key("calibration_report_json")
                            }
                    })
                })
                .cloned()
            {
                super::section(
                    ui,
                    if self.asset_options.nam_output == NamOutput::Wav {
                        crate::localization::text("Retained WAV for this model")
                    } else {
                        crate::localization::text("Retained CLO for this model")
                    },
                );
                ui.small(asset.path.display().to_string());
                self.nam_export_button(ui, &asset);
                conversion_history(ui, &asset);
                if self.asset_options.nam_output == NamOutput::ExperimentalClo {
                    calibration_summary(ui, &asset);
                }
            }
            return;
        };
        super::section(ui, crate::localization::text("Local conversion"));
        match &conversion.terminal {
            Some(Ok(info)) => {
                ui.label(if conversion.output == NamOutput::Wav {
                    crate::localization::text("WAV render complete. Original sources and render options retained.")
                } else {
                    crate::localization::text("CLO calibration complete. Intermediate and final coefficient files retained.")
                });
                ui.small(info.path.display().to_string());
                self.nam_export_button(ui, info);
                conversion_history(ui, info);
                if conversion.output == NamOutput::ExperimentalClo {
                    calibration_summary(ui, info);
                }
            }
            Some(Err(message)) => {
                ui.colored_label(Palette::for_context(ui.ctx()).warning, message);
            }
            None => {
                let progress = conversion.progress.as_ref();
                let fitting = conversion.output == NamOutput::ExperimentalClo
                    && progress.is_some_and(|progress| {
                        progress
                            .total
                            .is_some_and(|total| total > 0 && progress.completed >= total)
                    });
                ui.horizontal_wrapped(|ui| {
                    ui.spinner();
                    ui.label(if fitting {
                        crate::localization::text("Fitting coefficients and refining tone match…")
                    } else {
                        progress
                            .map(|progress| progress.message.as_str())
                            .unwrap_or(crate::localization::text(
                                "Waiting for the local asset worker…",
                            ))
                    });
                    if ui
                        .button(crate::localization::text("Cancel local conversion"))
                        .clicked()
                    {
                        self.cancel_scoped_job(conversion.id);
                    }
                });
                if !fitting
                    && let Some(progress) = progress
                    && let Some(total) = progress.total
                    && total > 0
                {
                    ui.add(
                        egui::ProgressBar::new(progress.completed as f32 / total as f32)
                            .desired_width(340.0)
                            .text(crate::localization::text("Model response")),
                    );
                }
                ui.small(crate::localization::text("Your pedal remains unchanged. Cancellation may take until the next worker checkpoint."));
            }
        }
    }
    pub(crate) fn advance_nam_workspace(&mut self) {
        if self.nam_ui.review_when_ready && !self.nam_ui.inspecting() {
            self.nam_ui.review_when_ready = false;
            self.submit_asset(true);
        }
    }
    pub(crate) fn nam_export_button(&mut self, ui: &mut egui::Ui, info: &AssetInfo) {
        let clo = info.kind == AssetKind::SnapTone
            && info.properties.contains_key("calibration_report_json")
            && info.properties.get("declared_bytes").map(String::as_str) == Some("4744");
        let wav = info.kind == AssetKind::Nam
            && info.properties.get("format").map(String::as_str)
                == Some(crate::localization::text("Stereo PCM16 WAV"));
        if !clo && !wav {
            return;
        }
        if ui
            .add_enabled(
                self.asset_export_pending.is_none(),
                egui::Button::new(if clo {
                    crate::localization::text("Export retained CLO…")
                } else {
                    crate::localization::text("Export retained WAV…")
                }),
            )
            .clicked()
        {
            self.begin_nam_export(info.clone(), clo);
        }
        if let Some((id, path)) = &self.services.last_asset_export
            && *id == info.id
        {
            ui.small(std::format!("Saved locally: {}", path.display()));
        }
    }
    pub(crate) fn begin_nam_export(&mut self, info: AssetInfo, clo: bool) {
        if self.asset_export_pending.is_some() {
            return;
        }
        if self.services.try_dialog(if clo {
            FilePurpose::ExportNamClo
        } else {
            FilePurpose::ExportNamWav
        }) {
            self.asset_export_pending = Some(info);
        }
    }
}
pub(super) fn conversion_history(ui: &mut egui::Ui, asset: &AssetInfo) {
    let Some(count) = asset.properties.get("conversion_recipe_count") else {
        return;
    };
    egui::CollapsingHeader::new(std::format!("Conversion history · {count} recipes")).id_salt(("recipes", &asset.id)).show(ui, |ui| {
        ui.small(crate::localization::text("Output bytes are immutable. Each distinct source or option set keeps its own durable recipe. Showing up to eight recent recipes."));
        let Some(history) = asset.properties.get("conversion_history_json").filter(|json| json.len() <= 32768).and_then(|json| serde_json::from_str::<Vec<serde_json::Value>>(json).ok()) else { return; };
        for recipe in history.into_iter().take(8) {
            ui.separator();
            ui.label(recipe["source_name"].as_str().unwrap_or(crate::localization::text("Retained source")));
            ui.small(std::format!("{} · size {} · {} · {} operations/sample", recipe["profile"].as_str().unwrap_or(crate::localization::text("Local preparation")), recipe["nam_size"].as_str().unwrap_or("full"), recipe["size_selection"].as_str().unwrap_or(crate::localization::text("Full model")), recipe["operations_per_sample"].as_str().unwrap_or("—")));
            if let Some(id) = recipe["id"].as_str() { ui.horizontal_wrapped(|ui| { ui.small(crate::localization::text("Recipe SHA-256")); ui.monospace(id); if ui.small_button(crate::localization::text("Copy")).clicked() { ui.ctx().copy_text(id.into()); } }); }
            egui::CollapsingHeader::new(crate::localization::text("Exact sources and options")).id_salt(recipe["id"].as_str().unwrap_or_default()).show(ui, |ui| { ui.monospace(serde_json::to_string_pretty(&recipe).unwrap_or_default()); });
        }
    });
}
pub(super) fn calibration_summary(ui: &mut egui::Ui, asset: &AssetInfo) {
    let palette = Palette::for_context(ui.ctx());
    ui.colored_label(
        palette.warning,
        crate::localization::text(
            "Hardware compatibility: unverified. No device transfer was prepared.",
        ),
    );
    let Some(report) = asset
        .properties
        .get("calibration_report_json")
        .filter(|report| report.len() <= 64 * 1024)
        .and_then(|report| serde_json::from_str::<serde_json::Value>(report).ok())
    else {
        return;
    };
    if let Some(score) = report
        .get("final_spectral_rmse_db")
        .and_then(serde_json::Value::as_f64)
        .filter(|score| score.is_finite())
    {
        ui.label(std::format!("Local spectral fit error: {score:.3} dB"));
        ui.small(crate::localization::text(
            "This score measures the local fit; it does not measure pedal sound.",
        ));
    }
    ui.horizontal_wrapped(|ui| {
        if let Some(latency) = report
            .get("latency_samples")
            .and_then(serde_json::Value::as_u64)
        {
            ui.label(std::format!("Alignment: {latency} samples"));
        }
        if let Some(windows) = report
            .get("analysis_windows")
            .and_then(serde_json::Value::as_u64)
        {
            ui.label(std::format!("Analysis windows: {windows}"));
        }
    });
    egui::CollapsingHeader::new(crate::localization::text("Conversion provenance"))
        .id_salt(&asset.id)
        .show(ui, |ui| {
            if let Some(profile) = report
                .get("conversion_profile")
                .and_then(serde_json::Value::as_str)
            {
                ui.label(profile);
            }
            for (key, label) in [
                ("model_sha256", crate::localization::text("NAM model")),
                (
                    "source_mono_sha256",
                    crate::localization::text("Selected stimulus channel"),
                ),
                (
                    "nam_response_sha256",
                    crate::localization::text("Model response"),
                ),
                (
                    "helper_sha256",
                    crate::localization::text("Calibration helper"),
                ),
                (
                    "intermediate_sha256",
                    crate::localization::text("Intermediate CLO"),
                ),
                ("final_sha256", crate::localization::text("Final CLO")),
            ] {
                if let Some(hash) = report.get(key).and_then(serde_json::Value::as_str) {
                    ui.label(std::format!("{label}: {hash}"));
                }
            }
        });
}
pub(crate) fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut result = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            result.push(',');
        }
        result.push(digit);
    }
    result
}
fn parse_budget(value: &str) -> Option<f64> {
    if value.len() > 64 {
        return None;
    }
    let digits: String = value
        .chars()
        .filter(|character| !matches!(character, ',' | '_' | ' '))
        .collect();
    let value = digits.parse::<u64>().ok()?;
    (gp200_assets::DEFAULT_NAM_RENDER_OPERATIONS..=gp200_assets::MAX_NAM_RENDER_OPERATIONS)
        .contains(&value)
        .then_some(value as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    fn model(name: &str) -> AssetInfo {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/assets")
            .join(name);
        let inspected = gp200_assets::inspect_nam(&std::fs::read(path).unwrap()).unwrap();
        AssetInfo {
            id: AssetId("test-model".into()),
            kind: AssetKind::Nam,
            name: name.into(),
            path: "retained-model".into(),
            bytes: 1,
            properties: BTreeMap::from([
                (
                    "inference_available".into(),
                    inspected.supported_inference.to_string(),
                ),
                ("architecture".into(), inspected.architecture),
                (
                    "network_profile".into(),
                    inspected.network_profile.unwrap_or_default(),
                ),
                ("nam_version".into(), inspected.version),
                (
                    "sample_rate".into(),
                    inspected
                        .sample_rate
                        .map(|rate| rate.to_string())
                        .unwrap_or_else(|| "unspecified".into()),
                ),
                (
                    "operations_per_sample".into(),
                    inspected
                        .operations_per_sample
                        .unwrap_or_default()
                        .to_string(),
                ),
            ]),
            warnings: vec![inspected.reason],
        }
    }
    fn input(frames: u64, rate: u32, channels: u16, verified: bool) -> AssetInfo {
        AssetInfo {
            id: AssetId("test-input".into()),
            kind: AssetKind::ImpulseResponse,
            name: "input.wav".into(),
            path: "retained-input".into(),
            bytes: 1,
            properties: BTreeMap::from([
                ("validation".into(), "inspected".into()),
                ("frames".into(), frames.to_string()),
                ("sample_rate".into(), rate.to_string()),
                ("channels".into(), channels.to_string()),
                (
                    "sha256".into(),
                    if verified {
                        gp200_assets::GP200_STIMULUS_WAV_SHA256
                    } else {
                        "different-hash"
                    }
                    .into(),
                ),
            ]),
            warnings: vec![],
        }
    }
    fn state(info: AssetInfo, wave: AssetInfo) -> NamUiState {
        let mut state = NamUiState::default();
        state.inspect(JobId(1), "model.nam".into(), InspectionPurpose::Model);
        state.inspect(JobId(2), "input.wav".into(), InspectionPurpose::Input);
        state.observe(&AssetEvent::Inspected {
            job: JobId(1),
            info,
        });
        state.observe(&AssetEvent::Inspected {
            job: JobId(2),
            info: wave,
        });
        state
    }
    fn selected_model(size: f64) -> AssetInfo {
        let mut info = model("upstream-slimmable_wavenet.nam");
        let bytes = std::fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/assets/upstream-slimmable_wavenet.nam"),
        )
        .unwrap();
        let selected = gp200_assets::inspect_nam_with_size(&bytes, size).unwrap();
        info.properties.insert("nam_size".into(), size.to_string());
        info.properties.insert(
            "operations_per_sample".into(),
            selected.operations_per_sample.unwrap().to_string(),
        );
        info.properties.insert(
            "size_selection".into(),
            selected.size_selection.unwrap().to_string(),
        );
        info
    }
    #[test]
    fn selected_size_preflight_requires_matching_worker_inspection_and_uses_its_cost() {
        let full = state(selected_model(1.0), input(2057, 48000, 1, false));
        let selected = state(selected_model(0.0), input(2057, 48000, 1, false));
        let mut options = AssetOptions {
            input_wav: "input.wav".into(),
            nam_size: 0.0,
            ..Default::default()
        };
        assert!(
            full.preflight("model.nam", &options)
                .issues
                .iter()
                .any(|issue| issue.contains("Inspect the selected size"))
        );
        let preflight = selected.preflight("model.nam", &options);
        assert!(preflight.issues.is_empty());
        let actual =
            selected.current_model("model.nam").unwrap().properties["operations_per_sample"]
                .parse::<u64>()
                .unwrap();
        assert_eq!(preflight.estimate, Some(2057 * actual));
        assert!(
            preflight.estimate.unwrap()
                < full
                    .preflight(
                        "model.nam",
                        &AssetOptions {
                            input_wav: "input.wav".into(),
                            ..Default::default()
                        }
                    )
                    .estimate
                    .unwrap()
        );
        options.nam_output = NamOutput::ExperimentalClo;
        assert!(
            selected
                .preflight("model.nam", &options)
                .issues
                .iter()
                .any(|issue| issue.contains("Inspect the selected size"))
        );
        let mut pending = NamUiState::default();
        pending.inspect(JobId(1), "model.nam".into(), InspectionPurpose::Model);
        pending.inspect(JobId(2), "model.nam".into(), InspectionPurpose::Model);
        pending.inspect(JobId(3), "input.wav".into(), InspectionPurpose::Input);
        pending.observe(&AssetEvent::Inspected {
            job: JobId(2),
            info: selected_model(0.0),
        });
        pending.observe(&AssetEvent::Inspected {
            job: JobId(1),
            info: selected_model(1.0),
        });
        pending.observe(&AssetEvent::Inspected {
            job: JobId(3),
            info: input(2057, 48000, 1, false),
        });
        options.nam_output = NamOutput::Wav;
        assert_eq!(
            pending.preflight("model.nam", &options).estimate,
            preflight.estimate
        );
    }
    #[test]
    fn actual_a2_cost_requires_explicit_opt_in_and_uses_selected_model_rate() {
        let state = state(
            model("upstream-A2.nam"),
            input(70 * 44_100, 44_100, 2, true),
        );
        let mut options = AssetOptions {
            input_wav: "input.wav".into(),
            nam_output: NamOutput::ExperimentalClo,
            ..Default::default()
        };
        let preflight = state.preflight("model.nam", &options);
        assert!(preflight.estimate.unwrap() > gp200_assets::DEFAULT_NAM_RENDER_OPERATIONS);
        assert!(
            preflight
                .issues
                .iter()
                .any(|issue| issue.contains("larger budget"))
        );
        options.larger_budget = true;
        options.max_operations = preflight.estimate.unwrap();
        assert!(state.preflight("model.nam", &options).issues.is_empty());
        options.larger_budget = false;
        assert!(!state.preflight("model.nam", &options).issues.is_empty());
    }
    #[test]
    fn stale_and_out_of_order_inspection_never_authorizes_the_new_path() {
        let mut state = NamUiState::default();
        state.inspect(JobId(1), "model.nam".into(), InspectionPurpose::Model);
        state.inspect(JobId(2), "another.nam".into(), InspectionPurpose::Model);
        state.observe(&AssetEvent::Inspected {
            job: JobId(2),
            info: model("upstream-wavenet.nam"),
        });
        state.observe(&AssetEvent::Inspected {
            job: JobId(1),
            info: model("upstream-lstm-0.5.4.nam"),
        });
        assert!(state.current_model("model.nam").is_none());
        assert_eq!(
            state.current_model("another.nam").unwrap().properties["architecture"],
            "WaveNet"
        );
        assert!(!state.inspecting());
        assert!(
            !state
                .preflight("edited-path.nam", &AssetOptions::default())
                .issues
                .is_empty()
        );
    }
    #[test]
    fn waveform_channel_and_resampling_choices_change_preflight_cost() {
        let state = state(model("upstream-wavenet.nam"), input(1001, 44_100, 2, false));
        let mut options = AssetOptions {
            input_wav: "input.wav".into(),
            ..Default::default()
        };
        let first = state.preflight("model.nam", &options);
        assert!(first.issues.iter().any(|issue| issue.contains("channel")));
        assert!(
            first
                .issues
                .iter()
                .any(|issue| issue.contains("resampling"))
        );
        options.resample_input = true;
        options.channel_mode = 2;
        let adapted = state.preflight("model.nam", &options);
        assert!(adapted.issues.is_empty(), "{:?}", adapted.issues);
        assert_eq!(adapted.frames, Some((1001_u64 * 48_000).div_ceil(44_100)));
        assert!(adapted.estimate.unwrap() > first.estimate.unwrap());
        options.input_wav = "new-path.wav".into();
        assert!(state.preflight("model.nam", &options).estimate.is_none());
    }
    #[test]
    fn arbitrary_audio_cannot_pass_calibration_and_unsupported_models_keep_the_reason() {
        let mut info = model("upstream-lstm-0.5.4.nam");
        let mut state = state(info.clone(), input(70 * 44_100, 44_100, 2, false));
        let options = AssetOptions {
            input_wav: "input.wav".into(),
            nam_output: NamOutput::ExperimentalClo,
            ..Default::default()
        };
        assert!(
            state
                .preflight("model.nam", &options)
                .issues
                .iter()
                .any(|issue| issue.contains("verified GP-200"))
        );
        info.properties
            .insert("inference_available".into(), "false".into());
        info.warnings = vec!["NAM architecture FutureModel has no implemented local kernel".into()];
        state.inspect(JobId(3), "model.nam".into(), InspectionPurpose::Model);
        state.observe(&AssetEvent::Inspected {
            job: JobId(3),
            info,
        });
        assert_eq!(
            state.preflight("model.nam", &options).issues,
            ["NAM architecture FutureModel has no implemented local kernel"]
        );
    }
    #[test]
    fn fitting_remains_cancellable_and_completion_does_not_prepare_a_device_job() {
        let mut state = NamUiState::default();
        let spec = AssetJobSpec {
            id: JobId(8),
            kind: AssetKind::Nam,
            source: "model.nam".into(),
            output: None,
            options: BTreeMap::from([("output_format".into(), "clo".into())]),
        };
        state.start_conversion(&spec);
        state.observe(&AssetEvent::JobProgress {
            job_id: JobId(8),
            progress: JobProgress {
                completed: 70,
                total: Some(70),
                message: "response rendered".into(),
                cancellable: true,
            },
        });
        assert!(state.busy());
        let info = input(1, 48_000, 1, false);
        state.observe(&AssetEvent::Prepared {
            job: JobId(9),
            info: info.clone(),
        });
        assert!(state.busy());
        state.observe(&AssetEvent::Failed {
            job: JobId(8),
            error: ServiceError::new(ErrorKind::Cancelled, "Local conversion cancelled"),
        });
        assert!(!state.busy());
        assert!(
            state
                .conversion
                .as_ref()
                .unwrap()
                .terminal
                .as_ref()
                .unwrap()
                .is_err()
        );
        state.start_conversion(&spec);
        state.observe(&AssetEvent::Prepared {
            job: JobId(8),
            info,
        });
        assert!(!state.busy());
        assert!(
            state
                .conversion
                .as_ref()
                .unwrap()
                .terminal
                .as_ref()
                .unwrap()
                .is_ok()
        );
    }
    #[test]
    fn both_outputs_render_in_both_themes_without_file_io_or_device_effects() {
        for size in [egui::vec2(960.0, 640.0), egui::vec2(1280.0, 800.0)] {
            for theme in [ThemePreference::Dark, ThemePreference::Light] {
                let controller = gp200_app::AppController::with_demo(
                    gp200_sim::captured_demo_snapshot(),
                    AppPreferences::default(),
                )
                .unwrap();
                let services = crate::runtime::RuntimeServices::new(
                    Box::new(gp200_sim::simulator_service(Arc::new(|| {}))),
                    Arc::new(|| {}),
                );
                let mut app = StudioApp::with_services(controller, services);
                app.selected_asset_kind = AssetKind::Nam;
                app.asset_path = "model.nam".into();
                app.asset_options.input_wav = "input.wav".into();
                app.asset_options.channel_mode = 1;
                app.nam_ui = state(
                    model("upstream-lstm-0.5.4.nam"),
                    input(70 * 44_100, 44_100, 2, true),
                );
                let original = app.controller.active_document().unwrap().draft.clone();
                let context = egui::Context::default();
                crate::theme::install_fonts(&context);
                crate::theme::apply(&context, theme, 1.0);
                for output in [NamOutput::Wav, NamOutput::ExperimentalClo] {
                    app.asset_options.nam_output = output;
                    let mut shapes = context.run_ui(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                            ..Default::default()
                        },
                        |ui| app.nam_processing_options(ui),
                    );
                    assert!(!shapes.shapes.is_empty());
                    shapes.textures_delta.clear();
                    assert!(!app.services.has_pending());
                    assert!(!app.controller.device_operation_pending());
                    assert!(app.controller.state().prepared_jobs.is_empty());
                    assert_eq!(app.controller.active_document().unwrap().draft, original);
                }
            }
        }
    }
    #[test]
    fn a_review_is_a_local_spec_and_does_not_start_conversion_or_pedal_writes() {
        let controller = gp200_app::AppController::with_demo(
            gp200_sim::captured_demo_snapshot(),
            AppPreferences::default(),
        )
        .unwrap();
        let services = crate::runtime::RuntimeServices::new(
            Box::new(gp200_sim::simulator_service(Arc::new(|| {}))),
            Arc::new(|| {}),
        );
        let mut app = StudioApp::with_services(controller, services);
        app.selected_asset_kind = AssetKind::Nam;
        app.asset_path = "model.nam".into();
        app.asset_options = AssetOptions {
            input_wav: "input.wav".into(),
            nam_output: NamOutput::ExperimentalClo,
            ..Default::default()
        };
        app.nam_ui = state(
            model("upstream-lstm-0.5.4.nam"),
            input(70 * 44_100, 44_100, 2, true),
        );
        app.submit_asset(true);
        let spec = app.asset_review.as_ref().unwrap();
        assert_eq!(spec.options["output_format"], "clo");
        assert!(!app.services.has_pending());
        assert!(app.local_asset_jobs.is_empty());
        assert!(app.controller.state().prepared_jobs.is_empty());
        assert!(!app.nam_ui.busy());
    }
    #[test]
    fn selected_files_flow_through_the_real_worker_into_a_retained_wav() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "gp200-nam-gui-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        std::fs::create_dir(&root).unwrap();
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/assets/upstream-lstm-0.5.4.nam");
        let wav = root.join("selected.wav");
        let audio = gp200_assets::WaveAudio {
            channels: 1,
            sample_rate: 48_000,
            source_encoding: gp200_assets::WaveEncoding::Pcm,
            source_bits: 16,
            samples: (0..2057)
                .map(|index| (index as f32 * 0.17).sin() * 0.2)
                .collect(),
        };
        std::fs::write(&wav, gp200_assets::write_pcm16_wav(&audio).unwrap()).unwrap();
        let controller = gp200_app::AppController::with_demo(
            gp200_sim::captured_demo_snapshot(),
            AppPreferences::default(),
        )
        .unwrap();
        let mut services = crate::runtime::RuntimeServices::new(
            Box::new(gp200_sim::simulator_service(Arc::new(|| {}))),
            Arc::new(|| {}),
        );
        services.assets = Some(Box::new(
            gp200_assets::AssetWorker::open(root.join("assets"), Arc::new(|| {})).unwrap(),
        ));
        let mut app = StudioApp::with_services(controller, services);
        app.open_nam_workspace(NamWorkspaceLaunch {
            model: source,
            input: wav,
            output: NamOutput::Wav,
            operation_limit: None,
            review_when_ready: false,
            input_channel: None,
            resample_input: true,
            nam_size: None,
        });
        let pump = |app: &mut StudioApp| {
            for input in app.services.pump().0 {
                if let AppInput::Assets(event) = &input {
                    app.nam_ui.observe(event);
                    if let AssetEvent::Inspected { job, .. }
                    | AssetEvent::Prepared { job, .. }
                    | AssetEvent::Failed { job, .. } = event
                    {
                        app.local_asset_jobs.remove(job);
                    }
                }
                let effects = app.controller.dispatch(input);
                app.services.enqueue(effects);
            }
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.nam_ui.inspecting() {
            pump(&mut app);
            assert!(std::time::Instant::now() < deadline, "Inspection timed out");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            app.nam_ui
                .preflight(&app.asset_path, &app.asset_options)
                .issues
                .is_empty()
        );
        app.submit_asset(true);
        let spec = app.asset_review.clone().unwrap();
        assert!(!spec.options.contains_key("output_format"));
        app.start_local_asset_job(spec);
        while app.nam_ui.busy() {
            pump(&mut app);
            assert!(std::time::Instant::now() < deadline, "Rendering timed out");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let info = app
            .nam_ui
            .conversion
            .as_ref()
            .unwrap()
            .terminal
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap();
        let derived = gp200_assets::inspect_wav(&std::fs::read(&info.path).unwrap()).unwrap();
        assert_eq!(derived.channels, 2);
        assert_eq!(derived.source_bits, 16);
        assert!(derived.frames() > 0);
        assert!(app.controller.state().prepared_jobs.is_empty());
        assert!(!app.controller.device_operation_pending());
        assert!(app.local_asset_jobs.is_empty());
    }
}
