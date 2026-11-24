use std::future::Future;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use reqwest::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, USER_AGENT};
use tokio::time::Instant;

use super::{HEAD_BLOCKS, STALL, Shared, block_count};

const STARTUP: Duration = Duration::from_secs(3);
const IDLE_PREFETCH_HOLD: Duration = Duration::from_secs(180);

#[derive(PartialEq, Eq)]
enum Access {
    Read,
    Wait { idle_prefetch: bool },
    Stop,
}

fn access(shared: &Shared, index: usize) -> Access {
    let inner = shared.inner.lock().unwrap();
    if inner.cancelled || inner.error.is_some() || inner.blocks[index].is_some() || inner.in_flight[index] {
        return Access::Stop;
    }
    if inner.wants.iter().any(|&(_, want)| {
        inner.blocks.get(want).is_some_and(Option::is_none)
            && (want < index || want.saturating_sub(index) > HEAD_BLOCKS)
    }) {
        // Range workers can reach a seek target without downloading the gap.
        return Access::Stop;
    }
    if index >= inner.limit || !inner.in_window(index) {
        return Access::Wait { idle_prefetch: index >= inner.limit && inner.readers == 0 };
    }
    Access::Read
}

async fn wait_for_block(shared: &Shared, index: usize) -> bool {
    let mut idle_until = None;
    loop {
        let changed = shared.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        match access(shared, index) {
            Access::Read => return true,
            Access::Stop => return false,
            Access::Wait { idle_prefetch: true } => {
                // Release an unused response while retaining the prefetched blocks.
                let deadline = *idle_until.get_or_insert_with(|| Instant::now() + IDLE_PREFETCH_HOLD);
                if tokio::time::timeout_at(deadline, changed).await.is_err() {
                    return access(shared, index) == Access::Read;
                }
            }
            Access::Wait { idle_prefetch: false } => {
                idle_until = None;
                changed.await;
            }
        }
    }
}

async fn wait_for_http<T>(
    shared: &Shared,
    index: usize,
    deadline: Instant,
    request: impl Future<Output = reqwest::Result<T>>,
) -> anyhow::Result<Option<T>> {
    let request = tokio::time::timeout_at(deadline, request);
    tokio::pin!(request);
    loop {
        let changed = shared.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if access(shared, index) != Access::Read {
            return Ok(None);
        }
        tokio::select! {
            biased;
            () = changed => {}
            result = &mut request => {
                let result = result.map_err(|_| anyhow::anyhow!("continuous file stopped sending bytes"))?;
                return result.map(Some).map_err(|e| e.without_url().into());
            }
        }
    }
}

async fn next_bytes(response: &mut reqwest::Response) -> reqwest::Result<Option<Bytes>> {
    while let Some(bytes) = response.chunk().await? {
        if !bytes.is_empty() {
            return Ok(Some(bytes));
        }
    }
    Ok(None)
}

fn checked_headers(response: &reqwest::Response, total: u64) -> anyhow::Result<()> {
    anyhow::ensure!(response.status() == reqwest::StatusCode::OK, "continuous file returned {}", response.status());
    let headers = response.headers();
    anyhow::ensure!(!headers.contains_key(CONTENT_RANGE), "continuous file returned a byte range");
    anyhow::ensure!(
        headers.get_all(CONTENT_ENCODING).iter().all(|value| value.as_bytes().eq_ignore_ascii_case(b"identity")),
        "continuous file returned encoded bytes"
    );
    let mut lengths = headers.get_all(CONTENT_LENGTH).iter();
    let length = lengths.next().and_then(|value| value.to_str().ok()).and_then(|value| value.parse().ok());
    anyhow::ensure!(length == Some(total) && lengths.next().is_none(), "continuous file length did not match its metadata");
    Ok(())
}

/// Try one whole-file response before starting the regular range workers.
/// A cancelled buffer or a seek out of sequence leaves those workers to the caller.
pub(super) async fn prefetch(http: &reqwest::Client, url: &str, ua: &str, shared: &Shared) -> anyhow::Result<()> {
    anyhow::ensure!(shared.total > 0 && shared.segments.is_none(), "continuous playback needs a file with a known size");
    let address = reqwest::Url::parse(url).map_err(|_| anyhow::anyhow!("invalid continuous file URL"))?;
    anyhow::ensure!(!address.query_pairs().any(|(key, _)| key == "range"), "continuous file URL already contains a range");
    if !wait_for_block(shared, 0).await {
        return Ok(());
    }

    let startup = Instant::now() + STARTUP;
    let request = http.get(url).header(USER_AGENT, ua).header(ACCEPT_ENCODING, "identity").send();
    let Some(response) = wait_for_http(shared, 0, startup, request).await? else { return Ok(()) };
    let mut response = response.error_for_status().map_err(reqwest::Error::without_url)?;
    checked_headers(&response, shared.total)?;
    shared.note_cache_policy(response.headers());

    let mut remaining = Bytes::new();
    let mut received = false;
    for index in 0..block_count(shared.total) {
        if !wait_for_block(shared, index).await {
            return Ok(());
        }
        let (start, end) = shared.range(index);
        let size = (end - start + 1) as usize;
        let mut block = BytesMut::with_capacity(size);
        while block.len() < size {
            if remaining.is_empty() {
                let deadline = if received { Instant::now() + STALL } else { startup };
                let Some(bytes) = wait_for_http(shared, index, deadline, next_bytes(&mut response)).await? else { return Ok(()) };
                remaining = bytes.ok_or_else(|| anyhow::anyhow!("continuous file ended before its declared length"))?;
                received = true;
            }
            let take = remaining.len().min(size - block.len());
            block.extend_from_slice(&remaining.split_to(take));
        }

        if end + 1 == shared.total {
            // Keep the last block private until the body framing has ended cleanly.
            anyhow::ensure!(remaining.is_empty(), "continuous file exceeded its declared length");
            let Some(bytes) = wait_for_http(shared, index, Instant::now() + STALL, next_bytes(&mut response)).await? else { return Ok(()) };
            anyhow::ensure!(bytes.is_none(), "continuous file exceeded its declared length");
        }

        if !wait_for_block(shared, index).await {
            return Ok(());
        }
        shared.put(index, block.freeze(), false);
    }
    Ok(())
}
