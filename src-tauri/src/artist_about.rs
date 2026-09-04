//! Optional artist-profile enrichment for the fork's About card.
//!
//! YouTube remains the source of truth for the artist page. This module only fills the gaps that
//! YouTube Music does not publish there: official social links, a Wikipedia biography, and freely
//! hosted artist photos. MusicBrainz provides the stable artist identity and URL relationships;
//! Wikidata/Wikipedia provide the prose and imagery. Every source is public and needs no bundled
//! client secret, which keeps the feature portable across macOS, Windows, and Linux builds.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

use crate::http;

const USER_AGENT: &str = concat!(
    "LiMusic-Lossless/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/builtbyDevon/limusic)"
);

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistAbout {
    pub description: Option<String>,
    pub photos: Vec<String>,
    pub instagram_url: Option<String>,
    pub x_url: Option<String>,
    pub website_url: Option<String>,
    pub wikipedia_url: Option<String>,
}

fn cache() -> &'static Mutex<HashMap<String, Arc<ArtistAbout>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<ArtistAbout>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn musicbrainz_clock() -> &'static Mutex<Option<Instant>> {
    static LAST_REQUEST: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    LAST_REQUEST.get_or_init(|| Mutex::new(None))
}

fn normalized(value: &str) -> String {
    value
        .nfkd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .filter(|c| c.is_alphanumeric())
        .collect()
}

async fn response_json(request: reqwest::RequestBuilder) -> Result<Value, String> {
    let response = request
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .timeout(Duration::from_secs(12))
        .send()
        .await
        .map_err(|e| format!("artist profile request failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("artist profile source returned an error: {e}"))?;
    let body = response.text().await.map_err(|e| format!("artist profile response failed: {e}"))?;
    serde_json::from_str(&body).map_err(|e| format!("artist profile returned invalid data: {e}"))
}

/// MusicBrainz asks clients sharing an IP to start at most one request per second. Serialize only
/// its requests; Wikipedia and Wikidata do not need to wait behind this clock.
async fn musicbrainz(path: &str, query: &[(&str, String)]) -> Result<Value, String> {
    let mut last = musicbrainz_clock().lock().await;
    if let Some(previous) = *last {
        let elapsed = previous.elapsed();
        if elapsed < Duration::from_millis(1100) {
            tokio::time::sleep(Duration::from_millis(1100) - elapsed).await;
        }
    }
    *last = Some(Instant::now());
    drop(last);

    response_json(http::client().get(format!("https://musicbrainz.org/ws/2/{path}")).query(query))
        .await
}

fn relation_urls(details: &Value) -> Vec<(String, String)> {
    details["relations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|relation| {
            Some((
                relation["type"].as_str()?.to_owned(),
                relation.pointer("/url/resource")?.as_str()?.to_owned(),
            ))
        })
        .collect()
}

fn first_relation(urls: &[(String, String)], kind: &str) -> Option<String> {
    urls.iter().find(|(ty, _)| ty == kind).map(|(_, url)| url.clone())
}

fn social_url(urls: &[(String, String)], hosts: &[&str]) -> Option<String> {
    urls.iter().map(|(_, url)| url).find(|url| hosts.iter().any(|host| url.contains(host))).cloned()
}

fn claim_strings(entity: &Value, property: &str) -> Vec<String> {
    entity
        .pointer(&format!("/claims/{property}"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|claim| claim.pointer("/mainsnak/datavalue/value")?.as_str().map(str::to_owned))
        .collect()
}

fn wikidata_id(url: &str) -> Option<&str> {
    let id = url.rsplit('/').next()?.split(['?', '#']).next()?;
    (id.starts_with('Q') && id[1..].chars().all(|c| c.is_ascii_digit())).then_some(id)
}

fn wikipedia_title(url: &str) -> Option<String> {
    let marker = ".wikipedia.org/wiki/";
    let start = url.find(marker)? + marker.len();
    let raw = url.get(start..)?.split(['?', '#']).next()?;
    (!raw.is_empty()).then(|| raw.replace('_', " "))
}

fn commons_photo(filename: &str) -> String {
    format!(
        "https://commons.wikimedia.org/wiki/Special:Redirect/file/{}?width=1600",
        urlencoding::encode(filename)
    )
}

async fn wikipedia_summary(title: &str) -> Result<Value, String> {
    response_json(http::client().get(format!(
        "https://en.wikipedia.org/api/rest_v1/page/summary/{}",
        urlencoding::encode(title)
    )))
    .await
}

async fn fetch(name: &str, channel_id: &str) -> Result<ArtistAbout, String> {
    let safe_name = name.trim().replace('"', "");
    if safe_name.is_empty() || safe_name.chars().count() > 200 {
        return Ok(ArtistAbout::default());
    }

    let search = musicbrainz(
        "artist",
        &[
            ("query", format!("artist:\"{safe_name}\"")),
            ("fmt", "json".into()),
            ("limit", "8".into()),
        ],
    )
    .await?;
    let wanted = normalized(&safe_name);
    let candidate = search["artists"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|artist| artist["score"].as_i64().unwrap_or_default() >= 90)
        .find(|artist| artist["name"].as_str().is_some_and(|value| normalized(value) == wanted))
        .or_else(|| {
            search["artists"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|artist| artist["score"].as_i64().unwrap_or_default() >= 98)
        });
    let Some(mbid) = candidate.and_then(|artist| artist["id"].as_str()) else {
        return Ok(ArtistAbout::default());
    };

    let details = musicbrainz(
        &format!("artist/{mbid}"),
        &[("inc", "url-rels".into()), ("fmt", "json".into())],
    )
    .await?;
    let urls = relation_urls(&details);

    // When MusicBrainz has an explicit YouTube channel relationship, refuse an artist that points
    // at a different UC channel. A missing YouTube relationship is common and is not a rejection.
    let channel_links: Vec<_> = urls
        .iter()
        .map(|(_, url)| url)
        .filter(|url| url.contains("youtube.com/channel/"))
        .collect();
    if !channel_links.is_empty() && !channel_links.iter().any(|url| url.contains(channel_id)) {
        return Ok(ArtistAbout::default());
    }

    let mut about = ArtistAbout {
        instagram_url: social_url(&urls, &["instagram.com/"]),
        x_url: social_url(&urls, &["x.com/", "twitter.com/"]),
        website_url: first_relation(&urls, "official homepage"),
        wikipedia_url: first_relation(&urls, "wikipedia"),
        ..ArtistAbout::default()
    };

    let wikidata = first_relation(&urls, "wikidata");
    let mut wikipedia_title_value = about.wikipedia_url.as_deref().and_then(wikipedia_title);
    if let Some(id) = wikidata.as_deref().and_then(wikidata_id) {
        if let Ok(root) = response_json(
            http::client()
                .get(format!("https://www.wikidata.org/wiki/Special:EntityData/{id}.json")),
        )
        .await
        {
            let entity = &root["entities"][id];
            about
                .photos
                .extend(claim_strings(entity, "P18").into_iter().map(|p| commons_photo(&p)));
            if about.instagram_url.is_none() {
                about.instagram_url = claim_strings(entity, "P2003")
                    .into_iter()
                    .next()
                    .map(|user| format!("https://www.instagram.com/{user}/"));
            }
            if about.x_url.is_none() {
                about.x_url = claim_strings(entity, "P2002")
                    .into_iter()
                    .next()
                    .map(|user| format!("https://x.com/{user}"));
            }
            if about.website_url.is_none() {
                about.website_url = claim_strings(entity, "P856").into_iter().next();
            }
            if wikipedia_title_value.is_none() {
                wikipedia_title_value = entity
                    .pointer("/sitelinks/enwiki/title")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
        }
    }

    if let Some(title) = wikipedia_title_value {
        if let Ok(summary) = wikipedia_summary(&title).await {
            about.description = summary["extract"].as_str().map(str::to_owned);
            if let Some(source) = summary.pointer("/originalimage/source").and_then(Value::as_str) {
                about.photos.insert(0, source.to_owned());
            }
            if about.wikipedia_url.is_none() {
                about.wikipedia_url = summary
                    .pointer("/content_urls/desktop/page")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| {
                        Some(format!(
                            "https://en.wikipedia.org/wiki/{}",
                            urlencoding::encode(&title.replace(' ', "_"))
                        ))
                    });
            }
        }
    }

    let mut seen = HashSet::new();
    about.photos.retain(|photo| seen.insert(photo.clone()));
    about.photos.truncate(4);
    Ok(about)
}

#[tauri::command]
pub async fn get_artist_about(name: String, channel_id: String) -> Result<ArtistAbout, String> {
    let key = format!("{}:{}", normalized(&name), channel_id);
    if let Some(hit) = cache().lock().await.get(&key).cloned() {
        return Ok((*hit).clone());
    }
    let about = fetch(&name, &channel_id).await?;
    cache().lock().await.insert(key, Arc::new(about.clone()));
    Ok(about)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_artist_names_for_exact_matching() {
        assert_eq!(normalized("Beyoncé"), "beyonce");
        assert_eq!(normalized("  93FEETOFSMOKE! "), "93feetofsmoke");
    }

    #[test]
    fn parses_wikidata_and_wikipedia_ids() {
        assert_eq!(wikidata_id("https://www.wikidata.org/wiki/Q151892"), Some("Q151892"));
        assert_eq!(
            wikipedia_title("https://en.wikipedia.org/wiki/Ariana_Grande"),
            Some("Ariana Grande".into())
        );
    }
}
