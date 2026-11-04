use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

pub type AudioTrackId = u64;
pub type AudioClipId = u64;
pub type AudioSourceId = u64;
pub type TakeLaneId = u64;
pub type MixerChannelId = u64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectAudioState {
    pub tracks: Vec<AudioTrack>,
    #[serde(default)]
    pub group_buses: Vec<MixerChannel>,
    #[serde(default = "default_return_buses")]
    pub return_buses: Vec<MixerChannel>,
    #[serde(default = "default_master_channel")]
    pub master: MixerChannel,
    #[serde(default)]
    pub master_options: MasterOptions,
    pub pool: ProjectAudioPool,
    pub edit_history: EditHistory,
    pub selected_track: Option<AudioTrackId>,
    pub selected_clip: Option<AudioClipId>,
    pub cursor_mode: CursorMode,
    pub snap: SnapSettings,
    /// Live Strudel pattern output is summed into this group bus (if set).
    #[serde(default)]
    pub patterns_bus_id: Option<MixerChannelId>,
    next_id: u64,
}

impl Default for ProjectAudioState {
    fn default() -> Self {
        let mut state = Self {
            tracks: Vec::new(),
            group_buses: Vec::new(),
            return_buses: default_return_buses(),
            master: default_master_channel(),
            master_options: MasterOptions::default(),
            pool: ProjectAudioPool::default(),
            edit_history: EditHistory::default(),
            selected_track: None,
            selected_clip: None,
            cursor_mode: CursorMode::Select,
            snap: SnapSettings::default(),
            patterns_bus_id: None,
            next_id: 1,
        };
        let patterns_id = state.alloc_id();
        state.group_buses.push(MixerChannel::new(
            patterns_id,
            "Patterns",
            MixerChannelKind::Group,
        ));
        state.patterns_bus_id = Some(patterns_id);

        let reverb_id = state.alloc_id();
        let mut reverb = MixerChannel::new(reverb_id, "Reverb", MixerChannelKind::Return);
        reverb.solo_safe = true;
        reverb.inserts.push(FxSlot::reverb(0.45, 0.72));
        state.return_buses.push(reverb);

        if let Some(patterns) = state
            .group_buses
            .iter_mut()
            .find(|bus| bus.id == patterns_id)
        {
            patterns.sends.push(SendSlot::new(reverb_id));
        }

        let track_id = state.alloc_id();
        state.tracks.push(AudioTrack::new(track_id, "Audio 1"));
        state.selected_track = Some(track_id);
        state
    }
}

impl ProjectAudioState {
    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1).max(1);
        id
    }

    pub fn add_source(&mut self, mut source: AudioSource) -> AudioSourceId {
        if source.id == 0 {
            source.id = self.alloc_id();
        }
        let id = source.id;
        self.pool.sources.insert(id, source);
        id
    }

    pub fn selected_track_mut(&mut self) -> Option<&mut AudioTrack> {
        let id = self.selected_track?;
        self.tracks.iter_mut().find(|track| track.id == id)
    }

    pub fn selected_track(&self) -> Option<&AudioTrack> {
        let id = self.selected_track?;
        self.tracks.iter().find(|track| track.id == id)
    }

    pub fn clip(&self, clip_id: AudioClipId) -> Option<&AudioClip> {
        self.tracks
            .iter()
            .flat_map(|track| track.clips.iter())
            .find(|clip| clip.id == clip_id)
    }

    pub fn clip_mut(&mut self, clip_id: AudioClipId) -> Option<&mut AudioClip> {
        self.tracks
            .iter_mut()
            .flat_map(|track| track.clips.iter_mut())
            .find(|clip| clip.id == clip_id)
    }

    pub fn track_mut(&mut self, track_id: AudioTrackId) -> Option<&mut AudioTrack> {
        self.tracks.iter_mut().find(|track| track.id == track_id)
    }

    pub fn mixer_channel(&self, channel_id: MixerChannelId) -> Option<&MixerChannel> {
        if self.master.id == channel_id {
            return Some(&self.master);
        }
        self.tracks
            .iter()
            .find(|track| track.channel.id == channel_id)
            .map(|track| &track.channel)
            .or_else(|| self.group_buses.iter().find(|bus| bus.id == channel_id))
            .or_else(|| self.return_buses.iter().find(|bus| bus.id == channel_id))
    }

    pub fn mixer_channel_mut(&mut self, channel_id: MixerChannelId) -> Option<&mut MixerChannel> {
        if self.master.id == channel_id {
            return Some(&mut self.master);
        }
        if let Some(track) = self
            .tracks
            .iter_mut()
            .find(|track| track.channel.id == channel_id)
        {
            return Some(&mut track.channel);
        }
        if let Some(bus) = self.group_buses.iter_mut().find(|bus| bus.id == channel_id) {
            return Some(bus);
        }
        self.return_buses
            .iter_mut()
            .find(|bus| bus.id == channel_id)
    }

    pub fn add_group_bus(&mut self, name: impl Into<String>) -> MixerChannelId {
        let id = self.alloc_id();
        self.group_buses
            .push(MixerChannel::new(id, name, MixerChannelKind::Group));
        id
    }

    pub fn add_return_bus(&mut self, name: impl Into<String>) -> MixerChannelId {
        let id = self.alloc_id();
        let mut channel = MixerChannel::new(id, name, MixerChannelKind::Return);
        channel.solo_safe = true;
        self.return_buses.push(channel);
        id
    }

    /// Upgrade older projects to the demo mixer layout: Patterns bus, Reverb return, example send.
    pub fn ensure_mixer_template(&mut self) {
        if self.patterns_bus_id.is_none() {
            if let Some(id) = self
                .group_buses
                .iter()
                .find(|bus| bus.name == "Patterns")
                .map(|bus| bus.id)
            {
                self.patterns_bus_id = Some(id);
            } else {
                let id = self.alloc_id();
                self.group_buses
                    .insert(0, MixerChannel::new(id, "Patterns", MixerChannelKind::Group));
                self.patterns_bus_id = Some(id);
            }
        }

        let reverb_id = if let Some(existing) = self
            .return_buses
            .iter()
            .find(|bus| bus.name == "Reverb")
            .map(|bus| bus.id)
        {
            existing
        } else if self.return_buses.is_empty() {
            let id = self.alloc_id();
            let mut reverb = MixerChannel::new(id, "Reverb", MixerChannelKind::Return);
            reverb.solo_safe = true;
            reverb.inserts.push(FxSlot::reverb(0.45, 0.72));
            self.return_buses.push(reverb);
            id
        } else {
            self.return_buses[0].id
        };

        if let Some(patterns_id) = self.patterns_bus_id {
            if let Some(patterns) = self.mixer_channel_mut(patterns_id) {
                if !patterns
                    .sends
                    .iter()
                    .any(|send| send.target_bus_id == reverb_id)
                {
                    patterns.sends.push(SendSlot::new(reverb_id));
                }
            }
        }

        if let Some(reverb) = self.mixer_channel_mut(reverb_id) {
            if reverb.inserts.is_empty() {
                reverb.inserts.push(FxSlot::reverb(0.45, 0.72));
            }
        }

        let _ = self.prune_broken_audio_sources();
    }

    /// Remove clips/sources that point at empty or missing WAV files (failed mic recordings).
    pub fn prune_broken_audio_sources(&mut self) -> usize {
        let broken: Vec<AudioSourceId> = self
            .pool
            .sources
            .iter()
            .filter(|(_, source)| source_is_broken(source))
            .map(|(id, _)| *id)
            .collect();
        if broken.is_empty() {
            return 0;
        }
        for track in &mut self.tracks {
            track
                .clips
                .retain(|clip| !broken.contains(&clip.source_file_id));
        }
        for id in &broken {
            self.pool.sources.remove(id);
            self.pool.peak_files.remove(id);
            if self.selected_clip.is_some() {
                let still_exists = self
                    .tracks
                    .iter()
                    .flat_map(|track| track.clips.iter())
                    .any(|clip| Some(clip.id) == self.selected_clip);
                if !still_exists {
                    self.selected_clip = None;
                }
            }
        }
        broken.len()
    }

    pub fn set_channel_output(
        &mut self,
        channel_id: MixerChannelId,
        output: OutputTarget,
    ) -> Result<(), RoutingError> {
        let old_output = self
            .mixer_channel(channel_id)
            .map(|channel| channel.output_target)
            .ok_or(RoutingError::MissingChannel(channel_id))?;
        if self.master.id == channel_id {
            return Err(RoutingError::MasterCannotRoute);
        }
        if let Some(channel) = self.mixer_channel_mut(channel_id) {
            channel.output_target = output;
        }
        if let Err(err) = self.validate_routing_graph() {
            if let Some(channel) = self.mixer_channel_mut(channel_id) {
                channel.output_target = old_output;
            }
            return Err(err);
        }
        Ok(())
    }

    pub fn set_send_target(
        &mut self,
        channel_id: MixerChannelId,
        send_index: usize,
        target_bus_id: MixerChannelId,
    ) -> Result<(), RoutingError> {
        let old_target = self
            .mixer_channel(channel_id)
            .and_then(|channel| channel.sends.get(send_index))
            .map(|send| send.target_bus_id)
            .ok_or(RoutingError::MissingChannel(channel_id))?;
        if let Some(channel) = self.mixer_channel_mut(channel_id) {
            if let Some(send) = channel.sends.get_mut(send_index) {
                send.target_bus_id = target_bus_id;
            }
        }
        if let Err(err) = self.validate_routing_graph() {
            if let Some(channel) = self.mixer_channel_mut(channel_id) {
                if let Some(send) = channel.sends.get_mut(send_index) {
                    send.target_bus_id = old_target;
                }
            }
            return Err(err);
        }
        Ok(())
    }

    pub fn validate_routing_graph(&self) -> Result<(), RoutingError> {
        self.routing_order().map(|_| ())
    }

    pub fn routing_order(&self) -> Result<Vec<MixerChannelId>, RoutingError> {
        let mut visiting = HashSet::new();
        let mut visited = HashSet::new();
        let mut order = Vec::new();
        for channel in self.all_mixer_channels() {
            visit_routing_node(channel.id, self, &mut visiting, &mut visited, &mut order)?;
        }
        Ok(order)
    }

    pub fn effective_audible(&self, channel_id: MixerChannelId) -> EffectiveAudible {
        let Some(channel) = self.mixer_channel(channel_id) else {
            return EffectiveAudible {
                audible: false,
                muted_by_solo: false,
            };
        };
        if channel.mute {
            return EffectiveAudible {
                audible: false,
                muted_by_solo: false,
            };
        }
        let any_solo = self
            .all_mixer_channels()
            .into_iter()
            .any(|candidate| candidate.solo);
        if !any_solo || channel.solo || channel.solo_safe {
            return EffectiveAudible {
                audible: true,
                muted_by_solo: false,
            };
        }
        let audible = self.channel_feeds_solo(channel_id);
        EffectiveAudible {
            audible,
            muted_by_solo: !audible,
        }
    }

    pub fn all_mixer_channels(&self) -> Vec<&MixerChannel> {
        let mut channels = Vec::with_capacity(
            self.tracks.len() + self.group_buses.len() + self.return_buses.len() + 1,
        );
        channels.extend(self.tracks.iter().map(|track| &track.channel));
        channels.extend(self.group_buses.iter());
        channels.extend(self.return_buses.iter());
        channels.push(&self.master);
        channels
    }

    fn channel_feeds_solo(&self, channel_id: MixerChannelId) -> bool {
        let mut stack = vec![channel_id];
        let mut seen = HashSet::new();
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            let Some(channel) = self.mixer_channel(id) else {
                continue;
            };
            if channel.solo || channel.solo_safe {
                return true;
            }
            match channel.output_target {
                OutputTarget::Master => stack.push(self.master.id),
                OutputTarget::Bus(target_id) => stack.push(target_id),
                OutputTarget::None => {}
            }
            for send in channel.sends.iter().filter(|send| send.enabled) {
                stack.push(send.target_bus_id);
            }
        }
        false
    }

    pub fn push_undo_snapshot(&mut self, label: impl Into<String>, before: ProjectAudioSnapshot) {
        let after = ProjectAudioSnapshot::from_state(self);
        self.edit_history.push(EditCommandSnapshot {
            label: label.into(),
            before,
            after,
        });
    }

    pub fn undo(&mut self) -> bool {
        if let Some(snapshot) = self.edit_history.undo() {
            snapshot.restore_into(self);
            true
        } else {
            false
        }
    }

    pub fn redo(&mut self) -> bool {
        if let Some(snapshot) = self.edit_history.redo() {
            snapshot.restore_into(self);
            true
        } else {
            false
        }
    }

    pub fn split_clip(&mut self, clip_id: AudioClipId, at: f64) -> bool {
        let before = ProjectAudioSnapshot::from_state(self);
        let new_id = self.alloc_id();
        let Some(track_index) = self
            .tracks
            .iter()
            .position(|track| track.clips.iter().any(|clip| clip.id == clip_id))
        else {
            return false;
        };
        let Some(clip_index) = self.tracks[track_index]
            .clips
            .iter()
            .position(|clip| clip.id == clip_id)
        else {
            return false;
        };
        let clip = self.tracks[track_index].clips[clip_index].clone();
        if at <= clip.timeline_position + 0.000_001
            || at >= clip.timeline_position + clip.length - 0.000_001
        {
            return false;
        }
        let left_len = at - clip.timeline_position;
        let right_len = clip.length - left_len;
        self.tracks[track_index].clips[clip_index].length = left_len;
        self.tracks[track_index].clips[clip_index].fade_out.length = self.tracks[track_index].clips
            [clip_index]
            .fade_out
            .length
            .min(left_len);

        let mut right = clip;
        right.id = new_id;
        right.timeline_position = at;
        right.source_offset += left_len * right.stretch.effective_source_rate();
        right.length = right_len;
        right.fade_in.length = right.fade_in.length.min(right_len);
        self.tracks[track_index].clips.insert(clip_index + 1, right);
        self.selected_clip = Some(new_id);
        self.push_undo_snapshot("Split clip", before);
        true
    }

    pub fn duplicate_clip(&mut self, clip_id: AudioClipId) -> bool {
        let before = ProjectAudioSnapshot::from_state(self);
        let new_id = self.alloc_id();
        let Some(track) = self
            .tracks
            .iter_mut()
            .find(|track| track.clips.iter().any(|clip| clip.id == clip_id))
        else {
            return false;
        };
        let Some(source) = track.clips.iter().find(|clip| clip.id == clip_id).cloned() else {
            return false;
        };
        let mut duplicate = source.clone();
        duplicate.id = new_id;
        duplicate.timeline_position = source.timeline_position + source.length;
        track.clips.push(duplicate);
        track.clips.sort_by(|a, b| {
            a.timeline_position
                .partial_cmp(&b.timeline_position)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        self.selected_clip = Some(new_id);
        self.push_undo_snapshot("Duplicate clip", before);
        true
    }

    pub fn delete_clip(&mut self, clip_id: AudioClipId, ripple: bool) -> bool {
        let before = ProjectAudioSnapshot::from_state(self);
        for track in &mut self.tracks {
            if let Some(index) = track.clips.iter().position(|clip| clip.id == clip_id) {
                let removed = track.clips.remove(index);
                if ripple {
                    for clip in &mut track.clips {
                        if clip.timeline_position >= removed.timeline_position + removed.length {
                            clip.timeline_position =
                                (clip.timeline_position - removed.length).max(0.0);
                        }
                    }
                }
                self.selected_clip = None;
                self.push_undo_snapshot("Delete clip", before);
                return true;
            }
        }
        false
    }

    pub fn move_clip(&mut self, clip_id: AudioClipId, new_start: f64) -> bool {
        let before = ProjectAudioSnapshot::from_state(self);
        let Some(track) = self
            .tracks
            .iter_mut()
            .find(|track| track.clips.iter().any(|clip| clip.id == clip_id))
        else {
            return false;
        };
        let Some(clip) = track.clips.iter_mut().find(|clip| clip.id == clip_id) else {
            return false;
        };
        let new_start = new_start.max(0.0);
        if (clip.timeline_position - new_start).abs() < 0.000_001 {
            return false;
        }
        clip.timeline_position = new_start;
        track.sort_clips();
        self.selected_clip = Some(clip_id);
        self.push_undo_snapshot("Move clip", before);
        true
    }

    pub fn trim_clip(
        &mut self,
        clip_id: AudioClipId,
        new_start: Option<f64>,
        new_end: Option<f64>,
    ) -> bool {
        let before = ProjectAudioSnapshot::from_state(self);
        let Some(clip) = self.clip_mut(clip_id) else {
            return false;
        };
        let old_start = clip.timeline_position;
        let old_end = clip.timeline_position + clip.length;
        let start = new_start.unwrap_or(old_start).clamp(0.0, old_end - 0.001);
        let end = new_end.unwrap_or(old_end).max(start + 0.001);
        let delta_source = (start - old_start) * clip.stretch.effective_source_rate();
        clip.timeline_position = start;
        clip.source_offset = (clip.source_offset + delta_source).max(0.0);
        clip.length = end - start;
        clip.fade_in.length = clip.fade_in.length.min(clip.length);
        clip.fade_out.length = clip.fade_out.length.min(clip.length);
        self.push_undo_snapshot("Trim clip", before);
        true
    }

    pub fn set_clip_fade(
        &mut self,
        clip_id: AudioClipId,
        edge: FadeEdge,
        length: f64,
        curve: Option<FadeCurve>,
    ) -> bool {
        let before = ProjectAudioSnapshot::from_state(self);
        let Some(clip) = self.clip_mut(clip_id) else {
            return false;
        };
        let fade = match edge {
            FadeEdge::In => &mut clip.fade_in,
            FadeEdge::Out => &mut clip.fade_out,
        };
        fade.length = length.clamp(0.0, clip.length);
        if let Some(curve) = curve {
            fade.curve = curve;
        }
        self.push_undo_snapshot("Set fade", before);
        true
    }

    pub fn set_clip_stretch(&mut self, clip_id: AudioClipId, stretch: StretchParams) -> bool {
        let before = ProjectAudioSnapshot::from_state(self);
        let Some(clip) = self.clip_mut(clip_id) else {
            return false;
        };
        clip.stretch = stretch;
        self.push_undo_snapshot("Set stretch", before);
        true
    }

    pub fn set_clip_pitch(&mut self, clip_id: AudioClipId, pitch: PitchParams) -> bool {
        let before = ProjectAudioSnapshot::from_state(self);
        let Some(clip) = self.clip_mut(clip_id) else {
            return false;
        };
        clip.pitch = pitch;
        self.push_undo_snapshot("Set pitch", before);
        true
    }

    pub fn insert_recording_clip(
        &mut self,
        track_id: AudioTrackId,
        source_id: AudioSourceId,
        timeline_position: f64,
        length: f64,
    ) -> Option<AudioClipId> {
        let clip_id = self.alloc_id();
        let clip = AudioClip {
            id: clip_id,
            source_file_id: source_id,
            timeline_position: timeline_position.max(0.0),
            source_offset: 0.0,
            length: length.max(0.001),
            gain: 1.0,
            pan: 0.0,
            fade_in: Fade::default(),
            fade_out: Fade::default(),
            stretch: StretchParams::default(),
            pitch: PitchParams::default(),
            warp_markers: Vec::new(),
            take_lanes: Vec::new(),
            selected_take: None,
            muted: false,
            name: None,
        };
        let track = self.track_mut(track_id)?;
        track.clips.push(clip);
        track.sort_clips();
        self.selected_clip = Some(clip_id);
        Some(clip_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioTrack {
    pub id: AudioTrackId,
    pub name: String,
    #[serde(default = "default_track_channel")]
    pub channel: MixerChannel,
    pub input: InputConfig,
    pub monitoring: MonitoringMode,
    pub record_state: RecordState,
    pub fx_chain: Vec<FxSlot>,
    pub clips: Vec<AudioClip>,
    pub color: [u8; 3],
    pub level_peak: f32,
    pub clipped: bool,
}

impl AudioTrack {
    pub fn new(id: AudioTrackId, name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            id,
            channel: MixerChannel::new(id, name.clone(), MixerChannelKind::Audio),
            name,
            input: InputConfig::default(),
            monitoring: MonitoringMode::Auto,
            record_state: RecordState::default(),
            fx_chain: Vec::new(),
            clips: Vec::new(),
            color: [0xEF, 0x44, 0x44],
            level_peak: 0.0,
            clipped: false,
        }
    }

    pub fn sort_clips(&mut self) {
        self.clips.sort_by(|a, b| {
            a.timeline_position
                .partial_cmp(&b.timeline_position)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputConfig {
    pub device_name: Option<String>,
    pub channel_mode: InputChannelMode,
    pub channel_a: usize,
    pub channel_b: Option<usize>,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            device_name: None,
            channel_mode: InputChannelMode::Mono,
            channel_a: 0,
            channel_b: Some(1),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputChannelMode {
    Mono,
    Stereo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MonitoringMode {
    On,
    Auto,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordState {
    pub armed: bool,
    pub mode: RecordMode,
    pub active_session: Option<RecordingSessionInfo>,
    pub punch: Option<PunchRange>,
    pub count_in_bars: f64,
    pub preroll_cycles: f64,
}

impl Default for RecordState {
    fn default() -> Self {
        Self {
            armed: false,
            mode: RecordMode::Replace,
            active_session: None,
            punch: None,
            count_in_bars: 0.5,
            preroll_cycles: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordMode {
    Replace,
    Overdub,
    Layered,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingSessionInfo {
    pub session_id: u64,
    pub started_at_cycle: f64,
    pub current_cycle: f64,
    pub file_path: PathBuf,
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PunchRange {
    pub start: f64,
    pub end: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FxSlot {
    pub name: String,
    pub enabled: bool,
    pub params: BTreeMap<String, f32>,
}

impl FxSlot {
    pub fn with_param(mut self, key: impl Into<String>, value: f32) -> Self {
        self.params.insert(key.into(), value);
        self
    }

    pub fn gain_db(gain_db: f32) -> Self {
        Self {
            name: "Gain".to_string(),
            enabled: true,
            params: BTreeMap::from([("gain_db".to_string(), gain_db)]),
        }
    }

    pub fn saturation(drive_db: f32) -> Self {
        Self {
            name: "Saturation".to_string(),
            enabled: true,
            params: BTreeMap::from([("drive".to_string(), drive_db)]),
        }
    }

    pub fn low_pass(cutoff_hz: f32) -> Self {
        Self {
            name: "LowPass".to_string(),
            enabled: true,
            params: BTreeMap::from([("cutoff_hz".to_string(), cutoff_hz)]),
        }
    }

    pub fn high_pass(cutoff_hz: f32) -> Self {
        Self {
            name: "HighPass".to_string(),
            enabled: true,
            params: BTreeMap::from([("cutoff_hz".to_string(), cutoff_hz)]),
        }
    }

    pub fn reverb(mix: f32, decay: f32) -> Self {
        Self {
            name: "Reverb".to_string(),
            enabled: true,
            params: BTreeMap::from([
                ("mix".to_string(), mix),
                ("decay".to_string(), decay),
            ]),
        }
    }

    pub fn compressor(threshold_db: f32, ratio: f32, makeup_db: f32) -> Self {
        Self {
            name: "Compressor".to_string(),
            enabled: true,
            params: BTreeMap::from([
                ("threshold_db".to_string(), threshold_db),
                ("ratio".to_string(), ratio),
                ("attack_ms".to_string(), 15.0),
                ("release_ms".to_string(), 150.0),
                ("makeup_db".to_string(), makeup_db),
            ]),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MixerChannel {
    pub id: MixerChannelId,
    pub name: String,
    pub kind: MixerChannelKind,
    pub volume_db: f32,
    pub pan: f32,
    pub pan_law: PanLaw,
    pub mute: bool,
    pub solo: bool,
    pub solo_safe: bool,
    pub inserts: Vec<FxSlot>,
    pub sends: Vec<SendSlot>,
    pub output_target: OutputTarget,
    #[serde(default)]
    pub automation: Vec<AutomationLane>,
    #[serde(skip)]
    pub meter: MeterState,
}

impl MixerChannel {
    pub fn new(id: MixerChannelId, name: impl Into<String>, kind: MixerChannelKind) -> Self {
        let output_target = match kind {
            MixerChannelKind::Master => OutputTarget::None,
            _ => OutputTarget::Master,
        };
        Self {
            id,
            name: name.into(),
            kind,
            volume_db: 0.0,
            pan: 0.0,
            pan_law: PanLaw::Minus3Db,
            mute: false,
            solo: false,
            solo_safe: matches!(kind, MixerChannelKind::Return),
            inserts: Vec::new(),
            sends: Vec::new(),
            output_target,
            automation: Vec::new(),
            meter: MeterState::default(),
        }
    }
}

impl Default for MixerChannel {
    fn default() -> Self {
        MixerChannel::new(0, "Channel", MixerChannelKind::Audio)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MixerChannelKind {
    Audio,
    Group,
    Return,
    Master,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PanLaw {
    Minus3Db,
    Minus45Db,
    Minus6Db,
}

impl PanLaw {
    pub fn center_gain_db(self) -> f32 {
        match self {
            PanLaw::Minus3Db => -3.0,
            PanLaw::Minus45Db => -4.5,
            PanLaw::Minus6Db => -6.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputTarget {
    Master,
    Bus(MixerChannelId),
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendSlot {
    pub target_bus_id: MixerChannelId,
    pub level_db: f32,
    pub pre_fader: bool,
    pub enabled: bool,
}

impl SendSlot {
    pub fn new(target_bus_id: MixerChannelId) -> Self {
        Self {
            target_bus_id,
            // -24 dB : une queue de reverb discrète. À -12 dB, les taps du DSP
            // s'entendaient comme des répétitions distinctes (effet « écho »).
            level_db: -24.0,
            pre_fader: false,
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutomationLane {
    pub parameter: AutomationParameter,
    pub mode: AutomationMode,
    pub points: Vec<AutomationPoint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutomationParameter {
    Volume,
    Pan,
    SendLevel { send_index: usize },
    Mute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutomationMode {
    Read,
    Write,
    Touch,
    Latch,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct AutomationPoint {
    pub cycle: f64,
    pub value: f32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MeterState {
    pub peak: f32,
    pub rms: f32,
    pub lufs_short: f32,
    pub clipped: bool,
    pub clip_hold_blocks: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct MasterOptions {
    pub limiter_enabled: bool,
    pub mono_monitor: bool,
    pub dim_enabled: bool,
    pub dim_db: f32,
    pub dither_enabled: bool,
    pub export_bit_depth: ExportBitDepth,
}

impl Default for MasterOptions {
    fn default() -> Self {
        Self {
            limiter_enabled: true,
            mono_monitor: false,
            dim_enabled: false,
            dim_db: -12.0,
            dither_enabled: false,
            export_bit_depth: ExportBitDepth::Float32,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExportBitDepth {
    Float32,
    Int24,
    Int16,
}

#[derive(Debug, Clone, Copy)]
pub struct EffectiveAudible {
    pub audible: bool,
    pub muted_by_solo: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingError {
    MissingChannel(MixerChannelId),
    MissingTarget(MixerChannelId),
    Cycle(MixerChannelId),
    MasterCannotRoute,
}

fn default_track_channel() -> MixerChannel {
    MixerChannel::new(0, "Audio", MixerChannelKind::Audio)
}

fn default_master_channel() -> MixerChannel {
    MixerChannel::new(0, "Master", MixerChannelKind::Master)
}

fn default_return_buses() -> Vec<MixerChannel> {
    Vec::new()
}

fn source_is_broken(source: &AudioSource) -> bool {
    if source.frames == 0 {
        return true;
    }
    let Ok(metadata) = std::fs::metadata(&source.path) else {
        return true;
    };
    if metadata.len() < 128 {
        return true;
    }
    hound::WavReader::open(&source.path)
        .map(|reader| reader.len() == 0)
        .unwrap_or(true)
}

fn visit_routing_node(
    channel_id: MixerChannelId,
    state: &ProjectAudioState,
    visiting: &mut HashSet<MixerChannelId>,
    visited: &mut HashSet<MixerChannelId>,
    order: &mut Vec<MixerChannelId>,
) -> Result<(), RoutingError> {
    if visited.contains(&channel_id) {
        return Ok(());
    }
    if !visiting.insert(channel_id) {
        return Err(RoutingError::Cycle(channel_id));
    }
    let channel = state
        .mixer_channel(channel_id)
        .ok_or(RoutingError::MissingChannel(channel_id))?;
    let mut targets = Vec::new();
    match channel.output_target {
        OutputTarget::Master => targets.push(state.master.id),
        OutputTarget::Bus(target_id) => targets.push(target_id),
        OutputTarget::None => {}
    }
    targets.extend(
        channel
            .sends
            .iter()
            .filter(|send| send.enabled)
            .map(|send| send.target_bus_id),
    );
    for target_id in targets {
        if state.mixer_channel(target_id).is_none() {
            return Err(RoutingError::MissingTarget(target_id));
        }
        visit_routing_node(target_id, state, visiting, visited, order)?;
    }
    visiting.remove(&channel_id);
    visited.insert(channel_id);
    order.push(channel_id);
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioClip {
    pub id: AudioClipId,
    pub source_file_id: AudioSourceId,
    pub timeline_position: f64,
    pub source_offset: f64,
    pub length: f64,
    pub gain: f32,
    pub pan: f32,
    pub fade_in: Fade,
    pub fade_out: Fade,
    pub stretch: StretchParams,
    pub pitch: PitchParams,
    pub warp_markers: Vec<WarpMarker>,
    pub take_lanes: Vec<TakeLane>,
    pub selected_take: Option<TakeLaneId>,
    pub muted: bool,
    pub name: Option<String>,
}

impl AudioClip {
    pub fn end(&self) -> f64 {
        self.timeline_position + self.length
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FadeEdge {
    In,
    Out,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Fade {
    pub length: f64,
    pub curve: FadeCurve,
}

impl Default for Fade {
    fn default() -> Self {
        Self {
            length: 0.0,
            curve: FadeCurve::EqualPower,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FadeCurve {
    Linear,
    EqualPower,
    Exponential,
    SCurve,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StretchParams {
    pub mode: StretchMode,
    pub algorithm: StretchAlgorithm,
    pub ratio: f64,
    pub preserve_formants: bool,
    pub follows_project_tempo: bool,
}

impl Default for StretchParams {
    fn default() -> Self {
        Self {
            mode: StretchMode::Free,
            algorithm: StretchAlgorithm::Complex,
            ratio: 1.0,
            preserve_formants: false,
            follows_project_tempo: false,
        }
    }
}

impl StretchParams {
    pub fn effective_source_rate(&self) -> f64 {
        (1.0 / self.ratio.max(0.001)).max(0.001)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StretchMode {
    Musical,
    Free,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StretchAlgorithm {
    Percussive,
    Tonal,
    Complex,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PitchParams {
    pub semitones: f32,
    pub cents: f32,
    pub formant_correction: bool,
    pub mode: PitchMode,
    pub scale_snap: Option<ScaleSnap>,
}

impl Default for PitchParams {
    fn default() -> Self {
        Self {
            semitones: 0.0,
            cents: 0.0,
            formant_correction: false,
            mode: PitchMode::Elastic,
            scale_snap: None,
        }
    }
}

impl PitchParams {
    pub fn ratio(&self) -> f32 {
        2.0_f32.powf((self.semitones + self.cents / 100.0) / 12.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PitchMode {
    Tape,
    Elastic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScaleSnap {
    pub key: String,
    pub scale: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WarpMarker {
    pub timeline_position: f64,
    pub source_position: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TakeLane {
    pub id: TakeLaneId,
    pub source_file_id: AudioSourceId,
    pub loop_index: u32,
    pub timeline_position: f64,
    pub source_offset: f64,
    pub length: f64,
    pub name: Option<String>,
    pub muted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectAudioPool {
    pub sources: HashMap<AudioSourceId, AudioSource>,
    pub reference_counts: HashMap<AudioSourceId, usize>,
    pub peak_files: HashMap<AudioSourceId, PathBuf>,
}

impl ProjectAudioPool {
    pub fn source(&self, id: AudioSourceId) -> Option<&AudioSource> {
        self.sources.get(&id)
    }

    pub fn refresh_reference_counts(&mut self, tracks: &[AudioTrack]) {
        self.reference_counts.clear();
        for clip in tracks.iter().flat_map(|track| track.clips.iter()) {
            *self
                .reference_counts
                .entry(clip.source_file_id)
                .or_insert(0) += 1;
            for take in &clip.take_lanes {
                *self
                    .reference_counts
                    .entry(take.source_file_id)
                    .or_insert(0) += 1;
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioSource {
    pub id: AudioSourceId,
    pub path: PathBuf,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    pub created_by_recording: bool,
    pub latency_compensation_frames: i64,
    pub peak_path: Option<PathBuf>,
}

impl AudioSource {
    pub fn duration_seconds(&self) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.frames as f64 / self.sample_rate as f64
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CursorMode {
    Select,
    Split,
    TimeStretch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapSettings {
    pub enabled: bool,
    pub grid_cycles: f64,
    pub snap_to_transients: bool,
    pub snap_to_markers: bool,
    pub zero_crossing_assist: bool,
    pub ripple_edit: bool,
}

impl Default for SnapSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            grid_cycles: 0.25,
            snap_to_transients: true,
            snap_to_markers: true,
            zero_crossing_assist: true,
            ripple_edit: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EditHistory {
    undo_stack: Vec<EditCommandSnapshot>,
    redo_stack: Vec<EditCommandSnapshot>,
}

impl EditHistory {
    pub fn push(&mut self, command: EditCommandSnapshot) {
        self.undo_stack.push(command);
        self.redo_stack.clear();
    }

    fn undo(&mut self) -> Option<ProjectAudioSnapshot> {
        let command = self.undo_stack.pop()?;
        let before = command.before.clone();
        self.redo_stack.push(command);
        Some(before)
    }

    fn redo(&mut self) -> Option<ProjectAudioSnapshot> {
        let command = self.redo_stack.pop()?;
        let after = command.after.clone();
        self.undo_stack.push(command);
        Some(after)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditCommandSnapshot {
    pub label: String,
    pub before: ProjectAudioSnapshot,
    pub after: ProjectAudioSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectAudioSnapshot {
    pub tracks: Vec<AudioTrack>,
    pub group_buses: Vec<MixerChannel>,
    pub return_buses: Vec<MixerChannel>,
    pub master: MixerChannel,
    pub master_options: MasterOptions,
    pub pool: ProjectAudioPool,
    pub selected_track: Option<AudioTrackId>,
    pub selected_clip: Option<AudioClipId>,
    pub cursor_mode: CursorMode,
    pub snap: SnapSettings,
    #[serde(default)]
    pub patterns_bus_id: Option<MixerChannelId>,
    pub next_id: u64,
}

impl ProjectAudioSnapshot {
    pub fn from_state(state: &ProjectAudioState) -> Self {
        Self {
            tracks: state.tracks.clone(),
            group_buses: state.group_buses.clone(),
            return_buses: state.return_buses.clone(),
            master: state.master.clone(),
            master_options: state.master_options,
            pool: state.pool.clone(),
            selected_track: state.selected_track,
            selected_clip: state.selected_clip,
            cursor_mode: state.cursor_mode,
            snap: state.snap.clone(),
            patterns_bus_id: state.patterns_bus_id,
            next_id: state.next_id,
        }
    }

    pub fn restore_into(self, state: &mut ProjectAudioState) {
        state.tracks = self.tracks;
        state.group_buses = self.group_buses;
        state.return_buses = self.return_buses;
        state.master = self.master;
        state.master_options = self.master_options;
        state.pool = self.pool;
        state.selected_track = self.selected_track;
        state.selected_clip = self.selected_clip;
        state.cursor_mode = self.cursor_mode;
        state.snap = self.snap;
        state.patterns_bus_id = self.patterns_bus_id;
        state.next_id = self.next_id;
    }
}

pub fn snap_cycle(position: f64, settings: &SnapSettings) -> f64 {
    if !settings.enabled || settings.grid_cycles <= 0.0 {
        return position.max(0.0);
    }
    (position / settings.grid_cycles).round() * settings.grid_cycles
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_clip() -> ProjectAudioState {
        let mut state = ProjectAudioState::default();
        let source_id = state.add_source(AudioSource {
            id: 0,
            path: PathBuf::from("test.wav"),
            sample_rate: 48_000,
            channels: 2,
            frames: 48_000,
            created_by_recording: false,
            latency_compensation_frames: 0,
            peak_path: None,
        });
        let track = state.selected_track.unwrap();
        state
            .insert_recording_clip(track, source_id, 0.0, 4.0)
            .unwrap();
        state
    }

    #[test]
    fn split_clip_preserves_total_length() {
        let mut state = state_with_clip();
        let clip_id = state.selected_clip.unwrap();
        assert!(state.split_clip(clip_id, 1.5));
        let track = state.selected_track().unwrap();
        assert_eq!(track.clips.len(), 2);
        let total: f64 = track.clips.iter().map(|clip| clip.length).sum();
        assert!((total - 4.0).abs() < 0.000_001);
        assert!(state.edit_history.can_undo());
    }

    #[test]
    fn undo_redo_restores_snapshots() {
        let mut state = state_with_clip();
        let clip_id = state.selected_clip.unwrap();
        assert!(state.duplicate_clip(clip_id));
        assert_eq!(state.selected_track().unwrap().clips.len(), 2);
        assert!(state.undo());
        assert_eq!(state.selected_track().unwrap().clips.len(), 1);
        assert!(state.redo());
        assert_eq!(state.selected_track().unwrap().clips.len(), 2);
    }

    #[test]
    fn default_mixer_has_patterns_reverb_and_send() {
        let state = ProjectAudioState::default();
        assert!(state.patterns_bus_id.is_some());
        assert!(
            state
                .group_buses
                .iter()
                .any(|bus| bus.name == "Patterns")
        );
        let reverb = state
            .return_buses
            .iter()
            .find(|bus| bus.name == "Reverb")
            .expect("reverb return");
        assert_eq!(reverb.inserts.len(), 1);
        assert_eq!(reverb.inserts[0].name, "Reverb");
        let patterns_id = state.patterns_bus_id.unwrap();
        let patterns = state.mixer_channel(patterns_id).unwrap();
        assert!(patterns
            .sends
            .iter()
            .any(|send| send.target_bus_id == reverb.id && send.enabled));
    }
}
