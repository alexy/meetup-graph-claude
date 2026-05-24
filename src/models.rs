use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

/// Top-level record: one JSON file per extracted talk, graph-DB ready.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TalkRecord {
    pub schema_version: String,
    /// Unique ID: bythebay-YYYYMMDD-talk-title-slug-speaker-slug
    pub id: String,
    pub scraped_at: DateTime<Utc>,
    /// Path relative to the data root of the saved event HTML
    pub source_file: String,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub properties: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<serde_json::Value>,
}

// ── internal working structs (not serialized directly) ────────────────────────

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct EventData {
    pub id: String,
    pub title: String,
    pub date: Option<NaiveDate>,
    pub datetime: Option<DateTime<Utc>>,
    pub url: String,
    pub description_html: String, // kept for re-parsing or debugging
    pub description_text: String,
    pub venue_name: Option<String>,
    pub venue_address: Option<String>,
    pub city: Option<String>,
    pub group_slug: String,
    pub group_name: String,
    pub talks: Vec<TalkData>,
}

#[derive(Debug, Clone, Default)]
pub struct TalkData {
    pub title: String,
    pub abstract_text: Option<String>,
    pub speakers: Vec<SpeakerData>,
    /// Zero-based position within the event
    pub order: usize,
}

#[derive(Debug, Clone, Default)]
pub struct SpeakerData {
    pub name: String,
    pub bio: Option<String>,
    pub company: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct EventLink {
    pub url: String,
    pub title: Option<String>, // hint only; authoritative title comes from event page
}
