//! Parallel, seek-aware HTTP audio buffer.
//!
//! The file is split into fixed blocks fetched by a few concurrent range requests.
//! Workers always claim the first missing block at or after the one a reader is
//! waiting on, so playback start and seeks are served first and the rest fills in
//! around them. Reads of a missing block block until it arrives (or the download fails).
//!
//! An HLS stream (`Media::Segments`) is buffered the same way, a segment per block.
//!
//! Several readers may share one buffer (the audio engine opens a second, seekable
//! decoder when the user seeks); the download stops once the last reader is dropped.

use std::future::Future;
use std::io::{self, Read, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::{Bytes, BytesMut};
use reqwest::header::{AGE, CACHE_CONTROL, DATE, EXPIRES, HeaderMap, HeaderName, VARY};

use crate::cache::audio::MAX_AUDIO_BYTES;

mod cdn;
mod continuous;
#[cfg(test)]
mod continuous_tests;
pub(crate) use cdn::CdnFallback;

const BLOCK: u64 = 256 * 1024;
/// The first block is smaller: a decoder opens after a few hundred bytes, so playback
/// waits on this one alone, and it still holds seconds of audio while the next arrive.
const FIRST: u64 = 64 * 1024;
/// Some signed-in file URLs serve only the first MiB.
const WEB_PREVIEW: u64 = 1024 * 1024;
const WORKERS: usize = 4;
/// Tries per block before the download gives up, waiting a little longer each time
/// (about half a minute in all): long enough to ride out a dropped connection.
const RETRIES: u32 = 6;
/// A response that delivers nothing for this long is dead; a slow one that keeps
/// delivering is fine, however long the block takes.
const STALL: Duration = Duration::from_secs(20);
/// googlevideo occasionally starts rejecting a URL partway through (HTTP 403);
/// a freshly resolved URL for the same video resumes the download.
const MAX_REFRESHES: u32 = 2;
/// A prefetch only fetches this much (~10 s of audio at 256 kbps, two HLS segments)
/// until playback actually opens it: enough to start at once, without holding a whole
/// extra song in memory.
const HEAD_BLOCKS: usize = 2;
/// Files larger than this (about 20 minutes at 256 kbps: long mixes, whole albums in
/// one video) aren't kept whole. Each reader keeps a window around where it reads,
/// `AHEAD` blocks forward (about 5 minutes at 256 kbps) and `BEHIND` back; the rest is dropped and
/// fetched again if a seek goes back to it.
const WINDOWED_FROM: u64 = 40 * 1024 * 1024;
const AHEAD: usize = 40;
const BEHIND: usize = 8;

/// Block `idx`'s first byte in a file.
fn block_start(idx: usize) -> u64 {
    if idx == 0 { 0 } else { FIRST + (idx as u64 - 1) * BLOCK }
}

/// The block holding byte `pos` of a file.
fn block_of(pos: u64) -> usize {
    if pos < FIRST { 0 } else { 1 + ((pos - FIRST) / BLOCK) as usize }
}

/// How many blocks a file of `total` bytes takes.
fn block_count(total: u64) -> usize {
    if total == 0 { 0 } else { block_of(total - 1) + 1 }
}

/// What a download fetches.
pub(crate) enum Media {
    /// One file, in ranges: `FIRST` bytes, then `BLOCK`s.
    File(String),
    /// A file with one alternate CDN explicitly advertised for this representation.
    CdnFile(String, CdnFallback),
    /// A podcast feed's file on its own server: the same ranges, asked for in a
    /// `Range` header (googlevideo's `&range=` means nothing there).
    Feed(String),
    /// An HLS playlist's segments, one block each.
    Segments(Vec<String>),
}

/// Resolves fresh URLs for the same video and format.
pub(crate) type Refresh = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = anyhow::Result<Media>> + Send>> + Send + Sync>;

struct Inner {
    blocks: Vec<Option<Bytes>>,
    in_flight: Vec<bool>,
    /// Block a reader most recently needed; workers prioritize from here.
    want: usize,
    error: Option<String>,
    /// The download stopped on HTTP 403s that fresh URLs didn't get past.
    refused: bool,
    readers: usize,
    /// Set once a reader existed and all were dropped.
    cancelled: bool,
    /// Blocks the workers may fetch; lifted to all of them when a reader opens.
    limit: usize,
    /// Each open reader's id and the block it last read (windowed files keep only
    /// what's near these).
    wants: Vec<(u64, usize)>,
    windowed: bool,
    /// Blocks fetched at least once, and how many: the download counts as finished
    /// once each was, even if a windowed file has dropped some since.
    ever: Vec<bool>,
    fetched: usize,
    /// A complete file has already been restored or handed off to the cache.
    cache_handled: bool,
    /// Verified file bytes past the web preview, even if the buffer has dropped them.
    verified_past_web_preview: bool,
    /// Provenance of verified reserve blocks, allocated only after one arrives.
    fallback_blocks: Option<Vec<u64>>,
}

impl Inner {
    /// Whether block `i` belongs in memory now.
    fn in_window(&self, i: usize) -> bool {
        !self.windowed || self.wants.is_empty() || self.wants.iter().any(|&(_, w)| i + BEHIND >= w && i < w + AHEAD)
    }
}

enum Claim {
    Block(usize),
    /// Everything allowed so far is fetched or in flight; wait for the limit to lift.
    Wait,
    Done,
}

/// Stream buffers alive, for the `YTM_MEM` dev log.
pub static LIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl Drop for Shared {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

pub struct Shared {
    inner: Mutex<Inner>,
    /// Earliest file response expiry in Unix seconds (MAX unset, zero uncacheable).
    cache_until: AtomicU64,
    #[cfg(test)]
    used_fallback_host: AtomicBool,
    ready: Condvar,
    pub total: u64,
    /// Track length reported by the stream metadata (0 if unknown).
    pub duration_ms: u64,
    /// YouTube's loudness for this stream, in dB above its -14 LUFS target.
    pub loudness_db: Option<f32>,
    /// Opus in WebM; otherwise AAC in MP4, or ADTS in HLS segments.
    pub opus: bool,
    /// A podcast feed's file (`Media::Feed`): MP3 mostly, which seeks by estimate,
    /// since an exact seek in MP3 reads every frame before the spot.
    pub feed: bool,
    /// Audio opened from the downloads folder.
    pub saved: bool,
    /// HLS: each segment's start (s). Its blocks are the segments.
    pub segments: Option<Arc<[f64]>>,
    /// Flips to true once the download task has stopped (complete, failed or cancelled).
    done: tokio::sync::watch::Sender<bool>,
    /// Wakes waiting workers: `limit` lifted, a reader moved or left, or cancelled.
    changed: tokio::sync::Notify,
}

impl Shared {
    /// A buffer for `total` bytes. `head_only` (a prefetch) fetches just the start until
    /// a reader opens it.
    pub fn new(total: u64, duration_ms: u64, loudness_db: Option<f32>, opus: bool, head_only: bool) -> Arc<Self> {
        Self::build(block_count(total), total, total > WINDOWED_FROM, duration_ms, loudness_db, opus, false, None, head_only, false)
    }

    /// A buffer for a podcast feed's file of `total` bytes.
    pub fn feed(total: u64, duration_ms: u64, head_only: bool) -> Arc<Self> {
        Self::build(block_count(total), total, total > WINDOWED_FROM, duration_ms, None, false, true, None, head_only, false)
    }

    /// A buffer for an HLS stream's segments, starting at `starts` (s).
    pub fn segmented(starts: Vec<f64>, duration_ms: u64, loudness_db: Option<f32>, head_only: bool) -> Arc<Self> {
        // About as many segments (~7 s) as a windowed file has blocks (~8 s at 256 kbps).
        let windowed = starts.len() as u64 > WINDOWED_FROM / BLOCK;
        Self::build(starts.len(), 0, windowed, duration_ms, loudness_db, false, false, Some(starts.into()), head_only, false)
    }

    #[allow(clippy::too_many_arguments)]
    fn build(n: usize, total: u64, windowed: bool, duration_ms: u64, loudness_db: Option<f32>, opus: bool, feed: bool, segments: Option<Arc<[f64]>>, head_only: bool, saved: bool) -> Arc<Self> {
        let limit = if head_only { HEAD_BLOCKS.min(n) } else { n };
        LIVE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Arc::new(Self {
            inner: Mutex::new(Inner {
                blocks: vec![None; n],
                in_flight: vec![false; n],
                want: 0,
                error: None,
                refused: false,
                readers: 0,
                cancelled: false,
                limit,
                wants: Vec::new(),
                windowed,
                ever: vec![false; n],
                fetched: 0,
                cache_handled: false,
                verified_past_web_preview: false,
                fallback_blocks: None,
            }),
            cache_until: AtomicU64::new(u64::MAX),
            ready: Condvar::new(),
            #[cfg(test)]
            used_fallback_host: AtomicBool::new(false),
            total,
            duration_ms,
            loudness_db,
            opus,
            feed,
            saved,
            segments,
            done: tokio::sync::watch::Sender::new(false),
            changed: tokio::sync::Notify::new(),
        })
    }

    /// A buffer that already holds the whole file (a downloaded song).
    pub fn from_bytes(data: Vec<u8>, duration_ms: u64, loudness_db: Option<f32>) -> Arc<Self> {
        let total = data.len() as u64;
        let data = Bytes::from(data);
        // Saved songs are AAC (downloads take that stream).
        let shared = Self::build(block_count(total), total, false, duration_ms, loudness_db, false, false, None, false, true);
        let n = shared.inner.lock().unwrap().blocks.len();
        let blocks: Vec<Option<Bytes>> = (0..n).map(|i| Some(data.slice(block_start(i) as usize..block_start(i + 1).min(total) as usize))).collect();
        {
            let mut inner = shared.inner.lock().unwrap();
            inner.blocks = blocks;
            inner.windowed = false;
            inner.ever = vec![true; n];
            inner.fetched = n;
            inner.verified_past_web_preview = total > WEB_PREVIEW;
        }
        shared.done.send_replace(true);
        shared
    }

    fn is_cacheable_file(&self) -> bool {
        (1..=MAX_AUDIO_BYTES).contains(&self.total)
            && !self.feed && !self.saved && self.segments.is_none()
    }

    /// Install checked cache bytes before starting a download, keeping the file's format.
    pub(crate) fn restore_cached_file(&self, bytes: Vec<u8>) -> bool {
        if !self.is_cacheable_file() || bytes.len() as u64 != self.total {
            return false;
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.windowed || inner.cancelled || inner.error.is_some() || inner.cache_handled
            || inner.fetched != 0 || inner.blocks.iter().any(Option::is_some)
            || inner.in_flight.iter().any(|&in_flight| in_flight)
        {
            return false;
        }

        let bytes = Bytes::from(bytes);
        for (index, block) in inner.blocks.iter_mut().enumerate() {
            let (start, end) = self.range(index);
            *block = Some(bytes.slice(start as usize..=end as usize));
        }
        inner.limit = inner.blocks.len();
        inner.ever.fill(true);
        inner.fetched = inner.limit;
        inner.verified_past_web_preview = self.total > WEB_PREVIEW;
        inner.cache_handled = true;
        drop(inner);
        self.done.send_replace(true);
        self.ready.notify_all();
        self.changed.notify_waiters();
        true
    }

    /// Hand off verified, still-live file blocks once, without copying their bytes.
    pub(crate) fn claim_cache_blocks(&self) -> Option<(Vec<Bytes>, u64)> {
        if !self.is_cacheable_file() {
            return None;
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.windowed || inner.cancelled || inner.error.is_some() || inner.cache_handled
            || inner.fetched != inner.blocks.len()
        {
            return None;
        }

        let until = self.cache_until.load(Ordering::Relaxed);
        if until == u64::MAX || until <= unix_seconds()? {
            return None;
        }
        let blocks = inner.blocks.iter().cloned().collect::<Option<Vec<_>>>()?;
        inner.cache_handled = true;
        Some((blocks, until))
    }

    fn observed_cache_until(&self, headers: &HeaderMap) -> Option<u64> {
        if !self.is_cacheable_file() || self.cache_until.load(Ordering::Relaxed) == 0 {
            return None;
        }
        Some(unix_seconds().and_then(|now| cache_policy_until(headers, now)).unwrap_or(0))
    }

    pub(super) fn note_cache_policy(&self, headers: &HeaderMap) {
        if let Some(until) = self.observed_cache_until(headers) {
            self.cache_until.fetch_min(until, Ordering::Relaxed);
        }
    }

    /// The whole file, once every block has arrived (a download nobody's reading).
    pub fn bytes(&self) -> Option<Vec<u8>> {
        let inner = self.inner.lock().unwrap();
        let mut out = Vec::with_capacity(self.total as usize);
        for b in &inner.blocks {
            out.extend_from_slice(b.as_ref()?);
        }
        Some(out)
    }

    /// Every block is in memory: readers never wait and the buffer can be replayed.
    pub fn is_complete(&self) -> bool {
        self.inner.lock().unwrap().blocks.iter().all(Option::is_some)
    }

    /// Wait until the download stops, however it ends.
    pub async fn finished(&self) {
        let _ = self.done.subscribe().wait_for(|d| *d).await;
    }

    /// Why the download stopped early, if it failed.
    pub fn error(&self) -> Option<String> {
        self.inner.lock().unwrap().error.clone()
    }

    /// Before replacing a working route, check that this web file actually serves
    /// audio beyond the preview. Observe the normal workers without starting a read.
    pub async fn past_web_preview(&self) -> anyhow::Result<()> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let inner = self.inner.lock().unwrap();
                anyhow::ensure!(!inner.cancelled && inner.error.is_none(), "web stream has stopped");
                if self.total <= WEB_PREVIEW || inner.verified_past_web_preview {
                    return Ok(());
                }
            }
            changed.await;
        }
    }

    /// The download failed on HTTP 403s that fresh URLs didn't get past.
    pub fn refused(&self) -> bool {
        self.inner.lock().unwrap().refused
    }

    /// A block a reader may need now is still to come (a windowed file counts only its
    /// window): downloads in the background make way for it.
    pub fn filling(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        !inner.cancelled && inner.error.is_none() && (0..inner.limit).any(|i| inner.blocks[i].is_none() && inner.in_window(i))
    }

    /// Stop downloading (e.g. a prefetch that was superseded).
    pub fn cancel(&self) {
        self.inner.lock().unwrap().cancelled = true;
        self.ready.notify_all();
        self.changed.notify_waiters();
    }

    fn claim(&self) -> Claim {
        let mut inner = self.inner.lock().unwrap();
        if inner.cancelled || inner.error.is_some() {
            return Claim::Done;
        }
        let n = inner.blocks.len();
        let start = inner.want.min(n.saturating_sub(1));
        let mut missing = false;
        for i in (start..n).chain(0..start) {
            if inner.blocks[i].is_some() || inner.in_flight[i] {
                continue;
            }
            missing = true;
            if i < inner.limit && inner.in_window(i) {
                inner.in_flight[i] = true;
                return Claim::Block(i);
            }
        }
        // A windowed file may need dropped blocks again while it's being read.
        if missing || (inner.windowed && inner.readers > 0) { Claim::Wait } else { Claim::Done }
    }

    /// True after a reader takes a validated block from the advertised fallback.
    #[cfg(test)]
    pub fn used_fallback_host(&self) -> bool {
        self.used_fallback_host.load(Ordering::Relaxed)
    }

    fn put(&self, idx: usize, data: Bytes, from_fallback: bool) {
        let mut inner = self.inner.lock().unwrap();
        if inner.cancelled || inner.error.is_some() {
            return;
        }
        inner.blocks[idx] = Some(data);
        if from_fallback && inner.fallback_blocks.is_none() {
            inner.fallback_blocks = Some(vec![0; inner.blocks.len().div_ceil(64)]);
        }
        if let Some(blocks) = inner.fallback_blocks.as_mut() {
            let bit = 1 << (idx % 64);
            if from_fallback {
                blocks[idx / 64] |= bit;
            } else {
                blocks[idx / 64] &= !bit;
            }
        }
        inner.in_flight[idx] = false;
        if !std::mem::replace(&mut inner.ever[idx], true) {
            inner.fetched += 1;
            if inner.fetched == inner.blocks.len() {
                self.done.send_replace(true);
            }
        }
        if inner.windowed && !inner.wants.is_empty() {
            for i in 0..inner.blocks.len() {
                if inner.blocks[i].is_some() && !inner.in_window(i) {
                    inner.blocks[i] = None;
                }
            }
        }
        self.ready.notify_all();
        if self.total > WEB_PREVIEW && idx >= block_of(WEB_PREVIEW) && !inner.verified_past_web_preview {
            inner.verified_past_web_preview = true;
            self.changed.notify_waiters();
        }
    }

    fn fail(&self, err: String, refused: bool) {
        let mut inner = self.inner.lock().unwrap();
        if inner.cancelled || inner.error.is_some() {
            return;
        }
        log::error!("download failed: {err}");
        FAIL_FROM.store(usize::MAX, std::sync::atomic::Ordering::Relaxed);
        inner.error = Some(err);
        inner.refused = refused;
        drop(inner);
        self.ready.notify_all();
        self.changed.notify_waiters();
    }

    async fn until_stopped(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let inner = self.inner.lock().unwrap();
                if inner.cancelled || inner.error.is_some() {
                    return;
                }
            }
            changed.await;
        }
    }

    fn range(&self, idx: usize) -> (u64, u64) {
        (block_start(idx), block_start(idx + 1).min(self.total) - 1)
    }
}

fn unix_seconds() -> Option<u64> {
    Some(SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs())
}

fn cache_seconds(value: &str) -> Option<u64> {
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn single_http_date(headers: &HeaderMap, name: HeaderName) -> Option<Option<u64>> {
    let mut values = headers.get_all(name).iter();
    let date = match values.next() {
        Some(value) => Some(httpdate::parse_http_date(value.to_str().ok()?).ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs()),
        None => None,
    };
    values.next().is_none().then_some(date)
}

fn cache_policy_until(headers: &HeaderMap, now: u64) -> Option<u64> {
    for value in headers.get_all(VARY) {
        if value.to_str().ok()?.split(',').any(|name| name.trim() == "*") {
            return None;
        }
    }

    let mut max_age = None;
    for value in headers.get_all(CACHE_CONTROL) {
        for directive in value.to_str().ok()?.split(',') {
            let (name, arg) = directive.split_once('=').map_or((directive, None), |(name, arg)| (name, Some(arg.trim())));
            let name = name.trim();
            if name.contains('"') || name.eq_ignore_ascii_case("no-store") || name.eq_ignore_ascii_case("no-cache") {
                return None;
            }
            // A quoted value split at a comma is ambiguous.
            let arg = match arg {
                Some(arg) if arg.contains('"') => {
                    let unquoted = arg.strip_prefix('"')?.strip_suffix('"')?;
                    if unquoted.contains('"') {
                        return None;
                    }
                    Some(unquoted)
                }
                other => other,
            };
            if name.eq_ignore_ascii_case("max-age") {
                let seconds = cache_seconds(arg?)?;
                if max_age.is_some_and(|previous| previous != seconds) {
                    return None;
                }
                max_age = Some(seconds);
            }
        }
    }

    let mut ages = headers.get_all(AGE).iter();
    let age = match ages.next() {
        Some(age) => cache_seconds(age.to_str().ok()?.trim())?,
        None => 0,
    };
    if ages.next().is_some() {
        return None;
    }
    let date = single_http_date(headers, DATE)?;
    let freshness = match max_age {
        Some(seconds) => seconds,
        None => {
            let expires = single_http_date(headers, EXPIRES)??;
            if expires <= now {
                return None;
            }
            expires.checked_sub(date.unwrap_or(now))?
        }
    };
    let apparent_age = date.map_or(0, |date| now.saturating_sub(date));
    let remaining = freshness.checked_sub(age.max(apparent_age))?.min(24 * 60 * 60);
    if remaining == 0 {
        return None;
    }
    now.checked_add(remaining)
}

struct UrlState {
    media: Media,
    /// Both refresh and fallback invalidate in-flight requests for the previous URL.
    version: u32,
    refreshes: u32,
    client: reqwest::Client,
    fallback_host: bool,
    failures: u32,
}

struct UrlSlot {
    url: tokio::sync::Mutex<UrlState>,
    renewal: tokio::sync::Mutex<()>,
    refresh: Option<Refresh>,
    /// Segments the buffer has: fresh ones must be the same.
    segments: Option<usize>,
    /// Ordinary File/Feed/Segments keep their current refresh behavior.
    changes: Option<tokio::sync::watch::Sender<u32>>,
}

impl UrlSlot {
    /// Replace the URL if `seen` is still current; returns false when out of refreshes.
    async fn renew(&self, seen: u32) -> bool {
        let Some(refresh) = self.refresh.as_ref() else {
            return self.url.lock().await.version != seen;
        };
        if self.url.lock().await.version != seen {
            return true;
        }
        // Deduplicate refresh lookups without blocking delivery of verified
        // blocks or holding the state lock across the network.
        let _renewal = self.renewal.lock().await;
        {
            let slot = self.url.lock().await;
            if slot.version != seen {
                return true;
            }
            if slot.refreshes >= MAX_REFRESHES {
                log::warn!("stream URL still refused after {MAX_REFRESHES} refreshes");
                return false;
            }
        }
        let result = refresh().await;
        let mut slot = self.url.lock().await;
        if slot.version != seen {
            return true;
        }
        let refreshed = match result {
            // The buffer is laid out for the stream it started with.
            Ok(Media::Segments(urls)) if Some(urls.len()) != self.segments => {
                log::warn!("stream URL refresh: {} segments for a buffer of {:?}", urls.len(), self.segments);
                return false;
            }
            Ok(Media::CdnFile(_, _) | Media::File(_) | Media::Feed(_)) if self.segments.is_some() => {
                log::warn!("stream URL refresh: a file for a segmented buffer");
                return false;
            }
            Ok(media) => media,
            Err(e) => {
                log::warn!("stream URL refresh failed: {e:#}");
                return false;
            }
        };
        slot.media = if slot.fallback_host {
            // A 403 does not authorize changing origins. A fresh player response
            // must still advertise the same reserve for this representation.
            let (Media::File(current), Media::CdnFile(_, reserve)) = (&slot.media, refreshed) else { return false };
            if !same_origin(current, &reserve.url) {
                return false;
            }
            Media::File(reserve.url)
        } else {
            refreshed
        };
        log::info!("stream URL refreshed after 403");
        slot.refreshes += 1;
        slot.client = crate::proxy::http();
        slot.failures = 0;
        self.advance(&mut slot);
        true
    }

    fn advance(&self, slot: &mut UrlState) {
        slot.version += 1;
        if let Some(changes) = &self.changes {
            changes.send_replace(slot.version);
        }
    }
}

fn same_origin(a: &str, b: &str) -> bool {
    match (reqwest::Url::parse(a), reqwest::Url::parse(b)) {
        (Ok(a), Ok(b)) => a.origin() == b.origin(),
        _ => false,
    }
}
/// The buffer a background download makes way for (the playing song's), if any.
pub type Ahead = Arc<dyn Fn() -> Option<Arc<Shared>> + Send + Sync>;

#[derive(PartialEq, Eq)]
enum DownloadMode {
    Playback,
    ContinuousFirst,
    Behind,
}

/// Download into `shared` with parallel range requests; returns when complete,
/// failed, or every reader was dropped.
pub(crate) async fn download(http: reqwest::Client, media: Media, ua: String, shared: Arc<Shared>, refresh: Option<Refresh>) {
    download_blocks(http, media, ua, shared, refresh, None, DownloadMode::Playback).await;
}

/// A short audio file can keep the same response from its first block through
/// playback. If that response cannot continue, ranges fill the missing blocks.
pub(crate) async fn download_prefetch(http: reqwest::Client, media: Media, ua: String, shared: Arc<Shared>, refresh: Option<Refresh>) {
    download_blocks(http, media, ua, shared, refresh, None, DownloadMode::ContinuousFirst).await;
}

/// `download`, but before each block it waits while `ahead`'s buffer still has blocks
/// to come that a reader may need: the playing song gets the bandwidth first.
pub(crate) async fn download_behind(http: reqwest::Client, media: Media, ua: String, shared: Arc<Shared>, refresh: Option<Refresh>, ahead: Option<Ahead>) {
    download_blocks(http, media, ua, shared, refresh, ahead, DownloadMode::Behind).await;
}

async fn download_blocks(http: reqwest::Client, media: Media, ua: String, shared: Arc<Shared>, refresh: Option<Refresh>, ahead: Option<Ahead>, mode: DownloadMode) {
    const MAX_CONTINUOUS_FILE: u64 = 8 * 1024 * 1024;
    let whole_url = match &media {
        Media::File(url) | Media::CdnFile(url, _) if mode == DownloadMode::ContinuousFirst
            && shared.total > 0 && shared.total <= MAX_CONTINUOUS_FILE
            && FAIL_FROM.load(Ordering::Relaxed) == usize::MAX && REFUSE_FROM.load(Ordering::Relaxed) == usize::MAX => Some(url.clone()),
        _ => None,
    };
    let playing = mode != DownloadMode::Behind;
    let segments = shared.segments.as_ref().map(|s| s.len());
    let cdn_file = playing && matches!(media, Media::CdnFile(_, _));
    let slot = Arc::new(UrlSlot {
        url: tokio::sync::Mutex::new(UrlState { media, version: 0, refreshes: 0, client: http.clone(), fallback_host: false, failures: 0 }),
        refresh,
        segments,
        changes: cdn_file.then(|| tokio::sync::watch::Sender::new(0)),
        renewal: tokio::sync::Mutex::new(()),
    });
    if let Some(url) = whole_url {
        if let Err(e) = continuous::prefetch(&http, &url, &ua, &shared).await {
            let status = e.downcast_ref::<reqwest::Error>().and_then(reqwest::Error::status);
            let forbidden = status == Some(reqwest::StatusCode::FORBIDDEN);
            let terminal = if forbidden {
                // Resolve through the same slot and budget as the range workers.
                // Only the refreshed offer can proceed to the range stage.
                let renewed = tokio::select! {
                    biased;
                    () = shared.until_stopped() => false,
                    renewed = slot.renew(0) => renewed,
                };
                let original_https = reqwest::Url::parse(&url).is_ok_and(|url| url.scheme() == "https");
                let downgrade = if renewed && original_https {
                    let current = slot.url.lock().await;
                    !matches!(&current.media, Media::File(fresh) | Media::CdnFile(fresh, _)
                        if reqwest::Url::parse(fresh).is_ok_and(|url| url.scheme() == "https"))
                } else {
                    false
                };
                !renewed || downgrade
            } else {
                status == Some(reqwest::StatusCode::UNAUTHORIZED)
            };
            if terminal {
                shared.fail(format!("continuous file: {e:#}"), forbidden);
            } else {
                log::debug!("continuous file: {e:#}; resuming with ranges");
            }
        }
        let finished = {
            let inner = shared.inner.lock().unwrap();
            inner.cancelled || inner.error.is_some() || inner.fetched == inner.blocks.len()
        };
        if finished {
            shared.done.send_replace(true);
            return;
        }
    }

    let mut set = tokio::task::JoinSet::new();
    for _ in 0..WORKERS {
        let (http, ua, shared, slot, ahead) = (http.clone(), ua.clone(), shared.clone(), slot.clone(), ahead.clone());
        set.spawn(async move {
            let mut changes = slot.changes.as_ref().map(tokio::sync::watch::Sender::subscribe);
            loop {
                if let Some(ahead) = &ahead {
                    while ahead().is_some_and(|s| !Arc::ptr_eq(&s, &shared) && s.filling()) {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                }
                // Registered before claiming, so a change in between isn't missed.
                let changed = shared.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let idx = match shared.claim() {
                    Claim::Block(idx) => idx,
                    Claim::Wait => {
                        changed.await;
                        continue;
                    }
                    Claim::Done => break,
                };
                let (a, b) = if segments.is_none() { shared.range(idx) } else { (0, 0) };
                let what = if segments.is_some() { format!("segment {idx}") } else { format!("range {a}-{b}") };
                let mut attempt = 0;
                let mut data = BytesMut::new();
                let mut data_version = 0;
                let mut unavailable = false;
                let mut used_fallback = false;
                loop {
                    let (url, range, version, client) = {
                        let current = slot.url.lock().await;
                        // A suffix may only resume against the URL generation that
                        // supplied the partial block.
                        if data_version != current.version {
                            data.clear();
                            data_version = current.version;
                            unavailable = false;
                        }
                        if current.fallback_host && !used_fallback {
                            attempt = 0;
                            used_fallback = true;
                        }
                        let start = a + data.len() as u64;
                        let (url, range) = match &current.media {
                            Media::File(url) | Media::CdnFile(url, _) => (format!("{url}&range={start}-{b}"), Range::Param(start, b)),
                            Media::Feed(url) => (url.clone(), Range::Header(start, b)),
                            Media::Segments(urls) => (urls[idx].clone(), Range::Whole),
                        };
                        // A 403 resolution may move the app's proxy circuit; a
                        // fallback URL itself is tried on the same client/circuit.
                        let client = if cdn_file { current.client.clone() }
                            else if current.refreshes > 0 { crate::proxy::http() }
                            else { http.clone() };
                        (url, range, current.version, client)
                    };
                    let res = if idx >= FAIL_FROM.load(std::sync::atomic::Ordering::Relaxed) {
                        Err(anyhow::anyhow!("simulated network failure"))
                    } else if segments.is_none() && idx >= REFUSE_FROM.load(std::sync::atomic::Ordering::Relaxed) {
                        Err(anyhow::Error::new(SimulatedRefusal))
                    } else {
                        let response = fetch_observed(&client, &url, &ua, range, shared.total, &mut data, STALL, Some(&shared));
                        if let Some(changes) = &mut changes {
                            tokio::select! {
                                biased;
                                _ = changes.wait_for(|&current| current != version) => continue,
                                response = response => response,
                            }
                        } else {
                            response.await
                        }
                    };
                    let res = match res {
                        Ok(()) if !cdn_file => {
                            shared.put(idx, data.freeze(), false);
                            break;
                        }
                        result => result,
                    };
                    if res.as_ref().is_err_and(is_forbidden) {
                        let renew = slot.renew(version);
                        let renewed = if let Some(changes) = &mut changes {
                            tokio::select! {
                                biased;
                                _ = changes.wait_for(|&current| current != version) => true,
                                renewed = renew => renewed,
                            }
                        } else {
                            renew.await
                        };
                        if renewed {
                            continue;
                        }
                    }
                    let attempt_wait = {
                        let mut current = slot.url.lock().await;
                        // Serialize delivery and failure against host switching:
                        // an old worker may have completed just before its cancel.
                        if cdn_file && current.version != version {
                            continue;
                        }
                        match res {
                            Ok(()) => {
                                shared.put(idx, data.freeze(), current.fallback_host);
                                break;
                            }
                            Err(e) => {
                                let qualifies = cdn_file && qualifies_for_fallback(&e);
                                let mut waiting_for_fallback = false;
                                if qualifies && let Media::CdnFile(_, reserve) = &current.media {
                                    let (after, url) = (reserve.after.max(1), &reserve.url);
                                    if current.failures + 1 >= after {
                                        current.media = Media::File(url.clone());
                                        current.fallback_host = true;
                                        slot.advance(&mut current);
                                        continue;
                                    }
                                    current.failures += 1;
                                    waiting_for_fallback = true;
                                }

                                // Without an eligible reserve, keep the normal
                                // first-block 503 and per-block retry policy.
                                let refused = is_forbidden(&e);
                                let unavailable_now = playing && idx == 0 && !shared.feed && data.is_empty() && service_unavailable(&e);
                                if refused || (attempt >= RETRIES && !waiting_for_fallback) || (unavailable && unavailable_now && !waiting_for_fallback) {
                                    let refreshed = current.refreshes;
                                    let tail = if refused { format!(" after {refreshed} URL refresh(es)") } else { String::new() };
                                    shared.fail(format!("{what}: {e:#}{tail}"), refused);
                                    return;
                                }
                                unavailable = unavailable_now;
                                attempt = (attempt + 1).min(RETRIES);
                                log::warn!("{what}: {e:#} (retry {attempt})");
                                Duration::from_millis(400 * (attempt * attempt) as u64)
                            }
                        }
                    };
                    if let Some(changes) = &mut changes {
                        tokio::select! {
                            _ = changes.wait_for(|&current| current != version) => {}
                            _ = tokio::time::sleep(attempt_wait) => {}
                        }
                    } else {
                        tokio::time::sleep(attempt_wait).await;
                    }
                }
            }
        });
    }
    loop {
        tokio::select! {
            _ = shared.until_stopped() => {
                set.abort_all();
                break;
            }
            result = set.join_next() => match result {
                Some(Err(e)) => shared.fail(format!("stream worker stopped: {e}"), false),
                Some(Ok(())) => {}
                None => break,
            }
        }
    }
    // Release in-flight responses and refresh futures before marking a stop done.
    while set.join_next().await.is_some() {}
    shared.done.send_replace(true);
}

fn service_unavailable(error: &anyhow::Error) -> bool {
    error.downcast_ref::<reqwest::Error>().and_then(reqwest::Error::status)
        == Some(reqwest::StatusCode::SERVICE_UNAVAILABLE)
}

fn qualifies_for_fallback(error: &anyhow::Error) -> bool {
    service_unavailable(error) || error.downcast_ref::<AttemptTimeout>().is_some_and(|e| !e.received)
}

/// Classify only stalling attempts with no bytes, even when HTTP framing rules
/// require discarding bytes that did arrive before the timeout.
#[derive(Debug)]
struct AttemptTimeout {
    idle: Duration,
    received: bool,
    source: Option<reqwest::Error>,
}

impl std::fmt::Display for AttemptTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(source) = &self.source {
            return write!(f, "{source}");
        }
        write!(f, "no data for {} s", self.idle.as_secs())
    }
}

impl std::error::Error for AttemptTimeout {}
fn is_forbidden(e: &anyhow::Error) -> bool {
    e.is::<SimulatedRefusal>()
        || e.downcast_ref::<reqwest::Error>()
            .and_then(reqwest::Error::status)
            .is_some_and(|s| s == reqwest::StatusCode::FORBIDDEN)
}

/// `YTM_STREAM_REFUSE`'s answer: what googlevideo says to a range it won't serve.
#[derive(Debug)]
struct SimulatedRefusal;

impl std::fmt::Display for SimulatedRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("HTTP status client error (403 Forbidden) (simulated)")
    }
}

impl std::error::Error for SimulatedRefusal {}

/// Dev hook: YTM_STREAM_FAIL=<block> makes the first download fail from that block on,
/// as if the connection died mid-song; later downloads work.
static FAIL_FROM: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX);

/// Dev hook: YTM_STREAM_REFUSE=<block> answers every file stream's ranges from that
/// block on with HTTP 403, fresh URLs too, as YouTube does past 1 MiB (in block 4) where
/// it wants a PO token for the visionOS player's URLs. HLS segments get through.
static REFUSE_FROM: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX);

pub fn init_dev_hooks() {
    if let Some(b) = std::env::var("YTM_STREAM_FAIL").ok().and_then(|v| v.parse().ok()) {
        FAIL_FROM.store(b, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(b) = std::env::var("YTM_STREAM_REFUSE").ok().and_then(|v| v.parse().ok()) {
        REFUSE_FROM.store(b, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The bytes `a`-`b` a request asks for, and how.
#[derive(Clone, Copy)]
enum Range {
    /// googlevideo's `&range=a-b`, already in the URL.
    Param(u64, u64),
    /// An HTTP `Range` header, which the server must honor (206).
    Header(u64, u64),
    /// A whole segment, its length unknown.
    Whole,
}

/// A concrete byte range, including the representation's total size.
fn content_range(value: &str) -> Option<(u64, u64, u64)> {
    let (unit, value) = value.split_once(' ')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (range, total) = value.split_once('/')?;
    let (a, b) = range.split_once('-')?;
    let parse = |s: &str| -> Option<u64> {
        if !s.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    };
    let (a, b, total) = (parse(a)?, parse(b)?, parse(total)?);
    (a <= b && b < total).then_some((a, b, total))
}

fn response_range(resp: &reqwest::Response) -> Option<(u64, u64, u64)> {
    let headers = resp.headers().get_all(reqwest::header::CONTENT_RANGE);
    let mut headers = headers.iter();
    let value = headers.next()?.to_str().ok()?;
    if headers.next().is_some() {
        return None;
    }
    content_range(value)
}

#[cfg(test)]
async fn fetch(http: &reqwest::Client, url: &str, ua: &str, range: Range, total: u64, buf: &mut BytesMut, idle: Duration) -> anyhow::Result<()> {
    fetch_observed(http, url, ua, range, total, buf, idle, None).await
}

/// Append to a block's verified prefix. An interrupted segment starts over.
#[allow(clippy::too_many_arguments)]
async fn fetch_observed(http: &reqwest::Client, url: &str, ua: &str, range: Range, total: u64, buf: &mut BytesMut, idle: Duration, cache: Option<&Shared>) -> anyhow::Result<()> {
    let stalled = |received| AttemptTimeout { idle, received, source: None };
    let request_error = |error: reqwest::Error, received| {
        let error = error.without_url();
        if error.is_timeout() && !error.is_connect() {
            anyhow::Error::new(AttemptTimeout { idle, received, source: Some(error) })
        } else {
            error.into()
        }
    };
    let mut req = http.get(url).header("User-Agent", ua);
    if !matches!(range, Range::Whole) {
        req = req.header("Accept-Encoding", "identity");
    }
    if let Range::Header(a, b) = range {
        req = req.header("Range", format!("bytes={a}-{b}"));
    }
    // Errors leave the URL out: it carries the client's IP address and signatures (logs).
    let mut resp = tokio::time::timeout(idle, req.send())
        .await
        .map_err(|_| stalled(false))?
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| request_error(error, false))?;
    let status = resp.status();
    let partial = status == reqwest::StatusCode::PARTIAL_CONTENT;
    let ok = status == reqwest::StatusCode::OK;
    let (expect, resume) = match range {
        Range::Param(a, b) | Range::Header(a, b) => {
            anyhow::ensure!(partial || (matches!(range, Range::Param(..)) && ok), "the server did not honor the range ({status})");
            let has_range = resp.headers().contains_key(reqwest::header::CONTENT_RANGE);
            // googlevideo also returns 200 for query ranges, without Content-Range.
            if partial || has_range {
                anyhow::ensure!(response_range(&resp) == Some((a, b, total)), "the server sent a different byte range");
            }
            let expect = b - a + 1;
            let length = resp.content_length();
            anyhow::ensure!(length.is_none_or(|n| n == expect), "the server sent a different range length");
            buf.reserve(expect as usize);
            (Some(expect), has_range || length.is_some())
        }
        Range::Whole => {
            anyhow::ensure!(ok && !resp.headers().contains_key(reqwest::header::CONTENT_RANGE), "the server did not send a whole segment ({status})");
            (None, false)
        }
    };
    let cache_policy = cache.and_then(|shared| shared.observed_cache_until(resp.headers()).map(|until| (shared, until)));
    let base = buf.len();
    let result = async {
        let mut received = false;
        while let Some(chunk) = tokio::time::timeout(idle, resp.chunk()).await
            .map_err(|_| stalled(received))?.map_err(|error| request_error(error, received))?
        {
            received |= !chunk.is_empty();
            if expect.is_some_and(|n| chunk.len() as u64 > n - (buf.len() - base) as u64) {
                buf.truncate(base);
                anyhow::bail!("the server sent more bytes than requested");
            }
            buf.extend_from_slice(&chunk);
        }
        if let Some(expect) = expect {
            anyhow::ensure!((buf.len() - base) as u64 == expect, "short read: {} of {expect} bytes", buf.len() - base);
        } else {
            anyhow::ensure!(buf.len() > base, "the server sent an empty segment");
        }
        Ok(())
    }.await;
    // An unverified prefix or an exact-length body with broken framing can't be
    // resumed. In the latter case retrying the suffix would also invert the range.
    if result.is_err() && (!resume || expect == Some((buf.len() - base) as u64)) {
        buf.truncate(base);
    }
    // Also account for a verified prefix that will be kept after a short body.
    if buf.len() > base && let Some((shared, until)) = cache_policy {
        shared.cache_until.fetch_min(until, Ordering::Relaxed);
    }
    result
}

/// Where a podcast feed's file ends up after its redirects (trackers, mostly), and its
/// size: asked with a one-byte range, which also shows the server serves ranges.
pub async fn probe_feed(http: &reqwest::Client, url: &str, ua: &str) -> anyhow::Result<(String, u64)> {
    let resp = tokio::time::timeout(STALL, http.get(url).header("User-Agent", ua).header("Range", "bytes=0-0").header("Accept-Encoding", "identity").send())
        .await
        .map_err(|_| anyhow::anyhow!("the podcast's server didn't answer"))?
        .and_then(reqwest::Response::error_for_status)
        .map_err(reqwest::Error::without_url)?;
    anyhow::ensure!(resp.status() == reqwest::StatusCode::PARTIAL_CONTENT, "the podcast's server doesn't serve ranges ({})", resp.status());
    let total = response_range(&resp)
        .filter(|&(a, b, _)| a == 0 && b == 0 && resp.content_length().is_none_or(|n| n == 1))
        .map(|(_, _, total)| total)
        .ok_or_else(|| anyhow::anyhow!("the podcast's server didn't give the file's size"))?;
    Ok((resp.url().to_string(), total))
}

pub struct StreamReader {
    fallback_witness: Arc<AtomicBool>,
    shared: Arc<Shared>,
    pos: u64,
    id: u64,
    /// The block this reader last read.
    block: usize,
}

impl StreamReader {
    pub fn new(shared: Arc<Shared>) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        {
            let mut inner = shared.inner.lock().unwrap();
            inner.readers += 1;
            inner.limit = inner.blocks.len();
            inner.wants.push((id, 0));
        }
        shared.changed.notify_waiters();
        Self { shared, pos: 0, id, block: 0, fallback_witness: Arc::new(AtomicBool::new(false)) }
    }
}

impl Drop for StreamReader {
    fn drop(&mut self) {
        let mut inner = self.shared.inner.lock().unwrap();
        inner.readers -= 1;
        inner.wants.retain(|&(id, _)| id != self.id);
        if inner.readers == 0 {
            // Stops the workers, including their current requests.
            inner.cancelled = true;
        }
        self.shared.changed.notify_waiters();
    }
}

impl StreamReader {
    /// Block `idx`, once it's here; an error if the download failed or stopped first.
    pub fn block_at(&mut self, idx: usize) -> io::Result<Bytes> {
        let mut inner = self.shared.inner.lock().unwrap();
        inner.want = idx;
        if idx != self.block {
            self.block = idx;
            if let Some(w) = inner.wants.iter_mut().find(|(id, _)| *id == self.id) {
                w.1 = idx;
            }
            if inner.windowed || inner.blocks[idx].is_none() {
                self.shared.changed.notify_waiters();
            }
        }
        loop {
            if let Some(b) = &inner.blocks[idx] {
                if !inner.cancelled && inner.fallback_blocks.as_ref().is_some_and(|blocks| blocks[idx / 64] & (1 << (idx % 64)) != 0) {
                    self.fallback_witness.store(true, Ordering::Relaxed);
                    #[cfg(test)]
                    self.shared.used_fallback_host.store(true, Ordering::Relaxed);
                }
                return Ok(b.clone());
            }
            if let Some(e) = &inner.error {
                return Err(io::Error::other(e.clone()));
            }
            if inner.cancelled {
                return Err(io::Error::other("download cancelled"));
            }
            inner = self.shared.ready.wait(inner).unwrap();
        }
    }
}

impl Read for StreamReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.pos >= self.shared.total {
            return Ok(0);
        }
        let idx = block_of(self.pos);
        let block = self.block_at(idx)?;
        let off = (self.pos - block_start(idx)) as usize;
        let n = out.len().min(block.len() - off);
        out[..n].copy_from_slice(&block[off..off + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for StreamReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let total = self.shared.total as i64;
        let new = match to {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(off) => total + off,
            SeekFrom::Current(off) => self.pos as i64 + off,
        };
        if new < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

/// Open a decoder over `shared`. Non-seekable AAC decoders start after the first
/// fragment; seekable ones index every fragment first (i.e. effectively wait for the
/// download). Opus ones are always seekable: WebM's index is at the start.
pub fn decoder(shared: Arc<Shared>, seekable: bool) -> anyhow::Result<TrackDecoder> {
    let reader = StreamReader::new(shared.clone());
    let fallback_witness = reader.fallback_witness.clone();
    let decoder = if let Some(starts) = shared.segments.clone() {
        DecoderKind::Hls(Box::new(crate::hls::HlsSource::new(reader, starts, shared.duration_ms)?))
    } else if shared.opus {
        DecoderKind::Opus(Box::new(crate::opus::OpusSource::new(reader, shared.total, shared.duration_ms)?))
    } else {
        // A feed's file is probed for what it is: MP3, or AAC in an MP4 whose index may
        // follow the audio (only a seekable reader gets to it). Without a VBR header, an
        // MP3's length is guessed from its first frames, and gapless trimming would cut
        // whatever runs past the guess.
        let builder = rodio::Decoder::builder().with_data(reader).with_byte_len(shared.total).with_seekable(seekable || shared.feed);
        let builder = if shared.feed { builder.with_coarse_seek(true).with_gapless(false) } else { builder.with_mime_type("audio/mp4").with_hint("m4a") };
        DecoderKind::Aac(builder.build()?)
    };
    Ok(TrackDecoder { decoder, fallback_witness })
}

/// A song's decoder and the fallback witness for the reader it actually owns.
pub struct TrackDecoder {
    decoder: DecoderKind,
    fallback_witness: Arc<AtomicBool>,
}

impl TrackDecoder {
    pub(crate) fn fallback_witness(&self) -> Arc<AtomicBool> {
        self.fallback_witness.clone()
    }
}

enum DecoderKind {
    Aac(rodio::Decoder<StreamReader>),
    /// Boxed: its decoder state is about 11 KB.
    Opus(Box<crate::opus::OpusSource>),
    /// AAC from HLS segments.
    Hls(Box<crate::hls::HlsSource>),
}

impl Iterator for TrackDecoder {
    type Item = rodio::Sample;

    fn next(&mut self) -> Option<rodio::Sample> {
        match &mut self.decoder {
            DecoderKind::Aac(d) => d.next(),
            DecoderKind::Opus(d) => d.next(),
            DecoderKind::Hls(d) => d.next(),
        }
    }
}

impl rodio::Source for TrackDecoder {
    fn current_span_len(&self) -> Option<usize> {
        match &self.decoder {
            DecoderKind::Aac(d) => d.current_span_len(),
            DecoderKind::Opus(d) => d.current_span_len(),
            DecoderKind::Hls(d) => d.current_span_len(),
        }
    }
    fn channels(&self) -> rodio::ChannelCount {
        match &self.decoder {
            DecoderKind::Aac(d) => d.channels(),
            DecoderKind::Opus(d) => d.channels(),
            DecoderKind::Hls(d) => d.channels(),
        }
    }
    fn sample_rate(&self) -> rodio::SampleRate {
        match &self.decoder {
            DecoderKind::Aac(d) => d.sample_rate(),
            DecoderKind::Opus(d) => d.sample_rate(),
            DecoderKind::Hls(d) => d.sample_rate(),
        }
    }
    fn total_duration(&self) -> Option<Duration> {
        match &self.decoder {
            DecoderKind::Aac(d) => d.total_duration(),
            DecoderKind::Opus(d) => d.total_duration(),
            DecoderKind::Hls(d) => d.total_duration(),
        }
    }
    fn try_seek(&mut self, pos: Duration) -> Result<(), rodio::source::SeekError> {
        match &mut self.decoder {
            DecoderKind::Aac(d) => d.try_seek(pos),
            DecoderKind::Opus(d) => d.try_seek(pos),
            DecoderKind::Hls(d) => d.try_seek(pos),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use rodio::Source;

    /// A real podcast's newest episode played the way an RSS episode is: found past its
    /// tracking redirects, downloaded in `Range` requests, decoded as MP3, seeked.
    /// `cargo test --release -- --ignored feed_file_live --nocapture`
    #[tokio::test]
    #[ignore]
    async fn feed_file_live() {
        let (http, ua) = (reqwest::Client::new(), crate::webmusic::UA);
        let rss = http.get("https://feeds.npr.org/510289/podcast.xml").header("User-Agent", ua).send().await.unwrap().text().await.unwrap();
        let tag = &rss[rss.find("<enclosure").unwrap()..];
        let url = tag[tag.find(" url=\"").unwrap() + 6..].split('"').next().unwrap().replace("&amp;", "&");
        let (file, size) = super::probe_feed(&http, &url, ua).await.unwrap();
        println!("{url}\n  -> {file}: {size} bytes");
        let shared = super::Shared::feed(size, 0, false);
        let t = Instant::now();
        super::download(http, super::Media::Feed(file), ua.to_owned(), shared.clone(), None).await;
        assert!(shared.error().is_none(), "{:?}", shared.error());
        println!("  downloaded in {:?}", t.elapsed());
        let mut d = super::decoder(shared.clone(), true).unwrap();
        let (ch, rate) = (d.channels().get() as usize, d.sample_rate().get() as usize);
        let secs = d.by_ref().count() as f64 / (ch * rate) as f64;
        println!("  {ch} ch {rate} Hz, {secs:.1} s");
        assert!(secs > 60.0, "too short: {secs} s");
        let mut d = super::decoder(shared, true).unwrap();
        let half = Duration::from_secs_f64(secs / 2.0);
        let t = Instant::now();
        d.try_seek(half).unwrap();
        println!("  seek to {:.1} s took {:?}", half.as_secs_f64(), t.elapsed());
        let left = d.count() as f64 / (ch * rate) as f64;
        println!("  {left:.1} s left");
        // Coarse: an estimate from the bitrate.
        assert!((left - (secs - half.as_secs_f64())).abs() < 5.0, "seek off");
    }

    mod audio_cache {
        use super::super::*;

        fn policy(value: &'static str) -> HeaderMap {
            let mut headers = HeaderMap::new();
            headers.insert(CACHE_CONTROL, reqwest::header::HeaderValue::from_static(value));
            headers
        }

        #[test]
        fn restores_a_complete_opus_file_and_keeps_its_seek_bytes() {
            let size = WEB_PREVIEW + 11;
            let bytes: Vec<_> = (0..size).map(|offset| (offset % 251) as u8).collect();
            let shared = Shared::new(size, 117_000, Some(-3.5), true, true);
            assert!(shared.restore_cached_file(bytes));
            assert!(shared.opus && !shared.saved && shared.is_complete());
            assert_eq!((shared.duration_ms, shared.loudness_db), (117_000, Some(-3.5)));
            assert!(*shared.done.borrow());
            shared.note_cache_policy(&policy("max-age=60"));
            assert!(shared.claim_cache_blocks().is_none());

            {
                let inner = shared.inner.lock().unwrap();
                assert!(inner.verified_past_web_preview);
                let first = inner.blocks[0].as_ref().unwrap().as_ptr();
                for (index, block) in inner.blocks.iter().enumerate() {
                    assert_eq!(block.as_ref().unwrap().as_ptr(), first.wrapping_add(block_start(index) as usize));
                }
            }

            let mut reader = StreamReader::new(shared);
            for (at, len) in [(FIRST - 4, 8), (WEB_PREVIEW, 11), (FIRST + 100, 8)] {
                reader.seek(SeekFrom::Start(at)).unwrap();
                let mut bytes = vec![0; len];
                reader.read_exact(&mut bytes).unwrap();
                assert!(bytes.into_iter().eq((at..at + len as u64).map(|offset| (offset % 251) as u8)));
            }
            assert!(!reader.fallback_witness.load(Ordering::Relaxed));
        }

        #[test]
        fn restoration_rejects_wrong_sizes_partial_or_claimed_buffers() {
            let shared = Shared::new(FIRST + 1, 0, None, false, true);
            assert!(!shared.restore_cached_file(vec![0; FIRST as usize]));
            assert!(matches!(shared.claim(), Claim::Block(0)));
            assert!(!shared.restore_cached_file(vec![0; FIRST as usize + 1]));
            shared.put(0, Bytes::from(vec![1; FIRST as usize]), false);
            assert!(!shared.restore_cached_file(vec![0; FIRST as usize + 1]));
            assert_eq!(shared.inner.lock().unwrap().blocks[0].as_ref().unwrap()[0], 1);
        }

        #[test]
        fn only_live_complete_blocks_with_a_current_policy_can_be_claimed_once() {
            let shared = Shared::new(FIRST + 1, 0, None, false, false);
            let head = Bytes::from(vec![0; FIRST as usize]);
            shared.note_cache_policy(&policy("public, max-age=60"));
            shared.put(0, head.clone(), false);
            assert!(shared.claim_cache_blocks().is_none());
            shared.put(1, Bytes::from_static(b"x"), false);
            let (blocks, until) = shared.claim_cache_blocks().unwrap();
            assert!(until > unix_seconds().unwrap());
            assert_eq!(blocks.len(), 2);
            assert_eq!(blocks[0].as_ptr(), head.as_ptr());
            assert_eq!(blocks[1].as_ref(), b"x");
            assert!(shared.claim_cache_blocks().is_none());

            for cancel in [false, true] {
                let stopped = Shared::new(1, 0, None, false, false);
                stopped.note_cache_policy(&policy("max-age=60"));
                stopped.put(0, Bytes::from_static(b"x"), false);
                if cancel {
                    stopped.cancel();
                } else {
                    stopped.fail("test stream stopped".into(), false);
                }
                assert!(stopped.claim_cache_blocks().is_none());
            }

            let no_policy = Shared::new(1, 0, None, false, false);
            no_policy.put(0, Bytes::from_static(b"x"), false);
            assert!(no_policy.claim_cache_blocks().is_none());
            no_policy.cache_until.store(unix_seconds().unwrap(), Ordering::Relaxed);
            assert!(no_policy.claim_cache_blocks().is_none());
        }

        #[test]
        fn cache_policy_uses_the_greater_of_age_and_apparent_age() {
            use reqwest::header::HeaderValue;

            let mut headers = policy("max-age=60");
            let date = HeaderValue::from_static("Thu, 01 Jan 1970 00:01:00 GMT");
            headers.insert(DATE, date.clone());
            assert_eq!(cache_policy_until(&headers, 100), Some(120));
            assert!(cache_policy_until(&headers, 120).is_none());
            assert_eq!(cache_policy_until(&headers, 50), Some(110));

            headers.insert(AGE, HeaderValue::from_static("30"));
            assert_eq!(cache_policy_until(&headers, 100), Some(120));
            headers.insert(AGE, HeaderValue::from_static("50"));
            assert_eq!(cache_policy_until(&headers, 100), Some(110));

            headers.append(DATE, date);
            assert!(cache_policy_until(&headers, 100).is_none());
            headers.insert(DATE, HeaderValue::from_static("invalid"));
            assert!(cache_policy_until(&headers, 100).is_none());
        }

        #[test]
        fn cache_policy_accepts_an_explicit_fresh_expires_without_max_age() {
            use reqwest::header::HeaderValue;

            let mut headers = HeaderMap::new();
            let expires = HeaderValue::from_static("Thu, 01 Jan 1970 00:02:00 GMT");
            headers.insert(EXPIRES, expires.clone());
            assert_eq!(cache_policy_until(&headers, 100), Some(120));
            assert!(cache_policy_until(&headers, 120).is_none());
            headers.insert(DATE, HeaderValue::from_static("Thu, 01 Jan 1970 00:01:00 GMT"));
            headers.insert(AGE, HeaderValue::from_static("50"));
            assert_eq!(cache_policy_until(&headers, 100), Some(110));

            headers.append(EXPIRES, expires);
            assert!(cache_policy_until(&headers, 100).is_none());
            headers.insert(EXPIRES, HeaderValue::from_static("invalid"));
            assert!(cache_policy_until(&headers, 100).is_none());
            headers.remove(AGE);
            headers.remove(DATE);
            headers.insert(EXPIRES, HeaderValue::from_static("Sun, 04 Jan 1970 00:00:00 GMT"));
            assert_eq!(cache_policy_until(&headers, 100), Some(100 + 86400));

            for policy in ["no-store", "no-cache"] {
                headers.insert(CACHE_CONTROL, HeaderValue::from_static(policy));
                assert!(cache_policy_until(&headers, 100).is_none());
            }
            headers.insert(CACHE_CONTROL, HeaderValue::from_static("max-age=60"));
            headers.insert(EXPIRES, HeaderValue::from_static("invalid"));
            assert_eq!(cache_policy_until(&headers, 100), Some(160));
        }

        #[test]
        fn cache_policy_uses_age_caps_lifetime_and_rejects_ambiguous_headers() {
            use reqwest::header::HeaderValue;

            let mut headers = policy("public, MAX-AGE=100000");
            headers.insert(AGE, HeaderValue::from_static("10"));
            assert_eq!(cache_policy_until(&headers, 123), Some(123 + 86400));
            headers.insert(CACHE_CONTROL, HeaderValue::from_static("max-age=60"));
            assert_eq!(cache_policy_until(&headers, 123), Some(173));
            headers.insert(VARY, HeaderValue::from_static("Accept, *"));
            assert!(cache_policy_until(&headers, 123).is_none());

            for value in [
                "max-age=0", "max-age=600, no-store", "max-age=600, no-cache=\"X-Test\"",
                "max-age=60, max-age=20", "private", "extension=\"x,max-age=60\"",
            ] {
                assert!(cache_policy_until(&policy(value), 123).is_none(), "{value}");
            }
            headers.remove(VARY);
            for age in ["60", "61", "nan", "-1"] {
                headers.insert(AGE, HeaderValue::from_static(age));
                assert!(cache_policy_until(&headers, 123).is_none(), "{age}");
            }

            let shared = Shared::new(1, 0, None, false, false);
            shared.note_cache_policy(&policy("max-age=60"));
            let longer = shared.cache_until.load(Ordering::Relaxed);
            shared.note_cache_policy(&policy("max-age=12"));
            assert!(shared.cache_until.load(Ordering::Relaxed) < longer);
            shared.note_cache_policy(&policy("no-store"));
            shared.note_cache_policy(&policy("max-age=60"));
            assert_eq!(shared.cache_until.load(Ordering::Relaxed), 0);
        }
    }

    mod web_preview {
        use std::future::Future;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::{Context, Poll, Wake, Waker};

        use bytes::Bytes;

        use super::super::{Shared, WEB_PREVIEW, block_count, block_of};

        const KIB: u64 = 1024;

        #[derive(Default)]
        struct Notified(AtomicBool);

        impl Wake for Notified {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::Relaxed);
            }
        }

        fn put_block(shared: &Shared, index: usize) {
            let (first, last) = shared.range(index);
            shared.put(index, Bytes::from(vec![0; (last - first + 1) as usize]), false);
        }

        #[test]
        fn past_web_preview_waits_for_the_verified_boundary_block() {
            let shared = Shared::new(1088 * KIB, 0, None, false, false);
            assert_eq!(shared.range(0), (0, 64 * KIB - 1));
            assert_eq!(block_of(WEB_PREVIEW), 4);
            assert_eq!(shared.range(4), (832 * KIB, 1088 * KIB - 1));

            let notified = Arc::new(Notified::default());
            let waker = Waker::from(notified.clone());
            let mut cx = Context::from_waker(&waker);
            let mut check = std::pin::pin!(shared.past_web_preview());
            assert!(check.as_mut().poll(&mut cx).is_pending());
            for index in 0..4 {
                put_block(&shared, index);
                assert!(check.as_mut().poll(&mut cx).is_pending());
            }
            assert!(!notified.0.load(Ordering::Relaxed));

            put_block(&shared, 4);
            assert!(notified.0.load(Ordering::Relaxed));
            assert!(matches!(check.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
        }

        #[test]
        fn past_web_preview_accepts_a_later_block_without_the_boundary_block() {
            let shared = Shared::new(48 * 1024 * KIB, 0, None, false, false);
            let notified = Arc::new(Notified::default());
            let waker = Waker::from(notified.clone());
            let mut cx = Context::from_waker(&waker);
            let mut check = std::pin::pin!(shared.past_web_preview());
            assert!(check.as_mut().poll(&mut cx).is_pending());

            // A far seek can fetch this block before those near the beginning.
            put_block(&shared, 100);
            assert!(notified.0.swap(false, Ordering::Relaxed));
            assert!(matches!(check.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));

            let mut changed = std::pin::pin!(shared.changed.notified());
            assert!(changed.as_mut().poll(&mut cx).is_pending());
            put_block(&shared, 4);
            assert!(!notified.0.load(Ordering::Relaxed));
        }

        #[test]
        fn past_web_preview_wakes_and_fails_on_cancel_or_error() {
            for cancel in [true, false] {
                let shared = Shared::new(WEB_PREVIEW + 1, 0, None, false, false);
                let notified = Arc::new(Notified::default());
                let waker = Waker::from(notified.clone());
                let mut cx = Context::from_waker(&waker);
                let mut check = std::pin::pin!(shared.past_web_preview());
                assert!(check.as_mut().poll(&mut cx).is_pending());

                if cancel {
                    shared.cancel();
                } else {
                    shared.fail("fixture range refused".into(), true);
                }
                assert!(notified.0.load(Ordering::Relaxed));
                assert!(matches!(check.as_mut().poll(&mut cx), Poll::Ready(Err(_))));
            }
        }

        #[test]
        fn past_web_preview_accepts_complete_files_up_to_the_limit() {
            for size in [64 * KIB, WEB_PREVIEW] {
                let shared = Shared::new(size, 0, None, false, false);
                for index in 0..block_count(size) {
                    put_block(&shared, index);
                }
                assert!(shared.is_complete());
                let mut check = std::pin::pin!(shared.past_web_preview());
                let mut cx = Context::from_waker(Waker::noop());
                assert!(matches!(check.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
            }
        }
        #[test]
        fn past_web_preview_checks_for_stops_before_short_or_verified_success() {
            for size in [64 * KIB, WEB_PREVIEW + 1] {
                for cancel in [true, false] {
                    let shared = Shared::new(size, 0, None, false, false);
                    // A full short file or the block spanning the preview boundary.
                    put_block(&shared, block_count(size) - 1);
                    if cancel {
                        shared.cancel();
                    } else {
                        shared.fail("fixture range refused".into(), true);
                    }
                    let mut check = std::pin::pin!(shared.past_web_preview());
                    let mut cx = Context::from_waker(Waker::noop());
                    assert!(matches!(check.as_mut().poll(&mut cx), Poll::Ready(Err(_))));
                }
            }
        }

        #[test]
        fn past_web_preview_accepts_a_saved_file_over_the_limit() {
            let shared = Shared::from_bytes(vec![0; WEB_PREVIEW as usize + 1], 0, None);
            let mut check = std::pin::pin!(shared.past_web_preview());
            let mut cx = Context::from_waker(Waker::noop());
            assert!(matches!(check.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
        }
    }

    mod http {
        use super::super::*;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio::sync::{Notify, mpsc};
        use tokio::task::JoinHandle;
        use tokio::time::timeout;

        const WAIT: Duration = Duration::from_secs(4);

        enum Reply {
            Bytes(String),
            Stall,
            NoHeaders,
            Drip,
        }

        fn reply(status: &str, headers: &str, body: &str) -> Reply {
            Reply::Bytes(format!("HTTP/1.1 {status}\r\n{headers}Connection: close\r\n\r\n{body}"))
        }

        struct Server {
            url: String,
            requests: mpsc::UnboundedReceiver<String>,
            task: JoinHandle<()>,
        }

        impl Server {
            async fn new(replies: Vec<Reply>) -> Self {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!("http://{}/audio?test=1", listener.local_addr().unwrap());
                let (sent, requests) = mpsc::unbounded_channel();
                let task = tokio::spawn(async move {
                    for reply in replies {
                        let (mut socket, _) = listener.accept().await.unwrap();
                        let mut request = Vec::new();
                        while !request.ends_with(b"\r\n\r\n") {
                            let mut chunk = [0; 1024];
                            let n = socket.read(&mut chunk).await.unwrap();
                            assert!(n > 0 && request.len() < 16 * 1024);
                            request.extend_from_slice(&chunk[..n]);
                        }
                        match &reply {
                            Reply::Bytes(bytes) => {
                                socket.write_all(bytes.as_bytes()).await.unwrap();
                            }
                            Reply::NoHeaders => {}
                            Reply::Stall | Reply::Drip => {
                                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nA").await.unwrap();
                            }
                        }
                        sent.send(String::from_utf8(request).unwrap()).unwrap();
                        match reply {
                            Reply::Bytes(_) => {}
                            Reply::Stall | Reply::NoHeaders => {
                                // Dropping the in-flight HTTP response closes its socket.
                                let result = socket.read(&mut [0]).await;
                                assert!(matches!(result, Ok(0)) || result.is_err());
                            }
                            Reply::Drip => {
                                for byte in b"BCDEFG" {
                                    tokio::time::sleep(Duration::from_millis(100)).await;
                                    socket.write_all(&[*byte]).await.unwrap();
                                }
                            }
                        }
                    }
                });
                Self { url, requests, task }
            }

            async fn request(&mut self) -> String {
                timeout(WAIT, self.requests.recv()).await.unwrap().expect("missing request")
            }

            async fn finish(&mut self) {
                timeout(WAIT, &mut self.task).await.unwrap().unwrap();
            }
        }

        impl Drop for Server {
            fn drop(&mut self) {
                self.task.abort();
            }
        }

        fn client() -> reqwest::Client {
            reqwest::Client::builder().no_proxy().build().unwrap()
        }

        #[tokio::test]
        async fn interrupted_file_and_feed_resume_at_the_missing_byte() {
            for feed in [false, true] {
                let replies = if feed {
                    vec![
                        reply("206 Partial Content", "Content-Length: 6\r\nContent-Range: bytes 0-5/6\r\n", "ABC"),
                        reply("206 Partial Content", "Content-Length: 3\r\nContent-Range: bytes 3-5/6\r\n", "DEF"),
                    ]
                } else {
                    vec![
                        reply("200 OK", "Content-Length: 6\r\n", "ABC"),
                        reply("200 OK", "Content-Length: 3\r\n", "DEF"),
                    ]
                };
                let mut server = Server::new(replies).await;
                let (media, shared) = if feed {
                    (Media::Feed(server.url.clone()), Shared::feed(6, 0, false))
                } else {
                    (Media::File(server.url.clone()), Shared::new(6, 0, None, false, false))
                };
                timeout(WAIT, download(client(), media, "test".into(), shared.clone(), None)).await.unwrap();
                let first = server.request().await.to_ascii_lowercase();
                let second = server.request().await.to_ascii_lowercase();
                if feed {
                    assert!(first.contains("\r\nrange: bytes=0-5\r\n"));
                    assert!(second.contains("\r\nrange: bytes=3-5\r\n"));
                } else {
                    assert!(first.starts_with("get /audio?test=1&range=0-5 "));
                    assert!(second.starts_with("get /audio?test=1&range=3-5 "));
                }
                server.finish().await;
                assert_eq!(shared.bytes().unwrap(), b"ABCDEF");
                assert!(shared.error().is_none());
            }
        }

        #[tokio::test]
        async fn a_resumed_file_cannot_cache_bytes_from_a_no_store_prefix() {
            let mut server = Server::new(vec![
                reply("200 OK", "Content-Length: 6\r\nCache-Control: no-store\r\n", "ABC"),
                reply("200 OK", "Content-Length: 3\r\nCache-Control: max-age=60\r\n", "DEF"),
            ]).await;
            let media = Media::File(server.url.clone());
            let shared = Shared::new(6, 0, None, false, false);
            timeout(WAIT, download(client(), media, "test".into(), shared.clone(), None)).await.unwrap();
            assert!(server.request().await.starts_with("GET /audio?test=1&range=0-5 "));
            assert!(server.request().await.starts_with("GET /audio?test=1&range=3-5 "));
            server.finish().await;
            assert_eq!(shared.bytes().as_deref(), Some(b"ABCDEF".as_slice()));
            assert_eq!(shared.cache_until.load(Ordering::Relaxed), 0);
            assert!(shared.claim_cache_blocks().is_none());
        }

        #[tokio::test]
        async fn interrupted_hls_segment_restarts_whole() {
            let mut server = Server::new(vec![
                reply("200 OK", "Content-Length: 6\r\n", "bad"),
                reply("200 OK", "Content-Length: 6\r\n", "ABCDEF"),
            ]).await;
            let shared = Shared::segmented(vec![0.0], 1000, None, false);
            let media = Media::Segments(vec![server.url.clone()]);
            timeout(WAIT, download(client(), media, "test".into(), shared.clone(), None)).await.unwrap();
            let first = server.request().await;
            let second = server.request().await;
            assert_eq!(first.lines().next(), second.lines().next());
            assert!(!second.to_ascii_lowercase().contains("range"));
            server.finish().await;
            assert_eq!(shared.bytes().unwrap(), b"ABCDEF");
        }

        #[tokio::test]
        async fn invalid_ranges_and_framing_do_not_change_the_verified_prefix() {
            for (range, reply) in [
                (Range::Header(2, 5), reply("200 OK", "Content-Length: 4\r\n", "CDEF")),
                (Range::Header(2, 5), reply("206 Partial Content", "Content-Length: 4\r\n", "CDEF")),
                (Range::Header(2, 5), reply("206 Partial Content", "Content-Range: bytes 2-5/7\r\n", "CDEF")),
                (Range::Param(2, 5), reply("206 Partial Content", "Content-Range: bytes 1-4/6\r\n", "BCDE")),
                (Range::Param(2, 5), reply("206 Partial Content", "Content-Range: bytes 2-5/6\r\nContent-Range: bytes 1-4/6\r\n", "CDEF")),
                (Range::Param(2, 5), reply("200 OK", "Content-Length: 5\r\n", "CDEFG")),
                (Range::Param(2, 5), reply("200 OK", "Transfer-Encoding: chunked\r\n", "2\r\nCD\r\n3\r\nEFG\r\n0\r\n\r\n")),
                (Range::Param(2, 5), reply("200 OK", "Content-Range: bytes 2-5/6\r\nTransfer-Encoding: chunked\r\n", "4\r\nCDEF\r\n")),
                (Range::Param(2, 5), reply("200 OK", "", "CD")),
            ] {
                let mut server = Server::new(vec![reply]).await;
                let mut bytes = BytesMut::from(&b"AB"[..]);
                let result = timeout(WAIT, fetch(&client(), &server.url, "test", range, 6, &mut bytes, STALL)).await.unwrap();
                assert!(result.is_err());
                assert_eq!(&bytes[..], b"AB");
                server.finish().await;
            }
        }

        #[tokio::test]
        async fn feed_probe_checks_requested_byte_and_total() {
            for (range, valid) in [
                ("bytes 0-0/6", true),
                ("bytes 1-1/6", false),
                ("bytes 0-1/6", false),
                ("bytes 0-0/*", false),
                ("bytes 0-0/0", false),
            ] {
                let mut server = Server::new(vec![reply("206 Partial Content", &format!("Content-Length: 1\r\nContent-Range: {range}\r\n"), "A")]).await;
                let result = timeout(WAIT, probe_feed(&client(), &server.url, "test")).await.unwrap();
                if valid {
                    assert_eq!(result.unwrap(), (server.url.clone(), 6));
                } else {
                    assert!(result.is_err(), "accepted {range}");
                }
                server.finish().await;
            }
        }

        #[tokio::test]
        async fn idle_timeout_restarts_on_progress_and_retains_a_stalled_prefix() {
            let idle = Duration::from_millis(400);
            let mut server = Server::new(vec![Reply::Drip, Reply::Stall]).await;
            let mut bytes = BytesMut::new();
            let started = std::time::Instant::now();
            timeout(WAIT, fetch(&client(), &server.url, "test", Range::Param(0, 6), 7, &mut bytes, idle)).await.unwrap().unwrap();
            assert!(started.elapsed() > idle);
            assert_eq!(&bytes[..], b"ABCDEFG");
            bytes.clear();
            let result = timeout(WAIT, fetch(&client(), &server.url, "test", Range::Param(0, 6), 7, &mut bytes, idle)).await.unwrap();
            assert!(result.unwrap_err().to_string().starts_with("no data"));
            assert_eq!(&bytes[..], b"A");
            server.finish().await;
        }

        #[tokio::test]
        async fn last_reader_drop_cancels_an_in_flight_response() {
            let mut server = Server::new(vec![Reply::Stall]).await;
            let shared = Shared::new(7, 0, None, false, false);
            let reader = StreamReader::new(shared.clone());
            let dl = download(client(), Media::File(server.url.clone()), "test".into(), shared.clone(), None);
            timeout(WAIT, async {
                tokio::join!(dl, async {
                    server.request().await;
                    drop(reader);
                    shared.finished().await;
                });
            }).await.unwrap();
            server.finish().await;
            assert!(!shared.is_complete());
            assert!(shared.error().is_none());
        }

        #[tokio::test]
        async fn cancellation_drops_a_hanging_refresh_before_finishing() {
            let mut server = Server::new(vec![reply("403 Forbidden", "Content-Length: 0\r\n", "")]).await;
            let shared = Shared::new(6, 0, None, false, false);
            let entered = Arc::new(Notify::new());
            let (sent, mut dropped) = tokio::sync::oneshot::channel::<()>();
            let sent = Mutex::new(Some(sent));
            let refresh: Refresh = {
                let entered = entered.clone();
                Arc::new(move || {
                    let entered = entered.clone();
                    let sender = sent.lock().unwrap().take().unwrap();
                    Box::pin(async move {
                        entered.notify_one();
                        std::future::pending::<()>().await;
                        let _ = sender.send(());
                        unreachable!()
                    })
                })
            };
            let dl = download(client(), Media::File(server.url.clone()), "test".into(), shared.clone(), Some(refresh));
            timeout(WAIT, async {
                tokio::join!(dl, async {
                    entered.notified().await;
                    shared.cancel();
                    shared.finished().await;
                    assert_eq!(dropped.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Closed));
                });
            }).await.unwrap();
            server.finish().await;
            assert!(shared.error().is_none());
        }

        fn cdn_file(primary: &str, fallback: &str, after: u32) -> Media {
            Media::CdnFile(primary.to_owned(), CdnFallback { url: fallback.to_owned(), after })
        }

        #[tokio::test]
        async fn cdn_fallback_discards_old_prefix_and_validates_the_new_suffix() {
            let unavailable = || reply("503 Service Unavailable", "Content-Length: 0\r\n", "");
            let mut primary = Server::new(vec![
                reply("200 OK", "Content-Length: 6\r\n", "ABC"),
                unavailable(), unavailable(), unavailable(),
            ]).await;
            let mut fallback = Server::new(vec![
                reply("206 Partial Content", "Content-Length: 5\r\nContent-Range: bytes 1-5/6\r\n", "WRONG"),
                reply("200 OK", "Content-Length: 6\r\n", "UVW"),
                reply("206 Partial Content", "Content-Length: 3\r\nContent-Range: bytes 3-5/6\r\n", "XYZ"),
            ]).await;
            let shared = Shared::new(6, 0, None, false, false);
            let media = cdn_file(&primary.url, &fallback.url, 3);
            timeout(Duration::from_secs(14), async {
                tokio::join!(
                    download(client(), media, "test".into(), shared.clone(), None),
                    async {
                        assert!(primary.request().await.starts_with("GET /audio?test=1&range=0-5 "));
                        for _ in 0..3 {
                            assert!(primary.request().await.starts_with("GET /audio?test=1&range=3-5 "));
                        }
                        // The first alternate response has incorrect framing, and the
                        // next is truncated. Neither marks the fallback as received.
                        assert!(fallback.request().await.starts_with("GET /audio?test=1&range=0-5 "));
                        assert!(!shared.used_fallback_host());
                        assert!(fallback.request().await.starts_with("GET /audio?test=1&range=0-5 "));
                        assert!(!shared.used_fallback_host());
                        assert!(fallback.request().await.starts_with("GET /audio?test=1&range=3-5 "));
                    }
                );
            }).await.unwrap();
            primary.finish().await;
            fallback.finish().await;
            assert!(!shared.used_fallback_host());
            assert_eq!(StreamReader::new(shared.clone()).block_at(0).unwrap().as_ref(), b"UVWXYZ");
            assert!(shared.used_fallback_host());
        }

        async fn accept_range(listener: &TcpListener) -> (tokio::net::TcpStream, String) {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut chunk = [0; 1024];
                let n = socket.read(&mut chunk).await.unwrap();
                assert!(n > 0 && request.len() < 16 * 1024);
                request.extend_from_slice(&chunk[..n]);
            }
            (socket, String::from_utf8(request).unwrap())
        }

        #[tokio::test]
        async fn cdn_fallback_combines_worker_failures_and_restarts_stale_requests() {
            let primary = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let fallback = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let total = FIRST + BLOCK + 3;
            let shared = Shared::new(total, 0, None, false, false);
            let media = cdn_file(
                &format!("http://{}/audio?test=1", primary.local_addr().unwrap()),
                &format!("http://{}/audio?test=1", fallback.local_addr().unwrap()),
                2,
            );
            let expect_ranges: Vec<String> = (0..3).map(|i| {
                let (start, end) = shared.range(i);
                format!("GET /audio?test=1&range={start}-{end} ")
            }).collect();
            let serve_primary = async {
                let mut opened = Vec::new();
                for _ in 0..3 {
                    opened.push(accept_range(&primary).await);
                }
                for range in &expect_ranges {
                    assert_eq!(opened.iter().filter(|(_, request)| request.starts_with(range)).count(), 1);
                }
                let (mut held, _) = opened.pop().unwrap();
                for (mut socket, _) in opened {
                    socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                }
                // The third request never receives headers. Once two different
                // workers report 503, its connection must be dropped immediately.
                let closed = held.read(&mut [0]).await;
                assert!(matches!(closed, Ok(0)) || closed.is_err());
            };
            let serve_fallback = async {
                let mut seen = [false; 3];
                for _ in 0..3 {
                    let (mut socket, request) = accept_range(&fallback).await;
                    let index = expect_ranges.iter().position(|r| request.starts_with(r)).unwrap();
                    assert!(!std::mem::replace(&mut seen[index], true));
                    let (start, end) = shared.range(index);
                    let length = (end - start + 1) as usize;
                    let body = vec![b'A' + index as u8; length];
                    let headers = format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {length}\r\nContent-Range: bytes {start}-{end}/{total}\r\nConnection: close\r\n\r\n");
                    socket.write_all(headers.as_bytes()).await.unwrap();
                    socket.write_all(&body).await.unwrap();
                }
            };
            timeout(WAIT, async {
                tokio::join!(download(client(), media, "test".into(), shared.clone(), None), serve_primary, serve_fallback);
            }).await.unwrap();
            let bytes = shared.bytes().unwrap();
            assert!(bytes[..FIRST as usize].iter().all(|&b| b == b'A'));
            assert!(bytes[FIRST as usize..(FIRST + BLOCK) as usize].iter().all(|&b| b == b'B'));
            assert_eq!(&bytes[(FIRST + BLOCK) as usize..], b"CCC");
            assert!(!shared.used_fallback_host());
            assert_eq!(StreamReader::new(shared.clone()).block_at(2).unwrap().as_ref(), b"CCC");
            assert!(shared.used_fallback_host());
            assert!(!Shared::new(3, 0, None, false, false).used_fallback_host());
        }

        #[test]
        fn reserve_witness_does_not_leak_across_readers_of_one_stream() {
            let shared = Shared::new(FIRST + 3, 0, None, false, false);
            let mut playing = StreamReader::new(shared.clone());
            let mut background = StreamReader::new(shared.clone());
            let playing_witness = playing.fallback_witness.clone();
            let discarded_witness = background.fallback_witness.clone();
            shared.put(0, Bytes::from(vec![b'A'; FIRST as usize]), false);
            shared.put(1, Bytes::from_static(b"AAC"), true);

            assert_eq!(background.block_at(1).unwrap().as_ref(), b"AAC");
            drop(background);
            assert!(discarded_witness.load(Ordering::Relaxed));
            assert!(!playing_witness.load(Ordering::Relaxed));
            assert_eq!(playing.block_at(0).unwrap().len(), FIRST as usize);
            assert!(!playing_witness.load(Ordering::Relaxed));
            assert_eq!(playing.block_at(1).unwrap().as_ref(), b"AAC");
            assert!(playing_witness.load(Ordering::Relaxed));
        }
        #[test]
        fn cdn_fallback_marker_requires_reading_a_verified_backup_block() {
            for cancelled in [false, true] {
                let shared = Shared::new(FIRST + 3, 0, None, false, false);
                let mut reader = StreamReader::new(shared.clone());
                shared.put(0, Bytes::from(vec![b'A'; FIRST as usize]), false);
                shared.put(1, Bytes::from_static(b"AAC"), true);
                assert!(!shared.used_fallback_host());
                assert_eq!(reader.block_at(0).unwrap().len(), FIRST as usize);
                assert!(!shared.used_fallback_host());
                if cancelled {
                    shared.cancel();
                }
                assert_eq!(reader.block_at(1).unwrap().as_ref(), b"AAC");
                assert_eq!(shared.used_fallback_host(), !cancelled);
            }
        }

        #[tokio::test]
        async fn pending_403_refresh_does_not_block_a_verified_head() {
            for cdn in [false, true] {
                let primary = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let reserve = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let total = FIRST + 3;
                let shared = Shared::new(total, 0, None, false, false);
                let mut reader = StreamReader::new(shared.clone());
                let url = format!("http://{}/audio?test=1", primary.local_addr().unwrap());
                let media = if cdn {
                    cdn_file(&url, &format!("http://{}/audio?test=1", reserve.local_addr().unwrap()), 1)
                } else {
                    Media::File(url)
                };
                let entered = Arc::new(Notify::new());
                let refresh: Refresh = {
                    let entered = entered.clone();
                    Arc::new(move || {
                        let entered = entered.clone();
                        Box::pin(async move {
                            entered.notify_one();
                            std::future::pending().await
                        })
                    })
                };
                let serve = async {
                    let (first, range) = accept_range(&primary).await;
                    let (second, _) = accept_range(&primary).await;
                    let (mut head, mut tail) = if range.starts_with("GET /audio?test=1&range=0-") {
                        (first, second)
                    } else {
                        (second, first)
                    };
                    tail.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                    entered.notified().await;
                    let last = FIRST - 1;
                    let header = format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {FIRST}\r\nContent-Range: bytes 0-{last}/{total}\r\nConnection: close\r\n\r\n");
                    head.write_all(header.as_bytes()).await.unwrap();
                    head.write_all(&vec![b'A'; FIRST as usize]).await.unwrap();
                    while shared.inner.lock().unwrap().blocks[0].is_none() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    assert!(reader.block_at(0).unwrap().iter().all(|&byte| byte == b'A'));
                    assert!(!shared.used_fallback_host());
                    shared.cancel();
                };
                timeout(WAIT, async {
                    tokio::join!(download(client(), media, "test".into(), shared.clone(), Some(refresh)), serve);
                }).await.unwrap();
                assert!(timeout(Duration::from_millis(30), reserve.accept()).await.is_err());
                assert!(shared.error().is_none());
            }
        }
        #[tokio::test]
        async fn cdn_fallback_cancellation_does_not_mark_an_incomplete_block() {
            let mut primary = Server::new(vec![reply("503 Service Unavailable", "Content-Length: 0\r\n", "")]).await;
            let mut fallback = Server::new(vec![Reply::Stall]).await;
            let shared = Shared::new(7, 0, None, false, false);
            let reader = StreamReader::new(shared.clone());
            let media = cdn_file(&primary.url, &fallback.url, 1);
            timeout(WAIT, async {
                tokio::join!(
                    download(client(), media, "test".into(), shared.clone(), None),
                    async {
                        primary.request().await;
                        fallback.request().await;
                        assert!(!shared.used_fallback_host());
                        drop(reader);
                        shared.finished().await;
                    },
                );
            }).await.unwrap();
            primary.finish().await;
            fallback.finish().await;
            assert!(!shared.used_fallback_host());
            assert!(shared.error().is_none());
        }

        #[tokio::test]
        async fn cdn_fallback_counts_only_a_timeout_without_received_bytes() {
            let timed_client = || reqwest::Client::builder().no_proxy().read_timeout(Duration::from_millis(250)).build().unwrap();
            let mut primary = Server::new(vec![Reply::NoHeaders]).await;
            let mut fallback = Server::new(vec![reply("200 OK", "Content-Length: 7\r\n", "RESERVE")]).await;
            let shared = Shared::new(7, 0, None, false, false);
            let media = cdn_file(&primary.url, &fallback.url, 1);
            timeout(WAIT, download(timed_client(), media, "test".into(), shared.clone(), None)).await.unwrap();
            primary.finish().await;
            fallback.finish().await;
            assert!(!shared.used_fallback_host());
            assert_eq!(StreamReader::new(shared.clone()).block_at(0).unwrap().as_ref(), b"RESERVE");
            assert!(shared.used_fallback_host());

            let mut primary = Server::new(vec![Reply::Stall, reply("200 OK", "Content-Length: 6\r\n", "BCDEFG")]).await;
            let fallback = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/audio?test=1", fallback.local_addr().unwrap());
            let shared = Shared::new(7, 0, None, false, false);
            let media = cdn_file(&primary.url, &url, 1);
            timeout(WAIT, download(timed_client(), media, "test".into(), shared.clone(), None)).await.unwrap();
            assert!(primary.request().await.starts_with("GET /audio?test=1&range=0-6 "));
            assert!(primary.request().await.starts_with("GET /audio?test=1&range=1-6 "));
            primary.finish().await;
            assert!(timeout(Duration::from_millis(30), fallback.accept()).await.is_err());
            assert!(!shared.used_fallback_host());
            assert_eq!(shared.bytes().as_deref(), Some(b"ABCDEFG".as_slice()));
        }
        #[tokio::test]
        async fn cdn_fallback_does_not_contact_reserve_on_authorization_or_not_found() {
            for (status, forbidden) in [("401 Unauthorized", false), ("403 Forbidden", true), ("404 Not Found", false)] {
                let mut replies = vec![reply(status, "Content-Length: 0\r\n", "")];
                if !forbidden {
                    replies.push(reply("200 OK", "Content-Length: 3\r\n", "AAC"));
                }
                let mut primary = Server::new(replies).await;
                let fallback = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!("http://{}/audio?test=1", fallback.local_addr().unwrap());
                let shared = Shared::new(3, 0, None, false, false);
                let media = cdn_file(&primary.url, &url, 1);
                timeout(WAIT, download(client(), media, "test".into(), shared.clone(), None)).await.unwrap();
                assert!(!shared.used_fallback_host());
                if forbidden {
                    assert!(shared.refused());
                } else {
                    assert_eq!(shared.bytes().as_deref(), Some(b"AAC".as_slice()));
                }
                primary.finish().await;
                assert!(timeout(Duration::from_millis(30), fallback.accept()).await.is_err());
            }
        }
        #[tokio::test]
        async fn repeated_503_at_the_start_ends_playback_before_the_download_budget() {
            for (playing, refusals) in [(true, 1), (true, 2), (false, 2)] {
                let mut replies: Vec<_> = (0..refusals)
                    .map(|_| reply("503 Service Unavailable", "Content-Length: 0\r\n", ""))
                    .collect();
                let fails = playing && refusals == 2;
                if !fails {
                    replies.push(reply("200 OK", "Content-Length: 3\r\n", "AAC"));
                }
                let mut server = Server::new(replies).await;
                let shared = Shared::new(3, 0, None, false, false);
                let media = Media::File(server.url.clone());
                timeout(WAIT, async {
                    if playing {
                        download(client(), media, "test".into(), shared.clone(), None).await;
                    } else {
                        download_behind(client(), media, "test".into(), shared.clone(), None, None).await;
                    }
                }).await.unwrap();
                if fails {
                    assert!(shared.error().unwrap().contains("503"));
                    assert!(!shared.is_complete());
                } else {
                    assert_eq!(shared.bytes().as_deref(), Some(b"AAC".as_slice()));
                }
                server.finish().await;
            }
        }

        #[tokio::test]
        async fn terminal_prefetch_error_wakes_idle_workers() {
            let mut server = Server::new(vec![reply("403 Forbidden", "Content-Length: 0\r\n", "")]).await;
            let shared = Shared::new(BLOCK * 3, 0, None, false, true);
            timeout(WAIT, async {
                tokio::join!(
                    download(client(), Media::File(server.url.clone()), "test".into(), shared.clone(), None),
                    shared.finished(),
                );
            }).await.unwrap();
            assert!(!shared.is_complete());
            assert!(shared.refused());
            let error = shared.error().unwrap();
            assert!(error.contains("403"));
            assert!(!error.contains(&server.url));
            server.finish().await;
        }
    }
}
