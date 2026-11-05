//! Directives visuelles inline style Strudel : `._scope()`, `._spectrum()`,
//! `._punchcard()`, `._pianoroll()`… affichent un mini-visual sous l'éditeur.
//! `.nom()` sans underscore = visual « background » (même rendu pour l'instant).
//!
//! Les directives ne modifient pas le son : `strip_visual_directives` les
//! retire du code envoyé au runner Node pour que l'eval ne casse pas
//! (superdough ne connaît pas ces méthodes).

use std::sync::Arc;

use crate::audio::SpectrumTap;
use crate::pattern_ir::PatternDocument;

/// Noms reconnus (Strudel visual API).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VisualKind {
    /// Oscilloscope : forme d'onde live.
    Scope,
    /// Analyseur de spectre FFT.
    Spectrum,
    /// Grille batterie (events par nom de sample).
    Punchcard,
    /// Piano roll (events note → hauteur).
    Pianoroll,
}

impl VisualKind {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "scope" => Some(Self::Scope),
            "spectrum" => Some(Self::Spectrum),
            "punchcard" => Some(Self::Punchcard),
            "pianoroll" => Some(Self::Pianoroll),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Scope => "scope",
            Self::Spectrum => "spectrum",
            Self::Punchcard => "punchcard",
            Self::Pianoroll => "pianoroll",
        }
    }
}

/// Une directive `._nom()` / `.nom()` trouvée dans le code.
#[derive(Clone, Debug)]
pub struct VisualDirective {
    pub kind: VisualKind,
    /// Numéro de ligne 0-based (pour l'infobulle).
    pub line: usize,
    /// true = `._nom()` (inline Strudel), false = `.nom()`.
    pub underscore: bool,
    /// Plage octets [start, end) du texte `._nom()` à retirer pour l'eval.
    pub span: (usize, usize),
}

/// Données live nécessaires au rendu des visuels.
pub struct VisualsData<'a> {
    pub spectrum: Option<Arc<SpectrumTap>>,
    pub doc: Option<&'a PatternDocument>,
    pub current_cycle: f64,
    pub playing: bool,
    /// Taux d'échantillonnage du flux de sortie (échelle spectre).
    pub sample_rate_hz: f32,
}

/// État du scanner : on saute strings et commentaires pour ne pas
/// confondre `s("._scope()")` ou `// ._scope()` avec une vraie directive.
#[derive(Clone, Copy, PartialEq)]
enum ScanState {
    Normal,
    LineComment,
    BlockComment,
    /// Quote ouvrant : b'"', b'\'' ou b'`' (template = backtick).
    Str(u8),
}

/// Trouve les directives visuelles hors strings/commentaires.
pub fn parse_visual_directives(code: &str) -> Vec<VisualDirective> {
    let bytes = code.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut state = ScanState::Normal;
    let mut line = 0usize;

    while i < bytes.len() {
        let c = bytes[i];
        let next = bytes.get(i + 1).copied().unwrap_or(0);

        match state {
            ScanState::LineComment => {
                if c == b'\n' {
                    state = ScanState::Normal;
                    line += 1;
                }
                i += 1;
                continue;
            }
            ScanState::BlockComment => {
                if c == b'*' && next == b'/' {
                    state = ScanState::Normal;
                    i += 2;
                } else {
                    if c == b'\n' {
                        line += 1;
                    }
                    i += 1;
                }
                continue;
            }
            ScanState::Str(q) => {
                if c == b'\\' {
                    i += 2;
                    continue;
                }
                if c == b'\n' {
                    line += 1;
                }
                if c == q {
                    state = ScanState::Normal;
                }
                i += 1;
                continue;
            }
            ScanState::Normal => {}
        }

        // Transitions Normal
        if c == b'/' && next == b'/' {
            state = ScanState::LineComment;
            i += 2;
            continue;
        }
        if c == b'/' && next == b'*' {
            state = ScanState::BlockComment;
            i += 2;
            continue;
        }
        if c == b'"' || c == b'\'' || c == b'`' {
            state = ScanState::Str(c);
            i += 1;
            continue;
        }
        if c == b'\n' {
            line += 1;
            i += 1;
            continue;
        }

        // Cherche `.` [`_`] nom `(` `)` en Normal
        if c == b'.' {
            let mut j = i + 1;
            let underscore = bytes.get(j) == Some(&b'_');
            if underscore {
                j += 1;
            }
            let name_start = j;
            while j < bytes.len() && (bytes[j].is_ascii_alphabetic()) {
                j += 1;
            }
            let name = &code[name_start..j];
            if let Some(kind) = VisualKind::from_name(name) {
                // `(` puis espaces puis `)` obligatoire
                let mut k = j;
                while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                    k += 1;
                }
                if bytes.get(k) == Some(&b'(') {
                    let mut m = k + 1;
                    while m < bytes.len() && bytes[m].is_ascii_whitespace() {
                        m += 1;
                    }
                    if bytes.get(m) == Some(&b')') {
                        out.push(VisualDirective {
                            kind,
                            line,
                            underscore,
                            span: (i, m + 1),
                        });
                        i = m + 1;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }
    out
}

/// Retire les directives visuelles du code destiné au runner Node.
/// Les commentaires/strings sont préservés tels quels (le scanner les saute,
/// donc un `._scope()` dans une string n'est jamais touché).
pub fn strip_visual_directives(code: &str) -> String {
    let directives = parse_visual_directives(code);
    if directives.is_empty() {
        return code.to_string();
    }
    let mut out = String::with_capacity(code.len());
    let mut last = 0usize;
    for d in &directives {
        if d.span.0 < last {
            continue; // chevauchement défensif
        }
        out.push_str(&code[last..d.span.0]);
        last = d.span.1;
    }
    out.push_str(&code[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_inline_and_plain_visuals() {
        let code = r#"s("bd hh").bank("x")._scope()
note("c3")._pianoroll().gain(0.5)
.spectrum()"#;
        let v = parse_visual_directives(code);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].kind, VisualKind::Scope);
        assert!(v[0].underscore);
        assert_eq!(v[0].line, 0);
        assert_eq!(v[1].kind, VisualKind::Pianoroll);
        assert_eq!(v[2].kind, VisualKind::Spectrum);
        assert!(!v[2].underscore);
        assert_eq!(v[2].line, 2);
    }

    #[test]
    fn ignores_strings_comments_and_args() {
        let code = r#"s("._scope()") // ._spectrum()
/* ._punchcard() */
._scope({ color: "red" })"#;
        let v = parse_visual_directives(code);
        assert!(v.is_empty(), "{v:?}");
    }

    #[test]
    fn strip_removes_calls_keeps_audio() {
        let code = r#"s("bd")._scope().gain(0.5).bank("x")"#;
        assert_eq!(
            strip_visual_directives(code),
            r#"s("bd").gain(0.5).bank("x")"#
        );
        let code2 = r#"s("._scope()")._spectrum()"#;
        assert_eq!(strip_visual_directives(code2), r#"s("._scope()")"#);
    }
}
