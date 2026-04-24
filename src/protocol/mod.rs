#![allow(dead_code)]
// The internal protocol layer keeps full PostgreSQL message shapes even when
// the high-level API only consumes a subset of each message today.

pub(crate) mod buffer_reader;
pub(crate) mod buffer_writer;
pub(crate) mod messages;
pub(crate) mod parser;
pub(crate) mod serializer;
pub(crate) mod string_utils;
pub(crate) mod types;

#[cfg(test)]
mod tests;

#[cfg(test)]
use messages::{AuthenticationMessage, BackendMessage, Field, MessageName};
#[cfg(test)]
use parser::Parser;
#[cfg(test)]
use serializer::{BindConfig, ExecConfig, PortalTarget, Serialize};
#[cfg(test)]
use string_utils::byte_length_utf8;
#[cfg(test)]
use types::Mode;
