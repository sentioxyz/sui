use move_binary_format::call_trace::InternalCallTrace;
use move_binary_format::errors::VMError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sui_types::SUI_FRAMEWORK_ADDRESS;

use crate::converter::{input_value_to_json, move_value_to_json};

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<CallTraceError>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CallTraceError {
    pub major_status: move_core_types::vm_status::StatusCode,
    pub sub_status: Option<u64>,
    pub message: Option<String>,
    pub location: Option<Location>,
}

impl CallTraceWithSource {
    pub fn default() -> Self {
        CallTraceWithSource {
            from: "".to_string(),
            to: "".to_string(),
            contract_name: "".to_string(),
            function_name: "".to_string(),
            inputs: vec![],
            return_value: vec![],
            type_args: vec![],
            calls: vec![],
            location: None,
            pc: 0,
            error: None,
        }
    }

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
                .map(|i| input_value_to_json(i, trace_v2))
                .collect::<Vec<Value>>(),
            return_value: call_trace
                .outputs
                .clone()
                .into_iter()
                .map(|i| move_value_to_json(i, trace_v2))
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
            error: {
                if let Some(vm_error) = call_trace.error {
                    Some(CallTraceError {
                        major_status: vm_error.major_status(),
                        sub_status: vm_error.sub_status(),
                        message: vm_error.message().cloned(),
                        location: None, // TODO
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
