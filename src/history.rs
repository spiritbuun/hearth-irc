//! IRCv3 CHATHISTORY selection. All SQL structure comes from fixed operators;
//! targets, timestamps, message IDs, and limits are bound parameters.
use crate::storage::{Event, Store};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params_from_iter, types::Value};

#[derive(Clone)]
enum Reference {
    Id(i64),
    Time(String),
}
impl Reference {
    fn column(&self) -> &'static str {
        match self {
            Self::Id(_) => "id",
            Self::Time(_) => "time",
        }
    }
    fn value(&self) -> Value {
        match self {
            Self::Id(n) => (*n).into(),
            Self::Time(s) => s.clone().into(),
        }
    }
}
pub fn timestamp(s: &str) -> Result<String, String> {
    DateTime::parse_from_rfc3339(
        s.strip_prefix("timestamp=")
            .ok_or("Expected timestamp reference")?,
    )
    .map(|t| {
        t.with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Millis, true)
    })
    .map_err(|_| "Invalid timestamp".into())
}
impl Store {
    fn reference(&self, channel: &str, value: &str) -> Result<Reference, String> {
        if value.starts_with("timestamp=") {
            return timestamp(value).map(Reference::Time);
        }
        let prefix = format!("msgid={}-", self.id);
        let id = value
            .strip_prefix(&prefix)
            .ok_or("Invalid message ID")?
            .parse::<i64>()
            .map_err(|_| "Invalid message ID")?;
        self.db
            .query_row(
                "SELECT id FROM events WHERE channel=?1 AND id=?2",
                rusqlite::params![channel, id],
                |r| r.get::<_, i64>(0),
            )
            .map(Reference::Id)
            .map_err(|_| "Message ID is unavailable in this conversation".into())
    }
    fn order_key(&self, r: &Reference) -> Result<(String, i64), String> {
        match r {
            Reference::Time(t) => Ok((t.clone(), 0)),
            Reference::Id(id) => self
                .db
                .query_row("SELECT time,id FROM events WHERE id=?1", [id], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .map_err(|_| "Message unavailable".into()),
        }
    }
    fn select(
        &self,
        channel: &str,
        bounds: &[(&Reference, &str)],
        descending: bool,
        limit: usize,
    ) -> Result<Vec<Event>, String> {
        let mut sql = "SELECT id,channel,time,nick,username,peer,body,command,target,sender FROM events WHERE channel=?".to_owned();
        let mut values = vec![Value::from(channel.to_owned())];
        for (reference, operator) in bounds {
            sql.push_str(&format!(" AND {} {operator} ?", reference.column()));
            values.push(reference.value());
        }
        sql.push_str(if descending {
            " ORDER BY id DESC LIMIT ?"
        } else {
            " ORDER BY id ASC LIMIT ?"
        });
        values.push(Value::from(limit as i64));
        let mut query = self
            .db
            .prepare(&sql)
            .map_err(|_| "History storage failure")?;
        query
            .query_map(params_from_iter(values), |r| {
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
            })
            .map_err(|_| "History storage failure")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|_| "History storage failure".into())
    }
    pub fn chathistory(&self, channel: &str, p: &[String]) -> Result<Vec<Event>, String> {
        let verb = p.first().ok_or("Missing subcommand")?.to_ascii_uppercase();
        let expected = if verb == "BETWEEN" { 5 } else { 4 };
        if p.len() != expected {
            return Err("Invalid parameter count".into());
        }
        let limit = p
            .last()
            .unwrap()
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=100).contains(n))
            .ok_or("Limit must be 1..100")?;
        let first = if verb == "LATEST" && p[2] == "*" {
            None
        } else {
            Some(self.reference(channel, &p[2])?)
        };
        let mut result = match verb.as_str() {
            "LATEST" => match &first {
                Some(r) => self.select(channel, &[(r, ">")], true, limit)?,
                None => self.select(channel, &[], true, limit)?,
            },
            "BEFORE" => self.select(channel, &[(first.as_ref().unwrap(), "<")], true, limit)?,
            "AFTER" => self.select(channel, &[(first.as_ref().unwrap(), ">")], false, limit)?,
            "AROUND" => {
                let r = first.as_ref().unwrap();
                let mut records = self.select(channel, &[(r, "<=")], true, limit.div_ceil(2))?;
                records.extend(self.select(channel, &[(r, ">")], false, limit - records.len())?);
                records
            }
            "BETWEEN" => {
                let a = first.as_ref().unwrap();
                let b = self.reference(channel, &p[3])?;
                if self.order_key(a)? <= self.order_key(&b)? {
                    self.select(channel, &[(a, ">"), (&b, "<")], false, limit)?
                } else {
                    self.select(channel, &[(a, "<"), (&b, ">")], true, limit)?
                }
            }
            _ => return Err("Unknown CHATHISTORY subcommand".into()),
        };
        result.sort_by_key(|r| r.id);
        Ok(result)
    }
    pub fn last_time(&self, channel: &str) -> rusqlite::Result<Option<String>> {
        self.db.query_row(
            "SELECT max(time) FROM events WHERE channel=?1",
            [channel],
            |r| r.get(0),
        )
    }
}
