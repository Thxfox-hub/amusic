use std::sync::mpsc;
use std::thread;

use crate::arrangement_ir::{stable_clip_id, ArrangementEdit, ArrangementIr, StableId};
use crate::project_audio::ProjectAudioState;
use crate::strudel_ast::StrudelAstDoc;
use crate::strudel_codegen::generate_clip_fragment;
use crate::strudel_patch::CodePatch;

#[derive(Debug, Clone)]
pub struct SyncState {
    pub revision: u64,
    pub code: String,
    pub ir: ArrangementIr,
    pub status: SyncStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncStatus {
    Idle,
    Parsing,
    Synced,
    Conflict(String),
    Error(String),
}

#[derive(Debug)]
pub enum SyncResult {
    CodeParsed {
        revision: u64,
        ir: ArrangementIr,
    },
    EditApplied {
        base_revision: u64,
        new_revision: u64,
        patch: CodePatch,
        code: String,
        ir: ArrangementIr,
    },
    Conflict {
        revision: u64,
        message: String,
        affected: Vec<StableId>,
    },
    Error {
        revision: u64,
        message: String,
    },
}

enum SyncRequest {
    Parse {
        code: String,
        revision: u64,
    },
    ApplyEdit {
        code: String,
        revision: u64,
        ir: ArrangementIr,
        edit: ArrangementEdit,
    },
}

pub struct SyncEngine {
    tx: mpsc::Sender<SyncRequest>,
    rx: mpsc::Receiver<SyncResult>,
    pub state: SyncState,
}

impl Default for SyncEngine {
    fn default() -> Self {
        let (tx_req, rx_req) = mpsc::channel();
        let (tx_res, rx_res) = mpsc::channel();
        thread::spawn(move || sync_worker(rx_req, tx_res));

        let ir = ArrangementIr::empty();
        Self {
            tx: tx_req,
            rx: rx_res,
            state: SyncState {
                revision: 0,
                code: String::new(),
                ir,
                status: SyncStatus::Idle,
            },
        }
    }
}

impl SyncEngine {
    pub fn set_code(&mut self, code: String, revision: u64) {
        self.state.revision = revision;
        self.state.code = code.clone();
        self.state.status = SyncStatus::Parsing;
        let _ = self.tx.send(SyncRequest::Parse { code, revision });
    }

    pub fn apply_edit(&mut self, edit: ArrangementEdit) {
        self.state.status = SyncStatus::Parsing;
        let _ = self.tx.send(SyncRequest::ApplyEdit {
            code: self.state.code.clone(),
            revision: self.state.revision,
            ir: self.state.ir.clone(),
            edit,
        });
    }

    pub fn poll(&mut self) -> Vec<SyncResult> {
        let mut results = Vec::new();
        while let Ok(result) = self.rx.try_recv() {
            match &result {
                SyncResult::CodeParsed { revision, ir } if *revision >= self.state.revision => {
                    self.state.revision = *revision;
                    self.state.ir = ir.clone();
                    self.state.status = SyncStatus::Synced;
                }
                SyncResult::EditApplied {
                    new_revision,
                    code,
                    ir,
                    ..
                } if *new_revision >= self.state.revision => {
                    self.state.revision = *new_revision;
                    self.state.code = code.clone();
                    self.state.ir = ir.clone();
                    self.state.status = SyncStatus::Synced;
                }
                SyncResult::Conflict { message, .. } => {
                    self.state.status = SyncStatus::Conflict(message.clone());
                }
                SyncResult::Error { message, .. } => {
                    self.state.status = SyncStatus::Error(message.clone());
                }
                _ => {}
            }
            results.push(result);
        }
        results
    }

    pub fn project_audio_state(&self) -> ProjectAudioState {
        self.state.ir.to_project_audio_state()
    }

    pub fn set_project_audio_state(&mut self, project: &ProjectAudioState) {
        let mut ir = ArrangementIr::from(project);
        ir.revision = self.state.revision;
        self.state.ir = ir;
    }
}

fn sync_worker(rx: mpsc::Receiver<SyncRequest>, tx: mpsc::Sender<SyncResult>) {
    while let Ok(request) = rx.recv() {
        let result = match request {
            SyncRequest::Parse { code, revision } => {
                let mut ir = code_to_ir(&code);
                ir.revision = revision;
                SyncResult::CodeParsed { revision, ir }
            }
            SyncRequest::ApplyEdit {
                code,
                revision,
                ir,
                edit,
            } => apply_edit_to_code(code, revision, ir, edit),
        };
        let _ = tx.send(result);
    }
}

fn code_to_ir(code: &str) -> ArrangementIr {
    let ast = StrudelAstDoc::parse(code);
    let mut ir = ArrangementIr::empty();
    attach_spans_from_ast(&mut ir, &ast);
    ir
}

fn attach_spans_from_code(ir: &mut ArrangementIr, code: &str) {
    let ast = StrudelAstDoc::parse(code);
    attach_spans_from_ast(ir, &ast);
}

fn attach_spans_from_ast(ir: &mut ArrangementIr, ast: &StrudelAstDoc) {
    for track in &mut ir.tracks {
        for clip in &mut track.clips {
            if let Some(node) = ast.node_by_id(&clip.id) {
                clip.source_span = node.body_span.map(|body| {
                    crate::arrangement_ir::SourceSpan::new(node.metadata_span.start, body.end)
                });
            }
        }
    }
}

fn apply_edit_to_code(
    code: String,
    revision: u64,
    mut ir: ArrangementIr,
    edit: ArrangementEdit,
) -> SyncResult {
    attach_spans_from_code(&mut ir, &code);
    if let Err(message) = apply_edit_to_ir(&mut ir, &edit) {
        return SyncResult::Conflict {
            revision,
            message,
            affected: vec![edit.target_id().clone()],
        };
    }

    let ast = StrudelAstDoc::parse(&code);
    let patch = if let Some(node) = ast.node_by_id(edit.target_id()) {
        if let Some(replacement) = replacement_for_edit(&code, &ir, &edit, node) {
            let end = node
                .body_span
                .map(|span| span.end)
                .unwrap_or(node.metadata_span.end);
            CodePatch::replace_span(
                crate::arrangement_ir::SourceSpan::new(node.metadata_span.start, end),
                replacement,
            )
        } else {
            CodePatch::replace_span(
                crate::arrangement_ir::SourceSpan::new(
                    node.metadata_span.start,
                    node.metadata_span.end,
                ),
                String::new(),
            )
        }
    } else {
        CodePatch::empty()
    };

    match patch.apply(&code) {
        Ok(new_code) => {
            let new_revision = revision.saturating_add(1);
            ir.revision = new_revision;
            SyncResult::EditApplied {
                base_revision: revision,
                new_revision,
                patch,
                code: new_code,
                ir,
            }
        }
        Err(message) => SyncResult::Error { revision, message },
    }
}

fn replacement_for_edit(
    code: &str,
    ir: &ArrangementIr,
    edit: &ArrangementEdit,
    node: &crate::strudel_ast::ManagedNode,
) -> Option<String> {
    let indent = line_indent_at(code, node.metadata_span.start);
    let mut fragments = Vec::new();
    match edit {
        ArrangementEdit::SplitClip { clip_id, .. } => {
            let right_id = ir.selected_clip.as_ref();
            for track in &ir.tracks {
                for clip in &track.clips {
                    if &clip.id == clip_id || Some(&clip.id) == right_id {
                        fragments.push(generate_clip_fragment(track, clip, &indent));
                    }
                }
            }
            fragments.sort_by(|a, b| a.cmp(b));
        }
        ArrangementEdit::DeleteClip { .. } => {
            fragments.push(format!("{indent}silence"));
        }
        _ => {
            if let Some((track, clip)) = ir.find_clip(edit.target_id()) {
                fragments.push(generate_clip_fragment(track, clip, &indent));
            }
        }
    }
    if fragments.is_empty() {
        return None;
    }
    let end = node
        .body_span
        .map(|span| span.end)
        .unwrap_or(node.metadata_span.end);
    let mut replacement = fragments.join(",\n");
    if code[node.metadata_span.start..end].ends_with('\n') {
        replacement.push('\n');
    }
    Some(replacement)
}

fn apply_edit_to_ir(ir: &mut ArrangementIr, edit: &ArrangementEdit) -> Result<(), String> {
    let mut project = ir.to_project_audio_state();
    let ok = match edit {
        ArrangementEdit::SplitClip { clip_id, at } => {
            project.split_clip(legacy_clip_id(clip_id)?, *at)
        }
        ArrangementEdit::DuplicateClip { clip_id } => {
            project.duplicate_clip(legacy_clip_id(clip_id)?)
        }
        ArrangementEdit::MoveClip { clip_id, new_start } => {
            project.move_clip(legacy_clip_id(clip_id)?, *new_start)
        }
        ArrangementEdit::TrimClipStart { clip_id, new_start } => {
            project.trim_clip(legacy_clip_id(clip_id)?, Some(*new_start), None)
        }
        ArrangementEdit::TrimClipEnd { clip_id, new_end } => {
            project.trim_clip(legacy_clip_id(clip_id)?, None, Some(*new_end))
        }
        ArrangementEdit::DeleteClip { clip_id } => {
            project.delete_clip(legacy_clip_id(clip_id)?, false)
        }
        ArrangementEdit::SetFade {
            clip_id,
            edge,
            length,
            curve,
        } => project.set_clip_fade(legacy_clip_id(clip_id)?, *edge, *length, Some(*curve)),
        ArrangementEdit::SetStretch { clip_id, stretch } => {
            project.set_clip_stretch(legacy_clip_id(clip_id)?, stretch.clone())
        }
        ArrangementEdit::SetPitch { clip_id, pitch } => {
            project.set_clip_pitch(legacy_clip_id(clip_id)?, pitch.clone())
        }
        ArrangementEdit::SetClipMute { clip_id, muted } => {
            if let Some(clip) = project.clip_mut(legacy_clip_id(clip_id)?) {
                clip.muted = *muted;
                true
            } else {
                false
            }
        }
        ArrangementEdit::SetClipGain { clip_id, gain } => {
            if let Some(clip) = project.clip_mut(legacy_clip_id(clip_id)?) {
                clip.gain = *gain;
                true
            } else {
                false
            }
        }
        ArrangementEdit::SetClipPan { clip_id, pan } => {
            if let Some(clip) = project.clip_mut(legacy_clip_id(clip_id)?) {
                clip.pan = *pan;
                true
            } else {
                false
            }
        }
    };

    if !ok {
        return Err(format!(
            "target {} not found in arrangement",
            edit.target_id().as_str()
        ));
    }
    let mut next = ArrangementIr::from(&project);
    next.revision = ir.revision;
    *ir = next;
    Ok(())
}

fn legacy_clip_id(id: &StableId) -> Result<u64, String> {
    id.legacy_suffix()
        .ok_or_else(|| format!("clip id {} has no legacy suffix", id.as_str()))
}

fn line_indent_at(code: &str, offset: usize) -> String {
    let line_start = code[..offset.min(code.len())]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    code[line_start..offset.min(code.len())]
        .chars()
        .take_while(|ch| ch.is_whitespace())
        .collect()
}

pub fn clip_edit_id(clip_id: u64) -> StableId {
    stable_clip_id(clip_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrangement_ir::ArrangementEdit;
    use crate::project_audio::{AudioSource, ProjectAudioState};
    use crate::strudel_codegen::generate_arrangement_code;
    use std::path::PathBuf;

    #[test]
    fn edit_generates_replacement_code() {
        let mut project = ProjectAudioState::default();
        let source_id = project.add_source(AudioSource {
            id: 0,
            path: PathBuf::from("bd.wav"),
            sample_rate: 48_000,
            channels: 2,
            frames: 48_000,
            created_by_recording: false,
            latency_compensation_frames: 0,
            peak_path: None,
        });
        let track_id = project.selected_track.unwrap();
        let clip_id = project
            .insert_recording_clip(track_id, source_id, 0.0, 1.0)
            .unwrap();
        let ir = ArrangementIr::from(&project);
        let code = generate_arrangement_code(&ir);
        let result = apply_edit_to_code(
            code,
            1,
            ir,
            ArrangementEdit::MoveClip {
                clip_id: StableId::new("clip", clip_id),
                new_start: 0.5,
            },
        );
        match result {
            SyncResult::Conflict { .. } => {}
            SyncResult::EditApplied { .. } => {}
            other => panic!("unexpected result: {other:?}"),
        }
    }
}
