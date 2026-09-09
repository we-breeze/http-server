use std::collections::BTreeSet;

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::ext::IdentExt;
use syn::parse::{Parse, ParseStream};
use syn::{Expr, Ident, Path, Token, Type, parse_macro_input};

use crate::expand::server_crate_path;

struct RegistryArguments {
    name: Ident,
    dependencies: Vec<(Ident, Type)>,
    auth: Option<Type>,
}

impl Parse for RegistryArguments {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut name = None;
        let mut dependencies = None;
        let mut auth = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            if key == "dependencies" && dependencies.is_none() {
                let content;
                syn::parenthesized!(content in input);
                let mut fields = Vec::new();
                let mut names = BTreeSet::new();
                while !content.is_empty() {
                    let field: Ident = content.parse()?;
                    if !names.insert(field.to_string()) {
                        return Err(syn::Error::new_spanned(field, "duplicate dependency name"));
                    }
                    content.parse::<Token![:]>()?;
                    fields.push((field, content.parse()?));
                    if !content.is_empty() {
                        content.parse::<Token![,]>()?;
                    }
                }
                dependencies = Some(fields);
            } else {
                input.parse::<Token![=]>()?;
                match key.to_string().as_str() {
                    "group" if name.is_none() => name = Some(input.parse()?),
                    "auth" if auth.is_none() => auth = Some(input.parse()?),
                    _ => {
                        return Err(syn::Error::new_spanned(
                            key,
                            "expected dependencies(...), group, or auth, each at most once",
                        ));
                    }
                }
            }
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            name: name.unwrap_or_else(|| syn::parse_quote!(http_apis)),
            dependencies: dependencies.unwrap_or_default(),
            auth,
        })
    }
}

pub(crate) fn declare(input: TokenStream) -> TokenStream {
    let RegistryArguments {
        name,
        dependencies,
        auth,
    } = parse_macro_input!(input as RegistryArguments);
    let server = server_crate_path();
    let auth = auth.map_or_else(|| quote!(#server::NoAuthenticator), |auth| quote!(#auth));
    let module = registry_module(&name);
    let state_type = format_ident!("__HttpDependencies_{}", name);
    let auth_alias = format_ident!("__HttpRegistryAuth_{}", name);
    let fields = dependencies.iter().map(|(name, ty)| quote!(pub #name: #ty));
    quote! {
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        pub(crate) struct #state_type { #(#fields),* }
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        pub(crate) type #auth_alias = #auth;

        #[doc(hidden)]
        pub(crate) mod #module {
            pub type Dependencies = super::#state_type;
            pub type State = ::std::sync::Arc<Dependencies>;
            pub type Auth = super::#auth_alias;

            pub struct Entry(pub #server::__private::ApiRegistration<State, Auth>);
            #server::__private::inventory::collect!(Entry);

            pub fn handlers(dependencies: Dependencies) -> ::core::result::Result<#server::Router<Auth>, #server::RegistryError> {
                let entries: ::std::vec::Vec<_> = #server::__private::inventory::iter::<Entry>
                    .into_iter()
                    .map(|entry| #server::__private::ApiRegistration {
                        name: entry.0.name,
                        routes: entry.0.routes,
                        build: entry.0.build,
                    })
                    .collect();
                #server::__private::collect(&::std::sync::Arc::new(dependencies), &entries)
            }
        }
    }.into()
}

struct CollectArguments {
    dependencies: Vec<(Ident, Expr)>,
    registry: Path,
}

impl Parse for CollectArguments {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut dependencies = Vec::new();
        let mut names = BTreeSet::new();
        while !input.is_empty() && !input.peek(Token![;]) {
            let name: Ident = input.parse()?;
            if !names.insert(name.to_string()) {
                return Err(syn::Error::new_spanned(name, "duplicate dependency name"));
            }
            let value = if input.peek(Token![=]) {
                input.parse::<Token![=]>()?;
                input.parse()?
            } else {
                syn::parse_quote!(#name)
            };
            dependencies.push((name, value));
            if !input.is_empty() && !input.peek(Token![;]) {
                input.parse::<Token![,]>()?;
            }
        }
        let mut registry = group_path(syn::parse_quote!(http_apis));
        if input.peek(Token![;]) {
            input.parse::<Token![;]>()?;
            let key: Ident = input.parse()?;
            if key != "group" {
                return Err(syn::Error::new_spanned(key, "expected group = path"));
            }
            input.parse::<Token![=]>()?;
            registry = group_path(input.parse()?);
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            dependencies,
            registry,
        })
    }
}

fn registry_module(name: &Ident) -> Ident {
    format_ident!("__http_registry_{}", name.unraw())
}

/// Resolve logical group paths to generated modules, preserving the parent path.
/// Bare group names refer to declarations in the application crate root.
pub(crate) fn group_path(mut path: Path) -> Path {
    let group = path.segments.last_mut().expect("a group path is nonempty");
    group.ident = registry_module(&group.ident);
    if path.leading_colon.is_none() && path.segments.len() == 1 {
        syn::parse_quote!(crate::#path)
    } else {
        path
    }
}

pub(crate) fn collect(input: TokenStream) -> TokenStream {
    let CollectArguments {
        dependencies,
        registry,
    } = parse_macro_input!(input as CollectArguments);
    let fields = dependencies
        .iter()
        .map(|(name, value)| quote!(#name: #value));
    quote!(#registry::handlers(#registry::Dependencies { #(#fields),* })).into()
}

pub(crate) fn registration(
    registry: &Path,
    adapter: &Ident,
    function: &Ident,
    server: &TokenStream2,
    descriptors: &[TokenStream2],
) -> TokenStream2 {
    quote! {
        const _: () = {
            #server::__private::inventory::submit! {
                #registry::Entry(#server::__private::ApiRegistration {
                    name: concat!(module_path!(), "::", stringify!(#function)),
                    routes: || {
                        const ROUTES: &[#server::__private::RouteDescriptor] = &[#(#descriptors),*];
                        ROUTES
                    },
                    build: |state| #server::Router::<#registry::Auth>::new(
                        #adapter { __http_dependencies: ::std::sync::Arc::clone(state) }
                    ),
                })
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicate_dependency_declarations_and_values() {
        for source in [
            "dependencies(state: String, state: u64)",
            "dependencies(), dependencies()",
            "group = first, group = second",
            "state = AppState",
        ] {
            assert!(
                syn::parse_str::<RegistryArguments>(source).is_err(),
                "{source}"
            );
        }
        for source in [
            "state = one, state = two",
            "state, state",
            "state; group = first; group = second",
            "state; auth = Auth",
        ] {
            assert!(
                syn::parse_str::<CollectArguments>(source).is_err(),
                "{source}"
            );
        }
    }
}
