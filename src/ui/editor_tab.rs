//! Редактор кода Strudel: многострочный, с номерами строк, вертикальным и горизонтальным скроллом,
//! Tab / Shift+Tab для отступов (когда автодополнение закрыто), подсветкой сэмплов и автодополнением.

use eframe::egui;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::doc_json::DocStore;
use crate::settings::IdeSettings;
use crate::strudel_language_index::{get_language_index, ApiCategory};
use crate::ui::debugger::SourceBreakpointKey;
use crate::ui::DebuggerState;

const COMPLETION_CATEGORIZATION_ENABLED: bool = false;
const BP_GUTTER_W: f32 = 18.0;

pub enum EditorAction {
    ToggleSourceBreakpoint(usize),
    PlayExample(String),
    StopExample,
    /// Un widget `slider(…)` inline a réécrit le code : propager + ré-évaluer si ça joue.
    ApplyLiveCode,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActiveSampleEvent {
    pub s: String,
    pub begin: f64,
    pub end: f64,
    pub n: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExamplePlaybackState {
    active: Option<String>,
}

impl ExamplePlaybackState {
    pub fn active(&self) -> Option<&str> {
        self.active.as_deref()
    }

    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub fn mark_started(&mut self, code: String) {
        self.active = Some(code);
    }

    pub fn clear(&mut self) {
        self.active = None;
    }
}

pub fn should_tick_arrangement_for_example_state(example_playback: &ExamplePlaybackState) -> bool {
    !example_playback.is_active()
}

pub fn should_stop_example_on_completion_close(
    completion_was_open: bool,
    completion_is_open: bool,
    example_playback: &ExamplePlaybackState,
) -> bool {
    completion_was_open && !completion_is_open && example_playback.is_active()
}

struct CompletionState {
    prefix: String,
    items: Vec<String>,
    tree_enabled: bool,
    item_groups: Vec<String>,
    groups: Vec<String>,
    active_group: usize,
    item_subgroups: Vec<String>,
    subgroups: Vec<String>,
    active_subgroup: usize,
    nav_level: u8, // 0 = categories, 1 = subcategories
    selected: usize,
    pending_scroll_align: Option<egui::Align>,
    anchor: egui::Pos2,
    prefix_start_char: usize,
    prefix_end_char: usize,
}

#[derive(Default)]
struct SoundBankLookupCache {
    by_sound: Arc<HashMap<String, String>>,
    last_refresh: Option<Instant>,
}

#[derive(Default)]
struct FindReplaceState {
    visible: bool,
    replace_mode: bool,
    case_sensitive: bool,
    query: String,
    replacement: String,
    matches: Vec<(usize, usize)>,
    active: usize,
    focus_search_requested: bool,
}

pub struct EditorTab {
    pub code: String,
    pub file_path: Option<std::path::PathBuf>,
    pub eval_error: Option<String>,
    pub eval_error_line: Option<usize>,
    pub eval_error_column: Option<usize>,
    pub run_requested: bool,
    pub load_samples_requested: bool,
    pub flash_until: Option<Instant>,
    completion: Option<CompletionState>,
    all_words: Vec<String>,
    find_replace: FindReplaceState,
    last_cursor_char: usize,
}

impl EditorTab {
    fn doc_markup_layout_job(markup: &str) -> egui::text::LayoutJob {
        let mut job = egui::text::LayoutJob::default();
        let normal = egui::TextFormat::default();
        let code = egui::TextFormat {
            font_id: egui::FontId::monospace(12.0),
            background: egui::Color32::from_gray(38),
            ..Default::default()
        };
        for segment in crate::strudel_completion_engine::parse_doc_markup(markup) {
            match segment {
                crate::strudel_completion_engine::DocMarkupSegment::Text(text) => {
                    job.append(&text, 0.0, normal.clone())
                }
                crate::strudel_completion_engine::DocMarkupSegment::Code(text) => {
                    job.append(&text, 0.0, code.clone())
                }
                crate::strudel_completion_engine::DocMarkupSegment::LineBreak => {
                    job.append("\n", 0.0, normal.clone())
                }
                crate::strudel_completion_engine::DocMarkupSegment::ParagraphBreak => {
                    job.append("\n\n", 0.0, normal.clone())
                }
            }
        }
        job
    }

    fn doc_markup_label(ui: &mut egui::Ui, markup: &str) {
        let mut job = Self::doc_markup_layout_job(markup);
        job.wrap.max_width = ui.available_width();
        ui.label(job);
    }

    fn example_layout_job(
        example: &str,
        active: bool,
        playing_events: &[ActiveSampleEvent],
    ) -> egui::text::LayoutJob {
        let mut job = egui::text::LayoutJob::default();
        let normal = egui::TextFormat {
            font_id: egui::FontId::monospace(12.0),
            ..Default::default()
        };
        let highlighted = egui::TextFormat {
            font_id: egui::FontId::monospace(12.0),
            background: egui::Color32::from_rgba_premultiplied(60, 140, 255, 70),
            underline: egui::Stroke::new(1.5, egui::Color32::from_rgb(100, 170, 255)),
            ..Default::default()
        };
        let mut ranges = if active {
            Self::find_example_active_ranges(example, playing_events)
        } else {
            Vec::new()
        };
        ranges.sort_unstable();
        ranges.dedup();
        let mut cursor = 0;
        for (start, end) in ranges {
            let start = start.min(example.len());
            let end = end.min(example.len());
            if start >= end || start < cursor {
                continue;
            }
            if cursor < start {
                job.append(&example[cursor..start], 0.0, normal.clone());
            }
            job.append(&example[start..end], 0.0, highlighted.clone());
            cursor = end;
        }
        if cursor < example.len() {
            job.append(&example[cursor..], 0.0, normal);
        }
        job
    }

    fn example_row(
        ui: &mut egui::Ui,
        example: &str,
        example_playback: &ExamplePlaybackState,
        example_playing_events: &[ActiveSampleEvent],
        editor_actions: &mut Vec<EditorAction>,
    ) {
        let active = example_playback.active() == Some(example);
        ui.horizontal(|ui| {
            let label = if active { "⏹" } else { "▶" };
            let tooltip = if active {
                "Stop example"
            } else {
                "Play example"
            };
            if ui.button(label).on_hover_text(tooltip).clicked() {
                if active {
                    editor_actions.push(EditorAction::StopExample);
                } else {
                    editor_actions.push(EditorAction::PlayExample(example.to_string()));
                }
            }
            ui.label(Self::example_layout_job(
                example,
                active,
                example_playing_events,
            ));
        });
    }

    fn parse_usize_token(token: &str) -> Option<usize> {
        let digits: String = token.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return None;
        }
        digits.parse::<usize>().ok().filter(|n| *n > 0)
    }

    fn parse_line_after_keyword(lower_err: &str, keyword: &str) -> Option<usize> {
        let idx = lower_err.find(keyword)?;
        let tail = &lower_err[idx + keyword.len()..];
        let digits: String = tail
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if digits.is_empty() {
            return None;
        }
        digits.parse::<usize>().ok().filter(|n| *n > 0)
    }

    fn parse_line_from_stack_like_token(token: &str) -> Option<usize> {
        let cleaned = token.trim_matches(|c| matches!(c, ')' | '(' | ',' | ';'));
        let mut parts = cleaned.rsplit(':');
        let col = parts.next().and_then(Self::parse_usize_token);
        let line = parts.next().and_then(Self::parse_usize_token);
        if col.is_some() && line.is_some() {
            return line;
        }
        None
    }

    fn runtime_error_line_number(error: &str) -> Option<usize> {
        for token in error.split_whitespace() {
            if let Some(line) = Self::parse_line_from_stack_like_token(token) {
                return Some(line);
            }
        }
        let lower = error.to_ascii_lowercase();
        Self::parse_line_after_keyword(&lower, "line ")
            .or_else(|| Self::parse_line_after_keyword(&lower, "line:"))
    }

    fn line_start_char_for_line(text: &str, line_number_1_based: usize) -> Option<usize> {
        if line_number_1_based == 0 {
            return None;
        }
        if line_number_1_based == 1 {
            return Some(0);
        }
        let mut line = 1usize;
        for (byte_idx, ch) in text.char_indices() {
            if ch == '\n' {
                line += 1;
                if line == line_number_1_based {
                    return Some(Self::byte_to_char(text, byte_idx + 1));
                }
            }
        }
        None
    }

    fn char_index_for_line_column(
        text: &str,
        line_number_1_based: usize,
        column_number_1_based: usize,
    ) -> Option<usize> {
        let line_start = Self::line_start_char_for_line(text, line_number_1_based)?;
        let line_end = Self::line_end_char(text, line_start);
        if line_end <= line_start {
            return Some(line_start);
        }
        let col = column_number_1_based.saturating_sub(1);
        Some((line_start + col).min(line_end))
    }

    fn cached_sound_bank_lookup() -> Arc<HashMap<String, String>> {
        static CACHE: OnceLock<Mutex<SoundBankLookupCache>> = OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(SoundBankLookupCache::default()));
        let mut guard = match cache.lock() {
            Ok(g) => g,
            Err(_) => return Arc::new(HashMap::new()),
        };
        let stale = guard
            .last_refresh
            .map(|t| t.elapsed() > Duration::from_secs(10))
            .unwrap_or(true);
        if stale {
            let root = crate::sample_loader::default_sample_root();
            let by_bank = crate::sample_loader::available_sounds_by_canonical_bank(&root);
            let mut by_sound: HashMap<String, String> = HashMap::new();
            for (bank, sounds) in by_bank {
                for sound in sounds {
                    by_sound
                        .entry(sound.to_ascii_lowercase())
                        .or_insert_with(|| bank.clone());
                }
            }
            guard.by_sound = Arc::new(by_sound);
            guard.last_refresh = Some(Instant::now());
        }
        guard.by_sound.clone()
    }

    fn bank_tree_group(bank: &str) -> String {
        bank.to_ascii_lowercase()
    }

    fn function_category_key(
        name: &str,
        categories_by_name: &HashMap<String, ApiCategory>,
    ) -> String {
        let cat = categories_by_name
            .get(name)
            .copied()
            .or_else(|| categories_by_name.get(&name.to_ascii_lowercase()).copied())
            .unwrap_or(ApiCategory::Unknown);
        match cat {
            ApiCategory::Source => "source",
            ApiCategory::Structure => "structure",
            ApiCategory::Time => "time",
            ApiCategory::Effect => "effect",
            ApiCategory::Tonal => "tonal",
            ApiCategory::Control => "control",
            ApiCategory::Unknown => "other",
        }
        .to_string()
    }

    fn group_label(key: &str) -> String {
        match key {
            "source" => "Source",
            "structure" => "Structure",
            "time" => "Time",
            "effect" => "Effect",
            "tonal" => "Tonal",
            "control" => "Control",
            "other" => "Other",
            _ => key,
        }
        .to_string()
    }

    fn build_completion_groups(
        items: &[String],
        handler: Option<&str>,
    ) -> (bool, Vec<String>, Vec<String>) {
        if !COMPLETION_CATEGORIZATION_ENABLED {
            return (
                false,
                vec!["all".to_string(); items.len()],
                vec!["all".to_string()],
            );
        }
        let mut item_groups = Vec::with_capacity(items.len());
        let mut groups: BTreeSet<String> = BTreeSet::new();
        let tree_enabled = matches!(handler, Some("bank" | "sound" | "string-mini-notation"));
        let function_category_map =
            if matches!(handler, Some("bank" | "sound" | "string-mini-notation")) {
                None
            } else {
                let idx = get_language_index();
                let mut by_name = HashMap::new();
                for (name, spec) in idx.specs_by_name {
                    by_name.insert(name, spec.category);
                }
                Some(by_name)
            };
        let sound_lookup = if matches!(handler, Some("sound" | "string-mini-notation")) {
            Some(Self::cached_sound_bank_lookup())
        } else {
            None
        };
        for item in items {
            let key = match handler {
                Some("bank") => Self::bank_tree_group(item),
                Some("sound" | "string-mini-notation") => {
                    let token = item.split(':').next().unwrap_or(item);
                    let bank = sound_lookup
                        .as_ref()
                        .and_then(|m| m.get(&token.to_ascii_lowercase()).cloned());
                    if let Some(bank) = bank {
                        Self::bank_tree_group(&bank)
                    } else {
                        Self::bank_tree_group(token)
                    }
                }
                _ => function_category_map
                    .as_ref()
                    .map(|m| Self::function_category_key(item, m))
                    .unwrap_or_else(|| "other".to_string()),
            };
            groups.insert(key.clone());
            item_groups.push(key);
        }

        let mut ordered: Vec<String> = groups.into_iter().collect();
        if !tree_enabled {
            let preferred = [
                "source",
                "structure",
                "time",
                "effect",
                "tonal",
                "control",
                "other",
            ];
            ordered.sort_by_key(|k| {
                preferred
                    .iter()
                    .position(|p| p == k)
                    .unwrap_or(preferred.len() + 100)
            });
        }
        (tree_enabled, item_groups, ordered)
    }

    fn filtered_completion_indices(cs: &CompletionState) -> Vec<usize> {
        if !cs.tree_enabled {
            return (0..cs.items.len()).collect();
        }
        let active_key = cs
            .groups
            .get(cs.active_group)
            .map(|s| s.as_str())
            .unwrap_or("");
        cs.item_groups
            .iter()
            .enumerate()
            .filter_map(|(idx, key)| if key == active_key { Some(idx) } else { None })
            .collect()
    }

    fn derive_item_subgroup(item: &str) -> String {
        let token = item.split(':').next().unwrap_or(item).trim();
        let first = token.chars().next();
        match first {
            Some(c) if c.is_ascii_alphabetic() => c.to_ascii_uppercase().to_string(),
            Some(c) if c.is_ascii_digit() => "0-9".to_string(),
            Some(_) => "#".to_string(),
            None => "#".to_string(),
        }
    }

    fn recompute_subgroups(cs: &mut CompletionState) {
        let mut item_subgroups = vec![String::new(); cs.items.len()];
        let mut subgroup_set: BTreeSet<String> = BTreeSet::new();
        let filtered = Self::filtered_completion_indices(cs);
        for idx in filtered {
            let sub = Self::derive_item_subgroup(&cs.items[idx]);
            item_subgroups[idx] = sub.clone();
            subgroup_set.insert(sub);
        }
        let mut subgroups = vec!["All".to_string()];
        subgroups.extend(subgroup_set.into_iter());
        cs.item_subgroups = item_subgroups;
        if cs.active_subgroup >= subgroups.len() {
            cs.active_subgroup = 0;
        }
        cs.subgroups = subgroups;
    }

    fn filtered_completion_indices_with_subgroup(cs: &CompletionState) -> Vec<usize> {
        let mut base = Self::filtered_completion_indices(cs);
        let active = cs
            .subgroups
            .get(cs.active_subgroup)
            .map(|s| s.as_str())
            .unwrap_or("All");
        if active == "All" {
            return base;
        }
        base.retain(|idx| cs.item_subgroups.get(*idx).map(|s| s.as_str()) == Some(active));
        base
    }
    fn utf8_char_end(text: &str, byte_start: usize) -> usize {
        let next_len = text[byte_start..]
            .chars()
            .next()
            .map(|c| c.len_utf8())
            .unwrap_or(1);
        (byte_start + next_len).min(text.len())
    }

    fn push_token(
        job: &mut egui::text::LayoutJob,
        text: &str,
        font: &egui::FontId,
        color: egui::Color32,
    ) {
        if text.is_empty() {
            return;
        }
        let format = egui::TextFormat {
            font_id: font.clone(),
            color,
            ..Default::default()
        };
        job.append(text, 0.0, format);
    }

    fn highlight_mini_notation(
        job: &mut egui::text::LayoutJob,
        content: &str,
        font: &egui::FontId,
        string_color: egui::Color32,
    ) {
        let drum_color = egui::Color32::from_rgb(86, 156, 214);
        let mult_color = egui::Color32::from_rgb(197, 134, 192);
        let group_bracket_color = egui::Color32::from_rgb(212, 212, 110);
        let alt_bracket_color = egui::Color32::from_rgb(78, 201, 176);
        let separator_color = egui::Color32::from_rgb(220, 220, 170);

        for token in crate::mini_notation::tokenize(content) {
            let color = match token.kind {
                crate::mini_notation::MiniTokenKind::GroupOpen
                | crate::mini_notation::MiniTokenKind::GroupClose => group_bracket_color,
                crate::mini_notation::MiniTokenKind::AltOpen
                | crate::mini_notation::MiniTokenKind::AltClose => alt_bracket_color,
                crate::mini_notation::MiniTokenKind::Separator => separator_color,
                crate::mini_notation::MiniTokenKind::Multiply => mult_color,
                crate::mini_notation::MiniTokenKind::Word => {
                    if matches!(token.text.as_str(), "bd" | "hh" | "sd") {
                        drum_color
                    } else {
                        string_color
                    }
                }
                _ => string_color,
            };
            Self::push_token(job, &token.text, font, color);
        }
    }

    fn syntax_highlight_layout_job(
        text: &str,
        font: &egui::FontId,
        default_color: egui::Color32,
    ) -> egui::text::LayoutJob {
        if text.is_empty() {
            return egui::text::LayoutJob::simple(
                String::new(),
                font.clone(),
                default_color,
                f32::INFINITY,
            );
        }

        let comment_color = egui::Color32::from_gray(140);
        let string_color = egui::Color32::from_rgb(152, 195, 121);
        let number_color = egui::Color32::from_rgb(209, 154, 102);
        let function_color = egui::Color32::from_rgb(97, 175, 239);

        let bytes = text.as_bytes();
        let mut i = 0usize;
        let mut job = egui::text::LayoutJob::default();
        job.wrap.max_width = f32::INFINITY;

        while i < bytes.len() {
            let ch = bytes[i] as char;

            if ch == '/' && i + 1 < bytes.len() && bytes[i + 1] as char == '/' {
                let mut j = i;
                while j < bytes.len() && bytes[j] as char != '\n' {
                    j += 1;
                }
                Self::push_token(&mut job, &text[i..j], font, comment_color);
                i = j;
                continue;
            }

            if ch == '"' || ch == '\'' || ch == '`' {
                let quote = ch;
                let start_quote = i;
                i += 1;
                let content_start = i;
                while i < bytes.len() {
                    let c = bytes[i] as char;
                    if c == '\\' && i + 1 < bytes.len() {
                        i += 2;
                        continue;
                    }
                    if c == quote {
                        break;
                    }
                    i += 1;
                }

                Self::push_token(
                    &mut job,
                    &text[start_quote..content_start],
                    font,
                    string_color,
                );
                Self::highlight_mini_notation(
                    &mut job,
                    &text[content_start..i],
                    font,
                    string_color,
                );

                if i < bytes.len() && bytes[i] as char == quote {
                    Self::push_token(&mut job, &text[i..i + 1], font, string_color);
                    i += 1;
                }
                continue;
            }

            if ch.is_ascii_digit() {
                let mut j = i + 1;
                while j < bytes.len() && (bytes[j] as char).is_ascii_digit() {
                    j += 1;
                }
                if j + 1 < bytes.len()
                    && bytes[j] as char == '.'
                    && (bytes[j + 1] as char).is_ascii_digit()
                {
                    j += 2;
                    while j < bytes.len() && (bytes[j] as char).is_ascii_digit() {
                        j += 1;
                    }
                }
                Self::push_token(&mut job, &text[i..j], font, number_color);
                i = j;
                continue;
            }

            if ch.is_ascii_alphabetic() || ch == '_' {
                let mut j = i + 1;
                while j < bytes.len() {
                    let c = bytes[j] as char;
                    if c.is_ascii_alphanumeric() || c == '_' {
                        j += 1;
                    } else {
                        break;
                    }
                }

                let ident = &text[i..j];
                let mut k = j;
                while k < bytes.len() && (bytes[k] as char).is_ascii_whitespace() {
                    k += 1;
                }
                let is_function = k < bytes.len() && bytes[k] as char == '(';
                let color = if is_function {
                    function_color
                } else {
                    default_color
                };
                Self::push_token(&mut job, ident, font, color);
                i = j;
                continue;
            }

            let end = Self::utf8_char_end(text, i);
            Self::push_token(&mut job, &text[i..end], font, default_color);
            i = end;
        }

        job
    }

    pub fn new(
        file_path: Option<std::path::PathBuf>,
        code: String,
        doc_store: Option<&DocStore>,
    ) -> Self {
        let mut all_words: Vec<String> = Vec::new();
        if let Some(store) = doc_store {
            for label in store.all_labels() {
                all_words.push(label.clone());
            }
        }
        for label in crate::strudel_language_index::get_language_index().labels {
            all_words.push(label);
        }
        for w in crate::strudel_editor::PITCH_NAMES {
            all_words.push(w.to_string());
        }
        for w in crate::strudel_editor::CHORD_SYMBOLS {
            all_words.push(w.to_string());
        }
        for w in &[
            "const",
            "let",
            "var",
            "function",
            "return",
            "if",
            "else",
            "for",
            "while",
            "true",
            "false",
            "null",
            "undefined",
            "async",
            "await",
            "s",
            "sound",
            "note",
            "scale",
            "chord",
            "degree",
            "struct",
            "stack",
            "layer",
            "append",
            "cat",
            "slowcat",
            "fastcat",
            "slow",
            "fast",
            "rev",
            "jux",
            "superimpose",
            "chunk",
            "splice",
            "segment",
            "split",
            "when",
            "sometimes",
            "sometimesBy",
            "off",
            "offadd",
            "ply",
            "iter",
            "stutter",
            "echo",
            "hurry",
            "degrade",
            "n",
            "gain",
            "speed",
            "vowel",
            "pan",
            "room",
            "size",
            "delay",
            "delaytime",
            "cutoff",
            "resonance",
            "lpf",
            "hpf",
            "crush",
            "coarse",
            "shape",
            "tri",
            "sine",
            "saw",
            "square",
            "rand",
            "randir",
            "perlin",
            "cosine",
            "mask",
            "inv",
            "pick",
            "choose",
            "wchoose",
            "slider",
            "bank",
            "mode",
            "voicing",
            "mini",
            "run",
            "setcps",
            "getcps",
            "solo",
            "mute",
            "silence",
            "dist",
            "seq",
            "bd*2",
            "bd*4",
            "bd hh",
        ] {
            if !all_words.iter().any(|x| x == w) {
                all_words.push(w.to_string());
            }
        }
        all_words.sort();
        all_words.dedup();
        Self {
            code,
            file_path,
            eval_error: None,
            eval_error_line: None,
            eval_error_column: None,
            run_requested: false,
            load_samples_requested: false,
            flash_until: None,
            completion: None,
            all_words,
            find_replace: FindReplaceState::default(),
            last_cursor_char: 0,
        }
    }

    pub fn title(&self) -> String {
        self.file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("Untitled.strudel")
            .to_string()
    }

    pub fn byte_offset_to_line(text: &str, byte_offset: usize) -> usize {
        let mut line = 1usize;
        for (b, ch) in text.char_indices() {
            if b >= byte_offset.min(text.len()) {
                break;
            }
            if ch == '\n' {
                line += 1;
            }
        }
        line
    }

    pub fn token_matches_sample(token: &str, sample: &str) -> bool {
        let token = token.trim_matches(|ch: char| {
            matches!(
                ch,
                '[' | ']' | '<' | '>' | '(' | ')' | ',' | '"' | '\'' | '`'
            )
        });
        token == sample
            || token
                .strip_prefix(sample)
                .map(|rest| rest.starts_with('*') || rest.starts_with(':') || rest.starts_with('/'))
                .unwrap_or(false)
    }

    pub fn active_source_keys_from_document(
        code: &str,
        cycle_time: f64,
        doc: &crate::pattern_ir::PatternDocument,
    ) -> BTreeSet<SourceBreakpointKey> {
        let frac = cycle_time.rem_euclid(1.0);
        let mut keys = BTreeSet::new();
        for ev in &doc.ir.events {
            if matches!(ev.value, crate::pattern_ir::PatternValue::Rest) {
                continue;
            }
            if ev.begin <= frac && frac < ev.end {
                let span = ev.token_span.unwrap_or(ev.source_span);
                keys.insert(SourceBreakpointKey {
                    line: Self::byte_offset_to_line(code, span.start),
                    instance: ev.begin.to_bits(),
                });
            }
        }
        keys
    }

    pub fn active_source_keys(
        code: &str,
        cycle_time: f64,
        doc: Option<&crate::pattern_ir::PatternDocument>,
        playing_events: &[(String, f64)],
    ) -> BTreeSet<SourceBreakpointKey> {
        if let Some(document) = doc.filter(|d| !d.ir.events.is_empty()) {
            let from_doc = Self::active_source_keys_from_document(code, cycle_time, document);
            if !from_doc.is_empty() {
                return from_doc;
            }
        }
        let mut keys = BTreeSet::new();
        for (byte_start, _) in Self::find_active_ranges(code, playing_events) {
            let line = Self::byte_offset_to_line(code, byte_start);
            let frac = cycle_time.rem_euclid(1.0);
            keys.insert(SourceBreakpointKey {
                line,
                instance: frac.to_bits(),
            });
        }
        keys
    }

    pub fn cursor_line_column(&self) -> (usize, usize) {
        let cursor_char = self.last_cursor_char.min(self.code.chars().count());
        let cursor_byte = Self::char_to_byte(&self.code, cursor_char);
        let before = &self.code[..cursor_byte];
        let line = before.as_bytes().iter().filter(|&&b| b == b'\n').count() + 1;
        let column = before
            .rsplit('\n')
            .next()
            .map(|segment| segment.chars().count() + 1)
            .unwrap_or(1);
        (line, column)
    }

    fn quoted_tokens(code: &str) -> Vec<(usize, &str)> {
        let mut tokens = Vec::new();
        let bytes = code.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let ch = bytes[i];
            if ch == b'"' || ch == b'\'' || ch == b'`' {
                let q = ch;
                let start = i + 1;
                let mut end = start;
                while end < bytes.len() && bytes[end] != q {
                    end += 1;
                }
                if end < bytes.len() {
                    let inner = &code[start..end];
                    tokens.extend(inner.split_whitespace().map(|tok| {
                        let offset = tok.as_ptr() as usize - inner.as_ptr() as usize;
                        (start + offset, tok)
                    }));
                }
                i = end + 1;
            } else {
                i += 1;
            }
        }
        tokens
    }

    /// Returns `(abs_byte_start, abs_byte_end)` for each playing event.
    fn find_active_ranges(code: &str, playing_events: &[(String, f64)]) -> Vec<(usize, usize)> {
        if playing_events.is_empty() {
            return Vec::new();
        }
        let tokens = Self::quoted_tokens(code);
        let mut out = Vec::new();
        for (name, _frac) in playing_events {
            if name.is_empty() {
                continue;
            }
            for (tok_offset, tok_text) in &tokens {
                if Self::token_matches_sample(tok_text, name) {
                    out.push((*tok_offset, *tok_offset + tok_text.len()));
                }
            }
        }
        out
    }

    /// Like `find_active_ranges`, but maps ordered example events to ordered matching tokens.
    fn find_example_active_ranges(
        code: &str,
        playing_events: &[ActiveSampleEvent],
    ) -> Vec<(usize, usize)> {
        if playing_events.is_empty() {
            return Vec::new();
        }
        let tokens = Self::quoted_tokens(code);
        let mut events: Vec<&ActiveSampleEvent> =
            playing_events.iter().filter(|e| !e.s.is_empty()).collect();
        events.sort_by(|a, b| {
            a.begin
                .total_cmp(&b.begin)
                .then_with(|| a.end.total_cmp(&b.end))
                .then_with(|| a.s.cmp(&b.s))
        });

        let current_begin = events.last().map(|e| e.begin.to_bits());
        let mut used = vec![false; tokens.len()];
        let mut out = Vec::new();
        for event in events {
            if let Some((idx, (tok_offset, tok_text))) =
                tokens.iter().enumerate().find(|(idx, (_, tok_text))| {
                    !used[*idx] && Self::token_matches_sample(tok_text, &event.s)
                })
            {
                used[idx] = true;
                if Some(event.begin.to_bits()) == current_begin {
                    out.push((*tok_offset, *tok_offset + tok_text.len()));
                }
            }
        }
        out
    }

    pub fn toggle_line_comment(&mut self) {
        let lines: Vec<&str> = self.code.lines().collect();
        if lines.is_empty() {
            return;
        }
        let comment = "//";
        let all_have = lines.iter().all(|l| l.trim_start().starts_with(comment));
        let new_lines: Vec<String> = if all_have {
            lines
                .iter()
                .map(|l| {
                    let t = l.trim_start();
                    if t.starts_with(comment) {
                        let rest = t[comment.len()..].trim_start();
                        format!("{}{}", l.split_at(l.len() - t.len()).0, rest)
                    } else {
                        (*l).to_string()
                    }
                })
                .collect()
        } else {
            lines
                .iter()
                .map(|l| {
                    let trim = l.trim_start();
                    let indent = l.len() - trim.len();
                    let rest = trim;
                    if rest.is_empty() {
                        format!("{}{}", &l[..indent], comment)
                    } else {
                        format!("{}{} {}", &l[..indent], comment, rest)
                    }
                })
                .collect()
        };
        self.code = new_lines.join("\n");
        if !self.code.ends_with('\n') && !self.code.is_empty() {
            self.code.push('\n');
        }
    }

    fn collect_completions(
        text: &str,
        cursor_char: usize,
        all: &[String],
        doc_store: Option<&DocStore>,
        settings: &IdeSettings,
    ) -> (usize, String, Vec<String>) {
        crate::strudel_completion_engine::collect_completions(
            text,
            cursor_char,
            all,
            doc_store,
            &settings.editor_bank_case_style,
            settings.editor_bank_aliases_enabled,
            &settings.editor_semantic_hints_mode,
        )
    }

    fn call_hint_text(
        text: &str,
        cursor_char: usize,
        doc_store: Option<&DocStore>,
    ) -> Option<String> {
        crate::strudel_completion_engine::call_hint_text(text, cursor_char, doc_store)
    }

    fn apply_completion(&mut self, full_word: &str) -> Option<usize> {
        let mut caret_char: Option<usize> = None;
        if let Some(ref cs) = self.completion {
            let apply_text = crate::strudel_completion_engine::completion_apply_text(
                &self.code,
                cs.prefix_end_char,
                full_word,
            );
            let byte_start: usize = self
                .code
                .char_indices()
                .nth(cs.prefix_start_char)
                .map(|(b, _)| b)
                .unwrap_or(self.code.len());
            let byte_end: usize = self
                .code
                .char_indices()
                .nth(cs.prefix_end_char)
                .map(|(b, _)| b)
                .unwrap_or(self.code.len());
            if byte_start <= byte_end && byte_end <= self.code.len() {
                self.code.replace_range(byte_start..byte_end, &apply_text);
                caret_char = Some(cs.prefix_start_char + apply_text.chars().count());
            }
        }
        self.completion = None;
        caret_char
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

    fn line_start_char(text: &str, char_index: usize) -> usize {
        let idx = char_index.min(text.chars().count());
        let before = Self::char_to_byte(text, idx);
        let prefix = &text[..before];
        match prefix.rfind('\n') {
            Some(pos) => Self::byte_to_char(text, pos + 1),
            None => 0,
        }
    }

    fn line_end_char(text: &str, char_index: usize) -> usize {
        let idx = char_index.min(text.chars().count());
        let from = Self::char_to_byte(text, idx);
        match text[from..].find('\n') {
            Some(rel) => Self::byte_to_char(text, from + rel),
            None => text.chars().count(),
        }
    }

    fn apply_tab_to_range(
        text: &str,
        cursor_range: egui::text::CCursorRange,
        shift: bool,
    ) -> Option<(String, egui::text::CCursorRange)> {
        let selected = cursor_range.as_sorted_char_range();
        let has_selection = !cursor_range.is_empty();
        let text_chars = text.chars().count();

        let block_start_char = Self::line_start_char(text, selected.start);
        let mut block_end_anchor = selected.end.min(text_chars);
        if has_selection && block_end_anchor > block_start_char {
            let last = text.chars().nth(block_end_anchor.saturating_sub(1));
            if last == Some('\n') {
                block_end_anchor = block_end_anchor.saturating_sub(1);
            }
        }
        let block_end_char = Self::line_end_char(text, block_end_anchor);

        let block_start_byte = Self::char_to_byte(text, block_start_char);
        let block_end_byte = Self::char_to_byte(text, block_end_char);
        let block = &text[block_start_byte..block_end_byte];
        let block_lines: Vec<&str> = block.split_inclusive('\n').collect();
        if block_lines.is_empty() {
            return None;
        }

        let mut new_block = String::new();
        let mut changed = false;
        for line in &block_lines {
            if shift {
                if let Some(rest) = line.strip_prefix('\t') {
                    new_block.push_str(rest);
                    changed = true;
                } else if line.starts_with("    ") {
                    new_block.push_str(&line[4..]);
                    changed = true;
                } else {
                    new_block.push_str(line);
                }
            } else {
                new_block.push('\t');
                new_block.push_str(line);
                changed = true;
            }
        }
        if !changed {
            return None;
        }

        let mut new_text = String::with_capacity(text.len() + new_block.len() + 16);
        new_text.push_str(&text[..block_start_byte]);
        new_text.push_str(&new_block);
        new_text.push_str(&text[block_end_byte..]);

        let delta = if !shift {
            block_lines.len() as isize
        } else {
            new_block.chars().count() as isize - block.chars().count() as isize
        };

        let new_range = if has_selection {
            let start =
                (selected.start as isize + if shift { delta.min(0) } else { 1 }).max(0) as usize;
            let end = (selected.end as isize + delta).max(0) as usize;
            egui::text::CCursorRange::two(
                egui::text::CCursor::new(start),
                egui::text::CCursor::new(end),
            )
        } else {
            let caret = cursor_range.primary.index;
            let new_caret = if shift {
                let line_start = Self::line_start_char(text, caret);
                if line_start < text_chars {
                    let line_start_byte = Self::char_to_byte(text, line_start);
                    let line = &text[line_start_byte
                        ..Self::char_to_byte(text, Self::line_end_char(text, caret))];
                    if line.starts_with('\t') {
                        caret.saturating_sub(1)
                    } else if line.starts_with("    ") {
                        caret.saturating_sub(4)
                    } else {
                        caret
                    }
                } else {
                    caret
                }
            } else {
                caret + 1
            };
            egui::text::CCursorRange::one(egui::text::CCursor::new(new_caret))
        };

        Some((new_text, new_range))
    }

    fn set_editor_selection(
        ctx: &egui::Context,
        editor_id: egui::Id,
        selection: egui::text::CCursorRange,
    ) {
        if let Some(mut state) = egui::TextEdit::load_state(ctx, editor_id) {
            state.cursor.set_char_range(Some(selection));
            state.store(ctx, editor_id);
        }
    }

    fn update_find_matches(&mut self) {
        self.find_replace.matches.clear();
        if self.find_replace.query.is_empty() {
            self.find_replace.active = 0;
            return;
        }
        self.find_replace.matches = Self::find_matches(
            &self.code,
            &self.find_replace.query,
            self.find_replace.case_sensitive,
        );
        if self.find_replace.matches.is_empty() {
            self.find_replace.active = 0;
        } else {
            self.find_replace.active = self
                .find_replace
                .active
                .min(self.find_replace.matches.len().saturating_sub(1));
        }
    }

    fn select_active_find_match(&self, ctx: &egui::Context, editor_id: egui::Id) {
        if self.find_replace.matches.is_empty() {
            return;
        }
        let (start_byte, end_byte) = self.find_replace.matches[self.find_replace.active];
        let start_char = Self::byte_to_char(&self.code, start_byte);
        let end_char = Self::byte_to_char(&self.code, end_byte);
        Self::set_editor_selection(
            ctx,
            editor_id,
            egui::text::CCursorRange::two(
                egui::text::CCursor::new(start_char),
                egui::text::CCursor::new(end_char),
            ),
        );
    }

    fn find_next(&mut self) {
        if self.find_replace.matches.is_empty() {
            return;
        }
        self.find_replace.active = (self.find_replace.active + 1) % self.find_replace.matches.len();
    }

    fn find_prev(&mut self) {
        if self.find_replace.matches.is_empty() {
            return;
        }
        let n = self.find_replace.matches.len();
        self.find_replace.active = (self.find_replace.active + n - 1) % n;
    }

    fn find_matches(text: &str, query: &str, case_sensitive: bool) -> Vec<(usize, usize)> {
        if query.is_empty() {
            return Vec::new();
        }
        if case_sensitive {
            return text
                .match_indices(query)
                .map(|(start, _)| (start, start + query.len()))
                .collect();
        }
        let q_chars = query.chars().count();
        if q_chars == 0 {
            return Vec::new();
        }
        let query_folded = query.to_lowercase();
        let mut starts: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
        starts.push(text.len());
        let total_chars = starts.len().saturating_sub(1);
        if q_chars > total_chars {
            return Vec::new();
        }
        let mut out = Vec::new();
        for i in 0..=(total_chars - q_chars) {
            let start = starts[i];
            let end = starts[i + q_chars];
            if text[start..end].to_lowercase() == query_folded {
                out.push((start, end));
            }
        }
        out
    }

    fn replace_active_match(&mut self) -> Option<usize> {
        if self.find_replace.matches.is_empty() {
            return None;
        }
        let (start, end) = self.find_replace.matches[self.find_replace.active];
        self.code
            .replace_range(start..end, &self.find_replace.replacement);
        let caret_byte = start + self.find_replace.replacement.len();
        self.update_find_matches();
        Some(Self::byte_to_char(&self.code, caret_byte))
    }

    fn replace_all_matches(&mut self) -> usize {
        if self.find_replace.query.is_empty() {
            return 0;
        }
        let matches = Self::find_matches(
            &self.code,
            &self.find_replace.query,
            self.find_replace.case_sensitive,
        );
        if matches.is_empty() {
            return 0;
        }
        for (start, end) in matches.iter().rev() {
            self.code
                .replace_range(*start..*end, &self.find_replace.replacement);
        }
        self.update_find_matches();
        matches.len()
    }

    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        settings: &IdeSettings,
        doc_store: Option<&DocStore>,
        playing_events: &[(String, f64)],
        insert_at_cursor: Option<String>,
        debugger: Option<&DebuggerState>,
        example_playback: &ExamplePlaybackState,
        example_playing_events: &[ActiveSampleEvent],
        visuals: Option<&crate::code_visuals::VisualsData>,
    ) -> Vec<EditorAction> {
        let mut editor_actions = Vec::new();
        let completion_was_open = self.completion.is_some();
        let available = ui.available_rect_before_wrap();
        let font_id = egui::FontId::monospace(settings.editor_font_size);
        let editor_id = egui::Id::new("editor_strudel_buffer");
        const LINE_NUM_W: f32 = 44.0;
        const PAD_X: f32 = 6.0;
        const FIND_PANEL_H: f32 = 56.0;

        let viewport_rect = egui::Rect::from_min_size(available.min, available.size());
        let find_visible = self.find_replace.visible;
        let find_panel_h = if find_visible { FIND_PANEL_H } else { 0.0 };
        // Directives visuelles `._xxx()` / `.xxx()` dans le code → bande en bas.
        let visual_directives = crate::code_visuals::parse_visual_directives(&self.code);
        let strip_h = if visual_directives.is_empty() {
            0.0
        } else {
            crate::ui::VISUAL_STRIP_H
        };
        let editor_viewport_rect = egui::Rect::from_min_max(
            viewport_rect.min,
            egui::pos2(
                viewport_rect.max.x,
                viewport_rect.max.y - find_panel_h - strip_h,
            ),
        );
        let visuals_rect = if strip_h > 0.0 {
            Some(egui::Rect::from_min_max(
                egui::pos2(viewport_rect.min.x, editor_viewport_rect.max.y),
                egui::pos2(viewport_rect.max.x, editor_viewport_rect.max.y + strip_h),
            ))
        } else {
            None
        };

        let ctrl_space = ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Space));
        let ctrl_escape = ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Escape));
        let ctrl_f = ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::F));
        let ctrl_h = ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::H));
        let mut accept_completion_tab = false;
        let mut completion_nav_delta: i32 = 0;
        let mut completion_category_delta: i32 = 0;
        let mut completion_subcategory_delta: i32 = 0;
        let mut dismiss_completion = false;
        if self.completion.is_some() && !ctrl_escape {
            ui.input_mut(|i| {
                if i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) {
                    dismiss_completion = true;
                }
            });
        }
        if dismiss_completion {
            self.completion = None;
        }
        if self.completion.is_some()
            && ui.memory(|m| m.has_focus(editor_id))
            && ui.input(|i| i.key_pressed(egui::Key::Tab))
        {
            ui.input_mut(|i| {
                i.consume_key(egui::Modifiers::NONE, egui::Key::Tab);
            });
            accept_completion_tab = true;
        }
        if self.completion.is_some() && ui.memory(|m| m.has_focus(editor_id)) {
            let nav_level = self.completion.as_ref().map(|c| c.nav_level).unwrap_or(0);
            let mut consumed_ctrl_space = false;
            let mut consumed_ctrl_escape = false;
            ui.input_mut(|i| {
                if i.consume_key(egui::Modifiers::CTRL, egui::Key::Space) {
                    consumed_ctrl_space = true;
                }
                if i.consume_key(egui::Modifiers::CTRL, egui::Key::Escape) {
                    consumed_ctrl_escape = true;
                }
            });
            if ui.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                ui.input_mut(|i| {
                    i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown);
                });
                completion_nav_delta = 1;
            } else if ui.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                ui.input_mut(|i| {
                    i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp);
                });
                completion_nav_delta = -1;
            } else if ui.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
                ui.input_mut(|i| {
                    i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight);
                });
                if nav_level == 1 {
                    completion_subcategory_delta = 1;
                } else {
                    completion_category_delta = 1;
                }
            } else if ui.input(|i| i.key_pressed(egui::Key::ArrowLeft)) {
                ui.input_mut(|i| {
                    i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft);
                });
                if nav_level == 1 {
                    completion_subcategory_delta = -1;
                } else {
                    completion_category_delta = -1;
                }
            }
            if consumed_ctrl_space {
                if let Some(cs) = self.completion.as_mut() {
                    if cs.tree_enabled && cs.subgroups.len() > 1 {
                        cs.nav_level = 1;
                    }
                    cs.pending_scroll_align = Some(egui::Align::TOP);
                }
            }
            if consumed_ctrl_escape {
                if let Some(cs) = self.completion.as_mut() {
                    cs.nav_level = 0;
                    cs.pending_scroll_align = Some(egui::Align::TOP);
                }
            }
        }

        let bp_gutter_w = if debugger.is_some() { BP_GUTTER_W } else { 0.0 };
        let line_num_w = if settings.editor_line_numbers {
            LINE_NUM_W
        } else {
            0.0
        };
        let gutter_w = bp_gutter_w + line_num_w;
        let code_min_x = editor_viewport_rect.min.x + gutter_w + PAD_X;
        let code_rect = egui::Rect::from_min_max(
            egui::pos2(code_min_x, editor_viewport_rect.min.y),
            editor_viewport_rect.max,
        );
        let find_panel_rect = egui::Rect::from_min_max(
            egui::pos2(viewport_rect.min.x, viewport_rect.max.y - find_panel_h),
            viewport_rect.max,
        );

        if ui.memory(|m| m.has_focus(editor_id)) && ui.input(|i| i.key_pressed(egui::Key::F9)) {
            let (line, _) = self.cursor_line_column();
            editor_actions.push(EditorAction::ToggleSourceBreakpoint(line));
        }

        // Render multiline TextEdit inside ScrollArea
        let mut code = self.code.clone();
        let mut te_output: Option<egui::text_edit::TextEditOutput> = None;
        let mut call_hint_anchor: Option<egui::Pos2> = None;
        let mut semantic_anchor: Option<egui::Pos2> = None;
        let mut semantic_cursor_char: Option<usize> = None;
        let mut call_hint_text: Option<String> = None;
        let mut hover_doc_anchor: Option<egui::Pos2> = None;
        let mut hover_doc_markup: Option<crate::strudel_completion_engine::HoverDocMarkup> = None;
        let mut semantic_diag: Option<crate::strudel_completion_engine::SemanticDiagnostic> = None;
        let mut debug_completion_ctx: Option<String> = None;
        let mut move_to_find_match = false;
        let mut set_caret_char: Option<usize> = None;

        if ui.memory(|m| m.has_focus(editor_id)) && (ctrl_f || ctrl_h) {
            ui.input_mut(|i| {
                i.consume_key(egui::Modifiers::CTRL, egui::Key::F);
                i.consume_key(egui::Modifiers::CTRL, egui::Key::H);
            });
            self.find_replace.visible = true;
            self.find_replace.replace_mode = ctrl_h || self.find_replace.replace_mode;
            self.find_replace.focus_search_requested = true;
            self.update_find_matches();
        }

        ui.allocate_ui_at_rect(code_rect, |ui| {
            let text_color = ui
                .visuals()
                .override_text_color
                .unwrap_or_else(|| ui.visuals().widgets.inactive.text_color());
            let font_layouter = font_id.clone();
            let mut no_wrap_layouter = move |ui: &egui::Ui,
                                             text: &dyn egui::TextBuffer,
                                             _wrap: f32| {
                let mut job =
                    Self::syntax_highlight_layout_job(text.as_str(), &font_layouter, text_color);
                inject_inline_slider_gaps(&mut job, text.as_str());
                ui.fonts_mut(|f| f.layout_job(job))
            };

            let tab_indents = self.completion.is_none();

            // Перехватываем Tab только для блочного сдвига по ВЫДЕЛЕНИЮ.
            // Если выделения нет, событие не трогаем — штатная логика TextEdit вставит '\t'.
            if tab_indents {
                let has_focus = ui.memory(|m| m.has_focus(editor_id));
                let tab_pressed = ui.input(|i| i.key_pressed(egui::Key::Tab));
                let shift_pressed = ui.input(|i| i.modifiers.shift);

                if has_focus && tab_pressed {
                    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), editor_id) {
                        if let Some(cursor_range) = state.cursor.char_range() {
                            if !cursor_range.is_empty() {
                                // Для выделения всегда съедаем Tab/Shift+Tab, чтобы TextEdit
                                // не заменял выделенный текст встроенной логикой.
                                ui.input_mut(|i| {
                                    if shift_pressed {
                                        i.consume_key(egui::Modifiers::SHIFT, egui::Key::Tab);
                                    } else {
                                        i.consume_key(egui::Modifiers::NONE, egui::Key::Tab);
                                    }
                                });

                                if let Some((new_text, new_range)) =
                                    Self::apply_tab_to_range(&code, cursor_range, shift_pressed)
                                {
                                    code = new_text;
                                    state.cursor.set_char_range(Some(new_range));
                                    state.store(ui.ctx(), editor_id);
                                }
                            }
                        }
                    }
                }
            }

            egui::ScrollArea::both()
                .id_salt("editor_code_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let output = egui::TextEdit::multiline(&mut code)
                        .id(editor_id)
                        .font(font_id.clone())
                        .frame(false)
                        .margin(egui::Margin::ZERO)
                        .desired_width(f32::INFINITY)
                        // Держим фокус в редакторе; Tab для completion перехватываем выше до TextEdit.
                        .lock_focus(true)
                        .layouter(&mut no_wrap_layouter)
                        .show(ui);
                    te_output = Some(output);
                });
        });

        let code_changed = code != self.code;
        // Вставка из браузера семплов (двойной клик / перетаскивание).
        if let Some(ref text) = insert_at_cursor {
            // Sans curseur actif (éditeur jamais focus), on insère en fin de
            // document — sinon le code du navigateur était ignoré silencieusement.
            let byte_off = te_output
                .as_ref()
                .and_then(|output| output.cursor_range.as_ref())
                .and_then(|range| {
                    code.char_indices()
                        .nth(range.primary.index)
                        .map(|(i, _)| i)
                })
                .unwrap_or(code.len());
            code.insert_str(byte_off, text);
        }
        if code_changed || insert_at_cursor.is_some() {
            self.code = code;
            if self.find_replace.visible {
                self.update_find_matches();
            }
        }

        // Use a foreground painter so line numbers and highlights aren't clipped by ScrollArea
        let fg_painter = ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Middle,
            egui::Id::new("editor_overlays"),
        ));

        if let Some(ref output) = te_output {
            let galley: &Arc<egui::Galley> = &output.galley;
            let gpos = output.galley_pos;
            if let Some(pointer_pos) = output.response.hover_pos() {
                let local = pointer_pos - gpos;
                let cursor = galley.cursor_from_pos(local);
                if let Some(doc_markup) = crate::strudel_completion_engine::hover_doc_markup(
                    galley.text(),
                    cursor.index,
                    doc_store,
                ) {
                    hover_doc_anchor = Some(pointer_pos + egui::vec2(12.0, 14.0));
                    hover_doc_markup = Some(doc_markup);
                }
            }
            let runtime_error_line = self.eval_error_line.or_else(|| {
                self.eval_error
                    .as_ref()
                    .and_then(|err| Self::runtime_error_line_number(err))
            });

            // --- Paused at source breakpoint: подсветка строки ---
            if let Some(paused_line) = debugger.and_then(|d| d.paused_source_line) {
                if let Some(line_start_char) =
                    Self::line_start_char_for_line(&self.code, paused_line)
                {
                    let start_rect = galley.pos_from_cursor(egui::text::CCursor {
                        index: line_start_char,
                        prefer_next_row: line_start_char > 0,
                    });
                    let row_top = gpos.y + start_rect.min.y;
                    let row_h = start_rect.height();
                    let line_rect = egui::Rect::from_min_max(
                        egui::pos2(code_min_x, row_top),
                        egui::pos2(editor_viewport_rect.max.x, row_top + row_h),
                    );
                    if line_rect.min.y <= editor_viewport_rect.max.y
                        && line_rect.max.y >= editor_viewport_rect.min.y
                    {
                        fg_painter.rect_filled(
                            line_rect,
                            2.0,
                            egui::Color32::from_rgba_premultiplied(255, 193, 72, 36),
                        );
                        fg_painter.rect_stroke(
                            line_rect,
                            2.0,
                            egui::Stroke::new(1.0, egui::Color32::from_rgb(255, 193, 72)),
                            egui::StrokeKind::Middle,
                        );
                        if bp_gutter_w > 0.0 {
                            let marker = egui::Rect::from_min_max(
                                egui::pos2(editor_viewport_rect.min.x + 2.0, row_top + 2.0),
                                egui::pos2(
                                    editor_viewport_rect.min.x + bp_gutter_w - 2.0,
                                    row_top + row_h - 2.0,
                                ),
                            );
                            fg_painter.rect_filled(
                                marker,
                                2.0,
                                egui::Color32::from_rgb(255, 193, 72),
                            );
                        }
                    }
                }
            }

            // --- Runtime error highlight ---
            if let Some(line_no) = runtime_error_line {
                if let Some(line_start_char) = Self::line_start_char_for_line(&self.code, line_no) {
                    let start_rect = galley.pos_from_cursor(egui::text::CCursor {
                        index: line_start_char,
                        prefer_next_row: line_start_char > 0,
                    });
                    let row_top = gpos.y + start_rect.min.y;
                    let row_h = start_rect.height();
                    let line_rect = egui::Rect::from_min_max(
                        egui::pos2(code_min_x, row_top),
                        egui::pos2(editor_viewport_rect.max.x, row_top + row_h),
                    );
                    if line_rect.min.y <= editor_viewport_rect.max.y
                        && line_rect.max.y >= editor_viewport_rect.min.y
                    {
                        fg_painter.rect_filled(
                            line_rect,
                            2.0,
                            egui::Color32::from_rgba_premultiplied(220, 70, 70, 34),
                        );
                        fg_painter.rect_stroke(
                            line_rect,
                            2.0,
                            egui::Stroke::new(1.0, egui::Color32::from_rgb(225, 80, 80)),
                            egui::StrokeKind::Middle,
                        );
                        if settings.editor_line_numbers || bp_gutter_w > 0.0 {
                            let gutter_marker = egui::Rect::from_min_max(
                                egui::pos2(
                                    editor_viewport_rect.min.x + bp_gutter_w + 4.0,
                                    row_top + 2.0,
                                ),
                                egui::pos2(
                                    editor_viewport_rect.min.x + bp_gutter_w + 8.0,
                                    row_top + row_h - 2.0,
                                ),
                            );
                            fg_painter.rect_filled(
                                gutter_marker,
                                1.5,
                                egui::Color32::from_rgb(230, 75, 75),
                            );
                        }
                        if let Some(col) = self.eval_error_column {
                            if let Some(err_char) =
                                Self::char_index_for_line_column(&self.code, line_no, col)
                            {
                                let err_rect = galley.pos_from_cursor(egui::text::CCursor {
                                    index: err_char,
                                    prefer_next_row: false,
                                });
                                let x = gpos.x + err_rect.min.x;
                                let y1 = row_top + 2.0;
                                let y2 = row_top + row_h - 2.0;
                                let marker_stroke =
                                    egui::Stroke::new(1.5, egui::Color32::from_rgb(245, 95, 95));
                                fg_painter.line_segment(
                                    [egui::pos2(x, y1), egui::pos2(x, y2)],
                                    marker_stroke,
                                );
                                fg_painter.line_segment(
                                    [egui::pos2(x - 4.0, y2), egui::pos2(x + 4.0, y2)],
                                    marker_stroke,
                                );
                            }
                        }
                    }
                }
            }

            // --- Breakpoint gutter + line numbers ---
            if settings.editor_line_numbers || bp_gutter_w > 0.0 {
                let mut draw_breakpoint_gutter =
                    |ui: &egui::Ui,
                     fg_painter: &egui::Painter,
                     line_num: usize,
                     row_top: f32,
                     row_h: f32| {
                        if bp_gutter_w <= 0.0 {
                            return;
                        }
                        let row_center_y = row_top + row_h * 0.5;
                        let bp_center = egui::pos2(
                            editor_viewport_rect.min.x + bp_gutter_w * 0.5,
                            row_center_y,
                        );
                        let has_bp = debugger
                            .map(|d| d.has_source_breakpoint(line_num))
                            .unwrap_or(false);
                        let bp_color = if has_bp {
                            egui::Color32::from_rgb(230, 72, 88)
                        } else {
                            egui::Color32::from_rgba_premultiplied(120, 125, 140, 90)
                        };
                        if has_bp {
                            fg_painter.circle_filled(bp_center, 5.0, bp_color);
                            fg_painter.circle_stroke(
                                bp_center,
                                5.0,
                                egui::Stroke::new(1.0, egui::Color32::from_rgb(255, 120, 130)),
                            );
                        } else {
                            fg_painter.circle_stroke(
                                bp_center,
                                4.0,
                                egui::Stroke::new(1.0, bp_color),
                            );
                        }
                        let bp_rect = egui::Rect::from_center_size(
                            bp_center,
                            egui::vec2(bp_gutter_w, row_h.max(12.0)),
                        );
                        let bp_id = ui.id().with("bp_gutter").with(line_num);
                        let resp = ui.interact(bp_rect, bp_id, egui::Sense::click());
                        if resp.clicked() {
                            editor_actions.push(EditorAction::ToggleSourceBreakpoint(line_num));
                        }
                        if resp.hovered() {
                            resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                        }
                    };

                if self.code.is_empty() {
                    let row_center_y = if let Some(cursor_range) = output.cursor_range {
                        let caret_rect = galley.pos_from_cursor(egui::text::CCursor {
                            index: cursor_range.primary.index,
                            prefer_next_row: cursor_range.primary.prefer_next_row,
                        });
                        gpos.y + caret_rect.center().y
                    } else {
                        // Keep line number visible even when editor is unfocused.
                        editor_viewport_rect.min.y + font_id.size * 0.6
                    };
                    let row_h = font_id.size + 4.0;
                    let row_top = row_center_y - row_h * 0.5;
                    draw_breakpoint_gutter(ui, &fg_painter, 1, row_top, row_h);
                    if settings.editor_line_numbers {
                        fg_painter.text(
                            egui::pos2(
                                editor_viewport_rect.min.x + bp_gutter_w + line_num_w - 8.0,
                                row_center_y,
                            ),
                            egui::Align2::RIGHT_CENTER,
                            "1",
                            font_id.clone(),
                            egui::Color32::from_gray(100),
                        );
                    }
                } else {
                    let mut line_num = 1usize;
                    let mut line_start_char = 0usize;
                    let mut line_start_byte = 0usize;
                    let text_len = self.code.len();

                    loop {
                        let caret_rect = galley.pos_from_cursor(egui::text::CCursor {
                            index: line_start_char,
                            // When line starts right after '\n' (especially empty lines),
                            // `prefer_next_row = true` anchors to the actual visual next row.
                            prefer_next_row: line_start_char > 0,
                        });
                        let row_top = gpos.y + caret_rect.min.y;
                        let row_h = caret_rect.height();
                        if row_top > editor_viewport_rect.max.y {
                            break;
                        }
                        if row_top + row_h >= editor_viewport_rect.min.y {
                            let row_center_y = row_top + row_h * 0.5;
                            draw_breakpoint_gutter(ui, &fg_painter, line_num, row_top, row_h);
                            if settings.editor_line_numbers {
                                fg_painter.text(
                                    egui::pos2(
                                        editor_viewport_rect.min.x + bp_gutter_w + line_num_w - 8.0,
                                        row_center_y,
                                    ),
                                    egui::Align2::RIGHT_CENTER,
                                    format!("{}", line_num),
                                    font_id.clone(),
                                    egui::Color32::from_gray(100),
                                );
                            }
                        }

                        if line_start_byte >= text_len {
                            break;
                        }
                        if let Some(rel_nl) = self.code[line_start_byte..].find('\n') {
                            let next_line_byte = line_start_byte + rel_nl + 1;
                            line_start_char = Self::byte_to_char(&self.code, next_line_byte);
                            line_start_byte = next_line_byte;
                            line_num += 1;
                        } else {
                            break;
                        }
                    }
                }
            }

            // --- Sample highlights ---
            let ranges = Self::find_active_ranges(&self.code, playing_events);
            if !ranges.is_empty() {
                let highlight_fill = egui::Color32::from_rgba_premultiplied(60, 140, 255, 70);
                let highlight_stroke =
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(100, 170, 255));
                for (byte_start, byte_end) in &ranges {
                    let char_start = Self::byte_to_char(&self.code, *byte_start);
                    let char_end = Self::byte_to_char(&self.code, *byte_end);
                    let start_rect = galley.pos_from_cursor(egui::text::CCursor {
                        index: char_start,
                        prefer_next_row: false,
                    });
                    let end_rect = galley.pos_from_cursor(egui::text::CCursor {
                        index: char_end,
                        prefer_next_row: false,
                    });
                    let r = egui::Rect::from_min_max(
                        egui::pos2(gpos.x + start_rect.min.x, gpos.y + start_rect.min.y),
                        egui::pos2(gpos.x + end_rect.max.x, gpos.y + end_rect.max.y),
                    );
                    if r.min.y <= editor_viewport_rect.max.y
                        && r.max.y >= editor_viewport_rect.min.y
                    {
                        fg_painter.rect_filled(r, 2.0, highlight_fill);
                        fg_painter.rect_stroke(r, 2.0, highlight_stroke, egui::StrokeKind::Middle);
                    }
                }
            }

            // --- Widgets inline (comme strudel.cc) : slider(v,min,max) = piste,
            // toggler(v) = case on/off — dessinés dans le vide inséré par le layouter.
            let inline_sliders = crate::live_params::find_inline_widgets(&self.code);
            if !inline_sliders.is_empty() {
                let track_bg = ui.visuals().widgets.inactive.bg_fill;
                let accent = ui.visuals().selection.bg_fill;
                for (si, s) in inline_sliders.iter().enumerate() {
                    let char_start = Self::byte_to_char(&self.code, s.value_start);
                    let char_end = Self::byte_to_char(&self.code, s.value_end);
                    let start_rect = galley.pos_from_cursor(egui::text::CCursor {
                        index: char_start,
                        prefer_next_row: false,
                    });
                    let end_rect = galley.pos_from_cursor(egui::text::CCursor {
                        index: char_end,
                        prefer_next_row: false,
                    });
                    // Pas de widget pour un appel réparti sur deux lignes.
                    if (end_rect.min.y - start_rect.min.y).abs() > 1.0 {
                        continue;
                    }
                    // Le widget occupe le vide inséré avant la valeur ; le chiffre
                    // reste visible juste après dans le texte.
                    let gap = inline_widget_gap(s.kind);
                    let x0 = gpos.x + start_rect.min.x - gap;
                    let x1 = gpos.x + start_rect.min.x;
                    let r = egui::Rect::from_min_max(
                        egui::pos2(x0, gpos.y + start_rect.min.y),
                        egui::pos2(x1, gpos.y + start_rect.max.y),
                    );
                    if r.min.y > editor_viewport_rect.max.y
                        || r.max.y < editor_viewport_rect.min.y
                    {
                        continue;
                    }
                    let cy = r.center().y;
                    match s.kind {
                        crate::live_params::InlineWidgetKind::Slider => {
                            let frac =
                                ((s.value - s.min) / (s.max - s.min)).clamp(0.0, 1.0);
                            // Piste horizontale centrée verticalement, avec marges.
                            let pad = 4.0;
                            let track_min_x = r.min.x + pad;
                            let track_max_x = r.max.x - pad;
                            let track_h = 4.0;
                            let track = egui::Rect::from_min_max(
                                egui::pos2(track_min_x, cy - track_h / 2.0),
                                egui::pos2(track_max_x, cy + track_h / 2.0),
                            );
                            let track_vis = track.intersect(editor_viewport_rect);
                            fg_painter.rect_filled(track_vis, track_h / 2.0, track_bg);
                            let handle_x =
                                track_min_x + (track_max_x - track_min_x) * frac;
                            let fill = egui::Rect::from_min_max(
                                track.min,
                                egui::pos2(handle_x, track.max.y),
                            )
                            .intersect(editor_viewport_rect);
                            fg_painter.rect_filled(fill, track_h / 2.0, accent);
                            // Curseur rond.
                            fg_painter.circle_filled(
                                egui::pos2(handle_x, cy),
                                5.0,
                                accent.gamma_multiply(1.2),
                            );
                            fg_painter.circle_stroke(
                                egui::pos2(handle_x, cy),
                                5.0,
                                egui::Stroke::new(1.0, egui::Color32::WHITE),
                            );
                            let slider_id = ui.id().with("inline_slider").with(si);
                            let r_hit = r.intersect(editor_viewport_rect);
                            let resp = ui.interact(
                                r_hit,
                                slider_id,
                                egui::Sense::click_and_drag(),
                            );
                            if resp.hovered() {
                                ui.ctx()
                                    .set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                            }
                            // Spans calculés sur self.code : get() évite tout
                            // slice hors-limites si le code a changé ce frame.
                            let resp = resp.on_hover_text(
                                self.code.get(s.call_start..s.call_end).unwrap_or(""),
                            );
                            if resp.dragged() {
                                if let Some(pos) = resp.interact_pointer_pos() {
                                    let t = ((pos.x - track_min_x)
                                        / (track_max_x - track_min_x))
                                    .clamp(0.0, 1.0);
                                    let v = s.min + t * (s.max - s.min);
                                    let new_code =
                                        crate::live_params::set_inline_widget_value(
                                            &self.code, s, v,
                                        );
                                    if new_code != self.code {
                                        self.code = new_code;
                                        editor_actions.push(EditorAction::ApplyLiveCode);
                                        // Tous les spans suivants sont périmés
                                        // (offsets octets décalés) — la frame
                                        // suivante recalcule la liste.
                                        break;
                                    }
                                }
                            }
                        }
                        crate::live_params::InlineWidgetKind::Toggler => {
                            // Case on/off centrée dans le vide ; clic = bascule 0↔1.
                            let side = (r.height() - 6.0).clamp(10.0, 14.0);
                            let box_r = egui::Rect::from_center_size(
                                egui::pos2(r.center().x, cy),
                                egui::vec2(side, side),
                            )
                            .intersect(editor_viewport_rect);
                            let on = s.value >= 0.5;
                            fg_painter.rect_filled(
                                box_r,
                                2.0,
                                if on { accent } else { track_bg },
                            );
                            fg_painter.rect_stroke(
                                box_r,
                                2.0,
                                egui::Stroke::new(1.0, egui::Color32::WHITE),
                                egui::StrokeKind::Inside,
                            );
                            if on {
                                // Coche : deux segments.
                                let c = box_r.center();
                                let u = side * 0.22;
                                fg_painter.line_segment(
                                    [
                                        egui::pos2(c.x - u * 1.2, c.y),
                                        egui::pos2(c.x - u * 0.2, c.y + u),
                                    ],
                                    egui::Stroke::new(1.6, egui::Color32::WHITE),
                                );
                                fg_painter.line_segment(
                                    [
                                        egui::pos2(c.x - u * 0.2, c.y + u),
                                        egui::pos2(c.x + u * 1.4, c.y - u * 1.1),
                                    ],
                                    egui::Stroke::new(1.6, egui::Color32::WHITE),
                                );
                            }
                            let widget_id = ui.id().with("inline_toggler").with(si);
                            let r_hit = r.intersect(editor_viewport_rect);
                            let resp = ui.interact(
                                r_hit,
                                widget_id,
                                egui::Sense::click(),
                            );
                            if resp.hovered() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                            let resp = resp.on_hover_text(
                                self.code.get(s.call_start..s.call_end).unwrap_or(""),
                            );
                            if resp.clicked() {
                                let new_code =
                                    crate::live_params::set_inline_widget_value(
                                        &self.code,
                                        s,
                                        if on { 0.0 } else { 1.0 },
                                    );
                                if new_code != self.code {
                                    self.code = new_code;
                                    editor_actions.push(EditorAction::ApplyLiveCode);
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            // --- Bouton « + » après chaque s("…") : menu des options à insérer
            // sur leur propre ligne sous l'appel (mode éditeur, la liste vient
            // de l'utilisateur).
            let sound_calls = crate::live_params::find_sound_calls(&self.code);
            for call in &sound_calls {
                let char_end = Self::byte_to_char(&self.code, call.end);
                let end_rect = galley.pos_from_cursor(egui::text::CCursor {
                    index: char_end,
                    prefer_next_row: false,
                });
                let btn_rect = egui::Rect::from_min_max(
                    egui::pos2(
                        gpos.x + end_rect.min.x - SOUND_OPTION_GAP,
                        gpos.y + end_rect.min.y,
                    ),
                    egui::pos2(gpos.x + end_rect.min.x, gpos.y + end_rect.max.y),
                );
                if btn_rect.min.y > editor_viewport_rect.max.y
                    || btn_rect.max.y < editor_viewport_rect.min.y
                {
                    continue;
                }
                let mut chosen: Option<&'static str> = None;
                let resp = ui.allocate_ui_at_rect(btn_rect, |ui| {
                    // Bouton simple (pas de ComboBox) : le chrome de la combo
                    // déborde du vide de 20px et recouvre le texte à droite.
                    ui.add(
                        egui::Button::new(egui::RichText::new("+").small())
                            .min_size(egui::vec2(SOUND_OPTION_GAP, btn_rect.height())),
                    )
                });
                let resp = resp.inner;
                egui::Popup::menu(&resp).show(|ui| {
                    ui.set_min_width(190.0);
                    ui.label(egui::RichText::new(&call.label).weak().small());
                    ui.separator();
                    for opt in crate::live_params::SOUND_OPTIONS {
                        if ui.selectable_label(false, opt.label).clicked() {
                            chosen = Some(opt.code);
                            egui::Popup::close_id(ui.ctx(), resp.id);
                        }
                    }
                });
                if let Some(code) = chosen {
                    let at = crate::live_params::chain_insert_point(&self.code, call.end)
                        .min(self.code.len());
                    let tail = self.code.split_off(at);
                    self.code.push_str("\n  ");
                    self.code.push_str(code);
                    self.code.push_str(&tail);
                    editor_actions.push(EditorAction::ApplyLiveCode);
                    // Positions des appels suivants décalées — on recalcule.
                    break;
                }
            }

            // --- Find/Replace highlights ---
            if self.find_replace.visible && !self.find_replace.matches.is_empty() {
                for (idx, (byte_start, byte_end)) in self.find_replace.matches.iter().enumerate() {
                    let char_start = Self::byte_to_char(&self.code, *byte_start);
                    let char_end = Self::byte_to_char(&self.code, *byte_end);
                    let start_rect = galley.pos_from_cursor(egui::text::CCursor {
                        index: char_start,
                        prefer_next_row: false,
                    });
                    let end_rect = galley.pos_from_cursor(egui::text::CCursor {
                        index: char_end,
                        prefer_next_row: false,
                    });
                    let r = egui::Rect::from_min_max(
                        egui::pos2(gpos.x + start_rect.min.x, gpos.y + start_rect.min.y),
                        egui::pos2(gpos.x + end_rect.max.x, gpos.y + end_rect.max.y),
                    );
                    if r.min.y <= editor_viewport_rect.max.y
                        && r.max.y >= editor_viewport_rect.min.y
                    {
                        let is_active = idx == self.find_replace.active;
                        let fill = if is_active {
                            egui::Color32::from_rgba_premultiplied(255, 215, 0, 90)
                        } else {
                            egui::Color32::from_rgba_premultiplied(255, 255, 0, 50)
                        };
                        let stroke = if is_active {
                            egui::Stroke::new(1.5, egui::Color32::from_rgb(255, 215, 0))
                        } else {
                            egui::Stroke::new(1.0, egui::Color32::from_rgb(220, 220, 120))
                        };
                        fg_painter.rect_filled(r, 2.0, fill);
                        fg_painter.rect_stroke(r, 2.0, stroke, egui::StrokeKind::Middle);
                    }
                }
            }

            // --- Ctrl+Space completion activation ---
            if ctrl_space && self.completion.is_none() {
                if let Some(range) = output.cursor_range {
                    let cursor_char = range.primary.index;
                    let galley_text = galley.text();
                    let (prefix_start_char, prefix, items) = Self::collect_completions(
                        galley_text,
                        cursor_char,
                        &self.all_words,
                        doc_store,
                        settings,
                    );
                    if !items.is_empty() {
                        let cursor_rect = galley.pos_from_cursor(egui::text::CCursor {
                            index: cursor_char,
                            prefer_next_row: false,
                        });
                        let anchor = egui::pos2(
                            gpos.x + cursor_rect.min.x,
                            gpos.y + cursor_rect.max.y + 2.0,
                        );
                        let handler = crate::strudel_completion_engine::active_handler_name(
                            galley_text,
                            cursor_char,
                        );
                        let (tree_enabled, item_groups, groups) =
                            Self::build_completion_groups(&items, handler);
                        self.completion = Some(CompletionState {
                            prefix,
                            items,
                            tree_enabled,
                            item_groups,
                            groups,
                            active_group: 0,
                            item_subgroups: Vec::new(),
                            subgroups: vec!["All".to_string()],
                            active_subgroup: 0,
                            nav_level: 0,
                            selected: 0,
                            pending_scroll_align: Some(egui::Align::TOP),
                            anchor,
                            prefix_start_char,
                            prefix_end_char: cursor_char,
                        });
                        if let Some(cs) = self.completion.as_mut() {
                            Self::recompute_subgroups(cs);
                        }
                    } else {
                        self.completion = None;
                    }
                }
            }

            // Update completion on text changes
            if code_changed {
                if let Some(range) = output.cursor_range {
                    let cursor_char = range.primary.index;
                    let galley_text = galley.text();
                    let (prefix_start_char, prefix, items) = Self::collect_completions(
                        galley_text,
                        cursor_char,
                        &self.all_words,
                        doc_store,
                        settings,
                    );
                    let cursor_rect = galley.pos_from_cursor(egui::text::CCursor {
                        index: cursor_char,
                        prefer_next_row: false,
                    });
                    let anchor =
                        egui::pos2(gpos.x + cursor_rect.min.x, gpos.y + cursor_rect.max.y + 2.0);

                    if let Some(ref mut cs) = self.completion {
                        let handler = crate::strudel_completion_engine::active_handler_name(
                            galley_text,
                            cursor_char,
                        );
                        let allow_empty_prefix =
                            matches!(handler, Some("sound" | "bank" | "chord" | "scale" | "mode"));
                        if items.is_empty() || (prefix.is_empty() && !allow_empty_prefix) {
                            self.completion = None;
                        } else {
                            cs.prefix = prefix;
                            cs.items = items;
                            let (tree_enabled, item_groups, groups) =
                                Self::build_completion_groups(&cs.items, handler);
                            cs.tree_enabled = tree_enabled;
                            cs.item_groups = item_groups;
                            cs.groups = groups;
                            cs.active_group =
                                cs.active_group.min(cs.groups.len().saturating_sub(1));
                            cs.active_subgroup = 0;
                            Self::recompute_subgroups(cs);
                            if cs.subgroups.len() <= 1 {
                                cs.nav_level = 0;
                            }
                            cs.prefix_start_char = prefix_start_char;
                            cs.prefix_end_char = cursor_char;
                            let filtered = Self::filtered_completion_indices_with_subgroup(cs);
                            if !filtered.iter().any(|&idx| idx == cs.selected) {
                                cs.selected = filtered.first().copied().unwrap_or(0);
                            }
                            cs.pending_scroll_align = Some(egui::Align::TOP);
                            cs.anchor = anchor;
                        }
                    } else {
                        let handler = crate::strudel_completion_engine::active_handler_name(
                            galley_text,
                            cursor_char,
                        );
                        let allow_empty_prefix =
                            matches!(handler, Some("sound" | "bank" | "chord" | "scale" | "mode"));
                        if (allow_empty_prefix || !prefix.is_empty()) && !items.is_empty() {
                            let (tree_enabled, item_groups, groups) =
                                Self::build_completion_groups(&items, handler);
                            self.completion = Some(CompletionState {
                                prefix,
                                items,
                                tree_enabled,
                                item_groups,
                                groups,
                                active_group: 0,
                                item_subgroups: Vec::new(),
                                subgroups: vec!["All".to_string()],
                                active_subgroup: 0,
                                nav_level: 0,
                                selected: 0,
                                pending_scroll_align: Some(egui::Align::TOP),
                                anchor,
                                prefix_start_char,
                                prefix_end_char: cursor_char,
                            });
                            if let Some(cs) = self.completion.as_mut() {
                                Self::recompute_subgroups(cs);
                            }
                        }
                    }
                } else {
                    self.completion = None;
                }
            }

            if let Some(range) = output.cursor_range {
                let cursor_char = range.primary.index;
                self.last_cursor_char = cursor_char;
                let galley_text = galley.text();
                if cfg!(debug_assertions) {
                    debug_completion_ctx = crate::strudel_completion_engine::debug_context_label(
                        galley_text,
                        cursor_char,
                    )
                    .map(|s| s.to_string());
                }
                if let Some(hint) = Self::call_hint_text(galley_text, cursor_char, doc_store) {
                    let cursor_rect = galley.pos_from_cursor(egui::text::CCursor {
                        index: cursor_char,
                        prefer_next_row: false,
                    });
                    call_hint_anchor = Some(egui::pos2(
                        gpos.x + cursor_rect.min.x,
                        gpos.y + cursor_rect.max.y + 2.0,
                    ));
                    call_hint_text = Some(hint);
                }
                semantic_diag = crate::strudel_completion_engine::semantic_diagnostic(
                    galley_text,
                    cursor_char,
                    &settings.editor_semantic_hints_mode,
                );
                semantic_cursor_char = Some(cursor_char);
                let cursor_rect = galley.pos_from_cursor(egui::text::CCursor {
                    index: cursor_char,
                    prefer_next_row: false,
                });
                semantic_anchor = Some(egui::pos2(
                    gpos.x + cursor_rect.min.x,
                    gpos.y + cursor_rect.max.y + 2.0,
                ));
            }
        }

        // --- Completion popup ---
        let mut completion_action: Option<String> = None;
        let mut refocus_editor = false;
        if let Some(ref mut cs) = self.completion {
            if completion_category_delta != 0 && !cs.groups.is_empty() {
                let n = cs.groups.len() as i32;
                let cur = cs.active_group as i32;
                let next = (cur + completion_category_delta).rem_euclid(n) as usize;
                cs.active_group = next;
                cs.active_subgroup = 0;
                Self::recompute_subgroups(cs);
                if cs.subgroups.len() <= 1 {
                    cs.nav_level = 0;
                }
                let filtered = Self::filtered_completion_indices_with_subgroup(cs);
                cs.selected = filtered.first().copied().unwrap_or(0);
                cs.pending_scroll_align = Some(egui::Align::TOP);
            }
            if completion_subcategory_delta != 0 && !cs.subgroups.is_empty() {
                let n = cs.subgroups.len() as i32;
                let cur = cs.active_subgroup as i32;
                let next = (cur + completion_subcategory_delta).rem_euclid(n) as usize;
                cs.active_subgroup = next;
                let filtered = Self::filtered_completion_indices_with_subgroup(cs);
                cs.selected = filtered.first().copied().unwrap_or(0);
                cs.pending_scroll_align = Some(egui::Align::TOP);
            }
            let filtered_indices = Self::filtered_completion_indices_with_subgroup(cs);
            let last = filtered_indices.len().saturating_sub(1);
            if completion_nav_delta > 0 {
                if !filtered_indices.is_empty() {
                    let pos = filtered_indices
                        .iter()
                        .position(|&idx| idx == cs.selected)
                        .unwrap_or(0);
                    let next_pos = if pos >= last { 0 } else { pos + 1 };
                    cs.selected = filtered_indices[next_pos];
                    cs.pending_scroll_align = Some(egui::Align::BOTTOM);
                }
            } else if completion_nav_delta < 0 {
                if !filtered_indices.is_empty() {
                    let pos = filtered_indices
                        .iter()
                        .position(|&idx| idx == cs.selected)
                        .unwrap_or(0);
                    let next_pos = if pos == 0 { last } else { pos - 1 };
                    cs.selected = filtered_indices[next_pos];
                    cs.pending_scroll_align = Some(egui::Align::TOP);
                }
            }
            if accept_completion_tab {
                if let Some(word) = cs.items.get(cs.selected) {
                    refocus_editor = true;
                    completion_action = Some(word.clone());
                }
            }
            ui.input_mut(|i| {
                if !ctrl_escape && !dismiss_completion
                    && i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
                {
                    completion_action = Some(String::new());
                } else if i.consume_key(egui::Modifiers::NONE, egui::Key::Tab)
                    || i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                {
                    if let Some(word) = cs.items.get(cs.selected) {
                        refocus_editor = true;
                        completion_action = Some(word.clone());
                    }
                }
            });

            if completion_action.is_none() {
                let popup_width = 220.0;
                let max_visible = 10;
                let row_h = settings.editor_font_size + 6.0;
                let visible_count = cs.items.len().min(max_visible);
                let popup_height = visible_count as f32 * row_h + 4.0;

                let bg = ui.visuals().window_fill;
                let border = ui.visuals().window_stroke;

                let area_resp = egui::Area::new(egui::Id::new("editor_completion"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(cs.anchor)
                    .show(ui.ctx(), |ui| {
                        egui::Frame::new()
                            .fill(bg)
                            .stroke(border)
                            .corner_radius(4.0)
                            .inner_margin(2.0)
                            .show(ui, |ui| {
                                ui.set_width(popup_width - 4.0);
                                if cfg!(debug_assertions) {
                                    if let Some(ctx_name) = debug_completion_ctx.as_deref() {
                                        ui.label(
                                            egui::RichText::new(ctx_name)
                                                .small()
                                                .monospace()
                                                .color(egui::Color32::from_gray(150)),
                                        );
                                        ui.add_space(2.0);
                                    }
                                }
                                if cs.nav_level == 0 && cs.groups.len() > 1 {
                                    let groups = cs.groups.clone();
                                    ui.push_id("completion_tabs_top", |ui| {
                                    ui.horizontal_wrapped(|ui| {
                                        for (idx, key) in groups.iter().enumerate() {
                                            let selected = idx == cs.active_group;
                                            let label = Self::group_label(key);
                                            if ui.selectable_label(selected, label).clicked() {
                                                cs.active_group = idx;
                                                cs.active_subgroup = 0;
                                                Self::recompute_subgroups(cs);
                                                let filtered = Self::filtered_completion_indices_with_subgroup(cs);
                                                cs.selected = filtered.first().copied().unwrap_or(0);
                                                cs.pending_scroll_align = Some(egui::Align::TOP);
                                            }
                                        }
                                    });
                                    });
                                    ui.add_space(4.0);
                                }
                                if cs.nav_level == 1 && cs.tree_enabled && cs.subgroups.len() > 1 {
                                    let subgroups = cs.subgroups.clone();
                                    ui.push_id("completion_subtabs_top", |ui| {
                                        ui.horizontal_wrapped(|ui| {
                                            for (idx, key) in subgroups.iter().enumerate() {
                                                let selected = idx == cs.active_subgroup;
                                                if ui.selectable_label(selected, key).clicked() {
                                                    cs.active_subgroup = idx;
                                                    let filtered = Self::filtered_completion_indices_with_subgroup(cs);
                                                    cs.selected = filtered.first().copied().unwrap_or(0);
                                                    cs.pending_scroll_align = Some(egui::Align::TOP);
                                                }
                                            }
                                        });
                                    });
                                    ui.add_space(4.0);
                                }
                                let mut requested_align = cs.pending_scroll_align.take();
                                let filtered_indices = Self::filtered_completion_indices_with_subgroup(cs);
                                egui::ScrollArea::vertical()
                                    .max_height(popup_height)
                                    .show(ui, |ui| {
                                        for &item_idx in &filtered_indices {
                                            let word = &cs.items[item_idx];
                                            let selected = item_idx == cs.selected;
                                            let text_color = if selected {
                                                egui::Color32::WHITE
                                            } else {
                                                ui.visuals().text_color()
                                            };
                                            let bg_color = if selected {
                                                egui::Color32::from_rgb(45, 90, 160)
                                            } else {
                                                egui::Color32::TRANSPARENT
                                            };
                                            let r = ui.allocate_exact_size(
                                                egui::vec2(popup_width - 8.0, row_h),
                                                egui::Sense::click(),
                                            );
                                            ui.painter().rect_filled(r.1.rect, 2.0, bg_color);
                                            ui.painter().text(
                                                egui::pos2(r.1.rect.left() + 4.0, r.1.rect.center().y),
                                                egui::Align2::LEFT_CENTER,
                                                word,
                                                egui::FontId::monospace(settings.editor_font_size),
                                                text_color,
                                            );
                                            if r.1.clicked() {
                                                cs.selected = item_idx;
                                                completion_action = Some(word.clone());
                                            }
                                            if selected && requested_align.is_some() {
                                                r.1.scroll_to_me(requested_align.take());
                                            }
                                        }
                                    });
                            });
                    });

                let popup_rect =
                    egui::Rect::from_min_size(cs.anchor, egui::vec2(popup_width, popup_height));
                let mut doc_area_rect: Option<egui::Rect> = None;
                if let Some(store) = doc_store {
                    if let Some(word) = cs.items.get(cs.selected) {
                        if let Some(doc) = store.get(word) {
                            let doc_x = popup_rect.right() + 4.0;
                            let doc_y = popup_rect.top();
                            let doc_width = 320.0;
                            let doc_resp = egui::Area::new(egui::Id::new("editor_completion_doc"))
                                .order(egui::Order::Foreground)
                                .fixed_pos(egui::pos2(doc_x, doc_y))
                                .show(ui.ctx(), |ui| {
                                    egui::Frame::new()
                                        .fill(bg)
                                        .stroke(border)
                                        .corner_radius(4.0)
                                        .inner_margin(8.0)
                                        .show(ui, |ui| {
                                            ui.set_width(doc_width);
                                            ui.set_max_height(300.0);
                                            egui::ScrollArea::vertical().max_height(300.0).show(
                                                ui,
                                                |ui| {
                                                    ui.label(
                                                        egui::RichText::new(&doc.name).strong(),
                                                    );
                                                    if let Some(desc) = &doc.description {
                                                        if !desc.trim().is_empty() {
                                                            Self::doc_markup_label(ui, desc.trim());
                                                        }
                                                    }
                                                    if let Some(params) = &doc.params {
                                                        ui.add_space(4.0);
                                                        ui.label(
                                                            egui::RichText::new("Parameters:")
                                                                .strong(),
                                                        );
                                                        for p in params {
                                                            let ty = p
                                                                .r#type
                                                                .as_ref()
                                                                .and_then(|t| t.names.as_ref())
                                                                .map(|n| n.join(" | "))
                                                                .unwrap_or_default();
                                                            ui.label(format!(
                                                                "  {}: {}",
                                                                p.name, ty
                                                            ));
                                                        }
                                                    }
                                                    if let Some(examples) = &doc.examples {
                                                        ui.add_space(4.0);
                                                        ui.label(
                                                            egui::RichText::new("Examples:")
                                                                .strong(),
                                                        );
                                                        for ex in examples {
                                                            Self::example_row(
                                                                ui,
                                                                ex,
                                                                example_playback,
                                                                example_playing_events,
                                                                &mut editor_actions,
                                                            );
                                                        }
                                                    }
                                                },
                                            );
                                        });
                                });
                            doc_area_rect = Some(doc_resp.response.rect);
                        }
                    }
                }

                let clicked_outside = ui.input(|i| {
                    i.pointer.any_click()
                        && i.pointer
                            .latest_pos()
                            .map(|p| {
                                !area_resp.response.rect.contains(p)
                                    && !doc_area_rect.map(|r| r.contains(p)).unwrap_or(false)
                            })
                            .unwrap_or(false)
                });
                if clicked_outside {
                    self.completion = None;
                }
            }
        }
        if refocus_editor || dismiss_completion {
            ui.memory_mut(|m| m.request_focus(editor_id));
        }

        if let Some(word) = completion_action {
            if word.is_empty() {
                self.completion = None;
                ui.memory_mut(|m| m.request_focus(editor_id));
            } else {
                if let Some(caret_char) = self.apply_completion(&word) {
                    Self::set_editor_selection(
                        ui.ctx(),
                        editor_id,
                        egui::text::CCursorRange::one(egui::text::CCursor::new(caret_char)),
                    );
                    ui.memory_mut(|m| m.request_focus(editor_id));
                }
            }
        }

        if self.find_replace.visible {
            let find_field_id = egui::Id::new("editor_find_field");
            let replace_field_id = egui::Id::new("editor_replace_field");
            let mut do_prev = false;
            let mut do_next = false;
            let mut do_replace = false;
            let mut do_replace_all = false;
            let mut replace_then_next = false;
            let mut do_close = false;
            let mut query_changed = false;
            let mut case_toggled = false;

            ui.allocate_ui_at_rect(find_panel_rect, |ui| {
                egui::Frame::new()
                    .fill(ui.visuals().faint_bg_color)
                    .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
                    .inner_margin(egui::Margin::same(6))
                    .show(ui, |ui| {
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.label("Find:");
                                let find_edit =
                                    egui::TextEdit::singleline(&mut self.find_replace.query)
                                        .id(find_field_id)
                                        .desired_width(220.0);
                                let find_resp = ui.add(find_edit);
                                query_changed = find_resp.changed();
                                if find_resp.lost_focus()
                                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                                {
                                    do_next = true;
                                }

                                let count_text = if self.find_replace.matches.is_empty() {
                                    "0/0".to_string()
                                } else {
                                    format!(
                                        "{}/{}",
                                        self.find_replace.active + 1,
                                        self.find_replace.matches.len()
                                    )
                                };
                                ui.label(count_text);
                                if ui.small_button("Prev").clicked() {
                                    do_prev = true;
                                }
                                if ui.small_button("Next").clicked() {
                                    do_next = true;
                                }
                                if ui
                                    .selectable_label(self.find_replace.case_sensitive, "Aa")
                                    .on_hover_text("Case sensitive")
                                    .clicked()
                                {
                                    self.find_replace.case_sensitive =
                                        !self.find_replace.case_sensitive;
                                    case_toggled = true;
                                }
                                if ui.small_button("X").clicked() {
                                    do_close = true;
                                }
                            });
                            if self.find_replace.replace_mode {
                                ui.horizontal(|ui| {
                                    ui.label("Replace:");
                                    let replace_edit = egui::TextEdit::singleline(
                                        &mut self.find_replace.replacement,
                                    )
                                    .id(replace_field_id)
                                    .desired_width(220.0);
                                    let replace_resp = ui.add(replace_edit);
                                    if replace_resp.lost_focus()
                                        && ui.input(|i| i.key_pressed(egui::Key::Enter))
                                    {
                                        do_replace = true;
                                        replace_then_next = true;
                                    }
                                    if ui.small_button("Replace").clicked() {
                                        do_replace = true;
                                    }
                                    if ui.small_button("Replace All").clicked() {
                                        do_replace_all = true;
                                    }
                                });
                            }
                        });
                    });
            });

            if self.find_replace.focus_search_requested {
                ui.memory_mut(|m| m.request_focus(find_field_id));
                self.find_replace.focus_search_requested = false;
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                do_close = true;
            }
            if query_changed {
                self.update_find_matches();
                move_to_find_match = true;
            }
            if case_toggled {
                self.update_find_matches();
                move_to_find_match = true;
            }
            if do_prev {
                self.find_prev();
                move_to_find_match = true;
            }
            if do_next {
                self.find_next();
                move_to_find_match = true;
            }
            if do_replace {
                set_caret_char = self.replace_active_match();
                move_to_find_match = true;
                if replace_then_next {
                    self.find_next();
                    move_to_find_match = true;
                }
            }
            if do_replace_all {
                if self.replace_all_matches() > 0 {
                    move_to_find_match = true;
                }
            }
            if do_close {
                self.find_replace.visible = false;
            }
        }

        if let Some(caret_char) = set_caret_char {
            Self::set_editor_selection(
                ui.ctx(),
                editor_id,
                egui::text::CCursorRange::one(egui::text::CCursor::new(caret_char)),
            );
            ui.memory_mut(|m| m.request_focus(editor_id));
        } else if move_to_find_match {
            self.select_active_find_match(ui.ctx(), editor_id);
            ui.memory_mut(|m| m.request_focus(editor_id));
        }

        if let (Some(anchor), Some(hint)) = (call_hint_anchor, call_hint_text.as_ref()) {
            let hint_pos = if let Some(cs) = self.completion.as_ref() {
                // Keep call hint attached to the right edge of completion popup.
                let popup_width = 220.0;
                cs.anchor + egui::vec2(popup_width + 8.0, 0.0)
            } else {
                anchor + egui::vec2(0.0, 6.0)
            };
            let _ = egui::Area::new(egui::Id::new("editor_call_hint"))
                .order(egui::Order::Foreground)
                .fixed_pos(hint_pos)
                .show(ui.ctx(), |ui| {
                    egui::Frame::new()
                        .fill(ui.visuals().faint_bg_color)
                        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
                        .corner_radius(4.0)
                        .inner_margin(egui::Margin::same(6))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(hint)
                                    .monospace()
                                    .color(ui.visuals().text_color()),
                            );
                        });
                });
        }
        if let (Some(anchor), Some(hover)) = (hover_doc_anchor, hover_doc_markup.as_ref()) {
            let _ = egui::Area::new(egui::Id::new("editor_hover_doc"))
                .order(egui::Order::Foreground)
                .fixed_pos(anchor)
                .show(ui.ctx(), |ui| {
                    egui::Frame::new()
                        .fill(ui.visuals().faint_bg_color)
                        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
                        .corner_radius(4.0)
                        .inner_margin(egui::Margin::same(6))
                        .show(ui, |ui| {
                            ui.set_max_width(360.0);
                            ui.label(egui::RichText::new(&hover.signature).monospace().strong());
                            if let Some(desc) = hover.description_markup.as_ref() {
                                ui.add_space(4.0);
                                Self::doc_markup_label(ui, desc);
                            }
                        });
                });
        }
        let mut apply_quick_fix = false;
        let mut quick_fix_code_to_apply: Option<String> = None;
        if let (Some(anchor), Some(diag)) =
            (semantic_anchor.or(call_hint_anchor), semantic_diag.as_ref())
        {
            let warn_pos = if let Some(cs) = self.completion.as_ref() {
                // Dock warning near completion popup, avoiding overlap.
                let popup_width = 220.0;
                cs.anchor + egui::vec2(popup_width + 10.0, 52.0)
            } else if call_hint_anchor.is_some() {
                // Keep warning below call-hint block.
                anchor + egui::vec2(0.0, 58.0)
            } else {
                // Keep warning on next visual line under cursor.
                anchor + egui::vec2(0.0, 22.0)
            };
            let _ = egui::Area::new(egui::Id::new("editor_semantic_warning"))
                .order(egui::Order::Foreground)
                .fixed_pos(warn_pos)
                .show(ui.ctx(), |ui| {
                    egui::Frame::new()
                        .fill(ui.visuals().extreme_bg_color)
                        .stroke(egui::Stroke::new(
                            1.0,
                            egui::Color32::from_rgb(180, 130, 30),
                        ))
                        .corner_radius(4.0)
                        .inner_margin(egui::Margin::same(6))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(&diag.message)
                                    .monospace()
                                    .color(egui::Color32::from_rgb(245, 200, 110)),
                            );
                            if let Some(label) = diag.quick_fix_label.as_deref() {
                                if ui.small_button(label).clicked() {
                                    apply_quick_fix = true;
                                    quick_fix_code_to_apply = diag.quick_fix_code.clone();
                                }
                            }
                        });
                });
        }
        if apply_quick_fix {
            if let Some(cursor_char) = semantic_cursor_char {
                let cursor_byte = Self::char_to_byte(&self.code, cursor_char);
                let left = self.code[..cursor_byte]
                    .rfind('\n')
                    .map(|i| i + 1)
                    .unwrap_or(0);
                let right = self.code[cursor_byte..]
                    .find('\n')
                    .map(|r| cursor_byte + r)
                    .unwrap_or(self.code.len());
                if let Some(fixed_line) = quick_fix_code_to_apply.as_ref() {
                    let mut new_code = String::new();
                    new_code.push_str(&self.code[..left]);
                    new_code.push_str(fixed_line);
                    new_code.push_str(&self.code[right..]);
                    self.code = new_code;
                    self.completion = None;
                } else if let Some(new_code) =
                    crate::strudel_completion_engine::apply_semantic_quick_fix(
                        &self.code,
                        cursor_char,
                    )
                {
                    self.code = new_code;
                    self.completion = None;
                }
            }
        }

        // --- Repaint & overlays ---
        if !playing_events.is_empty() {
            ui.ctx().request_repaint();
        }

        if let Some(until) = self.flash_until {
            if Instant::now() < until {
                ui.ctx().request_repaint();
                fg_painter.rect_filled(
                    editor_viewport_rect,
                    0.0,
                    egui::Color32::from_rgba_premultiplied(255, 255, 255, 25),
                );
            } else {
                self.flash_until = None;
            }
        }

        if let Some(ref err) = self.eval_error {
            ui.colored_label(egui::Color32::RED, err);
        }

        if should_stop_example_on_completion_close(
            completion_was_open,
            self.completion.is_some(),
            example_playback,
        ) {
            editor_actions.push(EditorAction::StopExample);
        }

        // Bande de visuels `._xxx()` sous le code (comme strudel.cc).
        if let Some(vrect) = visuals_rect {
            if vrect.height() > 1.0 {
                ui.allocate_ui_at_rect(vrect, |ui| {
                    crate::ui::visuals_strip_ui(ui, &self.code, visuals);
                });
            }
        }

        editor_actions
    }
}

/// Largeur du vide inséré dans la ligne pour accueillir un widget inline :
/// une piste de slider est large, une case on/off est compacte.
fn inline_widget_gap(kind: crate::live_params::InlineWidgetKind) -> f32 {
    match kind {
        crate::live_params::InlineWidgetKind::Slider => 108.0,
        crate::live_params::InlineWidgetKind::Toggler => 24.0,
    }
}

/// Injecte la place des widgets inline (`slider(v,min,max)`, `toggler(v)`) dans
/// le layout : un vide de `inline_widget_gap(kind)` px avant l'argument valeur —
/// le widget y est dessiné et la valeur reste affichée en texte juste à côté
/// (comme sur strudel.cc). La suite du texte est décalée : le widget prend une
/// place réelle dans la ligne.
/// Largeur du bouton « + » réservé après chaque appel `s("…")`.
const SOUND_OPTION_GAP: f32 = 20.0;

fn inject_inline_slider_gaps(job: &mut egui::text::LayoutJob, text: &str) {
    use crate::live_params::{find_inline_widgets, find_sound_calls};
    let widgets = find_inline_widgets(text);
    let calls = find_sound_calls(text);
    if widgets.is_empty() && calls.is_empty() {
        return;
    }
    let mut cuts: Vec<usize> = Vec::with_capacity(widgets.len() * 2 + calls.len());
    let mut value_spans: Vec<(usize, usize, f32)> =
        Vec::with_capacity(widgets.len() + calls.len());
    for s in &widgets {
        cuts.push(s.value_start);
        cuts.push(s.value_end);
        value_spans.push((s.value_start, s.value_end, inline_widget_gap(s.kind)));
    }
    // Un vide juste après le `)` de chaque `s("…")` : le bouton « + option »
    // s'y dessine (voir l'overlay plus bas).
    for c in &calls {
        cuts.push(c.end);
        cuts.push(c.end);
        value_spans.push((c.end, c.end, SOUND_OPTION_GAP));
    }
    cuts.sort_unstable();
    cuts.dedup();
    let gap_at = |p: usize| -> f32 {
        value_spans
            .iter()
            .find(|&&(a, _, _)| a == p)
            .map(|&(_, _, g)| g)
            .unwrap_or(0.0)
    };
    // Le texte de la valeur reste visible : le widget se dessine dans le vide,
    // juste avant le chiffre.
    let mut out: Vec<egui::text::LayoutSection> =
        Vec::with_capacity(job.sections.len() + cuts.len() * 2);
    for sec in job.sections.drain(..) {
        let inner: Vec<usize> = cuts
            .iter()
            .copied()
            .filter(|&c| c > sec.byte_range.start && c < sec.byte_range.end)
            .collect();
        if inner.is_empty() {
            out.push(sec);
            continue;
        }
        let mut start = sec.byte_range.start;
        for c in inner.iter().copied().chain(std::iter::once(sec.byte_range.end)) {
            out.push(egui::text::LayoutSection {
                leading_space: if start == sec.byte_range.start {
                    sec.leading_space
                } else {
                    0.0
                },
                byte_range: start..c,
                format: sec.format.clone(),
            });
            start = c;
        }
    }
    for sec in out.iter_mut() {
        let g = gap_at(sec.byte_range.start);
        if g > 0.0 {
            sec.leading_space += g;
        }
    }
    job.sections = out;
}

#[cfg(test)]
mod tests {
    use super::{ActiveSampleEvent, ExamplePlaybackState};

    #[test]
    fn example_playback_state_tracks_start_and_clear() {
        let mut state = ExamplePlaybackState::default();
        assert_eq!(state.active(), None);
        state.mark_started("s(\"bd\")".to_string());
        assert_eq!(state.active(), Some("s(\"bd\")"));
        assert!(!super::should_tick_arrangement_for_example_state(&state));
        state.clear();
        assert_eq!(state.active(), None);
        assert!(super::should_tick_arrangement_for_example_state(&state));
    }

    #[test]
    fn completion_close_requests_stop_only_for_active_example() {
        let mut state = ExamplePlaybackState::default();
        assert!(!super::should_stop_example_on_completion_close(
            true, false, &state
        ));
        assert!(!super::should_stop_example_on_completion_close(
            false, false, &state
        ));

        state.mark_started("s(\"bd\")".to_string());
        assert!(super::should_stop_example_on_completion_close(
            true, false, &state
        ));
        assert!(!super::should_stop_example_on_completion_close(
            true, true, &state
        ));
        assert!(!super::should_stop_example_on_completion_close(
            false, false, &state
        ));
    }

    #[test]
    fn sample_token_variants_match_base_sample() {
        assert!(super::EditorTab::token_matches_sample("bd", "bd"));
        assert!(super::EditorTab::token_matches_sample("bd:0", "bd"));
        assert!(super::EditorTab::token_matches_sample("bd:1:1.4", "bd"));
        assert!(super::EditorTab::token_matches_sample("bd*2", "bd"));
        assert!(!super::EditorTab::token_matches_sample("hh", "bd"));
    }

    #[test]
    fn active_ranges_find_example_tokens() {
        let ranges = super::EditorTab::find_active_ranges(
            "s(\"bd bd:1:1.4 hh\")",
            &[("bd".to_string(), 0.0)],
        );
        let tokens: Vec<&str> = ranges
            .iter()
            .map(|(start, end)| &"s(\"bd bd:1:1.4 hh\")"[*start..*end])
            .collect();
        assert_eq!(tokens, vec!["bd", "bd:1:1.4"]);
    }

    #[test]
    fn example_occurrence_matching_uses_event_order() {
        let code = "s(\"bd:0 bd:1 bd:0:0.3 bd:1:1.4\")";
        let events = [
            ActiveSampleEvent {
                s: "bd".to_string(),
                begin: 0.0,
                end: 0.1,
                n: Some(0.0),
            },
            ActiveSampleEvent {
                s: "bd".to_string(),
                begin: 0.25,
                end: 0.35,
                n: Some(1.0),
            },
            ActiveSampleEvent {
                s: "bd".to_string(),
                begin: 0.5,
                end: 0.6,
                n: Some(0.0),
            },
            ActiveSampleEvent {
                s: "bd".to_string(),
                begin: 0.75,
                end: 0.85,
                n: Some(1.0),
            },
        ];

        for (idx, expected) in ["bd:0", "bd:1", "bd:0:0.3", "bd:1:1.4"].iter().enumerate() {
            let ranges = super::EditorTab::find_example_active_ranges(code, &events[..=idx]);
            let tokens: Vec<&str> = ranges
                .iter()
                .map(|(start, end)| &code[*start..*end])
                .collect();
            assert_eq!(tokens, vec![*expected]);
        }
    }

    #[test]
    fn one_active_example_event_highlights_one_token() {
        let code = "s(\"bd:0 bd:1 bd:0:0.3 bd:1:1.4\")";
        let ranges = super::EditorTab::find_example_active_ranges(
            code,
            &[ActiveSampleEvent {
                s: "bd".to_string(),
                begin: 0.5,
                end: 0.6,
                n: Some(0.0),
            }],
        );
        let tokens: Vec<&str> = ranges
            .iter()
            .map(|(start, end)| &code[*start..*end])
            .collect();
        assert_eq!(tokens, vec!["bd:0"]);
    }
}
