//! Кастомный автодополнитель для StruIDEl.
//! Основан на `egui_code_editor::Completer`, но:
//! - показывает документацию и примеры из `DocStore` рядом со списком подсказок;
//! - позволяет кликнуть по примеру, чтобы вставить его в код.

use std::collections::BTreeSet;
use std::iter::Peekable;
use std::str::Chars;

use egui::{
    self, text_edit::TextEditOutput, text_selection::text_cursor_state::ccursor_previous_word,
    Event, Frame, Modifiers, Sense, Stroke, TextBuffer,
};
use egui_code_editor::{format_token, ColorTheme, Syntax, Token, TokenType};

use crate::doc_json::DocStore;

const ROOT_CHAR: char = ' ';

/// Простой trie для поиска по префиксу (скопирован из `egui_code_editor`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trie {
    root: char,
    is_word: bool,
    leaves: Vec<Trie>,
}

impl Default for Trie {
    fn default() -> Self {
        Self {
            root: ROOT_CHAR,
            is_word: false,
            leaves: vec![],
        }
    }
}

impl PartialOrd for Trie {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Trie {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.root.cmp(&other.root)
    }
}

impl Trie {
    pub fn new(root: char) -> Self {
        Trie {
            root,
            ..Default::default()
        }
    }

    pub fn clear(&mut self) {
        self.leaves.clear();
    }

    pub fn push(&mut self, word: &str) {
        self.push_chars(&mut word.chars());
    }

    pub fn push_chars(&mut self, word: &mut Chars<'_>) {
        if let Some(first) = word.next() {
            if let Some(leaf) = self.leaves.iter_mut().find(|l| l.root == first) {
                leaf.push_chars(word)
            } else {
                let mut new = Trie::new(first);
                new.push_chars(word);
                self.leaves.push(new);
            }
        } else {
            self.is_word = true;
            self.leaves.sort();
            self.leaves.reverse();
        }
    }

    pub fn from_words(words: &[&str]) -> Self {
        let mut trie = Trie::new(ROOT_CHAR);
        for w in words {
            trie.push_chars(&mut w.chars());
        }
        trie
    }

    pub fn words(&self) -> Vec<String> {
        let mut words = Vec::new();
        for child in &self.leaves {
            child.words_recursive("", &mut words);
        }
        words.reverse();
        words
    }

    fn words_recursive(&self, prefix: &str, words: &mut Vec<String>) {
        let mut prefix = prefix.to_string();
        prefix.push(self.root);
        if self.is_word {
            words.push(prefix.clone());
        }
        for child in &self.leaves {
            child.words_recursive(&prefix, words);
        }
    }

    pub fn find_completions(&self, prefix: &str) -> Vec<String> {
        self.find_by_prefix(prefix)
            .map(|t| t.words())
            .unwrap_or_default()
    }

    pub fn find_by_prefix(&self, prefix: &str) -> Option<&Trie> {
        let mut found: Option<&Trie> = None;
        let mut start = " ".to_string();
        start.push_str(prefix);
        let mut part = start.chars().peekable();
        self.find_recursive(&mut part, &mut found);
        found
    }

    fn find_recursive<'a, 'b>(
        &'a self,
        part: &mut Peekable<Chars<'b>>,
        found: &mut Option<&'a Trie>,
    ) {
        if let Some(c) = part.next() {
            if self.root == c {
                if part.peek().is_none() {
                    *found = Some(self);
                }
                for leaf in &self.leaves {
                    leaf.find_recursive(&mut part.clone(), found);
                }
            }
        }
    }
}

impl From<&Syntax> for Trie {
    fn from(syntax: &Syntax) -> Trie {
        let mut trie = Trie::default();

        syntax.keywords.iter().for_each(|word| trie.push(word));
        syntax.types.iter().for_each(|word| trie.push(word));
        syntax.special.iter().for_each(|word| trie.push(word));
        if !syntax.case_sensitive {
            syntax
                .keywords
                .iter()
                .for_each(|word| trie.push(&word.to_lowercase()));
            syntax
                .types
                .iter()
                .for_each(|word| trie.push(&word.to_lowercase()));
            syntax
                .special
                .iter()
                .for_each(|word| trie.push(&word.to_lowercase()));
        }
        trie
    }
}

#[derive(Default, Debug, Clone, PartialEq)]
/// Автодополнитель с попапом и панелью документации.
pub struct StrudelCompleter {
    prefix: String,
    cursor: usize,
    ignore_cursor: Option<usize>,
    trie_syntax: Trie,
    trie_user: Option<Trie>,
    variant_id: usize,
    completions: BTreeSet<String>,
    /// Плавная анимация высоты панели документации.
    doc_height: f32,
    /// Id редактора, чтобы возвращать ему фокус после кликов по доке.
    editor_id: Option<egui::Id>,
    /// Стадия обработки Escape: 0 — обычная, 1 — после перевода фокуса в редактор.
    esc_stage: u8,
    /// Нужно ли после обработки ввода вернуть фокус редактору (выставляется в handle_input).
    focus_editor_after_input: bool,
    /// Были ли клики/взаимодействия внутри попапов (список или дока).
    popup_interacted: bool,
}

impl StrudelCompleter {
    /// Completer должен храниться в состоянии редактора.
    pub fn new_with_syntax(syntax: &Syntax) -> Self {
        StrudelCompleter {
            trie_syntax: Trie::from(syntax),
            ..Default::default()
        }
    }

    pub fn with_user_words(self) -> Self {
        StrudelCompleter {
            trie_user: Some(Trie::default()),
            ..self
        }
    }

    pub fn push_word(&mut self, word: &str) {
        self.trie_syntax.push(word);
    }

    /// Обработка стрелок/Tab/Esc.
    pub fn handle_input(&mut self, ctx: &egui::Context) {
        if self.prefix.is_empty() {
            return;
        }
        if let Some(cursor) = self.ignore_cursor {
            if cursor == self.cursor {
                return;
            }
        }

        let completions_syntax = self.trie_syntax.find_completions(&self.prefix);
        let completions_user = self
            .trie_user
            .as_ref()
            .map(|t| t.find_completions(&self.prefix))
            .unwrap_or_default();
        // Объединяем и убираем дубликаты без учёта регистра (чтобы не было `sometimesBy` и `sometimesby` одновременно).
        let mut merged = BTreeSet::new();
        for c in completions_syntax.into_iter().chain(completions_user) {
            let key = c.to_ascii_lowercase();
            if !merged
                .iter()
                .any(|existing: &String| existing.to_ascii_lowercase() == key)
            {
                merged.insert(c);
            }
        }
        self.completions = merged;
        if self.completions.is_empty() {
            return;
        }
        let last = self.completions.len().saturating_sub(1);

        // Editor-mode: не было взаимодействий с попапами → считаем, что фокус в редакторе.
        let editor_has_focus = !self.popup_interacted;

        self.focus_editor_after_input = false;
        ctx.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, egui::Key::Escape) {
                if editor_has_focus {
                    // Если фокус уже в редакторе, один Escape сразу закрывает попап.
                    self.ignore_cursor = Some(self.cursor);
                    self.esc_stage = 0;
                    self.popup_interacted = false;
                    self.focus_editor_after_input = false;
                } else {
                    // Первая Escape: вернуть фокус в редактор, попап оставить.
                    // Вторая Escape подряд: закрыть попап (как раньше).
                    if let Some(_editor_id) = self.editor_id {
                        if self.esc_stage == 0 {
                            // Сигнализируем, что после выхода из input_mut нужно вернуть фокус.
                            self.focus_editor_after_input = true;
                            self.esc_stage = 1;
                        } else {
                            self.ignore_cursor = Some(self.cursor);
                            self.esc_stage = 0;
                            self.popup_interacted = false;
                        }
                    } else {
                        self.ignore_cursor = Some(self.cursor);
                        self.esc_stage = 0;
                        self.popup_interacted = false;
                    }
                }
            } else if i.consume_key(Modifiers::NONE, egui::Key::ArrowDown) {
                self.variant_id = if self.variant_id == last {
                    0
                } else {
                    self.variant_id.saturating_add(1).min(last)
                };
            } else if i.consume_key(Modifiers::NONE, egui::Key::ArrowUp) {
                self.variant_id = if self.variant_id == 0 {
                    last
                } else {
                    self.variant_id.saturating_sub(1)
                };
            } else if i.consume_key(Modifiers::NONE, egui::Key::Enter) {
                // Enter — принять текущую подсказку (как Tab).
                let completion = self
                    .completions
                    .iter()
                    .nth(self.variant_id)
                    .map(String::from)
                    .unwrap_or_default();
                if !completion.is_empty() {
                    i.events.push(Event::Paste(completion));
                }
            } else if i.consume_key(Modifiers::NONE, egui::Key::Tab) {
                let completion = self
                    .completions
                    .iter()
                    .nth(self.variant_id)
                    .map(String::from)
                    .unwrap_or_default();
                if !completion.is_empty() {
                    i.events.push(Event::Paste(completion));
                }
            }
        });

        // ВАЖНО: любые изменения memory делаем СТРОГО вне input_mut, иначе возможен дедлок в epaint.
        if self.focus_editor_after_input {
            if let Some(editor_id) = self.editor_id {
                ctx.memory_mut(|mem| mem.request_focus(editor_id));
            }
            self.focus_editor_after_input = false;
        }
    }

    /// Показ попапа автодополнения с панелью документации.
    pub fn show(
        &mut self,
        syntax: &Syntax,
        theme: &ColorTheme,
        fontsize: f32,
        editor_output: &mut TextEditOutput,
        doc_store: Option<&DocStore>,
    ) {
        let ctx = editor_output.response.ctx.clone();
        let galley = &editor_output.galley;

        // Запоминаем Id редактора для последующего возврата фокуса.
        self.editor_id = Some(editor_output.response.id);

        if editor_output.response.changed() {
            // Обновляем пользовательский словарь.
            if let Some(trie_user) = self.trie_user.as_mut() {
                trie_user.clear();
                Token::default()
                    .tokens(syntax, galley.text())
                    .iter()
                    .filter(|t| matches!(t.ty(), TokenType::Literal | TokenType::Function))
                    .for_each(|t| trie_user.push(t.buffer()));
            }
        }

        // Автодополнение.
        let cursor_range = editor_output.state.cursor.char_range();
        if let Some(range) = cursor_range {
            let cursor = range.primary;
            let cursor_pos_in_galley = galley.pos_from_cursor(cursor);
            let cursor_rect =
                cursor_pos_in_galley.translate(editor_output.response.rect.left_top().to_vec2());
            let word_start = ccursor_previous_word(galley.text(), cursor);

            if self.cursor != cursor.index {
                self.cursor = cursor.index;
                self.prefix.clear();
                self.ignore_cursor = None;
                self.variant_id = 0;
                self.esc_stage = 0;
                self.popup_interacted = false;
            }

            if self.ignore_cursor.is_some() && self.ignore_cursor == Some(self.cursor) {
                editor_output.response.request_focus();
                // Сбросим состояние ESC, так как попап будет закрыт.
                self.esc_stage = 0;
                self.popup_interacted = false;
                return;
            } else {
                // Если ignore_cursor не установлен или курсор ушёл, просто очищаем флаг,
                // но не трогаем esc_stage — он управляется только в handle_input.
                self.ignore_cursor = None;
            }

            let next_char_allows = galley
                .chars()
                .nth(cursor.index)
                .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
                || (range.secondary.index > range.primary.index);

            self.prefix = if next_char_allows {
                let prefix = galley
                    .text()
                    .char_range(word_start.index..cursor.index)
                    .to_string();
                if let Some((_, tail)) =
                    prefix.rsplit_once(|c: char| !(c.is_alphanumeric() || c == '_'))
                {
                    tail.to_string()
                } else {
                    prefix
                }
            } else {
                String::new()
            };

            if self.prefix.is_empty() || self.completions.is_empty() {
                return;
            }

            // Текущий label и отображаемое слово.
            let (selected_label, selected_word) =
                if let Some(completion) = self.completions.iter().nth(self.variant_id) {
                    let display = format!("{}{completion}", &self.prefix);
                    (Some(completion.as_str()), Some(display))
                } else {
                    (None, None)
                };

            // Поиск документации: сначала по точному совпадению, потом — без учёта регистра.
            let selected_doc =
                if let (Some(label), Some(ref display)) = (selected_label, &selected_word) {
                    doc_store.and_then(|store| {
                        #[cfg(debug_assertions)]
                        eprintln!(
                            "[StrudelCompleter] lookup doc for label='{label}', display='{}'",
                            display.trim()
                        );
                        // 1. Прямая попытка по label.
                        if let Some(d) = store.get(label) {
                            #[cfg(debug_assertions)]
                            eprintln!("[StrudelCompleter]   found by label");
                            return Some(d);
                        }
                        // 2. Попытка по отображаемому слову (на случай отличий префикса).
                        if let Some(d) = store.get(display.trim()) {
                            #[cfg(debug_assertions)]
                            eprintln!("[StrudelCompleter]   found by display");
                            return Some(d);
                        }
                        // 3. Case-insensitive поиск по всем меткам.
                        let lower = label.to_ascii_lowercase();
                        let from_labels = store
                            .all_labels()
                            .iter()
                            .find(|l| l.to_ascii_lowercase() == lower)
                            .and_then(|real| store.get(real));
                        if from_labels.is_some() {
                            #[cfg(debug_assertions)]
                            eprintln!("[StrudelCompleter]   found by labels (ci, label)");
                            return from_labels;
                        }
                        // 4. Попытка по display (также без учёта регистра).
                        let display_lower = display.trim().to_ascii_lowercase();
                        store
                            .all_labels()
                            .iter()
                            .find(|l| l.to_ascii_lowercase() == display_lower)
                            .and_then(|real| store.get(real))
                    })
                } else {
                    #[cfg(debug_assertions)]
                    eprintln!(
                        "[StrudelCompleter] no selected label/word, doc_store={}",
                        if doc_store.is_some() { "Some" } else { "None" }
                    );
                    None
                };

            // --- Попап №1: список вариантов автодополнения ---
            let list_width = 190.0;
            let max_rows = 10usize;

            let list_popup = egui::Popup::new(
                egui::Id::new("Completer"),
                ctx.clone(),
                cursor_rect,
                editor_output.response.layer_id,
            )
            .frame(Frame::popup(&ctx.style()).fill(theme.bg()))
            .sense(Sense::empty())
            .show(|ui| {
                ui.response().sense = Sense::empty();
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);

                let rows = self.completions.len().max(1);
                let visible_rows = rows.min(max_rows);

                let row_height = fontsize
                    + ui.style().visuals.widgets.hovered.bg_stroke.width * 2.0
                    + ui.style().spacing.button_padding.y * 2.0
                    + ui.style().spacing.item_spacing.y;
                let list_height =
                    row_height * visible_rows as f32 - ui.style().spacing.item_spacing.y;

                ui.set_width(list_width);
                ui.set_height(list_height);

                egui::ScrollArea::vertical()
                    .auto_shrink([true, true])
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                    .show(ui, |ui| {
                        for (i, completion) in self.completions.iter().enumerate() {
                            let word = format!("{}{completion}", &self.prefix);
                            let token_type = match &word {
                                word if syntax.is_keyword(word) => TokenType::Keyword,
                                word if syntax.is_special(word) => TokenType::Special,
                                word if syntax.is_type(word) => TokenType::Type,
                                _ => TokenType::Literal,
                            };
                            let fmt = format_token(theme, fontsize, token_type);
                            let colored_text = egui::text::LayoutJob::single_section(word, fmt);
                            let selected = i == self.variant_id;

                            let button = ui.add(
                                egui::Button::new(colored_text)
                                    .sense(Sense::click())
                                    .frame(true)
                                    .fill(theme.bg())
                                    .stroke(if selected {
                                        Stroke::new(
                                            ui.style().visuals.widgets.hovered.bg_stroke.width,
                                            theme.type_color(TokenType::Literal),
                                        )
                                    } else {
                                        Stroke::NONE
                                    }),
                            );
                            if button.clicked() {
                                self.variant_id = i;
                                ui.ctx().request_repaint();
                                self.popup_interacted = true;
                            }
                            if selected {
                                button.scroll_to_me(None);
                            }
                        }
                    });
            });

            // Любой клик внутри прямоугольника списка считается взаимодействием с попапом.
            if let Some(list_popup) = list_popup {
                let clicked_in_list = ctx.input(|i| {
                    if !i.pointer.any_click() {
                        false
                    } else if let Some(pos) = i.pointer.latest_pos() {
                        list_popup.response.rect.contains(pos)
                    } else {
                        false
                    }
                });
                if clicked_in_list {
                    self.popup_interacted = true;
                }
            }

            // --- Попап №2: документация, прикреплённая справа от списка ---
            if let Some(doc) = selected_doc {
                let doc_width = 360.0;
                let gap = 8.0;

                // Верхний левый угол док-попапа: сразу справа от списка.
                let doc_pos = egui::pos2(
                    cursor_rect.left_top().x + list_width + gap,
                    cursor_rect.left_top().y,
                );

                // Лёгкая анимация появления/исчезновения по ширине.
                let anim_t = ctx.animate_bool(egui::Id::new("StrudelCompleterDocVisible"), true);
                let animated_width = 180.0 + anim_t * (doc_width - 180.0);

                // Оценка целевой высоты (по количеству строк/примеров) и плавная анимация к ней.
                let mut approx_lines = 0usize;
                if let Some(desc) = &doc.description {
                    approx_lines += desc.lines().count();
                }
                if let Some(params) = &doc.params {
                    approx_lines += params.len() * 2;
                }
                if let Some(examples) = &doc.examples {
                    approx_lines += examples.len() * 3;
                }
                approx_lines = approx_lines.clamp(6, 32);
                let line_height = fontsize * 1.35;
                let target_height = (approx_lines as f32) * line_height;

                // Экспоненциальное сглаживание высоты.
                let alpha = 0.2;
                if self.doc_height <= 0.0 {
                    self.doc_height = target_height;
                } else {
                    self.doc_height = self.doc_height + alpha * (target_height - self.doc_height);
                }
                ctx.request_repaint();

                let doc_area = egui::Area::new(egui::Id::new("StrudelCompleterDocArea"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(doc_pos)
                    .constrain(true)
                    .show(&ctx, |ui| {
                        let visuals = ui.style().visuals.clone();
                        let text_color = visuals.text_color();

                        Frame::popup(&ctx.style()).fill(theme.bg()).show(ui, |ui| {
                            ui.set_width(animated_width);
                            let max_height = 420.0_f32;
                            if self.doc_height > 0.0 {
                                ui.set_min_height(self.doc_height.min(max_height));
                            }
                            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);

                            egui::ScrollArea::vertical()
                                .auto_shrink([true, true])
                                .max_height(max_height)
                                .show(ui, |ui| {
                                    ui.label(
                                        egui::RichText::new(&doc.name).strong().color(text_color),
                                    );
                                    if let Some(desc) = &doc.description {
                                        let clean = desc.replace("<p>", "").replace("</p>", "\n");
                                        if !clean.trim().is_empty() {
                                            ui.label(
                                                egui::RichText::new(clean.trim()).color(text_color),
                                            );
                                        }
                                    }
                                    if let Some(params) = &doc.params {
                                        ui.add_space(4.0);
                                        ui.label(
                                            egui::RichText::new("Parameters:")
                                                .strong()
                                                .color(text_color),
                                        );
                                        for p in params {
                                            let ty = p
                                                .r#type
                                                .as_ref()
                                                .and_then(|t| t.names.as_ref())
                                                .map(|n| n.join(" | "))
                                                .unwrap_or_default();
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "  {}: {}",
                                                    p.name, ty
                                                ))
                                                .color(text_color),
                                            );
                                        }
                                    }
                                    if let Some(examples) = &doc.examples {
                                        ui.add_space(4.0);
                                        ui.label(
                                            egui::RichText::new("Examples (click to insert):")
                                                .strong()
                                                .color(text_color),
                                        );
                                        for ex in examples {
                                            let button = ui.add(
                                                egui::Button::new(
                                                    egui::RichText::new(ex)
                                                        .monospace()
                                                        .color(text_color),
                                                )
                                                .fill(theme.bg())
                                                .stroke(Stroke::new(
                                                    visuals.widgets.inactive.bg_stroke.width,
                                                    visuals.widgets.inactive.bg_stroke.color,
                                                )),
                                            );
                                            if button.clicked() {
                                                ctx.input_mut(|i| {
                                                    i.events.push(Event::Paste(ex.clone()));
                                                });
                                                if let Some(id) = self.editor_id {
                                                    ctx.memory_mut(|mem| mem.request_focus(id));
                                                }
                                                self.popup_interacted = true;
                                            }
                                        }
                                    }
                                });
                        });
                    });

                // Любой клик внутри прямоугольника док-попапа тоже считается взаимодействием.
                let clicked_in_doc = ctx.input(|i| {
                    if !i.pointer.any_click() {
                        false
                    } else if let Some(pos) = i.pointer.latest_pos() {
                        doc_area.response.rect.contains(pos)
                    } else {
                        false
                    }
                });
                if clicked_in_doc {
                    self.popup_interacted = true;
                }
            } else {
                // Обновляем анимацию, чтобы док-попап плавно скрывался.
                let _ = ctx.animate_bool(egui::Id::new("StrudelCompleterDocVisible"), false);
            }
        }
    }
}
