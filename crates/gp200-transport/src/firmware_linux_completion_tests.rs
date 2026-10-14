//! Offline checks of actual local ownership/evidence boundaries; no physical permit.
use crate::{
    firmware_linux_completion_admission::private_directory, firmware_linux_completion_io::*,
    firmware_linux_completion_journal::*,
};
use gp200_domain::*;
use gp200_library::LibraryStore;
use sha2::Digest;
use std::{
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
// Exact receiver response retained from the one reviewed physical query in
// 06d0815c833cdf1abe81286ac36d3420. Replaying it here opens no physical device.
// V1.0.6 belongs to the receiver, not the image or application firmware.
const RETAINED_GP200_180_IDENTITY: [u8; 39] = [
    0xf0, 0x7e, 0x48, 1, b'G', b'P', b'-', b'2', b'0', b'0', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, b'V',
    b'1', b'.', b'0', b'.', b'6', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x0b, 0x04, 0xf7,
];
// Exact second-section descriptor and reply from the physical ae1a48b2209f874b8dc4f4056996b189
// run. The original journal records UnexpectedTraffic; preserve these historical
// bytes separately from the corrected native descriptor expected by current tests.
// Its fresh identity was byte-identical to RETAINED_GP200_180_IDENTITY above.
const RETAINED_SECOND_DESCRIPTOR_180: [u8; 21] = [
    0xf0, 0x7e, 0x48, 2, 0, 0, 1, 0x0b, 0x0f, 0x0c, 3, 0, 0, 0, 0, 0, 2, 7, 0x0c, 0x0e, 0xf7,
];
// Exact original sender actual21 from contained vendor full03 ordinal23300.
const NATIVE_SECOND_DESCRIPTOR_180: [u8; 21] = [
    0xf0, 0x7e, 0x48, 2, 0, 0, 0, 4, 8, 0, 0, 0, 0, 0, 0, 0, 0, 6, 6, 7, 0xf7,
];
const RETAINED_DESCRIPTOR_REJECTION: [u8; 6] = [0xf0, 0x7e, 0x48, 3, 0x7e, 0xf7];

#[test]
fn historical_linux_descriptor_stays_distinct_from_corrected_native_descriptor() {
    use gp200_protocol::FirmwareDescriptor;
    let historical = FirmwareDescriptor::decode(&RETAINED_SECOND_DESCRIPTOR_180).unwrap();
    assert_eq!(
        (historical.section_bytes(), historical.block_count()),
        (1_834_032, 10_190)
    );
    let native = FirmwareDescriptor::decode(&NATIVE_SECOND_DESCRIPTOR_180).unwrap();
    assert_eq!(
        (native.section_bytes(), native.block_count()),
        (294_912, 1_639)
    );
    assert_ne!(RETAINED_SECOND_DESCRIPTOR_180, NATIVE_SECOND_DESCRIPTOR_180);
}

#[test]
fn retained_gp200_receiver_identity_decodes_exact_180_byte_chunks() {
    use gp200_protocol::{BootloaderIdentity, FirmwareWireError};
    assert_eq!(
        JobSha256::of(&RETAINED_GP200_180_IDENTITY).hex(),
        "8ea5ccf0d18a28890d497f3773d39b3d6f50d30c5de064ddc733168a1d44a513"
    );
    let identity = BootloaderIdentity::decode(&RETAINED_GP200_180_IDENTITY).unwrap();
    assert_eq!(identity.chunk_bytes, 180);
    assert_eq!(identity.product_raw, *b"GP-200\0\0\0\0\0\0\0\0\0\0");
    assert_eq!(identity.version_raw, *b"V1.0.6\0\0\0\0\0\0\0\0\0\0");
    assert_eq!(identity.raw(), RETAINED_GP200_180_IDENTITY);

    let mut swapped = RETAINED_GP200_180_IDENTITY;
    swapped.swap(36, 37);
    assert_eq!(
        BootloaderIdentity::decode(&swapped).unwrap().chunk_bytes,
        75
    );
    let mut zero = RETAINED_GP200_180_IDENTITY;
    zero[36..38].fill(0);
    assert_eq!(
        BootloaderIdentity::decode(&zero),
        Err(FirmwareWireError::InvalidChunkLength)
    );
    let mut invalid_nibble = RETAINED_GP200_180_IDENTITY;
    invalid_nibble[36] = 16;
    assert_eq!(
        BootloaderIdentity::decode(&invalid_nibble),
        Err(FirmwareWireError::InvalidNibble)
    );
    assert_eq!(
        BootloaderIdentity::decode(&RETAINED_GP200_180_IDENTITY[..38]),
        Err(FirmwareWireError::InvalidShape)
    );
}

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "gp200-linux-completion-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir(&p).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn event(journal: &CompletionJournal) -> JournalEvent {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(e) = journal.poll() {
            return e;
        }
        assert!(Instant::now() < deadline, "actual worker did not return");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn authenticated_records(path: &std::path::Path) -> Vec<serde_json::Value> {
    let bytes = std::fs::read(path).unwrap();
    let mut offset = 0;
    let mut records = Vec::new();
    while offset < bytes.len() {
        assert!(bytes.len() - offset >= 44, "partial journal header");
        assert_eq!(&bytes[offset..offset + 8], b"GP2LCJ01");
        let len = u32::from_le_bytes(bytes[offset + 8..offset + 12].try_into().unwrap()) as usize;
        let body = &bytes[offset + 44..offset + 44 + len];
        assert_eq!(
            &bytes[offset + 12..offset + 44],
            sha2::Sha256::digest(body).as_slice()
        );
        records.push(serde_json::from_slice(body).unwrap());
        offset += 44 + len;
    }
    records
}
#[test]
fn journal_batch_keeps_ordinary_frames_order_final_prefix_and_one_sync() {
    let temp = Temp::new();
    let store = LibraryStore::open(temp.0.join("store")).unwrap();
    let root = store.root().to_owned();
    let profile = test_profile::Profile::new();
    profile.register_roots(&root, &temp.0.join("mock-usb"));
    let mut journal = CompletionJournal::start(
        store,
        "66666666666666666666666666666666".into(),
        serde_json::json!({"fixture":"batch ordinary frames only"}),
    )
    .unwrap_or_else(|failed| panic!("{}", failed.error));
    let values = vec![
        serde_json::json!({"kind":"write-syscall","ordinal":7}),
        serde_json::json!({"kind":"drain-syscall","ordinal":7}),
        serde_json::json!({"kind":"raw-chunk","observation":raw(1,99,&[240,247])}),
        serde_json::json!({"kind":"vendor-action-pair","ordinal":7}),
    ];
    // A filesystem short write is completed by write_all within the same
    // original journal operation; this does not authorize another native offer.
    journal.test_fail_next_batch(TestBatchFault::ShortWrite(7));
    let intent = journal.records(values.clone()).unwrap();
    let result = event(&journal);
    assert_eq!(result.intent, intent);
    let JournalReply::Recorded(receipt) = result.result.unwrap() else {
        panic!("batch minted checkpoint")
    };
    assert_eq!(receipt.sequence, 5);
    receipt.revalidate().unwrap();
    assert_eq!(
        &authenticated_records(journal.path())[1..],
        values.as_slice()
    );
    assert_eq!(
        receipt.prefix_sha256,
        JobSha256::of(&std::fs::read(journal.path()).unwrap()).hex()
    );
    let checkpoint = journal
        .checkpoint(
            8,
            "next exact checkpoint".into(),
            vec![240, 126, 72, 1, 247],
        )
        .unwrap();
    assert_eq!(event(&journal).intent, checkpoint);
    let returned = journal.join().unwrap();
    assert_eq!(returned.root(), root);
    let measured = profile.snapshot();
    assert_eq!(measured["successful_commits"], 3);
    assert_eq!(measured["phases"]["append_sync_all"]["count"], 3);
    assert_eq!(measured["phases"]["append_frame_hash_encode"]["count"], 6);
    assert_eq!(measured["phases"]["worker_reply_publication"]["count"], 2);
}
fn combined_outcome_values(ordinal: u64) -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({"kind":"write-syscall","ordinal":ordinal}),
        serde_json::json!({"kind":"drain-syscall","ordinal":ordinal}),
        serde_json::json!({"kind":"raw-chunk","observation":raw(1,99,&[240,247])}),
        serde_json::json!({"kind":"vendor-action-pair","receipt":{"ordinal":ordinal}}),
    ]
}
#[test]
fn combined_outcome_checkpoint_commits_once_and_binds_final_ordinary_frame() {
    let temp = Temp::new();
    let store = LibraryStore::open(temp.0.join("store")).unwrap();
    let profile = test_profile::Profile::new();
    profile.register_roots(store.root(), &temp.0.join("mock-usb"));
    let mut journal = CompletionJournal::start(
        store,
        "91919191919191919191919191919191".into(),
        serde_json::json!({"MockSimulation":true}),
    )
    .unwrap_or_else(|failed| panic!("{}", failed.error));
    let values = combined_outcome_values(7);
    let wire = vec![240, 126, 72, 4, 0, 1, 247];
    journal.test_fail_next_batch(TestBatchFault::ShortWrite(7));
    let intent = journal
        .outcome_checkpoint(values.clone(), 8, "vendor Block".into(), wire.clone())
        .unwrap();
    assert!(
        journal
            .checkpoint(9, "replacement".into(), wire.clone())
            .is_err()
    );
    let returned = event(&journal);
    assert_eq!(returned.intent, intent);
    let JournalReply::Checkpoint(mut checkpoint) = returned.result.unwrap() else {
        panic!("combined transaction omitted its actual checkpoint");
    };
    let receipt = checkpoint.outcome_receipt.take().unwrap();
    receipt.revalidate().unwrap();
    assert_eq!(receipt.sequence, 6);
    assert_eq!(checkpoint.sequence(), receipt.sequence);
    assert_eq!(checkpoint.prefix(), receipt.prefix_sha256);
    assert_eq!(checkpoint.ordinal(), 8);
    assert_eq!(checkpoint.wire(), wire);
    let records = authenticated_records(journal.path());
    assert_eq!(&records[1..5], values);
    assert_eq!(
        records[5],
        serde_json::json!({"kind":"checkpoint","ordinal":8,
        "purpose":"vendor Block","wire_sha256":JobSha256::of(&wire).hex(),"wire":wire})
    );
    assert_eq!(profile.snapshot()["successful_commits"], 2);
    assert_eq!(profile.snapshot()["phases"]["append_sync_all"]["count"], 2);
    drop(journal.offer_fence(checkpoint, 8, &wire).unwrap());
    let _ = journal.join().unwrap();
}
#[test]
fn combined_outcome_checkpoint_bounds_fail_before_any_bytes_and_issue_no_offer() {
    for values in [
        vec![serde_json::json!({"kind":"checkpoint","ordinal":1})],
        vec![serde_json::json!({"kind":"begin"})],
        vec![serde_json::json!({"kind":"raw-chunk"}); MAX_BATCH_RECORDS],
        vec![serde_json::json!({"kind":"raw-chunk","bytes":"x".repeat(400*1024)}); 3],
    ] {
        let temp = Temp::new();
        let mut journal = CompletionJournal::start(
            LibraryStore::open(temp.0.join("store")).unwrap(),
            "92929292929292929292929292929292".into(),
            serde_json::json!({"MockSimulation":true}),
        )
        .unwrap_or_else(|failed| panic!("{}", failed.error));
        let before = std::fs::read(journal.path()).unwrap();
        journal
            .outcome_checkpoint(values, 8, "vendor Block".into(), vec![240, 247])
            .unwrap();
        assert!(event(&journal).result.is_err());
        assert!(!journal.healthy());
        assert!(
            journal
                .checkpoint(8, "resubmit".into(), vec![240, 247])
                .is_err()
        );
        assert_eq!(std::fs::read(journal.path()).unwrap(), before);
        let _ = journal.join().unwrap();
    }
}
#[test]
fn combined_outcome_checkpoint_sync_reread_ack_and_revocation_never_mint_offer() {
    for fault in [
        TestBatchFault::WriteThenError(7),
        TestBatchFault::Sync,
        TestBatchFault::Reread,
        TestBatchFault::WrongIntent,
        TestBatchFault::Revoked,
    ] {
        let temp = Temp::new();
        let mut journal = CompletionJournal::start(
            LibraryStore::open(temp.0.join("store")).unwrap(),
            "93939393939393939393939393939393".into(),
            serde_json::json!({"MockSimulation":true}),
        )
        .unwrap_or_else(|failed| panic!("{}", failed.error));
        journal.test_fail_next_batch(fault);
        let intent = journal
            .outcome_checkpoint(
                combined_outcome_values(7),
                8,
                "vendor Block".into(),
                vec![240, 247],
            )
            .unwrap();
        let returned = event(&journal);
        assert!(returned.result.is_err(), "{fault:?}");
        assert!(!journal.healthy());
        assert!(
            journal
                .outcome_checkpoint(
                    combined_outcome_values(7),
                    8,
                    "replacement".into(),
                    vec![240, 247]
                )
                .is_err()
        );
        if matches!(fault, TestBatchFault::WrongIntent) {
            assert_eq!(journal.pending_intent(), Some(intent));
        }
        let _ = journal.join().unwrap();
    }
}
#[test]
fn combined_outcome_checkpoint_wrong_bytes_ordinal_run_and_consumption_fail_closed() {
    for case in ["bytes", "ordinal", "run", "consumed"] {
        let temp = Temp::new();
        let mut journal = CompletionJournal::start(
            LibraryStore::open(temp.0.join("store")).unwrap(),
            "94949494949494949494949494949494".into(),
            serde_json::json!({"MockSimulation":true}),
        )
        .unwrap_or_else(|failed| panic!("{}", failed.error));
        let wire = vec![240, 247];
        journal
            .outcome_checkpoint(
                combined_outcome_values(7),
                8,
                "vendor Block".into(),
                wire.clone(),
            )
            .unwrap();
        let JournalReply::Checkpoint(mut checkpoint) = event(&journal).result.unwrap() else {
            panic!("checkpoint absent");
        };
        // The durable frame remains authentic even when the owner rejects a wrong
        // invocation. No DTO or reread can restore the consumed capability.
        let rejected = match case {
            "bytes" => journal.offer_fence(checkpoint, 8, &[240, 1, 247]).is_err(),
            "ordinal" => journal.offer_fence(checkpoint, 9, &wire).is_err(),
            "run" => {
                let mut other = CompletionJournal::start(
                    LibraryStore::open(temp.0.join("other")).unwrap(),
                    "95959595959595959595959595959595".into(),
                    serde_json::json!({"MockSimulation":true}),
                )
                .unwrap_or_else(|failed| panic!("{}", failed.error));
                let rejected = other.offer_fence(checkpoint, 8, &wire).is_err();
                other.record(serde_json::json!({"kind":"test"})).unwrap();
                assert!(event(&other).result.is_ok());
                let _ = other.join().unwrap();
                rejected
            }
            "consumed" => {
                checkpoint.test_consume_offer();
                journal.offer_fence(checkpoint, 8, &wire).is_err()
            }
            _ => unreachable!(),
        };
        assert!(rejected, "{case}");
        let _ = journal.join().unwrap();
    }
}
#[test]
fn journal_batch_rejects_every_input_bound_before_writing_and_poisons_next_offer() {
    let cases = vec![
        vec![],
        vec![serde_json::json!({"kind":"begin"})],
        vec![serde_json::json!({"kind":"checkpoint"})],
        vec![serde_json::json!({"kind":"raw-chunk"}); MAX_BATCH_RECORDS + 1],
        vec![serde_json::json!({"kind":"raw-chunk","bytes":"x".repeat(512*1024)})],
        vec![serde_json::json!({"kind":"raw-chunk","bytes":"x".repeat(400*1024)}); 3],
    ];
    for values in cases {
        let temp = Temp::new();
        let mut journal = CompletionJournal::start(
            LibraryStore::open(temp.0.join("store")).unwrap(),
            "77777777777777777777777777777777".into(),
            serde_json::json!({"fixture":true}),
        )
        .unwrap_or_else(|failed| panic!("{}", failed.error));
        let prefix = std::fs::read(journal.path()).unwrap();
        journal.records(values).unwrap();
        assert!(event(&journal).result.is_err());
        assert_eq!(std::fs::read(journal.path()).unwrap(), prefix);
        assert!(!journal.healthy());
        assert!(
            journal
                .record(serde_json::json!({"kind":"failed"}))
                .is_err()
        );
        assert!(
            journal
                .checkpoint(0, "denied".into(), vec![240, 247])
                .is_err()
        );
        let returned = journal.join().unwrap();
        assert_eq!(returned.root(), temp.0.join("store"));
    }
}
#[test]
fn journal_batch_filesystem_faults_keep_original_prefix_and_poison_all_future_commands() {
    for fault in [
        TestBatchFault::WriteThenError(7),
        TestBatchFault::WriteThenError(51),
        TestBatchFault::Sync,
        TestBatchFault::Reread,
        TestBatchFault::WrongIntent,
        TestBatchFault::Revoked,
    ] {
        let temp = Temp::new();
        let mut journal = CompletionJournal::start(
            LibraryStore::open(temp.0.join("store")).unwrap(),
            "88888888888888888888888888888888".into(),
            serde_json::json!({"fixture":true}),
        )
        .unwrap_or_else(|failed| panic!("{}", failed.error));
        let prefix = std::fs::read(journal.path()).unwrap();
        journal.test_fail_next_batch(fault);
        let intent = journal
            .records(vec![
                serde_json::json!({"kind":"raw-chunk","bytes":[240,247]}),
                serde_json::json!({"kind":"vendor-action-pair"}),
            ])
            .unwrap();
        let reply = event(&journal);
        assert!(reply.result.is_err(), "{fault:?}");
        if matches!(fault, TestBatchFault::WrongIntent) {
            assert_eq!(journal.pending_intent(), Some(intent));
        }
        let bytes = std::fs::read(journal.path()).unwrap();
        assert_eq!(&bytes[..prefix.len()], prefix.as_slice());
        assert!(bytes.len() > prefix.len());
        assert!(!journal.healthy());
        assert!(
            journal
                .records(vec![serde_json::json!({"kind":"raw-chunk"})])
                .is_err()
        );
        assert!(
            journal
                .checkpoint(1, "no next offer".into(), vec![240, 247])
                .is_err()
        );
        let returned = journal.join().unwrap();
        assert_eq!(returned.root(), temp.0.join("store"));
        assert_eq!(
            std::fs::read(
                temp.0.join(
                    "store/linux-firmware-completion/88888888888888888888888888888888.journal"
                )
            )
            .unwrap(),
            bytes
        );
    }
}
#[test]
fn journal_batch_between_group_private_file_mutations_are_rejected() {
    use std::os::unix::fs::FileExt;
    for mutation in 0..5 {
        let temp = Temp::new();
        let mut journal = CompletionJournal::start(
            LibraryStore::open(temp.0.join("store")).unwrap(),
            "99999999999999999999999999999999".into(),
            serde_json::json!({"fixture":true}),
        )
        .unwrap_or_else(|failed| panic!("{}", failed.error));
        journal
            .records(vec![serde_json::json!({"kind":"raw-chunk"})])
            .unwrap();
        assert!(event(&journal).result.is_ok());
        match mutation {
            0 => {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(journal.path())
                    .unwrap();
                let prior_modified = file.metadata().unwrap().modified().unwrap();
                file.write_all_at(b"X", 0).unwrap();
                file.sync_all().unwrap();
                file.set_times(std::fs::FileTimes::new().set_modified(prior_modified))
                    .unwrap();
            }
            1 => std::fs::set_permissions(journal.path(), std::fs::Permissions::from_mode(0o644))
                .unwrap(),
            2 => std::fs::hard_link(journal.path(), temp.0.join("extra-link")).unwrap(),
            3 => {
                std::fs::rename(journal.path(), temp.0.join("old-inode")).unwrap();
                std::fs::write(journal.path(), b"replacement").unwrap();
            }
            4 => {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(journal.path())
                    .unwrap()
                    .set_len(1)
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let mutated = std::fs::read(journal.path()).unwrap();
        journal
            .records(vec![serde_json::json!({"kind":"raw-chunk"})])
            .unwrap();
        assert!(event(&journal).result.is_err(), "mutation {mutation}");
        assert_eq!(std::fs::read(journal.path()).unwrap(), mutated);
        assert!(
            journal
                .checkpoint(1, "denied".into(), vec![240, 247])
                .is_err()
        );
        journal.join().unwrap();
    }
}
fn raw(sequence: u64, at_ns: u64, bytes: &[u8]) -> FirmwareLifecycleRawObservation {
    FirmwareLifecycleRawObservation {
        sequence,
        at_ns,
        epoch: SessionEpoch(7),
        generation: 2,
        binding_sha256: JobSha256::of(b"fixture binding").hex(),
        clock_origin: "test original clock".into(),
        raw: bytes.into(),
    }
}
#[test]
fn journal_one_use_checkpoint_retains_exact_original_store_until_join() {
    let temp = Temp::new();
    let store = LibraryStore::open(temp.0.join("store")).unwrap();
    let root = store.root().to_owned();
    let mut journal = CompletionJournal::start(
        store,
        "11111111111111111111111111111111".into(),
        serde_json::json!({"origin":"mock filesystem only"}),
    )
    .unwrap_or_else(|f| panic!("{}", f.error));
    assert!(LibraryStore::open(&root).is_err());
    let wire = vec![0xf0, 0x7e, 0x48, 1, 0xf7];
    let intent = journal
        .checkpoint(0, "bounded exact query fixture".into(), wire.clone())
        .unwrap();
    let e = event(&journal);
    assert_eq!(e.intent, intent);
    let JournalReply::Checkpoint(checkpoint) = e.result.unwrap() else {
        panic!("missing checkpoint")
    };
    assert_eq!(checkpoint.wire(), wire);
    assert_eq!(checkpoint.ordinal(), 0);
    assert!(checkpoint.sequence() > 0);
    assert_eq!(checkpoint.prefix().len(), 64);
    {
        let _fence = journal.offer_fence(checkpoint, 0, &wire).unwrap();
    }
    let intent = journal
        .record(serde_json::json!({"kind":"negative-send","extent":"unknown","no_retry":true}))
        .unwrap();
    let e = event(&journal);
    assert_eq!(e.intent, intent);
    let JournalReply::Recorded(receipt) = e.result.unwrap() else {
        panic!("wrong result")
    };
    assert_eq!(receipt.sequence, 3);
    assert_eq!(
        receipt.bytes,
        std::fs::metadata(&receipt.path).unwrap().len()
    );
    let returned = journal.join().unwrap();
    assert_eq!(returned.root(), root);
    assert!(LibraryStore::open(&root).is_err());
    drop(returned);
    assert!(LibraryStore::open(&root).is_ok());
}
#[test]
fn checkpoint_from_other_worker_and_stop_revocation_cannot_offer() {
    let a = Temp::new();
    let b = Temp::new();
    let mut ja = CompletionJournal::start(
        LibraryStore::open(a.0.join("store")).unwrap(),
        "22222222222222222222222222222222".into(),
        serde_json::json!({"fixture":true}),
    )
    .unwrap_or_else(|f| panic!("{}", f.error));
    let mut jb = CompletionJournal::start(
        LibraryStore::open(b.0.join("store")).unwrap(),
        "33333333333333333333333333333333".into(),
        serde_json::json!({"fixture":true}),
    )
    .unwrap_or_else(|f| panic!("{}", f.error));
    let wire = vec![0xf0, 0x7e, 0x48, 1, 0xf7];
    ja.checkpoint(0, "query".into(), wire.clone()).unwrap();
    let JournalReply::Checkpoint(c) = event(&ja).result.unwrap() else {
        panic!("checkpoint")
    };
    assert!(jb.offer_fence(c, 0, &wire).is_err());
    jb.checkpoint(0, "query".into(), wire.clone()).unwrap();
    let JournalReply::Checkpoint(c) = event(&jb).result.unwrap() else {
        panic!("checkpoint")
    };
    jb.stop();
    assert!(jb.offer_fence(c, 0, &wire).is_err());
    let _ = ja.join().unwrap();
    let _ = jb.join().unwrap();
}
#[test]
fn changed_journal_prefix_fails_and_cannot_mint_checkpoint() {
    let temp = Temp::new();
    let mut journal = CompletionJournal::start(
        LibraryStore::open(temp.0.join("store")).unwrap(),
        "44444444444444444444444444444444".into(),
        serde_json::json!({"fixture":true}),
    )
    .unwrap_or_else(|f| panic!("{}", f.error));
    std::fs::OpenOptions::new()
        .write(true)
        .open(journal.path())
        .unwrap()
        .set_len(1)
        .unwrap();
    journal
        .checkpoint(0, "query".into(), vec![0xf0, 0x7e, 0x48, 1, 0xf7])
        .unwrap();
    assert!(event(&journal).result.is_err());
    let _ = journal.join().unwrap();
}
#[test]
fn symlinked_or_public_run_directory_is_denied() {
    let temp = Temp::new();
    let alias = temp.0.join("alias");
    std::os::unix::fs::symlink(&temp.0, &alias).unwrap();
    assert!(private_directory(&alias).is_err());
    std::fs::set_permissions(&temp.0, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(private_directory(&temp.0).is_err());
}
#[test]
fn framer_retains_original_start_time_and_all_chunk_positions() {
    let mut f = CompletionFramer::new();
    assert!(f.feed(&raw(9, 120, &[0xf0, 0x7e])).unwrap().is_empty());
    assert!(f.pending());
    let frames = f
        .feed(&raw(10, 190, &[0x48, 1, 0xf7, 0xf0, 0x7e, 0x48, 6, 0xf7]))
        .unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(
        (frames[0].first_chunk, frames[0].last_chunk, frames[0].at_ns),
        (9, 10, 120)
    );
    assert_eq!(frames[0].sequence, 1);
    assert_eq!(frames[1].sequence, 2);
    assert!(!f.pending());
}
#[test]
fn malformed_nested_unframed_and_oversized_raw_cannot_become_status() {
    let mut f = CompletionFramer::new();
    assert!(f.feed(&raw(1, 1, &[0x7f])).is_err());
    let mut f = CompletionFramer::new();
    assert!(f.feed(&raw(1, 1, &[0xf0, 0xf0])).is_err());
    let mut f = CompletionFramer::new();
    let mut bytes = vec![0; 4097];
    bytes[0] = 0xf0;
    assert!(f.feed(&raw(1, 1, &bytes)).is_err());
}
fn endpoint(
    binding: FirmwareUsbObservation,
) -> crate::firmware_linux_completion_binding::CompletionEndpointObservation {
    crate::firmware_linux_completion_binding::CompletionEndpointObservation {
        selection: DeviceSelection {
            input_id: format!(
                "alsa-raw:in:{}:0:0",
                binding
                    .endpoint
                    .strip_prefix("hw:")
                    .unwrap()
                    .split(',')
                    .next()
                    .unwrap()
            ),
            output_id: format!(
                "alsa-raw:out:{}:0:0",
                binding
                    .endpoint
                    .strip_prefix("hw:")
                    .unwrap()
                    .split(',')
                    .next()
                    .unwrap()
            ),
            fingerprint_hint: None,
        },
        binding,
    }
}
#[test]
fn same_enumeration_associates_fresh_reader_but_never_implies_boot_version() {
    let f = crate::firmware_entry_enumeration::tests::Fixture::new();
    let observation = crate::firmware_linux_completion_binding::observe_at(
        &f.binding,
        2,
        &f.root.join("usb"),
        &f.root.join("sound"),
        vec![endpoint(f.binding.clone())],
    )
    .unwrap();
    assert_eq!(observation.binding, f.binding);
    assert!(observation.relation.contains("same enumeration"));
    assert!(observation.relation.contains("unobserved"));
    assert_eq!(observation.observed_generation, 2);
}
#[test]
fn changed_devnum_and_logical_card_alias_are_fresh_observations() {
    let f = crate::firmware_entry_enumeration::tests::Fixture::new();
    let before = crate::firmware_linux_completion_binding::observe_at(
        &f.binding,
        2,
        &f.root.join("usb"),
        &f.root.join("sound"),
        vec![endpoint(f.binding.clone())],
    )
    .unwrap();
    std::fs::write(PathBuf::from(&f.binding.topology).join("devnum"), "7").unwrap();
    std::fs::write(f.root.join("sound/card3/id"), "GP200Boot").unwrap();
    let mut changed = f.binding.clone();
    changed.endpoint = "hw:GP200Boot,0,0".into();
    let after = crate::firmware_linux_completion_binding::observe_at(
        &f.binding,
        3,
        &f.root.join("usb"),
        &f.root.join("sound"),
        vec![endpoint(changed.clone())],
    )
    .unwrap();
    assert_eq!(after.binding, changed);
    assert_ne!(before.usb.owned, after.usb.owned);
    assert_ne!(before.selection, after.selection);
    assert!(after.relation.contains("unobserved"));
}
#[test]
fn ambiguous_aliases_and_changed_usb_identity_are_denied() {
    let f = crate::firmware_entry_enumeration::tests::Fixture::new();
    assert!(
        crate::firmware_linux_completion_binding::observe_at(
            &f.binding,
            2,
            &f.root.join("usb"),
            &f.root.join("sound"),
            vec![endpoint(f.binding.clone()), endpoint(f.binding.clone())]
        )
        .is_err()
    );
    std::fs::write(
        PathBuf::from(&f.binding.topology).join("serial"),
        "different",
    )
    .unwrap();
    assert!(
        crate::firmware_linux_completion_binding::observe_at(
            &f.binding,
            2,
            &f.root.join("usb"),
            &f.root.join("sound"),
            vec![endpoint(f.binding.clone())]
        )
        .is_err()
    );
}

#[test]
fn original_native_drain_time_is_preserved_after_slow_durable_delivery() {
    let original = CompletionSyscall {
        operation: "drain".into(),
        requested: 0,
        returned_count: Some(0),
        errno: None,
        before_sequence: 4,
        after_sequence: Some(5),
        before_ns: 12_000_000,
        after_ns: Some(13_999_999),
        after_clock_error: None,
        diagnostic: None,
    };
    // A later host clock is deliberately not an input to the receipt conversion.
    let journal_delivery_at_ms = 50_000;
    assert_eq!(syscall_completion_ms(&original).unwrap(), 13);
    assert!(journal_delivery_at_ms > syscall_completion_ms(&original).unwrap());
    let mut missing = original;
    missing.after_ns = None;
    missing.after_clock_error = Some("original clock unavailable".into());
    assert!(syscall_completion_ms(&missing).is_err());
    assert_eq!(missing.returned_count, Some(0));
}
#[test]
fn only_single_native_disconnect_with_actual_passive_transition_is_accountable() {
    use crate::firmware_linux_completion::expected_transition_fault as check;
    let fault = CompletionReadFault {
        operation: "read".into(),
        errno: Some(19),
        sequence: 8,
        at_ns: 900,
        before_sequence: 7,
        after_sequence: 8,
        diagnostic: "device disappeared".into(),
    };
    assert!(check(Some(&fault), 7, 8, true, None, true));
    assert!(!check(Some(&fault), 7, 8, true, None, false));
    assert!(!check(Some(&fault), 7, 9, true, None, true));
    assert!(!check(Some(&fault), 7, 8, false, None, true));
    assert!(!check(Some(&fault), 6, 8, true, None, true));
    let malformed = ServiceError::invalid("Nested completion SysEx frame");
    assert!(!check(Some(&fault), 7, 8, true, Some(&malformed), true));
    let mut overrun = fault.clone();
    overrun.errno = None;
    overrun.operation = "publication".into();
    assert!(!check(Some(&overrun), 7, 8, true, None, true));
    let mut unsupported = fault;
    unsupported.errno = Some(11);
    assert!(!check(Some(&unsupported), 7, 8, true, None, true));
}
#[test]
fn same_length_external_journal_edit_invalidates_a_next_checkpoint() {
    use std::os::unix::fs::FileExt;
    let temp = Temp::new();
    let mut journal = CompletionJournal::start(
        LibraryStore::open(temp.0.join("store")).unwrap(),
        "55555555555555555555555555555555".into(),
        serde_json::json!({"fixture":true}),
    )
    .unwrap_or_else(|f| panic!("{}", f.error));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(journal.path())
        .unwrap();
    file.write_all_at(b"X", 0).unwrap();
    file.sync_all().unwrap();
    journal
        .checkpoint(0, "query".into(), vec![0xf0, 0x7e, 0x48, 1, 0xf7])
        .unwrap();
    assert!(event(&journal).result.is_err());
    let _ = journal.join().unwrap();
}

// Build-time private simulation plumbing: it cannot be supplied to a physical
// request, deserialized, or used by the ordinary hardware constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FixtureFault {
    None,
    EntryOpen,
    PartialEntry,
    UnknownEntry,
    DrainEntry,
    NormalRead,
    PrewriteExpiry,
    RawLoss,
    StopBeforeEntry,
    StopPendingCheckpoint,
    StopPendingRecord,
    MidTransfer,
    SecondDescriptorRejected,
    BlockStopAfterWrite,
    BlockZero,
    BlockShort,
    BlockUnknown,
    BlockEintr,
    BlockErrno,
    BlockDrainError,
    BlockBatchSubmit,
    BlockClock,
    BlockDeliveryClock,
    BlockEagain,
    BlockRepeatedEagain,
    BlockSplit,
    BlockDuringDrain,
    BlockMalformed,
    BlockStale,
    BlockPreoffer,
    BlockPublication,
    BlockBatchSync,
    BlockBatchReread,
    BlockWrongIntent,
    BlockRevoked,
    BlockRawDuringSync,
    BlockCloseQueuedJournalFailure,
    BlockStopBatch,
    BlockStopStall,
    BlockCapacity,
    BlockRejected,
    BlockStopBeforeFusionSubmit,
    BlockStopBeforeFusionCommit,
    BlockLostReply,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PhaseWaitBoundary {
    BeforeRead,
    AfterRevalidation,
    AfterJournal,
}
pub(super) struct PhaseWaitHook {
    boundary: PhaseWaitBoundary,
    arrived: std::sync::atomic::AtomicBool,
    release: std::sync::atomic::AtomicBool,
    deadline: std::sync::Mutex<Option<Instant>>,
}
impl PhaseWaitHook {
    pub(super) fn pause(&self, boundary: PhaseWaitBoundary, deadline: Instant) {
        if boundary != self.boundary || self.arrived.load(Ordering::SeqCst) {
            return;
        }
        *self.deadline.lock().unwrap() = Some(deadline);
        self.arrived.store(true, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst)
            && Instant::now() < deadline + Duration::from_secs(5)
        {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
#[derive(Default)]
pub(super) struct FixtureTrace {
    pub(super) drains: Vec<serde_json::Value>,
    pub(super) retirements: Vec<serde_json::Value>,
    pub(super) durable_eagain_prefixes: usize,
    pub(super) pair_retirements: usize,
    pub(super) journal_waits: Vec<(u64, Instant)>,
    pub(super) before_fusion_sync_accepted: Option<u64>,
}
#[derive(Clone)]
pub(super) struct FixturePlatform {
    pub(super) usb: PathBuf,
    pub(super) sound: PathBuf,
    pub(super) registry: PathBuf,
    pub(super) endpoints:
        Vec<crate::firmware_linux_completion_binding::CompletionEndpointObservation>,
    pub(super) fault: FixtureFault,
    pub(super) chunk: u8,
    pub(super) retained_identity: Option<Vec<u8>>,
    pub(super) phase_wait_hook: Option<std::sync::Arc<PhaseWaitHook>>,
    pub(super) writes: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    pub(super) trace: std::sync::Arc<std::sync::Mutex<FixtureTrace>>,
    pub(super) journal_root: PathBuf,
}
impl FixturePlatform {
    pub(super) fn baseline(
        &self,
        normal: &FirmwareUsbObservation,
    ) -> Result<crate::firmware_entry_enumeration::EnumerationReceipt, ServiceError> {
        crate::firmware_entry_enumeration::capture_at(&self.usb, &self.sound, normal)
    }
    pub(super) fn transition(
        &self,
        previous: &crate::firmware_entry_enumeration::EnumerationReceipt,
    ) -> Result<bool, ServiceError> {
        let (devices, errors) = crate::firmware_entry_enumeration::inventory_at(&self.usb)?;
        if !errors.is_empty() {
            return Err(ServiceError::invalid("Mock inventory errors"));
        }
        let matches = devices
            .iter()
            .filter(|device| device.location == previous.owned.location)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => Ok(true),
            [current] => Ok(**current != previous.owned),
            _ => Err(ServiceError::invalid("Mock ambiguous location")),
        }
    }
    pub(super) fn accept(
        &self,
        normal: &FirmwareUsbObservation,
        generation: u64,
        guard: &mut crate::device_session_guard::DeviceSessionGuard,
    ) -> Result<crate::firmware_linux_completion_binding::CompletionBindingWitness, ServiceError>
    {
        crate::firmware_linux_completion_binding::accept_fixture(
            normal,
            generation,
            guard,
            &self.usb,
            &self.sound,
            self.endpoints.clone(),
        )
    }
    pub(super) fn open(
        &self,
        sink: crate::firmware_backend::FirmwareReceiveSink,
        boot: bool,
    ) -> Result<Box<dyn CompletionIo>, crate::firmware_linux_completion_io::CompletionOpenFailure>
    {
        if !boot && self.fault == FixtureFault::EntryOpen {
            return Err(crate::firmware_linux_completion_io::CompletionOpenFailure {
                error: ServiceError::invalid("Mock native open failed before any invocation"),
                io: None,
            });
        }
        let io = Box::new(FixtureIo {
            sink,
            writes: self.writes.clone(),
            fault: if boot
                && !matches!(
                    self.fault,
                    FixtureFault::None
                        | FixtureFault::MidTransfer
                        | FixtureFault::SecondDescriptorRejected
                        | FixtureFault::BlockStopAfterWrite
                        | FixtureFault::BlockZero
                        | FixtureFault::BlockShort
                        | FixtureFault::BlockUnknown
                        | FixtureFault::BlockEintr
                        | FixtureFault::BlockErrno
                        | FixtureFault::BlockDrainError
                        | FixtureFault::BlockBatchSubmit
                        | FixtureFault::BlockClock
                        | FixtureFault::BlockDeliveryClock
                        | FixtureFault::BlockEagain
                        | FixtureFault::BlockRepeatedEagain
                        | FixtureFault::BlockSplit
                        | FixtureFault::BlockDuringDrain
                        | FixtureFault::BlockMalformed
                        | FixtureFault::BlockStale
                        | FixtureFault::BlockPreoffer
                        | FixtureFault::BlockPublication
                        | FixtureFault::BlockBatchSync
                        | FixtureFault::BlockBatchReread
                        | FixtureFault::BlockWrongIntent
                        | FixtureFault::BlockRevoked
                        | FixtureFault::BlockRawDuringSync
                        | FixtureFault::BlockCloseQueuedJournalFailure
                        | FixtureFault::BlockStopBatch
                        | FixtureFault::BlockStopStall
                        | FixtureFault::BlockCapacity
                        | FixtureFault::BlockRejected
                        | FixtureFault::BlockStopBeforeFusionSubmit
                        | FixtureFault::BlockStopBeforeFusionCommit
                        | FixtureFault::BlockLostReply
                ) {
                FixtureFault::None
            } else {
                self.fault
            },
            chunk: self.chunk,
            retained_identity: self.retained_identity.clone(),
            boot,
            retired: None,
            descriptor_offers: 0,
            trace: self.trace.clone(),
            journal_root: self.journal_root.clone(),
            last_wire: vec![],
            drain_attempt: 0,
            capacity_final_pending: false,
        });
        if !boot && self.fault == FixtureFault::StopPendingRecord {
            return Err(crate::firmware_linux_completion_io::CompletionOpenFailure {
                error: ServiceError::invalid("Mock output open failed with actual reader retained"),
                io: Some(io),
            });
        }
        Ok(io)
    }
    pub(super) fn normal_reads(
        &self,
        witness: &crate::firmware_linux_completion_binding::CompletionBindingWitness,
        epoch: SessionEpoch,
        stop: &std::sync::atomic::AtomicBool,
    ) -> Result<super::CompletionNormalProof, ServiceError> {
        let source = crate::backend::NormalCallbackSource::new(Instant::now(), epoch);
        let overflow = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, received) = crossbeam_channel::bounded(64);
        let sink = crate::ReceiveSink::observed(tx, overflow.clone(), epoch, source.clone());
        let session = Box::new(FixtureNormalSession {
            sink,
            closed: None,
            binding: witness.observation().binding.clone(),
            fail: self.fault == FixtureFault::NormalRead,
        });
        super::read_normal_owned(
            super::CompletionNormalReadInput {
                session,
                source,
                received,
                overflow,
                epoch,
                selection: witness.observation().selection.clone(),
                binding: witness.observation().binding.clone(),
                mode: BackendMode::Simulator,
                config: crate::TransportConfig::default(),
            },
            stop,
        )
    }
}
struct FixtureIo {
    chunk: u8,
    retained_identity: Option<Vec<u8>>,
    sink: crate::firmware_backend::FirmwareReceiveSink,
    writes: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    fault: FixtureFault,
    boot: bool,
    retired: Option<crate::firmware_backend::FirmwareReaderRetirement>,
    descriptor_offers: u8,
    trace: std::sync::Arc<std::sync::Mutex<FixtureTrace>>,
    journal_root: PathBuf,
    last_wire: Vec<u8>,
    drain_attempt: usize,
    capacity_final_pending: bool,
}
impl FixtureIo {
    fn call(
        &self,
        operation: &str,
        requested: usize,
        before: (u64, u64),
        returned: Option<usize>,
        errno: Option<i32>,
    ) -> CompletionSyscall {
        let after = self.sink.now().unwrap();
        CompletionSyscall {
            operation: operation.into(),
            requested,
            returned_count: returned,
            errno,
            before_sequence: before.0,
            after_sequence: Some(after.0),
            before_ns: before.1,
            after_ns: Some(after.1),
            after_clock_error: None,
            diagnostic: errno.map(|_| "Mock original syscall errno".into()),
        }
    }
}
impl CompletionIo for FixtureIo {
    fn write_once(
        &mut self,
        wire: &[u8],
        invocation: &crate::firmware_linux_completion_io::CompletionInvocation<'_>,
        on_invoked: &mut dyn FnMut() -> Result<(), ServiceError>,
    ) -> Result<CompletionSyscall, ServiceError> {
        if self.fault == FixtureFault::PrewriteExpiry {
            return Err(ServiceError::invalid(
                "Mock deadline expired after checkpoint before native invocation",
            ));
        }
        if self.fault == FixtureFault::StopBeforeEntry {
            invocation.stop.store(true, Ordering::SeqCst);
        }
        if self.boot && wire[3] == 4 && self.fault == FixtureFault::BlockPreoffer {
            self.sink.receive(&[240, 126, 72, 3, 127, 247]);
        }
        let before = invocation.admit(&self.sink)?;
        on_invoked()?;
        self.last_wire = wire.to_vec();
        self.drain_attempt = 0;
        self.writes.lock().unwrap().push(wire.to_vec());
        if self.fault == FixtureFault::PartialEntry {
            return Ok(self.call("write", wire.len(), before, Some(wire.len() / 2), None));
        }
        if self.fault == FixtureFault::UnknownEntry {
            return Ok(self.call("write", wire.len(), before, None, Some(5)));
        }
        if self.boot
            && self.fault == FixtureFault::MidTransfer
            && self.writes.lock().unwrap().len() > 300
        {
            return Ok(self.call("write", wire.len(), before, None, Some(5)));
        }
        if self.boot && wire[3] == 4 {
            let bad_extent = match self.fault {
                FixtureFault::BlockZero => Some((Some(0), None)),
                FixtureFault::BlockShort => Some((Some(wire.len() / 2), None)),
                FixtureFault::BlockUnknown => Some((None, Some(5))),
                FixtureFault::BlockEintr => Some((None, Some(4))),
                FixtureFault::BlockErrno => Some((Some(wire.len()), Some(5))),
                _ => None,
            };
            if let Some((count, errno)) = bad_extent {
                return Ok(self.call("write", wire.len(), before, count, errno));
            }
            if self.fault == FixtureFault::BlockClock {
                let mut call = self.call("write", wire.len(), before, Some(wire.len()), None);
                call.after_ns = None;
                call.after_clock_error = Some("Mock original postcall clock failed".into());
                return Ok(call);
            }
            if self.fault == FixtureFault::BlockStopAfterWrite {
                invocation.stop.store(true, Ordering::SeqCst);
            }
        }
        if self.boot {
            let mut response = match wire[3] {
                1 => Some(self.retained_identity.clone().unwrap_or_else(|| {
                    let mut raw = vec![0xf0, 0x7e, 0x48, 1];
                    let mut product = [0; 16];
                    product[..6].copy_from_slice(b"GP-200");
                    raw.extend(product);
                    raw.extend([b'B'; 16]);
                    raw.extend([self.chunk >> 4, self.chunk & 15, 0xf7]);
                    raw
                })),
                0x0b | 9 => None,
                7 => Some(vec![
                    0xf0, 0x7e, 0x48, 3, 10, 11, 0xf7, 0xf0, 0x7e, 0x48, 7, 12, 13, 0xf7,
                ]),
                _ => Some(vec![0xf0, 0x7e, 0x48, 3, 0x7f, 0xf7]),
            };
            if self.fault == FixtureFault::SecondDescriptorRejected && wire[3] == 2 {
                self.descriptor_offers += 1;
                if self.descriptor_offers == 2 {
                    assert_eq!(wire, NATIVE_SECOND_DESCRIPTOR_180);
                    response = Some(RETAINED_DESCRIPTOR_REJECTION.to_vec());
                }
            }
            if wire[3] == 4 {
                match self.fault {
                    FixtureFault::BlockDuringDrain => response = None,
                    FixtureFault::BlockRejected => {
                        response = Some(RETAINED_DESCRIPTOR_REJECTION.to_vec())
                    }
                    FixtureFault::BlockMalformed => response = Some(vec![240, 240]),
                    FixtureFault::BlockSplit => {
                        let raw = response.take().unwrap();
                        self.sink.receive(&raw[..3]);
                        self.sink.receive(&raw[3..]);
                    }
                    FixtureFault::BlockCapacity => {
                        response = None;
                        self.sink.receive(&[240]);
                        for _ in 0..63 {
                            self.sink.receive(&[0]);
                        }
                        self.capacity_final_pending = true;
                    }
                    FixtureFault::BlockPublication => {
                        response = None;
                        for _ in 0..66 {
                            self.sink.receive(&[240, 247]);
                        }
                    }
                    _ => {}
                }
            }
            if let Some(raw) = response {
                self.sink.receive(&raw);
            }
        }
        if self.fault == FixtureFault::RawLoss {
            for _ in 0..66 {
                self.sink.receive(&[0xf0, 0xf7]);
            }
        }
        let call = self.call("write", wire.len(), before, Some(wire.len()), None);
        if self.boot && wire[3] == 4 && self.fault == FixtureFault::BlockDeliveryClock {
            self.sink.clock_failed.store(true, Ordering::SeqCst);
        }
        Ok(call)
    }
    fn drain_once(&mut self) -> Result<CompletionSyscall, ServiceError> {
        let before = self.sink.now()?;
        self.drain_attempt += 1;
        let block = self.boot && self.last_wire.get(3) == Some(&4);
        self.trace
            .lock()
            .unwrap()
            .drains
            .push(serde_json::json!({"block":block,
            "attempt":self.drain_attempt,"at_ns":before.1}));
        if block
            && matches!(
                self.fault,
                FixtureFault::BlockEagain | FixtureFault::BlockRepeatedEagain
            )
        {
            if self.drain_attempt > 1 {
                let path = std::fs::read_dir(&self.journal_root)
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path();
                let records = authenticated_records(&path);
                let write = records
                    .iter()
                    .rev()
                    .find(|record| record["kind"] == "write-syscall")
                    .unwrap();
                let drain = records.last().unwrap();
                assert_eq!(write["observation"]["requested"], self.last_wire.len());
                assert_eq!(drain["kind"], "drain-syscall");
                assert_eq!(drain["observation"]["errno"], 11);
                assert_eq!(drain["ordinal"], write["ordinal"]);
                self.trace.lock().unwrap().durable_eagain_prefixes += 1;
            }
            if self.drain_attempt
                <= if self.fault == FixtureFault::BlockEagain {
                    1
                } else {
                    3
                }
            {
                return Ok(self.call("drain", 0, before, None, Some(11)));
            }
        }
        if block && self.fault == FixtureFault::BlockDrainError {
            return Ok(self.call("drain", 0, before, None, Some(5)));
        }
        if block && self.fault == FixtureFault::BlockDuringDrain {
            self.sink.receive(&[240, 126, 72, 3, 127, 247]);
        }
        Ok(self.call(
            "drain",
            0,
            before,
            if self.fault == FixtureFault::DrainEntry {
                None
            } else {
                Some(0)
            },
            if self.fault == FixtureFault::DrainEntry {
                Some(5)
            } else {
                None
            },
        ))
    }
    fn healthy(&mut self) -> Result<(), ServiceError> {
        if self.capacity_final_pending && self.drain_attempt > 0 {
            self.capacity_final_pending = false;
            self.sink.receive(&[247]);
        }
        if self.sink.fault.load(Ordering::SeqCst) {
            Err(ServiceError::invalid("Mock original reader loss"))
        } else {
            Ok(())
        }
    }
    fn read_fault(&self) -> Option<CompletionReadFault> {
        None
    }
    fn close(&mut self) -> crate::firmware_backend::FirmwareReaderRetirement {
        if let Some(retired) = &self.retired {
            return retired.repeat();
        }
        let receipt = self.sink.synthetic_close();
        self.trace
            .lock()
            .unwrap()
            .retirements
            .push(serde_json::json!({"boot":self.boot,
            "source":receipt.observation(),"output_destroyed":receipt.output_destroyed}));
        self.retired = Some(receipt.repeat());
        receipt
    }
}
struct FixtureNormalSession {
    sink: crate::ReceiveSink,
    closed: Option<crate::backend::NormalReaderCloseReceipt>,
    binding: FirmwareUsbObservation,
    fail: bool,
}
impl crate::MidiSession for FixtureNormalSession {
    fn send(&mut self, bytes: &[u8]) -> Result<(), ServiceError> {
        use gp200_protocol::*;
        if self.fail {
            return Err(ServiceError::invalid(
                "Mock actual actor normal-read send failed",
            ));
        }
        let packet = decode_packet(bytes).unwrap();
        let query = RawTransfer::from_payload(packet.kind, packet.payload).unwrap();
        let response = if query.records().iter().any(|r| r.command == 0x2001) {
            RawTransfer::from_records(
                WRITE_KIND,
                &[RawRecord {
                    command: 0x2001,
                    body: vec![1, 0, 2, 0],
                }],
            )
            .unwrap()
        } else if query.records().iter().any(|r| r.command == 0x1006) {
            RawTransfer::from_payload(
                WRITE_KIND,
                include_bytes!("../../../tests/fixtures/globals/baseline.payload.bin").to_vec(),
            )
            .unwrap()
        } else {
            let mut decoder = StreamDecoder::new();
            let mut assembler = Reassembler::default();
            let mut result = None;
            for event in decoder.feed(include_bytes!(
                "../../gp200-protocol/tests/fixtures/current-baseline.bin"
            )) {
                if let StreamEvent::Packet(packet) = event
                    && let Some(raw) = assembler.add(&packet, Duration::ZERO).unwrap()
                {
                    result = Some(raw);
                }
            }
            result.unwrap()
        };
        for packet in encode_transfer(&response).unwrap() {
            self.sink.receive(&packet);
        }
        Ok(())
    }
    fn close_normal_capture(&mut self) -> Result<(), ServiceError> {
        let started = self.sink.source.now_ns();
        self.closed = Some(crate::backend::NormalReaderCloseReceipt {
            source: self.sink.source.clone(),
            epoch: self.sink.epoch,
            started,
            finished: self.sink.source.now_ns(),
            allocated: self.sink.source.fence(0).allocated,
            join: FirmwareNormalReaderJoinObservation::Returned,
            fault: None,
            overflow: self.sink.overflow.load(Ordering::SeqCst),
            output_destroyed: true,
        });
        Ok(())
    }
    fn take_firmware_handoff(&mut self) -> Result<crate::FirmwareNormalHandoff, ServiceError> {
        if self.closed.is_none() {
            self.close_normal_capture()?;
        }
        Ok(crate::FirmwareNormalHandoff {
            binding: self.binding.clone(),
            normal_epoch: self.sink.epoch,
            normal_close: self.closed.take(),
            retirement: None,
            transition: None,
            factory: None,
            guard: None,
            mock: None,
        })
    }
}

struct OwnerFixture {
    _temp: Temp,
    _enumeration: crate::firmware_entry_enumeration::tests::Fixture,
    run: PathBuf,
    store_root: PathBuf,
    platform: FixturePlatform,
    owner: super::LinuxCompletionOwner,
    stop: std::sync::Arc<crate::firmware_service::OwnerStop>,
}
fn pinned(path: PathBuf) -> crate::firmware_entry_study::EntryStudyFilePin {
    let bytes = std::fs::read(&path).unwrap();
    crate::firmware_entry_study::EntryStudyFilePin {
        path,
        bytes: bytes.len() as u64,
        sha256: JobSha256::of(&bytes).hex(),
    }
}
fn owner_fixture(fault: FixtureFault, journal_start_fails: bool) -> OwnerFixture {
    owner_fixture_with_passive_boot(fault, journal_start_fails, false)
}
fn owner_fixture_with_passive_boot(
    fault: FixtureFault,
    journal_start_fails: bool,
    passive_boot: bool,
) -> OwnerFixture {
    owner_fixture_with_receiver(fault, journal_start_fails, passive_boot, 255, None, 300)
}
fn owner_fixture_with_receiver(
    fault: FixtureFault,
    journal_start_fails: bool,
    passive_boot: bool,
    chunk: u8,
    retained_identity: Option<Vec<u8>>,
    transfer_seconds: u64,
) -> OwnerFixture {
    owner_fixture_with_receiver_profile(
        fault,
        journal_start_fails,
        passive_boot,
        chunk,
        retained_identity,
        transfer_seconds,
        None,
    )
}
fn owner_fixture_with_receiver_profile(
    fault: FixtureFault,
    journal_start_fails: bool,
    passive_boot: bool,
    chunk: u8,
    retained_identity: Option<Vec<u8>>,
    transfer_seconds: u64,
    profile: Option<&crate::firmware_linux_completion_journal::test_profile::Profile>,
) -> OwnerFixture {
    use crate::firmware_linux_completion_admission::{LinuxCompletionDraft, LinuxCompletionInputs};
    use std::sync::Arc;
    let temp = Temp::new();
    let enumeration = crate::firmware_entry_enumeration::tests::Fixture::new();
    let registry_path = temp.0.join("registry");
    std::fs::create_dir(&registry_path).unwrap();
    std::fs::set_permissions(&registry_path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let guard_root = temp.0.join("runtime");
    std::fs::create_dir(&guard_root).unwrap();
    std::fs::set_permissions(&guard_root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let registry = crate::firmware_quarantine::QuarantineRegistry::mock(&registry_path).unwrap();
    let alias = crate::device_session_guard::EndpointIdentity {
        location: enumeration.binding.interface.clone(),
        card_id: enumeration
            .binding
            .endpoint
            .trim_start_matches("hw:")
            .split(',')
            .next()
            .unwrap()
            .into(),
        device: 0,
        subdevice: 0,
    };
    let guard = crate::device_session_guard::DeviceSessionGuard::fixture_physical(
        &guard_root,
        &[alias],
        &enumeration.binding,
        &registry,
    )
    .unwrap();
    let epoch = SessionEpoch(7);
    let source = crate::backend::NormalCallbackSource::new(Instant::now(), epoch);
    let (_tx, received) = crossbeam_channel::bounded(4);
    let before = source.fence(0);
    let receipt = crate::backend::NormalReaderCloseReceipt {
        source: source.clone(),
        epoch,
        started: source.now_ns(),
        finished: source.now_ns(),
        allocated: 0,
        join: FirmwareNormalReaderJoinObservation::Returned,
        fault: None,
        overflow: false,
        output_destroyed: true,
    };
    let (_, retirement) = crate::firmware_transition::normal_retirement(
        receipt,
        &source,
        epoch,
        enumeration.binding.clone(),
        before,
        &received,
        false,
    );
    let handoff = crate::FirmwareNormalHandoff {
        binding: enumeration.binding.clone(),
        normal_epoch: epoch,
        normal_close: None,
        retirement: Some(retirement.unwrap()),
        transition: None,
        factory: None,
        guard: Some(guard),
        mock: None,
    };
    let reservation = FirmwareCaptureReservation {
        actor_run: "MockSimulation actual owner fixture".into(),
        reservation: "mock captured reservation".into(),
        normal_owner: "mock original retired owner".into(),
        epoch,
        usb: enumeration.binding.clone(),
        clock_origin: source.clock_origin().into(),
        prepared_monotonic_ns: 0,
        prepared_wall_millis: 0,
        initial_fence: before,
    };
    let path = std::env::var_os("GP200_VENDOR_SEQUENCE_IMAGE")
        .expect("explicit pinned local image is required for this ignored actual-owner replay");
    let image = std::fs::read(path).unwrap();
    assert_eq!(image.len(), 6_452_728);
    assert_eq!(
        JobSha256::of(&image).hex(),
        crate::firmware_vendor_sequence::VENDOR_IMAGE_SHA256
    );
    let source_image = RetainedJobBlob::checked(
        AssetId("mock exact image".into()),
        Arc::from(image),
        JobSha256::parse(crate::firmware_vendor_sequence::VENDOR_IMAGE_SHA256).unwrap(),
    )
    .unwrap();
    let context = FirmwareUpdateContextDto {
        profile_id: "offline mock actual owner".into(),
        hardware_policy: "No physical opener; private build-time fixture only".into(),
        update_mode: "MockSimulation".into(),
        normal: None,
        versions: vec![],
        backup_objects: vec![],
        loss_items: vec!["Installed IR audio remains unavailable".into()],
        power_statement: "Mock only".into(),
        connection_statement: "No USB opened".into(),
    };
    let materials = [
        FirmwareMaterialKind::Profile,
        FirmwareMaterialKind::Evidence,
        FirmwareMaterialKind::BackupArchive,
        FirmwareMaterialKind::BackupInventory,
        FirmwareMaterialKind::RestorePlan,
        FirmwareMaterialKind::Recovery,
        FirmwareMaterialKind::ApprovalStatement,
    ]
    .into_iter()
    .map(|kind| FirmwareMaterialInput {
        kind,
        bytes: Arc::from(&b"MockSimulation descriptive retained material"[..]),
    })
    .collect();
    let job =
        Arc::new(ReviewedFirmwareUpdate::prepare(source_image, context, materials, 128).unwrap());
    let run = temp.0.join("run");
    std::fs::create_dir(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut material_pins = Vec::new();
    for name in [
        "source",
        "loss",
        "recovery",
        "vendor-guidance",
        "archive",
        "sidecar",
        "index",
    ] {
        let p = temp.0.join(name);
        std::fs::write(&p, format!("MockSimulation {name}")).unwrap();
        material_pins.push(pinned(p));
    }
    let inputs = LinuxCompletionInputs {
        sources: vec![material_pins[0].clone()],
        loss: material_pins[1].clone(),
        recovery: material_pins[2].clone(),
        vendor_guidance: material_pins[3].clone(),
        contract: pinned(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
            "../../analysis/implementation/firmware-corrected-contract-binding/sequential-contract.json",
        )),
        run_directory: run.clone(),
        approval_wait_seconds: 30,
        transition_seconds: 5,
        transfer_seconds,
    };
    let draft = LinuxCompletionDraft::prepare(
        job,
        reservation,
        "mock actual capture acquisition".into(),
        material_pins[6].sha256.clone(),
        serde_json::json!({"origin":"MockSimulation","physical_hardware":false}),
        material_pins[4..].to_vec(),
        inputs,
    )
    .unwrap();
    let signal = format!("execute-entry:{}", draft.digest());
    let authorization = draft.approve_exact(&signal).unwrap();
    let store_root = temp.0.join("store");
    let store = LibraryStore::open(&store_root).unwrap();
    if journal_start_fails {
        std::fs::write(
            store.root().join("linux-firmware-completion"),
            "deliberate pre-invocation journal open failure",
        )
        .unwrap();
    }
    let mut candidate = enumeration.binding.clone();
    if passive_boot {
        candidate.product = 0x002b;
        candidate.serial = "V1.0.0".into();
        let node = std::path::Path::new(&candidate.topology);
        for (field, value) in [
            ("idProduct", "002b"),
            ("serial", "V1.0.0"),
            ("manufacturer", "Valeton"),
            ("product", "GP-200"),
            ("bcdDevice", "0100"),
            ("bNumInterfaces", "1"),
            ("devnum", "5"),
        ] {
            std::fs::write(node.join(field), value).unwrap();
        }
        // Independently retained first-entry descriptor blob; no USB is opened.
        let hex = "1201000200000040ef842b0000010102030109025300010104802009040000020103000507240100014100062402010100062402020200092403010901020100092403020a0101010009058302000200000005250101090905030200020000000525010101";
        let descriptors = hex
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect::<Vec<_>>();
        std::fs::write(node.join("descriptors"), descriptors).unwrap();
        let interface = std::path::Path::new(&candidate.interface);
        std::fs::remove_dir_all(interface.join("ep_81")).unwrap();
        for (field, value) in [("bInterfaceNumber", "00"), ("bNumEndpoints", "02")] {
            std::fs::write(interface.join(field), value).unwrap();
        }
        for endpoint in ["83", "03"] {
            let path = interface.join(format!("ep_{endpoint}"));
            std::fs::create_dir(&path).unwrap();
            for (field, value) in [
                ("bEndpointAddress", endpoint),
                ("bmAttributes", "02"),
                ("wMaxPacketSize", "0200"),
                ("bInterval", "0"),
            ] {
                std::fs::write(path.join(field), value).unwrap();
            }
        }
        // Establish that this is a valid passive boot candidate before exercising
        // either actual zero-invocation owner return, rather than malformed sysfs.
        crate::firmware_linux_completion_binding::observe_at(
            &enumeration.binding,
            1,
            &enumeration.root.join("usb"),
            &enumeration.root.join("sound"),
            vec![endpoint(candidate.clone())],
        )
        .unwrap();
    }
    let platform = FixturePlatform {
        usb: enumeration.root.join("usb"),
        sound: enumeration.root.join("sound"),
        registry: registry_path,
        endpoints: vec![endpoint(candidate)],
        fault,
        chunk,
        retained_identity,
        phase_wait_hook: None,
        writes: Arc::new(std::sync::Mutex::new(vec![])),
        trace: Arc::new(std::sync::Mutex::new(FixtureTrace::default())),
        journal_root: store_root.join("linux-firmware-completion"),
    };
    let stop = Arc::new(crate::firmware_service::OwnerStop::default());
    if let Some(profile) = profile {
        profile.register_roots(&store_root, &platform.usb);
    }
    let owner = super::LinuxCompletionOwner::start_fixture(
        authorization,
        store,
        handoff,
        stop.clone(),
        platform.clone(),
    )
    .unwrap_or_else(|f| panic!("{}", f.error));
    OwnerFixture {
        _temp: temp,
        _enumeration: enumeration,
        run,
        store_root,
        platform,
        owner,
        stop,
    }
}
fn write_exact_phase(path: &std::path::Path, text: &str) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(text.as_bytes()).unwrap();
    file.sync_all().unwrap();
    std::fs::File::open(path.parent().unwrap())
        .unwrap()
        .sync_all()
        .unwrap();
}
fn finish_owner(
    fixture: OwnerFixture,
    wrong_manual: bool,
) -> (
    Temp,
    PathBuf,
    FixturePlatform,
    LibraryStore,
    super::LinuxCompletionOutcome,
) {
    finish_owner_with_deadline(fixture, wrong_manual, Duration::from_secs(360), None)
}
fn finish_owner_with_deadline(
    fixture: OwnerFixture,
    wrong_manual: bool,
    budget: Duration,
    expected_transfer: Option<(u16, u64, u64)>,
) -> (
    Temp,
    PathBuf,
    FixturePlatform,
    LibraryStore,
    super::LinuxCompletionOutcome,
) {
    use super::LinuxCompletionPhase;
    let deadline = Instant::now() + budget;
    while !fixture.owner.stopped() {
        assert!(
            LibraryStore::open(&fixture.store_root).is_err(),
            "actual original writer lock must stay owned"
        );
        let view = fixture.owner.view();
        // The phase precedes durable review publication and can outlive the
        // review after its grant is consumed. Only the complete published
        // review and digest make this snapshot ready for approval.
        let published_matches = |name: &str, review: &serde_json::Value, digest: &str| {
            let bytes = std::fs::read(fixture.run.join(format!("{name}-review.json"))).unwrap();
            assert_eq!(JobSha256::of(&bytes).hex(), digest);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                *review
            );
            assert_eq!(review["run"].as_str(), Some(view.run.as_str()));
        };
        let signal = match (
            view.phase,
            view.review.as_ref(),
            view.review_digest.as_deref(),
        ) {
            (LinuxCompletionPhase::ReviewBootQuery, Some(review), Some(hash)) => {
                published_matches("boot-query", review, hash);
                Some(("approve-query", format!("execute-query:{hash}")))
            }
            (LinuxCompletionPhase::ReviewTransfer, Some(review), Some(hash)) => {
                published_matches("transfer", review, hash);
                if let Some((chunk, transfer_seconds, blocks)) = expected_transfer
                    && !fixture.run.join("approve-transfer").exists()
                {
                    assert_eq!(review["actual_chunk"], chunk);
                    assert_eq!(review["transfer_seconds"], transfer_seconds);
                    assert_eq!(review["approval_wait_seconds"], 30);
                    assert_eq!(review["budget"]["blocks"], blocks);
                    assert_eq!(review["offers_each"], 1);
                    assert_eq!(review["automatic_retry"], false);
                    assert!(!fixture.run.join("approve-transfer").exists());
                }
                Some(("approve-transfer", format!("execute-transfer:{hash}")))
            }
            (LinuxCompletionPhase::ConfirmApplicationVersion, Some(review), Some(hash)) => {
                published_matches("application-version", review, hash);
                Some((
                    "confirm-application-version",
                    format!(
                        "confirm-application:{hash}:{}",
                        if wrong_manual { "V1.6.3" } else { "V1.8.2" }
                    ),
                ))
            }
            _ => None,
        };
        if let Some((name, text)) = signal {
            let path = fixture.run.join(name);
            if !path.exists() {
                write_exact_phase(&path, &text);
            }
        }
        if Instant::now() >= deadline {
            fixture.stop.requested.store(true, Ordering::SeqCst);
            panic!("bounded actual owner fixture failed to settle");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let (_, store, outcome) = fixture.owner.retire().unwrap();
    assert_eq!(store.root(), fixture.store_root);
    assert!(LibraryStore::open(&fixture.store_root).is_err());
    (fixture._temp, fixture.run, fixture.platform, store, outcome)
}
#[test]
#[ignore = "requires separately pinned image via GP200_VENDOR_SEQUENCE_IMAGE; actual owner/journal mock I/O only"]
fn exact_image_actual_owner_returns_same_store_after_fresh_normal_and_manual_version() {
    let (_temp, _run, platform, store, outcome) =
        finish_owner(owner_fixture(FixtureFault::None, false), false);
    assert!(
        outcome.editor_return_allowed,
        "{:?}",
        outcome.view.diagnostic
    );
    assert!(
        outcome.view.host_finished_unverified
            && outcome.view.normal_mode_verified
            && outcome.view.target_version_verified
    );
    assert!(!outcome.view.quarantined && !outcome.view.contents_verified);
    assert_eq!(outcome.view.device_verified_bytes, 0);
    assert_eq!(outcome.view.host_accepted_blocks, 25_307);
    assert_eq!(outcome.application_version.as_deref(), Some("V1.8.2"));
    assert!(outcome.verified_normal_epoch.unwrap().0 > outcome.original_normal_epoch.0);
    assert_eq!(platform.writes.lock().unwrap().len(), 25_328);
    crate::firmware_quarantine::QuarantineRegistry::mock(&platform.registry)
        .unwrap()
        .admit_normal()
        .unwrap();
    drop(store);
}
#[test]
#[ignore = "requires pinned image and explicit private TMPDIR; bounded 180-byte actual-owner disk timing, MockSimulation I/O only"]
fn bounded_180_actual_owner_durable_journal_timing_probe() {
    use gp200_protocol::{FirmwareBlock, FirmwareFixedPacket};
    use std::{collections::BTreeMap, io::Read};

    fn statistics_ns(values: &[u64]) -> serde_json::Value {
        assert!(!values.is_empty());
        let mut sorted = values.to_vec();
        sorted.sort_unstable();
        let micros = |value: u64| value as f64 / 1_000.0;
        serde_json::json!({
            "count": sorted.len(),
            "min_us": micros(sorted[0]),
            "mean_us": sorted.iter().map(|value| *value as u128).sum::<u128>() as f64
                / sorted.len() as f64 / 1_000.0,
            "p50_us": micros(sorted[(sorted.len() - 1) / 2]),
            "p95_us": micros(sorted[(sorted.len() * 95).div_ceil(100) - 1]),
            "max_us": micros(*sorted.last().unwrap()),
        })
    }

    // The caller chooses actual durable storage before this process starts.
    // /tmp can be tmpfs, so its default must never silently become this probe's
    // claimed disk baseline. Filesystem type/mount evidence belongs to the runner.
    let temp_root = PathBuf::from(std::env::var_os("TMPDIR").expect("set a private disk TMPDIR"));
    assert_eq!(std::env::temp_dir(), temp_root);
    let temp_metadata = std::fs::metadata(&temp_root).unwrap();
    assert!(temp_metadata.is_dir());
    assert_eq!(temp_metadata.permissions().mode() & 0o777, 0o700);
    let profile = crate::firmware_linux_completion_journal::test_profile::Profile::new();
    let started = Instant::now();
    let fixture = owner_fixture_with_receiver_profile(
        FixtureFault::MidTransfer,
        false,
        false,
        180,
        Some(RETAINED_GP200_180_IDENTITY.to_vec()),
        300,
        Some(&profile),
    );
    assert!(LibraryStore::open(&fixture.store_root).is_err());
    let deadline = Instant::now() + Duration::from_secs(360);
    while !fixture.owner.stopped() {
        let view = fixture.owner.view();
        // A phase name alone is transitional. Only the atomically published
        // review plus its exact digest can grant this synthetic owner's phase.
        let signal = match (
            view.phase,
            view.review.as_ref(),
            view.review_digest.as_deref(),
        ) {
            (super::LinuxCompletionPhase::ReviewBootQuery, Some(review), Some(digest)) => {
                let bytes = std::fs::read(fixture.run.join("boot-query-review.json")).unwrap();
                assert_eq!(JobSha256::of(&bytes).hex(), digest);
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                    *review
                );
                assert_eq!(review["run"].as_str(), Some(view.run.as_str()));
                assert_eq!(
                    review["wire"],
                    serde_json::json!(FirmwareFixedPacket::IdentityQuery.raw())
                );
                assert_eq!(review["offers"], 1);
                assert_eq!(review["transfer_permitted"], false);
                Some(("approve-query", format!("execute-query:{digest}")))
            }
            (super::LinuxCompletionPhase::ReviewTransfer, Some(review), Some(digest)) => {
                let bytes = std::fs::read(fixture.run.join("transfer-review.json")).unwrap();
                assert_eq!(JobSha256::of(&bytes).hex(), digest);
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                    *review
                );
                assert_eq!(review["run"].as_str(), Some(view.run.as_str()));
                assert_eq!(review["actual_chunk"], 180);
                assert_eq!(review["transfer_seconds"], 300);
                assert_eq!(review["approval_wait_seconds"], 30);
                assert_eq!(review["budget"]["blocks"], 35_851);
                assert_eq!(review["offers_each"], 1);
                assert_eq!(review["automatic_retry"], false);
                Some(("approve-transfer", format!("execute-transfer:{digest}")))
            }
            _ => None,
        };
        if let Some((name, signal)) = signal
            && !fixture.run.join(name).exists()
        {
            write_exact_phase(&fixture.run.join(name), &signal);
        }
        assert!(
            Instant::now() < deadline,
            "bounded disk timing owner did not settle"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!fixture.stop.requested.load(Ordering::SeqCst));
    let (_, store, outcome) = fixture.owner.retire().unwrap();
    let elapsed_ns = started.elapsed().as_nanos();
    assert_eq!(store.root(), fixture.store_root);
    assert!(LibraryStore::open(&fixture.store_root).is_err());
    assert_eq!(
        outcome.view.phase,
        super::LinuxCompletionPhase::RecoveryRequired
    );
    assert!(outcome.view.quarantined && !outcome.editor_return_allowed);
    assert!(!outcome.view.host_finished_unverified && !outcome.view.normal_mode_verified);
    assert!(!outcome.view.target_version_verified && !outcome.view.contents_verified);
    assert_eq!(outcome.view.device_verified_bytes, 0);
    assert_eq!(outcome.view.host_write_invocations, 301);
    assert_eq!(outcome.view.host_accepted_blocks, 294);
    assert_eq!(outcome.view.allocated_raw_frontier, 297);
    assert!(outcome.application_version.is_none() && outcome.verified_normal_epoch.is_none());
    assert!(!fixture.run.join("application-version-review.json").exists());
    let wires = fixture.platform.writes.lock().unwrap();
    assert_eq!(wires.len(), 301);
    assert_eq!(wires[..2], crate::firmware_profile::entry_candidates());
    assert_eq!(wires[2], FirmwareFixedPacket::IdentityQuery.raw());
    assert_eq!(wires.iter().filter(|wire| wire[3] == 4).count(), 295);
    for (index, wire) in wires[6..].iter().enumerate() {
        assert_eq!(
            FirmwareBlock::decode(wire, 180).unwrap().block_index_mod128,
            (index % 128) as u8
        );
    }
    assert!(!wires.iter().any(|wire| matches!(wire[3], 5 | 7 | 9)));
    drop(wires);

    // Syscall samples use the original owner clock; phase counters remain only
    // in memory until this original owner joins. The instrumentation itself has
    // CPU/locking overhead, but adds no journal records or filesystem writes.
    let mut journal = std::io::BufReader::new(std::fs::File::open(&outcome.journal_path).unwrap());
    let mut journal_hash = sha2::Sha256::new();
    let mut counts = BTreeMap::<String, u64>::new();
    let mut blocks = BTreeMap::<u64, (u64, u64)>::new();
    let mut drains = BTreeMap::<u64, (u64, u64)>::new();
    let mut block_ordinals = Vec::new();
    let mut verified_frames = 0;
    let mut reader_retirement_generations = Vec::new();
    loop {
        let mut header = [0_u8; 44];
        if journal.read(&mut header[..1]).unwrap() == 0 {
            break;
        }
        journal.read_exact(&mut header[1..]).unwrap();
        assert_eq!(&header[..8], b"GP2LCJ01");
        let length = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
        assert!(length <= 32 * 1024 * 1024);
        let mut body = vec![0; length];
        journal.read_exact(&mut body).unwrap();
        assert_eq!(&header[12..], sha2::Sha256::digest(&body).as_slice());
        journal_hash.update(header);
        journal_hash.update(&body);
        let record: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let kind = record["kind"].as_str().unwrap();
        *counts.entry(kind.into()).or_default() += 1;
        verified_frames += 1;
        match kind {
            "checkpoint" if record["wire"][3] == 4 => {
                block_ordinals.push(record["ordinal"].as_u64().unwrap());
            }
            "write-syscall" if block_ordinals.last() == record["ordinal"].as_u64().as_ref() => {
                let observation = &record["observation"];
                if observation["errno"].is_null() {
                    blocks.insert(
                        record["ordinal"].as_u64().unwrap(),
                        (
                            observation["before_ns"].as_u64().unwrap(),
                            observation["after_ns"].as_u64().unwrap(),
                        ),
                    );
                }
            }
            "drain-syscall" => {
                let observation = &record["observation"];
                assert!(observation["errno"].is_null());
                assert_eq!(observation["returned_count"], 0);
                drains.insert(
                    record["ordinal"].as_u64().unwrap(),
                    (
                        observation["before_ns"].as_u64().unwrap(),
                        observation["after_ns"].as_u64().unwrap(),
                    ),
                );
            }
            "firmware-reader-retired" => {
                assert_eq!(record["accounted"], true);
                assert_eq!(record["output_destroyed"], true);
                assert_eq!(record["source"]["reader_join"], "Returned");
                let generation = record["source"]["generation"].as_u64().unwrap();
                let expected_frontier = match generation {
                    1 => 0,
                    2 => 297,
                    _ => panic!("unexpected reader generation"),
                };
                assert_eq!(record["source"]["allocated"], expected_frontier);
                assert_eq!(record["source"]["processed"], expected_frontier);
                assert_eq!(record["source"]["fault_or_missing"], false);
                reader_retirement_generations.push(generation);
            }
            "normal-mode-observed" | "live-completion-verified" => {
                panic!("partial mock may not close a hold")
            }
            _ => {}
        }
    }
    assert_eq!(reader_retirement_generations, [1, 2]);
    assert_eq!(counts["checkpoint"], 301);
    assert_eq!(counts["write-syscall"], 301);
    assert_eq!(counts["drain-syscall"], 300);
    assert_eq!(counts["raw-chunk"], 297);
    assert_eq!(counts["vendor-action-pair"], 298);
    assert_eq!(counts["failed"], 1);
    assert_eq!(counts["firmware-reader-retired"], 2);
    assert_eq!(blocks.len(), 294);
    let successful = blocks
        .iter()
        .map(|(&ordinal, &(before, after))| (ordinal, before, after))
        .collect::<Vec<_>>();
    let intervals = successful
        .windows(2)
        .map(|pair| pair[1].1.checked_sub(pair[0].1).unwrap())
        .collect::<Vec<_>>();
    let write_to_drain = successful
        .iter()
        .map(|(ordinal, _, after)| drains[ordinal].0.checked_sub(*after).unwrap())
        .collect::<Vec<_>>();
    let drain_to_next = successful
        .windows(2)
        .map(|pair| pair[1].1.checked_sub(drains[&pair[0].0].1).unwrap())
        .collect::<Vec<_>>();

    let registry = std::fs::read(
        fixture
            .platform
            .registry
            .join(format!("{}.quarantine", outcome.view.run)),
    )
    .unwrap();
    let mut offset = 0;
    let mut states = Vec::new();
    while offset < registry.len() {
        assert_eq!(&registry[offset..offset + 8], b"GP2QUAR1");
        let length =
            u32::from_le_bytes(registry[offset + 8..offset + 12].try_into().unwrap()) as usize;
        let body = &registry[offset + 44..offset + 44 + length];
        assert_eq!(
            &registry[offset + 12..offset + 44],
            sha2::Sha256::digest(body).as_slice()
        );
        states.push(body[0]);
        offset += 44 + length;
    }
    assert_eq!(states, [0, 1]);
    assert!(
        crate::firmware_quarantine::QuarantineRegistry::mock(&fixture.platform.registry)
            .unwrap()
            .admit_normal()
            .is_err()
    );
    let host_costs = profile.snapshot();
    assert_host_cost_profile(&host_costs, verified_frames, &counts);
    assert_eq!(host_costs["successful_commits"], verified_frames - 4 * 294);
    println!(
        "{}",
        serde_json::json!({
            "schema": 1,
            "kind": "bounded-180-actual-owner-durable-journal-timing",
            "physical_hardware": false,
            "io": "FixtureIo MockSimulation only; no USB or ALSA opener",
            "tmpdir": temp_root,
            "elapsed_until_original_owner_retired_ns": elapsed_ns,
            "elapsed_window": "before fixture construction to after original owner retirement",
            "image_sha256": crate::firmware_vendor_sequence::VENDOR_IMAGE_SHA256,
            "receiver_chunk_bytes": 180,
            "writes": 301,
            "accepted_blocks": 294,
            "verified_journal_frames": verified_frames,
            "journal_sha256": format!("{:x}", journal_hash.finalize()),
            "journal_record_counts": counts,
            "block_write_start_intervals": statistics_ns(&intervals),
            "write_return_to_drain_call": statistics_ns(&write_to_drain),
            "drain_return_to_next_block_write": statistics_ns(&drain_to_next),
            "host_costs": host_costs,
            "logical_records_per_complete_block": 5,
            "steady_state_syncs_per_complete_block": 1,
            "durable_policy": "previous Block outcome plus next same-bank checkpoint, single original transaction",
            "successful_journal_commits": host_costs["successful_commits"],
            "accepted_block_records": 5 * 294,
            "records_outside_fully_accepted_blocks": verified_frames - 5 * 294,
            "stage_gap_limit": "Phase counters are test-only wall-clock samples, include instrumentation overhead, and do not explain physical-pedal timing",
            "original_store_returned": true,
            "original_readers_retired": true,
            "reader_retirement_generations": reader_retirement_generations,
            "held_registry_states": states,
            "normal_admission_denied": true,
        })
    );
    let store_root = store.root().to_owned();
    drop(store);
    assert_eq!(LibraryStore::open(&store_root).unwrap().root(), store_root);
}

fn assert_host_cost_profile(
    profile: &serde_json::Value,
    verified_frames: u64,
    verified_kinds: &std::collections::BTreeMap<String, u64>,
) {
    let phases = &profile["phases"];
    let commits = profile["successful_commits"].as_u64().unwrap();
    assert!(commits <= verified_frames);
    for phase in ["append_serialize_payload", "append_frame_hash_encode"] {
        assert_eq!(phases[phase]["count"], verified_frames, "{phase}");
    }
    for phase in [
        "append_total",
        "append_metadata_before",
        "append_write_all",
        "append_sync_all",
        "append_reread_metadata",
        "append_publication_receipt",
    ] {
        assert_eq!(phases[phase]["count"], commits, "{phase}");
        assert!(phases[phase]["total_ns"].as_u64().is_some());
    }
    assert_eq!(
        profile["successful_append_kinds"],
        serde_json::to_value(verified_kinds).unwrap()
    );
    // begin is appended synchronously before the journal worker starts; all
    // remaining commits publish one original worker reply, even for many frames.
    assert_eq!(phases["worker_reply_publication"]["count"], commits - 1);
    assert_eq!(phases["owner_reply_acknowledge"]["count"], commits - 1);
    assert!(
        phases["binding_fixture_enumeration"]["count"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(
        phases["binding_revalidate_total"]["count"],
        phases["binding_fixture_enumeration"]["count"]
    );
    assert_eq!(profile["physical_hardware"], false);
    assert_eq!(profile["durations_overlap"], true);
}

#[test]
#[ignore = "requires separately pinned image; exact retained180 identity and full actual owner with MockSimulation I/O only"]
fn exact_retained_180_receiver_actual_owner_transfers_every_section_and_closes_after_fresh_version()
{
    use gp200_protocol::{FirmwareBlock, FirmwareDescriptor, FirmwareFixedPacket};
    use std::io::Read;

    let profile = test_profile::Profile::new();
    let started = Instant::now();
    let fixture = owner_fixture_with_receiver_profile(
        FixtureFault::None,
        false,
        false,
        180,
        Some(RETAINED_GP200_180_IDENTITY.to_vec()),
        1800,
        Some(&profile),
    );
    let (_temp, run, platform, store, outcome) = finish_owner_with_deadline(
        fixture,
        false,
        Duration::from_secs(1860),
        Some((180, 1800, 35_851)),
    );
    assert!(
        outcome.editor_return_allowed,
        "{:?}",
        outcome.view.diagnostic
    );
    assert!(
        outcome.view.host_finished_unverified
            && outcome.view.normal_mode_verified
            && outcome.view.target_version_verified
            && !outcome.view.quarantined
    );
    assert!(!outcome.view.contents_verified);
    assert_eq!(outcome.view.device_verified_bytes, 0);
    assert_eq!(outcome.view.host_accepted_blocks, 35_851);
    assert_eq!(outcome.view.total_blocks, Some(35_851));
    assert_eq!(outcome.view.host_write_invocations, 35_872);
    assert_eq!(outcome.application_version.as_deref(), Some("V1.8.2"));
    assert!(outcome.verified_normal_epoch.unwrap().0 > outcome.original_normal_epoch.0);

    let image = std::fs::read(std::env::var_os("GP200_VENDOR_SEQUENCE_IMAGE").unwrap()).unwrap();
    assert_eq!(image.len(), 6_452_728);
    assert_eq!(
        JobSha256::of(&image).hex(),
        crate::firmware_vendor_sequence::VENDOR_IMAGE_SHA256
    );
    let writes = platform.writes.lock().unwrap();
    assert_eq!(writes.len(), 35_872);
    assert_eq!(writes[..2], crate::firmware_profile::entry_candidates());
    assert_eq!(writes[2], FirmwareFixedPacket::IdentityQuery.raw());
    assert_eq!(
        writes
            .iter()
            .filter(|wire| wire.as_slice() == FirmwareFixedPacket::IdentityQuery.raw())
            .count(),
        1,
        "the receiver query is never retried"
    );
    let geometry = [
        (0, 80, 4_192_632, 23_293, 108),
        (1, 6_026_744, 294_912, 1_639, 108),
        (2, 6_321_656, 131_072, 729, 148),
        (0x10, 4_192_712, 1_834_032, 10_190, 168),
    ];
    let mut cursor = 3;
    let mut blocks = 0;
    for (selector, offset, section_bytes, count, final_padding) in geometry {
        assert_eq!(writes[cursor][3], 0x0b);
        assert_eq!(writes[cursor], writes[3]);
        cursor += 1;
        assert_eq!(writes[cursor], [0xf0, 0x7e, 0x48, 0x7f, selector, 0xf7]);
        cursor += 1;
        let descriptor = FirmwareDescriptor::decode(&writes[cursor]).unwrap();
        assert_eq!(descriptor.section_bytes() as usize, section_bytes);
        assert_eq!(descriptor.block_count() as usize, count);
        cursor += 1;
        for block_in_section in 0..count {
            let wire = &writes[cursor];
            assert_eq!(wire.len(), 368);
            // Decode validates framing, nibbles and the exact nibble checksum.
            let decoded = FirmwareBlock::decode(wire, 180).unwrap();
            assert_eq!(decoded.block_index_mod128, (block_in_section % 128) as u8);
            let consumed = block_in_section * 180;
            let content_bytes = (section_bytes - consumed).min(180);
            assert_eq!(
                decoded.data_padded[..content_bytes],
                image[offset + consumed..offset + consumed + content_bytes]
            );
            assert!(
                decoded.data_padded[content_bytes..]
                    .iter()
                    .all(|byte| *byte == 0xff)
            );
            let expected_checksum = decoded.data_padded.iter().fold(0_u8, |sum, byte| {
                sum.wrapping_add(byte >> 4).wrapping_add(byte & 15)
            });
            assert_eq!(decoded.checksum, expected_checksum);
            if block_in_section == 0 {
                assert_eq!(decoded.block_index_mod128, 0);
            }
            if block_in_section == 127 {
                assert_eq!(decoded.block_index_mod128, 127);
            }
            if block_in_section == 128 {
                assert_eq!(decoded.block_index_mod128, 0);
            }
            if block_in_section == count - 1 {
                assert_eq!(180 - content_bytes, final_padding);
            }
            cursor += 1;
            blocks += 1;
        }
        assert_eq!(writes[cursor], FirmwareFixedPacket::SectionTerminator.raw());
        cursor += 1;
    }
    assert_eq!(blocks, 35_851);
    assert_eq!(writes[cursor], FirmwareFixedPacket::TerminalQuery.raw());
    assert_eq!(
        writes[cursor + 1],
        FirmwareFixedPacket::TerminalControl.raw()
    );
    assert_eq!(cursor + 2, writes.len(), "no extra control, block or retry");
    drop(writes);

    // Stream and authenticate every durable frame rather than loading the full
    // complete-image journal into a second large in-memory representation.
    let mut journal = std::io::BufReader::new(std::fs::File::open(&outcome.journal_path).unwrap());
    let mut identity_pairs = 0;
    let mut action_pairs = 0;
    let mut terminal_order = Vec::new();
    let mut normal_observed = false;
    let mut completion_verified = false;
    loop {
        let mut header = [0_u8; 44];
        if journal.read(&mut header[..1]).unwrap() == 0 {
            break;
        }
        journal.read_exact(&mut header[1..]).unwrap();
        assert_eq!(&header[..8], b"GP2LCJ01");
        let length = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
        assert!(length <= 32 * 1024 * 1024);
        let mut body = vec![0; length];
        journal.read_exact(&mut body).unwrap();
        assert_eq!(&header[12..], sha2::Sha256::digest(&body).as_slice());
        let record: serde_json::Value = serde_json::from_slice(&body).unwrap();
        if record["kind"] == "vendor-action-pair" {
            action_pairs += 1;
            let receipt = &record["receipt"];
            assert_eq!(receipt["write"]["drain"], "Returned");
            if receipt["phase"] == "Identity" {
                identity_pairs += 1;
                assert_eq!(receipt["frames"].as_array().unwrap().len(), 1);
                assert_eq!(
                    receipt["frames"][0]["raw"],
                    serde_json::json!(RETAINED_GP200_180_IDENTITY.as_slice())
                );
            }
            if receipt["phase"] == "TerminalQuery" {
                assert_eq!(receipt["frames"].as_array().unwrap().len(), 2);
                assert_eq!(
                    receipt["frames"][0]["raw"],
                    serde_json::json!([240, 126, 72, 3, 10, 11, 247])
                );
                assert_eq!(
                    receipt["frames"][1]["raw"],
                    serde_json::json!([240, 126, 72, 7, 12, 13, 247])
                );
                assert_eq!(receipt["terminal_data"][0]["opcode"], 3);
                assert_eq!(
                    receipt["terminal_data"][0]["decoded"],
                    serde_json::json!([0xab])
                );
                assert_eq!(receipt["terminal_data"][1]["opcode"], 7);
                assert_eq!(
                    receipt["terminal_data"][1]["decoded"],
                    serde_json::json!([0xcd])
                );
                terminal_order.extend([3, 7]);
            }
            if receipt["phase"] == "FinishControl" {
                assert_eq!(receipt["wire"], serde_json::json!([240, 126, 72, 9, 247]));
                assert_eq!(receipt["decision"], "HostFinishedUnverified");
                terminal_order.push(9);
            }
        }
        if record["kind"] == "normal-mode-observed" {
            normal_observed = true;
            let proof = &record["proof"];
            assert_eq!(proof["reader_joined"], true);
            assert_eq!(proof["output_destroyed"], true);
            assert_eq!(proof["allocated"], proof["processed"]);
            assert_eq!(record["target_application_version"], "not observed");
        }
        if record["kind"] == "live-completion-verified" {
            assert!(normal_observed);
            completion_verified = true;
            assert_eq!(record["manual_version"], "V1.8.2");
            assert_eq!(record["contents_verified"], false);
            assert_eq!(record["installed_IR_audio"], "unavailable");
        }
    }
    assert_eq!(identity_pairs, 1);
    assert_eq!(action_pairs, 35_870);
    assert_eq!(terminal_order, [3, 7, 9]);
    assert!(normal_observed && completion_verified);
    for (file, prefix) in [
        ("boot-query", "execute-query"),
        ("transfer", "execute-transfer"),
        ("application-version", "confirm-application"),
    ] {
        let bytes = std::fs::read(run.join(format!("{file}-review.json"))).unwrap();
        let digest = JobSha256::of(&bytes).hex();
        let signal = if file == "application-version" {
            format!("{prefix}:{digest}:V1.8.2")
        } else {
            format!("{prefix}:{digest}")
        };
        let signal_name = match file {
            "boot-query" => "approve-query",
            "transfer" => "approve-transfer",
            _ => "confirm-application-version",
        };
        assert_eq!(
            std::fs::read_to_string(run.join(signal_name)).unwrap(),
            signal
        );
    }
    crate::firmware_quarantine::QuarantineRegistry::mock(&platform.registry)
        .unwrap()
        .admit_normal()
        .unwrap();
    let root = store.root().to_owned();
    drop(store);
    assert_eq!(LibraryStore::open(&root).unwrap().root(), root);
    let records = authenticated_records(std::path::Path::new(&outcome.journal_path));
    let measured = profile.snapshot();
    // Each bank starts with one checkpoint and ends with one standalone
    // outcome; all interior Block outcome/checkpoint pairs share one commit.
    assert_eq!(
        measured["successful_commits"],
        records.len() as u64 - 4 * 35_851 + 4
    );
    assert_eq!(
        measured["phases"]["append_sync_all"]["count"],
        measured["successful_commits"]
    );
    assert_eq!(platform.trace.lock().unwrap().pair_retirements, 35_851);
    for (index, record) in records.iter().enumerate() {
        if record["kind"] == "checkpoint" && record["purpose"] == "vendor Block" {
            let ordinal = record["ordinal"].as_u64().unwrap();
            if index > 0 && records[index - 1]["receipt"]["phase"] == "Block" {
                // This normal-mode fixture offered two entry messages before
                // the engine's ordinal zero identity query.
                assert_eq!(
                    records[index - 1]["receipt"]["ordinal"].as_u64().unwrap() + 3,
                    ordinal
                );
            }
        }
        if record["kind"] == "checkpoint" && record["purpose"] == "vendor SectionTerminator" {
            assert_eq!(records[index - 1]["kind"], "vendor-action-pair");
            assert_eq!(records[index - 1]["receipt"]["phase"], "Block");
        }
    }
    let wires = platform.writes.lock().unwrap();
    let mut tx = sha2::Sha256::new();
    for wire in wires.iter() {
        tx.update(wire);
    }
    println!(
        "{}",
        serde_json::json!({"kind":"full-180-fusion-candidate-acceptance", "physical_hardware":false,
        "image_sha256":crate::firmware_vendor_sequence::VENDOR_IMAGE_SHA256,
        "writes":wires.len(),"accepted_blocks":35_851,"verified_journal_frames":records.len(),
        "ordered_tx_sha256":format!("{:x}",tx.finalize()),"successful_journal_commits":measured["successful_commits"],
        "removed_syncs_from_two_barrier_policy":35_847,"elapsed_ns":started.elapsed().as_nanos(),
        "host_costs":measured,"normal_reads_and_manual_version":"synthetic fixture proof only",
        "block_pair_retirements":platform.trace.lock().unwrap().pair_retirements})
    );
}
#[test]
#[ignore = "requires separately pinned image; actual owner zero-call and fault boundaries only"]
fn actual_owner_zero_invocations_return_but_any_attempted_entry_remains_held() {
    for (fault, journal_failure, expected_calls, allowed) in [
        (FixtureFault::EntryOpen, false, 0, true),
        (FixtureFault::PrewriteExpiry, false, 0, true),
        (FixtureFault::None, true, 0, true),
        (FixtureFault::PartialEntry, false, 1, false),
        (FixtureFault::UnknownEntry, false, 1, false),
        (FixtureFault::DrainEntry, false, 1, false),
        (FixtureFault::RawLoss, false, 1, false),
    ] {
        let (_temp, run, platform, _store, outcome) =
            finish_owner(owner_fixture(fault, journal_failure), false);
        assert_eq!(platform.writes.lock().unwrap().len(), expected_calls);
        assert_eq!(
            outcome.editor_return_allowed, allowed,
            "{:?}",
            outcome.view.diagnostic
        );
        assert!(
            !outcome.view.target_version_verified
                && !outcome.view.contents_verified
                && !outcome.view.host_finished_unverified
        );
        assert_eq!(
            crate::firmware_quarantine::QuarantineRegistry::mock(&platform.registry)
                .unwrap()
                .admit_normal()
                .is_ok(),
            allowed
        );
        if journal_failure {
            assert!(run.join("startup-zero-invocations.json").exists());
        }
    }
}
#[test]
#[ignore = "requires separately pinned image; actual zero-invocation owner rejects valid passive boot return, no hardware"]
fn actual_zero_invocation_returns_reject_passive_boot_without_normal_proof() {
    for journal_start_fails in [false, true] {
        let fixture =
            owner_fixture_with_passive_boot(FixtureFault::None, journal_start_fails, true);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !fixture.owner.stopped() {
            assert!(LibraryStore::open(&fixture.store_root).is_err());
            assert!(Instant::now() < deadline, "zero-call owner did not retire");
            std::thread::sleep(Duration::from_millis(1));
        }
        let (_, store, outcome) = fixture.owner.retire().unwrap();
        assert_eq!(store.root(), fixture.store_root);
        assert!(LibraryStore::open(&fixture.store_root).is_err());
        assert!(fixture.platform.writes.lock().unwrap().is_empty());
        assert_eq!(outcome.view.host_write_invocations, 0);
        assert_eq!(outcome.view.os_offered_bytes, 0);
        assert_eq!(
            outcome.view.phase,
            super::LinuxCompletionPhase::RecoveryRequired
        );
        assert!(outcome.view.quarantined && !outcome.editor_return_allowed);
        assert!(!outcome.view.normal_mode_verified);
        assert!(!outcome.view.target_version_verified);
        assert!(!outcome.view.contents_verified);
        assert!(outcome.selection.is_none());
        assert!(outcome.verified_normal_epoch.is_none());
        assert!(outcome.application_version.is_none());
        assert!(!fixture.run.join("startup-zero-invocations.json").exists());
        for name in [
            "approve-query",
            "approve-transfer",
            "confirm-application-version",
        ] {
            assert!(!fixture.run.join(name).exists());
        }
        assert!(!fixture.stop.requested.load(Ordering::SeqCst));
        drop(store);
        assert!(LibraryStore::open(&fixture.store_root).is_ok());
    }
}
#[test]
#[ignore = "requires separately pinned image; deterministic actual Stop and original delayed journal replies, no hardware"]
fn actual_stop_zero_calls_settles_pending_intent_and_returns_the_original_store() {
    for fault in [
        FixtureFault::StopBeforeEntry,
        FixtureFault::StopPendingCheckpoint,
        FixtureFault::StopPendingRecord,
    ] {
        let fixture = owner_fixture(fault, false);
        let stop = fixture.stop.clone();
        let (_temp, _run, platform, _store, outcome) = finish_owner(fixture, false);
        assert!(stop.requested.load(Ordering::SeqCst), "{fault:?}");
        assert!(platform.writes.lock().unwrap().is_empty(), "{fault:?}");
        assert_eq!(outcome.view.host_write_invocations, 0);
        assert!(
            outcome.editor_return_allowed
                && outcome.view.normal_mode_verified
                && !outcome.view.quarantined,
            "{fault:?}: {:?}",
            outcome.view.diagnostic
        );
        assert!(
            !outcome.view.host_finished_unverified
                && !outcome.view.target_version_verified
                && !outcome.view.contents_verified
        );
        assert!(outcome.verified_normal_epoch.unwrap().0 > outcome.original_normal_epoch.0);
        crate::firmware_quarantine::QuarantineRegistry::mock(&platform.registry)
            .unwrap()
            .admit_normal()
            .unwrap();
        let bytes = std::fs::read(&outcome.journal_path).unwrap();
        let mut offset = 0;
        let mut records = Vec::new();
        while offset < bytes.len() {
            assert_eq!(&bytes[offset..offset + 8], b"GP2LCJ01");
            let len =
                u32::from_le_bytes(bytes[offset + 8..offset + 12].try_into().unwrap()) as usize;
            let body = &bytes[offset + 44..offset + 44 + len];
            assert_eq!(
                &bytes[offset + 12..offset + 44],
                sha2::Sha256::digest(body).as_slice()
            );
            records.push(serde_json::from_slice::<serde_json::Value>(body).unwrap());
            offset += 44 + len;
        }
        assert!(!records.iter().any(|r| r["kind"] == "write-syscall"));
        assert!(
            records
                .iter()
                .any(|r| r["kind"] == "creating-owner-zero-invocations"
                    && r["calls"] == 0
                    && r["normal"]["reader_joined"] == true
                    && r["normal"]["output_destroyed"] == true)
        );
        let retirement = records
            .iter()
            .find(|r| r["kind"] == "creating-owner-uninvoked-journal-retirement")
            .unwrap();
        if fault != FixtureFault::StopBeforeEntry {
            assert!(retirement["settled_original_intent"].as_u64().is_some());
        }
        if fault == FixtureFault::StopPendingCheckpoint {
            assert!(
                retirement["retired_unused_checkpoint_intent"]
                    .as_u64()
                    .is_some()
            );
        }
    }
}
#[test]
#[ignore = "requires separately pinned image; two exact-image mock replays, no hardware"]
fn fresh_normal_failure_or_wrong_fresh_manual_application_version_never_clears() {
    for (fault, wrong_manual) in [
        (FixtureFault::NormalRead, false),
        (FixtureFault::None, true),
    ] {
        let (_temp, _run, platform, _store, outcome) =
            finish_owner(owner_fixture(fault, false), wrong_manual);
        assert!(outcome.view.host_finished_unverified);
        assert!(
            !outcome.editor_return_allowed
                && outcome.view.quarantined
                && !outcome.view.target_version_verified
        );
        assert!(
            crate::firmware_quarantine::QuarantineRegistry::mock(&platform.registry)
                .unwrap()
                .admit_normal()
                .is_err()
        );
    }
}

#[test]
#[cfg(gp200_private_evidence)]
fn actual_561_read_metadata_fits_only_the_owned_first_journal_record() {
    let bytes = include_bytes!("../test-fixtures/firmware/genuine-entry-review.json");
    assert_eq!(bytes.len(), 568951);
    assert_eq!(
        JobSha256::of(bytes).hex(),
        "c44ca19fa9b18380275c5c2c7bb9408e4afb6569219cbb0684ece5b446ca9651"
    );
    let review: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    let compact = serde_json::to_vec(&review).unwrap();
    assert_eq!(compact.len(), 568950);
    assert!(compact.len() > 512 * 1024);
    let temp = Temp::new();
    let root = temp.0.join("store");
    let mut journal = CompletionJournal::start(
        LibraryStore::open(&root).unwrap(),
        "66666666666666666666666666666666".into(),
        serde_json::json!({"fixture_only":true,"physical_authority":false,"review":review}),
    )
    .unwrap_or_else(|failure| panic!("{}", failure.error));
    assert!(LibraryStore::open(&root).is_err());
    let data = std::fs::read(journal.path()).unwrap();
    assert_eq!(&data[..8], b"GP2LCJ01");
    assert!(u32::from_le_bytes(data[8..12].try_into().unwrap()) > 512 * 1024);
    // Relabeling a later body as begin never grants the startup-only allowance.
    journal
        .record(serde_json::json!({"kind":"begin","review":review}))
        .unwrap();
    assert!(event(&journal).result.is_err());
    let returned = journal.join().unwrap();
    assert_eq!(returned.root(), root);
}
#[test]
fn zero_call_journal_retirement_settles_original_intent_and_revokes_its_offer() {
    let temp = Temp::new();
    let mut journal = CompletionJournal::start(
        LibraryStore::open(temp.0.join("store")).unwrap(),
        "99999999999999999999999999999999".into(),
        serde_json::json!({"fixture_only":true}),
    )
    .unwrap_or_else(|f| panic!("{}", f.error));
    let intent = journal
        .checkpoint(0, "never invoked".into(), vec![0xf0, 0xf7])
        .unwrap();
    assert_eq!(journal.pending_intent(), Some(intent));
    assert!(journal.retire_zero_call_offer().is_err());
    let checkpoint = match event(&journal).result.unwrap() {
        JournalReply::Checkpoint(c) => c,
        _ => panic!("checkpoint"),
    };
    assert_eq!(journal.pending_intent(), None);
    assert_eq!(journal.retire_zero_call_offer().unwrap(), Some(intent));
    assert!(journal.offer_fence(checkpoint, 0, &[0xf0, 0xf7]).is_err());
    journal
        .record(serde_json::json!({"kind":"zero-call-cleanup"}))
        .unwrap();
    assert!(matches!(
        event(&journal).result.unwrap(),
        JournalReply::Recorded(_)
    ));
    let _ = journal.join().unwrap();
}
#[test]
fn live_receipt_revalidation_rejects_same_length_changed_final_prefix() {
    use std::os::unix::fs::FileExt;
    let temp = Temp::new();
    let mut journal = CompletionJournal::start(
        LibraryStore::open(temp.0.join("store")).unwrap(),
        "77777777777777777777777777777777".into(),
        serde_json::json!({"fixture":true}),
    )
    .unwrap_or_else(|failure| panic!("{}", failure.error));
    journal
        .record(serde_json::json!({"kind":"live-completion-fixture"}))
        .unwrap();
    let JournalReply::Recorded(receipt) = event(&journal).result.unwrap() else {
        panic!("wrong receipt")
    };
    receipt.revalidate().unwrap();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&receipt.path)
        .unwrap();
    file.write_all_at(b"X", 0).unwrap();
    file.sync_all().unwrap();
    assert!(receipt.revalidate().is_err());
    let _ = journal.join().unwrap();
}
#[test]
fn the_shared_final_native_fence_checks_real_stop_deadline_fault_and_allocated_frontier() {
    use crate::firmware_backend::FirmwareReceiver;
    use std::sync::atomic::AtomicBool;
    let plan = FirmwareLifecyclePlanDto {
        review_sha256: JobSha256::of(b"review").hex(),
        profile_sha256: JobSha256::of(b"profile").hex(),
        origin: FirmwareObservationOrigin::MockSimulation,
        nonblock_actions: 20,
        epoch: SessionEpoch(9),
        binding: FirmwareUsbObservation {
            topology: "fixture".into(),
            vendor: 1,
            product: 2,
            serial: "fixture".into(),
            interface: "fixture".into(),
            endpoint: "fixture".into(),
        },
        generation: 2,
        clock_origin: "fixture-final-native".into(),
    };
    let receiver = FirmwareReceiver::new(&plan);
    let sink = receiver.sink.clone();
    let stop = AtomicBool::new(false);
    let admitted = CompletionInvocation {
        deadline: Instant::now() + Duration::from_secs(3),
        stop: &stop,
        processed: 0,
    };
    assert!(admitted.admit(&sink).is_ok());
    stop.store(true, Ordering::SeqCst);
    assert!(admitted.admit(&sink).is_err());
    stop.store(false, Ordering::SeqCst);
    let expired = CompletionInvocation {
        deadline: Instant::now() - Duration::from_millis(1),
        stop: &stop,
        processed: 0,
    };
    assert!(expired.admit(&sink).is_err());
    sink.receive(&[0xf0, 0xf7]);
    assert!(admitted.admit(&sink).is_err());
    assert_eq!(receiver.received.try_recv().unwrap().sequence, 1);
    let settled = CompletionInvocation {
        deadline: admitted.deadline,
        stop: &stop,
        processed: 1,
    };
    assert!(settled.admit(&sink).is_ok());
    sink.reader_fault();
    assert!(settled.admit(&sink).is_err());
}

struct RecoveryRetained {
    _temp: Temp,
    _enumeration: crate::firmware_entry_enumeration::tests::Fixture,
    store_root: PathBuf,
}
struct RecoveryOwnerFixture {
    original: RecoveryRetained,
    store: Option<LibraryStore>,
    old_attempt: FirmwareAttemptId,
    old_registry: Vec<u8>,
    old_journal: Vec<u8>,
    recovery_run: PathBuf,
    recovery_platform: FixturePlatform,
}
fn recovery_fixture() -> RecoveryOwnerFixture {
    let original = owner_fixture(FixtureFault::MidTransfer, false);
    let deadline = Instant::now() + Duration::from_secs(60);
    while !original.owner.stopped() {
        let view = original.owner.view();
        let signal = match (view.phase, view.review_digest) {
            (super::LinuxCompletionPhase::ReviewBootQuery, Some(h)) => {
                Some(("approve-query", format!("execute-query:{h}")))
            }
            (super::LinuxCompletionPhase::ReviewTransfer, Some(h)) => {
                Some(("approve-transfer", format!("execute-transfer:{h}")))
            }
            _ => None,
        };
        if let Some((name, text)) = signal
            && !original.run.join(name).exists()
        {
            write_exact_phase(&original.run.join(name), &text);
        }
        assert!(
            Instant::now() < deadline,
            "initial interrupted mock owner deadline"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let old_attempt = FirmwareAttemptId::parse(&original.owner.view().run).unwrap();
    // Retire by moving the real owner; retain every mock USB/guard directory for
    // a separately constructed fresh owner, without any normal handoff.
    let OwnerFixture {
        _temp,
        _enumeration,
        store_root,
        platform,
        owner,
        ..
    } = original;
    let (_, store, outcome) = owner.retire().unwrap();
    assert!(!outcome.editor_return_allowed && outcome.view.host_write_invocations > 300);
    let old_registry = std::fs::read(
        platform
            .registry
            .join(format!("{}.quarantine", old_attempt.as_str())),
    )
    .unwrap();
    let old_journal = std::fs::read(&outcome.journal_path).unwrap();
    let recovery_run = _temp.0.join("fresh-recovery-run");
    std::fs::create_dir(&recovery_run).unwrap();
    std::fs::set_permissions(&recovery_run, std::fs::Permissions::from_mode(0o700)).unwrap();
    let recovery_platform = FixturePlatform {
        chunk: 254,
        fault: FixtureFault::None,
        writes: std::sync::Arc::new(std::sync::Mutex::new(vec![])),
        trace: std::sync::Arc::new(std::sync::Mutex::new(FixtureTrace::default())),
        ..platform.clone()
    };
    let original = RecoveryRetained {
        _temp,
        _enumeration,
        store_root,
    };
    RecoveryOwnerFixture {
        original,
        store: Some(store),
        old_attempt,
        old_registry,
        old_journal,
        recovery_run,
        recovery_platform,
    }
}
impl RecoveryOwnerFixture {
    fn start(&mut self) -> (super::LinuxCompletionOwner, crate::LinuxCompletionMonitor) {
        self.start_with_transfer_seconds(300)
    }
    fn start_with_transfer_seconds(
        &mut self,
        transfer_seconds: u64,
    ) -> (super::LinuxCompletionOwner, crate::LinuxCompletionMonitor) {
        let bytes = std::fs::read(
            std::env::var_os("GP200_VENDOR_SEQUENCE_IMAGE").expect("pinned exact offline image"),
        )
        .unwrap();
        let source = RetainedJobBlob::checked(
            AssetId("MockSimulation fresh whole image".into()),
            std::sync::Arc::from(bytes),
            JobSha256::parse(crate::firmware_vendor_sequence::VENDOR_IMAGE_SHA256).unwrap(),
        )
        .unwrap();
        let image =
            FirmwareReview::inspect(source, None, &FirmwareBackupAssessment::default()).unwrap();
        let t = &self.original._temp.0;
        let inputs = crate::LinuxCompletionInputs {
            sources: vec![pinned(t.join("source"))],
            loss: pinned(t.join("loss")),
            recovery: pinned(t.join("recovery")),
            vendor_guidance: pinned(t.join("vendor-guidance")),
            contract: pinned(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
                "../../analysis/implementation/firmware-corrected-contract-binding/sequential-contract.json",
            )),
            run_directory: self.recovery_run.clone(),
            approval_wait_seconds: 30,
            transition_seconds: 5,
            transfer_seconds,
        };
        let registry =
            crate::firmware_quarantine::QuarantineRegistry::mock(&self.recovery_platform.registry)
                .unwrap();
        let binding = self.original._enumeration.binding.clone();
        let alias = crate::device_session_guard::EndpointIdentity {
            location: binding.interface.clone(),
            card_id: binding
                .endpoint
                .trim_start_matches("hw:")
                .split(',')
                .next()
                .unwrap()
                .into(),
            device: 0,
            subdevice: 0,
        };
        let guard = crate::device_session_guard::DeviceSessionGuard::fixture_physical(
            &t.join("runtime"),
            &[alias],
            &binding,
            &registry,
        )
        .unwrap();
        drop(registry);
        let anchor = super::CompletionOwnedAnchor {
            binding,
            guard: Some(guard),
        };
        let (request, monitor) = crate::LinuxRecoveryRequest::new(
            image,
            inputs,
            Some(self.old_attempt.clone()),
            SessionEpoch(70),
            self.store.take().unwrap(),
        );
        let owner = super::LinuxCompletionOwner::start_recovery_fixture(
            request,
            SessionEpoch(20),
            anchor,
            self.recovery_platform.clone(),
        );
        monitor.attach(&owner);
        (owner, monitor)
    }
    fn finish(
        &self,
        owner: super::LinuxCompletionOwner,
        wrong_manual: bool,
        stop_before_query: bool,
    ) -> (LibraryStore, crate::LinuxRecoveryOutcome) {
        let deadline = Instant::now() + Duration::from_secs(360);
        while !owner.stopped() {
            assert!(
                LibraryStore::open(&self.original.store_root).is_err(),
                "recovery retains original Store writer lock"
            );
            let view = owner.view();
            if stop_before_query && view.phase == super::LinuxCompletionPhase::ReviewBootQuery {
                owner.stop.requested.store(true, Ordering::SeqCst);
            }
            let signal = if stop_before_query {
                None
            } else {
                match (view.phase, view.review_digest) {
                    (super::LinuxCompletionPhase::ReviewBootQuery, Some(h)) => {
                        Some(("approve-query", format!("execute-query:{h}")))
                    }
                    (super::LinuxCompletionPhase::ReviewTransfer, Some(h)) => {
                        Some(("approve-transfer", format!("execute-transfer:{h}")))
                    }
                    (super::LinuxCompletionPhase::ConfirmApplicationVersion, Some(h)) => Some((
                        "confirm-application-version",
                        format!(
                            "confirm-application:{h}:{}",
                            if wrong_manual { "V1.6.3" } else { "V1.8.2" }
                        ),
                    )),
                    _ => None,
                }
            };
            if let Some((name, text)) = signal
                && !self.recovery_run.join(name).exists()
            {
                write_exact_phase(&self.recovery_run.join(name), &text);
            }
            assert!(
                Instant::now() < deadline,
                "fresh recovery actual owner deadline"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let (_, store, outcome) = owner.retire_recovery().unwrap();
        assert_eq!(store.root(), self.original.store_root);
        assert!(LibraryStore::open(&self.original.store_root).is_err());
        (store, outcome)
    }
}
#[test]
#[ignore = "requires separately pinned image; captured second-descriptor rejection through a fresh actual owner, MockSimulation I/O only"]
fn fresh_180_recovery_corrected_descriptor_synthetic_rejection_retires_reader_and_preserves_all_holds()
 {
    use gp200_protocol::{FirmwareBlock, FirmwareDescriptor, FirmwareFixedPacket};
    use std::io::Read;

    // The predecessor is a separate synthetic interrupted attempt. Its old
    // receiver geometry remains unchanged; only this new owner's fresh query
    // replays the identity captured from ae1a48b2. Injecting its status after
    // the corrected descriptor tests stop/hold semantics, not device acceptance.
    let mut f = recovery_fixture();
    f.recovery_platform.chunk = 180;
    f.recovery_platform.retained_identity = Some(RETAINED_GP200_180_IDENTITY.to_vec());
    f.recovery_platform.fault = FixtureFault::SecondDescriptorRejected;
    let profile = crate::firmware_linux_completion_journal::test_profile::Profile::new();
    profile.register_roots(&f.original.store_root, &f.recovery_platform.usb);
    let profile_started = Instant::now();
    let (owner, _monitor) = f.start_with_transfer_seconds(1800);
    let deadline = Instant::now() + Duration::from_secs(1860);
    while !owner.stopped() {
        assert!(LibraryStore::open(&f.original.store_root).is_err());
        let view = owner.view();
        assert!(!matches!(
            view.phase,
            super::LinuxCompletionPhase::HostFinishedUnverified
                | super::LinuxCompletionPhase::FindingNormal
                | super::LinuxCompletionPhase::ReadingNormal
                | super::LinuxCompletionPhase::ConfirmApplicationVersion
        ));
        // set_phase precedes phase_review's durable publication; wait_signal
        // also clears the review before the next phase becomes visible. Neither
        // transitional phase-only snapshot is a ready approval request.
        let published_matches = |name: &str, review: &serde_json::Value, digest: &str| {
            let bytes = std::fs::read(f.recovery_run.join(format!("{name}-review.json"))).unwrap();
            assert_eq!(JobSha256::of(&bytes).hex(), digest);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                *review
            );
            assert_eq!(review["run"].as_str(), Some(view.run.as_str()));
        };
        let signal = match (
            view.phase,
            view.review.as_ref(),
            view.review_digest.as_deref(),
        ) {
            (super::LinuxCompletionPhase::ReviewBootQuery, Some(review), Some(h)) => {
                published_matches("boot-query", review, h);
                assert_eq!(review["wire"], serde_json::json!([240, 126, 72, 1, 247]));
                assert_eq!(review["offers"], 1);
                assert_eq!(review["transfer_permitted"], false);
                Some(("approve-query", format!("execute-query:{h}")))
            }
            (super::LinuxCompletionPhase::ReviewTransfer, Some(review), Some(h)) => {
                published_matches("transfer", review, h);
                assert_eq!(review["actual_chunk"], 180);
                assert_eq!(review["transfer_seconds"], 1800);
                assert_eq!(review["budget"]["blocks"], 35_851);
                assert_eq!(review["offers_each"], 1);
                assert_eq!(review["automatic_retry"], false);
                Some(("approve-transfer", format!("execute-transfer:{h}")))
            }
            _ => None,
        };
        if let Some((name, text)) = signal
            && !f.recovery_run.join(name).exists()
        {
            write_exact_phase(&f.recovery_run.join(name), &text);
        }
        assert!(
            Instant::now() < deadline,
            "descriptor-negative owner did not settle"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!owner.stop.requested.load(Ordering::SeqCst));
    let (_, store, outcome) = owner.retire_recovery().unwrap();
    let elapsed_until_original_owner_retired_ns = profile_started.elapsed().as_nanos();
    assert_eq!(store.root(), f.original.store_root);
    assert!(LibraryStore::open(&f.original.store_root).is_err());
    assert_eq!(
        outcome.view.phase,
        super::LinuxCompletionPhase::RecoveryRequired
    );
    assert!(outcome.view.quarantined && !outcome.editor_return_allowed);
    assert!(!outcome.view.host_finished_unverified);
    assert!(!outcome.view.normal_mode_verified && !outcome.view.target_version_verified);
    assert!(!outcome.view.contents_verified && outcome.view.device_verified_bytes == 0);
    assert!(outcome.selection.is_none() && outcome.verified_normal_epoch.is_none());
    assert!(outcome.application_version.is_none());
    assert_eq!(outcome.view.total_blocks, Some(35_851));
    assert_eq!(outcome.view.host_accepted_blocks, 23_293);
    assert_eq!(outcome.view.host_write_invocations, 23_301);
    assert_eq!(outcome.view.allocated_raw_frontier, 23_299);
    let diagnostic = "Vendor write/drain: Descriptor response rejected for section 2 (selector 0x01): received status 0x7e, expected 0x7f";
    assert_eq!(outcome.view.diagnostic.as_deref(), Some(diagnostic));
    assert!(
        !f.recovery_run
            .join("application-version-review.json")
            .exists()
    );
    assert!(!f.recovery_run.join("confirm-application-version").exists());
    assert_eq!(outcome.history.as_ref().unwrap().predecessors.len(), 1);

    let image = std::fs::read(std::env::var_os("GP200_VENDOR_SEQUENCE_IMAGE").unwrap()).unwrap();
    assert_eq!(image.len(), 6_452_728);
    assert_eq!(
        JobSha256::of(&image).hex(),
        crate::firmware_vendor_sequence::VENDOR_IMAGE_SHA256
    );
    let writes = f.recovery_platform.writes.lock().unwrap();
    assert_eq!(writes.len(), 23_301);
    assert_eq!(writes[0], FirmwareFixedPacket::IdentityQuery.raw());
    assert_eq!(
        writes
            .iter()
            .filter(|w| w.as_slice() == FirmwareFixedPacket::IdentityQuery.raw())
            .count(),
        1
    );
    assert!(
        !writes
            .iter()
            .any(|w| crate::firmware_profile::entry_candidates().contains(w))
    );
    assert_eq!(writes[1][3], 0x0b);
    assert_eq!(writes[2], [0xf0, 0x7e, 0x48, 0x7f, 0, 0xf7]);
    let first_descriptor = FirmwareDescriptor::decode(&writes[3]).unwrap();
    assert_eq!(first_descriptor.section_bytes(), 4_192_632);
    assert_eq!(first_descriptor.block_count(), 23_293);
    for (index, wire) in writes[4..23_297].iter().enumerate() {
        let block = FirmwareBlock::decode(wire, 180).unwrap();
        assert_eq!(block.block_index_mod128, (index % 128) as u8);
        let consumed = index * 180;
        let content_bytes = (4_192_632 - consumed).min(180);
        assert_eq!(
            block.data_padded[..content_bytes],
            image[80 + consumed..80 + consumed + content_bytes]
        );
        assert!(
            block.data_padded[content_bytes..]
                .iter()
                .all(|byte| *byte == 0xff)
        );
        if index == 23_292 {
            assert_eq!(180 - content_bytes, 108);
        }
    }
    assert_eq!(writes[23_297], FirmwareFixedPacket::SectionTerminator.raw());
    assert_eq!(writes[23_298], writes[1]);
    assert_eq!(writes[23_299], [0xf0, 0x7e, 0x48, 0x7f, 1, 0xf7]);
    assert_eq!(writes[23_300], NATIVE_SECOND_DESCRIPTOR_180);
    assert_eq!(writes.iter().filter(|wire| wire[3] == 2).count(), 2);
    assert_eq!(writes.iter().filter(|wire| wire[3] == 4).count(), 23_293);
    assert!(!writes.iter().any(|wire| matches!(wire[3], 7 | 9)));
    assert_eq!(
        outcome.view.os_offered_bytes,
        writes.iter().map(|wire| wire.len() as u64).sum::<u64>()
    );
    drop(writes);

    // Authenticate every original journal frame while streaming. The rejected
    // reply is retained in its raw chunk even though the engine returns the
    // error before the owner publishes a completed descriptor action pair.
    let mut journal = std::io::BufReader::new(std::fs::File::open(&outcome.journal_path).unwrap());
    let mut checkpoints = 0_u64;
    let mut writes = 0_u64;
    let mut drains = 0_u64;
    let mut action_pairs = 0_u64;
    let mut accepted_blocks = 0_u64;
    let mut raw_chunks = 0_u64;
    let mut failures = 0;
    let mut retirements = 0;
    let mut verified_frames = 0;
    let mut verified_kinds = std::collections::BTreeMap::<String, u64>::new();
    loop {
        let mut header = [0_u8; 44];
        if journal.read(&mut header[..1]).unwrap() == 0 {
            break;
        }
        journal.read_exact(&mut header[1..]).unwrap();
        assert_eq!(&header[..8], b"GP2LCJ01");
        let length = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
        assert!(length <= 32 * 1024 * 1024);
        let mut body = vec![0; length];
        journal.read_exact(&mut body).unwrap();
        assert_eq!(&header[12..], sha2::Sha256::digest(&body).as_slice());
        let record: serde_json::Value = serde_json::from_slice(&body).unwrap();
        verified_frames += 1;
        *verified_kinds
            .entry(record["kind"].as_str().unwrap().into())
            .or_default() += 1;
        match record["kind"].as_str().unwrap() {
            "checkpoint" => {
                assert_eq!(record["ordinal"], checkpoints);
                if checkpoints == 23_300 {
                    assert_eq!(
                        record["wire"],
                        serde_json::json!(NATIVE_SECOND_DESCRIPTOR_180)
                    );
                }
                checkpoints += 1;
            }
            "write-syscall" => {
                assert_eq!(record["ordinal"], writes);
                assert_eq!(
                    record["observation"]["returned_count"],
                    record["observation"]["requested"]
                );
                assert!(record["observation"]["errno"].is_null());
                writes += 1;
            }
            "drain-syscall" => {
                assert_eq!(record["ordinal"], drains);
                assert_eq!(record["observation"]["returned_count"], 0);
                assert!(record["observation"]["errno"].is_null());
                drains += 1;
            }
            "vendor-action-pair" => {
                assert_eq!(record["receipt"]["ordinal"], action_pairs);
                assert_eq!(record["receipt"]["write"]["drain"], "Returned");
                assert!(!matches!(
                    record["receipt"]["phase"].as_str(),
                    Some("TerminalQuery" | "FinishControl")
                ));
                if record["receipt"]["phase"] == "Block" {
                    assert_eq!(
                        record["receipt"]["source_block"]["absolute_block_ordinal"],
                        accepted_blocks
                    );
                    assert_eq!(
                        record["receipt"]["source_block"]["block_in_section"],
                        accepted_blocks
                    );
                    accepted_blocks += 1;
                }
                action_pairs += 1;
            }
            "raw-chunk" => {
                raw_chunks += 1;
                let observation = &record["observation"];
                assert_eq!(observation["sequence"], raw_chunks);
                assert_eq!(observation["generation"], 2);
                if raw_chunks == 1 {
                    assert_eq!(
                        observation["raw"],
                        serde_json::json!(RETAINED_GP200_180_IDENTITY.as_slice())
                    );
                } else if raw_chunks == 23_299 {
                    assert_eq!(
                        observation["raw"],
                        serde_json::json!(RETAINED_DESCRIPTOR_REJECTION)
                    );
                } else {
                    assert_eq!(
                        observation["raw"],
                        serde_json::json!([240, 126, 72, 3, 127, 247])
                    );
                }
            }
            "failed" => {
                assert_eq!(record["error"], diagnostic);
                assert_eq!(record["calls"], 23_301);
                assert_eq!(record["raw_frontier"], 23_299);
                assert_eq!(record["no_retry"], true);
                failures += 1;
            }
            "firmware-reader-retired" => {
                assert_eq!(failures, 1);
                assert_eq!(record["accounted"], true);
                assert_eq!(record["output_destroyed"], true);
                assert_eq!(record["source"]["reader_join"], "Returned");
                assert_eq!(record["source"]["allocated"], 23_299);
                assert_eq!(record["source"]["processed"], 23_299);
                assert_eq!(record["source"]["generation"], 2);
                assert_eq!(record["source"]["fault_or_missing"], false);
                assert!(record["fault"].is_null());
                retirements += 1;
            }
            "normal-mode-observed" | "live-completion-verified" => {
                panic!("descriptor rejection must not verify firmware completion or normal mode");
            }
            _ => {}
        }
    }
    assert_eq!((checkpoints, writes, drains), (23_301, 23_301, 23_301));
    assert_eq!(action_pairs, 23_300);
    assert_eq!(accepted_blocks, 23_293);
    assert_eq!(raw_chunks, 23_299);
    assert_eq!((failures, retirements), (1, 1));

    assert_eq!(
        std::fs::read(
            f.recovery_platform
                .registry
                .join(format!("{}.quarantine", f.old_attempt.as_str()))
        )
        .unwrap(),
        f.old_registry
    );
    assert_eq!(
        std::fs::read(
            f.original
                .store_root
                .join("linux-firmware-completion")
                .join(format!("{}.journal", f.old_attempt.as_str()))
        )
        .unwrap(),
        f.old_journal
    );
    let registry = std::fs::read(
        f.recovery_platform
            .registry
            .join(format!("{}.quarantine", outcome.view.run)),
    )
    .unwrap();
    let mut offset = 0;
    let mut states = Vec::new();
    while offset < registry.len() {
        assert_eq!(&registry[offset..offset + 8], b"GP2QUAR1");
        let length =
            u32::from_le_bytes(registry[offset + 8..offset + 12].try_into().unwrap()) as usize;
        let body = &registry[offset + 44..offset + 44 + length];
        assert_eq!(
            &registry[offset + 12..offset + 44],
            sha2::Sha256::digest(body).as_slice()
        );
        states.push(body[0]);
        offset += 44 + length;
    }
    assert_eq!(
        states,
        [0, 1],
        "neither this hold nor a predecessor may receive a linked closure"
    );
    assert!(
        crate::firmware_quarantine::QuarantineRegistry::mock(&f.recovery_platform.registry)
            .unwrap()
            .admit_normal()
            .is_err()
    );
    let host_costs = profile.snapshot();
    assert_host_cost_profile(&host_costs, verified_frames, &verified_kinds);
    assert_eq!(
        host_costs["successful_commits"],
        verified_frames - 4 * accepted_blocks + 1
    );
    println!(
        "{}",
        serde_json::json!({
            "schema": 1,
            "kind": "first-section-180-rejected-second-descriptor-host-cost-profile",
            "physical_hardware": false,
            "io": "FixtureIo MockSimulation only; no USB or ALSA opener",
            "image_sha256": crate::firmware_vendor_sequence::VENDOR_IMAGE_SHA256,
            "receiver_chunk_bytes": 180,
            "writes": writes,
            "accepted_blocks": accepted_blocks,
            "verified_journal_frames": verified_frames,
            "journal_record_counts": verified_kinds,
            "elapsed_until_original_owner_retired_ns": elapsed_until_original_owner_retired_ns,
            "elapsed_window": "immediately before fresh recovery owner start to after its original owner retirement; predecessor fixture excluded",
            "host_costs": host_costs,
            "logical_records_per_complete_block": 5,
            "steady_state_syncs_per_complete_block": 1,
            "durable_policy": "previous Block outcome plus next same-bank checkpoint, single original transaction",
            "successful_journal_commits": host_costs["successful_commits"],
            "accepted_block_records": 5 * accepted_blocks,
            "records_outside_fully_accepted_blocks": verified_frames - 5 * accepted_blocks,
            "original_store_returned": true,
            "original_reader_retired": true,
            "predecessor_journal_and_registry_unchanged": true,
            "held_registry_states": states,
            "normal_admission_denied": true,
        })
    );
    let root = store.root().to_owned();
    drop(store);
    assert_eq!(LibraryStore::open(&root).unwrap().root(), root);
}
#[test]
#[ignore = "requires pinned exact local image; actual new owner/guards/journal/store with only MockSimulation I/O"]
fn fresh_recovery_from_middle_failure_queries_again_restarts_every_section_and_links_all_holds() {
    let mut f = recovery_fixture();
    let (owner, _monitor) = f.start();
    let (store, outcome) = f.finish(owner, false, false);
    assert!(
        outcome.editor_return_allowed,
        "{:?}",
        outcome.view.diagnostic
    );
    assert!(
        outcome.view.host_finished_unverified
            && outcome.view.normal_mode_verified
            && outcome.view.target_version_verified
            && !outcome.view.quarantined
    );
    assert!(!outcome.view.contents_verified && outcome.view.device_verified_bytes == 0);
    assert_eq!(outcome.editor_source_epoch, SessionEpoch(70));
    assert_eq!(outcome.application_version.as_deref(), Some("V1.8.2"));
    let writes = f.recovery_platform.writes.lock().unwrap();
    assert_eq!(writes[0], [0xf0, 0x7e, 0x48, 1, 0xf7]);
    assert!(
        !writes
            .iter()
            .any(|w| crate::firmware_profile::entry_candidates().contains(w))
    );
    assert_eq!(
        outcome.view.host_accepted_blocks, 25_407,
        "the newly queried exact chunk254 must replace old255"
    );
    let transfer: serde_json::Value = serde_json::from_slice(
        &std::fs::read(f.recovery_run.join("transfer-review.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(transfer["actual_chunk"], 254);
    assert_eq!(outcome.history.as_ref().unwrap().predecessors.len(), 1);
    let registry_path = f
        .recovery_platform
        .registry
        .join(format!("{}.quarantine", f.old_attempt.as_str()));
    let registry = std::fs::read(registry_path).unwrap();
    assert!(registry.starts_with(&f.old_registry));
    let old_journal = std::fs::read(
        f.original
            .store_root
            .join("linux-firmware-completion")
            .join(format!("{}.journal", f.old_attempt.as_str())),
    )
    .unwrap();
    assert_eq!(old_journal, f.old_journal);
    crate::firmware_quarantine::QuarantineRegistry::mock(&f.recovery_platform.registry)
        .unwrap()
        .admit_normal()
        .unwrap();
    // Importing the mock disk closure into a physical description never clears.
    let catalogue =
        crate::review_linux_firmware_history_at(store.root(), &f.recovery_platform.registry)
            .unwrap();
    assert!(!catalogue.diagnostics.is_empty());
    drop(writes);
    drop(store);
}
#[test]
#[ignore = "requires pinned exact local image; local actual owner Stop boundary only"]
fn fresh_recovery_stop_before_query_closes_only_new_zero_call_reservation() {
    let mut f = recovery_fixture();
    let (owner, _) = f.start();
    let (store, outcome) = f.finish(owner, false, true);
    assert!(!outcome.editor_return_allowed && outcome.view.quarantined);
    assert_eq!(outcome.view.host_write_invocations, 0);
    assert!(f.recovery_platform.writes.lock().unwrap().is_empty());
    assert_eq!(
        std::fs::read(
            f.recovery_platform
                .registry
                .join(format!("{}.quarantine", f.old_attempt.as_str()))
        )
        .unwrap(),
        f.old_registry
    );
    assert_eq!(
        std::fs::read(
            f.original
                .store_root
                .join("linux-firmware-completion")
                .join(format!("{}.journal", f.old_attempt.as_str()))
        )
        .unwrap(),
        f.old_journal
    );
    let q = std::fs::read(
        f.recovery_platform
            .registry
            .join(format!("{}.quarantine", outcome.view.run)),
    )
    .unwrap();
    assert!(
        q.windows(b"creating owner observed zero driver calls".len())
            .any(|w| w == b"creating owner observed zero driver calls")
    );
    assert!(
        crate::firmware_quarantine::QuarantineRegistry::mock(&f.recovery_platform.registry)
            .unwrap()
            .admit_normal()
            .is_err()
    );
    drop(store);
}
#[test]
#[ignore = "requires pinned exact local image; fresh full-owner manual-version negative"]
fn fresh_recovery_wrong_manual_version_leaves_predecessor_set_unchanged() {
    let mut f = recovery_fixture();
    let (owner, _) = f.start();
    let (store, outcome) = f.finish(owner, true, false);
    assert!(
        !outcome.editor_return_allowed
            && outcome.view.quarantined
            && outcome.view.host_finished_unverified
            && outcome.view.normal_mode_verified
            && !outcome.view.target_version_verified
    );
    assert_eq!(
        std::fs::read(
            f.recovery_platform
                .registry
                .join(format!("{}.quarantine", f.old_attempt.as_str()))
        )
        .unwrap(),
        f.old_registry
    );
    assert!(
        crate::firmware_quarantine::QuarantineRegistry::mock(&f.recovery_platform.registry)
            .unwrap()
            .admit_normal()
            .is_err()
    );
    drop(store);
}

#[test]
#[ignore = "requires exact pinned local image; actual owner original deadline boundary hooks, zero firmware calls"]
fn actual_recovery_phase_wait_keeps_original_deadline_before_read_after_revalidation_and_after_journal()
 {
    for boundary in [
        PhaseWaitBoundary::BeforeRead,
        PhaseWaitBoundary::AfterRevalidation,
        PhaseWaitBoundary::AfterJournal,
    ] {
        let mut f = recovery_fixture();
        let hook = std::sync::Arc::new(PhaseWaitHook {
            boundary,
            arrived: false.into(),
            release: false.into(),
            deadline: std::sync::Mutex::new(None),
        });
        f.recovery_platform.phase_wait_hook = Some(hook.clone());
        let (owner, _) = f.start();
        let startup_deadline = Instant::now() + Duration::from_secs(60);
        eprintln!(
            "Actual expiry boundary {boundary:?}: owner started with separate60s startup bound"
        );
        while !hook.arrived.load(Ordering::SeqCst) {
            assert!(
                !owner.stopped(),
                "owner stopped before boundary {:?}: {:?}",
                boundary,
                owner.view().diagnostic
            );
            if boundary != PhaseWaitBoundary::BeforeRead {
                let view = owner.view();
                if view.phase == super::LinuxCompletionPhase::ReviewBootQuery
                    && let Some(digest) = view.review_digest
                    && !f.recovery_run.join("approve-query").exists()
                {
                    write_exact_phase(
                        &f.recovery_run.join("approve-query"),
                        &format!("execute-query:{digest}"),
                    );
                }
            }
            assert!(
                Instant::now() < startup_deadline,
                "expiry boundary {boundary:?} startup timed out: view={:?}",
                owner.view()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let old_deadline = hook.deadline.lock().unwrap().unwrap();
        assert!(
            Instant::now() < old_deadline,
            "expiry boundary {boundary:?} arrived after the original finite phase clock: view={:?}",
            owner.view()
        );
        eprintln!(
            "Actual expiry boundary {boundary:?}: actually observed while original phase clock remains fresh"
        );
        while Instant::now() <= old_deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(f.recovery_platform.writes.lock().unwrap().is_empty());
        if boundary == PhaseWaitBoundary::BeforeRead {
            let digest = owner.view().review_digest.unwrap();
            write_exact_phase(
                &f.recovery_run.join("approve-query"),
                &format!("execute-query:{digest}"),
            );
        }
        assert!(LibraryStore::open(&f.original.store_root).is_err());
        let teardown_deadline = Instant::now() + Duration::from_secs(60);
        hook.release.store(true, Ordering::SeqCst);
        while !owner.stopped() {
            assert!(
                Instant::now() < teardown_deadline,
                "expiry boundary {boundary:?} teardown timed out: view={:?}",
                owner.view()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let (_, store, outcome) = owner.retire_recovery().unwrap();
        assert_eq!(store.root(), f.original.store_root);
        assert_eq!(outcome.view.host_write_invocations, 0);
        assert!(!outcome.editor_return_allowed && outcome.view.quarantined);
        assert!(
            outcome
                .view
                .diagnostic
                .as_deref()
                .unwrap()
                .contains("approval expired")
        );
        assert!(f.recovery_platform.writes.lock().unwrap().is_empty());
        assert_eq!(
            std::fs::read(
                f.recovery_platform
                    .registry
                    .join(format!("{}.quarantine", f.old_attempt.as_str()))
            )
            .unwrap(),
            f.old_registry
        );
        let new_q = std::fs::read(
            f.recovery_platform
                .registry
                .join(format!("{}.quarantine", outcome.view.run)),
        )
        .unwrap();
        assert!(
            new_q
                .windows(b"creating owner observed zero driver calls".len())
                .any(|w| w == b"creating owner observed zero driver calls")
        );
        assert!(
            crate::firmware_quarantine::QuarantineRegistry::mock(&f.recovery_platform.registry)
                .unwrap()
                .admit_normal()
                .is_err()
        );
        eprintln!(
            "Actual expiry boundary {boundary:?}: retired original Store with zero firmware writes and predecessor hold unchanged"
        );
        drop(store);
    }
}

#[test]
fn journal_batch_accepts_the_exact_67_record_boundary_and_checkpoint_only_reopen_has_no_token() {
    let temp = Temp::new();
    let root = temp.0.join("store");
    let run = "abababababababababababababababab";
    let mut journal = CompletionJournal::start(
        LibraryStore::open(&root).unwrap(),
        run.into(),
        serde_json::json!({"fixture":true}),
    )
    .unwrap_or_else(|failed| panic!("{}", failed.error));
    journal
        .records(vec![
            serde_json::json!({"kind":"raw-chunk"});
            MAX_BATCH_RECORDS
        ])
        .unwrap();
    let JournalReply::Recorded(receipt) = event(&journal).result.unwrap() else {
        panic!("batch reply")
    };
    assert_eq!(receipt.sequence, 68);
    journal
        .checkpoint(9, "uncertain on crash".into(), vec![240, 126, 72, 1, 247])
        .unwrap();
    let JournalReply::Checkpoint(_unused_token) = event(&journal).result.unwrap() else {
        panic!("checkpoint")
    };
    let path = journal.path().to_owned();
    let returned = journal.join().unwrap();
    let records = authenticated_records(&path);
    assert_eq!(records.last().unwrap()["kind"], "checkpoint");
    assert!(
        !records
            .iter()
            .any(|record| record["kind"] == "creating-owner-zero-invocations")
    );
    // Reopening the original history cannot manufacture another live token or
    // an owner-issued zero-call clearance; the existing file is never replaced.
    let failed = CompletionJournal::start(
        returned,
        run.into(),
        serde_json::json!({"fixture":"reopen"}),
    )
    .err()
    .unwrap();
    assert_eq!(authenticated_records(&path), records);
    assert_eq!(failed.store.root(), root);
}

const CHECKPOINT_CRASH_CHILD: &str =
    "firmware_linux_completion::tests::checkpoint_process_exit_child_fixture";
const CHECKPOINT_CRASH_SCOPE: &str = "GP200_COMPLETION_CHECKPOINT_CRASH_SCOPE";
const CHECKPOINT_CRASH_TOKEN: &str = "GP200_COMPLETION_CHECKPOINT_CRASH_TOKEN";
const CHECKPOINT_CRASH_EXIT: i32 = 73;

fn checkpoint_crash_plan(root: &std::path::Path) -> FirmwareLifecyclePlanDto {
    FirmwareLifecyclePlanDto {
        review_sha256: JobSha256::of(b"MockSimulation checkpoint crash source job").hex(),
        profile_sha256: crate::firmware_vendor_sequence::VENDOR_CONTRACT_SHA256.into(),
        // The ordinary history reader requires the local-observation schema.
        // This descriptive binding belongs only to the private mock registry;
        // no discovery, physical guard or native endpoint is constructed.
        origin: FirmwareObservationOrigin::LocalObservation,
        nonblock_actions: 20,
        epoch: SessionEpoch(8),
        binding: FirmwareUsbObservation {
            topology: root.join("mock-usb").display().to_string(),
            vendor: 0x84ef,
            product: 0x002a,
            serial: "MockSimulation checkpoint crash".into(),
            interface: "MockSimulation private interface".into(),
            endpoint: "MockSimulation:no-native-endpoint".into(),
        },
        generation: 1,
        clock_origin: "MockSimulation checkpoint crash clock".into(),
    }
}

fn checkpoint_crash_private_json(path: &std::path::Path, value: &serde_json::Value) {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(&serde_json::to_vec(value).unwrap()).unwrap();
    file.sync_all().unwrap();
}

#[test]
fn checkpoint_process_exit_keeps_fresh_recovery_held_without_zero_call_clearance() {
    checkpoint_process_exit_case("native-return");
}
#[test]
fn combined_process_exit_before_commit_after_ack_and_next_return_keeps_recovery_held() {
    for boundary in [
        "before-combined-submit",
        "after-combined-ack-before-offer",
        "after-next-native-return",
    ] {
        checkpoint_process_exit_case(boundary);
    }
}
fn checkpoint_process_exit_case(boundary: &str) {
    use std::{os::unix::process::ExitStatusExt, process::Stdio};
    let temp = Temp::new();
    let scope_path = temp.0.join("child-scope.json");
    let token = JobSha256::of(
        format!(
            "checkpoint-crash:{}:{}",
            std::process::id(),
            temp.0.display()
        )
        .as_bytes(),
    )
    .hex();
    checkpoint_crash_private_json(
        &scope_path,
        &serde_json::json!({"schema":1,"parent_pid":std::process::id(),
            "root":temp.0,"token":token,"child_test":CHECKPOINT_CRASH_CHILD,
            "physical_hardware":false,"boundary":boundary}),
    );
    // The child runs only the internal helper, never this parent test. Clear
    // inherited image/runtime controls and pass no native endpoint or opener.
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            CHECKPOINT_CRASH_CHILD,
            "--exact",
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .env_clear()
        .env(CHECKPOINT_CRASH_SCOPE, &scope_path)
        .env(CHECKPOINT_CRASH_TOKEN, &token)
        .current_dir(&temp.0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let child_pid = child.id();
    let child_pid_pending = temp.0.join("child-pid.pending.json");
    checkpoint_crash_private_json(
        &child_pid_pending,
        &serde_json::json!({"child_pid":child_pid,"token":token}),
    );
    // Publish the PID gate atomically so the child cannot read partial JSON.
    std::fs::rename(child_pid_pending, temp.0.join("child-pid.json")).unwrap();
    let output = child.wait_with_output().unwrap();
    let raw_wait_status = output.status.into_raw();
    assert_eq!(
        output.status.code(),
        Some(CHECKPOINT_CRASH_EXIT),
        "scoped child did not reach process::exit: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(raw_wait_status, CHECKPOINT_CRASH_EXIT << 8);
    assert_eq!(output.status.signal(), None);
    assert!(!output.status.core_dumped());
    let ready: serde_json::Value =
        serde_json::from_slice(&std::fs::read(temp.0.join("child-ready.json")).unwrap()).unwrap();
    assert_eq!(ready["child_pid"], child_pid);
    assert_eq!(ready["token"], token);
    assert_eq!(ready["boundary"], boundary);
    let combined_committed = matches!(
        boundary,
        "after-combined-ack-before-offer" | "after-next-native-return"
    );
    let write_calls = if boundary == "after-next-native-return" {
        2
    } else {
        1
    };
    let drain_calls = usize::from(boundary != "native-return");
    assert_eq!(ready["mock_write_calls"], write_calls);
    assert_eq!(ready["drain_calls"], drain_calls);
    assert_eq!(ready["reader_retirements"], 0);
    assert_eq!(ready["physical_hardware"], false);

    // A new process lifetime owns the real store lock and authenticates the
    // original retained files. The child's RAM-only syscall observation died;
    // its missing disk record is not a creating-owner zero-call proof.
    let store = LibraryStore::open(temp.0.join("store")).unwrap();
    let attempt = FirmwareAttemptId::parse("cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd").unwrap();
    let registry_path = temp.0.join("registry");
    let journal_path = store
        .root()
        .join("linux-firmware-completion")
        .join(format!("{}.journal", attempt.as_str()));
    let quarantine_path = registry_path.join(format!("{}.quarantine", attempt.as_str()));
    let journal_before = std::fs::read(&journal_path).unwrap();
    let quarantine_before = std::fs::read(&quarantine_path).unwrap();
    let records = authenticated_records(&journal_path);
    assert_eq!(records.len(), if combined_committed { 7 } else { 2 });
    assert_eq!(records[0]["kind"], "begin");
    assert_eq!(records[1]["kind"], "checkpoint");
    assert_eq!(records[1]["purpose"], "vendor Block");
    assert_eq!(records[1]["ordinal"], 0);
    assert_eq!(ready["checkpoint_sequence"], records.len());
    if combined_committed {
        assert_eq!(
            records
                .iter()
                .map(|record| record["kind"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "begin",
                "checkpoint",
                "write-syscall",
                "drain-syscall",
                "raw-chunk",
                "vendor-action-pair",
                "checkpoint"
            ]
        );
        assert_eq!(records[6]["ordinal"], 1);
        assert_eq!(records[6]["purpose"], "vendor Block");
        assert_eq!(
            records[2]["observation"]["returned_count"],
            records[2]["observation"]["requested"]
        );
        assert_eq!(records[3]["observation"]["returned_count"], 0);
    }
    assert_eq!(
        ready["checkpoint_prefix"],
        JobSha256::of(&journal_before).hex()
    );
    assert!(
        !quarantine_before
            .windows(b"creating owner observed zero driver calls".len())
            .any(|bytes| bytes == b"creating owner observed zero driver calls")
    );
    let registry = crate::firmware_quarantine::QuarantineRegistry::mock(&registry_path).unwrap();
    assert!(registry.admit_normal().is_err());
    let lineage = registry
        .checked_recovery_fixture_default(store.root())
        .unwrap();
    assert_eq!(lineage.selected(), &attempt);
    lineage.revalidate().unwrap();
    let preparation =
        crate::firmware_linux_recovery::RecoveryPreparation::from_checked_lineage(lineage);
    preparation.revalidate().unwrap();
    let review = preparation.review();
    assert_eq!(review.predecessors.len(), 1);
    assert_eq!(review.predecessors[0].attempt, attempt);
    assert_eq!(review.predecessors[0].journal.path, journal_path);
    assert_eq!(
        review.predecessors[0].journal.sha256,
        JobSha256::of(&journal_before).hex()
    );
    assert_eq!(
        review.predecessors[0].registry.sha256,
        JobSha256::of(&quarantine_before).hex()
    );
    assert_eq!(review.starts_at_section, 0);
    assert_eq!(review.starts_at_block, 0);
    assert!(review.requires_fresh_live_query && review.requires_new_full_image_approval);
    assert!(!review.can_resume() && !review.permits_flash());
    assert!(!review.physical_authority && !review.fresh_normal_backup_available);
    let catalogue = crate::firmware_linux_recovery::review_linux_firmware_history_at(
        store.root(),
        &registry_path,
    )
    .unwrap();
    assert!(catalogue.diagnostics.is_empty());
    assert_eq!(catalogue.rows.len(), 1);
    let row = &catalogue.rows[0];
    assert_eq!(row.attempt, attempt);
    assert_eq!(row.local_state, 1);
    assert!(!row.resolved && !row.physical_authority && !row.resume_supported);
    assert_eq!(row.closure_status, "unresolved local attempt; held");
    // The fresh registry cannot replace the lost owner's hold with a new
    // zero-offer reservation. Only a live creating owner can issue that proof.
    let replacement = FirmwareAttemptId::parse("dededededededededededededededede").unwrap();
    assert!(
        registry
            .reserve(
                &replacement,
                &checkpoint_crash_plan(&temp.0),
                crate::firmware_vendor_sequence::VENDOR_IMAGE_SHA256,
                store
                    .root()
                    .join("linux-firmware-completion")
                    .to_str()
                    .unwrap(),
            )
            .is_err()
    );
    assert!(
        !registry_path
            .join(format!("{}.quarantine", replacement.as_str()))
            .exists()
    );
    assert!(registry.admit_normal().is_err());
    assert_eq!(std::fs::read(&journal_path).unwrap(), journal_before);
    assert_eq!(std::fs::read(&quarantine_path).unwrap(), quarantine_before);
    eprintln!(
        "Checkpoint process-exit fixture: {}",
        serde_json::json!({"child_pid":child_pid,"raw_wait_status":raw_wait_status,
            "exit_code":output.status.code(),"signal":output.status.signal(),
            "boundary":boundary,"journal_kinds":records.iter().map(|record| &record["kind"]).collect::<Vec<_>>(),
            "mock_write_calls":write_calls,"drain_calls":drain_calls,"reader_retirements":0,"local_state":row.local_state,
            "resolved":row.resolved,"normal_admission":false,"physical_hardware":false})
    );
}

#[test]
#[ignore = "internal no-device subprocess helper; invoked only by the scoped parent test"]
fn checkpoint_process_exit_child_fixture() {
    use std::os::unix::fs::MetadataExt;
    let Some(scope_path) = std::env::var_os(CHECKPOINT_CRASH_SCOPE) else {
        // Ordinary --ignored runs cannot enter the process-exit fixture.
        eprintln!("Checkpoint crash child is disabled without its private parent scope");
        return;
    };
    assert_eq!(
        std::env::args().skip(1).collect::<Vec<_>>(),
        [
            CHECKPOINT_CRASH_CHILD,
            "--exact",
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ]
    );
    assert!(std::env::var_os("GP200_VENDOR_SEQUENCE_IMAGE").is_none());
    let scope_path = PathBuf::from(scope_path);
    let root = scope_path.parent().unwrap();
    assert_eq!(scope_path, root.join("child-scope.json"));
    private_directory(root).unwrap();
    assert_eq!(std::fs::canonicalize(root).unwrap(), root);
    let metadata = std::fs::symlink_metadata(&scope_path).unwrap();
    assert!(metadata.is_file() && !metadata.file_type().is_symlink());
    assert_eq!(metadata.mode() & 0o777, 0o600);
    assert_eq!(metadata.uid(), rustix::process::geteuid().as_raw());
    assert_eq!(metadata.nlink(), 1);
    let scope: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&scope_path).unwrap()).unwrap();
    let token = std::env::var(CHECKPOINT_CRASH_TOKEN).unwrap();
    assert_eq!(scope["schema"], 1);
    assert_eq!(scope["root"], root.to_str().unwrap());
    assert_eq!(scope["token"], token);
    assert_eq!(scope["child_test"], CHECKPOINT_CRASH_CHILD);
    assert_eq!(scope["physical_hardware"], false);
    let boundary = scope["boundary"].as_str().unwrap();
    assert!(matches!(
        boundary,
        "native-return"
            | "before-combined-submit"
            | "after-combined-ack-before-offer"
            | "after-next-native-return"
    ));
    assert_ne!(scope["parent_pid"], std::process::id());
    assert_eq!(
        scope["parent_pid"].as_u64(),
        Some(u64::try_from(rustix::process::getppid().unwrap().as_raw_pid()).unwrap())
    );
    let child_pid_path = root.join("child-pid.json");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !child_pid_path.exists() {
        assert!(Instant::now() < deadline, "private parent PID gate absent");
        std::thread::sleep(Duration::from_millis(1));
    }
    let pid: serde_json::Value =
        serde_json::from_slice(&std::fs::read(child_pid_path).unwrap()).unwrap();
    assert_eq!(pid["child_pid"], std::process::id());
    assert_eq!(pid["token"], token);

    let plan = checkpoint_crash_plan(root);
    let capture = FirmwareCaptureReservation {
        actor_run: "MockSimulation process-ending checkpoint fixture".into(),
        reservation: "MockSimulation descriptive original acquisition".into(),
        normal_owner: "MockSimulation retired original normal owner".into(),
        epoch: SessionEpoch(7),
        usb: plan.binding.clone(),
        clock_origin: plan.clock_origin.clone(),
        prepared_monotonic_ns: 0,
        prepared_wall_millis: 1000,
        initial_fence: FirmwareCaptureFence {
            allocated: 0,
            processed: 0,
        },
    };
    let pins = (0..3)
        .map(|index| {
            let path = root.join(format!("mock-capture-{index}.json"));
            checkpoint_crash_private_json(
                &path,
                &serde_json::json!({"MockSimulation":true,"index":index}),
            );
            pinned(path)
        })
        .collect::<Vec<_>>();
    let metadata = serde_json::json!({"review":{"schema":1,
        "job_sha256":plan.review_sha256,"reservation":capture,
        "acquisition":"MockSimulation descriptive original acquisition",
        "evidence_index_sha256":JobSha256::of(b"MockSimulation evidence index").hex(),
        "captured_materials":pins},
        "review_digest":JobSha256::of(b"MockSimulation source review").hex(),
        "original_normal_epoch":SessionEpoch(7)});
    let attempt = FirmwareAttemptId::parse("cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd").unwrap();
    let mut journal = CompletionJournal::start(
        LibraryStore::open(root.join("store")).unwrap(),
        attempt.as_str().into(),
        metadata,
    )
    .unwrap_or_else(|failed| panic!("{}", failed.error));
    let registry =
        crate::firmware_quarantine::QuarantineRegistry::mock(&root.join("registry")).unwrap();
    let mut quarantine = registry
        .reserve(
            &attempt,
            &plan,
            crate::firmware_vendor_sequence::VENDOR_IMAGE_SHA256,
            journal.path().parent().unwrap().to_str().unwrap(),
        )
        .unwrap();
    quarantine.arm().unwrap();
    let wire = gp200_protocol::firmware_block_candidate(
        0,
        180,
        b"MockSimulation lost original RAM outcome",
    )
    .unwrap()
    .raw()
    .to_vec();
    let intent = journal
        .checkpoint(0, "vendor Block".into(), wire.clone())
        .unwrap();
    let reply = event(&journal);
    assert_eq!(reply.intent, intent);
    let JournalReply::Checkpoint(checkpoint) = reply.result.unwrap() else {
        panic!("durable checkpoint reply absent")
    };
    let mut checkpoint_sequence = checkpoint.sequence();
    let mut checkpoint_prefix = checkpoint.prefix().to_owned();
    let _fence = journal.offer_fence(checkpoint, 0, &wire).unwrap();
    let receiver = crate::firmware_backend::FirmwareReceiver::new(&plan);
    let writes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let trace = std::sync::Arc::new(std::sync::Mutex::new(FixtureTrace::default()));
    let mut io = FixtureIo {
        chunk: 180,
        retained_identity: None,
        sink: receiver.sink.clone(),
        writes: writes.clone(),
        fault: FixtureFault::None,
        boot: boundary != "native-return",
        retired: None,
        descriptor_offers: 0,
        trace: trace.clone(),
        journal_root: journal.path().parent().unwrap().to_owned(),
        last_wire: Vec::new(),
        drain_attempt: 0,
        capacity_final_pending: false,
    };
    let stop = std::sync::atomic::AtomicBool::new(false);
    let invocation = CompletionInvocation {
        deadline: Instant::now() + Duration::from_secs(15),
        stop: &stop,
        processed: 0,
    };
    let original_return = io
        .write_once(&wire, &invocation, &mut || {
            quarantine.offered();
            Ok(())
        })
        .unwrap();
    assert_eq!(original_return.returned_count, Some(wire.len()));
    assert_eq!(original_return.errno, None);
    assert!(quarantine.zero_offer().is_err());
    assert_eq!(*writes.lock().unwrap(), vec![wire.clone()]);
    assert!(trace.lock().unwrap().drains.is_empty());
    assert!(trace.lock().unwrap().retirements.is_empty());
    assert_eq!(authenticated_records(journal.path()).len(), 2);
    drop(_fence);
    let mut drain_calls = 0;
    if boundary != "native-return" {
        let original_drain = io.drain_once().unwrap();
        assert_eq!(original_drain.returned_count, Some(0));
        drain_calls = 1;
        let original_raw = receiver.received.try_recv().unwrap();
        let values = vec![
            serde_json::json!({"kind":"write-syscall","ordinal":0,"observation":original_return}),
            serde_json::json!({"kind":"drain-syscall","ordinal":0,"observation":original_drain}),
            serde_json::json!({"kind":"raw-chunk","observation":original_raw}),
            serde_json::json!({"kind":"vendor-action-pair","MockSimulation":true,
                "receipt":{"ordinal":0,"phase":"Block","wire":wire,
                    "decision":"OpaqueStatusAccepted","original_return":true}}),
        ];
        // The staged previous native outcome exists only in the creating RAM
        // owner at this boundary. Exiting cannot retrospectively clear its hold.
        if boundary != "before-combined-submit" {
            let next_wire = gp200_protocol::firmware_block_candidate(
                1,
                180,
                b"MockSimulation successor original return",
            )
            .unwrap()
            .raw()
            .to_vec();
            let intent = journal
                .outcome_checkpoint(values, 1, "vendor Block".into(), next_wire.clone())
                .unwrap();
            let reply = event(&journal);
            assert_eq!(reply.intent, intent);
            let JournalReply::Checkpoint(mut checkpoint) = reply.result.unwrap() else {
                panic!("combined checkpoint absent");
            };
            checkpoint
                .outcome_receipt
                .take()
                .unwrap()
                .revalidate()
                .unwrap();
            checkpoint_sequence = checkpoint.sequence();
            checkpoint_prefix = checkpoint.prefix().to_owned();
            assert_eq!(authenticated_records(journal.path()).len(), 7);
            if boundary == "after-next-native-return" {
                let _fence = journal.offer_fence(checkpoint, 1, &next_wire).unwrap();
                let invocation = CompletionInvocation {
                    deadline: Instant::now() + Duration::from_secs(15),
                    stop: &stop,
                    processed: original_raw.sequence,
                };
                let original_next_return = io
                    .write_once(&next_wire, &invocation, &mut || {
                        quarantine.offered();
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(original_next_return.returned_count, Some(next_wire.len()));
                assert_eq!(writes.lock().unwrap().len(), 2);
                // This new return and its new RX remain in RAM when the process exits.
            }
        }
    }
    checkpoint_crash_private_json(
        &root.join("child-ready.json"),
        &serde_json::json!({"child_pid":std::process::id(),"token":token,
            "checkpoint_sequence":checkpoint_sequence,"checkpoint_prefix":checkpoint_prefix,
            "mock_write_calls":writes.lock().unwrap().len(),"drain_calls":drain_calls,"reader_retirements":0,
            "physical_hardware":false,"boundary":boundary}),
    );
    // Deliberately lose the original syscall observation, consumed checkpoint,
    // journal worker and store without Drop, close, settlement or join. This
    // test-only helper has no path to a physical USB/ALSA opener or parent spawn.
    std::process::exit(CHECKPOINT_CRASH_EXIT);
}

fn block_records(records: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    let checkpoint = records
        .iter()
        .find(|record| record["kind"] == "checkpoint" && record["purpose"] == "vendor Block")
        .unwrap();
    let ordinal = checkpoint["ordinal"].as_u64().unwrap();
    let offset = records
        .iter()
        .position(|record| std::ptr::eq(record, checkpoint))
        .unwrap();
    records[offset..]
        .iter()
        .take_while(|record| {
            record["kind"] != "checkpoint" || record["ordinal"].as_u64() == Some(ordinal)
        })
        .collect()
}
fn assert_block_owner_held(platform: &FixturePlatform, outcome: &super::LinuxCompletionOutcome) {
    assert!(outcome.view.quarantined && !outcome.editor_return_allowed);
    assert!(!outcome.view.host_finished_unverified && !outcome.view.target_version_verified);
    assert!(!outcome.view.normal_mode_verified);
    assert_eq!(outcome.view.device_verified_bytes, 0);
    assert!(
        crate::firmware_quarantine::QuarantineRegistry::mock(&platform.registry)
            .unwrap()
            .admit_normal()
            .is_err()
    );
    let trace = platform.trace.lock().unwrap();
    assert_eq!(
        trace
            .retirements
            .iter()
            .filter(|receipt| receipt["boot"] == true)
            .count(),
        1
    );
    assert!(
        trace
            .retirements
            .iter()
            .all(|receipt| receipt["output_destroyed"] == true
                && receipt["source"]["reader_join"] == "Returned")
    );
}
#[test]
#[ignore = "requires separately pinned image; first Block owner native faults and original durable evidence, MockSimulation only"]
fn actual_block_native_faults_retain_original_return_before_clock_work_and_never_resend() {
    for fault in [
        FixtureFault::BlockStopAfterWrite,
        FixtureFault::BlockZero,
        FixtureFault::BlockShort,
        FixtureFault::BlockUnknown,
        FixtureFault::BlockEintr,
        FixtureFault::BlockErrno,
        FixtureFault::BlockClock,
        FixtureFault::BlockDeliveryClock,
    ] {
        let (_temp, _run, platform, _store, outcome) =
            finish_owner(owner_fixture(fault, false), false);
        assert_block_owner_held(&platform, &outcome);
        let writes = platform.writes.lock().unwrap();
        assert_eq!(
            writes.iter().filter(|wire| wire[3] == 4).count(),
            1,
            "{fault:?}"
        );
        assert_eq!(outcome.view.host_write_invocations, writes.len() as u64);
        assert_eq!(outcome.view.host_accepted_blocks, 0, "{fault:?}");
        assert_eq!(
            platform
                .trace
                .lock()
                .unwrap()
                .drains
                .iter()
                .filter(|record| record["block"] == true)
                .count(),
            0,
            "{fault:?}"
        );
        let records = authenticated_records(std::path::Path::new(&outcome.journal_path));
        let block = block_records(&records);
        let native = block
            .iter()
            .find(|record| record["kind"] == "write-syscall")
            .unwrap();
        let observation = &native["observation"];
        assert_eq!(observation["requested"], writes.last().unwrap().len());
        assert_eq!(observation["operation"], "write");
        assert!(observation["before_ns"].as_u64().is_some());
        match fault {
            FixtureFault::BlockZero => assert_eq!(observation["returned_count"], 0),
            FixtureFault::BlockShort => assert_eq!(
                observation["returned_count"],
                writes.last().unwrap().len() / 2
            ),
            FixtureFault::BlockUnknown => {
                assert!(observation["returned_count"].is_null());
                assert_eq!(observation["errno"], 5);
            }
            FixtureFault::BlockEintr => {
                assert!(observation["returned_count"].is_null());
                assert_eq!(observation["errno"], 4);
            }
            FixtureFault::BlockErrno => {
                assert_eq!(observation["returned_count"], writes.last().unwrap().len());
                assert_eq!(observation["errno"], 5);
            }
            FixtureFault::BlockClock => {
                assert!(observation["after_ns"].is_null());
                assert_eq!(
                    observation["after_clock_error"],
                    "Mock original postcall clock failed"
                );
            }
            FixtureFault::BlockDeliveryClock => {
                assert!(native["journal_delivery_at_ms"].is_null());
                assert!(observation["after_ns"].as_u64().is_some());
            }
            _ => assert_eq!(observation["returned_count"], writes.last().unwrap().len()),
        }
        assert!(
            !block
                .iter()
                .any(|record| record["kind"] == "vendor-action-pair")
        );
        assert!(
            records
                .iter()
                .any(|record| record["kind"] == "firmware-reader-retired"
                    && record["source"]["generation"] == 2)
        );
    }
}
#[test]
#[ignore = "requires separately pinned image; first Returned, split ACK, during-drain, EAGAIN durable prefixes, MockSimulation only"]
fn actual_block_two_barriers_split_spans_and_eagain_prefixes_keep_original_action() {
    for fault in [
        FixtureFault::BlockDuringDrain,
        FixtureFault::BlockSplit,
        FixtureFault::BlockEagain,
        FixtureFault::BlockRepeatedEagain,
    ] {
        let profile = test_profile::Profile::new();
        let fixture = owner_fixture_with_receiver_profile(
            fault,
            false,
            false,
            180,
            Some(RETAINED_GP200_180_IDENTITY.to_vec()),
            300,
            Some(&profile),
        );
        let (_temp, _run, platform, _store, outcome) = finish_owner(fixture, false);
        assert_block_owner_held(&platform, &outcome);
        assert_eq!(
            platform
                .writes
                .lock()
                .unwrap()
                .iter()
                .filter(|wire| wire[3] == 4)
                .count(),
            1
        );
        assert_eq!(outcome.view.host_accepted_blocks, 1);
        let records = authenticated_records(std::path::Path::new(&outcome.journal_path));
        let block = block_records(&records);
        let kinds = block
            .iter()
            .map(|record| record["kind"].as_str().unwrap())
            .filter(|kind| !matches!(*kind, "failed" | "firmware-reader-retired"))
            .collect::<Vec<_>>();
        let drains = match fault {
            FixtureFault::BlockEagain => 2,
            FixtureFault::BlockRepeatedEagain => 4,
            _ => 1,
        };
        let raws = if fault == FixtureFault::BlockSplit {
            2
        } else {
            1
        };
        let expected = std::iter::once("checkpoint")
            .chain(std::iter::once("write-syscall"))
            .chain(std::iter::repeat_n("drain-syscall", drains))
            .chain(std::iter::repeat_n("raw-chunk", raws))
            .chain(std::iter::once("vendor-action-pair"))
            .collect::<Vec<_>>();
        assert_eq!(kinds, expected, "{fault:?}");
        let trace = platform.trace.lock().unwrap();
        assert_eq!(trace.pair_retirements, 1);
        assert_eq!(trace.durable_eagain_prefixes, drains - 1);
        let attempts = trace
            .drains
            .iter()
            .filter(|record| record["block"] == true)
            .collect::<Vec<_>>();
        assert_eq!(attempts.len(), drains);
        for pair in attempts.windows(2) {
            assert!(
                pair[1]["at_ns"].as_u64().unwrap() - pair[0]["at_ns"].as_u64().unwrap()
                    >= 1_000_000
            );
        }
        let pair = block
            .iter()
            .find(|record| record["kind"] == "vendor-action-pair")
            .unwrap();
        let chunks = block
            .iter()
            .filter(|record| record["kind"] == "raw-chunk")
            .map(|record| &record["observation"])
            .collect::<Vec<_>>();
        let span = &pair["original_frame_spans"][0];
        assert_eq!(span["first_chunk"], chunks[0]["sequence"]);
        assert_eq!(span["last_chunk"], chunks.last().unwrap()["sequence"]);
        assert_eq!(span["at_ns"], chunks[0]["at_ns"]);
        assert_eq!(
            pair["receipt"]["frames"][0]["at_ms"],
            chunks[0]["at_ns"].as_u64().unwrap() / 1_000_000
        );
        let measured = profile.snapshot();
        let frames = records.len() as u64;
        let savings = if drains == 1 { 2 + raws as u64 } else { 2 };
        assert_eq!(measured["successful_commits"], frames - savings);
        assert_eq!(
            measured["phases"]["append_sync_all"]["count"],
            frames - savings
        );
        assert_eq!(
            measured["phases"]["append_frame_hash_encode"]["count"],
            frames
        );
    }
}
#[test]
#[ignore = "requires separately pinned image; tentative pair, failed original batch, reader retirement and dual diagnostics, MockSimulation only"]
fn actual_block_batch_failure_never_retires_pair_and_still_returns_original_reader_and_store() {
    for fault in [
        FixtureFault::BlockBatchSubmit,
        FixtureFault::BlockBatchSync,
        FixtureFault::BlockBatchReread,
        FixtureFault::BlockWrongIntent,
        FixtureFault::BlockRevoked,
        FixtureFault::BlockCloseQueuedJournalFailure,
    ] {
        let (_temp, _run, platform, _store, outcome) =
            finish_owner(owner_fixture(fault, false), false);
        assert_block_owner_held(&platform, &outcome);
        assert_eq!(
            platform
                .writes
                .lock()
                .unwrap()
                .iter()
                .filter(|wire| wire[3] == 4)
                .count(),
            1
        );
        assert_eq!(outcome.view.host_accepted_blocks, 0);
        assert_eq!(platform.trace.lock().unwrap().pair_retirements, 0);
        let retained = outcome.view.retained_block_outcome.as_ref().unwrap();
        if fault == FixtureFault::BlockBatchSubmit {
            assert!(retained["original_intent"].is_null());
        } else {
            assert!(retained["original_intent"].as_u64().is_some());
        }
        let staged = retained["original_records"].as_array().unwrap();
        assert_eq!(
            staged
                .iter()
                .map(|record| record["kind"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "write-syscall",
                "drain-syscall",
                "raw-chunk",
                "vendor-action-pair"
            ]
        );
        let diagnostic = outcome.view.diagnostic.as_ref().unwrap();
        assert!(
            diagnostic.contains("original evidence settlement"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("actual I/O retirement"), "{diagnostic}");
        let retired = outcome.view.reader_retirement_diagnostics.last().unwrap();
        assert_eq!(retired["source"]["reader_join"], "Returned");
        assert_eq!(retired["output_destroyed"], true);
        assert!(!retired["raw_persistence_error"].is_null());
        let records = authenticated_records(std::path::Path::new(&outcome.journal_path));
        assert!(
            !block_records(&records)
                .iter()
                .any(|record| record["kind"] == "failed")
        );
        // Readable pair frames do not recreate the missing live acknowledgement.
        assert_eq!(
            records
                .iter()
                .any(|record| record["kind"] == "vendor-action-pair"
                    && record["receipt"]["phase"] == "Block"),
            fault != FixtureFault::BlockBatchSubmit
        );
        if fault == FixtureFault::BlockCloseQueuedJournalFailure {
            assert_eq!(outcome.view.retained_raw_chunks.len(), 1);
            assert_eq!(
                outcome.view.retained_raw_chunks[0].raw,
                [240, 126, 72, 3, 127, 247]
            );
            assert_eq!(
                retired["source"]["allocated"],
                retired["source"]["processed"]
            );
        }
    }
}
#[test]
#[ignore = "requires separately pinned image; actual Stop during original intent and deadline stall, MockSimulation only"]
fn actual_block_stop_during_outcome_settles_same_intent_without_renewing_budget() {
    for fault in [FixtureFault::BlockStopBatch, FixtureFault::BlockStopStall] {
        let fixture = owner_fixture(fault, false);
        let stop = fixture.stop.clone();
        let (_temp, _run, platform, _store, outcome) = finish_owner(fixture, false);
        assert!(stop.requested.load(Ordering::SeqCst));
        assert_block_owner_held(&platform, &outcome);
        assert_eq!(
            platform
                .writes
                .lock()
                .unwrap()
                .iter()
                .filter(|wire| wire[3] == 4)
                .count(),
            1
        );
        let records = authenticated_records(std::path::Path::new(&outcome.journal_path));
        assert_eq!(
            records
                .iter()
                .filter(
                    |record| record["kind"] == "checkpoint" && record["purpose"] == "vendor Block"
                )
                .count(),
            2 // original combined checkpoint exists, but the second native offer is forbidden
        );
        if fault == FixtureFault::BlockStopStall {
            assert_eq!(outcome.view.host_accepted_blocks, 0);
            assert_eq!(platform.trace.lock().unwrap().pair_retirements, 0);
            let trace = platform.trace.lock().unwrap();
            let intent = outcome.view.retained_block_outcome.as_ref().unwrap()["original_intent"]
                .as_u64()
                .unwrap();
            let waits = trace
                .journal_waits
                .iter()
                .filter(|(seen, _)| *seen == intent)
                .collect::<Vec<_>>();
            assert!(waits.len() >= 2);
            assert!(waits.iter().all(|(_, deadline)| *deadline == waits[0].1));
            drop(trace);
            assert!(
                outcome
                    .view
                    .diagnostic
                    .as_ref()
                    .unwrap()
                    .contains("exceeded host budget")
            );
            assert!(
                outcome.view.retained_block_outcome.as_ref().unwrap()["original_intent"]
                    .as_u64()
                    .is_some()
            );
            assert!(!records.iter().any(|record| record["kind"] == "failed"));
        } else {
            assert_eq!(outcome.view.host_accepted_blocks, 1);
            assert_eq!(platform.trace.lock().unwrap().pair_retirements, 1);
        }
    }
}
#[test]
#[ignore = "requires separately pinned image; original raw source, capacity fallback and sync-time unsolicited traffic, MockSimulation only"]
fn actual_block_raw_fault_capacity_and_sync_arrival_preserve_evidence_without_advancing() {
    for fault in [
        FixtureFault::BlockDrainError,
        FixtureFault::BlockMalformed,
        FixtureFault::BlockStale,
        FixtureFault::BlockPreoffer,
        FixtureFault::BlockPublication,
        FixtureFault::BlockCapacity,
        FixtureFault::BlockRejected,
        FixtureFault::BlockRawDuringSync,
    ] {
        let (_temp, _run, platform, _store, outcome) =
            finish_owner(owner_fixture(fault, false), false);
        assert_block_owner_held(&platform, &outcome);
        let accepted = if fault == FixtureFault::BlockRawDuringSync {
            1
        } else {
            0
        };
        assert_eq!(outcome.view.host_accepted_blocks, accepted);
        assert_eq!(
            platform.trace.lock().unwrap().pair_retirements,
            accepted as usize
        );
        let writes = platform.writes.lock().unwrap();
        assert_eq!(
            writes.iter().filter(|wire| wire[3] == 4).count(),
            usize::from(fault != FixtureFault::BlockPreoffer)
        );
        let records = authenticated_records(std::path::Path::new(&outcome.journal_path));
        let block = block_records(&records);
        let raw = block
            .iter()
            .filter(|record| record["kind"] == "raw-chunk")
            .map(|record| &record["observation"])
            .collect::<Vec<_>>();
        assert!(!raw.is_empty(), "{fault:?}");
        if fault == FixtureFault::BlockCapacity {
            assert_eq!(raw.len(), 65);
            let sequences = raw
                .iter()
                .map(|raw| raw["sequence"].as_u64().unwrap())
                .collect::<Vec<_>>();
            assert!(sequences.windows(2).all(|pair| pair[1] == pair[0] + 1));
            assert_eq!(raw[0]["raw"], serde_json::json!([240]));
            assert_eq!(raw[64]["raw"], serde_json::json!([247]));
        }
        if fault == FixtureFault::BlockRawDuringSync {
            // The previous pair is acknowledged with the next checkpoint. Raw
            // arriving during that sync belongs after that checkpoint, and the
            // exact live frontier fence forbids the second native invocation.
            assert_eq!(raw.len(), 1);
            assert!(
                records.iter().any(|record| record["kind"] == "raw-chunk"
                    && record["observation"]["sequence"] == 5)
            );
            assert!(
                outcome
                    .view
                    .diagnostic
                    .as_ref()
                    .unwrap()
                    .contains("Actual receive frontier changed after the durable offer checkpoint")
            );
        }
        if fault == FixtureFault::BlockStale {
            assert_eq!(raw[0]["generation"], 1);
        }
        if fault == FixtureFault::BlockPublication {
            let block_start = records
                .iter()
                .position(|record| std::ptr::eq(record, block[0]))
                .unwrap();
            let preceding = &records[..block_start];
            let preceding_raw = preceding
                .iter()
                .filter(|record| record["kind"] == "raw-chunk")
                .map(|record| &record["observation"])
                .collect::<Vec<_>>();
            assert_eq!(preceding_raw.len(), 3);
            assert_eq!(
                preceding_raw
                    .iter()
                    .map(|raw| raw["sequence"].as_u64().unwrap())
                    .collect::<Vec<_>>(),
                [1, 2, 3]
            );
            assert_eq!(preceding_raw[0]["raw"][3], 1);
            assert_eq!(preceding_raw[0]["raw"].as_array().unwrap().len(), 39);
            assert_eq!(
                preceding_raw[1]["raw"],
                serde_json::json!([240, 126, 72, 3, 127, 247])
            );
            assert_eq!(
                preceding_raw[2]["raw"],
                serde_json::json!([240, 126, 72, 3, 127, 247])
            );
            assert_eq!(
                preceding
                    .iter()
                    .filter(|record| record["kind"] == "vendor-action-pair")
                    .map(|record| (
                        record["receipt"]["phase"].as_str().unwrap(),
                        record["raw_frontier"].as_u64().unwrap()
                    ))
                    .collect::<Vec<_>>(),
                [
                    ("Identity", 1),
                    ("Announcement", 1),
                    ("SectionRequest", 2),
                    ("Descriptor", 3)
                ]
            );
            let prefix_frontier = preceding_raw.last().unwrap()["sequence"].as_u64().unwrap();
            let write = block
                .iter()
                .find(|record| record["kind"] == "write-syscall")
                .unwrap();
            assert_eq!(write["observation"]["before_sequence"], prefix_frontier);
            assert_eq!(write["observation"]["after_sequence"], prefix_frontier + 66);
            // The 65th block-local arrival fails publication after the preceding
            // three controls. All 64 queued originals survive; arrival 66 only
            // advances allocation, leaving an explicit unaccounted frontier.
            assert_eq!(raw.len(), 64);
            for (index, observation) in raw.iter().enumerate() {
                assert_eq!(observation["sequence"], prefix_frontier + index as u64 + 1);
                assert_eq!(observation["raw"], serde_json::json!([240, 247]));
                for field in ["epoch", "generation", "binding_sha256", "clock_origin"] {
                    assert_eq!(observation[field], preceding_raw[0][field]);
                }
            }
            assert!(
                !block
                    .iter()
                    .any(|record| record["kind"] == "vendor-action-pair")
            );
            let retired = outcome.view.reader_retirement_diagnostics.last().unwrap();
            assert_eq!(
                retired["first_publication_failure"]["sequence"],
                prefix_frontier + 65
            );
            assert_eq!(
                retired["first_publication_failure"]["raw"],
                serde_json::json!([240, 247])
            );
            for field in ["epoch", "generation", "binding_sha256", "clock_origin"] {
                assert_eq!(
                    retired["first_publication_failure"][field],
                    preceding_raw[0][field]
                );
            }
            assert_eq!(retired["source"]["allocated"], prefix_frontier + 66);
            assert_eq!(retired["source"]["processed"], prefix_frontier + 64);
            assert_eq!(outcome.view.allocated_raw_frontier, prefix_frontier + 66);
            assert_eq!(retired["accounted"], false);
        }
        if fault == FixtureFault::BlockRejected {
            assert_eq!(
                raw[0]["raw"],
                serde_json::json!(RETAINED_DESCRIPTOR_REJECTION)
            );
        }
    }
}

#[test]
#[ignore = "requires pinned image; explicit combined Stop and lost reply, MockSimulation only"]
fn actual_block_fusion_stop_before_submit_before_sync_and_lost_reply_remain_held() {
    for fault in [
        FixtureFault::BlockStopBeforeFusionSubmit,
        FixtureFault::BlockStopBeforeFusionCommit,
        FixtureFault::BlockLostReply,
    ] {
        let fixture = owner_fixture(fault, false);
        let stop = fixture.stop.clone();
        let (_temp, _run, platform, _store, outcome) = finish_owner(fixture, false);
        assert_block_owner_held(&platform, &outcome);
        assert_eq!(
            platform
                .writes
                .lock()
                .unwrap()
                .iter()
                .filter(|wire| wire[3] == 4)
                .count(),
            1
        );
        let records = authenticated_records(std::path::Path::new(&outcome.journal_path));
        let checkpoints = records
            .iter()
            .filter(|record| record["kind"] == "checkpoint" && record["purpose"] == "vendor Block")
            .count();
        match fault {
            FixtureFault::BlockStopBeforeFusionSubmit => {
                assert!(stop.requested.load(Ordering::SeqCst));
                assert_eq!(checkpoints, 1);
                assert_eq!(outcome.view.host_accepted_blocks, 1);
                assert_eq!(platform.trace.lock().unwrap().pair_retirements, 1);
            }
            FixtureFault::BlockStopBeforeFusionCommit => {
                assert!(stop.requested.load(Ordering::SeqCst));
                assert_eq!(
                    platform.trace.lock().unwrap().before_fusion_sync_accepted,
                    Some(0)
                );
                assert_eq!(checkpoints, 2);
                assert_eq!(outcome.view.host_accepted_blocks, 1);
                assert_eq!(platform.trace.lock().unwrap().pair_retirements, 1);
            }
            FixtureFault::BlockLostReply => {
                assert!(!stop.requested.load(Ordering::SeqCst));
                assert_eq!(checkpoints, 2); // readable frames cannot restore a live acknowledgement
                assert_eq!(outcome.view.host_accepted_blocks, 0);
                assert_eq!(platform.trace.lock().unwrap().pair_retirements, 0);
                let retained = outcome.view.retained_block_outcome.as_ref().unwrap();
                assert!(retained["original_intent"].as_u64().is_some());
                assert_eq!(retained["original_combined_checkpoint"]["ordinal"], 7);
                assert!(retained["provisional_completed_receipt"].is_object());
                assert!(
                    outcome
                        .view
                        .diagnostic
                        .as_ref()
                        .unwrap()
                        .contains("exceeded host budget")
                );
                let trace = platform.trace.lock().unwrap();
                let intent = retained["original_intent"].as_u64().unwrap();
                let waits = trace
                    .journal_waits
                    .iter()
                    .filter(|(seen, _)| *seen == intent)
                    .collect::<Vec<_>>();
                assert!(waits.len() >= 2);
                assert!(waits.iter().all(|wait| **wait == *waits[0]));
                assert!(!records.iter().any(|record| record["kind"] == "failed"));
            }
            _ => unreachable!(),
        }
    }
}
