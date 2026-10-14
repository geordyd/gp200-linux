use crate::*;
use gp200_protocol::{F32Bits, RawTransfer};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PresetTarget {
    EditBuffer,
    Stored(SlotId),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditTarget {
    DeviceBuffer,
    Offline(DocumentId),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireContext {
    pub group: u16,
    pub selector: u16,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestStamp {
    pub epoch: SessionEpoch,
    pub intent: IntentId,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotSource {
    Hardware { epoch: SessionEpoch },
    Library { version: LibraryVersionId },
    Capture { capture_id: String },
    Simulator,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssetRef {
    Factory {
        module: ModuleId,
        model: ModelCode,
    },
    UserSlot {
        kind: AssetKind,
        slot: u8,
        content: Option<AssetId>,
    },
    Content {
        id: AssetId,
        name: String,
    },
    Unresolved {
        description: String,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssetKind {
    ImpulseResponse,
    SnapTone,
    Nam,
    Firmware,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleSnapshot {
    pub module: ModuleId,
    pub state_raw: u8,
    pub slot_count_raw: u16,
    pub model: ModelCode,
    pub parameters: Vec<F32Bits>,
    pub raw_record_index: usize,
}
impl ModuleSnapshot {
    pub fn enabled(&self) -> bool {
        self.state_raw != 0
    }
    pub fn parameter(&self, id: ParamId) -> Option<F32Bits> {
        self.parameters.get(id.get() as usize).copied()
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetView {
    pub name: Recovered<String>,
    pub tempo: Recovered<u16>,
    pub level: Recovered<u16>,
    pub chain: Recovered<Vec<ModuleId>>,
    pub modules: Vec<ModuleSnapshot>,
    #[serde(default)]
    pub metadata: MetadataPatch,
    #[serde(default)]
    pub expressions: Vec<ExpressionPatch>,
    #[serde(default)]
    pub footswitches: Vec<FootswitchPatch>,
    #[serde(default)]
    pub quick_controls: Vec<QuickControlPatch>,
    #[serde(default)]
    pub patch_assignments: PatchAssignments,
}
impl PresetView {
    pub fn display_name(&self) -> &str {
        self.name.value().map_or("Unnamed preset", String::as_str)
    }
    pub fn module(&self, module: ModuleId) -> Option<&ModuleSnapshot> {
        self.modules.iter().find(|m| m.module == module)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetSnapshot {
    pub source: SnapshotSource,
    pub device_model: DeviceModel,
    pub firmware: Option<FirmwareIdentity>,
    pub target: Option<PresetTarget>,
    pub context: Option<WireContext>,
    pub revision: Revision,
    pub raw: Arc<RawTransfer>,
    pub view: PresetView,
    pub asset_refs: Vec<AssetRef>,
    pub evidence: Vec<EvidenceRef>,
}
impl PresetSnapshot {
    /// Preserve all raw records, projecting only structurally established fields.
    pub fn from_raw(
        raw: Arc<RawTransfer>,
        source: SnapshotSource,
        target: Option<PresetTarget>,
        revision: Revision,
    ) -> Self {
        let evidence = vec![EvidenceRef::new(
            "analysis/PROTOCOL.txt",
            EvidenceLevel::StaticRecovered,
        )];
        let mut view = PresetView {
            name: Recovered::Unknown,
            tempo: Recovered::Unknown,
            level: Recovered::Unknown,
            chain: Recovered::Unknown,
            modules: Vec::new(),
            metadata: MetadataPatch::default(),
            expressions: Vec::new(),
            footswitches: Vec::new(),
            quick_controls: Vec::new(),
            patch_assignments: PatchAssignments::from_raw(&raw),
        };
        let mut context = None;
        for (index, record) in raw.records().iter().enumerate() {
            let body = &record.body;
            match record.command {
                0 if body.len() == 4 => {
                    context = Some(WireContext {
                        group: u16::from_le_bytes([body[0], body[1]]),
                        selector: u16::from_le_bytes([body[2], body[3]]),
                    })
                }
                2 if body.len() >= 32 => {
                    // The adjacent bytes 32..48 are a separate vendor field.
                    // A full-width name must not consume that opaque field.
                    let name_bytes = &body[16..32];
                    let end = name_bytes
                        .iter()
                        .position(|&b| b == 0)
                        .unwrap_or(name_bytes.len());
                    view.name = Recovered::Candidate {
                        value: String::from_utf8_lossy(&name_bytes[..end]).into_owned(),
                        evidence: evidence.clone(),
                    };
                    view.tempo = Recovered::Candidate {
                        value: u16::from_le_bytes([body[2], body[3]]),
                        evidence: evidence.clone(),
                    };
                    view.level = Recovered::Candidate {
                        value: u16::from_le_bytes([body[4], body[5]]),
                        evidence: evidence.clone(),
                    };
                    view.metadata.native_pan = Some(PatchPan::from_raw(body[7] as i8));
                    view.metadata.native_style =
                        Some(NativeStyleId::from_raw(u16::from_le_bytes([
                            body[8], body[9],
                        ])));
                    view.metadata.fx_send_level = Some(u16::from_le_bytes([body[10], body[11]]));
                    view.metadata.fx_return_level = Some(u16::from_le_bytes([body[12], body[13]]));
                    view.metadata.fx_loop_mode = Some(PatchFxLoopMode::from_raw(body[14]));
                }
                8 if body.len() >= 15 => {
                    let chain = body[4..15]
                        .iter()
                        .map(|&id| ModuleId::new(id))
                        .collect::<Result<Vec<_>, _>>();
                    if let Ok(chain) = chain
                        && chain
                            .iter()
                            .copied()
                            .collect::<std::collections::BTreeSet<_>>()
                            .len()
                            == 11
                    {
                        view.chain = Recovered::Candidate {
                            value: chain,
                            evidence: evidence.clone(),
                        };
                    }
                }
                0x14 if body.len() >= 8 => {
                    if let Ok(module) = ModuleId::new(body[0]) {
                        let slot_count_raw = u16::from_le_bytes([body[2], body[3]]);
                        let parameters = body[8..]
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|bytes| F32Bits::from_le_bytes(*bytes))
                            .collect();
                        view.modules.push(ModuleSnapshot {
                            module,
                            state_raw: body[1],
                            slot_count_raw,
                            model: ModelCode(u32::from_le_bytes(
                                body[4..8].try_into().expect("four-byte code"),
                            )),
                            parameters,
                            raw_record_index: index,
                        });
                    }
                }
                _ => {}
            }
        }
        view.metadata.name = view.name.value().cloned();
        view.metadata.tempo = view.tempo.value().copied();
        view.metadata.level = view.level.value().copied();
        let asset_refs = view
            .modules
            .iter()
            .map(|module| {
                if module.module.get() == 5 && (0x0a10_0000..=0x0a10_0013).contains(&module.model.0)
                {
                    AssetRef::UserSlot {
                        kind: AssetKind::ImpulseResponse,
                        slot: (module.model.0 - 0x0a10_0000) as u8,
                        content: None,
                    }
                } else if (0x0f00_0000..=0x0f00_0009).contains(&module.model.0) {
                    AssetRef::UserSlot {
                        kind: AssetKind::SnapTone,
                        slot: (module.model.0 - 0x0f00_0000) as u8,
                        content: None,
                    }
                } else {
                    AssetRef::Factory {
                        module: module.module,
                        model: module.model,
                    }
                }
            })
            .collect();
        Self {
            source,
            device_model: DeviceModel::Gp200,
            firmware: None,
            target,
            context,
            revision,
            raw,
            view,
            asset_refs,
            evidence,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedSlot {
    pub snapshot: Arc<PresetSnapshot>,
    pub fresh: bool,
}
#[derive(Debug, Clone)]
pub struct HardwareBank {
    pub epoch: SessionEpoch,
    pub slots: BTreeMap<SlotId, CachedSlot>,
}
impl HardwareBank {
    pub fn new(epoch: SessionEpoch) -> Self {
        Self {
            epoch,
            slots: BTreeMap::new(),
        }
    }
}
