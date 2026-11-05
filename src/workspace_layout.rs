//! Гибкая разметка рабочего пространства: дерево сплитов и панелей с вкладками.
//! Пользователь может перетаскивать вкладки на край области (новая панель) или в другую панель.

use std::mem;

/// Тип контента панели (каждая вкладка — один из этих типов).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentId {
    /// Транспортная панель в стиле DAW (play/rec/punch/snap/meters).
    Transport,
    /// Редактор кода (вкладки файлов + код).
    Workspace,
    /// Таймлайн событий.
    Timeline,
    /// Микшер: channel strips, sends, returns, groups, master.
    Mixer,
    /// Palette de tous les sons par catégorie (synths, GM, samples) — insertion en un clic.
    SoundPalette,
    /// Дерево файлов открытой папки.
    Files,
    /// Дерево семплов (sample_folder).
    Samples,
    /// Настройки семплов (панель менеджера и strudel.json генератора).
    SamplesSettings,
    /// Документация по функциям Strudel.
    Documentation,
    /// Настройки приложения.
    Settings,
    /// Терминал (логи runner: stderr + отладка скачивания).
    Terminal,
    /// Список проблем по открытым файлам (ошибки/предупреждения).
    Problems,
    /// Отладчик: брейкпоинты, вотчпоинты, кэтчпоинты.
    Debugger,
    /// Линейная аранжировка паттернов (размещение блоков на временной сетке).
    Arrangement,
    /// On-chain / NFT seed → Strudel pattern (no Hydra).
    ChainSeed,
    /// Hydra sketch editor + Visual Stage.
    Hydra,
    /// Local Ollama → Strudel pattern (no Hydra / no chain).
    AiPattern,
    /// Vues du son : rythme du pattern, ondes des samples, spectre live.
    SoundView,
}

impl ContentId {
    pub fn label(self) -> &'static str {
        match self {
            ContentId::Transport => "Transport",
            ContentId::Workspace => "Espace de travail",
            ContentId::Timeline => "Timeline",
            ContentId::Mixer => "Mixer",
            ContentId::SoundPalette => "Sons",
            ContentId::Files => "Fichiers",
            ContentId::Samples => "Samples",
            ContentId::SamplesSettings => "Paramètres des samples",
            ContentId::Documentation => "Documentation",
            ContentId::Settings => "Paramètres",
            ContentId::Terminal => "Terminal",
            ContentId::Problems => "Problèmes",
            ContentId::Debugger => "Débogueur",
            ContentId::Arrangement => "Arrangement",
            ContentId::ChainSeed => "Chain Seed",
            ContentId::Hydra => "Hydra",
            ContentId::AiPattern => "AI Pattern",
            ContentId::SoundView => "Vue",
        }
    }

    /// Строка для drag payload.
    pub fn payload_str(self) -> &'static str {
        match self {
            ContentId::Transport => "Transport",
            ContentId::Workspace => "Workspace",
            ContentId::Timeline => "Timeline",
            ContentId::Mixer => "Mixer",
            ContentId::SoundPalette => "SoundPalette",
            ContentId::Files => "Files",
            ContentId::Samples => "Samples",
            ContentId::SamplesSettings => "SamplesSettings",
            ContentId::Documentation => "Documentation",
            ContentId::Settings => "Settings",
            ContentId::Terminal => "Terminal",
            ContentId::Problems => "Problems",
            ContentId::Debugger => "Debugger",
            ContentId::Arrangement => "Arrangement",
            ContentId::ChainSeed => "ChainSeed",
            ContentId::Hydra => "Hydra",
            ContentId::AiPattern => "AiPattern",
            ContentId::SoundView => "SoundView",
        }
    }

    pub fn from_payload(s: &str) -> Option<ContentId> {
        match s {
            "Transport" => Some(ContentId::Transport),
            "Workspace" => Some(ContentId::Workspace),
            "Timeline" => Some(ContentId::Timeline),
            // Old sessions may still contain the temporary standalone DAW tab payload.
            "AudioTimeline" => Some(ContentId::Timeline),
            "Mixer" => Some(ContentId::Mixer),
            "SoundPalette" => Some(ContentId::SoundPalette),
            "Files" => Some(ContentId::Files),
            "Samples" => Some(ContentId::Samples),
            "SamplesSettings" => Some(ContentId::SamplesSettings),
            "Documentation" => Some(ContentId::Documentation),
            "Settings" => Some(ContentId::Settings),
            "Terminal" => Some(ContentId::Terminal),
            "Problems" => Some(ContentId::Problems),
            "Debugger" => Some(ContentId::Debugger),
            "Arrangement" => Some(ContentId::Arrangement),
            "ChainSeed" => Some(ContentId::ChainSeed),
            "Hydra" => Some(ContentId::Hydra),
            "AiPattern" => Some(ContentId::AiPattern),
            "SoundView" => Some(ContentId::SoundView),
            _ => None,
        }
    }
}

/// Данные одной панели: список вкладок и активная.
#[derive(Clone, Default)]
pub struct PanelData {
    pub tabs: Vec<ContentId>,
    pub active: usize,
}

impl PanelData {
    pub fn new(tabs: Vec<ContentId>) -> Self {
        let active = 0;
        Self { tabs, active }
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn remove_content(&mut self, id: ContentId) {
        if let Some(pos) = self.tabs.iter().position(|&c| c == id) {
            self.tabs.remove(pos);
            if self.active >= self.tabs.len() && !self.tabs.is_empty() {
                self.active = self.tabs.len() - 1;
            } else if self.tabs.is_empty() {
                self.active = 0;
            }
        }
    }

    pub fn add_content(&mut self, id: ContentId) {
        if !self.tabs.contains(&id) {
            self.tabs.push(id);
        }
    }

    pub fn active_content(&self) -> Option<ContentId> {
        self.tabs.get(self.active).copied()
    }
}

/// Узел дерева разметки: либо панель с вкладками, либо сплит из двух подузлов.
#[derive(Clone)]
pub enum LayoutNode {
    Panel(PanelData),
    Split {
        horizontal: bool,
        ratio: f32,
        a: Box<LayoutNode>,
        b: Box<LayoutNode>,
    },
}

impl LayoutNode {
    pub fn panel(tabs: Vec<ContentId>) -> Self {
        LayoutNode::Panel(PanelData::new(tabs))
    }

    /// Разметка по умолчанию: транспорт сверху, снизу редактор слева и таймлайн справа.
    pub fn default_layout() -> Self {
        LayoutNode::Split {
            horizontal: false,
            ratio: 0.18,
            a: Box::new(LayoutNode::panel(vec![ContentId::Transport])),
            b: Box::new(LayoutNode::Split {
                horizontal: true,
                ratio: 0.55,
                a: Box::new(LayoutNode::panel(vec![
                    ContentId::Workspace,
                    ContentId::Files,
                    ContentId::Samples,
                    ContentId::Documentation,
                    ContentId::Settings,
                    ContentId::Terminal,
                    ContentId::Problems,
                    ContentId::Debugger,
                    ContentId::Hydra,
                    ContentId::ChainSeed,
                    ContentId::AiPattern,
                ])),
                b: Box::new(LayoutNode::panel(vec![
                    ContentId::Timeline,
                    ContentId::Mixer,
                    ContentId::SoundPalette,
                    ContentId::SoundView,
                ])),
            }),
        }
    }
}

impl Default for LayoutNode {
    fn default() -> Self {
        Self::default_layout()
    }
}

impl LayoutNode {
    /// Проверить, есть ли вкладка с контентом `id` где-то в дереве.
    pub fn has_content(&self, id: ContentId) -> bool {
        match self {
            LayoutNode::Panel(data) => data.tabs.contains(&id),
            LayoutNode::Split { a, b, .. } => a.has_content(id) || b.has_content(id),
        }
    }

    /// Удалить вкладку с контентом `id` из той панели, где она есть (рекурсивно).
    pub fn remove_content(&mut self, id: ContentId) {
        match self {
            LayoutNode::Panel(data) => data.remove_content(id),
            LayoutNode::Split { a, b, .. } => {
                a.remove_content(id);
                b.remove_content(id);
            }
        }
    }

    /// Схлопнуть сплиты, у которых один из детей — пустая панель. Возвращает true, если что-то изменилось.
    pub fn collapse_empty_splits(&mut self) -> bool {
        let mut changed = false;
        loop {
            if !self.collapse_one() {
                break;
            }
            changed = true;
        }
        changed
    }

    fn collapse_one(&mut self) -> bool {
        match self {
            LayoutNode::Panel(_) => false,
            LayoutNode::Split { a, b, .. } => {
                a.collapse_one();
                b.collapse_one();
                if let LayoutNode::Panel(data) = a.as_mut() {
                    if data.tabs.is_empty() {
                        let other =
                            std::mem::replace(b.as_mut(), LayoutNode::Panel(PanelData::default()));
                        *self = other;
                        return true;
                    }
                }
                if let LayoutNode::Panel(data) = b.as_mut() {
                    if data.tabs.is_empty() {
                        let other =
                            std::mem::replace(a.as_mut(), LayoutNode::Panel(PanelData::default()));
                        *self = other;
                        return true;
                    }
                }
                false
            }
        }
    }

    /// Получить мутабельную ссылку на узел по пути. Путь: 0 = первый ребёнок (a), 1 = второй (b).
    /// Найти путь к первой панели в дереве (для добавления новых вкладок без перетаскивания).
    pub fn first_panel_path(&self) -> Vec<usize> {
        let mut path = Vec::new();
        let mut node = self;
        loop {
            match node {
                LayoutNode::Panel(_) => return path,
                LayoutNode::Split { a, .. } => {
                    path.push(0);
                    node = a;
                }
            }
        }
    }

    pub fn node_at_path_mut(&mut self, path: &[usize]) -> Option<&mut LayoutNode> {
        if path.is_empty() {
            return Some(self);
        }
        match self {
            LayoutNode::Panel(_) => None,
            LayoutNode::Split { a, b, .. } => {
                let idx = path[0];
                let child = if idx == 0 { a.as_mut() } else { b.as_mut() };
                child.node_at_path_mut(&path[1..])
            }
        }
    }

    /// Выставить всем сплитам в дереве ratio = 0.5 (деление 50/50).
    pub fn set_all_ratios_half(&mut self) {
        match self {
            LayoutNode::Panel(_) => {}
            LayoutNode::Split { ratio, a, b, .. } => {
                *ratio = 0.5;
                a.set_all_ratios_half();
                b.set_all_ratios_half();
            }
        }
    }

    /// Вставить новый сплит на месте панели по пути.
    /// `new_panel_first`: true = новая панель с вкладкой будет первым ребёнком (a), иначе вторым (b).
    pub fn split_at_path(
        &mut self,
        path: &[usize],
        horizontal: bool,
        new_tab: ContentId,
        ratio: f32,
        new_panel_first: bool,
    ) -> bool {
        let node = match self.node_at_path_mut(path) {
            Some(n) => n,
            None => return false,
        };
        if let LayoutNode::Panel(data) = node {
            data.remove_content(new_tab);
            if data.tabs.is_empty() {
                data.tabs.push(new_tab);
                return false;
            }
            let new_panel = LayoutNode::Panel(PanelData {
                tabs: vec![new_tab],
                active: 0,
            });
            let old_data = mem::take(data);
            let (a, b) = if new_panel_first {
                (Box::new(new_panel), Box::new(LayoutNode::Panel(old_data)))
            } else {
                (Box::new(LayoutNode::Panel(old_data)), Box::new(new_panel))
            };
            *node = LayoutNode::Split {
                horizontal,
                ratio,
                a,
                b,
            };
            true
        } else {
            false
        }
    }
}
