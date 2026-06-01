use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{NaiveDate, Utc};
use grust::{Edge, Node, Props, Value};
use tokio::fs;
use tracing::info;

use crate::config::SCHEMA_VERSION;
use crate::models::{EventData, SpeakerData, TalkData, TalkRecord};

// ── ID / slug helpers ─────────────────────────────────────────────────────────

pub fn slugify(text: &str) -> String {
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
    // cap each component so IDs stay manageable
    if slug.len() > 60 {
        slug[..60].trim_end_matches('-').to_string()
    } else {
        slug
    }
}

pub fn make_talk_id(date: &NaiveDate, title: &str, speaker: Option<&str>) -> String {
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

// ── file I/O ──────────────────────────────────────────────────────────────────

/// Save raw event HTML.  Returns path relative to `output_dir` for referencing.
pub async fn save_source(
    group_slug: &str,
    event_id: &str,
    html: &str,
    output_dir: &Path,
) -> Result<PathBuf> {
    let dir = output_dir.join("source").join(group_slug);
    fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{event_id}.html"));
    fs::write(&path, html).await?;
    info!("saved source → {}", path.display());
    Ok(path)
}

pub async fn save_talk(record: &TalkRecord, output_dir: &Path) -> Result<PathBuf> {
    let dir = output_dir.join("talks");
    fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{}.json", record.id));
    let json = serde_json::to_string_pretty(record)?;
    fs::write(&path, json).await?;
    info!("saved talk  → {}", path.display());
    Ok(path)
}

// ── graph record builder ──────────────────────────────────────────────────────

pub fn build_talk_record(
    talk: &TalkData,
    event: &EventData,
    source_path: &Path,
    output_dir: &Path,
) -> TalkRecord {
    let date = event.date.unwrap_or_else(|| Utc::now().date_naive());
    let first_speaker = talk.speakers.first().map(|s| s.name.as_str());
    let talk_id = make_talk_id(&date, &talk.title, first_speaker);

    let relative_source = source_path
        .strip_prefix(output_dir)
        .unwrap_or(source_path)
        .to_string_lossy()
        .to_string();

    // Node IDs
    let talk_nid = format!("talk:{talk_id}");
    let event_nid = format!("event:{}-{}", event.group_slug, event.id);
    let group_nid = format!("group:{}", event.group_slug);

    let mut nodes = vec![
        Node::new("Talk", talk_nid.as_str(), make_props([
            ("id",       Value::String(talk_id.clone())),
            ("title",    Value::String(talk.title.clone())),
            ("abstract", talk.abstract_text.as_deref().map(Value::from).unwrap_or(Value::Null)),
            ("order",    Value::Int(talk.order as i64)),
        ])),
        Node::new("Event", event_nid.as_str(), make_props([
            ("id",            Value::String(event.id.clone())),
            ("title",         Value::String(event.title.clone())),
            ("date",          event.date.map(|d| Value::String(d.to_string())).unwrap_or(Value::Null)),
            ("datetime",      event.datetime.map(|d| Value::String(d.to_rfc3339())).unwrap_or(Value::Null)),
            ("url",           Value::String(event.url.clone())),
            ("venue_name",    event.venue_name.as_deref().map(Value::from).unwrap_or(Value::Null)),
            ("venue_address", event.venue_address.as_deref().map(Value::from).unwrap_or(Value::Null)),
            ("city",          event.city.as_deref().map(Value::from).unwrap_or(Value::Null)),
        ])),
        Node::new("Group", group_nid.as_str(), make_props([
            ("slug", Value::String(event.group_slug.clone())),
            ("name", Value::String(event.group_name.clone())),
            ("url",  Value::String(format!("https://www.meetup.com/{}", event.group_slug))),
        ])),
    ];

    let mut edges = vec![
        Edge::new("PRESENTED_AT", talk_nid.as_str(), event_nid.as_str(), Props::new()),
        Edge::new("PART_OF", event_nid.as_str(), group_nid.as_str(), Props::new()),
    ];

    for speaker in &talk.speakers {
        let sp_nid = format!("speaker:{}", slugify(&speaker.name));
        nodes.push(speaker_node(&sp_nid, speaker));
        edges.push(Edge::new("PRESENTED_BY", talk_nid.as_str(), sp_nid.as_str(), Props::new()));
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

fn speaker_node(id: &str, s: &SpeakerData) -> Node {
    Node::new("Speaker", id, make_props([
        ("name",    Value::String(s.name.clone())),
        ("bio",     s.bio.as_deref().map(Value::from).unwrap_or(Value::Null)),
        ("company", s.company.as_deref().map(Value::from).unwrap_or(Value::Null)),
        ("role",    s.role.as_deref().map(Value::from).unwrap_or(Value::Null)),
    ]))
}

fn make_props<const N: usize>(entries: [(&str, Value); N]) -> Props {
    entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}
