use std::borrow::Cow;

use tracing::warn;

use crate::protocol::buffer_writer::BufferWriter;
use crate::protocol::string_utils::byte_length_utf8;

const CODE_STARTUP: u8 = b'p';
const CODE_QUERY: u8 = b'Q';
const CODE_PARSE: u8 = b'P';
const CODE_BIND: u8 = b'B';
const CODE_EXECUTE: u8 = b'E';
const CODE_FLUSH: u8 = b'H';
const CODE_SYNC: u8 = b'S';
const CODE_END: u8 = b'X';
const CODE_CLOSE: u8 = b'C';
const CODE_DESCRIBE: u8 = b'D';
const CODE_COPY_DATA: u8 = b'd';
const CODE_COPY_DONE: u8 = b'c';
const CODE_COPY_FAIL: u8 = b'f';

#[derive(Debug, Clone)]
pub enum BindValue {
    Null,
    Text(String),
    Binary(Vec<u8>),
}

impl From<Option<&str>> for BindValue {
    fn from(value: Option<&str>) -> Self {
        match value {
            Some(text) => BindValue::Text(text.to_owned()),
            None => BindValue::Null,
        }
    }
}

impl From<Option<String>> for BindValue {
    fn from(value: Option<String>) -> Self {
        match value {
            Some(text) => BindValue::Text(text),
            None => BindValue::Null,
        }
    }
}

impl From<&str> for BindValue {
    fn from(value: &str) -> Self {
        BindValue::Text(value.to_owned())
    }
}

impl From<String> for BindValue {
    fn from(value: String) -> Self {
        BindValue::Text(value)
    }
}

impl From<&[u8]> for BindValue {
    fn from(value: &[u8]) -> Self {
        BindValue::Binary(value.to_vec())
    }
}

impl From<Vec<u8>> for BindValue {
    fn from(value: Vec<u8>) -> Self {
        BindValue::Binary(value)
    }
}

pub type ValueMapper = Box<dyn Fn(&BindValue, usize) -> BindValue + Send + Sync>;

#[derive(Default)]
pub struct BindConfig {
    pub portal: Option<String>,
    pub statement: Option<String>,
    pub binary: bool,
    pub values: Vec<BindValue>,
    pub value_mapper: Option<ValueMapper>,
}

#[derive(Debug, Clone, Default)]
pub struct ExecConfig {
    pub portal: Option<String>,
    pub rows: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct PortalTarget {
    pub target_type: char,
    pub name: Option<String>,
}

impl PortalTarget {
    pub fn new(target_type: char, name: Option<String>) -> Self {
        Self { target_type, name }
    }
}

pub struct Serialize;

impl Serialize {
    pub fn startup<I, K, V>(options: I) -> Vec<u8>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut writer = BufferWriter::default();
        writer.add_int16(3).add_int16(0);

        for (key, value) in options {
            writer.add_cstring(key.as_ref()).add_cstring(value.as_ref());
        }

        writer
            .add_cstring("client_encoding")
            .add_cstring("UTF8")
            .add_cstring("");

        let body = writer.flush(None);
        let length = (body.len() + 4) as i32;
        let mut result = Vec::with_capacity(body.len() + 4);
        result.extend_from_slice(&length.to_be_bytes());
        result.extend_from_slice(&body);
        result
    }

    pub fn request_ssl() -> Vec<u8> {
        let mut buffer = [0u8; 8];
        buffer[..4].copy_from_slice(&8_i32.to_be_bytes());
        buffer[4..8].copy_from_slice(&80877103_i32.to_be_bytes());
        buffer.to_vec()
    }

    pub fn password(password: &str) -> Vec<u8> {
        let mut writer = BufferWriter::default();
        writer.add_cstring(password);
        writer.flush(Some(CODE_STARTUP))
    }

    pub fn send_sasl_initial_response_message(mechanism: &str, initial_response: &str) -> Vec<u8> {
        let mut writer = BufferWriter::default();
        writer
            .add_cstring(mechanism)
            .add_int32(byte_length_utf8(initial_response) as i32)
            .add_string(initial_response);
        writer.flush(Some(CODE_STARTUP))
    }

    pub fn send_scram_client_final_message(additional_data: &str) -> Vec<u8> {
        let mut writer = BufferWriter::default();
        writer.add_string(additional_data);
        writer.flush(Some(CODE_STARTUP))
    }

    pub fn query(text: &str) -> Vec<u8> {
        let mut writer = BufferWriter::default();
        writer.add_cstring(text);
        writer.flush(Some(CODE_QUERY))
    }

    pub fn parse(name: Option<&str>, text: &str, types: &[i32]) -> Vec<u8> {
        if let Some(name) = name
            && name.len() > 63
        {
            warn!(
                "Postgres only supports 63 characters for query names. You supplied {len}",
                len = name.len()
            );
        }

        let mut writer = BufferWriter::default();
        writer
            .add_cstring(name.unwrap_or(""))
            .add_cstring(text)
            .add_int16(types.len() as i16);

        for oid in types {
            writer.add_int32(*oid);
        }

        writer.flush(Some(CODE_PARSE))
    }

    pub fn bind(config: &BindConfig) -> Vec<u8> {
        let mut writer = BufferWriter::default();
        let mut param_writer = BufferWriter::default();

        let portal = config.portal.as_deref().unwrap_or("");
        let statement = config.statement.as_deref().unwrap_or("");
        let values = &config.values;
        let len = values.len() as i16;

        writer.add_cstring(portal).add_cstring(statement);
        writer.add_int16(len);

        for (idx, value) in values.iter().enumerate() {
            let mapped = if let Some(mapper) = &config.value_mapper {
                mapper(value, idx)
            } else {
                value.clone()
            };

            match mapped {
                BindValue::Null => {
                    writer.add_int16(0);
                    param_writer.add_int32(-1);
                }
                BindValue::Binary(bytes) => {
                    writer.add_int16(1);
                    param_writer.add_int32(bytes.len() as i32);
                    param_writer.add_bytes(&bytes);
                }
                BindValue::Text(text) => {
                    writer.add_int16(0);
                    param_writer.add_int32(byte_length_utf8(&text) as i32);
                    param_writer.add_string(&text);
                }
            }
        }

        writer.add_int16(len);
        let param_body = param_writer.flush(None);
        writer.add_bytes(&param_body);
        writer.add_int16(if config.binary { 1 } else { 0 });
        writer.flush(Some(CODE_BIND))
    }

    pub fn execute(config: Option<&ExecConfig>) -> Vec<u8> {
        let Some(cfg) = config else {
            return vec![CODE_EXECUTE, 0, 0, 0, 9, 0, 0, 0, 0, 0];
        };

        if cfg.portal.as_ref().is_none_or(|p| p.is_empty()) && cfg.rows.unwrap_or(0) == 0 {
            return vec![CODE_EXECUTE, 0, 0, 0, 9, 0, 0, 0, 0, 0];
        }

        let portal = cfg.portal.as_deref().unwrap_or("");
        let rows = cfg.rows.unwrap_or(0);

        let portal_length = byte_length_utf8(portal);
        let len = 4 + portal_length + 1 + 4;
        let mut buffer = vec![0u8; 1 + len];
        buffer[0] = CODE_EXECUTE;
        let len_bytes = (len as i32).to_be_bytes();
        buffer[1..5].copy_from_slice(&len_bytes);
        buffer[5..5 + portal_length].copy_from_slice(portal.as_bytes());
        buffer[5 + portal_length] = 0;
        let row_bytes = rows.to_be_bytes();
        let end = buffer.len();
        buffer[end - 4..].copy_from_slice(&row_bytes);
        buffer
    }

    pub fn describe(target: &PortalTarget) -> Vec<u8> {
        let mut writer = BufferWriter::default();
        if let Some(name) = &target.name {
            let mut text = String::with_capacity(1 + name.len());
            text.push(target.target_type);
            text.push_str(name);
            writer.add_cstring(&text);
        } else {
            let mut value = String::with_capacity(2);
            value.push(target.target_type);
            writer.add_cstring(&value);
        }
        writer.flush(Some(CODE_DESCRIBE))
    }

    pub fn close(target: &PortalTarget) -> Vec<u8> {
        let mut text = String::with_capacity(target.name.as_ref().map_or(1, |s| 1 + s.len()));
        text.push(target.target_type);
        if let Some(name) = &target.name {
            text.push_str(name);
        }
        let mut writer = BufferWriter::default();
        writer.add_cstring(&text);
        writer.flush(Some(CODE_CLOSE))
    }

    pub fn flush() -> Vec<u8> {
        code_only_buffer(CODE_FLUSH)
    }

    pub fn sync() -> Vec<u8> {
        code_only_buffer(CODE_SYNC)
    }

    pub fn end() -> Vec<u8> {
        code_only_buffer(CODE_END)
    }

    pub fn copy_data(chunk: &[u8]) -> Vec<u8> {
        let mut writer = BufferWriter::default();
        writer.add_bytes(chunk);
        writer.flush(Some(CODE_COPY_DATA))
    }

    pub fn copy_done() -> Vec<u8> {
        code_only_buffer(CODE_COPY_DONE)
    }

    pub fn copy_fail(message: &str) -> Vec<u8> {
        let mut writer = BufferWriter::default();
        writer.add_cstring(message);
        writer.flush(Some(CODE_COPY_FAIL))
    }

    pub fn cancel(process_id: i32, secret_key: i32) -> Vec<u8> {
        let mut buffer = vec![0u8; 16];
        buffer[..4].copy_from_slice(&16i32.to_be_bytes());
        let code1 = 1234i16.to_be_bytes();
        let code2 = 5678i16.to_be_bytes();
        buffer[4..6].copy_from_slice(&code1);
        buffer[6..8].copy_from_slice(&code2);
        buffer[8..12].copy_from_slice(&process_id.to_be_bytes());
        buffer[12..].copy_from_slice(&secret_key.to_be_bytes());
        buffer
    }
}

fn code_only_buffer(code: u8) -> Vec<u8> {
    let mut buf = vec![0u8; 5];
    buf[0] = code;
    buf[1..5].copy_from_slice(&(4i32).to_be_bytes());
    buf
}

pub trait SerializeExt {
    fn serialize(&self) -> Vec<u8>;
}

pub trait SerializeBytes {
    fn serialize_bytes(&self) -> Cow<'_, [u8]>;
}
