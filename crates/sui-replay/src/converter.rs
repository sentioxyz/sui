use anyhow::{anyhow, Result};
use move_core_types::identifier::Identifier;
use serde_json::{Map, Value};
use move_core_types::annotated_value as A;
use move_binary_format::call_trace::InputValue;

pub fn input_value_to_json(val: InputValue, trace_v2: bool) -> Value {
    match val {
        InputValue::MoveValue(mv) => move_value_to_json(mv, trace_v2),
        InputValue::String(s) => Value::String(s)
    }
}

pub fn move_value_to_json(val: A::MoveValue, trace_v2: bool) -> Value {
    match val {
        A::MoveValue::U8(n) => serde_json::to_value(n).unwrap(),
        A::MoveValue::U64(n) => serde_json::to_value(n.to_string()).unwrap(),
        A::MoveValue::U128(n) => serde_json::to_value(n.to_string()).unwrap(),
        A::MoveValue::Bool(b) => serde_json::to_value(b).unwrap(),
        A::MoveValue::Address(add) => serde_json::to_value(add).unwrap(),
        A::MoveValue::Vector(vals) => {
            // If this is a vector<u8>, convert it to hex string
            if is_non_empty_vector_u8(&vals) {
                let bytes = vec_to_vec_u8(vals).unwrap();
                serde_json::to_value(format!("0x{}", hex::encode(&bytes))).unwrap()
            } else {
                Value::Array(vals.into_iter().map(|v| move_value_to_json(v, trace_v2)).collect())
            }
        }
        A::MoveValue::Struct(move_struct) => {
            if !trace_v2 {
                return struct_fields_to_json(move_struct.fields, trace_v2);
            }
            let mut map = Map::new();
            map.insert("type".to_string(), serde_json::to_value(move_struct.type_).unwrap());
            map.insert("fields".to_string(), struct_fields_to_json(move_struct.fields, trace_v2));
            Value::Object(map)
        },
        A::MoveValue::Signer(add) => serde_json::to_value(add).unwrap(),
        A::MoveValue::U16(n) => serde_json::to_value(n).unwrap(),
        A::MoveValue::U32(n) => serde_json::to_value(n).unwrap(),
        A::MoveValue::U256(n) => serde_json::to_value(n.to_string()).unwrap(),
        A::MoveValue::Variant(var) => serde_json::to_value(var.to_string()).unwrap(),
    }
}

fn struct_fields_to_json(fields: Vec<(Identifier, A::MoveValue)>, trace_v2: bool) -> Value {
    let mut iter = fields.into_iter();
    let mut map = Map::new();
    while let Some((field_name, field_value)) = iter.next() {
        map.insert(field_name.into_string(), move_value_to_json(field_value, trace_v2));
    }
    Value::Object(map)
}

fn is_non_empty_vector_u8(vec: &Vec<A::MoveValue>) -> bool {
    if vec.is_empty() {
        false
    } else {
        matches!(vec.last().unwrap(), A::MoveValue::U8(_))
    }
}

/// Converts the `Vec<MoveValue>` to a `Vec<u8>` if the inner `MoveValue` is a `MoveValue::U8`,
/// or returns an error otherwise.
fn vec_to_vec_u8(vec: Vec<A::MoveValue>) -> Result<Vec<u8>> {
    let mut vec_u8 = Vec::with_capacity(vec.len());

    for byte in vec {
        match byte {
            A::MoveValue::U8(u8) => {
                vec_u8.push(u8);
            }
            _ => {
                return Err(anyhow!(
                    "Expected inner MoveValue in Vec<MoveValue> to be a MoveValue::U8".to_string(),
                ));
            }
        }
    }
    Ok(vec_u8)
}
