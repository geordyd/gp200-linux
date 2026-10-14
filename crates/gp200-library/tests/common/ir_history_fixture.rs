//! Shared synthetic IR history fixture; never opens a device.
#![allow(dead_code)] // Shared synthetic helper functions are used by different test harnesses.
use gp200_domain::*;
use gp200_library::*;
use gp200_protocol::{RawRecord, RawTransfer, WRITE_KIND};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

pub const WAV: &[u8] =
    include_bytes!("../../../../tests/fixtures/device-assets/synthetic-ir-pcm24.wav");
pub const COEFFICIENTS: &[u8] =
    include_bytes!("../../../../tests/fixtures/device-assets/synthetic-ir-coefficients.bin");
pub struct TempRoot(pub PathBuf);
impl TempRoot {
    pub fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "gp200-ir-ledger-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    pub fn journal(&self, id: &IrAttemptId) -> PathBuf {
        self.0
            .join("ir-attempt-ledger")
            .join(format!("{}.journal", id.as_str()))
    }
}
impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
pub struct Fixture {
    pub job: PreparedDeviceJob,
    pub identity: RawTransfer,
}
pub fn retained(bytes: &[u8]) -> RetainedJobBlob {
    RetainedJobBlob::checked(
        AssetId(format!("sha256:{}", JobSha256::of(bytes).hex())),
        Arc::from(bytes),
        JobSha256::of(bytes),
    )
    .unwrap()
}
pub fn metadata(
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
pub fn fixture() -> Fixture {
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
        "Offline IR test".into(),
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
    Fixture { job, identity }
}
pub fn material(fixture: &Fixture) -> IrAttemptMaterials {
    IrAttemptMaterials::from_prepared(&fixture.job, WAV, fixture.identity.payload()).unwrap()
}
pub fn status(
    context: IrObservationContextDto,
    at_ms: u64,
    slot: u16,
    flags: u16,
    value: u32,
) -> IrAttemptEventDto {
    let mut body = slot.to_le_bytes().to_vec();
    body.extend_from_slice(&flags.to_le_bytes());
    body.extend_from_slice(&value.to_le_bytes());
    IrAttemptEventDto::Status {
        context,
        at_ms,
        raw: RawTransfer::from_records(
            WRITE_KIND,
            &[RawRecord {
                command: 0x100b,
                body,
            }],
        )
        .unwrap(),
    }
}
pub fn send(
    context: IrObservationContextDto,
    at_ms: u64,
    count: u64,
    completion: IrSendCompletion,
) -> IrAttemptEventDto {
    IrAttemptEventDto::Send {
        context,
        at_ms,
        extent: IrTransportExtent::OfferedToProcess,
        wire_bytes: count,
        completion,
    }
}
pub fn complete() -> IrSendCompletion {
    IrSendCompletion::Process(IrProcessObservation {
        exit_code: Some(0),
        timed_out: false,
        reaped: true,
    })
}
pub fn metadata_event(binding: &IrAttemptBinding, at_ms: u64) -> IrAttemptEventDto {
    let pins = binding.pins();
    let snapshot = metadata(pins.name_raw, pins.additive_checksum, 0, 3, at_ms);
    IrAttemptEventDto::Metadata {
        context: binding.context().to_dto(),
        at_ms,
        metadata: IrMetadataObservationDto {
            slot: snapshot.slot(),
            stamp: snapshot.stamp(),
            revision: snapshot.revision(),
            source: snapshot.source().clone(),
            captured_at_ms: snapshot.captured_at_ms(),
            raw: (**snapshot.raw()).clone(),
        },
    }
}
pub fn first_frame_end(bytes: &[u8]) -> usize {
    44 + u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize
}

pub fn populate(store: &mut LibraryStore, complete_observation: bool) -> IrAttemptId {
    let f = fixture();
    let mut ledger = IrAttemptLedger::open(store).unwrap();
    let id = ledger.reserve_id().unwrap();
    let binding = IrAttemptBinding::from_prepared(id.clone(), &f.job).unwrap();
    let mut cursor = ledger
        .begin_reserved(&id, binding.clone(), &material(&f), 100)
        .unwrap();
    if complete_observation {
        cursor = ledger
            .append_observation(
                &cursor,
                send(binding.context().to_dto(), 101, 8570, complete()),
            )
            .unwrap();
        cursor = ledger
            .append_observation(
                &cursor,
                status(binding.context().to_dto(), 102, 19, 0x9001, 0xfedcba98),
            )
            .unwrap();
        cursor = ledger
            .append_observation(&cursor, metadata_event(&binding, 103))
            .unwrap();
        ledger
            .close(&cursor, IrAttemptFinishReason::ObservationComplete)
            .unwrap();
    }
    id
}
