use std::collections::HashMap;

use crate::arrangement_ir::{SourceSpan, StableId};

#[derive(Debug, Clone)]
pub struct PatternDocument {
    pub revision: u64,
    pub code: String,
    pub ast: StrudelAst,
    pub ir: PatternIr,
    pub source_map: SourceMap,
    pub diagnostics: Vec<PatternDiagnostic>,
}

#[derive(Debug, Clone, Default)]
pub struct StrudelAst {
    pub expressions: Vec<PatternExpression>,
}

#[derive(Debug, Clone)]
pub struct PatternExpression {
    pub id: StableId,
    pub source_span: SourceSpan,
    pub source_fn: String,
    pub mini_span: SourceSpan,
    pub quote: char,
    pub bank: Option<PatternChainValue>,
    pub editable: Editability,
}

#[derive(Debug, Clone)]
pub struct PatternChainValue {
    pub value: String,
    pub call_span: SourceSpan,
    pub value_span: SourceSpan,
    pub quote: char,
}

#[derive(Debug, Clone, Default)]
pub struct PatternIr {
    pub patterns: Vec<PatternNode>,
    pub tracks: Vec<PatternTrack>,
    pub events: Vec<PatternEvent>,
    pub lanes: Vec<AutomationLane>,
}

#[derive(Debug, Clone)]
pub struct PatternNode {
    pub id: StableId,
    pub source_span: SourceSpan,
    pub source_fn: String,
    pub bank: Option<String>,
    pub editable: Editability,
}

#[derive(Debug, Clone)]
pub struct PatternTrack {
    pub id: StableId,
    pub pattern_id: StableId,
    pub title: String,
    pub bank: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PatternEvent {
    pub id: StableId,
    pub pattern_id: StableId,
    pub track_id: StableId,
    pub source_span: SourceSpan,
    pub token_span: Option<SourceSpan>,
    pub begin: f64,
    pub end: f64,
    pub value: PatternValue,
    pub modifiers: Vec<PatternModifier>,
    pub editable: Editability,
}

impl PatternEvent {
    pub fn sample(&self) -> Option<&str> {
        match &self.value {
            PatternValue::Sample(sample) | PatternValue::Note(sample) => Some(sample),
            PatternValue::Rest | PatternValue::Unknown(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AutomationLane {
    pub id: StableId,
    pub pattern_id: StableId,
    pub name: String,
    pub source_span: SourceSpan,
    pub editable: Editability,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PatternValue {
    Sample(String),
    Note(String),
    Rest,
    Unknown(String),
}

#[derive(Debug, Clone)]
pub struct PatternModifier {
    pub name: String,
    pub value: Option<String>,
    pub source_span: SourceSpan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Editability {
    Editable,
    ReadOnly(String),
}

impl Editability {
    pub fn is_editable(&self) -> bool {
        matches!(self, Self::Editable)
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Editable => None,
            Self::ReadOnly(reason) => Some(reason),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SourceMap {
    pub event_sources: HashMap<StableId, SourceSpan>,
    pub pattern_sources: HashMap<StableId, SourceSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternDiagnostic {
    pub span: SourceSpan,
    pub message: String,
    pub severity: DiagnosticSeverity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

pub fn span_id(prefix: &str, span: SourceSpan, salt: impl AsRef<str>) -> StableId {
    let salt = sanitize_id_part(salt.as_ref());
    StableId::from(format!("{prefix}_{}_{}_{}", span.start, span.end, salt))
}

pub fn sanitize_id_part(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if ch == '_' || ch == '-' {
            out.push(ch);
        }
    }
    if out.is_empty() {
        "x".to_string()
    } else {
        out
    }
}
