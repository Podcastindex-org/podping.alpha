use rusqlite::{params, Connection};
use std::error::Error;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Archive {
    conn: Connection,
}

impl Archive {
    /// Open (or create) the SQLite archive at the given path.
    pub fn open(path: &str) -> Result<Self, Box<dyn Error>> {
        let conn = Connection::open(path)?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages (
                 hash       TEXT PRIMARY KEY,
                 payload    BLOB,
                 created_at INTEGER
             );
             CREATE TABLE IF NOT EXISTS manifest (
                 hour_key   TEXT,
                 hash       TEXT,
                 sender     TEXT,
                 medium     TEXT,
                 reason     TEXT,
                 timestamp  INTEGER,
                 iri_count  INTEGER,
                 UNIQUE(hour_key, hash)
             );",
        )?;

        Ok(Archive { conn })
    }

    /// Store a notification payload, deduplicating by its blake3 content hash.
    /// Returns `true` if the row was newly inserted, `false` if it already existed.
    pub fn store(
        &self,
        payload: &[u8],
        sender: &str,
        medium: &str,
        reason: &str,
        timestamp: u64,
        iri_count: usize,
    ) -> Result<bool, Box<dyn Error>> {
        let hash = blake3::hash(payload);
        let hash_hex = hash.to_hex().to_string();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_secs();

        // INSERT OR IGNORE deduplicates by content hash
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO messages (hash, payload, created_at) VALUES (?1, ?2, ?3)",
            params![hash_hex, payload, now as i64],
        )?;

        // hour_key for manifest partitioning: "YYYY-MM-DD-HH"
        let hour_key = {
            // Simple hour key from unix timestamp
            let hours_since_epoch = timestamp / 3600;
            let day = hours_since_epoch / 24;
            let hour = hours_since_epoch % 24;
            let days_since_epoch = day;
            // Approximate date from days since epoch
            format!("{}-{:02}", days_since_epoch, hour)
        };

        self.conn.execute(
            "INSERT OR IGNORE INTO manifest (hour_key, hash, sender, medium, reason, timestamp, iri_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                hour_key,
                hash_hex,
                sender,
                medium,
                reason,
                timestamp as i64,
                iri_count as i64,
            ],
        )?;

        Ok(inserted > 0)
    }
}
