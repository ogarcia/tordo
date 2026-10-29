//! `tordo import` — import catalog entries from a Rucio database.
//!
//! Reads the `shared_files` table of a Rucio SQLite database, groups the shared
//! video files into works (one film or one series), parses the release details
//! out of each file name, resolves every work to its TMDB id and writes the
//! matching `content/<type>-<id>.toml` entries the generator expects.
//!
//! Files already linked from an existing entry are skipped, so the import only
//! adds what is missing. Works whose TMDB match is uncertain are "doubtful": by
//! default the user is asked about them at the end; with --auto they are skipped.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use clap::{Args, ValueEnum};
use difflib::sequencematcher::SequenceMatcher;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use unicode_normalization::UnicodeNormalization;

// --------------------------------------------------------------------------- //
// CLI
// --------------------------------------------------------------------------- //

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum DateMode {
    /// The file's own share time (default).
    Added,
    /// Today.
    Today,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum ReportFormat {
    /// Human-readable.
    Txt,
    /// Full detail incl. releases/links, for tools.
    Json,
}

#[derive(Args)]
pub struct ImportArgs {
    /// Rucio database (default: ask, offering the daemon's own rucio.db or a
    /// ./rucio.db; $RUCIOD_DB_PATH/$RUCIOD_BASE_DIR and $XDG_DATA_HOME/rucio
    /// are honored).
    #[arg(long)]
    pub db: Option<String>,

    /// Catalog content directory where the .toml entries live (default: ask,
    /// offering a content/ under CATALOG_DIR, the current dir, or one up).
    #[arg(long)]
    pub content: Option<String>,

    /// TMDB v3 API key (query param); or set TMDB_API_KEY.
    #[arg(long, env = "TMDB_API_KEY")]
    pub tmdb_key: Option<String>,

    /// TMDB v4 read token (Bearer); or set TMDB_READ_TOKEN.
    #[arg(long, env = "TMDB_READ_TOKEN")]
    pub tmdb_token: Option<String>,

    /// Search/metadata language.
    #[arg(long, default_value = "es-ES")]
    pub lang: String,

    /// Treat DIR as a series library (repeatable; overrides name detection).
    #[arg(long, value_name = "DIR")]
    pub series: Vec<String>,

    /// Treat DIR as a films library (repeatable).
    #[arg(long, value_name = "DIR")]
    pub movies: Vec<String>,

    /// Treat DIR as a mixed/downloads folder (repeatable).
    #[arg(long, value_name = "DIR")]
    pub mixed: Vec<String>,

    /// Per-release date: the file's share time (default) or today.
    #[arg(long, value_enum, default_value = "added")]
    pub date: DateMode,

    /// Non-interactive: skip doubtful works instead of asking.
    #[arg(long)]
    pub auto: bool,

    /// Offline: group and parse only, no TMDB, no files written.
    #[arg(long)]
    pub plan: bool,

    /// Offline: extract the parsed works (links + release details) to PATH as
    /// JSON, no TMDB. Hand the file to someone who runs --from-works.
    #[arg(long, value_name = "PATH")]
    pub export: Option<String>,

    /// Import from a works file written by --export instead of a Rucio DB.
    #[arg(long, value_name = "PATH")]
    pub from_works: Option<String>,

    /// Query TMDB and build the report, but write no .toml files.
    #[arg(long)]
    pub dry_run: bool,

    /// Process at most N works (0 = all).
    #[arg(long, default_value_t = 0)]
    pub limit: usize,

    /// Also write a report file to PATH (off unless given).
    #[arg(long, value_name = "PATH")]
    pub report: Option<String>,

    /// Format of --report: txt (human) or json. Only used with --report.
    #[arg(long, value_enum, default_value = "txt")]
    pub report_format: ReportFormat,
}

// --------------------------------------------------------------------------- //
// Terminal colour (honours NO_COLOR / FORCE_COLOR; off when stdout is not a TTY)
// --------------------------------------------------------------------------- //

static COLOR: LazyLock<bool> = LazyLock::new(|| {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if std::env::var_os("FORCE_COLOR").is_some() {
        return true;
    }
    io::stdout().is_terminal()
});

fn paint(s: &str, code: &str) -> String {
    if *COLOR {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}
fn bold(s: &str) -> String {
    paint(s, "1")
}
fn dim(s: &str) -> String {
    paint(s, "2")
}
fn red(s: &str) -> String {
    paint(s, "31")
}
fn green(s: &str) -> String {
    paint(s, "32")
}
fn yellow(s: &str) -> String {
    paint(s, "33")
}
fn cyan(s: &str) -> String {
    paint(s, "36")
}

fn prompt_line(prompt: &str) -> String {
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return String::new();
    }
    line.trim().to_string()
}

// --------------------------------------------------------------------------- //
// DB / content resolution
// --------------------------------------------------------------------------- //

fn default_db_path() -> PathBuf {
    if let Ok(p) = std::env::var("RUCIOD_DB_PATH") {
        return PathBuf::from(p);
    }
    if let Ok(base) = std::env::var("RUCIOD_BASE_DIR") {
        return Path::new(&base).join("rucio.db");
    }
    let xdg = std::env::var("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share")
    });
    xdg.join("rucio").join("rucio.db")
}

fn first_existing(paths: &[PathBuf]) -> Option<PathBuf> {
    paths.iter().find(|p| p.exists()).cloned()
}

fn resolve_db(explicit: &Option<String>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(absolute(p));
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let default = first_existing(&[default_db_path(), cwd.join("rucio.db")]);
    if !io::stdin().is_terminal() {
        return Ok(default.unwrap_or_else(default_db_path));
    }
    let hint = match &default {
        Some(d) => format!(" [{}]", d.display()),
        None => " (no database found nearby; type a path)".to_string(),
    };
    let ans = prompt_line(&format!("{}{}: ", bold("Rucio database"), dim(&hint)));
    if !ans.is_empty() {
        return Ok(absolute(&ans));
    }
    default.ok_or_else(|| anyhow::anyhow!("no database given"))
}

fn resolve_content(explicit: &Option<String>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(absolute(p));
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut candidates = Vec::new();
    if let Ok(cd) = std::env::var("CATALOG_DIR") {
        candidates.push(Path::new(&cd).join("content"));
    }
    candidates.push(cwd.join("content"));
    candidates.push(cwd.join("..").join("content"));
    let default = first_existing(&candidates);
    if !io::stdin().is_terminal() {
        return Ok(default.unwrap_or_else(|| candidates[0].clone()));
    }
    if let Some(d) = &default {
        let ans = prompt_line(&format!(
            "{}{}: ",
            bold("Content directory"),
            dim(&format!(" [{}]", d.display()))
        ));
        return Ok(if ans.is_empty() { d.clone() } else { absolute(&ans) });
    }
    let ans = prompt_line(&format!(
        "{}{}: ",
        bold("Content directory"),
        dim(" (none found; type a path to create)")
    ));
    if ans.is_empty() {
        bail!("no content directory given");
    }
    Ok(absolute(&ans))
}

/// Expand a leading `~` and make the path absolute (without touching the disk).
fn absolute(p: &str) -> PathBuf {
    let expanded = if let Some(rest) = p.strip_prefix("~/") {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest)
    } else {
        PathBuf::from(p)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().unwrap_or_default().join(expanded)
    }
}

// --------------------------------------------------------------------------- //
// File-name parsing
// --------------------------------------------------------------------------- //

const VIDEO_EXT: &[&str] = &["mkv", "mp4", "avi", "mpg", "mpeg", "m4v"];

static EP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:^|[^0-9a-z])(?:s(\d{1,2})e(\d{1,3})|(\d{1,2})x(\d{2,3}))").unwrap());
static YEAR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\(((?:19|20)\d{2})\)").unwrap());
static TN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^T(\d{1,2})$").unwrap());
static RES_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(2160p|1080p|720p|480p)\b").unwrap());
static YEAR_END_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\((\d{4})\)\s*$").unwrap());
static PARENS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\([^)]*\)").unwrap());
static NAME_IN_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[?&]name=([^&]*)").unwrap());
static HASH_IN_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"rucio:([0-9a-fA-F]{64})").unwrap());
static ID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(tv|movie)-(\d+)$").unwrap());
static NON_ALNUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9]+").unwrap());
static WS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

static RES_ALIASES: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(|| {
    vec![(Regex::new(r"(?i)\b(4k|uhd|2160)\b").unwrap(), "2160p")]
});

fn rx_list(pairs: &[(&'static str, &'static str)]) -> Vec<(Regex, &'static str)> {
    // Labels are &'static (string literals from the tables below).
    pairs
        .iter()
        .map(|(p, _)| Regex::new(p).unwrap())
        .zip(pairs.iter().map(|(_, l)| *l))
        .collect()
}

static SOURCES: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(|| {
    rx_list(&[
        (r"(?i)\bbd[\s._-]?remux\b", "BDRemux"),
        (r"(?i)\bweb[\s._-]?dl\b", "WEB-DL"),
        (r"(?i)\bweb[\s._-]?rip\b", "WEBRip"),
        (r"(?i)\bblu[\s._-]?ray\b", "BluRay"),
        (r"(?i)\bbd[\s._-]?rip\b", "BDRip"),
        (r"(?i)\bbr[\s._-]?rip\b", "BDRip"),
        (r"(?i)\bmicro[\s._-]?hd\b", "MicroHD"),
        (r"(?i)\bhd[\s._-]?rip\b", "HDRip"),
        (r"(?i)\bhdtv\b", "HDTV"),
        (r"(?i)\bdvd[\s._-]?rip\b", "DVDRip"),
        (r"(?i)\bhdit\b", "HDTV"),
        (r"(?i)\bweb\b", "WEB-DL"),
    ])
});
static VCODECS: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(|| {
    rx_list(&[
        (r"(?i)\b(x265|h\.?265|hevc)\b", "x265"),
        (r"(?i)\b(x264|h\.?264|avc)\b", "x264"),
        (r"(?i)\bav1\b", "AV1"),
        (r"(?i)\bvp9\b", "VP9"),
        (r"(?i)\b(xvid|divx)\b", "XviD"),
    ])
});
static ACODECS: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(|| {
    rx_list(&[
        (r"(?i)\beac3\b|\bdd\+|\bddp\b", "EAC3"),
        (r"(?i)\bdts[\s._-]?hd\b", "DTS-HD"),
        (r"(?i)\bdts[:._-]?x\b", "DTS:X"),
        (r"(?i)\bdts\b", "DTS"),
        (r"(?i)\bac3\b", "AC3"),
        (r"(?i)\baac\b", "AAC"),
        (r"(?i)\bflac\b", "FLAC"),
        (r"(?i)\bopus\b", "Opus"),
        (r"(?i)\bmp3\b", "MP3"),
    ])
});
static HDRS: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(|| {
    rx_list(&[
        (r"(?i)\bhdr10\+\b|\bhdr10plus\b", "HDR10+"),
        (r"(?i)\bhdr10\b", "HDR10"),
        (r"(?i)\b(dv|dovi|dolby[\s._-]?vision)\b", "DV"),
        (r"(?i)\bhlg\b", "HLG"),
        (r"(?i)\bhdr\b", "HDR10"),
    ])
});
static ATMOS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\batmos\b").unwrap());

fn res_rank(res: &Option<String>) -> i32 {
    match res.as_deref() {
        Some("2160p") => 0,
        Some("1080p") => 1,
        Some("720p") => 2,
        Some("480p") => 3,
        _ => 9,
    }
}

static LANGMAP: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    [
        ("spanish", "es-ES"), ("spa", "es-ES"), ("espanol", "es-ES"), ("castellano", "es-ES"),
        ("cast", "es-ES"), ("esp", "es-ES"),
        ("latino", "es-MX"), ("lat", "es-MX"),
        ("english", "en-US"), ("eng", "en-US"), ("ingles", "en-US"), ("en", "en-US"),
        ("japanese", "ja-JP"), ("jap", "ja-JP"), ("japones", "ja-JP"), ("jpn", "ja-JP"),
        ("catala", "ca-ES"), ("catalan", "ca-ES"), ("cat", "ca-ES"),
        ("frances", "fr-FR"), ("french", "fr-FR"), ("fra", "fr-FR"),
        ("aleman", "de-DE"), ("german", "de-DE"), ("deu", "de-DE"),
        ("italiano", "it-IT"), ("italian", "it-IT"), ("ita", "it-IT"),
        ("euskera", "eu-ES"), ("gallego", "gl-ES"),
    ]
    .into_iter()
    .collect()
});
const SUB_WORDS: &[&str] = &["sub", "subs", "subtitulos", "subtitles", "subtitulado"];

fn ext_of(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e).unwrap_or("").to_lowercase()
}

fn strip_accents(s: &str) -> String {
    s.nfkd().filter(|c| !(0x300..=0x36F).contains(&(*c as u32))).collect()
}

fn norm_title(s: &str) -> String {
    let s = strip_accents(s).to_lowercase();
    let s = NON_ALNUM.replace_all(&s, " ");
    WS_RE.replace_all(s.trim(), " ").to_string()
}

fn episode_of(name: &str) -> (Option<i64>, Option<i64>) {
    let Some(c) = EP_RE.captures(name) else {
        return (None, None);
    };
    if let Some(s) = c.get(1) {
        (Some(s.as_str().parse().unwrap()), Some(c[2].parse().unwrap()))
    } else {
        (Some(c[3].parse().unwrap()), Some(c[4].parse().unwrap()))
    }
}

fn clean_title(raw: &str) -> String {
    let t = raw.replace('.', " ").replace('_', " ");
    let t = WS_RE.replace_all(&t, " ");
    let t = t.trim_matches([' ', '-', ',', '.']);
    Regex::new(r"\s*-\s*$").unwrap().replace(t, "").trim().to_string()
}

fn cut_tech(seg: &str, hard: bool) -> String {
    let mut cut = seg.len();
    if let Some(m) = YEAR_RE.find(seg) {
        cut = cut.min(m.start());
    }
    if let Some(i) = seg.find('[') {
        cut = cut.min(i);
    }
    // A resolution or video codec never appears in a real title: always safe.
    let always: Vec<&Regex> = std::iter::once(&*RES_RE)
        .chain(RES_ALIASES.iter().map(|(r, _)| r))
        .chain(VCODECS.iter().map(|(r, _)| r))
        .collect();
    for rx in always {
        if let Some(m) = rx.find(seg) {
            cut = cut.min(m.start());
        }
    }
    if hard {
        if let Some(i) = seg.find('(') {
            cut = cut.min(i);
        }
        for (rx, _) in SOURCES.iter() {
            if let Some(m) = rx.find(seg) {
                cut = cut.min(m.start());
            }
        }
    }
    seg[..cut].to_string()
}

fn title_from_filename(name: &str) -> String {
    let base = name.rsplit_once('.').map(|(b, _)| b).unwrap_or(name);
    if let Some(m) = EP_RE.find(base) {
        if m.start() > 0 {
            return clean_title(&cut_tech(&base[..m.start()], false));
        }
        return clean_title(&cut_tech(&base[m.end()..], true));
    }
    clean_title(&cut_tech(base, false))
}

#[derive(Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct Release {
    resolution: Option<String>,
    source: Option<String>,
    vcodec: Option<String>,
    acodec: Option<String>,
    hdr: Vec<String>,
    container: String,
    audio: Vec<String>,
    subs: Vec<String>,
}

fn first_match<'a>(pairs: &'a [(Regex, &'a str)], name: &str) -> Option<String> {
    pairs.iter().find(|(r, _)| r.is_match(name)).map(|(_, l)| l.to_string())
}

fn parse_release(name: &str) -> Release {
    let mut res = RES_RE.captures(name).map(|c| c[1].to_lowercase());
    if res.is_none() {
        for (rx, val) in RES_ALIASES.iter() {
            if rx.is_match(name) {
                res = Some(val.to_string());
                break;
            }
        }
    }
    let mut acodec = first_match(&ACODECS, name);
    if let Some(a) = &acodec {
        if ATMOS_RE.is_match(name) {
            acodec = Some(format!("{a} Atmos"));
        }
    }
    let mut hdr = Vec::new();
    for (rx, v) in HDRS.iter() {
        if rx.is_match(name) && !hdr.contains(&v.to_string()) {
            hdr.push(v.to_string());
        }
    }
    let (audio, subs) = parse_langs(name);
    Release {
        resolution: res,
        source: first_match(&SOURCES, name),
        vcodec: first_match(&VCODECS, name),
        acodec,
        hdr,
        container: ext_of(name),
        audio,
        subs,
    }
}

fn parse_langs(name: &str) -> (Vec<String>, Vec<String>) {
    let base = name.rsplit_once('.').map(|(b, _)| b).unwrap_or(name);
    let lowered = strip_accents(base).to_lowercase();
    let tokens = Regex::new(r"[.\s_\-\[\]()/]+").unwrap();
    let mut audio: Vec<String> = Vec::new();
    let mut subs: Vec<String> = Vec::new();
    let mut generic_sub = false;
    for tok in tokens.split(&lowered) {
        if tok.is_empty() {
            continue;
        }
        if SUB_WORDS.contains(&tok) {
            generic_sub = true;
            continue;
        }
        if tok.ends_with("subs") || tok.ends_with("sub") {
            let stem = if tok.ends_with("subs") { &tok[..tok.len() - 4] } else { &tok[..tok.len() - 3] };
            if let Some(code) = LANGMAP.get(stem) {
                if !subs.contains(&code.to_string()) {
                    subs.push(code.to_string());
                }
                continue;
            }
        }
        if let Some(code) = LANGMAP.get(tok) {
            if !audio.contains(&code.to_string()) {
                audio.push(code.to_string());
            }
        }
    }
    if generic_sub && subs.is_empty() {
        subs = vec!["es-ES".to_string()];
    }
    (audio, subs)
}

// --------------------------------------------------------------------------- //
// Grouping shared files into works
// --------------------------------------------------------------------------- //

#[derive(Clone, Serialize, Deserialize)]
struct FileRec {
    hash: String,
    name: String,
    size: i64,
    added_at: i64,
    season: Option<i64>,
    episode: Option<i64>,
    rel: Release,
}

#[derive(Serialize, Deserialize)]
struct Work {
    media_type: String,
    title: String,
    year: Option<String>,
    files: Vec<FileRec>,
    #[serde(default)]
    warnings: BTreeSet<String>,
}

impl Work {
    fn seasons(&self) -> Vec<i64> {
        let mut s: Vec<i64> = self.files.iter().filter_map(|f| f.season).collect();
        s.sort_unstable();
        s.dedup();
        s
    }
    fn newest(&self) -> i64 {
        self.files.iter().map(|f| f.added_at).max().unwrap_or(0)
    }
}

/// On-disk envelope for the offline link hand-off (`--export` / `--from-works`):
/// the parsed works with their links and release details, before any TMDB
/// resolution. Lets a contributor without a TMDB credential extract links from a
/// Rucio DB and pass them to someone who finishes the import.
const WORKS_FORMAT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct WorksFile {
    version: u32,
    works: Vec<Work>,
}

/// Load works from a `--export` file, dropping files already linked in this
/// catalog (and works thereby left empty), so a re-import stays idempotent.
fn load_works_file(path: &str, linked: &BTreeSet<String>) -> Result<Vec<Work>> {
    let text = fs::read_to_string(path)
        .with_context(|| red(&format!("error: cannot read works file: {path}")))?;
    let doc: WorksFile = serde_json::from_str(&text)
        .with_context(|| red(&format!("error: not a valid works file: {path}")))?;
    if doc.version != WORKS_FORMAT_VERSION {
        bail!(red(&format!(
            "error: works file version {} is not supported (expected {WORKS_FORMAT_VERSION}).",
            doc.version
        )));
    }
    let mut works = doc.works;
    for w in &mut works {
        w.files.retain(|f| !linked.contains(&f.hash));
    }
    works.retain(|w| !w.files.is_empty());
    Ok(works)
}

type Row = (String, String, i64, String, i64); // hash, name, size, path, added_at

fn path_parts(path: &str) -> Vec<String> {
    path.split('/').filter(|p| !p.is_empty()).map(|s| s.to_string()).collect()
}

fn norm_comp(s: &str) -> String {
    strip_accents(s).to_lowercase().trim().to_string()
}

static ROLE_KEYWORDS: LazyLock<Vec<(&'static str, Vec<&'static str>)>> = LazyLock::new(|| {
    vec![
        ("series", vec!["series", "serie", "tv", "show", "shows", "tvshows", "tvseries", "seriestv"]),
        ("movies", vec!["peliculas", "pelicula", "pelis", "peli", "movies", "movie", "films", "film", "cine"]),
        ("mixed", vec!["complete", "completo", "completos", "completado", "descargas", "downloads", "download",
                       "incoming", "temp", "varios", "mixed", "torrents", "emule", "finished", "done", "unsorted"]),
    ]
});
const ROLES: [&str; 3] = ["series", "movies", "mixed"];

/// (role, keyword_index). Overrides (longest prefix wins) first, then directory
/// names against ROLE_KEYWORDS, else "mixed".
fn classify_path(parts: &[String], overrides: &[(Vec<String>, String)]) -> (String, Option<usize>) {
    for (pref, role) in overrides {
        if !pref.is_empty() && parts.len() >= pref.len() && &parts[..pref.len()] == pref.as_slice() {
            return (role.clone(), Some(pref.len() - 1));
        }
    }
    let comps = &parts[..parts.len().saturating_sub(1)];
    for (i, comp) in comps.iter().enumerate() {
        let c = norm_comp(comp);
        for (role, kws) in ROLE_KEYWORDS.iter() {
            if kws.contains(&c.as_str()) {
                return (role.to_string(), Some(i));
            }
        }
    }
    for (i, comp) in comps.iter().enumerate() {
        let toks: BTreeSet<String> = Regex::new(r"[^a-z0-9]+")
            .unwrap()
            .split(&norm_comp(comp))
            .filter(|t| !t.is_empty())
            .map(|s| s.to_string())
            .collect();
        for (role, kws) in ROLE_KEYWORDS.iter() {
            if kws.iter().any(|k| toks.contains(*k)) {
                return (role.to_string(), Some(i));
            }
        }
    }
    ("mixed".to_string(), None)
}

fn zone_dir(parts: &[String], kw_idx: Option<usize>) -> String {
    let n = kw_idx.map(|i| i + 1).unwrap_or(1);
    format!("/{}", parts[..n.min(parts.len())].join("/"))
}

struct BuildOut {
    works: Vec<Work>,
    skipped_linked: usize,
    skipped_nonvideo: usize,
    role_dirs: HashMap<String, BTreeSet<String>>,
}

fn build_works(rows: &[Row], linked: &BTreeSet<String>, overrides: &[(Vec<String>, String)]) -> BuildOut {
    let mut works: Vec<Work> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut skipped_linked = 0;
    let mut skipped_nonvideo = 0;
    let mut role_dirs: HashMap<String, BTreeSet<String>> = HashMap::new();

    for (h, name, size, path, added_at) in rows {
        if !VIDEO_EXT.contains(&ext_of(name).as_str()) {
            skipped_nonvideo += 1;
            continue;
        }
        if linked.contains(h) {
            skipped_linked += 1;
            continue;
        }
        let parts = path_parts(path);
        if parts.is_empty() {
            continue;
        }
        let (role, kw_idx) = classify_path(&parts, overrides);
        role_dirs.entry(role.clone()).or_default().insert(zone_dir(&parts, kw_idx));
        let (mut season, mut episode) = episode_of(name);
        let rel = parse_release(name);

        let (key, media_type, title, year): (String, &str, String, Option<String>);
        if role == "series" {
            let mut show: Option<String> = None;
            if let Some(ki) = kw_idx {
                if ki + 1 <= parts.len().saturating_sub(2) {
                    show = Some(parts[ki + 1].clone());
                    if ki + 2 <= parts.len().saturating_sub(2) {
                        if let Some(c) = TN_RE.captures(&parts[ki + 2]) {
                            season = Some(c[1].parse().unwrap());
                        }
                    }
                }
            }
            if let Some(show) = show {
                let yr = YEAR_END_RE.captures(&show).map(|c| c[1].to_string());
                let t = clean_title(&Regex::new(r"\s*\(\d{4}\)\s*$").unwrap().replace(&show, ""));
                let n = norm_title(&t);
                key = format!("tv\t{}", if n.is_empty() { format!("folder:{}", parts[..parts.len() - 1].join("/")) } else { n });
                (media_type, title, year) = ("tv", t, yr);
            } else {
                let mut t = title_from_filename(name);
                if t.is_empty() {
                    t = clean_title(&parts[parts.len() - 2]);
                }
                let yr = YEAR_RE.captures(name).map(|c| c[1].to_string());
                let n = norm_title(&t);
                key = format!("tv\t{}", if n.is_empty() { format!("folder:{}", parts[..parts.len() - 1].join("/")) } else { n });
                (media_type, title, year) = ("tv", t, yr);
            }
        } else if role == "movies" {
            let t = title_from_filename(name);
            let yr = YEAR_RE.captures(name).map(|c| c[1].to_string());
            key = format!("movie\t{}\t{}", norm_title(&t), yr.clone().unwrap_or_default());
            (media_type, title, year) = ("movie", t, yr);
            season = None;
            episode = None;
        } else if season.is_some() {
            let mut t = title_from_filename(name);
            if t.is_empty() && parts.len() >= 2 {
                let folder = Regex::new(r"\s*\(\d{4}\)\s*$").unwrap().replace(&parts[parts.len() - 2], "").to_string();
                let folder = Regex::new(r"(?i)\s+T\d+\s*$").unwrap().replace(&folder, "").to_string();
                t = clean_title(&folder);
            }
            let yr = YEAR_RE.captures(name).map(|c| c[1].to_string());
            let n = norm_title(&t);
            key = format!("tv\t{}", if n.is_empty() { format!("folder:{}", parts[..parts.len() - 1].join("/")) } else { n });
            (media_type, title, year) = ("tv", t, yr);
        } else {
            let t = title_from_filename(name);
            let yr = YEAR_RE.captures(name).map(|c| c[1].to_string());
            key = format!("movie\t{}\t{}", norm_title(&t), yr.clone().unwrap_or_default());
            (media_type, title, year) = ("movie", t, yr);
        }

        let idx = match index.get(&key) {
            Some(&i) => {
                let w = &mut works[i];
                if w.year.is_none() && year.is_some() {
                    w.year = year.clone();
                }
                if !title.is_empty() && title.len() > w.title.len() {
                    w.title = title.clone();
                }
                i
            }
            None => {
                works.push(Work {
                    media_type: media_type.to_string(),
                    title: title.clone(),
                    year: year.clone(),
                    files: Vec::new(),
                    warnings: BTreeSet::new(),
                });
                index.insert(key.clone(), works.len() - 1);
                works.len() - 1
            }
        };

        let w = &mut works[idx];
        if norm_title(&w.title).is_empty() {
            w.warnings.insert("empty-title".to_string());
        }
        if w.media_type == "movie" && w.year.is_none() {
            w.warnings.insert("no-year".to_string());
        }
        w.files.push(FileRec {
            hash: h.clone(),
            name: name.clone(),
            size: *size,
            added_at: *added_at,
            season: if w.media_type == "tv" { season } else { None },
            episode,
            rel,
        });
    }

    BuildOut { works, skipped_linked, skipped_nonvideo, role_dirs }
}

fn detect_libraries(rows: &[Row], linked: &BTreeSet<String>, overrides: &[(Vec<String>, String)]) -> Vec<(String, String, usize)> {
    let mut libs: HashMap<String, (String, usize)> = HashMap::new();
    for (h, name, _size, path, _added) in rows {
        if !VIDEO_EXT.contains(&ext_of(name).as_str()) || linked.contains(h) {
            continue;
        }
        let parts = path_parts(path);
        if parts.is_empty() {
            continue;
        }
        let (role, kw_idx) = classify_path(&parts, overrides);
        let zd = zone_dir(&parts, kw_idx);
        let e = libs.entry(zd).or_insert((role, 0));
        e.1 += 1;
    }
    let mut out: Vec<(String, String, usize)> = libs.into_iter().map(|(zd, (r, c))| (zd, r, c)).collect();
    out.sort_by(|a, b| {
        role_index(&a.1).cmp(&role_index(&b.1)).then(a.0.cmp(&b.0))
    });
    out
}

fn role_index(role: &str) -> usize {
    ROLES.iter().position(|r| *r == role).unwrap_or(99)
}

fn confirm_libraries(libs: &[(String, String, usize)]) -> HashMap<String, String> {
    println!(
        "{} {}1=series 2=movies 3=mixed:",
        bold("Detected libraries"),
        dim("— press Enter to accept each, or type ")
    );
    let mut chosen = HashMap::new();
    for (zd, role, count) in libs {
        let ans = prompt_line(&format!(
            "  [{}] {}  {}  > ",
            cyan(&format!("{role:6}")),
            bold(zd),
            dim(&format!("({count} files)"))
        ));
        let role = match ans.as_str() {
            "1" | "2" | "3" => ROLES[ans.parse::<usize>().unwrap() - 1].to_string(),
            _ => role.clone(),
        };
        chosen.insert(zd.clone(), role);
    }
    println!();
    chosen
}

// --------------------------------------------------------------------------- //
// TMDB resolution
// --------------------------------------------------------------------------- //

struct Tmdb {
    key: Option<String>,
    token: Option<String>,
    lang: String,
    cache_path: PathBuf,
    cache: HashMap<String, Value>,
}

impl Tmdb {
    fn new(key: Option<String>, token: Option<String>, lang: String, cache_path: PathBuf) -> Self {
        let cache = fs::read_to_string(&cache_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Tmdb { key, token, lang, cache_path, cache }
    }

    fn save(&self) {
        if let Ok(s) = serde_json::to_string(&self.cache) {
            let tmp = self.cache_path.with_extension("json.tmp");
            if fs::write(&tmp, s).is_ok() {
                let _ = fs::rename(&tmp, &self.cache_path);
            }
        }
    }

    fn search(&mut self, media_type: &str, title: &str, year: &Option<String>) -> Vec<Value> {
        let ck = format!("{media_type}\t{title}\t{}", year.clone().unwrap_or_default());
        if let Some(v) = self.cache.get(&ck) {
            return v.as_array().cloned().unwrap_or_default();
        }
        let mut url = format!(
            "https://api.themoviedb.org/3/search/{media_type}?language={}&query={}&include_adult=false",
            pct_encode(&self.lang),
            pct_encode(title),
        );
        if self.token.is_none() {
            if let Some(k) = &self.key {
                url.push_str(&format!("&api_key={}", pct_encode(k)));
            }
        }
        if let Some(y) = year {
            let field = if media_type == "movie" { "year" } else { "first_air_date_year" };
            url.push_str(&format!("&{field}={y}"));
        }
        let results = self.get(&url);
        self.cache.insert(ck, Value::Array(results.clone()));
        self.save();
        std::thread::sleep(Duration::from_millis(300));
        results
    }

    fn get(&self, url: &str) -> Vec<Value> {
        for attempt in 0..3 {
            let mut req = minreq::get(url).with_header("Accept", "application/json");
            if let Some(t) = &self.token {
                req = req.with_header("Authorization", format!("Bearer {t}"));
            }
            match req.send() {
                Ok(resp) => {
                    if let Ok(body) = resp.as_str() {
                        if let Ok(v) = serde_json::from_str::<Value>(body) {
                            return v.get("results").and_then(|r| r.as_array()).cloned().unwrap_or_default();
                        }
                    }
                }
                Err(e) => {
                    if attempt == 2 {
                        eprintln!("  ! TMDB request failed: {e}");
                        return Vec::new();
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(1500 * (attempt + 1)));
        }
        Vec::new()
    }
}

fn result_title(media_type: &str, r: &Value) -> (String, String, String) {
    let g = |k: &str| r.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    if media_type == "movie" {
        (g("title"), g("original_title"), g("release_date").chars().take(4).collect())
    } else {
        (g("name"), g("original_name"), g("first_air_date").chars().take(4).collect())
    }
}

fn sim(a: &str, b: &str) -> f64 {
    SequenceMatcher::new(a, b).ratio() as f64
}

struct Scored {
    score: f64,
    sim: f64,
    ry: String,
    r: Value,
}

fn score_results(media_type: &str, results: &[Value], title: &str, year: &Option<String>) -> Vec<Scored> {
    let nt = norm_title(title);
    let mut scored: Vec<Scored> = results
        .iter()
        .map(|r| {
            let (t, ot, ry) = result_title(media_type, r);
            let simv = sim(&nt, &norm_title(&t)).max(sim(&nt, &norm_title(&ot)));
            let mut s = simv;
            if let (Some(y), false) = (year, ry.is_empty()) {
                let yi: i64 = y.parse().unwrap_or(0);
                let ryi: i64 = ry.parse().unwrap_or(0);
                if ryi == yi {
                    s += 0.3;
                } else if (ryi - yi).abs() == 1 {
                    s += 0.1;
                } else {
                    s -= 0.15;
                }
            }
            Scored { score: s, sim: simv, ry, r: r.clone() }
        })
        .collect();
    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    scored
}

#[derive(Clone)]
struct Candidate {
    id: i64,
    title: String,
    year: String,
    score: f64,
    sim: f64,
}

struct Resolution {
    id: Option<i64>,
    confident: bool,
    reason: String,
    candidates: Vec<Candidate>,
}

fn resolve(media_type: &str, work: &Work, tmdb: &mut Tmdb) -> Resolution {
    let mut queries = vec![work.title.clone()];
    let stripped = PARENS_RE.replace_all(&work.title, "").trim().to_string();
    if !stripped.is_empty() && stripped != work.title {
        queries.push(stripped);
    }
    let mut results: Vec<Value> = Vec::new();
    let mut used_q = work.title.clone();
    'outer: for q in &queries {
        let years: Vec<Option<String>> = if work.year.is_some() {
            vec![work.year.clone(), None]
        } else {
            vec![None]
        };
        for yr in years {
            results = tmdb.search(media_type, q, &yr);
            if !results.is_empty() {
                used_q = q.clone();
                break 'outer;
            }
        }
    }
    if results.is_empty() {
        return Resolution { id: None, confident: false, reason: "no-results".into(), candidates: vec![] };
    }
    let scored = score_results(media_type, &results, &used_q, &work.year);
    let cands: Vec<Candidate> = scored
        .iter()
        .take(4)
        .map(|s| Candidate {
            id: s.r.get("id").and_then(|v| v.as_i64()).unwrap_or(0),
            title: result_title(media_type, &s.r).0,
            year: s.ry.clone(),
            score: (s.score * 100.0).round() / 100.0,
            sim: (s.sim * 100.0).round() / 100.0,
        })
        .collect();
    let best = &scored[0];
    let mut confident = best.sim >= 0.6;
    let mut reason = if confident { "" } else { "low-similarity" }.to_string();
    if confident && scored.len() > 1 && (best.score - scored[1].score) < 0.15 && best.sim < 0.85 {
        confident = false;
        reason = "ambiguous".into();
    }
    if let (Some(y), false) = (&work.year, best.ry.is_empty()) {
        if best.ry.parse::<i64>().ok() != y.parse::<i64>().ok() {
            confident = false;
            reason = "year-mismatch".into();
        }
    }
    Resolution {
        id: best.r.get("id").and_then(|v| v.as_i64()),
        confident,
        reason,
        candidates: cands,
    }
}

// --------------------------------------------------------------------------- //
// TOML emission
// --------------------------------------------------------------------------- //

fn pct_encode(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let hex = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn rucio_link(h: &str, name: &str, size: i64) -> String {
    format!("rucio:{h}?name={}&size={size}", pct_encode(name))
}

fn link_hash(link: &str) -> Option<String> {
    HASH_IN_LINK.captures(link).map(|c| c[1].to_lowercase())
}

fn link_name(link: &str) -> String {
    NAME_IN_LINK.captures(link).map(|c| pct_decode(&c[1])).unwrap_or_default()
}

fn episodes_of_links(links: &[String]) -> BTreeSet<i64> {
    links.iter().filter_map(|l| episode_of(&link_name(l)).1).collect()
}

fn link_sort_key(link: &str) -> (i64, String) {
    (episode_of(&link_name(link)).1.unwrap_or(0), link_name(link))
}

#[derive(Clone)]
struct ReleaseOut {
    season: Option<i64>,
    header: Option<String>,
    note: Option<String>,
    source: Option<String>,
    resolution: Option<String>,
    hdr: Vec<String>,
    container: Option<String>,
    vcodec: Option<String>,
    acodec: Option<String>,
    date: Option<String>,
    ripper: Option<String>,
    origin: Option<String>,
    audio: Vec<String>,
    subs: Vec<String>,
    links: Vec<String>,
}

fn release_date(files: &[FileRec], mode: DateMode) -> String {
    let secs = match mode {
        DateMode::Today => now_secs(),
        DateMode::Added => files.iter().map(|f| f.added_at).max().unwrap_or(0),
    };
    date_str(secs)
}

fn build_releases(work: &Work, mode: DateMode) -> Vec<ReleaseOut> {
    // Group by release spec, preserving first-seen order (like Python's ordered
    // dict), then a STABLE sort by the display key keeps ties in that order.
    let mut order: Vec<ReleaseKey> = Vec::new();
    let mut groups: HashMap<ReleaseKey, Vec<FileRec>> = HashMap::new();
    for f in &work.files {
        let k = release_key(&work.media_type, f);
        if !groups.contains_key(&k) {
            order.push(k.clone());
        }
        groups.entry(k).or_default().push(f.clone());
    }
    let mut entries: Vec<(ReleaseKey, Vec<FileRec>)> =
        order.into_iter().map(|k| { let files = groups.remove(&k).unwrap(); (k, files) }).collect();
    entries.sort_by(|a, b| {
        let ka = &a.0;
        let kb = &b.0;
        (ka.season.unwrap_or(0), res_rank(&ka.resolution), ka.source.clone().unwrap_or_default())
            .cmp(&(kb.season.unwrap_or(0), res_rank(&kb.resolution), kb.source.clone().unwrap_or_default()))
    });
    entries
        .into_iter()
        .map(|(key, mut files)| {
            files.sort_by(|a, b| {
                (a.episode.unwrap_or(0), a.name.clone()).cmp(&(b.episode.unwrap_or(0), b.name.clone()))
            });
            let rel = &files[0].rel;
            ReleaseOut {
                season: key.season,
                header: None,
                note: None,
                source: rel.source.clone(),
                resolution: rel.resolution.clone(),
                hdr: rel.hdr.clone(),
                container: Some(rel.container.clone()),
                vcodec: rel.vcodec.clone(),
                acodec: rel.acodec.clone(),
                date: Some(release_date(&files, mode)),
                ripper: None,
                origin: None,
                audio: rel.audio.clone(),
                subs: rel.subs.clone(),
                links: files.iter().map(|f| rucio_link(&f.hash, &f.name, f.size)).collect(),
            }
        })
        .collect()
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ReleaseKey {
    season: Option<i64>,
    source: Option<String>,
    resolution: Option<String>,
    vcodec: Option<String>,
    acodec: Option<String>,
    container: String,
    hdr: Vec<String>,
    audio: Vec<String>,
    subs: Vec<String>,
}

fn release_key(media_type: &str, f: &FileRec) -> ReleaseKey {
    ReleaseKey {
        season: if media_type == "tv" { f.season } else { None },
        source: f.rel.source.clone(),
        resolution: f.rel.resolution.clone(),
        vcodec: f.rel.vcodec.clone(),
        acodec: f.rel.acodec.clone(),
        container: f.rel.container.clone(),
        hdr: f.rel.hdr.clone(),
        audio: f.rel.audio.clone(),
        subs: f.rel.subs.clone(),
    }
}

fn release_sig(r: &ReleaseOut) -> (Option<i64>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Vec<String>, Vec<String>, Vec<String>) {
    (r.season, r.source.clone(), r.resolution.clone(), r.vcodec.clone(), r.acodec.clone(), r.container.clone(), r.hdr.clone(), r.audio.clone(), r.subs.clone())
}

fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn toml_list(items: &[String]) -> String {
    format!("[{}]", items.iter().map(|i| toml_str(i)).collect::<Vec<_>>().join(", "))
}

fn render_release(r: &ReleaseOut) -> String {
    let mut out = vec!["[[releases]]".to_string()];
    if let Some(s) = r.season {
        out.push(format!("season      = {s}"));
    }
    if let Some(h) = &r.header {
        out.push(format!("header      = {}", toml_str(h)));
    }
    if let Some(n) = &r.note {
        out.push(format!("note        = {}", toml_str(n)));
    }
    if let Some(v) = &r.source {
        out.push(format!("source      = {}", toml_str(v)));
    }
    if let Some(v) = &r.resolution {
        out.push(format!("resolution  = {}", toml_str(v)));
    }
    if !r.hdr.is_empty() {
        out.push(format!("hdr         = {}", toml_list(&r.hdr)));
    }
    if let Some(v) = &r.container {
        out.push(format!("container   = {}", toml_str(v)));
    }
    if let Some(v) = &r.vcodec {
        out.push(format!("codec.video = {}", toml_str(v)));
    }
    if let Some(v) = &r.acodec {
        out.push(format!("codec.audio = {}", toml_str(v)));
    }
    if let Some(d) = &r.date {
        out.push(format!("date        = {d}"));
    }
    if let Some(v) = &r.ripper {
        out.push(format!("ripper      = {}", toml_str(v)));
    }
    if let Some(v) = &r.origin {
        out.push(format!("origin      = {}", toml_str(v)));
    }
    if !r.audio.is_empty() {
        out.push(format!("language.audio     = {}", toml_list(&r.audio)));
    }
    if !r.subs.is_empty() {
        out.push(format!("language.subtitles = {}", toml_list(&r.subs)));
    }
    let links = r.links.iter().map(|l| format!("  {}", toml_str(l))).collect::<Vec<_>>().join(",\n");
    out.push(format!("links = [\n{links},\n]"));
    out.join("\n")
}

fn render_ficha(media_type: &str, tmdb_id: i64, releases: &[ReleaseOut], note: &Option<String>) -> String {
    let mut head = vec![format!("type = \"{media_type}\""), format!("id   = {tmdb_id}")];
    if let Some(n) = note {
        head.push(format!("note = {}", toml_str(n)));
    }
    let blocks: Vec<String> = releases.iter().map(render_release).collect();
    format!("{}\n\n{}\n", head.join("\n"), blocks.join("\n\n"))
}

// Existing-ficha parsing for merge.
#[derive(Deserialize, Default)]
struct ExCodec {
    video: Option<String>,
    audio: Option<String>,
}
#[derive(Deserialize, Default)]
struct ExLang {
    #[serde(default)]
    audio: Vec<String>,
    #[serde(default)]
    subtitles: Vec<String>,
}
#[derive(Deserialize)]
struct ExRelease {
    season: Option<i64>,
    header: Option<String>,
    note: Option<String>,
    source: Option<String>,
    resolution: Option<String>,
    #[serde(default)]
    hdr: Vec<String>,
    container: Option<String>,
    #[serde(default)]
    codec: ExCodec,
    date: Option<toml::value::Datetime>,
    ripper: Option<String>,
    origin: Option<String>,
    #[serde(default)]
    language: ExLang,
    #[serde(default)]
    links: Vec<String>,
}
#[derive(Deserialize)]
struct ExFicha {
    #[serde(rename = "type")]
    media_type: Option<String>,
    note: Option<String>,
    #[serde(default)]
    releases: Vec<ExRelease>,
}

fn flatten_release(r: ExRelease) -> ReleaseOut {
    ReleaseOut {
        season: r.season,
        header: r.header,
        note: r.note,
        source: r.source,
        resolution: r.resolution,
        hdr: r.hdr,
        container: r.container,
        vcodec: r.codec.video,
        acodec: r.codec.audio,
        date: r.date.map(|d| d.to_string()),
        ripper: r.ripper,
        origin: r.origin,
        audio: r.language.audio,
        subs: r.language.subtitles,
        links: r.links,
    }
}

// --------------------------------------------------------------------------- //
// Dates (UTC, dependency-free)
// --------------------------------------------------------------------------- //

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

fn date_str(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86400));
    format!("{y:04}-{m:02}-{d:02}")
}

fn datetime_str(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86400));
    let s = secs.rem_euclid(86400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

// --------------------------------------------------------------------------- //
// Driver
// --------------------------------------------------------------------------- //

fn subdir(media_type: &str) -> &'static str {
    match media_type {
        "tv" => "tv",
        "movie" => "movies",
        _ => "",
    }
}

fn entry_path(content_dir: &Path, media_type: &str, tmdb_id: i64, existing_paths: &HashMap<(String, i64), PathBuf>) -> PathBuf {
    if let Some(p) = existing_paths.get(&(media_type.to_string(), tmdb_id)) {
        return p.clone();
    }
    content_dir.join(subdir(media_type)).join(format!("{media_type}-{tmdb_id}.toml"))
}

struct Existing {
    linked: BTreeSet<String>,
    ids: BTreeSet<(String, i64)>,
    paths: HashMap<(String, i64), PathBuf>,
}

fn load_existing(content_dir: &Path) -> Existing {
    let mut e = Existing { linked: BTreeSet::new(), ids: BTreeSet::new(), paths: HashMap::new() };
    if !content_dir.is_dir() {
        return e;
    }
    let id_re = Regex::new(r"^(tv|movie)-(\d+)\.toml$").unwrap();
    let mut stack = vec![content_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let fname = entry.file_name().to_string_lossy().into_owned();
            if !fname.ends_with(".toml") {
                continue;
            }
            if let Some(c) = id_re.captures(&fname) {
                let key = (c[1].to_string(), c[2].parse().unwrap());
                e.ids.insert(key.clone());
                e.paths.insert(key, path.clone());
            }
            if let Ok(text) = fs::read_to_string(&path) {
                for cap in HASH_IN_LINK.captures_iter(&text) {
                    e.linked.insert(cap[1].to_lowercase());
                }
            }
        }
    }
    e
}

fn read_rows(db_path: &Path) -> Result<Vec<Row>> {
    use rusqlite::{Connection, OpenFlags};
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {}", db_path.display()))?;
    let mut stmt = conn
        .prepare("SELECT lower(hex(root_hash)), name, size, path, added_at FROM shared_files")
        .map_err(|e| anyhow::anyhow!("cannot read 'shared_files' from {}: {e}\n       is this a Rucio database?", db_path.display()))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?, r.get::<_, i64>(4)?))
        })?
        .collect::<rusqlite::Result<Vec<Row>>>()?;
    Ok(rows)
}

struct Record {
    work_idx: usize,
    status: String, // write | merge | doubtful
    media_type: String,
    title: String,
    year: Option<String>,
    tmdb_id: Option<i64>,
    reason: String,
    candidates: Vec<Candidate>,
    files: usize,
    seasons: Vec<i64>,
    warnings: Vec<String>,
    added_links: Option<usize>,
    target: Option<String>,
}

fn write_entry(work: &Work, tmdb_id: i64, content_dir: &Path, mode: DateMode, existing_paths: &mut HashMap<(String, i64), PathBuf>) -> Result<()> {
    let path = entry_path(content_dir, &work.media_type, tmdb_id, existing_paths);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, render_ficha(&work.media_type, tmdb_id, &build_releases(work, mode), &None))?;
    existing_paths.insert((work.media_type.clone(), tmdb_id), path);
    println!("  {} {}-{tmdb_id}  {:?} ({} files)", green("+"), work.media_type, work.title, work.files.len());
    Ok(())
}

/// Fold the incoming releases into `releases`, adding only links whose hash is
/// not already present. A TV release with no overlapping episode numbers folds
/// into the matching same-spec release (bumping its date); otherwise it becomes
/// a new block, keeping its own metadata. Returns the number of links added.
fn merge_into(releases: &mut Vec<ReleaseOut>, incoming: Vec<ReleaseOut>) -> usize {
    let mut have: BTreeSet<String> =
        releases.iter().flat_map(|r| r.links.iter().filter_map(|l| link_hash(l))).collect();
    let mut added = 0usize;
    for nr in incoming {
        let new_links: Vec<String> = nr
            .links
            .iter()
            .filter(|l| link_hash(l).map(|h| !have.contains(&h)).unwrap_or(false))
            .cloned()
            .collect();
        if new_links.is_empty() {
            continue;
        }
        for l in &new_links {
            if let Some(h) = link_hash(l) {
                have.insert(h);
            }
        }
        added += new_links.len();

        let mut target: Option<usize> = None;
        if nr.season.is_some() {
            let new_eps = episodes_of_links(&new_links);
            if !new_eps.is_empty() {
                for (i, er) in releases.iter().enumerate() {
                    if release_sig(er) == release_sig(&nr) && episodes_of_links(&er.links).is_disjoint(&new_eps) {
                        target = Some(i);
                        break;
                    }
                }
            }
        }
        if let Some(i) = target {
            let mut merged = releases[i].links.clone();
            merged.extend(new_links);
            merged.sort_by(|a, b| link_sort_key(a).cmp(&link_sort_key(b)));
            releases[i].links = merged;
            releases[i].date = [releases[i].date.clone(), nr.date.clone()].into_iter().flatten().max();
        } else {
            let mut block = nr.clone();
            block.links = new_links;
            releases.push(block);
        }
    }
    added
}

fn merge_entry(work: &Work, tmdb_id: i64, content_dir: &Path, mode: DateMode, existing_paths: &mut HashMap<(String, i64), PathBuf>) -> Result<usize> {
    let path = entry_path(content_dir, &work.media_type, tmdb_id, existing_paths);
    let data: ExFicha = toml::from_str(&fs::read_to_string(&path)?)?;
    let mtype = data.media_type.unwrap_or_else(|| work.media_type.clone());
    let note = data.note;
    let mut releases: Vec<ReleaseOut> = data.releases.into_iter().map(flatten_release).collect();
    let added = merge_into(&mut releases, build_releases(work, mode));
    if added == 0 {
        return Ok(0);
    }
    fs::write(&path, render_ficha(&mtype, tmdb_id, &releases, &note))?;
    println!("  {} merge  {mtype}-{tmdb_id}  {} -> {:?}", cyan("^"), cyan(&format!("+{added} links")), work.title);
    Ok(added)
}

const DEFAULT_SIM: f64 = 0.85;
const DEFAULT_GAP: f64 = 0.10;

fn default_choice(cands: &[Candidate]) -> Option<usize> {
    if cands.len() == 1 {
        return Some(0);
    }
    if cands.len() >= 2 && cands[0].sim >= DEFAULT_SIM && cands[0].sim - cands[1].sim >= DEFAULT_GAP {
        return Some(0);
    }
    None
}

fn parse_id(s: &str, default_type: &str) -> Option<(String, i64)> {
    let s = s.trim().to_lowercase();
    if let Some(c) = ID_RE.captures(&s) {
        return Some((c[1].to_string(), c[2].parse().unwrap()));
    }
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
        return Some((default_type.to_string(), s.parse().unwrap()));
    }
    None
}

pub fn run(args: ImportArgs) -> Result<()> {
    // Mutually exclusive inputs: --from-works reads an already-parsed file, so
    // there is nothing for --export to extract.
    if args.from_works.is_some() && args.export.is_some() {
        bail!(red("error: --from-works and --export cannot be combined."));
    }

    let content_dir = resolve_content(&args.content)?;
    let mut existing = load_existing(&content_dir);
    println!(
        "Existing entries: {} ({} links already covered)",
        bold(&existing.ids.len().to_string()),
        existing.linked.len()
    );

    // Obtain the works either from a previously exported file (the offline link
    // hand-off) or by parsing a Rucio database.
    let mut works = if let Some(path) = &args.from_works {
        let works = load_works_file(path, &existing.linked)?;
        let tv = works.iter().filter(|w| w.media_type == "tv").count();
        println!(
            "{}  ({tv} series, {} films)\n",
            bold(&format!("Works from {path}: {}", works.len())),
            works.len() - tv
        );
        works
    } else {
        // Fail fast: resolve and validate the database before doing any work.
        let db = resolve_db(&args.db)?;
        if !db.exists() {
            bail!(red(&format!(
                "error: database not found: {}\n       pass --db PATH (e.g. the Rucio daemon's rucio.db).",
                db.display()
            )));
        }
        let rows = read_rows(&db).map_err(|e| anyhow::anyhow!(red(&format!("error: {e}"))))?;

        let mut overrides: Vec<(Vec<String>, String)> = Vec::new();
        for p in &args.series {
            overrides.push((path_parts(p), "series".into()));
        }
        for p in &args.movies {
            overrides.push((path_parts(p), "movies".into()));
        }
        for p in &args.mixed {
            overrides.push((path_parts(p), "mixed".into()));
        }
        overrides.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

        // --plan and --export are offline previews: never prompt for libraries.
        let offline = args.plan || args.export.is_some();
        let interactive = !args.auto && !offline && !args.dry_run && io::stdin().is_terminal();
        if interactive {
            let chosen = confirm_libraries(&detect_libraries(&rows, &existing.linked, &overrides));
            for (zd, r) in chosen {
                overrides.push((path_parts(&zd), r));
            }
            overrides.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        }

        let BuildOut { works, skipped_linked, skipped_nonvideo, role_dirs } =
            build_works(&rows, &existing.linked, &overrides);

        if !interactive {
            println!("{}", bold("Detected libraries") + &dim(" (override with --series/--movies/--mixed DIR):"));
            for role in ["series", "movies", "mixed"] {
                if let Some(dirs) = role_dirs.get(role) {
                    for d in dirs {
                        println!("  {} {d}", cyan(&format!("{role:7}")));
                    }
                }
            }
            println!();
        }

        let tv = works.iter().filter(|w| w.media_type == "tv").count();
        println!(
            "DB rows: {} | skipped (already linked): {skipped_linked} | skipped (non-video): {skipped_nonvideo}",
            rows.len()
        );
        println!(
            "{}  ({tv} series, {} films)\n",
            bold(&format!("New works to import: {}", works.len())),
            works.len() - tv
        );
        works
    };

    works.sort_by(|a, b| b.newest().cmp(&a.newest()));
    if args.limit > 0 {
        works.truncate(args.limit);
    }

    if args.plan {
        let mut plan = Vec::new();
        for w in &works {
            let ss = w.seasons();
            let flag: Vec<&String> = w.warnings.iter().collect();
            let flag_str = flag.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ");
            println!(
                "[{}] {:?} ({}) files={} seasons={} {}",
                cyan(&format!("{:5}", w.media_type)),
                w.title,
                w.year.clone().unwrap_or_else(|| "?".into()),
                w.files.len(),
                seasons_str(&ss),
                yellow(&flag_str)
            );
            plan.push(json!({
                "type": w.media_type, "title": w.title, "year": w.year,
                "files": w.files.len(), "seasons": ss, "warnings": w.warnings.iter().collect::<Vec<_>>(),
            }));
        }
        if let Some(path) = &args.report {
            fs::write(path, serde_json::to_string_pretty(&plan)?)?;
            println!("\n{}{}{}", bold("Plan: "), absolute(path).display(), dim(" (offline, nothing imported)"));
        }
        return Ok(());
    }

    // --export: write the parsed works (links + release details) for someone
    // else to finish with --from-works. Offline, no TMDB.
    if let Some(path) = &args.export {
        let doc = WorksFile { version: WORKS_FORMAT_VERSION, works };
        fs::write(path, serde_json::to_string_pretty(&doc)?)?;
        let links: usize = doc.works.iter().map(|w| w.files.len()).sum();
        println!(
            "\n{}{} ({} works, {} links){}",
            bold("Exported: "),
            absolute(path).display(),
            doc.works.len(),
            links,
            dim(" — finish with `tordo import --from-works`.")
        );
        return Ok(());
    }

    // Real import (DB or works file): a TMDB credential is required.
    if args.tmdb_key.is_none() && args.tmdb_token.is_none() {
        bail!(red(
            "error: no TMDB credential. Set TMDB_READ_TOKEN (v4) or TMDB_API_KEY (v3),\n       pass --tmdb-token/--tmdb-key, or use --plan for an offline preview."
        ));
    }

    let cache_path = content_dir
        .parent()
        .unwrap_or(Path::new("."))
        .join("cache")
        .join("tmdb-search-cache.json");
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut tmdb = Tmdb::new(args.tmdb_key.clone(), args.tmdb_token.clone(), args.lang.clone(), cache_path);
    if args.dry_run {
        println!("{}: querying TMDB, writing no .toml files.\n", yellow(&bold("DRY RUN")));
    } else {
        fs::create_dir_all(&content_dir)?;
    }

    let mut records: Vec<Record> = Vec::new();
    for (idx, w) in works.iter().enumerate() {
        let res = resolve(&w.media_type, w, &mut tmdb);
        let mut status = "doubtful".to_string();
        if let (Some(_id), true) = (res.id, res.confident) {
            status = if existing.ids.contains(&(w.media_type.clone(), res.id.unwrap())) { "merge" } else { "write" }.to_string();
        }
        let target = res.id.map(|id| format!("{}/{}-{id}.toml", subdir(&w.media_type), w.media_type));
        let mut rec = Record {
            work_idx: idx,
            status: status.clone(),
            media_type: w.media_type.clone(),
            title: w.title.clone(),
            year: w.year.clone(),
            tmdb_id: res.id,
            reason: res.reason.clone(),
            candidates: res.candidates.clone(),
            files: w.files.len(),
            seasons: w.seasons(),
            warnings: w.warnings.iter().cloned().collect(),
            added_links: None,
            target,
        };
        if status == "write" {
            existing.ids.insert((w.media_type.clone(), res.id.unwrap()));
            if args.dry_run {
                println!("{}{}  {:?}", dim("  ~ would write "), green(&format!("{}-{}", w.media_type, res.id.unwrap())), w.title);
            } else {
                write_entry(w, res.id.unwrap(), &content_dir, args.date, &mut existing.paths)?;
            }
        } else if status == "merge" {
            if args.dry_run {
                rec.added_links = Some(w.files.len());
                println!("{}{}  +{} links  {:?}", dim("  ~ would merge "), cyan(&format!("{}-{}", w.media_type, res.id.unwrap())), w.files.len(), w.title);
            } else {
                rec.added_links = Some(merge_entry(w, res.id.unwrap(), &content_dir, args.date, &mut existing.paths)?);
            }
        }
        records.push(rec);
    }

    let count = |s: &str| records.iter().filter(|r| r.status == s).count();
    let verb = if args.dry_run { "would write" } else { "written" };
    println!(
        "\n{verb}: {} | merged into existing: {} | doubtful: {}",
        green(&count("write").to_string()),
        cyan(&count("merge").to_string()),
        yellow(&count("doubtful").to_string())
    );

    if !args.auto && !args.dry_run && io::stdin().is_terminal() {
        review(&mut records, &works, &content_dir, args.date, &mut existing)?;
    }

    if let Some(path) = &args.report {
        write_report(path, &records, &works, &args)?;
        println!("\n{}{}", bold("Report: "), absolute(path).display());
    } else if args.dry_run {
        println!("{}", dim("\n(nothing written; pass --report PATH [--report-format json] to save a detailed report)"));
    }
    if !args.dry_run {
        println!("Review the new content/*.toml, then rebuild with `tordo generate`.");
    }
    Ok(())
}

fn seasons_str(ss: &[i64]) -> String {
    if ss.is_empty() {
        "-".to_string()
    } else {
        format!("{ss:?}")
    }
}

fn human_size(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

fn review(records: &mut [Record], works: &[Work], content_dir: &Path, mode: DateMode, existing: &mut Existing) -> Result<()> {
    let doubtful: Vec<usize> = records.iter().enumerate().filter(|(_, r)| r.status == "doubtful").map(|(i, _)| i).collect();
    if doubtful.is_empty() {
        return Ok(());
    }
    println!("{}{}", bold("\n--- Doubtful works: review "), dim("(TMDB ids at https://www.themoviedb.org)"));
    for ri in doubtful {
        let (w, cands, reason) = {
            let r = &records[ri];
            let w = &works[r.work_idx];
            let cands: Vec<Candidate> = r.candidates.iter().take(9).cloned().collect();
            (w, cands, r.reason.clone())
        };
        let mut media_type = w.media_type.clone();
        let dflt = default_choice(&cands);
        println!(
            "\n[{}] {} ({})  files={} seasons={}  reason={}",
            cyan(&media_type),
            bold(&format!("{:?}", w.title)),
            w.year.clone().unwrap_or_else(|| "?".into()),
            w.files.len(),
            seasons_str(&w.seasons()),
            yellow(&reason)
        );
        // Show a sample of the actual shared files so the user can recognise
        // the work from the real names, not just the (possibly wrong) title.
        const SAMPLE: usize = 4;
        for f in w.files.iter().take(SAMPLE) {
            let tag = match (f.season, f.episode) {
                (Some(s), Some(e)) => format!("S{s:02}E{e:02} · "),
                (None, Some(e)) => format!("E{e:02} · "),
                _ => String::new(),
            };
            println!("     {} {}  {}", dim("·"), f.name, dim(&format!("[{tag}{}]", human_size(f.size))));
        }
        if w.files.len() > SAMPLE {
            println!("     {}", dim(&format!("… +{} more files", w.files.len() - SAMPLE)));
        }
        for (i, c) in cands.iter().enumerate() {
            let mark = if dflt == Some(i) { format!("  {}", green("<- Enter")) } else { String::new() };
            println!(
                "   {}) {} ({})  id={}  score={}{mark}",
                bold(&(i + 1).to_string()),
                c.title,
                if c.year.is_empty() { "?" } else { &c.year },
                green(&c.id.to_string()),
                c.score
            );
        }
        let enter = match dflt {
            Some(d) => format!("Enter={}", d + 1),
            None if cands.is_empty() => "Enter=skip".to_string(),
            None => "Enter re-asks".to_string(),
        };
        let mut segs: Vec<String> = Vec::new();
        if !cands.is_empty() {
            segs.push(format!("pick [{}]", if cands.len() == 1 { "1".to_string() } else { format!("1-{}", cands.len()) }));
        }
        segs.extend(["<id> or tv-<id>", "i=enter id", "s=skip", "q=quit"].map(String::from));
        let prompt = dim(&format!("   {} ({enter}): ", segs.join(", ")));

        let mut chosen: Option<(String, i64)> = None;
        loop {
            let ans = prompt_line(&prompt).to_lowercase();
            if ans == "q" {
                return Ok(());
            }
            if ans == "s" {
                break;
            }
            if ans.is_empty() {
                if let Some(d) = dflt {
                    chosen = Some((media_type.clone(), cands[d].id));
                    break;
                }
                if cands.is_empty() {
                    break;
                }
                println!("{}", dim("   no default here — choose a number, enter a TMDB id, or s to skip"));
                continue;
            }
            if ans == "i" {
                let sub = prompt_line(&dim("     TMDB id (number, or tv-<id> / movie-<id>): "));
                if let Some(c) = parse_id(&sub, &media_type) {
                    chosen = Some(c);
                    break;
                }
                println!("{}", dim("   not a valid id"));
                continue;
            }
            if let Some(rest) = ans.strip_prefix('i') {
                if let Some(c) = parse_id(rest, &media_type) {
                    chosen = Some(c);
                    break;
                }
            }
            if let Ok(n) = ans.parse::<usize>() {
                if n <= 9 {
                    if n >= 1 && n <= cands.len() {
                        chosen = Some((media_type.clone(), cands[n - 1].id));
                        break;
                    }
                    println!("{}", dim(&format!("   there is no option {n}")));
                    continue;
                }
            }
            if let Some(c) = parse_id(&ans, &media_type) {
                chosen = Some(c);
                break;
            }
            println!("{}", dim("   invalid — try again"));
        }
        let Some((ctype, cid)) = chosen else { continue };
        media_type = ctype.clone();
        // Build a work clone with the possibly-corrected media type for rendering.
        let mut cw = clone_work(&works[records[ri].work_idx]);
        cw.media_type = ctype.clone();
        records[ri].media_type = ctype.clone();
        records[ri].tmdb_id = Some(cid);
        records[ri].target = Some(format!("{}/{}-{cid}.toml", subdir(&ctype), ctype));
        if existing.ids.contains(&(ctype.clone(), cid)) {
            records[ri].status = "merge".into();
            records[ri].added_links = Some(merge_entry(&cw, cid, content_dir, mode, &mut existing.paths)?);
        } else {
            write_entry(&cw, cid, content_dir, mode, &mut existing.paths)?;
            existing.ids.insert((ctype.clone(), cid));
            records[ri].status = "write".into();
        }
    }
    Ok(())
}

fn clone_work(w: &Work) -> Work {
    Work {
        media_type: w.media_type.clone(),
        title: w.title.clone(),
        year: w.year.clone(),
        files: w.files.clone(),
        warnings: w.warnings.clone(),
    }
}

fn releases_json(releases: &[ReleaseOut]) -> Value {
    Value::Array(
        releases
            .iter()
            .map(|r| {
                json!({
                    "season": r.season, "date": r.date,
                    "source": r.source, "resolution": r.resolution,
                    "hdr": r.hdr, "container": r.container,
                    "codec": {"video": r.vcodec, "audio": r.acodec},
                    "language": {"audio": r.audio, "subtitles": r.subs},
                    "links": r.links,
                })
            })
            .collect(),
    )
}

fn write_report(path: &str, records: &[Record], works: &[Work], args: &ImportArgs) -> Result<()> {
    if matches!(args.report_format, ReportFormat::Json) {
        let mut works_json = Vec::new();
        for rec in records {
            let mut d = json!({
                "status": rec.status, "type": rec.media_type, "title": rec.title,
                "year": rec.year, "tmdb_id": rec.tmdb_id, "reason": rec.reason,
                "files": rec.files, "seasons": rec.seasons, "warnings": rec.warnings,
                "added_links": rec.added_links,
                "candidates": rec.candidates.iter().map(|c| json!({
                    "id": c.id, "title": c.title, "year": c.year, "score": c.score, "sim": c.sim
                })).collect::<Vec<_>>(),
                "target": rec.target,
            });
            if let Some(_id) = rec.tmdb_id {
                let rels = build_releases(&works[rec.work_idx], args.date);
                let updated = rels.iter().filter_map(|r| r.date.clone()).max();
                d["updated"] = json!(updated);
                d["releases"] = releases_json(&rels);
            }
            works_json.push(d);
        }
        let summary = json!({
            "write": records.iter().filter(|r| r.status == "write").count(),
            "merge": records.iter().filter(|r| r.status == "merge").count(),
            "doubtful": records.iter().filter(|r| r.status == "doubtful").count(),
            "total": records.len(),
        });
        let doc = json!({
            "generated": datetime_str(now_secs()),
            "dry_run": args.dry_run,
            "summary": summary,
            "works": works_json,
        });
        fs::write(path, serde_json::to_string_pretty(&doc)?)?;
        return Ok(());
    }

    let group = |s: &str| records.iter().filter(|r| r.status == s).collect::<Vec<_>>();
    let head = if args.dry_run { "Would write" } else { "Written" };
    let mhead = if args.dry_run { "Would merge" } else { "Merged" };
    let mut lines = vec![
        "# Rucio DB import report".to_string(),
        format!("# generated {}{}", datetime_str(now_secs()), if args.dry_run { "  (DRY RUN)" } else { "" }),
        String::new(),
    ];
    let w = group("write");
    lines.push(format!("## {head} — new fichas ({})", w.len()));
    for r in &w {
        lines.push(format!("  {}\t{} ({})\t{} files", r.target.clone().unwrap_or_default(), r.title, r.year.clone().unwrap_or_else(|| "?".into()), r.files));
    }
    lines.push(String::new());
    let e = group("merge");
    lines.push(format!("## {mhead} — links added to an existing ficha ({})", e.len()));
    for r in &e {
        lines.push(format!("  {}\t{} ({})\t+{} links", r.target.clone().unwrap_or_default(), r.title, r.year.clone().unwrap_or_else(|| "?".into()), r.added_links.unwrap_or(0)));
    }
    lines.push(String::new());
    let d = group("doubtful");
    lines.push(format!("## Doubtful, NOT imported ({})", d.len()));
    for r in &d {
        let cs = r.candidates.iter().map(|c| format!("{} ({}) id={}", c.title, if c.year.is_empty() { "?" } else { &c.year }, c.id)).collect::<Vec<_>>().join("; ");
        lines.push(format!("  [{}] {} ({})  reason={}", r.media_type, r.title, r.year.clone().unwrap_or_else(|| "?".into()), r.reason));
        lines.push(format!("      files={} seasons={}  candidates: {}", r.files, seasons_str(&r.seasons), if cs.is_empty() { "none".to_string() } else { cs }));
    }
    fs::write(path, lines.join("\n") + "\n")?;
    Ok(())
}

// --------------------------------------------------------------------------- //
// `tordo merge` — merge one content directory into another
// --------------------------------------------------------------------------- //

#[derive(Args)]
pub struct MergeArgs {
    /// Base content directory; new links and entries are added here, in place.
    pub base: String,
    /// Content directory to merge onto the base.
    pub other: String,
    /// Show what would change, write nothing.
    #[arg(long)]
    pub dry_run: bool,
}

pub fn run_merge(args: MergeArgs) -> Result<()> {
    let base_dir = absolute(&args.base);
    let other_dir = absolute(&args.other);
    if !base_dir.is_dir() {
        bail!(red(&format!("error: base content not found: {}", base_dir.display())));
    }
    if !other_dir.is_dir() {
        bail!(red(&format!("error: content to merge not found: {}", other_dir.display())));
    }
    let mut base = load_existing(&base_dir);
    let other = load_existing(&other_dir);

    // Process in a stable order so output and any conflicts are reproducible.
    let mut keys: Vec<(String, i64)> = other.paths.keys().cloned().collect();
    keys.sort();

    let (mut new_fichas, mut merged_fichas, mut added_links) = (0usize, 0usize, 0usize);
    for key in keys {
        let id = key.1;
        let other_path = &other.paths[&key];
        let data: ExFicha = toml::from_str(&fs::read_to_string(other_path)?)
            .with_context(|| format!("parsing {}", other_path.display()))?;
        let onote = data.note.clone();
        let omtype = data.media_type.clone().unwrap_or_else(|| key.0.clone());
        let incoming: Vec<ReleaseOut> = data.releases.into_iter().map(flatten_release).collect();

        if base.ids.contains(&key) {
            // Same entry on both sides: fold the new links into the base ficha.
            let base_path = base.paths[&key].clone();
            let bdata: ExFicha = toml::from_str(&fs::read_to_string(&base_path)?)
                .with_context(|| format!("parsing {}", base_path.display()))?;
            let bmtype = bdata.media_type.unwrap_or_else(|| key.0.clone());
            let bnote = bdata.note;
            let mut releases: Vec<ReleaseOut> = bdata.releases.into_iter().map(flatten_release).collect();
            let n = merge_into(&mut releases, incoming);
            if n > 0 {
                if !args.dry_run {
                    fs::write(&base_path, render_ficha(&bmtype, id, &releases, &bnote))?;
                }
                merged_fichas += 1;
                added_links += n;
                println!("  {} merge  {bmtype}-{id}  {}", cyan("^"), cyan(&format!("+{n} links")));
            }
        } else {
            // Only in the other side: add it to the base under its per-type subdir.
            let links: usize = incoming.iter().map(|r| r.links.len()).sum();
            let dest = base_dir.join(subdir(&omtype)).join(format!("{omtype}-{id}.toml"));
            if !args.dry_run {
                if let Some(p) = dest.parent() {
                    fs::create_dir_all(p)?;
                }
                fs::write(&dest, render_ficha(&omtype, id, &incoming, &onote))?;
            }
            base.ids.insert(key.clone());
            base.paths.insert(key, dest);
            new_fichas += 1;
            added_links += links;
            println!("  {} new    {omtype}-{id}  ({links} links)", green("+"));
        }
    }

    println!(
        "\n{}: {} new fichas, {} merged, {} links added{}",
        bold("merge"),
        green(&new_fichas.to_string()),
        cyan(&merged_fichas.to_string()),
        added_links,
        if args.dry_run { dim(" (dry run, nothing written)") } else { String::new() }
    );
    Ok(())
}
