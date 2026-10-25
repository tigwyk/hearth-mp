//! Hearth attribute macros: `#[hearth::room]` and `#[hearth::service]`.
//!
//! Both take the `impl` block of the trait and add everything the Durable
//! Object adapter needs: the compile-time config, the typed [`Callable`]
//! descriptor and the `#[worker::durable_object]` wrapper the Worker exports.
//!
//! ```ignore
//! #[hearth::room(mode = "event", max_clients = 4, binding = "CARD_MATCH", rpc)]
//! impl Room for CardMatch { /* ... */ }
//!
//! #[hearth::service(binding = "STATS")]
//! impl Service for Stats { /* ... */ }
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
    reconnect_grace_secs: Option<u64>,
    binding: Option<(String, Span)>,
    rpc: Option<Span>,
}

#[proc_macro_attribute]
pub fn room(attr: TokenStream, item: TokenStream) -> TokenStream {
    expand_room(&TokenStream2::from(attr), &TokenStream2::from(item)).into()
}

#[proc_macro_attribute]
pub fn service(attr: TokenStream, item: TokenStream) -> TokenStream {
    expand_service(&TokenStream2::from(attr), &TokenStream2::from(item)).into()
}

fn error_with_item(item: &TokenStream2, err: syn::Error) -> TokenStream2 {
    // Keep the user's item so the error is the only new diagnostic.
    let err = err.to_compile_error();
    quote!(#item #err)
}

fn expand_room(attr: &TokenStream2, item: &TokenStream2) -> TokenStream2 {
    match expand_room_impl(attr, item) {
        Ok(expanded) => expanded,
        Err(err) => error_with_item(item, err),
    }
}

fn expand_room_impl(attr: &TokenStream2, item: &TokenStream2) -> syn::Result<TokenStream2> {
    let item_impl: ItemImpl = syn::parse2(item.clone())?;
    let args = parse_room_args(attr)?;

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
            ))
        }
        "handoff" => {
            return Err(syn::Error::new(
                args.mode.as_ref().expect("mode present").1,
                "handoff rooms arrive in Hearth v0.5",
            ))
        }
        _ => {
            return Err(syn::Error::new(
                args.mode.as_ref().expect("mode present").1,
                "mode must be \"event\"",
            ))
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
    let Some((binding, _binding_span)) = args.binding else {
        return Err(syn::Error::new(
            first_token_span(attr).unwrap_or_else(|| item_impl.span()),
            "#[hearth::room] needs binding = \"...\" (the wrangler DO binding name)",
        ));
    };
    if binding.is_empty() {
        return Err(syn::Error::new(
            first_token_span(attr).unwrap_or_else(|| item_impl.span()),
            "#[hearth::room] needs binding = \"...\" (the wrangler DO binding name)",
        ));
    }
    let rpc = args.rpc.is_some();
    let max_clients = max_clients as u8;
    let empty_grace_secs = args.empty_grace_secs.unwrap_or(60);
    let reconnect_grace_secs = args.reconnect_grace_secs.unwrap_or(30);

    let room = self_ident(&item_impl)?;
    let self_ty = &item_impl.self_ty;
    let object = format_ident!("{room}Object");
    let room_name = room.to_string();
    let wrapper = format_ident!("__hearth_{}_object", room_name.to_ascii_lowercase());
    let export_test = format_ident!("__hearth_schema_export_{}", snake_case(&room_name));

    let callable = rpc.then(|| {
        quote! {
            impl ::hearth::server::Callable for #room {
                const BINDING: &'static str = #binding;
                type Rpc = <#room as ::hearth::server::RoomRpc>::Rpc;
                type Reply = <#room as ::hearth::server::RoomRpc>::Reply;
            }
        }
    });
    let call_route = if rpc {
        quote! {
            if req.path() == "/__hearth/call" {
                return self.0.call(req).await;
            }
        }
    } else {
        quote!()
    };

    Ok(quote! {
        #item_impl

        impl ::hearth::server::RoomConfig for #room {
            const MODE: ::hearth::server::Mode = ::hearth::server::Mode::Event;
            const MAX_CLIENTS: u8 = #max_clients;
            const EMPTY_GRACE: ::std::time::Duration =
                ::std::time::Duration::from_secs(#empty_grace_secs);
            const RECONNECT_GRACE: ::std::time::Duration =
                ::std::time::Duration::from_secs(#reconnect_grace_secs);
        }

        #callable

        /// Test-only schema export for `hearth codegen`: when the driver sets
        /// `HEARTH_SCHEMA_OUT`, this writes the room's `SchemaDoc` there as
        /// pretty JSON. Otherwise it is a no-op, so plain `cargo test` is
        /// unaffected.
        #[cfg(test)]
        #[test]
        fn #export_test() {
            let Some(directory) = ::std::env::var_os("HEARTH_SCHEMA_OUT") else {
                return;
            };
            let doc = ::hearth::server::schema_doc::<#room>(#room_name);
            let json = ::serde_json::to_string_pretty(&doc)
                .expect("a SchemaDoc always serialises as JSON");
            ::std::fs::create_dir_all(&directory)
                .expect("HEARTH_SCHEMA_OUT is a creatable directory");
            let path =
                ::std::path::Path::new(&directory).join(concat!(#room_name, ".json"));
            ::std::fs::write(path, json).expect("HEARTH_SCHEMA_OUT is writable");
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
                Self(::hearth::server::worker::RoomObject::<#room>::new(
                    state,
                    env,
                    #binding,
                ))
            }

            async fn fetch(
                &self,
                req: ::worker::Request,
            ) -> ::worker::Result<::worker::Response> {
                #call_route
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

/// Options accepted by `#[hearth::service(...)]`.
#[derive(Default)]
struct ServiceArgs {
    binding: Option<(String, Span)>,
}

fn expand_service(attr: &TokenStream2, item: &TokenStream2) -> TokenStream2 {
    match expand_service_impl(attr, item) {
        Ok(expanded) => expanded,
        Err(err) => error_with_item(item, err),
    }
}

fn expand_service_impl(attr: &TokenStream2, item: &TokenStream2) -> syn::Result<TokenStream2> {
    let item_impl: ItemImpl = syn::parse2(item.clone())?;
    let args = parse_service_args(attr)?;
    let Some((binding, _)) = args.binding else {
        return Err(syn::Error::new(
            first_token_span(attr).unwrap_or_else(|| item_impl.span()),
            "#[hearth::service] needs binding = \"...\" (the wrangler DO binding name)",
        ));
    };
    if binding.is_empty() {
        return Err(syn::Error::new(
            first_token_span(attr).unwrap_or_else(|| item_impl.span()),
            "#[hearth::service] needs binding = \"...\" (the wrangler DO binding name)",
        ));
    }
    let service = self_ident(&item_impl)?;
    let self_ty = &item_impl.self_ty;
    let object = format_ident!("{service}Object");
    let service_name = service.to_string();
    let wrapper = format_ident!("__hearth_{}_object", service_name.to_ascii_lowercase());

    Ok(quote! {
        #item_impl

        impl ::hearth::server::Callable for #service {
            const BINDING: &'static str = #binding;
            type Rpc = <#service as ::hearth::server::Service>::Rpc;
            type Reply = <#service as ::hearth::server::Service>::Reply;
        }

        // `#[worker::durable_object]`'s wasm-bindgen struct-marker expansion
        // spells the crate as bare `wasm_bindgen`, so the wrapper lives in a
        // hidden module that puts that name in scope.
        #[doc(hidden)]
        pub mod #wrapper {
            #[allow(unused_imports)]
            use super::*;
            #[allow(unused_imports)]
            use ::worker::wasm_bindgen as wasm_bindgen;

            #[::worker::durable_object]
            pub struct #object(::hearth::server::worker::ServiceObject<#self_ty>);

            impl ::worker::DurableObject for #object {
                fn new(state: ::worker::State, env: ::worker::Env) -> Self {
                    Self(::hearth::server::worker::ServiceObject::<#service>::new(
                        state,
                        env,
                        #binding,
                    ))
                }

                async fn fetch(
                    &self,
                    req: ::worker::Request,
                ) -> ::worker::Result<::worker::Response> {
                    self.0.fetch(req).await
                }

                async fn alarm(&self) -> ::worker::Result<::worker::Response> {
                    self.0.alarm().await?;
                    ::worker::Response::empty()
                }
            }
        }

        pub use #wrapper::#object;
    })
}

fn parse_room_args(attr: &TokenStream2) -> syn::Result<RoomArgs> {
    let mut args = RoomArgs::default();
    for meta in metas(attr)? {
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
            "reconnect_grace_secs" => {
                let value = int_value(&meta, "reconnect_grace_secs")?;
                args.reconnect_grace_secs = Some(value.base10_parse::<u64>()?);
            }
            "binding" => {
                let value = str_value(&meta, "binding")?;
                args.binding = Some((value.value(), value.span()));
            }
            "rpc" => match &meta {
                Meta::Path(path) if path.is_ident("rpc") => args.rpc = Some(path.span()),
                _ => return Err(syn::Error::new_spanned(&meta, "rpc is a flag; write `rpc`")),
            },
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

fn parse_service_args(attr: &TokenStream2) -> syn::Result<ServiceArgs> {
    let mut args = ServiceArgs::default();
    for meta in metas(attr)? {
        let Some(name) = meta.path().get_ident().map(ToString::to_string) else {
            return Err(syn::Error::new_spanned(
                &meta,
                "unknown #[hearth::service] option",
            ));
        };
        match name.as_str() {
            "binding" => {
                let value = str_value(&meta, "binding")?;
                args.binding = Some((value.value(), value.span()));
            }
            other => {
                return Err(syn::Error::new_spanned(
                    &meta,
                    format!("unknown #[hearth::service] option `{other}`"),
                ));
            }
        }
    }
    Ok(args)
}

fn metas(attr: &TokenStream2) -> syn::Result<syn::punctuated::Punctuated<Meta, Token![,]>> {
    if attr.is_empty() {
        return Ok(syn::punctuated::Punctuated::new());
    }
    syn::parse::Parser::parse2(
        syn::punctuated::Punctuated::<Meta, Token![,]>::parse_terminated,
        attr.clone(),
    )
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
        "the Hearth attribute needs a plain type on `impl Trait for X`",
    ))
}

fn first_token_span(tokens: &TokenStream2) -> Option<Span> {
    tokens
        .clone()
        .into_iter()
        .next()
        .map(|token: TokenTree| token.span())
}

/// `TicTacToe` becomes `tic_tac_toe` for the export test's function name.
fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (index, character) in name.chars().enumerate() {
        if character.is_ascii_uppercase() {
            if index != 0 && !out.ends_with('_') {
                out.push('_');
            }
            out.push(character.to_ascii_lowercase());
        } else {
            out.push(character);
        }
    }
    out
}
