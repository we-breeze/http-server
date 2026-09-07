use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{Expr, Ident, ItemImpl, Path, Token, Type, parse_macro_input};

use crate::expand::server_crate_path;

struct RegistryArguments {
    name: Ident,
    state: Type,
    auth: Option<Type>,
}

impl Parse for RegistryArguments {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut name = None;
        let mut state = None;
        let mut auth = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "name" | "group" if name.is_none() => name = Some(input.parse()?),
                "state" if state.is_none() => state = Some(input.parse()?),
                "auth" if auth.is_none() => auth = Some(input.parse()?),
                _ => {
                    return Err(syn::Error::new_spanned(
                        key,
                        "expected group, state, or auth, each at most once (name aliases group)",
                    ));
                }
            }
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            name: name.unwrap_or_else(|| syn::parse_quote!(http_apis)),
            state: state
                .ok_or_else(|| input.error("registry requires state = ApplicationState"))?,
            auth,
        })
    }
}

pub(crate) fn declare(input: TokenStream) -> TokenStream {
    let RegistryArguments { name, state, auth } = parse_macro_input!(input as RegistryArguments);
    let server = server_crate_path();
    let auth = auth.map_or_else(|| quote!(#server::NoAuthenticator), |auth| quote!(#auth));
    let state_alias = format_ident!("__HttpRegistryState_{}", name);
    let auth_alias = format_ident!("__HttpRegistryAuth_{}", name);
    quote! {
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        pub(crate) type #state_alias = #state;
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        pub(crate) type #auth_alias = #auth;

        #[doc(hidden)]
        pub(crate) mod #name {
            pub type State = super::#state_alias;
            pub type Auth = super::#auth_alias;

            pub struct Entry(pub #server::__private::ApiRegistration<State, Auth>);
            #server::__private::inventory::collect!(Entry);

            pub fn handlers(state: &State) -> ::core::result::Result<#server::Router<Auth>, #server::RegistryError> {
                let entries: ::std::vec::Vec<_> = #server::__private::inventory::iter::<Entry>
                    .into_iter()
                    .map(|entry| #server::__private::ApiRegistration {
                        name: entry.0.name,
                        routes: entry.0.routes,
                        build: entry.0.build,
                    })
                    .collect();
                #server::__private::collect(state, &entries)
            }
        }
    }.into()
}

struct CollectArguments {
    state: Expr,
    registry: Path,
}

impl Parse for CollectArguments {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let state = input.parse()?;
        let mut registry = syn::parse_quote!(crate::http_apis);
        if !input.is_empty() {
            input.parse::<Token![,]>()?;
            if !input.is_empty() {
                if input.peek(Ident) && input.peek2(Token![=]) {
                    let key: Ident = input.parse()?;
                    if key != "registry" && key != "group" {
                        return Err(syn::Error::new_spanned(
                            key,
                            "expected a group name or registry = path",
                        ));
                    }
                    input.parse::<Token![=]>()?;
                    registry = input.parse()?;
                    if key == "group" {
                        registry = group_path(registry);
                    }
                } else {
                    registry = group_path(input.parse()?);
                }
                if !input.is_empty() {
                    input.parse::<Token![,]>()?;
                }
            }
        }
        Ok(Self { state, registry })
    }
}

/// Bare group names refer to declarations in the application crate root.
pub(crate) fn group_path(path: Path) -> Path {
    if path.leading_colon.is_none() && path.segments.len() == 1 {
        syn::parse_quote!(crate::#path)
    } else {
        path
    }
}

pub(crate) fn collect(input: TokenStream) -> TokenStream {
    let CollectArguments { state, registry } = parse_macro_input!(input as CollectArguments);
    quote!(#registry::handlers(&(#state))).into()
}

pub(crate) fn registration(
    registry: &Path,
    input: &ItemImpl,
    server: &TokenStream2,
    descriptors: &[TokenStream2],
) -> syn::Result<TokenStream2> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "automatic registration requires a concrete API type; use a concrete impl or merge a generic API instance manually",
        ));
    }
    let self_ty = &input.self_ty;
    let cfg = input.attrs.iter().filter(|attribute| {
        attribute.path().is_ident("cfg") || attribute.path().is_ident("cfg_attr")
    });
    Ok(quote! {
        #(#cfg)*
        const _: () = {
            #server::__private::inventory::submit! {
                #registry::Entry(#server::__private::ApiRegistration {
                    name: concat!(module_path!(), "::", stringify!(#self_ty)),
                    routes: || {
                        const ROUTES: &[#server::__private::RouteDescriptor] = &[#(#descriptors),*];
                        ROUTES
                    },
                    build: |state| #server::Router::<#registry::Auth>::new(
                        <#self_ty as #server::FromState<#registry::State>>::from_state(state)
                    ),
                })
            }
        };
    })
}
