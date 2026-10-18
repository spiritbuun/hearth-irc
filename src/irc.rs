//! Small, bounded IRC framing shared by the server and the filesystem client.
use std::collections::BTreeMap;
use std::io;

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

pub const MAX_FRAME: usize = 512;
pub const MAX_MESSAGE: usize = 350; // Fits a server prefix, even with an IPv6 peer.
pub const REQUIRED_CAPABILITIES: &str = "echo-message server-time hearth-history";
pub const CAPABILITIES: &str =
    "echo-message server-time hearth-history message-tags batch draft/chathistory away-notify";

pub fn action(text: &str) -> Option<&str> {
    text.strip_prefix("\x01ACTION ")?.strip_suffix('\x01')
}
pub fn message_ok(text: &str) -> bool {
    text.len() <= MAX_MESSAGE && text_ok(action(text).unwrap_or(text), MAX_MESSAGE)
}

#[derive(Debug, PartialEq, Eq)]
pub struct Message {
    pub tags: BTreeMap<String, String>,
    pub prefix: Option<String>,
    pub command: String,
    pub params: Vec<String>,
}

pub fn parse(line: &str) -> Result<Message, &'static str> {
    if line.bytes().any(|b| b == 0 || b == b'\r' || b == b'\n') {
        return Err("invalid control character");
    }
    let mut rest = line.trim_start_matches(' ');
    let mut tags = BTreeMap::new();
    if let Some(tagged) = rest.strip_prefix('@') {
        let (raw, next) = tagged.split_once(' ').ok_or("missing command")?;
        for item in raw.split(';') {
            let (key, value) = item.split_once('=').unwrap_or((item, ""));
            if key.is_empty() {
                return Err("empty tag");
            }
            tags.insert(key.to_owned(), value.to_owned());
        }
        rest = next.trim_start_matches(' ');
    }
    let prefix = if let Some(prefixed) = rest.strip_prefix(':') {
        let (prefix, next) = prefixed.split_once(' ').ok_or("missing command")?;
        rest = next.trim_start_matches(' ');
        Some(prefix.to_owned())
    } else {
        None
    };
    let (command, next) = rest.split_once(' ').unwrap_or((rest, ""));
    if command.is_empty() || !command.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("invalid command");
    }
    rest = next.trim_start_matches(' ');
    let mut params = Vec::new();
    while !rest.is_empty() {
        if params.len() == 15 {
            return Err("too many parameters");
        }
        if let Some(trailing) = rest.strip_prefix(':') {
            params.push(trailing.to_owned());
            break;
        }
        let (param, next) = rest.split_once(' ').unwrap_or((rest, ""));
        params.push(param.to_owned());
        rest = next.trim_start_matches(' ');
    }
    Ok(Message {
        tags,
        prefix,
        command: command.to_ascii_uppercase(),
        params,
    })
}

/// Never allocate an unbounded line, including when a peer never sends LF.
/// Keep this future alive across select! calls; cancelling it loses partial bytes.
pub async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> io::Result<Option<String>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unterminated IRC frame",
                ))
            };
        }
        let end = available.iter().position(|b| *b == b'\n').map(|n| n + 1);
        let n = end.unwrap_or(available.len());
        if frame.len() + n > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "IRC frame too long",
            ));
        }
        frame.extend_from_slice(&available[..n]);
        reader.consume(n);
        if end.is_some() {
            frame.pop();
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            return String::from_utf8(frame)
                .map(Some)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "IRC requires UTF-8"));
        }
    }
}

pub fn valid_nick(nick: &str) -> bool {
    !nick.is_empty()
        && nick.len() <= 32
        && nick
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-[]{}\\|^`".contains(&b))
}

pub fn valid_channel(channel: &str) -> bool {
    channel.starts_with('#')
        && channel.len() > 1
        && channel.len() <= 32
        && channel[1..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

pub fn text_ok(text: &str, max: usize) -> bool {
    text.len() <= max && !text.chars().any(char::is_control)
}

pub fn fold(name: &str) -> String {
    name.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parser_preserves_trailing_paths_and_spaces() {
        let m = parse("HEARTH LOCATION agent-host :/workspace/my project").unwrap();
        assert_eq!(
            m.params,
            ["LOCATION", "agent-host", "/workspace/my project"]
        );
        assert_eq!(
            parse("PRIVMSG #general : hello ").unwrap().params[1],
            " hello "
        );
        assert!(parse("PRIVMSG #general :bad\rdata").is_err());
        assert!(parse("@x").is_err());
    }
    #[tokio::test]
    async fn framing_is_bounded_and_handles_split_unicode() {
        use tokio::io::{AsyncWriteExt, BufReader};
        let (mut write, read) = tokio::io::duplex(64);
        let task = tokio::spawn(async move {
            write.write_all(b"PRIVMSG #g :\xc3").await.unwrap();
            write.write_all(b"\xa9\r\nPING :two\n").await.unwrap();
        });
        let mut read = BufReader::new(read);
        assert_eq!(
            read_frame(&mut read, 512).await.unwrap().unwrap(),
            "PRIVMSG #g :é"
        );
        assert_eq!(
            read_frame(&mut read, 512).await.unwrap().unwrap(),
            "PING :two"
        );
        task.await.unwrap();
        assert_eq!(read_frame(&mut read, 512).await.unwrap(), None);
        let mut oversized = BufReader::new(&b"123456789"[..]);
        assert!(read_frame(&mut oversized, 8).await.is_err());
    }
}
