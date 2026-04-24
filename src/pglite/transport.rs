use anyhow::{Context, Result, bail, ensure};
use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use super::postgres_mod::PostgresMod;
use crate::pglite::interface::DataTransferContainer;

/// Mirrors the TypeScript transport abstraction (CMA vs file-backed lock files).
/// Currently only the shared-memory CMA channel is implemented.
pub enum Transport {
    Cma {
        buffer_addr: usize,
        buffer_len: usize,
    },
    #[allow(dead_code)]
    File,
}

impl Transport {
    pub fn from_postgres_mod(pg: &PostgresMod) -> Result<Self> {
        if let (Some(addr), Some(len)) = (pg.buffer_addr(), pg.buffer_len()) {
            Ok(Self::Cma {
                buffer_addr: addr,
                buffer_len: len,
            })
        } else {
            Ok(Self::File)
        }
    }

    pub fn prepare(pg: &mut PostgresMod) -> Result<Self> {
        pg.use_wire(true)?;
        pg.backend()?;
        Self::from_postgres_mod(pg)
    }

    pub fn send(
        &self,
        pg: &mut PostgresMod,
        payload: &[u8],
        requested: Option<DataTransferContainer>,
    ) -> Result<Vec<u8>> {
        match self {
            Transport::Cma {
                buffer_addr,
                buffer_len,
            } => match requested {
                Some(DataTransferContainer::File) => {
                    bail!("file transport is not implemented yet")
                }
                _ => send_cma(pg, *buffer_addr, *buffer_len, payload),
            },
            Transport::File => send_file(pg, payload),
        }
    }
}

fn send_cma(
    pg: &mut PostgresMod,
    buffer_addr: usize,
    buffer_len: usize,
    payload: &[u8],
) -> Result<Vec<u8>> {
    ensure!(
        payload.len() <= buffer_len,
        "payload of {} bytes exceeds CMA buffer ({} bytes)",
        payload.len(),
        buffer_len
    );

    pg.interactive_write(payload.len() as i32)?;
    if !payload.is_empty() {
        pg.write_memory(buffer_addr, payload)?;
    }
    pg.interactive_one()?;

    let available = pg.interactive_read()?;
    if available <= 0 {
        return Ok(Vec::new());
    }

    let response_len = available as usize;
    let response_addr = buffer_addr + payload.len() + 1;
    ensure!(
        response_addr + response_len <= buffer_addr + buffer_len,
        "response range [{}..{}) exceeds CMA buffer [{}..{})",
        response_addr,
        response_addr + response_len,
        buffer_addr,
        buffer_addr + buffer_len
    );

    let mut response = vec![0; response_len];
    pg.read_memory(response_addr, &mut response)?;
    pg.interactive_write(0)?;

    Ok(response)
}

fn send_file(pg: &mut PostgresMod, payload: &[u8]) -> Result<Vec<u8>> {
    let base = pg.paths().pgroot.join("pglite/base");
    let lock_in = base.join(".s.PGSQL.5432.lck.in");
    let in_path = base.join(".s.PGSQL.5432.in");
    let out_path = base.join(".s.PGSQL.5432.out");

    if out_path.exists() {
        let _ = fs::remove_file(&out_path);
    }

    fs::write(&lock_in, payload)
        .with_context(|| format!("write payload to {}", lock_in.display()))?;
    fs::rename(&lock_in, &in_path)
        .with_context(|| format!("rename {} -> {}", lock_in.display(), in_path.display()))?;

    let start = Instant::now();
    let timeout = Duration::from_secs(5);
    loop {
        if out_path.exists() {
            let bytes = fs::read(&out_path)
                .with_context(|| format!("read response from {}", out_path.display()))?;
            let _ = fs::remove_file(&out_path);
            return Ok(bytes);
        }
        if start.elapsed() > timeout {
            bail!("file transport timed out waiting for response");
        }
        thread::sleep(Duration::from_millis(2));
    }
}
