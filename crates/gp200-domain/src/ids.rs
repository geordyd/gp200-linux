use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind} value {value} exceeds maximum {maximum}")]
pub struct IdError {
    pub kind: &'static str,
    pub value: u16,
    pub maximum: u16,
}

macro_rules! bounded_id {
    ($name:ident, $inner:ty, $max:expr) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name($inner);
        impl $name {
            pub fn new(value: $inner) -> Result<Self, IdError> {
                if value as u16 > $max {
                    Err(IdError {
                        kind: stringify!($name),
                        value: value as u16,
                        maximum: $max,
                    })
                } else {
                    Ok(Self(value))
                }
            }
            pub const fn get(self) -> $inner {
                self.0
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Self::new(<$inner>::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}
bounded_id!(SlotId, u16, 255);
bounded_id!(ModuleId, u8, 10);
bounded_id!(ParamId, u8, 14);

impl ModuleId {
    pub const NAMES: [&'static str; 11] = [
        "PRE", "WAH", "DST", "AMP", "NR", "CAB", "EQ", "MOD", "DLY", "RVB", "VOL",
    ];
    pub fn name(self) -> &'static str {
        Self::NAMES[self.0 as usize]
    }
    pub fn all() -> impl Iterator<Item = Self> {
        (0..=10).map(Self)
    }
}

macro_rules! number_id {
    ($name:ident, $inner:ty) => {
        #[derive(
            Debug,
            Default,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            Serialize,
            Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub $inner);
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}
number_id!(ModelCode, u32);
// XML model Index used by command 5; distinct from the command 14/16 Code.
number_id!(ModelIndex, u32);
number_id!(SessionEpoch, u64);
number_id!(IntentId, u64);
number_id!(Revision, u64);
number_id!(JobId, u64);

macro_rules! string_id {
    ($name:ident) => {
        #[derive(
            Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub String);
        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}
string_id!(DocumentId);
string_id!(LibraryId);
string_id!(LibraryVersionId);
string_id!(AssetId);
string_id!(ValidationProfileId);
