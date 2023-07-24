use crate::errors::Error;
use crate::highlight::highlight;
use crate::id::Id;
use async_compression::tokio::bufread::{ZstdDecoder, ZstdEncoder};
use lru::LruCache;
use rusqlite::{params, Connection, Transaction};
use rusqlite_migration::{HookError, Migrations, M};
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::task::spawn_blocking;

fn migrations() -> &'static Migrations<'static> {
    static MIGRATIONS: OnceLock<Migrations> = OnceLock::new();

    MIGRATIONS.get_or_init(|| {
        Migrations::new(vec![
            M::up(include_str!("migrations/0001-initial.sql")),
            M::up(include_str!("migrations/0002-add-created-column.sql")),
            M::up(include_str!(
                "migrations/0003-drop-created-add-uid-column.sql"
            )),
            M::up_with_hook(
                include_str!("migrations/0004-add-compressed-column.sql"),
                |tx: &Transaction| {
                    let mut stmt = tx.prepare("SELECT id, text FROM entries")?;

                    let rows = stmt
                        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                        .collect::<Result<Vec<(u32, String)>, _>>()?;

                    tracing::debug!("compressing {} rows", rows.len());

                    for (id, text) in rows {
                        let cursor = Cursor::new(text);
                        let data =
                            zstd::stream::encode_all(cursor, zstd::DEFAULT_COMPRESSION_LEVEL)
                                .map_err(|e| HookError::Hook(e.to_string()))?;

                        tx.execute(
                            "UPDATE entries SET data = ?1 WHERE id = ?2",
                            params![data, id],
                        )?;
                    }

                    Ok(())
                },
            ),
            M::up(include_str!("migrations/0005-drop-text-column.sql")),
        ])
    })
}

/// Our main database and integrated cache.
#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
    cache: Arc<Mutex<Cache>>,
}

/// Database opening modes
#[derive(Debug)]
pub enum Open {
    /// Open in-memory database that is wiped after reload
    Memory,
    /// Open database from given path
    Path(PathBuf),
}

/// Type that stores formatted HTML.
pub type Cache = LruCache<CacheKey, String>;

/// Cache based on identifier and format.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub id: Id,
    pub ext: String,
}

/// An uncompressed entry to be inserted into the database.
#[derive(Default, Debug, Serialize, Deserialize)]
pub struct InsertEntry {
    /// Content
    pub text: String,
    /// File extension
    pub extension: Option<String>,
    /// Expiration in seconds from now
    pub expires: Option<u32>,
    /// Delete if read
    pub burn_after_reading: Option<bool>,
    /// User identifier that inserted the entry
    pub uid: Option<i64>,
}

/// A compressed entry to be inserted.
pub struct CompressedEntry {
    /// Compressed data
    data: Vec<u8>,
    /// Expiration in seconds from now
    expires: Option<u32>,
    /// Delete if read
    burn_after_reading: Option<bool>,
    /// User identifier that inserted the entry
    uid: Option<i64>,
}

/// A raw entry as read from the database.
struct RawEntry {
    /// Compressed data
    data: Vec<u8>,
    /// Entry is expired
    expired: bool,
    /// Entry must be deleted
    must_be_deleted: bool,
    /// User identifier that inserted the entry
    uid: Option<i64>,
}

/// An entry read from the database.
pub struct ReadEntry {
    /// Content
    pub text: String,
    /// Delete if read
    must_be_deleted: bool,
    /// User identifier that inserted the entry
    pub uid: Option<i64>,
}

impl InsertEntry {
    /// Compress the entry for insertion.
    pub async fn compress(self) -> Result<CompressedEntry, Error> {
        let reader = BufReader::new(Cursor::new(self.text));
        let mut encoder = ZstdEncoder::new(reader);
        let mut data = Vec::new();

        encoder
            .read_to_end(&mut data)
            .await
            .map_err(|e| Error::Compression(e.to_string()))?;

        Ok(CompressedEntry {
            data,
            expires: self.expires,
            burn_after_reading: self.burn_after_reading,
            uid: self.uid,
        })
    }
}

impl RawEntry {
    async fn decompress(self) -> Result<ReadEntry, Error> {
        let reader = BufReader::new(Cursor::new(self.data));
        let mut decoder = ZstdDecoder::new(reader);
        let mut text = String::new();

        decoder
            .read_to_string(&mut text)
            .await
            .map_err(|e| Error::Compression(e.to_string()))?;

        Ok(ReadEntry {
            text,
            uid: self.uid,
            must_be_deleted: self.must_be_deleted,
        })
    }
}

impl Database {
    /// Create new database with the given `method` and `cache`.
    pub fn new(method: Open, cache: Cache) -> Result<Self, Error> {
        tracing::debug!("opening {method:?}");

        let mut conn = match method {
            Open::Memory => Connection::open_in_memory()?,
            Open::Path(path) => Connection::open(path)?,
        };

        migrations().to_latest(&mut conn)?;

        let conn = Arc::new(Mutex::new(conn));
        let cache = Arc::new(Mutex::new(cache));

        Ok(Self { conn, cache })
    }

    /// Insert `entry` under `id` into the database and optionally set owner to `uid`.
    pub async fn insert(&self, id: Id, entry: InsertEntry) -> Result<(), Error> {
        let conn = self.conn.clone();
        let id = id.as_u32();
        let entry = entry.compress().await?;

        spawn_blocking(move || match entry.expires {
            None => conn.lock().unwrap().execute(
                "INSERT INTO entries (id, uid, data, burn_after_reading) VALUES (?1, ?2, ?3, ?4)",
                params![id, entry.uid, entry.data, entry.burn_after_reading],
            ),
            Some(expires) => conn.lock().unwrap().execute(
                "INSERT INTO entries (id, uid, data, burn_after_reading, expires) VALUES (?1, ?2, ?3, ?4, datetime('now', ?5))",
                params![
                    id,
                    entry.uid,
                    entry.data,
                    entry.burn_after_reading,
                    format!("{expires} seconds")
                ],
            ),
        })
        .await??;

        Ok(())
    }

    /// Get entire entry for `id`.
    pub async fn get(&self, id: Id) -> Result<ReadEntry, Error> {
        let conn = self.conn.clone();
        let id_as_u32 = id.as_u32();

        let entry = spawn_blocking(move || {
            conn.lock().unwrap().query_row(
                "SELECT data, burn_after_reading, uid, expires < datetime('now') FROM entries WHERE id=?1",
                params![id_as_u32],
                |row| {
                    Ok(RawEntry {
                        data: row.get(0)?,
                        must_be_deleted: row.get::<_, Option<bool>>(1)?.unwrap_or(false),
                        uid: row.get(2)?,
                        expired: row.get::<_, Option<bool>>(3)?.unwrap_or(false),
                    })
                },
            )
        })
        .await??;

        if entry.expired {
            self.delete(id).await?;
            return Err(Error::NotFound);
        }

        if entry.must_be_deleted {
            self.delete(id).await?;
        }

        entry.decompress().await
    }

    /// Get optional `uid` for `id` if it exists but error with `Error::NotFound` if `id` is
    /// expired or does not exist.
    pub async fn get_uid(&self, id: Id) -> Result<Option<i64>, Error> {
        let conn = self.conn.clone();
        let id_as_u32 = id.as_u32();

        let (uid, expired) = spawn_blocking(move || {
            conn.lock().unwrap().query_row(
                "SELECT uid, expires < datetime('now') FROM entries WHERE id=?1",
                params![id_as_u32],
                |row| {
                    let uid: Option<i64> = row.get(0)?;
                    let expired: Option<bool> = row.get(1)?;
                    Ok((uid, expired))
                },
            )
        })
        .await??;

        if expired.unwrap_or(false) {
            self.delete(id).await?;
            return Err(Error::NotFound);
        }

        Ok(uid)
    }

    /// Look up or generate HTML formatted data. Return `None` if `key` is not found.
    pub async fn get_html(&self, key: &CacheKey) -> Result<String, Error> {
        if let Some(html) = self.cache.lock().unwrap().get(key) {
            tracing::trace!(?key, "found cached item");
            return Ok(html.to_string());
        }

        let entry = self.get(key.id).await?;
        let ext = key.ext.clone();
        let html = tokio::task::spawn_blocking(move || highlight(&entry.text, &ext)).await??;

        if !entry.must_be_deleted {
            tracing::trace!(?key, "cache item");
            self.cache.lock().unwrap().put(key.clone(), html.clone());
        }

        Ok(html)
    }

    /// Delete `id`.
    pub async fn delete(&self, id: Id) -> Result<(), Error> {
        let conn = self.conn.clone();
        let id = id.as_u32();

        spawn_blocking(move || {
            conn.lock()
                .unwrap()
                .execute("DELETE FROM entries WHERE id=?1", params![id])
        })
        .await??;

        Ok(())
    }

    /// Retrieve next monotonically increasing uid.
    pub async fn next_uid(&self) -> Result<i64, Error> {
        let conn = self.conn.clone();

        let uid = spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            conn.query_row(
                "UPDATE uids SET n = n + 1 WHERE id = 0 RETURNING n",
                [],
                |row| row.get(0),
            )
        })
        .await??;

        Ok(uid)
    }
}

impl CacheKey {
    /// Make a copy of the owned id.
    pub fn id(&self) -> String {
        self.id.to_string()
    }
}

impl FromStr for CacheKey {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (id, ext) = match value.split_once('.') {
            None => (value.parse()?, "txt".to_string()),
            Some((id, ext)) => (id.parse()?, ext.to_string()),
        };

        Ok(Self { id, ext })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;

    fn new_db() -> Result<Database, Box<dyn std::error::Error>> {
        let cache = Cache::new(NonZeroUsize::new(128).unwrap());
        Ok(Database::new(Open::Memory, cache)?)
    }

    #[tokio::test]
    async fn insert() -> Result<(), Box<dyn std::error::Error>> {
        let db = new_db()?;

        let entry = InsertEntry {
            text: "hello world".to_string(),
            uid: Some(10),
            ..Default::default()
        };

        let id = Id::from(1234);
        db.insert(id, entry).await?;

        let entry = db.get(id).await?;
        assert_eq!(entry.text, "hello world");
        assert!(entry.uid.is_some());
        assert_eq!(entry.uid.unwrap(), 10);

        let result = db.get(Id::from(5678)).await;
        assert!(result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn burn_after_reading() -> Result<(), Box<dyn std::error::Error>> {
        let db = new_db()?;
        let entry = InsertEntry {
            burn_after_reading: Some(true),
            ..Default::default()
        };
        let id = Id::from(1234);
        db.insert(id, entry).await?;
        assert!(db.get(id).await.is_ok());
        assert!(db.get(id).await.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn expired_does_not_exist() -> Result<(), Box<dyn std::error::Error>> {
        let db = new_db()?;

        let entry = InsertEntry {
            expires: Some(1),
            ..Default::default()
        };

        let id = Id::from(1234);
        db.insert(id, entry).await?;

        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        let result = db.get(id).await;
        assert!(result.is_err());
        assert!(matches!(result.err().unwrap(), Error::NotFound));

        Ok(())
    }

    #[tokio::test]
    async fn delete() -> Result<(), Box<dyn std::error::Error>> {
        let db = new_db()?;

        let id = Id::from(1234);
        db.insert(id, InsertEntry::default()).await?;

        assert!(db.get(id).await.is_ok());
        assert!(db.delete(id).await.is_ok());
        assert!(db.get(id).await.is_err());

        Ok(())
    }

    #[test]
    fn cache_key() {
        let key = CacheKey::from_str("bJZCna").unwrap();
        assert_eq!(key.id(), "bJZCna");
        assert_eq!(key.id, 104651828.into());
        assert_eq!(key.ext, "txt");

        let key = CacheKey::from_str("sIiFec.rs").unwrap();
        assert_eq!(key.id(), "sIiFec");
        assert_eq!(key.id, 1243750162.into());
        assert_eq!(key.ext, "rs");

        assert!(CacheKey::from_str("foo").is_err());
        assert!(CacheKey::from_str("bar.rs").is_err());
    }
}
