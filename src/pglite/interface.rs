use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::protocol::messages::{BackendMessage, NoticeMessage};

/// Row output mode matching the TypeScript `RowMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowMode {
    Object,
    Array,
}

/// Debug logging level used by the runtime. Matches the TypeScript enum values.
pub type DebugLevel = u8;

/// Parser function used to convert textual Postgres values into richer Rust values.
/// Mirrors the signature of the TypeScript parser callbacks.
pub type TypeParser = Arc<dyn Fn(&str, i32) -> Value + Send + Sync>;
pub type Serializer = Arc<dyn Fn(&Value) -> anyhow::Result<String> + Send + Sync>;
pub type NoticeCallback = Arc<dyn Fn(&NoticeMessage) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataTransferContainer {
    Cma,
    File,
}

pub type ParserMap = HashMap<i32, TypeParser>;
pub type SerializerMap = HashMap<i32, Serializer>;

#[derive(Default, Clone)]
pub struct QueryOptions {
    pub row_mode: Option<RowMode>,
    pub parsers: ParserMap,
    pub serializers: SerializerMap,
    pub blob: Option<Vec<u8>>,
    pub param_types: Vec<i32>,
    pub on_notice: Option<NoticeCallback>,
    pub data_transfer_container: Option<DataTransferContainer>,
}

#[derive(Debug, Clone)]
pub struct FieldInfo {
    pub name: String,
    pub data_type_id: i32,
}

#[derive(Debug, Clone)]
pub struct Results {
    pub rows: Vec<Value>,
    pub fields: Vec<FieldInfo>,
    pub affected_rows: Option<usize>,
    pub blob: Option<Vec<u8>>,
}

#[derive(Clone)]
pub struct ExecProtocolOptions {
    pub sync_to_fs: bool,
    pub throw_on_error: bool,
    pub on_notice: Option<NoticeCallback>,
    pub data_transfer_container: Option<DataTransferContainer>,
}

impl ExecProtocolOptions {
    pub const fn no_sync() -> Self {
        Self {
            sync_to_fs: false,
            throw_on_error: true,
            on_notice: None,
            data_transfer_container: None,
        }
    }
}

impl Default for ExecProtocolOptions {
    fn default() -> Self {
        Self {
            sync_to_fs: true,
            throw_on_error: true,
            on_notice: None,
            data_transfer_container: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExecProtocolResult {
    pub messages: Vec<BackendMessage>,
}

#[derive(Clone)]
pub struct DescribeQueryParam {
    pub data_type_id: i32,
    pub serializer: Option<Serializer>,
}

#[derive(Clone)]
pub struct DescribeResultField {
    pub name: String,
    pub data_type_id: i32,
    pub parser: Option<TypeParser>,
}

#[derive(Clone)]
pub struct DescribeQueryResult {
    pub query_params: Vec<DescribeQueryParam>,
    pub result_fields: Vec<DescribeResultField>,
}
