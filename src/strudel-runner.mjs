#!/usr/bin/env node
/**
 * Strudel Node runner for ide-native: stdin/stdout JSON protocol.
 * Run from repo root: node strudel/scripts/strudel-runner.mjs
 * Or from strudel: node scripts/strudel-runner.mjs
 * Commands: eval, start, pause, stop, now, queryArc
 * Only JSON lines go to stdout; all other logs go to stderr so Rust can parse.
 */
import { createInterface } from 'readline';
import { pathToFileURL } from 'url';
import path from 'path';
import fs from 'fs';
import { fileURLToPath } from 'url';
import { format } from 'util';

// Redirect console.log to stderr so stdout contains only our JSON responses
const stderrWrite = process.stderr.write.bind(process.stderr);
console.log = (...args) => {
  stderrWrite(format(...args) + '\n');
};
console.info = console.log;
console.debug = console.log;
console.warn = (...args) => {
  stderrWrite(format(...args) + '\n');
};

/** Флаг отмены скачивания (команда canceldownloads). Сбрасывается при loadsamples. */
let cancelDownloads = false;

/** Отправить строку прогресса в stdout для IDE (полоса прогресса). */
function sendProgress(obj) {
  process.stdout.write(JSON.stringify(obj) + '\n');
}

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const strudelRoot = path.resolve(__dirname, '..');
const packagesPath = path.join(strudelRoot, 'packages');
const strudelFilesEnv = process.env.STRUDEL_FILES;
const strudelFiles = strudelFilesEnv
  ? path.resolve(strudelFilesEnv)
  : path.resolve(strudelRoot, '..', 'strudel_files');
if (!fs.existsSync(strudelFiles)) {
  fs.mkdirSync(strudelFiles, { recursive: true });
}
function githubSampleMapUrl(spec) {
  if (!spec.startsWith('github:')) return null;
  let p = spec.slice(7).replace(/\/$/, '');
  const parts = p.split('/');
  const user = parts[0];
  const repo = parts[1] || 'samples';
  const branch = parts[2] || 'main';
  return `https://raw.githubusercontent.com/${user}/${repo}/${branch}/strudel.json`;
}

function githubBaseUrl(spec) {
  if (!spec.startsWith('github:')) return spec;
  let p = spec.slice(7).replace(/\/$/, '');
  const parts = p.split('/');
  const user = parts[0];
  const repo = parts[1] || 'samples';
  const branch = parts[2] || 'main';
  return `https://raw.githubusercontent.com/${user}/${repo}/${branch}/`;
}

function collectRelPaths(value) {
  if (Array.isArray(value)) {
    return value.filter((v) => typeof v === 'string');
  }
  if (typeof value === 'object' && value !== null) {
    return Object.values(value).flat().filter((v) => typeof v === 'string');
  }
  return [];
}

function findPackDir(root, jsonStem, json) {
  if (json._dir) {
    const p = path.join(root, json._dir);
    if (fs.existsSync(p)) return p;
  }
  const exact = path.join(root, jsonStem);
  if (fs.existsSync(exact)) return exact;
  try {
    for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
      if (entry.isDirectory() && entry.name.toLowerCase() === jsonStem.toLowerCase()) {
        return path.join(root, entry.name);
      }
    }
  } catch {}
  if (json._base) {
    const m = json._base.match(/raw\.githubusercontent\.com\/[^/]+\/([^/]+)\//);
    if (m && m[1].toLowerCase() !== jsonStem.toLowerCase()) {
      const p = path.join(root, m[1]);
      if (fs.existsSync(p)) return p;
    }
  }
  return exact;
}

function detectLocalBase(packDir, firstSamplePath) {
  const decoded = decodeURIComponent(firstSamplePath);
  if (fs.existsSync(path.join(packDir, decoded))) return packDir;
  try {
    for (const entry of fs.readdirSync(packDir, { withFileTypes: true })) {
      if (entry.isDirectory()) {
        const candidate = path.join(packDir, entry.name, decoded);
        if (fs.existsSync(candidate)) return path.join(packDir, entry.name);
      }
    }
  } catch {}
  return packDir;
}

async function downloadFilesToPack(localBase, writeDir, baseUrl, entries) {
  let total = 0;
  let missing = 0;
  for (const [key, value] of entries) {
    if (key.startsWith('_') || typeof value !== 'object') continue;
    const relPaths = collectRelPaths(value);
    for (const relPath of relPaths) {
      const localRelPath = decodeURIComponent(relPath);
      if (fs.existsSync(path.join(localBase, localRelPath))) continue;
      if (writeDir !== localBase && fs.existsSync(path.join(writeDir, localRelPath))) continue;
      missing++;
    }
  }
  if (missing === 0) {
    console.log(`[runner] pack ${path.basename(writeDir)}: all files present, skip`);
    return 0;
  }
  const packName = path.basename(writeDir);
  console.log(`[runner] pack ${packName}: downloading ${missing} files…`);
  sendProgress({ type: 'download_progress', pack: packName, current: 0, total: missing });
  for (const [key, value] of entries) {
    if (cancelDownloads) break;
    if (key.startsWith('_') || typeof value !== 'object') continue;
    const relPaths = collectRelPaths(value);
    for (const relPath of relPaths) {
      if (cancelDownloads) break;
      const localRelPath = decodeURIComponent(relPath);
      if (fs.existsSync(path.join(localBase, localRelPath))) continue;
      const filePath = path.join(writeDir, localRelPath);
      if (fs.existsSync(filePath)) continue;
      const fileDir = path.dirname(filePath);
      if (!fs.existsSync(fileDir)) fs.mkdirSync(fileDir, { recursive: true });
      const url = baseUrl + relPath;
      try {
        const r = await fetch(url, { signal: AbortSignal.timeout(5000) });
        if (!r.ok) {
          console.warn('[runner] skip', localRelPath, r.status);
          continue;
        }
        const buf = Buffer.from(await r.arrayBuffer());
        fs.writeFileSync(filePath, buf);
        total++;
        sendProgress({ type: 'download_progress', pack: packName, current: total, total: missing });
        console.log(`[runner]   + ${localRelPath}`);
      } catch (e) {
        console.warn('[runner] skip', localRelPath, e.message);
      }
    }
  }
  console.log(`[runner] pack ${packName}: done, ${total} downloaded`);
  return total;
}

async function downloadSamplesToCache(spec) {
  let jsonUrl = githubSampleMapUrl(spec);
  let defaultBase = githubBaseUrl(spec);
  if (!jsonUrl) {
    if (spec.startsWith('http://') || spec.startsWith('https://')) {
      jsonUrl = spec;
      defaultBase = spec.replace(/\/[^/]*$/, '/');
    } else {
      console.warn('[runner] samples() only supports github:user/repo or direct http(s) URL');
      return null;
    }
  }

  const jsonFilename = path.basename(new URL(jsonUrl).pathname, '.json');
  const manifestPath = path.join(strudelFiles, `${jsonFilename}.json`);

  let json;
  if (fs.existsSync(manifestPath)) {
    try {
      json = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
    } catch { json = null; }
  }

  if (!json) {
    try {
      const res = await fetch(jsonUrl, { signal: AbortSignal.timeout(10000) });
      if (!res.ok) throw new Error(`${res.status}`);
      json = await res.json();
      fs.writeFileSync(manifestPath, JSON.stringify(json, null, 2), 'utf8');
    } catch (e) {
      console.warn(`[runner] failed to fetch manifest ${jsonFilename}: ${e.message}`);
      return null;
    }
  }

  const baseUrl = json._base || defaultBase;
  const packDir = findPackDir(strudelFiles, jsonFilename, json);
  const writeDir = path.join(strudelFiles, jsonFilename);

  const allRelPaths = [];
  for (const [key, value] of Object.entries(json)) {
    if (key.startsWith('_') || typeof value !== 'object') continue;
    allRelPaths.push(...collectRelPaths(value));
  }
  const localBase = allRelPaths.length > 0
    ? detectLocalBase(packDir, allRelPaths[0])
    : packDir;

  try {
    const total = await downloadFilesToPack(localBase, writeDir, baseUrl, Object.entries(json));
    if (total > 0) console.log(`[runner] downloaded ${total} samples for pack "${jsonFilename}"`);
  } catch (e) {
    console.warn(`[runner] download error for ${jsonFilename}: ${e.message}`);
  }
  return packDir;
}

async function downloadInlineSamplesToCache(sampleMap, baseUrl, packName) {
  if (!baseUrl || !packName) return;
  const manifestPath = path.join(strudelFiles, `${packName}.json`);
  const packDir = findPackDir(strudelFiles, packName, { _base: baseUrl });

  if (fs.existsSync(manifestPath) && fs.existsSync(packDir)) {
    return;
  }

  const writeDir = path.join(strudelFiles, packName);
  const allRelPaths = [];
  for (const [key, value] of Object.entries(sampleMap)) {
    if (key.startsWith('_') || typeof value !== 'object') continue;
    allRelPaths.push(...collectRelPaths(value));
  }
  const localBase = allRelPaths.length > 0
    ? detectLocalBase(packDir, allRelPaths[0])
    : packDir;

  try {
    const total = await downloadFilesToPack(localBase, writeDir, baseUrl, Object.entries(sampleMap));
    if (total > 0) console.log(`[runner] downloaded ${total} samples for inline pack "${packName}"`);
  } catch (e) {
    console.warn(`[runner] download error for ${packName}: ${e.message}`);
  }

  if (!fs.existsSync(manifestPath)) {
    const manifest = { _base: baseUrl, ...sampleMap };
    fs.writeFileSync(manifestPath, JSON.stringify(manifest, null, 2), 'utf8');
  }
}

async function loadRepl() {
  const coreReplPath = pathToFileURL(path.join(packagesPath, 'core', 'repl.mjs')).href;
  const coreIndexPath = pathToFileURL(path.join(packagesPath, 'core', 'index.mjs')).href;
  const miniPath = pathToFileURL(path.join(packagesPath, 'mini', 'index.mjs')).href;
  const tonalPath = pathToFileURL(path.join(packagesPath, 'tonal', 'tonal.mjs')).href;
  const transpilerPath = pathToFileURL(path.join(packagesPath, 'transpiler', 'transpiler.mjs')).href;

  const [{ repl }, { transpiler }, core, mini, tonal] = await Promise.all([
    import(coreReplPath),
    import(transpilerPath),
    import(coreIndexPath),
    import(miniPath),
    import(tonalPath),
  ]);

  // Поднимем DSL (mini, tonal, core) в globalThis так же, как это делает web.mjs,
  // но без webaudio/superdough (нам нужен только паттерн, не звук в браузере).
  const { evalScope, hush, evaluate } = core;
  if (typeof evalScope === 'function') {
    try {
      await evalScope(
        evalScope,
        core,
        mini,
        tonal,
        { hush, evaluate },
      );
    } catch (err) {
      console.warn('[runner] evalScope preload failed:', err);
    }
  }
  // Enable mini notation for all strings so s("bd sd, hh*16") expands to per-event sounds.
  if (typeof globalThis.miniAllStrings === 'function') {
    globalThis.miniAllStrings();
  }

  // Widget slider(v, min, max) : pattern constant — la valeur lue est dans le code.
  // Le panneau natif détecte ces appels et réécrit la valeur (source de vérité = code).
  const sliderValues = {};
  globalThis.slider = (value, _min, _max) => core.pure(value);
  globalThis.sliderWithID = (id, value, _min, _max) => {
    sliderValues[id] = value;
    return core.ref(() => sliderValues[id]);
  };
  // toggler(v) : case on/off inline — 1 = actif, 0 = coupé (typiquement .mask(toggler(1))).
  globalThis.toggler = (value) => core.pure(value);

  // samples(): download GitHub sample packs to cache and register so the pattern runs.
  let fetchSampleMap, processSampleMap, registerSampleSource;
  try {
    globalThis.getAudioContext = () => ({
      createBufferSource: () => ({ buffer: null, connect: () => {}, start: () => {}, stop: () => {}, playbackRate: { value: 1 }, detune: { value: 0 } }),
      createGain: () => ({ gain: { value: 1 }, connect: () => {} }),
      createBuffer: () => ({}),
      currentTime: 0,
    });
    const superdoughPath = pathToFileURL(path.join(packagesPath, 'superdough', 'index.mjs')).href;
    const samplerPath = pathToFileURL(path.join(packagesPath, 'superdough', 'sampler.mjs')).href;
    await import(superdoughPath);
    const sampler = await import(samplerPath);
    fetchSampleMap = sampler.fetchSampleMap;
    processSampleMap = sampler.processSampleMap;
    registerSampleSource = sampler.registerSampleSource;
  } catch (e) {
    console.warn('[runner] superdough/sampler not available, samples() will only download to cache:', e.message);
  }
  if (typeof globalThis.samples !== 'function') {
    globalThis.samples = async (sampleMap, baseUrl = '', options = {}) => {
      if (typeof sampleMap === 'string') {
        await downloadSamplesToCache(sampleMap);
        if (fetchSampleMap && processSampleMap && registerSampleSource) {
          const [json, base] = await fetchSampleMap(sampleMap);
          processSampleMap(json, (key, bank) => registerSampleSource(key, bank, { baseUrl: base || baseUrl }), base || baseUrl);
        }
      } else {
        if (baseUrl && typeof baseUrl === 'string' && baseUrl.startsWith('http')) {
          const urlParts = baseUrl.replace(/\/+$/, '').split('/');
          const packName = urlParts[urlParts.length - 1] || 'inline-samples';
          await downloadInlineSamplesToCache(sampleMap, baseUrl, packName);
        }
        if (processSampleMap && registerSampleSource) {
          processSampleMap(sampleMap, (key, bank) => registerSampleSource(key, bank, { baseUrl }), baseUrl);
        }
      }
    };
    if (globalThis.strudelScope) globalThis.strudelScope.samples = globalThis.samples;
  }

  // Helper: load a curated set of default sample packs into the sampler and cache.
  // This roughly mirrors what the web IDE does on startup.
  globalThis.__loadDefaultSamples = async function loadDefaultSamples() {
    const samplesFn = globalThis.samples;
    if (typeof samplesFn !== 'function') {
      console.warn('[runner] samples() is not available, cannot load default packs');
      return;
    }
    // Base CDN for official packs.
    let base = 'https://strudel.b-cdn.net';
    // Allow overriding via env if needed.
    if (process.env.STRUDEL_BASE_URL && /^https?:/i.test(process.env.STRUDEL_BASE_URL)) {
      base = process.env.STRUDEL_BASE_URL.replace(/\/+$/, '');
    }

    try {
      await Promise.all([
        samplesFn(`${base}/piano.json`, `${base}/piano/`, { prebake: true }),
        samplesFn(`${base}/vcsl.json`, `${base}/VCSL/`, { prebake: true }),
        samplesFn(`${base}/tidal-drum-machines.json`, `${base}/tidal-drum-machines/machines/`, {
          prebake: true,
          tag: 'drum-machines',
        }),
        samplesFn(`${base}/uzu-drumkit.json`, `${base}/uzu-drumkit/`, {
          prebake: true,
          tag: 'drum-machines',
        }),
        samplesFn(`${base}/uzu-wavetables.json`, `${base}/uzu-wavetables/`, { prebake: true }),
        samplesFn(`${base}/mridangam.json`, `${base}/mrid/`, { prebake: true, tag: 'drum-machines' }),
        samplesFn(
          {
            casio: ['casio/high.wav', 'casio/low.wav', 'casio/noise.wav'],
            crow: ['crow/000_crow.wav', 'crow/001_crow2.wav', 'crow/002_crow3.wav', 'crow/003_crow4.wav'],
            insect: [
              'insect/000_everglades_conehead.wav',
              'insect/001_robust_shieldback.wav',
              'insect/002_seashore_meadow_katydid.wav',
            ],
            wind: [
              'wind/000_wind1.wav', 'wind/001_wind10.wav', 'wind/002_wind2.wav', 'wind/003_wind3.wav',
              'wind/004_wind4.wav', 'wind/005_wind5.wav', 'wind/006_wind6.wav', 'wind/007_wind7.wav',
              'wind/008_wind8.wav', 'wind/009_wind9.wav',
            ],
            jazz: [
              'jazz/000_BD.wav', 'jazz/001_CB.wav', 'jazz/002_FX.wav', 'jazz/003_HH.wav',
              'jazz/004_OH.wav', 'jazz/005_P1.wav', 'jazz/006_P2.wav', 'jazz/007_SN.wav',
            ],
            metal: [
              'metal/000_0.wav', 'metal/001_1.wav', 'metal/002_2.wav', 'metal/003_3.wav',
              'metal/004_4.wav', 'metal/005_5.wav', 'metal/006_6.wav', 'metal/007_7.wav',
              'metal/008_8.wav', 'metal/009_9.wav',
            ],
            east: [
              'east/000_nipon_wood_block.wav', 'east/001_ohkawa_mute.wav', 'east/002_ohkawa_open.wav',
              'east/003_shime_hi.wav', 'east/004_shime_hi_2.wav', 'east/005_shime_mute.wav',
              'east/006_taiko_1.wav', 'east/007_taiko_2.wav', 'east/008_taiko_3.wav',
            ],
            space: [
              'space/000_0.wav', 'space/001_1.wav', 'space/002_11.wav', 'space/003_12.wav',
              'space/004_13.wav', 'space/005_14.wav', 'space/006_15.wav', 'space/007_16.wav',
              'space/008_17.wav', 'space/009_18.wav', 'space/010_2.wav', 'space/011_3.wav',
              'space/012_4.wav', 'space/013_5.wav', 'space/014_6.wav', 'space/015_7.wav',
              'space/016_8.wav', 'space/017_9.wav',
            ],
            numbers: [
              'numbers/0.wav', 'numbers/1.wav', 'numbers/2.wav', 'numbers/3.wav',
              'numbers/4.wav', 'numbers/5.wav', 'numbers/6.wav', 'numbers/7.wav', 'numbers/8.wav',
            ],
            num: [
              'num/00.wav', 'num/01.wav', 'num/02.wav', 'num/03.wav', 'num/04.wav', 'num/05.wav',
              'num/06.wav', 'num/07.wav', 'num/08.wav', 'num/09.wav', 'num/10.wav', 'num/11.wav',
              'num/12.wav', 'num/13.wav', 'num/14.wav', 'num/15.wav', 'num/16.wav', 'num/17.wav',
              'num/18.wav', 'num/19.wav', 'num/20.wav',
            ],
          },
          `${base}/Dirt-Samples/`,
          { prebake: true },
        ),
      ]);
      if (globalThis.console && console.log) {
        console.log('[runner] default sample packs loaded');
      }
    } catch (e) {
      console.warn('[runner] failed to load default sample packs:', e.message || e);
    }
  };

  const getTime = () => Date.now() / 1000;
  const defaultOutput = async () => {};
  const r = repl({
    defaultOutput,
    getTime,
    transpiler,
    setInterval: globalThis.setInterval,
    clearInterval: globalThis.clearInterval,
  });
  return r;
}

function hapToEvent(hap, effectiveNow) {
  const value = hap.value || {};
  const begin = Number(hap.whole?.begin ?? hap.part?.begin ?? 0);
  const end = Number(hap.whole?.end ?? hap.part?.end ?? begin);
  let label = '';
  if (value.s) {
    label = value.s;
    if (value.n !== undefined) label += `:${value.n}`;
    if (value.note !== undefined) label += ` (${value.note})`;
  } else if (value.note !== undefined) {
    label = String(value.note);
  } else if (value.chord) {
    label = Array.isArray(value.chord) ? value.chord.join(' ') : String(value.chord);
  } else if (value.id) label = value.id;
  else if (value.label) label = value.label;
  else label = 'event';
  const color = value.color || '#7491D2';
  // Пробрасываем технические поля для таймлайна ide-native:
  // - s / bank / n нужны для поиска waveforms по семплам,
  // - source (id блока кода) — для группировки событий по месту в коде.
  const s = value.s;
  const bank = value.bank;
  const n = value.n;
  const source = value.id || value.source;
  return { label, begin, end, color, s, bank, n, source };
}

function send(obj) {
  process.stdout.write(JSON.stringify(obj) + '\n');
}

function extractErrorLocation(err) {
  const lineNumber = Number(err?.lineNumber);
  const columnNumber = Number(err?.columnNumber);
  if (Number.isFinite(lineNumber) && lineNumber > 0) {
    return {
      line: Math.trunc(lineNumber),
      column: Number.isFinite(columnNumber) && columnNumber > 0 ? Math.trunc(columnNumber) : null,
    };
  }
  const stack = String(err?.stack || '');
  const match = stack.match(/:(\d+):(\d+)\)?(?:\n|$)/);
  if (!match) return { line: null, column: null };
  return {
    line: Number(match[1]) || null,
    column: Number(match[2]) || null,
  };
}

function buildErrorPayload(err) {
  const message = err?.message || String(err);
  const { line, column } = extractErrorLocation(err);
  return {
    message,
    line,
    column,
  };
}

function schedulerPosition(scheduler) {
  return scheduler.started
    ? scheduler.now()
    : (scheduler._pausedTime ?? 0);
}

function setSchedulerPosition(scheduler, position) {
  const pos = Math.max(0, Number.isFinite(position) ? position : 0);
  const now = typeof scheduler.getTime === 'function' ? scheduler.getTime() : Date.now() / 1000;
  scheduler.clock?.stop?.();
  scheduler.lastBegin = pos;
  scheduler.lastEnd = pos;
  scheduler.lastTick = now;
  scheduler.num_ticks_since_cps_change = 0;
  scheduler.num_cycles_at_cps_change = pos;
  scheduler.seconds_at_cps_change = now;
  scheduler._pausedTime = pos;
}

async function main() {
  let replInstance;
  try {
    replInstance = await loadRepl();
  } catch (err) {
    const details = buildErrorPayload(err);
    send({
      ok: false,
      error: 'Failed to load Strudel: ' + details.message,
      errorLine: details.line,
      errorColumn: details.column,
    });
    process.exit(1);
  }

  const debug = process.env.STRUDEL_RUNNER_DEBUG === '1' || process.env.STRUDEL_RUNNER_DEBUG === 'true';
  const rl = createInterface({ input: process.stdin, output: process.stdout });
  // Quitte quand le parent ferme stdin (fin/crash de l'IDE) : sinon le
  // scheduler Strudel garde l'event loop vivant et le processus devient orphelin.
  rl.on('close', () => process.exit(0));
  process.stdin.on('end', () => process.exit(0));
  rl.on('line', async (line) => {
    if (debug) stderrWrite(`[runner] received: ${line}\n`);
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      if (debug) stderrWrite(`[runner] invalid JSON\n`);
      send({ ok: false, error: 'Invalid JSON' });
      return;
    }
    const cmd = typeof msg.cmd === 'string' ? msg.cmd.toLowerCase() : '';
    if (debug) stderrWrite(`[runner] cmd=${cmd}\n`);
    try {
      if (cmd === 'eval') {
        await replInstance.evaluate(msg.code || '', msg.autostart !== false);
        send({ ok: true });
      } else if (cmd === 'start') {
        if (!replInstance.scheduler.started && Number.isFinite(replInstance.scheduler._pausedTime)) {
          setSchedulerPosition(replInstance.scheduler, replInstance.scheduler._pausedTime);
        }
        await replInstance.start();
        send({ ok: true, value: schedulerPosition(replInstance.scheduler) });
      } else if (cmd === 'pause') {
        const pausedAt = typeof msg.position === 'number'
          ? msg.position
          : schedulerPosition(replInstance.scheduler);
        replInstance.pause();
        setSchedulerPosition(replInstance.scheduler, pausedAt);
        send({ ok: true, value: pausedAt });
      } else if (cmd === 'stop') {
        replInstance.stop();
        setSchedulerPosition(replInstance.scheduler, 0);
        send({ ok: true, value: 0 });
      } else if (cmd === 'now') {
        const n = schedulerPosition(replInstance.scheduler);
        send({ ok: true, value: n });
      } else if (cmd === 'queryarc') {
        const from = msg.from ?? 0;
        const to = msg.to ?? 4;
        const pattern = replInstance.scheduler.pattern;
        if (!pattern) {
          send({ ok: true, events: [] });
          return;
        }
        const cps = replInstance.scheduler.cps ?? 1;
        const haps = pattern.queryArc(from, to, { _cps: cps }) || [];
        const effectiveNow = schedulerPosition(replInstance.scheduler);
        const events = haps.map((h) => hapToEvent(h, effectiveNow));
        send({ ok: true, events, value: effectiveNow });
      } else if (cmd === 'audiowindow') {
        const from = msg.from ?? 0;
        const to = msg.to ?? 4;
        const pattern = replInstance.scheduler.pattern;
        if (!pattern) {
          send({ ok: true, audioEvents: [], cps: replInstance.scheduler.cps ?? 1 });
          return;
        }
        const cps = replInstance.scheduler.cps ?? 1;
        const haps = pattern.queryArc(from, to, { _cps: cps }) || [];
        const audioEvents = haps
          .filter((h) => (typeof h.hasOnset === 'function' ? h.hasOnset() : true))
          .map((h) => {
            const value = h.value || {};
            const begin = Number(h.whole?.begin ?? h.part?.begin ?? 0);
            const end = Number(h.whole?.end ?? h.part?.end ?? begin);
            // Попробуем получить MIDI-ноту: либо из value.note (строка или число),
            // либо из value.n (если это число). Для строковых нот используем noteToMidi из core.
            let midi = undefined;
            const rawNote = value.note;
            const rawN = value.n;
            if (typeof rawNote === 'number') {
              midi = rawNote;
            } else if (typeof rawNote === 'string') {
              const ntm = globalThis.noteToMidi || globalThis.strudelScope?.noteToMidi;
              if (typeof ntm === 'function') {
                try {
                  const m = ntm(rawNote);
                  if (typeof m === 'number' && !Number.isNaN(m)) {
                    midi = m;
                  }
                } catch {
                  // ignore noteToMidi errors, fall back to undefined midi
                }
              }
            }
            if (typeof rawN === 'number' && midi === undefined) {
              midi = rawN;
            }
            return {
              begin,
              end,
              value: {
                s: value.s,
                bank: value.bank,
                n: value.n,
                midi,
                note: value.note,
                gain: value.gain,
                pan: value.pan,
                cps: value.cps,
                cut: value.cut,
                room: value.room,
                shape: value.shape,
                attack: value.attack,
                decay: value.decay,
                sustain: value.sustain,
                release: value.release,
                speed: value.speed,
                cutoff: value.cutoff,
              },
            };
          });
        send({ ok: true, audioEvents, cps });
      } else if (cmd === 'loadsamples') {
        cancelDownloads = false;
        if (typeof globalThis.__loadDefaultSamples === 'function') {
          await globalThis.__loadDefaultSamples();
        }
        send({ ok: true });
      } else if (cmd === 'canceldownloads') {
        cancelDownloads = true;
        send({ ok: true });
      } else if (cmd === 'seek') {
        const pos = typeof msg.position === 'number' ? msg.position : 0;
        const wasStarted = replInstance.scheduler.started;
        replInstance.pause();
        setSchedulerPosition(replInstance.scheduler, pos);
        if (wasStarted) await replInstance.start();
        send({ ok: true, value: schedulerPosition(replInstance.scheduler) });
      } else if (cmd === 'setbpm') {
        const bpm = Math.max(1, typeof msg.bpm === 'number' ? msg.bpm : 120);
        const cps = bpm / 60;
        if (typeof replInstance.scheduler.setCps === 'function') {
          replInstance.scheduler.setCps(cps);
        } else {
          replInstance.scheduler.cps = cps;
        }
        send({ ok: true });
      } else if (cmd === 'getbpm') {
        const cps = replInstance.scheduler.cps ?? 1;
        send({ ok: true, value: cps * 60 });
      } else {
        send({ ok: false, error: 'Unknown command: ' + cmd });
      }
    } catch (err) {
      const details = buildErrorPayload(err);
      send({
        ok: false,
        error: details.message,
        errorLine: details.line,
        errorColumn: details.column,
      });
    }
  });
}

main();
