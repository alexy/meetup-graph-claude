//! GitHub enrichment for Project nodes.
//!
//! Reads all talk JSON files produced by `llm-extract`, collects every unique
//! Project node, searches the GitHub API for the canonical repository URL of
//! each project, and rewrites the JSON files in-place with the `github_url`
//! field populated.
//!
//! ```text
//! # Enrich talks in data/talks-llm/ (default)
//! GITHUB_TOKEN=ghp_... cargo run --bin github-enrich
//!
//! # Point at a custom directory
//! cargo run --bin github-enrich -- --input data/talks-llm
//!
//! # Dry-run (print what would be set without writing)
//! cargo run --bin github-enrich -- --dry-run
//!
//! # Limit to N projects (for testing)
//! cargo run --bin github-enrich -- --limit 10
//! ```
//!
//! Authentication: set `GITHUB_TOKEN` for 5 000 req/hour.  Without it,
//! GitHub allows 60 req/hour (enough for small corpora; the binary back-offs
//! on 429 / 403 rate-limit responses).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(about = "Enrich Project nodes in talk JSONs with GitHub repository URLs")]
struct Args {
    /// Directory containing LLM-extracted talk JSON files
    #[arg(short, long, default_value = "data/talks-llm")]
    input: PathBuf,

    /// Milliseconds between GitHub API requests
    #[arg(long, default_value_t = 500)]
    delay_ms: u64,

    /// Print what would change but do not write any files
    #[arg(long)]
    dry_run: bool,

    /// Process at most this many unique projects (for testing)
    #[arg(long)]
    limit: Option<usize>,

    /// Skip projects that already have a non-empty github_url
    #[arg(long, default_value_t = true)]
    skip_enriched: bool,
}

// ── on-disk JSON shape ────────────────────────────────────────────────────────
// These structs are used only through serde deserialization; the fields are
// read by the GitHub search logic even though Rust doesn't see direct use.

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
struct TalkRecord {
    #[serde(flatten)]
    extra: serde_json::Value,
    nodes: Vec<RawNode>,
    edges: Vec<serde_json::Value>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawNode {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    properties: serde_json::Value,
}

// ── GitHub search response ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SearchResponse {
    items: Vec<RepoItem>,
}

#[derive(Debug, Deserialize)]
struct RepoItem {
    html_url: String,
    name: String,
    full_name: String,
    description: Option<String>,
}

// ── main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("github_enrich=info".parse()?),
        )
        .init();

    let args = Args::parse();
    let token = std::env::var("GITHUB_TOKEN").ok();
    if token.is_none() {
        warn!("GITHUB_TOKEN not set — unauthenticated rate limit is 60 req/hour");
    }

    // ── Phase 1: collect all (project_nid → name) pairs from every JSON file ─
    let mut project_names: BTreeMap<String, String> = BTreeMap::new(); // nid → name
    let mut file_paths: Vec<PathBuf> = Vec::new();

    let entries = std::fs::read_dir(&args.input)
        .with_context(|| format!("read_dir {}", args.input.display()))?;

    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        let rec: serde_json::Value = serde_json::from_str(&raw)
            .with_context(|| format!("parse {}", path.display()))?;

        if let Some(nodes) = rec["nodes"].as_array() {
            for node in nodes {
                if node["type"].as_str() != Some("Project") {
                    continue;
                }
                let nid = node["id"].as_str().unwrap_or("").to_string();
                let name = node["properties"]["name"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                let existing_url = node["properties"]["github_url"].as_str().unwrap_or("");

                if nid.is_empty() || name.is_empty() {
                    continue;
                }
                if args.skip_enriched && !existing_url.is_empty() {
                    continue;
                }
                project_names.entry(nid).or_insert(name);
            }
        }
        file_paths.push(path);
    }

    let mut projects: Vec<(String, String)> = project_names.into_iter().collect();
    if let Some(limit) = args.limit {
        projects.truncate(limit);
    }

    info!(
        "found {} unique project nids to search across {} files",
        projects.len(),
        file_paths.len()
    );

    if projects.is_empty() {
        info!("nothing to do");
        return Ok(());
    }

    // ── Phase 2: search GitHub for each project ───────────────────────────────
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("meetup-graph-enricher/1.0")
        .build()?;

    let mut enrichment: HashMap<String, String> = HashMap::new(); // nid → github_url

    for (nid, name) in &projects {
        match search_github(&client, token.as_deref(), name, args.delay_ms).await {
            Ok(Some(url)) => {
                info!("  {name} → {url}");
                enrichment.insert(nid.clone(), url);
            }
            Ok(None) => {
                info!("  {name} → (no match found)");
            }
            Err(e) => {
                warn!("  {name}: search failed: {e:#}");
            }
        }
        tokio::time::sleep(Duration::from_millis(args.delay_ms)).await;
    }

    info!(
        "found GitHub URLs for {}/{} projects",
        enrichment.len(),
        projects.len()
    );

    if args.dry_run {
        info!("--dry-run: skipping file writes");
        return Ok(());
    }

    // ── Phase 3: rewrite JSON files with enriched github_url ─────────────────
    let mut files_updated = 0usize;
    let mut nodes_updated = 0usize;

    for path in &file_paths {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("re-read {}", path.display()))?;
        let mut rec: serde_json::Value = serde_json::from_str(&raw)
            .with_context(|| format!("re-parse {}", path.display()))?;

        let mut changed = false;
        if let Some(nodes) = rec["nodes"].as_array_mut() {
            for node in nodes.iter_mut() {
                if node["type"].as_str() != Some("Project") {
                    continue;
                }
                let nid = node["id"].as_str().unwrap_or("").to_string();
                if let Some(url) = enrichment.get(&nid) {
                    node["properties"]["github_url"] =
                        serde_json::Value::String(url.clone());
                    changed = true;
                    nodes_updated += 1;
                }
            }
        }

        if changed {
            let json = serde_json::to_string_pretty(&rec)?;
            std::fs::write(path, json)
                .with_context(|| format!("write {}", path.display()))?;
            files_updated += 1;
        }
    }

    info!("updated {nodes_updated} Project nodes across {files_updated} files");
    Ok(())
}

// ── GitHub search ─────────────────────────────────────────────────────────────

/// Search GitHub for the canonical repository URL of `project_name`.
///
/// Returns `Ok(Some(url))` when a high-confidence match is found,
/// `Ok(None)` when nothing plausible turns up, or `Err` on network/API failure.
///
/// Matching heuristic: the repo `name` or `full_name` must contain the
/// (lowercased) project name (or vice-versa) after stripping common noise.
async fn search_github(
    client: &reqwest::Client,
    token: Option<&str>,
    project_name: &str,
    delay_ms: u64,
) -> Result<Option<String>> {
    let query = format!("{project_name} in:name,description");
    let url = format!(
        "https://api.github.com/search/repositories?q={}&sort=stars&order=desc&per_page=5",
        urlencoding::encode(&query)
    );

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let mut req = client.get(&url);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.context("GitHub API request")?;
        let status = resp.status();

        // Respect rate limit
        if (status == reqwest::StatusCode::FORBIDDEN
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS)
            && attempt <= 3
        {
            let wait = 60u64 * attempt as u64;
            warn!("GitHub rate limit hit; waiting {wait}s (attempt {attempt})");
            tokio::time::sleep(Duration::from_secs(wait)).await;
            continue;
        }
        if status.is_server_error() && attempt <= 3 {
            let wait = 5u64 * attempt as u64;
            warn!("GitHub server error {status}; retrying in {wait}s");
            tokio::time::sleep(Duration::from_secs(wait)).await;
            continue;
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("GitHub API HTTP {status}: {body}");
        }

        let search: SearchResponse = resp.json().await.context("decode GitHub response")?;
        let needle = normalize_name(project_name);

        for item in &search.items {
            let repo = normalize_name(&item.name);
            let full = normalize_name(&item.full_name);
            let desc = item
                .description
                .as_deref()
                .map(normalize_name)
                .unwrap_or_default();

            if repo.contains(&needle)
                || needle.contains(&repo)
                || full.contains(&needle)
                || desc.contains(&needle)
            {
                return Ok(Some(item.html_url.clone()));
            }
        }

        // Fallback: accept exact top result when name tokens overlap ≥50%
        if let Some(top) = search.items.first() {
            let repo_tokens: std::collections::HashSet<&str> =
                normalize_name(&top.name).leak().split('-').collect();
            let query_tokens: std::collections::HashSet<&str> =
                needle.as_str().split('-').collect();
            let intersection = repo_tokens.intersection(&query_tokens).count();
            let union = repo_tokens.union(&query_tokens).count();
            if union > 0 && intersection * 2 >= union {
                return Ok(Some(top.html_url.clone()));
            }
        }

        // Respect the caller-supplied delay between requests
        let _ = delay_ms; // already applied by caller
        return Ok(None);
    }
}

fn normalize_name(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}
