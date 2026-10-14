//! Developer-only, pure-codec CLI. It never constructs an ALSA backend.
use gp200_domain::BackendMode;
use gp200_protocol::{encode_transfer, identify_request, read_current_request};
use gp200_sim::*;
use serde_json::{Value, json};
use std::{
    error::Error,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
    time::Duration,
};

fn read_bounded(path: &Path) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("Scenario/manifest exceeds 8 MiB".into());
    }
    Ok(bytes)
}
fn delivery_json(deliveries: Vec<Delivery>) -> Vec<Value> {
    deliveries.into_iter().map(|delivery| match delivery {
        Delivery::Bytes { at, bytes } => json!({"at_ms":at.as_millis(),"bytes_hex":bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>()}),
        Delivery::Disconnected { at } => json!({"at_ms":at.as_millis(),"disconnected":true}),
    }).collect()
}
fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "--offline".into());
    let mut output = None;
    let result = match mode.as_str() {
        "--offline" => {
            let scenario = SimulationScenario { schema_version: 1, profile: ReplyProfile::default(), steps: vec![SimulationStep { at_ms: 0, request: identify_request(), fault: None }, SimulationStep { at_ms: 1, request: read_current_request(), fault: None }] };
            let (log, deliveries) = run_scenario(scenario)?;
            json!({"backend_mode":BackendMode::Simulator,"log":log,"deliveries":delivery_json(deliveries)})
        }
        "--simulate" => {
            let path = args.next().ok_or("--simulate requires a scenario JSON path")?;
            let scenario: SimulationScenario = serde_json::from_slice(&read_bounded(Path::new(&path))?)?;
            let (log, deliveries) = run_scenario(scenario)?;
            json!({"backend_mode":BackendMode::Simulator,"log":log,"deliveries":delivery_json(deliveries)})
        }
        "--replay" | "--passive" => {
            let path = args.next().ok_or("Replay requires a manifest JSON path")?;
            let manifest = ReplayManifest::parse(&read_bounded(Path::new(&path))?)?;
            let exchanges = manifest.exchanges()?;
            let synthetic_timing = exchanges.iter().any(|exchange| exchange.synthetic_timing);
            let deliveries = if mode == "--replay" {
                let expected = exchanges.clone(); let mut replay = ExpectationReplay::new(exchanges)?; let mut deliveries = Vec::new();
                for (index, exchange) in expected.iter().enumerate() {
                    for packet in encode_transfer(&exchange.expected_request)? { deliveries.extend(replay.receive(&packet, Duration::from_millis(index as u64 * 100))?); }
                }
                deliveries
            } else {
                let chunks = exchanges.into_iter().enumerate().flat_map(|(index, exchange)| exchange.response.into_iter().map(move |chunk| CaptureChunk { offset: Duration::from_millis(index as u64 * 100) + chunk.offset, bytes: chunk.bytes })).collect();
                let mut passive = PassiveReplay::new(chunks)?; passive.advance(Duration::from_secs(3600))
            };
            json!({"backend_mode":BackendMode::CaptureReplay,"synthetic_timing":synthetic_timing,"exchange_order_timing":"Synthetic 100ms spacing for this CLI","passive":mode=="--passive","deliveries":delivery_json(deliveries)})
        }
        _ => return Err("Usage: codec_lab [--offline | --simulate SCENARIO | --replay MANIFEST | --passive MANIFEST] [--output NEW_FILE]".into()),
    };
    while let Some(argument) = args.next() {
        if argument != "--output" || output.is_some() {
            return Err("Unexpected or repeated argument".into());
        }
        output = Some(args.next().ok_or("--output requires a new file path")?);
    }
    let bytes = serde_json::to_vec_pretty(&result)?;
    if let Some(path) = output {
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
    } else {
        std::io::stdout().write_all(&bytes)?;
        std::io::stdout().write_all(b"\n")?;
    }
    Ok(())
}
