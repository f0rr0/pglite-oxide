use anyhow::{Result, anyhow, bail, ensure};
use pglite_oxide::PgliteProxy;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

const SSL_REQUEST_CODE: i32 = 80_877_103;
const PROTOCOL_3: i32 = 196_608;

#[test]
fn tcp_proxy_handles_psql_style_connections() -> Result<()> {
    let temp_dir = tempfile::TempDir::new()?;
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let addr = listener.local_addr()?;
    let root = temp_dir.path().to_path_buf();

    let handle = thread::spawn(move || PgliteProxy::new(root).accept_tcp_connections(&listener, 2));

    let first = query_proxy(addr, false, "SELECT 1 AS one")?;
    assert_eq!(first, vec!["1"]);

    let second = query_proxy(addr, true, "SELECT 2 AS two")?;
    assert_eq!(second, vec!["2"]);

    handle
        .join()
        .map_err(|_| anyhow!("proxy thread panicked"))??;
    Ok(())
}

fn query_proxy(addr: SocketAddr, request_ssl: bool, sql: &str) -> Result<Vec<String>> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;

    if request_ssl {
        stream.write_all(&ssl_request())?;
        let mut response = [0u8; 1];
        stream.read_exact(&mut response)?;
        ensure!(response[0] == b'N', "expected SSL refusal");
    }

    stream.write_all(&startup_message())?;
    read_until_ready(&mut stream)?;

    stream.write_all(&simple_query_message(sql))?;
    let values = read_query_values(&mut stream)?;

    stream.write_all(&terminate_message())?;
    Ok(values)
}

fn read_until_ready(stream: &mut TcpStream) -> Result<()> {
    loop {
        let (tag, body) = read_backend_message(stream)?;
        match tag {
            b'R' => {
                ensure!(body.len() >= 4, "authentication message too short");
                let code = i32::from_be_bytes(body[0..4].try_into().unwrap());
                ensure!(code == 0, "unexpected authentication code {code}");
            }
            b'E' => bail!("startup error: {}", error_message(&body)),
            b'Z' => return Ok(()),
            _ => {}
        }
    }
}

fn read_query_values(stream: &mut TcpStream) -> Result<Vec<String>> {
    let mut values = Vec::new();
    loop {
        let (tag, body) = read_backend_message(stream)?;
        match tag {
            b'D' => values.extend(data_row_values(&body)?),
            b'E' => bail!("query error: {}", error_message(&body)),
            b'Z' => return Ok(values),
            _ => {}
        }
    }
}

fn read_backend_message(stream: &mut TcpStream) -> Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    stream.read_exact(&mut header)?;
    let len = i32::from_be_bytes(header[1..5].try_into().unwrap());
    ensure!(len >= 4, "invalid backend message length {len}");
    let mut body = vec![0u8; len as usize - 4];
    stream.read_exact(&mut body)?;
    Ok((header[0], body))
}

fn data_row_values(body: &[u8]) -> Result<Vec<String>> {
    ensure!(body.len() >= 2, "data row too short");
    let count = i16::from_be_bytes(body[0..2].try_into().unwrap()) as usize;
    let mut offset = 2usize;
    let mut values = Vec::with_capacity(count);

    for _ in 0..count {
        ensure!(offset + 4 <= body.len(), "data row field length missing");
        let len = i32::from_be_bytes(body[offset..offset + 4].try_into().unwrap());
        offset += 4;
        if len < 0 {
            values.push(String::new());
            continue;
        }
        let len = len as usize;
        ensure!(
            offset + len <= body.len(),
            "data row field overruns message"
        );
        values.push(std::str::from_utf8(&body[offset..offset + len])?.to_string());
        offset += len;
    }

    Ok(values)
}

fn error_message(body: &[u8]) -> String {
    let mut offset = 0usize;
    while offset < body.len() {
        let code = body[offset];
        if code == 0 {
            break;
        }
        offset += 1;
        let Some(end) = body[offset..].iter().position(|byte| *byte == 0) else {
            break;
        };
        if code == b'M' {
            return String::from_utf8_lossy(&body[offset..offset + end]).to_string();
        }
        offset += end + 1;
    }
    String::from_utf8_lossy(body).to_string()
}

fn ssl_request() -> Vec<u8> {
    let mut message = Vec::new();
    message.extend_from_slice(&8_i32.to_be_bytes());
    message.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
    message
}

fn startup_message() -> Vec<u8> {
    let mut message = Vec::new();
    message.extend_from_slice(&0_i32.to_be_bytes());
    message.extend_from_slice(&PROTOCOL_3.to_be_bytes());
    for (key, value) in [
        ("user", "postgres"),
        ("database", "template1"),
        ("application_name", "pglite-oxide-test"),
    ] {
        message.extend_from_slice(key.as_bytes());
        message.push(0);
        message.extend_from_slice(value.as_bytes());
        message.push(0);
    }
    message.push(0);
    let len = message.len() as i32;
    message[0..4].copy_from_slice(&len.to_be_bytes());
    message
}

fn simple_query_message(sql: &str) -> Vec<u8> {
    let mut message = Vec::with_capacity(sql.len() + 6);
    message.push(b'Q');
    message.extend_from_slice(&((sql.len() + 5) as i32).to_be_bytes());
    message.extend_from_slice(sql.as_bytes());
    message.push(0);
    message
}

fn terminate_message() -> Vec<u8> {
    let mut message = Vec::new();
    message.push(b'X');
    message.extend_from_slice(&4_i32.to_be_bytes());
    message
}
