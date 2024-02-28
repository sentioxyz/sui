use anyhow::{anyhow, Result};
use move_core_types::{identifier::Identifier, runtime_value::MoveValue};
use serde_json::{Map, Value};

pub fn move_value_to_json(val: MoveValue) -> Value {
    match val {
        MoveValue::U8(n) => serde_json::to_value(n).unwrap(),
        MoveValue::U64(n) => serde_json::to_value(n.to_string()).unwrap(),
        MoveValue::U128(n) => serde_json::to_value(n.to_string()).unwrap(),
        MoveValue::Bool(b) => serde_json::to_value(b).unwrap(),
        MoveValue::Address(add) => serde_json::to_value(add).unwrap(),
        MoveValue::Vector(vals) => {
            // If this is a vector<u8>, convert it to hex string
            if is_non_empty_vector_u8(&vals) {
                let bytes = vec_to_vec_u8(vals).unwrap();
                serde_json::to_value(format!("0x{}", hex::encode(&bytes))).unwrap()
            } else {
                Value::Array(vals.into_iter().map(|v| move_value_to_json(v)).collect())
            }
        }
        MoveValue::Struct(move_struct) => Value::Array(
            move_struct
                .0
                .into_iter()
                .map(|v| move_value_to_json(v))
                .collect(),
        ),
        MoveValue::Signer(add) => serde_json::to_value(add).unwrap(),
        MoveValue::U16(n) => serde_json::to_value(n).unwrap(),
        MoveValue::U32(n) => serde_json::to_value(n).unwrap(),
        MoveValue::U256(n) => serde_json::to_value(n.to_string()).unwrap(),
    }
}

fn struct_fields_to_json(fields: Vec<(Identifier, MoveValue)>) -> Value {
    let mut iter = fields.into_iter();
    let mut map = Map::new();
    while let Some((field_name, field_value)) = iter.next() {
        map.insert(field_name.into_string(), move_value_to_json(field_value));
    }
    Value::Object(map)
}

fn is_non_empty_vector_u8(vec: &Vec<MoveValue>) -> bool {
    if vec.is_empty() {
        false
    } else {
        matches!(vec.last().unwrap(), MoveValue::U8(_))
    }
}

/// Converts the `Vec<MoveValue>` to a `Vec<u8>` if the inner `MoveValue` is a `MoveValue::U8`,
/// or returns an error otherwise.
fn vec_to_vec_u8(vec: Vec<MoveValue>) -> Result<Vec<u8>> {
    let mut vec_u8 = Vec::with_capacity(vec.len());

    for byte in vec {
        match byte {
            MoveValue::U8(u8) => {
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
