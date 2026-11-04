use crate::arrangement_ir::{SourceSpan, StableId};
use crate::mini_notation::{
    flatten_events, parse_pattern, tokenize, MiniEvent, MiniItem, MiniItemKind, MiniTokenKind,
};
use crate::pattern_ir::{PatternDocument, PatternEvent, PatternExpression};
use crate::strudel_parser::parse_pattern_document;
use crate::strudel_patch::{CodeEdit, CodePatch};

#[derive(Debug, Clone)]
pub struct PatternEdit {
    pub event_id: Option<StableId>,
    pub pattern_id: StableId,
    pub token_span: Option<SourceSpan>,
    pub token_value: Option<String>,
    pub kind: PatternEditKind,
}

#[derive(Debug, Clone)]
pub enum PatternEditKind {
    DeleteEvent,
    DuplicateEvent,
    InsertRest { at: f64 },
    MuteEvent,
    ReplaceSample(String),
    MoveEvent { new_begin: f64 },
    ResizeEvent { new_end: f64 },
    SplitEvent,
    SplitIntoN { n: usize },
    PasteAfterEvent { sample: String },
    Quantize { grid: f64 },
    ChangeBank(String),
}

#[derive(Debug, Clone)]
pub struct AppliedPatternEdit {
    pub revision: u64,
    pub patch: CodePatch,
    pub code: String,
    pub document: PatternDocument,
}

pub fn apply_pattern_edit(
    code: &str,
    revision: u64,
    edit: PatternEdit,
) -> Result<AppliedPatternEdit, String> {
    let document = parse_pattern_document(code.to_string(), revision);
    let expr = document
        .ast
        .expressions
        .iter()
        .find(|expr| expr.id == edit.pattern_id)
        .ok_or_else(|| "conflict: pattern no longer exists".to_string())?;
    let patch = match &edit.kind {
        PatternEditKind::ChangeBank(bank) => patch_bank(expr, bank),
        PatternEditKind::InsertRest { at } => patch_insert_rest(code, expr, *at),
        _ => {
            let event = find_event(&document, &edit)
                .ok_or_else(|| "conflict: event no longer exists".to_string())?;
            if !event.editable.is_editable() {
                return Err(event
                    .editable
                    .reason()
                    .unwrap_or("cannot patch this pattern yet")
                    .to_string());
            }
            match &edit.kind {
                PatternEditKind::DeleteEvent => patch_delete_token(code, event),
                PatternEditKind::DuplicateEvent => patch_duplicate_token(code, event),
                PatternEditKind::InsertRest { .. } => unreachable!(),
                PatternEditKind::MuteEvent => patch_replace_token(event, "~".to_string()),
                PatternEditKind::ReplaceSample(sample) => patch_replace_sample(code, event, sample),
                PatternEditKind::MoveEvent { new_begin } => {
                    patch_move_token(code, expr, event, *new_begin)
                }
                PatternEditKind::ResizeEvent { new_end } => {
                    patch_resize_token(code, expr, event, *new_end)
                }
                PatternEditKind::SplitEvent => patch_split_into_n(code, event, 2),
                PatternEditKind::SplitIntoN { n } => patch_split_into_n(code, event, *n),
                PatternEditKind::PasteAfterEvent { sample } => {
                    patch_insert_sample_after(code, event, sample)
                }
                PatternEditKind::Quantize { grid } => patch_quantize(code, expr, *grid),
                PatternEditKind::ChangeBank(_) => unreachable!(),
            }
        }
    }?;

    let next_code = patch.apply(code)?;
    let next_revision = revision.saturating_add(1);
    let next_document = parse_pattern_document(next_code.clone(), next_revision);
    Ok(AppliedPatternEdit {
        revision: next_revision,
        patch,
        code: next_code,
        document: next_document,
    })
}

fn find_event<'a>(document: &'a PatternDocument, edit: &PatternEdit) -> Option<&'a PatternEvent> {
    document.ir.events.iter().find(|event| {
        if let Some(event_id) = &edit.event_id {
            if &event.id == event_id {
                return true;
            }
        }
        event.pattern_id == edit.pattern_id
            && event.token_span == edit.token_span
            && edit
                .token_value
                .as_deref()
                .zip(event.sample())
                .map(|(expected, actual)| expected == actual)
                .unwrap_or(true)
    })
}

fn patch_bank(expr: &PatternExpression, bank: &str) -> Result<CodePatch, String> {
    if let Some(existing) = &expr.bank {
        Ok(CodePatch::replace_span(
            existing.value_span,
            escape_string(bank, existing.quote),
        ))
    } else {
        Ok(CodePatch::insert_at(
            expr.source_span.end,
            format!(".bank(\"{}\")", escape_string(bank, '"')),
        ))
    }
}

fn patch_delete_token(code: &str, event: &PatternEvent) -> Result<CodePatch, String> {
    let span = event
        .token_span
        .ok_or_else(|| "cannot patch this pattern yet".to_string())?;
    Ok(CodePatch::replace_span(token_delete_span(code, span), ""))
}

fn patch_duplicate_token(code: &str, event: &PatternEvent) -> Result<CodePatch, String> {
    let span = event
        .token_span
        .ok_or_else(|| "cannot patch this pattern yet".to_string())?;
    let token = &code[span.start..span.end];
    Ok(CodePatch::insert_at(span.end, format!(" {token}")))
}

fn patch_insert_rest(code: &str, expr: &PatternExpression, at: f64) -> Result<CodePatch, String> {
    let tokens = simple_tokens(code, expr)?;
    let index =
        (at.rem_euclid(1.0).clamp(0.0, 0.999_999) * (tokens.len() + 1) as f64).floor() as usize;
    let mut values: Vec<String> = tokens.into_iter().map(|token| token.text).collect();
    values.insert(index.min(values.len()), "~".to_string());
    Ok(CodePatch::replace_span(expr.mini_span, values.join(" ")))
}

fn patch_replace_token(event: &PatternEvent, replacement: String) -> Result<CodePatch, String> {
    let span = event
        .token_span
        .ok_or_else(|| "cannot patch this pattern yet".to_string())?;
    Ok(CodePatch::replace_span(span, replacement))
}

fn patch_replace_sample(
    code: &str,
    event: &PatternEvent,
    sample: &str,
) -> Result<CodePatch, String> {
    let span = event
        .token_span
        .ok_or_else(|| "cannot patch this pattern yet".to_string())?;
    let token = &code[span.start..span.end];
    let suffix_start = token
        .find(|ch| matches!(ch, ':' | '/' | '@' | '?' | '!'))
        .unwrap_or(token.len());
    let replacement = format!("{}{}", sample, &token[suffix_start..]);
    Ok(CodePatch::replace_span(span, replacement))
}

fn patch_move_token(
    code: &str,
    expr: &PatternExpression,
    event: &PatternEvent,
    new_begin: f64,
) -> Result<CodePatch, String> {
    let duration = (event.end - event.begin).max(1.0 / 16.0);
    let new_end = new_begin + duration;
    let steps = minimal_steps(new_begin, new_end);
    patch_piano_roll_event(code, expr, event, new_begin, new_end, steps)
}

fn patch_resize_token(
    code: &str,
    expr: &PatternExpression,
    event: &PatternEvent,
    new_end: f64,
) -> Result<CodePatch, String> {
    let duration = (new_end - event.begin).max(0.0);
    if duration <= 0.0 {
        return patch_delete_token(code, event);
    }
    let steps = minimal_steps(event.begin, new_end);
    patch_piano_roll_event(code, expr, event, event.begin, new_end, steps)
}

/// Минимальное число равных слотов, позволяющее точно выразить begin и end
/// как целые слоты. Значения snap=1/16, поэтому перебираем степени двойки.
fn minimal_steps(begin: f64, end: f64) -> usize {
    for &n in &[1usize, 2, 4, 8, 16] {
        let b = (begin * n as f64).round() as i64;
        let e = (end * n as f64).round() as i64;
        let b_ok = (b as f64 / n as f64 - begin).abs() < 1e-9;
        let e_ok = (e as f64 / n as f64 - end).abs() < 1e-9;
        if b_ok && e_ok && e > b && e <= n as i64 {
            return n;
        }
    }
    16
}

/// Совпадает с `patch_move_token`: не короче одной 1/16, иначе `new_end - new_begin` не совпадёт с «сырой»
/// длительностью события и `try_slot_swap` / `try_reorder` отвалятся в fallback.
fn piano_roll_move_duration(event: &PatternEvent) -> f64 {
    (event.end - event.begin).max(1.0 / 16.0)
}

fn move_interval_matches_piano_roll_drag(
    event: &PatternEvent,
    new_begin: f64,
    new_end: f64,
) -> bool {
    (new_end - new_begin - piano_roll_move_duration(event)).abs() <= 1e-5_f64
}

fn patch_split_into_n(code: &str, event: &PatternEvent, n: usize) -> Result<CodePatch, String> {
    if n < 2 {
        return Err("split count must be at least 2".to_string());
    }
    let span = event
        .token_span
        .ok_or_else(|| "cannot patch this pattern yet".to_string())?;
    let token = &code[span.start..span.end];
    let base = strip_timing_postfix(token);
    let postfix = &token[base.len()..];
    let repeated = (0..n).map(|_| base).collect::<Vec<_>>().join(" ");
    let new_token = if postfix.is_empty() {
        format!("[{repeated}]")
    } else {
        format!("[{repeated}]{postfix}")
    };
    Ok(CodePatch::replace_span(span, new_token))
}

fn patch_insert_sample_after(
    _code: &str,
    event: &PatternEvent,
    sample: &str,
) -> Result<CodePatch, String> {
    let span = event
        .token_span
        .ok_or_else(|| "cannot patch this pattern yet".to_string())?;
    Ok(CodePatch::insert_at(span.end, format!(" {sample}")))
}

fn patch_quantize(code: &str, expr: &PatternExpression, grid: f64) -> Result<CodePatch, String> {
    if grid <= 0.0 {
        return Err("cannot patch this pattern yet".to_string());
    }
    let tokens = simple_tokens(code, expr)?;
    Ok(CodePatch::replace_span(
        expr.mini_span,
        tokens
            .into_iter()
            .map(|token| token.text)
            .collect::<Vec<_>>()
            .join(" "),
    ))
}

fn patch_piano_roll_event(
    code: &str,
    expr: &PatternExpression,
    event: &PatternEvent,
    begin: f64,
    end: f64,
    steps: usize,
) -> Result<CodePatch, String> {
    let span = event
        .token_span
        .ok_or_else(|| "cannot patch this pattern yet".to_string())?;
    if span.start < expr.mini_span.start || span.end > expr.mini_span.end {
        return Err("cannot patch this pattern yet".to_string());
    }
    let token = strip_timing_postfix(&code[span.start..span.end]).to_string();
    if token.is_empty() || token_is_rest_like(&token) {
        return Err("cannot patch this pattern yet".to_string());
    }

    // Тот же интервал по таймлайну (например отпустили перетаскивание на месте) — не трогаем код,
    // иначе try_slot_swap вернёт None и сработает разрушительный fallback.
    let t_eps = 1e-5_f64;
    if (begin - event.begin).abs() < t_eps && (end - event.end).abs() < t_eps {
        return Ok(CodePatch::empty());
    }

    let mini = &code[expr.mini_span.start..expr.mini_span.end];
    if let Some(patch) = try_slot_resize_in_parallel_row(code, expr, event, begin, end) {
        return Ok(patch);
    }
    if let Some(patch) = try_reorder_token_in_bracket_row(code, expr, event, begin, end) {
        return Ok(patch);
    }
    if let Some(patch) = try_slot_swap_in_parallel_row(code, expr, event, begin, end) {
        return Ok(patch);
    }
    let rel_start = span.start - expr.mini_span.start;
    let rel_end = span.end - expr.mini_span.start;
    let rest_fill = rest_fill_string_for_mini(mini);
    let mut replacement = String::with_capacity(mini.len() + 64);
    replacement.push_str(&mini[..rel_start]);
    replacement.push_str(&rest_fill);
    replacement.push_str(&mini[rel_end..]);
    replacement.push_str(", ");
    replacement.push_str(&pretty_grid_branch_line(
        &token,
        begin,
        end,
        steps.max(1),
        &rest_fill,
    )?);
    // `compact_parallel_branches` / `remove_rest_only_branches` заточены под плоские ветки и
    // `split_whitespace`; на `[-  -  oh - ], …` дают битые скобки и лишние слияния.
    if split_top_level_branches(mini).len() >= 2 {
        return Ok(CodePatch::replace_span(expr.mini_span, replacement));
    }
    let replacement = remove_rest_only_branches(&replacement);
    let replacement = compact_parallel_branches(&replacement);
    Ok(CodePatch::replace_span(expr.mini_span, replacement))
}

/// Растягивание/сжатие внутри `[ ... ]` одной строки параллели: доля события
/// `f = new_dur / длина_скобки` задаётся весами Strudel: `w / (L + w + R) = f`,
/// где `L` и `R` — число соседних пауз (вес 1) до/после сэмпла, `w` — вес `@`.
/// Переписываем скобку целиком и выбираем **компактную** раскладку (меньше токенов):
/// например `[- - cp@6]` вместо `[- - cp@12 - -]` при той же доле.
fn try_slot_resize_in_parallel_row(
    code: &str,
    expr: &PatternExpression,
    event: &PatternEvent,
    begin: f64,
    end: f64,
) -> Option<CodePatch> {
    let span = event.token_span?;
    if split_top_level_branches(&code[expr.mini_span.start..expr.mini_span.end]).len() < 2 {
        return None;
    }
    if (begin - event.begin).abs() > 1e-6 {
        return None;
    }
    let rel = span.start.saturating_sub(expr.mini_span.start);
    let mini = &code[expr.mini_span.start..expr.mini_span.end];
    let (bstart, bend) = branch_bounds(mini, rel)?;
    let branch_slice = &mini[bstart..bend];
    let source_base = expr.mini_span.start + bstart;
    let pattern = parse_pattern(branch_slice, source_base);
    let slots = flatten_events(&pattern, 0.0, 1.0);
    if slots.is_empty() || slots.iter().any(|s| !s.editable && !s.rest) {
        return None;
    }
    slots
        .iter()
        .position(|s| s.span.start == span.start && s.span.end == span.end)?;
    let (group_children, group_ast_span) = enclosing_group_children(&pattern.items, span)?;
    let m = group_children.len();
    if m < 2 {
        return None;
    }
    let idx = group_children
        .iter()
        .position(|c| c.span.start == span.start && c.span.end == span.end)?;
    if !siblings_have_unit_weight(group_children, idx) {
        return None;
    }
    for (i, ch) in group_children.iter().enumerate() {
        if i == idx {
            if !matches!(ch.kind, MiniItemKind::Token(_)) {
                return None;
            }
        } else if !matches!(ch.kind, MiniItemKind::Rest) {
            return None;
        }
    }
    let l = idx;
    let r_cur = m - idx - 1;
    let (t0, t1) = time_extent_for_ast_span(&slots, group_ast_span)?;
    let bracket_dur = t1 - t0;
    if bracket_dur <= 1e-12 {
        return None;
    }
    let new_dur = end - begin;
    let f = new_dur / bracket_dur;
    if f <= 1e-6 || f >= 1.0 - 1e-6 {
        return None;
    }
    let r_cap = r_cur.max(16).saturating_add(8);
    let (w_new, r_new) = best_weighted_cp_bracket_layout(f, l, r_cap)?;
    let full_src = code.get(span.start..span.end)?;
    let base = strip_timing_postfix(full_src);
    let rest_fill = rest_fill_string_for_mini(branch_slice);
    let rest_tok = rest_fill.as_str();
    let mut parts = Vec::with_capacity(l + r_new + 1);
    for _ in 0..l {
        parts.push(rest_tok.to_string());
    }
    parts.push(if w_new <= 1 {
        base.to_string()
    } else {
        format!("{base}@{w_new}")
    });
    for _ in 0..r_new {
        parts.push(rest_tok.to_string());
    }
    let old_group = code.get(group_ast_span.start..group_ast_span.end)?;
    let new_group =
        rebuild_bracket_preserving_child_gaps(code, group_ast_span, group_children, &parts)
            .unwrap_or_else(|| format_weighted_bracket_group(rest_tok, l, w_new, r_new, base));
    if new_group == old_group {
        return None;
    }
    let patch = CodePatch::replace_span(group_ast_span, new_group);
    patch.apply(code).ok()?;
    Some(patch)
}

/// Подбирает неотрицательные целые `w`, `r` с минимальным `l + 1 + r`, чтобы
/// `w / (l + w + r) ≈ f` (паузы веса 1, один сэмпл с весом `w`).
fn best_weighted_cp_bracket_layout(f: f64, l: usize, r_max: usize) -> Option<(usize, usize)> {
    if f <= 1e-9 || f >= 1.0 - 1e-9 {
        return None;
    }
    let mut best: Option<(usize, usize, usize)> = None;
    for r in 0..=r_max {
        let w_f = f * (l + r) as f64 / (1.0 - f);
        let w = w_f.round() as i64;
        if w < 1 {
            continue;
        }
        let w = w as usize;
        let denom = l + w + r;
        if denom == 0 {
            continue;
        }
        let f_act = w as f64 / denom as f64;
        if (f_act - f).abs() > 2e-3 || (w_f - w as f64).abs() > 1.2e-2 {
            continue;
        }
        let tokens = l + 1 + r;
        best = match best {
            None => Some((tokens, w, r)),
            Some((bt, _, _)) if tokens < bt => Some((tokens, w, r)),
            Some(b) => Some(b),
        };
    }
    best.map(|(_, w, r)| (w, r))
}

fn format_weighted_bracket_group(
    rest_tok: &str,
    l: usize,
    w: usize,
    r: usize,
    sample_base: &str,
) -> String {
    let mut parts = Vec::with_capacity(l + r + 1);
    for _ in 0..l {
        parts.push(rest_tok.to_string());
    }
    parts.push(if w <= 1 {
        sample_base.to_string()
    } else {
        format!("{sample_base}@{w}")
    });
    for _ in 0..r {
        parts.push(rest_tok.to_string());
    }
    format!("[{}]", parts.join(" "))
}

/// Пробелы между соседними детьми `[ ... ]` по исходным span'ам (как в файле).
#[derive(Debug, Clone)]
struct BracketChildGapLayout {
    leading_ws: String,
    gaps: Vec<String>,
    trailing_ws: String,
}

fn bracket_layout_from_group_children(
    code: &str,
    group_ast_span: SourceSpan,
    children: &[MiniItem],
) -> Option<BracketChildGapLayout> {
    if children.is_empty() {
        return None;
    }
    let full = code.get(group_ast_span.start..group_ast_span.end)?;
    let inner_open = full.find('[')? + 1;
    let inner_close = full.rfind(']')?;
    if inner_close < inner_open {
        return None;
    }
    let abs_inner_start = group_ast_span.start + inner_open;
    let abs_inner_end = group_ast_span.start + inner_close;
    let first = children.first()?;
    let leading_ws = code.get(abs_inner_start..first.span.start)?.to_string();
    let mut gaps = Vec::with_capacity(children.len().saturating_sub(1));
    for i in 0..children.len().saturating_sub(1) {
        let g = code
            .get(children[i].span.end..children[i + 1].span.start)?
            .to_string();
        gaps.push(g);
    }
    let last = children.last()?;
    let trailing_ws = code.get(last.span.end..abs_inner_end)?.to_string();
    Some(BracketChildGapLayout {
        leading_ws,
        gaps,
        trailing_ws,
    })
}

fn fallback_gap_from_layout(layout: &BracketChildGapLayout) -> &str {
    layout
        .gaps
        .iter()
        .max_by_key(|g| g.len())
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(" ")
}

fn rebuild_inner_preserving_gaps(layout: &BracketChildGapLayout, new_tokens: &[String]) -> String {
    let n = new_tokens.len();
    if n == 0 {
        return format!("{}{}", layout.leading_ws, layout.trailing_ws);
    }
    let mut s = String::with_capacity(layout.leading_ws.len() + layout.trailing_ws.len() + 32);
    s.push_str(&layout.leading_ws);
    s.push_str(&new_tokens[0]);
    for i in 1..n {
        let gap = if i - 1 < layout.gaps.len() {
            layout.gaps[i - 1].as_str()
        } else {
            fallback_gap_from_layout(layout)
        };
        s.push_str(gap);
        s.push_str(&new_tokens[i]);
    }
    s.push_str(&layout.trailing_ws);
    s
}

/// Собирает строку всей скобки `[ ... ]`, сохраняя пробелы из текущих детей.
fn rebuild_bracket_preserving_child_gaps(
    code: &str,
    group_ast_span: SourceSpan,
    old_children: &[MiniItem],
    new_tokens: &[String],
) -> Option<String> {
    let layout = bracket_layout_from_group_children(code, group_ast_span, old_children)?;
    let inner = rebuild_inner_preserving_gaps(&layout, new_tokens);
    let full = code.get(group_ast_span.start..group_ast_span.end)?;
    let inner_open = full.find('[')? + 1;
    let inner_close = full.rfind(']')?;
    let mut out = String::with_capacity(full.len() + inner.len());
    out.push_str(&full[..inner_open]);
    out.push_str(&inner);
    out.push_str(&full[inner_close..]);
    Some(out)
}

fn gap_for_old_indices(layout: &BracketChildGapLayout, a: usize, b: usize) -> &str {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    if hi == lo + 1 {
        layout.gaps.get(lo).map(|s| s.as_str()).unwrap_or(" ")
    } else {
        fallback_gap_from_layout(layout)
    }
}

fn rebuild_bracket_after_reorder(
    code: &str,
    group_ast_span: SourceSpan,
    group_children: &[MiniItem],
    perm: &[usize],
) -> Option<String> {
    let layout = bracket_layout_from_group_children(code, group_ast_span, group_children)?;
    let m = perm.len();
    if m == 0 {
        return None;
    }
    let mut new_tokens = Vec::with_capacity(m);
    for &oi in perm {
        let sp = group_children[oi].span;
        new_tokens.push(code.get(sp.start..sp.end)?.to_string());
    }
    let mut inner = String::with_capacity(code.len().min(256));
    inner.push_str(&layout.leading_ws);
    inner.push_str(&new_tokens[0]);
    for i in 1..m {
        let a = perm[i - 1];
        let b = perm[i];
        let gap = gap_for_old_indices(&layout, a, b);
        inner.push_str(gap);
        inner.push_str(&new_tokens[i]);
    }
    inner.push_str(&layout.trailing_ws);
    let full = code.get(group_ast_span.start..group_ast_span.end)?;
    let inner_open = full.find('[')? + 1;
    let inner_close = full.rfind(']')?;
    let mut out = String::with_capacity(full.len());
    out.push_str(&full[..inner_open]);
    out.push_str(&inner);
    out.push_str(&full[inner_close..]);
    Some(out)
}

/// Родительская группа `[ ... ]`, в которой лежит лист с данным span.
fn enclosing_group_children<'a>(
    items: &'a [MiniItem],
    span: SourceSpan,
) -> Option<(&'a [MiniItem], SourceSpan)> {
    for it in items {
        match &it.kind {
            MiniItemKind::Group(ch) => {
                if ch
                    .iter()
                    .any(|c| c.span.start == span.start && c.span.end == span.end)
                {
                    return Some((ch.as_slice(), it.span));
                }
                if let Some(inner) = enclosing_group_children(ch, span) {
                    return Some(inner);
                }
            }
            MiniItemKind::Parallel(branches) => {
                for br in branches {
                    if let Some(inner) = enclosing_group_children(br, span) {
                        return Some(inner);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn siblings_have_unit_weight(children: &[MiniItem], target_idx: usize) -> bool {
    for (i, ch) in children.iter().enumerate() {
        if i == target_idx {
            continue;
        }
        if (ch.weight - 1.0).abs() > 1e-5 {
            return false;
        }
    }
    true
}

fn time_extent_for_ast_span(slots: &[MiniEvent], sp: SourceSpan) -> Option<(f64, f64)> {
    let mut t0 = f64::MAX;
    let mut t1 = f64::MIN;
    let mut any = false;
    for s in slots {
        if s.span.start >= sp.start && s.span.end <= sp.end {
            t0 = t0.min(s.begin);
            t1 = t1.max(s.end);
            any = true;
        }
    }
    any.then_some((t0, t1))
}

/// Сдвиг токена (в т.ч. `cp@3`) внутри одной скобки `[ ... ]` строки: перестановка детей AST,
/// а не обмен двух одинаковых по длине span'ов.
fn try_reorder_token_in_bracket_row(
    code: &str,
    expr: &PatternExpression,
    event: &PatternEvent,
    new_begin: f64,
    new_end: f64,
) -> Option<CodePatch> {
    let span = event.token_span?;
    if split_top_level_branches(&code[expr.mini_span.start..expr.mini_span.end]).len() < 2 {
        return None;
    }
    if !move_interval_matches_piano_roll_drag(event, new_begin, new_end) {
        return None;
    }
    let rel = span.start.saturating_sub(expr.mini_span.start);
    let mini = &code[expr.mini_span.start..expr.mini_span.end];
    let (bstart, bend) = branch_bounds(mini, rel)?;
    let branch_slice = &mini[bstart..bend];
    let source_base = expr.mini_span.start + bstart;
    let pattern = parse_pattern(branch_slice, source_base);
    let slots = flatten_events(&pattern, 0.0, 1.0);
    if slots.is_empty() || slots.iter().any(|s| !s.editable && !s.rest) {
        return None;
    }
    let (group_children, group_ast_span) = enclosing_group_children(&pattern.items, span)?;
    if !children_are_flat_tokens_or_rests(group_children) {
        return None;
    }
    let m = group_children.len();
    if m < 2 {
        return None;
    }
    let i = group_children
        .iter()
        .position(|c| c.span.start == span.start && c.span.end == span.end)?;
    let ranges = child_time_ranges(group_children, &slots)?;
    let t = new_begin.rem_euclid(1.0);
    let (t0_br, t1_br) = time_extent_for_ast_span(&slots, group_ast_span)?;
    let j = child_index_containing_time(&ranges, t)?;
    let perm: Vec<usize> = if j != i {
        let mut perm: Vec<usize> = (0..m).collect();
        let moved = perm.remove(i);
        perm.insert(j, moved);
        perm
    } else {
        // `cp@3` занимает несколько «ячеек» сетки: по времени `t` всё ещё в том же AST-ребёнке,
        // но цель — другой порядок токенов в скобке (`[cp@3 -]` → `[- cp@3]`). Перебираем
        // перестановки (m≤4) и берём ту, где начало сегмента переносимого токена ближе к `t`.
        let perms = permutations_up_to_4(m)?;
        let identity: Vec<usize> = (0..m).collect();
        let mut best: Option<(f64, Vec<usize>)> = None;
        for p in perms {
            let ranges_p = time_ranges_for_perm(group_children, &p, t0_br, t1_br)?;
            let r = ranges_p[i];
            if !time_in_open_interval(t, r) {
                continue;
            }
            let score = t - r.0;
            let better = match &best {
                None => true,
                Some((s0, p0)) => {
                    if (score - *s0).abs() < 1e-9 {
                        p < *p0
                    } else {
                        score < *s0 - 1e-12
                    }
                }
            };
            if better {
                best = Some((score, p));
            }
        }
        let (_, p) = best?;
        if p == identity {
            return None;
        }
        p
    };
    let new_bracket = rebuild_bracket_after_reorder(code, group_ast_span, group_children, &perm)?;
    let old_bracket = code.get(group_ast_span.start..group_ast_span.end)?;
    if new_bracket == old_bracket {
        return None;
    }
    let patch = CodePatch::replace_span(group_ast_span, new_bracket);
    patch.apply(code).ok()?;
    Some(patch)
}

fn children_are_flat_tokens_or_rests(children: &[MiniItem]) -> bool {
    children
        .iter()
        .all(|c| matches!(c.kind, MiniItemKind::Token(_) | MiniItemKind::Rest))
}

fn child_time_ranges(children: &[MiniItem], slots: &[MiniEvent]) -> Option<Vec<(f64, f64)>> {
    let mut out = Vec::with_capacity(children.len());
    for ch in children {
        let ev = slots
            .iter()
            .find(|s| s.span.start == ch.span.start && s.span.end == ch.span.end)?;
        out.push((ev.begin, ev.end));
    }
    Some(out)
}

/// Временные интервалы детей скобки при порядке `perm` (как в `rebuild_bracket_after_reorder`).
fn time_ranges_for_perm(
    children: &[MiniItem],
    perm: &[usize],
    t0: f64,
    t1: f64,
) -> Option<Vec<(f64, f64)>> {
    if perm.len() != children.len() {
        return None;
    }
    let bl = t1 - t0;
    let tw: f64 = children
        .iter()
        .map(|c| c.weight.max(0.0))
        .sum::<f64>()
        .max(1e-12);
    let mut starts = vec![0.0_f64; children.len()];
    let mut ends = vec![0.0_f64; children.len()];
    let mut c = t0;
    for &pi in perm {
        let ch = children.get(pi)?;
        let len = bl * ch.weight.max(0.0) / tw;
        let s = c;
        c += len;
        starts[pi] = s;
        ends[pi] = c;
    }
    Some(starts.into_iter().zip(ends).collect())
}

fn permutations_up_to_4(m: usize) -> Option<Vec<Vec<usize>>> {
    if m == 0 || m > 4 {
        return None;
    }
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut buf = vec![0usize; m];
    let mut used = vec![false; m];
    fn rec(
        m: usize,
        buf: &mut [usize],
        used: &mut [bool],
        out: &mut Vec<Vec<usize>>,
        depth: usize,
    ) {
        if depth == m {
            out.push(buf.to_vec());
            return;
        }
        for i in 0..m {
            if used[i] {
                continue;
            }
            used[i] = true;
            buf[depth] = i;
            rec(m, buf, used, out, depth + 1);
            used[i] = false;
        }
    }
    rec(m, &mut buf, &mut used, &mut out, 0);
    Some(out)
}

fn time_in_open_interval(t: f64, r: (f64, f64)) -> bool {
    let eps = 1e-7_f64;
    t >= r.0 - eps && t < r.1 - eps
}

/// Полуинтервал [a, b): на границе `t == b` слот не считается — иначе на стыке `[..][..]`
/// выбирается последний элемент предыдущей скобки вместо первого следующей.
fn child_index_containing_time(ranges: &[(f64, f64)], t: f64) -> Option<usize> {
    let eps = 1e-7_f64;
    ranges
        .iter()
        .position(|&(a, b)| t >= a - eps && t < b - eps)
}

fn flat_slot_index_at_time(slots: &[MiniEvent], t: f64) -> Option<usize> {
    let eps = 1e-7_f64;
    slots
        .iter()
        .position(|s| t >= s.begin - eps && t < s.end - eps)
}

/// Перенос события внутри одной верхнеуровневой ветки (строки) `a, b, c, d` без добавления
/// новой параллельной ветки с сеткой из `~`.
fn try_slot_swap_in_parallel_row(
    code: &str,
    expr: &PatternExpression,
    event: &PatternEvent,
    new_begin: f64,
    new_end: f64,
) -> Option<CodePatch> {
    let span = event.token_span?;
    if split_top_level_branches(&code[expr.mini_span.start..expr.mini_span.end]).len() < 2 {
        return None;
    }
    let rel = span.start.saturating_sub(expr.mini_span.start);
    let mini = &code[expr.mini_span.start..expr.mini_span.end];
    let (bstart, bend) = branch_bounds(mini, rel)?;
    let branch_slice = &mini[bstart..bend];
    let source_base = expr.mini_span.start + bstart;
    let pattern = parse_pattern(branch_slice, source_base);
    let slots = flatten_events(&pattern, 0.0, 1.0);
    if slots.is_empty() {
        return None;
    }
    if slots.iter().any(|s| !s.editable && !s.rest) {
        return None;
    }
    if !move_interval_matches_piano_roll_drag(event, new_begin, new_end) {
        return None;
    }
    let src_ix = slots
        .iter()
        .position(|s| s.span.start == span.start && s.span.end == span.end)?;
    let t = new_begin.rem_euclid(1.0);
    let dst_ix = flat_slot_index_at_time(&slots, t)?;
    if src_ix == dst_ix {
        return None;
    }
    let src_span = slots[src_ix].span;
    let dst_span = slots[dst_ix].span;
    if src_span.start < dst_span.end && dst_span.start < src_span.end {
        return None;
    }
    let sb = slots[src_ix].begin;
    let se = slots[src_ix].end;
    let db = slots[dst_ix].begin;
    let de = slots[dst_ix].end;
    let teps = 1e-7_f64;
    if sb < de - teps && db < se - teps {
        return None;
    }
    let src_full = code.get(src_span.start..src_span.end)?.to_string();
    let dst_full = code.get(dst_span.start..dst_span.end)?.to_string();
    let rest_fill = rest_fill_string_for_mini(branch_slice);
    let patch = if slots[dst_ix].rest {
        CodePatch::new(vec![
            CodeEdit {
                span: dst_span,
                replacement: src_full,
            },
            CodeEdit {
                span: src_span,
                replacement: rest_fill,
            },
        ])
    } else {
        let s_src = sample_name_token(&src_full);
        let s_dst = sample_name_token(&dst_full);
        if s_src != s_dst {
            return None;
        }
        CodePatch::new(vec![
            CodeEdit {
                span: src_span,
                replacement: dst_full.clone(),
            },
            CodeEdit {
                span: dst_span,
                replacement: src_full,
            },
        ])
    };
    patch.apply(code).ok()?;
    Some(patch)
}

fn sample_name_token(token: &str) -> &str {
    token
        .split(|ch| matches!(ch, ':' | '/' | '@' | '?' | '!'))
        .next()
        .unwrap_or(token)
}

fn rest_fill_string_for_mini(mini: &str) -> String {
    infer_rest_char_for_source(mini).to_string()
}

fn infer_rest_char_for_source(src: &str) -> char {
    let mut dash = 0usize;
    let mut tilde = 0usize;
    for t in tokenize(src) {
        if t.kind == MiniTokenKind::Whitespace {
            continue;
        }
        match t.text.as_str() {
            "-" => dash += 1,
            "~" => tilde += 1,
            _ => {}
        }
    }
    if tilde > dash {
        '~'
    } else if dash > tilde {
        '-'
    } else if dash > 0 {
        '-'
    } else {
        '~'
    }
}

fn grid_branch(
    token: &str,
    begin: f64,
    end: f64,
    steps: usize,
    rest_fill: &str,
) -> Result<String, String> {
    let steps_f = steps as f64;
    let start = (begin.rem_euclid(1.0) * steps_f).floor() as usize;
    let mut finish = (end.clamp(0.0, 1.0) * steps_f).ceil() as usize;
    let start = start.min(steps.saturating_sub(1));
    finish = finish.clamp(start + 1, steps);
    let duration = finish - start;
    let mut parts = Vec::with_capacity(steps - duration + 1);
    parts.extend(std::iter::repeat(rest_fill.to_string()).take(start));
    if duration == 1 {
        parts.push(token.to_string());
    } else {
        parts.push(format!("{token}@{duration}"));
    }
    parts.extend(std::iter::repeat(rest_fill.to_string()).take(steps - finish));
    if parts.is_empty() {
        Err("cannot patch this pattern yet".to_string())
    } else {
        Ok(parts.join(" "))
    }
}

/// Хвост при переносе в новую ветку: как `grid_branch`, но шаги сгруппированы в `[a b c d]` по четыре
/// (последняя группа может быть короче — число токенов от сетки не всегда кратно 4).
fn pretty_grid_branch_line(
    token: &str,
    begin: f64,
    end: f64,
    steps: usize,
    rest_fill: &str,
) -> Result<String, String> {
    let flat = grid_branch(token, begin, end, steps, rest_fill)?;
    let tokens: Vec<&str> = flat.split_whitespace().collect();
    const CHUNK: usize = 4;
    if tokens.is_empty() {
        return Ok(flat);
    }
    let mut out = String::with_capacity(flat.len() + (tokens.len() + CHUNK - 1) / CHUNK * 4);
    for (i, chunk) in tokens.chunks(CHUNK).enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push('[');
        out.push_str(&chunk.join(" "));
        out.push(']');
    }
    Ok(out)
}

fn compact_parallel_branches(mini: &str) -> String {
    let branches = split_top_level_branches(mini);
    if branches.len() <= 1 {
        return mini.to_string();
    }
    let parsed: Vec<Vec<&str>> = branches
        .iter()
        .map(|b| b.split_whitespace().collect())
        .collect();
    let slot_count = match parsed.first() {
        Some(t) if !t.is_empty() => t.len(),
        _ => return mini.to_string(),
    };
    if !parsed.iter().all(|t| t.len() == slot_count) {
        return mini.to_string();
    }
    let is_simple = |tok: &&str| -> bool {
        token_is_rest_like(tok)
            || tok
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    if !parsed.iter().all(|tokens| tokens.iter().all(is_simple)) {
        return mini.to_string();
    }
    let mut merged: Vec<&str> = vec!["~"; slot_count];
    for tokens in &parsed {
        for (i, &tok) in tokens.iter().enumerate() {
            if !token_is_rest_like(tok) {
                if merged[i] != "~" {
                    return mini.to_string();
                }
                merged[i] = tok;
            }
        }
    }
    merged.join(" ")
}

fn remove_rest_only_branches(mini: &str) -> String {
    let branches = split_top_level_branches(mini);
    let non_rest: Vec<&str> = branches
        .into_iter()
        .filter(|branch| !branch.split_whitespace().all(token_is_rest_like))
        .collect();
    if non_rest.is_empty() {
        "~".to_string()
    } else {
        non_rest.join(", ")
    }
}

fn split_top_level_branches(mini: &str) -> Vec<&str> {
    let mut branches = Vec::new();
    let mut depth_square = 0usize;
    let mut depth_angle = 0usize;
    let mut depth_round = 0usize;
    let mut start = 0usize;
    for (idx, ch) in mini.char_indices() {
        match ch {
            '[' => depth_square += 1,
            ']' => depth_square = depth_square.saturating_sub(1),
            '<' => depth_angle += 1,
            '>' => depth_angle = depth_angle.saturating_sub(1),
            '(' => depth_round += 1,
            ')' => depth_round = depth_round.saturating_sub(1),
            ',' if depth_square == 0 && depth_angle == 0 && depth_round == 0 => {
                branches.push(mini[start..idx].trim());
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    branches.push(mini[start..].trim());
    branches
}

fn strip_timing_postfix(token: &str) -> &str {
    token
        .find(|ch| matches!(ch, '@' | '*' | '/' | '!' | '?'))
        .map(|index| &token[..index])
        .unwrap_or(token)
}

fn token_is_rest_like(token: &str) -> bool {
    let base = strip_timing_postfix(token).trim();
    if base.is_empty() {
        return false;
    }
    let mut saw_rest = false;
    for ch in base.chars() {
        match ch {
            '~' | '-' => saw_rest = true,
            '[' | ']' | '<' | '>' | '(' | ')' | ' ' | '\t' | '\n' | '\r' => {}
            _ => return false,
        }
    }
    saw_rest
}

fn branch_slot_count(mini: &str, token_start: usize) -> usize {
    let branch = branch_bounds(mini, token_start)
        .map(|(start, end)| &mini[start..end])
        .unwrap_or(mini);
    let count = branch
        .split_whitespace()
        .filter(|part| !part.trim_matches(',').is_empty())
        .count();
    count.max(1)
}

fn branch_bounds(mini: &str, token_start: usize) -> Option<(usize, usize)> {
    let bytes = mini.as_bytes();
    let mut start = 0usize;
    let mut depth_square = 0usize;
    let mut depth_angle = 0usize;
    let mut depth_round = 0usize;
    for (idx, ch) in mini.char_indices() {
        if idx >= token_start {
            break;
        }
        match ch {
            '[' => depth_square += 1,
            ']' => depth_square = depth_square.saturating_sub(1),
            '<' => depth_angle += 1,
            '>' => depth_angle = depth_angle.saturating_sub(1),
            '(' => depth_round += 1,
            ')' => depth_round = depth_round.saturating_sub(1),
            ',' if depth_square == 0 && depth_angle == 0 && depth_round == 0 => {
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    let mut end = mini.len();
    depth_square = 0;
    depth_angle = 0;
    depth_round = 0;
    for (rel, ch) in mini[token_start..].char_indices() {
        let idx = token_start + rel;
        match ch {
            '[' => depth_square += 1,
            ']' => depth_square = depth_square.saturating_sub(1),
            '<' => depth_angle += 1,
            '>' => depth_angle = depth_angle.saturating_sub(1),
            '(' => depth_round += 1,
            ')' => depth_round = depth_round.saturating_sub(1),
            ',' if depth_square == 0 && depth_angle == 0 && depth_round == 0 => {
                end = idx;
                break;
            }
            _ => {}
        }
    }
    while start < end && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    Some((start, end))
}

#[derive(Clone)]
struct SimpleToken {
    span: SourceSpan,
    text: String,
}

fn simple_tokens(code: &str, expr: &PatternExpression) -> Result<Vec<SimpleToken>, String> {
    let mini = &code[expr.mini_span.start..expr.mini_span.end];
    if mini
        .chars()
        .any(|ch| matches!(ch, '[' | ']' | '<' | '>' | ',' | '(' | ')'))
    {
        return Err("cannot patch this pattern yet".to_string());
    }
    let mut tokens = Vec::new();
    let mut cursor = 0usize;
    while cursor < mini.len() {
        while mini[cursor..]
            .chars()
            .next()
            .map(|ch| ch.is_whitespace())
            .unwrap_or(false)
        {
            cursor += mini[cursor..].chars().next().unwrap().len_utf8();
        }
        if cursor >= mini.len() {
            break;
        }
        let start = cursor;
        while cursor < mini.len()
            && !mini[cursor..]
                .chars()
                .next()
                .map(|ch| ch.is_whitespace())
                .unwrap_or(false)
        {
            cursor += mini[cursor..].chars().next().unwrap().len_utf8();
        }
        tokens.push(SimpleToken {
            span: SourceSpan::new(expr.mini_span.start + start, expr.mini_span.start + cursor),
            text: mini[start..cursor].to_string(),
        });
    }
    if tokens.is_empty() {
        Err("cannot patch this pattern yet".to_string())
    } else {
        Ok(tokens)
    }
}

fn token_delete_span(code: &str, span: SourceSpan) -> SourceSpan {
    let mut start = span.start;
    let mut end = span.end;
    while end < code.len()
        && code[end..]
            .chars()
            .next()
            .map(|ch| ch.is_whitespace())
            .unwrap_or(false)
    {
        end += code[end..].chars().next().unwrap().len_utf8();
        return SourceSpan::new(start, end);
    }
    while start > 0 {
        let Some((prev_start, prev)) = code[..start].char_indices().next_back() else {
            break;
        };
        if prev.is_whitespace() {
            start = prev_start;
        } else {
            break;
        }
    }
    SourceSpan::new(start, end)
}

fn escape_string(value: &str, quote: char) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch == '\\' || ch == quote {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strudel_parser::parse_pattern_document;

    fn edit_for_second_event(code: &str, kind: PatternEditKind) -> PatternEdit {
        let doc = parse_pattern_document(code, 1);
        let event = doc.ir.events[1].clone();
        PatternEdit {
            event_id: Some(event.id.clone()),
            pattern_id: event.pattern_id.clone(),
            token_span: event.token_span,
            token_value: event.sample().map(str::to_string),
            kind,
        }
    }

    #[test]
    fn deletes_token_locally() {
        let code = r#"sound("bd hh sd oh")"#;
        let result = apply_pattern_edit(
            code,
            1,
            edit_for_second_event(code, PatternEditKind::DeleteEvent),
        )
        .unwrap();
        assert_eq!(result.code, r#"sound("bd sd oh")"#);
    }

    #[test]
    fn duplicates_token_locally() {
        let code = r#"sound("bd hh sd oh")"#;
        let result = apply_pattern_edit(
            code,
            1,
            edit_for_second_event(code, PatternEditKind::DuplicateEvent),
        )
        .unwrap();
        assert_eq!(result.code, r#"sound("bd hh hh sd oh")"#);
    }

    #[test]
    fn changes_bank_locally() {
        let code = r#"sound("bd").bank("A")"#;
        let doc = parse_pattern_document(code, 1);
        let edit = PatternEdit {
            event_id: None,
            pattern_id: doc.ir.patterns[0].id.clone(),
            token_span: None,
            token_value: None,
            kind: PatternEditKind::ChangeBank("B".to_string()),
        };
        let result = apply_pattern_edit(code, 1, edit).unwrap();
        assert_eq!(result.code, r#"sound("bd").bank("B")"#);
    }

    #[test]
    fn inserts_rest_locally() {
        let code = r#"sound("bd hh sd oh")"#;
        let doc = parse_pattern_document(code, 1);
        let edit = PatternEdit {
            event_id: None,
            pattern_id: doc.ir.patterns[0].id.clone(),
            token_span: None,
            token_value: None,
            kind: PatternEditKind::InsertRest { at: 0.5 },
        };
        let result = apply_pattern_edit(code, 1, edit).unwrap();
        assert_eq!(result.code, r#"sound("bd hh ~ sd oh")"#);
    }

    #[test]
    fn resize_extracts_event_to_parallel_branch() {
        let code = r#"sound("bd hh sd oh")"#;
        let doc = parse_pattern_document(code, 1);
        let event = doc.ir.events[1].clone();
        let edit = PatternEdit {
            event_id: Some(event.id.clone()),
            pattern_id: event.pattern_id.clone(),
            token_span: event.token_span,
            token_value: event.sample().map(str::to_string),
            kind: PatternEditKind::ResizeEvent { new_end: 0.75 },
        };
        let result = apply_pattern_edit(code, 1, edit).unwrap();
        assert_eq!(result.code, r#"sound("bd ~ sd oh, [~ hh@2 ~]")"#);
        let hh = result
            .document
            .ir
            .events
            .iter()
            .find(|event| event.sample() == Some("hh"))
            .unwrap();
        assert!((hh.begin - 0.25).abs() < 0.000_001);
        assert!((hh.end - 0.75).abs() < 0.000_001);
    }

    #[test]
    fn move_extracts_event_to_parallel_branch() {
        let code = r#"sound("bd hh sd oh")"#;
        let doc = parse_pattern_document(code, 1);
        let event = doc.ir.events[1].clone();
        let edit = PatternEdit {
            event_id: Some(event.id.clone()),
            pattern_id: event.pattern_id.clone(),
            token_span: event.token_span,
            token_value: event.sample().map(str::to_string),
            kind: PatternEditKind::MoveEvent { new_begin: 0.0 },
        };
        let result = apply_pattern_edit(code, 1, edit).unwrap();
        assert_eq!(result.code, r#"sound("bd ~ sd oh, [hh ~ ~ ~]")"#);
    }

    #[test]
    fn move_casio_onto_bd_keeps_compact_parallel_shape() {
        let code = r#"sound("hh hh hh, bd casio")"#;
        let doc = parse_pattern_document(code, 1);
        let event = doc
            .ir
            .events
            .iter()
            .find(|event| event.sample() == Some("casio"))
            .unwrap()
            .clone();
        let edit = PatternEdit {
            event_id: Some(event.id.clone()),
            pattern_id: event.pattern_id.clone(),
            token_span: event.token_span,
            token_value: event.sample().map(str::to_string),
            kind: PatternEditKind::MoveEvent { new_begin: 0.0 },
        };
        let result = apply_pattern_edit(code, 1, edit).unwrap();
        assert_eq!(result.code, r#"sound("hh hh hh, bd ~, [casio ~]")"#);
    }

    #[test]
    fn move_all_events_compacts_to_single_branch() {
        // After moving hh to position 0 (same as bd), oh and sd each get their own branch.
        // When all branches are flat with one non-rest per slot, compact into one.
        let mini = "bd ~ ~ ~, ~ oh ~ ~, ~ ~ hh ~, ~ ~ ~ sd";
        assert_eq!(compact_parallel_branches(mini), "bd oh hh sd");
    }

    #[test]
    fn partial_compact_merges_what_it_can() {
        let mini = "bd ~ ~ ~, ~ ~ hh ~";
        assert_eq!(compact_parallel_branches(mini), "bd ~ hh ~");
    }

    #[test]
    fn compact_skips_on_slot_conflict() {
        let mini = "bd ~ ~, bd hh ~";
        assert_eq!(compact_parallel_branches(mini), "bd ~ ~, bd hh ~");
    }

    #[test]
    fn compact_skips_on_mismatched_slot_count() {
        let mini = "bd ~ ~, ~ hh ~ ~";
        assert_eq!(compact_parallel_branches(mini), "bd ~ ~, ~ hh ~ ~");
    }

    #[test]
    fn remove_rest_only_branches_understands_grouped_rests() {
        let mini = "bd ~ ~ ~, [~ ~ ~]@7 ~ ~, ~ hh ~ ~";
        assert_eq!(remove_rest_only_branches(mini), "bd ~ ~ ~, ~ hh ~ ~");
    }

    #[test]
    fn compact_parallel_branches_treats_grouped_rests_as_empty() {
        let mini = "bd ~ ~ ~, [~]@2 ~ ~ ~, ~ ~ hh ~";
        assert_eq!(compact_parallel_branches(mini), "bd ~ hh ~");
    }

    #[test]
    fn branch_quantization_uses_floor_for_start_slot() {
        let branch = grid_branch("casio", 0.49, 0.5, 2, "~").unwrap();
        assert_eq!(branch, "casio ~");
    }

    #[test]
    fn resize_in_parallel_row_double_cp_compacts_to_cp_at2() {
        // `[- - cp - ]`: доля cp 1/4; удваиваем длину → 1/2 скобки → w/(2+w)=1/2 ⇒ cp@2; паузы справа убираем: `[- - cp@2]`.
        let code = r#"sound("[- - cp -], [hh hh hh hh]")"#;
        let doc = parse_pattern_document(code.to_string(), 1);
        let ev = doc
            .ir
            .events
            .iter()
            .find(|e| e.sample() == Some("cp"))
            .unwrap()
            .clone();
        let u = ev.end - ev.begin;
        let result = apply_pattern_edit(
            code,
            1,
            PatternEdit {
                event_id: Some(ev.id.clone()),
                pattern_id: ev.pattern_id.clone(),
                token_span: ev.token_span,
                token_value: ev.sample().map(str::to_string),
                kind: PatternEditKind::ResizeEvent {
                    new_end: ev.begin + 2.0 * u,
                },
            },
        )
        .unwrap();
        assert!(
            result.code.contains("cp@2") && result.code.contains("[- - cp@2]"),
            "got {}",
            result.code
        );
        assert_eq!(
            result.code.matches(',').count(),
            1,
            "single top-level comma, got {}",
            result.code
        );
    }

    #[test]
    fn resize_in_parallel_row_triple_cp_compacts_to_cp_at6() {
        // Тройная доля от 1/4 → 3/4 ⇒ w/(2+w)=3/4 ⇒ cp@6, компактно `[- - cp@6]`.
        let code = r#"sound("[- - cp -], [hh hh hh hh]")"#;
        let doc = parse_pattern_document(code.to_string(), 1);
        let ev = doc
            .ir
            .events
            .iter()
            .find(|e| e.sample() == Some("cp"))
            .unwrap()
            .clone();
        let u = ev.end - ev.begin;
        let result = apply_pattern_edit(
            code,
            1,
            PatternEdit {
                event_id: Some(ev.id.clone()),
                pattern_id: ev.pattern_id.clone(),
                token_span: ev.token_span,
                token_value: ev.sample().map(str::to_string),
                kind: PatternEditKind::ResizeEvent {
                    new_end: ev.begin + 3.0 * u,
                },
            },
        )
        .unwrap();
        assert!(
            result.code.contains("cp@6") && result.code.contains("[- - cp@6]"),
            "got {}",
            result.code
        );
    }

    #[test]
    fn resize_in_bracket_preserves_inter_token_whitespace() {
        let code = r#"sound("[ -  -  cp  -  ], [hh]")"#;
        let doc = parse_pattern_document(code.to_string(), 1);
        let ev = doc
            .ir
            .events
            .iter()
            .find(|e| e.sample() == Some("cp"))
            .unwrap()
            .clone();
        let u = ev.end - ev.begin;
        let result = apply_pattern_edit(
            code,
            1,
            PatternEdit {
                event_id: Some(ev.id.clone()),
                pattern_id: ev.pattern_id.clone(),
                token_span: ev.token_span,
                token_value: ev.sample().map(str::to_string),
                kind: PatternEditKind::ResizeEvent {
                    new_end: ev.begin + 2.0 * u,
                },
            },
        )
        .unwrap();
        assert!(
            result.code.contains("  "),
            "doubled spaces between atoms should remain, got {}",
            result.code
        );
        assert!(result.code.contains("cp@2"), "{}", result.code);
    }

    #[test]
    fn resize_cp_at4_with_trailing_rests_compacts_to_cp_at6() {
        // Как в UI: было `[- - cp@4 - -]`, тянем до 3/4 четверти → `[- - cp@6]`.
        let code = r#"sound("[- - cp@4 - -], [hh hh hh hh]")"#;
        let doc = parse_pattern_document(code.to_string(), 1);
        let ev = doc
            .ir
            .events
            .iter()
            .find(|e| e.sample() == Some("cp"))
            .unwrap()
            .clone();
        let u = ev.end - ev.begin;
        let result = apply_pattern_edit(
            code,
            1,
            PatternEdit {
                event_id: Some(ev.id.clone()),
                pattern_id: ev.pattern_id.clone(),
                token_span: ev.token_span,
                token_value: ev.sample().map(str::to_string),
                kind: PatternEditKind::ResizeEvent {
                    new_end: ev.begin + 1.5 * u,
                },
            },
        )
        .unwrap();
        assert!(
            result.code.contains("[- - cp@6]"),
            "expected compact bracket, got {}",
            result.code
        );
    }

    #[test]
    fn resize_in_parallel_row_shrinks_cp_at9_to_plain_cp() {
        let code = r#"sound("[- - cp@9 -], [hh hh hh hh]")"#;
        let doc = parse_pattern_document(code.to_string(), 1);
        let ev = doc
            .ir
            .events
            .iter()
            .find(|e| e.sample() == Some("cp"))
            .unwrap()
            .clone();
        let u = (ev.end - ev.begin) / 3.0;
        let result = apply_pattern_edit(
            code,
            1,
            PatternEdit {
                event_id: Some(ev.id.clone()),
                pattern_id: ev.pattern_id.clone(),
                token_span: ev.token_span,
                token_value: ev.sample().map(str::to_string),
                kind: PatternEditKind::ResizeEvent {
                    new_end: ev.begin + u,
                },
            },
        )
        .unwrap();
        assert!(
            !result.code.contains("cp@"),
            "strip @ for one slot, got {}",
            result.code
        );
        assert!(result.code.contains("cp"), "{}", result.code);
    }

    #[test]
    fn move_to_same_timeline_position_is_noop() {
        let code = r#"sound("[- - oh -], [hh hh - -], [cp@9 - - -], [bd - - -]")"#;
        let doc = parse_pattern_document(code.to_string(), 1);
        let ev = doc
            .ir
            .events
            .iter()
            .find(|e| e.sample() == Some("cp"))
            .unwrap()
            .clone();
        let result = apply_pattern_edit(
            code,
            1,
            PatternEdit {
                event_id: Some(ev.id.clone()),
                pattern_id: ev.pattern_id.clone(),
                token_span: ev.token_span,
                token_value: ev.sample().map(str::to_string),
                kind: PatternEditKind::MoveEvent {
                    new_begin: ev.begin,
                },
            },
        )
        .unwrap();
        assert_eq!(result.code, code);
    }

    #[test]
    fn move_within_grid_row_swaps_with_dash_no_extra_branch() {
        let code = r#"sound("[bd -] [- bd], [hh -] [- hh]")"#;
        let doc = parse_pattern_document(code.to_string(), 1);
        let ev = doc
            .ir
            .events
            .iter()
            .find(|e| e.sample() == Some("bd") && (e.begin - 0.75).abs() < 0.01)
            .unwrap()
            .clone();
        let result = apply_pattern_edit(
            code,
            1,
            PatternEdit {
                event_id: Some(ev.id.clone()),
                pattern_id: ev.pattern_id.clone(),
                token_span: ev.token_span,
                token_value: ev.sample().map(str::to_string),
                kind: PatternEditKind::MoveEvent { new_begin: 0.5 },
            },
        )
        .unwrap();
        assert!(
            !result.code.contains('~'),
            "should keep `-` rests, got {}",
            result.code
        );
        assert!(
            result.code.contains("[bd -] [bd -]"),
            "expected swapped row, got {}",
            result.code
        );
        assert_eq!(
            result.code.matches(',').count(),
            1,
            "single comma between tracks, got {}",
            result.code
        );
    }

    #[test]
    fn move_cp_at3_one_step_right_reorders_inside_bracket() {
        let code = r#"sound("[- - - -], [hh - - -], [cp@3 - - -], [bd - - -]")"#;
        let doc = parse_pattern_document(code.to_string(), 1);
        let ev_cp = doc
            .ir
            .events
            .iter()
            .find(|e| e.sample() == Some("cp"))
            .unwrap()
            .clone();
        let result = apply_pattern_edit(
            code,
            1,
            PatternEdit {
                event_id: Some(ev_cp.id.clone()),
                pattern_id: ev_cp.pattern_id.clone(),
                token_span: ev_cp.token_span,
                token_value: ev_cp.sample().map(str::to_string),
                kind: PatternEditKind::MoveEvent {
                    new_begin: ev_cp.end,
                },
            },
        )
        .unwrap();
        assert!(
            result.code.contains("- cp@3")
                || result.code.contains(" - cp@3")
                || result.code.contains("[ - cp@3"),
            "cp should move right inside bracket, got {}",
            result.code
        );
        assert_eq!(
            result.code.matches(',').count(),
            3,
            "still four parallel rows, got {}",
            result.code
        );
    }

    /// Как в UI: `[cp@3 - ]` внутри четверти, сдвиг на одну 1/16 вправо — `[- cp@3 …]`, без fallback-хвоста.
    #[test]
    fn move_cp_at3_in_two_token_bracket_one_step_right_no_parallel_tail() {
        let code = r#"sound("[-  -  -  - ] [cp@3 - ] [-  -  -  - ] [cp -  -  - ], [hh]")"#;
        let doc = parse_pattern_document(code.to_string(), 1);
        let ev_cp = doc
            .ir
            .events
            .iter()
            .find(|e| {
                e.sample() == Some("cp")
                    && e.token_span.is_some_and(|sp| {
                        code.get(sp.start..sp.end).is_some_and(|t| t.contains('@'))
                    })
            })
            .unwrap()
            .clone();
        let new_begin = 5.0 / 16.0;
        let result = apply_pattern_edit(
            code,
            1,
            PatternEdit {
                event_id: Some(ev_cp.id.clone()),
                pattern_id: ev_cp.pattern_id.clone(),
                token_span: ev_cp.token_span,
                token_value: ev_cp.sample().map(str::to_string),
                kind: PatternEditKind::MoveEvent { new_begin },
            },
        )
        .unwrap();
        assert!(
            result.code.contains("- cp@3")
                || result.code.contains(" - cp@3")
                || result.code.contains("[- cp@3"),
            "expected rest before cp@3 in bracket, got {}",
            result.code
        );
        assert_eq!(
            result.code.matches(',').count(),
            1,
            "no extra parallel branch, got {}",
            result.code
        );
        assert_eq!(
            result.code.matches("cp@").count(),
            1,
            "single cp@ timing, got {}",
            result.code
        );
    }

    #[test]
    fn move_cp_at3_four_tracks_quarter_boundary_stays_three_commas() {
        let code = r#"sound(`[- - - -], [- - - -], [- - - -] [- cp@3 - -] [- - - -] [cp - - -], [- - - -]`)"#;
        let doc = parse_pattern_document(code.to_string(), 1);
        let ev_cp = doc
            .ir
            .events
            .iter()
            .find(|e| {
                e.sample() == Some("cp")
                    && e.token_span.is_some_and(|sp| {
                        code.get(sp.start..sp.end).is_some_and(|t| t.contains('@'))
                    })
            })
            .unwrap()
            .clone();
        let new_begin = 0.5 + 1.0 / 64.0;
        let result = apply_pattern_edit(
            code,
            1,
            PatternEdit {
                event_id: Some(ev_cp.id.clone()),
                pattern_id: ev_cp.pattern_id.clone(),
                token_span: ev_cp.token_span,
                token_value: ev_cp.sample().map(str::to_string),
                kind: PatternEditKind::MoveEvent { new_begin },
            },
        )
        .unwrap();
        assert_eq!(
            result.code.matches(',').count(),
            3,
            "no fifth parallel branch from grid fallback, got {}",
            result.code
        );
    }

    #[test]
    fn move_cp_at3_to_next_bracket_no_extra_parallel_tail() {
        let code = r#"sound("hh, [- - - -] [- cp@3 - -] [- - - -] [cp - - -]")"#;
        let doc = parse_pattern_document(code.to_string(), 1);
        let ev_cp = doc
            .ir
            .events
            .iter()
            .find(|e| e.sample() == Some("cp") && e.begin > 0.2 && e.begin < 0.35)
            .unwrap()
            .clone();
        // третья четверть такта: ~0.5 + чуть внутрь первого слота
        let new_begin = 0.5 + 1.0 / 64.0;
        let result = apply_pattern_edit(
            code,
            1,
            PatternEdit {
                event_id: Some(ev_cp.id.clone()),
                pattern_id: ev_cp.pattern_id.clone(),
                token_span: ev_cp.token_span,
                token_value: ev_cp.sample().map(str::to_string),
                kind: PatternEditKind::MoveEvent { new_begin },
            },
        )
        .unwrap();
        assert_eq!(
            result.code.matches(',').count(),
            1,
            "no extra `,` branch from grid fallback, got {}",
            result.code
        );
        assert!(
            !result.code.contains("cp@2") || result.code.contains("[- cp@3"),
            "unexpected resize to @2 or junk tail: {}",
            result.code
        );
    }

    #[test]
    fn pretty_grid_branch_line_chunks_by_four() {
        let s = pretty_grid_branch_line("cp", 0.5, 0.75, 16, "-").unwrap();
        assert!(
            s.contains('[') && s.contains(']'),
            "expected bracket chunks, got {s:?}"
        );
        assert!(
            s.starts_with('['),
            "should lead with a bar group, got {s:?}"
        );
        assert_eq!(s.matches("cp@4").count(), 1, "got {s:?}");
    }
}
