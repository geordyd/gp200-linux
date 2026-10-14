//! Pure bank permutations recovered from the original editor's insertion behavior.
//!
//! A plan describes original sound identities, not live device authority. It emits
//! no protocol records, selects no preset, and creates no restore journal.

use gp200_domain::{DeviceModel, SlotId};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PresetMovementError {
    #[error("Unsupported preset bank variant: {0:?}")]
    UnsupportedModel(DeviceModel),
    #[error("Slot {slot} is outside {model:?}'s bank (last slot {maximum})")]
    SlotOutOfRange {
        model: DeviceModel,
        slot: SlotId,
        maximum: u16,
    },
    #[error("Insertion boundary {boundary} exceeds {model:?}'s bank end {maximum}")]
    BoundaryOutOfRange {
        model: DeviceModel,
        boundary: u16,
        maximum: u16,
    },
    #[error("Select at least one preset to move")]
    EmptySelection,
    #[error("Selection has {selected} entries but the bank contains {maximum} slots")]
    SelectionTooLarge { selected: usize, maximum: u16 },
    #[error("Selected preset slot {0} occurs more than once")]
    DuplicateSelection(SlotId),
}

/// A gap before a bank slot, including the gap after the final slot.
///
/// This is distinct from [`SlotId`]: GP-200/LT boundary 256 is valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PresetInsertionBoundary(u16);

impl PresetInsertionBoundary {
    pub fn new(model: DeviceModel, value: u16) -> Result<Self, PresetMovementError> {
        let maximum = bank_size(model)?;
        if value > maximum {
            return Err(PresetMovementError::BoundaryOutOfRange {
                model,
                boundary: value,
                maximum,
            });
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetMoveAction {
    Top,
    Bottom,
    Previous,
    Next,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetMovementOperation {
    Insertion,
    Selection(PresetMoveAction),
}

/// One recovered insertion step. Sources are positions at that step, not an
/// invitation to send the original native move command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresetInsertionStep {
    pub source: SlotId,
    pub boundary: PresetInsertionBoundary,
    pub destination: SlotId,
}

/// An original sound identity and its final position in the planned bank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresetSlotMapping {
    pub source: SlotId,
    pub destination: SlotId,
}

impl PresetSlotMapping {
    pub fn shift(self) -> i16 {
        self.destination.get() as i16 - self.source.get() as i16
    }
}

/// Inclusive interval containing every changed slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresetSlotInterval {
    first: SlotId,
    last: SlotId,
}

impl PresetSlotInterval {
    pub const fn first(self) -> SlotId {
        self.first
    }

    pub const fn last(self) -> SlotId {
        self.last
    }

    pub fn contains(self, slot: SlotId) -> bool {
        (self.first.get()..=self.last.get()).contains(&slot.get())
    }

    pub fn slots(self) -> impl Iterator<Item = SlotId> {
        (self.first.get()..=self.last.get()).map(known_slot)
    }
}

/// Immutable arithmetic plan. This type deliberately cannot deserialize a
/// serialized claim into a validated live-device operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetMovementPlan {
    model: DeviceModel,
    operation: PresetMovementOperation,
    original_selection: Vec<SlotId>,
    final_selection: Vec<SlotId>,
    insertion_steps: Vec<PresetInsertionStep>,
    destination_to_source: Vec<SlotId>,
    source_to_destination: Vec<SlotId>,
    mappings: Vec<PresetSlotMapping>,
    affected_interval: Option<PresetSlotInterval>,
    sentinels: Vec<SlotId>,
    current_before: Option<SlotId>,
    current_after_original_reselection: Option<SlotId>,
}

impl PresetMovementPlan {
    pub fn insert(
        model: DeviceModel,
        source: SlotId,
        boundary: PresetInsertionBoundary,
        current: Option<SlotId>,
    ) -> Result<Self, PresetMovementError> {
        let count = bank_size(model)?;
        validate_slot(model, source, count)?;
        PresetInsertionBoundary::new(model, boundary.get())?;
        validate_current(model, current, count)?;
        Ok(Self::compose(
            model,
            PresetMovementOperation::Insertion,
            vec![source],
            vec![(source, boundary)],
            current,
            count,
        ))
    }

    pub fn to_destination(
        model: DeviceModel,
        source: SlotId,
        destination: SlotId,
        current: Option<SlotId>,
    ) -> Result<Self, PresetMovementError> {
        let count = bank_size(model)?;
        validate_slot(model, source, count)?;
        validate_slot(model, destination, count)?;
        let value = destination.get() + u16::from(destination > source);
        Self::insert(
            model,
            source,
            PresetInsertionBoundary::new(model, value)?,
            current,
        )
    }

    /// Canonicalizes display-order selections into ascending physical slots.
    /// Noncontiguous selections become a stable block under the original actions.
    pub fn selected(
        model: DeviceModel,
        selection: &[SlotId],
        action: PresetMoveAction,
        current: Option<SlotId>,
    ) -> Result<Self, PresetMovementError> {
        let count = bank_size(model)?;
        if selection.is_empty() {
            return Err(PresetMovementError::EmptySelection);
        }
        if selection.len() > usize::from(count) {
            return Err(PresetMovementError::SelectionTooLarge {
                selected: selection.len(),
                maximum: count,
            });
        }
        let mut selected = selection.to_vec();
        selected.sort_unstable();
        for &slot in &selected {
            validate_slot(model, slot, count)?;
        }
        if let Some(pair) = selected.windows(2).find(|pair| pair[0] == pair[1]) {
            return Err(PresetMovementError::DuplicateSelection(pair[0]));
        }
        validate_current(model, current, count)?;
        let first = selected[0].get();
        let last = selected[selected.len() - 1].get();
        let mut steps = Vec::with_capacity(selected.len());
        match action {
            PresetMoveAction::Top | PresetMoveAction::Previous => {
                let start = if action == PresetMoveAction::Top {
                    0
                } else {
                    first.saturating_sub(1)
                };
                for (index, &source) in selected.iter().enumerate() {
                    steps.push((source, PresetInsertionBoundary(start + index as u16)));
                }
            }
            PresetMoveAction::Bottom | PresetMoveAction::Next => {
                let start = if action == PresetMoveAction::Bottom {
                    count
                } else {
                    (last + 2).min(count)
                };
                for (index, &source) in selected.iter().rev().enumerate() {
                    steps.push((source, PresetInsertionBoundary(start - index as u16)));
                }
            }
        }
        Ok(Self::compose(
            model,
            PresetMovementOperation::Selection(action),
            selected,
            steps,
            current,
            count,
        ))
    }

    fn compose(
        model: DeviceModel,
        operation: PresetMovementOperation,
        original_selection: Vec<SlotId>,
        steps: Vec<(SlotId, PresetInsertionBoundary)>,
        current_before: Option<SlotId>,
        count: u16,
    ) -> Self {
        let mut destination_to_source: Vec<_> = (0..count).map(known_slot).collect();
        let mut insertion_steps = Vec::with_capacity(steps.len());
        for (source, boundary) in steps {
            let destination = boundary.get() - u16::from(source.get() < boundary.get());
            let destination = known_slot(destination);
            if destination != source {
                let sound = destination_to_source.remove(usize::from(source.get()));
                destination_to_source.insert(usize::from(destination.get()), sound);
            }
            insertion_steps.push(PresetInsertionStep {
                source,
                boundary,
                destination,
            });
        }
        let mut source_to_destination = vec![known_slot(0); usize::from(count)];
        let mappings: Vec<_> = destination_to_source
            .iter()
            .enumerate()
            .map(|(index, &source)| {
                let destination = known_slot(index as u16);
                source_to_destination[usize::from(source.get())] = destination;
                PresetSlotMapping {
                    source,
                    destination,
                }
            })
            .collect();
        let first = mappings.iter().find(|row| row.source != row.destination);
        let last = mappings.iter().rfind(|row| row.source != row.destination);
        let affected_interval = first.zip(last).map(|(first, last)| PresetSlotInterval {
            first: first.destination,
            last: last.destination,
        });
        let mut sentinels = Vec::with_capacity(2);
        if let Some(interval) = affected_interval {
            if interval.first.get() > 0 {
                sentinels.push(known_slot(interval.first.get() - 1));
            }
            if interval.last.get() + 1 < count {
                sentinels.push(known_slot(interval.last.get() + 1));
            }
        }
        let final_selection = original_selection
            .iter()
            .map(|slot| source_to_destination[usize::from(slot.get())])
            .collect();
        let current_after_original_reselection =
            current_before.map(|slot| source_to_destination[usize::from(slot.get())]);
        Self {
            model,
            operation,
            original_selection,
            final_selection,
            insertion_steps,
            destination_to_source,
            source_to_destination,
            mappings,
            affected_interval,
            sentinels,
            current_before,
            current_after_original_reselection,
        }
    }

    pub const fn model(&self) -> DeviceModel {
        self.model
    }

    pub fn bank_size(&self) -> u16 {
        self.destination_to_source.len() as u16
    }

    pub const fn operation(&self) -> PresetMovementOperation {
        self.operation
    }

    pub fn original_selection(&self) -> &[SlotId] {
        &self.original_selection
    }

    pub fn final_selection(&self) -> &[SlotId] {
        &self.final_selection
    }

    pub fn insertion_steps(&self) -> &[PresetInsertionStep] {
        &self.insertion_steps
    }

    /// Index is the final destination, value is its original source identity.
    pub fn destination_to_source(&self) -> &[SlotId] {
        &self.destination_to_source
    }

    /// Index is the original source identity, value is its final position.
    pub fn source_to_destination(&self) -> &[SlotId] {
        &self.source_to_destination
    }

    /// Every mapping, including unchanged entries, ordered by destination.
    pub fn mappings(&self) -> &[PresetSlotMapping] {
        &self.mappings
    }

    pub fn changed_mappings(&self) -> impl Iterator<Item = &PresetSlotMapping> {
        self.mappings
            .iter()
            .filter(|row| row.source != row.destination)
    }

    /// Complete interval mapping for a future all-or-none reviewed transaction.
    pub fn affected_mappings(&self) -> impl Iterator<Item = &PresetSlotMapping> {
        self.mappings.iter().filter(|row| {
            self.affected_interval
                .is_some_and(|interval| interval.contains(row.destination))
        })
    }

    pub fn destination_for(&self, original_source: SlotId) -> Option<SlotId> {
        self.source_to_destination
            .get(usize::from(original_source.get()))
            .copied()
    }

    pub fn original_source_for(&self, destination: SlotId) -> Option<SlotId> {
        self.destination_to_source
            .get(usize::from(destination.get()))
            .copied()
    }

    /// Mathematical inverse ordered by original slot. Sources refer to the
    /// planned final bank; live rollback still requires immutable before-images.
    pub fn inverse_mappings(&self) -> impl Iterator<Item = PresetSlotMapping> + '_ {
        self.source_to_destination
            .iter()
            .enumerate()
            .map(|(index, &source)| PresetSlotMapping {
                source,
                destination: known_slot(index as u16),
            })
    }

    pub const fn affected_interval(&self) -> Option<PresetSlotInterval> {
        self.affected_interval
    }

    pub fn sentinels(&self) -> &[SlotId] {
        &self.sentinels
    }

    pub const fn is_noop(&self) -> bool {
        self.affected_interval.is_none()
    }

    pub const fn current_before(&self) -> Option<SlotId> {
        self.current_before
    }

    /// Position of the original current sound under the original editor's
    /// reselection policy. This getter does not select or alter the pedal.
    pub const fn current_after_original_reselection(&self) -> Option<SlotId> {
        self.current_after_original_reselection
    }
}

fn bank_size(model: DeviceModel) -> Result<u16, PresetMovementError> {
    match model {
        DeviceModel::Gp200 | DeviceModel::Gp200Lt => Ok(256),
        DeviceModel::Gp200Jr => Ok(255),
        DeviceModel::Unknown(_) => Err(PresetMovementError::UnsupportedModel(model)),
    }
}

fn validate_slot(model: DeviceModel, slot: SlotId, count: u16) -> Result<(), PresetMovementError> {
    if slot.get() >= count {
        Err(PresetMovementError::SlotOutOfRange {
            model,
            slot,
            maximum: count - 1,
        })
    } else {
        Ok(())
    }
}

fn validate_current(
    model: DeviceModel,
    current: Option<SlotId>,
    count: u16,
) -> Result<(), PresetMovementError> {
    if let Some(slot) = current {
        validate_slot(model, slot, count)?;
    }
    Ok(())
}

fn known_slot(value: u16) -> SlotId {
    SlotId::new(value).expect("planner positions are inside a checked 255/256-slot bank")
}
