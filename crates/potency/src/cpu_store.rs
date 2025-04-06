//! In-memory CPU store implementation.
//!
//! Mostly meant for testing.

use std::{collections::BTreeMap, sync::Arc};

use async_lock::RwLock;
use snafu::prelude::*;

use super::*;

#[derive(Snafu, Debug)]
pub enum Error {
    #[snafu(display("Error serialize: {source}"))]
    Serialize { source: serde_json::Error },

    #[snafu(display("Error deserializing: {source}"))]
    Deserialize { source: serde_json::Error },

    #[snafu(display("{msg}"))]
    Other { msg: String },
}

pub struct CpuStoreDeserializedValue<T>(T);

#[derive(Debug, PartialEq)]
pub struct CpuStoreValue(serde_json::Value);

impl<T: serde::de::DeserializeOwned> DeserializeFrom<CpuStoreValue, Error>
    for CpuStoreDeserializedValue<T>
{
    fn try_from_store_value(stored: CpuStoreValue) -> Result<Self, Error> {
        let value = serde_json::from_value(stored.0).context(DeserializeSnafu)?;
        Ok(Self(value))
    }
}

impl<T: Clone + serde::Serialize> SerializeTo<CpuStoreValue, Error>
    for CpuStoreDeserializedValue<T>
{
    fn try_into_store_value(&self) -> Result<CpuStoreValue, Error> {
        let value = serde_json::to_value(self.0.clone()).context(SerializeSnafu)?;
        Ok(CpuStoreValue(value))
    }
}

#[derive(Clone)]
pub struct CpuStore {
    inner: Arc<RwLock<BTreeMap<Vec<String>, serde_json::Value>>>,
}

impl IsStore for CpuStore {
    type Error = Error;

    type StoreValue = CpuStoreValue;

    fn fetch_serialized_by_key<'a>(
        &'a self,
        key: &'a [impl AsRef<str>],
    ) -> Stored<'a, Option<Self::StoreValue>, Self::Error> {
        let key = key
            .iter()
            .map(|k| k.as_ref().to_owned())
            .collect::<Vec<_>>();
        Box::pin(async move {
            log::trace!("fetching key {key:?}");
            let guard = self.inner.read().await;
            match guard.get(&key) {
                None => {
                    log::trace!("  {key:?} not found");
                    Ok(None)
                }
                Some(value) => {
                    log::trace!("  found {key:?}");
                    Ok(Some(CpuStoreValue(value.clone())))
                }
            }
        })
    }

    fn store_serialized_by_key<'a>(
        &'a self,
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
            let mut guard = self.inner.write().await;
            guard.insert(key, serialized_value.0);
            Ok(())
        })
    }

    type DeserializedValue<T> = CpuStoreDeserializedValue<T>;

    fn construct_deserialized<T>(value: T) -> Self::DeserializedValue<T> {
        CpuStoreDeserializedValue(value)
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
            let maybe_value = store.fetch_serialized_by_key(&["hello"]).await.unwrap();
            assert_eq!(None, maybe_value);

            let value = CpuStoreDeserializedValue((0u32, 1.0f32, "goodbye".to_string()));
            let cpu_value: CpuStoreValue = value.try_into_store_value().unwrap();
            store
                .store_serialized_by_key(&["hello"], cpu_value)
                .await
                .unwrap();

            let stored = store
                .fetch_serialized_by_key(&["hello"])
                .await
                .unwrap()
                .unwrap();
            let stored_value =
                CpuStoreDeserializedValue::<(u32, f32, String)>::try_from_store_value(stored)
                    .unwrap();
            assert_eq!(value.0, stored_value.0);

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

    async fn test_async_function_no_params_no_return() -> Result<(), super::Error> {
        println!("done!");
        Ok(())
    }

    async fn test_async_function_one_param_string(name: String) -> Result<String, super::Error> {
        Ok(format!(
            "{name} - this is a test of the emergency pants system",
        ))
    }

    async fn test_async_function_one_param_string_2(
        millis_to_wait: u32,
    ) -> Result<String, super::Error> {
        smol::Timer::after(std::time::Duration::from_millis(millis_to_wait as u64)).await;
        Ok(format!(
            "{millis_to_wait} - this is a timed test of the emergency pants system"
        ))
    }

    fn test_function_three_params_string(
        a: f32,
        b: u32,
        c: String,
    ) -> Result<String, super::Error> {
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
