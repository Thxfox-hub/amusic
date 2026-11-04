use crate::arrangement_ir::SourceSpan;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiniTokenKind {
    Word,
    Number,
    GroupOpen,
    GroupClose,
    AltOpen,
    AltClose,
    Separator,
    Multiply,
    Modifier,
    Whitespace,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MiniToken {
    pub kind: MiniTokenKind,
    pub text: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MiniContext {
    pub depth_group: usize,
    pub depth_alt: usize,
    pub last_kind: Option<MiniTokenKind>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MiniPattern {
    pub items: Vec<MiniItem>,
    pub diagnostics: Vec<MiniDiagnostic>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MiniItem {
    pub kind: MiniItemKind,
    pub span: SourceSpan,
    pub speed: f64,
    pub weight: f64,
    pub replicate: usize,
    pub editable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MiniItemKind {
    Token(String),
    Rest,
    Group(Vec<MiniItem>),
    Parallel(Vec<Vec<MiniItem>>),
    Alternation(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MiniDiagnostic {
    pub span: SourceSpan,
    pub message: String,
}

pub fn tokenize(input: &str) -> Vec<MiniToken> {
    let mut out = Vec::new();
    let mut it = input.char_indices().peekable();
    while let Some((start, ch)) = it.next() {
        let mut end = it.peek().map(|(idx, _)| *idx).unwrap_or(input.len());
        let kind = match ch {
            '[' => MiniTokenKind::GroupOpen,
            ']' => MiniTokenKind::GroupClose,
            '<' => MiniTokenKind::AltOpen,
            '>' => MiniTokenKind::AltClose,
            ',' => MiniTokenKind::Separator,
            '*' => {
                while let Some((idx, c)) = it.peek().copied() {
                    if c.is_ascii_digit() || c == '.' {
                        end = idx + c.len_utf8();
                        let _ = it.next();
                    } else {
                        break;
                    }
                }
                MiniTokenKind::Multiply
            }
            '?' | '!' | '~' | '-' | '@' | '/' => MiniTokenKind::Modifier,
            _ if ch.is_whitespace() => {
                while let Some((idx, c)) = it.peek().copied() {
                    if c.is_whitespace() {
                        end = idx + c.len_utf8();
                        let _ = it.next();
                    } else {
                        break;
                    }
                }
                MiniTokenKind::Whitespace
            }
            _ if ch.is_ascii_digit() => {
                while let Some((idx, c)) = it.peek().copied() {
                    if c.is_ascii_digit() {
                        end = idx + c.len_utf8();
                        let _ = it.next();
                    } else {
                        break;
                    }
                }
                MiniTokenKind::Number
            }
            _ if ch.is_ascii_alphabetic() || ch == '_' => {
                while let Some((idx, c)) = it.peek().copied() {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        end = idx + c.len_utf8();
                        let _ = it.next();
                    } else {
                        break;
                    }
                }
                MiniTokenKind::Word
            }
            _ => MiniTokenKind::Other,
        };
        out.push(MiniToken {
            kind,
            text: input[start..end].to_string(),
            start,
            end,
        });
    }
    out
}

pub fn parse_pattern(input: &str, source_offset: usize) -> MiniPattern {
    let mut parser = MiniParser {
        input,
        source_offset,
        cursor: 0,
        diagnostics: Vec::new(),
    };
    let items = parser.parse_items(None);
    MiniPattern {
        items,
        diagnostics: parser.diagnostics,
    }
}

pub fn flatten_events(pattern: &MiniPattern, cycle_start: f64, cycle_len: f64) -> Vec<MiniEvent> {
    let mut events = Vec::new();
    flatten_items(&pattern.items, cycle_start, cycle_len, &mut events);
    events
}

#[derive(Debug, Clone, PartialEq)]
pub struct MiniEvent {
    pub token: String,
    pub span: SourceSpan,
    pub begin: f64,
    pub end: f64,
    pub rest: bool,
    pub editable: bool,
}

fn flatten_items(items: &[MiniItem], begin: f64, len: f64, out: &mut Vec<MiniEvent>) {
    let total_weight: f64 = items
        .iter()
        .map(|item| item.weight.max(0.0) * item.replicate.max(1) as f64)
        .sum::<f64>()
        .max(1.0);
    let mut cursor = begin;
    for item in items {
        for _ in 0..item.replicate.max(1) {
            let slot_len = len * item.weight.max(0.0) / total_weight;
            flatten_item(item, cursor, slot_len, out);
            cursor += slot_len;
        }
    }
}

fn flatten_item(item: &MiniItem, begin: f64, len: f64, out: &mut Vec<MiniEvent>) {
    let repeat = item.speed.max(0.000_001).ceil() as usize;
    for repeat_index in 0..repeat {
        let repeat_begin = begin + len * repeat_index as f64 / item.speed.max(0.000_001);
        let repeat_len = len / item.speed.max(0.000_001);
        if repeat_begin >= begin + len {
            break;
        }
        match &item.kind {
            MiniItemKind::Token(token) => out.push(MiniEvent {
                token: token.clone(),
                span: item.span,
                begin: repeat_begin,
                end: (repeat_begin + repeat_len).min(begin + len),
                rest: false,
                editable: item.editable,
            }),
            MiniItemKind::Rest => out.push(MiniEvent {
                token: "~".to_string(),
                span: item.span,
                begin: repeat_begin,
                end: repeat_begin + repeat_len,
                rest: true,
                editable: item.editable,
            }),
            MiniItemKind::Group(items) => flatten_items(items, repeat_begin, repeat_len, out),
            MiniItemKind::Parallel(branches) => {
                for branch in branches {
                    flatten_items(branch, repeat_begin, repeat_len, out);
                }
            }
            MiniItemKind::Alternation(text) => out.push(MiniEvent {
                token: text.clone(),
                span: item.span,
                begin: repeat_begin,
                end: repeat_begin + repeat_len,
                rest: false,
                editable: false,
            }),
        }
    }
}

struct MiniParser<'a> {
    input: &'a str,
    source_offset: usize,
    cursor: usize,
    diagnostics: Vec<MiniDiagnostic>,
}

impl<'a> MiniParser<'a> {
    fn parse_items(&mut self, terminator: Option<char>) -> Vec<MiniItem> {
        let branch_start = self.cursor;
        let mut branches: Vec<Vec<MiniItem>> = Vec::new();
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            if self.cursor >= self.input.len() {
                if let Some(ch) = terminator {
                    self.diagnostics.push(MiniDiagnostic {
                        span: self.span(self.cursor, self.cursor),
                        message: format!("missing closing `{ch}`"),
                    });
                }
                break;
            }
            if self.peek_char() == Some(',') {
                self.cursor += 1;
                branches.push(items);
                items = Vec::new();
                continue;
            }
            if let Some(ch) = terminator {
                if self.peek_char() == Some(ch) {
                    self.cursor += ch.len_utf8();
                    break;
                }
            }
            if matches!(self.peek_char(), Some(']') | Some('>')) {
                let ch = self.peek_char().unwrap();
                self.diagnostics.push(MiniDiagnostic {
                    span: self.span(self.cursor, self.cursor + ch.len_utf8()),
                    message: format!("unexpected closing `{ch}`"),
                });
                self.cursor += ch.len_utf8();
                continue;
            }
            if let Some(item) = self.parse_item() {
                items.push(item);
            } else {
                break;
            }
        }
        if branches.is_empty() {
            items
        } else {
            branches.push(items);
            vec![MiniItem {
                kind: MiniItemKind::Parallel(branches),
                span: self.span(branch_start, self.cursor),
                speed: 1.0,
                weight: 1.0,
                replicate: 1,
                editable: true,
            }]
        }
    }

    fn parse_item(&mut self) -> Option<MiniItem> {
        let start = self.cursor;
        let ch = self.peek_char()?;
        let mut item = match ch {
            '[' => {
                self.cursor += 1;
                let children = self.parse_items(Some(']'));
                MiniItem {
                    kind: MiniItemKind::Group(children),
                    span: self.span(start, self.cursor),
                    speed: 1.0,
                    weight: 1.0,
                    replicate: 1,
                    editable: true,
                }
            }
            '<' => {
                self.cursor += 1;
                while self.cursor < self.input.len() && self.peek_char() != Some('>') {
                    self.advance_char();
                }
                if self.peek_char() == Some('>') {
                    self.cursor += 1;
                } else {
                    self.diagnostics.push(MiniDiagnostic {
                        span: self.span(start, self.cursor),
                        message: "missing closing `>`".to_string(),
                    });
                }
                MiniItem {
                    kind: MiniItemKind::Alternation(self.input[start..self.cursor].to_string()),
                    span: self.span(start, self.cursor),
                    speed: 1.0,
                    weight: 1.0,
                    replicate: 1,
                    editable: false,
                }
            }
            '~' | '-' => {
                self.cursor += 1;
                MiniItem {
                    kind: MiniItemKind::Rest,
                    span: self.span(start, self.cursor),
                    speed: 1.0,
                    weight: 1.0,
                    replicate: 1,
                    editable: true,
                }
            }
            _ => {
                while self.cursor < self.input.len() {
                    let Some(next) = self.peek_char() else {
                        break;
                    };
                    if next.is_whitespace()
                        || matches!(
                            next,
                            '[' | ']' | '<' | '>' | '*' | ',' | '@' | '/' | '!' | '?' | '|'
                        )
                    {
                        break;
                    }
                    self.advance_char();
                }
                if self.cursor == start {
                    self.advance_char();
                }
                MiniItem {
                    kind: MiniItemKind::Token(self.input[start..self.cursor].to_string()),
                    span: self.span(start, self.cursor),
                    speed: 1.0,
                    weight: 1.0,
                    replicate: 1,
                    editable: true,
                }
            }
        };
        self.parse_postfix_modifiers(&mut item);
        Some(item)
    }

    fn parse_postfix_modifiers(&mut self, item: &mut MiniItem) {
        loop {
            match self.peek_char() {
                Some('*') => {
                    self.cursor += 1;
                    if let Some(value) = self.parse_number() {
                        item.speed *= value.max(0.000_001);
                        item.span.end = self.source_offset + self.cursor;
                    }
                }
                Some('/') => {
                    self.cursor += 1;
                    if let Some(value) = self.parse_number() {
                        item.speed /= value.max(0.000_001);
                        item.span.end = self.source_offset + self.cursor;
                    }
                }
                Some('@') => {
                    self.cursor += 1;
                    if let Some(value) = self.parse_number() {
                        item.weight = value.max(0.000_001);
                        item.span.end = self.source_offset + self.cursor;
                    }
                }
                Some('!') => {
                    self.cursor += 1;
                    if let Some(value) = self.parse_number() {
                        item.replicate = value.round().max(1.0) as usize;
                        item.span.end = self.source_offset + self.cursor;
                    }
                }
                Some('?') => {
                    self.cursor += 1;
                    let _ = self.parse_number();
                    item.editable = false;
                    item.span.end = self.source_offset + self.cursor;
                    self.diagnostics.push(MiniDiagnostic {
                        span: item.span,
                        message: "probability modifiers are read-only in timeline edits"
                            .to_string(),
                    });
                }
                Some('(') => {
                    self.consume_balanced_parens();
                    item.editable = false;
                    item.span.end = self.source_offset + self.cursor;
                    self.diagnostics.push(MiniDiagnostic {
                        span: item.span,
                        message: "euclidean rhythms are read-only in timeline edits".to_string(),
                    });
                }
                _ => break,
            }
        }
    }

    fn parse_number(&mut self) -> Option<f64> {
        let start = self.cursor;
        let mut seen_digit = false;
        while let Some(ch) = self.peek_char() {
            if ch.is_ascii_digit() {
                seen_digit = true;
                self.cursor += ch.len_utf8();
            } else if ch == '.' {
                self.cursor += ch.len_utf8();
            } else {
                break;
            }
        }
        if seen_digit {
            self.input[start..self.cursor].parse().ok()
        } else {
            None
        }
    }

    fn consume_balanced_parens(&mut self) {
        if self.peek_char() != Some('(') {
            return;
        }
        let mut depth = 0usize;
        while let Some(ch) = self.peek_char() {
            self.cursor += ch.len_utf8();
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
    }

    fn skip_ws(&mut self) {
        while self
            .peek_char()
            .map(|ch| ch.is_whitespace())
            .unwrap_or(false)
        {
            self.advance_char();
        }
    }

    fn advance_char(&mut self) {
        if let Some(ch) = self.peek_char() {
            self.cursor += ch.len_utf8();
        }
    }

    fn peek_char(&self) -> Option<char> {
        self.input[self.cursor..].chars().next()
    }

    fn span(&self, start: usize, end: usize) -> SourceSpan {
        SourceSpan::new(self.source_offset + start, self.source_offset + end)
    }
}

pub fn context_at(input: &str, cursor_byte_inside: usize) -> MiniContext {
    let mut depth_group = 0usize;
    let mut depth_alt = 0usize;
    let mut last_kind = None;
    for t in tokenize(input) {
        if t.start >= cursor_byte_inside {
            break;
        }
        match t.kind {
            MiniTokenKind::GroupOpen => depth_group += 1,
            MiniTokenKind::GroupClose => depth_group = depth_group.saturating_sub(1),
            MiniTokenKind::AltOpen => depth_alt += 1,
            MiniTokenKind::AltClose => depth_alt = depth_alt.saturating_sub(1),
            _ => {}
        }
        if !matches!(t.kind, MiniTokenKind::Whitespace) {
            last_kind = Some(t.kind);
        }
    }
    MiniContext {
        depth_group,
        depth_alt,
        last_kind,
    }
}

pub fn completion_separators_for_context(ctx: &MiniContext) -> Vec<&'static str> {
    let mut out = Vec::new();
    if ctx.depth_group > 0 || ctx.depth_alt > 0 {
        out.push(",");
    }
    if ctx.depth_group > 0 {
        out.push("]");
    }
    if ctx.depth_alt > 0 {
        out.push(">");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_tracks_nested_groups() {
        let tokens = tokenize("[bd <sd,hh>*2]");
        assert!(tokens.iter().any(|t| t.kind == MiniTokenKind::GroupOpen));
        assert!(tokens.iter().any(|t| t.kind == MiniTokenKind::AltOpen));
        assert!(tokens.iter().any(|t| t.kind == MiniTokenKind::Multiply));
    }

    #[test]
    fn context_detects_depth_and_separator() {
        let src = "[bd <sd,hh";
        let ctx = context_at(src, src.len());
        assert_eq!(ctx.depth_group, 1);
        assert_eq!(ctx.depth_alt, 1);
        let seps = completion_separators_for_context(&ctx);
        assert!(seps.contains(&","));
        assert!(seps.contains(&"]"));
        assert!(seps.contains(&">"));
    }

    #[test]
    fn tokenizer_handles_unicode_without_panic() {
        let tokens = tokenize("ф");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].text, "ф");
    }

    #[test]
    fn comma_creates_parallel_branches() {
        let pattern = parse_pattern("hh hh hh, bd casio", 0);
        let events = flatten_events(&pattern, 0.0, 1.0);
        let hh: Vec<_> = events.iter().filter(|event| event.token == "hh").collect();
        let bd: Vec<_> = events.iter().filter(|event| event.token == "bd").collect();
        let casio: Vec<_> = events
            .iter()
            .filter(|event| event.token == "casio")
            .collect();
        assert_eq!(hh.len(), 3);
        assert_eq!(bd.len(), 1);
        assert_eq!(casio.len(), 1);
        assert!((hh[1].begin - 1.0 / 3.0).abs() < 0.000_001);
        assert!((casio[0].begin - 0.5).abs() < 0.000_001);
    }

    #[test]
    fn dash_is_rest() {
        let pattern = parse_pattern("bd - hh", 0);
        let events = flatten_events(&pattern, 0.0, 1.0);
        assert!(events[1].rest);
    }

    #[test]
    fn elongation_weights_sequence_children() {
        let pattern = parse_pattern("a@2 b c", 0);
        let events = flatten_events(&pattern, 0.0, 1.0);
        assert!((events[0].begin - 0.0).abs() < 0.000_001);
        assert!((events[0].end - 0.5).abs() < 0.000_001);
        assert!((events[1].begin - 0.5).abs() < 0.000_001);
        assert!((events[1].end - 0.75).abs() < 0.000_001);
    }

    #[test]
    fn decimal_multiplication_speeds_item_up() {
        let pattern = parse_pattern("[a b]*2.5", 0);
        let events = flatten_events(&pattern, 0.0, 1.0);
        assert!(events.len() >= 5);
        assert!((events[2].begin - 0.4).abs() < 0.000_001);
    }

    #[test]
    fn division_slows_item_down() {
        let pattern = parse_pattern("[a b]/2", 0);
        let events = flatten_events(&pattern, 0.0, 1.0);
        assert!((events[0].end - 1.0).abs() < 0.000_001);
        assert!((events[1].begin - 1.0).abs() < 0.000_001);
    }

    #[test]
    fn replication_repeats_without_speeding_up() {
        let pattern = parse_pattern("a!2 b", 0);
        let events = flatten_events(&pattern, 0.0, 1.0);
        assert_eq!(events.iter().filter(|event| event.token == "a").count(), 2);
        assert!((events[1].begin - 1.0 / 3.0).abs() < 0.000_001);
    }
}
