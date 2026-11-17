//! Shared HTTP handling for long-running public QLever snapshot requests.
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, RETRY_AFTER};

pub(crate) fn request(
    client: &Client,
    endpoint: &str,
    query: &str,
    accept: &str,
    send: Option<&str>,
) -> Result<Response> {
    for attempt in 0..=4 {
        let mut request = client.get(endpoint).header(ACCEPT, accept).query(&[("query", query)]);
        if let Some(send) = send {
            request = request.query(&[("send", send)]);
        }
        let response = request.send().context("requesting QLever index")?;
        if matches!(response.status(), StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE)
            && attempt < 4
        {
            let delay = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(1 << attempt)
                .min(120);
            eprintln!("QLever returned {}; retrying in {delay}s", response.status());
            thread::sleep(Duration::from_secs(delay));
            continue;
        }
        return response.error_for_status().context("QLever index response");
    }
    unreachable!("the final request always returns a response or error")
}
