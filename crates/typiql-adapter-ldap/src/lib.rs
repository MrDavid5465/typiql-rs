//! A `TypiQLAdapter` backed by an LDAP directory — Active Directory or any
//! other — the Rust port of the original C# typiql's AD adapter (`ADData.cs`).
//!
//! Each type is a class of directory entries, configured on the adapter
//! ([`LdapClass`]): the LDAP filter that selects them (C#'s model name, e.g.
//! `(&(objectCategory=person)(objectClass=user))`), where they live, and how
//! new ones are named. A field's `data_name` is its attribute; the pseudo
//! attribute `dn` is the entry's distinguished name.
//!
//! Filters go to the directory as LDAP filters, so equality, `in` and the
//! pattern operators follow the directory's matching rules — for most AD
//! attributes, case-insensitive, as they were in C#. What LDAP can't express
//! (ordering on text, filters inside lists) is applied in memory with the
//! reference rules, as are sorting and paging (C# did those in memory too).
//! Searches are paged, so AD's 1,000-entry limit doesn't cut results short,
//! and AD's ranged attributes (`member;range=0-1499`) are read in full.
//!
//! A list field is a multi-valued attribute: its values are a set, so their
//! order isn't kept.
//!
//! AD's binary attributes read as text: `objectSid` as `S-1-5-…`,
//! `objectGUID` as its GUID string, photos and certificates as base64. As in
//! C#, a `thumbnailPhoto` written in any common image format is stored as a
//! JPEG no larger than 512 pixels.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use base64::Engine;
use ldap3::adapters::{Adapter, EntriesOnly, PagedResults};
use ldap3::{ldap_escape, Ldap, LdapConnAsync, LdapConnSettings, Mod, Scope, SearchEntry};
use serde_json::{Map, Value};
use typiql_core::query::matches_all;
use typiql_core::{FieldFilter, FilterOp, Location, TypiQLAdapter};

/// The entry's distinguished name, as a field.
const DN: &str = "dn";
/// LDAP result codes.
const NO_SUCH_OBJECT: u32 = 32;
const ALREADY_EXISTS: u32 = 68;
/// Matches no entry.
const NOTHING: &str = "(!(objectClass=*))";
const PAGE_SIZE: i32 = 500;
const THUMBNAIL_SIZE: u32 = 512;

/// How the adapter authenticates.
#[derive(Clone, Debug)]
pub enum Bind {
    Anonymous,
    /// A DN — or, on AD, `user@domain` — and password. Use `ldaps://` (or
    /// StartTLS) so the password isn't sent in the clear.
    Simple {
        dn: String,
        password: String,
    },
    /// Kerberos as the process's own identity — C#'s integrated auth.
    /// `server_fqdn` is the directory server's host name.
    #[cfg(feature = "gssapi")]
    Kerberos {
        server_fqdn: String,
    },
}

/// Which entries a type is, and where new ones go.
#[derive(Clone, Debug)]
pub struct LdapClass {
    filter: String,
    base: Option<String>,
    rdn: Option<String>,
    object_classes: Vec<String>,
}

impl LdapClass {
    /// The entries matching `filter` (an LDAP filter, parenthesised).
    pub fn new(filter: &str) -> Self {
        LdapClass {
            filter: filter.to_string(),
            base: None,
            rdn: None,
            object_classes: Vec::new(),
        }
    }

    /// Search under `base` rather than the adapter's base DN; new entries are
    /// created here too.
    pub fn base(mut self, base: &str) -> Self {
        self.base = Some(base.to_string());
        self
    }

    /// Lets the type be added to: new entries are named `rdn=<value>,<base>`
    /// and get these object classes.
    pub fn creatable(mut self, rdn: &str, object_classes: &[&str]) -> Self {
        self.rdn = Some(rdn.to_string());
        self.object_classes = object_classes.iter().map(|c| c.to_string()).collect();
        self
    }
}

#[derive(Clone)]
pub struct LdapAdapter {
    inner: Arc<Inner>,
}

struct Inner {
    url: String,
    base_dn: String,
    bind: Bind,
    settings: LdapConnSettings,
    classes: RwLock<HashMap<String, LdapClass>>,
    tables: RwLock<HashMap<String, Table>>,
    conn: tokio::sync::Mutex<Option<Ldap>>,
}

#[derive(Clone, Default)]
struct Table {
    key: String,
    /// Attribute and its column type, as registered.
    columns: Vec<(String, String)>,
}

impl Table {
    fn ty(&self, attr: &str) -> &str {
        self.columns
            .iter()
            .find(|(a, _)| a.eq_ignore_ascii_case(attr))
            .map(|(_, t)| t.as_str())
            .unwrap_or("VARCHAR")
    }
}

impl LdapAdapter {
    /// `url` is `ldap://host`, `ldaps://host` or `ldapi://…`; `base_dn` is
    /// where searches start unless a class says otherwise. The connection
    /// is made on first use.
    pub fn new(url: &str, base_dn: &str, bind: Bind) -> Self {
        Self::with_settings(url, base_dn, bind, LdapConnSettings::new())
    }

    /// With the connection's settings (StartTLS, TLS configuration,
    /// timeouts) given.
    pub fn with_settings(url: &str, base_dn: &str, bind: Bind, settings: LdapConnSettings) -> Self {
        LdapAdapter {
            inner: Arc::new(Inner {
                url: url.to_string(),
                base_dn: base_dn.to_string(),
                bind,
                settings,
                classes: RwLock::new(HashMap::new()),
                tables: RwLock::new(HashMap::new()),
                conn: tokio::sync::Mutex::new(None),
            }),
        }
    }

    /// Serves the collection `collection` (a type's `collection_name()`)
    /// from the entries `class` describes.
    pub fn class(self, collection: &str, class: LdapClass) -> Self {
        self.inner
            .classes
            .write()
            .unwrap()
            .insert(collection.to_string(), class);
        self
    }

    async fn ldap(&self) -> Result<Ldap, String> {
        let mut conn = self.inner.conn.lock().await;
        if let Some(ldap) = conn.as_mut() {
            if !ldap.is_closed() {
                return Ok(ldap.clone());
            }
        }
        let (c, mut ldap) =
            LdapConnAsync::with_settings(self.inner.settings.clone(), &self.inner.url)
                .await
                .map_err(err)?;
        ldap3::drive!(c);
        match &self.inner.bind {
            Bind::Anonymous => {}
            Bind::Simple { dn, password } => {
                ldap.simple_bind(dn, password)
                    .await
                    .map_err(err)?
                    .success()
                    .map_err(err)?;
            }
            #[cfg(feature = "gssapi")]
            Bind::Kerberos { server_fqdn } => {
                ldap.sasl_gssapi_bind(server_fqdn)
                    .await
                    .map_err(err)?
                    .success()
                    .map_err(err)?;
            }
        }
        *conn = Some(ldap.clone());
        Ok(ldap)
    }

    async fn reset(&self) {
        *self.inner.conn.lock().await = None;
    }

    fn resolve(
        &self,
        location: &Location,
        key_field: &str,
    ) -> Result<(String, Table, LdapClass), String> {
        let name = match location {
            Location::Named(t) => t.clone(),
            Location::At { table, .. } => table.clone(),
        };
        let class = self
            .inner
            .classes
            .read()
            .unwrap()
            .get(&name)
            .cloned()
            .ok_or_else(|| format!("ldap adapter: no LdapClass configured for `{name}`"))?;
        let table = self.inner.tables.read().unwrap().get(&name).cloned();
        let table = table.unwrap_or_else(|| Table {
            key: key_field.to_string(),
            ..Default::default()
        });
        Ok((name, table, class))
    }

    fn base<'a>(&'a self, class: &'a LdapClass) -> &'a str {
        class.base.as_deref().unwrap_or(&self.inner.base_dn)
    }

    /// Every entry matching `filter` under `base`, paged, with ranged
    /// attributes read in full; a missing base is no entries.
    async fn search(
        &self,
        base: &str,
        scope: Scope,
        filter: &str,
        attrs: &[String],
    ) -> Result<Vec<SearchEntry>, String> {
        let mut entries = self.search_once(base, scope, filter, attrs).await?;
        for entry in &mut entries {
            self.read_ranges(entry).await?;
        }
        Ok(entries)
    }

    async fn search_once(
        &self,
        base: &str,
        scope: Scope,
        filter: &str,
        attrs: &[String],
    ) -> Result<Vec<SearchEntry>, String> {
        let mut ldap = self.ldap().await?;
        let attrs: Vec<String> = if attrs.is_empty() {
            vec!["1.1".to_string()]
        } else {
            attrs.to_vec()
        };
        let adapters: Vec<Box<dyn Adapter<'_, String, Vec<String>>>> = if scope == Scope::Base {
            vec![Box::new(EntriesOnly::new())]
        } else {
            vec![
                Box::new(EntriesOnly::new()),
                Box::new(PagedResults::new(PAGE_SIZE)),
            ]
        };
        let result = async {
            let mut stream = ldap
                .streaming_search_with(adapters, base, scope, filter, attrs)
                .await?;
            let mut entries = Vec::new();
            while let Some(entry) = stream.next().await? {
                entries.push(SearchEntry::construct(entry));
            }
            Ok::<_, ldap3::LdapError>((entries, stream.finish().await))
        }
        .await;
        let (entries, done) = match result {
            Ok(r) => r,
            Err(e) => {
                self.reset().await;
                return Err(err(e));
            }
        };
        if done.rc == NO_SUCH_OBJECT {
            return Ok(Vec::new());
        }
        done.success().map_err(err)?;
        Ok(entries)
    }

    /// AD returns large multi-valued attributes in ranges
    /// (`member;range=0-1499`); reads the rest.
    async fn read_ranges(&self, entry: &mut SearchEntry) -> Result<(), String> {
        loop {
            let partial = entry
                .attrs
                .keys()
                .chain(entry.bin_attrs.keys())
                .find_map(|name| {
                    let (attr, range) = name.split_once(";range=")?;
                    let (_, end) = range.split_once('-')?;
                    (end != "*").then(|| (name.clone(), attr.to_string(), end.parse::<u64>().ok()))
                });
            let Some((name, attr, Some(end))) = partial else {
                return Ok(());
            };
            let next = format!("{attr};range={}-*", end + 1);
            let more = self
                .search_once(&entry.dn, Scope::Base, "(objectClass=*)", &[next])
                .await?;
            let mut values = entry.attrs.remove(&name).unwrap_or_default();
            let mut bin = entry.bin_attrs.remove(&name).unwrap_or_default();
            let mut continued = None;
            for m in more {
                for (n, v) in m.attrs {
                    values.extend(v);
                    continued = Some(n);
                }
                for (n, v) in m.bin_attrs {
                    bin.extend(v);
                    continued = Some(n);
                }
            }
            // Keep the name of the last range read, so the loop continues
            // until the server says `*`.
            let name = continued.filter(|n| !n.ends_with("-*")).unwrap_or(attr);
            if !values.is_empty() {
                entry.attrs.insert(name.clone(), values);
            }
            if !bin.is_empty() {
                entry.bin_attrs.insert(name, bin);
            }
        }
    }

    /// The rows matching `filters`: the directory does what LDAP can say,
    /// memory the rest.
    async fn fetch(
        &self,
        table: &Table,
        class: &LdapClass,
        filters: &[FieldFilter],
    ) -> Result<Vec<Value>, String> {
        let mut pushed = Vec::new();
        let mut in_memory = Vec::new();
        let mut bases: Option<Vec<String>> = None;
        for f in filters {
            if f.field == DN {
                match (f.op, &f.value) {
                    (FilterOp::Eq, Value::String(dn)) if bases.is_none() => {
                        bases = Some(vec![dn.clone()])
                    }
                    (FilterOp::In, Value::Array(dns)) if bases.is_none() => {
                        bases = Some(
                            dns.iter()
                                .filter_map(|d| d.as_str().map(str::to_string))
                                .collect(),
                        )
                    }
                    _ => in_memory.push(f.clone()),
                }
                continue;
            }
            match ldap_filter(f, table) {
                Some((part, exact)) => {
                    pushed.push(part);
                    if !exact {
                        in_memory.push(f.clone());
                    }
                }
                None => in_memory.push(f.clone()),
            }
        }
        let filter = format!("(&{}{})", class.filter, pushed.concat());
        let attrs = attributes(table);
        let mut entries = Vec::new();
        match bases {
            // By DN: read those entries (DNs match as the directory
            // compares them, e.g. regardless of case).
            Some(dns) => {
                for dn in dns {
                    entries.extend(self.search(&dn, Scope::Base, &filter, &attrs).await?);
                }
            }
            None => {
                entries = self
                    .search(self.base(class), Scope::Subtree, &filter, &attrs)
                    .await?
            }
        }
        Ok(entries
            .into_iter()
            .map(|e| row(table, e))
            .filter(|r| matches_all(r, &in_memory))
            .collect())
    }

    /// The entry a key names, as `(dn, row)`.
    async fn find(
        &self,
        table: &Table,
        class: &LdapClass,
        key: &str,
    ) -> Result<Option<(String, Value)>, String> {
        let filter = FieldFilter {
            field: table.key.clone(),
            op: FilterOp::Eq,
            value: key_value(table, key),
        };
        let rows = self.fetch(table, class, &[filter]).await?;
        Ok(rows.into_iter().next().map(|r| {
            (
                r.get(DN)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                r,
            )
        }))
    }

    /// The row as the directory has it now.
    async fn reread(
        &self,
        table: &Table,
        class: &LdapClass,
        dn: &str,
    ) -> Result<Option<Value>, String> {
        let f = FieldFilter {
            field: DN.into(),
            op: FilterOp::Eq,
            value: Value::String(dn.to_string()),
        };
        Ok(self.fetch(table, class, &[f]).await?.into_iter().next())
    }

    async fn update_one(
        &self,
        table: &Table,
        class: &LdapClass,
        key: &str,
        patch: &Value,
    ) -> Result<Option<Value>, String> {
        let Some((dn, _)) = self.find(table, class, key).await? else {
            return Ok(None);
        };
        let mut mods = Vec::new();
        for (field, value) in patch.as_object().into_iter().flatten() {
            if *field == table.key || field == DN {
                continue;
            }
            mods.push(Mod::Replace(
                field.as_bytes().to_vec(),
                attribute_values(field, value)?,
            ));
        }
        if !mods.is_empty() {
            let mut ldap = self.ldap().await?;
            ldap.modify(&dn, mods)
                .await
                .map_err(err)?
                .success()
                .map_err(err)?;
        }
        self.reread(table, class, &dn).await
    }

    async fn add_one(
        &self,
        table: &Table,
        class: &LdapClass,
        mut row: Value,
    ) -> Result<Value, String> {
        let rdn = class
            .rdn
            .as_deref()
            .ok_or("ldap adapter: this class isn't creatable (see LdapClass::creatable)")?;
        if table
            .ty(&table.key)
            .trim_end_matches(" NOT NULL")
            .ends_with("INT")
            && key_text(&row, &table.key).is_none()
        {
            let rows = self.fetch(table, class, &[]).await?;
            let next = rows
                .iter()
                .filter_map(|r| r.get(&table.key).and_then(Value::as_i64))
                .max()
                .unwrap_or(0)
                + 1;
            if let Value::Object(map) = &mut row {
                map.insert(table.key.clone(), Value::from(next));
            }
        }
        if let Some(key) = key_text(&row, &table.key) {
            if self.find(table, class, &key).await?.is_some() {
                return Err(format!("Duplicate key `{key}`"));
            }
        }
        let rdn_value = key_text(&row, rdn)
            .ok_or_else(|| format!("ldap adapter: a new entry needs `{rdn}`, its name"))?;
        let dn = format!(
            "{rdn}={},{}",
            ldap3::dn_escape(rdn_value.as_str()),
            self.base(class)
        );
        let mut attrs: Vec<(Vec<u8>, HashSet<Vec<u8>>)> = Vec::new();
        if !class.object_classes.is_empty() {
            attrs.push((
                b"objectClass".to_vec(),
                class
                    .object_classes
                    .iter()
                    .map(|c| c.as_bytes().to_vec())
                    .collect(),
            ));
        }
        for (field, value) in row.as_object().into_iter().flatten() {
            if field == DN {
                continue;
            }
            let values = attribute_values(field, value)?;
            if !values.is_empty() {
                attrs.push((field.as_bytes().to_vec(), values));
            }
        }
        let mut ldap = self.ldap().await?;
        let done = ldap.add(&dn, attrs).await.map_err(err)?;
        if done.rc == ALREADY_EXISTS {
            return Err(format!("Duplicate key `{rdn_value}`"));
        }
        done.success().map_err(err)?;
        self.reread(table, class, &dn)
            .await?
            .ok_or_else(|| format!("ldap adapter: {dn} was added but can't be read"))
    }

    async fn delete(&self, dn: &str) -> Result<(), String> {
        let mut ldap = self.ldap().await?;
        ldap.delete(dn).await.map_err(err)?.success().map_err(err)?;
        Ok(())
    }

    fn log<T: Default>(result: Result<T, String>) -> T {
        result.unwrap_or_else(|e| {
            eprintln!("typiql-adapter-ldap: {e}");
            T::default()
        })
    }
}

#[async_trait]
impl TypiQLAdapter for LdapAdapter {
    async fn get_one(&self, location: Location, key_field: &str, key_value: &str) -> Option<Value> {
        let result = async {
            let (_, table, class) = self.resolve(&location, key_field)?;
            let table = Table {
                key: key_field.to_string(),
                ..table
            };
            Ok(self
                .find(&table, &class, key_value)
                .await?
                .map(|(_, row)| row))
        }
        .await;
        Self::log(result)
    }

    async fn get_many(&self, location: Location, filters: Vec<FieldFilter>) -> Vec<Value> {
        let result = async {
            let (_, table, class) = self.resolve(&location, "")?;
            self.fetch(&table, &class, &filters).await
        }
        .await;
        Self::log(result)
    }

    async fn get_many_matching_any(
        &self,
        location: Location,
        or_groups: Vec<Vec<FieldFilter>>,
    ) -> Vec<Value> {
        let result = async {
            let (_, table, class) = self.resolve(&location, "")?;
            // One search when every group is plain LDAP; otherwise one per
            // group.
            let plain: Option<Vec<String>> = or_groups
                .iter()
                .map(|g| {
                    g.iter()
                        .map(|f| {
                            (f.field != DN)
                                .then(|| ldap_filter(f, &table))
                                .flatten()
                                .filter(|(_, exact)| *exact)
                                .map(|(p, _)| p)
                        })
                        .collect::<Option<Vec<_>>>()
                        .map(|parts| format!("(&{})", parts.concat()))
                })
                .collect();
            let mut rows = Vec::new();
            match plain {
                Some(groups) if !groups.is_empty() => {
                    let filter = format!("(&{}(|{}))", class.filter, groups.concat());
                    let entries = self
                        .search(
                            self.base(&class),
                            Scope::Subtree,
                            &filter,
                            &attributes(&table),
                        )
                        .await?;
                    rows.extend(entries.into_iter().map(|e| row(&table, e)));
                }
                _ => {
                    let mut seen = HashSet::new();
                    for g in &or_groups {
                        for r in self.fetch(&table, &class, g).await? {
                            if seen.insert(
                                r.get(DN)
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_lowercase(),
                            ) {
                                rows.push(r);
                            }
                        }
                    }
                }
            }
            Ok(rows)
        }
        .await;
        Self::log(result)
    }

    async fn add(
        &self,
        location: Location,
        key_field: &str,
        values: Value,
    ) -> Result<Value, String> {
        let (_, table, class) = self.resolve(&location, key_field)?;
        let table = Table {
            key: key_field.to_string(),
            ..table
        };
        self.add_one(&table, &class, values).await
    }

    /// All or none: entries added before a failure are removed again.
    async fn add_many(
        &self,
        location: Location,
        key_field: &str,
        values: Vec<Value>,
    ) -> Result<Vec<Value>, String> {
        let (_, table, class) = self.resolve(&location, key_field)?;
        let table = Table {
            key: key_field.to_string(),
            ..table
        };
        let mut seen = HashSet::new();
        for v in &values {
            if let Some(k) = key_text(v, key_field) {
                if !seen.insert(k.clone()) {
                    return Err(format!("Duplicate key `{k}`"));
                }
            }
        }
        let mut added: Vec<Value> = Vec::new();
        for v in values {
            match self.add_one(&table, &class, v).await {
                Ok(row) => added.push(row),
                Err(e) => {
                    for row in &added {
                        if let Some(dn) = row.get(DN).and_then(Value::as_str) {
                            let _ = self.delete(dn).await;
                        }
                    }
                    return Err(e);
                }
            }
        }
        Ok(added)
    }

    async fn update(
        &self,
        location: Location,
        key_field: &str,
        key_value: &str,
        patch: Value,
    ) -> Option<Value> {
        let result = async {
            let (_, table, class) = self.resolve(&location, key_field)?;
            let table = Table {
                key: key_field.to_string(),
                ..table
            };
            self.update_one(&table, &class, key_value, &patch).await
        }
        .await;
        Self::log(result)
    }

    async fn remove(&self, location: Location, key_field: &str, key_value: &str) -> Option<Value> {
        let result = async {
            let (_, table, class) = self.resolve(&location, key_field)?;
            let table = Table {
                key: key_field.to_string(),
                ..table
            };
            let Some((dn, row)) = self.find(&table, &class, key_value).await? else {
                return Ok(None);
            };
            self.delete(&dn).await?;
            Ok(Some(row))
        }
        .await;
        Self::log(result)
    }

    async fn set_table(&self, location: Location, _rows: Vec<Value>) {
        eprintln!("typiql-adapter-ldap: replacing every entry of {location:?} isn't supported");
    }

    fn register_type(
        &self,
        table: &str,
        key_field: &str,
        columns: &[(&'static str, &'static str)],
    ) {
        let columns = columns
            .iter()
            .map(|(n, t)| (n.to_string(), t.to_string()))
            .collect();
        self.inner.tables.write().unwrap().insert(
            table.to_string(),
            Table {
                key: key_field.to_string(),
                columns,
            },
        );
    }
}

// ── filters ─────────────────────────────────────────────────────────────────

/// A filter as LDAP, and whether LDAP's answer is the whole answer (`false`:
/// it's a superset, narrowed in memory). `None` when LDAP can't say it.
fn ldap_filter(f: &FieldFilter, table: &Table) -> Option<(String, bool)> {
    let attr = &f.field;
    if !is_attribute(attr) {
        return None;
    }
    let ty = table.ty(attr).trim_end_matches(" NOT NULL");
    let eq = |v: &Value| -> Option<String> {
        match v {
            Value::Null => Some(format!("(!({attr}=*))")),
            v => Some(format!("({attr}={})", assertion(attr, v)?)),
        }
    };
    let any = |v: &Value| -> Option<String> {
        let parts: Vec<String> = v.as_array()?.iter().map(eq).collect::<Option<_>>()?;
        Some(if parts.is_empty() {
            NOTHING.to_string()
        } else {
            format!("(|{})", parts.concat())
        })
    };
    let pattern = |make: &dyn Fn(&str) -> String, negated: bool| -> Option<String> {
        let needle = ldap_escape(f.value.as_str()?).into_owned();
        let p = format!("({attr}={})", make(&needle));
        // A negated pattern never matches a missing value.
        Some(if negated {
            format!("(&({attr}=*)(!{p}))")
        } else {
            p
        })
    };
    let integer = is_integer(ty);
    let exact = |s: Option<String>| s.map(|s| (s, true));
    match f.op {
        FilterOp::Eq => exact(eq(&f.value)),
        FilterOp::Ne => exact(eq(&f.value).map(|p| format!("(!{p})"))),
        FilterOp::In | FilterOp::AnyIn => exact(any(&f.value)),
        FilterOp::NotIn | FilterOp::NoneIn => exact(any(&f.value).map(|p| format!("(!{p})"))),
        FilterOp::StartsWith => exact(pattern(&|n| format!("{n}*"), false)),
        FilterOp::EndsWith => exact(pattern(&|n| format!("*{n}"), false)),
        FilterOp::Contains => exact(pattern(&|n| format!("*{n}*"), false)),
        FilterOp::NotStartsWith => exact(pattern(&|n| format!("{n}*"), true)),
        FilterOp::NotEndsWith => exact(pattern(&|n| format!("*{n}"), true)),
        FilterOp::NotContains => exact(pattern(&|n| format!("*{n}*"), true)),
        // LDAP has only >= and <=, and orders text by each attribute's own
        // rule (if any): integers only, narrowed in memory.
        FilterOp::Gt | FilterOp::Gte if integer => f
            .value
            .as_i64()
            .map(|v| (format!("({attr}>={v})"), f.op == FilterOp::Gte)),
        FilterOp::Lt | FilterOp::Lte if integer => f
            .value
            .as_i64()
            .map(|v| (format!("({attr}<={v})"), f.op == FilterOp::Lte)),
        _ => None,
    }
}

/// A value as an LDAP assertion value, escaped.
fn assertion(attr: &str, v: &Value) -> Option<String> {
    match v {
        Value::String(s) if attr.eq_ignore_ascii_case("objectGUID") => {
            guid_bytes(s).map(|b| escape_bytes(&b))
        }
        Value::String(s) => Some(ldap_escape(s.as_str()).into_owned()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(if *b { "TRUE" } else { "FALSE" }.to_string()),
        _ => None,
    }
}

fn is_attribute(name: &str) -> bool {
    name != DN
        && !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == ';' || c == '.')
}

fn escape_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("\\{b:02x}")).collect()
}

// ── values ──────────────────────────────────────────────────────────────────

/// The attributes to read: every registered column but `dn`.
fn attributes(table: &Table) -> Vec<String> {
    let mut attrs: Vec<String> = table
        .columns
        .iter()
        .map(|(a, _)| a.clone())
        .filter(|a| is_attribute(a))
        .collect();
    if attrs.is_empty() {
        attrs.push("*".into());
    }
    attrs
}

/// An entry as a row: every registered column, by its column type — a list
/// is every value, a scalar the first; missing is null (an empty list).
fn row(table: &Table, entry: SearchEntry) -> Value {
    let mut values: HashMap<String, (String, Vec<Vec<u8>>)> = HashMap::new();
    let attrs = entry
        .attrs
        .into_iter()
        .map(|(n, v)| (n, v.into_iter().map(String::into_bytes).collect::<Vec<_>>()));
    for (name, vals) in attrs.chain(entry.bin_attrs) {
        let base = name.split(';').next().unwrap_or(&name).to_string();
        values
            .entry(base.to_lowercase())
            .or_insert_with(|| (base, Vec::new()))
            .1
            .extend(vals);
    }
    let mut out = Map::new();
    if table.columns.is_empty() {
        for (name, vals) in values.values() {
            let v = if vals.len() == 1 {
                text(name, &vals[0])
            } else {
                Value::Array(vals.iter().map(|b| text(name, b)).collect())
            };
            out.insert(name.clone(), v);
        }
    }
    for (col, ty) in &table.columns {
        if col == DN {
            continue;
        }
        let vals = values
            .get(&col.to_lowercase())
            .map(|(_, v)| v.as_slice())
            .unwrap_or_default();
        out.insert(col.clone(), convert(col, ty, vals));
    }
    out.insert(DN.to_string(), Value::String(entry.dn));
    Value::Object(out)
}

fn convert(attr: &str, ty: &str, values: &[Vec<u8>]) -> Value {
    let base = ty.trim_end_matches(" NOT NULL");
    if base == "JSON" || base.ends_with("[]") {
        return Value::Array(values.iter().map(|v| text(attr, v)).collect());
    }
    let Some(first) = values.first() else {
        return Value::Null;
    };
    let s = String::from_utf8_lossy(first);
    match base {
        b if is_integer(b) => s
            .trim()
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or(Value::Null),
        "DOUBLE" | "FLOAT" | "REAL" => s
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        "BOOLEAN" => Value::Bool(s.eq_ignore_ascii_case("TRUE")),
        _ => text(attr, first),
    }
}

/// An attribute value as text: AD's SID and GUID in their usual forms, other
/// binary values as base64.
fn text(attr: &str, bytes: &[u8]) -> Value {
    let lower = attr.to_ascii_lowercase();
    let s = match lower.as_str() {
        "objectsid" | "securityidentifier" | "sidhistory" => sid_string(bytes),
        "objectguid" | "msexchmailboxguid" => guid_string(bytes),
        _ if is_binary(&lower) => None,
        _ => std::str::from_utf8(bytes).ok().map(str::to_string),
    };
    Value::String(s.unwrap_or_else(|| base64::engine::general_purpose::STANDARD.encode(bytes)))
}

fn is_binary(lower: &str) -> bool {
    matches!(
        lower,
        "thumbnailphoto"
            | "jpegphoto"
            | "photo"
            | "usercertificate"
            | "cacertificate"
            | "usersmimecertificate"
            | "objectsid"
            | "objectguid"
    ) || lower.ends_with(";binary")
}

/// A value to write, as the attribute's values; null and `""` delete it,
/// as in C#.
fn attribute_values(attr: &str, v: &Value) -> Result<HashSet<Vec<u8>>, String> {
    let one = |v: &Value| -> Result<Option<Vec<u8>>, String> {
        Ok(match v {
            Value::Null => None,
            Value::String(s) if s.is_empty() => None,
            Value::String(s) if is_binary(&attr.to_ascii_lowercase()) => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .map_err(|e| format!("ldap adapter: `{attr}` must be base64: {e}"))?;
                Some(if attr.eq_ignore_ascii_case("thumbnailPhoto") {
                    thumbnail(&bytes)?
                } else {
                    bytes
                })
            }
            Value::String(s) => Some(s.as_bytes().to_vec()),
            Value::Bool(b) => Some(if *b {
                b"TRUE".to_vec()
            } else {
                b"FALSE".to_vec()
            }),
            Value::Number(n) => Some(n.to_string().into_bytes()),
            other => Some(other.to_string().into_bytes()),
        })
    };
    match v {
        Value::Array(items) => items.iter().filter_map(|i| one(i).transpose()).collect(),
        v => Ok(one(v)?.into_iter().collect()),
    }
}

/// C#'s thumbnail rule: stored as JPEG, at most 512 pixels (kept in
/// proportion here, where C# squeezed it into 512 × 512).
fn thumbnail(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let image = image::load_from_memory(bytes)
        .map_err(|e| format!("ldap adapter: thumbnailPhoto isn't an image: {e}"))?;
    let image = if image.height() > THUMBNAIL_SIZE || image.width() > THUMBNAIL_SIZE {
        image.resize(
            THUMBNAIL_SIZE,
            THUMBNAIL_SIZE,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        image
    };
    let mut out = std::io::Cursor::new(Vec::new());
    image
        .to_rgb8()
        .write_to(&mut out, image::ImageFormat::Jpeg)
        .map_err(err)?;
    Ok(out.into_inner())
}

/// A binary SID as `S-1-5-21-…`.
fn sid_string(b: &[u8]) -> Option<String> {
    if b.len() < 8 || b.len() != 8 + 4 * b[1] as usize {
        return None;
    }
    let authority = b[2..8].iter().fold(0u64, |a, x| (a << 8) | *x as u64);
    let mut s = format!("S-{}-{authority}", b[0]);
    for chunk in b[8..].chunks(4) {
        s.push_str(&format!(
            "-{}",
            u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
        ));
    }
    Some(s)
}

/// A binary GUID in its usual text form (the first three groups are
/// little-endian, as Windows stores them).
fn guid_string(b: &[u8]) -> Option<String> {
    let b: &[u8; 16] = b.try_into().ok()?;
    Some(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    ))
}

fn guid_bytes(s: &str) -> Option<Vec<u8>> {
    let hex: String = s
        .chars()
        .filter(|c| *c != '-' && *c != '{' && *c != '}')
        .collect();
    if hex.len() != 32 {
        return None;
    }
    let raw: Vec<u8> = (0..16)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok())
        .collect::<Option<_>>()?;
    let order = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
    Some(order.iter().map(|&i| raw[i]).collect())
}

/// A key given as text, as the filter value its column compares with.
fn key_value(table: &Table, key: &str) -> Value {
    if is_integer(table.ty(&table.key).trim_end_matches(" NOT NULL")) {
        if let Ok(i) = key.parse::<i64>() {
            return Value::from(i);
        }
    }
    Value::String(key.to_string())
}

fn key_text(row: &Value, key: &str) -> Option<String> {
    match row.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn is_integer(base: &str) -> bool {
    matches!(
        base,
        "TINYINT"
            | "SMALLINT"
            | "INTEGER"
            | "BIGINT"
            | "HUGEINT"
            | "UTINYINT"
            | "USMALLINT"
            | "UINTEGER"
            | "UBIGINT"
            | "UHUGEINT"
    )
}

fn err(e: impl std::fmt::Display) -> String {
    format!("ldap adapter: {e}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sids_and_guids_read_as_text() {
        // S-1-5-21-1004336348-1177238915-682003330-512 (Domain Admins).
        let sid = [
            1, 5, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0, 0xdc, 0xf4, 0xdc, 0x3b, 0x83, 0x3d, 0x2b, 0x46,
            0x82, 0x8b, 0xa6, 0x28, 0x00, 0x02, 0x00, 0x00,
        ];
        assert_eq!(
            sid_string(&sid).unwrap(),
            "S-1-5-21-1004336348-1177238915-682003330-512"
        );
        let guid = guid_bytes("3f2504e0-4f89-11d3-9a0c-0305e82c3301").unwrap();
        assert_eq!(guid[..4], [0xe0, 0x04, 0x25, 0x3f]);
        assert_eq!(
            guid_string(&guid).unwrap(),
            "3f2504e0-4f89-11d3-9a0c-0305e82c3301"
        );
    }

    #[test]
    fn filters_are_ldap() {
        let table = Table {
            key: "uid".into(),
            columns: vec![
                ("uidNumber".into(), "BIGINT".into()),
                ("cn".into(), "VARCHAR".into()),
            ],
        };
        let f = |field: &str, op, value| {
            ldap_filter(
                &FieldFilter {
                    field: field.into(),
                    op,
                    value,
                },
                &table,
            )
        };
        assert_eq!(
            f("cn", FilterOp::Contains, json!("a*(b)")),
            Some(("(cn=*a\\2a\\28b\\29*)".into(), true))
        );
        assert_eq!(
            f("cn", FilterOp::NotStartsWith, json!("x")),
            Some(("(&(cn=*)(!(cn=x*)))".into(), true))
        );
        assert_eq!(
            f("cn", FilterOp::In, json!([])),
            Some((NOTHING.into(), true))
        );
        assert_eq!(
            f("uidNumber", FilterOp::Gt, json!(5)),
            Some(("(uidNumber>=5)".into(), false))
        );
        assert_eq!(f("cn", FilterOp::Gt, json!("m")), None);
        assert_eq!(f("dn", FilterOp::Eq, json!("x")), None);
    }

    #[test]
    fn thumbnails_are_small_jpegs() {
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbImage::new(1000, 600)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let out = thumbnail(png.get_ref()).unwrap();
        assert_eq!(image::guess_format(&out).unwrap(), image::ImageFormat::Jpeg);
        let small = image::load_from_memory(&out).unwrap();
        assert_eq!((small.width(), small.height()), (512, 307));
        assert!(thumbnail(b"not an image").is_err());
    }

    #[test]
    fn columns_read_by_type() {
        assert_eq!(convert("uidNumber", "BIGINT", &[b"42".to_vec()]), json!(42));
        assert_eq!(
            convert("mail", "JSON", &[b"a@x".to_vec(), b"b@x".to_vec()]),
            json!(["a@x", "b@x"])
        );
        assert_eq!(convert("mail", "JSON", &[]), json!([]));
        assert_eq!(
            convert("enabled", "BOOLEAN", &[b"TRUE".to_vec()]),
            json!(true)
        );
        assert_eq!(
            convert("jpegPhoto", "VARCHAR", &[vec![0xff, 0xd8]]),
            json!("/9g=")
        );
    }
}
