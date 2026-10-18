use std::path::Path;

use chrono::{SecondsFormat, Utc};
use rusqlite::{Connection, params};

#[derive(Clone, Debug)]
pub struct Event {
    pub id: i64,
    pub channel: String,
    pub time: String,
    pub nick: String,
    pub user: String,
    pub peer: String,
    pub body: String,
    pub command: String,
    pub target: String,
    pub sender: String,
}

#[derive(Clone, Debug)]
pub struct Channel {
    pub name: String,
    pub topic: String,
    pub topic_set_by: String,
    pub topic_set_at: i64,
}

pub struct Store {
    pub(crate) db: Connection,
    pub id: String,
    pub history_id: String,
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(3))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > 3 {
            return Err(rusqlite::Error::InvalidQuery);
        }
        db.execute_batch(
            "BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            INSERT OR IGNORE INTO metadata VALUES ('server_id', lower(hex(randomblob(16))));
            INSERT OR IGNORE INTO metadata VALUES ('history_epoch', lower(hex(randomblob(16))));
            CREATE TABLE IF NOT EXISTS events (
                id INTEGER PRIMARY KEY AUTOINCREMENT, channel TEXT NOT NULL,
                time TEXT NOT NULL, nick TEXT NOT NULL, username TEXT NOT NULL,
                peer TEXT NOT NULL, host_label TEXT NOT NULL, workdir TEXT NOT NULL,
                body TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS events_channel_id ON events(channel, id);
            CREATE TABLE IF NOT EXISTS channels (
                name TEXT PRIMARY KEY, topic TEXT NOT NULL DEFAULT '',
                topic_set_by TEXT NOT NULL DEFAULT '', topic_set_at INTEGER NOT NULL DEFAULT 0);",
        )?;
        if version < 2 {
            db.execute(
                "INSERT OR IGNORE INTO channels(name) SELECT DISTINCT channel FROM events",
                [],
            )?;
        }
        if version < 3 {
            db.execute_batch(
                "ALTER TABLE events ADD COLUMN command TEXT NOT NULL DEFAULT 'PRIVMSG';
                ALTER TABLE events ADD COLUMN target TEXT NOT NULL DEFAULT '';
                ALTER TABLE events ADD COLUMN sender TEXT NOT NULL DEFAULT '';
                UPDATE events SET target=channel;
                CREATE TABLE dm_peers(actor TEXT NOT NULL,peer TEXT NOT NULL,nick TEXT NOT NULL,
                    PRIMARY KEY(actor,peer));
                CREATE INDEX events_channel_time ON events(channel,time,id);",
            )?;
        }
        db.execute_batch("PRAGMA user_version=3; COMMIT;")?;
        let id: String = db.query_row(
            "SELECT value FROM metadata WHERE key='server_id'",
            [],
            |r| r.get(0),
        )?;
        let epoch: String = db.query_row(
            "SELECT value FROM metadata WHERE key='history_epoch'",
            [],
            |r| r.get(0),
        )?;
        let history_id = format!("{id}{epoch}");
        Ok(Self { db, id, history_id })
    }

    pub fn channels(&self) -> rusqlite::Result<Vec<Channel>> {
        self.db
            .prepare("SELECT name,topic,topic_set_by,topic_set_at FROM channels")?
            .query_map([], |r| {
                Ok(Channel {
                    name: r.get(0)?,
                    topic: r.get(1)?,
                    topic_set_by: r.get(2)?,
                    topic_set_at: r.get(3)?,
                })
            })?
            .collect()
    }

    pub fn ensure_channel(&self, name: &str) -> rusqlite::Result<()> {
        self.db.execute(
            "INSERT INTO channels(name) VALUES (?1) ON CONFLICT(name) DO NOTHING",
            [name],
        )?;
        Ok(())
    }

    pub fn set_topic(&self, name: &str, topic: &str, setter: &str) -> rusqlite::Result<Channel> {
        let channel = Channel {
            name: name.into(),
            topic: topic.into(),
            topic_set_by: setter.into(),
            topic_set_at: Utc::now().timestamp(),
        };
        let changed = self.db.execute(
            "UPDATE channels SET topic=?2,topic_set_by=?3,topic_set_at=?4 WHERE name=?1",
            params![name, topic, setter, channel.topic_set_at],
        )?;
        if changed != 1 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        Ok(channel)
    }

    /// SQLite's FULL-synchronous commit completes before the event is returned
    /// to the broadcaster. Failed inserts must never appear on the wire.
    pub fn append(&mut self, event: &mut Event, host: &str, workdir: &str) -> rusqlite::Result<()> {
        event.time = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        self.db.execute("INSERT INTO events(channel,time,nick,username,peer,host_label,workdir,body,command,target,sender) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![event.channel, event.time, event.nick, event.user, event.peer, host, workdir, event.body, event.command, event.target, event.sender])?;
        event.id = self.db.last_insert_rowid();
        Ok(())
    }

    pub fn history(&self, channel: &str, after: i64, limit: usize) -> rusqlite::Result<Vec<Event>> {
        let mut statement = self.db.prepare("SELECT id,channel,time,nick,username,peer,body,command,target,sender FROM events WHERE channel=?1 AND id>?2 ORDER BY id LIMIT ?3")?;
        statement
            .query_map(params![channel, after, limit as i64], |r| {
                Ok(Event {
                    id: r.get(0)?,
                    channel: r.get(1)?,
                    time: r.get(2)?,
                    nick: r.get(3)?,
                    user: r.get(4)?,
                    peer: r.get(5)?,
                    body: r.get(6)?,
                    command: r.get(7)?,
                    target: r.get(8)?,
                    sender: r.get(9)?,
                })
            })?
            .collect()
    }

    pub fn remember_peer(&self, actor: &str, peer: &str, nick: &str) -> rusqlite::Result<()> {
        self.db.execute("INSERT INTO dm_peers VALUES (?1,?2,?3) ON CONFLICT(actor,peer) DO UPDATE SET nick=excluded.nick",params![actor,peer,nick])?;
        Ok(())
    }
    pub fn append_dm(
        &mut self,
        event: &mut Event,
        host: &str,
        workdir: &str,
        actor: &str,
        peer: &str,
    ) -> rusqlite::Result<()> {
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            self.append(event, host, workdir)?;
            self.remember_peer(actor, peer, &event.target)?;
            self.remember_peer(peer, actor, &event.nick)?;
            self.db.execute_batch("COMMIT")
        })();
        if result.is_err() {
            let _ = self.db.execute_batch("ROLLBACK");
        }
        result
    }
    pub fn rename_peer(&self, actor: &str, nick: &str) -> rusqlite::Result<()> {
        self.db.execute(
            "UPDATE dm_peers SET nick=?2 WHERE peer=?1",
            params![actor, nick],
        )?;
        Ok(())
    }
    pub fn peers(&self, actor: &str) -> rusqlite::Result<Vec<(String, String)>> {
        self.db
            .prepare("SELECT peer,nick FROM dm_peers WHERE actor=?1 ORDER BY peer")?
            .query_map([actor], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect()
    }
    pub fn dm_channel(a: &str, b: &str) -> String {
        if a <= b {
            format!("@{a}:{b}")
        } else {
            format!("@{b}:{a}")
        }
    }
    pub fn latest(&self) -> rusqlite::Result<i64> {
        self.db.query_row(
            "SELECT coalesce((SELECT seq FROM sqlite_sequence WHERE name='events'),0)",
            [],
            |r| r.get(0),
        )
    }
}
