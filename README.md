# Kimo

**Pixel-exact SVG → MP4 conversion, rendered by real Chromium.**

Kimo turns *any* animated SVG — SMIL, CSS animations/transitions, Web Animations API,
JavaScript (`requestAnimationFrame` / timers / canvas / WebGL), filters, gradients, masks,
clip paths, `<use>`, `foreignObject`, embedded fonts — into a 60 fps H.264 MP4 that is
byte-for-byte faithful to what Chrome actually draws on screen.

No resvg. No librsvg. No format conversion, no animation re-implementation, no DOM
re-creation. Kimo drives a real Chrome browser over the Chrome DevTools Protocol (CDP),
asks it to paint every frame, and streams the resulting PNG bytes straight into FFmpeg
through a pipe. **No frame ever touches the disk.**

## Why Chromium?

Every other SVG renderer quietly re-implements (a subset of) SVG animation semantics and
gets the details wrong — spline keySplines, additive transforms, animated filter
parameters, `feTurbulence` seeds, discrete calcMode, syncbase timing, font rasterization.
Chrome is the reference implementation of the web platform. If Chrome can render your SVG,
Kimo can film it — exactly.

## Install

- **Linux binary** — grab `kimo-linux-amd64.tar` from the [Releases](../../releases) page (built by CI), or from the build artifacts of any CI run.
- **From source** — Rust 1.75+:

  ```sh
  cargo build --release
  ```

Requirements at runtime:

| Requirement | How Kimo finds it |
|---|---|
| Chrome for Testing (or system Chrome/Chromium ≥ 111) | `KIMO_CHROME_PATH` env var, else `PATH` (`chrome`, `chromium`, …) |
| FFmpeg + ffprobe with libx264 | `PATH` |

Kimo never downloads anything by itself; it reuses whatever Chromium you give it.

## Usage

```
kimo [OPTIONS] --input <FILE>

  -i, --input <FILE>       input SVG file
  -o, --output <FILE>      output MP4 (required unless --analyze)
  -a, --analyze            print analysis (resolution, timeline, duration) and exit
      --duration <SECONDS> force the video duration
  -h, --help               help
```

Examples:

```sh
kimo -i scene.svg -o scene.mp4             # render with auto-detected duration
kimo -i scene.svg --analyze                # inspect resolution + timeline first
kimo -i scene.svg -o scene.mp4 --duration 12
```

## How it works

1. **Local asset server** — a minimal HTTP server serves the SVG as `image/svg+xml` and
   any sibling files it references (images, fonts, scripts), so relative references and
   `<script>` execution work exactly like they would on the web.
2. **Instrumentation** — before any page script runs, Kimo injects a small shim that
   gates `requestAnimationFrame` and counts timing-API usage, so JS animations start from
   their true zero state and JS-driven documents can be detected honestly.
3. **Analysis** — resolution comes from the live DOM (explicit `width`/`height` in px
   wins; one dimension + `viewBox` derives the other; `viewBox`-only is rendered at the
   viewBox aspect with the longest side at 1280 px — never rescaled away from the
   author's intent). The animation inventory is read from the browser itself:
   `getComputedTiming()` for CSS/WAAPI and live SMIL attributes, with proper SMIL timing
   math (clock values, syncbase fixpoint, repeat cycles) computed in Rust.
4. **Deterministic frame engine** — every frame is *seeked*, never sampled by wall clock:
   - SMIL: `setCurrentTime(t)` on the SVG root.
   - CSS / WAAPI: every `Animation` is paused at capture start and its `currentTime` is
     set to `startOffset + t`.
   - JS (rAF/timers): `Emulation.setVirtualTimePolicy` advances a deterministic virtual
     clock by exactly `1/60 s` per frame — timers and rAF fire in the right order at the
     right times regardless of how slow the machine is.
   - `t = frame_index / 60`, computed fresh every frame — no accumulated floating point drift.
5. **Encoding** — PNG bytes from `Page.captureScreenshot` are pushed through a bounded
   in-memory channel (backpressure!) into FFmpeg's stdin (`image2pipe`), encoded as
   libx264, CRF 19, `preset medium`, `yuv420p`, `+faststart`, exactly 60 fps.
6. **Verification gate** — the MP4 is written to `<output>.part` and only renamed after
   ffprobe confirms it exists, is non-empty, has the exact expected width × height,
   `60/1` fps, H.264 codec and `frames/60` duration. A failed verification deletes the
   part file; the output path is never left in a broken state.

## Duration rules

| Situation | Auto duration |
|---|---|
| Finite animations only | latest finite end |
| Infinite loops mixed in | `max(latest finite end, latest loop start) + longest cycle` |
| Pure loops (no finite end) | latest loop start + longest cycle |
| Static document | 1.0 s |
| JS-driven / event-based / unresolved timing | requires explicit `--duration` (kimo refuses to guess) |

`--duration` always wins. Kimo reports infinite loops honestly instead of pretending to
know an end that doesn't exist.

## Files Kimo creates

| Path | Lifetime |
|---|---|
| `<cwd>/.kimo/chromium-profile/` | **persistent** — reused forever, speeds up every run, never deleted |
| `<cwd>/.kimo/kimo.lock` | per-run flock; prevents two Kimos from sharing one profile |
| `<output>.part` | minutes — renamed to the output on ffprobe success, deleted on failure |
| PNG frames | **never written anywhere** — memory → pipe only |

## Development

```sh
cargo build
bash scripts/smoke.sh target/debug/kimo   # full pipeline test against tests/fixtures
```

The test corpus in `tests/fixtures/` covers: static SVGs, finite SMIL, infinite SMIL
loops, CSS animations, JS/rAF animation, canvas/foreignObject, event-based timing
(must refuse without `--duration`), `viewBox`-only resolution inference, width-derivation
(`width=540` + `viewBox 1080×1920` → 540×960), and a real-world stress fixture
(`laziness.svg` — 23 SMIL elements, animated `feTurbulence`/`feDisplacementMap`,
additive transforms, spline and discrete keyTimes, animated clip paths).

CI (Linux) builds the release binary, runs the smoke suite with Chrome for Testing +
FFmpeg, and publishes `kimo-linux-amd64.tar` as an artifact — tags `v*` create a Release.

## License

MIT — see [LICENSE](LICENSE).
