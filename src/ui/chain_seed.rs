//! Chain Seed panel: on-chain / NFT seed → inject math into YOUR Strudel.
//! Same seed → unusual Hydra sketch. Code remains source of truth.

use crate::chain_seed::ChainSeedState;
use crate::chain_seed::SeedMode;
use egui::{RichText, ScrollArea, Ui};

#[derive(Debug, Clone)]
pub enum ChainSeedAction {
    Fetch,
    Apply,
    LoadDemo,
    SaveRpcUrl(String),
    Recall(usize),
}

pub fn chain_seed_ui(ui: &mut Ui, state: &mut ChainSeedState) -> Vec<ChainSeedAction> {
    let mut actions = Vec::new();

    ui.heading("Chain Seed");
    ui.label(
        RichText::new(
            "Fetch NFT/chain entropy → inject seed math into your Strudel (does not replace your stack). Same seed → unusual Hydra.",
        )
        .weak()
        .small(),
    );
    ui.add_space(8.0);

    ui.group(|ui| {
        ui.label(RichText::new("RPC").strong());
        ui.horizontal(|ui| {
            ui.label("URL");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut state.rpc_url)
                    .desired_width(ui.available_width().max(120.0)),
            );
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                actions.push(ChainSeedAction::SaveRpcUrl(state.rpc_url.clone()));
            }
        });
        if ui
            .small_button("Save RPC URL")
            .on_hover_text("Persist to settings")
            .clicked()
        {
            actions.push(ChainSeedAction::SaveRpcUrl(state.rpc_url.clone()));
        }
    });
    ui.add_space(6.0);

    ui.group(|ui| {
        ui.label(RichText::new("Seed source").strong());
        egui::ComboBox::from_id_salt("chain_seed_mode")
            .selected_text(state.mode.label())
            .show_ui(ui, |ui| {
                for m in SeedMode::all() {
                    ui.selectable_value(&mut state.mode, *m, m.label());
                }
            });
        ui.add_space(4.0);
        match state.mode {
            SeedMode::LatestBlock => {
                ui.label(
                    RichText::new("Uses eth_getBlockByNumber(\"latest\").")
                        .small()
                        .weak(),
                );
            }
            SeedMode::BlockNumber => {
                ui.horizontal(|ui| {
                    ui.label("Block #");
                    ui.text_edit_singleline(&mut state.block_number);
                });
            }
            SeedMode::Address => {
                ui.horizontal(|ui| {
                    ui.label("Address");
                    ui.add(
                        egui::TextEdit::singleline(&mut state.address)
                            .desired_width(ui.available_width()),
                    );
                });
            }
            SeedMode::TxHash => {
                ui.horizontal(|ui| {
                    ui.label("Tx");
                    ui.add(
                        egui::TextEdit::singleline(&mut state.tx_hash)
                            .desired_width(ui.available_width()),
                    );
                });
            }
            SeedMode::Nft => {
                ui.horizontal(|ui| {
                    ui.label("Contract");
                    ui.add(
                        egui::TextEdit::singleline(&mut state.nft_contract)
                            .desired_width(ui.available_width()),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("tokenId");
                    ui.text_edit_singleline(&mut state.nft_token_id);
                });
                ui.label(
                    RichText::new(
                        "NFT → tokenURI → metadata → seed → //@chain-seed consts + Hydra topology",
                    )
                    .small()
                    .weak(),
                );
            }
        }
        ui.add_space(4.0);
        ui.checkbox(
            &mut state.push_hydra,
            "Also push unusual Hydra from this seed",
        );
        ui.add_space(6.0);
        ui.group(|ui| {
            ui.label(RichText::new("Live seed").strong());
            ui.label(
                RichText::new(
                    "Follow = new chain blocks refresh entropy. Morph = while playing, seed crawls each cycle → Hydra pulse.",
                )
                .small()
                .weak(),
            );
            ui.checkbox(&mut state.live_follow, "Follow chain (poll latest block)");
            ui.horizontal(|ui| {
                ui.label("every");
                ui.add(
                    egui::DragValue::new(&mut state.live_interval_secs)
                        .range(4.0..=60.0)
                        .suffix(" s")
                        .speed(0.5),
                );
            });
            ui.checkbox(
                &mut state.live_morph,
                "Morph on play (cycle → new seed phase)",
            );
            ui.checkbox(
                &mut state.live_apply,
                "Live-write //@chain-seed into editor",
            );
            if state.live_follow || state.live_morph {
                ui.label(
                    RichText::new(format!(
                        "tick {} · morph cycle {} · history {}",
                        state.live_tick,
                        state.last_morph_cycle,
                        state.seed_history.len()
                    ))
                    .small()
                    .weak(),
                );
            }
            if !state.seed_history.is_empty() {
                ui.label(RichText::new("Recent seeds").small());
                for (i, (lab, _)) in state.seed_history.iter().rev().take(5).enumerate() {
                    let idx = state.seed_history.len() - 1 - i;
                    if ui.small_button(lab).clicked() {
                        actions.push(ChainSeedAction::Recall(idx));
                    }
                }
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let fetch_label = if state.busy {
                if state.phase.starts_with("2/") {
                    "Mapping…"
                } else {
                    "Fetching…"
                }
            } else {
                "Fetch + inject preview"
            };
            if ui
                .add_enabled(!state.busy, egui::Button::new(fetch_label))
                .on_hover_text("Fetch seed, inject //@chain-seed into a copy of your editor code")
                .clicked()
            {
                actions.push(ChainSeedAction::Fetch);
            }
            if ui.button("Load demo source").clicked() {
                actions.push(ChainSeedAction::LoadDemo);
            }
            if ui
                .add_enabled(
                    !state.preview_code.is_empty() && !state.seed_hex.is_empty(),
                    egui::Button::new("Apply to editor"),
                )
                .on_hover_text("Write injected Strudel into Workspace (source of truth)")
                .clicked()
            {
                actions.push(ChainSeedAction::Apply);
            }
        });
    });

    ui.add_space(6.0);
    if state.busy {
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.strong("Running");
            });
            ui.label(RichText::new(&state.status).small());
        });
    } else {
        ui.label(RichText::new(&state.status).small());
    }
    if !state.seed_hex.is_empty() {
        let short = if state.seed_hex.len() > 20 {
            format!(
                "{}…{}",
                &state.seed_hex[..10],
                &state.seed_hex[state.seed_hex.len() - 6..]
            )
        } else {
            state.seed_hex.clone()
        };
        ui.monospace(format!("seed {short}"));
    }
    if !state.source_label.is_empty() {
        ui.label(RichText::new(&state.source_label).small().weak());
    }
    if !state.meta_note.is_empty() {
        ui.label(RichText::new(&state.meta_note).small().italics());
    }
    if state.last_bpm > 0 || !state.last_generator.is_empty() {
        ui.label(
            RichText::new(format!(
                "mode {} · {} BPM",
                if state.last_generator.is_empty() {
                    "—"
                } else {
                    &state.last_generator
                },
                state.last_bpm
            ))
            .small()
            .weak(),
        );
    }

    ui.add_space(8.0);
    ui.group(|ui| {
        ui.label(RichText::new("Cycle").strong());
        ScrollArea::vertical()
            .id_salt("chain_seed_pipeline")
            .max_height(120.0)
            .show(ui, |ui| {
                for step in &state.pipeline {
                    ui.monospace(step);
                }
            });
    });

    ui.add_space(8.0);
    ui.label(RichText::new("Preview (your code + //@chain-seed math)").strong());
    ScrollArea::vertical()
        .id_salt("chain_seed_preview")
        .max_height(200.0)
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut state.preview_code)
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .desired_rows(10),
            );
        });

    if !state.last_hydra_code.is_empty() {
        ui.add_space(8.0);
        ui.label(RichText::new("Hydra from this seed").strong());
        ScrollArea::vertical()
            .id_salt("chain_seed_hydra_preview")
            .max_height(140.0)
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut state.last_hydra_code)
                        .code_editor()
                        .desired_width(f32::INFINITY)
                        .desired_rows(6),
                );
            });
    }

    actions
}
