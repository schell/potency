//! Integration tests for `#[durable]`.

use std::sync::OnceLock;

use potency::{install_global_store, Store, StoreError};
use potency_macros::durable;

fn shared_store() -> &'static Store {
    static CELL: OnceLock<Store> = OnceLock::new();
    CELL.get_or_init(|| smol::block_on(async { Store::in_memory().await.unwrap() }))
}

/// Install the shared store once for the entire test process.
fn install_shared_store() {
    let store = shared_store();
    let _ = install_global_store(store.clone());
}

// ---------------------------------------------------------------------------
// Sync fragile
// ---------------------------------------------------------------------------

#[durable(namespace = "sync-default-ns")]
fn add(a: u32, b: u32) -> Result<u32, StoreError> {
    Ok(a + b)
}

#[durable]
fn sub(a: u32, b: u32) -> Result<u32, StoreError> {
    Ok(a - b)
}

#[test]
fn sync_durable_runs_through_potency() {
    install_shared_store();
    smol::block_on(async {
        let n = durable_add(2, 3).await.unwrap();
        assert_eq!(n, 5);
        // Second call hits the cache.
        let n = durable_add(2, 3).await.unwrap();
        assert_eq!(n, 5);
    });
}

#[test]
fn sync_default_namespace_uses_function_name() {
    install_shared_store();
    smol::block_on(async {
        let n = durable_sub(10, 4).await.unwrap();
        assert_eq!(n, 6);
    });
}

// ---------------------------------------------------------------------------
// Async fragile
// ---------------------------------------------------------------------------

#[durable(namespace = "async-durable-ns")]
async fn slow_add(a: u32, b: u32) -> Result<u32, StoreError> {
    smol::Timer::after(std::time::Duration::from_millis(5)).await;
    Ok(a + b)
}

#[test]
fn async_durable_runs_through_potency() {
    install_shared_store();
    smol::block_on(async {
        let n = durable_slow_add(7, 8).await.unwrap();
        assert_eq!(n, 15);
        let n = durable_slow_add(7, 8).await.unwrap();
        assert_eq!(n, 15);
    });
}

// ---------------------------------------------------------------------------
// Visibility mirroring (compile-time): both `original` and `durable_original`
// carry the same visibility. The fact that the test compiles is the assertion.
// ---------------------------------------------------------------------------

#[durable(namespace = "vis-test")]
pub(crate) fn pub_crate_fn(x: u32) -> Result<u32, StoreError> {
    Ok(x * 2)
}

#[test]
fn pub_crate_visibility_compiles_and_works() {
    install_shared_store();
    smol::block_on(async {
        let n = durable_pub_crate_fn(21).await.unwrap();
        assert_eq!(n, 42);
    });
}

// ---------------------------------------------------------------------------
// Original function still callable directly (untouched).
// ---------------------------------------------------------------------------

#[durable(namespace = "untouched-ns")]
fn untouched(x: u32) -> Result<u32, StoreError> {
    Ok(x + 100)
}

#[test]
fn original_is_emitted_verbatim() {
    install_shared_store();
    // Direct call — no caching layer.
    let n = untouched(1).unwrap();
    assert_eq!(n, 101);
    let n = untouched(2).unwrap();
    assert_eq!(n, 102);
}

// ---------------------------------------------------------------------------
// trybuild compile-fail tests would live here; we use plain assertions
// instead since `trybuild` isn't a dev-dep.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Nesting: `#[durable]` wrappers can call other `#[durable]` wrappers (or
// each other) without deadlocking. Pre lock-drop, this would deadlock on
// the SQLite connection mutex.
// ---------------------------------------------------------------------------

#[durable(namespace = "nest-inner")]
fn double(x: u32) -> Result<u32, StoreError> {
    Ok(x * 2)
}

#[durable(namespace = "nest-outer")]
async fn double_plus_one(x: u32) -> Result<u32, StoreError> {
    // Call another `#[durable]` wrapper from inside this one. Each
    // `#[durable]` carries its own namespace, so cache keys don't collide.
    let y = durable_double(x).await?;
    Ok(y + 1)
}

#[test]
fn macro_nested_durable_calls() {
    install_shared_store();
    smol::block_on(async {
        let n = durable_double_plus_one(10).await.unwrap();
        assert_eq!(n, 21); // (10 * 2) + 1

        // Second call: outer hits cache.
        let n = durable_double_plus_one(10).await.unwrap();
        assert_eq!(n, 21);
    });
}

#[durable(namespace = "nest-async-outer")]
async fn chain_step_a(x: u32) -> Result<u32, StoreError> {
    Ok(x + 1)
}

#[durable(namespace = "nest-async-mid")]
async fn chain_step_b(x: u32) -> Result<u32, StoreError> {
    let y = durable_chain_step_a(x).await?;
    Ok(y * 3)
}

#[durable(namespace = "nest-async-top")]
async fn chain_top(x: u32) -> Result<u32, StoreError> {
    let y = durable_chain_step_b(x).await?;
    Ok(y + 100)
}

#[test]
fn macro_deeply_nested_durable_calls() {
    install_shared_store();
    smol::block_on(async {
        // a(5) = 6, b(5) = 6 * 3 = 18, top(5) = 18 + 100 = 118.
        let n = durable_chain_top(5).await.unwrap();
        assert_eq!(n, 118);

        let n = durable_chain_top(5).await.unwrap();
        assert_eq!(n, 118);
    });
}
