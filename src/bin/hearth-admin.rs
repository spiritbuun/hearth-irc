//! Offline, OS-authorized maintenance. Ordinary IRC clients have no admin route.
use clap::{Parser, Subcommand};
use hearth_irc::{identity, irc};
use rusqlite::{Connection, params};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version,
    about = "Inspect, back up, prune, or destroy hearth history; stop the IRCd before use"
)]
struct Args {
    #[arg(long)]
    database: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Stats,
    /// SQLite-consistent copy; refuses an existing destination
    Backup {
        destination: PathBuf,
    },
    /// Delete messages older than a UTC/RFC3339 timestamp; optionally only a channel
    Prune {
        #[arg(long)]
        before: String,
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        apply: bool,
    },
    /// Remove a channel and its history. Startup seeds can recreate it empty.
    DestroyChannel {
        channel: String,
        #[arg(long)]
        apply: bool,
    },
    /// Remove all channel and DM messages, preserving channel definitions/topics
    DestroyHistory {
        #[arg(long)]
        apply: bool,
    },
    /// Reclaim space after deletion. Pruning alone leaves reusable SQLite pages.
    Vacuum,
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if !args.database.is_file() {
        return Err("database does not exist".into());
    }
    let path = args.database.canonicalize()?;
    let _lock = identity::lock_database(&path)?;
    let mut db = Connection::open(&path)?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != 3 {
        return Err(
            "start this version of hearth-ircd once to migrate the database before maintenance"
                .into(),
        );
    }
    match args.command {
        Command::Stats => {
            let (messages, channels): (i64, i64) = db.query_row(
                "SELECT (SELECT count(*) FROM events),(SELECT count(*) FROM channels)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            println!("messages: {messages}\nchannels: {channels}");
        }
        Command::Backup { destination } => {
            if destination.exists() {
                return Err("backup destination already exists".into());
            }
            // VACUUM INTO creates the database; use a private process umask.
            unsafe {
                libc::umask(0o077);
            }
            db.execute(
                "VACUUM INTO ?1",
                [destination.to_str().ok_or("destination must be UTF-8")?],
            )?;
            println!("backup: {}", destination.display());
        }
        Command::Vacuum => {
            db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
            println!("space reclaimed");
        }
        command => {
            let (where_sql, values, channel, apply) = match command {
                Command::Prune {
                    before,
                    channel,
                    apply,
                } => {
                    let before = hearth_irc::history::timestamp(&format!("timestamp={before}"))?;
                    if let Some(name) = channel {
                        if !irc::valid_channel(&name) {
                            return Err("invalid channel".into());
                        }
                        (
                            "time < ?1 AND channel=?2",
                            vec![before, irc::fold(&name)],
                            None,
                            apply,
                        )
                    } else {
                        ("time < ?1", vec![before], None, apply)
                    }
                }
                Command::DestroyChannel { channel, apply } => {
                    if !irc::valid_channel(&channel) {
                        return Err("invalid channel".into());
                    }
                    let channel = irc::fold(&channel);
                    ("channel=?1", vec![channel.clone()], Some(channel), apply)
                }
                Command::DestroyHistory { apply } => ("1=1", vec![], None, apply),
                _ => unreachable!(),
            };
            let count: i64 = db.query_row(
                &format!("SELECT count(*) FROM events WHERE {where_sql}"),
                rusqlite::params_from_iter(&values),
                |r| r.get(0),
            )?;
            println!("matching messages: {count}");
            if !apply {
                println!("preview only; add --apply to delete");
                return Ok(());
            }
            let tx = db.transaction()?;
            tx.execute(
                &format!("DELETE FROM events WHERE {where_sql}"),
                rusqlite::params_from_iter(values),
            )?;
            let removed = if let Some(channel) = channel {
                tx.execute("DELETE FROM channels WHERE name=?1", params![channel])?
            } else {
                0
            };
            if count > 0 || removed > 0 {
                // Existing mounts must not silently keep presenting deleted history.
                tx.execute(
                    "UPDATE metadata SET value=lower(hex(randomblob(16))) WHERE key='history_epoch'",
                    [],
                )?;
            }
            tx.commit()?;
            println!(
                "deleted messages: {count}\ndeleted channels: {removed}\nRemount clients after restarting the IRCd. Run vacuum to reclaim disk space."
            );
        }
    }
    Ok(())
}
