//! Настройки: общие и редактор (тема, нумерация строк, размер шрифта).

use crate::settings::{BankCaseStyle, IdeSettings, SemanticHintsMode};
use eframe::egui;
use egui_code_editor::DEFAULT_THEMES;

pub fn settings_ui(ui: &mut egui::Ui, settings: &mut IdeSettings) {
    ui.heading("Paramètres");
    ui.separator();
    ui.horizontal(|ui| {
        ui.label("URL de base Strudel :");
        ui.text_edit_singleline(&mut settings.strudel_base_url);
    });
    ui.checkbox(
        &mut settings.is_installed,
        "Configuration initiale terminée",
    );

    ui.add_space(8.0);
    ui.heading("Éditeur");
    ui.separator();
    ui.horizontal(|ui| {
        ui.label("Thème :");
        let current = settings.editor_theme.clone();
        egui::ComboBox::from_id_salt("editor_theme")
            .selected_text(&current)
            .show_ui(ui, |ui| {
                for t in DEFAULT_THEMES {
                    if ui.selectable_label(t.name == current, t.name).clicked() {
                        settings.editor_theme = t.name.to_string();
                    }
                }
            });
    });
    ui.checkbox(&mut settings.editor_line_numbers, "Numérotation des lignes");
    ui.horizontal(|ui| {
        ui.label("Taille de police :");
        ui.add(
            egui::DragValue::new(&mut settings.editor_font_size)
                .range(8.0..=32.0)
                .speed(0.5),
        );
    });
    ui.horizontal(|ui| {
        ui.label("Suggestions Bank :");
        egui::ComboBox::from_id_salt("bank_case_style")
            .selected_text(match settings.editor_bank_case_style {
                BankCaseStyle::AsIs => "Comme dans les fichiers",
                BankCaseStyle::Lowercase => "Minuscules uniquement",
                BankCaseStyle::Both => "Les deux variantes",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut settings.editor_bank_case_style,
                    BankCaseStyle::AsIs,
                    "Comme dans les fichiers",
                );
                ui.selectable_value(
                    &mut settings.editor_bank_case_style,
                    BankCaseStyle::Lowercase,
                    "Minuscules uniquement",
                );
                ui.selectable_value(
                    &mut settings.editor_bank_case_style,
                    BankCaseStyle::Both,
                    "Les deux variantes",
                );
            });
    });
    ui.checkbox(
        &mut settings.editor_bank_aliases_enabled,
        "Bank : activer les abréviations/alias",
    );
    ui.horizontal(|ui| {
        ui.label("Semantic hints:");
        egui::ComboBox::from_id_salt("semantic_hints_mode")
            .selected_text(match settings.editor_semantic_hints_mode {
                SemanticHintsMode::Off => "Off",
                SemanticHintsMode::Warn => "Warn",
                SemanticHintsMode::Strict => "Strict",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut settings.editor_semantic_hints_mode,
                    SemanticHintsMode::Off,
                    "Off",
                );
                ui.selectable_value(
                    &mut settings.editor_semantic_hints_mode,
                    SemanticHintsMode::Warn,
                    "Warn",
                );
                ui.selectable_value(
                    &mut settings.editor_semantic_hints_mode,
                    SemanticHintsMode::Strict,
                    "Strict",
                );
            });
    });
    ui.checkbox(
        &mut settings.diagnostics_chain_rules_enabled,
        "Diagnostics: chain rules",
    );
    ui.checkbox(
        &mut settings.diagnostics_argument_conflicts_enabled,
        "Diagnostics: argument conflicts",
    );
    ui.checkbox(
        &mut settings.diagnostics_literal_rules_enabled,
        "Diagnostics: literal validity",
    );
    ui.add_space(8.0);
    ui.heading("Audio");
    ui.separator();
    ui.checkbox(&mut settings.audio_enabled, "Activer le son");
    ui.horizontal(|ui| {
        ui.label("Volume :");
        ui.add(egui::Slider::new(&mut settings.audio_gain, 0.0..=1.0).show_value(true));
    });
    ui.horizontal(|ui| {
        ui.label("Dossier des samples :");
        ui.text_edit_singleline(&mut settings.sample_folder);
    });
    if ui.button("Test sound").clicked() {
        settings.audio_test_requested = true;
    }
    if ui.button("Reload samples").clicked() {
        settings.reload_samples_requested = true;
    }
    ui.add_space(8.0);
    ui.heading("Chain Seed (NFT / on-chain)");
    ui.separator();
    ui.horizontal(|ui| {
        ui.label("Ethereum RPC:");
        ui.text_edit_singleline(&mut settings.eth_rpc_url);
    });
    ui.add_space(8.0);
    ui.heading("AI Pattern (Ollama)");
    ui.separator();
    ui.horizontal(|ui| {
        ui.label("Ollama URL:");
        ui.text_edit_singleline(&mut settings.ollama_url);
    });
    ui.horizontal(|ui| {
        ui.label("Ollama model:");
        ui.text_edit_singleline(&mut settings.ollama_model);
    });
}
