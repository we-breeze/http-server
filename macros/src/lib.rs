//! Static API adapter generation for `http-server`.
use proc_macro::TokenStream;

mod expand;
mod registry;

/// Declares an application registry with concrete state and authenticator types.
#[proc_macro]
pub fn registry(input: TokenStream) -> TokenStream {
    registry::declare(input)
}

/// Builds registered API instances from application state once at startup.
#[proc_macro]
pub fn handlers(input: TokenStream) -> TokenStream {
    registry::collect(input)
}

/// Generates the internal `Handler` implementation for one business API type.
///
/// APIs register in the default group unless `group` selects another group or
/// `register = false` opts out. Registered APIs implement `FromState` for the
/// group's state type.
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
