//! Read-only, fail-closed loading for an offline personal-data export.
//!
//! Normal app startup deliberately repairs or defaults damaged stores so playback can continue.
//! An export has a different contract: a present store that cannot be read completely must abort
//! the backup, while a genuinely absent store is a valid first-run default.

use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::Error as _;
use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use sha2::{Digest, Sha256};

use super::ExportError;
use crate::config::Config;
use crate::library::Library;
use crate::playlists::Playlists;
use crate::signals::Signals;
use crate::station::StationStore;

// Keep these aligned with the owning stores. They are repeated here rather than widening the
// production persistence API solely for an export-only reader.
const CONFIG_MAX_BYTES: u64 = 1024 * 1024;
const LIBRARY_MAX_BYTES: u64 = 50 * 1024 * 1024;
const PLAYLISTS_MAX_BYTES: u64 = 50 * 1024 * 1024;
const SIGNALS_MAX_BYTES: u64 = 32 * 1024 * 1024;
const STATION_MAX_BYTES: u64 = 16 * 1024 * 1024;
const INTENT_JOURNAL_MAX_BYTES: u64 = 1024 * 1024;
const INTENT_SNAPSHOT_MAX_BYTES: u64 = 64 * 1024 * 1024;

// Raw byte limits do not bound serde allocation: a small JSON token such as `""` can become a
// 24-byte String, and an array of empty objects can materialize large Rust structs. Walk the JSON
// once without building a value tree and reject shapes whose conservative heap estimate exceeds
// the same aggregate budget used for a live-owner clone.
const JSON_MAX_DEPTH: usize = 64;
const JSON_CONTAINER_LIMIT: usize = 200_000;
const JSON_ENTRY_LIMIT: usize = 1_000_000;
const JSON_CONTAINER_COST: usize = 64;
const JSON_ENTRY_COST: usize = 16;
const JSON_STRING_COST: usize = std::mem::size_of::<String>();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreProfile {
    Config,
    Generic,
    Library,
    Playlists,
    Signals,
    Station,
}

impl StoreProfile {
    fn for_kind(kind: &str) -> Self {
        match kind {
            "config" => Self::Config,
            "library" => Self::Library,
            "playlists" => Self::Playlists,
            "signals" => Self::Signals,
            "station profile" => Self::Station,
            _ => Self::Generic,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueContext {
    Generic,
    SongArray,
    PlaylistSongs,
    DynamicKeyMap,
    NestedTextArray,
    NestedTextValue,
}

#[derive(Debug, Default)]
struct StructuralBudget {
    estimated_heap: usize,
    containers: usize,
    entries: usize,
    nested_text_items: usize,
    text_bytes: usize,
}

impl StructuralBudget {
    fn charge(&mut self, bytes: usize) -> Result<(), &'static str> {
        self.estimated_heap = self
            .estimated_heap
            .checked_add(bytes)
            .ok_or("estimated allocation size overflowed")?;
        if self.estimated_heap > super::live::CLONE_BUDGET_BYTES {
            return Err("estimated decoded allocation exceeds the 64 MiB safety budget");
        }
        Ok(())
    }

    fn container(&mut self) -> Result<(), &'static str> {
        self.containers = self
            .containers
            .checked_add(1)
            .ok_or("container count overflowed")?;
        if self.containers > JSON_CONTAINER_LIMIT {
            return Err("JSON contains too many nested containers");
        }
        self.charge(JSON_CONTAINER_COST)
    }

    fn entry(&mut self) -> Result<(), &'static str> {
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or("entry count overflowed")?;
        if self.entries > JSON_ENTRY_LIMIT {
            return Err("JSON contains too many array or object entries");
        }
        self.charge(JSON_ENTRY_COST)
    }

    fn text(&mut self, bytes: usize) -> Result<(), &'static str> {
        self.text_bytes = self
            .text_bytes
            .checked_add(bytes)
            .ok_or("decoded text size overflowed")?;
        if self.text_bytes > super::live::CLONE_BUDGET_BYTES {
            return Err("decoded text exceeds the 64 MiB safety budget");
        }
        self.charge(
            JSON_STRING_COST
                .checked_add(bytes)
                .ok_or("decoded string allocation overflowed")?,
        )
    }

    fn nested_text(&mut self, bytes: usize) -> Result<(), &'static str> {
        self.nested_text_items = self
            .nested_text_items
            .checked_add(1)
            .ok_or("text item count overflowed")?;
        if self.nested_text_items > super::live::NESTED_TEXT_ITEMS_LIMIT {
            return Err("decoded data contains too many dynamic text items");
        }
        self.text(bytes)
    }

    fn song(&mut self) -> Result<(), &'static str> {
        self.charge(std::mem::size_of::<crate::api::Song>())
    }
}

#[derive(Debug, Default)]
struct AggregateBudget {
    estimated_heap: usize,
}

impl AggregateBudget {
    fn add(&mut self, store: &'static str, estimate: usize) -> Result<(), ExportError> {
        self.estimated_heap = self.estimated_heap.checked_add(estimate).ok_or_else(|| {
            source_error(store, "combined decoded allocation estimate overflowed")
        })?;
        if self.estimated_heap > super::live::CLONE_BUDGET_BYTES {
            return Err(source_error(
                store,
                "combined decoded stores exceed the 64 MiB safety budget",
            ));
        }
        Ok(())
    }
}

struct JsonSeed<'a> {
    budget: &'a mut StructuralBudget,
    profile: StoreProfile,
    context: ValueContext,
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for JsonSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if self.depth > JSON_MAX_DEPTH {
            return Err(de::Error::custom(
                "JSON nesting exceeds the 64-level safety limit",
            ));
        }
        deserializer.deserialize_any(JsonVisitor {
            budget: self.budget,
            profile: self.profile,
            context: self.context,
            depth: self.depth,
        })
    }
}

struct JsonVisitor<'a> {
    budget: &'a mut StructuralBudget,
    profile: StoreProfile,
    context: ValueContext,
    depth: usize,
}

impl JsonVisitor<'_> {
    fn child_depth<E: de::Error>(&self) -> Result<usize, E> {
        self.depth
            .checked_add(1)
            .ok_or_else(|| E::custom("JSON nesting depth overflowed"))
    }

    fn text<E: de::Error>(&mut self, bytes: usize) -> Result<(), E> {
        if self.context == ValueContext::NestedTextValue {
            self.budget.nested_text(bytes).map_err(E::custom)
        } else {
            self.budget.text(bytes).map_err(E::custom)
        }
    }
}

impl<'de> Visitor<'de> for JsonVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("bounded personal-data store JSON")
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_char<E>(mut self, value: char) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.text(value.len_utf8())
    }

    fn visit_borrowed_str<E>(mut self, value: &'de str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.text(value.len())
    }

    fn visit_str<E>(mut self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.text(value.len())
    }

    fn visit_string<E>(mut self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.text(value.len())
    }

    fn visit_borrowed_bytes<E>(mut self, value: &'de [u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.text(value.len())
    }

    fn visit_bytes<E>(mut self, value: &[u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.text(value.len())
    }

    fn visit_byte_buf<E>(mut self, value: Vec<u8>) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.text(value.len())
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        JsonSeed {
            budget: self.budget,
            profile: self.profile,
            context: self.context,
            depth: self.depth,
        }
        .deserialize(deserializer)
    }

    fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        self.visit_some(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        self.budget.container().map_err(A::Error::custom)?;
        let child_depth = self.child_depth::<A::Error>()?;
        let child_context = if self.context == ValueContext::NestedTextArray {
            ValueContext::NestedTextValue
        } else {
            ValueContext::Generic
        };
        while sequence
            .next_element_seed(JsonSeed {
                budget: self.budget,
                profile: self.profile,
                context: child_context,
                depth: child_depth,
            })?
            .is_some()
        {
            self.budget.entry().map_err(A::Error::custom)?;
            match self.context {
                ValueContext::SongArray => {
                    self.budget.song().map_err(A::Error::custom)?;
                }
                ValueContext::PlaylistSongs => {
                    self.budget.song().map_err(A::Error::custom)?;
                }
                ValueContext::Generic
                | ValueContext::DynamicKeyMap
                | ValueContext::NestedTextArray
                | ValueContext::NestedTextValue => {}
            }
        }
        Ok(())
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        self.budget.container().map_err(A::Error::custom)?;
        let child_depth = self.child_depth::<A::Error>()?;
        while let Some(context) = map.next_key_seed(JsonKeySeed {
            budget: self.budget,
            profile: self.profile,
            parent: self.context,
        })? {
            self.budget.entry().map_err(A::Error::custom)?;
            map.next_value_seed(JsonSeed {
                budget: self.budget,
                profile: self.profile,
                context,
                depth: child_depth,
            })?;
        }
        Ok(())
    }
}

struct JsonKeySeed<'a> {
    budget: &'a mut StructuralBudget,
    profile: StoreProfile,
    parent: ValueContext,
}

impl<'de> DeserializeSeed<'de> for JsonKeySeed<'_> {
    type Value = ValueContext;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(JsonKeyVisitor {
            budget: self.budget,
            profile: self.profile,
            parent: self.parent,
        })
    }
}

struct JsonKeyVisitor<'a> {
    budget: &'a mut StructuralBudget,
    profile: StoreProfile,
    parent: ValueContext,
}

impl JsonKeyVisitor<'_> {
    fn classify<E: de::Error>(self, key: &str) -> Result<ValueContext, E> {
        if self.parent == ValueContext::DynamicKeyMap {
            self.budget.nested_text(key.len()).map_err(E::custom)?;
            return Ok(ValueContext::Generic);
        }
        Ok(match (self.profile, key) {
            (StoreProfile::Config, "keybindings" | "mouse_bindings") => ValueContext::DynamicKeyMap,
            (StoreProfile::Library, "favorites" | "history" | "radio_favorites" | "radios") => {
                ValueContext::SongArray
            }
            (StoreProfile::Playlists, "songs") => ValueContext::PlaylistSongs,
            (StoreProfile::Library | StoreProfile::Playlists, "artists" | "album_artists") => {
                ValueContext::NestedTextArray
            }
            (StoreProfile::Signals, "tracks" | "artist_weight") => ValueContext::DynamicKeyMap,
            (StoreProfile::Station, "avoid_artist_keys") => ValueContext::NestedTextArray,
            _ => ValueContext::Generic,
        })
    }
}

impl<'de> Visitor<'de> for JsonKeyVisitor<'_> {
    type Value = ValueContext;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON object key")
    }

    fn visit_borrowed_str<E>(self, key: &'de str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.classify(key)
    }

    fn visit_str<E>(self, key: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.classify(key)
    }

    fn visit_string<E>(self, key: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.classify(&key)
    }
}

fn preflight_store_json(
    store: &'static str,
    component: &str,
    bytes: &[u8],
    profile: StoreProfile,
) -> Result<usize, ExportError> {
    let mut budget = StructuralBudget::default();
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    JsonSeed {
        budget: &mut budget,
        profile,
        context: ValueContext::Generic,
        depth: 0,
    }
    .deserialize(&mut deserializer)
    .map_err(|error| {
        source_error(
            store,
            format!("{component} is invalid or exceeds structural safety limits: {error}"),
        )
    })?;
    deserializer.end().map_err(|error| {
        source_error(
            store,
            format!("{component} has trailing or invalid JSON: {error}"),
        )
    })?;
    Ok(budget.estimated_heap)
}

pub(crate) struct OfflineSources {
    pub config: Config,
    pub library: Library,
    pub playlists: Playlists,
    pub signals: Signals,
    pub station: StationStore,
}

struct SourcePaths {
    config: Option<PathBuf>,
    library: Option<PathBuf>,
    playlists: Option<PathBuf>,
    signals: Option<PathBuf>,
    station: Option<PathBuf>,
}

impl SourcePaths {
    fn resolved() -> Self {
        Self {
            config: crate::config::config_path(),
            library: crate::library::library_path(),
            playlists: crate::playlists::playlists_path(),
            signals: crate::signals::signals_path(),
            station: crate::station::station_path(),
        }
    }
}

pub(crate) fn load_sources() -> Result<OfflineSources, ExportError> {
    load_sources_at(SourcePaths::resolved())
}

/// Load only the daemon-owned playlist store without invoking its recovery/defaulting loader.
pub(crate) fn load_playlists_read_only(
    live_estimated_bytes: usize,
) -> Result<Playlists, ExportError> {
    let path = crate::playlists::playlists_path();
    let path = required_path("playlists", path.as_deref())?;
    load_playlists_at_with_prior_budget(path, live_estimated_bytes)
}

#[cfg(test)]
fn load_playlists_at(path: &Path) -> Result<Playlists, ExportError> {
    load_store("playlists", path, PLAYLISTS_MAX_BYTES, "playlists")
}

fn load_playlists_at_with_prior_budget(
    path: &Path,
    prior_estimate: usize,
) -> Result<Playlists, ExportError> {
    load_store_parsed(
        "playlists",
        path,
        PLAYLISTS_MAX_BYTES,
        "playlists",
        prior_estimate,
    )
    .map(|loaded| loaded.value)
}

fn load_sources_at(paths: SourcePaths) -> Result<OfflineSources, ExportError> {
    let mut budget = AggregateBudget::default();
    Ok(OfflineSources {
        config: load_store_budgeted(
            "config",
            required_path("config", paths.config.as_deref())?,
            CONFIG_MAX_BYTES,
            "config",
            &mut budget,
        )?,
        library: load_store_budgeted(
            "library",
            required_path("library", paths.library.as_deref())?,
            LIBRARY_MAX_BYTES,
            "library",
            &mut budget,
        )?,
        playlists: load_store_budgeted(
            "playlists",
            required_path("playlists", paths.playlists.as_deref())?,
            PLAYLISTS_MAX_BYTES,
            "playlists",
            &mut budget,
        )?,
        signals: load_store_budgeted(
            "signals",
            required_path("signals", paths.signals.as_deref())?,
            SIGNALS_MAX_BYTES,
            "signals",
            &mut budget,
        )?,
        station: load_store_budgeted(
            "station profile",
            required_path("station profile", paths.station.as_deref())?,
            STATION_MAX_BYTES,
            "station profile",
            &mut budget,
        )?,
    })
}

struct BaseStore<T> {
    value: T,
    bytes: Option<Vec<u8>>,
    estimate: usize,
}

struct ParsedStore<T> {
    value: T,
    estimate: usize,
}

#[cfg(test)]
fn load_store<T>(
    store: &'static str,
    path: &Path,
    max_bytes: u64,
    journal_kind: &'static str,
) -> Result<T, ExportError>
where
    T: DeserializeOwned + Default,
{
    load_store_parsed(store, path, max_bytes, journal_kind, 0).map(|loaded| loaded.value)
}

fn load_store_budgeted<T>(
    store: &'static str,
    path: &Path,
    max_bytes: u64,
    journal_kind: &'static str,
    budget: &mut AggregateBudget,
) -> Result<T, ExportError>
where
    T: DeserializeOwned + Default,
{
    let loaded = load_store_parsed(store, path, max_bytes, journal_kind, budget.estimated_heap)?;
    budget.add(store, loaded.estimate)?;
    Ok(loaded.value)
}

fn load_store_parsed<T>(
    store: &'static str,
    path: &Path,
    max_bytes: u64,
    journal_kind: &'static str,
    prior_estimate: usize,
) -> Result<ParsedStore<T>, ExportError>
where
    T: DeserializeOwned + Default,
{
    let profile = StoreProfile::for_kind(journal_kind);
    let base_bytes = read_optional(store, "snapshot", path, max_bytes)?;
    let base = match base_bytes {
        Some(bytes) => {
            let parsed = parse_store(store, "snapshot", &bytes, profile, prior_estimate)?;
            BaseStore {
                value: parsed.value,
                bytes: Some(bytes),
                estimate: parsed.estimate,
            }
        }
        None => {
            let estimate = std::mem::size_of::<T>();
            ensure_combined_budget(store, prior_estimate, estimate)?;
            let value = T::default();
            BaseStore {
                estimate,
                value,
                bytes: None,
            }
        }
    };

    let journal_path = sibling_with_suffix(path, ".intent.jsonl", store)?;
    let sidecar_path = sibling_with_suffix(path, ".intent.latest.json", store)?;
    let journal = read_optional(
        store,
        "persistence journal",
        &journal_path,
        INTENT_JOURNAL_MAX_BYTES,
    )?;
    let sidecar = read_optional(
        store,
        "persistence journal sidecar",
        &sidecar_path,
        max_bytes.min(INTENT_SNAPSHOT_MAX_BYTES),
    )?;

    let Some(journal) = journal else {
        return match sidecar {
            None => Ok(ParsedStore {
                value: base.value,
                estimate: base.estimate,
            }),
            Some(sidecar) if base.bytes.as_deref() == Some(sidecar.as_slice()) => {
                // A crash while clearing a fully persisted intent can leave only the sidecar.
                // Byte equality proves it contains no state newer than the already validated main
                // snapshot, so parsing a second full T would only increase peak memory.
                Ok(ParsedStore {
                    value: base.value,
                    estimate: base.estimate,
                })
            }
            Some(_) => Err(source_error(
                store,
                "an orphan persistence sidecar may contain newer data",
            )),
        };
    };

    let record = parse_latest_intent(store, journal_kind, path, &journal)?;
    match sidecar {
        Some(sidecar) => {
            if sha256_hex(&sidecar) != record.sha256 {
                return Err(source_error(
                    store,
                    "persistence journal sidecar checksum does not match its latest intent",
                ));
            }
            // The present base was validated above to retain fail-closed corruption semantics, but
            // the journal makes this sidecar authoritative. Release the base T before decoding its
            // replacement so both materialized stores are never live together.
            drop(base);
            parse_store(
                store,
                "persistence journal sidecar",
                &sidecar,
                profile,
                prior_estimate,
            )
        }
        None => {
            let Some(base_bytes) = base.bytes.as_deref() else {
                return Err(source_error(
                    store,
                    "persistence journal exists but its snapshot and sidecar are missing",
                ));
            };
            if sha256_hex(base_bytes) != record.sha256 {
                return Err(source_error(
                    store,
                    "persistence journal sidecar is missing before its data reached the main snapshot",
                ));
            }
            // Main data already matches the intent; the process likely stopped while clearing
            // journal files after a successful atomic store write.
            Ok(ParsedStore {
                value: base.value,
                estimate: base.estimate,
            })
        }
    }
}

fn required_path<'a>(store: &'static str, path: Option<&'a Path>) -> Result<&'a Path, ExportError> {
    path.ok_or_else(|| source_error(store, "storage location cannot be resolved"))
}

#[derive(Deserialize)]
struct IntentRecord {
    v: u64,
    op: String,
    kind: String,
    sidecar: String,
    sha256: String,
}

fn parse_latest_intent(
    store: &'static str,
    expected_kind: &str,
    snapshot_path: &Path,
    bytes: &[u8],
) -> Result<IntentRecord, ExportError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| source_error(store, "persistence journal is not valid UTF-8 JSON Lines"))?;
    let expected_sidecar = sibling_file_name(snapshot_path, ".intent.latest.json", store)?;
    let mut latest = None;
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            return Err(source_error(
                store,
                format!(
                    "persistence journal contains an empty record at line {}",
                    index + 1
                ),
            ));
        }
        let record: IntentRecord = serde_json::from_str(line).map_err(|error| {
            source_error(
                store,
                format!(
                    "persistence journal contains invalid JSON at line {}: {error}",
                    index + 1
                ),
            )
        })?;
        if record.v != 1
            || record.op != "replace"
            || record.kind != expected_kind
            || record.sidecar != expected_sidecar
            || record.sha256.len() != 64
            || !record
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(source_error(
                store,
                format!(
                    "persistence journal record {} has an unsafe or unsupported shape",
                    index + 1
                ),
            ));
        }
        latest = Some(record);
    }
    latest.ok_or_else(|| source_error(store, "persistence journal is empty"))
}

fn read_optional(
    store: &'static str,
    component: &str,
    path: &Path,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>, ExportError> {
    match crate::util::safe_fs::read_no_symlink_limited(path, max_bytes) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(source_error(
            store,
            format!(
                "cannot safely read {component} `{}`: {error}",
                path.display()
            ),
        )),
    }
}

fn parse_store<T>(
    store: &'static str,
    component: &str,
    bytes: &[u8],
    profile: StoreProfile,
    prior_estimate: usize,
) -> Result<ParsedStore<T>, ExportError>
where
    T: DeserializeOwned,
{
    let estimate = preflight_store_json(store, component, bytes, profile)?;
    ensure_combined_budget(store, prior_estimate, estimate)?;
    let value = serde_json::from_slice(bytes).map_err(|error| {
        source_error(
            store,
            format!("{component} is not valid current yututui JSON: {error}"),
        )
    })?;
    Ok(ParsedStore { value, estimate })
}

fn ensure_combined_budget(
    store: &'static str,
    prior_estimate: usize,
    estimate: usize,
) -> Result<(), ExportError> {
    let mut budget = AggregateBudget {
        estimated_heap: prior_estimate,
    };
    budget.add(store, estimate)
}

fn sibling_with_suffix(
    path: &Path,
    suffix: &str,
    store: &'static str,
) -> Result<PathBuf, ExportError> {
    let name = sibling_file_name(path, suffix, store)?;
    Ok(path.with_file_name(name))
}

fn sibling_file_name(
    path: &Path,
    suffix: &str,
    store: &'static str,
) -> Result<String, ExportError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| source_error(store, "snapshot path does not have a safe UTF-8 file name"))?;
    Ok(format!("{name}{suffix}"))
}

fn source_error(store: &'static str, detail: impl Into<String>) -> ExportError {
    ExportError::SourceStore {
        store,
        detail: detail.into(),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::api::Song;

    static COUNTED_STORE_DESERIALIZATIONS: AtomicUsize = AtomicUsize::new(0);

    #[derive(Default)]
    struct CountedStore;

    impl<'de> serde::Deserialize<'de> for CountedStore {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            COUNTED_STORE_DESERIALIZATIONS.fetch_add(1, Ordering::SeqCst);
            <serde::de::IgnoredAny as serde::Deserialize>::deserialize(deserializer)?;
            Ok(Self)
        }
    }

    fn test_directory(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "yututui-offline-export-{label}-{}-{}",
            std::process::id(),
            super::super::random_suffix().expect("random suffix")
        ));
        fs::create_dir(&path).expect("create test directory");
        path
    }

    fn paths_under(root: &Path) -> SourcePaths {
        SourcePaths {
            config: Some(root.join("config.json")),
            library: Some(root.join("library.json")),
            playlists: Some(root.join("playlists.json")),
            signals: Some(root.join("signals.json")),
            station: Some(root.join("station.json")),
        }
    }

    fn expect_source_error<T>(result: Result<T, ExportError>, message: &str) -> ExportError {
        match result {
            Ok(_) => panic!("{message}"),
            Err(error) => error,
        }
    }

    fn json_array(value: &str, count: usize) -> Vec<u8> {
        let mut json = String::with_capacity(
            value
                .len()
                .saturating_add(1)
                .saturating_mul(count)
                .saturating_add(2),
        );
        json.push('[');
        for index in 0..count {
            if index != 0 {
                json.push(',');
            }
            json.push_str(value);
        }
        json.push(']');
        json.into_bytes()
    }

    #[test]
    fn structural_preflight_rejects_deep_and_nested_string_amplified_json() {
        let depth = JSON_MAX_DEPTH + 2;
        let mut nested = "[".repeat(depth);
        nested.push_str("null");
        nested.push_str(&"]".repeat(depth));
        let error = preflight_store_json(
            "config",
            "snapshot",
            nested.as_bytes(),
            StoreProfile::Generic,
        )
        .expect_err("excessive depth must fail before typed deserialization");
        assert!(error.to_string().contains("nesting"));

        let artists = json_array("\"\"", super::super::live::NESTED_TEXT_ITEMS_LIMIT + 1);
        let mut library = br#"{"favorites":[{"artists":"#.to_vec();
        library.extend_from_slice(&artists);
        library.extend_from_slice(br#"}]}"#);
        let error = preflight_store_json("library", "snapshot", &library, StoreProfile::Library)
            .expect_err("nested string vectors must not amplify without a count bound");
        assert!(error.to_string().contains("dynamic text items"));
    }

    #[test]
    fn playlist_preflight_accepts_rich_scalar_metadata_at_the_live_track_limit() {
        const SCALAR_STRINGS_PER_SONG: usize = 15;
        let song = r#"{"video_id":"abcdefghijk","title":"Title","artist":"Artist","duration":"3:00","album":"Album","album_artist":"Album Artist","album_release_date":"2026","album_release_date_precision":"year","album_type":"album","album_art_url":"https://example.invalid/art","isrc":"ISRC","origin_key":"origin","origin_url":"https://example.invalid/source","import_session_id":"session","yt_video_id":"abcdefghijk"}"#;
        const {
            assert!(
                SCALAR_STRINGS_PER_SONG * super::super::live::PLAYLIST_TRACK_LIMIT
                    > super::super::live::NESTED_TEXT_ITEMS_LIMIT
            );
        }
        let songs = json_array(song, super::super::live::PLAYLIST_TRACK_LIMIT);
        let mut bytes = br#"{"playlists":[{"id":"rich","name":"Rich","songs":"#.to_vec();
        bytes.extend_from_slice(&songs);
        bytes.extend_from_slice(br#"}]}"#);

        preflight_store_json("playlists", "snapshot", &bytes, StoreProfile::Playlists)
            .expect("ordinary scalar Song strings are heap-budgeted, not nested-item-counted");
        serde_json::from_slice::<Playlists>(&bytes).expect("fixture is valid playlist JSON");
    }

    #[test]
    fn offline_playlist_loader_accepts_more_than_the_live_clone_limit() {
        let root = test_directory("playlist-above-live-limit");
        let path = root.join("playlists.json");
        let song =
            r#"{"video_id":"abcdefghijk","title":"Title","artist":"Artist","duration":"3:00"}"#;
        let songs = json_array(song, super::super::live::PLAYLIST_TRACK_LIMIT + 1);
        let mut bytes = br#"{"playlists":[{"id":"many","name":"Many","songs":"#.to_vec();
        bytes.extend_from_slice(&songs);
        bytes.extend_from_slice(br#"}]}"#);
        fs::write(&path, &bytes).expect("write playlist above live clone limit");

        let playlists =
            load_playlists_at(&path).expect("offline allocation budget should permit this store");

        assert_eq!(
            playlists.list()[0].songs.len(),
            super::super::live::PLAYLIST_TRACK_LIMIT + 1
        );
        assert_eq!(fs::read(&path).expect("store remains unchanged"), bytes);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn daemon_playlist_loader_combines_live_and_disk_allocation_budgets() {
        let root = test_directory("daemon-combined-budget");
        let path = root.join("playlists.json");
        let bytes = serde_json::to_vec(&Playlists::default()).expect("playlist JSON");
        fs::write(&path, &bytes).expect("write playlists");

        let error =
            load_playlists_at_with_prior_budget(&path, super::super::live::CLONE_BUDGET_BYTES)
                .expect_err("daemon playlist load must share the live clone budget");

        assert!(error.to_string().contains("combined decoded stores"));
        assert_eq!(fs::read(&path).expect("store remains unchanged"), bytes);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn journal_sidecar_budget_is_checked_before_typed_deserialization() {
        let root = test_directory("journal-sidecar-budget");
        let path = root.join("counted.json");
        let base = b"{}";
        let sidecar = br#"{"payload":["newer"]}"#;
        let base_estimate =
            preflight_store_json("counted", "snapshot", base, StoreProfile::Generic)
                .expect("base preflight");
        let sidecar_estimate = preflight_store_json(
            "counted",
            "persistence journal sidecar",
            sidecar,
            StoreProfile::Generic,
        )
        .expect("sidecar preflight");
        assert!(sidecar_estimate > base_estimate);
        let prior_estimate = super::super::live::CLONE_BUDGET_BYTES
            .checked_sub(base_estimate)
            .expect("base estimate fits the aggregate budget");

        fs::write(&path, base).expect("write base");
        let sidecar_path = path.with_file_name("counted.json.intent.latest.json");
        fs::write(&sidecar_path, sidecar).expect("write sidecar");
        let journal = serde_json::json!({
            "v": 1,
            "op": "replace",
            "kind": "counted",
            "sidecar": "counted.json.intent.latest.json",
            "sha256": sha256_hex(sidecar),
        });
        fs::write(
            path.with_file_name("counted.json.intent.jsonl"),
            format!("{journal}\n"),
        )
        .expect("write journal");

        COUNTED_STORE_DESERIALIZATIONS.store(0, Ordering::SeqCst);
        let error = expect_source_error(
            load_store_parsed::<CountedStore>(
                "counted",
                &path,
                PLAYLISTS_MAX_BYTES,
                "counted",
                prior_estimate,
            ),
            "oversized replacement must fail before its typed decode",
        );

        assert!(error.to_string().contains("combined decoded stores"));
        assert_eq!(
            COUNTED_STORE_DESERIALIZATIONS.load(Ordering::SeqCst),
            1,
            "only the in-budget base snapshot may be typed-deserialized"
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn structural_preflight_counts_empty_song_objects_and_signal_map_keys() {
        let song_count = super::super::live::CLONE_BUDGET_BYTES
            .checked_div(std::mem::size_of::<Song>().max(1))
            .unwrap_or_default()
            .saturating_add(1);
        let songs = json_array("{}", song_count);
        let mut library = br#"{"history":"#.to_vec();
        library.extend_from_slice(&songs);
        library.extend_from_slice(b"}");
        preflight_store_json("library", "snapshot", &library, StoreProfile::Library)
            .expect_err("empty song structs must still consume the allocation budget");

        let mut signals = String::from("{\"tracks\":{");
        for index in 0..=super::super::live::NESTED_TEXT_ITEMS_LIMIT {
            if index != 0 {
                signals.push(',');
            }
            use std::fmt::Write as _;
            write!(&mut signals, "\"track-{index}\":{{}}").expect("append signal entry");
        }
        signals.push_str("}}");
        let error = preflight_store_json(
            "signals",
            "snapshot",
            signals.as_bytes(),
            StoreProfile::Signals,
        )
        .expect_err("dynamic signal map keys must count as allocated strings");
        assert!(error.to_string().contains("dynamic text items"));
    }

    #[test]
    fn aggregate_budget_bounds_all_final_stores_together() {
        let mut budget = AggregateBudget::default();
        budget
            .add("library", super::super::live::CLONE_BUDGET_BYTES / 2)
            .expect("first store fits");
        let error = budget
            .add("playlists", super::super::live::CLONE_BUDGET_BYTES / 2 + 1)
            .expect_err("combined final stores must share one budget");
        assert!(error.to_string().contains("combined decoded stores"));
    }

    #[test]
    fn absent_stores_are_valid_first_run_defaults() {
        let root = test_directory("missing");
        let sources = load_sources_at(paths_under(&root)).expect("missing stores are valid");
        assert_eq!(sources.config.volume, Config::default().volume);
        assert!(sources.library.favorites.is_empty());
        assert!(sources.playlists.list().is_empty());
        assert!(sources.signals.play_log().is_empty());
        assert!(sources.station.active.is_none());
        assert!(fs::read_dir(&root).expect("read root").next().is_none());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn unresolved_store_location_fails_instead_of_exporting_defaults() {
        let root = test_directory("unresolved");
        let mut paths = paths_under(&root);
        paths.library = None;

        let error = expect_source_error(
            load_sources_at(paths),
            "unresolved storage must not look like a missing file",
        );
        assert!(error.to_string().contains("library"));
        assert!(error.to_string().contains("cannot be resolved"));
        assert!(fs::read_dir(&root).expect("read root").next().is_none());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn corrupt_existing_snapshot_fails_without_recovery_mutation() {
        let root = test_directory("corrupt");
        let paths = paths_under(&root);
        let library_path = paths.library.clone().expect("library path");
        let corrupt = b"{ definitely-not-json";
        fs::write(&library_path, corrupt).expect("write corrupt fixture");

        let error = expect_source_error(load_sources_at(paths), "corrupt store must fail closed");
        assert!(error.to_string().contains("library"));
        assert_eq!(
            fs::read(&library_path).expect("read unchanged store"),
            corrupt
        );
        assert_eq!(fs::read_dir(&root).expect("list root").count(), 1);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn daemon_playlist_loader_rejects_corrupt_data_without_recovery_mutation() {
        let root = test_directory("daemon-playlists-corrupt");
        let path = root.join("playlists.json");
        let corrupt = b"{ corrupt-playlists";
        fs::write(&path, corrupt).expect("write corrupt fixture");

        let error = load_playlists_at(&path).expect_err("corrupt playlists must fail closed");
        assert!(error.to_string().contains("playlists"));
        assert_eq!(fs::read(&path).expect("read unchanged store"), corrupt);
        assert_eq!(fs::read_dir(&root).expect("list root").count(), 1);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn non_regular_and_oversized_existing_snapshots_fail_closed() {
        let non_regular_root = test_directory("non-regular");
        let non_regular_paths = paths_under(&non_regular_root);
        fs::create_dir(non_regular_paths.signals.as_ref().expect("signals path"))
            .expect("create non-regular fixture");
        let error = expect_source_error(
            load_sources_at(non_regular_paths),
            "directory store must fail",
        );
        assert!(error.to_string().contains("signals"));
        fs::remove_dir_all(non_regular_root).expect("cleanup");

        let oversized_root = test_directory("oversized");
        let oversized_paths = paths_under(&oversized_root);
        let station_path = oversized_paths.station.clone().expect("station path");
        File::create(&station_path)
            .expect("create oversized fixture")
            .set_len(STATION_MAX_BYTES + 1)
            .expect("size sparse fixture");
        let error = expect_source_error(
            load_sources_at(oversized_paths),
            "oversized store must fail",
        );
        assert!(error.to_string().contains("station profile"));
        assert_eq!(
            fs::metadata(&station_path)
                .expect("unchanged fixture")
                .len(),
            STATION_MAX_BYTES + 1
        );
        fs::remove_dir_all(oversized_root).expect("cleanup");
    }

    #[test]
    fn valid_latest_journal_is_replayed_read_only() {
        let root = test_directory("journal");
        let path = root.join("library.json");
        let base_bytes = serde_json::to_vec_pretty(&Library::default()).expect("base JSON");
        fs::write(&path, &base_bytes).expect("write base");

        let mut latest = Library::default();
        latest
            .favorites
            .push(Song::remote("dQw4w9WgXcQ", "Newest", "Artist", "3:32"));
        let sidecar_bytes = serde_json::to_vec_pretty(&latest).expect("sidecar JSON");
        let sidecar_path = path.with_file_name("library.json.intent.latest.json");
        fs::write(&sidecar_path, &sidecar_bytes).expect("write sidecar");
        let journal = serde_json::json!({
            "v": 1,
            "op": "replace",
            "kind": "library",
            "sidecar": "library.json.intent.latest.json",
            "sha256": sha256_hex(&sidecar_bytes),
        });
        let journal_path = path.with_file_name("library.json.intent.jsonl");
        fs::write(&journal_path, format!("{journal}\n")).expect("write journal");

        let loaded: Library = load_store("library", &path, LIBRARY_MAX_BYTES, "library")
            .expect("replay valid journal");
        assert_eq!(loaded.favorites[0].title, "Newest");
        assert_eq!(fs::read(&path).expect("base unchanged"), base_bytes);
        assert_eq!(
            fs::read(&sidecar_path).expect("sidecar unchanged"),
            sidecar_bytes
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn valid_journal_does_not_mask_a_corrupt_base_snapshot() {
        let root = test_directory("journal-corrupt-base");
        let path = root.join("library.json");
        let corrupt = b"{ corrupt-base";
        fs::write(&path, corrupt).expect("write corrupt base");

        let sidecar_bytes = serde_json::to_vec_pretty(&Library::default()).expect("sidecar JSON");
        let sidecar_path = path.with_file_name("library.json.intent.latest.json");
        fs::write(&sidecar_path, &sidecar_bytes).expect("write sidecar");
        let journal = serde_json::json!({
            "v": 1,
            "op": "replace",
            "kind": "library",
            "sidecar": "library.json.intent.latest.json",
            "sha256": sha256_hex(&sidecar_bytes),
        });
        let journal_path = path.with_file_name("library.json.intent.jsonl");
        fs::write(&journal_path, format!("{journal}\n")).expect("write journal");

        let error = load_store::<Library>("library", &path, LIBRARY_MAX_BYTES, "library")
            .expect_err("a valid journal must not hide a corrupt base");

        assert!(error.to_string().contains("snapshot"));
        assert_eq!(fs::read(&path).expect("base unchanged"), corrupt);
        assert_eq!(
            fs::read(&sidecar_path).expect("sidecar unchanged"),
            sidecar_bytes
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn corrupt_journal_and_orphan_sidecar_fail_closed() {
        let corrupt_root = test_directory("bad-journal");
        let corrupt_path = corrupt_root.join("library.json");
        let base = serde_json::to_vec_pretty(&Library::default()).expect("base JSON");
        fs::write(&corrupt_path, &base).expect("write base");
        let journal_path = corrupt_path.with_file_name("library.json.intent.jsonl");
        fs::write(&journal_path, b"not-json\n").expect("write bad journal");
        let error = load_store::<Library>("library", &corrupt_path, LIBRARY_MAX_BYTES, "library")
            .expect_err("corrupt journal must fail");
        assert!(error.to_string().contains("persistence journal"));
        assert_eq!(
            fs::read(&journal_path).expect("journal unchanged"),
            b"not-json\n"
        );
        fs::remove_dir_all(corrupt_root).expect("cleanup");

        let orphan_root = test_directory("orphan-sidecar");
        let orphan_path = orphan_root.join("library.json");
        fs::write(&orphan_path, &base).expect("write base");
        let mut newer = Library::default();
        newer
            .favorites
            .push(Song::remote("dQw4w9WgXcQ", "Newer", "Artist", "3:32"));
        fs::write(
            orphan_path.with_file_name("library.json.intent.latest.json"),
            serde_json::to_vec_pretty(&newer).expect("sidecar JSON"),
        )
        .expect("write orphan sidecar");
        let error = load_store::<Library>("library", &orphan_path, LIBRARY_MAX_BYTES, "library")
            .expect_err("orphan newer sidecar must fail");
        assert!(error.to_string().contains("orphan persistence sidecar"));
        fs::remove_dir_all(orphan_root).expect("cleanup");
    }
}
