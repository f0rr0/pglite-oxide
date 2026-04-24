#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

mod pglite;
mod protocol;

pub use pglite::{
    DataTransferContainer, DescribeQueryParam, DescribeQueryResult, DescribeResultField, FieldInfo,
    GlobalListenerHandle, ListenerHandle, NoticeCallback, ParserMap, Pglite, PgliteBuilder,
    PgliteError, PgliteServer, PgliteServerBuilder, QueryOptions, QueryTemplate, Results, RowMode,
    Serializer, SerializerMap, TemplatedQuery, Transaction, TypeParser, format_query,
    quote_identifier,
};
pub use protocol::messages::{DatabaseError, NoticeMessage};

#[doc(hidden)]
pub use pglite::{
    DebugLevel, InstallOptions, InstallOutcome, MountInfo, PglitePaths, PgliteProxy,
    ensure_cluster, install_and_init, install_and_init_in, install_default,
    install_extension_archive, install_extension_bytes, install_into, install_with_options,
};
