# potency

`potency` is a bare-bones durability and synchronization library for writing
idempotent processes without thinking too hard.

For background on durability: <https://flawless.dev/docs/>

The rough idea: the results of "expensive" and fallible processes are cached
under a key derived from a namespace and input parameters. Before running the
work, the cache is queried; on a hit the result is read back instead of
recomputed.

Storage is SQLite-backed: pass `":memory:"` for an in-process store or a file
path for a persistent one. `potency` supports **multi-color** functions —
both sync (`fn -> T`) and async (`async fn -> impl Future<Output = T>`).

> **The `potency` API itself is always async.** Every builder returns a
> future that must be `.await`ed, even when the work you're wrapping is a
> plain sync function. Multi-color describes the *work*, not the runtime.

## Quickstart

```rust,no_run
# async fn doc() -> Result<(), potency::StoreError> {
use potency::Store;

async fn three(a: u32, b: u32, c: u32) -> Result<u32, potency::StoreError> {
    Ok(a + b + c)
}

let store = Store::in_memory().await?;
let n = store
    .entry_async(three)
    .param(1u32).param(2u32).param(3u32)
    .run()
    .await?;
assert_eq!(n, 6);
# Ok(())
# }
```

## Learn more

The full tutorial — namespaces, keying, durable side-effects, and "when not
to use this" — lives in the `tutorial` module of the crate docs:

```sh
cargo doc --open
```

Or browse the source at
[`crates/potency/src/tutorial.rs`](crates/potency/src/tutorial.rs).

## Optional: `#[durable]` macro

`potency-macros` is an optional companion crate that provides a
`#[durable]` attribute. It leaves your original function untouched and
emits a `durable_{name}` wrapper that runs it through the global `potency`
store:

```rust,ignore
use potency::{install_global_store, Store};
use potency_macros::durable;

#[durable(namespace = "users")]
async fn fetch_user(id: u64) -> Result<User, MyError> { /* ... */ }

# async fn main_ish() -> Result<(), potency::StoreError> {
install_global_store(Store::in_memory().await?).unwrap();
let user = durable_fetch_user(42).await?;
# Ok(())
# }
```

## Goals

- replace bespoke persistence and idempotency processes with `potency` + your
  raw operations
- cache/storage: SQLite (`":memory:"` or file path)
- [ ] replicate / sync storages
- multi-color support
  - [x] sync (`Store::entry`)
  - [x] async (`Store::entry_async`)
- [ ] easy key generation (the `potency-macros` crate is a start)

## License

Dual-licensed under MIT or Apache 2.0, at your option.