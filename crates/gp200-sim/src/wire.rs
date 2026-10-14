use crate::{Delivery, ExpressionOverlay, LoggedExchange, ReplyFault, ReplyProfile, ScenarioLog};
use gp200_catalog::Catalog;
use gp200_domain::{
    AssetBank, AssetSlot, DeviceGlobalState, ErrorKind, GlobalControlEdit, Revision, ServiceError,
    SessionEpoch,
};
use gp200_protocol::{
    DrumSetting, EqParameter, F32Bits, GeneralSetting, GlobalFootswitchEvent,
    GlobalFootswitchFunction, GlobalFootswitchTableId, RawRecord, RawTransfer, Reassembler,
    StreamDecoder, StreamEvent, WRITE_KIND, WireContext, encode_packets_with_chunk_size,
    full_stored_write_request, identify_request, read_current_request, read_globals_request,
    read_stored_request,
};
use std::collections::VecDeque;
use std::time::Duration;

const BANK: &[u8] = include_bytes!("../../../tests/fixtures/sim/gp200-bank.bin");
const CURRENT: &[u8] = include_bytes!("../../../assets/demo/read-current-payload.bin");
const GLOBALS: &[u8] = include_bytes!("../../../tests/fixtures/globals/baseline.payload.bin");
const IR_METADATA: &[u8] =
    include_bytes!("../../../tests/fixtures/device-assets/connected-ir-metadata.payload.bin");

fn protocol_error(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::new(ErrorKind::Protocol, error.to_string())
}
fn word(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}
fn context_reply(raw: &RawTransfer, group: u16, slot: u16) -> Result<RawTransfer, ServiceError> {
    let mut records: Vec<_> = raw
        .records()
        .iter()
        .filter(|record| record.command != 0)
        .cloned()
        .collect();
    records.insert(
        0,
        WireContext {
            group,
            selector: slot,
        }
        .to_record(),
    );
    RawTransfer::from_records(WRITE_KIND, &records).map_err(protocol_error)
}
fn set_slot(raw: &RawTransfer, group: u16, slot: u16) -> Result<RawTransfer, ServiceError> {
    let mut raw = context_reply(raw, group, slot)?;
    let mut records = raw.records().to_vec();
    for record in &mut records {
        if matches!(record.command, 2 | 8) && record.body.len() >= 2 {
            record.body[..2].copy_from_slice(&slot.to_le_bytes());
        }
    }
    raw = RawTransfer::from_records(WRITE_KIND, &records).map_err(protocol_error)?;
    Ok(raw)
}
fn unique_record(
    records: &mut [RawRecord],
    command: u16,
    length: usize,
) -> Result<&mut RawRecord, ServiceError> {
    let mut matching = records
        .iter_mut()
        .filter(|record| record.command == command);
    let record = matching
        .next()
        .ok_or_else(|| protocol_error("Simulated retained record missing"))?;
    if matching.next().is_some() || record.body.len() != length {
        return Err(protocol_error(
            "Simulated retained record is ambiguous or malformed",
        ));
    }
    Ok(record)
}
fn module_record(records: &mut [RawRecord], module: u8) -> Result<&mut RawRecord, ServiceError> {
    records
        .iter_mut()
        .find(|record| {
            record.command == 0x14 && record.body.len() == 68 && record.body[0] == module
        })
        .ok_or_else(|| protocol_error("Simulated module is absent"))
}

/// Normal-mode GP-200 reconstruction. Both requests and responses traverse the
/// production stream, packet and TLV codecs. Stored bytes originate from all
/// 256 captured slots; globals use a separate lossless captured settings bundle.
/// The simulator does not reconstruct effects/DSP or user asset contents.
pub struct SimulatedPedal {
    decoder: StreamDecoder,
    assembler: Reassembler,
    catalog: Catalog,
    current: RawTransfer,
    stored: Vec<RawTransfer>,
    globals: RawTransfer,
    asset_metadata: Vec<RawRecord>,
    selected: u16,
    pub profile: ReplyProfile,
    faults: VecDeque<ReplyFault>,
    overlays: Vec<ExpressionOverlay>,
    log: ScenarioLog,
    now: Duration,
}
impl Default for SimulatedPedal {
    fn default() -> Self {
        Self::captured()
    }
}
impl SimulatedPedal {
    /// Friendly presentation for `--demo`; capture-oriented test constructors
    /// continue to expose the original fixture bytes.
    pub fn public_demo() -> Self {
        let mut pedal = Self::captured();
        pedal.current = crate::public_demo_snapshot().raw.as_ref().clone();
        pedal
    }

    pub fn captured() -> Self {
        let stored = BANK
            .as_chunks::<1176>()
            .0
            .iter()
            .map(|payload| {
                RawTransfer::from_payload(WRITE_KIND, payload.to_vec())
                    .expect("owned, hashed bank fixture")
            })
            .collect::<Vec<_>>();
        assert_eq!(stored.len(), 256);
        let current = RawTransfer::from_payload(WRITE_KIND, CURRENT.to_vec())
            .expect("owned, hashed current fixture");
        let selected = WireContext::from_transfer(&current).map_or(23, |context| context.selector);
        Self {
            decoder: StreamDecoder::new(),
            assembler: Reassembler::default(),
            catalog: Catalog::embedded().expect("validated catalog"),
            current,
            stored,
            globals: RawTransfer::from_payload(WRITE_KIND, GLOBALS.to_vec())
                .expect("hashed globals fixture"),
            asset_metadata: {
                let mut records = RawTransfer::from_payload(WRITE_KIND, IR_METADATA.to_vec())
                    .expect("captured IR metadata")
                    .records()
                    .to_vec();
                // Explicit synthetic SnapTone metadata, never hardware evidence
                // or a claim that the connected firmware answers this query.
                for index in 0..AssetBank::SnapTone.slots() {
                    let mut body = vec![0_u8; 24];
                    body[..2].copy_from_slice(&index.to_le_bytes());
                    let name = format!("Demo Snap {}", index + 1);
                    body[8..8 + name.len()].copy_from_slice(name.as_bytes());
                    records.push(RawRecord {
                        command: AssetBank::SnapTone.metadata_command(),
                        body,
                    });
                }
                records
            },
            selected,
            profile: ReplyProfile::default(),
            faults: VecDeque::new(),
            overlays: Vec::new(),
            log: ScenarioLog::default(),
            now: Duration::ZERO,
        }
    }
    pub fn current(&self) -> &RawTransfer {
        &self.current
    }
    pub fn stored(&self, slot: u16) -> Option<&RawTransfer> {
        self.stored.get(usize::from(slot))
    }
    pub fn globals(&self) -> &RawTransfer {
        &self.globals
    }
    pub fn selected_slot(&self) -> u16 {
        self.selected
    }
    pub fn log(&self) -> &ScenarioLog {
        &self.log
    }
    pub fn queue_fault(&mut self, fault: ReplyFault) -> Result<(), ServiceError> {
        if self.faults.len() >= 256 {
            return Err(ServiceError::new(
                ErrorKind::Busy,
                "Simulator fault queue is full",
            ));
        }
        self.faults.push_back(fault);
        Ok(())
    }
    pub fn set_expression_overlays(
        &mut self,
        overlays: Vec<ExpressionOverlay>,
    ) -> Result<(), ServiceError> {
        if overlays.len() > 32
            || overlays.iter().any(|overlay| {
                overlay.module > 10
                    || overlay.parameter > 14
                    || overlay.selected_slot.is_some_and(|slot| slot > 255)
            })
        {
            return Err(ServiceError::invalid(
                "Expression overlay target exceeds simulator bounds",
            ));
        }
        self.overlays = overlays;
        Ok(())
    }
    /// Reset an incomplete request on cancellation/disconnect. Pedal memory
    /// survives reconnect, but old partial frames do not become new requests.
    pub fn reset_stream(&mut self) {
        self.decoder.reset();
        self.assembler.reset();
    }
    pub fn expire_request(&mut self, now: Duration) -> bool {
        self.assembler.expire(now).is_some()
    }
    pub fn receive(&mut self, bytes: &[u8], now: Duration) -> Result<Vec<Delivery>, ServiceError> {
        self.profile.validate()?;
        if now < self.now {
            return Err(protocol_error("Simulator timestamp moved backwards"));
        }
        self.now = now;
        let mut deliveries = Vec::new();
        for event in self.decoder.feed(bytes) {
            match event {
                StreamEvent::Packet(packet) => {
                    if let Some(request) =
                        self.assembler.add(&packet, now).map_err(protocol_error)?
                    {
                        let reply = self.process(&request)?;
                        let fault = reply
                            .as_ref()
                            .map(|_| self.faults.pop_front().unwrap_or_default());
                        self.log.exchanges.push(LoggedExchange {
                            at_ms: now.as_millis().min(u128::from(u64::MAX)) as u64,
                            request,
                            reply: reply.clone(),
                            fault: fault.clone(),
                        });
                        // Keep the diagnostic log bounded independently of bytes.
                        if self.log.exchanges.len() > 1024 {
                            self.log.exchanges.remove(0);
                        }
                        if let Some(reply) = reply {
                            deliveries.extend(self.reply_delivery(
                                reply,
                                fault.unwrap_or_default(),
                                now,
                            )?);
                        }
                    }
                }
                StreamEvent::Error(error) => return Err(protocol_error(error)),
                StreamEvent::UnrecognizedSysex(_) => {
                    return Err(protocol_error("Simulator accepts normal GP-200 SysEx only"));
                }
                StreamEvent::Realtime(_) | StreamEvent::OtherMidi(_) => {}
            }
        }
        Ok(deliveries)
    }
    fn process(&mut self, request: &RawTransfer) -> Result<Option<RawTransfer>, ServiceError> {
        if request.kind() == gp200_protocol::READ_KIND && request.records().len() == 1 {
            let record = &request.records()[0];
            let bank = match record.command {
                0x1009 => Some(AssetBank::Ir),
                0x1012 => Some(AssetBank::SnapTone),
                _ => None,
            };
            if let Some(bank) = bank {
                if record.body.len() != 24 {
                    return Err(protocol_error("Malformed asset metadata request"));
                }
                let slot = AssetSlot::new(bank, word(&record.body, 0)).map_err(protocol_error)?;
                if request != &gp200_protocol::asset_metadata_read_candidate(slot) {
                    return Err(protocol_error("Unrecognized asset metadata request fields"));
                }
                let metadata = self
                    .asset_metadata
                    .iter()
                    .find(|metadata| {
                        metadata.command == bank.metadata_command()
                            && word(&metadata.body, 0) == slot.index()
                    })
                    .ok_or_else(|| protocol_error("Missing simulated metadata"))?;
                return Ok(Some(
                    RawTransfer::from_records(WRITE_KIND, std::slice::from_ref(metadata))
                        .map_err(protocol_error)?,
                ));
            }
        }
        if request == &read_globals_request().map_err(protocol_error)? {
            return Ok(Some(self.globals.clone()));
        }
        if request == &identify_request() {
            return Ok(Some(
                RawTransfer::from_records(
                    WRITE_KIND,
                    &[RawRecord {
                        command: 0x2001,
                        body: vec![1, 0, 2, 0],
                    }],
                )
                .map_err(protocol_error)?,
            ));
        }
        if request == &read_current_request() {
            return Ok(Some(self.current.clone()));
        }
        if let Some(context) = WireContext::from_transfer(request)
            && context.group == 1
            && context.selector < 256
            && request == &read_stored_request(context.selector).map_err(protocol_error)?
        {
            return Ok(Some(self.stored[usize::from(context.selector)].clone()));
        }
        if request.kind() != WRITE_KIND {
            return Err(ServiceError::unsupported("Unknown simulated read query"));
        }
        if request.records().len() == 1 {
            let record = &request.records()[0];
            match record.command {
                0x1008 if record.body.len() == 4 && record.body[..2] == [0, 0] => {
                    let slot = word(&record.body, 2);
                    let stored = self
                        .stored
                        .get(usize::from(slot))
                        .ok_or_else(|| protocol_error("Simulated selection outside bank"))?;
                    let mut current = set_slot(stored, 0, slot)?;
                    let mut records = current.records().to_vec();
                    for overlay in &self.overlays {
                        if overlay.selected_slot.is_none_or(|target| target == slot) {
                            let module = module_record(&mut records, overlay.module)?;
                            let offset = 8 + usize::from(overlay.parameter) * 4;
                            module.body[offset..offset + 4]
                                .copy_from_slice(&overlay.value.to_le_bytes());
                        }
                    }
                    current =
                        RawTransfer::from_records(WRITE_KIND, &records).map_err(protocol_error)?;
                    self.current = current;
                    self.selected = slot;
                    self.synchronize_global_selected_slot(slot)?;
                    return Ok(None);
                }
                0x2003 if record.body.len() == 20 => {
                    let slot = word(&record.body, 0);
                    if slot > 255 {
                        return Err(protocol_error("Simulated save outside bank"));
                    }
                    let mut records = set_slot(&self.current, 1, slot)?.records().to_vec();
                    unique_record(&mut records, 2, 88)?.body[16..32]
                        .copy_from_slice(&record.body[4..20]);
                    self.stored[usize::from(slot)] =
                        RawTransfer::from_records(WRITE_KIND, &records).map_err(protocol_error)?;
                    // Live Save As capture also changes the active buffer's
                    // context to the explicit saved target.
                    self.current = set_slot(&self.stored[usize::from(slot)], 0, slot)?;
                    self.selected = slot;
                    self.synchronize_global_selected_slot(slot)?;
                    return Ok(None);
                }
                0x1008 if record.body.len() == 4 => {
                    let field = GeneralSetting::ALL
                        .into_iter()
                        .find(|field| field.selector() == word(&record.body, 0))
                        .ok_or_else(|| protocol_error("Unrecovered global selector"))?;
                    self.apply_global(GlobalControlEdit::General {
                        field,
                        value: i16::from_le_bytes(record.body[2..4].try_into().unwrap()),
                    })?;
                    return Ok(None);
                }
                0x1005 if record.body.len() == 8 => {
                    let parameter =
                        EqParameter::from_index(word(&record.body, 0)).map_err(protocol_error)?;
                    self.apply_global(GlobalControlEdit::Eq {
                        parameter,
                        value: F32Bits::from_le_bytes(record.body[4..8].try_into().unwrap()),
                    })?;
                    return Ok(None);
                }
                0x1001 if record.body.len() == 4 => {
                    let field = match word(&record.body, 0) {
                        0 => DrumSetting::Pattern,
                        1 => DrumSetting::Volume,
                        2 => DrumSetting::Tempo,
                        _ => return Err(protocol_error("Unrecovered drum selector")),
                    };
                    self.apply_global(GlobalControlEdit::Drum {
                        field,
                        value: word(&record.body, 2),
                    })?;
                    return Ok(None);
                }
                0x1011
                    if record.body.len() == 4
                        && word(&record.body, 0) == 0
                        && word(&record.body, 2) <= 1 =>
                {
                    self.apply_global(GlobalControlEdit::DrumPlay(word(&record.body, 2) == 1))?;
                    return Ok(None);
                }
                0x100f if record.body.len() == 4 => {
                    self.apply_global(GlobalControlEdit::Footswitch {
                        table: GlobalFootswitchTableId::new(word(&record.body, 0))
                            .map_err(protocol_error)?,
                        event: GlobalFootswitchEvent::new(record.body[2])
                            .map_err(protocol_error)?,
                        function: GlobalFootswitchFunction::new(record.body[3])
                            .map_err(protocol_error)?,
                    })?;
                    return Ok(None);
                }
                _ => {
                    return Err(ServiceError::unsupported(
                        "Unknown context-free simulated mutation",
                    ));
                }
            }
        }
        let context = WireContext::from_transfer(request)
            .ok_or_else(|| protocol_error("Simulated mutation requires an explicit context"))?;
        if context.selector > 255
            || context.group > 1
            || request.records().first() != Some(&context.to_record())
        {
            return Err(protocol_error("Simulated mutation has an invalid context"));
        }
        if context.group == 1
            && request
                .records()
                .get(1)
                .is_some_and(|record| record.command == 1)
        {
            if request
                == &gp200_protocol::clear_stored_request(context.selector)
                    .map_err(protocol_error)?
            {
                let cleared = RawTransfer::from_payload(
                    WRITE_KIND,
                    include_bytes!("../../../tests/fixtures/sim/device-cleared.payload.bin")
                        .to_vec(),
                )
                .map_err(protocol_error)?;
                self.stored[usize::from(context.selector)] =
                    set_slot(&cleared, 1, context.selector)?;
                return Ok(None);
            }
            let source = RawTransfer::from_records(WRITE_KIND, &request.records()[2..])
                .map_err(protocol_error)?;
            if full_stored_write_request(context.selector, &source).map_err(protocol_error)?
                != *request
            {
                return Err(protocol_error(
                    "Simulated full preset write differs from the established structure",
                ));
            }
            self.stored[usize::from(context.selector)] =
                context_reply(&source, 1, context.selector)?;
            return Ok(None);
        }
        if context.group == 0 && context.selector != self.selected {
            return Err(ServiceError::new(
                ErrorKind::Conflict,
                "Simulated buffer selection changed",
            ));
        }
        let source = if context.group == 0 {
            &self.current
        } else {
            &self.stored[usize::from(context.selector)]
        };
        let mut records = source.records().to_vec();
        // Validate and reconstruct a private copy before committing any mutation.
        for mutation in &request.records()[1..] {
            match mutation.command {
                0x0e if mutation.body.len() == 8 => {
                    let slot = gp200_protocol::ExpressionSlotId::from_packed(mutation.body[1])
                        .map_err(protocol_error)?;
                    let value = F32Bits::from_le_bytes(mutation.body[4..8].try_into().unwrap());
                    if mutation.body[0] > 1 || !value.as_f32().is_finite() {
                        return Err(protocol_error("Invalid simulated EXP endpoint"));
                    }
                    let matches: Vec<_> = records
                        .iter()
                        .enumerate()
                        .filter(|(_, record)| {
                            record.command == 12
                                && record.body.len() == 12
                                && record.body[0] == slot.packed()
                        })
                        .map(|(index, _)| index)
                        .collect();
                    if matches.len() != 1 {
                        return Err(protocol_error("EXP identity absent or ambiguous"));
                    }
                    let index = matches[0];
                    let module = records[index].body[1];
                    let parameter = records[index].body[2..4].to_vec();
                    if module > 10 {
                        return Err(protocol_error("EXP endpoints require an assigned module"));
                    }
                    // Deterministic simulator policy follows the native endpoint
                    // observer for references that share a parameter in one bank.
                    let offset = if mutation.body[0] == 1 { 4 } else { 8 };
                    for record in &mut records {
                        if record.command == 12
                            && record.body.len() == 12
                            && record.body[0] >> 4 == slot.group()
                            && record.body[1] == module
                            && record.body[2..4] == parameter
                        {
                            record.body[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
                        }
                    }
                }
                0x0c | 0x0f | 0x10 => {
                    gp200_protocol::patch_assignment_write_request(context, mutation)
                        .map_err(protocol_error)?;
                    let identity = mutation.body[0];
                    let matches: Vec<_> = records
                        .iter()
                        .enumerate()
                        .filter(|(_, record)| {
                            record.command == mutation.command
                                && record.body.first() == Some(&identity)
                        })
                        .map(|(index, _)| index)
                        .collect();
                    if matches.len() != 1 || records[matches[0]].body.len() != mutation.body.len() {
                        return Err(protocol_error(
                            "Simulated assignment identity is absent, ambiguous or malformed",
                        ));
                    }
                    let mut replacement = mutation.clone();
                    // Connected assignment-expression-clear-normalized proves
                    // only this exact native clear request normalizes param255
                    // to param0. Preserve all other OFF/opaque/stale records.
                    if replacement.command == 0x0c
                        && replacement.body[1..4] == [0xff, 0xff, 0]
                        && replacement.body[4..12] == [0; 8]
                    {
                        replacement.body[2..4].fill(0);
                    }
                    records[matches[0]] = replacement;
                }
                5 if mutation.body.len() == 12 => {
                    let module_id = mutation.body[0];
                    let parameter = mutation.body[1];
                    if parameter > 14 {
                        return Err(protocol_error("Simulated parameter outside retained slots"));
                    }
                    let module = module_record(&mut records, module_id)?;
                    let code =
                        u32::from_le_bytes(module.body[4..8].try_into().expect("four-byte code"));
                    let index = u32::from_le_bytes(
                        mutation.body[4..8].try_into().expect("four-byte index"),
                    );
                    let definition =
                        self.catalog.model_by_id(module_id, code).ok_or_else(|| {
                            ServiceError::unsupported("Unknown model has no XML Index mapping")
                        })?;
                    if u32::from(definition.xml_index) != index
                        || definition.parameter(parameter).is_none()
                    {
                        return Err(protocol_error(
                            "Simulated parameter XML ID/Index does not match its model",
                        ));
                    }
                    let offset = 8 + usize::from(parameter) * 4;
                    module.body[offset..offset + 4].copy_from_slice(&mutation.body[8..12]);
                }
                0x15 if mutation.body.len() == 4 && mutation.body[1] <= 1 => {
                    module_record(&mut records, mutation.body[0])?.body[1] = mutation.body[1]
                }
                0x16 if mutation.body.len() == 8 => {
                    let module_id = mutation.body[0];
                    let code =
                        u32::from_le_bytes(mutation.body[4..8].try_into().expect("four-byte code"));
                    let module = module_record(&mut records, module_id)?;
                    module.body[4..8].copy_from_slice(&code.to_le_bytes());
                    if let Some(definition) = self.catalog.model_by_id(module_id, code) {
                        gp200_catalog::ModelDefaultPlan::from_definition(definition)
                            .apply_to_module_body(&mut module.body)
                            .map_err(protocol_error)?;
                    }
                }
                8 if mutation.body.len() == 16 => {
                    let mut ids = mutation.body[4..15].to_vec();
                    ids.sort_unstable();
                    if ids != (0..11).collect::<Vec<_>>()
                        || word(&mutation.body, 0) != context.selector
                        || mutation.body[2] > mutation.body[3]
                        || mutation.body[3] > 11
                    {
                        return Err(protocol_error("Invalid simulated chain permutation/slot"));
                    }
                    *unique_record(&mut records, 8, 16)? = mutation.clone();
                }
                6 if mutation.body.len() == 4 => {
                    let selector = word(&mutation.body, 0);
                    if selector == 6 || selector == 5 {
                        let value =
                            i16::from_le_bytes(mutation.body[2..4].try_into().expect("word"));
                        if selector == 6 && !(-100..=100).contains(&value)
                            || selector == 5 && !(0..=1).contains(&value)
                        {
                            return Err(protocol_error("Invalid simulated Pan/FX mode"));
                        }
                        unique_record(&mut records, 2, 88)?.body
                            [if selector == 6 { 7 } else { 14 }] = value as u8;
                        continue;
                    }
                    let offset = match selector {
                        0 => 4,
                        1 => 2,
                        2 if word(&mutation.body, 2) < 20 => 8,
                        3 if word(&mutation.body, 2) <= 100 => 10,
                        4 if word(&mutation.body, 2) <= 100 => 12,
                        _ => {
                            return Err(ServiceError::unsupported(
                                "Unresolved simulated metadata selector",
                            ));
                        }
                    };
                    unique_record(&mut records, 2, 88)?.body[offset..offset + 2]
                        .copy_from_slice(&mutation.body[2..4]);
                }
                7 if mutation.body.len() == 20 && word(&mutation.body, 0) == context.selector => {
                    unique_record(&mut records, 2, 88)?.body[16..32]
                        .copy_from_slice(&mutation.body[4..20])
                }
                _ => {
                    return Err(ServiceError::unsupported(format!(
                        "Simulated command {:#x} is not implemented",
                        mutation.command
                    )));
                }
            }
        }
        let replacement =
            RawTransfer::from_records(WRITE_KIND, &records).map_err(protocol_error)?;
        if context.group == 0 {
            self.current = replacement;
        } else {
            self.stored[usize::from(context.selector)] = replacement;
        }
        Ok(None)
    }
    fn synchronize_global_selected_slot(&mut self, slot: u16) -> Result<(), ServiceError> {
        let mut records = self.globals.records().to_vec();
        unique_record(&mut records, gp200_protocol::GENERAL_SETTINGS_COMMAND, 72)?.body[4..6]
            .copy_from_slice(&slot.to_le_bytes());
        self.globals = RawTransfer::from_records(WRITE_KIND, &records).map_err(protocol_error)?;
        Ok(())
    }
    fn apply_global(&mut self, edit: GlobalControlEdit) -> Result<(), ServiceError> {
        let baseline = DeviceGlobalState::from_transfer(
            Revision(1),
            SessionEpoch(0),
            self.globals.clone(),
            vec![],
        )
        .map_err(protocol_error)?;
        self.globals = baseline.expected_after(&edit).map_err(protocol_error)?;
        Ok(())
    }
    pub fn external_parameter_edit(
        &mut self,
        module: u8,
        parameter: u8,
        value: F32Bits,
        now: Duration,
    ) -> Result<Vec<Delivery>, ServiceError> {
        if parameter > 14 {
            return Err(ServiceError::invalid(
                "External parameter outside retained slots",
            ));
        }
        let mut records = self.current.records().to_vec();
        let record = module_record(&mut records, module)?;
        let offset = 8 + usize::from(parameter) * 4;
        record.body[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        let code = u32::from_le_bytes(record.body[4..8].try_into().expect("code"));
        let index = self
            .catalog
            .model_by_id(module, code)
            .ok_or_else(|| ServiceError::unsupported("External edit needs known model Index"))?
            .xml_index;
        self.current = RawTransfer::from_records(WRITE_KIND, &records).map_err(protocol_error)?;
        let record = gp200_protocol::ParameterWrite {
            module_id: module,
            xml_param_id: parameter,
            model_index: gp200_protocol::WireModelIndex(u32::from(index)),
            value,
        }
        .to_record()
        .map_err(protocol_error)?;
        // Notification padding/context may differ on hardware. This shape is an
        // explicit scenario based on the observed command5 notification.
        let notification =
            RawTransfer::from_records(WRITE_KIND, &[record]).map_err(protocol_error)?;
        self.reply_delivery(notification, ReplyFault::default(), now)
    }
    fn reply_delivery(
        &self,
        mut reply: RawTransfer,
        fault: ReplyFault,
        now: Duration,
    ) -> Result<Vec<Delivery>, ServiceError> {
        let start = now + Duration::from_millis(self.profile.latency_ms);
        if fault.disconnect {
            return Ok(vec![Delivery::Disconnected { at: start }]);
        }
        if fault.silent {
            return Ok(Vec::new());
        }
        if let Some((group, selector)) = fault.wrong_context {
            reply = context_reply(&reply, group, selector)?;
        }
        let mut packets = encode_packets_with_chunk_size(
            reply.payload(),
            reply.kind(),
            self.profile.decoded_chunk_size,
        )
        .map_err(protocol_error)?;
        let mut modified = Vec::new();
        for (index, mut packet) in packets.drain(..).enumerate() {
            if fault.drop_packet == Some(index) {
                continue;
            }
            if fault.corrupt_packet == Some(index) {
                packet[13] = 16;
            }
            modified.push(packet.clone());
            if fault.duplicate_packet == Some(index) {
                modified.push(packet.clone());
            }
            if fault.conflicting_duplicate == Some(index) {
                packet[13] ^= 1;
                modified.push(packet);
            }
            if fault.unsolicited_after_packet == Some(index) {
                let notification = gp200_protocol::ParameterWrite {
                    module_id: 1,
                    xml_param_id: 3,
                    model_index: gp200_protocol::WireModelIndex(0),
                    value: F32Bits::from_f32(99.355),
                }
                .to_record()
                .map_err(protocol_error)?;
                let raw = RawTransfer::from_records(WRITE_KIND, &[notification])
                    .map_err(protocol_error)?;
                modified.extend(
                    encode_packets_with_chunk_size(raw.payload(), raw.kind(), 185)
                        .map_err(protocol_error)?,
                );
            }
        }
        if fault.reverse_packets {
            modified.reverse();
        }
        let mut deliveries = Vec::new();
        for (index, packet) in modified.into_iter().enumerate() {
            let at = start
                + Duration::from_millis(self.profile.inter_packet_ms.saturating_mul(index as u64));
            let packet = if let Some(realtime) = self.profile.realtime_byte {
                packet
                    .into_iter()
                    .flat_map(|byte| [byte, realtime])
                    .collect()
            } else {
                packet
            };
            let fragment = if self.profile.callback_chunk_size == 0 {
                packet.len()
            } else {
                self.profile.callback_chunk_size
            };
            for bytes in packet.chunks(fragment) {
                deliveries.push(Delivery::Bytes {
                    at,
                    bytes: bytes.to_vec(),
                });
            }
        }
        Ok(deliveries)
    }
}
