//! In-memory CPU store implementation.
//!
//! Mostly meant for testing.

use std::{collections::BTreeMap, sync::Arc};

use async_lock::{RwLock, RwLockWriteGuard};

use super::*;

#[derive(Clone)]
pub struct CpuStore {
    inner: Arc<RwLock<BTreeMap<Vec<String>, serde_json::Value>>>,
}

impl IsStore for CpuStore {
    type Error = crate::json::Error;
    type Lock<'a> = RwLockWriteGuard<'a, BTreeMap<Vec<String>, serde_json::Value>>;
    type StoreValue = crate::json::JsonSerialized;

    fn lock<'a>(&'a self) -> Pin<Box<dyn Future<Output = Self::Lock<'a>> + 'a>> {
        Box::pin(async move { self.inner.write().await })
    }

    fn fetch_serialized_by_key<'a, 'l: 'a>(
        lock: &'a Self::Lock<'l>,
        key: &'a [impl AsRef<str>],
    ) -> Stored<'a, Option<Self::StoreValue>, Self::Error> {
        let key = key
            .iter()
            .map(|k| k.as_ref().to_owned())
            .collect::<Vec<_>>();
        Box::pin(async move {
            log::trace!("fetching key {key:?}");
            match lock.get(&key) {
                None => {
                    log::trace!("  {key:?} not found");
                    Ok(None)
                }
                Some(value) => {
                    log::trace!("  found {key:?}");
                    Ok(Some(value.clone().into()))
                }
            }
        })
    }

    fn store_serialized_by_key<'a, 'l: 'a>(
        lock: &'a mut Self::Lock<'l>,
        key: &'a [impl AsRef<str>],
        serialized_value: Self::StoreValue,
    ) -> Stored<'a, (), Self::Error> {
        let key = key
            .iter()
            .map(|k| k.as_ref().to_owned())
            .collect::<Vec<_>>();
        Box::pin(async move {
            log::trace!(
                "storing {key:?}: '{}'",
                serde_json::to_string(&serialized_value.0).unwrap()
            );
            lock.insert(key, serialized_value.0);
            Ok(())
        })
    }

    type DeserializedValue<T> = crate::json::JsonDeserialized<T>;

    fn construct_deserialized<T>(value: T) -> Self::DeserializedValue<T> {
        value.into()
    }

    fn extract_deserialized<T>(value: Self::DeserializedValue<T>) -> T {
        value.0
    }
}

impl CpuStore {
    pub fn new() -> Self {
        Self {
            inner: Default::default(),
        }
    }
}

impl Default for CpuStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod test {

    use json::{Error, JsonDeserialized, JsonSerialized};

    use super::*;

    async fn test_function(millis_to_wait: u32, string: String) -> Result<String, Error> {
        smol::Timer::after(std::time::Duration::from_millis(millis_to_wait as u64)).await;
        Ok(format!("{millis_to_wait} {string}"))
    }

    #[test]
    fn cpu_store_sanity() {
        let _ = env_logger::builder().try_init();
        smol::block_on(async {
            let store = CpuStore::new();
            {
                let mut lock = store.lock().await;
                let maybe_value = CpuStore::fetch_serialized_by_key(&lock, &["hello"])
                    .await
                    .unwrap();
                assert_eq!(None, maybe_value);

                let value = JsonDeserialized((0u32, 1.0f32, "goodbye".to_string()));
                let result: Result<JsonSerialized, Error> = value.try_into_store_value();
                let store_value = result.unwrap();
                CpuStore::store_serialized_by_key(&mut lock, &["hello"], store_value)
                    .await
                    .unwrap();

                let stored = CpuStore::fetch_serialized_by_key(&lock, &["hello"])
                    .await
                    .unwrap()
                    .unwrap();
                let result: Result<JsonDeserialized<(u32, f32, String)>, Error> =
                    JsonDeserialized::try_from_store_value(stored);

                let stored_value = result.unwrap();
                assert_eq!(value.0, stored_value.0);
            }

            let store = Store::new(store);
            let millis_to_wait = 500;
            let input_string = "To each their own.".to_string();
            let start = std::time::Instant::now();
            let output_string = store
                .fetch_or_else(&["the", "key"], || {
                    test_function(millis_to_wait, input_string.clone())
                })
                .await
                .unwrap();
            let elapsed = start.elapsed();
            assert_eq!("500 To each their own.", &output_string);
            assert!(elapsed.as_millis() >= 500);

            let start = std::time::Instant::now();
            let _output_string = store
                .fetch_or_else(&["the", "key"], || {
                    test_function(millis_to_wait, input_string)
                })
                .await
                .unwrap();
            let elapsed = start.elapsed();
            assert!(
                elapsed.as_millis() < 500,
                "elapsed: {}",
                elapsed.as_millis()
            );
        });
    }

    use crate::{cpu_store::CpuStore, Store};

    async fn test_async_function_no_params_no_return() -> Result<(), Error> {
        println!("done!");
        Ok(())
    }

    async fn test_async_function_one_param_string(name: String) -> Result<String, Error> {
        Ok(format!(
            "{name} - this is a test of the emergency pants system",
        ))
    }

    async fn test_async_function_one_param_string_2(millis_to_wait: u32) -> Result<String, Error> {
        smol::Timer::after(std::time::Duration::from_millis(millis_to_wait as u64)).await;
        Ok(format!(
            "{millis_to_wait} - this is a timed test of the emergency pants system"
        ))
    }

    fn test_function_three_params_string(a: f32, b: u32, c: String) -> Result<String, Error> {
        Ok(format!("({a}, {b}, {c})"))
    }

    #[test]
    fn builder_sanity() {
        let _ = env_logger::builder().try_init();
        smol::block_on(async {
            let store = Store::new(CpuStore::new());
            {
                let store = store.namespace("async-param0");
                store
                    .entry_async(test_async_function_no_params_no_return)
                    .run()
                    .await
                    .unwrap();
            }

            {
                let store = store.namespace("async-param1-andparam2");
                let builder = store
                    .entry_async(test_async_function_one_param_string)
                    .param("Bill".to_string());
                builder.run().await.unwrap();

                store
                    .entry_async(test_async_function_one_param_string_2)
                    .param(300u32)
                    .run()
                    .await
                    .unwrap();
            }

            {
                let store = store.namespace("sync-param3");
                let result = store
                    .entry(test_function_three_params_string)
                    .param(69.0f32)
                    .param(666u32)
                    .param("Late Night".to_string())
                    .run()
                    .await
                    .unwrap();
                assert_eq!("(69, 666, Late Night)", &result);
            }
        });
    }
}
