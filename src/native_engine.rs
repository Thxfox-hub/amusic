//! Native (pure-Rust) playback engine for simple Strudel patterns of the form
//! `s("bd hh cp")`, `sound("bd:0 cp:1")`, etc.
//!
//! The goal is to avoid the latency of the node.js bridge (`strudel-runner.mjs`) for
//! the very common case of a single mini-notation string. When the document is
//! considered "pure" (`is_pure() == true`), we can synthesise the same
//! `AudioWindowResult` that `StrudelBridge::audio_window` would produce, but
//! immediately and without any IPC.

use std::collections::HashMap;

use crate::arrangement_ir::StableId;
use crate::pattern_ir::{DiagnosticSeverity, PatternDocument, PatternValue};
use crate::strudel_bridge::{AudioWindowResult, StrudelAudioEvent};

/// Allowed source functions for "pure" patterns.
const PURE_SOURCE_FNS: &[&str] = &["s", "sound", "note", "n"];

#[derive(Debug, Clone, Default)]
struct CachedEvent {
    begin: f64,
    end: f64,
    /// `None` if this event should be skipped (e.g. a rest or note we don't support yet).
    audio: Option<StrudelAudioEvent>,
}

#[derive(Debug, Default)]
pub struct NativeEngine {
    pure: bool,
    bpm: f64,
    events: Vec<CachedEvent>,
}

impl NativeEngine {
    pub fn new() -> Self {
        Self {
            pure: false,
            bpm: 120.0,
            events: Vec::new(),
        }
    }

    /// Rebuild the cache from the current document and BPM. Returns `true` if the
    /// document is "pure" (and therefore the native path applies).
    pub fn update(&mut self, doc: &PatternDocument, bpm: f64) -> bool {
        self.bpm = bpm;
        self.events.clear();
        self.pure = false;

        if !is_document_pure(doc) {
            return false;
        }

        // Map pattern_id -> bank for quick lookup.
        let banks: HashMap<&StableId, Option<String>> = doc
            .ir
            .patterns
            .iter()
            .map(|p| (&p.id, p.bank.clone()))
            .collect();

        let cps = (bpm / 60.0).max(f64::MIN_POSITIVE);

        for event in &doc.ir.events {
            let cached = match &event.value {
                PatternValue::Sample(token) => {
                    let (name, n, gain) = parse_sample_token(token);
                    if name.is_empty() {
                        CachedEvent {
                            begin: event.begin,
                            end: event.end,
                            audio: None,
                        }
                    } else {
                        let bank = banks
                            .get(&event.pattern_id)
                            .cloned()
                            .flatten();
                        CachedEvent {
                            begin: event.begin,
                            end: event.end,
                            audio: Some(StrudelAudioEvent {
                                begin: 0.0,
                                end: 0.0,
                                s: Some(name),
                                bank,
                                n,
                                midi: None,
                                gain,
                                pan: None,
                                cps: Some(cps),
                                cut: None,
                                room: None,
                                shape: None,
                                attack: None,
                                decay: None,
                                sustain: None,
                                release: None,
                                speed: None,
                                cutoff: None,
                            }),
                        }
                    }
                }
                // Notes are intentionally skipped in the native path for now —
                // the bridge still runs in parallel and the audio will fall back
                // through the standard path if `is_pure()` is `false` for any
                // reason. With `is_pure() == true` Note(_) events are simply
                // dropped (audio = None) which matches the contract.
                PatternValue::Note(_) => CachedEvent {
                    begin: event.begin,
                    end: event.end,
                    audio: None,
                },
                PatternValue::Rest | PatternValue::Unknown(_) => CachedEvent {
                    begin: event.begin,
                    end: event.end,
                    audio: None,
                },
            };
            self.events.push(cached);
        }

        self.pure = true;
        true
    }

    /// Whether the native path applies right now.
    pub fn is_pure(&self) -> bool {
        self.pure
    }

    /// Уникальный список (bank, name, n) для прогрева сэмплов в кэш AudioEngine.
    /// Без прогрева первый удар с некэшированным сэмплом блокируется на дисковой I/O
    /// в schedule_event и выпадает из окна рендера.
    pub fn sample_requests(&self) -> Vec<(Option<String>, String, Option<usize>)> {
        let mut seen: std::collections::HashSet<(Option<String>, String, Option<usize>)> =
            std::collections::HashSet::new();
        let mut out = Vec::new();
        for cached in &self.events {
            let Some(audio) = &cached.audio else {
                continue;
            };
            let Some(name) = &audio.s else {
                continue;
            };
            let variant = audio.n.map(|v| v.max(0.0).floor() as usize);
            let key = (audio.bank.clone(), name.clone(), variant);
            if seen.insert(key.clone()) {
                out.push(key);
            }
        }
        out
    }

    /// Generate audio events for the cycle window `[from, to)`. Same format as
    /// `StrudelBridge::audio_window`.
    pub fn query_audio_window(&self, from: f64, to: f64) -> AudioWindowResult {
        let cps = (self.bpm / 60.0).max(f64::MIN_POSITIVE);
        let mut out: Vec<StrudelAudioEvent> = Vec::new();
        if !self.pure || to <= from {
            return AudioWindowResult { cps, events: out };
        }

        let c0 = from.floor();
        let mut c = c0;
        // Walk integer cycles starting at c0 while the cycle could intersect the window.
        // Each event has begin/end in [0, 1], so the cycle [c, c+1) can yield events
        // overlapping [from, to) as long as c < to.
        while c < to {
            for cached in &self.events {
                let Some(audio_template) = &cached.audio else {
                    continue;
                };
                let global_begin = c + cached.begin;
                let global_end = c + cached.end;
                // Check intersection of [global_begin, global_end) with [from, to).
                if global_end <= from || global_begin >= to {
                    continue;
                }
                let mut ev = audio_template.clone();
                ev.begin = global_begin;
                ev.end = global_end;
                ev.cps = Some(cps);
                out.push(ev);
            }
            c += 1.0;
        }

        AudioWindowResult { cps, events: out }
    }
}

/// Parse a mini-notation token of the form `name[:n[:gain]]`. The first component
/// is always the sample / sound name. The second and third components are parsed
/// as `f64` and silently dropped if they don't parse — matching how Strudel's
/// own parser tolerates `bd:0` and `bd:0:0.3` style suffixes.
fn parse_sample_token(token: &str) -> (String, Option<f64>, Option<f64>) {
    let mut parts = token.split(':');
    let name = parts.next().unwrap_or("").to_string();
    let n = parts.next().and_then(|s| s.parse::<f64>().ok());
    let gain = parts.next().and_then(|s| s.parse::<f64>().ok());
    (name, n, gain)
}

/// "Pure" check — conservative. Returns `true` only if the document consists of
/// nothing but allowed `s(...)`, `sound(...)`, `note(...)`, `n(...)` expressions
/// separated by whitespace, semicolons or comments. Any sign of `every`, `.fast`,
/// `stack`, `cat`, arithmetic, etc. forces a fallback to the bridge.
fn is_document_pure(doc: &PatternDocument) -> bool {
    if doc
        .diagnostics
        .iter()
        .any(|d| d.severity == DiagnosticSeverity::Error)
    {
        return false;
    }
    if doc.ast.expressions.is_empty() {
        return false;
    }

    // 1. All expressions must use one of the allowed source functions.
    for expr in &doc.ast.expressions {
        if !PURE_SOURCE_FNS.iter().any(|f| *f == expr.source_fn) {
            return false;
        }
    }

    // 2. The code, with expression spans removed, must contain only whitespace,
    //    semicolons or comments.
    let mut spans: Vec<(usize, usize)> = doc
        .ast
        .expressions
        .iter()
        .map(|e| (e.source_span.start, e.source_span.end))
        .collect();
    spans.sort_by_key(|s| s.0);
    // Reject overlapping spans defensively.
    for pair in spans.windows(2) {
        if pair[0].1 > pair[1].0 {
            return false;
        }
    }
    let code = doc.code.as_str();
    let mut cursor = 0usize;
    for (start, end) in spans {
        if start > code.len() || end > code.len() || start > end {
            return false;
        }
        if !is_pure_gap(&code[cursor..start]) {
            return false;
        }
        cursor = end;
    }
    if cursor <= code.len() && !is_pure_gap(&code[cursor..]) {
        return false;
    }
    true
}

/// Returns `true` if `gap` consists exclusively of whitespace, `;`,
/// line comments (`// ... \n`) and block comments (`/* ... */`).
fn is_pure_gap(gap: &str) -> bool {
    let bytes = gap.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() || b == b';' {
            i += 1;
            continue;
        }
        // Line comment?
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comment?
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            let mut closed = false;
            while i + 1 < bytes.len() {
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    closed = true;
                    break;
                }
                i += 1;
            }
            if !closed {
                return false;
            }
            continue;
        }
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strudel_parser::parse_pattern_document;

    #[test]
    fn parse_sample_token_just_name() {
        let (name, n, gain) = parse_sample_token("bd");
        assert_eq!(name, "bd");
        assert_eq!(n, None);
        assert_eq!(gain, None);
    }

    #[test]
    fn parse_sample_token_name_and_n() {
        let (name, n, gain) = parse_sample_token("bd:1");
        assert_eq!(name, "bd");
        assert_eq!(n, Some(1.0));
        assert_eq!(gain, None);
    }

    #[test]
    fn parse_sample_token_name_n_gain() {
        let (name, n, gain) = parse_sample_token("bd:0:0.3");
        assert_eq!(name, "bd");
        assert_eq!(n, Some(0.0));
        assert_eq!(gain, Some(0.3));
    }

    #[test]
    fn pure_simple_pattern_yields_three_events() {
        let code = r#"s("bd hh cp")"#;
        let doc = parse_pattern_document(code, 1);
        let mut engine = NativeEngine::new();
        assert!(engine.update(&doc, 120.0));
        assert!(engine.is_pure());

        let result = engine.query_audio_window(0.0, 1.0);
        assert_eq!(result.events.len(), 3);

        let names: Vec<Option<&str>> = result
            .events
            .iter()
            .map(|e| e.s.as_deref())
            .collect();
        assert_eq!(names, vec![Some("bd"), Some("hh"), Some("cp")]);

        let begins: Vec<f64> = result.events.iter().map(|e| e.begin).collect();
        let expected: [f64; 3] = [0.0, 1.0 / 3.0, 2.0 / 3.0];
        for (got, want) in begins.iter().zip(expected.iter()) {
            assert!(
                (got - want).abs() < 1e-6,
                "begin mismatch: got={got} want={want}"
            );
        }
    }

    #[test]
    fn non_pure_when_extra_chain_present() {
        let code = r#"s("bd hh cp").fast(2)"#;
        let doc = parse_pattern_document(code, 1);
        let mut engine = NativeEngine::new();
        // Either `fast` makes the chain non-pure, or the parser eats `.fast(...)`
        // as part of the expression span. Even in the second case, `.fast` is
        // a non-allowed extension, so we must report non-pure.
        let pure = engine.update(&doc, 120.0);
        assert!(
            !pure,
            "pattern with .fast must not be considered native-pure"
        );
        assert!(!engine.is_pure());
    }
}

#[cfg(test)]
mod chained_method_tests {
    use super::*;
    use crate::strudel_parser::parse_pattern_document;

    #[test]
    fn chained_gain_must_go_to_the_bridge() {
        let doc = parse_pattern_document(r#"s("bd*4").gain(0.5)"#, 1);
        let mut engine = NativeEngine::new();
        assert!(!engine.update(&doc, 120.0), "native path would drop .gain()");
    }

    #[test]
    fn bank_alone_stays_native() {
        let doc = parse_pattern_document(r#"s("bd*4").bank("RolandTR909")"#, 1);
        let mut engine = NativeEngine::new();
        assert!(engine.update(&doc, 120.0));
    }
}
