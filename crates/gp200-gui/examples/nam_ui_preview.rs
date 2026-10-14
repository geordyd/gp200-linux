//! Native offline UI evidence with real worker inspections; never constructs a hardware backend.
use gp200_domain::*;
use gp200_gui::{LaunchOptions, NamOutput, NamWorkspaceLaunch, StudioApp};
use std::{collections::BTreeMap, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 6 {
        return Err("Usage: nam_ui_preview wav|budget|budget-opt|size|size-history|clo|clo-wrong|clo-review|complete MODEL INPUT dark|light WIDTHxHEIGHT SCREENSHOT".into());
    }
    let mode = args[0].as_str();
    let output = match mode {
        "wav" | "budget" | "budget-opt" | "size" | "size-history" => NamOutput::Wav,
        "clo" | "clo-wrong" | "clo-review" | "complete" => NamOutput::ExperimentalClo,
        _ => return Err("Unknown preview mode".into()),
    };
    let model = PathBuf::from(&args[1]);
    let input = PathBuf::from(&args[2]);
    if mode == "size-history" {
        let mut store = gp200_assets::AssetStore::open(gp200_assets::data_dir()?)?;
        // Distinct requests in the same native bin produce identical bytes and distinct durable recipes.
        for (id, size) in [(97, "0"), (98, "0.2")] {
            store.prepare(
                &AssetJobSpec {
                    id: JobId(id),
                    kind: AssetKind::Nam,
                    source: model.clone(),
                    output: None,
                    options: BTreeMap::from([
                        ("input_wav".into(), input.to_string_lossy().into_owned()),
                        ("nam_size".into(), size.into()),
                    ]),
                },
                || false,
            )?;
        }
        println!(
            "{}",
            serde_json::json!({"offline_conversion":true, "hardware_commands":0, "conversion_recipe_count":store.conversion_recipes().len()})
        );
    }
    if mode == "complete" {
        // Run before creating a window. The GUI then inspects these same real
        // sources and reads the durable result from its normal asset worker.
        let mut store = gp200_assets::AssetStore::open(gp200_assets::data_dir()?)?;
        if !store.inventory().iter().any(|asset| {
            asset.properties.contains_key("calibration_report_json")
                && asset.properties.get("declared_bytes").map(String::as_str) == Some("4744")
        }) {
            let result = store.prepare(
                &AssetJobSpec {
                    id: JobId(99),
                    kind: AssetKind::Nam,
                    source: model.clone(),
                    output: None,
                    options: BTreeMap::from([
                        ("output_format".into(), "clo".into()),
                        ("input_wav".into(), input.to_string_lossy().into_owned()),
                        ("input_channel".into(), "0".into()),
                        ("activation".into(), "fast".into()),
                        ("max_operations".into(), "2000000000".into()),
                        ("calibration_timeout_seconds".into(), "900".into()),
                    ]),
                },
                || false,
            )?;
            println!(
                "{}",
                serde_json::json!({ "offline_conversion": true, "hardware_commands": 0, "result": result.derived })
            );
        }
    }
    let mut options = LaunchOptions::parse([
        "--demo".into(),
        "--view".into(),
        "assets".into(),
        "--theme".into(),
        args[3].clone(),
        "--size".into(),
        args[4].clone(),
        "--screenshot".into(),
        args[5].clone(),
    ])?;
    options.nam_workspace = Some(NamWorkspaceLaunch {
        model,
        input,
        output,
        operation_limit: (mode == "budget-opt").then_some(100_000_000_000),
        review_when_ready: mode == "clo-review",
        input_channel: Some(0),
        resample_input: output == NamOutput::Wav,
        nam_size: matches!(mode, "size" | "size-history").then_some(0.0),
    });
    let status = options.qa_result.clone();
    eframe::run_native(
        "GP200 Studio · offline NAM preview",
        eframe::NativeOptions {
            persist_window: false,
            glow_options: eframe::egui_glow::GlowConfiguration {
                vsync: false,
                ..Default::default()
            },
            viewport: eframe::egui::ViewportBuilder::default()
                .with_inner_size([options.width.unwrap(), options.height.unwrap()])
                .with_min_inner_size([640.0, 480.0]),
            ..Default::default()
        },
        Box::new(move |cc| {
            StudioApp::new(cc, options)
                .map(|app| Box::new(app) as Box<dyn eframe::App>)
                .map_err(Into::into)
        }),
    )?;
    let code = status.load(std::sync::atomic::Ordering::Relaxed);
    if code != 0 {
        return Err(format!("Preview exited with QA status {code}").into());
    }
    Ok(())
}
