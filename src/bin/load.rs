//! Unified graph loader with pluggable backends.
//!
//! Talk JSON files are parsed once into a deduplicated node/edge graph, then
//! dispatched to whichever backend was selected via `--backend`.
//!
//! Each backend implements the [`GraphLoader`] trait:
//!   - [`falkor::FalkorLoader`]    — FalkorDB via Redis + Cypher UNWIND/MERGE
//!   - [`helix_http::Loader`]      — HelixDB stored queries over raw HTTP
//!   - [`helix_sdk::Loader`]       — HelixDB dynamic write_batch() queries (no helix-gen needed)
//!   - [`surreal_http::Loader`]    — SurrealDB via REST `/sql` endpoint
//!   - [`surreal_sdk::Loader`]     — SurrealDB via the `surrealdb` Rust SDK (WebSocket)
//!
//! ```text
//! cargo run --bin load -- --backend falkor       [--url redis://localhost:6379] [--graph bythebay]
//! cargo run --bin load -- --backend helix-http   [--url http://localhost:8080]  # needs helix-gen
//! cargo run --bin load -- --backend helix-sdk    [--url http://localhost:8080] [--api-key KEY] [--batch-size 100]
//! cargo run --bin load -- --backend surreal-http [--url http://localhost:8000] [--surreal-ns meetup --surreal-db graph]
//! cargo run --bin load -- --backend surreal-sdk  [--url http://localhost:8000] [--surreal-ns meetup --surreal-db graph]
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use serde::Deserialize;
use tracing::info;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(clap::ValueEnum, Debug, Clone)]
enum Backend {
    /// FalkorDB via Redis + Cypher
    Falkor,
    /// HelixDB via raw HTTP stored queries (run `helix-gen` first)
    #[value(name = "helix-http")]
    HelixHttp,
    /// HelixDB via the helix-db Rust SDK (dynamic write_batch queries)
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
    /// URL normalization is applied per backend:
    ///   helix-*   — strips any /v1/query suffix, forces http://
    ///   surreal-sdk — converts http:// → ws://, strips any path
    #[arg(long)]
    url: Option<String>,

    /// FalkorDB graph name [backend=falkor]
    #[arg(short, long, default_value = "bythebay")]
    graph: String,

    /// HelixDB API key [backend=helix-sdk; env: HELIXDB_API_KEY]
    #[arg(long, env = "HELIXDB_API_KEY")]
    api_key: Option<String>,

    /// Drop all data before loading
    #[arg(long)]
    clear: bool,

    /// Max concurrent HTTP/SDK requests [helix-* and surreal-sdk backends]
    #[arg(long, default_value_t = 20)]
    concurrency: usize,

    /// Nodes/edges per HelixDB write_batch() call [backend=helix-sdk]
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

// ── URL normalization helpers (pub so unit tests can call them) ───────────────

/// Normalize any reasonable HelixDB URL to a bare HTTP base (no path).
///
/// Strips a trailing `/`, `/v1/query`, or `/v1/query/` suffix.
/// Accepts `http://`, `https://`, or a bare `host:port`.
///
/// ```text
/// helix_base_url("http://host:8080")           → "http://host:8080"
/// helix_base_url("http://host:8080/")          → "http://host:8080"
/// helix_base_url("http://host:8080/v1/query")  → "http://host:8080"
/// ```
pub fn helix_base_url(raw: &str) -> String {
    let s = raw.trim_end_matches('/');
    let s = s.strip_suffix("/v1/query").unwrap_or(s);
    let s = s.trim_end_matches('/');
    s.to_string()
}

/// Normalize any reasonable SurrealDB URL to a WebSocket address (no path).
///
/// Converts `http://` → `ws://` and `https://` → `wss://`.
/// Strips any path component after the host:port.
///
/// ```text
/// surreal_ws_address("ws://localhost:8000")          → "ws://localhost:8000"
/// surreal_ws_address("http://localhost:8000")        → "ws://localhost:8000"
/// surreal_ws_address("http://localhost:8000/rpc")    → "ws://localhost:8000"
/// surreal_ws_address("https://host.example.com")     → "wss://host.example.com"
/// ```
pub fn surreal_ws_address(raw: &str) -> String {
    let s = if raw.starts_with("http://") {
        raw.replacen("http://", "ws://", 1)
    } else if raw.starts_with("https://") {
        raw.replacen("https://", "wss://", 1)
    } else {
        raw.to_string()
    };
    // Strip everything after host:port (the path)
    let scheme_end = s.find("://").map(|i| i + 3).unwrap_or(0);
    let host_part = &s[scheme_end..];
    let path_start = host_part.find('/').unwrap_or(host_part.len());
    format!("{}{}", &s[..scheme_end], &host_part[..path_start])
}

// ── Shared graph data types ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct TalkRecord {
    nodes: Vec<RawNode>,
    edges: Vec<RawEdge>,
}

#[derive(Deserialize)]
struct RawNode {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    properties: serde_json::Value,
}

#[derive(Deserialize)]
struct RawEdge {
    from: String,
    to: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    properties: Option<serde_json::Value>,
}

/// nid → raw properties for all nodes of one label.
/// BTreeMap gives deterministic iteration order across runs.
pub type NodeKindMap = BTreeMap<String, serde_json::Value>;

/// (from_nid, to_nid) → optional edge properties for all edges of one type.
/// BTreeMap gives deterministic iteration order across runs.
pub type EdgeKindMap = BTreeMap<(String, String), Option<serde_json::Value>>;

type NodesByKind = BTreeMap<String, NodeKindMap>;
type EdgesByKind = BTreeMap<String, EdgeKindMap>;

/// Read and deduplicate all talk JSON files from `dir`.
fn collect(dir: &std::path::Path) -> Result<(NodesByKind, EdgesByKind)> {
    let mut nodes: NodesByKind = BTreeMap::new();
    let mut edges: EdgesByKind = BTreeMap::new();
    let mut file_count = 0usize;

    for entry in std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        file_count += 1;
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        let rec: TalkRecord = match serde_json::from_str(&raw) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("skip {}: {e}", path.display());
                continue;
            }
        };
        for node in rec.nodes {
            nodes
                .entry(node.kind)
                .or_default()
                .entry(node.id)
                .or_insert(node.properties);
        }
        for edge in rec.edges {
            edges
                .entry(edge.kind)
                .or_default()
                .entry((edge.from, edge.to))
                .or_insert(edge.properties);
        }
    }

    info!("read {file_count} talk files from {}", dir.display());
    Ok((nodes, edges))
}

// ── Graph schema (defined once, used by all backends) ─────────────────────────

const NODE_KINDS: &[&str] = &["Talk", "Event", "Group", "Speaker"];

/// (edge_type, from_node_label, to_node_label)
const EDGE_SCHEMA: &[(&str, &str, &str)] = &[
    ("PRESENTED_AT", "Talk", "Event"),
    ("PRESENTED_BY", "Talk", "Speaker"),
    ("PART_OF", "Event", "Group"),
];

// ── Shared node-property builder (single source of truth) ────────────────────

/// Build the canonical property map for a node of `kind`.
///
/// This is the single definition used by all backends — helix_http, helix_sdk,
/// surreal_http, and surreal_sdk all call this instead of maintaining their own
/// `build_node_body` / `build_node_content` copies.
pub fn node_props(kind: &str, nid: &str, props: &serde_json::Value) -> serde_json::Value {
    match kind {
        "Talk" => serde_json::json!({
            "nid":           nid,
            "title":         str_prop(props, "title"),
            "abstract_text": str_prop(props, "abstract"),
            "talk_order":    i64_prop(props, "order"),
        }),
        "Event" => serde_json::json!({
            "nid":           nid,
            "event_id":      str_prop(props, "id"),
            "title":         str_prop(props, "title"),
            "date":          str_prop(props, "date"),
            "datetime":      str_prop(props, "datetime"),
            "url":           str_prop(props, "url"),
            "venue_name":    str_prop(props, "venue_name"),
            "venue_address": str_prop(props, "venue_address"),
            "city":          str_prop(props, "city"),
        }),
        "Group" => serde_json::json!({
            "nid":  nid,
            "slug": str_prop(props, "slug"),
            "name": str_prop(props, "name"),
            "url":  str_prop(props, "url"),
        }),
        _ => serde_json::json!({
            "nid":     nid,
            "name":    str_prop(props, "name"),
            "bio":     str_prop(props, "bio"),
            "company": str_prop(props, "company"),
        }),
    }
}

// ── Backend abstraction ───────────────────────────────────────────────────────

struct BatchStats {
    loaded: usize,
    skipped: usize,
}

trait GraphLoader {
    /// Optional one-time setup (e.g. create namespace/database).
    /// Default implementation is a no-op.
    async fn bootstrap(&self) -> Result<()> {
        Ok(())
    }

    /// Drop all nodes and edges.
    async fn clear(&self) -> Result<()>;

    /// Upsert all nodes of a single label.
    async fn load_nodes_batch(&self, kind: &str, nodes: &NodeKindMap) -> Result<BatchStats>;

    /// Upsert all edges of a single type.
    /// `from_label`/`to_label` are the source/destination node labels.
    async fn load_edges_batch(
        &self,
        kind: &str,
        edges: &EdgeKindMap,
        from_label: &str,
        to_label: &str,
    ) -> Result<BatchStats>;
}

/// Parse once, then dispatch to the selected backend.
/// Calls `bootstrap()`, optionally `clear()`, then loads nodes then edges.
async fn load_graph(
    loader: &impl GraphLoader,
    nodes: &NodesByKind,
    edges: &EdgesByKind,
    clear: bool,
) -> Result<()> {
    loader.bootstrap().await?;

    if clear {
        info!("clearing all graph data…");
        loader.clear().await?;
        info!("graph cleared");
    }

    for &kind in NODE_KINDS {
        if let Some(n) = nodes.get(kind) {
            let s = loader.load_nodes_batch(kind, n).await?;
            info!("  {:>8}: {} loaded, {} skipped", kind, s.loaded, s.skipped);
        }
    }

    for &(kind, from_label, to_label) in EDGE_SCHEMA {
        if let Some(e) = edges.get(kind) {
            let s = loader.load_edges_batch(kind, e, from_label, to_label).await?;
            info!("  {:>14}: {} loaded, {} skipped", kind, s.loaded, s.skipped);
        }
    }

    Ok(())
}

// ── Shared helpers ────────────────────────────────────────────────────────────

fn str_prop<'a>(props: &'a serde_json::Value, key: &str) -> &'a str {
    props.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

fn i64_prop(props: &serde_json::Value, key: &str) -> i64 {
    props.get(key).and_then(|v| v.as_i64()).unwrap_or(0)
}

/// Drain a batch of spawned tasks (each returning `Result<()>`) into a
/// `BatchStats`, logging any per-task errors.
async fn collect_concurrent(handles: Vec<tokio::task::JoinHandle<Result<()>>>) -> BatchStats {
    let mut loaded = 0usize;
    let mut skipped = 0usize;
    for h in handles {
        match h.await {
            Ok(Ok(())) => loaded += 1,
            Ok(Err(e)) => {
                tracing::warn!("  task error: {e:#}");
                skipped += 1;
            }
            Err(e) => {
                tracing::warn!("  join error: {e}");
                skipped += 1;
            }
        }
    }
    BatchStats { loaded, skipped }
}

/// Drain a batch of spawned tasks (each returning `Result<usize>` — the
/// item count they processed) into a `BatchStats`.  Used by helix_sdk batching.
async fn collect_concurrent_counted(
    handles: Vec<tokio::task::JoinHandle<Result<usize>>>,
) -> BatchStats {
    let mut loaded = 0usize;
    let mut skipped = 0usize;
    for h in handles {
        match h.await {
            Ok(Ok(n)) => loaded += n,
            Ok(Err(e)) => {
                tracing::warn!("  task error: {e:#}");
                skipped += 1;
            }
            Err(e) => {
                tracing::warn!("  join error: {e}");
                skipped += 1;
            }
        }
    }
    BatchStats { loaded, skipped }
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

    let (nodes, edges) = collect(&args.input)?;
    let n_nodes: usize = nodes.values().map(|v| v.len()).sum();
    let n_edges: usize = edges.values().map(|v| v.len()).sum();
    info!(
        "collected {} unique nodes ({} kinds), {} unique edges ({} kinds)",
        n_nodes,
        nodes.len(),
        n_edges,
        edges.len(),
    );

    match args.backend {
        Backend::Falkor => {
            let loader = falkor::FalkorLoader::new(&url, &args.graph)?;
            load_graph(&loader, &nodes, &edges, args.clear).await?;
        }
        Backend::HelixHttp => {
            let loader = helix_http::Loader::new(&url, args.concurrency)?;
            load_graph(&loader, &nodes, &edges, args.clear).await?;
        }
        Backend::HelixSdk => {
            let loader = helix_sdk::Loader::new(
                &url,
                args.api_key.as_deref(),
                args.concurrency,
                args.batch_size,
            )?;
            load_graph(&loader, &nodes, &edges, args.clear).await?;
        }
        Backend::SurrealHttp => {
            let loader = surreal_http::Loader::new(
                &url,
                &args.surreal_ns,
                &args.surreal_db,
                &args.surreal_user,
                &args.surreal_pass,
            )?;
            load_graph(&loader, &nodes, &edges, args.clear).await?;
        }
        Backend::SurrealSdk => {
            let loader = surreal_sdk::Loader::new(
                &url,
                &args.surreal_ns,
                &args.surreal_db,
                &args.surreal_user,
                &args.surreal_pass,
                args.concurrency,
            )
            .await?;
            load_graph(&loader, &nodes, &edges, args.clear).await?;
        }
    }

    info!("done");
    Ok(())
}

// ── Backend: FalkorDB ─────────────────────────────────────────────────────────

mod falkor {
    use super::{BatchStats, EdgeKindMap, GraphLoader, NodeKindMap};
    use anyhow::{Context, Result, bail};
    use std::sync::Mutex;

    pub struct FalkorLoader {
        conn: Mutex<redis::Connection>,
        graph: String,
    }

    impl FalkorLoader {
        pub fn new(url: &str, graph: &str) -> Result<Self> {
            let conn = redis::Client::open(url)
                .context("redis client")?
                .get_connection()
                .context("redis connection")?;
            Ok(Self {
                conn: Mutex::new(conn),
                graph: graph.to_string(),
            })
        }

        fn cypher_query(&self, q: &str) -> Result<()> {
            let mut conn = self.conn.lock().unwrap();
            redis::cmd("GRAPH.QUERY")
                .arg(&self.graph)
                .arg(q)
                .query::<redis::Value>(&mut *conn)
                .context("GRAPH.QUERY")?;
            Ok(())
        }
    }

    impl GraphLoader for FalkorLoader {
        async fn clear(&self) -> Result<()> {
            tokio::task::block_in_place(|| {
                let mut conn = self.conn.lock().unwrap();
                // Ignore "graph not found" errors on first run.
                let _: redis::RedisResult<redis::Value> =
                    redis::cmd("GRAPH.DELETE").arg(&self.graph).query(&mut *conn);
                Ok(())
            })
        }

        async fn load_nodes_batch(&self, kind: &str, nodes: &NodeKindMap) -> Result<BatchStats> {
            if !is_safe_label(kind) {
                bail!("unsafe label: {kind}");
            }
            if nodes.is_empty() {
                return Ok(BatchStats { loaded: 0, skipped: 0 });
            }
            tokio::task::block_in_place(|| {
                // Best-effort index creation; ignore "already exists" errors.
                let _ = self.cypher_query(&format!(
                    "CREATE INDEX FOR (n:{kind}) ON (n.nid)"
                ));
                let array = nodes_array(nodes);
                self.cypher_query(&format!(
                    "UNWIND {array} AS row \
                     MERGE (n:{kind} {{nid: row.nid}}) \
                     SET n += row.props"
                ))
                .with_context(|| format!("MERGE {kind} nodes"))?;
                Ok(BatchStats {
                    loaded: nodes.len(),
                    skipped: 0,
                })
            })
        }

        async fn load_edges_batch(
            &self,
            kind: &str,
            edges: &EdgeKindMap,
            _from_label: &str,
            _to_label: &str,
        ) -> Result<BatchStats> {
            if !is_safe_label(kind) {
                bail!("unsafe relationship type: {kind}");
            }
            if edges.is_empty() {
                return Ok(BatchStats { loaded: 0, skipped: 0 });
            }
            tokio::task::block_in_place(|| {
                let array = edges_array(edges);
                self.cypher_query(&format!(
                    "UNWIND {array} AS row \
                     MATCH (a {{nid: row.from}}), (b {{nid: row.to}}) \
                     MERGE (a)-[r:{kind}]->(b) \
                     SET r += row.props"
                ))
                .with_context(|| format!("MERGE {kind} edges"))?;
                Ok(BatchStats {
                    loaded: edges.len(),
                    skipped: 0,
                })
            })
        }
    }

    // ── Cypher literal builders (pub(super) so unit tests can inspect them) ──

    /// Build a Cypher array literal: `[{nid:'...', props:{...}}, ...]`
    pub(super) fn nodes_array(nodes: &NodeKindMap) -> String {
        let items: Vec<String> = nodes
            .iter()
            .map(|(nid, props)| {
                format!(
                    "{{nid:{},props:{}}}",
                    cypher_str(nid),
                    json_to_cypher(props)
                )
            })
            .collect();
        format!("[{}]", items.join(","))
    }

    /// Build a Cypher array literal: `[{from:'...', to:'...', props:{...}}, ...]`
    pub(super) fn edges_array(edges: &EdgeKindMap) -> String {
        let empty = serde_json::Value::Object(Default::default());
        let items: Vec<String> = edges
            .iter()
            .map(|((from, to), props)| {
                let p = props.as_ref().unwrap_or(&empty);
                format!(
                    "{{from:{},to:{},props:{}}}",
                    cypher_str(from),
                    cypher_str(to),
                    json_to_cypher(p)
                )
            })
            .collect();
        format!("[{}]", items.join(","))
    }

    fn json_to_cypher(v: &serde_json::Value) -> String {
        match v {
            serde_json::Value::Null => "null".into(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => cypher_str(s),
            serde_json::Value::Array(a) => {
                format!(
                    "[{}]",
                    a.iter().map(json_to_cypher).collect::<Vec<_>>().join(",")
                )
            }
            serde_json::Value::Object(o) => {
                let pairs: Vec<String> = o
                    .iter()
                    .map(|(k, v)| format!("{}:{}", cypher_key(k), json_to_cypher(v)))
                    .collect();
                format!("{{{}}}", pairs.join(","))
            }
        }
    }

    /// Escape a string as a single-quoted Cypher string literal.
    pub(super) fn cypher_str(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('\'');
        for c in s.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\'' => out.push_str("\\'"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c => out.push(c),
            }
        }
        out.push('\'');
        out
    }

    fn cypher_key(k: &str) -> String {
        let safe = k
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_alphanumeric() || b == b'_' || (i > 0 && b.is_ascii_digit()));
        if safe && !k.is_empty() && k.as_bytes()[0].is_ascii_alphabetic() {
            k.to_string()
        } else {
            format!("`{}`", k.replace('`', ""))
        }
    }

    fn is_safe_label(s: &str) -> bool {
        !s.is_empty()
            && s.chars().next().map_or(false, |c| c.is_ascii_alphabetic())
            && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    }
}

// ── Backend: HelixDB raw HTTP (stored queries) ────────────────────────────────

mod helix_http {
    use super::{
        BatchStats, EdgeKindMap, GraphLoader, NodeKindMap, collect_concurrent, helix_base_url,
        node_props,
    };
    use anyhow::{Context, Result, bail};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Semaphore;

    pub struct Loader {
        client: reqwest::Client,
        base_url: Arc<String>,
        concurrency: usize,
    }

    impl Loader {
        pub fn new(raw_url: &str, concurrency: usize) -> Result<Self> {
            Ok(Self {
                client: reqwest::Client::builder()
                    .timeout(Duration::from_secs(30))
                    .build()?,
                base_url: Arc::new(helix_base_url(raw_url)),
                concurrency,
            })
        }
    }

    impl GraphLoader for Loader {
        async fn clear(&self) -> Result<()> {
            http_post(&self.client, &self.base_url, "clear_all", serde_json::json!({})).await
        }

        async fn load_nodes_batch(&self, kind: &str, nodes: &NodeKindMap) -> Result<BatchStats> {
            let ep = Arc::new(node_endpoint(kind).to_string());
            let sem = Arc::new(Semaphore::new(self.concurrency));
            let mut handles = Vec::new();
            for (nid, props) in nodes {
                let body = node_props(kind, nid, props);
                let client = self.client.clone();
                let base_url = self.base_url.clone();
                let ep = ep.clone();
                let sem = sem.clone();
                handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire_owned().await?;
                    http_post(&client, &base_url, &ep, body).await
                }));
            }
            Ok(collect_concurrent(handles).await)
        }

        async fn load_edges_batch(
            &self,
            kind: &str,
            edges: &EdgeKindMap,
            _from_label: &str,
            _to_label: &str,
        ) -> Result<BatchStats> {
            let (ep, from_param, to_param) = edge_endpoint(kind)?;
            let ep = Arc::new(ep.to_string());
            let sem = Arc::new(Semaphore::new(self.concurrency));
            let mut handles = Vec::new();
            for ((from, to), _) in edges {
                let mut m = serde_json::Map::new();
                m.insert(from_param.to_owned(), serde_json::Value::String(from.clone()));
                m.insert(to_param.to_owned(), serde_json::Value::String(to.clone()));
                let body = serde_json::Value::Object(m);
                let client = self.client.clone();
                let base_url = self.base_url.clone();
                let ep = ep.clone();
                let sem = sem.clone();
                handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire_owned().await?;
                    http_post(&client, &base_url, &ep, body).await
                }));
            }
            Ok(collect_concurrent(handles).await)
        }
    }

    async fn http_post(
        client: &reqwest::Client,
        base_url: &str,
        name: &str,
        body: serde_json::Value,
    ) -> Result<()> {
        let resp = client
            .post(format!("{base_url}/v1/query/{name}"))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST /v1/query/{name}"))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let text = resp.text().await.unwrap_or_default();
        bail!("HTTP {status} from /v1/query/{name}: {text}");
    }

    fn node_endpoint(kind: &str) -> &'static str {
        match kind {
            "Talk" => "add_talk",
            "Event" => "add_event",
            "Group" => "add_group",
            _ => "add_speaker",
        }
    }

    fn edge_endpoint(kind: &str) -> Result<(&'static str, &'static str, &'static str)> {
        Ok(match kind {
            "PRESENTED_AT" => ("add_presented_at", "talk_nid", "event_nid"),
            "PRESENTED_BY" => ("add_presented_by", "talk_nid", "speaker_nid"),
            "PART_OF" => ("add_part_of", "event_nid", "group_nid"),
            other => bail!("unknown edge kind: {other}"),
        })
    }
}

// ── Backend: HelixDB Rust SDK (dynamic write_batch queries) ───────────────────
//
// Nodes and edges are batched into chunks of `batch_size` and sent as a single
// write_batch() call per chunk, reducing round-trips and write-conflict retries.

mod helix_sdk {
    use super::{
        BatchStats, EdgeKindMap, GraphLoader, NodeKindMap, collect_concurrent_counted,
        helix_base_url, node_props,
    };
    use anyhow::Result;
    use helix_db::{
        Client,
        dsl::prelude::{
            DynamicQueryRequest, NodeRef, PropertyValue, SourcePredicate, g, write_batch,
        },
    };
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    pub struct Loader {
        client: Arc<Client>,
        concurrency: usize,
        batch_size: usize,
    }

    impl Loader {
        pub fn new(
            raw_url: &str,
            api_key: Option<&str>,
            concurrency: usize,
            batch_size: usize,
        ) -> Result<Self> {
            let url = helix_base_url(raw_url);
            let client = Client::new(Some(&url))
                .map_err(|e| anyhow::anyhow!("HelixDB client: {e}"))?
                .with_api_key(api_key);
            Ok(Self {
                client: Arc::new(client),
                concurrency,
                batch_size: batch_size.max(1),
            })
        }

        async fn send(&self, req: DynamicQueryRequest) -> Result<()> {
            self.client
                .query::<serde_json::Value>()
                .dynamic_query(req)
                .send()
                .await
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!("{e}"))
        }
    }

    impl GraphLoader for Loader {
        async fn clear(&self) -> Result<()> {
            self.send(DynamicQueryRequest::write(
                write_batch()
                    .var_as("talks", g().n_with_label("Talk").drop())
                    .var_as("events", g().n_with_label("Event").drop())
                    .var_as("groups", g().n_with_label("Group").drop())
                    .var_as("speakers", g().n_with_label("Speaker").drop())
                    .returning(["talks", "events", "groups", "speakers"]),
            ))
            .await
        }

        async fn load_nodes_batch(&self, kind: &str, nodes: &NodeKindMap) -> Result<BatchStats> {
            let items: Vec<(String, serde_json::Value)> = nodes
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();

            let sem = Arc::new(Semaphore::new(self.concurrency));
            let mut handles: Vec<tokio::task::JoinHandle<Result<usize>>> = Vec::new();

            for chunk in items.chunks(self.batch_size) {
                let chunk_owned: Vec<(String, serde_json::Value)> = chunk.to_vec();
                let client = self.client.clone();
                let sem = sem.clone();
                let kind = kind.to_string();
                handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire_owned().await?;
                    let n = chunk_owned.len();
                    let req = make_node_batch(&kind, &chunk_owned);
                    client
                        .query::<serde_json::Value>()
                        .dynamic_query(req)
                        .send()
                        .await
                        .map(|_| n)
                        .map_err(|e| anyhow::anyhow!("node batch: {e}"))
                }));
            }
            Ok(collect_concurrent_counted(handles).await)
        }

        async fn load_edges_batch(
            &self,
            kind: &str,
            edges: &EdgeKindMap,
            from_label: &str,
            to_label: &str,
        ) -> Result<BatchStats> {
            let items: Vec<((String, String), Option<serde_json::Value>)> = edges
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();

            let sem = Arc::new(Semaphore::new(self.concurrency));
            let mut handles: Vec<tokio::task::JoinHandle<Result<usize>>> = Vec::new();

            for chunk in items.chunks(self.batch_size) {
                let chunk_owned: Vec<((String, String), Option<serde_json::Value>)> =
                    chunk.to_vec();
                let client = self.client.clone();
                let sem = sem.clone();
                let kind = kind.to_string();
                let from_label = from_label.to_string();
                let to_label = to_label.to_string();
                handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire_owned().await?;
                    let n = chunk_owned.len();
                    let req = make_edge_batch(&kind, &from_label, &to_label, &chunk_owned);
                    client
                        .query::<serde_json::Value>()
                        .dynamic_query(req)
                        .send()
                        .await
                        .map(|_| n)
                        .map_err(|e| anyhow::anyhow!("edge batch: {e}"))
                }));
            }
            Ok(collect_concurrent_counted(handles).await)
        }
    }

    // ── Batch builders ────────────────────────────────────────────────────────

    /// Build a single write_batch() that upserts all nodes in `chunk`.
    fn make_node_batch(
        kind: &str,
        chunk: &[(String, serde_json::Value)],
    ) -> DynamicQueryRequest {
        let mut batch = write_batch();
        let mut names: Vec<String> = Vec::with_capacity(chunk.len());
        for (i, (nid, raw_props)) in chunk.iter().enumerate() {
            let name = format!("n{i}");
            let np = node_props(kind, nid, raw_props);
            let pairs = json_to_helix_pairs(&np);
            batch = batch.var_as(&name, g().add_n(kind, pairs));
            names.push(name);
        }
        DynamicQueryRequest::write(batch.returning(names.iter().map(|s| s.as_str())))
    }

    /// Build a single write_batch() that creates all edges in `chunk`.
    ///
    /// For each edge i the batch contains three vars:
    ///   `src{i}` — source node lookup
    ///   `dst{i}` — destination node lookup
    ///   `e{i}`   — the RELATES step
    fn make_edge_batch(
        kind: &str,
        from_label: &str,
        to_label: &str,
        chunk: &[((String, String), Option<serde_json::Value>)],
    ) -> DynamicQueryRequest {
        let mut batch = write_batch();
        let mut e_names: Vec<String> = Vec::with_capacity(chunk.len());
        for (i, ((from_nid, to_nid), _)) in chunk.iter().enumerate() {
            let src_var = format!("src{i}");
            let dst_var = format!("dst{i}");
            let e_var = format!("e{i}");
            batch = batch
                .var_as(
                    &src_var,
                    g().n_with_label_where(
                        from_label,
                        SourcePredicate::eq("nid", from_nid.as_str()),
                    )
                    .limit(1),
                )
                .var_as(
                    &dst_var,
                    g().n_with_label_where(
                        to_label,
                        SourcePredicate::eq("nid", to_nid.as_str()),
                    )
                    .limit(1),
                )
                .var_as(
                    &e_var,
                    g().n(NodeRef::var(&src_var))
                        .add_e(kind, NodeRef::var(&dst_var), Vec::<(&str, &str)>::new())
                        .count(),
                );
            e_names.push(e_var);
        }
        DynamicQueryRequest::write(batch.returning(e_names.iter().map(|s| s.as_str())))
    }

    /// Convert a `serde_json::Value::Object` to helix-db `(String, PropertyValue)` pairs.
    fn json_to_helix_pairs(v: &serde_json::Value) -> Vec<(String, PropertyValue)> {
        let obj = match v {
            serde_json::Value::Object(m) => m,
            _ => return vec![],
        };
        obj.iter()
            .map(|(k, v)| {
                let pv = match v {
                    serde_json::Value::String(s) => PropertyValue::from(s.as_str()),
                    serde_json::Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            PropertyValue::I64(i)
                        } else if let Some(f) = n.as_f64() {
                            PropertyValue::F64(f)
                        } else {
                            PropertyValue::Null
                        }
                    }
                    serde_json::Value::Bool(b) => PropertyValue::Bool(*b),
                    serde_json::Value::Null => PropertyValue::Null,
                    other => PropertyValue::from(other.to_string()),
                };
                (k.clone(), pv)
            })
            .collect()
    }
}

// ── Backend: SurrealDB REST HTTP (`/sql` endpoint) ────────────────────────────

mod surreal_http {
    use super::{BatchStats, EdgeKindMap, GraphLoader, NodeKindMap, node_props};
    use anyhow::{Context, Result};
    use std::time::Duration;

    pub struct Loader {
        client: reqwest::Client,
        base_url: String,
        ns: String,
        db: String,
        user: String,
        pass: String,
    }

    impl Loader {
        pub fn new(
            base_url: &str,
            ns: &str,
            database: &str,
            user: &str,
            pass: &str,
        ) -> Result<Self> {
            Ok(Self {
                client: reqwest::Client::builder()
                    .timeout(Duration::from_secs(60))
                    .build()?,
                base_url: base_url.trim_end_matches('/').to_string(),
                ns: ns.to_string(),
                db: database.to_string(),
                user: user.to_string(),
                pass: pass.to_string(),
            })
        }

        /// POST a multi-statement SurrealQL body to `/sql` and tally OK/ERR results.
        async fn run_batch(&self, sql: &str) -> Result<BatchStats> {
            self.run_sql(sql, true).await
        }

        /// POST SurrealQL to `/sql`.  When `with_ns_db` is false the Surreal-Ns/Db
        /// headers are omitted (root-level statements like DEFINE NAMESPACE).
        async fn run_sql(&self, sql: &str, with_ns_db: bool) -> Result<BatchStats> {
            let mut req = self
                .client
                .post(format!("{}/sql", self.base_url))
                .basic_auth(&self.user, Some(&self.pass))
                .header("Content-Type", "text/plain");
            if with_ns_db {
                req = req
                    .header("Surreal-Ns", &self.ns)
                    .header("Surreal-Db", &self.db);
            }
            let resp = req.body(sql.to_string()).send().await.context("POST /sql")?;

            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("HTTP {status}: {text}");
            }

            let results: Vec<serde_json::Value> =
                resp.json().await.context("parse /sql response")?;

            let mut loaded = 0usize;
            let mut skipped = 0usize;
            for r in &results {
                if r.get("status").and_then(|s| s.as_str()) == Some("OK") {
                    loaded += 1;
                } else {
                    skipped += 1;
                    tracing::warn!(
                        "surreal-http: {}",
                        r.get("detail").and_then(|d| d.as_str()).unwrap_or("?")
                    );
                }
            }
            Ok(BatchStats { loaded, skipped })
        }
    }

    impl GraphLoader for Loader {
        /// Create the SurrealDB namespace and database if they don't already exist.
        ///
        /// Uses `IF NOT EXISTS` so it's idempotent; errors are logged but not fatal,
        /// allowing the tool to work against an already-bootstrapped instance.
        async fn bootstrap(&self) -> Result<()> {
            // Step 1 — DEFINE NAMESPACE at root level (no ns/db headers).
            if let Err(e) = self
                .run_sql(
                    &format!("DEFINE NAMESPACE IF NOT EXISTS {};", self.ns),
                    false,
                )
                .await
            {
                tracing::warn!("SurrealDB bootstrap (DEFINE NAMESPACE): {e}");
            }
            // Step 2 — DEFINE DATABASE within the namespace.
            if let Err(e) = self
                .run_sql(
                    &format!("DEFINE DATABASE IF NOT EXISTS {};", self.db),
                    true,
                )
                .await
            {
                tracing::warn!("SurrealDB bootstrap (DEFINE DATABASE): {e}");
            }
            Ok(())
        }

        async fn clear(&self) -> Result<()> {
            let sql = "DELETE talk; DELETE event; DELETE group; DELETE speaker; \
                       DELETE presented_at; DELETE presented_by; DELETE part_of;";
            self.run_batch(sql).await.map(|_| ())
        }

        async fn load_nodes_batch(&self, kind: &str, nodes: &NodeKindMap) -> Result<BatchStats> {
            if nodes.is_empty() {
                return Ok(BatchStats { loaded: 0, skipped: 0 });
            }
            let table = kind.to_lowercase();
            let mut sql = String::new();
            for (nid, raw_props) in nodes {
                let id = surreal_id(&table, nid);
                let content = node_props(kind, nid, raw_props);
                let json =
                    serde_json::to_string(&content).unwrap_or_else(|_| "{}".to_string());
                sql.push_str(&format!("UPSERT {id} CONTENT {json};\n"));
            }
            self.run_batch(&sql).await
        }

        async fn load_edges_batch(
            &self,
            kind: &str,
            edges: &EdgeKindMap,
            from_label: &str,
            to_label: &str,
        ) -> Result<BatchStats> {
            if edges.is_empty() {
                return Ok(BatchStats { loaded: 0, skipped: 0 });
            }
            let edge_table = kind.to_lowercase();
            let from_table = from_label.to_lowercase();
            let to_table = to_label.to_lowercase();
            let mut sql = String::new();
            for ((from_nid, to_nid), _) in edges {
                let from = surreal_id(&from_table, from_nid);
                let to = surreal_id(&to_table, to_nid);
                sql.push_str(&format!("RELATE {from}->{edge_table}->{to};\n"));
            }
            self.run_batch(&sql).await
        }
    }

    /// Produce a SurrealDB record ID with angle-bracket-escaped key (U+27E8 / U+27E9).
    ///
    /// E.g. `surreal_id("talk", "talk:219857140")` → `talk:⟨talk:219857140⟩`
    pub(super) fn surreal_id(table: &str, nid: &str) -> String {
        format!("{table}:\u{27E8}{nid}\u{27E9}")
    }
}

// ── Backend: SurrealDB Rust SDK (WebSocket) ───────────────────────────────────

mod surreal_sdk {
    use super::{
        BatchStats, EdgeKindMap, GraphLoader, NodeKindMap, collect_concurrent, node_props,
        surreal_ws_address,
    };
    use anyhow::Result;
    use std::sync::Arc;
    use surrealdb::{
        Surreal,
        engine::remote::ws::{Client as WsClient, Ws},
        opt::auth::Root,
        types::RecordId,
    };
    use tokio::sync::Semaphore;

    pub struct Loader {
        db: Surreal<WsClient>,
        concurrency: usize,
    }

    impl Loader {
        pub async fn new(
            raw_url: &str,
            ns: &str,
            database: &str,
            user: &str,
            pass: &str,
            concurrency: usize,
        ) -> Result<Self> {
            let ws_url = surreal_ws_address(raw_url);
            let db: Surreal<WsClient> = Surreal::new::<Ws>(ws_url.as_str())
                .await
                .map_err(|e| anyhow::anyhow!("SurrealDB connect ({ws_url}): {e}"))?;
            db.signin(Root {
                username: user.to_string(),
                password: pass.to_string(),
            })
            .await
            .map_err(|e| anyhow::anyhow!("SurrealDB signin: {e}"))?;

            // Bootstrap at root level (before selecting ns/db).
            let _ = db
                .query(format!("DEFINE NAMESPACE IF NOT EXISTS {};", ns))
                .await;
            let _ = db
                .query(format!("USE NS {}; DEFINE DATABASE IF NOT EXISTS {};", ns, database))
                .await;

            db.use_ns(ns)
                .use_db(database)
                .await
                .map_err(|e| anyhow::anyhow!("SurrealDB use_ns/use_db: {e}"))?;
            Ok(Self { db, concurrency })
        }
    }

    impl GraphLoader for Loader {
        async fn clear(&self) -> Result<()> {
            self.db
                .query(
                    "DELETE talk; DELETE event; DELETE group; DELETE speaker; \
                     DELETE presented_at; DELETE presented_by; DELETE part_of;",
                )
                .await
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!("clear: {e}"))
        }

        async fn load_nodes_batch(&self, kind: &str, nodes: &NodeKindMap) -> Result<BatchStats> {
            let sem = Arc::new(Semaphore::new(self.concurrency));
            let mut handles = Vec::new();
            for (nid, raw_props) in nodes {
                let nid = nid.clone();
                let raw_props = raw_props.clone();
                let db = self.db.clone();
                let sem = sem.clone();
                let table = kind.to_lowercase();
                let kind = kind.to_string();
                handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire_owned().await?;
                    let rid = RecordId::new(table.clone(), nid.clone());
                    let content = node_props(&kind, &nid, &raw_props);
                    db.upsert::<Option<serde_json::Value>>(rid)
                        .content(content)
                        .await
                        .map(|_| ())
                        .map_err(|e| anyhow::anyhow!("upsert {table} {nid}: {e}"))
                }));
            }
            Ok(collect_concurrent(handles).await)
        }

        async fn load_edges_batch(
            &self,
            kind: &str,
            edges: &EdgeKindMap,
            from_label: &str,
            to_label: &str,
        ) -> Result<BatchStats> {
            let sem = Arc::new(Semaphore::new(self.concurrency));
            let mut handles = Vec::new();
            let edge_table = kind.to_lowercase();
            for ((from_nid, to_nid), _) in edges {
                let from_nid = from_nid.clone();
                let to_nid = to_nid.clone();
                let db = self.db.clone();
                let sem = sem.clone();
                let from_table = from_label.to_lowercase();
                let to_table = to_label.to_lowercase();
                let edge_table = edge_table.clone();
                handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire_owned().await?;
                    let from = RecordId::new(from_table, from_nid);
                    let to = RecordId::new(to_table, to_nid);
                    db.query(format!("RELATE $from->{edge_table}->$to"))
                        .bind(("from", from))
                        .bind(("to", to))
                        .await
                        .map(|_| ())
                        .map_err(|e| anyhow::anyhow!("relate {edge_table}: {e}"))
                }));
            }
            Ok(collect_concurrent(handles).await)
        }
    }
}

// ── Unit tests (no live DB required) ─────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── 1. Cypher string escaping ─────────────────────────────────────────────

    #[test]
    fn test_cypher_str_escaping() {
        // Basic round-trip
        assert_eq!(falkor::cypher_str("hello"), "'hello'");
        // Single quote must be escaped
        assert!(falkor::cypher_str("it's").contains("\\'"));
        // Backslash must be doubled
        assert!(falkor::cypher_str("a\\b").contains("\\\\"));
        // Newline and tab
        assert!(falkor::cypher_str("a\nb").contains("\\n"));
        assert!(falkor::cypher_str("a\tb").contains("\\t"));
    }

    // ── 2. Cypher UNWIND nodes_array shape ───────────────────────────────────

    #[test]
    fn test_cypher_nodes_array_shape() {
        let mut nodes: NodeKindMap = BTreeMap::new();
        nodes.insert(
            "talk:foo".to_string(),
            serde_json::json!({"title": "Foo Talk", "order": 1}),
        );
        nodes.insert(
            "talk:bar".to_string(),
            serde_json::json!({"title": "Bar Talk", "order": 2}),
        );

        let s = falkor::nodes_array(&nodes);

        assert!(s.starts_with('['), "should start with [: {s}");
        assert!(s.ends_with(']'), "should end with ]: {s}");
        assert!(s.contains("nid:"), "should have nid key: {s}");
        assert!(s.contains("props:"), "should have props key: {s}");
        assert!(s.contains("'talk:foo'"), "first nid quoted: {s}");
        assert!(s.contains("'talk:bar'"), "second nid quoted: {s}");
        // BTreeMap → deterministic order: bar before foo
        assert!(
            s.find("'talk:bar'").unwrap() < s.find("'talk:foo'").unwrap(),
            "BTreeMap order: bar < foo alphabetically: {s}"
        );
    }

    // ── 3. Cypher edges_array shape ───────────────────────────────────────────

    #[test]
    fn test_cypher_edges_array_shape() {
        let mut edges: EdgeKindMap = BTreeMap::new();
        edges.insert(
            ("talk:foo".to_string(), "event:bar".to_string()),
            None,
        );

        let s = falkor::edges_array(&edges);

        assert!(s.starts_with('['), "should start with [: {s}");
        assert!(s.contains("from:"), "should have from key: {s}");
        assert!(s.contains("to:"), "should have to key: {s}");
        assert!(s.contains("'talk:foo'"), "from nid: {s}");
        assert!(s.contains("'event:bar'"), "to nid: {s}");
    }

    // ── 4. SurrealDB angle-bracket record ID ─────────────────────────────────

    #[test]
    fn test_surreal_id_angle_brackets() {
        let id = surreal_http::surreal_id("talk", "talk:some-event-123");
        // Format: table:⟨nid⟩
        assert!(id.starts_with("talk:"), "table prefix: {id}");
        assert!(
            id.contains('\u{27E8}'),
            "left mathematical angle bracket U+27E8: {id}"
        );
        assert!(
            id.contains('\u{27E9}'),
            "right mathematical angle bracket U+27E9: {id}"
        );
        assert_eq!(
            id,
            "talk:\u{27E8}talk:some-event-123\u{27E9}",
            "exact format"
        );
    }

    // ── 5. SurrealQL UPSERT statement shape ───────────────────────────────────

    #[test]
    fn test_surreal_upsert_shape() {
        let raw = serde_json::json!({"title": "My Talk", "abstract": "About AI", "order": 2});
        let content = node_props("Talk", "talk:my-talk", &raw);
        let id = surreal_http::surreal_id("talk", "talk:my-talk");
        let json = serde_json::to_string(&content).unwrap();
        let sql = format!("UPSERT {id} CONTENT {json};");

        assert!(sql.starts_with("UPSERT talk:"), "UPSERT keyword + table: {sql}");
        assert!(sql.contains("CONTENT"), "CONTENT keyword present: {sql}");
        assert!(sql.contains("\"nid\""), "nid field in content: {sql}");
        assert!(sql.contains("\"title\""), "title field in content: {sql}");
        assert!(sql.contains("\"abstract_text\""), "abstract_text field: {sql}");
        assert!(sql.contains("\"talk_order\""), "talk_order field: {sql}");
    }

    // ── 6. SurrealQL RELATE statement shape ───────────────────────────────────

    #[test]
    fn test_surreal_relate_shape() {
        let from = surreal_http::surreal_id("talk", "talk:foo");
        let to = surreal_http::surreal_id("event", "event:bar");
        let sql = format!("RELATE {from}->presented_at->{to};");

        assert!(sql.starts_with("RELATE talk:"), "starts with RELATE + from table: {sql}");
        assert!(sql.contains("->presented_at->"), "edge table in arrow syntax: {sql}");
        assert!(sql.contains("event:"), "to table present: {sql}");
        // Verify angle brackets survive intact
        assert!(sql.contains('\u{27E8}'), "angle bracket in from: {sql}");
        assert!(
            sql.matches('\u{27E9}').count() >= 2,
            "closing angle bracket for both sides: {sql}"
        );
    }

    // ── 7. node_props — Talk fields ───────────────────────────────────────────

    #[test]
    fn test_node_props_talk_fields() {
        let raw = serde_json::json!({
            "title": "Graph Databases",
            "abstract": "An intro to graphs",
            "order": 3,
        });
        let p = node_props("Talk", "talk:graph-dbs", &raw);

        assert_eq!(p["nid"], "talk:graph-dbs");
        assert_eq!(p["title"], "Graph Databases");
        assert_eq!(p["abstract_text"], "An intro to graphs"); // renamed
        assert_eq!(p["talk_order"], 3i64);
        assert!(p.get("abstract").is_none(), "raw 'abstract' key absent");
    }

    // ── 8. node_props — Speaker defaults on missing fields ───────────────────

    #[test]
    fn test_node_props_speaker_defaults() {
        // Completely empty props — all string fields should default to ""
        let raw = serde_json::json!({});
        let p = node_props("Speaker", "speaker:anon", &raw);

        assert_eq!(p["nid"], "speaker:anon");
        assert_eq!(p["name"], "");
        assert_eq!(p["bio"], "");
        assert_eq!(p["company"], "");
    }

    // ── 9. helix_base_url normalization ───────────────────────────────────────

    #[test]
    fn test_helix_base_url_normalization() {
        // Plain URL — untouched
        assert_eq!(helix_base_url("http://localhost:8080"), "http://localhost:8080");
        // Trailing slash stripped
        assert_eq!(helix_base_url("http://localhost:8080/"), "http://localhost:8080");
        // /v1/query suffix stripped
        assert_eq!(
            helix_base_url("http://localhost:8080/v1/query"),
            "http://localhost:8080"
        );
        // /v1/query/ (trailing slash) stripped
        assert_eq!(
            helix_base_url("http://localhost:8080/v1/query/"),
            "http://localhost:8080"
        );
        // Remote host with port
        assert_eq!(
            helix_base_url("https://cluster.helix-db.com:443/v1/query"),
            "https://cluster.helix-db.com:443"
        );
    }

    // ── 10. surreal_ws_address normalization ──────────────────────────────────

    #[test]
    fn test_surreal_ws_address_normalization() {
        // ws:// — unchanged
        assert_eq!(
            surreal_ws_address("ws://localhost:8000"),
            "ws://localhost:8000"
        );
        // http:// → ws://
        assert_eq!(
            surreal_ws_address("http://localhost:8000"),
            "ws://localhost:8000"
        );
        // Path stripped
        assert_eq!(
            surreal_ws_address("http://localhost:8000/rpc"),
            "ws://localhost:8000"
        );
        // https:// → wss://
        assert_eq!(
            surreal_ws_address("https://db.example.com"),
            "wss://db.example.com"
        );
        // wss:// with path
        assert_eq!(
            surreal_ws_address("wss://db.example.com:443/rpc"),
            "wss://db.example.com:443"
        );
    }

    // ── 11. HelixDB write_batch JSON shape ────────────────────────────────────

    #[test]
    fn test_helix_batch_json_shape() {
        use helix_db::{
            DynamicQueryRequest,
            dsl::prelude::{PropertyValue, g, write_batch},
        };

        let req = DynamicQueryRequest::write(
            write_batch()
                .var_as(
                    "n0",
                    g().add_n("Talk", vec![("nid", PropertyValue::from("talk:foo"))]),
                )
                .var_as(
                    "n1",
                    g().add_n("Talk", vec![("nid", PropertyValue::from("talk:bar"))]),
                )
                .returning(["n0", "n1"]),
        );

        let json = serde_json::to_string(&req).expect("serialize DynamicQueryRequest");

        assert!(json.contains("\"write\""), "request_type is write: {json}");
        assert!(json.contains("\"queries\""), "queries field present: {json}");
        assert!(json.contains("\"n0\""), "var n0 present: {json}");
        assert!(json.contains("\"n1\""), "var n1 present: {json}");
        assert!(json.contains("\"returns\""), "returns field present: {json}");
    }

    // ── 12. BTreeMap gives deterministic edge ordering ────────────────────────

    #[test]
    fn test_btreemap_deterministic_edge_order() {
        let mut edges: EdgeKindMap = BTreeMap::new();
        edges.insert(("talk:z".to_string(), "event:z".to_string()), None);
        edges.insert(("talk:a".to_string(), "event:a".to_string()), None);
        edges.insert(("talk:m".to_string(), "event:m".to_string()), None);

        let keys: Vec<_> = edges.keys().collect();
        // BTreeMap iterates in sorted order
        assert_eq!(keys[0].0, "talk:a");
        assert_eq!(keys[1].0, "talk:m");
        assert_eq!(keys[2].0, "talk:z");
    }
}
