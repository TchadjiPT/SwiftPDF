//! SwiftPDF
//!
//! One worker thread owns PDFium for the whole process and every open
//! document. The UI never blocks: page sizes arrive first, pixels and text are
//! produced lazily, and search runs against a text index the worker builds in
//! the background while idle.

#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

use eframe::egui::{
    self, pos2, vec2, Align2, Color32, FontId, Rect, Sense, Stroke, TextureHandle, TextureOptions, Vec2,
};
use pdfium_render::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const GAP: f32 = 16.0; // space between pages
const PREFETCH: usize = 2; // pages rendered beyond the viewport
const MAX_CACHED: usize = 40; // page textures kept per document
const THUMB_GAP: f32 = 8.0;
const THUMB_LABEL: f32 = 20.0;
const MAX_THUMBS: usize = 250;
const MAX_HITS: usize = 50_000;
const MAX_RECENT: usize = 12;

// ======================================================================
// Shared types
// ======================================================================

/// (page index, character index within that page's text)
type Pos = (usize, usize);

#[derive(Clone)]
struct CharInfo {
    ch: char,
    /// left, top, right, bottom in PDF points, origin at the page's top-left
    rect: [f32; 4],
}

#[derive(Clone, Copy)]
struct Hit {
    page: usize,
    start: usize,
    len: usize,
}

enum Req {
    Open { id: u64, path: PathBuf, password: Option<String> },
    Close { id: u64 },
    Render { id: u64, page: usize, width: u32, thumb: bool },
    Chars { id: u64, page: usize },
    Copy { id: u64, start: Pos, end: Pos },
    Search { id: u64, query: String },
    /// Burn the highlights into the document and save it (to `path`).
    Save { id: u64, path: PathBuf, highlights: Vec<(usize, usize, usize, u8)> },
    /// Merge / split / export job that runs on the worker thread.
    Job { job: u64, kind: JobKind },
}

enum JobKind {
    Merge { inputs: Vec<PathBuf>, output: PathBuf },
    Extract { source: PathBuf, ranges: String, output: PathBuf },
    SplitEvery { source: PathBuf, every: usize, dir: PathBuf },
    ExportImages { source: PathBuf, ranges: String, dpi: u32, dir: PathBuf },
    ExportText { source: PathBuf, ranges: String, output: PathBuf },
    ExportWord { source: PathBuf, ranges: String, output: PathBuf },
}

enum Msg {
    Opened { id: u64, sizes: Vec<(f32, f32)> },
    OpenFailed { id: u64, error: String },
    Page { id: u64, page: usize, width: u32, thumb: bool, image: egui::ColorImage },
    Dropped { id: u64, page: usize, width: u32, thumb: bool },
    Chars { id: u64, page: usize, chars: Arc<Vec<CharInfo>> },
    Copied(String),
    Index { id: u64, done: usize, total: usize },
    Hits { id: u64, query: String, hits: Vec<Hit>, complete: bool },
    /// The worker could not start (e.g. pdfium.dll missing); nothing can be opened.
    Fatal(String),
    Saved { id: u64, path: PathBuf, error: Option<String> },
    JobProgress { job: u64, done: usize, total: usize },
    JobDone { job: u64, result: Result<String, String> },
    /// The PDF is encrypted; `wrong` is true when a password was tried and rejected.
    NeedPassword { id: u64, wrong: bool },
}

fn msg_id(m: &Msg) -> u64 {
    match m {
        Msg::Opened { id, .. }
        | Msg::OpenFailed { id, .. }
        | Msg::Page { id, .. }
        | Msg::Dropped { id, .. }
        | Msg::Chars { id, .. }
        | Msg::Index { id, .. }
        | Msg::Hits { id, .. }
        | Msg::Saved { id, .. }
        | Msg::NeedPassword { id, .. } => *id,
        Msg::Copied(_) | Msg::Fatal(_) | Msg::JobProgress { .. } | Msg::JobDone { .. } => 0,
    }
}

/// What the UI currently cares about. The worker skips render requests that
/// are for a background tab or have scrolled out of range.
#[derive(Default)]
struct Focus {
    active: AtomicU64,
    main_lo: AtomicUsize,
    main_hi: AtomicUsize, // exclusive
    thumb_lo: AtomicUsize,
    thumb_hi: AtomicUsize, // exclusive
}

// ======================================================================
// Worker thread
// ======================================================================

struct PageIndex {
    text: Vec<char>, // normalised (lowercase, whitespace -> ' ', no '\r')
    map: Vec<u32>,   // text index -> original char index
}

struct WDoc<'a> {
    doc: PdfDocument<'a>,
    path: PathBuf,
    password: Option<&'static str>,
    index: Vec<Option<PageIndex>>,
    indexed: usize,
    last_query: String,
}

fn norm(c: char) -> char {
    if c.is_whitespace() {
        ' '
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

fn build_index(chars: &[CharInfo]) -> PageIndex {
    let mut text = Vec::with_capacity(chars.len());
    let mut map = Vec::with_capacity(chars.len());
    for (i, c) in chars.iter().enumerate() {
        if c.ch == '\r' {
            continue;
        }
        text.push(norm(c.ch));
        map.push(i as u32);
    }
    PageIndex { text, map }
}

fn run_search(index: &[Option<PageIndex>], query: &str) -> (Vec<Hit>, bool) {
    let needle: Vec<char> = query.trim().chars().map(norm).collect();
    let mut hits = Vec::new();
    if needle.is_empty() {
        return (hits, true);
    }
    let mut complete = true;
    for (page, entry) in index.iter().enumerate() {
        let Some(pi) = entry else {
            complete = false;
            continue;
        };
        let n = needle.len();
        let mut i = 0;
        while i + n <= pi.text.len() {
            if pi.text[i] == needle[0] && pi.text[i..i + n] == needle[..] {
                let start = pi.map[i] as usize;
                let end = pi.map[i + n - 1] as usize;
                hits.push(Hit { page, start, len: end - start + 1 });
                if hits.len() >= MAX_HITS {
                    return (hits, false);
                }
                i += n;
            } else {
                i += 1;
            }
        }
    }
    (hits, complete)
}

fn extract_chars(doc: &PdfDocument, page: usize, bounds: bool) -> Vec<CharInfo> {
    let mut out = Vec::new();
    let Ok(p) = doc.pages().get(page as u16) else { return out };
    let h = p.height().value;
    let Ok(text) = p.text() else { return out };
    for c in text.chars().iter() {
        let ch = c.unicode_char().unwrap_or(' ');
        let rect = if bounds {
            c.tight_bounds()
                .or_else(|_| c.loose_bounds())
                .map(|r| [r.left().value, h - r.top().value, r.right().value, h - r.bottom().value])
                .unwrap_or([0.0; 4])
        } else {
            [0.0; 4]
        };
        out.push(CharInfo { ch, rect });
    }
    out
}

/// Name of the PDFium shared library on this platform.
const PDFIUM_LIB_NAME: &str = if cfg!(target_os = "windows") {
    "pdfium.dll"
} else if cfg!(target_os = "macos") {
    "libpdfium.dylib"
} else {
    "libpdfium.so"
};

/// A copy of the library can be baked into the executable (put pdfium.dll in the
/// project root before building); build.rs sets the `embedded_pdfium` cfg for that.
#[cfg(embedded_pdfium)]
static EMBEDDED_PDFIUM: &[u8] = include_bytes!("../pdfium.dll");

/// Writes the embedded library to the data folder (once) and returns that folder.
#[cfg(embedded_pdfium)]
fn extract_embedded_pdfium() -> Option<PathBuf> {
    let dir = config_dir()?.join("bin");
    let path = dir.join(PDFIUM_LIB_NAME);
    let stale = std::fs::metadata(&path).map(|m| m.len() != EMBEDDED_PDFIUM.len() as u64).unwrap_or(true);
    if stale {
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::write(&path, EMBEDDED_PDFIUM).ok()?;
    }
    Some(dir)
}

#[cfg(not(embedded_pdfium))]
fn extract_embedded_pdfium() -> Option<PathBuf> {
    None
}

/// Folders searched for the PDFium library: next to the executable, a few
/// parent folders (handy while developing: target\\release -> project root)
/// and the working directory. The system library path is tried afterwards.
fn pdfium_search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(|p| p.to_path_buf());
        for _ in 0..5 {
            match dir {
                Some(d) => {
                    let next = d.parent().map(|p| p.to_path_buf());
                    dirs.push(d);
                    dir = next;
                }
                None => break,
            }
        }
    }
    dirs.push(PathBuf::from("./"));
    dirs
}

// ---------------------------------------------------------------- saving / tools

fn page_text(doc: &PdfDocument, page: usize) -> String {
    let s: String = extract_chars(doc, page, false).iter().map(|c| c.ch).collect();
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// "1-3, 5, 8-" -> zero-based page indices. Empty = every page.
fn parse_ranges(spec: &str, total: usize) -> Result<Vec<usize>, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok((0..total).collect());
    }
    let mut out: Vec<usize> = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let bad = || format!("'{part}' is not a valid page range");
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => {
                let (a, b) = (a.trim(), b.trim());
                let a = if a.is_empty() { 1 } else { a.parse::<usize>().map_err(|_| bad())? };
                let b = if b.is_empty() { total } else { b.parse::<usize>().map_err(|_| bad())? };
                (a, b)
            }
            None => {
                let n = part.parse::<usize>().map_err(|_| bad())?;
                (n, n)
            }
        };
        if a < 1 || b < a || b > total {
            return Err(format!("Pages must be between 1 and {total} (got '{part}')"));
        }
        for p in a..=b {
            if !out.contains(&(p - 1)) {
                out.push(p - 1);
            }
        }
    }
    if out.is_empty() {
        return Err("No pages selected".to_string());
    }
    Ok(out)
}

fn save_png(path: &Path, rgba: &[u8], w: u32, h: u32) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    Ok(())
}

/// Draws the highlights into the page content as translucent rectangles.
fn burn_highlights(doc: &PdfDocument, highlights: &[(usize, usize, usize, u8)]) -> Result<(), String> {
    let mut pages: Vec<usize> = highlights.iter().map(|h| h.0).collect();
    pages.sort_unstable();
    pages.dedup();
    for pg in pages {
        let chars = extract_chars(doc, pg, true);
        let mut page = doc.pages().get(pg as u16).map_err(|e| e.to_string())?;
        let h = page.height().value;
        for &(_, start, len, color) in highlights.iter().filter(|x| x.0 == pg) {
            let c = hl_color(color);
            for r in merged_rects(&chars, start, start.saturating_add(len), 1.0, pos2(0.0, 0.0)) {
                // back from top-left-origin points to PDF page coordinates
                let rect = PdfRect::new(
                    PdfPoints::new(h - r.max.y),
                    PdfPoints::new(r.min.x),
                    PdfPoints::new(h - r.min.y),
                    PdfPoints::new(r.max.x),
                );
                page.objects_mut()
                    .create_path_object_rect(rect, None, None, Some(PdfColor::new(c.r(), c.g(), c.b(), 105)))
                    .map_err(|e| e.to_string())?;
            }
        }
        let _ = page.regenerate_content();
    }
    Ok(())
}

/// Saves document `id` to `target` (burning in highlights), then reopens it.
/// Overwriting the open file goes through a temporary file and a rename.
fn save_doc<'a>(
    pdfium: &'a Pdfium,
    docs: &mut HashMap<u64, WDoc<'a>>,
    id: u64,
    target: &Path,
    same: bool,
    highlights: &[(usize, usize, usize, u8)],
) -> Result<(), String> {
    let w = docs.remove(&id).ok_or_else(|| "The document is no longer open.".to_string())?;
    let WDoc { doc, index, indexed, last_query, path: old_path, password } = w;
    let out: PathBuf = if same {
        let mut s = old_path.clone().into_os_string();
        s.push(".swiftpdf-tmp");
        PathBuf::from(s)
    } else {
        target.to_path_buf()
    };

    let step: Result<(), String> = (|| {
        burn_highlights(&doc, highlights)?;
        doc.save_to_file(&out).map_err(|e| e.to_string())?;
        Ok(())
    })();
    drop(doc); // release the file before replacing it

    let outcome: Result<PathBuf, String> = match step {
        Ok(()) if same => match std::fs::rename(&out, &old_path) {
            Ok(()) => Ok(old_path.clone()),
            Err(e) => {
                let _ = std::fs::remove_file(&out);
                Err(format!("Could not replace the original file: {e}"))
            }
        },
        Ok(()) => Ok(target.to_path_buf()),
        Err(e) => {
            if same {
                let _ = std::fs::remove_file(&out);
            }
            Err(e)
        }
    };

    // On success reopen the saved file; on failure reopen the untouched original.
    let reopen = match &outcome {
        Ok(p) => p.clone(),
        Err(_) => old_path.clone(),
    };
    match pdfium.load_pdf_from_file(&reopen, password) {
        Ok(d) => {
            docs.insert(id, WDoc { doc: d, path: reopen, password, index, indexed, last_query });
        }
        Err(e) => return Err(format!("The file could not be reopened: {e}")),
    }
    outcome.map(|_| ())
}

fn run_job(pdfium: &Pdfium, kind: JobKind, progress: &dyn Fn(usize, usize)) -> Result<String, String> {
    let err = |e: PdfiumError| e.to_string();
    let open = |p: &Path| {
        pdfium
            .load_pdf_from_file(p, None)
            .map_err(|e| format!("{}: {e}", file_name(p)))
    };
    let stem_of = |p: &Path| {
        p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "document".to_string())
    };
    match kind {
        JobKind::Merge { inputs, output } => {
            let mut dest = pdfium.create_new_pdf().map_err(err)?;
            let n = inputs.len();
            for (i, p) in inputs.iter().enumerate() {
                let src = open(p)?;
                dest.pages_mut().append(&src).map_err(err)?;
                progress(i + 1, n);
            }
            dest.save_to_file(&output).map_err(err)?;
            Ok(format!("Merged {n} files into {}", file_name(&output)))
        }
        JobKind::Extract { source, ranges, output } => {
            let src = open(&source)?;
            let total = src.pages().len() as usize;
            let idx = parse_ranges(&ranges, total)?;
            let mut dest = pdfium.create_new_pdf().map_err(err)?;
            for (k, &pg) in idx.iter().enumerate() {
                let at = dest.pages().len();
                dest.pages_mut().copy_page_from_document(&src, pg as u16, at).map_err(err)?;
                progress(k + 1, idx.len());
            }
            dest.save_to_file(&output).map_err(err)?;
            Ok(format!("Extracted {} pages into {}", idx.len(), file_name(&output)))
        }
        JobKind::SplitEvery { source, every, dir } => {
            let src = open(&source)?;
            let total = src.pages().len() as usize;
            let every = every.max(1);
            let parts = (total + every - 1) / every;
            let stem = stem_of(&source);
            let width = parts.to_string().len().max(2);
            for part in 0..parts {
                let from = part * every;
                let to = ((part + 1) * every).min(total);
                let mut dest = pdfium.create_new_pdf().map_err(err)?;
                for pg in from..to {
                    let at = dest.pages().len();
                    dest.pages_mut().copy_page_from_document(&src, pg as u16, at).map_err(err)?;
                }
                let name = format!("{stem}_part{:0w$}.pdf", part + 1, w = width);
                dest.save_to_file(&dir.join(name)).map_err(err)?;
                progress(part + 1, parts);
            }
            Ok(format!("Created {parts} files in {}", dir.display()))
        }
        JobKind::ExportImages { source, ranges, dpi, dir } => {
            let src = open(&source)?;
            let total = src.pages().len() as usize;
            let idx = parse_ranges(&ranges, total)?;
            let stem = stem_of(&source);
            for (k, &pg) in idx.iter().enumerate() {
                let page = src.pages().get(pg as u16).map_err(err)?;
                let px = ((page.width().value * dpi as f32 / 72.0).round() as i32).max(1);
                let rgba = {
                    let bmp = page
                        .render_with_config(&PdfRenderConfig::new().set_target_width(px))
                        .map_err(err)?;
                    bmp.as_image().to_rgba8()
                };
                let name = format!("{stem}_page_{:03}.png", pg + 1);
                save_png(&dir.join(name), rgba.as_raw(), rgba.width(), rgba.height())?;
                progress(k + 1, idx.len());
            }
            Ok(format!("Exported {} images to {}", idx.len(), dir.display()))
        }
        JobKind::ExportText { source, ranges, output } => {
            let src = open(&source)?;
            let total = src.pages().len() as usize;
            let idx = parse_ranges(&ranges, total)?;
            let mut text = String::new();
            for (k, &pg) in idx.iter().enumerate() {
                text.push_str(&page_text(&src, pg));
                text.push_str("\n\n");
                progress(k + 1, idx.len());
            }
            std::fs::write(&output, text).map_err(|e| e.to_string())?;
            Ok(format!("Exported text to {}", file_name(&output)))
        }
        JobKind::ExportWord { source, ranges, output } => {
            let src = open(&source)?;
            let total = src.pages().len() as usize;
            let idx = parse_ranges(&ranges, total)?;
            let mut docx = docx_rs::Docx::new();
            for (k, &pg) in idx.iter().enumerate() {
                let text = page_text(&src, pg);
                for line in text.lines() {
                    let clean: String = line.chars().filter(|c| !c.is_control() || *c == '\t').collect();
                    docx = docx.add_paragraph(
                        docx_rs::Paragraph::new().add_run(docx_rs::Run::new().add_text(clean)),
                    );
                }
                if k + 1 < idx.len() {
                    docx = docx.add_paragraph(
                        docx_rs::Paragraph::new()
                            .add_run(docx_rs::Run::new().add_break(docx_rs::BreakType::Page)),
                    );
                }
                progress(k + 1, idx.len());
            }
            let file = std::fs::File::create(&output).map_err(|e| e.to_string())?;
            docx.build().pack(file).map_err(|e| e.to_string())?;
            Ok(format!("Exported Word document to {}", file_name(&output)))
        }
    }
}

fn route(
    req: Req,
    hi: &mut Vec<Req>,
    lo: &mut Vec<Req>,
    opens: &mut Vec<(u64, PathBuf, Option<String>)>,
    closes: &mut Vec<u64>,
) {
    match req {
        Req::Open { id, path, password } => opens.push((id, path, password)),
        Req::Close { id } => closes.push(id),
        Req::Render { thumb: true, .. } => lo.push(req), // thumbnails: low priority
        _ => hi.push(req),
    }
}

fn spawn_worker(ctx: egui::Context, tx: Sender<Msg>, focus: Arc<Focus>) -> Sender<Req> {
    let (req_tx, req_rx) = mpsc::channel::<Req>();
    thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_worker(&ctx, &tx, req_rx, &focus)
        }));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(msg)) => {
                let _ = tx.send(Msg::Fatal(msg));
            }
            Err(_) => {
                let _ = tx.send(Msg::Fatal(
                    "The PDF engine stopped unexpectedly. Please restart SwiftPDF.\n\
                     Details were written to crash.log in the data folder (Settings > About)."
                        .to_string(),
                ));
            }
        }
        ctx.request_repaint();
    });
    req_tx
}

fn run_worker(ctx: &egui::Context, tx: &Sender<Msg>, rx: Receiver<Req>, focus: &Focus) -> Result<(), String> {
    // PDFium is loaded ONCE here and shared by every document.
    let mut found = None;
    for d in &pdfium_search_dirs() {
        if let Ok(b) = Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(d)) {
            found = Some(b);
            break;
        }
    }
    if found.is_none() {
        if let Some(dir) = extract_embedded_pdfium() {
            if let Ok(b) = Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(&dir)) {
                found = Some(b);
            }
        }
    }
    let bindings = match found {
        Some(b) => b,
        None => Pdfium::bind_to_system_library().map_err(|e| {
            format!(
                "SwiftPDF could not load the PDFium library (pdfium.dll on Windows).\n\
                 Copy pdfium.dll into the same folder as swiftpdf.exe, then start the app again.\n\n\
                 Technical details: {e}"
            )
        })?,
    };
    let pdfium = Pdfium::new(bindings);

    let send = |m: Msg| {
        let _ = tx.send(m);
        ctx.request_repaint();
    };

    let mut docs: HashMap<u64, WDoc> = HashMap::new();
    let mut hi: Vec<Req> = Vec::new();
    let mut lo: Vec<Req> = Vec::new();
    let mut opens: Vec<(u64, PathBuf, Option<String>)> = Vec::new();
    let mut closes: Vec<u64> = Vec::new();

    loop {
        let indexing = docs.values().any(|w| w.indexed < w.index.len());
        if hi.is_empty() && lo.is_empty() && opens.is_empty() && closes.is_empty() && !indexing {
            match rx.recv() {
                Ok(r) => route(r, &mut hi, &mut lo, &mut opens, &mut closes),
                Err(_) => return Ok(()),
            }
        }
        loop {
            match rx.try_recv() {
                Ok(r) => route(r, &mut hi, &mut lo, &mut opens, &mut closes),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }

        for (id, path, password) in opens.drain(..) {
            // pdfium-render ties the password's lifetime to the library, so keep it for the process lifetime
            let pw: Option<&'static str> = password.map(|p| &*Box::leak(p.into_boxed_str()));
            match pdfium.load_pdf_from_file(&path, pw) {
                Ok(d) => {
                    let sizes: Vec<(f32, f32)> =
                        d.pages().iter().map(|p| (p.width().value, p.height().value)).collect();
                    let index: Vec<Option<PageIndex>> = std::iter::repeat_with(|| None).take(sizes.len()).collect();
                    docs.insert(id, WDoc { doc: d, path: path.clone(), password: pw, index, indexed: 0, last_query: String::new() });
                    send(Msg::Opened { id, sizes });
                }
                Err(e) => {
                    if format!("{e:?}").contains("Password") {
                        send(Msg::NeedPassword { id, wrong: pw.is_some() });
                    } else {
                        send(Msg::OpenFailed { id, error: format!("{e}") });
                    }
                }
            }
        }
        for id in closes.drain(..) {
            docs.remove(&id);
        }

        let next = hi.pop().or_else(|| lo.pop());
        match next {
            Some(Req::Render { id, page, width, thumb }) => {
                let Some(w) = docs.get(&id) else { continue };
                let (lo_b, hi_b) = if thumb {
                    (focus.thumb_lo.load(Relaxed), focus.thumb_hi.load(Relaxed))
                } else {
                    (focus.main_lo.load(Relaxed), focus.main_hi.load(Relaxed))
                };
                if focus.active.load(Relaxed) != id || page < lo_b || page >= hi_b {
                    send(Msg::Dropped { id, page, width, thumb });
                    continue;
                }
                let rendered = w.doc.pages().get(page as u16).ok().and_then(|p| {
                    // Convert to an owned image while `p` is alive (the bitmap borrows the page).
                    let bmp = p.render_with_config(&PdfRenderConfig::new().set_target_width(width as i32)).ok()?;
                    Some(bmp.as_image().to_rgba8())
                });
                match rendered {
                    Some(rgba) => {
                        let size = [rgba.width() as usize, rgba.height() as usize];
                        let image = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                        send(Msg::Page { id, page, width, thumb, image });
                    }
                    None => send(Msg::Dropped { id, page, width, thumb }),
                }
            }
            Some(Req::Chars { id, page }) => {
                let Some(w) = docs.get(&id) else { continue };
                let chars = extract_chars(&w.doc, page, true);
                send(Msg::Chars { id, page, chars: Arc::new(chars) });
            }
            Some(Req::Copy { id, start, end }) => {
                let Some(w) = docs.get(&id) else { continue };
                let (s, e) = if start <= end { (start, end) } else { (end, start) };
                let last_page = e.0.min(w.index.len().saturating_sub(1));
                let mut out = String::new();
                for pg in s.0..=last_page {
                    let chars = extract_chars(&w.doc, pg, false);
                    let from = if pg == s.0 { s.1 } else { 0 };
                    let to = if pg == e.0 { (e.1 + 1).min(chars.len()) } else { chars.len() };
                    if from < to {
                        out.extend(chars[from..to].iter().map(|c| c.ch));
                    }
                    if pg != e.0 {
                        out.push('\n');
                    }
                }
                send(Msg::Copied(out.replace("\r\n", "\n")));
            }
            Some(Req::Search { id, query }) => {
                let Some(w) = docs.get_mut(&id) else { continue };
                let (hits, complete) = run_search(&w.index, &query);
                w.last_query = query.clone();
                send(Msg::Hits { id, query, hits, complete });
            }
            Some(Req::Save { id, path, highlights }) => {
                let Some(w) = docs.get(&id) else { continue };
                let same = w.path == path
                    || (path.exists() && std::fs::canonicalize(&path).ok() == std::fs::canonicalize(&w.path).ok());
                let result = save_doc(&pdfium, &mut docs, id, &path, same, &highlights);
                send(Msg::Saved { id, path: path.clone(), error: result.err() });
            }
            Some(Req::Job { job, kind }) => {
                let progress = |done: usize, total: usize| send(Msg::JobProgress { job, done, total });
                let result = run_job(&pdfium, kind, &progress);
                send(Msg::JobDone { job, result });
            }
            Some(Req::Open { .. }) | Some(Req::Close { .. }) => {}
            None => {
                // Idle: build search indexes, one page at a time (active tab first),
                // so new requests are never kept waiting for long.
                let active = focus.active.load(Relaxed);
                let pick = if docs.get(&active).map_or(false, |w| w.indexed < w.index.len()) {
                    Some(active)
                } else {
                    docs.iter().find(|(_, w)| w.indexed < w.index.len()).map(|(id, _)| *id)
                };
                if let Some(id) = pick {
                    if let Some(w) = docs.get_mut(&id) {
                        let i = w.indexed;
                        let chars = extract_chars(&w.doc, i, false);
                        w.index[i] = Some(build_index(&chars));
                        w.indexed += 1;
                        if w.indexed % 20 == 0 || w.indexed == w.index.len() {
                            send(Msg::Index { id, done: w.indexed, total: w.index.len() });
                        }
                        if w.indexed == w.index.len() && !w.last_query.is_empty() {
                            let (hits, complete) = run_search(&w.index, &w.last_query);
                            send(Msg::Hits { id, query: w.last_query.clone(), hits, complete });
                        }
                    }
                }
            }
        }
    }
}

// ======================================================================
// Saved state: reading positions and recent files
// ======================================================================

fn config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("swiftpdf"))
}

fn path_key(p: &Path) -> String {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().to_string()
}

/// key -> (page position as page + fraction, zoom)
fn load_positions() -> HashMap<String, (f32, f32)> {
    let mut m = HashMap::new();
    let Some(dir) = config_dir() else { return m };
    let Ok(s) = std::fs::read_to_string(dir.join("positions.tsv")) else { return m };
    for line in s.lines() {
        let mut it = line.rsplitn(3, '\t');
        let zoom = it.next().and_then(|v| v.parse::<f32>().ok());
        let pos = it.next().and_then(|v| v.parse::<f32>().ok());
        let path = it.next();
        if let (Some(z), Some(p), Some(path)) = (zoom, pos, path) {
            m.insert(path.to_string(), (p, z));
        }
    }
    m
}

fn save_positions(m: &HashMap<String, (f32, f32)>) {
    let Some(dir) = config_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let s: String = m.iter().map(|(k, (pos, z))| format!("{k}\t{pos}\t{z}\n")).collect();
    let _ = std::fs::write(dir.join("positions.tsv"), s);
}

fn load_recents() -> Vec<PathBuf> {
    let Some(dir) = config_dir() else { return Vec::new() };
    let Ok(s) = std::fs::read_to_string(dir.join("recent.txt")) else { return Vec::new() };
    s.lines().map(PathBuf::from).filter(|p| p.exists()).take(MAX_RECENT).collect()
}

fn save_recents(list: &[PathBuf]) {
    let Some(dir) = config_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let s: String = list.iter().map(|p| format!("{}\n", p.to_string_lossy())).collect();
    let _ = std::fs::write(dir.join("recent.txt"), s);
}

#[derive(Clone)]
struct Settings {
    restore_tabs: bool,
    keep_recents: bool,
    remember_position: bool,
    dark: bool,
    accent: usize,
    animations: bool,
    page_shadows: bool,
    show_sidebar: bool,
    default_zoom: f32, // percent
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            restore_tabs: true,
            keep_recents: true,
            remember_position: true,
            dark: true,
            accent: 0,
            animations: true,
            page_shadows: true,
            show_sidebar: true,
            default_zoom: 100.0,
        }
    }
}

fn load_settings() -> Settings {
    let mut s = Settings::default();
    let Some(dir) = config_dir() else { return s };
    let Ok(txt) = std::fs::read_to_string(dir.join("settings.txt")) else { return s };
    for line in txt.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        let b = v == "true";
        match k.trim() {
            "restore_tabs" => s.restore_tabs = b,
            "keep_recents" => s.keep_recents = b,
            "remember_position" => s.remember_position = b,
            "dark" => s.dark = b,
            "animations" => s.animations = b,
            "page_shadows" => s.page_shadows = b,
            "show_sidebar" => s.show_sidebar = b,
            "accent" => s.accent = v.parse::<usize>().unwrap_or(0),
            "default_zoom" => s.default_zoom = v.parse::<f32>().unwrap_or(100.0),
            _ => {}
        }
    }
    s.accent = s.accent.min(4);
    s.default_zoom = s.default_zoom.clamp(50.0, 300.0);
    s
}

fn save_settings(s: &Settings) {
    let Some(dir) = config_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let txt = format!(
        "restore_tabs={}\nkeep_recents={}\nremember_position={}\ndark={}\naccent={}\nanimations={}\npage_shadows={}\nshow_sidebar={}\ndefault_zoom={}\n",
        s.restore_tabs,
        s.keep_recents,
        s.remember_position,
        s.dark,
        s.accent,
        s.animations,
        s.page_shadows,
        s.show_sidebar,
        s.default_zoom
    );
    let _ = std::fs::write(dir.join("settings.txt"), txt);
}

/// (active tab index, open files) saved from the previous run.
fn load_session() -> (usize, Vec<PathBuf>) {
    let Some(dir) = config_dir() else { return (0, Vec::new()) };
    let Ok(s) = std::fs::read_to_string(dir.join("session.txt")) else { return (0, Vec::new()) };
    let mut lines = s.lines();
    let active = lines.next().and_then(|l| l.trim().parse::<usize>().ok()).unwrap_or(0);
    let paths: Vec<PathBuf> = lines.map(PathBuf::from).filter(|p| p.exists()).collect();
    (active, paths)
}

fn write_session(active: usize, paths: &[PathBuf]) {
    let Some(dir) = config_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let mut s = format!("{active}\n");
    for p in paths {
        s.push_str(&p.to_string_lossy());
        s.push('\n');
    }
    let _ = std::fs::write(dir.join("session.txt"), s);
}

/// Highlight length meaning "to the end of the page".
const TO_END: usize = 1 << 30;

/// A highlighted run of characters on one page (indices match the page's text).
#[derive(Clone, Copy)]
struct Highlight {
    page: usize,
    start: usize,
    len: usize,
    color: u8,
}

const HL_NAMES: [&str; 4] = ["Yellow", "Green", "Pink", "Blue"];

fn hl_color(i: u8) -> Color32 {
    match i {
        0 => Color32::from_rgb(255, 214, 10),
        1 => Color32::from_rgb(84, 214, 128),
        2 => Color32::from_rgb(255, 122, 182),
        _ => Color32::from_rgb(96, 170, 255),
    }
}

fn load_highlights(key: &str) -> Vec<Highlight> {
    let Some(dir) = config_dir() else { return Vec::new() };
    let Ok(s) = std::fs::read_to_string(dir.join("highlights.tsv")) else { return Vec::new() };
    let prefix = format!("{key}\t");
    let mut out = Vec::new();
    for line in s.lines() {
        let Some(rest) = line.strip_prefix(&prefix) else { continue };
        let f: Vec<&str> = rest.split('\t').collect();
        if f.len() == 4 {
            if let (Ok(page), Ok(start), Ok(len), Ok(color)) = (
                f[0].parse::<usize>(),
                f[1].parse::<usize>(),
                f[2].parse::<usize>(),
                f[3].parse::<u8>(),
            ) {
                out.push(Highlight { page, start, len, color });
            }
        }
    }
    out
}

fn save_highlights(key: &str, list: &[Highlight]) {
    let Some(dir) = config_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("highlights.tsv");
    let prefix = format!("{key}\t");
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let mut out: String = old.lines().filter(|l| !l.starts_with(&prefix)).map(|l| format!("{l}\n")).collect();
    for h in list {
        out.push_str(&format!("{key}\t{}\t{}\t{}\t{}\n", h.page, h.start, h.len, h.color));
    }
    let _ = std::fs::write(path, out);
}

#[derive(Clone, Copy, PartialEq)]
enum ToolTab {
    Merge,
    Split,
    Export,
}

#[derive(Clone, Copy, PartialEq)]
enum SplitMode {
    Extract,
    EveryN,
    Single,
}

#[derive(Clone, Copy, PartialEq)]
enum ExportFormat {
    Png,
    Text,
    Word,
}

struct Tools {
    open: bool,
    tab: ToolTab,
    merge_files: Vec<PathBuf>,
    split_source: Option<PathBuf>,
    split_mode: SplitMode,
    split_ranges: String,
    split_every: u32,
    export_source: Option<PathBuf>,
    export_format: ExportFormat,
    export_pages: String,
    export_dpi: u32,
    /// (job id, label, done, total) of the running job
    job: Option<(u64, String, usize, usize)>,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            open: false,
            tab: ToolTab::Merge,
            merge_files: Vec::new(),
            split_source: None,
            split_mode: SplitMode::Extract,
            split_ranges: String::new(),
            split_every: 5,
            export_source: None,
            export_format: ExportFormat::Png,
            export_pages: String::new(),
            export_dpi: 150,
            job: None,
        }
    }
}

/// Display options passed down to the document views.
#[derive(Clone, Copy)]
struct ViewOpts {
    anim: bool,
    shadows: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum WelcomePage {
    Home,
    Recent,
    Settings,
}

fn open_in_explorer(path: &Path) {
    let prog = if cfg!(target_os = "windows") {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(prog).arg(path).spawn();
}

/// Appends panics to crash.log in the data folder (the release build has no console).
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        use std::io::Write;
        let when = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let text = format!("[unix time {when}] {info}\n");
        if let Some(dir) = config_dir() {
            let _ = std::fs::create_dir_all(&dir);
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("crash.log")) {
                let _ = f.write_all(text.as_bytes());
            }
        }
    }));
}

// ---- single instance: a second launch hands its files to the running window ----

const INSTANCE_PORT: u16 = 47619;

/// Tries to give `args` to an already running SwiftPDF. Returns true if it accepted them.
fn forward_to_running_instance(args: &[PathBuf]) -> bool {
    use std::io::{Read, Write};
    use std::net::{Shutdown, SocketAddr, TcpStream};
    let addr = SocketAddr::from(([127, 0, 0, 1], INSTANCE_PORT));
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(300)) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_millis(1500)));
    let mut msg = String::from("SWIFTPDF1\n");
    for p in args {
        let abs = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
        msg.push_str(&abs.to_string_lossy());
        msg.push('\n');
    }
    if s.write_all(msg.as_bytes()).is_err() {
        return false;
    }
    let _ = s.shutdown(Shutdown::Write);
    let mut reply = String::new();
    let _ = s.read_to_string(&mut reply);
    reply.trim() == "OK"
}

/// Listens for files forwarded by later launches (local connections only).
fn start_instance_server(ctx: egui::Context) -> Option<Receiver<Vec<PathBuf>>> {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind(("127.0.0.1", INSTANCE_PORT)).ok()?;
    let (tx, rx) = mpsc::channel::<Vec<PathBuf>>();
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else { continue };
            let _ = stream.set_read_timeout(Some(Duration::from_millis(800)));
            let mut buf = String::new();
            let _ = std::io::Read::by_ref(&mut stream).take(64 * 1024).read_to_string(&mut buf);
            let mut lines = buf.lines();
            if lines.next() != Some("SWIFTPDF1") {
                continue;
            }
            let paths: Vec<PathBuf> = lines.filter(|l| !l.is_empty()).map(PathBuf::from).collect();
            let _ = stream.write_all(b"OK\n");
            let _ = tx.send(paths);
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.request_repaint();
        }
    });
    Some(rx)
}

fn load_icon() -> Option<egui::IconData> {
    let bytes = include_bytes!("../assets/icon.png");
    let decoder = png::Decoder::new(std::io::Cursor::new(&bytes[..]));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    buf.truncate(info.buffer_size());
    Some(egui::IconData { rgba: buf, width: info.width, height: info.height })
}

fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).collect::<String>() + "\u{2026}"
    }
}

fn shorten_left(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else {
        "\u{2026}".to_string() + &s.chars().skip(n - (max - 1)).collect::<String>()
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "Untitled".to_string())
}

// ======================================================================
// Theme, icons and small widgets
// ======================================================================

#[derive(Clone, Copy)]
struct Theme {
    dark: bool,
    bg: Color32,      // canvas behind the pages, tab strip
    panel: Color32,   // toolbar / sidebar / active tab
    surface: Color32, // cards, inputs
    border: Color32,
    text: Color32,
    muted: Color32,
    accent: Color32,
    hover: Color32,
}

const ACCENT_NAMES: [&str; 5] = ["Blue", "Violet", "Green", "Orange", "Pink"];
const ACCENTS_DARK: [Color32; 5] = [
    Color32::from_rgb(124, 156, 255),
    Color32::from_rgb(176, 146, 255),
    Color32::from_rgb(105, 214, 152),
    Color32::from_rgb(255, 172, 102),
    Color32::from_rgb(255, 132, 184),
];
const ACCENTS_LIGHT: [Color32; 5] = [
    Color32::from_rgb(61, 99, 240),
    Color32::from_rgb(124, 77, 230),
    Color32::from_rgb(20, 150, 86),
    Color32::from_rgb(222, 108, 16),
    Color32::from_rgb(212, 58, 124),
];

impl Theme {
    fn new(dark: bool, accent: usize) -> Self {
        let i = accent.min(ACCENTS_DARK.len() - 1);
        if dark {
            Self::dark(ACCENTS_DARK[i])
        } else {
            Self::light(ACCENTS_LIGHT[i])
        }
    }

    fn dark(accent: Color32) -> Self {
        Self {
            dark: true,
            bg: Color32::from_rgb(20, 21, 24),
            panel: Color32::from_rgb(28, 29, 33),
            surface: Color32::from_rgb(38, 39, 44),
            border: Color32::from_rgb(52, 53, 60),
            text: Color32::from_rgb(232, 233, 237),
            muted: Color32::from_rgb(139, 141, 152),
            accent,
            hover: Color32::from_rgb(46, 47, 54),
        }
    }

    fn light(accent: Color32) -> Self {
        Self {
            dark: false,
            bg: Color32::from_rgb(233, 234, 238),
            panel: Color32::from_rgb(247, 247, 249),
            surface: Color32::WHITE,
            border: Color32::from_rgb(220, 221, 227),
            text: Color32::from_rgb(30, 31, 36),
            muted: Color32::from_rgb(107, 109, 120),
            accent,
            hover: Color32::from_rgb(236, 237, 241),
        }
    }

    /// Colour of a page while its pixels are still being rendered.
    fn page_placeholder(&self) -> Color32 {
        if self.dark {
            Color32::from_rgb(43, 44, 50)
        } else {
            Color32::WHITE
        }
    }

    fn error_bg(&self) -> Color32 {
        if self.dark {
            Color32::from_rgb(88, 30, 34)
        } else {
            Color32::from_rgb(255, 232, 232)
        }
    }

    fn error_text(&self) -> Color32 {
        if self.dark {
            Color32::from_rgb(255, 205, 205)
        } else {
            Color32::from_rgb(150, 30, 30)
        }
    }
}

const ERROR_RED: Color32 = Color32::from_rgb(235, 95, 95);

fn with_alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

/// Colour `c` with its opacity multiplied by `a` (0.0 - 1.0).
fn fade(c: Color32, a: f32) -> Color32 {
    let [r, g, b, al] = c.to_srgba_unmultiplied();
    Color32::from_rgba_unmultiplied(r, g, b, (al as f32 * a.clamp(0.0, 1.0)) as u8)
}

fn mix(a: Color32, b: Color32, f: f32) -> Color32 {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f).round() as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

fn apply_theme(ctx: &egui::Context, t: &Theme) {
    let mut v = if t.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.panel_fill = t.panel;
    v.window_fill = t.surface;
    v.extreme_bg_color = t.bg;
    v.faint_bg_color = t.hover;
    v.hyperlink_color = t.accent;
    v.window_stroke = Stroke::new(1.0, t.border);
    v.window_rounding = egui::Rounding::same(12.0);
    v.selection.bg_fill = with_alpha(t.accent, 90);
    v.selection.stroke = Stroke::new(1.0, t.accent);

    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, t.border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, t.text);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, t.text);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, t.text);
    v.widgets.active.fg_stroke = Stroke::new(1.0, t.text);

    v.widgets.inactive.bg_fill = t.surface;
    v.widgets.inactive.weak_bg_fill = t.surface;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, t.border);
    v.widgets.hovered.bg_fill = t.hover;
    v.widgets.hovered.weak_bg_fill = t.hover;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, t.border);
    v.widgets.active.bg_fill = mix(t.hover, t.accent, 0.25);
    v.widgets.active.weak_bg_fill = mix(t.hover, t.accent, 0.25);
    v.widgets.active.bg_stroke = Stroke::new(1.0, t.accent);
    for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active] {
        w.rounding = egui::Rounding::same(8.0);
    }

    ctx.style_mut(|s| {
        s.visuals = v;
        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(10.0, 5.0);
        s.spacing.scroll = egui::style::ScrollStyle::floating(); // slim overlay scrollbars
        s.text_styles.insert(egui::TextStyle::Body, FontId::proportional(14.0));
        s.text_styles.insert(egui::TextStyle::Button, FontId::proportional(14.0));
        s.text_styles.insert(egui::TextStyle::Small, FontId::proportional(11.5));
        s.text_styles.insert(egui::TextStyle::Heading, FontId::proportional(20.0));
    });
}

#[derive(Clone, Copy, PartialEq)]
enum Icon {
    Sidebar,
    Open,
    Search,
    Up,
    Down,
    Close,
    Minus,
    Plus,
    Fit,
    Contrast,
    Home,
    Clock,
    Gear,
    Trash,
    Save,
    Tools,
}

fn paint_icon(p: &egui::Painter, icon: Icon, c: egui::Pos2, col: Color32) {
    let s = Stroke::new(1.6, col);
    let l = |a: (f32, f32), b: (f32, f32)| {
        p.line_segment([c + vec2(a.0, a.1), c + vec2(b.0, b.1)], s);
    };
    match icon {
        Icon::Sidebar => {
            p.rect_stroke(Rect::from_center_size(c, vec2(16.0, 13.0)), 3.0, s);
            l((-2.5, -6.0), (-2.5, 6.0));
        }
        Icon::Open => {
            l((-8.0, -5.0), (-8.0, 6.0));
            l((-8.0, 6.0), (8.0, 6.0));
            l((8.0, 6.0), (8.0, -2.0));
            l((8.0, -2.0), (-0.5, -2.0));
            l((-0.5, -2.0), (-2.5, -5.0));
            l((-2.5, -5.0), (-8.0, -5.0));
        }
        Icon::Search => {
            p.circle_stroke(c + vec2(-1.5, -1.5), 5.5, s);
            l((2.5, 2.5), (7.0, 7.0));
        }
        Icon::Up => {
            l((-5.0, 2.5), (0.0, -2.5));
            l((0.0, -2.5), (5.0, 2.5));
        }
        Icon::Down => {
            l((-5.0, -2.5), (0.0, 2.5));
            l((0.0, 2.5), (5.0, -2.5));
        }
        Icon::Close => {
            l((-4.0, -4.0), (4.0, 4.0));
            l((-4.0, 4.0), (4.0, -4.0));
        }
        Icon::Minus => {
            l((-6.0, 0.0), (6.0, 0.0));
        }
        Icon::Plus => {
            l((-6.0, 0.0), (6.0, 0.0));
            l((0.0, -6.0), (0.0, 6.0));
        }
        Icon::Fit => {
            l((-8.0, -5.0), (-8.0, 5.0));
            l((8.0, -5.0), (8.0, 5.0));
            l((-5.0, 0.0), (5.0, 0.0));
            l((-5.0, 0.0), (-2.5, -2.5));
            l((-5.0, 0.0), (-2.5, 2.5));
            l((5.0, 0.0), (2.5, -2.5));
            l((5.0, 0.0), (2.5, 2.5));
        }
        Icon::Save => {
            p.rect_stroke(Rect::from_center_size(c, vec2(15.0, 15.0)), 2.5, s);
            l((-4.0, -7.5), (-4.0, -2.0));
            l((-4.0, -2.0), (4.0, -2.0));
            l((4.0, -2.0), (4.0, -7.5));
            l((-4.0, 3.0), (4.0, 3.0));
            l((-4.0, 6.0), (4.0, 6.0));
        }
        Icon::Tools => {
            p.rect_stroke(Rect::from_center_size(c + vec2(0.0, 1.5), vec2(17.0, 11.0)), 2.5, s);
            l((-3.0, -4.0), (-3.0, -6.5));
            l((-3.0, -6.5), (3.0, -6.5));
            l((3.0, -6.5), (3.0, -4.0));
            l((-8.5, 1.5), (8.5, 1.5));
        }
        Icon::Home => {
            l((-8.0, 0.0), (0.0, -7.0));
            l((0.0, -7.0), (8.0, 0.0));
            l((-5.5, -1.5), (-5.5, 7.0));
            l((-5.5, 7.0), (5.5, 7.0));
            l((5.5, 7.0), (5.5, -1.5));
        }
        Icon::Clock => {
            p.circle_stroke(c, 7.5, s);
            l((0.0, 0.0), (0.0, -4.0));
            l((0.0, 0.0), (3.5, 2.0));
        }
        Icon::Gear => {
            p.circle_stroke(c, 5.0, s);
            p.circle_stroke(c, 1.8, s);
            for k in 0..8 {
                let a = k as f32 * std::f32::consts::FRAC_PI_4;
                let d = vec2(a.cos(), a.sin());
                p.line_segment([c + d * 5.0, c + d * 8.0], s);
            }
        }
        Icon::Trash => {
            l((-6.0, -3.5), (6.0, -3.5));
            l((-2.0, -3.5), (-2.0, -6.0));
            l((-2.0, -6.0), (2.0, -6.0));
            l((2.0, -6.0), (2.0, -3.5));
            l((-4.5, -2.0), (-3.8, 6.5));
            l((-3.8, 6.5), (3.8, 6.5));
            l((3.8, 6.5), (4.5, -2.0));
        }
        Icon::Contrast => {
            p.circle_stroke(c, 7.0, s);
            let pts: Vec<egui::Pos2> = (0..=12)
                .map(|i| {
                    let a = std::f32::consts::FRAC_PI_2 + std::f32::consts::PI * i as f32 / 12.0;
                    c + vec2(a.cos(), a.sin()) * 7.0
                })
                .collect();
            p.add(egui::Shape::convex_polygon(pts, col, Stroke::NONE));
        }
    }
}

fn icon_button(ui: &mut egui::Ui, t: &Theme, icon: Icon, tip: &str, active: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip);
    let fill = if resp.is_pointer_button_down_on() {
        mix(t.hover, t.accent, 0.3)
    } else if active {
        with_alpha(t.accent, 50)
    } else if resp.hovered() {
        t.hover
    } else {
        Color32::TRANSPARENT
    };
    let col = if active {
        t.accent
    } else if resp.hovered() {
        t.text
    } else {
        mix(t.muted, t.text, 0.5)
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 8.0, fill);
    paint_icon(painter, icon, rect.center(), col);
    resp
}

fn vsep(ui: &mut egui::Ui, t: &Theme) {
    let (rect, _) = ui.allocate_exact_size(vec2(1.0, 20.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, t.border);
}

fn paint_primary(p: &egui::Painter, t: &Theme, text: &str, rect: Rect, a: f32, hot: bool, down: bool) {
    let fg = if t.dark { Color32::from_rgb(15, 16, 20) } else { Color32::WHITE };
    let fill = if down {
        mix(t.accent, Color32::BLACK, 0.15)
    } else if hot {
        mix(t.accent, Color32::WHITE, 0.12)
    } else {
        t.accent
    };
    p.rect_filled(rect, 10.0, fade(fill, a));
    p.text(rect.center(), Align2::CENTER_CENTER, text, FontId::proportional(15.0), fade(fg, a));
}

/// Accent-filled button that takes up layout space; `a` is its opacity.
fn primary_button(ui: &mut egui::Ui, t: &Theme, text: &str, a: f32, min_w: f32) -> egui::Response {
    let g = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(15.0), Color32::WHITE);
    let mut size = g.size() + vec2(40.0, 22.0);
    size.x = size.x.max(min_w);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    paint_primary(ui.painter(), t, text, rect, a, resp.hovered(), resp.is_pointer_button_down_on());
    resp
}

/// Same look, drawn at an exact rectangle without touching the layout.
fn primary_button_at(ui: &mut egui::Ui, t: &Theme, text: &str, rect: Rect, a: f32) -> egui::Response {
    let resp = ui
        .interact(rect, ui.id().with(("pbtn", text)), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    paint_primary(ui.painter(), t, text, rect, a, resp.hovered(), resp.is_pointer_button_down_on());
    resp
}

fn ghost_button_at(
    ui: &mut egui::Ui,
    t: &Theme,
    icon: Option<Icon>,
    label: &str,
    rect: Rect,
    a: f32,
    danger: bool,
) -> egui::Response {
    let resp = ui
        .interact(rect, ui.id().with(("ghost", label)), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let hot = resp.hovered();
    let col = if hot && danger {
        ERROR_RED
    } else if hot {
        t.text
    } else {
        mix(t.muted, t.text, 0.4)
    };
    let p = ui.painter();
    p.rect_filled(rect, 9.0, fade(if hot { t.hover } else { Color32::TRANSPARENT }, a));
    p.rect_stroke(rect, 9.0, Stroke::new(1.0, fade(if hot && danger { ERROR_RED } else { t.border }, a)));
    match icon {
        Some(ic) => {
            paint_icon(p, ic, pos2(rect.left() + 20.0, rect.center().y), fade(col, a));
            p.text(
                pos2(rect.left() + 36.0, rect.center().y),
                Align2::LEFT_CENTER,
                label,
                FontId::proportional(13.5),
                fade(col, a),
            );
        }
        None => {
            p.text(rect.center(), Align2::CENTER_CENTER, label, FontId::proportional(13.5), fade(col, a));
        }
    }
    resp
}

fn link_at(ui: &mut egui::Ui, t: &Theme, text: &str, rect: Rect, a: f32) -> egui::Response {
    let resp = ui
        .interact(rect, ui.id().with(("link", text)), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let col = fade(if resp.hovered() { mix(t.accent, t.text, 0.3) } else { t.accent }, a);
    ui.painter().text(rect.right_center(), Align2::RIGHT_CENTER, text, FontId::proportional(13.5), col);
    resp
}

/// One recent-file card. Returns (open clicked, remove clicked).
fn recent_row(
    ui: &mut egui::Ui,
    t: &Theme,
    path: &Path,
    info: Option<String>,
    a: f32,
    w: f32,
    removable: bool,
) -> (bool, bool) {
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 58.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let rm_rect = Rect::from_center_size(pos2(rect.right() - 26.0, rect.center().y), vec2(28.0, 28.0));
    let rm = if removable {
        Some(
            ui.interact(rm_rect, ui.id().with(("rm", path)), Sense::click())
                .on_hover_text("Remove from recent"),
        )
    } else {
        None
    };
    let rm_hot = rm.as_ref().map_or(false, |r| r.hovered());
    let hov = resp.hovered() || rm_hot;
    let p = ui.painter();
    p.rect_filled(rect, 12.0, fade(if hov { t.hover } else { t.surface }, a));
    p.rect_stroke(rect, 12.0, Stroke::new(1.0, fade(if hov { t.accent } else { t.border }, a)));
    // mini page icon
    let ic = Rect::from_center_size(pos2(rect.left() + 28.0, rect.center().y), vec2(18.0, 24.0));
    p.rect_stroke(ic, 3.0, Stroke::new(1.5, fade(t.accent, a)));
    for k in 0..3 {
        let y = ic.top() + 7.0 + k as f32 * 5.0;
        p.line_segment(
            [pos2(ic.left() + 4.0, y), pos2(ic.right() - 4.0, y)],
            Stroke::new(1.2, fade(t.muted, a)),
        );
    }
    let name = shorten(&file_name(path), 46);
    let dir = shorten_left(&path.parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_default(), 54);
    p.text(
        pos2(rect.left() + 54.0, rect.center().y - 9.0),
        Align2::LEFT_CENTER,
        name,
        FontId::proportional(14.5),
        fade(t.text, a),
    );
    p.text(
        pos2(rect.left() + 54.0, rect.center().y + 10.0),
        Align2::LEFT_CENTER,
        dir,
        FontId::proportional(11.5),
        fade(t.muted, a),
    );
    if let Some(txt) = info {
        let x = rect.right() - if removable { 54.0 } else { 18.0 };
        p.text(pos2(x, rect.center().y), Align2::RIGHT_CENTER, txt, FontId::proportional(12.0), fade(t.muted, a));
    }
    if removable && hov {
        if rm_hot {
            p.rect_filled(rm_rect, 8.0, fade(t.border, a));
        }
        paint_icon(p, Icon::Close, rm_rect.center(), fade(if rm_hot { ERROR_RED } else { t.muted }, a));
    }
    let removed = rm.map_or(false, |r| r.clicked());
    let opened = resp.clicked() && !removed;
    (opened, removed)
}

fn nav_item(
    ui: &mut egui::Ui,
    t: &Theme,
    icon: Icon,
    label: &str,
    selected: bool,
    badge: Option<usize>,
    a: f32,
) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 40.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let hot = resp.hovered();
    let p = ui.painter();
    let fill = if selected {
        with_alpha(t.accent, 40)
    } else if hot {
        t.hover
    } else {
        Color32::TRANSPARENT
    };
    p.rect_filled(rect, 10.0, fade(fill, a));
    if selected {
        p.rect_filled(
            Rect::from_min_max(
                pos2(rect.left(), rect.center().y - 9.0),
                pos2(rect.left() + 3.0, rect.center().y + 9.0),
            ),
            2.0,
            fade(t.accent, a),
        );
    }
    let col = if selected {
        t.accent
    } else if hot {
        t.text
    } else {
        mix(t.muted, t.text, 0.45)
    };
    paint_icon(p, icon, pos2(rect.left() + 24.0, rect.center().y), fade(col, a));
    p.text(
        pos2(rect.left() + 46.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(14.5),
        fade(if selected { t.text } else { col }, a),
    );
    if let Some(n) = badge {
        let g = p.layout_no_wrap(format!("{n}"), FontId::proportional(11.5), fade(t.muted, a));
        let bw = g.size().x + 14.0;
        let br = Rect::from_center_size(pos2(rect.right() - 12.0 - bw / 2.0, rect.center().y), vec2(bw, 20.0));
        p.rect_filled(br, 10.0, fade(t.surface, a));
        p.galley(br.center() - g.size() / 2.0, g, fade(t.muted, a));
    }
    resp
}

fn dashed_rect(p: &egui::Painter, rect: Rect, col: Color32) {
    let s = Stroke::new(1.5, col);
    let (dash, gap) = (8.0_f32, 6.0_f32);
    let edge = |a: egui::Pos2, b: egui::Pos2| {
        let len = (b - a).length();
        let dir = (b - a) / len.max(0.001);
        let mut d = 0.0_f32;
        while d < len {
            let e = (d + dash).min(len);
            p.line_segment([a + dir * d, a + dir * e], s);
            d += dash + gap;
        }
    };
    edge(rect.left_top(), rect.right_top());
    edge(rect.right_top(), rect.right_bottom());
    edge(rect.right_bottom(), rect.left_bottom());
    edge(rect.left_bottom(), rect.left_top());
}

fn big_page_icon(p: &egui::Painter, c: egui::Pos2, col: Color32) {
    let r = Rect::from_center_size(c, vec2(30.0, 38.0));
    p.rect_stroke(r, 5.0, Stroke::new(2.0, col));
    for k in 0..3 {
        let y = r.top() + 12.0 + k as f32 * 7.0;
        p.line_segment([pos2(r.left() + 7.0, y), pos2(r.right() - 7.0, y)], Stroke::new(2.0, col));
    }
}

fn key_chip(ui: &mut egui::Ui, t: &Theme, key: &str, desc: &str, a: f32) {
    let kg = ui.painter().layout_no_wrap(key.to_string(), FontId::proportional(11.5), fade(t.text, a));
    let dg = ui.painter().layout_no_wrap(desc.to_string(), FontId::proportional(12.5), fade(t.muted, a));
    let kw = kg.size().x + 14.0;
    let (rect, _) = ui.allocate_exact_size(vec2(kw + 8.0 + dg.size().x, 24.0), Sense::hover());
    let p = ui.painter();
    let kr = Rect::from_min_size(rect.min, vec2(kw, 24.0));
    p.rect_filled(kr, 6.0, fade(t.surface, a));
    p.rect_stroke(kr, 6.0, Stroke::new(1.0, fade(t.border, a)));
    p.galley(pos2(kr.left() + 7.0, rect.center().y - kg.size().y / 2.0), kg, fade(t.text, a));
    p.galley(pos2(kr.right() + 8.0, rect.center().y - dg.size().y / 2.0), dg, fade(t.muted, a));
}

/// "Source: name  [Choose...]" row used by the Split and Export tabs.
fn source_row(ui: &mut egui::Ui, t: &Theme, src: &mut Option<PathBuf>, active: &Option<PathBuf>) -> Option<PathBuf> {
    let eff = src.clone().or_else(|| active.clone());
    ui.horizontal(|ui| {
        ui.label("Source:");
        let text = match &eff {
            Some(p) => shorten(&file_name(p), 44),
            None => "no PDF selected".to_string(),
        };
        ui.label(egui::RichText::new(text).color(t.muted));
        if ui.button("Choose\u{2026}").clicked() {
            if let Some(p) = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).pick_file() {
                *src = Some(p);
            }
        }
    });
    eff
}

fn card<R>(ui: &mut egui::Ui, t: &Theme, a: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let w = ui.available_width();
    egui::Frame::none()
        .fill(fade(t.surface, a))
        .stroke(Stroke::new(1.0, fade(t.border, a)))
        .rounding(14.0)
        .show(ui, |ui| {
            ui.set_width(w - 2.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            add(ui)
        })
        .inner
}

fn section_label(ui: &mut egui::Ui, t: &Theme, text: &str, a: f32) {
    ui.add_space(18.0);
    ui.label(egui::RichText::new(text).size(11.5).color(fade(t.muted, a)));
    ui.add_space(6.0);
}

fn page_title(ui: &mut egui::Ui, t: &Theme, title: &str, sub: &str, a: f32) {
    ui.label(egui::RichText::new(title).size(28.0).strong().color(fade(t.text, a)));
    ui.add_space(4.0);
    ui.label(egui::RichText::new(sub).size(14.0).color(fade(t.muted, a)));
    ui.add_space(22.0);
}

/// A settings row with a switch on the right. Returns true when toggled.
fn setting_row(
    ui: &mut egui::Ui,
    t: &Theme,
    title: &str,
    desc: &str,
    on: &mut bool,
    divider: bool,
    a: f32,
) -> bool {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 60.0), Sense::hover());
    let r = ui
        .interact(rect, ui.id().with(("row", title)), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let mut changed = false;
    if r.clicked() {
        *on = !*on;
        changed = true;
    }
    let k = ui.ctx().animate_bool_with_time(r.id, *on, 0.14);
    let p = ui.painter();
    if r.hovered() {
        p.rect_filled(rect.shrink(2.0), 12.0, fade(t.hover, a));
    }
    p.text(
        pos2(rect.left() + 16.0, rect.center().y - 9.0),
        Align2::LEFT_CENTER,
        title,
        FontId::proportional(14.5),
        fade(t.text, a),
    );
    p.text(
        pos2(rect.left() + 16.0, rect.center().y + 10.0),
        Align2::LEFT_CENTER,
        desc,
        FontId::proportional(12.0),
        fade(t.muted, a),
    );
    let sw = Rect::from_center_size(pos2(rect.right() - 36.0, rect.center().y), vec2(40.0, 22.0));
    p.rect_filled(sw, 11.0, fade(mix(t.border, t.accent, k), a));
    p.circle_filled(pos2(sw.left() + 11.0 + k * 18.0, sw.center().y), 8.0, fade(Color32::WHITE, a));
    if divider {
        p.line_segment(
            [pos2(rect.left() + 16.0, rect.bottom()), pos2(rect.right() - 16.0, rect.bottom())],
            Stroke::new(1.0, fade(t.border, a)),
        );
    }
    changed
}

fn swatch_row(ui: &mut egui::Ui, t: &Theme, accent: &mut usize, divider: bool, a: f32) -> bool {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 60.0), Sense::hover());
    let pal = if t.dark { &ACCENTS_DARK } else { &ACCENTS_LIGHT };
    let mut changed = false;
    for i in 0..pal.len() {
        let c = pos2(rect.right() - 28.0 - (pal.len() - 1 - i) as f32 * 30.0, rect.center().y);
        let r = ui
            .interact(Rect::from_center_size(c, vec2(26.0, 26.0)), ui.id().with(("accent", i)), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(ACCENT_NAMES[i]);
        if r.clicked() {
            *accent = i;
            changed = true;
        }
    }
    let p = ui.painter();
    p.text(
        pos2(rect.left() + 16.0, rect.center().y - 9.0),
        Align2::LEFT_CENTER,
        "Accent color",
        FontId::proportional(14.5),
        fade(t.text, a),
    );
    p.text(
        pos2(rect.left() + 16.0, rect.center().y + 10.0),
        Align2::LEFT_CENTER,
        "Buttons, highlights and focus rings.",
        FontId::proportional(12.0),
        fade(t.muted, a),
    );
    for i in 0..pal.len() {
        let c = pos2(rect.right() - 28.0 - (pal.len() - 1 - i) as f32 * 30.0, rect.center().y);
        p.circle_filled(c, 9.0, fade(pal[i], a));
        if *accent == i {
            p.circle_stroke(c, 13.0, Stroke::new(2.0, fade(t.text, a)));
        }
    }
    if divider {
        p.line_segment(
            [pos2(rect.left() + 16.0, rect.bottom()), pos2(rect.right() - 16.0, rect.bottom())],
            Stroke::new(1.0, fade(t.border, a)),
        );
    }
    changed
}

fn slider_row(
    ui: &mut egui::Ui,
    t: &Theme,
    title: &str,
    desc: &str,
    v: &mut f32,
    min: f32,
    max: f32,
    divider: bool,
    a: f32,
) -> bool {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 60.0), Sense::hover());
    let track = Rect::from_min_max(
        pos2(rect.right() - 16.0 - 56.0 - 150.0, rect.center().y - 10.0),
        pos2(rect.right() - 16.0 - 56.0, rect.center().y + 10.0),
    );
    let r = ui
        .interact(track, ui.id().with(("slider", title)), Sense::click_and_drag())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let mut changed = false;
    if r.dragged() || r.clicked() {
        if let Some(p) = r.interact_pointer_pos() {
            let f = ((p.x - track.left()) / track.width()).clamp(0.0, 1.0);
            let nv = ((min + f * (max - min)) / 5.0).round() * 5.0;
            if (nv - *v).abs() > 0.01 {
                *v = nv;
                changed = true;
            }
        }
    }
    let p = ui.painter();
    p.text(
        pos2(rect.left() + 16.0, rect.center().y - 9.0),
        Align2::LEFT_CENTER,
        title,
        FontId::proportional(14.5),
        fade(t.text, a),
    );
    p.text(
        pos2(rect.left() + 16.0, rect.center().y + 10.0),
        Align2::LEFT_CENTER,
        desc,
        FontId::proportional(12.0),
        fade(t.muted, a),
    );
    let ty = track.center().y;
    let f = ((*v - min) / (max - min)).clamp(0.0, 1.0);
    let kx = track.left() + f * track.width();
    p.rect_filled(
        Rect::from_min_max(pos2(track.left(), ty - 2.0), pos2(track.right(), ty + 2.0)),
        2.0,
        fade(t.border, a),
    );
    p.rect_filled(Rect::from_min_max(pos2(track.left(), ty - 2.0), pos2(kx, ty + 2.0)), 2.0, fade(t.accent, a));
    p.circle_filled(pos2(kx, ty), 8.0, fade(Color32::WHITE, a));
    p.circle_stroke(pos2(kx, ty), 8.0, Stroke::new(1.0, fade(t.border, a)));
    p.text(
        pos2(rect.right() - 16.0, rect.center().y),
        Align2::RIGHT_CENTER,
        format!("{:.0}%", *v),
        FontId::proportional(13.5),
        fade(t.text, a),
    );
    if divider {
        p.line_segment(
            [pos2(rect.left() + 16.0, rect.bottom()), pos2(rect.right() - 16.0, rect.bottom())],
            Stroke::new(1.0, fade(t.border, a)),
        );
    }
    changed
}

/// Turns a run of characters into highlight rectangles, one per text line.
/// Uses tight glyph boxes and gives each line a uniform, slightly padded
/// height so the highlight sits on the text.
fn merged_rects(chars: &[CharInfo], from: usize, to: usize, zoom: f32, origin: egui::Pos2) -> Vec<Rect> {
    let mut out: Vec<Rect> = Vec::new();
    for c in chars.iter().take(to.min(chars.len())).skip(from) {
        let [l, t, r, b] = c.rect;
        if r - l <= 0.01 || b - t <= 0.01 {
            continue; // spaces/newlines have no box; the merge below bridges the gap
        }
        let rect = Rect::from_min_max(origin + vec2(l * zoom, t * zoom), origin + vec2(r * zoom, b * zoom));
        if let Some(last) = out.last_mut() {
            let overlap = last.max.y.min(rect.max.y) - last.min.y.max(rect.min.y);
            let same_line = overlap > 0.3 * last.height().min(rect.height());
            let tol = last.height().max(rect.height()) * 0.8;
            let near = rect.min.x >= last.min.x && rect.min.x <= last.max.x + tol;
            if same_line && near {
                *last = last.union(rect);
                continue;
            }
        }
        out.push(rect);
    }
    for r in &mut out {
        let pad = r.height() * 0.12;
        r.min.y -= pad;
        r.max.y += pad;
    }
    out
}

// ======================================================================
// One open document (one tab)
// ======================================================================

struct Tex {
    handle: TextureHandle,
    width: u32,
}

struct Selection {
    anchor: Pos,
    head: Pos,
}

struct Doc {
    id: u64,
    path: PathBuf,
    key: String,
    title: String,
    req_tx: Sender<Req>,
    focus: Arc<Focus>,
    error: Option<String>,

    // layout
    sizes: Vec<(f32, f32)>,
    offsets: Vec<f32>,
    total_h: f32,
    max_w: f32,
    content_w: f32,
    thumb_offsets: Vec<f32>,
    thumb_total: f32,
    thumb_w: f32,
    zoom: f32,

    // caches
    textures: HashMap<usize, Tex>,
    thumbs: HashMap<usize, Tex>,
    in_flight: HashSet<(usize, u32, bool)>,
    chars: HashMap<usize, Arc<Vec<CharInfo>>>,
    chars_in_flight: HashSet<usize>,

    // interaction
    selection: Option<Selection>,
    scroll_y: f32,
    scroll_x: f32,
    view_h: f32,
    view_w: f32,
    view_rect: Rect,
    pending_scroll: Option<(Vec2, u8)>, // (target offset, retries)
    current: usize,
    thumb_scroll: Option<f32>,
    thumb_followed: usize,

    // search
    show_search: bool,
    focus_search: bool,
    query: String,
    hits: Vec<Hit>,
    hits_by_page: HashMap<usize, Vec<(usize, usize, usize)>>, // (start, len, hit index)
    hit_cur: usize,
    jump_on_hits: bool,
    reveal: Option<usize>,
    index_progress: (usize, usize),

    restore: Option<(f32, f32)>,

    highlights: Vec<Highlight>,
    menu_target: Option<Pos>,
    /// highlights not yet saved into the PDF
    dirty: bool,

    needs_password: bool,
    password_wrong: bool,
    password_input: String,
}

impl Doc {
    fn new(id: u64, path: PathBuf, key: String, req_tx: Sender<Req>, focus: Arc<Focus>, zoom: f32) -> Self {
        let title = file_name(&path);
        let highlights = load_highlights(&key);
        let dirty = !highlights.is_empty();
        Self {
            id,
            path,
            key,
            title,
            req_tx,
            focus,
            error: None,
            sizes: Vec::new(),
            offsets: Vec::new(),
            total_h: 0.0,
            max_w: 0.0,
            content_w: 0.0,
            thumb_offsets: Vec::new(),
            thumb_total: 0.0,
            thumb_w: 120.0,
            zoom,
            textures: HashMap::new(),
            thumbs: HashMap::new(),
            in_flight: HashSet::new(),
            chars: HashMap::new(),
            chars_in_flight: HashSet::new(),
            selection: None,
            scroll_y: 0.0,
            scroll_x: 0.0,
            view_h: 800.0,
            view_w: 800.0,
            view_rect: Rect::NOTHING,
            pending_scroll: Some((Vec2::ZERO, 0)),
            current: 0,
            thumb_scroll: None,
            thumb_followed: usize::MAX,
            show_search: false,
            focus_search: false,
            query: String::new(),
            hits: Vec::new(),
            hits_by_page: HashMap::new(),
            hit_cur: 0,
            jump_on_hits: false,
            reveal: None,
            index_progress: (0, 0),
            restore: None,
            highlights,
            menu_target: None,
            dirty,
            needs_password: false,
            password_wrong: false,
            password_input: String::new(),
        }
    }

    // ---------------- layout math ----------------

    fn rebuild_layout(&mut self) {
        self.offsets.clear();
        self.max_w = 0.0;
        let mut y = GAP;
        for &(w, h) in &self.sizes {
            self.offsets.push(y);
            y += h * self.zoom + GAP;
            self.max_w = self.max_w.max(w * self.zoom);
        }
        self.total_h = y;
        self.rebuild_thumb_layout(self.thumb_w);
    }

    fn rebuild_thumb_layout(&mut self, tw: f32) {
        self.thumb_w = tw;
        self.thumb_offsets.clear();
        let mut ty = THUMB_GAP;
        for &(w, h) in &self.sizes {
            self.thumb_offsets.push(ty);
            ty += tw * h / w.max(1.0) + THUMB_GAP + THUMB_LABEL;
        }
        self.thumb_total = ty;
    }

    /// Continuous position: page index + fraction through that page.
    fn y_to_page_pos(&self, y: f32) -> f32 {
        let n = self.sizes.len();
        if n == 0 {
            return 0.0;
        }
        let i = self.offsets.partition_point(|&o| o <= y).saturating_sub(1).min(n - 1);
        let h = (self.sizes[i].1 * self.zoom).max(1.0);
        i as f32 + ((y - self.offsets[i]) / h).clamp(0.0, 1.0)
    }

    fn page_pos_to_y(&self, pos: f32) -> f32 {
        let n = self.sizes.len();
        if n == 0 {
            return 0.0;
        }
        let i = (pos.floor().max(0.0) as usize).min(n - 1);
        let frac = (pos - i as f32).clamp(0.0, 1.0);
        self.offsets[i] + frac * self.sizes[i].1 * self.zoom
    }

    fn page_rect(&self, i: usize, origin: egui::Pos2, content_w: f32) -> Rect {
        let (w, h) = self.sizes[i];
        let (pw, ph) = (w * self.zoom, h * self.zoom);
        Rect::from_min_size(pos2(origin.x + (content_w - pw) / 2.0, origin.y + self.offsets[i]), vec2(pw, ph))
    }

    fn pos_state(&self) -> Option<(f32, f32)> {
        if self.sizes.is_empty() {
            None
        } else {
            Some((self.y_to_page_pos(self.scroll_y), self.zoom))
        }
    }

    /// Zoom while keeping the point `anchor_off_y` pixels below the top of the
    /// viewport (cursor or centre) pinned to the same spot in the document.
    fn set_zoom(&mut self, new_zoom: f32, anchor_off_y: f32) {
        let new_zoom = new_zoom.clamp(0.3, 6.0);
        if self.sizes.is_empty() {
            self.zoom = new_zoom;
            return;
        }
        let pos = self.y_to_page_pos(self.scroll_y + anchor_off_y);
        let cx = self.scroll_x + self.view_w / 2.0;
        let old_cw = self.content_w.max(1.0);
        self.zoom = new_zoom;
        self.rebuild_layout();
        let new_y = (self.page_pos_to_y(pos) - anchor_off_y).max(0.0);
        let new_cw = (self.max_w + 2.0 * GAP).max(self.view_w);
        let new_x = (cx * new_cw / old_cw - self.view_w / 2.0).max(0.0);
        self.scroll_y = new_y;
        self.scroll_x = new_x;
        self.pending_scroll = Some((vec2(new_x, new_y), 0));
    }

    fn jump_to_page(&mut self, p: usize) {
        if p < self.offsets.len() {
            self.pending_scroll = Some((vec2(self.scroll_x, (self.offsets[p] - GAP).max(0.0)), 0));
        }
    }

    // ---------------- worker messages ----------------

    fn on_msg(&mut self, ctx: &egui::Context, m: Msg) {
        match m {
            Msg::Opened { sizes, .. } => {
                if sizes.is_empty() {
                    self.error = Some("This PDF has no pages.".to_string());
                    return;
                }
                self.sizes = sizes;
                if let Some((pos, z)) = self.restore.take() {
                    self.zoom = z.clamp(0.3, 6.0);
                    self.rebuild_layout();
                    let y = self.page_pos_to_y(pos);
                    self.scroll_y = y;
                    self.pending_scroll = Some((vec2(0.0, y), 0));
                } else {
                    self.rebuild_layout();
                }
            }
            Msg::OpenFailed { error, .. } => {
                self.error = Some(error);
            }
            Msg::Page { page, width, thumb, image, .. } => {
                self.in_flight.remove(&(page, width, thumb));
                let name = format!("{}{}-{page}", if thumb { "t" } else { "p" }, self.id);
                let handle = ctx.load_texture(name, image, TextureOptions::LINEAR);
                let map = if thumb { &mut self.thumbs } else { &mut self.textures };
                map.insert(page, Tex { handle, width });
            }
            Msg::Dropped { page, width, thumb, .. } => {
                self.in_flight.remove(&(page, width, thumb));
            }
            Msg::Chars { page, chars, .. } => {
                self.chars_in_flight.remove(&page);
                self.chars.insert(page, chars);
            }
            Msg::Index { done, total, .. } => self.index_progress = (done, total),
            Msg::Hits { query, hits, .. } => {
                if query == self.query {
                    self.set_hits(hits);
                }
            }
            Msg::NeedPassword { wrong, .. } => {
                self.needs_password = true;
                self.password_wrong = wrong;
            }
            Msg::Copied(_) | Msg::Fatal(_) | Msg::Saved { .. } | Msg::JobProgress { .. } | Msg::JobDone { .. } => {}
        }
    }

    /// Keep memory bounded: drop whatever is farthest from the reader.
    fn evict(&mut self) {
        let cur = self.current;
        while self.textures.len() > MAX_CACHED {
            match self.textures.keys().copied().max_by_key(|&k| k.abs_diff(cur)) {
                Some(k) => self.textures.remove(&k),
                None => break,
            };
        }
        while self.thumbs.len() > MAX_THUMBS {
            match self.thumbs.keys().copied().max_by_key(|&k| k.abs_diff(cur)) {
                Some(k) => self.thumbs.remove(&k),
                None => break,
            };
        }
        if self.chars.len() > 80 {
            self.chars.retain(|&k, _| k.abs_diff(cur) <= 30);
        }
    }

    /// Called when this tab stops being the active one.
    fn trim_for_background(&mut self) {
        let cur = self.current;
        self.textures.retain(|&k, _| k.abs_diff(cur) <= 2);
        self.thumbs.retain(|&k, _| k.abs_diff(cur) <= 12);
        self.chars.retain(|&k, _| k.abs_diff(cur) <= 3);
        self.chars_in_flight.clear();
        self.in_flight.clear();
    }

    // ---------------- search ----------------

    fn on_query_changed(&mut self) {
        self.hits.clear();
        self.hits_by_page.clear();
        self.hit_cur = 0;
        self.jump_on_hits = true;
        if !self.query.trim().is_empty() && !self.sizes.is_empty() {
            let _ = self.req_tx.send(Req::Search { id: self.id, query: self.query.clone() });
        }
    }

    fn set_hits(&mut self, hits: Vec<Hit>) {
        self.hits = hits;
        self.hits_by_page.clear();
        for (idx, h) in self.hits.iter().enumerate() {
            self.hits_by_page.entry(h.page).or_default().push((h.start, h.len, idx));
        }
        if self.hit_cur >= self.hits.len() {
            self.hit_cur = 0;
        }
        if self.jump_on_hits && !self.hits.is_empty() {
            self.jump_on_hits = false;
            self.goto_hit(0);
        }
    }

    fn goto_hit(&mut self, i: usize) {
        if i >= self.hits.len() || self.sizes.is_empty() {
            return;
        }
        self.hit_cur = i;
        let page = self.hits[i].page.min(self.sizes.len() - 1);
        self.reveal = Some(i); // refined once the page's text is loaded
        self.pending_scroll = Some((vec2(self.scroll_x, (self.offsets[page] - GAP).max(0.0)), 0));
    }

    fn step_hit(&mut self, dir: i64) {
        if self.hits.is_empty() {
            return;
        }
        let n = self.hits.len() as i64;
        let next = ((self.hit_cur as i64 + dir) % n + n) % n;
        self.goto_hit(next as usize);
    }

    fn close_search(&mut self) {
        self.show_search = false;
        self.hits.clear();
        self.hits_by_page.clear();
        self.reveal = None;
    }

    // ---------------- selection ----------------

    fn hit_char(&self, pos: egui::Pos2, origin: egui::Pos2, content_w: f32) -> Option<Pos> {
        let n = self.sizes.len();
        if n == 0 {
            return None;
        }
        let y = pos.y - origin.y;
        let i = self.offsets.partition_point(|&o| o <= y).saturating_sub(1).min(n - 1);
        let chars = self.chars.get(&i)?;
        let rect = self.page_rect(i, origin, content_w);
        let p = (pos - rect.min) / self.zoom; // in PDF points
        let mut best: Option<usize> = None;
        let mut best_d = f32::MAX;
        for (idx, c) in chars.iter().enumerate() {
            let [l, t, r, b] = c.rect;
            if r - l <= 0.01 || b - t <= 0.01 {
                continue;
            }
            let dx = (l - p.x).max(0.0).max(p.x - r);
            let dy = (t - p.y).max(0.0).max(p.y - b);
            let d = dx * dx + dy * dy * 4.0; // prefer staying on the same line
            if d < best_d {
                best_d = d;
                best = Some(idx);
                if d == 0.0 {
                    break;
                }
            }
        }
        best.map(|idx| (i, idx))
    }

    fn sel_on_page(&self, i: usize, len: usize) -> Option<(usize, usize)> {
        let sel = self.selection.as_ref()?;
        let (s, e) = if sel.anchor <= sel.head { (sel.anchor, sel.head) } else { (sel.head, sel.anchor) };
        if i < s.0 || i > e.0 {
            return None;
        }
        let from = if i == s.0 { s.1 } else { 0 };
        let to = if i == e.0 { e.1 + 1 } else { len };
        Some((from, to.min(len)))
    }

    // ---------------- highlights & copy ----------------

    fn persist_highlights(&self) {
        save_highlights(&self.key, &self.highlights);
    }

    /// Highlight the current selection in colour `color` (replaces overlapping highlights).
    fn highlight_selection(&mut self, color: u8) {
        let Some(sel) = &self.selection else { return };
        let (s, e) = if sel.anchor <= sel.head { (sel.anchor, sel.head) } else { (sel.head, sel.anchor) };
        let mut added: Vec<Highlight> = Vec::new();
        for page in s.0..=e.0 {
            let start = if page == s.0 { s.1 } else { 0 };
            let end = if page == e.0 { e.1 + 1 } else { TO_END };
            if end > start {
                added.push(Highlight { page, start, len: end - start, color });
            }
        }
        for n in &added {
            self.highlights.retain(|h| {
                !(h.page == n.page
                    && h.start < n.start.saturating_add(n.len)
                    && n.start < h.start.saturating_add(h.len))
            });
        }
        self.highlights.extend(added);
        self.dirty = true;
        self.persist_highlights();
    }

    fn highlight_at(&self, pos: Pos) -> bool {
        self.highlights
            .iter()
            .any(|h| h.page == pos.0 && pos.1 >= h.start && pos.1 < h.start.saturating_add(h.len))
    }

    fn remove_highlight_at(&mut self, pos: Pos) {
        let before = self.highlights.len();
        self.highlights
            .retain(|h| !(h.page == pos.0 && pos.1 >= h.start && pos.1 < h.start.saturating_add(h.len)));
        if self.highlights.len() != before {
            self.dirty = true;
            self.persist_highlights();
        }
    }

    /// After a save the PDF itself contains the highlights: drop cached pixels so they re-render.
    fn refresh_after_save(&mut self) {
        self.textures.clear();
        self.thumbs.clear();
        self.in_flight.clear();
    }

    fn request_copy(&self) {
        if let Some(s) = &self.selection {
            let _ = self.req_tx.send(Req::Copy { id: self.id, start: s.anchor, end: s.head });
        }
    }

    // ---------------- password prompt ----------------

    fn password_card(&mut self, ui: &mut egui::Ui, t: &Theme) {
        let mut submit = false;
        egui::Frame::none()
            .fill(t.surface)
            .stroke(Stroke::new(1.0, t.border))
            .rounding(14.0)
            .inner_margin(egui::Margin::symmetric(24.0, 20.0))
            .show(ui, |ui| {
                ui.set_width(340.0);
                ui.label(egui::RichText::new("This PDF is password protected").size(16.0).strong());
                ui.add_space(4.0);
                let (msg, col) = if self.password_wrong {
                    ("Incorrect password. Try again.", ERROR_RED)
                } else {
                    ("Enter the password to open it.", t.muted)
                };
                ui.label(egui::RichText::new(msg).color(col));
                ui.add_space(10.0);
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.password_input)
                        .password(true)
                        .hint_text("Password")
                        .desired_width(f32::INFINITY),
                );
                if ui.memory(|m| m.focused().is_none()) {
                    r.request_focus();
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
                ui.add_space(10.0);
                if primary_button(ui, t, "Unlock", 1.0, 340.0).clicked() {
                    submit = true;
                }
            });
        if submit && !self.password_input.is_empty() {
            let pw = std::mem::take(&mut self.password_input);
            self.needs_password = false;
            self.error = None;
            let _ = self.req_tx.send(Req::Open { id: self.id, path: self.path.clone(), password: Some(pw) });
        }
    }

    // ---------------- drawing: sidebar ----------------

    /// Thumbnail list. Returns the page the user clicked, if any.
    fn show_sidebar(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, t: &Theme, o: ViewOpts) -> Option<usize> {
        let mut jump: Option<usize> = None;
        let tw = (ui.available_width() - 8.0).clamp(70.0, 300.0);
        if (tw - self.thumb_w).abs() > 0.5 {
            self.rebuild_thumb_layout(tw);
        }
        let ppp = ctx.pixels_per_point();
        // quantised so dragging the sidebar edge doesn't re-render on every pixel
        let tw_px = ((((tw * ppp) / 32.0).ceil()) as u32 * 32).max(64);

        let mut sa = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if let Some(y) = self.thumb_scroll.take() {
            sa = sa.vertical_scroll_offset(y);
        }
        let doc_id = self.id;
        ui.push_id(doc_id, |ui| {
            sa.show_viewport(ui, |ui, vp| {
                let n = self.sizes.len();
                let (area, _) = ui.allocate_exact_size(vec2(tw + 8.0, self.thumb_total), Sense::hover());
                let first = self.thumb_offsets.partition_point(|&o| o <= vp.min.y).saturating_sub(1).min(n - 1);
                let last = self.thumb_offsets.partition_point(|&o| o < vp.max.y).max(first + 1).min(n);
                let lo = first.saturating_sub(4);
                let hi = (last + 4).min(n);
                self.focus.thumb_lo.store(lo.saturating_sub(3), Relaxed);
                self.focus.thumb_hi.store((hi + 3).min(n), Relaxed);

                let mut order: Vec<usize> = (lo..hi).collect();
                order.sort_by_key(|&i| std::cmp::Reverse(i.abs_diff(first)));
                for i in order {
                    let fresh = self.thumbs.get(&i).map_or(false, |x| x.width == tw_px);
                    if !fresh && !self.in_flight.contains(&(i, tw_px, true)) {
                        let req = Req::Render { id: self.id, page: i, width: tw_px, thumb: true };
                        if self.req_tx.send(req).is_ok() {
                            self.in_flight.insert((i, tw_px, true));
                        }
                    }
                }

                let painter = ui.painter().clone();
                for i in first..last {
                    let h = tw * self.sizes[i].1 / self.sizes[i].0.max(1.0);
                    let rect = Rect::from_min_size(
                        pos2(area.min.x + 4.0, area.min.y + self.thumb_offsets[i]),
                        vec2(tw, h),
                    );
                    let r = ui
                        .interact(rect, ui.id().with(("thumb", i)), Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    let is_cur = i == self.current;

                    painter.rect_filled(
                        rect.translate(vec2(0.0, 2.0)).expand(1.0),
                        4.0,
                        Color32::from_black_alpha(40),
                    );
                    painter.rect_filled(rect, 3.0, t.page_placeholder());
                    let present = self.thumbs.contains_key(&i);
                    let a = ctx.animate_bool_with_time(egui::Id::new(("tfade", self.id, i)), present, if o.anim { 0.25 } else { 0.0 });
                    if let Some(tex) = self.thumbs.get(&i) {
                        let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                        let tint = Color32::from_white_alpha((a.clamp(0.0, 1.0) * 255.0) as u8);
                        painter.image(tex.handle.id(), rect, uv, tint);
                    }
                    if is_cur {
                        painter.rect_stroke(rect.expand(3.0), 5.0, Stroke::new(2.0, t.accent));
                    } else if r.hovered() {
                        painter.rect_stroke(rect.expand(3.0), 5.0, Stroke::new(1.5, t.border));
                    }
                    painter.text(
                        pos2(rect.center().x, rect.max.y + 4.0),
                        Align2::CENTER_TOP,
                        format!("{}", i + 1),
                        FontId::proportional(11.5),
                        if is_cur { t.accent } else { t.muted },
                    );
                    if r.clicked() {
                        jump = Some(i);
                    }
                }

                // Keep the current page's thumbnail in view as you scroll.
                if self.thumb_followed != self.current {
                    self.thumb_followed = self.current;
                    let cur = self.current.min(n - 1);
                    let top = self.thumb_offsets[cur];
                    let bot = top + tw * self.sizes[cur].1 / self.sizes[cur].0.max(1.0);
                    if top < vp.min.y + 10.0 || bot > vp.max.y - 10.0 {
                        self.thumb_scroll = Some((top - 40.0).max(0.0));
                        ui.ctx().request_repaint();
                    }
                }
            });
        });
        jump
    }

    // ---------------- drawing: pages ----------------

    fn show_pages(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, t: &Theme, o: ViewOpts) {
        if self.sizes.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.36).max(40.0));
                if self.needs_password {
                    self.password_card(ui, t);
                } else if let Some(e) = &self.error {
                    egui::Frame::none()
                        .fill(t.surface)
                        .stroke(Stroke::new(1.0, t.border))
                        .rounding(12.0)
                        .inner_margin(egui::Margin::symmetric(20.0, 14.0))
                        .show(ui, |ui| {
                            ui.set_max_width(440.0);
                            ui.label(egui::RichText::new("Couldn't open this file").size(16.0).strong());
                            ui.add_space(4.0);
                            ui.label(egui::RichText::new(e).color(t.muted));
                            ui.add_space(4.0);
                            ui.label(egui::RichText::new("Close this tab with Ctrl+W.").size(12.0).color(t.muted));
                        });
                } else {
                    ui.add(egui::Spinner::new().size(28.0));
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("Opening\u{2026}").color(t.muted));
                }
            });
            return;
        }
        let ppp = ctx.pixels_per_point();

        let mut sa = egui::ScrollArea::both().auto_shrink([false; 2]).drag_to_scroll(false);
        let target = self.pending_scroll;
        if let Some((off, _)) = target {
            sa = sa.scroll_offset(off);
        }

        let doc_id = self.id;
        ui.push_id(doc_id, |ui| {
            sa.show_viewport(ui, |ui, vp| {
                self.scroll_y = vp.min.y;
                self.scroll_x = vp.min.x;
                self.view_h = vp.height();
                self.view_w = vp.width();
                self.view_rect = ui.clip_rect();

                let n = self.sizes.len();
                let content_w = (self.max_w + 2.0 * GAP).max(vp.width());
                self.content_w = content_w;
                let (area, resp) = ui.allocate_exact_size(vec2(content_w, self.total_h), Sense::click_and_drag());

                let first = self.offsets.partition_point(|&o| o <= vp.min.y).saturating_sub(1).min(n - 1);
                let last = self.offsets.partition_point(|&o| o < vp.max.y).max(first + 1).min(n);
                let probe = vp.min.y + vp.height() * 0.3;
                self.current = self.offsets.partition_point(|&o| o <= probe).saturating_sub(1).min(n - 1);
                let lo = first.saturating_sub(PREFETCH);
                let hi = (last + PREFETCH).min(n);
                self.focus.main_lo.store(lo.saturating_sub(3), Relaxed);
                self.focus.main_hi.store((hi + 3).min(n), Relaxed);

                // A search hit we are jumping to: refine the scroll once its text is known.
                if let Some(ri) = self.reveal {
                    match self.hits.get(ri).copied() {
                        Some(h) => {
                            if let Some(c) = self.chars.get(&h.page).and_then(|cs| cs.get(h.start)) {
                                let y = self.offsets[h.page] + c.rect[1] * self.zoom - vp.height() * 0.3;
                                self.pending_scroll = Some((vec2(vp.min.x, y.max(0.0)), 0));
                                self.reveal = None;
                                ui.ctx().request_repaint();
                            }
                        }
                        None => self.reveal = None,
                    }
                }

                // Request pixels (farthest first so the nearest page is served first).
                let mut order: Vec<usize> = (lo..hi).collect();
                order.sort_by_key(|&i| std::cmp::Reverse(i.abs_diff(first)));
                for i in order {
                    let want = (((self.sizes[i].0 * self.zoom * ppp) / 32.0).round() as u32 * 32).max(32);
                    let fresh = self.textures.get(&i).map_or(false, |x| x.width == want);
                    if !fresh && !self.in_flight.contains(&(i, want, false)) {
                        let req = Req::Render { id: self.id, page: i, width: want, thumb: false };
                        if self.req_tx.send(req).is_ok() {
                            self.in_flight.insert((i, want, false));
                        }
                    }
                }
                // Request text (for selection and highlights) of visible pages.
                for i in first..last {
                    if !self.chars.contains_key(&i) && !self.chars_in_flight.contains(&i) {
                        if self.req_tx.send(Req::Chars { id: self.id, page: i }).is_ok() {
                            self.chars_in_flight.insert(i);
                        }
                    }
                }

                // Mouse: drag to select, click to clear.
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
                }
                if resp.drag_started_by(egui::PointerButton::Primary) {
                    if let Some(p) = ui.ctx().input(|i| i.pointer.press_origin()) {
                        if let Some(h) = self.hit_char(p, area.min, content_w) {
                            self.selection = Some(Selection { anchor: h, head: h });
                        }
                    }
                }
                if resp.dragged_by(egui::PointerButton::Primary) {
                    if let Some(p) = resp.interact_pointer_pos() {
                        if let Some(h) = self.hit_char(p, area.min, content_w) {
                            if let Some(s) = &mut self.selection {
                                s.head = h;
                            }
                        }
                        let clip = ui.clip_rect();
                        if p.y < clip.top() + 30.0 {
                            ui.scroll_with_delta(vec2(0.0, 25.0));
                        } else if p.y > clip.bottom() - 30.0 {
                            ui.scroll_with_delta(vec2(0.0, -25.0));
                        }
                        ui.ctx().request_repaint();
                    }
                }
                if resp.clicked() {
                    self.selection = None;
                }

                // Right-click menu: copy, highlight colours, remove highlight.
                if resp.secondary_clicked() {
                    self.menu_target = resp.interact_pointer_pos().and_then(|p| self.hit_char(p, area.min, content_w));
                }
                let menu_target = self.menu_target;
                let has_sel = self.selection.is_some();
                let on_hl = menu_target.map_or(false, |p| self.highlight_at(p));
                resp.context_menu(|ui| {
                    ui.set_min_width(210.0);
                    if ui.add_enabled(has_sel, egui::Button::new("Copy")).clicked() {
                        self.request_copy();
                        ui.close_menu();
                    }
                    ui.separator();
                    ui.label(egui::RichText::new("Highlight selection").size(12.0).color(t.muted));
                    ui.horizontal(|ui| {
                        for c in 0..4u8 {
                            let (rect, r) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
                            let r = r
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .on_hover_text(HL_NAMES[c as usize]);
                            let col = hl_color(c);
                            let radius = if r.hovered() { 10.5 } else { 9.0 };
                            ui.painter().circle_filled(
                                rect.center(),
                                radius,
                                if has_sel { col } else { with_alpha(col, 70) },
                            );
                            if r.clicked() && has_sel {
                                self.highlight_selection(c);
                                ui.close_menu();
                            }
                        }
                    });
                    if on_hl {
                        ui.separator();
                        if ui.button("Remove highlight").clicked() {
                            if let Some(p) = menu_target {
                                self.remove_highlight_at(p);
                            }
                            ui.close_menu();
                        }
                    }
                });

                // Draw.
                let painter = ui.painter().clone();
                let hit_color = Color32::from_rgba_unmultiplied(255, 213, 0, 120);
                let cur_color = Color32::from_rgba_unmultiplied(255, 130, 0, 170);
                let sel_color = with_alpha(t.accent, 95);
                let shadow_alpha = if t.dark { 26 } else { 12 };
                for i in first..last {
                    let rect = self.page_rect(i, area.min, content_w);
                    // soft layered drop shadow
                    for k in 0..(if o.shadows { 6 } else { 0 }) {
                        let e = k as f32 * 2.0;
                        painter.rect_filled(
                            rect.expand(e).translate(vec2(0.0, 3.0 + e * 0.35)),
                            2.0 + e,
                            Color32::from_black_alpha(shadow_alpha),
                        );
                    }
                    painter.rect_filled(rect, 2.0, t.page_placeholder());
                    let present = self.textures.contains_key(&i);
                    let a = ctx.animate_bool_with_time(egui::Id::new(("pfade", self.id, i)), present, if o.anim { 0.22 } else { 0.0 });
                    if let Some(tex) = self.textures.get(&i) {
                        // A stale-resolution texture is stretched until the sharp one arrives.
                        let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                        let tint = Color32::from_white_alpha((a.clamp(0.0, 1.0) * 255.0) as u8);
                        painter.image(tex.handle.id(), rect, uv, tint);
                    }
                    if let Some(chars) = self.chars.get(&i) {
                        for h in self.highlights.iter().filter(|h| h.page == i) {
                            let col = with_alpha(hl_color(h.color), 120);
                            for r in merged_rects(chars, h.start, h.start.saturating_add(h.len), self.zoom, rect.min) {
                                painter.rect_filled(r, 2.0, col);
                            }
                        }
                        if let Some(hs) = self.hits_by_page.get(&i) {
                            for &(start, len, idx) in hs {
                                let color = if idx == self.hit_cur { cur_color } else { hit_color };
                                for r in merged_rects(chars, start, start + len, self.zoom, rect.min) {
                                    painter.rect_filled(r, 3.0, color);
                                }
                            }
                        }
                        if let Some((from, to)) = self.sel_on_page(i, chars.len()) {
                            for r in merged_rects(chars, from, to, self.zoom, rect.min) {
                                painter.rect_filled(r, 2.0, sel_color);
                            }
                        }
                    }
                }
            });
        });

        // A programmatic scroll is re-applied for a few frames until it
        // sticks (the content may not have been tall enough yet).
        if self.pending_scroll == target {
            if let Some((off, tries)) = target {
                if (self.scroll_y - off.y).abs() < 1.5 || tries >= 5 {
                    self.pending_scroll = None;
                } else {
                    self.pending_scroll = Some((off, tries + 1));
                    ctx.request_repaint();
                }
            }
        }
    }
}

// ======================================================================
// Application: tabs, welcome screen, shortcuts
// ======================================================================

struct App {
    req_tx: Sender<Req>,
    rx: Receiver<Msg>,
    focus: Arc<Focus>,

    tabs: Vec<Doc>,
    active: usize,
    next_id: u64,

    theme: Theme,
    show_thumbs: bool,

    positions: HashMap<String, (f32, f32)>,
    recents: Vec<(PathBuf, String)>,
    last_save: Instant,

    fatal: Option<String>,
    welcome_t0: Option<f64>,

    page_input: String,
    page_err: Option<(Instant, String)>,
    page_box_rect: Rect,
    last_title: String,

    settings: Settings,
    welcome_page: WelcomePage,
    show_settings_window: bool,

    tools: Tools,
    next_job: u64,
    toast: Option<(Instant, String, bool)>,
    instance_rx: Option<Receiver<Vec<PathBuf>>>,
}

impl App {
    fn new(ctx: egui::Context) -> Self {
        let settings = load_settings();
        let theme = Theme::new(settings.dark, settings.accent);
        apply_theme(&ctx, &theme);
        let instance_rx = start_instance_server(ctx.clone());
        let (tx, rx) = mpsc::channel();
        let focus = Arc::new(Focus::default());
        let req_tx = spawn_worker(ctx, tx, focus.clone());
        Self {
            req_tx,
            rx,
            focus,
            tabs: Vec::new(),
            active: 0,
            next_id: 0,
            theme,
            show_thumbs: settings.show_sidebar,
            positions: load_positions(),
            recents: load_recents()
                .into_iter()
                .map(|p| {
                    let k = path_key(&p);
                    (p, k)
                })
                .collect(),
            last_save: Instant::now(),
            fatal: None,
            welcome_t0: None,
            page_input: String::new(),
            page_err: None,
            page_box_rect: Rect::NOTHING,
            last_title: String::new(),
            settings,
            welcome_page: WelcomePage::Home,
            show_settings_window: false,
            tools: Tools::default(),
            next_job: 0,
            toast: None,
            instance_rx,
        }
    }

    // ---------------- tabs ----------------

    fn default_zoom(&self) -> f32 {
        self.settings.default_zoom / 100.0 * 1.33
    }

    fn open(&mut self, path: PathBuf) {
        self.open_inner(path, true);
    }

    fn open_inner(&mut self, path: PathBuf, record_recent: bool) {
        if self.fatal.is_some() {
            return;
        }
        let key = path_key(&path);
        if let Some(i) = self.tabs.iter().position(|d| d.key == key) {
            self.switch_to(i);
            return;
        }
        if record_recent {
            self.push_recent(&path);
        }
        self.next_id += 1;
        let id = self.next_id;
        let zoom = self.default_zoom();
        let mut doc = Doc::new(id, path.clone(), key.clone(), self.req_tx.clone(), self.focus.clone(), zoom);
        doc.restore = if self.settings.remember_position { self.positions.get(&key).copied() } else { None };
        if let Some(d) = self.tabs.get_mut(self.active) {
            d.trim_for_background();
        }
        self.tabs.push(doc);
        self.active = self.tabs.len() - 1;
        self.focus.active.store(id, Relaxed);
        let _ = self.req_tx.send(Req::Open { id, path, password: None });
        self.save_session();
    }

    /// Reopen the tabs from the previous run (the visible tab is loaded first).
    fn restore_session(&mut self) {
        if !self.settings.restore_tabs || self.fatal.is_some() {
            return;
        }
        let (active, paths) = load_session();
        let mut pending: Vec<(u64, PathBuf)> = Vec::new();
        for p in paths {
            let key = path_key(&p);
            if self.tabs.iter().any(|d| d.key == key) {
                continue;
            }
            self.next_id += 1;
            let id = self.next_id;
            let zoom = self.default_zoom();
            let mut doc = Doc::new(id, p.clone(), key.clone(), self.req_tx.clone(), self.focus.clone(), zoom);
            doc.restore = if self.settings.remember_position { self.positions.get(&key).copied() } else { None };
            self.tabs.push(doc);
            pending.push((id, p));
        }
        if self.tabs.is_empty() {
            return;
        }
        self.active = active.min(self.tabs.len() - 1);
        self.activate();
        let act = self.tabs[self.active].id;
        pending.sort_by_key(|(id, _)| if *id == act { 0 } else { 1 });
        for (id, path) in pending {
            let _ = self.req_tx.send(Req::Open { id, path, password: None });
        }
        self.save_session();
    }

    // ---------------- save, tools, notifications ----------------

    fn notify(&mut self, msg: impl Into<String>, error: bool) {
        self.toast = Some((Instant::now(), msg.into(), error));
    }

    /// Save (burn highlights into the PDF) or Save As.
    fn save_active(&mut self, as_new: bool) {
        if self.fatal.is_some() {
            return;
        }
        let Some(d) = self.tabs.get(self.active) else { return };
        let id = d.id;
        let highlights: Vec<(usize, usize, usize, u8)> =
            d.highlights.iter().map(|h| (h.page, h.start, h.len, h.color)).collect();
        let target = if as_new {
            let mut dlg = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).set_file_name(d.title.clone());
            if let Some(dir) = d.path.parent() {
                dlg = dlg.set_directory(dir);
            }
            dlg.save_file()
        } else if d.dirty {
            Some(d.path.clone())
        } else {
            None
        };
        match target {
            Some(path) => {
                let _ = self.req_tx.send(Req::Save { id, path, highlights });
                self.notify("Saving\u{2026}", false);
            }
            None => {
                if !as_new {
                    self.notify("No changes to save.", false);
                }
            }
        }
    }

    fn on_saved(&mut self, id: u64, path: PathBuf, error: Option<String>) {
        if let Some(e) = error {
            self.notify(format!("Save failed: {e}"), true);
            return;
        }
        if let Some(i) = self.tabs.iter().position(|d| d.id == id) {
            let new_key = path_key(&path);
            {
                let d = &mut self.tabs[i];
                d.highlights.clear();
                d.persist_highlights(); // clears the stored highlights for the old key
                d.dirty = false;
                d.path = path.clone();
                d.key = new_key;
                d.title = file_name(&path);
                d.refresh_after_save();
            }
            self.push_recent(&path);
            self.save_session();
        }
        self.notify("Saved", false);
    }

    fn start_job(&mut self, kind: JobKind, label: &str) {
        self.next_job += 1;
        let job = self.next_job;
        self.tools.job = Some((job, label.to_string(), 0, 1));
        let _ = self.req_tx.send(Req::Job { job, kind });
    }

    fn show_toast(&mut self, ctx: &egui::Context, t: &Theme) {
        let Some((t0, msg, is_err)) = self.toast.clone() else { return };
        let el = t0.elapsed().as_secs_f32();
        let life = if is_err { 6.0 } else { 3.2 };
        if el >= life {
            self.toast = None;
            return;
        }
        let a = (el / 0.18).min(1.0).min(((life - el) / 0.4).clamp(0.0, 1.0));
        egui::Area::new(egui::Id::new("toast"))
            .order(egui::Order::Foreground)
            .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -26.0 + (1.0 - a) * 8.0))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(fade(if is_err { t.error_bg() } else { t.surface }, a))
                    .stroke(Stroke::new(1.0, fade(if is_err { ERROR_RED } else { t.border }, a)))
                    .rounding(12.0)
                    .inner_margin(egui::Margin::symmetric(16.0, 10.0))
                    .shadow(egui::epaint::Shadow {
                        offset: vec2(0.0, 6.0),
                        blur: 18.0,
                        spread: 0.0,
                        color: Color32::from_black_alpha((70.0 * a) as u8),
                    })
                    .show(ui, |ui| {
                        ui.set_max_width(440.0);
                        ui.label(
                            egui::RichText::new(msg)
                                .size(13.5)
                                .color(fade(if is_err { t.error_text() } else { t.text }, a)),
                        );
                    });
            });
        ctx.request_repaint();
    }

    /// The Merge / Split / Export window.
    fn show_tools(&mut self, ctx: &egui::Context, t: &Theme) {
        if !self.tools.open {
            return;
        }
        let mut open = true;
        let active_path = self.tabs.get(self.active).map(|d| d.path.clone());
        let open_paths: Vec<PathBuf> = self.tabs.iter().map(|d| d.path.clone()).collect();
        let busy = self.tools.job.is_some();
        egui::Window::new("PDF tools")
            .open(&mut open)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .resizable(false)
            .collapsible(false)
            .default_width(540.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.tools.tab, ToolTab::Merge, "Merge");
                    ui.selectable_value(&mut self.tools.tab, ToolTab::Split, "Split");
                    ui.selectable_value(&mut self.tools.tab, ToolTab::Export, "Export");
                });
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(8.0);

                let tab = self.tools.tab;
                match tab {
                    ToolTab::Merge => {
                        ui.label(
                            egui::RichText::new("Combine several PDFs into one, in the order shown.").color(t.muted),
                        );
                        ui.add_space(8.0);
                        let n = self.tools.merge_files.len();
                        let mut remove: Option<usize> = None;
                        let mut up: Option<usize> = None;
                        let mut down: Option<usize> = None;
                        egui::ScrollArea::vertical().max_height(220.0).auto_shrink([false, true]).show(ui, |ui| {
                            for (i, p) in self.tools.merge_files.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    ui.set_min_height(32.0);
                                    ui.label(format!("{}.  {}", i + 1, shorten(&file_name(p), 46)));
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if icon_button(ui, t, Icon::Close, "Remove", false).clicked() {
                                            remove = Some(i);
                                        }
                                        if i + 1 < n && icon_button(ui, t, Icon::Down, "Move down", false).clicked() {
                                            down = Some(i);
                                        }
                                        if i > 0 && icon_button(ui, t, Icon::Up, "Move up", false).clicked() {
                                            up = Some(i);
                                        }
                                    });
                                });
                            }
                            if n == 0 {
                                ui.label(egui::RichText::new("No files yet.").color(t.muted));
                            }
                        });
                        if let Some(i) = remove {
                            self.tools.merge_files.remove(i);
                        }
                        if let Some(i) = up {
                            self.tools.merge_files.swap(i, i - 1);
                        }
                        if let Some(i) = down {
                            self.tools.merge_files.swap(i, i + 1);
                        }
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("Add files\u{2026}").clicked() {
                                if let Some(ps) = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).pick_files() {
                                    for p in ps {
                                        if !self.tools.merge_files.contains(&p) {
                                            self.tools.merge_files.push(p);
                                        }
                                    }
                                }
                            }
                            if ui.button("Add open tabs").clicked() {
                                for p in &open_paths {
                                    if !self.tools.merge_files.contains(p) {
                                        self.tools.merge_files.push(p.clone());
                                    }
                                }
                            }
                            if ui.button("Clear").clicked() {
                                self.tools.merge_files.clear();
                            }
                        });
                        ui.add_space(12.0);
                        let can = self.tools.merge_files.len() >= 2 && !busy;
                        ui.add_enabled_ui(can, |ui| {
                            if primary_button(ui, t, "Merge into one PDF", 1.0, 0.0).clicked() {
                                if let Some(out) = rfd::FileDialog::new()
                                    .add_filter("PDF", &["pdf"])
                                    .set_file_name("merged.pdf")
                                    .save_file()
                                {
                                    let inputs = self.tools.merge_files.clone();
                                    self.start_job(JobKind::Merge { inputs, output: out }, "Merging\u{2026}");
                                }
                            }
                        });
                    }
                    ToolTab::Split => {
                        let src = source_row(ui, t, &mut self.tools.split_source, &active_path);
                        ui.add_space(8.0);
                        ui.radio_value(
                            &mut self.tools.split_mode,
                            SplitMode::Extract,
                            "Extract pages into a new PDF",
                        );
                        ui.radio_value(&mut self.tools.split_mode, SplitMode::EveryN, "Split every N pages");
                        ui.radio_value(&mut self.tools.split_mode, SplitMode::Single, "Split into single pages");
                        ui.add_space(8.0);
                        match self.tools.split_mode {
                            SplitMode::Extract => {
                                ui.horizontal(|ui| {
                                    ui.label("Pages");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.tools.split_ranges)
                                            .hint_text("e.g. 1-3, 5, 8-")
                                            .desired_width(260.0),
                                    );
                                });
                            }
                            SplitMode::EveryN => {
                                ui.horizontal(|ui| {
                                    ui.label("Pages per file");
                                    ui.add(egui::DragValue::new(&mut self.tools.split_every).speed(0.2));
                                });
                                self.tools.split_every = self.tools.split_every.clamp(1, 1000);
                            }
                            SplitMode::Single => {}
                        }
                        ui.add_space(12.0);
                        let can = src.is_some() && !busy;
                        ui.add_enabled_ui(can, |ui| {
                            if primary_button(ui, t, "Split", 1.0, 0.0).clicked() {
                                if let Some(source) = src.clone() {
                                    let stem = source
                                        .file_stem()
                                        .map(|s| s.to_string_lossy().to_string())
                                        .unwrap_or_else(|| "document".to_string());
                                    let mode = self.tools.split_mode;
                                    if mode == SplitMode::Extract {
                                        if let Some(out) = rfd::FileDialog::new()
                                            .add_filter("PDF", &["pdf"])
                                            .set_file_name(format!("{stem}_extract.pdf"))
                                            .save_file()
                                        {
                                            let ranges = self.tools.split_ranges.clone();
                                            self.start_job(
                                                JobKind::Extract { source, ranges, output: out },
                                                "Extracting pages\u{2026}",
                                            );
                                        }
                                    } else {
                                        let every = if mode == SplitMode::Single {
                                            1
                                        } else {
                                            self.tools.split_every.max(1) as usize
                                        };
                                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                                            self.start_job(
                                                JobKind::SplitEvery { source, every, dir },
                                                "Splitting\u{2026}",
                                            );
                                        }
                                    }
                                }
                            }
                        });
                    }
                    ToolTab::Export => {
                        let src = source_row(ui, t, &mut self.tools.export_source, &active_path);
                        ui.add_space(8.0);
                        ui.radio_value(&mut self.tools.export_format, ExportFormat::Png, "Images (PNG, one per page)");
                        ui.radio_value(&mut self.tools.export_format, ExportFormat::Text, "Plain text (.txt)");
                        ui.radio_value(
                            &mut self.tools.export_format,
                            ExportFormat::Word,
                            "Word document (.docx), text only",
                        );
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.label("Pages");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.tools.export_pages)
                                    .hint_text("All pages, or e.g. 1-3, 5")
                                    .desired_width(260.0),
                            );
                        });
                        let format = self.tools.export_format;
                        if format == ExportFormat::Png {
                            ui.horizontal(|ui| {
                                ui.label("Resolution");
                                for d in [72u32, 150, 300] {
                                    ui.radio_value(&mut self.tools.export_dpi, d, format!("{d} dpi"));
                                }
                            });
                        }
                        if format == ExportFormat::Word {
                            ui.label(
                                egui::RichText::new(
                                    "Only the text is exported. Layout, images and fonts are not preserved.",
                                )
                                .size(12.0)
                                .color(t.muted),
                            );
                        }
                        ui.add_space(12.0);
                        let can = src.is_some() && !busy;
                        ui.add_enabled_ui(can, |ui| {
                            if primary_button(ui, t, "Export", 1.0, 0.0).clicked() {
                                if let Some(source) = src.clone() {
                                    let stem = source
                                        .file_stem()
                                        .map(|s| s.to_string_lossy().to_string())
                                        .unwrap_or_else(|| "document".to_string());
                                    let ranges = self.tools.export_pages.clone();
                                    match format {
                                        ExportFormat::Png => {
                                            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                                                let dpi = self.tools.export_dpi;
                                                self.start_job(
                                                    JobKind::ExportImages { source, ranges, dpi, dir },
                                                    "Exporting images\u{2026}",
                                                );
                                            }
                                        }
                                        ExportFormat::Text => {
                                            if let Some(out) = rfd::FileDialog::new()
                                                .add_filter("Text", &["txt"])
                                                .set_file_name(format!("{stem}.txt"))
                                                .save_file()
                                            {
                                                self.start_job(
                                                    JobKind::ExportText { source, ranges, output: out },
                                                    "Exporting text\u{2026}",
                                                );
                                            }
                                        }
                                        ExportFormat::Word => {
                                            if let Some(out) = rfd::FileDialog::new()
                                                .add_filter("Word document", &["docx"])
                                                .set_file_name(format!("{stem}.docx"))
                                                .save_file()
                                            {
                                                self.start_job(
                                                    JobKind::ExportWord { source, ranges, output: out },
                                                    "Exporting to Word\u{2026}",
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        });
                    }
                }

                if let Some((_, label, done, total)) = self.tools.job.clone() {
                    ui.add_space(12.0);
                    ui.label(egui::RichText::new(label).color(t.muted));
                    ui.add(egui::ProgressBar::new(done as f32 / total.max(1) as f32).show_percentage());
                    ctx.request_repaint();
                }
            });
        if !open {
            self.tools.open = false;
        }
    }

    fn save_session(&self) {
        let paths: Vec<PathBuf> = self.tabs.iter().map(|d| d.path.clone()).collect();
        write_session(self.active, &paths);
    }

    fn activate(&mut self) {
        if let Some(d) = self.tabs.get_mut(self.active) {
            self.focus.active.store(d.id, Relaxed);
            d.pending_scroll = Some((vec2(d.scroll_x, d.scroll_y), 0));
            d.thumb_followed = usize::MAX;
        } else {
            self.focus.active.store(0, Relaxed);
        }
    }

    fn switch_to(&mut self, i: usize) {
        if i >= self.tabs.len() || i == self.active {
            return;
        }
        if let Some(d) = self.tabs.get_mut(self.active) {
            d.trim_for_background();
        }
        self.active = i;
        self.activate();
        self.save_session();
    }

    fn close_tab(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        self.save_positions_now();
        let d = self.tabs.remove(i);
        let _ = self.req_tx.send(Req::Close { id: d.id });
        if self.tabs.is_empty() {
            self.active = 0;
            self.activate();
        } else if i < self.active {
            self.active -= 1;
        } else if i == self.active {
            self.active = self.active.min(self.tabs.len() - 1);
            self.activate();
        }
        self.save_session();
    }

    fn push_recent(&mut self, path: &Path) {
        if !self.settings.keep_recents {
            return;
        }
        self.recents.retain(|(p, _)| p != path);
        self.recents.insert(0, (path.to_path_buf(), path_key(path)));
        self.recents.truncate(MAX_RECENT);
        self.persist_recents();
    }

    fn persist_recents(&self) {
        let list: Vec<PathBuf> = self.recents.iter().map(|(p, _)| p.clone()).collect();
        save_recents(&list);
    }

    fn clear_recents(&mut self) {
        self.recents.clear();
        self.persist_recents();
    }

    /// Re-derive the theme from the settings, apply it and save everything.
    fn apply_settings(&mut self, ctx: &egui::Context) {
        self.theme = Theme::new(self.settings.dark, self.settings.accent);
        apply_theme(ctx, &self.theme);
        if !self.settings.keep_recents && !self.recents.is_empty() {
            self.clear_recents();
        }
        save_settings(&self.settings);
        ctx.request_repaint();
    }

    fn save_positions_now(&mut self) {
        if !self.settings.remember_position {
            self.last_save = Instant::now();
            return;
        }
        let mut changed = false;
        for d in &self.tabs {
            if let Some((pos, z)) = d.pos_state() {
                let old = self.positions.get(&d.key).copied();
                let differs = old.map_or(true, |(p, zz)| (p - pos).abs() > 0.01 || (zz - z).abs() > 0.001);
                if differs {
                    self.positions.insert(d.key.clone(), (pos, z));
                    changed = true;
                }
            }
        }
        if changed {
            save_positions(&self.positions);
        }
        self.last_save = Instant::now();
    }

    // ---------------- worker messages ----------------

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Fatal(e) => {
                    for d in &mut self.tabs {
                        d.error = Some(e.clone());
                    }
                    self.fatal = Some(e);
                }
                Msg::Copied(text) => ctx.copy_text(text),
                Msg::Saved { id, path, error } => self.on_saved(id, path, error),
                Msg::JobProgress { job, done, total } => {
                    if let Some(j) = &mut self.tools.job {
                        if j.0 == job {
                            j.2 = done;
                            j.3 = total;
                        }
                    }
                }
                Msg::JobDone { job, result } => {
                    if self.tools.job.as_ref().map_or(false, |j| j.0 == job) {
                        self.tools.job = None;
                    }
                    match result {
                        Ok(m) => self.notify(m, false),
                        Err(e) => self.notify(e, true),
                    }
                }
                other => {
                    let id = msg_id(&other);
                    if let Some(d) = self.tabs.iter_mut().find(|d| d.id == id) {
                        d.on_msg(ctx, other);
                    }
                }
            }
        }
        for d in &mut self.tabs {
            d.evict();
        }
    }

    // ---------------- input ----------------

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::O)) {
            if let Some(ps) = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).pick_files() {
                for p in ps {
                    self.open(p);
                }
            }
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::W)) {
            let a = self.active;
            self.close_tab(a);
        }
        let prev_tab = egui::Modifiers { ctrl: true, shift: true, ..Default::default() };
        if ctx.input_mut(|i| i.consume_key(prev_tab, egui::Key::Tab)) {
            if self.tabs.len() > 1 {
                let n = self.tabs.len();
                self.switch_to((self.active + n - 1) % n);
            }
        } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab)) {
            if self.tabs.len() > 1 {
                let n = self.tabs.len();
                self.switch_to((self.active + 1) % n);
            }
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)) {
            if let Some(d) = self.tabs.get_mut(self.active) {
                d.show_search = true;
                d.focus_search = true;
            }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if let Some(d) = self.tabs.get_mut(self.active) {
                if d.show_search {
                    d.close_search();
                } else {
                    d.selection = None;
                }
            }
        }
        let copy = ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
        if copy && !ctx.wants_keyboard_input() {
            if let Some(d) = self.tabs.get(self.active) {
                if let Some(s) = &d.selection {
                    let _ = self.req_tx.send(Req::Copy { id: d.id, start: s.anchor, end: s.head });
                }
            }
        }
        // zoom and navigation keys
        let zoom_in = ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Equals));
        let zoom_out = ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Minus));
        let zoom_reset = ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Num0));
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::G)) {
            ctx.memory_mut(|m| m.request_focus(egui::Id::new("page_box")));
        }
        let default_zoom = self.default_zoom();
        let typing = ctx.wants_keyboard_input();
        let (page_down, page_up, line_down, line_up, home, end) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::PageDown) || (i.key_pressed(egui::Key::Space) && !i.modifiers.shift),
                i.key_pressed(egui::Key::PageUp) || (i.key_pressed(egui::Key::Space) && i.modifiers.shift),
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::Home),
                i.key_pressed(egui::Key::End),
            )
        });
        if let Some(d) = self.tabs.get_mut(self.active) {
            if !d.sizes.is_empty() {
                let c = d.view_h / 2.0;
                if zoom_in {
                    let z = d.zoom * 1.15;
                    d.set_zoom(z, c);
                }
                if zoom_out {
                    let z = d.zoom / 1.15;
                    d.set_zoom(z, c);
                }
                if zoom_reset {
                    d.set_zoom(default_zoom, c);
                }
                if !typing {
                    let max_y = (d.total_h - d.view_h).max(0.0);
                    let page = d.view_h * 0.9;
                    let target = if page_down {
                        Some(d.scroll_y + page)
                    } else if page_up {
                        Some(d.scroll_y - page)
                    } else if line_down {
                        Some(d.scroll_y + 90.0)
                    } else if line_up {
                        Some(d.scroll_y - 90.0)
                    } else if home {
                        Some(0.0)
                    } else if end {
                        Some(max_y)
                    } else {
                        None
                    };
                    if let Some(y) = target {
                        d.pending_scroll = Some((vec2(d.scroll_x, y.clamp(0.0, max_y)), 0));
                    }
                }
            }
        }
        let save_as = egui::Modifiers { command: true, shift: true, ..Default::default() };
        if ctx.input_mut(|i| i.consume_key(save_as, egui::Key::S)) {
            self.save_active(true);
        } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S)) {
            self.save_active(false);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::H)) {
            if let Some(d) = self.tabs.get_mut(self.active) {
                d.highlight_selection(0);
            }
        }
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        for p in dropped {
            self.open(p);
        }
    }

    // ---------------- welcome screen ----------------

    /// 0.0 -> 1.0 progress of the welcome fade/slide-in (always 1.0 with animations off).
    fn welcome_e(&mut self, ctx: &egui::Context) -> f32 {
        let now = ctx.input(|i| i.time);
        let t0 = *self.welcome_t0.get_or_insert(now);
        if !self.settings.animations {
            return 1.0;
        }
        let p = (((now - t0) / 0.55).clamp(0.0, 1.0)) as f32;
        if p < 1.0 {
            ctx.request_repaint();
        }
        1.0 - (1.0 - p).powi(3)
    }

    /// Left navigation rail of the welcome screen. Returns files to open.
    fn show_nav(&mut self, ctx: &egui::Context, t: &Theme) -> Vec<PathBuf> {
        let mut opens: Vec<PathBuf> = Vec::new();
        let e = self.welcome_e(ctx);
        egui::SidePanel::left("nav")
            .exact_width(244.0)
            .resizable(false)
            .frame(egui::Frame::none().fill(t.panel).inner_margin(egui::Margin::symmetric(16.0, 24.0)))
            .show(ctx, |ui| {
                // brand
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(38.0, 38.0), Sense::hover());
                    {
                        let p = ui.painter();
                        p.rect_filled(rect, 11.0, fade(t.accent, e));
                        let fg = if t.dark { Color32::from_rgb(15, 16, 20) } else { Color32::WHITE };
                        let page = Rect::from_center_size(rect.center(), vec2(14.0, 18.0));
                        p.rect_stroke(page, 3.0, Stroke::new(1.8, fade(fg, e)));
                        for k in 0..2 {
                            let y = page.top() + 6.0 + k as f32 * 4.5;
                            p.line_segment(
                                [pos2(page.left() + 3.5, y), pos2(page.right() - 3.5, y)],
                                Stroke::new(1.5, fade(fg, e)),
                            );
                        }
                    }
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new("SwiftPDF").size(19.0).strong().color(fade(t.text, e)));
                        ui.label(egui::RichText::new("Instant PDF reading").size(11.5).color(fade(t.muted, e)));
                    });
                });

                ui.add_space(22.0);
                let w = ui.available_width();
                if primary_button(ui, t, "Open PDF", e, w).clicked() {
                    if let Some(ps) = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).pick_files() {
                        opens.extend(ps);
                    }
                }
                ui.add_space(18.0);

                let n = self.recents.len();
                if nav_item(ui, t, Icon::Home, "Home", self.welcome_page == WelcomePage::Home, None, e).clicked() {
                    self.welcome_page = WelcomePage::Home;
                }
                ui.add_space(2.0);
                let badge = if n > 0 { Some(n) } else { None };
                if nav_item(ui, t, Icon::Clock, "Recent", self.welcome_page == WelcomePage::Recent, badge, e)
                    .clicked()
                {
                    self.welcome_page = WelcomePage::Recent;
                }
                ui.add_space(2.0);
                if nav_item(ui, t, Icon::Gear, "Settings", self.welcome_page == WelcomePage::Settings, None, e)
                    .clicked()
                {
                    self.welcome_page = WelcomePage::Settings;
                }
                ui.add_space(2.0);
                if nav_item(ui, t, Icon::Tools, "PDF tools", self.tools.open, None, e).clicked() {
                    self.tools.open = true;
                }

                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    let label = if t.dark { "Light mode" } else { "Dark mode" };
                    if nav_item(ui, t, Icon::Contrast, label, false, None, e).clicked() {
                        self.settings.dark = !self.settings.dark;
                        self.apply_settings(ctx);
                    }
                });
            });
        opens
    }

    /// Content area of the welcome screen (Home / Recent / Settings page).
    fn show_welcome(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, t: &Theme) -> Vec<PathBuf> {
        let mut opens: Vec<PathBuf> = Vec::new();
        let e = self.welcome_e(ctx);
        let lift = (1.0 - e) * 14.0;
        let side = ((ui.available_width() - 780.0) / 2.0).max(32.0);
        let page = self.welcome_page;
        egui::Frame::none()
            .inner_margin(egui::Margin { left: side, right: side, top: 36.0 + lift, bottom: 16.0 })
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match page {
                    WelcomePage::Home => self.page_home(ctx, ui, t, e, &mut opens),
                    WelcomePage::Recent => self.page_recent(ui, t, e, &mut opens),
                    WelcomePage::Settings => {
                        page_title(ui, t, "Settings", "Changes are saved automatically.", e);
                        self.settings_ui(ctx, ui, t, e);
                    }
                });
            });
        opens
    }

    fn page_home(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, t: &Theme, e: f32, opens: &mut Vec<PathBuf>) {
        page_title(ui, t, "Open a PDF", "Drop files below or browse. Every PDF opens in its own tab.", e);

        if let Some(msg) = self.fatal.clone() {
            egui::Frame::none()
                .fill(fade(t.surface, e))
                .stroke(Stroke::new(1.0, fade(ERROR_RED, e)))
                .rounding(14.0)
                .inner_margin(egui::Margin::symmetric(22.0, 18.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        egui::RichText::new("PDF engine not found").size(17.0).strong().color(fade(ERROR_RED, e)),
                    );
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(msg).color(fade(t.muted, e)));
                });
        } else {
            let w = ui.available_width();
            let (rect, resp) = ui.allocate_exact_size(vec2(w, 190.0), Sense::click());
            let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
            let dragging = ctx.input(|i| !i.raw.hovered_files.is_empty());
            let hot = resp.hovered() || dragging;
            {
                let p = ui.painter();
                p.rect_filled(rect, 16.0, fade(if hot { mix(t.surface, t.accent, 0.10) } else { t.surface }, e));
                p.rect_stroke(rect, 16.0, Stroke::new(1.0, fade(t.border, e)));
                dashed_rect(p, rect.shrink(12.0), fade(if hot { t.accent } else { t.muted }, e * 0.6));
                big_page_icon(p, pos2(rect.center().x, rect.center().y - 46.0), fade(t.accent, e));
                p.text(
                    pos2(rect.center().x, rect.center().y - 6.0),
                    Align2::CENTER_CENTER,
                    if dragging { "Release to open" } else { "Drop PDFs here" },
                    FontId::proportional(17.0),
                    fade(t.text, e),
                );
            }
            let br = Rect::from_center_size(pos2(rect.center().x, rect.center().y + 42.0), vec2(160.0, 42.0));
            let btn = primary_button_at(ui, t, "Browse files", br, e).clicked();
            if btn || resp.clicked() {
                if let Some(ps) = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).pick_files() {
                    opens.extend(ps);
                }
            }
        }

        if !self.recents.is_empty() {
            ui.add_space(30.0);
            let w = ui.available_width();
            let (hr, _) = ui.allocate_exact_size(vec2(w, 26.0), Sense::hover());
            ui.painter().text(
                hr.left_center(),
                Align2::LEFT_CENTER,
                "Continue reading",
                FontId::proportional(16.0),
                fade(t.text, e),
            );
            let lr = Rect::from_min_size(pos2(hr.right() - 70.0, hr.top()), vec2(70.0, 26.0));
            if link_at(ui, t, "See all", lr, e).clicked() {
                self.welcome_page = WelcomePage::Recent;
            }
            ui.add_space(10.0);
            let list: Vec<(PathBuf, String)> = self.recents.iter().take(3).cloned().collect();
            for (path, key) in list {
                let info = self.positions.get(&key).map(|&(p, _)| format!("Page {}", p.floor() as usize + 1));
                let (open, _) = recent_row(ui, t, &path, info, e, w, false);
                if open {
                    opens.push(path.clone());
                }
                ui.add_space(6.0);
            }
        }

        ui.add_space(30.0);
        ui.label(egui::RichText::new("SHORTCUTS").size(11.5).color(fade(t.muted, e)));
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(16.0, 10.0);
            for (k, d) in [
                ("Ctrl+O", "Open"),
                ("Ctrl+F", "Find"),
                ("Ctrl+W", "Close tab"),
                ("Ctrl+G", "Go to page"),
                ("Ctrl+H", "Highlight"),
                ("Right-click", "Menu"),
                ("Ctrl+Tab", "Next tab"),
                ("Ctrl+Scroll", "Zoom"),
            ] {
                key_chip(ui, t, k, d, e);
            }
        });
        ui.add_space(24.0);
    }

    fn page_recent(&mut self, ui: &mut egui::Ui, t: &Theme, e: f32, opens: &mut Vec<PathBuf>) {
        let w = ui.available_width();
        let (hr, _) = ui.allocate_exact_size(vec2(w, 40.0), Sense::hover());
        ui.painter().text(hr.left_center(), Align2::LEFT_CENTER, "Recent", FontId::proportional(28.0), fade(t.text, e));
        let mut clear = false;
        if !self.recents.is_empty() {
            let br = Rect::from_min_size(pos2(hr.right() - 116.0, hr.center().y - 17.0), vec2(116.0, 34.0));
            if ghost_button_at(ui, t, Some(Icon::Trash), "Clear all", br, e, true).clicked() {
                clear = true;
            }
        }
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new("Files you opened recently. Clearing only empties this list.")
                .size(14.0)
                .color(fade(t.muted, e)),
        );
        ui.add_space(20.0);

        if self.recents.is_empty() {
            let (rect, _) = ui.allocate_exact_size(vec2(w, 160.0), Sense::hover());
            let p = ui.painter();
            p.rect_filled(rect, 16.0, fade(t.surface, e));
            p.rect_stroke(rect, 16.0, Stroke::new(1.0, fade(t.border, e)));
            paint_icon(p, Icon::Clock, pos2(rect.center().x, rect.center().y - 32.0), fade(t.muted, e));
            p.text(
                pos2(rect.center().x, rect.center().y + 2.0),
                Align2::CENTER_CENTER,
                "Nothing here yet",
                FontId::proportional(16.0),
                fade(t.text, e),
            );
            p.text(
                pos2(rect.center().x, rect.center().y + 26.0),
                Align2::CENTER_CENTER,
                "Files you open will show up here.",
                FontId::proportional(13.0),
                fade(t.muted, e),
            );
        } else {
            let list = self.recents.clone();
            let mut remove: Option<usize> = None;
            for (i, (path, key)) in list.iter().enumerate() {
                let info = self.positions.get(key).map(|&(p, _)| format!("Page {}", p.floor() as usize + 1));
                let (open, rm) = recent_row(ui, t, path, info, e, w, true);
                if open {
                    opens.push(path.clone());
                }
                if rm {
                    remove = Some(i);
                }
                ui.add_space(6.0);
            }
            if let Some(i) = remove {
                self.recents.remove(i);
                self.persist_recents();
            }
        }
        if clear {
            self.clear_recents();
        }
        ui.add_space(24.0);
    }

    /// The settings page. Used on the welcome screen and in the Settings window.
    fn settings_ui(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, t: &Theme, a: f32) {
        let mut changed = false;
        let mut reset = false;

        section_label(ui, t, "STARTUP", a);
        card(ui, t, a, |ui| {
            changed |= setting_row(
                ui,
                t,
                "Restore tabs on startup",
                "Reopen the PDFs you had open last time.",
                &mut self.settings.restore_tabs,
                true,
                a,
            );
            changed |= setting_row(
                ui,
                t,
                "Keep recent files",
                "Show recently opened files on the welcome screen.",
                &mut self.settings.keep_recents,
                true,
                a,
            );
            changed |= setting_row(
                ui,
                t,
                "Remember reading position",
                "Return to your last page and zoom in each PDF.",
                &mut self.settings.remember_position,
                false,
                a,
            );
        });

        section_label(ui, t, "APPEARANCE", a);
        card(ui, t, a, |ui| {
            changed |= setting_row(
                ui,
                t,
                "Dark theme",
                "Easier on the eyes in low light.",
                &mut self.settings.dark,
                true,
                a,
            );
            changed |= swatch_row(ui, t, &mut self.settings.accent, true, a);
            changed |= setting_row(
                ui,
                t,
                "Smooth animations",
                "Fade pages and panels in as they appear.",
                &mut self.settings.animations,
                true,
                a,
            );
            changed |= setting_row(
                ui,
                t,
                "Page shadows",
                "Soft shadows around each page.",
                &mut self.settings.page_shadows,
                false,
                a,
            );
        });

        section_label(ui, t, "READING", a);
        card(ui, t, a, |ui| {
            changed |= setting_row(
                ui,
                t,
                "Show page thumbnails",
                "Open the thumbnail sidebar by default (next launch).",
                &mut self.settings.show_sidebar,
                true,
                a,
            );
            changed |= slider_row(
                ui,
                t,
                "Default zoom",
                "Zoom used when a PDF opens.",
                &mut self.settings.default_zoom,
                50.0,
                300.0,
                false,
                a,
            );
        });

        section_label(ui, t, "ABOUT", a);
        card(ui, t, a, |ui| {
            let w = ui.available_width();
            let (rect, _) = ui.allocate_exact_size(vec2(w, 78.0), Sense::hover());
            {
                let p = ui.painter();
                p.text(
                    pos2(rect.left() + 16.0, rect.center().y - 20.0),
                    Align2::LEFT_CENTER,
                    format!("SwiftPDF  {}", env!("CARGO_PKG_VERSION")),
                    FontId::proportional(14.5),
                    fade(t.text, a),
                );
                p.text(
                    pos2(rect.left() + 16.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    "A fast, lightweight PDF reader built with Rust, egui and PDFium.",
                    FontId::proportional(12.0),
                    fade(t.muted, a),
                );
                let dir = config_dir().map(|d| d.to_string_lossy().to_string()).unwrap_or_default();
                p.text(
                    pos2(rect.left() + 16.0, rect.center().y + 20.0),
                    Align2::LEFT_CENTER,
                    format!("Data: {}", shorten_left(&dir, 58)),
                    FontId::proportional(11.5),
                    fade(t.muted, a),
                );
            }
            let br = Rect::from_center_size(pos2(rect.right() - 16.0 - 70.0, rect.center().y - 20.0), vec2(140.0, 30.0));
            if ghost_button_at(ui, t, None, "Open data folder", br, a, false).clicked() {
                if let Some(d) = config_dir() {
                    let _ = std::fs::create_dir_all(&d);
                    open_in_explorer(&d);
                }
            }
        });

        ui.add_space(20.0);
        let w = ui.available_width();
        let (fr, _) = ui.allocate_exact_size(vec2(w, 34.0), Sense::hover());
        let br = Rect::from_min_size(pos2(fr.right() - 160.0, fr.top()), vec2(160.0, 34.0));
        if ghost_button_at(ui, t, None, "Reset to defaults", br, a, false).clicked() {
            reset = true;
        }
        ui.add_space(24.0);

        if reset {
            self.settings = Settings::default();
            changed = true;
        }
        if changed {
            self.apply_settings(ctx);
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
        let forwarded: Vec<Vec<PathBuf>> = match &self.instance_rx {
            Some(rx) => rx.try_iter().collect(),
            None => Vec::new(),
        };
        for list in forwarded {
            for p in list {
                self.open(p);
            }
        }
        self.handle_shortcuts(ctx);
        let t = self.theme;
        let has_tabs = !self.tabs.is_empty();
        let o = ViewOpts { anim: self.settings.animations, shadows: self.settings.page_shadows };

        // Ctrl+scroll / pinch zoom, anchored on the cursor
        let z = ctx.input(|i| i.zoom_delta());
        if z != 1.0 {
            let hover = ctx.input(|i| i.pointer.hover_pos());
            if let Some(d) = self.tabs.get_mut(self.active) {
                if !d.sizes.is_empty() {
                    let off = match hover {
                        Some(p) if d.view_rect.contains(p) => p.y - d.view_rect.top(),
                        _ => d.view_h / 2.0,
                    };
                    let nz = d.zoom * z;
                    d.set_zoom(nz, off);
                }
            }
        }

        let mut open_reqs: Vec<PathBuf> = Vec::new();
        let mut close_req: Option<usize> = None;
        let mut switch_req: Option<usize> = None;
        let mut toggle_theme = false;
        let mut open_settings = false;
        let mut open_tools = false;
        let mut save_req: Option<bool> = None;

        if has_tabs {
            self.welcome_t0 = None;

            // ---------- tab strip ----------
            egui::TopBottomPanel::top("tabs")
                .frame(
                    egui::Frame::none()
                        .fill(t.bg)
                        .inner_margin(egui::Margin { left: 8.0, right: 8.0, top: 6.0, bottom: 0.0 }),
                )
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        for (i, d) in self.tabs.iter().enumerate() {
                            let active = i == self.active;
                            let title = shorten(&d.title, 26);
                            let col = if active { t.text } else { t.muted };
                            let galley = ui.painter().layout_no_wrap(title, FontId::proportional(13.0), col);
                            let w = (galley.size().x + if d.dirty { 66.0 } else { 56.0 }).clamp(100.0, 230.0);
                            let (rect, resp) = ui.allocate_exact_size(vec2(w, 34.0), Sense::click());
                            let resp = resp
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .on_hover_text(d.path.display().to_string());

                            let close_rect =
                                Rect::from_center_size(pos2(rect.right() - 17.0, rect.center().y), vec2(20.0, 20.0));
                            let cr = ui.interact(close_rect, ui.id().with(("tabx", d.id)), Sense::click());

                            let painter = ui.painter();
                            let fill = if active {
                                t.panel
                            } else if resp.hovered() {
                                mix(t.bg, t.panel, 0.55)
                            } else {
                                Color32::TRANSPARENT
                            };
                            let rounding = egui::Rounding { nw: 9.0, ne: 9.0, sw: 0.0, se: 0.0 };
                            painter.rect_filled(rect, rounding, fill);
                            if active {
                                painter.rect_filled(
                                    Rect::from_min_max(
                                        pos2(rect.left() + 12.0, rect.top()),
                                        pos2(rect.right() - 12.0, rect.top() + 2.0),
                                    ),
                                    1.0,
                                    t.accent,
                                );
                            }
                            let tx = rect.left() + if d.dirty { 24.0 } else { 14.0 };
                            if d.dirty {
                                painter.circle_filled(pos2(rect.left() + 15.0, rect.center().y), 3.5, t.accent);
                            }
                            painter.galley(pos2(tx, rect.center().y - galley.size().y / 2.0), galley, col);
                            if active || resp.hovered() || cr.hovered() {
                                if cr.hovered() {
                                    painter.rect_filled(close_rect, 6.0, t.hover);
                                }
                                paint_icon(
                                    painter,
                                    Icon::Close,
                                    close_rect.center(),
                                    if cr.hovered() { t.text } else { t.muted },
                                );
                            }
                            if cr.clicked() || resp.middle_clicked() {
                                close_req = Some(i);
                            } else if resp.clicked() {
                                switch_req = Some(i);
                            }
                        }
                        if icon_button(ui, &t, Icon::Plus, "Open PDF (Ctrl+O)", false).clicked() {
                            if let Some(ps) = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).pick_files() {
                                open_reqs.extend(ps);
                            }
                        }
                    });
                });

            // ---------- toolbar ----------
            egui::TopBottomPanel::top("bar")
                .frame(egui::Frame::none().fill(t.panel).inner_margin(egui::Margin::symmetric(12.0, 8.0)))
                .show(ctx, |ui| {
                    let active = self.active;
                    let Some(doc) = self.tabs.get_mut(active) else { return };
                    ui.horizontal(|ui| {
                        if icon_button(ui, &t, Icon::Sidebar, "Pages", self.show_thumbs).clicked() {
                            self.show_thumbs = !self.show_thumbs;
                        }
                        if icon_button(ui, &t, Icon::Open, "Open PDF (Ctrl+O)", false).clicked() {
                            if let Some(ps) = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).pick_files() {
                                open_reqs.extend(ps);
                            }
                        }
                        let sv = icon_button(
                            ui,
                            &t,
                            Icon::Save,
                            "Save (Ctrl+S) - right-click for Save as",
                            doc.dirty,
                        );
                        if sv.clicked() {
                            save_req = Some(false);
                        }
                        sv.context_menu(|ui| {
                            if ui.button("Save").clicked() {
                                save_req = Some(false);
                                ui.close_menu();
                            }
                            if ui.button("Save as\u{2026}").clicked() {
                                save_req = Some(true);
                                ui.close_menu();
                            }
                        });
                        ui.add_space(8.0);
                        ui.label(egui::RichText::new(shorten(&doc.title, 60)).size(14.0));
                        if let Some(e) = &doc.error {
                            ui.add_space(8.0);
                            ui.label(egui::RichText::new(shorten(e, 70)).color(ERROR_RED));
                        }

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if icon_button(ui, &t, Icon::Gear, "Settings", false).clicked() {
                                open_settings = true;
                            }
                            if icon_button(ui, &t, Icon::Contrast, "Light / dark", false).clicked() {
                                toggle_theme = true;
                            }
                            if icon_button(ui, &t, Icon::Tools, "PDF tools", false).clicked() {
                                open_tools = true;
                            }
                            if icon_button(ui, &t, Icon::Search, "Find (Ctrl+F)", doc.show_search).clicked() {
                                if doc.show_search {
                                    doc.close_search();
                                } else {
                                    doc.show_search = true;
                                    doc.focus_search = true;
                                }
                            }
                            if !doc.sizes.is_empty() {
                                vsep(ui, &t);
                                if icon_button(ui, &t, Icon::Fit, "Fit width", false).clicked() {
                                    let w0 = doc.sizes.iter().map(|s| s.0).fold(1.0_f32, f32::max);
                                    let tz = (doc.view_w - 2.0 * GAP - 14.0) / w0;
                                    let c = doc.view_h / 2.0;
                                    doc.set_zoom(tz, c);
                                }
                                if icon_button(ui, &t, Icon::Plus, "Zoom in", false).clicked() {
                                    let nz = doc.zoom * 1.15;
                                    let c = doc.view_h / 2.0;
                                    doc.set_zoom(nz, c);
                                }
                                ui.add_sized(
                                    vec2(48.0, 32.0),
                                    egui::Label::new(egui::RichText::new(format!("{:.0}%", doc.zoom / 1.33 * 100.0))),
                                );
                                if icon_button(ui, &t, Icon::Minus, "Zoom out", false).clicked() {
                                    let nz = doc.zoom / 1.15;
                                    let c = doc.view_h / 2.0;
                                    doc.set_zoom(nz, c);
                                }
                                vsep(ui, &t);

                                // "[ 12 ] / 340": the first number is an editable box.
                                let total = doc.sizes.len();
                                ui.label(egui::RichText::new(format!("{total}")).color(t.muted));
                                ui.label(egui::RichText::new("/").color(t.muted));
                                let id = egui::Id::new("page_box");
                                let has_focus = ui.ctx().memory(|m| m.has_focus(id));
                                if !has_focus {
                                    self.page_input = format!("{}", doc.current + 1);
                                }
                                let r = ui.add(
                                    egui::TextEdit::singleline(&mut self.page_input)
                                        .id(id)
                                        .desired_width(44.0)
                                        .horizontal_align(egui::Align::Center)
                                        .char_limit(7),
                                );
                                self.page_box_rect = r.rect;
                                let err_active = self
                                    .page_err
                                    .as_ref()
                                    .map_or(false, |(t0, _)| t0.elapsed() < Duration::from_millis(2300));
                                if err_active {
                                    ui.painter().rect_stroke(r.rect.expand(1.0), 6.0, Stroke::new(1.5, ERROR_RED));
                                }
                                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                    match self.page_input.trim().parse::<usize>() {
                                        Ok(n) if n >= 1 && n <= total => doc.jump_to_page(n - 1),
                                        _ => {
                                            self.page_err = Some((
                                                Instant::now(),
                                                format!("Enter a page number from 1 to {total}"),
                                            ));
                                            r.request_focus();
                                        }
                                    }
                                }
                            }
                        });
                    });
                });

            // ---------- resizable thumbnail sidebar ----------
            let active = self.active;
            let ready = self.tabs.get(active).map_or(false, |d| !d.sizes.is_empty());
            if self.show_thumbs && ready {
                let mut jump: Option<usize> = None;
                egui::SidePanel::left("thumbs")
                    .resizable(true)
                    .default_width(150.0)
                    .width_range(110.0..=340.0)
                    .frame(egui::Frame::none().fill(t.panel).inner_margin(egui::Margin::symmetric(10.0, 0.0)))
                    .show(ctx, |ui| {
                        if let Some(doc) = self.tabs.get_mut(active) {
                            jump = doc.show_sidebar(ctx, ui, &t, o);
                        }
                    });
                if let Some(i) = jump {
                    if let Some(d) = self.tabs.get_mut(active) {
                        d.jump_to_page(i);
                    }
                }
            } else if let Some(d) = self.tabs.get(active) {
                d.focus.thumb_lo.store(0, Relaxed);
                d.focus.thumb_hi.store(0, Relaxed);
            }
        } else {
            let reqs = self.show_nav(ctx, &t);
            open_reqs.extend(reqs);
        }

        // ---------- pages / welcome ----------
        egui::CentralPanel::default().frame(egui::Frame::none().fill(t.bg)).show(ctx, |ui| {
            if has_tabs {
                let active = self.active;
                if let Some(doc) = self.tabs.get_mut(active) {
                    doc.show_pages(ctx, ui, &t, o);
                }
            } else {
                let reqs = self.show_welcome(ctx, ui, &t);
                open_reqs.extend(reqs);
            }
        });

        // ---------- floating search card (slides in) ----------
        if has_tabs {
            let active = self.active;
            if let Some(doc) = self.tabs.get_mut(active) {
                let k = ctx.animate_bool_with_time(egui::Id::new("search_slide"), doc.show_search, if o.anim { 0.15 } else { 0.0 });
                if k > 0.0 {
                    egui::Area::new(egui::Id::new("search_overlay"))
                        .order(egui::Order::Foreground)
                        .anchor(Align2::RIGHT_TOP, vec2(-24.0, 112.0 - (1.0 - k) * 12.0))
                        .show(ctx, |ui| {
                            egui::Frame::none()
                                .fill(t.surface)
                                .stroke(Stroke::new(1.0, t.border))
                                .rounding(12.0)
                                .inner_margin(egui::Margin::symmetric(12.0, 6.0))
                                .shadow(egui::epaint::Shadow {
                                    offset: vec2(0.0, 8.0),
                                    blur: 24.0,
                                    spread: 0.0,
                                    color: Color32::from_black_alpha(90),
                                })
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        let r = ui.add(
                                            egui::TextEdit::singleline(&mut doc.query)
                                                .hint_text("Find in document")
                                                .desired_width(210.0)
                                                .frame(false),
                                        );
                                        if doc.focus_search {
                                            r.request_focus();
                                            doc.focus_search = false;
                                        }
                                        if r.changed() {
                                            doc.on_query_changed();
                                        }
                                        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                            let shift = ui.input(|i| i.modifiers.shift);
                                            doc.step_hit(if shift { -1 } else { 1 });
                                            r.request_focus();
                                        }

                                        let (done, total) = doc.index_progress;
                                        let status = if doc.query.trim().is_empty() {
                                            String::new()
                                        } else if !doc.hits.is_empty() {
                                            format!("{} of {}", doc.hit_cur + 1, doc.hits.len())
                                        } else if total > 0 && done < total {
                                            format!("indexing {}%", done * 100 / total)
                                        } else {
                                            "No results".to_string()
                                        };
                                        if !status.is_empty() {
                                            ui.label(egui::RichText::new(status).size(12.5).color(t.muted));
                                        }
                                        if icon_button(ui, &t, Icon::Up, "Previous (Shift+Enter)", false).clicked() {
                                            doc.step_hit(-1);
                                        }
                                        if icon_button(ui, &t, Icon::Down, "Next (Enter)", false).clicked() {
                                            doc.step_hit(1);
                                        }
                                        if icon_button(ui, &t, Icon::Close, "Close (Esc)", false).clicked() {
                                            doc.close_search();
                                        }
                                    });
                                });
                        });
                }
            }
        }

        // ---------- page-number error box (fades out) ----------
        if let Some((t0, msg)) = self.page_err.clone() {
            let el = t0.elapsed().as_secs_f32();
            if el < 2.4 {
                let a = ((2.4 - el) / 0.4).clamp(0.0, 1.0);
                let pos = pos2(self.page_box_rect.right(), self.page_box_rect.bottom() + 8.0);
                egui::Area::new(egui::Id::new("page_err"))
                    .order(egui::Order::Foreground)
                    .pivot(Align2::RIGHT_TOP)
                    .fixed_pos(pos)
                    .show(ctx, |ui| {
                        egui::Frame::none()
                            .fill(fade(t.error_bg(), a))
                            .stroke(Stroke::new(1.0, fade(ERROR_RED, a)))
                            .rounding(8.0)
                            .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                            .show(ui, |ui| {
                                ui.label(egui::RichText::new(msg).size(12.5).color(fade(t.error_text(), a)));
                            });
                    });
                ctx.request_repaint();
            } else {
                self.page_err = None;
            }
        }

        // ---------- settings window (while PDFs are open) ----------
        if open_settings {
            self.show_settings_window = true;
        }
        if self.show_settings_window && has_tabs {
            let mut open = true;
            egui::Window::new("Settings")
                .open(&mut open)
                .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
                .resizable(false)
                .collapsible(false)
                .default_width(540.0)
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().max_height(540.0).show(ui, |ui| {
                        self.settings_ui(ctx, ui, &t, 1.0);
                    });
                });
            if !open {
                self.show_settings_window = false;
            }
        }

        // ---------- theme toggle, drop overlay ----------
        if toggle_theme {
            self.settings.dark = !self.settings.dark;
            self.apply_settings(ctx);
        }
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let rect = ctx.screen_rect().shrink(16.0);
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("drop")));
            painter.rect_filled(rect, 16.0, with_alpha(t.accent, 40));
            painter.rect_stroke(rect, 16.0, Stroke::new(2.0, t.accent));
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "Drop PDF to open",
                FontId::proportional(24.0),
                t.text,
            );
        }

        // ---------- PDF tools window, toast, save ----------
        if open_tools {
            self.tools.open = true;
        }
        self.show_tools(ctx, &t);
        self.show_toast(ctx, &t);
        if let Some(as_new) = save_req {
            self.save_active(as_new);
        }

        // ---------- apply deferred actions ----------
        if let Some(i) = switch_req {
            self.switch_to(i);
        }
        if let Some(i) = close_req {
            self.close_tab(i);
        }
        for p in open_reqs {
            self.open(p);
        }

        // window title follows the active tab
        let title = match self.tabs.get(self.active) {
            Some(d) => format!("{}{} \u{2014} SwiftPDF", if d.dirty { "* " } else { "" }, d.title),
            None => "SwiftPDF".to_string(),
        };
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }

        if ctx.input(|i| i.viewport().close_requested()) || self.last_save.elapsed() >= Duration::from_secs(1) {
            self.save_positions_now();
        }
    }
}

fn main() -> eframe::Result<()> {
    install_panic_hook();
    let args: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    // A second launch hands its files to the running window and exits.
    if forward_to_running_instance(&args) {
        return Ok(());
    }
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1100.0, 800.0])
        .with_min_inner_size([640.0, 420.0]);
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "SwiftPDF",
        options,
        Box::new(move |cc| {
            let mut app = App::new(cc.egui_ctx.clone());
            if args.is_empty() {
                app.restore_session();
            } else {
                for p in args {
                    app.open(p);
                }
            }
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<CharInfo> {
        s.chars().map(|ch| CharInfo { ch, rect: [0.0; 4] }).collect()
    }

    #[test]
    fn ranges_basic() {
        assert_eq!(parse_ranges("", 3).unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_ranges("1-2, 5", 6).unwrap(), vec![0, 1, 4]);
        assert_eq!(parse_ranges("3-", 5).unwrap(), vec![2, 3, 4]);
    }

    #[test]
    fn ranges_dedupe() {
        assert_eq!(parse_ranges("1,1-2", 4).unwrap(), vec![0, 1]);
    }

    #[test]
    fn ranges_errors() {
        assert!(parse_ranges("0", 5).is_err());
        assert!(parse_ranges("6", 5).is_err());
        assert!(parse_ranges("3-2", 5).is_err());
        assert!(parse_ranges("x", 5).is_err());
    }

    #[test]
    fn search_is_case_insensitive() {
        let idx = vec![Some(build_index(&chars("Hello World")))];
        let (hits, complete) = run_search(&idx, "WORLD");
        assert!(complete);
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].start, hits[0].len), (6, 5));
    }

    #[test]
    fn search_crosses_line_breaks() {
        let idx = vec![Some(build_index(&chars("hello\r\nworld")))];
        let (hits, _) = run_search(&idx, "hello world");
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].start, hits[0].len), (0, 12));
    }

    #[test]
    fn search_reports_incomplete_index() {
        let idx = vec![None, Some(build_index(&chars("abc")))];
        let (hits, complete) = run_search(&idx, "abc");
        assert!(!complete);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn shortening() {
        assert_eq!(shorten("abcdef", 4), "abc\u{2026}");
        assert_eq!(shorten("abc", 4), "abc");
        assert_eq!(shorten_left("abcdef", 4), "\u{2026}def");
    }

    #[test]
    fn highlight_rects_merge_per_line() {
        let same_line = vec![
            CharInfo { ch: 'a', rect: [0.0, 0.0, 5.0, 10.0] },
            CharInfo { ch: 'b', rect: [5.0, 0.0, 10.0, 10.0] },
        ];
        assert_eq!(merged_rects(&same_line, 0, 2, 1.0, pos2(0.0, 0.0)).len(), 1);
        let two_lines = vec![
            CharInfo { ch: 'a', rect: [0.0, 0.0, 5.0, 10.0] },
            CharInfo { ch: 'b', rect: [0.0, 20.0, 5.0, 30.0] },
        ];
        assert_eq!(merged_rects(&two_lines, 0, 2, 1.0, pos2(0.0, 0.0)).len(), 2);
    }

    #[test]
    fn fade_scales_alpha() {
        assert_eq!(fade(Color32::WHITE, 0.0).a(), 0);
        assert_eq!(fade(Color32::WHITE, 1.0).a(), 255);
    }
}
