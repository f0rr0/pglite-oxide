use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::LazyLock;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use super::interface::{ParserMap, Serializer, SerializerMap, TypeParser};

macro_rules! const_oid {
    ($name:ident = $value:expr) => {
        pub const $name: i32 = $value;
    };
}

const_oid!(BOOL = 16);
const_oid!(BYTEA = 17);
const_oid!(CHAR = 18);
const_oid!(INT8 = 20);
const_oid!(INT2 = 21);
const_oid!(INT4 = 23);
const_oid!(TEXT = 25);
const_oid!(OID = 26);
const_oid!(JSON = 114);
const_oid!(FLOAT4 = 700);
const_oid!(FLOAT8 = 701);
const_oid!(DATE = 1082);
const_oid!(TIMESTAMP = 1114);
const_oid!(TIMESTAMPTZ = 1184);
const_oid!(NUMERIC = 1700);
const_oid!(UUID = 2950);
const_oid!(JSONB = 3802);

pub static DEFAULT_PARSERS: LazyLock<ParserMap> = LazyLock::new(build_default_parsers);
pub static DEFAULT_SERIALIZERS: LazyLock<SerializerMap> = LazyLock::new(build_default_serializers);

pub struct ParserLookup<'a> {
    defaults: &'a ParserMap,
    overrides: &'a ParserMap,
}

impl<'a> ParserLookup<'a> {
    pub fn new(defaults: &'a ParserMap, overrides: &'a ParserMap) -> Self {
        Self {
            defaults,
            overrides,
        }
    }

    pub fn apply(&self, text: &str, type_id: i32) -> Value {
        let parser = self
            .overrides
            .get(&type_id)
            .or_else(|| self.defaults.get(&type_id));
        if let Some(parser) = parser {
            parser(text, type_id)
        } else {
            json!(text)
        }
    }
}

fn array_delimiter(typarray: i32) -> char {
    if typarray == 1020 { ';' } else { ',' }
}

pub fn serialize_array_value(
    value: &Value,
    element_serializer: Option<Serializer>,
    typarray: i32,
) -> Result<String> {
    match value {
        Value::Array(items) => {
            if items.is_empty() {
                return Ok("{}".to_string());
            }

            let delimiter = array_delimiter(typarray);
            let mut parts = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::Null => parts.push("null".to_string()),
                    Value::Array(_) => {
                        parts.push(serialize_array_value(
                            item,
                            element_serializer.clone(),
                            typarray,
                        )?);
                    }
                    _ => {
                        let raw = if let Some(serializer) = element_serializer.as_ref() {
                            serializer(item)?
                        } else {
                            value_to_string(item)
                        };
                        let escaped = raw.replace('\\', "\\\\").replace('"', "\\\"");
                        parts.push(format!("\"{}\"", escaped));
                    }
                }
            }
            let joined = parts.join(&delimiter.to_string());
            Ok(format!("{{{}}}", joined))
        }
        Value::Null => Ok("null".to_string()),
        _ => Ok(value_to_string(value)),
    }
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => {
            if *b {
                "t".to_string()
            } else {
                "f".to_string()
            }
        }
        Value::Null => "null".to_string(),
        Value::Array(_) => value.to_string(),
        _ => value.to_string(),
    }
}

#[derive(Default)]
struct ArrayParserState {
    index: usize,
    last: usize,
    quoted: bool,
    buffer: String,
    prev: Option<char>,
}

pub fn parse_array_text(
    text: &str,
    element_parser: Option<TypeParser>,
    element_type_id: i32,
    typarray: i32,
) -> Value {
    let mut state = ArrayParserState::default();
    let result = parse_array_loop(
        text,
        &mut state,
        element_parser.as_ref(),
        element_type_id,
        typarray,
    );
    match result {
        Value::Array(outer) => {
            if let Some(Value::Array(inner)) = outer.into_iter().next() {
                Value::Array(inner)
            } else {
                Value::Array(Vec::new())
            }
        }
        _ => Value::Array(Vec::new()),
    }
}

fn parse_array_loop(
    text: &str,
    state: &mut ArrayParserState,
    element_parser: Option<&TypeParser>,
    element_type_id: i32,
    typarray: i32,
) -> Value {
    let delimiter = array_delimiter(typarray);
    let bytes = text.as_bytes();
    let mut values: Vec<Value> = Vec::new();

    while state.index < bytes.len() {
        let ch = bytes[state.index] as char;
        if state.quoted {
            if ch == '\\' {
                state.index += 1;
                if state.index < bytes.len() {
                    state.buffer.push(bytes[state.index] as char);
                }
            } else if ch == '"' {
                let value = apply_element_parser(&state.buffer, element_parser, element_type_id);
                values.push(value);
                state.buffer.clear();
                if state.index + 1 < bytes.len() && bytes[state.index + 1] as char == '"' {
                    state.index += 1;
                    state.quoted = true;
                } else {
                    state.quoted = false;
                }
                state.last = state.index + 1;
            } else {
                state.buffer.push(ch);
            }
        } else if ch == '"' {
            state.quoted = true;
            state.buffer.clear();
            state.last = state.index + 1;
        } else if ch == '{' {
            state.last = state.index + 1;
            state.index += 1;
            values.push(parse_array_loop(
                text,
                state,
                element_parser,
                element_type_id,
                typarray,
            ));
        } else if ch == '}' {
            state.quoted = false;
            if state.last < state.index && state.prev != Some('}') && state.prev != Some('"') {
                let slice = &text[state.last..state.index];
                if !slice.is_empty() {
                    values.push(apply_element_parser(slice, element_parser, element_type_id));
                }
            }
            state.last = state.index + 1;
            break;
        } else if ch == delimiter && state.prev != Some('}') && state.prev != Some('"') {
            let slice = &text[state.last..state.index];
            values.push(apply_element_parser(slice, element_parser, element_type_id));
            state.last = state.index + 1;
        }
        state.prev = Some(ch);
        state.index += 1;
    }

    if state.last < state.index {
        let slice = &text[state.last..state.index];
        if !slice.is_empty() {
            values.push(apply_element_parser(slice, element_parser, element_type_id));
        }
    }

    Value::Array(values)
}

fn apply_element_parser(slice: &str, parser: Option<&TypeParser>, element_type_id: i32) -> Value {
    if let Some(p) = parser {
        p(slice, element_type_id)
    } else if slice.eq_ignore_ascii_case("NULL") {
        Value::Null
    } else {
        Value::String(slice.to_string())
    }
}

fn build_default_parsers() -> ParserMap {
    let mut map: ParserMap = HashMap::new();

    map.insert(
        TEXT,
        Arc::new(|value: &str, _| json!(value.to_string())) as TypeParser,
    );
    map.insert(CHAR, Arc::new(|value: &str, _| json!(value.to_string())));

    map.insert(INT2, Arc::new(|value: &str, _| parse_int(value)));
    map.insert(INT4, Arc::new(|value: &str, _| parse_int(value)));
    map.insert(INT8, Arc::new(|value: &str, _| parse_bigint(value)));
    map.insert(OID, Arc::new(|value: &str, _| parse_int(value)));
    map.insert(NUMERIC, Arc::new(|value: &str, _| parse_numeric(value)));

    map.insert(FLOAT4, Arc::new(|value: &str, _| parse_float(value)));
    map.insert(FLOAT8, Arc::new(|value: &str, _| parse_float(value)));

    map.insert(BOOL, Arc::new(|value: &str, _| json!(value == "t")));

    map.insert(JSON, Arc::new(|value: &str, _| parse_json(value)));
    map.insert(JSONB, Arc::new(|value: &str, _| parse_json(value)));

    map.insert(BYTEA, Arc::new(|value: &str, _| parse_bytea(value)));

    map.insert(UUID, Arc::new(|value: &str, _| json!(value.to_string())));

    map.insert(
        TIMESTAMP,
        Arc::new(|value: &str, _| json!(value.to_string())),
    );
    map.insert(
        TIMESTAMPTZ,
        Arc::new(|value: &str, _| json!(value.to_string())),
    );
    map.insert(DATE, Arc::new(|value: &str, _| json!(value.to_string())));

    map
}

fn build_default_serializers() -> SerializerMap {
    let mut map: SerializerMap = HashMap::new();

    map.insert(
        TEXT,
        Arc::new(|value: &Value| serialize_string(value)) as Serializer,
    );
    map.insert(CHAR, Arc::new(|value: &Value| serialize_string(value)));

    map.insert(INT2, Arc::new(|value: &Value| serialize_number(value)));
    map.insert(INT4, Arc::new(|value: &Value| serialize_number(value)));
    map.insert(INT8, Arc::new(|value: &Value| serialize_number(value)));
    map.insert(OID, Arc::new(|value: &Value| serialize_number(value)));
    map.insert(NUMERIC, Arc::new(|value: &Value| serialize_number(value)));
    map.insert(FLOAT4, Arc::new(|value: &Value| serialize_number(value)));
    map.insert(FLOAT8, Arc::new(|value: &Value| serialize_number(value)));

    map.insert(BOOL, Arc::new(|value: &Value| serialize_bool(value)));
    map.insert(JSON, Arc::new(|value: &Value| serialize_json(value)));
    map.insert(JSONB, Arc::new(|value: &Value| serialize_json(value)));
    map.insert(BYTEA, Arc::new(|value: &Value| serialize_bytea(value)));
    map.insert(UUID, Arc::new(|value: &Value| serialize_string(value)));
    map.insert(TIMESTAMP, Arc::new(|value: &Value| serialize_string(value)));
    map.insert(
        TIMESTAMPTZ,
        Arc::new(|value: &Value| serialize_string(value)),
    );
    map.insert(DATE, Arc::new(|value: &Value| serialize_string(value)));

    map
}

fn parse_int(value: &str) -> Value {
    match value.parse::<i64>() {
        Ok(int) => json!(int),
        Err(_) => json!(value.to_string()),
    }
}

fn parse_bigint(value: &str) -> Value {
    match value.parse::<i128>() {
        Ok(int) => json!(int),
        Err(_) => json!(value.to_string()),
    }
}

fn parse_numeric(value: &str) -> Value {
    serde_json::Number::from_str(value)
        .map(Value::Number)
        .unwrap_or_else(|_| json!(value.to_string()))
}

fn parse_float(value: &str) -> Value {
    match value.parse::<f64>() {
        Ok(float) => json!(float),
        Err(_) => json!(value.to_string()),
    }
}

fn parse_json(value: &str) -> Value {
    serde_json::from_str(value).unwrap_or_else(|_| json!(value.to_string()))
}

fn parse_bytea(value: &str) -> Value {
    value
        .strip_prefix("\\x")
        .and_then(|hex| hex::decode(hex).ok())
        .map(Value::from)
        .unwrap_or_else(|| json!(value.to_string()))
}

fn serialize_string(value: &Value) -> Result<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        other => Ok(other.to_string()),
    }
}

fn serialize_number(value: &Value) -> Result<String> {
    match value {
        Value::Number(num) => Ok(num.to_string()),
        Value::String(s) => Ok(s.clone()),
        other => Err(anyhow!("cannot serialize value {other} as number")),
    }
}

fn serialize_bool(value: &Value) -> Result<String> {
    match value {
        Value::Bool(b) => Ok(if *b { "t" } else { "f" }.to_string()),
        Value::Number(num) => Ok(if num.as_i64().unwrap_or(0) != 0 {
            "t"
        } else {
            "f"
        }
        .to_string()),
        Value::String(s) => Ok(match s.as_ref() {
            "true" | "t" | "1" => "t".to_string(),
            _ => "f".to_string(),
        }),
        other => Err(anyhow!("cannot serialize value {other} as boolean")),
    }
}

fn serialize_json(value: &Value) -> Result<String> {
    if let Some(value) = value.as_str() {
        Ok(value.to_string())
    } else {
        serde_json::to_string(value).map_err(|err| anyhow!(err))
    }
}

fn serialize_bytea(value: &Value) -> Result<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Array(arr) => {
            let bytes: Vec<u8> = arr
                .iter()
                .filter_map(|v| v.as_u64().map(|n| n as u8))
                .collect();
            Ok(format!("\\x{}", hex::encode(bytes)))
        }
        Value::Null => Ok("\\x".to_string()),
        _ => Err(anyhow!("unsupported value for bytea serialization")),
    }
}
