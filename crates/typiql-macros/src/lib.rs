use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    parse_macro_input, Data, DeriveInput, Fields, Ident, LitStr, Token,
};

// ── helpers ─────────────────────────────────────────────────────────────────

fn to_snake_case(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

fn simple_plural(s: &str) -> String {
    if s.ends_with('s') || s.ends_with('x') || s.ends_with('z') {
        format!("{}es", s)
    } else if s.ends_with('y') && !s.ends_with("ay") && !s.ends_with("ey") && !s.ends_with("oy") {
        format!("{}ies", &s[..s.len() - 1])
    } else {
        format!("{}s", s)
    }
}

// ── per-field metadata ───────────────────────────────────────────────────────

struct FieldInfo {
    ident: proc_macro2::Ident,
    ty: syn::Type,
    is_key: bool,
    data_name: Option<String>,
    other_attrs: Vec<syn::Attribute>,
}

fn parse_field_info(f: &syn::Field) -> FieldInfo {
    let ident = f.ident.as_ref().unwrap().clone();
    let ty = f.ty.clone();
    let mut is_key = false;
    let mut data_name: Option<String> = None;
    let mut other_attrs = Vec::new();

    for attr in &f.attrs {
        if attr.path().is_ident("typiql") {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("key") {
                    is_key = true;
                } else if meta.path.is_ident("data_name") {
                    let value = meta.value()?;
                    let s: LitStr = value.parse()?;
                    data_name = Some(s.value());
                }
                Ok(())
            })
            .ok();
        } else {
            other_attrs.push(attr.clone());
        }
    }

    FieldInfo { ident, ty, is_key, data_name, other_attrs }
}

// ── #[typiql_type] ──────────────────────────────────────────────────────────

/// Attribute macro that turns a plain struct into a typiql-managed GraphQL type.
///
/// Generates:
///   - `#[derive(SimpleObject, Serialize, Deserialize, Clone)]` on the struct
///   - A companion `{Name}Input` InputObject with all non-key fields as `Option<T>`
///   - `TypiQLType` trait impl
///   - `{Name}Query` / `{Name}Mutation` structs with CRUD resolvers
///   - `{Name}Changed` event type and `{Name}Subscription` for real-time updates
///
/// Mutations publish to `TypiQLBroker<{Name}Changed>` on add/update/remove.
/// The generated subscription filters by optional `id` (for subscribeToOne).
///
/// Field attributes:
///   - `#[typiql(key)]`              — marks the primary-key field (defaults to first)
///   - `#[typiql(data_name = "…")]`  — generates `#[serde(rename = "…")]`
#[proc_macro_attribute]
pub fn typiql_type(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as DeriveInput);
    let name = &input.ident;
    let name_str = name.to_string();
    let name_snake = to_snake_case(&name_str);
    let collection_name = simple_plural(&name_snake);

    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(f) => f.named.iter().collect::<Vec<_>>(),
            _ => panic!("#[typiql_type] requires named struct fields"),
        },
        _ => panic!("#[typiql_type] requires a struct"),
    };

    let field_infos: Vec<FieldInfo> = fields.iter().map(|f| parse_field_info(f)).collect();

    let key_idx = field_infos.iter().position(|fi| fi.is_key).unwrap_or(0);
    let key_field_str = field_infos[key_idx].ident.to_string();
    let key_field_ident = &field_infos[key_idx].ident;

    // Struct fields with typiql attrs stripped; serde renames + defaults applied.
    let struct_fields: Vec<TokenStream2> = field_infos.iter().map(|fi| {
        let ident = &fi.ident;
        let ty = &fi.ty;
        let attrs = &fi.other_attrs;
        let serde_rename = fi.data_name.as_ref().map(|dn| {
            quote! { #[serde(rename = #dn)] }
        });
        let serde_default = (!fi.is_key).then(|| quote! { #[serde(default)] });
        quote! {
            #(#attrs)*
            #serde_rename
            #serde_default
            pub #ident: #ty,
        }
    }).collect();

    let non_key: Vec<&FieldInfo> = field_infos.iter().filter(|fi| !fi.is_key).collect();

    let input_fields: Vec<TokenStream2> = non_key.iter().map(|fi| {
        let ident = &fi.ident;
        let ty = &fi.ty;
        quote! { pub #ident: ::std::option::Option<#ty>, }
    }).collect();

    let filter_args: Vec<TokenStream2> = non_key.iter().map(|fi| {
        let ident = &fi.ident;
        let ty = &fi.ty;
        quote! { #ident: ::std::option::Option<#ty>, }
    }).collect();

    let filter_inserts: Vec<TokenStream2> = non_key.iter().map(|fi| {
        let ident = &fi.ident;
        let ident_str = ident.to_string();
        quote! {
            if let ::std::option::Option::Some(v) = #ident {
                filters.insert(
                    #ident_str.to_string(),
                    ::serde_json::to_value(v).unwrap_or_default(),
                );
            }
        }
    }).collect();

    let input_name          = format_ident!("{}Input",        name);
    let query_struct        = format_ident!("{}Query",        name);
    let mutation_struct     = format_ident!("{}Mutation",     name);
    let changed_struct      = format_ident!("{}Changed",      name);
    let subscription_struct = format_ident!("{}Subscription", name);

    let get_fn        = format_ident!("get_{}", &name_snake);
    let get_list_fn   = format_ident!("get_{}", simple_plural(&name_snake));
    let add_fn        = format_ident!("add_{}", &name_snake);
    let update_fn     = format_ident!("update_{}", &name_snake);
    let remove_fn     = format_ident!("remove_{}", &name_snake);
    let changed_fn    = format_ident!("{}_changed", &name_snake);

    let vis = &input.vis;
    let other_struct_attrs: Vec<_> = input.attrs.iter()
        .filter(|a| !a.path().is_ident("typiql_type"))
        .collect();

    let expanded = quote! {
        #[derive(
            ::async_graphql::SimpleObject,
            ::serde::Serialize,
            ::serde::Deserialize,
            ::std::clone::Clone,
            ::std::fmt::Debug,
        )]
        #(#other_struct_attrs)*
        #vis struct #name {
            #(#struct_fields)*
        }

        #[derive(
            ::async_graphql::InputObject,
            ::serde::Serialize,
            ::serde::Deserialize,
            ::std::clone::Clone,
            ::std::default::Default,
            ::std::fmt::Debug,
        )]
        #vis struct #input_name {
            #(#input_fields)*
        }

        impl ::typiql::TypiQLType for #name {
            type Input = #input_name;
            fn type_name()       -> &'static str { #name_str }
            fn key_field()       -> &'static str { #key_field_str }
            fn collection_name() -> &'static str { #collection_name }
        }

        // ── Change event broadcast type ──────────────────────────────────────

        #[derive(::async_graphql::SimpleObject, ::std::clone::Clone, ::std::fmt::Debug)]
        #vis struct #changed_struct {
            pub operation_name: ::std::string::String,
            pub value: #name,
        }

        // ── CRUD queries ─────────────────────────────────────────────────────

        #[derive(::std::default::Default)]
        #vis struct #query_struct;

        #[::async_graphql::Object]
        impl #query_struct {
            async fn #get_fn(
                &self,
                ctx: &::async_graphql::Context<'_>,
                #key_field_ident: ::std::string::String,
            ) -> ::async_graphql::Result<::std::option::Option<#name>> {
                ::typiql::resolve_get::<#name>(ctx, #key_field_ident).await
            }

            async fn #get_list_fn(
                &self,
                ctx: &::async_graphql::Context<'_>,
                #(#filter_args)*
            ) -> ::async_graphql::Result<::std::vec::Vec<#name>> {
                let mut filters = ::std::collections::HashMap::new();
                #(#filter_inserts)*
                ::typiql::resolve_list::<#name>(ctx, filters).await
            }
        }

        // ── CRUD mutations — publish change events on success ────────────────

        #[derive(::std::default::Default)]
        #vis struct #mutation_struct;

        #[::async_graphql::Object]
        impl #mutation_struct {
            async fn #add_fn(
                &self,
                ctx: &::async_graphql::Context<'_>,
                values: #input_name,
            ) -> ::async_graphql::Result<#name> {
                let __item = ::typiql::resolve_add::<#name>(ctx, values).await?;
                ::typiql::TypiQLBroker::publish(#changed_struct {
                    operation_name: "add".to_string(),
                    value: __item.clone(),
                });
                ::std::result::Result::Ok(__item)
            }

            async fn #update_fn(
                &self,
                ctx: &::async_graphql::Context<'_>,
                #key_field_ident: ::std::string::String,
                update: #input_name,
            ) -> ::async_graphql::Result<::std::option::Option<#name>> {
                let __item = ::typiql::resolve_update::<#name>(ctx, #key_field_ident, update).await?;
                if let ::std::option::Option::Some(ref __v) = __item {
                    ::typiql::TypiQLBroker::publish(#changed_struct {
                        operation_name: "update".to_string(),
                        value: __v.clone(),
                    });
                }
                ::std::result::Result::Ok(__item)
            }

            async fn #remove_fn(
                &self,
                ctx: &::async_graphql::Context<'_>,
                #key_field_ident: ::std::string::String,
            ) -> ::async_graphql::Result<::std::option::Option<#name>> {
                let __item = ::typiql::resolve_remove::<#name>(ctx, #key_field_ident).await?;
                if let ::std::option::Option::Some(ref __v) = __item {
                    ::typiql::TypiQLBroker::publish(#changed_struct {
                        operation_name: "remove".to_string(),
                        value: __v.clone(),
                    });
                }
                ::std::result::Result::Ok(__item)
            }
        }

        // ── Real-time subscription ───────────────────────────────────────────

        #[derive(::std::default::Default)]
        #vis struct #subscription_struct;

        #[::async_graphql::Subscription]
        impl #subscription_struct {
            /// Subscribe to all changes for this type, or filter to a single record
            /// by passing its `id`. Matches the `subscribe` / `subscribeToOne`
            /// dispatcher slots used by ReactiveAdmin.
            async fn #changed_fn(
                &self,
                id: ::std::option::Option<::std::string::String>,
            ) -> impl ::async_graphql::futures_util::Stream<Item = #changed_struct> {
                use ::async_graphql::futures_util::StreamExt as _;
                ::typiql::TypiQLBroker::<#changed_struct>::subscribe()
                    .filter(move |__event| {
                        let __matches = id.as_ref()
                            .map(|__id| &__event.value.#key_field_ident == __id)
                            .unwrap_or(true);
                        ::std::future::ready(__matches)
                    })
            }
        }
    };

    expanded.into()
}

// ── typiql_schema!(Types; ExtraQueries; ExtraMutations; ExtraSubscriptions) ──

struct TypiQLSchemaInput {
    types: Vec<Ident>,
    extra_queries: Vec<Ident>,
    extra_mutations: Vec<Ident>,
    extra_subscriptions: Vec<Ident>,
}

impl syn::parse::Parse for TypiQLSchemaInput {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let mut types = vec![];
        while !input.is_empty() && !input.peek(Token![;]) {
            types.push(input.parse::<Ident>()?);
            if input.peek(Token![,]) { let _: Token![,] = input.parse()?; }
        }

        let mut extra_queries = vec![];
        if input.peek(Token![;]) {
            let _: Token![;] = input.parse()?;
            while !input.is_empty() && !input.peek(Token![;]) {
                extra_queries.push(input.parse::<Ident>()?);
                if input.peek(Token![,]) { let _: Token![,] = input.parse()?; }
            }
        }

        let mut extra_mutations = vec![];
        if input.peek(Token![;]) {
            let _: Token![;] = input.parse()?;
            while !input.is_empty() && !input.peek(Token![;]) {
                extra_mutations.push(input.parse::<Ident>()?);
                if input.peek(Token![,]) { let _: Token![,] = input.parse()?; }
            }
        }

        // Extra subscription roots to merge alongside the per-type generated ones.
        let mut extra_subscriptions = vec![];
        if input.peek(Token![;]) {
            let _: Token![;] = input.parse()?;
            while !input.is_empty() {
                extra_subscriptions.push(input.parse::<Ident>()?);
                if input.peek(Token![,]) { let _: Token![,] = input.parse()?; }
            }
        }

        Ok(TypiQLSchemaInput { types, extra_queries, extra_mutations, extra_subscriptions })
    }
}

/// Wires multiple typiql types into a single schema.
///
/// ```rust,ignore
/// typiql_schema!(Vehicle, Client);
/// typiql_schema!(Vehicle, Client; CustomQuery; CustomMutation);
/// typiql_schema!(Vehicle, Client; CustomQuery; CustomMutation; ExtraSubscriptionRoot);
/// ```
///
/// Generates:
///   - `TypiQLQuery`        — merged CRUD queries + extra queries
///   - `TypiQLMutation`     — merged CRUD mutations + extra mutations
///   - `TypiQLSubscription` — merged per-type change subscriptions + extra subscriptions
///   - `build_typiql_schema(adapter)` — schema builder function
#[proc_macro]
pub fn typiql_schema(input: TokenStream) -> TokenStream {
    let schema_input = parse_macro_input!(input as TypiQLSchemaInput);

    let gen_query_types: Vec<proc_macro2::Ident> = schema_input.types.iter()
        .map(|t| format_ident!("{}Query", t)).collect();
    let gen_mutation_types: Vec<proc_macro2::Ident> = schema_input.types.iter()
        .map(|t| format_ident!("{}Mutation", t)).collect();
    let gen_subscription_types: Vec<proc_macro2::Ident> = schema_input.types.iter()
        .map(|t| format_ident!("{}Subscription", t)).collect();

    let all_query_types: Vec<&proc_macro2::Ident> = gen_query_types.iter()
        .chain(schema_input.extra_queries.iter()).collect();
    let all_mutation_types: Vec<&proc_macro2::Ident> = gen_mutation_types.iter()
        .chain(schema_input.extra_mutations.iter()).collect();
    let all_subscription_types: Vec<&proc_macro2::Ident> = gen_subscription_types.iter()
        .chain(schema_input.extra_subscriptions.iter()).collect();

    let expanded = quote! {
        #[derive(::async_graphql::MergedObject, ::std::default::Default)]
        pub struct TypiQLQuery(#(#all_query_types),*);

        #[derive(::async_graphql::MergedObject, ::std::default::Default)]
        pub struct TypiQLMutation(#(#all_mutation_types),*);

        #[derive(::async_graphql::MergedSubscription, ::std::default::Default)]
        pub struct TypiQLSubscription(#(#all_subscription_types),*);

        pub fn build_typiql_schema(
            adapter: impl ::typiql::TypiQLAdapter + 'static,
        ) -> ::async_graphql::Schema<
            TypiQLQuery,
            TypiQLMutation,
            TypiQLSubscription,
        > {
            ::async_graphql::Schema::build(
                TypiQLQuery::default(),
                TypiQLMutation::default(),
                TypiQLSubscription::default(),
            )
            .data(::std::sync::Arc::new(adapter) as ::std::sync::Arc<dyn ::typiql::TypiQLAdapter>)
            .finish()
        }
    };

    expanded.into()
}
