//! Typed device-global state, separate from preset documents and preset targets.
use crate::{EvidenceRef, F32Bits, Revision, SessionEpoch};
use gp200_protocol::{
    DrumSetting, EqParameter, GeneralSetting, GlobalError, GlobalFootswitchEvent,
    GlobalFootswitchFunction, GlobalFootswitchTableId, GlobalRecords, RawTransfer,
    drum_play_request, drum_setting_request, general_setting_request, global_eq_request,
    global_footswitch_request, gp200_footswitch_choices,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GlobalControlEdit {
    General {
        field: GeneralSetting,
        value: i16,
    },
    Eq {
        parameter: EqParameter,
        value: F32Bits,
    },
    Drum {
        field: DrumSetting,
        value: u16,
    },
    DrumPlay(bool),
    Footswitch {
        table: GlobalFootswitchTableId,
        event: GlobalFootswitchEvent,
        function: GlobalFootswitchFunction,
    },
}

impl GlobalControlEdit {
    /// Pure construction only. Actors must check epoch, revision, field-specific
    /// validation profile, and fresh readback before confirming a hardware edit.
    pub fn request(&self) -> Result<RawTransfer, GlobalError> {
        match *self {
            Self::General { field, value } => general_setting_request(field, value),
            Self::Eq { parameter, value } => global_eq_request(parameter, value.as_f32()),
            Self::Drum { field, value } => drum_setting_request(field, value),
            Self::DrumPlay(playing) => drum_play_request(playing),
            Self::Footswitch {
                table,
                event,
                function,
            } => global_footswitch_request(table, event, function),
        }
    }
}

/// A bounded storage edit in an inactive GP-200 switch table. This validates
/// the recovered menu and a particular complete global baseline, not device
/// identity, physical action, persistence, or hardware capability. The actor
/// repeats validation against its fresh preflight before sending anything.
/// Private fields and no Deserialize implementation prevent a saved object
/// from supplying validation authority for another session or revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedInactiveFootswitchEdit {
    edit: GlobalControlEdit,
    epoch: SessionEpoch,
    revision: Revision,
    original: u8,
    baseline: Arc<GlobalRecords>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InactiveFootswitchError {
    #[error("Choose a footswitch mapping edit.")]
    WrongEdit,
    #[error("Read complete global settings and a known active switch mode first.")]
    UnknownMode,
    #[error("Choose an inactive table; this table is the pedal's current switch mode.")]
    ActiveTable,
    #[error("This function is outside the native menu for this tap, hold or combined event.")]
    UnsupportedFunction,
    #[error(
        "The retained function is outside this event's native menu; keep it unchanged until its profile is understood."
    )]
    UnsupportedBaseline,
    #[error("Read this complete switch table before editing it.")]
    MissingTable,
    #[error("The global session or revision changed; refresh this mapping before applying it.")]
    Stale,
}

impl ValidatedInactiveFootswitchEdit {
    pub fn validate(
        edit: &GlobalControlEdit,
        state: &DeviceGlobalState,
    ) -> Result<Self, InactiveFootswitchError> {
        let GlobalControlEdit::Footswitch {
            table,
            event,
            function,
        } = edit
        else {
            return Err(InactiveFootswitchError::WrongEdit);
        };
        let mode = state
            .records()
            .general
            .as_ref()
            .map(|general| general.value(GeneralSetting::FootswitchMode))
            .filter(|mode| (0..=2).contains(mode))
            .ok_or(InactiveFootswitchError::UnknownMode)?;
        if table.value() == mode as u16 {
            return Err(InactiveFootswitchError::ActiveTable);
        }
        let choices = gp200_footswitch_choices(*event);
        if !choices.contains(&function.value()) {
            return Err(InactiveFootswitchError::UnsupportedFunction);
        }
        let original = state
            .records()
            .footswitch_table(*table)
            .ok_or(InactiveFootswitchError::MissingTable)?
            .function_raw(*event);
        if !choices.contains(&original) {
            return Err(InactiveFootswitchError::UnsupportedBaseline);
        }
        Ok(Self {
            edit: edit.clone(),
            epoch: state.epoch,
            revision: state.revision,
            original,
            baseline: Arc::clone(&state.records),
        })
    }

    pub fn edit(&self) -> &GlobalControlEdit {
        &self.edit
    }

    pub fn revalidate(&self, state: &DeviceGlobalState) -> Result<(), InactiveFootswitchError> {
        if self.epoch != state.epoch || self.revision != state.revision {
            return Err(InactiveFootswitchError::Stale);
        }
        let fresh = Self::validate(&self.edit, state)?;
        if fresh.original != self.original || !self.baseline.control_state_equal(state.records()) {
            return Err(InactiveFootswitchError::Stale);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceGlobalState {
    pub revision: Revision,
    pub epoch: SessionEpoch,
    records: Arc<GlobalRecords>,
    pub evidence: Vec<EvidenceRef>,
}

impl DeviceGlobalState {
    pub fn from_transfer(
        revision: Revision,
        epoch: SessionEpoch,
        raw: RawTransfer,
        evidence: Vec<EvidenceRef>,
    ) -> Result<Self, GlobalError> {
        let records = GlobalRecords::from_transfer(raw)?;
        if !records.is_complete() {
            return Err(GlobalError::MissingBaseline);
        }
        Ok(Self {
            revision,
            epoch,
            records: Arc::new(records),
            evidence,
        })
    }
    pub fn records(&self) -> &GlobalRecords {
        &self.records
    }

    /// Lossless expected delta for simulator and verification. This method
    /// does not advance revision or change evidence. Only the affected field is
    /// changed, including preservation of hidden EQ bands and unknown records.
    pub fn expected_after(&self, edit: &GlobalControlEdit) -> Result<RawTransfer, GlobalError> {
        // Validate even when the target record is unexpectedly absent.
        edit.request()?;
        let replacement = match *edit {
            GlobalControlEdit::General { field, value } => self
                .records
                .general
                .as_ref()
                .ok_or(GlobalError::MissingBaseline)?
                .with_value(field, value)?
                .to_record(),
            GlobalControlEdit::Eq { parameter, value } => self
                .records
                .eq
                .as_ref()
                .ok_or(GlobalError::MissingBaseline)?
                .with_value(parameter, value.as_f32())?
                .to_record(),
            GlobalControlEdit::Drum { field, value } => self
                .records
                .drum
                .as_ref()
                .ok_or(GlobalError::MissingBaseline)?
                .with_value(field, value)?
                .to_record(),
            GlobalControlEdit::DrumPlay(playing) => {
                let mut record = self
                    .records
                    .runtime
                    .as_ref()
                    .ok_or(GlobalError::MissingBaseline)?
                    .to_record();
                record.body[..2].copy_from_slice(&u16::from(playing).to_le_bytes());
                record
            }
            GlobalControlEdit::Footswitch {
                table,
                event,
                function,
            } => self
                .records
                .footswitch_table(table)
                .ok_or(GlobalError::MissingBaseline)?
                .with_function(event, function)
                .to_record(),
        };
        let mut records = self.records.raw().records().to_vec();
        let target = records
            .iter_mut()
            .find(|r| {
                r.command == replacement.command
                    && match edit {
                        GlobalControlEdit::Footswitch { table, .. } => {
                            r.body.get(..2) == Some(table.value().to_le_bytes().as_slice())
                        }
                        _ => true,
                    }
            })
            .ok_or(GlobalError::MissingBaseline)?;
        *target = replacement;
        Ok(RawTransfer::from_records(
            self.records.raw().kind(),
            &records,
        )?)
    }

    /// Verify the precise expected delta against a freshly parsed snapshot.
    /// The comparison policy requires the actor's exact captured GP-200 profile.
    /// Raw equality, revision advancement, persistence, and preset isolation
    /// are separate checks; this never promotes a synthetic state to evidence.
    pub fn verifies_expected(
        &self,
        edit: &GlobalControlEdit,
        actual: &Self,
    ) -> Result<bool, GlobalError> {
        if self.epoch != actual.epoch {
            return Ok(false);
        }
        let expected = GlobalRecords::from_transfer(self.expected_after(edit)?)?;
        Ok(expected.control_state_equal(actual.records()))
    }
}

impl<'de> Deserialize<'de> for DeviceGlobalState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Stored {
            revision: Revision,
            epoch: SessionEpoch,
            records: GlobalRecords,
            evidence: Vec<EvidenceRef>,
        }
        let value = Stored::deserialize(deserializer)?;
        Self::from_transfer(
            value.revision,
            value.epoch,
            value.records.raw().clone(),
            value.evidence,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// Original desktop drum menu is zero-based on the wire. These are the eight
/// group ranges; the slot numbers remain authoritative across localized names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrumPatternGroup {
    pub name: &'static str,
    pub first: u16,
    pub count: u16,
}
pub const DRUM_PATTERN_GROUPS: [DrumPatternGroup; 8] = [
    DrumPatternGroup {
        name: "Rock",
        first: 0,
        count: 30,
    },
    DrumPatternGroup {
        name: "Funk",
        first: 30,
        count: 7,
    },
    DrumPatternGroup {
        name: "Blues / Country",
        first: 37,
        count: 10,
    },
    DrumPatternGroup {
        name: "Pop / Hip Hop",
        first: 47,
        count: 8,
    },
    DrumPatternGroup {
        name: "Jazz",
        first: 55,
        count: 7,
    },
    DrumPatternGroup {
        name: "Electronic",
        first: 62,
        count: 7,
    },
    DrumPatternGroup {
        name: "World",
        first: 69,
        count: 21,
    },
    DrumPatternGroup {
        name: "Metronome",
        first: 90,
        count: 10,
    },
];
pub const DRUM_PATTERN_NAMES: [&str; 100] = [
    "Classic Rock 1",
    "Classic Rock 2",
    "Classic Rock 3",
    "Classic Rock 4",
    "Classic Rock 5",
    "Classic Rock 6",
    "Hard Rock 1",
    "Hard Rock 2",
    "Hard Rock 3",
    "Post Rock 1",
    "Post Rock 2",
    "Post Rock 3",
    "Garage Rock",
    "Prog Rock",
    "Surf Rock",
    "Punk 1",
    "Punk 2",
    "Punk 3",
    "Punk 4",
    "Post Punk 1",
    "Post Punk 2",
    "Heavy Metal 1",
    "Heavy Metal 2",
    "Nu Metal 1",
    "Nu Metal 2",
    "Hardcore",
    "EMO",
    "Grunge",
    "New Wave",
    "Rock 5/4",
    "Funk 1",
    "Funk 2",
    "Funk 3",
    "Funk 4",
    "Jazz Funk 1",
    "Jazz Funk 2",
    "Jazz Funk 3",
    "Blues 1",
    "Blues 2",
    "Blues 3",
    "Blues 4",
    "Swing",
    "Shuffle",
    "Shuffle 3/4",
    "Bluegrass",
    "Country",
    "Country Folk",
    "Pop 1",
    "Pop 2",
    "Pop 3",
    "Hip Hop 1",
    "Hip Hop 2",
    "Hip Hop 3",
    "Hip Hop Rock",
    "Pub",
    "Jazz 1",
    "Jazz 2",
    "Jazz 3",
    "Jazz 4",
    "Bossanova 1",
    "Bossanova 2",
    "Fusion",
    "Electro 1",
    "Electro 2",
    "Techno",
    "Trip Hop",
    "Electronic Pop",
    "Break Beat",
    "Drum & Bass",
    "Latin 1",
    "Latin 2",
    "Latin 3",
    "Latin Pop 1",
    "Latin Pop 2",
    "Samba",
    "Tango",
    "Beguine",
    "Ska",
    "Polka",
    "Waltz",
    "Reggae 1",
    "Reggae 2",
    "Mazuke",
    "Musette",
    "March 1",
    "March 2",
    "March 3",
    "New Age 1",
    "New Age 2",
    "World",
    "1/4",
    "2/4",
    "3/4",
    "4/4",
    "5/4",
    "6/4",
    "7/4",
    "6/8",
    "7/8",
    "9/8",
];
