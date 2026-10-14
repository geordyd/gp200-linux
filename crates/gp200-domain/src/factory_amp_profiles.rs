//! Exact factory AMP candidate descriptors. Descriptions do not grant I/O authority.
use crate::{
    DeviceGlobalState, ErrorKind, ModelCode, ModuleId, PresetSnapshot, RawTransfer, ServiceError,
    dependent_auto_cab, dependent_globals_shape, dependent_preset_shape,
};

/// The retained editor metadata defining these candidate defaults and CAB bindings.
pub const FACTORY_AMP_METADATA_SHA256: &str =
    "d95d6e019d602e4dafd5b2b0aec7c1494dd52546b0e67f0ee6663d8fa6cb8e0e";
pub const FACTORY_AMP_BASELINE: ModelCode = ModelCode(0x07000015);
pub const FACTORY_AMP_BASELINE_CAB: ModelCode = ModelCode(0x0a000010);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FactoryAmpDescriptor {
    pub model: ModelCode,
    pub xml_index: u16,
    pub paired_cabinet: ModelCode,
    pub defaults: &'static [(u8, u32)],
    pub independently_blocked_controls: &'static [u8],
}

pub const FACTORY_AMP_CANDIDATES: &[FactoryAmpDescriptor] = &[
    FactoryAmpDescriptor {
        model: ModelCode(0x07000003),
        xml_index: 1,
        paired_cabinet: ModelCode(0x0a000016),
        defaults: &[
            (0, 0x41f00000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000024),
        xml_index: 2,
        paired_cabinet: ModelCode(0x0a000016),
        defaults: &[
            (0, 0x420c0000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000004),
        xml_index: 3,
        paired_cabinet: ModelCode(0x0a000012),
        defaults: &[
            (0, 0x420c0000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42200000),
            (4, 0x42700000),
            (5, 0x3f800000),
        ],
        independently_blocked_controls: &[5],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000005),
        xml_index: 4,
        paired_cabinet: ModelCode(0x0a00000b),
        defaults: &[
            (0, 0x41f00000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000009),
        xml_index: 5,
        paired_cabinet: ModelCode(0x0a000004),
        defaults: &[
            (0, 0x41f00000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x3f800000),
        ],
        independently_blocked_controls: &[4],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700000d),
        xml_index: 6,
        paired_cabinet: ModelCode(0x0a000003),
        defaults: &[
            (0, 0x42200000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700000f),
        xml_index: 7,
        paired_cabinet: ModelCode(0x0a000017),
        defaults: &[(0, 0x41f00000), (1, 0x42480000), (2, 0x42480000)],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000028),
        xml_index: 8,
        paired_cabinet: ModelCode(0x0a000017),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000010),
        xml_index: 9,
        paired_cabinet: ModelCode(0x0a000008),
        defaults: &[
            (0, 0x41f00000),
            (1, 0x42700000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000011),
        xml_index: 10,
        paired_cabinet: ModelCode(0x0a00000f),
        defaults: &[
            (0, 0x41f00000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x00000000),
        ],
        independently_blocked_controls: &[3],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000027),
        xml_index: 11,
        paired_cabinet: ModelCode(0x0a00000f),
        defaults: &[
            (0, 0x41f00000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x00000000),
        ],
        independently_blocked_controls: &[5],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000014),
        xml_index: 12,
        paired_cabinet: ModelCode(0x0a000011),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x00000000),
        ],
        independently_blocked_controls: &[4],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000048),
        xml_index: 14,
        paired_cabinet: ModelCode(0x0a000018),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000019),
        xml_index: 15,
        paired_cabinet: ModelCode(0x0a000019),
        defaults: &[
            (0, 0x42200000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700001a),
        xml_index: 17,
        paired_cabinet: ModelCode(0x0a000006),
        defaults: &[
            (0, 0x41f00000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x00000000),
        ],
        independently_blocked_controls: &[5],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700003d),
        xml_index: 18,
        paired_cabinet: ModelCode(0x0a000006),
        defaults: &[
            (0, 0x41f00000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000043),
        xml_index: 19,
        paired_cabinet: ModelCode(0x0a000025),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000044),
        xml_index: 20,
        paired_cabinet: ModelCode(0x0a000025),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700006d),
        xml_index: 21,
        paired_cabinet: ModelCode(0x0a000025),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700006e),
        xml_index: 22,
        paired_cabinet: ModelCode(0x0a000025),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700001b),
        xml_index: 23,
        paired_cabinet: ModelCode(0x0a000029),
        defaults: &[
            (0, 0x420c0000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000049),
        xml_index: 24,
        paired_cabinet: ModelCode(0x0a000029),
        defaults: &[
            (0, 0x41f00000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700001f),
        xml_index: 25,
        paired_cabinet: ModelCode(0x0a000007),
        defaults: &[
            (0, 0x420c0000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700007b),
        xml_index: 26,
        paired_cabinet: ModelCode(0x0a000007),
        defaults: &[
            (0, 0x41a00000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700007c),
        xml_index: 27,
        paired_cabinet: ModelCode(0x0a000007),
        defaults: &[
            (0, 0x41f00000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000022),
        xml_index: 28,
        paired_cabinet: ModelCode(0x0a000007),
        defaults: &[(0, 0x420c0000), (1, 0x42480000), (2, 0x42480000)],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700004b),
        xml_index: 29,
        paired_cabinet: ModelCode(0x0a000007),
        defaults: &[
            (0, 0x41f00000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000023),
        xml_index: 30,
        paired_cabinet: ModelCode(0x0a000028),
        defaults: &[
            (0, 0x41f00000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000047),
        xml_index: 31,
        paired_cabinet: ModelCode(0x0a000028),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000059),
        xml_index: 32,
        paired_cabinet: ModelCode(0x0a000028),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700002a),
        xml_index: 33,
        paired_cabinet: ModelCode(0x0a00001f),
        defaults: &[
            (0, 0x41c80000),
            (1, 0x42820000),
            (2, 0x42480000),
            (3, 0x42340000),
            (4, 0x42480000),
            (5, 0x42820000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700002b),
        xml_index: 34,
        paired_cabinet: ModelCode(0x0a00000e),
        defaults: &[
            (0, 0x42340000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700002d),
        xml_index: 36,
        paired_cabinet: ModelCode(0x0a000021),
        defaults: &[
            (0, 0x42200000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700002e),
        xml_index: 37,
        paired_cabinet: ModelCode(0x0a000021),
        defaults: &[
            (0, 0x42200000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000030),
        xml_index: 39,
        paired_cabinet: ModelCode(0x0a00001b),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000035),
        xml_index: 40,
        paired_cabinet: ModelCode(0x0a000022),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700004e),
        xml_index: 41,
        paired_cabinet: ModelCode(0x0a000022),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000040),
        xml_index: 42,
        paired_cabinet: ModelCode(0x0a000020),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000041),
        xml_index: 43,
        paired_cabinet: ModelCode(0x0a000020),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700005d),
        xml_index: 44,
        paired_cabinet: ModelCode(0x0a000020),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700005e),
        xml_index: 45,
        paired_cabinet: ModelCode(0x0a000020),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000039),
        xml_index: 46,
        paired_cabinet: ModelCode(0x0a000024),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700003a),
        xml_index: 47,
        paired_cabinet: ModelCode(0x0a000024),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700003b),
        xml_index: 48,
        paired_cabinet: ModelCode(0x0a000024),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000055),
        xml_index: 49,
        paired_cabinet: ModelCode(0x0a000024),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000056),
        xml_index: 50,
        paired_cabinet: ModelCode(0x0a000024),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000057),
        xml_index: 51,
        paired_cabinet: ModelCode(0x0a000024),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000068),
        xml_index: 52,
        paired_cabinet: ModelCode(0x0a000024),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000069),
        xml_index: 53,
        paired_cabinet: ModelCode(0x0a000024),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700003e),
        xml_index: 54,
        paired_cabinet: ModelCode(0x0a000029),
        defaults: &[
            (0, 0x41f00000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000053),
        xml_index: 55,
        paired_cabinet: ModelCode(0x0a000029),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700005f),
        xml_index: 57,
        paired_cabinet: ModelCode(0x0a000026),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000060),
        xml_index: 58,
        paired_cabinet: ModelCode(0x0a000026),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000063),
        xml_index: 59,
        paired_cabinet: ModelCode(0x0a000026),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000065),
        xml_index: 60,
        paired_cabinet: ModelCode(0x0a00002e),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000066),
        xml_index: 61,
        paired_cabinet: ModelCode(0x0a00002e),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700006a),
        xml_index: 62,
        paired_cabinet: ModelCode(0x0a00002e),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0700006b),
        xml_index: 63,
        paired_cabinet: ModelCode(0x0a00002e),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000073),
        xml_index: 64,
        paired_cabinet: ModelCode(0x0a00003b),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x3f800000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[3],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000075),
        xml_index: 65,
        paired_cabinet: ModelCode(0x0a000037),
        defaults: &[(0, 0x42480000), (1, 0x42480000), (2, 0x42480000)],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x07000077),
        xml_index: 66,
        paired_cabinet: ModelCode(0x0a000038),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x08000075),
        xml_index: 67,
        paired_cabinet: ModelCode(0x0a000035),
        defaults: &[(0, 0x42480000), (1, 0x42480000), (2, 0x42480000)],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x08000076),
        xml_index: 68,
        paired_cabinet: ModelCode(0x0a000034),
        defaults: &[
            (0, 0x42480000),
            (1, 0x00000000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
        ],
        independently_blocked_controls: &[1],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0800007a),
        xml_index: 69,
        paired_cabinet: ModelCode(0x0a00003c),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
    FactoryAmpDescriptor {
        model: ModelCode(0x0800007b),
        xml_index: 70,
        paired_cabinet: ModelCode(0x0a00003d),
        defaults: &[
            (0, 0x42480000),
            (1, 0x42480000),
            (2, 0x42480000),
            (3, 0x42480000),
            (4, 0x42480000),
            (5, 0x42480000),
        ],
        independently_blocked_controls: &[],
    },
];

pub fn factory_amp_descriptor(model: ModelCode) -> Option<&'static FactoryAmpDescriptor> {
    FACTORY_AMP_CANDIDATES
        .iter()
        .find(|descriptor| descriptor.model == model)
}

/// One exact Match/candidate pair in one Auto CAB context. No caller-set eligibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FactoryAmpProfileId {
    candidate: ModelCode,
    auto_cab: bool,
}
impl FactoryAmpProfileId {
    pub fn new(candidate: ModelCode, auto_cab: bool) -> Result<Self, ServiceError> {
        factory_amp_descriptor(candidate).ok_or_else(|| {
            unverified("This AMP is outside the exact first-phase factory candidate table")
        })?;
        Ok(Self {
            candidate,
            auto_cab,
        })
    }
    pub const fn candidate(self) -> ModelCode {
        self.candidate
    }
    pub const fn auto_cab(self) -> bool {
        self.auto_cab
    }
    pub fn name(self) -> String {
        format!(
            "amp-{:08x}-auto-cab-{}",
            self.candidate.0,
            u8::from(self.auto_cab)
        )
    }
    pub const fn normal_build_accepted(self) -> bool {
        false
    }
}

pub fn factory_amp_pair(module: ModuleId, previous: ModelCode, target: ModelCode) -> bool {
    module.get() == 3
        && ((previous == FACTORY_AMP_BASELINE && factory_amp_descriptor(target).is_some())
            || (target == FACTORY_AMP_BASELINE && factory_amp_descriptor(previous).is_some()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FactoryAmpPlan {
    pub profile: FactoryAmpProfileId,
    pub module: ModuleId,
    pub previous: ModelCode,
    pub target: ModelCode,
    pub auto_cab: bool,
    pub paired_cabinet: Option<ModelCode>,
}
fn unverified(message: &str) -> ServiceError {
    ServiceError::new(ErrorKind::Unverified, message)
}
fn raw_code(raw: &RawTransfer, module: u8) -> ModelCode {
    ModelCode(u32::from_le_bytes(
        raw.records()[3 + usize::from(module)].body[4..8]
            .try_into()
            .expect("complete shape"),
    ))
}
impl FactoryAmpPlan {
    pub fn from_snapshot(
        snapshot: &PresetSnapshot,
        globals: &DeviceGlobalState,
        target: ModelCode,
    ) -> Result<Self, ServiceError> {
        if snapshot.device_model != crate::DeviceModel::Gp200
            || !matches!(snapshot.source, crate::SnapshotSource::Hardware { epoch } if epoch == globals.epoch)
        {
            return Err(unverified(
                "Factory AMP candidates require GP-200 preset and globals from the same hardware session",
            ));
        }
        dependent_preset_shape(&snapshot.raw)?;
        dependent_globals_shape(globals.records().raw())?;
        let context = snapshot
            .context
            .filter(|context| context.group == 0 && context.selector <= 255)
            .ok_or_else(|| {
                unverified("Factory AMP candidates need a confirmed current-buffer context")
            })?;
        if snapshot.raw.records()[0].body[..2] != context.group.to_le_bytes()
            || snapshot.raw.records()[0].body[2..4] != context.selector.to_le_bytes()
            || [1, 2].into_iter().any(|index| {
                snapshot.raw.records()[index].body[..2] != context.selector.to_le_bytes()
            })
        {
            return Err(unverified(
                "Factory AMP candidate raw context and embedded slots differ from the confirmed current buffer",
            ));
        }
        let module = ModuleId::new(3).unwrap();
        let previous = raw_code(&snapshot.raw, 3);
        if !factory_amp_pair(module, previous, target) {
            return Err(unverified(
                "This exact factory AMP pair has no candidate descriptor",
            ));
        }
        if snapshot
            .view
            .module(module)
            .is_none_or(|value| value.model != previous)
            || snapshot
                .view
                .module(ModuleId::new(5).unwrap())
                .is_none_or(|value| value.model != raw_code(&snapshot.raw, 5))
        {
            return Err(unverified(
                "Factory AMP decoded model identities differ from their raw before-image",
            ));
        }
        let candidate = if previous == FACTORY_AMP_BASELINE {
            target
        } else {
            previous
        };
        let descriptor = factory_amp_descriptor(candidate).expect("pair checked");
        let auto_cab = dependent_auto_cab(globals.records().raw())?;
        let profile = FactoryAmpProfileId::new(candidate, auto_cab)?;
        if snapshot
            .raw
            .records()
            .iter()
            .any(|record| matches!(record.command, 0x0c | 0x10) && matches!(record.body[1], 3 | 5))
        {
            return Err(unverified(
                "Assigned AMP/CAB factory candidates await a separate validated profile",
            ));
        }
        let source_cab = if auto_cab && previous != FACTORY_AMP_BASELINE {
            descriptor.paired_cabinet
        } else {
            FACTORY_AMP_BASELINE_CAB
        };
        if raw_code(&snapshot.raw, 5) != source_cab {
            return Err(unverified(
                "Factory AMP candidate requires its exact ordinary paired source cabinet",
            ));
        }
        let paired_cabinet = auto_cab.then_some(if target == FACTORY_AMP_BASELINE {
            FACTORY_AMP_BASELINE_CAB
        } else {
            descriptor.paired_cabinet
        });
        Ok(Self {
            profile,
            module,
            previous,
            target,
            auto_cab,
            paired_cabinet,
        })
    }

    /// Complete synthetic projection. Hardware readback remains independently required.
    pub fn projected_raw(&self, before: &PresetSnapshot) -> Result<RawTransfer, ServiceError> {
        dependent_preset_shape(&before.raw)?;
        if raw_code(&before.raw, 3) != self.previous
            || !factory_amp_pair(self.module, self.previous, self.target)
            || self.profile.candidate()
                != if self.previous == FACTORY_AMP_BASELINE {
                    self.target
                } else {
                    self.previous
                }
            || self.profile.auto_cab() != self.auto_cab
        {
            return Err(unverified(
                "Factory AMP projection does not match its exact source/profile",
            ));
        }
        let mut records = before.raw.records().to_vec();
        let defaults = if self.target == FACTORY_AMP_BASELINE {
            FACTORY_AMP_MATCH_DEFAULTS
        } else {
            factory_amp_descriptor(self.target)
                .expect("pair checked")
                .defaults
        };
        overlay(&mut records[6].body, self.target, defaults);
        let descriptor = factory_amp_descriptor(self.profile.candidate()).expect("profile checked");
        let expected_cab = self
            .auto_cab
            .then_some(if self.target == FACTORY_AMP_BASELINE {
                FACTORY_AMP_BASELINE_CAB
            } else {
                descriptor.paired_cabinet
            });
        if self.paired_cabinet != expected_cab {
            return Err(unverified(
                "Factory AMP projection cabinet differs from its descriptor",
            ));
        }
        if let Some(cabinet) = self.paired_cabinet {
            overlay(&mut records[8].body, cabinet, FACTORY_AMP_CAB_DEFAULTS);
        }
        RawTransfer::from_records(before.raw.kind(), &records)
            .map_err(|error| ServiceError::invalid(error.to_string()))
    }
}
fn overlay(body: &mut [u8], code: ModelCode, defaults: &[(u8, u32)]) {
    body[4..8].copy_from_slice(&code.0.to_le_bytes());
    for &(id, bits) in defaults {
        let offset = 8 + usize::from(id) * 4;
        body[offset..offset + 4].copy_from_slice(&bits.to_le_bytes());
    }
}
pub const FACTORY_AMP_MATCH_DEFAULTS: &[(u8, u32)] = &[
    (0, 0x420c0000),
    (1, 0x42480000),
    (2, 0x42480000),
    (3, 0x42480000),
    (4, 0x42480000),
    (5, 0x42480000),
];
pub const FACTORY_AMP_CAB_DEFAULTS: &[(u8, u32)] =
    &[(1, 0x42480000), (5, 0x41980000), (6, 0x469c4200)];

/// Complete candidate readback comparison under the existing native preset exclusions.
pub fn factory_amp_readback_matches(expected: &RawTransfer, actual: &RawTransfer) -> bool {
    dependent_preset_shape(expected).is_ok()
        && dependent_preset_shape(actual).is_ok()
        && expected.kind() == actual.kind()
        && expected
            .records()
            .iter()
            .zip(actual.records())
            .all(|(old, new)| {
                old.command == new.command
                    && old.body.len() == new.body.len()
                    && old
                        .body
                        .iter()
                        .zip(&new.body)
                        .enumerate()
                        .all(|(index, (a, b))| {
                            a == b
                                || match old.command {
                                    2 | 8 => index == 15,
                                    0x0f => matches!(index, 2 | 3),
                                    _ => false,
                                }
                        })
            })
}
