//! Parallel, seek-aware HTTP audio buffer.
//!
//! The file is split into fixed blocks fetched by a few concurrent range requests.
//! Workers always claim the first missing block at or after the one a reader is
//! waiting on, so playback start and seeks are served first and the rest fills in
//! around them. Reads of a missing block block until it arrives (or the download fails).
//!
//! Several readers may share one buffer (the audio engine opens a second, seekable
//! decoder when the user seeks); the download stops once the last reader is dropped.

use std::future::Future;
use std::io::{self, Read, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use bytes::Bytes;

const BLOCK: u64 = 256 * 1024;
const WORKERS: usize = 4;
/// Retries are also bounded by a per-block deadline, including URL renewal.
const RETRIES: u32 = 6;
/// A response that delivers nothing for this long is dead; a slow one that keeps
/// delivering is fine, however long the block takes.
const STALL: Duration = Duration::from_secs(8);
const BLOCK_DEADLINE: Duration = Duration::from_secs(30);
/// googlevideo occasionally starts rejecting a URL partway through (HTTP 403);
/// a freshly resolved URL for the same video resumes the download.
const MAX_REFRESHES: u32 = 2;
/// A prefetch only fetches this much (~16 s of audio) until playback actually opens it:
/// enough to start at once, without holding a whole extra song in memory.
const HEAD_BLOCKS: usize = 2;
/// Files larger than this (about 20 minutes at 256 kbps: long mixes, whole albums in
/// one video) aren't kept whole. Each reader keeps a window around where it reads,
/// `AHEAD` blocks forward (about 5 minutes at 256 kbps) and `BEHIND` back; the rest is dropped and
/// fetched again if a seek goes back to it.
const WINDOWED_FROM: u64 = 40 * 1024 * 1024;
const AHEAD: usize = 40;
const BEHIND: usize = 8;

/// Resolves a fresh stream URL for the same video.
pub type Refresh = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send>> + Send + Sync>;

struct Inner {
    blocks: Vec<Option<Bytes>>,
    in_flight: Vec<bool>,
    /// Block a reader most recently needed; workers prioritize from here.
    want: usize,
    error: Option<String>,
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

pub struct Shared {
    inner: Mutex<Inner>,
    ready: Condvar,
    pub total: u64,
    /// Track length reported by the stream metadata (0 if unknown).
    pub duration_ms: u64,
    /// YouTube's loudness for this stream, in dB above its -14 LUFS target.
    pub loudness_db: Option<f32>,
    /// Opus in WebM; otherwise AAC in MP4.
    pub opus: bool,
    /// Flips to true once the download task has stopped (complete, failed or cancelled).
    done: tokio::sync::watch::Sender<bool>,
    /// Wakes waiting workers: `limit` lifted, a reader moved or left, or cancelled.
    changed: tokio::sync::Notify,
}

impl Shared {
    /// A buffer for `total` bytes. `head_only` (a prefetch) fetches just the start until
    /// a reader opens it.
    pub fn new(total: u64, duration_ms: u64, loudness_db: Option<f32>, opus: bool, head_only: bool) -> Arc<Self> {
        let n = total.div_ceil(BLOCK) as usize;
        let limit = if head_only { HEAD_BLOCKS.min(n) } else { n };
        Arc::new(Self {
            inner: Mutex::new(Inner {
                blocks: vec![None; n],
                in_flight: vec![false; n],
                want: 0,
                error: None,
                readers: 0,
                cancelled: false,
                limit,
                wants: Vec::new(),
                windowed: total > WINDOWED_FROM,
                ever: vec![false; n],
                fetched: 0,
            }),
            ready: Condvar::new(),
            total,
            duration_ms,
            loudness_db,
            opus,
            done: tokio::sync::watch::Sender::new(false),
            changed: tokio::sync::Notify::new(),
        })
    }

    /// A buffer that already holds the whole file (a downloaded song).
    pub fn from_bytes(data: Vec<u8>, duration_ms: u64, loudness_db: Option<f32>) -> Arc<Self> {
        let total = data.len() as u64;
        let data = Bytes::from(data);
        let blocks: Vec<Option<Bytes>> =
            (0..total.div_ceil(BLOCK)).map(|i| Some(data.slice(i as usize * BLOCK as usize..((i + 1) * BLOCK).min(total) as usize))).collect();
        let n = blocks.len();
        // Saved songs are AAC (downloads take that stream).
        let shared = Self::new(total, duration_ms, loudness_db, false, false);
        {
            let mut inner = shared.inner.lock().unwrap();
            inner.blocks = blocks;
            inner.windowed = false;
            inner.ever = vec![true; n];
            inner.fetched = n;
        }
        shared.done.send_replace(true);
        shared
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

    pub fn can_reuse(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.error.is_none() && (!inner.cancelled || inner.blocks.iter().all(Option::is_some))
    }

    /// Wait until the download stops, however it ends.
    pub async fn finished(&self) {
        let _ = self.done.subscribe().wait_for(|d| *d).await;
    }

    /// Why the download stopped early, if it failed.
    pub fn error(&self) -> Option<String> {
        self.inner.lock().unwrap().error.clone()
    }

    /// Stop downloading (e.g. a prefetch that was superseded).
    pub fn cancel(&self) {
        self.inner.lock().unwrap().cancelled = true;
        self.ready.notify_all();
        self.changed.notify_waiters();
    }

    /// Reserve a decoder reader before starting a blocking constructor. A local
    /// interruption wakes that reader without cancelling another seek's reader.
    pub fn read_interrupt(self: &Arc<Self>) -> ReadInterrupt {
        ReadInterrupt::new(self.clone())
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

    fn put(&self, idx: usize, data: Bytes) {
        let mut inner = self.inner.lock().unwrap();
        inner.blocks[idx] = Some(data);
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
    }

    fn fail(&self, err: String) {
        FAIL_FROM.store(usize::MAX, std::sync::atomic::Ordering::Relaxed);
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.cancelled || inner.error.is_some() {
                return;
            }
            log::error!("download failed: {err}");
            inner.error = Some(err);
        }
        self.ready.notify_all();
        self.changed.notify_waiters();
    }

    fn stopped(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.cancelled || inner.error.is_some()
    }

    fn range(&self, idx: usize) -> (u64, u64) {
        let start = idx as u64 * BLOCK;
        (start, (start + BLOCK).min(self.total) - 1)
    }
}

/// The current URL plus how many times it has been replaced.
struct UrlSlot {
    url: tokio::sync::Mutex<(String, u32)>,
    refresh: Option<Refresh>,
}

impl UrlSlot {
    /// Replace the URL if `seen` is still current; returns false when out of refreshes.
    async fn renew(&self, seen: u32) -> bool {
        let mut slot = self.url.lock().await;
        if slot.1 != seen {
            return true; // another worker already refreshed it
        }
        let Some(refresh) = self.refresh.as_ref().filter(|_| slot.1 < MAX_REFRESHES) else { return false };
        match refresh().await {
            Ok(url) => {
                log::info!("stream URL refreshed after a rejected or stalled request");
                *slot = (url, slot.1 + 1);
                true
            }
            Err(e) => {
                log::warn!("stream URL refresh failed: {e:#}");
                false
            }
        }
    }
}

/// Download into `shared` with parallel range requests; returns when complete,
/// failed, or every reader was dropped.
pub async fn download(http: reqwest::Client, url: String, ua: String, shared: Arc<Shared>, refresh: Option<Refresh>) {
    download_with_policy(http, url, ua, shared, refresh, DownloadPolicy { stall: STALL, deadline: BLOCK_DEADLINE, retry_base: Duration::from_millis(400), refresh_http: |_| crate::proxy::http() }).await;
}

#[derive(Clone, Copy)]
struct DownloadPolicy {
    stall: Duration,
    deadline: Duration,
    retry_base: Duration,
    refresh_http: fn(&reqwest::Client) -> reqwest::Client,
}

/// Normal reader/limit notifications do not restart a request. Cancellation or
/// another worker's terminal failure drops the in-flight HTTP/renew/backoff future.
async fn until_stopped<T>(shared: &Shared, future: impl Future<Output = T>) -> Option<T> {
    tokio::pin!(future);
    loop {
        let changed = shared.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if shared.stopped() {
            return None;
        }
        tokio::select! {
            result = &mut future => return Some(result),
            _ = changed => {}
        }
    }
}

async fn download_with_policy(http: reqwest::Client, url: String, ua: String, shared: Arc<Shared>, refresh: Option<Refresh>, policy: DownloadPolicy) {
    let slot = Arc::new(UrlSlot { url: tokio::sync::Mutex::new((url, 0)), refresh });
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..WORKERS {
        let (http, ua, shared, slot) = (http.clone(), ua.clone(), shared.clone(), slot.clone());
        set.spawn(async move {
            loop {
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
                let (a, b) = shared.range(idx);
                let block = async {
                  let mut attempt = 0;
                  loop {
                    let (url, version) = slot.url.lock().await.clone();
                    // A refreshed URL is tied to the address that resolved it, which is the
                    // current shared client's (a proxy may have switched exits since).
                    let client = if version > 0 { (policy.refresh_http)(&http) } else { http.clone() };
                    let res = if idx >= FAIL_FROM.load(std::sync::atomic::Ordering::Relaxed) {
                        Err(anyhow::anyhow!("simulated network failure"))
                    } else {
                        fetch(&client, &format!("{url}&range={a}-{b}"), &ua, b - a + 1, policy.stall).await
                    };
                    match res {
                        Ok(data) => return Ok(data),
                        Err(e) if should_renew(&e) && slot.renew(version).await => {}
                        Err(e) if attempt < RETRIES && !is_forbidden(&e) => {
                            attempt += 1;
                            log::warn!("range {a}-{b}: {e:#} (retry {attempt})");
                            tokio::time::sleep(policy.retry_base * (attempt * attempt)).await;
                        }
                        Err(e) => {
                            return Err(e);
                        }
                    }
                  }
                };
                match until_stopped(&shared, tokio::time::timeout(policy.deadline, block)).await {
                    None => return,
                    Some(Ok(Ok(data))) => shared.put(idx, data),
                    Some(Ok(Err(e))) => { shared.fail(format!("range {a}-{b}: {e:#}")); return; }
                    Some(Err(_)) => { shared.fail(format!("range {a}-{b}: block deadline exceeded ({} s)", policy.deadline.as_secs())); return; }
                }
            }
        });
    }
    while let Some(result) = set.join_next().await {
        if result.is_err() {
            shared.fail("audio download worker stopped unexpectedly".into());
        }
    }
    shared.done.send_replace(true);
}

fn is_forbidden(e: &anyhow::Error) -> bool {
    e.downcast_ref::<reqwest::Error>()
        .and_then(reqwest::Error::status)
        .is_some_and(|s| s == reqwest::StatusCode::FORBIDDEN)
}

fn should_renew(e: &anyhow::Error) -> bool {
    e.is::<Stalled>() || e.downcast_ref::<reqwest::Error>().is_some_and(|e| {
        e.is_timeout() || e.is_connect() || e.is_body()
            || e.status().is_some_and(|s| s == reqwest::StatusCode::FORBIDDEN || s.is_server_error())
    })
}

#[derive(Debug)]
struct Stalled;
impl std::fmt::Display for Stalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("audio response stopped delivering data") }
}
impl std::error::Error for Stalled {}

/// Dev hook: YTM_STREAM_FAIL=<block> makes the first download fail from that block on,
/// as if the connection died mid-song; later downloads work.
static FAIL_FROM: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX);

pub fn init_dev_hooks() {
    if let Some(b) = std::env::var("YTM_STREAM_FAIL").ok().and_then(|v| v.parse().ok()) {
        FAIL_FROM.store(b, std::sync::atomic::Ordering::Relaxed);
    }
}

async fn fetch(http: &reqwest::Client, url: &str, ua: &str, expect: u64, stall: Duration) -> anyhow::Result<Bytes> {
    let stalled = || anyhow::Error::new(Stalled);
    // Errors leave the URL out: it carries the client's IP address and signatures (logs).
    let mut resp = tokio::time::timeout(stall, http.get(url).header("User-Agent", ua).send())
        .await
        .map_err(|_| stalled())?
        .and_then(reqwest::Response::error_for_status)
        .map_err(reqwest::Error::without_url)?;
    let mut buf = bytes::BytesMut::with_capacity(expect as usize);
    while let Some(chunk) = tokio::time::timeout(stall, resp.chunk()).await.map_err(|_| stalled())?.map_err(reqwest::Error::without_url)? {
        anyhow::ensure!(buf.len() as u64 + chunk.len() as u64 <= expect, "server returned more bytes than the requested range");
        buf.extend_from_slice(&chunk);
    }
    anyhow::ensure!(buf.len() as u64 == expect, "short read: {} of {expect} bytes", buf.len());
    Ok(buf.freeze())
}

struct ReadControl {
    shared: Arc<Shared>,
    stopped: std::sync::atomic::AtomicBool,
    waiting: std::sync::atomic::AtomicBool,
    id: u64,
}

/// A reader reservation and its independent cancellation signal. Keeping the
/// reservation alive bridges the gap between retiring a decoder and opening its
/// replacement on the same download.
#[derive(Clone)]
pub struct ReadInterrupt(Arc<ReadControl>);

impl ReadInterrupt {
    fn new(shared: Arc<Shared>) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        {
            let mut inner = shared.inner.lock().unwrap();
            inner.readers += 1;
            inner.limit = inner.blocks.len();
            inner.wants.push((id, 0));
        }
        shared.changed.notify_waiters();
        Self(Arc::new(ReadControl { shared, stopped: std::sync::atomic::AtomicBool::new(false), waiting: std::sync::atomic::AtomicBool::new(false), id }))
    }

    pub fn cancel(&self) {
        // Pair the flag with the Condvar mutex: a cancellation between the
        // reader's predicate check and wait must not lose its notification.
        let _inner = self.0.shared.inner.lock().unwrap();
        self.0.stopped.store(true, std::sync::atomic::Ordering::Release);
        self.0.shared.ready.notify_all();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.stopped.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn is_waiting(&self) -> bool {
        self.0.waiting.load(std::sync::atomic::Ordering::Acquire)
    }
}

struct ReadWait<'a>(&'a std::sync::atomic::AtomicBool);
impl Drop for ReadWait<'_> {
    fn drop(&mut self) { self.0.store(false, std::sync::atomic::Ordering::Release); }
}

impl Drop for ReadControl {
    fn drop(&mut self) {
        let mut inner = self.shared.inner.lock().unwrap();
        inner.readers -= 1;
        inner.wants.retain(|&(id, _)| id != self.id);
        if inner.readers == 0 {
            inner.cancelled = true;
        }
        self.shared.ready.notify_all();
        self.shared.changed.notify_waiters();
    }
}

pub struct StreamReader {
    shared: Arc<Shared>,
    pos: u64,
    id: u64,
    /// The block this reader last read.
    block: usize,
    interrupt: ReadInterrupt,
}

impl StreamReader {
    #[cfg(test)]
    pub fn new(shared: Arc<Shared>) -> Self {
        Self::with_interrupt(shared.read_interrupt())
    }

    pub(crate) fn with_interrupt(interrupt: ReadInterrupt) -> Self {
        Self { shared: interrupt.0.shared.clone(), pos: 0, id: interrupt.0.id, block: 0, interrupt }
    }
}

impl Read for StreamReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.pos >= self.shared.total {
            return Ok(0);
        }
        let idx = (self.pos / BLOCK) as usize;
        let mut inner = self.shared.inner.lock().unwrap();
        if self.interrupt.is_cancelled() {
            return Err(io::Error::other("reader interrupted"));
        }
        inner.want = idx;
        if idx != self.block {
            self.block = idx;
            if let Some(w) = inner.wants.iter_mut().find(|(id, _)| *id == self.id) {
                w.1 = idx;
            }
            if inner.windowed {
                self.shared.changed.notify_waiters();
            }
        }
        while inner.blocks[idx].is_none() {
            if self.interrupt.is_cancelled() {
                return Err(io::Error::other("reader interrupted"));
            }
            if let Some(e) = &inner.error {
                return Err(io::Error::other(e.clone()));
            }
            if inner.cancelled {
                return Err(io::Error::other("download cancelled"));
            }
            self.interrupt.0.waiting.store(true, std::sync::atomic::Ordering::Release);
            let waiting = ReadWait(&self.interrupt.0.waiting);
            inner = self.shared.ready.wait(inner).unwrap();
            drop(waiting);
        }
        let block = inner.blocks[idx].as_ref().unwrap();
        let off = (self.pos - idx as u64 * BLOCK) as usize;
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
    let interrupt = shared.read_interrupt();
    decoder_with_interrupt(shared, seekable, interrupt)
}

pub fn decoder_with_interrupt(shared: Arc<Shared>, seekable: bool, interrupt: ReadInterrupt) -> anyhow::Result<TrackDecoder> {
    anyhow::ensure!(Arc::ptr_eq(&shared, &interrupt.0.shared), "reader belongs to a different download");
    anyhow::ensure!(!interrupt.is_cancelled(), "decoder opening interrupted");
    let size = shared.total;
    let reader = StreamReader::with_interrupt(interrupt.clone());
    let kind = if shared.opus {
        let duration_ms = shared.duration_ms;
        DecoderKind::Opus(Box::new(crate::opus::OpusSource::new(reader, size, duration_ms)?))
    } else {
        DecoderKind::Aac(
        rodio::Decoder::builder()
            .with_data(reader)
            .with_byte_len(size)
            .with_seekable(seekable)
            .with_mime_type("audio/mp4")
            .with_hint("m4a")
            .build()?,
        )
    };
    Ok(TrackDecoder { kind, interrupt })
}

/// A song's decoder: AAC (rodio's own) or Opus.
pub struct TrackDecoder {
    kind: DecoderKind,
    interrupt: ReadInterrupt,
}

impl TrackDecoder {
    pub fn interrupt_handle(&self) -> ReadInterrupt {
        self.interrupt.clone()
    }
}

enum DecoderKind {
    Aac(rodio::Decoder<StreamReader>),
    /// Boxed: its decoder state is about 11 KB.
    Opus(Box<crate::opus::OpusSource>),
}

impl Iterator for TrackDecoder {
    type Item = rodio::Sample;

    fn next(&mut self) -> Option<rodio::Sample> {
        match &mut self.kind {
            DecoderKind::Aac(d) => d.next(),
            DecoderKind::Opus(d) => d.next(),
        }
    }
}

impl rodio::Source for TrackDecoder {
    fn current_span_len(&self) -> Option<usize> {
        match &self.kind {
            DecoderKind::Aac(d) => d.current_span_len(),
            DecoderKind::Opus(d) => d.current_span_len(),
        }
    }
    fn channels(&self) -> rodio::ChannelCount {
        match &self.kind {
            DecoderKind::Aac(d) => d.channels(),
            DecoderKind::Opus(d) => d.channels(),
        }
    }
    fn sample_rate(&self) -> rodio::SampleRate {
        match &self.kind {
            DecoderKind::Aac(d) => d.sample_rate(),
            DecoderKind::Opus(d) => d.sample_rate(),
        }
    }
    fn total_duration(&self) -> Option<Duration> {
        match &self.kind {
            DecoderKind::Aac(d) => d.total_duration(),
            DecoderKind::Opus(d) => d.total_duration(),
        }
    }
    fn try_seek(&mut self, pos: Duration) -> Result<(), rodio::source::SeekError> {
        match &mut self.kind {
            DecoderKind::Aac(d) => d.try_seek(pos),
            DecoderKind::Opus(d) => d.try_seek(pos),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn local_interrupt_wakes_reader_and_preserves_reserved_replacement() {
        let shared = Shared::new(1, 0, None, false, false);
        let old = shared.read_interrupt();
        let replacement = shared.read_interrupt();
        let mut reader = StreamReader::with_interrupt(old.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || { let result = reader.read(&mut [0]); let _ = tx.send(result); });
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !old.is_waiting() && std::time::Instant::now() < deadline { std::thread::yield_now(); }
        let was_waiting = old.is_waiting();
        old.cancel();
        let result = rx.recv_timeout(Duration::from_secs(1));
        worker.join().unwrap();
        assert!(was_waiting);
        assert!(result.unwrap().is_err());
        assert!(!old.is_waiting());
        drop(old);
        assert!(!shared.stopped());
        shared.put(0, Bytes::from_static(b"Z"));
        let mut next = StreamReader::with_interrupt(replacement.clone());
        let mut out = [0];
        assert_eq!(next.read(&mut out).unwrap(), 1);
        assert_eq!(out, [b'Z']);
        drop(next);
        assert!(!shared.stopped());
        drop(replacement);
        assert!(shared.stopped());
    }

    #[test]
    fn local_interrupt_stops_even_a_fully_cached_reader() {
        let shared = Shared::from_bytes(b"ABC".to_vec(), 0, None);
        let handle = shared.read_interrupt();
        let mut reader = StreamReader::with_interrupt(handle.clone());
        handle.cancel();
        assert!(reader.read(&mut [0]).is_err());
    }

    fn fast_policy() -> DownloadPolicy {
        DownloadPolicy { stall: Duration::from_millis(50), deadline: Duration::from_millis(300), retry_base: Duration::ZERO, refresh_http: Clone::clone }
    }

    #[tokio::test]
    async fn failed_head_prefetch_releases_waiting_workers_and_finished() {
        let shared = Shared::new(BLOCK * 3, 0, None, false, true);
        let run = download_with_policy(reqwest::Client::new(), "invalid-url".into(), "test".into(), shared.clone(), None, fast_policy());
        tokio::time::timeout(Duration::from_secs(1), run).await.unwrap();
        tokio::time::timeout(Duration::from_millis(100), shared.finished()).await.unwrap();
        assert!(shared.error().is_some());
    }

    #[tokio::test]
    async fn cancellation_interrupts_in_flight_http_before_its_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted, wait_accepted) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let _ = accepted.send(());
            let _socket = socket;
            std::future::pending::<()>().await;
        });
        let shared = Shared::new(1, 0, None, false, false);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let task = tokio::spawn(download_with_policy(client, format!("http://{address}/audio?x=1"), "test".into(), shared.clone(), None,
            DownloadPolicy { stall: Duration::from_secs(30), deadline: Duration::from_secs(60), retry_base: Duration::from_secs(10), refresh_http: Clone::clone }));
        tokio::time::timeout(Duration::from_secs(1), wait_accepted).await.unwrap().unwrap();
        shared.cancel();
        let result = tokio::time::timeout(Duration::from_millis(500), task).await;
        server.abort();
        let _ = server.await;
        result.unwrap().unwrap();
        assert!(shared.error().is_none());
        assert!(*shared.done.borrow());
    }

    #[tokio::test]
    async fn terminal_failure_interrupts_registered_backoff() {
        let shared = Shared::new(1, 0, None, false, false);
        let (entered, wait_entered) = tokio::sync::oneshot::channel();
        let other = shared.clone();
        let task = tokio::spawn(async move { until_stopped(&other, async move {
            let _ = entered.send(());
            tokio::time::sleep(Duration::from_secs(60)).await;
        }).await });
        wait_entered.await.unwrap();
        shared.fail("simulated terminal failure".into());
        assert!(tokio::time::timeout(Duration::from_millis(500), task).await.unwrap().unwrap().is_none());
    }

    #[tokio::test]
    async fn service_error_renews_url_and_fetches_same_representation() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut paths = Vec::new();
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                let n = socket.read(&mut request).await.unwrap();
                let good = String::from_utf8_lossy(&request[..n]).starts_with("GET /good?");
                paths.push(good);
                let response: &[u8] = if good { b"HTTP/1.1 206 Partial Content\r\nContent-Length: 1\r\nConnection: close\r\n\r\nZ" }
                    else { b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n" };
                socket.write_all(response).await.unwrap();
            }
            paths
        });
        let renewals = Arc::new(AtomicUsize::new(0));
        let count = renewals.clone();
        let refresh: Refresh = Arc::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move { Ok(format!("http://{address}/good?x=1")) })
        });
        let shared = Shared::new(1, 0, None, false, false);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        tokio::time::timeout(Duration::from_secs(2), download_with_policy(client, format!("http://{address}/bad?x=1"), "test".into(), shared.clone(), Some(refresh), fast_policy())).await.unwrap();
        assert_eq!(server.await.unwrap(), [false, true]);
        assert_eq!(renewals.load(Ordering::Relaxed), 1);
        assert_eq!(shared.bytes().unwrap(), b"Z");
        assert!(shared.error().is_none());
    }
}
