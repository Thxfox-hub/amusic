use crate::arrangement_ir::{SourceSpan, StableId};
use crate::mini_notation::{flatten_events, parse_pattern};
use crate::pattern_ir::{
    span_id, DiagnosticSeverity, Editability, PatternChainValue, PatternDiagnostic,
    PatternDocument, PatternEvent, PatternExpression, PatternIr, PatternModifier, PatternNode,
    PatternTrack, PatternValue, SourceMap, StrudelAst,
};

pub fn parse_pattern_document(code: impl Into<String>, revision: u64) -> PatternDocument {
    let code = code.into();
    let (ast, mut diagnostics) = scan_strudel_expressions(&code);
    let mut ir = PatternIr::default();
    let mut source_map = SourceMap::default();

    for expr in &ast.expressions {
        let mini = &code[expr.mini_span.start..expr.mini_span.end];
        let mini_ast = parse_pattern(mini, expr.mini_span.start);
        diagnostics.extend(mini_ast.diagnostics.iter().map(|diag| PatternDiagnostic {
            span: diag.span,
            message: diag.message.clone(),
            severity: DiagnosticSeverity::Warning,
        }));

        let pattern_id = expr.id.clone();
        source_map
            .pattern_sources
            .insert(pattern_id.clone(), expr.source_span);
        ir.patterns.push(PatternNode {
            id: pattern_id.clone(),
            source_span: expr.source_span,
            source_fn: expr.source_fn.clone(),
            bank: expr.bank.as_ref().map(|bank| bank.value.clone()),
            editable: expr.editable.clone(),
        });

        for event in flatten_events(&mini_ast, 0.0, 1.0) {
            let value = if event.rest {
                PatternValue::Rest
            } else if expr.source_fn == "note" || expr.source_fn == "n" {
                PatternValue::Note(sample_name(&event.token).to_string())
            } else {
                PatternValue::Sample(sample_name(&event.token).to_string())
            };
            let track_title = match &value {
                PatternValue::Sample(value) | PatternValue::Note(value) => value.clone(),
                PatternValue::Rest => "~".to_string(),
                PatternValue::Unknown(value) => value.clone(),
            };
            let track_id = span_id(
                "ptrk",
                expr.source_span,
                format!(
                    "{}_{}",
                    expr.bank
                        .as_ref()
                        .map(|bank| bank.value.as_str())
                        .unwrap_or("none"),
                    track_title
                ),
            );
            if !ir
                .tracks
                .iter()
                .any(|track| track.id == track_id && track.pattern_id == pattern_id)
            {
                ir.tracks.push(PatternTrack {
                    id: track_id.clone(),
                    pattern_id: pattern_id.clone(),
                    title: track_title,
                    bank: expr.bank.as_ref().map(|bank| bank.value.clone()),
                });
            }

            let editable = if expr.editable.is_editable() && event.editable {
                Editability::Editable
            } else {
                Editability::ReadOnly(
                    expr.editable
                        .reason()
                        .unwrap_or("cannot patch this pattern yet")
                        .to_string(),
                )
            };
            let event_id = span_id(
                "pevt",
                event.span,
                format!(
                    "{}_{}_{}_{}",
                    pattern_id.as_str(),
                    event.token,
                    event.begin,
                    event.end
                ),
            );
            source_map
                .event_sources
                .insert(event_id.clone(), event.span);
            ir.events.push(PatternEvent {
                id: event_id,
                pattern_id: pattern_id.clone(),
                track_id,
                source_span: expr.source_span,
                token_span: Some(event.span),
                begin: event.begin,
                end: event.end,
                value,
                modifiers: token_modifiers(&event.token, event.span),
                editable,
            });
        }
    }

    if ast.expressions.is_empty() && !code.trim().is_empty() {
        diagnostics.push(PatternDiagnostic {
            span: SourceSpan::new(0, code.len()),
            message: "No editable sound(\"...\") or s(\"...\") Strudel patterns found".to_string(),
            severity: DiagnosticSeverity::Info,
        });
    }

    PatternDocument {
        revision,
        code,
        ast,
        ir,
        source_map,
        diagnostics,
    }
}

pub fn scan_strudel_expressions(code: &str) -> (StrudelAst, Vec<PatternDiagnostic>) {
    let mut expressions = Vec::new();
    let mut diagnostics = Vec::new();
    let mut cursor = 0usize;
    while cursor < code.len() {
        let Some((call_start, source_fn)) = find_next_source_call(code, cursor) else {
            break;
        };
        let Some(open_paren) = next_non_ws_is(code, call_start + source_fn.len(), '(') else {
            cursor = call_start + source_fn.len();
            continue;
        };
        let Some((quote_start, quote)) = first_string_after_open(code, open_paren) else {
            diagnostics.push(PatternDiagnostic {
                span: SourceSpan::new(call_start, open_paren + 1),
                message: format!(
                    "{source_fn}(...) is read-only until its first argument is a string literal"
                ),
                severity: DiagnosticSeverity::Warning,
            });
            cursor = open_paren + 1;
            continue;
        };
        if !code[open_paren + 1..quote_start].trim().is_empty() {
            cursor = open_paren + 1;
            continue;
        }
        let Some(quote_end) = find_quote_end(code, quote_start, quote) else {
            diagnostics.push(PatternDiagnostic {
                span: SourceSpan::new(quote_start, code.len()),
                message: "Unclosed Strudel mini-notation string".to_string(),
                severity: DiagnosticSeverity::Error,
            });
            break;
        };
        let Some(close_paren) = next_non_ws_is(code, quote_end + quote.len_utf8(), ')') else {
            diagnostics.push(PatternDiagnostic {
                span: SourceSpan::new(call_start, quote_end + quote.len_utf8()),
                message: "Only single-string source calls are editable in the timeline".to_string(),
                severity: DiagnosticSeverity::Warning,
            });
            cursor = quote_end + quote.len_utf8();
            continue;
        };

        let mut expr_end = close_paren + 1;
        let bank = parse_bank_chain(code, expr_end);
        if let Some(bank) = &bank {
            expr_end = bank.call_span.end;
        }
        let source_span = SourceSpan::new(call_start, expr_end);
        let id = span_id(
            "pat",
            source_span,
            format!("{}_{}", source_fn, expressions.len()),
        );
        expressions.push(PatternExpression {
            id,
            source_span,
            source_fn: source_fn.to_string(),
            mini_span: SourceSpan::new(quote_start + quote.len_utf8(), quote_end),
            quote,
            bank,
            editable: Editability::Editable,
        });
        cursor = expr_end;
    }
    (StrudelAst { expressions }, diagnostics)
}

fn find_next_source_call(code: &str, from: usize) -> Option<(usize, &'static str)> {
    let mut best: Option<(usize, &'static str)> = None;
    for name in ["sound", "s", "note", "n"] {
        let mut cursor = from;
        while let Some(rel) = code[cursor..].find(name) {
            let start = cursor + rel;
            let end = start + name.len();
            if is_identifier_boundary(code, start, end) {
                if best
                    .map(|(best_start, _)| start < best_start)
                    .unwrap_or(true)
                {
                    best = Some((start, name));
                }
                break;
            }
            cursor = end;
        }
    }
    best
}

fn is_identifier_boundary(code: &str, start: usize, end: usize) -> bool {
    let before = code[..start].chars().next_back();
    let after = code[end..].chars().next();
    !before
        .map(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '.')
        .unwrap_or(false)
        && !after
            .map(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            .unwrap_or(false)
}

fn next_non_ws_is(code: &str, from: usize, expected: char) -> Option<usize> {
    for (idx, ch) in code[from..].char_indices() {
        if ch.is_whitespace() {
            continue;
        }
        return (ch == expected).then_some(from + idx);
    }
    None
}

fn first_string_after_open(code: &str, open: usize) -> Option<(usize, char)> {
    for (idx, ch) in code[open + 1..].char_indices() {
        if ch.is_whitespace() {
            continue;
        }
        if matches!(ch, '"' | '\'' | '`') {
            return Some((open + 1 + idx, ch));
        }
        return None;
    }
    None
}

fn parse_bank_chain(code: &str, from: usize) -> Option<PatternChainValue> {
    let dot = next_non_ws_is(code, from, '.')?;
    let name_start = dot + 1;
    if !code[name_start..].starts_with("bank") {
        return None;
    }
    let name_end = name_start + "bank".len();
    if code[name_end..]
        .chars()
        .next()
        .map(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        .unwrap_or(false)
    {
        return None;
    }
    let open = next_non_ws_is(code, name_end, '(')?;
    let (quote_start, quote) = first_string_after_open(code, open)?;
    if !code[open + 1..quote_start].trim().is_empty() {
        return None;
    }
    let quote_end = find_quote_end(code, quote_start, quote)?;
    let close = next_non_ws_is(code, quote_end + quote.len_utf8(), ')')?;
    Some(PatternChainValue {
        value: code[quote_start + quote.len_utf8()..quote_end].to_string(),
        call_span: SourceSpan::new(dot, close + 1),
        value_span: SourceSpan::new(quote_start + quote.len_utf8(), quote_end),
        quote,
    })
}

pub fn find_quote_end(code: &str, quote_start: usize, quote: char) -> Option<usize> {
    let mut escaped = false;
    for (idx, ch) in code[quote_start + quote.len_utf8()..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            return Some(quote_start + quote.len_utf8() + idx);
        }
    }
    None
}

fn sample_name(token: &str) -> &str {
    token
        .split(|ch| matches!(ch, ':' | '/' | '@' | '?' | '!'))
        .next()
        .unwrap_or(token)
}

fn token_modifiers(token: &str, span: SourceSpan) -> Vec<PatternModifier> {
    let mut modifiers = Vec::new();
    for marker in [':', '/', '@', '?', '!'] {
        if let Some(index) = token.find(marker) {
            modifiers.push(PatternModifier {
                name: marker.to_string(),
                value: token.get(index + marker.len_utf8()..).map(str::to_string),
                source_span: SourceSpan::new(span.start + index, span.end),
            });
        }
    }
    modifiers
}

pub fn stable_event_lookup<'a>(
    document: &'a PatternDocument,
    event_id: &StableId,
    token_value: Option<&str>,
) -> Option<&'a PatternEvent> {
    document.ir.events.iter().find(|event| {
        &event.id == event_id
            || (token_value
                .zip(event.sample())
                .map(|(expected, actual)| expected == actual)
                .unwrap_or(false)
                && event.token_span.is_some())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sound_bank_to_events() {
        let doc = parse_pattern_document(r#"sound("bd hh sd oh").bank("RolandTR909")"#, 1);
        assert_eq!(doc.ir.patterns.len(), 1);
        assert_eq!(doc.ir.events.len(), 4);
        assert_eq!(doc.ir.events[1].sample(), Some("hh"));
        assert_eq!(doc.ir.events[1].begin, 0.25);
        assert_eq!(doc.ir.patterns[0].bank.as_deref(), Some("RolandTR909"));
    }

    #[test]
    fn replicated_events_get_unique_ids() {
        let doc = parse_pattern_document(r#"s("bd*4")"#, 1);
        let ids: Vec<_> = doc.ir.events.iter().map(|event| event.id.clone()).collect();
        assert_eq!(ids.len(), 4);
        assert_eq!(ids.iter().collect::<std::collections::BTreeSet<_>>().len(), 4);
    }

    #[test]
    fn parses_stack_children() {
        let doc = parse_pattern_document("stack(\n  s(\"bd ~ sd ~\"),\n  sound(\"hh*4\")\n)", 1);
        assert_eq!(doc.ir.patterns.len(), 2);
        assert_eq!(
            doc.ir
                .events
                .iter()
                .filter(|event| !matches!(event.value, PatternValue::Rest))
                .count(),
            6
        );
    }
}
