//! Command line interface: exactly five flags, no subcommands.
//!
//! `-i/--input`    path to the input SVG file (always required)
//! `-o/--output`   path to the output MP4 (required unless `--analyze`)
//! `-a/--analyze`  print the analysis report and exit without rendering
//! `--duration`    override the auto-detected duration, in seconds
//! `-h/--help`     usage

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "kimo",
    about = "Convert any animated SVG into a pixel-exact MP4, rendered by real Chromium at a locked 60 fps.",
    long_about = "Kimo renders SVG animations with a real Chromium browser over the Chrome DevTools \
Protocol and streams PNG frames straight into FFmpeg over a pipe (no frame files ever touch disk).\n\
\n\
Supported animation engines: SMIL, CSS animations/transitions, the Web Animations API (WAAPI), \
and JavaScript driven animation (requestAnimationFrame / timers / canvas / WebGL) via deterministic \
virtual-time stepping. Filters, gradients, masks, clip paths, <use>, foreignObject and embedded \
fonts all render exactly as Chrome draws them, because Chrome is the renderer.",
    after_help = "Examples:\n  kimo -i scene.svg -o scene.mp4\n  kimo -i scene.svg --analyze\n  kimo -i scene.svg -o scene.mp4 --duration 12.5"
)]
pub struct Cli {
    /// Path to the input SVG file.
    #[arg(short, long, value_name = "FILE")]
    pub input: PathBuf,

    /// Path to the output MP4 file (required unless --analyze).
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Print a full analysis (resolution, animation inventory, timeline and duration detection) and exit without rendering.
    #[arg(short, long)]
    pub analyze: bool,

    /// Force the video duration in seconds (overrides auto-detection).
    #[arg(long, value_name = "SECONDS")]
    pub duration: Option<f64>,
}

impl Cli {
    /// Validate cross-flag rules and the duration value.
    pub fn validate(&self) -> Result<(), String> {
        if !self.input.exists() {
            return Err(format!(
                "input file does not exist: {}",
                self.input.display()
            ));
        }
        if !self.input.is_file() {
            return Err(format!(
                "input path is not a file: {}",
                self.input.display()
            ));
        }
        if let Some(d) = self.duration {
            if !d.is_finite() || d <= 0.0 {
                return Err(format!("invalid --duration value: {d} (must be a positive, finite number of seconds)"));
            }
            if d > 3600.0 {
                return Err(format!("--duration {d}s exceeds the 3600s safety cap"));
            }
        }
        if !self.analyze && self.output.is_none() {
            return Err(
                "missing required argument: -o/--output <FILE> (or pass --analyze to print the analysis without rendering)"
                    .to_string(),
            );
        }
        if self.analyze && self.output.is_some() {
            // Allowed, but make the precedence explicit so no one is surprised.
            eprintln!(
                "kimo: note: --analyze was given; -o '{}' is ignored for this run",
                self.output.as_ref().unwrap().display()
            );
        }
        if self.analyze && self.duration.is_some() {
            eprintln!("kimo: note: --analyze was given; --duration is only reported, not used for rendering");
        }
        Ok(())
    }
}
