//! Device-wide records recovered from the original GP-200 editor.
//!
//! These records have no preset-context prefix. Raw bundles remain authoritative;
//! decoding does not grant hardware capability or manufacture a firmware identity.
use crate::{F32Bits, READ_KIND, RawError, RawRecord, RawTransfer, WRITE_KIND};
use serde::{Deserialize, Serialize};

pub const GENERAL_SETTINGS_COMMAND: u16 = 0x1007;
pub const GLOBAL_EQ_COMMAND: u16 = 0x1004;
pub const DRUM_SETTINGS_COMMAND: u16 = 0x1002;
pub const RUNTIME_STATE_COMMAND: u16 = 0x1000;
pub const GLOBAL_FOOTSWITCH_COMMAND: u16 = 0x100e;

/// Original GP-200 popup order and wire IDs, distinct from the complete
/// function-name inventory. Native479636..479804 constructs these lists;
/// 47c000 uses the tap/hold/combined grid groups and47c530 maps them to events.
/// These menus do not establish hardware support or action execution.
pub fn gp200_footswitch_choices(event: GlobalFootswitchEvent) -> &'static [u8] {
    const TAP: &[u8] = &[
        17, 9, 10, 11, 12, 13, 14, 15, 16, 5, 6, 18, 19, 20, 21, 7, 1, 2, 3, 4, 23, 24, 25, 26, 8,
        0,
    ];
    const HOLD: &[u8] = &[
        17, 9, 10, 11, 12, 13, 14, 15, 16, 5, 6, 18, 19, 20, 21, 7, 0,
    ];
    const COMBINED: &[u8] = &[
        17, 9, 10, 11, 12, 13, 14, 15, 16, 5, 6, 18, 19, 20, 21, 7, 1, 2, 3, 4, 23, 24, 25, 26, 0,
    ];
    match event.value() {
        16..=19 => COMBINED,
        value if value % 2 == 0 => TAP,
        _ => HOLD,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GlobalError {
    #[error("command {command:#06x} needs {expected} body bytes, got {actual}")]
    InvalidLength {
        command: u16,
        expected: usize,
        actual: usize,
    },
    #[error("duplicate global record {0:#06x}")]
    DuplicateRecord(u16),
    #[error("duplicate global footswitch table {0}")]
    DuplicateFootswitchTable(u16),
    #[error("record command does not match the requested global type")]
    WrongCommand,
    #[error("global setting value is outside its recovered range")]
    InvalidValue,
    #[error("global EQ parameter must be 0..30")]
    InvalidEqParameter,
    #[error("a complete global baseline is required")]
    MissingBaseline,
    #[error(transparent)]
    Raw(#[from] RawError),
}

fn exact(record: &RawRecord, command: u16, length: usize) -> Result<(), GlobalError> {
    if record.command != command {
        return Err(GlobalError::WrongCommand);
    }
    if record.body.len() != length {
        return Err(GlobalError::InvalidLength {
            command,
            expected: length,
            actual: record.body.len(),
        });
    }
    Ok(())
}

/// Recovered scalar controls. Selector0 (preset audition), reset/calibration,
/// and unidentified selectors are deliberately absent from this edit type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum GeneralSetting {
    InputLevel,
    InputMode,
    NoCabLeft,
    NoCabRight,
    RecordingLevel,
    RecordingModeLeft,
    RecordingModeRight,
    AuxToUsb,
    MonitorLevel,
    ExternalMode,
    MidiClockSource,
    MidiInputUsb,
    MidiInputDin,
    MidiOutputUsb,
    MidiOutputDin,
    MidiInputSource,
    MidiClockOutUsb,
    MidiClockOutDin,
    FootswitchTemplate,
    ExternalFootswitch1,
    ExternalFootswitch2,
    FootswitchMode,
    AutoCabMatch,
    DisplayMode,
}

impl GeneralSetting {
    pub const ALL: [Self; 24] = [
        Self::InputLevel,
        Self::InputMode,
        Self::NoCabLeft,
        Self::NoCabRight,
        Self::RecordingLevel,
        Self::RecordingModeLeft,
        Self::RecordingModeRight,
        Self::AuxToUsb,
        Self::MonitorLevel,
        Self::ExternalMode,
        Self::MidiClockSource,
        Self::MidiInputUsb,
        Self::MidiInputDin,
        Self::MidiOutputUsb,
        Self::MidiOutputDin,
        Self::MidiInputSource,
        Self::MidiClockOutUsb,
        Self::MidiClockOutDin,
        Self::FootswitchTemplate,
        Self::ExternalFootswitch1,
        Self::ExternalFootswitch2,
        Self::FootswitchMode,
        Self::AutoCabMatch,
        Self::DisplayMode,
    ];
    pub const fn selector(self) -> u16 {
        match self {
            Self::InputLevel => 1,
            Self::InputMode => 2,
            Self::NoCabLeft => 3,
            Self::NoCabRight => 4,
            Self::RecordingLevel => 5,
            Self::RecordingModeLeft => 6,
            Self::RecordingModeRight => 7,
            Self::AuxToUsb => 8,
            Self::MonitorLevel => 9,
            Self::ExternalMode => 12,
            Self::MidiInputSource => 14,
            Self::MidiInputUsb => 15,
            Self::MidiInputDin => 16,
            Self::MidiOutputUsb => 17,
            Self::MidiOutputDin => 18,
            Self::MidiClockSource => 19,
            Self::MidiClockOutUsb => 20,
            Self::MidiClockOutDin => 21,
            Self::FootswitchTemplate => 24,
            Self::ExternalFootswitch1 => 31,
            Self::ExternalFootswitch2 => 33,
            Self::FootswitchMode => 35,
            Self::AutoCabMatch => 36,
            Self::DisplayMode => 40,
        }
    }
    /// Offset in cmd1007's body, excluding its four-byte TLV header.
    pub const fn body_offset(self) -> usize {
        match self {
            Self::DisplayMode => 57,
            Self::AutoCabMatch => 70,
            Self::ExternalMode => 45,
            Self::FootswitchTemplate => 48,
            Self::ExternalFootswitch1 => 60,
            Self::ExternalFootswitch2 => 64,
            Self::FootswitchMode => 68,
            Self::MidiClockSource
            | Self::MidiInputUsb
            | Self::MidiInputDin
            | Self::MidiOutputUsb
            | Self::MidiOutputDin
            | Self::MidiInputSource
            | Self::MidiClockOutUsb
            | Self::MidiClockOutDin => self.selector() as usize * 2,
            _ => 4 + self.selector() as usize * 2,
        }
    }
    pub const fn body_width(self) -> usize {
        if matches!(self, Self::DisplayMode | Self::ExternalMode) {
            1
        } else {
            2
        }
    }
    pub const fn label(self) -> &'static str {
        match self {
            Self::InputLevel => "Input level",
            Self::InputMode => "Input mode",
            Self::NoCabLeft => "No Cab (left)",
            Self::NoCabRight => "No Cab (right)",
            Self::RecordingLevel => "USB recording level",
            Self::RecordingModeLeft => "USB recording mode (left)",
            Self::RecordingModeRight => "USB recording mode (right)",
            Self::AuxToUsb => "AUX to USB",
            Self::MonitorLevel => "USB monitor level",
            Self::ExternalMode => "EXP2 / external footswitch mode",
            Self::MidiClockSource => "MIDI clock source",
            Self::MidiInputUsb => "MIDI input channel (USB)",
            Self::MidiInputDin => "MIDI input channel (DIN)",
            Self::MidiOutputUsb => "MIDI output channel (USB)",
            Self::MidiOutputDin => "MIDI output channel (DIN)",
            Self::MidiInputSource => "MIDI input source",
            Self::MidiClockOutUsb => "MIDI clock output (USB)",
            Self::MidiClockOutDin => "MIDI clock output (DIN)",
            Self::FootswitchTemplate => "Footswitch template",
            Self::ExternalFootswitch1 => "External footswitch 1",
            Self::ExternalFootswitch2 => "External footswitch 2",
            Self::FootswitchMode => "Footswitch mode",
            Self::AutoCabMatch => "Auto Cab Match",
            Self::DisplayMode => "Pedal display mode",
        }
    }
    pub fn validate(self, value: i16) -> Result<(), GlobalError> {
        let valid = match self {
            Self::InputLevel | Self::RecordingLevel | Self::MonitorLevel => {
                (-20..=20).contains(&value)
            }
            Self::InputMode
            | Self::MidiInputSource
            | Self::DisplayMode
            | Self::ExternalMode
            | Self::FootswitchTemplate
            | Self::FootswitchMode => (0..=2).contains(&value),
            Self::ExternalFootswitch1 | Self::ExternalFootswitch2 => (0..=26).contains(&value),
            Self::MidiClockSource => (0..=4).contains(&value),
            // Original4824f0 skips OFF for selectors15/16. Its output menu
            // retains both OFF(-1) and Omni(0); do not invent channel semantics.
            Self::MidiInputUsb | Self::MidiInputDin => (0..=16).contains(&value),
            Self::MidiOutputUsb | Self::MidiOutputDin => (-1..=16).contains(&value),
            _ => (0..=1).contains(&value),
        };
        if valid {
            Ok(())
        } else {
            Err(GlobalError::InvalidValue)
        }
    }
    pub const fn choices(self) -> &'static [&'static str] {
        match self {
            Self::InputMode => &["A.GT", "E.GT", "Line"],
            Self::RecordingModeLeft | Self::RecordingModeRight => &["Dry", "Effect"],
            Self::MidiClockSource => &["Internal", "DIN Only", "USB Only", "External", "Mixed"],
            Self::MidiInputSource => &["DIN Only", "USB Only", "Mixed"],
            Self::DisplayMode => &["Footswitch", "Patch", "Signal Chain"],
            Self::ExternalMode => &["Pedal", "Single Footswitch", "Double Footswitch"],
            Self::FootswitchMode | Self::FootswitchTemplate => {
                &["Preset Mode", "Stomp Mode", "User Mode"]
            }
            Self::ExternalFootswitch1 | Self::ExternalFootswitch2 => {
                &GLOBAL_FOOTSWITCH_FUNCTION_NAMES
            }
            _ => &[],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RawGeneralSettings(Vec<u8>);

impl RawGeneralSettings {
    pub fn from_record(record: &RawRecord) -> Result<Self, GlobalError> {
        exact(record, GENERAL_SETTINGS_COMMAND, 72)?;
        Ok(Self(record.body.clone()))
    }
    pub fn body(&self) -> &[u8] {
        &self.0
    }
    pub fn to_record(&self) -> RawRecord {
        RawRecord {
            command: GENERAL_SETTINGS_COMMAND,
            body: self.0.clone(),
        }
    }
    pub fn value(&self, field: GeneralSetting) -> i16 {
        let p = field.body_offset();
        if field.body_width() == 1 {
            i16::from(self.0[p])
        } else {
            i16::from_le_bytes([self.0[p], self.0[p + 1]])
        }
    }
    /// Pure simulation updates only the established bytes; unknown values and
    /// padding are retained verbatim. This is not a whole-record hardware write.
    pub fn with_value(&self, field: GeneralSetting, value: i16) -> Result<Self, GlobalError> {
        field.validate(value)?;
        let mut result = self.clone();
        let p = field.body_offset();
        if field.body_width() == 1 {
            result.0[p] = value as u8;
        } else {
            result.0[p..p + 2].copy_from_slice(&value.to_le_bytes());
        }
        Ok(result)
    }
}

impl<'de> Deserialize<'de> for RawGeneralSettings {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let body = Vec::<u8>::deserialize(d)?;
        Self::from_record(&RawRecord {
            command: GENERAL_SETTINGS_COMMAND,
            body,
        })
        .map_err(serde::de::Error::custom)
    }
}

/// Exact IEEE-754 bits for the 31-field global EQ. It is independent of the
/// preset EQ module. Hidden shelf fields remain intact even when not displayed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawGlobalEq {
    pub values: [F32Bits; 31],
}
impl RawGlobalEq {
    pub fn from_record(record: &RawRecord) -> Result<Self, GlobalError> {
        exact(record, GLOBAL_EQ_COMMAND, 124)?;
        Ok(Self {
            values: std::array::from_fn(|i| {
                F32Bits::from_le_bytes(record.body[i * 4..i * 4 + 4].try_into().unwrap())
            }),
        })
    }
    pub fn to_record(&self) -> RawRecord {
        RawRecord {
            command: GLOBAL_EQ_COMMAND,
            body: self.values.iter().flat_map(|v| v.to_le_bytes()).collect(),
        }
    }
    pub fn with_value(&self, parameter: EqParameter, value: f32) -> Result<Self, GlobalError> {
        parameter.validate(value)?;
        let mut result = self.clone();
        result.values[parameter.index() as usize] = F32Bits::from_f32(value);
        Ok(result)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum EqParameter {
    LowCutEnabled,
    LowCutFrequency,
    LowCutQ,
    LowShelfEnabled,
    LowShelfFrequency,
    LowShelfQ,
    LowShelfGain,
    Band1Enabled,
    Band1Frequency,
    Band1Q,
    Band1Gain,
    Band2Enabled,
    Band2Frequency,
    Band2Q,
    Band2Gain,
    Band3Enabled,
    Band3Frequency,
    Band3Q,
    Band3Gain,
    Band4Enabled,
    Band4Frequency,
    Band4Q,
    Band4Gain,
    HighShelfEnabled,
    HighShelfFrequency,
    HighShelfQ,
    HighShelfGain,
    HighCutEnabled,
    HighCutFrequency,
    HighCutQ,
    Level,
}
impl EqParameter {
    pub const ALL: [Self; 31] = [
        Self::LowCutEnabled,
        Self::LowCutFrequency,
        Self::LowCutQ,
        Self::LowShelfEnabled,
        Self::LowShelfFrequency,
        Self::LowShelfQ,
        Self::LowShelfGain,
        Self::Band1Enabled,
        Self::Band1Frequency,
        Self::Band1Q,
        Self::Band1Gain,
        Self::Band2Enabled,
        Self::Band2Frequency,
        Self::Band2Q,
        Self::Band2Gain,
        Self::Band3Enabled,
        Self::Band3Frequency,
        Self::Band3Q,
        Self::Band3Gain,
        Self::Band4Enabled,
        Self::Band4Frequency,
        Self::Band4Q,
        Self::Band4Gain,
        Self::HighShelfEnabled,
        Self::HighShelfFrequency,
        Self::HighShelfQ,
        Self::HighShelfGain,
        Self::HighCutEnabled,
        Self::HighCutFrequency,
        Self::HighCutQ,
        Self::Level,
    ];
    pub const fn index(self) -> u16 {
        self as u16
    }
    pub fn from_index(index: u16) -> Result<Self, GlobalError> {
        Self::ALL
            .get(index as usize)
            .copied()
            .ok_or(GlobalError::InvalidEqParameter)
    }
    pub const fn bounds(self) -> (f32, f32) {
        match self {
            Self::LowCutEnabled
            | Self::LowShelfEnabled
            | Self::Band1Enabled
            | Self::Band2Enabled
            | Self::Band3Enabled
            | Self::Band4Enabled
            | Self::HighShelfEnabled
            | Self::HighCutEnabled => (0., 1.),
            Self::LowCutFrequency => (19., 20000.),
            Self::HighCutFrequency => (20., 20001.),
            Self::LowShelfFrequency
            | Self::Band1Frequency
            | Self::Band2Frequency
            | Self::Band3Frequency
            | Self::Band4Frequency
            | Self::HighShelfFrequency => (20., 20000.),
            Self::LowCutQ
            | Self::LowShelfQ
            | Self::Band1Q
            | Self::Band2Q
            | Self::Band3Q
            | Self::Band4Q
            | Self::HighShelfQ
            | Self::HighCutQ => (0.1, 10.),
            Self::Level => (0., 100.),
            _ => (-20., 20.),
        }
    }
    pub fn validate(self, value: f32) -> Result<(), GlobalError> {
        let (min, max) = self.bounds();
        let valid = value.is_finite()
            && (min..=max).contains(&value)
            && (!self.is_switch() || value == 0. || value == 1.);
        if valid {
            Ok(())
        } else {
            Err(GlobalError::InvalidValue)
        }
    }
    pub const fn is_switch(self) -> bool {
        matches!(
            self,
            Self::LowCutEnabled
                | Self::LowShelfEnabled
                | Self::Band1Enabled
                | Self::Band2Enabled
                | Self::Band3Enabled
                | Self::Band4Enabled
                | Self::HighShelfEnabled
                | Self::HighCutEnabled
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DrumSetting {
    Pattern,
    Volume,
    Tempo,
}
impl DrumSetting {
    pub const fn selector(self) -> u16 {
        match self {
            Self::Pattern => 0,
            Self::Volume => 1,
            Self::Tempo => 2,
        }
    }
    pub const fn body_offset(self) -> usize {
        match self {
            Self::Pattern => 0,
            Self::Tempo => 2,
            Self::Volume => 4,
        }
    }
    pub fn validate(self, value: u16) -> Result<(), GlobalError> {
        let valid = match self {
            Self::Pattern => value < 100,
            Self::Volume => value <= 100,
            // Tempo widget range is separately constrained by the desktop editor.
            Self::Tempo => (40..=250).contains(&value),
        };
        if valid {
            Ok(())
        } else {
            Err(GlobalError::InvalidValue)
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawDrumSettings {
    body: [u8; 8],
}
impl RawDrumSettings {
    pub fn from_record(record: &RawRecord) -> Result<Self, GlobalError> {
        exact(record, DRUM_SETTINGS_COMMAND, 8)?;
        Ok(Self {
            body: record.body.as_slice().try_into().unwrap(),
        })
    }
    pub fn body(&self) -> &[u8; 8] {
        &self.body
    }
    pub fn to_record(&self) -> RawRecord {
        RawRecord {
            command: DRUM_SETTINGS_COMMAND,
            body: self.body.to_vec(),
        }
    }
    pub fn value(&self, field: DrumSetting) -> u16 {
        let p = field.body_offset();
        u16::from_le_bytes([self.body[p], self.body[p + 1]])
    }
    pub fn with_value(&self, field: DrumSetting, value: u16) -> Result<Self, GlobalError> {
        field.validate(value)?;
        let mut result = self.clone();
        let p = field.body_offset();
        result.body[p..p + 2].copy_from_slice(&value.to_le_bytes());
        Ok(result)
    }
}

/// Runtime cmd1000 is not a firmware version. The first word observes
/// DrumView play/stop; the second is the original editor's preset-changed latch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawGlobalRuntime {
    body: [u8; 6],
}
impl RawGlobalRuntime {
    pub fn from_record(record: &RawRecord) -> Result<Self, GlobalError> {
        exact(record, RUNTIME_STATE_COMMAND, 6)?;
        Ok(Self {
            body: record.body.as_slice().try_into().unwrap(),
        })
    }
    pub fn body(&self) -> &[u8; 6] {
        &self.body
    }
    pub fn to_record(&self) -> RawRecord {
        RawRecord {
            command: RUNTIME_STATE_COMMAND,
            body: self.body.to_vec(),
        }
    }
    pub fn drum_playing(&self) -> Option<bool> {
        match u16::from_le_bytes([self.body[0], self.body[1]]) {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
    /// Native PresetManager consumer 5bc5a0 and save-before-change guard 5d810b.
    /// Zero is an observed latch state, never confirmation of a persistent Save.
    pub fn preset_changed(&self) -> Option<bool> {
        match u16::from_le_bytes([self.body[2], self.body[3]]) {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
}

/// Three independent global templates. This is not a stored preset slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct GlobalFootswitchTableId(u16);
impl GlobalFootswitchTableId {
    pub fn new(value: u16) -> Result<Self, GlobalError> {
        if value < 3 {
            Ok(Self(value))
        } else {
            Err(GlobalError::InvalidValue)
        }
    }
    pub const fn value(self) -> u16 {
        self.0
    }
}
impl<'de> Deserialize<'de> for GlobalFootswitchTableId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(u16::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

/// Wire ordering is tap/hold pairs for FS1..8, then the four combinations.
/// The original grid converts its row order through helper47c530.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct GlobalFootswitchEvent(u8);
impl GlobalFootswitchEvent {
    pub fn new(value: u8) -> Result<Self, GlobalError> {
        if value < 20 {
            Ok(Self(value))
        } else {
            Err(GlobalError::InvalidValue)
        }
    }
    pub const fn value(self) -> u8 {
        self.0
    }
    pub const fn label(self) -> &'static str {
        GLOBAL_FOOTSWITCH_EVENT_NAMES[self.0 as usize]
    }
}
impl<'de> Deserialize<'de> for GlobalFootswitchEvent {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(u8::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
pub const GLOBAL_FOOTSWITCH_EVENT_NAMES: [&str; 20] = [
    "FS1 tap",
    "FS1 hold",
    "FS2 tap",
    "FS2 hold",
    "FS3 tap",
    "FS3 hold",
    "FS4 tap",
    "FS4 hold",
    "FS5 tap",
    "FS5 hold",
    "FS6 tap",
    "FS6 hold",
    "FS7 tap",
    "FS7 hold",
    "FS8 tap",
    "FS8 hold",
    "FS1 + FS5",
    "FS5 + FS6",
    "FS6 + FS7",
    "FS7 + FS8",
];
pub const GLOBAL_FOOTSWITCH_FUNCTION_NAMES: [&str; 27] = [
    "None",
    "CTRL 1",
    "CTRL 2",
    "CTRL 3",
    "CTRL 4",
    "Looper",
    "Drum",
    "Tuner",
    "TAP",
    "Bank+",
    "Bank-",
    "Patch+",
    "Patch-",
    "Patch A",
    "Patch B",
    "Patch C",
    "Patch D",
    "Bank",
    "Drum Patch +",
    "Drum Patch -",
    "EXP1 A/B",
    "MIDI",
    "Bypass",
    "CTRL 5",
    "CTRL 6",
    "CTRL 7",
    "CTRL 8",
];
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct GlobalFootswitchFunction(u8);
impl GlobalFootswitchFunction {
    pub fn new(value: u8) -> Result<Self, GlobalError> {
        if value < 27 {
            Ok(Self(value))
        } else {
            Err(GlobalError::InvalidValue)
        }
    }
    pub const fn value(self) -> u8 {
        self.0
    }
    pub const fn label(self) -> &'static str {
        GLOBAL_FOOTSWITCH_FUNCTION_NAMES[self.0 as usize]
    }
}
impl<'de> Deserialize<'de> for GlobalFootswitchFunction {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(u8::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

/// Cmd100E body40: table index, retained opaque metadata, twenty functions.
/// Unknown function values remain readable; edits accept only recovered codes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RawGlobalFootswitchTable(Vec<u8>);
impl RawGlobalFootswitchTable {
    pub fn from_record(record: &RawRecord) -> Result<Self, GlobalError> {
        exact(record, GLOBAL_FOOTSWITCH_COMMAND, 40)?;
        Ok(Self(record.body.clone()))
    }
    pub fn body(&self) -> &[u8] {
        &self.0
    }
    pub fn table_raw(&self) -> u16 {
        u16::from_le_bytes([self.0[0], self.0[1]])
    }
    pub fn function_raw(&self, event: GlobalFootswitchEvent) -> u8 {
        self.0[20 + event.value() as usize]
    }
    pub fn to_record(&self) -> RawRecord {
        RawRecord {
            command: GLOBAL_FOOTSWITCH_COMMAND,
            body: self.0.clone(),
        }
    }
    pub fn with_function(
        &self,
        event: GlobalFootswitchEvent,
        function: GlobalFootswitchFunction,
    ) -> Self {
        let mut changed = self.clone();
        changed.0[20 + event.value() as usize] = function.value();
        changed
    }
}
impl<'de> Deserialize<'de> for RawGlobalFootswitchTable {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::from_record(&RawRecord {
            command: GLOBAL_FOOTSWITCH_COMMAND,
            body: Vec::<u8>::deserialize(d)?,
        })
        .map_err(serde::de::Error::custom)
    }
}

/// Optional projections of a retained ordered transfer. Duplicate recognized
/// singleton records are rejected, while unknown commands and all bytes survive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GlobalRecords {
    raw: RawTransfer,
    #[serde(skip)]
    pub general: Option<RawGeneralSettings>,
    #[serde(skip)]
    pub eq: Option<RawGlobalEq>,
    #[serde(skip)]
    pub drum: Option<RawDrumSettings>,
    #[serde(skip)]
    pub runtime: Option<RawGlobalRuntime>,
    #[serde(skip)]
    pub footswitch_tables: Vec<RawGlobalFootswitchTable>,
}
impl GlobalRecords {
    pub fn from_transfer(raw: RawTransfer) -> Result<Self, GlobalError> {
        let mut result = Self {
            raw,
            general: None,
            eq: None,
            drum: None,
            runtime: None,
            footswitch_tables: vec![],
        };
        for record in result.raw.records() {
            match record.command {
                GENERAL_SETTINGS_COMMAND => {
                    if result.general.is_some() {
                        return Err(GlobalError::DuplicateRecord(record.command));
                    }
                    result.general = Some(RawGeneralSettings::from_record(record)?);
                }
                GLOBAL_EQ_COMMAND => {
                    if result.eq.is_some() {
                        return Err(GlobalError::DuplicateRecord(record.command));
                    }
                    result.eq = Some(RawGlobalEq::from_record(record)?);
                }
                DRUM_SETTINGS_COMMAND => {
                    if result.drum.is_some() {
                        return Err(GlobalError::DuplicateRecord(record.command));
                    }
                    result.drum = Some(RawDrumSettings::from_record(record)?);
                }
                RUNTIME_STATE_COMMAND => {
                    if result.runtime.is_some() {
                        return Err(GlobalError::DuplicateRecord(record.command));
                    }
                    result.runtime = Some(RawGlobalRuntime::from_record(record)?);
                }
                GLOBAL_FOOTSWITCH_COMMAND => {
                    let table = RawGlobalFootswitchTable::from_record(record)?;
                    if result
                        .footswitch_tables
                        .iter()
                        .any(|t| t.table_raw() == table.table_raw())
                    {
                        return Err(GlobalError::DuplicateFootswitchTable(table.table_raw()));
                    }
                    result.footswitch_tables.push(table);
                }
                _ => {}
            }
        }
        Ok(result)
    }
    pub fn raw(&self) -> &RawTransfer {
        &self.raw
    }
    pub fn is_complete(&self) -> bool {
        self.general.is_some() && self.eq.is_some() && self.drum.is_some() && self.runtime.is_some()
    }
    pub fn footswitch_table(
        &self,
        table: GlobalFootswitchTableId,
    ) -> Option<&RawGlobalFootswitchTable> {
        self.footswitch_tables
            .iter()
            .find(|t| t.table_raw() == table.value())
    }
    /// Equality for the five-read GP-200 capture profile, not raw equality.
    /// Only cmd1007 offsets54/55 (observed varying opaque),56 (native-skipped),
    /// cmd1002 tail6/7 (native-skipped), and cmdA alignment/name-after-NUL are
    /// excluded. All other bytes and the exact ordered TLV structure must agree.
    /// Callers must gate this policy to the separately verified device profile.
    pub fn control_state_equal(&self, other: &Self) -> bool {
        self.raw.kind() == other.raw.kind()
            && self.raw.records().len() == other.raw.records().len()
            && self
                .raw
                .records()
                .iter()
                .zip(other.raw.records())
                .all(|(a, b)| {
                    a.command == b.command
                        && a.body.len() == b.body.len()
                        && comparable_body(a) == comparable_body(b)
                })
    }
}

fn comparable_body(record: &RawRecord) -> Vec<u8> {
    let mut b = record.body.clone();
    match record.command {
        GENERAL_SETTINGS_COMMAND if b.len() == 72 => b[54..57].fill(0),
        DRUM_SETTINGS_COMMAND if b.len() == 8 => b[6..8].fill(0),
        0x000a if b.len() == 20 => {
            b[2..4].fill(0);
            if let Some(nul) = b[4..].iter().position(|v| *v == 0) {
                b[5 + nul..].fill(0);
            }
        }
        _ => {}
    }
    b
}
impl<'de> Deserialize<'de> for GlobalRecords {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Stored {
            raw: RawTransfer,
        }
        let stored = Stored::deserialize(d)?;
        Self::from_transfer(stored.raw).map_err(serde::de::Error::custom)
    }
}

/// Original GET global path5d7730: an empty1006 record, kind11, no context.
pub fn read_globals_request() -> Result<RawTransfer, GlobalError> {
    Ok(RawTransfer::from_records(
        READ_KIND,
        &[RawRecord {
            command: 0x1006,
            body: vec![],
        }],
    )?)
}
fn scalar_request(command: u16, selector: u16, value: u16) -> Result<RawTransfer, GlobalError> {
    let mut body = selector.to_le_bytes().to_vec();
    body.extend_from_slice(&value.to_le_bytes());
    Ok(RawTransfer::from_records(
        WRITE_KIND,
        &[RawRecord { command, body }],
    )?)
}
pub fn general_setting_request(
    field: GeneralSetting,
    value: i16,
) -> Result<RawTransfer, GlobalError> {
    field.validate(value)?;
    scalar_request(0x1008, field.selector(), value as u16)
}
pub fn global_eq_request(parameter: EqParameter, value: f32) -> Result<RawTransfer, GlobalError> {
    parameter.validate(value)?;
    let mut body = parameter.index().to_le_bytes().to_vec();
    // Original59f5c0 leaves this padding word unspecified. Zero is deterministic;
    // the actor must independently verify acceptance on the connected profile.
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(&value.to_le_bytes());
    Ok(RawTransfer::from_records(
        WRITE_KIND,
        &[RawRecord {
            command: 0x1005,
            body,
        }],
    )?)
}
pub fn drum_setting_request(field: DrumSetting, value: u16) -> Result<RawTransfer, GlobalError> {
    field.validate(value)?;
    scalar_request(0x1001, field.selector(), value)
}
pub fn drum_play_request(playing: bool) -> Result<RawTransfer, GlobalError> {
    scalar_request(0x1011, 0, u16::from(playing))
}

/// Original47c467: exact four-byte template/event/function command, no context.
pub fn global_footswitch_request(
    table: GlobalFootswitchTableId,
    event: GlobalFootswitchEvent,
    function: GlobalFootswitchFunction,
) -> Result<RawTransfer, GlobalError> {
    let mut body = table.value().to_le_bytes().to_vec();
    body.extend_from_slice(&[event.value(), function.value()]);
    Ok(RawTransfer::from_records(
        WRITE_KIND,
        &[RawRecord {
            command: 0x100f,
            body,
        }],
    )?)
}
