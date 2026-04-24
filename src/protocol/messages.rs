use std::fmt;

use anyhow::Result;

use crate::protocol::types::Mode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageName {
    ParseComplete,
    BindComplete,
    CloseComplete,
    NoData,
    PortalSuspended,
    ReplicationStart,
    EmptyQuery,
    CopyDone,
    CopyData,
    RowDescription,
    ParameterDescription,
    ParameterStatus,
    BackendKeyData,
    Notification,
    ReadyForQuery,
    CommandComplete,
    DataRow,
    CopyInResponse,
    CopyOutResponse,
    AuthenticationOk,
    AuthenticationMD5Password,
    AuthenticationCleartextPassword,
    AuthenticationSasl,
    AuthenticationSaslContinue,
    AuthenticationSaslFinal,
    Error,
    Notice,
}

impl fmt::Display for MessageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use MessageName::*;
        let name = match self {
            ParseComplete => "parseComplete",
            BindComplete => "bindComplete",
            CloseComplete => "closeComplete",
            NoData => "noData",
            PortalSuspended => "portalSuspended",
            ReplicationStart => "replicationStart",
            EmptyQuery => "emptyQuery",
            CopyDone => "copyDone",
            CopyData => "copyData",
            RowDescription => "rowDescription",
            ParameterDescription => "parameterDescription",
            ParameterStatus => "parameterStatus",
            BackendKeyData => "backendKeyData",
            Notification => "notification",
            ReadyForQuery => "readyForQuery",
            CommandComplete => "commandComplete",
            DataRow => "dataRow",
            CopyInResponse => "copyInResponse",
            CopyOutResponse => "copyOutResponse",
            AuthenticationOk => "authenticationOk",
            AuthenticationMD5Password => "authenticationMD5Password",
            AuthenticationCleartextPassword => "authenticationCleartextPassword",
            AuthenticationSasl => "authenticationSASL",
            AuthenticationSaslContinue => "authenticationSASLContinue",
            AuthenticationSaslFinal => "authenticationSASLFinal",
            Error => "error",
            Notice => "notice",
        };
        write!(f, "{name}")
    }
}

#[derive(Debug, Clone)]
pub enum BackendMessage {
    ParseComplete { length: usize },
    BindComplete { length: usize },
    CloseComplete { length: usize },
    NoData { length: usize },
    PortalSuspended { length: usize },
    ReplicationStart { length: usize },
    EmptyQuery { length: usize },
    CopyDone { length: usize },
    ReadyForQuery(ReadyForQueryMessage),
    CommandComplete(CommandCompleteMessage),
    DataRow(DataRowMessage),
    RowDescription(RowDescriptionMessage),
    ParameterDescription(ParameterDescriptionMessage),
    ParameterStatus(ParameterStatusMessage),
    BackendKeyData(BackendKeyDataMessage),
    Notification(NotificationResponseMessage),
    CopyResponse(CopyResponse),
    CopyData(CopyDataMessage),
    Authentication(AuthenticationMessage),
    Error(DatabaseError),
    Notice(NoticeMessage),
}

impl BackendMessage {
    pub fn name(&self) -> MessageName {
        use BackendMessage::*;
        match self {
            ParseComplete { .. } => MessageName::ParseComplete,
            BindComplete { .. } => MessageName::BindComplete,
            CloseComplete { .. } => MessageName::CloseComplete,
            NoData { .. } => MessageName::NoData,
            PortalSuspended { .. } => MessageName::PortalSuspended,
            ReplicationStart { .. } => MessageName::ReplicationStart,
            EmptyQuery { .. } => MessageName::EmptyQuery,
            CopyDone { .. } => MessageName::CopyDone,
            ReadyForQuery(_) => MessageName::ReadyForQuery,
            CommandComplete(_) => MessageName::CommandComplete,
            DataRow(_) => MessageName::DataRow,
            RowDescription(_) => MessageName::RowDescription,
            ParameterDescription(_) => MessageName::ParameterDescription,
            ParameterStatus(_) => MessageName::ParameterStatus,
            BackendKeyData(_) => MessageName::BackendKeyData,
            Notification(_) => MessageName::Notification,
            CopyResponse(resp) => match resp.name {
                MessageName::CopyInResponse => MessageName::CopyInResponse,
                MessageName::CopyOutResponse => MessageName::CopyOutResponse,
                _ => resp.name,
            },
            CopyData(_) => MessageName::CopyData,
            Authentication(auth) => auth.name(),
            Error(_) => MessageName::Error,
            Notice(_) => MessageName::Notice,
        }
    }

    pub fn length(&self) -> usize {
        use BackendMessage::*;
        match self {
            ParseComplete { length }
            | BindComplete { length }
            | CloseComplete { length }
            | NoData { length }
            | PortalSuspended { length }
            | ReplicationStart { length }
            | EmptyQuery { length }
            | CopyDone { length } => *length,
            ReadyForQuery(msg) => msg.length,
            CommandComplete(msg) => msg.length,
            DataRow(msg) => msg.length,
            RowDescription(msg) => msg.length,
            ParameterDescription(msg) => msg.length,
            ParameterStatus(msg) => msg.length,
            BackendKeyData(msg) => msg.length,
            Notification(msg) => msg.length,
            CopyResponse(msg) => msg.length,
            CopyData(msg) => msg.length,
            Authentication(msg) => msg.length(),
            Error(msg) => msg.length,
            Notice(msg) => msg.length,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReadyForQueryMessage {
    pub length: usize,
    pub status: u8,
}

#[derive(Debug, Clone)]
pub struct CommandCompleteMessage {
    pub length: usize,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct CopyDataMessage {
    pub length: usize,
    pub chunk: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct CopyResponse {
    pub length: usize,
    pub name: MessageName,
    pub binary: bool,
    pub column_types: Vec<i16>,
}

#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub table_id: i32,
    pub column_id: i16,
    pub data_type_id: i32,
    pub data_type_size: i16,
    pub data_type_modifier: i32,
    pub format: Mode,
}

#[derive(Debug, Clone)]
pub struct RowDescriptionMessage {
    pub length: usize,
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone)]
pub struct ParameterDescriptionMessage {
    pub length: usize,
    pub data_type_ids: Vec<i32>,
}

#[derive(Debug, Clone)]
pub struct ParameterStatusMessage {
    pub length: usize,
    pub parameter_name: String,
    pub parameter_value: String,
}

#[derive(Debug, Clone)]
pub struct BackendKeyDataMessage {
    pub length: usize,
    pub process_id: i32,
    pub secret_key: i32,
}

#[derive(Debug, Clone)]
pub struct NotificationResponseMessage {
    pub length: usize,
    pub process_id: i32,
    pub channel: String,
    pub payload: String,
}

#[derive(Debug, Clone)]
pub struct CommandTag(pub String);

#[derive(Debug, Clone)]
pub struct DataRowMessage {
    pub length: usize,
    pub fields: Vec<Option<String>>,
}

pub trait NoticeOrErrorFields {
    fn apply_fields(&mut self, fields: &std::collections::HashMap<String, String>);
}

#[derive(Debug, Clone)]
pub struct NoticeMessage {
    pub length: usize,
    pub message: Option<String>,
    pub severity: Option<String>,
    pub code: Option<String>,
    pub detail: Option<String>,
    pub hint: Option<String>,
    pub position: Option<String>,
    pub internal_position: Option<String>,
    pub internal_query: Option<String>,
    pub r#where: Option<String>,
    pub schema: Option<String>,
    pub table: Option<String>,
    pub column: Option<String>,
    pub data_type: Option<String>,
    pub constraint: Option<String>,
    pub file: Option<String>,
    pub line: Option<String>,
    pub routine: Option<String>,
}

impl NoticeMessage {
    pub fn new(length: usize, message: Option<String>) -> Self {
        Self {
            length,
            message,
            severity: None,
            code: None,
            detail: None,
            hint: None,
            position: None,
            internal_position: None,
            internal_query: None,
            r#where: None,
            schema: None,
            table: None,
            column: None,
            data_type: None,
            constraint: None,
            file: None,
            line: None,
            routine: None,
        }
    }
}

impl NoticeOrErrorFields for NoticeMessage {
    fn apply_fields(&mut self, fields: &std::collections::HashMap<String, String>) {
        self.severity = fields.get("S").cloned();
        self.code = fields.get("C").cloned();
        self.detail = fields.get("D").cloned();
        self.hint = fields.get("H").cloned();
        self.position = fields.get("P").cloned();
        self.internal_position = fields.get("p").cloned();
        self.internal_query = fields.get("q").cloned();
        self.r#where = fields.get("W").cloned();
        self.schema = fields.get("s").cloned();
        self.table = fields.get("t").cloned();
        self.column = fields.get("c").cloned();
        self.data_type = fields.get("d").cloned();
        self.constraint = fields.get("n").cloned();
        self.file = fields.get("F").cloned();
        self.line = fields.get("L").cloned();
        self.routine = fields.get("R").cloned();
    }
}

#[derive(Debug, Clone)]
pub struct DatabaseError {
    pub length: usize,
    pub message: String,
    pub severity: Option<String>,
    pub code: Option<String>,
    pub detail: Option<String>,
    pub hint: Option<String>,
    pub position: Option<String>,
    pub internal_position: Option<String>,
    pub internal_query: Option<String>,
    pub r#where: Option<String>,
    pub schema: Option<String>,
    pub table: Option<String>,
    pub column: Option<String>,
    pub data_type: Option<String>,
    pub constraint: Option<String>,
    pub file: Option<String>,
    pub line: Option<String>,
    pub routine: Option<String>,
}

impl DatabaseError {
    pub fn new(length: usize, message: String) -> Self {
        Self {
            length,
            message,
            severity: None,
            code: None,
            detail: None,
            hint: None,
            position: None,
            internal_position: None,
            internal_query: None,
            r#where: None,
            schema: None,
            table: None,
            column: None,
            data_type: None,
            constraint: None,
            file: None,
            line: None,
            routine: None,
        }
    }
}

impl std::fmt::Display for DatabaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for DatabaseError {}

impl NoticeOrErrorFields for DatabaseError {
    fn apply_fields(&mut self, fields: &std::collections::HashMap<String, String>) {
        self.severity = fields.get("S").cloned();
        self.code = fields.get("C").cloned();
        self.detail = fields.get("D").cloned();
        self.hint = fields.get("H").cloned();
        self.position = fields.get("P").cloned();
        self.internal_position = fields.get("p").cloned();
        self.internal_query = fields.get("q").cloned();
        self.r#where = fields.get("W").cloned();
        self.schema = fields.get("s").cloned();
        self.table = fields.get("t").cloned();
        self.column = fields.get("c").cloned();
        self.data_type = fields.get("d").cloned();
        self.constraint = fields.get("n").cloned();
        self.file = fields.get("F").cloned();
        self.line = fields.get("L").cloned();
        self.routine = fields.get("R").cloned();
    }
}

#[derive(Debug, Clone)]
pub struct AuthenticationOk {
    pub length: usize,
}

#[derive(Debug, Clone)]
pub struct AuthenticationCleartextPassword {
    pub length: usize,
}

#[derive(Debug, Clone)]
pub struct AuthenticationMD5Password {
    pub length: usize,
    pub salt: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct AuthenticationSasl {
    pub length: usize,
    pub mechanisms: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AuthenticationSaslContinue {
    pub length: usize,
    pub data: String,
}

#[derive(Debug, Clone)]
pub struct AuthenticationSaslFinal {
    pub length: usize,
    pub data: String,
}

#[derive(Debug, Clone)]
pub enum AuthenticationMessage {
    Ok(AuthenticationOk),
    Cleartext(AuthenticationCleartextPassword),
    Md5(AuthenticationMD5Password),
    Sasl(AuthenticationSasl),
    SaslContinue(AuthenticationSaslContinue),
    SaslFinal(AuthenticationSaslFinal),
}

impl AuthenticationMessage {
    pub fn name(&self) -> MessageName {
        use AuthenticationMessage::*;
        match self {
            Ok(_) => MessageName::AuthenticationOk,
            Cleartext(_) => MessageName::AuthenticationCleartextPassword,
            Md5(_) => MessageName::AuthenticationMD5Password,
            Sasl(_) => MessageName::AuthenticationSasl,
            SaslContinue(_) => MessageName::AuthenticationSaslContinue,
            SaslFinal(_) => MessageName::AuthenticationSaslFinal,
        }
    }

    pub fn length(&self) -> usize {
        use AuthenticationMessage::*;
        match self {
            Ok(msg) => msg.length,
            Cleartext(msg) => msg.length,
            Md5(msg) => msg.length,
            Sasl(msg) => msg.length,
            SaslContinue(msg) => msg.length,
            SaslFinal(msg) => msg.length,
        }
    }
}

pub fn collect_fields(
    reader: &mut crate::protocol::buffer_reader::BufferReader<'_>,
) -> Result<std::collections::HashMap<String, String>> {
    use std::collections::HashMap;
    let mut map = HashMap::new();
    loop {
        let field_type = reader.string(1)?;
        if field_type == "\0" {
            break;
        }
        let value = reader.cstring()?;
        map.insert(field_type, value);
    }
    Ok(map)
}
