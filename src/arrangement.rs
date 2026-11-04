//! Линейная аранжировка strudel-паттернов.
//! Пользователь размещает блоки кода на временной сетке (в циклах).
//! Во время воспроизведения все активные (не приглушённые) блоки объединяются
//! и отправляются в bridge.eval().

/// Один блок паттерна на аранжировке.
#[derive(Clone, Debug)]
pub struct PatternBlock {
    pub id: u64,
    /// Начало блока (в циклах).
    pub start: f64,
    /// Длина блока (в циклах).
    pub length: f64,
    /// Strudel-код этого блока.
    pub code: String,
    /// Источник кода (абсолютный/рабочий путь к `.strudel`), если блок привязан к файлу.
    pub source_file: Option<String>,
    /// Идентификатор конкретного паттерна внутри source_file.
    pub source_pattern_id: Option<String>,
    /// Отображаемое имя (по умолчанию — первые N символов кода).
    pub name: String,
    /// RGB-цвет для визуализации.
    pub color: [u8; 3],
    pub muted: bool,
}

impl PatternBlock {
    pub fn end(&self) -> f64 {
        self.start + self.length
    }

    pub fn contains(&self, cycle: f64) -> bool {
        !self.muted && cycle >= self.start && cycle < self.end()
    }

    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.code
        } else {
            &self.name
        }
    }

    /// Итоговый код блока: либо из привязанного файла, либо локальный code.
    pub fn resolved_code_with(
        &self,
        mut source_lookup: impl FnMut(&str, Option<&str>) -> Option<String>,
    ) -> Option<String> {
        if let Some(path) = self.source_file.as_deref() {
            return source_lookup(path, self.source_pattern_id.as_deref())
                .map(|code| code.trim().to_string())
                .filter(|code| !code.is_empty());
        }
        None
    }
}

/// Один трек в аранжировке (одна горизонтальная полоса).
#[derive(Clone, Debug)]
pub struct ArrangementRow {
    pub id: u64,
    pub name: String,
    pub color: [u8; 3],
    pub muted: bool,
    pub solo: bool,
    pub blocks: Vec<PatternBlock>,
}

impl ArrangementRow {
    pub fn new(id: u64, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            color: [80, 120, 200],
            muted: false,
            solo: false,
            blocks: Vec::new(),
        }
    }

    pub fn active_block_at(&self, cycle: f64) -> Option<&PatternBlock> {
        self.blocks.iter().find(|b| b.contains(cycle))
    }
}

/// Полная линейная аранжировка.
#[derive(Clone, Debug)]
pub struct PatternArrangement {
    pub rows: Vec<ArrangementRow>,
    pub next_id: u64,
    /// Включён ли режим аранжировки (иначе — играет текущий редактор).
    pub enabled: bool,
    /// Конец аранжировки (для экспорта и петли). None — авторасчёт.
    pub end_cycles: Option<f64>,
    /// Выбранный блок (row_id, block_id).
    pub selected: Option<(u64, u64)>,
}

impl Default for PatternArrangement {
    fn default() -> Self {
        let row = ArrangementRow::new(1, "Track 1");
        Self {
            rows: vec![row],
            next_id: 2,
            enabled: false,
            end_cycles: None,
            selected: None,
        }
    }
}

impl PatternArrangement {
    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn add_row(&mut self) {
        let id = self.alloc_id();
        let n = self.rows.len() + 1;
        self.rows
            .push(ArrangementRow::new(id, format!("Track {n}")));
    }

    /// Добавляет блок в указанный трек.
    pub fn add_block(
        &mut self,
        row_id: u64,
        start: f64,
        length: f64,
        code: String,
        name: String,
        source_file: Option<String>,
        source_pattern_id: Option<String>,
    ) {
        let block_id = self.alloc_id();
        if let Some(row) = self.rows.iter_mut().find(|r| r.id == row_id) {
            row.blocks.push(PatternBlock {
                id: block_id,
                start,
                length,
                code,
                source_file,
                source_pattern_id,
                name,
                color: [100, 160, 220],
                muted: false,
            });
            row.blocks
                .sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap());
        }
    }

    pub fn delete_block(&mut self, row_id: u64, block_id: u64) {
        if let Some(row) = self.rows.iter_mut().find(|r| r.id == row_id) {
            row.blocks.retain(|b| b.id != block_id);
        }
        if self.selected == Some((row_id, block_id)) {
            self.selected = None;
        }
    }

    /// Возвращает объединённый код всех активных (не заглушённых) блоков в момент cycle.
    pub fn active_code_at_with(
        &self,
        cycle: f64,
        mut source_lookup: impl FnMut(&str, Option<&str>) -> Option<String>,
    ) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let any_solo = self.rows.iter().any(|r| r.solo);
        let parts: Vec<String> = self
            .rows
            .iter()
            .filter(|r| !r.muted && (!any_solo || r.solo))
            .filter_map(|r| r.active_block_at(cycle))
            .filter_map(|b| b.resolved_code_with(&mut source_lookup))
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n"))
        }
    }

    /// Генерирует итоговый Strudel-скрипт аранжировки (для экспорта/ревью).
    pub fn generate_script(
        &self,
        mut source_lookup: impl FnMut(&str, Option<&str>) -> Option<String>,
    ) -> String {
        let mut lines = Vec::new();
        lines.push("// Auto-generated by Arrangement".to_string());
        lines.push("// Edit source .strudel files instead of this file.".to_string());
        lines.push(String::new());
        let mut rows: Vec<&ArrangementRow> = self.rows.iter().collect();
        rows.sort_by_key(|r| r.id);
        for row in rows {
            lines.push(format!("// Track: {}", row.name));
            let mut blocks: Vec<&PatternBlock> = row.blocks.iter().collect();
            blocks.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap());
            for block in blocks {
                let src = block
                    .source_file
                    .as_deref()
                    .unwrap_or("<inline block code>");
                let pat = block.source_pattern_id.as_deref().unwrap_or("*");
                lines.push(format!(
                    "// Block {} [{:.3}..{:.3}] source={} pattern={}",
                    block.display_name(),
                    block.start,
                    block.end(),
                    src,
                    pat,
                ));
                if let Some(code) = block.resolved_code_with(&mut source_lookup) {
                    lines.push(code);
                } else {
                    lines.push("// (empty block)".to_string());
                }
                lines.push(String::new());
            }
        }
        lines.join("\n")
    }

    /// Авторасчёт конца аранжировки.
    pub fn auto_end(&self) -> f64 {
        self.end_cycles.unwrap_or_else(|| {
            self.rows
                .iter()
                .flat_map(|r| r.blocks.iter().map(|b| b.end()))
                .fold(0.0_f64, f64::max)
                .max(16.0)
        })
    }

    pub fn block_mut(&mut self, row_id: u64, block_id: u64) -> Option<&mut PatternBlock> {
        self.rows
            .iter_mut()
            .find(|r| r.id == row_id)
            .and_then(|r| r.blocks.iter_mut().find(|b| b.id == block_id))
    }
}
