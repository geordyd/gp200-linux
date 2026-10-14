//! Offline file-review entry point. It never opens ALSA/MIDI or executes a
//! vendor binary. FirmwareReview cannot be converted into a device command.
use gp200_domain::{AssetId, FirmwareBackupAssessment, FirmwareReview, JobSha256, RetainedJobBlob};
use std::{io::Read, path::PathBuf, sync::Arc};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("Usage: cargo run -p gp200-domain --example review_firmware -- firmware.bin")?;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    let bytes: Arc<[u8]> = bytes.into();
    let digest = JobSha256::of(&bytes);
    let source = RetainedJobBlob::checked(AssetId(digest.hex()), bytes, digest)?;
    let review = FirmwareReview::inspect(source, None, &FirmwareBackupAssessment::default())?;
    println!("{}", serde_json::to_string_pretty(review.summary())?);
    Ok(())
}
