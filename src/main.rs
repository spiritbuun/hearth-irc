mod adapter;
mod core;
mod fs;
mod local_store;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use clap::Parser;
use fuser::{Config, MountOption, Session};

#[derive(Parser)]
#[command(
    version,
    about = "IRC through files: channel logs, topics, members, and commands"
)]
struct Args {
    /// Existing empty directory to mount (foreground; Ctrl-C unmounts)
    mountpoint: PathBuf,
    /// Local loopback identity (ASCII letters/digits, '_' and '-')
    #[arg(long, default_value = "agent", value_parser = parse_nick)]
    nick: String,
    /// Channels to join/create on connect (comma-separated, at most 32)
    #[arg(long, value_delimiter = ',', default_value = "#general")]
    channels: Vec<String>,
    /// Log callback UID/PID, flags, offsets, byte counts, and errors to stderr
    #[arg(long)]
    trace: bool,
    /// IRC server hostname/IP; omit for the local loopback spike
    #[arg(long)]
    server: Option<String>,
    #[arg(long, default_value_t = 6667, requires = "server")]
    port: u16,
    /// Shared IRC server password file
    #[arg(long, requires = "server")]
    password_file: Option<PathBuf>,
    /// Reusable private DM history key; default is <mountpoint>.hearth-key
    #[arg(long, requires = "server")]
    identity_file: Option<PathBuf>,
    /// Verify and encrypt the IRC connection with TLS
    #[arg(long, requires = "server")]
    tls: bool,
    /// Additional trusted CA/certificate PEM for private servers
    #[arg(long, requires = "tls")]
    tls_ca: Option<PathBuf>,
    /// Self-reported machine label for WHOIS (defaults to this machine's hostname)
    #[arg(long, requires = "server")]
    host_label: Option<String>,
    /// Self-reported working directory for WHOIS (defaults to startup directory)
    #[arg(long, requires = "server")]
    workdir: Option<PathBuf>,
}

fn parse_nick(value: &str) -> Result<String, String> {
    if !hearth_irc::irc::valid_nick(value) {
        return Err("use a valid IRC nickname of at most 32 ASCII bytes".into());
    }
    Ok(value.to_owned())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(if args.trace {
        "hearthd=debug,fuser=info"
    } else {
        "info"
    }))
    .init();
    let mountpoint = args.mountpoint.canonicalize()?;
    if std::fs::read_dir(&mountpoint)?.next().is_some() {
        return Err("mountpoint must be an empty directory".into());
    }
    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName("hearth".into()),
        MountOption::Subtype("hearth".into()),
        MountOption::DefaultPermissions,
        MountOption::NoDev,
        MountOption::NoSuid,
        MountOption::NoExec,
        MountOption::RW,
    ];
    let mut model = core::Model::new(args.nick.clone());
    let channels: Vec<_> = args
        .channels
        .iter()
        .map(|s| hearth_irc::irc::fold(s))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if channels.is_empty()
        || channels.len() > 32
        || !channels.iter().all(|s| hearth_irc::irc::valid_channel(s))
    {
        return Err("choose 1–32 channels: # followed by 1–31 ASCII letters/digits/_/-".into());
    }
    let network = if let Some(server) = args.server {
        let host_label = args.host_label.unwrap_or(
            std::fs::read_to_string("/proc/sys/kernel/hostname")?
                .trim()
                .to_owned(),
        );
        if host_label.is_empty()
            || host_label.len() > 64
            || !host_label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err("host label must be 1–64 ASCII letters/digits/./_/-".into());
        }
        let workdir = args
            .workdir
            .unwrap_or(std::env::current_dir()?)
            .canonicalize()?;
        if !workdir.is_dir() {
            return Err("working directory must be a directory".into());
        }
        let workdir = workdir
            .to_str()
            .ok_or("working directory must be UTF-8")?
            .to_owned();
        if !hearth_irc::irc::text_ok(&workdir, 300) {
            return Err(
                "working directory must fit 300 UTF-8 bytes without control characters".into(),
            );
        }
        model.remote = Some(core::Remote::default());
        model.location = Some((host_label, workdir));
        Some(adapter::Settings {
            server,
            port: args.port,
            password: hearth_irc::transport::password(args.password_file.as_deref())?,
            identity: hearth_irc::identity::load_key(
                &args
                    .identity_file
                    .unwrap_or_else(|| mountpoint.with_extension("hearth-key")),
            )?,
            tls: if args.tls {
                Some(hearth_irc::transport::tls_connector(
                    args.tls_ca.as_deref(),
                )?)
            } else {
                None
            },
        })
    } else {
        None
    };
    model
        .configure_channels(&channels)
        .map_err(std::io::Error::from_raw_os_error)?;
    let model = Arc::new(Mutex::new(model));
    let runtime = if network.is_some() {
        Some(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?,
        )
    } else {
        None
    };
    let fs = fs::HearthFs::new(model.clone());
    let mut session = Session::new(fs, &mountpoint, &config)?;
    let mut unmounter = session.unmount_callable();
    ctrlc::set_handler(move || {
        if let Err(error) = unmounter.unmount() {
            log::error!("unmount failed: {error}");
        }
    })?;
    if let (Some(runtime), Some(network)) = (&runtime, network) {
        runtime.spawn(adapter::run(network, model.clone()));
    }
    log::info!(
        "mounted {} (see status for connection and persistence; newline required)",
        mountpoint.display()
    );
    session.run()?;
    if let Some(remote) = &model.lock().unwrap().remote {
        let pending = remote.queue.len() + usize::from(remote.inflight.is_some());
        if pending != 0 {
            log::warn!(
                "unmount with {pending} unacknowledged outbound messages; queue is volatile"
            );
        }
    }
    if let Some(runtime) = runtime {
        runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    }
    Ok(())
}
