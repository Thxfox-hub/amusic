use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::hash::Hasher;
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const INDEX_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ApiCategory {
    Source,
    Structure,
    Time,
    Effect,
    Tonal,
    Control,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NumericRange {
    pub min: Option<f64>,
    pub max: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ArgumentSpec {
    pub name: String,
    pub arg_types: Vec<String>,
    pub range: Option<NumericRange>,
    pub enum_values: Vec<String>,
    pub optional: bool,
    pub default_value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionSpec {
    pub name: String,
    pub aliases: Vec<String>,
    pub params: Vec<String>,
    pub signatures: Vec<String>,
    pub arguments: Vec<ArgumentSpec>,
    pub category: ApiCategory,
    pub special_literals: Vec<String>,
    pub usage_contexts: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LanguageIndex {
    pub source_version: String,
    pub specs_by_name: HashMap<String, FunctionSpec>,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredLanguageIndex {
    schema_version: u32,
    source_version: String,
    index: LanguageIndex,
}

#[derive(Default)]
struct IndexCache {
    version: String,
    index: LanguageIndex,
}

#[derive(Deserialize)]
struct RawDocJson {
    docs: Vec<RawDocEntry>,
}

#[derive(Deserialize)]
struct RawDocEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    longname: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    params: Option<Vec<RawParam>>,
    #[serde(default)]
    synonyms: Option<Vec<String>>,
    #[serde(default)]
    tags: Vec<RawTag>,
    #[serde(default)]
    meta: Option<RawMeta>,
}

#[derive(Deserialize, Clone)]
struct RawParam {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    r#type: Option<RawParamType>,
    #[serde(default)]
    optional: Option<bool>,
    #[serde(default)]
    defaultvalue: Option<serde_json::Value>,
}

#[derive(Deserialize, Clone)]
struct RawParamType {
    #[serde(default)]
    names: Option<Vec<String>>,
}

#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum RawTag {
    Simple(String),
    Detailed {
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        value: Option<String>,
        #[serde(rename = "originalTitle")]
        #[serde(default)]
        original_title: Option<String>,
    },
}

#[derive(Deserialize)]
struct RawMeta {
    #[serde(default)]
    path: String,
}

fn repo_root() -> PathBuf {
    // Historical name: callers join("strudel"/...). Prefer strudel_root for new code.
    crate::paths::strudel_root()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn default_doc_json_path() -> PathBuf {
    crate::paths::strudel_root().join("doc.json")
}

fn autocomplete_source_path() -> PathBuf {
    crate::paths::package_dir("codemirror").join("autocomplete.mjs")
}

fn tonal_ireal_path() -> PathBuf {
    crate::paths::package_dir("tonal").join("ireal.mjs")
}

fn index_cache_path() -> PathBuf {
    crate::paths::language_index_cache_path()
}

fn package_root(package: &str) -> PathBuf {
    crate::paths::package_dir(package)
}

fn collect_mjs_files(base: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(base) {
        Ok(v) => v,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_mjs_files(&path, out);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) == Some("mjs") {
            out.push(path);
        }
    }
}

fn source_paths() -> Vec<PathBuf> {
    let mut paths = vec![
        default_doc_json_path(),
        autocomplete_source_path(),
        tonal_ireal_path(),
    ];
    for pkg in ["core", "mini", "tonal", "effects", "audio", "draw"] {
        collect_mjs_files(&package_root(pkg), &mut paths);
    }
    paths.sort();
    paths.dedup();
    paths
}

fn classify(name: &str) -> ApiCategory {
    let n = name.to_ascii_lowercase();
    if matches!(n.as_str(), "s" | "sound" | "n" | "note" | "bank") {
        return ApiCategory::Source;
    }
    if matches!(
        n.as_str(),
        "stack" | "layer" | "cat" | "slowcat" | "fastcat" | "append"
    ) {
        return ApiCategory::Structure;
    }
    if matches!(
        n.as_str(),
        "slow" | "fast" | "hurry" | "segment" | "chunk" | "stutter" | "echo"
    ) {
        return ApiCategory::Time;
    }
    if matches!(
        n.as_str(),
        "gain"
            | "cutoff"
            | "resonance"
            | "hpf"
            | "lpf"
            | "crush"
            | "coarse"
            | "shape"
            | "room"
            | "size"
            | "delay"
            | "delaytime"
            | "dist"
            | "pan"
            | "vowel"
    ) {
        return ApiCategory::Effect;
    }
    if matches!(
        n.as_str(),
        "chord" | "scale" | "mode" | "voicing" | "degree"
    ) {
        return ApiCategory::Tonal;
    }
    if matches!(
        n.as_str(),
        "solo" | "mute" | "silence" | "setcps" | "getcps"
    ) {
        return ApiCategory::Control;
    }
    ApiCategory::Unknown
}

fn tag_to_string(tag: &RawTag) -> Option<String> {
    match tag {
        RawTag::Simple(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_ascii_lowercase())
            }
        }
        RawTag::Detailed {
            text,
            value,
            original_title,
        } => {
            let raw = value
                .as_deref()
                .or(text.as_deref())
                .or(original_title.as_deref())
                .unwrap_or("")
                .trim();
            if raw.is_empty() {
                None
            } else {
                Some(raw.to_ascii_lowercase())
            }
        }
    }
}

fn category_from_tag_or_path(tags: &[String], meta_path: Option<&str>) -> Option<ApiCategory> {
    for tag in tags {
        if matches!(
            tag.as_str(),
            "samples" | "sampler" | "synth" | "synths" | "soundfonts"
        ) {
            return Some(ApiCategory::Source);
        }
        if matches!(
            tag.as_str(),
            "tonal" | "chords" | "chord" | "scales" | "scale" | "voicing"
        ) {
            return Some(ApiCategory::Tonal);
        }
        if matches!(tag.as_str(), "timing" | "time" | "rhythm" | "euclid") {
            return Some(ApiCategory::Time);
        }
        if matches!(tag.as_str(), "structure" | "pattern") {
            return Some(ApiCategory::Structure);
        }
        if matches!(
            tag.as_str(),
            "fx" | "effect"
                | "effects"
                | "filter"
                | "envelope"
                | "modulation"
                | "distortion"
                | "reverb"
                | "delay"
        ) {
            return Some(ApiCategory::Effect);
        }
        if matches!(tag.as_str(), "external_io" | "visualization" | "control") {
            return Some(ApiCategory::Control);
        }
    }
    let path = meta_path.unwrap_or("").to_ascii_lowercase();
    if path.contains("/packages/tonal") {
        return Some(ApiCategory::Tonal);
    }
    if path.contains("/packages/mini") || path.contains("/packages/core") {
        return Some(ApiCategory::Structure);
    }
    if path.contains("/packages/draw") || path.contains("/packages/codemirror") {
        return Some(ApiCategory::Control);
    }
    None
}

fn parse_literal_array(source: &str, const_name: &str) -> Vec<String> {
    let needle = format!("const {} = [", const_name);
    let start = match source.find(&needle) {
        Some(s) => s + needle.len(),
        None => return Vec::new(),
    };
    let rest = &source[start..];
    let end_rel = match rest.find("];") {
        Some(e) => e,
        None => return Vec::new(),
    };
    let body = &rest[..end_rel];
    let mut out: Vec<String> = Vec::new();
    for chunk in body.split(',') {
        let t = chunk.trim();
        let cleaned = t.trim_start_matches('{').trim_end_matches('}').trim();
        if cleaned.starts_with('\'') && cleaned.ends_with('\'') && cleaned.len() >= 2 {
            out.push(cleaned[1..cleaned.len() - 1].to_string());
        }
    }
    out
}

fn parse_mode_completions(source: &str) -> Vec<String> {
    let needle = "const modeCompletions = [";
    let start = match source.find(needle) {
        Some(s) => s + needle.len(),
        None => return Vec::new(),
    };
    let rest = &source[start..];
    let end_rel = match rest.find("];") {
        Some(e) => e,
        None => return Vec::new(),
    };
    let body = &rest[..end_rel];
    let mut out = Vec::new();
    let mut s = body;
    while let Some(idx) = s.find("label: '") {
        let from = idx + "label: '".len();
        let tail = &s[from..];
        if let Some(end) = tail.find('\'') {
            let item = tail[..end].trim();
            if !item.is_empty() {
                out.push(item.to_string());
            }
            s = &tail[end + 1..];
        } else {
            break;
        }
    }
    out
}

fn strip_html(input: &str) -> String {
    input
        .replace("<p>", " ")
        .replace("</p>", " ")
        .replace("<br>", " ")
        .replace("<br/>", " ")
        .replace("<br />", " ")
        .replace('\n', " ")
}

fn parse_number_token(token: &str) -> Option<f64> {
    let cleaned = token.trim_matches(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-')));
    if cleaned.is_empty() || cleaned == "-" || cleaned == "." {
        None
    } else {
        cleaned.parse::<f64>().ok()
    }
}

fn parse_numeric_range(description: &str) -> Option<NumericRange> {
    let plain = strip_html(description).to_ascii_lowercase();
    let words: Vec<&str> = plain.split_whitespace().collect();
    for i in 0..words.len() {
        if words[i] == "between" && i + 3 < words.len() && words[i + 2] == "and" {
            let min = parse_number_token(words[i + 1]);
            let max = parse_number_token(words[i + 3]);
            if min.is_some() || max.is_some() {
                return Some(NumericRange { min, max });
            }
        }
    }
    None
}

fn parse_enum_values(description: &str) -> Vec<String> {
    let plain = strip_html(description);
    let lower = plain.to_ascii_lowercase();
    let Some(one_of) = lower.find("one of") else {
        return Vec::new();
    };
    let tail = plain[one_of + "one of".len()..].trim();
    let candidate = tail.split('.').next().unwrap_or("").trim();
    if candidate.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = candidate
        .split(|c: char| c == ',' || c == '|' || c == '/' || c == ';')
        .map(|s| {
            s.trim()
                .trim_matches('\'')
                .trim_matches('"')
                .trim_matches('`')
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

fn read_special_literals() -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    let src = match std::fs::read_to_string(autocomplete_source_path()) {
        Ok(s) => s,
        Err(_) => return map,
    };
    let pitch = parse_literal_array(&src, "pitchNames");
    if !pitch.is_empty() {
        map.insert("scale".to_string(), pitch.clone());
        map.insert("chord".to_string(), pitch);
    }
    let mode = parse_mode_completions(&src);
    if !mode.is_empty() {
        map.insert("mode".to_string(), mode);
    }
    let mut chord_symbols = Vec::new();
    let ireal = std::fs::read_to_string(tonal_ireal_path()).unwrap_or_default();
    if !ireal.is_empty() {
        let mut in_complex = false;
        let mut depth = 0i32;
        for line in ireal.lines() {
            if !in_complex {
                if line.contains("export const complex") && line.contains('{') {
                    in_complex = true;
                    depth = 1;
                }
                continue;
            }
            for ch in line.chars() {
                if ch == '{' {
                    depth += 1;
                } else if ch == '}' {
                    depth -= 1;
                }
            }
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix('\'') {
                if let Some(end) = rest.find('\'') {
                    let key = rest[..end].trim();
                    if !key.is_empty() {
                        chord_symbols.push(key.to_string());
                    }
                }
            }
            if depth <= 0 {
                break;
            }
        }
    }
    if !chord_symbols.is_empty() {
        chord_symbols.sort();
        chord_symbols.dedup();
        map.insert("chord_symbol".to_string(), chord_symbols);
    }
    map
}

fn package_context_from_path(path: &str) -> Option<String> {
    let lower = path.to_ascii_lowercase();
    for pkg in ["core", "mini", "tonal", "effects", "audio", "draw"] {
        if lower.contains(&format!("/packages/{pkg}")) {
            return Some(pkg.to_string());
        }
    }
    None
}

fn parse_named_exports(source: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim();
        for prefix in ["export const ", "export function ", "export class "] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    names.push(name);
                }
            }
        }
        if let Some(mut rest) = trimmed.strip_prefix("export {") {
            if let Some(close_idx) = rest.find('}') {
                rest = &rest[..close_idx];
                for item in rest.split(',') {
                    let token = item.trim();
                    if token.is_empty() {
                        continue;
                    }
                    let name = token
                        .split_whitespace()
                        .last()
                        .unwrap_or("")
                        .trim_matches('{')
                        .trim_matches('}');
                    if !name.is_empty() {
                        names.push(name.to_string());
                    }
                }
            }
        }
    }
    names
}

fn collect_package_metadata() -> HashMap<String, BTreeSet<String>> {
    let mut by_symbol: HashMap<String, BTreeSet<String>> = HashMap::new();
    for pkg in ["core", "mini", "tonal", "effects", "audio", "draw"] {
        let mut files = Vec::new();
        collect_mjs_files(&package_root(pkg), &mut files);
        for file in files {
            let src = match std::fs::read_to_string(&file) {
                Ok(v) => v,
                Err(_) => continue,
            };
            for symbol in parse_named_exports(&src) {
                by_symbol
                    .entry(symbol.to_ascii_lowercase())
                    .or_default()
                    .insert(pkg.to_string());
            }
        }
    }
    by_symbol
}

fn source_version(paths: &[PathBuf]) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for path in paths {
        hasher.write(path.to_string_lossy().as_bytes());
        if let Ok(bytes) = std::fs::read(path) {
            hasher.write(&bytes);
        }
    }
    format!(
        "lang-index-v{}-{:016x}",
        INDEX_SCHEMA_VERSION,
        hasher.finish()
    )
}

fn build_index(path: &PathBuf) -> LanguageIndex {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return LanguageIndex::default(),
    };
    let raw: RawDocJson = match serde_json::from_str(&src) {
        Ok(v) => v,
        Err(_) => return LanguageIndex::default(),
    };
    let mut specs_by_name: HashMap<String, FunctionSpec> = HashMap::new();
    let mut labels: BTreeSet<String> = BTreeSet::new();
    let special_literals = read_special_literals();
    let package_metadata = collect_package_metadata();

    for entry in raw.docs {
        if entry.kind == "package" {
            continue;
        }
        let name = if !entry.name.trim().is_empty() {
            entry.name.trim().to_string()
        } else {
            entry.longname.trim().to_string()
        };
        if name.is_empty() || name.starts_with('_') {
            continue;
        }
        let aliases: Vec<String> = entry
            .synonyms
            .unwrap_or_default()
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect();
        let raw_params = entry.params.unwrap_or_default();
        let params: Vec<String> = raw_params
            .iter()
            .map(|p| p.name.clone())
            .filter(|s| !s.trim().is_empty())
            .collect();
        let mut arguments: Vec<ArgumentSpec> = raw_params
            .into_iter()
            .filter_map(|p| {
                let pname = p.name.trim().to_string();
                if pname.is_empty() {
                    return None;
                }
                let description = p.description.clone().unwrap_or_default();
                let mut enum_values = parse_enum_values(&description);
                let range = parse_numeric_range(&description);
                let arg_types = p
                    .r#type
                    .and_then(|t| t.names)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|t| !t.trim().is_empty())
                    .collect::<Vec<_>>();
                let default_value = p.defaultvalue.map(|v| match v {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                });
                if enum_values.is_empty()
                    && arg_types.iter().any(|t| t.eq_ignore_ascii_case("string"))
                    && p.name.eq_ignore_ascii_case("mode")
                {
                    enum_values.extend(special_literals.get("mode").cloned().unwrap_or_default());
                }
                enum_values.sort();
                enum_values.dedup();
                Some(ArgumentSpec {
                    name: pname,
                    arg_types,
                    range,
                    enum_values,
                    optional: p.optional.unwrap_or(false),
                    default_value,
                })
            })
            .collect();
        let signatures = if arguments.is_empty() {
            vec![format!("{name}()")]
        } else {
            let parts: Vec<String> = arguments
                .iter()
                .map(|a| {
                    if a.optional {
                        format!("[{}]", a.name)
                    } else {
                        a.name.clone()
                    }
                })
                .collect();
            vec![format!("{}({})", name, parts.join(", "))]
        };
        let tags: Vec<String> = entry.tags.iter().filter_map(tag_to_string).collect();
        let category =
            category_from_tag_or_path(&tags, entry.meta.as_ref().map(|m| m.path.as_str()))
                .unwrap_or_else(|| classify(&name));
        let mut spec_literals = Vec::new();
        if let Some(items) = special_literals.get(&name.to_ascii_lowercase()) {
            spec_literals.extend(items.clone());
        }
        if name.eq_ignore_ascii_case("chord") {
            if let Some(items) = special_literals.get("chord_symbol") {
                spec_literals.extend(items.clone());
            }
        }
        if let Some(first) = arguments.first_mut() {
            if first.enum_values.is_empty() {
                if name.eq_ignore_ascii_case("scale") {
                    first.enum_values.extend(spec_literals.clone());
                }
                if name.eq_ignore_ascii_case("mode") {
                    first.enum_values.extend(spec_literals.clone());
                }
            }
            first.enum_values.sort();
            first.enum_values.dedup();
        }
        spec_literals.sort();
        spec_literals.dedup();
        let mut usage_contexts: BTreeSet<String> = BTreeSet::new();
        if let Some(path) = entry.meta.as_ref().map(|m| m.path.as_str()) {
            if let Some(pkg) = package_context_from_path(path) {
                usage_contexts.insert(pkg);
            }
        }
        if let Some(ctxs) = package_metadata.get(&name.to_ascii_lowercase()) {
            usage_contexts.extend(ctxs.iter().cloned());
        }
        for alias in &aliases {
            if let Some(ctxs) = package_metadata.get(&alias.to_ascii_lowercase()) {
                usage_contexts.extend(ctxs.iter().cloned());
            }
        }
        let spec = FunctionSpec {
            name: name.clone(),
            aliases: aliases.clone(),
            params,
            signatures,
            arguments,
            category,
            special_literals: spec_literals,
            usage_contexts: usage_contexts.into_iter().collect(),
        };
        labels.insert(name.clone());
        for a in &aliases {
            labels.insert(a.clone());
        }
        specs_by_name.insert(name.clone(), spec.clone());
        for a in aliases {
            specs_by_name.entry(a).or_insert_with(|| spec.clone());
        }
    }

    LanguageIndex {
        source_version: source_version(&source_paths()),
        specs_by_name,
        labels: labels.into_iter().collect(),
    }
}

fn read_cached_index(expected_version: &str) -> Option<LanguageIndex> {
    let path = index_cache_path();
    let src = std::fs::read_to_string(path).ok()?;
    let cached: StoredLanguageIndex = serde_json::from_str(&src).ok()?;
    if cached.schema_version != INDEX_SCHEMA_VERSION {
        return None;
    }
    if cached.source_version != expected_version || cached.index.source_version != expected_version
    {
        return None;
    }
    Some(cached.index)
}

fn write_cached_index(index: &LanguageIndex) {
    let path = index_cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let payload = StoredLanguageIndex {
        schema_version: INDEX_SCHEMA_VERSION,
        source_version: index.source_version.clone(),
        index: index.clone(),
    };
    if let Ok(serialized) = serde_json::to_string(&payload) {
        let _ = std::fs::write(path, serialized);
    }
}

pub fn get_language_index() -> LanguageIndex {
    static CACHE: OnceLock<Mutex<IndexCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(IndexCache::default()));
    let mut guard = match cache.lock() {
        Ok(g) => g,
        Err(_) => return LanguageIndex::default(),
    };
    let path = default_doc_json_path();
    let version = source_version(&source_paths());
    if guard.version != version {
        let index = read_cached_index(&version).unwrap_or_else(|| {
            let built = build_index(&path);
            write_cached_index(&built);
            built
        });
        guard.version = version.clone();
        guard.index = index;
    }
    guard.index.clone()
}

pub fn category_for(name: &str) -> ApiCategory {
    let idx = get_language_index();
    idx.specs_by_name
        .get(name)
        .map(|s| s.category)
        .unwrap_or_else(|| classify(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numeric_range_from_description() {
        let range = parse_numeric_range("<p>value between 0 and 1</p>").expect("range");
        assert_eq!(range.min, Some(0.0));
        assert_eq!(range.max, Some(1.0));
    }

    #[test]
    fn parses_named_exports_variants() {
        let src = r#"
            export const foo = 1;
            export function bar() {}
            export { baz, qux as q };
        "#;
        let names = parse_named_exports(src);
        assert!(names.iter().any(|n| n == "foo"));
        assert!(names.iter().any(|n| n == "bar"));
        assert!(names.iter().any(|n| n == "baz"));
        assert!(names.iter().any(|n| n == "q"));
    }
}
