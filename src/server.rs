//! Single-owner server state. Socket tasks use bounded mailboxes; SQLite work
//! runs on a dedicated blocking task, in the same order as channel broadcasts.
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use subtle::ConstantTimeEq;

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, mpsc};
use tokio_rustls::TlsAcceptor;

use crate::irc::{self, Message};
use crate::storage::{Channel, Event as Record, Store};
use crate::transport::BoxStream;

const SEND_QUEUE: usize = 256;
const MAX_CHANNELS: usize = 128;
const MAX_JOINED: usize = 32;
type Id = u64;

fn service_name(nick: &str) -> Option<&'static str> {
    match irc::fold(nick).as_str() {
        "nickserv" => Some("NickServ"),
        "chanserv" => Some("ChanServ"),
        _ => None,
    }
}

pub struct Settings {
    pub password: Option<String>,
    pub channels: BTreeSet<String>,
    pub max_clients: usize,
}

enum Input {
    Connect(Id, SocketAddr, mpsc::Sender<String>),
    Line(Id, String),
    Disconnect(Id),
    Tick,
    Stop,
}

struct Client {
    tx: mpsc::Sender<String>,
    peer: String,
    nick: String,
    user: String,
    realname: String,
    host_label: String,
    workdir: String,
    actor: String,
    away: String,
    authenticated: bool,
    registered: bool,
    cap_pending: bool,
    caps: BTreeSet<String>,
    channels: BTreeSet<String>,
    connected: Instant,
    signon: i64,
    active: Instant,
    ping: Option<Instant>,
    tokens: f64,
    refill: Instant,
}

impl Client {
    fn new(peer: String, tx: mpsc::Sender<String>, authenticated: bool) -> Self {
        let now = Instant::now();
        Self {
            tx,
            peer,
            authenticated,
            nick: String::new(),
            user: String::new(),
            realname: String::new(),
            host_label: String::new(),
            workdir: String::new(),
            actor: crate::identity::actor(
                &crate::identity::random_key().expect("secure random identity"),
            ),
            away: String::new(),
            registered: false,
            cap_pending: false,
            caps: BTreeSet::new(),
            channels: BTreeSet::new(),
            connected: now,
            signon: chrono::Utc::now().timestamp(),
            active: now,
            ping: None,
            tokens: 100.0,
            refill: now,
        }
    }

    fn prefix(&self) -> String {
        format!("{}!{}@{}", self.nick, self.user, self.peer)
    }
}

struct Server {
    store: Store,
    settings: Settings,
    channels: BTreeMap<String, Channel>,
    clients: BTreeMap<Id, Client>,
    failed: BTreeSet<Id>,
    batch_id: u64,
}

impl Server {
    fn new(store: Store, settings: Settings) -> rusqlite::Result<Self> {
        for channel in &settings.channels {
            store.ensure_channel(channel)?;
        }
        let channels = store
            .channels()?
            .into_iter()
            .map(|c| (c.name.clone(), c))
            .collect();
        Ok(Self {
            store,
            settings,
            channels,
            clients: BTreeMap::new(),
            failed: BTreeSet::new(),
            batch_id: 0,
        })
    }

    fn send(&mut self, id: Id, line: String) {
        // IRCv3 tags have a separate size budget. Never truncate protocol data.
        let body = if line.starts_with('@') {
            line.split_once(' ').map(|(_, b)| b).unwrap_or(&line)
        } else {
            &line
        };
        if body.len() + 2 > irc::MAX_FRAME || line.contains(['\r', '\n', '\0']) {
            log::error!("refused invalid outbound frame for connection {id}");
            self.failed.insert(id);
            return;
        }
        if let Some(client) = self.clients.get(&id) {
            if client.tx.try_send(format!("{line}\r\n")).is_err() {
                self.failed.insert(id);
            }
        }
    }

    fn numeric(&mut self, id: Id, code: u16, text: impl AsRef<str>) {
        let nick = self
            .clients
            .get(&id)
            .map(|c| c.nick.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("*");
        self.send(id, format!(":hearth {code:03} {nick} {}", text.as_ref()));
    }

    fn peers(&self, id: Id) -> Vec<Id> {
        let Some(client) = self.clients.get(&id) else {
            return Vec::new();
        };
        let correspondents = self.store.peers(&client.actor).unwrap_or_default();
        self.clients
            .iter()
            .filter(|(other, c)| {
                **other == id
                    || !c.channels.is_disjoint(&client.channels)
                    || correspondents.iter().any(|(actor, _)| actor == &c.actor)
            })
            .map(|(id, _)| *id)
            .collect()
    }

    fn broadcast(&mut self, channel: &str, line: &str, except: Option<Id>) {
        let ids: Vec<_> = self
            .clients
            .iter()
            .filter(|(id, c)| Some(**id) != except && c.channels.contains(channel))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.send(id, line.to_owned());
        }
    }

    fn disconnect(&mut self, id: Id, reason: &str) {
        if let Some(c) = self.clients.get(&id) {
            if c.registered {
                let line = format!(":{} QUIT :{reason}", c.prefix());
                for other in self.peers(id) {
                    if other != id {
                        self.send(other, line.clone());
                    }
                }
            }
        }
        self.clients.remove(&id);
    }

    fn reap(&mut self) {
        while let Some(id) = self.failed.pop_first() {
            self.disconnect(id, "Send queue exceeded");
        }
    }

    fn register(&mut self, id: Id) {
        let c = &self.clients[&id];
        if c.registered || c.cap_pending || c.nick.is_empty() || c.user.is_empty() {
            return;
        }
        self.clients.get_mut(&id).unwrap().registered = true;
        self.numeric(id, 1, ":Welcome to hearth");
        self.numeric(id, 2, ":Your host is hearth, running hearth-ircd-0.1");
        self.numeric(
            id,
            3,
            ":This server stores channel messages before broadcasting",
        );
        self.numeric(id, 4, "hearth hearth-ircd-0.1 - n");
        self.numeric(id, 5, format!("CHANTYPES=# CHANMODES=,,,n PREFIX= CASEMAPPING=ascii NICKLEN=32 CHANNELLEN=32 CHANLIMIT=#:{MAX_JOINED} TOPICLEN=350 NETWORK=hearth HEARTH=1 SERVERID={} :are supported by this server", self.store.history_id));
        self.numeric(
            id,
            5,
            "CHATHISTORY=100 MSGREFTYPES=msgid,timestamp :are supported by this server",
        );
        self.numeric(id, 422, ":MOTD not configured");
    }

    fn cap(&mut self, id: Id, p: &[String]) {
        let Some(sub) = p.first() else {
            self.numeric(id, 461, "CAP :Not enough parameters");
            return;
        };
        let nick = if self.clients[&id].nick.is_empty() {
            "*"
        } else {
            &self.clients[&id].nick
        }
        .to_owned();
        match sub.to_ascii_uppercase().as_str() {
            "LS" => {
                if !self.clients[&id].registered {
                    self.clients.get_mut(&id).unwrap().cap_pending = true;
                }
                self.send(id, format!(":hearth CAP {nick} LS :{}", irc::CAPABILITIES));
            }
            "LIST" => {
                let caps = self.clients[&id]
                    .caps
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ");
                self.send(id, format!(":hearth CAP {nick} LIST :{caps}"));
            }
            "REQ" if p.len() == 2 => {
                if !self.clients[&id].registered {
                    self.clients.get_mut(&id).unwrap().cap_pending = true;
                }
                let requested: Vec<_> = p[1].split_whitespace().collect();
                let valid = !requested.is_empty()
                    && requested.iter().all(|cap| {
                        irc::CAPABILITIES
                            .split(' ')
                            .any(|v| v == cap.trim_start_matches('-'))
                    });
                if valid {
                    let c = self.clients.get_mut(&id).unwrap();
                    for cap in requested {
                        if let Some(cap) = cap.strip_prefix('-') {
                            c.caps.remove(cap);
                        } else {
                            c.caps.insert(cap.to_owned());
                        }
                    }
                }
                self.send(
                    id,
                    format!(
                        ":hearth CAP {nick} {} :{}",
                        if valid { "ACK" } else { "NAK" },
                        p[1]
                    ),
                );
            }
            "END" => {
                self.clients.get_mut(&id).unwrap().cap_pending = false;
                self.register(id);
            }
            _ => self.numeric(id, 410, format!("{sub} :Invalid CAP command")),
        }
    }

    fn command(&mut self, id: Id, line: &str) {
        let Some(c) = self.clients.get_mut(&id) else {
            return;
        };
        let now = Instant::now();
        c.tokens = (c.tokens + now.duration_since(c.refill).as_secs_f64() * 50.0).min(100.0);
        c.refill = now;
        c.active = now;
        if c.tokens < 1.0 {
            self.send(id, "ERROR :Command rate exceeded".into());
            self.disconnect(id, "Command rate exceeded");
            return;
        }
        c.tokens -= 1.0;
        if line.trim().is_empty() {
            return;
        }
        let Ok(message) = irc::parse(line) else {
            self.numeric(id, 417, ":Malformed input");
            return;
        };
        // Never honor a client-supplied source prefix (identity belongs to socket).
        if message.prefix.is_some() {
            self.numeric(id, 417, ":Client prefix not allowed");
            return;
        }
        let Message {
            command, params: p, ..
        } = message;
        match command.as_str() {
            "CAP" => {
                self.cap(id, &p);
                return;
            }
            "PING" => {
                if let Some(token) = p.first() {
                    self.send(id, format!(":hearth PONG hearth :{}", truncate(token, 200)));
                } else {
                    self.numeric(id, 409, ":No origin specified");
                }
                return;
            }
            "PONG" => {
                self.clients.get_mut(&id).unwrap().ping = None;
                return;
            }
            "QUIT" => {
                self.disconnect(id, "Client quit");
                return;
            }
            "PASS" => {
                if self.clients[&id].registered {
                    self.numeric(id, 462, ":Already registered");
                } else if p.len() != 1 {
                    self.numeric(id, 461, "PASS :Not enough parameters");
                } else {
                    let supplied = &p[0];
                    let base = self.settings.password.as_deref().unwrap_or("");
                    let direct = bool::from(base.as_bytes().ct_eq(supplied.as_bytes()));
                    let keyed = supplied.rsplit_once('/').filter(|(password, key)| {
                        crate::identity::valid_key(key)
                            && bool::from(base.as_bytes().ct_eq(password.as_bytes()))
                    });
                    let c = self.clients.get_mut(&id).unwrap();
                    c.authenticated = direct || keyed.is_some();
                    if let Some((_, key)) = keyed {
                        c.actor = crate::identity::actor(key);
                    }
                }
                return;
            }
            _ => {}
        }
        if !self.clients[&id].authenticated {
            self.numeric(id, 464, ":Password incorrect");
            self.send(id, "ERROR :Password required or incorrect".into());
            self.disconnect(id, "Authentication failed");
            return;
        }
        match command.as_str() {
            "NICK" => {
                let Some(nick) = p.first() else {
                    self.numeric(id, 431, ":No nickname given");
                    return;
                };
                if !irc::valid_nick(nick) {
                    self.numeric(id, 432, "* :Erroneous nickname");
                    return;
                }
                if service_name(nick).is_some()
                    || self
                        .clients
                        .iter()
                        .any(|(other, c)| *other != id && irc::fold(&c.nick) == irc::fold(nick))
                {
                    self.numeric(id, 433, format!("{nick} :Nickname is already in use"));
                    return;
                }
                if self
                    .store
                    .rename_peer(&self.clients[&id].actor, nick)
                    .is_err()
                {
                    self.numeric(id, 400, "NICK :Storage failure");
                    return;
                }
                if self.clients[&id].registered {
                    let line = format!(":{} NICK :{nick}", self.clients[&id].prefix());
                    for other in self.peers(id) {
                        let line = if self.clients[&other].caps.contains("hearth-history") {
                            format!("@hearth/actor={} {line}", self.clients[&id].actor)
                        } else {
                            line.clone()
                        };
                        self.send(other, line);
                    }
                }
                self.clients.get_mut(&id).unwrap().nick = nick.clone();
                self.register(id);
                return;
            }
            "USER" => {
                if self.clients[&id].registered || !self.clients[&id].user.is_empty() {
                    self.numeric(id, 462, ":Already registered");
                    return;
                }
                if p.len() != 4
                    || p[0].is_empty()
                    || p[0].len() > 16
                    || !p[0]
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                    || !irc::text_ok(&p[3], 128)
                {
                    self.numeric(id, 461, "USER :Expected username (1-16 ASCII letters/digits/_/-), 0, *, and realname (up to 128 bytes)");
                    return;
                }
                let c = self.clients.get_mut(&id).unwrap();
                c.user = p[0].clone();
                c.realname = p[3].clone();
                self.register(id);
                return;
            }
            _ => {}
        }
        if !self.clients[&id].registered {
            self.numeric(id, 451, ":Finish NICK/USER and CAP END first");
            return;
        }
        match command.as_str() {
            "JOIN" if !p.is_empty() => {
                if p[0] == "0" {
                    let channels: Vec<_> = self.clients[&id].channels.iter().cloned().collect();
                    for channel in channels {
                        self.part(id, &channel);
                    }
                } else {
                    for channel in p[0].split(',') {
                        self.join(id, &irc::fold(channel));
                    }
                }
            }
            "PART" if !p.is_empty() => {
                for channel in p[0].split(',') {
                    self.part(id, &irc::fold(channel));
                }
            }
            "PRIVMSG" | "NOTICE" => self.message(id, &command, &p),
            "WHOIS" if !p.is_empty() => self.whois(id, p.last().unwrap()),
            "HEARTH" => self.hearth(id, &p),
            "CHATHISTORY" => self.chathistory(id, &p),
            "AWAY" => self.away(id, &p),
            "INVITE" => self.invite(id, &p),
            "NAMES" => {
                let channels = p
                    .first()
                    .map(|s| s.split(',').map(irc::fold).collect::<Vec<_>>())
                    .unwrap_or_else(|| self.channels.keys().cloned().collect());
                for channel in channels {
                    self.names(id, &channel);
                }
            }
            "LIST" => {
                self.numeric(id, 321, "Channel :Users Name");
                for (channel, state) in self.channels.clone() {
                    let count = self
                        .clients
                        .values()
                        .filter(|c| c.channels.contains(&channel))
                        .count();
                    self.numeric(id, 322, format!("{channel} {count} :{}", state.topic));
                }
                self.numeric(id, 323, ":End of LIST");
            }
            "WHO" => self.who(id, p.first().map(String::as_str).unwrap_or("*")),
            "MODE" if !p.is_empty() => {
                let target = irc::fold(&p[0]);
                if self.channels.contains_key(&target) {
                    if p.len() == 1 {
                        self.numeric(id, 324, format!("{target} +n"));
                    } else {
                        self.numeric(id, 482, format!("{target} :Channel modes are fixed"));
                    }
                } else if target == irc::fold(&self.clients[&id].nick) {
                    if p.len() == 1 {
                        self.numeric(id, 221, "+");
                    } else {
                        self.numeric(id, 501, ":User modes are not supported");
                    }
                } else {
                    self.numeric(id, 403, "* :No such channel");
                }
            }
            "TOPIC" if !p.is_empty() => self.topic(id, &irc::fold(&p[0]), p.get(1)),
            "MOTD" => self.numeric(id, 422, ":MOTD not configured"),
            "VERSION" => self.numeric(
                id,
                351,
                "hearth-ircd-0.1 hearth :Persistent IRC for agents and humans",
            ),
            "TIME" => self.numeric(
                id,
                391,
                format!("hearth :{}", chrono::Utc::now().to_rfc3339()),
            ),
            "LUSERS" => self.numeric(
                id,
                251,
                format!(
                    ":There are {} users on 1 server",
                    self.clients.values().filter(|c| c.registered).count()
                ),
            ),
            "JOIN" | "PART" | "WHOIS" | "MODE" | "TOPIC" => {
                self.numeric(id, 461, format!("{command} :Not enough parameters"))
            }
            _ => self.numeric(
                id,
                421,
                format!("{} :Unknown command", truncate(&command, 32)),
            ),
        }
    }

    fn join(&mut self, id: Id, channel: &str) {
        if !irc::valid_channel(channel) {
            self.numeric(id, 476, "* :Invalid channel name");
            return;
        }
        if self.clients[&id].channels.contains(channel) {
            return;
        }
        if self.clients[&id].channels.len() >= MAX_JOINED {
            self.numeric(
                id,
                405,
                format!("{channel} :Too many joined channels (limit {MAX_JOINED})"),
            );
            return;
        }
        if !self.channels.contains_key(channel) {
            if self.channels.len() >= MAX_CHANNELS {
                self.numeric(
                    id,
                    405,
                    format!("{channel} :Server channel limit reached ({MAX_CHANNELS})"),
                );
                return;
            }
            if let Err(error) = self.store.ensure_channel(channel) {
                log::error!("channel creation failed: {error}");
                self.numeric(id, 400, "JOIN :Channel storage failed; try again");
                return;
            }
            self.channels.insert(
                channel.into(),
                Channel {
                    name: channel.into(),
                    topic: String::new(),
                    topic_set_by: String::new(),
                    topic_set_at: 0,
                },
            );
        }
        if self
            .clients
            .get_mut(&id)
            .unwrap()
            .channels
            .insert(channel.to_owned())
        {
            self.broadcast(
                channel,
                &format!(":{} JOIN {channel}", self.clients[&id].prefix()),
                None,
            );
            let away = self.clients[&id].away.clone();
            if !away.is_empty() {
                let line = format!(":{} AWAY :{away}", self.clients[&id].prefix());
                let peers: Vec<_> = self
                    .clients
                    .iter()
                    .filter(|(other, c)| {
                        **other != id
                            && c.channels.contains(channel)
                            && c.caps.contains("away-notify")
                    })
                    .map(|(other, _)| *other)
                    .collect();
                for other in peers {
                    self.send(other, line.clone());
                }
            }
            self.topic_reply(id, channel);
            self.names(id, channel);
        }
    }

    fn topic_reply(&mut self, id: Id, channel: &str) {
        let state = self.channels[channel].clone();
        if state.topic.is_empty() {
            self.numeric(id, 331, format!("{channel} :No topic is set"));
        } else {
            self.numeric(id, 332, format!("{channel} :{}", state.topic));
            self.numeric(
                id,
                333,
                format!("{channel} {} {}", state.topic_set_by, state.topic_set_at),
            );
        }
    }

    fn topic(&mut self, id: Id, channel: &str, topic: Option<&String>) {
        if !self.channels.contains_key(channel) {
            self.numeric(id, 403, "* :No such channel");
            return;
        }
        let Some(topic) = topic else {
            self.topic_reply(id, channel);
            return;
        };
        if !self.clients[&id].channels.contains(channel) {
            self.numeric(id, 442, format!("{channel} :Not on that channel"));
            return;
        }
        if !irc::text_ok(topic, irc::MAX_MESSAGE) {
            self.numeric(id, 417, ":Topics must be plain UTF-8, at most 350 bytes");
            return;
        }
        let setter = self.clients[&id].prefix();
        match self.store.set_topic(channel, topic, &setter) {
            Ok(state) => {
                self.channels.insert(channel.into(), state);
                self.broadcast(
                    channel,
                    &format!(":{setter} TOPIC {channel} :{topic}"),
                    None,
                );
            }
            Err(error) => {
                log::error!("topic storage failed: {error}");
                self.numeric(id, 400, "TOPIC :Topic storage failed; try again");
            }
        }
    }

    fn part(&mut self, id: Id, channel: &str) {
        if !self.clients[&id].channels.contains(channel) {
            self.numeric(id, 442, "* :Not on that channel");
            return;
        }
        self.broadcast(
            channel,
            &format!(":{} PART {channel}", self.clients[&id].prefix()),
            None,
        );
        self.clients.get_mut(&id).unwrap().channels.remove(channel);
    }

    fn names(&mut self, id: Id, channel: &str) {
        if self.channels.contains_key(channel) {
            let names: Vec<_> = self
                .clients
                .values()
                .filter(|c| c.channels.contains(channel))
                .map(|c| c.nick.clone())
                .collect();
            for chunk in names.chunks(8) {
                self.numeric(id, 353, format!("= {channel} :{}", chunk.join(" ")));
            }
        }
        self.numeric(
            id,
            366,
            format!("{} :End of NAMES list", truncate(channel, 32)),
        );
    }

    fn message(&mut self, id: Id, command: &str, p: &[String]) {
        if p.len() < 2 || p[1].is_empty() {
            if command != "NOTICE" {
                self.numeric(id, 412, ":No text to send");
            }
            return;
        }
        if !irc::message_ok(&p[1]) {
            if command != "NOTICE" {
                self.numeric(id, 417, ":Messages must be plain UTF-8, at most 350 bytes");
            }
            return;
        }
        let target = irc::fold(&p[0]);
        if let Some(service) = service_name(&target) {
            // Services never reply to NOTICE, and never echo credentials supplied
            // in IDENTIFY/REGISTER requests or store service requests in history.
            if command == "PRIVMSG" {
                self.service(id, service, &p[1]);
            }
            return;
        }
        let c = &self.clients[&id];
        if target.starts_with('#') {
            if !c.channels.contains(&target) {
                if command != "NOTICE" {
                    self.numeric(id, 404, "* :Join the channel before sending");
                }
                return;
            }
            let mut record = Record {
                id: 0,
                channel: target.clone(),
                time: String::new(),
                nick: c.nick.clone(),
                user: c.user.clone(),
                peer: c.peer.clone(),
                body: p[1].clone(),
                command: command.into(),
                target: target.clone(),
                sender: c.actor.clone(),
            };
            if let Err(error) = self.store.append(&mut record, &c.host_label, &c.workdir) {
                log::error!("channel commit failed: {error}");
                self.numeric(
                    id,
                    400,
                    format!("PRIVMSG {target} :Storage failure; message was not accepted"),
                );
                return;
            }
            let recipients: Vec<_> = self
                .clients
                .iter()
                .filter(|(other, c)| {
                    c.channels.contains(&target)
                        && (**other != id || c.caps.contains("echo-message"))
                })
                .map(|(id, _)| *id)
                .collect();
            for recipient in recipients {
                self.deliver_record(recipient, &record, None, None);
            }
        } else {
            let online = self
                .clients
                .values()
                .find(|c| c.registered && irc::fold(&c.nick) == target)
                .map(|c| (c.actor.clone(), c.nick.clone()));
            let peer = online.or_else(|| {
                let peers = self.store.peers(&self.clients[&id].actor).ok()?;
                let matches: Vec<_> = peers
                    .into_iter()
                    .filter(|(_, nick)| irc::fold(nick) == target)
                    .collect();
                if matches.len() == 1 {
                    matches.into_iter().next()
                } else {
                    None
                }
            });
            if let Some((peer, nick)) = peer {
                self.direct(id, &peer, &nick, &p[1], command);
            } else if command != "NOTICE" {
                self.numeric(id,401,"* :Unknown or ambiguous recipient; offline delivery requires a known correspondent");
            }
        }
    }

    fn direct(&mut self, id: Id, peer_actor: &str, target: &str, body: &str, command: &str) {
        let c = &self.clients[&id];
        let actor = c.actor.clone();
        let targets: Vec<_> = self
            .clients
            .iter()
            .filter(|(_, c)| c.registered && c.actor == peer_actor)
            .map(|(id, c)| (*id, c.nick.clone(), c.away.clone()))
            .collect();
        let target_nick = targets
            .first()
            .map(|(_, nick, _)| nick.as_str())
            .unwrap_or(target)
            .to_owned();
        let mut record = Record {
            id: 0,
            channel: Store::dm_channel(&actor, peer_actor),
            time: String::new(),
            nick: c.nick.clone(),
            user: c.user.clone(),
            peer: c.peer.clone(),
            body: body.into(),
            command: command.into(),
            target: target_nick.clone(),
            sender: actor.clone(),
        };
        let host = c.host_label.clone();
        let workdir = c.workdir.clone();
        if self
            .store
            .append_dm(&mut record, &host, &workdir, &actor, peer_actor)
            .is_err()
        {
            self.numeric(
                id,
                400,
                "PRIVMSG :Storage failure; message was not accepted",
            );
            return;
        }
        for (recipient, nick, away) in &targets {
            record.target = nick.clone();
            self.deliver_record(*recipient, &record, None, Some((&actor, &record.nick)));
            if command == "PRIVMSG" && !away.is_empty() {
                self.numeric(id, 301, format!("{nick} :{away}"));
            }
        }
        if !targets.iter().any(|(recipient, _, _)| *recipient == id)
            && self.clients[&id].caps.contains("echo-message")
        {
            record.target = target_nick.clone();
            self.deliver_record(id, &record, None, Some((peer_actor, &target_nick)));
        }
    }

    fn deliver_record(
        &mut self,
        id: Id,
        record: &Record,
        batch: Option<&str>,
        dm_peer: Option<(&str, &str)>,
    ) {
        let c = &self.clients[&id];
        let mut tags = Vec::new();
        if c.caps.contains("server-time") {
            tags.push(format!("time={}", record.time));
        }
        if c.caps.contains("message-tags") || c.caps.contains("hearth-history") {
            tags.push(format!("msgid={}-{}", self.store.id, record.id));
        }
        if c.caps.contains("hearth-history") {
            tags.push(format!("hearth/id={}", record.id));
        }
        if let Some(batch) = batch {
            tags.push(format!("batch={batch}"));
        }
        let extension = c.caps.contains("hearth-history");
        if let Some((peer, nick)) = dm_peer {
            if extension {
                tags.push(format!("hearth/peer={peer}"));
                self.send(id, format!(":hearth HEARTH PEER {peer} {nick}"));
            }
        }
        let tags = if tags.is_empty() {
            String::new()
        } else {
            format!("@{} ", tags.join(";"))
        };
        self.send(
            id,
            format!(
                "{tags}:{}!{}@{} {} {} :{}",
                record.nick, record.user, record.peer, record.command, record.target, record.body
            ),
        );
    }

    fn away(&mut self, id: Id, p: &[String]) {
        let message = p.first().map(String::as_str).unwrap_or("");
        if p.len() > 1 || !irc::text_ok(message, 300) {
            self.numeric(id, 417, ":Away message must fit 300 bytes");
            return;
        }
        self.clients.get_mut(&id).unwrap().away = message.into();
        let suffix = if message.is_empty() {
            String::new()
        } else {
            format!(" :{message}")
        };
        let line = format!(":{} AWAY{suffix}", self.clients[&id].prefix());
        for peer in self.peers(id) {
            if peer != id && self.clients[&peer].caps.contains("away-notify") {
                self.send(peer, line.clone());
            }
        }
        self.numeric(
            id,
            if message.is_empty() { 305 } else { 306 },
            if message.is_empty() {
                ":You are no longer away"
            } else {
                ":You are now away"
            },
        );
    }

    fn invite(&mut self, id: Id, p: &[String]) {
        if p.len() != 2 {
            self.numeric(id, 461, "INVITE :Expected nickname and channel");
            return;
        }
        let channel = irc::fold(&p[1]);
        if !self.channels.contains_key(&channel) {
            self.numeric(id, 403, "* :No such channel");
            return;
        }
        if !self.clients[&id].channels.contains(&channel) {
            self.numeric(id, 442, format!("{channel} :Join before inviting"));
            return;
        }
        let Some((&other, c)) = self
            .clients
            .iter()
            .find(|(_, c)| c.registered && irc::fold(&c.nick) == irc::fold(&p[0]))
        else {
            self.numeric(id, 401, "* :No such nick");
            return;
        };
        let nick = c.nick.clone();
        let away = c.away.clone();
        if c.channels.contains(&channel) {
            self.numeric(id, 443, format!("{nick} {channel} :Already on channel"));
            return;
        }
        self.send(
            other,
            format!(":{} INVITE {nick} :{channel}", self.clients[&id].prefix()),
        );
        self.numeric(id, 341, format!("{nick} {channel}"));
        if !away.is_empty() {
            self.numeric(id, 301, format!("{nick} :{away}"));
        }
    }

    fn chathistory(&mut self, id: Id, p: &[String]) {
        let fail = |s: &mut Self, code: &str, message: &str| {
            s.send(id, format!(":hearth FAIL CHATHISTORY {code} :{message}"))
        };
        if !self.clients[&id].caps.contains("draft/chathistory") {
            fail(self, "INVALID_PARAMS", "Negotiate draft/chathistory first");
            return;
        }
        if p.first().is_some_and(|s| s.eq_ignore_ascii_case("TARGETS")) {
            self.history_targets(id, p);
            return;
        }
        let Some(target) = p.get(1) else {
            fail(self, "INVALID_PARAMS", "Missing target");
            return;
        };
        let target = irc::fold(target);
        let c = &self.clients[&id];
        let channel = if target.starts_with('#') {
            if !c.channels.contains(&target) {
                fail(
                    self,
                    "INVALID_TARGET",
                    "Join before requesting channel history",
                );
                return;
            }
            target.clone()
        } else {
            let online = self
                .clients
                .values()
                .find(|other| other.registered && irc::fold(&other.nick) == target)
                .map(|other| other.actor.as_str());
            let peer = self.store.peers(&c.actor).ok().and_then(|peers| {
                let matches: Vec<_> = peers
                    .into_iter()
                    .filter(|(actor, nick)| {
                        online
                            .map(|current| current == actor)
                            .unwrap_or_else(|| irc::fold(nick) == target)
                    })
                    .collect();
                if matches.len() == 1 {
                    matches.into_iter().next()
                } else {
                    None
                }
            });
            let Some((peer, _)) = peer else {
                fail(self, "INVALID_TARGET", "No conversation with this nickname");
                return;
            };
            Store::dm_channel(&c.actor, &peer)
        };
        match self.store.chathistory(&channel, p) {
            Ok(mut records) => {
                self.batch_id += 1;
                let batch = format!("h{}", self.batch_id);
                let batched = self.clients[&id].caps.contains("batch");
                if batched {
                    self.send(id, format!(":hearth BATCH +{batch} chathistory {target}"));
                }
                for record in &mut records {
                    if !target.starts_with('#') {
                        if record.sender == self.clients[&id].actor {
                            record.nick = self.clients[&id].nick.clone();
                            record.target = target.clone();
                        } else {
                            record.nick = target.clone();
                            record.target = self.clients[&id].nick.clone();
                        }
                    }
                    self.deliver_record(id, record, batched.then_some(batch.as_str()), None);
                }
                if batched {
                    self.send(id, format!(":hearth BATCH -{batch}"));
                }
            }
            Err(message) => fail(
                self,
                if message == "History storage failure" {
                    "MESSAGE_ERROR"
                } else {
                    "INVALID_PARAMS"
                },
                &message,
            ),
        }
    }

    fn history_targets(&mut self, id: Id, p: &[String]) {
        let valid = (|| {
            if p.len() != 4 {
                return None;
            }
            let a = crate::history::timestamp(&p[1]).ok()?;
            let b = crate::history::timestamp(&p[2]).ok()?;
            let limit = p[3]
                .parse::<usize>()
                .ok()
                .filter(|n| (1..=100).contains(n))?;
            Some((a, b, limit))
        })();
        let Some((a, b, limit)) = valid else {
            self.send(id,":hearth FAIL CHATHISTORY INVALID_PARAMS :TARGETS needs two timestamps and limit 1..100".into());
            return;
        };
        let c = &self.clients[&id];
        let result = (|| -> rusqlite::Result<Vec<(String, String)>> {
            let mut targets = Vec::new();
            for channel in &c.channels {
                if let Some(time) = self.store.last_time(channel)? {
                    targets.push((channel.clone(), time));
                }
            }
            for (peer, nick) in self.store.peers(&c.actor)? {
                if let Some(time) = self.store.last_time(&Store::dm_channel(&c.actor, &peer))? {
                    targets.push((nick, time));
                }
            }
            Ok(targets)
        })();
        let Ok(mut targets) = result else {
            self.send(
                id,
                ":hearth FAIL CHATHISTORY MESSAGE_ERROR :History storage failure".into(),
            );
            return;
        };
        let (low, high) = if a <= b { (&a, &b) } else { (&b, &a) };
        targets.retain(|(_, t)| t > low && t < high);
        targets.sort_by(|x, y| {
            if a <= b {
                x.1.cmp(&y.1).then(x.0.cmp(&y.0))
            } else {
                y.1.cmp(&x.1).then(x.0.cmp(&y.0))
            }
        });
        self.batch_id += 1;
        let batch = format!("h{}", self.batch_id);
        let batched = c.caps.contains("batch");
        if batched {
            self.send(
                id,
                format!(":hearth BATCH +{batch} draft/chathistory-targets"),
            );
        }
        for (target, time) in targets.into_iter().take(limit) {
            self.send(
                id,
                format!(
                    "{}:hearth CHATHISTORY TARGETS {target} {time}",
                    if batched {
                        format!("@batch={batch} ")
                    } else {
                        String::new()
                    }
                ),
            );
        }
        if batched {
            self.send(id, format!(":hearth BATCH -{batch}"));
        }
    }

    fn whois(&mut self, id: Id, target: &str) {
        if let Some(service) = service_name(target) {
            self.numeric(
                id,
                311,
                format!("{service} service hearth * :Built-in account-free IRC commands"),
            );
            self.numeric(id, 312, format!("{service} hearth :hearth IRC server"));
            self.numeric(id, 318, format!("{service} :End of WHOIS"));
            return;
        }
        let target = irc::fold(target);
        if let Some(c) = self
            .clients
            .values()
            .find(|c| c.registered && irc::fold(&c.nick) == target)
        {
            let mut replies = vec![
                (
                    311,
                    format!("{} {} {} * :{}", c.nick, c.user, c.peer, c.realname),
                ),
                (312, format!("{} hearth :hearth IRC server", c.nick)),
                (320, format!("{} :Observed peer: {}", c.nick, c.peer)),
            ];
            if !c.away.is_empty() {
                replies.push((301, format!("{} :{}", c.nick, c.away)));
            }
            if !c.host_label.is_empty() {
                replies.push((
                    320,
                    format!(
                        "{} :Reported host: {} (self-reported)",
                        c.nick, c.host_label
                    ),
                ));
            }
            if !c.workdir.is_empty() {
                replies.push((
                    320,
                    format!(
                        "{} :Working directory (self-reported): {}",
                        c.nick, c.workdir
                    ),
                ));
            }
            for channel in &c.channels {
                replies.push((319, format!("{} :{channel}", c.nick)));
            }
            replies.push((
                317,
                format!(
                    "{} {} {} :seconds idle, signon time",
                    c.nick,
                    c.active.elapsed().as_secs(),
                    c.signon
                ),
            ));
            for (code, reply) in replies {
                self.numeric(id, code, reply);
            }
        } else {
            self.numeric(id, 401, "* :No such nick");
        }
        self.numeric(id, 318, format!("{} :End of WHOIS", truncate(&target, 32)));
    }

    fn service_notice(&mut self, id: Id, service: &str, text: &str) {
        let nick = self.clients[&id].nick.clone();
        self.send(
            id,
            format!(":{service}!service@hearth NOTICE {nick} :{text}"),
        );
    }

    fn service(&mut self, id: Id, service: &str, body: &str) {
        let Ok(request) = irc::parse(body) else {
            self.service_notice(
                id,
                service,
                "Invalid command. Send HELP for available commands.",
            );
            return;
        };
        let p = &request.params;
        if request.prefix.is_some() || !request.tags.is_empty() {
            self.service_notice(
                id,
                service,
                "Invalid command. Send HELP for available commands.",
            );
            return;
        }
        if service == "NickServ" {
            match request.command.as_str() {
                "HELP" => {
                    self.service_notice(id, service, "Commands: NICK <newnick>, INFO [nick], STATUS [nick], HELP. Standard /nick also works.");
                    self.service_notice(id, service, "No accounts or nickname ownership. Server PASS controls admission; nicknames last for a connection. REGISTER and IDENTIFY are unnecessary.");
                }
                "NICK" if p.len() == 1 => self.command(id, &format!("NICK {}", p[0])),
                "INFO" if p.len() <= 1 => {
                    let target = p.first().cloned().unwrap_or_else(|| self.clients[&id].nick.clone());
                    self.whois(id, &target);
                    self.service_notice(id, service, "Nicknames are connection identities, not registered accounts.");
                }
                "STATUS" if p.len() <= 1 => {
                    let target = p.first().cloned().unwrap_or_else(|| self.clients[&id].nick.clone());
                    self.service_notice(id, service, &format!("STATUS {} 0", truncate(&target, 32)));
                    self.service_notice(id, service, "No account authentication or nickname ownership is provided; admission uses server PASS.");
                }
                "REGISTER" | "IDENTIFY" | "LOGIN" | "LOGOUT" => self.service_notice(id, service, "No accounts are required or created. Connect using the server password and choose a nickname with /nick. Do not send account passwords here."),
                "GHOST" | "RECOVER" | "RELEASE" => self.service_notice(id, service, "Nickname takeover is not supported. Choose another nickname or wait for the existing connection to close."),
                _ => self.service_notice(id, service, "Unknown command or invalid arguments. Send HELP for available commands."),
            }
        } else {
            match request.command.as_str() {
                "HELP" => {
                    self.service_notice(id, service, "Commands: JOIN <#channel>, REGISTER <#channel>, INFO <#channel>, TOPIC <#channel> [text], CLEAR <#channel> TOPIC, SET <#channel> TOPIC <text>.");
                    self.service_notice(id, service, "JOIN/REGISTER create and join a persistent shared channel. No owners or operators; members may set topics. REGISTER does not claim ownership.");
                }
                "JOIN" | "REGISTER" if !p.is_empty() => {
                    let channel = irc::fold(&p[0]);
                    self.join(id, &channel);
                    if self.clients[&id].channels.contains(&channel) {
                        self.service_notice(id, service, &format!("Joined {channel}. This is a persistent shared channel; no ownership or channel password was registered."));
                    }
                }
                "INFO" if p.len() == 1 => self.topic(id, &irc::fold(&p[0]), None),
                "TOPIC" if !p.is_empty() => {
                    let topic = (p.len() > 1).then(|| p[1..].join(" "));
                    self.topic(id, &irc::fold(&p[0]), topic.as_ref());
                }
                "SET" if p.len() >= 3 && p[1].eq_ignore_ascii_case("TOPIC") => {
                    self.topic(id, &irc::fold(&p[0]), Some(&p[2..].join(" ")));
                }
                "CLEAR" if p.len() == 2 && p[1].eq_ignore_ascii_case("TOPIC") => {
                    self.topic(id, &irc::fold(&p[0]), Some(&String::new()));
                }
                _ => self.service_notice(id, service, "Unknown command or invalid arguments. No channel ownership, operator grants, or takeover commands are supported. Send HELP."),
            }
        }
    }

    fn who(&mut self, id: Id, target: &str) {
        let target = irc::fold(target);
        let replies: Vec<_> = self
            .clients
            .values()
            .filter(|c| {
                c.registered
                    && (target == "*"
                        || target == "0"
                        || c.channels.contains(&target)
                        || irc::fold(&c.nick) == target)
            })
            .map(|c| {
                let channel = c.channels.iter().next().map(String::as_str).unwrap_or("*");
                format!(
                    "{channel} {} {} hearth {} {} :0 {}",
                    c.user,
                    c.peer,
                    c.nick,
                    if c.away.is_empty() { "H" } else { "G" },
                    c.realname
                )
            })
            .collect();
        for reply in replies {
            self.numeric(id, 352, reply);
        }
        self.numeric(id, 315, format!("{} :End of WHO", truncate(&target, 32)));
    }

    fn hearth(&mut self, id: Id, p: &[String]) {
        match p.first().map(|p| p.to_ascii_uppercase()).as_deref() {
            Some("DMSG") if p.len() == 3 => {
                if !irc::message_ok(&p[2]) {
                    self.numeric(id, 417, ":Invalid message");
                    return;
                }
                let peer = self
                    .store
                    .peers(&self.clients[&id].actor)
                    .ok()
                    .and_then(|peers| peers.into_iter().find(|(key, _)| key == &p[1]));
                if let Some((peer, nick)) = peer {
                    self.direct(id, &peer, &nick, &p[2], "PRIVMSG");
                } else {
                    self.numeric(id, 401, "* :Unknown correspondent");
                }
            }
            Some("PEERS") if p.len() == 3 => {
                let limit = p[2].parse::<usize>().ok().filter(|n| (1..=100).contains(n));
                let Some(limit) = limit else {
                    self.numeric(id, 461, "HEARTH :PEERS cursor limit");
                    return;
                };
                let Ok(peers) = self.store.peers(&self.clients[&id].actor) else {
                    self.numeric(id, 400, "HEARTH :Storage failure");
                    return;
                };
                let peers: Vec<_> = peers
                    .into_iter()
                    .filter(|(key, _)| p[1] == "*" || key > &p[1])
                    .collect();
                let mut cursor = p[1].clone();
                for (key, nick) in peers.iter().take(limit) {
                    self.send(id, format!(":hearth HEARTH PEER {key} {nick}"));
                    cursor = key.clone();
                }
                self.send(
                    id,
                    format!(
                        ":hearth HEARTH PEERSEND {cursor} {}",
                        usize::from(peers.len() > limit)
                    ),
                );
            }
            Some("DMS") if p.len() == 4 => {
                let after = p[2].parse::<i64>().ok().filter(|n| *n >= 0);
                let limit = p[3].parse::<usize>().ok().filter(|n| (1..=100).contains(n));
                let (Some(after), Some(limit)) = (after, limit) else {
                    self.numeric(id, 461, "HEARTH :DMS peer after limit");
                    return;
                };
                let known = self
                    .store
                    .peers(&self.clients[&id].actor)
                    .is_ok_and(|peers| peers.iter().any(|(key, _)| key == &p[1]));
                if !known {
                    self.numeric(id, 401, "* :Unknown conversation");
                    return;
                }
                let channel = Store::dm_channel(&self.clients[&id].actor, &p[1]);
                match self.store.history(&channel, after, limit + 1) {
                    Ok(records) => {
                        let mut cursor = after;
                        for r in records.iter().take(limit) {
                            self.send(
                                id,
                                format!(
                                    ":hearth HEARTH DMEVENT {} {} {} {} {} :{}",
                                    p[1], r.id, r.time, r.nick, r.command, r.body
                                ),
                            );
                            cursor = r.id;
                        }
                        self.send(
                            id,
                            format!(
                                ":hearth HEARTH DMEND {} {cursor} {}",
                                p[1],
                                usize::from(records.len() > limit)
                            ),
                        );
                    }
                    Err(_) => self.numeric(id, 400, "HEARTH :History storage failure"),
                }
            }
            Some("LOCATION") if p.len() == 3 => {
                if p[1].is_empty()
                    || p[1].len() > 64
                    || !p[1]
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                    || !p[2].starts_with('/')
                    || !irc::text_ok(&p[2], 300)
                {
                    self.numeric(id, 461, "HEARTH :LOCATION needs a host label (up to 64 ASCII letters/digits/./_/-) and absolute directory (up to 300 UTF-8 bytes)");
                    return;
                }
                let c = self.clients.get_mut(&id).unwrap();
                c.host_label = p[1].clone();
                c.workdir = p[2].clone();
                self.send(
                    id,
                    ":hearth HEARTH LOCATION :Updated self-reported location".into(),
                );
            }
            Some("HISTORY") if p.len() == 4 => {
                let channel = irc::fold(&p[1]);
                if !self.clients[&id].channels.contains(&channel) {
                    self.numeric(id, 442, "* :Join before requesting history");
                    return;
                }
                let after = p[2].parse::<i64>().ok().filter(|v| *v >= 0);
                let limit = p[3].parse::<usize>().ok().filter(|v| (1..=100).contains(v));
                let (Some(after), Some(limit)) = (after, limit) else {
                    self.numeric(
                        id,
                        461,
                        "HEARTH :HISTORY needs channel, nonnegative cursor, and limit 1..100",
                    );
                    return;
                };
                let records = self.store.latest().and_then(|latest| {
                    if after > latest {
                        return Err(rusqlite::Error::InvalidQuery);
                    }
                    self.store.history(&channel, after, limit + 1)
                });
                match records {
                    Ok(records) => {
                        let more = usize::from(records.len() > limit);
                        let mut cursor = after;
                        self.send(
                            id,
                            format!(
                                ":hearth HEARTH BEGIN {channel} {} {after}",
                                self.store.history_id
                            ),
                        );
                        for record in records.iter().take(limit) {
                            self.send(
                                id,
                                format!(
                                    ":hearth HEARTH EVENT {channel} {} {} {} :{}",
                                    record.id, record.time, record.nick, record.body
                                ),
                            );
                            cursor = record.id;
                        }
                        self.send(id, format!(":hearth HEARTH END {channel} {cursor} {more}"));
                    }
                    Err(error) => {
                        log::error!("history query failed: {error}");
                        self.numeric(id, 400, "HEARTH :Invalid cursor or history unavailable");
                    }
                }
            }
            _ => self.numeric(
                id,
                461,
                "HEARTH :Use LOCATION host :/absolute/path or HISTORY #channel after-id limit",
            ),
        }
    }

    fn tick(&mut self) {
        let ids: Vec<_> = self.clients.keys().copied().collect();
        for id in ids {
            let c = &self.clients[&id];
            if (!c.registered && c.connected.elapsed() > Duration::from_secs(30))
                || c.ping
                    .is_some_and(|p| p.elapsed() > Duration::from_secs(60))
            {
                self.send(id, "ERROR :Connection timed out".into());
                self.disconnect(id, "Connection timed out");
            } else if c.ping.is_none() && c.active.elapsed() > Duration::from_secs(60) {
                self.clients.get_mut(&id).unwrap().ping = Some(Instant::now());
                self.send(id, "PING :hearth".into());
            }
        }
    }

    fn run(mut self, mut rx: mpsc::Receiver<Input>) {
        while let Some(input) = rx.blocking_recv() {
            match input {
                Input::Connect(id, addr, tx) => {
                    self.clients.insert(
                        id,
                        Client::new(addr.ip().to_string(), tx, self.settings.password.is_none()),
                    );
                }
                Input::Line(id, line) => self.command(id, &line),
                Input::Disconnect(id) => self.disconnect(id, "Connection closed"),
                Input::Tick => self.tick(),
                Input::Stop => break,
            }
            self.reap();
        }
    }
}

fn truncate(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

async fn connection(stream: BoxStream, id: Id, addr: SocketAddr, events: mpsc::Sender<Input>) {
    let (tx, mut rx) = mpsc::channel::<String>(SEND_QUEUE);
    if events.send(Input::Connect(id, addr, tx)).await.is_err() {
        return;
    }
    let (read, mut write) = tokio::io::split(stream);
    let read_loop = async {
        let mut reader = BufReader::new(read);
        while let Some(line) = irc::read_frame(&mut reader, irc::MAX_FRAME).await? {
            if events.send(Input::Line(id, line)).await.is_err() {
                break;
            }
        }
        Ok::<(), std::io::Error>(())
    };
    let write_loop = async {
        while let Some(line) = rx.recv().await {
            tokio::time::timeout(Duration::from_secs(10), write.write_all(line.as_bytes()))
                .await??;
        }
        write.shutdown().await
    };
    tokio::select! { _ = read_loop => {}, _ = write_loop => {} }
    let _ = events.send(Input::Disconnect(id)).await;
}

pub async fn serve(
    listener: TcpListener,
    store: Store,
    settings: Settings,
    tls: Option<TlsAcceptor>,
) -> std::io::Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let permits = Arc::new(Semaphore::new(settings.max_clients));
    let (tx, rx) = mpsc::channel(1024);
    let server = Server::new(store, settings).map_err(std::io::Error::other)?;
    let actor = tokio::task::spawn_blocking(move || server.run(rx));
    let mut next_id = 1;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut connections = tokio::task::JoinSet::new();
    let result = loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break Ok(()),
            _ = terminate.recv() => break Ok(()),
            _ = tick.tick() => { if tx.send(Input::Tick).await.is_err() { break Err(std::io::Error::other("server state task stopped")); } },
            Some(_) = connections.join_next() => {},
            accepted = listener.accept() => {
                let (stream, addr) = match accepted { Ok(v) => v, Err(e) => break Err(e) };
                let Ok(permit) = permits.clone().try_acquire_owned() else { drop(stream); continue; };
                if let Err(error) = stream.set_nodelay(true) { break Err(error); }
                let id = next_id; next_id += 1;
                let events = tx.clone(); let tls = tls.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    let stream: BoxStream = if let Some(tls) = tls {
                        match tokio::time::timeout(Duration::from_secs(10), tls.accept(stream)).await {
                            Ok(Ok(stream)) => Box::new(stream), _ => return,
                        }
                    } else { Box::new(stream) };
                    connection(stream, id, addr, events).await;
                });
            }
        }
    };
    let _ = tx.send(Input::Stop).await;
    actor.await.map_err(std::io::Error::other)?;
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        Server::new(
            Store::open(std::path::Path::new(":memory:")).unwrap(),
            Settings {
                password: None,
                channels: BTreeSet::from(["#general".into()]),
                max_clients: 3,
            },
        )
        .unwrap()
    }

    fn connect(server: &mut Server, id: Id, nick: &str) -> mpsc::Receiver<String> {
        let (tx, mut rx) = mpsc::channel(32);
        server
            .clients
            .insert(id, Client::new("127.0.0.1".into(), tx, true));
        server.command(id, &format!("NICK {nick}"));
        server.command(id, "USER agent 0 * :test");
        server.command(id, "JOIN #general");
        while rx.try_recv().is_ok() {}
        rx
    }

    #[test]
    fn channel_limits_reject_without_creating_or_joining() {
        let mut s = server();
        let mut receiver = connect(&mut s, 1, "creator");
        for i in 1..MAX_JOINED {
            s.join(1, &format!("#room{i}"));
            while receiver.try_recv().is_ok() {}
        }
        s.join(1, "#over-limit");
        assert!(receiver.try_recv().unwrap().contains(" 405 "));
        assert!(!s.channels.contains_key("#over-limit"));
        assert_eq!(s.store.channels().unwrap().len(), MAX_JOINED);
        s.part(1, "#general");
        while receiver.try_recv().is_ok() {}
        for i in MAX_JOINED..MAX_CHANNELS {
            let channel = format!("#room{i}");
            s.join(1, &channel);
            s.part(1, &channel);
            while receiver.try_recv().is_ok() {}
        }
        s.join(1, "#over-limit");
        assert!(receiver.try_recv().unwrap().contains(" 405 "));
        assert!(!s.clients[&1].channels.contains("#over-limit"));
        assert_eq!(s.store.channels().unwrap().len(), MAX_CHANNELS);
        // Existing channels remain joinable at the global creation limit.
        s.join(1, "#general");
        assert!(s.clients[&1].channels.contains("#general"));
        assert!(s.failed.is_empty());
    }

    #[test]
    fn slow_receiver_is_disconnected_without_losing_committed_history_for_others() {
        let mut s = server();
        let mut sender = connect(&mut s, 1, "sender");
        let _slow = connect(&mut s, 2, "slow");
        let mut fast = connect(&mut s, 3, "fast");
        let mut received = Vec::new();
        for i in 0..40 {
            s.command(1, &format!("PRIVMSG #general :message {i}"));
            s.reap();
            while let Ok(line) = fast.try_recv() {
                if line.contains(" PRIVMSG ") {
                    received.push(line);
                }
            }
            while sender.try_recv().is_ok() {}
        }
        assert!(!s.clients.contains_key(&2));
        assert!(s.clients.contains_key(&1));
        assert!(s.clients.contains_key(&3));
        assert_eq!(received.len(), 40);
        assert_eq!(s.store.history("#general", 0, 100).unwrap().len(), 40);
        for (i, line) in received.iter().enumerate() {
            assert!(line.ends_with(&format!(":message {i}\r\n")));
        }
    }

    #[test]
    fn unfinished_registration_and_command_floods_release_connections() {
        let mut s = server();
        let (tx, _rx) = mpsc::channel(32);
        let mut client = Client::new("127.0.0.1".into(), tx, false);
        client.connected = Instant::now() - Duration::from_secs(31);
        s.clients.insert(1, client);
        s.tick();
        assert!(!s.clients.contains_key(&1));
        let mut receiver = connect(&mut s, 2, "flood");
        let mut limited = false;
        for _ in 0..110 {
            s.command(2, "PING :flood");
            while let Ok(line) = receiver.try_recv() {
                limited |= line.contains("Command rate exceeded");
            }
        }
        assert!(limited);
        assert!(!s.clients.contains_key(&2));
    }
}
