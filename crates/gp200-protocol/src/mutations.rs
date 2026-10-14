//! Statically recovered mutation records. Building bytes never grants hardware
//! permission: actors must validate device/epoch/context/revision and readback.
use crate::{RawError, RawRecord, RawTransfer, WRITE_KIND, WireContext};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MutationError {
    #[error("module ID must be 0..10")]
    InvalidModule,
    #[error("preset slot/context must be bounded to 0..255 and group 0 or 1")]
    InvalidContext,
    #[error("chain must be an eleven-module permutation")]
    InvalidChain,
    #[error("name must contain at most 16 printable ASCII bytes")]
    InvalidName,
    #[error("retained preset does not match the recovered 33-record structure")]
    InvalidPreset,
    #[error(transparent)]
    Raw(#[from] RawError),
}

fn validate_context(context: WireContext) -> Result<(), MutationError> {
    if context.group > 1 || context.selector > 255 {
        return Err(MutationError::InvalidContext);
    }
    Ok(())
}

pub fn contextual_write_request(
    context: WireContext,
    records: &[RawRecord],
) -> Result<RawTransfer, MutationError> {
    validate_context(context)?;
    let mut all = vec![context.to_record()];
    all.extend_from_slice(records);
    Ok(RawTransfer::from_records(WRITE_KIND, &all)?)
}

pub fn enabled_write_record(module: u8, enabled: bool) -> Result<RawRecord, MutationError> {
    if module > 10 {
        return Err(MutationError::InvalidModule);
    }
    Ok(RawRecord {
        command: 0x15,
        body: vec![module, u8::from(enabled), 0, 0],
    })
}

/// Command 0x16 uses XML Code, unlike command 5's XML Index.
pub fn model_write_record(module: u8, code: u32) -> Result<RawRecord, MutationError> {
    if module > 10 {
        return Err(MutationError::InvalidModule);
    }
    let mut body = vec![module, 0, 0, 0];
    body.extend_from_slice(&code.to_le_bytes());
    Ok(RawRecord {
        command: 0x16,
        body,
    })
}

/// Preserve both unnamed mode bytes and the incoming final byte. Only the
/// target slot and eleven routing IDs are established fields in this builder.
pub fn chain_write_record(
    retained: &RawRecord,
    order: &[u8; 11],
) -> Result<RawRecord, MutationError> {
    if retained.command != 8 || retained.body.len() != 16 {
        return Err(MutationError::InvalidPreset);
    }
    let mut sorted = *order;
    sorted.sort_unstable();
    if sorted != [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10] {
        return Err(MutationError::InvalidChain);
    }
    let mut result = retained.clone();
    result.body[4..15].copy_from_slice(order);
    Ok(result)
}

pub fn metadata_write_record(selector: u16, value: u16) -> Result<RawRecord, MutationError> {
    // Only the seven selectors consumed by original manager 0x5ba3c0.
    if selector > 6 {
        return Err(MutationError::InvalidPreset);
    }
    let mut body = selector.to_le_bytes().to_vec();
    body.extend_from_slice(&value.to_le_bytes());
    Ok(RawRecord { command: 6, body })
}

pub fn ascii_name(name: &str) -> Result<[u8; 16], MutationError> {
    if name.len() > 16 || !name.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return Err(MutationError::InvalidName);
    }
    let mut encoded = [0; 16];
    encoded[..name.len()].copy_from_slice(name.as_bytes());
    Ok(encoded)
}

pub fn rename_write_record(slot: u16, name: &str) -> Result<RawRecord, MutationError> {
    named_record(7, slot, ascii_name(name)?)
}

fn named_record(command: u16, slot: u16, name: [u8; 16]) -> Result<RawRecord, MutationError> {
    if slot > 255 {
        return Err(MutationError::InvalidContext);
    }
    let mut body = slot.to_le_bytes().to_vec();
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(&name);
    Ok(RawRecord { command, body })
}

/// Named save-current path 0x5b7c9d. Unlike contextual edits it has no context
/// prefix. Use readback of the explicit stored target to establish persistence.
pub fn save_buffer_request(slot: u16, name: [u8; 16]) -> Result<RawTransfer, MutationError> {
    Ok(RawTransfer::from_records(
        WRITE_KIND,
        &[named_record(0x2003, slot, name)?],
    )?)
}

/// Original UI selection path 0x5d7ed0 → manager 0x5b9a10. There is no
/// command0000 prefix. A stored read is a separate operation, never audition.
pub fn select_stored_request(slot: u16) -> Result<RawTransfer, MutationError> {
    if slot > 255 {
        return Err(MutationError::InvalidContext);
    }
    let mut body = vec![0, 0];
    body.extend_from_slice(&slot.to_le_bytes());
    Ok(RawTransfer::from_records(
        WRITE_KIND,
        &[RawRecord {
            command: 0x1008,
            body,
        }],
    )?)
}

/// Original asynchronous clear worker 0x5ac4f0 emits command2009 in the
/// full-preset observer's context1/range envelope, not the save-current path.
pub fn clear_stored_request(slot: u16) -> Result<RawTransfer, MutationError> {
    validate_context(WireContext {
        group: 1,
        selector: slot,
    })?;
    let slot_bytes = slot.to_le_bytes();
    contextual_write_request(
        WireContext {
            group: 1,
            selector: slot,
        },
        &[
            RawRecord {
                command: 1,
                body: [slot_bytes, slot_bytes].concat(),
            },
            RawRecord {
                command: 0x2009,
                body: [slot_bytes, [0, 0]].concat(),
            },
        ],
    )
}

/// The original paste path's exact payload, excluding the file CRC. Retarget
/// only the two established embedded slot fields; preserve every other byte.
pub fn full_stored_write_request(
    slot: u16,
    source: &RawTransfer,
) -> Result<RawTransfer, MutationError> {
    validate_context(WireContext {
        group: 1,
        selector: slot,
    })?;
    let contexts: Vec<_> = source.records().iter().filter(|r| r.command == 0).collect();
    if contexts.len() > 1 || contexts.first().is_some_and(|r| r.body.len() != 4) {
        return Err(MutationError::InvalidPreset);
    }
    if let Some(context) = WireContext::from_transfer(source) {
        validate_context(context)?;
    }
    let mut records: Vec<_> = source
        .records()
        .iter()
        .filter(|r| r.command != 0)
        .cloned()
        .collect();
    let mut expected = vec![(2, 88), (8, 16)];
    expected.extend(std::iter::repeat_n((0x14, 68), 11));
    expected.extend(std::iter::repeat_n((0xc, 12), 9));
    expected.extend(std::iter::repeat_n((0x10, 4), 3));
    expected.extend(std::iter::repeat_n((0xf, 8), 8));
    if records
        .iter()
        .map(|r| (r.command, r.body.len()))
        .collect::<Vec<_>>()
        != expected
    {
        return Err(MutationError::InvalidPreset);
    }
    let modules: Vec<_> = records[2..13].iter().map(|r| r.body[0]).collect();
    let mut unique = modules;
    unique.sort_unstable();
    if unique != (0..11).collect::<Vec<_>>() {
        return Err(MutationError::InvalidPreset);
    }
    if records[2..13]
        .iter()
        .any(|r| r.body[1] > 1 || u16::from_le_bytes([r.body[2], r.body[3]]) != 15)
    {
        return Err(MutationError::InvalidPreset);
    }
    let order: [u8; 11] = records[1].body[4..15]
        .try_into()
        .map_err(|_| MutationError::InvalidChain)?;
    chain_write_record(&records[1], &order)?;
    if records[0].body[..2] != records[1].body[..2] {
        return Err(MutationError::InvalidPreset);
    }
    let slot_bytes = slot.to_le_bytes();
    records[0].body[0..2].copy_from_slice(&slot_bytes);
    records[1].body[0..2].copy_from_slice(&slot_bytes);
    let mut range = slot_bytes.to_vec();
    range.extend_from_slice(&slot_bytes);
    records.insert(
        0,
        RawRecord {
            command: 1,
            body: range,
        },
    );
    contextual_write_request(
        WireContext {
            group: 1,
            selector: slot,
        },
        &records,
    )
}
