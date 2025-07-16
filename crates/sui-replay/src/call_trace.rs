use move_binary_format::call_trace::{GasInfo, InternalCallTrace, InternalCallTraceError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sui_types::SUI_FRAMEWORK_ADDRESS;

use crate::converter::{input_value_to_json, move_value_to_json};
use move_binary_format::errors::Location as MoveLoc;
use move_binary_format::file_format::CodeOffset;
use move_core_types::language_storage::ModuleId;

/// A call trace with source
///
/// This is a representation of the debug call trace
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CallTraceError {
    pub major_status: String,
    pub sub_status: Option<u64>,
    pub message: Option<String>,
    pub location: Option<ModuleId>,
    pub function_name: Option<String>,
    pub code_offset: Option<CodeOffset>,
}

impl CallTraceWithSource {
    pub fn from(call_trace: InternalCallTrace, trace_v2: bool) -> Self {
        let mut split_module = call_trace.from_module_id.split("::");
        let account = split_module.next();
        let module_name = split_module.next();
        let mut split_to_module = call_trace.module_id.split("::");
        let to_account = split_to_module.next();
        let to_module_name = split_to_module.next();
        CallTraceWithSource {
            from: account.unwrap().to_string(),
            contract_name: module_name.unwrap_or(SUI_FRAMEWORK_ADDRESS.to_string().as_str()).to_string(),
            to: to_account.unwrap().to_string(),
            function_name: format!(
                "{}::{}",
                to_module_name.unwrap_or(SUI_FRAMEWORK_ADDRESS.to_string().as_str()).to_string(),
                call_trace.func_name
            ),
            inputs: call_trace
                .inputs
                .clone()
                .into_iter()
                .map(|i| i.map_or(serde_json::to_value("?").unwrap(), |i| input_value_to_json(i, trace_v2)))
                .collect::<Vec<Value>>(),
            return_value: call_trace
                .outputs
                .clone()
                .into_iter()
                .map(|i| i.map_or(serde_json::to_value("?").unwrap(), |i| move_value_to_json(i, trace_v2)))
                .collect::<Vec<Value>>(),
            type_args: call_trace.type_args.clone(),
            calls: call_trace
                .sub_traces
                .clone()
                .0
                .into_iter()
                .map(|sub_trace| CallTraceWithSource::from(sub_trace, trace_v2))
                .collect(),
            location: None,
            pc: call_trace.pc,
            gas_used: call_trace.gas_info.gas_used(),
            error: {
                if let Some(InternalCallTraceError{vm_error, function_name, code_offset}) = call_trace.error {
                    Some(CallTraceError {
                        major_status: vm_error.major_status().to_string(),
                        sub_status: vm_error.sub_status(),
                        message: vm_error.message().cloned(),
                        location: if let MoveLoc::Module(module_id) = vm_error.location() {
                            Some(module_id.clone())
                        } else {
                            None
                        },
                        function_name,
                        code_offset,
                    })
                } else {
                    None
                }
            },
        }
    }
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
