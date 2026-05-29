//! LLM-based talk extraction — batched edition.
//!
//! Reads cached event HTML from `data/source/<group>/<event_id>.html`,
//! decodes the event description from the Apollo `__NEXT_DATA__` state,
//! then sends batches of ~10 event descriptions in a **single API call**
//! and asks the model to extract all talks semantically (no regexes).
//!
//! Each batch call uses a `save_events` forced-tool schema that returns an
//! array of `{event_id, talks}` objects, one per input event.  The LLM
//! understands the content — it is not pattern-matching.
//!
//! Output files are written to `data/talks-llm/`, one JSON per talk.
//! The schema is byte-compatible with `data/talks/` so `load` accepts either.
//!
//! # Usage
//!
//! ```text
//! # Full run (all groups, batch_size=10, concurrency=3)
//! ANTHROPIC_API_KEY=sk-ant-... cargo run --bin llm-extract
//!
//! # Skip events already extracted
//! cargo run --bin llm-extract -- --skip-existing
//!
//! # Process only 30 events for a smoke test
//! cargo run --bin llm-extract -- --limit 30
//!
//! # Smaller batches for very long descriptions
//! cargo run --bin llm-extract -- --batch-size 5
//! ```

use std::collections::{HashMap, HashSet};
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
const SCHEMA_VERSION: &str = "1.1";

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(about = "Extract talks from cached meetup HTML using an LLM (batched)")]
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

    /// Events per LLM batch call
    #[arg(long, default_value_t = 10)]
    batch_size: usize,

    /// Concurrent batch calls in flight at once
    #[arg(long, default_value_t = 3)]
    concurrency: usize,

    /// Skip events already present in the output directory
    #[arg(long)]
    skip_existing: bool,

    /// Model to use
    #[arg(long, default_value = "claude-sonnet-4-6")]
    model: String,

    /// Max output tokens per batch call
    #[arg(long, default_value_t = 8192)]
    max_tokens: u32,

    /// Stop after processing at most this many events (for testing)
    #[arg(long)]
    limit: Option<usize>,
}

// ── Output schema (mirrors models::TalkRecord) ────────────────────────────────

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

// ── LLM response structs ──────────────────────────────────────────────────────

/// Top-level result from the `save_events` tool call.
#[derive(Debug, Deserialize)]
struct BatchResult {
    events: Vec<EventExtractionResult>,
}

#[derive(Debug, Deserialize)]
struct EventExtractionResult {
    event_id: String,
    #[serde(default)]
    talks: Vec<ExtractedTalk>,
}

#[derive(Debug, Deserialize)]
struct ExtractedTalk {
    title: String,
    #[serde(default, alias = "abstract", alias = "abstract_text")]
    abstract_text: Option<String>,
    #[serde(default)]
    speakers: Vec<ExtractedSpeaker>,
    #[serde(default)]
    projects: Vec<ExtractedProject>,
}

#[derive(Debug, Deserialize)]
struct ExtractedSpeaker {
    name: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    company: Option<String>,
    #[serde(default)]
    bio: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct ExtractedProject {
    name: String,
}

// ── Event metadata pulled from the Apollo state ───────────────────────────────

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

// ── Main ──────────────────────────────────────────────────────────────────────

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

    // Pre-scan output dir to build skip-set (event_ids already extracted).
    let extracted: HashSet<String> = if args.skip_existing {
        scan_extracted_events(&args.output)
    } else {
        HashSet::new()
    };
    if !extracted.is_empty() {
        info!("skip-existing: {} event_ids already in output dir", extracted.len());
    }

    // ── Discover and parse HTML files ─────────────────────────────────────────
    let groups = discover_groups(&args.source, &args.groups).await?;
    info!("groups: {}", groups.join(", "));

    // Collect (source_path, EventMeta) for every HTML file we'll process.
    let mut items: Vec<(PathBuf, EventMeta)> = Vec::new();

    for group in &groups {
        let dir = args.source.join(group);
        let mut entries = tokio::fs::read_dir(&dir).await
            .with_context(|| format!("read_dir {}", dir.display()))?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("html") {
                continue;
            }
            let event_id = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };

            if args.skip_existing && extracted.contains(&event_id) {
                continue;
            }

            let html = match tokio::fs::read_to_string(&path).await {
                Ok(h) => h,
                Err(e) => { warn!("read {}: {e}", path.display()); continue; }
            };

            let meta = match extract_event_meta(&html, group, &event_id) {
                Some(m) => m,
                None => { warn!("no Apollo state: {}", path.display()); continue; }
            };

            // Skip events with no description — nothing for the LLM to work with.
            if meta.description_md.trim().len() < 50 {
                continue;
            }

            items.push((path, meta));
        }
    }

    if let Some(limit) = args.limit {
        items.truncate(limit);
    }

    let total_events = items.len();
    let batch_size = args.batch_size.max(1);
    let total_batches = (total_events + batch_size - 1) / batch_size;
    info!("events={total_events}  batch_size={batch_size}  batches={total_batches}  concurrency={}", args.concurrency);

    // ── Dispatch batches ──────────────────────────────────────────────────────
    let client = Arc::new(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()?
    );
    let sem = Arc::new(Semaphore::new(args.concurrency));
    let api_key = Arc::new(api_key);
    let model = Arc::new(args.model.clone());
    let output_dir = Arc::new(args.output.clone());

    let mut handles = Vec::new();

    for (batch_idx, chunk) in items.chunks(batch_size).enumerate() {
        let chunk: Vec<(PathBuf, EventMeta)> = chunk.to_vec();
        let client = client.clone();
        let api_key = api_key.clone();
        let model = model.clone();
        let output_dir = output_dir.clone();
        let sem = sem.clone();
        let max_tokens = args.max_tokens;

        let handle = tokio::spawn(async move {
            let _permit = sem.acquire_owned().await?;
            process_batch(batch_idx, &chunk, &client, &api_key, &model, &output_dir, max_tokens).await
        });
        handles.push(handle);
    }

    let mut total_talks = 0usize;
    let mut total_failed = 0usize;

    for handle in handles {
        match handle.await? {
            Ok(n) => total_talks += n,
            Err(e) => {
                error!("batch failed: {e:#}");
                total_failed += 1;
            }
        }
    }

    info!("════════════════════════════════════════════");
    info!("done  events={total_events}  talks={total_talks}  failed_batches={total_failed}");
    Ok(())
}

// ── Batch processing ──────────────────────────────────────────────────────────

/// Process one batch of events: call the LLM once, save all resulting talks.
/// Returns the total number of talk files written.
async fn process_batch(
    batch_idx: usize,
    items: &[(PathBuf, EventMeta)],
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    output_dir: &Path,
    max_tokens: u32,
) -> Result<usize> {
    if items.is_empty() {
        return Ok(0);
    }

    let metas: Vec<&EventMeta> = items.iter().map(|(_, m)| m).collect();
    let batch_results = call_llm_batch(client, api_key, model, &metas, max_tokens).await
        .with_context(|| format!("batch {batch_idx} LLM call"))?;

    // Index metas by event_id for fast lookup.
    let meta_map: HashMap<&str, (&PathBuf, &EventMeta)> = items
        .iter()
        .map(|(path, meta)| (meta.id.as_str(), (path, meta)))
        .collect();

    let mut written = 0usize;
    for evt_result in &batch_results {
        let Some(&(source_path, meta)) = meta_map.get(evt_result.event_id.as_str()) else {
            warn!("batch {batch_idx}: unexpected event_id '{}' in response", evt_result.event_id);
            continue;
        };

        let talks_with_speakers: Vec<&ExtractedTalk> = evt_result.talks
            .iter()
            .filter(|t| !t.speakers.is_empty())
            .collect();

        for (order, talk) in talks_with_speakers.iter().enumerate() {
            let record = build_talk_record(talk, order, meta, source_path);
            save_talk(output_dir, &record).await?;
            written += 1;
        }

        if !evt_result.talks.is_empty() {
            info!(
                "  batch {batch_idx} | {} | {} → {} talks",
                meta.group, meta.id,
                talks_with_speakers.len()
            );
        }
    }

    Ok(written)
}

// ── Group discovery ───────────────────────────────────────────────────────────

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

// ── Skip-existing: pre-scan output directory ──────────────────────────────────

/// Build a set of event_ids that have already been extracted.
/// Each output JSON has `"source_file": "source/<group>/<event_id>.html"`.
fn scan_extracted_events(output_dir: &Path) -> HashSet<String> {
    let mut extracted = HashSet::new();
    let Ok(entries) = std::fs::read_dir(output_dir) else { return extracted };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else { continue };
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) else { continue };
        if let Some(sf) = val["source_file"].as_str() {
            // source_file = "source/{group}/{event_id}.html"
            if let Some(stem) = Path::new(sf).file_stem().and_then(|s| s.to_str()) {
                extracted.insert(stem.to_string());
            }
        }
    }
    extracted
}

// ── HTML / Apollo state parsing ───────────────────────────────────────────────

fn extract_event_meta(html: &str, group: &str, event_id: &str) -> Option<EventMeta> {
    let document = Html::parse_document(html);
    let sel = Selector::parse("script#__NEXT_DATA__").ok()?;
    let script = document.select(&sel).next()?;
    let raw: String = script.text().collect();
    let data: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let apollo = data["props"]["pageProps"]["__APOLLO_STATE__"].as_object()?;

    let key = format!("Event:{event_id}");
    let ev = apollo.get(&key)?;

    let title = ev.get("title")?.as_str()?.to_string();
    let description_md = ev
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let datetime = ev
        .get("dateTime")
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    let date = datetime.map(|d| d.date_naive());
    let url = ev
        .get("eventUrl")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut venue_name = None;
    let mut venue_address = None;
    let mut city = None;
    if let Some(vref) = ev
        .get("venue")
        .and_then(|v| v.get("__ref"))
        .and_then(|v| v.as_str())
    {
        if let Some(venue) = apollo.get(vref) {
            venue_name    = venue.get("name").and_then(|v| v.as_str()).map(str::to_string);
            venue_address = venue.get("address").and_then(|v| v.as_str()).map(str::to_string);
            city          = venue.get("city").and_then(|v| v.as_str()).map(str::to_string);
        }
    } else if let Some(venue) = ev.get("venue").and_then(|v| v.as_object()) {
        venue_name    = venue.get("name").and_then(|v| v.as_str()).map(str::to_string);
        venue_address = venue.get("address").and_then(|v| v.as_str()).map(str::to_string);
        city          = venue.get("city").and_then(|v| v.as_str()).map(str::to_string);
    }

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
        group_name: group_display_name(group),
    })
}

fn group_display_name(slug: &str) -> String {
    match slug {
        "bay-area-ai"               => "Bay Area AI".to_string(),
        "sf-scala"                  => "SF Scala".to_string(),
        "unstructured-data-sf"      => "Unstructured Data SF".to_string(),
        "hadoopsf"                  => "San Francisco Hadoop Users".to_string(),
        "graphql-by-the-bay"        => "GraphQL By the Bay".to_string(),
        "scala-bay"                 => "Scala Bay".to_string(),
        "sf-data-and-ai-engineering"=> "SF Data and AI Engineering".to_string(),
        "big-data-developers-in-nyc"=> "Data, Cloud and AI in NYC".to_string(),
        other => other.to_string(),
    }
}

// ── LLM batch call ────────────────────────────────────────────────────────────

/// Send up to N event descriptions in a single API call.
/// Returns `(event_id, talks)` pairs for every event the model returned data for.
async fn call_llm_batch(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    events: &[&EventMeta],
    max_tokens: u32,
) -> Result<Vec<EventExtractionResult>> {
    let n = events.len();

    let tool = build_tool_schema(n);
    let prompt = build_batch_prompt(events);

    let body = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "tools": [tool],
        "tool_choice": {"type": "tool", "name": "save_events"},
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
        let result: BatchResult = serde_json::from_value(tool_use["input"].clone())
            .with_context(|| format!("decode tool input: {tool_use}"))?;
        return Ok(result.events);
    }
}

// ── Prompt and tool schema builders ──────────────────────────────────────────

fn build_batch_prompt(events: &[&EventMeta]) -> String {
    let n = events.len();
    let mut prompt = format!(
        "Extract structured talk information from each of the following {n} meetup event descriptions.\n\
         \n\
         For each event, identify every talk and return:\n\
         - title: the specific talk title (not the event title)\n\
         - abstract: the verbatim abstract or description paragraph for that talk (null if absent)\n\
         - speakers: each named human presenter with name, role (job title if mentioned), \
           and company (affiliation if mentioned)\n\
         - projects: named open-source libraries, frameworks, or tools that are explicitly \
           discussed or demoed (only things with a plausible GitHub repo)\n\
         \n\
         Rules:\n\
         - A real talk requires a specific title AND at least one named human speaker.\n\
         - Do NOT invent any data. Use only facts stated in the description.\n\
         - Skip non-talk items: networking segments, food/drinks, registration, Q&A, \
           sponsor pitches, announcements, or generic meetup boilerplate.\n\
         - When multiple panelists share one talk, put them all in that talk's speakers \
           array — do not split one talk into one entry per speaker.\n\
         - Speaker.name must be a person's full name. Strip role prefixes \
           ('CEO at', 'VP of', 'Engineer @') from the name field; those go in role/company.\n\
         - Projects must be specific named things ('Apache Kafka', 'LangChain', 'PyTorch'), \
           not generic concepts ('machine learning', 'REST API', 'databases').\n\
         - Return an entry for every event_id in the input, even if talks is empty.\n\
         \n"
    );

    for (i, meta) in events.iter().enumerate() {
        prompt.push_str(&format!(
            "══ EVENT {}/{n}  event_id:{}  group:{}  date:{}\n\
             Title: {}\n\
             URL: {}\n\
             \n\
             {}\n\
             \n",
            i + 1,
            meta.id,
            meta.group_name,
            meta.date.map(|d| d.to_string()).unwrap_or_else(|| "unknown".to_string()),
            meta.title,
            meta.url,
            meta.description_md.trim(),
        ));
    }

    prompt
}

fn build_tool_schema(n: usize) -> serde_json::Value {
    serde_json::json!({
        "name": "save_events",
        "description": format!(
            "Save extracted talk data for all {n} events in this batch. \
             Include one entry per event_id, using an empty talks array for events with no identifiable talks."
        ),
        "input_schema": {
            "type": "object",
            "properties": {
                "events": {
                    "type": "array",
                    "description": format!(
                        "Exactly one entry per input event. Must contain all {n} event_ids."
                    ),
                    "items": {
                        "type": "object",
                        "properties": {
                            "event_id": {
                                "type": "string",
                                "description": "The event_id exactly as given in the input (do not modify)."
                            },
                            "talks": {
                                "type": "array",
                                "description": "All talks at this event. Empty array if there are no identifiable talks.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "title": {
                                            "type": "string",
                                            "description": "The specific talk title."
                                        },
                                        "abstract": {
                                            "type": ["string", "null"],
                                            "description": "Verbatim abstract paragraph from the description, or null."
                                        },
                                        "speakers": {
                                            "type": "array",
                                            "items": {
                                                "type": "object",
                                                "properties": {
                                                    "name": {
                                                        "type": "string",
                                                        "description": "Person's full name only (no titles or company)."
                                                    },
                                                    "role": {
                                                        "type": ["string", "null"],
                                                        "description": "Job title or role if mentioned (e.g. 'Staff Engineer'), else null."
                                                    },
                                                    "company": {
                                                        "type": ["string", "null"],
                                                        "description": "Employer or affiliation if mentioned, else null."
                                                    }
                                                },
                                                "required": ["name"]
                                            }
                                        },
                                        "projects": {
                                            "type": "array",
                                            "items": {
                                                "type": "object",
                                                "properties": {
                                                    "name": {
                                                        "type": "string",
                                                        "description": "Project name as mentioned (e.g. 'Apache Kafka')."
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
                        "required": ["event_id", "talks"]
                    }
                }
            },
            "required": ["events"]
        }
    })
}

// ── Record builders ───────────────────────────────────────────────────────────

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
    let _ = source_path;

    let talk_nid  = format!("talk:{talk_id}");
    let event_nid = format!("event:{}-{}", event.group, event.id);
    let group_nid = format!("group:{}", event.group);

    let mut nodes = vec![
        Node {
            id: talk_nid.clone(),
            kind: "Talk".to_string(),
            properties: serde_json::json!({
                "id":       talk_id,
                "title":    talk.title,
                "abstract": talk.abstract_text,
                "order":    order,
            }),
        },
        Node {
            id: event_nid.clone(),
            kind: "Event".to_string(),
            properties: serde_json::json!({
                "id":            event.id,
                "title":         event.title,
                "date":          event.date.map(|d| d.to_string()),
                "datetime":      event.datetime.map(|d| d.to_rfc3339()),
                "url":           event.url,
                "venue_name":    event.venue_name,
                "venue_address": event.venue_address,
                "city":          event.city,
            }),
        },
        Node {
            id: group_nid.clone(),
            kind: "Group".to_string(),
            properties: serde_json::json!({
                "slug": event.group,
                "name": event.group_name,
                "url":  format!("https://www.meetup.com/{}", event.group),
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
                "name":    speaker.name,
                "bio":     speaker.bio,
                "company": speaker.company,
                "role":    speaker.role,
            }),
        });
        edges.push(Edge {
            from: talk_nid.clone(),
            to: sp_nid.clone(),
            kind: "PRESENTED_BY".to_string(),
            properties: None,
        });

        if let Some(company) = &speaker.company {
            if !company.trim().is_empty() {
                let co_nid = format!("company:{}", slugify(company));
                nodes.push(Node {
                    id: co_nid.clone(),
                    kind: "Company".to_string(),
                    properties: serde_json::json!({ "name": company }),
                });
                edges.push(Edge {
                    from: sp_nid.clone(),
                    to: co_nid,
                    kind: "WORKS_AT".to_string(),
                    properties: None,
                });
            }
        }
    }

    for project in &talk.projects {
        if project.name.trim().is_empty() {
            continue;
        }
        let proj_nid = format!("project:{}", slugify(&project.name));
        nodes.push(Node {
            id: proj_nid.clone(),
            kind: "Project".to_string(),
            properties: serde_json::json!({
                "name":       project.name,
                "github_url": serde_json::Value::Null,
            }),
        });
        edges.push(Edge {
            from: talk_nid.clone(),
            to: proj_nid,
            kind: "MENTIONS".to_string(),
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

// ── ID helpers ────────────────────────────────────────────────────────────────

fn slugify(text: &str) -> String {
    let slug = text
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
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
