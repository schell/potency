//! Implementation of the `#[durable]` attribute macro.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    parse::{Parse, ParseStream},
    parse2, FnArg, ItemFn, LitStr, Result, ReturnType, Signature, Token,
};

/// `#[durable]` or `#[durable(namespace = "...")]`.
pub(crate) struct DurableAttr {
    namespace: Option<LitStr>,
}

impl Parse for DurableAttr {
    fn parse(input: ParseStream) -> Result<Self> {
        if input.is_empty() {
            return Ok(Self { namespace: None });
        }
        let ident: syn::Ident = input.parse()?;
        if ident != "namespace" {
            return Err(syn::Error::new_spanned(
                ident,
                "#[durable] only accepts `namespace = \"...\"` (or no argument)",
            ));
        }
        let _eq: Token![=] = input.parse()?;
        let namespace: LitStr = input.parse()?;
        if !input.is_empty() {
            return Err(
                input.error("#[durable] only accepts a single `namespace = \"...\"` argument")
            );
        }
        Ok(Self {
            namespace: Some(namespace),
        })
    }
}

pub(crate) fn durable(attr: TokenStream2, input: TokenStream2) -> Result<TokenStream2> {
    let DurableAttr { namespace } = parse2::<DurableAttr>(attr)?;
    let fn_item: ItemFn = parse2::<ItemFn>(input)?;

    // Reject methods (anything with `self`).
    for arg in &fn_item.sig.inputs {
        if let FnArg::Receiver(receiver) = arg {
            return Err(syn::Error::new_spanned(
                receiver,
                "#[durable] does not yet support methods (functions taking `self`); \
                 apply it to a free function instead",
            ));
        }
    }

    let original_ident = &fn_item.sig.ident;
    let original_vis = &fn_item.vis;

    // Namespace: explicit argument, or default to function name.
    let namespace = match namespace {
        Some(lit) => lit.value(),
        None => original_ident.to_string(),
    };
    let namespace_lit = LitStr::new(&namespace, fn_item.sig.ident.span());

    // Detect asyncness of the original.
    let original_is_async = fn_item.sig.asyncness.is_some();

    // Build the param chain (one `.param(arg)` per input).
    let param_chain = build_param_chain(&fn_item.sig);

    // Choose the entry point based on the original's color.
    let entry_method = if original_is_async {
        quote! { .entry_async }
    } else {
        quote! { .entry }
    };

    // The wrapper's signature inputs + return type (preserved from the
    // original), without the `async` keyword (we emit it explicitly).
    let wrapper_inputs = &fn_item.sig.inputs;
    let wrapper_output = match &fn_item.sig.output {
        ReturnType::Default => quote! {},
        ReturnType::Type(arrow, ty) => quote! { #arrow #ty },
    };

    // The wrapper body:
    //   ::potency::global_store()
    //       .expect("...")
    //       .namespace(<ns>)
    //       .<entry_or_entry_async>(<orig_ident>)
    //       .param(a1).param(a2)...
    //       .run()
    //       .await
    let wrapper_ident = format_ident!("durable_{}", original_ident);
    let wrapper_body = quote! {
        ::potency::global_store()
            .expect(
                "potency: no global store installed; call \
                 ::potency::install_global_store(...) at startup before invoking \
                 durable wrappers",
            )
            .namespace(#namespace_lit)
            #entry_method(#original_ident)
            #param_chain
            .run()
            .await
    };

    let wrapper = quote! {
        #original_vis async fn #wrapper_ident(#wrapper_inputs) #wrapper_output {
            #wrapper_body
        }
    };

    // Re-emit the original function verbatim, then the wrapper.
    Ok(quote! {
        #fn_item
        #wrapper
    })
}

/// Build the `.param(...)` chain for the wrapper body.
fn build_param_chain(sig: &Signature) -> TokenStream2 {
    let mut tokens = TokenStream2::new();
    for arg in &sig.inputs {
        if let FnArg::Typed(pat_type) = arg {
            if let syn::Pat::Ident(pat_ident) = &*pat_type.pat {
                let name = &pat_ident.ident;
                tokens.extend(quote! { .param(#name) });
            }
        }
    }
    tokens
}
