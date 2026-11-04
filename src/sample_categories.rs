//! Классификация семплов по категориям (ударные, клавиши, оркестр и т.д.)
//! для группировки на таймлайне и поиска в браузере семплов.
//!
//! Маппинг банк/пак → категория задаётся в `categories.json` в корне семплов
//! (strudel_files) или используется встроенный маппинг по умолчанию.

use std::collections::HashMap;
use std::path::Path;

/// Порядок категорий на таймлайне и в браузере.
const CATEGORY_ORDER: &[&str] = &[
    "drums",
    "percussion",
    "bass",
    "keys",
    "synth",
    "orchestral",
    "vocal",
    "fx",
    "other",
];

/// Человекочитаемые названия категорий.
fn default_category_labels() -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("drums".to_string(), "Batterie".to_string());
    m.insert("percussion".to_string(), "Percussion".to_string());
    m.insert("bass".to_string(), "Basse".to_string());
    m.insert("keys".to_string(), "Raccourcis".to_string());
    m.insert("synth".to_string(), "Synthétiseur".to_string());
    m.insert("orchestral".to_string(), "Orchestre".to_string());
    m.insert("vocal".to_string(), "Voix".to_string());
    m.insert("fx".to_string(), "Effets".to_string());
    m.insert("other".to_string(), "Autre".to_string());
    m
}

/// Встроенный маппинг известных паков/банков → категория (lowercase keys).
fn default_bank_to_category() -> HashMap<String, String> {
    let mut m = HashMap::new();
    // Клавиши
    m.insert("piano".to_string(), "keys".to_string());
    // Оркестр / VST
    m.insert("vcsl".to_string(), "orchestral".to_string());
    // Ударные и драм-машины
    m.insert("crate".to_string(), "drums".to_string());
    m.insert("tidal-drum-machines".to_string(), "drums".to_string());
    m.insert("uzu-drumkit".to_string(), "drums".to_string());
    m.insert("mridangam".to_string(), "drums".to_string());
    m.insert("dirt-samples".to_string(), "drums".to_string());
    // Синт
    m.insert("uzu-wavetables".to_string(), "synth".to_string());
    // Частые имена банков из drum machines (из манифестов)
    m.insert("rolandtr909".to_string(), "drums".to_string());
    m.insert("rolandtr808".to_string(), "drums".to_string());
    m.insert("korgvolca".to_string(), "drums".to_string());
    m.insert("novationcircuit".to_string(), "drums".to_string());
    m.insert("arturia".to_string(), "drums".to_string());
    m.insert("alesis".to_string(), "drums".to_string());
    m.insert("linn".to_string(), "drums".to_string());
    m.insert("oberheim".to_string(), "drums".to_string());
    m.insert("yamaha".to_string(), "drums".to_string());
    m.insert("kawai".to_string(), "drums".to_string());
    m.insert("casio".to_string(), "percussion".to_string());
    m.insert("crow".to_string(), "synth".to_string());
    // Prises voix enregistrées dans l'app (banque export FX voix).
    m.insert("vox".to_string(), "vocal".to_string());
    m.insert("recordings".to_string(), "vocal".to_string());
    m.insert("insect".to_string(), "fx".to_string());
    m.insert("wind".to_string(), "fx".to_string());
    m.insert("jazz".to_string(), "drums".to_string());
    m.insert("metal".to_string(), "percussion".to_string());
    m.insert("east".to_string(), "percussion".to_string());
    m.insert("space".to_string(), "fx".to_string());
    m.insert("numbers".to_string(), "fx".to_string());
    m.insert("num".to_string(), "fx".to_string());
    m
}

/// Результат разрешения: категория по ключу и человекочитаемая подпись.
#[derive(Clone, Debug)]
pub struct CategoryInfo {
    pub key: String,
    pub label: String,
}

/// Хранит маппинг банк → категория и порядок/подписи категорий.
pub struct SampleCategories {
    /// bank (lowercase) → category key
    map: HashMap<String, String>,
    order: Vec<String>,
    labels: HashMap<String, String>,
}

impl Default for SampleCategories {
    fn default() -> Self {
        let labels = default_category_labels();
        let order = CATEGORY_ORDER.iter().map(|s| (*s).to_string()).collect();
        let map = default_bank_to_category();
        Self { map, order, labels }
    }
}

impl SampleCategories {
    pub fn new() -> Self {
        Self::default()
    }

    /// Загрузить переопределения из `categories.json` в корне семплов.
    /// Формат: `{ "piano": "keys", "my-pack": "synth" }`. Ключи приводятся к lowercase.
    pub fn load_from_path(&mut self, root: &Path) {
        let path = root.join("categories.json");
        let data = match std::fs::read_to_string(&path) {
            Ok(d) => d,
            Err(_) => return,
        };
        let obj: HashMap<String, String> = match serde_json::from_str(&data) {
            Ok(o) => o,
            Err(_) => return,
        };
        for (bank, cat) in obj {
            let bank_lc = bank.trim().to_lowercase();
            let cat_lc = cat.trim().to_lowercase();
            if !bank_lc.is_empty() && !cat_lc.is_empty() {
                if !self.order.contains(&cat_lc) {
                    self.order.push(cat_lc.clone());
                }
                self.map.insert(bank_lc, cat_lc);
            }
        }
    }

    /// Категория для банка/пака (по имени). Регистронезависимый поиск.
    pub fn category_for_bank(&self, bank: &str) -> CategoryInfo {
        let key = bank.trim().to_lowercase();
        let cat_key = self
            .map
            .get(&key)
            .cloned()
            .unwrap_or_else(|| "other".to_string());
        let label = self
            .labels
            .get(&cat_key)
            .cloned()
            .unwrap_or_else(|| cat_key.clone());
        CategoryInfo {
            key: cat_key,
            label,
        }
    }

    /// Порядок категорий для отображения.
    pub fn category_order(&self) -> &[String] {
        &self.order
    }

    /// Подпись категории по ключу.
    pub fn category_label(&self, key: &str) -> String {
        self.labels
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.to_string())
    }
}
