use gp200_assets::inspect_firmware;
use gp200_domain::FirmwareImageProvenance;
const IMAGE: &[u8] = include_bytes!("../../../tests/fixtures/device-assets/synthetic-firmware.bin");
fn checksum(bytes: &mut [u8]) {
    bytes[14] = bytes[48..]
        .iter()
        .fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
}

#[test]
fn raw_container_and_header_fields_remain_exact_when_native_values_differ() {
    let mut raw = IMAGE.to_vec();
    raw[8..12].copy_from_slice(&[3, 0, 8, 0]);
    raw[32..36].copy_from_slice(&[1, 2, 0x45, 0x58]);
    raw[88..92].copy_from_slice(b"V182");
    checksum(&mut raw);
    let image = inspect_firmware(&raw).unwrap();
    let info = image.info();
    assert_eq!(image.raw(), raw);
    assert_eq!(info.image_family_raw, 3);
    assert_eq!(info.version_word_raw, 8);
    assert_eq!(info.metadata_raw[1], 2);
    assert_eq!(info.version_tag_raw, *b"V182");
    assert_eq!(info.image_provenance.raw_header(), &raw[..92]);
    assert_eq!(
        info.image_provenance.raw_words(),
        [3, 8, 0xcafe, u16::from_le_bytes([raw[14], 0xab])]
    );
    assert_eq!(info.image_provenance.native_normalized_words()[1], 264);
    assert_eq!(info.image_provenance.version_tag_file_offset(), 88);
    assert_eq!(
        info.image_provenance
            .announcement_candidate()
            .unwrap()
            .raw(),
        [240, 126, 72, 11, 1, 86, 49, 56, 50, 247]
    );
    assert_eq!(image.section(0).unwrap(), &raw[80..112]);
}
#[test]
fn native_tag_edges_are_preserved_and_only_strict_announcements_reject_invalid_tags() {
    for (tag, normalized_kind, normalized_version, normalized_patch, accepted) in [
        (*b"B163", 1, 262, 3, true),
        (*b"b163", 3, 262, 3, true),
        (*b"V163", 3, 262, 3, true),
        (*b"v163", 3, 262, 3, true),
        (*b"V/8:", 3, 0xff08, 10, false),
        (*b"X999", 3, 8, 2, false),
    ] {
        let mut raw = IMAGE.to_vec();
        raw[8..12].copy_from_slice(&[3, 0, 8, 0]);
        raw[33] = 2;
        raw[88..92].copy_from_slice(&tag);
        checksum(&mut raw);
        let image = inspect_firmware(&raw).unwrap();
        let provenance = &image.info().image_provenance;
        assert_eq!(image.raw(), raw);
        assert_eq!(provenance.raw_header(), &raw[..92]);
        assert_eq!(provenance.version_tag_raw(), &tag);
        assert_eq!(provenance.raw_words()[..2], [3, 8]);
        assert_eq!(
            provenance.native_normalized_words()[..2],
            [normalized_kind, normalized_version]
        );
        assert_eq!(provenance.metadata_raw()[1], 2);
        assert_eq!(provenance.native_normalized_metadata()[1], normalized_patch);
        assert_eq!(provenance.announcement_candidate().is_ok(), accepted);
        assert_eq!(image.info().image_version.is_some(), accepted);
    }
}
#[test]
fn additive_provenance_serialization_keeps_existing_raw_field_names_and_values() {
    let image = inspect_firmware(IMAGE).unwrap();
    let value = serde_json::to_value(image.info()).unwrap();
    assert_eq!(
        value["version_word_raw"],
        serde_json::json!(image.info().version_word_raw)
    );
    assert_eq!(
        value["metadata_raw"],
        serde_json::json!(image.info().metadata_raw)
    );
    assert_eq!(
        value["version_tag_raw"],
        serde_json::json!(image.info().version_tag_raw)
    );
    assert_eq!(value["image_provenance"]["version_tag_file_offset"], 88);
    assert_eq!(
        value["image_provenance"]["raw_header"],
        serde_json::json!(&IMAGE[..92])
    );
    assert_eq!(
        image.info().image_provenance,
        FirmwareImageProvenance::from_header(&IMAGE[..92]).unwrap()
    );
}
