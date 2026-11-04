use crate::arrangement_ir::{ArrangementClip, ArrangementIr, ArrangementTrack, StableId};

pub fn generate_arrangement_code(ir: &ArrangementIr) -> String {
    let mut out = String::new();
    out.push_str("// @struidel arrangement version=1\n");
    out.push_str("stack(\n");

    let mut first = true;
    let mut tracks: Vec<&ArrangementTrack> = ir.tracks.iter().collect();
    tracks.sort_by(|a, b| a.id.cmp(&b.id));
    for track in tracks {
        let mut clips: Vec<&ArrangementClip> = track.clips.iter().collect();
        clips.sort_by(|a, b| {
            a.timeline_position
                .partial_cmp(&b.timeline_position)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        for clip in clips {
            if !first {
                out.push_str(",\n");
            }
            first = false;
            out.push_str(&generate_clip_fragment(track, clip, "  "));
        }
    }

    out.push_str("\n)\n");
    out
}

pub fn generate_clip_fragment(
    track: &ArrangementTrack,
    clip: &ArrangementClip,
    indent: &str,
) -> String {
    let mut out = String::new();
    out.push_str(indent);
    out.push_str(&format!(
        "// @struidel clip id={} track={} source={} start={} length={} offset={} gain={} pan={} muted={}\n",
        clip.id.as_str(),
        track.id.as_str(),
        clip.source_id.as_str(),
        fmt_f64(clip.timeline_position),
        fmt_f64(clip.length),
        fmt_f64(clip.source_offset),
        fmt_f32(clip.gain),
        fmt_f32(clip.pan),
        clip.muted
    ));
    out.push_str(indent);
    out.push_str(&clip_expression(clip));
    out
}

pub fn clip_expression(clip: &ArrangementClip) -> String {
    if clip.muted {
        return "silence".to_string();
    }
    let name = clip
        .name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .or_else(|| {
            clip.source_name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
        })
        .map(str::to_string)
        .unwrap_or_else(|| source_label(&clip.source_id));
    let mut expr = format!("s(\"{}\")", escape_string(&name));
    if clip.source_duration > 0.0 {
        let begin = (clip.source_offset / clip.source_duration).clamp(0.0, 1.0);
        let end = ((clip.source_offset + clip.length) / clip.source_duration).clamp(begin, 1.0);
        if begin > 0.000_001 {
            expr.push_str(&format!(".begin({})", fmt_f64(begin)));
        }
        if end < 0.999_999 {
            expr.push_str(&format!(".end({})", fmt_f64(end)));
        }
    }
    expr.push_str(&format!(".clip({})", fmt_f64(clip.length)));
    if clip.timeline_position > 0.000_001 {
        expr.push_str(&format!(".late({})", fmt_f64(clip.timeline_position)));
    }
    if (clip.gain - 1.0).abs() > f32::EPSILON {
        expr.push_str(&format!(".gain({})", fmt_f32(clip.gain)));
    }
    if clip.pan.abs() > f32::EPSILON {
        expr.push_str(&format!(".pan({})", fmt_f32(clip.pan)));
    }
    expr
}

pub fn source_label(source_id: &StableId) -> String {
    source_id
        .as_str()
        .strip_prefix("src_")
        .map(|suffix| format!("sample_{suffix}"))
        .unwrap_or_else(|| source_id.as_str().to_string())
}

pub fn fmt_f64(value: f64) -> String {
    let rounded = if value.abs() < 0.000_000_1 {
        0.0
    } else {
        value
    };
    let mut text = format!("{rounded:.6}");
    trim_float(&mut text);
    text
}

pub fn fmt_f32(value: f32) -> String {
    let rounded = if value.abs() < 0.000_001 { 0.0 } else { value };
    let mut text = format!("{rounded:.4}");
    trim_float(&mut text);
    text
}

fn trim_float(text: &mut String) {
    while text.contains('.') && text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
}

fn escape_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrangement_ir::ArrangementIr;

    #[test]
    fn codegen_is_deterministic_for_same_ir() {
        let ir = ArrangementIr::empty();
        assert_eq!(
            generate_arrangement_code(&ir),
            generate_arrangement_code(&ir)
        );
    }
}
