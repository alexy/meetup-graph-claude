mod config;
mod fetch;
mod gql;
mod models;
mod parse;
mod store;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing::{error, info, warn};

use config::GROUPS;
use fetch::Fetcher;
use gql::{GqlClient, GqlEvent};
use models::TalkData;
use parse::parse_from_gql;
use store::{build_talk_record, save_source, save_talk};

#[derive(Parser, Debug)]
#[command(about = "Scrape all ByTheBay meetup events into graph-ready JSON")]
struct Args {
    /// Output directory (source/ and talks/ subdirs are created here)
    #[arg(short, long, default_value = "data")]
    output: PathBuf,

    /// Milliseconds between requests (HTML fetches and GraphQL pages)
    #[arg(long, default_value_t = 1000)]
    delay_ms: u64,

    /// Only these groups (defaults to all 8 ByTheBay groups)
    #[arg(long, value_name = "GROUP")]
    groups: Vec<String>,

    /// Re-download HTML even if source file already exists
    #[arg(long)]
    force_refetch: bool,

    /// Re-parse already-saved source HTML files (no network calls)
    #[arg(long)]
    reparse: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("meetup_scraper=info".parse()?),
        )
        .init();

    let args = Args::parse();
    let cookies = std::env::var("MEETUP_COOKIES").ok();
    if cookies.is_none() {
        warn!("MEETUP_COOKIES not set — set it if you hit 403 errors.");
    }

    let groups: Vec<&str> = if args.groups.is_empty() {
        GROUPS.to_vec()
    } else {
        args.groups.iter().map(|s| s.as_str()).collect()
    };

    tokio::fs::create_dir_all(args.output.join("source")).await?;
    tokio::fs::create_dir_all(args.output.join("talks")).await?;

    let mut total_events = 0usize;
    let mut total_talks = 0usize;
    let mut total_skipped = 0usize;

    if args.reparse {
        // ── offline reparse mode ──────────────────────────────────────────
        for group in &groups {
            info!("── reparse {group} ──────────────────────────");
            let (ev, tk) = reparse_group(group, &args.output).await;
            total_events += ev;
            total_talks  += tk;
        }
    } else {
        // ── live scrape via GraphQL + HTML archival ───────────────────────
        let gql = GqlClient::new(args.delay_ms)?;
        let fetcher = Fetcher::new(args.delay_ms, cookies.as_deref())?;

        for group in &groups {
            info!("── group: {group} ──────────────────────────");

            let events = gql.all_events(group).await;
            info!("  {group}: {} events to process", events.len());

            for ev in &events {
                match process_event(ev, group, &fetcher, &args.output, args.force_refetch).await {
                    Ok((n, skipped)) => {
                        total_events += 1;
                        total_talks  += n;
                        total_skipped += skipped;
                    }
                    Err(e) => error!("  failed {}: {e:#}", ev.event_url),
                }
            }
        }
    }

    info!("════════════════════════════════════════════");
    info!(
        "done  events={total_events}  talks={total_talks}  html_skipped={total_skipped}"
    );
    Ok(())
}

// ── per-event processing ──────────────────────────────────────────────────────

/// Returns (n_talks, html_was_skipped).
async fn process_event(
    gql_ev: &GqlEvent,
    group: &str,
    fetcher: &Fetcher,
    output_dir: &std::path::Path,
    force_refetch: bool,
) -> Result<(usize, usize)> {
    let event_id = &gql_ev.id;
    let source_path = output_dir
        .join("source")
        .join(group)
        .join(format!("{event_id}.html"));

    // Fetch HTML (or reuse cached)
    let (html, was_cached) = if !force_refetch && source_path.exists() {
        let html = tokio::fs::read_to_string(&source_path).await?;
        (html, true)
    } else {
        let html = fetcher.get(&gql_ev.event_url).await?;
        (html, false)
    };

    // Save / overwrite source file
    let saved_source = save_source(group, event_id, &html, output_dir).await?;

    // Parse event data from GraphQL response (clean, structured)
    let mut event = parse_from_gql(gql_ev, group);

    // If GQL description was empty, try to recover from saved HTML
    if event.description_text.is_empty() {
        if let Ok(ev2) = parse::parse_event_page(&html, &gql_ev.event_url, group) {
            event.talks = ev2.talks;
            event.description_text = ev2.description_text;
        }
    }

    // Build and save talk records — only save talks that have at least one speaker
    let talks: Vec<&TalkData> = event
        .talks
        .iter()
        .filter(|t| !t.speakers.is_empty())
        .collect();

    for talk in &talks {
        let record = build_talk_record(talk, &event, &saved_source, output_dir);
        save_talk(&record, output_dir).await?;
    }

    Ok((talks.len(), was_cached as usize))
}

// ── offline reparse (no network) ─────────────────────────────────────────────

async fn reparse_group(group: &str, output_dir: &std::path::Path) -> (usize, usize) {
    let source_dir = output_dir.join("source").join(group);
    let Ok(mut entries) = tokio::fs::read_dir(&source_dir).await else {
        return (0, 0);
    };

    let mut events = 0usize;
    let mut talks  = 0usize;

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("html") {
            continue;
        }
        let Ok(html) = tokio::fs::read_to_string(&path).await else { continue };
        let event_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
        let url = format!("https://www.meetup.com/{group}/events/{event_id}/");

        match reparse_html(group, &event_id, &html, &url, output_dir).await {
            Ok(n) => { events += 1; talks += n; }
            Err(e) => tracing::error!("reparse {}: {e:#}", path.display()),
        }
    }
    info!("  reparsed {events} events, {talks} talks");
    (events, talks)
}

async fn reparse_html(
    group: &str,
    event_id: &str,
    html: &str,
    url: &str,
    output_dir: &std::path::Path,
) -> Result<usize> {
    let event = parse::parse_event_page(html, url, group)?;
    let source_path = save_source(group, event_id, html, output_dir).await?;

    let talks: Vec<&TalkData> = event
        .talks
        .iter()
        .filter(|t| !t.speakers.is_empty())
        .collect();

    for talk in &talks {
        let record = build_talk_record(talk, &event, &source_path, output_dir);
        save_talk(&record, output_dir).await?;
    }
    Ok(talks.len())
}
