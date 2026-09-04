//! Native Eclipse addon resolver.
//!
//! The addon URL and token-bearing media URLs deliberately never cross the Rust/webview boundary.
//! This module discovers Eclipse's installed Cloud addon, matches a YouTube Music queue item, and
//! returns only the playable URL to the backend player.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use base64::Engine;
use futures_util::future::join_all;
use regex::Regex;
use serde_json::Value;
use tokio::sync::Mutex;
use unicode_normalization::UnicodeNormalization;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const SOURCE_TTL: Duration = Duration::from_secs(15);
const MATCH_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const MISS_TTL: Duration = Duration::from_secs(10);
const FAILED_STREAM_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone)]
struct Source {
    base_url: String,
    provider: Option<String>,
    cloud: bool,
    settings: Vec<(String, String)>,
}

#[derive(Clone)]
struct Candidate {
    id: String,
    title: String,
    artist: String,
    album: Option<String>,
    duration_ms: Option<i64>,
    isrc: Option<String>,
    direct_url: Option<String>,
    format: Option<String>,
    quality: Option<String>,
}

#[derive(Clone)]
struct CachedMatch {
    candidates: Vec<Candidate>,
    expires_at: Instant,
}

#[derive(Clone)]
pub struct TrackQuery {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    /// Individual linked artist names from YouTube's structured credit runs. A catalog result may
    /// name only the primary act while YouTube prints every member/contributor in one long line.
    pub artist_aliases: Vec<String>,
    pub album: Option<String>,
    pub duration_ms: Option<i64>,
}

pub struct EclipseStream {
    pub url: String,
    pub provider: String,
    pub delivered_quality: String,
}

impl EclipseStream {
    pub fn client_label(&self) -> String {
        format!("eclipse:{}:{}", self.provider.to_lowercase(), self.delivered_quality)
    }
}

#[derive(Clone, Copy)]
pub enum RequestedQuality {
    Lossless,
    HiRes,
}

impl RequestedQuality {
    pub fn from_setting(value: Option<&str>) -> Self {
        match value {
            Some("lossless") => Self::Lossless,
            _ => Self::HiRes,
        }
    }

    fn api_value(self) -> &'static str {
        match self {
            Self::Lossless => "LOSSLESS",
            Self::HiRes => "HI_RES_LOSSLESS",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EclipseError {
    #[error("Eclipse addon is unavailable")]
    Unavailable,
    #[error("Eclipse addon data is invalid")]
    InvalidData,
    #[error("Eclipse addon request failed")]
    Request,
}

pub struct EclipseResolver {
    client: reqwest::Client,
    sources: Mutex<Option<(Vec<Source>, Instant)>>,
    matches: Mutex<HashMap<String, CachedMatch>>,
    failed_streams: Mutex<HashMap<String, Instant>>,
}

impl EclipseResolver {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("build Eclipse addon HTTP client");
        Self {
            client,
            sources: Mutex::new(None),
            matches: Mutex::new(HashMap::new()),
            failed_streams: Mutex::new(HashMap::new()),
        }
    }

    pub async fn mark_stream_failed(&self, video_id: &str) {
        let now = Instant::now();
        let mut failed = self.failed_streams.lock().await;
        failed.retain(|_, at| now.duration_since(*at) < FAILED_STREAM_TTL);
        failed.insert(video_id.to_owned(), now);
    }

    async fn is_suppressed(&self, video_id: &str) -> bool {
        let now = Instant::now();
        let mut failed = self.failed_streams.lock().await;
        failed.retain(|_, at| now.duration_since(*at) < FAILED_STREAM_TTL);
        failed.contains_key(video_id)
    }

    pub async fn resolve(
        &self,
        track: &TrackQuery,
        quality: RequestedQuality,
    ) -> Result<Option<EclipseStream>, EclipseError> {
        if self.is_suppressed(&track.video_id).await {
            return Ok(None);
        }
        let sources = self.discover_sources().await?;
        let mut best = None;
        for source in sources {
            let resolved = match self.stream_from_source(track, &source, quality).await {
                Ok(stream) => stream,
                Err(_) => continue,
            };
            let Some(stream) = resolved else { continue };
            let preferred = stream.provider.eq_ignore_ascii_case("QOBUZ")
                || source.provider.as_deref() == Some("QOBUZ");
            if preferred || source.provider.is_none() {
                return Ok(Some(stream));
            }
            best.get_or_insert(stream);
        }
        Ok(best)
    }

    async fn discover_sources(&self) -> Result<Vec<Source>, EclipseError> {
        if let Ok(url) = std::env::var("LIMUSIC_ECLIPSE_ADDON_URL") {
            if !url.trim().is_empty() {
                return Ok(vec![Source {
                    provider: provider_from_text(&url),
                    base_url: clean_base_url(&url),
                    cloud: true,
                    settings: Vec::new(),
                }]);
            }
        }
        let now = Instant::now();
        if let Some((sources, expires_at)) = &*self.sources.lock().await {
            if *expires_at > now {
                return Ok(sources.clone());
            }
        }
        let addons = load_installed_addons()?;
        let mut sources = parse_sources(&addons, "QOBUZ");
        if sources.is_empty() {
            return Err(EclipseError::Unavailable);
        }
        sources.sort_by_key(|source| match source.provider.as_deref() {
            Some("QOBUZ") => 0,
            None => 1,
            _ => 2,
        });
        *self.sources.lock().await = Some((sources.clone(), now + SOURCE_TTL));
        Ok(sources)
    }

    async fn stream_from_source(
        &self,
        track: &TrackQuery,
        source: &Source,
        quality: RequestedQuality,
    ) -> Result<Option<EclipseStream>, EclipseError> {
        let candidates = self.search_candidates(track, source).await?;
        let mut best = None;
        let mut attempted = HashSet::new();
        for candidate in candidates.into_iter().take(4) {
            if let Some(url) = candidate.direct_url.as_deref().filter(|url| playable_url(url)) {
                let provider = provider_from_text(url)
                    .or_else(|| source.provider.clone())
                    .unwrap_or_else(|| "ECLIPSE".to_owned());
                return Ok(Some(EclipseStream {
                    url: url.to_owned(),
                    provider,
                    delivered_quality: delivered_quality(
                        candidate.quality.as_deref(),
                        None,
                        None,
                        candidate.format.as_deref(),
                    ),
                }));
            }

            let mut ids = Vec::new();
            if source.cloud {
                if let Some(isrc) = &candidate.isrc {
                    if let Ok(value) = self
                        .request_json(source, "tidal/resolve-isrc", &[("isrc", isrc.clone())])
                        .await
                    {
                        if let Some(id) = value
                            .get("trackId")
                            .or_else(|| value.get("id"))
                            .or_else(|| value.pointer("/track/id"))
                            .and_then(value_as_id)
                        {
                            ids.push(id);
                        }
                    }
                }
            }
            ids.push(candidate.id.clone());

            for id in ids {
                if !attempted.insert(id.clone()) {
                    continue;
                }
                let (path, query) = if source.cloud {
                    (
                        "music/stream".to_owned(),
                        vec![("trackId", id), ("quality", quality.api_value().to_owned())],
                    )
                } else {
                    (
                        format!("stream/{}", urlencoding::encode(&id)),
                        vec![("quality", quality.api_value().to_owned())],
                    )
                };
                let Ok(value) = self.request_json(source, &path, &query).await else { continue };
                let stream = value.get("stream").or_else(|| value.get("data")).unwrap_or(&value);
                let Some(url) = stream
                    .get("url")
                    .or_else(|| stream.get("streamURL"))
                    .or_else(|| stream.get("streamUrl"))
                    .and_then(Value::as_str)
                    .filter(|url| playable_url(url))
                else {
                    continue;
                };
                let provider = stream
                    .get("provider")
                    .or_else(|| stream.get("service"))
                    .or_else(|| stream.get("source"))
                    .and_then(Value::as_str)
                    .and_then(provider_from_text)
                    .or_else(|| provider_from_text(url))
                    .or_else(|| source.provider.clone())
                    .unwrap_or_else(|| "ECLIPSE".to_owned());
                let bit_depth = stream.get("bitDepth").and_then(Value::as_i64);
                let sample_rate = stream.get("sampleRate").and_then(Value::as_i64);
                let resolved = EclipseStream {
                    url: url.to_owned(),
                    provider,
                    delivered_quality: delivered_quality(
                        stream.get("quality").and_then(Value::as_str),
                        bit_depth,
                        sample_rate,
                        stream
                            .get("codec")
                            .or_else(|| stream.get("format"))
                            .and_then(Value::as_str),
                    ),
                };
                let hi_res = bit_depth.is_some_and(|bits| bits > 16)
                    || sample_rate.is_some_and(|rate| rate > 48_000)
                    || stream
                        .get("quality")
                        .and_then(Value::as_str)
                        .is_some_and(|q| q.eq_ignore_ascii_case("HI_RES_LOSSLESS"));
                if hi_res {
                    return Ok(Some(resolved));
                }
                best.get_or_insert(resolved);
            }
        }
        Ok(best)
    }

    async fn search_candidates(
        &self,
        track: &TrackQuery,
        source: &Source,
    ) -> Result<Vec<Candidate>, EclipseError> {
        let cache_key = format!(
            "{}|{}|{}|{}",
            source.base_url,
            normalize_title(&track.title),
            normalize_text(&track.artist),
            track.duration_ms.unwrap_or_default() / 1000
        );
        let now = Instant::now();
        if let Some(cached) = self.matches.lock().await.get(&cache_key) {
            if cached.expires_at > now {
                return Ok(cached.candidates.clone());
            }
        }

        let normalized_title = normalize_title(&track.title);
        let normalized_artist = normalize_text(&track.artist);
        let mut queries = vec![
            format!("{} {}", track.title, track.artist),
            format!("{normalized_title} {normalized_artist}"),
            normalized_title.clone(),
            format!("{normalized_artist} {normalized_title}"),
        ];
        queries.retain(|query| !query.trim().is_empty());
        let mut seen = HashSet::new();
        queries.retain(|query| seen.insert(normalize_text(query)));
        let path = if source.cloud { "music/search" } else { "search" };
        let requests = queries.iter().map(|query| async move {
            let params = [("q", query.trim().to_owned()), ("type", "tracks".to_owned())];
            self.request_json(source, path, &params).await
        });
        let results = join_all(requests).await;
        let failures = results.iter().filter(|result| result.is_err()).count();
        let mut raw = Vec::new();
        for value in results.into_iter().flatten() {
            raw.extend(extract_results(&value).iter().cloned());
        }
        if raw.is_empty() && failures == queries.len() {
            return Err(EclipseError::Request);
        }

        let mut unique = HashMap::new();
        for value in raw {
            if let Some(candidate) = candidate_from_value(&value) {
                unique.entry(candidate.id.clone()).or_insert(candidate);
            }
        }
        let mut ranked: Vec<(f64, Candidate)> = unique
            .into_values()
            .filter_map(|candidate| {
                score_candidate(track, &candidate).map(|score| (score, candidate))
            })
            .collect();
        ranked.sort_by(|left, right| right.0.total_cmp(&left.0));
        let candidates: Vec<_> =
            ranked.into_iter().take(8).map(|(_, candidate)| candidate).collect();
        let ttl = if candidates.is_empty() { MISS_TTL } else { MATCH_TTL };
        self.matches.lock().await.insert(
            cache_key,
            CachedMatch { candidates: candidates.clone(), expires_at: now + ttl },
        );
        Ok(candidates)
    }

    async fn request_json(
        &self,
        source: &Source,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Value, EclipseError> {
        for attempt in 0..2 {
            let mut url = reqwest::Url::parse(&format!(
                "{}/{}",
                source.base_url.trim_end_matches('/'),
                path.trim_start_matches('/')
            ))
            .map_err(|_| EclipseError::InvalidData)?;
            {
                let mut pairs = url.query_pairs_mut();
                if !source.cloud {
                    for (key, value) in &source.settings {
                        pairs.append_pair(key, value);
                    }
                }
                for (key, value) in query {
                    pairs.append_pair(key.as_ref(), value);
                }
            }
            let result = self
                .client
                .get(url)
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await;
            if let Ok(response) = result {
                if response.status().is_success() {
                    if let Ok(text) = response.text().await {
                        if let Ok(value) = serde_json::from_str(&text) {
                            return Ok(value);
                        }
                    }
                }
            }
            if attempt == 0 {
                tokio::time::sleep(Duration::from_millis(120)).await;
            }
        }
        Err(EclipseError::Request)
    }
}

/// A cheap settings-page probe that never waits on the resolver's async caches.
/// Discovery is local-file-only on supported platforms, so keeping this synchronous also means a
/// damaged Eclipse preferences file cannot hold the whole Settings dialog on its loading state.
pub fn installed_available() -> bool {
    load_installed_addons()
        .map(|addons| !parse_sources(&addons, "QOBUZ").is_empty())
        .unwrap_or(false)
}

fn clean_base_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    trimmed.strip_suffix("/manifest.json").unwrap_or(trimmed).trim_end_matches('/').to_owned()
}

fn provider_from_text(value: &str) -> Option<String> {
    let value = value.to_lowercase();
    if value.contains("qobuz") {
        Some("QOBUZ".to_owned())
    } else if value.contains("tidal") {
        Some("TIDAL".to_owned())
    } else if value.contains("apple music") || value.contains("applemusic") {
        Some("APPLE MUSIC".to_owned())
    } else {
        None
    }
}

fn scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn parse_sources(addons: &Value, preferred_provider: &str) -> Vec<Source> {
    let Some(addons) = addons.as_array() else { return Vec::new() };
    addons
        .iter()
        .filter_map(|addon| {
            if addon.get("isEnabled").and_then(Value::as_bool) == Some(false) {
                return None;
            }
            let raw_url = addon.get("url").and_then(Value::as_str)?.trim();
            if raw_url.is_empty() {
                return None;
            }
            let manifest = addon.get("manifest").and_then(Value::as_object);
            let name = manifest
                .and_then(|manifest| manifest.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let base_url = clean_base_url(raw_url);
            let cloud = name == "Cloud" || base_url.contains("api.eclipsemusic.app/addon/");
            let resources: HashSet<_> = manifest
                .and_then(|manifest| manifest.get("resources"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            if !cloud && !(resources.contains("search") && resources.contains("stream")) {
                return None;
            }
            let provider_text = [
                addon.get("provider").and_then(Value::as_str),
                Some(name),
                manifest.and_then(|manifest| manifest.get("id")).and_then(Value::as_str),
                manifest.and_then(|manifest| manifest.get("description")).and_then(Value::as_str),
                Some(raw_url),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
            let provider = provider_from_text(&provider_text);

            let fields =
                manifest.and_then(|manifest| manifest.get("settings")).and_then(|settings| {
                    settings.as_array().or_else(|| settings.get("fields").and_then(Value::as_array))
                });
            let mut settings = HashMap::new();
            if let Some(fields) = fields {
                for field in fields {
                    let Some(key) = field.get("key").and_then(Value::as_str) else { continue };
                    if let Some(value) = field.get("default").and_then(scalar_string) {
                        settings.insert(key.to_owned(), value);
                    }
                    let preferred = field
                        .get("options")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|option| {
                            let label =
                                option.get("label").and_then(Value::as_str).unwrap_or_default();
                            let value =
                                option.get("value").and_then(scalar_string).unwrap_or_default();
                            format!("{label} {value}")
                                .to_lowercase()
                                .contains(&preferred_provider.to_lowercase())
                        })
                        .max_by_key(|option| {
                            let text = format!(
                                "{} {}",
                                option.get("label").and_then(Value::as_str).unwrap_or_default(),
                                option.get("value").and_then(scalar_string).unwrap_or_default()
                            );
                            Regex::new("(?i)hi.?res|192|24.?bit").unwrap().is_match(&text)
                        });
                    if let Some(value) =
                        preferred.and_then(|option| option.get("value")).and_then(scalar_string)
                    {
                        settings.insert(key.to_owned(), value);
                    }
                }
            }
            for key in ["settingValues", "settingsValues", "configuration", "config"] {
                if let Some(saved) = addon.get(key).and_then(Value::as_object) {
                    for (key, value) in saved {
                        if let Some(value) = scalar_string(value) {
                            settings.insert(key.clone(), value);
                        }
                    }
                    break;
                }
            }
            Some(Source { base_url, provider, cloud, settings: settings.into_iter().collect() })
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn installed_addons_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(
        "Library/Containers/com.debridmusic.app/Data/Library/Preferences/com.debridmusic.app.plist",
    ))
}

#[cfg(target_os = "windows")]
fn installed_addons_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(std::env::var_os("USERPROFILE")?).join("AppData/Local")))?;
    Some(base.join("EclipseMusic/installed_addons.json"))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn installed_addons_path() -> Option<PathBuf> {
    None
}

#[cfg(target_os = "macos")]
fn load_installed_addons() -> Result<Value, EclipseError> {
    let plist = plist::Value::from_file(installed_addons_path().ok_or(EclipseError::Unavailable)?)
        .map_err(|_| EclipseError::Unavailable)?;
    let stored = plist
        .as_dictionary()
        .and_then(|dict| dict.get("installed_community_addons"))
        .ok_or(EclipseError::InvalidData)?;
    let bytes = if let Some(data) = stored.as_data() {
        data.to_vec()
    } else if let Some(encoded) = stored.as_string() {
        base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|_| EclipseError::InvalidData)?
    } else {
        return Err(EclipseError::InvalidData);
    };
    serde_json::from_slice(&bytes).map_err(|_| EclipseError::InvalidData)
}

#[cfg(target_os = "windows")]
fn load_installed_addons() -> Result<Value, EclipseError> {
    let bytes = std::fs::read(installed_addons_path().ok_or(EclipseError::Unavailable)?)
        .map_err(|_| EclipseError::Unavailable)?;
    let text = String::from_utf8(bytes).map_err(|_| EclipseError::InvalidData)?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|_| EclipseError::InvalidData)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn load_installed_addons() -> Result<Value, EclipseError> {
    Err(EclipseError::Unavailable)
}

fn extract_results(value: &Value) -> &[Value] {
    value
        .get("tracks")
        .and_then(Value::as_array)
        .or_else(|| value.pointer("/data/tracks").and_then(Value::as_array))
        .or_else(|| value.get("data").and_then(Value::as_array))
        .or_else(|| value.get("results").and_then(Value::as_array))
        .or_else(|| value.pointer("/results/items").and_then(Value::as_array))
        .or_else(|| value.pointer("/data/items").and_then(Value::as_array))
        .or_else(|| value.get("items").and_then(Value::as_array))
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn value_as_id(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned).or_else(|| value.as_i64().map(|id| id.to_string()))
}

fn candidate_from_value(value: &Value) -> Option<Candidate> {
    let id = value.get("id").or_else(|| value.get("trackId")).and_then(value_as_id)?;
    let title =
        value.get("title").or_else(|| value.get("name")).and_then(Value::as_str)?.trim().to_owned();
    let credited = value
        .get("artists")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|artist| artist.get("name").and_then(Value::as_str).or_else(|| artist.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    let artist = value
        .pointer("/artist/name")
        .or_else(|| value.get("artist"))
        .and_then(Value::as_str)
        .filter(|artist| !artist.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or(credited);
    if artist.is_empty() {
        return None;
    }
    let album = value
        .pointer("/album/title")
        .or_else(|| value.pointer("/album/name"))
        .or_else(|| value.get("album"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let duration_ms = value
        .get("durationMs")
        .and_then(Value::as_f64)
        .map(|duration| duration.round() as i64)
        .or_else(|| {
            value.get("duration").and_then(Value::as_f64).map(|duration| {
                if duration > 10_000.0 { duration } else { duration * 1000.0 }.round() as i64
            })
        });
    Some(Candidate {
        id,
        title,
        artist,
        album,
        duration_ms,
        isrc: value.get("isrc").and_then(Value::as_str).map(str::to_owned),
        direct_url: value
            .get("streamURL")
            .or_else(|| value.get("streamUrl"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        format: value
            .get("format")
            .or_else(|| value.get("codec"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        quality: value.get("quality").and_then(Value::as_str).map(str::to_owned),
    })
}

fn normalize_text(value: &str) -> String {
    static DECORATION: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(
            r"(?iu)\s*[\[(](?:official\s+)?(?:music\s+)?(?:video|audio|lyrics?|visuali[sz]er)[\])]",
        )
        .unwrap()
    });
    static FEATURE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?iu)\s+(?:feat\.?|ft\.?)\s+").unwrap());
    static NON_WORD: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"[^\p{L}\p{N}]+").unwrap());
    let decomposed: String = value.nfkd().collect();
    let undecorated = DECORATION.replace_all(&decomposed, " ");
    let unfeatured = FEATURE.replace_all(&undecorated, " ");
    let ampersands = unfeatured.replace('&', " and ");
    NON_WORD.replace_all(&ampersands, " ").trim().to_lowercase()
}

fn normalize_title(value: &str) -> String {
    static TITLE_FEATURE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?iu)(?:\s*[\[(]\s*(?:feat\.?|ft\.?|featuring)\s+.*?[\])])|(?:\s+(?:feat\.?|ft\.?|featuring)\s+.*$)")
            .unwrap()
    });
    normalize_text(&TITLE_FEATURE.replace_all(value, " "))
        .split_whitespace()
        .map(|token| match token {
            "remixed" => "remix",
            "remastered" => "remaster",
            _ => token,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn tokens(value: &str, title: bool) -> HashSet<String> {
    let normalized = if title { normalize_title(value) } else { normalize_text(value) };
    normalized
        .split_whitespace()
        .filter(|token| !matches!(*token, "and" | "with" | "x" | "vs"))
        .map(str::to_owned)
        .collect()
}

fn coverage(expected: &str, actual: &str, title: bool) -> f64 {
    let expected = tokens(expected, title);
    if expected.is_empty() {
        return 0.0;
    }
    let actual = tokens(actual, title);
    expected.iter().filter(|token| actual.contains(*token)).count() as f64 / expected.len() as f64
}

fn version_tokens(value: &str) -> HashSet<String> {
    normalize_text(value)
        .split_whitespace()
        .filter_map(|token| match token {
            "remixed" => Some("remix".to_owned()),
            "acoustic" | "demo" | "edit" | "instrumental" | "karaoke" | "live" | "remix"
            | "reverb" | "slowed" | "sped" => Some(token.to_owned()),
            _ => None,
        })
        .collect()
}

fn score_candidate(expected: &TrackQuery, actual: &Candidate) -> Option<f64> {
    let title_forward = coverage(&expected.title, &actual.title, true);
    let title_reverse = coverage(&actual.title, &expected.title, true);
    // Keep the full credit line, but also compare each linked artist run. YouTube commonly renders
    // a group plus its members ("100 gecs, Laura Les & Dylan Brady") while the lossless catalog
    // correctly credits only the group. Requiring half of the combined words rejected that exact
    // match even though title, duration, album, and ISRC all agreed.
    let (artist_forward, artist_reverse) = std::iter::once(&expected.artist)
        .chain(expected.artist_aliases.iter())
        .map(|artist| {
            (coverage(artist, &actual.artist, false), coverage(&actual.artist, artist, false))
        })
        .max_by(|left, right| left.0.max(left.1).total_cmp(&right.0.max(right.1)))
        .unwrap_or_default();
    if title_forward < 0.8 || title_reverse < 0.8 || artist_forward < 0.5 {
        return None;
    }
    if version_tokens(&expected.title) != version_tokens(&actual.title) {
        return None;
    }
    let exact_title = normalize_title(&expected.title) == normalize_title(&actual.title);
    let tolerance =
        if exact_title && artist_forward.max(artist_reverse) >= 0.75 { 30_000 } else { 18_000 };
    let duration_score = match (expected.duration_ms, actual.duration_ms) {
        (Some(expected), Some(actual)) => {
            let delta = (expected - actual).abs();
            if delta > tolerance {
                return None;
            }
            1.0 - delta as f64 / tolerance as f64
        }
        _ => 0.0,
    };
    let album_score = match (&expected.album, &actual.album) {
        (Some(expected), Some(actual)) => coverage(expected, actual, false) * 0.5,
        _ => 0.0,
    };
    Some(
        if exact_title { 4.0 } else { 0.0 }
            + title_forward
            + title_reverse
            + artist_forward * 2.0
            + album_score
            + duration_score
            + if actual.isrc.is_some() { 0.2 } else { 0.0 },
    )
}

fn playable_url(value: &str) -> bool {
    reqwest::Url::parse(value).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

fn delivered_quality(
    quality: Option<&str>,
    bit_depth: Option<i64>,
    sample_rate: Option<i64>,
    format: Option<&str>,
) -> String {
    if bit_depth.is_some_and(|bits| bits > 16)
        || sample_rate.is_some_and(|rate| rate > 48_000)
        || quality.is_some_and(|quality| quality.eq_ignore_ascii_case("HI_RES_LOSSLESS"))
    {
        "hi-res".to_owned()
    } else if quality.is_some_and(|quality| quality.to_lowercase().contains("lossless"))
        || format.is_some_and(|format| format.eq_ignore_ascii_case("flac"))
    {
        "lossless".to_owned()
    } else {
        "unverified".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str, artist: &str, duration_ms: i64) -> TrackQuery {
        TrackQuery {
            video_id: "video".into(),
            title: title.into(),
            artist: artist.into(),
            artist_aliases: Vec::new(),
            album: None,
            duration_ms: Some(duration_ms),
        }
    }

    fn candidate(title: &str, artist: &str, duration_ms: i64) -> Candidate {
        Candidate {
            id: "1".into(),
            title: title.into(),
            artist: artist.into(),
            album: None,
            duration_ms: Some(duration_ms),
            isrc: None,
            direct_url: None,
            format: None,
            quality: None,
        }
    }

    #[test]
    fn matching_accepts_featured_artist_and_small_duration_difference() {
        let expected = track("Smile (Official Audio)", "Dominic Fike", 87_000);
        let actual = candidate("Smile", "Dominic Fike", 88_200);
        assert!(score_candidate(&expected, &actual).is_some());
    }

    #[test]
    fn matching_rejects_a_different_recording_version() {
        let expected = track("Song", "Artist", 180_000);
        let actual = candidate("Song (Live)", "Artist", 180_000);
        assert!(score_candidate(&expected, &actual).is_none());
    }

    #[test]
    fn matching_accepts_primary_artist_from_structured_credits() {
        let mut expected = track("ringtone", "100 gecs, Laura Les & Dylan Brady", 144_000);
        expected.artist_aliases = vec!["100 gecs".into(), "Laura Les".into(), "Dylan Brady".into()];
        let actual = candidate("ringtone", "100 gecs", 144_000);
        assert!(score_candidate(&expected, &actual).is_some());

        let unrelated = candidate("ringtone", "Ringtone Tribute Band", 144_000);
        assert!(score_candidate(&expected, &unrelated).is_none());
    }

    #[test]
    fn cloud_manifest_stream_only_is_accepted() {
        let addons = serde_json::json!([{
            "isEnabled": true,
            "url": "https://example.invalid/addon/token/manifest.json",
            "manifest": { "name": "Cloud", "resources": ["stream"] }
        }]);
        let sources = parse_sources(&addons, "QOBUZ");
        assert_eq!(sources.len(), 1);
        assert!(sources[0].cloud);
        assert_eq!(sources[0].base_url, "https://example.invalid/addon/token");
    }

    #[test]
    fn ordinary_addon_requires_search_and_stream() {
        let addons = serde_json::json!([{
            "url": "https://example.invalid/manifest.json",
            "manifest": { "name": "Only Search", "resources": ["search"] }
        }]);
        assert!(parse_sources(&addons, "QOBUZ").is_empty());
    }

    #[test]
    fn candidate_payload_shapes_and_units_are_coerced() {
        let value = serde_json::json!({
            "trackId": 42,
            "name": "Smile",
            "artists": [{"name": "Dominic Fike"}],
            "duration": 87
        });
        let parsed = candidate_from_value(&value).unwrap();
        assert_eq!(parsed.id, "42");
        assert_eq!(parsed.duration_ms, Some(87_000));
    }
}
