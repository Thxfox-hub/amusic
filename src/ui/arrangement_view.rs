//! UI аранжировки: горизонтальная временная сетка с перетаскиваемыми блоками паттернов.

use crate::arrangement::PatternArrangement;
use eframe::egui;

const LABEL_W: f32 = 130.0;
const ROW_H: f32 = 56.0;
const RULER_H: f32 = 24.0;
const PIXELS_PER_CYCLE: f32 = 80.0;
const HANDLE_W: f32 = 8.0;

pub enum ArrangementAction {
    SetEnabled(bool),
    AddRow,
    DeleteRow(u64),
    MuteRow(u64, bool),
    SoloRow(u64, bool),
    RenameRow(u64, String),
    AddBlock {
        row_id: u64,
        start: f64,
        length: f64,
        code: String,
        name: String,
        source_file: Option<String>,
        source_pattern_id: Option<String>,
    },
    MoveBlock {
        row_id: u64,
        block_id: u64,
        new_start: f64,
    },
    ResizeBlock {
        row_id: u64,
        block_id: u64,
        new_length: f64,
    },
    DeleteBlock {
        row_id: u64,
        block_id: u64,
    },
    MuteBlock {
        row_id: u64,
        block_id: u64,
        muted: bool,
    },
    SelectBlock(Option<(u64, u64)>),
    EditBlockCode {
        row_id: u64,
        block_id: u64,
        code: String,
    },
    EditBlockName {
        row_id: u64,
        block_id: u64,
        name: String,
    },
    SetBlockSource {
        row_id: u64,
        block_id: u64,
        source_file: Option<String>,
        source_pattern_id: Option<String>,
    },
    ExportGeneratedScript,
    Seek(f64),
}

#[derive(Default)]
pub struct ArrangementViewState {
    pub zoom: f64,
    pub view_start: f64,
    drag: Option<BlockDrag>,
    /// Редактирование имени: (row_id, block_id, буфер).
    renaming: Option<(u64, u64, String)>,
    /// Редактирование кода: (row_id, block_id, буфер).
    editing_code: Option<(u64, u64, String)>,
    /// Именование нового блока: (row_id, start, length, name_buffer, source_idx).
    new_block_dialog: Option<(u64, f64, f64, String, usize)>,
}

#[derive(Clone, Debug)]
pub struct ArrangementSourceOption {
    pub source_file: Option<String>,
    pub source_pattern_id: Option<String>,
    pub label: String,
}

#[derive(Clone, Copy)]
struct BlockDrag {
    row_id: u64,
    block_id: u64,
    mode: DragMode,
    origin_start: f64,
    origin_length: f64,
    pointer_x_start: f32,
}

#[derive(Clone, Copy, PartialEq)]
enum DragMode {
    Move,
    ResizeRight,
}

pub fn arrangement_ui(
    ui: &mut egui::Ui,
    arr: &PatternArrangement,
    current_time: f64,
    playing: bool,
    active_code: &str,
    source_options: &[ArrangementSourceOption],
    view: &mut ArrangementViewState,
) -> Vec<ArrangementAction> {
    let mut actions = Vec::new();

    if view.zoom <= 0.0 {
        view.zoom = 1.0;
    }

    // Release block drag on mouse up
    if ui.input(|i| !i.pointer.primary_down()) {
        view.drag = None;
    }

    let ppc = PIXELS_PER_CYCLE * view.zoom as f32;
    let avail = ui.available_rect_before_wrap();

    // Scroll to follow playhead when playing
    if playing {
        let playhead_x = (current_time - view.view_start) as f32 * ppc;
        let track_w = avail.width() - LABEL_W;
        if playhead_x > track_w * 0.75 || playhead_x < 0.0 {
            view.view_start = (current_time - (track_w * 0.25 / ppc) as f64).max(0.0);
        }
    }

    ui.vertical(|ui| {
        // ── Header ──────────────────────────────────────────────────────────
        ui.horizontal(|ui| {
            ui.heading("Arrangement");
            ui.separator();
            let mut enabled = arr.enabled;
            if ui.checkbox(&mut enabled, "Arrangement mode").changed() {
                actions.push(ArrangementAction::SetEnabled(enabled));
            }
            ui.separator();
            if ui.button("+ Track").clicked() {
                actions.push(ArrangementAction::AddRow);
            }
            if ui.button("Export .strudel").clicked() {
                actions.push(ArrangementAction::ExportGeneratedScript);
            }
            ui.separator();
            ui.label("zoom");
            let mut z = view.zoom as f32;
            if ui.add(egui::Slider::new(&mut z, 0.2..=4.0).show_value(false)).changed() {
                view.zoom = z as f64;
            }
            if arr.enabled {
                ui.separator();
                ui.label(
                    egui::RichText::new(format!(
                        "active code: {}",
                        if active_code.is_empty() { "(none)" } else { &active_code[..active_code.len().min(40)] }
                    ))
                    .small()
                    .color(egui::Color32::from_rgb(134, 239, 172)),
                );
            }
        });

        ui.add_space(2.0);

        // ── New block dialog ─────────────────────────────────────────────────
        if let Some((row_id, start, length, ref mut buf, ref mut source_idx)) = view.new_block_dialog {
            let mut confirmed = false;
            let mut cancelled = false;
            egui::Window::new("New Pattern Block")
                .collapsible(false)
                .resizable(false)
                .show(ui.ctx(), |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Name:");
                        let resp = ui.add(
                            egui::TextEdit::singleline(buf).hint_text("e.g. Verse").desired_width(200.0),
                        );
                        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            confirmed = true;
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Source:");
                        let selected = source_options
                            .get(*source_idx)
                            .map(|o| o.label.as_str())
                            .unwrap_or("(inline block code)");
                        egui::ComboBox::from_id_salt(("arr_new_block_src", row_id, start.to_bits(), length.to_bits()))
                            .selected_text(selected)
                            .show_ui(ui, |ui| {
                                for (idx, option) in source_options.iter().enumerate() {
                                    ui.selectable_value(source_idx, idx, &option.label);
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Add").clicked() {
                            confirmed = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancelled = true;
                        }
                    });
                });
            if confirmed {
                let name = buf.clone();
                let selected = source_options.get(*source_idx);
                actions.push(ArrangementAction::AddBlock {
                    row_id,
                    start,
                    length,
                    code: String::new(),
                    name,
                    source_file: selected.and_then(|o| o.source_file.clone()),
                    source_pattern_id: selected.and_then(|o| o.source_pattern_id.clone()),
                });
                view.new_block_dialog = None;
            } else if cancelled {
                view.new_block_dialog = None;
            }
        }

        // ── Block code editor ────────────────────────────────────────────────
        if let Some((row_id, block_id, ref mut buf)) = view.editing_code {
            let mut closed = false;
            egui::Window::new("Pattern Block Code")
                .collapsible(false)
                .resizable(true)
                .default_size([480.0, 200.0])
                .show(ui.ctx(), |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(buf)
                                .font(egui::FontId::monospace(13.0))
                                .desired_width(f32::INFINITY)
                                .desired_rows(8),
                        );
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Apply").clicked() {
                            actions.push(ArrangementAction::EditBlockCode {
                                row_id,
                                block_id,
                                code: buf.clone(),
                            });
                            closed = true;
                        }
                        if ui.button("Close").clicked() {
                            closed = true;
                        }
                    });
                });
            if closed {
                view.editing_code = None;
            }
        }

        // ── Timeline area ────────────────────────────────────────────────────
        let avail = ui.available_rect_before_wrap();
        let track_w = (avail.width() - LABEL_W).max(200.0);
        let total_w = LABEL_W + track_w;

        // Ruler
        {
            let (ruler_rect, _) = ui.allocate_exact_size(egui::vec2(total_w, RULER_H), egui::Sense::click());
            let painter = ui.painter_at(ruler_rect);
            painter.rect_filled(ruler_rect, 0.0, egui::Color32::from_gray(22));
            painter.rect_filled(
                egui::Rect::from_min_size(ruler_rect.min, egui::vec2(LABEL_W, RULER_H)),
                0.0,
                egui::Color32::from_gray(32),
            );
            let tl = ruler_rect.min + egui::vec2(LABEL_W, 0.0);
            let t_start = view.view_start;
            let t_end = t_start + track_w as f64 / ppc as f64;
            for beat in (t_start.floor() as i32)..=(t_end.ceil() as i32) {
                let x = tl.x + (beat as f64 - t_start) as f32 * ppc;
                if x >= tl.x && x <= tl.x + track_w {
                    painter.line_segment(
                        [egui::pos2(x, ruler_rect.top()), egui::pos2(x, ruler_rect.bottom())],
                        (1.0, egui::Color32::from_gray(70)),
                    );
                    painter.text(
                        egui::pos2(x + 3.0, ruler_rect.top() + 4.0),
                        egui::Align2::LEFT_TOP,
                        beat.to_string(),
                        egui::FontId::monospace(10.0),
                        egui::Color32::from_gray(180),
                    );
                }
            }
            // Playhead on ruler
            let ph_x = tl.x + (current_time - view.view_start) as f32 * ppc;
            if ph_x >= tl.x && ph_x <= tl.x + track_w {
                painter.line_segment(
                    [egui::pos2(ph_x, ruler_rect.top()), egui::pos2(ph_x, ruler_rect.bottom())],
                    (2.0, egui::Color32::WHITE),
                );
            }
            // Click ruler to seek
            let ruler_resp = ui.interact(ruler_rect, ui.id().with("ruler_seek"), egui::Sense::click());
            if ruler_resp.clicked() {
                if let Some(pos) = ruler_resp.interact_pointer_pos() {
                    let cycle = view.view_start + (pos.x - tl.x) as f64 / ppc as f64;
                    actions.push(ArrangementAction::Seek(cycle.max(0.0)));
                }
            }
        }

        // Rows
        egui::ScrollArea::vertical().id_salt("arr_scroll").show(ui, |ui| {
            for row in &arr.rows {
                let (row_rect, row_resp) = ui.allocate_exact_size(
                    egui::vec2(total_w, ROW_H),
                    egui::Sense::click_and_drag(),
                );

                let painter = ui.painter_at(row_rect);
                painter.rect_filled(row_rect, 0.0, egui::Color32::from_gray(28));

                // Label area
                let label_rect = egui::Rect::from_min_size(row_rect.min, egui::vec2(LABEL_W, ROW_H));
                painter.rect_filled(label_rect, 0.0, egui::Color32::from_gray(38));

                ui.allocate_ui_at_rect(label_rect.shrink(4.0), |ui| {
                    ui.horizontal(|ui| {
                        let mut muted = row.muted;
                        if ui.toggle_value(&mut muted, "M").on_hover_text("Mute").changed() {
                            actions.push(ArrangementAction::MuteRow(row.id, muted));
                        }
                        let mut solo = row.solo;
                        if ui.toggle_value(&mut solo, "S").on_hover_text("Solo").changed() {
                            actions.push(ArrangementAction::SoloRow(row.id, solo));
                        }
                        ui.label(
                            egui::RichText::new(&row.name)
                                .monospace()
                                .small()
                                .color(if row.muted {
                                    egui::Color32::from_gray(100)
                                } else {
                                    egui::Color32::WHITE
                                }),
                        );
                    });
                    if ui.small_button("✕").on_hover_text("Delete track").clicked() {
                        actions.push(ArrangementAction::DeleteRow(row.id));
                    }
                });

                let track_origin_x = row_rect.min.x + LABEL_W;
                let t_start = view.view_start;
                let pointer_pos = ui.input(|i| i.pointer.hover_pos());
                let mut pointer_over_block = false;

                // Draw grid lines
                let t_end = t_start + track_w as f64 / ppc as f64;
                for beat in (t_start.floor() as i32)..=(t_end.ceil() as i32) {
                    let x = track_origin_x + (beat as f64 - t_start) as f32 * ppc;
                    if x >= track_origin_x && x <= track_origin_x + track_w {
                        painter.line_segment(
                            [egui::pos2(x, row_rect.top()), egui::pos2(x, row_rect.bottom())],
                            (1.0, egui::Color32::from_gray(45)),
                        );
                    }
                }


                // Draw blocks
                for block in &row.blocks {
                    let bx = track_origin_x + (block.start - t_start) as f32 * ppc;
                    let bw = (block.length as f32 * ppc).max(4.0);
                    if bx + bw < track_origin_x || bx > track_origin_x + track_w {
                        continue;
                    }
                    let block_rect = egui::Rect::from_min_size(
                        egui::pos2(bx, row_rect.top() + 4.0),
                        egui::vec2(bw, ROW_H - 8.0),
                    );
                    if pointer_pos
                        .map(|pos| block_rect.contains(pos))
                        .unwrap_or(false)
                    {
                        pointer_over_block = true;
                    }
                    let selected = arr.selected == Some((row.id, block.id));
                    let [r, g, b] = block.color;
                    let base = egui::Color32::from_rgb(r, g, b);
                    let fill = if block.muted {
                        egui::Color32::from_gray(60)
                    } else if selected {
                        egui::Color32::from_rgb(
                            (r as u16 + 50).min(255) as u8,
                            (g as u16 + 50).min(255) as u8,
                            (b as u16 + 50).min(255) as u8,
                        )
                    } else {
                        base
                    };
                    painter.rect_filled(block_rect, 3.0, fill);
                    painter.rect_stroke(
                        block_rect,
                        3.0,
                        egui::Stroke::new(
                            if selected { 2.0 } else { 1.0 },
                            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 120),
                        ),
                        egui::StrokeKind::Inside,
                    );
                    painter.text(
                        block_rect.left_top() + egui::vec2(4.0, 3.0),
                        egui::Align2::LEFT_TOP,
                        block.display_name(),
                        egui::FontId::monospace(11.0),
                        egui::Color32::WHITE,
                    );
                    // Resize handle (right edge)
                    let handle_rect = egui::Rect::from_min_size(
                        egui::pos2(block_rect.right() - HANDLE_W, block_rect.top()),
                        egui::vec2(HANDLE_W, block_rect.height()),
                    );

                    let block_resp = ui.interact(block_rect, ui.id().with(("blk", row.id, block.id)), egui::Sense::click_and_drag());
                    let handle_resp = ui.interact(handle_rect, ui.id().with(("blk_h", row.id, block.id)), egui::Sense::drag());

                    // Hover cursor
                    if handle_resp.hovered() {
                        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::ResizeHorizontal);
                    } else if block_resp.hovered() {
                        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::Grab);
                    }

                    if block_resp.clicked() {
                        let was_selected = selected;
                        actions.push(ArrangementAction::SelectBlock(if was_selected { None } else { Some((row.id, block.id)) }));
                    }
                    if block_resp.double_clicked() {
                        // Source-driven blocks are edited in the source file/pattern, not inline.
                    }

                    // Start drag
                    if handle_resp.drag_started() {
                        view.drag = Some(BlockDrag {
                            row_id: row.id,
                            block_id: block.id,
                            mode: DragMode::ResizeRight,
                            origin_start: block.start,
                            origin_length: block.length,
                            pointer_x_start: handle_resp.interact_pointer_pos().map(|p| p.x).unwrap_or(0.0),
                        });
                    } else if block_resp.drag_started() && view.drag.is_none() {
                        view.drag = Some(BlockDrag {
                            row_id: row.id,
                            block_id: block.id,
                            mode: DragMode::Move,
                            origin_start: block.start,
                            origin_length: block.length,
                            pointer_x_start: block_resp.interact_pointer_pos().map(|p| p.x).unwrap_or(0.0),
                        });
                    }

                    // Context menu
                    block_resp.context_menu(|ui| {
                        ui.label("Code is sourced from file/pattern");
                        ui.separator();
                        ui.label("Source file");
                        for option in source_options {
                            let Some(path) = option.source_file.as_deref() else {
                                continue;
                            };
                            let selected_src = block.source_file.as_deref() == Some(path)
                                && block.source_pattern_id == option.source_pattern_id;
                            if ui.selectable_label(selected_src, &option.label).clicked() {
                                actions.push(ArrangementAction::SetBlockSource {
                                    row_id: row.id,
                                    block_id: block.id,
                                    source_file: Some(path.to_string()),
                                    source_pattern_id: option.source_pattern_id.clone(),
                                });
                                ui.close_menu();
                            }
                        }
                        let mute_label = if block.muted { "Unmute" } else { "Mute" };
                        if ui.button(mute_label).clicked() {
                            actions.push(ArrangementAction::MuteBlock { row_id: row.id, block_id: block.id, muted: !block.muted });
                            ui.close_menu();
                        }
                        if ui.button("Delete").clicked() {
                            actions.push(ArrangementAction::DeleteBlock { row_id: row.id, block_id: block.id });
                            ui.close_menu();
                        }
                    });
                }

                // Drag update
                if let Some(drag) = view.drag.filter(|d| d.row_id == row.id) {
                    let cur_x = ui.input(|i| i.pointer.hover_pos().map(|p| p.x).unwrap_or(drag.pointer_x_start));
                    let delta_cycles = (cur_x - drag.pointer_x_start) as f64 / ppc as f64;
                    match drag.mode {
                        DragMode::Move => {
                            let new_start = (drag.origin_start + delta_cycles).max(0.0);
                            actions.push(ArrangementAction::MoveBlock {
                                row_id: drag.row_id,
                                block_id: drag.block_id,
                                new_start,
                            });
                        }
                        DragMode::ResizeRight => {
                            let new_length = (drag.origin_length + delta_cycles).max(0.5);
                            actions.push(ArrangementAction::ResizeBlock {
                                row_id: drag.row_id,
                                block_id: drag.block_id,
                                new_length,
                            });
                        }
                    }
                }

                // Double-click on empty track space → create block
                let track_area = egui::Rect::from_min_size(
                    egui::pos2(track_origin_x, row_rect.top()),
                    egui::vec2(track_w, ROW_H),
                );
                let track_sense = if pointer_over_block {
                    egui::Sense::hover()
                } else {
                    egui::Sense::click()
                };
                let track_click = ui.interact(track_area, ui.id().with(("track_add", row.id)), track_sense);
                if track_click.double_clicked() && !pointer_over_block {
                    if let Some(pos) = track_click.interact_pointer_pos() {
                        let cycle = (t_start + (pos.x - track_origin_x) as f64 / ppc as f64).max(0.0);
                        let snapped = cycle.round();
                        let default_source_idx = source_options
                            .iter()
                            .position(|o| o.source_file.is_some())
                            .unwrap_or(0);
                        view.new_block_dialog =
                            Some((row.id, snapped, 4.0, String::new(), default_source_idx));
                    }
                }

                // Playhead
                let ph_x = track_origin_x + (current_time - t_start) as f32 * ppc;
                if ph_x >= track_origin_x && ph_x <= track_origin_x + track_w {
                    painter.line_segment(
                        [egui::pos2(ph_x, row_rect.top()), egui::pos2(ph_x, row_rect.bottom())],
                        (2.0, egui::Color32::from_rgba_unmultiplied(255, 255, 255, 180)),
                    );
                }

                // Horizontal scroll via left-button drag on empty space only
                if row_resp.dragged_by(egui::PointerButton::Primary) && !pointer_over_block {
                    view.view_start = (view.view_start - row_resp.drag_delta().x as f64 / ppc as f64).max(0.0);
                }
            }
        });

        // Status bar
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!(
                    "playhead {:.2} cycles  |  {} tracks  |  double-click on empty track space to add block  |  double-click block to edit code",
                    current_time,
                    arr.rows.len(),
                ))
                .small()
                .monospace(),
            );
        });
    });

    actions
}
