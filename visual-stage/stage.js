/* Visual Stage — Strudel INTERVENES in the Hydra signal path.
 * Not pulse cosmetics: hits latch buses that rewrite osc/mod/feedback/geometry/thresh.
 * Inspired by hydra.cc sketches (CC BY-NC-SA); music replaces mouse/a.fft.
 */

(() => {
  const params = new URLSearchParams(location.search);
  const host = params.get("host") || location.hostname || "127.0.0.1";
  const wsPort = params.get("ws") || "9877";
  const wsUrl = `ws://${host}:${wsPort}`;
  const startPatch = params.get("patch");

  // ─── Signal buses (music writes, Hydra reads) ─────────────────────────────
  // These are the "wires" Strudel plugs into. Hits rewire them, they decay slowly.
  const bus = {
    // Carrier
    freq: 12, // osc frequency register (jumps hard on pitch/kick)
    sync: 0.08, // sync/offset
    // Modulator path
    modDepth: 0.25, // how hard noise/osc modulates the carrier (0..2+)
    modRate: 1.2, // modulator speed
    // Feedback loop (this is what makes "signal rewrite" feel global)
    feedback: 0.35, // blend of previous frame into current
    fbDistort: 0.03, // modulate(o0) amount
    // Geometry / structure
    sides: 4, // shape sides / kaleid
    repeatX: 2,
    repeatY: 2,
    scale: 1.0,
    rotate: 0,
    scrollX: 0,
    scrollY: 0,
    // Nonlinear ops
    thresh: 0.55,
    poster: 80,
    invert: 0, // latched 0|1
    contrast: 1.2,
    brightness: 0,
    colorama: 0.3,
    // Which source dominates: 0 osc, 1 noise, 2 voronoi, 3 shape-grid
    branch: 0,
    // Smooth blend when branch / look changes
    xfade: 1,
    xfadeDur: 3.5,
  };

  const state = {
    dens: 0.1,
    kickEnergy: 0,
    lastHit: "—",
    hitTimes: [],
    hitCount: 0,
    patchIndex: 0,
    autoRotate: true,
    lastSwitch: 0,
    lastKickAt: 0,
    barHits: 0,
    // IDE-eval'd sketch (overrides presets until look 1–N / N)
    userSketch: null,
    followMusic: true,
    evalError: null,
  };

  const el = (id) => document.getElementById(id);
  const b = bus; // short

  // Accessors for Hydra lambdas (must be functions)
  const F = () => b.freq;
  const SYNC = () => b.sync;
  const MD = () => b.modDepth;
  const MR = () => b.modRate;
  const FB = () => b.feedback;
  const FD = () => b.fbDistort;
  const SIDES = () => Math.max(3, Math.round(b.sides));
  const RX = () => Math.max(1, Math.round(b.repeatX));
  const RY = () => Math.max(1, Math.round(b.repeatY));
  const SC = () => b.scale;
  const ROT = () => b.rotate;
  const SX = () => b.scrollX;
  const SY = () => b.scrollY;
  const TH = () => b.thresh;
  const PO = () => b.poster;
  const INV = () => b.invert;
  const CT = () => b.contrast;
  const BR = () => b.brightness;
  const CA = () => b.colorama;
  const XF = () => b.xfade;
  const BRN = () => b.branch;

  function withFeedback(chain) {
    return chain
      .blend(src(o0), () => Math.min(0.95, FB()))
      .modulate(src(o0), () => FD())
      .blend(src(o0), () => Math.pow(1 - XF(), 1.6));
  }

  /** Source chosen by branch bus — music can switch the whole signal family. */
  function carrier() {
    const br = Math.round(BRN()) % 4;
    if (br === 1) {
      return noise(() => 0.5 + F() * 0.15, () => SYNC()).colorama(CA);
    }
    if (br === 2) {
      return voronoi(() => 40 + F() * 8, () => 0.05 + MD() * 0.1).colorama(CA);
    }
    if (br === 3) {
      return shape(SIDES, 0.2, 0.01)
        .repeat(RX, RY)
        .scale(SC)
        .scrollX(SX, () => 0.01 + Math.abs(SX()) * 0.2)
        .scrollY(SY, () => 0.01 + Math.abs(SY()) * 0.2)
        .colorama(CA);
    }
    return osc(F, () => -SYNC(), () => 0.15 + MD() * 0.4).colorama(CA);
  }

  function modulator() {
    return noise(MR, () => 0.1 + MD() * 0.2).modulate(
      osc(() => F() * 0.35).rotate(ROT),
      () => 0.2 + MD()
    );
  }

  // ─── Looks = different wiring of the SAME buses ───────────────────────────
  // `code` is the sketch currently playing (music writes buses F/MD/FB… Hydra reads them).
  const BRANCH_LABELS = ["osc", "noise", "voronoi", "shape-grid"];

  const PATCHES = [
    {
      id: "wire_diff",
      title: "wire_diff",
      credit: "signal bus",
      code: `// carrier() picks branch: osc | noise | voronoi | shape-grid
withFeedback(
  carrier()
    .diff(osc(F*1.7, SYNC*2).rotate(π/2 + ROT))
    .modulateScale(modulator(), MD)
    .contrast(CT).invert(INV).brightness(BR)
    .thresh(TH, 0.1 + MD*0.2)
).out()`,
      run: () => {
        withFeedback(
          carrier()
            .diff(osc(() => F() * 1.7, () => SYNC() * 2).rotate(() => Math.PI / 2 + ROT()))
            .modulateScale(modulator(), MD)
            .contrast(CT)
            .invert(INV)
            .brightness(BR)
            .thresh(TH, () => 0.1 + MD() * 0.2)
        ).out();
      },
    },
    {
      id: "wire_feedback",
      title: "wire_feedback",
      credit: "after Vera/Ritchse",
      code: `withFeedback(
  carrier()
    .color(-1.5,-1.5,-1.5)
    .rotate(-0.2+ROT, -0.2-MD*0.3)
    .modulate(
      shape(SIDES).scale(SC).repeat(RX,RY)
        .modulate(o0, FD).rotate(0.5,0.5),
      MD)
    .contrast(CT).invert(INV)
).out()`,
      run: () => {
        withFeedback(
          carrier()
            .color(-1.5, -1.5, -1.5)
            .rotate(() => -0.2 + ROT(), () => -0.2 - MD() * 0.3)
            .modulate(
              shape(SIDES)
                .scale(SC)
                .repeat(RX, RY)
                .modulate(o0, FD)
                .rotate(0.5, 0.5),
              MD
            )
            .contrast(CT)
            .invert(INV)
        ).out();
      },
    },
    {
      id: "wire_scale",
      title: "wire_scale",
      credit: "after closer",
      code: `withFeedback(
  carrier()
    .diff(osc(F*2, MR).rotate(ROT))
    .modulateScale(
      noise(1+MD*4, 0.1+SYNC)
        .modulateScale(osc(8+F*0.3).rotate(time*0.1+ROT), MD),
      0.3+MD)
    .color(0.5+CA, 0.3+INV*0.4, 0.3+MD*0.3)
    .contrast(CT).invert(INV).brightness(BR)
    .modulateScale(osc(2), -0.1-MD*0.4)
).out()`,
      run: () => {
        withFeedback(
          carrier()
            .diff(osc(() => F() * 2, MR).rotate(ROT))
            .modulateScale(
              noise(() => 1 + MD() * 4, () => 0.1 + SYNC()).modulateScale(
                osc(() => 8 + F() * 0.3).rotate(() => time * 0.1 + ROT()),
                MD
              ),
              () => 0.3 + MD()
            )
            .color(() => 0.5 + CA(), () => 0.3 + INV() * 0.4, () => 0.3 + MD() * 0.3)
            .contrast(CT)
            .invert(INV)
            .brightness(BR)
            .modulateScale(osc(2), () => -0.1 - MD() * 0.4)
        ).out();
      },
    },
    {
      id: "wire_poster",
      title: "wire_poster",
      credit: "after AFALFL",
      code: `withFeedback(
  carrier()
    .diff(osc(F*0.4, 0.00008).rotate(π/(0.00004+FD)))
    .modulateScale(
      noise(1.5, MR).modulateScale(osc(13).rotate(ROT), 1+MD*3),
      1+MD*2)
    .posterize(PO).contrast(CT).invert(INV).colorama(CA)
    .modulateScale(osc(2), -0.1-MD*0.3)
    .rotate(1, 0.05+|ROT|*0.5)
).out()`,
      run: () => {
        withFeedback(
          carrier()
            .diff(osc(() => F() * 0.4, 0.00008).rotate(() => Math.PI / (0.00004 + FD())))
            .modulateScale(noise(1.5, MR).modulateScale(osc(13).rotate(ROT), () => 1 + MD() * 3), () => 1 + MD() * 2)
            .posterize(PO)
            .contrast(CT)
            .invert(INV)
            .colorama(CA)
            .modulateScale(osc(2), () => -0.1 - MD() * 0.3, 2, 1, 0.3)
            .rotate(1, () => 0.05 + Math.abs(ROT()) * 0.5, 0.01, 0.001)
        ).out();
      },
    },
    {
      id: "wire_kaleid",
      title: "wire_kaleid",
      credit: "after Mahalia",
      code: `shape(SIDES,0.1).scale(SC*(0.8+MD))
  .repeat(F*0.4+RX, RY)
  .modulateRotate(o0, MD)
  .modulate(noise(MR), 0.1+MD*0.5)
  .rotate(ROT, 0.4+CA)
  .blend(o0, xfade).out(o0)
src(o0).modulate(osc(80+F*20)).out(o1)
src(o1)
  .modulateKaleid(voronoi(2+MD*4+F*0.1), 2+MD*3+SIDES*0.2)
  .scale(0.7+SC*0.6).contrast(CT).invert(INV)
  .out(o2)
render(o2)`,
      run: () => {
        shape(SIDES, 0.1, 0.01)
          .scale(() => SC() * (0.8 + MD()))
          .repeat(() => Math.max(1, Math.round(F() * 0.4 + RX())), RY)
          .modulateRotate(o0, MD)
          .modulate(noise(MR, 0), () => 0.1 + MD() * 0.5)
          .rotate(ROT, () => 0.4 + CA())
          .blend(src(o0), () => Math.pow(1 - XF(), 1.6))
          .out(o0);

        src(o0)
          .modulate(osc(() => 80 + F() * 20, 0, 0))
          .out(o1);

        src(o1)
          .modulateKaleid(
            voronoi(() => 2 + MD() * 4 + F() * 0.1, 0.1, 0.01),
            () => 2 + MD() * 3 + SIDES() * 0.2
          )
          .scale(() => 0.7 + SC() * 0.6)
          .contrast(CT)
          .invert(INV)
          .out(o2);

        render(o2);
      },
    },
    {
      id: "wire_voronoi",
      title: "wire_voronoi",
      credit: "after screensaver",
      code: `withFeedback(
  voronoi(80+F*12, 0.06+MD*0.12)
    .modulateScale(osc(MR).rotate(sin(t)+ROT), MD)
    .thresh(TH)
    .modulateRotate(osc(3+MD*5), 0.2+MD*0.4)
    .thresh(TH*0.9)
    .diff(src(o0).scale(1.2+SC*0.5))
    .modulateScale(osc(2).modulateRotate(o0, 0.4+FD*8))
    .colorama(CA).contrast(CT).invert(INV).brightness(BR)
).out()`,
      run: () => {
        withFeedback(
          voronoi(() => 80 + F() * 12, () => 0.06 + MD() * 0.12)
            .modulateScale(osc(MR).rotate(() => Math.sin(time) + ROT()), MD)
            .thresh(TH)
            .modulateRotate(osc(() => 3 + MD() * 5), () => 0.2 + MD() * 0.4)
            .thresh(() => TH() * 0.9)
            .diff(src(o0).scale(() => 1.2 + SC() * 0.5))
            .modulateScale(osc(2).modulateRotate(o0, () => 0.4 + FD() * 8))
            .colorama(CA)
            .contrast(CT)
            .invert(INV)
            .brightness(BR)
        ).out();
      },
    },
    {
      id: "wire_rift",
      title: "wire_rift",
      credit: "hard intervene",
      code: `withFeedback(
  carrier()
    .modulate(osc(4+F*0.5), MD)
    .modulateRotate(src(o0), 0.05+MD*0.7)
    .scale(SC)
    .diff(shape(SIDES).repeat(RX,RY).scrollX(SX).scrollY(SY))
    .invert(INV).contrast(CT).thresh(TH,0.15).colorama(CA)
).out()`,
      run: () => {
        withFeedback(
          carrier()
            .modulate(osc(() => 4 + F() * 0.5), MD)
            .modulateRotate(src(o0), () => 0.05 + MD() * 0.7)
            .scale(SC)
            .diff(
              shape(SIDES, 0.2, 0.01)
                .repeat(RX, RY)
                .scrollX(SX)
                .scrollY(SY)
            )
            .invert(INV)
            .contrast(CT)
            .thresh(TH, 0.15)
            .colorama(CA)
        ).out();
      },
    },
  ];

  function liveHydraSketch(p) {
    if (state.userSketch) {
      return [
        `// PLAYING · editor sketch (IDE Eval)`,
        state.evalError ? `// ERROR: ${state.evalError}` : `// ok`,
        `// followMusic=${state.followMusic}`,
        ``,
        state.userSketch,
      ].join("\n");
    }
    const br = Math.round(b.branch) % 4;
    const carrierLine =
      br === 1
        ? `noise(${(0.5 + b.freq * 0.15).toFixed(2)}, ${b.sync.toFixed(3)}).colorama(${b.colorama.toFixed(2)})`
        : br === 2
          ? `voronoi(${(40 + b.freq * 8).toFixed(1)}, ${(0.05 + b.modDepth * 0.1).toFixed(3)}).colorama(${b.colorama.toFixed(2)})`
          : br === 3
            ? `shape(${Math.max(3, Math.round(b.sides))}).repeat(${Math.round(b.repeatX)},${Math.round(b.repeatY)}).scale(${b.scale.toFixed(2)}).scroll…`
            : `osc(${b.freq.toFixed(1)}, ${(-b.sync).toFixed(3)}, ${(0.15 + b.modDepth * 0.4).toFixed(2)}).colorama(${b.colorama.toFixed(2)})`;

    return [
      `// PLAYING NOW · look ${p.title} · carrier = ${BRANCH_LABELS[br]}`,
      `// Strudel hits latch these buses (decay slowly — not one-frame pulses)`,
      `carrier = ${carrierLine}`,
      ``,
      p.code,
      ``,
      `// feedback blend: ${b.feedback.toFixed(2)} · distort: ${b.fbDistort.toFixed(3)} · xfade: ${b.xfade.toFixed(2)}`,
    ].join("\n");
  }

  function refreshHydraCode(reason) {
    const p = PATCHES[state.patchIndex];
    if (!p && !state.userSketch) return;
    const titleEl = el("code-title");
    const codeEl = el("hydra-code");
    const busEl = el("bus-live");
    if (titleEl) {
      titleEl.textContent = state.userSketch
        ? `editor${reason ? " · " + reason : ""}`
        : `${p.title}${reason ? " · " + reason : ""}`;
    }
    if (codeEl) codeEl.textContent = liveHydraSketch(p || { title: "editor", code: "" });
    if (busEl) {
      busEl.textContent = [
        `freq ${b.freq.toFixed(1)}  sync ${b.sync.toFixed(3)}  modDepth ${b.modDepth.toFixed(2)}  modRate ${b.modRate.toFixed(2)}`,
        `feedback ${b.feedback.toFixed(2)}  distort ${b.fbDistort.toFixed(3)}  thresh ${b.thresh.toFixed(2)}  invert ${Math.round(b.invert)}`,
        `sides ${Math.round(b.sides)}  repeat ${Math.round(b.repeatX)}×${Math.round(b.repeatY)}  scale ${b.scale.toFixed(2)}  rotate ${b.rotate.toFixed(2)}`,
        `scroll ${b.scrollX.toFixed(2)},${b.scrollY.toFixed(2)}  colorama ${b.colorama.toFixed(2)}  branch ${BRANCH_LABELS[Math.round(b.branch) % 4]}`,
        state.followMusic
          ? `map: kick→freq/fb · snare→sides/repeat · hat→scroll · note/midi→freq`
          : `music buses paused (followMusic=false)`,
      ].join("\n");
    }
  }

  /** Run Hydra source from the IDE editor. */
  function evalHydraCode(code, followMusic) {
    const src = String(code || "").trim();
    if (!src) return;
    if (typeof followMusic === "boolean") state.followMusic = followMusic;
    // Expose bus helpers for sketches from the IDE
    Object.assign(window, {
      F,
      SYNC,
      MD,
      MR,
      FB,
      FD,
      SIDES,
      RX,
      RY,
      SC,
      ROT,
      SX,
      SY,
      TH,
      PO,
      INV,
      CT,
      BR,
      CA,
      XF,
      BRN,
      bus: b,
      withFeedback,
      carrier,
      modulator,
    });
    try {
      // Hydra globals (osc, noise, …) already on window via makeGlobal
      // eslint-disable-next-line no-eval
      eval(src);
      state.userSketch = src;
      state.evalError = null;
      state.autoRotate = false;
      el("patch").textContent = "editor";
      el("credit").textContent = "IDE Eval";
      refreshHydraCode("eval");
      el("ws-state").textContent = "live · eval ok";
    } catch (e) {
      state.evalError = String(e && e.message ? e.message : e);
      console.error("Hydra eval", e);
      refreshHydraCode("error");
      el("ws-state").textContent = "eval error";
    }
  }

  function setPatch(i, reason) {
    state.userSketch = null;
    state.evalError = null;
    const idx = ((i % PATCHES.length) + PATCHES.length) % PATCHES.length;
    state.patchIndex = idx;
    state.lastSwitch = performance.now();
    b.xfade = 0;
    const p = PATCHES[idx];
    try {
      p.run();
    } catch (e) {
      console.error(p.id, e);
    }
    el("patch").textContent = p.title;
    el("credit").textContent = `${p.credit} · ${reason || ""} · branch ${Math.round(b.branch)}`;
    refreshHydraCode(reason || "look");
  }

  function nextPatch(reason) {
    setPatch(
      (state.patchIndex + 1 + Math.floor(Math.random() * (PATCHES.length - 1))) %
        PATCHES.length,
      reason
    );
  }

  // ─── Music → hard signal intervention ─────────────────────────────────────
  function classify(s, bank) {
    const x = String(s || "").toLowerCase();
    const blob = `${x} ${String(bank || "").toLowerCase()}`;
    if (
      x === "bd" ||
      blob.includes("bd") ||
      blob.includes("kick") ||
      blob.includes("bassdrum")
    )
      return "kick";
    if (
      x === "cp" ||
      blob.includes("clap") ||
      x === "sd" ||
      blob.includes("snare") ||
      blob.includes("rim")
    )
      return "snare";
    if (blob.includes("hh") || blob.includes("hat") || x === "oh" || blob.includes("open"))
      return "hat";
    if (blob.includes("crash") || x === "cr" || blob.includes("cym")) return "cymbal";
    return "other";
  }

  /** Structural rewrite — latches / jumps, not a flash envelope. */
  function intervene(kind, g, n, midi) {
    const force = Math.max(0.35, Math.min(1.4, g));
    state.hitCount += 1;
    state.barHits += 1;

    if (kind === "kick") {
      // Kick rewires the carrier + feedback loop
      b.freq = Math.max(3, Math.min(120, b.freq * (0.5 + Math.random())));
      if (Math.random() < 0.55) b.freq = 6 + Math.random() * 40;
      b.modDepth = Math.min(2.2, 0.4 + force * 1.2 + Math.random() * 0.5);
      b.feedback = Math.min(0.92, 0.45 + force * 0.4);
      b.fbDistort = Math.min(0.2, 0.04 + force * 0.1);
      b.scale = 0.6 + force * 0.9;
      b.contrast = 1.1 + force * 0.8;
      b.thresh = 0.35 + Math.random() * 0.45;
      // Flip invert every few kicks — binary structural change
      if (state.hitCount % 4 === 0) b.invert = b.invert > 0.5 ? 0 : 1;
      // Occasional branch switch = whole source family changes
      if (Math.random() < 0.22 * force) {
        b.branch = (Math.round(b.branch) + 1 + Math.floor(Math.random() * 2)) % 4;
        b.xfade = 0;
        if (state.userSketch) evalHydraCode(state.userSketch);
        else {
          PATCHES[state.patchIndex].run();
          refreshHydraCode("branch");
        }
      }
      state.kickEnergy = Math.min(2, state.kickEnergy + force);
      state.lastKickAt = performance.now();
    } else if (kind === "snare") {
      // Snare attacks nonlinear / geometric wiring
      b.sides = 3 + Math.floor(Math.random() * 7);
      b.repeatX = 1 + Math.floor(Math.random() * 6);
      b.repeatY = 1 + Math.floor(Math.random() * 6);
      b.poster = 20 + Math.random() * 200;
      b.thresh = Math.max(0.15, Math.min(0.95, b.thresh + (Math.random() - 0.5) * 0.5));
      b.modRate = 0.5 + Math.random() * 6;
      b.rotate += (Math.random() - 0.5) * 1.2 * force;
      if (Math.random() < 0.35) b.invert = b.invert > 0.5 ? 0 : 1;
    } else if (kind === "hat") {
      // Hats scrub spatial signal path
      b.scrollX += (Math.random() - 0.5) * 0.25 * force;
      b.scrollY += (Math.random() - 0.5) * 0.2 * force;
      b.sync = Math.max(0.01, Math.min(0.5, b.sync + (Math.random() - 0.5) * 0.08));
      b.modRate = Math.max(0.3, Math.min(10, b.modRate * (0.85 + Math.random() * 0.4)));
      b.fbDistort = Math.min(0.18, b.fbDistort + 0.01 * force);
    } else if (kind === "cymbal") {
      b.colorama = (b.colorama + 0.15 + force * 0.2) % 1;
      b.modDepth = Math.min(2.4, b.modDepth + 0.35 * force);
      b.scale = Math.min(2.8, b.scale + 0.25 * force);
      b.feedback = Math.min(0.95, b.feedback + 0.08);
    } else {
      // Unlabelled / tonal hits still rewire buses — ANY strudel event counts
      b.modDepth = Math.min(2.2, b.modDepth + 0.35 * force);
      b.feedback = Math.min(0.9, b.feedback + 0.12 * force);
      b.scale = Math.min(2.5, b.scale + 0.2 * force);
      b.rotate += (Math.random() - 0.5) * 0.5 * force;
      b.thresh = Math.max(0.2, Math.min(0.9, b.thresh + (Math.random() - 0.5) * 0.25));
      if (Math.random() < 0.15 * force) {
        b.branch = (Math.round(b.branch) + 1) % 4;
        b.xfade = 0;
        if (state.userSketch) evalHydraCode(state.userSketch);
        else {
          PATCHES[state.patchIndex].run();
          refreshHydraCode("branch");
        }
      }
    }

    // Pitch / note → oscillator register + color (tonal control of the signal)
    if (midi != null && Number.isFinite(midi)) {
      b.freq = 4 + ((midi - 24) % 48) * 1.8;
      b.colorama = ((midi % 12) / 12 + b.colorama * 0.3) % 1;
      b.sync = 0.02 + ((midi % 7) / 7) * 0.25;
      b.modDepth = Math.min(2.2, b.modDepth + 0.25);
    } else if (n != null && Number.isFinite(n)) {
      b.freq = 5 + (((n % 12) + 12) % 12) * 4;
      b.colorama = (((n % 12) + 12) % 12) / 12;
      b.modDepth = Math.min(2.2, b.modDepth + 0.2);
    }

    // Density of recent hits pushes feedback/branch — arrangement energy as wiring
    const dens = Math.min(1.4, state.hitTimes.length / 7);
    state.dens = dens;
    if (dens > 0.85) {
      b.feedback = Math.min(0.95, Math.max(b.feedback, 0.55 + dens * 0.3));
      b.modDepth = Math.min(2.3, Math.max(b.modDepth, dens * 1.1));
    }
  }

  function onMusicEvent(ev) {
    if (ev.type === "scene" || ev.type === "hydra_shift") {
      // Force a structural reset when IDE asks
      b.branch = Math.floor(Math.random() * 4);
      b.freq = 5 + Math.random() * 50;
      b.modDepth = 0.5 + Math.random();
      b.sides = 3 + Math.floor(Math.random() * 6);
      if (!state.userSketch) nextPatch(ev.reason || "ide");
      if (typeof ev.xfade === "number") {
        b.xfadeDur = Math.max(1.2, Math.min(10, ev.xfade));
      }
      return;
    }

    if (!state.followMusic) {
      el("hit").textContent = "buses paused";
      return;
    }

    const s = ev.s || "";
    const bank = ev.bank || "";
    const g = typeof ev.g === "number" ? ev.g : 0.7;
    const n = typeof ev.n === "number" ? ev.n : null;
    const midi = typeof ev.midi === "number" ? ev.midi : null;

    state.lastHit = s || bank || (midi != null ? `midi:${midi}` : "hit");
    const kind = classify(s, bank);
    el("hit").textContent = `${state.lastHit} → ${kind}`;
    state.hitTimes.push(performance.now());

    intervene(kind, g, n, midi);
    refreshHydraCode(kind);
  }

  // ─── Tick: slow decay of buses (latches settle, don't vanish as pulses) ────
  let lastTs = performance.now();
  function tick(now) {
    const dt = Math.min(0.05, (now - lastTs) / 1000);
    lastTs = now;

    if (b.xfade < 1) {
      b.xfade = Math.min(1, b.xfade + dt / b.xfadeDur);
    }

    // Slow settle — keep interventions present for beats, not frames
    const settle = Math.pow(0.997, dt * 60);
    const settleFast = Math.pow(0.99, dt * 60);
    b.modDepth = 0.2 + (b.modDepth - 0.2) * settle;
    b.feedback = 0.25 + (b.feedback - 0.25) * settle;
    b.fbDistort = 0.02 + (b.fbDistort - 0.02) * settle;
    b.scale = 1 + (b.scale - 1) * settleFast;
    b.contrast = 1.15 + (b.contrast - 1.15) * settle;
    b.brightness *= settleFast;
    b.scrollX *= settleFast;
    b.scrollY *= settleFast;
    b.rotate *= settle;
    state.kickEnergy *= settleFast;

    // Drift carrier slowly unless music is hitting hard
    if (state.kickEnergy < 0.15) {
      b.freq += Math.sin(now * 0.0003) * dt * 2;
    }

    state.hitTimes = state.hitTimes.filter((t) => now - t < 1800);
    state.dens = Math.min(1.4, state.hitTimes.length / 7);

    // Section change: after a run of structure, swap patch wiring (smooth)
    if (state.autoRotate && !state.userSketch && b.xfade >= 1) {
      const elapsed = now - state.lastSwitch;
      if (elapsed > 18000 && state.barHits > 24) {
        state.barHits = 0;
        b.branch = (Math.round(b.branch) + 1) % 4;
        nextPatch("section");
      } else if (elapsed > 32000) {
        state.barHits = 0;
        nextPatch("drift");
      }
    }

    el("dens").textContent =
      state.dens.toFixed(2) +
      " · f" +
      b.freq.toFixed(1) +
      " · md" +
      b.modDepth.toFixed(2) +
      " · fb" +
      b.feedback.toFixed(2);
    el("hue").textContent =
      "br" + Math.round(b.branch) + " · inv" + Math.round(b.invert) + " · th" + b.thresh.toFixed(2);

    // Refresh code panel ~8 fps (buses change continuously)
    if (!state._codeAt || now - state._codeAt > 120) {
      state._codeAt = now;
      refreshHydraCode();
    }

    requestAnimationFrame(tick);
  }

  function initHydra() {
    const canvas = document.createElement("canvas");
    canvas.id = "hydra-canvas";
    document.body.prepend(canvas);
    // eslint-disable-next-line no-undef
    const hydra = new Hydra({
      canvas,
      detectAudio: false,
      makeGlobal: true,
      width: window.innerWidth,
      height: window.innerHeight,
    });
    window.addEventListener("resize", () => {
      try {
        hydra.setResolution(window.innerWidth, window.innerHeight);
      } catch (_) {}
    });

    let start = 0;
    if (startPatch) {
      const byId = PATCHES.findIndex((p) => p.id === startPatch);
      if (byId >= 0) start = byId;
    }
    b.xfade = 1;
    setPatch(start, "boot");

    window.addEventListener("keydown", (e) => {
      if (e.key >= "1" && e.key <= String(Math.min(9, PATCHES.length))) {
        setPatch(Number(e.key) - 1, "manual");
      } else if (e.key === "n" || e.key === "N") nextPatch("manual");
      else if (e.key === "a" || e.key === "A") {
        state.autoRotate = !state.autoRotate;
      } else if (e.key === "b" || e.key === "B") {
        b.branch = (Math.round(b.branch) + 1) % 4;
        b.xfade = 0;
        PATCHES[state.patchIndex].run();
        el("credit").textContent = `branch ${Math.round(b.branch)}`;
        refreshHydraCode("branch");
      } else if (e.key === "c" || e.key === "C") {
        const panel = el("hydra-panel");
        if (panel) panel.classList.toggle("hidden");
      } else if (e.key === "[" || e.key === "]") {
        b.xfadeDur = Math.max(1.2, Math.min(10, b.xfadeDur + (e.key === "]" ? 0.6 : -0.6)));
      }
    });
    return hydra;
  }

  function connectWs() {
    el("ws-state").textContent = "connecting…";
    let ws;
    try {
      ws = new WebSocket(wsUrl);
    } catch (_) {
      el("ws-state").textContent = "error";
      setTimeout(connectWs, 1500);
      return;
    }
    ws.onopen = () => {
      el("ws-state").textContent = "live";
    };
    ws.onclose = () => {
      el("ws-state").textContent = "reconnecting…";
      setTimeout(connectWs, 1200);
    };
    ws.onerror = () => {
      el("ws-state").textContent = "error";
    };
    ws.onmessage = (msg) => {
      try {
        const data = JSON.parse(msg.data);
        if (data.type === "eval" && typeof data.code === "string") {
          evalHydraCode(data.code, data.followMusic);
        } else if (data.type === "seed" && data.buses && typeof data.buses === "object") {
          // Live chain/morph pulse — latch buses from seed without replacing the sketch
          const v = data.buses;
          const setNum = (k, key) => {
            if (typeof v[key] === "number" && Number.isFinite(v[key])) b[k] = v[key];
          };
          setNum("freq", "freq");
          setNum("modDepth", "modDepth");
          setNum("modRate", "modRate");
          setNum("feedback", "feedback");
          setNum("fbDistort", "fbDistort");
          setNum("thresh", "thresh");
          setNum("colorama", "colorama");
          setNum("sides", "sides");
          setNum("repeatX", "repeatX");
          setNum("repeatY", "repeatY");
          setNum("scale", "scale");
          setNum("branch", "branch");
          setNum("invert", "invert");
          setNum("poster", "poster");
          setNum("scrollX", "scrollX");
          setNum("scrollY", "scrollY");
          if (data.flash) b.xfade = Math.min(b.xfade, 0.35);
          refreshHydraCode("seed");
          el("ws-state").textContent = "live · seed pulse";
          el("hit").textContent = data.flash ? "seed flash" : "seed";
        } else if (data.type === "followMusic" && typeof data.enabled === "boolean") {
          state.followMusic = data.enabled;
          refreshHydraCode("follow");
        } else if (data.type === "hits" && Array.isArray(data.events)) {
          // Batch from IDE audio window — every Strudel event hits the same buses
          for (const ev of data.events) {
            onMusicEvent(ev);
          }
          el("ws-state").textContent = `live · ${data.n || data.events.length} hits`;
        } else if (
          data.type === "hit" ||
          data.s != null ||
          data.g != null ||
          data.type === "scene"
        ) {
          onMusicEvent(data);
        }
      } catch (_) {}
    };
  }

  try {
    initHydra();
  } catch (e) {
    el("ws-state").textContent = "hydra failed";
    console.error(e);
  }
  requestAnimationFrame(tick);
  connectWs();
})();
