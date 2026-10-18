//! Filesystem snapshots and bounded, volatile outgoing requests.
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use hearth_irc::irc;

pub const MAX_LINE: usize = 400;
pub const MAX_WRITE: usize = 64 * 1024;
pub const MAX_HANDLES: usize = 1024;
pub const MAX_OUTBOUND: usize = 256;
pub const MAX_CHANNELS: usize = 128;
pub const MOUNT_README: &str = "Files for agents. Chat for humans.\n\
Read channels/<name>.log with cat or tail -n; reopen to see new messages.\n\
Send: printf 'hello\\n' >> channels/general.log\n\
Join/create: printf 'JOIN #research\\n' >> control\n\
Leave: printf 'PART #research\\n' >> control\n\
Topic: printf 'New topic\\n' >> channels/research.topic (blank line clears)\n\
Members: cat channels/research.members\n\
Discover: printf 'LIST\\n' >> control; then cat rooms\n\
Identity: printf 'NICK newnick\\nWHOIS othernick\\n' >> control\n\
Replies, notices, direct messages, and service replies: cat events\n\
DMs: /msg nick starts a conversation; cat/append users/<nick>.log (ls users for stable names).\n\
Actions: /me waves. Presence: /away busy, /away to return. Invitations: /invite nick #channel.\n\
Channel and DM history lives on the server; local SQLite logs have no configured byte cap.\n\
IRC slash commands (/nick, /join, /topic, /msg, /whois) also work in channel logs.\n\
Use // to send a message beginning with /. Topic files always take literal text.\n\
Append only; UTF-8; newline required. Check status/events for asynchronous errors.\n\
Writes queue locally; fsync is not a delivery barrier. Queues are volatile.\n";

#[derive(Clone, Debug)]
pub struct Outbound {
    pub wire: String,
    pub message: Option<(String, String)>,
}

#[derive(Default)]
pub struct Remote {
    pub ready: bool,
    pub server_id: Option<String>,
    pub queue: VecDeque<Outbound>,
    pub inflight: Option<Outbound>,
    pub acknowledged: u64,
    pub canceled: u64,
    pub uncertain: u64,
    pub reconnects: u64,
    pub error: String,
}

pub type Result<T> = std::result::Result<T, i32>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Node {
    Root,
    Channels,
    Status,
    Readme,
    Control,
    Events,
    Rooms,
    Users,
    Dm(usize),
    Log(usize),
    Topic(usize),
    Members(usize),
}

impl Node {
    pub fn from_ino(ino: u64) -> Result<Self> {
        Ok(match ino {
            1 => Self::Root,
            2 => Self::Channels,
            4 => Self::Status,
            5 => Self::Readme,
            6 => Self::Control,
            7 => Self::Events,
            8 => Self::Rooms,
            9 => Self::Users,
            n if n >= 1_000_000 => Self::Dm((n - 1_000_000) as usize),
            n if (16..16 + MAX_CHANNELS as u64 * 3).contains(&n) => {
                let index = ((n - 16) / 3) as usize;
                match (n - 16) % 3 {
                    0 => Self::Log(index),
                    1 => Self::Topic(index),
                    _ => Self::Members(index),
                }
            }
            _ => return Err(libc::ENOENT),
        })
    }
    pub fn ino(self) -> u64 {
        match self {
            Self::Root => 1,
            Self::Channels => 2,
            Self::Status => 4,
            Self::Readme => 5,
            Self::Control => 6,
            Self::Events => 7,
            Self::Rooms => 8,
            Self::Users => 9,
            Self::Dm(i) => 1_000_000 + i as u64,
            Self::Log(i) => 16 + i as u64 * 3,
            Self::Topic(i) => 17 + i as u64 * 3,
            Self::Members(i) => 18 + i as u64 * 3,
        }
    }
    pub fn is_dir(self) -> bool {
        matches!(self, Self::Root | Self::Channels | Self::Users)
    }
    pub fn writable(self) -> bool {
        matches!(
            self,
            Self::Log(_) | Self::Topic(_) | Self::Dm(_) | Self::Control
        )
    }
}

pub struct Conversation {
    pub peer: String,
    pub nick: String,
    pub cursor: i64,
    pub ready: bool,
    history: usize,
}

pub struct Channel {
    pub name: String,
    pub desired: bool,
    pub joined: bool,
    pub ready: bool,
    pub cursor: i64,
    history: usize,
    modified: SystemTime,
    pub topic: String,
    pub members: BTreeMap<String, String>,
}

enum Handle {
    History {
        node: Node,
        end: usize,
        modified: SystemTime,
    },
    Text {
        node: Node,
        bytes: Vec<u8>,
    },
    Writer {
        node: Node,
        partial: Vec<u8>,
    },
    Directory {
        node: Node,
        entries: Vec<(Node, String)>,
    },
}
impl Handle {
    fn node(&self) -> Node {
        match self {
            Self::History { node, .. }
            | Self::Text { node, .. }
            | Self::Writer { node, .. }
            | Self::Directory { node, .. } => *node,
        }
    }
}

pub struct Model {
    pub remote: Option<Remote>,
    pub channels: Vec<Channel>,
    pub conversations: Vec<Conversation>,
    pub away: String,
    pub rooms: BTreeMap<String, (usize, String)>,
    events: usize,
    logs: crate::local_store::Logs,
    dropped_events: u64,
    history_bytes: usize,
    handles: HashMap<u64, Handle>,
    next_handle: u64,
    pub nick: String,
    pub location: Option<(String, String)>,
    pub started: SystemTime,
    messages: u64,
    discarded_partials: u64,
    last_error: &'static str,
}

impl Model {
    pub fn new(nick: String) -> Self {
        let mut model = Self {
            remote: None,
            channels: Vec::new(),
            conversations: Vec::new(),
            away: String::new(),
            rooms: BTreeMap::new(),
            events: 0,
            logs: crate::local_store::Logs::new().expect("create SQLite log spool"),
            dropped_events: 0,
            history_bytes: 0,
            handles: HashMap::new(),
            next_handle: 1,
            nick,
            location: None,
            started: SystemTime::now(),
            messages: 0,
            discarded_partials: 0,
            last_error: "none",
        };
        let i = model.ensure_channel("#general").unwrap();
        model.channels[i].desired = true;
        model.channels[i].joined = true;
        model.channels[i].ready = true;
        model
    }

    pub fn configure_channels(&mut self, names: &[String]) -> Result<()> {
        self.channels.clear(); // Only called before mounting; inode identities then remain stable.
        for name in names {
            let i = self.ensure_channel(name)?;
            self.channels[i].desired = true;
            self.channels[i].joined = self.remote.is_none();
            self.channels[i].ready = self.remote.is_none();
        }
        Ok(())
    }
    pub fn index(&self, name: &str) -> Option<usize> {
        self.channels.iter().position(|c| c.name == irc::fold(name))
    }
    pub fn ensure_channel(&mut self, name: &str) -> Result<usize> {
        if !irc::valid_channel(name) {
            return Err(libc::EINVAL);
        }
        if let Some(i) = self.index(name) {
            return Ok(i);
        }
        if self.channels.len() >= MAX_CHANNELS {
            return Err(libc::ENOSPC);
        }
        let i = self.channels.len();
        self.channels.push(Channel {
            name: irc::fold(name),
            desired: false,
            joined: false,
            ready: false,
            cursor: 0,
            history: 0,
            modified: SystemTime::now(),
            topic: String::new(),
            members: BTreeMap::new(),
        });
        Ok(i)
    }
    fn check_node(&self, node: Node) -> Result<()> {
        if matches!(node,Node::Dm(i) if i>=self.conversations.len()) {
            return Err(libc::ENOENT);
        }
        if matches!(node, Node::Log(i) | Node::Topic(i) | Node::Members(i) if i >= self.channels.len())
        {
            return Err(libc::ENOENT);
        }
        Ok(())
    }
    pub fn entries(&self, node: Node) -> Result<Vec<(Node, String)>> {
        let mut entries = vec![(node, ".".into()), (Node::Root, "..".into())];
        match node {
            Node::Root => entries.extend(
                [
                    (Node::Channels, "channels"),
                    (Node::Status, "status"),
                    (Node::Readme, "README"),
                    (Node::Control, "control"),
                    (Node::Events, "events"),
                    (Node::Rooms, "rooms"),
                    (Node::Users, "users"),
                ]
                .map(|(n, s)| (n, s.into())),
            ),
            Node::Channels => {
                for (i, c) in self.channels.iter().enumerate() {
                    for (node, suffix) in [
                        (Node::Log(i), "log"),
                        (Node::Topic(i), "topic"),
                        (Node::Members(i), "members"),
                    ] {
                        entries.push((node, format!("{}.{suffix}", &c.name[1..])));
                    }
                }
            }
            Node::Users => {
                for (i, c) in self.conversations.iter().enumerate() {
                    entries.push((
                        Node::Dm(i),
                        format!("{}~{}.log", irc::fold(&c.nick), c.peer),
                    ));
                }
            }
            _ => return Err(libc::ENOTDIR),
        }
        Ok(entries)
    }
    pub fn lookup(&self, parent: Node, name: &str) -> Result<Node> {
        if parent == Node::Users {
            if let Some(nick) = name.strip_suffix(".log") {
                let found: Vec<_> = self
                    .conversations
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| irc::fold(&c.nick) == nick)
                    .collect();
                if found.len() == 1 {
                    return Ok(Node::Dm(found[0].0));
                }
            }
        }

        self.entries(parent)?
            .into_iter()
            .find(|(_, n)| n == name)
            .map(|(n, _)| n)
            .ok_or(libc::ENOENT)
    }
    fn allocate(&mut self, handle: Handle) -> Result<u64> {
        if self.handles.len() >= MAX_HANDLES {
            return Err(libc::EMFILE);
        }
        let id = self.next_handle;
        self.next_handle = id.checked_add(1).ok_or(libc::EMFILE)?;
        self.handles.insert(id, handle);
        Ok(id)
    }
    pub fn open_dir(&mut self, node: Node) -> Result<u64> {
        self.allocate(Handle::Directory {
            node,
            entries: self.entries(node)?,
        })
    }
    pub fn read_dir(&self, node: Node, id: u64) -> Result<&[(Node, String)]> {
        match self.handle(node, id)? {
            Handle::Directory { entries, .. } => Ok(entries),
            _ => Err(libc::ENOTDIR),
        }
    }
    pub fn status(&self) -> String {
        let (backend, connection, persistence, pending, limit, error) = match &self.remote {
            Some(r) => (
                "irc",
                if r.ready {
                    "ready"
                } else {
                    "offline or replaying"
                },
                "server SQLite; local SQLite spool; outgoing queue volatile",
                r.queue.len() + usize::from(r.inflight.is_some()),
                irc::MAX_MESSAGE,
                if r.error.is_empty() {
                    self.last_error
                } else {
                    &r.error
                },
            ),
            None => (
                "loopback",
                "disabled (FUSE spike)",
                "none",
                0,
                MAX_LINE,
                self.last_error,
            ),
        };
        let names = self
            .channels
            .iter()
            .filter(|c| c.desired)
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let network = self.remote.as_ref().map(|r| format!("server_id: {}\nacknowledged_outbound: {}\ncanceled_outbound: {}\nuncertain_outbound: {}\nreconnects: {}\n", r.server_id.as_deref().unwrap_or("unknown"), r.acknowledged,r.canceled,r.uncertain,r.reconnects)).unwrap_or_default();
        let channel_status = self
            .channels
            .iter()
            .map(|c| {
                format!(
                    "channel: {} joined={} ready={} last_event_id={}\n",
                    c.name, c.joined, c.ready, c.cursor
                )
            })
            .collect::<String>();
        format!(
            "away: {}\nbackend: {backend}\nconnection: {connection}\nnickname: {}\nchannels: {names}\npersistence: {persistence}\npending_outbound: {pending}\nmessages: {}\nhistory_bytes: {}\nhistory_storage: SQLite disk spool; no configured size limit\nmax_message_bytes: {limit}\nopen_handles: {}\ndiscarded_partial_lines: {}\ndropped_events: {}\nlast_error: {error}\n{network}{channel_status}",
            self.away,
            self.nick,
            self.messages,
            self.history_bytes,
            self.handles.len(),
            self.discarded_partials,
            self.dropped_events
        )
    }
    pub fn event(&mut self, line: &str) {
        let record = format!("{line}\n");
        if self
            .logs
            .append(Node::Events.ino(), self.events, record.as_bytes())
            .is_ok()
        {
            self.events += record.len();
        } else {
            self.dropped_events += 1;
            self.last_error = "SQLite event spool write failed";
        }
    }
    pub fn receive(
        &mut self,
        channel: &str,
        id: i64,
        time: &str,
        nick: &str,
        body: &str,
    ) -> Result<()> {
        let i = self.index(channel).ok_or(libc::ENOENT)?;
        if id <= self.channels[i].cursor {
            return Ok(());
        }
        let timestamp = chrono::DateTime::parse_from_rfc3339(time).map_err(|_| libc::EINVAL)?;
        let display = if let Some(action) = irc::action(body) {
            format!("* {nick} {action}")
        } else {
            format!("{nick}: {body}")
        };
        let record = format!(
            "[{}] {display}\n",
            timestamp
                .with_timezone(&Utc)
                .format("%Y-%m-%d %H:%M:%S UTC")
        );
        self.logs
            .append(
                Node::Log(i).ino(),
                self.channels[i].history,
                record.as_bytes(),
            )
            .map_err(|_| libc::EIO)?;
        self.history_bytes += record.len();
        let c = &mut self.channels[i];
        c.history += record.len();
        c.modified = SystemTime::now();
        c.cursor = id;
        self.messages += 1;
        Ok(())
    }
    pub fn peer(&mut self, peer: &str, nick: &str) -> Result<usize> {
        if peer.len() != 32
            || !peer.bytes().all(|b| b.is_ascii_hexdigit())
            || !irc::valid_nick(nick)
        {
            return Err(libc::EINVAL);
        }
        if let Some(i) = self.conversations.iter().position(|c| c.peer == peer) {
            self.conversations[i].nick = nick.into();
            return Ok(i);
        }
        let i = self.conversations.len();
        self.conversations.push(Conversation {
            peer: peer.into(),
            nick: nick.into(),
            cursor: 0,
            ready: false,
            history: 0,
        });
        Ok(i)
    }
    pub fn receive_dm(
        &mut self,
        peer: &str,
        id: i64,
        time: &str,
        nick: &str,
        body: &str,
    ) -> Result<()> {
        let i = self
            .conversations
            .iter()
            .position(|c| c.peer == peer)
            .ok_or(libc::ENOENT)?;
        let c = &mut self.conversations[i];
        if id <= c.cursor {
            return Ok(());
        }
        let time = chrono::DateTime::parse_from_rfc3339(time).map_err(|_| libc::EINVAL)?;
        let display = if let Some(action) = irc::action(body) {
            format!("* {nick} {action}")
        } else {
            format!("{nick}: {body}")
        };
        let record = format!(
            "[{}] {display}\n",
            time.with_timezone(&Utc).format("%Y-%m-%d %H:%M:%S UTC")
        );
        self.logs
            .append(Node::Dm(i).ino(), c.history, record.as_bytes())
            .map_err(|_| libc::EIO)?;
        c.history += record.len();
        c.cursor = id;
        self.history_bytes += record.len();
        self.messages += 1;
        Ok(())
    }
    pub fn network_failed(&mut self, reason: String) {
        if let Some(r) = &mut self.remote {
            r.ready = false;
            r.canceled += r.queue.len() as u64;
            r.uncertain += u64::from(r.inflight.is_some());
            r.queue.clear();
            r.inflight = None;
            r.reconnects += 1;
            r.error = reason;
        }
        for c in &mut self.conversations {
            c.ready = false;
        }
        for c in &mut self.channels {
            c.joined = false;
            c.ready = false;
            c.members.clear();
        }
    }
    fn handle(&self, node: Node, id: u64) -> Result<&Handle> {
        self.handles
            .get(&id)
            .filter(|h| h.node() == node)
            .ok_or(libc::EBADF)
    }
    fn text(&self, node: Node) -> Result<String> {
        self.check_node(node)?;
        Ok(match node {
            Node::Status => self.status(),
            Node::Readme => MOUNT_README.into(),
            Node::Control => String::new(),
            Node::Topic(i) => format!("{}\n", self.channels[i].topic),
            Node::Members(i) => self.channels[i]
                .members
                .values()
                .map(|n| format!("{n}\n"))
                .collect(),
            Node::Rooms => self
                .rooms
                .iter()
                .map(|(name, (count, topic))| format!("{name}\t{count}\t{topic}\n"))
                .collect(),
            _ => return Err(libc::EINVAL),
        })
    }
    pub fn metadata(&self, node: Node, id: Option<u64>) -> Result<(usize, SystemTime)> {
        self.check_node(node)?;
        if let Some(id) = id {
            match self.handle(node, id)? {
                Handle::History { end, modified, .. } => return Ok((*end, *modified)),
                Handle::Text { bytes, .. } => return Ok((bytes.len(), self.started)),
                _ => {}
            }
        }
        Ok(match node {
            Node::Root | Node::Channels | Node::Users => (0, self.started),
            Node::Dm(i) => (self.conversations[i].history, self.started),
            Node::Log(i) => (self.channels[i].history, self.channels[i].modified),
            Node::Events => (self.events, self.started),
            _ => (self.text(node)?.len(), SystemTime::now()),
        })
    }
    pub fn open(&mut self, node: Node, flags: i32) -> Result<u64> {
        self.check_node(node)?;
        if node.is_dir() {
            return Err(libc::EISDIR);
        }
        if flags & libc::O_TRUNC != 0 {
            return Err(libc::EPERM);
        }
        if flags & (libc::O_SYNC | libc::O_DSYNC) != 0 {
            return Err(libc::EOPNOTSUPP);
        }
        let handle = match flags & libc::O_ACCMODE {
            libc::O_RDONLY => match node {
                Node::Log(_) | Node::Dm(_) | Node::Events => {
                    let (end, modified) = self.metadata(node, None)?;
                    Handle::History {
                        node,
                        end,
                        modified,
                    }
                }
                _ => Handle::Text {
                    node,
                    bytes: self.text(node)?.into_bytes(),
                },
            },
            libc::O_WRONLY if !node.writable() => return Err(libc::EACCES),
            libc::O_WRONLY if flags & libc::O_APPEND != 0 => Handle::Writer {
                node,
                partial: Vec::new(),
            },
            libc::O_WRONLY => return Err(libc::EPERM),
            _ => return Err(libc::EOPNOTSUPP),
        };
        self.allocate(handle)
    }
    pub fn read(&self, node: Node, id: u64, offset: u64, size: u32) -> Result<Vec<u8>> {
        match self.handle(node, id)? {
            Handle::History { end, .. } => {
                let start = usize::try_from(offset).unwrap_or(usize::MAX).min(*end);
                self.logs
                    .read(node.ino(), start, (size as usize).min(end - start))
                    .map_err(|_| libc::EIO)
            }
            Handle::Text { bytes, .. } => {
                let start = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(bytes.len());
                Ok(bytes[start..start.saturating_add(size as usize).min(bytes.len())].to_vec())
            }
            _ => Err(libc::EBADF),
        }
    }
    pub fn write(&mut self, node: Node, id: u64, data: &[u8], flags: i32) -> Result<usize> {
        let result = self.write_inner(node, id, data, flags);
        if let Err(error) = result {
            self.last_error = match error {
                libc::ENOSPC => "history full; no bytes accepted by this callback",
                libc::EMSGSIZE => "message or write too large",
                libc::EILSEQ => "invalid UTF-8",
                libc::EINVAL => "invalid message or command",
                libc::ENOTCONN => "server offline, channel parted, or history replay incomplete",
                libc::EAGAIN => "outgoing queue full; no bytes accepted by this callback",
                _ => "write rejected",
            };
        }
        result
    }
    fn write_inner(&mut self, node: Node, id: u64, data: &[u8], flags: i32) -> Result<usize> {
        let Handle::Writer { partial, .. } = self.handle(node, id)? else {
            return Err(libc::EBADF);
        };
        if flags & libc::O_APPEND == 0 {
            return Err(libc::EPERM);
        }
        if data.len() > MAX_WRITE {
            return Err(libc::EMSGSIZE);
        }
        let limit = if node == Node::Control {
            510
        } else if self.remote.is_some() {
            irc::MAX_MESSAGE
        } else {
            MAX_LINE
        };
        let mut pending = partial.clone();
        pending.extend_from_slice(data);
        let mut lines = Vec::new();
        let mut start = 0;
        for (i, byte) in pending.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            let line = pending[start..i]
                .strip_suffix(b"\r")
                .unwrap_or(&pending[start..i]);
            validate_line(line, true, limit)?;
            if !line.is_empty() || matches!(node, Node::Topic(_)) {
                lines.push(std::str::from_utf8(line).map_err(|_| libc::EILSEQ)?);
            }
            start = i + 1;
        }
        let remainder = &pending[start..];
        validate_line(
            remainder.strip_suffix(b"\r").unwrap_or(remainder),
            false,
            limit,
        )?;
        if self.remote.is_some() {
            if !self.remote.as_ref().unwrap().ready {
                return Err(libc::ENOTCONN);
            }
            let mut requests = Vec::new();
            for line in lines {
                let request = match node {
                    Node::Control => command(line)?,
                    Node::Topic(i) => {
                        if !self.channels[i].ready {
                            return Err(libc::ENOTCONN);
                        }
                        command(&format!("TOPIC {} :{line}", self.channels[i].name))?
                    }
                    Node::Dm(i) => {
                        let c = &self.conversations[i];
                        if !c.ready {
                            return Err(libc::ENOTCONN);
                        }
                        let mut request = if let Some(slash) =
                            line.strip_prefix('/').filter(|s| !s.starts_with('/'))
                        {
                            slash_command(slash, &c.nick)?
                        } else {
                            command(&format!(
                                "PRIVMSG {} :{}",
                                c.nick,
                                if line.starts_with("//") {
                                    &line[1..]
                                } else {
                                    line
                                }
                            ))?
                        };
                        let parsed = irc::parse(&request.wire).map_err(|_| libc::EINVAL)?;
                        if parsed.command == "PRIVMSG"
                            && parsed.params.len() == 2
                            && irc::fold(&parsed.params[0]) == irc::fold(&c.nick)
                        {
                            request.wire = format!("HEARTH DMSG {} :{}", c.peer, parsed.params[1]);
                            request.message =
                                Some((format!("@{}", c.peer), parsed.params[1].clone()));
                        }
                        request
                    }
                    Node::Log(i) => {
                        if let Some(slash) = line.strip_prefix('/').filter(|s| !s.starts_with('/'))
                        {
                            slash_command(slash, &self.channels[i].name)?
                        } else {
                            if !self.channels[i].ready {
                                return Err(libc::ENOTCONN);
                            }
                            let body = if line.starts_with("//") {
                                &line[1..]
                            } else {
                                line
                            };
                            command(&format!("PRIVMSG {} :{body}", self.channels[i].name))?
                        }
                    }
                    _ => return Err(libc::EACCES),
                };
                requests.push(request);
            }
            let r = self.remote.as_mut().unwrap();
            if r.queue.len() + usize::from(r.inflight.is_some()) + requests.len() > MAX_OUTBOUND {
                return Err(libc::EAGAIN);
            }
            r.queue.extend(requests);
        } else {
            let Node::Log(i) = node else {
                return Err(libc::EOPNOTSUPP);
            };
            let now = SystemTime::now();
            let timestamp = DateTime::<Utc>::from(now).format("%Y-%m-%d %H:%M:%S UTC");
            let records = lines
                .iter()
                .map(|line| format!("[{timestamp}] {}: {line}\n", self.nick))
                .collect::<String>();
            self.logs
                .append(node.ino(), self.channels[i].history, records.as_bytes())
                .map_err(|_| libc::EIO)?;
            self.history_bytes += records.len();
            self.messages += lines.len() as u64;
            if !records.is_empty() {
                self.channels[i].history += records.len();
                self.channels[i].modified = now;
            }
        }
        self.handles.insert(
            id,
            Handle::Writer {
                node,
                partial: remainder.to_vec(),
            },
        );
        Ok(data.len())
    }
    pub fn flush(&mut self, node: Node, id: u64) -> Result<()> {
        if matches!(self.handle(node,id)?, Handle::Writer {partial,..} if !partial.is_empty()) {
            self.last_error = "unterminated line; append a newline before close";
            return Err(libc::EINVAL);
        }
        Ok(())
    }
    pub fn release(&mut self, node: Node, id: u64) -> Result<()> {
        self.handle(node, id)?;
        if let Some(Handle::Writer { partial, .. }) = self.handles.remove(&id) {
            if !partial.is_empty() {
                self.discarded_partials += 1;
                self.last_error = "discarded unterminated line on final release";
                log::warn!(
                    "discarded unterminated line fh={id} bytes={}",
                    partial.len()
                );
            }
        }
        Ok(())
    }
}

fn validate_line(bytes: &[u8], complete: bool, limit: usize) -> Result<()> {
    if bytes.len() > limit {
        return Err(libc::EMSGSIZE);
    }
    if bytes.iter().any(|b| *b < 0x20 || *b == 0x7f) {
        return Err(libc::EINVAL);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) if text.chars().any(char::is_control) => Err(libc::EINVAL),
        Ok(_) => Ok(()),
        Err(e) if !complete && e.error_len().is_none() => Ok(()),
        Err(_) => Err(libc::EILSEQ),
    }
}

/// Restrict controls to application commands: credentials and connection state
/// remain owned by the adapter. The server validates arguments and permissions.
pub fn command(line: &str) -> Result<Outbound> {
    if line.len() > 510 || line.contains(['\r', '\n', '\0']) {
        return Err(libc::EINVAL);
    }
    let m = irc::parse(line).map_err(|_| libc::EINVAL)?;
    if m.prefix.is_some() || !m.tags.is_empty() {
        return Err(libc::EINVAL);
    }
    for (i, param) in m.params.iter().enumerate() {
        let message = matches!(m.command.as_str(), "PRIVMSG" | "NOTICE") && i == 1;
        if !(if message {
            irc::message_ok(param)
        } else {
            irc::text_ok(param, 510)
        }) {
            return Err(libc::EINVAL);
        }
    }
    let allowed = matches!(
        m.command.as_str(),
        "JOIN"
            | "PART"
            | "TOPIC"
            | "NICK"
            | "LIST"
            | "NAMES"
            | "WHO"
            | "WHOIS"
            | "PRIVMSG"
            | "NOTICE"
            | "MODE"
            | "MOTD"
            | "VERSION"
            | "TIME"
            | "LUSERS"
            | "AWAY"
            | "INVITE"
    ) || (m.command == "HEARTH"
        && m.params
            .first()
            .is_some_and(|s| s.eq_ignore_ascii_case("LOCATION")));
    if !allowed {
        return Err(libc::EINVAL);
    }
    let message = if matches!(m.command.as_str(), "PRIVMSG" | "NOTICE")
        && m.params.len() == 2
        && !matches!(irc::fold(&m.params[0]).as_str(), "nickserv" | "chanserv")
    {
        Some((irc::fold(&m.params[0]), m.params[1].clone()))
    } else {
        None
    };
    Ok(Outbound {
        wire: line.into(),
        message,
    })
}

fn slash_command(line: &str, channel: &str) -> Result<Outbound> {
    let (verb, args) = line.split_once(' ').unwrap_or((line, ""));
    let args = args.trim_start();
    let wire = match verb.to_ascii_uppercase().as_str() {
        "ME" => format!("PRIVMSG {channel} :\x01ACTION {args}\x01"),
        "AWAY" => {
            if args.is_empty() {
                "AWAY".into()
            } else {
                format!("AWAY :{args}")
            }
        }
        "INVITE" => {
            if args.contains(' ') {
                format!("INVITE {args}")
            } else {
                format!("INVITE {args} {channel}")
            }
        }
        "NS" | "NICKSERV" => format!("PRIVMSG NickServ :{args}"),
        "CS" | "CHANSERV" => format!("PRIVMSG ChanServ :{args}"),
        "MSG" | "NOTICE" => {
            let (target, body) = args.split_once(' ').ok_or(libc::EINVAL)?;
            format!(
                "{} {target} :{body}",
                if verb.eq_ignore_ascii_case("msg") {
                    "PRIVMSG"
                } else {
                    "NOTICE"
                }
            )
        }
        "TOPIC" => {
            if args.starts_with('#') {
                if let Some((target, topic)) = args.split_once(' ') {
                    format!(
                        "TOPIC {target} :{}",
                        topic.strip_prefix(':').unwrap_or(topic)
                    )
                } else {
                    format!("TOPIC {args}")
                }
            } else if args.is_empty() {
                format!("TOPIC {channel}")
            } else {
                format!(
                    "TOPIC {channel} :{}",
                    args.strip_prefix(':').unwrap_or(args)
                )
            }
        }
        "PART" if args.is_empty() => format!("PART {channel}"),
        "QUOTE" => args.into(),
        _ => line.into(),
    };
    command(&wire)
}

#[cfg(test)]
#[path = "core_tests.rs"]
mod tests;
