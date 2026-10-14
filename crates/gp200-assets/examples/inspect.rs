//! Local asset CLI. It has no device backend or MIDI capability.
use gp200_assets::*;
use gp200_domain::*;
use std::{collections::BTreeMap, path::PathBuf};
fn main() {
    if let Err(error) = run() {
        eprintln!("{:?}: {}", error.kind, error.message);
        std::process::exit(1);
    }
}
fn run() -> Result<(), ServiceError> {
    let mut source = None;
    let mut kind = AssetKind::ImpulseResponse;
    let mut root = None;
    let mut output = None;
    let mut options = BTreeMap::new();
    let mut prepare = false;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--help" => {
                println!(
                    "Local GP200 asset inspector\n--kind ir|nam|snaptone|firmware --input PATH [--store ABSOLUTE_PATH]\n--prepare [--output NEW_PATH] [--option KEY=VALUE]\nIR options: sample_rate, channel=preserve|average|INDEX, normalize_peak, truncate_frames\nNAM options: input_wav, input_channel=average|INDEX, resample_input=true|false, activation=standard|fast\nNAM output is a local stereo WAV, never an upload ticket."
                );
                return Ok(());
            }
            "--input" => source = arguments.next().map(PathBuf::from),
            "--store" => root = arguments.next().map(PathBuf::from),
            "--output" => output = arguments.next().map(PathBuf::from),
            "--prepare" => prepare = true,
            "--kind" => {
                kind = match arguments.next().as_deref() {
                    Some("ir") => AssetKind::ImpulseResponse,
                    Some("nam") => AssetKind::Nam,
                    Some("snaptone") => AssetKind::SnapTone,
                    Some("firmware") => AssetKind::Firmware,
                    _ => return Err(ServiceError::invalid("Unknown asset kind")),
                }
            }
            "--option" => {
                let text = arguments
                    .next()
                    .ok_or_else(|| ServiceError::invalid("Missing option"))?;
                let (key, value) = text
                    .split_once('=')
                    .ok_or_else(|| ServiceError::invalid("Option must be KEY=VALUE"))?;
                options.insert(key.into(), value.into());
            }
            _ => {
                return Err(ServiceError::invalid(format!(
                    "Unknown argument {argument}"
                )));
            }
        }
    }
    let source =
        source.ok_or_else(|| ServiceError::invalid("An input path is required; use --help"))?;
    let mut store = AssetStore::open(root.map(Ok).unwrap_or_else(data_dir)?)?;
    let spec = AssetJobSpec {
        id: JobId(1),
        kind,
        source,
        output,
        options,
    };
    let info = if prepare {
        store
            .prepare(&spec, || false)?
            .derived
            .expect("prepared output")
    } else {
        store.inspect(&spec)?
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&info)
            .map_err(|error| ServiceError::invalid(error.to_string()))?
    );
    Ok(())
}
