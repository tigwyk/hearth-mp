//! Hearth attribute macros: `#[hearth_mp::room]` and `#[hearth_mp::service]`.
//!
//! Both take the `impl` block of the trait and add everything the Durable
//! Object adapter needs: the compile-time config, the typed [`Callable`]
//! descriptor and the `#[worker::durable_object]` wrapper the Worker exports.
//!
//! ```ignore
//! #[hearth_mp::room(mode = "event", max_clients = 4, binding = "CARD_MATCH", rpc)]
//! impl Room for CardMatch { /* ... */ }
//!
//! #[hearth_mp::service(binding = "STATS")]
//! impl Service for Stats { /* ... */ }
//! ```

#![warn(missing_docs)]

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2, TokenTree};
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::{Expr, ItemImpl, Lit, Meta, Token, Type};

/// Options accepted by `#[hearth_mp::room(...)]`.
#[derive(Default)]
struct RoomArgs {
    mode: Option<(String, Span)>,
    tick_hz: Option<(u64, Span)>,
    persist_every: Option<(u64, Span)>,
    max_clients: Option<(i64, Span)>,
    empty_grace_secs: Option<u64>,
    reconnect_grace_secs: Option<u64>,
    intent_rate: Option<u32>,
    intent_burst: Option<u32>,
    binding: Option<(String, Span)>,
    /// `allocator = "webhook" | "container"`.
    allocator: Option<(String, Span)>,
    /// `container = "ARENA_SERVER"`: the session container class's DO binding.
    container: Option<(String, Span)>,
    rpc: Option<Span>,
}

#[proc_macro_attribute]
/// Turn an `impl Room for X` into a Hearth room: its [`RoomConfig`](hearth_mp_server::RoomConfig), schema export test and Durable Object glue. See the `hearth` crate docs for the attributes.
pub fn room(attr: TokenStream, item: TokenStream) -> TokenStream {
    expand_room(&TokenStream2::from(attr), &TokenStream2::from(item)).into()
}

#[proc_macro_attribute]
/// Turn an `impl Service for X` into a Hearth service object (a player record, a matchmaker): its `Callable` descriptor and Durable Object glue.
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

    let mode = args
        .mode
        .as_ref()
        .map(|(mode, _)| mode.as_str())
        .unwrap_or("event");
    match mode {
        "event" => {
            if let Some((_, span)) = &args.tick_hz {
                return Err(syn::Error::new(*span, "tick_hz needs mode = \"ticked\""));
            }
            if let Some((_, span)) = &args.persist_every {
                return Err(syn::Error::new(
                    *span,
                    "persist_every needs mode = \"ticked\"",
                ));
            }
        }
        "ticked" => {}
        "handoff" => {
            return Err(syn::Error::new(
                args.mode.as_ref().expect("mode present").1,
                "handoff is not a room mode; call ctx.handoff(...) from an event or ticked room \
                 (see docs/handoff.md)",
            ))
        }
        _ => {
            return Err(syn::Error::new(
                args.mode.as_ref().expect("mode present").1,
                "mode must be \"event\" or \"ticked\"",
            ))
        }
    }

    let Some((max_clients, max_clients_span)) = args.max_clients else {
        return Err(syn::Error::new(
            first_token_span(attr).unwrap_or_else(|| item_impl.span()),
            "missing max_clients; write #[hearth_mp::room(mode = \"event\", max_clients = N)]",
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
            "#[hearth_mp::room] needs binding = \"...\" (the wrangler DO binding name)",
        ));
    };
    if binding.is_empty() {
        return Err(syn::Error::new(
            first_token_span(attr).unwrap_or_else(|| item_impl.span()),
            "#[hearth_mp::room] needs binding = \"...\" (the wrangler DO binding name)",
        ));
    }
    let rpc = args.rpc.is_some();
    let max_clients = max_clients as u8;
    let empty_grace_secs = args.empty_grace_secs.unwrap_or(60);
    let reconnect_grace_secs = args.reconnect_grace_secs.unwrap_or(30);
    let intent_rate = args.intent_rate.unwrap_or(60);
    let intent_burst = args.intent_burst.unwrap_or(120);
    let allocator = match &args.allocator {
        Some((allocator, _)) => quote!(::std::option::Option::Some(#allocator)),
        None => quote!(::std::option::Option::None),
    };
    let container = match &args.container {
        Some((container, _)) => quote!(::std::option::Option::Some(#container)),
        None => quote!(::std::option::Option::None),
    };

    // Ticked mode: a required tick rate in 5..=20 Hz (the ADR's ceiling) and a
    // persist interval; event mode leaves both at the trait defaults.
    let (mode_tokens, tick_hz, persist_every) = if mode == "ticked" {
        let Some((hz, hz_span)) = args.tick_hz else {
            return Err(syn::Error::new(
                first_token_span(attr).unwrap_or_else(|| item_impl.span()),
                "mode = \"ticked\" needs tick_hz = N (5..=20)",
            ));
        };
        if !(5..=20).contains(&hz) {
            return Err(syn::Error::new(
                hz_span,
                "tick_hz must be 5..=20; above 20 Hz use a dedicated server (Handoff, v0.5)",
            ));
        }
        let every = match args.persist_every {
            Some((every, span)) => {
                if every < 1 {
                    return Err(syn::Error::new(span, "persist_every must be at least 1"));
                }
                every
            }
            None => 20,
        };
        let every = u32::try_from(every).map_err(|_| {
            syn::Error::new(
                first_token_span(attr).unwrap_or_else(|| item_impl.span()),
                "persist_every is too large",
            )
        })?;
        (quote!(::hearth_mp::server::Mode::Ticked), hz as u8, every)
    } else {
        (quote!(::hearth_mp::server::Mode::Event), 0u8, 20u32)
    };

    let room = self_ident(&item_impl)?;
    let self_ty = &item_impl.self_ty;
    let object = format_ident!("{room}Object");
    let room_name = room.to_string();
    let wrapper = format_ident!("__hearth_{}_object", room_name.to_ascii_lowercase());
    let export_test = format_ident!("__hearth_schema_export_{}", snake_case(&room_name));

    let callable = rpc.then(|| {
        quote! {
            impl ::hearth_mp::server::Callable for #room {
                const BINDING: &'static str = #binding;
                type Rpc = <#room as ::hearth_mp::server::RoomRpc>::Rpc;
                type Reply = <#room as ::hearth_mp::server::RoomRpc>::Reply;
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

        impl ::hearth_mp::server::RoomConfig for #room {
            const MODE: ::hearth_mp::server::Mode = #mode_tokens;
            const TICK_HZ: u8 = #tick_hz;
            const PERSIST_EVERY: u32 = #persist_every;
            const MAX_CLIENTS: u8 = #max_clients;
            const EMPTY_GRACE: ::std::time::Duration =
                ::std::time::Duration::from_secs(#empty_grace_secs);
            const RECONNECT_GRACE: ::std::time::Duration =
                ::std::time::Duration::from_secs(#reconnect_grace_secs);
            const INTENT_RATE: u32 = #intent_rate;
            const INTENT_BURST: u32 = #intent_burst;
            const ALLOCATOR: ::std::option::Option<&'static str> = #allocator;
            const CONTAINER: ::std::option::Option<&'static str> = #container;
        }

        #callable

        /// Test-only schema export for `hearth-mp codegen`: when the driver sets
        /// `HEARTH_SCHEMA_OUT`, this writes the room's `SchemaDoc` there as
        /// pretty JSON. Otherwise it is a no-op, so plain `cargo test` is
        /// unaffected.
        #[cfg(test)]
        #[test]
        fn #export_test() {
            let Some(directory) = ::std::env::var_os("HEARTH_SCHEMA_OUT") else {
                return;
            };
            let doc = ::hearth_mp::server::schema_doc::<#room>(#room_name);
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
            pub struct #object(::hearth_mp::server::worker::RoomObject<#self_ty>);

            impl ::worker::DurableObject for #object {
            fn new(state: ::worker::State, env: ::worker::Env) -> Self {
                Self(::hearth_mp::server::worker::RoomObject::<#room>::new(
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

/// Options accepted by `#[hearth_mp::service(...)]`.
#[derive(Default)]
struct ServiceArgs {
    binding: Option<(String, Span)>,
    /// Generate a player DO (with the read-only `GET /__hearth/view` path).
    player: Option<Span>,
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
            "#[hearth_mp::service] needs binding = \"...\" (the wrangler DO binding name)",
        ));
    };
    if binding.is_empty() {
        return Err(syn::Error::new(
            first_token_span(attr).unwrap_or_else(|| item_impl.span()),
            "#[hearth_mp::service] needs binding = \"...\" (the wrangler DO binding name)",
        ));
    }
    let service = self_ident(&item_impl)?;
    let self_ty = &item_impl.self_ty;
    let object = format_ident!("{service}Object");
    let service_name = service.to_string();
    let wrapper = format_ident!("__hearth_{}_object", service_name.to_ascii_lowercase());
    // A player service is the same DO with a typed client view path, so the
    // wrapper picks PlayerObject and the trait bound enforces the impl.
    let (wrapper_type, object_new) = if args.player.is_some() {
        (
            quote!(::hearth_mp::server::worker::PlayerObject<#self_ty>),
            quote!(::hearth_mp::server::worker::PlayerObject::<#service>::new(
                state, env, #binding
            )),
        )
    } else {
        (
            quote!(::hearth_mp::server::worker::ServiceObject<#self_ty>),
            quote!(::hearth_mp::server::worker::ServiceObject::<#service>::new(
                state, env, #binding
            )),
        )
    };

    Ok(quote! {
        #item_impl

        impl ::hearth_mp::server::Callable for #service {
            const BINDING: &'static str = #binding;
            type Rpc = <#service as ::hearth_mp::server::Service>::Rpc;
            type Reply = <#service as ::hearth_mp::server::Service>::Reply;
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
            pub struct #object(#wrapper_type);

            impl ::worker::DurableObject for #object {
                fn new(state: ::worker::State, env: ::worker::Env) -> Self {
                    Self(#object_new)
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
                "unknown #[hearth_mp::room] option",
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
            "intent_rate" => {
                let value = int_value(&meta, "intent_rate")?;
                let rate = value.base10_parse::<u32>()?;
                if rate < 1 {
                    return Err(syn::Error::new(
                        value.span(),
                        "intent_rate must be at least 1",
                    ));
                }
                args.intent_rate = Some(rate);
            }
            "tick_hz" => {
                let value = int_value(&meta, "tick_hz")?;
                args.tick_hz = Some((value.base10_parse::<u64>()?, value.span()));
            }
            "persist_every" => {
                let value = int_value(&meta, "persist_every")?;
                args.persist_every = Some((value.base10_parse::<u64>()?, value.span()));
            }
            "intent_burst" => {
                let value = int_value(&meta, "intent_burst")?;
                let burst = value.base10_parse::<u32>()?;
                if burst < 1 {
                    return Err(syn::Error::new(
                        value.span(),
                        "intent_burst must be at least 1",
                    ));
                }
                args.intent_burst = Some(burst);
            }
            "binding" => {
                let value = str_value(&meta, "binding")?;
                args.binding = Some((value.value(), value.span()));
            }
            "allocator" => {
                let value = str_value(&meta, "allocator")?;
                let allocator = value.value();
                if allocator != "webhook" && allocator != "container" {
                    return Err(syn::Error::new(
                        value.span(),
                        "allocator must be \"webhook\" or \"container\"",
                    ));
                }
                args.allocator = Some((allocator, value.span()));
            }
            "container" => {
                let value = str_value(&meta, "container")?;
                let binding = value.value();
                if binding.is_empty() {
                    return Err(syn::Error::new(
                        value.span(),
                        "container must name the DO binding, e.g. container = \"ARENA_SERVER\"",
                    ));
                }
                args.container = Some((binding, value.span()));
            }
            "rpc" => match &meta {
                Meta::Path(path) if path.is_ident("rpc") => args.rpc = Some(path.span()),
                _ => return Err(syn::Error::new_spanned(&meta, "rpc is a flag; write `rpc`")),
            },
            other => {
                return Err(syn::Error::new_spanned(
                    &meta,
                    format!("unknown #[hearth_mp::room] option `{other}`"),
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
                "unknown #[hearth_mp::service] option",
            ));
        };
        match name.as_str() {
            "binding" => {
                let value = str_value(&meta, "binding")?;
                args.binding = Some((value.value(), value.span()));
            }
            "player" => match &meta {
                Meta::Path(path) if path.is_ident("player") => args.player = Some(path.span()),
                _ => {
                    return Err(syn::Error::new_spanned(
                        &meta,
                        "player is a flag; write `player`",
                    ))
                }
            },
            other => {
                return Err(syn::Error::new_spanned(
                    &meta,
                    format!("unknown #[hearth_mp::service] option `{other}`"),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn expanded(attr: &str) -> String {
        let attr: TokenStream2 = attr.parse().expect("attribute tokens");
        let item: TokenStream2 = quote! { impl Room for Game {} };
        expand_room_impl(&attr, &item)
            .expect("the attribute expands")
            .to_string()
    }

    fn expansion_error(attr: &str) -> String {
        let attr: TokenStream2 = attr.parse().expect("attribute tokens");
        let item: TokenStream2 = quote! { impl Room for Game {} };
        expand_room_impl(&attr, &item)
            .expect_err("the attribute is rejected")
            .to_string()
    }

    #[test]
    fn intent_caps_are_emitted() {
        let expanded = expanded(
            "mode = \"event\", max_clients = 2, binding = \"GAME\", \
             intent_rate = 5, intent_burst = 9",
        );
        assert!(
            expanded.contains("const INTENT_RATE : u32 = 5u32"),
            "{expanded}"
        );
        assert!(
            expanded.contains("const INTENT_BURST : u32 = 9u32"),
            "{expanded}"
        );
    }

    #[test]
    fn intent_caps_default_to_v0_1_values() {
        let expanded = expanded("mode = \"event\", max_clients = 2, binding = \"GAME\"");
        assert!(
            expanded.contains("const INTENT_RATE : u32 = 60u32"),
            "{expanded}"
        );
        assert!(
            expanded.contains("const INTENT_BURST : u32 = 120u32"),
            "{expanded}"
        );
    }

    #[test]
    fn intent_rate_and_burst_must_be_at_least_one() {
        let base = "mode = \"event\", max_clients = 2, binding = \"GAME\"";
        assert_eq!(
            expansion_error(&format!("{base}, intent_rate = 0")),
            "intent_rate must be at least 1"
        );
        assert_eq!(
            expansion_error(&format!("{base}, intent_burst = 0")),
            "intent_burst must be at least 1"
        );
    }

    #[test]
    fn allocator_is_emitted_or_none() {
        let base = "mode = \"event\", max_clients = 2, binding = \"GAME\"";
        let default = expanded(base);
        assert!(default.contains("const ALLOCATOR"), "{default}");
        assert!(default.contains("None"), "{default}");
        let webhook = expanded(&format!("{base}, allocator = \"webhook\""));
        assert!(webhook.contains("Some"), "{webhook}");
        assert!(webhook.contains("webhook"), "{webhook}");
        assert_eq!(
            expansion_error(&format!("{base}, allocator = \"kubernetes\"")),
            "allocator must be \"webhook\" or \"container\""
        );
    }
}
