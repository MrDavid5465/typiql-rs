# typiql-rs

A small Rust ORM that turns plain structs into a full GraphQL CRUD API —
queries, mutations, and live-update subscriptions — generated at compile
time, with no schema or migration files to write by hand.

Extracted from [typiql-tauri](https://github.com/MrDavid5465/typiql-tauri),
where it's the backend data layer.

## Crates

- **typiql-core** — the runtime: adapter trait (`TypiQLAdapter`), the
  `Location` addressing scheme (single shared file vs. per-record files),
  filtering (`FieldFilter`/`FilterOp`), and batched relation loading via
  `DataLoader`.
- **typiql-macros** — the `#[typiql_type]` and `typiql_schema!` proc macros.
  Annotating a struct generates its GraphQL object type, a typed filter
  argument per field, `get`/`getMany`/`add`/`update`/`remove` resolvers, a
  `{Type}Changed` subscription, and (via `#[typiql(relation(...))]`)
  batched-relation fields resolved through an auto-registered `DataLoader`.
- **typiql-adapter-json** — a `TypiQLAdapter` implementation backed by a
  single JSON document (or per-record files via `Location::At`, for content
  like uploaded sprites that shouldn't all live in one growing file).

- **typiql-adapter-duckdb** — a `TypiQLAdapter` backed by DuckDB, for
  columnar/analytical data.
- **typiql-conformance** (unpublished) — runs one GraphQL-only test suite
  against every adapter, alone and mixed. Which adapter serves a type is
  backend configuration; clients must not be able to tell the difference.
  Where adapters could disagree (e.g. filters over null values), the
  original C# Mongo adapter's semantics are the reference.

## Usage

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
schema the macro call produces.

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
parsing) — typiql only generates the CRUD surface.

## License

MIT — see [LICENSE](./LICENSE).
