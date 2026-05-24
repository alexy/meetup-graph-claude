//! LLM-based talk extraction. Replaces parse.rs heuristics with a single Sonnet
//! 4.6 call per event using forced tool calling for structured output.
//!
//! Reads cached event HTML from data/source/<group>/<event_id>.html, decodes the
//! description from the Apollo state, asks the model to extract a list of talks,
//! and writes one TalkRecord JSON per talk into data/talks-llm/.
//!
//! Output schema is byte-identical to data/talks/ records so the same falkor-load
//! binary can ingest either directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, NaiveDate, Utc};
use clap::Parser;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const SCHEMA_VERSION: &str = "1.0";

#[derive(Parser, Debug)]
#[command(about = "Extract talks from cached meetup HTML using an LLM")]
struct Args {
    /// Directory containing cached source HTML (output of `meetup-scraper`)
    #[arg(short, long, default_value = "data/source")]
    source: PathBuf,

    /// Output directory for LLM-extracted talk JSONs
    #[arg(short, long, default_value = "data/talks-llm")]
    output: PathBuf,

    /// Only process these groups (default: all subdirs of --source)
    #[arg(long, value_name = "GROUP")]
    groups: Vec<String>,

    /// Concurrent in-flight API calls
    #[arg(long, default_value_t = 6)]
    concurrency: usize,

    /// Skip events whose extraction file already exists in the output dir
    #[arg(long)]
    skip_existing: bool,

    /// Model id to use
    #[arg(long, default_value = "claude-sonnet-4-6")]
    model: String,

    /// Max events to process (debug / dry-run)
    #[arg(long)]
    limit: Option<usize>,
}

// ── output schema (mirrors models::TalkRecord) ────────────────────────────────

#[derive(Debug, Serialize)]
struct TalkRecord {
    schema_version: String,
    id: String,
    scraped_at: DateTime<Utc>,
    source_file: String,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}

#[derive(Debug, Serialize)]
struct Node {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    properties: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct Edge {
    from: String,
    to: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    properties: Option<serde_json::Value>,
}

// ── LLM tool input schema ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ExtractionResult {
    talks: Vec<ExtractedTalk>,
}

#[derive(Debug, Deserialize)]
struct ExtractedTalk {
    title: String,
    #[serde(default, alias = "abstract", alias = "abstract_text")]
    abstract_text: Option<String>,
    #[serde(default)]
    speakers: Vec<ExtractedSpeaker>,
}

#[derive(Debug, Deserialize)]
struct ExtractedSpeaker {
    name: String,
    #[serde(default)]
    company: Option<String>,
    #[serde(default)]
    bio: Option<String>,
}

// ── event metadata pulled from Apollo state ───────────────────────────────────

#[derive(Debug, Clone)]
struct EventMeta {
    id: String,
    group: String,
    title: String,
    description_md: String,
    date: Option<NaiveDate>,
    datetime: Option<DateTime<Utc>>,
    url: String,
    venue_name: Option<String>,
    venue_address: Option<String>,
    city: Option<String>,
    group_name: String,
}

// ── main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("llm_extract=info".parse()?),
        )
        .init();

    let args = Args::parse();
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .context("ANTHROPIC_API_KEY environment variable not set")?;

    tokio::fs::create_dir_all(&args.output).await?;

    let groups = discover_groups(&args.source, &args.groups).await?;
    info!("groups: {}", groups.join(", "));

    let mut tasks: Vec<(PathBuf, String)> = Vec::new();
    for group in &groups {
        let dir = args.source.join(group);
        let mut entries = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("html") {
                continue;
            }
            tasks.push((path, group.clone()));
        }
    }
    if let Some(limit) = args.limit {
        tasks.truncate(limit);
    }
    info!("found {} events to process", tasks.len());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    let semaphore = Arc::new(Semaphore::new(args.concurrency));
    let api_key = Arc::new(api_key);
    let model = Arc::new(args.model.clone());
    let output_dir = Arc::new(args.output.clone());

    let mut handles = Vec::new();
    for (path, group) in tasks {
        let permit = semaphore.clone().acquire_owned().await?;
        let client = client.clone();
        let api_key = api_key.clone();
        let model = model.clone();
        let output_dir = output_dir.clone();
        let skip_existing = args.skip_existing;

        let handle = tokio::spawn(async move {
            let _permit = permit;
            let event_id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();
            match process_event(
                &path, &group, &event_id, &client, &api_key, &model,
                &output_dir, skip_existing,
            ).await {
                Ok(n) => Ok((event_id, n)),
                Err(e) => Err((event_id, e)),
            }
        });
        handles.push(handle);
    }

    let mut total_events = 0usize;
    let mut total_talks = 0usize;
    let mut total_failed = 0usize;
    for handle in handles {
        match handle.await? {
            Ok((_id, n)) => {
                total_events += 1;
                total_talks += n;
            }
            Err((id, e)) => {
                total_failed += 1;
                error!("event {id}: {e:#}");
            }
        }
    }

    info!("════════════════════════════════════════════");
    info!(
        "done  events={total_events}  talks={total_talks}  failed={total_failed}"
    );
    Ok(())
}

async fn discover_groups(source: &Path, only: &[String]) -> Result<Vec<String>> {
    if !only.is_empty() {
        return Ok(only.to_vec());
    }
    let mut out = Vec::new();
    let mut entries = tokio::fs::read_dir(source).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            if let Some(name) = entry.file_name().to_str() {
                out.push(name.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

// ── per-event pipeline ────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn process_event(
    path: &Path,
    group: &str,
    event_id: &str,
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    output_dir: &Path,
    skip_existing: bool,
) -> Result<usize> {
    let html = tokio::fs::read_to_string(path).await?;
    let meta = extract_event_meta(&html, group, event_id)
        .ok_or_else(|| anyhow!("could not extract event metadata"))?;

    // Skip events with empty descriptions — no extraction possible
    if meta.description_md.trim().len() < 50 {
        info!("  {event_id}: empty description, skipped");
        return Ok(0);
    }

    if skip_existing && already_extracted(output_dir, &meta).await {
        return Ok(0);
    }

    let talks = call_llm(client, api_key, model, &meta).await?;
    let mut written = 0usize;
    for (order, talk) in talks.into_iter().enumerate() {
        if talk.speakers.is_empty() {
            continue;
        }
        let rec = build_talk_record(&talk, order, &meta, path);
        save_talk(output_dir, &rec).await?;
        written += 1;
    }
    info!("  {group}/{event_id}: {written} talks");
    Ok(written)
}

async fn already_extracted(output_dir: &Path, meta: &EventMeta) -> bool {
    let date_str = meta
        .date
        .map(|d| d.format("%Y%m%d").to_string())
        .unwrap_or_else(|| "00000000".to_string());
    let prefix = format!("bythebay-{date_str}-");
    let Ok(mut entries) = tokio::fs::read_dir(output_dir).await else {
        return false;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) {
            // Check whether this event_id appears in any saved record
            let path = entry.path();
            if let Ok(raw) = tokio::fs::read_to_string(&path).await {
                if raw.contains(&format!("\"id\": \"{}\"", meta.id))
                    || raw.contains(&format!("\"{}-{}\"", meta.group, meta.id))
                {
                    return true;
                }
            }
        }
    }
    false
}

// ── HTML / Apollo state parsing ───────────────────────────────────────────────

fn extract_event_meta(html: &str, group: &str, event_id: &str) -> Option<EventMeta> {
    let document = Html::parse_document(html);
    let sel = Selector::parse("script#__NEXT_DATA__").ok()?;
    let script = document.select(&sel).next()?;
    let raw: String = script.text().collect();
    let data: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let apollo = data["props"]["pageProps"]["__APOLLO_STATE__"].as_object()?;

    // Find the Event:<id> entry
    let key = format!("Event:{event_id}");
    let ev = apollo.get(&key)?;

    let title = ev.get("title")?.as_str()?.to_string();
    let description_md = ev.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let datetime_str = ev.get("dateTime").and_then(|v| v.as_str());
    let datetime = datetime_str.and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    let date = datetime.map(|d| d.date_naive());
    let url = ev.get("eventUrl").and_then(|v| v.as_str()).unwrap_or("").to_string();

    // Venue may be a reference like { __ref: "Venue:xxx" }
    let mut venue_name: Option<String> = None;
    let mut venue_address: Option<String> = None;
    let mut city: Option<String> = None;
    if let Some(venue_ref) = ev.get("venue").and_then(|v| v.get("__ref")).and_then(|v| v.as_str()) {
        if let Some(venue) = apollo.get(venue_ref) {
            venue_name = venue.get("name").and_then(|v| v.as_str()).map(str::to_string);
            venue_address = venue.get("address").and_then(|v| v.as_str()).map(str::to_string);
            city = venue.get("city").and_then(|v| v.as_str()).map(str::to_string);
        }
    } else if let Some(venue) = ev.get("venue").and_then(|v| v.as_object()) {
        venue_name = venue.get("name").and_then(|v| v.as_str()).map(str::to_string);
        venue_address = venue.get("address").and_then(|v| v.as_str()).map(str::to_string);
        city = venue.get("city").and_then(|v| v.as_str()).map(str::to_string);
    }

    let group_name = group_display_name(group);

    Some(EventMeta {
        id: event_id.to_string(),
        group: group.to_string(),
        title,
        description_md,
        date,
        datetime,
        url,
        venue_name,
        venue_address,
        city,
        group_name,
    })
}

fn group_display_name(slug: &str) -> String {
    match slug {
        "bay-area-ai" => "Bay Area AI".to_string(),
        "sf-scala" => "SF Scala".to_string(),
        "unstructured-data-sf" => "Unstructured Data SF".to_string(),
        "hadoopsf" => "San Francisco Hadoop Users".to_string(),
        "graphql-by-the-bay" => "GraphQL By the Bay".to_string(),
        "scala-bay" => "Scala Bay".to_string(),
        "sf-data-and-ai-engineering" => "SF Data and AI Engineering".to_string(),
        "big-data-developers-in-nyc" => "Data, Cloud and AI in NYC".to_string(),
        other => other.to_string(),
    }
}

// ── LLM call ──────────────────────────────────────────────────────────────────

async fn call_llm(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    meta: &EventMeta,
) -> Result<Vec<ExtractedTalk>> {
    let tool = serde_json::json!({
        "name": "save_talks",
        "description": "Save the list of talks identified in this meetup event description.",
        "input_schema": {
            "type": "object",
            "properties": {
                "talks": {
                    "type": "array",
                    "description": "List of talks at this event. Empty if there are no identifiable talks (e.g. social events, announcements, generic meetup descriptions).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": {
                                "type": "string",
                                "description": "The talk title. Should be a specific topic, not a generic event title."
                            },
                            "abstract": {
                                "type": ["string", "null"],
                                "description": "The talk abstract / summary. Use null if no abstract is provided in the description."
                            },
                            "speakers": {
                                "type": "array",
                                "description": "Speakers presenting this talk.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "name": {
                                            "type": "string",
                                            "description": "The speaker's full name as a person (e.g. 'Jane Smith', not 'Jane Smith Engineering Inc')."
                                        },
                                        "company": {
                                            "type": ["string", "null"],
                                            "description": "Company or affiliation, if mentioned. Strip role titles (e.g. 'CEO at Acme' → 'Acme')."
                                        }
                                    },
                                    "required": ["name"]
                                }
                            }
                        },
                        "required": ["title", "speakers"]
                    }
                }
            },
            "required": ["talks"]
        }
    });

    let prompt = format!(
        "Extract the structured list of talks from this meetup event.\n\
         \n\
         GROUP: {}\n\
         EVENT TITLE: {}\n\
         EVENT URL: {}\n\
         DATE: {}\n\
         \n\
         DESCRIPTION (markdown):\n\
         ```\n{}\n```\n\
         \n\
         Rules:\n\
         - A real talk has a specific title AND at least one named human speaker.\n\
         - Do NOT invent speakers, titles, or affiliations. Only use facts present in the description.\n\
         - Skip non-talk items: networking, food, registration, Q&A, sponsor pitches, book references, panel headers without speakers.\n\
         - Each panelist on a multi-speaker talk goes in the same talk's speakers array; don't split one talk into one per speaker.\n\
         - If the description has no identifiable talks (announcements, social events, empty agenda), return an empty array.\n\
         - Speaker.name must be a person's full name. Company belongs in `company`, not name. Strip roles like 'CEO at', 'VP, ', 'Software Engineer at'.\n\
         - For abstracts, prefer the verbatim abstract paragraph from the description; do not summarize or paraphrase.",
        meta.group_name, meta.title, meta.url,
        meta.date.map(|d| d.to_string()).unwrap_or_else(|| "unknown".to_string()),
        meta.description_md
    );

    let body = serde_json::json!({
        "model": model,
        "max_tokens": 4096,
        "tools": [tool],
        "tool_choice": {"type": "tool", "name": "save_talks"},
        "messages": [{"role": "user", "content": prompt}],
    });

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let resp = client
            .post(ANTHROPIC_URL)
            .header("x-api-key", api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) if attempt < 4 => {
                warn!("network error (attempt {attempt}): {e:#}");
                tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
                continue;
            }
            Err(e) => return Err(e).context("Anthropic API request"),
        };

        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt < 5 {
            let wait = 2u64.pow(attempt);
            warn!("rate limited, sleeping {wait}s (attempt {attempt})");
            tokio::time::sleep(Duration::from_secs(wait)).await;
            continue;
        }
        if status.is_server_error() && attempt < 4 {
            let wait = 2u64.pow(attempt);
            warn!("server error {status}, retrying in {wait}s");
            tokio::time::sleep(Duration::from_secs(wait)).await;
            continue;
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("Anthropic API HTTP {status}: {body}");
        }

        let value: serde_json::Value = resp.json().await.context("decode API response")?;
        let content = value["content"]
            .as_array()
            .ok_or_else(|| anyhow!("response missing content array"))?;
        let tool_use = content
            .iter()
            .find(|c| c["type"] == "tool_use")
            .ok_or_else(|| anyhow!("response missing tool_use block: {value}"))?;
        let input = tool_use["input"].clone();
        let result: ExtractionResult = serde_json::from_value(input)
            .with_context(|| format!("decode tool input: {tool_use}"))?;
        return Ok(result.talks);
    }
}

// ── record building (mirror of store::build_talk_record) ──────────────────────

fn build_talk_record(
    talk: &ExtractedTalk,
    order: usize,
    event: &EventMeta,
    source_path: &Path,
) -> TalkRecord {
    let date = event.date.unwrap_or_else(|| Utc::now().date_naive());
    let first_speaker = talk.speakers.first().map(|s| s.name.as_str());
    let talk_id = make_talk_id(&date, &talk.title, first_speaker);

    let relative_source = format!("source/{}/{}.html", event.group, event.id);
    let _ = source_path; // path is already encoded in relative_source

    let talk_nid = format!("talk:{talk_id}");
    let event_nid = format!("event:{}-{}", event.group, event.id);
    let group_nid = format!("group:{}", event.group);

    let mut nodes = vec![
        Node {
            id: talk_nid.clone(),
            kind: "Talk".to_string(),
            properties: serde_json::json!({
                "id": talk_id,
                "title": talk.title,
                "abstract": talk.abstract_text,
                "order": order,
            }),
        },
        Node {
            id: event_nid.clone(),
            kind: "Event".to_string(),
            properties: serde_json::json!({
                "id": event.id,
                "title": event.title,
                "date": event.date.map(|d| d.to_string()),
                "datetime": event.datetime.map(|d| d.to_rfc3339()),
                "url": event.url,
                "venue_name": event.venue_name,
                "venue_address": event.venue_address,
                "city": event.city,
            }),
        },
        Node {
            id: group_nid.clone(),
            kind: "Group".to_string(),
            properties: serde_json::json!({
                "slug": event.group,
                "name": event.group_name,
                "url": format!("https://www.meetup.com/{}", event.group),
            }),
        },
    ];

    let mut edges = vec![
        Edge {
            from: talk_nid.clone(),
            to: event_nid.clone(),
            kind: "PRESENTED_AT".to_string(),
            properties: None,
        },
        Edge {
            from: event_nid.clone(),
            to: group_nid.clone(),
            kind: "PART_OF".to_string(),
            properties: None,
        },
    ];

    for speaker in &talk.speakers {
        let sp_nid = format!("speaker:{}", slugify(&speaker.name));
        nodes.push(Node {
            id: sp_nid.clone(),
            kind: "Speaker".to_string(),
            properties: serde_json::json!({
                "name": speaker.name,
                "bio": speaker.bio,
                "company": speaker.company,
            }),
        });
        edges.push(Edge {
            from: talk_nid.clone(),
            to: sp_nid,
            kind: "PRESENTED_BY".to_string(),
            properties: None,
        });
    }

    TalkRecord {
        schema_version: SCHEMA_VERSION.to_string(),
        id: talk_id,
        scraped_at: Utc::now(),
        source_file: relative_source,
        nodes,
        edges,
    }
}

async fn save_talk(output_dir: &Path, record: &TalkRecord) -> Result<()> {
    let path = output_dir.join(format!("{}.json", record.id));
    let json = serde_json::to_string_pretty(record)?;
    tokio::fs::write(&path, json).await?;
    Ok(())
}

fn slugify(text: &str) -> String {
    let parts: Vec<String> = text
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let slug = parts.join("-");
    if slug.len() > 60 {
        slug[..60].trim_end_matches('-').to_string()
    } else {
        slug
    }
}

fn make_talk_id(date: &NaiveDate, title: &str, speaker: Option<&str>) -> String {
    let date_str = date.format("%Y%m%d").to_string();
    let title_slug = {
        let s = slugify(title);
        if s.len() > 45 { s[..45].trim_end_matches('-').to_string() } else { s }
    };
    match speaker {
        Some(name) => {
            let sp_slug = {
                let s = slugify(name);
                if s.len() > 30 { s[..30].trim_end_matches('-').to_string() } else { s }
            };
            format!("bythebay-{date_str}-{title_slug}-{sp_slug}")
        }
        None => format!("bythebay-{date_str}-{title_slug}"),
    }
}
