# potency

`potency` is a bare-bones durability and synchronization library for writing
idempotent processes without thinking too hard.

For background on durability: <https://flawless.dev/docs/>

The rough idea: the results of "expensive" and fallible processes are cached
under a key derived from a namespace and input parameters. Before running the
work, the cache is queried; on a hit the result is read back instead of
recomputed.

`potency` abstracts over multiple persistence layers via `Store<S>` and
supports **multi-color** functions — both sync (`fn -> T`) and async
(`async fn -> impl Future<Output = T>`).

> **The `potency` API itself is always async.** Every builder returns a
> future that must be `.await`ed, even when the work you're wrapping is a
> plain sync function. Multi-color describes the *work*, not the runtime.

## Quickstart

```rust,no_run
# async fn doc() {
use potency::{cpu_store::CpuStore, Store};

async fn three(a: u32, b: u32, c: u32) -> Result<u32, potency::json::Error> {
    Ok(a + b + c)
}

let store = Store::new(CpuStore::new());
let n = store
    .entry_async(three)
    .param(1u32).param(2u32).param(3u32)
    .run()
    .await
    .unwrap();
assert_eq!(n, 6);
# }
```

## Learn more

The full tutorial — namespaces, keying, custom stores, durable side-effects,
and "when not to use this" — lives in the `tutorial` module of the crate docs:

```sh
cargo doc --open
```

Or browse the source at
[`crates/potency/src/tutorial.rs`](crates/potency/src/tutorial.rs).

## Goals

- replace bespoke persistence and idempotency processes with `potency` + your
  raw operations
- cache/storage support for
  - [x] in memory (`cpu_store`)
  - [x] sqlite (`sqlite_store`)
  - [ ] AWS DynamoDB
  - [ ] Postgres
- [ ] replicate / sync storages
- multi-color support
  - [x] sync (`Store::entry`)
  - [x] async (`Store::entry_async`)
- [ ] easy key generation

## License

Dual-licensed under MIT or Apache 2.0, at your option.