use std::{fs, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{anyhow, Context};
use bytes::Bytes;
use clap::Parser;
use perf::ConnectionHandle;
use s2n_quic::{connection::Connection, server::Server, stream::ReceiveStream, stream::SendStream};
use tracing::{debug, error, info};

#[derive(Parser)]
#[clap(name = "server")]
struct Opt {
    /// Address to listen on
    #[clap(long = "listen", default_value = "[::]:4433")]
    listen: SocketAddr,
    /// TLS private key in PEM format
    #[clap(parse(from_os_str), short = 'k', long = "key", requires = "cert")]
    key: Option<PathBuf>,
    /// TLS certificate in PEM format
    #[clap(parse(from_os_str), short = 'c', long = "cert", requires = "key")]
    cert: Option<PathBuf>,
    /// Send buffer size in bytes
    #[clap(long, default_value = "2097152")]
    send_buffer_size: usize,
    /// Receive buffer size in bytes
    #[clap(long, default_value = "2097152")]
    recv_buffer_size: usize,
    /// Whether to print connection statistics
    #[clap(long)]
    conn_stats: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let opt = Opt::parse();

    tracing_subscriber::fmt::init();

    if let Err(e) = run(opt).await {
        error!("{:#}", e);
    }
}

/// NOTE: this certificate is to be used for demonstration purposes only!
pub static CERT_PEM: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../quic/s2n-quic-core/certs/cert.pem"
));
/// NOTE: this certificate is to be used for demonstration purposes only!
pub static KEY_PEM: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../quic/s2n-quic-core/certs/key.pem"
));

/*
pub static CERT_PEM: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/pki/perf-serv.crt"));

pub static KEY_PEM: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/pki/perf-serv-key.pem"
));
*/

async fn run(opt: Opt) -> Result<(), Box<dyn std::error::Error>> {
    let mut server = Server::builder()
        .with_tls((CERT_PEM, KEY_PEM))?
        .with_io(opt.listen)?
        .start()?;

    info!("listening on {}", opt.listen);

    let opt = Arc::new(opt);

    while let Some(stream) = server.accept().await {
        let opt = opt.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, opt).await {
                error!("connection lost: {:#}", e);
            }
        });
    }

    Ok(())
}

async fn handle(conn: Connection, opt: Arc<Opt>) -> Result<(), anyhow::Error> {
    debug!("{} connected", conn.remote_addr()?);
    let connection = ConnectionHandle::new(conn);
    tokio::try_join!(drive_uni(connection.clone()), drive_bi(connection.clone()),)?;
    Ok(())
}

async fn drive_uni(connection: ConnectionHandle) -> Result<(), anyhow::Error> {
    while let Some(stream) = connection.lock().await.accept_receive_stream().await? {
        let connection = connection.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_uni(connection, stream).await {
                error!("request failed: {:#}", e);
            }
        });
    }
    Ok(())
}

async fn handle_uni(
    connection: ConnectionHandle,
    stream: ReceiveStream,
) -> Result<(), anyhow::Error> {
    let bytes = read_req(stream).await?;
    let response = connection.lock().await.open_send_stream().await?;
    respond(bytes, response).await?;
    Ok(())
}

async fn drive_bi(connection: ConnectionHandle) -> Result<(), anyhow::Error> {
    while let Some(strm) = connection
        .lock()
        .await
        .accept_bidirectional_stream()
        .await?
    {
        let (recv, send) = strm.split();
        tokio::spawn(async move {
            if let Err(e) = handle_bi(send, recv).await {
                error!("request failed: {:#}", e);
            }
        });
    }
    Ok(())
}

async fn handle_bi(send: SendStream, recv: ReceiveStream) -> Result<(), anyhow::Error> {
    let bytes = read_req(recv).await?;
    respond(bytes, send).await?;
    Ok(())
}

async fn read_req(mut stream: ReceiveStream) -> Result<u64, anyhow::Error> {
    let mut buf = [0; 8];
    if let Some(blah) = stream.receive().await.context("reading request")? {
        buf.copy_from_slice(&blah);
        let n = u64::from_be_bytes(buf);
        debug!("got req for {} bytes on {}", n, stream.id());
        drain_stream(stream).await?;
        Ok(n)
    } else {
        Err(anyhow!("Failed to read in read_req"))
    }
}

async fn drain_stream(mut stream: ReceiveStream) -> Result<(), anyhow::Error> {
    #[rustfmt::skip]
    let mut bufs = [
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
    ];
    while let (_, true) = stream.receive_vectored(&mut bufs[..]).await? {}
    debug!("finished reading {}", stream.id());
    Ok(())
}

async fn respond(mut bytes: u64, mut stream: SendStream) -> Result<(), anyhow::Error> {
    const DATA: [u8; 1024 * 1024] = [42; 1024 * 1024];

    while bytes > 0 {
        let chunk_len = bytes.min(DATA.len() as u64);
        stream
            .send(Bytes::from_static(&DATA[..chunk_len as usize]))
            .await
            .context("sending response")?;
        bytes -= chunk_len;
    }
    debug!("finished responding on {}", stream.id());
    Ok(())
}
