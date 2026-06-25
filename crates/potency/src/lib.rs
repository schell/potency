//! `potency` is a bare-bones durability and synchronization library for
//! writing idempotent processes without thinking too hard.
//!
//! For some background on durability:
//!
//! <https://flawless.dev/docs/>
//!
//! The rough idea: the results of "expensive" and fallible processes are
//! cached under a key derived from a namespace and input parameters. Before
//! running the work, the cache is queried; on a hit the result is read back
//! instead of recomputed.
//!
//! `potency` abstracts over multiple persistence layers via [`Store<S>`] and
//! supports **multi-color** functions — both sync (`fn -> T`) and async
//! (`async fn -> impl Future<Output = T>`).
//!
//! > **The `potency` API itself is always async.** Every builder returns a
//! > future that must be `.await`ed, even when the work you're wrapping is a
//! > plain sync function. Multi-color describes the *work*, not the runtime.
//!
//! ## Quickstart
//!
//! ```rust,no_run
//! # async fn doc() {
//! use potency::{cpu_store::CpuStore, Store};
//!
//! async fn three(a: u32, b: u32, c: u32) -> Result<u32, potency::json::Error> {
//!     Ok(a + b + c)
//! }
//!
//! let store = Store::new(CpuStore::new());
//! let n = store
//!     .entry_async(three)
//!     .param(1u32).param(2u32).param(3u32)
//!     .run()
//!     .await
//!     .unwrap();
//! assert_eq!(n, 6);
//! # }
//! ```
//!
//! For the full walkthrough — namespaces, keying, custom stores, durable
//! side-effects, and "when not to use this" — see the [`tutorial`] module.

#[cfg(doc)]
pub mod tutorial;

use std::{future::Future, marker::PhantomData, pin::Pin};

#[cfg(feature = "cpu-store")]
pub mod cpu_store;
#[cfg(feature = "json")]
pub mod json;
#[cfg(feature = "sqlite-store")]
pub mod sqlite_store;

#[cfg(feature = "json")]
pub mod effect;

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
        lock: &'a mut Self::Lock<'l>,
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

    pub fn entry<F>(&self, f: F) -> Builder<'_, S, (), F> {
        let _input: PhantomData<(Sync, ())> = PhantomData;
        let fn_pair: FnPair<(), F, Sync> = FnPair { f, _input };
        Builder {
            store: self,
            key: vec![],
            input: (),
            fn_pair,
        }
    }

    pub fn entry_async<F>(&self, f: F) -> Builder<'_, S, (), F, Async> {
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

    /// Begin building a durable *side-effect* entry.
    ///
    /// Unlike [`Store::entry`]/[`Store::entry_async`], which cache a function's
    /// return value, an effect represents an operation whose real product is
    /// *external state* (e.g. files on disk). The effect's [`Manifest`] — a
    /// small, serializable description of the committed result — is what gets
    /// cached. On replay, the effect is skipped only if its manifest is cached
    /// **and** [`Effect::verify`] confirms the external state still holds.
    ///
    /// [`Manifest`]: Effect::Manifest
    pub fn effect<E>(&self, effect: E) -> EffectBuilder<'_, S, E> {
        EffectBuilder {
            store: self,
            key: self.key.clone(),
            effect,
        }
    }
}

/// A durable side-effecting operation.
///
/// The contract is split into four steps so that `potency` can own the
/// ordering guarantee that makes replay safe:
///
/// 1. [`fresh_staging`] — allocate a clean staging location for a new attempt.
/// 2. [`produce`] — perform the work into staging, returning a [`Manifest`].
/// 3. [`commit`] — atomically promote staging to its final location.
/// 4. [`verify`] — on a cache hit, confirm the committed effect still holds.
///
/// **Invariant maintained by [`EffectBuilder::run`]:** a cache entry exists
/// *iff* the effect is committed. A crash before the manifest is stored leaves
/// no entry, so the next run re-attempts cleanly.
///
/// [`Manifest`]: Effect::Manifest
/// [`fresh_staging`]: Effect::fresh_staging
/// [`produce`]: Effect::produce
/// [`commit`]: Effect::commit
/// [`verify`]: Effect::verify
pub trait Effect {
    /// A handle to where work happens before commit (e.g. a temp directory).
    type Staging;
    /// A small, serializable record of the committed effect.
    type Manifest;
    /// The effect's error type.
    type Error;

    /// Allocate a fresh staging location for a new attempt, keyed by `key`.
    ///
    /// Implementations should ensure the location starts empty (e.g. remove any
    /// leftover staging from a previously-crashed attempt).
    fn fresh_staging<'a>(&'a self, key: &'a str) -> Stored<'a, Self::Staging, Self::Error>;

    /// Perform the work into `staging`, returning a manifest describing it.
    ///
    /// Takes `staging` by reference so the same handle can be passed to
    /// [`commit`](Effect::commit) afterwards.
    fn produce<'a>(&'a self, staging: &'a Self::Staging)
        -> Stored<'a, Self::Manifest, Self::Error>;

    /// Atomically promote `staging` to its final committed location.
    fn commit<'a>(
        &'a self,
        staging: &'a Self::Staging,
        manifest: &'a Self::Manifest,
    ) -> Stored<'a, (), Self::Error>;

    /// On a cache hit, confirm the committed effect still holds.
    fn verify<'a>(&'a self, manifest: &'a Self::Manifest) -> Stored<'a, bool, Self::Error>;
}

/// Error returned by [`EffectBuilder::run`]: either from the backing store or
/// from the effect itself. Keeps the two error domains distinct rather than
/// forcing one to absorb the other.
#[derive(Debug)]
pub enum EffectError<SE, EE> {
    /// An error from the backing [`Store`].
    Store(SE),
    /// An error from the [`Effect`].
    Effect(EE),
}

impl<SE: std::fmt::Display, EE: std::fmt::Display> std::fmt::Display for EffectError<SE, EE> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EffectError::Store(e) => write!(f, "store error: {e}"),
            EffectError::Effect(e) => write!(f, "effect error: {e}"),
        }
    }
}

impl<SE, EE> std::error::Error for EffectError<SE, EE>
where
    SE: std::fmt::Debug + std::fmt::Display,
    EE: std::fmt::Debug + std::fmt::Display,
{
}

/// Builder for a durable side-effect entry. Compose the cache key with
/// [`EffectBuilder::param`], then call [`EffectBuilder::run`].
pub struct EffectBuilder<'a, S, E> {
    store: &'a Store<S>,
    key: Vec<String>,
    effect: E,
}

impl<'a, S, E> EffectBuilder<'a, S, E> {
    /// Append a parameter to the effect's cache key.
    pub fn param<T: AsKey>(mut self, input: T) -> Self {
        self.key.push(input.as_key());
        self
    }

    /// Append a namespace segment to the effect's cache key.
    pub fn namespace(mut self, ns: impl AsRef<str>) -> Self {
        self.key.push(ns.as_ref().to_string());
        self
    }
}

impl<S, E, M, Err> EffectBuilder<'_, S, E>
where
    S: IsStore,
    E: Effect<Manifest = M, Error = Err>,
    M: Clone,
    S::DeserializedValue<M>:
        SerializeTo<S::StoreValue, S::Error> + DeserializeFrom<S::StoreValue, S::Error>,
{
    /// Run the durable effect protocol.
    ///
    /// - **Hit + valid:** returns the cached manifest, performing no work.
    /// - **Hit + stale:** deletes the entry and re-runs.
    /// - **Miss:** stages, produces, commits, then records the manifest.
    pub async fn run(self) -> Result<M, EffectError<S::Error, Err>> {
        let Self { store, key, effect } = self;
        let full_key = key.join(",");
        let mut lock = store.inner.lock().await;

        // 1. Check the cache.
        let cached: Option<S::StoreValue> = S::fetch_serialized_by_key(&lock, &full_key)
            .await
            .map_err(EffectError::Store)?;
        if let Some(serialized) = cached {
            let deserialized = S::DeserializedValue::<M>::try_from_store_value(serialized)
                .map_err(EffectError::Store)?;
            let manifest = S::extract_deserialized(deserialized);
            // 2. Verify the committed effect still holds.
            if effect
                .verify(&manifest)
                .await
                .map_err(EffectError::Effect)?
            {
                log::trace!("{full_key:?} effect cache hit (verified)");
                return Ok(manifest);
            }
            log::trace!("{full_key:?} effect cache stale; invalidating");
            S::delete_key(&mut lock, &full_key)
                .await
                .map_err(EffectError::Store)?;
        }

        // 3. Miss (or invalidated): stage -> produce -> commit -> record.
        log::trace!("{full_key:?} effect computing");
        let staging = effect
            .fresh_staging(&full_key)
            .await
            .map_err(EffectError::Effect)?;
        let manifest = effect
            .produce(&staging)
            .await
            .map_err(EffectError::Effect)?;
        effect
            .commit(&staging, &manifest)
            .await
            .map_err(EffectError::Effect)?;

        // 4. Only now record the manifest: entry exists iff committed.
        let deserialized = S::construct_deserialized(manifest.clone());
        let serialized = deserialized
            .try_into_store_value()
            .map_err(EffectError::Store)?;
        S::store_serialized_by_key(&mut lock, &full_key, serialized)
            .await
            .map_err(EffectError::Store)?;
        Ok(manifest)
    }
}
