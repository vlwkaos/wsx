//! Compact copies of uploads: what an agent reads first instead of the original.
//! Contract: docs/web-client.md "Media". One handler per type (`make`); a type with no
//! handler, a missing tool, a timeout or a bad file gives `None`, and the prompt then names
//! the original only. Nothing here fails an upload.
//! ^ The copy lives in the media directory under the original's hash, so it is
//! content-addressed and removed with its entry (`Media::remove`).
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

/// A text page with fewer letters and digits than this counts as having no text.
const PAGE_TEXT_MIN: usize = 10;
const PDF_TEXT_MAX: usize = 4 * 1024 * 1024;
const PDF_TIME: Duration = Duration::from_secs(30);
const VIDEO_FRAMES: usize = 12;
const VIDEO_MAX_SECONDS: f64 = 30.0 * 60.0;
/// Whisper (CPU) runs at about half the speed of the audio; longer clips get frames only.
const TRANSCRIPT_MAX_SECONDS: f64 = 120.0;
const FFMPEG_TIME: Duration = Duration::from_secs(60);
const WHISPER_TIME: Duration = Duration::from_secs(90);

/// The compact copy of one media entry. `files` are names in the media directory (a file
/// or, for frames, a directory); the first one is what the prompt names.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Compact {
    pub files: Vec<String>,
    /// What the copy holds, as the prompt line says it.
    pub summary: String,
    /// When the original is worth opening.
    pub original_for: String,
    pub size: u64,
}

/// Make the compact copy of `<dir>/<hash>.<ext>`, or none when the type has no handler
/// or the copy cannot be made.
pub fn make(dir: &Path, hash: &str, ext: &str) -> Option<Compact> {
    let original = dir.join(format!("{hash}.{ext}"));
    match ext {
        "pdf" => pdf(&original, dir, hash),
        "mp4" | "webm" => video(&original, dir, hash, ext),
        _ => None,
    }
}

/// Remove the files of a copy, also a partly made one.
pub fn remove(dir: &Path, name: &str) {
    let path = dir.join(name);
    if path.is_dir() {
        let _ = fs::remove_dir_all(path);
    } else {
        let _ = fs::remove_file(path);
    }
}

/// Bytes of a file, or of everything under a directory.
fn bytes_of(path: &Path) -> u64 {
    match fs::read_dir(path) {
        Ok(items) => items.flatten().map(|item| bytes_of(&item.path())).sum(),
        Err(_) => fs::metadata(path).map_or(0, |meta| meta.len()),
    }
}

fn size_of(dir: &Path, files: &[String]) -> u64 {
    files.iter().map(|name| bytes_of(&dir.join(name))).sum()
}

/// The first `name` file on PATH. The server's own PATH decides: a tool missing there is
/// a tool the machine lacks.
fn tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Run a tool with no shell and no input. Returns its first `keep` bytes of output when it
/// exits well inside `limit`; kills it on a timeout or on more output than `keep`.
fn run(name: &str, args: &[&OsStr], keep: usize, limit: Duration) -> Option<Vec<u8>> {
    let mut child = Command::new(tool(name)?)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let over = Arc::new(AtomicBool::new(false));
    let reader = {
        let over = over.clone();
        thread::spawn(move || {
            let mut out = Vec::new();
            let _ = stdout.by_ref().take(keep as u64 + 1).read_to_end(&mut out);
            if out.len() > keep {
                over.store(true, Ordering::SeqCst);
            }
            out
        })
    };
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if start.elapsed() < limit && !over.load(Ordering::SeqCst) => {
                thread::sleep(Duration::from_millis(20));
            }
            _ => break None,
        }
    };
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let out = reader.join().ok()?;
    status.filter(|status| status.success())?;
    (!over.load(Ordering::SeqCst)).then_some(out)
}

fn text_of(out: Vec<u8>) -> String {
    String::from_utf8_lossy(&out).into_owned()
}

fn list(numbers: &[usize]) -> String {
    numbers
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// `pdftotext` text with `<!-- page:N -->` before each page. A page with no text is listed,
/// never filled in. Every page without text means no copy.
fn pdf(original: &Path, dir: &Path, hash: &str) -> Option<Compact> {
    let out = run(
        "pdftotext",
        &[
            "-enc".as_ref(),
            "UTF-8".as_ref(),
            original.as_ref(),
            "-".as_ref(),
        ],
        PDF_TEXT_MAX,
        PDF_TIME,
    )?;
    let text = text_of(out);
    // pdftotext ends every page with a form feed.
    let mut pages: Vec<&str> = text.split('\x0c').collect();
    if pages.last().is_some_and(|last| last.trim().is_empty()) {
        pages.pop();
    }
    let missing: Vec<usize> = pages
        .iter()
        .enumerate()
        .filter(|(_, page)| page.chars().filter(|c| c.is_alphanumeric()).count() < PAGE_TEXT_MIN)
        .map(|(index, _)| index + 1)
        .collect();
    if pages.is_empty() || missing.len() == pages.len() {
        return None;
    }
    let mut note = format!("{} pages", pages.len());
    let mut open_for = "figures, tables or exact layout".to_owned();
    let mut header = format!("<!-- wsx: {note}");
    if !missing.is_empty() {
        let gap = format!("no text on pages {}", list(&missing));
        header.push_str(&format!("; {gap}: open those in the original as images"));
        note.push_str(&format!(", {gap}"));
        open_for.push_str(" and the pages without text");
    }
    header.push_str(" -->\n");
    for (index, page) in pages.iter().enumerate() {
        header.push_str(&format!("<!-- page:{} -->\n{}\n", index + 1, page.trim()));
    }
    let name = format!("{hash}.pdf.md");
    crate::devices::write_private(&dir.join(&name), header.as_bytes()).ok()?;
    let files = vec![name];
    Some(Compact {
        size: size_of(dir, &files),
        summary: format!("text of {note}"),
        original_for: open_for,
        files,
    })
}

fn clock(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

/// A frame folder (scene changes, at least one every 30 s, `VIDEO_FRAMES` at most, named
/// with their time) and, for a short clip with a model already on the machine, a transcript.
fn video(original: &Path, dir: &Path, hash: &str, ext: &str) -> Option<Compact> {
    let probe = |args: &[&OsStr]| run("ffprobe", args, 64 * 1024, Duration::from_secs(10));
    let seconds: f64 = text_of(probe(&[
        "-v".as_ref(),
        "error".as_ref(),
        "-show_entries".as_ref(),
        "format=duration".as_ref(),
        "-of".as_ref(),
        "csv=p=0".as_ref(),
        original.as_ref(),
    ])?)
    .trim()
    .parse()
    .ok()?;
    if !(seconds > 0.0 && seconds <= VIDEO_MAX_SECONDS) {
        return None;
    }
    let has_audio = probe(&[
        "-v".as_ref(),
        "error".as_ref(),
        "-select_streams".as_ref(),
        "a".as_ref(),
        "-show_entries".as_ref(),
        "stream=index".as_ref(),
        "-of".as_ref(),
        "csv=p=0".as_ref(),
        original.as_ref(),
    ])
    .is_some_and(|out| !out.is_empty());

    let work = dir.join(format!("{hash}.{ext}.part"));
    let _ = fs::remove_dir_all(&work);
    crate::devices::private_dir(&work).ok()?;
    let frames = frames(original, &work, seconds);
    let transcript = (has_audio && seconds <= TRANSCRIPT_MAX_SECONDS)
        .then(|| transcript(original, &work))
        .flatten();
    if frames.is_none() && transcript.is_none() {
        let _ = fs::remove_dir_all(&work);
        return None;
    }
    let mut files = Vec::new();
    let mut parts = vec![clock(seconds)];
    let folder = format!("{hash}.{ext}.frames");
    let shown = frames.filter(|_| {
        let _ = fs::remove_dir_all(dir.join(&folder));
        fs::rename(work.join("frames"), dir.join(&folder)).is_ok()
    });
    if let Some(text) = transcript {
        let name = format!("{hash}.{ext}.md");
        let header = format!(
            "<!-- wsx: machine transcript (whisper), may contain errors; {} long -->\n",
            clock(seconds)
        );
        let bytes = format!("{header}{text}");
        if crate::devices::write_private(&dir.join(&name), bytes.as_bytes()).is_ok() {
            parts.push("transcript".to_owned());
            files.push(name);
        }
    }
    if let Some(count) = shown {
        // ^ The prompt names the first file; frames are named when they are not it.
        let place = if files.is_empty() {
            String::new()
        } else {
            format!(" in {}/", dir.join(&folder).display())
        };
        let plural = if count == 1 { "" } else { "s" };
        parts.push(format!("{count} frame{plural} named fNN-tM_SS.jpg{place}"));
        files.push(folder);
    }
    let _ = fs::remove_dir_all(&work);
    if files.is_empty() {
        return None;
    }
    Some(Compact {
        size: size_of(dir, &files),
        summary: parts.join(", "),
        original_for: "motion, sound or detail the frames lack".to_owned(),
        files,
    })
}

/// Frames into `<work>/frames/fNN-tM_SS.jpg`; how many.
fn frames(original: &Path, work: &Path, seconds: f64) -> Option<usize> {
    let out = work.join("frames");
    crate::devices::private_dir(&out).ok()?;
    let times = work.join("times.txt");
    let filter = format!(
        "select='gt(scene,0.3)+isnan(prev_selected_t)+gte(t-prev_selected_t,30)',\
         metadata=print:file={},scale='min(768,iw)':-2",
        times.display()
    );
    let target = out.join("f%02d.jpg");
    run(
        "ffmpeg",
        &[
            "-v".as_ref(),
            "error".as_ref(),
            "-y".as_ref(),
            "-i".as_ref(),
            original.as_ref(),
            "-vf".as_ref(),
            filter.as_ref(),
            "-fps_mode".as_ref(),
            "vfr".as_ref(),
            "-frames:v".as_ref(),
            VIDEO_FRAMES.to_string().as_ref(),
            "-q:v".as_ref(),
            "5".as_ref(),
            target.as_ref(),
        ],
        64 * 1024,
        FFMPEG_TIME,
    )?;
    let stamps: Vec<f64> = fs::read_to_string(&times)
        .ok()?
        .split("pts_time:")
        .skip(1)
        .filter_map(|rest| rest.split_whitespace().next()?.parse().ok())
        .collect();
    let mut count = 0;
    for (index, stamp) in stamps.iter().enumerate() {
        let from = out.join(format!("f{:02}.jpg", index + 1));
        let total = stamp.min(seconds).max(0.0) as u64;
        let to = out.join(format!(
            "f{:02}-t{}_{:02}.jpg",
            index + 1,
            total / 60,
            total % 60
        ));
        if fs::rename(&from, &to).is_ok() {
            count += 1;
        }
    }
    (count > 0).then_some(count)
}

/// `[m:ss] text` lines from whisper, using a model already on the machine: it never
/// downloads one, so a conversion makes no network call.
fn transcript(original: &Path, work: &Path) -> Option<String> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".cache")))?
        .join("whisper");
    cache.join("base.pt").is_file().then_some(())?;
    let audio = work.join("audio.wav");
    run(
        "ffmpeg",
        &[
            "-v".as_ref(),
            "error".as_ref(),
            "-y".as_ref(),
            "-i".as_ref(),
            original.as_ref(),
            "-vn".as_ref(),
            "-ac".as_ref(),
            "1".as_ref(),
            "-ar".as_ref(),
            "16000".as_ref(),
            audio.as_ref(),
        ],
        64 * 1024,
        FFMPEG_TIME,
    )?;
    run(
        "whisper",
        &[
            audio.as_ref(),
            "--model".as_ref(),
            "base".as_ref(),
            "--model_dir".as_ref(),
            cache.as_ref(),
            "--fp16".as_ref(),
            "False".as_ref(),
            "--output_format".as_ref(),
            "vtt".as_ref(),
            "--output_dir".as_ref(),
            work.as_ref(),
        ],
        1024 * 1024,
        WHISPER_TIME,
    )?;
    let vtt = fs::read_to_string(work.join("audio.vtt")).ok()?;
    let mut lines = Vec::new();
    let mut at = None;
    for line in vtt.lines() {
        if let Some((start, _)) = line.split_once(" --> ") {
            // `[hh:]mm:ss.mmm`
            let parts: Vec<f64> = start.split(':').filter_map(|p| p.parse().ok()).collect();
            at = Some(parts.iter().fold(0.0, |total, part| total * 60.0 + part));
        } else if let (Some(seconds), false) = (at, line.trim().is_empty()) {
            lines.push(format!("[{}] {}", clock(seconds), line.trim()));
        }
    }
    (!lines.is_empty()).then(|| lines.join("\n") + "\n")
}
