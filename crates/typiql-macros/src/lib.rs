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

/// Strips one layer of `Box<...>` if present — a has-one relation's target
/// may be boxed (`Option<Box<Item>>`), which two types relating to each
/// other one-to-one need to have a finite size.
fn unwrap_box(ty: &syn::Type) -> &syn::Type {
    if let syn::Type::Path(type_path) = ty {
        if let Some(segment) = type_path.path.segments.last() {
            if segment.ident == "Box" {
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

/// A relation field's target type: `T` in `Vec<T>`, `Option<T>` or
/// `Option<Box<T>>`.
fn relation_target(ty: &syn::Type) -> &syn::Type {
    if is_vec_type(ty) {
        vec_inner_type(ty)
    } else {
        unwrap_box(unwrap_option(ty))
    }
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
/// `TypiQLType::duckdb_columns()` at macro-expansion time. Scalars get their natural DuckDB
/// equivalent; anything else (nested structs, `Vec<T>`, enums without a
/// direct scalar mapping) is stored as text — `"JSON"` for containers
/// (always JSON-encoded), `"ANY"` for other named types (plain text when the
/// value serializes to a string, as a unit enum does; JSON otherwise) — matching the "big/irregular data as a
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
        // Containers are always structured: stored as JSON text.
        Some("Vec" | "HashMap" | "BTreeMap" | "HashSet" | "BTreeSet" | "Value") => "JSON",
        // Any other named type — an enum serializing to a plain string, or
        // a nested struct — is decided per value by the adapter (see
        // typiql-adapter-duckdb's `ANY` handling).
        _ => "ANY",
    };
    if nullable {
        base.to_string()
    } else {
        format!("{} NOT NULL", base)
    }
}

/// One generated filter-operator variant for a field: `suffix` is appended to
/// the field's Rust name for the resolver argument (`""` for bare equality;
/// its GraphQL name comes from `csharp_suffix` instead), `op` is
/// the `FilterOp` variant ident to emit, `is_list` means the argument takes
/// `Vec<T>` (for `In`/`NotIn`) rather than a scalar.
struct OpSpec {
    suffix: &'static str,
    op: &'static str,
    is_list: bool,
}

/// The GraphQL argument suffix for a filter operator — C#'s names
/// (`name_startsWith`, `count_gt`, `id_in`, `active_not`), so clients written
/// against the original typiql (Q, its admin lists) work unchanged. Appended
/// to the field's GraphQL (camelCase) name; bare equality has no suffix.
fn csharp_suffix(op: &str) -> &'static str {
    match op {
        "Eq" => "",
        "Ne" => "_not",
        "Gt" => "_gt",
        "Gte" => "_gte",
        "Lt" => "_lt",
        "Lte" => "_lte",
        "In" => "_in",
        "NotIn" => "_notIn",
        "StartsWith" => "_startsWith",
        "EndsWith" => "_endsWith",
        "Contains" => "_contains",
        "NotStartsWith" => "_notStartsWith",
        "NotEndsWith" => "_notEndsWith",
        "NotContains" => "_notContains",
        "AnyIn" => "_anyEq",
        "NoneIn" => "_anyNe",
        other => panic!("no C# suffix for filter op {other}"),
    }
}

/// A plain value type: string, bool or number (after unwrapping `Option`).
fn is_scalar(ty: &syn::Type) -> bool {
    matches!(
        scalar_type_name(ty).as_deref(),
        Some(
            "String" | "str" | "bool" | "f32" | "f64" | "i8" | "i16" | "i32" | "i64" | "i128" | "isize"
                | "u8" | "u16" | "u32" | "u64" | "u128" | "usize"
        )
    )
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
    // A list of plain values also gets C#'s `anyEq` / `anyNe`: contains
    // any / none of the given values.
    let list = unwrap_option(ty);
    if is_vec_type(list) && is_scalar(vec_inner_type(list)) {
        let mut specs: Vec<OpSpec> = EQ_NE
            .iter()
            .map(|s| OpSpec { suffix: s.suffix, op: s.op, is_list: s.is_list })
            .collect();
        specs.push(OpSpec { suffix: "_any_eq", op: "AnyIn", is_list: false });
        specs.push(OpSpec { suffix: "_any_ne", op: "NoneIn", is_list: false });
        return specs;
    }
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

/// Where a relation condition's value comes from — C#'s relation argument
/// values: the parent's own field (`local = "…"`), or a literal / variable
/// such as `"@currentUser"` (`value = …`).
enum ConditionSource {
    Local(String),
    Value(syn::Lit),
}

/// One condition of a relation field: the related type's `foreign` field
/// `op` the value from `source`. A relation field with more than one
/// condition (repeated `#[typiql(relation(...))]` attributes on the same
/// field) is a composite relation.
struct RelationCondition {
    source: ConditionSource,
    /// `FilterOp` variant name.
    op: String,
    foreign: String,
}

/// A relation's own sorting and paging — C#'s `_orderBy`, `_orderBy_desc`,
/// `_start` and `_limit` relation arguments (e.g. "the 5 most recent
/// visits"). Field names are the related type's Rust field names.
#[derive(Default)]
struct RelationOptions {
    sort: Vec<(String, bool)>,
    start: Option<usize>,
    limit: Option<usize>,
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
    relation_options: RelationOptions,
    file_table_relation: Option<FileTableRelation>,
    /// `#[typiql(elements)]`: a `Vec<T>` of `TypiQLElement`s, filterable
    /// element by element.
    elements: bool,
    /// `#[typiql(log)]` on a relation field: log its successful resolutions.
    log: bool,
    other_attrs: Vec<syn::Attribute>,
}

impl FieldInfo {
    /// The name the adapter stores this field under — `data_name` if given,
    /// else the Rust field name. GraphQL never sees it: the schema always
    /// uses the Rust field name, whatever the store calls the field
    /// (differently-cased columns, an alias, a SQL subquery, ...).
    fn storage_name(&self) -> String {
        self.data_name.clone().unwrap_or_else(|| self.ident.to_string())
    }

    fn is_relation(&self) -> bool {
        !self.relations.is_empty() || self.file_table_relation.is_some()
    }
}

fn parse_field_info(f: &syn::Field) -> syn::Result<FieldInfo> {
    let ident = f.ident.as_ref().unwrap().clone();
    let ty = f.ty.clone();
    let mut is_key = false;
    let mut data_name: Option<String> = None;
    let mut relations: Vec<RelationCondition> = Vec::new();
    let mut relation_options = RelationOptions::default();
    let mut file_table_relation: Option<FileTableRelation> = None;
    let mut elements = false;
    let mut log = false;
    let mut other_attrs = Vec::new();

    for attr in &f.attrs {
        if !attr.path().is_ident("typiql") {
            other_attrs.push(attr.clone());
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("key") {
                is_key = true;
            } else if meta.path.is_ident("log") {
                log = true;
            } else if meta.path.is_ident("elements") {
                if !is_vec_type(unwrap_option(&ty)) {
                    return Err(meta.error("`elements` needs a `Vec<T>` field whose `T` derives `TypiQLElement`"));
                }
                elements = true;
            } else if meta.path.is_ident("data_name") {
                let s: LitStr = meta.value()?.parse()?;
                data_name = Some(s.value());
            } else if meta.path.is_ident("relation") {
                let mut local: Option<String> = None;
                let mut value: Option<syn::Lit> = None;
                let mut op: Option<LitStr> = None;
                let mut foreign: Option<String> = None;
                let mut file: Option<String> = None;
                let mut table: Option<String> = None;
                meta.parse_nested_meta(|inner| {
                    let str_value = |inner: &syn::meta::ParseNestedMeta| -> syn::Result<String> {
                        Ok(inner.value()?.parse::<LitStr>()?.value())
                    };
                    let sort_list = |list: String, desc: bool| {
                        list.split(',')
                            .map(str::trim)
                            .filter(|f| !f.is_empty())
                            .map(|f| (f.to_string(), desc))
                            .collect::<Vec<_>>()
                    };
                    if inner.path.is_ident("local") {
                        local = Some(str_value(&inner)?);
                    } else if inner.path.is_ident("value") {
                        value = Some(inner.value()?.parse()?);
                    } else if inner.path.is_ident("op") {
                        op = Some(inner.value()?.parse()?);
                    } else if inner.path.is_ident("foreign") {
                        foreign = Some(str_value(&inner)?);
                    } else if inner.path.is_ident("file") {
                        file = Some(str_value(&inner)?);
                    } else if inner.path.is_ident("table") {
                        table = Some(str_value(&inner)?);
                    } else if inner.path.is_ident("order_by") {
                        let list = str_value(&inner)?;
                        relation_options.sort.extend(sort_list(list, false));
                    } else if inner.path.is_ident("order_by_desc") {
                        let list = str_value(&inner)?;
                        relation_options.sort.extend(sort_list(list, true));
                    } else if inner.path.is_ident("start") {
                        relation_options.start = Some(inner.value()?.parse::<syn::LitInt>()?.base10_parse()?);
                    } else if inner.path.is_ident("limit") {
                        relation_options.limit = Some(inner.value()?.parse::<syn::LitInt>()?.base10_parse()?);
                    } else {
                        return Err(inner.error(
                            "expected local, value, op, foreign, order_by, order_by_desc, start, limit, file or table",
                        ));
                    }
                    Ok(())
                })?;
                if let (Some(file), Some(table)) = (file, table) {
                    file_table_relation = Some(FileTableRelation {
                        file_template: file,
                        table,
                    });
                } else if let Some(foreign) = foreign {
                    let source = match (local, value) {
                        (Some(l), None) => ConditionSource::Local(l),
                        (None, Some(v)) => ConditionSource::Value(v),
                        _ => {
                            return Err(meta.error(
                                "a relation condition needs exactly one of `local = \"field\"` or `value = …`",
                            ))
                        }
                    };
                    let op = match op {
                        Some(op) => op_variant(&op.value())
                            .ok_or_else(|| syn::Error::new_spanned(&op, format!("unknown relation op {:?}", op.value())))?,
                        None => "Eq",
                    };
                    relations.push(RelationCondition { source, op: op.to_string(), foreign });
                } else if local.is_some() || value.is_some() || op.is_some() {
                    return Err(meta.error("a relation condition needs `foreign = \"field\"`"));
                }
            }
            Ok(())
        })?;
    }

    if log && relations.is_empty() && file_table_relation.is_none() {
        return Err(syn::Error::new_spanned(
            &ident,
            "`#[typiql(log)]` goes on a relation field; plain fields are logged with the operation that returns them",
        ));
    }
    if relations.is_empty()
        && (!relation_options.sort.is_empty() || relation_options.start.is_some() || relation_options.limit.is_some())
    {
        return Err(syn::Error::new_spanned(
            &ident,
            "relation sorting/paging needs at least one relation condition",
        ));
    }

    Ok(FieldInfo {
        ident,
        ty,
        is_key,
        data_name,
        relations,
        relation_options,
        file_table_relation,
        elements,
        log,
        other_attrs,
    })
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

/// A relation condition's `op`, by either its C# name (`not`, `notIn`,
/// `startsWith`, …) or its Rust one (`ne`, `not_in`, `starts_with`, …) —
/// the `FilterOp` variant name.
fn op_variant(op: &str) -> Option<&'static str> {
    Some(match op {
        "eq" => "Eq",
        "ne" | "not" => "Ne",
        "gt" => "Gt",
        "gte" => "Gte",
        "lt" => "Lt",
        "lte" => "Lte",
        "in" => "In",
        "not_in" | "notIn" => "NotIn",
        "starts_with" | "startsWith" => "StartsWith",
        "ends_with" | "endsWith" => "EndsWith",
        "contains" => "Contains",
        "not_starts_with" | "notStartsWith" => "NotStartsWith",
        "not_ends_with" | "notEndsWith" => "NotEndsWith",
        "not_contains" | "notContains" => "NotContains",
        _ => return None,
    })
}

// ── declared CRUD surface: #[typiql_type(ops(...))] ─────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum OpKind {
    Get,
    List,
    Add,
    Update,
    Remove,
    Subscribe,
    AddMany,
    UpdateMany,
    RemoveMany,
}

impl OpKind {
    const ALL: [OpKind; 9] = [
        OpKind::Get,
        OpKind::List,
        OpKind::Add,
        OpKind::Update,
        OpKind::Remove,
        OpKind::Subscribe,
        OpKind::AddMany,
        OpKind::UpdateMany,
        OpKind::RemoveMany,
    ];

    /// The surface a type gets without `ops(...)`. The bulk operations are
    /// opt-in only, so no type gains a mutation that can change many rows
    /// without declaring it.
    const DEFAULT: [OpKind; 6] = [
        OpKind::Get,
        OpKind::List,
        OpKind::Add,
        OpKind::Update,
        OpKind::Remove,
        OpKind::Subscribe,
    ];

    fn keyword(self) -> &'static str {
        match self {
            OpKind::Get => "get",
            OpKind::List => "list",
            OpKind::Add => "add",
            OpKind::Update => "update",
            OpKind::Remove => "remove",
            OpKind::Subscribe => "subscribe",
            OpKind::AddMany => "add_many",
            OpKind::UpdateMany => "update_many",
            OpKind::RemoveMany => "remove_many",
        }
    }

    /// Operations that select their rows with filter arguments.
    fn takes_filters(self) -> bool {
        matches!(self, OpKind::Get | OpKind::List | OpKind::UpdateMany | OpKind::RemoveMany)
    }

    fn takes_scope(self) -> bool {
        !matches!(self, OpKind::Add | OpKind::AddMany)
    }

    fn takes_set(self) -> bool {
        matches!(self, OpKind::Add | OpKind::Update | OpKind::AddMany | OpKind::UpdateMany)
    }
}

/// One declared operation — the Rust counterpart of one entry in a C# type's
/// `queries`/`mutations`/`subscriptions` list:
///
/// - `name = "…"`: the GraphQL field name (C#'s function `name`), replacing
///   the generated default (`getCar`, `addCar`, ...). Lets a type declare
///   the same kind of operation more than once, e.g. a plain `get` plus a
///   `get(name = "my", scope(username = "@currentUser"))`.
/// - `filters(field, ...)`: `get`/`list`/`update_many`/`remove_many`. The client-facing filter
///   fields; blank means every plain field. Each listed field still gets
///   every operator its type supports — as C#'s `FilterArgs` expanded each
///   field declared in a function's signature.
/// - `roles("…", ...)`: C#'s `allowedGroups` — the user must hold one of
///   them. None declared means open to everyone.
/// - `scope(field = value, ...)`: C#'s fixed `arguments` — equality filters
///   the server always applies and the client can't see or override (scoped
///   fields are dropped from the filter arguments). On `update`/`remove` a
///   key outside the scope behaves as missing.
/// - `set(field = value, ...)`: `add`/`update` only. C#'s `values` arguments
///   — server-side values overwriting whatever the client sent.
/// - `by(field, ...)`: `update` only. Find the row by these fields (all
///   equality) instead of the key; `by()` finds it by the scope alone.
/// - `upsert`: `update` only. Create the row when none matches, always
///   (C#'s fixed `_upsert: true`); otherwise clients may pass `_upsert`.
///
/// `scope`/`set` values are literals, or `"@currentUser"`,
/// `"@currentUser.<field>"`, `"@now"` (resolved per request by
/// `typiql::resolve_value`).
struct OpDecl {
    kind: OpKind,
    name: Option<LitStr>,
    filters: Option<Vec<Ident>>,
    /// `update` only: identify the row by these fields (all equality)
    /// instead of the key; `Some(vec![])` means by the scope alone.
    by: Option<Vec<Ident>>,
    /// `update` only: always upsert (C#'s fixed `_upsert: true`), and don't
    /// expose `_upsert` to clients.
    upsert: bool,
    /// Log this operation's successful calls (errors are always logged).
    log: bool,
    roles: Vec<LitStr>,
    scope: Vec<(Ident, syn::Lit)>,
    set: Vec<(Ident, syn::Lit)>,
}

impl OpDecl {
    fn plain(kind: OpKind) -> Self {
        OpDecl {
            kind,
            name: None,
            filters: None,
            by: None,
            upsert: false,
            log: false,
            roles: Vec::new(),
            scope: Vec::new(),
            set: Vec::new(),
        }
    }
}

/// `meta.parse_nested_meta`, except empty parentheses — `ops()`, `filters()` —
/// are accepted as an explicitly empty list instead of rejected.
fn parse_nested_allow_empty(
    meta: &syn::meta::ParseNestedMeta,
    logic: impl FnMut(syn::meta::ParseNestedMeta) -> syn::Result<()>,
) -> syn::Result<()> {
    let content;
    syn::parenthesized!(content in meta.input);
    syn::meta::parser(logic).parse2(content.parse()?)
}

/// `(a, b, ...)` after a nested meta's path.
fn parse_paren_list<T: syn::parse::Parse>(
    meta: &syn::meta::ParseNestedMeta,
) -> syn::Result<Vec<T>> {
    let content;
    syn::parenthesized!(content in meta.input);
    Ok(syn::punctuated::Punctuated::<T, Token![,]>::parse_terminated(&content)?
        .into_iter()
        .collect())
}

/// `(field = literal, ...)` after a nested meta's path.
fn parse_assignments(meta: &syn::meta::ParseNestedMeta) -> syn::Result<Vec<(Ident, syn::Lit)>> {
    let mut out = Vec::new();
    parse_nested_allow_empty(meta, |a| {
        let field = a
            .path
            .get_ident()
            .cloned()
            .ok_or_else(|| a.error("expected a field name"))?;
        let value: syn::Lit = a.value()?.parse()?;
        out.push((field, value));
        Ok(())
    })?;
    Ok(out)
}

/// Parses the inside of `ops(...)` — see `OpDecl` for each operation's options.
fn parse_ops(meta: &syn::meta::ParseNestedMeta) -> syn::Result<Vec<OpDecl>> {
    let mut ops = Vec::new();
    parse_nested_allow_empty(meta, |op| {
        let kind = OpKind::ALL
            .into_iter()
            .find(|k| op.path.is_ident(k.keyword()))
            .ok_or_else(|| {
                op.error("unknown operation — expected get, list, add, update, remove or subscribe")
            })?;
        let mut decl = OpDecl::plain(kind);
        if op.input.peek(syn::token::Paren) {
            parse_nested_allow_empty(&op, |opt| {
                let not_here = |what: &str| {
                    Err(opt.error(format!("`{what}` doesn't apply to `{}`", kind.keyword())))
                };
                if opt.path.is_ident("name") {
                    decl.name = Some(opt.value()?.parse()?);
                } else if opt.path.is_ident("by") {
                    if kind != OpKind::Update {
                        return not_here("by");
                    }
                    decl.by = Some(parse_paren_list(&opt)?);
                } else if opt.path.is_ident("log") {
                    if kind == OpKind::Subscribe {
                        return not_here("log");
                    }
                    decl.log = true;
                } else if opt.path.is_ident("upsert") {
                    if kind != OpKind::Update {
                        return not_here("upsert");
                    }
                    decl.upsert = true;
                } else if opt.path.is_ident("filters") {
                    if !kind.takes_filters() {
                        return not_here("filters");
                    }
                    decl.filters = Some(parse_paren_list(&opt)?);
                } else if opt.path.is_ident("roles") {
                    decl.roles = parse_paren_list(&opt)?;
                } else if opt.path.is_ident("scope") {
                    if !kind.takes_scope() {
                        return not_here("scope");
                    }
                    decl.scope = parse_assignments(&opt)?;
                } else if opt.path.is_ident("set") {
                    if !kind.takes_set() {
                        return not_here("set");
                    }
                    decl.set = parse_assignments(&opt)?;
                } else {
                    return Err(opt.error("expected name, filters, roles, scope, set, by, upsert or log"));
                }
                Ok(())
            })?;
        }
        ops.push(decl);
        Ok(())
    })?;
    Ok(ops)
}

fn to_camel_case(snake: &str) -> String {
    let mut out = String::new();
    let mut upper = false;
    for c in snake.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            out.push(c.to_ascii_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
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
///   - `#[typiql(data_name = "…")]`  — the name the adapter stores/queries
///     this field under (a differently-cased column, an alias, a SQL
///     subquery, ...). Adapter-side only: GraphQL (fields, inputs, filter
///     arguments) always uses the Rust field name, and so do declarations
///     (`filters`, `scope`, `set`, relation `local`/`foreign`).
///   - `#[typiql(relation(local = "…", op = "…", foreign = "…"))]` — declares
///     this field (typed as another `#[typiql_type]` struct: `Option<T>`,
///     `Option<Box<T>>` or `Vec<T>`) as a relation: the target's `foreign`
///     field `op` (default `eq`; any filter operator, by its C# or Rust
///     name) this struct's `local` field. `value = …` instead of `local`
///     compares against a literal or a variable (`"@currentUser"`). Repeat
///     the attribute for more conditions (all must hold). `order_by`,
///     `order_by_desc`, `start` and `limit` sort and page the relation's own
///     rows (C#'s relation `_orderBy`/`_orderBy_desc`/`_start`/`_limit`).
///     Has-one relations load through a `DataLoader`, batched across a whole
///     query — see `typiql_schema!`, which registers one
///     `DataLoader<RelationLoader<T>>` per type. With `data_name`, the
///     relation is instead joined into this type's own reads under that
///     name (C#'s relation columns with a DataName), so it's sortable as
///     `_orderBy: "field.targetField"`.
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
/// Struct-level `#[typiql_type(ops(...))]` declares the type's CRUD surface
/// — only the listed operations are generated (omit it for all of them):
///
/// ```rust,ignore
/// #[typiql_type(ops(
///     add(roles("User"), set(owner = "@currentUser", created_at = "@now")),
///     update(roles("User"), scope(owner = "@currentUser")),
///     remove(roles("User"), scope(owner = "@currentUser")),
///     list(name = "allCars", roles("Admin"), filters(make, model)),
/// ))]
/// ```
///
/// Operations are `get`, `list`, `add`, `update`, `remove`, `subscribe`,
/// each optionally taking `name = "…"`, `roles(...)`, `scope(...)`,
/// `set(...)` and (`get`/`list`) `filters(...)` — see `OpDecl`. `roles`
/// and `@currentUser` values need the app's user type attached to the
/// request (`typiql::TypiQLRouter::with_user`); without one they fail as
/// not authenticated, and an app that uses neither never needs a user.
///
/// Struct-level `#[typiql_type(adapter = "…")]` picks which registered
/// adapter (see `typiql_schema!`'s `HashMap<&str, Arc<dyn TypiQLAdapter>>`)
/// stores this type — defaults to `"default"` if omitted. The name only
/// picks a map entry: every type derives `TypiQLType::duckdb_columns()`
/// from its non-relation fields' Rust types (see `duckdb_type_for`), so
/// whichever adapter the name maps to — `DuckDbAdapter` creating its table
/// with no hand-written DDL, or a schemaless adapter ignoring the columns —
/// the type needs no other change to move between adapters. Both attributes
/// can be combined: `#[typiql_type(no_location, adapter = "duckdb")]`.
#[proc_macro_attribute]
pub fn typiql_type(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut is_dumb = false;
    let mut adapter_attr: Option<String> = None;
    let mut ops_attr: Option<Vec<OpDecl>> = None;
    let mut log_all = false;
    let mut is_log_store = false;
    let attr_parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("no_location") {
            is_dumb = true;
        } else if meta.path.is_ident("log") {
            log_all = true;
        } else if meta.path.is_ident("__log_store") {
            is_log_store = true;
        } else if meta.path.is_ident("adapter") {
            let s: LitStr = meta.value()?.parse()?;
            adapter_attr = Some(s.value());
        } else if meta.path.is_ident("ops") {
            if ops_attr.is_some() {
                return Err(meta.error("`ops(...)` declared more than once"));
            }
            ops_attr = Some(parse_ops(&meta)?);
        }
        Ok(())
    });
    parse_macro_input!(attr with attr_parser);
    let adapter_str = adapter_attr.unwrap_or_else(|| "default".to_string());

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

    let field_infos: Vec<FieldInfo> = match fields.iter().map(|f| parse_field_info(f)).collect() {
        Ok(infos) => infos,
        Err(e) => return e.to_compile_error().into(),
    };

    // A relation's `local` must be a plain field of this struct.
    for fi in &field_infos {
        for r in &fi.relations {
            if let ConditionSource::Local(local) = &r.source {
                if !field_infos.iter().any(|o| o.ident == local.as_str() && !o.is_relation()) {
                    return syn::Error::new_spanned(
                        &fi.ident,
                        format!("relation `local = \"{local}\"` isn't a plain field of `{}`", input.ident),
                    )
                    .to_compile_error()
                    .into();
                }
            }
        }
    }

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
        // Same rename as the struct itself, so what the client sends is
        // stored under the field's `data_name`.
        let serde_rename = fi.data_name.as_ref().map(|dn| quote! { #[serde(rename = #dn)] });
        quote! {
            #serde_rename
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
    // derived for every type, whatever adapter name it declares, so which
    // adapter actually serves a type stays purely a matter of the adapter
    // map: a schemaless adapter ignores them (`register_type` is a no-op),
    // and a type moved onto DuckDB already has its DDL.
    let duckdb_column_entries: Vec<TokenStream2> = filterable
        .iter()
        .map(|fi| {
            let col_name = fi.storage_name();
            let col_type = duckdb_type_for(&fi.ty);
            quote! { (#col_name, #col_type) }
        })
        .collect();

    // Two fields sharing one storage name (an alias) can't go through the
    // plain serde rename: reads fill only the first field, and writes emit
    // the key twice — most JSON parsers keep the last, so saving would
    // clobber the real value. Refused until aliasing gets real support.
    for (i, fi) in field_infos.iter().enumerate().filter(|(_, fi)| !fi.is_relation()) {
        let clash = field_infos[..i]
            .iter()
            .filter(|other| !other.is_relation())
            .find(|other| other.storage_name() == fi.storage_name());
        if let Some(other) = clash {
            return syn::Error::new_spanned(
                &fi.ident,
                format!(
                    "`{}` and `{}` are both stored as `{}` — aliasing one stored field under two names isn't supported yet",
                    other.ident,
                    fi.ident,
                    fi.storage_name()
                ),
            )
            .to_compile_error()
            .into();
        }
    }

    if is_dumb && ops_attr.is_some() {
        return syn::Error::new_spanned(
            name,
            "`ops(...)` has no effect on a `no_location` type — it gets no generated CRUD",
        )
        .to_compile_error()
        .into();
    }
    // Omitting `ops(...)` keeps the full surface — every existing type's
    // behavior. Declaring it generates only what's listed: an operation left
    // out doesn't exist in the schema, rather than existing and being
    // refused at runtime (as a C# type only had its configured functions).
    let ops: Vec<OpDecl> =
        ops_attr.unwrap_or_else(|| OpKind::DEFAULT.into_iter().map(OpDecl::plain).collect());

    // Every field an operation names must be a plain field of this struct —
    // a typo or a relation field is a compile error here, not a silently
    // missing argument or an unscoped query.
    let key_ident = key_idx.map(|i| &field_infos[i].ident);
    for op in &ops {
        let named = op
            .filters
            .iter()
            .flatten()
            .chain(op.scope.iter().map(|(f, _)| f))
            .chain(op.set.iter().map(|(f, _)| f))
            .chain(op.by.iter().flatten());
        for f in named {
            if !filterable.iter().any(|fi| &fi.ident == f) {
                return syn::Error::new_spanned(
                    f,
                    format!("`{f}` is not a plain field of `{name}` (relation fields can't be used here)"),
                )
                .to_compile_error()
                .into();
            }
        }
        for (f, _) in &op.set {
            if Some(f) == key_ident {
                return syn::Error::new_spanned(f, "the key field can't be `set`")
                    .to_compile_error()
                    .into();
            }
        }
        if op.by.as_ref().is_some_and(|b| b.is_empty()) && op.scope.is_empty() {
            return syn::Error::new_spanned(
                name,
                "`update(by())` finds its row by the scope alone, so it needs a `scope(...)`",
            )
            .to_compile_error()
            .into();
        }
        for f in op.filters.iter().flatten() {
            if op.scope.iter().any(|(s, _)| s == f) {
                return syn::Error::new_spanned(
                    f,
                    format!("`{f}` is scoped, so it can't also be a client filter"),
                )
                .to_compile_error()
                .into();
            }
        }
    }

    // One GraphQL argument per applicable filter operator per filterable
    // field — e.g. `count: Option<i32>, count_ne: Option<i32>, count_gt:
    // Option<i32>, ...` — narrowed to the operation's declared `filters(...)`
    // fields, if it declared any, and never including a scoped field (the
    // server decides those). Returns (arguments, filter-building code).
    let filter_code = |op: &OpDecl| -> (Vec<TokenStream2>, Vec<TokenStream2>) {
        let mut filter_args: Vec<TokenStream2> = Vec::new();
        let mut filter_inserts: Vec<TokenStream2> = Vec::new();
        let fields = filterable.iter().filter(|fi| {
            op.filters.as_ref().is_none_or(|declared| declared.contains(&fi.ident))
                && !op.scope.iter().any(|(s, _)| s == &fi.ident)
        });
        for fi in fields {
            let field_str = fi.storage_name();
            let inner_ty = unwrap_option(&fi.ty);
            if fi.elements {
                let elem = vec_inner_type(inner_ty);
                let camel = to_camel_case(&fi.ident.to_string());
                for (suffix, at) in [("any", quote! { Any }), ("first", quote! { First }), ("last", quote! { Last })] {
                    let arg_ident = format_ident!("{}_{}", fi.ident, suffix);
                    let gql_name = format!("{camel}_{suffix}");
                    filter_args.push(quote! {
                        #[graphql(name = #gql_name)]
                        #arg_ident: ::std::option::Option<<#elem as ::typiql::TypiQLElement>::Filter>,
                    });
                    filter_inserts.push(quote! {
                        if let ::std::option::Option::Some(v) = #arg_ident {
                            filters.push(::typiql::ElementMatch::filter(
                                #field_str,
                                ::typiql::ElementAt::#at,
                                <#elem as ::typiql::TypiQLElement>::element_filters(v),
                            ));
                        }
                    });
                }
                let arg_ident = format_ident!("{}_at_index", fi.ident);
                let gql_name = format!("{camel}_atIndex");
                filter_args.push(quote! {
                    #[graphql(name = #gql_name)]
                    #arg_ident: ::std::option::Option<<#elem as ::typiql::TypiQLElement>::IndexFilter>,
                });
                filter_inserts.push(quote! {
                    if let ::std::option::Option::Some(v) = #arg_ident {
                        let (__i, __f) = <#elem as ::typiql::TypiQLElement>::index_filter(v);
                        filters.push(::typiql::ElementMatch::filter(#field_str, ::typiql::ElementAt::Index(__i), __f));
                    }
                });
                continue;
            }
            for spec in op_specs_for(&fi.ty) {
                let arg_ident = format_ident!("{}{}", fi.ident, spec.suffix);
                let gql_name = format!("{}{}", to_camel_case(&fi.ident.to_string()), csharp_suffix(spec.op));
                let op_variant = format_ident!("{}", spec.op);
                let arg_ty = if spec.is_list {
                    quote! { ::std::option::Option<::std::vec::Vec<#inner_ty>> }
                } else {
                    quote! { ::std::option::Option<#inner_ty> }
                };
                filter_args.push(quote! { #[graphql(name = #gql_name)] #arg_ident: #arg_ty, });
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
        (filter_args, filter_inserts)
    };

    // A declared field's storage name — declarations name fields the Rust
    // (i.e. GraphQL) way; the adapter needs its `data_name`.
    let storage_name_of = |f: &Ident| -> String {
        filterable
            .iter()
            .find(|fi| &fi.ident == f)
            .map(|fi| fi.storage_name())
            .unwrap_or_else(|| f.to_string())
    };

    // `roles(...)` check, run before anything else in a resolver.
    let roles_check = |op: &OpDecl| {
        let roles = &op.roles;
        (!roles.is_empty()).then(|| quote! { ::typiql::check_roles(ctx, &[#(#roles),*])?; })
    };
    // `scope(...)` as `FieldFilter`s pushed onto `vec`.
    let scope_pushes = |op: &OpDecl, vec: &proc_macro2::Ident| -> Vec<TokenStream2> {
        op.scope
            .iter()
            .map(|(f, lit)| {
                let field_str = storage_name_of(f);
                quote! {
                    #vec.push(::typiql::FieldFilter {
                        field: #field_str.to_string(),
                        op: ::typiql::FilterOp::Eq,
                        value: ::typiql::resolve_value(ctx, ::serde_json::json!(#lit))?,
                    });
                }
            })
            .collect()
    };
    // `set(...)` as a `Vec<(&str, Value)>` expression.
    let set_values = |op: &OpDecl| -> TokenStream2 {
        let entries = op.set.iter().map(|(f, lit)| {
            let field_str = storage_name_of(f);
            quote! { (#field_str, ::typiql::resolve_value(ctx, ::serde_json::json!(#lit))?) }
        });
        quote! { ::std::vec![#(#entries),*] }
    };

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

        // Each condition's value, resolved against this parent row: its own
        // field (a missing optional value means no related rows at all), or
        // a literal / variable. `on_missing` is what the resolver returns
        // when a local optional field is None.
        let condition_values = |fi: &FieldInfo, on_missing: TokenStream2| -> Vec<TokenStream2> {
            fi.relations.iter().map(|r| match &r.source {
                ConditionSource::Local(local) => {
                    let local_ident = format_ident!("{}", local);
                    let is_option = field_infos.iter()
                        .find(|o| o.ident == *local)
                        .map(|o| is_option_type(&o.ty))
                        .unwrap_or(false);
                    if is_option {
                        quote! {
                            match &self.#local_ident {
                                ::std::option::Option::Some(__v) => ::serde_json::to_value(__v)?,
                                ::std::option::Option::None => return ::std::result::Result::Ok(#on_missing),
                            }
                        }
                    } else {
                        quote! { ::serde_json::to_value(&self.#local_ident)? }
                    }
                }
                ConditionSource::Value(lit) => quote! {
                    ::typiql::resolve_value(ctx, ::serde_json::json!(#lit))?
                },
            }).collect()
        };

        // A join-mode relation first checks the request's JoinCache — its rows
        // were already joined in if this parent came from a list or `get`.
        let join_cache_check = |fi: &FieldInfo| -> Option<TokenStream2> {
            if is_dumb || fi.data_name.is_none() {
                return None;
            }
            let key_ident = &field_infos[key_idx.unwrap_or(0)].ident;
            let storage = fi.storage_name();
            Some(quote! {
                if let ::std::option::Option::Some(__cache) = ctx.data_opt::<::typiql::JoinCache>() {
                    let __key = ::serde_json::to_value(&self.#key_ident)?.to_string();
                    if let ::std::option::Option::Some(__v) = __cache.get(#name_str, &__key, #storage) {
                        return ::std::result::Result::Ok(::serde_json::from_value(__v)?);
                    }
                }
            })
        };

        // Has-one relations resolve via the per-target-type DataLoader
        // registered by typiql_schema!, batching this field's lookup across
        // every row in the same GraphQL execution instead of one adapter call
        // per row.
        for fi in field_infos.iter().filter(|fi| !fi.relations.is_empty() && !is_vec_type(&fi.ty)) {
            let field_ident = &fi.ident;
            let target_ty = unwrap_box(unwrap_option(&fi.ty));
            let values = condition_values(fi, quote! { ::std::option::Option::None });
            let foreign: Vec<&str> = fi.relations.iter().map(|r| r.foreign.as_str()).collect();
            let ops: Vec<proc_macro2::Ident> = fi.relations.iter().map(|r| format_ident!("{}", r.op)).collect();
            let sort_fields: Vec<&str> = fi.relation_options.sort.iter().map(|(f, _)| f.as_str()).collect();
            let sort_desc: Vec<bool> = fi.relation_options.sort.iter().map(|(_, d)| *d).collect();
            let skip = fi.relation_options.start.unwrap_or(0);
            let cache_check = join_cache_check(fi);

            let __log_on = fi.log;
            let __log_name = ::std::format!("{}.{}", name_str, to_camel_case(&fi.ident.to_string()));
            let __log_kind = "field";
            resolvers.push(quote! {
                async fn #field_ident(&self, ctx: &::async_graphql::Context<'_>) -> ::async_graphql::Result<::std::option::Option<#target_ty>> {
                    let __started = ::std::time::Instant::now();

                    let __result: ::async_graphql::Result<::std::option::Option<#target_ty>> = async {

                    #cache_check
                    let __key = ::typiql::RelationKey {
                        conditions: ::std::vec![#(
                            ::typiql::RelationCondition::new(
                                <#target_ty as ::typiql::TypiQLType>::data_name(#foreign),
                                ::typiql::FilterOp::#ops,
                                &#values,
                            )
                        ),*],
                        sort: ::std::vec![#(
                            (<#target_ty as ::typiql::TypiQLType>::data_name(#sort_fields).to_string(), #sort_desc)
                        ),*],
                        skip: #skip,
                    };
                    let loader = ctx.data::<::async_graphql::dataloader::DataLoader<::typiql::RelationLoader<#target_ty>>>()?;
                    loader.load_one(__key).await
                        .map_err(|e| ::async_graphql::Error::new(e.to_string()))
                
                    }.await;

                    if let (true, ::std::result::Result::Ok(__v)) = (#__log_on, &__result) {

                        ::typiql::log_success(ctx, &#__log_name, #__log_kind, __started, __v);

                    }

                    __result
}
            });
        }

        // Has-many (`Vec<Target>`) relations — e.g. Recording's many
        // RecordingFrame rows — aren't DataLoader-batched (there's no
        // single-related-row cache key to share); they resolve through the
        // same adapter-aware `resolve_query` every list read uses, so they
        // cross adapters (a JSON parent to a DuckDB child) with no
        // relation-specific plumbing, and push their sort/limit into the
        // target's adapter.
        for fi in field_infos.iter().filter(|fi| !fi.relations.is_empty() && is_vec_type(&fi.ty)) {
            let field_ident = &fi.ident;
            let target_ty = vec_inner_type(&fi.ty);
            let values = condition_values(fi, quote! { ::std::vec::Vec::new() });
            let foreign: Vec<&str> = fi.relations.iter().map(|r| r.foreign.as_str()).collect();
            let ops: Vec<proc_macro2::Ident> = fi.relations.iter().map(|r| format_ident!("{}", r.op)).collect();
            let sort_fields: Vec<&str> = fi.relation_options.sort.iter().map(|(f, _)| f.as_str()).collect();
            let sort_desc: Vec<bool> = fi.relation_options.sort.iter().map(|(_, d)| *d).collect();
            let skip = fi.relation_options.start.unwrap_or(0);
            let limit = match fi.relation_options.limit {
                Some(l) => quote! { ::std::option::Option::Some(#l) },
                None => quote! { ::std::option::Option::None },
            };

            let cache_check = join_cache_check(fi);
            let __log_on = fi.log;
            let __log_name = ::std::format!("{}.{}", name_str, to_camel_case(&fi.ident.to_string()));
            let __log_kind = "field";
            resolvers.push(quote! {
                async fn #field_ident(&self, ctx: &::async_graphql::Context<'_>) -> ::async_graphql::Result<::std::vec::Vec<#target_ty>> {
                    let __started = ::std::time::Instant::now();

                    let __result: ::async_graphql::Result<::std::vec::Vec<#target_ty>> = async {

                    #cache_check
                    let __query = ::typiql::ListQuery {
                        filters: ::std::vec![#(
                            ::typiql::FieldFilter {
                                field: <#target_ty as ::typiql::TypiQLType>::data_name(#foreign).to_string(),
                                op: ::typiql::FilterOp::#ops,
                                value: #values,
                            }
                        ),*],
                        sort: ::std::vec![#(
                            ::typiql::SortKey {
                                field: <#target_ty as ::typiql::TypiQLType>::data_name(#sort_fields).to_string(),
                                desc: #sort_desc,
                            }
                        ),*],
                        skip: #skip,
                        limit: #limit,
                        key_field: <#target_ty as ::typiql::TypiQLType>::key_field().to_string(),
                        joins: ::std::vec::Vec::new(),
                    };
                    ::typiql::resolve_query::<#target_ty>(ctx, __query).await
                
                    }.await;

                    if let (true, ::std::result::Result::Ok(__v)) = (#__log_on, &__result) {

                        ::typiql::log_success(ctx, &#__log_name, #__log_kind, __started, __v);

                    }

                    __result
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

            let __log_on = fi.log;
            let __log_name = ::std::format!("{}.{}", name_str, to_camel_case(&fi.ident.to_string()));
            let __log_kind = "field";
            resolvers.push(quote! {
                async fn #field_ident(&self, ctx: &::async_graphql::Context<'_>) -> ::async_graphql::Result<::std::option::Option<#target_ty>> {
                    let __started = ::std::time::Instant::now();

                    let __result: ::async_graphql::Result<::std::option::Option<#target_ty>> = async {

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
                
                    }.await;

                    if let (true, ::std::result::Result::Ok(__v)) = (#__log_on, &__result) {

                        ::typiql::log_success(ctx, &#__log_name, #__log_kind, __started, __v);

                    }

                    __result
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
        let key_field_str = field_infos[key_idx.unwrap_or(0)].storage_name();
        // `data_name()` arms — only fields that are actually renamed.
        let data_name_arms: Vec<TokenStream2> = filterable
            .iter()
            .filter_map(|fi| {
                let dn = fi.data_name.as_ref()?;
                let ident_str = fi.ident.to_string();
                Some(quote! { #ident_str => #dn, })
            })
            .collect();
        // `sort_field()` arms: every plain field, by GraphQL name.
        let sort_field_arms: Vec<TokenStream2> = filterable
            .iter()
            .map(|fi| {
                let gql = to_camel_case(&fi.ident.to_string());
                let storage = fi.storage_name();
                quote! { #gql => ::std::option::Option::Some(#storage.to_string()), }
            })
            .collect();
        // Join-mode relations: relation fields with a `data_name` (C#'s
        // relation columns with a DataName). Their rows are joined into the
        // parent's own reads, so the parent can sort by them —
        // `_orderBy: "owner.name"`.
        let join_relations: Vec<&FieldInfo> = field_infos
            .iter()
            .filter(|fi| !fi.relations.is_empty() && fi.data_name.is_some())
            .collect();
        let sort_path_arms: Vec<TokenStream2> = join_relations
            .iter()
            .map(|fi| {
                let prefix = format!("{}.", to_camel_case(&fi.ident.to_string()));
                let storage = fi.storage_name();
                let target = relation_target(&fi.ty);
                quote! {
                    // One relation deep: joined rows don't carry joins of
                    // their own, so a longer path isn't sortable.
                    __n if __n.starts_with(#prefix) => <#target as ::typiql::TypiQLType>::sort_field(&__n[#prefix.len()..])
                        .filter(|__f| !__f.contains('.'))
                        .map(|__f| ::std::format!("{}.{}", #storage, __f)),
                }
            })
            .collect();
        let join_specs: Vec<TokenStream2> = join_relations
            .iter()
            .map(|fi| {
                let storage = fi.storage_name();
                let graphql_name = to_camel_case(&fi.ident.to_string());
                let target = relation_target(&fi.ty);
                let many = is_vec_type(&fi.ty);
                let conditions = fi.relations.iter().map(|r| {
                    let foreign = &r.foreign;
                    let op = format_ident!("{}", r.op);
                    let value = match &r.source {
                        ConditionSource::Local(local) => {
                            let local_storage = field_infos
                                .iter()
                                .find(|o| o.ident == local.as_str())
                                .map(|o| o.storage_name())
                                .unwrap_or_else(|| local.clone());
                            quote! { ::typiql::JoinValue::Parent(#local_storage.to_string()) }
                        }
                        ConditionSource::Value(lit) => quote! {
                            ::typiql::JoinValue::Literal(::typiql::resolve_value(ctx, ::serde_json::json!(#lit))?)
                        },
                    };
                    quote! {
                        ::typiql::JoinCondition {
                            field: <#target as ::typiql::TypiQLType>::data_name(#foreign).to_string(),
                            op: ::typiql::FilterOp::#op,
                            value: #value,
                        }
                    }
                });
                let sorts = fi.relation_options.sort.iter().map(|(f, desc)| quote! {
                    ::typiql::SortKey { field: <#target as ::typiql::TypiQLType>::data_name(#f).to_string(), desc: #desc }
                });
                let skip = fi.relation_options.start.unwrap_or(0);
                let limit = match fi.relation_options.limit {
                    Some(l) => quote! { ::std::option::Option::Some(#l) },
                    None => quote! { ::std::option::Option::None },
                };
                quote! {
                    ::typiql::Join {
                        name: #storage.to_string(),
                        graphql_name: #graphql_name.to_string(),
                        target: <#target as ::typiql::TypiQLType>::collection_name().to_string(),
                        target_adapter: <#target as ::typiql::TypiQLType>::adapter_name(),
                        target_key: <#target as ::typiql::TypiQLType>::key_field().to_string(),
                        many: #many,
                        conditions: ::std::vec![#(#conditions),*],
                        sort: ::std::vec![#(#sorts),*],
                        skip: #skip,
                        limit: #limit,
                    }
                }
            })
            .collect();
        let joins_fn = (!join_specs.is_empty()).then(|| quote! {
            fn joins(ctx: &::async_graphql::Context<'_>) -> ::async_graphql::Result<::std::vec::Vec<::typiql::Join>> {
                ::std::result::Result::Ok(::std::vec![#(#join_specs),*])
            }
        });
        let data_name_fn = (!data_name_arms.is_empty()).then(|| {
            quote! {
                fn data_name(field: &'static str) -> &'static str {
                    match field {
                        #(#data_name_arms)*
                        other => other,
                    }
                }
            }
        });
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
        let duckdb_columns_fn = quote! {
            fn duckdb_columns() -> &'static [(&'static str, &'static str)] {
                &[ #(#duckdb_column_entries),* ]
            }
        };

        // Each declared operation's resolver, by root. The Query/Mutation/
        // Subscription structs themselves always exist — `typiql_schema!`
        // merges them by name — but one with no operations gets no
        // `#[Object]`/`#[Subscription]` impl, just an empty MergedObject /
        // MergedSubscription that contributes nothing to the schema.
        let mut query_fns: Vec<TokenStream2> = Vec::new();
        let mut mutation_fns: Vec<TokenStream2> = Vec::new();
        let mut subscription_fns: Vec<TokenStream2> = Vec::new();
        let mut seen_names: Vec<String> = Vec::new();
        // (GraphQL name, kind) of every root operation, for log entries.
        let mut op_kinds: Vec<(String, &'static str)> = Vec::new();
        let filters_vec = format_ident!("filters");
        let scope_vec = format_ident!("__scope");

        for op in &ops {
            let fn_ident = match &op.name {
                Some(n) => format_ident!("{}", to_snake_case(&n.value())),
                None => match op.kind {
                    OpKind::Get => get_fn.clone(),
                    OpKind::List => get_list_fn.clone(),
                    OpKind::Add => add_fn.clone(),
                    OpKind::Update => update_fn.clone(),
                    OpKind::Remove => remove_fn.clone(),
                    OpKind::Subscribe => changed_fn.clone(),
                    OpKind::AddMany => format_ident!("add_{}", simple_plural(&name_snake)),
                    OpKind::UpdateMany => format_ident!("update_{}", simple_plural(&name_snake)),
                    OpKind::RemoveMany => format_ident!("remove_{}", simple_plural(&name_snake)),
                },
            };
            let gql_name = op
                .name
                .as_ref()
                .map(|n| n.value())
                .unwrap_or_else(|| to_camel_case(&fn_ident.to_string()));
            if seen_names.contains(&gql_name) {
                let msg = format!(
                    "`{gql_name}` is declared more than once — give each `{}` its own `name = \"…\"`",
                    op.kind.keyword()
                );
                return match &op.name {
                    Some(n) => syn::Error::new_spanned(n, msg),
                    None => syn::Error::new_spanned(name, msg),
                }
                .to_compile_error();
            }
            seen_names.push(gql_name.clone());
            let name_attr = op.name.as_ref().map(|n| quote! { #[graphql(name = #n)] });
            let roles = roles_check(op);
            // Successful calls are logged when the operation (or its whole
            // type) opts in; errors are always logged, per request.
            if op.kind != OpKind::Subscribe {
                op_kinds.push((gql_name.clone(), op.kind.keyword()));
                if op.kind == OpKind::List {
                    op_kinds.push((format!("{gql_name}PageInfo"), op.kind.keyword()));
                }
            }
            let __log_on = op.log || log_all;
            let __log_kind = op.kind.keyword();
            let __log_name = gql_name.clone();

            match op.kind {
                OpKind::Get | OpKind::List => {
                    let (filter_args, filter_inserts) = filter_code(op);
                    let scope = scope_pushes(op, &filters_vec);
                    // C#'s sorting/paging arguments, on `get` and every list.
                    let paging_args = quote! {
                        #[graphql(name = "_orderBy")] __order_by: ::std::option::Option<::std::string::String>,
                        #[graphql(name = "_orderBy_desc")] __order_by_desc: ::std::option::Option<::std::string::String>,
                        #[graphql(name = "_start")] __start: ::std::option::Option<i32>,
                        #[graphql(name = "_limit")] __limit: ::std::option::Option<i32>,
                    };
                    let build_query = quote! {
                        #[allow(unused_mut)]
                        let mut filters: ::std::vec::Vec<::typiql::FieldFilter> = ::std::vec::Vec::new();
                        #(#scope)*
                        #(#filter_inserts)*
                        let mut __query = ::typiql::list_query(
                            filters, __order_by, __order_by_desc, __start, __limit,
                            <#name as ::typiql::TypiQLType>::key_field(),
                            <#name as ::typiql::TypiQLType>::sort_field,
                        )?;
                    };
                    let (ret, finish) = if op.kind == OpKind::Get {
                        (
                            quote! { ::std::option::Option<#name> },
                            // The first match after sorting and `_start`
                            // wins; no match returns null.
                            quote! {
                                __query.limit = ::std::option::Option::Some(1);
                                ::std::result::Result::Ok(
                                    ::typiql::resolve_query::<#name>(ctx, __query).await?.into_iter().next()
                                )
                            },
                        )
                    } else {
                        (
                            quote! { ::std::vec::Vec<#name> },
                            quote! { ::typiql::resolve_query::<#name>(ctx, __query).await },
                        )
                    };
                    // `get` is filter-based with the same argument set as
                    // `list` (including the key field) rather than an
                    // exact-key-only lookup.
                    query_fns.push(quote! {
                        #name_attr
                        #[allow(clippy::too_many_arguments)]
                        async fn #fn_ident(
                            &self,
                            ctx: &::async_graphql::Context<'_>,
                            #(#filter_args)*
                            #paging_args
                        ) -> ::async_graphql::Result<#ret> {
                            let __started = ::std::time::Instant::now();

                            let __result: ::async_graphql::Result<#ret> = async {

                            #roles
                            #build_query
                            #finish
                        
                            }.await;

                            if let (true, ::std::result::Result::Ok(__v)) = (#__log_on, &__result) {

                                ::typiql::log_success(ctx, &#__log_name, #__log_kind, __started, __v);

                            }

                            __result
}
                    });

                    // C#'s `<list>PageInfo`: the list's own arguments, roles
                    // and scope, answering how many rows match and which page
                    // `_start` is on.
                    if op.kind == OpKind::List {
                        let page_info_name = format!("{gql_name}PageInfo");
                        if seen_names.contains(&page_info_name) {
                            return syn::Error::new_spanned(
                                name,
                                format!("`{page_info_name}` is declared more than once"),
                            )
                            .to_compile_error();
                        }
                        seen_names.push(page_info_name.clone());
                        let page_info_fn = format_ident!("{}_page_info", fn_ident);
                        let roles = roles_check(op);
                        let __log_name = page_info_name.clone();
                        query_fns.push(quote! {
                            #[graphql(name = #page_info_name)]
                            #[allow(clippy::too_many_arguments)]
                            async fn #page_info_fn(
                                &self,
                                ctx: &::async_graphql::Context<'_>,
                                #(#filter_args)*
                                #paging_args
                            ) -> ::async_graphql::Result<::typiql::PageInfo> {
                                let __started = ::std::time::Instant::now();

                                let __result: ::async_graphql::Result<::typiql::PageInfo> = async {

                                #roles
                                #build_query
                                ::typiql::resolve_page_info::<#name>(ctx, __query).await
                            
                                }.await;

                                if let (true, ::std::result::Result::Ok(__v)) = (#__log_on, &__result) {

                                    ::typiql::log_success(ctx, &#__log_name, #__log_kind, __started, __v);

                                }

                                __result
}
                        });
                    }
                }
                OpKind::Add => {
                    let set = set_values(op);
                    mutation_fns.push(quote! {
                        #name_attr
                        async fn #fn_ident(
                            &self,
                            ctx: &::async_graphql::Context<'_>,
                            values: #input_name,
                        ) -> ::async_graphql::Result<#name> {
                            let __started = ::std::time::Instant::now();

                            let __result: ::async_graphql::Result<#name> = async {

                            #roles
                            let __set = #set;
                            let __item = ::typiql::resolve_add_with::<#name>(ctx, values, __set).await?;
                            ::typiql::TypiQLBroker::publish(#changed_struct {
                                operation_name: "add".to_string(),
                                value: __item.clone(),
                            });
                            ::std::result::Result::Ok(__item)
                        
                            }.await;

                            if let (true, ::std::result::Result::Ok(__v)) = (#__log_on, &__result) {

                                ::typiql::log_success(ctx, &#__log_name, #__log_kind, __started, __v);

                            }

                            __result
}
                    });
                }
                OpKind::Update => {
                    let scope = scope_pushes(op, &scope_vec);
                    let set = set_values(op);
                    // The row is found by the key, or by the declared
                    // `by(...)` fields (all equality).
                    let (identify_args, identify_pushes, by_key) = match &op.by {
                        None => (
                            quote! { #key_field_ident: ::std::string::String, },
                            quote! {
                                __identify.push(::typiql::FieldFilter {
                                    field: #key_field_str.to_string(),
                                    op: ::typiql::FilterOp::Eq,
                                    value: ::serde_json::Value::String(#key_field_ident.clone()),
                                });
                            },
                            true,
                        ),
                        Some(fields) => {
                            let infos: Vec<&&FieldInfo> = fields
                                .iter()
                                .filter_map(|f| filterable.iter().find(|fi| &fi.ident == f))
                                .collect();
                            let args = infos.iter().map(|fi| {
                                let ident = &fi.ident;
                                let gql = to_camel_case(&ident.to_string());
                                let ty = unwrap_option(&fi.ty);
                                quote! { #[graphql(name = #gql)] #ident: #ty, }
                            });
                            let pushes = infos.iter().map(|fi| {
                                let ident = &fi.ident;
                                let storage = fi.storage_name();
                                quote! {
                                    __identify.push(::typiql::FieldFilter {
                                        field: #storage.to_string(),
                                        op: ::typiql::FilterOp::Eq,
                                        value: ::serde_json::to_value(&#ident)?,
                                    });
                                }
                            });
                            (quote! { #(#args)* }, quote! { #(#pushes)* }, false)
                        }
                    };
                    let (upsert_arg, upsert_value) = if op.upsert {
                        (quote! {}, quote! { true })
                    } else {
                        (
                            quote! { #[graphql(name = "_upsert")] __upsert: ::std::option::Option<bool>, },
                            quote! { __upsert.unwrap_or(false) },
                        )
                    };
                    // A plain keyed update keeps the direct path; finding by
                    // other fields, or upserting, goes through resolve_upsert.
                    let call = if by_key {
                        quote! {
                            if __upsert {
                                #[allow(unused_mut)]
                                let mut __identify: ::std::vec::Vec<::typiql::FieldFilter> = ::std::vec::Vec::new();
                                #identify_pushes
                                ::typiql::resolve_upsert::<#name>(ctx, __identify, update, __scope, __set, true).await?
                            } else {
                                ::typiql::resolve_update_with::<#name>(ctx, #key_field_ident, update, __scope, __set).await?
                            }
                        }
                    } else {
                        quote! {
                            {
                                #[allow(unused_mut)]
                                let mut __identify: ::std::vec::Vec<::typiql::FieldFilter> = ::std::vec::Vec::new();
                                #identify_pushes
                                ::typiql::resolve_upsert::<#name>(ctx, __identify, update, __scope, __set, __upsert).await?
                            }
                        }
                    };
                    mutation_fns.push(quote! {
                        #name_attr
                        #[allow(clippy::too_many_arguments)]
                        async fn #fn_ident(
                            &self,
                            ctx: &::async_graphql::Context<'_>,
                            #identify_args
                            update: #input_name,
                            #upsert_arg
                        ) -> ::async_graphql::Result<::std::option::Option<#name>> {
                            let __started = ::std::time::Instant::now();

                            let __result: ::async_graphql::Result<::std::option::Option<#name>> = async {

                            #roles
                            #[allow(unused_mut)]
                            let mut __scope: ::std::vec::Vec<::typiql::FieldFilter> = ::std::vec::Vec::new();
                            #(#scope)*
                            let __set = #set;
                            let __upsert: bool = #upsert_value;
                            let __item = #call;
                            if let ::std::option::Option::Some(ref __v) = __item {
                                ::typiql::TypiQLBroker::publish(#changed_struct {
                                    operation_name: "update".to_string(),
                                    value: __v.clone(),
                                });
                            }
                            ::std::result::Result::Ok(__item)
                        
                            }.await;

                            if let (true, ::std::result::Result::Ok(__v)) = (#__log_on, &__result) {

                                ::typiql::log_success(ctx, &#__log_name, #__log_kind, __started, __v);

                            }

                            __result
}
                    });
                }
                OpKind::Remove => {
                    let scope = scope_pushes(op, &scope_vec);
                    mutation_fns.push(quote! {
                        #name_attr
                        async fn #fn_ident(
                            &self,
                            ctx: &::async_graphql::Context<'_>,
                            #key_field_ident: ::std::string::String,
                        ) -> ::async_graphql::Result<::std::option::Option<#name>> {
                            let __started = ::std::time::Instant::now();

                            let __result: ::async_graphql::Result<::std::option::Option<#name>> = async {

                            #roles
                            #[allow(unused_mut)]
                            let mut __scope: ::std::vec::Vec<::typiql::FieldFilter> = ::std::vec::Vec::new();
                            #(#scope)*
                            let __item = ::typiql::resolve_remove_with::<#name>(ctx, #key_field_ident, __scope).await?;
                            if let ::std::option::Option::Some(ref __v) = __item {
                                ::typiql::TypiQLBroker::publish(#changed_struct {
                                    operation_name: "remove".to_string(),
                                    value: __v.clone(),
                                });
                            }
                            ::std::result::Result::Ok(__item)
                        
                            }.await;

                            if let (true, ::std::result::Result::Ok(__v)) = (#__log_on, &__result) {

                                ::typiql::log_success(ctx, &#__log_name, #__log_kind, __started, __v);

                            }

                            __result
}
                    });
                }
                OpKind::AddMany => {
                    let set = set_values(op);
                    mutation_fns.push(quote! {
                        #name_attr
                        async fn #fn_ident(
                            &self,
                            ctx: &::async_graphql::Context<'_>,
                            items: ::std::vec::Vec<#input_name>,
                        ) -> ::async_graphql::Result<::std::vec::Vec<#name>> {
                            let __started = ::std::time::Instant::now();

                            let __result: ::async_graphql::Result<::std::vec::Vec<#name>> = async {

                            #roles
                            let __set = #set;
                            let __items = ::typiql::resolve_add_many_inputs::<#name>(ctx, items, __set).await?;
                            for __v in &__items {
                                ::typiql::TypiQLBroker::publish(#changed_struct {
                                    operation_name: "add".to_string(),
                                    value: __v.clone(),
                                });
                            }
                            ::std::result::Result::Ok(__items)
                        
                            }.await;

                            if let (true, ::std::result::Result::Ok(__v)) = (#__log_on, &__result) {

                                ::typiql::log_success(ctx, &#__log_name, #__log_kind, __started, __v);

                            }

                            __result
}
                    });
                }
                OpKind::UpdateMany | OpKind::RemoveMany => {
                    let (filter_args, filter_inserts) = filter_code(op);
                    let scope = scope_pushes(op, &filters_vec);
                    let (update_arg, call, event) = if op.kind == OpKind::UpdateMany {
                        let set = set_values(op);
                        (
                            quote! { update: #input_name, },
                            quote! { ::typiql::resolve_update_many::<#name>(ctx, #gql_name, filters, update, #set).await? },
                            "update",
                        )
                    } else {
                        (
                            quote! {},
                            quote! { ::typiql::resolve_remove_many::<#name>(ctx, #gql_name, filters).await? },
                            "remove",
                        )
                    };
                    mutation_fns.push(quote! {
                        #name_attr
                        #[allow(clippy::too_many_arguments)]
                        async fn #fn_ident(
                            &self,
                            ctx: &::async_graphql::Context<'_>,
                            #(#filter_args)*
                            #update_arg
                        ) -> ::async_graphql::Result<::std::vec::Vec<#name>> {
                            let __started = ::std::time::Instant::now();

                            let __result: ::async_graphql::Result<::std::vec::Vec<#name>> = async {

                            #roles
                            #[allow(unused_mut)]
                            let mut filters: ::std::vec::Vec<::typiql::FieldFilter> = ::std::vec::Vec::new();
                            #(#scope)*
                            #(#filter_inserts)*
                            let __items = #call;
                            for __v in &__items {
                                ::typiql::TypiQLBroker::publish(#changed_struct {
                                    operation_name: #event.to_string(),
                                    value: __v.clone(),
                                });
                            }
                            ::std::result::Result::Ok(__items)
                        
                            }.await;

                            if let (true, ::std::result::Result::Ok(__v)) = (#__log_on, &__result) {

                                ::typiql::log_success(ctx, &#__log_name, #__log_kind, __started, __v);

                            }

                            __result
}
                    });
                }
                OpKind::Subscribe => {
                    let scope = scope_pushes(op, &scope_vec);
                    subscription_fns.push(quote! {
                        /// Subscribe to all changes for this type, or filter to a single record
                        /// by passing its `id`. Matches the `subscribe` / `subscribeToOne`
                        /// dispatcher slots used by ReactiveAdmin.
                        #name_attr
                        async fn #fn_ident(
                            &self,
                            ctx: &::async_graphql::Context<'_>,
                            id: ::std::option::Option<::std::string::String>,
                        ) -> ::async_graphql::Result<impl ::async_graphql::futures_util::Stream<Item = #changed_struct>> {
                            use ::async_graphql::futures_util::StreamExt as _;
                            #roles
                            // Resolved once, at subscribe time: events for
                            // records outside the scope are never sent.
                            #[allow(unused_mut)]
                            let mut __scope: ::std::vec::Vec<::typiql::FieldFilter> = ::std::vec::Vec::new();
                            #(#scope)*
                            ::std::result::Result::Ok(::typiql::TypiQLBroker::<#changed_struct>::subscribe()
                                .filter(move |__event| {
                                    let __matches = id.as_ref()
                                        .map(|__id| &__event.value.#key_field_ident == __id)
                                        .unwrap_or(true)
                                        && ::typiql::matches_scope(&__event.value, &__scope);
                                    ::std::future::ready(__matches)
                                }))
                        }
                    });
                }
            }
        }

        let op_kind_names: Vec<&String> = op_kinds.iter().map(|(n, _)| n).collect();
        let op_kind_kinds: Vec<&str> = op_kinds.iter().map(|(_, k)| *k).collect();
        let log_store_fn = is_log_store.then(|| quote! { fn is_log_store() -> bool { true } });
        let query_block = if !query_fns.is_empty() {
            quote! {
                #[derive(::std::default::Default)]
                #vis struct #query_struct;

                #[::async_graphql::Object]
                impl #query_struct {
                    #(#query_fns)*
                }
            }
        } else {
            quote! {
                #[derive(::async_graphql::MergedObject, ::std::default::Default)]
                #vis struct #query_struct();
            }
        };

        let mutation_block = if !mutation_fns.is_empty() {
            quote! {
                #[derive(::std::default::Default)]
                #vis struct #mutation_struct;

                #[::async_graphql::Object]
                impl #mutation_struct {
                    #(#mutation_fns)*
                }
            }
        } else {
            quote! {
                #[derive(::async_graphql::MergedObject, ::std::default::Default)]
                #vis struct #mutation_struct();
            }
        };

        let subscription_block = if !subscription_fns.is_empty() {
            quote! {
                #[derive(::std::default::Default)]
                #vis struct #subscription_struct;

                #[::async_graphql::Subscription]
                impl #subscription_struct {
                    #(#subscription_fns)*
                }
            }
        } else {
            quote! {
                #[derive(::async_graphql::MergedSubscription, ::std::default::Default)]
                #vis struct #subscription_struct();
            }
        };

        quote! {
            impl ::typiql::TypiQLType for #name {
                type Input = #input_name;
                fn type_name()       -> &'static str { #name_str }
                fn key_field()       -> &'static str { #key_field_str }
                #data_name_fn
                #joins_fn
                fn operations() -> &'static [(&'static str, &'static str)] {
                    &[#((#op_kind_names, #op_kind_kinds)),*]
                }
                #log_store_fn
                fn sort_field(graphql_name: &str) -> ::std::option::Option<::std::string::String> {
                    match graphql_name {
                        #(#sort_field_arms)*
                        #(#sort_path_arms)*
                        _ => ::std::option::Option::None,
                    }
                }
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

            #query_block

            // ── CRUD mutations — publish change events on success ────────────

            #mutation_block

            // ── Real-time subscription ───────────────────────────────────────

            #subscription_block
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

        /// Every generated root operation's GraphQL name and kind — for
        /// `typiql::enable_logging` with a custom logger.
        pub fn typiql_operation_kinds() -> ::std::collections::HashMap<&'static str, &'static str> {
            let mut kinds = ::std::collections::HashMap::new();
            #(
                for (name, kind) in <#types as ::typiql::TypiQLType>::operations() {
                    kinds.insert(*name, *kind);
                }
            )*
            kinds
        }

        /// The schema, built.
        pub fn build_typiql_schema(
            adapters: ::typiql::AdapterMap,
        ) -> ::async_graphql::Schema<TypiQLQuery, TypiQLMutation, TypiQLSubscription> {
            typiql_schema_builder(adapters).finish()
        }

        /// The schema builder, ready but not finished — for adding an app's
        /// own data or extensions (or a custom logger, via
        /// `typiql::enable_logging`) before `.finish()`. Logging is already
        /// on if the schema includes a `#[typiql_log]` type.
        pub fn typiql_schema_builder(
            adapters: ::typiql::AdapterMap,
        ) -> ::async_graphql::SchemaBuilder<TypiQLQuery, TypiQLMutation, TypiQLSubscription> {
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
                    .register_type(
                        <#types as ::typiql::TypiQLType>::collection_name(),
                        <#types as ::typiql::TypiQLType>::key_field(),
                        <#types as ::typiql::TypiQLType>::duckdb_columns(),
                    );
            )*

            #[allow(unused_mut)]
            let mut builder = ::async_graphql::Schema::build(
                TypiQLQuery::default(),
                TypiQLMutation::default(),
                TypiQLSubscription::default(),
            )
            .data(adapters.clone())
            .extension(::typiql::JoinCacheExtension)
            // Logs every error once a logger is set (below, or by the app
            // through `typiql::enable_logging`); idle until then.
            .extension(::typiql::ErrorLogExtension)
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
            )*;

            // Including a log type turns logging on, into that type's rows.
            let mut __logging = false;
            #(
                if !__logging && <#types as ::typiql::TypiQLType>::is_log_store() {
                    let adapter = adapters.get(<#types as ::typiql::TypiQLType>::adapter_name())
                        .expect("log type's adapter is registered (checked above)")
                        .clone();
                    builder = ::typiql::enable_logging(
                        builder,
                        ::std::sync::Arc::new(::typiql::StoreLogger::new(
                            adapter,
                            <#types as ::typiql::TypiQLType>::collection_name(),
                            <#types as ::typiql::TypiQLType>::key_field(),
                        )),
                        typiql_operation_kinds(),
                    );
                    __logging = true;
                }
            )*
            let _ = __logging;
            builder
        }
    };

    expanded.into()
}

// ── #[derive(TypiQLElement)] ───────────────────────────────────────────────

/// Makes a struct filterable as the element of a parent's list field
/// (`#[typiql(elements)] pub interactions: Vec<Interaction>`). Generates
/// `<Name>Filter` — one optional argument per operator per field, with C#'s
/// names (`endPoint`, `endPoint_not`, `endPoint_startsWith`, `at_gte`, ...),
/// all of which must hold for the same element — and `<Name>IndexFilter`
/// (`{ index, filter }`; a negative index counts from the end). Nested lists
/// and fields marked `#[typiql(no_filter)]` aren't filterable. Field names
/// follow `#[serde(rename = "…")]` where given.
#[proc_macro_derive(TypiQLElement, attributes(typiql))]
pub fn derive_typiql_element(item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as DeriveInput);
    let name = &input.ident;
    let vis = &input.vis;
    let Data::Struct(data) = &input.data else {
        return syn::Error::new_spanned(name, "TypiQLElement needs a struct").to_compile_error().into();
    };
    let Fields::Named(fields) = &data.fields else {
        return syn::Error::new_spanned(name, "TypiQLElement needs named fields").to_compile_error().into();
    };
    let filter_name = format_ident!("{}Filter", name);
    let index_name = format_ident!("{}IndexFilter", name);
    let filter_str = filter_name.to_string();
    let index_str = index_name.to_string();

    let mut filter_fields = Vec::new();
    let mut pushes = Vec::new();
    for f in &fields.named {
        let ident = f.ident.as_ref().unwrap();
        let mut skip = false;
        let mut storage = ident.to_string();
        for attr in &f.attrs {
            if attr.path().is_ident("typiql") {
                let _ = attr.parse_nested_meta(|m| {
                    if m.path.is_ident("no_filter") {
                        skip = true;
                    }
                    Ok(())
                });
            } else if attr.path().is_ident("serde") {
                let _ = attr.parse_nested_meta(|m| {
                    if m.path.is_ident("rename") {
                        if let Ok(v) = m.value() {
                            if let Ok(s) = v.parse::<LitStr>() {
                                storage = s.value();
                            }
                        }
                    }
                    Ok(())
                });
            }
        }
        if skip || is_vec_type(unwrap_option(&f.ty)) {
            continue;
        }
        let inner_ty = unwrap_option(&f.ty);
        let camel = to_camel_case(&ident.to_string());
        for spec in op_specs_for(&f.ty) {
            let field_ident = format_ident!("{}{}", ident, spec.suffix);
            let gql = format!("{}{}", camel, csharp_suffix(spec.op));
            let op = format_ident!("{}", spec.op);
            let ty = if spec.is_list {
                quote! { ::std::option::Option<::std::vec::Vec<#inner_ty>> }
            } else {
                quote! { ::std::option::Option<#inner_ty> }
            };
            filter_fields.push(quote! {
                #[graphql(name = #gql)]
                pub #field_ident: #ty,
            });
            pushes.push(quote! {
                if let ::std::option::Option::Some(v) = filter.#field_ident {
                    out.push(::typiql::FieldFilter {
                        field: #storage.to_string(),
                        op: ::typiql::FilterOp::#op,
                        value: ::serde_json::to_value(v).unwrap_or_default(),
                    });
                }
            });
        }
    }

    quote! {
        #[derive(::async_graphql::InputObject, ::std::default::Default, ::std::fmt::Debug)]
        #[graphql(name = #filter_str)]
        #vis struct #filter_name {
            #(#filter_fields)*
        }

        #[derive(::async_graphql::InputObject, ::std::fmt::Debug)]
        #[graphql(name = #index_str)]
        #vis struct #index_name {
            /// Position in the list; negative counts from the end.
            pub index: i32,
            pub filter: #filter_name,
        }

        impl ::typiql::TypiQLElement for #name {
            type Filter = #filter_name;
            type IndexFilter = #index_name;
            fn element_filters(filter: #filter_name) -> ::std::vec::Vec<::typiql::FieldFilter> {
                let mut out = ::std::vec::Vec::new();
                #(#pushes)*
                out
            }
            fn index_filter(filter: #index_name) -> (i64, ::std::vec::Vec<::typiql::FieldFilter>) {
                (filter.index as i64, Self::element_filters(filter.filter))
            }
        }
    }
    .into()
}

// ── #[typiql_log] ──────────────────────────────────────────────────────────

/// Declares an app's log type — including it in `typiql_schema!` turns
/// logging on, stored as rows of this type through the adapter it names:
///
/// ```rust,ignore
/// #[typiql_log(adapter = "logs", ops(list(roles("Admin")), get(roles("Admin"))))]
/// pub struct Log;
/// ```
///
/// typiql supplies the fields (`id`, `at`, `user`, `operation`, `kind`,
/// `arguments`, `result`, `error`, `path`, `durationMs` — see
/// `typiql::LogEntry`); the app picks the name, the adapter (`"default"` if
/// omitted) and the operations clients get — `get` and `list` by default,
/// with the usual filters, sorting and paging. What gets logged: every
/// error, plus successful operations and relation fields that opt in with
/// `log` (see `typiql::log`).
#[proc_macro_attribute]
pub fn typiql_log(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut adapter: Option<LitStr> = None;
    let mut ops: Option<TokenStream2> = None;
    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("adapter") {
            adapter = Some(meta.value()?.parse()?);
            Ok(())
        } else if meta.path.is_ident("ops") {
            let content;
            syn::parenthesized!(content in meta.input);
            ops = Some(content.parse()?);
            Ok(())
        } else {
            Err(meta.error("expected `adapter = \"…\"` or `ops(...)`"))
        }
    });
    parse_macro_input!(attr with parser);
    let input = parse_macro_input!(item as syn::ItemStruct);
    if !matches!(input.fields, Fields::Unit) {
        return syn::Error::new_spanned(&input.ident, "declare the log type as a unit struct (`pub struct Log;`) — typiql supplies its fields")
            .to_compile_error()
            .into();
    }
    let adapter = adapter.map(|a| a.value()).unwrap_or_else(|| "default".to_string());
    let ops = ops.unwrap_or_else(|| quote! { get, list });
    let name = &input.ident;
    let vis = &input.vis;
    let attrs = &input.attrs;
    quote! {
        #[::typiql::typiql_type(adapter = #adapter, ops(#ops), __log_store)]
        #(#attrs)*
        #vis struct #name {
            #[typiql(key)]
            pub id: ::std::string::String,
            /// When, as RFC 3339 (UTC).
            pub at: ::std::string::String,
            /// The request's user, if any.
            pub user: ::std::option::Option<::std::string::String>,
            /// The GraphQL field: `getCars`, `addCar`, `Car.owner`, ...
            pub operation: ::std::string::String,
            /// `get`, `list`, `add`, `update`, `remove`, ..., `field`, or `custom`.
            pub kind: ::std::string::String,
            /// The arguments, as JSON text.
            pub arguments: ::std::string::String,
            /// The result, as JSON text (successes).
            pub result: ::std::option::Option<::std::string::String>,
            /// The error message (failures).
            pub error: ::std::option::Option<::std::string::String>,
            /// Where in the response an error occurred.
            pub path: ::std::option::Option<::std::string::String>,
            pub duration_ms: ::std::option::Option<f64>,
        }
    }
    .into()
}
