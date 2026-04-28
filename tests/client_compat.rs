use anyhow::{Context, Result};
use pglite_oxide::PgliteServer;
use sqlx::{Connection, Executor, Row};
use std::io::{Read, Write};
use std::net::TcpStream;
use tokio::time::{Duration, timeout};
use tokio_postgres::NoTls;

const SSL_REQUEST_CODE: i32 = 80_877_103;
const CANCEL_REQUEST_CODE: i32 = 80_877_102;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tokio_postgres_extended_query_works() -> Result<()> {
    let server = PgliteServer::temporary_tcp()?;
    let (client, connection) = tokio_postgres::connect(&server.connection_uri(), NoTls)
        .await
        .context("connect with tokio-postgres")?;
    let connection_task = tokio::spawn(connection);

    let row = client
        .query_one("SELECT $1::int4 + 1 AS answer", &[&41_i32])
        .await
        .context("run tokio-postgres parameter query")?;
    assert_eq!(row.get::<_, i32>("answer"), 42);

    client
        .batch_execute(
            "CREATE TABLE items(value TEXT);
             INSERT INTO items(value) VALUES ('alpha');",
        )
        .await?;
    let row = client
        .query_one("SELECT value FROM items WHERE value = $1", &[&"alpha"])
        .await
        .context("run tokio-postgres table query")?;
    assert_eq!(row.get::<_, &str>(0), "alpha");

    drop(client);
    wait_for_tokio_postgres(connection_task).await?;
    server.shutdown()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tokio_postgres_extended_query_errors_recover_after_sync() -> Result<()> {
    let server = PgliteServer::temporary_tcp()?;
    let (client, connection) = tokio_postgres::connect(&server.connection_uri(), NoTls)
        .await
        .context("connect with tokio-postgres")?;
    let connection_task = tokio::spawn(connection);

    let parse_err = client
        .query_one("SELECT missing FROM missing_table WHERE id = $1", &[&7_i32])
        .await
        .expect_err("undefined table should fail during extended-query parse");
    assert_eq!(
        parse_err.code().map(|code| code.code()),
        Some("42P01"),
        "undefined table should preserve SQLSTATE"
    );
    let row = client
        .query_one("SELECT 11::int4 AS recovered_after_parse", &[])
        .await
        .context("query after parse error")?;
    assert_eq!(row.get::<_, i32>("recovered_after_parse"), 11);

    let execute_err = client
        .query_one("SELECT 10 / $1::int4 AS impossible", &[&0_i32])
        .await
        .expect_err("division by zero should fail during extended-query execute");
    assert_eq!(
        execute_err.code().map(|code| code.code()),
        Some("22012"),
        "execute error should preserve SQLSTATE"
    );
    let row = client
        .query_one("SELECT 12::int4 AS recovered_after_execute", &[])
        .await
        .context("query after execute error")?;
    assert_eq!(row.get::<_, i32>("recovered_after_execute"), 12);

    drop(client);
    wait_for_tokio_postgres(connection_task).await?;
    server.shutdown()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlx_bind_errors_recover_after_sync() -> Result<()> {
    let server = PgliteServer::temporary_tcp()?;
    let mut conn = sqlx::PgConnection::connect(&server.connection_uri())
        .await
        .context("connect with SQLx")?;

    let invalid_int = sqlx::query("SELECT $1::int4 AS value")
        .bind("not_an_int")
        .fetch_one(&mut conn)
        .await
        .expect_err("invalid int text should fail while binding a typed parameter");
    assert_sqlx_code(&invalid_int, "22P02");
    let row = sqlx::query("SELECT 31::int4 AS recovered_after_invalid_bind")
        .fetch_one(&mut conn)
        .await
        .context("query after invalid bind value")?;
    assert_eq!(row.try_get::<i32, _>("recovered_after_invalid_bind")?, 31);

    let wrong_param_count = sqlx::query("SELECT $1::int4 + $2::int4 AS value")
        .bind(1_i32)
        .fetch_one(&mut conn)
        .await
        .expect_err("missing parameter should fail during extended-query bind");
    assert_sqlx_code(&wrong_param_count, "08P01");
    let row = sqlx::query("SELECT 32::int4 AS recovered_after_param_count")
        .fetch_one(&mut conn)
        .await
        .context("query after wrong parameter count")?;
    assert_eq!(row.try_get::<i32, _>("recovered_after_param_count")?, 32);

    conn.close().await?;
    server.shutdown()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tokio_postgres_pipelined_extended_queries_keep_ready_state() -> Result<()> {
    let server = PgliteServer::temporary_tcp()?;
    let (client, connection) = tokio_postgres::connect(&server.connection_uri(), NoTls)
        .await
        .context("connect with tokio-postgres")?;
    let connection_task = tokio::spawn(connection);

    let first = client.query_one("SELECT $1::int4 AS value", &[&10_i32]);
    let second = client.query_one("SELECT $1::int4 + 1 AS value", &[&41_i32]);
    let (first, second) = tokio::try_join!(first, second).context("run pipelined queries")?;

    assert_eq!(first.get::<_, i32>("value"), 10);
    assert_eq!(second.get::<_, i32>("value"), 42);

    drop(client);
    wait_for_tokio_postgres(connection_task).await?;
    server.shutdown()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tokio_postgres_mixed_pipelined_success_error_success_recovers() -> Result<()> {
    let server = PgliteServer::temporary_tcp()?;
    let (client, connection) = tokio_postgres::connect(&server.connection_uri(), NoTls)
        .await
        .context("connect with tokio-postgres")?;
    let connection_task = tokio::spawn(connection);

    let first = client.query_one("SELECT $1::int4 AS value", &[&1_i32]);
    let second = client.query_one("SELECT 10 / $1::int4 AS value", &[&0_i32]);
    let third = client.query_one("SELECT $1::int4 AS value", &[&3_i32]);
    let (first, second, third) = tokio::join!(first, second, third);

    assert_eq!(first?.get::<_, i32>("value"), 1);
    let second = second.expect_err("middle pipelined query should fail");
    assert_eq!(second.code().map(|code| code.code()), Some("22012"));
    assert_eq!(third?.get::<_, i32>("value"), 3);

    let row = client
        .query_one("SELECT 4::int4 AS recovered_after_pipeline", &[])
        .await?;
    assert_eq!(row.get::<_, i32>("recovered_after_pipeline"), 4);

    drop(client);
    wait_for_tokio_postgres(connection_task).await?;
    server.shutdown()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_wire_protocol_bind_errors_are_synchronized() -> Result<()> {
    let server = PgliteServer::temporary_tcp()?;
    let addr = server.tcp_addr().context("server should use TCP")?;

    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut stream = TcpStream::connect(addr).context("connect raw protocol socket")?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;

        stream
            .write_all(&startup_message())
            .context("write startup message")?;
        let startup = read_until_ready(&mut stream).context("read startup response")?;
        assert!(
            startup.iter().any(|msg| msg.tag == b'R'),
            "startup should include AuthenticationOk"
        );
        assert_eq!(startup.last().map(|msg| msg.tag), Some(b'Z'));

        stream
            .write_all(
                &[
                    parse_statement("typed_int", "SELECT $1::int4 AS value"),
                    sync(),
                ]
                .concat(),
            )
            .context("write Parse + Sync")?;
        let parsed = read_until_ready(&mut stream).context("read Parse response")?;
        assert_message_tags(&parsed, &[b'1', b'Z']);

        stream
            .write_all(
                &[
                    bind_statement("", "typed_int", &["not_an_int"]),
                    describe_portal(""),
                    execute_portal(""),
                    sync(),
                ]
                .concat(),
            )
            .context("write invalid Bind batch")?;
        let invalid_bind = read_until_ready(&mut stream).context("read invalid Bind response")?;
        assert_eq!(invalid_bind.last().map(|msg| msg.tag), Some(b'Z'));
        assert!(
            invalid_bind.iter().all(|msg| msg.tag != b'2'),
            "invalid Bind must not emit BindComplete"
        );
        assert_eq!(first_error_code(&invalid_bind).as_deref(), Some("22P02"));

        stream
            .write_all(&[bind_statement("", "typed_int", &[]), sync()].concat())
            .context("write wrong parameter count Bind batch")?;
        let wrong_count = read_until_ready(&mut stream).context("read wrong Bind response")?;
        assert_eq!(wrong_count.last().map(|msg| msg.tag), Some(b'Z'));
        assert!(
            wrong_count.iter().all(|msg| msg.tag != b'2'),
            "wrong parameter count must not emit BindComplete"
        );
        assert_eq!(first_error_code(&wrong_count).as_deref(), Some("08P01"));

        stream
            .write_all(&query_message("SELECT 33::int4 AS recovered"))
            .context("write recovery query")?;
        let recovered = read_until_ready(&mut stream).context("read recovery query")?;
        assert!(
            recovered.iter().any(|msg| msg.tag == b'T')
                && recovered.iter().any(|msg| msg.tag == b'D')
                && recovered.iter().any(|msg| msg.tag == b'C')
                && recovered.last().is_some_and(|msg| msg.tag == b'Z'),
            "connection should recover after raw Bind errors"
        );

        Ok(())
    })
    .await??;

    server.shutdown()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_wire_protocol_handles_partial_reads_and_pipelined_simple_queries() -> Result<()> {
    let server = PgliteServer::temporary_tcp()?;
    let addr = server.tcp_addr().context("server should use TCP")?;

    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut stream = TcpStream::connect(addr).context("connect raw protocol socket")?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;

        write_in_small_chunks(&mut stream, &startup_message(), 3)?;
        let startup = read_until_ready(&mut stream).context("read startup response")?;
        assert_eq!(startup.last().map(|msg| msg.tag), Some(b'Z'));

        write_in_small_chunks(
            &mut stream,
            &query_message(
                "CREATE TABLE partial_items(value text);
                 INSERT INTO partial_items(value) VALUES ('alpha'), ('beta');
                 SELECT count(*)::int4 AS count FROM partial_items",
            ),
            5,
        )?;
        let first = read_until_ready(&mut stream).context("read split simple-query response")?;
        assert!(
            first.iter().any(|msg| msg.tag == b'D'),
            "split query should return a DataRow"
        );
        assert_eq!(first.last().map(|msg| msg.tag), Some(b'Z'));

        write_in_small_chunks(
            &mut stream,
            &[
                query_message("SELECT value FROM partial_items WHERE value = 'alpha'"),
                query_message("SELECT value FROM partial_items WHERE value = 'beta'"),
            ]
            .concat(),
            4,
        )?;
        let first_pipelined =
            read_until_ready(&mut stream).context("read first pipelined query")?;
        let second_pipelined =
            read_until_ready(&mut stream).context("read second pipelined query")?;
        assert!(
            first_pipelined.iter().any(|msg| msg.tag == b'D')
                && second_pipelined.iter().any(|msg| msg.tag == b'D'),
            "both pipelined simple queries should return rows"
        );
        Ok(())
    })
    .await??;

    server.shutdown()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_wire_copy_from_stdin_is_rejected_without_closing_connection() -> Result<()> {
    let server = PgliteServer::temporary_tcp()?;
    let addr = server.tcp_addr().context("server should use TCP")?;

    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut stream = TcpStream::connect(addr).context("connect raw protocol socket")?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;

        stream.write_all(&startup_message())?;
        let startup = read_until_ready(&mut stream).context("read startup response")?;
        assert_eq!(startup.last().map(|msg| msg.tag), Some(b'Z'));

        stream.write_all(&query_message("CREATE TABLE copy_items(value text)"))?;
        let created = read_until_ready(&mut stream).context("read create table response")?;
        assert!(created.iter().any(|msg| msg.tag == b'C'));

        write_in_small_chunks(
            &mut stream,
            &query_message("COPY copy_items(value) FROM STDIN"),
            3,
        )?;
        let rejected = read_until_ready(&mut stream).context("read COPY rejection")?;
        assert_eq!(first_error_code(&rejected).as_deref(), Some("0A000"));
        assert_eq!(rejected.last().map(|msg| msg.tag), Some(b'Z'));

        stream.write_all(&query_message("SELECT 42::int4 AS recovered"))?;
        let recovered = read_until_ready(&mut stream).context("read recovery query")?;
        assert!(
            recovered.iter().any(|msg| msg.tag == b'D'),
            "connection should remain usable after COPY FROM STDIN rejection"
        );

        Ok(())
    })
    .await??;

    server.shutdown()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_wire_disconnect_during_extended_query_does_not_poison_backend() -> Result<()> {
    let server = PgliteServer::temporary_tcp()?;
    let addr = server.tcp_addr().context("server should use TCP")?;

    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut stream = TcpStream::connect(addr).context("connect raw protocol socket")?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        stream.write_all(&startup_message())?;
        let startup = read_until_ready(&mut stream).context("read startup response")?;
        assert_eq!(startup.last().map(|msg| msg.tag), Some(b'Z'));

        stream.write_all(&parse_statement("will_disconnect", "SELECT $1::int4"))?;
        drop(stream);
        Ok(())
    })
    .await??;

    let mut conn = sqlx::PgConnection::connect(&server.connection_uri()).await?;
    let row = sqlx::query("SELECT 41::int4 + 1 AS answer")
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(row.try_get::<i32, _>("answer")?, 42);
    conn.close().await?;
    server.shutdown()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlx_query_works() -> Result<()> {
    let server = PgliteServer::temporary_tcp()?;
    let mut conn = sqlx::PgConnection::connect(&server.connection_uri())
        .await
        .context("connect with SQLx")?;

    let row = sqlx::query("SELECT $1::int4 + 1 AS answer")
        .bind(41_i32)
        .fetch_one(&mut conn)
        .await
        .context("run SQLx parameter query")?;
    assert_eq!(row.try_get::<i32, _>("answer")?, 42);

    sqlx::query("CREATE TABLE items(value TEXT)")
        .execute(&mut conn)
        .await?;
    sqlx::query("INSERT INTO items(value) VALUES ($1)")
        .bind("alpha")
        .execute(&mut conn)
        .await?;
    let row = sqlx::query("SELECT value FROM items WHERE value = $1")
        .bind("alpha")
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(row.try_get::<String, _>("value")?, "alpha");

    conn.close().await?;
    server.shutdown()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tokio_postgres_prepared_statement_reuse_works() -> Result<()> {
    let server = PgliteServer::temporary_tcp()?;
    let (client, connection) = tokio_postgres::connect(&server.connection_uri(), NoTls)
        .await
        .context("connect with tokio-postgres")?;
    let connection_task = tokio::spawn(connection);

    let statement = client.prepare("SELECT $1::int4 + $2::int4 AS sum").await?;
    for value in [1_i32, 10, 40] {
        let row = client.query_one(&statement, &[&value, &2_i32]).await?;
        assert_eq!(row.get::<_, i32>("sum"), value + 2);
    }

    drop(client);
    wait_for_tokio_postgres(connection_task).await?;
    server.shutdown()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlx_transaction_error_recovers_after_rollback() -> Result<()> {
    let server = PgliteServer::temporary_tcp()?;
    let mut conn = sqlx::PgConnection::connect(&server.connection_uri())
        .await
        .context("connect with SQLx")?;

    conn.execute("BEGIN").await?;
    let err = sqlx::query("SELECT 10 / $1::int4 AS impossible")
        .bind(0_i32)
        .fetch_one(&mut conn)
        .await
        .expect_err("transaction query should fail");
    assert_sqlx_code(&err, "22012");

    let aborted = sqlx::query("SELECT 1::int4 AS still_aborted")
        .fetch_one(&mut conn)
        .await
        .expect_err("transaction should stay aborted until rollback");
    assert_sqlx_code(&aborted, "25P02");

    conn.execute("ROLLBACK").await?;
    let row = sqlx::query("SELECT 42::int4 AS recovered")
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(row.try_get::<i32, _>("recovered")?, 42);

    conn.close().await?;
    server.shutdown()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlx_extended_query_errors_recover_after_sync() -> Result<()> {
    let server = PgliteServer::temporary_tcp()?;
    let mut conn = sqlx::PgConnection::connect(&server.connection_uri())
        .await
        .context("connect with SQLx")?;

    let parse_err = sqlx::query("SELECT missing FROM missing_table WHERE id = $1")
        .bind(7_i32)
        .fetch_one(&mut conn)
        .await
        .expect_err("undefined table should fail during extended-query parse");
    assert_sqlx_code(&parse_err, "42P01");
    let row = sqlx::query("SELECT 21::int4 AS recovered_after_parse")
        .fetch_one(&mut conn)
        .await
        .context("query after parse error")?;
    assert_eq!(row.try_get::<i32, _>("recovered_after_parse")?, 21);

    let execute_err = sqlx::query("SELECT 10 / $1::int4 AS impossible")
        .bind(0_i32)
        .fetch_one(&mut conn)
        .await
        .expect_err("division by zero should fail during extended-query execute");
    assert_sqlx_code(&execute_err, "22012");
    let row = sqlx::query("SELECT 22::int4 AS recovered_after_execute")
        .fetch_one(&mut conn)
        .await
        .context("query after execute error")?;
    assert_eq!(row.try_get::<i32, _>("recovered_after_execute")?, 22);

    conn.close().await?;
    server.shutdown()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlx_simple_query_timezone_errors_recover() -> Result<()> {
    let server = PgliteServer::temporary_tcp()?;
    let mut conn = sqlx::PgConnection::connect(&server.connection_uri())
        .await
        .context("connect with SQLx")?;

    let timezone = sqlx::query("SELECT current_setting('TimeZone') AS timezone")
        .fetch_one(&mut conn)
        .await
        .context("read default timezone")?;
    assert_eq!(timezone.try_get::<String, _>("timezone")?, "UTC");

    conn.execute("SET TIME ZONE 'America/New_York'")
        .await
        .context("set named timezone")?;
    let row = sqlx::query(
        "SELECT current_setting('TimeZone') AS timezone, \
                count(*)::int4 AS matching_zones \
         FROM pg_timezone_names \
         WHERE name = 'America/New_York' \
         GROUP BY 1",
    )
    .fetch_one(&mut conn)
    .await
    .context("query timezone catalog")?;
    assert_eq!(row.try_get::<String, _>("timezone")?, "America/New_York");
    assert_eq!(row.try_get::<i32, _>("matching_zones")?, 1);

    conn.execute("SET TIME ZONE 'Missing/Zone'")
        .await
        .expect_err("invalid timezone should fail");
    let row = sqlx::query("SELECT 24::int4 AS recovered_after_timezone_error")
        .fetch_one(&mut conn)
        .await
        .context("query after invalid timezone")?;
    assert_eq!(row.try_get::<i32, _>("recovered_after_timezone_error")?, 24);

    conn.close().await?;

    let mut next_conn = sqlx::PgConnection::connect(&server.connection_uri())
        .await
        .context("reconnect with SQLx")?;
    let row = sqlx::query("SELECT current_setting('TimeZone') AS timezone")
        .fetch_one(&mut next_conn)
        .await
        .context("read timezone after connection cleanup")?;
    assert_eq!(row.try_get::<String, _>("timezone")?, "UTC");
    next_conn.close().await?;

    server.shutdown()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_control_packets_are_handled_safely() -> Result<()> {
    let server = PgliteServer::temporary_tcp()?;
    let addr = server.tcp_addr().context("server should use TCP")?;

    let ssl_response = tokio::task::spawn_blocking(move || -> Result<u8> {
        let mut stream = TcpStream::connect(addr).context("connect raw SSLRequest socket")?;
        stream
            .write_all(&startup_control_packet(SSL_REQUEST_CODE, &[]))
            .context("write SSLRequest")?;
        let mut response = [0u8; 1];
        stream
            .read_exact(&mut response)
            .context("read SSLRequest response")?;
        Ok(response[0])
    })
    .await??;
    assert_eq!(ssl_response, b'N');

    let cancel_closed = tokio::task::spawn_blocking(move || -> Result<bool> {
        let mut stream = TcpStream::connect(addr).context("connect raw CancelRequest socket")?;
        stream
            .write_all(&startup_control_packet(
                CANCEL_REQUEST_CODE,
                &[0, 0, 0, 1, 0, 0, 0, 2],
            ))
            .context("write CancelRequest")?;
        let mut response = [0u8; 1];
        let read = stream
            .read(&mut response)
            .context("read CancelRequest close")?;
        Ok(read == 0)
    })
    .await??;
    assert!(
        cancel_closed,
        "CancelRequest should close without backend panic"
    );

    server.shutdown()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_startup_identity_is_validated() -> Result<()> {
    let server = PgliteServer::temporary_tcp()?;
    let addr = server.tcp_addr().context("server should use TCP")?;

    let bad_user = tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let mut stream = TcpStream::connect(addr).context("connect raw startup socket")?;
        stream
            .write_all(&startup_message_with(&[
                ("user", "alice"),
                ("database", "template1"),
            ]))
            .context("write unsupported user startup message")?;
        let message = read_one_message(&mut stream).context("read unsupported user response")?;
        assert_eq!(message.tag, b'E');
        Ok(error_sqlstate(&message))
    })
    .await??;
    assert_eq!(bad_user.as_deref(), Some("28000"));

    let bad_database = tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let mut stream = TcpStream::connect(addr).context("connect raw startup socket")?;
        stream
            .write_all(&startup_message_with(&[
                ("user", "postgres"),
                ("database", "postgres"),
            ]))
            .context("write unsupported database startup message")?;
        let message =
            read_one_message(&mut stream).context("read unsupported database response")?;
        assert_eq!(message.tag, b'E');
        Ok(error_sqlstate(&message))
    })
    .await??;
    assert_eq!(bad_database.as_deref(), Some("3D000"));

    server.shutdown()?;
    Ok(())
}

async fn wait_for_tokio_postgres(
    connection_task: tokio::task::JoinHandle<Result<(), tokio_postgres::Error>>,
) -> Result<()> {
    timeout(Duration::from_secs(5), connection_task).await???;
    Ok(())
}

fn assert_sqlx_code(err: &sqlx::Error, expected: &str) {
    let code = err
        .as_database_error()
        .and_then(|db| db.code())
        .map(|code| code.into_owned());
    assert_eq!(code.as_deref(), Some(expected));
}

fn startup_control_packet(code: i32, tail: &[u8]) -> Vec<u8> {
    let len = 8 + tail.len() as i32;
    let mut packet = Vec::with_capacity(len as usize);
    packet.extend_from_slice(&len.to_be_bytes());
    packet.extend_from_slice(&code.to_be_bytes());
    packet.extend_from_slice(tail);
    packet
}

#[derive(Debug)]
struct RawBackendMessage {
    tag: u8,
    body: Vec<u8>,
}

fn startup_message() -> Vec<u8> {
    startup_message_with(&[
        ("user", "postgres"),
        ("database", "template1"),
        ("client_encoding", "UTF8"),
    ])
}

fn startup_message_with(params: &[(&str, &str)]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&3_i16.to_be_bytes());
    body.extend_from_slice(&0_i16.to_be_bytes());
    for (key, value) in params {
        add_cstring(&mut body, key);
        add_cstring(&mut body, value);
    }
    add_cstring(&mut body, "");

    let mut packet = Vec::with_capacity(body.len() + 4);
    packet.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    packet.extend_from_slice(&body);
    packet
}

fn query_message(sql: &str) -> Vec<u8> {
    let mut body = Vec::new();
    add_cstring(&mut body, sql);
    tagged_message(b'Q', body)
}

fn parse_statement(name: &str, sql: &str) -> Vec<u8> {
    let mut body = Vec::new();
    add_cstring(&mut body, name);
    add_cstring(&mut body, sql);
    body.extend_from_slice(&0_i16.to_be_bytes());
    tagged_message(b'P', body)
}

fn bind_statement(portal: &str, statement: &str, values: &[&str]) -> Vec<u8> {
    let mut body = Vec::new();
    add_cstring(&mut body, portal);
    add_cstring(&mut body, statement);
    body.extend_from_slice(&0_i16.to_be_bytes());
    body.extend_from_slice(&(values.len() as i16).to_be_bytes());
    for value in values {
        body.extend_from_slice(&(value.len() as i32).to_be_bytes());
        body.extend_from_slice(value.as_bytes());
    }
    body.extend_from_slice(&0_i16.to_be_bytes());
    tagged_message(b'B', body)
}

fn describe_portal(portal: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(b'P');
    add_cstring(&mut body, portal);
    tagged_message(b'D', body)
}

fn execute_portal(portal: &str) -> Vec<u8> {
    let mut body = Vec::new();
    add_cstring(&mut body, portal);
    body.extend_from_slice(&0_i32.to_be_bytes());
    tagged_message(b'E', body)
}

fn sync() -> Vec<u8> {
    tagged_message(b'S', Vec::new())
}

fn tagged_message(tag: u8, body: Vec<u8>) -> Vec<u8> {
    let mut packet = Vec::with_capacity(body.len() + 5);
    packet.push(tag);
    packet.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    packet.extend_from_slice(&body);
    packet
}

fn add_cstring(buffer: &mut Vec<u8>, value: &str) {
    buffer.extend_from_slice(value.as_bytes());
    buffer.push(0);
}

fn read_until_ready(stream: &mut TcpStream) -> Result<Vec<RawBackendMessage>> {
    let mut messages = Vec::new();
    loop {
        let message = read_one_message(stream)?;
        let ready = message.tag == b'Z';
        messages.push(message);
        if ready {
            return Ok(messages);
        }
    }
}

fn write_in_small_chunks(stream: &mut TcpStream, bytes: &[u8], chunk_size: usize) -> Result<()> {
    for chunk in bytes.chunks(chunk_size.max(1)) {
        stream.write_all(chunk).context("write protocol chunk")?;
    }
    stream.flush().context("flush protocol chunks")
}

fn read_one_message(stream: &mut TcpStream) -> Result<RawBackendMessage> {
    let mut header = [0u8; 5];
    stream
        .read_exact(&mut header)
        .context("read backend message header")?;
    let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    anyhow::ensure!(len >= 4, "invalid backend message length {len}");
    let body_len = (len - 4) as usize;
    let mut body = vec![0u8; body_len];
    stream
        .read_exact(&mut body)
        .context("read backend message body")?;
    Ok(RawBackendMessage {
        tag: header[0],
        body,
    })
}

fn assert_message_tags(messages: &[RawBackendMessage], expected: &[u8]) {
    let actual = messages.iter().map(|msg| msg.tag).collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

fn first_error_code(messages: &[RawBackendMessage]) -> Option<String> {
    messages
        .iter()
        .find(|msg| msg.tag == b'E')
        .and_then(error_sqlstate)
}

fn error_sqlstate(message: &RawBackendMessage) -> Option<String> {
    let mut cursor = 0usize;
    while cursor < message.body.len() {
        let field = message.body[cursor];
        cursor += 1;
        if field == 0 {
            break;
        }
        let end = message.body[cursor..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|offset| cursor + offset)?;
        if field == b'C' {
            return Some(String::from_utf8_lossy(&message.body[cursor..end]).into_owned());
        }
        cursor = end + 1;
    }
    None
}
