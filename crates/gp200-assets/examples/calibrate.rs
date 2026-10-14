//! Explicit local conversion example. Does not open MIDI or create device jobs.
use gp200_assets::AssetStore;
use gp200_domain::{AssetJobSpec, AssetKind, JobId};
use std::{collections::BTreeMap, path::PathBuf};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.len() != 4 {
        eprintln!("Usage: calibrate MODEL.nam VERIFIED_STIMULUS.wav PRIVATE_STORE OUTPUT.clo");
        std::process::exit(2);
    }
    let mut store = AssetStore::open(PathBuf::from(&arguments[2]))?;
    let result = store.prepare_with_progress(
        &AssetJobSpec {
            id: JobId(1),
            kind: AssetKind::Nam,
            source: PathBuf::from(&arguments[0]),
            output: Some(PathBuf::from(&arguments[3])),
            options: BTreeMap::from([
                ("output_format".into(), "clo".into()),
                ("input_channel".into(), "0".into()),
                (
                    "input_wav".into(),
                    arguments[1].to_string_lossy().into_owned(),
                ),
            ]),
        },
        || false,
        |done, total| {
            if done == 0 || done == total {
                eprintln!("NAM samples {done}/{total}; offline calibration follows");
            }
        },
    )?;
    println!("{}", serde_json::to_string_pretty(&result.derived)?);
    Ok(())
}
