//! Kimo — pixel-exact SVG to MP4 conversion, rendered by real Chromium.
//!
//! Pipeline: flock -> local HTTP server for the SVG and its sibling assets ->
//! Chromium (persistent profile) -> in-browser analysis (dimensions, SMIL/CSS/
//! WAAPI/JS timeline) -> deterministic 60 fps frame engine (seek / virtual
//! time / hybrid / static) -> PNG bytes piped from memory into FFmpeg ->
//! ffprobe verification gate -> atomic rename. No frame files ever hit disk.

mod anim;
mod analyze;
mod browser;
mod cdp;
mod cli;
mod encode;
mod error;
mod render;
mod server;

use anyhow::Result;
use clap::Parser;
use cli::Cli;
use error::KimoError;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(msg) = cli.validate() {
        eprintln!("kimo: error: {msg}");
        std::process::exit(2);
    }
    match run(&cli).await {
        Ok(0) => std::process::exit(0),
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("kimo: error: {e:#}");
            std::process::exit(1);
        }
    }
}

async fn run(cli: &Cli) -> Result<i32> {
    let started = Instant::now();
    let cwd = std::env::current_dir()
        .map_err(|e| KimoError::Usage(format!("cannot determine the working directory: {e}")))?;

    // ---- Exclusive run lock (flock) -------------------------------------
    let lock = browser::RunLock::acquire(&cwd)?;

    // ---- Input -----------------------------------------------------------
    let input_abs = absolutize(&cli.input);
    let svg_bytes = tokio::fs::read(&input_abs)
        .await
        .map_err(|e| KimoError::Usage(format!("cannot read input {}: {e}", input_abs.display())))?;
    if svg_bytes.is_empty() {
        return Err(KimoError::Usage(format!(
            "input file is empty: {}",
            input_abs.display()
        ))
        .into());
    }
    let size_bytes = svg_bytes.len() as u64;

    // ---- Tools -----------------------------------------------------------
    // Fail fast on missing encoders before we ever launch a browser.
    let (ffmpeg, ffprobe) = if cli.analyze {
        (None, None)
    } else {
        let ffmpeg = browser::find_tool("ffmpeg")?;
        let ffprobe = browser::find_tool("ffprobe")?;
        browser::verify_libx264(&ffmpeg)?;
        (Some(ffmpeg), Some(ffprobe))
    };

    // ---- Asset server + browser -------------------------------------------
    let server = server::AssetServer::start(&input_abs, svg_bytes).await?;
    let browser_exe = browser::find_browser()?;
    let mut browser = browser::Browser::launch(&browser_exe, &cwd).await?;
    eprintln!("kimo: chromium: {}", browser.product);
    eprintln!("kimo: profile: {} (persistent, reused across runs)", browser::PROFILE_DIR);

    let outcome = {
        let result = phase(cli, &browser, &server, &input_abs, size_bytes, &ffmpeg, &ffprobe).await;
        // No matter what happened, shut Chromium down cleanly so the profile
        // is flushed; the profile always survives.
        browser.shutdown().await;
        server.shutdown();
        drop(lock);
        result
    };

    match outcome? {
        PhaseOutcome::Analyzed => Ok(0),
        PhaseOutcome::Rendered {
            out,
            frames,
            width,
            height,
            duration,
        } => {
            let meta = tokio::fs::metadata(&out).await?;
            eprintln!(
                "kimo: done — {} ({}x{}, {} frames at 60fps, {:.3}s, h264 crf19 yuv420p, {:.2} MB) in {:.1}s",
                out.display(),
                width,
                height,
                frames,
                duration,
                meta.len() as f64 / 1e6,
                started.elapsed().as_secs_f64()
            );
            eprintln!(
                "kimo: verified with ffprobe: codec h264, dimensions {}x{}, frame rate 60/1, duration {:.3}s",
                width, height, frames as f64 / 60.0
            );
            eprintln!(
                "kimo: temp files this run: 0 PNG frames written to disk (streamed through memory into ffmpeg's stdin); staged output .part was renamed after verification"
            );
            let profile = cwd.join(browser::PROFILE_DIR);
            let entries: Vec<String> = std::fs::read_dir(&profile)
                .map(|rd| {
                    rd.flatten()
                        .take(24)
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect()
                })
                .unwrap_or_default();
            eprintln!(
                "kimo: persistent files (never deleted): {} [{}] and {}",
                profile.display(),
                entries.join(", "),
                browser::LOCK_FILE
            );
            Ok(0)
        }
    }
}

enum PhaseOutcome {
    Analyzed,
    Rendered {
        out: PathBuf,
        frames: u64,
        width: u64,
        height: u64,
        duration: f64,
    },
}

async fn phase(
    cli: &Cli,
    browser: &browser::Browser,
    server: &server::AssetServer,
    input_abs: &Path,
    size_bytes: u64,
    ffmpeg: &Option<PathBuf>,
    ffprobe: &Option<PathBuf>,
) -> Result<PhaseOutcome> {
    // ---- Load the page and probe it --------------------------------------
    let (_target_id, session) = browser::new_page(&browser.cdp).await?;
    let (page, probe) = render::Page::setup(browser.cdp.clone(), session, server.base_url.clone()).await?;

    let analysis = analyze::analyze_probe(&probe, cli.duration)?;

    if cli.analyze {
        analyze::print_report(
            &analysis,
            &input_abs.display().to_string(),
            size_bytes,
            &browser.product,
            browser::PROFILE_DIR,
        );
        let (requests, missed) = server.stats();
        if !missed.is_empty() {
            eprintln!(
                "kimo: warning: {} of {} asset requests were not found locally: {}",
                missed.len(),
                requests,
                missed.join(", ")
            );
        }
        return Ok(PhaseOutcome::Analyzed);
    }

    // ---- Rendering path -----------------------------------------------------
    let out_abs = absolutize(cli.output.as_ref().expect("validated"));
    if let Some(parent) = out_abs.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Err(KimoError::Usage(format!(
                "output directory does not exist: {}",
                parent.display()
            ))
            .into());
        }
    }

    // Apply the real output viewport now that dimensions are known.
    page.apply_viewport(analysis.dims.width, analysis.dims.height).await?;

    let duration = analysis.duration.ok_or_else(|| {
        KimoError::Usage(format!(
            "duration cannot be auto-detected — {}. Pass --duration SECONDS to record a fixed length \
(example: kimo -i {} -o out.mp4 --duration 10)",
            analysis.duration_source,
            input_abs.display()
        ))
    })?;
    let frames = anim::frames_for(duration);

    eprintln!(
        "kimo: resolution: {}x{} ({})",
        analysis.dims.width, analysis.dims.height, analysis.dims.source
    );
    eprintln!("kimo: mode: {}", analysis.mode.as_str());
    eprintln!(
        "kimo: duration: {:.3}s ({}) -> {} frames at 60fps",
        duration, analysis.duration_source, frames
    );
    for w in &analysis.warnings {
        eprintln!("kimo: warning: {w}");
    }

    // ---- Encoder + frame engine ---------------------------------------------
    let mut encoder = encode::Encoder::spawn(
        ffmpeg.as_ref().expect("checked"),
        &out_abs,
        analysis.dims.width,
        analysis.dims.height,
        frames,
    )
    .await?;

    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(encode::FRAME_BUFFER);
    let cfg = render::RenderConfig::from(&analysis, duration);

    let render_page = page.clone();
    let render_tx = tx.clone();
    let render_task = tokio::spawn(async move {
        render::render_frames(&render_page, &cfg, render_tx).await
    });
    drop(tx); // keep only the task's sender alive

    let pump_result = encoder.pump(rx).await;
    let render_result = render_task
        .await
        .map_err(|e| KimoError::Cdp(format!("frame engine task panicked: {e}")))
        .and_then(|r| r);

    if let Err(e) = render_result {
        encoder.cleanup().await;
        return Err(e.into());
    }
    if let Err(e) = pump_result {
        encoder.cleanup().await;
        return Err(e.into());
    }

    match encoder.verify_and_rename(ffprobe.as_ref().expect("checked")).await {
        Ok(_final) => Ok(PhaseOutcome::Rendered {
            out: out_abs,
            frames,
            width: analysis.dims.width + (analysis.dims.width % 2),
            height: analysis.dims.height + (analysis.dims.height % 2),
            duration,
        }),
        Err(e) => {
            encoder.cleanup().await;
            Err(e.into())
        }
    }
}

fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(p)
    }
}
