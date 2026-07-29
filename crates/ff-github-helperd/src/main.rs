mod authority;
mod credentials;
mod protocol;
mod service;
mod socket;

use std::{
    os::unix::net::{UnixDatagram, UnixStream},
    path::Path,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, bail};
use service::{Service, UnavailableTransport};
use socket::{activated_listener, peer_identity, read_request, write_response};
use sqlx::postgres::PgPoolOptions;
use tokio::{signal, sync::Semaphore, task, time::timeout};

const SOCKET_PATH: &str = "/run/forgefleet/github-helperd.sock";
const MAX_CONNECTIONS: usize = 32;
const REQUEST_DEADLINE: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_target(false).init();
    let credential = credentials::load_systemd_credential("github-app.pem")
        .context("GitHub App credential rejected")?;
    if !credential.expose().starts_with(b"-----BEGIN") {
        bail!("GitHub App credential is malformed");
    }
    let expected_uid = required_u32("FORGEFLEET_SCHEDULER_UID")?;
    let expected_gid = required_u32("FORGEFLEET_SCHEDULER_GID")?;
    let expected_digest = required_digest("FORGEFLEET_SCHEDULER_SHA256")?;
    let database_url =
        std::env::var("FORGEFLEET_DATABASE_URL").context("FORGEFLEET_DATABASE_URL is required")?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(Duration::from_secs(3))
        .connect(&database_url)
        .await
        .context("authority database unavailable")?;
    let listener = activated_listener(Path::new(SOCKET_PATH))?;
    listener.set_nonblocking(true)?;
    let listener = tokio::net::UnixListener::from_std(listener)?;
    let service = Arc::new(Service::new(
        authority::AuthorityStore::new(pool),
        UnavailableTransport,
    ));
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    notify_ready()?;

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _) = result?;
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    continue;
                };
                let service = service.clone();
                let expected_digest = expected_digest.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let _ = timeout(REQUEST_DEADLINE, handle_connection(
                        stream.into_std().map_err(anyhow::Error::from)?,
                        service,
                        expected_uid,
                        expected_gid,
                        &expected_digest,
                    )).await;
                    Ok::<(), anyhow::Error>(())
                });
            }
            _ = signal::ctrl_c() => break,
            _ = terminate_signal() => break,
        }
    }
    Ok(())
}

async fn handle_connection(
    mut stream: UnixStream,
    service: Arc<Service<UnavailableTransport>>,
    expected_uid: u32,
    expected_gid: u32,
    expected_digest: &str,
) -> anyhow::Result<()> {
    stream.set_read_timeout(Some(REQUEST_DEADLINE))?;
    stream.set_write_timeout(Some(REQUEST_DEADLINE))?;
    let peer = peer_identity(&stream)?;
    if peer.uid != expected_uid
        || peer.gid != expected_gid
        || peer.executable_sha256 != expected_digest
    {
        write_response(
            &mut stream,
            &protocol::Response::Denied {
                code: protocol::DenialCode::UnauthorizedPeer,
            },
        )?;
        return Ok(());
    }
    let request = task::spawn_blocking(move || {
        let request = read_request(&mut stream)?;
        Ok::<_, socket::SocketError>((stream, request))
    })
    .await??;
    let (mut stream, request) = request;
    let response = service.handle(&peer, request).await;
    task::spawn_blocking(move || write_response(&mut stream, &response)).await??;
    Ok(())
}

fn required_u32(name: &str) -> anyhow::Result<u32> {
    std::env::var(name)
        .with_context(|| format!("{name} is required"))?
        .parse()
        .with_context(|| format!("{name} must be numeric"))
}

fn required_digest(name: &str) -> anyhow::Result<String> {
    let value = std::env::var(name).with_context(|| format!("{name} is required"))?;
    if !protocol::valid_digest(&value) {
        bail!("{name} must be lowercase SHA-256");
    }
    Ok(value)
}

fn notify_ready() -> anyhow::Result<()> {
    let path = std::env::var_os("NOTIFY_SOCKET").context("NOTIFY_SOCKET is required")?;
    let socket = UnixDatagram::unbound().context("create readiness socket")?;
    socket
        .send_to(
            b"READY=1\nSTATUS=Fail-closed capability service ready",
            path,
        )
        .context("send readiness notification")?;
    Ok(())
}

#[cfg(unix)]
async fn terminate_signal() {
    if let Ok(mut signal) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
        signal.recv().await;
    }
}
