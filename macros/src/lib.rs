//! Static API adapter generation for `http-server`.

use std::collections::{BTreeMap, BTreeSet};

use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{
    Attribute, FnArg, Ident, ImplItem, ImplItemFn, ItemImpl, LitStr, Pat, ReturnType, Token, Type,
    parse_macro_input,
};

/// Generates the internal `Handler` implementation for one business API type.
#[proc_macro_attribute]
pub fn api(arguments: TokenStream, input: TokenStream) -> TokenStream {
    let arguments = parse_macro_input!(arguments as ApiArguments);
    let input = parse_macro_input!(input as ItemImpl);
    expand_api(&arguments, input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Marker consumed by [`api`]. It is a pass-through when used outside an API
/// implementation so diagnostics remain local to the caller.
#[proc_macro_attribute]
pub fn get(_arguments: TokenStream, input: TokenStream) -> TokenStream {
    input
}

/// Marker consumed by [`api`].
#[proc_macro_attribute]
pub fn post(_arguments: TokenStream, input: TokenStream) -> TokenStream {
    input
}

/// Marker consumed by [`api`].
#[proc_macro_attribute]
pub fn put(_arguments: TokenStream, input: TokenStream) -> TokenStream {
    input
}

/// Marker consumed by [`api`].
#[proc_macro_attribute]
pub fn patch(_arguments: TokenStream, input: TokenStream) -> TokenStream {
    input
}

/// Marker consumed by [`api`].
#[proc_macro_attribute]
pub fn delete(_arguments: TokenStream, input: TokenStream) -> TokenStream {
    input
}

struct ApiArguments {
    prefix: String,
    consumes: Codec,
    produces: Codec,
    auth: AuthMode,
}

impl Parse for ApiArguments {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut prefix = String::new();
        let mut consumes = Codec::Json;
        let mut produces = Codec::Json;
        let mut auth = AuthMode::None;
        while !input.is_empty() {
            let name: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            if name == "prefix" {
                prefix = input.parse::<LitStr>()?.value();
            } else if name == "consumes" {
                consumes = input.parse()?;
            } else if name == "produces" {
                produces = input.parse()?;
            } else if name == "auth" {
                auth = input.parse()?;
            } else {
                return Err(syn::Error::new_spanned(
                    name,
                    "supported api options are prefix, consumes, produces, and auth",
                ));
            }
            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }
        validate_prefix(&prefix, input.span())?;
        Ok(Self {
            prefix: normalize_prefix(prefix),
            consumes,
            produces,
            auth,
        })
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Codec {
    Json,
    Protobuf,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum AuthMode {
    None,
    Optional,
    Required,
}

impl Parse for AuthMode {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let value: Ident = input.parse()?;
        match value.to_string().as_str() {
            "none" => Ok(Self::None),
            "optional" => Ok(Self::Optional),
            "required" => Ok(Self::Required),
            _ => Err(syn::Error::new_spanned(
                value,
                "supported auth modes are none, optional, and required",
            )),
        }
    }
}

impl Parse for Codec {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let value: Ident = input.parse()?;
        match value.to_string().as_str() {
            "json" => Ok(Self::Json),
            "protobuf" => Ok(Self::Protobuf),
            _ => Err(syn::Error::new_spanned(
                value,
                "supported codecs are json and protobuf",
            )),
        }
    }
}

struct RouteArguments {
    path: LitStr,
    consumes: Option<Codec>,
    produces: Option<Codec>,
    auth: Option<AuthMode>,
    headers: BTreeMap<String, LitStr>,
}

impl Parse for RouteArguments {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let path = input.parse()?;
        let mut consumes = None;
        let mut produces = None;
        let mut auth = None;
        let mut headers = BTreeMap::new();
        while !input.is_empty() {
            input.parse::<Token![,]>()?;
            let name: Ident = input.parse()?;
            if name == "headers" {
                let content;
                syn::parenthesized!(content in input);
                while !content.is_empty() {
                    let parameter: Ident = content.parse()?;
                    content.parse::<Token![=]>()?;
                    let header: LitStr = content.parse()?;
                    if headers.insert(parameter.to_string(), header).is_some() {
                        return Err(syn::Error::new_spanned(
                            parameter,
                            "a parameter may only bind one HTTP header",
                        ));
                    }
                    if content.is_empty() {
                        break;
                    }
                    content.parse::<Token![,]>()?;
                }
            } else {
                input.parse::<Token![=]>()?;
                if name == "consumes" {
                    consumes = Some(input.parse()?);
                } else if name == "produces" {
                    produces = Some(input.parse()?);
                } else if name == "auth" {
                    auth = Some(input.parse()?);
                } else {
                    return Err(syn::Error::new_spanned(
                        name,
                        "supported route options are consumes, produces, auth, and headers(...)",
                    ));
                }
            }
        }
        Ok(Self {
            path,
            consumes,
            produces,
            auth,
            headers,
        })
    }
}

struct Endpoint {
    method: &'static str,
    path: String,
    shape: String,
    handler: Ident,
    parameters: Vec<Parameter>,
    result: ResultKind,
    has_json_body: bool,
    auth: AuthMode,
}

struct Parameter {
    ident: Ident,
    ty: Type,
    source: ParameterSource,
}

enum ParameterSource {
    Path(usize),
    Query {
        key: String,
        optional_inner: Option<Type>,
    },
    Header {
        name: LitStr,
        optional_inner: Option<Type>,
    },
    Authenticated,
    JsonBody,
}

#[derive(Clone, Copy)]
enum ResultKind {
    Value,
    Result,
}

#[allow(clippy::too_many_lines)]
fn expand_api(arguments: &ApiArguments, mut input: ItemImpl) -> syn::Result<TokenStream2> {
    if input.trait_.is_some() {
        return Err(syn::Error::new_spanned(
            &input,
            "http_server::api requires an inherent impl",
        ));
    }
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "generic HTTP API impls are not supported",
        ));
    }

    let mut endpoints = Vec::new();
    for item in &mut input.items {
        let ImplItem::Fn(method) = item else {
            continue;
        };
        let Some((route_method, attribute_index)) = route_attribute(&method.attrs) else {
            continue;
        };
        let attribute = method.attrs.remove(attribute_index);
        let route = attribute.parse_args::<RouteArguments>()?;
        let consumes = route.consumes.unwrap_or(arguments.consumes);
        let produces = route.produces.unwrap_or(arguments.produces);
        let auth = route.auth.unwrap_or(arguments.auth);
        if consumes != Codec::Json || produces != Codec::Json {
            return Err(syn::Error::new_spanned(
                attribute,
                "protobuf API codecs are declared but not implemented yet; use json",
            ));
        }
        endpoints.push(parse_endpoint(
            route_method,
            &arguments.prefix,
            method,
            &route,
            auth,
        )?);
    }
    if endpoints.is_empty() {
        return Err(syn::Error::new_spanned(
            &input,
            "http_server::api requires at least one #[get], #[post], #[put], #[patch], or #[delete] method",
        ));
    }

    let groups = group_endpoints(endpoints)?;
    let server = server_crate_path();
    let self_ty = &input.self_ty;
    let route_groups = groups.iter().map(|group| expand_group(group, &server));
    let principal = authentication_principal(&groups)?;
    let handler_impl = if let Some(principal) = principal {
        quote! {
            impl<__HttpAuthenticator> #server::Handler<__HttpAuthenticator> for #self_ty
            where
                __HttpAuthenticator: #server::Authenticator<Principal = #principal>,
        }
    } else {
        quote! {
            impl<__HttpAuthenticator> #server::Handler<__HttpAuthenticator> for #self_ty
            where
                __HttpAuthenticator: #server::Authenticator,
        }
    };

    Ok(quote! {
        #input

        #handler_impl {
            fn call<'a>(
                &'a self,
                __http_request: #server::Request<'a>,
                __http_authenticator: &'a __HttpAuthenticator,
            ) -> impl ::core::future::Future<Output = #server::Response> + Send + 'a {
                async move {
                    let __http_method = __http_request.method();
                    let __http_path = __http_request.path();
                    #(#route_groups)*
                    #server::Response::empty(#server::StatusCode::new(404))
                }
            }
        }
    })
}

fn route_attribute(attributes: &[Attribute]) -> Option<(&'static str, usize)> {
    attributes
        .iter()
        .enumerate()
        .find_map(|(index, attribute)| {
            let name = attribute.path().segments.last()?.ident.to_string();
            let method = match name.as_str() {
                "get" => "GET",
                "post" => "POST",
                "put" => "PUT",
                "patch" => "PATCH",
                "delete" => "DELETE",
                _ => return None,
            };
            Some((method, index))
        })
}

#[allow(clippy::too_many_lines)]
fn parse_endpoint(
    method: &'static str,
    prefix: &str,
    function: &ImplItemFn,
    route: &RouteArguments,
    auth: AuthMode,
) -> syn::Result<Endpoint> {
    if function.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            function.sig.fn_token,
            "HTTP API methods must be async",
        ));
    }
    if function
        .sig
        .generics
        .params
        .iter()
        .any(|parameter| !matches!(parameter, syn::GenericParam::Lifetime(_)))
    {
        return Err(syn::Error::new_spanned(
            &function.sig.generics,
            "HTTP API methods may have lifetime parameters but not type or const parameters",
        ));
    }
    let endpoint_path = route.path.value();
    validate_endpoint_path(&endpoint_path, &route.path)?;
    let path = join_path(prefix, &endpoint_path);
    let captures = parse_captures(&path, &route.path)?;
    let shape = route_shape(&path);

    let mut inputs = function.sig.inputs.iter();
    match inputs.next() {
        Some(FnArg::Receiver(receiver)) if receiver.reference.is_some() => {}
        _ => {
            return Err(syn::Error::new_spanned(
                &function.sig.inputs,
                "HTTP API methods must begin with &self",
            ));
        }
    }
    let mut parameters = Vec::new();
    for argument in inputs {
        let FnArg::Typed(argument) = argument else {
            return Err(syn::Error::new_spanned(
                argument,
                "HTTP API methods may only have one self receiver",
            ));
        };
        let Pat::Ident(pattern) = argument.pat.as_ref() else {
            return Err(syn::Error::new_spanned(
                &argument.pat,
                "HTTP API parameter patterns must be identifiers",
            ));
        };
        parameters.push(Parameter {
            ident: pattern.ident.clone(),
            ty: (*argument.ty).clone(),
            source: ParameterSource::JsonBody,
        });
    }
    if parameters.len() < captures.len() {
        return Err(syn::Error::new_spanned(
            &function.sig.inputs,
            "route has more path captures than method parameters",
        ));
    }
    for (index, capture) in captures.iter().enumerate() {
        if parameters[index].ident != capture.as_str() {
            return Err(syn::Error::new_spanned(
                &parameters[index].ident,
                format!("path capture :{capture} must bind parameter {capture} in route order"),
            ));
        }
        parameters[index].source = ParameterSource::Path(index);
    }

    let parameter_names: BTreeSet<_> = parameters
        .iter()
        .map(|parameter| parameter.ident.to_string())
        .collect();
    for name in route.headers.keys() {
        if !parameter_names.contains(name) {
            return Err(syn::Error::new_spanned(
                &route.path,
                format!("headers(...) binds unknown parameter {name}"),
            ));
        }
    }
    let mut body_index = None;
    let mut authentication_parameter = None;
    for parameter in parameters.iter_mut().skip(captures.len()) {
        if authenticated_inner(&parameter.ty).is_some() {
            if route.headers.contains_key(&parameter.ident.to_string()) {
                return Err(syn::Error::new_spanned(
                    &parameter.ident,
                    "Authenticated<T> is injected by auth, not headers(...) ",
                ));
            }
            if authentication_parameter
                .replace(parameter.ident.clone())
                .is_some()
            {
                return Err(syn::Error::new_spanned(
                    &parameter.ident,
                    "an HTTP API method may have at most one Authenticated<T> parameter",
                ));
            }
            parameter.source = ParameterSource::Authenticated;
        } else if let Some(name) = route.headers.get(&parameter.ident.to_string()) {
            parameter.source = ParameterSource::Header {
                name: name.clone(),
                optional_inner: option_inner(&parameter.ty),
            };
        } else if is_scalar(&parameter.ty) {
            parameter.source = ParameterSource::Query {
                key: parameter.ident.to_string(),
                optional_inner: option_inner(&parameter.ty),
            };
        } else if body_index.replace(parameter.ident.clone()).is_some() {
            return Err(syn::Error::new_spanned(
                &parameter.ident,
                "an HTTP API method may have at most one non-scalar JSON body parameter",
            ));
        }
    }
    validate_auth_parameters(auth, &parameters, authentication_parameter.as_ref())?;
    let result = result_kind(&function.sig.output)?;
    Ok(Endpoint {
        method,
        path,
        shape,
        handler: function.sig.ident.clone(),
        parameters,
        result,
        has_json_body: body_index.is_some(),
        auth,
    })
}

fn validate_auth_parameters(
    auth: AuthMode,
    parameters: &[Parameter],
    authentication_parameter: Option<&Ident>,
) -> syn::Result<()> {
    let Some(parameter) = parameters
        .iter()
        .find(|parameter| matches!(parameter.source, ParameterSource::Authenticated))
    else {
        return Ok(());
    };
    let Some((_, optional)) = authenticated_inner(&parameter.ty) else {
        unreachable!("authenticated parameter source has an authenticated type");
    };
    match auth {
        AuthMode::None => Err(syn::Error::new_spanned(
            &parameter.ty,
            "Authenticated<T> requires auth = required or auth = optional",
        )),
        AuthMode::Required if optional => Err(syn::Error::new_spanned(
            &parameter.ty,
            "auth = required injects Authenticated<T>, not Option<Authenticated<T>>",
        )),
        AuthMode::Optional if !optional => Err(syn::Error::new_spanned(
            &parameter.ty,
            "auth = optional injects Option<Authenticated<T>>",
        )),
        AuthMode::Required | AuthMode::Optional => {
            debug_assert_eq!(authentication_parameter, Some(&parameter.ident));
            Ok(())
        }
    }
}

fn authentication_principal(groups: &[RouteGroup]) -> syn::Result<Option<Type>> {
    let mut principal = None;
    let mut principal_key = None;
    for endpoint in groups.iter().flat_map(|group| &group.endpoints) {
        for parameter in &endpoint.parameters {
            if !matches!(parameter.source, ParameterSource::Authenticated) {
                continue;
            }
            let Some((candidate, _)) = authenticated_inner(&parameter.ty) else {
                unreachable!("authenticated parameter source has an authenticated type");
            };
            let candidate_key = quote!(#candidate).to_string();
            if let Some(existing) = &principal_key
                && existing != &candidate_key
            {
                return Err(syn::Error::new_spanned(
                    &parameter.ty,
                    "one #[api] impl may inject only one authenticated principal type",
                ));
            }
            principal_key = Some(candidate_key);
            principal = Some(candidate);
        }
    }
    Ok(principal)
}

struct RouteGroup {
    path: String,
    specificity: usize,
    endpoints: Vec<Endpoint>,
}

fn group_endpoints(endpoints: Vec<Endpoint>) -> syn::Result<Vec<RouteGroup>> {
    let mut shapes = BTreeMap::<String, String>::new();
    let mut groups = BTreeMap::<String, Vec<Endpoint>>::new();
    for endpoint in endpoints {
        if let Some(existing) = shapes.insert(endpoint.shape.clone(), endpoint.path.clone())
            && existing != endpoint.path
        {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                format!(
                    "ambiguous route templates {existing} and {}; rename captures consistently",
                    endpoint.path
                ),
            ));
        }
        groups
            .entry(endpoint.path.clone())
            .or_default()
            .push(endpoint);
    }
    let mut result = Vec::with_capacity(groups.len());
    for (path, mut endpoints) in groups {
        endpoints.sort_by_key(|endpoint| endpoint.method);
        for pair in endpoints.windows(2) {
            if pair[0].method == pair[1].method {
                return Err(syn::Error::new(
                    proc_macro2::Span::call_site(),
                    format!("duplicate {} route {path}", pair[0].method),
                ));
            }
        }
        let specificity = path
            .split('/')
            .filter(|segment| !segment.starts_with(':'))
            .count();
        result.push(RouteGroup {
            path,
            specificity,
            endpoints,
        });
    }
    result.sort_by(|left, right| {
        right
            .specificity
            .cmp(&left.specificity)
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(result)
}

fn expand_group(group: &RouteGroup, server: &TokenStream2) -> TokenStream2 {
    let path = LitStr::new(&group.path, proc_macro2::Span::call_site());
    let allow = LitStr::new(
        &group
            .endpoints
            .iter()
            .map(|endpoint| endpoint.method)
            .collect::<Vec<_>>()
            .join(", "),
        proc_macro2::Span::call_site(),
    );
    let method_arms = group.endpoints.iter().map(|endpoint| {
        let method = LitStr::new(endpoint.method, proc_macro2::Span::call_site());
        let bindings = endpoint
            .parameters
            .iter()
            .map(|parameter| expand_parameter(parameter, server));
        let arguments = endpoint.parameters.iter().map(|parameter| &parameter.ident);
        let handler = &endpoint.handler;
        let authentication = expand_authentication(endpoint, server);
        let content_type_check = endpoint.has_json_body.then(|| {
            quote! {
                if !#server::__private::is_json_content_type(
                    __http_request.header("content-type"),
                ) {
                    return #server::Response::empty(#server::StatusCode::new(415));
                }
            }
        });
        let response = match endpoint.result {
            ResultKind::Value => quote! {
                #server::__private::json_response(self.#handler(#(#arguments),*).await, __http_request.response_arena())
            },
            ResultKind::Result => quote! {
                #server::__private::json_result_response(self.#handler(#(#arguments),*).await, __http_request.response_arena())
            },
        };
        quote! {
            #method => {
                #authentication
                #content_type_check
                #(#bindings)*
                return #response;
            }
        }
    });
    quote! {
        if let Some(__http_match) = #server::__private::match_route(__http_path, #path) {
            match __http_method {
                #(#method_arms,)*
                _ => return #server::Response::method_not_allowed(#allow),
            }
        }
    }
}

fn expand_authentication(endpoint: &Endpoint, server: &TokenStream2) -> TokenStream2 {
    let authentication_parameter = endpoint
        .parameters
        .iter()
        .find(|parameter| matches!(parameter.source, ParameterSource::Authenticated));
    let request = quote! {
        #server::AuthRequest::from_request(&__http_request)
    };
    match (endpoint.auth, authentication_parameter) {
        (AuthMode::None, None) => quote! {},
        (AuthMode::None, Some(_)) => unreachable!("auth parameters are validated during parsing"),
        (AuthMode::Required, Some(parameter)) => {
            let ident = &parameter.ident;
            let ty = &parameter.ty;
            quote! {
                let #ident: #ty = match #server::__private::authenticate_required(
                    __http_authenticator,
                    #request,
                )
                .await
                {
                    Ok(value) => value,
                    Err(response) => return response,
                };
            }
        }
        (AuthMode::Optional, Some(parameter)) => {
            let ident = &parameter.ident;
            let ty = &parameter.ty;
            quote! {
                let #ident: #ty = match #server::__private::authenticate_optional(
                    __http_authenticator,
                    #request,
                )
                .await
                {
                    Ok(value) => value,
                    Err(response) => return response,
                };
            }
        }
        (AuthMode::Required, None) => quote! {
            if let Err(response) = #server::__private::authenticate_required(
                __http_authenticator,
                #request,
            )
            .await
            {
                return response;
            }
        },
        (AuthMode::Optional, None) => quote! {
            if let Err(response) = #server::__private::authenticate_optional(
                __http_authenticator,
                #request,
            )
            .await
            {
                return response;
            }
        },
    }
}

fn expand_parameter(parameter: &Parameter, server: &TokenStream2) -> TokenStream2 {
    let ident = &parameter.ident;
    let ty = &parameter.ty;
    let failure = quote! { return #server::Response::empty(#server::StatusCode::BAD_REQUEST); };
    match &parameter.source {
        ParameterSource::Path(index) => {
            quote! {
                let #ident: #ty = match #server::__private::path(
                    __http_match.capture(#index).expect("route capture is present"),
                ) {
                    Ok(value) => value,
                    Err(_) => { #failure }
                };
            }
        }
        ParameterSource::Query {
            key,
            optional_inner,
        } => {
            let key = LitStr::new(key, proc_macro2::Span::call_site());
            if let Some(inner) = optional_inner {
                quote! {
                    let #ident: #ty = match #server::__private::query_optional::<#inner>(
                        __http_request.query(), #key,
                    ) {
                        Ok(value) => value,
                        Err(_) => { #failure }
                    };
                }
            } else {
                quote! {
                    let #ident: #ty = match #server::__private::query_required::<#ty>(
                        __http_request.query(), #key,
                    ) {
                        Ok(value) => value,
                        Err(_) => { #failure }
                    };
                }
            }
        }
        ParameterSource::Header {
            name,
            optional_inner,
        } => {
            if let Some(inner) = optional_inner {
                quote! {
                    let #ident: #ty = match #server::__private::header_optional::<#inner>(
                        __http_request.header(#name),
                    ) {
                        Ok(value) => value,
                        Err(_) => { #failure }
                    };
                }
            } else {
                quote! {
                    let #ident: #ty = match #server::__private::header_required::<#ty>(
                        __http_request.header(#name),
                    ) {
                        Ok(value) => value,
                        Err(_) => { #failure }
                    };
                }
            }
        }
        ParameterSource::Authenticated => quote! {},
        ParameterSource::JsonBody => quote! {
            let #ident: #ty = match #server::__private::json_body(__http_request.body()) {
                Ok(value) => value,
                Err(_) => { #failure }
            };
        },
    }
}

fn result_kind(output: &ReturnType) -> syn::Result<ResultKind> {
    let ReturnType::Type(_, ty) = output else {
        return Err(syn::Error::new_spanned(
            output,
            "HTTP API methods must return a JSON-serializable value or ApiResult<T>",
        ));
    };
    let Type::Path(path) = ty.as_ref() else {
        return Ok(ResultKind::Value);
    };
    let Some(segment) = path.path.segments.last() else {
        return Ok(ResultKind::Value);
    };
    Ok(match segment.ident.to_string().as_str() {
        "Result" | "ApiResult" => ResultKind::Result,
        _ => ResultKind::Value,
    })
}

fn option_inner(ty: &Type) -> Option<Type> {
    let Type::Path(path) = ty else {
        return None;
    };
    let segment = path.path.segments.last()?;
    if segment.ident != "Option" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    match arguments.args.first()? {
        syn::GenericArgument::Type(inner) => Some(inner.clone()),
        _ => None,
    }
}

fn authenticated_inner(ty: &Type) -> Option<(Type, bool)> {
    if let Some(inner) = option_inner(ty) {
        let (principal, false) = authenticated_inner(&inner)? else {
            return None;
        };
        return Some((principal, true));
    }
    let Type::Path(path) = ty else {
        return None;
    };
    let segment = path.path.segments.last()?;
    if segment.ident != "Authenticated" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    let syn::GenericArgument::Type(principal) = arguments.args.first()? else {
        return None;
    };
    Some((principal.clone(), false))
}

fn is_scalar(ty: &Type) -> bool {
    if let Some(inner) = option_inner(ty) {
        return is_scalar(&inner);
    }
    if let Type::Reference(reference) = ty
        && let Type::Path(path) = reference.elem.as_ref()
    {
        return path.path.is_ident("str");
    }
    let Type::Path(path) = ty else {
        return false;
    };
    let Some(segment) = path.path.segments.last() else {
        return false;
    };
    matches!(
        segment.ident.to_string().as_str(),
        "String"
            | "bool"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "f32"
            | "f64"
    )
}

fn parse_captures(path: &str, span: impl quote::ToTokens) -> syn::Result<Vec<String>> {
    let mut captures = Vec::new();
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        let Some(name) = segment.strip_prefix(':') else {
            continue;
        };
        if name.is_empty() || syn::parse_str::<Ident>(name).is_err() {
            return Err(syn::Error::new_spanned(
                &span,
                "path captures must use Rust identifier names, e.g. :user_id",
            ));
        }
        if captures.iter().any(|capture| capture == name) {
            return Err(syn::Error::new_spanned(
                &span,
                "a route may not repeat a path capture name",
            ));
        }
        captures.push(name.to_owned());
    }
    if captures.len() > 8 {
        return Err(syn::Error::new_spanned(
            &span,
            "routes support at most eight path captures",
        ));
    }
    Ok(captures)
}

fn validate_prefix(prefix: &str, span: proc_macro2::Span) -> syn::Result<()> {
    if prefix.is_empty() || prefix == "/" {
        return Ok(());
    }
    if !prefix.starts_with('/') || prefix.ends_with('/') {
        return Err(syn::Error::new(
            span,
            "api prefix must be empty or start with / and have no trailing /",
        ));
    }
    Ok(())
}

fn normalize_prefix(prefix: String) -> String {
    if prefix == "/" { String::new() } else { prefix }
}

fn validate_endpoint_path(path: &str, span: impl quote::ToTokens) -> syn::Result<()> {
    if !path.starts_with('/') {
        return Err(syn::Error::new_spanned(
            span,
            "endpoint paths must start with /",
        ));
    }
    if path.len() > 1 && path.ends_with('/') {
        return Err(syn::Error::new_spanned(
            span,
            "endpoint paths must not have a trailing /",
        ));
    }
    Ok(())
}

fn join_path(prefix: &str, path: &str) -> String {
    if prefix.is_empty() {
        path.to_owned()
    } else if path == "/" {
        prefix.to_owned()
    } else {
        format!("{prefix}{path}")
    }
}

fn route_shape(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            if segment.starts_with(':') {
                ":"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn server_crate_path() -> TokenStream2 {
    match crate_name("http-server") {
        Ok(FoundCrate::Itself) => quote!(crate),
        Ok(FoundCrate::Name(name)) => {
            let ident = Ident::new(&name.replace('-', "_"), proc_macro2::Span::call_site());
            quote!(::#ident)
        }
        Err(_) => quote!(::http_server),
    }
}
