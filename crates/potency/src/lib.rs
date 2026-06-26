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
    /// Run the cached call.
    ///
    /// The cache key is `key.join(",")` — i.e. the namespace segments (added
    /// via [`Store::namespace`] on the originating [`Store`]) followed by the
    /// `.param(...)` arguments. Two entries share a cache slot iff their
    /// joined keys are equal.
    ///
    /// **Nesting.** The user's function runs *without* the SQLite connection
    /// lock held, so a durable call may freely invoke other durable calls
    /// (including recursively) without deadlocking.
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
    ///
    /// **Locking.** The SQLite connection is locked only for the brief
    /// fetch/store round-trips. The user's function `f` runs *without* the
    /// lock held, so a durable call may invoke other durable calls (or
    /// recurse) without deadlocking.
    ///
    /// **Concurrent same-key misses.** Two tasks that miss the same key
    /// concurrently will both compute; the second writer, on re-acquiring
    /// the lock, observes the first writer's stored value and returns it
    /// instead of overwriting. The cost is one redundant compute per pair;
    /// the observable result is the same for any deterministic function.
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
            // Step 1: brief lock to fetch.
            let maybe_value: Option<serde_json::Value> = {
                let mut lock = self.inner.lock().await;
                fetch_value(&mut lock, &full_key).await?
            };
            if let Some(json_value) = maybe_value {
                log::trace!("{full_key:?} is cached, returning cache hit");
                let output: O = serde_json::from_value(json_value)?;
                return Ok(output);
            }
            log::trace!("{full_key:?} is not cached, computing the value");

            // Step 2: user work — NO LOCK held. This is what makes
            // durable-in-durable and recursive durable calls safe.
            let output = f().await.map_err(Into::into)?;

            // Step 3: brief lock to store, with re-check for racing writers.
            let mut lock = self.inner.lock().await;
            if let Some(existing) = fetch_value(&mut lock, &full_key).await? {
                log::trace!("{full_key:?} racing writer detected, using their value");
                let output: O = serde_json::from_value(existing)?;
                return Ok(output);
            }
            let json_value = serde_json::to_value(output.clone())?;
            store_value(&mut lock, &full_key, &json_value).await?;
            Ok(output)
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
            // Inherit the Store's accumulated namespace segments so the
            // resulting cache key reflects both the namespace and the
            // params added via `.param(...)`.
            key: self.key.clone(),
            input: (),
            fn_pair,
        }
    }

    /// Begin an async entry.
    pub fn entry_async<F>(&self, f: F) -> Builder<'_, (), F, Async> {
        Builder {
            store: self,
            key: self.key.clone(),
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
    /// Run the durable effect protocol.
    ///
    /// - **Hit + valid:** returns the cached manifest, performing no work.
    /// - **Hit + stale:** deletes the entry and re-runs.
    /// - **Miss:** stages, produces, commits, then records the manifest.
    ///
    /// **Nesting.** The `fresh_staging` / `produce` / `commit` phase runs
    /// *without* the SQLite connection lock held, so an `Effect`'s
    /// filesystem work can include nested durable calls (or other
    /// effects) without deadlocking. Note that two `Effect` runs sharing
    /// the same cache key from *different tasks* would still race on the
    /// staging directory; this design supports nesting in a single task,
    /// not concurrent same-key runs across tasks.
    pub async fn run(self) -> Result<E::Manifest, EffectError> {
        let Self { store, key, effect } = self;
        let full_key = key.join(",");

        // Step 1: brief lock to fetch.
        let cached: Option<serde_json::Value> = {
            let mut lock = store.inner.lock().await;
            fetch_value(&mut lock, &full_key)
                .await
                .map_err(EffectError::Store)?
        };

        if let Some(json_value) = cached {
            let manifest: E::Manifest = serde_json::from_value(json_value)
                .map_err(|e| EffectError::Store(StoreError::Json { source: e }))?;

            // Step 2: verify outside the lock — verify is filesystem-only.
            if effect
                .verify(&manifest)
                .await
                .map_err(|e| EffectError::Store(e.into()))?
            {
                log::trace!("{full_key:?} effect cache hit (verified)");
                return Ok(manifest);
            }
            // Stale: brief lock to delete the entry.
            log::trace!("{full_key:?} effect cache stale; invalidating");
            let mut lock = store.inner.lock().await;
            delete_value(&mut lock, &full_key)
                .await
                .map_err(EffectError::Store)?;
        }

        // Step 3: filesystem work — NO LOCK held. This allows effects to
        // themselves be invoked from inside another durable call without
        // deadlocking on the SQLite connection.
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

        // Step 4: brief lock to store the manifest.
        let mut lock = store.inner.lock().await;
        let json_value = serde_json::to_value(manifest.clone())
            .map_err(|e| EffectError::Store(StoreError::Json { source: e }))?;
        store_value(&mut lock, &full_key, &json_value)
            .await
            .map_err(EffectError::Store)?;
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    //! Tests for nesting and reentrancy. These exercise the lock-dropping
    //! changes in `fetch_or_else` and `EffectBuilder::run`.

    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    use super::*;

    /// A simple counter so tests can assert "the function body ran N times."
    #[derive(Clone, Default)]
    struct Counter(Arc<AtomicU32>);

    impl Counter {
        fn bump(&self) -> u32 {
            self.0.fetch_add(1, Ordering::SeqCst) + 1
        }
        fn get(&self) -> u32 {
            self.0.load(Ordering::SeqCst)
        }
    }

    /// Sync entry nested inside an async entry. Pre-lock-drop this would
    /// deadlock the SQLite connection.
    #[test]
    fn nesting_sync_inside_async() {
        smol::block_on(async {
            let store = Store::in_memory().await.unwrap();
            let inner_calls = Counter::default();
            let outer_calls = Counter::default();

            // First call: outer miss + inner miss. Both bodies run.
            {
                let store_for_call = store.clone();
                let store_for_closure = store_for_call.clone();
                let outer_calls_clone = outer_calls.clone();
                let inner_calls_clone = inner_calls.clone();
                let outer = move |x: u32| {
                    let outer_calls = outer_calls_clone.clone();
                    let inner_calls = inner_calls_clone.clone();
                    let store = store_for_closure.clone();
                    async move {
                        let _ = outer_calls.bump();
                        // Use a distinct namespace for the inner entry so its
                        // cache key doesn't collide with the outer's. Two
                        // entries sharing the same key would race in the
                        // re-check step.
                        let y = store
                            .namespace("inner")
                            .entry(move |x: u32| -> Result<u32, StoreError> {
                                let _ = inner_calls.bump();
                                Ok(x * 2)
                            })
                            .param(x)
                            .run()
                            .await?;
                        Ok::<u32, StoreError>(y + 1)
                    }
                };
                let n = store_for_call
                    .entry_async(outer)
                    .param(7u32)
                    .run()
                    .await
                    .unwrap();
                assert_eq!(n, 15); // (7 * 2) + 1
                assert_eq!(outer_calls.get(), 1);
                assert_eq!(inner_calls.get(), 1);
            }

            // Second call: outer hit, no inner body.
            {
                let store_for_call = store.clone();
                let store_for_closure = store_for_call.clone();
                let outer_calls_clone = outer_calls.clone();
                let inner_calls_clone = inner_calls.clone();
                let outer = move |x: u32| {
                    let outer_calls = outer_calls_clone.clone();
                    let inner_calls = inner_calls_clone.clone();
                    let store = store_for_closure.clone();
                    async move {
                        let _ = outer_calls.bump();
                        let y = store
                            .namespace("inner")
                            .entry(move |x: u32| -> Result<u32, StoreError> {
                                let _ = inner_calls.bump();
                                Ok(x * 2)
                            })
                            .param(x)
                            .run()
                            .await?;
                        Ok::<u32, StoreError>(y + 1)
                    }
                };
                let n = store_for_call
                    .entry_async(outer)
                    .param(7u32)
                    .run()
                    .await
                    .unwrap();
                assert_eq!(n, 15);
                assert_eq!(outer_calls.get(), 1, "outer body should not re-run");
                assert_eq!(inner_calls.get(), 1, "inner body should not re-run");
            }
        });
    }

    /// Async entry nested inside an async entry.
    #[test]
    fn nesting_async_inside_async() {
        smol::block_on(async {
            let store = Store::in_memory().await.unwrap();
            let outer_calls = Counter::default();

            let store_for_call = store.clone();
            let store_for_closure = store_for_call.clone();
            let outer_calls_clone = outer_calls.clone();
            let outer = move |x: u32| {
                let outer_calls = outer_calls_clone.clone();
                let store = store_for_closure.clone();
                async move {
                    let _ = outer_calls.bump();
                    let y = store
                        .namespace("inner")
                        .entry_async(|n: u32| async move { Ok::<u32, StoreError>(n * 3) })
                        .param(x)
                        .run()
                        .await?;
                    Ok::<u32, StoreError>(y + 5)
                }
            };
            let n = store_for_call
                .entry_async(outer)
                .param(4u32)
                .run()
                .await
                .unwrap();
            assert_eq!(n, 17); // (4 * 3) + 5
            assert_eq!(outer_calls.get(), 1);
        });
    }

    /// Recursive durable call: an async entry's body contains another async
    /// entry. Pre lock-drop this would deadlock; with the lock-drop fix
    /// both calls run.
    #[test]
    fn recursive_durable_call() {
        smol::block_on(async {
            let store = Store::in_memory().await.unwrap();
            let outer_calls = Counter::default();

            // Plain recursive factorial (no potency).
            fn plain_fact(n: u32) -> u32 {
                if n <= 1 {
                    1
                } else {
                    n * plain_fact(n - 1)
                }
            }

            // First call.
            {
                let store_for_call = store.clone();
                let store_for_closure = store_for_call.clone();
                let outer_calls_clone = outer_calls.clone();
                let outer = move |n: u32| {
                    let outer_calls = outer_calls_clone.clone();
                    let store = store_for_closure.clone();
                    async move {
                        let _ = outer_calls.bump();
                        let f = plain_fact(n);
                        let g = store
                            .namespace("inner-fact")
                            .entry_async(|m: u32| async move { Ok::<u32, StoreError>(m * 100) })
                            .param(n)
                            .run()
                            .await?;
                        Ok::<u32, StoreError>(f + g)
                    }
                };
                let n = store_for_call
                    .entry_async(outer)
                    .param(5u32)
                    .run()
                    .await
                    .unwrap();
                assert_eq!(n, 620); // 5! + 5*100
            }

            // Second call (cache hit on outer).
            {
                let store_for_call = store.clone();
                let store_for_closure = store_for_call.clone();
                let outer_calls_clone = outer_calls.clone();
                let outer = move |n: u32| {
                    let outer_calls = outer_calls_clone.clone();
                    let store = store_for_closure.clone();
                    async move {
                        let _ = outer_calls.bump();
                        let f = plain_fact(n);
                        let g = store
                            .namespace("inner-fact")
                            .entry_async(|m: u32| async move { Ok::<u32, StoreError>(m * 100) })
                            .param(n)
                            .run()
                            .await?;
                        Ok::<u32, StoreError>(f + g)
                    }
                };
                let n = store_for_call
                    .entry_async(outer)
                    .param(5u32)
                    .run()
                    .await
                    .unwrap();
                assert_eq!(n, 620);
            }
            assert_eq!(
                outer_calls.get(),
                1,
                "outer body should not re-run on cache hit"
            );
        });
    }

    /// Two concurrent tasks hitting the same key. Both miss, both compute;
    /// one wins the store race, the other observes the winner's value via
    /// re-check. The observable result is consistent across both tasks.
    #[test]
    fn concurrent_same_key_consistent() {
        use std::sync::Barrier;

        let store = smol::block_on(Store::in_memory()).unwrap();
        let calls = Counter::default();

        // Two threads, each running its own smol block_on. They share the
        // store (it's Clone — just an Arc<Mutex<Connection>>).
        let barrier = Arc::new(Barrier::new(2));
        let s1 = store.clone();
        let s2 = store.clone();
        let c1 = calls.clone();
        let c2 = calls.clone();
        let b1 = barrier.clone();
        let b2 = barrier.clone();
        let h1 = std::thread::spawn(move || {
            smol::block_on(async move {
                b1.wait();
                s1.entry_async(move |x: u32| {
                    let calls = c1.clone();
                    async move {
                        smol::Timer::after(std::time::Duration::from_millis(5)).await;
                        let _ = calls.bump();
                        Ok::<u32, StoreError>(x * 10)
                    }
                })
                .param(3u32)
                .run()
                .await
            })
        });
        let h2 = std::thread::spawn(move || {
            smol::block_on(async move {
                b2.wait();
                s2.entry_async(move |x: u32| {
                    let calls = c2.clone();
                    async move {
                        smol::Timer::after(std::time::Duration::from_millis(5)).await;
                        let _ = calls.bump();
                        Ok::<u32, StoreError>(x * 10)
                    }
                })
                .param(3u32)
                .run()
                .await
            })
        });
        let v1 = h1.join().unwrap().unwrap();
        let v2 = h2.join().unwrap().unwrap();

        // Both must observe the same value.
        assert_eq!(v1, v2);
        assert_eq!(v1, 30);

        // At most two compute passes; the re-check should not produce a
        // third.
        let n = calls.get();
        assert!((1..=2).contains(&n), "unexpected compute count {n}");
    }
}
// (debug tests removed)
