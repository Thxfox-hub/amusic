//! Загрузка strudel/doc.json для подсказок и автодополнения (как в @strudel/codemirror).

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct DocParam {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub r#type: Option<DocParamType>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DocParamType {
    #[serde(default)]
    pub names: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DocEntry {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub longname: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub params: Option<Vec<DocParam>>,
    #[serde(default)]
    pub examples: Option<Vec<String>>,
    #[serde(default)]
    pub synonyms: Option<Vec<String>>,
    #[serde(default)]
    pub tags: Option<Vec<DocTag>>,
    #[serde(default)]
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum DocTag {
    /// Простой тег в виде строки, например `"external_io"`.
    Simple(String),
    /// Полная форма тега с полем `originalTitle` (как в @strudel/codemirror).
    Detailed {
        #[serde(rename = "originalTitle")]
        original_title: Option<String>,
        #[serde(default)]
        text: Option<String>,
    },
}

fn has_excluded_tag(entry: &DocEntry) -> bool {
    let tags = match &entry.tags {
        Some(t) => t,
        None => return false,
    };
    for t in tags {
        if let DocTag::Detailed { original_title, .. } = t {
            let title = original_title.as_deref();
            if title == Some("superdirtOnly") || title == Some("noAutocomplete") {
                return true;
            }
        }
    }
    false
}

fn is_valid_doc(entry: &DocEntry) -> bool {
    let label = if entry.longname.trim().is_empty() {
        entry.name.as_str()
    } else {
        entry.longname.as_str()
    };
    !label.is_empty()
        && !label.starts_with('_')
        && entry.kind != "package"
        && !has_excluded_tag(entry)
}

#[derive(Deserialize)]
struct DocJson {
    docs: Vec<DocEntry>,
}

/// Хранилище документации: по имени/синониму возвращаем запись для тултипа и список имён для автодополнения.
pub struct DocStore {
    by_name: HashMap<String, DocEntry>,
    all_labels: Vec<String>,
}

impl DocStore {
    /// Загружает doc.json из `path` (обычно repo/strudel/doc.json).
    pub fn load(path: &Path) -> Result<Self, String> {
        #[cfg(debug_assertions)]
        eprintln!("[DocStore] loading {:?}", path);

        let s = std::fs::read_to_string(path).map_err(|e| {
            #[cfg(debug_assertions)]
            eprintln!("[DocStore] read error: {e}");
            e.to_string()
        })?;

        let doc: DocJson = serde_json::from_str(&s).map_err(|e| {
            #[cfg(debug_assertions)]
            eprintln!("[DocStore] parse error: {e}");
            e.to_string()
        })?;
        let mut by_name = HashMap::new();
        let mut all_labels = Vec::new();
        for entry in doc.docs {
            if !is_valid_doc(&entry) {
                continue;
            }
            // Как в packages/codemirror/autocomplete.mjs: getDocLabel(doc) = doc.name || doc.longname
            let label = if !entry.name.trim().is_empty() {
                entry.name.clone()
            } else {
                entry.longname.clone()
            };
            if !label.is_empty() && !by_name.contains_key(&label) {
                by_name.insert(label.clone(), entry.clone());
                all_labels.push(label.clone());
            }
            if let Some(syn) = &entry.synonyms {
                for s in syn {
                    if !s.is_empty() && !by_name.contains_key(s) {
                        let mut syn_entry = entry.clone();
                        syn_entry.name = s.clone();
                        syn_entry.longname = s.clone();
                        by_name.insert(s.clone(), syn_entry);
                        all_labels.push(s.clone());
                    }
                }
            }
        }
        all_labels.sort();

        #[cfg(debug_assertions)]
        eprintln!(
            "[DocStore] loaded {} entries, {} labels",
            by_name.len(),
            all_labels.len()
        );

        Ok(DocStore {
            by_name,
            all_labels,
        })
    }

    /// Все имена и синонимы для подсветки/автодополнения.
    pub fn all_labels(&self) -> &[String] {
        &self.all_labels
    }

    /// Документация по имени функции (или синониму).
    pub fn get(&self, name: &str) -> Option<&DocEntry> {
        self.by_name.get(name)
    }
}

/// Путь к doc.json (установленный runtime или monorepo).
pub fn default_doc_json_path() -> Option<std::path::PathBuf> {
    crate::paths::doc_json_path()
}
