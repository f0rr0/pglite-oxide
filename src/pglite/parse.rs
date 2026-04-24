use std::collections::HashMap;

use serde_json::Value;

use crate::pglite::interface::{FieldInfo, ParserMap, QueryOptions, Results, RowMode};
use crate::pglite::types::ParserLookup;
use crate::protocol::messages::{
    BackendMessage, CommandCompleteMessage, DataRowMessage, RowDescriptionMessage,
};

pub fn parse_results(
    messages: &[BackendMessage],
    default_parsers: &ParserMap,
    options: Option<&QueryOptions>,
    blob: Option<Vec<u8>>,
) -> Vec<Results> {
    let mut result_sets: Vec<Results> = Vec::new();
    let mut current_fields: Vec<FieldInfo> = Vec::new();
    let mut current_rows: Vec<Value> = Vec::new();
    let mut affected_rows = 0usize;

    let empty_parsers = HashMap::new();
    let (row_mode, parsers_override) = options
        .map(|opts| (opts.row_mode, &opts.parsers))
        .unwrap_or((None, &empty_parsers));

    let parser_lookup = ParserLookup::new(default_parsers, parsers_override);

    for message in messages {
        match message {
            BackendMessage::RowDescription(desc) => {
                current_fields = map_fields(desc);
            }
            BackendMessage::DataRow(row) => {
                if current_fields.is_empty() {
                    continue;
                }
                let row_value = map_row(row, &current_fields, &parser_lookup, row_mode);
                current_rows.push(row_value);
            }
            BackendMessage::CommandComplete(cmd) => {
                affected_rows += retrieve_row_count(cmd);
                result_sets.push(Results {
                    rows: std::mem::take(&mut current_rows),
                    fields: current_fields.clone(),
                    affected_rows: Some(affected_rows),
                    blob: blob.clone(),
                });
                current_fields.clear();
            }
            _ => {}
        }
    }

    if result_sets.is_empty() {
        result_sets.push(Results {
            rows: Vec::new(),
            fields: Vec::new(),
            affected_rows: Some(0),
            blob,
        })
    }

    result_sets
}

pub fn parse_describe_statement_results(messages: &[BackendMessage]) -> Vec<i32> {
    messages
        .iter()
        .find_map(|msg| match msg {
            BackendMessage::ParameterDescription(desc) => Some(desc.data_type_ids.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn map_fields(desc: &RowDescriptionMessage) -> Vec<FieldInfo> {
    desc.fields
        .iter()
        .map(|field| FieldInfo {
            name: field.name.clone(),
            data_type_id: field.data_type_id,
        })
        .collect()
}

fn map_row(
    row: &DataRowMessage,
    fields: &[FieldInfo],
    parsers: &ParserLookup,
    row_mode: Option<RowMode>,
) -> Value {
    match row_mode {
        Some(RowMode::Array) => {
            let values: Vec<Value> = row
                .fields
                .iter()
                .zip(fields.iter())
                .map(|(value, field)| parse_cell(value.as_deref(), field.data_type_id, parsers))
                .collect();
            Value::Array(values)
        }
        _ => {
            let mut map = serde_json::Map::with_capacity(fields.len());
            for (value, field) in row.fields.iter().zip(fields.iter()) {
                let parsed = parse_cell(value.as_deref(), field.data_type_id, parsers);
                map.insert(field.name.clone(), parsed);
            }
            Value::Object(map)
        }
    }
}

fn parse_cell(value: Option<&str>, type_id: i32, parsers: &ParserLookup) -> Value {
    match value {
        None => Value::Null,
        Some(text) => parsers.apply(text, type_id),
    }
}

fn retrieve_row_count(msg: &CommandCompleteMessage) -> usize {
    let parts: Vec<&str> = msg.text.split(' ').collect();
    match parts.first().copied() {
        Some("INSERT") => parts.get(2).and_then(|v| v.parse().ok()).unwrap_or(0),
        Some("UPDATE") | Some("DELETE") | Some("COPY") | Some("MERGE") => {
            parts.get(1).and_then(|v| v.parse().ok()).unwrap_or(0)
        }
        _ => 0,
    }
}
