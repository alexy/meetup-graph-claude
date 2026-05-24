use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::header::{self, HeaderMap, HeaderValue};
use serde::Deserialize;
use tokio::time::sleep;
use tracing::{info, warn};

const GQL_URL: &str = "https://www.meetup.com/gql2";

// Uses filter status ACTIVE|PAST|CANCELLED to get everything, DESC so newest first.
const EVENTS_QUERY: &str = r#"
query($urlname: String!, $after: String) {
  groupByUrlname(urlname: $urlname) {
    id
    name
    link
    events(
      filter: { status: [ACTIVE, PAST, CANCELLED] }
      first: 20
      sort: DESC
      after: $after
    ) {
      totalCount
      edges {
        node {
          id
          title
          dateTime
          endTime
          description
          eventUrl
          venue { name address city state }
        }
      }
      pageInfo { hasNextPage endCursor }
    }
  }
}
"#;

// ── wire types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct GqlEvent {
    pub id: String,
    pub title: String,
    #[serde(rename = "dateTime")]
    pub date_time: Option<String>,
    pub description: Option<String>,
    #[serde(rename = "eventUrl")]
    pub event_url: String,
    pub venue: Option<GqlVenue>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GqlVenue {
    pub name: Option<String>,
    pub address: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
}

// ── client ────────────────────────────────────────────────────────────────────

pub struct GqlClient {
    client: reqwest::Client,
    delay: Duration,
}

impl GqlClient {
    pub fn new(delay_ms: u64) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            HeaderValue::from_static(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                 AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36",
            ),
        );
        headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .build()?;

        Ok(Self { client, delay: Duration::from_millis(delay_ms) })
    }

    /// Fetches every page of events for `group` and returns them all.
    pub async fn all_events(&self, group: &str) -> Vec<GqlEvent> {
        let mut all: Vec<GqlEvent> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut page = 0u32;

        loop {
            sleep(self.delay).await;
            page += 1;

            match self.fetch_page(group, cursor.as_deref()).await {
                Ok((events, total, next_cursor, has_next)) => {
                    info!(
                        "  page {page}: +{} events (total reported: {total}, fetched so far: {})",
                        events.len(),
                        all.len() + events.len()
                    );
                    all.extend(events);
                    if has_next {
                        cursor = Some(next_cursor);
                    } else {
                        break;
                    }
                }
                Err(e) => {
                    warn!("  GraphQL fetch failed for {group} page {page}: {e:#}");
                    break;
                }
            }
        }

        info!("  {group}: {total} events fetched via GraphQL", total = all.len());
        all
    }

    async fn fetch_page(
        &self,
        group: &str,
        after: Option<&str>,
    ) -> Result<(Vec<GqlEvent>, u64, String, bool)> {
        let body = serde_json::json!({
            "query": EVENTS_QUERY,
            "variables": { "urlname": group, "after": after }
        });

        let resp = self
            .client
            .post(GQL_URL)
            .json(&body)
            .send()
            .await
            .context("GraphQL POST")?;

        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow::anyhow!("GraphQL HTTP {status}"));
        }

        let data: serde_json::Value = resp.json().await.context("GraphQL JSON")?;

        if let Some(errors) = data.get("errors") {
            return Err(anyhow::anyhow!("GraphQL errors: {errors}"));
        }

        let ev_conn = &data["data"]["groupByUrlname"]["events"];
        let total = ev_conn["totalCount"].as_u64().unwrap_or(0);

        let edges = ev_conn["edges"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("no edges in response"))?;

        let events: Vec<GqlEvent> = edges
            .iter()
            .filter_map(|e| {
                serde_json::from_value::<GqlEvent>(e["node"].clone())
                    .map_err(|err| { warn!("deserialise event: {err}"); err })
                    .ok()
            })
            .collect();

        let pi = &ev_conn["pageInfo"];
        let has_next = pi["hasNextPage"].as_bool().unwrap_or(false);
        let end_cursor = pi["endCursor"].as_str().unwrap_or("").to_string();

        Ok((events, total, end_cursor, has_next))
    }
}
