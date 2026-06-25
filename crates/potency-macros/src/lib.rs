//! Procedural macros for `potency`.
//!
//! See the [`durable`] attribute macro.

use proc_macro::TokenStream;

mod expand;

/// Mark a function as durable.
///
/// `#[durable]` or `#[durable(namespace = "my-namespace")]` (the `namespace`
/// argument is optional; if omitted, the function's identifier is used).
///
/// Generates two functions:
///
/// - `{name}` — emitted verbatim from the input tokens.
/// - `durable_{name}` — an async wrapper that runs the original through the
///   process-global `potency::Store` registered via
///   `potency::install_global_store`.
///
/// The wrapper is always `async fn`. It uses `Store::entry` when the
/// original was sync and `Store::entry_async` when the original was
/// `async`. Visibility is mirrored verbatim from the original.
///
/// See the [`potency` tutorial](https://docs.rs/potency) for usage.
///
/// # Example
///
/// ```rust,ignore
/// use potency::{install_global_store, Store, StoreError};
/// use potency_macros::durable;
///
/// #[durable(namespace = "users")]
/// async fn fetch_user(id: u64) -> Result<String, StoreError> {
///     Ok(format!("user-{id}"))
/// }
///
/// # async fn doc() -> Result<(), StoreError> {
/// install_global_store(Store::in_memory().await?).unwrap();
/// let user = durable_fetch_user(7).await?;
/// assert_eq!(user, "user-7");
/// # Ok(())
/// # }
/// ```
#[proc_macro_attribute]
pub fn durable(attr: TokenStream, input: TokenStream) -> TokenStream {
    expand::durable(attr.into(), input.into())
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}