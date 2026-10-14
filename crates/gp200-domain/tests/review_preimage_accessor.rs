//! Exact immutable review bytes; no library or transport is opened.
use gp200_domain::*;
use gp200_protocol::{RawRecord, RawTransfer, WRITE_KIND};
use std::{collections::BTreeMap, sync::Arc};
const WAV: &[u8] = include_bytes!("../../../tests/fixtures/device-assets/synthetic-ir-pcm24.wav");
const COEFFICIENTS: &[u8] =
    include_bytes!("../../../tests/fixtures/device-assets/synthetic-ir-coefficients.bin");
struct Fixture {
    job: PreparedDeviceJob,
    source: RetainedJobBlob,
    coefficients: RetainedJobBlob,
    session: DeviceJobSession,
    baseline: Arc<DeviceAssetMetadata>,
    options: Arc<[u8]>,
}
fn retained(bytes: &[u8]) -> RetainedJobBlob {
    RetainedJobBlob::checked(
        AssetId(format!("sha256:{}", JobSha256::of(bytes).hex())),
        Arc::from(bytes),
        JobSha256::of(bytes),
    )
    .unwrap()
}
fn metadata(
    name: [u8; 16],
    sum: u32,
    flags: u16,
    revision: u64,
    captured_at_ms: u64,
) -> DeviceAssetMetadata {
    let mut body = 19u16.to_le_bytes().to_vec();
    body.extend_from_slice(&flags.to_le_bytes());
    body.extend_from_slice(&sum.to_le_bytes());
    body.extend_from_slice(&name);
    DeviceAssetMetadata::from_transfer(
        AssetSlot::new(AssetBank::Ir, 19).unwrap(),
        RequestStamp {
            epoch: SessionEpoch(7),
            intent: IntentId(23),
        },
        Revision(revision),
        SnapshotSource::Hardware {
            epoch: SessionEpoch(7),
        },
        captured_at_ms,
        RawTransfer::from_records(
            WRITE_KIND,
            &[RawRecord {
                command: 0x1009,
                body,
            }],
        )
        .unwrap(),
    )
    .unwrap()
}
fn fixture() -> Fixture {
    let identity = RawTransfer::from_records(
        WRITE_KIND,
        &[RawRecord {
            command: 0x2001,
            body: vec![1, 0, 2, 0],
        }],
    )
    .unwrap();
    let caps = Capabilities::gp200_read_only();
    let session = DeviceJobSession::observe_normal(
        SessionEpoch(7),
        DeviceSelection {
            input_id: "synthetic:input".into(),
            output_id: "synthetic:output".into(),
            fingerprint_hint: None,
        },
        Revision(1),
        &identity,
        &DeviceIdentity {
            model: DeviceModel::Gp200,
            mode: BackendMode::Hardware,
            firmware: None,
            display_name: "Offline fixture".into(),
        },
        &caps,
    )
    .unwrap();
    let source = retained(WAV);
    let coefficients = retained(COEFFICIENTS);
    let mut before_name = [0; 16];
    before_name[..7].copy_from_slice(b"User IR");
    let baseline = Arc::new(metadata(before_name, 0, 0, 2, 99));
    let options: Arc<[u8]> = Arc::from(b"{\"truncate\":false}".as_slice());
    let recipe = NormalAssetRecipe::ir(
        AssetSlot::new(AssetBank::Ir, 19).unwrap(),
        "Linux IR test".into(),
        source.clone(),
        IrJobLengthPolicy::RejectLong,
        coefficients.clone(),
    )
    .unwrap();
    let conversion = JobConversion::checked(
        recipe.codec_revision().into(),
        options.clone(),
        JobSha256::of(&options),
        BTreeMap::new(),
    )
    .unwrap();
    let job = PreparedDeviceJob::prepare(
        JobId(4),
        session.clone(),
        source.clone(),
        recipe,
        conversion,
        baseline.clone(),
        AssetBackupReview::Unavailable {
            reason: "Only metadata is retained".into(),
        },
    )
    .unwrap();
    Fixture {
        job,
        source,
        coefficients,
        session,
        baseline,
        options,
    }
}
#[test]
fn exact_review_preimage_equals_existing_digest_and_original_tuple_recipe() {
    let f = fixture();
    let actual = f.job.review_preimage().unwrap();
    assert_eq!(JobSha256::of(&actual), f.job.review_sha256());
    // Independently spelled from the original pre-extraction private recipe.
    // This catches tuple/field/order/option changes, not merely accessor wiring.
    let expected = serde_json::to_vec(&(
        (
            "gp200-reviewed-asset-v1",
            JobId(4),
            &f.session,
            f.source.id(),
            f.source.sha256(),
            WAV.len(),
            AssetSlot::new(AssetBank::Ir, 19).unwrap(),
            "Linux IR test",
            "gp200-ir-44100-pcm24-v1",
            Option::<NativeCloProfile>::None,
            Some(IrJobLengthPolicy::RejectLong),
            f.source.sha256(),
            f.coefficients.sha256(),
            COEFFICIENTS.len(),
        ),
        (
            "gp200-ir-44100-pcm24-v1",
            JobSha256::of(&f.options),
            Vec::<u8>::new(),
            f.baseline.revision(),
            f.baseline.source(),
            f.baseline.captured_at_ms(),
            JobSha256::of(f.baseline.raw().payload()),
            (Option::<u8>::None, Some("Only metadata is retained")),
        ),
    ))
    .unwrap();
    assert_eq!(actual, expected);
    let changed = PreparedDeviceJob::prepare(
        JobId(5),
        f.session.clone(),
        f.source.clone(),
        f.job.recipe().clone(),
        JobConversion::checked(
            "gp200-ir-44100-pcm24-v1".into(),
            f.options.clone(),
            JobSha256::of(&f.options),
            BTreeMap::new(),
        )
        .unwrap(),
        f.baseline.clone(),
        AssetBackupReview::Unavailable {
            reason: "Only metadata is retained".into(),
        },
    )
    .unwrap();
    assert_ne!(changed.review_preimage().unwrap(), actual);
    assert_ne!(changed.review_sha256(), f.job.review_sha256());
}
