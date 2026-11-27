use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::ConnectInfo;
use axum::extract::connect_info::Connected;
use axum::serve::IncomingStream;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use forgenerd::api::{self, AppState, ClientIp, ViaProxy};
use forgenerd::auth::{self, crypto::Cipher};
use forgenerd::config::{self, Config};
use forgenerd::db;
use forgenerd::deploy::{reconciler, routing};
use forgenerd::executor::local::LocalDockerExecutor;
use forgenerd::executor::{NodeExecutor, NodeRegistry};

#[derive(Parser)]
#[command(
    name = "forgenerd",
    version,
    about = "Self-hosted PaaS in a single binary"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the control plane and the local node
    Server,
    /// Run a remote worker agent (planned for v1.1)
    Agent,
    /// Set a user's password and sign them out everywhere. Reads the new password
    /// from the first line of stdin, e.g. `forgenerd reset-password you@example.com`.
    ResetPassword {
        /// Email of the account to reset
        email: String,
    },
    /// Replace the database with a panel backup (`.db` or `.db.gz`). Stop the service
    /// first; the current database is kept next to it as `forgenerd.db.before-restore-*`.
    RestoreDb {
        /// Backup file downloaded from the S3 target
        file: std::path::PathBuf,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match Cli::parse().command {
        Command::Server => match run_server().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                tracing::error!("{err:#}");
                ExitCode::FAILURE
            }
        },
        Command::Agent => {
            eprintln!("forgenerd agent is not implemented in v1");
            ExitCode::from(2)
        }
        Command::ResetPassword { email } => match reset_password(&email).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err:#}");
                ExitCode::FAILURE
            }
        },
        Command::RestoreDb { file } => match restore_db(&file).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err:#}");
                ExitCode::FAILURE
            }
        },
    }
}

/// Swap in a panel backup after checking it is an intact forgenerd database. Migrations
/// run on the next start, so backups from older versions are upgraded then.
async fn restore_db(file: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut config = Config::from_env()?;
    config.ensure_data_dir()?;
    let running = tokio::time::timeout(
        Duration::from_secs(1),
        tokio::net::TcpStream::connect(config.listen),
    )
    .await
    .is_ok_and(|r| r.is_ok());
    anyhow::ensure!(
        !running,
        "forgenerd is running on {}; stop it first (systemctl stop forgenerd)",
        config.listen
    );

    let db = config.db_path();
    let staged = db.with_extension("db.restore");
    let is_gzip = std::fs::read(file)
        .with_context(|| format!("reading {}", file.display()))?
        .starts_with(&[0x1f, 0x8b]);
    {
        let mut input = std::fs::File::open(file)?;
        let mut output = std::fs::File::create(&staged)
            .with_context(|| format!("creating {}", staged.display()))?;
        if is_gzip {
            std::io::copy(&mut flate2::read::GzDecoder::new(input), &mut output)
                .context("the backup is not a valid gzip file")?;
        } else {
            std::io::copy(&mut input, &mut output)?;
        }
        output.sync_all()?;
    }
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))?;

    let check = async {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&staged)
            .read_only(true);
        let mut conn = sqlx::ConnectOptions::connect(&options)
            .await
            .context("the backup is not a SQLite database")?;
        let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(&mut conn)
            .await
            .context("the backup is not a SQLite database")?;
        anyhow::ensure!(integrity == "ok", "the backup is corrupt: {integrity}");
        let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&mut conn)
            .await
            .context("the backup is not a forgenerd database")?;
        anyhow::ensure!(users > 0, "the backup has no user accounts");
        Ok(())
    }
    .await;
    if let Err(err) = check {
        let _ = std::fs::remove_file(&staged);
        return Err(err);
    }

    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    if db.exists() {
        for suffix in ["", "-wal", "-shm"] {
            let current = PathBuf::from(format!("{}{suffix}", db.display()));
            if current.exists() {
                let kept =
                    PathBuf::from(format!("{}.before-restore-{stamp}{suffix}", db.display()));
                std::fs::rename(&current, &kept)
                    .with_context(|| format!("moving {} aside", current.display()))?;
            }
        }
    }
    std::fs::rename(&staged, &db).with_context(|| format!("installing {}", db.display()))?;
    eprintln!(
        "Restored {} into {}. Make sure the master key matches the server the backup came \
         from, then start forgenerd.",
        file.display(),
        db.display()
    );
    Ok(())
}

/// Recovery for a lost admin password; requires shell access to the data directory.
async fn reset_password(email: &str) -> Result<()> {
    use std::io::{BufRead, IsTerminal};

    let config = Config::from_env()?;
    let pool = db::connect(&config.db_path()).await?;
    let user: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE email = ?")
        .bind(email.trim().to_ascii_lowercase())
        .fetch_optional(&pool)
        .await?;
    let user = user.with_context(|| format!("no user with email {email}"))?;

    if std::io::stdin().is_terminal() {
        eprint!("New password (shown as you type): ");
    }
    let mut password = String::new();
    std::io::stdin().lock().read_line(&mut password)?;
    let password = password.trim_end_matches(['\r', '\n']).to_string();
    anyhow::ensure!(
        password.chars().count() >= auth::MIN_PASSWORD_LEN,
        "password must be at least {} characters",
        auth::MIN_PASSWORD_LEN
    );

    let hash = auth::hash_password(password).await?;
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(hash)
        .bind(&user)
        .execute(&mut *tx)
        .await?;
    let sessions = sqlx::query("DELETE FROM sessions WHERE user_id = ?")
        .bind(&user)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    eprintln!("Password updated for {email}; {sessions} session(s) signed out.");
    Ok(())
}

async fn run_server() -> Result<()> {
    let mut config = Config::from_env()?;
    config.ensure_data_dir()?;
    let key = config::load_or_create_master_key(&config.data_dir)?;
    let pool = db::connect(&config.db_path()).await?;

    let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&pool)
        .await?;
    let setup_token = (users == 0).then(auth::new_token);
    if let Some(token) = &setup_token {
        eprintln!(
            "\n================ forgenerd first-time setup ================\n\
             Open http://{} and enter this one-time setup token:\n\n    {token}\n\n\
             ============================================================\n",
            config.listen
        );
    }

    spawn_session_purger(pool.clone());
    let interrupted = db::fail_interrupted_deployments(&pool).await?;
    if interrupted > 0 {
        tracing::warn!(
            count = interrupted,
            "marked deployments interrupted by a restart as failed"
        );
    }

    // Local node: Docker network + Caddy container. If Docker is unavailable the panel
    // still starts, with the node marked offline.
    let local = Arc::new(LocalDockerExecutor::new(
        &config.data_dir,
        config.listen.port(),
    )?);
    let node_online = match local.ensure_node_ready().await {
        Ok(()) => true,
        Err(err) => {
            tracing::error!(error = %err, "local node is not ready; deployments will fail until Docker is available");
            false
        }
    };
    sqlx::query("UPDATE nodes SET status = ?, last_seen_at = CURRENT_TIMESTAMP WHERE id = 'local'")
        .bind(if node_online { "online" } else { "offline" })
        .execute(&pool)
        .await?;

    let mut registry = NodeRegistry::default();
    registry.insert("local", local.clone());
    let state = AppState::new(
        pool,
        Cipher::new(&key),
        setup_token,
        registry,
        config.data_dir.clone(),
    );
    if node_online {
        match reconciler::remove_orphans(&state).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(count = n, "removed orphaned containers"),
            Err(err) => tracing::warn!(error = %err, "orphan cleanup failed"),
        }
        if let Err(err) = routing::sync_node(&state, "local").await {
            tracing::error!(error = %err, "initial proxy sync failed");
        }
    }
    reconciler::spawn(state.clone());
    forgenerd::backup::spawn_scheduler(state.clone());
    forgenerd::maintenance::spawn_scheduler(state.clone());

    // Caddy reaches the panel on the host's forgenerd-internal gateway address; requests
    // that arrive on that address were served to the browser over HTTPS. The panel binds
    // FORGENERD_LISTEN plus the gateway address, or only FORGENERD_LISTEN when that is a
    // wildcard (it then covers the gateway too, and the panel is public on plain HTTP).
    let gateway = local.gateway_ip();
    let app = api::router(state).layer(axum::middleware::from_fn(
        move |ConnectInfo(conn): ConnectInfo<Conn>,
              mut req: axum::extract::Request,
              next: axum::middleware::Next| async move {
            let mut client = conn.peer.ip().to_canonical();
            if gateway.is_some_and(|g| g == conn.local.ip().to_canonical()) {
                req.extensions_mut().insert(ViaProxy);
                if let Some(ip) = forwarded_for(req.headers()) {
                    client = ip;
                }
            }
            req.extensions_mut().insert(ClientIp(client));
            next.run(req).await
        },
    ));

    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding {}", config.listen))?;
    if config.listen.ip().is_unspecified() {
        tracing::warn!(addr = %config.listen, "panel is reachable on every interface over plain HTTP; set a panel domain and close this port once HTTPS works");
    } else if let Some(gateway) = gateway {
        let addr = SocketAddr::new(gateway, config.listen.port());
        match tokio::net::TcpListener::bind(addr).await {
            Ok(proxied) => {
                tracing::info!(%addr, "panel listening for proxied traffic");
                let app = app.clone();
                tokio::spawn(async move {
                    if let Err(err) =
                        axum::serve(proxied, app.into_make_service_with_connect_info::<Conn>())
                            .with_graceful_shutdown(shutdown_signal())
                            .await
                    {
                        tracing::error!(error = %err, "proxy listener failed");
                    }
                });
            }
            Err(err) => {
                tracing::warn!(%addr, error = %err, "cannot bind proxy listener; panel domain will not work")
            }
        }
    }
    tracing::info!(addr = %config.listen, "forgenerd panel listening");
    axum::serve(listener, app.into_make_service_with_connect_info::<Conn>())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")
}

/// Both ends of a connection: the local address tells which listener it arrived on.
#[derive(Clone, Copy)]
struct Conn {
    local: SocketAddr,
    peer: SocketAddr,
}

impl Connected<IncomingStream<'_, tokio::net::TcpListener>> for Conn {
    fn connect_info(stream: IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        let unknown = SocketAddr::from(([0, 0, 0, 0], 0));
        Self {
            local: stream.io().local_addr().unwrap_or(unknown),
            peer: *stream.remote_addr(),
        }
    }
}

/// The last `X-Forwarded-For` entry, which is the one Caddy itself appended.
fn forwarded_for(headers: &axum::http::HeaderMap) -> Option<std::net::IpAddr> {
    headers
        .get_all("x-forwarded-for")
        .iter()
        .next_back()?
        .to_str()
        .ok()?
        .rsplit(',')
        .next()?
        .trim()
        .parse()
        .ok()
}

fn spawn_session_purger(pool: sqlx::SqlitePool) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
            if let Err(err) = auth::purge_expired_sessions(&pool).await {
                tracing::warn!(error = %err, "purging expired sessions failed");
            }
        }
    });
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %err, "failed to listen for ctrl-c");
            std::future::pending::<()>().await;
        }
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "failed to listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutting down");
}
