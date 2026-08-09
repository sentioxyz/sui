// Copyright (c) Sentio
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{BTreeMap, VecDeque},
    io::Read,
};

use anyhow::{Result, bail};
use move_binary_format::file_format::CodeOffset;
use move_core_types::{
    account_address::AccountAddress,
    identifier::Identifier,
    language_storage::{ModuleId, StructTag, TypeTag},
};
use move_trace_format::{
    format::{
        Effect, Frame, Location as TraceLocation, MoveTraceReader, TraceEvent, TraceIndex,
        TraceValue,
    },
    value::{SerializableMoveValue, SimplifiedMoveStruct, SimplifiedMoveVariant},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sui_types::{SUI_FRAMEWORK_ADDRESS, coin::Coin};

/// Backward-compatible Sentio call trace JSON shape.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallTraceWithSource {
    pub from: String,
    pub to: String,
    pub contract_name: String,
    pub function_name: String,
    pub inputs: Vec<Value>,
    pub return_value: Vec<Value>,
    pub type_args: Vec<String>,
    pub calls: Vec<CallTraceWithSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    pub pc: u16,
    pub gas_used: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<CallTraceError>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CallTraceError {
    pub major_status: String,
    pub sub_status: Option<u64>,
    pub message: Option<String>,
    pub location: Option<ModuleId>,
    pub function_name: Option<String>,
    pub code_offset: Option<CodeOffset>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Location {
    pub account: String,
    pub module: String,
    pub lines: Range,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Position {
    line: u32,
    column: u32,
}

struct PendingFrame {
    frame_id: usize,
    module_id: String,
    start_gas: u64,
    last_gas: u64,
    last_pc: u16,
    trace: CallTraceWithSource,
}

#[derive(Clone, Debug, Default)]
pub struct TraceCompatibilityContext {
    transfer_recipients: VecDeque<Option<Value>>,
}

impl TraceCompatibilityContext {
    pub fn new(transfer_recipients: Vec<Option<Value>>) -> Self {
        Self {
            transfer_recipients: transfer_recipients.into(),
        }
    }

    fn pop_transfer_recipient(&mut self) -> Option<Value> {
        self.transfer_recipients.pop_front().flatten()
    }
}

pub fn transfer_objects_trace(inputs: Vec<Value>) -> CallTraceWithSource {
    let sui_framework = SUI_FRAMEWORK_ADDRESS.to_string();
    call_trace(
        &sui_framework,
        &sui_framework,
        "transfer_objects",
        inputs,
        vec![],
        0,
        0,
    )
}

#[allow(dead_code)]
pub fn call_trace_from_reader<R: Read>(
    reader: MoveTraceReader<'_, R>,
) -> Result<Option<Vec<CallTraceWithSource>>> {
    call_trace_from_reader_with_context(reader, TraceCompatibilityContext::default())
}

pub fn call_trace_from_reader_with_context<R: Read>(
    reader: MoveTraceReader<'_, R>,
    mut context: TraceCompatibilityContext,
) -> Result<Option<Vec<CallTraceWithSource>>> {
    let mut stack: Vec<PendingFrame> = vec![];
    let mut roots: Vec<CallTraceWithSource> = vec![];
    let mut memory = TraceMemoryState::default();
    let mut error_halt_gas_left = None;

    for event in reader {
        match event? {
            TraceEvent::OpenFrame { frame, gas_left } => {
                let frame = memory.frame_with_current_global_references(*frame);
                let pending = open_frame(frame.clone(), gas_left, stack.last());
                memory.open_frame(&frame);
                stack.push(pending);
            }
            TraceEvent::CloseFrame {
                frame_id,
                return_,
                gas_left,
            } => {
                let return_ = memory.return_values_with_current_global_references(return_);
                close_frame(&mut stack, &mut roots, frame_id, return_, gas_left)?;
                memory.close_frame();
            }
            TraceEvent::Instruction {
                pc,
                gas_left,
                instruction,
                ..
            } => {
                memory.start_instruction(&instruction);
                if let Some(frame) = stack.last_mut() {
                    frame.last_pc = pc;
                    frame.last_gas = gas_left;
                }
            }
            TraceEvent::Effect(effect) => {
                let effect = *effect;
                if let Effect::ExecutionError(message) = &effect {
                    if error_halt_gas_left.is_none() {
                        error_halt_gas_left = stack.last().map(|frame| frame.last_gas);
                    }
                    for frame in &mut stack {
                        frame.trace.error = Some(call_trace_error_from_execution_error(message));
                    }
                }
                memory.apply_effect(&effect);
            }
            TraceEvent::External(event) => {
                if let Some(trace) =
                    transfer_objects_trace_from_external_event(&event, &mut context)
                {
                    roots.push(trace);
                }
            }
        }
    }

    close_unclosed_frames(&mut stack, &mut roots, error_halt_gas_left);
    Ok(Some(roots))
}

#[derive(Default)]
struct TraceMemoryState {
    loaded_state: BTreeMap<TraceIndex, SerializableMoveValue>,
    operand_stack: Vec<TraceValue>,
    call_stack: BTreeMap<TraceIndex, BTreeMap<usize, TraceValue>>,
    local_aliases: BTreeMap<(TraceIndex, usize), TraceLocation>,
    current_instruction: Option<String>,
    current_instruction_pops: Vec<TraceValue>,
    current_vector_write_applied: bool,
}

#[derive(Clone)]
enum VectorMutation {
    Push(SerializableMoveValue),
    Swap(usize, usize),
    PopBack,
}

impl TraceMemoryState {
    fn start_instruction(&mut self, instruction: &str) {
        self.current_instruction = Some(instruction.to_owned());
        self.current_instruction_pops.clear();
        self.current_vector_write_applied = false;
    }

    fn frame_with_current_global_references(&self, mut frame: Frame) -> Frame {
        frame.parameters = frame
            .parameters
            .into_iter()
            .map(|value| self.trace_value_with_current_global_snapshot(value))
            .collect();
        frame
    }

    fn open_frame(&mut self, frame: &Frame) {
        let mut locals = BTreeMap::new();
        for (index, parameter) in frame.parameters.iter().enumerate() {
            self.operand_stack.pop();
            if let Some(location) = parameter.location().cloned() {
                self.local_aliases.insert((frame.frame_id, index), location);
            }
            locals.insert(index, parameter.clone());
        }
        self.call_stack.insert(frame.frame_id, locals);
    }

    fn close_frame(&mut self) {
        if let Some((frame_id, _locals)) = self.call_stack.pop_last() {
            self.local_aliases
                .retain(|(alias_frame_id, _), _| *alias_frame_id != frame_id);
        }
    }

    fn return_values_with_current_global_references(
        &self,
        values: Vec<TraceValue>,
    ) -> Vec<TraceValue> {
        values
            .into_iter()
            .map(|value| self.trace_value_with_current_global_snapshot(value))
            .collect()
    }

    fn apply_effect(&mut self, effect: &Effect) {
        match effect {
            Effect::Push(value) => {
                self.operand_stack.push(value.clone());
                self.apply_current_vec_pop_back();
            }
            Effect::Pop(value) => {
                self.current_instruction_pops.push(value.clone());
                self.operand_stack.pop();
            }
            Effect::Read(read) => {
                if read.moved
                    && let TraceLocation::Local(frame_id, local_index) = read.location
                    && let Some(frame) = self.call_stack.get_mut(&frame_id)
                {
                    frame.remove(&local_index);
                }
            }
            Effect::Write(write) => {
                if self.apply_current_vector_write(&write.location) {
                    return;
                }
                let exact_write_snapshot = self.current_write_ref_exact_snapshot(&write.location);
                if let Some(exact_write_snapshot) = exact_write_snapshot {
                    let mut applied =
                        self.write_exact_snapshot(&write.location, exact_write_snapshot.clone());
                    if let Some(alias_location) = self.aliased_location(&write.location) {
                        applied |= self.write_exact_snapshot(&alias_location, exact_write_snapshot);
                    }
                    if !applied {
                        applied = self.write_exact_snapshot_into_trace_root(
                            &write.location,
                            trace_value_root_snapshot(&write.root_value_after_write).clone(),
                            self.current_write_ref_exact_snapshot(&write.location)
                                .expect("exact write snapshot was just checked"),
                        );
                    }
                    if applied {
                        return;
                    }
                }
                let snapshot = trace_value_root_snapshot(&write.root_value_after_write).clone();
                self.write_root_snapshot(&write.location, snapshot.clone());
                if let Some(alias_location) = self.local_root_alias_location(&write.location) {
                    self.write_root_snapshot(&alias_location, snapshot);
                }
            }
            Effect::DataLoad(data_load) => {
                if let TraceLocation::Global(global_id) = data_load.location {
                    self.loaded_state
                        .insert(global_id, data_load.snapshot.clone());
                }
            }
            Effect::ExecutionError(_) => {}
        }
    }

    fn write_root_snapshot(&mut self, location: &TraceLocation, snapshot: SerializableMoveValue) {
        match location {
            TraceLocation::Local(frame_id, local_index) => {
                if let Some(frame) = self.call_stack.get_mut(frame_id) {
                    frame.insert(*local_index, TraceValue::RuntimeValue { value: snapshot });
                }
            }
            TraceLocation::Indexed(parent, _) => {
                if let Some(value) = self.value_mut_at_root_location(parent) {
                    *value = snapshot;
                } else if let Some(global_id) = global_root_location(parent) {
                    self.loaded_state.insert(global_id, snapshot);
                }
            }
            TraceLocation::Global(global_id) => {
                self.loaded_state.insert(*global_id, snapshot);
            }
        }
    }

    fn write_exact_snapshot(
        &mut self,
        location: &TraceLocation,
        snapshot: SerializableMoveValue,
    ) -> bool {
        match location {
            TraceLocation::Global(global_id) => {
                self.loaded_state.insert(*global_id, snapshot);
                true
            }
            TraceLocation::Local(frame_id, local_index) => {
                if let Some(frame) = self.call_stack.get_mut(frame_id) {
                    frame.insert(*local_index, TraceValue::RuntimeValue { value: snapshot });
                    true
                } else {
                    false
                }
            }
            TraceLocation::Indexed(_, _) => {
                if let Some(value) = self.value_mut_at_exact_location(location) {
                    *value = snapshot;
                    true
                } else {
                    false
                }
            }
        }
    }

    fn write_exact_snapshot_into_trace_root(
        &mut self,
        location: &TraceLocation,
        mut root: SerializableMoveValue,
        snapshot: SerializableMoveValue,
    ) -> bool {
        let Some(value) = value_mut_at_trace_location(&mut root, location) else {
            return false;
        };
        *value = snapshot;
        self.write_root_snapshot(location, root);
        true
    }

    fn local_root_alias_location(&self, location: &TraceLocation) -> Option<TraceLocation> {
        let local_root = local_root_location(location)?;
        self.local_aliases.get(&local_root).cloned()
    }

    fn aliased_location(&self, location: &TraceLocation) -> Option<TraceLocation> {
        match location {
            TraceLocation::Local(frame_id, local_index) => {
                self.local_aliases.get(&(*frame_id, *local_index)).cloned()
            }
            TraceLocation::Indexed(parent, index) => self
                .aliased_location(parent)
                .map(|parent| TraceLocation::Indexed(Box::new(parent), *index)),
            TraceLocation::Global(_) => None,
        }
    }

    fn current_write_ref_exact_snapshot(
        &self,
        location: &TraceLocation,
    ) -> Option<SerializableMoveValue> {
        if self.current_instruction.as_deref() != Some("WRITE_REF") {
            return None;
        }
        let [reference, value] = self.current_instruction_pops.as_slice() else {
            return None;
        };
        if reference.location()? != location {
            return None;
        }
        Some(trace_value_snapshot(value).clone())
    }

    fn apply_current_vector_write(&mut self, location: &TraceLocation) -> bool {
        if self.current_vector_write_applied {
            return true;
        }
        let mutation = match self.current_instruction.as_deref() {
            Some("VEC_PUSH_BACK") => self.current_vec_push_back_mutation(location),
            Some("VEC_SWAP") => self.current_vec_swap_mutation(location),
            _ => None,
        };
        if let Some(mutation) = mutation {
            let applied = self.apply_vector_mutation(location, &mutation);
            self.current_vector_write_applied = applied;
            return applied;
        }
        false
    }

    fn apply_current_vec_pop_back(&mut self) {
        if self.current_vector_write_applied
            || self.current_instruction.as_deref() != Some("VEC_POP_BACK")
        {
            return;
        }
        let [reference] = self.current_instruction_pops.as_slice() else {
            return;
        };
        let Some(location) = reference.location().cloned() else {
            return;
        };
        self.current_vector_write_applied =
            self.apply_vector_mutation(&location, &VectorMutation::PopBack);
    }

    fn current_vec_push_back_mutation(&self, location: &TraceLocation) -> Option<VectorMutation> {
        let [value, reference] = self.current_instruction_pops.as_slice() else {
            return None;
        };
        if reference.location()? != location {
            return None;
        }
        Some(VectorMutation::Push(trace_value_snapshot(value).clone()))
    }

    fn current_vec_swap_mutation(&self, location: &TraceLocation) -> Option<VectorMutation> {
        let [right_index, left_index, reference] = self.current_instruction_pops.as_slice() else {
            return None;
        };
        if reference.location()? != location {
            return None;
        }
        Some(VectorMutation::Swap(
            usize::try_from(numeric_trace_value(left_index)?).ok()?,
            usize::try_from(numeric_trace_value(right_index)?).ok()?,
        ))
    }

    fn apply_vector_mutation(
        &mut self,
        location: &TraceLocation,
        mutation: &VectorMutation,
    ) -> bool {
        let mut applied = self
            .apply_vector_mutation_at_location(location, mutation)
            .is_some();
        if let Some(alias_location) = self.aliased_location(location) {
            applied |= self
                .apply_vector_mutation_at_location(&alias_location, mutation)
                .is_some();
        }
        applied
    }

    fn apply_vector_mutation_at_location(
        &mut self,
        location: &TraceLocation,
        mutation: &VectorMutation,
    ) -> Option<()> {
        let vector = self.value_mut_at_exact_location(location)?;
        let SerializableMoveValue::Vector(values) = vector else {
            return None;
        };
        match mutation {
            VectorMutation::Push(value) => values.push(value.clone()),
            VectorMutation::Swap(left, right) => {
                if *left >= values.len() || *right >= values.len() {
                    return None;
                }
                values.swap(*left, *right);
            }
            VectorMutation::PopBack => {
                values.pop()?;
            }
        }
        Some(())
    }

    fn trace_value_with_current_global_snapshot(&self, value: TraceValue) -> TraceValue {
        match value {
            TraceValue::RuntimeValue { value } => TraceValue::RuntimeValue { value },
            TraceValue::ImmRef { location, snapshot } => TraceValue::ImmRef {
                snapshot: Box::new(
                    self.current_global_root_snapshot(&location)
                        .unwrap_or_else(|| *snapshot),
                ),
                location,
            },
            TraceValue::MutRef { location, snapshot } => TraceValue::MutRef {
                snapshot: Box::new(
                    self.current_global_root_snapshot(&location)
                        .unwrap_or_else(|| *snapshot),
                ),
                location,
            },
        }
    }

    fn current_global_root_snapshot(
        &self,
        location: &TraceLocation,
    ) -> Option<SerializableMoveValue> {
        self.loaded_state
            .get(&global_root_location(location)?)
            .cloned()
    }

    fn value_mut_at_root_location(
        &mut self,
        location: &TraceLocation,
    ) -> Option<&mut SerializableMoveValue> {
        match location {
            TraceLocation::Local(frame_id, local_index) => self
                .call_stack
                .get_mut(frame_id)?
                .get_mut(local_index)?
                .value_mut(),
            TraceLocation::Indexed(parent, _) => self.value_mut_at_root_location(parent),
            TraceLocation::Global(global_id) => self.loaded_state.get_mut(global_id),
        }
    }

    fn value_mut_at_exact_location(
        &mut self,
        location: &TraceLocation,
    ) -> Option<&mut SerializableMoveValue> {
        match location {
            TraceLocation::Local(frame_id, local_index) => self
                .call_stack
                .get_mut(frame_id)?
                .get_mut(local_index)?
                .value_mut(),
            TraceLocation::Global(global_id) => self.loaded_state.get_mut(global_id),
            TraceLocation::Indexed(parent, index) => {
                child_value_mut(self.value_mut_at_exact_location(parent)?, *index)
            }
        }
    }
}

fn global_root_location(location: &TraceLocation) -> Option<TraceIndex> {
    match location {
        TraceLocation::Global(global_id) => Some(*global_id),
        TraceLocation::Indexed(parent, _) => global_root_location(parent),
        TraceLocation::Local(_, _) => None,
    }
}

fn local_root_location(location: &TraceLocation) -> Option<(TraceIndex, usize)> {
    match location {
        TraceLocation::Local(frame_id, local_index) => Some((*frame_id, *local_index)),
        TraceLocation::Indexed(parent, _) => local_root_location(parent),
        TraceLocation::Global(_) => None,
    }
}

#[allow(dead_code)]
#[derive(Deserialize)]
enum PtbEventCompat {
    ExternalEvent(PtbExternalEventCompat),
    Summary(Value),
    MoveCallStart,
    MoveCallEnd,
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct PtbExternalEventCompat {
    name: String,
    values: Vec<ExtMoveValueCompat>,
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct ExtMoveValueInfoCompat {
    #[serde(rename = "type_")]
    _type: Value,
    value: SerializableMoveValue,
}

#[allow(dead_code)]
#[derive(Deserialize)]
enum ExtMoveValueCompat {
    Single {
        name: String,
        info: ExtMoveValueInfoCompat,
    },
    Vector {
        name: String,
        #[serde(rename = "type_")]
        _type: Value,
        value: Vec<SerializableMoveValue>,
    },
}

fn transfer_objects_trace_from_external_event(
    event: &Value,
    context: &mut TraceCompatibilityContext,
) -> Option<CallTraceWithSource> {
    let PtbEventCompat::ExternalEvent(event) =
        serde_json::from_value::<PtbEventCompat>(event.clone()).ok()?
    else {
        return None;
    };
    if event.name != "Transfer" {
        return None;
    }
    let recipient = context.pop_transfer_recipient()?;
    let mut inputs = event
        .values
        .iter()
        .map(ext_move_value_to_json)
        .collect::<Vec<_>>();
    inputs.push(recipient);

    Some(transfer_objects_trace(inputs))
}

fn ext_move_value_to_json(value: &ExtMoveValueCompat) -> Value {
    match value {
        ExtMoveValueCompat::Single { info, .. } => transfer_input_value_to_json(&info.value),
        ExtMoveValueCompat::Vector { value, .. } => Value::Array(
            value
                .iter()
                .map(transfer_input_value_to_json)
                .collect::<Vec<_>>(),
        ),
    }
}

fn transfer_input_value_to_json(value: &SerializableMoveValue) -> Value {
    if let SerializableMoveValue::Struct(struct_value) = value
        && let Some(coin) = coin_transfer_input_to_json(struct_value)
    {
        return coin;
    }
    if matches!(value, SerializableMoveValue::Struct(_)) {
        return Value::String(legacy_annotated_move_value_string(value));
    }
    serializable_move_value_to_json(value)
}

fn coin_transfer_input_to_json(value: &SimplifiedMoveStruct) -> Option<Value> {
    if !Coin::is_coin(&value.type_) {
        return None;
    }
    let coin_id = value
        .fields
        .iter()
        .find(|(field, _)| field.as_str() == "id")
        .and_then(|(_, value)| object_id_address_from_uid(value))?;
    let balance = value
        .fields
        .iter()
        .find(|(field, _)| field.as_str() == "balance")
        .and_then(|(_, value)| balance_from_balance_struct(value))?;

    let mut fields = Map::new();
    fields.insert("id".to_string(), Value::String(coin_id));
    fields.insert("balance".to_string(), Value::String(balance.to_string()));

    let mut map = Map::new();
    map.insert(
        "type".to_string(),
        serde_json::to_value(&value.type_).unwrap(),
    );
    map.insert("fields".to_string(), Value::Object(fields));
    Some(Value::Object(map))
}

fn legacy_annotated_move_value_string(value: &SerializableMoveValue) -> String {
    serde_json::to_string(&legacy_annotated_move_value_to_json(value)).unwrap()
}

fn legacy_annotated_move_value_to_json(value: &SerializableMoveValue) -> Value {
    match value {
        SerializableMoveValue::Bool(v) => Value::Bool(*v),
        SerializableMoveValue::U8(v) => serde_json::to_value(v).unwrap(),
        SerializableMoveValue::U16(v) => serde_json::to_value(v).unwrap(),
        SerializableMoveValue::U32(v) => serde_json::to_value(v).unwrap(),
        SerializableMoveValue::U64(v) => serde_json::to_value(v).unwrap(),
        SerializableMoveValue::U128(v) => {
            serde_json::to_value(v).unwrap_or_else(|_| Value::String(v.to_string()))
        }
        SerializableMoveValue::U256(v) => Value::String(v.to_string()),
        SerializableMoveValue::Address(v) => serde_json::to_value(v).unwrap(),
        SerializableMoveValue::Struct(v) => legacy_annotated_struct_to_json(v),
        SerializableMoveValue::Vector(v) => Value::Array(
            v.iter()
                .map(legacy_annotated_move_value_to_json)
                .collect::<Vec<_>>(),
        ),
        SerializableMoveValue::Variant(v) => Value::String(v.to_string()),
    }
}

fn legacy_annotated_struct_to_json(value: &SimplifiedMoveStruct) -> Value {
    let mut map = Map::new();
    map.insert(
        "type".to_string(),
        Value::String(legacy_struct_tag_to_string(&value.type_)),
    );
    map.insert(
        "fields".to_string(),
        legacy_annotated_struct_fields_to_json(&value.fields),
    );
    Value::Object(map)
}

fn legacy_annotated_struct_fields_to_json(fields: &[(Identifier, SerializableMoveValue)]) -> Value {
    let mut map = Map::new();
    for (field_name, field_value) in fields {
        map.insert(
            field_name.to_string(),
            legacy_annotated_move_value_to_json(field_value),
        );
    }
    Value::Object(map)
}

fn legacy_struct_tag_to_string(tag: &StructTag) -> String {
    let mut value = format!(
        "{}::{}::{}",
        legacy_address_to_string(&tag.address),
        tag.module,
        tag.name
    );
    if !tag.type_params.is_empty() {
        let type_params = tag
            .type_params
            .iter()
            .map(legacy_type_tag_to_string)
            .collect::<Vec<_>>()
            .join(", ");
        value.push('<');
        value.push_str(&type_params);
        value.push('>');
    }
    value
}

fn legacy_address_to_string(address: &AccountAddress) -> String {
    let address = address.short_str_lossless();
    if address.starts_with("0x") {
        address
    } else {
        format!("0x{address}")
    }
}

fn legacy_type_tag_to_string(tag: &TypeTag) -> String {
    match tag {
        TypeTag::Bool => "bool".to_string(),
        TypeTag::U8 => "u8".to_string(),
        TypeTag::U16 => "u16".to_string(),
        TypeTag::U32 => "u32".to_string(),
        TypeTag::U64 => "u64".to_string(),
        TypeTag::U128 => "u128".to_string(),
        TypeTag::U256 => "u256".to_string(),
        TypeTag::Address => "address".to_string(),
        TypeTag::Signer => "signer".to_string(),
        TypeTag::Vector(inner) => format!("vector<{}>", legacy_type_tag_to_string(inner)),
        TypeTag::Struct(inner) => legacy_struct_tag_to_string(inner),
    }
}

fn object_id_address_from_uid(value: &SerializableMoveValue) -> Option<String> {
    let SerializableMoveValue::Struct(uid) = value else {
        return None;
    };
    let (_, id_value) = uid
        .fields
        .iter()
        .find(|(field, _)| field.as_str() == "id")?;
    let SerializableMoveValue::Struct(id) = id_value else {
        return None;
    };
    let (_, bytes_value) = id
        .fields
        .iter()
        .find(|(field, _)| field.as_str() == "bytes")?;
    if let SerializableMoveValue::Address(address) = bytes_value {
        return serde_json::to_value(address)
            .ok()?
            .as_str()
            .map(ToOwned::to_owned);
    }
    let bytes = u8_vector(bytes_value)?;
    if bytes.len() != 32 {
        return None;
    }
    Some(hex::encode(bytes))
}

fn balance_from_balance_struct(value: &SerializableMoveValue) -> Option<u64> {
    match value {
        SerializableMoveValue::U64(balance) => Some(*balance),
        SerializableMoveValue::Struct(balance) => balance
            .fields
            .iter()
            .find(|(field, _)| field.as_str() == "value")
            .and_then(|(_, value)| match value {
                SerializableMoveValue::U64(balance) => Some(*balance),
                _ => None,
            }),
        _ => None,
    }
}

fn u8_vector(value: &SerializableMoveValue) -> Option<Vec<u8>> {
    let SerializableMoveValue::Vector(values) = value else {
        return None;
    };
    values
        .iter()
        .map(|value| match value {
            SerializableMoveValue::U8(byte) => Some(*byte),
            _ => None,
        })
        .collect()
}

fn call_trace_error_from_execution_error(message: &str) -> CallTraceError {
    if let Some(error) = call_trace_error_from_vm_error_debug(message) {
        return error;
    }
    CallTraceError {
        major_status: message.to_owned(),
        sub_status: None,
        message: Some(message.to_owned()),
        location: None,
        function_name: None,
        code_offset: None,
    }
}

fn call_trace_error_from_vm_error_debug(message: &str) -> Option<CallTraceError> {
    let vm_error = message.split_once("VMError {")?.1;
    let major_status = text_between(vm_error, "major_status: ", ",")?.to_owned();
    let sub_status = optional_number_after(vm_error, "sub_status: ");
    let vm_message = optional_debug_string_after(vm_error, "message: ");
    let location = module_id_after(vm_error, "location: Module(ModuleId { ");
    let function_name = optional_debug_string_after(message, "function_name: ");
    let code_offset = code_offset_from_debug(message).or_else(|| code_offset_from_message(message));

    Some(CallTraceError {
        major_status,
        sub_status,
        message: vm_message,
        location,
        function_name,
        code_offset,
    })
}

fn text_between<'a>(value: &'a str, prefix: &str, suffix: &str) -> Option<&'a str> {
    let start = value.find(prefix)? + prefix.len();
    let rest = &value[start..];
    let end = rest.find(suffix)?;
    Some(&rest[..end])
}

fn optional_number_after(value: &str, prefix: &str) -> Option<u64> {
    let rest = value.split_once(prefix)?.1;
    let rest = rest.strip_prefix("Some(")?;
    let end = rest.find(')')?;
    rest[..end].parse().ok()
}

fn optional_debug_string_after(value: &str, prefix: &str) -> Option<String> {
    let rest = value.split_once(prefix)?.1;
    let rest = rest.strip_prefix("Some(\"")?;
    let escaped = read_debug_string_until_end(rest, ')')?;
    serde_json::from_str(&format!("\"{escaped}\"")).ok()
}

fn debug_string_after(value: &str, prefix: &str, terminator: char) -> Option<String> {
    let rest = value.split_once(prefix)?.1;
    let rest = rest.strip_prefix('"')?;
    let escaped = read_debug_string_until_end(rest, terminator)?;
    serde_json::from_str(&format!("\"{escaped}\"")).ok()
}

fn read_debug_string_until_end(value: &str, terminator: char) -> Option<&str> {
    let mut escaped = false;
    for (index, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' if value[index + ch.len_utf8()..].starts_with(terminator) => {
                return Some(&value[..index]);
            }
            _ => {}
        }
    }
    None
}

fn module_id_after(value: &str, prefix: &str) -> Option<ModuleId> {
    let rest = value.split_once(prefix)?.1;
    let address = text_between(rest, "address: ", ",")?;
    let module = debug_string_after(rest, "name: Identifier(", ')')?;
    let address = AccountAddress::from_hex_literal(&format!("0x{address}")).ok()?;
    let module = Identifier::new(module).ok()?;
    Some(ModuleId::new(address, module))
}

fn code_offset_from_debug(value: &str) -> Option<CodeOffset> {
    let rest = value.rsplit_once("FunctionDefinitionIndex(")?.1;
    let rest = rest.split_once("), ")?.1;
    let offset = rest.split_once(')')?.0;
    offset.parse().ok()
}

fn code_offset_from_message(value: &str) -> Option<CodeOffset> {
    let rest = value.rsplit_once(" at offset ")?.1;
    let offset = rest
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    offset.parse().ok()
}

fn open_frame(frame: Frame, gas_left: u64, parent: Option<&PendingFrame>) -> PendingFrame {
    let module_id = normalized_module_id(&frame);
    let from_module_id = parent
        .map(|parent| parent.module_id.clone())
        .unwrap_or_else(|| module_id.clone());
    let trace = call_trace(
        &from_module_id,
        &module_id,
        &frame.function_name,
        frame
            .parameters
            .iter()
            .enumerate()
            .map(|(index, value)| input_trace_value_to_json(&frame, index, value))
            .collect(),
        frame
            .type_instantiation
            .iter()
            .map(ToString::to_string)
            .collect(),
        parent.map_or(0, |parent| parent.last_pc),
        0,
    );

    PendingFrame {
        frame_id: frame.frame_id,
        module_id,
        start_gas: gas_left,
        last_gas: gas_left,
        last_pc: 0,
        trace,
    }
}

fn close_frame(
    stack: &mut Vec<PendingFrame>,
    roots: &mut Vec<CallTraceWithSource>,
    frame_id: usize,
    return_values: Vec<TraceValue>,
    gas_left: u64,
) -> Result<()> {
    let Some(mut frame) = stack.pop() else {
        bail!("unbalanced trace: close frame {frame_id} without open frame");
    };
    if frame.frame_id != frame_id {
        bail!(
            "unbalanced trace: close frame {frame_id}, expected {}",
            frame.frame_id
        );
    }

    frame.trace.return_value =
        return_trace_values_to_json(&frame.trace.function_name, &return_values);
    frame.trace.gas_used = frame.start_gas.saturating_sub(gas_left);
    apply_legacy_trace_postprocessing(&mut frame.trace);

    if let Some(parent) = stack.last_mut() {
        parent.trace.calls.push(frame.trace);
    } else {
        roots.push(frame.trace);
    }
    Ok(())
}

fn close_unclosed_frames(
    stack: &mut Vec<PendingFrame>,
    roots: &mut Vec<CallTraceWithSource>,
    error_halt_gas_left: Option<u64>,
) {
    while let Some(mut frame) = stack.pop() {
        frame.trace.gas_used = match error_halt_gas_left {
            Some(gas_left) if !stack.is_empty() => frame.start_gas.saturating_sub(gas_left),
            Some(_) => frame.start_gas,
            None => frame.start_gas.saturating_sub(frame.last_gas),
        };
        apply_legacy_trace_postprocessing(&mut frame.trace);
        if let Some(parent) = stack.last_mut() {
            parent.trace.calls.push(frame.trace);
        } else {
            roots.push(frame.trace);
        }
    }
}

fn apply_legacy_trace_postprocessing(trace: &mut CallTraceWithSource) {
    for call in &mut trace.calls {
        apply_legacy_trace_postprocessing(call);
    }
}

fn call_trace(
    from_module_id: &str,
    module_id: &str,
    function: &str,
    inputs: Vec<Value>,
    type_args: Vec<String>,
    pc: u16,
    gas_used: u64,
) -> CallTraceWithSource {
    let (from, contract_name) = split_module_for_source(from_module_id);
    let (to, to_module_name) = split_module_for_source(module_id);
    CallTraceWithSource {
        from,
        to,
        contract_name,
        function_name: format!("{to_module_name}::{function}"),
        inputs,
        return_value: vec![],
        type_args,
        calls: vec![],
        location: None,
        pc,
        gas_used,
        error: None,
    }
}

fn normalized_module_id(frame: &Frame) -> String {
    let module_name = frame.module.name();
    format!("{}::{}", frame.module.address(), module_name)
}

fn split_module_for_source(module_id: &str) -> (String, String) {
    let default = SUI_FRAMEWORK_ADDRESS.to_string();
    let mut parts = module_id.split("::");
    let account = parts.next().unwrap_or(&default).to_owned();
    let module = parts.next().unwrap_or(&default).to_owned();
    (account, module)
}

fn trace_value_to_json(value: &TraceValue) -> Value {
    serializable_move_value_to_json(trace_value_snapshot(value))
}

fn input_trace_value_to_json(frame: &Frame, index: usize, value: &TraceValue) -> Value {
    if frame.module.name().as_str() == "cursor"
        && frame.function_name.as_str() == "new"
        && index == 0
    {
        return unknown_value();
    }
    if frame.module.name().as_str() == "bcs" && frame.function_name.as_str() == "to_bytes" {
        return unknown_value();
    }
    if frame.module.name().as_str() == "hot_potato_vector"
        && frame.function_name.as_str() == "new"
        && index == 0
    {
        return unknown_value();
    }
    if is_std_vector_frame(frame)
        && (is_ref_trace_value(value) || frame.function_name.as_str() == "append")
    {
        return unknown_value();
    }
    if is_coin_vector_trace_value(value)
        || (frame.module.name().as_str() == "pay"
            && frame.function_name.as_str() == "join_vec"
            && index == 1
            && matches!(
                trace_value_snapshot(value),
                SerializableMoveValue::Vector(values) if values.is_empty()
            ))
        || (frame.function_name.as_str() == "merge_coins"
            && matches!(
                trace_value_snapshot(value),
                SerializableMoveValue::Vector(values) if values.is_empty()
            ))
        || (frame.function_name.as_str() == "swapWithReturn"
            && matches!(
                trace_value_snapshot(value),
                SerializableMoveValue::Vector(values) if values.is_empty()
            ))
        || is_router_swap_cap_trace_value(value)
    {
        return unknown_value();
    }
    if is_sui_object_frame(frame)
        && matches!(
            frame.function_name.as_str(),
            "id" | "id_address" | "borrow_id" | "borrow_uid"
        )
        && is_ref_trace_value(value)
    {
        return unknown_value();
    }
    if frame.module.name().as_str() == "vec_set"
        && matches!(frame.function_name.as_str(), "contains" | "remove")
        && index == 1
        && is_ref_trace_value(value)
    {
        return unknown_value();
    }
    if frame.module.name().as_str() == "vec_map"
        && matches!(
            frame.function_name.as_str(),
            "contains" | "get" | "get_idx" | "get_idx_opt"
        )
        && index == 1
        && is_ref_trace_value(value)
    {
        return unknown_value();
    }
    if frame.module.name().as_str() == "option"
        && frame.function_name.as_str() == "contains"
        && index == 1
        && is_ref_trace_value(value)
    {
        return unknown_value();
    }
    if frame.module.name().as_str() == "ordered_map"
        && matches!(
            frame.function_name.as_str(),
            "binary_search_p" | "remove_at"
        )
        && index == 0
    {
        return unknown_value();
    }
    trace_value_to_json(value)
}

fn return_trace_values_to_json(function_name: &str, values: &[TraceValue]) -> Vec<Value> {
    values
        .iter()
        .map(|value| return_trace_value_to_json(function_name, value))
        .collect()
}

fn return_trace_value_to_json(function_name: &str, value: &TraceValue) -> Value {
    if function_name == "vec_set::into_keys"
        || function_name == "vector::singleton"
        || legacy_unknown_return(function_name)
        || is_router_swap_cap_trace_value(value)
        || (is_ref_trace_value(value) && legacy_unknown_ref_return(function_name))
    {
        return unknown_value();
    }
    trace_value_to_json(value)
}

fn is_std_vector_frame(frame: &Frame) -> bool {
    frame.module.address() == &move_core_types::account_address::AccountAddress::ONE
        && frame.module.name().as_str() == "vector"
}

fn is_sui_object_frame(frame: &Frame) -> bool {
    frame.module.address() == &SUI_FRAMEWORK_ADDRESS && frame.module.name().as_str() == "object"
}

fn is_ref_trace_value(value: &TraceValue) -> bool {
    matches!(value, TraceValue::ImmRef { .. } | TraceValue::MutRef { .. })
}

fn is_coin_vector_trace_value(value: &TraceValue) -> bool {
    matches!(
        trace_value_snapshot(value),
        SerializableMoveValue::Vector(values) if values.iter().any(is_coin_move_value)
    )
}

fn is_coin_move_value(value: &SerializableMoveValue) -> bool {
    matches!(value, SerializableMoveValue::Struct(value) if Coin::is_coin(&value.type_))
}

fn is_router_swap_cap_trace_value(value: &TraceValue) -> bool {
    matches!(
        trace_value_snapshot(value),
        SerializableMoveValue::Struct(value)
            if value.type_.module.as_str() == "router"
                && value.type_.name.as_str() == "RouterSwapCapExtended"
    )
}

fn legacy_unknown_ref_return(function_name: &str) -> bool {
    function_name.starts_with("vector::") || legacy_unknown_return(function_name)
}

fn legacy_unknown_return(function_name: &str) -> bool {
    matches!(
        function_name,
        "option::borrow"
            | "table::borrow"
            | "table::borrow_mut"
            | "dynamic_field::borrow"
            | "dynamic_field::borrow_mut"
            | "dynamic_field::borrow_child_object"
            | "dynamic_field::borrow_child_object_mut"
            | "dynamic_field::remove_child_object"
            | "dynamic_object_field::borrow"
            | "dynamic_object_field::borrow_mut"
            | "bag::borrow"
            | "bag::borrow_mut"
            | "versioned::load_value"
            | "versioned::load_value_mut"
            | "big_vector::borrow"
            | "big_vector::borrow_mut"
            | "big_vector::slice_borrow"
            | "big_vector::slice_borrow_mut"
            | "cell::get"
            | "cursor::take_rest"
            | "hot_potato_vector::borrow"
            | "margin_registry::get_config"
            | "skip_list::borrow_value"
            | "skip_list::borrow_mut"
            | "vec_map::get"
    )
}

fn trace_value_root_snapshot(value: &TraceValue) -> &SerializableMoveValue {
    match value {
        TraceValue::RuntimeValue { value } => value,
        TraceValue::ImmRef { snapshot, .. } | TraceValue::MutRef { snapshot, .. } => snapshot,
    }
}

fn trace_value_snapshot(value: &TraceValue) -> &SerializableMoveValue {
    match value {
        TraceValue::RuntimeValue { value } => value,
        TraceValue::ImmRef { location, snapshot } | TraceValue::MutRef { location, snapshot } => {
            value_at_trace_location(snapshot, location).unwrap_or(snapshot)
        }
    }
}

fn value_at_trace_location<'a>(
    root: &'a SerializableMoveValue,
    location: &TraceLocation,
) -> Option<&'a SerializableMoveValue> {
    match location {
        TraceLocation::Local(_, _) | TraceLocation::Global(_) => Some(root),
        TraceLocation::Indexed(parent, index) => {
            let parent = value_at_trace_location(root, parent)?;
            child_value(parent, *index)
        }
    }
}

fn value_mut_at_trace_location<'a>(
    root: &'a mut SerializableMoveValue,
    location: &TraceLocation,
) -> Option<&'a mut SerializableMoveValue> {
    match location {
        TraceLocation::Local(_, _) | TraceLocation::Global(_) => Some(root),
        TraceLocation::Indexed(parent, index) => {
            let parent = value_mut_at_trace_location(root, parent)?;
            child_value_mut(parent, *index)
        }
    }
}

fn child_value(value: &SerializableMoveValue, index: usize) -> Option<&SerializableMoveValue> {
    match value {
        SerializableMoveValue::Struct(value) => value.fields.get(index).map(|(_, value)| value),
        SerializableMoveValue::Variant(value) => value.fields.get(index).map(|(_, value)| value),
        SerializableMoveValue::Vector(values) => values.get(index),
        _ => None,
    }
}

fn child_value_mut(
    value: &mut SerializableMoveValue,
    index: usize,
) -> Option<&mut SerializableMoveValue> {
    match value {
        SerializableMoveValue::Struct(value) => value.fields.get_mut(index).map(|(_, value)| value),
        SerializableMoveValue::Variant(value) => {
            value.fields.get_mut(index).map(|(_, value)| value)
        }
        SerializableMoveValue::Vector(values) => values.get_mut(index),
        _ => None,
    }
}

fn numeric_value(value: &SerializableMoveValue) -> Option<u128> {
    match value {
        SerializableMoveValue::U8(value) => Some((*value).into()),
        SerializableMoveValue::U16(value) => Some((*value).into()),
        SerializableMoveValue::U32(value) => Some((*value).into()),
        SerializableMoveValue::U64(value) => Some((*value).into()),
        SerializableMoveValue::U128(value) => Some(*value),
        SerializableMoveValue::U256(value) => value.to_string().parse().ok(),
        _ => None,
    }
}

fn numeric_trace_value(value: &TraceValue) -> Option<u128> {
    numeric_value(trace_value_snapshot(value))
}

fn unknown_value() -> Value {
    Value::String("?".to_string())
}

fn serializable_move_value_to_json(value: &SerializableMoveValue) -> Value {
    match value {
        SerializableMoveValue::Bool(v) => Value::Bool(*v),
        SerializableMoveValue::U8(v) => serde_json::to_value(v).unwrap(),
        SerializableMoveValue::U16(v) => serde_json::to_value(v).unwrap(),
        SerializableMoveValue::U32(v) => serde_json::to_value(v).unwrap(),
        SerializableMoveValue::U64(v) => Value::String(v.to_string()),
        SerializableMoveValue::U128(v) => Value::String(v.to_string()),
        SerializableMoveValue::U256(v) => Value::String(v.to_string()),
        SerializableMoveValue::Address(v) => serde_json::to_value(v).unwrap(),
        SerializableMoveValue::Struct(v) => struct_to_json(v),
        SerializableMoveValue::Vector(v) => vector_to_json(v),
        SerializableMoveValue::Variant(v) => variant_to_json(v),
    }
}

fn struct_to_json(value: &SimplifiedMoveStruct) -> Value {
    let mut map = Map::new();
    map.insert(
        "type".to_string(),
        serde_json::to_value(&value.type_).unwrap(),
    );
    map.insert("fields".to_string(), struct_fields_to_json(&value.fields));
    Value::Object(map)
}

fn struct_fields_to_json(fields: &[(Identifier, SerializableMoveValue)]) -> Value {
    let mut map = Map::new();
    for (field_name, field_value) in fields {
        map.insert(
            field_name.to_string(),
            serializable_move_value_to_json(field_value),
        );
    }
    Value::Object(map)
}

fn variant_to_json(value: &SimplifiedMoveVariant) -> Value {
    Value::String(value.to_string())
}

fn vector_to_json(values: &[SerializableMoveValue]) -> Value {
    if matches!(values.last(), Some(SerializableMoveValue::U8(_))) {
        let bytes = values
            .iter()
            .map(|value| match value {
                SerializableMoveValue::U8(byte) => *byte,
                _ => unreachable!(),
            })
            .collect::<Vec<_>>();
        Value::String(format!("0x{}", hex::encode(bytes)))
    } else {
        Value::Array(values.iter().map(serializable_move_value_to_json).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use move_core_types::{
        account_address::AccountAddress,
        language_storage::{StructTag, TypeTag},
        u256,
    };
    use move_trace_format::format::{
        DataLoad, Effect, MoveTraceBuilder, RefType, TraceValue, TypeTagWithRefs, Write,
    };
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn value_conversion_matches_old_trace_v2_shapes() {
        assert_eq!(
            serializable_move_value_to_json(&SerializableMoveValue::U8(7)),
            json!(7)
        );
        assert_eq!(
            serializable_move_value_to_json(&SerializableMoveValue::U16(8)),
            json!(8)
        );
        assert_eq!(
            serializable_move_value_to_json(&SerializableMoveValue::U32(9)),
            json!(9)
        );
        assert_eq!(
            serializable_move_value_to_json(&SerializableMoveValue::U64(10)),
            json!("10")
        );
        assert_eq!(
            serializable_move_value_to_json(&SerializableMoveValue::U128(11)),
            json!("11")
        );
        assert_eq!(
            serializable_move_value_to_json(&SerializableMoveValue::U256(u256::U256::from(12u8))),
            json!("12")
        );
        assert_eq!(
            serializable_move_value_to_json(&SerializableMoveValue::Bool(true)),
            json!(true)
        );
    }

    #[test]
    fn vector_u8_is_hex_only_when_non_empty() {
        let bytes = SerializableMoveValue::Vector(vec![
            SerializableMoveValue::U8(0),
            SerializableMoveValue::U8(15),
            SerializableMoveValue::U8(255),
        ]);
        assert_eq!(serializable_move_value_to_json(&bytes), json!("0x000fff"));

        let empty = SerializableMoveValue::Vector(vec![]);
        assert_eq!(serializable_move_value_to_json(&empty), json!([]));

        let u64_values = SerializableMoveValue::Vector(vec![
            SerializableMoveValue::U64(1),
            SerializableMoveValue::U64(2),
        ]);
        assert_eq!(
            serializable_move_value_to_json(&u64_values),
            json!(["1", "2"])
        );
    }

    #[test]
    fn reference_trace_values_project_to_referenced_child() {
        let root = SerializableMoveValue::Struct(SimplifiedMoveStruct {
            type_: struct_tag("0x42", "module", "Thing"),
            fields: vec![
                (
                    Identifier::new("count").unwrap(),
                    SerializableMoveValue::U64(42),
                ),
                (
                    Identifier::new("bytes").unwrap(),
                    SerializableMoveValue::Vector(vec![
                        SerializableMoveValue::U8(0xab),
                        SerializableMoveValue::U8(0xcd),
                    ]),
                ),
            ],
        });
        let value = TraceValue::ImmRef {
            location: TraceLocation::Indexed(Box::new(TraceLocation::Local(1, 0)), 1),
            snapshot: Box::new(root),
        };

        assert_eq!(trace_value_to_json(&value), json!("0xabcd"));
    }

    #[test]
    fn open_frame_references_use_current_global_snapshot() {
        let initial = count_struct(1);
        let updated = count_struct(2);
        let location = TraceLocation::Global(7);
        let mut first = frame(1, "0x2", "target", "first");
        first.parameters = vec![TraceValue::MutRef {
            location: location.clone(),
            snapshot: Box::new(initial.clone()),
        }];
        let mut second = frame(2, "0x2", "target", "second");
        second.parameters = vec![TraceValue::MutRef {
            location: location.clone(),
            snapshot: Box::new(initial.clone()),
        }];

        let mut builder = MoveTraceBuilder::new();
        builder.push_event(TraceEvent::Effect(Box::new(Effect::DataLoad(DataLoad {
            ref_type: RefType::Mut,
            location: location.clone(),
            snapshot: initial,
        }))));
        builder.push_event(TraceEvent::OpenFrame {
            frame: Box::new(first),
            gas_left: 100,
        });
        builder.push_event(TraceEvent::Effect(Box::new(Effect::Write(Write {
            location,
            root_value_after_write: runtime(updated),
        }))));
        builder.push_event(TraceEvent::CloseFrame {
            frame_id: 1,
            return_: vec![],
            gas_left: 90,
        });
        builder.push_event(TraceEvent::OpenFrame {
            frame: Box::new(second),
            gas_left: 90,
        });
        builder.push_event(TraceEvent::CloseFrame {
            frame_id: 2,
            return_: vec![],
            gas_left: 80,
        });

        let trace = MoveTraceReader::new(Cursor::new(
            builder.into_trace().into_compressed_json_bytes(),
        ))
        .unwrap();
        let roots = call_trace_from_reader(trace).unwrap().unwrap();

        assert_eq!(roots[0].inputs[0]["fields"]["count"], json!("1"));
        assert_eq!(roots[1].inputs[0]["fields"]["count"], json!("2"));
    }

    #[test]
    fn indexed_global_write_initializes_current_root_snapshot() {
        let initial = count_struct(1);
        let updated = count_struct(2);
        let location = TraceLocation::Global(7);
        let mut first = frame(1, "0x2", "target", "first");
        first.parameters = vec![TraceValue::MutRef {
            location: location.clone(),
            snapshot: Box::new(initial.clone()),
        }];
        let mut second = frame(2, "0x2", "target", "second");
        second.parameters = vec![TraceValue::MutRef {
            location: location.clone(),
            snapshot: Box::new(initial),
        }];

        let mut builder = MoveTraceBuilder::new();
        builder.push_event(TraceEvent::OpenFrame {
            frame: Box::new(first),
            gas_left: 100,
        });
        builder.push_event(TraceEvent::Effect(Box::new(Effect::Write(Write {
            location: TraceLocation::Indexed(Box::new(location), 0),
            root_value_after_write: runtime(updated),
        }))));
        builder.push_event(TraceEvent::CloseFrame {
            frame_id: 1,
            return_: vec![],
            gas_left: 90,
        });
        builder.push_event(TraceEvent::OpenFrame {
            frame: Box::new(second),
            gas_left: 90,
        });
        builder.push_event(TraceEvent::CloseFrame {
            frame_id: 2,
            return_: vec![],
            gas_left: 80,
        });

        let trace = MoveTraceReader::new(Cursor::new(
            builder.into_trace().into_compressed_json_bytes(),
        ))
        .unwrap();
        let roots = call_trace_from_reader(trace).unwrap().unwrap();

        assert_eq!(roots[1].inputs[0]["fields"]["count"], json!("2"));
    }

    #[test]
    fn write_ref_uses_exact_stack_value_for_global_snapshot() {
        let initial = count_struct(1);
        let root_location = TraceLocation::Global(7);
        let field_location = TraceLocation::Indexed(Box::new(root_location.clone()), 0);
        let mut writer = frame(1, "0x2", "target", "writer");
        writer.parameters = vec![TraceValue::MutRef {
            location: field_location.clone(),
            snapshot: Box::new(initial.clone()),
        }];
        let mut reader = frame(2, "0x2", "target", "reader");
        reader.parameters = vec![TraceValue::MutRef {
            location: root_location,
            snapshot: Box::new(initial.clone()),
        }];

        let mut builder = MoveTraceBuilder::new();
        builder.push_event(TraceEvent::OpenFrame {
            frame: Box::new(writer),
            gas_left: 100,
        });
        builder.push_event(TraceEvent::Instruction {
            type_parameters: vec![],
            pc: 1,
            gas_left: 95,
            instruction: Box::new("WRITE_REF".to_string()),
        });
        builder.push_event(TraceEvent::Effect(Box::new(Effect::Pop(
            TraceValue::MutRef {
                location: field_location.clone(),
                snapshot: Box::new(initial.clone()),
            },
        ))));
        builder.push_event(TraceEvent::Effect(Box::new(Effect::Pop(runtime(
            SerializableMoveValue::U64(2),
        )))));
        builder.push_event(TraceEvent::Effect(Box::new(Effect::Write(Write {
            location: field_location,
            root_value_after_write: runtime(count_struct(99)),
        }))));
        builder.push_event(TraceEvent::CloseFrame {
            frame_id: 1,
            return_: vec![],
            gas_left: 90,
        });
        builder.push_event(TraceEvent::OpenFrame {
            frame: Box::new(reader),
            gas_left: 90,
        });
        builder.push_event(TraceEvent::CloseFrame {
            frame_id: 2,
            return_: vec![],
            gas_left: 80,
        });

        let trace = MoveTraceReader::new(Cursor::new(
            builder.into_trace().into_compressed_json_bytes(),
        ))
        .unwrap();
        let roots = call_trace_from_reader(trace).unwrap().unwrap();

        assert_eq!(roots[1].inputs[0]["fields"]["count"], json!("2"));
    }

    #[test]
    fn vector_instructions_replay_current_global_snapshot() {
        let initial = vector_struct(&[1, 2]);
        let after_push = vector_struct(&[1, 2, 3]);
        let after_swap = vector_struct(&[3, 2, 1]);
        let after_pop = vector_struct(&[3, 2]);
        let root_location = TraceLocation::Global(8);
        let vector_location = TraceLocation::Indexed(Box::new(root_location.clone()), 0);
        let vector_ref = |snapshot: SerializableMoveValue| TraceValue::MutRef {
            location: vector_location.clone(),
            snapshot: Box::new(snapshot),
        };
        let mut writer = frame(1, "0x2", "target", "writer");
        writer.parameters = vec![vector_ref(initial.clone())];
        let mut reader = frame(2, "0x2", "target", "reader");
        reader.parameters = vec![vector_ref(initial.clone())];

        let mut builder = MoveTraceBuilder::new();
        builder.push_event(TraceEvent::Effect(Box::new(Effect::DataLoad(DataLoad {
            ref_type: RefType::Mut,
            location: root_location,
            snapshot: initial.clone(),
        }))));
        builder.push_event(TraceEvent::OpenFrame {
            frame: Box::new(writer),
            gas_left: 100,
        });
        builder.push_event(TraceEvent::Instruction {
            type_parameters: vec![],
            pc: 1,
            gas_left: 95,
            instruction: Box::new("VEC_PUSH_BACK".to_string()),
        });
        builder.push_event(TraceEvent::Effect(Box::new(Effect::Pop(runtime(
            SerializableMoveValue::U64(3),
        )))));
        builder.push_event(TraceEvent::Effect(Box::new(Effect::Pop(vector_ref(
            after_push.clone(),
        )))));
        builder.push_event(TraceEvent::Effect(Box::new(Effect::Write(Write {
            location: vector_location.clone(),
            root_value_after_write: runtime(after_push.clone()),
        }))));
        builder.push_event(TraceEvent::Instruction {
            type_parameters: vec![],
            pc: 2,
            gas_left: 94,
            instruction: Box::new("VEC_SWAP".to_string()),
        });
        builder.push_event(TraceEvent::Effect(Box::new(Effect::Pop(runtime(
            SerializableMoveValue::U64(2),
        )))));
        builder.push_event(TraceEvent::Effect(Box::new(Effect::Pop(runtime(
            SerializableMoveValue::U64(0),
        )))));
        builder.push_event(TraceEvent::Effect(Box::new(Effect::Pop(vector_ref(
            after_swap.clone(),
        )))));
        builder.push_event(TraceEvent::Effect(Box::new(Effect::Write(Write {
            location: vector_location.clone(),
            root_value_after_write: runtime(after_swap.clone()),
        }))));
        builder.push_event(TraceEvent::Instruction {
            type_parameters: vec![],
            pc: 3,
            gas_left: 93,
            instruction: Box::new("VEC_POP_BACK".to_string()),
        });
        builder.push_event(TraceEvent::Effect(Box::new(Effect::Pop(vector_ref(
            after_swap,
        )))));
        builder.push_event(TraceEvent::Effect(Box::new(Effect::Push(runtime(
            SerializableMoveValue::U64(1),
        )))));
        builder.push_event(TraceEvent::Effect(Box::new(Effect::Write(Write {
            location: vector_location,
            root_value_after_write: runtime(after_pop),
        }))));
        builder.push_event(TraceEvent::CloseFrame {
            frame_id: 1,
            return_: vec![],
            gas_left: 90,
        });
        builder.push_event(TraceEvent::OpenFrame {
            frame: Box::new(reader),
            gas_left: 90,
        });
        builder.push_event(TraceEvent::CloseFrame {
            frame_id: 2,
            return_: vec![],
            gas_left: 80,
        });

        let trace = MoveTraceReader::new(Cursor::new(
            builder.into_trace().into_compressed_json_bytes(),
        ))
        .unwrap();
        let roots = call_trace_from_reader(trace).unwrap().unwrap();

        assert_eq!(roots[1].inputs[0], json!(["3", "2"]));
    }

    #[test]
    fn open_frame_local_references_keep_frame_snapshot() {
        let mut parent = frame(1, "0x2", "target", "parent");
        parent.parameters = vec![runtime(count_struct(1))];
        let mut child = frame(2, "0x2", "target", "child");
        child.parameters = vec![TraceValue::MutRef {
            location: TraceLocation::Local(1, 0),
            snapshot: Box::new(count_struct(2)),
        }];

        let mut builder = MoveTraceBuilder::new();
        builder.push_event(TraceEvent::OpenFrame {
            frame: Box::new(parent),
            gas_left: 100,
        });
        builder.push_event(TraceEvent::OpenFrame {
            frame: Box::new(child),
            gas_left: 90,
        });
        builder.push_event(TraceEvent::CloseFrame {
            frame_id: 2,
            return_: vec![],
            gas_left: 80,
        });
        builder.push_event(TraceEvent::CloseFrame {
            frame_id: 1,
            return_: vec![],
            gas_left: 70,
        });

        let trace = MoveTraceReader::new(Cursor::new(
            builder.into_trace().into_compressed_json_bytes(),
        ))
        .unwrap();
        let roots = call_trace_from_reader(trace).unwrap().unwrap();

        assert_eq!(roots[0].calls[0].inputs[0]["fields"]["count"], json!("2"));
    }

    #[test]
    fn cursor_new_bytes_input_matches_old_unknown_value() {
        let frame = frame(1, "0x42", "cursor", "new");
        let value = runtime(SerializableMoveValue::Vector(vec![
            SerializableMoveValue::U8(0xab),
            SerializableMoveValue::U8(0xcd),
        ]));

        assert_eq!(input_trace_value_to_json(&frame, 0, &value), json!("?"));
    }

    #[test]
    fn std_vector_reference_inputs_match_old_unknown_value() {
        let frame = frame(1, "0x1", "vector", "reverse");
        let value = TraceValue::MutRef {
            location: TraceLocation::Local(1, 0),
            snapshot: Box::new(SerializableMoveValue::Vector(vec![
                SerializableMoveValue::U8(0xab),
            ])),
        };

        assert_eq!(input_trace_value_to_json(&frame, 0, &value), json!("?"));
    }

    #[test]
    fn std_vector_append_inputs_match_old_unknown_value() {
        let frame = frame(1, "0x1", "vector", "append");
        let value = runtime(SerializableMoveValue::Vector(vec![
            SerializableMoveValue::U64(42),
        ]));

        assert_eq!(input_trace_value_to_json(&frame, 0, &value), json!("?"));
    }

    #[test]
    fn coin_vector_inputs_match_old_unknown_value() {
        let frame = frame(1, "0x42", "pool", "merge_coin");
        let value = runtime(SerializableMoveValue::Vector(vec![coin_value(0x77, 123)]));

        assert_eq!(input_trace_value_to_json(&frame, 0, &value), json!("?"));
    }

    #[test]
    fn pay_join_vec_empty_vector_input_matches_old_unknown_value() {
        let frame = frame(1, "0x2", "pay", "join_vec");
        let value = runtime(SerializableMoveValue::Vector(vec![]));

        assert_eq!(input_trace_value_to_json(&frame, 1, &value), json!("?"));
    }

    #[test]
    fn merge_coins_empty_vector_input_matches_old_unknown_value() {
        let frame = frame(1, "0x42", "router", "merge_coins");
        let value = runtime(SerializableMoveValue::Vector(vec![]));

        assert_eq!(input_trace_value_to_json(&frame, 0, &value), json!("?"));
    }

    #[test]
    fn swap_with_return_empty_vector_input_matches_old_unknown_value() {
        let frame = frame(1, "0x42", "router", "swapWithReturn");
        let value = runtime(SerializableMoveValue::Vector(vec![]));

        assert_eq!(input_trace_value_to_json(&frame, 2, &value), json!("?"));
    }

    #[test]
    fn ordered_map_search_inputs_match_old_unknown_value() {
        let frame = frame(1, "0x42", "ordered_map", "binary_search_p");
        let value = runtime(SerializableMoveValue::Vector(vec![
            SerializableMoveValue::U64(42),
        ]));

        assert_eq!(input_trace_value_to_json(&frame, 0, &value), json!("?"));
    }

    #[test]
    fn vec_map_key_reference_inputs_match_old_unknown_value() {
        let frame = frame(1, "0x42", "vec_map", "get");
        let value = TraceValue::ImmRef {
            location: TraceLocation::Local(1, 0),
            snapshot: Box::new(count_struct(42)),
        };

        assert_eq!(input_trace_value_to_json(&frame, 1, &value), json!("?"));
    }

    #[test]
    fn sui_object_id_address_reference_input_matches_old_unknown_value() {
        let frame = frame(1, "0x2", "object", "id_address");
        let value = TraceValue::ImmRef {
            location: TraceLocation::Local(1, 0),
            snapshot: Box::new(count_struct(42)),
        };

        assert_eq!(input_trace_value_to_json(&frame, 0, &value), json!("?"));
    }

    #[test]
    fn bcs_to_bytes_inputs_match_old_unknown_value() {
        let frame = frame(1, "0x1", "bcs", "to_bytes");
        let value = runtime(SerializableMoveValue::U64(42));

        assert_eq!(input_trace_value_to_json(&frame, 0, &value), json!("?"));
    }

    #[test]
    fn reference_returns_match_old_unknown_value() {
        let value = TraceValue::ImmRef {
            location: TraceLocation::Local(1, 0),
            snapshot: Box::new(SerializableMoveValue::U64(42)),
        };

        assert_eq!(
            return_trace_value_to_json("table::borrow", &value),
            json!("?")
        );
    }

    #[test]
    fn borrow_runtime_returns_match_old_unknown_value() {
        let value = runtime(SerializableMoveValue::U64(42));

        assert_eq!(
            return_trace_value_to_json("dynamic_object_field::borrow_mut", &value),
            json!("?")
        );
        assert_eq!(
            return_trace_value_to_json("big_vector::borrow", &value),
            json!("?")
        );
        assert_eq!(return_trace_value_to_json("cell::get", &value), json!("?"));
        assert_eq!(
            return_trace_value_to_json("skip_list::borrow_value", &value),
            json!("?")
        );
        assert_eq!(
            return_trace_value_to_json("skip_list::borrow_mut", &value),
            json!("?")
        );
    }

    #[test]
    fn vector_singleton_return_matches_old_unknown_value() {
        let value = runtime(SerializableMoveValue::Vector(vec![
            SerializableMoveValue::U64(42),
        ]));

        assert_eq!(
            return_trace_value_to_json("vector::singleton", &value),
            json!("?")
        );
    }

    #[test]
    fn cursor_take_rest_return_matches_old_unknown_value() {
        let value = runtime(SerializableMoveValue::Vector(vec![
            SerializableMoveValue::U8(0xab),
        ]));

        assert_eq!(
            return_trace_value_to_json("cursor::take_rest", &value),
            json!("?")
        );
    }

    #[test]
    fn router_swap_cap_values_match_old_unknown_value() {
        let frame = frame(1, "0x42", "router", "use_cap");
        let value = runtime(SerializableMoveValue::Struct(SimplifiedMoveStruct {
            type_: struct_tag("0x42", "router", "RouterSwapCapExtended"),
            fields: vec![(
                Identifier::new("expected_amount_out").unwrap(),
                SerializableMoveValue::U64(42),
            )],
        }));

        assert_eq!(input_trace_value_to_json(&frame, 0, &value), json!("?"));
        assert_eq!(
            return_trace_value_to_json("router::begin_router_tx", &value),
            json!("?")
        );
    }

    #[test]
    fn non_coin_transfer_input_matches_old_annotated_string() {
        let value = SerializableMoveValue::Struct(SimplifiedMoveStruct {
            type_: struct_tag("0x42", "module", "Thing"),
            fields: vec![
                (
                    Identifier::new("count").unwrap(),
                    SerializableMoveValue::U64(42),
                ),
                (
                    Identifier::new("bytes").unwrap(),
                    SerializableMoveValue::Vector(vec![
                        SerializableMoveValue::U8(0xab),
                        SerializableMoveValue::U8(0xcd),
                    ]),
                ),
            ],
        });

        assert_eq!(
            transfer_input_value_to_json(&value),
            json!(
                "{\"type\":\"0x42::module::Thing\",\"fields\":{\"count\":42,\"bytes\":[171,205]}}"
            )
        );
    }

    #[test]
    fn struct_conversion_keeps_type_and_fields() {
        let value = SerializableMoveValue::Struct(SimplifiedMoveStruct {
            type_: struct_tag("0x42", "module", "Thing"),
            fields: vec![
                (
                    Identifier::new("count").unwrap(),
                    SerializableMoveValue::U64(42),
                ),
                (
                    Identifier::new("bytes").unwrap(),
                    SerializableMoveValue::Vector(vec![
                        SerializableMoveValue::U8(0xab),
                        SerializableMoveValue::U8(0xcd),
                    ]),
                ),
            ],
        });

        assert_eq!(
            serializable_move_value_to_json(&value),
            json!({
                "type": {
                    "address": "0000000000000000000000000000000000000000000000000000000000000042",
                    "module": "module",
                    "name": "Thing",
                    "type_args": []
                },
                "fields": {
                    "count": "42",
                    "bytes": "0xabcd"
                }
            })
        );
    }

    #[test]
    fn frame_open_close_builds_old_call_tree_shape() {
        let mut roots = vec![];
        let mut parent = open_frame(frame(1, "0xaa", "parent", "outer"), 100, None);
        parent.last_pc = 7;
        let child = open_frame(frame(2, "0xbb", "child", "inner"), 90, Some(&parent));
        let mut stack = vec![parent, child];

        close_frame(
            &mut stack,
            &mut roots,
            2,
            vec![runtime(SerializableMoveValue::U64(55))],
            70,
        )
        .unwrap();
        close_frame(&mut stack, &mut roots, 1, vec![], 60).unwrap();

        assert_eq!(roots.len(), 1);
        assert_eq!(
            roots[0].from,
            "00000000000000000000000000000000000000000000000000000000000000aa"
        );
        assert_eq!(
            roots[0].to,
            "00000000000000000000000000000000000000000000000000000000000000aa"
        );
        assert_eq!(roots[0].contract_name, "parent");
        assert_eq!(roots[0].function_name, "parent::outer");
        assert_eq!(roots[0].type_args, vec!["u64"]);
        assert_eq!(roots[0].pc, 0);
        assert_eq!(roots[0].gas_used, 40);
        assert_eq!(roots[0].calls.len(), 1);

        let child = &roots[0].calls[0];
        assert_eq!(
            child.from,
            "00000000000000000000000000000000000000000000000000000000000000aa"
        );
        assert_eq!(
            child.to,
            "00000000000000000000000000000000000000000000000000000000000000bb"
        );
        assert_eq!(child.contract_name, "parent");
        assert_eq!(child.function_name, "child::inner");
        assert_eq!(child.type_args, vec!["u64"]);
        assert_eq!(child.return_value, vec![json!("55")]);
        assert_eq!(child.pc, 7);
        assert_eq!(child.gas_used, 20);
    }

    #[test]
    fn close_frame_without_open_reports_unbalanced_trace() {
        let mut stack = vec![];
        let mut roots = vec![];

        let err = close_frame(&mut stack, &mut roots, 1, vec![], 90).unwrap_err();

        assert_eq!(
            err.to_string(),
            "unbalanced trace: close frame 1 without open frame"
        );
        assert!(roots.is_empty());
    }

    #[test]
    fn close_frame_id_mismatch_reports_expected_frame() {
        let mut stack = vec![open_frame(frame(7, "0xaa", "parent", "outer"), 100, None)];
        let mut roots = vec![];

        let err = close_frame(&mut stack, &mut roots, 8, vec![], 90).unwrap_err();

        assert_eq!(
            err.to_string(),
            "unbalanced trace: close frame 8, expected 7"
        );
        assert!(roots.is_empty());
    }

    #[test]
    fn reader_preserves_error_trace_when_frame_does_not_close() {
        let mut builder = MoveTraceBuilder::new();
        builder.push_event(TraceEvent::OpenFrame {
            frame: Box::new(frame(1, "0xcc", "aborter", "boom")),
            gas_left: 100,
        });
        builder.push_event(TraceEvent::Instruction {
            type_parameters: vec![],
            pc: 11,
            gas_left: 73,
            instruction: Box::new("Abort".to_string()),
        });
        builder.push_event(TraceEvent::External(Box::new(json!({
            "ignored": true
        }))));
        builder.effect(Effect::ExecutionError("ABORTED".to_string()));

        let trace = MoveTraceReader::new(Cursor::new(
            builder.into_trace().into_compressed_json_bytes(),
        ))
        .unwrap();
        let roots = call_trace_from_reader(trace).unwrap().unwrap();

        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].function_name, "aborter::boom");
        assert_eq!(roots[0].gas_used, 100);
        assert!(roots[0].calls.is_empty());
        assert!(roots[0].return_value.is_empty());

        let error = roots[0].error.as_ref().unwrap();
        assert_eq!(error.major_status, "ABORTED");
        assert_eq!(error.message.as_deref(), Some("ABORTED"));
        assert_eq!(error.sub_status, None);
        assert_eq!(error.location, None);
        assert_eq!(error.function_name, None);
        assert_eq!(error.code_offset, None);
    }

    #[test]
    fn execution_error_is_attached_to_all_open_frames() {
        let mut builder = MoveTraceBuilder::new();
        builder.push_event(TraceEvent::OpenFrame {
            frame: Box::new(frame(1, "0xaa", "parent", "outer")),
            gas_left: 100,
        });
        builder.push_event(TraceEvent::Instruction {
            type_parameters: vec![],
            pc: 9,
            gas_left: 80,
            instruction: Box::new("Call".to_string()),
        });
        builder.push_event(TraceEvent::OpenFrame {
            frame: Box::new(frame(2, "0xbb", "middle", "mid")),
            gas_left: 70,
        });
        builder.push_event(TraceEvent::Instruction {
            type_parameters: vec![],
            pc: 4,
            gas_left: 60,
            instruction: Box::new("Call".to_string()),
        });
        builder.push_event(TraceEvent::OpenFrame {
            frame: Box::new(frame(3, "0xcc", "child", "inner")),
            gas_left: 50,
        });
        builder.push_event(TraceEvent::Instruction {
            type_parameters: vec![],
            pc: 3,
            gas_left: 40,
            instruction: Box::new("Abort".to_string()),
        });
        builder.effect(Effect::ExecutionError("ABORTED".to_string()));

        let trace = MoveTraceReader::new(Cursor::new(
            builder.into_trace().into_compressed_json_bytes(),
        ))
        .unwrap();
        let roots = call_trace_from_reader(trace).unwrap().unwrap();

        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].function_name, "parent::outer");
        assert_eq!(roots[0].error.as_ref().unwrap().major_status, "ABORTED");
        assert_eq!(roots[0].gas_used, 100);
        assert_eq!(roots[0].calls.len(), 1);

        let middle = &roots[0].calls[0];
        assert_eq!(middle.function_name, "middle::mid");
        assert_eq!(middle.error.as_ref().unwrap().major_status, "ABORTED");
        assert_eq!(middle.pc, 9);
        assert_eq!(middle.gas_used, 30);
        assert_eq!(middle.calls.len(), 1);

        let child = &middle.calls[0];
        assert_eq!(child.function_name, "child::inner");
        assert_eq!(child.error.as_ref().unwrap().major_status, "ABORTED");
        assert_eq!(child.pc, 4);
        assert_eq!(child.gas_used, 10);
    }

    #[test]
    fn vm_error_debug_string_is_converted_to_old_error_shape() {
        let message = r#"ExecutionError: ExecutionError { inner: ExecutionErrorInner { kind: MoveAbort(MoveLocation { module: ModuleId { address: b29d83c26cdd2a64959263abbcfc4a6937f0c9fccaf98580ca56faded65be244, name: Identifier("balance_manager") }, function: 21, instruction: 55, function_name: Some("withdraw_with_proof") }, 3), source: Some(VMError { major_status: ABORTED, sub_status: Some(3), message: Some("0x2c8d603bc51326b8c13cef9dd07031a408a48dddb541963357661df5d3204809::balance_manager::withdraw_with_proof at offset 55"), exec_state: None, location: Module(ModuleId { address: 2c8d603bc51326b8c13cef9dd07031a408a48dddb541963357661df5d3204809, name: Identifier("balance_manager") }), indices: [], offsets: [(FunctionDefinitionIndex(21), 55)] }), command: Some(1) } }"#;

        let error = call_trace_error_from_execution_error(message);

        assert_eq!(error.major_status, "ABORTED");
        assert_eq!(error.sub_status, Some(3));
        assert_eq!(
            error.message.as_deref(),
            Some(
                "0x2c8d603bc51326b8c13cef9dd07031a408a48dddb541963357661df5d3204809::balance_manager::withdraw_with_proof at offset 55"
            )
        );
        assert_eq!(error.function_name.as_deref(), Some("withdraw_with_proof"));
        assert_eq!(error.code_offset, Some(55));
        assert_eq!(
            error.location.as_ref().map(ToString::to_string).as_deref(),
            Some(
                "2c8d603bc51326b8c13cef9dd07031a408a48dddb541963357661df5d3204809::balance_manager"
            )
        );
    }

    #[test]
    fn transfer_external_event_builds_old_transfer_objects_root() {
        let mut builder = MoveTraceBuilder::new();
        builder.push_event(TraceEvent::External(Box::new(json!({
            "ExternalEvent": {
                "description": "TransferObjects: obj0...objN => ()",
                "name": "Transfer",
                "values": [
                    {
                        "Single": {
                            "name": "obj0",
                            "info": {
                                "type_": type_tag(TypeTag::U64),
                                "value": SerializableMoveValue::U64(99)
                            }
                        }
                    }
                ]
            }
        }))));

        let trace = MoveTraceReader::new(Cursor::new(
            builder.into_trace().into_compressed_json_bytes(),
        ))
        .unwrap();
        let roots = call_trace_from_reader_with_context(
            trace,
            TraceCompatibilityContext::new(vec![Some(json!(
                "0000000000000000000000000000000000000000000000000000000000000042"
            ))]),
        )
        .unwrap()
        .unwrap();

        assert_eq!(roots.len(), 1);
        let root = &roots[0];
        assert_eq!(root.from, SUI_FRAMEWORK_ADDRESS.to_string());
        assert_eq!(root.to, SUI_FRAMEWORK_ADDRESS.to_string());
        assert_eq!(root.contract_name, SUI_FRAMEWORK_ADDRESS.to_string());
        assert_eq!(
            root.function_name,
            format!("{}::transfer_objects", SUI_FRAMEWORK_ADDRESS)
        );
        assert_eq!(
            root.inputs,
            vec![
                json!("99"),
                json!("0000000000000000000000000000000000000000000000000000000000000042")
            ]
        );
        assert!(root.return_value.is_empty());
        assert!(root.type_args.is_empty());
        assert!(root.calls.is_empty());
        assert_eq!(root.pc, 0);
        assert_eq!(root.gas_used, 0);
        assert_eq!(root.error, None);
    }

    #[test]
    fn transfer_external_event_flattens_coin_like_old_transfer_trace() {
        let mut builder = MoveTraceBuilder::new();
        builder.push_event(TraceEvent::External(Box::new(json!({
            "ExternalEvent": {
                "description": "TransferObjects: obj0...objN => ()",
                "name": "Transfer",
                "values": [
                    {
                        "Single": {
                            "name": "obj0",
                            "info": {
                                "type_": type_tag(TypeTag::Struct(Box::new(coin_struct_tag()))),
                                "value": coin_value(0x77, 1234)
                            }
                        }
                    }
                ]
            }
        }))));

        let trace = MoveTraceReader::new(Cursor::new(
            builder.into_trace().into_compressed_json_bytes(),
        ))
        .unwrap();
        let roots = call_trace_from_reader_with_context(
            trace,
            TraceCompatibilityContext::new(vec![Some(json!(
                "0000000000000000000000000000000000000000000000000000000000000042"
            ))]),
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            roots[0].inputs,
            vec![
                json!({
                    "type": {
                        "address": "0000000000000000000000000000000000000000000000000000000000000002",
                        "module": "coin",
                        "name": "Coin",
                        "type_args": [{
                            "struct": {
                                "address": "0000000000000000000000000000000000000000000000000000000000000002",
                                "module": "sui",
                                "name": "SUI",
                                "type_args": []
                            }
                        }]
                    },
                    "fields": {
                        "id": "0000000000000000000000000000000000000000000000000000000000000077",
                        "balance": "1234"
                    }
                }),
                json!("0000000000000000000000000000000000000000000000000000000000000042")
            ]
        );
    }

    #[test]
    fn call_trace_serializes_with_old_field_names() {
        let trace = CallTraceWithSource {
            from: "0x1".to_string(),
            to: "0x2".to_string(),
            contract_name: "source".to_string(),
            function_name: "target::call".to_string(),
            inputs: vec![json!("input")],
            return_value: vec![json!("output")],
            type_args: vec!["u64".to_string()],
            calls: vec![],
            location: None,
            pc: 9,
            gas_used: 10,
            error: Some(CallTraceError {
                major_status: "ABORTED".to_string(),
                sub_status: Some(42),
                message: Some("abort message".to_string()),
                location: None,
                function_name: Some("target::call".to_string()),
                code_offset: Some(7),
            }),
        };

        assert_eq!(
            serde_json::to_value(trace).unwrap(),
            json!({
                "from": "0x1",
                "to": "0x2",
                "contractName": "source",
                "functionName": "target::call",
                "inputs": ["input"],
                "returnValue": ["output"],
                "typeArgs": ["u64"],
                "calls": [],
                "pc": 9,
                "gasUsed": 10,
                "error": {
                    "major_status": "ABORTED",
                    "sub_status": 42,
                    "message": "abort message",
                    "location": null,
                    "function_name": "target::call",
                    "code_offset": 7
                }
            })
        );
    }

    fn frame(frame_id: usize, address: &str, module: &str, function_name: &str) -> Frame {
        Frame {
            frame_id,
            function_name: function_name.to_string(),
            module: ModuleId::new(
                AccountAddress::from_hex_literal(address).unwrap(),
                Identifier::new(module).unwrap(),
            ),
            version_id: AccountAddress::from_hex_literal(address).unwrap(),
            binary_member_index: 0,
            type_instantiation: vec![TypeTag::U64],
            parameters: vec![runtime(SerializableMoveValue::U8(1))],
            return_types: vec![type_tag(TypeTag::U64)],
            locals_types: vec![],
            is_native: false,
        }
    }

    fn runtime(value: SerializableMoveValue) -> TraceValue {
        TraceValue::RuntimeValue { value }
    }

    fn count_struct(count: u64) -> SerializableMoveValue {
        SerializableMoveValue::Struct(SimplifiedMoveStruct {
            type_: struct_tag("0x42", "module", "Thing"),
            fields: vec![(
                Identifier::new("count").unwrap(),
                SerializableMoveValue::U64(count),
            )],
        })
    }

    fn vector_struct(values: &[u64]) -> SerializableMoveValue {
        SerializableMoveValue::Struct(SimplifiedMoveStruct {
            type_: struct_tag("0x42", "module", "VectorHolder"),
            fields: vec![(
                Identifier::new("items").unwrap(),
                SerializableMoveValue::Vector(
                    values
                        .iter()
                        .copied()
                        .map(SerializableMoveValue::U64)
                        .collect(),
                ),
            )],
        })
    }

    fn type_tag(type_: TypeTag) -> TypeTagWithRefs {
        TypeTagWithRefs {
            type_,
            ref_type: None,
        }
    }

    fn struct_tag(address: &str, module: &str, name: &str) -> StructTag {
        StructTag {
            address: AccountAddress::from_hex_literal(address).unwrap(),
            module: Identifier::new(module).unwrap(),
            name: Identifier::new(name).unwrap(),
            type_params: vec![],
        }
    }

    fn coin_struct_tag() -> StructTag {
        StructTag {
            address: SUI_FRAMEWORK_ADDRESS,
            module: Identifier::new("coin").unwrap(),
            name: Identifier::new("Coin").unwrap(),
            type_params: vec![TypeTag::Struct(Box::new(StructTag {
                address: SUI_FRAMEWORK_ADDRESS,
                module: Identifier::new("sui").unwrap(),
                name: Identifier::new("SUI").unwrap(),
                type_params: vec![],
            }))],
        }
    }

    fn coin_value(last_id_byte: u8, balance: u64) -> SerializableMoveValue {
        SerializableMoveValue::Struct(SimplifiedMoveStruct {
            type_: coin_struct_tag(),
            fields: vec![
                (
                    Identifier::new("id").unwrap(),
                    SerializableMoveValue::Struct(SimplifiedMoveStruct {
                        type_: struct_tag("0x2", "object", "UID"),
                        fields: vec![(
                            Identifier::new("id").unwrap(),
                            SerializableMoveValue::Struct(SimplifiedMoveStruct {
                                type_: struct_tag("0x2", "object", "ID"),
                                fields: vec![(
                                    Identifier::new("bytes").unwrap(),
                                    SerializableMoveValue::Address(
                                        AccountAddress::from_hex_literal(&format!(
                                            "0x{last_id_byte:02x}"
                                        ))
                                        .unwrap(),
                                    ),
                                )],
                            }),
                        )],
                    }),
                ),
                (
                    Identifier::new("balance").unwrap(),
                    SerializableMoveValue::Struct(SimplifiedMoveStruct {
                        type_: StructTag {
                            address: SUI_FRAMEWORK_ADDRESS,
                            module: Identifier::new("balance").unwrap(),
                            name: Identifier::new("Balance").unwrap(),
                            type_params: vec![TypeTag::Struct(Box::new(struct_tag(
                                "0x2", "sui", "SUI",
                            )))],
                        },
                        fields: vec![(
                            Identifier::new("value").unwrap(),
                            SerializableMoveValue::U64(balance),
                        )],
                    }),
                ),
            ],
        })
    }
}
