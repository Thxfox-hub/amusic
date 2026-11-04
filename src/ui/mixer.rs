use eframe::egui;

use crate::project_audio::{
    ExportBitDepth, FxSlot, MasterOptions, MixerChannelId, MixerChannelKind, OutputTarget, PanLaw,
    ProjectAudioState, SendSlot,
};

const STRIP_WIDTH: f32 = 176.0;
const STRIP_HEIGHT: f32 = 620.0;
const CONTROLS_HEIGHT: f32 = 360.0;
const FADER_HEIGHT: f32 = 150.0;
const FADER_WIDTH: f32 = 48.0;

pub fn mixer_ui(ui: &mut egui::Ui, project: &mut ProjectAudioState) {
    project.ensure_mixer_template();

    ui.horizontal(|ui| {
        if ui.button("+ Group Bus").clicked() {
            let next = project.group_buses.len() + 1;
            project.add_group_bus(format!("Group {}", next));
        }
        if ui.button("+ Return").clicked() {
            let next = project.return_buses.len() + 1;
            project.add_return_bus(format!("Return {}", next));
        }
        if let Err(err) = project.validate_routing_graph() {
            ui.colored_label(
                egui::Color32::from_rgb(248, 113, 113),
                format!("Routing: {err:?}"),
            );
        }
    });
    ui.label(
        egui::RichText::new(
            "Signal flow: Patterns (live code) → sends → Reverb return → Master · Audio tracks = timeline clips",
        )
        .small()
        .weak(),
    );
    ui.separator();

    let bus_targets = collect_bus_targets();
    let return_targets = collect_return_targets(project);
    let all_targets = collect_output_targets(project);

    egui::ScrollArea::both()
        .id_salt("mixer_channel_strips")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                let group_ids: Vec<_> = project.group_buses.iter().map(|bus| bus.id).collect();
                for channel_id in group_ids {
                    let effective = project.effective_audible(channel_id);
                    if let Some(channel) = project.mixer_channel_mut(channel_id) {
                        channel_strip(
                            ui,
                            channel,
                            effective.muted_by_solo,
                            &bus_targets,
                            &return_targets,
                        );
                    }
                }
                if !project.group_buses.is_empty() && !project.tracks.is_empty() {
                    group_separator(ui);
                }

                let track_ids: Vec<_> = project
                    .tracks
                    .iter()
                    .map(|track| track.channel.id)
                    .collect();
                for channel_id in track_ids {
                    let effective = project.effective_audible(channel_id);
                    if let Some(channel) = project.mixer_channel_mut(channel_id) {
                        channel_strip(
                            ui,
                            channel,
                            effective.muted_by_solo,
                            &all_targets,
                            &return_targets,
                        );
                    }
                }
                if !project.tracks.is_empty()
                    && (!project.group_buses.is_empty() || !project.return_buses.is_empty())
                {
                    group_separator(ui);
                }

                let return_ids: Vec<_> = project.return_buses.iter().map(|bus| bus.id).collect();
                for channel_id in return_ids {
                    let effective = project.effective_audible(channel_id);
                    if let Some(channel) = project.mixer_channel_mut(channel_id) {
                        channel_strip(ui, channel, effective.muted_by_solo, &bus_targets, &[]);
                    }
                }
                if !project.return_buses.is_empty() {
                    group_separator(ui);
                }

                let effective = project.effective_audible(project.master.id);
                channel_strip(ui, &mut project.master, effective.muted_by_solo, &[], &[]);
                master_options_ui(ui, &mut project.master_options);
            });
        });
}

fn channel_strip(
    ui: &mut egui::Ui,
    channel: &mut crate::project_audio::MixerChannel,
    muted_by_solo: bool,
    output_targets: &[(String, OutputTarget)],
    send_targets: &[(String, MixerChannelId)],
) {
    ui.allocate_ui_with_layout(
        egui::vec2(STRIP_WIDTH, STRIP_HEIGHT),
        egui::Layout::top_down(egui::Align::Center),
        |ui| {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(STRIP_WIDTH - 10.0);
                ui.set_min_height(STRIP_HEIGHT - 8.0);

                channel_header(ui, channel, muted_by_solo);
                ui.separator();

                egui::ScrollArea::vertical()
                    .id_salt(("strip_controls", channel.id))
                    .max_height(CONTROLS_HEIGHT)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(STRIP_WIDTH - 28.0);
                        channel_controls(ui, channel, output_targets, send_targets);
                    });

                ui.separator();
                fader_section(ui, channel);
            });
        },
    );
}

fn channel_header(
    ui: &mut egui::Ui,
    channel: &mut crate::project_audio::MixerChannel,
    muted_by_solo: bool,
) {
    ui.vertical_centered(|ui| {
        ui.label(kind_label(channel.kind));
        ui.add(
            egui::TextEdit::singleline(&mut channel.name)
                .desired_width(STRIP_WIDTH - 32.0)
                .clip_text(false),
        );
        if channel.kind == MixerChannelKind::Group && channel.name == "Patterns" {
            ui.colored_label(egui::Color32::from_rgb(96, 165, 250), "live Strudel code");
        } else if channel.kind == MixerChannelKind::Audio {
            ui.colored_label(egui::Color32::from_rgb(148, 163, 184), "timeline clips");
        } else if channel.kind == MixerChannelKind::Return {
            ui.colored_label(egui::Color32::from_rgb(167, 139, 250), "aux / FX bus");
        }
    });
    ui.horizontal_wrapped(|ui| {
        toggle_button(
            ui,
            "M",
            &mut channel.mute,
            egui::Color32::from_rgb(220, 38, 38),
        );
        toggle_button(
            ui,
            "S",
            &mut channel.solo,
            egui::Color32::from_rgb(234, 179, 8),
        );
        toggle_button(
            ui,
            "Safe",
            &mut channel.solo_safe,
            egui::Color32::from_rgb(59, 130, 246),
        );
    });
    if muted_by_solo {
        ui.colored_label(egui::Color32::from_rgb(148, 163, 184), "muted by solo");
    }
}

fn channel_controls(
    ui: &mut egui::Ui,
    channel: &mut crate::project_audio::MixerChannel,
    output_targets: &[(String, OutputTarget)],
    send_targets: &[(String, MixerChannelId)],
) {
    ui.label("Pan");
    let pan_resp = ui.add_sized(
        [STRIP_WIDTH - 36.0, 18.0],
        egui::Slider::new(&mut channel.pan, -1.0..=1.0).show_value(false),
    );
    if pan_resp.double_clicked() {
        channel.pan = 0.0;
    }
    ui.horizontal(|ui| {
        ui.small("L");
        ui.small(format!("{:+.2}", channel.pan));
        ui.small("R");
    });
    egui::ComboBox::from_id_salt(("pan_law", channel.id))
        .width(STRIP_WIDTH - 36.0)
        .selected_text(format!(
            "{:.1} dB pan law",
            channel.pan_law.center_gain_db()
        ))
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut channel.pan_law, PanLaw::Minus3Db, "-3 dB");
            ui.selectable_value(&mut channel.pan_law, PanLaw::Minus45Db, "-4.5 dB");
            ui.selectable_value(&mut channel.pan_law, PanLaw::Minus6Db, "-6 dB");
        });

    ui.separator();
    ui.label("Inserts");
    for (index, insert) in channel.inserts.iter_mut().enumerate() {
        insert_row(ui, channel.id, index, insert);
    }
    ui.horizontal_wrapped(|ui| {
        if ui.small_button("+ Gain").clicked() {
            channel.inserts.push(FxSlot::gain_db(0.0));
        }
        if ui.small_button("+ Saturation").clicked() {
            channel.inserts.push(FxSlot::saturation(6.0));
        }
        if ui.small_button("+ Reverb").clicked() {
            channel.inserts.push(FxSlot::reverb(0.35, 0.65));
        }
        if ui.small_button("+ LowPass").clicked() {
            channel.inserts.push(FxSlot::low_pass(8_000.0));
        }
    });

    if !matches!(channel.kind, MixerChannelKind::Master | MixerChannelKind::Return) {
        ui.separator();
        ui.label("Sends");
        if send_targets.is_empty() {
            ui.colored_label(
                egui::Color32::from_rgb(148, 163, 184),
                "Add + Return above to enable sends",
            );
        } else {
            for (index, send) in channel.sends.iter_mut().enumerate() {
                send_row(ui, channel.id, index, send, send_targets);
            }
            channel.sends.retain(|send| send.enabled);
            ui.horizontal_wrapped(|ui| {
                for (label, target_id) in send_targets {
                    if ui.small_button(format!("+ → {label}")).clicked() {
                        if !channel
                            .sends
                            .iter()
                            .any(|send| send.target_bus_id == *target_id)
                        {
                            channel.sends.push(SendSlot::new(*target_id));
                        }
                    }
                }
            });
        }
    }

    if !matches!(channel.kind, MixerChannelKind::Master) {
        ui.separator();
        ui.label("Output");
        egui::ComboBox::from_id_salt(("output", channel.id))
            .width(STRIP_WIDTH - 36.0)
            .selected_text(output_label(channel.output_target, output_targets))
            .show_ui(ui, |ui| {
                for (label, target) in output_targets {
                    ui.selectable_value(&mut channel.output_target, *target, label);
                }
            });
    }
}

fn fader_section(ui: &mut egui::Ui, channel: &mut crate::project_audio::MixerChannel) {
    ui.vertical_centered(|ui| {
        ui.label(if channel.volume_db <= -89.9 {
            "-inf dB".to_string()
        } else {
            format!("{:+.1} dB", channel.volume_db)
        });
        ui.horizontal(|ui| {
            vertical_fader(ui, channel);
            vertical_meter(
                ui,
                channel.meter.peak,
                channel.meter.rms,
                channel.meter.clipped,
            );
        });
        meter(
            ui,
            channel.meter.peak,
            channel.meter.rms,
            channel.meter.clipped,
        );
    });
}

fn vertical_fader(ui: &mut egui::Ui, channel: &mut crate::project_audio::MixerChannel) {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(FADER_WIDTH, FADER_HEIGHT),
        egui::Sense::click_and_drag(),
    );

    if (response.dragged() || response.clicked()) && response.interact_pointer_pos().is_some() {
        if let Some(pos) = response.interact_pointer_pos() {
            let normalized = 1.0 - ((pos.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
            channel.volume_db = -90.0 + normalized * 102.0;
        }
    }
    if response.double_clicked() {
        channel.volume_db = 0.0;
    }

    let painter = ui.painter();
    let track = egui::Rect::from_center_size(rect.center(), egui::vec2(6.0, rect.height()));
    painter.rect_filled(track, 3.0, egui::Color32::from_gray(45));

    let zero_y = volume_db_to_y(0.0, rect);
    painter.line_segment(
        [
            egui::pos2(rect.center().x - 18.0, zero_y),
            egui::pos2(rect.center().x + 18.0, zero_y),
        ],
        egui::Stroke::new(1.0, egui::Color32::from_gray(95)),
    );

    let knob_y = volume_db_to_y(channel.volume_db, rect);
    let knob =
        egui::Rect::from_center_size(egui::pos2(rect.center().x, knob_y), egui::vec2(36.0, 12.0));
    painter.rect_filled(knob, 3.0, egui::Color32::from_rgb(148, 163, 184));
    painter.rect_stroke(
        knob,
        3.0,
        egui::Stroke::new(1.0, egui::Color32::from_gray(230)),
        egui::StrokeKind::Inside,
    );
}

fn volume_db_to_y(volume_db: f32, rect: egui::Rect) -> f32 {
    let normalized = ((volume_db.clamp(-90.0, 12.0) + 90.0) / 102.0).clamp(0.0, 1.0);
    rect.bottom() - normalized * rect.height()
}

fn vertical_meter(ui: &mut egui::Ui, peak: f32, rms: f32, clipped: bool) {
    let peak = peak.clamp(0.0, 1.0);
    let rms = rms.clamp(0.0, 1.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, FADER_HEIGHT), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, 2.0, egui::Color32::from_gray(28));
    let rms_height = rect.height() * rms;
    ui.painter().rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(rect.left(), rect.bottom() - rms_height),
            rect.right_bottom(),
        ),
        2.0,
        egui::Color32::from_rgb(34, 197, 94),
    );
    let peak_height = rect.height() * peak;
    ui.painter().rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(rect.left(), rect.bottom() - peak_height),
            egui::pos2(rect.right(), rect.bottom() - rms_height),
        ),
        2.0,
        if clipped {
            egui::Color32::from_rgb(239, 68, 68)
        } else {
            egui::Color32::from_rgb(250, 204, 21)
        },
    );
}

fn insert_row(ui: &mut egui::Ui, channel_id: MixerChannelId, index: usize, insert: &mut FxSlot) {
    ui.push_id(("insert", channel_id, index), |ui| {
        ui.horizontal(|ui| {
            ui.checkbox(&mut insert.enabled, "");
            ui.label(insert.name.as_str());
        });
        let name = insert.name.to_ascii_lowercase();
        if name.contains("gain") || name.contains("trim") {
            let gain = insert
                .params
                .entry("gain_db".to_string())
                .or_insert(0.0);
            ui.add_sized(
                [STRIP_WIDTH - 36.0, 18.0],
                egui::Slider::new(gain, -24.0..=12.0).text("gain dB"),
            );
        } else if name.contains("sat") || name.contains("drive") {
            let drive = insert.params.entry("drive".to_string()).or_insert(6.0);
            ui.add_sized(
                [STRIP_WIDTH - 36.0, 18.0],
                egui::Slider::new(drive, 0.0..=24.0).text("drive dB"),
            );
        } else if name.contains("lowpass") || name.contains("low-pass") || name.contains("lpf") {
            let cutoff = insert
                .params
                .entry("cutoff_hz".to_string())
                .or_insert(8_000.0);
            ui.add_sized(
                [STRIP_WIDTH - 36.0, 18.0],
                egui::Slider::new(cutoff, 80.0..=18_000.0)
                    .logarithmic(true)
                    .text("cutoff Hz"),
            );
        } else if name.contains("highpass") || name.contains("high-pass") || name.contains("hpf") {
            let cutoff = insert.params.entry("cutoff_hz".to_string()).or_insert(120.0);
            ui.add_sized(
                [STRIP_WIDTH - 36.0, 18.0],
                egui::Slider::new(cutoff, 20.0..=2_000.0)
                    .logarithmic(true)
                    .text("cutoff Hz"),
            );
        } else if name.contains("reverb") || name.contains("room") {
            let mix = insert.params.entry("mix".to_string()).or_insert(0.35);
            ui.add_sized(
                [STRIP_WIDTH - 36.0, 18.0],
                egui::Slider::new(mix, 0.0..=1.0).text("mix"),
            );
            let decay = insert.params.entry("decay".to_string()).or_insert(0.65);
            ui.add_sized(
                [STRIP_WIDTH - 36.0, 18.0],
                egui::Slider::new(decay, 0.0..=0.92).text("decay"),
            );
        }
    });
    ui.add_space(2.0);
}

fn send_row(
    ui: &mut egui::Ui,
    channel_id: MixerChannelId,
    index: usize,
    send: &mut SendSlot,
    send_targets: &[(String, MixerChannelId)],
) {
    ui.push_id(("send", channel_id, index), |ui| {
        ui.horizontal(|ui| {
            ui.checkbox(&mut send.enabled, "");
            ui.checkbox(&mut send.pre_fader, "pre");
        });
        egui::ComboBox::from_id_salt("target")
            .width(STRIP_WIDTH - 42.0)
            .selected_text(
                send_targets
                    .iter()
                    .find(|(_, id)| *id == send.target_bus_id)
                    .map(|(label, _)| label.as_str())
                    .unwrap_or("Missing"),
            )
            .show_ui(ui, |ui| {
                for (label, id) in send_targets {
                    ui.selectable_value(&mut send.target_bus_id, *id, label);
                }
            });
        ui.add_sized(
            [STRIP_WIDTH - 42.0, 18.0],
            egui::Slider::new(&mut send.level_db, -90.0..=6.0).text("send dB"),
        );
        if ui.small_button("✕").clicked() {
            send.enabled = false;
        }
    });
}

fn toggle_button(ui: &mut egui::Ui, text: &str, value: &mut bool, color: egui::Color32) {
    let mut button = egui::Button::new(text);
    if *value {
        button = button.fill(color);
    }
    if ui.add(button).clicked() {
        *value = !*value;
    }
}

fn meter(ui: &mut egui::Ui, peak: f32, rms: f32, clipped: bool) {
    let peak = peak.clamp(0.0, 1.0);
    let rms = rms.clamp(0.0, 1.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(96.0, 8.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, 2.0, egui::Color32::from_gray(36));
    ui.painter().rect_filled(
        egui::Rect::from_min_size(
            rect.left_top(),
            egui::vec2(rect.width() * rms, rect.height()),
        ),
        2.0,
        egui::Color32::from_rgb(34, 197, 94),
    );
    ui.painter().rect_filled(
        egui::Rect::from_min_size(
            rect.left_top(),
            egui::vec2(rect.width() * peak, rect.height()),
        ),
        2.0,
        if clipped {
            egui::Color32::from_rgb(239, 68, 68)
        } else {
            egui::Color32::from_rgb(250, 204, 21)
        },
    );
}

fn master_options_ui(ui: &mut egui::Ui, options: &mut MasterOptions) {
    ui.allocate_ui_with_layout(
        egui::vec2(STRIP_WIDTH, STRIP_HEIGHT),
        egui::Layout::top_down(egui::Align::Center),
        |ui| {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(STRIP_WIDTH - 10.0);
                ui.label("Master monitor");
                ui.checkbox(&mut options.limiter_enabled, "Limiter");
                ui.checkbox(&mut options.mono_monitor, "Mono");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut options.dim_enabled, "Dim");
                    ui.add(
                        egui::DragValue::new(&mut options.dim_db)
                            .speed(0.5)
                            .suffix(" dB"),
                    );
                });
                ui.separator();
                ui.label("Export");
                ui.checkbox(&mut options.dither_enabled, "Dither");
                egui::ComboBox::from_id_salt("export_bit_depth")
                    .width(STRIP_WIDTH - 28.0)
                    .selected_text(match options.export_bit_depth {
                        ExportBitDepth::Float32 => "32-bit float",
                        ExportBitDepth::Int24 => "24-bit int",
                        ExportBitDepth::Int16 => "16-bit int",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut options.export_bit_depth,
                            ExportBitDepth::Float32,
                            "32-bit float",
                        );
                        ui.selectable_value(
                            &mut options.export_bit_depth,
                            ExportBitDepth::Int24,
                            "24-bit int",
                        );
                        ui.selectable_value(
                            &mut options.export_bit_depth,
                            ExportBitDepth::Int16,
                            "16-bit int",
                        );
                    });
            });
        },
    );
}

fn collect_output_targets(project: &ProjectAudioState) -> Vec<(String, OutputTarget)> {
    let mut targets = vec![("Master".to_string(), OutputTarget::Master)];
    targets.extend(
        project
            .group_buses
            .iter()
            .map(|bus| (bus.name.clone(), OutputTarget::Bus(bus.id))),
    );
    targets
}

fn collect_bus_targets() -> Vec<(String, OutputTarget)> {
    vec![("Master".to_string(), OutputTarget::Master)]
}

fn collect_return_targets(project: &ProjectAudioState) -> Vec<(String, MixerChannelId)> {
    project
        .return_buses
        .iter()
        .map(|bus| (bus.name.clone(), bus.id))
        .collect()
}

fn output_label(target: OutputTarget, options: &[(String, OutputTarget)]) -> String {
    options
        .iter()
        .find(|(_, candidate)| *candidate == target)
        .map(|(label, _)| label.clone())
        .unwrap_or_else(|| "Missing".to_string())
}

fn kind_label(kind: MixerChannelKind) -> &'static str {
    match kind {
        MixerChannelKind::Audio => "Track",
        MixerChannelKind::Group => "Group",
        MixerChannelKind::Return => "Return",
        MixerChannelKind::Master => "Master",
    }
}

fn group_separator(ui: &mut egui::Ui) {
    ui.add_space(8.0);
    ui.separator();
    ui.add_space(8.0);
}
