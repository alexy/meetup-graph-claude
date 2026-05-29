use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{NaiveDate, Utc};
use tokio::fs;
use tracing::info;

use crate::config::SCHEMA_VERSION;
use crate::models::{Edge, EventData, Node, SpeakerData, TalkData, TalkRecord};

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
        Node {
            id: talk_nid.clone(),
            kind: "Talk".to_string(),
            properties: serde_json::json!({
                "id":       talk_id,
                "title":    talk.title,
                "abstract": talk.abstract_text,
                "order":    talk.order,
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
                "slug": event.group_slug,
                "name": event.group_name,
                "url":  format!("https://www.meetup.com/{}", event.group_slug),
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
        nodes.push(speaker_node(&sp_nid, speaker));
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

fn speaker_node(id: &str, s: &SpeakerData) -> Node {
    Node {
        id: id.to_string(),
        kind: "Speaker".to_string(),
        properties: serde_json::json!({
            "name":    s.name,
            "bio":     s.bio,
            "company": s.company,
            "role":    s.role,
        }),
    }
}
