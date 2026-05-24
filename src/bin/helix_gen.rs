//! HelixDB Enterprise query generator for the ByTheBay graph schema.
//!
//! This binary uses the `helix-enterprise-ql` Rust DSL to define every
//! graph query as a typed Rust function, then serialises them into the
//! `queries.json` bundle that HelixDB Enterprise consumes.
//!
//! # Graph schema
//!
//! Nodes:  Talk · Event · Group · Speaker
//! Edges:  PRESENTED_AT (Talk→Event) · PRESENTED_BY (Talk→Speaker)
//!         PART_OF (Event→Group)
//!
//! # Workflow
//!
//! ```text
//! # 1. Generate the bundle
//! cargo run --bin helix-gen [-- --output helix/queries.json]
//!
//! # 2. Start a local HelixDB Enterprise instance (Docker)
//! #    Mount queries.json as PATH_TO_QUERIES — see docs.helix-db.com/enterprise/local-development
//! docker run -p 6969:6969 \
//!   -e PATH_TO_QUERIES=/queries.json \
//!   -v $(pwd)/helix/queries.json:/queries.json \
//!   helixdb/enterprise-dev
//!
//! # 3. Load the scraped data
//! cargo run --bin helix-load [-- --url http://localhost:6969 --input data/talks]
//! ```

use helix_dsl::prelude::*;

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(about = "Generate HelixDB queries.json from the Rust DSL")]
struct Args {
    /// Where to write queries.json
    #[arg(short, long, default_value = "helix/queries.json")]
    output: PathBuf,
}

// ── Node-creation queries ─────────────────────────────────────────────────────

/// Create a Talk node.
/// HTTP: POST /v1/query/add_talk  {"nid":"…","title":"…","abstract_text":"…","talk_order":0}
#[register]
pub fn add_talk(
    nid: String,
    title: String,
    abstract_text: String,
    talk_order: i64,
) -> WriteBatch {
    let _ = (&nid, &title, &abstract_text, &talk_order);
    write_batch()
        .var_as(
            "talk",
            g().add_n(
                "Talk",
                vec![
                    ("nid",           PropertyInput::param("nid")),
                    ("title",         PropertyInput::param("title")),
                    ("abstract_text", PropertyInput::param("abstract_text")),
                    ("talk_order",    PropertyInput::param("talk_order")),
                ],
            )
            .project(vec![PropertyProjection::renamed("$id", "id")]),
        )
        .returning(["talk"])
}

/// Create an Event node.
/// HTTP: POST /v1/query/add_event  {"nid":"…","event_id":"…","title":"…",…}
#[register]
pub fn add_event(
    nid: String,
    event_id: String,
    title: String,
    date: String,
    datetime: String,
    url: String,
    venue_name: String,
    venue_address: String,
    city: String,
) -> WriteBatch {
    let _ = (&nid, &event_id, &title, &date, &datetime, &url,
             &venue_name, &venue_address, &city);
    write_batch()
        .var_as(
            "event",
            g().add_n(
                "Event",
                vec![
                    ("nid",           PropertyInput::param("nid")),
                    ("event_id",      PropertyInput::param("event_id")),
                    ("title",         PropertyInput::param("title")),
                    ("date",          PropertyInput::param("date")),
                    ("datetime",      PropertyInput::param("datetime")),
                    ("url",           PropertyInput::param("url")),
                    ("venue_name",    PropertyInput::param("venue_name")),
                    ("venue_address", PropertyInput::param("venue_address")),
                    ("city",          PropertyInput::param("city")),
                ],
            )
            .project(vec![PropertyProjection::renamed("$id", "id")]),
        )
        .returning(["event"])
}

/// Create a Group node.
/// HTTP: POST /v1/query/add_group  {"nid":"…","slug":"…","name":"…","url":"…"}
#[register]
pub fn add_group(
    nid: String,
    slug: String,
    name: String,
    url: String,
) -> WriteBatch {
    let _ = (&nid, &slug, &name, &url);
    write_batch()
        .var_as(
            "grp",
            g().add_n(
                "Group",
                vec![
                    ("nid",  PropertyInput::param("nid")),
                    ("slug", PropertyInput::param("slug")),
                    ("name", PropertyInput::param("name")),
                    ("url",  PropertyInput::param("url")),
                ],
            )
            .project(vec![PropertyProjection::renamed("$id", "id")]),
        )
        .returning(["grp"])
}

/// Create a Speaker node.
/// HTTP: POST /v1/query/add_speaker  {"nid":"…","name":"…","bio":"…","company":"…"}
#[register]
pub fn add_speaker(
    nid: String,
    name: String,
    bio: String,
    company: String,
) -> WriteBatch {
    let _ = (&nid, &name, &bio, &company);
    write_batch()
        .var_as(
            "speaker",
            g().add_n(
                "Speaker",
                vec![
                    ("nid",     PropertyInput::param("nid")),
                    ("name",    PropertyInput::param("name")),
                    ("bio",     PropertyInput::param("bio")),
                    ("company", PropertyInput::param("company")),
                ],
            )
            .project(vec![PropertyProjection::renamed("$id", "id")]),
        )
        .returning(["speaker"])
}

// ── Edge-creation queries ─────────────────────────────────────────────────────
//
// Each edge query looks up both endpoint nodes by the `nid` property
// (a stable string identifier we store on every node), then creates a
// directed edge between them.

/// Talk -[PRESENTED_AT]-> Event
/// HTTP: POST /v1/query/add_presented_at  {"talk_nid":"…","event_nid":"…"}
#[register]
pub fn add_presented_at(talk_nid: String, event_nid: String) -> WriteBatch {
    let _ = (&talk_nid, &event_nid);
    write_batch()
        .var_as(
            "talk",
            g().n_with_label("Talk")
                .where_(Predicate::eq_param("nid", "talk_nid"))
                .limit(1),
        )
        .var_as(
            "event",
            g().n_with_label("Event")
                .where_(Predicate::eq_param("nid", "event_nid"))
                .limit(1),
        )
        .var_as(
            "edge",
            g().n(NodeRef::var("talk"))
                .add_e("PRESENTED_AT", NodeRef::var("event"), Vec::<(&str, &str)>::new())
                .count(),
        )
        .returning(["edge"])
}

/// Talk -[PRESENTED_BY]-> Speaker
/// HTTP: POST /v1/query/add_presented_by  {"talk_nid":"…","speaker_nid":"…"}
#[register]
pub fn add_presented_by(talk_nid: String, speaker_nid: String) -> WriteBatch {
    let _ = (&talk_nid, &speaker_nid);
    write_batch()
        .var_as(
            "talk",
            g().n_with_label("Talk")
                .where_(Predicate::eq_param("nid", "talk_nid"))
                .limit(1),
        )
        .var_as(
            "speaker",
            g().n_with_label("Speaker")
                .where_(Predicate::eq_param("nid", "speaker_nid"))
                .limit(1),
        )
        .var_as(
            "edge",
            g().n(NodeRef::var("talk"))
                .add_e("PRESENTED_BY", NodeRef::var("speaker"), Vec::<(&str, &str)>::new())
                .count(),
        )
        .returning(["edge"])
}

/// Event -[PART_OF]-> Group
/// HTTP: POST /v1/query/add_part_of  {"event_nid":"…","group_nid":"…"}
#[register]
pub fn add_part_of(event_nid: String, group_nid: String) -> WriteBatch {
    let _ = (&event_nid, &group_nid);
    write_batch()
        .var_as(
            "event",
            g().n_with_label("Event")
                .where_(Predicate::eq_param("nid", "event_nid"))
                .limit(1),
        )
        .var_as(
            "grp",
            g().n_with_label("Group")
                .where_(Predicate::eq_param("nid", "group_nid"))
                .limit(1),
        )
        .var_as(
            "edge",
            g().n(NodeRef::var("event"))
                .add_e("PART_OF", NodeRef::var("grp"), Vec::<(&str, &str)>::new())
                .count(),
        )
        .returning(["edge"])
}

// ── Utility queries ───────────────────────────────────────────────────────────

/// Delete all nodes of every type (and their incident edges).
/// Used by helix-load --clear before a fresh bulk load.
/// HTTP: POST /v1/query/clear_all  {}
#[register]
pub fn clear_all() -> WriteBatch {
    write_batch()
        .var_as("talks",    g().n_with_label("Talk").drop())
        .var_as("events",   g().n_with_label("Event").drop())
        .var_as("groups",   g().n_with_label("Group").drop())
        .var_as("speakers", g().n_with_label("Speaker").drop())
        .returning(["talks", "events", "groups", "speakers"])
}

/// Count nodes by label — handy for post-load verification.
/// HTTP: POST /v1/query/node_counts  {}
#[register]
pub fn node_counts() -> ReadBatch {
    read_batch()
        .var_as("talks",    g().n_with_label("Talk").count())
        .var_as("events",   g().n_with_label("Event").count())
        .var_as("groups",   g().n_with_label("Group").count())
        .var_as("speakers", g().n_with_label("Speaker").count())
        .returning(["talks", "events", "groups", "speakers"])
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Create output directory if needed
    if let Some(parent) = args.output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let path = helix_dsl::generate_to_path(&args.output)
        .map_err(|e| anyhow::anyhow!("generate_to_path failed: {e:?}"))?;

    println!("Generated {}", path.display());
    println!();
    println!("Registered queries:");
    println!("  Write  add_talk, add_event, add_group, add_speaker");
    println!("  Write  add_presented_at, add_presented_by, add_part_of");
    println!("  Write  clear_all");
    println!("  Read   node_counts");
    println!();
    println!("Next steps:");
    println!("  1. Start HelixDB Enterprise with this bundle (see helix/ README)");
    println!("  2. cargo run --bin helix-load -- --url http://localhost:6969 --input data/talks");
    Ok(())
}
