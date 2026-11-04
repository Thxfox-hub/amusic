//! Таймлайн: события из queryArc, визуальное отображение как в web IDE —
//! сетка времени, полоска воспроизведения, треки по label с цветными барами.
//! Группировка по категориям (ударные, клавиши и т.д.) при наличии SampleCategories.
//! Рисует реальную волну семпла, если доступна через get_waveform.

use crate::arrangement_ir::StableId;
use crate::pattern_codegen::{PatternEdit, PatternEditKind};
use crate::pattern_ir::{PatternEvent, PatternIr, PatternTrack, PatternValue};
use crate::sample_categories::SampleCategories;
use crate::ui::debugger::DebuggerState;
use crate::ui::EditorTab;
use eframe::egui;
use std::collections::HashMap;
use std::sync::Arc;

/// Действия, возвращаемые таймлайном в app (добавить брейкпоинт, сик и т.д.).
pub enum TimelineAction {
    AddBreakpoint(f64),
    RemoveBreakpoint(usize),
    ToggleSourceBreakpoint(usize),
    AddWatchpoint(String),
    Seek(f64),
    RemoveSoundToken { sample: String, begin: f64 },
    ApplyPatternEdit(PatternEdit),
}

const DEFAULT_LABEL_WIDTH: f32 = 120.0;
const PIXELS_PER_CYCLE: f32 = 100.0;
const MIN_ZOOM: f64 = 0.25;
const MAX_ZOOM: f64 = 4.0;
const DEFAULT_ROW_HEIGHT: f32 = 28.0;
const RULER_HEIGHT: f32 = 28.0;
/// Ширина зоны плавного появления/исчезновения элементов у краёв (в пикселях).
const FADE_EDGE_WIDTH: f32 = 48.0;

/// Цвета временной шкалы
fn ruler_bg() -> egui::Color32 {
    egui::Color32::from_gray(26)
}
fn ruler_label_bg() -> egui::Color32 {
    egui::Color32::from_gray(32)
}
fn grid_major() -> egui::Color32 {
    egui::Color32::from_gray(60)
}
fn grid_minor() -> egui::Color32 {
    egui::Color32::from_gray(45)
}
fn ruler_text() -> egui::Color32 {
    egui::Color32::from_gray(210)
}
fn ruler_text_dim() -> egui::Color32 {
    egui::Color32::from_gray(140)
}

/// Состояние вида таймлайна.
pub struct TimelineViewState {
    /// Множитель зума: 1.0 = базовый масштаб.
    pub zoom: f64,
    /// Начало видимого окна (в циклах). Обновляется: при воспроизведении — за playhead, иначе — за курсором.
    pub view_start: f64,
    /// Состояние сворачивания групп по банкам: key -> collapsed.
    pub bank_folds: HashMap<String, bool>,
    /// Состояние сворачивания категорий (Ударные, Клавиши и т.д.): category_key -> collapsed.
    pub category_folds: HashMap<String, bool>,
    /// Текущая ширина колонки с названиями треков/банков.
    pub label_width: f32,
    /// Текущая высота строки трека/банка.
    pub row_height: f32,
    /// Индивидуальные высоты дорожек (по track_id), если пользователь менял их вручную.
    pub track_heights: HashMap<String, f32>,
    /// Пользовательские цвета треков (по track_id).
    pub track_colors: HashMap<String, egui::Color32>,
    /// Активный трек, для которого открыт попап выбора цвета.
    pub color_popup_track: Option<String>,
    /// Позиция попапа выбора цвета (рядом с кликнутым лейблом).
    pub color_popup_pos: Option<egui::Pos2>,
    /// Настройка: для каких треков всегда рисовать волну, независимо от зума.
    pub track_always_waveform: HashMap<String, bool>,
    /// Настройка: для каких треков показывать время событий на барах.
    pub track_show_time: HashMap<String, bool>,
    /// Выбранное событие Strudel Pattern Editor.
    pub selected_pattern_event: Option<StableId>,
    /// Текущее перетаскивание события Strudel Pattern Editor.
    pattern_drag: Option<PatternDragState>,
    /// Событие, над правым краем которого сейчас находится курсор (для определения ресайза).
    hover_resize_event: Option<StableId>,
    /// Буфер обмена: имя скопированного сэмпла.
    pub clipboard: Option<String>,
}

#[derive(Debug, Clone)]
struct PatternDragState {
    event_id: StableId,
    cycle_offset: f64,
    mode: PatternDragMode,
    pending_edit: Option<PatternEdit>,
    start_pointer_x: f32,
    origin_begin: f64,
    origin_end: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternDragMode {
    Move,
    ResizeEnd,
}

impl Default for TimelineViewState {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            view_start: 0.0,
            bank_folds: HashMap::new(),
            category_folds: HashMap::new(),
            label_width: DEFAULT_LABEL_WIDTH,
            row_height: DEFAULT_ROW_HEIGHT,
            track_heights: HashMap::new(),
            track_colors: HashMap::new(),
            color_popup_track: None,
            color_popup_pos: None,
            track_always_waveform: HashMap::new(),
            track_show_time: HashMap::new(),
            selected_pattern_event: None,
            pattern_drag: None,
            hover_resize_event: None,
            clipboard: None,
        }
    }
}

fn parse_hex_color(s: &str) -> egui::Color32 {
    let s = s.trim_start_matches('#');
    if s.len() >= 6 {
        let r = u8::from_str_radix(&s[0..2], 16).unwrap_or(0x94);
        let g = u8::from_str_radix(&s[2..4], 16).unwrap_or(0xA3);
        let b = u8::from_str_radix(&s[4..6], 16).unwrap_or(0xB8);
        return egui::Color32::from_rgb(r, g, b);
    }
    egui::Color32::from_rgb(0x94, 0xA3, 0xB8)
}

fn format_cycle(t: f64) -> String {
    if t < 0.0 {
        return format!("{:.1}", t);
    }
    let whole = t.floor() as i32;
    let frac = t - (whole as f64);
    if frac.abs() < 0.001 {
        return whole.to_string();
    }
    let denom = 8;
    let num = (frac * (denom as f64)).round() as i32;
    if num == 0 {
        return whole.to_string();
    }
    if num >= denom {
        return (whole + 1).to_string();
    }
    let g = gcd(num.abs(), denom);
    format!("{} {}/{}", whole, num / g, denom / g)
}

/// Короткая подпись для подделений (1/4, 2/4, 3/4).
fn format_subdivision(whole: i32, num: i32, denom: i32) -> String {
    if whole == 0 && num == 0 {
        "0".to_string()
    } else if whole == 0 {
        format!("{}/{}", num, denom)
    } else {
        format!("{} {}/{}", whole, num, denom)
    }
}

fn gcd(a: i32, b: i32) -> i32 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

struct PatternTrackGroup<'a> {
    category_key: String,
    category_title: String,
    banks: Vec<PatternBankGroup<'a>>,
}

struct PatternBankGroup<'a> {
    key: String,
    title: String,
    tracks: Vec<&'a PatternTrack>,
}

fn group_pattern_tracks<'a>(
    tracks: Vec<&'a PatternTrack>,
    categories: Option<&SampleCategories>,
) -> Vec<PatternTrackGroup<'a>> {
    let mut by_category: HashMap<String, PatternTrackGroup<'a>> = HashMap::new();
    for track in tracks {
        let bank_title = track.bank.clone().unwrap_or_else(|| "no bank".to_string());
        let bank_key = track
            .bank
            .as_deref()
            .map(|bank| bank.trim().to_lowercase())
            .unwrap_or_else(|| "no-bank".to_string());
        let (category_key, category_title) = categories
            .map(|categories| categories.category_for_bank(&bank_key))
            .map(|info| (info.key, info.label))
            .unwrap_or_else(|| ("all".to_string(), "Tous".to_string()));

        let category = by_category
            .entry(category_key.clone())
            .or_insert_with(|| PatternTrackGroup {
                category_key: category_key.clone(),
                category_title,
                banks: Vec::new(),
            });
        if let Some(bank) = category.banks.iter_mut().find(|bank| bank.key == bank_key) {
            bank.tracks.push(track);
        } else {
            category.banks.push(PatternBankGroup {
                key: bank_key,
                title: bank_title,
                tracks: vec![track],
            });
        }
    }

    let mut groups: Vec<_> = by_category.into_values().collect();
    let category_rank = |key: &str| -> usize {
        categories
            .and_then(|categories| categories.category_order().iter().position(|item| item == key))
            .unwrap_or(usize::MAX)
    };
    groups.sort_by(|a, b| {
        category_rank(&a.category_key)
            .cmp(&category_rank(&b.category_key))
            .then_with(|| a.category_title.cmp(&b.category_title))
    });
    for category in &mut groups {
        category.banks.sort_by(|a, b| a.title.cmp(&b.title));
        for bank in &mut category.banks {
            bank.tracks.sort_by(|a, b| a.title.cmp(&b.title).then_with(|| a.id.cmp(&b.id)));
        }
    }
    groups
}

/// Callback для Pattern Editor: по банку/семплу возвращает downsampled waveform.
pub type PatternWaveformLookup = dyn Fn(Option<&str>, &str) -> Option<Arc<[f32]>>;

pub fn pattern_timeline_ui(
    ui: &mut egui::Ui,
    code: &str,
    ir: &PatternIr,
    diagnostics: &[crate::pattern_ir::PatternDiagnostic],
    current_time: f64,
    playing: bool,
    view_state: &mut TimelineViewState,
    debugger: Option<&DebuggerState>,
    categories: Option<&SampleCategories>,
    waveform: Option<&PatternWaveformLookup>,
) -> Vec<TimelineAction> {
    let mut actions = Vec::new();
    if !ui.input(|input| input.pointer.primary_down()) {
        if let Some(drag) = view_state.pattern_drag.take() {
            if let Some(edit) = drag.pending_edit {
                actions.push(TimelineAction::ApplyPatternEdit(edit));
            }
        }
    }

    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.heading("Timeline");
            ui.add_space(12.0);
            ui.label(
                egui::RichText::new(format!("▶ {} cycles", format_cycle(current_time)))
                    .color(egui::Color32::from_rgb(0x94, 0xA3, 0xB8))
                    .monospace(),
            );
            ui.add_space(8.0);
            if ui.small_button("⟲ Vers la tête de lecture").clicked() {
                view_state.view_start = (current_time - 2.0).max(0.0);
            }
            if let Some(diag) = diagnostics.first() {
                ui.label(
                    egui::RichText::new(&diag.message)
                        .color(egui::Color32::from_rgb(0xF6, 0xC1, 0x77)),
                );
            }
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("snap:");
            ui.label(egui::RichText::new("1/16").small());
            ui.separator();
            if ui.button("−").clicked() {
                view_state.zoom = (view_state.zoom / 1.2).clamp(MIN_ZOOM, MAX_ZOOM);
            }
            ui.add(
                egui::DragValue::new(&mut view_state.zoom)
                    .range(MIN_ZOOM..=MAX_ZOOM)
                    .speed(0.05)
                    .prefix("zoom: "),
            );
            if ui.button("+").clicked() {
                view_state.zoom = (view_state.zoom * 1.2).clamp(MIN_ZOOM, MAX_ZOOM);
            }
            ui.separator();
            ui.label("labels:");
            ui.add(
                egui::DragValue::new(&mut view_state.label_width)
                    .range(60.0..=260.0)
                    .speed(1.0),
            );
            ui.label("row:");
            ui.add(
                egui::DragValue::new(&mut view_state.row_height)
                    .range(18.0..=80.0)
                    .speed(0.5),
            );
        });

        let label_width = view_state.label_width.clamp(60.0, 260.0);
        let row_height = view_state.row_height.clamp(18.0, 80.0);
        view_state.label_width = label_width;
        view_state.row_height = row_height;
        view_state.zoom = view_state.zoom.clamp(MIN_ZOOM, MAX_ZOOM);

        let track_width_px = (ui.available_width() - label_width).max(100.0);
        let pixels_per_cycle = PIXELS_PER_CYCLE as f64 * view_state.zoom;
        let visible_cycles = (track_width_px as f64 / pixels_per_cycle).max(0.001);
        if playing {
            view_state.view_start = (current_time - visible_cycles * 0.25).max(0.0);
        }
        let time_start = view_state.view_start;
        let time_end = view_state.view_start + visible_cycles;
        let time_to_x = |t: f64| -> f32 { ((t - time_start) * pixels_per_cycle) as f32 };
        let total_width = label_width + track_width_px;

        // ─── Keyboard shortcuts ───────────────────────────────────────────────
        let selected_event = view_state
            .selected_pattern_event
            .as_ref()
            .and_then(|id| ir.events.iter().find(|ev| &ev.id == id));

        if ui.input(|i| i.key_pressed(egui::Key::Delete)) {
            if let Some(event) = selected_event {
                actions.push(TimelineAction::ApplyPatternEdit(edit_for_event(
                    event,
                    PatternEditKind::DeleteEvent,
                )));
            }
        }
        if ui.input(|i| i.key_pressed(egui::Key::C) && i.modifiers.ctrl) {
            if let Some(event) = selected_event {
                if let Some(sample) = event.sample() {
                    view_state.clipboard = Some(sample.to_string());
                }
            }
        }
        if ui.input(|i| i.key_pressed(egui::Key::V) && i.modifiers.ctrl) {
            if let (Some(event), Some(sample)) = (selected_event, view_state.clipboard.clone()) {
                actions.push(TimelineAction::ApplyPatternEdit(edit_for_event(
                    event,
                    PatternEditKind::PasteAfterEvent { sample },
                )));
            }
        }
        for (key, n) in [
            (egui::Key::Num2, 2usize),
            (egui::Key::Num3, 3),
            (egui::Key::Num4, 4),
        ] {
            if ui.input(|i| i.key_pressed(key)) {
                if let Some(event) = selected_event {
                    actions.push(TimelineAction::ApplyPatternEdit(edit_for_event(
                        event,
                        PatternEditKind::SplitIntoN { n },
                    )));
                }
            }
        }

        // ─── Selected-event action bar ────────────────────────────────────────
        if let Some(event) = selected_event {
            let sample_label = event.sample().unwrap_or("?");
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("✦ {sample_label}"))
                        .color(egui::Color32::from_rgb(0x38, 0xBD, 0xF8))
                        .monospace(),
                );
                ui.separator();
                if ui.small_button("÷2  [2]").clicked() {
                    actions.push(TimelineAction::ApplyPatternEdit(edit_for_event(
                        event,
                        PatternEditKind::SplitIntoN { n: 2 },
                    )));
                }
                if ui.small_button("÷3  [3]").clicked() {
                    actions.push(TimelineAction::ApplyPatternEdit(edit_for_event(
                        event,
                        PatternEditKind::SplitIntoN { n: 3 },
                    )));
                }
                if ui.small_button("÷4  [4]").clicked() {
                    actions.push(TimelineAction::ApplyPatternEdit(edit_for_event(
                        event,
                        PatternEditKind::SplitIntoN { n: 4 },
                    )));
                }
                ui.separator();
                if ui.small_button("Copy  [Ctrl+C]").clicked() {
                    if let Some(sample) = event.sample() {
                        view_state.clipboard = Some(sample.to_string());
                    }
                }
                if let Some(clip) = &view_state.clipboard.clone() {
                    if ui
                        .small_button(format!("Paste «{clip}»  [Ctrl+V]"))
                        .clicked()
                    {
                        actions.push(TimelineAction::ApplyPatternEdit(edit_for_event(
                            event,
                            PatternEditKind::PasteAfterEvent {
                                sample: clip.clone(),
                            },
                        )));
                    }
                }
                ui.separator();
                if ui.small_button("Delete  [Del]").clicked() {
                    actions.push(TimelineAction::ApplyPatternEdit(edit_for_event(
                        event,
                        PatternEditKind::DeleteEvent,
                    )));
                }
            });
            ui.add_space(4.0);
        }

        let tracks: Vec<_> = ir
            .tracks
            .iter()
            .filter(|track| {
                ir.events
                    .iter()
                    .any(|ev| ev.track_id == track.id && !matches!(ev.value, PatternValue::Rest))
            })
            .collect();
        let track_groups = group_pattern_tracks(tracks, categories);

        egui::ScrollArea::both()
            .id_salt("pattern_timeline_scroll")
            .max_height(ui.available_height())
            .show(ui, |ui| {
                let (ruler_rect, ruler_resp) = ui.allocate_exact_size(
                    egui::vec2(total_width, RULER_HEIGHT),
                    egui::Sense::click_and_drag(),
                );
                if !playing && ruler_resp.dragged() && view_state.pattern_drag.is_none() {
                    view_state.view_start -= (ruler_resp.drag_delta().x as f64) / pixels_per_cycle;
                    view_state.view_start =
                        (view_state.view_start.max(0.0) * 100.0).round() / 100.0;
                }
                draw_pattern_ruler(
                    ui,
                    ruler_rect,
                    label_width,
                    track_width_px,
                    time_start,
                    time_end,
                    current_time,
                    &time_to_x,
                );

                for category in track_groups {
                    let mut category_collapsed = *view_state
                        .category_folds
                        .get(&category.category_key)
                        .unwrap_or(&false);
                    let (cat_rect, cat_resp) = ui.allocate_exact_size(
                        egui::vec2(total_width, row_height),
                        egui::Sense::click_and_drag(),
                    );
                    if cat_resp.clicked() {
                        category_collapsed = !category_collapsed;
                        view_state
                            .category_folds
                            .insert(category.category_key.clone(), category_collapsed);
                    }
                    if !playing && cat_resp.dragged() && view_state.pattern_drag.is_none() {
                        view_state.view_start = (view_state.view_start
                            - cat_resp.drag_delta().x as f64 / pixels_per_cycle)
                            .max(0.0);
                    }
                    let cat_painter = ui.painter_at(cat_rect);
                    cat_painter.rect_filled(cat_rect, 0.0, egui::Color32::from_gray(24));
                    cat_painter.rect_filled(
                        egui::Rect::from_min_size(
                            cat_rect.left_top(),
                            egui::vec2(label_width, cat_rect.height()),
                        ),
                        0.0,
                        egui::Color32::from_gray(34),
                    );
                    let cat_arrow = if category_collapsed { "▸" } else { "▾" };
                    cat_painter.text(
                        egui::pos2(cat_rect.left() + 6.0, cat_rect.center().y - 7.0),
                        egui::Align2::LEFT_TOP,
                        format!("{cat_arrow} {}", category.category_title),
                        egui::FontId::proportional(12.0),
                        egui::Color32::from_rgb(0x94, 0xA3, 0xB8),
                    );
                    draw_pattern_grid(
                        &cat_painter,
                        cat_rect,
                        label_width,
                        track_width_px,
                        time_start,
                        time_end,
                        &time_to_x,
                    );
                    if category_collapsed {
                        continue;
                    }

                    for bank in category.banks {
                        let bank_fold_key = format!("pattern:{}", bank.key);
                        let mut bank_collapsed = *view_state
                            .bank_folds
                            .get(&bank_fold_key)
                            .unwrap_or(&false);
                        let (bank_rect, bank_resp) = ui.allocate_exact_size(
                            egui::vec2(total_width, row_height),
                            egui::Sense::click_and_drag(),
                        );
                        if bank_resp.clicked() {
                            bank_collapsed = !bank_collapsed;
                            view_state
                                .bank_folds
                                .insert(bank_fold_key.clone(), bank_collapsed);
                        }
                        if !playing && bank_resp.dragged() && view_state.pattern_drag.is_none() {
                            view_state.view_start = (view_state.view_start
                                - bank_resp.drag_delta().x as f64 / pixels_per_cycle)
                                .max(0.0);
                        }
                        let bank_painter = ui.painter_at(bank_rect);
                        bank_painter.rect_filled(bank_rect, 0.0, egui::Color32::from_gray(26));
                        bank_painter.rect_filled(
                            egui::Rect::from_min_size(
                                bank_rect.left_top(),
                                egui::vec2(label_width, bank_rect.height()),
                            ),
                            0.0,
                            egui::Color32::from_gray(36),
                        );
                        let bank_arrow = if bank_collapsed { "▸" } else { "▾" };
                        bank_painter.text(
                            egui::pos2(bank_rect.left() + 14.0, bank_rect.center().y - 7.0),
                            egui::Align2::LEFT_TOP,
                            format!("{bank_arrow} {}", bank.title),
                            egui::FontId::monospace(11.0),
                            egui::Color32::from_rgb(0xCB, 0xD5, 0xE1),
                        );
                        draw_pattern_grid(
                            &bank_painter,
                            bank_rect,
                            label_width,
                            track_width_px,
                            time_start,
                            time_end,
                            &time_to_x,
                        );
                        if bank_collapsed {
                            continue;
                        }

                        for track in bank.tracks {
                            let track_height = view_state
                                .track_heights
                                .get(track.id.as_str())
                                .copied()
                                .unwrap_or(row_height);
                            let (row_rect, row_resp) = ui.allocate_exact_size(
                                egui::vec2(total_width, track_height),
                                egui::Sense::click_and_drag(),
                            );
                            if !playing && row_resp.dragged() && view_state.pattern_drag.is_none() {
                                view_state.view_start = (view_state.view_start
                                    - row_resp.drag_delta().x as f64 / pixels_per_cycle)
                                    .max(0.0);
                            }
                            let painter = ui.painter_at(row_rect);
                            painter.rect_filled(row_rect, 0.0, egui::Color32::from_gray(28));
                            painter.rect_filled(
                                egui::Rect::from_min_size(
                                    row_rect.left_top(),
                                    egui::vec2(label_width, row_rect.height()),
                                ),
                                0.0,
                                egui::Color32::from_gray(36),
                            );
                            let track_color = view_state
                                .track_colors
                                .get(track.id.as_str())
                                .copied()
                                .unwrap_or_else(|| egui::Color32::from_rgb(0xE2, 0xE8, 0xF0));
                            painter.text(
                                egui::pos2(row_rect.left() + 24.0, row_rect.center().y - 6.0),
                                egui::Align2::LEFT_TOP,
                                ellipsize(&track.title, ((label_width - 32.0) / 7.0).max(4.0) as usize),
                                egui::FontId::monospace(11.0),
                                track_color,
                            );
                            let label_rect = egui::Rect::from_min_size(
                                row_rect.left_top(),
                                egui::vec2(label_width, row_rect.height()),
                            );
                            let label_resp = ui.interact(
                                label_rect,
                                ui.id().with(("pattern_track_label", track.id.as_str())),
                                egui::Sense::click(),
                            );
                            if label_resp.clicked() {
                                view_state.color_popup_track = Some(track.id.0.clone());
                                view_state.color_popup_pos = Some(label_rect.right_top() + egui::vec2(8.0, 8.0));
                            }
                            label_resp.context_menu(|ui| {
                                if ui.button(format!("◆ Watchpoint sur « {} »", track.title)).clicked() {
                                    actions.push(TimelineAction::AddWatchpoint(track.title.clone()));
                                    ui.close_menu();
                                }
                                let always_wave = view_state
                                    .track_always_waveform
                                    .entry(track.id.0.clone())
                                    .or_insert(false);
                                ui.checkbox(always_wave, "Toujours afficher la forme d'onde");
                                let show_time = view_state
                                    .track_show_time
                                    .entry(track.id.0.clone())
                                    .or_insert(false);
                                ui.checkbox(show_time, "Afficher le temps des événements");
                            });

                            draw_pattern_grid(
                                &painter,
                                row_rect,
                                label_width,
                                track_width_px,
                                time_start,
                                time_end,
                                &time_to_x,
                            );

                            let row_insert_at = row_resp
                                .interact_pointer_pos()
                                .map(|pointer| {
                                    ((pointer.x - (row_rect.left() + label_width)) as f64
                                        / pixels_per_cycle
                                        + time_start)
                                        .rem_euclid(1.0)
                                })
                                .unwrap_or(0.0);
                            row_resp.context_menu(|ui| {
                                let at = row_insert_at;
                                if ui.button(format!("Insert rest at {}", format_cycle(at))).clicked() {
                                    actions.push(TimelineAction::ApplyPatternEdit(PatternEdit {
                                        event_id: None,
                                        pattern_id: track.pattern_id.clone(),
                                        token_span: None,
                                        token_value: None,
                                        kind: PatternEditKind::InsertRest {
                                            at: snap_to_grid(at, 1.0 / 16.0),
                                        },
                                    }));
                                    ui.close_menu();
                                }
                            });

                            let first_cycle = time_start.floor() as i32 - 1;
                            let last_cycle = time_end.ceil() as i32 + 1;
                            let track_bank = track.bank.as_deref();
                            for cycle in first_cycle..=last_cycle {
                                let cycle_offset = cycle as f64;
                                for event in ir.events.iter().filter(|event| event.track_id == track.id) {
                                    draw_pattern_event(
                                        ui,
                                        view_state,
                                        code,
                                        event,
                                        track_bank,
                                        cycle_offset,
                                        row_rect,
                                        label_width,
                                        track_width_px,
                                        time_start,
                                        time_end,
                                        current_time,
                                        pixels_per_cycle,
                                        &time_to_x,
                                        debugger,
                                        waveform,
                                        &mut actions,
                                    );
                                }
                            }
                        }
                    }
                }
            });
    });

    actions
}

fn draw_pattern_ruler(
    ui: &egui::Ui,
    ruler_rect: egui::Rect,
    label_width: f32,
    track_width_px: f32,
    time_start: f64,
    time_end: f64,
    current_time: f64,
    time_to_x: &dyn Fn(f64) -> f32,
) {
    let track_left = ruler_rect.left() + label_width;
    let painter = ui.painter_at(ruler_rect);
    painter.rect_filled(ruler_rect, 0.0, ruler_bg());
    painter.rect_filled(
        egui::Rect::from_min_size(
            ruler_rect.left_top(),
            egui::vec2(label_width, ruler_rect.height()),
        ),
        0.0,
        ruler_label_bg(),
    );
    let cycle_start = time_start.floor() as i32;
    let cycle_end = time_end.ceil() as i32;
    for i in (cycle_start * 4)..=((cycle_end + 1) * 4) {
        let t = i as f64 / 4.0;
        let x = track_left + time_to_x(t);
        if x < track_left || x > track_left + track_width_px {
            continue;
        }
        let major = i % 4 == 0;
        painter.line_segment(
            [
                egui::pos2(x, ruler_rect.top()),
                egui::pos2(x, ruler_rect.bottom()),
            ],
            (
                if major { 1.5 } else { 1.0 },
                if major { grid_major() } else { grid_minor() },
            ),
        );
        if major {
            painter.text(
                egui::pos2(x + 6.0, ruler_rect.center().y - 6.0),
                egui::Align2::LEFT_TOP,
                format_cycle(t),
                egui::FontId::monospace(11.0),
                ruler_text(),
            );
        }
    }
    let cursor_x = track_left + time_to_x(current_time);
    if cursor_x >= track_left && cursor_x <= track_left + track_width_px {
        painter.line_segment(
            [
                egui::pos2(cursor_x, ruler_rect.top()),
                egui::pos2(cursor_x, ruler_rect.bottom()),
            ],
            (2.0, egui::Color32::from_rgb(0xE2, 0xE8, 0xF0)),
        );
    }
}

fn draw_pattern_grid(
    painter: &egui::Painter,
    row_rect: egui::Rect,
    label_width: f32,
    track_width_px: f32,
    time_start: f64,
    time_end: f64,
    time_to_x: &dyn Fn(f64) -> f32,
) {
    let track_left = row_rect.left() + label_width;
    let cycle_start = time_start.floor() as i32;
    let cycle_end = time_end.ceil() as i32;
    for i in (cycle_start * 4)..=((cycle_end + 1) * 4) {
        let t = i as f64 / 4.0;
        let x = track_left + time_to_x(t);
        if x < track_left || x > track_left + track_width_px {
            continue;
        }
        let major = i % 4 == 0;
        painter.line_segment(
            [
                egui::pos2(x, row_rect.top()),
                egui::pos2(x, row_rect.bottom()),
            ],
            (
                if major { 1.0 } else { 0.75 },
                if major { grid_major() } else { grid_minor() },
            ),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_pattern_event(
    ui: &mut egui::Ui,
    view_state: &mut TimelineViewState,
    code: &str,
    event: &PatternEvent,
    track_bank: Option<&str>,
    cycle_offset: f64,
    row_rect: egui::Rect,
    label_width: f32,
    track_width_px: f32,
    time_start: f64,
    time_end: f64,
    current_time: f64,
    pixels_per_cycle: f64,
    time_to_x: &dyn Fn(f64) -> f32,
    debugger: Option<&DebuggerState>,
    waveform: Option<&PatternWaveformLookup>,
    actions: &mut Vec<TimelineAction>,
) {
    let display_begin = event.begin + cycle_offset;
    let display_end = event.end + cycle_offset;
    if matches!(event.value, PatternValue::Rest)
        || display_end <= time_start
        || display_begin >= time_end
    {
        return;
    }
    let track_left = row_rect.left() + label_width;
    let drag_preview = view_state
        .pattern_drag
        .as_ref()
        .filter(|drag| {
            drag.event_id == event.id && (drag.cycle_offset - cycle_offset).abs() < 0.000_001
        })
        .and_then(|drag| {
            ui.input(|input| input.pointer.latest_pos()).map(|pos| {
                let delta = (pos.x - drag.start_pointer_x) as f64 / pixels_per_cycle;
                match drag.mode {
                    PatternDragMode::Move => {
                        // Совпадает с `pattern_codegen::patch_move_token`: иначе превью уже не то, что запишется.
                        let duration = (drag.origin_end - drag.origin_begin).max(1.0 / 16.0);
                        let begin = snap_to_grid(display_begin + delta, 1.0 / 16.0)
                            .clamp(cycle_offset, cycle_offset + (1.0 - duration).max(0.0));
                        (begin, begin + duration)
                    }
                    PatternDragMode::ResizeEnd => {
                        let begin = display_begin;
                        let end = snap_to_grid(display_end + delta, 1.0 / 16.0)
                            .clamp(begin + 1.0 / 64.0, cycle_offset + 1.0);
                        (begin, end)
                    }
                }
            })
        });
    let (paint_begin, paint_end) = drag_preview.unwrap_or((display_begin, display_end));
    let left = time_to_x(paint_begin).clamp(0.0, track_width_px);
    let right = time_to_x(paint_end).clamp(0.0, track_width_px);
    let bar_rect = egui::Rect::from_min_size(
        egui::pos2(track_left + left, row_rect.top() + 4.0),
        egui::vec2((right - left).max(3.0), row_rect.height() - 8.0),
    );
    let editable = event.editable.is_editable();
    let selected = view_state
        .selected_pattern_event
        .as_ref()
        .map(|id| id == &event.id)
        .unwrap_or(false);
    let active = display_begin <= current_time && current_time < display_end;
    let fill = if !editable {
        egui::Color32::from_gray(82)
    } else if active {
        egui::Color32::from_rgb(0x38, 0xBD, 0xF8)
    } else {
        egui::Color32::from_rgb(0x25, 0x6D, 0x85)
    };
    let painter = ui.painter_at(row_rect);
    painter.rect_filled(bar_rect, 2.0, fill);

    // Реальная волна сэмпла внутри блока — вводится из timeline_ui,
    // чтобы PatternEditor и «Timeline» были одной инстанцией с волнами и ресайзом.
    if let (Some(lookup), Some(sample_name)) = (waveform, event.sample()) {
        if let Some(wave) = lookup(track_bank, sample_name).filter(|w| !w.is_empty()) {
            let n = wave.len();
            let max_val = wave.iter().copied().fold(0.0f32, f32::max);
            let scale = if max_val > 1e-6 { 1.0 / max_val } else { 1.0 };
            let h = bar_rect.height() * 0.7;
            let mid_y = bar_rect.center().y;
            let wave_color = egui::Color32::from_rgba_unmultiplied(0xE2, 0xE8, 0xF0, 200);
            let mut last_top = egui::pos2(bar_rect.left(), mid_y);
            let mut last_bot = egui::pos2(bar_rect.left(), mid_y);
            for i in 0..n {
                let t = (i as f32 + 0.5) / n as f32;
                let x = bar_rect.left() + bar_rect.width() * t;
                let amp = (wave[i] * scale).min(1.0);
                let top_next = egui::pos2(x, mid_y - amp * (h * 0.5));
                let bot_next = egui::pos2(x, mid_y + amp * (h * 0.5));
                painter.line_segment([last_top, top_next], egui::Stroke::new(1.0, wave_color));
                painter.line_segment([last_bot, bot_next], egui::Stroke::new(1.0, wave_color));
                last_top = top_next;
                last_bot = bot_next;
            }
        }
    }

    if selected {
        painter.rect_stroke(
            bar_rect,
            2.0,
            egui::Stroke::new(2.0, egui::Color32::WHITE),
            egui::StrokeKind::Outside,
        );
    }
    painter.text(
        egui::pos2(bar_rect.left() + 4.0, bar_rect.center().y - 6.0),
        egui::Align2::LEFT_TOP,
        event.sample().unwrap_or("?"),
        egui::FontId::monospace(11.0),
        egui::Color32::from_rgb(0xE2, 0xE8, 0xF0),
    );

    let response = ui.interact(
        bar_rect,
        ui.id().with((
            "pattern_event",
            event.id.as_str(),
            cycle_offset.to_bits(),
            event.begin.to_bits(),
            event.end.to_bits(),
        )),
        egui::Sense::click_and_drag(),
    );
    if response.clicked() {
        view_state.selected_pattern_event = Some(event.id.clone());
    }
    let at_resize_edge = response
        .hover_pos()
        .map(|pos| bar_rect.right() - pos.x <= 8.0)
        .unwrap_or(false);
    if response.hovered() {
        if at_resize_edge {
            view_state.hover_resize_event = Some(event.id.clone());
            ui.output_mut(|output| output.cursor_icon = egui::CursorIcon::ResizeHorizontal);
        } else if view_state.hover_resize_event.as_ref() == Some(&event.id) {
            view_state.hover_resize_event = None;
        }
    }
    if response.drag_started() {
        let is_resize = view_state.hover_resize_event.as_ref() == Some(&event.id);
        let start_x = response
            .interact_pointer_pos()
            .map(|pos| pos.x)
            .unwrap_or(if is_resize {
                bar_rect.right()
            } else {
                bar_rect.left()
            });
        view_state.pattern_drag = Some(PatternDragState {
            event_id: event.id.clone(),
            cycle_offset,
            mode: if is_resize {
                PatternDragMode::ResizeEnd
            } else {
                PatternDragMode::Move
            },
            pending_edit: None,
            start_pointer_x: start_x,
            origin_begin: event.begin,
            origin_end: event.end,
        });
        if ui.input(|input| input.modifiers.alt) && editable {
            actions.push(TimelineAction::ApplyPatternEdit(edit_for_event(
                event,
                PatternEditKind::DuplicateEvent,
            )));
        }
    }
    if response.dragged() && editable {
        if let Some(drag) = view_state.pattern_drag.as_mut().filter(|drag| {
            drag.event_id == event.id && (drag.cycle_offset - cycle_offset).abs() < 0.000_001
        }) {
            ui.output_mut(|output| {
                output.cursor_icon = if drag.mode == PatternDragMode::ResizeEnd {
                    egui::CursorIcon::ResizeHorizontal
                } else {
                    egui::CursorIcon::Grabbing
                };
            });
            let delta = ui
                .input(|input| input.pointer.latest_pos())
                .map(|pos| (pos.x - drag.start_pointer_x) as f64 / pixels_per_cycle)
                .unwrap_or(0.0);
            let edit = match drag.mode {
                PatternDragMode::Move => {
                    let duration = (event.end - event.begin).max(1.0 / 16.0);
                    let new_begin = snap_to_grid(display_begin + delta, 1.0 / 16.0)
                        .clamp(cycle_offset, cycle_offset + (1.0 - duration).max(0.0))
                        - cycle_offset;
                    edit_for_event(event, PatternEditKind::MoveEvent { new_begin })
                }
                PatternDragMode::ResizeEnd => {
                    let new_end_abs = snap_to_grid(display_end + delta, 1.0 / 16.0)
                        .clamp(display_begin + 1.0 / 64.0, drag.cycle_offset + 1.0);
                    edit_for_event(
                        event,
                        PatternEditKind::ResizeEvent {
                            new_end: (new_end_abs - drag.cycle_offset).max(event.begin),
                        },
                    )
                }
            };
            drag.pending_edit = Some(edit);
        }
    }
    if let Some(reason) = event.editable.reason() {
        response.clone().on_hover_text(reason);
    }
    response.context_menu(|ui| {
        if !editable {
            ui.label(
                event
                    .editable
                    .reason()
                    .unwrap_or("cannot patch this pattern yet"),
            );
            return;
        }
        if ui.button("Delete").clicked() {
            actions.push(TimelineAction::ApplyPatternEdit(edit_for_event(
                event,
                PatternEditKind::DeleteEvent,
            )));
            ui.close_menu();
        }
        if ui.button("Duplicate").clicked() {
            actions.push(TimelineAction::ApplyPatternEdit(edit_for_event(
                event,
                PatternEditKind::DuplicateEvent,
            )));
            ui.close_menu();
        }
        if ui.button("Mute").clicked() {
            actions.push(TimelineAction::ApplyPatternEdit(edit_for_event(
                event,
                PatternEditKind::MuteEvent,
            )));
            ui.close_menu();
        }
        ui.menu_button("Split", |ui| {
            for n in [2usize, 3, 4] {
                if ui.button(format!("÷{n}")).clicked() {
                    actions.push(TimelineAction::ApplyPatternEdit(edit_for_event(
                        event,
                        PatternEditKind::SplitIntoN { n },
                    )));
                    ui.close_menu();
                }
            }
        });
        if ui.button("Copy").clicked() {
            if let Some(sample) = event.sample() {
                view_state.clipboard = Some(sample.to_string());
            }
            ui.close_menu();
        }
        if let Some(clip) = &view_state.clipboard.clone() {
            if ui.button(format!("Paste «{clip}»")).clicked() {
                actions.push(TimelineAction::ApplyPatternEdit(edit_for_event(
                    event,
                    PatternEditKind::PasteAfterEvent {
                        sample: clip.clone(),
                    },
                )));
                ui.close_menu();
            }
        }
        ui.separator();
        for sample in ["bd", "hh", "sd", "oh"] {
            if ui.button(format!("Replace with {sample}")).clicked() {
                actions.push(TimelineAction::ApplyPatternEdit(edit_for_event(
                    event,
                    PatternEditKind::ReplaceSample(sample.to_string()),
                )));
                ui.close_menu();
            }
        }
        ui.separator();
        for bank in ["RolandTR909", "RolandTR808"] {
            if ui.button(format!("Set bank {bank}")).clicked() {
                actions.push(TimelineAction::ApplyPatternEdit(PatternEdit {
                    event_id: None,
                    pattern_id: event.pattern_id.clone(),
                    token_span: None,
                    token_value: None,
                    kind: PatternEditKind::ChangeBank(bank.to_string()),
                }));
                ui.close_menu();
            }
        }
        ui.separator();
        let span = event.token_span.unwrap_or(event.source_span);
        let line = EditorTab::byte_offset_to_line(code, span.start);
        let has_bp = debugger
            .map(|d| d.has_source_breakpoint(line))
            .unwrap_or(false);
        let bp_label = if has_bp {
            format!("Retirer le point d'arrêt (ligne {line})")
        } else {
            format!("Point d'arrêt ligne {line}")
        };
        if ui.button(bp_label).clicked() {
            actions.push(TimelineAction::ToggleSourceBreakpoint(line));
            ui.close_menu();
        }
        if ui
            .button(format!("Point d'arrêt @ {:.3}", current_time))
            .clicked()
        {
            actions.push(TimelineAction::AddBreakpoint(current_time));
            ui.close_menu();
        }
    });
}

fn edit_for_event(event: &PatternEvent, kind: PatternEditKind) -> PatternEdit {
    PatternEdit {
        event_id: Some(event.id.clone()),
        pattern_id: event.pattern_id.clone(),
        token_span: event.token_span,
        token_value: event.sample().map(str::to_string),
        kind,
    }
}

fn snap_to_grid(value: f64, grid: f64) -> f64 {
    if grid <= 0.0 {
        value
    } else {
        (value / grid).round() * grid
    }
}

fn ellipsize(s: &str, max_chars: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max_chars - 1).collect::<String>())
    }
}
