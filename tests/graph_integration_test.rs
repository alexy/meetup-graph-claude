//! Integration tests for the meetup graph backends.
//!
//! Each test verifies that scraped meetup data was loaded correctly into the
//! graph store: node and edge counts, speakers who present at multiple meetups,
//! events that have multiple co-presenting speakers, and the talk→event→group
//! path that lets a speaker "join" a group by giving a talk at one of its events.
//!
//! Tests skip gracefully when a backend is not reachable.  Load data first:
//!
//! ```text
//! cargo run --bin load -- --backend surreal-http --clear                             # SurrealDB (:8000)
//! cargo run --bin load -- --backend falkor --clear                                   # FalkorDB  (:6379)
//! cargo run --bin load -- --backend helix-sdk --url http://localhost:8080 --clear   # HelixDB   (:8080)
//! ```
//!
//! Expected totals after a complete load of `data/talks/` (163 files):
//!   Talks 163 · Events 124 · Groups 7 · Speakers 181
//!   PRESENTED_AT 163 · PRESENTED_BY 215 · PART_OF 124

use std::collections::HashSet;

// ══════════════════════════════════════════════════════════ Expected constants ═

const EXPECTED_TALKS: usize = 163;
const EXPECTED_EVENTS: usize = 124;
const EXPECTED_GROUPS: usize = 7;
const EXPECTED_SPEAKERS: usize = 181;

const EXPECTED_PRESENTED_AT: usize = 163;
const EXPECTED_PRESENTED_BY: usize = 215;
const EXPECTED_PART_OF: usize = 124;

// Speaker with the most talks in the dataset.
const STEFAN_WEBB: &str = "speaker:stefan-webb";
const STEFAN_WEBB_TALK_COUNT: usize = 4;

// An event where 5 speakers co-presented (Gemma 4 SF Edition, 2026-04-18).
const GEMMA4_EVENT: &str = "event:bay-area-ai-314321640";
const GEMMA4_SPEAKERS: &[&str] = &[
    "speaker:cormac-brick",
    "speaker:fereshteh-mahvar",
    "speaker:henry-ndubuaku",
    "speaker:olivier-lacombe",
    "speaker:sam-herring",
];

// Speaker who gave 3 talks, all inside the same group (Scala Bay).
const ADAM_WARSKI: &str = "speaker:adam-warski";
const ADAM_WARSKI_TALK_COUNT: usize = 3;
const ADAM_WARSKI_GROUP: &str = "group:scala-bay";

// ══════════════════════════════════════════════════════════ SurrealDB helpers ═

mod surreal {
    use std::collections::HashSet;
    use std::time::Duration;

    pub const URL: &str = "http://localhost:8000";
    const NS: &str = "meetup";
    const DB: &str = "graph";

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap()
    }

    pub async fn is_available() -> bool {
        client()
            .get(format!("{URL}/health"))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    /// POST a SurrealQL body and return the per-statement result array.
    pub async fn sql(query: &str) -> Vec<serde_json::Value> {
        client()
            .post(format!("{URL}/sql"))
            .header("Surreal-Ns", NS)
            .header("Surreal-Db", DB)
            .basic_auth("root", Some("root"))
            .header("Content-Type", "text/plain")
            .body(query.to_string())
            .send()
            .await
            .expect("POST /sql failed")
            .json::<Vec<serde_json::Value>>()
            .await
            .expect("parse /sql JSON failed")
    }

    /// `SELECT count() FROM <table> GROUP ALL` → row count.
    pub async fn count_table(table: &str) -> usize {
        let resp = sql(&format!("SELECT count() FROM {table} GROUP ALL;")).await;
        resp[0]["result"][0]["count"].as_u64().unwrap_or(0) as usize
    }

    /// SurrealDB angle-bracket record ID for a node whose `nid` is `nid`.
    /// E.g. `sid("speaker", "speaker:stefan-webb")` → `speaker:⟨speaker:stefan-webb⟩`
    pub fn sid(table: &str, nid: &str) -> String {
        format!("{table}:\u{27E8}{nid}\u{27E9}")
    }

    /// nids of talks delivered by `speaker_nid` (via the `presented_by` edge table).
    pub async fn talks_by_speaker(speaker_nid: &str) -> HashSet<String> {
        let q = format!(
            "SELECT in.nid AS nid FROM presented_by WHERE out = {};",
            sid("speaker", speaker_nid)
        );
        sql(&q).await[0]["result"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|r| r["nid"].as_str().map(str::to_string))
            .collect()
    }

    /// nids of speakers who co-presented at `event_nid`.
    pub async fn speakers_at_event(event_nid: &str) -> HashSet<String> {
        let q = format!(
            "SELECT out.nid AS nid FROM presented_by \
             WHERE in IN (SELECT VALUE in FROM presented_at WHERE out = {});",
            sid("event", event_nid)
        );
        sql(&q).await[0]["result"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|r| r["nid"].as_str().map(str::to_string))
            .collect()
    }

    /// Distinct group nids a speaker has been associated with through their
    /// talks (talk → PRESENTED_AT → event → PART_OF → group).
    pub async fn groups_of_speaker(speaker_nid: &str) -> HashSet<String> {
        let q = format!(
            "SELECT out.nid AS nid FROM part_of \
             WHERE in IN \
               (SELECT VALUE out FROM presented_at \
                WHERE in IN \
                  (SELECT VALUE in FROM presented_by WHERE out = {}));",
            sid("speaker", speaker_nid)
        );
        // HashSet deduplicates the results automatically.
        sql(&q).await[0]["result"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|r| r["nid"].as_str().map(str::to_string))
            .collect()
    }

    /// Number of records in an edge table (presented_at / presented_by / part_of).
    pub async fn count_edge_table(table: &str) -> usize {
        let resp = sql(&format!("SELECT count() FROM {table} GROUP ALL;")).await;
        resp[0]["result"][0]["count"].as_u64().unwrap_or(0) as usize
    }
}

// ══════════════════════════════════════════════════════════ FalkorDB helpers ═

mod falkor {
    use std::collections::HashSet;

    pub const REDIS_URL: &str = "redis://localhost:6379";
    pub const GRAPH: &str = "bythebay";

    pub fn is_available() -> bool {
        redis::Client::open(REDIS_URL)
            .and_then(|c| c.get_connection())
            .is_ok()
    }

    fn graph_query(cypher: &str) -> redis::Value {
        let mut conn = redis::Client::open(REDIS_URL)
            .unwrap()
            .get_connection()
            .unwrap();
        redis::cmd("GRAPH.QUERY")
            .arg(GRAPH)
            .arg(cypher)
            .query(&mut conn)
            .expect("GRAPH.QUERY failed")
    }

    /// Extract a scalar integer from GRAPH.QUERY result (first cell, first row).
    ///
    /// FalkorDB wraps values as `[type_code, value]` pairs; for integers the
    /// value may arrive as `Int` or inside a nested array.
    fn scalar_int(val: &redis::Value) -> usize {
        let redis::Value::Array(outer) = val else { return 0 };
        // outer[1] = rows array
        let rows = match outer.get(1) {
            Some(redis::Value::Array(r)) => r,
            _ => return 0,
        };
        let row = match rows.first() {
            Some(redis::Value::Array(r)) => r,
            _ => return 0,
        };
        // Cell may be [type, value] or a bare integer.
        let cell = row.first().unwrap_or(&redis::Value::Nil);
        match cell {
            redis::Value::Int(n) => *n as usize,
            redis::Value::Array(pair) => match pair.get(1) {
                Some(redis::Value::Int(n)) => *n as usize,
                _ => 0,
            },
            _ => 0,
        }
    }

    /// Extract a set of strings from column 0 of all rows.
    fn string_column(val: &redis::Value) -> HashSet<String> {
        let mut out = HashSet::new();
        let redis::Value::Array(outer) = val else { return out };
        let rows = match outer.get(1) {
            Some(redis::Value::Array(r)) => r,
            _ => return out,
        };
        for row in rows {
            let cells = match row {
                redis::Value::Array(c) => c,
                _ => continue,
            };
            // Cell may be bare BulkString/SimpleString or [type, value] pair.
            let cell = cells.first().unwrap_or(&redis::Value::Nil);
            let s = match cell {
                redis::Value::BulkString(b) => std::str::from_utf8(b).ok().map(str::to_string),
                redis::Value::SimpleString(s) => Some(s.clone()),
                redis::Value::Array(pair) => match pair.get(1) {
                    Some(redis::Value::BulkString(b)) => {
                        std::str::from_utf8(b).ok().map(str::to_string)
                    }
                    Some(redis::Value::SimpleString(s)) => Some(s.clone()),
                    _ => None,
                },
                _ => None,
            };
            if let Some(v) = s {
                out.insert(v);
            }
        }
        out
    }

    pub fn count_label(label: &str) -> usize {
        scalar_int(&graph_query(&format!(
            "MATCH (n:{label}) RETURN count(n)"
        )))
    }

    pub fn count_rel(rel_type: &str) -> usize {
        scalar_int(&graph_query(&format!(
            "MATCH ()-[r:{rel_type}]->() RETURN count(r)"
        )))
    }

    pub fn talks_by_speaker(speaker_nid: &str) -> HashSet<String> {
        string_column(&graph_query(&format!(
            "MATCH (s:Speaker {{nid:'{speaker_nid}'}})<-[:PRESENTED_BY]-(t:Talk) \
             RETURN t.nid"
        )))
    }

    pub fn speakers_at_event(event_nid: &str) -> HashSet<String> {
        string_column(&graph_query(&format!(
            "MATCH (e:Event {{nid:'{event_nid}'}})<-[:PRESENTED_AT]-(t:Talk)\
             -[:PRESENTED_BY]->(s:Speaker) \
             RETURN s.nid"
        )))
    }

    pub fn groups_of_speaker(speaker_nid: &str) -> HashSet<String> {
        string_column(&graph_query(&format!(
            "MATCH (sp:Speaker {{nid:'{speaker_nid}'}})<-[:PRESENTED_BY]-(:Talk)\
             -[:PRESENTED_AT]->(:Event)-[:PART_OF]->(g:Group) \
             RETURN DISTINCT g.nid"
        )))
    }
}

// ══════════════════════════════════════════════════════════ HelixDB helpers ═

mod helix {
    use std::collections::HashSet;
    use std::time::Duration;

    use helix_db::{
        Client,
        dsl::prelude::{DynamicQueryRequest, NodeRef, SourcePredicate, g, read_batch},
    };

    pub const URL: &str = "http://localhost:8080";

    pub async fn is_available() -> bool {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap()
            .get(format!("{URL}/health"))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    async fn sdk_read(req: DynamicQueryRequest) -> serde_json::Value {
        Client::new(Some(URL))
            .expect("helix Client::new failed")
            .with_api_key(None)
            .query::<serde_json::Value>()
            .dynamic_query(req)
            .send()
            .await
            .expect("helix sdk read failed")
    }

    /// Returns (talks, events, groups, speakers) counts.
    pub async fn node_counts() -> (usize, usize, usize, usize) {
        let v = sdk_read(DynamicQueryRequest::read(
            read_batch()
                .var_as("talks",    g().n_with_label("Talk").count())
                .var_as("events",   g().n_with_label("Event").count())
                .var_as("groups",   g().n_with_label("Group").count())
                .var_as("speakers", g().n_with_label("Speaker").count())
                .returning(["talks", "events", "groups", "speakers"]),
        ))
        .await;
        let n = |key: &str| v[key]["count"].as_u64().unwrap_or(0) as usize;
        (n("talks"), n("events"), n("groups"), n("speakers"))
    }

    /// nids of talks delivered by `speaker_nid`.
    /// Edge direction: Talk -[PRESENTED_BY]-> Speaker
    /// So from Speaker, we traverse `in_(PRESENTED_BY)` to reach talks.
    pub async fn talks_by_speaker(speaker_nid: &str) -> HashSet<String> {
        let v = sdk_read(DynamicQueryRequest::read(
            read_batch()
                .var_as(
                    "speaker",
                    g().n_with_label_where("Speaker", SourcePredicate::eq("nid", speaker_nid)),
                )
                .var_as(
                    "talks",
                    g().n(NodeRef::var("speaker"))
                        .in_(Some("PRESENTED_BY"))
                        .value_map(Some(vec!["nid"])),
                )
                .returning(["talks"]),
        ))
        .await;
        v["talks"]["properties"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|r| r["nid"].as_str().map(str::to_string))
            .collect()
    }

    /// nids of speakers who co-presented at `event_nid`.
    /// Path: Event <-[PRESENTED_AT]- Talk -[PRESENTED_BY]-> Speaker
    pub async fn speakers_at_event(event_nid: &str) -> HashSet<String> {
        let v = sdk_read(DynamicQueryRequest::read(
            read_batch()
                .var_as(
                    "event",
                    g().n_with_label_where("Event", SourcePredicate::eq("nid", event_nid)),
                )
                .var_as(
                    "speakers",
                    g().n(NodeRef::var("event"))
                        .in_(Some("PRESENTED_AT"))
                        .out(Some("PRESENTED_BY"))
                        .dedup()
                        .value_map(Some(vec!["nid"])),
                )
                .returning(["speakers"]),
        ))
        .await;
        v["speakers"]["properties"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|r| r["nid"].as_str().map(str::to_string))
            .collect()
    }

    /// Distinct group nids a speaker has been associated with through their talks.
    /// Path: Speaker <-[PRESENTED_BY]- Talk -[PRESENTED_AT]-> Event -[PART_OF]-> Group
    pub async fn groups_of_speaker(speaker_nid: &str) -> HashSet<String> {
        let v = sdk_read(DynamicQueryRequest::read(
            read_batch()
                .var_as(
                    "speaker",
                    g().n_with_label_where("Speaker", SourcePredicate::eq("nid", speaker_nid)),
                )
                .var_as(
                    "groups",
                    g().n(NodeRef::var("speaker"))
                        .in_(Some("PRESENTED_BY"))
                        .out(Some("PRESENTED_AT"))
                        .out(Some("PART_OF"))
                        .dedup()
                        .value_map(Some(vec!["nid"])),
                )
                .returning(["groups"]),
        ))
        .await;
        v["groups"]["properties"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|r| r["nid"].as_str().map(str::to_string))
            .collect()
    }
}

// ══════════════════════════════════════════════════════════ SurrealDB tests ════

/// All four node labels must match the exact file count.
#[tokio::test]
async fn surreal_node_counts() {
    if !surreal::is_available().await {
        eprintln!("surreal not available — skipping");
        return;
    }
    assert_eq!(surreal::count_table("talk").await,    EXPECTED_TALKS,    "talk count");
    assert_eq!(surreal::count_table("event").await,   EXPECTED_EVENTS,   "event count");
    assert_eq!(surreal::count_table("group").await,   EXPECTED_GROUPS,   "group count");
    assert_eq!(surreal::count_table("speaker").await, EXPECTED_SPEAKERS, "speaker count");
}

/// Edge tables must contain exactly the deduplicated edge count.
#[tokio::test]
async fn surreal_edge_counts() {
    if !surreal::is_available().await {
        eprintln!("surreal not available — skipping");
        return;
    }
    assert_eq!(
        surreal::count_edge_table("presented_at").await,
        EXPECTED_PRESENTED_AT, "presented_at count"
    );
    assert_eq!(
        surreal::count_edge_table("presented_by").await,
        EXPECTED_PRESENTED_BY, "presented_by count"
    );
    assert_eq!(
        surreal::count_edge_table("part_of").await,
        EXPECTED_PART_OF, "part_of count"
    );
}

/// A speaker who gave multiple talks appears the correct number of times in
/// the `presented_by` edge table, one edge per talk.
#[tokio::test]
async fn surreal_speaker_gives_talks_at_multiple_meetups() {
    if !surreal::is_available().await {
        eprintln!("surreal not available — skipping");
        return;
    }
    let talks = surreal::talks_by_speaker(STEFAN_WEBB).await;
    assert_eq!(
        talks.len(),
        STEFAN_WEBB_TALK_COUNT,
        "{STEFAN_WEBB} should have {STEFAN_WEBB_TALK_COUNT} talks, got {talks:?}"
    );
}

/// Every talk delivered by a speaker must also be linked to an event.
#[tokio::test]
async fn surreal_each_talk_has_exactly_one_event() {
    if !surreal::is_available().await {
        eprintln!("surreal not available — skipping");
        return;
    }
    let talks = surreal::talks_by_speaker(STEFAN_WEBB).await;
    for talk_nid in &talks {
        let q = format!(
            "SELECT count() FROM presented_at WHERE in.nid = '{talk_nid}' GROUP ALL;"
        );
        let resp = surreal::sql(&q).await;
        let cnt = resp[0]["result"][0]["count"].as_u64().unwrap_or(0);
        assert_eq!(cnt, 1, "talk {talk_nid} should have exactly one PRESENTED_AT edge");
    }
}

/// An event where multiple speakers co-presented must list all of them.
#[tokio::test]
async fn surreal_event_has_multiple_speakers() {
    if !surreal::is_available().await {
        eprintln!("surreal not available — skipping");
        return;
    }
    let got = surreal::speakers_at_event(GEMMA4_EVENT).await;
    let expected: HashSet<&str> = GEMMA4_SPEAKERS.iter().copied().collect();
    let got_refs: HashSet<&str> = got.iter().map(|s| s.as_str()).collect();
    assert_eq!(
        got_refs, expected,
        "Gemma 4 event speaker set mismatch"
    );
}

/// A speaker joins a group by presenting a talk at one of that group's events.
/// Adam Warski gave three talks, all at Scala Bay meetups → one unique group.
#[tokio::test]
async fn surreal_speaker_joins_group_through_talk() {
    if !surreal::is_available().await {
        eprintln!("surreal not available — skipping");
        return;
    }
    let groups = surreal::groups_of_speaker(ADAM_WARSKI).await; // HashSet → deduped
    assert_eq!(
        groups.len(), 1,
        "{ADAM_WARSKI} gave {ADAM_WARSKI_TALK_COUNT} talks but they all belong to \
         one group; got {groups:?}"
    );
    assert!(
        groups.contains(ADAM_WARSKI_GROUP),
        "{ADAM_WARSKI} should be in {ADAM_WARSKI_GROUP}, got {groups:?}"
    );
}

/// A speaker who gave talks at multiple meetups can be looked up by name.
#[tokio::test]
async fn surreal_speaker_lookup_by_nid() {
    if !surreal::is_available().await {
        eprintln!("surreal not available — skipping");
        return;
    }
    let q = format!(
        "SELECT nid, name FROM speaker WHERE nid = '{}';",
        STEFAN_WEBB
    );
    let resp = surreal::sql(&q).await;
    let result = &resp[0]["result"];
    assert!(result.is_array() && !result.as_array().unwrap().is_empty(),
        "speaker {STEFAN_WEBB} not found");
    assert_eq!(result[0]["nid"].as_str().unwrap_or(""), STEFAN_WEBB);
    assert_eq!(result[0]["name"].as_str().unwrap_or(""), "Stefan Webb");
}

/// Talks at the same event share the same PART_OF group.
/// Checked by looking up the group for each Gemma 4 co-presenter independently.
#[tokio::test]
async fn surreal_co_presented_talks_belong_to_same_group() {
    if !surreal::is_available().await {
        eprintln!("surreal not available — skipping");
        return;
    }
    // For each Gemma 4 speaker, find which group(s) their talks belong to.
    let mut all_groups: HashSet<String> = HashSet::new();
    for &speaker_nid in GEMMA4_SPEAKERS {
        let groups = surreal::groups_of_speaker(speaker_nid).await;
        assert!(!groups.is_empty(), "{speaker_nid} has no group affiliation");
        all_groups.extend(groups);
    }
    assert_eq!(
        all_groups,
        HashSet::from(["group:bay-area-ai".to_string()]),
        "all Gemma 4 co-presenters should belong to group:bay-area-ai"
    );
}

// ══════════════════════════════════════════════════════════ FalkorDB tests ════

/// All four node labels must match the exact file count.
#[tokio::test]
async fn falkor_node_counts() {
    if !tokio::task::spawn_blocking(falkor::is_available).await.unwrap() {
        eprintln!("falkor not available — skipping");
        return;
    }
    assert_eq!(
        tokio::task::spawn_blocking(|| falkor::count_label("Talk")).await.unwrap(),
        EXPECTED_TALKS, "talk count"
    );
    assert_eq!(
        tokio::task::spawn_blocking(|| falkor::count_label("Event")).await.unwrap(),
        EXPECTED_EVENTS, "event count"
    );
    assert_eq!(
        tokio::task::spawn_blocking(|| falkor::count_label("Group")).await.unwrap(),
        EXPECTED_GROUPS, "group count"
    );
    assert_eq!(
        tokio::task::spawn_blocking(|| falkor::count_label("Speaker")).await.unwrap(),
        EXPECTED_SPEAKERS, "speaker count"
    );
}

/// Relationship counts must match the deduplicated edge totals.
#[tokio::test]
async fn falkor_edge_counts() {
    if !tokio::task::spawn_blocking(falkor::is_available).await.unwrap() {
        eprintln!("falkor not available — skipping");
        return;
    }
    assert_eq!(
        tokio::task::spawn_blocking(|| falkor::count_rel("PRESENTED_AT")).await.unwrap(),
        EXPECTED_PRESENTED_AT, "PRESENTED_AT"
    );
    assert_eq!(
        tokio::task::spawn_blocking(|| falkor::count_rel("PRESENTED_BY")).await.unwrap(),
        EXPECTED_PRESENTED_BY, "PRESENTED_BY"
    );
    assert_eq!(
        tokio::task::spawn_blocking(|| falkor::count_rel("PART_OF")).await.unwrap(),
        EXPECTED_PART_OF, "PART_OF"
    );
}

/// A speaker who gave multiple talks across meetups.
#[tokio::test]
async fn falkor_speaker_gives_talks_at_multiple_meetups() {
    if !tokio::task::spawn_blocking(falkor::is_available).await.unwrap() {
        eprintln!("falkor not available — skipping");
        return;
    }
    let talks = tokio::task::spawn_blocking(|| falkor::talks_by_speaker(STEFAN_WEBB))
        .await
        .unwrap();
    assert_eq!(
        talks.len(),
        STEFAN_WEBB_TALK_COUNT,
        "{STEFAN_WEBB} should have {STEFAN_WEBB_TALK_COUNT} talks, got {talks:?}"
    );
}

/// Multi-speaker event lists all co-presenters.
#[tokio::test]
async fn falkor_event_has_multiple_speakers() {
    if !tokio::task::spawn_blocking(falkor::is_available).await.unwrap() {
        eprintln!("falkor not available — skipping");
        return;
    }
    let got = tokio::task::spawn_blocking(|| falkor::speakers_at_event(GEMMA4_EVENT))
        .await
        .unwrap();
    let expected: HashSet<&str> = GEMMA4_SPEAKERS.iter().copied().collect();
    let got_refs: HashSet<&str> = got.iter().map(|s| s.as_str()).collect();
    assert_eq!(got_refs, expected, "Gemma 4 event speaker set mismatch");
}

/// Speaker joins a group through their talk at one of the group's events.
#[tokio::test]
async fn falkor_speaker_joins_group_through_talk() {
    if !tokio::task::spawn_blocking(falkor::is_available).await.unwrap() {
        eprintln!("falkor not available — skipping");
        return;
    }
    let groups = tokio::task::spawn_blocking(|| falkor::groups_of_speaker(ADAM_WARSKI))
        .await
        .unwrap();
    // RETURN DISTINCT collapses all 3 talks into one group.
    assert_eq!(groups.len(), 1,
        "{ADAM_WARSKI} should appear in 1 unique group, got {groups:?}");
    assert!(groups.contains(ADAM_WARSKI_GROUP),
        "{ADAM_WARSKI} should be in {ADAM_WARSKI_GROUP}, got {groups:?}");
}

// ══════════════════════════════════════════════════════════ HelixDB tests ════

/// Node counts via helix-db SDK dynamic read queries.
#[tokio::test]
async fn helix_node_counts() {
    if !helix::is_available().await {
        eprintln!("helix not available — skipping");
        return;
    }
    let (talks, events, groups, speakers) = helix::node_counts().await;
    assert_eq!(talks,    EXPECTED_TALKS,    "talk count");
    assert_eq!(events,   EXPECTED_EVENTS,   "event count");
    assert_eq!(groups,   EXPECTED_GROUPS,   "group count");
    assert_eq!(speakers, EXPECTED_SPEAKERS, "speaker count");
}

/// A speaker who gave multiple talks across meetups appears linked to the right count.
#[tokio::test]
async fn helix_speaker_gives_talks_at_multiple_meetups() {
    if !helix::is_available().await {
        eprintln!("helix not available — skipping");
        return;
    }
    let talks = helix::talks_by_speaker(STEFAN_WEBB).await;
    assert_eq!(
        talks.len(),
        STEFAN_WEBB_TALK_COUNT,
        "{STEFAN_WEBB} should have {STEFAN_WEBB_TALK_COUNT} talks, got {talks:?}"
    );
}

/// An event where multiple speakers co-presented must list all of them.
#[tokio::test]
async fn helix_event_has_multiple_speakers() {
    if !helix::is_available().await {
        eprintln!("helix not available — skipping");
        return;
    }
    let got = helix::speakers_at_event(GEMMA4_EVENT).await;
    let expected: HashSet<&str> = GEMMA4_SPEAKERS.iter().copied().collect();
    let got_refs: HashSet<&str> = got.iter().map(|s| s.as_str()).collect();
    assert_eq!(got_refs, expected, "Gemma 4 event speaker set mismatch");
}

/// A speaker joins a group by presenting a talk at one of that group's events.
#[tokio::test]
async fn helix_speaker_joins_group_through_talk() {
    if !helix::is_available().await {
        eprintln!("helix not available — skipping");
        return;
    }
    let groups = helix::groups_of_speaker(ADAM_WARSKI).await;
    assert_eq!(
        groups.len(), 1,
        "{ADAM_WARSKI} gave {ADAM_WARSKI_TALK_COUNT} talks but they all belong to \
         one group; got {groups:?}"
    );
    assert!(
        groups.contains(ADAM_WARSKI_GROUP),
        "{ADAM_WARSKI} should be in {ADAM_WARSKI_GROUP}, got {groups:?}"
    );
}
