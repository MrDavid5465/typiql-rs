use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use parking_lot::RwLock;
use serde_json::Value;
use async_trait::async_trait;
use uuid::Uuid;
use typiql_core::{FieldFilter, FilterOp, Location, TypiQLAdapter};

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
#[derive(Clone)]
pub struct JsonAdapter {
    data_dir: PathBuf,
    files: Arc<RwLock<HashMap<PathBuf, HashMap<String, Vec<Value>>>>>,
}

impl JsonAdapter {
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self {
            data_dir: data_dir.as_ref().to_path_buf(),
            files: Arc::new(RwLock::new(HashMap::new())),
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

    fn load_file_from_disk(path: &Path) -> HashMap<String, Vec<Value>> {
        if path.exists() {
            let content = std::fs::read_to_string(path).unwrap_or_default();
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            HashMap::new()
        }
    }

    fn flush_file_to_disk(path: &Path, data: &HashMap<String, Vec<Value>>) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let content = serde_json::to_string_pretty(data).unwrap_or_default();
        std::fs::write(path, content).ok();
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
        let loaded = Self::load_file_from_disk(path);
        let result = loaded.get(table).cloned().unwrap_or_default();
        self.files.write().insert(path.to_path_buf(), loaded);
        result
    }

    /// Write one table's rows back. Loads the containing file from disk
    /// first if it isn't already cached, so sibling tables already stored in
    /// the same file (e.g. a dashboard's `dashboard` table when writing its
    /// `files` table) are never clobbered.
    fn write_table(&self, path: &Path, table: &str, data: Vec<Value>) {
        let mut files = self.files.write();
        if !files.contains_key(path) {
            let loaded = Self::load_file_from_disk(path);
            files.insert(path.to_path_buf(), loaded);
        }
        let entry = files.get_mut(path).unwrap();
        entry.insert(table.to_string(), data);
        Self::flush_file_to_disk(path, entry);
    }

    /// Ordinal comparison across the two JSON scalar shapes that support it —
    /// numbers (compared as f64) and strings (lexicographic). Anything else
    /// (bool, null, object, array, or a type mismatch) has no ordering.
    fn compare_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
        match (a, b) {
            (Value::Number(a), Value::Number(b)) => a.as_f64()?.partial_cmp(&b.as_f64()?),
            (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }

    fn matches_filter(item: &Value, filter: &FieldFilter) -> bool {
        let item_val = item.get(&filter.field);
        match filter.op {
            FilterOp::Eq => item_val == Some(&filter.value),
            FilterOp::Ne => item_val != Some(&filter.value),
            FilterOp::Gt => item_val
                .and_then(|v| Self::compare_values(v, &filter.value))
                == Some(std::cmp::Ordering::Greater),
            FilterOp::Gte => matches!(
                item_val.and_then(|v| Self::compare_values(v, &filter.value)),
                Some(std::cmp::Ordering::Greater) | Some(std::cmp::Ordering::Equal)
            ),
            FilterOp::Lt => item_val
                .and_then(|v| Self::compare_values(v, &filter.value))
                == Some(std::cmp::Ordering::Less),
            FilterOp::Lte => matches!(
                item_val.and_then(|v| Self::compare_values(v, &filter.value)),
                Some(std::cmp::Ordering::Less) | Some(std::cmp::Ordering::Equal)
            ),
            FilterOp::In => filter
                .value
                .as_array()
                .map(|arr| item_val.map(|v| arr.contains(v)).unwrap_or(false))
                .unwrap_or(false),
            FilterOp::NotIn => filter
                .value
                .as_array()
                .map(|arr| item_val.map(|v| !arr.contains(v)).unwrap_or(true))
                .unwrap_or(true),
        }
    }

    fn matches_all(item: &Value, filters: &[FieldFilter]) -> bool {
        filters.iter().all(|f| Self::matches_filter(item, f))
    }
}

#[async_trait]
impl TypiQLAdapter for JsonAdapter {
    async fn get_one(
        &self,
        location: Location,
        key_field: &str,
        key_value: &str,
    ) -> Option<Value> {
        let (path, table) = self.resolve(&location);
        self.read_table(&path, &table).into_iter().find(|item| {
            item.get(key_field)
                .and_then(Value::as_str)
                .map(|s| s == key_value)
                .unwrap_or(false)
        })
    }

    async fn get_many(
        &self,
        location: Location,
        filters: Vec<FieldFilter>,
    ) -> Vec<Value> {
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
        mut values: Value,
    ) -> Result<Value, String> {
        let (path, table) = self.resolve(&location);
        let mut data = self.read_table(&path, &table);

        // Auto-assign UUID if the key is absent or empty.
        let needs_id = values
            .get(key_field)
            .and_then(Value::as_str)
            .map(str::is_empty)
            .unwrap_or(true);

        if needs_id {
            if let Value::Object(ref mut map) = values {
                map.insert(
                    key_field.to_string(),
                    Value::String(Uuid::new_v4().to_string()),
                );
            }
        }

        data.push(values.clone());
        self.write_table(&path, &table, data);
        Ok(values)
    }

    async fn update(
        &self,
        location: Location,
        key_field: &str,
        key_value: &str,
        patch: Value,
    ) -> Option<Value> {
        let (path, table) = self.resolve(&location);
        let mut data = self.read_table(&path, &table);
        let mut updated: Option<Value> = None;

        for item in &mut data {
            let is_match = item
                .get(key_field)
                .and_then(Value::as_str)
                .map(|s| s == key_value)
                .unwrap_or(false);

            if is_match {
                if let (Value::Object(item_map), Value::Object(patch_map)) =
                    (item, &patch)
                {
                    for (k, v) in patch_map {
                        if k != key_field {
                            item_map.insert(k.clone(), v.clone());
                        }
                    }
                    updated = Some(Value::Object(item_map.clone()));
                }
                break;
            }
        }

        if updated.is_some() {
            self.write_table(&path, &table, data);
        }
        updated
    }

    async fn remove(
        &self,
        location: Location,
        key_field: &str,
        key_value: &str,
    ) -> Option<Value> {
        let (path, table) = self.resolve(&location);
        let data = self.read_table(&path, &table);
        let mut removed: Option<Value> = None;

        let remaining: Vec<Value> = data
            .into_iter()
            .filter(|item| {
                let is_match = item
                    .get(key_field)
                    .and_then(Value::as_str)
                    .map(|s| s == key_value)
                    .unwrap_or(false);
                if is_match {
                    removed = Some(item.clone());
                }
                !is_match
            })
            .collect();

        if removed.is_some() {
            self.write_table(&path, &table, remaining);
        }
        removed
    }

    async fn set_table(&self, location: Location, rows: Vec<Value>) {
        let (path, table) = self.resolve(&location);
        self.write_table(&path, &table, rows);
    }
}
