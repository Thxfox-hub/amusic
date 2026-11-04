//! Дерево файлов (std::fs). Открытие .strudel в редакторе.

use eframe::egui;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct FileTree {
    root: Option<PathBuf>,
    expanded: HashSet<PathBuf>,
    loaded_children: std::collections::HashMap<PathBuf, Vec<DirEntry>>,
}

#[derive(Clone)]
pub(crate) struct DirEntry {
    path: PathBuf,
    name: String,
    is_dir: bool,
}

impl FileTree {
    pub fn set_root(&mut self, path: Option<PathBuf>) {
        if self.root != path {
            self.root = path;
            self.expanded.clear();
            self.loaded_children.clear();
        }
    }

    pub fn root(&self) -> Option<&PathBuf> {
        self.root.as_ref()
    }

    fn load_children(&mut self, path: &Path) -> Vec<DirEntry> {
        let mut entries: Vec<DirEntry> = match std::fs::read_dir(path) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| {
                    let path = e.path();
                    let name = e.file_name().to_string_lossy().to_string();
                    let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    DirEntry { path, name, is_dir }
                })
                .collect(),
            Err(_) => return vec![],
        };
        entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });
        self.loaded_children
            .insert(path.to_path_buf(), entries.clone());
        entries
    }

    fn children(&mut self, path: &Path) -> &[DirEntry] {
        if !self.loaded_children.contains_key(path) {
            self.load_children(path);
        }
        self.loaded_children
            .get(path)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Рисует дерево. Возвращает Some(path, contents) если пользователь кликнул по .strudel файлу.
    /// `title` — заголовок секции (например "Fichiers" или "Samples"); если пустой, заголовок не рисуется.
    pub fn ui(&mut self, ui: &mut egui::Ui, title: &str) -> Option<(PathBuf, String)> {
        if !title.is_empty() {
            ui.heading(title);
            ui.separator();
        }
        let root = match &self.root {
            Some(r) => r.clone(),
            None => {
                ui.label("Ouvrez un dossier : File → Open Folder…");
                return None;
            }
        };
        self.ui_dir(ui, &root, 0)
    }

    fn ui_dir(
        &mut self,
        ui: &mut egui::Ui,
        path: &Path,
        level: usize,
    ) -> Option<(PathBuf, String)> {
        let children: Vec<DirEntry> = self.children(path).to_vec();
        for entry in children {
            let indent = level as f32 * 16.0;
            let is_expanded = self.expanded.contains(&entry.path);
            let (icon, expanded_icon) = if entry.is_dir {
                (if is_expanded { "▼" } else { "▶" }, is_expanded)
            } else {
                ("📄", false)
            };
            let label = format!("{}  {}", icon, entry.name);
            let full_w = ui.available_width().max(indent + 100.0);
            let response = ui.add(
                egui::Button::new(egui::RichText::new(label).size(12.0))
                    .fill(egui::Color32::TRANSPARENT)
                    .stroke(egui::Stroke::NONE)
                    .min_size(egui::vec2(full_w, 20.0)),
            );
            if response.clicked() {
                if entry.is_dir {
                    if expanded_icon {
                        self.expanded.remove(&entry.path);
                    } else {
                        self.expanded.insert(entry.path.clone());
                    }
                } else if entry.name.ends_with(".strudel") {
                    if let Ok(contents) = std::fs::read_to_string(&entry.path) {
                        return Some((entry.path.clone(), contents));
                    }
                }
            }
            if entry.is_dir && self.expanded.contains(&entry.path) {
                let path_buf = entry.path.clone();
                if let Some(req) = ui
                    .indent(egui::Id::new(path).with(level), |ui| {
                        self.ui_dir(ui, &path_buf, level + 1)
                    })
                    .inner
                {
                    return Some(req);
                }
            }
        }
        None
    }
}
