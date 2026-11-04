//! Настройки IDE. Хранятся в ~/.config/struidel/settings.json (или аналог на Windows/macOS).

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

const SETTINGS_FILENAME: &str = "settings.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdeSettings {
    #[serde(default = "default_true")]
    pub use_short_drum_names: bool,
    #[serde(default)]
    pub insert_note_format: NoteFormat,
    #[serde(default)]
    pub note_display_mode: NoteDisplayMode,
    #[serde(default)]
    pub midi_keyboard_enabled: bool,
    #[serde(default)]
    pub is_installed: bool,
    #[serde(default = "default_strudel_base_url")]
    pub strudel_base_url: String,
    #[serde(default)]
    pub strudel_local_path: Option<String>,
    /// Тема редактора: gruvbox, dracula, monokai, nord, vscode_dark, etc.
    #[serde(default = "default_editor_theme")]
    pub editor_theme: String,
    #[serde(default = "default_true")]
    pub editor_line_numbers: bool,
    #[serde(default = "default_editor_font_size")]
    pub editor_font_size: f32,
    #[serde(default)]
    pub editor_bank_case_style: BankCaseStyle,
    #[serde(default = "default_true")]
    pub editor_bank_aliases_enabled: bool,
    #[serde(default)]
    pub editor_semantic_hints_mode: SemanticHintsMode,
    #[serde(default = "default_true")]
    pub diagnostics_chain_rules_enabled: bool,
    #[serde(default = "default_true")]
    pub diagnostics_argument_conflicts_enabled: bool,
    #[serde(default = "default_true")]
    pub diagnostics_literal_rules_enabled: bool,
    #[serde(default)]
    pub audio_enabled: bool,
    #[serde(default = "default_audio_gain")]
    pub audio_gain: f32,
    /// Папка с семплами для нативного аудио-движка.
    #[serde(default = "default_sample_folder")]
    pub sample_folder: String,
    /// Дополнительные папки с семплами, которые будут подмешаны к основной.
    #[serde(default)]
    pub extra_sample_folders: Vec<String>,
    /// Effets voix appliqués aux prises enregistrées (pitch, compresseur, fades).
    #[serde(default)]
    pub voice_fx: VoiceFxSettings,
    /// Ethereum JSON-RPC URL for Chain Seed panel.
    #[serde(default = "default_eth_rpc_url")]
    pub eth_rpc_url: String,
    #[serde(default = "default_ollama_url")]
    pub ollama_url: String,
    #[serde(default = "default_ollama_model")]
    pub ollama_model: String,
    #[serde(skip)]
    pub audio_test_requested: bool,
    #[serde(skip)]
    pub reload_samples_requested: bool,
}

fn default_eth_rpc_url() -> String {
    crate::chain_seed::DEFAULT_RPC_URL.to_string()
}

fn default_ollama_url() -> String {
    crate::chain_seed::DEFAULT_OLLAMA_URL.to_string()
}

fn default_ollama_model() -> String {
    crate::chain_seed::DEFAULT_OLLAMA_MODEL.to_string()
}

fn default_editor_theme() -> String {
    "gruvbox".to_string()
}
fn default_editor_font_size() -> f32 {
    14.0
}

fn default_audio_gain() -> f32 {
    0.3
}

fn default_sample_folder() -> String {
    crate::paths::default_sample_folder()
        .to_string_lossy()
        .into_owned()
}

fn default_true() -> bool {
    true
}

/// Réglages des effets voix appliqués aux prises enregistrées.
/// Le clip garde pitch/fades en paramètres non destructifs ; la copie exportée
/// dans la banque de samples est rendue avec la chaîne complète.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceFxSettings {
    /// Appliquer les effets aux prises enregistrées.
    pub enabled: bool,
    /// Pitch en demi-tons (mode Elastic : durée conservée).
    pub pitch_semitones: f32,
    /// Compresseur sur la piste d'enregistrement.
    pub comp_enabled: bool,
    pub comp_threshold_db: f32,
    pub comp_ratio: f32,
    pub comp_makeup_db: f32,
    /// Passe-haut de nettoyage (Hz).
    pub highpass_hz: f32,
    /// Saturation (waveshaper tanh) après le pitch : 0 = off, 1 = fort.
    #[serde(default)]
    pub saturation: f32,
    /// Crescendo au début (secondes).
    pub fade_in_secs: f32,
    /// Absorption rapide à la fin (secondes).
    pub fade_out_secs: f32,
    /// Banque de samples où copier chaque prise traitée (vide = pas d'export).
    pub export_bank: String,
}

impl Default for VoiceFxSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            pitch_semitones: 0.0,
            comp_enabled: true,
            comp_threshold_db: -22.0,
            comp_ratio: 3.0,
            comp_makeup_db: 4.0,
            highpass_hz: 80.0,
            saturation: 0.0,
            fade_in_secs: 1.5,
            fade_out_secs: 0.4,
            export_bank: "vox".to_string(),
        }
    }
}
fn default_strudel_base_url() -> String {
    "https://strudel.b-cdn.net".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoteFormat {
    #[default]
    Letters,
    Numbers,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoteDisplayMode {
    Letters,
    Numbers,
    #[default]
    Both,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum BankCaseStyle {
    AsIs,
    Lowercase,
    #[default]
    Both,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SemanticHintsMode {
    Off,
    #[default]
    Warn,
    Strict,
}

impl Default for IdeSettings {
    fn default() -> Self {
        Self {
            use_short_drum_names: true,
            insert_note_format: NoteFormat::default(),
            note_display_mode: NoteDisplayMode::default(),
            midi_keyboard_enabled: false,
            is_installed: false,
            strudel_base_url: default_strudel_base_url(),
            strudel_local_path: None,
            editor_theme: default_editor_theme(),
            editor_line_numbers: true,
            editor_font_size: default_editor_font_size(),
            editor_bank_case_style: BankCaseStyle::default(),
            editor_bank_aliases_enabled: true,
            editor_semantic_hints_mode: SemanticHintsMode::default(),
            diagnostics_chain_rules_enabled: true,
            diagnostics_argument_conflicts_enabled: true,
            diagnostics_literal_rules_enabled: true,
            audio_enabled: true,
            audio_gain: default_audio_gain(),
            audio_test_requested: false,
            sample_folder: default_sample_folder(),
            extra_sample_folders: Vec::new(),
            voice_fx: VoiceFxSettings::default(),
            eth_rpc_url: default_eth_rpc_url(),
            ollama_url: default_ollama_url(),
            ollama_model: default_ollama_model(),
            reload_samples_requested: false,
        }
    }
}

fn config_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("cc", "strudel", "struidel")
        .map(|d| d.config_dir().to_path_buf())
}

pub fn load_settings() -> IdeSettings {
    let path = match config_dir() {
        Some(mut d) => {
            let _ = fs::create_dir_all(&d);
            d.push(SETTINGS_FILENAME);
            d
        }
        None => return IdeSettings::default(),
    };
    match fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| IdeSettings::default()),
        Err(_) => IdeSettings::default(),
    }
}

pub fn save_settings(settings: &IdeSettings) {
    if let Some(mut d) = config_dir() {
        let _ = fs::create_dir_all(&d);
        d.push(SETTINGS_FILENAME);
        if let Ok(s) = serde_json::to_string_pretty(settings) {
            let _ = fs::write(d, s);
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct RecentItem {
    pub path: String,
    pub kind: String, // "file" | "folder"
    pub name: String,
}

const RECENT_FILENAME: &str = "recent.json";
const RECENT_MAX: usize = 10;

pub fn load_recent() -> Vec<RecentItem> {
    let path = config_dir().map(|mut d| {
        d.push(RECENT_FILENAME);
        d
    });
    match path.and_then(|p| fs::read_to_string(p).ok()) {
        Some(s) => serde_json::from_str(&s).unwrap_or_default(),
        None => vec![],
    }
}

pub fn save_recent(items: &[RecentItem]) {
    let items: Vec<RecentItem> = items.iter().take(RECENT_MAX).cloned().collect();
    if let Some(mut d) = config_dir() {
        let _ = fs::create_dir_all(&d);
        d.push(RECENT_FILENAME);
        if let Ok(s) = serde_json::to_string_pretty(&items) {
            let _ = fs::write(d, s);
        }
    }
}
