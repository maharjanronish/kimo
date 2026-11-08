//! The deterministic 60 fps frame engine.
//!
//! Every frame index k maps to exactly t = k/60 (recomputed fresh each frame,
//! never accumulated). Three strategies, chosen by the analyzer:
//!
//! * **seek** — no JS timing usage. SMIL is pinned with
//!   `pauseAnimations()` + `setCurrentTime(t)`; every CSS/WAAPI animation is
//!   frozen at its zero state and re-seeked with `currentTime`. Exact.
//! * **virtual** — JS-only animation. Chromium virtual time is stepped in
//!   1/60s budgets (`Emulation.setVirtualTimePolicy`), which drives rAF,
//!   timers, Date.now and performance.now deterministically.
//! * **hybrid** — JS animation plus CSS/WAAPI/SMIL: virtual time for JS with
//!   pinned-and-seeked CSS/SMIL aligned to the same instant each frame.
//!
//! The engine also gates requestAnimationFrame from document creation until
//! frame 0 is captured, so JS animations start from their true zero state.

use crate::analyze::{eval_js, eval_js_promise, Analysis, Mode, INSTRUMENT_JS, PROBE_JS};
use crate::cdp::Cdp;
use crate::error::{KimoError, Result};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const STEP_TIMEOUT: Duration = Duration::from_secs(300);
const SETTLE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct Page {
    pub cdp: Arc<Cdp>,
    pub session: String,
    pub url: String,
}

impl Page {
    /// Full page bring-up: target, domains, instrumentation, navigation,
    /// settle (load + fonts + network idle), and probing.
    pub async fn setup(cdp: Arc<Cdp>, session: String, url: String) -> Result<(Self, crate::analyze::ProbeData)> {
        // Enable the domains we need.
        for method in ["Page.enable", "Runtime.enable", "Network.enable", "Log.enable"] {
            cdp.cmd(Some(&session), method, json!({})).await?;
        }

        // Instrumentation before any page script runs.
        cdp.cmd(
            Some(&session),
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": INSTRUMENT_JS }),
        )
        .await?;

        // Probe viewport so percentage sizes resolve deterministically during
        // analysis; the real output size is applied right after.
        cdp.cmd(
            Some(&session),
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": 1920,
                "height": 1080,
                "deviceScaleFactor": 1,
                "mobile": false,
            }),
        )
        .await?;

        // Navigate.
        let mut events = cdp.events();
        cdp.cmd(Some(&session), "Page.navigate", json!({ "url": url })).await?;

        // Wait for load (bounded; a hanging external reference must not wedge us).
        let loaded = match cdp
            .wait_event(&mut events, "Page.loadEventFired", Some(&session), Duration::from_secs(20))
            .await
        {
            Ok(_) => true,
            Err(_) => false,
        };
        if !loaded {
            let state = eval_js(&cdp, &session, "document.readyState").await.unwrap_or_default();
            if !state.contains("complete") {
                eprintln!(
                    "kimo: warning: page load event did not fire within 20s (readyState {state}); continuing with what Chrome has"
                );
            }
        }

        // Settle: fonts + network idle. Bounded, honest warnings on timeout.
        Self::wait_settle(&cdp, &session, &mut events).await;

        // Probe.
        let probe_raw = eval_js(&cdp, &session, PROBE_JS).await?;
        let probe = crate::analyze::ProbeData::parse(&probe_raw)?;

        Ok((Page { cdp, session, url }, probe))
    }

    /// Apply the final output viewport (forces relayout) and let the page
    /// repaint once.
    pub async fn apply_viewport(&self, width: u64, height: u64) -> Result<()> {
        self.cdp
            .cmd(
                Some(&self.session),
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": width,
                    "height": height,
                    "deviceScaleFactor": 1,
                    "mobile": false,
                }),
            )
            .await?;
        let _ = eval_js_promise(
            &self.cdp,
            &self.session,
            "new Promise(function(r){ setTimeout(r, 0); })",
        )
        .await;
        Ok(())
    }

    /// SMIL pin + seek. Absent SMIL makes this a no-op.
    async fn smil_seek(&self, t_seconds: f64) -> Result<()> {
        let expr = format!(
            "(function(){{ var s = document.documentElement; \
             if (s && s.setCurrentTime) {{ try {{ if (!window.__kimoSmilPaused) {{ s.pauseAnimations(); \
             window.__kimoSmilPaused = true; }} s.setCurrentTime({t}); }} catch(e) {{}} }} \
             return 'ok'; }})()",
            t = t_seconds
        );
        let _ = eval_js(&self.cdp, &self.session, &expr).await;
        Ok(())
    }

    /// Freeze CSS/WAAPI animations at their zero state (idempotent).
    async fn freeze_css(&self) -> Result<()> {
        let _ = eval_js(&self.cdp, &self.session, "window.__kimoFreeze && window.__kimoFreeze()").await;
        Ok(())
    }

    /// Seek frozen CSS/WAAPI animations to video time t (milliseconds).
    async fn seek_css(&self, t_ms: f64) -> Result<()> {
        let expr = format!(
            "(function(){{ window.__kimoSeek && window.__kimoSeek({t}); return 'ok'; }})()",
            t = t_ms
        );
        let _ = eval_js(&self.cdp, &self.session, &expr).await;
        Ok(())
    }

    /// Release the rAF gate.
    async fn release_raf(&self) -> Result<()> {
        let _ = eval_js(&self.cdp, &self.session, "window.__kimoReleaseRaf && window.__kimoReleaseRaf()").await;
        Ok(())
    }

    /// Wait for two animation frames so seeks have repainted. Only valid when
    /// virtual time is NOT active (the rAF clock would be frozen otherwise).
    async fn wait_paint(&self) -> Result<()> {
        let _ = eval_js_promise(
            &self.cdp,
            &self.session,
            "new Promise(function(r){ requestAnimationFrame(function(){ requestAnimationFrame(r); }); })",
        )
        .await;
        Ok(())
    }

    /// Capture the viewport as PNG bytes.
    pub async fn capture(&self) -> Result<Vec<u8>> {
        use base64::Engine;
        let res = self
            .cdp
            .call(
                Some(&self.session),
                "Page.captureScreenshot",
                json!({ "format": "png", "fromSurface": true, "captureBeyondViewport": false }),
                Duration::from_secs(180),
            )
            .await?;
        let b64 = res
            .get("data")
            .and_then(|d| d.as_str())
            .ok_or_else(|| KimoError::Cdp("captureScreenshot returned no data".into()))?;
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| KimoError::Cdp(format!("failed to decode screenshot PNG: {e}")))
    }

    /// Advance virtual time to exactly `target_ms` since the virtual origin
    /// and wait for the budget to be consumed.
    async fn virtual_step(&self, events: &mut tokio::sync::broadcast::Receiver<Value>, current_ms: &mut f64, target_ms: f64) -> Result<()> {
        let budget = target_ms - *current_ms;
        if budget <= 0.0 {
            return Ok(());
        }
        self.cdp
            .call(
                Some(&self.session),
                "Emulation.setVirtualTimePolicy",
                json!({
                    "policy": "pauseIfNetworkFetchesPending",
                    "budget": budget,
                }),
                Duration::from_secs(60),
            )
            .await?;
        self.cdp
            .wait_event(events, "Emulation.virtualTimeBudgetExpired", Some(&self.session), STEP_TIMEOUT)
            .await?;
        *current_ms = target_ms;
        Ok(())
    }

    async fn wait_settle(cdp: &Arc<Cdp>, session: &str, events: &mut tokio::sync::broadcast::Receiver<Value>) {
        // Track network in-flight via events while waiting.
        let start = Instant::now();
        let mut quiet_since: Option<Instant> = None;
        let mut inflight: i64 = 0;
        let fonts_ready = loop {
            if start.elapsed() > SETTLE_TIMEOUT {
                eprintln!("kimo: warning: settle timeout after 20s; continuing with what Chrome has");
                break false;
            }
            // Drain pending network events (non-blocking-ish via try_recv loop).
            loop {
                match events.try_recv() {
                    Ok(v) => {
                        let m = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
                        let sid = v.get("sessionId").and_then(|s| s.as_str());
                        if sid != Some(session) {
                            continue;
                        }
                        match m {
                            "Network.requestWillBeSent" => inflight += 1,
                            "Network.loadingFinished" | "Network.loadingFailed" => inflight -= 1,
                            _ => {}
                        }
                    }
                    Err(_) => break,
                }
            }
            let fonts: String = eval_js(cdp, session, "(document.fonts ? document.fonts.status : 'loaded')")
                .await
                .unwrap_or_else(|_| "loaded".into());
            let fonts_ok = fonts.contains("loaded");
            let idle = inflight <= 0;
            if fonts_ok && idle {
                match quiet_since {
                    None => quiet_since = Some(Instant::now()),
                    Some(q) if q.elapsed() >= Duration::from_millis(300) => break true,
                    Some(_) => {}
                }
            } else {
                quiet_since = None;
            }
            tokio::time::sleep(Duration::from_millis(60)).await;
        };
        let _ = fonts_ready;
    }
}

#[derive(Clone)]
pub struct RenderConfig {
    pub frames: u64,
    pub mode: Mode,
    pub has_smil: bool,
    pub has_csswaa: bool,
}

impl RenderConfig {
    pub fn from(analysis: &Analysis, duration: f64) -> Self {
        RenderConfig {
            frames: crate::anim::frames_for(duration),
            mode: analysis.mode,
            has_smil: analysis.smil_count > 0,
            has_csswaa: analysis.css_count + analysis.waapi_count > 0,
        }
    }
}

/// Run the frame loop, streaming PNG bytes into `tx`.
pub async fn render_frames(
    page: &Page,
    cfg: &RenderConfig,
    tx: mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let start = Instant::now();
    let frames = cfg.frames.max(1);

    // Pin SMIL at document zero and freeze CSS/WAAPI at their zero states.
    if cfg.mode == Mode::Seek || cfg.mode == Mode::Hybrid {
        page.smil_seek(0.0).await?;
        page.freeze_css().await?;
    }

    // Frame 0 at exactly t = 0 — before any rAF callback has ever run.
    let t0_ms = 0.0f64;
    if cfg.mode == Mode::Seek || cfg.mode == Mode::Hybrid {
        page.seek_css(t0_ms).await?;
    }
    let frame0 = page.capture().await?;
    tx.send(frame0.clone())
        .await
        .map_err(|_| KimoError::Encode("encoder stopped accepting frames".into()))?;

    // Static mode: the rest of the video repeats frame 0.
    if cfg.mode == Mode::Static {
        for _ in 1..frames {
            tx.send(frame0.clone())
                .await
                .map_err(|_| KimoError::Encode("encoder stopped accepting frames".into()))?;
        }
        return Ok(());
    }

    // Now that frame 0 is banked, let JS run.
    page.release_raf().await?;

    let mut events = page.cdp.events();
    let mut virtual_ms: f64 = 0.0;

    let report_every: u64 = 60;
    for k in 1..frames {
        // t computed fresh from the integer frame index — never accumulated.
        let t = k as f64 / crate::anim::FPS;
        let t_ms = t * 1000.0;

        match cfg.mode {
            Mode::Seek => {
                page.seek_css(t_ms).await?;
                page.smil_seek(t).await?;
                page.wait_paint().await?;
            }
            Mode::Virtual => {
                page.virtual_step(&mut events, &mut virtual_ms, t_ms).await?;
            }
            Mode::Hybrid => {
                // Pin CSS/SMIL to t first, then step virtual time to t so rAF
                // callbacks observe a consistent instant.
                page.seek_css(t_ms).await?;
                page.smil_seek(t).await?;
                page.virtual_step(&mut events, &mut virtual_ms, t_ms).await?;
            }
            Mode::Static => unreachable!(),
        }

        let png = page.capture().await?;
        tx.send(png)
            .await
            .map_err(|_| KimoError::Encode("encoder stopped accepting frames (ffmpeg likely died — see its error output)".into()))?;

        if k % report_every == 0 || k == frames - 1 {
            let elapsed = start.elapsed().as_secs_f64();
            let fps_actual = k as f64 / elapsed.max(0.001);
            let eta = (frames - k - 1) as f64 / fps_actual.max(0.001);
            eprintln!(
                "kimo: frame {}/{} ({:.0}%) — {:.1} frames/s — eta {:.0}s",
                k + 1,
                frames,
                100.0 * (k + 1) as f64 / frames as f64,
                fps_actual,
                eta
            );
        }
    }
    Ok(())
}

