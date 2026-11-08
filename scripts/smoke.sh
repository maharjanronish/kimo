#!/usr/bin/env bash
# Kimo CI smoke tests: exercise the analyzer and the full render pipeline
# (Chromium -> PNG in memory -> FFmpeg pipe -> MP4 -> ffprobe verification)
# against the bundled fixtures. Exits non-zero on the first failure.
#
# Usage: scripts/smoke.sh /path/to/kimo

set -uo pipefail

KIMO="${1:-target/release/kimo}"
OUT_DIR="smoke-out"
mkdir -p "$OUT_DIR"
FAIL=0

note()  { printf '\n\033[1;34m== %s ==\033[0m\n' "$*"; }
ok()    { printf '\033[1;32mPASS\033[0m %s\n' "$*"; }
fail()  { printf '\033[1;31mFAIL\033[0m %s\n' "$*"; FAIL=1; }

# ffprobe assertions: h264, exact width x height, 60/1 fps, ~expected duration
verify_mp4() {
  local file="$1" w="$2" h="$3" dur="$4" label="$5"
  local stream duration
  stream="$(ffprobe -v error -select_streams v:0 \
    -show_entries stream=codec_name,width,height,r_frame_rate \
    -of csv=p=0 "$file" 2>/dev/null)" || { fail "$label: ffprobe could not read $file"; return; }
  duration="$(ffprobe -v error -show_entries format=duration \
    -of csv=p=0 "$file" 2>/dev/null)" || { fail "$label: ffprobe could not read duration"; return; }
  local codec width height fps
  codec="$(cut -d, -f1 <<<"$stream")"
  width="$(cut -d, -f2 <<<"$stream")"
  height="$(cut -d, -f3 <<<"$stream")"
  fps="$(cut -d, -f4 <<<"$stream")"
  [[ "$codec" == "h264" ]]        || { fail "$label: codec=$codec (want h264)"; return; }
  [[ "$width" == "$w" ]]           || { fail "$label: width=$width (want $w)"; return; }
  [[ "$height" == "$h" ]]          || { fail "$label: height=$height (want $h)"; return; }
  [[ "$fps" == "60/1" ]]           || { fail "$label: fps=$fps (want 60/1)"; return; }
  awk -v d="$duration" -v want="$dur" 'BEGIN { exit !(d > want - 0.05 && d < want + 0.05) }' \
    || { fail "$label: duration=$duration (want ~$dur)"; return; }
  ok "$label: h264 ${width}x${height} ${fps} ${duration}s"
}

analyze() { # fixture label
  note "analyze: $1"
  if "$KIMO" -i "tests/fixtures/$1" --analyze; then ok "analyze $1"; else fail "analyze $1"; fi
}

render() { # fixture outname extra-args...
  local fixture="$1" out="$2"; shift 2
  note "render: $fixture $*"
  if "$KIMO" -i "tests/fixtures/$fixture" -o "$OUT_DIR/$out" "$@"; then
    ok "render $fixture"
  else
    fail "render $fixture"; return 1
  fi
}

# ---- analyzer ---------------------------------------------------------------
analyze static.svg
analyze smil_finite.svg
analyze smil_loop.svg
analyze css_anim.svg
analyze js_raf.svg
analyze viewbox_only.svg
analyze width_derive.svg
analyze event_based.svg
analyze laziness.svg

# ---- full renders + ffprobe verification ------------------------------------
render static.svg        static.mp4         && verify_mp4 "$OUT_DIR/static.mp4"        400 300 1.0  "static"
render smil_finite.svg   smil_finite.mp4    && verify_mp4 "$OUT_DIR/smil_finite.mp4"   320 240 2.5  "smil_finite"
render smil_loop.svg     smil_loop.mp4 --duration 2 && verify_mp4 "$OUT_DIR/smil_loop.mp4" 320 180 2.0 "smil_loop(--duration 2)"
render css_anim.svg      css_anim.mp4       && verify_mp4 "$OUT_DIR/css_anim.mp4"      400 300 5.0  "css_anim"
render viewbox_only.svg  viewbox_only.mp4   && verify_mp4 "$OUT_DIR/viewbox_only.mp4"  720 1280 2.0 "viewbox_only (viewBox-only, longest side 1280)"
render width_derive.svg  width_derive.mp4   && verify_mp4 "$OUT_DIR/width_derive.mp4"  540 960 2.0  "width_derive (width=540 + viewBox 1080x1920 -> 540x960)"
render js_raf.svg        js_raf.mp4 --duration 2 && verify_mp4 "$OUT_DIR/js_raf.mp4"   480 270 2.0  "js_raf(--duration 2)"
render foreign_canvas.svg foreign_canvas.mp4 --duration 2 && verify_mp4 "$OUT_DIR/foreign_canvas.mp4" 640 360 2.0 "foreign_canvas(--duration 2)"
render event_based.svg   event_based.mp4 --duration 1.5 && verify_mp4 "$OUT_DIR/event_based.mp4" 300 200 1.5 "event_based(--duration 1.5)"

# ---- failure-path checks ----------------------------------------------------
note "error handling: missing output"
missing_out_msg="$("$KIMO" -i tests/fixtures/static.svg 2>&1 || true)"
if grep -q -- "-o/--output" <<<"$missing_out_msg"; then
  ok "missing -o exits non-zero with a clear message"
else
  fail "missing -o message is unclear"
fi

note "error handling: nonexistent input"
if "$KIMO" -i tests/fixtures/does_not_exist.svg --analyze >/dev/null 2>&1; then
  fail "nonexistent input must exit non-zero"
else
  ok "nonexistent input exits non-zero"
fi

note "result"
if [[ "$FAIL" -eq 0 ]]; then
  echo "ALL SMOKE TESTS PASSED"
else
  echo "SMOKE TESTS FAILED"
  exit 1
fi
