use crate::arrangement_ir::SourceSpan;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeEdit {
    pub span: SourceSpan,
    pub replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodePatch {
    pub edits: Vec<CodeEdit>,
}

impl CodePatch {
    pub fn new(edits: Vec<CodeEdit>) -> Self {
        Self {
            edits: merge_adjacent_edits(edits),
        }
    }

    pub fn empty() -> Self {
        Self { edits: Vec::new() }
    }

    pub fn replace_span(span: SourceSpan, replacement: impl Into<String>) -> Self {
        Self::new(vec![CodeEdit {
            span,
            replacement: replacement.into(),
        }])
    }

    pub fn insert_at(offset: usize, text: impl Into<String>) -> Self {
        Self::replace_span(SourceSpan::new(offset, offset), text)
    }

    pub fn apply(&self, code: &str) -> Result<String, String> {
        let mut edits = self.edits.clone();
        edits.sort_by_key(|edit| edit.span.start);
        for pair in edits.windows(2) {
            if pair[0].span.end > pair[1].span.start {
                return Err("overlapping code edits".to_string());
            }
        }

        let mut output = code.to_string();
        for edit in edits.iter().rev() {
            if edit.span.start > edit.span.end || edit.span.end > output.len() {
                return Err("code edit span is out of bounds".to_string());
            }
            output.replace_range(edit.span.start..edit.span.end, &edit.replacement);
        }
        Ok(output)
    }

    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }
}

pub fn merge_adjacent_edits(mut edits: Vec<CodeEdit>) -> Vec<CodeEdit> {
    if edits.len() <= 1 {
        return edits;
    }
    edits.sort_by_key(|edit| edit.span.start);
    let mut merged: Vec<CodeEdit> = Vec::with_capacity(edits.len());
    for edit in edits {
        if let Some(last) = merged.last_mut() {
            if last.span.end == edit.span.start {
                last.span.end = edit.span.end;
                last.replacement.push_str(&edit.replacement);
                continue;
            }
        }
        merged.push(edit);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_edits_from_end_to_start() {
        let patch = CodePatch::new(vec![
            CodeEdit {
                span: SourceSpan::new(0, 1),
                replacement: "A".to_string(),
            },
            CodeEdit {
                span: SourceSpan::new(2, 3),
                replacement: "C".to_string(),
            },
        ]);
        assert_eq!(patch.apply("abc").unwrap(), "AbC");
    }
}
