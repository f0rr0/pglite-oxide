use anyhow::{Result, bail};

use super::postgres_mod::PostgresMod;
use crate::pglite::interface::DataTransferContainer;

/// Protocol transport for the WASIX PGlite backend.
pub enum Transport {
    Wasix,
}

impl Transport {
    pub fn prepare(_pg: &mut PostgresMod) -> Result<Self> {
        Ok(Self::Wasix)
    }

    pub fn send(
        &self,
        pg: &mut PostgresMod,
        payload: &[u8],
        requested: Option<DataTransferContainer>,
    ) -> Result<Vec<u8>> {
        if matches!(requested, Some(DataTransferContainer::File)) {
            bail!("file transport is not implemented for the WASIX backend")
        }
        pg.send_protocol(payload)
    }
}
