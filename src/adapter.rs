use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hearth_irc::{irc, transport::BoxStream};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_rustls::{TlsConnector, rustls::pki_types::ServerName};

use crate::core::{Model, Outbound};

pub struct Settings {
    pub server: String,
    pub port: u16,
    pub password: Option<String>,
    pub identity: String,
    pub tls: Option<TlsConnector>,
}

pub async fn run(settings: Settings, model: Arc<Mutex<Model>>) {
    loop {
        let error = session(&settings, &model)
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "connection closed".into());
        log::warn!("IRC connection stopped: {error}");
        model.lock().unwrap().network_failed(error);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
fn failure(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}
async fn send<W: tokio::io::AsyncWrite + Unpin>(writer: &mut W, line: &str) -> io::Result<()> {
    if line.len() > 510 || line.contains(['\r', '\n', '\0']) {
        return Err(failure("invalid outgoing frame"));
    }
    tokio::time::timeout(
        Duration::from_secs(10),
        writer.write_all(format!("{line}\r\n").as_bytes()),
    )
    .await?
}

struct Pending {
    request: Outbound,
    token: String,
    sent: Instant,
    failed: bool,
    echoed: bool,
}

async fn session(settings: &Settings, model: &Arc<Mutex<Model>>) -> io::Result<()> {
    let tcp = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::TcpStream::connect((settings.server.as_str(), settings.port)),
    )
    .await??;
    tcp.set_nodelay(true)?;
    let stream: BoxStream = if let Some(tls) = &settings.tls {
        let name = ServerName::try_from(settings.server.clone())
            .map_err(|_| failure("invalid TLS server name"))?;
        Box::new(tokio::time::timeout(Duration::from_secs(10), tls.connect(name, tcp)).await??)
    } else {
        Box::new(tcp)
    };
    let (read, mut write) = tokio::io::split(stream);
    send(
        &mut write,
        &format!(
            "PASS :{}/{}",
            settings.password.as_deref().unwrap_or(""),
            settings.identity
        ),
    )
    .await?;
    send(&mut write, "CAP LS 302").await?;
    send(&mut write, &format!("CAP REQ :{}", irc::CAPABILITIES)).await?;
    let nick = model.lock().unwrap().nick.clone();
    send(&mut write, &format!("NICK {nick}")).await?;
    send(&mut write, "USER agent 0 * :hearth filesystem agent").await?;
    send(&mut write, "CAP END").await?;
    // A dedicated reader preserves partially received frames across timer wakeups.
    let (tx, mut rx) = mpsc::channel(128);
    let reader = tokio::spawn(async move {
        let mut read = BufReader::new(read);
        loop {
            let result = match irc::read_frame(&mut read, 8192).await {
                Ok(Some(line)) => Ok(line),
                Ok(None) => Err(failure("server disconnected")),
                Err(error) => Err(error),
            };
            let failed = result.is_err();
            if tx.send(result).await.is_err() || failed {
                return;
            }
        }
    });
    let result = async {
        // At most 20 requests/s, including their PING barriers (40 commands/s),
        // below the IRCd's sustained rate. Internal joins/replay share this budget.
        let mut ticks = tokio::time::interval(Duration::from_millis(50));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_received = Instant::now();
        let mut last_progress = Instant::now();
        let mut pending: Option<Pending> = None;
        let mut sequence = 0u64;
        let mut caps_acked = false;
        let mut registered = false;
        let mut internal = VecDeque::<String>::new();
        let mut syncing = BTreeSet::<String>::new();
        let mut dm_syncing = BTreeSet::<String>::new();
        let mut peers_done = false;
        let mut names = BTreeMap::<String,BTreeMap<String,String>>::new();
        let mut rooms = BTreeMap::<String,(usize,String)>::new();
        loop {
            tokio::select! {
                line = rx.recv() => {
                    let line = line.ok_or_else(|| failure("server reader stopped"))??;
                    last_received = Instant::now();
                    let m = irc::parse(&line).map_err(failure)?;
                    let p = &m.params;
                    if m.command == "PING" {
                        let token = p.first().ok_or_else(|| failure("invalid PING"))?;
                        send(&mut write,&format!("PONG :{token}")).await?;
                        continue;
                    }
                    if m.command == "PONG" && m.prefix.as_deref() == Some("hearth") {
                        if pending.as_ref().is_some_and(|r| p.last() == Some(&r.token)) {
                            let completed = pending.take().unwrap();
                            if !completed.failed && completed.request.message.is_some() && !completed.echoed {
                                return Err(failure("message acknowledgment missing; delivery uncertain"));
                            }
                            let mut state = model.lock().unwrap();
                            if !completed.failed {
                                // Location changes survive reconnect only after server confirmation.
                                let request = irc::parse(&completed.request.wire).map_err(failure)?;
                                if request.command == "HEARTH" && request.params.len() == 3 {
                                    state.location = Some((request.params[1].clone(),request.params[2].clone()));
                                }
                                if request.command=="AWAY" {state.away=request.params.first().cloned().unwrap_or_default();}
                                let r = state.remote.as_mut().unwrap();
                                if r.inflight.take().is_some() {r.acknowledged += 1;}
                            }
                        }
                        continue;
                    }
                    if m.command != "HEARTH" && !(m.command == "PRIVMSG" && p.first().is_some_and(|s| s.starts_with('#'))) {
                        model.lock().unwrap().event(&line);
                    }
                    if m.command == "CAP" && p.get(1).map(String::as_str) == Some("ACK") {
                        let caps = p.get(2).ok_or_else(|| failure("invalid CAP ACK"))?;
                        caps_acked = irc::CAPABILITIES.split(' ').all(|cap| caps.split(' ').any(|c| c == cap));
                    } else if m.command == "CAP" && p.get(1).map(String::as_str) == Some("NAK") {
                        return Err(failure("server lacks required hearth capabilities"));
                    } else if m.command == "001" {
                        if !caps_acked {return Err(failure("server did not negotiate required capabilities"));}
                        registered = true;
                        last_progress = Instant::now();
                        let state = model.lock().unwrap();
                        if let Some((host,path)) = &state.location {internal.push_back(format!("HEARTH LOCATION {host} :{path}"));}
                        internal.push_back("LIST".into());
                        internal.push_back("HEARTH PEERS * 100".into());
                        if !state.away.is_empty() {internal.push_back(format!("AWAY :{}",state.away));}
                        for c in state.channels.iter().filter(|c| c.desired) {internal.push_back(format!("JOIN {}",c.name));}
                    } else if m.command == "005" {
                        if let Some(server_id) = p.iter().find_map(|part| part.strip_prefix("SERVERID=")) {
                            let mut state = model.lock().unwrap();
                            let r = state.remote.as_mut().unwrap();
                            if r.server_id.as_deref().is_some_and(|previous| previous != server_id) {
                                return Err(failure("server database identity changed; remount to start a new history"));
                            }
                            r.server_id = Some(server_id.into());
                        }
                    } else if m.command == "353" && p.len() == 4 {
                        let members = names.entry(irc::fold(&p[2])).or_default();
                        for nick in p[3].split_whitespace() {members.insert(irc::fold(nick),nick.into());}
                    } else if m.command == "366" && p.len() >= 2 {
                        let channel = irc::fold(&p[1]);
                        let mut state = model.lock().unwrap();
                        if let Some(i) = state.index(&channel) {
                            let c = &mut state.channels[i];
                            c.members = names.remove(&channel).unwrap_or_default();
                            if c.joined && !c.ready && syncing.insert(channel.clone()) {
                                last_progress = Instant::now();
                                internal.push_back(format!("HEARTH HISTORY {channel} {} 100",c.cursor));
                            }
                        } else {names.remove(&channel);}
                    } else if m.command == "321" {rooms.clear();
                    } else if m.command == "322" && p.len() == 4 {
                        if irc::valid_channel(&p[1]) && rooms.len() < crate::core::MAX_CHANNELS {
                            rooms.insert(irc::fold(&p[1]),(p[2].parse().unwrap_or(0),p[3].clone()));
                        }
                    } else if m.command == "323" {
                        model.lock().unwrap().rooms = std::mem::take(&mut rooms);
                    } else if m.command == "HEARTH" && m.prefix.as_deref() == Some("hearth") {
                        match p.first().map(String::as_str) {
                            Some("BEGIN") if p.len() == 4 && syncing.contains(&p[1]) => {
                                let mut state = model.lock().unwrap();
                                let r = state.remote.as_mut().unwrap();
                                if r.server_id.as_ref().is_some_and(|id| id != &p[2]) {return Err(failure("server database identity changed; remount to start a new history"));}
                                r.server_id = Some(p[2].clone());
                            }
                            Some("EVENT") if p.len() == 6 && syncing.contains(&p[1]) => {
                                last_progress = Instant::now();
                                let id = p[2].parse::<i64>().map_err(|_| failure("invalid history message ID"))?;
                                validate_event(id,&p[4],&p[5])?;
                                model.lock().unwrap().receive(&p[1],id,&p[3],&p[4],&p[5]).map_err(|_| failure("local SQLite spool write failed or invalid event"))?;
                            }
                            Some("END") if p.len() == 4 && syncing.contains(&p[1]) => {
                                last_progress = Instant::now();
                                let mut state = model.lock().unwrap();
                                let i = state.index(&p[1]).ok_or_else(|| failure("unknown replay channel"))?;
                                let c = &mut state.channels[i];
                                if p[2].parse::<i64>().ok() != Some(c.cursor) {return Err(failure("history cursor mismatch"));}
                                if p[3] == "1" {internal.push_back(format!("HEARTH HISTORY {} {} 100",c.name,c.cursor));}
                                else if p[3] == "0" {c.ready = true; syncing.remove(&p[1]);}
                                else {return Err(failure("invalid history pagination"));}
                            }
                            Some("PEER") if p.len()==3 => {
                                let mut state=model.lock().unwrap();
                                let i=state.peer(&p[1],&p[2]).map_err(|_|failure("invalid DM peer"))?;
                                let c=&state.conversations[i];
                                if !c.ready && dm_syncing.insert(c.peer.clone()) {
                                    last_progress=Instant::now();
                                    internal.push_back(format!("HEARTH DMS {} {} 100",c.peer,c.cursor));
                                }
                            }
                            Some("PEERSEND") if p.len()==3 => {
                                if p[2]=="1" {internal.push_back(format!("HEARTH PEERS {} 100",p[1]));}
                                else if p[2]=="0" {peers_done=true;}
                                else {return Err(failure("invalid peer pagination"));}
                            }
                            Some("DMEVENT") if p.len()==7 && dm_syncing.contains(&p[1]) => {
                                last_progress=Instant::now();
                                let id=p[2].parse::<i64>().map_err(|_|failure("invalid DM event ID"))?;
                                validate_event(id,&p[4],&p[6])?;
                                model.lock().unwrap().receive_dm(&p[1],id,&p[3],&p[4],&p[6]).map_err(|_|failure("DM SQLite spool write failed"))?;
                            }
                            Some("DMEND") if p.len()==4 && dm_syncing.contains(&p[1]) => {
                                last_progress=Instant::now();
                                let mut state=model.lock().unwrap();
                                let c=state.conversations.iter_mut().find(|c| c.peer==p[1]).ok_or_else(||failure("unknown DM peer"))?;
                                if p[2].parse::<i64>().ok()!=Some(c.cursor) {return Err(failure("DM cursor mismatch"));}
                                if p[3]=="1" {internal.push_back(format!("HEARTH DMS {} {} 100",c.peer,c.cursor));}
                                else if p[3]=="0" {c.ready=true;dm_syncing.remove(&p[1]);}
                                else {return Err(failure("invalid DM pagination"));}
                            }
                            Some("LOCATION") => {model.lock().unwrap().event(&line);},
                            _ => return Err(failure("invalid hearth server response")),
                        }
                    } else if matches!(m.command.as_str(),"PRIVMSG"|"NOTICE") && p.len()==2 && m.tags.contains_key("hearth/peer") {
                        let peer=&m.tags["hearth/peer"];
                        let id=m.tags.get("hearth/id").and_then(|id| id.parse::<i64>().ok()).ok_or_else(||failure("missing DM ID"))?;
                        let time=m.tags.get("time").ok_or_else(||failure("missing DM time"))?;
                        let nick=sender(&m).ok_or_else(||failure("invalid DM sender"))?;
                        validate_event(id,nick,&p[1])?;
                        let mut state=model.lock().unwrap();
                        if let Some(request)=&mut pending {
                            if irc::fold(nick)==irc::fold(&state.nick) && (request.request.message.as_ref()==Some(&(irc::fold(&p[0]),p[1].clone())) || request.request.message.as_ref()==Some(&(format!("@{peer}"),p[1].clone()))) {
                                request.echoed=true;
                                let r=state.remote.as_mut().unwrap();if r.inflight.take().is_some() {r.acknowledged+=1;}
                            }
                        }
                        if state.conversations.iter().any(|c| c.peer==*peer && c.ready) {
                            state.receive_dm(peer,id,time,nick,&p[1]).map_err(|_|failure("DM SQLite spool write failed"))?;
                        }
                    } else if matches!(m.command.as_str(),"PRIVMSG"|"NOTICE") && p.len() == 2 && p[0].starts_with('#') && m.tags.contains_key("hearth/id") {
                        let mut state = model.lock().unwrap();
                        let Some(i) = state.index(&p[0]) else {continue;};
                        // Never advance this channel's cursor past unreplayed history.
                        if !state.channels[i].ready {continue;}
                        let id = m.tags.get("hearth/id").and_then(|id| id.parse::<i64>().ok()).ok_or_else(|| failure("live message missing persistent ID"))?;
                        let time = m.tags.get("time").ok_or_else(|| failure("live message missing server time"))?;
                        let nick = sender(&m).ok_or_else(|| failure("invalid sender prefix"))?;
                        validate_event(id,nick,&p[1])?;
                        if let Some(request) = &mut pending {
                            if id > state.channels[i].cursor && irc::fold(nick) == irc::fold(&state.nick)
                                && request.request.message.as_ref() == Some(&(irc::fold(&p[0]),p[1].clone())) {
                                request.echoed = true;
                                let r = state.remote.as_mut().unwrap();
                                if r.inflight.take().is_some() {r.acknowledged += 1;}
                            }
                        }
                        state.receive(&p[0],id,time,nick,&p[1]).map_err(|_| failure("local SQLite spool write failed; server message remains persisted"))?;
                    } else if m.command == "ERROR" || m.command.parse::<u16>().is_ok_and(|n| (400..600).contains(&n) && n != 422) {
                        let error = format!("server rejected request ({})",m.command);
                        if !registered || pending.is_none() || m.command == "ERROR" {return Err(failure(error));}
                        pending.as_mut().unwrap().failed = true;
                        let mut state = model.lock().unwrap();
                        let r = state.remote.as_mut().unwrap();
                        if r.inflight.take().is_some() {r.canceled += 1;}
                        r.error = error;
                    } else {
                        observe(&mut model.lock().unwrap(),&m)?;
                    }
                    // Complete initial readiness only after all desired channels have replayed.
                    if registered && peers_done && internal.is_empty() && syncing.is_empty() && dm_syncing.is_empty() {
                        let mut state = model.lock().unwrap();
                        if state.channels.iter().filter(|c| c.desired).all(|c| c.ready) && state.conversations.iter().all(|c| c.ready) {
                            state.remote.as_mut().unwrap().ready = true;
                        }
                    }
                }
                _ = ticks.tick() => {
                    if last_progress.elapsed() > Duration::from_secs(60)
                        && (!registered || !syncing.is_empty() || !dm_syncing.is_empty() || !model.lock().unwrap().remote.as_ref().unwrap().ready) {
                        return Err(failure("registration/history replay stopped making progress"));
                    }
                    if last_received.elapsed() > Duration::from_secs(120) {return Err(failure("server stopped responding"));}
                    if pending.as_ref().is_some_and(|p| p.sent.elapsed() > Duration::from_secs(15)) {return Err(failure("request acknowledgment timed out; delivery uncertain"));}
                    // Internal requests cannot overlap a user request: numeric errors then
                    // belong to exactly one operation, even for topic and service commands.
                    if pending.is_some() {continue;}
                    if let Some(line) = internal.pop_front() {send(&mut write,&line).await?; continue;}
                    if !syncing.is_empty() || !dm_syncing.is_empty() {continue;}
                    let outgoing = {
                        let mut state = model.lock().unwrap();
                        let r = state.remote.as_mut().unwrap();
                        if r.ready {let request = r.queue.pop_front(); r.inflight = request.clone(); request} else {None}
                    };
                    if let Some(request) = outgoing {
                        sequence += 1;
                        let token = format!("hearth-request-{sequence}");
                        send(&mut write,&request.wire).await?;
                        send(&mut write,&format!("PING :{token}")).await?;
                        pending = Some(Pending {request,token,sent:Instant::now(),failed:false,echoed:false});
                    }
                }
            }
        }
    }.await;
    reader.abort();
    result
}

fn sender(m: &irc::Message) -> Option<&str> {
    m.prefix
        .as_deref()
        .and_then(|s| s.split_once('!'))
        .map(|(nick, _)| nick)
}

fn observe(state: &mut Model, m: &irc::Message) -> io::Result<()> {
    let p = &m.params;
    let nick = sender(m).unwrap_or("");
    let own = irc::fold(nick) == irc::fold(&state.nick);
    match m.command.as_str() {
        "JOIN" if !p.is_empty() => {
            let channel = irc::fold(&p[0]);
            if own {
                let i = state
                    .ensure_channel(&channel)
                    .map_err(|_| failure("too many tracked channels; remount"))?;
                let c = &mut state.channels[i];
                c.desired = true;
                c.joined = true;
                c.ready = false;
                c.members.clear();
            }
            if let Some(i) = state.index(&channel) {
                state.channels[i]
                    .members
                    .insert(irc::fold(nick), nick.into());
            }
        }
        "PART" if !p.is_empty() => {
            if let Some(i) = state.index(&p[0]) {
                let c = &mut state.channels[i];
                c.members.remove(&irc::fold(nick));
                if own {
                    c.desired = false;
                    c.joined = false;
                    c.ready = false;
                    c.members.clear();
                }
            }
        }
        "QUIT" => {
            for c in &mut state.channels {
                c.members.remove(&irc::fold(nick));
            }
        }
        "NICK" if p.len() == 1 => {
            for c in &mut state.channels {
                if c.members.remove(&irc::fold(nick)).is_some() {
                    c.members.insert(irc::fold(&p[0]), p[0].clone());
                }
            }
            for c in &mut state.conversations {
                if m.tags
                    .get("hearth/actor")
                    .map(|actor| actor == &c.peer)
                    .unwrap_or_else(|| irc::fold(&c.nick) == irc::fold(nick))
                {
                    c.nick = p[0].clone();
                }
            }
            if own {
                state.nick = p[0].clone();
            }
        }
        "TOPIC" if p.len() == 2 => {
            if let Some(i) = state.index(&p[0]) {
                state.channels[i].topic = p[1].clone();
            }
        }
        "331" if p.len() >= 2 => {
            if let Some(i) = state.index(&p[1]) {
                state.channels[i].topic.clear();
            }
        }
        "332" if p.len() == 3 => {
            if let Some(i) = state.index(&p[1]) {
                state.channels[i].topic = p[2].clone();
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_event(id: i64, nick: &str, body: &str) -> io::Result<()> {
    if id <= 0 || !irc::valid_nick(nick) || !irc::message_ok(body) {
        return Err(failure("invalid history event"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nickname_reuse_does_not_rename_an_unrelated_dm_identity() {
        let mut state = Model::new("viewer".into());
        let old = "a".repeat(32);
        let new = "b".repeat(32);
        state.peer(&old, "bob").unwrap();
        state.peer(&new, "bob").unwrap();
        let event = irc::parse(&format!(
            "@hearth/actor={new} :bob!agent@localhost NICK :bobby"
        ))
        .unwrap();
        observe(&mut state, &event).unwrap();
        assert_eq!(state.conversations[0].nick, "bob");
        assert_eq!(state.conversations[1].nick, "bobby");
    }
}
