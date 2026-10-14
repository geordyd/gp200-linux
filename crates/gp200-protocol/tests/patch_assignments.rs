use gp200_protocol::*;

fn record(command: u16, bytes: &[u8]) -> RawRecord {
    RawRecord {
        command,
        body: bytes.to_vec(),
    }
}

#[test]
fn exp_max_precedes_min_and_sparse_identity_roundtrips_all_float_bits() {
    // Independent layout expectation: group 1, slot 2, WAH, sparse u16 ID;
    // noncanonical NaN payload as MAX and negative zero as MIN.
    let raw = record(12, &[0x12, 1, 3, 1, 0x45, 0x23, 0xc1, 0x7f, 0, 0, 0, 0x80]);
    let decoded = ExpressionAssignmentRecord::decode(&raw).unwrap();
    assert_eq!((decoded.slot.group(), decoded.slot.slot()), (1, 2));
    assert_eq!(decoded.parameter_raw, 259);
    assert_eq!(decoded.maximum.0, 0x7fc12345);
    assert_eq!(decoded.minimum.0, 0x80000000);
    assert_eq!(decoded.to_record(), raw);
    for bad in [3, 0x13, 0x30, 0xff] {
        assert!(ExpressionSlotId::from_packed(bad).is_err());
    }
}

#[test]
fn ctrl_membership_edits_retain_padding_state_and_high_mask_bits() {
    let raw = record(15, &[7, 1, 0xaa, 0x55, 1, 8, 0xef, 0xcd]);
    let mut decoded = CtrlAssignmentRecord::decode(&raw).unwrap();
    decoded.set_module(10, true).unwrap();
    decoded.set_module(0, false).unwrap();
    assert_eq!(
        decoded.to_record().body,
        [7, 1, 0xaa, 0x55, 0, 12, 0xef, 0xcd]
    );
    assert!(decoded.contains_module(10).unwrap());
    assert!(decoded.set_module(11, false).is_err());
}

#[test]
fn quick_special_references_and_unknowns_do_not_normalize() {
    for module in [11, 12, 0xff, 201] {
        for parameter in [0, 255, 65535u16] {
            let mut body = vec![2, module];
            body.extend_from_slice(&parameter.to_le_bytes());
            let raw = record(16, &body);
            assert_eq!(
                QuickAssignmentRecord::decode(&raw).unwrap().to_record(),
                raw
            );
        }
    }
}

#[test]
fn assignment_write_matches_independently_specified_native_tlv() {
    let raw = record(12, &[0x20, 1, 3, 0, 0, 0, 0xc8, 0x42, 0, 0, 0x20, 0x41]);
    let request = patch_assignment_write_request(
        WireContext {
            group: 0,
            selector: 23,
        },
        &raw,
    )
    .unwrap();
    assert_eq!(request.kind(), WRITE_KIND);
    assert_eq!(
        request.payload(),
        &[
            0, 0, 4, 0, 0, 0, 23, 0, 12, 0, 12, 0, 0x20, 1, 3, 0, 0, 0, 0xc8, 0x42, 0, 0, 0x20,
            0x41,
        ]
    );
}

#[test]
fn malformed_identities_and_unsupported_new_references_fail_closed() {
    for (cmd, size) in [(12, 12), (15, 8), (16, 4)] {
        for wrong_size in [0, size - 1, size + 1] {
            let raw = record(cmd, &vec![0; wrong_size]);
            assert!(
                patch_assignment_write_request(
                    WireContext {
                        group: 0,
                        selector: 0
                    },
                    &raw
                )
                .is_err()
            );
        }
    }
    for raw in [
        record(16, &[3, 0, 0, 0]),
        record(16, &[0, 13, 0, 0]),
        record(15, &[8, 0, 1, 2, 0, 0, 0, 0]),
        record(15, &[0, 2, 0, 0, 0, 0, 0, 0]),
        record(12, &[0, 1, 15, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ] {
        assert!(
            patch_assignment_write_request(
                WireContext {
                    group: 0,
                    selector: 0
                },
                &raw
            )
            .is_err()
        );
    }
    assert!(
        patch_assignment_write_request(
            WireContext {
                group: 2,
                selector: 0
            },
            &record(16, &[0, 11, 0, 0])
        )
        .is_err()
    );
}

#[test]
fn native_expression_endpoint_uses_cmd_e_and_preserves_explicit_padding() {
    let request = expression_endpoint_write_request(
        WireContext {
            group: 0,
            selector: 23,
        },
        ExpressionSlotId::new(1, 0).unwrap(),
        ExpressionEndpoint::Maximum,
        F32Bits::from_f32(99.0),
        [0x12, 0x34],
    )
    .unwrap();
    assert_eq!(
        request.payload(),
        [
            0, 0, 4, 0, 0, 0, 23, 0, 14, 0, 8, 0, 1, 16, 0x12, 0x34, 0, 0, 0xc6, 0x42
        ]
    );
    assert!(
        expression_endpoint_record(
            ExpressionSlotId::new(1, 0).unwrap(),
            ExpressionEndpoint::Minimum,
            F32Bits(0x7fc12345),
            [0; 2]
        )
        .is_err()
    );
}
