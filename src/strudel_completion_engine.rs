use crate::doc_json::DocStore;
use crate::settings::BankCaseStyle;
use crate::settings::SemanticHintsMode;
use crate::strudel_language_index::{
    category_for, get_language_index, ApiCategory, ArgumentSpec, FunctionSpec,
};
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const SOUND_HINTS: [&str; 16] = [
    "bd", "sd", "hh", "cp", "lt", "mt", "ht", "rim", "oh", "ch", "kick", "snare", "clap", "perc",
    "tom", "noise",
];
const BANK_HINTS: [&str; 8] = [
    "RolandTR909",
    "RolandTR808",
    "Akai",
    "LinnDrum",
    "breaks",
    "drums",
    "vox",
    "fx",
];
const SCALE_HINTS: [&str; 12] = [
    "major",
    "minor",
    "dorian",
    "phrygian",
    "lydian",
    "mixolydian",
    "aeolian",
    "locrian",
    "harmonic_minor",
    "melodic_minor",
    "pentatonic",
    "blues",
];

#[derive(Default)]
struct BankCache {
    values: Vec<String>,
    last_refresh: Option<Instant>,
}

#[derive(Default)]
struct SoundCache {
    values: Vec<String>,
    last_refresh: Option<Instant>,
}

#[derive(Default)]
struct SoundsByBankCache {
    values: HashMap<String, Vec<String>>,
    last_refresh: Option<Instant>,
}

#[derive(Default)]
struct ScaleNamesCache {
    values: Vec<String>,
    last_refresh: Option<Instant>,
}

#[derive(Default)]
struct DebugCounters {
    no_suggestions: usize,
    hidden_by_strict: usize,
}

fn debug_counters() -> &'static Mutex<DebugCounters> {
    static COUNTERS: OnceLock<Mutex<DebugCounters>> = OnceLock::new();
    COUNTERS.get_or_init(|| Mutex::new(DebugCounters::default()))
}

fn repo_root() -> PathBuf {
    crate::paths::strudel_root()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn cached_real_banks() -> Vec<String> {
    static CACHE: OnceLock<Mutex<BankCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(BankCache::default()));
    let mut guard = match cache.lock() {
        Ok(g) => g,
        Err(_) => return Vec::new(),
    };
    let stale = guard
        .last_refresh
        .map(|t| t.elapsed() > Duration::from_secs(10))
        .unwrap_or(true);
    if stale {
        let root = crate::sample_loader::default_sample_root();
        guard.values = crate::sample_loader::available_banks(&root);
        guard.last_refresh = Some(Instant::now());
    }
    guard.values.clone()
}

fn cached_real_sounds() -> Vec<String> {
    static CACHE: OnceLock<Mutex<SoundCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(SoundCache::default()));
    let mut guard = match cache.lock() {
        Ok(g) => g,
        Err(_) => return Vec::new(),
    };
    let stale = guard
        .last_refresh
        .map(|t| t.elapsed() > Duration::from_secs(10))
        .unwrap_or(true);
    if stale {
        let root = crate::sample_loader::default_sample_root();
        guard.values = crate::sample_loader::available_sound_names(&root);
        guard.last_refresh = Some(Instant::now());
    }
    guard.values.clone()
}

fn cached_real_sounds_by_bank() -> HashMap<String, Vec<String>> {
    static CACHE: OnceLock<Mutex<SoundsByBankCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(SoundsByBankCache::default()));
    let mut guard = match cache.lock() {
        Ok(g) => g,
        Err(_) => return HashMap::new(),
    };
    let stale = guard
        .last_refresh
        .map(|t| t.elapsed() > Duration::from_secs(10))
        .unwrap_or(true);
    if stale {
        let root = crate::sample_loader::default_sample_root();
        guard.values = crate::sample_loader::available_sounds_by_bank(&root)
            .into_iter()
            .collect();
        guard.last_refresh = Some(Instant::now());
    }
    guard.values.clone()
}

fn index_special_literals(fn_name: &str) -> Vec<String> {
    get_language_index()
        .specs_by_name
        .get(fn_name)
        .map(|s| s.special_literals.clone())
        .unwrap_or_default()
}

fn pitch_literals() -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for key in ["scale", "chord"] {
        for v in index_special_literals(key) {
            let t = v.trim();
            if t.is_empty() {
                continue;
            }
            let mut chars = t.chars();
            if let Some(head) = chars.next() {
                let tail: String = chars.collect();
                if matches!(head, 'A'..='G' | 'a'..='g')
                    && (tail.is_empty() || tail.chars().all(|c| c == '#' || c == 'b'))
                {
                    out.insert(t.to_string());
                }
            }
        }
    }
    if out.is_empty() {
        crate::strudel_editor::PITCH_NAMES
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    } else {
        out.into_iter().collect()
    }
}

fn looks_like_pitch_token(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let mut chars = token.chars();
    let Some(head) = chars.next() else {
        return false;
    };
    if !matches!(head, 'A'..='G' | 'a'..='g') {
        return false;
    }
    chars.all(|c| c == '#' || c == 'b')
}

fn scale_candidates_for_query(query: &str, known_scale_names: &[String]) -> Vec<String> {
    let normalized = query.trim().replace(' ', ":");
    let base = normalized.trim_end_matches(':');
    let base_segments: Vec<&str> = if base.is_empty() {
        Vec::new()
    } else {
        base.split(':').collect()
    };
    let need_next_segment = normalized.ends_with(':');
    let mut out: BTreeSet<String> = BTreeSet::new();

    for name in known_scale_names {
        let parts: Vec<&str> = name.split(':').collect();
        if parts.is_empty() || parts[0].is_empty() {
            continue;
        }

        if base_segments.is_empty() {
            out.insert(parts[0].to_string());
            continue;
        }

        let mut matches_base = true;
        for (i, seg) in base_segments.iter().enumerate() {
            let Some(candidate_seg) = parts.get(i) else {
                matches_base = false;
                break;
            };
            let cmp = if need_next_segment || i + 1 < base_segments.len() {
                candidate_seg.eq_ignore_ascii_case(seg)
            } else {
                candidate_seg
                    .to_ascii_lowercase()
                    .starts_with(&seg.to_ascii_lowercase())
            };
            if !cmp {
                matches_base = false;
                break;
            }
        }
        if !matches_base {
            continue;
        }

        let next_idx = if need_next_segment {
            base_segments.len()
        } else {
            base_segments.len().saturating_sub(1)
        };
        if let Some(next) = parts.get(next_idx) {
            if !next.is_empty() {
                out.insert((*next).to_string());
            }
        }
    }

    out.into_iter().collect()
}

fn normalize_scale_name(raw: &str) -> Option<String> {
    let clean = raw.trim();
    if clean.is_empty() {
        return None;
    }
    if !clean
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ' ' | ':'))
    {
        return None;
    }
    let normalized = clean
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(":")
        .replace("::", ":");
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn read_mode_names_from_codemirror() -> Vec<String> {
    let path = crate::paths::package_dir("codemirror").join("autocomplete.mjs");
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let Some(mode_pos) = src.find("const modeCompletions") else {
        return Vec::new();
    };
    let tail = &src[mode_pos..];
    let Some(open_rel) = tail.find('[') else {
        return Vec::new();
    };
    let body = &tail[open_rel + 1..];
    let Some(close_rel) = body.find("];") else {
        return Vec::new();
    };
    let body = &body[..close_rel];
    let mut out: BTreeSet<String> = BTreeSet::new();
    let mut s = body;
    while let Some(idx) = s.find("label: '") {
        let from = idx + "label: '".len();
        let tail = &s[from..];
        if let Some(end) = tail.find('\'') {
            let item = tail[..end].trim();
            if !item.is_empty() {
                out.insert(item.to_string());
            }
            s = &tail[end + 1..];
        } else {
            break;
        }
    }
    out.into_iter().collect()
}

fn mode_literals() -> Vec<String> {
    let mut merged: BTreeSet<String> = BTreeSet::new();
    for s in index_special_literals("mode") {
        merged.insert(s);
    }
    for s in read_mode_names_from_codemirror() {
        merged.insert(s);
    }
    if merged.is_empty() {
        crate::strudel_editor::MODE_COMPLETIONS
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    } else {
        merged.into_iter().collect()
    }
}

fn chord_symbol_literals() -> Vec<String> {
    let mut out: BTreeSet<String> = index_special_literals("chord_symbol").into_iter().collect();
    if out.is_empty() {
        for v in index_special_literals("chord") {
            let t = v.trim();
            if t.is_empty() {
                continue;
            }
            // Fallback for older index shapes that may still mix symbols into chord literals.
            if !looks_like_pitch_token(t) {
                out.insert(t.to_string());
            }
        }
    }
    if out.is_empty() {
        crate::strudel_editor::CHORD_SYMBOLS
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    } else {
        out.into_iter().collect()
    }
}

fn line_slice(text: &str, cursor_char: usize) -> &str {
    let cursor_byte = char_to_byte(text, cursor_char);
    let left = text[..cursor_byte].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let right = text[cursor_byte..]
        .find('\n')
        .map(|r| cursor_byte + r)
        .unwrap_or(text.len());
    &text[left..right]
}

fn extract_bank_from_line(line: &str) -> Option<String> {
    let mut start = 0usize;
    while let Some(idx) = line[start..]
        .find(".bank(")
        .or_else(|| line[start..].find("bank("))
    {
        let open = start + idx;
        let paren = line[open..].find('(').map(|p| open + p)?;
        let after = &line[paren + 1..];
        let q_idx = after.find('"').or_else(|| after.find('\''))?;
        let q_abs = paren + 1 + q_idx;
        let q = line.as_bytes()[q_abs];
        let rest = &line[q_abs + 1..];
        let close = rest.as_bytes().iter().position(|b| *b == q)?;
        let bank = rest[..close].trim().to_lowercase();
        if !bank.is_empty() {
            return Some(bank);
        }
        start = q_abs + 1 + close + 1;
    }
    None
}

#[derive(Debug, Clone)]
pub struct SemanticDiagnostic {
    pub message: String,
    pub quick_fix_label: Option<String>,
    pub quick_fix_code: Option<String>,
}

#[derive(Debug)]
pub struct SemanticProblem {
    pub byte_start: usize,
    pub message: String,
    pub severity: &'static str,
}

fn chain_categories(text: &str, cursor_char: usize) -> Vec<(String, ApiCategory)> {
    let line = line_slice(text, cursor_char);
    let mut names: Vec<String> = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'.' {
            i += 1;
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            if start < i {
                let name = line[start..i].to_ascii_lowercase();
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'(' {
                    names.push(name);
                }
            }
        } else {
            i += 1;
        }
    }
    names
        .into_iter()
        .map(|n| {
            let category = category_for(&n);
            (n, category)
        })
        .collect()
}

fn method_call_spans(line: &str) -> Vec<(String, usize, usize)> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut active_quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = active_quote {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == q {
                active_quote = None;
            }
            i += 1;
            continue;
        }
        if b == b'"' || b == b'\'' || b == b'`' {
            active_quote = Some(b);
            i += 1;
            continue;
        }
        if b == b'.' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j == i + 1 {
                i += 1;
                continue;
            }
            let name = line[i + 1..j].to_ascii_lowercase();
            let mut k = j;
            while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            if k >= bytes.len() || bytes[k] != b'(' {
                i = j;
                continue;
            }
            let mut p = k + 1;
            let mut depth = 1i32;
            let mut qmode: Option<u8> = None;
            while p < bytes.len() {
                let pb = bytes[p];
                if let Some(q) = qmode {
                    if pb == b'\\' && p + 1 < bytes.len() {
                        p += 2;
                        continue;
                    }
                    if pb == q {
                        qmode = None;
                    }
                    p += 1;
                    continue;
                }
                if pb == b'"' || pb == b'\'' || pb == b'`' {
                    qmode = Some(pb);
                    p += 1;
                    continue;
                }
                if pb == b'(' {
                    depth += 1;
                } else if pb == b')' {
                    depth -= 1;
                    if depth == 0 {
                        out.push((name, i, p + 1));
                        break;
                    }
                }
                p += 1;
            }
            i = p.saturating_add(1);
            continue;
        }
        i += 1;
    }
    out
}

fn reorder_source_before_control(line: &str) -> Option<String> {
    let calls = method_call_spans(line);
    let control = calls
        .iter()
        .find(|(name, _, _)| matches!(name.as_str(), "gain" | "cutoff"))?;
    let source = calls.iter().find(|(name, start, _)| {
        *start > control.1 && matches!(name.as_str(), "s" | "sound" | "n")
    })?;
    let (control_start, source_start, source_end) = (control.1, source.1, source.2);
    if source_start <= control_start || source_end > line.len() {
        return None;
    }
    let moved = &line[source_start..source_end];
    let mut out = String::with_capacity(line.len());
    out.push_str(&line[..control_start]);
    out.push_str(moved);
    out.push_str(&line[control_start..source_start]);
    out.push_str(&line[source_end..]);
    Some(out)
}

fn apply_semantic_mode_filter(
    text: &str,
    cursor_char: usize,
    semantic_mode: &SemanticHintsMode,
    items: &mut Vec<String>,
) {
    if matches!(semantic_mode, SemanticHintsMode::Strict) {
        if semantic_diagnostic(text, cursor_char, semantic_mode).is_some() {
            let before = items.len();
            items.retain(|s| !matches!(category_for(s), ApiCategory::Source));
            let removed = before.saturating_sub(items.len());
            if removed > 0 {
                if let Ok(mut c) = debug_counters().lock() {
                    c.hidden_by_strict += removed;
                }
            }
        }
    }
    if items.is_empty() {
        if let Ok(mut c) = debug_counters().lock() {
            c.no_suggestions += 1;
        }
    }
}

fn semantic_chain_warning(text: &str, cursor_char: usize) -> Option<SemanticDiagnostic> {
    let line = line_slice(text, cursor_char);
    let names = chain_categories(text, cursor_char);
    if names.len() < 2 {
        return None;
    }
    let is_source = |c: ApiCategory| matches!(c, ApiCategory::Source);
    let control_like_seen = names.iter().any(|(_, c)| !is_source(*c));
    if control_like_seen {
        for pair in names.windows(2) {
            if let [(left_name, left_cat), (right_name, right_cat)] = pair {
                if !is_source(*left_cat) && is_source(*right_cat) {
                    let quick_fix = reorder_source_before_control(line);
                    return Some(SemanticDiagnostic {
                        message: format!(
                            "Suspicious transition: {} -> {}. Usually source comes earlier.",
                            left_name, right_name
                        ),
                        quick_fix_label: quick_fix
                            .as_ref()
                            .map(|_| "Move source before effect".to_string()),
                        quick_fix_code: quick_fix,
                    });
                }
            }
        }
    }
    None
}

fn collect_quoted_args_for_fn(text: &str, fn_names: &[&str]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &text[start..i];
            if !fn_names.iter().any(|n| *n == ident) {
                continue;
            }
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() || bytes[i] != b'(' {
                continue;
            }
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() || (bytes[i] != b'"' && bytes[i] != b'\'' && bytes[i] != b'`') {
                continue;
            }
            let quote = bytes[i];
            i += 1;
            let value_start = i;
            while i < bytes.len() {
                if quote != b'`' && bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    break;
                }
                i += 1;
            }
            let value_end = i.min(text.len());
            if value_start <= value_end {
                let value = &text[value_start..value_end];
                if !value.trim().is_empty() {
                    out.push((value_start, value.to_string()));
                }
            }
        } else {
            i += 1;
        }
    }
    out
}

pub fn collect_sound_token_problems(text: &str) -> Vec<SemanticProblem> {
    let mut known: BTreeSet<String> = SOUND_HINTS.iter().map(|s| s.to_ascii_lowercase()).collect();
    for s in cached_real_sounds() {
        known.insert(s.to_ascii_lowercase());
    }
    let mut out = Vec::new();
    for (arg_start, value) in collect_quoted_args_for_fn(text, &["s", "sound"]) {
        let mut tok_start: Option<usize> = None;
        for (idx, ch) in value.char_indices() {
            let is_tok = ch.is_alphanumeric() || ch == '_' || ch == '-';
            if is_tok {
                if tok_start.is_none() {
                    tok_start = Some(idx);
                }
                continue;
            }
            if let Some(start) = tok_start.take() {
                let token = &value[start..idx];
                let token_lc = token.to_ascii_lowercase();
                let token_is_tail = idx == value.len();
                if token.chars().all(|c| c.is_ascii_digit())
                    || token.chars().all(|c| matches!(c, '-' | '~'))
                    || known.contains(&token_lc)
                    || (token_is_tail && known.iter().any(|k| k.starts_with(&token_lc)))
                {
                    continue;
                }
                let message = if token.chars().any(|c| !c.is_ascii()) {
                    format!(
                        "Unknown sample token `{}`: contains non-ASCII characters.",
                        token
                    )
                } else {
                    format!("Unknown sample `{}`.", token)
                };
                out.push(SemanticProblem {
                    byte_start: arg_start + start,
                    message,
                    severity: "Warning",
                });
            }
        }
        if let Some(start) = tok_start.take() {
            let token = &value[start..];
            let token_lc = token.to_ascii_lowercase();
            if token.chars().all(|c| c.is_ascii_digit())
                || token.chars().all(|c| matches!(c, '-' | '~'))
                || known.contains(&token_lc)
                || known.iter().any(|k| k.starts_with(&token_lc))
            {
                continue;
            }
            let message = if token.chars().any(|c| !c.is_ascii()) {
                format!(
                    "Unknown sample token `{}`: contains non-ASCII characters.",
                    token
                )
            } else {
                format!("Unknown sample `{}`.", token)
            };
            out.push(SemanticProblem {
                byte_start: arg_start + start,
                message,
                severity: "Warning",
            });
        }
    }
    out
}

pub fn collect_function_call_problems(text: &str) -> Vec<SemanticProblem> {
    fn find_call_end(text: &str, open_paren: usize) -> Option<usize> {
        let bytes = text.as_bytes();
        if open_paren >= bytes.len() || bytes[open_paren] != b'(' {
            return None;
        }
        let mut depth = 0usize;
        let mut i = open_paren;
        let mut quote: Option<u8> = None;
        while i < bytes.len() {
            let b = bytes[i];
            if let Some(q) = quote {
                if q != b'`' && b == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if b == q {
                    quote = None;
                }
                i += 1;
                continue;
            }
            if b == b'"' || b == b'\'' || b == b'`' {
                quote = Some(b);
                i += 1;
                continue;
            }
            if b == b'(' {
                depth += 1;
            } else if b == b')' {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i);
                }
            }
            i += 1;
        }
        None
    }

    fn split_top_level_args(src: &str) -> Vec<String> {
        let bytes = src.as_bytes();
        let mut out: Vec<String> = Vec::new();
        let mut start = 0usize;
        let mut depth_paren = 0usize;
        let mut depth_bracket = 0usize;
        let mut depth_brace = 0usize;
        let mut quote: Option<u8> = None;
        let mut i = 0usize;
        while i < bytes.len() {
            let b = bytes[i];
            if let Some(q) = quote {
                if q != b'`' && b == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if b == q {
                    quote = None;
                }
                i += 1;
                continue;
            }
            if b == b'"' || b == b'\'' || b == b'`' {
                quote = Some(b);
                i += 1;
                continue;
            }
            match b {
                b'(' => depth_paren += 1,
                b')' => depth_paren = depth_paren.saturating_sub(1),
                b'[' => depth_bracket += 1,
                b']' => depth_bracket = depth_bracket.saturating_sub(1),
                b'{' => depth_brace += 1,
                b'}' => depth_brace = depth_brace.saturating_sub(1),
                b',' if depth_paren == 0 && depth_bracket == 0 && depth_brace == 0 => {
                    out.push(src[start..i].trim().to_string());
                    start = i + 1;
                }
                _ => {}
            }
            i += 1;
        }
        out.push(src[start..].trim().to_string());
        out
    }

    fn parse_quoted_literal(arg: &str) -> Option<String> {
        let t = arg.trim();
        if t.len() < 2 {
            return None;
        }
        let first = t.as_bytes()[0];
        let last = *t.as_bytes().last()?;
        if (first == b'"' || first == b'\'' || first == b'`') && first == last {
            Some(t[1..t.len() - 1].to_string())
        } else {
            None
        }
    }

    let mut known: BTreeSet<String> = BTreeSet::new();
    for name in get_language_index().specs_by_name.keys() {
        known.insert(name.to_ascii_lowercase());
    }
    for label in get_language_index().labels {
        known.insert(label.to_ascii_lowercase());
    }
    // Common non-Strudel call names to avoid noise in mixed JS snippets.
    for allowed in [
        "if", "for", "while", "switch", "catch", "function", "return", "Math", "Date", "Array",
        "Object", "String", "Number", "Boolean", "Promise",
    ] {
        known.insert(allowed.to_ascii_lowercase());
    }

    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut out = Vec::new();
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &text[start..i];
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'(' {
                // Skip method calls obj.foo(...), check only bare function names.
                if start > 0 && bytes[start - 1] == b'.' {
                    i = j + 1;
                    continue;
                }
                let ident_lc = ident.to_ascii_lowercase();
                if !known.contains(&ident_lc) {
                    out.push(SemanticProblem {
                        byte_start: start,
                        message: format!("Unknown function `{}`.", ident),
                        severity: "Error",
                    });
                    i = j + 1;
                    continue;
                }
                if let Some(spec) = get_language_index().specs_by_name.get(&ident_lc) {
                    if let Some(call_end) = find_call_end(text, j) {
                        let args_src = &text[j + 1..call_end];
                        let args = split_top_level_args(args_src);
                        let provided_count = args.iter().filter(|a| !a.trim().is_empty()).count();
                        let required_count = spec
                            .arguments
                            .iter()
                            .filter(|a| !a.optional && a.default_value.is_none())
                            .count();
                        if provided_count < required_count {
                            out.push(SemanticProblem {
                                byte_start: start,
                                message: format!(
                                    "Function `{}` expects at least {} argument(s), got {}.",
                                    ident, required_count, provided_count
                                ),
                                severity: "Warning",
                            });
                        }
                        for (arg_idx, arg_text) in args.iter().enumerate() {
                            let Some(arg_spec) = spec.arguments.get(arg_idx) else {
                                break;
                            };
                            if arg_spec.enum_values.is_empty() {
                                continue;
                            }
                            let Some(lit) = parse_quoted_literal(arg_text) else {
                                continue;
                            };
                            let valid = arg_spec
                                .enum_values
                                .iter()
                                .any(|v| v.eq_ignore_ascii_case(&lit));
                            if !valid {
                                out.push(SemanticProblem {
                                    byte_start: start,
                                    message: format!(
                                        "Argument `{}` of `{}` has unknown value `{}`.",
                                        arg_spec.name, ident, lit
                                    ),
                                    severity: "Warning",
                                });
                            }
                        }
                    }
                }
                i = j + 1;
                continue;
            }
        } else {
            i += 1;
        }
    }
    out
}

fn read_scale_names_from_doc_json() -> Vec<String> {
    let path = crate::paths::strudel_root().join("doc.json");
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut out = BTreeSet::new();
    let mut start = 0usize;
    while let Some(idx) = src[start..].find(".scale(\"") {
        let open = start + idx + ".scale(\"".len();
        let rest = &src[open..];
        if let Some(close) = rest.find('"') {
            let arg = &rest[..close];
            if let Some((_, scale)) = arg.split_once(':') {
                if let Some(normalized) = normalize_scale_name(scale) {
                    out.insert(normalized);
                }
            } else {
                if let Some(normalized) = normalize_scale_name(arg) {
                    out.insert(normalized);
                }
            }
            start = open + close + 1;
        } else {
            break;
        }
    }
    out.into_iter().collect()
}

fn read_scale_names_from_tonal_mjs() -> Vec<String> {
    let path = crate::paths::package_dir("tonal").join("tonal.mjs");
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut out = BTreeSet::new();
    let mut start = 0usize;
    while let Some(idx) = src[start..].find(".scale(\"") {
        let open = start + idx + ".scale(\"".len();
        let rest = &src[open..];
        if let Some(close) = rest.find('"') {
            let arg = &rest[..close];
            if let Some((_, scale)) = arg.split_once(':') {
                if let Some(normalized) = normalize_scale_name(scale) {
                    out.insert(normalized);
                }
            }
            start = open + close + 1;
        } else {
            break;
        }
    }
    out.into_iter().collect()
}

fn read_scale_names_from_tonal_scale_type_dist() -> Vec<String> {
    let path = repo_root()
        .join("ide")
        .join("node_modules")
        .join("@tonaljs")
        .join("scale-type")
        .join("dist")
        .join("index.mjs");
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let Some(start_idx) = src.find("var SCALES = [") else {
        return Vec::new();
    };
    let mut out: BTreeSet<String> = BTreeSet::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape = false;
    let mut current = String::new();
    let mut entry_strings: Vec<String> = Vec::new();

    for ch in src[start_idx..].chars() {
        if in_string {
            if escape {
                current.push(ch);
                escape = false;
                continue;
            }
            if ch == '\\' {
                escape = true;
                continue;
            }
            if ch == '"' {
                in_string = false;
                if depth >= 2 {
                    entry_strings.push(current.clone());
                }
                current.clear();
                continue;
            }
            current.push(ch);
            continue;
        }

        match ch {
            '"' => {
                in_string = true;
            }
            '[' => {
                depth = depth.saturating_add(1);
            }
            ']' => {
                if depth == 2 && !entry_strings.is_empty() {
                    // First string is interval set, the rest are scale names/aliases.
                    for name in entry_strings.iter().skip(1) {
                        if let Some(normalized) = normalize_scale_name(name) {
                            out.insert(normalized);
                        }
                    }
                    entry_strings.clear();
                }
                if depth == 1 {
                    break;
                }
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }
    out.into_iter().collect()
}

fn cached_scale_names() -> Vec<String> {
    static CACHE: OnceLock<Mutex<ScaleNamesCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(ScaleNamesCache::default()));
    let mut guard = match cache.lock() {
        Ok(g) => g,
        Err(_) => return SCALE_HINTS.iter().map(|s| (*s).to_string()).collect(),
    };
    let stale = guard
        .last_refresh
        .map(|t| t.elapsed() > Duration::from_secs(30))
        .unwrap_or(true);
    if stale {
        let mut merged: BTreeSet<String> = SCALE_HINTS.iter().map(|s| (*s).to_string()).collect();
        for s in read_scale_names_from_doc_json() {
            merged.insert(s);
        }
        for s in read_scale_names_from_tonal_mjs() {
            merged.insert(s);
        }
        for s in read_scale_names_from_tonal_scale_type_dist() {
            merged.insert(s);
        }
        guard.values = merged.into_iter().collect();
        guard.last_refresh = Some(Instant::now());
    }
    guard.values.clone()
}

fn trailing_fragment_by_predicate(inside: &str, pred: impl Fn(char) -> bool) -> String {
    let mut start = inside.len();
    for (idx, ch) in inside.char_indices().rev() {
        if pred(ch) {
            start = idx;
        } else {
            break;
        }
    }
    inside[start..].to_string()
}

fn chord_tail_fragment(inside: &str) -> String {
    trailing_fragment_by_predicate(inside, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '#' | 'b' | '+' | '^' | ':' | '-')
    })
}

fn fragment_has_chord_root(fragment: &str) -> bool {
    pitch_literals().iter().any(|p| {
        fragment
            .to_ascii_lowercase()
            .starts_with(&p.to_ascii_lowercase())
    })
}

fn byte_to_char(text: &str, byte_offset: usize) -> usize {
    text[..byte_offset.min(text.len())].chars().count()
}

fn char_to_byte(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(b, _)| b)
        .unwrap_or(text.len())
}

fn extract_prefix(text: &str, cursor_char: usize) -> (usize, String) {
    let before: String = text.chars().take(cursor_char).collect();
    let start = before
        .rfind(|c: char| !(c.is_alphanumeric() || c == '_'))
        .map(|i| i + 1)
        .unwrap_or(0);
    let prefix_str = &before[start..];
    let start_char = before[..start].chars().count();
    (start_char, prefix_str.to_string())
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn identifier_at_char(text: &str, cursor_char: usize) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return None;
    }
    let mut idx = cursor_char.min(chars.len());
    if idx == chars.len() || !is_ident_char(chars[idx]) {
        if idx > 0 && is_ident_char(chars[idx - 1]) {
            idx -= 1;
        } else {
            return None;
        }
    }
    let mut start = idx;
    while start > 0 && is_ident_char(chars[start - 1]) {
        start -= 1;
    }
    let mut end = idx + 1;
    while end < chars.len() && is_ident_char(chars[end]) {
        end += 1;
    }
    let name: String = chars[start..end].iter().collect();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocMarkupSegment {
    Text(String),
    Code(String),
    LineBreak,
    ParagraphBreak,
}

fn push_doc_text_segment(segments: &mut Vec<DocMarkupSegment>, in_code: bool, text: &str) {
    if text.is_empty() {
        return;
    }
    match segments.last_mut() {
        Some(DocMarkupSegment::Text(prev)) if !in_code => prev.push_str(text),
        Some(DocMarkupSegment::Code(prev)) if in_code => prev.push_str(text),
        _ if in_code => segments.push(DocMarkupSegment::Code(text.to_string())),
        _ => segments.push(DocMarkupSegment::Text(text.to_string())),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupportedDocTag {
    CodeOpen,
    CodeClose,
    ParagraphOpen,
    ParagraphClose,
    Break,
}

fn supported_doc_tag_at(input: &str, byte_index: usize) -> Option<(SupportedDocTag, usize)> {
    let tail = input.get(byte_index..)?;
    if !tail.starts_with('<') {
        return None;
    }
    let end_rel = tail.find('>')?;
    let raw = &tail[1..end_rel];
    let tag = match raw {
        "code" => SupportedDocTag::CodeOpen,
        "/code" => SupportedDocTag::CodeClose,
        "p" => SupportedDocTag::ParagraphOpen,
        "/p" => SupportedDocTag::ParagraphClose,
        "br" | "br/" | "br /" => SupportedDocTag::Break,
        _ => return None,
    };
    Some((tag, end_rel + 1))
}

pub fn parse_doc_markup(input: &str) -> Vec<DocMarkupSegment> {
    let mut segments = Vec::new();
    let mut in_code = false;
    let mut paragraph_open = false;
    let mut i = 0;
    while i < input.len() {
        if input[i..].starts_with('<') {
            if let Some((tag, tag_len)) = supported_doc_tag_at(input, i) {
                let raw_tag = &input[i..i + tag_len];
                match tag {
                    SupportedDocTag::CodeOpen if !in_code => {
                        in_code = true;
                        i += tag_len;
                        continue;
                    }
                    SupportedDocTag::CodeClose if in_code => {
                        in_code = false;
                        i += tag_len;
                        continue;
                    }
                    SupportedDocTag::ParagraphOpen if !in_code => {
                        paragraph_open = true;
                        i += tag_len;
                        continue;
                    }
                    SupportedDocTag::ParagraphClose if !in_code && paragraph_open => {
                        paragraph_open = false;
                        segments.push(DocMarkupSegment::ParagraphBreak);
                        i += tag_len;
                        continue;
                    }
                    SupportedDocTag::Break if !in_code => {
                        segments.push(DocMarkupSegment::LineBreak);
                        i += tag_len;
                        continue;
                    }
                    _ => {
                        push_doc_text_segment(&mut segments, in_code, raw_tag);
                        i += tag_len;
                        continue;
                    }
                }
            }
        }
        let ch = input[i..].chars().next().unwrap();
        push_doc_text_segment(&mut segments, in_code, ch.encode_utf8(&mut [0; 4]));
        i += ch.len_utf8();
    }
    segments
}

pub fn doc_markup_to_plain(input: &str) -> String {
    let mut out = String::new();
    for segment in parse_doc_markup(input) {
        match segment {
            DocMarkupSegment::Text(text) | DocMarkupSegment::Code(text) => out.push_str(&text),
            DocMarkupSegment::LineBreak | DocMarkupSegment::ParagraphBreak => out.push(' '),
        }
    }
    out.replace('\n', " ")
}

fn html_to_plain(input: &str) -> String {
    doc_markup_to_plain(input)
}

fn compact_doc_description(text: &str) -> String {
    html_to_plain(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_with_ellipsis(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, ch) in text.chars().enumerate() {
        if i >= max_chars {
            out.push_str("...");
            return out;
        }
        out.push(ch);
    }
    out
}

fn doc_signature(doc: &crate::doc_json::DocEntry) -> String {
    let name = if doc.name.trim().is_empty() {
        doc.longname.trim()
    } else {
        doc.name.trim()
    };
    let mut sig = String::new();
    sig.push_str(name);
    sig.push('(');
    if let Some(params) = doc.params.as_ref() {
        let mut first = true;
        for p in params {
            let pname = p.name.trim();
            if pname.is_empty() {
                continue;
            }
            if !first {
                sig.push_str(", ");
            }
            first = false;
            sig.push_str(pname);
        }
    }
    sig.push(')');
    sig
}

fn parse_numeric_bounds(description: &str) -> Option<(String, String)> {
    let plain = html_to_plain(description).to_ascii_lowercase();
    let words: Vec<&str> = plain.split_whitespace().collect();
    for i in 0..words.len() {
        if words[i] == "between" && i + 3 < words.len() && words[i + 2] == "and" {
            let a = words[i + 1].trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
            let b = words[i + 3].trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
            if !a.is_empty() && !b.is_empty() {
                return Some((a.to_string(), b.to_string()));
            }
        }
    }
    None
}

fn extract_ident_before(text: &str, pos: usize) -> Option<String> {
    let bytes = text.as_bytes();
    if pos == 0 || pos > bytes.len() {
        return None;
    }
    let mut i = pos;
    while i > 0 && (bytes[i - 1] as char).is_ascii_whitespace() {
        i -= 1;
    }
    let end = i;
    while i > 0 {
        let c = bytes[i - 1] as char;
        if c.is_ascii_alphanumeric() || c == '_' {
            i -= 1;
        } else {
            break;
        }
    }
    if i < end {
        Some(text[i..end].to_string())
    } else {
        None
    }
}

fn is_inside_quoted_string(text: &str, cursor_char: usize) -> bool {
    let cursor_byte = char_to_byte(text, cursor_char);
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut active_quote: Option<u8> = None;
    while i < cursor_byte && i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = active_quote {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == q {
                active_quote = None;
            }
            i += 1;
            continue;
        }
        if b == b'"' || b == b'\'' || b == b'`' {
            active_quote = Some(b);
        }
        i += 1;
    }
    active_quote.is_some()
}

fn active_function_call(text: &str, cursor_char: usize) -> Option<(String, usize)> {
    let cursor_byte = char_to_byte(text, cursor_char);
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut stack: Vec<(String, usize)> = Vec::new();
    let mut active_quote: Option<u8> = None;

    while i < cursor_byte && i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = active_quote {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == q {
                active_quote = None;
            }
            i += 1;
            continue;
        }
        if b == b'"' || b == b'\'' || b == b'`' {
            active_quote = Some(b);
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < cursor_byte && bytes[i + 1] == b'/' {
            while i < cursor_byte && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'(' {
            if let Some(name) = extract_ident_before(text, i) {
                stack.push((name, 0));
            }
            i += 1;
            continue;
        }
        if b == b',' {
            if let Some((_, arg_idx)) = stack.last_mut() {
                *arg_idx += 1;
            }
            i += 1;
            continue;
        }
        if b == b')' {
            let _ = stack.pop();
            i += 1;
            continue;
        }
        i += 1;
    }
    stack.last().cloned()
}

fn find_active_quote_start(text: &str, cursor_char: usize) -> Option<(usize, u8)> {
    let cursor_byte = char_to_byte(text, cursor_char);
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut active_quote: Option<u8> = None;
    let mut quote_start = 0usize;
    while i < cursor_byte && i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = active_quote {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == q {
                active_quote = None;
            }
            i += 1;
            continue;
        }
        if b == b'"' || b == b'\'' || b == b'`' {
            active_quote = Some(b);
            quote_start = i;
        }
        i += 1;
    }
    active_quote.map(|q| (quote_start, q))
}

fn string_fragment(text: &str, cursor_char: usize) -> Option<(usize, String, String)> {
    let cursor_byte = char_to_byte(text, cursor_char);
    let (quote_start, _quote_char) = find_active_quote_start(text, cursor_char)?;
    // Allow empty fragment right after opening quote: mode("|")
    if cursor_byte < quote_start + 1 || cursor_byte > text.len() {
        return None;
    }
    let inside = &text[quote_start + 1..cursor_byte];
    let split_idx = inside
        .char_indices()
        .rfind(|(_, c)| {
            c.is_whitespace() || matches!(c, '[' | ']' | '{' | '}' | '(' | ')' | '<' | '>' | ',')
        })
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let fragment = inside[split_idx..].to_string();
    let fragment_start_byte = quote_start + 1 + split_idx;
    let fragment_start_char = byte_to_char(text, fragment_start_byte);
    Some((fragment_start_char, fragment, inside.to_string()))
}

fn quoted_args_for_fn(text: &str, fn_names: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &text[start..i];
            if !fn_names.iter().any(|n| *n == ident) {
                continue;
            }
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() || bytes[i] != b'(' {
                continue;
            }
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() || (bytes[i] != b'"' && bytes[i] != b'\'') {
                continue;
            }
            let quote = bytes[i];
            i += 1;
            let value_start = i;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    break;
                }
                i += 1;
            }
            if i <= bytes.len() {
                let value = &text[value_start..i.min(text.len())];
                if !value.trim().is_empty() {
                    out.push(value.to_string());
                }
            }
        } else {
            i += 1;
        }
    }
    out.sort_by(|a, b| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()));
    out.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    out
}

fn filter_completions(all: &[String], prefix: &str) -> Vec<String> {
    if prefix.is_empty() {
        return Vec::new();
    }
    let lower = prefix.to_ascii_lowercase();
    let mut out: Vec<String> = all
        .iter()
        .filter(|w| {
            let wl = w.to_ascii_lowercase();
            wl.starts_with(&lower) && wl != lower
        })
        .cloned()
        .collect();
    out.sort_by(|a, b| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()));
    out
}

fn match_score(prefix: &str, candidate: &str, source_boost: i32) -> i32 {
    fn is_subsequence(needle: &str, haystack: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        let mut it = needle.chars();
        let mut current = it.next();
        for ch in haystack.chars() {
            if let Some(n) = current {
                if ch == n {
                    current = it.next();
                    if current.is_none() {
                        return true;
                    }
                }
            } else {
                return true;
            }
        }
        current.is_none()
    }

    let p = prefix.to_ascii_lowercase();
    let c = candidate.to_ascii_lowercase();
    if p.is_empty() {
        return source_boost;
    }
    if candidate == prefix {
        return 1200 + source_boost;
    }
    if c == p {
        return 1000 + source_boost;
    }
    if c.starts_with(&p) {
        let len_bonus = (40i32 - candidate.len() as i32).max(0);
        return 800 + len_bonus + source_boost;
    }
    let token_prefix = c
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|tok| !tok.is_empty() && tok.starts_with(&p));
    if token_prefix {
        return 620 + source_boost;
    }
    if c.contains(&p) {
        return 500 + source_boost;
    }
    if is_subsequence(&p, &c) {
        // Tolerates minor typos/missing characters, e.g. "Rolad" -> "RolandTR909".
        return 350 + source_boost;
    }
    -1
}

fn rank_and_dedup(prefix: &str, entries: Vec<(String, i32)>) -> Vec<String> {
    let mut best: HashMap<String, (String, i32)> = HashMap::new();
    for (item, source_boost) in entries {
        let score = match_score(prefix, &item, source_boost);
        if score < 0 {
            continue;
        }
        let key = item.to_ascii_lowercase();
        let replace = match best.get(&key) {
            Some((_, old_score)) => score > *old_score,
            None => true,
        };
        if replace {
            best.insert(key, (item, score));
        }
    }

    let mut ranked: Vec<(String, i32)> = best.into_values().collect();
    ranked.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_ascii_lowercase().cmp(&b.0.to_ascii_lowercase()))
    });
    ranked.into_iter().map(|(s, _)| s).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionContext {
    Global,
    FunctionCall,
    StringMiniNotation,
    NamedArgs,
    MethodChain,
}

fn looks_like_named_arg_context(text: &str, cursor_char: usize) -> bool {
    if is_inside_quoted_string(text, cursor_char) {
        return false;
    }
    let before: String = text.chars().take(cursor_char).collect();
    let segment = before
        .rsplit(|c: char| matches!(c, ',' | '(' | ')' | '\n' | '{' | '}'))
        .next()
        .unwrap_or("")
        .trim();
    // Covers:
    // fn({ cu| })  -> key position
    // fn({ cutoff: 10, ga| }) -> next key
    segment.ends_with('{')
        || segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c.is_ascii_whitespace())
}

fn detect_completion_context(text: &str, cursor_char: usize) -> CompletionContext {
    if let Some((fn_name, _)) = active_function_call(text, cursor_char) {
        if is_inside_quoted_string(text, cursor_char) && matches!(fn_name.as_str(), "s" | "sound") {
            return CompletionContext::StringMiniNotation;
        }
        if looks_like_named_arg_context(text, cursor_char) {
            return CompletionContext::NamedArgs;
        }
        return CompletionContext::FunctionCall;
    }
    if line_slice(text, cursor_char).contains('.') {
        return CompletionContext::MethodChain;
    }
    CompletionContext::Global
}

fn try_named_args_handler(text: &str, cursor_char: usize) -> Option<(usize, String, Vec<String>)> {
    let (fn_name, _) = active_function_call(text, cursor_char)?;
    if !looks_like_named_arg_context(text, cursor_char) {
        return None;
    }
    let (prefix_start_char, prefix) = extract_prefix(text, cursor_char);
    let idx = get_language_index();
    let spec = idx.specs_by_name.get(&fn_name)?;
    if spec.arguments.is_empty() {
        return None;
    }
    let mut entries = Vec::new();
    for arg in &spec.arguments {
        entries.push((format!("{}:", arg.name), 220));
        entries.push((arg.name.clone(), 180));
    }
    let items = rank_and_dedup(&prefix, entries);
    if items.is_empty() {
        None
    } else {
        Some((prefix_start_char, prefix, items))
    }
}

fn collect_contextual_completions(
    text: &str,
    cursor_char: usize,
    bank_case_style: &BankCaseStyle,
    bank_aliases_enabled: bool,
) -> Option<(usize, String, Vec<String>)> {
    // Rule-based contextual pipeline:
    // global / function-call / string-mini-notation / method-chain / named-args.
    match detect_completion_context(text, cursor_char) {
        CompletionContext::StringMiniNotation => {
            // Preserve codemirror-compatible ordering in string handlers.
            try_sound_handler(text, cursor_char)
                .or_else(|| try_chord_handler(text, cursor_char))
                .or_else(|| try_scale_handler(text, cursor_char))
                .or_else(|| try_mode_handler(text, cursor_char))
                .or_else(|| try_voicing_handler(text, cursor_char))
                .or_else(|| try_degree_handler(text, cursor_char))
                .or_else(|| {
                    try_bank_handler(text, cursor_char, bank_case_style, bank_aliases_enabled)
                })
                .or_else(|| try_index_argument_handler(text, cursor_char))
        }
        CompletionContext::NamedArgs => try_named_args_handler(text, cursor_char)
            .or_else(|| try_index_argument_handler(text, cursor_char)),
        CompletionContext::FunctionCall => try_sound_handler(text, cursor_char)
            .or_else(|| try_bank_handler(text, cursor_char, bank_case_style, bank_aliases_enabled))
            .or_else(|| try_chord_handler(text, cursor_char))
            .or_else(|| try_scale_handler(text, cursor_char))
            .or_else(|| try_mode_handler(text, cursor_char))
            .or_else(|| try_voicing_handler(text, cursor_char))
            .or_else(|| try_degree_handler(text, cursor_char))
            .or_else(|| try_index_argument_handler(text, cursor_char)),
        CompletionContext::MethodChain | CompletionContext::Global => None,
    }
}

fn ensure_quoted_arg_context(
    text: &str,
    cursor_char: usize,
    expected_names: &[&str],
) -> Option<(usize, String, String)> {
    let (fn_name, _) = active_function_call(text, cursor_char)?;
    if !expected_names.iter().any(|n| *n == fn_name) {
        return None;
    }
    if !is_inside_quoted_string(text, cursor_char) {
        // codemirror behavior: context recognized, but block suggestions until quote opens
        return Some((cursor_char, String::new(), String::new()));
    }
    let (fragment_start_char, _, inside) = string_fragment(text, cursor_char)?;
    Some((fragment_start_char, inside.clone(), inside))
}

fn try_sound_handler(text: &str, cursor_char: usize) -> Option<(usize, String, Vec<String>)> {
    let (fragment_start_char, _, inside) =
        ensure_quoted_arg_context(text, cursor_char, &["s", "sound"])?;
    if inside.is_empty() && !is_inside_quoted_string(text, cursor_char) {
        return Some((fragment_start_char, String::new(), Vec::new()));
    }
    let prefix = trailing_fragment_by_predicate(&inside, |c| c.is_ascii_alphanumeric() || c == '_');
    let prefix_start_char = cursor_char.saturating_sub(prefix.chars().count());
    let mut out: Vec<(String, i32)> = Vec::new();
    let selected_bank = extract_bank_from_line(line_slice(text, cursor_char));
    for c in SOUND_HINTS {
        out.push((c.to_string(), 90));
    }
    if let Some(bank) = selected_bank {
        if let Some(by_bank) = cached_real_sounds_by_bank().get(&bank) {
            for s in by_bank {
                out.push((s.clone(), 240));
            }
        }
    } else {
        for real in cached_real_sounds() {
            out.push((real, 180));
        }
    }
    for seen in quoted_args_for_fn(text, &["s", "sound"]) {
        if should_include_sound_history_value(&seen) {
            out.push((seen, 120));
        }
    }
    if let Some((_, _, inside)) = string_fragment(text, cursor_char) {
        let ctx = crate::mini_notation::context_at(&inside, inside.len());
        for sep in crate::mini_notation::completion_separators_for_context(&ctx) {
            out.push((sep.to_string(), 70));
        }
    }
    let items = rank_and_dedup(&prefix, out);
    Some((prefix_start_char, prefix, items))
}

fn should_include_sound_history_value(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 32 {
        return false;
    }
    trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':' | '/' | '@' | '.'))
}

fn try_bank_handler(
    text: &str,
    cursor_char: usize,
    bank_case_style: &BankCaseStyle,
    bank_aliases_enabled: bool,
) -> Option<(usize, String, Vec<String>)> {
    let (fragment_start_char, _, inside) = ensure_quoted_arg_context(text, cursor_char, &["bank"])?;
    if inside.is_empty() && !is_inside_quoted_string(text, cursor_char) {
        return Some((fragment_start_char, String::new(), Vec::new()));
    }
    // codemirror bank handler uses the whole inside string as fragment.
    let prefix = inside.clone();
    let prefix_start_char = fragment_start_char;
    let mut out: Vec<(String, i32)> = Vec::new();
    let bank_aliases = |bank: &str| {
        let mut aliases: BTreeSet<String> = BTreeSet::new();
        let trimmed = bank.trim();
        if trimmed.is_empty() {
            return aliases;
        }
        aliases.insert(trimmed.to_string());
        aliases.insert(trimmed.to_ascii_lowercase());
        let compact: String = trimmed
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();
        if !compact.is_empty() {
            aliases.insert(compact.clone());
            aliases.insert(compact.to_ascii_lowercase());
        }
        let compact_lc = compact.to_ascii_lowercase();
        for vendor in ["roland", "akai", "linndrum", "linn", "yamaha"] {
            if let Some(rest) = compact_lc.strip_prefix(vendor) {
                if !rest.is_empty() {
                    aliases.insert(rest.to_string());
                }
            }
        }
        aliases
    };
    let bank_variants = |bank: &str| -> Vec<String> {
        let mut variants: BTreeSet<String> = BTreeSet::new();
        match bank_case_style {
            BankCaseStyle::AsIs => {
                variants.insert(bank.to_string());
            }
            BankCaseStyle::Lowercase => {
                variants.insert(bank.to_ascii_lowercase());
            }
            BankCaseStyle::Both => {
                variants.insert(bank.to_string());
                variants.insert(bank.to_ascii_lowercase());
            }
        }
        variants.into_iter().collect()
    };

    for c in BANK_HINTS {
        out.push((c.to_string(), 90));
    }
    let real_banks = cached_real_banks();
    for real in &real_banks {
        for variant in bank_variants(&real) {
            out.push((variant, 220));
        }
        if bank_aliases_enabled {
            for alias in bank_aliases(&real) {
                out.push((alias, 210));
            }
        }
    }
    for seen in quoted_args_for_fn(text, &["bank"]) {
        // Do not suggest the exact value currently being typed.
        if !seen.eq_ignore_ascii_case(&inside) {
            out.push((seen, 120));
        }
    }

    // For bank completions we keep case variants distinct ("RolandTR909" and "rolandtr909")
    // when the user selected "Both" style, and use stricter matching (no subsequence fuzzy)
    // to avoid unrelated banks appearing.
    let bank_score = |prefix: &str, candidate: &str, source_boost: i32| -> i32 {
        if !prefix.is_empty() {
            if candidate == prefix {
                return 1200 + source_boost;
            }
            if candidate.starts_with(prefix) {
                let len_bonus = (40i32 - candidate.len() as i32).max(0);
                return 980 + len_bonus + source_boost;
            }
        }
        let p = prefix.to_ascii_lowercase();
        let c = candidate.to_ascii_lowercase();
        if p.is_empty() {
            return source_boost;
        }
        if c == p {
            return 1000 + source_boost;
        }
        if c.starts_with(&p) {
            let len_bonus = (40i32 - candidate.len() as i32).max(0);
            return 850 + len_bonus + source_boost;
        }
        if c.contains(&p) {
            return 500 + source_boost;
        }
        -1
    };

    let mut best_exact: HashMap<String, i32> = HashMap::new();
    for (item, source_boost) in out {
        let score = bank_score(&prefix, &item, source_boost);
        if score < 0 {
            continue;
        }
        let replace = match best_exact.get(&item) {
            Some(old) => score > *old,
            None => true,
        };
        if replace {
            best_exact.insert(item, score);
        }
    }
    let mut ranked: Vec<(String, i32)> = best_exact.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_ascii_lowercase().cmp(&b.0.to_ascii_lowercase()))
    });
    let items: Vec<String> = ranked.into_iter().map(|(s, _)| s).collect();
    Some((prefix_start_char, prefix, items))
}

fn try_chord_handler(text: &str, cursor_char: usize) -> Option<(usize, String, Vec<String>)> {
    let (_, _, inside) = ensure_quoted_arg_context(text, cursor_char, &["chord"])?;
    if inside.is_empty() && !is_inside_quoted_string(text, cursor_char) {
        return Some((cursor_char, String::new(), Vec::new()));
    }
    let fragment = trailing_fragment_by_predicate(&inside, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '#' | 'b' | '+' | '^' | ':' | '-')
    });
    let mut root_match: Option<String> = None;
    let mut symbol_fragment = fragment.clone();
    for p in pitch_literals() {
        if fragment
            .to_ascii_lowercase()
            .starts_with(&p.to_ascii_lowercase())
        {
            root_match = Some(p.clone());
            symbol_fragment = fragment[p.chars().count()..].to_string();
            break;
        }
    }
    if let Some(rest) = symbol_fragment.strip_prefix(':') {
        symbol_fragment = rest.to_string();
    }
    let prefix_start_char = cursor_char.saturating_sub(symbol_fragment.chars().count());
    let prefix = symbol_fragment.clone();
    let mut out: Vec<(String, i32)> = Vec::new();
    if root_match.is_some() {
        for c in chord_symbol_literals() {
            if c.to_ascii_lowercase()
                .starts_with(&symbol_fragment.to_ascii_lowercase())
            {
                out.push((c, 110));
            }
        }
    } else {
        for c in pitch_literals() {
            out.push((c, 100));
        }
    }
    let items = rank_and_dedup(&prefix, out);
    Some((prefix_start_char, prefix, items))
}

fn try_scale_handler(text: &str, cursor_char: usize) -> Option<(usize, String, Vec<String>)> {
    let (_, _, inside) = ensure_quoted_arg_context(text, cursor_char, &["scale"])?;
    if inside.is_empty() && !is_inside_quoted_string(text, cursor_char) {
        return Some((cursor_char, String::new(), Vec::new()));
    }
    let full_prefix = trailing_fragment_by_predicate(&inside, |c| {
        c.is_ascii_alphabetic() || matches!(c, '#' | 'b' | ':' | '_' | '-')
    });
    let (mut prefix, mut prefix_start_char) =
        if let Some((_, after_colon)) = full_prefix.rsplit_once(':') {
            (
                after_colon.to_string(),
                cursor_char.saturating_sub(after_colon.chars().count()),
            )
        } else {
            (
                full_prefix.clone(),
                cursor_char.saturating_sub(full_prefix.chars().count()),
            )
        };
    let mut out: Vec<(String, i32)> = Vec::new();
    let known_scale_names = cached_scale_names();
    let scale_boost = |name: &str| -> i32 {
        if SCALE_HINTS.iter().any(|h| h.eq_ignore_ascii_case(name)) {
            180
        } else {
            110
        }
    };
    if let Some((_, scale_part)) = inside.rsplit_once(':') {
        let query_after_root = inside
            .split_once(':')
            .map(|(_, rhs)| rhs)
            .unwrap_or(scale_part);
        for c in scale_candidates_for_query(query_after_root, &known_scale_names) {
            out.push((c.clone(), scale_boost(&c)));
        }
    } else {
        let token = inside.trim();
        if looks_like_pitch_token(token) {
            // Once tonic is present in `scale("C...")`, switch to scale-name hints.
            prefix.clear();
            prefix_start_char = cursor_char;
            for c in scale_candidates_for_query("", &known_scale_names) {
                out.push((format!(":{c}"), scale_boost(&c)));
            }
        } else {
            for c in pitch_literals() {
                out.push((c, 100));
            }
        }
    }
    let items = rank_and_dedup(&prefix, out);
    Some((prefix_start_char, prefix, items))
}

fn try_mode_handler(text: &str, cursor_char: usize) -> Option<(usize, String, Vec<String>)> {
    let (_, _, inside) = ensure_quoted_arg_context(text, cursor_char, &["mode", "anchor"])?;
    if inside.is_empty() && !is_inside_quoted_string(text, cursor_char) {
        return Some((cursor_char, String::new(), Vec::new()));
    }
    let full_prefix = trailing_fragment_by_predicate(&inside, |c| {
        c.is_ascii_alphanumeric() || c == '_' || c == ':'
    });
    let (mut prefix, mut prefix_start_char) =
        if let Some((_, after_colon)) = full_prefix.rsplit_once(':') {
            (
                after_colon.to_string(),
                cursor_char.saturating_sub(after_colon.chars().count()),
            )
        } else {
            (
                full_prefix.clone(),
                cursor_char.saturating_sub(full_prefix.chars().count()),
            )
        };
    let mut out: Vec<(String, i32)> = Vec::new();
    let known_modes: Vec<String> = mode_literals();
    if let Some((mode_part, anchor_part)) = inside.rsplit_once(':') {
        let mode_part = mode_part.trim();
        let mode_known = known_modes
            .iter()
            .any(|m| m.eq_ignore_ascii_case(mode_part));
        if mode_known {
            for c in pitch_literals() {
                if c.to_ascii_lowercase()
                    .starts_with(&anchor_part.to_ascii_lowercase())
                {
                    out.push((c, 110));
                }
            }
        } else {
            // If the text before ':' is not a valid mode yet (e.g. "a:"),
            // show all mode names so user can quickly pick one.
            for c in known_modes {
                out.push((c, 120));
            }
        }
    } else {
        let token = inside.trim();
        let mode_known = known_modes.iter().any(|m| m.eq_ignore_ascii_case(token));
        if mode_known {
            prefix.clear();
            prefix_start_char = cursor_char;
            for c in pitch_literals() {
                out.push((format!(":{c}"), 120));
            }
        } else {
            for c in known_modes {
                out.push((c, 100));
            }
        }
    }
    let items = rank_and_dedup(&prefix, out);
    Some((prefix_start_char, prefix, items))
}

fn try_voicing_handler(text: &str, cursor_char: usize) -> Option<(usize, String, Vec<String>)> {
    let (_, _, inside) = ensure_quoted_arg_context(text, cursor_char, &["voicing"])?;
    if inside.is_empty() && !is_inside_quoted_string(text, cursor_char) {
        return Some((cursor_char, String::new(), Vec::new()));
    }
    let fragment = trailing_fragment_by_predicate(&inside, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '#' | 'b' | '+' | '^' | ':' | '-')
    });
    let mut root_match: Option<String> = None;
    let mut symbol_fragment = fragment.clone();
    for p in pitch_literals() {
        if fragment
            .to_ascii_lowercase()
            .starts_with(&p.to_ascii_lowercase())
        {
            root_match = Some(p.clone());
            symbol_fragment = fragment[p.chars().count()..].to_string();
            break;
        }
    }
    if let Some(rest) = symbol_fragment.strip_prefix(':') {
        symbol_fragment = rest.to_string();
    }
    let prefix_start_char = cursor_char.saturating_sub(symbol_fragment.chars().count());
    let prefix = symbol_fragment.clone();
    let mut out: Vec<(String, i32)> = Vec::new();
    if root_match.is_some() {
        for c in chord_symbol_literals() {
            if c.to_ascii_lowercase()
                .starts_with(&symbol_fragment.to_ascii_lowercase())
            {
                out.push((c, 110));
            }
        }
    } else {
        for c in pitch_literals() {
            out.push((c, 100));
        }
    }
    let items = rank_and_dedup(&prefix, out);
    Some((prefix_start_char, prefix, items))
}

fn try_degree_handler(text: &str, cursor_char: usize) -> Option<(usize, String, Vec<String>)> {
    let (fragment_start_char, _, inside) =
        ensure_quoted_arg_context(text, cursor_char, &["degree"])?;
    if inside.is_empty() && !is_inside_quoted_string(text, cursor_char) {
        // degree usually expects numeric values; suppress noisy fallback before quote opens.
        return Some((fragment_start_char, String::new(), Vec::new()));
    }
    let prefix =
        trailing_fragment_by_predicate(&inside, |c| c.is_ascii_digit() || matches!(c, '-' | '+'));
    let prefix_start_char = cursor_char.saturating_sub(prefix.chars().count());
    let mut out: Vec<(String, i32)> = Vec::new();
    for c in ["0", "1", "2", "3", "4", "5", "6", "7", "-1", "-2"] {
        out.push((c.to_string(), 100));
    }
    let items = rank_and_dedup(&prefix, out);
    Some((prefix_start_char, prefix, items))
}

fn active_argument_spec(fn_spec: &FunctionSpec, arg_idx: usize) -> Option<ArgumentSpec> {
    fn_spec
        .arguments
        .get(arg_idx)
        .cloned()
        .or_else(|| fn_spec.arguments.last().cloned())
}

fn numeric_hint_values(arg: &ArgumentSpec) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(range) = &arg.range {
        match (range.min, range.max) {
            (Some(a), Some(b)) => {
                out.push(format!("{a}"));
                out.push(format!("{}", (a + b) / 2.0));
                out.push(format!("{b}"));
            }
            (Some(a), None) => out.push(format!("{a}")),
            (None, Some(b)) => out.push(format!("{b}")),
            (None, None) => {}
        }
    }
    if out.is_empty() {
        out.push("0".to_string());
        out.push("1".to_string());
    }
    out
}

fn type_hint_values(arg: &ArgumentSpec) -> Vec<String> {
    let mut out = Vec::new();
    let has = |t: &str| arg.arg_types.iter().any(|x| x.eq_ignore_ascii_case(t));
    if has("boolean") || has("bool") {
        out.push("true".to_string());
        out.push("false".to_string());
    }
    if has("number") || has("float") || has("int") {
        out.extend(numeric_hint_values(arg));
    }
    if has("string") && !arg.enum_values.is_empty() {
        out.extend(arg.enum_values.clone());
    }
    out.sort();
    out.dedup();
    out
}

fn try_index_argument_handler(
    text: &str,
    cursor_char: usize,
) -> Option<(usize, String, Vec<String>)> {
    let (fn_name, arg_idx) = active_function_call(text, cursor_char)?;
    let idx = get_language_index();
    let spec = idx.specs_by_name.get(&fn_name)?;
    let arg = active_argument_spec(spec, arg_idx)?;

    if is_inside_quoted_string(text, cursor_char) {
        let (fragment_start_char, fragment, _inside) = string_fragment(text, cursor_char)?;
        let mut entries: Vec<(String, i32)> = Vec::new();
        for item in &arg.enum_values {
            entries.push((item.clone(), 220));
        }
        // Fallback: if there are no explicit enum values, use parameter-name based literals.
        if entries.is_empty() {
            for item in index_special_literals(&arg.name.to_ascii_lowercase()) {
                entries.push((item, 190));
            }
        }
        let items = rank_and_dedup(&fragment, entries);
        if !items.is_empty() {
            return Some((fragment_start_char, fragment, items));
        }
        return None;
    }

    let (prefix_start_char, prefix) = extract_prefix(text, cursor_char);
    let mut entries: Vec<(String, i32)> = Vec::new();
    for item in type_hint_values(&arg) {
        entries.push((item, 170));
    }
    if entries.is_empty() && !arg.enum_values.is_empty() {
        for item in &arg.enum_values {
            entries.push((item.clone(), 130));
        }
    }
    let items = rank_and_dedup(&prefix, entries);
    if items.is_empty() {
        None
    } else {
        Some((prefix_start_char, prefix, items))
    }
}

pub fn collect_completions(
    text: &str,
    cursor_char: usize,
    all: &[String],
    doc_store: Option<&DocStore>,
    bank_case_style: &BankCaseStyle,
    bank_aliases_enabled: bool,
    semantic_mode: &SemanticHintsMode,
) -> (usize, String, Vec<String>) {
    if let Some((ps, p, items)) =
        collect_contextual_completions(text, cursor_char, bank_case_style, bank_aliases_enabled)
    {
        let mut items = items;
        apply_semantic_mode_filter(text, cursor_char, semantic_mode, &mut items);
        return (ps, p, items);
    }

    let (prefix_start_char, prefix) = extract_prefix(text, cursor_char);
    let mut ranked_entries: Vec<(String, i32)> = filter_completions(all, &prefix)
        .into_iter()
        .map(|s| (s, 20))
        .collect();
    for label in get_language_index().labels {
        ranked_entries.push((label, 30));
    }
    if let (Some((fn_name, _)), Some(store)) = (active_function_call(text, cursor_char), doc_store)
    {
        if let Some(doc) = store.get(&fn_name) {
            if let Some(params) = &doc.params {
                for p in params {
                    let label = p.name.trim();
                    if label.is_empty() {
                        continue;
                    }
                    ranked_entries.push((label.to_string(), 140));
                }
            }
        }
    }
    let mut items = rank_and_dedup(&prefix, ranked_entries);

    // Avoid noisy numeric fallback suggestions like "11"/"13" in non-chord contexts
    // (they come from chord symbol vocabulary but are confusing for calls like slow(1)).
    if let Some((fn_name, _)) = active_function_call(text, cursor_char) {
        if fn_name != "chord" {
            items.retain(|s| {
                let t = s.trim();
                !t.is_empty() && !t.chars().all(|c| c.is_ascii_digit())
            });
        }
    }

    apply_semantic_mode_filter(text, cursor_char, semantic_mode, &mut items);
    (prefix_start_char, prefix, items)
}

pub fn active_handler_name(text: &str, cursor_char: usize) -> Option<&'static str> {
    match detect_completion_context(text, cursor_char) {
        CompletionContext::StringMiniNotation => Some("string-mini-notation"),
        CompletionContext::NamedArgs => Some("named-args"),
        CompletionContext::FunctionCall => {
            if let Some((fn_name, _)) = active_function_call(text, cursor_char) {
                return match fn_name.as_str() {
                    "s" | "sound" => Some("sound"),
                    "bank" => Some("bank"),
                    "chord" => Some("chord"),
                    "scale" => Some("scale"),
                    "mode" | "anchor" => Some("mode"),
                    "voicing" => Some("voicing"),
                    "degree" => Some("degree"),
                    _ => Some("function-call"),
                };
            }
            Some("function-call")
        }
        CompletionContext::MethodChain => Some("method-chain"),
        CompletionContext::Global => {
            let (_, prefix) = extract_prefix(text, cursor_char);
            if prefix.is_empty() {
                None
            } else {
                Some("global")
            }
        }
    }
}

pub fn debug_context_label(text: &str, cursor_char: usize) -> Option<String> {
    let handler = active_handler_name(text, cursor_char)?;
    let mut label = format!("ctx: {}", handler);
    if let Some((fn_name, arg_idx)) = active_function_call(text, cursor_char) {
        label.push_str(&format!(" fn:{} arg:{}", fn_name, arg_idx));
    }
    if let Ok(c) = debug_counters().lock() {
        label.push_str(&format!(
            " no-suggest:{} strict-hidden:{}",
            c.no_suggestions, c.hidden_by_strict
        ));
    }
    Some(label)
}

pub fn semantic_warning_text(
    text: &str,
    cursor_char: usize,
    semantic_mode: &SemanticHintsMode,
) -> Option<String> {
    semantic_diagnostic(text, cursor_char, semantic_mode).map(|d| d.message)
}

pub fn semantic_diagnostic(
    text: &str,
    cursor_char: usize,
    semantic_mode: &SemanticHintsMode,
) -> Option<SemanticDiagnostic> {
    if matches!(semantic_mode, SemanticHintsMode::Off) {
        None
    } else {
        semantic_chain_warning(text, cursor_char)
    }
}

pub fn apply_semantic_quick_fix(full_text: &str, cursor_char: usize) -> Option<String> {
    let line = line_slice(full_text, cursor_char);
    reorder_source_before_control(line).map(|fixed_line| {
        let cursor_byte = char_to_byte(full_text, cursor_char);
        let left = full_text[..cursor_byte]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let right = full_text[cursor_byte..]
            .find('\n')
            .map(|r| cursor_byte + r)
            .unwrap_or(full_text.len());
        let mut out = String::new();
        out.push_str(&full_text[..left]);
        out.push_str(&fixed_line);
        out.push_str(&full_text[right..]);
        out
    })
}

pub fn call_hint_text(
    text: &str,
    cursor_char: usize,
    doc_store: Option<&DocStore>,
) -> Option<String> {
    let (fn_name, arg_idx) = active_function_call(text, cursor_char)?;
    let idx = get_language_index();
    if let Some(spec) = idx.specs_by_name.get(&fn_name) {
        if spec.arguments.is_empty() {
            return spec.signatures.first().cloned();
        }
        let clamped = arg_idx.min(spec.arguments.len().saturating_sub(1));
        let active = &spec.arguments[clamped];
        let mut hint = spec
            .signatures
            .first()
            .cloned()
            .unwrap_or_else(|| format!("{fn_name}()"));
        hint.push('\n');
        let ty = if active.arg_types.is_empty() {
            "unknown".to_string()
        } else {
            active.arg_types.join(" | ")
        };
        hint.push_str(&format!("{} : {}", active.name, ty));
        if let Some(range) = &active.range {
            if range.min.is_some() || range.max.is_some() {
                let min = range
                    .min
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".to_string());
                let max = range
                    .max
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".to_string());
                hint.push_str(&format!("  range: {}..{}", min, max));
            }
        }
        if !active.enum_values.is_empty() {
            let sample = active
                .enum_values
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            hint.push_str(&format!("  one of: {}", sample));
        }
        if active.optional {
            hint.push_str("  optional");
        }
        if let Some(default) = &active.default_value {
            hint.push_str(&format!("  default: {}", default));
        }
        return Some(hint);
    }

    let store = doc_store?;
    let doc = store.get(&fn_name)?;
    let params = doc.params.as_ref()?;
    if params.is_empty() {
        return None;
    }
    let clamped = arg_idx.min(params.len().saturating_sub(1));
    let active = &params[clamped];
    let mut hint = format!("{}(", fn_name);
    for (idx, p) in params.iter().enumerate() {
        if idx > 0 {
            hint.push_str(", ");
        }
        if idx == clamped {
            hint.push('[');
            hint.push_str(&p.name);
            hint.push(']');
        } else {
            hint.push_str(&p.name);
        }
    }
    hint.push(')');
    let param_type = active
        .r#type
        .as_ref()
        .and_then(|t| t.names.as_ref())
        .map(|names| names.join(" | "))
        .unwrap_or_else(|| "unknown".to_string());
    let param_desc = active
        .description
        .as_ref()
        .map(|d| html_to_plain(d).trim().to_string())
        .filter(|d| !d.is_empty())
        .unwrap_or_default();
    hint.push('\n');
    hint.push_str(&format!("{} : {}", active.name, param_type));
    if !param_desc.is_empty() {
        hint.push_str(" - ");
        hint.push_str(&param_desc);
    }
    if let Some(desc) = &active.description {
        if let Some((a, b)) = parse_numeric_bounds(desc) {
            hint.push_str(&format!("  range: {}..{}", a, b));
        }
    }
    Some(hint)
}

pub struct HoverDocMarkup {
    pub signature: String,
    pub description_markup: Option<String>,
}

pub fn hover_doc_markup(
    text: &str,
    hover_char: usize,
    doc_store: Option<&DocStore>,
) -> Option<HoverDocMarkup> {
    let store = doc_store?;
    let name = identifier_at_char(text, hover_char)?;
    let doc = store.get(&name)?;
    let description_markup = doc
        .description
        .as_ref()
        .map(|desc| desc.trim().to_string())
        .filter(|desc| !desc.is_empty());
    Some(HoverDocMarkup {
        signature: doc_signature(doc),
        description_markup,
    })
}

pub fn hover_doc_text(
    text: &str,
    hover_char: usize,
    doc_store: Option<&DocStore>,
) -> Option<String> {
    let hover = hover_doc_markup(text, hover_char, doc_store)?;
    let mut out = hover.signature;
    if let Some(desc) = hover.description_markup.as_ref() {
        let desc = truncate_with_ellipsis(&compact_doc_description(desc), 220);
        if !desc.is_empty() {
            out.push('\n');
            out.push_str(&desc);
        }
    }
    Some(out)
}

pub fn completion_apply_text(text: &str, cursor_char: usize, selected: &str) -> String {
    let Some((fn_name, _)) = active_function_call(text, cursor_char) else {
        return selected.to_string();
    };
    if fn_name == "chord" && selected.eq_ignore_ascii_case("major") {
        if let Some((_, _, inside)) = string_fragment(text, cursor_char) {
            let fragment = chord_tail_fragment(&inside);
            if fragment_has_chord_root(&fragment) {
                return String::new();
            }
        }
    }
    selected.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{BankCaseStyle, SemanticHintsMode};

    #[test]
    fn strict_filters_source_after_warning() {
        let text = "stack().gain(0.5).s(\"bd\")";
        let cursor = text.chars().count();
        let (start, prefix, items) = collect_completions(
            text,
            cursor,
            &["s".to_string(), "gain".to_string(), "cutoff".to_string()],
            None,
            &BankCaseStyle::Both,
            true,
            &SemanticHintsMode::Strict,
        );
        let _ = (start, prefix);
        assert!(!items.iter().any(|s| s.eq_ignore_ascii_case("s")));
    }

    #[test]
    fn warn_emits_semantic_message() {
        let text = "stack().gain(0.5).s(\"bd\")";
        let diag = semantic_diagnostic(text, text.chars().count(), &SemanticHintsMode::Warn);
        assert!(diag.is_some());
        assert!(diag.unwrap().message.contains("Suspicious transition"));
    }

    #[test]
    fn parses_limited_doc_markup() {
        let segments = parse_doc_markup("<p>Use <code>s(\"bd\")</code><br/>now</p>");
        assert_eq!(
            segments,
            vec![
                DocMarkupSegment::Text("Use ".to_string()),
                DocMarkupSegment::Code("s(\"bd\")".to_string()),
                DocMarkupSegment::LineBreak,
                DocMarkupSegment::Text("now".to_string()),
                DocMarkupSegment::ParagraphBreak,
            ]
        );
    }

    #[test]
    fn doc_markup_plain_compacts_supported_tags() {
        assert_eq!(
            doc_markup_to_plain("<p>A <code>b</code><br>C</p>"),
            "A b C "
        );
    }

    #[test]
    fn doc_markup_preserves_unknown_tags() {
        assert_eq!(
            parse_doc_markup("A <unknown x=\"1\">B</unknown> C"),
            vec![DocMarkupSegment::Text(
                "A <unknown x=\"1\">B</unknown> C".to_string()
            )]
        );
    }

    #[test]
    fn doc_markup_preserves_literal_angle_text() {
        assert_eq!(
            doc_markup_to_plain("gain < 1 and freq > 20"),
            "gain < 1 and freq > 20"
        );
    }

    #[test]
    fn doc_markup_preserves_unmatched_closing_code() {
        assert_eq!(
            parse_doc_markup("A </code> B"),
            vec![DocMarkupSegment::Text("A </code> B".to_string())]
        );
    }

    #[test]
    fn doc_markup_preserves_unmatched_closing_paragraph() {
        assert_eq!(
            parse_doc_markup("A </p> B"),
            vec![DocMarkupSegment::Text("A </p> B".to_string())]
        );
    }

    #[test]
    fn doc_markup_code_allows_literal_less_than() {
        assert_eq!(
            parse_doc_markup("<code>a < b</code>"),
            vec![DocMarkupSegment::Code("a < b".to_string())]
        );
    }

    #[test]
    fn doc_markup_preserves_spaced_supported_tag_like_text() {
        assert_eq!(
            parse_doc_markup("< code >x</code>"),
            vec![DocMarkupSegment::Text("< code >x</code>".to_string())]
        );
    }

    #[test]
    fn doc_markup_preserves_uppercase_supported_tag_like_text() {
        assert_eq!(
            parse_doc_markup("<CODE>x</CODE>"),
            vec![DocMarkupSegment::Text("<CODE>x</CODE>".to_string())]
        );
    }

    #[test]
    fn collect_unknown_sound_token_in_mini_notation() {
        let text = "sound(`bd впываbd`)";
        let problems = collect_sound_token_problems(text);
        assert!(problems
            .iter()
            .any(|p| p.message.contains("Unknown sample token")));
    }

    #[test]
    fn no_problem_for_known_sound_prefix_while_typing() {
        let text = "sound(`b`)";
        let problems = collect_sound_token_problems(text);
        assert!(problems.is_empty());
    }

    #[test]
    fn warn_lists_multiple_unknown_samples() {
        let text = "sound(`bd zzkick zzsn`)";
        let problems = collect_sound_token_problems(text);
        let all = problems
            .iter()
            .map(|p| p.message.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(all.contains("zzkick"));
    }

    #[test]
    fn collect_unknown_function_call_problem() {
        let text = "soudwnd(`bd`)";
        let problems = collect_function_call_problems(text);
        assert!(problems
            .iter()
            .any(|p| p.message.contains("Unknown function `soudwnd`")));
    }

    #[test]
    fn collect_missing_required_arguments_problem() {
        let idx = get_language_index();
        // Ordre déterministe : iter() sur HashMap donnait un candidat aléatoire,
        // dont certains ne produisent pas l'avertissement (test instable).
        let mut names: Vec<String> = idx
            .specs_by_name
            .iter()
            .filter(|(name, s)| {
                name.chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
                    && name
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
                    && s.arguments
                        .iter()
                        .any(|a| !a.optional && a.default_value.is_none())
            })
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        let warned: Vec<String> = names
            .iter()
            .filter(|name| {
                let problems = collect_function_call_problems(&format!("{name}()"));
                problems
                    .iter()
                    .any(|p| p.message.contains("expects at least"))
            })
            .cloned()
            .collect();
        assert!(
            !warned.is_empty(),
            "aucune fonction avec argument requis ne produit d'avertissement \
             (candidats testés : {names:?})"
        );
    }

    #[test]
    fn quick_fix_handles_nested_parentheses() {
        let line = "stack().gain(add(0.1, mul(0.2, 3))).s(\"bd\")";
        let fixed = reorder_source_before_control(line).expect("must produce fix");
        assert!(fixed.find(".s(\"bd\")").unwrap_or(usize::MAX) < fixed.find(".gain(").unwrap_or(0));
    }

    #[test]
    fn scale_suggests_modes_after_root_before_colon() {
        let text = "scale(\"C\")";
        let cursor = text.find("C").unwrap_or(0) + 1;
        let (start, prefix, items) = collect_completions(
            text,
            cursor,
            &[],
            None,
            &BankCaseStyle::Both,
            true,
            &SemanticHintsMode::Off,
        );
        assert_eq!(start, cursor);
        assert!(prefix.is_empty());
        assert!(items.iter().any(|s| s == ":major"));
        assert!(items.iter().all(|s| !s.eq_ignore_ascii_case("A")));
    }

    #[test]
    fn scale_candidates_follow_nested_chain() {
        let names = vec![
            "major".to_string(),
            "minor".to_string(),
            "major:blues".to_string(),
            "minor:pentatonic".to_string(),
        ];
        let root = scale_candidates_for_query("", &names);
        assert!(root.iter().any(|s| s == "major"));
        assert!(root.iter().any(|s| s == "minor"));

        let after_major = scale_candidates_for_query("major:", &names);
        assert!(after_major.iter().any(|s| s == "blues"));
        assert!(!after_major.iter().any(|s| s == "minor"));
    }

    #[test]
    fn scale_after_colon_avoids_pitch_suggestions() {
        let text = "scale(\"C:\")";
        let cursor = text.find(':').unwrap_or(0) + 1;
        let (_, _, items) = collect_completions(
            text,
            cursor,
            &[],
            None,
            &BankCaseStyle::Both,
            true,
            &SemanticHintsMode::Off,
        );
        assert!(items.iter().any(|s| s.eq_ignore_ascii_case("major")));
        assert!(!items.iter().any(|s| s.eq_ignore_ascii_case("A")));
    }

    #[test]
    fn chord_symbol_suggestions_work_without_colon() {
        let text = "chord(\"C\")";
        let cursor = text.find("C").unwrap_or(0) + 1;
        let (_, _, items) = collect_completions(
            text,
            cursor,
            &[],
            None,
            &BankCaseStyle::Both,
            true,
            &SemanticHintsMode::Off,
        );
        assert!(!items.is_empty(), "items={items:?}");
        assert!(!items.iter().any(|s| s.eq_ignore_ascii_case("A")));
    }

    #[test]
    fn mode_suggests_anchor_after_valid_mode() {
        let text = "mode(\"below\")";
        let cursor = text.find("below").unwrap_or(0) + "below".len();
        let (_, _, items) = collect_completions(
            text,
            cursor,
            &[],
            None,
            &BankCaseStyle::Both,
            true,
            &SemanticHintsMode::Off,
        );
        assert!(items.iter().any(|s| s == ":C"));
        assert!(!items.iter().any(|s| s.eq_ignore_ascii_case("below")));
    }

    #[test]
    fn anchor_alias_uses_mode_handler() {
        let text = "anchor(\"below\")";
        let cursor = text.find("below").unwrap_or(0) + "below".len();
        let (_, _, items) = collect_completions(
            text,
            cursor,
            &[],
            None,
            &BankCaseStyle::Both,
            true,
            &SemanticHintsMode::Off,
        );
        assert!(items.iter().any(|s| s == ":C"));
    }

    #[test]
    fn voicing_suggests_chord_symbols_after_root() {
        let text = "voicing(\"C\")";
        let cursor = text.find("C").unwrap_or(0) + 1;
        let (_, _, items) = collect_completions(
            text,
            cursor,
            &[],
            None,
            &BankCaseStyle::Both,
            true,
            &SemanticHintsMode::Off,
        );
        assert!(!items.is_empty());
        assert!(!items.iter().any(|s| s.eq_ignore_ascii_case("A")));
    }

    #[test]
    fn generic_index_handler_suggests_numeric_range_values() {
        let text = "slider(0.";
        let cursor = text.chars().count();
        let (_, _, items) = collect_completions(
            text,
            cursor,
            &[],
            None,
            &BankCaseStyle::Both,
            true,
            &SemanticHintsMode::Off,
        );
        assert!(
            items.iter().any(|s| s == "0" || s == "1" || s == "0.5"),
            "items={items:?}"
        );
    }

    #[test]
    fn call_hint_prefers_language_index_signature() {
        let text = "slider(0, 0.1";
        let cursor = text.chars().count();
        let hint = call_hint_text(text, cursor, None).unwrap_or_default();
        assert!(hint.contains("slider("), "hint={hint}");
        assert!(hint.contains("min : number"), "hint={hint}");
    }

    #[test]
    fn named_args_context_suggests_argument_keys() {
        let text = "slider({ va";
        let cursor = text.chars().count();
        let (_, _, items) = collect_completions(
            text,
            cursor,
            &[],
            None,
            &BankCaseStyle::Both,
            true,
            &SemanticHintsMode::Off,
        );
        assert!(
            items.iter().any(|s| s.eq_ignore_ascii_case("value:")),
            "items={items:?}"
        );
    }

    #[test]
    fn fallback_completion_is_not_capped_to_thirty_items() {
        let text = "a";
        let cursor = text.chars().count();
        let all: Vec<String> = (0..80).map(|i| format!("a_item_{i:02}")).collect();
        let (_, _, items) = collect_completions(
            text,
            cursor,
            &all,
            None,
            &BankCaseStyle::Both,
            true,
            &SemanticHintsMode::Off,
        );
        assert!(items.len() > 30, "len={} items={items:?}", items.len());
    }

    #[test]
    fn sound_history_does_not_suggest_full_mini_notation_lines() {
        let text = "sound(\"bd ~ ~ hh@2\")\nsound(\"b\")";
        let cursor = text.rfind("\"b").unwrap_or(0) + 2;
        let (_, _, items) = collect_completions(
            text,
            cursor,
            &[],
            None,
            &BankCaseStyle::Both,
            true,
            &SemanticHintsMode::Off,
        );
        assert!(!items.iter().any(|s| s == "bd ~ ~ hh@2"), "items={items:?}");
    }
}
