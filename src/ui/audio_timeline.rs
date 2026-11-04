use eframe::egui;

use crate::audio_recording::RecordingMeters;
use crate::project_audio::{
    snap_cycle, AudioClipId, AudioTrackId, CursorMode, FadeCurve, FadeEdge, InputChannelMode,
    MonitoringMode, PitchMode, PitchParams, ProjectAudioState, RecordMode, StretchAlgorithm,
    StretchMode, StretchParams,
};
use crate::settings::VoiceFxSettings;
use crate::waveform_cache::WaveformPeak;

pub enum AudioTimelineAction {
    ArmTrack(AudioTrackId, bool),
    SetMonitoring(AudioTrackId, MonitoringMode),
    SetInputMode(AudioTrackId, InputChannelMode),
    StartRecording(AudioTrackId),
    StopRecording,
    SelectTrack(AudioTrackId),
    SelectClip(AudioClipId),
    SplitSelected,
    SplitClip(AudioClipId, f64),
    DuplicateSelected,
    DeleteSelected,
    MoveClip(AudioClipId, f64),
    TrimClipStart(AudioClipId, f64),
    TrimClipEnd(AudioClipId, f64),
    SetFade(AudioClipId, FadeEdge, f64, FadeCurve),
    SetStretch(AudioClipId, StretchParams),
    SetPitch(AudioClipId, PitchParams),
    SetVoiceFx(VoiceFxSettings),
    BounceSession,
    ExportPatterns,
    Undo,
    Redo,
    SetCursorMode(CursorMode),
}

#[derive(Default)]
pub struct AudioTimelineViewState {
    pub view_start: f64,
    pub zoom: f64,
    pub row_height: f32,
    pub label_width: f32,
    clip_drag: Option<ClipDragState>,
}

#[derive(Clone, Copy)]
struct ClipDragState {
    clip_id: AudioClipId,
    mode: ClipDragMode,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClipDragMode {
    Move,
    TrimStart,
    TrimEnd,
}

pub fn audio_timeline_ui(
    ui: &mut egui::Ui,
    project: &mut ProjectAudioState,
    current_time: f64,
    playing: bool,
    recording_active: bool,
    finalizing_recording: bool,
    meters: RecordingMeters,
    input_devices: &[String],
    voice_fx: &VoiceFxSettings,
    peak_lookup: impl Fn(u64) -> Option<WaveformPeak>,
    view: &mut AudioTimelineViewState,
) -> Vec<AudioTimelineAction> {
    let mut actions = Vec::new();
    if ui.input(|input| !input.pointer.primary_down()) {
        view.clip_drag = None;
    }
    if view.zoom <= 0.0 {
        view.zoom = 1.0;
    }
    if view.row_height <= 0.0 {
        view.row_height = 72.0;
    }
    if view.label_width <= 0.0 {
        view.label_width = 170.0;
    }

    collect_hotkeys(ui, &mut actions);

    ui.vertical(|ui| {
        header(ui, project, recording_active, finalizing_recording, &mut actions);
        ui.add_space(4.0);
        controls(ui, project, view, input_devices, voice_fx, &mut actions);
        selected_clip_inspector(ui, project, &mut actions);
        ui.add_space(4.0);

        let available = ui.available_rect_before_wrap();
        let label_width = view.label_width.clamp(120.0, 280.0);
        let track_width = (available.width() - label_width).max(160.0);
        let pixels_per_cycle = 120.0 * view.zoom.max(0.2);
        let visible_cycles = track_width as f64 / pixels_per_cycle;
        if playing && !finalizing_recording {
            view.view_start = (current_time - visible_cycles * 0.25).max(0.0);
        }
        let time_start = view.view_start.max(0.0);
        let time_end = time_start + visible_cycles;

        let time_to_x = |t: f64| -> f32 { ((t - time_start) * pixels_per_cycle) as f32 };
        egui::ScrollArea::both()
            .id_salt("audio_timeline_scroll")
            .show(ui, |ui| {
                let total_width = label_width + track_width;
                draw_ruler(
                    ui,
                    total_width,
                    label_width,
                    track_width,
                    time_start,
                    time_end,
                    current_time,
                    &time_to_x,
                );

                for track in &mut project.tracks {
                    let (row_rect, row_resp) = ui.allocate_exact_size(
                        egui::vec2(total_width, view.row_height),
                        egui::Sense::click_and_drag(),
                    );
                    if row_resp.clicked() {
                        actions.push(AudioTimelineAction::SelectTrack(track.id));
                    }
                    if !playing && row_resp.dragged() {
                        view.view_start = (view.view_start
                            - row_resp.drag_delta().x as f64 / pixels_per_cycle)
                            .max(0.0);
                    }
                    let painter = ui.painter_at(row_rect);
                    painter.rect_filled(row_rect, 0.0, egui::Color32::from_gray(25));
                    let label_rect = egui::Rect::from_min_size(
                        row_rect.left_top(),
                        egui::vec2(label_width, row_rect.height()),
                    );
                    painter.rect_filled(label_rect, 0.0, egui::Color32::from_gray(34));
                    draw_track_header(
                        ui,
                        label_rect,
                        track,
                        recording_active,
                        meters,
                        &mut actions,
                    );

                    let track_left = row_rect.left() + label_width;
                    draw_grid(
                        &painter,
                        row_rect,
                        track_left,
                        track_width,
                        time_start,
                        time_end,
                        &time_to_x,
                    );

                    if current_time >= time_start && current_time <= time_end {
                        let x = track_left + time_to_x(current_time);
                        painter.line_segment(
                            [
                                egui::pos2(x, row_rect.top()),
                                egui::pos2(x, row_rect.bottom()),
                            ],
                            (2.0, egui::Color32::from_rgb(248, 250, 252)),
                        );
                    }

                    for clip in &track.clips {
                        if clip.end() <= time_start || clip.timeline_position >= time_end {
                            continue;
                        }
                        let left = track_left + time_to_x(clip.timeline_position).max(0.0);
                        let right = track_left + time_to_x(clip.end()).min(track_width).max(0.0);
                        let clip_rect = egui::Rect::from_min_max(
                            egui::pos2(left, row_rect.top() + 8.0),
                            egui::pos2(right.max(left + 3.0), row_rect.bottom() - 8.0),
                        );
                        let id = ui.id().with(("audio_clip", clip.id));
                        let resp = ui.interact(clip_rect, id, egui::Sense::click_and_drag());
                        if resp.clicked() {
                            if project.cursor_mode == CursorMode::Split {
                                if let Some(pos) = resp.interact_pointer_pos() {
                                    let ratio = ((pos.x - clip_rect.left()) / clip_rect.width())
                                        .clamp(0.0, 1.0)
                                        as f64;
                                    let split_at = snap_cycle(
                                        clip.timeline_position + clip.length * ratio,
                                        &project.snap,
                                    );
                                    actions.push(AudioTimelineAction::SplitClip(clip.id, split_at));
                                }
                            } else {
                                actions.push(AudioTimelineAction::SelectClip(clip.id));
                            }
                        }
                        let selected = project.selected_clip == Some(clip.id);
                        draw_clip(
                            ui,
                            &painter,
                            clip_rect,
                            clip,
                            selected,
                            peak_lookup(clip.source_file_id),
                        );
                        handle_clip_drag(
                            ui,
                            &resp,
                            clip.id,
                            clip.timeline_position,
                            clip.length,
                            clip_rect,
                            pixels_per_cycle,
                            project.snap.grid_cycles,
                            view,
                            &mut actions,
                        );
                    }

                    if let Some(session) = track.record_state.active_session.as_ref() {
                        let end_cycle = session.current_cycle.max(session.started_at_cycle);
                        let left = track_left + time_to_x(session.started_at_cycle);
                        let right = track_left + time_to_x(end_cycle);
                        let rec_rect = egui::Rect::from_min_max(
                            egui::pos2(left, row_rect.top() + 10.0),
                            egui::pos2(right.max(left + 4.0), row_rect.bottom() - 10.0),
                        );
                        let (fill, label) = if finalizing_recording {
                            (
                                egui::Color32::from_rgba_unmultiplied(234, 88, 12, 200),
                                "Saving…",
                            )
                        } else {
                            (
                                egui::Color32::from_rgba_unmultiplied(239, 68, 68, 180),
                                "REC",
                            )
                        };
                        painter.rect_filled(rec_rect, 2.0, fill);
                        painter.text(
                            rec_rect.left_top() + egui::vec2(6.0, 4.0),
                            egui::Align2::LEFT_TOP,
                            label,
                            egui::FontId::monospace(11.0),
                            egui::Color32::WHITE,
                        );
                    }
                }
            });

        let snapped = snap_cycle(current_time, &project.snap);
        ui.label(
            egui::RichText::new(format!(
                "cursor {:.3} cycles | snap {:.3} | mode {:?}",
                current_time, snapped, project.cursor_mode
            ))
            .monospace()
            .small(),
        );
    });

    actions
}

fn selected_clip_inspector(
    ui: &mut egui::Ui,
    project: &ProjectAudioState,
    actions: &mut Vec<AudioTimelineAction>,
) {
    let Some(clip_id) = project.selected_clip else {
        return;
    };
    let Some(clip) = project.clip(clip_id) else {
        return;
    };
    let mut stretch = clip.stretch.clone();
    let mut pitch = clip.pitch.clone();
    let mut fade_in = clip.fade_in;
    let mut fade_out = clip.fade_out;
    let mut changed_stretch = false;
    let mut changed_pitch = false;

    ui.group(|ui| {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Clip Inspector").strong());
            ui.separator();
            ui.label(format!("id {}", clip_id));
            ui.separator();
            ui.label("stretch");
            changed_stretch |= ui
                .add(
                    egui::DragValue::new(&mut stretch.ratio)
                        .speed(0.01)
                        .range(0.25..=4.0),
                )
                .changed();
            egui::ComboBox::from_id_salt(("stretch_mode", clip_id))
                .selected_text(format!("{:?}", stretch.mode))
                .show_ui(ui, |ui| {
                    changed_stretch |= ui
                        .selectable_value(&mut stretch.mode, StretchMode::Musical, "Musical")
                        .changed();
                    changed_stretch |= ui
                        .selectable_value(&mut stretch.mode, StretchMode::Free, "Free")
                        .changed();
                });
            egui::ComboBox::from_id_salt(("stretch_alg", clip_id))
                .selected_text(format!("{:?}", stretch.algorithm))
                .show_ui(ui, |ui| {
                    changed_stretch |= ui
                        .selectable_value(
                            &mut stretch.algorithm,
                            StretchAlgorithm::Percussive,
                            "Percussive",
                        )
                        .changed();
                    changed_stretch |= ui
                        .selectable_value(&mut stretch.algorithm, StretchAlgorithm::Tonal, "Tonal")
                        .changed();
                    changed_stretch |= ui
                        .selectable_value(
                            &mut stretch.algorithm,
                            StretchAlgorithm::Complex,
                            "Complex",
                        )
                        .changed();
                });
            changed_stretch |= ui
                .checkbox(&mut stretch.preserve_formants, "formants")
                .changed();
        });
        ui.horizontal(|ui| {
            ui.label("pitch");
            changed_pitch |= ui
                .add(
                    egui::DragValue::new(&mut pitch.semitones)
                        .speed(0.25)
                        .range(-24.0..=24.0),
                )
                .changed();
            ui.label("semi");
            changed_pitch |= ui
                .add(
                    egui::DragValue::new(&mut pitch.cents)
                        .speed(1.0)
                        .range(-100.0..=100.0),
                )
                .changed();
            ui.label("cents");
            egui::ComboBox::from_id_salt(("pitch_mode", clip_id))
                .selected_text(format!("{:?}", pitch.mode))
                .show_ui(ui, |ui| {
                    changed_pitch |= ui
                        .selectable_value(&mut pitch.mode, PitchMode::Tape, "Tape")
                        .changed();
                    changed_pitch |= ui
                        .selectable_value(&mut pitch.mode, PitchMode::Elastic, "Elastic")
                        .changed();
                });
            changed_pitch |= ui
                .checkbox(&mut pitch.formant_correction, "formant correction")
                .changed();
            ui.separator();
            ui.label("fade in");
            if ui
                .add(
                    egui::DragValue::new(&mut fade_in.length)
                        .speed(0.01)
                        .range(0.0..=clip.length),
                )
                .changed()
            {
                actions.push(AudioTimelineAction::SetFade(
                    clip_id,
                    FadeEdge::In,
                    fade_in.length,
                    fade_in.curve,
                ));
            }
            ui.label("fade out");
            if ui
                .add(
                    egui::DragValue::new(&mut fade_out.length)
                        .speed(0.01)
                        .range(0.0..=clip.length),
                )
                .changed()
            {
                actions.push(AudioTimelineAction::SetFade(
                    clip_id,
                    FadeEdge::Out,
                    fade_out.length,
                    fade_out.curve,
                ));
            }
        });
    });

    if changed_stretch {
        actions.push(AudioTimelineAction::SetStretch(clip_id, stretch));
    }
    if changed_pitch {
        actions.push(AudioTimelineAction::SetPitch(clip_id, pitch));
    }
}

fn collect_hotkeys(ui: &mut egui::Ui, actions: &mut Vec<AudioTimelineAction>) {
    ui.input_mut(|input| {
        if input.consume_key(egui::Modifiers::NONE, egui::Key::S) {
            actions.push(AudioTimelineAction::SplitSelected);
        }
        if input.consume_key(egui::Modifiers::NONE, egui::Key::D) {
            actions.push(AudioTimelineAction::DuplicateSelected);
        }
        if input.consume_key(egui::Modifiers::NONE, egui::Key::Backspace) {
            actions.push(AudioTimelineAction::DeleteSelected);
        }
        if input.consume_key(egui::Modifiers::CTRL, egui::Key::Z) {
            actions.push(AudioTimelineAction::Undo);
        }
        if input.consume_key(egui::Modifiers::CTRL, egui::Key::Y) {
            actions.push(AudioTimelineAction::Redo);
        }
    });
}

fn header(
    ui: &mut egui::Ui,
    project: &mut ProjectAudioState,
    recording_active: bool,
    finalizing_recording: bool,
    actions: &mut Vec<AudioTimelineAction>,
) {
    ui.horizontal(|ui| {
        ui.heading("Audio Timeline");
        ui.separator();
        let rec_label = if finalizing_recording {
            "Saving…"
        } else if recording_active {
            "Stop Rec"
        } else {
            "Record"
        };
        if ui
            .add_enabled(
                !finalizing_recording,
                egui::Button::new(rec_label).fill(if recording_active {
                    egui::Color32::from_rgb(127, 29, 29)
                } else if finalizing_recording {
                    egui::Color32::from_rgb(90, 70, 20)
                } else {
                    egui::Color32::from_rgb(70, 70, 70)
                }),
            )
            .clicked()
        {
            if recording_active {
                actions.push(AudioTimelineAction::StopRecording);
            } else if let Some(track_id) = project.selected_track {
                actions.push(AudioTimelineAction::StartRecording(track_id));
            }
        }
        ui.separator();
        if ui.button("Bounce WAV").clicked() {
            actions.push(AudioTimelineAction::BounceSession);
        }
        if ui
            .button("Export Patterns…")
            .on_hover_text("Render strudel patterns to WAV offline")
            .clicked()
        {
            actions.push(AudioTimelineAction::ExportPatterns);
        }
        ui.separator();
        mode_button(
            ui,
            project.cursor_mode,
            CursorMode::Select,
            "Select",
            actions,
        );
        mode_button(ui, project.cursor_mode, CursorMode::Split, "Split", actions);
        mode_button(
            ui,
            project.cursor_mode,
            CursorMode::TimeStretch,
            "Time-stretch",
            actions,
        );
        ui.separator();
        ui.checkbox(&mut project.snap.enabled, "Snap");
        ui.checkbox(&mut project.snap.ripple_edit, "Ripple");
    });
}

fn mode_button(
    ui: &mut egui::Ui,
    current: CursorMode,
    mode: CursorMode,
    label: &str,
    actions: &mut Vec<AudioTimelineAction>,
) {
    if ui.selectable_label(current == mode, label).clicked() {
        actions.push(AudioTimelineAction::SetCursorMode(mode));
    }
}

/// Presets de la chaîne FX voix : appliquent un jeu de réglages complet.
/// `export_bank` n'est pas touché par le preset (choix de l'utilisateur).
fn voice_fx_presets() -> Vec<(&'static str, VoiceFxSettings)> {
    vec![
        ("nettoyé", VoiceFxSettings::default()),
        (
            "pitch saturé",
            VoiceFxSettings {
                pitch_semitones: -7.0,
                saturation: 0.85,
                comp_threshold_db: -20.0,
                comp_ratio: 4.0,
                comp_makeup_db: 6.0,
                highpass_hz: 60.0,
                fade_in_secs: 1.5,
                fade_out_secs: 0.4,
                ..VoiceFxSettings::default()
            },
        ),
        (
            "aigu saturé",
            VoiceFxSettings {
                pitch_semitones: 7.0,
                saturation: 0.85,
                comp_threshold_db: -20.0,
                comp_ratio: 4.0,
                comp_makeup_db: 6.0,
                highpass_hz: 60.0,
                fade_in_secs: 1.5,
                fade_out_secs: 0.4,
                ..VoiceFxSettings::default()
            },
        ),
    ]
}

fn controls(
    ui: &mut egui::Ui,
    project: &mut ProjectAudioState,
    view: &mut AudioTimelineViewState,
    input_devices: &[String],
    voice_fx: &VoiceFxSettings,
    actions: &mut Vec<AudioTimelineAction>,
) {
    ui.horizontal(|ui| {
        ui.label("zoom");
        ui.add(egui::Slider::new(&mut view.zoom, 0.2..=8.0).show_value(true));
        ui.label("row");
        ui.add(egui::Slider::new(&mut view.row_height, 44.0..=140.0).show_value(true));
        if ui.button("Undo").clicked() {
            actions.push(AudioTimelineAction::Undo);
        }
        if ui.button("Redo").clicked() {
            actions.push(AudioTimelineAction::Redo);
        }
        if let Some(track) = project.selected_track_mut() {
            egui::ComboBox::from_id_salt("audio_input_device")
                .selected_text(
                    track
                        .input
                        .device_name
                        .as_deref()
                        .unwrap_or("default input"),
                )
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(track.input.device_name.is_none(), "default input")
                        .clicked()
                    {
                        track.input.device_name = None;
                    }
                    for device in input_devices {
                        if ui
                            .selectable_label(
                                track.input.device_name.as_ref() == Some(device),
                                device,
                            )
                            .clicked()
                        {
                            track.input.device_name = Some(device.clone());
                        }
                    }
                });
            egui::ComboBox::from_id_salt("record_mode")
                .selected_text(format!("{:?}", track.record_state.mode))
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut track.record_state.mode,
                        RecordMode::Replace,
                        "Replace",
                    );
                    ui.selectable_value(
                        &mut track.record_state.mode,
                        RecordMode::Overdub,
                        "Overdub",
                    );
                    ui.selectable_value(
                        &mut track.record_state.mode,
                        RecordMode::Layered,
                        "Layered",
                    );
                });
            ui.label("count-in bars");
            ui.add(
                egui::DragValue::new(&mut track.record_state.count_in_bars)
                    .speed(0.25)
                    .range(0.0..=2.0),
            );
            ui.label("preroll");
            ui.add(
                egui::DragValue::new(&mut track.record_state.preroll_cycles)
                    .speed(0.25)
                    .range(0.0..=8.0),
            );
        }
    });

    let mut vfx = voice_fx.clone();
    let mut vfx_changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new("🎙 FX voix").strong());
        vfx_changed |= ui
            .checkbox(&mut vfx.enabled, "actifs à l'enregistrement")
            .changed();
        if vfx.enabled {
            egui::ComboBox::from_id_salt("vfx_preset")
                .selected_text("preset…")
                .show_ui(ui, |ui| {
                    for (name, preset) in voice_fx_presets() {
                        if ui.selectable_label(false, name).clicked() {
                            let mut applied = preset;
                            applied.export_bank = vfx.export_bank.clone();
                            vfx = applied;
                            vfx_changed = true;
                        }
                    }
                });
            ui.separator();
            ui.label("pitch");
            vfx_changed |= ui
                .add(
                    egui::DragValue::new(&mut vfx.pitch_semitones)
                        .speed(0.25)
                        .range(-12.0..=12.0),
                )
                .changed();
            ui.label("semi");
            ui.label("saturation");
            vfx_changed |= ui
                .add(
                    egui::DragValue::new(&mut vfx.saturation)
                        .speed(0.01)
                        .range(0.0..=1.0),
                )
                .changed();
            ui.separator();
            vfx_changed |= ui.checkbox(&mut vfx.comp_enabled, "comp").changed();
            if vfx.comp_enabled {
                ui.label("seuil");
                vfx_changed |= ui
                    .add(
                        egui::DragValue::new(&mut vfx.comp_threshold_db)
                            .speed(0.5)
                            .range(-48.0..=0.0),
                    )
                    .changed();
                ui.label("dB");
                ui.label("ratio");
                vfx_changed |= ui
                    .add(
                        egui::DragValue::new(&mut vfx.comp_ratio)
                            .speed(0.1)
                            .range(1.0..=10.0),
                    )
                    .changed();
                ui.label("makeup");
                vfx_changed |= ui
                    .add(
                        egui::DragValue::new(&mut vfx.comp_makeup_db)
                            .speed(0.5)
                            .range(0.0..=18.0),
                    )
                    .changed();
                ui.label("dB");
            }
            ui.separator();
            ui.label("HP");
            vfx_changed |= ui
                .add(
                    egui::DragValue::new(&mut vfx.highpass_hz)
                        .speed(2.0)
                        .range(0.0..=400.0),
                )
                .changed();
            ui.label("Hz");
            ui.label("crescendo");
            vfx_changed |= ui
                .add(
                    egui::DragValue::new(&mut vfx.fade_in_secs)
                        .speed(0.05)
                        .range(0.0..=5.0),
                )
                .changed();
            ui.label("s");
            ui.label("absorption");
            vfx_changed |= ui
                .add(
                    egui::DragValue::new(&mut vfx.fade_out_secs)
                        .speed(0.05)
                        .range(0.0..=3.0),
                )
                .changed();
            ui.label("s");
            ui.separator();
            ui.label("banque export");
            vfx_changed |= ui
                .add(
                    egui::TextEdit::singleline(&mut vfx.export_bank)
                        .hint_text("vide = off")
                        .desired_width(56.0),
                )
                .changed();
        }
    });
    if vfx_changed {
        actions.push(AudioTimelineAction::SetVoiceFx(vfx));
    }
}

fn draw_ruler(
    ui: &mut egui::Ui,
    total_width: f32,
    label_width: f32,
    track_width: f32,
    time_start: f64,
    time_end: f64,
    current_time: f64,
    time_to_x: &impl Fn(f64) -> f32,
) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(total_width, 28.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, egui::Color32::from_gray(22));
    painter.rect_filled(
        egui::Rect::from_min_size(rect.left_top(), egui::vec2(label_width, rect.height())),
        0.0,
        egui::Color32::from_gray(32),
    );
    let track_left = rect.left() + label_width;
    for beat in (time_start.floor() as i32)..=(time_end.ceil() as i32) {
        let t = beat as f64;
        let x = track_left + time_to_x(t);
        if x >= track_left && x <= track_left + track_width {
            painter.line_segment(
                [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                (1.0, egui::Color32::from_gray(80)),
            );
            painter.text(
                egui::pos2(x + 4.0, rect.center().y - 6.0),
                egui::Align2::LEFT_TOP,
                beat.to_string(),
                egui::FontId::monospace(11.0),
                egui::Color32::from_gray(190),
            );
        }
    }
    if current_time >= time_start && current_time <= time_end {
        let x = track_left + time_to_x(current_time);
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            (2.0, egui::Color32::WHITE),
        );
    }
}

fn draw_track_header(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    track: &mut crate::project_audio::AudioTrack,
    recording_active: bool,
    meters: RecordingMeters,
    actions: &mut Vec<AudioTimelineAction>,
) {
    ui.allocate_ui_at_rect(rect.shrink(5.0), |ui| {
        ui.horizontal(|ui| {
            if ui
                .selectable_label(track.record_state.armed, "R")
                .on_hover_text("Arm track")
                .clicked()
            {
                actions.push(AudioTimelineAction::ArmTrack(
                    track.id,
                    !track.record_state.armed,
                ));
            }
            if ui
                .selectable_label(track.monitoring != MonitoringMode::Off, "I")
                .on_hover_text("Input monitoring")
                .clicked()
            {
                let next = match track.monitoring {
                    MonitoringMode::Off => MonitoringMode::On,
                    MonitoringMode::On => MonitoringMode::Auto,
                    MonitoringMode::Auto => MonitoringMode::Off,
                };
                actions.push(AudioTimelineAction::SetMonitoring(track.id, next));
            }
            ui.label(egui::RichText::new(&track.name).monospace());
        });
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt(("input_mode", track.id))
                .selected_text(match track.input.channel_mode {
                    InputChannelMode::Mono => "mono",
                    InputChannelMode::Stereo => "stereo",
                })
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(
                            track.input.channel_mode == InputChannelMode::Mono,
                            "mono",
                        )
                        .clicked()
                    {
                        actions.push(AudioTimelineAction::SetInputMode(
                            track.id,
                            InputChannelMode::Mono,
                        ));
                    }
                    if ui
                        .selectable_label(
                            track.input.channel_mode == InputChannelMode::Stereo,
                            "stereo",
                        )
                        .clicked()
                    {
                        actions.push(AudioTimelineAction::SetInputMode(
                            track.id,
                            InputChannelMode::Stereo,
                        ));
                    }
                });
            let peak = if recording_active {
                meters.peak
            } else {
                track.level_peak
            };
            let clipped = if recording_active {
                meters.clipped
            } else {
                track.clipped
            };
            let meter_rect = ui
                .allocate_exact_size(egui::vec2(46.0, 8.0), egui::Sense::hover())
                .0;
            ui.painter()
                .rect_filled(meter_rect, 1.0, egui::Color32::from_gray(50));
            ui.painter().rect_filled(
                egui::Rect::from_min_size(
                    meter_rect.left_top(),
                    egui::vec2(
                        meter_rect.width() * peak.clamp(0.0, 1.0),
                        meter_rect.height(),
                    ),
                ),
                1.0,
                if clipped {
                    egui::Color32::from_rgb(248, 113, 113)
                } else {
                    egui::Color32::from_rgb(74, 222, 128)
                },
            );
        });
    });
}

fn draw_grid(
    painter: &egui::Painter,
    row_rect: egui::Rect,
    track_left: f32,
    track_width: f32,
    time_start: f64,
    time_end: f64,
    time_to_x: &impl Fn(f64) -> f32,
) {
    for beat in (time_start.floor() as i32)..=(time_end.ceil() as i32) {
        let x = track_left + time_to_x(beat as f64);
        if x >= track_left && x <= track_left + track_width {
            painter.line_segment(
                [
                    egui::pos2(x, row_rect.top()),
                    egui::pos2(x, row_rect.bottom()),
                ],
                (1.0, egui::Color32::from_gray(50)),
            );
        }
    }
}

fn draw_clip(
    ui: &egui::Ui,
    painter: &egui::Painter,
    rect: egui::Rect,
    clip: &crate::project_audio::AudioClip,
    selected: bool,
    peak: Option<WaveformPeak>,
) {
    let color = if selected {
        egui::Color32::from_rgb(217, 119, 6)
    } else {
        egui::Color32::from_rgb(127, 29, 29)
    };
    painter.rect_filled(rect, 4.0, color);
    painter.rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(1.5, egui::Color32::from_rgb(253, 230, 138)),
        egui::StrokeKind::Inside,
    );

    if let Some(peak) = peak {
        let envelope = peak.display_envelope();
        if !envelope.is_empty() {
            let mid_y = rect.center().y;
            let amp_h = rect.height() * 0.36;
            let peak_max = envelope
                .iter()
                .copied()
                .fold(0.0_f32, |acc, amp| acc.max(amp));
            // Below ~-46 dBFS treat as silence — don't amplify ADC/mic noise floor.
            const SILENCE_FLOOR: f32 = 0.005;
            if peak_max < SILENCE_FLOOR {
                painter.line_segment(
                    [egui::pos2(rect.left() + 4.0, mid_y), egui::pos2(rect.right() - 4.0, mid_y)],
                    (
                        1.0,
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 120),
                    ),
                );
            } else {
                let mut last = egui::pos2(rect.left(), mid_y);
                for (idx, amp) in envelope.iter().enumerate() {
                    let x = rect.left() + rect.width() * idx as f32 / envelope.len().max(1) as f32;
                    let y = mid_y - amp.clamp(0.0, 1.0) * amp_h;
                    let pos = egui::pos2(x, y);
                    painter.line_segment([last, pos], (1.0, egui::Color32::WHITE));
                    last = pos;
                }
            }
        }
        for transient in peak.transients.iter().take(48) {
            let ratio = *transient as f32 / peak.frames.max(1) as f32;
            let x = rect.left() + rect.width() * ratio.clamp(0.0, 1.0);
            painter.line_segment(
                [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                (
                    1.0,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 80),
                ),
            );
        }
    }

    let name = clip
        .name
        .as_deref()
        .map(str::to_string)
        .unwrap_or_else(|| format!("clip {}", clip.id));
    painter.text(
        rect.left_top() + egui::vec2(6.0, 4.0),
        egui::Align2::LEFT_TOP,
        name,
        egui::FontId::monospace(11.0),
        egui::Color32::WHITE,
    );
    if !clip.take_lanes.is_empty() {
        painter.text(
            rect.left_bottom() + egui::vec2(6.0, -16.0),
            egui::Align2::LEFT_TOP,
            format!("{} takes", clip.take_lanes.len()),
            egui::FontId::monospace(10.0),
            egui::Color32::from_rgb(254, 226, 226),
        );
        let lane_h = (rect.height() / clip.take_lanes.len().max(1) as f32).clamp(3.0, 8.0);
        for (idx, take) in clip.take_lanes.iter().enumerate() {
            let y = rect.bottom() - lane_h * (idx as f32 + 1.0);
            let lane_rect = egui::Rect::from_min_size(
                egui::pos2(rect.left() + 2.0, y),
                egui::vec2(rect.width() - 4.0, lane_h - 1.0),
            );
            painter.rect_filled(
                lane_rect,
                1.0,
                if Some(take.id) == clip.selected_take {
                    egui::Color32::from_rgb(252, 165, 165)
                } else {
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 50)
                },
            );
        }
    }

    let fade_w = (rect.width() * 0.2)
        .min(clip.fade_in.length as f32 * 80.0)
        .max(4.0);
    painter.line_segment(
        [
            rect.left_bottom(),
            egui::pos2(rect.left() + fade_w, rect.top()),
        ],
        (1.0, egui::Color32::from_rgb(254, 202, 202)),
    );
    painter.line_segment(
        [
            egui::pos2(rect.right() - fade_w, rect.top()),
            rect.right_bottom(),
        ],
        (1.0, egui::Color32::from_rgb(254, 202, 202)),
    );

    let fade_in_handle = egui::Rect::from_min_size(rect.left_top(), egui::vec2(10.0, 10.0));
    let fade_out_handle = egui::Rect::from_min_size(
        rect.right_top() - egui::vec2(10.0, 0.0),
        egui::vec2(10.0, 10.0),
    );
    ui.painter()
        .rect_filled(fade_in_handle, 2.0, egui::Color32::WHITE);
    ui.painter()
        .rect_filled(fade_out_handle, 2.0, egui::Color32::WHITE);
}

fn handle_clip_drag(
    ui: &egui::Ui,
    resp: &egui::Response,
    clip_id: AudioClipId,
    clip_start: f64,
    clip_length: f64,
    clip_rect: egui::Rect,
    pixels_per_cycle: f64,
    grid: f64,
    view: &mut AudioTimelineViewState,
    actions: &mut Vec<AudioTimelineAction>,
) {
    let hover_mode = clip_drag_mode(resp.hover_pos(), clip_rect);
    if resp.hovered() {
        ui.output_mut(|output| {
            output.cursor_icon = match hover_mode {
                ClipDragMode::TrimStart | ClipDragMode::TrimEnd => {
                    egui::CursorIcon::ResizeHorizontal
                }
                ClipDragMode::Move => egui::CursorIcon::Grab,
            };
        });
    }
    if resp.drag_started() {
        view.clip_drag = Some(ClipDragState {
            clip_id,
            mode: hover_mode,
        });
    }
    let active_mode = view
        .clip_drag
        .filter(|state| state.clip_id == clip_id)
        .map(|state| state.mode);
    if active_mode.is_some() {
        ui.output_mut(|output| {
            output.cursor_icon = match active_mode.unwrap() {
                ClipDragMode::TrimStart | ClipDragMode::TrimEnd => {
                    egui::CursorIcon::ResizeHorizontal
                }
                ClipDragMode::Move => egui::CursorIcon::Grabbing,
            };
        });
    }
    if !resp.dragged() {
        return;
    }
    let delta_cycles = ui.input(|i| i.pointer.delta().x as f64) / pixels_per_cycle.max(0.000_001);
    match active_mode.unwrap_or(hover_mode) {
        ClipDragMode::TrimStart => {
            let new_start = clip_start + delta_cycles;
            let snapped = if grid > 0.0 {
                (new_start / grid).round() * grid
            } else {
                new_start
            };
            actions.push(AudioTimelineAction::TrimClipStart(
                clip_id,
                snapped.max(0.0),
            ));
        }
        ClipDragMode::TrimEnd => {
            let new_end = clip_start + clip_length + delta_cycles;
            let snapped = if grid > 0.0 {
                (new_end / grid).round() * grid
            } else {
                new_end
            };
            actions.push(AudioTimelineAction::TrimClipEnd(clip_id, snapped.max(0.0)));
        }
        ClipDragMode::Move => {
            let new_start = clip_start + delta_cycles;
            let snapped_start = if grid > 0.0 {
                (new_start / grid).round() * grid
            } else {
                new_start
            };
            actions.push(AudioTimelineAction::MoveClip(
                clip_id,
                snapped_start.max(0.0),
            ));
        }
    }
}

fn clip_drag_mode(pos: Option<egui::Pos2>, clip_rect: egui::Rect) -> ClipDragMode {
    let Some(pos) = pos else {
        return ClipDragMode::Move;
    };
    let handle_width = 14.0_f32.min((clip_rect.width() * 0.25).max(6.0));
    if (pos.x - clip_rect.left()).abs() <= handle_width {
        ClipDragMode::TrimStart
    } else if (clip_rect.right() - pos.x).abs() <= handle_width {
        ClipDragMode::TrimEnd
    } else {
        ClipDragMode::Move
    }
}
