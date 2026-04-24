use anyhow::{Result, anyhow, ensure};

use crate::protocol::buffer_reader::BufferReader;
use crate::protocol::messages::{
    AuthenticationCleartextPassword, AuthenticationMD5Password, AuthenticationMessage,
    AuthenticationOk, AuthenticationSasl, AuthenticationSaslContinue, AuthenticationSaslFinal,
    BackendKeyDataMessage, BackendMessage, CommandCompleteMessage, CopyDataMessage, CopyResponse,
    DataRowMessage, DatabaseError, Field, MessageName, NoticeMessage, NoticeOrErrorFields,
    NotificationResponseMessage, ParameterDescriptionMessage, ParameterStatusMessage,
    ReadyForQueryMessage, RowDescriptionMessage, collect_fields,
};
use crate::protocol::types::{BufferParameter, Mode, Modes};

const HEADER_LEN: usize = 5;

const CODE_DATA_ROW: u8 = b'D';
const CODE_PARSE_COMPLETE: u8 = b'1';
const CODE_BIND_COMPLETE: u8 = b'2';
const CODE_CLOSE_COMPLETE: u8 = b'3';
const CODE_COMMAND_COMPLETE: u8 = b'C';
const CODE_READY_FOR_QUERY: u8 = b'Z';
const CODE_NO_DATA: u8 = b'n';
const CODE_NOTIFICATION_RESPONSE: u8 = b'A';
const CODE_AUTHENTICATION: u8 = b'R';
const CODE_PARAMETER_STATUS: u8 = b'S';
const CODE_BACKEND_KEY_DATA: u8 = b'K';
const CODE_ERROR: u8 = b'E';
const CODE_NOTICE: u8 = b'N';
const CODE_ROW_DESCRIPTION: u8 = b'T';
const CODE_PARAMETER_DESCRIPTION: u8 = b't';
const CODE_PORTAL_SUSPENDED: u8 = b's';
const CODE_REPLICATION_START: u8 = b'W';
const CODE_EMPTY_QUERY: u8 = b'I';
const CODE_COPY_IN: u8 = b'G';
const CODE_COPY_OUT: u8 = b'H';
const CODE_COPY_DONE: u8 = b'c';
const CODE_COPY_DATA: u8 = b'd';

pub type MessageCallback = dyn FnMut(BackendMessage) -> Result<()>;

#[derive(Debug, Default)]
pub struct Parser {
    buffer: Vec<u8>,
}

impl Parser {
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    pub fn parse<F>(&mut self, input: BufferParameter, mut callback: F) -> Result<()>
    where
        F: FnMut(BackendMessage) -> Result<()>,
    {
        self.buffer.extend_from_slice(input);

        let mut cursor = 0usize;
        while self.buffer.len().saturating_sub(cursor) >= HEADER_LEN {
            let code = self.buffer[cursor];
            let length_bytes = &self.buffer[cursor + 1..cursor + HEADER_LEN];
            let length = u32::from_be_bytes([
                length_bytes[0],
                length_bytes[1],
                length_bytes[2],
                length_bytes[3],
            ]) as usize;
            let total_len = 1 + length;

            if self.buffer.len() - cursor < total_len {
                break; // wait for more data
            }

            let body = &self.buffer[cursor + HEADER_LEN..cursor + total_len]; // exclude code+length header
            let message = self.handle_packet(code, body, length)?;
            callback(message)?;
            cursor += total_len;
        }

        if cursor > 0 {
            self.buffer.drain(0..cursor);
        }

        Ok(())
    }

    fn handle_packet(&self, code: u8, bytes: &[u8], length: usize) -> Result<BackendMessage> {
        let mut reader = BufferReader::default();
        reader.set_buffer(0, bytes);

        match code {
            CODE_BIND_COMPLETE => Ok(BackendMessage::BindComplete { length: 5 }),
            CODE_PARSE_COMPLETE => Ok(BackendMessage::ParseComplete { length: 5 }),
            CODE_CLOSE_COMPLETE => Ok(BackendMessage::CloseComplete { length: 5 }),
            CODE_NO_DATA => Ok(BackendMessage::NoData { length: 5 }),
            CODE_PORTAL_SUSPENDED => Ok(BackendMessage::PortalSuspended { length: 5 }),
            CODE_COPY_DONE => Ok(BackendMessage::CopyDone { length: 4 }),
            CODE_REPLICATION_START => Ok(BackendMessage::ReplicationStart { length: 4 }),
            CODE_EMPTY_QUERY => Ok(BackendMessage::EmptyQuery { length: 4 }),
            CODE_COMMAND_COMPLETE => self.parse_command_complete(length, &mut reader),
            CODE_READY_FOR_QUERY => self.parse_ready_for_query(length, &mut reader),
            CODE_DATA_ROW => self.parse_data_row(length, &mut reader),
            CODE_NOTIFICATION_RESPONSE => self.parse_notification(length, &mut reader),
            CODE_PARAMETER_STATUS => self.parse_parameter_status(length, &mut reader),
            CODE_BACKEND_KEY_DATA => self.parse_backend_key_data(length, &mut reader),
            CODE_ERROR => self.parse_error_message(length, &mut reader),
            CODE_NOTICE => self.parse_notice_message(length, &mut reader),
            CODE_ROW_DESCRIPTION => self.parse_row_description(length, &mut reader),
            CODE_PARAMETER_DESCRIPTION => self.parse_parameter_description(length, &mut reader),
            CODE_COPY_IN => {
                self.parse_copy_message(length, &mut reader, MessageName::CopyInResponse)
            }
            CODE_COPY_OUT => {
                self.parse_copy_message(length, &mut reader, MessageName::CopyOutResponse)
            }
            CODE_COPY_DATA => self.parse_copy_data(length, bytes),
            CODE_AUTHENTICATION => self.parse_authentication(length, &mut reader),
            _ => Err(anyhow!("received invalid response: {:x}", code)),
        }
    }

    fn parse_ready_for_query(
        &self,
        length: usize,
        reader: &mut BufferReader<'_>,
    ) -> Result<BackendMessage> {
        let status = reader.string(1)?;
        ensure!(status.len() == 1, "invalid readyForQuery status");
        Ok(BackendMessage::ReadyForQuery(ReadyForQueryMessage {
            length,
            status: status.as_bytes()[0],
        }))
    }

    fn parse_command_complete(
        &self,
        length: usize,
        reader: &mut BufferReader<'_>,
    ) -> Result<BackendMessage> {
        let text = reader.cstring()?;
        Ok(BackendMessage::CommandComplete(CommandCompleteMessage {
            length,
            text,
        }))
    }

    fn parse_copy_data(&self, length: usize, bytes: &[u8]) -> Result<BackendMessage> {
        let data_len = length.saturating_sub(4);
        let chunk = bytes[..data_len].to_vec();
        Ok(BackendMessage::CopyData(CopyDataMessage { length, chunk }))
    }

    fn parse_copy_message(
        &self,
        length: usize,
        reader: &mut BufferReader<'_>,
        name: MessageName,
    ) -> Result<BackendMessage> {
        let is_binary = reader.byte()? != 0;
        let column_count = reader.int16()? as usize;
        let mut column_types = Vec::with_capacity(column_count);
        for _ in 0..column_count {
            column_types.push(reader.int16()?);
        }
        Ok(BackendMessage::CopyResponse(CopyResponse {
            length,
            name,
            binary: is_binary,
            column_types,
        }))
    }

    fn parse_notification(
        &self,
        length: usize,
        reader: &mut BufferReader<'_>,
    ) -> Result<BackendMessage> {
        let process_id = reader.int32()?;
        let channel = reader.cstring()?;
        let payload = reader.cstring()?;
        Ok(BackendMessage::Notification(NotificationResponseMessage {
            length,
            process_id,
            channel,
            payload,
        }))
    }

    fn parse_row_description(
        &self,
        length: usize,
        reader: &mut BufferReader<'_>,
    ) -> Result<BackendMessage> {
        let field_count = reader.int16()? as usize;
        let mut fields = Vec::with_capacity(field_count);
        for _ in 0..field_count {
            fields.push(self.parse_field(reader)?);
        }
        Ok(BackendMessage::RowDescription(RowDescriptionMessage {
            length,
            fields,
        }))
    }

    fn parse_field(&self, reader: &mut BufferReader<'_>) -> Result<Field> {
        let name = reader.cstring()?;
        let table_id = reader.int32()?;
        let column_id = reader.int16()?;
        let data_type_id = reader.int32()?;
        let data_type_size = reader.int16()?;
        let data_type_modifier = reader.int32()?;
        let mode = reader.int16()?;
        let format = Mode::try_from(mode).unwrap_or(Modes::TEXT);
        Ok(Field {
            name,
            table_id,
            column_id,
            data_type_id,
            data_type_size,
            data_type_modifier,
            format,
        })
    }

    fn parse_parameter_description(
        &self,
        length: usize,
        reader: &mut BufferReader<'_>,
    ) -> Result<BackendMessage> {
        let count = reader.int16()? as usize;
        let mut ids = Vec::with_capacity(count);
        for _ in 0..count {
            ids.push(reader.int32()?);
        }
        Ok(BackendMessage::ParameterDescription(
            ParameterDescriptionMessage {
                length,
                data_type_ids: ids,
            },
        ))
    }

    fn parse_data_row(
        &self,
        length: usize,
        reader: &mut BufferReader<'_>,
    ) -> Result<BackendMessage> {
        let field_count = reader.int16()? as usize;
        let mut fields = Vec::with_capacity(field_count);
        for _ in 0..field_count {
            let len = reader.int32()?;
            if len == -1 {
                fields.push(None);
            } else {
                let len = len as usize;
                let value = reader.string(len)?;
                fields.push(Some(value));
            }
        }
        Ok(BackendMessage::DataRow(DataRowMessage { length, fields }))
    }

    fn parse_parameter_status(
        &self,
        length: usize,
        reader: &mut BufferReader<'_>,
    ) -> Result<BackendMessage> {
        let name = reader.cstring()?;
        let value = reader.cstring()?;
        Ok(BackendMessage::ParameterStatus(ParameterStatusMessage {
            length,
            parameter_name: name,
            parameter_value: value,
        }))
    }

    fn parse_backend_key_data(
        &self,
        length: usize,
        reader: &mut BufferReader<'_>,
    ) -> Result<BackendMessage> {
        let process_id = reader.int32()?;
        let secret_key = reader.int32()?;
        Ok(BackendMessage::BackendKeyData(BackendKeyDataMessage {
            length,
            process_id,
            secret_key,
        }))
    }

    fn parse_authentication(
        &self,
        length: usize,
        reader: &mut BufferReader<'_>,
    ) -> Result<BackendMessage> {
        let code = reader.int32()?;
        let message = match code {
            0 => AuthenticationMessage::Ok(AuthenticationOk { length }),
            3 => AuthenticationMessage::Cleartext(AuthenticationCleartextPassword { length }),
            5 => {
                let salt = reader.bytes(4)?;
                AuthenticationMessage::Md5(AuthenticationMD5Password { length, salt })
            }
            10 => {
                let mut mechanisms = Vec::new();
                loop {
                    let mechanism = reader.cstring()?;
                    if mechanism.is_empty() {
                        break;
                    }
                    mechanisms.push(mechanism);
                }
                AuthenticationMessage::Sasl(AuthenticationSasl { length, mechanisms })
            }
            11 => {
                let data = reader.string(length.saturating_sub(8))?;
                AuthenticationMessage::SaslContinue(AuthenticationSaslContinue { length, data })
            }
            12 => {
                let data = reader.string(length.saturating_sub(8))?;
                AuthenticationMessage::SaslFinal(AuthenticationSaslFinal { length, data })
            }
            other => {
                return Err(anyhow!(
                    "Unknown authentication message type {other} (length={length})"
                ));
            }
        };
        Ok(BackendMessage::Authentication(message))
    }

    fn parse_error_message(
        &self,
        length: usize,
        reader: &mut BufferReader<'_>,
    ) -> Result<BackendMessage> {
        let fields = collect_fields(reader)?;
        let message = fields.get("M").cloned().unwrap_or_default();
        let mut error = DatabaseError::new(length, message);
        error.apply_fields(&fields);
        Ok(BackendMessage::Error(error))
    }

    fn parse_notice_message(
        &self,
        length: usize,
        reader: &mut BufferReader<'_>,
    ) -> Result<BackendMessage> {
        let fields = collect_fields(reader)?;
        let message = fields.get("M").cloned();
        let mut notice = NoticeMessage::new(length, message);
        notice.apply_fields(&fields);
        Ok(BackendMessage::Notice(notice))
    }
}
