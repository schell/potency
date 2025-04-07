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
    /// Open a store.
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
        key: &'a [impl AsRef<str>],
    ) -> crate::Stored<'a, Option<Self::StoreValue>, Self::Error> {
        let key: String = key
            .iter()
            .map(|k| k.as_ref().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        Box::pin(async move {
            log::trace!("fetching {key}");
            let query = "SELECT value FROM potency WHERE key = :key";
            let mut statement = lock.prepare(query)?;
            statement.bind((":key", key.as_str()))?;
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
        key: &'a [impl AsRef<str>],
        serialized_value: Self::StoreValue,
    ) -> crate::Stored<'a, (), Self::Error> {
        let key: String = key
            .iter()
            .map(|k| k.as_ref().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        Box::pin(async move {
            // UNWRAP: safe because we know `Value` always serializes.
            let value = serde_json::to_string(&serialized_value.0).unwrap();
            log::trace!("storing key {key}: {value}");
            let query = "INSERT INTO potency (key, value) VALUES (:key, :value)";
            let mut statement = lock.prepare(query)?;
            statement.bind(&[(":key", key.as_str()), (":value", value.as_str())][..])?;
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

    fn lock<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Self::Lock<'a>> + 'a>> {
        Box::pin(async { self.connection.lock().await })
    }
}

#[cfg(test)]
mod test {
    use crate::Store;

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
}
