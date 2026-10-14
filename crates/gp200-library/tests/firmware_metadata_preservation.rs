use gp200_catalog::{CheckedEmbeddedMetadata, MetadataInspection, ModelKey};
use gp200_domain::*;
use gp200_library::NativeFile;
use std::sync::Arc;

// This checks opaque byte preservation across metadata views, not vendor-file
// interoperability; an independently constructed carrier supplies the layout.
const CONSTRUCTED_PRESET: &[u8] =
    include_bytes!("../../../tests/fixtures/native/constructed-mode1.prst");

fn native_with_code(key: ModelKey) -> Vec<u8> {
    let original = NativeFile::parse(CONSTRUCTED_PRESET).unwrap();
    let mut records = original.raw().records().to_vec();
    let record = records
        .iter_mut()
        .find(|r| r.command == 0x14 && r.body.len() == 68 && r.body[0] == key.module)
        .unwrap();
    record.body[4..8].copy_from_slice(&key.code.to_le_bytes());
    // Opaque values intentionally include a NaN payload and negative zero.
    record.body[8..12].copy_from_slice(&0x7fc12345u32.to_le_bytes());
    record.body[64..68].copy_from_slice(&0x80000000u32.to_le_bytes());
    let raw = RawTransfer::from_records(original.raw().kind(), &records).unwrap();
    original.preserve_with_raw(&raw).unwrap()
}

fn assert_unchanged_across_views(owner: &CheckedEmbeddedMetadata, key: ModelKey) {
    let bytes = native_with_code(key);
    let imported = NativeFile::parse(&bytes).unwrap();
    let snapshot = PresetSnapshot::from_raw(
        imported.raw().clone(),
        SnapshotSource::Capture {
            capture_id: "native metadata preservation".into(),
        },
        None,
        Revision(1),
    );
    let draft = PresetDraft::from_snapshot(&snapshot);
    for profile in [
        EditorMetadataProfile::Gp200Editor160,
        EditorMetadataProfile::Gp200Editor181,
        EditorMetadataProfile::Gp200Editor160,
    ] {
        let _inspection = owner.view(profile).inspect_any(key);
        assert_eq!(imported.original_bytes(), bytes);
        assert_eq!(imported.preserve_with_raw(&snapshot.raw).unwrap(), bytes);
        let projected = draft
            .snapshot(&snapshot, snapshot.source.clone(), Revision(2))
            .unwrap();
        assert_eq!(projected.raw.payload(), imported.raw().payload());
        let exported = imported.preserve_with_raw(&projected.raw).unwrap();
        assert_eq!(exported, bytes);
        let again = NativeFile::parse(&exported).unwrap();
        assert_eq!(again.original_bytes(), bytes);
        assert_eq!(again.header(), imported.header());
        assert_eq!(again.raw().payload(), snapshot.raw.payload());
        let copied = PresetSnapshot::from_raw(
            Arc::new(again.raw().as_ref().clone()),
            snapshot.source.clone(),
            None,
            Revision(3),
        );
        assert_eq!(
            copied
                .view
                .modules
                .iter()
                .find(|m| m.module.get() == key.module)
                .unwrap()
                .model
                .0,
            key.code
        );
    }
}

#[test]
fn every_model_outside_old_profile_preserves_complete_native_prst_import_export() {
    let owner = CheckedEmbeddedMetadata::embedded().unwrap();
    for fact in owner.vendor_minima() {
        assert!(matches!(
            owner
                .view(EditorMetadataProfile::Gp200Editor160)
                .inspect_any(fact.key()),
            MetadataInspection::OutsideSelectedProfile { .. }
        ));
        assert_unchanged_across_views(&owner, fact.key());
    }
}

#[test]
fn unknown_code_and_ice_default_are_not_replaced_or_clamped_by_view_selection() {
    let owner = CheckedEmbeddedMetadata::embedded().unwrap();
    for key in [
        ModelKey {
            module: 3,
            code: 0xdeadbeef,
        },
        ModelKey {
            module: 8,
            code: 0x0b000033,
        },
    ] {
        assert_unchanged_across_views(&owner, key);
    }
}
