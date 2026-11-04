//! AI pattern generator (Ollama) — separate from Hydra and from Chain/NFT seed.

use crate::chain_seed::{ChainSeedState, DEFAULT_OLLAMA_MODEL, DEFAULT_OLLAMA_URL};
use egui::{RichText, ScrollArea, Ui};

#[derive(Debug, Clone)]
pub enum AiPatternAction {
    Generate,
    Apply,
    SavePrefs { url: String, model: String },
}

#[derive(Debug, Clone)]
pub struct AiPatternState {
    pub ollama_url: String,
    pub ollama_model: String,
    pub prompt: String,
    pub preview_code: String,
    pub status: String,
    pub busy: bool,
    pub window_open: bool,
}

impl Default for AiPatternState {
    fn default() -> Self {
        Self {
            ollama_url: DEFAULT_OLLAMA_URL.into(),
            ollama_model: DEFAULT_OLLAMA_MODEL.into(),
            prompt: "dark techno with sparse kicks and metal hats, A minor".into(),
            preview_code: String::new(),
            status: "Idle — describe a pattern, Generate via local Ollama".into(),
            busy: false,
            window_open: false,
        }
    }
}

impl AiPatternState {
    pub fn sync_from_chain_prefs(&mut self, chain: &ChainSeedState) {
        if self.ollama_url.is_empty() {
            self.ollama_url = chain.ollama_url.clone();
        }
        if self.ollama_model.is_empty() {
            self.ollama_model = chain.ollama_model.clone();
        }
    }
}

pub fn ai_pattern_ui(ui: &mut Ui, state: &mut AiPatternState) -> Vec<AiPatternAction> {
    let mut actions = Vec::new();

    ui.heading("AI Pattern");
    ui.label(
        RichText::new("Local Ollama → Strudel code. Separate from Hydra and from NFT/Chain Seed.")
            .weak()
            .small(),
    );
    ui.add_space(6.0);

    ui.group(|ui| {
        ui.label(RichText::new("Ollama").strong());
        ui.horizontal(|ui| {
            ui.label("URL");
            ui.text_edit_singleline(&mut state.ollama_url);
        });
        ui.horizontal(|ui| {
            ui.label("Model");
            ui.text_edit_singleline(&mut state.ollama_model);
        });
        if ui.small_button("Save prefs").clicked() {
            actions.push(AiPatternAction::SavePrefs {
                url: state.ollama_url.clone(),
                model: state.ollama_model.clone(),
            });
        }
    });

    ui.add_space(6.0);
    ui.label(RichText::new("Prompt").strong());
    ui.add(
        egui::TextEdit::multiline(&mut state.prompt)
            .desired_width(f32::INFINITY)
            .desired_rows(3),
    );

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(!state.busy, egui::Button::new(if state.busy {
                "Generating…"
            } else {
                "Generate"
            }))
            .clicked()
        {
            actions.push(AiPatternAction::Generate);
        }
        if ui
            .add_enabled(
                !state.preview_code.trim().is_empty(),
                egui::Button::new("Apply to editor"),
            )
            .clicked()
        {
            actions.push(AiPatternAction::Apply);
        }
    });

    if state.busy {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new(&state.status).small());
        });
    } else {
        ui.label(RichText::new(&state.status).small());
    }

    ui.add_space(8.0);
    ui.label(RichText::new("Generated Strudel").strong());
    ScrollArea::vertical()
        .id_salt("ai_pattern_preview")
        .max_height(240.0)
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut state.preview_code)
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .desired_rows(10),
            );
        });

    actions
}
