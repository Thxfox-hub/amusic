//! Профессиональная панель управления воспроизведением.

use eframe::egui;

// ── Состояние транспорта (хранится в app) ─────────────────────────────────────

pub struct TransportState {
    pub bpm: f64,
    pub step_size: f64, // шаг в циклах (0.25 = 1 удар при 4/4)
    pub reverse: bool,
    pub loop_enabled: bool,
    pub loop_start: f64,
    pub loop_end: f64,
    pub session_start: f64,
    pub session_end: f64,
    pub punch_enabled: bool,
    pub punch_in: f64,
    pub punch_out: f64,
    pub record_armed: bool,
    pub rec_cues: bool,
    pub play_cues: bool,
    pub metronome: bool,
    pub solo: bool,
    pub audition: bool,
    pub feedback: bool,
    pub smart_tool: bool,
    pub snap_enabled: bool,
    pub grid_mode: GridMode,
    pub edit_mode: EditMode,
    pub mouse_mode: MouseMode,
    pub rec_mode: RecMode,
    pub selected_channel: ChannelMode,
    pub playback_source: PlaybackSource,
    pub playback_source_locked: bool,
    // UI-буфер для редактирования BPM
    bpm_edit: String,
    bpm_editing: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PlaybackSource {
    Editor,
    Arrangement,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GridMode {
    NoGrid,
    Beat,
    Bar,
    Eighth,
    Sixteenth,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EditMode {
    Slide,
    Lock,
    Ripple,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MouseMode {
    Mouse,
    Draw,
    Range,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RecMode {
    Layered,
    SoundOnSound,
    Replace,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChannelMode {
    Left,
    Stereo,
    Right,
}

impl Default for TransportState {
    fn default() -> Self {
        Self {
            bpm: 120.0,
            step_size: 0.25,
            reverse: false,
            loop_enabled: false,
            loop_start: 0.0,
            loop_end: 4.0,
            session_start: 0.0,
            session_end: 4.0,
            punch_enabled: false,
            punch_in: 0.0,
            punch_out: 4.0,
            record_armed: false,
            rec_cues: false,
            play_cues: true,
            metronome: false,
            solo: false,
            audition: false,
            feedback: false,
            smart_tool: true,
            snap_enabled: true,
            grid_mode: GridMode::NoGrid,
            edit_mode: EditMode::Slide,
            mouse_mode: MouseMode::Mouse,
            rec_mode: RecMode::Layered,
            selected_channel: ChannelMode::Left,
            playback_source: PlaybackSource::Editor,
            playback_source_locked: false,
            bpm_edit: String::new(),
            bpm_editing: false,
        }
    }
}

// ── Действия транспорта ───────────────────────────────────────────────────────

#[derive(Clone, PartialEq)]
pub enum TransportAction {
    Play,
    Pause,
    Stop,
    Rewind,
    StepBack,
    StepForward,
    ToggleReverse,
    ToggleLoop,
    SetLoopStart,
    SetLoopEnd,
    SetBpm(f64),
    SetStepSize(f64),
    ToggleMetronome,
    MidiPanic,
    SetSessionStart,
    SetSessionEnd,
    GoToSessionStart,
    GoToSessionEnd,
}

// ── UI ────────────────────────────────────────────────────────────────────────

pub fn transport_ui(
    ui: &mut egui::Ui,
    current_time: f64,
    playing: bool,
    paused: bool,
    active_tab_label: &str,
    state: &mut TransportState,
) -> Vec<TransportAction> {
    let mut actions = Vec::new();
    let full = ui.available_rect_before_wrap();
    ui.painter()
        .rect_filled(full, 0.0, egui::Color32::from_rgb(42, 42, 42));

    egui::ScrollArea::horizontal()
        .id_salt("ardour_transport_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(4.0, 3.0);
                ui.horizontal(|ui| {
                    utility_block(ui, state, active_tab_label, &mut actions);
                    ui.separator();
                    transport_controls(ui, playing, paused, state, &mut actions);
                    ui.separator();
                    punch_block(ui, current_time, state, &mut actions);
                    ui.separator();
                    clock_block(ui, current_time, state, playing, paused, &mut actions);
                    ui.separator();
                    cue_record_block(ui, state);
                    ui.separator();
                    monitor_block(ui, state);
                });
                ui.horizontal(|ui| {
                    edit_toolbar(ui, state);
                    ui.separator();
                    snap_toolbar(ui, state);
                    ui.separator();
                    navigation_toolbar(ui, current_time, state, &mut actions);
                });
            });
        });

    actions
}

fn utility_block(
    ui: &mut egui::Ui,
    state: &mut TransportState,
    active_tab_label: &str,
    actions: &mut Vec<TransportAction>,
) {
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("!").color(egui::Color32::from_rgb(255, 120, 90)),
                    )
                    .min_size(egui::vec2(24.0, 22.0)),
                )
                .on_hover_text("MIDI Panic : coupe immédiatement toutes les voix actives")
                .clicked()
            {
                actions.push(TransportAction::MidiPanic);
            }
            if toggle_response(ui, "M", state.metronome, "Métronome").clicked() {
                actions.push(TransportAction::ToggleMetronome);
            }
            toggle_button(ui, "~", &mut state.audition, "Audition");
        });
        ui.horizontal(|ui| {
            toggle_button(ui, "VS", &mut state.feedback, "Feedback / visual state");
            let bpm_slider = ui
                .add_sized(
                    egui::vec2(52.0, 18.0),
                    egui::Slider::new(&mut state.bpm, 20.0..=300.0).show_value(false),
                )
                .on_hover_text("Tempo/vitesse rapide");
            if bpm_slider.changed() {
                actions.push(TransportAction::SetBpm(state.bpm));
            }
        });
        ui.horizontal(|ui| {
            ui.label("Source:");
            let lock_label = if state.playback_source_locked {
                "🔒"
            } else {
                "🔓"
            };
            if ui
                .button(lock_label)
                .on_hover_text("Lock source (disable auto switch by active tab)")
                .clicked()
            {
                state.playback_source_locked = !state.playback_source_locked;
            }
            egui::ComboBox::from_id_salt("transport_playback_source")
                .selected_text(match state.playback_source {
                    PlaybackSource::Editor => "Editor",
                    PlaybackSource::Arrangement => "Arrangement",
                })
                .show_ui(ui, |ui| {
                    ui.add_enabled_ui(state.playback_source_locked, |ui| {
                        ui.selectable_value(
                            &mut state.playback_source,
                            PlaybackSource::Editor,
                            "Editor",
                        );
                        ui.selectable_value(
                            &mut state.playback_source,
                            PlaybackSource::Arrangement,
                            "Arrangement",
                        );
                    });
                });
        });
        ui.label(
            egui::RichText::new(format!("Active tab: {active_tab_label}"))
                .small()
                .color(egui::Color32::from_gray(170)),
        );
    });
}

fn transport_controls(
    ui: &mut egui::Ui,
    playing: bool,
    paused: bool,
    state: &TransportState,
    actions: &mut Vec<TransportAction>,
) {
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            if icon_button(ui, "|<", "Début de session").clicked() {
                actions.push(TransportAction::Rewind);
            }
            if icon_button(
                ui,
                "<",
                &format!("Pas arrière ({})", step_label(state.step_size)),
            )
            .clicked()
            {
                actions.push(TransportAction::StepBack);
            }
            let reverse_color = if state.reverse {
                egui::Color32::from_rgb(255, 170, 70)
            } else {
                egui::Color32::from_gray(165)
            };
            if ui
                .add(
                    egui::Button::new(egui::RichText::new("<<").color(reverse_color))
                        .min_size(egui::vec2(28.0, 24.0)),
                )
                .on_hover_text("Lecture arrière")
                .clicked()
            {
                actions.push(TransportAction::ToggleReverse);
            }
            let stopped = !playing && !paused;
            let stop_color = if stopped {
                egui::Color32::from_rgb(105, 230, 130)
            } else {
                egui::Color32::from_gray(210)
            };
            if ui
                .add(
                    egui::Button::new(egui::RichText::new("■").color(stop_color))
                        .min_size(egui::vec2(28.0, 24.0)),
                )
                .on_hover_text("Stop (position conservée ; |< remet à 0)")
                .clicked()
            {
                actions.push(TransportAction::Stop);
            }
            if playing && !paused {
                if icon_button(ui, "||", "Pause").clicked() {
                    actions.push(TransportAction::Pause);
                }
            } else if icon_button(
                ui,
                ">",
                if paused {
                    "Continuer"
                } else {
                    "Lecture"
                },
            )
            .clicked()
            {
                actions.push(TransportAction::Play);
            }
            if icon_button(
                ui,
                ">|",
                &format!("Pas avant ({})", step_label(state.step_size)),
            )
            .clicked()
            {
                actions.push(TransportAction::StepForward);
            }
        });
        ui.horizontal(|ui| {
            status_dot(
                ui,
                "Stop",
                !playing && !paused,
                egui::Color32::from_rgb(90, 230, 120),
            );
            status_dot(
                ui,
                "Play",
                playing && !paused,
                egui::Color32::from_rgb(90, 190, 255),
            );
            status_dot(
                ui,
                "Rec",
                state.record_armed,
                egui::Color32::from_rgb(230, 50, 50),
            );
            status_dot(
                ui,
                "Metro",
                state.metronome,
                egui::Color32::from_rgb(230, 210, 70),
            );
        });
    });
}

fn punch_block(
    ui: &mut egui::Ui,
    current_time: f64,
    state: &mut TransportState,
    actions: &mut Vec<TransportAction>,
) {
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.label("Punch:");
            toggle_button(ui, "In", &mut state.punch_enabled, "Activer punch in/out");
            if ui.small_button("Set In").clicked() {
                state.punch_in = current_time;
                actions.push(TransportAction::SetLoopStart);
            }
            if ui.small_button("Out").clicked() {
                state.punch_out = current_time.max(state.punch_in + state.step_size);
                actions.push(TransportAction::SetLoopEnd);
            }
        });
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("{:.2}", state.punch_in))
                    .monospace()
                    .small(),
            );
            ui.label("...");
            ui.label(
                egui::RichText::new(format!("{:.2}", state.punch_out))
                    .monospace()
                    .small(),
            );
        });
    });
}

fn clock_block(
    ui: &mut egui::Ui,
    current_time: f64,
    state: &mut TransportState,
    playing: bool,
    paused: bool,
    actions: &mut Vec<TransportAction>,
) {
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.label(clock_display(
                bbt_display(current_time),
                egui::Color32::from_rgb(140, 255, 45),
                22.0,
            ));
            ui.label(clock_display(
                smpte_display(current_time, state.bpm),
                egui::Color32::from_rgb(30, 220, 40),
                19.0,
            ));
        });
        ui.horizontal(|ui| {
            bpm_editor(ui, state, actions);
            ui.label("TS: 4/4");
            ui.label("INT/M-Clk");
            let mode = if playing {
                "Playing"
            } else if paused {
                "Paused"
            } else {
                "Stopped"
            };
            ui.label(
                egui::RichText::new(mode)
                    .color(egui::Color32::from_gray(160))
                    .small(),
            );
        });
    });
}

fn cue_record_block(ui: &mut egui::Ui, state: &mut TransportState) {
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            if toggle_response(ui, "Rec", state.record_armed, "Enregistrement / arm").clicked() {
                state.record_armed = !state.record_armed;
            }
            status_led(ui, state.record_armed, egui::Color32::from_rgb(230, 40, 40));
        });
        ui.horizontal(|ui| {
            if ui.small_button("Rec Cues").clicked() {
                state.rec_cues = !state.rec_cues;
            }
            status_led(ui, state.rec_cues, egui::Color32::from_rgb(230, 40, 40));
        });
        ui.horizontal(|ui| {
            if ui.small_button("Play Cues").clicked() {
                state.play_cues = !state.play_cues;
            }
            status_led(ui, state.play_cues, egui::Color32::from_rgb(80, 230, 80));
        });
    });
}

fn monitor_block(ui: &mut egui::Ui, state: &mut TransportState) {
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            toggle_button(ui, "Solo", &mut state.solo, "Solo");
            toggle_button(ui, "Audit", &mut state.audition, "Audition");
        });
        ui.horizontal(|ui| {
            toggle_button(ui, "Feedback", &mut state.feedback, "Feedback");
            ui.label("Mix");
        });
        ui.horizontal(|ui| {
            level_meter(ui, -0.0, "-0.0");
            level_meter(ui, f32::NEG_INFINITY, "-inf");
        });
    });
}

fn edit_toolbar(ui: &mut egui::Ui, state: &mut TransportState) {
    combo(
        ui,
        "edit_mode",
        edit_mode_label(state.edit_mode),
        78.0,
        |ui| {
            selectable_value(ui, &mut state.edit_mode, EditMode::Slide, "Slide");
            selectable_value(ui, &mut state.edit_mode, EditMode::Lock, "Lock");
            selectable_value(ui, &mut state.edit_mode, EditMode::Ripple, "Ripple");
        },
    );
    combo(
        ui,
        "mouse_mode",
        mouse_mode_label(state.mouse_mode),
        88.0,
        |ui| {
            selectable_value(ui, &mut state.mouse_mode, MouseMode::Mouse, "Mouse");
            selectable_value(ui, &mut state.mouse_mode, MouseMode::Draw, "Draw");
            selectable_value(ui, &mut state.mouse_mode, MouseMode::Range, "Range");
        },
    );
    toggle_button(ui, "Smart", &mut state.smart_tool, "Smart tool");
    tool_button(ui, "↔", "Range tool");
    tool_button(ui, "✂", "Split tool");
    tool_button(ui, "↕", "Grab/move");
    tool_button(ui, "+", "Zoom tool");
}

fn snap_toolbar(ui: &mut egui::Ui, state: &mut TransportState) {
    toggle_button(ui, "Snap", &mut state.snap_enabled, "Alignement sur la grille");
    combo(
        ui,
        "grid_mode",
        grid_mode_label(state.grid_mode),
        86.0,
        |ui| {
            selectable_value(ui, &mut state.grid_mode, GridMode::NoGrid, "No Grid");
            selectable_value(ui, &mut state.grid_mode, GridMode::Beat, "Beat");
            selectable_value(ui, &mut state.grid_mode, GridMode::Bar, "Bar");
            selectable_value(ui, &mut state.grid_mode, GridMode::Eighth, "1/8");
            selectable_value(ui, &mut state.grid_mode, GridMode::Sixteenth, "1/16");
        },
    );
    combo(ui, "rec_mode", rec_mode_label(state.rec_mode), 92.0, |ui| {
        selectable_value(ui, &mut state.rec_mode, RecMode::Layered, "Layered");
        selectable_value(
            ui,
            &mut state.rec_mode,
            RecMode::SoundOnSound,
            "Sound on Sound",
        );
        selectable_value(ui, &mut state.rec_mode, RecMode::Replace, "Replace");
    });
}

fn navigation_toolbar(
    ui: &mut egui::Ui,
    current_time: f64,
    state: &mut TransportState,
    actions: &mut Vec<TransportAction>,
) {
    if icon_button(ui, "S<", "Aller à Session Start").clicked() {
        actions.push(TransportAction::GoToSessionStart);
    }
    if icon_button(ui, "<", "Reculer d'un pas").clicked() {
        actions.push(TransportAction::StepBack);
    }
    ui.label(clock_display(
        format!("00:00:{:05.2}", current_time),
        egui::Color32::from_rgb(120, 255, 35),
        13.0,
    ));
    if icon_button(ui, ">", "Avancer d'un pas").clicked() {
        actions.push(TransportAction::StepForward);
    }
    if icon_button(ui, ">E", "Aller à Session End").clicked() {
        actions.push(TransportAction::GoToSessionEnd);
    }
    if ui
        .small_button("Set Start")
        .on_hover_text("Session Start = cycle courant")
        .clicked()
    {
        actions.push(TransportAction::SetSessionStart);
    }
    if ui
        .small_button("Set End")
        .on_hover_text("Session End = cycle courant")
        .clicked()
    {
        actions.push(TransportAction::SetSessionEnd);
    }
    ui.label(
        egui::RichText::new(format!(
            "Session {:.2}..{:.2}",
            state.session_start, state.session_end
        ))
        .monospace()
        .small()
        .color(egui::Color32::from_gray(170)),
    );
    combo(
        ui,
        "channel_mode",
        channel_label(state.selected_channel),
        74.0,
        |ui| {
            selectable_value(ui, &mut state.selected_channel, ChannelMode::Left, "Left");
            selectable_value(
                ui,
                &mut state.selected_channel,
                ChannelMode::Stereo,
                "Stereo",
            );
            selectable_value(ui, &mut state.selected_channel, ChannelMode::Right, "Right");
        },
    );
}

fn bpm_editor(ui: &mut egui::Ui, state: &mut TransportState, actions: &mut Vec<TransportAction>) {
    ui.label(
        egui::RichText::new("♩ =")
            .color(egui::Color32::from_gray(160))
            .small(),
    );
    if state.bpm_editing {
        let resp = ui.add(
            egui::TextEdit::singleline(&mut state.bpm_edit)
                .desired_width(50.0)
                .font(egui::TextStyle::Monospace),
        );
        if resp.lost_focus() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            if let Ok(v) = state.bpm_edit.trim().parse::<f64>() {
                let bpm = v.clamp(1.0, 300.0);
                state.bpm = bpm;
                actions.push(TransportAction::SetBpm(bpm));
            }
            state.bpm_editing = false;
        }
    } else if ui
        .add(
            egui::Button::new(egui::RichText::new(format!("{:.3}", state.bpm)).monospace())
                .frame(false),
        )
        .on_hover_text("Cliquez pour modifier le BPM")
        .clicked()
    {
        state.bpm_edit = format!("{:.3}", state.bpm);
        state.bpm_editing = true;
    }
}

fn icon_button(ui: &mut egui::Ui, icon: &str, hint: &str) -> egui::Response {
    ui.add(egui::Button::new(egui::RichText::new(icon).size(13.0)).min_size(egui::vec2(28.0, 24.0)))
        .on_hover_text(hint)
}

fn tool_button(ui: &mut egui::Ui, label: &str, hint: &str) {
    let _ = ui
        .add(egui::Button::new(label).min_size(egui::vec2(24.0, 22.0)))
        .on_hover_text(hint);
}

fn toggle_button(ui: &mut egui::Ui, label: &str, value: &mut bool, hint: &str) {
    if toggle_response(ui, label, *value, hint).clicked() {
        *value = !*value;
    }
}

fn toggle_response(ui: &mut egui::Ui, label: &str, selected: bool, hint: &str) -> egui::Response {
    let fill = if selected {
        egui::Color32::from_rgb(55, 95, 55)
    } else {
        egui::Color32::from_rgb(48, 48, 48)
    };
    let text = if selected {
        egui::Color32::from_rgb(180, 255, 180)
    } else {
        egui::Color32::from_gray(190)
    };
    ui.add(
        egui::Button::new(egui::RichText::new(label).color(text).size(12.0))
            .fill(fill)
            .min_size(egui::vec2(28.0, 22.0)),
    )
    .on_hover_text(hint)
}

fn status_dot(ui: &mut egui::Ui, label: &str, active: bool, color: egui::Color32) {
    ui.label(
        egui::RichText::new(label)
            .small()
            .color(egui::Color32::from_gray(150)),
    );
    status_led(ui, active, color);
}

fn status_led(ui: &mut egui::Ui, active: bool, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    let fill = if active {
        color
    } else {
        egui::Color32::from_rgb(32, 18, 18)
    };
    ui.painter().circle_filled(rect.center(), 4.0, fill);
    ui.painter().circle_stroke(
        rect.center(),
        4.0,
        egui::Stroke::new(1.0, egui::Color32::BLACK),
    );
}

fn level_meter(ui: &mut egui::Ui, value: f32, label: &str) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(54.0, 10.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, 1.0, egui::Color32::from_rgb(18, 18, 18));
    if value.is_finite() {
        let w = rect.width() * 0.35;
        let filled = egui::Rect::from_min_size(rect.min, egui::vec2(w, rect.height()));
        ui.painter()
            .rect_filled(filled, 1.0, egui::Color32::from_rgb(70, 210, 80));
    }
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::monospace(9.0),
        egui::Color32::from_gray(185),
    );
}

fn clock_display(text: String, color: egui::Color32, size: f32) -> egui::RichText {
    egui::RichText::new(text)
        .monospace()
        .size(size)
        .color(color)
        .background_color(egui::Color32::from_rgb(5, 12, 5))
}

fn combo(
    ui: &mut egui::Ui,
    id: &'static str,
    selected: &'static str,
    width: f32,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(selected)
        .width(width)
        .show_ui(ui, add_contents);
}

fn selectable_value<T: PartialEq + Copy>(ui: &mut egui::Ui, value: &mut T, option: T, label: &str) {
    if ui.selectable_label(*value == option, label).clicked() {
        *value = option;
        ui.close_menu();
    }
}

const STEP_SIZES: &[(f64, &str)] = &[
    (4.0, "4 cycles"),
    (1.0, "1 cycle"),
    (0.5, "1/2"),
    (0.25, "1/4 (beat)"),
    (0.125, "1/8"),
    (0.0625, "1/16"),
];

fn step_label(size: f64) -> &'static str {
    STEP_SIZES
        .iter()
        .find(|(s, _)| (s - size).abs() < 1e-9)
        .map(|(_, l)| *l)
        .unwrap_or("1/4 (beat)")
}

fn bbt_display(current_time: f64) -> String {
    let t = current_time.max(0.0);
    let bar = (t.floor() as u64) + 1;
    let beat_float = (t.fract() * 4.0).clamp(0.0, 3.999);
    let beat = beat_float.floor() as u64 + 1;
    let tick = ((beat_float.fract() * 10_000.0).round() as u64).min(9999);
    format!("{:03}|{:02}|{:04}", bar, beat, tick)
}

fn smpte_display(current_time: f64, bpm: f64) -> String {
    let seconds = current_time.max(0.0) * 60.0 / bpm.max(1.0);
    let hours = (seconds / 3600.0).floor() as u64;
    let minutes = ((seconds % 3600.0) / 60.0).floor() as u64;
    let secs = (seconds % 60.0).floor() as u64;
    let frames = ((seconds.fract() * 30.0).round() as u64).min(29);
    format!("{:02}:{:02}:{:02}:{:02}", hours, minutes, secs, frames)
}

fn grid_mode_label(mode: GridMode) -> &'static str {
    match mode {
        GridMode::NoGrid => "No Grid",
        GridMode::Beat => "Beat",
        GridMode::Bar => "Bar",
        GridMode::Eighth => "1/8",
        GridMode::Sixteenth => "1/16",
    }
}

fn edit_mode_label(mode: EditMode) -> &'static str {
    match mode {
        EditMode::Slide => "Slide",
        EditMode::Lock => "Lock",
        EditMode::Ripple => "Ripple",
    }
}

fn mouse_mode_label(mode: MouseMode) -> &'static str {
    match mode {
        MouseMode::Mouse => "Mouse",
        MouseMode::Draw => "Draw",
        MouseMode::Range => "Range",
    }
}

fn rec_mode_label(mode: RecMode) -> &'static str {
    match mode {
        RecMode::Layered => "Layered",
        RecMode::SoundOnSound => "Sound on Sound",
        RecMode::Replace => "Replace",
    }
}

fn channel_label(mode: ChannelMode) -> &'static str {
    match mode {
        ChannelMode::Left => "Left",
        ChannelMode::Stereo => "Stereo",
        ChannelMode::Right => "Right",
    }
}
