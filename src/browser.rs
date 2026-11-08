//! Chromium process management: binary discovery, launch with the persistent
//! profile, process-group cleanup, and the run lock (flock).

use crate::cdp::Cdp;
use crate::error::{KimoError, Result};
use fs2::FileExt;
use serde_json::json;
use std::collections::VecDeque;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The persistent Chromium user-data directory, relative to the cwd.
pub const PROFILE_DIR: &str = ".kimo/chromium-profile";
/// The advisory lock guarding concurrent runs in the same directory.
pub const LOCK_FILE: &str = ".kimo/kimo.lock";

/// Holds the flock for the lifetime of the run.
pub struct RunLock {
    _file: std::fs::File,
    path: PathBuf,
}

impl RunLock {
    pub fn acquire(base: &Path) -> Result<Self> {
        let dir = base.join(".kimo");
        std::fs::create_dir_all(&dir)
            .map_err(|e| KimoError::Io(std::io::Error::other(format!(
                "cannot create {} (is the working directory writable?): {e}",
                dir.display()
            ))))?;
        let path = dir.join("kimo.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .truncate(false)
            .open(&path)?;
        file.try_lock_exclusive().map_err(|_| {
            KimoError::Usage(format!(
                "another kimo run is already active in this directory (lock held: {})",
                path.display()
            ))
        })?;
        Ok(RunLock { _file: file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Find a Chromium/Chrome binary: $KIMO_CHROME_PATH first, then PATH search,
/// then well-known locations.
pub fn find_browser() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("KIMO_CHROME_PATH") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Ok(path);
        }
        return Err(KimoError::Browser(format!(
            "KIMO_CHROME_PATH points to {}, which is not a file",
            path.display()
        )));
    }

    let candidates = [
        "chrome",
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "chrome-headless-shell",
        "headless-shell",
    ];
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            if dir.is_empty() {
                continue;
            }
            for cand in candidates {
                let p = PathBuf::from(dir).join(cand);
                if p.is_file() {
                    return Ok(p);
                }
            }
        }
    }
    let known = [
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/opt/google/chrome/chrome",
        "/snap/bin/chromium",
    ];
    for k in known {
        let p = PathBuf::from(k);
        if p.is_file() {
            return Ok(p);
        }
    }

    Err(KimoError::Browser(
        "no Chromium/Chrome binary found. Install Chrome for Testing \
(https://googlechromelabs.github.io/chrome-for-testing/) or a system Chrome, \
then either put it on PATH or set KIMO_CHROME_PATH=/path/to/chrome"
            .into(),
    ))
}

pub struct Browser {
    pub child: tokio::process::Child,
    pub cdp: Arc<Cdp>,
    pub stderr_tail: Arc<Mutex<VecDeque<String>>>,
    pub product: String,
}

impl Browser {
    /// Launch Chromium with the persistent profile and connect to DevTools.
    pub async fn launch(exe: &Path, cwd: &Path) -> Result<Self> {
        let profile = cwd.join(PROFILE_DIR);
        std::fs::create_dir_all(&profile).map_err(|e| {
            KimoError::Browser(format!(
                "cannot create the persistent Chromium profile at {}: {e}",
                profile.display()
            ))
        })?;

        let mut cmd = std::process::Command::new(exe);
        cmd.env_remove("KIMO_CHROME_PATH"); // never forward our own knob
        let args = [
            "--headless=new".to_string(),
            "--no-sandbox".to_string(),
            "--disable-gpu".to_string(),
            "--enable-unsafe-swiftshader".to_string(),
            "--disable-dev-shm-usage".to_string(),
            "--hide-scrollbars".to_string(),
            "--force-color-profile=srgb".to_string(),
            "--force-device-scale-factor=1".to_string(),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
            "--disable-extensions".to_string(),
            "--disable-background-networking".to_string(),
            "--disable-component-update".to_string(),
            "--disable-sync".to_string(),
            "--disable-crash-reporter".to_string(),
            "--disable-default-apps".to_string(),
            "--disable-client-side-phishing-detection".to_string(),
            "--mute-audio".to_string(),
            "--autoplay-policy=no-user-gesture-required".to_string(),
            "--metrics-recording-local-only".to_string(),
            format!("--user-data-dir={}", profile.display()),
            "--remote-debugging-port=0".to_string(),
            "about:blank".to_string(),
        ];
        for a in args {
            cmd.arg(a);
        }
        cmd.stdout(Stdio::null())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        // Own process group so we can clean up every Chromium child at once.
        cmd.process_group(0);

        let mut child = tokio::process::Command::from(cmd)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| KimoError::Browser(format!("failed to launch {}: {e}", exe.display())))?;

        let pid = child.id().unwrap_or(0);
        let mut stderr = child.stderr.take()
            .ok_or_else(|| KimoError::Browser("failed to capture Chromium stderr".into()))?;

        let tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        let (ws_tx, ws_rx) = tokio::sync::oneshot::channel::<String>();
        let mut ws_tx = Some(ws_tx);
        let tail_clone = tail.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if let Some(idx) = line.find("DevTools listening on ") {
                    let url = line[idx + "DevTools listening on ".len()..].trim().to_string();
                    ws_tx.take().map(|t| {
                        let _ = t.send(url);
                    });
                }
                let mut t = tail_clone.lock().unwrap();
                t.push_back(line);
                while t.len() > 40 {
                    t.pop_front();
                }
            }
        });

        let ws_url = match tokio::time::timeout(Duration::from_secs(45), ws_rx).await {
            Ok(Ok(url)) if url.starts_with("ws://") => url,
            Ok(Ok(_)) => {
                kill_process_group(pid);
                return Err(KimoError::Browser(
                    "Chromium did not report a DevTools WebSocket endpoint".into(),
                ));
            }
            Ok(Err(_)) | Err(_) => {
                kill_process_group(pid);
                let log = tail.lock().unwrap().make_string("\n");
                return Err(KimoError::Browser(format!(
                    "Chromium exited before exposing DevTools (crashed at startup). stderr tail:\n{log}"
                )));
            }
        };

        let cdp = Cdp::connect(&ws_url).await?;

        let product = match cdp
            .call(None, "Browser.getVersion", json!({}), Duration::from_secs(15))
            .await
        {
            Ok(v) => v
                .get("product")
                .and_then(|p| p.as_str())
                .unwrap_or("unknown")
                .to_string(),
            Err(_) => "unknown".to_string(),
        };

        Ok(Browser {
            child,
            cdp,
            stderr_tail: tail,
            product,
        })
    }

    /// True if the Chromium process has already exited.
    pub async fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// Graceful shutdown: Browser.close first, then SIGTERM, then SIGKILL to
    /// the whole process group. The profile always survives on disk.
    pub async fn shutdown(&mut self) {
        let _ = self
            .cdp
            .call(None, "Browser.close", json!({}), Duration::from_secs(5))
            .await;
        for wait_s in [5u64, 5] {
            match tokio::time::timeout(Duration::from_secs(wait_s), self.child.wait()).await {
                Ok(_) => return,
                Err(_) => {}
            }
        }
        let pid = self.child.id().unwrap_or(0);
        let _ = self.child.start_kill();
        kill_process_group(pid);
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
    }

    /// Last lines of Chromium stderr, for honest crash reporting.
    pub fn stderr_string(&self) -> String {
        self.stderr_tail.lock().unwrap().make_string("\n")
    }
}

trait Join {
    fn make_string(&self, sep: &str) -> String;
}

impl Join for VecDeque<String> {
    fn make_string(&self, sep: &str) -> String {
        self.iter().cloned().collect::<Vec<_>>().join(sep)
    }
}

fn kill_process_group(pid: u32) {
    if pid == 0 {
        return;
    }
    // negative pid = whole process group (we are the group leader)
    let _ = std::process::Command::new("kill")
        .arg("-9")
        .arg(format!("-{pid}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Create the page target and return (targetId, sessionId).
pub async fn new_page(cdp: &Arc<Cdp>) -> Result<(String, String)> {
    let target = cdp
        .cmd(None, "Target.createTarget", json!({ "url": "about:blank" }))
        .await?;
    let target_id = target
        .get("targetId")
        .and_then(|t| t.as_str())
        .ok_or_else(|| KimoError::Cdp("Target.createTarget returned no targetId".into()))?
        .to_string();

    let attach = cdp
        .cmd(
            None,
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        )
        .await?;
    let session_id = attach
        .get("sessionId")
        .and_then(|s| s.as_str())
        .ok_or_else(|| KimoError::Cdp("Target.attachToTarget returned no sessionId".into()))?
        .to_string();

    Ok((target_id, session_id))
}

/// Check that ffmpeg and ffprobe are available and that libx264 exists.
pub fn find_tool(name: &str) -> Result<PathBuf> {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            if dir.is_empty() {
                continue;
            }
            let p = PathBuf::from(dir).join(name);
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    Err(KimoError::Encode(format!(
        "{name} not found on PATH — install FFmpeg (with libx264) and retry"
    )))
}

pub fn verify_libx264(ffmpeg: &Path) -> Result<()> {
    let out = std::process::Command::new(ffmpeg)
        .arg("-hide_banner")
        .arg("-encoders")
        .output()
        .map_err(|e| KimoError::Encode(format!("failed to run {}: {e}", ffmpeg.display())))?;
    let text = String::from_utf8_lossy(&out.stdout);
    if text.contains("libx264") {
        Ok(())
    } else {
        Err(KimoError::Encode(
            "this FFmpeg build has no libx264 encoder — install FFmpeg with libx264 support".into(),
        ))
    }
}
