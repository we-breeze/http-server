use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::ext::IdentExt;
use syn::{Data, DeriveInput, Fields, parse_macro_input};

use crate::expand::server_crate_path;

pub(crate) fn expand(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_derive(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand_derive(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "FromState can only be derived for a struct with one named state field",
        ));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "FromState requires a named state field; implement FromState manually for tuple or unit structs",
        ));
    };
    let field = fields
        .named
        .iter()
        .find(|field| field.ident.as_ref().is_some_and(|name| name.unraw() == "state"))
        .ok_or_else(|| syn::Error::new_spanned(
            fields,
            "FromState requires a field named state; implement FromState manually for other layouts",
        ))?;
    if fields.named.len() != 1 {
        return Err(syn::Error::new_spanned(
            fields,
            "FromState requires state to be the only field; implement FromState manually to initialize additional fields",
        ));
    }

    let server = server_crate_path();
    let name = &input.ident;
    let state = &field.ty;
    let field_name = &field.ident;
    let mut generics = input.generics.clone();
    generics
        .make_where_clause()
        .predicates
        .push(syn::parse_quote!(#state: ::core::clone::Clone));
    let (impl_generics, type_generics, where_clause) = generics.split_for_impl();

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics #server::FromState<#state> for #name #type_generics #where_clause {
            fn from_state(state: &#state) -> Self {
                Self { #field_name: <#state as ::core::clone::Clone>::clone(state) }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_layouts_produce_actionable_errors() {
        for (source, expected) in [
            ("enum Api { State(u32) }", "only be derived for a struct"),
            ("union Api { state: u32 }", "only be derived for a struct"),
            ("struct Api;", "named state field"),
            ("struct Api(u32);", "named state field"),
            ("struct Api {}", "field named state"),
            ("struct Api { context: u32 }", "field named state"),
            ("struct Api { state: u32, extra: bool }", "only field"),
        ] {
            let input = syn::parse_str(source).unwrap();
            let error = expand_derive(&input).unwrap_err();
            assert!(error.to_string().contains(expected), "{source}: {error}");
        }
    }
}
