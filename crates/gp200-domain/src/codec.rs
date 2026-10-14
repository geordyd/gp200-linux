//! Lossless edits to fields established by static code and live round trips.
use crate::*;
use std::collections::BTreeSet;

fn unique_record<'a>(
    records: &'a mut [RawRecord],
    command: u16,
    length: usize,
    field: &str,
) -> Result<&'a mut RawRecord, ServiceError> {
    let mut matches = records
        .iter_mut()
        .filter(|record| record.command == command);
    let record = matches.next().ok_or_else(|| {
        ServiceError::invalid(format!("Cannot encode {field}: retained record is absent"))
    })?;
    if matches.next().is_some() || record.body.len() != length {
        return Err(ServiceError::invalid(format!(
            "Cannot encode {field}: retained record is ambiguous or has an unsupported length"
        )));
    }
    Ok(record)
}

pub(crate) fn encode_draft_fields(
    draft: &PresetDraft,
    baseline: &PresetSnapshot,
) -> Result<RawTransfer, ServiceError> {
    let projected = PresetSnapshot::from_raw(
        baseline.raw.clone(),
        baseline.source.clone(),
        baseline.target,
        baseline.revision,
    );
    let mut records = baseline.raw.records().to_vec();
    draft
        .view
        .patch_assignments
        .encode_into(&projected.view.patch_assignments, &mut records)?;
    let original_indices: BTreeSet<_> = projected
        .view
        .modules
        .iter()
        .map(|module| module.raw_record_index)
        .collect();
    let draft_indices: BTreeSet<_> = draft
        .view
        .modules
        .iter()
        .map(|module| module.raw_record_index)
        .collect();
    if original_indices != draft_indices || draft_indices.len() != draft.view.modules.len() {
        return Err(ServiceError::invalid(
            "Draft cannot add, remove or alias retained module records",
        ));
    }
    for module in &draft.view.modules {
        let record = records
            .get_mut(module.raw_record_index)
            .ok_or_else(|| ServiceError::invalid("Retained module record missing"))?;
        if record.command != 0x14
            || record.body.len() < 8
            || record.body[0] != module.module.get()
            || (record.body.len() - 8) / 4 != module.parameters.len()
            || u16::from_le_bytes([record.body[2], record.body[3]]) != module.slot_count_raw
        {
            return Err(ServiceError::invalid(
                "Retained module identity, size or slot count mismatch",
            ));
        }
        record.body[1] = module.state_raw;
        record.body[4..8].copy_from_slice(&module.model.0.to_le_bytes());
        for (index, bits) in module.parameters.iter().enumerate() {
            record.body[8 + index * 4..12 + index * 4].copy_from_slice(&bits.to_le_bytes());
        }
    }
    if draft.view.chain.value() != projected.view.chain.value() {
        let chain = draft
            .view
            .chain
            .value()
            .ok_or_else(|| ServiceError::invalid("Cannot erase an established signal chain"))?;
        let ids: Vec<_> = chain.iter().map(|module| module.get()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        if sorted != (0..11).collect::<Vec<_>>() {
            return Err(ServiceError::invalid(
                "Signal chain must contain all eleven modules exactly once",
            ));
        }
        unique_record(&mut records, 8, 16, "signal chain")?.body[4..15].copy_from_slice(&ids);
    }
    for (field, value, original, offset) in [
        (
            "tempo",
            draft.view.tempo.value(),
            projected.view.tempo.value(),
            2,
        ),
        (
            "preset level",
            draft.view.level.value(),
            projected.view.level.value(),
            4,
        ),
    ] {
        if value != original {
            let value = value.ok_or_else(|| {
                ServiceError::invalid(format!("Cannot erase established {field}"))
            })?;
            unique_record(&mut records, 2, 88, field)?.body[offset..offset + 2]
                .copy_from_slice(&value.to_le_bytes());
        }
    }
    if draft.view.name.value() != projected.view.name.value() {
        let name = draft
            .view
            .name
            .value()
            .ok_or_else(|| ServiceError::invalid("Cannot erase an established preset name"))?;
        // Preserve unverified encodings as a local projection. A changed non-ASCII
        // name cannot silently become lossy replacement bytes in a native export.
        if let Ok(encoded) = gp200_protocol::ascii_name(name) {
            unique_record(&mut records, 2, 88, "preset name")?.body[16..32]
                .copy_from_slice(&encoded);
        }
    }
    macro_rules! word {
        ($field:ident, $offset:expr) => {
            if draft.metadata.$field != projected.view.metadata.$field {
                let value = draft.metadata.$field.ok_or_else(|| {
                    ServiceError::invalid(concat!("Cannot erase ", stringify!($field)))
                })?;
                if value > 100 {
                    return Err(ServiceError::invalid(concat!(
                        stringify!($field),
                        " must be 0..100"
                    )));
                }
                unique_record(&mut records, 2, 88, stringify!($field))?.body[$offset..$offset + 2]
                    .copy_from_slice(&value.to_le_bytes());
            }
        };
    }
    word!(fx_send_level, 10);
    word!(fx_return_level, 12);
    if draft.metadata.native_style != projected.view.metadata.native_style {
        let value = draft
            .metadata
            .native_style
            .ok_or_else(|| ServiceError::invalid("Cannot erase native style"))?;
        value.validate()?;
        unique_record(&mut records, 2, 88, "native style")?.body[8..10]
            .copy_from_slice(&value.raw().to_le_bytes());
    }
    if draft.metadata.native_pan != projected.view.metadata.native_pan {
        let value = draft
            .metadata
            .native_pan
            .ok_or_else(|| ServiceError::invalid("Cannot erase native Pan"))?;
        value.validate()?;
        unique_record(&mut records, 2, 88, "native Pan")?.body[7] = value.raw() as u8;
    }
    if draft.metadata.fx_loop_mode != projected.view.metadata.fx_loop_mode {
        let value = draft
            .metadata
            .fx_loop_mode
            .ok_or_else(|| ServiceError::invalid("Cannot erase FX loop mode"))?;
        PatchFxLoopMode::new(value.raw())?;
        unique_record(&mut records, 2, 88, "FX loop mode")?.body[14] = value.raw();
    }
    RawTransfer::from_records(baseline.raw.kind(), &records)
        .map_err(|error| ServiceError::invalid(error.to_string()))
}
