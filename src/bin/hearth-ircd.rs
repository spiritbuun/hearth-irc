use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use clap::Parser;
use hearth_irc::{irc, server, storage::Store, transport};

#[derive(Parser)]
#[command(
    version,
    about = "Persistent single-server IRC for agents and humans; no user accounts"
)]
struct Args {
    /// TCP bind address; use a LAN address or 0.0.0.0 to accept remote clients
    #[arg(long, default_value = "127.0.0.1:6667")]
    listen: SocketAddr,
    /// SQLite event database (parent directory must exist)
    #[arg(long, default_value = "hearth.sqlite3")]
    database: PathBuf,
    /// Shared IRC PASS secret; file contents are never logged
    #[arg(long)]
    password_file: Option<PathBuf>,
    /// PEM certificate chain; enables TLS on this listener
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<PathBuf>,
    /// PEM private key
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,
    /// Channels to create at startup (comma-separated); JOIN can create more
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "#general,#research,#implementation"
    )]
    channels: Vec<String>,
    #[arg(long, default_value_t = 256, value_parser = clap::value_parser!(u32).range(1..=4096))]
    max_clients: u32,
}

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    // Resolve the parent to make symlink/relative database paths use one lock.
    let parent = args
        .database
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let path = if args.database.exists() {
        args.database.canonicalize()?
    } else {
        parent
            .canonicalize()?
            .join(args.database.file_name().ok_or("invalid database path")?)
    };
    let _database_lock = hearth_irc::identity::lock_database(&path)?;
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if args.channels.is_empty()
        || args.channels.len() > 16
        || !args.channels.iter().all(|c| irc::valid_channel(c))
    {
        return Err(
            "configure 1–16 channels: # followed by up to 31 ASCII letters/digits/_/-".into(),
        );
    }
    let password = transport::password(args.password_file.as_deref())?;
    if !args.listen.ip().is_loopback() && password.is_none() {
        return Err("remote listeners require --password-file (no user accounts needed)".into());
    }
    let tls = match (&args.tls_cert, &args.tls_key) {
        (Some(cert), Some(key)) => Some(transport::tls_acceptor(cert, key)?),
        _ => None,
    };
    // New databases contain conversation history and self-reported paths.
    // Preserve the operator's permissions on an existing database.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&args.database)
    {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let store = Store::open(&args.database)?;
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    log::info!(
        "listening on {} tls={} password={} database={}",
        listener.local_addr()?,
        tls.is_some(),
        password.is_some(),
        args.database.display()
    );
    server::serve(
        listener,
        store,
        server::Settings {
            password,
            channels: args
                .channels
                .iter()
                .map(|c| irc::fold(c))
                .collect::<BTreeSet<_>>(),
            max_clients: args.max_clients as usize,
        },
        tls,
    )
    .await?;
    Ok(())
}
