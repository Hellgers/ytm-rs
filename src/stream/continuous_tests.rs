use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::time::timeout;

use super::{
    BLOCK, FIRST, HEAD_BLOCKS, Media, Refresh, Shared, StreamReader, WORKERS, block_count,
    block_start, download_prefetch,
};

const WAIT: Duration = Duration::from_secs(5);
const HEAD_BYTES: usize = (FIRST + (HEAD_BLOCKS as u64 - 1) * BLOCK) as usize;
const TAIL_BYTES: usize = 4_096;
const AT_15: usize = HEAD_BYTES - FIRST as usize / 2;
const AT_18: usize = HEAD_BYTES;

struct CancelOnDrop(Arc<Shared>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn fixture() -> Arc<Vec<u8>> {
    Arc::new(
        (0..HEAD_BYTES + TAIL_BYTES)
            .map(|index| (index % 251) as u8)
            .collect(),
    )
}

async fn listen() -> (TcpListener, Media) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let media = Media::File(format!(
        "http://{}/audio?fixture=1",
        listener.local_addr().unwrap()
    ));
    (listener, media)
}

async fn accept(listener: &TcpListener) -> (TcpStream, String) {
    let (mut socket, _) = listener.accept().await.unwrap();
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        let mut chunk = [0; 1024];
        let count = socket.read(&mut chunk).await.unwrap();
        assert!(count > 0 && request.len() < 8192);
        request.extend_from_slice(&chunk[..count]);
    }
    (socket, String::from_utf8(request).unwrap())
}

async fn accept_whole_get(listener: &TcpListener) -> TcpStream {
    let (socket, request) = accept(listener).await;
    assert!(request.starts_with("GET /audio?fixture=1 HTTP/1.1\r\n"));
    assert!(
        !request
            .lines()
            .any(|line| line.to_ascii_lowercase().starts_with("range:"))
    );
    socket
}

async fn send_header(socket: &mut TcpStream, length: usize) {
    socket
        .write_all(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
}

async fn wait_for_head(shared: &Arc<Shared>) {
    let shared = shared.clone();
    tokio::task::spawn_blocking(move || {
        let inner = shared.inner.lock().unwrap();
        let (inner, _) = shared
            .ready
            .wait_timeout_while(inner, WAIT, |inner| {
                !inner.cancelled
                    && inner.error.is_none()
                    && inner.blocks[..HEAD_BLOCKS].iter().any(Option::is_none)
            })
            .unwrap();
        assert!(inner.blocks[..HEAD_BLOCKS].iter().all(Option::is_some));
        assert!(inner.blocks[HEAD_BLOCKS].is_none());
        assert!(!inner.cancelled && inner.error.is_none());
    })
    .await
    .unwrap();
}

fn requested_range(request: &str) -> (u64, u64) {
    let range = request
        .lines()
        .next()
        .unwrap()
        .strip_prefix("GET /audio?fixture=1&range=")
        .unwrap()
        .strip_suffix(" HTTP/1.1")
        .unwrap();
    let (start, end) = range.split_once('-').unwrap();
    (start.parse().unwrap(), end.parse().unwrap())
}

async fn send_range(socket: &mut TcpStream, data: &[u8], start: u64, end: u64) {
    let body = &data[start as usize..=end as usize];
    socket.write_all(
        format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\nConnection: close\r\n\r\n",
            body.len(), data.len(),
        ).as_bytes(),
    ).await.unwrap();
    socket.write_all(body).await.unwrap();
}

async fn ordinary_ranges(listener: &TcpListener, data: &[u8], first_needed: usize) {
    let mut expected: BTreeSet<_> = (first_needed..block_count(data.len() as u64))
        .map(|index| {
            (
                block_start(index),
                block_start(index + 1).min(data.len() as u64) - 1,
            )
        })
        .collect();
    while !expected.is_empty() {
        let (mut socket, request) = accept(listener).await;
        let (start, end) = requested_range(&request);
        assert!(
            expected.remove(&(start, end)),
            "unexpected or duplicate range {start}-{end}"
        );
        send_range(&mut socket, data, start, end).await;
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

#[tokio::test]
async fn the_prefetched_response_continues_on_the_same_connection() {
    let data = fixture();
    let shared = Shared::new(data.len() as u64, 0, None, false, true);
    let _cancel = CancelOnDrop(shared.clone());
    let (listener, media) = listen().await;
    let (request_tail, send_tail) = oneshot::channel();
    let (sent_tail, tail_sent) = oneshot::channel();

    let server = async {
        let mut socket = accept_whole_get(&listener).await;
        send_header(&mut socket, data.len()).await;
        let split = FIRST as usize / 2;
        socket.write_all(&data[..split]).await.unwrap();
        socket.write_all(&data[split..HEAD_BYTES]).await.unwrap();

        let mut peer_close_probe = [0_u8];
        tokio::select! {
            biased;
            result = socket.read(&mut peer_close_probe) => panic!("whole GET closed at the head boundary: {result:?}"),
            signal = send_tail => signal.unwrap(),
        }
        socket.write_all(&data[HEAD_BYTES..]).await.unwrap();
        sent_tail.send(()).unwrap();
    };
    let playback = async {
        wait_for_head(&shared).await;
        request_tail.send(()).unwrap();
        tail_sent.await.unwrap();
        // Even with the tail on the wire, a prefetch exposes only its head until
        // playback installs a reader.
        assert!(
            timeout(Duration::from_millis(25), shared.finished())
                .await
                .is_err()
        );
        assert!(!shared.is_complete());

        let mut reader = StreamReader::new(shared.clone());
        let data = data.clone();
        tokio::task::spawn_blocking(move || {
            // Byte anchors straddle the same boundary as seeking from 15 s
            // through 18 s in the playback regression.
            for (at, count) in [(AT_15, AT_18 - AT_15 + 64), (AT_18, 64), (AT_15, 64)] {
                reader.seek(SeekFrom::Start(at as u64)).unwrap();
                let mut bytes = vec![0; count];
                reader.read_exact(&mut bytes).unwrap();
                assert_eq!(bytes, data[at..at + count]);
            }
        })
        .await
        .unwrap();
    };
    timeout(WAIT, async {
        tokio::join!(
            download_prefetch(client(), media, "test".into(), shared.clone(), None),
            server,
            playback,
        );
    })
    .await
    .unwrap();
    assert!(shared.is_complete());
}

#[tokio::test]
async fn a_bad_whole_get_retries_only_unverified_ranges() {
    for wrong_length in [true, false] {
        let data = fixture();
        let shared = Shared::new(data.len() as u64, 0, None, false, true);
        let _cancel = CancelOnDrop(shared.clone());
        let (listener, media) = listen().await;
        let (response_started, started) = oneshot::channel();

        let server = async {
            let mut socket = accept_whole_get(&listener).await;
            let first_needed = if wrong_length {
                let bad_length = data.len() - 1;
                send_header(&mut socket, bad_length).await;
                response_started.send(()).unwrap();
                // The client is allowed to close as soon as it rejects the size.
                let _ = socket.write_all(&vec![0xff; bad_length]).await;
                0
            } else {
                send_header(&mut socket, data.len()).await;
                socket.write_all(&data[..HEAD_BYTES]).await.unwrap();
                socket.write_all(&[0xff; 64]).await.unwrap();
                response_started.send(()).unwrap();
                HEAD_BLOCKS
            };
            drop(socket);
            ordinary_ranges(&listener, &data, first_needed).await;
        };
        let playback = async {
            started.await.unwrap();
            if !wrong_length {
                wait_for_head(&shared).await;
            }
            let mut reader = StreamReader::new(shared.clone());
            let actual = tokio::task::spawn_blocking(move || {
                let mut output = Vec::new();
                reader.read_to_end(&mut output).unwrap();
                output
            })
            .await
            .unwrap();
            assert_eq!(actual.as_slice(), data.as_slice());
        };

        timeout(WAIT, async {
            tokio::join!(
                download_prefetch(client(), media, "test".into(), shared.clone(), None),
                server,
                playback,
            );
        })
        .await
        .unwrap();
        assert!(shared.is_complete());
    }
}

#[tokio::test]
async fn cancelling_a_waiting_prefetch_closes_the_whole_response() {
    let data = fixture();
    let shared = Shared::new(data.len() as u64, 0, None, false, true);
    let _cancel = CancelOnDrop(shared.clone());
    let (listener, media) = listen().await;
    let server = async {
        let mut socket = accept_whole_get(&listener).await;
        send_header(&mut socket, data.len()).await;
        socket.write_all(&data[..HEAD_BYTES]).await.unwrap();
        assert!(matches!(socket.read(&mut [0]).await, Ok(0) | Err(_)));
    };
    let cancel = async {
        wait_for_head(&shared).await;
        shared.cancel();
    };

    timeout(WAIT, async {
        tokio::join!(
            download_prefetch(client(), media, "test".into(), shared.clone(), None),
            server,
            cancel,
        );
    })
    .await
    .unwrap();
    assert!(!shared.is_complete());
    assert!(shared.error().is_none());
}

#[tokio::test]
async fn a_forbidden_whole_get_renews_before_requesting_any_ranges() {
    let data = b"valid audio bytes";
    let shared = Shared::new(data.len() as u64, 0, None, false, true);
    let (expired, expired_media) = listen().await;
    let (fresh, fresh_media) = listen().await;
    let Media::File(fresh_url) = fresh_media else {
        unreachable!()
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let refresh: Refresh = {
        let calls = calls.clone();
        Arc::new(move || {
            calls.fetch_add(1, Ordering::Relaxed);
            let fresh_url = fresh_url.clone();
            Box::pin(async move { Ok(Media::File(fresh_url)) })
        })
    };

    let deny_expired = async {
        let mut socket = accept_whole_get(&expired).await;
        socket
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
    };
    timeout(WAIT, async {
        tokio::join!(
            download_prefetch(
                client(),
                expired_media,
                "test".into(),
                shared.clone(),
                Some(refresh)
            ),
            deny_expired,
            ordinary_ranges(&fresh, data, 0),
        );
    })
    .await
    .unwrap();

    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(shared.bytes().as_deref(), Some(data.as_slice()));
    assert!(shared.error().is_none());
    assert!(
        timeout(Duration::from_millis(25), expired.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn an_unauthorized_whole_get_stops_without_fallback_ranges() {
    let shared = Shared::new(5, 0, None, false, true);
    let (listener, media) = listen().await;
    let refresh: Refresh = Arc::new(|| panic!("401 started a URL refresh"));
    let refuse = async {
        let mut socket = accept_whole_get(&listener).await;
        socket
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
    };
    timeout(WAIT, async {
        tokio::join!(
            download_prefetch(
                client(),
                media,
                "test".into(),
                shared.clone(),
                Some(refresh)
            ),
            refuse,
        );
    })
    .await
    .unwrap();

    assert!(
        shared
            .error()
            .is_some_and(|message| message.contains("401"))
    );
    assert!(!shared.refused());
    assert!(!shared.is_complete());
    assert!(
        timeout(Duration::from_millis(25), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_far_seek_reaches_the_target_without_the_delayed_whole_response() {
    let target_block = HEAD_BLOCKS * 2 + 1;
    let target_start = block_start(target_block);
    let target_end = target_start + 127;
    let data: Vec<u8> = (0..=target_end).map(|index| (index % 251) as u8).collect();
    let shared = Shared::new(data.len() as u64, 0, None, false, true);
    let _cancel = CancelOnDrop(shared.clone());
    let (listener, media) = listen().await;
    let (target_read, reader_done) = oneshot::channel();

    let server = async {
        let mut whole = accept_whole_get(&listener).await;
        send_header(&mut whole, data.len()).await;
        whole.write_all(&data[..HEAD_BYTES]).await.unwrap();
        // No bytes of the gap are sent on this response.
        assert!(matches!(whole.read(&mut [0]).await, Ok(0) | Err(_)));

        let mut waiting_ranges = Vec::new();
        loop {
            let (mut socket, request) = accept(&listener).await;
            let (start, end) = requested_range(&request);
            assert!(
                start >= HEAD_BYTES as u64,
                "re-requested a verified head block"
            );
            if start == target_start {
                assert_eq!(end, target_end);
                send_range(&mut socket, &data, start, end).await;
                break;
            }
            waiting_ranges.push(socket);
            assert!(
                waiting_ranges.len() < WORKERS,
                "no worker requested the seek target"
            );
        }
        reader_done.await.unwrap();
    };
    let playback = async {
        wait_for_head(&shared).await;
        let mut reader = StreamReader::new(shared.clone());
        let (actual, reader) = tokio::task::spawn_blocking(move || {
            reader.seek(SeekFrom::Start(target_start + 16)).unwrap();
            let mut bytes = [0u8; 32];
            reader.read_exact(&mut bytes).unwrap();
            (bytes, reader)
        })
        .await
        .unwrap();
        let at = target_start as usize + 16;
        assert_eq!(&actual, &data[at..at + actual.len()]);
        assert!(shared.inner.lock().unwrap().blocks[HEAD_BLOCKS].is_none());
        target_read.send(()).unwrap();
        drop(reader);
    };

    timeout(WAIT, async {
        tokio::join!(
            download_prefetch(client(), media, "test".into(), shared.clone(), None),
            server,
            playback,
        );
    })
    .await
    .unwrap();
}
