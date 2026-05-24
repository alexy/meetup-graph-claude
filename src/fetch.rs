use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::header::{self, HeaderMap, HeaderValue};
use tokio::sync::Semaphore;
use tokio::time::sleep;
use tracing::{info, warn};

pub struct Fetcher {
    client: reqwest::Client,
    delay: Duration,
    sem: Semaphore,
    max_retries: u32,
}

impl Fetcher {
    pub fn new(delay_ms: u64, cookies: Option<&str>) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            HeaderValue::from_static(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                 AppleWebKit/537.36 (KHTML, like Gecko) \
                 Chrome/124.0.0.0 Safari/537.36",
            ),
        );
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static(
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            ),
        );
        headers.insert(
            header::ACCEPT_LANGUAGE,
            HeaderValue::from_static("en-US,en;q=0.9"),
        );
        if let Some(c) = cookies {
            headers.insert(header::COOKIE, HeaderValue::from_str(c)?);
        }

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .cookie_store(true)
            .gzip(true)
            .timeout(Duration::from_secs(30))
            .build()?;

        Ok(Self {
            client,
            delay: Duration::from_millis(delay_ms),
            sem: Semaphore::new(1), // sequential — one request at a time
            max_retries: 3,
        })
    }

    pub async fn get(&self, url: &str) -> Result<String> {
        let _permit = self.sem.acquire().await?;
        sleep(self.delay).await;

        let mut last_err: Option<anyhow::Error> = None;

        for attempt in 0..self.max_retries {
            if attempt > 0 {
                let backoff = self.delay * 2u32.pow(attempt);
                warn!("Retry {attempt} for {url}, backoff {}ms", backoff.as_millis());
                sleep(backoff).await;
            }

            match self.client.get(url).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        let text = resp.text().await.context("reading body")?;
                        info!("GET {} → {} bytes", url, text.len());
                        return Ok(text);
                    }
                    if status.as_u16() == 429 {
                        warn!("Rate-limited on {url}, waiting 60 s");
                        sleep(Duration::from_secs(60)).await;
                        continue;
                    }
                    if status.as_u16() == 403 {
                        return Err(anyhow::anyhow!(
                            "403 Forbidden for {url}. \
                             Export your browser cookies and set MEETUP_COOKIES."
                        ));
                    }
                    last_err = Some(anyhow::anyhow!("HTTP {status} for {url}"));
                }
                Err(e) => {
                    warn!("Request error for {url}: {e}");
                    last_err = Some(e.into());
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("max retries for {url}")))
    }
}
