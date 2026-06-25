//! # `potency` tutorial
//!
//! A walkthrough of `potency`, from "what problem does it solve" to a real
//! durable side-effect on disk. Each section has a runnable example; the whole
//! file is exercised by `cargo test --doc`.
//!
//! ## 1. Concepts
//!
//! A *durable* computation is one whose result is worth remembering. Network
//! calls, expensive renders, queries against slow services — anything that
//! costs time, money, or both, and that you'd rather not repeat on retry.
//!
//! `potency` remembers results by *key*:
//!
//! - A **namespace** scopes the work (e.g. `"weather"`, `"render-job"`).
//! - **Parameters** describe the inputs (e.g. a city id, a config hash).
//!
//! When you ask `potency` to run a piece of work, it builds the key, looks in
//! the configured [store][crate::Store] for a cached result, and:
//!
//! - **On a hit**, returns the cached value. No work is done.
//! - **On a miss**, runs the function, stores its result under the key, and
//!   returns it.
//!
//! That contract is the whole library. Everything else — sync vs async,
//! in-memory vs SQLite, plain values vs side-effects — is variation on top.
//!
//! ## 2. Multi-color support (sync and async)
//!
//! `potency` works with both sync and async functions. You choose the
//! entry-point that matches your work:
//!
//! - [`crate::Store::entry`] for a plain `fn(...) -> T`.
//! - [`crate::Store::entry_async`] for an `async fn(...) -> impl Future<Output = T>`.
//!
//! Both produce the same [`crate::Builder`], with the same keying and caching
//! behavior.
//!
//! > **The `potency` API itself is always async.** Every builder returns a
//! > future that must be `.await`ed, even when the work you're wrapping is a
//! > plain sync function. Multi-color describes the *work*, not the runtime.
//!
//! ```rust
//! # async fn doc() {
//! use potency::{cpu_store::CpuStore, Store};
//!
//! // Sync work, async API.
//! fn add(a: u32, b: u32) -> Result<u32, potency::json::Error> {
//!     Ok(a + b)
//! }
//!
//! let store = Store::new(CpuStore::new());
//! let sum = store.entry(add).param(2u32).param(3u32).run().await.unwrap();
//! assert_eq!(sum, 5);
//! # }
//! ```
//!
//! ## 3. Quickstart
//!
//! The smallest useful program: cache the result of a three-input async
//! function.
//!
//! ```rust
//! # async fn doc() {
//! use potency::{cpu_store::CpuStore, Store};
//!
//! async fn three(a: u32, b: u32, c: u32) -> Result<u32, potency::json::Error> {
//!     Ok(a + b + c)
//! }
//!
//! let store = Store::new(CpuStore::new());
//!
//! let n = store
//!     .namespace("quickstart")
//!     .entry_async(three)
//!     .param(1u32)
//!     .param(2u32)
//!     .param(3u32)
//!     .run()
//!     .await
//!     .unwrap();
//!
//! assert_eq!(n, 6);
//!
//! // Second call hits the cache; the function body does not run again.
//! let n = store
//!     .namespace("quickstart")
//!     .entry_async(three)
//!     .param(1u32)
//!     .param(2u32)
//!     .param(3u32)
//!     .run()
//!     .await
//!     .unwrap();
//! assert_eq!(n, 6);
//! # }
//! ```
//!
//! Note the second block: same namespace, same params, same return value, no
//! recomputation. That's `potency` doing its job.
//!
//! ## 4. Namespaces & keys
//!
//! A cache key is the namespace joined with the `.param(...)` arguments, in
//! the order you supplied them. Two entries with the same joined key share a
//! cache slot; entries with different keys are independent.
//!
//! Use namespaces to group related work, and `.param(...)` for inputs that
//! should *change* the answer.
//!
//! ```rust
//! # async fn doc() {
//! use potency::{cpu_store::CpuStore, Store};
//!
//! async fn greet(name: String) -> Result<String, potency::json::Error> {
//!     Ok(format!("hello, {name}"))
//! }
//!
//! let store = Store::new(CpuStore::new());
//!
//! let a = store.namespace("greet").entry_async(greet).param("alice".to_string()).run().await.unwrap();
//! let b = store.namespace("greet").entry_async(greet).param("bob".to_string()).run().await.unwrap();
//!
//! assert_eq!(a, "hello, alice");
//! assert_eq!(b, "hello, bob");
//! # }
//! ```
//!
//! The keys here are roughly `"greet,alice"` and `"greet,bob"` — distinct,
//! cached independently.
//!
//! ## 5. Custom key types
//!
//! Anything implementing [`AsKey`][crate::AsKey] can be passed to `.param`.
//! The crate provides impls for primitives, `String`, `&str`, `Vec<T>`,
//! arrays, slices, and tuples up to 12 elements. For domain types, write a
//! small `impl` so the type flows into keys without an extra `.to_string()` at
//! every call site.
//!
//! ```rust
//! use potency::AsKey;
//!
//! #[derive(Clone)]
//! struct UserId(u64);
//!
//! impl AsKey for UserId {
//!     fn as_key(&self) -> String {
//!         format!("user:{}", self.0)
//!     }
//! }
//!
//! let id = UserId(42);
//! assert_eq!(id.as_key(), "user:42");
//! ```
//!
//! ## 6. Backing stores
//!
//! The `Store<S>` is generic over a backing store. The whole library is
//! runtime-agnostic — the choice of store is independent of the choice of
//! sync vs async for the work.
//!
//! For tests and small in-process use:
//!
//! ```rust
//! # async fn doc() {
//! use potency::{cpu_store::CpuStore, Store};
//!
//! let store = Store::new(CpuStore::new());
//! # let _ = store;
//! # }
//! ```
//!
//! For persistence across runs, use the SQLite store. (`Cargo.toml`: enable
//! the `sqlite-store` feature.)
//!
//! ```rust,no_run
//! # async fn doc() -> Result<(), potency::sqlite_store::Error> {
//! use potency::{sqlite_store::SqliteStore, Store};
//!
//! let store = Store::new(SqliteStore::open("state.db").await?);
//! # Ok(())
//! # }
//! ```
//!
//! The two are interchangeable at every call site — only `Store::new` differs.
//!
//! ## 7. Durable side-effects (`Effect`)
//!
//! Caching a return value is fine when the value *is* the product. But what
//! about work whose product is *external state* — files on disk, rows in a
//! remote database, a record in a third-party service? Re-running the function
//! doesn't undo the side effect, so "did we run this before?" is the wrong
//! question; the right one is "is the side effect still in place?"
//!
//! [`Effect`][crate::Effect] models exactly that. The crate ships
//! [`fs_effect`][crate::effect::fs_effect] for the common "produce a directory
//! of files" case. It writes to a staging directory, then atomically renames
//! staging onto the final output. On replay, the cached `Manifest` is checked
//! against the filesystem; if the output is still there, the work is skipped.
//!
//! ```rust,no_run
//! # async fn doc() -> Result<(), potency::EffectError<potency::json::Error, potency::effect::FsEffectError<std::io::Error>>> {
//! use std::path::PathBuf;
//! use potency::{cpu_store::CpuStore, effect::fs_effect, Store};
//!
//! async fn render_frames(staging: PathBuf) -> Result<u64, std::io::Error> {
//!     // Pretend we just rendered 3 frames into `staging`.
//!     for i in 0..3 {
//!         std::fs::write(staging.join(format!("{i:03}.png")), b"frame").unwrap();
//!     }
//!     Ok(3)
//! }
//!
//! let store = Store::new(CpuStore::new());
//! let output = std::env::temp_dir().join("potency-doc-out");
//!
//! // First run: produces 3 frames.
//! let m = store
//!     .namespace("render")
//!     .effect(fs_effect(&output, render_frames))
//!     .param("config-v1")
//!     .run()
//!     .await?;
//! assert_eq!(m.file_count, 3);
//!
//! // Second run: cache hit (output is still on disk) — no work done.
//! let m = store
//!     .namespace("render")
//!     .effect(fs_effect(&output, render_frames))
//!     .param("config-v1")
//!     .run()
//!     .await?;
//! assert_eq!(m.file_count, 3);
//!
//! # let _ = std::fs::remove_dir_all(&output);
//! # Ok(())
//! # }
//! ```
//!
//! Change the param (`"config-v2"`), delete the output dir between runs, or
//! corrupt the output — any of those will trigger a re-produce.
//!
//! ## 8. When *not* to use `potency`
//!
//! - **Pure, cheap functions** — there's nothing to cache; just call them.
//! - **Work that must run every time** — telemetry, real-time data, anything
//!   where stale results are wrong.
//! - **Side-effects without verification hooks** — `Effect` needs a way to
//!   confirm the external state is still in place. If you can't write one,
//!   caching the *call* (with [`entry_async`][crate::Store::entry_async]) is
//!   still useful, but you lose the "did the side effect actually happen?"
//!   guarantee.
//!
//! ## 9. Where to look next
//!
//! - [`crate::Store`] — the entry point.
//! - [`crate::Builder`] — composing a durable call.
//! - [`crate::Effect`] / [`crate::effect::fs_effect`] — durable side-effects.
//! - [`crate::AsKey`] — turning parameters into keys.