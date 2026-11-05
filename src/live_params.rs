//! Paramètres live : lecture et réécriture des méthodes `.gain(…)`, `.room(…)`, etc.
//! attachées aux appels `s("…")` d'un motif.

/// Paramètres réglables au slider. Seuls ceux que le moteur lit réellement (évènement audio).
#[derive(Debug, Clone, Copy)]
pub struct LiveParam {
    pub method: &'static str,
    pub label: &'static str,
    pub min: f32,
    pub max: f32,
    /// Valeur affichée quand la méthode est absente (= valeur utilisée par le moteur).
    pub default: f32,
    /// Slider en demi-tons ; le code écrit le facteur `2^(st/12)` (ex. `.speed(2)` pour +12).
    pub semitones: bool,
}

pub const LIVE_PARAMS: &[LiveParam] = &[
    LiveParam { method: "gain", label: "gain", min: 0.0, max: 2.0, default: 0.8, semitones: false },
    LiveParam { method: "pan", label: "pan (gauche/droite)", min: -1.0, max: 1.0, default: 0.0, semitones: false },
    LiveParam { method: "room", label: "room (réverbération)", min: 0.0, max: 1.0, default: 0.0, semitones: false },
    LiveParam { method: "shape", label: "shape (saturation)", min: 0.0, max: 1.0, default: 0.0, semitones: false },
    LiveParam { method: "speed", label: "pitch (demi-tons)", min: -12.0, max: 12.0, default: 0.0, semitones: true },
    LiveParam { method: "attack", label: "crescendo — attack (s)", min: 0.0, max: 2.0, default: 0.0, semitones: false },
    LiveParam { method: "decay", label: "decay (s)", min: 0.0, max: 2.0, default: 0.0, semitones: false },
    LiveParam { method: "sustain", label: "sustain", min: 0.0, max: 1.0, default: 1.0, semitones: false },
    LiveParam { method: "release", label: "absorption — release (s)", min: 0.0, max: 2.0, default: 0.0, semitones: false },
    LiveParam { method: "lpf", label: "filtre passe-bas (Hz)", min: 80.0, max: 16000.0, default: 8000.0, semitones: false },
];

/// Valeur du slider → texte écrit dans le code.
pub fn to_written(p: &LiveParam, slider: f32) -> String {
    if p.semitones {
        let s = format!("{:.4}", 2f32.powf(slider / 12.0));
        let s = s.trim_end_matches('0').trim_end_matches('.');
        if s.is_empty() { "1".to_string() } else { s.to_string() }
    } else {
        fmt_value(slider)
    }
}

/// Valeur écrite dans le code → valeur du slider.
pub fn to_slider(p: &LiveParam, written: f32) -> f32 {
    if p.semitones { 12.0 * written.max(1e-6).log2() } else { written }
}

/// Portée d'un réglage : un seul son, tous les sons d'un type, ou tous les sons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveScope {
    Sound,
    Type,
    All,
}

/// Un appel `s("…")` trouvé dans le code.
#[derive(Debug, Clone, PartialEq)]
pub struct SoundCall {
    /// Index du `s` dans le code.
    pub start: usize,
    /// Index juste après le `)` fermant de `s("…")`.
    pub end: usize,
    /// Fin de la chaîne de méthodes : début de l'appel suivant, ou fin du code.
    pub chain_end: usize,
    /// Libellé lisible : `vox / recording-7` ou `bd*4`.
    pub label: String,
    /// Contenu de `s("…")`, ex. `bd*4` ou `recording-7`.
    pub name: String,
    /// Banque lue dans `.bank("…")`, si présente.
    pub bank: Option<String>,
}

/// Type d'un son : `Synthèse`, `Instruments GM`, `Voix`, ou la catégorie de sa banque.
pub fn sound_type(call: &SoundCall, bank_category: &dyn Fn(&str) -> String) -> String {
    let name = call.name.trim();
    match call.bank.as_deref() {
        Some("vox") | Some("recordings") => "Voix".to_string(),
        Some(bank) => bank_category(bank),
        None if name.starts_with("gm_") => "Instruments GM".to_string(),
        None if SYNTH_NAMES.contains(&name) => "Synthèse".to_string(),
        None => bank_category(""),
    }
}

/// Types présents dans le code, dans l'ordre d'apparition, sans doublon.
pub fn types_in(calls: &[SoundCall], bank_category: &dyn Fn(&str) -> String) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in calls {
        let t = sound_type(c, bank_category);
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// Une option ajoutable à un appel `s("…")` depuis le bouton « + » de l'éditeur :
/// le fragment est inséré sur sa propre ligne sous l'appel, avec un widget
/// `slider(…)` inline pour ajuster la valeur en direct.
#[derive(Debug, Clone, Copy)]
pub struct SoundOption {
    /// Label FR affiché dans le menu.
    pub label: &'static str,
    /// Fragment chaîné inséré (`.method(slider(v,min,max))…`).
    pub code: &'static str,
}

/// Catalogue personnalisé — chaque entrée doit correspondre à un vrai
/// paramètre superdough (sinon le contrôle serait muet). Pas de « noise gate »
/// : inexistant dans Strudel (`.gate` = durée, pas un seuil).
pub const SOUND_OPTIONS: &[SoundOption] = &[
    SoundOption { label: "Pitch (demi-tons)", code: ".speed(slider(1.156,0.25,4))" },
    SoundOption { label: "Bitcrush – fréquence", code: ".coarse(slider(5,1,96))" },
    SoundOption { label: "Bitcrush – bits", code: ".crush(slider(8,1,16))" },
    SoundOption { label: "Saturation", code: ".shape(slider(0.4,0,1))" },
    SoundOption { label: "Compression – seuil", code: ".compressor(slider(-32,-60,0))" },
    SoundOption { label: "Compression – ratio", code: ".compressor(slider(-32,-60,0)).compressorRatio(slider(10,1,20))" },
    SoundOption { label: "Coupe-bas", code: ".hpf(slider(90,20,2000))" },
    SoundOption { label: "Filtre passe-bas", code: ".lpf(slider(2000,200,8000))" },
    SoundOption { label: "Crescendo début", code: ".attack(slider(0.25,0,3))" },
    SoundOption { label: "Decrescendo fin", code: ".release(slider(0.35,0,3))" },
    SoundOption { label: "Montée de pitch au début", code: ".penv(slider(3,-24,24)).pattack(slider(0.05,0.001,0.5))" },
    SoundOption { label: "Chute de pitch à la fin", code: ".penv(slider(-3,-24,24)).prelease(slider(0.1,0.001,1))" },
    SoundOption { label: "Volume", code: ".gain(slider(1,0,2))" },
    SoundOption { label: "Panoramique", code: ".pan(slider(0,-1,1))" },
    // Enchaînement : empêcher les voix de se chevaucher sur un son long.
    SoundOption { label: "Une voix à la fois — espacement (cycles)", code: ".slow(slider(4,1,128))" },
    SoundOption { label: "Une voix à la fois — couper la précédente", code: ".cut(slider(1,0,8))" },
    SoundOption { label: "Caler le sample sur N cycles", code: ".loopAt(slider(4,1,64))" },
];

/// Point d'insertion d'une option chaînée : fin de la ligne de l'appel `s("…")`,
/// prolongée tant que les lignes suivantes continuent la chaîne (commencent
/// par `.`). L'option est insérée sur sa propre ligne juste après.
pub fn chain_insert_point(code: &str, call_end: usize) -> usize {
    let mut at = line_end(code, call_end);
    loop {
        let next = at + 1; // saute le '\n'
        if next >= code.len() {
            break;
        }
        let nl = line_end(code, next);
        if code[next..nl].trim_start().starts_with('.') {
            at = nl;
        } else {
            break;
        }
    }
    at
}

fn line_end(code: &str, from: usize) -> usize {
    code[from..]
        .find('\n')
        .map(|r| from + r)
        .unwrap_or(code.len())
}

/// Liste les appels `s("…")` dans l'ordre du code.
pub fn find_sound_calls(code: &str) -> Vec<SoundCall> {
    const OPEN: &str = "s(\"";
    let bytes = code.as_bytes();
    let mut found: Vec<(usize, usize, String)> = Vec::new();
    let mut from = 0;
    while let Some(off) = code[from..].find(OPEN) {
        let start = from + off;
        from = start + OPEN.len();
        let prev_is_ident = start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_');
        if prev_is_ident {
            continue;
        }
        let content_start = start + OPEN.len();
        let Some(rel) = code[content_start..].find('"') else { continue };
        let content_end = content_start + rel;
        if bytes.get(content_end + 1) != Some(&b')') {
            continue;
        }
        found.push((start, content_end + 2, code[content_start..content_end].to_string()));
    }
    let starts: Vec<usize> = found.iter().map(|(s, _, _)| *s).chain(std::iter::once(code.len())).collect();
    found
        .iter()
        .enumerate()
        .map(|(i, (start, end, inner))| {
            let chain_end = starts[i + 1];
            let bank = method_span(code, *end, chain_end, "bank").and_then(|(_, args, close)| {
                code.get(args..close).map(|a| a.trim().trim_matches('"').to_string())
            });
            let label = match &bank {
                Some(b) => format!("{b} / {inner}"),
                None => inner.clone(),
            };
            SoundCall { start: *start, end: *end, chain_end, label, name: inner.clone(), bank }
        })
        .collect()
}

/// Cherche `.method(` dans `code[from..to]`. Renvoie (index du point, début des arguments, index de la `)` fermante).
fn method_span(code: &str, from: usize, to: usize, method: &str) -> Option<(usize, usize, usize)> {
    let pat = format!(".{method}(");
    let window = code.get(from..to)?;
    let dot = from + window.find(&pat)?;
    let args = dot + pat.len();
    let close = matching_paren(code, args)?;
    Some((dot, args, close))
}

/// Index de la `)` qui ferme la parenthèse ouverte juste avant `args`, en ignorant les chaînes.
fn matching_paren(code: &str, args: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut quote: Option<u8> = None;
    for (i, &b) in code.as_bytes().iter().enumerate().skip(args) {
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None => match b {
                b'"' | b'\'' | b'`' => quote = Some(b),
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            },
        }
    }
    None
}

/// Valeur numérique actuelle de `.method(…)` sur cet appel, si elle est écrite en nombre.
pub fn read_method_value(code: &str, call: &SoundCall, method: &str) -> Option<f32> {
    let (_, args, close) = method_span(code, call.end, call.chain_end, method)?;
    let inner = code.get(args..close)?.trim();
    // Widget inline : `.lpf(slider(4507.2,600,8000))` / `.mask(toggler(1))` —
    // la valeur lue est le premier argument du widget.
    if let Some(rest) = inner
        .strip_prefix("slider(")
        .or_else(|| inner.strip_prefix("toggler("))
    {
        let end = rest.find([',', ')'])?;
        return rest[..end].trim().parse::<f32>().ok();
    }
    inner.parse::<f32>().ok()
}

/// Valeur affichée pour une portée : moyenne des valeurs écrites sur les appels retenus (dans l'unité du slider).
/// `None` si aucun appel retenu n'écrit cette méthode en nombre (le slider prend alors la valeur par défaut).
pub fn aggregate_value(
    code: &str,
    calls: &[SoundCall],
    p: &LiveParam,
    keep: &dyn Fn(&SoundCall) -> bool,
) -> Option<f32> {
    let vals: Vec<f32> = calls
        .iter()
        .filter(|c| keep(c))
        .filter_map(|c| read_method_value(code, c, p.method))
        .map(|w| to_slider(p, w))
        .collect();
    if vals.is_empty() {
        None
    } else {
        Some(vals.iter().sum::<f32>() / vals.len() as f32)
    }
}

/// Met la méthode à `value` (ou la retire si `value` est `None`) sur un seul appel.
/// Si la méthode n'existe pas encore, elle est ajoutée juste après `s("…")`.
pub fn set_method(code: &str, call: &SoundCall, method: &str, value: Option<&str>) -> String {
    match (method_span(code, call.end, call.chain_end, method), value) {
        (Some((_, args, close)), Some(v)) => {
            // Un widget `slider(`/`toggler(` en argument n'est pas écrasé par le
            // réglage panneau : on ne réécrit que son premier argument.
            let args_text = &code[args..close];
            let inner = args_text.trim_start();
            let inner_off = args + (args_text.len() - inner.len());
            if inner.starts_with("slider(") || inner.starts_with("toggler(") {
                let wargs = inner_off + inner.find('(').unwrap_or(0) + 1;
                if let Some(wclose) = matching_paren(code, wargs) {
                    if let Some(&(vs, ve)) = split_args(code, wargs, wclose).first() {
                        return format!("{}{}{}", &code[..vs], v, &code[ve..]);
                    }
                }
            }
            format!("{}{}{}", &code[..args], v, &code[close..])
        }
        (Some((dot, _, close)), None) => format!("{}{}", &code[..dot], &code[close + 1..]),
        (None, Some(v)) => format!("{}.{method}({v}){}", &code[..call.end], &code[call.end..]),
        (None, None) => code.to_string(),
    }
}

/// Applique la même méthode à tous les appels `s("…")` retenus par `keep`.
/// On part de la fin pour que les positions des appels précédents restent valides.
pub fn set_method_where(code: &str, method: &str, value: Option<&str>, keep: &dyn Fn(&SoundCall) -> bool) -> String {
    let calls = find_sound_calls(code);
    calls
        .iter()
        .rev()
        .filter(|c| keep(c))
        .fold(code.to_string(), |acc, call| set_method(&acc, call, method, value))
}

/// Applique la même méthode à tous les appels `s("…")` du code.
pub fn set_method_all(code: &str, method: &str, value: Option<&str>) -> String {
    set_method_where(code, method, value, &|_| true)
}

/// Sorte de widget inline détecté dans le code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineWidgetKind {
    /// `slider(v, min, max)` — piste draggable.
    Slider,
    /// `toggler(v)` — case on/off ; typiquement `.mask(toggler(1))` pour muter un son.
    Toggler,
}

/// Un widget `slider(v, min, max)` ou `toggler(v)` écrit dans le code (syntaxe strudel inline).
#[derive(Debug, Clone, PartialEq)]
pub struct InlineWidget {
    pub kind: InlineWidgetKind,
    /// Début de l'appel `slider(`/`toggler(` (position du premier caractère).
    pub call_start: usize,
    /// Fin de l'appel (juste après la `)` fermante).
    pub call_end: usize,
    /// Début de l'argument « valeur » dans le code (span déjà trimé).
    pub value_start: usize,
    /// Fin de l'argument « valeur ».
    pub value_end: usize,
    /// Méthode qui contient le widget (`.lpf(…)` → `"lpf"`) ; nom du widget si racine.
    pub context: String,
    /// Valeur lue dans le code.
    pub value: f32,
    pub min: f32,
    pub max: f32,
}

/// Liste les appels `slider(…)`/`toggler(…)` dont le premier argument est un nombre.
/// `x.slider(…)` (appel membre), les occurrences dans une chaîne et celles dans un
/// commentaire `//` ou `/* */` sont ignorés.
pub fn find_inline_widgets(code: &str) -> Vec<InlineWidget> {
    const TAGS: &[(&[u8], InlineWidgetKind, &str)] = &[
        (b"slider(", InlineWidgetKind::Slider, "slider"),
        (b"toggler(", InlineWidgetKind::Toggler, "toggler"),
    ];
    let bytes = code.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'"' || b == b'\'' || b == b'`' => quote = Some(b),
            // Commentaires JS : aucune chance de contenir un widget.
            None if b == b'/' && bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            None if b == b'/' && bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            None => {
                let Some(&(tag, kind, label)) = TAGS
                    .iter()
                    .find(|(t, _, _)| bytes[i..].starts_with(*t))
                else {
                    i += 1;
                    continue;
                };
                let start = i;
                let args = start + tag.len();
                i = args;
                if start > 0 {
                    let p = bytes[start - 1];
                    if p.is_ascii_alphanumeric() || p == b'_' || p == b'.' || p == b'$' {
                        continue;
                    }
                }
                let Some(close) = matching_paren(code, args) else { continue };
                let parts = split_args(code, args, close);
                let text = |n: usize| parts.get(n).map(|&(a, b)| &code[a..b]);
                let Ok(value) = text(0).unwrap_or("").parse::<f32>() else { continue };
                let Some(&(value_start, value_end)) = parts.first() else { continue };
                let min = text(1).and_then(|t| t.parse::<f32>().ok()).unwrap_or(0.0);
                let max = text(2).and_then(|t| t.parse::<f32>().ok()).unwrap_or(1.0);
                out.push(InlineWidget {
                    kind,
                    call_start: start,
                    call_end: close + 1,
                    value_start,
                    value_end,
                    context: enclosing_method(code, start).unwrap_or_else(|| label.to_string()),
                    value,
                    min,
                    max,
                });
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Réécrit la valeur d'un widget inline (span issu de `find_inline_widgets`), autres args conservés.
pub fn set_inline_widget_value(code: &str, s: &InlineWidget, v: f32) -> String {
    if s.value_start > s.value_end {
        return code.to_string();
    }
    let (Some(head), Some(tail)) = (code.get(..s.value_start), code.get(s.value_end..))
    else {
        return code.to_string();
    };
    format!("{}{}{}", head, fmt_value(v), tail)
}

/// Découpe `code[args..close]` (contenu d'une paire de parenthèses) en arguments au niveau 0.
/// Renvoie les spans absolus, déjà trimés.
fn split_args(code: &str, args: usize, close: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    let mut seg = args;
    for (i, &b) in code.as_bytes()[args..close].iter().enumerate() {
        let i = args + i;
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None => match b {
                b'"' | b'\'' | b'`' => quote = Some(b),
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth = depth.saturating_sub(1),
                b',' if depth == 0 => {
                    out.push(trim_span(code, seg, i));
                    seg = i + 1;
                }
                _ => {}
            },
        }
    }
    out.push(trim_span(code, seg, close));
    out
}

fn trim_span(code: &str, a: usize, b: usize) -> (usize, usize) {
    let (mut a, mut b) = (a, b);
    while a < b && code.as_bytes()[a].is_ascii_whitespace() {
        a += 1;
    }
    while b > a && code.as_bytes()[b - 1].is_ascii_whitespace() {
        b -= 1;
    }
    (a, b)
}

/// Nom de la méthode `.ident(` la plus imbriquée contenant `pos`, s'il y en a une.
fn enclosing_method(code: &str, pos: usize) -> Option<String> {
    let bytes = code.as_bytes();
    let mut found: Option<String> = None;
    for i in 0..pos {
        if bytes[i] != b'.' {
            continue;
        }
        let id_start = i + 1;
        let mut id_end = id_start;
        while id_end < pos && (bytes[id_end].is_ascii_alphanumeric() || bytes[id_end] == b'_') {
            id_end += 1;
        }
        if id_end == id_start || bytes.get(id_end) != Some(&b'(') {
            continue;
        }
        if matching_paren(code, id_end + 1).is_some_and(|c| c > pos) {
            found = Some(code[id_start..id_end].to_string());
        }
    }
    found
}

/// Format court d'un réglage : `0.8`, `1`, `-0.5`.
pub fn fmt_value(v: f32) -> String {
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" || s == "-0" {
        "0".to_string()
    } else {
        s.to_string()
    }
}

/// Noms des synthés natifs (même liste que `audio::SYNTH_NAMES`).
pub const SYNTH_NAMES: &[&str] = &[
    "sine", "cosine", "sawtooth", "saw", "square", "triangle", "tri", "pink", "white", "brown", "crackle",
    "supersaw", "hoover", "pulse", "subbass", "fmsine",
    "synthbd", "synthsd", "synthhh", "synthoh", "synthcp",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn first(code: &str) -> SoundCall {
        find_sound_calls(code).remove(0)
    }

    fn cat(bank: &str) -> String {
        match bank {
            "tidal-drum-machines" | "RolandTR909" => "Batterie".to_string(),
            "" => "Autres".to_string(),
            other => other.to_string(),
        }
    }

    #[test]
    fn classifies_sound_types() {
        let code = r#"stack(s("sine"), s("gm_piano"), s("bd*4").bank("RolandTR909"), s("recording-7").bank("vox"), s("x").bank("pad"))"#;
        let calls = find_sound_calls(code);
        let types: Vec<String> = calls.iter().map(|c| sound_type(c, &cat)).collect();
        assert_eq!(types, ["Synthèse", "Instruments GM", "Batterie", "Voix", "pad"]);
        assert_eq!(types_in(&calls, &cat).len(), 5);
    }

    #[test]
    fn aggregate_is_mean_of_written_values_in_scope() {
        let code = r#"stack(s("bd").gain(0.4), s("sd").gain(0.8), s("sine").gain(1.0))"#;
        let calls = find_sound_calls(code);
        assert_eq!(aggregate_value(code, &calls, &LIVE_PARAMS[0], &|c| c.name != "sine"), Some(0.6));
        assert_eq!(aggregate_value(code, &calls, &LIVE_PARAMS[2], &|_| true), None);
    }

    #[test]
    fn filters_by_type_only() {
        let code = r#"stack(s("bd*4").bank("RolandTR909"), s("sine"), s("hh*8").bank("RolandTR909"))"#;
        let out = set_method_where(code, "gain", Some("0.5"), &|c| sound_type(c, &cat) == "Batterie");
        assert_eq!(
            out,
            r#"stack(s("bd*4").gain(0.5).bank("RolandTR909"), s("sine"), s("hh*8").gain(0.5).bank("RolandTR909"))"#
        );
    }

    #[test]
    fn finds_calls_with_bank_label() {
        let calls = find_sound_calls(r#"stack(s("bd*4"), s("recording-7").bank("vox"))"#);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].label, "bd*4");
        assert_eq!(calls[1].label, "vox / recording-7");
    }

    #[test]
    fn ignores_identifiers_ending_in_s() {
        assert!(find_sound_calls(r#"yeas("bd")"#).is_empty());
        assert_eq!(find_sound_calls(r#"s("bd(3,8)")"#)[0].label, "bd(3,8)");
    }

    #[test]
    fn inserts_missing_method_after_the_call() {
        let code = r#"s("bd*4").bank("x")"#;
        assert_eq!(set_method(code, &first(code), "gain", Some("0.5")), r#"s("bd*4").gain(0.5).bank("x")"#);
    }

    #[test]
    fn replaces_existing_method_only() {
        let code = r#"s("bd").gain(0.8).room(0.2)"#;
        assert_eq!(set_method(code, &first(code), "gain", Some("0.3")), r#"s("bd").gain(0.3).room(0.2)"#);
        assert_eq!(read_method_value(code, &first(code), "gain"), Some(0.8));
        assert_eq!(read_method_value(code, &first(code), "pan"), None);
    }

    #[test]
    fn removes_method() {
        let code = r#"s("bd").gain(0.8).room(0.2)"#;
        assert_eq!(set_method(code, &first(code), "gain", None), r#"s("bd").room(0.2)"#);
    }

    #[test]
    fn non_numeric_argument_is_not_read_but_can_be_replaced() {
        let code = r#"s("bd").gain(sine.range(0.1, 0.9))"#;
        assert_eq!(read_method_value(code, &first(code), "gain"), None);
        assert_eq!(set_method(code, &first(code), "gain", Some("1")), r#"s("bd").gain(1)"#);
    }

    #[test]
    fn applies_to_all_calls_without_shifting_positions() {
        let code = r#"stack(s("bd"), s("sd").gain(0.4), s("hh").bank("x"))"#;
        assert_eq!(
            set_method_all(code, "gain", Some("0.7")),
            r#"stack(s("bd").gain(0.7), s("sd").gain(0.7), s("hh").gain(0.7).bank("x"))"#
        );
    }

    #[test]
    fn pitch_slider_maps_to_speed_factor() {
        let p = LIVE_PARAMS.iter().find(|p| p.method == "speed").unwrap();
        assert_eq!(to_written(p, 12.0), "2");
        assert_eq!(to_written(p, 0.0), "1");
        assert_eq!(to_written(p, -12.0), "0.5");
        assert!((to_slider(p, 0.5) + 12.0).abs() < 1e-3);
        let code = r#"s("sine")"#;
        let call = first(code);
        assert_eq!(set_method(code, &call, "speed", Some(&to_written(p, 7.0))), r#"s("sine").speed(1.4983)"#);
    }

    #[test]
    fn formats_values_compactly() {
        assert_eq!(fmt_value(0.8), "0.8");
        assert_eq!(fmt_value(1.0), "1");
        assert_eq!(fmt_value(-0.5), "-0.5");
        assert_eq!(fmt_value(0.0), "0");
    }

    #[test]
    fn finds_inline_sliders_with_enclosing_method() {
        let code = r#"s("bd").lpf(slider(4507.2,600,8000))"#;
        let s = find_inline_widgets(code);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].kind, InlineWidgetKind::Slider);
        assert_eq!(s[0].context, "lpf");
        assert_eq!(s[0].value, 4507.2);
        assert_eq!((s[0].min, s[0].max), (600.0, 8000.0));
        assert_eq!(&code[s[0].value_start..s[0].value_end], "4507.2");
    }

    #[test]
    fn inline_slider_defaults_and_nested_contexts() {
        let code = r#"stack(note("c3").lpf(slider(0.5)), s("bd").gain(slider(0.9,0,2)))"#;
        let s = find_inline_widgets(code);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].context, "lpf");
        assert_eq!((s[0].min, s[0].max), (0.0, 1.0));
        assert_eq!(s[1].context, "gain");
        assert_eq!((s[1].min, s[1].max), (0.0, 2.0));
        assert_eq!(find_inline_widgets("slider(2)")[0].context, "slider");
    }

    #[test]
    fn finds_toggler_widgets() {
        let code = r#"s("bd").gain(slider(0.8,0,1.5)).mask(toggler(1))"#;
        let s = find_inline_widgets(code);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].kind, InlineWidgetKind::Slider);
        assert_eq!(s[1].kind, InlineWidgetKind::Toggler);
        assert_eq!(s[1].context, "mask");
        assert_eq!(s[1].value, 1.0);
        assert_eq!((s[1].min, s[1].max), (0.0, 1.0));
        assert_eq!(&code[s[1].value_start..s[1].value_end], "1");
    }

    #[test]
    fn widgets_in_comments_are_ignored() {
        assert!(find_inline_widgets("// slider(1)").is_empty());
        assert!(find_inline_widgets("s(\"bd\") // .lpf(slider(5))").is_empty());
        assert!(find_inline_widgets("/* slider(2) */ s(\"bd\")").is_empty());
        assert!(find_inline_widgets("/* x\nslider(3)\n*/ s(\"bd\")").is_empty());
        let s = find_inline_widgets("s(\"bd\").mask(toggler(1)) // slider(9)");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].kind, InlineWidgetKind::Toggler);
    }

    #[test]
    fn chain_insert_point_single_line() {
        let code = "s(\"bd\").gain(1)\ns(\"hh\")";
        let calls = find_sound_calls(code);
        let at = chain_insert_point(code, calls[0].end);
        assert_eq!(&code[..at], "s(\"bd\").gain(1)");
    }

    #[test]
    fn chain_insert_point_extends_multiline() {
        let code = "s(\"bd\")\n  .gain(1)\n  .lpf(400)\n\ns(\"hh\")";
        let calls = find_sound_calls(code);
        let at = chain_insert_point(code, calls[0].end);
        assert_eq!(&code[..at], "s(\"bd\")\n  .gain(1)\n  .lpf(400)");
    }

    #[test]
    fn chain_insert_point_stops_at_plain_code() {
        let code = "s(\"bd\")\n  .gain(1)\nstack(x)";
        let calls = find_sound_calls(code);
        let at = chain_insert_point(code, calls[0].end);
        assert_eq!(&code[..at], "s(\"bd\")\n  .gain(1)");
    }

    #[test]
    fn member_calls_and_non_numeric_are_not_widgets() {
        assert!(find_inline_widgets("x.slider(1)").is_empty());
        assert!(find_inline_widgets("fooslider(1)").is_empty());
        assert!(find_inline_widgets("x.toggler(1)").is_empty());
        assert!(find_inline_widgets(r#"s("bd").lpf(slider(sine,0,1))"#).is_empty());
        assert!(find_inline_widgets("s(\"bd\").lpf(slider()").is_empty());
        assert!(find_inline_widgets(r#"s("bd slider(2)")"#).is_empty());
    }

    #[test]
    fn rewrites_inline_slider_value_only() {
        let code = r#"s("bd").lpf(slider(4507.2,600,8000))"#;
        let s = find_inline_widgets(code).remove(0);
        assert_eq!(
            set_inline_widget_value(code, &s, 1200.0),
            r#"s("bd").lpf(slider(1200,600,8000))"#
        );
    }

    #[test]
    fn param_reads_slider_widget_value() {
        let code = r#"s("bd").lpf(slider(4507.2,600,8000)).gain(0.8)"#;
        assert_eq!(read_method_value(code, &first(code), "lpf"), Some(4507.2));
        assert_eq!(read_method_value(code, &first(code), "gain"), Some(0.8));
        // `.mask(toggler(1))` est lu aussi (mute live).
        let code2 = r#"s("bd").mask(toggler(0))"#;
        assert_eq!(read_method_value(code2, &first(code2), "mask"), Some(0.0));
    }

    #[test]
    fn set_method_preserves_widget_calls() {
        // Le panneau live réécrit arg0 du widget au lieu de détruire slider(/toggler(.
        let code = r#"s("bd").gain(slider(0.8,0,1.5)).mask(toggler(1))"#;
        assert_eq!(
            set_method(code, &first(code), "gain", Some("0.5")),
            r#"s("bd").gain(slider(0.5,0,1.5)).mask(toggler(1))"#
        );
        let code2 = r#"s("bd").mask(toggler(1))"#;
        assert_eq!(
            set_method(code2, &first(code2), "mask", Some("0")),
            r#"s("bd").mask(toggler(0))"#
        );
    }
}
