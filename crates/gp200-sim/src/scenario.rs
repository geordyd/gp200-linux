use gp200_domain::{BackendMode, EvidenceLevel, ServiceError};
use gp200_protocol::{F32Bits, MAX_TRANSFER_LENGTH, RawTransfer};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Synthetic fault coverage, not a statement that the physical pedal behaves
/// this way. Timing is caller-provided and never uses a test wall clock.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyFault {
    pub silent: bool,
    pub disconnect: bool,
    pub drop_packet: Option<usize>,
    pub duplicate_packet: Option<usize>,
    pub reverse_packets: bool,
    pub corrupt_packet: Option<usize>,
    pub conflicting_duplicate: Option<usize>,
    pub wrong_context: Option<(u16, u16)>,
    pub unsolicited_after_packet: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyProfile {
    pub decoded_chunk_size: usize,
    pub callback_chunk_size: usize,
    pub latency_ms: u64,
    pub inter_packet_ms: u64,
    pub realtime_byte: Option<u8>,
}
impl Default for ReplyProfile {
    fn default() -> Self {
        Self {
            decoded_chunk_size: 185,
            callback_chunk_size: 0,
            latency_ms: 0,
            inter_packet_ms: 0,
            realtime_byte: None,
        }
    }
}
impl ReplyProfile {
    pub fn validate(&self) -> Result<(), ServiceError> {
        if !(1..=MAX_TRANSFER_LENGTH).contains(&self.decoded_chunk_size)
            || self.callback_chunk_size > gp200_protocol::MAX_PACKET_LENGTH
            || self.latency_ms > 60_000
            || self.inter_packet_ms > 60_000
            || self.realtime_byte.is_some_and(|byte| byte < 0xf8)
        {
            return Err(ServiceError::invalid(
                "Simulator framing/timing profile exceeds its bounds",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    Bytes { at: Duration, bytes: Vec<u8> },
    Disconnected { at: Duration },
}
impl Delivery {
    pub fn at(&self) -> Duration {
        match self {
            Self::Bytes { at, .. } | Self::Disconnected { at } => *at,
        }
    }
}

/// Optional runtime expression input is deliberately separate from stored
/// preset data. Actual select captures show physical EXP changes to WAH/VOL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpressionOverlay {
    pub selected_slot: Option<u16>,
    pub module: u8,
    pub parameter: u8,
    pub value: F32Bits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenarioLog {
    pub schema_version: u16,
    pub backend_mode: BackendMode,
    pub profile: String,
    pub evidence_level: EvidenceLevel,
    pub timing: String,
    pub exchanges: Vec<LoggedExchange>,
}
impl Default for ScenarioLog {
    fn default() -> Self {
        Self {
            schema_version: 1,
            backend_mode: BackendMode::Simulator,
            profile: "gp200-real-codec-simulator-v1".into(),
            evidence_level: EvidenceLevel::Proposed,
            timing: "Synthetic caller-supplied monotonic timestamps".into(),
            exchanges: Vec::new(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoggedExchange {
    pub at_ms: u64,
    pub request: RawTransfer,
    pub reply: Option<RawTransfer>,
    pub fault: Option<ReplyFault>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SimulationScenario {
    pub schema_version: u16,
    pub profile: ReplyProfile,
    pub steps: Vec<SimulationStep>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SimulationStep {
    pub at_ms: u64,
    pub request: RawTransfer,
    pub fault: Option<ReplyFault>,
}
pub fn run_scenario(
    scenario: SimulationScenario,
) -> Result<(ScenarioLog, Vec<Delivery>), ServiceError> {
    if scenario.schema_version != 1
        || scenario.steps.len() > 1024
        || scenario
            .steps
            .windows(2)
            .any(|pair| pair[0].at_ms > pair[1].at_ms)
        || scenario
            .steps
            .last()
            .is_some_and(|step| step.at_ms > 3_600_000)
    {
        return Err(ServiceError::invalid(
            "Simulation scenario schema/time/step count is unsupported",
        ));
    }
    scenario.profile.validate()?;
    let mut pedal = crate::SimulatedPedal::captured();
    pedal.profile = scenario.profile;
    let mut deliveries = Vec::new();
    for step in scenario.steps {
        if let Some(fault) = step.fault {
            pedal.queue_fault(fault)?;
        }
        for packet in gp200_protocol::encode_transfer(&step.request)
            .map_err(|error| ServiceError::invalid(error.to_string()))?
        {
            let reply = pedal.receive(&packet, Duration::from_millis(step.at_ms))?;
            if deliveries.len() + reply.len() > 100_000 {
                return Err(ServiceError::invalid(
                    "Scenario delivery count exceeds its bound",
                ));
            }
            deliveries.extend(reply);
        }
    }
    Ok((pedal.log().clone(), deliveries))
}
