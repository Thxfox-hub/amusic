use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::settings;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use walkdir::WalkDir;

/// Number of points in downsampled waveform preview (RMS envelope).
const WAVEFORM_PREVIEW_POINTS: usize = 64;

const INDEX_CACHE_FILENAME: &str = ".sample_index.json";

/// IEEE Float subformat GUID (WAVE_FORMAT_EXTENSIBLE), same as hound.
const SUBFMT_IEEE_FLOAT: [u8; 16] = [
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
];

/// In-memory audio sample.
pub struct Sample {
    pub data: Arc<Vec<f32>>, // interleaved channels
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub struct SampleKey {
    pub bank: Option<String>,
    pub name: String,
}

/// Индекс: ключ -> пути к WAV (без загрузки в память).
/// Кэш: уже загруженные сэмплы, подгружаются по требованию при первом воспроизведении.
/// waveform_cache: (SampleKey, variant) -> downsampled RMS envelope для отрисовки в таймлайне.
pub struct SampleLibrary {
    index: HashMap<SampleKey, Vec<PathBuf>>,
    cache: Mutex<HashMap<SampleKey, Vec<Arc<Sample>>>>,
    waveform_cache: Mutex<HashMap<(SampleKey, usize), Arc<[f32]>>>,
}

#[derive(Serialize, Deserialize)]
struct IndexCacheFile {
    root: String,
    entries: Vec<IndexEntry>,
}

#[derive(Serialize, Deserialize)]
struct IndexEntry {
    bank: Option<String>,
    name: String,
    paths: Vec<String>,
}

impl SampleLibrary {
    pub fn new() -> Self {
        Self {
            index: HashMap::new(),
            cache: Mutex::new(HashMap::new()),
            waveform_cache: Mutex::new(HashMap::new()),
        }
    }

    fn from_index(index: HashMap<SampleKey, Vec<PathBuf>>) -> Self {
        Self {
            index,
            cache: Mutex::new(HashMap::new()),
            waveform_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Число ключей (bank, name) в индексе.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Список всех ключей и числа вариантов (для браузера семплов).
    pub fn list_entries(&self) -> Vec<(SampleKey, usize)> {
        self.index
            .iter()
            .map(|(k, paths)| (k.clone(), paths.len()))
            .collect()
    }

    fn lookup_candidates(key: &SampleKey) -> Vec<SampleKey> {
        let name = key.name.to_lowercase();
        let bank = key.bank.as_ref().map(|s| s.to_lowercase());
        let mut candidates = Vec::new();
        let mut push = |bank: Option<&str>| {
            let candidate = SampleKey {
                bank: bank.map(str::to_string),
                name: name.clone(),
            };
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        };

        match bank.as_deref() {
            Some(bank) => {
                push(Some(bank));
                if bank == "tr909" {
                    push(Some("rolandtr909"));
                }
            }
            None => {
                push(Some("rolandtr909"));
                push(Some("rolandtr808"));
                push(None);
            }
        }

        candidates
    }

    /// Поиск сэмпла: сначала кэш, при промахе — загрузка WAV по пути из индекса и сохранение в кэш.
    pub fn lookup(&self, key: &SampleKey, variant: Option<usize>) -> Option<Arc<Sample>> {
        let idx = match variant {
            Some(v) => v.min(usize::MAX),
            None => 0,
        };
        let candidates = Self::lookup_candidates(key);

        // 1) Уже в кэше
        {
            let cache = self.cache.lock().ok()?;
            for candidate in &candidates {
                if let Some(list) = cache.get(candidate) {
                    if idx < list.len() {
                        return Some(Arc::clone(&list[idx]));
                    }
                    if !list.is_empty() {
                        let i = idx.min(list.len().saturating_sub(1));
                        return Some(Arc::clone(&list[i]));
                    }
                }
            }
        }

        // 2) Пути из индекса, включая runtime aliases/default drum-machine fallback.
        let (cache_key, paths) = candidates.iter().find_map(|candidate| {
            self.index
                .get(candidate)
                .cloned()
                .map(|paths| (candidate.clone(), paths))
        })?;

        let need_idx = idx.min(paths.len().saturating_sub(1));

        // 3) Догружаем в кэш все варианты до need_idx включительно, потом возвращаем нужный
        let mut cache = match self.cache.lock() {
            Ok(c) => c,
            Err(_) => return None,
        };
        let list = cache.entry(cache_key).or_default();
        for i in list.len()..=need_idx {
            if let Some(path) = paths.get(i) {
                match load_wav_sample(path) {
                    Ok(sample) => list.push(sample),
                    Err(e) => {
                        eprintln!("[sample_loader] failed to load {:?}: {}", path, e);
                        if i == need_idx {
                            return None;
                        }
                    }
                }
            }
        }
        list.get(need_idx).cloned()
    }

    /// Только кэш, без загрузки. Чтобы не держать lock на время чтения с диска.
    pub fn get_cached(&self, key: &SampleKey, variant: Option<usize>) -> Option<Arc<Sample>> {
        let idx = variant.unwrap_or(0).min(usize::MAX);
        let candidates = Self::lookup_candidates(key);
        let cache = self.cache.lock().ok()?;
        for candidate in &candidates {
            let Some(list) = cache.get(candidate) else {
                continue;
            };
            let i = idx.min(list.len().saturating_sub(1));
            if let Some(sample) = list.get(i).cloned() {
                return Some(sample);
            }
        }
        None
    }

    /// Путь по индексу (без загрузки). Для загрузки вне блокировки state в аудио-движке.
    pub fn get_path(&self, key: &SampleKey, variant: Option<usize>) -> Option<PathBuf> {
        let idx = variant.unwrap_or(0).min(usize::MAX);
        for candidate in Self::lookup_candidates(key) {
            if let Some(paths) = self.index.get(&candidate) {
                return paths.get(idx.min(paths.len().saturating_sub(1))).cloned();
            }
        }
        None
    }

    /// Вставить загруженный сэмпл в кэш (после загрузки вне lock).
    pub fn insert_cached(&self, key: &SampleKey, variant: Option<usize>, sample: Arc<Sample>) {
        let idx = variant.unwrap_or(0).min(usize::MAX);
        let key_norm = SampleKey {
            bank: key.bank.as_ref().map(|s| s.to_lowercase()),
            name: key.name.to_lowercase(),
        };
        if let Ok(mut cache) = self.cache.lock() {
            let list = cache.entry(key_norm).or_default();
            while list.len() <= idx {
                list.push(Arc::clone(&sample));
            }
            list[idx] = sample;
        }
    }

    /// Downsampled waveform preview (RMS envelope) для отрисовки в таймлайне.
    /// Возвращает массив из WAVEFORM_PREVIEW_POINTS значений в диапазоне 0..1.
    pub fn get_waveform_preview(
        &self,
        key: &SampleKey,
        variant: Option<usize>,
    ) -> Option<Arc<[f32]>> {
        let idx = variant.unwrap_or(0).min(usize::MAX);
        let key_norm = SampleKey {
            bank: key.bank.as_ref().map(|s| s.to_lowercase()),
            name: key.name.to_lowercase(),
        };
        let cache_key = (key_norm.clone(), idx);

        // Проверяем кэш волн
        {
            let wc = self.waveform_cache.lock().ok()?;
            if let Some(cached) = wc.get(&cache_key) {
                return Some(Arc::clone(cached));
            }
        }

        // Загружаем сэмпл (может триггернуть load с диска)
        let sample = self.lookup(&key_norm, Some(idx))?;
        let data = sample.data.as_ref();
        let channels = sample.channels as usize;
        if channels == 0 || data.is_empty() {
            return None;
        }

        // Downsample: разбиваем на WAVEFORM_PREVIEW_POINTS блоков, берём RMS по каналам
        let frames = data.len() / channels;
        if frames == 0 {
            return None;
        }
        let mut out = Vec::with_capacity(WAVEFORM_PREVIEW_POINTS);

        for i in 0..WAVEFORM_PREVIEW_POINTS {
            let start = (i * frames / WAVEFORM_PREVIEW_POINTS).min(frames.saturating_sub(1));
            let end = ((i + 1) * frames / WAVEFORM_PREVIEW_POINTS).min(frames);
            let count = (end - start).max(1);

            let mut sum_sq = 0.0f32;
            for f in start..end {
                for c in 0..channels {
                    let v = data[f * channels + c];
                    sum_sq += v * v;
                }
            }
            let rms = (sum_sq / (count * channels) as f32).sqrt();
            out.push(rms.min(1.0));
        }

        let result: Arc<[f32]> = Arc::from(out.into_boxed_slice());
        if let Ok(mut wc) = self.waveform_cache.lock() {
            wc.insert(cache_key, Arc::clone(&result));
        }
        Some(result)
    }
}

/// Публично для загрузки вне lock (audio engine). Читает WAV с диска.
pub fn load_wav_sample(path: &Path) -> Result<Arc<Sample>, String> {
    if is_wav(path) {
        load_wav_sample_impl(path)
    } else {
        load_symphonia_sample(path)
    }
}

/// Décode un fichier non-WAV (mp3, ogg, flac, aiff, m4a/aac, caf…) via symphonia
/// vers le format interne : f32 interleaved à la fréquence native du fichier.
fn load_symphonia_sample(path: &Path) -> Result<Arc<Sample>, String> {
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::errors::Error as SymError;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::default::{get_codecs, get_probe};

    let file = std::fs::File::open(path).map_err(|e| format!("open {:?}: {}", path, e))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(OsStr::to_str) {
        hint.with_extension(ext);
    }
    let mut format = get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| format!("probe {:?}: {}", path, e))?;
    let (track_id, codec_params) = {
        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| format!("no audio track in {:?}", path))?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio().cloned())
            .ok_or_else(|| format!("no audio codec params in {:?}", path))?;
        (track.id, params)
    };
    let mut decoder = get_codecs()
        .make_audio_decoder(&codec_params, &AudioDecoderOptions::default())
        .map_err(|e| format!("decoder {:?}: {}", path, e))?;

    let mut data: Vec<f32> = Vec::new();
    let mut sample_rate = 0u32;
    let mut channels = 0u16;

    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break, // fin de flux
            Err(SymError::ResetRequired) => {
                decoder = get_codecs()
                    .make_audio_decoder(&codec_params, &AudioDecoderOptions::default())
                    .map_err(|e| format!("decoder reset {:?}: {}", path, e))?;
                continue;
            }
            Err(e) => return Err(format!("demux {:?}: {}", path, e)),
        };
        if packet.track_id != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymError::DecodeError(_)) => continue, // paquet corrompu : on saute
            Err(e) => return Err(format!("decode {:?}: {}", path, e)),
        };
        let spec = decoded.spec().clone();
        if spec.channels().count() == 0 {
            continue;
        }
        sample_rate = spec.rate();
        channels = spec.channels().count() as u16;
        decoded.copy_to_vec_interleaved::<f32>(&mut data);
    }

    if channels == 0 || data.is_empty() {
        return Err(format!("no decodable audio in {:?}", path));
    }
    Ok(Arc::new(Sample {
        data: Arc::new(data),
        sample_rate,
        channels,
    }))
}

/// Список корней для загрузки сэмплов: основной + repos GitHub importés
/// (`<data>/repos/*`, un dossier = une racine) + dossiers supplémentaires
/// des réglages IDE.
pub fn sample_roots(main_root: &Path) -> Vec<PathBuf> {
    let mut roots = vec![main_root.to_path_buf()];
    let repos_root = crate::paths::github_repos_root();
    if let Ok(entries) = std::fs::read_dir(&repos_root) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                roots.push(p);
            }
        }
    }
    let extra = settings::load_settings()
        .extra_sample_folders
        .into_iter()
        .map(PathBuf::from);
    roots.extend(extra);
    roots
}

/// Сгенерировать JSON-карту семплов для указанной папки в формате, совместимом со `strudel.json`.
///
/// Правила:
/// - если файл лежит в подпапке `samples/swoop/...`, то ключом будет `\"swoop\"`,
///   а значениями — все относительные пути WAV внутри этой подпапки (включая поддиректории);
/// - если WAV лежит прямо в корне (`samples/boom.wav`), то ключом будет `\"boom\"`,
///   а значением — одиночный путь `\"boom.wav\"`.
pub fn generate_strudel_json_from_folder(root: &Path) -> Value {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();

    for entry in WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_entry(is_scannable_entry)
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        if !is_audio_file(path) {
            continue;
        }
        let rel = match path.strip_prefix(root) {
            Ok(r) => r,
            Err(_) => continue,
        };

        let rel_str = rel.to_string_lossy().replace('\\', "/");

        // Первый компонент пути относительно корня.
        let mut components = rel.components();
        let first = components.next();

        if let Some(first_comp) = first {
            // Если у файла есть родительская папка, используем её как имя звука (swoop, smash, ...).
            if rel.parent().is_some() {
                if let std::path::Component::Normal(os) = first_comp {
                    let key = os.to_string_lossy().to_string();
                    map.entry(key).or_default().push(rel_str);
                }
            } else {
                // WAV лежит прямо в корне — используем имя файла без расширения как ключ.
                let stem = path
                    .file_stem()
                    .and_then(OsStr::to_str)
                    .unwrap_or("")
                    .to_string();
                if !stem.is_empty() {
                    map.entry(stem).or_default().push(rel_str);
                }
            }
        }
    }

    // Преобразуем в объект JSON: имя -> строка или массив.
    let mut obj = serde_json::Map::new();
    let mut keys: Vec<_> = map.keys().cloned().collect();
    keys.sort();
    for key in keys {
        if let Some(mut paths) = map.remove(&key) {
            paths.sort();
            if paths.len() == 1 {
                obj.insert(key, Value::String(paths.remove(0)));
            } else {
                obj.insert(
                    key,
                    Value::Array(paths.into_iter().map(Value::String).collect()),
                );
            }
        }
    }

    Value::Object(obj)
}

/// Удобный хелпер: вернуть pretty-JSON как строку.
pub fn generate_strudel_json_string(root: &Path) -> Result<String, String> {
    let value = generate_strudel_json_from_folder(root);
    serde_json::to_string_pretty(&value).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// JSON manifest–based sample index
// ---------------------------------------------------------------------------

fn percent_decode(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let src = s.as_bytes();
    let mut i = 0;
    while i < src.len() {
        if src[i] == b'%' && i + 2 < src.len() {
            if let (Some(hi), Some(lo)) = (hex_val(src[i + 1]), hex_val(src[i + 2])) {
                bytes.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        bytes.push(src[i]);
        i += 1;
    }
    String::from_utf8(bytes).unwrap_or_else(|_| s.to_string())
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn github_repo_name(base_url: &str) -> Option<String> {
    let trimmed = base_url.trim_end_matches('/');
    let parts: Vec<&str> = trimmed.split('/').collect();
    let gh_idx = parts
        .iter()
        .position(|&p| p == "raw.githubusercontent.com")?;
    parts.get(gh_idx + 2).map(|s| s.to_string())
}

/// Определяет папку пака на диске: пробует `_dir`, имя JSON, регистронезависимый поиск,
/// имя репозитория из `_base`.
fn find_pack_dir(root: &Path, json_stem: &str, json: &Value) -> PathBuf {
    if let Some(dir) = json.get("_dir").and_then(|v| v.as_str()) {
        let p = root.join(dir);
        if p.exists() {
            return p;
        }
    }
    let exact = root.join(json_stem);
    if exact.exists() {
        return exact;
    }
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.eq_ignore_ascii_case(json_stem) {
                return entry.path();
            }
        }
    }
    if let Some(base) = json.get("_base").and_then(|v| v.as_str()) {
        if let Some(repo) = github_repo_name(base) {
            if !repo.eq_ignore_ascii_case(json_stem) {
                let p = root.join(&repo);
                if p.exists() {
                    return p;
                }
            }
        }
    }
    exact
}

/// Sonde la meilleure base locale pour un manifeste : `pack_dir`, ses
/// sous-répertoires, puis `manifest_dir` (racine du dépôt — les chemins des
/// manifestes sont souvent relatifs à elle, ex. Dirt-Samples). Chaque base
/// est notée par le nombre de chemins de `probes` qui y existent vraiment ;
/// la première base avec le meilleur score gagne (égalité → priorité d'ordre).
fn detect_local_base(
    pack_dir: &Path,
    manifest_dir: &Path,
    probes: &[String],
) -> PathBuf {
    if probes.is_empty() {
        return pack_dir.to_path_buf();
    }
    let mut bases: Vec<PathBuf> = vec![pack_dir.to_path_buf()];
    if manifest_dir != pack_dir {
        bases.push(manifest_dir.to_path_buf());
    }
    // Sous-répertoires directs de chaque base (ex. `machines/`).
    for base in bases.clone() {
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    bases.push(p);
                }
            }
        }
    }
    let mut best: Option<(usize, PathBuf)> = None;
    for base in bases {
        let score = probes
            .iter()
            .filter(|p| base.join(percent_decode(p)).is_file())
            .count();
        if score == 0 {
            continue;
        }
        if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, base));
        }
    }
    best.map(|(_, b)| b).unwrap_or_else(|| pack_dir.to_path_buf())
}

/// Extensions de fichiers audio que le loader sait décoder (WAV via le parseur
/// maison, tout le reste via symphonia).
pub(crate) fn is_audio_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(OsStr::to_str)
            .map(|s| s.to_ascii_lowercase()),
        Some(ext)
            if matches!(
                ext.as_str(),
                "wav" | "wave" | "mp3" | "ogg" | "oga" | "flac" | "aif" | "aiff" | "m4a" | "aac"
                    | "caf" | "mp4"
            )
    )
}

fn collect_rel_paths_from_value(value: &Value) -> Vec<String> {
    match value {
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        Value::Object(obj) => obj
            .values()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

fn find_manifest_files(root: &Path) -> Vec<PathBuf> {
    let mut manifests = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return manifests,
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let name = match p.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !name.ends_with(".json") {
            continue;
        }
        if name.starts_with('.')
            || name.ends_with("-alias.json")
            || name == "strudel-failed-downloads.json"
        {
            continue;
        }
        manifests.push(p);
    }
    manifests
}

/// Строит индекс сэмплов из JSON-манифестов (`*.json` с полем `_base`), лежащих в `root`.
/// Возвращает пустую `HashMap`, если ни одного манифеста не найдено.
fn build_sample_index_from_manifests(root: &Path) -> HashMap<SampleKey, Vec<PathBuf>> {
    let mut index: HashMap<SampleKey, Vec<PathBuf>> = HashMap::new();

    let mut alias_map: HashMap<String, String> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let p = entry.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if name.ends_with("-alias.json") && p.is_file() {
                if let Ok(data) = std::fs::read_to_string(&p) {
                    if let Ok(aliases) = serde_json::from_str::<HashMap<String, String>>(&data) {
                        alias_map.extend(aliases);
                    }
                }
            }
        }
    }

    let manifests = find_manifest_files(root);
    for manifest_path in &manifests {
        let json_stem = manifest_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();

        let data = match std::fs::read_to_string(manifest_path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let json: Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if json.get("_base").and_then(|v| v.as_str()).is_none() {
            continue;
        }

        let pack_dir = find_pack_dir(root, &json_stem, &json);
        let obj = match json.as_object() {
            Some(o) => o,
            None => continue,
        };

        // Sondes : premier chemin de chaque clé (max 12) pour trouver la
        // bonne base sans se fier à un seul fichier peut-être absent.
        let mut probes: Vec<String> = Vec::new();
        for (key, value) in obj.iter() {
            if key.starts_with('_') {
                continue;
            }
            if let Some(p) = collect_rel_paths_from_value(value).into_iter().next() {
                probes.push(p);
                if probes.len() >= 12 {
                    break;
                }
            }
        }
        let local_base = detect_local_base(&pack_dir, root, &probes);

        for (key, value) in obj.iter() {
            if key.starts_with('_') {
                continue;
            }
            let rel_paths = collect_rel_paths_from_value(value);
            let resolved: Vec<PathBuf> = rel_paths
                .iter()
                .map(|rp| local_base.join(percent_decode(rp)))
                // is_file() : un manifeste peut lister des fichiers absents
                // (ex. pack partiel) — ne jamais indexer de chemin fantôme.
                .filter(|p| p.is_file() && is_audio_file(p))
                .collect();
            if resolved.is_empty() {
                continue;
            }

            let key_lc = key.to_lowercase();

            let primary = SampleKey {
                bank: None,
                name: key_lc.clone(),
            };
            index
                .entry(primary)
                .or_default()
                .extend(resolved.iter().cloned());

            if let Some(sep) = key_lc.find('_') {
                let bank = key_lc[..sep].to_string();
                let name = key_lc[sep + 1..].to_string();
                if !name.is_empty() {
                    let split_key = SampleKey {
                        bank: Some(bank.clone()),
                        name: name.clone(),
                    };
                    index
                        .entry(split_key)
                        .or_default()
                        .extend(resolved.iter().cloned());

                    let orig_bank = &key[..key.find('_').unwrap_or(key.len())];
                    if let Some(alias) = alias_map.get(orig_bank) {
                        let alias_key = SampleKey {
                            bank: Some(alias.to_lowercase()),
                            name,
                        };
                        index
                            .entry(alias_key)
                            .or_default()
                            .extend(resolved.iter().cloned());
                    }
                }
            }
        }
        eprintln!(
            "[sample_loader] manifest {:?}: pack_dir={:?}",
            manifest_path.file_name().unwrap_or_default(),
            local_base
        );
    }

    index
}

/// Загружает библиотеку из нескольких корней и объединяет индексы (варианты одного ключа склеиваются).
/// Для корней с JSON-манифестами использует манифесты; иначе сканирует дерево (WalkDir).
pub fn load_sample_library_merged(roots: &[PathBuf]) -> Result<SampleLibrary, String> {
    let mut merged: HashMap<SampleKey, Vec<PathBuf>> = HashMap::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        let manifest_index = build_sample_index_from_manifests(root);
        let index = if manifest_index.is_empty() {
            build_sample_index(root)
        } else {
            manifest_index
        };
        for (key, paths) in index {
            merged.entry(key).or_default().extend(paths);
        }
    }
    eprintln!(
        "[sample_loader] merged {} roots, {} keys",
        roots.len(),
        merged.len()
    );
    Ok(SampleLibrary::from_index(merged))
}

/// Возвращает список доступных bank-имён из всех подключённых корней сэмплов.
pub fn available_banks(main_root: &Path) -> Vec<String> {
    let roots = sample_roots(main_root);
    let mut banks = BTreeSet::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        for entry in WalkDir::new(&root)
            .follow_links(true)
            .into_iter()
            .filter_entry(is_scannable_entry)
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();
            if !is_audio_file(path) {
                continue;
            }
            let rel = match path.strip_prefix(&root) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let bank = if let Some((b, _)) = bank_name_from_machines_path(rel) {
                Some(b)
            } else {
                bank_from_relative_path(rel)
            };
            if let Some(bank) = bank {
                if !bank.trim().is_empty() {
                    banks.insert(bank);
                }
            }
        }
    }
    banks.into_iter().collect()
}

/// Возвращает список доступных sound-имён (ключей `s("...")`) из всех подключённых корней сэмплов.
pub fn available_sound_names(main_root: &Path) -> Vec<String> {
    let roots = sample_roots(main_root);
    let mut names = BTreeSet::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        let index = build_sample_index(&root);
        for key in index.keys() {
            if !key.name.trim().is_empty() {
                names.insert(key.name.clone());
            }
        }
    }
    names.into_iter().collect()
}

/// Возвращает список sound-имён по каждому банку.
/// Ключ банка — lower-case (как в runtime lookup).
/// Имена нормализуются: если name начинается с "{bank}_", возвращается суффикс без префикса.
pub fn available_sounds_by_bank(main_root: &Path) -> BTreeMap<String, Vec<String>> {
    fn bank_aliases(bank_lc: &str) -> Vec<String> {
        let mut out: BTreeSet<String> = BTreeSet::new();
        let trimmed = bank_lc.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
        out.insert(trimmed.to_string());
        let compact: String = trimmed
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();
        if !compact.is_empty() {
            out.insert(compact.clone());
        }
        for prefix in ["roland", "akai", "linndrum", "linn", "yamaha"] {
            if let Some(rest) = compact.strip_prefix(prefix) {
                if !rest.is_empty() {
                    out.insert(rest.to_string());
                }
            }
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                if !rest.is_empty() {
                    out.insert(rest.to_string());
                }
            }
        }
        out.into_iter().collect()
    }

    let roots = sample_roots(main_root);
    let mut grouped: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        let index = build_sample_index(&root);
        for key in index.keys() {
            let Some(bank) = key.bank.as_ref() else {
                continue;
            };
            let bank_lc = bank.to_lowercase();
            let name = key.name.trim().to_lowercase();
            if name.is_empty() {
                continue;
            }
            let prefix = format!("{}_", bank_lc);
            let normalized = if let Some(stripped) = name.strip_prefix(&prefix) {
                if stripped.is_empty() {
                    name.clone()
                } else {
                    stripped.to_string()
                }
            } else {
                name.clone()
            };
            for alias in bank_aliases(&bank_lc) {
                grouped.entry(alias).or_default().insert(normalized.clone());
            }
        }
    }
    grouped
        .into_iter()
        .map(|(k, set)| (k, set.into_iter().collect::<Vec<_>>()))
        .collect()
}

/// Возвращает sound-имена по реальным bank-ключам (без алиасов/сокращений).
/// Ключ банка — lower-case имя банка из индекса.
pub fn available_sounds_by_canonical_bank(main_root: &Path) -> BTreeMap<String, Vec<String>> {
    let roots = sample_roots(main_root);
    let mut grouped: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        let index = build_sample_index(&root);
        for key in index.keys() {
            let Some(bank) = key.bank.as_ref() else {
                continue;
            };
            let bank_lc = bank.to_lowercase();
            let name = key.name.trim().to_lowercase();
            if name.is_empty() {
                continue;
            }
            let prefix = format!("{}_", bank_lc);
            let normalized = if let Some(stripped) = name.strip_prefix(&prefix) {
                if stripped.is_empty() {
                    name.clone()
                } else {
                    stripped.to_string()
                }
            } else {
                name.clone()
            };
            grouped.entry(bank_lc).or_default().insert(normalized);
        }
    }
    grouped
        .into_iter()
        .map(|(k, set)| (k, set.into_iter().collect::<Vec<_>>()))
        .collect()
}

/// Загружает библиотеку сэмплов: индекс строится по дереву (или из кэша), WAV читаются по требованию при первом воспроизведении.
pub fn load_sample_library(root: &Path) -> Result<SampleLibrary, String> {
    if !root.exists() {
        return Ok(SampleLibrary::new());
    }

    let root_str = root.to_string_lossy().to_string();
    let cache_path = root.join(INDEX_CACHE_FILENAME);

    // Пробуем загрузить индекс из кэша
    if cache_path.exists() {
        if let Ok(data) = std::fs::read_to_string(&cache_path) {
            if let Ok(file) = serde_json::from_str::<IndexCacheFile>(&data) {
                if file.root == root_str {
                    let index: HashMap<SampleKey, Vec<PathBuf>> = file
                        .entries
                        .into_iter()
                        .map(|e| {
                            let key = SampleKey {
                                bank: e.bank.map(|s| s.to_lowercase()),
                                name: e.name.to_lowercase(),
                            };
                            let paths = e.paths.into_iter().map(PathBuf::from).collect();
                            (key, paths)
                        })
                        .collect();
                    eprintln!(
                        "[sample_loader] index loaded from cache ({} keys)",
                        index.len()
                    );
                    return Ok(SampleLibrary::from_index(index));
                }
            }
        }
    }

    // Сканируем дерево (только пути, без чтения WAV)
    let index = build_sample_index(root);
    let entries: Vec<IndexEntry> = index
        .iter()
        .map(|(k, paths)| IndexEntry {
            bank: k.bank.clone(),
            name: k.name.clone(),
            paths: paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect(),
        })
        .collect();
    let cache_file = IndexCacheFile {
        root: root_str,
        entries,
    };
    if let Ok(json) = serde_json::to_string_pretty(&cache_file) {
        let _ = std::fs::write(&cache_path, json);
    }
    eprintln!(
        "[sample_loader] index built and cached ({} keys)",
        index.len()
    );

    Ok(SampleLibrary::from_index(index))
}

/// Сканирует дерево и строит индекс (ключ -> пути к WAV). WAV не загружаются.
fn build_sample_index(root: &Path) -> HashMap<SampleKey, Vec<PathBuf>> {
    let mut index: HashMap<SampleKey, Vec<PathBuf>> = HashMap::new();
    for entry in WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_entry(is_scannable_entry)
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path().to_path_buf();
        if !is_audio_file(&path) {
            continue;
        }
        let rel = match path.strip_prefix(root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let (bank, name) = if let Some((b, n)) = bank_name_from_machines_path(rel) {
            (Some(b), n)
        } else {
            let bank = bank_from_relative_path(rel);
            let (name, _) = split_name_and_variant(path.file_stem());
            (bank, name)
        };
        if name.is_empty() {
            continue;
        }
        let bank_lc = bank.as_ref().map(|s| s.to_lowercase());
        let name_lc = name.to_lowercase();
        let key = SampleKey {
            bank: bank_lc.clone(),
            name: name_lc.clone(),
        };
        index.entry(key.clone()).or_default().push(path.clone());

        // Runner saves as bank/name_0.wav where name can be "crate_bd"; pattern asks (bank="crate", name="bd").
        // Also index under (bank, name without "bank_" prefix) so s("bd").bank("crate") finds crate/crate_bd_0.wav.
        if let Some(ref b) = bank_lc {
            let prefix = format!("{}_", b);
            if name_lc.starts_with(&prefix) {
                let short_name = name_lc
                    .strip_prefix(&prefix)
                    .unwrap_or(&name_lc)
                    .to_string();
                if !short_name.is_empty() {
                    let short_key = SampleKey {
                        bank: bank_lc.clone(),
                        name: short_name,
                    };
                    index.entry(short_key).or_default().push(path);
                }
            }
        }
    }
    index
}

fn is_wav(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(OsStr::to_str)
            .map(|s| s.to_ascii_lowercase()),
        Some(ext) if ext == "wav" || ext == "wave"
    )
}

/// Exclut les entrées cachées (`.git`, `.trash`, …) du scan : elles ne doivent
/// pas devenir des banques ni des samples. La racine (profondeur 0) est gardée
/// même si son nom commence par un point.
fn is_scannable_entry(entry: &walkdir::DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    !entry
        .file_name()
        .to_str()
        .map(|n| n.starts_with('.'))
        .unwrap_or(false)
}

fn bank_from_relative_path(rel: &Path) -> Option<String> {
    let parent = rel.parent()?;
    let first = parent.components().next()?;
    let as_os: &OsStr = match first {
        std::path::Component::Normal(os) => os,
        _ => return None,
    };
    let s = as_os.to_string_lossy().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Для путей вида .../tidal-drum-machines/machines/RolandTR909/rolandtr909-oh/file.wav
/// возвращает (bank, name) = ("RolandTR909", "oh"), pour correspondre à .bank("RolandTR909") et s("oh").
fn bank_name_from_machines_path(rel: &Path) -> Option<(String, String)> {
    let components: Vec<_> = rel.components().collect();
    // Нужно минимум: tidal-drum-machines / machines / BankName / bankname-shortname / file.wav
    if components.len() < 4 {
        return None;
    }
    let get = |i: usize| -> Option<String> {
        match components.get(i)? {
            std::path::Component::Normal(os) => Some(os.to_string_lossy().to_string()),
            _ => None,
        }
    };
    let first = get(0)?;
    let second = get(1)?;
    if first != "tidal-drum-machines" || second != "machines" {
        return None;
    }
    let bank = get(2)?;
    let folder = get(3)?;
    let prefix = format!("{}-", bank.to_lowercase());
    let name = folder.strip_prefix(&prefix)?.to_string();
    if name.is_empty() {
        return None;
    }
    Some((bank, name))
}

fn split_name_and_variant(stem: Option<&OsStr>) -> (String, Option<usize>) {
    let s = match stem.and_then(OsStr::to_str) {
        Some(s) => s,
        None => return (String::new(), None),
    };
    if let Some(idx) = s.rfind('_') {
        let (base, suffix) = s.split_at(idx);
        let suffix = &suffix[1..];
        if let Ok(n) = suffix.parse::<usize>() {
            return (base.to_string(), Some(n));
        }
    }
    (s.to_string(), None)
}

fn load_wav_sample_impl(path: &Path) -> Result<Arc<Sample>, String> {
    match load_wav_sample_hound(path) {
        Ok(s) => return Ok(s),
        Err(e) => {
            // Fallback for WAV with extended fmt (e.g. WAVE_FORMAT_EXTENSIBLE with 40-byte fmt,
            // or IEEE Float with 40-byte chunk) which hound rejects.
            if e.contains("unexpected fmt chunk size")
                || e.contains("invalid fmt chunk size")
                || e.contains("unexpected WAVEFORMATEXTENSIBLE size")
                || e.contains("unexpected WAVEFORMATEX size")
            {
                return load_wav_sample_permissive(path);
            }
            return Err(e);
        }
    }
}

fn load_wav_sample_hound(path: &Path) -> Result<Arc<Sample>, String> {
    let mut reader = hound::WavReader::open(path)
        .map_err(|e| format!("open wav failed for {:?}: {}", path, e))?;
    let spec = reader.spec();
    let channels = spec.channels;
    if channels == 0 {
        return Err("wav file has zero channels".to_string());
    }
    let sample_rate = spec.sample_rate;

    let total_samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap_or(0.0)).collect(),
        hound::SampleFormat::Int => {
            let max_amplitude = (1i64 << (spec.bits_per_sample.saturating_sub(1) as u32)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.unwrap_or(0) as f32 / max_amplitude)
                .collect()
        }
    };

    if total_samples.is_empty() {
        return Err("wav file is empty".to_string());
    }

    Ok(Arc::new(Sample {
        data: Arc::new(total_samples),
        sample_rate,
        channels,
    }))
}

/// Permissive WAV parser that accepts any fmt chunk size (e.g. 40 for WAVE_FORMAT_EXTENSIBLE).
fn load_wav_sample_permissive(path: &Path) -> Result<Arc<Sample>, String> {
    let data = std::fs::read(path).map_err(|e| format!("read {:?}: {}", path, e))?;
    if data.len() < 12 {
        return Err("file too short for RIFF header".to_string());
    }
    if &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return Err("not a RIFF WAVE file".to_string());
    }

    let mut i = 12usize;
    let mut fmt_channels = 0u16;
    let mut fmt_sample_rate = 0u32;
    let mut fmt_bits = 0u16;
    let mut fmt_float = false;

    while i + 8 <= data.len() {
        let chunk_id = &data[i..i + 4];
        let chunk_len =
            u32::from_le_bytes([data[i + 4], data[i + 5], data[i + 6], data[i + 7]]) as usize;
        i += 8;
        if i + chunk_len > data.len() {
            break;
        }

        if chunk_id == b"fmt " {
            if chunk_len < 16 {
                return Err("fmt chunk too short".to_string());
            }
            // WAVEFORMATEX: 0-1 tag, 2-3 ch, 4-7 rate, 8-11 avgBytesPerSec, 12-13 blockAlign, 14-15 bitsPerSample
            let format_tag = u16::from_le_bytes([data[i], data[i + 1]]);
            fmt_channels = u16::from_le_bytes([data[i + 2], data[i + 3]]);
            fmt_sample_rate =
                u32::from_le_bytes([data[i + 4], data[i + 5], data[i + 6], data[i + 7]]);
            let block_align = u16::from_le_bytes([data[i + 12], data[i + 13]]);
            fmt_bits = u16::from_le_bytes([data[i + 14], data[i + 15]]);

            match format_tag {
                1 => {
                    /* PCM */
                    fmt_float = false;
                }
                3 => {
                    /* IEEE Float */
                    fmt_float = true;
                }
                0xfffe => {
                    // WAVE_FORMAT_EXTENSIBLE: subformat + valid_bits_per_sample.
                    if chunk_len >= 40 && data.len() >= i + 40 {
                        let subfmt = &data[i + 24..i + 40];
                        fmt_float = subfmt == SUBFMT_IEEE_FLOAT;
                        let valid_bits = u16::from_le_bytes([data[i + 18], data[i + 19]]);
                        if fmt_bits == 0 && valid_bits > 0 {
                            fmt_bits = valid_bits;
                        }
                    }
                }
                _ => return Err(format!("unsupported format tag {}", format_tag)),
            }
            if fmt_channels == 0 {
                return Err("zero channels".to_string());
            }
            // Если wBitsPerSample так и остался 0 — берём из block_align (байт на канал × 8).
            if fmt_bits == 0 && block_align > 0 && fmt_channels > 0 {
                let bytes_per_ch = (block_align / fmt_channels) as u16;
                fmt_bits = (bytes_per_ch * 8).min(32);
            }
            i += chunk_len;
            continue;
        }

        if chunk_id == b"data" {
            if fmt_channels == 0 || fmt_sample_rate == 0 {
                return Err("fmt chunk missing or before data".to_string());
            }
            if fmt_bits == 0 {
                return Err("invalid bits per sample (zero in fmt)".to_string());
            }
            let samples = decode_pcm_data(
                &data[i..i + chunk_len],
                fmt_channels as usize,
                fmt_bits,
                fmt_float,
            )?;
            if samples.is_empty() {
                return Err("wav file is empty".to_string());
            }
            return Ok(Arc::new(Sample {
                data: Arc::new(samples),
                sample_rate: fmt_sample_rate,
                channels: fmt_channels,
            }));
        }

        i += chunk_len;
    }

    Err("no data chunk found".to_string())
}

fn decode_pcm_data(
    raw: &[u8],
    channels: usize,
    bits_per_sample: u16,
    is_float: bool,
) -> Result<Vec<f32>, String> {
    let bytes_per_sample = (bits_per_sample / 8) as usize;
    let frame_size = channels * bytes_per_sample;
    if frame_size == 0 {
        return Err("invalid bits per sample".to_string());
    }
    let num_frames = raw.len() / frame_size;
    let mut out = Vec::with_capacity(num_frames * channels);

    if is_float {
        if bits_per_sample != 32 {
            return Err("permissive loader: only 32-bit float supported".to_string());
        }
        for chunk in raw.chunks_exact(4) {
            let v = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            out.push(v);
        }
    } else {
        let max_amp = (1i64 << (bits_per_sample.saturating_sub(1) as u32)) as f32;
        for frame in raw.chunks_exact(frame_size) {
            for ch in 0..channels {
                let b = &frame[ch * bytes_per_sample..][..bytes_per_sample];
                let v = match bits_per_sample {
                    8 => i8::from_le_bytes([b[0]]) as f32,
                    16 => i16::from_le_bytes([b[0], b[1]]) as f32,
                    24 => {
                        let x = (b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i32) << 16);
                        let x = if x & (1 << 23) != 0 {
                            x | (0xff00_0000u32 as i32)
                        } else {
                            x
                        };
                        x as f32
                    }
                    32 => {
                        let x = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                        x as f32
                    }
                    _ => return Err(format!("unsupported bits per sample {}", bits_per_sample)),
                };
                out.push(v / max_amp);
            }
        }
    }

    Ok(out)
}

/// Helper to get the default sample root path for this project.
pub fn default_sample_root() -> PathBuf {
    crate::paths::default_sample_folder()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir() -> PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!(
            "struidel_sample_loader_test_{}_{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_minimal_wav(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        writer.write_sample::<i16>(0).unwrap();
        writer.finalize().unwrap();
    }

    fn fake_roland_tr909_tree() -> (PathBuf, PathBuf) {
        let root = unique_temp_dir();
        let sample_path =
            root.join("tidal-drum-machines/machines/RolandTR909/rolandtr909-bd/BT0A0A7.WAV");
        write_minimal_wav(&sample_path);
        (root, sample_path)
    }

    #[test]
    fn explicit_missing_bank_does_not_fall_back_to_unbanked() {
        let root = unique_temp_dir();
        let sample_path = root.join("bd.wav");
        write_minimal_wav(&sample_path);
        let library = load_sample_library(&root).unwrap();
        let missing_bank_key = SampleKey {
            bank: Some("missing".to_string()),
            name: "bd".to_string(),
        };

        assert_eq!(library.get_path(&missing_bank_key, Some(0)), None);
        assert!(library.lookup(&missing_bank_key, Some(0)).is_none());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn no_bank_bd_falls_back_to_default_roland_tr909() {
        let (root, sample_path) = fake_roland_tr909_tree();
        let library = load_sample_library(&root).unwrap();

        assert_eq!(
            library.get_path(
                &SampleKey {
                    bank: None,
                    name: "bd".to_string(),
                },
                Some(0),
            ),
            Some(sample_path)
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tr909_alias_finds_roland_tr909_bank() {
        let (root, sample_path) = fake_roland_tr909_tree();
        let library = load_sample_library(&root).unwrap();

        assert_eq!(
            library.get_path(
                &SampleKey {
                    bank: Some("tr909".to_string()),
                    name: "bd".to_string(),
                },
                Some(0),
            ),
            Some(sample_path)
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn canonical_roland_tr909_bank_still_works() {
        let (root, sample_path) = fake_roland_tr909_tree();
        let library = load_sample_library(&root).unwrap();

        assert_eq!(
            library.get_path(
                &SampleKey {
                    bank: Some("RolandTR909".to_string()),
                    name: "bd".to_string(),
                },
                Some(0),
            ),
            Some(sample_path)
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hidden_dirs_are_not_indexed() {
        let root = std::env::temp_dir()
            .join(format!("struidel-scan-hidden-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for sub in ["vox", ".trash/vox", ".git/objects"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        std::fs::write(root.join("vox/take.wav"), b"RIFF").unwrap();
        std::fs::write(root.join(".trash/vox/old.wav"), b"RIFF").unwrap();
        std::fs::write(root.join(".git/objects/pack.wav"), b"RIFF").unwrap();

        let index = build_sample_index(&root);
        let banks: Vec<_> = index.keys().filter_map(|k| k.bank.clone()).collect();
        assert!(banks.iter().any(|b| b == "vox"), "{banks:?}");
        assert!(banks.iter().all(|b| !b.starts_with('.')), "{banks:?}");

        std::fs::remove_dir_all(&root).unwrap();
    }

    fn write_sine_wav(path: &Path, sample_rate: u32, frames: usize) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..frames {
            let t = i as f32 / sample_rate as f32;
            writer
                .write_sample::<i16>((t * 440.0 * 2.0 * std::f32::consts::PI).sin().mul_add(0.5, 0.0)
                    .clamp(-1.0, 1.0)
                    .mul_add(i16::MAX as f32, 0.0) as i16)
                .unwrap();
        }
        writer.finalize().unwrap();
    }

    /// Décode mp3/flac/ogg via symphonia (ffmpeg requis : le test est sauté
    /// s'il n'est pas installé).
    #[test]
    fn non_wav_formats_decode_via_symphonia() {
        if std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_err()
        {
            eprintln!("ffmpeg absent, test sauté");
            return;
        }
        let root = unique_temp_dir();
        let wav = root.join("sine.wav");
        write_sine_wav(&wav, 22_050, 4_410);

        for ext in ["flac", "mp3", "ogg"] {
            let out = root.join(format!("sine.{ext}"));
            let status = std::process::Command::new("ffmpeg")
                .args([
                    "-y", "-v", "error", "-i",
                ])
                .arg(&wav)
                .arg(&out)
                .status()
                .unwrap();
            assert!(status.success(), "ffmpeg encode {ext} failed");

            let sample = load_wav_sample(&out)
                .unwrap_or_else(|e| panic!("decode {ext} failed: {e}"));
            assert_eq!(sample.channels, 1, "{ext}: channels");
            assert_eq!(sample.sample_rate, 22_050, "{ext}: sample_rate");
            let rms: f32 = (sample.data.iter().map(|v| v * v).sum::<f32>()
                / sample.data.len().max(1) as f32)
                .sqrt();
            assert!(rms > 0.1, "{ext}: rms {rms} trop faible");
        }

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Un fichier .mp3 posé dans une banque est indexé comme sample normal.
    #[test]
    fn mp3_files_are_indexed_as_samples() {
        let root = unique_temp_dir();
        let bank = root.join("drums");
        std::fs::create_dir_all(&bank).unwrap();
        std::fs::write(bank.join("kick.mp3"), b"fake").unwrap();
        std::fs::write(bank.join("notes.txt"), b"not audio").unwrap();

        let index = build_sample_index(&root);
        assert!(
            index.keys().any(|k| k.name == "kick" && k.bank.as_deref() == Some("drums")),
            "kick.mp3 devrait être indexé"
        );
        assert!(
            !index.keys().any(|k| k.name == "notes"),
            "notes.txt ne devrait pas être indexé"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }
}
