//! Loads scraped talk records from `data/talks/*.json` into a FalkorDB graph.
//!
//! Each TalkRecord has a `nodes` and `edges` array. Across the 163 records
//! the same Speaker/Event/Group nodes (and their edges) are repeated many times,
//! so we deduplicate first and then load each kind in one `UNWIND … MERGE` batch.
//! Re-running is idempotent.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use serde::Deserialize;
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(about = "Load scraped talk JSON files into a FalkorDB graph")]
struct Args {
    /// Directory containing talk JSON files
    #[arg(short, long, default_value = "data/talks")]
    input: PathBuf,

    /// FalkorDB / Redis URL
    #[arg(long, env = "FALKORDB_URL", default_value = "redis://localhost:6379")]
    url: String,

    /// Graph name in FalkorDB
    #[arg(short, long, default_value = "bythebay")]
    graph: String,

    /// Drop the graph before loading (else MERGE is idempotent)
    #[arg(long)]
    clear: bool,
}

// ── on-disk shape (mirrors meetup_scraper::models::TalkRecord) ───────────────

#[derive(Debug, Deserialize)]
struct TalkRecord {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}

#[derive(Debug, Deserialize)]
struct Node {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    properties: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct Edge {
    from: String,
    to: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    properties: Option<serde_json::Value>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("falkor_load=info".parse()?),
        )
        .init();

    let args = Args::parse();

    let (nodes_by_kind, edges_by_kind) = collect(&args.input)?;
    let n_nodes: usize = nodes_by_kind.values().map(|v| v.len()).sum();
    let n_edges: usize = edges_by_kind.values().map(|v| v.len()).sum();
    info!(
        "collected {} unique nodes ({} kinds), {} unique edges ({} kinds)",
        n_nodes,
        nodes_by_kind.len(),
        n_edges,
        edges_by_kind.len(),
    );

    let client = redis::Client::open(args.url.as_str()).context("redis client")?;
    let mut conn = client.get_connection().context("redis connection")?;

    if args.clear {
        info!("dropping graph '{}'", args.graph);
        // GRAPH.DELETE returns an error if the graph doesn't exist — ignore it
        let _: redis::RedisResult<redis::Value> =
            redis::cmd("GRAPH.DELETE").arg(&args.graph).query(&mut conn);
    }

    // Indexes on the merge key speed every subsequent MERGE call.
    for kind in nodes_by_kind.keys() {
        let q = format!("CREATE INDEX FOR (n:{kind}) ON (n.nid)");
        // Index creation fails harmlessly if it already exists
        let _ = run_query(&mut conn, &args.graph, &q);
    }

    // Load nodes by kind
    for (kind, nodes) in &nodes_by_kind {
        load_nodes(&mut conn, &args.graph, kind, nodes)?;
    }

    // Load edges by kind
    for (kind, edges) in &edges_by_kind {
        load_edges(&mut conn, &args.graph, kind, edges)?;
    }

    info!("done");
    Ok(())
}

// ── disk → in-memory dedup ────────────────────────────────────────────────────

type NodesByKind = HashMap<String, HashMap<String, serde_json::Value>>;
type EdgesByKind = HashMap<String, HashMap<(String, String), Option<serde_json::Value>>>;

fn collect(dir: &std::path::Path) -> Result<(NodesByKind, EdgesByKind)> {
    let mut nodes: NodesByKind = HashMap::new();
    let mut edges: EdgesByKind = HashMap::new();

    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("read_dir {}", dir.display()))?;
    let mut file_count = 0usize;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        file_count += 1;
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        let rec: TalkRecord = match serde_json::from_str(&raw) {
            Ok(r) => r,
            Err(e) => {
                warn!("skip {}: {e}", path.display());
                continue;
            }
        };

        for node in rec.nodes {
            nodes
                .entry(node.kind)
                .or_default()
                .insert(node.id, node.properties);
        }
        for edge in rec.edges {
            edges
                .entry(edge.kind)
                .or_default()
                .insert((edge.from, edge.to), edge.properties);
        }
    }

    info!("read {file_count} talk files from {}", dir.display());
    Ok((nodes, edges))
}

// ── batched loaders ───────────────────────────────────────────────────────────

fn load_nodes(
    conn: &mut redis::Connection,
    graph: &str,
    kind: &str,
    nodes: &HashMap<String, serde_json::Value>,
) -> Result<()> {
    if !is_safe_label(kind) {
        bail!("unsafe label for Cypher: {kind}");
    }
    if nodes.is_empty() {
        return Ok(());
    }

    let array_lit = nodes_array_literal(nodes);
    let q = format!(
        "UNWIND {array_lit} AS row \
         MERGE (n:{kind} {{nid: row.nid}}) \
         SET n += row.props"
    );
    let stats = run_query(conn, graph, &q)
        .with_context(|| format!("MERGE {kind} nodes"))?;
    info!("  {kind:>8}: {} nodes  ({stats})", nodes.len());
    Ok(())
}

fn load_edges(
    conn: &mut redis::Connection,
    graph: &str,
    kind: &str,
    edges: &HashMap<(String, String), Option<serde_json::Value>>,
) -> Result<()> {
    if !is_safe_label(kind) {
        bail!("unsafe relationship type for Cypher: {kind}");
    }
    if edges.is_empty() {
        return Ok(());
    }

    let array_lit = edges_array_literal(edges);
    let q = format!(
        "UNWIND {array_lit} AS row \
         MATCH (a {{nid: row.from}}), (b {{nid: row.to}}) \
         MERGE (a)-[r:{kind}]->(b) \
         SET r += row.props"
    );
    let stats = run_query(conn, graph, &q)
        .with_context(|| format!("MERGE {kind} edges"))?;
    info!("  {kind:>14}: {} edges  ({stats})", edges.len());
    Ok(())
}

fn run_query(conn: &mut redis::Connection, graph: &str, query: &str) -> Result<String> {
    let value: redis::Value = redis::cmd("GRAPH.QUERY")
        .arg(graph)
        .arg(query)
        .query(conn)
        .context("GRAPH.QUERY")?;
    Ok(summarize_stats(&value))
}

/// FalkorDB returns a 3-element array: [header, results, statistics-strings].
/// The third element is a list of strings like "Nodes created: 12".
fn summarize_stats(v: &redis::Value) -> String {
    let redis::Value::Array(items) = v else { return String::new() };
    let stats_idx = items.len().saturating_sub(1);
    let redis::Value::Array(stats) = &items[stats_idx] else { return String::new() };

    stats
        .iter()
        .filter_map(|s| match s {
            redis::Value::SimpleString(s) => Some(s.clone()),
            redis::Value::BulkString(b) => std::str::from_utf8(b).ok().map(str::to_string),
            _ => None,
        })
        .filter(|line| {
            let l = line.to_lowercase();
            l.contains("nodes created")
                || l.contains("relationships created")
                || l.contains("properties set")
                || l.contains("nodes deleted")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

// ── Cypher literal building (with escaping) ──────────────────────────────────

fn nodes_array_literal(nodes: &HashMap<String, serde_json::Value>) -> String {
    let items: Vec<String> = nodes
        .iter()
        .map(|(nid, props)| {
            format!(
                "{{nid: {}, props: {}}}",
                cypher_string(nid),
                json_to_cypher(props)
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

fn edges_array_literal(
    edges: &HashMap<(String, String), Option<serde_json::Value>>,
) -> String {
    let empty = serde_json::Value::Object(Default::default());
    let items: Vec<String> = edges
        .iter()
        .map(|((from, to), props)| {
            let p = props.as_ref().unwrap_or(&empty);
            format!(
                "{{from: {}, to: {}, props: {}}}",
                cypher_string(from),
                cypher_string(to),
                json_to_cypher(p)
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

fn json_to_cypher(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => cypher_string(s),
        serde_json::Value::Array(a) => {
            let items: Vec<String> = a.iter().map(json_to_cypher).collect();
            format!("[{}]", items.join(","))
        }
        serde_json::Value::Object(o) => {
            let pairs: Vec<String> = o
                .iter()
                .map(|(k, v)| format!("{}: {}", cypher_key(k), json_to_cypher(v)))
                .collect();
            format!("{{{}}}", pairs.join(","))
        }
    }
}

/// Quote and escape a Cypher string literal. Newlines and control characters
/// are escaped; single quotes and backslashes are doubled.
fn cypher_string(s: &str) -> String {
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

/// Cypher map keys must be valid identifiers; non-identifier keys are backticked.
fn cypher_key(k: &str) -> String {
    let bytes = k.as_bytes();
    let safe = !bytes.is_empty()
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b == b'_');
    if safe {
        k.to_string()
    } else {
        format!("`{}`", k.replace('`', ""))
    }
}

fn is_safe_label(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().map_or(false, |c| c.is_ascii_alphabetic())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}
