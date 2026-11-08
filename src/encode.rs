//! Encoding: PNG bytes are streamed from memory straight into FFmpeg's stdin
//! (`image2pipe`), with a bounded channel providing backpressure. No frame
//! ever touches the disk. Output is staged at `<output>.part`, verified with
//! ffprobe, and only then renamed to the final name; on any failure the
//! staged file is deleted and kimo reports an error.

use crate::error::{KimoError, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// Bounded in-flight frame buffer: backpressure without unbounded memory.
pub const FRAME_BUFFER: usize = 8;

pub struct Encoder {
    child: Child,
    stderr_tail: Arc<tokio::sync::Mutex<std::collections::VecDeque<String>>>,
    part_path: PathBuf,
    final_path: PathBuf,
    pub expected_w: u64,
    pub expected_h: u64,
    pub frames: u64,
}

impl Encoder {
    /// Spawn FFmpeg. `pad` pads odd dimensions by 1px for yuv420p.
    pub async fn spawn(
        ffmpeg: &Path,
        final_path: &Path,
        width: u64,
        height: u64,
        frames: u64,
    ) -> Result<Self> {
        let part_path = final_path.with_extension("mp4.part");

        let pad_w = width + (width % 2);
        let pad_h = height + (height % 2);
        let needs_pad = pad_w != width || pad_h != height;

        let mut args: Vec<String> = vec![
            "-y".into(),
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            // PNG byte stream on stdin, exactly 60 frames per second in.
            "-f".into(),
            "image2pipe".into(),
            "-c:v".into(),
            "png".into(),
            "-framerate".into(),
            "60".into(),
            "-i".into(),
            "-".into(),
        ];
        if needs_pad {
            args.push("-vf".into());
            args.push(format!("pad=w={pad_w}:h={pad_h}:x=0:y=0:color=white"));
        }
        args.extend([
            "-an".into(),
            "-c:v".into(),
            "libx264".into(),
            "-preset".into(),
            "medium".into(),
            "-crf".into(),
            "19".into(),
            "-pix_fmt".into(),
            "yuv420p".into(),
            "-movflags".into(),
            "+faststart".into(),
            "-r".into(),
            "60".into(),
            // The staged file ends in .part, so the muxer must be forced.
            "-f".into(),
            "mp4".into(),
            part_path.to_string_lossy().into(),
        ]);

        let mut cmd = Command::new(ffmpeg);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| KimoError::Encode(format!("failed to spawn ffmpeg: {e}")))?;

        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| KimoError::Encode("failed to capture ffmpeg stderr".into()))?;
        let tail = Arc::new(tokio::sync::Mutex::new(std::collections::VecDeque::new()));
        let tail2 = tail.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let mut t = tail2.lock().await;
                t.push_back(line);
                while t.len() > 30 {
                    t.pop_front();
                }
            }
        });

        Ok(Encoder {
            child,
            stderr_tail: tail,
            part_path,
            final_path: final_path.to_path_buf(),
            expected_w: pad_w,
            expected_h: pad_h,
            frames,
        })
    }

    /// Stream all frames from the channel into FFmpeg's stdin, then close it
    /// and wait for FFmpeg to finish. Returns () on a clean encode.
    pub async fn pump(&mut self, mut rx: mpsc::Receiver<Vec<u8>>) -> Result<()> {
        let mut stdin = self
            .child
            .stdin
            .take()
            .ok_or_else(|| KimoError::Encode("ffmpeg stdin unavailable".into()))?;

        let mut sent: u64 = 0;
        while let Some(png) = rx.recv().await {
            if let Err(e) = stdin.write_all(&png).await {
                // FFmpeg died mid-stream. Drain its stderr for the real cause.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                let tail = self.stderr_string().await;
                return Err(KimoError::Encode(format!(
                    "ffmpeg stopped accepting frames after {sent} written ({e}). ffmpeg stderr:\n{tail}"
                )));
            }
            sent += 1;
        }
        drop(stdin); // signal end-of-stream so libx264 finalizes the MP4

        let status = self
            .child
            .wait()
            .await
            .map_err(|e| KimoError::Encode(format!("failed waiting for ffmpeg: {e}")))?;
        if !status.success() {
            let tail = self.stderr_string().await;
            return Err(KimoError::Encode(format!(
                "ffmpeg exited with {status} after {sent} frames. ffmpeg stderr:\n{tail}"
            )));
        }
        if sent != self.frames {
            return Err(KimoError::Encode(format!(
                "internal error: {sent} frames were captured but {} were expected",
                self.frames
            )));
        }
        Ok(())
    }

    /// Verify the staged file with ffprobe, then rename into place.
    pub async fn verify_and_rename(&self, ffprobe: &Path) -> Result<PathBuf> {
        // Basic existence checks first.
        let meta = tokio::fs::metadata(&self.part_path).await.map_err(|_| {
            KimoError::Verify(format!(
                "ffmpeg reported success but {} is missing",
                self.part_path.display()
            ))
        })?;
        if meta.len() == 0 {
            return Err(KimoError::Verify(format!(
                "ffmpeg reported success but {} is empty",
                self.part_path.display()
            )));
        }

        // ffprobe the stream.
        let out = tokio::process::Command::new(ffprobe)
            .args([
                "-v",
                "error",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
            ])
            .arg(&self.part_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| KimoError::Verify(format!("failed to run ffprobe: {e}")))?;
        if !out.status.success() {
            return Err(KimoError::Verify(format!(
                "ffprobe failed on the staged output: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout)
            .map_err(|e| KimoError::Verify(format!("ffprobe produced unparseable JSON: {e}")))?;

        let stream = v
            .get("streams")
            .and_then(|s| s.as_array())
            .and_then(|a| {
                a.iter()
                    .find(|st| st.get("codec_type").and_then(|t| t.as_str()) == Some("video"))
            })
            .ok_or_else(|| KimoError::Verify("ffprobe found no video stream in the output".into()))?;

        let codec = stream
            .get("codec_name")
            .and_then(|c| c.as_str())
            .unwrap_or("");
        if codec != "h264" {
            return Err(KimoError::Verify(format!(
                "video codec is '{codec}', expected h264"
            )));
        }

        let w = stream.get("width").and_then(|w| w.as_u64()).unwrap_or(0);
        let h = stream.get("height").and_then(|h| h.as_u64()).unwrap_or(0);
        if w != self.expected_w || h != self.expected_h {
            return Err(KimoError::Verify(format!(
                "video dimensions are {w}x{h}, expected {}x{}",
                self.expected_w, self.expected_h
            )));
        }

        let rate = stream
            .get("avg_frame_rate")
            .and_then(|r| r.as_str())
            .unwrap_or("0/1");
        let fps = parse_fraction(rate);
        if (fps - 60.0).abs() > 0.01 {
            return Err(KimoError::Verify(format!(
                "average frame rate is {rate} ({fps:.3} fps), expected 60/1"
            )));
        }

        let duration = v
            .get("format")
            .and_then(|f| f.get("duration"))
            .and_then(|d| d.as_str())
            .and_then(|d| d.parse::<f64>().ok())
            .unwrap_or(0.0);
        let expected_duration = self.frames as f64 / 60.0;
        if (duration - expected_duration).abs() > 0.15 {
            return Err(KimoError::Verify(format!(
                "container duration is {duration:.3}s, expected {expected_duration:.3}s ({} frames at 60fps)",
                self.frames
            )));
        }

        // All checks passed: atomically rename into place.
        tokio::fs::rename(&self.part_path, &self.final_path)
            .await
            .map_err(|e| {
                KimoError::Verify(format!(
                    "verification passed but renaming {} -> {} failed: {e}",
                    self.part_path.display(),
                    self.final_path.display()
                ))
            })?;
        Ok(self.final_path.clone())
    }

    /// Delete the staged file (failure path). Never touches the profile.
    pub async fn cleanup(&self) {
        let _ = tokio::fs::remove_file(&self.part_path).await;
    }

    pub async fn stderr_string(&self) -> String {
        self.stderr_tail
            .lock()
            .await
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn parse_fraction(s: &str) -> f64 {
    let mut it = s.splitn(2, '/');
    let num: f64 = it.next().and_then(|n| n.parse().ok()).unwrap_or(0.0);
    let den: f64 = match it.next() {
        Some(d) => d.parse().unwrap_or(1.0),
        None => 1.0,
    };
    if den == 0.0 {
        0.0
    } else {
        num / den
    }
}
