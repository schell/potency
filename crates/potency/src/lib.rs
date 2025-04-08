//! `potency` provides a durability and sync engine all in one!
//!
//! But wait, there's more!
//!
//! TODO: write about "more"

use std::{future::Future, marker::PhantomData, pin::Pin};

#[cfg(feature = "cpu-store")]
pub mod cpu_store;
#[cfg(feature = "json")]
pub mod json;
#[cfg(feature = "sqlite-store")]
pub mod sqlite_store;

mod tuple;
pub use tuple::*;

mod key;
pub use key::*;

mod async_impl;
mod sync_impl;

pub trait SerializeTo<T, E> {
    fn try_into_store_value(&self) -> Result<T, E>;
}

pub trait DeserializeFrom<T, E>: Sized {
    fn try_from_store_value(stored: T) -> Result<Self, E>;
}

pub type Stored<'a, T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + 'a>>;

pub trait IsStore: Clone {
    type Error;
    type Lock<'a>;
    type StoreValue;
    type DeserializedValue<T>;

    fn construct_deserialized<T>(value: T) -> Self::DeserializedValue<T>;
    fn extract_deserialized<T>(value: Self::DeserializedValue<T>) -> T;

    fn lock<'a>(&'a self) -> Pin<Box<dyn Future<Output = Self::Lock<'a>> + 'a>>;

    fn fetch_serialized_by_key<'a, 'l: 'a>(
        lock: &'a Self::Lock<'l>,
        key: impl AsRef<str> + 'a,
    ) -> Stored<'a, Option<Self::StoreValue>, Self::Error>;

    fn store_serialized_by_key<'a, 'l: 'a>(
        lock: &'a mut Self::Lock<'l>,
        key: impl AsRef<str> + 'a,
        serialized_value: Self::StoreValue,
    ) -> Stored<'a, (), Self::Error>;

    fn delete_key<'a, 'l: 'a>(
        lock: &'a mut Self::Lock<'a>,
        key: impl AsRef<str> + 'a,
    ) -> Stored<'a, (), Self::Error>;
}

pub struct Builder<'a, S, I, F, C = Sync> {
    store: &'a Store<S>,
    key: Vec<String>,
    input: I,
    fn_pair: FnPair<I, F, C>,
}

impl<'a, S, C, I: Bundle, F> Builder<'a, S, I, F, C> {
    fn suffix<T>(self, element: T) -> Builder<'a, S, I::Suffixed<T>, F, C> {
        Builder {
            store: self.store,
            key: self.key,
            input: self.input.suffix(element),
            fn_pair: FnPair {
                f: self.fn_pair.f,
                _input: std::marker::PhantomData,
            },
        }
    }

    pub fn param<T: AsKey>(mut self, input: T) -> Builder<'a, S, I::Suffixed<T>, F, C> {
        self.key.push(input.as_key());
        self.suffix(input)
    }
}

pub struct Async;

pub struct Sync;

pub struct FnPair<I, F, C> {
    f: F,
    _input: std::marker::PhantomData<(C, I)>,
}

pub trait IsStoreFunction<I> {
    type Output;

    #[expect(clippy::type_complexity)]
    fn construct_fn(
        self,
        input: I,
    ) -> Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = Self::Output>>>>;
}

impl<S, I, O, E, C, F> Builder<'_, S, I, F, C>
where
    S: IsStore,
    I: Bundle,
    FnPair<I, F, C>: IsStoreFunction<I, Output = Result<O, E>>,
    S::DeserializedValue<O>:
        SerializeTo<S::StoreValue, S::Error> + DeserializeFrom<S::StoreValue, S::Error>,
    S::Error: From<E>,
{
    pub async fn run(self) -> Result<O, S::Error> {
        let Self {
            store,
            key,
            input,
            fn_pair,
        } = self;
        let fn_call = fn_pair.construct_fn(input);
        store.fetch_or_else(key.join(","), fn_call).await
    }
}

#[derive(Clone)]
pub struct Store<S> {
    key: Vec<String>,
    inner: S,
}

impl<S> Store<S>
where
    S: IsStore,
{
    pub fn new(inner: S) -> Self {
        Self { key: vec![], inner }
    }

    fn fetch_or_else<'a, O, E, Fut>(
        &'a self,
        key: impl AsRef<str> + 'a,
        f: impl FnOnce() -> Fut + 'a,
    ) -> Pin<Box<dyn Future<Output = Result<O, S::Error>> + 'a>>
    where
        Fut: Future<Output = Result<O, E>> + 'a,
        S::Error: From<E>,
        S::DeserializedValue<O>:
            SerializeTo<S::StoreValue, S::Error> + DeserializeFrom<S::StoreValue, S::Error>,
    {
        let full_key = key.as_ref().to_owned();
        Box::pin(async move {
            let mut lock = self.inner.lock().await;
            let maybe_serialized_value: Option<S::StoreValue> =
                S::fetch_serialized_by_key(&lock, &key).await?;
            if let Some(serialized) = maybe_serialized_value {
                log::trace!("{full_key:?} is cached, returning cache hit");
                let deserialized = S::DeserializedValue::<O>::try_from_store_value(serialized)?;
                let output = S::extract_deserialized(deserialized);
                Ok(output)
            } else {
                log::trace!("{full_key:?} is not cached, computing the value");
                // TODO: maybe count retries here?
                let output = f().await?;
                let deserialized = S::construct_deserialized(output);
                let serialized_value = deserialized.try_into_store_value()?;
                S::store_serialized_by_key(&mut lock, &full_key, serialized_value).await?;
                Ok(S::extract_deserialized(deserialized))
            }
        })
    }

    pub fn namespace(&self, namespace: impl AsRef<str>) -> Self {
        let namespace = namespace.as_ref().to_string();
        let mut store = self.clone();
        log::trace!("store '{:?}' adding '{namespace}'", store.key);
        store.key.push(namespace);
        store
    }

    pub fn entry<F>(&self, f: F) -> Builder<S, (), F> {
        let _input: PhantomData<(Sync, ())> = PhantomData;
        let fn_pair: FnPair<(), F, Sync> = FnPair { f, _input };
        Builder {
            store: self,
            key: vec![],
            input: (),
            fn_pair,
        }
    }

    pub fn entry_async<F>(&self, f: F) -> Builder<S, (), F, Async> {
        Builder {
            store: self,
            key: vec![],
            input: (),
            fn_pair: FnPair {
                f,
                _input: std::marker::PhantomData,
            },
        }
    }
}
