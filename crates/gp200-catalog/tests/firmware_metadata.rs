use gp200_catalog::*;
use gp200_domain::{EditorMetadataProfile::*, *};
#[cfg(gp200_private_evidence)]
use std::collections::BTreeSet;
use std::{collections::BTreeMap, sync::Arc};

#[cfg(gp200_private_evidence)]
const COMPARISON: &str = include_str!(
    "../../../analysis/implementation/firmware-163-compatibility-preparation/catalog-comparison.json"
);
fn key(module: u8, code: u32) -> ModelKey {
    ModelKey { module, code }
}
fn owner() -> CheckedEmbeddedMetadata {
    CheckedEmbeddedMetadata::embedded().unwrap()
}
fn report(version: &str) -> UserReportedFirmware {
    UserReportedFirmware::new(
        version,
        FirmwareDeviceAssociation::usb_serial("GP-200", 0x84ef, 0x002a, "20220118").unwrap(),
        FirmwareReportDate::new(2026, 10, 11).unwrap(),
    )
    .unwrap()
}

#[test]
#[cfg(gp200_private_evidence)]
fn exact_counts_and_all_membership_keys_match_independent_accepted_comparison() {
    let owner = owner();
    let comparison: serde_json::Value = serde_json::from_str(COMPARISON).unwrap();
    for (profile, prefix, counts, total, controls) in [
        (
            Gp200Editor160,
            "old",
            [31, 6, 37, 71, 3, 90, 6, 27, 21, 15, 1],
            308,
            1329,
        ),
        (
            Gp200Editor181,
            "new",
            [31, 6, 43, 76, 4, 90, 6, 28, 22, 15, 1],
            322,
            1400,
        ),
    ] {
        let view = owner.view(profile);
        assert_eq!(
            (view.model_count(), view.parameter_count()),
            (total, controls)
        );
        for (module, count) in counts.into_iter().enumerate() {
            assert_eq!(view.module_model_count(module as u8), Some(count));
        }
        let expected: BTreeSet<_> = comparison["GP-200"][format!("{prefix}_membership")]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                key(
                    module_id(row["module"].as_str().unwrap()).unwrap(),
                    row["code"].as_u64().unwrap() as u32,
                )
            })
            .collect();
        assert_eq!(
            view.models().map(|m| m.key()).collect::<BTreeSet<_>>(),
            expected
        );
        assert_eq!(view.identity().profile(), profile);
    }
}

#[test]
#[cfg(gp200_private_evidence)]
fn every_index_delta_retains_both_algorithm_sources_without_wire_conversion() {
    let owner = owner();
    let comparison: serde_json::Value = serde_json::from_str(COMPARISON).unwrap();
    let rows: Vec<_> = owner.index_changes().collect();
    assert_eq!(rows.len(), 56);
    let mut counts = BTreeMap::new();
    for (row, oracle) in rows.iter().zip(
        comparison["GP-200"]["model_xml_index_changes"]
            .as_array()
            .unwrap(),
    ) {
        *counts.entry(row.key().module).or_insert(0) += 1;
        for (index, side, profile) in [
            (row.old(), "old", Gp200Editor160),
            (row.current(), "new", Gp200Editor181),
        ] {
            assert_eq!(index.key(), row.key());
            assert_eq!(
                index.observed_value(),
                oracle[side]["xml_index"].as_u64().unwrap() as u16
            );
            assert_eq!(
                index.source().file,
                oracle[side]["source"]["path"].as_str().unwrap()
            );
            assert_eq!(
                index.source().line,
                Some(oracle[side]["source"]["line"].as_u64().unwrap() as u32)
            );
            assert_eq!(index.identity().profile(), profile);
            assert_eq!(index.source().file, index.identity().algorithm_file());
        }
    }
    assert_eq!(counts, BTreeMap::from([(2, 28), (7, 27), (4, 1)]));
    let ac = rows.iter().find(|r| r.key() == key(2, 10)).unwrap();
    assert_eq!(
        (ac.old().observed_value(), ac.current().observed_value()),
        (31, 32)
    );
    assert_ne!(
        ac.old().identity().algorithm_sha256(),
        ac.current().identity().algorithm_sha256()
    );
    let transport = Catalog::embedded().unwrap();
    assert_eq!(transport.model_by_key(ac.key()).unwrap().xml_index, 32);
}

#[test]
fn four_semantic_changes_are_exact_old_definitions_and_f32_bits() {
    let owner = owner();
    let old = owner.view(Gp200Editor160);
    let current = owner.view(Gp200Editor181);
    assert_eq!(owner.parameter_changes().len(), 4);
    for code in [0x0b00001d, 0x0b00001f, 0x0b000021] {
        let a = old.model(key(8, code)).unwrap().parameter(3).unwrap();
        let b = current.model(key(8, code)).unwrap().parameter(3).unwrap();
        assert_eq!(a.numeric_bounds(), Some((1.0, 100.0, 0.1)));
        assert_eq!(b.numeric_bounds(), Some((1.0, 100.0, 1.0)));
        assert_eq!(a.raw_attributes()["Dmin"], "1.0");
        assert_eq!(b.raw_attributes()["Dmin"], "1");
        assert_eq!(a.default_bits(), Some(0x42773333));
        assert_eq!(a.default_bits(), b.default_bits());
        assert_eq!((a.parameter_id(), a.control_order()), (3, 3));
    }
    let ice = key(8, 0x0b000033);
    assert_eq!(
        old.model(ice).unwrap().parameter(0).unwrap().default_bits(),
        Some(0x42480000)
    );
    assert_eq!(
        current
            .model(ice)
            .unwrap()
            .parameter(0)
            .unwrap()
            .default_bits(),
        Some(0x41a00000)
    );
    for delta in owner.parameter_changes() {
        for (view, raw, bits) in [
            (old, delta.old_definition(), delta.default_bits().0),
            (current, delta.new_definition(), delta.default_bits().1),
        ] {
            let parameter = view
                .model(delta.key())
                .unwrap()
                .parameter(delta.parameter_id())
                .unwrap();
            assert_eq!(raw.attributes(), parameter.raw_attributes());
            assert_eq!(raw.source(), parameter.source());
            assert_eq!(bits, parameter.default_bits());
        }
    }
}

#[test]
fn every_shared_amp_control_default_and_paired_cab_is_equal() {
    let owner = owner();
    let a = owner.view(Gp200Editor160);
    let b = owner.view(Gp200Editor181);
    assert_eq!(owner.shared_amp_evidence().len(), 71);
    for row in owner.shared_amp_evidence() {
        let old = a.model(row.key()).unwrap();
        let new = b.model(row.key()).unwrap();
        assert_eq!(row.key().module, 3);
        assert_eq!(
            old.local_defaults().declared(),
            new.local_defaults().declared()
        );
        assert_eq!(
            old.raw_attributes()["CABCode"],
            row.paired_cab_code().to_string()
        );
        assert_eq!(
            new.raw_attributes()["CABCode"],
            row.paired_cab_code().to_string()
        );
        assert!(a.model(key(5, row.paired_cab_code())).is_some());
        assert!(b.model(key(5, row.paired_cab_code())).is_some());
        assert_eq!(row.sources(), (old.source(), new.source()));
        let defaults: BTreeMap<_, _> = old
            .parameters()
            .map(|p| (p.parameter_id(), p.default_bits()))
            .collect();
        assert_eq!(defaults, row.declared_defaults().collect());
        for p in old.parameters() {
            let q = new.parameter(p.parameter_id()).unwrap();
            assert_eq!(
                (
                    p.name(),
                    p.ui_kind(),
                    p.control_kind(),
                    p.numeric_bounds(),
                    p.default_bits()
                ),
                (
                    q.name(),
                    q.ui_kind(),
                    q.control_kind(),
                    q.numeric_bounds(),
                    q.default_bits()
                )
            );
            assert_eq!(
                p.choices().map(|x| (x.id(), x.name())).collect::<Vec<_>>(),
                q.choices().map(|x| (x.id(), x.name())).collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn all_later_slots_and_effects_are_explicitly_outside_the_old_profile() {
    let owner = owner();
    let old = owner.view(Gp200Editor160);
    let current = owner.view(Gp200Editor181);
    assert_eq!(owner.vendor_minima().len(), 14);
    for fact in owner.vendor_minima() {
        assert!(old.model(fact.key()).is_none());
        assert!(current.model(fact.key()).is_some());
        match old.inspect_any(fact.key()) {
            MetadataInspection::OutsideSelectedProfile {
                selected_profile,
                model,
            } => {
                assert_eq!(selected_profile, Gp200Editor160);
                assert_eq!(model.identity().profile(), Gp200Editor181);
                assert_eq!(model.key(), fact.key());
            }
            other => panic!("unexpected inspection {other:?}"),
        }
        assert!(matches!(
            current.inspect_any(fact.key()),
            MetadataInspection::SelectedProfile(_)
        ));
    }
    assert!(matches!(
        old.inspect_any(key(3, 0xdeadbeef)),
        MetadataInspection::UnknownModel(_)
    ));
    assert_eq!(
        owner
            .vendor_minima()
            .iter()
            .filter(|f| f.minimum().components() == [1, 7, 4])
            .count(),
        10
    );
    assert_eq!(
        owner
            .vendor_minima()
            .iter()
            .filter(|f| f.minimum().components() == [1, 8, 0])
            .count(),
        4
    );
    assert_eq!(owner.release_note_source().bytes(), 73918);
}

#[test]
fn manual_report_assessment_is_partial_and_never_interpolates_a_catalog() {
    let owner = owner();
    let old = owner.view(Gp200Editor160);
    let basic = key(3, 0x07000015);
    let snap = key(3, 0x0f000000);
    assert_eq!(
        old.assess(basic, None, None),
        MetadataAvailability::UnknownFirmwareOrProfile
    );
    let version = report("V1.6.3");
    assert_eq!(
        old.assess(basic, Some(&version), Some(version.device())),
        MetadataAvailability::PresentInEditor160Metadata
    );
    assert!(
        matches!(old.assess(snap, Some(&version), Some(version.device())), MetadataAvailability::VendorMinimumNotMet { minimum } if minimum.components() == [1, 7, 4])
    );
    let another =
        FirmwareDeviceAssociation::usb_serial("GP-200", 0x84ef, 0x002a, "another").unwrap();
    assert_eq!(
        old.assess(basic, Some(&version), Some(&another)),
        MetadataAvailability::UnknownFirmwareOrProfile
    );
    for value in ["1.6.4", "1.7.0", "1.8.2", "2.0.0"] {
        let report = report(value);
        assert_eq!(
            old.assess(basic, Some(&report), Some(report.device())),
            MetadataAvailability::UnknownFirmwareOrProfile
        );
    }
    let newer = report("1.7.4");
    assert!(matches!(
        old.assess(snap, Some(&newer), Some(newer.device())),
        MetadataAvailability::VendorMinimumMetMetadataUnconfirmed { .. }
    ));
    let wrong_variant = UserReportedFirmware::new(
        "1.6.3",
        FirmwareDeviceAssociation::named_local_record("GP-200LT", "local pedal").unwrap(),
        version.recorded_date(),
    )
    .unwrap();
    assert_eq!(
        old.assess(basic, Some(&wrong_variant), Some(wrong_variant.device())),
        MetadataAvailability::UnknownFirmwareOrProfile
    );
    assert_eq!(
        wrong_variant.suggested_metadata_profile(wrong_variant.device()),
        None
    );
}

#[test]
fn name_collision_search_and_favorites_use_module_code_and_preserve_unknown_keys() {
    let owner = owner();
    let old = owner.view(Gp200Editor160);
    let tube_delay = old
        .models()
        .find(|m| m.module_name() == "DLY" && m.name() == "Tube")
        .unwrap();
    let tube_distortion = key(2, 0x0300000b);
    assert_ne!(tube_delay.key(), tube_distortion);
    assert!(
        old.search(None, "Tube")
            .iter()
            .any(|m| m.key() == tube_delay.key())
    );
    assert!(
        !old.search(None, "Tube")
            .iter()
            .any(|m| m.key() == tube_distortion)
    );
    let mut favorites = Favorites::default();
    let unknown = key(3, 0xdeadbeef);
    for k in [tube_delay.key(), tube_distortion, unknown] {
        favorites.set(k, true);
    }
    let imported: Favorites =
        serde_json::from_str(&serde_json::to_string(&favorites).unwrap()).unwrap();
    assert_eq!(
        imported.keys().collect::<Vec<_>>(),
        favorites.keys().collect::<Vec<_>>()
    );
    assert!(
        matches!(old.inspect_any(unknown), MetadataInspection::UnknownModel(k) if k == unknown)
    );
}

fn body(module: u8, code: u32) -> Vec<u8> {
    let mut body = vec![0; 68];
    body[0] = module;
    body[1] = 0x91;
    body[2..4].copy_from_slice(&15u16.to_le_bytes());
    body[4..8].copy_from_slice(&code.to_le_bytes());
    for (i, part) in body[8..].as_chunks_mut::<4>().0.iter_mut().enumerate() {
        part.copy_from_slice(
            &(if i.is_multiple_of(2) {
                0x7fc12345u32 + i as u32
            } else {
                0x80000000u32
            })
            .to_le_bytes(),
        );
    }
    body
}

#[test]
fn simultaneous_views_and_retained_local_previews_are_independent() {
    let owner = owner();
    let key = key(8, 0x0b000033);
    let old = owner.view(Gp200Editor160);
    let current = owner.view(Gp200Editor181);
    let prior = old.model(key).unwrap().local_defaults();
    let latest = current.model(key).unwrap().local_defaults();
    assert_ne!(prior, latest);
    let mut before = body(8, 0xdeadbeef);
    let mut other = before.clone();
    prior.apply_to_local_module_body(&mut before).unwrap();
    latest.apply_to_local_module_body(&mut other).unwrap();
    assert_eq!(
        u32::from_le_bytes(before[8..12].try_into().unwrap()),
        0x42480000
    );
    assert_eq!(
        u32::from_le_bytes(other[8..12].try_into().unwrap()),
        0x41a00000
    );
    assert_eq!(prior, old.model(key).unwrap().local_defaults());
    assert_eq!(prior.profile(), Gp200Editor160);
    assert_ne!(prior.algorithm_sha256(), latest.algorithm_sha256());
}

#[test]
fn sparse_local_preview_preserves_every_undeclared_bit_and_never_partially_mutates() {
    let owner = owner();
    let model = owner.view(Gp200Editor160).model(key(0, 0)).unwrap();
    let preview = model.local_defaults();
    let before = body(0, 0xdeadbeef);
    let mut after = before.clone();
    preview.apply_to_local_module_body(&mut after).unwrap();
    assert_eq!(&after[..4], &before[..4]);
    assert_eq!(&after[16..], &before[16..]);
    assert_eq!(u32::from_le_bytes(after[4..8].try_into().unwrap()), 0);
    assert_eq!(
        u32::from_le_bytes(after[8..12].try_into().unwrap()),
        20f32.to_bits()
    );
    for len in [0, 8, 67, 69] {
        let mut bad = vec![0xa5; len];
        let unchanged = bad.clone();
        assert!(preview.apply_to_local_module_body(&mut bad).is_err());
        assert_eq!(bad, unchanged);
    }
    for offset in [0, 2] {
        let mut bad = before.clone();
        bad[offset] = 12;
        let unchanged = bad.clone();
        assert!(preview.apply_to_local_module_body(&mut bad).is_err());
        assert_eq!(bad, unchanged);
    }
    for len in [0, 1, 14, 16] {
        let mut bad = vec![0x7fc12345; len];
        let unchanged = bad.clone();
        assert!(preview.apply_to_local_bits(&mut bad).is_err());
        assert_eq!(bad, unchanged);
    }
    let detune = owner
        .view(Gp200Editor160)
        .models()
        .find(|m| m.module_name() == "MOD" && m.name() == "Detune")
        .unwrap();
    let unresolved = detune
        .parameters()
        .find(|p| {
            p.raw_attributes()
                .get("default")
                .is_some_and(|raw| raw == "-25 Cents")
        })
        .unwrap();
    assert!(unresolved.default_bits().is_none());
    assert!(
        detune
            .local_defaults()
            .unresolved_ids()
            .contains(&unresolved.parameter_id())
    );
}

#[test]
fn unsupported_unknown_and_ice_preset_inspection_and_raw_export_preserve_full_bytes() {
    let owner = owner();
    for code in [0x0b000032, 0xdeadbeef, 0x0b000033] {
        let raw = RawTransfer::from_records(
            1,
            &[
                RawRecord {
                    command: 0x99,
                    body: vec![0xff, 0x0, 0x91],
                },
                RawRecord {
                    command: 0x14,
                    body: body(8, code),
                },
            ],
        )
        .unwrap();
        let original = raw.payload().to_vec();
        let snapshot = PresetSnapshot::from_raw(
            Arc::new(raw),
            SnapshotSource::Capture {
                capture_id: "metadata preservation".into(),
            },
            None,
            Revision(1),
        );
        let draft = PresetDraft::from_snapshot(&snapshot);
        for profile in [Gp200Editor160, Gp200Editor181, Gp200Editor160] {
            let view = owner.view(profile);
            let _inspection = view.inspect_any(key(8, code));
            let _results = view.search(Some(8), "Delay");
            assert_eq!(snapshot.raw.payload(), original);
            let exported = draft
                .snapshot(&snapshot, snapshot.source.clone(), Revision(2))
                .unwrap();
            assert_eq!(exported.raw.payload(), original);
            let imported: RawTransfer =
                serde_json::from_slice(&serde_json::to_vec(exported.raw.as_ref()).unwrap())
                    .unwrap();
            assert_eq!(imported.payload(), original);
        }
    }
}

#[test]
fn existing_wire_catalog_and_control_adapter_remain_at_the_current_index() {
    let transport = Catalog::embedded().unwrap();
    let before = serde_json::to_vec(transport.data()).unwrap();
    let owner = owner();
    let old = owner.view(Gp200Editor160);
    let model = old.model(key(2, 10)).unwrap();
    assert_eq!(model.observed_index().observed_value(), 31);
    assert_eq!(transport.model_by_key(model.key()).unwrap().xml_index, 32);
    assert_eq!(serde_json::to_vec(transport.data()).unwrap(), before);
    assert_eq!(
        Catalog::embedded()
            .unwrap()
            .model_by_key(model.key())
            .unwrap()
            .xml_index,
        32
    );
}
