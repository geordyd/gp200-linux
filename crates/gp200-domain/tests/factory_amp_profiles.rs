// Private capture acceptance suite; excluded from public source builds.
// Run with RUSTFLAGS="--cfg gp200_private_evidence" only in the retained lab checkout.
#![cfg(gp200_private_evidence)]

//! Candidate projections against independent accepted preparation vectors; no new hardware proof.
use gp200_domain::*;
use serde_json::Value;
use std::sync::Arc;

const VECTORS: &str = include_str!(
    "../../../analysis/implementation/factory-amp-expansion-execution/synthetic-vectors.json"
);
const TABLE: &str = include_str!(
    "../../../analysis/implementation/factory-model-expansion/settled-01/factory-amp-table.json"
);
fn decode(hex: &str) -> RawTransfer {
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
        .collect();
    RawTransfer::from_payload(0x12, bytes).unwrap()
}
fn snapshot(raw: RawTransfer) -> PresetSnapshot {
    PresetSnapshot::from_raw(
        Arc::new(raw),
        SnapshotSource::Hardware {
            epoch: SessionEpoch(1),
        },
        Some(PresetTarget::EditBuffer),
        Revision(1),
    )
}
fn globals(raw: RawTransfer) -> DeviceGlobalState {
    DeviceGlobalState::from_transfer(Revision(1), SessionEpoch(1), raw, vec![]).unwrap()
}
fn changed(raw: &RawTransfer, record: usize, offset: usize, byte: u8) -> RawTransfer {
    let mut records = raw.records().to_vec();
    records[record].body[offset] = byte;
    RawTransfer::from_records(raw.kind(), &records).unwrap()
}
fn cases() -> Vec<Value> {
    serde_json::from_str::<Value>(VECTORS).unwrap()["cases"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn table_exactly_binds_65_candidate_codes_indices_cabinets_and_sparse_bits() {
    let table: Value = serde_json::from_str(TABLE).unwrap();
    let rows: Vec<_> = table["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["required_phase"] == "AMP-M1")
        .collect();
    assert_eq!(rows.len(), 65);
    assert_eq!(FACTORY_AMP_CANDIDATES.len(), 65);
    for row in rows {
        let descriptor =
            factory_amp_descriptor(ModelCode(row["code"].as_u64().unwrap() as u32)).unwrap();
        assert_eq!(
            descriptor.xml_index,
            row["xml_index"].as_u64().unwrap() as u16
        );
        assert_eq!(
            descriptor.paired_cabinet.0,
            row["paired_cab"].as_u64().unwrap() as u32
        );
        let mut defaults: Vec<_> = row["declared_default_bits"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(key, value)| (key.parse::<u8>().unwrap(), value.as_u64().unwrap() as u32))
            .collect();
        defaults.sort();
        assert_eq!(descriptor.defaults, defaults);
        assert!(descriptor.defaults.iter().all(|(id, _)| *id <= 5));
        assert_eq!(
            descriptor.independently_blocked_controls,
            row["independent_control_gates"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_u64().unwrap() as u8)
                .collect::<Vec<_>>()
        );
        for mode in [false, true] {
            assert!(
                !FactoryAmpProfileId::new(descriptor.model, mode)
                    .unwrap()
                    .normal_build_accepted()
            );
        }
    }
    for excluded in [
        0x07000001, 0x07000015, 0x0700004a, 0x0700002c, 0x0700002f, 0x0700005a, 0x07100000,
        0x0a100000, 0,
    ] {
        assert!(FactoryAmpProfileId::new(ModelCode(excluded), true).is_err());
    }
}

#[test]
fn every_forward_and_reverse_projection_matches_the_independent_full_raw_vector() {
    let cases = cases();
    assert_eq!(cases.len(), 130);
    for case in cases {
        let before = snapshot(decode(case["before"].as_str().unwrap()));
        let global = globals(decode(case["globals"].as_str().unwrap()));
        let plan = FactoryAmpPlan::from_snapshot(
            &before,
            &global,
            ModelCode(case["target"].as_u64().unwrap() as u32),
        )
        .unwrap();
        assert_eq!(plan.profile.name(), case["name"].as_str().unwrap());
        assert_eq!(plan.auto_cab, case["auto_cab"].as_bool().unwrap());
        let forward = plan.projected_raw(&before).unwrap();
        assert_eq!(
            forward.payload(),
            decode(case["forward"].as_str().unwrap()).payload()
        );
        let reverse =
            FactoryAmpPlan::from_snapshot(&snapshot(forward), &global, FACTORY_AMP_BASELINE)
                .unwrap();
        assert_eq!(reverse.profile, plan.profile);
        assert_eq!(
            reverse
                .projected_raw(&snapshot(decode(case["forward"].as_str().unwrap())))
                .unwrap()
                .payload(),
            decode(case["reverse"].as_str().unwrap()).payload()
        );
        assert_eq!(
            decode(case["undo"].as_str().unwrap()).payload(),
            before.raw.payload()
        );
    }
}

#[test]
fn unknown_pair_source_cab_context_shapes_and_affected_assignments_fail_closed() {
    let case = cases().remove(0);
    let before = decode(case["before"].as_str().unwrap());
    let global = globals(decode(case["globals"].as_str().unwrap()));
    for raw in [
        changed(&before, 8, 4, 2),
        changed(&before, 1, 0, 24),
        changed(&before, 2, 0, 24),
        changed(&before, 6, 0, 4),
        changed(&before, 6, 1, 2),
        changed(&before, 6, 2, 14),
        changed(&before, 17, 1, 3),
        changed(&before, 23, 1, 5),
    ] {
        assert!(
            FactoryAmpPlan::from_snapshot(&snapshot(raw), &global, ModelCode(0x07000003)).is_err()
        );
    }
    for target in [
        0x0700004a, 0x0700002c, 0x0700002f, 0x0700005a, 0x07100000, 0x07000015,
    ] {
        assert!(
            FactoryAmpPlan::from_snapshot(&snapshot(before.clone()), &global, ModelCode(target))
                .is_err()
        );
    }
    let mut records = before.records().to_vec();
    records.swap(14, 15);
    assert!(
        FactoryAmpPlan::from_snapshot(
            &snapshot(RawTransfer::from_records(0x12, &records).unwrap()),
            &global,
            ModelCode(0x07000003)
        )
        .is_err()
    );
    let forward = snapshot(decode(case["forward"].as_str().unwrap()));
    assert!(FactoryAmpPlan::from_snapshot(&forward, &global, ModelCode(0x07000004)).is_err());
}

#[test]
fn hidden_bits_and_unaffected_bytes_stay_strict_with_only_existing_exclusions() {
    let case = cases().remove(1);
    let before = decode(case["before"].as_str().unwrap());
    let global = globals(decode(case["globals"].as_str().unwrap()));
    let hidden = changed(&before, 6, 64, 0x7f);
    let plan =
        FactoryAmpPlan::from_snapshot(&snapshot(hidden.clone()), &global, ModelCode(0x07000003))
            .unwrap();
    let expected = plan.projected_raw(&snapshot(hidden)).unwrap();
    assert_eq!(expected.records()[6].body[64], 0x7f);
    for (record, offset) in [(6, 64), (3, 8), (17, 2), (8, 0), (8, 1), (1, 16)] {
        let original = expected.records()[record].body[offset];
        assert!(!factory_amp_readback_matches(
            &expected,
            &changed(&expected, record, offset, original ^ 1)
        ));
    }
    for (record, offset) in [(1, 15), (2, 15), (26, 2), (26, 3)] {
        let original = expected.records()[record].body[offset];
        assert!(factory_amp_readback_matches(
            &expected,
            &changed(&expected, record, offset, original ^ 1)
        ));
    }
}

#[test]
fn different_hardware_epoch_or_variant_is_not_a_factory_candidate_context() {
    let case = cases().remove(1);
    let before = snapshot(decode(case["before"].as_str().unwrap()));
    let global = globals(decode(case["globals"].as_str().unwrap()));
    let mut wrong = before.clone();
    wrong.source = SnapshotSource::Hardware {
        epoch: SessionEpoch(2),
    };
    assert!(FactoryAmpPlan::from_snapshot(&wrong, &global, ModelCode(0x07000003)).is_err());
    wrong = before;
    wrong.device_model = DeviceModel::Gp200Lt;
    assert!(FactoryAmpPlan::from_snapshot(&wrong, &global, ModelCode(0x07000003)).is_err());
}
