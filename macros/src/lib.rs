//! Static API adapter generation for `http-server`.
use proc_macro::TokenStream;

mod expand;

/// Generates the internal `Handler` implementation for one business API type.
#[proc_macro_attribute]
pub fn api(arguments: TokenStream, input: TokenStream) -> TokenStream {
    expand::expand(arguments, input)
}

/// Route marker consumed by [`api`].
#[proc_macro_attribute]
pub fn get(_arguments: TokenStream, input: TokenStream) -> TokenStream {
    input
}

/// Route marker consumed by [`api`].
#[proc_macro_attribute]
pub fn post(_arguments: TokenStream, input: TokenStream) -> TokenStream {
    input
}

/// Route marker consumed by [`api`].
#[proc_macro_attribute]
pub fn put(_arguments: TokenStream, input: TokenStream) -> TokenStream {
    input
}

/// Route marker consumed by [`api`].
#[proc_macro_attribute]
pub fn patch(_arguments: TokenStream, input: TokenStream) -> TokenStream {
    input
}

/// Route marker consumed by [`api`].
#[proc_macro_attribute]
pub fn delete(_arguments: TokenStream, input: TokenStream) -> TokenStream {
    input
}

/// Route marker consumed by [`api`].
#[proc_macro_attribute]
pub fn head(_arguments: TokenStream, input: TokenStream) -> TokenStream {
    input
}

/// Route marker consumed by [`api`].
#[proc_macro_attribute]
pub fn options(_arguments: TokenStream, input: TokenStream) -> TokenStream {
    input
}
