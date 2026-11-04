//! Отладчик: брейкпоинты по строке кода и по позиции цикла, вотчпоинты, кэтчпоинты.

use crate::arrangement_ir::SourceSpan;
use crate::pattern_ir::{PatternDocument, PatternEvent, PatternValue};
use crate::strudel_bridge::StrudelEvent;
use eframe::egui;
use std::collections::{BTreeSet, HashMap};

/// Одна «нота» в паттерне: строка кода + позиция события в цикле (не вся строка целиком).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct SourceBreakpointKey {
    pub line: usize,
    pub instance: u64,
}

// ── Типы ──────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Breakpoint {
    pub id: usize,
    /// Позиция в циклах (таймлайн). `None` — только строковый брейкпоинт.
    pub position: Option<f64>,
    /// Строка в редакторе (1-based). `None` — только цикловой брейкпоинт.
    pub source_line: Option<usize>,
    /// Фильтр по лейблу трека (пусто = любой трек).
    pub label_filter: String,
    pub enabled: bool,
    pub hit_count: u32,
}

impl Breakpoint {
    pub fn is_cycle(&self) -> bool {
        self.position.is_some()
    }

    pub fn is_source(&self) -> bool {
        self.source_line.is_some()
    }

    pub fn summary(&self) -> String {
        match (self.source_line, self.position) {
            (Some(line), None) => format!("ligne {}", line),
            (None, Some(pos)) => format!("cycle {:.3}", pos),
            (Some(line), Some(pos)) => format!("ligne {} / cycle {:.3}", line, pos),
            (None, None) => format!("#{}", self.id),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Watchpoint {
    pub id: usize,
    pub label: String,
    pub enabled: bool,
    pub hit_count: u32,
    pub last_event_begin: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CatchpointKind {
    /// Пауза при любой ошибке Strudel/JS.
    AnyError,
    /// Пауза, когда в паттерне срабатывает вызов API (`sound`, `saw`, `note`…).
    Keyword(String),
}

impl CatchpointKind {
    pub fn display(&self) -> String {
        match self {
            Self::AnyError => "Erreur JS".to_string(),
            Self::Keyword(k) => format!("API: {}", k),
        }
    }
}

/// Быстрые кэтчпоинты на ключевые слова Strudel (не путать с семплами bd/hh — это вотчпоинты).
pub const CATCH_KEYWORD_PRESETS: &[&str] = &[
    "sound", "s", "note", "n", "bank", "stack", "saw", "sine", "tri", "square", "gain", "slow",
    "fast", "rev", "jux",
];

#[derive(Clone, Debug)]
pub struct Catchpoint {
    pub id: usize,
    pub kind: CatchpointKind,
    pub enabled: bool,
    pub hit_count: u32,
}

// ── Состояние ─────────────────────────────────────────────────────────────────

pub struct DebuggerState {
    pub breakpoints: Vec<Breakpoint>,
    pub watchpoints: Vec<Watchpoint>,
    pub catchpoints: Vec<Catchpoint>,
    next_id: usize,
    pub paused_at_breakpoint: bool,
    pub paused_source_line: Option<usize>,
    pub last_hit_message: Option<String>,
    prev_active_keys: BTreeSet<SourceBreakpointKey>,
    /// Строка → абсолютное время: не останавливаться, пока playhead < этого момента (после F5).
    skip_source_line_until: HashMap<usize, f64>,
    /// Цикловая позиция (to_bits) → не срабатывать, пока playhead не пройдёт вперёд.
    skip_cycle_until: HashMap<u64, f64>,
    /// Вотчпоинт → множество begin активных событий на прошлом кадре.
    prev_watch_active: HashMap<usize, BTreeSet<u64>>,
    /// Вотчпоинт → не срабатывать, пока playhead < этого времени (после F5).
    skip_watch_until: HashMap<usize, f64>,
    prev_catch_active: HashMap<usize, BTreeSet<u64>>,
    skip_catch_until: HashMap<usize, f64>,
    new_bp_pos: String,
    new_bp_label: String,
    new_wp_label: String,
    new_cp_keyword: String,
    active_tab: DebuggerTab,
}

#[derive(Clone, Copy, PartialEq)]
enum DebuggerTab {
    Breakpoints,
    Watchpoints,
    Catchpoints,
}

impl Default for DebuggerState {
    fn default() -> Self {
        Self {
            breakpoints: Vec::new(),
            watchpoints: Vec::new(),
            catchpoints: Vec::new(),
            next_id: 1,
            paused_at_breakpoint: false,
            paused_source_line: None,
            last_hit_message: None,
            prev_active_keys: BTreeSet::new(),
            skip_source_line_until: HashMap::new(),
            skip_cycle_until: HashMap::new(),
            prev_watch_active: HashMap::new(),
            skip_watch_until: HashMap::new(),
            prev_catch_active: HashMap::new(),
            skip_catch_until: HashMap::new(),
            new_bp_pos: String::new(),
            new_bp_label: String::new(),
            new_wp_label: String::new(),
            new_cp_keyword: String::new(),
            active_tab: DebuggerTab::Breakpoints,
        }
    }
}

impl DebuggerState {
    fn next_id(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn add_breakpoint(&mut self, position: f64, label_filter: String) -> usize {
        let id = self.next_id();
        self.breakpoints.push(Breakpoint {
            id,
            position: Some(position),
            source_line: None,
            label_filter,
            enabled: true,
            hit_count: 0,
        });
        id
    }

    pub fn add_breakpoint_at_line(&mut self, line: usize) -> usize {
        if let Some(bp) = self
            .breakpoints
            .iter_mut()
            .find(|bp| bp.source_line == Some(line))
        {
            bp.enabled = true;
            return bp.id;
        }
        let id = self.next_id();
        self.breakpoints.push(Breakpoint {
            id,
            position: None,
            source_line: Some(line),
            label_filter: String::new(),
            enabled: true,
            hit_count: 0,
        });
        id
    }

    pub fn toggle_source_line(&mut self, line: usize) -> bool {
        if let Some(pos) = self
            .breakpoints
            .iter()
            .position(|bp| bp.source_line == Some(line))
        {
            self.breakpoints.remove(pos);
            if self.paused_source_line == Some(line) {
                self.paused_source_line = None;
            }
            false
        } else {
            self.add_breakpoint_at_line(line);
            true
        }
    }

    pub fn has_source_breakpoint(&self, line: usize) -> bool {
        self.breakpoints
            .iter()
            .any(|bp| bp.enabled && bp.source_line == Some(line))
    }

    pub fn source_breakpoint_lines(&self) -> BTreeSet<usize> {
        self.breakpoints
            .iter()
            .filter(|bp| bp.enabled)
            .filter_map(|bp| bp.source_line)
            .collect()
    }

    pub fn add_watchpoint(&mut self, label: String) -> usize {
        if self.watchpoints.iter().any(|w| w.label == label) {
            return 0;
        }
        let id = self.next_id();
        self.watchpoints.push(Watchpoint {
            id,
            label,
            enabled: true,
            hit_count: 0,
            last_event_begin: None,
        });
        id
    }

    pub fn add_catchpoint(&mut self, kind: CatchpointKind) -> usize {
        let id = self.next_id();
        self.catchpoints.push(Catchpoint {
            id,
            kind,
            enabled: true,
            hit_count: 0,
        });
        id
    }

    pub fn check_breakpoints(
        &mut self,
        prev_time: f64,
        current_time: f64,
        events: &[StrudelEvent],
    ) -> bool {
        self.skip_cycle_until
            .retain(|_, until| current_time < *until);

        for bp in &mut self.breakpoints {
            if !bp.enabled {
                continue;
            }
            let Some(pos) = bp.position else {
                continue;
            };
            let pos_bits = pos.to_bits();
            if let Some(until) = self.skip_cycle_until.get(&pos_bits) {
                if current_time < *until {
                    continue;
                }
                self.skip_cycle_until.remove(&pos_bits);
            }
            if !time_crossed(prev_time, current_time, pos) {
                continue;
            }
            if !bp.label_filter.is_empty() {
                let t = current_time;
                let matches_label = events.iter().any(|ev| {
                    (ev.begin <= t && t < ev.end)
                        && (ev.label.contains(&bp.label_filter)
                            || ev.s.as_deref() == Some(bp.label_filter.as_str()))
                });
                if !matches_label {
                    continue;
                }
            }
            bp.hit_count += 1;
            let msg = if bp.label_filter.is_empty() {
                format!(
                    "Cycle {:.3} — breakpoint #{} (déclenchement {})",
                    pos, bp.id, bp.hit_count
                )
            } else {
                format!(
                    "Cycle {:.3} [{}] — breakpoint #{} (déclenchement {})",
                    pos, bp.label_filter, bp.id, bp.hit_count
                )
            };
            self.last_hit_message = Some(msg);
            self.paused_at_breakpoint = true;
            self.paused_source_line = None;
            return true;
        }
        false
    }

    /// Останавливает на фронте новой ноты (строка + слот в цикле).
    pub fn track_source_keys(
        &mut self,
        active: &BTreeSet<SourceBreakpointKey>,
        current_time: f64,
    ) -> bool {
        self.skip_source_line_until
            .retain(|_, until| current_time < *until);

        let newly_active: Vec<SourceBreakpointKey> = active
            .iter()
            .filter(|key| !self.prev_active_keys.contains(key))
            .filter(|key| {
                self.skip_source_line_until
                    .get(&key.line)
                    .map(|until| current_time < *until)
                    .unwrap_or(false)
                    == false
            })
            .copied()
            .collect();
        self.prev_active_keys = active.clone();

        for key in newly_active {
            for bp in &mut self.breakpoints {
                if !bp.enabled {
                    continue;
                }
                if bp.source_line != Some(key.line) {
                    continue;
                }
                bp.hit_count += 1;
                self.last_hit_message = Some(format!(
                    "Ligne {} (slot {:.3}) — breakpoint #{} (déclenchement {})",
                    key.line,
                    f64::from_bits(key.instance),
                    bp.id,
                    bp.hit_count
                ));
                self.paused_at_breakpoint = true;
                self.paused_source_line = Some(key.line);
                return true;
            }
        }
        false
    }

    pub fn clear_source_line_tracking(&mut self) {
        self.prev_active_keys.clear();
        self.skip_source_line_until.clear();
        self.skip_cycle_until.clear();
        self.prev_watch_active.clear();
        self.skip_watch_until.clear();
        self.prev_catch_active.clear();
    }

    pub fn check_watchpoints(&mut self, events: &[StrudelEvent], current_time: f64) -> bool {
        self.skip_watch_until
            .retain(|_, until| current_time < *until);

        for wp in &mut self.watchpoints {
            if !wp.enabled {
                continue;
            }
            if self
                .skip_watch_until
                .get(&wp.id)
                .map(|until| current_time < *until)
                .unwrap_or(false)
            {
                continue;
            }

            let active = active_watch_event_begins(events, current_time, &wp.label);
            let prev = self
                .prev_watch_active
                .entry(wp.id)
                .or_insert_with(BTreeSet::new);
            let newly_active: Vec<f64> = active
                .iter()
                .filter(|bits| !prev.contains(bits))
                .filter_map(|bits| Some(f64::from_bits(*bits)))
                .collect();
            *prev = active;

            if let Some(&ev_begin) = newly_active.first() {
                wp.last_event_begin = Some(ev_begin);
                wp.hit_count += 1;
                self.last_hit_message = Some(format!(
                    "Watchpoint #{} : piste «{}» @ {:.3} (déclenchement {})",
                    wp.id, wp.label, ev_begin, wp.hit_count
                ));
                self.paused_at_breakpoint = true;
                self.paused_source_line = None;
                return true;
            }
        }
        false
    }

    pub fn check_catchpoints(
        &mut self,
        error: Option<&str>,
        current_time: f64,
        code: Option<&str>,
        doc: Option<&PatternDocument>,
    ) -> bool {
        if let Some(err) = error {
            for cp in &mut self.catchpoints {
                if !cp.enabled {
                    continue;
                }
                if matches!(cp.kind, CatchpointKind::AnyError) {
                    cp.hit_count += 1;
                    self.last_hit_message = Some(format!(
                        "Catchpoint #{} : erreur JS : {} (déclenchement {})",
                        cp.id, err, cp.hit_count
                    ));
                    self.paused_at_breakpoint = true;
                    self.paused_source_line = None;
                    return true;
                }
            }
        }

        let (Some(code), Some(doc)) = (code, doc) else {
            return false;
        };
        if doc.ir.events.is_empty() {
            return false;
        }

        self.skip_catch_until
            .retain(|_, until| current_time < *until);

        for cp in &mut self.catchpoints {
            if !cp.enabled {
                continue;
            }
            let CatchpointKind::Keyword(ref keyword) = cp.kind else {
                continue;
            };
            if self
                .skip_catch_until
                .get(&cp.id)
                .map(|until| current_time < *until)
                .unwrap_or(false)
            {
                continue;
            }

            let active = active_keyword_begins(code, current_time, doc, keyword);
            let prev = self
                .prev_catch_active
                .entry(cp.id)
                .or_insert_with(BTreeSet::new);
            let newly = active.iter().find(|b| !prev.contains(b)).copied();
            *prev = active;
            if let Some(bits) = newly {
                let at = f64::from_bits(bits);
                cp.hit_count += 1;
                self.last_hit_message = Some(format!(
                    "Catchpoint #{} : «{}» @ {:.3} (déclenchement {})",
                    cp.id, keyword, at, cp.hit_count
                ));
                self.paused_at_breakpoint = true;
                self.paused_source_line = None;
                return true;
            }
        }
        false
    }

    /// После F5: не ловить тот же API-вызов, пока playhead не пройдёт конец текущей ноты.
    pub fn skip_catch_until_continue(
        &self,
        current_time: f64,
        code: &str,
        doc: &PatternDocument,
    ) -> HashMap<usize, f64> {
        let frac = current_time.rem_euclid(1.0);
        let cycle_base = current_time.floor();
        let mut out = HashMap::new();
        for cp in &self.catchpoints {
            if !cp.enabled {
                continue;
            }
            let CatchpointKind::Keyword(ref keyword) = cp.kind else {
                continue;
            };
            let mut pass_until = current_time + 1e-4;
            for ev in &doc.ir.events {
                if matches!(ev.value, PatternValue::Rest) {
                    continue;
                }
                if !(ev.begin <= frac && frac < ev.end) {
                    continue;
                }
                if pattern_event_matches_keyword(code, doc, ev, keyword) {
                    pass_until = pass_until.max(cycle_base + ev.end + 1e-4);
                }
            }
            out.insert(cp.id, pass_until);
        }
        out
    }

    /// Полный сброс (Stop / Rewind).
    pub fn resume(&mut self) {
        self.paused_at_breakpoint = false;
        self.paused_source_line = None;
        self.prev_active_keys.clear();
        self.skip_source_line_until.clear();
        self.skip_cycle_until.clear();
        self.prev_watch_active.clear();
        self.skip_watch_until.clear();
        self.prev_catch_active.clear();
        self.skip_catch_until.clear();
    }

    /// Продолжить: проигнорировать текущую ноту и ближайшие цикловые точки впереди playhead.
    pub fn resume_continuing(
        &mut self,
        currently_active: &BTreeSet<SourceBreakpointKey>,
        current_time: f64,
        skip_lines_until: HashMap<usize, f64>,
        skip_watch_until: HashMap<usize, f64>,
        skip_catch_until: HashMap<usize, f64>,
        events: &[StrudelEvent],
        code: Option<&str>,
        doc: Option<&PatternDocument>,
    ) {
        self.paused_at_breakpoint = false;
        self.paused_source_line = None;
        self.prev_active_keys = currently_active.clone();
        self.skip_source_line_until = skip_lines_until;
        self.skip_watch_until = skip_watch_until;
        self.skip_catch_until = skip_catch_until;

        for wp in &self.watchpoints {
            let active = active_watch_event_begins(events, current_time, &wp.label);
            self.prev_watch_active.insert(wp.id, active);
        }

        if let (Some(code), Some(doc)) = (code, doc) {
            for cp in &self.catchpoints {
                let CatchpointKind::Keyword(ref keyword) = cp.kind else {
                    continue;
                };
                let active = active_keyword_begins(code, current_time, doc, keyword);
                self.prev_catch_active.insert(cp.id, active);
            }
        }

        const CYCLE_PASS_MARGIN: f64 = 0.001;
        for bp in &self.breakpoints {
            if !bp.enabled {
                continue;
            }
            let Some(pos) = bp.position else {
                continue;
            };
            if pos + CYCLE_PASS_MARGIN > current_time {
                self.skip_cycle_until
                    .insert(pos.to_bits(), pos + CYCLE_PASS_MARGIN);
            }
        }
    }
}

fn watch_label_matches(ev: &StrudelEvent, label: &str) -> bool {
    if label.is_empty() {
        return false;
    }
    if ev.label.contains(label) || label.contains(&ev.label) {
        return true;
    }
    if let Some(s) = &ev.s {
        return s == label || s.contains(label) || label.contains(s);
    }
    false
}

fn active_watch_event_begins(
    events: &[StrudelEvent],
    current_time: f64,
    label: &str,
) -> BTreeSet<u64> {
    events
        .iter()
        .filter(|ev| watch_label_matches(ev, label))
        .filter(|ev| ev.begin <= current_time && current_time < ev.end)
        .map(|ev| ev.begin.to_bits())
        .collect()
}

fn active_keyword_begins(
    code: &str,
    cycle_time: f64,
    doc: &PatternDocument,
    keyword: &str,
) -> BTreeSet<u64> {
    let frac = cycle_time.rem_euclid(1.0);
    let cycle_base = cycle_time.floor();
    doc.ir
        .events
        .iter()
        .filter(|ev| !matches!(ev.value, PatternValue::Rest))
        .filter(|ev| ev.begin <= frac && frac < ev.end)
        .filter(|ev| pattern_event_matches_keyword(code, doc, ev, keyword))
        .map(|ev| (cycle_base + ev.begin).to_bits())
        .collect()
}

fn pattern_event_matches_keyword(
    code: &str,
    doc: &PatternDocument,
    ev: &PatternEvent,
    keyword: &str,
) -> bool {
    if let Some(pattern) = doc.ir.patterns.iter().find(|p| p.id == ev.pattern_id) {
        for alias in keyword_aliases(keyword) {
            if pattern.source_fn.eq_ignore_ascii_case(&alias) {
                return true;
            }
        }
        if span_contains_keyword(code, pattern.source_span, keyword) {
            return true;
        }
    }
    if let Some(span) = ev.token_span {
        if span_contains_keyword(code, span, keyword) {
            return true;
        }
    }
    span_contains_keyword(code, ev.source_span, keyword)
}

fn keyword_aliases(keyword: &str) -> Vec<String> {
    match keyword.to_ascii_lowercase().as_str() {
        "sound" => vec!["sound".into(), "s".into()],
        "s" => vec!["s".into(), "sound".into()],
        "note" => vec!["note".into(), "n".into()],
        "n" => vec!["n".into(), "note".into()],
        other => vec![other.to_string()],
    }
}

fn span_contains_keyword(code: &str, span: SourceSpan, keyword: &str) -> bool {
    if span.start >= span.end || span.end > code.len() {
        return false;
    }
    let slice = &code[span.start..span.end];
    keyword_aliases(keyword)
        .iter()
        .any(|alias| contains_identifier(slice, alias))
}

fn contains_identifier(haystack: &str, word: &str) -> bool {
    let word_lower = word.to_ascii_lowercase();
    let hay_lower = haystack.to_ascii_lowercase();
    let mut start = 0usize;
    while let Some(rel) = hay_lower[start..].find(&word_lower) {
        let b_start = start + rel;
        let b_end = b_start + word_lower.len();
        let before = hay_lower[..b_start].chars().last();
        let after = hay_lower[b_end..].chars().next();
        let before_ok = before
            .map(|ch| !ch.is_ascii_alphanumeric() && ch != '_')
            .unwrap_or(true);
        let after_ok = after
            .map(|ch| !ch.is_ascii_alphanumeric() && ch != '_')
            .unwrap_or(true);
        if before_ok && after_ok {
            return true;
        }
        if b_start > 0 && hay_lower.as_bytes()[b_start - 1] == b'.' {
            return true;
        }
        start = b_end;
    }
    false
}

fn time_crossed(prev: f64, cur: f64, target: f64) -> bool {
    if (prev - cur).abs() < 1e-9 {
        return false;
    }
    if prev < cur {
        prev < target && target <= cur
    } else {
        cur <= target && target < prev
    }
}

// ── Действия из UI ────────────────────────────────────────────────────────────

pub enum DebuggerAction {
    Resume,
    Step,
    RemoveBreakpoint(usize),
    RemoveWatchpoint(usize),
    RemoveCatchpoint(usize),
    ToggleBreakpoint(usize),
    ToggleWatchpoint(usize),
    ToggleCatchpoint(usize),
    AddBreakpointAt(f64, String),
    AddWatchpoint(String),
    AddCatchpointError,
    AddCatchpointKeyword(String),
    ClearAllBreakpoints,
    DisableAllBreakpoints,
    EnableAllBreakpoints,
}

// ── Палитра UI ────────────────────────────────────────────────────────────────

mod theme {
    use eframe::egui::Color32;

    pub const PAUSE_BAR: Color32 = Color32::from_rgb(255, 193, 72);
    pub const PAUSE_BG: Color32 = Color32::from_rgba_premultiplied(255, 193, 72, 28);
    /// Текст на жёлтой полосе паузы (жёлтый PAUSE_BAR на PAUSE_BG не читается).
    pub const PAUSE_FG: Color32 = Color32::WHITE;
    pub const BP_ENABLED: Color32 = Color32::from_rgb(230, 72, 88);
    pub const BP_DISABLED: Color32 = Color32::from_rgb(110, 110, 120);
    pub const WP_ENABLED: Color32 = Color32::from_rgb(72, 168, 235);
    pub const CP_ENABLED: Color32 = Color32::from_rgb(235, 155, 72);
    pub const ACCENT: Color32 = Color32::from_rgb(88, 166, 255);
    pub const MUTED: Color32 = Color32::from_rgb(140, 148, 165);
    pub const ROW_HOVER: Color32 = Color32::from_rgba_premultiplied(255, 255, 255, 12);
}

// ── UI ────────────────────────────────────────────────────────────────────────

pub fn debugger_ui(ui: &mut egui::Ui, state: &mut DebuggerState) -> Vec<DebuggerAction> {
    let mut actions = Vec::new();

    control_bar_ui(ui, state, &mut actions);
    ui.add_space(6.0);
    tab_bar_ui(ui, state);
    ui.add_space(4.0);

    egui::Frame::new()
        .fill(ui.visuals().extreme_bg_color)
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .corner_radius(6.0)
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| match state.active_tab {
            DebuggerTab::Breakpoints => breakpoints_ui(ui, state, &mut actions),
            DebuggerTab::Watchpoints => watchpoints_ui(ui, state, &mut actions),
            DebuggerTab::Catchpoints => catchpoints_ui(ui, state, &mut actions),
        });

    actions
}

fn control_bar_ui(ui: &mut egui::Ui, state: &mut DebuggerState, actions: &mut Vec<DebuggerAction>) {
    let paused = state.paused_at_breakpoint;
    let frame_fill = if paused {
        theme::PAUSE_BG
    } else {
        ui.visuals().faint_bg_color
    };
    let stroke = if paused {
        egui::Stroke::new(1.0, theme::PAUSE_BAR)
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke
    };

    egui::Frame::new()
        .fill(frame_fill)
        .stroke(stroke)
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let pause_text = if paused {
                    Some(theme::PAUSE_FG)
                } else {
                    None
                };

                if paused {
                    ui.label(
                        egui::RichText::new("PAUSE")
                            .strong()
                            .color(theme::PAUSE_FG),
                    );
                    ui.separator();
                    if tool_button(ui, "▶", "Reprendre (F5)", pause_text).clicked() {
                        actions.push(DebuggerAction::Resume);
                    }
                    if tool_button(ui, "→", "Avancer d'un cycle", pause_text).clicked() {
                        actions.push(DebuggerAction::Step);
                    }
                    ui.separator();
                    if let Some(msg) = &state.last_hit_message {
                        ui.label(
                            egui::RichText::new(msg)
                                .monospace()
                                .color(theme::PAUSE_FG),
                        );
                    }
                    if let Some(line) = state.paused_source_line {
                        ui.label(
                            egui::RichText::new(format!("ligne {}", line))
                                .small()
                                .color(theme::PAUSE_FG),
                        );
                    }
                } else {
                    ui.label(
                        egui::RichText::new("Débogueur")
                            .strong()
                            .color(ui.visuals().text_color()),
                    );
                    ui.label(
                        egui::RichText::new(
                            "Breakpoints — ligne/cycle · watchpoints — sample bd/hh · catchpoints — JS ou API",
                        )
                        .small()
                        .color(theme::MUTED),
                    );
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let bp_count = state.breakpoints.len();
                    let wp_count = state.watchpoints.len();
                    let cp_count = state.catchpoints.len();
                    let count_color = if paused {
                        theme::PAUSE_FG
                    } else {
                        theme::MUTED
                    };
                    ui.label(
                        egui::RichText::new(format!(
                            "{} / {} / {}",
                            bp_count, wp_count, cp_count
                        ))
                        .monospace()
                        .small()
                        .color(count_color),
                    );
                });
            });
        });
}

fn tool_button(
    ui: &mut egui::Ui,
    label: &str,
    tip: &str,
    fg: Option<eframe::egui::Color32>,
) -> egui::Response {
    let mut text = egui::RichText::new(label).size(15.0);
    if let Some(color) = fg {
        text = text.color(color);
    }
    ui.add(egui::Button::new(text).min_size(egui::vec2(28.0, 24.0)))
        .on_hover_text(tip)
}

fn tab_bar_ui(ui: &mut egui::Ui, state: &mut DebuggerState) {
    ui.horizontal(|ui| {
        let cycle_count = state.breakpoints.iter().filter(|bp| bp.is_cycle()).count();
        let source_count = state.breakpoints.iter().filter(|bp| bp.is_source()).count();
        let tabs = [
            (
                DebuggerTab::Breakpoints,
                format!("Points d'arrêt ({})", state.breakpoints.len()),
                Some(format!(
                    "{} dans le code · {} sur la timeline",
                    source_count, cycle_count
                )),
            ),
            (
                DebuggerTab::Watchpoints,
                format!("Watchpoints ({})", state.watchpoints.len()),
                Some("Sample/label à la lecture : bd, hh, cp…".to_string()),
            ),
            (
                DebuggerTab::Catchpoints,
                format!("Catchpoints ({})", state.catchpoints.len()),
                Some("Erreur JS ou appel API : sound, saw, note…".to_string()),
            ),
        ];
        for (tab, label, hint) in &tabs {
            let selected = state.active_tab == *tab;
            let mut resp = ui.selectable_label(selected, label);
            if let Some(h) = hint {
                resp = resp.on_hover_text(h);
            }
            if resp.clicked() {
                state.active_tab = *tab;
            }
        }
    });
}

fn breakpoints_ui(ui: &mut egui::Ui, state: &mut DebuggerState, actions: &mut Vec<DebuggerAction>) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Cycle :").small().color(theme::MUTED));
        ui.add(
            egui::TextEdit::singleline(&mut state.new_bp_pos)
                .desired_width(64.0)
                .hint_text("2.0"),
        );
        ui.label(egui::RichText::new("Piste :").small().color(theme::MUTED));
        ui.add(
            egui::TextEdit::singleline(&mut state.new_bp_label)
                .desired_width(72.0)
                .hint_text("bd"),
        );
        if ui.button("+ Sur la timeline").clicked() {
            if let Ok(pos) = state.new_bp_pos.trim().parse::<f64>() {
                actions.push(DebuggerAction::AddBreakpointAt(
                    pos,
                    state.new_bp_label.trim().to_string(),
                ));
                state.new_bp_pos.clear();
                state.new_bp_label.clear();
            }
        }
        ui.separator();
        if ui.small_button("Tout désactiver").clicked() {
            actions.push(DebuggerAction::DisableAllBreakpoints);
        }
        if ui.small_button("Tout supprimer").clicked() {
            actions.push(DebuggerAction::ClearAllBreakpoints);
        }
    });
    ui.add_space(6.0);

    if state.breakpoints.is_empty() {
        empty_hint(
            ui,
            "Aucun point d'arrêt",
            "Cliquez à gauche du numéro de ligne dans l'éditeur (F9) ou clic droit sur la règle de la timeline.",
        );
        return;
    }

    let mut source_ids: Vec<usize> = state
        .breakpoints
        .iter()
        .filter(|bp| bp.is_source())
        .map(|bp| bp.id)
        .collect();
    let mut cycle_ids: Vec<usize> = state
        .breakpoints
        .iter()
        .filter(|bp| bp.is_cycle())
        .map(|bp| bp.id)
        .collect();
    source_ids.sort_by_key(|id| {
        state
            .breakpoints
            .iter()
            .find(|bp| bp.id == *id)
            .and_then(|bp| bp.source_line)
            .unwrap_or(0)
    });
    cycle_ids.sort_by(|a, b| {
        let pos_a = state
            .breakpoints
            .iter()
            .find(|bp| bp.id == *a)
            .and_then(|bp| bp.position)
            .unwrap_or(0.0);
        let pos_b = state
            .breakpoints
            .iter()
            .find(|bp| bp.id == *b)
            .and_then(|bp| bp.position)
            .unwrap_or(0.0);
        pos_a
            .partial_cmp(&pos_b)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    egui::ScrollArea::vertical()
        .id_salt("bp_scroll")
        .auto_shrink([false; 2])
        .show(ui, |ui| {
            if !source_ids.is_empty() {
                section_heading(ui, "Dans le code");
                breakpoint_list(ui, state, actions, &source_ids, true);
                ui.add_space(4.0);
            }
            if !cycle_ids.is_empty() {
                section_heading(ui, "Sur la timeline");
                breakpoint_list(ui, state, actions, &cycle_ids, false);
            }
        });
}

fn section_heading(ui: &mut egui::Ui, title: &str) {
    ui.label(
        egui::RichText::new(title)
            .small()
            .strong()
            .color(theme::MUTED),
    );
    ui.add_space(2.0);
}

fn breakpoint_list(
    ui: &mut egui::Ui,
    state: &DebuggerState,
    actions: &mut Vec<DebuggerAction>,
    ids: &[usize],
    is_source: bool,
) {
    let mut to_remove: Option<usize> = None;
    let mut to_toggle: Option<usize> = None;

    for id in ids {
        let Some(bp) = state.breakpoints.iter().find(|bp| bp.id == *id) else {
            continue;
        };
        let _row_id = ui.id().with("bp_row").with(bp.id);
        let enabled = bp.enabled;
        let dot_color = if enabled {
            theme::BP_ENABLED
        } else {
            theme::BP_DISABLED
        };

        let row_response = egui::Frame::new()
            .fill(egui::Color32::TRANSPARENT)
            .corner_radius(4.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (dot_rect, dot_resp) =
                        ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::click());
                    let center = dot_rect.center();
                    if enabled {
                        ui.painter().circle_filled(center, 5.0, dot_color);
                    } else {
                        ui.painter()
                            .circle_stroke(center, 5.0, egui::Stroke::new(1.5, dot_color));
                    }
                    if dot_resp.on_hover_text("Activer / désactiver").clicked() {
                        to_toggle = Some(bp.id);
                    }

                    let summary = if is_source {
                        format!("ligne {}", bp.source_line.unwrap_or_default())
                    } else {
                        format!("{:.3}", bp.position.unwrap_or(0.0))
                    };
                    ui.label(egui::RichText::new(summary).monospace().strong());
                    if !bp.label_filter.is_empty() {
                        ui.label(
                            egui::RichText::new(format!("[{}]", bp.label_filter))
                                .color(egui::Color32::from_rgb(120, 200, 120)),
                        );
                    }
                    ui.label(
                        egui::RichText::new(format!("×{}", bp.hit_count))
                            .small()
                            .color(theme::MUTED),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("✕").on_hover_text("Supprimer").clicked() {
                            to_remove = Some(bp.id);
                        }
                    });
                });
            })
            .response;

        if row_response.hovered() {
            ui.painter()
                .rect_filled(row_response.rect, 4.0, theme::ROW_HOVER);
        }
    }

    if let Some(id) = to_remove {
        actions.push(DebuggerAction::RemoveBreakpoint(id));
    }
    if let Some(id) = to_toggle {
        actions.push(DebuggerAction::ToggleBreakpoint(id));
    }
}

fn watchpoints_ui(ui: &mut egui::Ui, state: &mut DebuggerState, actions: &mut Vec<DebuggerAction>) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("Label de piste :")
                .small()
                .color(theme::MUTED),
        );
        ui.add(
            egui::TextEdit::singleline(&mut state.new_wp_label)
                .desired_width(140.0)
                .hint_text("bd"),
        );
        if ui.button("+ Ajouter").clicked() {
            let label = state.new_wp_label.trim().to_string();
            if !label.is_empty() {
                actions.push(DebuggerAction::AddWatchpoint(label));
                state.new_wp_label.clear();
            }
        }
    });
    ui.add_space(6.0);

    if state.watchpoints.is_empty() {
        empty_hint(
            ui,
            "Aucun watchpoint",
            "Suit les événements d'une piste par son label.",
        );
        return;
    }

    egui::ScrollArea::vertical()
        .id_salt("wp_scroll")
        .auto_shrink([false; 2])
        .show(ui, |ui| {
            let mut to_remove: Option<usize> = None;
            let mut to_toggle: Option<usize> = None;
            for wp in &state.watchpoints {
                ui.horizontal(|ui| {
                    let color = if wp.enabled {
                        theme::WP_ENABLED
                    } else {
                        theme::BP_DISABLED
                    };
                    if ui
                        .label(egui::RichText::new("◆").color(color))
                        .on_hover_text("Activer/désactiver")
                        .clicked()
                    {
                        to_toggle = Some(wp.id);
                    }
                    ui.label(egui::RichText::new(format!("«{}»", wp.label)).monospace());
                    if let Some(b) = wp.last_event_begin {
                        ui.label(
                            egui::RichText::new(format!("@ {:.3}", b))
                                .small()
                                .color(theme::MUTED),
                        );
                    }
                    ui.label(
                        egui::RichText::new(format!("×{}", wp.hit_count))
                            .small()
                            .color(theme::MUTED),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("✕").clicked() {
                            to_remove = Some(wp.id);
                        }
                    });
                });
            }
            if let Some(id) = to_remove {
                actions.push(DebuggerAction::RemoveWatchpoint(id));
            }
            if let Some(id) = to_toggle {
                actions.push(DebuggerAction::ToggleWatchpoint(id));
            }
        });
}

fn catchpoints_ui(ui: &mut egui::Ui, state: &mut DebuggerState, actions: &mut Vec<DebuggerAction>) {
    ui.label(
        egui::RichText::new(
            "Les watchpoints suivent les samples (bd, hh). Ici — erreurs JS et mots-clés Strudel dans le code.",
        )
        .small()
        .color(theme::MUTED),
    );
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui.button("+ Erreur JS").clicked() {
            actions.push(DebuggerAction::AddCatchpointError);
        }
        ui.label(egui::RichText::new("API:").small().color(theme::MUTED));
        ui.add(
            egui::TextEdit::singleline(&mut state.new_cp_keyword)
                .desired_width(72.0)
                .hint_text("sound"),
        );
        if ui.button("+").clicked() {
            let k = state.new_cp_keyword.trim().to_string();
            if !k.is_empty() {
                actions.push(DebuggerAction::AddCatchpointKeyword(k));
                state.new_cp_keyword.clear();
            }
        }
    });
    ui.horizontal_wrapped(|ui| {
        for &preset in CATCH_KEYWORD_PRESETS {
            if ui.small_button(preset).clicked() {
                actions.push(DebuggerAction::AddCatchpointKeyword(preset.to_string()));
            }
        }
    });
    ui.add_space(6.0);

    if state.catchpoints.is_empty() {
        empty_hint(
            ui,
            "Aucun catchpoint",
            "«+ Erreur JS» ou bouton API (sound, saw, note…) — pause à l'appel dans le pattern.",
        );
        return;
    }

    egui::ScrollArea::vertical()
        .id_salt("cp_scroll")
        .auto_shrink([false; 2])
        .show(ui, |ui| {
            let mut to_remove: Option<usize> = None;
            let mut to_toggle: Option<usize> = None;
            for cp in &state.catchpoints {
                ui.horizontal(|ui| {
                    let color = if cp.enabled {
                        theme::CP_ENABLED
                    } else {
                        theme::BP_DISABLED
                    };
                    if ui
                        .label(egui::RichText::new("▲").color(color))
                        .on_hover_text("Activer/désactiver")
                        .clicked()
                    {
                        to_toggle = Some(cp.id);
                    }
                    ui.label(egui::RichText::new(cp.kind.display()).monospace());
                    ui.label(
                        egui::RichText::new(format!("×{}", cp.hit_count))
                            .small()
                            .color(theme::MUTED),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("✕").clicked() {
                            to_remove = Some(cp.id);
                        }
                    });
                });
            }
            if let Some(id) = to_remove {
                actions.push(DebuggerAction::RemoveCatchpoint(id));
            }
            if let Some(id) = to_toggle {
                actions.push(DebuggerAction::ToggleCatchpoint(id));
            }
        });
}

fn empty_hint(ui: &mut egui::Ui, title: &str, body: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(12.0);
        ui.label(egui::RichText::new(title).strong().color(theme::MUTED));
        ui.add_space(4.0);
        ui.label(egui::RichText::new(body).small().color(theme::MUTED));
        ui.add_space(12.0);
    });
}
