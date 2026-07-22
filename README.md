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
`addCar`, `updateCar`, `removeCar`, a `carChanged` subscription, and a
batched `day_photo` relation field — all wired into the schema the macro
call produces.

Hand-written resolvers are still the right tool for anything that isn't
CRUD (cross-row invariants, live external-process control, non-JSON file
parsing) — typiql only generates the CRUD surface.

## License

MIT — see [LICENSE](./LICENSE).
