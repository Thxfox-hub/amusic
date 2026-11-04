//! Strudel-редактор: синтаксис Mini-нотации и автодополнение (как в @strudel/codemirror).

use egui_code_editor::{ColorTheme, Syntax, DEFAULT_THEMES};
use std::collections::BTreeSet;

#[allow(unused_imports)]
use crate::doc_json::DocStore;

/// Тема редактора по имени (из настроек).
pub fn theme_from_name(name: &str) -> ColorTheme {
    let n = name.trim().to_lowercase();
    for t in DEFAULT_THEMES {
        if t.name.to_lowercase() == n {
            return t;
        }
    }
    ColorTheme::GRUVBOX
}

/// Контексты для автодополнения (pitch, chord, scale, mode, sound) — статические списки как в autocomplete.mjs.
pub const PITCH_NAMES: &[&str] = &[
    "C", "C#", "Db", "D", "D#", "Eb", "E", "E#", "Fb", "F", "F#", "Gb", "G", "G#", "Ab", "A", "A#",
    "Bb", "B", "B#", "Cb",
];

pub const MODE_COMPLETIONS: &[&str] = &["below", "above", "duck", "root"];

/// Символы аккордов (major = пустая строка в chord()) — как в @strudel/tonal complex.
pub const CHORD_SYMBOLS: &[&str] = &[
    "major", "minor", "dim", "dim7", "aug", "maj7", "m7", "7", "sus2", "sus4", "m6", "6", "m9",
    "9", "m11", "11", "13", "add9", "m7b5", "7#5", "7b5",
];

/// Синтаксис для Strudel: комментарии // и /* */, ключевые слова, полный API из doc.json при наличии.
pub fn strudel_syntax(doc_store: Option<&DocStore>) -> Syntax {
    let mut keywords = BTreeSet::new();
    for &w in &[
        "const",
        "let",
        "var",
        "function",
        "return",
        "if",
        "else",
        "for",
        "while",
        "true",
        "false",
        "null",
        "undefined",
        "async",
        "await",
        "in",
        "of",
    ] {
        keywords.insert(w);
    }
    for &w in PITCH_NAMES {
        keywords.insert(w);
    }
    for &w in MODE_COMPLETIONS {
        keywords.insert(w);
    }
    for &w in CHORD_SYMBOLS {
        keywords.insert(w);
    }
    // Полный список из doc.json требует 'static; используем статический набор как в REPL.
    for &w in &[
        "s",
        "sound",
        "note",
        "scale",
        "chord",
        "degree",
        "struct",
        "stack",
        "layer",
        "append",
        "cat",
        "slowcat",
        "fastcat",
        "slow",
        "fast",
        "rev",
        "jux",
        "superimpose",
        "chunk",
        "splice",
        "segment",
        "split",
        "when",
        "sometimes",
        "sometimesBy",
        "off",
        "offadd",
        "ply",
        "iter",
        "stutter",
        "echo",
        "hurry",
        "degrade",
        "n",
        "gain",
        "speed",
        "vowel",
        "pan",
        "room",
        "size",
        "delay",
        "delaytime",
        "cutoff",
        "resonance",
        "lpf",
        "hpf",
        "crush",
        "coarse",
        "shape",
        "tri",
        "sine",
        "saw",
        "square",
        "rand",
        "randir",
        "perlin",
        "cosine",
        "mask",
        "inv",
        "pick",
        "choose",
        "wchoose",
        "slider",
        "toggler",
        "markcss",
        "bank",
        "mode",
        "voicing",
        "mini",
        "run",
        "setcps",
        "getcps",
        "solo",
        "mute",
        "silence",
        "seq",
        "seqCycles",
        "poly",
        "polyR",
        "listToPat",
        "listPat",
        "sig",
        "sig2",
        "sig3",
        "sig4",
        "substruct",
        "stripe",
        "striate",
        "early",
        "late",
        "rot",
        "rotL",
        "rotR",
        "palindrome",
        "spread",
        "spreadChoose",
        "scramble",
        "shuffle",
        "fadeIn",
        "fadeOut",
        "fade",
        "range",
        "rangex",
    ] {
        keywords.insert(w);
    }
    let _ = doc_store; // для будущего: можно генерировать статический список из doc.json

    let mut special = BTreeSet::new();
    for &w in &[
        "bd", "sd", "hh", "cp", "lt", "mt", "ht", "rim", "noise", "cowbell", "tom", "kick",
        "snare", "clap", "open", "closed",
    ] {
        special.insert(w);
    }

    Syntax {
        language: "strudel",
        case_sensitive: false,
        comment: "//",
        comment_multiline: ["/*", "*/"],
        hyperlinks: BTreeSet::new(),
        keywords,
        types: BTreeSet::new(),
        special,
        quotes: BTreeSet::new(),
    }
}
