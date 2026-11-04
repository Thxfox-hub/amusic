use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::project_audio::{
    AudioClip, AudioClipId, AudioSource, AudioSourceId, AudioTrack, AudioTrackId, CursorMode,
    EditHistory, FadeCurve, FadeEdge, MasterOptions, MixerChannel, MixerChannelId,
    MixerChannelKind, PitchParams, ProjectAudioPool, ProjectAudioSnapshot, ProjectAudioState,
    SnapSettings, StretchParams,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StableId(pub String);

impl StableId {
    pub fn new(prefix: &str, legacy_id: u64) -> Self {
        Self(format!("{prefix}_{legacy_id}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn legacy_suffix(&self) -> Option<u64> {
        self.0.rsplit_once('_')?.1.parse().ok()
    }
}

impl From<String> for StableId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for StableId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpan {
    pub start: usize,
    pub end: usize,
}

impl SourceSpan {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArrangementIr {
    pub schema_version: u32,
    pub revision: u64,
    pub tracks: Vec<ArrangementTrack>,
    pub group_buses: Vec<ArrangementMixerBus>,
    pub return_buses: Vec<ArrangementMixerBus>,
    pub master: ArrangementMixerBus,
    pub master_options: MasterOptions,
    pub pool: ArrangementPool,
    pub selected_track: Option<StableId>,
    pub selected_clip: Option<StableId>,
    pub cursor_mode: CursorMode,
    pub snap: SnapSettings,
    pub legacy_next_id: u64,
}

impl ArrangementIr {
    pub fn empty() -> Self {
        ProjectAudioState::default().into()
    }

    pub fn find_clip(&self, clip_id: &StableId) -> Option<(&ArrangementTrack, &ArrangementClip)> {
        self.tracks.iter().find_map(|track| {
            track
                .clips
                .iter()
                .find(|clip| &clip.id == clip_id)
                .map(|clip| (track, clip))
        })
    }

    pub fn find_clip_mut(&mut self, clip_id: &StableId) -> Option<&mut ArrangementClip> {
        for track in &mut self.tracks {
            if let Some(index) = track.clips.iter().position(|clip| &clip.id == clip_id) {
                return Some(&mut track.clips[index]);
            }
        }
        None
    }

    pub fn to_project_audio_state(&self) -> ProjectAudioState {
        let mut next_id = self.legacy_next_id;
        for id in self.all_legacy_ids() {
            next_id = next_id.max(id.saturating_add(1));
        }

        let snapshot = ProjectAudioSnapshot {
            tracks: self
                .tracks
                .iter()
                .map(ArrangementTrack::to_audio_track)
                .collect(),
            group_buses: self
                .group_buses
                .iter()
                .map(ArrangementMixerBus::to_mixer_channel)
                .collect(),
            return_buses: self
                .return_buses
                .iter()
                .map(ArrangementMixerBus::to_mixer_channel)
                .collect(),
            master: self.master.to_mixer_channel(),
            master_options: self.master_options,
            pool: self.pool.to_audio_pool(),
            selected_track: self
                .selected_track
                .as_ref()
                .and_then(StableId::legacy_suffix),
            selected_clip: self
                .selected_clip
                .as_ref()
                .and_then(StableId::legacy_suffix),
            cursor_mode: self.cursor_mode,
            snap: self.snap.clone(),
            patterns_bus_id: self
                .group_buses
                .iter()
                .find(|bus| {
                    bus.raw.kind == MixerChannelKind::Group && bus.raw.name == "Patterns"
                })
                .map(|bus| bus.legacy_id),
            next_id: next_id.max(1),
        };

        let mut state = ProjectAudioState::default();
        snapshot.restore_into(&mut state);
        state
    }

    pub fn all_legacy_ids(&self) -> Vec<u64> {
        let mut ids = Vec::new();
        for track in &self.tracks {
            ids.push(track.legacy_id);
            ids.push(track.channel.legacy_id);
            ids.extend(track.clips.iter().map(|clip| clip.legacy_id));
        }
        ids.extend(self.group_buses.iter().map(|bus| bus.legacy_id));
        ids.extend(self.return_buses.iter().map(|bus| bus.legacy_id));
        ids.push(self.master.legacy_id);
        ids.extend(self.pool.sources.values().map(|source| source.legacy_id));
        ids
    }
}

impl From<&ProjectAudioState> for ArrangementIr {
    fn from(state: &ProjectAudioState) -> Self {
        let snapshot = ProjectAudioSnapshot::from_state(state);
        let pool = ArrangementPool::from_audio_pool(&snapshot.pool);
        let mut tracks: Vec<ArrangementTrack> = snapshot
            .tracks
            .iter()
            .map(ArrangementTrack::from_audio_track)
            .collect();
        for track in &mut tracks {
            for clip in &mut track.clips {
                if let Some(source) = pool.sources.get(&clip.source_id) {
                    clip.source_name = source.display_name();
                    clip.source_duration = source.raw.duration_seconds();
                }
            }
        }
        Self {
            schema_version: 1,
            revision: 0,
            tracks,
            group_buses: snapshot
                .group_buses
                .iter()
                .map(ArrangementMixerBus::from_mixer_channel)
                .collect(),
            return_buses: snapshot
                .return_buses
                .iter()
                .map(ArrangementMixerBus::from_mixer_channel)
                .collect(),
            master: ArrangementMixerBus::from_mixer_channel(&snapshot.master),
            master_options: snapshot.master_options,
            pool,
            selected_track: snapshot.selected_track.map(|id| StableId::new("trk", id)),
            selected_clip: snapshot.selected_clip.map(|id| StableId::new("clip", id)),
            cursor_mode: snapshot.cursor_mode,
            snap: snapshot.snap,
            legacy_next_id: snapshot.next_id,
        }
    }
}

impl From<ProjectAudioState> for ArrangementIr {
    fn from(state: ProjectAudioState) -> Self {
        Self::from(&state)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArrangementTrack {
    pub id: StableId,
    pub legacy_id: AudioTrackId,
    pub name: String,
    pub channel: ArrangementMixerBus,
    pub clips: Vec<ArrangementClip>,
    pub raw: AudioTrack,
    pub source_span: Option<SourceSpan>,
}

impl ArrangementTrack {
    fn from_audio_track(track: &AudioTrack) -> Self {
        Self {
            id: StableId::new("trk", track.id),
            legacy_id: track.id,
            name: track.name.clone(),
            channel: ArrangementMixerBus::from_mixer_channel(&track.channel),
            clips: track
                .clips
                .iter()
                .map(|clip| ArrangementClip::from_audio_clip(clip, track.id))
                .collect(),
            raw: track.clone(),
            source_span: None,
        }
    }

    fn to_audio_track(&self) -> AudioTrack {
        let mut track = self.raw.clone();
        track.id = self.legacy_id;
        track.name = self.name.clone();
        track.channel = self.channel.to_mixer_channel();
        track.clips = self
            .clips
            .iter()
            .map(ArrangementClip::to_audio_clip)
            .collect();
        track.sort_clips();
        track
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArrangementClip {
    pub id: StableId,
    pub legacy_id: AudioClipId,
    pub track_id: StableId,
    pub legacy_track_id: AudioTrackId,
    pub source_id: StableId,
    pub legacy_source_id: AudioSourceId,
    pub timeline_position: f64,
    pub source_offset: f64,
    pub length: f64,
    pub source_duration: f64,
    pub gain: f32,
    pub pan: f32,
    pub muted: bool,
    pub name: Option<String>,
    pub source_name: Option<String>,
    pub raw: AudioClip,
    pub source_span: Option<SourceSpan>,
}

impl ArrangementClip {
    fn from_audio_clip(clip: &AudioClip, track_id: AudioTrackId) -> Self {
        Self {
            id: StableId::new("clip", clip.id),
            legacy_id: clip.id,
            track_id: StableId::new("trk", track_id),
            legacy_track_id: track_id,
            source_id: StableId::new("src", clip.source_file_id),
            legacy_source_id: clip.source_file_id,
            timeline_position: clip.timeline_position,
            source_offset: clip.source_offset,
            length: clip.length,
            source_duration: 0.0,
            gain: clip.gain,
            pan: clip.pan,
            muted: clip.muted,
            name: clip.name.clone(),
            source_name: None,
            raw: clip.clone(),
            source_span: None,
        }
    }

    fn to_audio_clip(&self) -> AudioClip {
        let mut clip = self.raw.clone();
        clip.id = self.legacy_id;
        clip.source_file_id = self.legacy_source_id;
        clip.timeline_position = self.timeline_position;
        clip.source_offset = self.source_offset;
        clip.length = self.length.max(0.001);
        clip.gain = self.gain;
        clip.pan = self.pan;
        clip.muted = self.muted;
        clip.name = self.name.clone();
        clip
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArrangementMixerBus {
    pub id: StableId,
    pub legacy_id: MixerChannelId,
    pub raw: MixerChannel,
    pub source_span: Option<SourceSpan>,
}

impl ArrangementMixerBus {
    fn from_mixer_channel(channel: &MixerChannel) -> Self {
        Self {
            id: StableId::new("bus", channel.id),
            legacy_id: channel.id,
            raw: channel.clone(),
            source_span: None,
        }
    }

    fn to_mixer_channel(&self) -> MixerChannel {
        let mut channel = self.raw.clone();
        channel.id = self.legacy_id;
        channel
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArrangementPool {
    pub sources: HashMap<StableId, ArrangementSource>,
    pub reference_counts: HashMap<StableId, usize>,
}

impl ArrangementPool {
    fn from_audio_pool(pool: &ProjectAudioPool) -> Self {
        let sources = pool
            .sources
            .iter()
            .map(|(id, source)| {
                (
                    StableId::new("src", *id),
                    ArrangementSource {
                        id: StableId::new("src", *id),
                        legacy_id: *id,
                        raw: source.clone(),
                    },
                )
            })
            .collect();
        let reference_counts = pool
            .reference_counts
            .iter()
            .map(|(id, count)| (StableId::new("src", *id), *count))
            .collect();
        Self {
            sources,
            reference_counts,
        }
    }

    fn to_audio_pool(&self) -> ProjectAudioPool {
        let mut pool = ProjectAudioPool::default();
        pool.sources = self
            .sources
            .values()
            .map(|source| {
                let mut raw = source.raw.clone();
                raw.id = source.legacy_id;
                (source.legacy_id, raw)
            })
            .collect();
        pool.reference_counts = self
            .reference_counts
            .iter()
            .filter_map(|(id, count)| id.legacy_suffix().map(|legacy| (legacy, *count)))
            .collect();
        pool
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArrangementSource {
    pub id: StableId,
    pub legacy_id: AudioSourceId,
    pub raw: AudioSource,
}

impl ArrangementSource {
    pub fn display_name(&self) -> Option<String> {
        self.raw
            .path
            .file_stem()
            .and_then(|name| name.to_str())
            .filter(|name| !name.trim().is_empty())
            .map(str::to_string)
    }
}

#[derive(Debug, Clone)]
pub enum ArrangementEdit {
    SplitClip {
        clip_id: StableId,
        at: f64,
    },
    DuplicateClip {
        clip_id: StableId,
    },
    MoveClip {
        clip_id: StableId,
        new_start: f64,
    },
    TrimClipStart {
        clip_id: StableId,
        new_start: f64,
    },
    TrimClipEnd {
        clip_id: StableId,
        new_end: f64,
    },
    DeleteClip {
        clip_id: StableId,
    },
    SetFade {
        clip_id: StableId,
        edge: FadeEdge,
        length: f64,
        curve: FadeCurve,
    },
    SetStretch {
        clip_id: StableId,
        stretch: StretchParams,
    },
    SetPitch {
        clip_id: StableId,
        pitch: PitchParams,
    },
    SetClipMute {
        clip_id: StableId,
        muted: bool,
    },
    SetClipGain {
        clip_id: StableId,
        gain: f32,
    },
    SetClipPan {
        clip_id: StableId,
        pan: f32,
    },
}

impl ArrangementEdit {
    pub fn target_id(&self) -> &StableId {
        match self {
            Self::SplitClip { clip_id, .. }
            | Self::DuplicateClip { clip_id }
            | Self::MoveClip { clip_id, .. }
            | Self::TrimClipStart { clip_id, .. }
            | Self::TrimClipEnd { clip_id, .. }
            | Self::DeleteClip { clip_id }
            | Self::SetFade { clip_id, .. }
            | Self::SetStretch { clip_id, .. }
            | Self::SetPitch { clip_id, .. }
            | Self::SetClipMute { clip_id, .. }
            | Self::SetClipGain { clip_id, .. }
            | Self::SetClipPan { clip_id, .. } => clip_id,
        }
    }
}

pub fn stable_clip_id(clip_id: AudioClipId) -> StableId {
    StableId::new("clip", clip_id)
}

#[allow(dead_code)]
fn _assert_edit_history_is_not_in_ir(_: &EditHistory) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::project_audio::AudioSource;

    #[test]
    fn project_audio_round_trip_preserves_clip_ids() {
        let mut project = ProjectAudioState::default();
        let source_id = project.add_source(AudioSource {
            id: 0,
            path: PathBuf::from("kick.wav"),
            sample_rate: 48_000,
            channels: 2,
            frames: 48_000,
            created_by_recording: false,
            latency_compensation_frames: 0,
            peak_path: None,
        });
        let track_id = project.selected_track.unwrap();
        let clip_id = project
            .insert_recording_clip(track_id, source_id, 0.25, 1.5)
            .unwrap();

        let ir = ArrangementIr::from(&project);
        let restored = ir.to_project_audio_state();

        assert_eq!(restored.selected_clip, Some(clip_id));
        assert!(restored.clip(clip_id).is_some());
        assert_eq!(restored.tracks[0].clips[0].source_file_id, source_id);
    }
}
