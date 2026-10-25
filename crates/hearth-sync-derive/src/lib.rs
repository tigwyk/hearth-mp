//! Derive macros for Hearth state sync.
//!
//! `#[derive(Schema)]` implements `hearth_sync::Schema` for a struct (named,
//! tuple or unit) or an enum: the compile-time `HASH` and the machine-readable
//! `describe`. `HASH` folds, in declaration order: the type name, then each
//! field's name and `<FieldTy as Schema>::HASH` (or for an enum, each variant's
//! name followed by its fields).
//!
//! `#[derive(HearthState)]` implements `Schema` and `HearthState` for a struct
//! with named fields. Only `#[sync]` and `#[sync(owner)]` fields take part in
//! the schema hash, the description and the per-client view; unmarked fields
//! never leave the server. The JSON key of a synced field is its Rust field
//! name.
//!
//! Because generated typed clients mirror the wire shape, both derives reject
//! `#[serde(...)]` attributes that would change that shape (`rename`,
//! `rename_all`, `tag`, `content`, `untagged`, `flatten` and the `skip*`
//! family). `#[serde(default)]` is allowed.
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
use syn::punctuated::Punctuated;
use syn::{Attribute, Data, DeriveInput, Field, Fields, Meta, Token};

/// `#[serde(...)]` options that change the wire shape. A generated typed client
/// would silently desync against a type that uses them.
const FORBIDDEN_SERDE: &[&str] = &[
    "rename",
    "rename_all",
    "tag",
    "content",
    "untagged",
    "flatten",
    "skip",
    "skip_serializing",
    "skip_deserializing",
    "skip_serializing_if",
];

/// Derive the compile-time state fingerprint and the machine-readable shape.
#[proc_macro_derive(Schema, attributes(sync, hearth))]
pub fn derive_schema(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    expand_schema(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derive schema hashing, the machine-readable shape and the owner-filtered
/// per-client view.
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

/// Reject `#[serde(...)]` attributes that would change the wire shape.
fn check_serde_attrs(attrs: &[Attribute]) -> syn::Result<()> {
    for attr in attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        let options = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
        for option in options {
            let Some(name) = option.path().get_ident().map(ToString::to_string) else {
                continue;
            };
            if FORBIDDEN_SERDE.contains(&name.as_str()) {
                return Err(syn::Error::new_spanned(
                    attr,
                    format!(
                        "Hearth schema types can't change their serde shape (found `{name}`); typed clients would desync"
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn expand_schema(input: &DeriveInput) -> syn::Result<TokenStream2> {
    check_serde_attrs(&input.attrs)?;
    let rt = runtime(&input.attrs)?;
    let ident = &input.ident;
    let type_name = ident.to_string();
    let mut hash = quote!(#rt::fnv1a64(#type_name.as_bytes()));
    let def = match &input.data {
        Data::Struct(data) => {
            let fields = describe_fields(&data.fields, &rt)?;
            hash = fold_fields(hash, &data.fields, &rt);
            quote!(#rt::TypeDef::Struct { fields: vec![#(#fields),*] })
        }
        Data::Enum(data) => {
            let mut variants = Vec::new();
            for variant in &data.variants {
                check_serde_attrs(&variant.attrs)?;
                let variant_name = variant.ident.to_string();
                let shape = describe_shape(&variant.fields, &rt)?;
                variants.push(quote!(#rt::Variant {
                    name: #variant_name.to_owned(),
                    shape: #shape,
                }));
                hash = quote!(#rt::mix(#hash, #rt::fnv1a64(#variant_name.as_bytes())));
                hash = fold_fields(hash, &variant.fields, &rt);
            }
            quote!(#rt::TypeDef::Enum { variants: vec![#(#variants),*] })
        }
        Data::Union(_) => {
            return Err(syn::Error::new_spanned(
                ident,
                "#[derive(Schema)] does not support unions",
            ));
        }
    };
    Ok(quote! {
        impl #rt::Schema for #ident {
            const HASH: u64 = #hash;

            fn describe(reg: &mut #rt::Registry) -> #rt::TypeRef {
                reg.add(#type_name, <Self as #rt::Schema>::HASH, |reg| #def);
                #rt::TypeRef::Named(#type_name.to_owned())
            }
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

/// Describe a struct's or struct variant's fields. Tuple fields are named by
/// their position.
fn describe_fields(fields: &Fields, rt: &TokenStream2) -> syn::Result<Vec<TokenStream2>> {
    let mut described = Vec::new();
    for (index, field) in fields.iter().enumerate() {
        check_serde_attrs(&field.attrs)?;
        let name = field
            .ident
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| index.to_string());
        let ty = &field.ty;
        described.push(quote!(#rt::Field {
            name: #name.to_owned(),
            ty: <#ty as #rt::Schema>::describe(reg),
            vis: #rt::Vis::Public,
        }));
    }
    Ok(described)
}

/// Describe one enum variant's payload shape.
fn describe_shape(fields: &Fields, rt: &TokenStream2) -> syn::Result<TokenStream2> {
    Ok(match fields {
        Fields::Unit => quote!(#rt::Shape::Unit),
        Fields::Unnamed(unnamed) => {
            let mut types = Vec::new();
            for field in &unnamed.unnamed {
                check_serde_attrs(&field.attrs)?;
                let ty = &field.ty;
                types.push(quote!(<#ty as #rt::Schema>::describe(reg)));
            }
            if let [only] = types.as_slice() {
                quote!(#rt::Shape::Newtype(#only))
            } else {
                quote!(#rt::Shape::Tuple(vec![#(#types),*]))
            }
        }
        Fields::Named(_) => {
            let fields = describe_fields(fields, rt)?;
            quote!(#rt::Shape::Struct(vec![#(#fields),*]))
        }
    })
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
    check_serde_attrs(&input.attrs)?;
    let rt = runtime(&input.attrs)?;
    let ident = &input.ident;
    let type_name = ident.to_string();
    let mut hash = quote!(#rt::fnv1a64(#type_name.as_bytes()));
    let mut inserts = Vec::new();
    let mut described = Vec::new();
    for field in &named.named {
        check_serde_attrs(&field.attrs)?;
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
        let vis = match visibility {
            Visibility::Plain => quote!(#rt::Vis::Public),
            Visibility::Owner => quote!(#rt::Vis::Owner),
        };
        described.push(quote!(#rt::Field {
            name: #field_name.to_owned(),
            ty: <#ty as #rt::Schema>::describe(reg),
            vis: #vis,
        }));
        inserts.push(quote!(map.insert(#field_name.to_owned(), #value);));
    }
    Ok(quote! {
        impl #rt::Schema for #ident {
            const HASH: u64 = #hash;

            fn describe(reg: &mut #rt::Registry) -> #rt::TypeRef {
                reg.add(#type_name, <Self as #rt::Schema>::HASH, |reg| #rt::TypeDef::Struct {
                    fields: vec![#(#described),*],
                });
                #rt::TypeRef::Named(#type_name.to_owned())
            }
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
