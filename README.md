# typiql-rs

A small Rust ORM that turns plain structs into a full GraphQL CRUD API —
queries, mutations, and live-update subscriptions — generated at compile
time, with no schema or migration files to write by hand.

Extracted from [typiql-tauri](https://github.com/MrDavid5465/typiql-tauri),
where it's the backend data layer.

## Using typiql with an AI coding agent

If you're an AI coding agent building on this repo, read
[`.claude/skills/typiql-api/SKILL.md`](./.claude/skills/typiql-api/SKILL.md)
before you write any code. It covers:

- wiring a new axum app
- the exact GraphQL names the macros generate
- relations
- when a hand-written resolver is justified, and how to write one

Skills in a cloned repo are only loaded automatically when the agent was
started inside that repo, so open the file yourself.

The frontend side is three companion libraries. Each has its own skills
under `.claude/skills/`:

- **typical-admin**: the base CRUD admin pattern. It builds list, show, edit
  and new screens for a typiql type from its generated queries, mutations and
  subscription.
- **typical-admin-fabric**: the Fluent UI implementation of typical-admin.
- **per-form**: schema-driven forms.

They're cloned side by side into one folder:

```
workspace/
  typiql-rs/
  typical-admin/
  typical-admin-fabric/
  per-form/
```

For a non-React frontend, the web-component alternatives are [typical-admin-wc](file:///var/mnt/Media/david-source/repos/typiql-standins/typical-admin-wc) (CRUD pattern, forms, data layer) and [denim-wc](file:///var/mnt/Media/david-source/repos/typiql-standins/denim-wc) (app shell), Lit 3 + Web Awesome 3, cloned side by side the same way; their skills are in each repo's `.claude/skills/`.

## Crates

- **typiql-core** — the runtime: adapter trait (`TypiQLAdapter`), the
  `Location` addressing scheme (single shared file vs. per-record files),
  filtering (`FieldFilter`/`FilterOp`), and batched relation loading via
  `DataLoader`.
- **typiql-macros** — the `#[typiql_type]` and `typiql_schema!` proc macros.
  Annotating a struct generates its GraphQL object type, a typed filter
  argument per field, `get{Type}`/`get{Types}`/`add{Type}`/`update{Type}`/
  `remove{Type}` resolvers, a `{type}Changed` subscription, and (via `#[typiql(relation(...))]`)
  batched-relation fields resolved through an auto-registered `DataLoader`.
- **typiql-adapter-json** — a `TypiQLAdapter` implementation backed by a
  single JSON document (or per-record files via `Location::At`, for content
  like uploaded sprites that shouldn't all live in one growing file).

- **typiql-adapter-duckdb** — a `TypiQLAdapter` backed by DuckDB, for
  columnar/analytical data.
- **typiql-adapter-mssql** — a `TypiQLAdapter` backed by SQL Server (the
  port of the original C# typiql's SQL adapter). As there, a `data_name`
  can be a column or a SQL expression; tables are created if missing.
- **typiql-adapter-mongo** — a `TypiQLAdapter` backed by MongoDB (the port
  of the original C# typiql's Mongo adapter, whose semantics are the
  reference for every adapter). Reads are aggregation pipelines, join-mode
  relations are `$lookup`s. As in C#, a key stored as `_id` is an
  `ObjectId`, read and matched as its hex string; any other key gets a
  unique index. GridFS file types aren't ported.
- **typiql-adapter-ldap** — a `TypiQLAdapter` backed by an LDAP directory,
  Active Directory included (the port of the original C# typiql's AD
  adapter). See [Directory types](#directory-types-ldap--active-directory).
- **typiql-conformance** (unpublished) — runs one GraphQL-only test suite
  against every adapter, alone and mixed. Which adapter serves a type is
  backend configuration; clients must not be able to tell the difference.
  Where adapters could disagree (e.g. filters over null values), the
  original C# Mongo adapter's semantics are the reference. Adapters that need a
  server run when it's configured, e.g.
  `TYPIQL_TEST_MSSQL="server=tcp:127.0.0.1,1433;user=sa;password=…;TrustServerCertificate=true"`
  or `TYPIQL_TEST_MONGO="mongodb://127.0.0.1:27017"`, or
  `TYPIQL_TEST_LDAP="ldap://127.0.0.1:389|<base DN>|<bind DN>|<password>"`.

## Usage

### Dependencies

The macros expand to `::typiql::…`, `::async_graphql::…`, `::tokio::…`,
`::serde::…` and `::serde_json::…` paths. So the core crate must be renamed to
`typiql`, and the consuming crate needs the other four crates as direct
dependencies:

```toml
typiql = { package = "typiql-core", git = "file:///var/mnt/Media/david-source/repos/typiql-standins/typiql-rs" }
typiql-adapter-json = { git = "file:///var/mnt/Media/david-source/repos/typiql-standins/typiql-rs" }
async-graphql = "7"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "net"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
axum = "0.8"
```

A schema with many types (from roughly 25–40) also needs
`#![recursion_limit = "256"]` in the crate root.

### Types and schema

```rust
#[typiql_type]
pub struct Car {
    #[typiql(key)]
    pub id: String,
    pub name: String,
    #[typiql(relation(local = "day_photo_path", op = "eq", foreign = "path"))]
    pub day_photo: Option<File>,
    pub day_photo_path: Option<String>,
}

typiql_schema!(
    Car, File;
    MyQueryRoot;
    MyMutationRoot;
    MySubscriptionRoot
);
```

This generates `getCar`, `getCars` (with per-field filter arguments),
`getCarsPageInfo`, `addCar`, `updateCar`, `removeCar`, a `carChanged`
subscription, and a batched `day_photo` relation field — all wired into the
schema the macro call produces. `typiql_schema!` must see each type's
generated `{Type}Query`/`{Type}Mutation`/`{Type}Subscription`, so call it
where the types are in scope (same module or a glob import).

Mutations take `addCar(values: CarInput!)`, `updateCar(id: String!,
update: CarInput!)` and `removeCar(id: String!)`. Input fields are camelCase
over GraphQL. In Rust they're snake_case `MaybeUndefined<T>`:

- an omitted field is left untouched
- `null` clears the field
- a value sets it

List names come from a simple pluralizer: `Entry` gives `getEntries`, but
`Recurring` gives `getRecurrings` and `Batch` gives `getBatchs`. Check the
playground when you're unsure. Subscriptions are served as HTTP multipart
(`Accept: multipart/mixed; boundary="graphql"; subscriptionSpec="1.0"`).
There's no WebSocket endpoint.

To listen to many types on one connection, you don't need server code. Put
several generated `xChanged` fields in one subscription operation, each under
its own alias:

```graphql
subscription { a: carChanged { operationName } b: fileChanged { operationName } }
```

Each event arrives keyed by its alias, and each field keeps its own
arguments and auth checks. The typical-admin frontend library merges mounted
components' subscriptions this way automatically.

### Serving it

```rust
let mut adapters: typiql::AdapterMap = HashMap::new();
adapters.insert("default", Arc::new(JsonAdapter::new("data")) as Arc<dyn TypiQLAdapter>); // data/data.json
let schema = build_typiql_schema(adapters);       // generated by typiql_schema!

let app = axum::Router::new()
    .nest("/typiql", typiql::typiql_router(schema, "/typiql/graphql"));
```

`typiql_router(schema, path)` is shorthand for
`TypiQLRouter::new(schema, path).build()`. Use the builder directly only to
attach a user type (`.with_user::<MyUser>()`; see [Optional auth](#optional-auth)).
It serves `POST /graphql` and `GET /playground` under the prefix you nest it at.
It adds no CORS layer, so add your own if the frontend is on another origin.

### Filtering, sorting and paging

Argument names match the original C# typiql, so its clients (Q, the admin
lists) work unchanged:

```graphql
{
  getCars(name_startsWith: "mx", year_gte: 1990, make_in: ["Mazda", "Honda"],
          _orderBy: "make", _orderBy_desc: "year", _start: 20, _limit: 10) { id name }
  getCarsPageInfo(name_startsWith: "mx", _start: 20, _limit: 10) { totalResults page totalPages }
}
```

- Filters: `field` (equals), `field_not`, `_in`, `_notIn`, `_gt`, `_gte`,
  `_lt`, `_lte`, and for strings `_startsWith`, `_endsWith`, `_contains`
  and their `_not…` forms (case-insensitive).
- `_orderBy` / `_orderBy_desc` take comma-separated field names; ascending
  keys apply first. `_start` skips rows, `_limit` caps them (`0` = none).
  `get` returns the first row after sorting and `_start`.
- `<list>PageInfo` takes the list's arguments: `totalResults` ignores
  paging, `page` is `_start / _limit + 1`, `totalPages` is at least 1.

Semantics follow the original's Mongo adapter on every adapter: a missing
value matches `_not`/`_notIn` but no other operator, sorts first ascending
and last descending; strings sort by bytes; ties break by key.

### Filtering inside lists

Lists of values get `field_anyEq` / `field_anyNe` (contains any / none of
the given values). A list of objects opts in, with its element type deriving
the filter:

```rust
#[derive(SimpleObject, InputObject, TypiQLElement, Serialize, Deserialize, Clone)]
#[graphql(input_name = "EventInput")]
pub struct Event { pub kind: String, pub at: i64 }

#[typiql_type]
pub struct Node {
    #[typiql(key)] pub id: String,
    #[typiql(elements)] pub events: Vec<Event>,
}
```

```graphql
getNodes(events_any: { kind: "out", at_lt: 4 })     # one element meets both
getNodes(events_last: { kind_not: "outside" })       # also _first, _atIndex: { index, filter }
```

A position with no element is checked as an empty element (Mongo's
`$arrayElemAt`), so `events_last: { kind_not: "outside" }` matches an empty
list.

### Relations

```rust
#[typiql_type]
pub struct Client {
    #[typiql(key)] pub id: String,
    pub name: String,
    // Conditions take any operator, and a parent field (`local`), a literal
    // or a variable (`value`); a relation may sort and page its own rows.
    #[typiql(relation(local = "id", foreign = "client_id"))]
    #[typiql(relation(foreign = "deleted", op = "not", value = true))]
    #[typiql(relation(order_by_desc = "created_at", limit = 5))]
    pub recent_visits: Vec<Visit>,
    // With a `data_name`, a relation is joined into the parent's own reads
    // (C#'s relation columns with a DataName), so clients can sort by it:
    // `getClients(_orderBy: "caseOwner.name")`.
    #[typiql(data_name = "caseOwnerDoc", relation(local = "case_owner_id", foreign = "id"))]
    pub case_owner: Option<Box<User>>,
    pub case_owner_id: Option<String>,
}
```

Has-one relations are batched across a whole query; joined ones cost no
extra query at all. Two types relating to each other one-to-one need a
`Box` on one side (`Option<Box<T>>`). Joins are done by the adapter when
both types share it (DuckDB in SQL), and by the core otherwise — results
are the same either way.

### Declaring the CRUD surface

By default every type gets the full surface. `ops(...)` generates only
what's declared — an operation left out doesn't exist in the schema:

```rust
// Append-only log: no update or remove, list filterable by `source` only.
#[typiql_type(ops(add, list(filters(source))))]
pub struct LogEntry { /* ... */ }
```

`get`/`list` take `filters(field, ...)` to narrow their filter arguments
(each listed field keeps every operator its type supports); blank means
every field, `filters()` means none.

### Upsert

An update creates the row when none matches — C#'s `_upsert`, on every
adapter. A plain update takes `_upsert: true` from the client; an
operation can also upsert always, and find its row by fields other than
the key:

```rust
#[typiql_type(ops(
    update,                                               // updateProfile(id, update, _upsert)
    // "Save my settings": no key argument, the scope finds the row.
    update(name = "updateMyProfile", by(), scope(username = "@currentUser"), upsert, set(seen = "@now")),
    // An import that updates or creates by an external id.
    update(name = "importProfile", by(external_id), upsert),
))]
```

A created row gets the update's values, the equality values that were
searched for (key, `by` fields, scope), and `set`.

### Bulk operations

Opt-in only — a type without `ops(...)` never gets them:

```rust
#[typiql_type(ops(list, add_many, update_many, remove_many,
    remove_many(name = "clearMyTasks", filters(), scope(owner = "@currentUser"))))]
pub struct Task { /* ... */ }
```

This adds `addTasks(items: [TaskInput!]!)` (all-or-nothing: one duplicate
key rejects the batch), `updateTasks(<filters>, update: TaskInput!)` and
`removeTasks(<filters>)`, each returning the affected rows and publishing a
change event per row. A bulk update or remove with no filter and no scope
is refused rather than applied to every row.

### Integer keys

A key field may be an integer instead of a `String`. On every adapter,
`add` without a key assigns the next one (max + 1; `IDENTITY` on SQL
Server), and `get`/`update`/`remove` take the key as an `Int`:

```rust
#[typiql_type]
pub struct Counter {
    #[typiql(key)]
    pub id: i64,
    pub label: String,
}
```

### Aggregate types

A read-only type computed from another type's rows, grouped by its plain
fields (none: one row over the whole source, or no row if it's empty):

```rust
#[typiql_type(aggregate = "Item")]
pub struct ItemSummary {
    pub owner: Option<String>,       // group by Item.owner
    #[typiql(aggregate(count))]
    pub items: i64,
    #[typiql(aggregate(sum = "price"))]
    pub total: f64,
    #[typiql(aggregate(avg = "price"))]
    pub average: Option<f64>,
    #[typiql(aggregate(max = "price"))]
    pub highest: Option<f64>,
}
```

`count`, `count = "field"` (non-null values), `sum`, `avg`, `min`, `max`.
It gets `get`/`list` (with filters, sorting, paging and PageInfo over the
aggregated rows) and uses the source's adapter. DuckDB and SQL Server run
it as `GROUP BY`, MongoDB as `$group`; the JSON adapter computes it in memory. As in Mongo, a sum
of nothing is 0 and `avg`/`min`/`max` of nothing are null.

### Stored procedures (SQL Server)

As in C#, a type can be served by a stored procedure instead of a table.
Its key fields are the procedure's parameters, in order, taken from the
query's equality filters (missing ones are passed as `NULL`). As in C#,
other filters, sorting and paging don't apply — the procedure's result is
returned as is:

```rust
#[typiql_type(adapter = "sql", procedure = "dbo.SalesByRegion", ops(get, list))]
pub struct SalesSummary {
    #[typiql(key)]
    pub region: String,
    #[typiql(key)]
    pub year: i64,
    pub total: f64,
}
```

Several `#[typiql(key)]` fields are allowed only on procedure types. They
are read-only; other adapters ignore `procedure`.

### Directory types (LDAP / Active Directory)

Each type is a class of directory entries, configured on the adapter — the
LDAP filter that selects them (C#'s model name), where they live, and, for
types that can be added to, how new entries are named:

```rust
let ad = LdapAdapter::new("ldaps://dc1.corp.example", "DC=corp,DC=example",
        Bind::Simple { dn: "svc-typiql@corp.example".into(), password })
    .class("users", LdapClass::new("(&(objectCategory=person)(objectClass=user))"))
    .class("groups", LdapClass::new("(objectClass=group)"));

#[typiql_type(adapter = "ad", ops(get, list, update))]
pub struct User {
    #[typiql(key, data_name = "sAMAccountName")]
    pub username: String,
    #[typiql(data_name = "displayName")]
    pub name: Option<String>,
    #[typiql(data_name = "memberOf")]
    pub groups: Vec<String>,          // multi-valued: every value
    #[typiql(data_name = "objectSid")]
    pub sid: Option<String>,          // S-1-5-21-…
    #[typiql(data_name = "thumbnailPhoto")]
    pub photo: Option<String>,        // base64; stored as a JPEG ≤ 512px
    pub dn: Option<String>,           // the entry's DN
}
```

- A field's `data_name` is its attribute; `dn` is the entry's DN.
- Equality, `in` and the pattern operators are LDAP filters, so they
  follow the directory's matching rules. For most AD attributes that's
  case-insensitive, as in C#.
- Text ordering, list-element filters, sorting and paging run in memory.
- Searches are paged, and ranged attributes (`member;range=…`) are read in
  full.
- A list field's values are a set: their order isn't kept.
- Writing `""` or null removes an attribute.
- `LdapClass::creatable(rdn, object_classes)` allows `add`. Clients then
  give the naming attribute, so mark the key `#[typiql(key, input)]`.
- Kerberos (integrated) binds need the `gssapi` feature.

### Logging

Including a log type in the schema turns logging on; its rows are stored
through the adapter it names and are queryable like any other type:

```rust
#[typiql_log(adapter = "logs", ops(list(roles("Admin")), get(roles("Admin"))))]
pub struct Log;                                   // typiql supplies the fields

typiql_schema!(Car, File, Log; ...);
```

- **Every error** is logged — from any operation, hand-written ones too —
  with its (variable-resolved) arguments, user and path. Refusals by
  `roles` included.
- **Successes** are logged where you opt in: `ops(add(log))`, a whole type
  with `#[typiql_type(log)]`, or a relation field with `#[typiql(log)]` —
  with arguments, result and duration.
- Entries are written in the background. To send them elsewhere, implement
  `TypiQLLogger` and register it:
  `typiql::enable_logging(typiql_schema_builder(adapters), Arc::new(MyLogger), typiql_operation_kinds())`.

Give logs their own adapter (e.g. a second `JsonAdapter` on a `logs/`
directory) so log writes never touch your data file.

### Optional auth

typiql never authenticates — the app does, in its own axum middleware, and
inserts its own user type into the request extensions. Apps without users
skip all of this.

```rust
#[derive(Serialize, Clone)]
pub struct MyUser { pub username: String, pub groups: Vec<String>, pub region: String }

impl TypiQLUser for MyUser {
    fn name(&self) -> &str { &self.username }
    fn has_role(&self, role: &str) -> bool { self.groups.iter().any(|g| g == role) }
}

let typiql = TypiQLRouter::new(schema, "/typiql/graphql").with_user::<MyUser>().build();
```

Operations can then declare:

- `roles("…", ...)` — the user must hold one of them.
- `scope(field = value, ...)` — equality filters the server always applies
  (the client can't see or override them). On `update`/`remove` a record
  outside the scope behaves as missing; on `subscribe`, its events aren't sent.
- `set(field = value, ...)` — `add`/`update` values that overwrite the client's.
- `name = "…"` — a custom GraphQL name, so a type can have several of one kind.

Values are literals or `"@currentUser"`, `"@currentUser.<field>"` (any
serialized field of the app's user type) and `"@now"`:

```rust
// Cars are only readable through `my { cars }`, and only their owner can change them.
#[typiql_type(ops(
    add(roles("User"), set(owner = "@currentUser")),
    update(roles("User"), scope(owner = "@currentUser")),
    remove(roles("User"), scope(owner = "@currentUser")),
))]
pub struct Car { #[typiql(key)] pub id: String, pub owner: String, pub make: String }

#[typiql_type(ops(get(name = "my", scope(username = "@currentUser"))))]
pub struct User {
    #[typiql(key)] pub username: String,
    #[typiql(relation(local = "username", foreign = "owner"))]
    pub cars: Vec<Car>,
}
```

Login/logout are ordinary hand-written resolvers, so each app keeps full
control of its session; they can set cookies with
`ctx.append_http_header("set-cookie", ...)`. Custom resolvers read the
app's user type with `ctx.data::<MyUser>()`.

Hand-written resolvers are still the right tool for anything that isn't
CRUD (cross-row invariants, live external-process control, non-JSON file
parsing) — typiql only generates the CRUD surface. They should still read
and write through typiql:

- **The helpers**: `typiql::resolve_list` / `resolve_query` / `resolve_add` /
  `resolve_update` / `resolve_remove`.
- **Publishing**: only the generated mutations publish change events. A
  hand-written write must call
  `TypiQLBroker::publish(CarChanged { operation_name: "update".into(), value })`
  itself, or subscribers never see it.


## License

MIT — see [LICENSE](./LICENSE).
