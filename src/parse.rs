use std::sync::OnceLock;

use anyhow::Result;
use chrono::{DateTime, Utc};
use regex::Regex;
use scraper::{Html, Selector};

use crate::gql::GqlEvent;
use crate::models::{EventData, EventLink, SpeakerData, TalkData};

// ── compiled regexes ──────────────────────────────────────────────────────────

fn re_md_link() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\[([^\]]+)\]\([^)]+\)").unwrap())
}
fn re_md_emphasis() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\*{1,3}([^*]+)\*{1,3}").unwrap())
}
fn re_excess_nl() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\n{3,}").unwrap())
}
fn re_bold_title_line() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // **Title** on its own line (not a bullet)
    R.get_or_init(|| Regex::new(r"^[\s\u{200B}\u{FEFF}]*\*\*([^*\n]{4,}?)\*\*\s*$").unwrap())
}
fn re_numbered_section() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // (1) Title or 1. Title or 1) Title
    R.get_or_init(|| Regex::new(r"^\s*\(?\d+[\)\.]\s+(.+)$").unwrap())
}
fn re_quoted_title() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"^["\u{201C}\u{2018}](.+?)["\u{201D}\u{2019}]\s*$"#).unwrap())
}
fn re_bullet() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // *, -, • bullets
    R.get_or_init(|| Regex::new(r"^\s*[*\-\u{2022}]\s+(.+)$").unwrap())
}
fn re_bullet_bold_title() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^\s*[*\-\u{2022}]\s+\*\*(.+?)\*\*\s*(?:[-–—]\s*\*?(.+?)\*?)?$").unwrap())
}
fn re_bullet_plain_dash() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // * Title - Speaker  (title >= 10 chars)
    R.get_or_init(|| Regex::new(r"^\s*[*\-\u{2022}]\s+([^*\n]{10,}?)\s+[-–—]\s+(.+)$").unwrap())
}
fn re_time_slot() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // 6:35 - 7:00 - rest  or  6:35 - rest
    R.get_or_init(|| Regex::new(r"^\s*\d+:\d+\s*[-–]\s*(?:\d+:\d+\s*[-–]\s*)?(.+)$").unwrap())
}
fn re_presentation_label() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)^\s*\**(?:presentation|talk|topic|title)\**\s*:\**\s*(.+?)\**\s*$").unwrap())
}
fn re_speaker_label() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)^\s*\**(?:speakers?|presenters?|by|presented\s+by)\**\s*:\**\s*(.+?)\**\s*$").unwrap())
}
fn re_md_header_title() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^\s*#{1,3}\s+(.+?)\s*#*\s*$").unwrap())
}
fn re_name_will() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // "FirstName LastName will/is/has/was/presented/gave/works..."
    R.get_or_init(|| {
        Regex::new(r"^([A-Z][a-z]+(?:\s+[A-Z][a-z]+){1,2})\s+(?:will\b|is\b|has\b|was\b|presented\b|gave\b|spoke\b|works\b|joins\b)")
            .unwrap()
    })
}
fn re_name_at_company() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\bat\s+([A-Z][A-Za-z0-9]+(?:\s+[A-Z][A-Za-z0-9]+){0,3})").unwrap())
}
fn re_title_by_speaker() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"(?i)^["\u{201C}](.+?)["\u{201D}]\s+by\s+(.+?)\.?$"#).unwrap())
}

// ── public API ────────────────────────────────────────────────────────────────

pub fn extract_event_links(html: &str) -> Vec<EventLink> {
    let mut links: Vec<EventLink> = Vec::new();
    if let Some(apollo) = extract_apollo_state(html) {
        for (key, val) in &apollo {
            if key.starts_with("Event:") {
                if let Some(url) = val["eventUrl"].as_str() {
                    let title = val["title"].as_str().map(str::to_string);
                    let url = url.trim_end_matches('/').to_string();
                    if !links.iter().any(|l| l.url == url) {
                        links.push(EventLink { url, title });
                    }
                }
            }
        }
    }
    links
}

pub fn parse_event_page(html: &str, url: &str, group_slug: &str) -> Result<EventData> {
    if let Some(ev) = parse_from_apollo(html, url, group_slug) {
        return Ok(ev);
    }
    if let Some(ev) = parse_from_json_ld(html, url, group_slug)? {
        return Ok(ev);
    }
    parse_from_html(html, url, group_slug)
}

pub fn parse_from_gql(ev: &GqlEvent, group_slug: &str) -> EventData {
    let desc_raw = ev.description.as_deref().unwrap_or("");
    let desc_text = markdown_to_text(desc_raw);
    let mut talks = extract_talks(desc_raw, &desc_text);

    // If the description yielded nothing, try to extract from the event title itself.
    // Many single-talk events encode "Speaker: Title" or "Speaker, Title" in the title.
    if talks.is_empty() {
        talks = talks_from_event_title(&ev.title, &desc_text);
    }

    let datetime: Option<DateTime<Utc>> = ev.date_time.as_deref().and_then(|s| s.parse().ok());
    let date = datetime.map(|dt| dt.date_naive());

    let (venue_name, venue_address, city) = ev
        .venue
        .as_ref()
        .map(|v| (v.name.clone(), v.address.clone(), v.city.clone()))
        .unwrap_or((None, None, None));

    EventData {
        id: ev.id.clone(),
        title: ev.title.clone(),
        date,
        datetime,
        url: ev.event_url.clone(),
        description_html: desc_raw.to_string(),
        description_text: desc_text,
        venue_name,
        venue_address,
        city,
        group_slug: group_slug.to_string(),
        group_name: group_slug.to_string(),
        talks,
    }
}

// ── Apollo / JSON-LD / HTML parsers ───────────────────────────────────────────

fn parse_from_apollo(html: &str, url: &str, group_slug: &str) -> Option<EventData> {
    let apollo = extract_apollo_state(html)?;
    let event_key = apollo.keys().find(|k| k.starts_with("Event:"))?;
    let ev = &apollo[event_key];

    let event_id = ev["id"].as_str().unwrap_or("unknown").to_string();
    let title = ev["title"].as_str().unwrap_or("Untitled Event").to_string();
    let description = ev["description"].as_str().unwrap_or("").to_string();
    let datetime: Option<DateTime<Utc>> = ev["dateTime"].as_str().and_then(|s| s.parse().ok());
    let date = datetime.map(|dt| dt.date_naive());

    let (venue_name, venue_address, city) = ev["venue"]
        .get("__ref")
        .and_then(|r| r.as_str())
        .and_then(|ref_key| apollo.get(ref_key))
        .map(|v| {
            (
                v["name"].as_str().map(str::to_string),
                v["address"].as_str().map(str::to_string),
                v["city"].as_str().map(str::to_string),
            )
        })
        .unwrap_or((None, None, None));

    let (group_name, _group_url) = ev["group"]
        .get("__ref")
        .and_then(|r| r.as_str())
        .and_then(|ref_key| apollo.get(ref_key))
        .map(|g| {
            (
                g["name"].as_str().unwrap_or(group_slug).to_string(),
                g["link"].as_str().unwrap_or("").to_string(),
            )
        })
        .unwrap_or_else(|| (group_slug.to_string(), String::new()));

    let desc_text = markdown_to_text(&description);
    let mut talks = extract_talks(&description, &desc_text);
    if talks.is_empty() {
        talks = talks_from_event_title(&title, &desc_text);
    }

    Some(EventData {
        id: event_id,
        title,
        date,
        datetime,
        url: url.to_string(),
        description_html: description.clone(),
        description_text: desc_text,
        venue_name,
        venue_address,
        city,
        group_slug: group_slug.to_string(),
        group_name,
        talks,
    })
}

fn parse_from_json_ld(html: &str, url: &str, group_slug: &str) -> Result<Option<EventData>> {
    let document = Html::parse_document(html);
    let sel = Selector::parse("script[type='application/ld+json']")
        .map_err(|e| anyhow::anyhow!("selector: {e:?}"))?;

    for script in document.select(&sel) {
        let raw: String = script.text().collect();
        if let Ok(data) = serde_json::from_str::<serde_json::Value>(&raw) {
            if data["@type"].as_str() != Some("Event") {
                continue;
            }
            let title = data["name"].as_str().unwrap_or("Untitled").to_string();
            let desc_raw = data["description"].as_str().unwrap_or("").to_string();
            let desc_text = markdown_to_text(&desc_raw);
            let event_id = extract_event_id(url).unwrap_or_else(|| "unknown".to_string());
            let datetime: Option<DateTime<Utc>> =
                data["startDate"].as_str().and_then(|s| s.parse().ok());
            let date = datetime.map(|dt| dt.date_naive());
            let loc = &data["location"];
            let addr = &loc["address"];
            let group_name = data["organizer"]["name"]
                .as_str()
                .unwrap_or(group_slug)
                .to_string();
            let talks = extract_talks(&desc_raw, &desc_text);
            return Ok(Some(EventData {
                id: event_id,
                title,
                date,
                datetime,
                url: url.to_string(),
                description_html: desc_raw,
                description_text: desc_text,
                venue_name: loc["name"].as_str().map(str::to_string),
                venue_address: addr["streetAddress"].as_str().map(str::to_string),
                city: addr["addressLocality"].as_str().map(str::to_string),
                group_slug: group_slug.to_string(),
                group_name,
                talks,
            }));
        }
    }
    Ok(None)
}

fn parse_from_html(html: &str, url: &str, group_slug: &str) -> Result<EventData> {
    let document = Html::parse_document(html);
    let title = Selector::parse("h1").ok().and_then(|sel| {
        document
            .select(&sel)
            .next()
            .map(|el| el.text().collect::<String>().trim().to_string())
    })
    .unwrap_or_else(|| "Untitled Event".to_string());

    let desc_raw = ["[data-testid='event-description']", ".event-description"]
        .iter()
        .find_map(|s| {
            Selector::parse(s)
                .ok()
                .and_then(|sel| document.select(&sel).next().map(|el| el.inner_html()))
        })
        .unwrap_or_default();

    let desc_text = markdown_to_text(&desc_raw);
    let event_id = extract_event_id(url).unwrap_or_else(|| "unknown".to_string());
    let talks = extract_talks(&desc_raw, &desc_text);

    Ok(EventData {
        id: event_id,
        title,
        url: url.to_string(),
        description_html: desc_raw,
        description_text: desc_text,
        group_slug: group_slug.to_string(),
        group_name: group_slug.to_string(),
        talks,
        ..Default::default()
    })
}

// ── talk extraction orchestrator ──────────────────────────────────────────────

/// Extract talks from a markdown description.
/// Every returned TalkData is guaranteed to have at least one speaker.
pub fn extract_talks(raw: &str, _plain: &str) -> Vec<TalkData> {
    // Strip zero-width chars and markdown backslash escapes (e.g. `\#`, `\-`)
    let md: String = raw
        .chars()
        .filter(|&c| c != '\u{200B}' && c != '\u{FEFF}')
        .collect();
    let md = md.replace("\\#", "#")
        .replace("\\-", "-")
        .replace("\\*", "*")
        .replace("\\.", ".");

    macro_rules! try_strategy {
        ($fn:expr) => {{
            let talks = $fn(&md);
            if talks.iter().any(|t| !t.speakers.is_empty()) {
                return renumber(talks.into_iter().filter(|t| !t.speakers.is_empty()).collect());
            }
        }};
    }

    // Bay Area AI / bythebay: **Title** then ***Speaker*, Company**
    try_strategy!(talks_bold_title_speaker);

    // SF Scala: (1) Title then prose with "Name will be talking..."
    try_strategy!(talks_numbered_sections);

    // Hadoopsf explicit labels: Presentation: / Speaker:
    try_strategy!(talks_explicit_labels);

    // SF Scala headers: # Title  then  **Speaker:** Name
    try_strategy!(talks_md_header_speaker);

    // Hadoopsf quoted title: "Title"\nName, Company
    try_strategy!(talks_quoted_title);

    // Big Data NYC: • Title. Name, Company  or  • Title - Name, Company
    try_strategy!(talks_bullet_period);

    // Standard bullet agenda: * **Title** - *Speaker*  or  * Title - Speaker
    try_strategy!(talks_bullet_dash);

    // Unstructured Data: HH:MM - [Name](link), Talk Title
    try_strategy!(talks_time_slots);

    vec![]
}

// ── Strategy 8: event title encodes "Speaker: Title" or "Speaker, Title" ─────

/// Called when the description strategies produce nothing.
/// Many SF Scala / GraphQL events have their speaker+title in the event title.
fn talks_from_event_title(event_title: &str, desc_text: &str) -> Vec<TalkData> {
    let mut talks: Vec<TalkData> = Vec::new();

    // Handle multi-talk titles separated by ";"
    // e.g. "Eugene Burmako, 'scala.meta'; Denis Shabalin, 'How unsafe is unsafe?'"
    let segments: Vec<&str> = event_title.split(';').collect();

    for seg in &segments {
        let seg = seg.trim();
        if let Some(talk) = event_title_segment_as_talk(seg, desc_text, segments.len() == 1) {
            talks.push(talk);
        }
    }

    renumber(talks.into_iter().filter(|t| !t.speakers.is_empty()).collect())
}

fn event_title_segment_as_talk(seg: &str, desc_text: &str, _single: bool) -> Option<TalkData> {
    // Pattern: "Name: Title"
    if let Some(colon) = seg.find(": ") {
        let name_cand = seg[..colon].trim();
        let title_cand = seg[colon + 2..].trim().trim_matches('"').trim_matches('\u{201C}').trim_matches('\u{201D}');
        if looks_like_person_name_only(name_cand) && is_real_talk_title(title_cand) {
            let sp = parse_speaker_string(name_cand);
            if !sp.is_empty() {
                let abstract_text = single_talk_abstract(desc_text);
                return Some(TalkData { title: title_cand.to_string(), abstract_text, speakers: sp, order: 0 });
            }
        }
    }

    // Pattern: "Name, Title" or "Name, 'Title'"
    if let Some((name_cand, title_cand)) = split_name_title_by_comma(seg) {
        if is_real_talk_title(&title_cand) {
            let sp = parse_speaker_string(&name_cand);
            if !sp.is_empty() {
                let abstract_text = single_talk_abstract(desc_text);
                return Some(TalkData { title: title_cand, abstract_text, speakers: sp, order: 0 });
            }
        }
    }

    None
}

/// For "Name, Title" patterns in event titles.  Returns None if first part doesn't look like
/// a person name (2–3 proper-noun words).
fn split_name_title_by_comma(seg: &str) -> Option<(String, String)> {
    let re = Regex::new(r#"^([A-Z][a-z]+(?:\s+[A-Z][a-z]+){1,2}),\s+["'\u{201C}]?(.+?)["'\u{201D}]?$"#).unwrap();
    re.captures(seg).and_then(|caps| {
        let name = caps[1].to_string();
        let title = caps[2].trim().to_string();
        if looks_like_person_name_only(&name) {
            Some((name, title))
        } else {
            None
        }
    })
}

/// Extract the abstract from the description text for a single-talk event.
fn single_talk_abstract(desc_text: &str) -> Option<String> {
    let text = desc_text.trim();
    if text.is_empty() || text.len() < 30 { return None; }
    // Trim to first 1000 chars to avoid noise
    let truncated = if text.len() > 1000 { &text[..1000] } else { text };
    Some(truncated.trim().to_string())
}

fn renumber(mut talks: Vec<TalkData>) -> Vec<TalkData> {
    for (i, t) in talks.iter_mut().enumerate() {
        t.title = normalize_title(&t.title);
        t.order = i;
    }
    talks
}

fn normalize_title(raw: &str) -> String {
    let s = raw.trim();
    // Strip "Tech Talk N:" / "Talk N:" / "Topic N:" structural prefixes
    static R: OnceLock<Regex> = OnceLock::new();
    let re = R.get_or_init(|| {
        Regex::new(r"^(?i)(?:tech\s+talk|talk|topic|presentation)\s*\d+\s*[:.\-]\s*").unwrap()
    });
    let s = re.replace(s, "").trim().to_string();
    s
}

// ── Strategy 1: **Title** / ***Speaker*, Company** ───────────────────────────

fn talks_bold_title_speaker(md: &str) -> Vec<TalkData> {
    let mut talks: Vec<TalkData> = Vec::new();
    let lines: Vec<&str> = md.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim();

        if let Some(caps) = re_bold_title_line().captures(line) {
            // Reject bullet lines
            if line.starts_with("* ") || line.starts_with("- ") || line.starts_with("• ") {
                i += 1;
                continue;
            }
            let title = clean_md(&caps[1]);
            if !is_real_talk_title(&title) {
                i += 1;
                continue;
            }

            // Find next non-empty line
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim().is_empty() {
                j += 1;
            }

            let (speakers, body_start) = if j < lines.len() {
                let candidate = lines[j].trim();
                if is_speaker_line(candidate) {
                    (parse_speaker_string(&clean_md(candidate)), j + 1)
                } else {
                    (vec![], j)
                }
            } else {
                (vec![], j)
            };

            if speakers.is_empty() {
                i += 1;
                continue;
            }

            let abstract_text = collect_abstract(&lines, body_start);
            let skip = abstract_lines_count(&lines, body_start);

            talks.push(TalkData { title, abstract_text, speakers, order: 0 });
            i = body_start + skip;
            continue;
        }

        i += 1;
    }

    talks
}

// ── Strategy 2: (N) Title / prose attribution ────────────────────────────────

fn talks_numbered_sections(md: &str) -> Vec<TalkData> {
    let lines: Vec<&str> = md.lines().collect();
    // Each entry: (line_idx, title, Option<inline_speaker>)
    let mut section_starts: Vec<(usize, String, Option<String>)> = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        if let Some(caps) = re_numbered_section().captures(line) {
            let raw = clean_md(caps[1].trim());

            // Detect "Name, Title" format: first part is a person name (2 Title Case words)
            let (title, inline_speaker) = split_name_title_if_applicable(&raw);

            if is_real_talk_title(&title) {
                section_starts.push((i, title, inline_speaker));
            }
        }
    }

    if section_starts.len() < 2 {
        return vec![];
    }

    let mut talks: Vec<TalkData> = Vec::new();
    for (idx, (line_idx, title, inline_speaker)) in section_starts.iter().enumerate() {
        let end = section_starts
            .get(idx + 1)
            .map(|(i, _, _)| *i)
            .unwrap_or(lines.len());

        let prose: Vec<&str> = lines[line_idx + 1..end].to_vec();

        let speakers = if let Some(sp_raw) = inline_speaker {
            // Speaker was embedded in the numbered line itself
            parse_speaker_string(sp_raw)
        } else {
            speaker_from_prose(&prose)
        };

        if speakers.is_empty() {
            continue;
        }

        let abstract_text = prose_to_abstract(&prose);
        talks.push(TalkData { title: title.clone(), abstract_text, speakers, order: 0 });
    }

    talks
}

/// If `raw` looks like "FirstName LastName, Actual Title", split and return (title, Some(speaker)).
/// Otherwise return (raw, None).
fn split_name_title_if_applicable(raw: &str) -> (String, Option<String>) {
    // Check if raw starts with 2–3 Title Case words followed by a comma
    let re = Regex::new(
        r"^([A-Z][a-z]+(?:\s+[A-Z][a-z]+){1,2}),\s+(.+)$"
    ).unwrap();
    if let Some(caps) = re.captures(raw) {
        let name_candidate = caps[1].to_string();
        let title_candidate = caps[2].to_string();
        // Use looks_like_person_name_only as a POSITIVE signal (it's what is_generic_item calls)
        if looks_like_person_name_only(&name_candidate) && is_real_talk_title(&title_candidate) {
            return (title_candidate, Some(name_candidate));
        }
    }
    (raw.to_string(), None)
}

fn speaker_from_prose(lines: &[&str]) -> Vec<SpeakerData> {
    for line in lines {
        let line = line.trim();
        if line.is_empty() { continue; }
        if line.starts_with('#') || line.starts_with('*') || line.starts_with('-') {
            continue;
        }

        // Short line that looks like "Name, Company" — standalone speaker credit
        if line.len() < 80 && !line.contains(". ") {
            if is_speaker_line(line) || looks_like_speaker_field(line) {
                let sp = parse_speaker_string(&clean_md(line));
                if !sp.is_empty() {
                    return sp;
                }
            }
        }

        // "FirstName LastName will/is/was/presented/spoke..."
        if let Some(caps) = re_name_will().captures(line) {
            let name = caps[1].to_string();
            let company = re_name_at_company()
                .captures(line)
                .map(|c| c[1].trim().to_string());
            return vec![SpeakerData { name, company, bio: None, role: None }];
        }
    }
    vec![]
}

fn prose_to_abstract(lines: &[&str]) -> Option<String> {
    let text: Vec<&str> = lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    let joined = text.join("\n");
    let cleaned = clean_md(&joined);
    (!cleaned.trim().is_empty()).then_some(cleaned.trim().to_string())
}

// ── Strategy 3: Presentation: / Speaker: explicit labels ─────────────────────

fn talks_explicit_labels(md: &str) -> Vec<TalkData> {
    let mut talks: Vec<TalkData> = Vec::new();
    let lines: Vec<&str> = md.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim();

        if let Some(caps) = re_presentation_label().captures(line) {
            let title = clean_md(caps[1].trim());
            if !is_real_talk_title(&title) {
                i += 1;
                continue;
            }

            let mut speakers: Vec<SpeakerData> = Vec::new();
            let mut abstract_lines: Vec<&str> = Vec::new();
            let mut j = i + 1;

            while j < lines.len() {
                let next = lines[j].trim();
                if next.is_empty() {
                    j += 1;
                    continue;
                }
                if re_presentation_label().is_match(next) {
                    break; // next talk
                }
                if let Some(sp_caps) = re_speaker_label().captures(next) {
                    let sp = parse_speaker_string(&clean_md(sp_caps[1].trim()));
                    speakers.extend(sp);
                } else {
                    abstract_lines.push(next);
                }
                j += 1;
            }

            if !speakers.is_empty() {
                let abstract_text = if abstract_lines.is_empty() {
                    None
                } else {
                    let t = clean_md(&abstract_lines.join("\n"));
                    (!t.trim().is_empty()).then_some(t.trim().to_string())
                };
                talks.push(TalkData { title, abstract_text, speakers, order: 0 });
            }
            i = j;
            continue;
        }

        i += 1;
    }

    talks
}

/// Strategy: markdown header titles followed by **Speaker:** lines.
/// Format: `# Title` (or `## Title`) then `**Speaker:** Name, role at Co`
/// optionally followed by `**Bio:** ...` and `**Abstract:** ...`.
fn talks_md_header_speaker(md: &str) -> Vec<TalkData> {
    let mut talks: Vec<TalkData> = Vec::new();
    let lines: Vec<&str> = md.lines().collect();
    let mut i = 0;

    let is_section_title = |line: &str| -> Option<String> {
        if let Some(c) = re_md_header_title().captures(line) {
            return Some(clean_md(c[1].trim()));
        }
        if let Some(c) = re_bold_title_line().captures(line) {
            return Some(clean_md(c[1].trim()));
        }
        None
    };

    while i < lines.len() {
        let line = lines[i].trim();

        let Some(title_raw) = is_section_title(line) else {
            i += 1;
            continue;
        };
        // Only fire from this strategy if THIS title is a real talk title and the
        // following non-empty line is a Speaker: label (otherwise leave it for
        // other strategies to handle).
        if !is_real_talk_title(&title_raw) { i += 1; continue; }

        // Peek ahead for speaker label
        let mut peek = i + 1;
        while peek < lines.len() && lines[peek].trim().is_empty() { peek += 1; }
        let has_speaker_label = peek < lines.len()
            && re_speaker_label().is_match(lines[peek].trim());
        if !has_speaker_label { i += 1; continue; }

        let mut speakers: Vec<SpeakerData> = Vec::new();
        let mut abstract_lines: Vec<&str> = Vec::new();
        let mut j = i + 1;

        while j < lines.len() {
            let next = lines[j].trim();
            if next.is_empty() { j += 1; continue; }
            // Stop at next title-like boundary
            if is_section_title(next).is_some() { break; }

            if let Some(sp_caps) = re_speaker_label().captures(next) {
                let sp = parse_speaker_string(&clean_md(sp_caps[1].trim()));
                speakers.extend(sp);
                j += 1;
                continue;
            }
            if !speakers.is_empty() {
                abstract_lines.push(next);
            }
            j += 1;
        }

        if !speakers.is_empty() {
            let abstract_text = if abstract_lines.is_empty() {
                None
            } else {
                let t = clean_md(&abstract_lines.join("\n"));
                (!t.trim().is_empty()).then_some(t.trim().to_string())
            };
            talks.push(TalkData { title: title_raw, abstract_text, speakers, order: 0 });
        }
        i = j;
    }

    talks
}

// ── Strategy 4: "Quoted Title" / Name, Company ───────────────────────────────

fn talks_quoted_title(md: &str) -> Vec<TalkData> {
    let mut talks: Vec<TalkData> = Vec::new();
    let lines: Vec<&str> = md.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim();

        // "Title" by Speaker  (inline variant)
        if let Some(caps) = re_title_by_speaker().captures(line) {
            let title = clean_md(&caps[1]);
            if is_real_talk_title(&title) {
                let sp = parse_speaker_string(&clean_md(&caps[2]));
                if !sp.is_empty() {
                    let abstract_text = collect_abstract(&lines, i + 1);
                    let skip = abstract_lines_count(&lines, i + 1);
                    talks.push(TalkData {
                        title,
                        abstract_text,
                        speakers: sp,
                        order: 0,
                    });
                    i += 1 + skip;
                    continue;
                }
            }
        }

        // "Title" on its own line, speaker on next line
        if let Some(caps) = re_quoted_title().captures(line) {
            let title = clean_md(&caps[1]);
            if is_real_talk_title(&title) {
                let mut j = i + 1;
                while j < lines.len() && lines[j].trim().is_empty() {
                    j += 1;
                }
                if j < lines.len() {
                    let next = clean_md(lines[j].trim());
                    let sp = parse_speaker_string(&next);
                    if !sp.is_empty() {
                        let abstract_text = collect_abstract(&lines, j + 1);
                        let skip = abstract_lines_count(&lines, j + 1);
                        talks.push(TalkData {
                            title,
                            abstract_text,
                            speakers: sp,
                            order: 0,
                        });
                        i = j + 1 + skip;
                        continue;
                    }
                }
            }
        }

        i += 1;
    }

    talks
}

// ── Strategy 5: bullet with period or dash separator ─────────────────────────

fn talks_bullet_period(md: &str) -> Vec<TalkData> {
    let mut talks: Vec<TalkData> = Vec::new();

    for line in md.lines() {
        let line_t = line.trim();
        let Some(caps) = re_bullet().captures(line_t) else { continue };
        let raw_inner = &caps[1];

        // Skip "speaker bullet" form: * **Person Name**, role, company
        // These are panelist entries, not talk titles
        if let Some(bc) = re_md_emphasis().captures(raw_inner) {
            if bc.get(0).unwrap().start() == 0 {
                let bold = bc[1].trim();
                let after = raw_inner[bc.get(0).unwrap().end()..].trim_start();
                if after.starts_with(',') && looks_like_person_name_only(bold) {
                    continue;
                }
            }
        }

        let content = clean_md(raw_inner);

        // Try period split: "Title. Name, Company"
        if let Some(dot_pos) = content.find(". ") {
            let title = content[..dot_pos].trim().to_string();
            let speaker_raw = content[dot_pos + 2..].trim().to_string();

            if is_real_talk_title(&title) && title.split_whitespace().count() >= 4 {
                let sp = parse_speaker_string(&speaker_raw);
                if !sp.is_empty() {
                    talks.push(TalkData {
                        title,
                        abstract_text: None,
                        speakers: sp,
                        order: 0,
                    });
                    continue;
                }
            }
        }

        // Try em/en-dash split: "Title – Name, Company"
        for sep in &[" – ", " — "] {
            if let Some(pos) = content.find(sep) {
                let title = content[..pos].trim().to_string();
                let speaker_raw = content[pos + sep.len()..].trim().to_string();

                if is_real_talk_title(&title) && title.split_whitespace().count() >= 4 {
                    let sp = parse_speaker_string(&speaker_raw);
                    if !sp.is_empty() {
                        talks.push(TalkData {
                            title,
                            abstract_text: None,
                            speakers: sp,
                            order: 0,
                        });
                        break;
                    }
                }
            }
        }
    }

    talks
}

// ── Strategy 6: standard bullet agenda ───────────────────────────────────────

fn talks_bullet_dash(md: &str) -> Vec<TalkData> {
    let mut talks: Vec<TalkData> = Vec::new();
    let lines: Vec<&str> = md.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim();
        if is_generic_item(line) { i += 1; continue; }

        // Pattern A: * **Title** - *Speaker, Company*
        if let Some(caps) = re_bullet_bold_title().captures(line) {
            let title = clean_md(&caps[1]);
            if is_real_talk_title(&title) {
                let speakers = caps
                    .get(2)
                    .map(|m| parse_speaker_string(&clean_md(m.as_str())))
                    .unwrap_or_default();
                if !speakers.is_empty() {
                    let abstract_text = collect_abstract(&lines, i + 1);
                    let skip = abstract_lines_count(&lines, i + 1);
                    talks.push(TalkData { title, abstract_text, speakers, order: 0 });
                    i += 1 + skip;
                    continue;
                }
            }
        }

        // Pattern B: * Title - Speaker, Company  (plain, no bold)
        if let Some(caps) = re_bullet_plain_dash().captures(line) {
            let title = clean_md(&caps[1]);
            if is_real_talk_title(&title) {
                let speaker_raw = clean_md(&caps[2]);
                let speakers = if looks_like_speaker_field(&speaker_raw) {
                    parse_speaker_string(&speaker_raw)
                } else {
                    vec![]
                };
                if !speakers.is_empty() {
                    let abstract_text = collect_abstract(&lines, i + 1);
                    let skip = abstract_lines_count(&lines, i + 1);
                    talks.push(TalkData { title, abstract_text, speakers, order: 0 });
                    i += 1 + skip;
                    continue;
                }
            }
        }

        // Pattern C: * **Title**  (title line), speaker on next non-bullet line
        if (line.starts_with("* **") || line.starts_with("- **") || line.starts_with("• **"))
            && (line.ends_with("**") || line.ends_with("** "))
        {
            let inner = line
                .trim_start_matches("• ")
                .trim_start_matches("* ")
                .trim_start_matches("- ")
                .trim_start_matches("**")
                .trim_end_matches("**")
                .trim();
            let title = clean_md(inner);
            if is_real_talk_title(&title) {
                let mut j = i + 1;
                while j < lines.len() && lines[j].trim().is_empty() { j += 1; }

                if j < lines.len() {
                    let next = lines[j].trim();
                    if !next.starts_with('*') && !next.starts_with('-') && !next.starts_with('#')
                        && looks_like_speaker_field(next)
                    {
                        let speakers = parse_speaker_string(&clean_md(next));
                        if !speakers.is_empty() {
                            let abs_start = j + 1;
                            let abstract_text = collect_abstract(&lines, abs_start);
                            let skip = 1 + abstract_lines_count(&lines, abs_start);
                            talks.push(TalkData { title, abstract_text, speakers, order: 0 });
                            i += 1 + skip;
                            continue;
                        }
                    }
                }
            }
        }

        i += 1;
    }

    talks
}

// ── Strategy 7: time-slot lines ───────────────────────────────────────────────

fn talks_time_slots(md: &str) -> Vec<TalkData> {
    let mut talks: Vec<TalkData> = Vec::new();

    for line in md.lines() {
        let Some(caps) = re_time_slot().captures(line.trim()) else { continue };
        let rest = clean_md(caps[1].trim());

        // Format A: "Title - Name, Role at Company"
        // Split on last " - " to separate title from speaker info
        if let Some(talk) = time_slot_title_dash_speaker(&rest) {
            talks.push(talk);
            continue;
        }

        // Format B: "Name, Talk Title (which may itself contain commas)"
        let Some((name_raw, title_rest)) = rest.split_once(',') else { continue };
        let name_raw = name_raw.trim();
        let title_rest = title_rest.trim();
        if name_raw.is_empty() || !name_raw.chars().next().map_or(false, |c| c.is_uppercase()) {
            continue;
        }

        if looks_like_talk_title_words(title_rest) {
            let title = title_rest.to_string();
            if is_real_talk_title(&title) {
                let speakers = parse_speaker_string(name_raw);
                if !speakers.is_empty() {
                    talks.push(TalkData {
                        title,
                        abstract_text: None,
                        speakers,
                        order: 0,
                    });
                }
            }
        }
    }

    talks
}

/// Parse "Title - Name, Role at Company" from a time-slot remainder.
fn time_slot_title_dash_speaker(rest: &str) -> Option<TalkData> {
    // Find the last " - " separator
    let dash = " - ";
    let pos = rest.rfind(dash)?;
    let title = rest[..pos].trim().to_string();
    let speaker_raw = rest[pos + dash.len()..].trim().to_string();

    if !is_real_talk_title(&title) { return None; }
    if title.split_whitespace().count() < 3 { return None; }

    // speaker_raw must look like "Name, Role..." or "Name at Company"
    let sp = parse_speaker_string(&speaker_raw);
    if sp.is_empty() { return None; }

    Some(TalkData {
        title,
        abstract_text: None,
        speakers: sp,
        order: 0,
    })
}

fn looks_like_talk_title_words(s: &str) -> bool {
    let words: Vec<&str> = s.split_whitespace().collect();
    if words.len() < 4 { return false; }
    let first = words[0].to_lowercase();
    let role_words = [
        "vp", "evp", "svp", "director", "ceo", "cto", "coo", "cfo",
        "head", "senior", "principal", "staff", "founder", "co-founder",
        "engineer", "manager", "architect", "lead", "president",
        "partner", "chair", "chief", "developer", "advocate", "consultant",
        "specialist", "researcher", "scientist", "professor", "oss",
    ];
    if role_words.iter().any(|r| first == *r) { return false; }
    // "X at the Y Foundation" / "... at <Company>" is an affiliation, not a talk title
    let lower = s.to_lowercase();
    if lower.contains(" at the ") { return false; }
    true
}

// ── speaker helpers ───────────────────────────────────────────────────────────

fn parse_speaker_string(raw: &str) -> Vec<SpeakerData> {
    let raw = raw
        .trim()
        .trim_start_matches("Speaker:")
        .trim_start_matches("Presenter:")
        .trim_start_matches("Presenters:")
        .trim_start_matches("By:")
        .trim();

    // Don't split on " and " or " & " if a comma precedes the conjunction —
    // that's typically "Name, Role with and X" rather than "Name1 and Name2".
    let comma_pos = raw.find(',');
    let split_and = match (comma_pos, raw.find(" and ")) {
        (Some(c), Some(a)) => a < c,
        (None, _) => true,
        _ => true,
    };
    let split_amp = match (comma_pos, raw.find(" & ")) {
        (Some(c), Some(a)) => a < c,
        (None, _) => true,
        _ => true,
    };

    let stage1: Vec<&str> = if split_and { raw.split(" and ").collect() } else { vec![raw] };
    let stage2: Vec<&str> = stage1.into_iter().flat_map(|s| s.split(';')).collect();
    let stage3: Vec<&str> = if split_amp {
        stage2.into_iter().flat_map(|s| s.split(" & ")).collect()
    } else {
        stage2
    };

    stage3.into_iter()
        .filter_map(|p| {
            let p = p.trim();
            if p.is_empty() || p.len() < 5 { return None; }
            if looks_like_job_title_only(p) { return None; }
            let (name, company) = split_name_company(p);
            if name.is_empty() { return None; }
            // Name must be 2–4 words, ALL starting with uppercase (proper nouns)
            let words: Vec<&str> = name.split_whitespace().collect();
            if words.len() < 2 || words.len() > 4 { return None; }
            if !words.iter().all(|w| w.chars().next().map_or(false, |c| c.is_uppercase())) {
                return None;
            }
            if name.contains("http") || name.contains("://") || name.contains(':') {
                return None;
            }
            if !passes_name_denylist(&name) { return None; }
            Some(SpeakerData { name, company, bio: None, role: None })
        })
        .collect()
}

fn split_name_company(raw: &str) -> (String, Option<String>) {
    let raw = raw.trim().trim_matches('*').trim_end_matches('.').trim();

    // "Name @ Company"
    if let Some(at) = raw.find('@') {
        let name = raw[..at].trim().to_string();
        let rest = raw[at + 1..].trim();
        let company = extract_company_from_role_at(rest);
        return (name, company);
    }
    // "Name (Company)"  or  "Name (Role, Dept, Company)"
    if let Some(open) = raw.find('(') {
        let name = raw[..open].trim().to_string();
        let inside = raw[open + 1..].trim_end_matches(')').trim();
        let company = extract_company_from_role_at(inside);
        return (name, company);
    }
    // Comma-split variants
    let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
    match parts.len() {
        0 | 1 => {
            // "Name at Company"
            if let Some(idx) = find_word_boundary(raw, " at ") {
                let name = raw[..idx].trim().to_string();
                let company = raw[idx + 4..].trim().to_string();
                return (name, (!company.is_empty()).then_some(company));
            }
            (raw.to_string(), None)
        }
        2 => {
            // Detect swap: "Company, Person Name" form (e.g. **Topaz Labs, Alexander Zhang**)
            if !looks_like_person_name_only(parts[0])
                && looks_like_person_name_only(parts[1])
            {
                return (parts[1].to_string(), Some(parts[0].to_string()));
            }
            let name = parts[0].to_string();
            // "Name, Role at Company" → extract just the company
            let company = extract_company_from_role_at(parts[1]);
            (name, company)
        }
        _ => {
            // "Name, Role, Company" → name=first, company=last part
            let name = parts[0].to_string();
            let last = parts.last().unwrap().trim_matches('*').trim();
            let company = extract_company_from_role_at(last);
            (name, company)
        }
    }
}

/// From a string like "VP Engineering at Zilliz" or just "Cloudera", extract the company.
/// Returns just the company name, stripping any leading role/title.
fn extract_company_from_role_at(s: &str) -> Option<String> {
    let s = s.trim().trim_end_matches('*').trim();
    if s.is_empty() { return None; }
    // If contains " at ", take the part after "at"
    if let Some(idx) = find_word_boundary(s, " at ") {
        let company = s[idx + 4..].trim().to_string();
        if !company.is_empty() {
            return clean_company(&company);
        }
    }
    // Check for multi-word with commas: "Role, Dept, Company" → last part
    if s.contains(',') {
        let last = s.split(',').last().unwrap().trim().trim_matches('*').trim().to_string();
        if !last.is_empty() { return clean_company(&last); }
    }
    clean_company(s)
}

/// Cleans/validates a candidate company string. Returns None for role-only
/// strings ("Founder", "CEO", "PhD"), strips trailing role keywords
/// ("Voyage AI CEO" → "Voyage AI", "Zilliz Cloud Software Engineer" → "Zilliz Cloud").
fn clean_company(raw: &str) -> Option<String> {
    let s = raw.trim().trim_end_matches(',').trim_matches('*').trim();
    if s.is_empty() { return None; }

    // Reject pure role/title-only strings
    let role_only = [
        "founder", "co-founder", "cofounder", "ceo", "cto", "coo", "cfo",
        "cio", "cmo", "vp", "evp", "svp", "phd", "ph.d.", "ph.d", "md",
        "professor", "asst. professor", "assistant professor",
        "associate professor", "engineer", "software engineer",
        "developer", "architect", "manager", "director",
    ];
    let s_lower = s.to_lowercase();
    if role_only.iter().any(|r| s_lower == *r) { return None; }

    // Strip trailing role suffixes: "Voyage AI CEO" → "Voyage AI"
    let trailing_role_words: &[&str] = &[
        "CEO", "CTO", "COO", "CFO", "CIO", "CMO", "VP", "EVP", "SVP",
        "Founder", "Co-Founder", "Cofounder", "President", "Director",
        "Manager", "Engineer", "Developer", "Architect", "Lead",
        "Scientist", "Researcher", "Advocate", "Evangelist", "Consultant",
        "Officer", "Specialist", "PhD", "Ph.D", "MD",
    ];
    let mut words: Vec<&str> = s.split_whitespace().collect();
    while let Some(last) = words.last() {
        let l = last.trim_matches(',').trim_matches('.');
        let is_role = trailing_role_words.iter().any(|r| r.eq_ignore_ascii_case(l));
        // Stop stripping if we'd leave fewer than 1 token
        if is_role && words.len() > 1 {
            words.pop();
        } else {
            break;
        }
    }
    let cleaned = words.join(" ");
    if cleaned.is_empty() { return None; }

    // After stripping, also strip "Software"/"Senior"/"Principal" trailing prefixes
    // when they precede a stripped role: e.g. "Zilliz Cloud Software" → "Zilliz Cloud"
    let modifier_only_trailing = ["Software", "Senior", "Principal", "Staff", "Junior", "Lead"];
    let mut words: Vec<&str> = cleaned.split_whitespace().collect();
    while let Some(last) = words.last() {
        let l = last.trim_matches(',').trim_matches('.');
        let is_modifier = modifier_only_trailing.iter().any(|m| m.eq_ignore_ascii_case(l));
        if is_modifier && words.len() > 1 {
            words.pop();
        } else {
            break;
        }
    }
    let cleaned = words.join(" ");
    if cleaned.is_empty() { return None; }

    // Reject role descriptions disguised as companies. These typically contain
    // mid-string prepositions or > 5 words.
    let word_count = cleaned.split_whitespace().count();
    if word_count > 5 { return None; }
    let cl = cleaned.to_lowercase();
    if cl.contains(" for ") || cl.contains(" of ") || cl.contains(" with ") {
        return None;
    }

    Some(cleaned)
}

fn find_word_boundary(s: &str, pattern: &str) -> Option<usize> {
    s.to_lowercase().find(pattern)
}

fn first_char_upper(s: &str) -> bool {
    s.chars().next().map_or(false, |c| c.is_uppercase())
}

fn looks_like_job_title_only(s: &str) -> bool {
    let l = s.to_lowercase();
    let exact = [
        "ceo", "cto", "coo", "cfo", "vp", "director", "manager", "engineer",
        "architect", "researcher", "developer", "scientist", "consultant",
        "founder", "co-founder", "partner", "senior engineer", "software engineer",
        "ml engineer", "principal engineer", "staff engineer",
    ];
    exact.iter().any(|t| l.trim() == *t)
        || (s.split_whitespace().count() <= 3
            && (s.contains("Engineer") || s.contains("Director") || s.contains("Manager"))
            && !s.contains(','))
}

/// True if this line looks like a standalone speaker credit (not a title).
fn is_speaker_line(s: &str) -> bool {
    let stripped = s.trim();
    // ***Name*, Company** or **Name, Company**
    if (stripped.starts_with("***") || stripped.starts_with("**")) && stripped.contains(',') {
        return stripped.len() < 120;
    }
    // *Name, Company*
    if stripped.starts_with('*') && stripped.ends_with('*') && stripped.contains(',') {
        return stripped.len() < 120;
    }
    looks_like_speaker_field(stripped)
}

fn looks_like_speaker_field(s: &str) -> bool {
    let s = s.trim();
    (s.contains(',') || s.contains(" at ") || s.contains('@'))
        && !s.contains("http")
        && s.len() < 120
        && s.split_whitespace().count() < 12
        && !s.ends_with(':')
        && first_char_upper(s)
}

// ── title / generic filters ───────────────────────────────────────────────────

fn is_real_talk_title(title: &str) -> bool {
    let len = title.len();
    if len < 8 || len > 200 { return false; }
    if title.contains("http") || title.contains("://") { return false; }
    if title.ends_with(':') { return false; }
    if title.starts_with('!') { return false; }
    if title.contains("MUST BE REGISTERED") || title.contains("RSVP") { return false; }
    if title.starts_with("Please ") || title.starts_with("Note:") || title.starts_with("NOTE:") {
        return false;
    }
    // Lowercase first letters are common in product names ("ducktape", "smithy4s"),
    // so don't blanket-reject. Other prose checks below catch sentence-form text.
    // Starts with a time pattern
    if Regex::new(r"^[\d:]+\s*(?:[aApP][mM])?[:\s–\-]")
        .unwrap()
        .is_match(title)
    {
        return false;
    }
    // "Name will/is/has/shows... X" — this is a prose sentence, not a title
    if Regex::new(r"^[A-Z][a-z]+ (?:[A-Z][a-z]+ )?(?:will\b|is\b|has\b|was\b|shows?\b|describes?\b|presents?\b|gave\b|joins?\b|talks?\b|explains?\b)")
        .unwrap()
        .is_match(title)
    {
        return false;
    }
    // Sentence fragments starting with verb-ed forms ("Built using ZIO...")
    if Regex::new(r"^(?:Built|Powered|Backed|Driven|Implemented|Hosted|Sponsored)\s+(?:using|by|with|on)\b")
        .unwrap()
        .is_match(title)
    {
        return false;
    }
    // Multi-sentence prose: titles are single fragments, not paragraphs
    if title.matches(". ").count() >= 2 { return false; }
    if title.contains(". ") && title.split_whitespace().count() > 14 { return false; }
    // Long prose-y titles ending with a period are almost always sentences, not titles
    if title.ends_with('.') && title.split_whitespace().count() > 12 { return false; }
    // Long titles ending with "!" or "?" are almost always exclamation/question prose
    if (title.ends_with('!') || title.ends_with('?'))
        && title.split_whitespace().count() > 12
    { return false; }
    // Intro form: "Name, a/an X who/that..." — biographical prose
    if Regex::new(r"^[A-Z][a-z]+ [A-Z][a-z]+, an? \w+").unwrap().is_match(title) {
        return false;
    }
    // "Come learn about ...", "Some considerations ...", "What would ..."
    let prose_starts: &[&str] = &[
        "Come learn ", "Come hear ", "Come see ",
        "Some considerations ", "Some thoughts ", "Some tips ",
        "What would ", "What if ", "What does ",
        "How would ", "How will ",
        "In this talk", "This talk ",
    ];
    if prose_starts.iter().any(|p| title.starts_with(p)) { return false; }
    !is_generic_item(title)
}

fn is_generic_item(text: &str) -> bool {
    let l = text.to_lowercase();
    let l = l.trim();
    if l.len() < 6 { return true; }

    let exact = [
        "networking", "food", "pizza", "beer", "checkin", "check-in",
        "doors open", "welcome", "registration", "refreshments", "social",
        "break", "q&a", "q & a", "intro", "agenda", "schedule",
        "announcements", "sponsor", "sponsors", "closing", "overview",
        "summary", "introduction", "description", "about", "join the community",
        "what to expect", "why join?", "why join", "sunset social",
        "welcome remarks", "speaker bio",
    ];
    if exact.contains(&l) { return true; }

    let prefixes = [
        "about the ", "about your ", "about our ", "featured talks",
        "join the ", "any questions", "everyone who", "google build",
        "register now", "sign up", "click here", "learn more",
        "you must be", "please note", "please contact", "for more info",
        "hands-on technical", "hands on technical",
        "lightning talk", "speaker 1 bio", "speaker 2 bio",
        "note: ", "our friends at", "registration is",
        "networking with", "great networking", "you have to",
        "live demos ", "ml and infra", "ways to get",
        "welcome remarks", "sunset social",
        "the on-going", "the mission of",
        "laptop to follow", "a photo id",
    ];
    if prefixes.iter().any(|p| l.starts_with(p)) { return true; }

    if text.trim_end().ends_with(':') { return true; }
    if looks_like_person_name_only(text) { return true; }

    false
}

fn looks_like_person_name_only(text: &str) -> bool {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() < 2 || words.len() > 3 { return false; }
    if !words.iter().all(|w| {
        let chars: Vec<char> = w.chars().collect();
        !chars.is_empty()
            && chars[0].is_uppercase()
            && chars[1..].iter().all(|c| c.is_lowercase())
            && w.len() <= 15
    }) { return false; }
    if text.contains(" and ") || text.contains(" of ")
        || text.contains(" with ") || text.contains(" for ")
        || text.contains(" in ")
    { return false; }
    passes_name_denylist(text)
}

/// Reject names whose first word is clearly not a given name, or whose last
/// word is clearly not a surname. Shared by both speaker-string parsing and
/// the stricter `looks_like_person_name_only` heuristic.
fn passes_name_denylist(text: &str) -> bool {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() { return false; }

    // Any digit in any word disqualifies (e.g. "A100 GPUs")
    if words.iter().any(|w| w.chars().any(|c| c.is_ascii_digit())) { return false; }

    let first = words[0].to_lowercase();
    let non_names = [
        "machine", "big", "deep", "applied", "open", "free", "cloud", "data",
        "community", "shapeless", "software", "enterprise", "distributed",
        "real", "advanced", "new", "fast", "reactive", "functional",
        "mobile", "web", "tech", "ai", "ml", "iot", "devops", "agile",
        "scala", "spark", "kafka", "hadoop", "akka", "play", "flink",
        "joint", "lightning", "keynote", "workshop", "panel", "session",
        "solving", "building", "using", "leveraging", "exploring", "introducing",
        "understanding", "scaling", "managing", "designing", "deploying",
        "getting", "making", "running", "powering", "enabling", "winning",
        "ask", "welcome", "apache", "exabytes", "four", "splice", "large",
        "small", "natural", "graph", "vector", "knowledge", "neural",
    ];
    if non_names.iter().any(|n| first == *n) { return false; }

    let last = words.last().unwrap().to_lowercase();
    let non_surnames = [
        "learning", "data", "day", "materialization", "workshop", "science",
        "engineering", "computing", "intelligence", "analytics", "systems",
        "platform", "framework", "technologies", "services", "solutions",
        "conference", "summit", "meetup", "group", "team", "labs", "research",
        "security", "privacy", "compliance", "management", "governance",
        "processing", "storage", "pipeline", "architecture", "infrastructure",
        "anything", "spark", "engines", "mates", "network", "machine",
        "daily", "models", "graph", "graphs", "search", "retrieval",
    ];
    if non_surnames.iter().any(|n| last == *n) { return false; }

    true
}

// ── abstract collection helpers ───────────────────────────────────────────────

fn collect_abstract(lines: &[&str], start: usize) -> Option<String> {
    let mut buf: Vec<&str> = Vec::new();
    for line in lines.iter().skip(start) {
        let t = line.trim();
        if t.is_empty() {
            if !buf.is_empty() { buf.push(""); }
        } else if t.starts_with('*') || t.starts_with('-') || t.starts_with('#')
            || re_numbered_section().is_match(t)
            || re_bold_title_line().is_match(t)
        {
            break;
        } else {
            buf.push(t);
        }
    }
    while buf.last().map_or(false, |s| s.is_empty()) {
        buf.pop();
    }
    let text = clean_md(&buf.join("\n"));
    (!text.trim().is_empty()).then_some(text.trim().to_string())
}

fn abstract_lines_count(lines: &[&str], start: usize) -> usize {
    let mut count = 0;
    let mut in_content = false;
    for line in lines.iter().skip(start) {
        let t = line.trim();
        if t.is_empty() {
            if in_content { count += 1; }
        } else if t.starts_with('*') || t.starts_with('-') || t.starts_with('#')
            || re_numbered_section().is_match(t)
            || re_bold_title_line().is_match(t)
        {
            break;
        } else {
            in_content = true;
            count += 1;
        }
    }
    count
}

// ── text utilities ────────────────────────────────────────────────────────────

pub fn markdown_to_text(md: &str) -> String {
    let s = re_md_link().replace_all(md, "$1");
    let s = re_md_emphasis().replace_all(&s, "$1");
    let s = Regex::new(r"(?m)^#{1,6}\s+").unwrap().replace_all(&s, "");
    let s = Regex::new(r"(?m)^[*\-\u{2022}]\s+").unwrap().replace_all(&s, "");
    let s = Regex::new(r"\\(.)").unwrap().replace_all(&s, "$1");
    let s = re_excess_nl().replace_all(&s, "\n\n");
    s.trim().to_string()
}

fn clean_md(s: &str) -> String {
    let s = re_md_link().replace_all(s, "$1");
    let s = re_md_emphasis().replace_all(&s, "$1");
    let s = s.replace('\\', "");
    s.trim().to_string()
}

// ── Apollo / DOM utilities ────────────────────────────────────────────────────

fn extract_apollo_state(html: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    let document = Html::parse_document(html);
    let sel = Selector::parse("script#__NEXT_DATA__").ok()?;
    let script = document.select(&sel).next()?;
    let raw: String = script.text().collect();
    let data: serde_json::Value = serde_json::from_str(&raw).ok()?;
    data["props"]["pageProps"]["__APOLLO_STATE__"]
        .as_object()
        .cloned()
}

fn extract_event_id(url: &str) -> Option<String> {
    let re = Regex::new(r"/events/(\d+)").unwrap();
    re.captures(url).map(|c| c[1].to_string())
}
