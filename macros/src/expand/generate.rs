use super::{
    AuthMode, Endpoint, LitStr, Parameter, ParameterSource, ResultKind, RouteGroup, TokenStream2,
    method_bit, quote,
};

pub(super) fn expand_group(group: &RouteGroup, server: &TokenStream2) -> TokenStream2 {
    let path = LitStr::new(&group.path, proc_macro2::Span::call_site());
    let methods = group
        .endpoints
        .iter()
        .fold(0, |bits, endpoint| bits | method_bit(endpoint.method));
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
                    return __http_request.reject(#server::Rejection::new(#server::RejectionKind::ContentType, "body", None, "expected application/json"));
                }
            }
        });
        let response = match endpoint.result {
            ResultKind::Value => quote! {
                #server::__private::response(self.#handler(#(#arguments),*).await, __http_request.response_arena())
            },
            ResultKind::Result => quote! {
                #server::__private::result_response(self.#handler(#(#arguments),*).await, __http_request.response_arena())
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
            __http_allow |= #methods;
            match __http_method {
                #(#method_arms,)*
                _ => {},
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
                    __http_request.response_arena(),
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
                    __http_request.response_arena(),
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
                __http_request.response_arena(),
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
                __http_request.response_arena(),
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
    let name = LitStr::new(&ident.to_string(), proc_macro2::Span::call_site());
    let (kind, input) = match &parameter.source {
        ParameterSource::Path(index) => (quote! { Path }, quote! { __http_match.capture(#index) }),
        ParameterSource::Query { .. }
        | ParameterSource::QueryMany(_)
        | ParameterSource::QueryObject => (quote! { Query }, quote! { __http_query.get(#name) }),
        ParameterSource::Header { name, .. } => (
            quote! { Header },
            quote! { __http_request.header(#name).and_then(|value| ::core::str::from_utf8(value).ok()) },
        ),
        _ => (
            quote! { Body },
            quote! { ::core::str::from_utf8(__http_request.body()).ok() },
        ),
    };
    let failure = quote! { return __http_request.reject(#server::Rejection::new(#server::RejectionKind::#kind, #name, #input, error.to_string())); };
    match &parameter.source {
        ParameterSource::Path(index) => {
            quote! {
                let #ident: #ty = match #server::__private::path(
                    __http_match.capture(#index).expect("route capture is present"),
                ) {
                    Ok(value) => value,
                    Err(error) => { #failure }
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
                        &__http_query, #key,
                    ) {
                        Ok(value) => value,
                        Err(error) => { #failure }
                    };
                }
            } else {
                quote! {
                    let #ident: #ty = match #server::__private::query_required::<#ty>(
                        &__http_query, #key,
                    ) {
                        Ok(value) => value,
                        Err(error) => { #failure }
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
                        Err(error) => { #failure }
                    };
                }
            } else {
                quote! {
                    let #ident: #ty = match #server::__private::header_required::<#ty>(
                        __http_request.header(#name),
                    ) {
                        Ok(value) => value,
                        Err(error) => { #failure }
                    };
                }
            }
        }
        ParameterSource::QueryMany(inner) => {
            let key = LitStr::new(&ident.to_string(), proc_macro2::Span::call_site());
            quote! {
                let #ident: #ty = match #server::__private::query_many::<#inner>(&__http_query, #key) {
                    Ok(value) => value,
                    Err(error) => { #failure }
                };
            }
        }
        ParameterSource::QueryObject => quote! {
            let #ident: #ty = match #server::__private::query_object(__http_request.query()) {
                Ok(value) => value,
                Err(error) => { #failure }
            };
        },
        ParameterSource::Authenticated => quote! {},
        _ => expand_body_parameter(parameter, server, &failure),
    }
}

fn expand_body_parameter(
    parameter: &Parameter,
    server: &TokenStream2,
    failure: &TokenStream2,
) -> TokenStream2 {
    let ident = &parameter.ident;
    let ty = &parameter.ty;
    let name = LitStr::new(&ident.to_string(), proc_macro2::Span::call_site());
    match &parameter.source {
        ParameterSource::FormBody => quote! {
            let #ident: #ty = match #server::__private::form(__http_request.body(), __http_request.header("content-type")) {
                Ok(value) => value,
                Err(error) => return __http_request.reject_body(&error, #name),
            };
        },
        ParameterSource::MultipartBody => quote! {
            let #ident: #ty = match #server::__private::multipart(__http_request.body(), __http_request.header("content-type")).await {
                Ok(value) => value,
                Err(error) => return __http_request.reject_body(&error, #name),
            };
        },
        ParameterSource::RawBody => quote! {
            let #ident: #ty = #server::Body(__http_request.body());
        },
        ParameterSource::BorrowedBody => quote! { let #ident: #ty = __http_request.body(); },
        ParameterSource::JsonBody => quote! {
            let __http_json = __http_request.json_body();
            let #ident: #ty = match __http_json.decode() {
                Ok(value) => value,
                Err(error) => { #failure }
            };
        },
        _ => unreachable!("body binding requires a body parameter"),
    }
}
