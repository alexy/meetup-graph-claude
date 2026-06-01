//! Unified graph loader — collects scraped talk JSON files into a deduplicated
//! graph then writes it to whichever backend was selected via `--backend`.
//!
//! Backends are provided by the `grust` crate and selected via feature flags:
//!   - [`grust::FalkorGraphStore`]      — FalkorDB via Redis + Cypher UNWIND/MERGE
//!   - [`grust::HelixHttpGraphStore`]   — HelixDB dynamic queries over raw HTTP
//!   - [`grust::HelixSdkGraphStore`]    — HelixDB via the helix-db Rust SDK
//!   - [`grust::SurrealHttpGraphStore`] — SurrealDB via REST `/sql` endpoint
//!   - [`grust::SurrealSdkGraphStore`]  — SurrealDB via the `surrealdb` Rust SDK (WebSocket)
//!
//! ```text
//! cargo run --bin load -- --backend falkor       [--url redis://localhost:6379] [--graph bythebay]
//! cargo run --bin load -- --backend helix-http   [--url http://localhost:6969]
//! cargo run --bin load -- --backend helix-sdk    [--url http://localhost:6969]
//! cargo run --bin load -- --backend surreal-http [--url http://localhost:8000] [--surreal-ns meetup --surreal-db graph]
//! cargo run --bin load -- --backend surreal-sdk  [--url http://localhost:8000] [--surreal-ns meetup --surreal-db graph]
//! ```

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use grust::{
    Edge as GrustEdge, Node as GrustNode, Props, Value,
    EdgePolicy, FalkorConfig, FalkorGraphStore, Graph, GraphAdminStore, GraphBuilder,
    HelixHttpConfig, HelixHttpGraphStore, HelixSdkConfig, HelixSdkGraphStore, SurrealConfig,
    SurrealHttpGraphStore, SurrealSdkGraphStore,
};
use tracing::info;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(clap::ValueEnum, Debug, Clone)]
enum Backend {
    /// FalkorDB via Redis + Cypher
    Falkor,
    /// HelixDB via dynamic HTTP queries
    #[value(name = "helix-http")]
    HelixHttp,
    /// HelixDB via the helix-db Rust SDK
    #[value(name = "helix-sdk")]
    HelixSdk,
    /// SurrealDB via REST `/sql` endpoint
    #[value(name = "surreal-http")]
    SurrealHttp,
    /// SurrealDB via the `surrealdb` Rust SDK over WebSocket
    #[value(name = "surreal-sdk")]
    SurrealSdk,
}

#[derive(Parser, Debug)]
#[command(about = "Load scraped talk JSON files into FalkorDB, HelixDB, or SurrealDB")]
struct Args {
    /// Directory containing talk JSON files
    #[arg(short, long, default_value = "data/talks")]
    input: PathBuf,

    /// Storage backend
    #[arg(long, value_enum, default_value_t = Backend::Falkor)]
    backend: Backend,

    /// Connection URL.
    /// Defaults: redis://localhost:6379 for falkor (env: FALKORDB_URL);
    ///           http://localhost:6969  for helix-* (env: HELIXDB_URL);
    ///           http://localhost:8000  for surreal-* (env: SURREALDB_URL).
    #[arg(long)]
    url: Option<String>,

    /// FalkorDB graph name [backend=falkor]
    #[arg(short, long, default_value = "bythebay")]
    graph: String,

    /// Drop all data before loading
    #[arg(long)]
    clear: bool,

    /// Nodes/edges per backend batch call
    #[arg(long, default_value_t = 100)]
    batch_size: usize,

    /// SurrealDB namespace [backend=surreal-*]
    #[arg(long, default_value = "meetup")]
    surreal_ns: String,

    /// SurrealDB database [backend=surreal-*]
    #[arg(long, default_value = "graph")]
    surreal_db: String,

    /// SurrealDB username [backend=surreal-*; env: SURREALDB_USER]
    #[arg(long, env = "SURREALDB_USER", default_value = "root")]
    surreal_user: String,

    /// SurrealDB password [backend=surreal-*; env: SURREALDB_PASS]
    #[arg(long, env = "SURREALDB_PASS", default_value = "root")]
    surreal_pass: String,
}

fn resolve_url(args: &Args) -> String {
    if let Some(u) = &args.url {
        return u.clone();
    }
    match args.backend {
        Backend::Falkor => std::env::var("FALKORDB_URL")
            .unwrap_or_else(|_| "redis://localhost:6379".to_string()),
        Backend::HelixHttp | Backend::HelixSdk => std::env::var("HELIXDB_URL")
            .unwrap_or_else(|_| "http://localhost:6969".to_string()),
        Backend::SurrealHttp | Backend::SurrealSdk => std::env::var("SURREALDB_URL")
            .unwrap_or_else(|_| "http://localhost:8000".to_string()),
    }
}

// ── URL helpers ───────────────────────────────────────────────────────────────

/// Ensure a HelixDB URL ends with `/v1/query` for the HTTP backend.
///
/// Accepts a bare base URL (`http://host:port`) or a full query URL; always
/// returns the canonical form with the `/v1/query` suffix.
pub fn helix_query_url(raw: &str) -> String {
    let base = raw.trim_end_matches('/');
    if base.ends_with("/v1/query") {
        base.to_string()
    } else {
        format!("{base}/v1/query")
    }
}

/// Ensure a SurrealDB URL ends with `/sql` for the HTTP backend.
///
/// The SDK backend ignores the path (only host:port is used), so this form
/// is safe to pass to both `SurrealHttpGraphStore` and `SurrealSdkGraphStore`.
pub fn surreal_sql_url(raw: &str) -> String {
    let base = raw.trim_end_matches('/');
    if base.ends_with("/sql") {
        base.to_string()
    } else {
        format!("{base}/sql")
    }
}

// ── Schema constants (for clear() label/relationship lists) ───────────────────

const NODE_LABELS: &[&str] = &["Talk", "Event", "Group", "Speaker", "Project", "Company"];
const EDGE_TYPES: &[&str] = &["PRESENTED_AT", "PRESENTED_BY", "PART_OF", "MENTIONS", "WORKS_AT"];

// ── Graph collection ──────────────────────────────────────────────────────────

/// Read and deduplicate all talk JSON files from `dir` into a single `Graph`.
///
/// Handles both the current grust format (`label`/`props`) and the legacy
/// pre-migration format (`type`/`properties` with plain JSON values).
/// Nodes are deduplicated by ID; edges by `(from, label, to)`.
fn collect(dir: &std::path::Path) -> Result<Graph> {
    let mut builder = GraphBuilder::new().edge_policy(EdgePolicy::DedupeByFromLabelTo);
    let mut file_count = 0usize;
    let mut legacy_count = 0usize;

    for entry in std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        file_count += 1;
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;

        // Try new grust format first, then fall back to legacy format.
        let fragment = match serde_json::from_str::<Graph>(&raw) {
            Ok(g) => g,
            Err(_) => match parse_legacy_talk_file(&raw) {
                Ok(g) => {
                    legacy_count += 1;
                    g
                }
                Err(e) => {
                    tracing::warn!("skip {}: {e}", path.display());
                    continue;
                }
            },
        };
        for node in fragment.nodes {
            builder.add_node(node);
        }
        for edge in fragment.edges {
            builder.add_edge(edge);
        }
    }

    if legacy_count > 0 {
        info!("read {file_count} talk files ({legacy_count} in legacy format) from {}", dir.display());
    } else {
        info!("read {file_count} talk files from {}", dir.display());
    }
    Ok(builder.build())
}

/// Parse a talk file written in the pre-grust-migration format:
/// `{"nodes": [{"id":"...", "type":"Talk", "properties":{...}}], "edges": [...]}`
fn parse_legacy_talk_file(raw: &str) -> Result<Graph> {
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct LegacyFile {
        nodes: Vec<LegacyNode>,
        edges: Vec<LegacyEdge>,
    }

    #[derive(Deserialize)]
    struct LegacyNode {
        id: String,
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        properties: serde_json::Value,
    }

    #[derive(Deserialize)]
    struct LegacyEdge {
        from: String,
        to: String,
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        properties: Option<serde_json::Value>,
    }

    let file: LegacyFile = serde_json::from_str(raw)?;

    let nodes = file
        .nodes
        .into_iter()
        .map(|n| {
            let props: Props = match n.properties {
                serde_json::Value::Object(map) => map
                    .into_iter()
                    // Skip the legacy bare "id" field so Node::new() can set
                    // it from the NodeId (the prefixed nid like "talk:bythebay-…").
                    // All grust backends key on props["id"] == node.id.as_str().
                    .filter(|(k, _)| k != "id")
                    .map(|(k, v)| (k, Value::from(v)))
                    .collect(),
                _ => Props::new(),
            };
            GrustNode::new(n.kind, n.id, props)
        })
        .collect();

    let edges = file
        .edges
        .into_iter()
        .map(|e| {
            let props: Props = match e.properties {
                Some(serde_json::Value::Object(m)) => {
                    m.into_iter().map(|(k, v)| (k, Value::from(v))).collect()
                }
                _ => Props::new(),
            };
            GrustEdge::new(e.kind, e.from, e.to, props)
        })
        .collect();

    Ok(Graph { nodes, edges })
}

// ── Load orchestration ────────────────────────────────────────────────────────

/// Bootstrap, optionally clear, then load the graph into `store`.
async fn load_graph(store: &impl GraphAdminStore, graph: &Graph, clear: bool) -> Result<()> {
    store
        .bootstrap()
        .await
        .map_err(|e| anyhow::anyhow!("bootstrap: {e}"))?;

    if clear {
        info!("clearing all graph data…");
        store
            .clear()
            .await
            .map_err(|e| anyhow::anyhow!("clear: {e}"))?;
        info!("graph cleared");
    }

    let report = store
        .put_graph(graph)
        .await
        .map_err(|e| anyhow::anyhow!("put_graph: {e}"))?;
    info!("loaded {} nodes, {} edges", report.nodes, report.edges);
    Ok(())
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("load=info".parse()?),
        )
        .init();

    let args = Args::parse();
    let url = resolve_url(&args);

    let graph = collect(&args.input)?;
    let n_node_kinds = graph
        .nodes
        .iter()
        .map(|n| &n.label)
        .collect::<BTreeSet<_>>()
        .len();
    let n_edge_kinds = graph
        .edges
        .iter()
        .map(|e| &e.label)
        .collect::<BTreeSet<_>>()
        .len();
    info!(
        "collected {} unique nodes ({} kinds), {} unique edges ({} kinds)",
        graph.nodes.len(),
        n_node_kinds,
        graph.edges.len(),
        n_edge_kinds,
    );

    let node_labels: Vec<String> = NODE_LABELS.iter().map(|s| s.to_string()).collect();
    let edge_types: Vec<String> = EDGE_TYPES.iter().map(|s| s.to_string()).collect();

    match args.backend {
        Backend::Falkor => {
            let store = FalkorGraphStore::new(FalkorConfig {
                redis_url: url,
                graph: args.graph.clone(),
                batch_size: args.batch_size,
                ..FalkorConfig::default()
            });
            load_graph(&store, &graph, args.clear).await?;
        }
        Backend::HelixHttp => {
            let store = HelixHttpGraphStore::connect(HelixHttpConfig {
                query_url: helix_query_url(&url),
                batch_size: args.batch_size,
                labels: node_labels,
            })
            .map_err(|e| anyhow::anyhow!("HelixDB HTTP: {e}"))?;
            load_graph(&store, &graph, args.clear).await?;
        }
        Backend::HelixSdk => {
            let store = HelixSdkGraphStore::connect(HelixSdkConfig {
                base_url: url,
                batch_size: args.batch_size,
                labels: node_labels,
            })
            .map_err(|e| anyhow::anyhow!("HelixDB SDK: {e}"))?;
            load_graph(&store, &graph, args.clear).await?;
        }
        Backend::SurrealHttp => {
            let store = SurrealHttpGraphStore::connect(SurrealConfig {
                url: surreal_sql_url(&url),
                user: args.surreal_user.clone(),
                pass: args.surreal_pass.clone(),
                namespace: args.surreal_ns.clone(),
                database: args.surreal_db.clone(),
                batch_size: args.batch_size,
                labels: node_labels,
                relationships: edge_types,
            })
            .map_err(|e| anyhow::anyhow!("SurrealDB HTTP: {e}"))?;
            load_graph(&store, &graph, args.clear).await?;
        }
        Backend::SurrealSdk => {
            let store = SurrealSdkGraphStore::connect(SurrealConfig {
                url: surreal_sql_url(&url),
                user: args.surreal_user.clone(),
                pass: args.surreal_pass.clone(),
                namespace: args.surreal_ns.clone(),
                database: args.surreal_db.clone(),
                batch_size: args.batch_size,
                labels: node_labels,
                relationships: edge_types,
            })
            .await
            .map_err(|e| anyhow::anyhow!("SurrealDB SDK: {e}"))?;
            load_graph(&store, &graph, args.clear).await?;
        }
    }

    info!("done");
    Ok(())
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helix_query_url_adds_suffix() {
        assert_eq!(helix_query_url("http://localhost:6969"), "http://localhost:6969/v1/query");
        assert_eq!(helix_query_url("http://localhost:6969/"), "http://localhost:6969/v1/query");
        assert_eq!(
            helix_query_url("http://localhost:6969/v1/query"),
            "http://localhost:6969/v1/query"
        );
        assert_eq!(
            helix_query_url("https://cluster.helix-db.com:443/v1/query"),
            "https://cluster.helix-db.com:443/v1/query"
        );
    }

    #[test]
    fn surreal_sql_url_adds_suffix() {
        assert_eq!(surreal_sql_url("http://localhost:8000"), "http://localhost:8000/sql");
        assert_eq!(surreal_sql_url("http://localhost:8000/"), "http://localhost:8000/sql");
        assert_eq!(surreal_sql_url("http://localhost:8000/sql"), "http://localhost:8000/sql");
        assert_eq!(
            surreal_sql_url("https://db.example.com:8000"),
            "https://db.example.com:8000/sql"
        );
    }
}
