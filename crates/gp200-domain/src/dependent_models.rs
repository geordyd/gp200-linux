//! Descriptive fixed dependent-model profiles; this module grants no I/O authority.

use crate::{DeviceGlobalState, ErrorKind, ModelCode, ModuleId, PresetSnapshot, ServiceError};
use gp200_protocol::{GlobalRecords, RawRecord, RawTransfer};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DependentModelProfile {
    AmpAutoCabOff,
    AmpAutoCabOn,
    WahAssignedPreserve,
}

impl DependentModelProfile {
    pub const fn name(self) -> &'static str {
        match self {
            Self::AmpAutoCabOff => "amp-auto-cab-off",
            Self::AmpAutoCabOn => "amp-auto-cab-on",
            Self::WahAssignedPreserve => "wah-assigned-preserve",
        }
    }

    /// These exact contexts passed root's bidirectional production-path model/Undo
    /// readbacks and five independent restorations; descriptor predicates stay strict.
    pub const fn normal_build_accepted(self) -> bool {
        match self {
            Self::AmpAutoCabOff | Self::AmpAutoCabOn | Self::WahAssignedPreserve => true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DependentModelPlan {
    pub profile: DependentModelProfile,
    pub module: ModuleId,
    pub previous: ModelCode,
    pub target: ModelCode,
    pub auto_cab: bool,
    pub paired_cabinet: Option<ModelCode>,
}

pub fn dependent_model_pair(module: ModuleId, previous: ModelCode, target: ModelCode) -> bool {
    let pair = match module.get() {
        3 => (0x07000015, 0x07000001),
        1 => (0x0500000a, 0x05000001),
        _ => return false,
    };
    (previous.0 == pair.0 && target.0 == pair.1) || (previous.0 == pair.1 && target.0 == pair.0)
}

fn unverified(message: &str) -> ServiceError {
    ServiceError::new(ErrorKind::Unverified, message)
}

pub fn dependent_preset_shape(raw: &RawTransfer) -> Result<(), ServiceError> {
    let expected = [(0, 4), (2, 88), (8, 16)]
        .into_iter()
        .chain(std::iter::repeat_n((0x14, 68), 11))
        .chain(std::iter::repeat_n((0x0c, 12), 9))
        .chain(std::iter::repeat_n((0x10, 4), 3))
        .chain(std::iter::repeat_n((0x0f, 8), 8));
    if !raw
        .records()
        .iter()
        .map(|record| (record.command, record.body.len()))
        .eq(expected)
    {
        return Err(unverified(
            "Dependent model selection requires the captured complete ordered native preset shape",
        ));
    }
    for (index, record) in raw.records()[3..14].iter().enumerate() {
        if record.body[0] != index as u8
            || record.body[1] > 1
            || record.body[2..4] != 15u16.to_le_bytes()
        {
            return Err(unverified(
                "Dependent model native module identity, count or enabled state is unknown",
            ));
        }
    }
    if raw.records()[14..23]
        .iter()
        .map(|record| record.body[0])
        .ne([0, 1, 2, 0x10, 0x11, 0x12, 0x20, 0x21, 0x22])
        || raw.records()[23..26]
            .iter()
            .map(|record| record.body[0])
            .ne([0, 1, 2])
        || raw.records()[26..34]
            .iter()
            .map(|record| record.body[0])
            .ne(0..8)
    {
        return Err(unverified(
            "Dependent model assignment identities are outside the captured native layout",
        ));
    }
    Ok(())
}

pub fn dependent_globals_shape(raw: &RawTransfer) -> Result<(), ServiceError> {
    let expected = [
        (0x1007, 72),
        (0x1002, 8),
        (0x1010, 4),
        (0x1000, 6),
        (0x1004, 124),
    ]
    .into_iter()
    .chain(std::iter::repeat_n((0x000a, 20), 20))
    .chain(std::iter::repeat_n((0x100e, 40), 3));
    if !raw
        .records()
        .iter()
        .map(|record| (record.command, record.body.len()))
        .eq(expected)
        || raw.records()[25..28]
            .iter()
            .enumerate()
            .any(|(index, record)| record.body[..2] != (index as u16).to_le_bytes())
        || raw.records()[5..25]
            .iter()
            .enumerate()
            .any(|(index, record)| record.body[..2] != (index as u16).to_le_bytes())
    {
        return Err(unverified(
            "Dependent model globals require the captured complete ordered native layout",
        ));
    }
    if !matches!(
        u16::from_le_bytes(raw.records()[3].body[2..4].try_into().unwrap()),
        0 | 1
    ) {
        return Err(unverified("Dependent model changed-latch value is unknown"));
    }
    dependent_auto_cab(raw)?;
    Ok(())
}

pub fn dependent_auto_cab(raw: &RawTransfer) -> Result<bool, ServiceError> {
    let records: Vec<&RawRecord> = raw
        .records()
        .iter()
        .filter(|record| record.command == 0x1007)
        .collect();
    let [record] = records.as_slice() else {
        return Err(unverified(
            "Dependent model Auto CAB requires one complete General record",
        ));
    };
    if record.body.len() != 72 {
        return Err(unverified("Dependent model General record is incomplete"));
    }
    match u16::from_le_bytes(record.body[70..72].try_into().unwrap()) {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(unverified("Dependent model Auto CAB value is unknown")),
    }
}

impl DependentModelPlan {
    pub fn from_snapshot(
        snapshot: &PresetSnapshot,
        globals: &DeviceGlobalState,
        module: ModuleId,
        target: ModelCode,
    ) -> Result<Self, ServiceError> {
        dependent_preset_shape(&snapshot.raw)?;
        dependent_globals_shape(globals.records().raw())?;
        if snapshot
            .context
            .is_none_or(|context| context.group != 0 || context.selector > 255)
        {
            return Err(unverified(
                "Dependent model selection needs a confirmed current-buffer context",
            ));
        }
        let context = snapshot.context.unwrap();
        if [1, 2]
            .into_iter()
            .any(|index| snapshot.raw.records()[index].body[..2] != context.selector.to_le_bytes())
        {
            return Err(unverified(
                "Dependent model embedded metadata/chain slots differ from the current context",
            ));
        }
        let previous = snapshot
            .view
            .module(module)
            .ok_or_else(|| unverified("Dependent model module is absent"))?
            .model;
        if !dependent_model_pair(module, previous, target) {
            return Err(unverified(
                "This dependent model pair has no fixed candidate profile",
            ));
        }
        let auto_cab = dependent_auto_cab(globals.records().raw())?;
        if module.get() == 1 && !auto_cab {
            return Err(unverified(
                "Assigned WAH model selection is captured only with Auto CAB1",
            ));
        }
        if module.get() == 3
            && !auto_cab
            && snapshot
                .view
                .module(ModuleId::new(5).unwrap())
                .is_none_or(|cabinet| cabinet.model.0 != 0x0a000010)
        {
            return Err(unverified(
                "Auto CAB0 AMP selection is captured with the unchanged ordinary Match cabinet only",
            ));
        }
        let profile = if module.get() == 3 {
            if auto_cab {
                DependentModelProfile::AmpAutoCabOn
            } else {
                DependentModelProfile::AmpAutoCabOff
            }
        } else {
            DependentModelProfile::WahAssignedPreserve
        };
        let affected =
            |candidate: u8| candidate == module.get() || (module.get() == 3 && candidate == 5);
        let references: Vec<&RawRecord> = snapshot
            .raw
            .records()
            .iter()
            .filter(|record| matches!(record.command, 0x0c | 0x10) && affected(record.body[1]))
            .collect();
        if module.get() == 1 {
            let exact = [0x10, 1, 3, 0, 0, 0, 0xc8, 0x42, 0, 0, 0, 0];
            if references.len() != 1 || references[0].command != 0x0c || references[0].body != exact
            {
                return Err(unverified(
                    "Assigned WAH selection is limited to the exact captured EXP10 Position3 record; other EXP/Quick cases remain unverified",
                ));
            }
        } else if !references.is_empty() {
            return Err(unverified(
                "Assigned AMP/CAB dependent model selection remains unverified",
            ));
        }
        let paired_cabinet = if module.get() == 3 && auto_cab {
            let previous_cabinet = if previous.0 == 0x07000015 {
                0x0a000010
            } else {
                0x0a000002
            };
            if snapshot
                .view
                .module(ModuleId::new(5).unwrap())
                .is_none_or(|cabinet| cabinet.model.0 != previous_cabinet)
            {
                return Err(unverified(
                    "Auto CAB dependent selection requires the exact captured ordinary paired cabinet source",
                ));
            }
            Some(ModelCode(if target.0 == 0x07000001 {
                0x0a000002
            } else {
                0x0a000010
            }))
        } else {
            None
        };
        Ok(Self {
            profile,
            module,
            previous,
            target,
            auto_cab,
            paired_cabinet,
        })
    }
}

/// Only the existing global comparison exclusions apply. The changed latch is
/// projected explicitly; its third runtime word and every unknown field stay strict.
pub fn dependent_globals_match(
    before: &RawTransfer,
    actual: &RawTransfer,
    expected_latch: Option<u16>,
) -> Result<bool, ServiceError> {
    dependent_globals_shape(before)?;
    dependent_globals_shape(actual)?;
    let mut expected = before.records().to_vec();
    if let Some(latch) = expected_latch {
        if latch > 1 {
            return Err(ServiceError::invalid(
                "Dependent model latch projection must be0/1",
            ));
        }
        expected[3].body[2..4].copy_from_slice(&latch.to_le_bytes());
    }
    let expected = RawTransfer::from_records(before.kind(), &expected)
        .map_err(|error| ServiceError::invalid(error.to_string()))?;
    let expected = GlobalRecords::from_transfer(expected)
        .map_err(|error| ServiceError::invalid(error.to_string()))?;
    let actual = GlobalRecords::from_transfer(actual.clone())
        .map_err(|error| ServiceError::invalid(error.to_string()))?;
    Ok(expected.control_state_equal(&actual))
}
