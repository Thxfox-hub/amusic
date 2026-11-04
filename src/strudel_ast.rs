use std::collections::HashMap;

use crate::arrangement_ir::{SourceSpan, StableId};

const META_PREFIX: &str = "// @struidel";

#[derive(Debug, Clone, Default)]
pub struct StrudelAstDoc {
    pub code: String,
    pub managed_nodes: Vec<ManagedNode>,
    pub raw_regions: Vec<SourceSpan>,
}

#[derive(Debug, Clone)]
pub struct ManagedNode {
    pub kind: ManagedNodeKind,
    pub id: StableId,
    pub attrs: HashMap<String, String>,
    pub metadata_span: SourceSpan,
    pub body_span: Option<SourceSpan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedNodeKind {
    Arrangement,
    Track,
    Clip,
    Bus,
    Automation,
    Unknown,
}

impl ManagedNodeKind {
    fn from_str(value: &str) -> Self {
        match value {
            "arrangement" => Self::Arrangement,
            "track" => Self::Track,
            "clip" => Self::Clip,
            "bus" => Self::Bus,
            "automation" => Self::Automation,
            _ => Self::Unknown,
        }
    }
}

impl StrudelAstDoc {
    pub fn parse(code: impl Into<String>) -> Self {
        let code = code.into();
        let mut managed_nodes = Vec::new();
        let mut line_starts = Vec::new();
        let mut offset = 0usize;
        for line in code.split_inclusive('\n') {
            line_starts.push((offset, line));
            offset += line.len();
        }
        if code.is_empty() || !code.ends_with('\n') {
            line_starts.push((offset, ""));
        }

        for index in 0..line_starts.len() {
            let (line_start, line) = line_starts[index];
            let trimmed = line.trim_start();
            if !trimmed.starts_with(META_PREFIX) {
                continue;
            }
            let trim_offset = line.len() - trimmed.len();
            let metadata_start = line_start + trim_offset;
            let metadata_end = line_start + line.len();
            let payload = trimmed[META_PREFIX.len()..].trim();
            let (kind, attrs) = parse_metadata_payload(payload);
            let id = attrs
                .get("id")
                .cloned()
                .unwrap_or_else(|| format!("unknown_{}", managed_nodes.len() + 1));
            let body_span = next_code_line_span(&line_starts, index + 1);
            managed_nodes.push(ManagedNode {
                kind,
                id: StableId::from(id),
                attrs,
                metadata_span: SourceSpan::new(metadata_start, metadata_end),
                body_span,
            });
        }

        let raw_regions = raw_regions_for(&code, &managed_nodes);
        Self {
            code,
            managed_nodes,
            raw_regions,
        }
    }

    pub fn node_by_id(&self, id: &StableId) -> Option<&ManagedNode> {
        self.managed_nodes.iter().find(|node| &node.id == id)
    }

    pub fn clip_nodes(&self) -> impl Iterator<Item = &ManagedNode> {
        self.managed_nodes
            .iter()
            .filter(|node| node.kind == ManagedNodeKind::Clip)
    }
}

fn parse_metadata_payload(payload: &str) -> (ManagedNodeKind, HashMap<String, String>) {
    let mut parts = payload.split_whitespace();
    let kind = parts
        .next()
        .map(ManagedNodeKind::from_str)
        .unwrap_or(ManagedNodeKind::Unknown);
    let mut attrs = HashMap::new();
    for part in parts {
        if let Some((key, value)) = part.split_once('=') {
            attrs.insert(key.to_string(), unquote(value));
        }
    }
    (kind, attrs)
}

fn unquote(value: &str) -> String {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
        .unwrap_or(value)
        .to_string()
}

fn next_code_line_span(lines: &[(usize, &str)], start_index: usize) -> Option<SourceSpan> {
    for (line_start, line) in lines.iter().skip(start_index) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with(META_PREFIX) {
            return None;
        }
        return Some(SourceSpan::new(*line_start, *line_start + line.len()));
    }
    None
}

fn raw_regions_for(code: &str, nodes: &[ManagedNode]) -> Vec<SourceSpan> {
    let mut managed = Vec::new();
    for node in nodes {
        let start = node.metadata_span.start;
        let end = node
            .body_span
            .map(|span| span.end)
            .unwrap_or(node.metadata_span.end);
        managed.push(SourceSpan::new(start, end));
    }
    managed.sort_by_key(|span| span.start);

    let mut raw = Vec::new();
    let mut cursor = 0usize;
    for span in managed {
        if cursor < span.start {
            raw.push(SourceSpan::new(cursor, span.start));
        }
        cursor = cursor.max(span.end);
    }
    if cursor < code.len() {
        raw.push(SourceSpan::new(cursor, code.len()));
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_managed_clip_with_body_span() {
        let code =
            "// intro\n// @struidel clip id=clip_7 track=trk_1 start=0 length=1\ns(\"bd\")\n";
        let doc = StrudelAstDoc::parse(code);
        let node = doc.node_by_id(&StableId::from("clip_7")).unwrap();
        assert_eq!(node.kind, ManagedNodeKind::Clip);
        assert_eq!(node.attrs.get("track").map(String::as_str), Some("trk_1"));
        assert_eq!(
            &doc.code[node.body_span.unwrap().start..node.body_span.unwrap().end],
            "s(\"bd\")\n"
        );
        assert_eq!(doc.raw_regions.len(), 1);
    }
}
