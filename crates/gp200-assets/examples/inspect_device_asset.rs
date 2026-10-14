//! Bounded local inspection CLI. Never opens MIDI or creates a device ticket.
use gp200_assets::{
    IrLengthPolicy, MAX_SOURCE_BYTES, inspect_firmware, inspect_snaptone, pack_device_ir,
};
use std::{env, fs::File, io::Read};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = env::args().skip(1).collect();
    if arguments.len() != 2 {
        return Err("Usage: inspect_device_asset firmware|clo|ir-pcm24 FILE".into());
    }
    let file = File::open(&arguments[1])?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > MAX_SOURCE_BYTES as u64 {
        return Err("Input must be a regular file within 64 MiB".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_SOURCE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err("Input grew past 64 MiB".into());
    }
    let inspected = match arguments[0].as_str() {
        "firmware" => serde_json::to_value(inspect_firmware(&bytes)?.info())?,
        "clo" => serde_json::to_value(inspect_snaptone(&bytes)?.info())?,
        "ir-pcm24" => {
            serde_json::to_value(pack_device_ir(&bytes, IrLengthPolicy::RejectTruncation)?.info())?
        }
        _ => return Err("Unknown inspection kind".into()),
    };
    println!("{}", serde_json::to_string_pretty(&inspected)?);
    Ok(())
}
