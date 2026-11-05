//! UI-модули: вкладки, транспорт, таймлайн, файлы, редактор, настройки, отладчик.

mod ai_pattern;
mod arrangement_view;
mod audio_timeline;
mod chain_seed;
mod code_visuals_view;
mod debugger;
mod editor_tab;
mod file_tree;
mod hydra_studio;
mod mixer;
mod settings_view;
mod sound_view;
mod timeline;
mod transport;

pub use ai_pattern::{ai_pattern_ui, AiPatternAction, AiPatternState};
pub use arrangement_view::{
    arrangement_ui, ArrangementAction, ArrangementSourceOption, ArrangementViewState,
};
pub use audio_timeline::{audio_timeline_ui, AudioTimelineAction, AudioTimelineViewState};
pub use chain_seed::{chain_seed_ui, ChainSeedAction};
pub use code_visuals_view::{visuals_strip_ui, VISUAL_STRIP_H};
pub use hydra_studio::{hydra_studio_ui, HydraAction, HydraStudioState, HYDRA_PRESETS};
pub use debugger::{
    debugger_ui, CatchpointKind, DebuggerAction, DebuggerState, SourceBreakpointKey,
};
pub use editor_tab::{
    should_tick_arrangement_for_example_state, ActiveSampleEvent, EditorAction, EditorTab,
    ExamplePlaybackState,
};
pub use file_tree::FileTree;
pub use mixer::mixer_ui;
pub use settings_view::settings_ui;
pub use sound_view::{sound_view_ui, SoundViewMode, SoundViewState};
pub use timeline::{pattern_timeline_ui, PatternWaveformLookup, TimelineAction, TimelineViewState};
pub use transport::{transport_ui, PlaybackSource, TransportAction, TransportState};

#[derive(Clone, Copy, PartialEq)]
pub enum TabId {
    Editor,
    Timeline,
    Files,
    Settings,
    Debugger,
}
