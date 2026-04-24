use super::serializer::BindValue;
use super::{
    AuthenticationMessage, BackendMessage, BindConfig, ExecConfig, Field, MessageName, Mode,
    Parser, PortalTarget, Serialize, byte_length_utf8,
};
use anyhow::Result;

mod helpers {
    #[derive(Debug, Default, Clone)]
    pub struct BufferList {
        buffers: Vec<Vec<u8>>,
    }

    impl BufferList {
        pub fn new() -> Self {
            Self {
                buffers: Vec::new(),
            }
        }

        pub fn add_bytes(&mut self, bytes: &[u8]) -> &mut Self {
            self.buffers.push(bytes.to_vec());
            self
        }

        pub fn add_int16(&mut self, value: i16) -> &mut Self {
            self.buffers.push(value.to_be_bytes().to_vec());
            self
        }

        pub fn add_int32(&mut self, value: i32) -> &mut Self {
            self.buffers.push(value.to_be_bytes().to_vec());
            self
        }

        pub fn add_cstring(&mut self, value: &str) -> &mut Self {
            let mut bytes = value.as_bytes().to_vec();
            bytes.push(0);
            self.buffers.push(bytes);
            self
        }

        pub fn add_string(&mut self, value: &str) -> &mut Self {
            self.buffers.push(value.as_bytes().to_vec());
            self
        }

        pub fn add_char(&mut self, ch: char) -> &mut Self {
            assert!(
                ch.is_ascii(),
                "non-ascii char {ch:?} not supported in these tests"
            );
            self.buffers.push(vec![ch as u8]);
            self
        }

        pub fn add_byte(&mut self, byte: u8) -> &mut Self {
            self.buffers.push(vec![byte]);
            self
        }

        pub fn join(&self, append_length: bool, code: Option<u8>) -> Vec<u8> {
            let body_len: usize = self.buffers.iter().map(|b| b.len()).sum();
            let mut result = Vec::with_capacity(
                body_len + usize::from(append_length) * 4 + usize::from(code.is_some()),
            );

            if let Some(code) = code {
                result.push(code);
            }

            if append_length {
                let length = (body_len + 4) as i32;
                result.extend_from_slice(&length.to_be_bytes());
            }

            for part in &self.buffers {
                result.extend_from_slice(part);
            }

            result
        }
    }

    pub fn concat_slices(parts: &[&[u8]]) -> Vec<u8> {
        let total_len: usize = parts.iter().map(|p| p.len()).sum();
        let mut result = Vec::with_capacity(total_len);
        for part in parts {
            result.extend_from_slice(part);
        }
        result
    }
}

mod test_buffers {
    use super::{Field, Mode, helpers::BufferList};

    pub fn ready_for_query() -> Vec<u8> {
        let mut list = BufferList::new();
        list.add_bytes(b"I");
        list.join(true, Some(b'Z'))
    }

    pub fn authentication_ok() -> Vec<u8> {
        let mut list = BufferList::new();
        list.add_int32(0);
        list.join(true, Some(b'R'))
    }

    pub fn authentication_cleartext_password() -> Vec<u8> {
        let mut list = BufferList::new();
        list.add_int32(3);
        list.join(true, Some(b'R'))
    }

    pub fn authentication_md5_password() -> Vec<u8> {
        let mut list = BufferList::new();
        list.add_int32(5);
        list.add_bytes(&[1, 2, 3, 4]);
        list.join(true, Some(b'R'))
    }

    pub fn authentication_sasl() -> Vec<u8> {
        let mut list = BufferList::new();
        list.add_int32(10);
        list.add_cstring("SCRAM-SHA-256");
        list.add_cstring("");
        list.join(true, Some(b'R'))
    }

    pub fn authentication_sasl_continue() -> Vec<u8> {
        let mut list = BufferList::new();
        list.add_int32(11);
        list.add_string("data");
        list.join(true, Some(b'R'))
    }

    pub fn authentication_sasl_final() -> Vec<u8> {
        let mut list = BufferList::new();
        list.add_int32(12);
        list.add_string("data");
        list.join(true, Some(b'R'))
    }

    pub fn parameter_status(name: &str, value: &str) -> Vec<u8> {
        let mut list = BufferList::new();
        list.add_cstring(name);
        list.add_cstring(value);
        list.join(true, Some(b'S'))
    }

    pub fn backend_key_data(process_id: i32, secret_key: i32) -> Vec<u8> {
        let mut list = BufferList::new();
        list.add_int32(process_id);
        list.add_int32(secret_key);
        list.join(true, Some(b'K'))
    }

    pub fn command_complete(text: &str) -> Vec<u8> {
        let mut list = BufferList::new();
        list.add_cstring(text);
        list.join(true, Some(b'C'))
    }

    pub fn row_description(fields: &[Field]) -> Vec<u8> {
        let mut list = BufferList::new();
        list.add_int16(fields.len() as i16);
        for field in fields {
            list.add_cstring(&field.name);
            list.add_int32(field.table_id);
            list.add_int16(field.column_id);
            list.add_int32(field.data_type_id);
            list.add_int16(field.data_type_size);
            list.add_int32(field.data_type_modifier);
            list.add_int16(match field.format {
                Mode::Text => 0,
                Mode::Binary => 1,
            });
        }
        list.join(true, Some(b'T'))
    }

    pub fn parameter_description(ids: &[i32]) -> Vec<u8> {
        let mut list = BufferList::new();
        list.add_int16(ids.len() as i16);
        for id in ids {
            list.add_int32(*id);
        }
        list.join(true, Some(b't'))
    }

    pub fn data_row(values: &[Option<&str>]) -> Vec<u8> {
        let mut list = BufferList::new();
        list.add_int16(values.len() as i16);
        for value in values {
            match value {
                Some(val) => {
                    let bytes = val.as_bytes();
                    list.add_int32(bytes.len() as i32);
                    list.add_bytes(bytes);
                }
                None => {
                    list.add_int32(-1);
                }
            }
        }
        list.join(true, Some(b'D'))
    }

    pub fn error(fields: &[(&str, &str)]) -> Vec<u8> {
        error_or_notice(fields).join(true, Some(b'E'))
    }

    pub fn notice(fields: &[(&str, &str)]) -> Vec<u8> {
        error_or_notice(fields).join(true, Some(b'N'))
    }

    fn error_or_notice(fields: &[(&str, &str)]) -> BufferList {
        let mut list = BufferList::new();
        for (field_type, value) in fields {
            let bytes = field_type.as_bytes();
            assert_eq!(
                bytes.len(),
                1,
                "field type must be a single character, got {field_type}"
            );
            list.add_byte(bytes[0]);
            list.add_cstring(value);
        }
        list.add_byte(0);
        list
    }

    pub fn parse_complete() -> Vec<u8> {
        BufferList::new().join(true, Some(b'1'))
    }

    pub fn bind_complete() -> Vec<u8> {
        BufferList::new().join(true, Some(b'2'))
    }

    pub fn close_complete() -> Vec<u8> {
        BufferList::new().join(true, Some(b'3'))
    }

    pub fn notification(process_id: i32, channel: &str, payload: &str) -> Vec<u8> {
        let mut list = BufferList::new();
        list.add_int32(process_id);
        list.add_cstring(channel);
        list.add_cstring(payload);
        list.join(true, Some(b'A'))
    }

    pub fn empty_query() -> Vec<u8> {
        BufferList::new().join(true, Some(b'I'))
    }

    pub fn portal_suspended() -> Vec<u8> {
        BufferList::new().join(true, Some(b's'))
    }

    pub fn replication_start() -> Vec<u8> {
        vec![b'W', 0, 0, 0, 4]
    }

    pub fn no_data() -> Vec<u8> {
        vec![b'n', 0, 0, 0, 4]
    }

    pub fn copy_in(cols: usize) -> Vec<u8> {
        let mut list = BufferList::new();
        list.add_byte(0);
        list.add_int16(cols as i16);
        for idx in 0..cols {
            list.add_int16(idx as i16);
        }
        list.join(true, Some(b'G'))
    }

    pub fn copy_out(cols: usize) -> Vec<u8> {
        let mut list = BufferList::new();
        list.add_byte(0);
        list.add_int16(cols as i16);
        for idx in 0..cols {
            list.add_int16(idx as i16);
        }
        list.join(true, Some(b'H'))
    }

    pub fn copy_data(bytes: &[u8]) -> Vec<u8> {
        let mut list = BufferList::new();
        list.add_bytes(bytes);
        list.join(true, Some(b'd'))
    }

    pub fn copy_done() -> Vec<u8> {
        BufferList::new().join(true, Some(b'c'))
    }
}

use helpers::{BufferList, concat_slices};
use test_buffers as buffers;

fn parse_vec_chunks(chunks: Vec<Vec<u8>>) -> Result<Vec<BackendMessage>> {
    let mut parser = Parser::new();
    let mut messages = Vec::new();
    for chunk in chunks {
        parser.parse(chunk.as_slice(), |msg| {
            messages.push(msg);
            Ok(())
        })?;
    }
    Ok(messages)
}

fn parse_slices(chunks: &[&[u8]]) -> Result<Vec<BackendMessage>> {
    let mut parser = Parser::new();
    let mut messages = Vec::new();
    for chunk in chunks {
        parser.parse(chunk, |msg| {
            messages.push(msg);
            Ok(())
        })?;
    }
    Ok(messages)
}

fn parse_single(buffer: Vec<u8>) -> Result<BackendMessage> {
    let mut messages = parse_vec_chunks(vec![buffer])?;
    Ok(messages.remove(0))
}

fn assert_data_row_fields(message: &BackendMessage, expected: &[Option<&str>]) {
    match message {
        BackendMessage::DataRow(row) => {
            assert_eq!(row.fields.len(), expected.len());
            for (actual, expected_value) in row.fields.iter().zip(expected.iter()) {
                match (actual, expected_value) {
                    (Some(actual), Some(expected)) => assert_eq!(actual, expected),
                    (None, None) => {}
                    (other_actual, other_expected) => panic!(
                        "mismatched field value: expected {:?}, got {:?}",
                        other_expected, other_actual
                    ),
                }
            }
        }
        other => panic!("expected dataRow message, got {:?}", other.name()),
    }
}

#[test]
fn parser_parses_authentication_messages() -> Result<()> {
    match parse_single(buffers::authentication_ok())? {
        BackendMessage::Authentication(AuthenticationMessage::Ok(msg)) => {
            assert_eq!(msg.length, 8);
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    match parse_single(buffers::authentication_cleartext_password())? {
        BackendMessage::Authentication(AuthenticationMessage::Cleartext(msg)) => {
            assert_eq!(msg.length, 8);
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    match parse_single(buffers::authentication_md5_password())? {
        BackendMessage::Authentication(AuthenticationMessage::Md5(msg)) => {
            assert_eq!(msg.length, 12);
            assert_eq!(msg.salt, vec![1, 2, 3, 4]);
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    match parse_single(buffers::authentication_sasl())? {
        BackendMessage::Authentication(AuthenticationMessage::Sasl(msg)) => {
            assert_eq!(msg.length, buffers::authentication_sasl().len() - 1);
            assert_eq!(msg.mechanisms, vec!["SCRAM-SHA-256".to_string()]);
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    match parse_single(buffers::authentication_sasl_continue())? {
        BackendMessage::Authentication(AuthenticationMessage::SaslContinue(msg)) => {
            assert_eq!(
                msg.length,
                buffers::authentication_sasl_continue().len() - 1
            );
            assert_eq!(msg.data, "data");
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    let mut extended_continue = buffers::authentication_sasl_continue();
    extended_continue.extend_from_slice(&[1, 2, 3, 4]);
    match parse_single(extended_continue)? {
        BackendMessage::Authentication(AuthenticationMessage::SaslContinue(msg)) => {
            assert_eq!(msg.data, "data");
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    match parse_single(buffers::authentication_sasl_final())? {
        BackendMessage::Authentication(AuthenticationMessage::SaslFinal(msg)) => {
            assert_eq!(msg.length, buffers::authentication_sasl_final().len() - 1);
            assert_eq!(msg.data, "data");
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    let mut extended_final = buffers::authentication_sasl_final();
    extended_final.extend_from_slice(&[1, 2, 4, 5]);
    match parse_single(extended_final)? {
        BackendMessage::Authentication(AuthenticationMessage::SaslFinal(msg)) => {
            assert_eq!(msg.data, "data");
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    Ok(())
}

#[test]
fn parser_parses_status_and_notification_messages() -> Result<()> {
    match parse_single(buffers::parameter_status("client_encoding", "UTF8"))? {
        BackendMessage::ParameterStatus(msg) => {
            assert_eq!(msg.length, 25);
            assert_eq!(msg.parameter_name, "client_encoding");
            assert_eq!(msg.parameter_value, "UTF8");
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    match parse_single(buffers::backend_key_data(1, 2))? {
        BackendMessage::BackendKeyData(msg) => {
            assert_eq!(msg.length, 12);
            assert_eq!(msg.process_id, 1);
            assert_eq!(msg.secret_key, 2);
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    match parse_single(buffers::ready_for_query())? {
        BackendMessage::ReadyForQuery(msg) => {
            assert_eq!(msg.length, 5);
            assert_eq!(msg.status, b'I');
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    match parse_single(buffers::command_complete("SELECT 3"))? {
        BackendMessage::CommandComplete(msg) => {
            assert_eq!(msg.length, 13);
            assert_eq!(msg.text, "SELECT 3");
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    match parse_single(buffers::notification(4, "hi", "boom"))? {
        BackendMessage::Notification(msg) => {
            assert_eq!(msg.length, buffers::notification(4, "hi", "boom").len() - 1);
            assert_eq!(msg.process_id, 4);
            assert_eq!(msg.channel, "hi");
            assert_eq!(msg.payload, "boom");
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    Ok(())
}

#[test]
fn parser_parses_simple_backend_messages() -> Result<()> {
    let cases: Vec<(Vec<u8>, MessageName, usize)> = vec![
        (buffers::parse_complete(), MessageName::ParseComplete, 5),
        (buffers::bind_complete(), MessageName::BindComplete, 5),
        (buffers::close_complete(), MessageName::CloseComplete, 5),
        (buffers::portal_suspended(), MessageName::PortalSuspended, 5),
        (
            buffers::replication_start(),
            MessageName::ReplicationStart,
            4,
        ),
        (buffers::empty_query(), MessageName::EmptyQuery, 4),
        (buffers::copy_done(), MessageName::CopyDone, 4),
        (buffers::no_data(), MessageName::NoData, 5),
    ];

    for (buffer, expected_name, expected_length) in cases {
        let message = parse_single(buffer)?;
        assert_eq!(message.name(), expected_name);
        assert_eq!(message.length(), expected_length);
    }

    Ok(())
}

#[test]
fn parser_parses_row_description_messages() -> Result<()> {
    let mut row1 = Field {
        name: "id".into(),
        table_id: 1,
        column_id: 2,
        data_type_id: 3,
        data_type_size: 4,
        data_type_modifier: 5,
        format: Mode::Text,
    };
    let row1_initial = row1.clone();
    let one_row_buffer = buffers::row_description(std::slice::from_ref(&row1_initial));

    row1.name = "bang".into();
    let row2 = Field {
        name: "whoah".into(),
        table_id: 10,
        column_id: 11,
        data_type_id: 12,
        data_type_size: 13,
        data_type_modifier: 14,
        format: Mode::Text,
    };
    let two_row_buffer = buffers::row_description(&[row1.clone(), row2.clone()]);

    let mut empty_list = BufferList::new();
    empty_list.add_int16(0);
    let empty_row_buffer = empty_list.join(true, Some(b'T'));

    match parse_single(empty_row_buffer)? {
        BackendMessage::RowDescription(msg) => {
            assert_eq!(msg.length, 6);
            assert!(msg.fields.is_empty());
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    match parse_single(one_row_buffer)? {
        BackendMessage::RowDescription(msg) => {
            assert_eq!(msg.length, 27);
            assert_eq!(msg.fields.len(), 1);
            let field = &msg.fields[0];
            assert_eq!(field.name, row1_initial.name);
            assert_eq!(field.table_id, row1_initial.table_id);
            assert_eq!(field.column_id, row1_initial.column_id);
            assert_eq!(field.data_type_id, row1_initial.data_type_id);
            assert_eq!(field.data_type_size, row1_initial.data_type_size);
            assert_eq!(field.data_type_modifier, row1_initial.data_type_modifier);
            assert_eq!(field.format, row1_initial.format);
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    match parse_single(two_row_buffer)? {
        BackendMessage::RowDescription(msg) => {
            assert_eq!(msg.length, 53);
            assert_eq!(msg.fields.len(), 2);
            let field1 = &msg.fields[0];
            assert_eq!(field1.name, row1.name);
            let field2 = &msg.fields[1];
            assert_eq!(field2.name, row2.name);
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    Ok(())
}

#[test]
fn parser_parses_parameter_description_messages() -> Result<()> {
    match parse_single(buffers::parameter_description(&[]))? {
        BackendMessage::ParameterDescription(msg) => {
            assert_eq!(msg.length, 6);
            assert!(msg.data_type_ids.is_empty());
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    match parse_single(buffers::parameter_description(&[1111]))? {
        BackendMessage::ParameterDescription(msg) => {
            assert_eq!(msg.length, 10);
            assert_eq!(msg.data_type_ids, vec![1111]);
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    match parse_single(buffers::parameter_description(&[2222, 3333]))? {
        BackendMessage::ParameterDescription(msg) => {
            assert_eq!(msg.length, 14);
            assert_eq!(msg.data_type_ids, vec![2222, 3333]);
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    Ok(())
}

#[test]
fn parser_parses_data_row_messages() -> Result<()> {
    let buffer_empty = buffers::data_row(&[]);
    match parse_single(buffer_empty.clone())? {
        BackendMessage::DataRow(msg) => {
            assert_eq!(msg.length, buffer_empty.len() - 1);
            assert!(msg.fields.is_empty());
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    let buffer_one = buffers::data_row(&[Some("test")]);
    match parse_single(buffer_one.clone())? {
        BackendMessage::DataRow(msg) => {
            assert_eq!(msg.length, buffer_one.len() - 1);
            assert_eq!(msg.fields, vec![Some("test".into())]);
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    Ok(())
}

#[test]
fn parser_parses_notice_and_error_messages() -> Result<()> {
    match parse_single(buffers::notice(&[("C", "code")]))? {
        BackendMessage::Notice(msg) => {
            assert_eq!(msg.length, buffers::notice(&[("C", "code")]).len() - 1);
            assert_eq!(msg.code.as_deref(), Some("code"));
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    let error_buffer = buffers::error(&[]);
    match parse_single(error_buffer.clone())? {
        BackendMessage::Error(msg) => {
            assert_eq!(msg.length, error_buffer.len() - 1);
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    let detailed_error = buffers::error(&[
        ("S", "ERROR"),
        ("C", "code"),
        ("M", "message"),
        ("D", "details"),
        ("H", "hint"),
        ("P", "100"),
        ("p", "101"),
        ("q", "query"),
        ("W", "where"),
        ("F", "file"),
        ("L", "line"),
        ("R", "routine"),
        ("Z", "ignored"),
    ]);
    match parse_single(detailed_error.clone())? {
        BackendMessage::Error(msg) => {
            assert_eq!(msg.length, detailed_error.len() - 1);
            assert_eq!(msg.severity.as_deref(), Some("ERROR"));
            assert_eq!(msg.code.as_deref(), Some("code"));
            assert_eq!(msg.message, "message");
            assert_eq!(msg.detail.as_deref(), Some("details"));
            assert_eq!(msg.hint.as_deref(), Some("hint"));
            assert_eq!(msg.position.as_deref(), Some("100"));
            assert_eq!(msg.internal_position.as_deref(), Some("101"));
            assert_eq!(msg.internal_query.as_deref(), Some("query"));
            assert_eq!(msg.r#where.as_deref(), Some("where"));
            assert_eq!(msg.file.as_deref(), Some("file"));
            assert_eq!(msg.line.as_deref(), Some("line"));
            assert_eq!(msg.routine.as_deref(), Some("routine"));
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    Ok(())
}

#[test]
fn parser_parses_copy_messages() -> Result<()> {
    match parse_single(buffers::copy_in(0))? {
        BackendMessage::CopyResponse(msg) => {
            assert_eq!(msg.length, 7);
            assert!(!msg.binary);
            assert!(msg.column_types.is_empty());
            assert_eq!(msg.name, MessageName::CopyInResponse);
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    match parse_single(buffers::copy_in(2))? {
        BackendMessage::CopyResponse(msg) => {
            assert_eq!(msg.length, 11);
            assert_eq!(msg.column_types, vec![0, 1]);
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    match parse_single(buffers::copy_out(0))? {
        BackendMessage::CopyResponse(msg) => {
            assert_eq!(msg.length, 7);
            assert_eq!(msg.name, MessageName::CopyOutResponse);
            assert!(msg.column_types.is_empty());
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    match parse_single(buffers::copy_out(3))? {
        BackendMessage::CopyResponse(msg) => {
            assert_eq!(msg.length, 13);
            assert_eq!(msg.column_types, vec![0, 1, 2]);
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    match parse_single(buffers::copy_done())? {
        BackendMessage::CopyDone { length } => {
            assert_eq!(length, 4);
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    match parse_single(buffers::copy_data(&[5, 6, 7]))? {
        BackendMessage::CopyData(msg) => {
            assert_eq!(msg.length, 7);
            assert_eq!(msg.chunk, vec![5, 6, 7]);
        }
        other => panic!("unexpected message: {:?}", other.name()),
    }

    Ok(())
}

#[test]
fn parser_handles_split_single_message() -> Result<()> {
    let full = buffers::data_row(&[None, Some("bang"), Some("zug zug"), None, Some("!")]);

    let messages = parse_vec_chunks(vec![full.clone()])?;
    assert_eq!(messages.len(), 1);
    assert_data_row_fields(
        &messages[0],
        &[None, Some("bang"), Some("zug zug"), None, Some("!")],
    );

    let splits = [
        6,
        2,
        full.len().saturating_sub(2),
        full.len().saturating_sub(1),
        full.len().saturating_sub(5),
    ];

    for split in splits {
        if split == 0 || split >= full.len() {
            continue;
        }
        let first_len = full.len() - split;
        let first = full[..first_len].to_vec();
        let second = full[first_len..].to_vec();
        let messages = parse_vec_chunks(vec![first, second])?;
        assert_eq!(messages.len(), 1);
        assert_data_row_fields(
            &messages[0],
            &[None, Some("bang"), Some("zug zug"), None, Some("!")],
        );
    }

    Ok(())
}

#[test]
fn parser_handles_split_multiple_messages() -> Result<()> {
    let data_row = buffers::data_row(&[Some("!")]);
    let ready_for_query = buffers::ready_for_query();
    let mut combined = data_row.clone();
    combined.extend_from_slice(&ready_for_query);

    let verify_messages = |messages: &[BackendMessage]| {
        assert_eq!(messages.len(), 2);
        assert_data_row_fields(&messages[0], &[Some("!")]);
        match &messages[1] {
            BackendMessage::ReadyForQuery(msg) => assert_eq!(msg.status, b'I'),
            other => panic!("unexpected message: {:?}", other.name()),
        }
    };

    verify_messages(&parse_vec_chunks(vec![combined.clone()])?);

    let splits = [
        11,
        combined.len().saturating_sub(1),
        combined.len().saturating_sub(4),
        combined.len().saturating_sub(6),
        8,
        1,
    ];

    for split in splits {
        if split == 0 || split >= combined.len() {
            continue;
        }
        let first_len = combined.len() - split;
        let first = combined[..first_len].to_vec();
        let second = combined[first_len..].to_vec();
        let messages = parse_vec_chunks(vec![first, second])?;
        verify_messages(&messages);
    }

    Ok(())
}

#[test]
fn parser_respects_buffer_views() -> Result<()> {
    let message = buffers::data_row(&[Some("bang")]);
    let wrapper = concat_slices(&[&[1, 2, 3, 4], message.as_slice(), &[5, 6, 7, 8]]);
    let slice = &wrapper[4..4 + message.len()];
    let messages = parse_slices(&[slice])?;
    assert_eq!(messages.len(), 1);
    assert_data_row_fields(&messages[0], &[Some("bang")]);
    Ok(())
}

#[test]
fn serializer_builds_messages() -> Result<()> {
    let startup = Serialize::startup([("user", "brian"), ("database", "bang")]);
    let mut expected = BufferList::new();
    expected
        .add_int16(3)
        .add_int16(0)
        .add_cstring("user")
        .add_cstring("brian")
        .add_cstring("database")
        .add_cstring("bang")
        .add_cstring("client_encoding")
        .add_cstring("UTF8")
        .add_cstring("");
    assert_eq!(startup, expected.join(true, None));

    let password = Serialize::password("!");
    let mut expected = BufferList::new();
    expected.add_cstring("!");
    assert_eq!(password, expected.join(true, Some(b'p')));

    let request_ssl = Serialize::request_ssl();
    let mut expected = BufferList::new();
    expected.add_int32(80877103);
    assert_eq!(request_ssl, expected.join(true, None));

    let sasl_initial = Serialize::send_sasl_initial_response_message("mech", "data");
    let mut expected = BufferList::new();
    expected.add_cstring("mech").add_int32(4).add_string("data");
    assert_eq!(sasl_initial, expected.join(true, Some(b'p')));

    let sasl_final = Serialize::send_scram_client_final_message("data");
    let mut expected = BufferList::new();
    expected.add_string("data");
    assert_eq!(sasl_final, expected.join(true, Some(b'p')));

    let query = Serialize::query("select * from boom");
    let mut expected = BufferList::new();
    expected.add_cstring("select * from boom");
    assert_eq!(query, expected.join(true, Some(b'Q')));

    let parse = Serialize::parse(None, "!", &[]);
    let mut expected = BufferList::new();
    expected.add_cstring("").add_cstring("!").add_int16(0);
    assert_eq!(parse, expected.join(true, Some(b'P')));

    let parse_named = Serialize::parse(Some("boom"), "select * from boom", &[]);
    let mut expected = BufferList::new();
    expected
        .add_cstring("boom")
        .add_cstring("select * from boom")
        .add_int16(0);
    assert_eq!(parse_named, expected.join(true, Some(b'P')));

    let parse_types = Serialize::parse(
        Some("force"),
        "select * from bang where name = $1",
        &[1, 2, 3, 4],
    );
    let mut expected = BufferList::new();
    expected
        .add_cstring("force")
        .add_cstring("select * from bang where name = $1")
        .add_int16(4)
        .add_int32(1)
        .add_int32(2)
        .add_int32(3)
        .add_int32(4);
    assert_eq!(parse_types, expected.join(true, Some(b'P')));

    let bind_config = BindConfig {
        portal: Some("bang".into()),
        statement: Some("woo".into()),
        values: vec![
            BindValue::from("1"),
            BindValue::from("hi"),
            BindValue::Null,
            BindValue::from("zing"),
        ],
        ..BindConfig::default()
    };
    let bind = Serialize::bind(&bind_config);
    let mut expected = BufferList::new();
    expected
        .add_cstring("bang")
        .add_cstring("woo")
        .add_int16(4)
        .add_int16(0)
        .add_int16(0)
        .add_int16(0)
        .add_int16(0)
        .add_int16(4)
        .add_int32(1)
        .add_bytes(b"1")
        .add_int32(2)
        .add_bytes(b"hi")
        .add_int32(-1)
        .add_int32(4)
        .add_bytes(b"zing")
        .add_int16(0);
    assert_eq!(bind, expected.join(true, Some(b'B')));

    let bind_config = BindConfig {
        portal: Some("bang".into()),
        statement: Some("woo".into()),
        values: vec![
            BindValue::from("1"),
            BindValue::from("hi"),
            BindValue::Null,
            BindValue::from(vec![b'z', b'i', b'n', b'g']),
        ],
        ..BindConfig::default()
    };
    let bind = Serialize::bind(&bind_config);
    let mut expected = BufferList::new();
    expected
        .add_cstring("bang")
        .add_cstring("woo")
        .add_int16(4)
        .add_int16(0)
        .add_int16(0)
        .add_int16(0)
        .add_int16(1)
        .add_int16(4)
        .add_int32(1)
        .add_bytes(b"1")
        .add_int32(2)
        .add_bytes(b"hi")
        .add_int32(-1)
        .add_int32(4)
        .add_bytes(b"zing")
        .add_int16(0);
    assert_eq!(bind, expected.join(true, Some(b'B')));

    let bind_config = BindConfig {
        portal: Some("bang".into()),
        statement: Some("woo".into()),
        values: vec![
            BindValue::from("1"),
            BindValue::from("hi"),
            BindValue::Null,
            BindValue::from("zing"),
        ],
        value_mapper: Some(Box::new(|_, _| BindValue::Null)),
        ..BindConfig::default()
    };
    let bind = Serialize::bind(&bind_config);
    let mut expected = BufferList::new();
    expected
        .add_cstring("bang")
        .add_cstring("woo")
        .add_int16(4)
        .add_int16(0)
        .add_int16(0)
        .add_int16(0)
        .add_int16(0)
        .add_int16(4)
        .add_int32(-1)
        .add_int32(-1)
        .add_int32(-1)
        .add_int32(-1)
        .add_int16(0);
    assert_eq!(bind, expected.join(true, Some(b'B')));

    let default_execute = Serialize::execute(None);
    assert_eq!(default_execute, vec![b'E', 0, 0, 0, 9, 0, 0, 0, 0, 0]);

    let exec_config = ExecConfig {
        portal: Some("my favorite portal".into()),
        rows: Some(100),
    };
    let execute = Serialize::execute(Some(&exec_config));
    let mut expected = BufferList::new();
    expected.add_cstring("my favorite portal").add_int32(100);
    assert_eq!(execute, expected.join(true, Some(b'E')));

    assert_eq!(Serialize::flush(), vec![b'H', 0, 0, 0, 4]);
    assert_eq!(Serialize::sync(), vec![b'S', 0, 0, 0, 4]);
    assert_eq!(Serialize::end(), vec![b'X', 0, 0, 0, 4]);

    let describe_statement = Serialize::describe(&PortalTarget::new('S', Some("bang".into())));
    let mut expected = BufferList::new();
    expected.add_char('S').add_cstring("bang");
    assert_eq!(describe_statement, expected.join(true, Some(b'D')));

    let describe_portal = Serialize::describe(&PortalTarget::new('P', None));
    let mut expected = BufferList::new();
    expected.add_char('P').add_cstring("");
    assert_eq!(describe_portal, expected.join(true, Some(b'D')));

    let close_statement = Serialize::close(&PortalTarget::new('S', Some("bang".into())));
    let mut expected = BufferList::new();
    expected.add_char('S').add_cstring("bang");
    assert_eq!(close_statement, expected.join(true, Some(b'C')));

    let close_portal = Serialize::close(&PortalTarget::new('P', None));
    let mut expected = BufferList::new();
    expected.add_char('P').add_cstring("");
    assert_eq!(close_portal, expected.join(true, Some(b'C')));

    let copy_data = Serialize::copy_data(&[1, 2, 3]);
    let mut expected = BufferList::new();
    expected.add_bytes(&[1, 2, 3]);
    assert_eq!(copy_data, expected.join(true, Some(b'd')));

    let copy_fail = Serialize::copy_fail("err!");
    let mut expected = BufferList::new();
    expected.add_cstring("err!");
    assert_eq!(copy_fail, expected.join(true, Some(b'f')));

    let copy_done = Serialize::copy_done();
    assert_eq!(copy_done, vec![b'c', 0, 0, 0, 4]);

    let cancel = Serialize::cancel(3, 4);
    let mut expected = BufferList::new();
    expected
        .add_int16(1234)
        .add_int16(5678)
        .add_int32(3)
        .add_int32(4);
    assert_eq!(cancel, expected.join(true, None));

    Ok(())
}

#[test]
fn string_utils_byte_length_utf8_matches_typescript_expectations() {
    assert_eq!(byte_length_utf8(""), 0);
    assert_eq!(byte_length_utf8("hello"), 5);
    assert_eq!(byte_length_utf8("©"), 2);
    assert_eq!(byte_length_utf8("你好"), 6);
    assert_eq!(byte_length_utf8("𝄞"), 4);
    assert_eq!(byte_length_utf8("hello 你好 𝄞"), 17);
    assert_eq!(byte_length_utf8("😀"), 4);
    assert_eq!(
        byte_length_utf8("The quick brown 🦊 jumps over 13 lazy 🐶! 你好世界"),
        58
    );
}
