use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::parse::Parser;
use syn::{parse_macro_input, Data, DeriveInput, Fields, Ident, LitStr, Token};

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

/// Strips one layer of `Option<...>` if present, returning the inner type;
/// otherwise returns the type unchanged.
fn unwrap_option(ty: &syn::Type) -> &syn::Type {
    if let syn::Type::Path(type_path) = ty {
        if let Some(segment) = type_path.path.segments.last() {
            if segment.ident == "Option" {
                if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
                    if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                        return inner;
                    }
                }
            }
        }
    }
    ty
}

/// The bare type name (last path segment), after unwrapping `Option<...>` —
/// used only to decide which filter operators make sense for a field.
fn scalar_type_name(ty: &syn::Type) -> Option<String> {
    if let syn::Type::Path(type_path) = unwrap_option(ty) {
        type_path.path.segments.last().map(|s| s.ident.to_string())
    } else {
        None
    }
}

fn is_option_type(ty: &syn::Type) -> bool {
    if let syn::Type::Path(type_path) = ty {
        if let Some(segment) = type_path.path.segments.last() {
            return segment.ident == "Option";
        }
    }
    false
}

fn is_vec_type(ty: &syn::Type) -> bool {
    if let syn::Type::Path(type_path) = ty {
        if let Some(segment) = type_path.path.segments.last() {
            return segment.ident == "Vec";
        }
    }
    false
}

/// The bare `T` of a `Vec<T>` field type — used for has-many relation
/// fields (`#[typiql(relation(...))]` on a `Vec<Target>` field, rather than
/// the has-one `Option<Target>` shape). Returns the type unchanged if it
/// isn't actually `Vec<...>`.
fn vec_inner_type(ty: &syn::Type) -> &syn::Type {
    if let syn::Type::Path(type_path) = ty {
        if let Some(segment) = type_path.path.segments.last() {
            if segment.ident == "Vec" {
                if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
                    if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                        return inner;
                    }
                }
            }
        }
    }
    ty
}

/// Maps a field's Rust type to a DuckDB column-type string, used to derive
/// `TypiQLType::duckdb_columns()` at macro-expansion time — see
/// `#[typiql_type(adapter = "duckdb")]`. Scalars get their natural DuckDB
/// equivalent; anything else (nested structs, `Vec<T>`, enums without a
/// direct scalar mapping) falls back to `VARCHAR` and is expected to be
/// JSON-serialized by the adapter, matching the "big/irregular data as a
/// JSON string column" convention already used elsewhere in this app (e.g.
/// `Dashboard.elements`). `Option<T>` maps to `T`'s type without `NOT NULL`;
/// everything else is `NOT NULL`.
fn duckdb_type_for(ty: &syn::Type) -> String {
    let nullable = is_option_type(ty);
    let base = match scalar_type_name(ty).as_deref() {
        Some("String") | Some("str") => "VARCHAR",
        Some("bool") => "BOOLEAN",
        Some("f32") | Some("f64") => "DOUBLE",
        Some("i8") => "TINYINT",
        Some("i16") => "SMALLINT",
        Some("i32") => "INTEGER",
        Some("i64") | Some("isize") => "BIGINT",
        Some("i128") => "HUGEINT",
        Some("u8") => "UTINYINT",
        Some("u16") => "USMALLINT",
        Some("u32") => "UINTEGER",
        Some("u64") | Some("usize") => "UBIGINT",
        Some("u128") => "UHUGEINT",
        _ => "VARCHAR",
    };
    if nullable {
        base.to_string()
    } else {
        format!("{} NOT NULL", base)
    }
}

/// One generated filter-operator variant for a field: `suffix` is appended to
/// the field name for the GraphQL argument (`""` for bare equality), `op` is
/// the `FilterOp` variant ident to emit, `is_list` means the argument takes
/// `Vec<T>` (for `In`/`NotIn`) rather than a scalar.
struct OpSpec {
    suffix: &'static str,
    op: &'static str,
    is_list: bool,
}

fn op_specs_for(ty: &syn::Type) -> Vec<OpSpec> {
    const EQ_NE: &[OpSpec] = &[
        OpSpec {
            suffix: "",
            op: "Eq",
            is_list: false,
        },
        OpSpec {
            suffix: "_ne",
            op: "Ne",
            is_list: false,
        },
    ];
    const FULL: &[OpSpec] = &[
        OpSpec {
            suffix: "",
            op: "Eq",
            is_list: false,
        },
        OpSpec {
            suffix: "_ne",
            op: "Ne",
            is_list: false,
        },
        OpSpec {
            suffix: "_gt",
            op: "Gt",
            is_list: false,
        },
        OpSpec {
            suffix: "_gte",
            op: "Gte",
            is_list: false,
        },
        OpSpec {
            suffix: "_lt",
            op: "Lt",
            is_list: false,
        },
        OpSpec {
            suffix: "_lte",
            op: "Lte",
            is_list: false,
        },
        OpSpec {
            suffix: "_in",
            op: "In",
            is_list: true,
        },
        OpSpec {
            suffix: "_not_in",
            op: "NotIn",
            is_list: true,
        },
    ];
    // LIKE-style operators only make sense for strings — matches C#
    // SqlData.cs, where startsWith/endsWith/contains are string-only too.
    const STRING_EXTRA: &[OpSpec] = &[
        OpSpec {
            suffix: "_starts_with",
            op: "StartsWith",
            is_list: false,
        },
        OpSpec {
            suffix: "_ends_with",
            op: "EndsWith",
            is_list: false,
        },
        OpSpec {
            suffix: "_contains",
            op: "Contains",
            is_list: false,
        },
        OpSpec {
            suffix: "_not_starts_with",
            op: "NotStartsWith",
            is_list: false,
        },
        OpSpec {
            suffix: "_not_ends_with",
            op: "NotEndsWith",
            is_list: false,
        },
        OpSpec {
            suffix: "_not_contains",
            op: "NotContains",
            is_list: false,
        },
    ];
    let type_name = scalar_type_name(ty);
    let is_string = matches!(type_name.as_deref(), Some("String") | Some("str"));
    let ordered = is_string
        || matches!(
            type_name.as_deref(),
            Some(
                "i8" | "i16"
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
        );
    let mut specs: Vec<OpSpec> = (if ordered { FULL } else { EQ_NE })
        .iter()
        .map(|s| OpSpec {
            suffix: s.suffix,
            op: s.op,
            is_list: s.is_list,
        })
        .collect();
    if is_string {
        specs.extend(STRING_EXTRA.iter().map(|s| OpSpec {
            suffix: s.suffix,
            op: s.op,
            is_list: s.is_list,
        }));
    }
    specs
}

// ── per-field metadata ───────────────────────────────────────────────────────

/// One condition of a relation field: match this struct's `local` field
/// against the related type's `foreign` field using `op`. A relation field
/// with more than one condition (repeated `#[typiql(relation(...))]`
/// attributes on the same field) is a composite relation.
struct RelationCondition {
    local: String,
    // Kept (validated at parse time, not yet read at codegen time) so
    // lifting the "eq only" restriction later doesn't need new attribute
    // parsing — only RelationLoader's key type gaining an operator.
    #[allow(dead_code)]
    op: String,
    foreign: String,
}

/// A relation whose target isn't in a fixed default location — its file is
/// computed per-record from a template string (`{field}` placeholders
/// substituted with `self.field` at resolve time), naming a specific table
/// within that file. Not DataLoader-batched like `RelationCondition`-based
/// relations: each parent record's file is already unique, so there's no
/// shared collection to batch a lookup against.
struct FileTableRelation {
    file_template: String,
    table: String,
}

struct FieldInfo {
    ident: proc_macro2::Ident,
    ty: syn::Type,
    is_key: bool,
    data_name: Option<String>,
    relations: Vec<RelationCondition>,
    file_table_relation: Option<FileTableRelation>,
    other_attrs: Vec<syn::Attribute>,
}

impl FieldInfo {
    fn is_relation(&self) -> bool {
        !self.relations.is_empty() || self.file_table_relation.is_some()
    }
}

fn parse_field_info(f: &syn::Field) -> FieldInfo {
    let ident = f.ident.as_ref().unwrap().clone();
    let ty = f.ty.clone();
    let mut is_key = false;
    let mut data_name: Option<String> = None;
    let mut relations: Vec<RelationCondition> = Vec::new();
    let mut file_table_relation: Option<FileTableRelation> = None;
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
                } else if meta.path.is_ident("relation") {
                    let mut local: Option<String> = None;
                    let mut op: Option<String> = None;
                    let mut foreign: Option<String> = None;
                    let mut file: Option<String> = None;
                    let mut table: Option<String> = None;
                    meta.parse_nested_meta(|inner| {
                        if inner.path.is_ident("local") {
                            let s: LitStr = inner.value()?.parse()?;
                            local = Some(s.value());
                        } else if inner.path.is_ident("op") {
                            let s: LitStr = inner.value()?.parse()?;
                            op = Some(s.value());
                        } else if inner.path.is_ident("foreign") {
                            let s: LitStr = inner.value()?.parse()?;
                            foreign = Some(s.value());
                        } else if inner.path.is_ident("file") {
                            let s: LitStr = inner.value()?.parse()?;
                            file = Some(s.value());
                        } else if inner.path.is_ident("table") {
                            let s: LitStr = inner.value()?.parse()?;
                            table = Some(s.value());
                        }
                        Ok(())
                    })?;
                    if let (Some(file), Some(table)) = (file, table) {
                        file_table_relation = Some(FileTableRelation {
                            file_template: file,
                            table,
                        });
                    } else if let (Some(local), Some(foreign)) = (local, foreign) {
                        let op = op.unwrap_or_else(|| "eq".to_string());
                        // Batched relation loading currently only implements
                        // equality matching (see RelationLoader in
                        // typiql-core) — validate eagerly with a clear
                        // message rather than silently ignoring a declared
                        // but unsupported operator. op_ident's full mapping
                        // stays in place so lifting this restriction later
                        // only needs RelationLoader's key type to carry the
                        // operator, not new attribute parsing.
                        if op_ident(&op) != "Eq" {
                            panic!(
                                "#[typiql(relation(op = {:?}))] is not yet supported — \
                                 relation conditions must use op = \"eq\" for now",
                                op
                            );
                        }
                        relations.push(RelationCondition { local, op, foreign });
                    }
                }
                Ok(())
            })
            .ok();
        } else {
            other_attrs.push(attr.clone());
        }
    }

    FieldInfo {
        ident,
        ty,
        is_key,
        data_name,
        relations,
        file_table_relation,
        other_attrs,
    }
}

/// Extracts `{field}`-style placeholder names from a template string, in
/// order of first appearance — used to build `format!(template, field =
/// self.field, ...)` for a `FileTableRelation`'s `file` template.
fn template_fields(template: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let chars = template.char_indices();
    for (i, c) in chars {
        if c == '{' {
            if let Some(end) = template[i + 1..].find('}') {
                let name = &template[i + 1..i + 1 + end];
                if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    fields.push(name.to_string());
                }
            }
        }
    }
    fields
}

fn op_ident(op: &str) -> proc_macro2::Ident {
    let variant = match op {
        "eq" => "Eq",
        "ne" => "Ne",
        "gt" => "Gt",
        "gte" => "Gte",
        "lt" => "Lt",
        "lte" => "Lte",
        "in" => "In",
        "not_in" => "NotIn",
        other => panic!(
            "unknown relation op {:?} — expected eq/ne/gt/gte/lt/lte/in/not_in",
            other
        ),
    };
    format_ident!("{}", variant)
}

// ── #[typiql_type] ──────────────────────────────────────────────────────────

/// Attribute macro that turns a plain struct into a typiql-managed GraphQL type.
///
/// Generates:
///   - `#[derive(SimpleObject, Serialize, Deserialize, Clone)]` on the struct
///     (plus `#[graphql(complex)]` and a companion `#[ComplexObject] impl` if
///     any field is a relation)
///   - A companion `{Name}Input` InputObject with all non-key, non-relation
///     fields as `Option<T>`
///   - `TypiQLType` trait impl
///   - `{Name}Query` / `{Name}Mutation` structs with CRUD resolvers — the
///     list query gets one filter argument per applicable operator per
///     scalar field (e.g. `count`/`count_ne`/`count_gt`/... for a numeric
///     field), narrowed to `eq`/`ne` only for `bool` and non-scalar types
///   - `{Name}Changed` event type and `{Name}Subscription` for real-time updates
///
/// Mutations publish to `TypiQLBroker<{Name}Changed>` on add/update/remove.
/// The generated subscription filters by optional `id` (for subscribeToOne).
///
/// Field attributes:
///   - `#[typiql(key)]`              — marks the primary-key field (defaults to first)
///   - `#[typiql(data_name = "…")]`  — generates `#[serde(rename = "…")]`
///   - `#[typiql(relation(local = "…", op = "eq", foreign = "…"))]` — declares
///     `this` field (typed as another `#[typiql_type]` struct, e.g.
///     `Option<File>`) as a relation: match this struct's `local` field
///     against the target's `foreign` field. Repeat the attribute on the same
///     field for a composite (multi-condition) relation. Resolved via an
///     `async-graphql` `DataLoader`, batched across a whole list query to
///     avoid N+1 — see `typiql_schema!`'s generated `build_typiql_schema`,
///     which registers one `DataLoader<RelationLoader<T>>` per type.
///   - `#[typiql(relation(file = "{field}/…", table = "…"))]` — declares a
///     relation whose target has no fixed default location: its file is
///     computed per-record from the template (`{field}` substituted with
///     `self.field`), reading the named table within it. Not batched (each
///     parent record's file is already unique) — resolves via a direct
///     adapter call, not the DataLoader/RelationLoader path above. The
///     target type is expected to be `#[typiql_type(no_location)]`.
///
/// Struct-level `#[typiql_type(no_location)]` marks a "dumb" type with no
/// default storage location of its own — it still gets the usual
/// `SimpleObject`/`InputObject`/serde derives (so it can be used as a
/// relation's target type), but no `{Name}Query`/`Mutation`/`Changed`/
/// `Subscription` are generated (there's nothing for auto-CRUD to operate
/// against) and it's not registered in `typiql_schema!`. Reachable only via
/// a relation field on another type, or hand-written resolvers calling the
/// adapter directly with an explicit `Location`.
///
/// Struct-level `#[typiql_type(adapter = "…")]` picks which registered
/// adapter (see `typiql_schema!`'s `HashMap<&str, Arc<dyn TypiQLAdapter>>`)
/// stores this type — defaults to `"default"` if omitted. `adapter =
/// "duckdb"` additionally derives `TypiQLType::duckdb_columns()` from each
/// non-relation field's Rust type (see `duckdb_type_for`), so
/// `DuckDbAdapter` can generically `CREATE TABLE IF NOT EXISTS` for this
/// type with no per-type hand-written DDL. Both attributes can be combined:
/// `#[typiql_type(no_location, adapter = "duckdb")]`.
#[proc_macro_attribute]
pub fn typiql_type(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr2: TokenStream2 = attr.into();
    let mut is_dumb = false;
    let mut adapter_attr: Option<String> = None;
    if !attr2.is_empty() {
        let parser = syn::punctuated::Punctuated::<syn::Meta, Token![,]>::parse_terminated;
        let metas = parser.parse2(attr2.clone()).unwrap_or_default();
        for meta in metas {
            match meta {
                syn::Meta::Path(p) if p.is_ident("no_location") => is_dumb = true,
                syn::Meta::NameValue(nv) if nv.path.is_ident("adapter") => {
                    if let syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(s),
                        ..
                    }) = nv.value
                    {
                        adapter_attr = Some(s.value());
                    }
                }
                _ => {}
            }
        }
    }
    let adapter_str = adapter_attr.unwrap_or_else(|| "default".to_string());
    let is_duckdb = adapter_str == "duckdb";

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

    // Dumb types don't need a key at all (never independently queried/
    // updated/removed by one) — every existing (non-dumb) type already
    // explicitly marks a key field, so the `unwrap_or(0)` fallback below is
    // only ever reached for a dumb type with none marked, or hypothetically
    // a non-dumb type that forgot to (preserving today's exact behavior).
    let key_idx = field_infos.iter().position(|fi| fi.is_key);

    let has_relations = field_infos.iter().any(|fi| fi.is_relation());

    // Struct fields with typiql attrs stripped; serde renames + defaults
    // applied; relation fields additionally hidden from GraphQL (resolved
    // instead via the companion ComplexObject impl below) and from storage
    // (they're never part of this record's own JSON — only the plain scalar
    // field(s) they're matched against are, for local/foreign relations; a
    // file/table relation has no local field to keep, it's purely computed).
    let struct_fields: Vec<TokenStream2> = field_infos
        .iter()
        .map(|fi| {
            let ident = &fi.ident;
            let ty = &fi.ty;
            let attrs = &fi.other_attrs;
            let serde_rename = fi.data_name.as_ref().map(|dn| {
                quote! { #[serde(rename = #dn)] }
            });
            let serde_default = (!fi.is_key).then(|| quote! { #[serde(default)] });
            let relation_skip = fi
                .is_relation()
                .then(|| quote! { #[graphql(skip)] #[serde(skip)] });
            quote! {
                #(#attrs)*
                #serde_rename
                #serde_default
                #relation_skip
                pub #ident: #ty,
            }
        })
        .collect();

    let non_key: Vec<&FieldInfo> = field_infos
        .iter()
        .filter(|fi| !fi.is_key && !fi.is_relation())
        .collect();

    // Every Input field is `MaybeUndefined<T>` (not `Option<T>`) — a
    // three-state type (Undefined/Null/Value) that lets an update
    // distinguish "field omitted" (leave untouched) from "field explicitly
    // set to null" (clear it), which plain `Option<T>` + a blanket
    // strip-all-nulls pass could never express. `#[serde(skip_serializing_if
    // = "MaybeUndefined::is_undefined")]` is the load-bearing bit: without
    // it, MaybeUndefined's own Serialize impl sends both Undefined and Null
    // to the same `serialize_none()`, so the omit-vs-null distinction only
    // becomes visible at the JSON-object-key level via this attribute
    // (Undefined -> key absent entirely; Null -> key present with JSON
    // null). `#[serde(default)]` is required alongside it because unlike
    // Option<T>, serde's struct-derive doesn't auto-treat MaybeUndefined<T>
    // as implicitly-optional when a key is missing on deserialize.
    // `unwrap_option` strips one Option layer first so a field already
    // declared `Option<String>` on the entity becomes `MaybeUndefined<String>`
    // on Input, not a double-wrapped `MaybeUndefined<Option<String>>`.
    let input_fields: Vec<TokenStream2> = non_key.iter().map(|fi| {
        let ident = &fi.ident;
        let inner_ty = unwrap_option(&fi.ty);
        quote! {
            #[serde(default, skip_serializing_if = "::async_graphql::MaybeUndefined::is_undefined")]
            pub #ident: ::async_graphql::MaybeUndefined<#inner_ty>,
        }
    }).collect();

    // Same field set as `non_key`, but with the key field included — used
    // for filter-argument generation only (Input generation still excludes
    // the key, unaffected). Shared by both the list query and the get-one
    // query, which is filter-based like the list query rather than an
    // exact-key-only lookup (get_fn below).
    let filterable: Vec<&FieldInfo> = field_infos.iter().filter(|fi| !fi.is_relation()).collect();

    // Column name/DuckDB-type pairs for `TypiQLType::duckdb_columns()` —
    // only populated for `adapter = "duckdb"` types; empty otherwise (the
    // trait's default already returns `&[]`, so an empty override here is
    // never actually emitted — see `duckdb_columns_fn` below).
    let duckdb_column_entries: Vec<TokenStream2> = if is_duckdb {
        filterable
            .iter()
            .map(|fi| {
                let col_name = fi.data_name.clone().unwrap_or_else(|| fi.ident.to_string());
                let col_type = duckdb_type_for(&fi.ty);
                quote! { (#col_name, #col_type) }
            })
            .collect()
    } else {
        Vec::new()
    };

    // One GraphQL argument per applicable filter operator per plain field —
    // e.g. `count: Option<i32>, count_ne: Option<i32>, count_gt: Option<i32>, ...`.
    let mut filter_args: Vec<TokenStream2> = Vec::new();
    let mut filter_inserts: Vec<TokenStream2> = Vec::new();
    for fi in &filterable {
        let field_str = fi.ident.to_string();
        let inner_ty = unwrap_option(&fi.ty);
        for spec in op_specs_for(&fi.ty) {
            let arg_ident = format_ident!("{}{}", fi.ident, spec.suffix);
            let op_variant = format_ident!("{}", spec.op);
            let arg_ty = if spec.is_list {
                quote! { ::std::option::Option<::std::vec::Vec<#inner_ty>> }
            } else {
                quote! { ::std::option::Option<#inner_ty> }
            };
            filter_args.push(quote! { #arg_ident: #arg_ty, });
            filter_inserts.push(quote! {
                if let ::std::option::Option::Some(v) = #arg_ident {
                    filters.push(::typiql::FieldFilter {
                        field: #field_str.to_string(),
                        op: ::typiql::FilterOp::#op_variant,
                        value: ::serde_json::to_value(v).unwrap_or_default(),
                    });
                }
            });
        }
    }

    // Companion ComplexObject resolvers for relation fields, if any.
    // Has-one (`Option<Target>`) local/foreign relations resolve via the
    // per-target-type DataLoader registered by typiql_schema!, batching
    // this field's lookup across every row in the same GraphQL execution
    // instead of one adapter call per row. Has-many (`Vec<Target>`)
    // local/foreign relations — e.g. Recording's many RecordingFrame rows —
    // aren't DataLoader-batched (there's no single-related-row cache key to
    // share), but need no relation-specific adapter plumbing either: they
    // resolve through the exact same `resolve_list::<Target>` every plain
    // list query already uses, which already picks the right adapter via
    // `Target::adapter_name()` — so a has-many relation crosses adapters
    // (e.g. a JSON parent to a DuckDB child) automatically, same as has-one.
    // file/table relations resolve via a direct adapter call — not batched,
    // since each parent record's file is already unique (no shared
    // collection to batch a lookup against).
    let complex_object_block = has_relations.then(|| {
        let mut resolvers: Vec<TokenStream2> = Vec::new();

        for fi in field_infos.iter().filter(|fi| !fi.relations.is_empty() && !is_vec_type(&fi.ty)) {
            let field_ident = &fi.ident;
            let target_ty = unwrap_option(&fi.ty);
            let local_idents: Vec<proc_macro2::Ident> = fi.relations.iter()
                .map(|r| format_ident!("{}", r.local)).collect();
            let foreign_strs: Vec<&str> = fi.relations.iter().map(|r| r.foreign.as_str()).collect();

            resolvers.push(quote! {
                async fn #field_ident(&self, ctx: &::async_graphql::Context<'_>) -> ::async_graphql::Result<::std::option::Option<#target_ty>> {
                    let mut __key: ::std::vec::Vec<(::std::string::String, ::std::string::String)> = ::std::vec::Vec::new();
                    #(
                        match &self.#local_idents {
                            ::std::option::Option::Some(__v) => __key.push((#foreign_strs.to_string(), __v.clone())),
                            ::std::option::Option::None => return ::std::result::Result::Ok(::std::option::Option::None),
                        }
                    )*
                    let loader = ctx.data::<::async_graphql::dataloader::DataLoader<::typiql::RelationLoader<#target_ty>>>()?;
                    ::std::result::Result::Ok(
                        loader.load_one(__key).await
                            .map_err(|e| ::async_graphql::Error::new(e.to_string()))?
                    )
                }
            });
        }

        for fi in field_infos.iter().filter(|fi| !fi.relations.is_empty() && is_vec_type(&fi.ty)) {
            let field_ident = &fi.ident;
            let target_ty = vec_inner_type(&fi.ty);

            let filter_pushes: Vec<TokenStream2> = fi.relations.iter().map(|r| {
                let local_ident = format_ident!("{}", r.local);
                let foreign_str = &r.foreign;
                let local_field_is_option = field_infos.iter()
                    .find(|other| other.ident == local_ident)
                    .map(|other| is_option_type(&other.ty))
                    .unwrap_or(false);
                if local_field_is_option {
                    quote! {
                        match &self.#local_ident {
                            ::std::option::Option::Some(__v) => __filters.push(::typiql::FieldFilter {
                                field: #foreign_str.to_string(),
                                op: ::typiql::FilterOp::Eq,
                                value: ::serde_json::Value::String(__v.clone()),
                            }),
                            ::std::option::Option::None => return ::std::result::Result::Ok(::std::vec::Vec::new()),
                        }
                    }
                } else {
                    quote! {
                        __filters.push(::typiql::FieldFilter {
                            field: #foreign_str.to_string(),
                            op: ::typiql::FilterOp::Eq,
                            value: ::serde_json::Value::String(self.#local_ident.clone()),
                        });
                    }
                }
            }).collect();

            resolvers.push(quote! {
                async fn #field_ident(&self, ctx: &::async_graphql::Context<'_>) -> ::async_graphql::Result<::std::vec::Vec<#target_ty>> {
                    let mut __filters: ::std::vec::Vec<::typiql::FieldFilter> = ::std::vec::Vec::new();
                    #(#filter_pushes)*
                    ::typiql::resolve_list::<#target_ty>(ctx, __filters).await
                }
            });
        }

        for fi in field_infos.iter().filter(|fi| fi.file_table_relation.is_some()) {
            let field_ident = &fi.ident;
            let target_ty = unwrap_option(&fi.ty);
            let rel = fi.file_table_relation.as_ref().unwrap();
            let template = &rel.file_template;
            let table = &rel.table;
            let tpl_field_idents: Vec<proc_macro2::Ident> = template_fields(template).iter()
                .map(|f| format_ident!("{}", f)).collect();

            resolvers.push(quote! {
                async fn #field_ident(&self, ctx: &::async_graphql::Context<'_>) -> ::async_graphql::Result<::std::option::Option<#target_ty>> {
                    // File/table relations use an arbitrary per-record computed
                    // file path (see `Location::At`) — a JsonAdapter-specific
                    // idiom (one file per record) that has no DuckDB analog, so
                    // (unlike local/foreign relations) this always resolves
                    // against the "default" adapter rather than a per-target
                    // adapter_name() — dumb (`no_location`) target types don't
                    // implement `TypiQLType` at all, so there's no adapter_name()
                    // to read here even in principle.
                    let __adapters = ctx.data::<::typiql::AdapterMap>()?;
                    let __adapter = __adapters.get("default").ok_or_else(|| {
                        ::async_graphql::Error::new("no adapter registered under name \"default\"")
                    })?;
                    let __file = ::std::path::PathBuf::from(::std::format!(#template, #(#tpl_field_idents = self.#tpl_field_idents),*));
                    let __rows = __adapter.get_many(::typiql::Location::At { file: __file, table: #table.to_string() }, ::std::vec::Vec::new()).await;
                    ::std::result::Result::Ok(__rows.into_iter().next().and_then(|v| ::serde_json::from_value(v).ok()))
                }
            });
        }

        quote! {
            #[::async_graphql::ComplexObject]
            impl #name {
                #(#resolvers)*
            }
        }
    });

    let input_name = format_ident!("{}Input", name);

    let vis = &input.vis;
    let other_struct_attrs: Vec<_> = input
        .attrs
        .iter()
        .filter(|a| !a.path().is_ident("typiql_type"))
        .collect();
    let complex_attr = has_relations.then(|| quote! { #[graphql(complex)] });

    // Dumb types (`#[typiql_type(no_location)]`) get the struct/ComplexObject/
    // Input generated above like any other type (so they're usable as a
    // relation's target and as a hand-written mutation's `update` argument),
    // but no location-driven auto-CRUD — there's no default collection for
    // `TypiQLType`/`{Name}Query`/`Mutation`/`Changed`/`Subscription` to
    // operate against, and nothing to register in `typiql_schema!`. Reading
    // happens only via a relation field on another type (see the
    // ComplexObject block above); writing needs a small hand-written
    // resolver using `Location::At` directly.
    let crud_block = (!is_dumb).then(|| {
        let key_field_str = field_infos[key_idx.unwrap_or(0)].ident.to_string();
        let key_field_ident = &field_infos[key_idx.unwrap_or(0)].ident;

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

        let adapter_name_fn = (adapter_str != "default").then(|| {
            quote! { fn adapter_name() -> &'static str { #adapter_str } }
        });
        let duckdb_columns_fn = is_duckdb.then(|| {
            quote! {
                fn duckdb_columns() -> &'static [(&'static str, &'static str)] {
                    &[ #(#duckdb_column_entries),* ]
                }
            }
        });

        quote! {
            impl ::typiql::TypiQLType for #name {
                type Input = #input_name;
                fn type_name()       -> &'static str { #name_str }
                fn key_field()       -> &'static str { #key_field_str }
                fn collection_name() -> &'static str { #collection_name }
                #adapter_name_fn
                #duckdb_columns_fn
            }

            // ── Change event broadcast type ──────────────────────────────────

            #[derive(::async_graphql::SimpleObject, ::std::clone::Clone, ::std::fmt::Debug)]
            #vis struct #changed_struct {
                pub operation_name: ::std::string::String,
                pub value: #name,
            }

            // ── CRUD queries ─────────────────────────────────────────────────

            #[derive(::std::default::Default)]
            #vis struct #query_struct;

            #[::async_graphql::Object]
            impl #query_struct {
                // Filter-based, same argument set as the list query below
                // (including the key field, unlike a plain exact-key
                // lookup) — first match wins, no match returns null. Making
                // a required argument optional is never a breaking change,
                // so callers that only ever passed the key are unaffected.
                async fn #get_fn(
                    &self,
                    ctx: &::async_graphql::Context<'_>,
                    #(#filter_args)*
                ) -> ::async_graphql::Result<::std::option::Option<#name>> {
                    let mut filters: ::std::vec::Vec<::typiql::FieldFilter> = ::std::vec::Vec::new();
                    #(#filter_inserts)*
                    let mut __results = ::typiql::resolve_list::<#name>(ctx, filters).await?;
                    ::std::result::Result::Ok(if __results.is_empty() {
                        ::std::option::Option::None
                    } else {
                        ::std::option::Option::Some(__results.remove(0))
                    })
                }

                async fn #get_list_fn(
                    &self,
                    ctx: &::async_graphql::Context<'_>,
                    #(#filter_args)*
                ) -> ::async_graphql::Result<::std::vec::Vec<#name>> {
                    let mut filters: ::std::vec::Vec<::typiql::FieldFilter> = ::std::vec::Vec::new();
                    #(#filter_inserts)*
                    ::typiql::resolve_list::<#name>(ctx, filters).await
                }
            }

            // ── CRUD mutations — publish change events on success ────────────

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

            // ── Real-time subscription ───────────────────────────────────────

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
        }
    });

    let expanded = quote! {
        #[derive(
            ::async_graphql::SimpleObject,
            ::serde::Serialize,
            ::serde::Deserialize,
            ::std::clone::Clone,
            ::std::fmt::Debug,
        )]
        #complex_attr
        #(#other_struct_attrs)*
        #vis struct #name {
            #(#struct_fields)*
        }

        #complex_object_block

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

        #crud_block
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
            if input.peek(Token![,]) {
                let _: Token![,] = input.parse()?;
            }
        }

        let mut extra_queries = vec![];
        if input.peek(Token![;]) {
            let _: Token![;] = input.parse()?;
            while !input.is_empty() && !input.peek(Token![;]) {
                extra_queries.push(input.parse::<Ident>()?);
                if input.peek(Token![,]) {
                    let _: Token![,] = input.parse()?;
                }
            }
        }

        let mut extra_mutations = vec![];
        if input.peek(Token![;]) {
            let _: Token![;] = input.parse()?;
            while !input.is_empty() && !input.peek(Token![;]) {
                extra_mutations.push(input.parse::<Ident>()?);
                if input.peek(Token![,]) {
                    let _: Token![,] = input.parse()?;
                }
            }
        }

        // Extra subscription roots to merge alongside the per-type generated ones.
        let mut extra_subscriptions = vec![];
        if input.peek(Token![;]) {
            let _: Token![;] = input.parse()?;
            while !input.is_empty() {
                extra_subscriptions.push(input.parse::<Ident>()?);
                if input.peek(Token![,]) {
                    let _: Token![,] = input.parse()?;
                }
            }
        }

        Ok(TypiQLSchemaInput {
            types,
            extra_queries,
            extra_mutations,
            extra_subscriptions,
        })
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
///   - `build_typiql_schema(adapters)` — schema builder function, taking a
///     `HashMap<&'static str, Arc<dyn TypiQLAdapter>>` keyed by adapter name
///     (see `#[typiql_type(adapter = "…")]`). Registers the whole map (for
///     custom resolvers, and for `resolve_add`/`list`/`update`/`remove` to
///     pick the right adapter per type) plus one `DataLoader<RelationLoader<T>>`
///     per type in the list, each built against `adapters[T::adapter_name()]`
///     — so a relation field pointing at a DuckDB-backed type resolves
///     against DuckDB even when the parent type is JSON-backed, with no
///     relation-specific special-casing.
#[proc_macro]
pub fn typiql_schema(input: TokenStream) -> TokenStream {
    let schema_input = parse_macro_input!(input as TypiQLSchemaInput);

    let gen_query_types: Vec<proc_macro2::Ident> = schema_input
        .types
        .iter()
        .map(|t| format_ident!("{}Query", t))
        .collect();
    let gen_mutation_types: Vec<proc_macro2::Ident> = schema_input
        .types
        .iter()
        .map(|t| format_ident!("{}Mutation", t))
        .collect();
    let gen_subscription_types: Vec<proc_macro2::Ident> = schema_input
        .types
        .iter()
        .map(|t| format_ident!("{}Subscription", t))
        .collect();

    let all_query_types: Vec<&proc_macro2::Ident> = gen_query_types
        .iter()
        .chain(schema_input.extra_queries.iter())
        .collect();
    let all_mutation_types: Vec<&proc_macro2::Ident> = gen_mutation_types
        .iter()
        .chain(schema_input.extra_mutations.iter())
        .collect();
    let all_subscription_types: Vec<&proc_macro2::Ident> = gen_subscription_types
        .iter()
        .chain(schema_input.extra_subscriptions.iter())
        .collect();

    let types = &schema_input.types;

    let expanded = quote! {
        #[derive(::async_graphql::MergedObject, ::std::default::Default)]
        pub struct TypiQLQuery(#(#all_query_types),*);

        #[derive(::async_graphql::MergedObject, ::std::default::Default)]
        pub struct TypiQLMutation(#(#all_mutation_types),*);

        #[derive(::async_graphql::MergedSubscription, ::std::default::Default)]
        pub struct TypiQLSubscription(#(#all_subscription_types),*);

        pub fn build_typiql_schema(
            adapters: ::typiql::AdapterMap,
        ) -> ::async_graphql::Schema<
            TypiQLQuery,
            TypiQLMutation,
            TypiQLSubscription,
        > {
            // Give every type's adapter a chance to set up a fixed schema
            // (e.g. DuckDB's CREATE TABLE) from that type's own
            // duckdb_columns(), before any query can run. A no-op for
            // adapters with no fixed schema (JSON, ...).
            #(
                adapters.get(<#types as ::typiql::TypiQLType>::adapter_name())
                    .unwrap_or_else(|| panic!(
                        "typiql_schema!: no adapter registered under name {:?} (needed by type {:?})",
                        <#types as ::typiql::TypiQLType>::adapter_name(),
                        <#types as ::typiql::TypiQLType>::type_name(),
                    ))
                    .register_type(<#types as ::typiql::TypiQLType>::collection_name(), <#types as ::typiql::TypiQLType>::duckdb_columns());
            )*

            ::async_graphql::Schema::build(
                TypiQLQuery::default(),
                TypiQLMutation::default(),
                TypiQLSubscription::default(),
            )
            .data(adapters.clone())
            #(
                .data(::async_graphql::dataloader::DataLoader::new(
                    ::typiql::RelationLoader::<#types>::new(
                        adapters.get(<#types as ::typiql::TypiQLType>::adapter_name())
                            .unwrap_or_else(|| panic!(
                                "typiql_schema!: no adapter registered under name {:?} (needed by type {:?})",
                                <#types as ::typiql::TypiQLType>::adapter_name(),
                                <#types as ::typiql::TypiQLType>::type_name(),
                            ))
                            .clone(),
                    ),
                    ::tokio::spawn,
                ))
            )*
            .finish()
        }
    };

    expanded.into()
}
