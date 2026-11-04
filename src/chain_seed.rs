//! On-chain seed → deterministic Strudel pattern codegen (no wallet / no mint).

use serde_json::{json, Value};
use std::time::Duration;

pub const DEFAULT_RPC_URL: &str = "https://ethereum.publicnode.com";
pub const DEFAULT_OLLAMA_URL: &str = "http://127.0.0.1:11434";
pub const DEFAULT_OLLAMA_MODEL: &str = "qwen2.5-coder:1.5b";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenMode {
    /// Deterministic compositional generator from seed (offline).
    Procedural,
    /// Try local Ollama; fall back to procedural on failure.
    Ollama,
}

impl GenMode {
    pub fn label(self) -> &'static str {
        match self {
            GenMode::Procedural => "Procedural (rich)",
            GenMode::Ollama => "Ollama (local LLM)",
        }
    }

    pub fn all() -> &'static [GenMode] {
        &[GenMode::Procedural, GenMode::Ollama]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedMode {
    LatestBlock,
    BlockNumber,
    Address,
    TxHash,
    Nft,
}

impl SeedMode {
    pub fn label(self) -> &'static str {
        match self {
            SeedMode::LatestBlock => "Latest block",
            SeedMode::BlockNumber => "Block number",
            SeedMode::Address => "Address",
            SeedMode::TxHash => "Tx hash",
            SeedMode::Nft => "NFT (contract + tokenId)",
        }
    }

    pub fn all() -> &'static [SeedMode] {
        &[
            SeedMode::LatestBlock,
            SeedMode::BlockNumber,
            SeedMode::Address,
            SeedMode::TxHash,
            SeedMode::Nft,
        ]
    }
}

#[derive(Debug, Clone)]
pub struct SeedResult {
    pub seed_hex: String,
    pub source_label: String,
    /// For inject mode this is empty until the IDE merges into the editor source.
    pub code: String,
    pub bpm: u32,
    pub meta_note: Option<String>,
    pub generator: String,
    /// Unusual Hydra sketch derived from the same seed (NFT → look).
    pub hydra_code: String,
    /// Human-readable full cycle: chain → seed → mapping → pattern → hydra link.
    pub pipeline: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ChainSeedState {
    pub mode: SeedMode,
    pub gen_mode: GenMode,
    pub rpc_url: String,
    pub ollama_url: String,
    pub ollama_model: String,
    pub block_number: String,
    pub address: String,
    pub tx_hash: String,
    pub nft_contract: String,
    pub nft_token_id: String,
    pub status: String,
    pub seed_hex: String,
    pub source_label: String,
    pub preview_code: String,
    pub meta_note: String,
    pub busy: bool,
    /// UI phase label while busy: Fetching / Generating / …
    pub phase: String,
    /// Full cycle explanation steps for the UI.
    pub pipeline: Vec<String>,
    pub last_generator: String,
    pub last_bpm: u32,
    /// Last unusual Hydra sketch derived from the seed.
    pub last_hydra_code: String,
    /// When Apply: also push last_hydra_code into Hydra studio + Eval.
    pub push_hydra: bool,
    /// Root on-chain seed (before play-morph).
    pub base_seed_hex: String,
    /// Follow new Ethereum blocks (poll RPC).
    pub live_follow: bool,
    /// While playing, morph seed each cycle → Hydra buses + optional reinject.
    pub live_morph: bool,
    /// Live ticks write //@chain-seed into the Workspace editor.
    pub live_apply: bool,
    /// Seconds between live block polls.
    pub live_interval_secs: f32,
    pub next_live_poll: Option<std::time::Instant>,
    pub live_tick: u32,
    pub last_morph_cycle: i64,
    /// Recent seeds (label, hex) for recall — newest last.
    pub seed_history: Vec<(String, String)>,
    /// Current fetch was started by live follow poll.
    pub live_poll_pending: bool,
}

impl Default for ChainSeedState {
    fn default() -> Self {
        Self {
            mode: SeedMode::LatestBlock,
            gen_mode: GenMode::Procedural,
            rpc_url: DEFAULT_RPC_URL.to_string(),
            ollama_url: DEFAULT_OLLAMA_URL.to_string(),
            ollama_model: DEFAULT_OLLAMA_MODEL.to_string(),
            block_number: String::new(),
            address: String::new(),
            tx_hash: String::new(),
            nft_contract: String::new(),
            nft_token_id: String::new(),
            status: "Idle — fetch an on-chain seed".into(),
            seed_hex: String::new(),
            source_label: String::new(),
            preview_code: demo_pattern_code().into(),
            meta_note: String::new(),
            busy: false,
            phase: String::new(),
            pipeline: vec![
                "① Choose source (block / address / tx / NFT)".into(),
                "② Fetch → inject //@chain-seed into YOUR Strudel".into(),
                "③ Live Follow: new blocks refresh the root seed".into(),
                "④ Live Morph: while playing, seed crawls each cycle → Hydra pulse".into(),
                "⑤ Optional live-write keeps editor //@chain-seed in sync".into(),
            ],
            last_generator: String::new(),
            last_bpm: 0,
            last_hydra_code: String::new(),
            push_hydra: true,
            base_seed_hex: String::new(),
            live_follow: false,
            live_morph: true,
            live_apply: false,
            live_interval_secs: 12.0,
            next_live_poll: None,
            live_tick: 0,
            last_morph_cycle: -1,
            seed_history: Vec::new(),
            live_poll_pending: false,
        }
    }
}

/// Progress / result events from the background fetch+generate worker.
#[derive(Debug, Clone)]
pub enum ChainSeedEvent {
    Status { phase: String, detail: String },
    Done(Result<SeedResult, String>),
}

pub fn demo_pattern_code() -> &'static str {
    r#"// Your Strudel — Chain Seed injects math consts, does not replace this stack
setcpm(120/4)
stack(
  s("[bd ~ bd bd] [~ bd ~ bd]").gain(0.85),
  s("hh*8").gain(0.28).pan(sine.range(-0.6,0.6)),
  s("~ cp ~ <cp rim>").gain(0.5),
  n("0 2 3 7").scale("A:minor").s("sawtooth").gain(0.18).room(0.35).lpf(1800)
)
"#
}

/// Fetch seed material from a public Ethereum JSON-RPC endpoint, then generate code.
pub fn fetch_seed(
    mode: SeedMode,
    rpc_url: &str,
    block_number: &str,
    address: &str,
    tx_hash: &str,
    nft_contract: &str,
    nft_token_id: &str,
    gen_mode: GenMode,
    ollama_url: &str,
    ollama_model: &str,
    progress: Option<&std::sync::mpsc::Sender<ChainSeedEvent>>,
) -> Result<SeedResult, String> {
    let notify = |phase: &str, detail: &str| {
        if let Some(tx) = progress {
            let _ = tx.send(ChainSeedEvent::Status {
                phase: phase.into(),
                detail: detail.into(),
            });
        }
    };

    notify("1/2 Fetching", "Querying Ethereum JSON-RPC…");

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("struIDEl-chain-seed/0.1")
        .build()
        .map_err(|e| format!("HTTP client: {e}"))?;

    let (seed, label, meta_note) = match mode {
        SeedMode::LatestBlock => {
            notify("1/2 Fetching", "eth_getBlockByNumber(latest)…");
            let block = eth_get_block(&client, rpc_url, "latest")?;
            let (seed, label) = seed_from_block(block, "latest block")?;
            (seed, label, None)
        }
        SeedMode::BlockNumber => {
            let n = parse_u64(block_number.trim())?;
            let hex = format!("0x{n:x}");
            notify("1/2 Fetching", &format!("eth_getBlockByNumber({n})…"));
            let block = eth_get_block(&client, rpc_url, &hex)?;
            let (seed, label) = seed_from_block(block, &format!("block {n}"))?;
            (seed, label, None)
        }
        SeedMode::Address => {
            let addr = normalize_hex_addr(address)?;
            notify("1/2 Fetching", &format!("balance+code for {addr}…"));
            let bal = eth_call_simple(
                &client,
                rpc_url,
                "eth_getBalance",
                json!([addr, "latest"]),
            )?;
            let code = eth_call_simple(
                &client,
                rpc_url,
                "eth_getCode",
                json!([addr, "latest"]),
            )?;
            let seed = mix_hex_parts(&[&addr, &string_hex(&bal), &string_hex(&code)]);
            (seed, format!("address {addr}"), None)
        }
        SeedMode::TxHash => {
            let h = normalize_tx_hash(tx_hash)?;
            notify("1/2 Fetching", "eth_getTransactionByHash…");
            let tx = eth_call_simple(
                &client,
                rpc_url,
                "eth_getTransactionByHash",
                json!([h]),
            )?;
            if tx.is_null() {
                return Err("transaction not found".into());
            }
            let th = tx
                .get("hash")
                .and_then(|v| v.as_str())
                .unwrap_or(&h)
                .to_string();
            let input = tx
                .get("input")
                .and_then(|v| v.as_str())
                .unwrap_or("0x")
                .to_string();
            let from = tx
                .get("from")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let seed = mix_hex_parts(&[&th, &input, &from]);
            (seed, format!("tx {th}"), None)
        }
        SeedMode::Nft => {
            let contract = normalize_hex_addr(nft_contract)?;
            let token_id = parse_u64(nft_token_id.trim())?;
            notify("1/2 Fetching", &format!("tokenURI {contract} #{token_id}…"));
            let data = encode_token_uri_call(token_id);
            let raw = eth_call_simple(
                &client,
                rpc_url,
                "eth_call",
                json!([{ "to": contract, "data": data }, "latest"]),
            )?;
            let uri = decode_abi_string(&raw).unwrap_or_default();
            let mut meta_note = if uri.is_empty() {
                None
            } else {
                Some(format!("tokenURI: {uri}"))
            };
            let mut seed_parts: Vec<String> =
                vec![contract.clone(), format!("{token_id:x}"), uri.clone()];
            let mut nft_trace = vec![
                format!("① NFT contract {contract}"),
                format!("   tokenId {token_id} → eth_call tokenURI(uint256)"),
            ];
            if uri.is_empty() {
                nft_trace.push("   tokenURI empty — seed from contract+tokenId only".into());
            } else {
                nft_trace.push(format!("   tokenURI → {uri}"));
            }
            let mut seed = mix_hex_parts(
                &seed_parts
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>(),
            );
            if uri.starts_with("http://") || uri.starts_with("https://") {
                notify("1/2 Fetching", "Downloading NFT metadata…");
                if let Ok(body) = client.get(&uri).send().and_then(|r| r.text()) {
                    seed_parts.push(body.clone());
                    seed = mix_hex_parts(
                        &seed_parts
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>(),
                    );
                    nft_trace.push(format!(
                        "   metadata JSON fetched ({} bytes) mixed into seed",
                        body.len()
                    ));
                    if let Ok(j) = serde_json::from_str::<Value>(&body) {
                        let name = j.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let attrs = j
                            .get("attributes")
                            .or_else(|| j.get("traits"))
                            .map(|a| a.to_string())
                            .unwrap_or_default();
                        meta_note = Some(format!("NFT “{name}” · {uri}"));
                        if !attrs.is_empty() {
                            nft_trace.push(format!(
                                "   traits/attributes present (hashed with body)"
                            ));
                        }
                    }
                } else {
                    nft_trace.push("   metadata download failed — URI still in seed".into());
                }
            } else if uri.starts_with("ipfs://") {
                meta_note = Some(format!("ipfs metadata (hashed URI only): {uri}"));
                nft_trace.push("   ipfs:// — URI string mixed (no gateway fetch)".into());
            }
            nft_trace.push(format!(
                "② mix(contract ⊕ tokenId ⊕ uri[⊕ metadata]) → seed {}",
                short_seed(&seed)
            ));
            if let Some(ref m) = meta_note {
                nft_trace.push(format!("   note: {m}"));
            }
            let label = format!("NFT {contract} #{token_id}");
            notify(
                "1/2 Fetching",
                &format!("Seed ready ({})", short_seed(&seed)),
            );
            return compose_result(
                &seed,
                &label,
                meta_note,
                gen_mode,
                ollama_url,
                ollama_model,
                progress,
                nft_trace,
            );
        }
    };

    notify(
        "1/2 Fetching",
        &format!("Seed ready ({})", short_seed(&seed)),
    );

    let mut pipeline = vec![format!("① Source: {label}")];
    if let Some(ref m) = meta_note {
        pipeline.push(format!("   meta: {m}"));
    }
    pipeline.push(format!(
        "② Seed hash (32 bytes): {}",
        short_seed(&seed)
    ));

    compose_result(
        &seed,
        &label,
        meta_note,
        gen_mode,
        ollama_url,
        ollama_model,
        progress,
        pipeline,
    )
}

pub fn short_seed(seed: &str) -> String {
    if seed.len() > 16 {
        format!("{}…{}", &seed[..8], &seed[seed.len() - 4..])
    } else {
        seed.to_string()
    }
}

fn explain_seed_mapping(seed: &str) -> Vec<String> {
    let feel = feel_from_seed(seed);
    let bpm = bpm_from_seed(seed);
    let scale = scale_from_seed(seed);
    let dens_kick = 2 + (seed_byte(seed, 1) % 6);
    let n_layers = 3 + (seed_byte(seed, 30) % 5);
    vec![
        format!(
            "③ Mapping seed→math: feel={} · BPM={} · scale={} · euclid≈{}",
            feel_name(feel),
            bpm,
            scale,
            dens_kick
        ),
        format!(
            "   byte[0]→BPM  byte[11]→feel  byte[12]→scale  byte[1]→density≈{dens_kick}  layers≈{n_layers}"
        ),
        "   Inject consts: seedGain · seedLpf · seedRoom · seedEuclid · seedScale · …".into(),
        "   Same seed also folds into an unusual Hydra sketch topology".into(),
    ]
}

const CHAIN_SEED_BEGIN: &str = "// @chain-seed";
const CHAIN_SEED_END: &str = "// @chain-seed-end";

/// Strip a previous `// @chain-seed` … `// @chain-seed-end` region (inclusive).
pub fn strip_chain_seed_block(source: &str) -> String {
    let Some(start) = source.find(CHAIN_SEED_BEGIN) else {
        return source.to_string();
    };
    let after_begin = start + CHAIN_SEED_BEGIN.len();
    let end = source[after_begin..]
        .find(CHAIN_SEED_END)
        .map(|i| after_begin + i + CHAIN_SEED_END.len())
        .unwrap_or(source.len());
    let mut out = String::with_capacity(source.len());
    out.push_str(&source[..start]);
    let rest = source[end..].trim_start_matches(['\r', '\n']);
    out.push_str(rest);
    out
}

/// Build reusable seed math consts for injection into Strudel (JS-ish).
pub fn build_seed_math_block(
    seed: &str,
    label: &str,
    meta: Option<&str>,
    phase: u64,
) -> String {
    let bpm = bpm_from_seed(seed);
    let feel = feel_name(feel_from_seed(seed));
    let scale = scale_from_seed(seed);
    let short = short_seed(seed);
    let gain = 0.35 + (seed_byte(seed, 2) as f32 / 255.0) * 0.55;
    let hat = 0.12 + (seed_byte(seed, 3) as f32 / 255.0) * 0.35;
    let lpf = 400 + (seed_byte(seed, 4) as u32 % 36) * 100;
    let room = 0.1 + (seed_byte(seed, 5) as f32 / 255.0) * 0.75;
    let pan = (seed_byte(seed, 6) as f32 / 255.0) * 1.6 - 0.8;
    let slow = 1 + (seed_byte(seed, 7) % 4);
    let euclid = 2 + (seed_byte(seed, 1) % 6);
    let note = seed_byte(seed, 8) % 12;
    let delay = 0.1 + (seed_byte(seed, 9) as f32 / 255.0) * 0.45;
    let crush = 1 + (seed_byte(seed, 10) % 8);
    let meta_line = meta
        .filter(|m| !m.is_empty())
        .map(|m| format!("// meta: {m}\n"))
        .unwrap_or_default();
    format!(
        "{CHAIN_SEED_BEGIN}\n\
         // source: {label}\n\
         // seed: {short} · feel={feel} · phase={phase}\n\
         {meta_line}\
         // Use these in YOUR pattern (code is source of truth):\n\
         //   .gain(seedGain) · .lpf(seedLpf) · .room(seedRoom) · .pan(seedPan)\n\
         // live morph tips (values refresh when seed ticks):\n\
         //   .gain(sine.range(seedGain*0.45, seedGain).slow(seedSlow))\n\
         //   .lpf(sine.range(seedLpf*0.5, seedLpf).slow(seedSlow*2))\n\
         //   s(`bd(<${{seedEuclid}},8>)`) · n(`0 2 ${{seedNote}}`).scale(seedScale)\n\
         const seedBpm = {bpm}\n\
         const seedGain = {gain:.3}\n\
         const seedHat = {hat:.3}\n\
         const seedLpf = {lpf}\n\
         const seedRoom = {room:.3}\n\
         const seedPan = {pan:.3}\n\
         const seedSlow = {slow}\n\
         const seedEuclid = {euclid}\n\
         const seedNote = {note}\n\
         const seedDelay = {delay:.3}\n\
         const seedCrush = {crush}\n\
         const seedScale = \"{scale}\"\n\
         const seedPhase = {phase}\n\
         {CHAIN_SEED_END}\n"
    )
}

fn rewrite_setcpm_to_seed(source: &str) -> String {
    // Prefer setcpm(seedBpm/4) once consts exist above.
    let bytes = source.as_bytes();
    let key = b"setcpm(";
    let Some(pos) = bytes.windows(key.len()).position(|w| w == key) else {
        // No setcpm — insert one after the inject block (caller may add).
        return source.to_string();
    };
    let start = pos;
    let mut i = start + key.len();
    let mut depth = 1i32;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    i += 1;
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    let mut out = String::with_capacity(source.len() + 16);
    out.push_str(&source[..start]);
    out.push_str("setcpm(seedBpm/4)");
    out.push_str(&source[i..]);
    out
}

/// Inject / refresh seed math into existing Strudel. Does not invent a new stack.
pub fn inject_seed_math(
    source: &str,
    seed: &str,
    label: &str,
    meta: Option<&str>,
) -> String {
    inject_seed_math_phased(source, seed, label, meta, 0)
}

pub fn inject_seed_math_phased(
    source: &str,
    seed: &str,
    label: &str,
    meta: Option<&str>,
    phase: u64,
) -> String {
    let stripped = strip_chain_seed_block(source);
    let block = build_seed_math_block(seed, label, meta, phase);
    let mut body = stripped.trim_start_matches(['\r', '\n']).to_string();
    let has_setcpm = body.contains("setcpm(");
    if has_setcpm {
        body = rewrite_setcpm_to_seed(&body);
    }

    let mut out = String::new();
    out.push_str(&block);
    if !has_setcpm {
        out.push_str("setcpm(seedBpm/4)\n");
    }
    if !body.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Mix a play/time phase into the root seed so entropy crawls while music runs.
pub fn morph_seed(base_seed: &str, phase: u64) -> String {
    mix_hex_parts(&[base_seed, &format!("phase-{phase:x}"), "live-morph"])
}

/// Hydra bus latch values derived from a seed (for live pulses without full re-eval).
pub fn seed_bus_values(seed: &str) -> serde_json::Value {
    serde_json::json!({
        "freq": 4.0 + (seed_byte(seed, 0) as f64 / 255.0) * 55.0,
        "modDepth": 0.2 + (seed_byte(seed, 2) as f64 / 255.0) * 1.8,
        "modRate": 0.4 + (seed_byte(seed, 3) as f64 / 255.0) * 6.0,
        "feedback": 0.25 + (seed_byte(seed, 9) as f64 / 255.0) * 0.65,
        "fbDistort": 0.02 + (seed_byte(seed, 4) as f64 / 255.0) * 0.14,
        "thresh": 0.2 + (seed_byte(seed, 8) as f64 / 255.0) * 0.7,
        "colorama": seed_byte(seed, 7) as f64 / 255.0,
        "sides": 3 + (seed_byte(seed, 4) % 7) as i32,
        "repeatX": 1 + (seed_byte(seed, 5) % 7) as i32,
        "repeatY": 1 + (seed_byte(seed, 6) % 7) as i32,
        "scale": 0.7 + (seed_byte(seed, 11) as f64 / 255.0) * 1.2,
        "branch": seed_byte(seed, 15) % 4,
        "invert": if seed_byte(seed, 16) > 140 { 1 } else { 0 },
        "poster": 8 + (seed_byte(seed, 10) % 40) as i32 * 4,
        "scrollX": (seed_byte(seed, 14) as f64 / 255.0) * 0.4 - 0.2,
        "scrollY": (seed_byte(seed, 17) as f64 / 255.0) * 0.4 - 0.2,
    })
}

pub fn push_seed_history(state: &mut ChainSeedState, label: &str, seed: &str) {
    let entry = (label.to_string(), seed.to_string());
    if state.seed_history.last().map(|(_, s)| s.as_str()) == Some(seed) {
        return;
    }
    state.seed_history.push(entry);
    if state.seed_history.len() > 10 {
        let drop_n = state.seed_history.len() - 10;
        state.seed_history.drain(0..drop_n);
    }
}

/// Deterministic unusual Hydra sketch from the same on-chain seed (not a preset).
pub fn generate_hydra_sketch(seed: &str, label: &str) -> String {
    let short = short_seed(seed);
    let topology = seed_byte(seed, 20) % 6;
    let f0 = 4.0 + (seed_byte(seed, 0) as f32 / 255.0) * 55.0;
    let md0 = 0.2 + (seed_byte(seed, 2) as f32 / 255.0) * 1.8;
    let sides = 3 + (seed_byte(seed, 4) % 7) as i32;
    let rx = 1 + (seed_byte(seed, 5) % 7) as i32;
    let ry = 1 + (seed_byte(seed, 6) % 7) as i32;
    let ca = seed_byte(seed, 7) as f32 / 255.0;
    let th = 0.2 + (seed_byte(seed, 8) as f32 / 255.0) * 0.7;
    let fb = 0.25 + (seed_byte(seed, 9) as f32 / 255.0) * 0.65;
    let po = 8 + (seed_byte(seed, 10) % 40) as i32 * 4;
    let kaleid = 2 + (seed_byte(seed, 12) % 8) as i32;
    let vor = 20 + (seed_byte(seed, 13) % 100) as i32;
    let scroll = (seed_byte(seed, 14) as f32 / 255.0) * 0.4 - 0.2;
    let use_buses = seed_byte(seed, 15) > 90;
    let invert = if seed_byte(seed, 16) > 140 { 1 } else { 0 };
    let rot = (seed_byte(seed, 17) as f32 / 255.0) * 6.28;
    let contrast = 1.0 + (seed_byte(seed, 18) as f32 / 255.0) * 1.4;

    // Inner numeric / function call used inside lambdas
    let (fi, mi, ci, ti, fbi) = if use_buses {
        (
            "F()".into(),
            "MD()".into(),
            "CA()".into(),
            "TH()".into(),
            "Math.min(0.95, FB())".into(),
        )
    } else {
        (
            format!("{f0:.2}"),
            format!("{md0:.3}"),
            format!("{ca:.3}"),
            format!("{th:.3}"),
            format!("{fb:.3}"),
        )
    };
    let fi: String = fi;
    let mi: String = mi;
    let ci: String = ci;
    let ti: String = ti;
    let fbi: String = fbi;

    let body = match topology {
        0 => format!(
            "noise(() => {fi}).colorama(() => {ci})\n\
  .modulate(voronoi({vor}, () => 0.05 + ({mi}) * 0.15), () => {mi})\n\
  .modulateScale(osc(() => 3 + ({mi}) * 4).rotate({rot:.3}), () => {mi})\n\
  .diff(shape({sides}, 0.2, 0.01).repeat({rx}, {ry}).scrollX({scroll:.3}))\n\
  .thresh(() => {ti}, 0.12)\n\
  .contrast({contrast:.2})\n\
  .invert({invert})\n\
  .blend(src(o0), () => {fbi})\n\
  .out()"
        ),
        1 => format!(
            "osc(() => {fi}, 0.1, () => 0.2 + ({mi}) * 0.5)\n\
  .colorama(() => {ci})\n\
  .modulateRotate(src(o0), () => 0.04 + ({mi}) * 0.6)\n\
  .modulate(shape({sides}).scale(() => 0.7 + ({mi}) * 0.5).repeat({rx},{ry}), () => {mi})\n\
  .posterize({po})\n\
  .diff(voronoi({vor}, 0.1).colorama(() => ({ci}) + 0.2))\n\
  .blend(src(o0), () => {fbi})\n\
  .contrast({contrast:.2})\n\
  .invert({invert})\n\
  .out()"
        ),
        2 => format!(
            "shape({sides}, 0.15, 0.01)\n\
  .repeat(() => Math.max(1, Math.round(({fi}) * 0.2 + {rx})), {ry})\n\
  .modulateKaleid(voronoi(() => 2 + ({mi}) * 5, 0.1, 0.01), {kaleid})\n\
  .modulate(noise(() => 1 + ({mi}) * 3), () => 0.1 + ({mi}) * 0.4)\n\
  .rotate({rot:.3}, () => 0.2 + ({ci}))\n\
  .scale(() => 0.6 + ({mi}) * 0.5)\n\
  .colorama(() => {ci})\n\
  .blend(src(o0), () => {fbi})\n\
  .thresh(() => {ti})\n\
  .invert({invert})\n\
  .out()"
        ),
        3 => format!(
            "voronoi(() => {vor} + ({fi}) * 0.5, () => 0.04 + ({mi}) * 0.2)\n\
  .modulateScale(osc(() => 2 + ({mi})).rotate(() => Math.sin(time) * 0.5 + {rot:.2}), () => {mi})\n\
  .diff(osc(() => {fi}).thresh(() => {ti}).colorama(() => {ci}))\n\
  .modulate(src(o0), () => 0.02 + ({mi}) * 0.15)\n\
  .posterize({po})\n\
  .contrast({contrast:.2})\n\
  .blend(src(o0), () => {fbi})\n\
  .invert({invert})\n\
  .out()"
        ),
        4 => format!(
            "osc(() => {fi}).color(-1.4, -1.2, -1.5)\n\
  .rotate(() => -0.15 + ({mi}) * 0.4, {rot:.3})\n\
  .modulate(\n\
    shape({sides}).scale(1.1).repeat({rx},{ry})\n\
      .modulate(o0, () => 0.03 + ({mi}) * 0.12)\n\
      .rotate(0.4, 0.5),\n\
    () => {mi})\n\
  .diff(noise(2).colorama(() => {ci}))\n\
  .thresh(() => {ti}, 0.1)\n\
  .blend(src(o0), () => {fbi})\n\
  .contrast({contrast:.2})\n\
  .invert({invert})\n\
  .out()"
        ),
        _ => format!(
            "// dual-buffer rift from seed\n\
osc(() => {fi}, 0.08, 0.7).colorama(() => {ci})\n\
  .modulate(noise(() => 0.5 + ({mi})), () => {mi})\n\
  .diff(shape({sides}, 0.25, 0.02).repeat({rx},{ry}).scrollY({scroll:.3}))\n\
  .out(o0)\n\
src(o0)\n\
  .modulateKaleid(voronoi({vor}, 0.12), {kaleid})\n\
  .modulateRotate(src(o0), () => 0.05 + ({mi}) * 0.5)\n\
  .scale(() => 0.75 + ({mi}) * 0.3)\n\
  .contrast({contrast:.2})\n\
  .invert({invert})\n\
  .blend(src(o0), () => {fbi})\n\
  .out(o1)\n\
render(o1)"
        ),
    };

    format!(
        "// Hydra from chain seed · {label}\n\
         // seed {short} · topology {topology} · buses={use_buses}\n\
         // F/MD/FB… live when Music→buses is on in Hydra panel\n\
         {body}\n"
    )
}

fn compose_result(
    seed: &str,
    label: &str,
    meta_note: Option<String>,
    gen_mode: GenMode,
    ollama_url: &str,
    ollama_model: &str,
    progress: Option<&std::sync::mpsc::Sender<ChainSeedEvent>>,
    mut pipeline: Vec<String>,
) -> Result<SeedResult, String> {
    let notify = |phase: &str, detail: &str| {
        if let Some(tx) = progress {
            let _ = tx.send(ChainSeedEvent::Status {
                phase: phase.into(),
                detail: detail.into(),
            });
        }
    };

    pipeline.extend(explain_seed_mapping(seed));

    let bpm = bpm_from_seed(seed);
    let hydra_code = generate_hydra_sketch(seed, label);
    let (code, generator) = match gen_mode {
        GenMode::Procedural => {
            // Chain Seed no longer invents a full stack — inject happens in the IDE
            // against the editor source of truth.
            notify("2/2 Mapping", "Seed math + Hydra sketch ready for inject…");
            pipeline.push(
                "④ Inject mode: YOUR Strudel stays; seed consts upsert into //@chain-seed"
                    .into(),
            );
            pipeline.push("⑤ Unusual Hydra derived from same seed (not a preset)".into());
            (String::new(), "inject".into())
        }
        GenMode::Ollama => {
            notify(
                "2/2 Generating",
                &format!("Ollama [{ollama_model}] + Strudel docs…"),
            );
            match generate_with_ollama(seed, label, bpm, ollama_url, ollama_model) {
                Ok(code) => {
                    pipeline.push(format!("   generator: ollama ({ollama_model})"));
                    (code, format!("ollama:{ollama_model}"))
                }
                Err(e) => {
                    notify(
                        "2/2 Generating",
                        &format!("Ollama rejected ({e}) → inject mode…"),
                    );
                    pipeline.push(format!("   generator: inject (ollama failed: {e})"));
                    (String::new(), format!("inject (ollama failed: {e})"))
                }
            }
        }
    };
    pipeline.push("⑥ Ready — Apply injects into editor · Hydra sketch available".into());
    Ok(SeedResult {
        seed_hex: seed.to_string(),
        source_label: label.to_string(),
        code,
        bpm,
        meta_note,
        generator,
        hydra_code,
        pipeline,
    })
}

fn seed_from_block(block: Value, label: &str) -> Result<(String, String), String> {
    let hash = block
        .get("hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "block missing hash".to_string())?
        .to_string();
    let number = block
        .get("number")
        .and_then(|v| v.as_str())
        .unwrap_or("0x0");
    let mix = block
        .get("mixHash")
        .and_then(|v| v.as_str())
        .unwrap_or("0x0");
    let seed = mix_hex_parts(&[&hash, number, mix]);
    Ok((seed, format!("{label} ({hash})")))
}

fn eth_get_block(
    client: &reqwest::blocking::Client,
    rpc: &str,
    block_id: &str,
) -> Result<Value, String> {
    let v = eth_call_simple(
        client,
        rpc,
        "eth_getBlockByNumber",
        json!([block_id, false]),
    )?;
    if v.is_null() {
        return Err("block not found".into());
    }
    Ok(v)
}

fn eth_call_simple(
    client: &reqwest::blocking::Client,
    rpc: &str,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let resp = client
        .post(rpc)
        .json(&body)
        .send()
        .map_err(|e| format!("RPC request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("RPC HTTP {}", resp.status()));
    }
    let v: Value = resp
        .json()
        .map_err(|e| format!("RPC JSON parse: {e}"))?;
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("RPC error");
        return Err(msg.to_string());
    }
    v.get("result")
        .cloned()
        .ok_or_else(|| "RPC missing result".to_string())
}

fn string_hex(v: &Value) -> String {
    v.as_str().unwrap_or("0x0").to_string()
}

fn parse_u64(s: &str) -> Result<u64, String> {
    if s.is_empty() {
        return Err("empty number".into());
    }
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(h, 16).map_err(|e| e.to_string())
    } else {
        s.parse::<u64>().map_err(|e| e.to_string())
    }
}

fn normalize_hex_addr(s: &str) -> Result<String, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("empty address".into());
    }
    let h = t.strip_prefix("0x").unwrap_or(t);
    if h.len() != 40 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("address must be 20-byte hex".into());
    }
    Ok(format!("0x{}", h.to_ascii_lowercase()))
}

fn normalize_tx_hash(s: &str) -> Result<String, String> {
    let t = s.trim();
    let h = t.strip_prefix("0x").unwrap_or(t);
    if h.len() != 64 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("tx hash must be 32-byte hex".into());
    }
    Ok(format!("0x{}", h.to_ascii_lowercase()))
}

fn strip_0x(s: &str) -> &str {
    s.strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s)
}

fn mix_hex_parts(parts: &[&str]) -> String {
    let mut bytes = [0u8; 32];
    for (i, p) in parts.iter().enumerate() {
        let clean = strip_0x(p);
        let raw = clean.as_bytes();
        for (j, b) in raw.iter().enumerate() {
            let idx = (i * 17 + j * 3) % 32;
            bytes[idx] ^= *b;
            bytes[(idx + 7) % 32] = bytes[(idx + 7) % 32].wrapping_add(b.rotate_left(1));
        }
    }
    let mut out = String::with_capacity(64);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn seed_byte(seed: &str, i: usize) -> u8 {
    let clean = strip_0x(seed);
    let chars: Vec<char> = clean.chars().collect();
    if chars.is_empty() {
        return 0;
    }
    let a = chars[i % chars.len()];
    let b = chars[(i * 3 + 1) % chars.len()];
    let nibble = |c: char| c.to_digit(16).unwrap_or(0) as u8;
    (nibble(a) << 4) | nibble(b)
}

pub fn bpm_from_seed(seed: &str) -> u32 {
    72 + (seed_byte(seed, 0) as u32 % 76) // 72..147 — more musical band
}

#[derive(Clone, Copy)]
enum Feel {
    Breaks,
    Techno,
    Ambient,
    Jungle,
    Footwork,
    Dub,
}

fn feel_from_seed(seed: &str) -> Feel {
    match seed_byte(seed, 11) % 6 {
        0 => Feel::Breaks,
        1 => Feel::Techno,
        2 => Feel::Ambient,
        3 => Feel::Jungle,
        4 => Feel::Footwork,
        _ => Feel::Dub,
    }
}

fn feel_name(f: Feel) -> &'static str {
    match f {
        Feel::Breaks => "breaks",
        Feel::Techno => "techno",
        Feel::Ambient => "ambient",
        Feel::Jungle => "jungle",
        Feel::Footwork => "footwork",
        Feel::Dub => "dub",
    }
}

fn scale_from_seed(seed: &str) -> &'static str {
    const SCALES: [&str; 8] = [
        "A:minor",
        "D:dorian",
        "E:phrygian",
        "C:major",
        "G:mixolydian",
        "F#:minor",
        "Bb:lydian",
        "C:chromatic",
    ];
    SCALES[seed_byte(seed, 12) as usize % SCALES.len()]
}

/// Compose a chaotic, high-variance Strudel pattern from seed bytes.
fn generate_pattern(seed: &str, label: &str) -> String {
    let bpm = 68 + (seed_byte(seed, 0) as u32 % 90); // 68..157 wider chaos
    let feel = feel_from_seed(seed);
    let scale = scale_from_seed(seed);
    let short = short_seed(seed);

    // Salt every pick with more seed indices so small seed changes flip structure hard
    let mut layers: Vec<String> = Vec::new();
    let n_layers = 3 + (seed_byte(seed, 30) % 5) as usize; // 3..7

    let mut bag: Vec<String> = Vec::new();
    bag.push(chaos_kick(feel, seed, 1));
    bag.push(chaos_hats(feel, seed, 2));
    if let Some(s) = chaos_perc(feel, seed, 6) {
        bag.push(s);
    }
    if let Some(b) = chaos_bass(feel, seed, scale, 9) {
        bag.push(b);
    }
    if let Some(l) = chaos_lead(feel, seed, scale, 18) {
        bag.push(l);
    }
    if let Some(x) = chaos_fx(feel, seed, 21) {
        bag.push(x);
    }
    // Extra wild layers from fragment pools
    bag.push(chaos_wild_layer(seed, 25, scale));
    if seed_byte(seed, 31) > 100 {
        bag.push(chaos_wild_layer(seed, 33, scale));
    }

    // Shuffle-like reorder via seed permutation
    let mut order: Vec<usize> = (0..bag.len()).collect();
    for i in 0..order.len() {
        let j = seed_byte(seed, 40 + i) as usize % order.len();
        order.swap(i, j);
    }
    for (i, &oi) in order.iter().take(n_layers).enumerate() {
        let mut layer = bag[oi].clone();
        layer = apply_chaos_mods(&layer, seed, 50 + i * 3);
        layers.push(layer);
    }

    // Outer chaos wrapper: sometimes slow/fast entire stack, or nest
    let wrap = seed_byte(seed, 70) % 5;
    let body = layers.join(",\n");
    let stack = match wrap {
        0 => format!("stack(\n{body}\n)"),
        1 => format!(
            "stack(\n{body}\n).slow({})",
            1 + seed_byte(seed, 71) % 3
        ),
        2 => format!(
            "stack(\n{body}\n).fast({})",
            1 + seed_byte(seed, 71) % 2
        ),
        3 => format!(
            "stack(\n{body}\n).jux(rev)"
        ),
        _ => format!(
            "stack(\n  stack(\n{body}\n  ).gain(0.85),\n  s(\"~ ~ oh ~\").gain(0.25)\n)"
        ),
    };

    format!(
        "// Chain → Pattern → Image\n\
         // source: {label}\n\
         // seed: {short}\n\
         // feel: {feel} · gen: procedural-chaos\n\
         setcpm({bpm}/4)\n\
         {stack}\n",
        feel = feel_name(feel),
    )
}

fn apply_chaos_mods(layer: &str, seed: &str, salt: usize) -> String {
    let mut out = layer.to_string();
    let r = seed_byte(seed, salt);
    // Don't double-wrap if already complex
    if r < 40 {
        return out;
    }
    if r < 90 && !out.contains(".sometimes") {
        out.push_str(".sometimes(rev)");
    } else if r < 130 && !out.contains(".rarely") {
        out.push_str(".rarely(ply(2))");
    } else if r < 170 && !out.contains(".fast(") {
        out.push_str(&format!(".fast({})", 1 + seed_byte(seed, salt + 1) % 3));
    } else if r < 210 && !out.contains(".slow(") {
        out.push_str(&format!(".slow({})", 2 + seed_byte(seed, salt + 1) % 3));
    } else if !out.contains(".gain(") {
        // leave
    } else if r > 230 {
        out.push_str(".jux(rev)");
    }
    out
}

fn chaos_kick(feel: Feel, seed: &str, salt: usize) -> String {
    let g = 0.55 + (seed_byte(seed, salt + 2) as f32 / 255.0) * 0.4;
    let pick = seed_byte(seed, salt) as usize;
    let pool: &[&str] = match feel {
        Feel::Breaks | Feel::Jungle => &[
            "[bd ~ bd <bd ~>]",
            "[bd bd ~ bd] [~ bd ~ <bd ~>]",
            "bd(<3,8>)",
            "bd(<5,16>)",
            "[bd ~ ~ bd]*2",
            "bd*2 ~ <bd bd> ~",
            "[bd <~ bd> bd ~] [~ bd ~ bd]",
        ],
        Feel::Techno | Feel::Footwork => &[
            "bd*4",
            "bd(<5,8>)",
            "[bd ~]*4",
            "bd*8",
            "bd(<3,8>)*2",
            "[bd bd ~ bd]*2",
        ],
        Feel::Ambient | Feel::Dub => &[
            "bd ~ ~ ~",
            "bd(<2,8>)",
            "[bd ~]*2",
            "bd ~ ~ <bd ~>",
            "~ bd ~ ~",
        ],
    };
    let pat = pool[pick % pool.len()];
    let mut line = format!("  s(\"{pat}\").gain({g:.2})");
    if seed_byte(seed, salt + 3) > 180 {
        line.push_str(&format!(
            ".room({:.2})",
            0.15 + (seed_byte(seed, salt + 4) as f32 / 255.0) * 0.5
        ));
    }
    line
}

fn chaos_hats(feel: Feel, seed: &str, salt: usize) -> String {
    let g = 0.12 + (seed_byte(seed, salt + 2) as f32 / 255.0) * 0.35;
    let pick = seed_byte(seed, salt) as usize;
    let pool: &[&str] = match feel {
        Feel::Breaks | Feel::Jungle | Feel::Footwork => &[
            "hh*8",
            "hh*16",
            "[hh ~ hh hh]*2",
            "hh(<5,8>)",
            "[hh hh] [~ hh] [hh ~] [<hh oh> hh]",
            "~ hh*4",
            "oh*4",
        ],
        Feel::Techno => &["hh*8", "hh*16", "[~ hh]*8", "hh(<7,16>)", "[hh]*12"],
        Feel::Ambient | Feel::Dub => &[
            "~ hh ~ hh",
            "[~ ~ hh ~]*2",
            "hh*2",
            "oh ~ ~ ~",
            "~ ~ <hh oh> ~",
        ],
    };
    let pat = pool[pick % pool.len()];
    let mut line = format!("  s(\"{pat}\").gain({g:.2})");
    match seed_byte(seed, salt + 5) % 4 {
        0 => line.push_str(".pan(sine.range(-0.8,0.8))"),
        1 => line.push_str(".pan(perlin.range(-0.9,0.9))"),
        2 => line.push_str(".jux(rev)"),
        _ => {}
    }
    line
}

fn chaos_perc(feel: Feel, seed: &str, salt: usize) -> Option<String> {
    if seed_byte(seed, salt) < 25 {
        return None;
    }
    let g = 0.3 + (seed_byte(seed, salt + 1) as f32 / 255.0) * 0.4;
    let pool: &[&str] = &[
        "~ cp ~ <cp rim>",
        "~ sd ~ sd",
        "[~ cp]*2",
        "~ <cp sd rim> ~ cp",
        "cp(<3,8>)",
        "~ ~ rim ~",
        "[~ sd]*2",
        "rim*2",
        "~ cp ~ ~ cp ~ cp ~",
    ];
    let pat = pool[seed_byte(seed, salt + 2) as usize % pool.len()];
    let mut line = format!("  s(\"{pat}\").gain({g:.2})");
    if matches!(feel, Feel::Dub | Feel::Ambient) || seed_byte(seed, salt + 3) > 160 {
        line.push_str(&format!(
            ".room({:.2})",
            0.25 + (seed_byte(seed, salt + 4) as f32 / 255.0) * 0.55
        ));
    }
    Some(line)
}

fn chaos_bass(feel: Feel, seed: &str, scale: &str, salt: usize) -> Option<String> {
    if seed_byte(seed, salt) < 20 {
        return None;
    }
    let g = 0.15 + (seed_byte(seed, salt + 1) as f32 / 255.0) * 0.28;
    let notes_pool: &[&str] = &[
        "0 ~ 3 ~ 5 ~ 7 ~",
        "0 0 3 5",
        "<0 2 3 7>/2",
        "0 ~ ~ ~",
        "0 <0 3> 0 <5 7>",
        "0 2 3 7",
        "0 ~ 3 5 7 ~",
        "<0 3> <5 7> <0 2> <3 8>",
        "0 3 5 7 8 7 5 3",
        "<0 5 7 10>/4",
        "0*4",
        "~ 0 ~ 3 ~ 5 ~ 0",
    ];
    let notes = notes_pool[seed_byte(seed, salt + 2) as usize % notes_pool.len()];
    let waves = ["sawtooth", "square", "triangle", "sine"];
    let wave = waves[seed_byte(seed, salt + 3) as usize % waves.len()];
    let lpf = 280 + (seed_byte(seed, salt + 4) as u32 % 2200);
    let room = (seed_byte(seed, salt + 5) as f32 / 255.0) * 0.55;
    let mut line = format!(
        "  n(\"{notes}\").scale(\"{scale}\").s(\"{wave}\").gain({g:.2}).lpf({lpf}).room({room:.2})"
    );
    if seed_byte(seed, salt + 6) > 200 {
        line.push_str(".sometimesBy(0.3, rev)");
    }
    let _ = feel;
    Some(line)
}

fn chaos_lead(feel: Feel, seed: &str, scale: &str, salt: usize) -> Option<String> {
    if seed_byte(seed, salt) < 70 {
        return None;
    }
    let g = 0.08 + (seed_byte(seed, salt + 1) as f32 / 255.0) * 0.18;
    let mel_pool: &[&str] = &[
        "<0 2 3 5 7 8>/2",
        "0 2 3 7 8 7 5 3",
        "0 0 3 5",
        "0 3 5 7 8 7",
        "0 ~ 5 ~",
        "<0 7> <3 5> <2 8> <5 10>",
        "0 5 3 7 ~ 8 5 3",
        "<0 2 4 7>*2",
    ];
    let mel = mel_pool[seed_byte(seed, salt + 2) as usize % mel_pool.len()];
    let mut line = format!(
        "  n(\"{mel}\").scale(\"{scale}\").s(\"triangle\").gain({g:.2}).room(0.45)"
    );
    if seed_byte(seed, salt + 3) > 110 {
        line.push_str(".delay(0.25).delaytime(0.33).delayfeedback(0.4)");
    }
    if seed_byte(seed, salt + 4) > 200 {
        line.push_str(".jux(rev)");
    }
    let _ = feel;
    Some(line)
}

fn chaos_fx(feel: Feel, seed: &str, salt: usize) -> Option<String> {
    if seed_byte(seed, salt) < 80 {
        return None;
    }
    let pick = seed_byte(seed, salt + 1) % 6;
    Some(match pick {
        0 => format!(
            "  s(\"cr\").slow({}).gain(0.12).room(0.85)",
            2 + seed_byte(seed, salt + 2) % 5
        ),
        1 => "  s(\"~ ~ oh ~\").gain(0.32)".into(),
        2 => "  s(\"rim*2\").gain(0.22).pan(perlin.range(-0.9,0.9))".into(),
        3 => format!(
            "  s(\"hh*4\").gain(0.15).speed({})",
            if seed_byte(seed, salt + 2) % 2 == 0 {
                "-1"
            } else {
                "0.5"
            }
        ),
        4 => "  s(\"[~ cp]*4\").gain(0.2).rarely(fast(2))".into(),
        _ => match feel {
            Feel::Dub | Feel::Ambient => "  s(\"bd ~ ~ ~\").gain(0.2).room(0.9)".into(),
            _ => "  s(\"~ sd ~ <sd ~>\").gain(0.28)".into(),
        },
    })
}

fn chaos_wild_layer(seed: &str, salt: usize, scale: &str) -> String {
    let pick = seed_byte(seed, salt) % 8;
    match pick {
        0 => format!(
            "  s(\"bd(<{},8>)\").gain(0.4).rarely(ply(2))",
            2 + seed_byte(seed, salt + 1) % 5
        ),
        1 => format!(
            "  n(\"<0 3 5 {}>\").scale(\"{scale}\").s(\"sawtooth\").gain(0.12).lpf({})",
            7 + seed_byte(seed, salt + 1) % 5,
            600 + seed_byte(seed, salt + 2) as u32 % 1800
        ),
        2 => "  s(\"[hh ~]*8\").gain(0.2).jux(rev)".into(),
        3 => format!(
            "  s(\"cp(<{},16>)\").gain(0.35)",
            3 + seed_byte(seed, salt + 1) % 7
        ),
        4 => "  s(\"~ oh ~ oh\").gain(0.25).fast(2)".into(),
        5 => format!(
            "  note(\"{a} ~ {b} ~\").s(\"triangle\").gain(0.14).room(0.5)",
            a = ["a3", "c4", "e3", "g3", "bb3"][seed_byte(seed, salt + 1) as usize % 5],
            b = ["e4", "g3", "a3", "d4", "c4"][seed_byte(seed, salt + 2) as usize % 5],
        ),
        6 => "  s(\"rim ~ rim rim\").gain(0.2).sometimes(slow(2))".into(),
        _ => format!(
            "  n(\"0 ~ {} ~\").scale(\"{scale}\").s(\"square\").gain(0.1).room(0.3)",
            seed_byte(seed, salt + 1) % 12
        ),
    }
}

/// Compact Strudel reference injected into every Ollama call (keeps small models honest).
const STRUDEL_DOCS: &str = r#"
STRUDEL MINI REFERENCE (only use these forms):

Tempo:
  setcpm(120/4)

Sound layers — ONE mini-notation STRING per s()/note()/n():
  s("bd*4")
  s("[bd ~ bd bd] [~ bd ~ bd]")
  s("hh*8")
  s("~ cp ~ <cp rim>")
  s("bd(<3,8>)")
  note("c3 eb3 g3").s("sawtooth")
  n("0 2 3 7").scale("A:minor").s("sawtooth").gain(0.2).lpf(1200).room(0.3)

Combine with stack:
  setcpm(128/4)
  stack(
    s("[bd ~ bd bd] [~ bd ~ bd]").gain(0.85),
    s("hh*8").gain(0.3),
    s("~ cp ~ cp").gain(0.5),
    n("0 2 3 7").scale("A:minor").s("triangle").gain(0.18).room(0.4)
  )

LEGAL mini tokens inside the string: letters (bd hh cp sd oh rim), ~ rest, * repeat,
[] groups, <> alternation, () euclid like bd(<3,8>), numbers, spaces.
Chain methods AFTER the call: .gain() .room() .lpf() .pan() .slow() .fast() .delay()

FORBIDDEN (never invent these):
  single_stack, withRest, withAngleBracket, withPolyrhythm, repeat(, .setcpm,
  s("a","b") multiple strings, initHydra, import, require, class, function declarations.
"#;

/// Ask a local Ollama model for Strudel code; validate output strictly.
pub fn generate_with_ollama(
    seed: &str,
    label: &str,
    bpm: u32,
    ollama_url: &str,
    model: &str,
) -> Result<String, String> {
    let feel = feel_name(feel_from_seed(seed));
    let scale = scale_from_seed(seed);
    let short = short_seed(seed);

    let system = format!(
        "You are a Strudel live-coding pattern writer.\n\
         {STRUDEL_DOCS}\n\
         Reply with ONLY executable Strudel code. No markdown. No prose."
    );
    let user = format!(
        "Write one playable pattern.\n\
         Required shape:\n\
         setcpm({bpm}/4)\n\
         stack(\n\
           /* 4-6 layers using s(\"...\") and optionally n(\"...\").scale(\"{scale}\") */\n\
         )\n\
         Feel/mood: {feel}. Make it CHAOTIC and irregular: euclid bd(<n,8>), nested [],\n\
         <<>> alternation, rests, .sometimes(rev)/.rarely(ply(2))/.jux(rev) on some layers.\n\
         Do NOT emit boring plain bd*4 + hh*8 only.\n\
         Source label (put in // comments only): {label}\n\
         Seed (comment only): {short}\n"
    );

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| e.to_string())?;
    let base = ollama_url.trim_end_matches('/');

    // Prefer /api/chat so the docs ride in a system message; fall back to /api/generate.
    let text = match ollama_chat(&client, base, model, &system, &user) {
        Ok(t) => t,
        Err(chat_err) => ollama_generate(
            &client,
            base,
            model,
            &format!("{system}\n\n{user}"),
        )
        .map_err(|e| format!("chat: {chat_err}; generate: {e}"))?,
    };

    sanitize_strudel_code(&text, bpm, label, &short, feel)
}

/// Prompt-driven Ollama generate for the AI Pattern panel (no chain fetch).
pub fn generate_from_prompt(
    prompt: &str,
    ollama_url: &str,
    model: &str,
) -> Result<(String, u32, String), String> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err("empty prompt".into());
    }
    let seed = mix_hex_parts(&[prompt, "ai-pattern"]);
    let bpm = bpm_from_seed(&seed);
    let label = format!("ai:{}", prompt.chars().take(48).collect::<String>());
    match generate_with_ollama(&seed, &label, bpm, ollama_url, model) {
        Ok(code) => Ok((code, bpm, format!("ollama:{model}"))),
        Err(e) => {
            let code = generate_pattern(&seed, &label);
            Ok((
                code,
                bpm,
                format!("procedural (ollama failed: {e})"),
            ))
        }
    }
}

fn ollama_chat(
    client: &reqwest::blocking::Client,
    base: &str,
    model: &str,
    system: &str,
    user: &str,
) -> Result<String, String> {
    let body = json!({
        "model": model,
        "stream": false,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user }
        ],
        "options": { "temperature": 0.35, "num_predict": 600 }
    });
    let resp = client
        .post(format!("{base}/api/chat"))
        .json(&body)
        .send()
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let v: Value = resp.json().map_err(|e| e.to_string())?;
    v.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "missing message.content".into())
}

fn ollama_generate(
    client: &reqwest::blocking::Client,
    base: &str,
    model: &str,
    prompt: &str,
) -> Result<String, String> {
    let body = json!({
        "model": model,
        "prompt": prompt,
        "stream": false,
        "options": { "temperature": 0.35, "num_predict": 600 }
    });
    let resp = client
        .post(format!("{base}/api/generate"))
        .json(&body)
        .send()
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let v: Value = resp.json().map_err(|e| e.to_string())?;
    v.get("response")
        .and_then(|r| r.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "missing response".into())
}

fn sanitize_strudel_code(
    raw: &str,
    bpm: u32,
    label: &str,
    short_seed: &str,
    feel: &str,
) -> Result<String, String> {
    let mut t = raw.trim().to_string();
    if let Some(start) = t.find("```") {
        let after = &t[start + 3..];
        let after = after
            .strip_prefix("js")
            .or_else(|| after.strip_prefix("javascript"))
            .or_else(|| after.strip_prefix("strudel"))
            .unwrap_or(after)
            .trim_start();
        if let Some(end) = after.find("```") {
            t = after[..end].trim().to_string();
        }
    }

    // Drop leading prose
    let lines: Vec<&str> = t
        .lines()
        .skip_while(|l| {
            let s = l.trim();
            s.is_empty()
                || (!s.starts_with("//")
                    && !s.starts_with("setcpm")
                    && !s.starts_with("stack")
                    && !s.starts_with("s(")
                    && !s.starts_with("$:")
                    && !s.starts_with("note(")
                    && !s.starts_with("n("))
        })
        .collect();
    let mut code = lines.join("\n").trim().to_string();
    if code.is_empty() {
        return Err("empty model output".into());
    }

    validate_playable_strudel(&code)?;

    // Normalize tempo if missing / wrongly chained
    if code.contains(".setcpm") {
        return Err("invalid .setcpm (must be top-level setcpm(...))".into());
    }
    if !code.contains("setcpm(") {
        code = format!("setcpm({bpm}/4)\n{code}");
    }

    // Strip duplicate source/seed comment noise from the model, keep our header.
    let body_lines: Vec<&str> = code
        .lines()
        .filter(|l| {
            let t = l.trim();
            !(t.starts_with("// Source:")
                || t.starts_with("// Seed:")
                || t.starts_with("// source:")
                || t.starts_with("// seed:")
                || t.starts_with("// Chain →")
                || t.starts_with("// feel:")
                || t.starts_with("// Break,")
                || t.starts_with("// 4 to 6"))
        })
        .collect();
    let body = body_lines.join("\n").trim().to_string();
    if body.is_empty() {
        return Err("empty after cleanup".into());
    }
    validate_playable_strudel(&body)?;

    let header = format!(
        "// Chain → Pattern → Image\n\
         // source: {label}\n\
         // seed: {short_seed}\n\
         // feel: {feel} · gen: ollama\n"
    );
    Ok(format!("{header}{body}\n"))
}

fn validate_playable_strudel(code: &str) -> Result<(), String> {
    const FORBIDDEN: &[&str] = &[
        "single_stack",
        "withRest",
        "withAngleBracket",
        "withPolyrhythm",
        "with_rest",
        "initHydra",
        "require(",
        "import ",
        "class ",
        "function ",
        ".setcpm",
    ];
    for bad in FORBIDDEN {
        if code.contains(bad) {
            return Err(format!("forbidden API `{bad}`"));
        }
    }
    // Fake helper often emitted by small models
    if code.contains("repeat(") && !code.contains("hh*") {
        // allow *repeat in mini notation only — JS repeat( is bad
        if code
            .lines()
            .any(|l| l.trim_start().starts_with("repeat(") || l.contains(" repeat("))
        {
            return Err("forbidden JS repeat()".into());
        }
    }
    if !code.contains("stack(") {
        return Err("missing stack(".into());
    }
    if !code.contains("s(\"") && !code.contains("note(\"") && !code.contains("n(\"") {
        return Err("missing s(\"/note(\"/n(\"".into());
    }
    // Catch s("a", "b") multi-arg anti-pattern roughly
    if code.contains("s(\"") {
        // look for s("...", "...")
        let needle = "s(\"";
        let mut i = 0;
        while let Some(rel) = code[i..].find(needle) {
            let start = i + rel + needle.len();
            if let Some(end_q) = code[start..].find('"') {
                let after = start + end_q + 1;
                let rest = code[after..].trim_start();
                if rest.starts_with(',') {
                    return Err("s() must take one mini-notation string".into());
                }
                i = after;
            } else {
                break;
            }
        }
    }
    Ok(())
}

/// ERC-721 tokenURI(uint256) selector 0xc87b56dd + padded token id.
fn encode_token_uri_call(token_id: u64) -> String {
    format!("0xc87b56dd{:064x}", token_id)
}

fn decode_abi_string(raw: &Value) -> Option<String> {
    let hex = strip_0x(raw.as_str()?);
    if hex.len() < 128 {
        return None;
    }
    // offset
    let offset = u64::from_str_radix(&hex[0..64], 16).ok()? as usize * 2;
    if hex.len() < offset + 64 {
        return None;
    }
    let len = u64::from_str_radix(&hex[offset..offset + 64], 16).ok()? as usize;
    let start = offset + 64;
    let end = start + len * 2;
    if hex.len() < end {
        return None;
    }
    let mut bytes = Vec::with_capacity(len);
    let slice = &hex[start..end];
    for i in (0..slice.len()).step_by(2) {
        let b = u8::from_str_radix(&slice[i..i + 2], 16).ok()?;
        bytes.push(b);
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codegen_is_deterministic() {
        let a = generate_pattern("deadbeefcaffee00aabbccddeeff00112233445566778899", "t");
        let b = generate_pattern("deadbeefcaffee00aabbccddeeff00112233445566778899", "t");
        assert_eq!(a, b);
        assert!(a.contains("setcpm("));
        assert!(a.contains("stack("));
        assert!(a.contains("feel:"));
    }

    #[test]
    fn different_seeds_differ() {
        let a = generate_pattern("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "a");
        let b = generate_pattern("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "b");
        assert_ne!(a, b);
    }

    #[test]
    fn sanitize_extracts_code() {
        let raw = "Sure!\n```js\nsetcpm(120/4)\nstack(\n  s(\"bd*4\")\n)\n```\n";
        let out = sanitize_strudel_code(raw, 120, "lab", "abc", "techno").unwrap();
        assert!(out.contains("setcpm"));
        assert!(out.contains("stack"));
    }

    #[test]
    fn reject_fake_ollama_api() {
        let raw = r#"
setcpm(107/4)
single_stack(() => {
  s("drums", "scale(\"A:minor\")").withRest();
});
"#;
        assert!(sanitize_strudel_code(raw, 107, "lab", "abc", "breaks").is_err());
    }

    #[test]
    fn reject_multi_arg_s() {
        let raw = r#"
setcpm(100/4)
stack(
  s("bd*", "hh*8")
)
"#;
        assert!(sanitize_strudel_code(raw, 100, "lab", "abc", "breaks").is_err());
    }

    #[test]
    fn mix_changes_with_input() {
        let a = mix_hex_parts(&["0xaaa"]);
        let b = mix_hex_parts(&["0xbbb"]);
        assert_ne!(a, b);
    }

    #[test]
    fn inject_keeps_user_stack() {
        let src = r#"// mine
setcpm(100/4)
stack(
  s("bd*4").gain(0.8)
)
"#;
        let seed = mix_hex_parts(&["0xdeadbeef", "nft"]);
        let out = inject_seed_math(src, &seed, "NFT test", Some("meta"));
        assert!(out.contains("// @chain-seed"));
        assert!(out.contains("const seedBpm"));
        assert!(out.contains("const seedGain"));
        assert!(out.contains(r#"s("bd*4").gain(0.8)"#));
        assert!(out.contains("setcpm(seedBpm/4)"));
        // Second inject refreshes block, still keeps stack
        let out2 = inject_seed_math(&out, &seed, "NFT test", None);
        assert_eq!(out2.matches("// @chain-seed-end").count(), 1);
        assert!(out2.contains(r#"s("bd*4").gain(0.8)"#));
    }

    #[test]
    fn morph_seed_moves() {
        let base = mix_hex_parts(&["0xabc"]);
        let a = morph_seed(&base, 0);
        let b = morph_seed(&base, 1);
        assert_ne!(a, b);
        assert_ne!(a, base);
    }

    #[test]
    fn seed_bus_values_has_freq() {
        let v = seed_bus_values(&mix_hex_parts(&["zz"]));
        assert!(v.get("freq").and_then(|x| x.as_f64()).is_some());
    }
}
