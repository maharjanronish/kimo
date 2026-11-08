//! Animation timeline mathematics.
//!
//! SMIL timing attributes are parsed here (clock values, begin/end instance
//! lists, repeat semantics, syncbase resolution). CSS / Web Animations timing
//! comes from the browser itself via `getAnimations().effect.getComputedTiming()`.
//! The aggregation implements Kimo's duration rules:
//!
//! * latest finite end across all finite animations
//! * infinite loops are reported honestly and the suggestion is
//!   `max(latest finite end, latest loop start) + longest cycle`
//! * static documents render for 1 second
//! * unknown timelines (event-based SMIL, JS-driven animation) require
//!   `--duration`
//! * output resolution follows: explicit px > one dimension + viewBox
//!   derivation > viewBox-only (1280 px longest side) > 300x150

use std::collections::HashMap;

pub const FPS: f64 = 60.0;
pub const STATIC_DURATION: f64 = 1.0;
pub const DEFAULT_LONGEST_SIDE: f64 = 1280.0;

// ---------------------------------------------------------------------------
// SMIL clock-value parsing
// ---------------------------------------------------------------------------

/// Parse a single SMIL clock value, e.g. `3`, `3s`, `250ms`, `2h`, `45min`,
/// `01:30`, `0:01:30.25`, `-1.5s`. Returns seconds.
pub fn parse_clock(raw: &str) -> Result<f64, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("empty clock value".into());
    }
    let (neg, body) = if let Some(rest) = s.strip_prefix('-') {
        (true, rest)
    } else if let Some(rest) = s.strip_prefix('+') {
        (false, rest)
    } else {
        (false, s)
    };
    if body.is_empty() {
        return Err("empty clock value".into());
    }

    // Colon forms: hh:mm:ss.frac or mm:ss.frac
    if body.contains(':') {
        let parts: Vec<&str> = body.split(':').collect();
        if parts.len() == 2 {
            let (m, sec) = (parts[0], parts[1]);
            let minutes: f64 = m.parse().map_err(|_| format!("bad minutes in clock value '{raw}'"))?;
            let seconds: f64 = sec
                .parse()
                .map_err(|_| format!("bad seconds in clock value '{raw}'"))?;
            if !(0.0..=60.0).contains(&seconds) && !sec.contains('.') {
                // still accept, SMIL allows fractional overflow; just parse
            }
            let total = minutes * 60.0 + seconds;
            return Ok(if neg { -total } else { total });
        }
        if parts.len() == 3 {
            let h: f64 = parts[0].parse().map_err(|_| format!("bad hours in clock value '{raw}'"))?;
            let m: f64 = parts[1].parse().map_err(|_| format!("bad minutes in clock value '{raw}'"))?;
            let sec: f64 = parts[2].parse().map_err(|_| format!("bad seconds in clock value '{raw}'"))?;
            let total = h * 3600.0 + m * 60.0 + sec;
            return Ok(if neg { -total } else { total });
        }
        return Err(format!("unsupported clock value '{raw}'"));
    }

    // Timecount with optional metric suffix.
    let (num_part, metric): (String, Option<String>) = {
        let idx = body.find(|c: char| !(c.is_ascii_digit() || c == '.'));
        match idx {
            Some(i) => (body[..i].to_string(), Some(body[i..].to_string())),
            None => (body.to_string(), None),
        }
    };
    let n: f64 = num_part
        .parse()
        .map_err(|_| format!("bad number in clock value '{raw}'"))?;
    let mult = match metric.as_deref() {
        None | Some("s") => 1.0,
        Some("ms") => 0.001,
        Some("min") => 60.0,
        Some("h") => 3600.0,
        Some(other) => return Err(format!("unknown time metric '{other}' in '{raw}'")),
    };
    let total = n * mult;
    Ok(if neg { -total } else { total })
}

// ---------------------------------------------------------------------------
// SMIL begin/end token parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum BeginToken {
    /// Plain offset from document begin, in seconds.
    Offset(f64),
    /// `id.begin+2s` / `id.end-1s` — resolved against another element.
    Syncbase { id: String, on_end: bool, offset: f64 },
    /// Event-value, accessKey, wallclock, repeat-value or indefinite —
    /// cannot be resolved statically, honestly reported as unknown.
    Unknown(String),
}

fn split_instance_list(raw: &str) -> Vec<String> {
    raw.split([';', ','])
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Parse a begin/end attribute value into its instance-time tokens.
pub fn parse_instance_list(raw: Option<&str>) -> Vec<BeginToken> {
    let Some(raw) = raw else {
        return vec![];
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return vec![];
    }
    split_instance_list(raw)
        .into_iter()
        .map(|tok| {
            let lower = tok.to_ascii_lowercase();
            if lower == "indefinite" {
                return BeginToken::Unknown("indefinite".into());
            }
            if lower.starts_with("wallclock(") {
                return BeginToken::Unknown("wallclock".into());
            }
            if lower.starts_with("accesskey(") || lower.starts_with("repeat(") {
                return BeginToken::Unknown("conditional event value".into());
            }
            // Clock values are a strict grammar — try them FIRST so that
            // fractional values like "8.05s" are never mistaken for the
            // `id.marker+offset` syncbase form.
            if let Ok(v) = parse_clock(&tok) {
                return BeginToken::Offset(v);
            }
            // `id.begin+2s` / `id.end-1s` syncbase.
            if let Some(dot) = tok.find('.') {
                let id = tok[..dot].to_string();
                let rest = &tok[dot + 1..];
                let (marker, offset) = split_marker_offset(rest);
                if id.is_empty() || marker.is_empty() {
                    return BeginToken::Unknown(format!("malformed syncbase '{tok}'"));
                }
                if marker.ends_with("event") {
                    return BeginToken::Unknown("event-based syncbase".into());
                }
                let is_end = marker.starts_with("end");
                return BeginToken::Syncbase { id, on_end: is_end, offset };
            }
            // Bare event name like `click`, `load`, `focusin`.
            BeginToken::Unknown(format!("event value '{tok}'"))
        })
        .collect()
}

fn split_marker_offset(rest: &str) -> (String, f64) {
    // rest like "begin+2s", "end-1s", "begin", "endEvent+0.5s"
    let (marker, offset_raw) = if let Some(i) = rest.find(['+', '-']) {
        (rest[..i].to_string(), &rest[i..])
    } else {
        (rest.to_string(), "")
    };
    let offset = if offset_raw.is_empty() {
        0.0
    } else {
        parse_clock(offset_raw).unwrap_or(0.0)
    };
    (marker.trim().to_ascii_lowercase(), offset)
}

// ---------------------------------------------------------------------------
// Per-element SMIL timing plan
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SmilElement {
    pub tag: String,
    pub id: Option<String>,
    pub target: Option<String>,
    pub begin_raw: Option<String>,
    pub dur_raw: Option<String>,
    pub end_raw: Option<String>,
    pub repeat_count_raw: Option<String>,
    pub repeat_dur_raw: Option<String>,
    pub fill: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ElementPlan {
    /// Finite: last active end in seconds.
    Finite(f64),
    /// Infinite loop: starts at `start`, one full cycle takes `cycle` seconds.
    Loop { start: f64, cycle: f64 },
    /// Runs but never changes after start (indefinite simple duration).
    Hold,
    /// Never begins in a non-interactive render (no valid begin instance).
    Inert,
    /// Timing cannot be resolved statically (event-based etc.).
    Unknown(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Dur {
    Secs(f64),
    Indefinite,
}

struct Repeat {
    /// number of iterations (>= 0) or infinite
    count: f64,
    /// total repeat duration cap, seconds, or infinite
    dur: Dur,
}

fn parse_dur(raw: Option<&str>) -> Result<Dur, String> {
    match raw {
        None => Ok(Dur::Indefinite), // default simple duration is indefinite
        Some(r) => {
            let r = r.trim();
            if r.eq_ignore_ascii_case("indefinite") || r.eq_ignore_ascii_case("media") {
                Ok(Dur::Indefinite)
            } else {
                Ok(Dur::Secs(parse_clock(r)?))
            }
        }
    }
}

fn parse_repeat(raw: Option<&str>) -> Repeat {
    let count = match raw {
        None => 1.0,
        Some(r) => {
            let r = r.trim();
            if r.eq_ignore_ascii_case("indefinite") {
                f64::INFINITY
            } else {
                r.parse().unwrap_or(1.0)
            }
        }
    };
    Repeat { count, dur: Dur::Secs(f64::INFINITY) }
}

/// Resolve all SMIL element plans together (syncbase fixpoint).
///
/// `id.end` resolves against the referenced element's effective end times:
/// explicit `end` instances plus each active interval's natural end
/// (begin + active duration), which is what Chrome uses.
pub fn plan_smil(elements: &[SmilElement]) -> Vec<(usize, ElementPlan)> {
    // Gather id -> element index for syncbase resolution.
    let mut by_id: HashMap<String, usize> = HashMap::new();
    for (i, e) in elements.iter().enumerate() {
        if let Some(id) = &e.id {
            if !id.is_empty() {
                by_id.insert(id.clone(), i);
            }
        }
    }

    let n = elements.len();

    // Begin defaults to a single 0s instance when the attribute is absent.
    let mut begins: Vec<Option<Vec<f64>>> = elements
        .iter()
        .map(|e| if e.begin_raw.is_none() { Some(vec![0.0]) } else { None })
        .collect();
    // Explicit `end` attribute instances (empty when absent).
    let mut explicit_ends: Vec<Option<Vec<f64>>> = elements
        .iter()
        .map(|e| if e.end_raw.is_none() { Some(vec![]) } else { None })
        .collect();
    // Effective end times: explicit ends plus natural interval ends.
    let mut effective_ends: Vec<Option<Vec<f64>>> = vec![None; n];
    let mut unknowns: Vec<Option<String>> = vec![None; n];

    let begin_tokens: Vec<Vec<BeginToken>> = elements
        .iter()
        .map(|e| parse_instance_list(e.begin_raw.as_deref()))
        .collect();
    let end_tokens: Vec<Vec<BeginToken>> = elements
        .iter()
        .map(|e| parse_instance_list(e.end_raw.as_deref()))
        .collect();

    for _round in 0..8 {
        let mut changed = false;
        for i in 0..n {
            if unknowns[i].is_some() {
                continue;
            }
            // 1. begins
            if begins[i].is_none() {
                match resolve_tokens(&begin_tokens[i], &by_id, &begins, &effective_ends) {
                    Ok(list) => {
                        begins[i] = Some(list);
                        changed = true;
                    }
                    Err(reason) => {
                        unknowns[i] = Some(reason);
                        changed = true;
                    }
                }
            }
            if unknowns[i].is_some() || begins[i].is_none() {
                continue;
            }
            // 2. explicit end instances
            if explicit_ends[i].is_none() {
                match resolve_tokens(&end_tokens[i], &by_id, &begins, &effective_ends) {
                    Ok(list) => {
                        explicit_ends[i] = Some(list);
                        changed = true;
                    }
                    Err(reason) => {
                        unknowns[i] = Some(reason);
                        changed = true;
                    }
                }
            }
            if unknowns[i].is_some() || explicit_ends[i].is_none() {
                continue;
            }
            // 3. effective ends (explicit + natural interval ends)
            if effective_ends[i].is_none() {
                let active = active_duration(&elements[i]);
                let mut eff = explicit_ends[i].clone().unwrap_or_default();
                if let Some(active) = active {
                    if active.is_finite() {
                        for b in begins[i].clone().unwrap_or_default() {
                            eff.push(b + active);
                        }
                    }
                }
                effective_ends[i] = Some(eff);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    // Anything still unresolved counts as unknown.
    for i in 0..n {
        if unknowns[i].is_none() && (begins[i].is_none() || explicit_ends[i].is_none()) {
            unknowns[i] = Some("unresolved syncbase dependency".into());
        }
    }

    let mut out = Vec::with_capacity(n);
    for (i, e) in elements.iter().enumerate() {
        let plan = if let Some(reason) = &unknowns[i] {
            ElementPlan::Unknown(reason.clone())
        } else {
            plan_element(
                e,
                begins[i].as_ref().unwrap_or(&vec![]),
                explicit_ends[i].as_ref().unwrap_or(&vec![]),
            )
        };
        out.push((i, plan));
    }
    out
}

fn resolve_tokens(
    tokens: &[BeginToken],
    by_id: &HashMap<String, usize>,
    begins: &[Option<Vec<f64>>],
    effective_ends: &[Option<Vec<f64>>],
) -> std::result::Result<Vec<f64>, String> {
    let mut out = Vec::new();
    for t in tokens {
        match t {
            BeginToken::Offset(v) => out.push(*v),
            BeginToken::Syncbase { id, on_end, offset } => {
                let Some(&j) = by_id.get(id) else {
                    // Unknown id: Chrome ignores the instance time entirely.
                    continue;
                };
                let source = if *on_end {
                    &effective_ends[j]
                } else {
                    &begins[j]
                };
                match source {
                    Some(list) => {
                        for v in list {
                            out.push(v + offset);
                        }
                    }
                    None => return Err(format!("syncbase dependency on '{id}' did not resolve")),
                }
            }
            BeginToken::Unknown(kind) => return Err(kind.clone()),
        }
    }
    Ok(out)
}

/// Total active duration from dur/repeatCount/repeatDur, or None when the
/// timing attributes are unparseable.
fn active_duration(e: &SmilElement) -> Option<f64> {
    let dur = match parse_dur(e.dur_raw.as_deref()) {
        Ok(Dur::Secs(s)) => s,
        Ok(Dur::Indefinite) => f64::INFINITY,
        Err(_) => return None,
    };
    let rc = match e.repeat_count_raw.as_deref().map(|r| r.trim()) {
        None => 1.0,
        Some(r) if r.eq_ignore_ascii_case("indefinite") => f64::INFINITY,
        Some(r) => r.parse::<f64>().ok()?,
    };
    let rd = match e.repeat_dur_raw.as_deref() {
        None => f64::INFINITY,
        Some(r) => {
            let r = r.trim();
            if r.eq_ignore_ascii_case("indefinite") {
                f64::INFINITY
            } else {
                parse_clock(r).ok()?
            }
        }
    };
    let base = dur * rc;
    Some(if rd.is_finite() { base.min(rd) } else { base })
}

fn plan_element(e: &SmilElement, begins: &[f64], ends: &[f64]) -> ElementPlan {
    if begins.is_empty() {
        return ElementPlan::Inert;
    }
    let dur = match parse_dur(e.dur_raw.as_deref()) {
        Ok(d) => d,
        Err(reason) => return ElementPlan::Unknown(format!("bad dur attribute: {reason}")),
    };
    let rc_raw = e.repeat_count_raw.as_deref().map(|r| r.trim());
    let repeat_count = match rc_raw {
        None => 1.0,
        Some(r) if r.eq_ignore_ascii_case("indefinite") => f64::INFINITY,
        Some(r) => match r.parse::<f64>() {
            Ok(v) if v >= 0.0 => v,
            _ => return ElementPlan::Unknown(format!("bad repeatCount '{r}'")),
        },
    };
    let repeat_dur = match e.repeat_dur_raw.as_deref() {
        None => f64::INFINITY,
        Some(r) => {
            let r = r.trim();
            if r.eq_ignore_ascii_case("indefinite") {
                f64::INFINITY
            } else {
                match parse_clock(r) {
                    Ok(v) => v,
                    Err(reason) => return ElementPlan::Unknown(format!("bad repeatDur: {reason}")),
                }
            }
        }
    };

    let simple_dur = match dur {
        Dur::Secs(s) => s,
        Dur::Indefinite => f64::INFINITY,
    };
    let active_base = simple_dur * repeat_count; // inf * anything >= 0 → inf
    let active = if repeat_dur.is_finite() {
        active_base.min(repeat_dur)
    } else {
        active_base
    };

    let latest_begin = begins.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

    // Explicit end instances cap the active duration (absolute doc times).
    let mut finite_end: Option<f64> = None;
    for &b in begins {
        let mut inst_end = if active.is_finite() { b + active } else { f64::INFINITY };
        if !ends.is_empty() {
            let cap = ends
                .iter()
                .filter(|&&ev| ev >= b)
                .cloned()
                .fold(f64::INFINITY, f64::min);
            if cap.is_finite() {
                inst_end = inst_end.min(cap);
            }
        }
        if inst_end.is_finite() && inst_end > b {
            finite_end = Some(finite_end.map_or(inst_end, |cur: f64| cur.max(inst_end)));
        }
    }

    if let Some(fe) = finite_end {
        if active.is_finite() || !ends.is_empty() {
            return ElementPlan::Finite(fe);
        }
    }
    if !ends.is_empty() {
        // Every interval was cut short/empty — nothing visible over time? Keep finite if any survived.
        if finite_end.is_none() {
            return ElementPlan::Inert;
        }
    }

    if active.is_infinite() {
        if simple_dur.is_finite() && simple_dur > 0.0 {
            return ElementPlan::Loop {
                start: latest_begin.max(0.0),
                cycle: simple_dur,
            };
        }
        return ElementPlan::Hold;
    }
    if active <= 0.0 {
        return ElementPlan::Inert;
    }
    // Finite active duration with no surviving interval end (end caps empty).
    let fe = latest_begin + active;
    if fe.is_finite() {
        ElementPlan::Finite(fe)
    } else {
        ElementPlan::Hold
    }
}

// ---------------------------------------------------------------------------
// CSS / Web Animations aggregation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BrowserAnim {
    pub kind: String, // css-animation | css-transition | waapi
    pub start_time: Option<f64>, // ms
    pub delay: f64,              // ms
    pub duration: Option<f64>,   // ms per iteration (-1 sentinel = infinite)
    pub iterations: Option<f64>, // -1 sentinel = infinite
    pub end_time: Option<f64>,   // ms (-1 sentinel = infinite)
}

impl BrowserAnim {
    pub fn plan(&self) -> ElementPlan {
        let start = self
            .start_time
            .map(|ms| ms / 1000.0)
            .or(if self.delay.is_finite() && self.delay.abs() > 0.0 {
                Some(self.delay / 1000.0)
            } else {
                Some(0.0)
            })
            .unwrap_or(0.0);
        let end = sentinel_to_inf(self.end_time);
        let iterations = sentinel_to_inf(self.iterations).unwrap_or(1.0);
        let dur = sentinel_to_inf(self.duration).unwrap_or(f64::INFINITY);

        if let Some(e) = end {
            if e.is_finite() {
                return ElementPlan::Finite(e / 1000.0);
            }
        }
        if iterations.is_infinite() {
            if dur.is_finite() && dur > 0.0 {
                return ElementPlan::Loop {
                    start,
                    cycle: dur / 1000.0,
                };
            }
            return ElementPlan::Hold;
        }
        if dur.is_finite() {
            let total = start + (dur * iterations) / 1000.0;
            if total.is_finite() {
                return ElementPlan::Finite(total);
            }
        }
        ElementPlan::Hold
    }
}

fn sentinel_to_inf(v: Option<f64>) -> Option<f64> {
    match v {
        None => None,
        Some(v) if v < 0.0 => Some(f64::INFINITY),
        Some(v) => Some(v),
    }
}

// ---------------------------------------------------------------------------
// Document-level duration plan
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct DocPlan {
    pub finite_ends: Vec<f64>,
    pub loops: Vec<(f64, f64)>, // (start, cycle)
    pub holds: usize,
    pub inert: usize,
    pub unknown: Vec<String>,
}

impl DocPlan {
    pub fn add(&mut self, plan: &ElementPlan, label: &str) {
        match plan {
            ElementPlan::Finite(e) => self.finite_ends.push(*e),
            ElementPlan::Loop { start, cycle } => self.loops.push((*start, *cycle)),
            ElementPlan::Hold => self.holds += 1,
            ElementPlan::Inert => self.inert += 1,
            ElementPlan::Unknown(reason) => {
                self.unknown.push(format!("{label}: {reason}"));
            }
        }
    }

    pub fn is_static(&self) -> bool {
        self.finite_ends.is_empty()
            && self.loops.is_empty()
            && self.unknown.is_empty()
            && self.holds == 0
    }

    pub fn has_any_animation(&self) -> bool {
        !self.finite_ends.is_empty() || !self.loops.is_empty() || self.holds > 0
    }

    /// The honest suggestion, or None when unknown.
    pub fn suggested_duration(&self) -> Option<f64> {
        if !self.unknown.is_empty() {
            return None;
        }
        if self.loops.is_empty() {
            if self.finite_ends.is_empty() {
                // Holds only (or nothing at all): static content.
                return Some(STATIC_DURATION);
            }
            return Some(self.finite_ends.iter().cloned().fold(f64::NEG_INFINITY, f64::max));
        }
        let latest_loop_start = self
            .loops
            .iter()
            .map(|(s, _)| *s)
            .fold(f64::NEG_INFINITY, f64::max);
        let longest_cycle = self
            .loops
            .iter()
            .map(|(_, c)| *c)
            .fold(f64::NEG_INFINITY, f64::max);
        let finite = self
            .finite_ends
            .iter()
            .cloned()
            .fold(f64::NEG_INFINITY, f64::max);
        let base = finite.max(latest_loop_start).max(0.0);
        Some(base + longest_cycle)
    }

    /// Human-readable formula for the suggestion.
    pub fn formula_string(&self) -> String {
        if !self.unknown.is_empty() {
            return "requires --duration (unknown timeline)".into();
        }
        if self.loops.is_empty() {
            if self.finite_ends.is_empty() {
                return "static document -> 1.0s".into();
            }
            let m = self.finite_ends.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            return format!("latest finite end = {m:.3}s");
        }
        let latest_loop_start = self.loops.iter().map(|(s, _)| *s).fold(f64::NEG_INFINITY, f64::max);
        let longest_cycle = self.loops.iter().map(|(_, c)| *c).fold(f64::NEG_INFINITY, f64::max);
        let finite = self.finite_ends.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        format!(
            "max(latest finite end = {f:.3}s, latest loop start = {s:.3}s) + longest cycle = {c:.3}s",
            f = if finite.is_finite() { finite } else { 0.0 },
            s = latest_loop_start,
            c = longest_cycle
        )
    }
}

pub fn frames_for(duration: f64) -> u64 {
    let n = (duration * FPS).round();
    n.max(1.0) as u64
}

// ---------------------------------------------------------------------------
// Output resolution rules
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedDims {
    pub width: u64,
    pub height: u64,
    pub source: String,
    pub inferred: bool,
}

fn attr_px(raw: &Option<String>) -> Option<f64> {
    let raw = raw.as_deref()?.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("auto") {
        return None;
    }
    let body = raw.strip_suffix("px").unwrap_or(raw).trim();
    let v: f64 = body.parse().ok()?;
    if v.is_finite() && v > 0.0 {
        Some(v)
    } else {
        None
    }
}

fn parse_viewbox(raw: &Option<String>) -> Option<(f64, f64, f64, f64)> {
    let raw = raw.as_deref()?.trim();
    let parts: Vec<f64> = raw
        .split_whitespace()
        .filter_map(|p| p.parse().ok())
        .collect();
    if parts.len() == 4 && parts[2] > 0.0 && parts[3] > 0.0 {
        Some((parts[0], parts[1], parts[2], parts[3]))
    } else {
        None
    }
}

/// Resolve output dimensions from the live DOM measurements.
/// `probe` is the viewport the measurement ran in (1920x1080).
pub fn resolve_dims(
    attr_w: &Option<String>,
    attr_h: &Option<String>,
    view_box: &Option<String>,
    computed: (Option<f64>, Option<f64>),
    probe: (f64, f64),
) -> Result<ResolvedDims, String> {
    let w = attr_px(attr_w);
    let h = attr_px(attr_h);
    let vb = parse_viewbox(view_box);

    // Rule 1: both explicit px.
    if let (Some(w), Some(h)) = (w, h) {
        return Ok(ResolvedDims {
            width: round_px(w),
            height: round_px(h),
            source: format!("explicit width/height attributes ({} x {})", trim(w), trim(h)),
            inferred: false,
        });
    }

    // Rule 2: one explicit px dimension + viewBox -> derive the other.
    if let Some(vb) = vb {
        let aspect = vb.3 / vb.2;
        if let Some(w) = w {
            let h = w * aspect;
            return Ok(ResolvedDims {
                width: round_px(w),
                height: round_px(h),
                source: format!(
                    "width attribute {} + viewBox aspect {}:{} -> height {}",
                    trim(w),
                    trim(vb.2),
                    trim(vb.3),
                    trim(h)
                ),
                inferred: false,
            });
        }
        if let Some(h) = h {
            let w = h / aspect;
            return Ok(ResolvedDims {
                width: round_px(w),
                height: round_px(h),
                source: format!(
                    "height attribute {} + viewBox aspect {}:{} -> width {}",
                    trim(h),
                    trim(vb.2),
                    trim(vb.3),
                    trim(w)
                ),
                inferred: false,
            });
        }

        // Rule 3: no usable px dims, but a viewBox. If CSS pins a concrete
        // size (computed differs from the probe viewport), honor it.
        let cw = computed.0.filter(|v| (*v - probe.0).abs() > 1.0 && *v > 0.0);
        let ch = computed.1.filter(|v| (*v - probe.1).abs() > 1.0 && *v > 0.0);
        if let (Some(cw), Some(ch)) = (cw, ch) {
            return Ok(ResolvedDims {
                width: round_px(cw),
                height: round_px(ch),
                source: format!("CSS-computed size {} x {}", trim(cw), trim(ch)),
                inferred: false,
            });
        }
        if let Some(cw) = cw {
            let h = cw * aspect;
            return Ok(ResolvedDims {
                width: round_px(cw),
                height: round_px(h),
                source: format!(
                    "CSS width {} + viewBox aspect -> height {}",
                    trim(cw),
                    trim(h)
                ),
                inferred: false,
            });
        }
        if let Some(ch) = ch {
            let w = ch / aspect;
            return Ok(ResolvedDims {
                width: round_px(w),
                height: round_px(ch),
                source: format!(
                    "CSS height {} + viewBox aspect -> width {}",
                    trim(ch),
                    trim(w)
                ),
                inferred: false,
            });
        }

        // viewBox only: 1280 px longest side, inferred.
        if vb.2 >= vb.3 {
            let width = DEFAULT_LONGEST_SIDE;
            let height = width * aspect;
            return Ok(ResolvedDims {
                width: round_px(width),
                height: round_px(height),
                source: format!(
                    "viewBox {}:{} only -> longest side fixed at 1280px",
                    trim(vb.2),
                    trim(vb.3)
                ),
                inferred: true,
            });
        } else {
            let height = DEFAULT_LONGEST_SIDE;
            let width = height / aspect;
            return Ok(ResolvedDims {
                width: round_px(width),
                height: round_px(height),
                source: format!(
                    "viewBox {}:{} only -> longest side fixed at 1280px",
                    trim(vb.2),
                    trim(vb.3)
                ),
                inferred: true,
            });
        }
    }

    // Rule 4: no viewBox. CSS-computed concrete size wins, else 300x150.
    let cw = computed.0.filter(|v| (*v - probe.0).abs() > 1.0 && *v > 0.0);
    let ch = computed.1.filter(|v| (*v - probe.1).abs() > 1.0 && *v > 0.0);
    if let (Some(cw), Some(ch)) = (cw, ch) {
        return Ok(ResolvedDims {
            width: round_px(cw),
            height: round_px(ch),
            source: format!("CSS-computed size {} x {}", trim(cw), trim(ch)),
            inferred: false,
        });
    }
    Ok(ResolvedDims {
        width: 300,
        height: 150,
        source: "no width/height/viewBox -> SVG default 300x150".to_string(),
        inferred: true,
    })
}

fn round_px(v: f64) -> u64 {
    let r = v.round();
    if r >= 1.0 {
        r as u64
    } else {
        1
    }
}

fn trim(v: f64) -> String {
    if (v - v.round()).abs() < 1e-9 {
        format!("{}", v.round() as i64)
    } else {
        format!("{v:.2}")
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_values() {
        assert_eq!(parse_clock("3").unwrap(), 3.0);
        assert_eq!(parse_clock("3s").unwrap(), 3.0);
        assert_eq!(parse_clock("250ms").unwrap(), 0.25);
        assert_eq!(parse_clock("2h").unwrap(), 7200.0);
        assert_eq!(parse_clock("45min").unwrap(), 2700.0);
        assert_eq!(parse_clock("01:30").unwrap(), 90.0);
        assert_eq!(parse_clock("0:01:30.25").unwrap(), 90.25);
        assert_eq!(parse_clock("-1.5s").unwrap(), -1.5);
        assert_eq!(parse_clock(".5").unwrap(), 0.5);
        assert!(parse_clock("nonsense").is_err());
    }

    #[test]
    fn begin_tokens() {
        let toks = parse_instance_list(Some("0s; 2s"));
        assert_eq!(toks, vec![BeginToken::Offset(0.0), BeginToken::Offset(2.0)]);
        let toks = parse_instance_list(Some("click"));
        assert!(matches!(toks[0], BeginToken::Unknown(_)));
        let toks = parse_instance_list(Some("id.end+1s"));
        assert!(matches!(
            toks[0],
            BeginToken::Syncbase { ref id, on_end: true, offset } if id == "id" && (offset - 1.0).abs() < 1e-9
        ));
        assert_eq!(parse_instance_list(None).len(), 0);
        // Fractional clock values must stay clock values, not syncbases.
        let toks = parse_instance_list(Some("8.05s"));
        assert_eq!(toks, vec![BeginToken::Offset(8.05)]);
        let toks = parse_instance_list(Some("4.2s"));
        assert_eq!(toks, vec![BeginToken::Offset(4.2)]);
        let toks = parse_instance_list(Some("0.5"));
        assert_eq!(toks, vec![BeginToken::Offset(0.5)]);
    }

    fn el(begin: Option<&str>, dur: Option<&str>, rc: Option<&str>) -> SmilElement {
        SmilElement {
            tag: "animate".into(),
            id: None,
            target: None,
            begin_raw: begin.map(|s| s.to_string()),
            dur_raw: dur.map(|s| s.to_string()),
            end_raw: None,
            repeat_count_raw: rc.map(|s| s.to_string()),
            repeat_dur_raw: None,
            fill: None,
        }
    }

    #[test]
    fn finite_and_loops() {
        let els = vec![
            el(Some("0.5s"), Some("2s"), None),
            el(Some("1s"), Some("1.5s"), None),
        ];
        let plans = plan_smil(&els);
        let mut doc = DocPlan::default();
        for (i, p) in &plans {
            doc.add(p, &format!("el{i}"));
        }
        assert_eq!(doc.finite_ends, vec![2.5, 2.5]);
        assert_eq!(doc.suggested_duration(), Some(2.5));

        let els = vec![
            el(Some("0s"), Some("1s"), Some("indefinite")),
            el(Some("0s"), Some("3s"), None),
        ];
        let plans = plan_smil(&els);
        let mut doc = DocPlan::default();
        for (i, p) in &plans {
            doc.add(p, &format!("el{i}"));
        }
        // loops empty? no: one loop; suggestion = max(3, 0) + 1 = 4
        assert_eq!(doc.loops, vec![(0.0, 1.0)]);
        assert_eq!(doc.suggested_duration(), Some(4.0));
    }

    #[test]
    fn unknown_from_events() {
        let els = vec![el(Some("click"), Some("2s"), None)];
        let plans = plan_smil(&els);
        assert!(matches!(plans[0].1, ElementPlan::Unknown(_)));
    }

    #[test]
    fn syncbase_resolution() {
        let mut a = el(Some("2s"), Some("3s"), None);
        a.id = Some("a".into());
        let b = el(Some("a.end+1s"), Some("2s"), None);
        let plans = plan_smil(&[a, b]);
        assert_eq!(plans[0].1, ElementPlan::Finite(5.0));
        assert_eq!(plans[1].1, ElementPlan::Finite(8.0));
    }

    #[test]
    fn resolution_rules() {
        // Spec example: width=540, viewBox 1080x1920 -> 540x960
        let d = resolve_dims(
            &Some("540".into()),
            &None,
            &Some("0 0 1080 1920".into()),
            (Some(1920.0), Some(1080.0)),
            (1920.0, 1080.0),
        )
        .unwrap();
        assert_eq!((d.width, d.height), (540, 960));
        assert!(!d.inferred);

        // viewBox only 9:16 -> 720x1280 inferred
        let d = resolve_dims(
            &None,
            &None,
            &Some("0 0 9 16".into()),
            (Some(1920.0), Some(1080.0)),
            (1920.0, 1080.0),
        )
        .unwrap();
        assert_eq!((d.width, d.height), (720, 1280));
        assert!(d.inferred);

        // explicit both
        let d = resolve_dims(
            &Some("400px".into()),
            &Some("300".into()),
            &Some("0 0 400 300".into()),
            (Some(1920.0), Some(1080.0)),
            (1920.0, 1080.0),
        )
        .unwrap();
        assert_eq!((d.width, d.height), (400, 300));

        // nothing at all -> 300x150
        let d = resolve_dims(&None, &None, &None, (Some(1920.0), Some(1080.0)), (1920.0, 1080.0))
            .unwrap();
        assert_eq!((d.width, d.height), (300, 150));
        assert!(d.inferred);
    }

    #[test]
    fn frames_math() {
        assert_eq!(frames_for(2.5), 150);
        assert_eq!(frames_for(0.001), 1);
        assert_eq!(frames_for(30.0), 1800);
    }
}
