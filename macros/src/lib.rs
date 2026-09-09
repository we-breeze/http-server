//! Static API adapter generation for `http-server`.
use proc_macro::TokenStream;

mod expand;
mod registry;

/// Declares an application registry with named dependency types and a concrete authenticator.
#[proc_macro]
pub fn registry(input: TokenStream) -> TokenStream {
    registry::declare(input)
}

/// Collects function APIs with named dependencies once at startup.
#[proc_macro]
pub fn handlers(input: TokenStream) -> TokenStream {
    registry::collect(input)
}

/// Exports an async free function with static HTTP adaptation.
/// Free functions use `#[inject(name)]` for named registry dependencies.
#[proc_macro_attribute]
pub fn get(arguments: TokenStream, input: TokenStream) -> TokenStream {
    expand::expand_function("GET", arguments, input)
}

/// Exports an async free function with static HTTP adaptation.
/// Free functions use `#[inject(name)]` for named registry dependencies.
#[proc_macro_attribute]
pub fn post(arguments: TokenStream, input: TokenStream) -> TokenStream {
    expand::expand_function("POST", arguments, input)
}

/// Exports an async free function with static HTTP adaptation.
/// Free functions use `#[inject(name)]` for named registry dependencies.
#[proc_macro_attribute]
pub fn put(arguments: TokenStream, input: TokenStream) -> TokenStream {
    expand::expand_function("PUT", arguments, input)
}

/// Exports an async free function with static HTTP adaptation.
/// Free functions use `#[inject(name)]` for named registry dependencies.
#[proc_macro_attribute]
pub fn patch(arguments: TokenStream, input: TokenStream) -> TokenStream {
    expand::expand_function("PATCH", arguments, input)
}

/// Exports an async free function with static HTTP adaptation.
/// Free functions use `#[inject(name)]` for named registry dependencies.
#[proc_macro_attribute]
pub fn delete(arguments: TokenStream, input: TokenStream) -> TokenStream {
    expand::expand_function("DELETE", arguments, input)
}

/// Exports an async free function with static HTTP adaptation.
/// Free functions use `#[inject(name)]` for named registry dependencies.
#[proc_macro_attribute]
pub fn head(arguments: TokenStream, input: TokenStream) -> TokenStream {
    expand::expand_function("HEAD", arguments, input)
}

/// Exports an async free function with static HTTP adaptation.
/// Free functions use `#[inject(name)]` for named registry dependencies.
#[proc_macro_attribute]
pub fn options(arguments: TokenStream, input: TokenStream) -> TokenStream {
    expand::expand_function("OPTIONS", arguments, input)
}
