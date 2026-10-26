//! Tordo — a static-site generator that builds your own browsable catalog of
//! media (films and series) whose details come from TMDB and whose body is a
//! set of Rucio links, grouped into releases.
//!
//! The build is the only moment anything "dynamic" happens: content TOML is
//! read, TMDB metadata is fetched (and cached on disk), and plain HTML plus a
//! search index are emitted into `dist/`. The site is static files; no backend.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Context as _, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tera::{Context, Tera};

const TMDB_IMG_BASE: &str = "https://image.tmdb.org/t/p/w500";
const CONTENT_DIR: &str = "content";
const EXAMPLES_DIR: &str = "examples";
const CACHE_DIR: &str = "cache";
const STATIC_DIR: &str = "static";
const OUT_DIR: &str = "dist";

// ---------------------------------------------------------------------------
// Site configuration
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SiteConfig {
    title: String,
    lang: String,
    #[serde(default)]
    base_url: String,
    /// TMDB content language; defaults to a sensible value derived from `lang`.
    #[serde(default)]
    tmdb_language: Option<String>,
    /// Days before an *ongoing* series is re-requested from TMDB (films and
    /// finished series are fetched once). Defaults to 30.
    #[serde(default)]
    tmdb_refresh_days: Option<u64>,
    /// Optional contact link (Telegram) shown in the footer for contributions.
    #[serde(default)]
    contact_telegram: Option<String>,
    /// Short tagline shown in link previews (Open Graph) for the home page.
    #[serde(default)]
    description: Option<String>,
    /// Image for home-page link previews (path relative to the site root, e.g.
    /// `static/og-image.png`; crawlers do not render SVG). Detail pages use the
    /// TMDB poster instead. Needs `base_url` set to resolve to an absolute URL.
    #[serde(default)]
    og_image: Option<String>,
}

// ---------------------------------------------------------------------------
// Parse layer: the content TOML as authored
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawFicha {
    #[serde(rename = "type")]
    media_type: String,
    id: u64,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    releases: Vec<RawRelease>,
}

#[derive(Deserialize)]
struct RawRelease {
    #[serde(default)]
    season: Option<i64>,
    #[serde(default)]
    header: Option<String>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    resolution: Option<String>,
    #[serde(default)]
    hdr: Vec<String>,
    #[serde(default)]
    container: Option<String>,
    #[serde(default)]
    codec: Option<RawCodec>,
    #[serde(default)]
    date: Option<toml::value::Datetime>,
    #[serde(default)]
    ripper: Option<String>,
    #[serde(default)]
    origin: Option<String>,
    #[serde(default)]
    language: Option<RawLanguage>,
    links: Vec<String>,
}

#[derive(Deserialize, Default)]
struct RawCodec {
    #[serde(default)]
    video: Option<String>,
    #[serde(default)]
    audio: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawLanguage {
    #[serde(default)]
    audio: Vec<String>,
    #[serde(default)]
    subtitles: Vec<String>,
}

// ---------------------------------------------------------------------------
// View layer: plain, serializable structs handed to the templates and index
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
struct Card {
    slug: String,
    media_type: String,
    title: String,
    year: String,
    date: String,
    poster: String,
    /// Short, ellipsised synopsis for the listing.
    overview: String,
    /// Distinct season count (0 for films or seasonless entries).
    season_count: usize,
    /// Distinct resolutions offered across releases.
    resolutions: Vec<String>,
    has_highlights: bool,
}

#[derive(Serialize)]
struct Lang {
    code: String,
    label: String,
    flag: String,
}

#[derive(Serialize)]
struct ReleaseView {
    season: Option<i64>,
    header: String,
    note_html: String,
    badges: Vec<Badge>,
    date: String,
    ripper: String,
    origin: String,
    audio: Vec<Lang>,
    subtitles: Vec<Lang>,
    links: Vec<LinkView>,
}

/// A technical tag with a display `label` and a `kind` that drives its colour
/// (res, hdr, dv, or neutral).
#[derive(Serialize)]
struct Badge {
    label: String,
    kind: String,
}

/// A Rucio link plus, when the URL carries `&size=<bytes>`, a human-readable
/// size for the second column. `episode` is the episode number pulled from the
/// link name (series only, empty when it can't be told).
#[derive(Serialize)]
struct LinkView {
    url: String,
    size: String,
    episode: String,
}

#[derive(Serialize)]
struct FichaView {
    slug: String,
    media_type: String,
    title: String,
    year: String,
    overview: String,
    poster: String,
    note_html: String,
    tagline: String,
    /// Original title, set only when it differs from the localized one.
    original_title: String,
    rating: String,
    genres: Vec<String>,
    /// Pre-joined facts line (seasons/episodes/status, or runtime).
    facts_line: String,
    credit: String,
    releases: Vec<ReleaseView>,
}

/// Per-page metadata: the document title plus the link-preview (Open Graph)
/// fields. Built per page so the home and each detail page preview differently.
#[derive(Serialize)]
struct Meta {
    doc_title: String,
    title: String,
    description: String,
    image: String,
    url: String,
}

// ---------------------------------------------------------------------------
// TMDB
// ---------------------------------------------------------------------------

/// How aggressively to bypass the cache this run.
#[derive(Clone, Copy, PartialEq)]
enum RefreshMode {
    /// Honor the TTL for ongoing series; films and finished series stay cached.
    Auto,
    /// Re-fetch ongoing series now; films and finished series stay cached.
    Live,
    /// Re-fetch everything.
    All,
}

/// Items whose TMDB data we treat as never-changing: films always, and series
/// whose run has ended or been canceled.
fn is_terminal(media_type: &str, j: &serde_json::Value) -> bool {
    media_type != "tv" || matches!(j["status"].as_str(), Some("Ended" | "Canceled"))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Return the TMDB payload for an item. Reads the on-disk cache and only hits
/// the network when the item is missing, its TTL expired (ongoing series), or a
/// refresh was requested. A failed refresh falls back to the cached copy so the
/// build never breaks. We stamp each cached payload with `_fetched_at` (in its
/// JSON, so it survives git/CI) rather than trusting file mtimes.
fn tmdb_fetch(
    media_type: &str,
    id: u64,
    lang: &str,
    mode: RefreshMode,
    ttl_secs: u64,
    cache_dir: &Path,
) -> Result<serde_json::Value> {
    let cache_path = cache_dir.join(format!("{media_type}-{id}.json"));

    let cached: Option<serde_json::Value> = if cache_path.exists() {
        Some(serde_json::from_slice(&fs::read(&cache_path)?)?)
    } else {
        None
    };

    let refetch = match (&cached, mode) {
        (None, _) => true,
        (Some(_), RefreshMode::All) => true,
        (Some(c), RefreshMode::Live) => !is_terminal(media_type, c),
        (Some(c), RefreshMode::Auto) => {
            if is_terminal(media_type, c) {
                false
            } else {
                let fetched = c["_fetched_at"].as_u64().unwrap_or(0);
                now_secs().saturating_sub(fetched) >= ttl_secs
            }
        }
    };

    if let Some(c) = &cached {
        if !refetch {
            return Ok(c.clone());
        }
    }

    eprintln!("· fetching {media_type}-{id} from TMDB");
    match tmdb_get(media_type, id, lang) {
        Ok(mut v) => {
            v["_fetched_at"] = serde_json::json!(now_secs());
            fs::create_dir_all(cache_dir)?;
            fs::write(&cache_path, serde_json::to_vec_pretty(&v)?)?;
            Ok(v)
        }
        // A refresh failed but we have older data: keep the build working.
        Err(e) => match cached {
            Some(c) => {
                eprintln!("  warning: refresh failed ({e}); using cached copy");
                Ok(c)
            }
            None => Err(e),
        },
    }
}

/// One TMDB GET with no caching. Credential: a v4 read access token (Bearer,
/// preferred) or a v3 API key (query param).
fn tmdb_get(media_type: &str, id: u64, lang: &str) -> Result<serde_json::Value> {
    let token = std::env::var("TMDB_READ_TOKEN").ok();
    let api_key = std::env::var("TMDB_API_KEY").ok();
    let mut url = format!("https://api.themoviedb.org/3/{media_type}/{id}?language={lang}");
    let req = if let Some(token) = &token {
        minreq::get(&url).with_header("Authorization", format!("Bearer {token}"))
    } else if let Some(api_key) = &api_key {
        url.push_str(&format!("&api_key={api_key}"));
        minreq::get(&url)
    } else {
        return Err(anyhow::anyhow!(
            "no cache and neither TMDB_READ_TOKEN nor TMDB_API_KEY is set"
        ));
    };
    let resp = req.send()?;
    if resp.status_code != 200 {
        return Err(anyhow::anyhow!(
            "TMDB returned HTTP {} for {media_type}/{id}",
            resp.status_code
        ));
    }
    Ok(serde_json::from_str(resp.as_str()?)?)
}

/// Pull (title, year, overview, poster_url) out of a TMDB payload.
fn extract(media_type: &str, j: &serde_json::Value) -> (String, String, String, String) {
    let title_key = if media_type == "tv" { "name" } else { "title" };
    let date_key = if media_type == "tv" {
        "first_air_date"
    } else {
        "release_date"
    };
    let title = j[title_key].as_str().unwrap_or_default().to_string();
    let year = j[date_key]
        .as_str()
        .unwrap_or_default()
        .chars()
        .take(4)
        .collect();
    let overview = j["overview"].as_str().unwrap_or_default().to_string();
    let poster = match j["poster_path"].as_str() {
        Some(p) if !p.is_empty() => format!("{TMDB_IMG_BASE}{p}"),
        _ => String::new(),
    };
    (title, year, overview, poster)
}

struct Details {
    tagline: String,
    original_title: String,
    rating: String,
    genres: Vec<String>,
    facts: Vec<String>,
    credit: String,
}

/// Pull the richer, display-only fields out of a TMDB payload, localizing the
/// labels through the loaded `t` strings. Everything is optional: a field the
/// payload lacks comes back empty and the template omits it.
fn details(
    media_type: &str,
    j: &serde_json::Value,
    title: &str,
    t: &HashMap<String, String>,
) -> Details {
    let tr = |key: &str| t.get(key).cloned().unwrap_or_default();

    let tagline = j["tagline"].as_str().unwrap_or_default().trim().to_string();

    let orig_key = if media_type == "tv" {
        "original_name"
    } else {
        "original_title"
    };
    let orig = j[orig_key].as_str().unwrap_or_default().trim();
    let original_title = if !orig.is_empty() && orig != title {
        orig.to_string()
    } else {
        String::new()
    };

    let rating = match j["vote_average"].as_f64() {
        Some(v) if v > 0.0 => format!("{v:.1}"),
        _ => String::new(),
    };

    let genres = j["genres"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|g| g["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let mut facts = Vec::new();
    let mut credit = String::new();
    if media_type == "tv" {
        if let Some(n) = j["number_of_seasons"].as_i64().filter(|n| *n > 0) {
            let unit = tr(if n == 1 { "season_one" } else { "season_many" });
            facts.push(format!("{n} {unit}"));
        }
        if let Some(n) = j["number_of_episodes"].as_i64().filter(|n| *n > 0) {
            let unit = tr(if n == 1 { "episode_one" } else { "episode_many" });
            facts.push(format!("{n} {unit}"));
        }
        if let Some(s) = j["status"].as_str() {
            let key = match s {
                "Ended" => "status_ended",
                "Returning Series" => "status_returning",
                "Canceled" => "status_canceled",
                "In Production" => "status_in_production",
                _ => "",
            };
            let label = t.get(key).cloned().unwrap_or_else(|| s.to_string());
            if !label.is_empty() {
                facts.push(label);
            }
        }
        let creators: Vec<String> = j["created_by"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|c| c["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if !creators.is_empty() {
            credit = format!("{} {}", tr("created_by"), creators.join(", "));
        }
    } else if let Some(m) = j["runtime"].as_i64().filter(|m| *m > 0) {
        facts.push(fmt_runtime(m));
    }

    Details {
        tagline,
        original_title,
        rating,
        genres,
        facts,
        credit,
    }
}

fn fmt_runtime(min: i64) -> String {
    let (h, m) = (min / 60, min % 60);
    if h > 0 && m > 0 {
        format!("{h}h {m}min")
    } else if h > 0 {
        format!("{h}h")
    } else {
        format!("{m}min")
    }
}

// ---------------------------------------------------------------------------
// Language helpers: a BCP-47-ish code becomes a human label plus a flag emoji
// ---------------------------------------------------------------------------

fn lang_view(code: &str) -> Lang {
    let code = code.trim();
    let mut parts = code.split(['-', '_']);
    let primary = parts.next().unwrap_or_default().to_lowercase();
    let region = parts.next().map(str::to_uppercase);
    let label = match primary.as_str() {
        "es" => "Español",
        "en" => "Inglés",
        "fr" => "Francés",
        "de" => "Alemán",
        "it" => "Italiano",
        "pt" => "Portugués",
        "ja" => "Japonés",
        "ko" => "Coreano",
        "zh" => "Chino",
        "ru" => "Ruso",
        "ca" => "Catalán",
        "gl" => "Gallego",
        "eu" => "Euskera",
        _ => "",
    };
    let label = if label.is_empty() {
        code.to_string()
    } else {
        label.to_string()
    };
    let region = region.unwrap_or_else(|| default_region(&primary));
    Lang {
        code: code.to_string(),
        label,
        flag: flag(&region),
    }
}

fn default_region(lang: &str) -> String {
    match lang {
        "es" | "ca" | "gl" | "eu" => "ES",
        "en" => "GB",
        "fr" => "FR",
        "de" => "DE",
        "it" => "IT",
        "pt" => "PT",
        "ja" => "JP",
        "ko" => "KR",
        "zh" => "CN",
        "ru" => "RU",
        _ => "",
    }
    .to_string()
}

/// Turn a two-letter country code into its regional-indicator flag emoji.
fn flag(region: &str) -> String {
    let r = region.to_uppercase();
    if r.len() == 2 && r.bytes().all(|b| b.is_ascii_alphabetic()) {
        r.chars()
            .map(|c| char::from_u32(0x1F1E6 + (c as u32 - 'A' as u32)).unwrap())
            .collect()
    } else {
        String::new()
    }
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

/// Trim to at most `max` characters, cutting back to a word boundary and
/// appending an ellipsis when anything was dropped.
fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    if let Some(idx) = out.rfind(' ') {
        out.truncate(idx);
    }
    out.push('…');
    out
}

fn md(src: &str) -> String {
    use pulldown_cmark::{Parser, html};
    let mut out = String::new();
    html::push_html(&mut out, Parser::new(src));
    out
}

// ---------------------------------------------------------------------------
// Release technical tags: free strings in, canonical styled badges out. The
// author may type loosely (H265, DD+, 4k) and we normalize for display.
// ---------------------------------------------------------------------------

fn nonempty(o: &Option<String>) -> Option<&str> {
    o.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// Collapse a token to a comparison key: lowercased, separators stripped.
fn norm_key(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '.' | '_'))
        .collect()
}

fn norm_source(s: &str) -> Badge {
    let label = match norm_key(s).as_str() {
        "webdl" => "WEB-DL",
        "webrip" => "WEBRip",
        "bluray" | "bd" => "BluRay",
        "bdremux" | "remux" => "BDRemux",
        "bdrip" | "brrip" => "BDRip",
        "hdtv" => "HDTV",
        "dvdrip" => "DVDRip",
        _ => s.trim(),
    };
    Badge { label: label.to_string(), kind: "neutral".into() }
}

fn norm_resolution(s: &str) -> String {
    match norm_key(s).as_str() {
        "2160p" | "2160" | "4k" | "uhd" => "4K",
        "1080p" | "1080" | "fhd" => "1080p",
        "720p" | "720" | "hd" => "720p",
        _ => s.trim(),
    }
    .to_string()
}

fn norm_hdr(s: &str) -> Badge {
    let (label, kind) = match norm_key(s).as_str() {
        "hdr10" => ("HDR10", "hdr"),
        "hdr10+" | "hdr10plus" => ("HDR10+", "hdr"),
        "dv" | "dovi" | "dolbyvision" => ("DV", "dv"),
        "hlg" => ("HLG", "hdr"),
        "hdr" => ("HDR", "hdr"),
        _ => (s.trim(), "hdr"),
    };
    Badge { label: label.to_string(), kind: kind.to_string() }
}

fn norm_container(s: &str) -> Badge {
    let label = match norm_key(s).as_str() {
        "mkv" => "mkv",
        "mp4" => "mp4",
        "avi" => "avi",
        _ => s.trim(),
    };
    Badge { label: label.to_string(), kind: "neutral".into() }
}

fn norm_video(s: &str) -> Badge {
    let label = match norm_key(s).as_str() {
        "x264" | "h264" | "avc" => "x264",
        "x265" | "h265" | "hevc" => "x265",
        "av1" => "AV1",
        "vp9" => "VP9",
        "xvid" | "divx" => "XviD",
        _ => s.trim(),
    };
    Badge { label: label.to_string(), kind: "neutral".into() }
}

fn norm_audio(s: &str) -> Badge {
    let label = s
        .split_whitespace()
        .map(norm_audio_token)
        .collect::<Vec<_>>()
        .join(" ");
    Badge { label, kind: "neutral".into() }
}

fn norm_audio_token(tok: &str) -> String {
    match norm_key(tok).as_str() {
        "ac3" | "dd" => "AC3",
        "eac3" | "dd+" | "ddp" | "ddplus" => "EAC3",
        "aac" => "AAC",
        "dts" => "DTS",
        "dtshd" | "dtshdma" => "DTS-HD",
        "truehd" => "TrueHD",
        "flac" => "FLAC",
        "opus" => "Opus",
        "mp3" => "MP3",
        "atmos" => "Atmos",
        "dts:x" | "dtsx" => "DTS:X",
        _ => tok.trim(),
    }
    .to_string()
}

fn link_view(url: String) -> LinkView {
    let size = parse_size(&url).map(human_size).unwrap_or_default();
    let episode = episode_label(&url);
    LinkView { url, size, episode }
}

/// Extract the `size=<bytes>` query parameter from a Rucio URL, if present.
fn parse_size(url: &str) -> Option<u64> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == "size").then(|| v.parse::<u64>().ok())?
    })
}

/// Extract the raw `name=<...>` query parameter from a Rucio URL, if present.
/// The value keeps its percent-encoding; decode with `percent_decode` before
/// scanning for anything separated by spaces.
fn link_name(url: &str) -> &str {
    let Some(query) = url.split_once('?') else {
        return "";
    };
    query
        .1
        .split('&')
        .find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            (k == "name").then_some(v)
        })
        .unwrap_or("")
}

/// Decode a query value's percent-escapes (`%20` → space) and `+` → space, so
/// an encoded space doesn't glue its digits onto an episode marker.
fn percent_decode(s: &str) -> Vec<u8> {
    let hex = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push(hi << 4 | lo);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Episode-number matchers for the series episode column, most specific first.
/// Each puts the first episode in capture 1 and, for a multi-episode range, the
/// last episode in capture 2. Modeled on the filename patterns Kodi documents
/// (kodi.wiki, Naming_video_files/Episodes): `SxxEyy` with all its separators
/// (`S1E2`, `S01.E02`, `S01_E02`, `S01xE02`) and `S01E01-E04` ranges; `NxEE`
/// with `1x01-1x04` ranges; no-season `epNN`/`ep_02` with `ep01-ep04` ranges;
/// plus a flat `NN.-.` anime convention. A range's tail must repeat an episode
/// marker, so `S01E01-1080p` reads as a single episode, not 1–1080. The leading
/// class on the `NxEE` rule rejects resolutions like `1920x1080`. The 3-digit
/// absolute `102` form is left out on purpose — it collides with resolutions
/// and years.
static EPISODE_RES: LazyLock<[Regex; 4]> = LazyLock::new(|| {
    [
        Regex::new(r"(?i)s\d{1,3}[ ._x-]?e(\d{1,3})(?:\s*-\s*(?:s\d{1,3}[ ._x-]?)?e[ ._]?(\d{1,3}))?")
            .unwrap(),
        Regex::new(r"(?i)(?:^|[^\dx])\d{1,2}x(\d{1,3})(?:\s*-\s*\d{1,2}x(\d{1,3}))?").unwrap(),
        Regex::new(r"(?i)(?:^|[^a-z])ep[ _.]?(\d{1,3})(?:\s*-\s*ep[ _.]?(\d{1,3}))?").unwrap(),
        Regex::new(r"(?:^|[ ._])(\d{1,3})[ ._]-[ ._]").unwrap(),
    ]
});

/// Strip leading zeros from a run of digits, keeping at least one (`05` → `5`).
fn strip_zeros(digits: &str) -> String {
    let t = digits.trim_start_matches('0');
    if t.is_empty() { "0".to_string() } else { t.to_string() }
}

/// Pull the episode number out of a link name for the series episode column.
/// Returns an empty string when no marker is found; a range comes back as
/// `first–last` (e.g. `1–4`).
fn episode_label(url: &str) -> String {
    let name = String::from_utf8_lossy(&percent_decode(link_name(url))).into_owned();
    for re in EPISODE_RES.iter() {
        let Some(caps) = re.captures(&name) else {
            continue;
        };
        let first = strip_zeros(&caps[1]);
        return match caps.get(2) {
            Some(last) if strip_zeros(last.as_str()) != first => {
                format!("{first}\u{2013}{}", strip_zeros(last.as_str()))
            }
            _ => first,
        };
    }
    String::new()
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

fn release_view(r: RawRelease) -> ReleaseView {
    let lang = r.language.unwrap_or_default();
    let audio: Vec<Lang> = lang.audio.iter().map(|c| lang_view(c)).collect();
    let subtitles: Vec<Lang> = lang.subtitles.iter().map(|c| lang_view(c)).collect();
    let date = r.date.map(|d| d.to_string()).unwrap_or_default();
    let ripper = r.ripper.unwrap_or_default();
    let origin = r.origin.unwrap_or_default();

    // Technical badges, in reading order: source · resolution · HDR · video ·
    // audio · container.
    let mut badges = Vec::new();
    if let Some(s) = nonempty(&r.source) {
        badges.push(norm_source(s));
    }
    if let Some(s) = nonempty(&r.resolution) {
        badges.push(Badge { label: norm_resolution(s), kind: "res".into() });
    }
    for h in &r.hdr {
        if !h.trim().is_empty() {
            badges.push(norm_hdr(h));
        }
    }
    let codec = r.codec.unwrap_or_default();
    if let Some(v) = nonempty(&codec.video) {
        badges.push(norm_video(v));
    }
    if let Some(a) = nonempty(&codec.audio) {
        badges.push(norm_audio(a));
    }
    if let Some(c) = nonempty(&r.container) {
        badges.push(norm_container(c));
    }

    ReleaseView {
        season: r.season,
        header: r.header.unwrap_or_default(),
        note_html: r.note.as_deref().map(md).unwrap_or_default(),
        badges,
        date,
        ripper,
        origin,
        audio,
        subtitles,
        links: r.links.into_iter().map(link_view).collect(),
    }
}

fn default_tmdb_lang(lang: &str) -> String {
    match lang {
        "es" => "es-ES",
        "en" => "en-US",
        other => other,
    }
    .to_string()
}

/// Collect every `*.toml` under `dir`, recursively, so content/ can be laid out
/// however the author likes (flat, or split into tv/ and movies/). The result is
/// sorted for a stable, reproducible build order.
fn collect_toml(dir: &Path) -> Vec<std::path::PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().and_then(|x| x.to_str()) == Some("toml") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out.sort();
    out
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    // The catalog — its config.toml, content/ and cache/ — can live in a
    // separate directory (e.g. a private content repo); select it with
    // CATALOG_DIR (default: here). A missing config.toml falls back to the
    // bundled config.example.toml, so a fresh checkout still builds the demo.
    let catalog = std::env::var("CATALOG_DIR").unwrap_or_else(|_| ".".to_string());
    let catalog = Path::new(&catalog);
    let config_path = {
        let own = catalog.join("config.toml");
        if own.exists() {
            own
        } else {
            Path::new("config.example.toml").to_path_buf()
        }
    };
    let cfg: SiteConfig = toml::from_str(
        &fs::read_to_string(&config_path)
            .with_context(|| format!("reading {}", config_path.display()))?,
    )?;
    let tmdb_lang = cfg
        .tmdb_language
        .clone()
        .unwrap_or_else(|| default_tmdb_lang(&cfg.lang));

    // Cache policy: `--refresh-all` re-fetches everything, `--refresh-live`
    // re-fetches only ongoing series; otherwise ongoing series honor the TTL.
    let args: Vec<String> = std::env::args().collect();
    let mode = if args.iter().any(|a| a == "--refresh-all") {
        RefreshMode::All
    } else if args.iter().any(|a| a == "--refresh-live") {
        RefreshMode::Live
    } else {
        RefreshMode::Auto
    };
    let ttl_secs = cfg.tmdb_refresh_days.unwrap_or(30).saturating_mul(86_400);

    // content/ and cache/ sit under the catalog directory alongside its config.
    let content_dir = catalog.join(CONTENT_DIR);
    let cache_dir = catalog.join(CACHE_DIR);

    let locale: HashMap<String, String> = {
        let path = format!("i18n/{}.toml", cfg.lang);
        toml::from_str(&fs::read_to_string(&path).with_context(|| format!("reading {path}"))?)?
    };

    let mut tera = Tera::new();
    tera.load_from_glob("templates/**/*.html")
        .context("loading templates")?;

    // Read and enrich every ficha, scanning content/ recursively so its layout
    // is free (flat, or split into tv/ and movies/). Fall back to the documented
    // examples/ when content/ has no entries, so a fresh checkout still renders a
    // demo.
    let mut entries = collect_toml(&content_dir);
    let source = if entries.is_empty() {
        entries = collect_toml(Path::new(EXAMPLES_DIR));
        EXAMPLES_DIR
    } else {
        CONTENT_DIR
    };
    let mut fichas: Vec<(FichaView, Card)> = Vec::new();
    for path in entries {
        let raw: RawFicha = toml::from_str(&fs::read_to_string(&path)?)
            .with_context(|| format!("parsing {}", path.display()))?;
        let slug = path.file_stem().unwrap().to_string_lossy().into_owned();

        let payload = tmdb_fetch(&raw.media_type, raw.id, &tmdb_lang, mode, ttl_secs, &cache_dir)
            .with_context(|| format!("TMDB data for {slug}"))?;
        let (title, year, overview, poster) = extract(&raw.media_type, &payload);
        let det = details(&raw.media_type, &payload, &title, &locale);

        // Aggregate the listing highlights before the releases are consumed.
        let mut seasons = std::collections::BTreeSet::new();
        let mut resolutions: Vec<String> = Vec::new();
        for r in &raw.releases {
            if let Some(s) = r.season {
                seasons.insert(s);
            }
            if let Some(res) = nonempty(&r.resolution) {
                let label = norm_resolution(res);
                if !resolutions.iter().any(|x| *x == label) {
                    resolutions.push(label);
                }
            }
        }
        let season_count = seasons.len();

        // The ficha's date is the most recent of its releases' dates: when a new
        // release (a season, a quality, more episodes) lands, the entry
        // resurfaces as updated. No dated release → empty, so it sorts last.
        let effective_date = raw
            .releases
            .iter()
            .filter_map(|r| r.date.as_ref().map(|d| d.to_string()))
            .max()
            .unwrap_or_default();

        let releases = raw.releases.into_iter().map(release_view).collect();
        let note_html = raw.note.as_deref().map(md).unwrap_or_default();

        let card = Card {
            slug: slug.clone(),
            media_type: raw.media_type.clone(),
            title: title.clone(),
            year: year.clone(),
            date: effective_date,
            poster: poster.clone(),
            overview: truncate(&overview, 160),
            season_count,
            has_highlights: season_count > 0 || !resolutions.is_empty(),
            resolutions,
        };
        let ficha = FichaView {
            slug,
            media_type: raw.media_type,
            title,
            year,
            overview,
            poster,
            note_html,
            tagline: det.tagline,
            original_title: det.original_title,
            rating: det.rating,
            genres: det.genres,
            facts_line: det.facts.join(" · "),
            credit: det.credit,
            releases,
        };
        fichas.push((ficha, card));
    }

    // Default ordering: newest alta first. The browser can re-sort live.
    fichas.sort_by(|a, b| b.1.date.cmp(&a.1.date));

    // Output goes to dist/ by default; CATALOG_OUT redirects it (the container
    // points it at the mounted catalog so the built site lands beside it).
    let out_dir = std::env::var("CATALOG_OUT").unwrap_or_else(|_| OUT_DIR.to_string());
    let out = Path::new(&out_dir);
    // Rebuild the output from scratch so renamed or removed fichas leave no stale pages.
    if out.exists() {
        fs::remove_dir_all(out).with_context(|| format!("clearing {out_dir}/"))?;
    }
    fs::create_dir_all(out)?;

    let base = cfg.base_url.trim_end_matches('/').to_string();

    // Catalog figures for the About modal (built once, rendered on every page).
    let movies = fichas.iter().filter(|(f, _)| f.media_type == "movie").count();
    let series = fichas.iter().filter(|(f, _)| f.media_type == "tv").count();
    let seasons: usize = fichas.iter().map(|(_, c)| c.season_count).sum();
    let releases: usize = fichas.iter().map(|(f, _)| f.releases.len()).sum();
    let links: usize = fichas
        .iter()
        .flat_map(|(f, _)| f.releases.iter())
        .map(|r| r.links.len())
        .sum();
    let stats = serde_json::json!({
        "total": fichas.len(),
        "movies": movies,
        "series": series,
        "seasons": seasons,
        "releases": releases,
        "links": links,
    });

    let site = serde_json::json!({
        "title": cfg.title,
        "lang": cfg.lang,
        "base_url": base,
        "telegram": cfg.contact_telegram,
        "stats": stats,
    });

    // Absolute image for the home-page preview (detail pages use the poster).
    let home_image = match &cfg.og_image {
        Some(img) if !base.is_empty() => format!("{base}/{img}"),
        _ => String::new(),
    };

    // Home.
    let cards: Vec<Card> = fichas.iter().map(|(_, c)| c.clone()).collect();
    let home_meta = Meta {
        doc_title: cfg.title.clone(),
        title: cfg.title.clone(),
        description: cfg.description.clone().unwrap_or_default(),
        image: home_image.clone(),
        url: if base.is_empty() {
            String::new()
        } else {
            format!("{base}/")
        },
    };
    let mut ctx = Context::new();
    ctx.insert("site", &site);
    ctx.insert("t", &locale);
    ctx.insert("meta", &home_meta);
    ctx.insert("cards", &cards);
    fs::write(out.join("index.html"), tera.render("index.html", &ctx)?)?;

    // Search index (kept for future tooling such as Pagefind).
    fs::write(out.join("index.json"), serde_json::to_string(&cards)?)?;

    // Detail pages.
    for (ficha, _) in &fichas {
        let meta = Meta {
            doc_title: format!("{} · {}", ficha.title, cfg.title),
            title: if ficha.year.is_empty() {
                ficha.title.clone()
            } else {
                format!("{} ({})", ficha.title, ficha.year)
            },
            description: if !ficha.tagline.is_empty() {
                ficha.tagline.clone()
            } else {
                truncate(&ficha.overview, 200)
            },
            image: if !ficha.poster.is_empty() {
                ficha.poster.clone()
            } else {
                home_image.clone()
            },
            url: if base.is_empty() {
                String::new()
            } else {
                format!("{base}/{}.html", ficha.slug)
            },
        };
        let mut ctx = Context::new();
        ctx.insert("site", &site);
        ctx.insert("t", &locale);
        ctx.insert("meta", &meta);
        ctx.insert("f", ficha);
        fs::write(
            out.join(format!("{}.html", ficha.slug)),
            tera.render("detail.html", &ctx)?,
        )?;
    }

    copy_dir(Path::new(STATIC_DIR), &out.join("static"))?;

    // The link-preview image belongs to the catalog (its title/branding), so a
    // catalog can ship its own next to its config. When `og_image` points at a
    // file under the catalog directory, copy it into dist at the same path,
    // overriding the bundled default.
    if let Some(img) = cfg.og_image.as_deref() {
        let src = catalog.join(img);
        if src.is_file() {
            let dst = out.join(img);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&src, &dst)
                .with_context(|| format!("copying og-image {}", src.display()))?;
        }
    }

    if source == EXAMPLES_DIR {
        eprintln!("note: no entries under {}/ — rendered {EXAMPLES_DIR}/ as a demo", content_dir.display());
    }
    println!("Generated {} fichas into {out_dir}/", fichas.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::episode_label;

    /// Wrap a (possibly percent-encoded) file name into a Rucio link.
    fn ep(name: &str) -> String {
        episode_label(&format!("rucio:deadbeef?name={name}&size=1"))
    }

    #[test]
    fn single_episode_forms() {
        // SxxEyy and all its documented separators (kodi.wiki).
        assert_eq!(ep("Angel.S01E01.mkv"), "1");
        assert_eq!(ep("Angel.S1E2.mkv"), "2");
        assert_eq!(ep("Angel.S01.E02.mkv"), "2");
        assert_eq!(ep("Angel.S01_E02.mkv"), "2");
        assert_eq!(ep("Angel.S01xE02.mkv"), "2");
        // NxEE and no-season epNN.
        assert_eq!(ep("Angel.1x05.mkv"), "5");
        assert_eq!(ep("Angel.ep02.mkv"), "2");
        assert_eq!(ep("Angel.ep_02.mkv"), "2");
        // Leading zeros are stripped.
        assert_eq!(ep("Show.S03E09.mkv"), "9");
    }

    #[test]
    fn multi_episode_ranges() {
        assert_eq!(ep("Angel.S01E01-E04.mkv"), "1\u{2013}4");
        assert_eq!(ep("Angel.s01e01-s01e04.mkv"), "1\u{2013}4");
        assert_eq!(ep("Angel.1x01-1x04.mkv"), "1\u{2013}4");
        assert_eq!(ep("Angel.ep01-ep04.mkv"), "1\u{2013}4");
    }

    #[test]
    fn percent_encoded_name_is_decoded() {
        // The encoded space must not glue "20" onto the season number.
        assert_eq!(ep("Adult%20Material%201x01%20Rosebud.mkv"), "1");
    }

    #[test]
    fn flat_anime_numbering() {
        assert_eq!(ep("Planetes.01.-.En.la.exosfera.mkv"), "1");
        assert_eq!(ep("Naruto%20-%2015%20-%20Title.mkv"), "15");
    }

    #[test]
    fn false_positives_are_rejected() {
        // A resolution is not an episode marker.
        assert_eq!(ep("Movie.1920x1080.BluRay.mkv"), "");
        // A range tail must repeat a marker, so the resolution is not an end.
        assert_eq!(ep("Angel.S01E01-1080p.mkv"), "1");
        // "ep" inside a word must not trigger the no-season rule.
        assert_eq!(ep("Deep.Blue.Sea.mkv"), "");
        // The dash-delimited "01" is the episode; the "10b-AC3" tag and the
        // "1080" resolution next to it must not interfere.
        assert_eq!(ep("Planetes.01.-.Title.HDrip.1080.HEVC.10b-AC3.mkv"), "1");
        // A plain number without the dash delimiter is not a flat episode.
        assert_eq!(ep("Planetes.01.HEVC.10b-AC3.mkv"), "");
        assert_eq!(ep("Show.Title.x264-FLUX.mkv"), "");
        // No marker at all.
        assert_eq!(ep("Some.Movie.2021.BluRay.mkv"), "");
    }

    #[test]
    fn explicit_marker_wins_over_title_numbers() {
        // The 1x36 marker, not the "28" in the episode title.
        assert_eq!(ep("Death.Note.1x36.28.De.Enero.mkv"), "36");
    }
}
