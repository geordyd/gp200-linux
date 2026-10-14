use super::*;
use gp200_domain::{AssetId, FirmwareBackupAssessment, RetainedJobBlob};
use gp200_protocol::FirmwareBlock;
use std::sync::Arc;

fn checked_review(bytes: Vec<u8>) -> FirmwareReview {
    let hash = JobSha256::of(&bytes);
    FirmwareReview::inspect(
        RetainedJobBlob::checked(
            AssetId("offline-vendor-test".into()),
            Arc::from(bytes),
            hash,
        )
        .unwrap(),
        None,
        &FirmwareBackupAssessment::default(),
    )
    .unwrap()
}
fn fixture_review(lengths: [usize; 4]) -> FirmwareReview {
    let mut bytes = vec![0_u8; 92 + lengths.iter().sum::<usize>()];
    bytes[..4].copy_from_slice(b"FRMW");
    let payload_length = bytes.len() as u32 - 48;
    bytes[4..8].copy_from_slice(&payload_length.to_le_bytes());
    bytes[16..22].copy_from_slice(b"GP-200");
    bytes[88..92].copy_from_slice(b"V182");
    let mut start = 92;
    for (section, length) in lengths.into_iter().enumerate() {
        bytes[48 + section * 8..52 + section * 8]
            .copy_from_slice(&((start - 48) as u32).to_le_bytes());
        bytes[52 + section * 8..56 + section * 8].copy_from_slice(&(length as u32).to_le_bytes());
        for (at, byte) in bytes[start..start + length].iter_mut().enumerate() {
            *byte = (at.wrapping_mul(17) + section) as u8;
        }
        start += length;
    }
    bytes[14] = bytes[48..].iter().fold(0_u8, |sum, b| sum.wrapping_add(*b));
    checked_review(bytes)
}
fn engine() -> VendorSequence {
    VendorSequence::from_checked_review(fixture_review([259, 5, 7, 3]), 11, 0).unwrap()
}
fn identity(chunk: u16) -> Vec<u8> {
    let mut raw = vec![0xf0, 0x7e, 0x48, 1];
    let mut product = [0_u8; 16];
    product[..6].copy_from_slice(b"GP-200");
    raw.extend(product);
    raw.extend([b'B'; 16]);
    raw.extend([(chunk >> 4) as u8, (chunk & 15) as u8, 0xf7]);
    raw
}
fn status() -> Vec<u8> {
    vec![0xf0, 0x7e, 0x48, 3, 0x7f, 0xf7]
}
fn frame(e: &VendorSequence, at_ms: u64, raw: Vec<u8>) -> VendorFrameObservation {
    VendorFrameObservation {
        generation: e.generation,
        sequence: e.last_frame_sequence + 1,
        at_ms,
        raw,
    }
}
fn action(e: &mut VendorSequence, clock: &mut u64) -> VendorAction {
    if let VendorState::Ready { not_before_ms, .. } = e.state() {
        *clock = (*clock).max(not_before_ms);
    }
    e.poll_action(*clock).unwrap().unwrap().clone()
}
fn offer(e: &mut VendorSequence, clock: &mut u64) -> VendorAction {
    let a = action(e, clock);
    e.begin_offer(a.ordinal(), *clock).unwrap();
    a
}
fn full(e: &mut VendorSequence, a: &VendorAction, clock: &mut u64) {
    *clock += 1;
    e.finish_write(
        a.ordinal(),
        VendorWriteReceipt {
            at_ms: *clock,
            extent: VendorWriteExtent::Known(a.wire().len()),
            drain: VendorDrain::Returned,
        },
    )
    .unwrap();
}
fn reply(e: &mut VendorSequence, clock: &mut u64, raw: Vec<u8>) {
    *clock += 1;
    let f = frame(e, *clock, raw);
    e.receive(f).unwrap();
}
fn complete_identity(e: &mut VendorSequence, clock: &mut u64, chunk: u16) {
    let a = offer(e, clock);
    full(e, &a, clock);
    reply(e, clock, identity(chunk));
    let receipt = e.take_completed().unwrap();
    assert!(
        matches!(receipt.decision, VendorDecision::BootIdentity { chunk_bytes, .. } if chunk_bytes == chunk)
    );
}
fn to_first_block(e: &mut VendorSequence, clock: &mut u64, chunk: u16) {
    complete_identity(e, clock, chunk);
    for phase in [
        VendorPhase::Announcement,
        VendorPhase::SectionRequest,
        VendorPhase::Descriptor,
    ] {
        let a = offer(e, clock);
        assert_eq!(a.phase(), phase);
        full(e, &a, clock);
        if phase != VendorPhase::Announcement {
            reply(e, clock, status());
        }
        e.take_completed().unwrap();
    }
}
fn replay(mut e: VendorSequence, chunk: u16) -> Vec<VendorAction> {
    let mut clock = 0;
    let mut actions = Vec::new();
    while !matches!(e.state(), VendorState::HostFinishedUnverified) {
        let a = offer(&mut e, &mut clock);
        full(&mut e, &a, &mut clock);
        match a.phase() {
            VendorPhase::Identity => reply(&mut e, &mut clock, identity(chunk)),
            VendorPhase::Announcement | VendorPhase::FinishControl => {}
            VendorPhase::TerminalQuery => {
                reply(
                    &mut e,
                    &mut clock,
                    vec![0xf0, 0x7e, 0x48, 3, 0x0a, 0x0b, 0xf7],
                );
                assert!(e.take_completed().is_none());
                reply(
                    &mut e,
                    &mut clock,
                    vec![0xf0, 0x7e, 0x48, 7, 0x0c, 0x0d, 0xf7],
                );
            }
            _ => reply(&mut e, &mut clock, status()),
        }
        let receipt = e.take_completed().unwrap();
        assert_eq!(receipt.action, a);
        assert_eq!(
            receipt.write.as_ref().unwrap().extent,
            VendorWriteExtent::Known(a.wire().len())
        );
        if a.phase() == VendorPhase::FinishControl {
            assert_eq!(receipt.decision, VendorDecision::HostFinishedUnverified);
        }
        actions.push(a);
    }
    assert_eq!(e.cancel(), Err(VendorFailure::AlreadyFinished));
    assert_eq!(e.state(), VendorState::HostFinishedUnverified);
    actions
}

#[test]
fn nonempty_sections_repeat_announcement_and_never_request_terminal_selector() {
    let actions = replay(engine(), 2);
    let requests: Vec<_> = actions
        .iter()
        .filter(|a| a.phase() == VendorPhase::SectionRequest)
        .map(|a| a.wire()[4])
        .collect();
    assert_eq!(requests, [0, 1, 2, 0x10]);
    assert_eq!(
        actions
            .iter()
            .filter(|a| a.phase() == VendorPhase::Announcement)
            .count(),
        4
    );
    assert_eq!(
        actions
            .iter()
            .filter(|a| a.phase() == VendorPhase::Descriptor)
            .count(),
        4
    );
    assert_eq!(
        actions
            .iter()
            .filter(|a| a.phase() == VendorPhase::SectionTerminator)
            .count(),
        4
    );
    let tail: Vec<_> = actions.iter().rev().take(3).map(|a| a.phase()).collect();
    assert_eq!(
        tail,
        [
            VendorPhase::FinishControl,
            VendorPhase::TerminalQuery,
            VendorPhase::SectionTerminator
        ]
    );
    assert!(
        actions
            .iter()
            .enumerate()
            .all(|(n, a)| a.ordinal() == n as u64)
    );
}
fn assert_native_payload_order(actions: &[VendorAction], review: &FirmwareReview, chunk: u16) {
    // Independent raw-table expectation from the native parser and callers.
    let mut absolute = 0;
    for (section, (slot, selector)) in [(0, 0), (2, 1), (3, 2), (1, 0x10)].into_iter().enumerate() {
        let raw_section = &review.summary().sections[slot];
        let section_actions: Vec<_> = actions
            .iter()
            .filter(|a| a.section() == Some(section as u8))
            .collect();
        assert!(
            section_actions
                .iter()
                .all(|a| a.selector() == Some(selector))
        );
        let descriptor = section_actions
            .iter()
            .find(|a| a.phase() == VendorPhase::Descriptor)
            .unwrap();
        let descriptor = FirmwareDescriptor::decode(descriptor.wire()).unwrap();
        let count = raw_section.bytes.div_ceil(usize::from(chunk));
        assert_eq!(descriptor.section_bytes() as usize, raw_section.bytes);
        assert_eq!(descriptor.block_count() as usize, count);
        let blocks: Vec<_> = section_actions
            .iter()
            .filter(|a| a.phase() == VendorPhase::Block)
            .collect();
        assert_eq!(blocks.len(), count);
        let mut content = Vec::with_capacity(raw_section.bytes);
        for (index, action) in blocks.into_iter().enumerate() {
            let source = action.block().unwrap();
            let local = index * usize::from(chunk);
            let bytes = (raw_section.bytes - local).min(usize::from(chunk));
            assert_eq!(source.file_offset, raw_section.file_offset + local);
            assert_eq!(source.content_bytes, bytes);
            assert_eq!(source.padded_bytes, usize::from(chunk) - bytes);
            assert_eq!(source.absolute_block_ordinal, absolute);
            assert_eq!(source.block_in_section, index as u32);
            let decoded = FirmwareBlock::decode(action.wire(), chunk).unwrap();
            assert_eq!(decoded.block_index_mod128, (index % 128) as u8);
            let expected = &review.source()[source.file_offset..source.file_offset + bytes];
            assert_eq!(&decoded.data_padded[..bytes], expected);
            assert!(decoded.data_padded[bytes..].iter().all(|b| *b == 0xff));
            assert_eq!(source.content_sha256, JobSha256::of(expected).hex());
            content.extend_from_slice(&decoded.data_padded[..bytes]);
            absolute += 1;
        }
        assert_eq!(content, review.section(slot).unwrap());
        assert_eq!(JobSha256::of(&content), raw_section.sha256);
    }
}
#[test]
fn native_section_mapping_drives_every_descriptor_and_payload_for_varied_chunks() {
    let review = fixture_review([259, 5, 7, 3]);
    for chunk in [1, 2, 7, 180, 255] {
        let actions = replay(
            VendorSequence::from_checked_review(review.clone(), 11, 0).unwrap(),
            chunk,
        );
        assert_native_payload_order(&actions, &review, chunk);
    }
}
#[test]
fn wrap_padding_and_absolute_source_provenance_survive_repeated_unkeyed_status() {
    let review = fixture_review([259, 5, 7, 3]);
    let actions = replay(
        VendorSequence::from_checked_review(review.clone(), 11, 0).unwrap(),
        2,
    );
    let blocks: Vec<_> = actions
        .iter()
        .filter(|a| a.phase() == VendorPhase::Block)
        .collect();
    assert_eq!(blocks[127].block().unwrap().index_mod128, 127);
    assert_eq!(blocks[128].block().unwrap().index_mod128, 0);
    assert_eq!(blocks[128].block().unwrap().block_in_section, 128);
    assert_eq!(blocks[128].block().unwrap().absolute_block_ordinal, 128);
    assert_eq!(blocks[130].block().unwrap().block_in_section, 0);
    assert_eq!(blocks[130].block().unwrap().absolute_block_ordinal, 130);
    let last = blocks[129];
    let source = last.block().unwrap();
    assert_eq!((source.content_bytes, source.padded_bytes), (1, 1));
    let decoded = FirmwareBlock::decode(last.wire(), 2).unwrap();
    assert_eq!(
        decoded.data_padded,
        [review.source()[source.file_offset], 0xff]
    );
    assert_eq!(source.padding_sha256, JobSha256::of(&[0xff]).hex());
    assert_eq!(last.wire_sha256(), JobSha256::of(last.wire()).hex());
    assert!(
        blocks
            .windows(2)
            .all(|pair| pair[0].block().unwrap().absolute_block_ordinal + 1
                == pair[1].block().unwrap().absolute_block_ordinal)
    );
}
#[test]
fn announcement_gap_is_after_successful_drain_and_receipt_retirement() {
    let mut e = engine();
    let mut clock = 0;
    complete_identity(&mut e, &mut clock, 2);
    let a = offer(&mut e, &mut clock);
    full(&mut e, &a, &mut clock);
    assert!(e.poll_action(clock + 30).unwrap().is_none());
    e.take_completed().unwrap();
    assert!(e.poll_action(clock + 29).is_err()); // Earlier clock cannot be resurrected.
    let mut e = engine();
    let mut clock = 0;
    complete_identity(&mut e, &mut clock, 2);
    let a = offer(&mut e, &mut clock);
    full(&mut e, &a, &mut clock);
    e.take_completed().unwrap();
    assert!(e.poll_action(clock + 29).unwrap().is_none());
    assert_eq!(
        e.poll_action(clock + 30).unwrap().unwrap().phase(),
        VendorPhase::SectionRequest
    );
}
#[test]
fn reply_during_write_is_buffered_and_cannot_advance_until_drain() {
    let mut e = engine();
    let mut clock = 0;
    let a = offer(&mut e, &mut clock);
    reply(&mut e, &mut clock, identity(2));
    assert!(matches!(e.state(), VendorState::Writing { .. }));
    assert_eq!(e.negotiated_chunk(), None);
    assert!(e.take_completed().is_none());
    full(&mut e, &a, &mut clock);
    assert_eq!(e.negotiated_chunk(), Some(2));
    let receipt = e.take_completed().unwrap();
    assert!(receipt.frames[0].at_ms < receipt.write.unwrap().at_ms);
}
#[test]
fn plausible_buffered_reply_never_overrides_partial_unknown_error_or_failed_drain() {
    let cases = [
        (
            VendorWriteExtent::Known(0),
            VendorDrain::NotAttempted,
            VendorFailure::PartialOrUnknownWrite,
        ),
        (
            VendorWriteExtent::Known(4),
            VendorDrain::Returned,
            VendorFailure::PartialOrUnknownWrite,
        ),
        (
            VendorWriteExtent::Unknown,
            VendorDrain::Returned,
            VendorFailure::PartialOrUnknownWrite,
        ),
        (
            VendorWriteExtent::Failed {
                known_bytes: Some(5),
            },
            VendorDrain::Returned,
            VendorFailure::PartialOrUnknownWrite,
        ),
        (
            VendorWriteExtent::Known(5),
            VendorDrain::Failed,
            VendorFailure::DrainNotReturned,
        ),
        (
            VendorWriteExtent::Known(5),
            VendorDrain::NotAttempted,
            VendorFailure::DrainNotReturned,
        ),
    ];
    for (extent, drain, expected) in cases {
        let mut e = engine();
        let mut clock = 0;
        let a = offer(&mut e, &mut clock);
        reply(&mut e, &mut clock, identity(2));
        let receipt = VendorWriteReceipt {
            at_ms: clock + 1,
            extent,
            drain,
        };
        assert_eq!(
            e.finish_write(a.ordinal(), receipt.clone()),
            Err(expected.clone())
        );
        assert_eq!(e.negotiated_chunk(), None);
        let stopped = e.take_completed().unwrap();
        assert_eq!(stopped.write, Some(receipt));
        assert_eq!(stopped.frames.len(), 1);
        assert_eq!(stopped.decision, VendorDecision::Stopped(expected));
        assert_eq!(e.poll_action(clock + 2), Err(VendorFailure::AlreadyStopped));
    }
}
#[test]
fn pre_offer_old_generation_and_missing_frame_are_rejected_with_raw_evidence() {
    for variant in 0..3 {
        let mut e = engine();
        let mut f = frame(&e, 1, status());
        let expected = match variant {
            0 => VendorFailure::ReceiveBeforeOffer,
            1 => {
                f.generation = 10;
                VendorFailure::WrongGeneration
            }
            _ => {
                f.sequence = 2;
                VendorFailure::ReceiveSequenceGap
            }
        };
        assert_eq!(e.receive(f.clone()), Err(expected.clone()));
        assert_eq!(e.rejected_frame().unwrap().raw.as_ref(), Some(&f.raw));
        assert_eq!(
            e.state(),
            VendorState::Stopped {
                reason: expected,
                any_offer_attempted: false
            }
        );
    }
}
#[test]
fn malformed_global_error_wrong_identity_and_unknown_chunk_stop() {
    let mut bad_product = identity(2);
    bad_product[4] = b'X';
    let cases = [
        (
            vec![0xf0, 0x7e, 0x48, 1, 0xf7],
            VendorFailure::InvalidIdentity,
        ),
        (identity(0), VendorFailure::InvalidIdentity),
        (bad_product, VendorFailure::ProductMismatch),
        (
            vec![0xf0, 0x7e, 0x48, 6, 0xf7],
            VendorFailure::NativeGlobalError,
        ),
        (
            vec![0xf0, 0x7e, 0x48, 1, 0x80, 0xf7],
            VendorFailure::MalformedFrame,
        ),
    ];
    for (raw, expected) in cases {
        let mut e = engine();
        let mut clock = 0;
        let a = offer(&mut e, &mut clock);
        full(&mut e, &a, &mut clock);
        let f = frame(&e, clock + 1, raw.clone());
        assert_eq!(e.receive(f), Err(expected.clone()));
        let receipt = e.take_completed().unwrap();
        assert_eq!(receipt.frames[0].raw, raw);
        assert_eq!(receipt.decision, VendorDecision::Stopped(expected));
    }
}
#[test]
fn repeated_reply_within_one_offer_stops_without_double_advance() {
    let mut e = engine();
    let mut clock = 0;
    to_first_block(&mut e, &mut clock, 2);
    let a = offer(&mut e, &mut clock);
    reply(&mut e, &mut clock, status());
    reply(&mut e, &mut clock, status());
    let receipt = VendorWriteReceipt {
        at_ms: clock + 1,
        extent: VendorWriteExtent::Known(a.wire().len()),
        drain: VendorDrain::Returned,
    };
    assert_eq!(
        e.finish_write(a.ordinal(), receipt),
        Err(VendorFailure::ExtraTraffic)
    );
    assert_eq!(e.accepted_blocks(), 0);
    assert_eq!(e.take_completed().unwrap().frames.len(), 2);
}
#[test]
fn extra_reply_across_action_boundary_cannot_advance_new_cursor() {
    let mut e = engine();
    let mut clock = 0;
    to_first_block(&mut e, &mut clock, 2);
    let a = offer(&mut e, &mut clock);
    full(&mut e, &a, &mut clock);
    reply(&mut e, &mut clock, status());
    assert_eq!(e.accepted_blocks(), 1);
    let f = frame(&e, clock + 1, status());
    assert_eq!(e.receive(f), Err(VendorFailure::ReceiveBeforeOffer));
    assert_eq!(e.accepted_blocks(), 1);
    assert!(e.take_completed().is_some());
}
#[test]
fn phase_relative_status_does_not_require_opcode_or_echoed_ordinal() {
    let mut e = engine();
    let mut clock = 0;
    complete_identity(&mut e, &mut clock, 2);
    let a = offer(&mut e, &mut clock);
    full(&mut e, &a, &mut clock);
    e.take_completed().unwrap();
    let a = offer(&mut e, &mut clock);
    full(&mut e, &a, &mut clock);
    reply(&mut e, &mut clock, vec![0xf0, 0x7e, 0x48, 0x55, 0x01, 0xf7]);
    e.take_completed().unwrap();
    let a = offer(&mut e, &mut clock);
    assert_eq!(a.phase(), VendorPhase::Descriptor);
    full(&mut e, &a, &mut clock);
    reply(&mut e, &mut clock, status());
    e.take_completed().unwrap();
    assert_eq!(action(&mut e, &mut clock).phase(), VendorPhase::Block);
}
#[test]
fn rejected_status_does_not_retry_or_consume_block() {
    let mut e = engine();
    let mut clock = 0;
    to_first_block(&mut e, &mut clock, 2);
    let a = offer(&mut e, &mut clock);
    full(&mut e, &a, &mut clock);
    let f = frame(&e, clock + 1, vec![0xf0, 0x7e, 0x48, 4, 0x7e, 0xf7]);
    assert_eq!(e.receive(f), Err(VendorFailure::UnexpectedTraffic));
    assert_eq!(e.accepted_blocks(), 0);
    e.take_completed().unwrap();
    assert_eq!(e.poll_action(clock + 2), Err(VendorFailure::AlreadyStopped));
}
#[test]
fn captured_status_rejects_corrected_second_descriptor_without_retry_or_later_control() {
    // Synthetic source bytes exercise the observed counts and packets without
    // requiring the retained firmware artifact or making any physical offers.
    let mut e = VendorSequence::from_checked_review(
        fixture_review([4_192_632, 1_834_032, 294_912, 131_072]),
        11,
        0,
    )
    .unwrap();
    let mut clock = 0;
    let captured_identity = vec![
        0xf0, 0x7e, 0x48, 1, 0x47, 0x50, 0x2d, 0x32, 0x30, 0x30, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0x56, 0x31, 0x2e, 0x30, 0x2e, 0x36, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x0b, 4, 0xf7,
    ];
    let a = offer(&mut e, &mut clock);
    assert_eq!(a.phase(), VendorPhase::Identity);
    full(&mut e, &a, &mut clock);
    reply(&mut e, &mut clock, captured_identity.clone());
    assert_eq!(
        e.take_completed().unwrap().decision,
        VendorDecision::BootIdentity {
            raw: captured_identity,
            chunk_bytes: 180,
        }
    );
    let mut offered_actions = 1_u64;
    let mut first_section_content = 0_usize;
    let mut saw_second_request = false;
    while e.phase != VendorPhase::Descriptor || e.section != 1 {
        let a = offer(&mut e, &mut clock);
        offered_actions += 1;
        assert!(offered_actions < 23_301);
        assert!(!matches!(
            a.phase(),
            VendorPhase::TerminalQuery | VendorPhase::FinishControl
        ));
        if let Some(block) = a.block() {
            assert_eq!(a.section(), Some(0));
            first_section_content += block.content_bytes;
        }
        if a.phase() == VendorPhase::SectionRequest && a.section() == Some(1) {
            assert_eq!(a.wire(), [0xf0, 0x7e, 0x48, 0x7f, 1, 0xf7]);
            saw_second_request = true;
        }
        full(&mut e, &a, &mut clock);
        if a.phase() != VendorPhase::Announcement {
            reply(&mut e, &mut clock, status());
        }
        e.take_completed().unwrap();
    }
    assert!(saw_second_request);
    assert_eq!(e.accepted_blocks(), 23_293);
    assert_eq!(first_section_content, 4_192_632);
    let descriptor = offer(&mut e, &mut clock);
    offered_actions += 1;
    assert_eq!(offered_actions, 23_301);
    assert_eq!(descriptor.ordinal(), 23_300);
    assert_eq!(descriptor.phase(), VendorPhase::Descriptor);
    assert_eq!(descriptor.section(), Some(1));
    assert_eq!(descriptor.selector(), Some(1));
    assert_eq!(
        descriptor.wire(),
        [
            0xf0, 0x7e, 0x48, 2, 0, 0, 0, 4, 8, 0, 0, 0, 0, 0, 0, 0, 0, 6, 6, 7, 0xf7,
        ]
    );
    full(&mut e, &descriptor, &mut clock);
    clock += 1;
    let captured_reply = frame(&e, clock, vec![0xf0, 0x7e, 0x48, 3, 0x7e, 0xf7]);
    let expected = VendorFailure::DescriptorStatusRejected {
        section_one_based: 2,
        selector: 1,
        status: 0x7e,
    };
    assert_eq!(e.receive(captured_reply.clone()), Err(expected.clone()));
    assert_eq!(
        expected.to_string(),
        "Descriptor response rejected for section 2 (selector 0x01): received status 0x7e, expected 0x7f"
    );
    assert_eq!(
        e.state(),
        VendorState::Stopped {
            reason: expected.clone(),
            any_offer_attempted: true,
        }
    );
    let rejected = e.rejected_frame().unwrap();
    assert_eq!(rejected.generation, captured_reply.generation);
    assert_eq!(rejected.sequence, captured_reply.sequence);
    assert_eq!(rejected.at_ms, captured_reply.at_ms);
    assert_eq!(rejected.original_bytes, 6);
    assert_eq!(rejected.raw.as_deref(), Some(captured_reply.raw.as_slice()));
    let receipt = e.take_completed().unwrap();
    assert_eq!(receipt.action, descriptor);
    assert_eq!(receipt.frames, [captured_reply]);
    assert_eq!(receipt.decision, VendorDecision::Stopped(expected));
    assert_eq!(receipt.write.unwrap().extent, VendorWriteExtent::Known(21));
    assert_eq!(e.accepted_blocks(), 23_293);
    // Retiring evidence, polling, and attempting a resend cannot authorize a
    // second-section block, another descriptor, or any terminal/control packet.
    assert_eq!(e.poll_action(clock + 1), Err(VendorFailure::AlreadyStopped));
    assert_eq!(
        e.begin_offer(descriptor.ordinal(), clock + 1),
        Err(VendorFailure::AlreadyStopped)
    );
    assert_eq!(e.tick(clock + 1), Err(VendorFailure::AlreadyStopped));
    assert!(e.take_completed().is_none());
    assert_eq!(e.accepted_blocks(), 23_293);
}

#[test]
fn descriptor_rejection_buffered_during_write_preserves_frame_and_drain_receipt() {
    let mut e = engine();
    let mut clock = 0;
    complete_identity(&mut e, &mut clock, 180);
    for phase in [VendorPhase::Announcement, VendorPhase::SectionRequest] {
        let a = offer(&mut e, &mut clock);
        assert_eq!(a.phase(), phase);
        full(&mut e, &a, &mut clock);
        if phase != VendorPhase::Announcement {
            reply(&mut e, &mut clock, status());
        }
        e.take_completed().unwrap();
    }
    let descriptor = offer(&mut e, &mut clock);
    assert_eq!(descriptor.phase(), VendorPhase::Descriptor);
    clock += 1;
    let captured_reply = frame(&e, clock, vec![0xf0, 0x7e, 0x48, 3, 0x7e, 0xf7]);
    e.receive(captured_reply.clone()).unwrap();
    assert!(matches!(e.state(), VendorState::Writing { .. }));
    assert!(e.rejected_frame().is_none());
    assert!(e.take_completed().is_none());
    clock += 1;
    let write = VendorWriteReceipt {
        at_ms: clock,
        extent: VendorWriteExtent::Known(descriptor.wire().len()),
        drain: VendorDrain::Returned,
    };
    let expected = VendorFailure::DescriptorStatusRejected {
        section_one_based: 1,
        selector: 0,
        status: 0x7e,
    };
    assert_eq!(
        e.finish_write(descriptor.ordinal(), write.clone()),
        Err(expected.clone())
    );
    let receipt = e.take_completed().unwrap();
    assert_eq!(
        receipt.frames.as_slice(),
        std::slice::from_ref(&captured_reply)
    );
    assert_eq!(receipt.write, Some(write));
    assert_eq!(receipt.decision, VendorDecision::Stopped(expected));
    assert_eq!(
        e.rejected_frame().unwrap().raw.as_deref(),
        Some(captured_reply.raw.as_slice())
    );
    assert_eq!(e.accepted_blocks(), 0);
    assert_eq!(e.poll_action(clock + 1), Err(VendorFailure::AlreadyStopped));
}
fn to_terminal(e: &mut VendorSequence, clock: &mut u64) {
    while e.phase != VendorPhase::TerminalQuery {
        let a = offer(e, clock);
        full(e, &a, clock);
        match a.phase() {
            VendorPhase::Identity => reply(e, clock, identity(255)),
            VendorPhase::Announcement => {}
            _ => reply(e, clock, status()),
        }
        e.take_completed().unwrap();
    }
}
#[test]
fn terminal03_retains_data_without_resend_or_deadline_extension() {
    let mut e = engine();
    let mut clock = 0;
    to_terminal(&mut e, &mut clock);
    let a = offer(&mut e, &mut clock);
    full(&mut e, &a, &mut clock);
    let deadline = clock + VENDOR_ACTION_BUDGET_MS;
    reply(&mut e, &mut clock, vec![0xf0, 0x7e, 0x48, 3, 1, 2, 0xf7]);
    assert!(
        matches!(e.state(),VendorState::AwaitingReply{deadline_ms,..} if deadline_ms==deadline)
    );
    assert!(e.poll_action(clock).unwrap().is_none());
    assert_eq!(e.tick(deadline), Err(VendorFailure::DeadlineExpired));
    let receipt = e.take_completed().unwrap();
    assert_eq!(receipt.frames.len(), 1);
}
#[test]
fn terminal07_before_drain_is_buffered_and_final09_has_no_invented_ack() {
    let mut e = engine();
    let mut clock = 0;
    to_terminal(&mut e, &mut clock);
    let a = offer(&mut e, &mut clock);
    reply(&mut e, &mut clock, vec![0xf0, 0x7e, 0x48, 3, 1, 2, 0xf7]);
    reply(&mut e, &mut clock, vec![0xf0, 0x7e, 0x48, 7, 3, 4, 0xf7]);
    full(&mut e, &a, &mut clock);
    let receipt = e.take_completed().unwrap();
    assert_eq!(
        receipt
            .terminal_data
            .iter()
            .map(|data| (data.opcode, data.decoded.clone()))
            .collect::<Vec<_>>(),
        vec![(3, vec![0x12]), (7, vec![0x34])]
    );
    assert_eq!(
        receipt.decision,
        VendorDecision::TerminalDataAccepted {
            decoded: vec![0x34]
        }
    );
    let a = offer(&mut e, &mut clock);
    assert_eq!(a.wire(), [0xf0, 0x7e, 0x48, 9, 0xf7]);
    reply(&mut e, &mut clock, status());
    assert!(matches!(e.state(), VendorState::Writing { .. }));
    full(&mut e, &a, &mut clock);
    assert_eq!(e.state(), VendorState::HostFinishedUnverified);
    let receipt = e.take_completed().unwrap();
    assert_eq!(receipt.frames.len(), 1);
    assert_eq!(receipt.decision, VendorDecision::HostFinishedUnverified);
}
#[test]
fn malformed_terminal_data_cannot_offer09() {
    for raw in [
        vec![0xf0, 0x7e, 0x48, 7, 1, 0xf7],
        vec![0xf0, 0x7e, 0x48, 7, 0x10, 0, 0xf7],
        status(),
    ] {
        let mut e = engine();
        let mut clock = 0;
        to_terminal(&mut e, &mut clock);
        let a = offer(&mut e, &mut clock);
        full(&mut e, &a, &mut clock);
        let f = frame(&e, clock + 1, raw);
        assert_eq!(e.receive(f), Err(VendorFailure::UnexpectedTraffic));
        e.take_completed().unwrap();
        assert_eq!(e.poll_action(clock + 2), Err(VendorFailure::AlreadyStopped));
    }
}
#[test]
fn absolute_write_timeout_retains_late_host_receipt_and_never_retries() {
    let mut e = engine();
    let mut clock = 0;
    let a = offer(&mut e, &mut clock);
    let receipt = VendorWriteReceipt {
        at_ms: VENDOR_ACTION_BUDGET_MS,
        extent: VendorWriteExtent::Known(5),
        drain: VendorDrain::Returned,
    };
    assert_eq!(
        e.finish_write(a.ordinal(), receipt.clone()),
        Err(VendorFailure::DeadlineExpired)
    );
    assert_eq!(e.take_completed().unwrap().write, Some(receipt));
    assert_eq!(
        e.poll_action(VENDOR_ACTION_BUDGET_MS + 1),
        Err(VendorFailure::AlreadyStopped)
    );
}
#[test]
fn final_control_partial_drain_failure_is_indeterminate_host_stop() {
    let mut e = engine();
    let mut clock = 0;
    to_terminal(&mut e, &mut clock);
    let a = offer(&mut e, &mut clock);
    full(&mut e, &a, &mut clock);
    reply(&mut e, &mut clock, vec![0xf0, 0x7e, 0x48, 7, 0xf7]);
    e.take_completed().unwrap();
    let a = offer(&mut e, &mut clock);
    assert_eq!(
        e.finish_write(
            a.ordinal(),
            VendorWriteReceipt {
                at_ms: clock + 1,
                extent: VendorWriteExtent::Known(5),
                drain: VendorDrain::Failed
            }
        ),
        Err(VendorFailure::DrainNotReturned)
    );
    assert!(matches!(
        e.state(),
        VendorState::Stopped {
            any_offer_attempted: true,
            ..
        }
    ));
    assert_eq!(
        e.take_completed().unwrap().action.phase(),
        VendorPhase::FinishControl
    );
}
#[test]
fn cancelled_lost_reader_bad_clock_overflow_and_oversized_input_are_bounded_stops() {
    for lost in [false, true] {
        let mut e = engine();
        let mut clock = 0;
        offer(&mut e, &mut clock);
        let result = if lost {
            e.reader_or_binding_lost()
        } else {
            e.cancel()
        };
        assert!(result.is_err());
        assert!(matches!(
            e.state(),
            VendorState::Stopped {
                any_offer_attempted: true,
                ..
            }
        ));
        assert!(e.take_completed().is_some());
    }
    let mut e = engine();
    e.poll_action(2).unwrap();
    assert_eq!(e.poll_action(1), Err(VendorFailure::InvalidClock));
    let mut e = VendorSequence::from_checked_review(fixture_review([1; 4]), 11, u64::MAX).unwrap();
    let a = e.poll_action(u64::MAX).unwrap().unwrap().clone();
    assert_eq!(
        e.begin_offer(a.ordinal(), u64::MAX),
        Err(VendorFailure::DeadlineOverflow)
    );
    let mut e = engine();
    let mut clock = 0;
    offer(&mut e, &mut clock);
    let f = frame(&e, 1, vec![0; MAX_FRAME_BYTES + 1]);
    assert_eq!(e.receive(f), Err(VendorFailure::MalformedFrame));
    let rejected = e.rejected_frame().unwrap();
    assert_eq!(rejected.original_bytes, MAX_FRAME_BYTES + 1);
    assert!(rejected.raw.is_none());
}
#[test]
fn public_constructor_rejects_valid_but_unpinned_image() {
    assert!(matches!(
        VendorSequence::new(fixture_review([259, 5, 7, 3]), 11, 0),
        Err(VendorFailure::UnsupportedImage)
    ));
}
#[test]
#[ignore = "requires separately pinned local Valeton image via GP200_VENDOR_SEQUENCE_IMAGE"]
fn exact_vendor_image_replay() {
    let path =
        std::env::var_os("GP200_VENDOR_SEQUENCE_IMAGE").expect("explicit retained image path");
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(bytes.len(), 6_452_728);
    assert_eq!(JobSha256::of(&bytes).hex(), VENDOR_IMAGE_SHA256);
    let review = checked_review(bytes);
    for chunk in 1..=255 {
        assert!(
            review
                .count_review(chunk)
                .unwrap()
                .sections
                .iter()
                .all(|s| !s.count.discrepancy())
        );
    }
    for chunk in [180, 255] {
        let count: u64 = review
            .summary()
            .sections
            .iter()
            .map(|s| (s.bytes as u64).div_ceil(u64::from(chunk)))
            .sum();
        let actions = replay(VendorSequence::new(review.clone(), 11, 0).unwrap(), chunk);
        assert_native_payload_order(&actions, &review, chunk);
        assert_eq!(
            actions
                .iter()
                .filter(|a| a.phase() == VendorPhase::Block)
                .count() as u64,
            count
        );
        assert_eq!(
            actions
                .iter()
                .filter(|a| a.phase() == VendorPhase::SectionRequest)
                .map(|a| a.wire()[4])
                .collect::<Vec<_>>(),
            SELECTORS.map(FirmwareSectionSelector::raw)
        );
        if chunk == 180 {
            assert_eq!(count, 35_851);
            assert_eq!(actions.len(), 35_870);
            assert_eq!(actions[23_300].phase(), VendorPhase::Descriptor);
            // Exact original sender actual21 at frozen comparison fatal28.
            assert_eq!(
                actions[23_300].wire(),
                [
                    0xf0, 0x7e, 0x48, 2, 0, 0, 0, 4, 8, 0, 0, 0, 0, 0, 0, 0, 0, 6, 6, 7, 0xf7,
                ]
            );
            let descriptors: Vec<_> = actions
                .iter()
                .filter(|a| a.phase() == VendorPhase::Descriptor)
                .map(|a| {
                    let d = FirmwareDescriptor::decode(a.wire()).unwrap();
                    (a.selector().unwrap(), d.section_bytes(), d.block_count())
                })
                .collect();
            assert_eq!(
                descriptors,
                [
                    (0, 4_192_632, 23_293),
                    (1, 294_912, 1_639),
                    (2, 131_072, 729),
                    (0x10, 1_834_032, 10_190)
                ]
            );
        }
        println!(
            "Exact-image offline replay: {} data blocks, {} actions, chunk{chunk}; host finish unverified",
            count,
            actions.len()
        );
    }
}

#[test]
fn during_drain_reply_delivered_after_write_or_poll_preserves_original_times() {
    for delivery_after_write in [true, false] {
        let mut e = engine();
        let mut clock = 0;
        let a = offer(&mut e, &mut clock);
        let captured = frame(&e, 5, identity(2));
        let receipt = VendorWriteReceipt {
            at_ms: 10,
            extent: VendorWriteExtent::Known(5),
            drain: VendorDrain::Returned,
        };
        if delivery_after_write {
            e.finish_write(a.ordinal(), receipt.clone()).unwrap();
            e.tick(20).unwrap();
            e.receive(captured).unwrap();
        } else {
            e.tick(20).unwrap();
            e.receive(captured).unwrap();
            e.finish_write(a.ordinal(), receipt.clone()).unwrap();
        }
        assert_eq!(e.negotiated_chunk(), Some(2));
        assert_eq!(e.clock_ms, 20);
        let retained = e.take_completed().unwrap();
        assert_eq!(retained.offered_at_ms, 0);
        assert_eq!(retained.write, Some(receipt));
        assert_eq!(retained.frames[0].at_ms, 5);
    }
}
#[test]
fn genuinely_late_or_reordered_rx_never_reopens_or_extends_deadline() {
    let mut e = engine();
    let mut clock = 0;
    let a = offer(&mut e, &mut clock);
    e.finish_write(
        a.ordinal(),
        VendorWriteReceipt {
            at_ms: 10,
            extent: VendorWriteExtent::Known(5),
            drain: VendorDrain::Returned,
        },
    )
    .unwrap();
    let f = frame(&e, 10 + VENDOR_ACTION_BUDGET_MS, identity(2));
    assert_eq!(e.receive(f), Err(VendorFailure::DeadlineExpired));
    assert_eq!(e.negotiated_chunk(), None);
    assert_eq!(
        e.rejected_frame().unwrap().at_ms,
        10 + VENDOR_ACTION_BUDGET_MS
    );
    let mut e = engine();
    let mut clock = 0;
    to_terminal(&mut e, &mut clock);
    let a = offer(&mut e, &mut clock);
    full(&mut e, &a, &mut clock);
    reply(&mut e, &mut clock, vec![0xf0, 0x7e, 0x48, 3, 1, 2, 0xf7]);
    let f = frame(&e, clock - 1, vec![0xf0, 0x7e, 0x48, 7, 3, 4, 0xf7]);
    assert_eq!(e.receive(f), Err(VendorFailure::InvalidClock));
    assert_eq!(e.take_completed().unwrap().terminal_data.len(), 1);
    let mut e = engine();
    let mut clock = 0;
    let a = offer(&mut e, &mut clock);
    assert_eq!(
        e.tick(VENDOR_ACTION_BUDGET_MS),
        Err(VendorFailure::DeadlineExpired)
    );
    let f = frame(&e, 1, identity(2));
    assert_eq!(e.receive(f), Err(VendorFailure::AlreadyStopped));
    assert_eq!(e.negotiated_chunk(), None);
    assert_eq!(e.take_completed().unwrap().action, a);
}
#[test]
fn terminal_evidence_and_decoded_body_limits_stop_without_another_offer() {
    let mut e = engine();
    let mut clock = 0;
    to_terminal(&mut e, &mut clock);
    let a = offer(&mut e, &mut clock);
    full(&mut e, &a, &mut clock);
    for _ in 0..MAX_ACTION_FRAMES {
        reply(&mut e, &mut clock, vec![0xf0, 0x7e, 0x48, 3, 0xf7]);
    }
    let f = frame(&e, clock + 1, vec![0xf0, 0x7e, 0x48, 7, 0xf7]);
    assert_eq!(e.receive(f), Err(VendorFailure::EvidenceCapacity));
    let retained = e.take_completed().unwrap();
    assert_eq!(retained.frames.len(), MAX_ACTION_FRAMES);
    assert_eq!(retained.terminal_data.len(), MAX_ACTION_FRAMES);
    let mut e = engine();
    let mut clock = 0;
    to_terminal(&mut e, &mut clock);
    let a = offer(&mut e, &mut clock);
    full(&mut e, &a, &mut clock);
    let mut raw = vec![0xf0, 0x7e, 0x48, 7];
    raw.extend([0; 512]);
    raw.push(0xf7);
    let f = frame(&e, clock + 1, raw);
    assert_eq!(e.receive(f), Err(VendorFailure::UnexpectedTraffic));
    assert_eq!(e.take_completed().unwrap().terminal_data.len(), 0);
}
