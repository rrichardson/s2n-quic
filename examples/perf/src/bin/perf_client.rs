use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context};
use bytes::Bytes;
use clap::Parser;
use perf::stats::{OpenStreamStats, Stats};
use perf::ConnectionHandle;
use s2n_quic::{client::Connect, stream::ReceiveStream, stream::SendStream, Client};
#[cfg(feature = "json-output")]
use std::path::PathBuf;
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

/// Connects to a QUIC perf server and maintains a specified pattern of requests until interrupted
#[derive(Parser)]
#[clap(name = "client")]
struct Opt {
    /// Host to connect to
    #[clap(default_value = "localhost:4433")]
    host: String,
    /// Override DNS resolution for host
    #[clap(long)]
    ip: Option<IpAddr>,
    /// Number of unidirectional requests to maintain concurrently
    #[clap(long, default_value = "0")]
    uni_requests: u64,
    /// Number of bidirectional requests to maintain concurrently
    #[clap(long, default_value = "1")]
    bi_requests: u64,
    /// Number of bytes to request
    #[clap(long, default_value = "1048576")]
    download_size: u64,
    /// Number of bytes to transmit, in addition to the request header
    #[clap(long, default_value = "1048576")]
    upload_size: u64,
    /// The time to run in seconds
    #[clap(long, default_value = "60")]
    duration: u64,
    /// The interval in seconds at which stats are reported
    #[clap(long, default_value = "1")]
    interval: u64,
    /// Send buffer size in bytes
    #[clap(long, default_value = "2097152")]
    send_buffer_size: usize,
    /// Receive buffer size in bytes
    #[clap(long, default_value = "2097152")]
    recv_buffer_size: usize,
    /// Specify the local socket address
    #[clap(long)]
    local_addr: Option<SocketAddr>,
    /// Whether to print connection statistics
    #[clap(long)]
    conn_stats: bool,
    /// File path to output JSON statistics to. If the file is '-', stdout will be used
    #[cfg(feature = "json-output")]
    #[clap(long)]
    json: Option<PathBuf>,
}

pub static CERT_PEM: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../quic/s2n-quic-core/certs/cert.pem"
));

/*
pub static CERT_PEM: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/pki/perf-client.crt"));
*/

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let opt = Opt::parse();

    tracing_subscriber::fmt::init();

    if let Err(e) = run(opt).await {
        error!("{:#}", e);
    }
}

async fn run(opt: Opt) -> Result<(), Box<dyn std::error::Error>> {
    let mut host_parts = opt.host.split(':');
    let host_name = host_parts.next().unwrap();
    let host_port = host_parts
        .next()
        .map_or(Ok(443), |x| x.parse())
        .context("parsing port")?;
    let addr = match opt.ip {
        None => tokio::net::lookup_host(&opt.host)
            .await
            .context("resolving host")?
            .next()
            .unwrap(),
        Some(ip) => SocketAddr::new(ip, host_port),
    };

    info!("connecting to {} at {}", host_name, addr);
    let bind_addr = opt.local_addr.unwrap_or_else(|| {
        let unspec = if addr.is_ipv4() {
            Ipv4Addr::UNSPECIFIED.into()
        } else {
            Ipv6Addr::UNSPECIFIED.into()
        };
        SocketAddr::new(unspec, 0)
    });

    info!("local addr {:?}", bind_addr);

    let stream_stats = OpenStreamStats::default();
    let client = Client::builder()
        .with_tls(CERT_PEM)?
        .with_io("0.0.0.0:0")?
        .start()?;
    info!("client created");
    let connect = Connect::new(addr).with_server_name(host_name);
    let connection = ConnectionHandle::new(client.connect(connect).await?);

    // ensure the connection doesn't time out with inactivity
    connection.lock().await.keep_alive(true)?;

    info!("established");

    let drive_fut = async {
        tokio::try_join!(
            drive_uni(
                connection.clone(),
                stream_stats.clone(),
                opt.uni_requests,
                opt.upload_size,
                opt.download_size,
            ),
            drive_bi(
                connection.clone(),
                stream_stats.clone(),
                opt.bi_requests,
                opt.upload_size,
                opt.download_size,
            )
        )
    };

    let mut stats = Stats::default();

    let stats_fut = async {
        let interval_duration = Duration::from_secs(opt.interval);

        loop {
            let start = Instant::now();
            tokio::time::sleep(interval_duration).await;
            {
                stats.on_interval(start, &stream_stats);

                stats.print();
            }
        }
    };

    tokio::select! {
        _ = drive_fut => {}
        _ = stats_fut => {}
        _ = tokio::signal::ctrl_c() => {
            info!("shutting down");
            connection.lock().await.close(0u32.into());
        }
        // Add a small duration so the final interval can be reported
        _ = tokio::time::sleep(Duration::from_secs(opt.duration) + Duration::from_millis(200)) => {
            info!("shutting down");
            connection.lock().await.close(0u32.into())
        }
    }

    #[cfg(feature = "json-output")]
    if let Some(path) = opt.json {
        stats.print_json(path.as_path())?;
    }

    Ok(())
}

async fn drain_stream(
    mut stream: ReceiveStream,
    download: u64,
    stream_stats: OpenStreamStats,
) -> Result<(), anyhow::Error> {
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
    let download_start = Instant::now();
    let recv_stream_stats = stream_stats.new_receiver(&stream, download);

    let mut first_byte = true;

    loop {
        let (size, is_open) = stream.receive_vectored(&mut bufs[..]).await?;
        if first_byte {
            recv_stream_stats.on_first_byte(download_start.elapsed());
            first_byte = false;
        }
        let bytes_received = bufs[..size].iter().map(|b| b.len()).sum();
        recv_stream_stats.on_bytes(bytes_received);
        if !is_open {
            break;
        }
    }

    if first_byte {
        recv_stream_stats.on_first_byte(download_start.elapsed());
    }
    recv_stream_stats.finish(download_start.elapsed());

    debug!("response finished on {}", stream.id());
    Ok(())
}

async fn drive_uni(
    connection: ConnectionHandle,
    stream_stats: OpenStreamStats,
    concurrency: u64,
    upload: u64,
    download: u64,
) -> Result<(), anyhow::Error> {
    let sem = Arc::new(Semaphore::new(concurrency as usize));

    loop {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let send = connection.lock().await.open_send_stream().await?;
        let stream_stats = stream_stats.clone();

        debug!("sending request on {}", send.id());
        let conn = connection.clone();
        tokio::spawn(async move {
            if let Err(e) = request_uni(send, conn, upload, download, stream_stats).await {
                error!("sending request failed: {:#}", e);
            }

            drop(permit);
        });
    }
}

async fn request_uni(
    send: SendStream,
    conn: ConnectionHandle,
    upload: u64,
    download: u64,
    stream_stats: OpenStreamStats,
) -> Result<(), anyhow::Error> {
    request(send, upload, download, stream_stats.clone()).await?;
    if let Some(recv) = conn.lock().await.accept_receive_stream().await? {
        drain_stream(recv, download, stream_stats).await?;
        Ok(())
    } else {
        Err(anyhow!("Stream unceremoniously closed"))
    }
}

async fn request(
    mut send: SendStream,
    mut upload: u64,
    download: u64,
    stream_stats: OpenStreamStats,
) -> Result<(), anyhow::Error> {
    let upload_start = Instant::now();
    let b = download.to_be_bytes();
    send.send(bytes::Bytes::copy_from_slice(b.as_slice()))
        .await?;
    if upload == 0 {
        send.close().await?;
        return Ok(());
    }

    let send_stream_stats = stream_stats.new_sender(&send, upload);

    const DATA: [u8; 1024 * 1024] = [42; 1024 * 1024];
    while upload > 0 {
        let chunk_len = upload.min(DATA.len() as u64);
        send.send(Bytes::from_static(&DATA[..chunk_len as usize]))
            .await
            .context("sending response")?;
        send_stream_stats.on_bytes(chunk_len as usize);
        upload -= chunk_len;
    }
    send.close().await?;
    send_stream_stats.finish(upload_start.elapsed());

    debug!("upload finished on {}.", send.id());
    Ok(())
}

async fn drive_bi(
    connection: ConnectionHandle,
    stream_stats: OpenStreamStats,
    concurrency: u64,
    upload: u64,
    download: u64,
) -> Result<(), anyhow::Error> {
    let sem = Arc::new(Semaphore::new(concurrency as usize));

    loop {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let (recv, send) = connection
            .lock()
            .await
            .open_bidirectional_stream()
            .await?
            .split();
        let stream_stats = stream_stats.clone();

        debug!("sending request on {}", send.id());
        tokio::spawn(async move {
            if let Err(e) = request_bi(send, recv, upload, download, stream_stats).await {
                error!("request failed: {:#}", e);
            }

            drop(permit);
        });
    }
}

async fn request_bi(
    send: SendStream,
    recv: ReceiveStream,
    upload: u64,
    download: u64,
    stream_stats: OpenStreamStats,
) -> Result<(), anyhow::Error> {
    request(send, upload, download, stream_stats.clone()).await?;
    drain_stream(recv, download, stream_stats).await?;
    Ok(())
}
