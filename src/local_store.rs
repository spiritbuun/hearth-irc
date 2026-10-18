//! Disk-backed rendered logs. SQLite owns the temporary file; the IRCd owns the
//! durable canonical history. Open readers pin byte endpoints, never whole logs.
use rusqlite::{Connection, params};

pub struct Logs(Connection);

impl Logs {
    pub fn new() -> rusqlite::Result<Self> {
        let db = Connection::open("")?;
        db.execute_batch(
            "PRAGMA cache_size=-2048; PRAGMA temp_store=FILE;
            CREATE TABLE chunks(stream INTEGER NOT NULL, pos INTEGER NOT NULL,
                data BLOB NOT NULL, PRIMARY KEY(stream,pos)) WITHOUT ROWID;",
        )?;
        Ok(Self(db))
    }

    pub fn append(&self, stream: u64, offset: usize, data: &[u8]) -> rusqlite::Result<()> {
        if !data.is_empty() {
            self.0.execute(
                "INSERT INTO chunks VALUES (?1,?2,?3)",
                params![stream as i64, offset as i64, data],
            )?;
        }
        Ok(())
    }

    pub fn read(&self, stream: u64, offset: usize, count: usize) -> rusqlite::Result<Vec<u8>> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let first: i64 = self.0.query_row(
            "SELECT coalesce(max(pos),0) FROM chunks WHERE stream=?1 AND pos<=?2",
            params![stream as i64, offset as i64],
            |r| r.get(0),
        )?;
        let mut query = self.0.prepare(
            "SELECT pos,data FROM chunks WHERE stream=?1 AND pos>=?2 AND pos<?3 ORDER BY pos",
        )?;
        let mut rows = query.query(params![
            stream as i64,
            first,
            offset.saturating_add(count) as i64
        ])?;
        let mut result = Vec::with_capacity(count);
        while let Some(row) = rows.next()? {
            let pos: i64 = row.get(0)?;
            let data: Vec<u8> = row.get(1)?;
            let start = offset.saturating_sub(pos as usize).min(data.len());
            let remaining = count - result.len();
            result.extend_from_slice(&data[start..start.saturating_add(remaining).min(data.len())]);
        }
        Ok(result)
    }
}
