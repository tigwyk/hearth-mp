//! Derive macros for Hearth state sync.
//!
//! `#[derive(Schema)]` implements `hearth_sync::Schema` for a struct (named,
//! tuple or unit) or an enum. `HASH` folds, in declaration order: the type
//! name, then each field's name and `<FieldTy as Schema>::HASH` (or for an
//! enum, each variant's name followed by its fields).
//!
//! `#[derive(HearthState)]` implements `Schema` and `HearthState` for a struct
//! with named fields. Only `#[sync]` and `#[sync(owner)]` fields take part in
//! the schema hash and the per-client view; unmarked fields never leave the
//! server. The JSON key of a synced field is its Rust field name.
//!
//! # Crate path
//!
//! Generated code refers to the runtime crate as `::hearth_sync` by default.
//! A container attribute `#[hearth(crate = "hearth")]` makes it refer to
//! `::hearth::sync` instead; the attribute value is the facade crate name, so
//! the `hearth` facade uses exactly `#[hearth(crate = "hearth")]`. As with any
//! derive helper attribute, it must follow the `#[derive(...)]` line.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Data, DeriveInput, Field, Fields, Meta, Token};

/// Derive the compile-time state fingerprint.
#[proc_macro_derive(Schema, attributes(sync, hearth))]
pub fn derive_schema(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    expand_schema(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derive schema hashing and the owner-filtered per-client view.
#[proc_macro_derive(HearthState, attributes(sync, hearth))]
pub fn derive_hearth_state(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    expand_hearth_state(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Path generated code uses to reach the runtime crate: `::hearth_sync`, or
/// `::<facade>::sync` when `#[hearth(crate = "<facade>")]` is present.
fn runtime(attrs: &[syn::Attribute]) -> syn::Result<TokenStream2> {
    for attr in attrs {
        if !attr.path().is_ident("hearth") {
            continue;
        }
        let facade = attr.parse_args_with(|input: syn::parse::ParseStream| {
            input.parse::<Token![crate]>()?;
            input.parse::<Token![=]>()?;
            input.parse::<syn::LitStr>()?.parse::<syn::Path>()
        })?;
        return Ok(quote!(::#facade::sync));
    }
    Ok(quote!(::hearth_sync))
}

fn expand_schema(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let rt = runtime(&input.attrs)?;
    let ident = &input.ident;
    let type_name = ident.to_string();
    let mut hash = quote!(#rt::fnv1a64(#type_name.as_bytes()));
    match &input.data {
        Data::Struct(data) => hash = fold_fields(hash, &data.fields, &rt),
        Data::Enum(data) => {
            for variant in &data.variants {
                let variant_name = variant.ident.to_string();
                hash = quote!(#rt::mix(#hash, #rt::fnv1a64(#variant_name.as_bytes())));
                hash = fold_fields(hash, &variant.fields, &rt);
            }
        }
        Data::Union(_) => {
            return Err(syn::Error::new_spanned(
                ident,
                "#[derive(Schema)] does not support unions",
            ));
        }
    }
    Ok(quote! {
        impl #rt::Schema for #ident {
            const HASH: u64 = #hash;
        }
    })
}

/// Fold field names (named fields only) and field type hashes into `hash`.
fn fold_fields(mut hash: TokenStream2, fields: &Fields, rt: &TokenStream2) -> TokenStream2 {
    for field in fields {
        if let Some(field_ident) = &field.ident {
            let field_name = field_ident.to_string();
            hash = quote!(#rt::mix(#hash, #rt::fnv1a64(#field_name.as_bytes())));
        }
        let ty = &field.ty;
        hash = quote!(#rt::mix(#hash, <#ty as #rt::Schema>::HASH));
    }
    hash
}

#[derive(Clone, Copy)]
enum Visibility {
    Plain,
    Owner,
}

/// Parse a field's `#[sync]` / `#[sync(owner)]` marker.
fn field_visibility(field: &Field) -> syn::Result<Option<Visibility>> {
    let mut visibility = None;
    for attr in &field.attrs {
        if !attr.path().is_ident("sync") {
            continue;
        }
        visibility = Some(match &attr.meta {
            Meta::Path(_) => Visibility::Plain,
            Meta::List(list) => {
                let arg = list.parse_args::<syn::Ident>()?;
                match arg.to_string().as_str() {
                    "owner" => Visibility::Owner,
                    "team" => {
                        return Err(syn::Error::new_spanned(
                            attr,
                            "#[sync(team)] is not supported yet; only #[sync] and #[sync(owner)]",
                        ));
                    }
                    other => {
                        return Err(syn::Error::new_spanned(
                            attr,
                            format!(
                                "#[sync({other})] is not supported; use #[sync] or #[sync(owner)]"
                            ),
                        ));
                    }
                }
            }
            Meta::NameValue(_) => {
                return Err(syn::Error::new_spanned(
                    attr,
                    "#[sync] takes no value; use #[sync] or #[sync(owner)]",
                ));
            }
        });
    }
    Ok(visibility)
}

fn expand_hearth_state(input: &DeriveInput) -> syn::Result<TokenStream2> {
    const NAMED_STRUCT: &str = "#[HearthState] must be derived on a struct with named fields";
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(&input.ident, NAMED_STRUCT));
    };
    let Fields::Named(named) = &data.fields else {
        return Err(syn::Error::new_spanned(&input.ident, NAMED_STRUCT));
    };
    let rt = runtime(&input.attrs)?;
    let ident = &input.ident;
    let type_name = ident.to_string();
    let mut hash = quote!(#rt::fnv1a64(#type_name.as_bytes()));
    let mut inserts = Vec::new();
    for field in &named.named {
        let Some(visibility) = field_visibility(field)? else {
            continue;
        };
        let field_ident = field.ident.as_ref().expect("named field");
        let field_name = field_ident.to_string();
        let ty = &field.ty;
        let tag = match visibility {
            Visibility::Plain => quote!(#rt::fnv1a64(b"sync")),
            Visibility::Owner => quote!(#rt::fnv1a64(b"owner")),
        };
        hash = quote!(#rt::mix(
            #rt::mix(#rt::mix(#hash, #rt::fnv1a64(#field_name.as_bytes())), #tag),
            <#ty as #rt::Schema>::HASH
        ));
        let value = match visibility {
            Visibility::Plain => quote!(#rt::__private::to_value(&self.#field_ident)),
            Visibility::Owner => quote!(#rt::OwnerScoped::owner_view(&self.#field_ident, viewer)),
        };
        inserts.push(quote!(map.insert(#field_name.to_owned(), #value);));
    }
    Ok(quote! {
        impl #rt::Schema for #ident {
            const HASH: u64 = #hash;
        }

        impl #rt::HearthState for #ident {
            #[allow(unused_mut)]
            fn view(&self, viewer: &#rt::PlayerId) -> #rt::Value {
                let _ = viewer;
                let mut map = #rt::__private::Map::new();
                #(#inserts)*
                #rt::Value::Object(map)
            }
        }
    })
}
