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
//! `potency` supports **multi-color** functions — both sync (`fn -> T`) and
//! async (`async fn -> impl Future<Output = T>`). Storage is SQLite-backed;
//! pass `":memory:"` for tests or a file path for a persistent store that
//! survives process restarts.
//!
//! > **The `potency` API itself is always async.** Every builder returns a
//! > future that must be `.await`ed, even when the work you're wrapping is a
//! > plain sync function. Multi-color describes the *work*, not the runtime.
//!
//! ## Quickstart
//!
//! ```rust,no_run
//! # async fn doc() -> Result<(), potency::Error> {
//! use potency::Store;
//!
//! async fn three(a: u32, b: u32, c: u32) -> Result<u32, potency::Error> {
//!     Ok(a + b + c)
//! }
//!
//! let store = Store::open(":memory:").await?;
//! let n = store
//!     .entry_async(three)
//!     .param(1u32).param(2u32).param(3u32)
//!     .run()
//!     .await?;
//! assert_eq!(n, 6);
//! # Ok(())
//! # }
//! ```
//!
//! For the full walkthrough — namespaces, keying, durable side-effects, and
//! "when not to use this" — see the [`tutorial`] module.

#[cfg(doc)]
pub mod tutorial;

use std::{future::Future, marker::PhantomData, pin::Pin, sync::Arc};

pub mod effect;

mod key;
pub use key::*;

mod tuple;
pub use tuple::*;

mod async_impl;
mod sync_impl;

pub use potency_macros::durable;

/// Errors returned by `potency`.
#[derive(Debug, snafu::Snafu)]
pub enum StoreError {
    /// A SQLite-level error.
    Sqlite { source: sqlite::Error },
    /// A JSON (de)serialization error from the value cache.
    Json { source: serde_json::Error },
}

impl From<sqlite::Error> for StoreError {
    fn from(source: sqlite::Error) -> Self {
        StoreError::Sqlite { source }
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(source: serde_json::Error) -> Self {
        StoreError::Json { source }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(source: std::io::Error) -> Self {
        StoreError::Json {
            source: serde_json::Error::io(source),
        }
    }
}

/// Backward-compat alias: `potency::Error` resolves to the store's error.
pub type Error = StoreError;

pub struct Builder<'a, I, F, C = Sync> {
    store: &'a Store,
    key: Vec<String>,
    input: I,
    fn_pair: FnPair<I, F, C>,
}

impl<'a, C, I: Bundle, F> Builder<'a, I, F, C> {
    fn suffix<T>(self, element: T) -> Builder<'a, I::Suffixed<T>, F, C> {
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

    pub fn param<T: AsKey>(mut self, input: T) -> Builder<'a, I::Suffixed<T>, F, C> {
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

impl<I, O, E, C, F> Builder<'_, I, F, C>
where
    I: Bundle,
    FnPair<I, F, C>: IsStoreFunction<I, Output = Result<O, E>>,
    O: serde::Serialize + serde::de::DeserializeOwned + Clone + Send + 'static,
    E: Into<StoreError> + Send + 'static,
{
    pub async fn run(self) -> Result<O, StoreError> {
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
pub struct Store {
    key: Vec<String>,
    inner: Arc<async_lock::Mutex<sqlite::Connection>>,
}

impl Store {
    /// Open a SQLite-backed store at `path`. Use `":memory:"` for an
    /// in-memory database (tests); pass a file path for persistence.
    pub async fn open(path: impl AsRef<std::path::Path>) -> Result<Self, StoreError> {
        let inner = Arc::new(async_lock::Mutex::new({
            sqlite::Connection::open_with_flags(
                path,
                sqlite::OpenFlags::default().with_create().with_read_write(),
            )?
        }));
        // Run migrations.
        {
            let guard = inner.lock().await;
            let query = r#"CREATE TABLE IF NOT EXISTS potency(
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            )"#;
            guard.execute(query)?;
        }
        Ok(Self { key: vec![], inner })
    }

    /// Open an in-memory store. Convenience for tests.
    pub async fn in_memory() -> Result<Self, StoreError> {
        Self::open(":memory:").await
    }

    /// Fetch the cached value for `key` or run `f` to compute and store it.
    ///
    /// `f` must return `Result<O, E>` where `E: Into<StoreError>`. On a miss
    /// the `Ok` value is serialized and stored; on `Err` the value is
    /// returned to the caller and **not** stored.
    fn fetch_or_else<'a, O, E, Fut>(
        &'a self,
        key: impl AsRef<str> + 'a,
        f: impl FnOnce() -> Fut + 'a,
    ) -> Pin<Box<dyn Future<Output = Result<O, StoreError>> + 'a>>
    where
        Fut: Future<Output = Result<O, E>> + 'a,
        O: serde::Serialize + serde::de::DeserializeOwned + Clone + Send + 'static,
        E: Into<StoreError>,
    {
        let full_key = key.as_ref().to_owned();
        Box::pin(async move {
            let mut lock = self.inner.lock().await;
            let maybe_value: Option<serde_json::Value> = fetch_value(&mut lock, &full_key).await?;
            if let Some(json_value) = maybe_value {
                log::trace!("{full_key:?} is cached, returning cache hit");
                let output: O = serde_json::from_value(json_value)?;
                Ok(output)
            } else {
                log::trace!("{full_key:?} is not cached, computing the value");
                let output = f().await.map_err(Into::into)?;
                let json_value = serde_json::to_value(output.clone())?;
                store_value(&mut lock, &full_key, &json_value).await?;
                Ok(output)
            }
        })
    }

    /// Attach a namespace segment to subsequent calls.
    pub fn namespace(&self, namespace: impl AsRef<str>) -> Self {
        let namespace = namespace.as_ref().to_string();
        let mut store = self.clone();
        log::trace!("store '{:?}' adding '{namespace}'", store.key);
        store.key.push(namespace);
        store
    }

    /// Begin a sync entry.
    pub fn entry<F>(&self, f: F) -> Builder<'_, (), F> {
        let _input: PhantomData<(Sync, ())> = PhantomData;
        let fn_pair: FnPair<(), F, Sync> = FnPair { f, _input };
        Builder {
            store: self,
            key: vec![],
            input: (),
            fn_pair,
        }
    }

    /// Begin an async entry.
    pub fn entry_async<F>(&self, f: F) -> Builder<'_, (), F, Async> {
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

    /// Begin a durable side-effect entry. See [`effect::fs_effect`] for the
    /// common filesystem case.
    pub fn effect<E>(&self, effect: E) -> EffectBuilder<'_, E> {
        EffectBuilder {
            store: self,
            key: self.key.clone(),
            effect,
        }
    }
}

async fn fetch_value(
    lock: &mut async_lock::MutexGuard<'_, sqlite::Connection>,
    key: &str,
) -> Result<Option<serde_json::Value>, StoreError> {
    log::trace!("fetching {key}");
    let query = "SELECT value FROM potency WHERE key = :key";
    let mut statement = lock.prepare(query)?;
    statement.bind((":key", key))?;
    match statement.next()? {
        sqlite::State::Row => {
            let string_value = statement.read::<String, _>("value")?;
            let value: serde_json::Value = serde_json::from_str(&string_value)?;
            Ok(Some(value))
        }
        sqlite::State::Done => Ok(None),
    }
}

async fn store_value(
    lock: &mut async_lock::MutexGuard<'_, sqlite::Connection>,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), StoreError> {
    // UNWRAP: safe because `Value` always serializes.
    let serialized = serde_json::to_string(value).unwrap();
    log::trace!("storing key {key}: {serialized}");
    let query = "INSERT OR REPLACE INTO potency (key, value) VALUES (:key, :value)";
    let mut statement = lock.prepare(query)?;
    statement.bind(&[(":key", key), (":value", serialized.as_str())][..])?;
    let _ = statement.next()?;
    Ok(())
}

async fn delete_value(
    lock: &mut async_lock::MutexGuard<'_, sqlite::Connection>,
    key: &str,
) -> Result<(), StoreError> {
    let mut statement = lock.prepare("DELETE FROM potency WHERE key = :key")?;
    statement.bind((":key", key))?;
    let _ = statement.next()?;
    Ok(())
}

// ============================================================================
// Global store for `potency-macros`
// ============================================================================

static GLOBAL_STORE: std::sync::OnceLock<Store> = std::sync::OnceLock::new();

/// Install the process-global [`Store`] used by `potency-macros`.
///
/// One-shot: subsequent calls return [`AlreadyInstalled`].
pub fn install_global_store(store: Store) -> Result<&'static Store, AlreadyInstalled> {
    GLOBAL_STORE.set(store).map_err(|_| AlreadyInstalled)?;
    Ok(GLOBAL_STORE.get().unwrap())
}

/// Read the global store, if installed.
pub fn global_store() -> Option<&'static Store> {
    GLOBAL_STORE.get()
}

/// Returned by [`install_global_store`] when a store was already installed.
#[derive(Debug)]
pub struct AlreadyInstalled;

impl std::fmt::Display for AlreadyInstalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "potency: global store already installed")
    }
}

impl std::error::Error for AlreadyInstalled {}

// ============================================================================
// Effect builder (durable side-effects)
// ============================================================================

/// A durable side-effecting operation.
///
/// See the module-level docs in [`effect`] for the contract.
#[allow(clippy::type_complexity)]
pub trait Effect {
    type Staging;
    type Manifest: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + 'static;
    type Error;

    fn fresh_staging<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Staging, Self::Error>> + 'a>>;
    fn produce<'a>(
        &'a self,
        staging: &'a Self::Staging,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Manifest, Self::Error>> + 'a>>;
    fn commit<'a>(
        &'a self,
        staging: &'a Self::Staging,
        manifest: &'a Self::Manifest,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + 'a>>;
    fn verify<'a>(
        &'a self,
        manifest: &'a Self::Manifest,
    ) -> Pin<Box<dyn Future<Output = Result<bool, Self::Error>> + 'a>>;
}

/// Error returned by [`EffectBuilder::run`].
///
/// Effect errors are flattened into [`StoreError`] via the user-supplied
/// `From<effect::Error> for StoreError`. The original effect error is not
/// retained.
#[derive(Debug)]
pub enum EffectError {
    /// An error from the backing [`Store`].
    Store(StoreError),
}

impl std::fmt::Display for EffectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EffectError::Store(e) => write!(f, "store error: {e}"),
        }
    }
}

impl std::error::Error for EffectError {}

impl From<StoreError> for EffectError {
    fn from(e: StoreError) -> Self {
        EffectError::Store(e)
    }
}

pub struct EffectBuilder<'a, E> {
    store: &'a Store,
    key: Vec<String>,
    effect: E,
}

impl<'a, E> EffectBuilder<'a, E> {
    pub fn param<T: AsKey>(mut self, input: T) -> Self {
        self.key.push(input.as_key());
        self
    }

    pub fn namespace(mut self, ns: impl AsRef<str>) -> Self {
        self.key.push(ns.as_ref().to_string());
        self
    }
}

impl<E, Err> EffectBuilder<'_, E>
where
    E: Effect<Error = Err>,
    Err: Into<StoreError>,
{
    pub async fn run(self) -> Result<E::Manifest, EffectError> {
        let Self { store, key, effect } = self;
        let full_key = key.join(",");
        let mut lock = store.inner.lock().await;

        // 1. Check the cache.
        let cached: Option<serde_json::Value> = fetch_value(&mut lock, &full_key)
            .await
            .map_err(EffectError::Store)?;
        if let Some(json_value) = cached {
            let manifest: E::Manifest = serde_json::from_value(json_value)
                .map_err(|e| EffectError::Store(StoreError::Json { source: e }))?;
            if effect
                .verify(&manifest)
                .await
                .map_err(|e| EffectError::Store(e.into()))?
            {
                log::trace!("{full_key:?} effect cache hit (verified)");
                return Ok(manifest);
            }
            log::trace!("{full_key:?} effect cache stale; invalidating");
            delete_value(&mut lock, &full_key)
                .await
                .map_err(EffectError::Store)?;
        }

        // 2. Miss (or invalidated): stage -> produce -> commit -> record.
        log::trace!("{full_key:?} effect computing");
        let staging = effect
            .fresh_staging(&full_key)
            .await
            .map_err(|e| EffectError::Store(e.into()))?;
        let manifest = effect
            .produce(&staging)
            .await
            .map_err(|e| EffectError::Store(e.into()))?;
        effect
            .commit(&staging, &manifest)
            .await
            .map_err(|e| EffectError::Store(e.into()))?;

        // 3. Only now record the manifest: entry exists iff committed.
        let json_value = serde_json::to_value(manifest.clone())
            .map_err(|e| EffectError::Store(StoreError::Json { source: e }))?;
        store_value(&mut lock, &full_key, &json_value)
            .await
            .map_err(EffectError::Store)?;
        Ok(manifest)
    }
}
// (debug tests removed)
