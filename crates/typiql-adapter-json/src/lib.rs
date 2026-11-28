use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use typiql_core::{FieldFilter, Location, TypiQLAdapter};
use uuid::Uuid;

/// File = schema (a namespace holding one or more named tables), top-level
/// JSON property = table. Most types share one file (`<data_dir>/data.json`,
/// `Location::Named(table)`); `Location::At { file, table }` reads/writes an
/// explicit, independent file — used for per-record-scoped storage (e.g. a
/// dashboard's own `.dashboard.json`, computed from a sibling field's value
/// rather than a fixed default location).
///
/// All data is cached in-memory per file (not per table — a file's tables
/// are read/written together, since they live in one JSON object on disk).
/// Writes go to disk synchronously on mutation.
type FileTables = HashMap<PathBuf, HashMap<String, Vec<Value>>>;

#[derive(Clone)]
pub struct JsonAdapter {
    data_dir: PathBuf,
    files: Arc<RwLock<FileTables>>,
    /// Tables whose key is an integer (from `register_type`): new rows get
    /// the next number rather than a UUID.
    integer_keys: Arc<RwLock<std::collections::HashSet<String>>>,
}

impl JsonAdapter {
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self {
            data_dir: data_dir.as_ref().to_path_buf(),
            files: Arc::new(RwLock::new(HashMap::new())),
            integer_keys: Arc::new(RwLock::new(Default::default())),
        }
    }

    fn shared_file_path(&self) -> PathBuf {
        self.data_dir.join("data.json")
    }

    /// Expand a leading `~` to the current user's home directory — needed
    /// for `Location::At` paths, which are built from user-facing path
    /// strings (e.g. a Dashboard's own `path` field) rather than the
    /// adapter's own `data_dir`.
    fn expand_tilde(path: &Path) -> PathBuf {
        let s = path.to_string_lossy();
        if let Some(rest) = s.strip_prefix("~/") {
            if let Some(home) = std::env::var_os("HOME") {
                return PathBuf::from(home).join(rest);
            }
        } else if s == "~" {
            if let Some(home) = std::env::var_os("HOME") {
                return PathBuf::from(home);
            }
        }
        path.to_path_buf()
    }

    fn resolve(&self, location: &Location) -> (PathBuf, String) {
        match location {
            Location::Named(table) => (self.shared_file_path(), table.clone()),
            Location::At { file, table } => (Self::expand_tilde(file), table.clone()),
        }
    }

    /// Loads one file's tables. A missing file is legitimately empty; a file
    /// that exists but can't be read or parsed is NOT — that used to fall
    /// through the same `unwrap_or_default()` and hand back an empty map,
    /// which is a silent total-data-loss bug: the caller (`modify_table`)
    /// immediately flushes that "empty" state back over the real file, so one
    /// unreadable byte turns the whole store into a single table.
    ///
    /// That is not hypothetical — it happened. A `data.json` torn by a
    /// non-atomic flush (see `flush_file_to_disk`) that was interrupted
    /// mid-write parsed as garbage on the next start; the app then rewrote
    /// the file with only the handful of tables it had touched since boot,
    /// dropping 18 others. So an unparseable file is now moved aside intact
    /// as `<name>.corrupt-<timestamp>` and loudly logged before this returns
    /// empty — the process still comes up, but the bytes are one `mv` away
    /// rather than gone.
    fn load_file_from_disk(path: &Path) -> HashMap<String, Vec<Value>> {
        if !path.exists() {
            return HashMap::new();
        }

        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) => {
                eprintln!("typiql-adapter-json: cannot read {}: {e}", path.display());
                Self::quarantine(path);
                return HashMap::new();
            }
        };

        // An empty file is the one benign case: `fs::write` of an empty map,
        // or a file created but never written. Nothing to salvage.
        if content.trim().is_empty() {
            return HashMap::new();
        }

        match serde_json::from_str(&content) {
            Ok(tables) => tables,
            Err(e) => {
                eprintln!(
                    "typiql-adapter-json: {} is not valid JSON ({e}) - quarantining it rather than \
                     treating it as empty, since the next write would otherwise overwrite it",
                    path.display()
                );
                Self::quarantine(path);
                HashMap::new()
            }
        }
    }

    /// Renames an unreadable data file out of the way so the next flush can't
    /// overwrite it. Best-effort: if even the rename fails there's nothing
    /// further to try, and the caller still needs to come up.
    fn quarantine(path: &Path) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut quarantined = path.as_os_str().to_os_string();
        quarantined.push(format!(".corrupt-{stamp}"));
        let quarantined = PathBuf::from(quarantined);

        match std::fs::rename(path, &quarantined) {
            Ok(()) => eprintln!(
                "typiql-adapter-json: preserved the unreadable file as {}",
                quarantined.display()
            ),
            Err(e) => eprintln!(
                "typiql-adapter-json: could not preserve {}: {e}",
                path.display()
            ),
        }
    }

    /// Writes one file's tables atomically: a full temp file in the same
    /// directory, fsynced, then `rename`d over the target. `fs::write`
    /// truncates the real file first and streams into it, so anything that
    /// interrupts it — a kill, a crash, a full disk — leaves a half-written
    /// file on disk where the whole store used to be. `rename` within a
    /// filesystem is atomic, so a reader either sees the entire old file or
    /// the entire new one, never a torn mixture of the two.
    ///
    /// The `sync_all` before the rename matters as much as the rename: it
    /// forces the temp file's contents out before the directory entry starts
    /// pointing at it, so a power loss can't leave the name resolving to
    /// unwritten blocks.
    fn flush_file_to_disk(path: &Path, data: &HashMap<String, Vec<Value>>) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let Ok(content) = serde_json::to_string_pretty(data) else {
            eprintln!(
                "typiql-adapter-json: refusing to write {} - its data could not be serialized",
                path.display()
            );
            return;
        };

        // Same directory, so the rename below stays within one filesystem.
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);

        let written = std::fs::File::create(&tmp).and_then(|mut file| {
            use std::io::Write;
            file.write_all(content.as_bytes())?;
            file.sync_all()
        });

        if let Err(e) = written {
            eprintln!(
                "typiql-adapter-json: could not write {}: {e}",
                tmp.display()
            );
            std::fs::remove_file(&tmp).ok();
            return;
        }

        if let Err(e) = std::fs::rename(&tmp, path) {
            eprintln!(
                "typiql-adapter-json: could not replace {}: {e}",
                path.display()
            );
            std::fs::remove_file(&tmp).ok();
        }
    }

    /// Read one table's rows, loading (and caching) its containing file from
    /// disk on first access.
    fn read_table(&self, path: &Path, table: &str) -> Vec<Value> {
        {
            let files = self.files.read();
            if let Some(tables) = files.get(path) {
                return tables.get(table).cloned().unwrap_or_default();
            }
        }
        // Only fill the cache if it's still empty: a writer may have cached
        // (and changed) this file since the read-lock miss above, and
        // replacing its entry with this older load would lose that write.
        let mut files = self.files.write();
        let tables = files
            .entry(path.to_path_buf())
            .or_insert_with(|| Self::load_file_from_disk(path));
        tables.get(table).cloned().unwrap_or_default()
    }

    /// Read-modify-write of one table, holding the write lock throughout —
    /// so concurrent writers queue instead of each writing back its own
    /// stale copy (which lost rows: 200 concurrent adds once left 38).
    /// Loads the containing file first if it isn't cached, so sibling tables
    /// in the same file (e.g. a dashboard's `dashboard` table when writing
    /// its `files` table) are never clobbered. `f` returns whether it changed
    /// anything; only then is the file flushed.
    fn modify_table<R>(
        &self,
        path: &Path,
        table: &str,
        f: impl FnOnce(&mut Vec<Value>) -> (bool, R),
    ) -> R {
        let mut files = self.files.write();
        let tables = files
            .entry(path.to_path_buf())
            .or_insert_with(|| Self::load_file_from_disk(path));
        let rows = tables.entry(table.to_string()).or_default();
        let (changed, result) = f(rows);
        if changed {
            Self::flush_file_to_disk(path, tables);
        }
        result
    }

    /// The row's key as text (a number's digits for an integer key), if it
    /// has one.
    fn key_of(row: &Value, key_field: &str) -> Option<String> {
        match row.get(key_field)? {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    }

    fn has_key(row: &Value, key_field: &str) -> bool {
        Self::key_of(row, key_field).is_some_and(|k| !k.is_empty())
    }

    fn matches_all(item: &Value, filters: &[FieldFilter]) -> bool {
        typiql_core::query::matches_all(item, filters)
    }
}

#[async_trait]
impl TypiQLAdapter for JsonAdapter {
    async fn get_one(&self, location: Location, key_field: &str, key_value: &str) -> Option<Value> {
        let (path, table) = self.resolve(&location);
        self.read_table(&path, &table)
            .into_iter()
            .find(|item| Self::key_of(item, key_field).as_deref() == Some(key_value))
    }

    async fn get_many(&self, location: Location, filters: Vec<FieldFilter>) -> Vec<Value> {
        let (path, table) = self.resolve(&location);
        let data = self.read_table(&path, &table);
        if filters.is_empty() {
            data
        } else {
            data.into_iter()
                .filter(|item| Self::matches_all(item, &filters))
                .collect()
        }
    }

    async fn get_many_matching_any(
        &self,
        location: Location,
        or_groups: Vec<Vec<FieldFilter>>,
    ) -> Vec<Value> {
        if or_groups.is_empty() {
            return vec![];
        }
        let (path, table) = self.resolve(&location);
        let data = self.read_table(&path, &table);
        data.into_iter()
            .filter(|item| or_groups.iter().any(|group| Self::matches_all(item, group)))
            .collect()
    }

    async fn add(
        &self,
        location: Location,
        key_field: &str,
        values: Value,
    ) -> Result<Value, String> {
        self.add_many(location, key_field, vec![values])
            .await
            .map(|mut added| added.remove(0))
    }

    /// All-or-nothing: a key already in the table, or repeated within
    /// `values`, rejects the whole batch — keys are unique, as Mongo's `_id`.
    async fn add_many(
        &self,
        location: Location,
        key_field: &str,
        mut values: Vec<Value>,
    ) -> Result<Vec<Value>, String> {
        let (path, table) = self.resolve(&location);
        let integer_key = self.integer_keys.read().contains(&table);
        self.modify_table(&path, &table, |rows| {
            // Missing keys: the next number(s) for an integer key — decided
            // under the write lock, so concurrent adds never collide — or a
            // UUID.
            let mut next = rows
                .iter()
                .filter_map(|r| r.get(key_field).and_then(Value::as_i64))
                .chain(
                    values
                        .iter()
                        .filter_map(|r| r.get(key_field).and_then(Value::as_i64)),
                )
                .max()
                .unwrap_or(0);
            for v in &mut values {
                if !Self::has_key(v, key_field) {
                    let key = if integer_key {
                        next += 1;
                        Value::from(next)
                    } else {
                        Value::String(Uuid::new_v4().to_string())
                    };
                    if let Value::Object(map) = v {
                        map.insert(key_field.to_string(), key);
                    }
                }
            }
            let mut keys: std::collections::HashSet<String> = rows
                .iter()
                .filter_map(|r| Self::key_of(r, key_field))
                .collect();
            for v in &values {
                let key = Self::key_of(v, key_field).unwrap_or_default();
                if !keys.insert(key.clone()) {
                    return (false, Err(format!("Duplicate key `{key}`")));
                }
            }
            rows.extend(values.iter().cloned());
            (true, Ok(values.clone()))
        })
    }

    async fn update(
        &self,
        location: Location,
        key_field: &str,
        key_value: &str,
        patch: Value,
    ) -> Option<Value> {
        let (path, table) = self.resolve(&location);
        self.modify_table(&path, &table, |rows| {
            let Some(Value::Object(item)) = rows
                .iter_mut()
                .find(|r| Self::key_of(r, key_field).as_deref() == Some(key_value))
            else {
                return (false, None);
            };
            if let Value::Object(patch) = &patch {
                for (k, v) in patch {
                    if k != key_field {
                        item.insert(k.clone(), v.clone());
                    }
                }
            }
            (true, Some(Value::Object(item.clone())))
        })
    }

    async fn remove(&self, location: Location, key_field: &str, key_value: &str) -> Option<Value> {
        let (path, table) = self.resolve(&location);
        self.modify_table(&path, &table, |rows| {
            match rows
                .iter()
                .position(|r| Self::key_of(r, key_field).as_deref() == Some(key_value))
            {
                Some(i) => (true, Some(rows.remove(i))),
                None => (false, None),
            }
        })
    }

    /// One locked write for the whole batch, rather than a file rewrite per
    /// row.
    async fn update_many(
        &self,
        location: Location,
        key_field: &str,
        keys: Vec<String>,
        patch: Value,
    ) -> Vec<Value> {
        let (path, table) = self.resolve(&location);
        let keys: std::collections::HashSet<String> = keys.into_iter().collect();
        self.modify_table(&path, &table, |rows| {
            let mut updated = Vec::new();
            for row in rows.iter_mut() {
                if !Self::key_of(row, key_field).is_some_and(|k| keys.contains(&k)) {
                    continue;
                }
                if let (Value::Object(item), Value::Object(patch)) = (&mut *row, &patch) {
                    for (k, v) in patch {
                        if k != key_field {
                            item.insert(k.clone(), v.clone());
                        }
                    }
                }
                updated.push(row.clone());
            }
            (!updated.is_empty(), updated)
        })
    }

    async fn remove_many(
        &self,
        location: Location,
        key_field: &str,
        keys: Vec<String>,
    ) -> Vec<Value> {
        let (path, table) = self.resolve(&location);
        let keys: std::collections::HashSet<String> = keys.into_iter().collect();
        self.modify_table(&path, &table, |rows| {
            let (removed, kept): (Vec<Value>, Vec<Value>) = std::mem::take(rows)
                .into_iter()
                .partition(|row| Self::key_of(row, key_field).is_some_and(|k| keys.contains(&k)));
            *rows = kept;
            (!removed.is_empty(), removed)
        })
    }

    fn register_type(
        &self,
        table: &str,
        key_field: &str,
        columns: &[(&'static str, &'static str)],
    ) {
        let integer = columns
            .iter()
            .find(|(name, _)| *name == key_field)
            .is_some_and(|(_, ty)| is_integer_type(ty));
        if integer {
            self.integer_keys.write().insert(table.to_string());
        }
    }

    async fn set_table(&self, location: Location, rows: Vec<Value>) {
        let (path, table) = self.resolve(&location);
        self.modify_table(&path, &table, |existing| {
            *existing = rows;
            (true, ())
        });
    }
}

/// Whether a column type marker (see `TypiQLType::duckdb_columns`) is an
/// integer.
fn is_integer_type(ty: &str) -> bool {
    let base = ty.strip_suffix(" NOT NULL").unwrap_or(ty);
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

#[cfg(test)]
mod persistence_tests {
    use super::*;
    use serde_json::json;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("typiql-json-adapter-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tables(pairs: &[(&str, Value)]) -> HashMap<String, Vec<Value>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), vec![v.clone()]))
            .collect()
    }

    /// The regression this whole pair of functions exists for: an unreadable
    /// file must never be handed back as "empty", because the very next
    /// write flushes that emptiness over the real data.
    #[test]
    fn unparseable_file_is_quarantined_rather_than_treated_as_empty() {
        let dir = scratch("corrupt");
        let path = dir.join("data.json");
        std::fs::write(&path, "{\"cars\": [{\"id\": \"1\"}], \"dash").unwrap(); // torn mid-write

        let loaded = JsonAdapter::load_file_from_disk(&path);
        assert!(
            loaded.is_empty(),
            "a corrupt file yields no tables to serve"
        );

        // ...but the bytes still exist under a .corrupt-* name, and the
        // original path is now free for a clean write instead of being
        // overwritten in place.
        assert!(!path.exists(), "the corrupt file was moved out of the way");
        let preserved: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .collect();
        assert_eq!(preserved.len(), 1, "exactly one quarantined copy");
        assert!(std::fs::read_to_string(preserved[0].path())
            .unwrap()
            .contains("cars"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_and_empty_files_are_legitimately_empty() {
        let dir = scratch("empty");
        let missing = dir.join("nope.json");
        assert!(JsonAdapter::load_file_from_disk(&missing).is_empty());
        assert!(!dir.join("nope.json.corrupt").exists());

        let blank = dir.join("blank.json");
        std::fs::write(&blank, "   \n").unwrap();
        assert!(JsonAdapter::load_file_from_disk(&blank).is_empty());
        assert!(
            blank.exists(),
            "a blank file is not corrupt, don't quarantine it"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn flush_round_trips_every_table_and_leaves_no_temp_file() {
        let dir = scratch("flush");
        let path = dir.join("data.json");
        let data = tables(&[
            ("cars", json!({"id": "1"})),
            ("dashboards", json!({"id": "2"})),
        ]);

        JsonAdapter::flush_file_to_disk(&path, &data);

        let reloaded = JsonAdapter::load_file_from_disk(&path);
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded["cars"][0]["id"], "1");
        assert_eq!(reloaded["dashboards"][0]["id"], "2");
        assert!(
            !dir.join("data.json.tmp").exists(),
            "temp file was renamed, not left behind"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `modify_table` loads siblings before flushing, so writing one table
    /// must not drop the others sharing the file.
    #[test]
    fn writing_one_table_preserves_its_siblings() {
        let dir = scratch("siblings");
        let path = dir.join("data.json");
        JsonAdapter::flush_file_to_disk(
            &path,
            &tables(&[
                ("cars", json!({"id": "1"})),
                ("dashboards", json!({"id": "2"})),
            ]),
        );

        let adapter = JsonAdapter::new(&dir);
        adapter.modify_table(&path, "cars", |rows| {
            *rows = vec![json!({"id": "99"})];
            (true, ())
        });

        let reloaded = JsonAdapter::load_file_from_disk(&path);
        assert_eq!(reloaded["cars"][0]["id"], "99", "the written table updated");
        assert_eq!(
            reloaded["dashboards"][0]["id"], "2",
            "the untouched table survived"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
