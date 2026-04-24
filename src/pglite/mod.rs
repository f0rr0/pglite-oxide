pub(crate) mod base;
pub(crate) mod builder;
pub(crate) mod client;
pub(crate) mod errors;
pub(crate) mod interface;
pub(crate) mod parse;
pub(crate) mod postgres_mod;
pub(crate) mod proxy;
pub(crate) mod server;
pub(crate) mod templating;
pub(crate) mod transport;
pub(crate) mod types;

pub use base::{
    InstallOptions, InstallOutcome, MountInfo, PglitePaths, ensure_cluster, install_and_init,
    install_and_init_in, install_default, install_extension_archive, install_extension_bytes,
    install_into, install_with_options,
};
pub use builder::PgliteBuilder;
pub use client::{GlobalListenerHandle, ListenerHandle, Pglite, Transaction};
pub use errors::PgliteError;
pub use interface::{
    DataTransferContainer, DebugLevel, DescribeQueryParam, DescribeQueryResult,
    DescribeResultField, FieldInfo, NoticeCallback, ParserMap, QueryOptions, Results, RowMode,
    Serializer, SerializerMap, TypeParser,
};
pub use proxy::PgliteProxy;
pub use server::{PgliteServer, PgliteServerBuilder};
pub use templating::{QueryTemplate, TemplatedQuery, format_query, quote_identifier};
