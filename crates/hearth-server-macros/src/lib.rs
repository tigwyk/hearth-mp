//! `#[hearth::room]`: turn an `impl Room for X` block into a Durable Object.
//!
//! The macro validates the room options, re-emits the impl, adds the
//! compile-time [`RoomConfig`] implementation and generates the
//! `#[worker::durable_object]` wrapper that delegates every event to
//! `hearth_server::worker::RoomObject<X>`.
//!
//! ```ignore
//! #[hearth::room(mode = "event", max_clients = 4, empty_grace_secs = 60)]
//! impl Room for CardMatch { /* ... */ }
//! ```

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2, TokenTree};
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::{Expr, ItemImpl, Lit, Meta, Token, Type};

/// Options accepted by `#[hearth::room(...)]`.
#[derive(Default)]
struct RoomArgs {
    mode: Option<(String, Span)>,
    max_clients: Option<(i64, Span)>,
    empty_grace_secs: Option<u64>,
}

#[proc_macro_attribute]
pub fn room(attr: TokenStream, item: TokenStream) -> TokenStream {
    expand(TokenStream2::from(attr), TokenStream2::from(item)).into()
}

fn expand(attr: TokenStream2, item: TokenStream2) -> TokenStream2 {
    match expand_impl(&attr, &item) {
        Ok(expanded) => expanded,
        Err(err) => {
            // Keep the user's item so the error is the only new diagnostic.
            let err = err.to_compile_error();
            quote!(#item #err)
        }
    }
}

fn expand_impl(attr: &TokenStream2, item: &TokenStream2) -> syn::Result<TokenStream2> {
    let item_impl: ItemImpl = syn::parse2(item.clone())?;
    let args = parse_args(attr)?;

    match args
        .mode
        .as_ref()
        .map(|(mode, _)| mode.as_str())
        .unwrap_or("event")
    {
        "event" => {}
        "ticked" => {
            return Err(syn::Error::new(
                args.mode.as_ref().expect("mode present").1,
                "ticked rooms arrive in Hearth v0.4; use mode = \"event\"",
            ));
        }
        "handoff" => {
            return Err(syn::Error::new(
                args.mode.as_ref().expect("mode present").1,
                "handoff rooms arrive in Hearth v0.5",
            ));
        }
        _ => {
            return Err(syn::Error::new(
                args.mode.as_ref().expect("mode present").1,
                "mode must be \"event\"",
            ));
        }
    }

    let Some((max_clients, max_clients_span)) = args.max_clients else {
        return Err(syn::Error::new(
            first_token_span(attr).unwrap_or_else(|| item_impl.span()),
            "missing max_clients; write #[hearth::room(mode = \"event\", max_clients = N)]",
        ));
    };
    if !(1..=255).contains(&max_clients) {
        return Err(syn::Error::new(
            max_clients_span,
            "max_clients must be 1..=255",
        ));
    }
    let max_clients = max_clients as u8;
    let empty_grace_secs = args.empty_grace_secs.unwrap_or(60);

    let room = self_ident(&item_impl)?;
    let self_ty = &item_impl.self_ty;
    let object = format_ident!("{room}Object");
    let wrapper = format_ident!("__hearth_{}_object", room.to_string().to_ascii_lowercase());
    Ok(quote! {
        #item_impl

        impl ::hearth::server::RoomConfig for #room {
            const MODE: ::hearth::server::Mode = ::hearth::server::Mode::Event;
            const MAX_CLIENTS: u8 = #max_clients;
            const EMPTY_GRACE: ::std::time::Duration =
                ::std::time::Duration::from_secs(#empty_grace_secs);
        }

        // `#[worker::durable_object]`'s wasm-bindgen struct-marker expansion
        // spells the crate as bare `wasm_bindgen` (wasm-bindgen 0.2.129), so
        // the wrapper lives in a hidden module that puts that name in scope.
        #[doc(hidden)]
        pub mod #wrapper {
            #[allow(unused_imports)]
            use super::*;
            #[allow(unused_imports)]
            use ::worker::wasm_bindgen as wasm_bindgen;

            #[::worker::durable_object]
            pub struct #object(::hearth::server::worker::RoomObject<#self_ty>);

            impl ::worker::DurableObject for #object {
            fn new(state: ::worker::State, env: ::worker::Env) -> Self {
                Self(::hearth::server::worker::RoomObject::<#room>::new(state, env))
            }

            async fn fetch(
                &self,
                req: ::worker::Request,
            ) -> ::worker::Result<::worker::Response> {
                self.0.fetch(req).await
            }

            async fn alarm(&self) -> ::worker::Result<::worker::Response> {
                self.0.alarm().await
            }

            async fn websocket_message(
                &self,
                ws: ::worker::WebSocket,
                message: ::worker::WebSocketIncomingMessage,
            ) -> ::worker::Result<()> {
                self.0.websocket_message(ws, message).await
            }

            async fn websocket_close(
                &self,
                ws: ::worker::WebSocket,
                code: usize,
                reason: ::std::string::String,
                was_clean: bool,
            ) -> ::worker::Result<()> {
                self.0.websocket_close(ws, code, reason, was_clean).await
            }

            async fn websocket_error(
                &self,
                ws: ::worker::WebSocket,
                error: ::worker::Error,
            ) -> ::worker::Result<()> {
                self.0.websocket_error(ws, error).await
            }
        }
    }

    pub use #wrapper::#object;
    })
}

fn parse_args(attr: &TokenStream2) -> syn::Result<RoomArgs> {
    let mut args = RoomArgs::default();
    if attr.is_empty() {
        return Ok(args);
    }
    let metas = syn::parse::Parser::parse2(
        syn::punctuated::Punctuated::<Meta, Token![,]>::parse_terminated,
        attr.clone(),
    )?;
    for meta in metas {
        let Some(name) = meta.path().get_ident().map(ToString::to_string) else {
            return Err(syn::Error::new_spanned(
                &meta,
                "unknown #[hearth::room] option",
            ));
        };
        match name.as_str() {
            "mode" => {
                let value = str_value(&meta, "mode")?;
                args.mode = Some((value.value(), value.span()));
            }
            "max_clients" => {
                let value = int_value(&meta, "max_clients")?;
                args.max_clients = Some((value.base10_parse::<i64>()?, value.span()));
            }
            "empty_grace_secs" => {
                let value = int_value(&meta, "empty_grace_secs")?;
                args.empty_grace_secs = Some(value.base10_parse::<u64>()?);
            }
            other => {
                return Err(syn::Error::new_spanned(
                    &meta,
                    format!("unknown #[hearth::room] option `{other}`"),
                ));
            }
        }
    }
    Ok(args)
}

fn str_value(meta: &Meta, name: &str) -> syn::Result<syn::LitStr> {
    let Meta::NameValue(name_value) = meta else {
        return Err(syn::Error::new_spanned(
            meta,
            format!("{name} needs a value, e.g. {name} = \"event\""),
        ));
    };
    let Expr::Lit(expr_lit) = &name_value.value else {
        return Err(syn::Error::new_spanned(
            meta,
            format!("{name} needs a string value, e.g. {name} = \"event\""),
        ));
    };
    let Lit::Str(value) = &expr_lit.lit else {
        return Err(syn::Error::new_spanned(
            meta,
            format!("{name} needs a string value, e.g. {name} = \"event\""),
        ));
    };
    Ok(value.clone())
}

fn int_value(meta: &Meta, name: &str) -> syn::Result<syn::LitInt> {
    let Meta::NameValue(name_value) = meta else {
        return Err(syn::Error::new_spanned(
            meta,
            format!("{name} needs an integer value, e.g. {name} = 60"),
        ));
    };
    let Expr::Lit(expr_lit) = &name_value.value else {
        return Err(syn::Error::new_spanned(
            meta,
            format!("{name} needs an integer value, e.g. {name} = 60"),
        ));
    };
    let Lit::Int(value) = &expr_lit.lit else {
        return Err(syn::Error::new_spanned(
            meta,
            format!("{name} needs an integer value, e.g. {name} = 60"),
        ));
    };
    Ok(value.clone())
}

fn self_ident(item_impl: &ItemImpl) -> syn::Result<syn::Ident> {
    if let Type::Path(type_path) = item_impl.self_ty.as_ref() {
        if let Some(segment) = type_path.path.segments.last() {
            if matches!(segment.arguments, syn::PathArguments::None) {
                return Ok(segment.ident.clone());
            }
        }
    }
    Err(syn::Error::new_spanned(
        &item_impl.self_ty,
        "#[hearth::room] needs a plain room type on `impl Room for X`",
    ))
}

fn first_token_span(tokens: &TokenStream2) -> Option<Span> {
    tokens
        .clone()
        .into_iter()
        .next()
        .map(|token: TokenTree| token.span())
}
