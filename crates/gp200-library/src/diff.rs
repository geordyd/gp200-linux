use gp200_catalog::Catalog;
use gp200_domain::*;
use gp200_protocol::F32Bits;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParameterChange {
    pub module: ModuleId,
    pub model: ModelCode,
    pub parameter: ParamId,
    pub name: String,
    pub before: F32Bits,
    pub after: F32Bits,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KnownChange {
    Name {
        before: Option<String>,
        after: Option<String>,
    },
    Tempo {
        before: Option<u16>,
        after: Option<u16>,
    },
    Level {
        before: Option<u16>,
        after: Option<u16>,
    },
    Chain {
        before: Option<Vec<ModuleId>>,
        after: Option<Vec<ModuleId>>,
    },
    ModuleAdded(ModuleId),
    ModuleRemoved(ModuleId),
    Model {
        module: ModuleId,
        before: ModelCode,
        after: ModelCode,
        before_name: Option<String>,
        after_name: Option<String>,
    },
    State {
        module: ModuleId,
        before: u8,
        after: u8,
    },
    Parameter(ParameterChange),
    LocalMetadata,
    Expressions,
    Footswitches,
    QuickControls,
    FxLoopPositions,
    Dependencies,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawChange {
    /// Record position, preserving duplicate and unknown command IDs.
    pub record_index: usize,
    pub before_command: Option<u16>,
    pub after_command: Option<u16>,
    pub before: Option<Vec<u8>>,
    pub after: Option<Vec<u8>>,
    /// Every changed byte remains available, even when a typed projection exists.
    pub changed_offsets: Vec<usize>,
    pub classification: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetDiff {
    pub known: Vec<KnownChange>,
    pub raw: Vec<RawChange>,
    pub transfer_kind_changed: bool,
    pub asset_refs_changed: bool,
}

/// Diagnostic comparison only. It grants no hardware-write capability.
pub fn compare_presets(
    before: &PresetSnapshot,
    after: &PresetSnapshot,
    catalog: &Catalog,
) -> PresetDiff {
    let mut known = Vec::new();
    if before.view.name.value() != after.view.name.value() {
        known.push(KnownChange::Name {
            before: before.view.name.value().cloned(),
            after: after.view.name.value().cloned(),
        });
    }
    if before.view.tempo.value() != after.view.tempo.value() {
        known.push(KnownChange::Tempo {
            before: before.view.tempo.value().copied(),
            after: after.view.tempo.value().copied(),
        });
    }
    if before.view.level.value() != after.view.level.value() {
        known.push(KnownChange::Level {
            before: before.view.level.value().copied(),
            after: after.view.level.value().copied(),
        });
    }
    if before.view.chain.value() != after.view.chain.value() {
        known.push(KnownChange::Chain {
            before: before.view.chain.value().cloned(),
            after: after.view.chain.value().cloned(),
        });
    }
    for id in ModuleId::all() {
        match (before.view.module(id), after.view.module(id)) {
            (None, Some(_)) => known.push(KnownChange::ModuleAdded(id)),
            (Some(_), None) => known.push(KnownChange::ModuleRemoved(id)),
            (Some(a), Some(b)) => {
                if a.state_raw != b.state_raw {
                    known.push(KnownChange::State {
                        module: id,
                        before: a.state_raw,
                        after: b.state_raw,
                    });
                }
                if a.model != b.model {
                    known.push(KnownChange::Model {
                        module: id,
                        before: a.model,
                        after: b.model,
                        before_name: catalog
                            .model_by_id(id.get(), a.model.0)
                            .map(|model| model.name.clone()),
                        after_name: catalog
                            .model_by_id(id.get(), b.model.0)
                            .map(|model| model.name.clone()),
                    });
                } else if let Some(model) = catalog.model_by_id(id.get(), a.model.0) {
                    for definition in &model.parameters {
                        let Ok(parameter) = ParamId::new(definition.parameter_id) else {
                            continue;
                        };
                        if let (Some(av), Some(bv)) =
                            (a.parameter(parameter), b.parameter(parameter))
                            && av != bv
                        {
                            known.push(KnownChange::Parameter(ParameterChange {
                                module: id,
                                model: a.model,
                                parameter,
                                name: definition.name.clone(),
                                before: av,
                                after: bv,
                            }));
                        }
                    }
                }
            }
            (None, None) => {}
        }
    }
    if before.view.metadata != after.view.metadata {
        known.push(KnownChange::LocalMetadata);
    }
    if before.view.expressions != after.view.expressions
        || before.view.patch_assignments.expressions != after.view.patch_assignments.expressions
    {
        known.push(KnownChange::Expressions);
    }
    if before.view.footswitches != after.view.footswitches
        || before
            .view
            .patch_assignments
            .ctrls
            .iter()
            .map(|item| {
                (
                    &item.record.slot,
                    &item.record.state_raw,
                    &item.record.module_mask_raw,
                )
            })
            .ne(after.view.patch_assignments.ctrls.iter().map(|item| {
                (
                    &item.record.slot,
                    &item.record.state_raw,
                    &item.record.module_mask_raw,
                )
            }))
    {
        known.push(KnownChange::Footswitches);
    }
    if before.view.quick_controls != after.view.quick_controls
        || before.view.patch_assignments.quicks != after.view.patch_assignments.quicks
    {
        known.push(KnownChange::QuickControls);
    }
    if before
        .view
        .patch_assignments
        .routing
        .as_ref()
        .map(|item| &item.record)
        != after
            .view
            .patch_assignments
            .routing
            .as_ref()
            .map(|item| &item.record)
    {
        known.push(KnownChange::FxLoopPositions);
    }
    if before.asset_refs != after.asset_refs {
        known.push(KnownChange::Dependencies);
    }
    let a = before.raw.records();
    let b = after.raw.records();
    let raw = (0..a.len().max(b.len()))
        .filter_map(|record_index| {
            let av = a.get(record_index);
            let bv = b.get(record_index);
            if av == bv {
                return None;
            }
            let ab = av.map(|record| record.body.as_slice()).unwrap_or_default();
            let bb = bv.map(|record| record.body.as_slice()).unwrap_or_default();
            let changed_offsets = (0..ab.len().max(bb.len()))
                .filter(|&offset| ab.get(offset) != bb.get(offset))
                .collect();
            Some(RawChange {
                record_index,
                before_command: av.map(|record| record.command),
                after_command: bv.map(|record| record.command),
                before: av.map(|record| record.body.clone()),
                after: bv.map(|record| record.body.clone()),
                changed_offsets,
                classification: if av.is_some_and(|r| r.command == 0x14)
                    && bv.is_some_and(|r| r.command == 0x14)
                {
                    "Module record; undeclared slots and opaque bytes remain unclassified"
                } else {
                    "Raw record change; bytes without a verified projection are unclassified"
                }
                .into(),
            })
        })
        .collect();
    PresetDiff {
        known,
        raw,
        transfer_kind_changed: before.raw.kind() != after.raw.kind(),
        asset_refs_changed: before.asset_refs != after.asset_refs,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyReport {
    pub satisfied: Vec<AssetRef>,
    pub missing: Vec<AssetRef>,
}
/// Only a registered matching content hash proves an asset is present.
pub fn dependency_report(
    snapshot: &PresetSnapshot,
    available: &BTreeMap<AssetId, crate::StoredAsset>,
) -> DependencyReport {
    let mut satisfied = Vec::new();
    let mut missing = Vec::new();
    let mut seen = BTreeSet::new();
    for reference in &snapshot.asset_refs {
        let key = serde_json::to_string(reference).unwrap_or_default();
        if !seen.insert(key) {
            continue;
        }
        let present = match reference {
            AssetRef::Factory { .. } => true,
            AssetRef::Content { id, .. } => available.contains_key(id),
            AssetRef::UserSlot {
                kind,
                content: Some(id),
                ..
            } => available.get(id).is_some_and(|asset| asset.kind == *kind),
            AssetRef::UserSlot { content: None, .. } | AssetRef::Unresolved { .. } => false,
        };
        if present {
            satisfied.push(reference.clone());
        } else {
            missing.push(reference.clone());
        }
    }
    DependencyReport { satisfied, missing }
}
