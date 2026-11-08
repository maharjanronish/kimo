//! In-browser analysis: page instrumentation, timeline inventory, duration
//! detection, and the `--analyze` report.
//!
//! Nothing here trusts the SVG file's bytes alone — the page is loaded in real
//! Chromium, the DOM is measured live, CSS/WAAPI timing comes from the
//! browser's own `getComputedTiming()`, and JavaScript timing usage is
//! observed through instrumentation injected before any page script runs.

use crate::anim::{
    parse_instance_list, resolve_dims, BrowserAnim, DocPlan, ElementPlan, ResolvedDims, SmilElement,
};
use crate::error::{KimoError, Result};
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

/// Injected via `Page.addScriptToEvaluateOnNewDocument` — runs before any
/// page script. It gates requestAnimationFrame (so JS animations stay at
/// their true zero state until we start capturing), counts timing-API usage,
/// and installs the freeze/seek helpers used by the deterministic frame engine.
pub const INSTRUMENT_JS: &str = r#"
(function () {
  if (typeof window === 'undefined' || window.__kimoInstalled) return;
  var W = window;
  W.__kimoInstalled = true;
  var S = {
    rafQueued: 0, rafRun: 0, timers: 0, intervalFires: 0,
    canvas2d: 0, webgl: 0, gateAutoFired: false, gateReleased: false
  };
  W.__kimoStats = S;
  try { S.scripts = document.querySelectorAll('script').length; } catch (e) { S.scripts = 0; }

  // --- requestAnimationFrame gate ---------------------------------------
  var origRaf = W.requestAnimationFrame && W.requestAnimationFrame.bind(W);
  if (origRaf) {
    var queue = [];
    W.requestAnimationFrame = function (cb) {
      if (S.gateReleased) {
        return origRaf(function (ts) { S.rafRun++; try { cb(ts); } catch (e) {} });
      }
      queue.push(cb);
      S.rafQueued = queue.length;
      return queue.length;
    };
    W.__kimoReleaseRaf = function () {
      if (S.gateReleased) return;
      S.gateReleased = true;
      var q = queue.splice(0, queue.length);
      for (var i = 0; i < q.length; i++) (function (cb) {
        origRaf(function (ts) { S.rafRun++; try { cb(ts); } catch (e) {} });
      })(q[i]);
    };
    // Safety valve: if kimo never releases (pathological page), release after
    // 5s of real time so the page still becomes usable.
    W.setTimeout(function () {
      if (!S.gateReleased) { S.gateAutoFired = true; W.__kimoReleaseRaf(); }
    }, 5000);
  }

  // --- timer / canvas usage counters ------------------------------------
  var oST = W.setTimeout && W.setTimeout.bind(W);
  if (oST) W.setTimeout = function (cb, ms) {
    var a = Array.prototype.slice.call(arguments, 2);
    return oST(function () { S.timers++; try { cb.apply(null, a); } catch (e) {} }, ms);
  };
  var oSI = W.setInterval && W.setInterval.bind(W);
  if (oSI) W.setInterval = function (cb, ms) {
    var a = Array.prototype.slice.call(arguments, 2);
    return oSI(function () { S.intervalFires++; try { cb.apply(null, a); } catch (e) {} }, ms);
  };
  var oGC = W.HTMLCanvasElement && W.HTMLCanvasElement.prototype.getContext;
  if (oGC) W.HTMLCanvasElement.prototype.getContext = function (t) {
    if (t === '2d') S.canvas2d++;
    else if (/webgl/i.test(String(t))) S.webgl++;
    return oGC.apply(this, arguments);
  };

  // --- freeze / seek helpers for CSS & Web Animations -------------------
  W.__kimoAnims = [];
  W.__kimoSeen = [];
  W.__kimoSkipped = [];
  W.__kimoFrozen = false;
  function tlNow() {
    try { return document.timeline ? document.timeline.currentTime : 0; } catch (e) { return 0; }
  }
  W.__kimoRescan = function () {
    var list = [];
    try { list = document.getAnimations(); } catch (e) { return; }
    var i, a;
    for (i = 0; i < list.length; i++) {
      a = list[i];
      if (W.__kimoSeen.indexOf(a) !== -1) continue;
      try {
        if (a.playState === 'paused' && !a.__kimoPausedByUs) {
          // Author-paused animation: leave it exactly as the author intended.
          W.__kimoSeen.push(a);
          W.__kimoSkipped.push(a);
          continue;
        }
        var abs = (a.startTime === null || typeof a.startTime !== 'number' || isNaN(a.startTime));
        a.__kimoPausedByUs = true;
        a.pause();
        W.__kimoAnims.push({ a: a, c0: a.currentTime, abs: abs, d0: tlNow() });
        W.__kimoSeen.push(a);
      } catch (e) {}
    }
    // Adopt previously skipped animations if the author resumed them.
    for (i = W.__kimoSkipped.length - 1; i >= 0; i--) {
      a = W.__kimoSkipped[i];
      try {
        if (a.playState !== 'paused') {
          var abs2 = (a.startTime === null || typeof a.startTime !== 'number' || isNaN(a.startTime));
          a.__kimoPausedByUs = true;
          a.pause();
          W.__kimoAnims.push({ a: a, c0: a.currentTime, abs: abs2, d0: tlNow() });
          W.__kimoSkipped.splice(i, 1);
        }
      } catch (e) {}
    }
  };
  W.__kimoFreeze = function () {
    if (W.__kimoFrozen) return;
    W.__kimoFrozen = true;
    W.__kimoRescan();
  };
  W.__kimoSeek = function (tMs) {
    if (!W.__kimoFrozen) W.__kimoFreeze();
    else W.__kimoRescan();
    for (var i = 0; i < W.__kimoAnims.length; i++) {
      var r = W.__kimoAnims[i];
      try {
        r.a.currentTime = r.abs ? (r.c0 + tMs) : (r.c0 + tMs - r.d0);
      } catch (e) {}
    }
  };
})();
"#;

/// One-shot probe evaluated after the page settles. Returns a JSON string.
pub const PROBE_JS: &str = r#"
(function () {
  var out = {};
  try {
    var svg = document.documentElement;
    out.rootTag = svg.tagName;
    out.nsOk = !!(svg.namespaceURI && svg.namespaceURI.indexOf('svg') >= 0);
    out.attrW = svg.getAttribute('width');
    out.attrH = svg.getAttribute('height');
    out.viewBox = svg.getAttribute('viewBox');
    var cs = null;
    try { cs = getComputedStyle(svg); } catch (e) {}
    out.computedW = cs ? parseFloat(cs.width) : null;
    out.computedH = cs ? parseFloat(cs.height) : null;
    out.probeW = window.innerWidth;
    out.probeH = window.innerHeight;
    out.fontsStatus = (document.fonts && document.fonts.status) ? document.fonts.status : 'na';

    var smil = [];
    try {
      var els = document.querySelectorAll('animate,animateTransform,animateMotion,set,animateColor');
      for (var i = 0; i < els.length; i++) {
        var e = els[i];
        var tgt = null;
        try { if (e.parentNode) tgt = e.parentNode.id || e.parentNode.tagName; } catch (er) {}
        smil.push({
          tag: e.tagName, id: e.id || null, target: tgt,
          begin: e.getAttribute('begin'),
          dur: e.getAttribute('dur'),
          end: e.getAttribute('end'),
          repeatCount: e.getAttribute('repeatCount'),
          repeatDur: e.getAttribute('repeatDur'),
          fill: e.getAttribute('fill')
        });
      }
    } catch (err) {}
    out.smil = smil;

    var anims = [];
    try {
      var A = document.getAnimations();
      for (var k = 0; k < A.length; k++) {
        var a = A[k], t = {};
        try { t = a.effect ? a.effect.getComputedTiming() : {}; } catch (er) {}
        anims.push({
          kind: (typeof CSSAnimation !== 'undefined' && a instanceof CSSAnimation) ? 'css-animation'
              : (typeof CSSTransition !== 'undefined' && a instanceof CSSTransition) ? 'css-transition'
              : 'waapi',
          playState: a.playState,
          startTime: (a.startTime === null || typeof a.startTime !== 'number') ? null : a.startTime,
          delay: (typeof t.delay === 'number') ? t.delay : 0,
          duration: (t.duration === Infinity) ? -1 : ((typeof t.duration === 'number') ? t.duration : null),
          iterations: (t.iterations === Infinity) ? -1 : ((typeof t.iterations === 'number') ? t.iterations : null),
          endTime: (t.endTime === Infinity) ? -1 : ((typeof t.endTime === 'number') ? t.endTime : null)
        });
      }
    } catch (err2) {}
    out.anims = anims;

    out.stats = window.__kimoStats || null;
    out.iframes = document.querySelectorAll('iframe').length;
    out.foreignObject = document.querySelectorAll('foreignObject').length;
    out.scripts = document.querySelectorAll('script').length;
    var remote = 0;
    try {
      var all = document.querySelectorAll('*');
      for (var m = 0; m < all.length; m++) {
        var h = null;
        try {
          h = all[m].getAttribute('href') || all[m].getAttributeNS('http://www.w3.org/1999/xlink', 'href') || all[m].getAttribute('src');
        } catch (er3) {}
        if (h && /^https?:/i.test(h)) remote++;
      }
    } catch (err3) {}
    out.remoteRefs = remote;
    return JSON.stringify(out);
  } catch (fatal) {
    return JSON.stringify({ fatal: String(fatal) });
  }
})()
"#;

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ProbeStats {
    #[serde(default)]
    pub raf_queued: f64,
    #[serde(default)]
    pub raf_run: f64,
    #[serde(default)]
    pub timers: f64,
    #[serde(default)]
    pub interval_fires: f64,
    #[serde(default)]
    pub canvas2d: f64,
    #[serde(default)]
    pub webgl: f64,
    #[serde(default)]
    pub gate_auto_fired: bool,
    #[serde(default)]
    pub gate_released: bool,
    #[serde(default)]
    pub scripts: f64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ProbeData {
    #[serde(rename = "rootTag")]
    pub root_tag: String,
    #[serde(rename = "nsOk", default)]
    pub ns_ok: bool,
    #[serde(rename = "attrW")]
    pub attr_w: Option<String>,
    #[serde(rename = "attrH")]
    pub attr_h: Option<String>,
    #[serde(rename = "viewBox")]
    pub view_box: Option<String>,
    #[serde(rename = "computedW")]
    pub computed_w: Option<f64>,
    #[serde(rename = "computedH")]
    pub computed_h: Option<f64>,
    #[serde(rename = "probeW")]
    pub probe_w: f64,
    #[serde(rename = "probeH")]
    pub probe_h: f64,
    #[serde(rename = "fontsStatus", default)]
    pub fonts_status: String,
    #[serde(default)]
    pub smil: Vec<SmilRaw>,
    #[serde(default)]
    pub anims: Vec<AnimRaw>,
    pub stats: Option<ProbeStats>,
    #[serde(default)]
    pub iframes: f64,
    #[serde(default)]
    pub foreign_object: f64,
    #[serde(default)]
    pub scripts: f64,
    #[serde(default)]
    pub remote_refs: f64,
    #[serde(default)]
    pub fatal: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SmilRaw {
    pub tag: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    pub begin: Option<String>,
    pub dur: Option<String>,
    pub end: Option<String>,
    #[serde(rename = "repeatCount")]
    pub repeat_count: Option<String>,
    #[serde(rename = "repeatDur")]
    pub repeat_dur: Option<String>,
    #[serde(default)]
    pub fill: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AnimRaw {
    pub kind: String,
    #[serde(rename = "playState", default)]
    pub play_state: String,
    pub start_time: Option<f64>,
    #[serde(default)]
    pub delay: f64,
    pub duration: Option<f64>,
    pub iterations: Option<f64>,
    pub end_time: Option<f64>,
}

impl ProbeData {
    pub fn parse(json_str: &str) -> Result<Self> {
        serde_json::from_str(json_str)
            .map_err(|e| KimoError::Cdp(format!("failed to parse probe result: {e}")))
    }
}

/// The rendering strategy chosen from the probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// SMIL/CSS/WAAPI only: pin every clock and seek each frame directly.
    Seek,
    /// JS animation + CSS/WAAPI/SMIL: virtual time for JS, pinned clocks for
    /// the rest, all aligned to the same instant each frame.
    Hybrid,
    /// JS animation only: pure deterministic virtual-time stepping.
    Virtual,
    /// Nothing animates: one capture repeated for the whole duration.
    Static,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Seek => "seek",
            Mode::Hybrid => "hybrid (virtual time + pinned CSS/SMIL)",
            Mode::Virtual => "virtual-time",
            Mode::Static => "static",
        }
    }
}

/// The full analysis outcome.
pub struct Analysis {
    pub dims: ResolvedDims,
    pub mode: Mode,
    pub duration: Option<f64>, // None = requires --duration
    pub duration_source: String,
    pub doc_plan: DocPlan,
    pub smil_count: usize,
    pub css_count: usize,
    pub waapi_count: usize,
    pub js_driven: bool,
    pub warnings: Vec<String>,
    pub probe: ProbeData,
}

pub fn analyze_probe(probe: &ProbeData, duration_flag: Option<f64>) -> Result<Analysis> {
    if let Some(fatal) = &probe.fatal {
        return Err(KimoError::Cdp(format!(
            "in-page analysis failed: {fatal}"
        )));
    }
    if !probe.ns_ok {
        return Err(KimoError::Usage(format!(
            "the document root is <{}>, not an SVG — kimo only renders SVG documents",
            probe.root_tag
        )));
    }

    // ---- resolution --------------------------------------------------------
    let dims = resolve_dims(
        &probe.attr_w,
        &probe.attr_h,
        &probe.view_box,
        (probe.computed_w, probe.computed_h),
        (probe.probe_w, probe.probe_h),
    )
    .map_err(KimoError::Usage)?;

    // ---- SMIL plan ---------------------------------------------------------
    let smil_elements: Vec<SmilElement> = probe
        .smil
        .iter()
        .map(|s| SmilElement {
            tag: s.tag.clone(),
            id: s.id.clone(),
            target: s.target.clone(),
            begin_raw: s.begin.clone(),
            dur_raw: s.dur.clone(),
            end_raw: s.end.clone(),
            repeat_count_raw: s.repeat_count.clone(),
            repeat_dur_raw: s.repeat_dur.clone(),
            fill: s.fill.clone(),
        })
        .collect();
    let smil_plans = crate::anim::plan_smil(&smil_elements);

    // ---- CSS / WAAPI plan --------------------------------------------------
    let browser_anims: Vec<BrowserAnim> = probe
        .anims
        .iter()
        .map(|a| BrowserAnim {
            kind: a.kind.clone(),
            start_time: a.start_time,
            delay: a.delay,
            duration: a.duration,
            iterations: a.iterations,
            end_time: a.end_time,
        })
        .collect();

    let mut doc_plan = DocPlan::default();
    let mut smil_labels = Vec::new();
    for (i, plan) in &smil_plans {
        let e = &smil_elements[*i];
        let label = format!(
            "<{}{}{}>",
            e.tag,
            e.id.as_ref().map(|i| format!(" #{i}")).unwrap_or_default(),
            e.begin_raw
                .as_ref()
                .map(|b| format!(" begin=\"{b}\""))
                .unwrap_or_default()
        );
        smil_labels.push(label.clone());
        doc_plan.add(plan, &label);
    }
    let mut css_labels = Vec::new();
    for (i, a) in browser_anims.iter().enumerate() {
        let plan = a.plan();
        let label = format!("{} #{}", a.kind, i + 1);
        css_labels.push(label.clone());
        doc_plan.add(&plan, &label);
    }

    let smil_count = smil_elements.len();
    let css_count = browser_anims
        .iter()
        .filter(|a| a.kind.starts_with("css"))
        .count();
    let waapi_count = browser_anims.iter().filter(|a| a.kind == "waapi").count();

    // ---- JS-driven detection -----------------------------------------------
    let stats = probe.stats.clone().unwrap_or_default();
    let raf_queued = stats.raf_queued as u64;
    let timer_fires = stats.timers as u64;
    let interval_fires = stats.interval_fires as u64;
    let scripts = (probe.scripts.max(stats.scripts) as u64) + if probe.scripts > 0.0 { 0 } else { 0 };
    let js_driven = scripts > 0
        && (raf_queued >= 1 || timer_fires >= 15 || interval_fires >= 5)
        || probe.iframes as u64 > 0;

    // ---- duration ------------------------------------------------------------
    let mut warnings = Vec::new();
    let mut duration: Option<f64> = None;
    let mut duration_source = String::new();

    if let Some(d) = duration_flag {
        duration = Some(d);
        duration_source = format!("--duration {d}s (override)");
        if js_driven || !doc_plan.unknown.is_empty() {
            // Fine: the user told us how long to record.
        }
    } else if js_driven {
        duration = None;
        duration_source = "requires --duration: JavaScript-driven animation detected \
(rAF/timers drive the timeline; the end cannot be computed statically)"
            .to_string();
    } else if !doc_plan.unknown.is_empty() {
        duration = None;
        duration_source = "requires --duration: unknown timeline (event-based or unresolved SMIL timing)"
            .to_string();
    } else {
        let suggested = doc_plan.suggested_duration().unwrap_or(crate::anim::STATIC_DURATION);
        duration = Some(suggested);
        duration_source = format!("auto-detected: {}", doc_plan.formula_string());
    }

    if duration.is_none() {
        // Enrich the message with the concrete reasons.
        let mut reasons: Vec<String> = doc_plan.unknown.clone();
        if js_driven {
            reasons.push(format!(
                "JS timing usage: requestAnimationFrame queued {raf_queued}, setTimeout fired {timer_fires}, setInterval fired {interval_fires}"
            ));
        }
        duration_source = format!("{duration_source} — {}", reasons.join("; "));
    }

    // ---- mode -----------------------------------------------------------------
    let has_csswaa = css_count + waapi_count > 0;
    let mode = if js_driven {
        if smil_count > 0 || has_csswaa {
            Mode::Hybrid
        } else {
            Mode::Virtual
        }
    } else if smil_count > 0 || has_csswaa {
        Mode::Seek
    } else {
        Mode::Static
    };

    // ---- warnings ---------------------------------------------------------------
    if dims.width % 2 == 1 || dims.height % 2 == 1 {
        warnings.push(format!(
            "output dimensions {}x{} are odd — libx264/yuv420p needs even numbers; \
kimo pads by 1px (right/bottom) during encoding",
            dims.width, dims.height
        ));
    }
    if dims.inferred {
        warnings.push(format!(
            "resolution was inferred ({}); set explicit width/height on the <svg> to pin it",
            dims.source
        ));
    }
    if probe.remote_refs > 0.0 {
        warnings.push(format!(
            "{} reference(s) to remote http(s) resources — rendering depends on network availability",
            probe.remote_refs as u64
        ));
    }
    if probe.foreign_object > 0.0 {
        warnings.push(format!(
            "{} <foreignObject> element(s) — rendered natively by Chrome",
            probe.foreign_object as u64
        ));
    }
    if stats.gate_auto_fired && js_driven {
        warnings.push(
            "the rAF gate auto-released before capture started (slow page setup); \
frame 0 may include a few milliseconds of JS-driven motion".into(),
        );
    }
    if doc_plan.holds > 0 {
        warnings.push(format!(
            "{} animation(s) have indefinite duration and never change after start (hold)",
            doc_plan.holds
        ));
    }
    if smil_count > 0 {
        // Elements whose begin/end lists contain unresolvable tokens.
        for (i, plan) in &smil_plans {
            if matches!(plan, ElementPlan::Unknown(_)) {
                if let Some(b) = &smil_elements[*i].begin_raw {
                    if parse_instance_list(Some(b)).iter().any(|t| matches!(t, crate::anim::BeginToken::Unknown(_))) {
                        // already covered by doc_plan.unknown
                    }
                }
            }
        }
    }
    let _ = smil_labels;
    let _ = css_labels;

    if let Some(d) = duration {
        if d > 600.0 {
            warnings.push(format!(
                "duration {d:.1}s is very long — a {d:.0}s video at 60fps is {} frames",
                (d * crate::anim::FPS).round() as u64
            ));
        }
    }

    Ok(Analysis {
        dims,
        mode,
        duration,
        duration_source,
        doc_plan,
        smil_count,
        css_count,
        waapi_count,
        js_driven,
        warnings,
        probe: probe.clone(),
    })
}

/// Render the human-readable `--analyze` report to stdout.
pub fn print_report(a: &Analysis, input_display: &str, size_bytes: u64, product: &str, profile: &str) {
    let mut w = String::new();
    w.push_str("KIMO ANALYSIS\n=============\n\n");

    w.push_str(&format!("Input\n  file:    {input_display}\n  size:    {} bytes\n\n", size_bytes));

    w.push_str("Renderer\n");
    w.push_str(&format!("  browser: {product}\n"));
    w.push_str(&format!("  profile: {profile} (persistent, reused across runs)\n\n"));

    w.push_str("Resolution\n");
    w.push_str(&format!(
        "  output:  {} x {} px\n  source:  {}\n",
        a.dims.width, a.dims.height, a.dims.source
    ));
    if a.dims.inferred {
        w.push_str("  note:    inferred (not pinned by the SVG itself)\n");
    }
    w.push('\n');

    w.push_str("Animations\n");
    w.push_str(&format!("  SMIL elements:      {}\n", a.smil_count));
    w.push_str(&format!("  CSS animations:     {}\n", a.css_count));
    w.push_str(&format!("  CSS transitions:    (counted above)\n"));
    w.push_str(&format!("  WAAPI animations:   {}\n", a.waapi_count));
    w.push_str(&format!(
        "  JS-driven:          {}\n",
        if a.js_driven { "yes" } else { "no" }
    ));
    let stats = a.probe.stats.clone().unwrap_or_default();
    w.push_str(&format!(
        "  JS timing usage:    rAF queued {}, fired {}, setTimeout fired {}, setInterval fired {}\n",
        stats.raf_queued as u64, stats.raf_run as u64, stats.timers as u64, stats.interval_fires as u64
    ));
    w.push_str(&format!(
        "  canvas/WebGL:       {} 2d, {} webgl contexts\n",
        stats.canvas2d as u64, stats.webgl as u64
    ));
    w.push_str(&format!(
        "  iframes/foreignObject: {} / {}\n\n",
        a.probe.iframes as u64, a.probe.foreign_object as u64
    ));

    w.push_str("Timeline\n");
    if !a.doc_plan.finite_ends.is_empty() {
        let latest = a.doc_plan.finite_ends.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        w.push_str(&format!(
            "  finite animations:  {} (latest end {:.3}s)\n",
            a.doc_plan.finite_ends.len(),
            latest
        ));
    } else {
        w.push_str("  finite animations:  none\n");
    }
    if !a.doc_plan.loops.is_empty() {
        w.push_str("  infinite loops:\n");
        for (start, cycle) in a.doc_plan.loops.iter().take(12) {
            w.push_str(&format!("    loop starting {start:.3}s, cycle {cycle:.3}s\n"));
        }
        if a.doc_plan.loops.len() > 12 {
            w.push_str(&format!("    ... and {} more\n", a.doc_plan.loops.len() - 12));
        }
    }
    if !a.doc_plan.unknown.is_empty() {
        w.push_str("  unknown timelines (require --duration):\n");
        for u in a.doc_plan.unknown.iter().take(12) {
            w.push_str(&format!("    {u}\n"));
        }
    }
    w.push('\n');

    w.push_str("Duration\n");
    w.push_str(&format!("  {}\n", a.duration_source));
    if let Some(d) = a.duration {
        w.push_str(&format!(
            "  video:              {d:.3}s = {} frames at 60fps (t = frame/60)\n",
            crate::anim::frames_for(d)
        ));
    } else {
        w.push_str("  render with:        kimo -i <file> -o <out.mp4> --duration <SECONDS>\n");
    }
    w.push('\n');

    w.push_str("Render mode\n");
    w.push_str(&format!("  {}\n\n", a.mode.as_str()));

    if !a.warnings.is_empty() {
        w.push_str("Warnings\n");
        for warn in &a.warnings {
            w.push_str(&format!("  - {warn}\n"));
        }
        w.push('\n');
    }

    print!("{w}");
}

/// Convert a `Runtime.evaluate` result into a plain JSON value / string.
pub fn eval_to_string(result: &Value) -> Result<String> {
    let text = result
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if let Some(t) = text {
        return Ok(t);
    }
    // Maybe an exception was thrown.
    if let Some(exc) = result.get("exceptionDetails") {
        let desc = exc
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(|d| d.as_str())
            .or_else(|| {
                exc.get("text")
                    .and_then(|t| t.as_str())
            })
            .unwrap_or("unknown in-page exception");
        return Err(KimoError::Cdp(format!("page script raised: {desc}")));
    }
    Ok(result
        .get("result")
        .and_then(|r| r.get("value"))
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".into()))
}

/// One CDP `Runtime.evaluate` call returning a string.
pub async fn eval_js(cdp: &crate::cdp::Cdp, session: &str, expr: &str) -> Result<String> {
    let res = cdp
        .call(
            Some(session),
            "Runtime.evaluate",
            serde_json::json!({
                "expression": expr,
                "returnByValue": true,
                "awaitPromise": false,
            }),
            Duration::from_secs(60),
        )
        .await?;
    eval_to_string(&res)
}

/// One CDP `Runtime.evaluate` call awaiting a promise.
pub async fn eval_js_promise(cdp: &crate::cdp::Cdp, session: &str, expr: &str) -> Result<String> {
    let res = cdp
        .call(
            Some(session),
            "Runtime.evaluate",
            serde_json::json!({
                "expression": expr,
                "returnByValue": true,
                "awaitPromise": true,
            }),
            Duration::from_secs(90),
        )
        .await?;
    eval_to_string(&res)
}
