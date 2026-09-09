use super::{RouteArguments, expand_adapter, route_attribute};
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{FnArg, Ident, ItemFn, Pat, PatType, Type, parse_macro_input};

pub(crate) fn expand_function(
    method: &'static str,
    arguments: TokenStream,
    input: TokenStream,
) -> TokenStream {
    let arguments = TokenStream2::from(arguments);
    let input = parse_macro_input!(input as ItemFn);
    expand(method, &arguments, input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand(
    method: &'static str,
    arguments: &TokenStream2,
    mut input: ItemFn,
) -> syn::Result<TokenStream2> {
    let route: RouteArguments = syn::parse2(arguments.clone())?;
    if let Some((_, index)) = route_attribute(&input.attrs) {
        return Err(syn::Error::new_spanned(
            &input.attrs[index],
            "use exactly one HTTP route attribute per function",
        ));
    }
    if input.sig.unsafety.is_some() || input.sig.abi.is_some() || input.sig.variadic.is_some() {
        return Err(syn::Error::new_spanned(
            &input.sig,
            "HTTP API functions must use a safe Rust signature",
        ));
    }
    let registry = route
        .group
        .clone()
        .unwrap_or_else(|| syn::parse_quote!(crate::http_apis));
    let name = &input.sig.ident;
    let adapter = format_ident!("__HttpEndpoint_{}", name);
    let mut call_arguments = Vec::new();
    for argument in &input.sig.inputs {
        let FnArg::Typed(argument) = argument else {
            return Err(syn::Error::new_spanned(
                argument,
                "HTTP route attributes require a free function without self",
            ));
        };
        let Pat::Ident(pattern) = argument.pat.as_ref() else {
            return Err(syn::Error::new_spanned(
                &argument.pat,
                "HTTP API parameter patterns must be identifiers",
            ));
        };
        if pattern.by_ref.is_some() || pattern.subpat.is_some() {
            return Err(syn::Error::new_spanned(
                pattern,
                "HTTP API parameter patterns must be plain identifiers",
            ));
        }
        call_arguments.push(pattern.ident.clone());
    }
    let mut alias = format_ident!("__http_business_function");
    while call_arguments.contains(&alias) {
        alias = format_ident!("{}_", alias);
    }
    let expanded = expand_adapter(&registry, &input, &route, method, &adapter, &alias)?;
    for argument in &mut input.sig.inputs {
        if let FnArg::Typed(argument) = argument {
            argument
                .attrs
                .retain(|attr| !attr.path().is_ident("inject"));
        }
    }
    let cfg = input
        .attrs
        .iter()
        .map(|attr| gating_attribute(&attr.meta))
        .collect::<syn::Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .map(|meta| quote!(#[#meta]));
    Ok(quote! {
        #[allow(unknown_lints, clippy::unused_async, clippy::unused_async_trait_impl)]
        #input

        #(#cfg)*
        const _: () = {
            use self::#name as #alias;
            #[allow(non_camel_case_types)]
            struct #adapter {
                __http_dependencies: #registry::State,
            }

            #expanded
        };
    })
}

// Only propagate conditional existence. Other cfg_attr contents (e.g. inline)
// belong to the business function and may be invalid on the generated const.
fn gating_attribute(meta: &syn::Meta) -> syn::Result<Option<syn::Meta>> {
    use syn::parse::Parser;
    if meta.path().is_ident("cfg") {
        return Ok(Some(meta.clone()));
    }
    let syn::Meta::List(list) = meta else {
        return Ok(None);
    };
    if !list.path.is_ident("cfg_attr") {
        return Ok(None);
    }
    let entries = syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated
        .parse2(list.tokens.clone())?;
    let mut entries = entries.into_iter();
    let condition = entries
        .next()
        .ok_or_else(|| syn::Error::new_spanned(meta, "cfg_attr requires a condition"))?;
    let attributes = entries
        .map(|entry| gating_attribute(&entry))
        .collect::<syn::Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    Ok((!attributes.is_empty()).then(|| syn::parse_quote!(cfg_attr(#condition, #(#attributes),*))))
}

pub(super) fn injection(argument: &PatType) -> syn::Result<Option<Ident>> {
    let mut dependency = None;
    for attr in argument
        .attrs
        .iter()
        .filter(|attr| attr.path().is_ident("inject"))
    {
        if dependency.is_some() {
            return Err(syn::Error::new_spanned(
                attr,
                "specify exactly one #[inject(name)] per parameter",
            ));
        }
        dependency =
            Some(attr.parse_args::<Ident>().map_err(|_| {
                syn::Error::new_spanned(attr, "expected #[inject(dependency_name)]")
            })?);
    }
    if dependency.is_some()
        && let Type::Reference(reference) = argument.ty.as_ref()
        && reference.mutability.is_some()
    {
        return Err(syn::Error::new_spanned(
            &argument.ty,
            "injected dependencies are shared; use &T or an owned Clone type with interior mutability",
        ));
    }
    Ok(dependency)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_ambiguous_or_unsupported_function_declarations() {
        for (route, source, expected) in [
            (
                quote!("/"),
                quote!(
                    async fn read(#[inject] state: &str) {}
                ),
                "expected #[inject(dependency_name)]",
            ),
            (
                quote!("/"),
                quote!(
                    async fn read(
                        #[inject(a)]
                        #[inject(b)]
                        state: &str,
                    ) {
                    }
                ),
                "exactly one",
            ),
            (
                quote!("/"),
                quote!(
                    async fn read(#[inject(a)] state: &mut String) {}
                ),
                "dependencies are shared",
            ),
            (
                quote!("/", headers(state = "x-state")),
                quote!(
                    async fn read(#[inject(a)] state: &str) {}
                ),
                "cannot also bind",
            ),
            (
                quote!("/"),
                quote!(
                    fn read() {}
                ),
                "must be async",
            ),
            (
                quote!("/"),
                quote!(
                    async fn read<T>() {}
                ),
                "not type or const",
            ),
            (
                quote!("/"),
                quote!(
                    async fn read(&self) {}
                ),
                "free function",
            ),
            (
                quote!("/"),
                quote!(
                    #[post("/other")]
                    async fn read() {}
                ),
                "one HTTP route attribute",
            ),
            (
                quote!("/"),
                quote!(
                    async unsafe fn read() {}
                ),
                "safe Rust signature",
            ),
            (
                quote!("/"),
                quote!(
                    async fn read((a, b): (u32, u32)) {}
                ),
                "patterns must be identifiers",
            ),
            (
                quote!("/", prefix = "/v1"),
                quote!(
                    async fn read() {}
                ),
                "supported route options",
            ),
        ] {
            let input = syn::parse2(source).unwrap();
            let error = expand("GET", &route, input).unwrap_err().to_string();
            assert!(
                error.contains(expected),
                "expected {expected}, received {error}"
            );
        }
    }
}
