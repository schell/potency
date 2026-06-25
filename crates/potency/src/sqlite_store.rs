//! Sqlite storage layer.
use snafu::prelude::*;

use std::sync::Arc;

use crate::{
    json::{JsonDeserialized, JsonSerialized},
    IsStore,
};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Json storage error: {source}"))]
    Json { source: crate::json::Error },

    #[snafu(display("Sqlite error: {source}"))]
    Sqlite { source: sqlite::Error },
}

impl From<crate::json::Error> for Error {
    fn from(source: crate::json::Error) -> Self {
        Error::Json { source }
    }
}

impl From<sqlite::Error> for Error {
    fn from(source: sqlite::Error) -> Self {
        Error::Sqlite { source }
    }
}

#[derive(Clone)]
pub struct SqliteStore {
    connection: Arc<async_lock::Mutex<sqlite::Connection>>,
}

impl SqliteStore {
    /// Open a store backed by a SQLite database at `path`.
    ///
    /// Use `":memory:"` for a per-connection in-memory database (tests), or a
    /// file path for a persistent store that survives process restarts.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # async fn doc() -> Result<(), potency::sqlite_store::Error> {
    /// use potency::{sqlite_store::SqliteStore, Store};
    ///
    /// let store = Store::new(SqliteStore::open("state.db").await?);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn open(path: impl AsRef<std::path::Path>) -> Result<Self, Error> {
        let store = Self {
            connection: Arc::new(async_lock::Mutex::new({
                sqlite::Connection::open_with_flags(
                    path,
                    sqlite::OpenFlags::default().with_create().with_read_write(),
                )?
            })),
        };
        store.migrate().await?;
        Ok(store)
    }

    /// Run migrations of the table, if needed.
    async fn migrate(&self) -> Result<(), Error> {
        log::trace!("creating potency sqlite key value table, if needed");
        let guard = self.connection.lock().await;
        let query = r#"CREATE TABLE IF NOT EXISTS potency(
            key TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
        )"#;
        guard.execute(query)?;
        Ok(())
    }
}

impl IsStore for SqliteStore {
    type Error = Error;
    type Lock<'a> = async_lock::MutexGuard<'a, sqlite::Connection>;
    type StoreValue = JsonSerialized;
    type DeserializedValue<T> = JsonDeserialized<T>;

    fn construct_deserialized<T>(value: T) -> Self::DeserializedValue<T> {
        value.into()
    }

    fn extract_deserialized<T>(value: Self::DeserializedValue<T>) -> T {
        value.0
    }

    fn fetch_serialized_by_key<'a, 'l: 'a>(
        lock: &'a Self::Lock<'l>,
        key: impl AsRef<str> + 'a,
    ) -> crate::Stored<'a, Option<Self::StoreValue>, Self::Error> {
        Box::pin(async move {
            log::trace!("fetching {}", key.as_ref());
            let query = "SELECT value FROM potency WHERE key = :key";
            let mut statement = lock.prepare(query)?;
            statement.bind((":key", key.as_ref()))?;
            match statement.next()? {
                sqlite::State::Row => {
                    let string_value = statement.read::<String, _>("value")?;
                    let value: serde_json::Value = serde_json::from_str(&string_value)
                        .map_err(|source| crate::json::Error::Deserialize { source })?;
                    Ok(Some(value.into()))
                }
                sqlite::State::Done => Ok(None),
            }
        })
    }

    fn store_serialized_by_key<'a, 'l: 'a>(
        lock: &'a mut Self::Lock<'l>,
        key: impl AsRef<str> + 'a,
        serialized_value: Self::StoreValue,
    ) -> crate::Stored<'a, (), Self::Error> {
        Box::pin(async move {
            // UNWRAP: safe because we know `Value` always serializes.
            let value = serde_json::to_string(&serialized_value.0).unwrap();
            log::trace!("storing key {}: {value}", key.as_ref());
            // `INSERT OR REPLACE` so that re-storing a key (e.g. after a
            // verify-invalidated effect is recomputed) overwrites rather than
            // failing the PRIMARY KEY constraint.
            let query = "INSERT OR REPLACE INTO potency (key, value) VALUES (:key, :value)";
            let mut statement = lock.prepare(query)?;
            statement.bind(&[(":key", key.as_ref()), (":value", value.as_str())][..])?;
            match statement.next()? {
                sqlite::State::Row => {
                    log::trace!("Row");
                }
                sqlite::State::Done => {
                    log::trace!("Done");
                }
            }
            Ok(())
        })
    }

    fn delete_key<'a, 'l: 'a>(
        lock: &'a mut Self::Lock<'l>,
        key: impl AsRef<str> + 'a,
    ) -> crate::Stored<'a, (), Self::Error> {
        Box::pin(async move {
            // `DELETE` (not the invalid `REMOVE`) is required SQL; the durable
            // effect protocol relies on this to invalidate stale entries.
            let mut statement = lock.prepare("DELETE FROM potency WHERE key = :key")?;
            statement.bind((":key", key.as_ref()))?;
            let result = statement.next()?;
            log::trace!("delete {result:?}");
            Ok(())
        })
    }

    fn lock<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Self::Lock<'a>> + 'a>> {
        Box::pin(async { self.connection.lock().await })
    }
}

#[cfg(test)]
mod test {
    use crate::{
        json::{JsonDeserialized, JsonSerialized},
        IsStore, SerializeTo, Store,
    };

    use super::SqliteStore;

    async fn three(a: u32, b: u32, c: u32) -> Result<u32, super::Error> {
        Ok(a + b + c)
    }

    #[test]
    fn sqlite_sanity() {
        let _ = env_logger::builder().try_init();
        smol::block_on(async {
            let store = match SqliteStore::open(":memory:").await {
                Ok(s) => s,
                Err(e) => panic!("{e}"),
            };
            let store = Store::new(store);
            let sum = store
                .entry_async(three)
                .param(1)
                .param(2)
                .param(3)
                .run()
                .await
                .unwrap();
            assert_eq!(6, sum);
        });
    }

    /// Regression test for the `DELETE` and `INSERT OR REPLACE` SQL fixes.
    /// The durable effect protocol relies on both: re-storing an existing key
    /// (overwrite) and deleting a stale key.
    #[test]
    fn sqlite_delete_and_overwrite() {
        let _ = env_logger::builder().try_init();
        smol::block_on(async {
            let store = SqliteStore::open(":memory:").await.unwrap();

            let val = |n: u32| -> JsonSerialized {
                let d: JsonDeserialized<u32> = JsonDeserialized(n);
                SerializeTo::<JsonSerialized, super::Error>::try_into_store_value(&d).unwrap()
            };

            // Store, then overwrite the same key (would fail with plain INSERT).
            {
                let mut lock = store.lock().await;
                SqliteStore::store_serialized_by_key(&mut lock, "k", val(1))
                    .await
                    .unwrap();
                SqliteStore::store_serialized_by_key(&mut lock, "k", val(2))
                    .await
                    .unwrap();
            }
            {
                let lock = store.lock().await;
                let got = SqliteStore::fetch_serialized_by_key(&lock, "k")
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(got.0, serde_json::json!(2));
            }

            // Delete (would error with invalid `REMOVE`).
            {
                let mut lock = store.lock().await;
                SqliteStore::delete_key(&mut lock, "k").await.unwrap();
            }
            {
                let lock = store.lock().await;
                let gone = SqliteStore::fetch_serialized_by_key(&lock, "k")
                    .await
                    .unwrap();
                assert!(gone.is_none());
            }
        });
    }
}
