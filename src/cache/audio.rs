//! Small profile-local cache for complete, validated playback files.
//!
//! A lookup is only useful after the player resolves the exact representation.
//! A slot is replaced only when the complete, fresh file is ready.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use sha2::{Digest, Sha256};

const DIR: &str = "playback-cache";
const LOCK: &str = "write.lock";
const TEMP: &str = "staging.tmp";
const SLOTS: [&str; 4] = ["00.bin", "01.bin", "02.bin", "03.bin"];
pub(crate) const MAX_AUDIO_BYTES: u64 = 8 * 1024 * 1024;
const KEY_LEN: usize = 32;
const EXPIRY_OFFSET: usize = KEY_LEN + 8;
const DIGEST_OFFSET: usize = EXPIRY_OFFSET + 8;
const HEADER_LEN: usize = DIGEST_OFFSET + 32;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Key {
    digest: [u8; KEY_LEN],
    total: u64,
}

impl Key {
    pub(crate) fn new(
        video_id: &str,
        itag: u64,
        modified: u64,
        audio_track: &str,
        total: u64,
    ) -> Option<Self> {
        if video_id.is_empty()
            || itag == 0
            || modified == 0
            || !(1..=MAX_AUDIO_BYTES).contains(&total)
        {
            return None;
        }
        let mut hash = Sha256::new();
        hash.update(b"ytm-rs playback-cache v1");
        for text in [video_id, audio_track] {
            hash.update((text.len() as u64).to_be_bytes());
            hash.update(text.as_bytes());
        }
        for number in [itag, modified, total] {
            hash.update(number.to_be_bytes());
        }
        Some(Self {
            digest: hash.finalize().into(),
            total,
        })
    }

    fn slot(self) -> &'static str {
        SLOTS[usize::from(self.digest[0]) % SLOTS.len()]
    }
}

/// Read one bucket. A collision is a miss and its payload is never read.
pub(crate) fn read(dir: &Path, key: Key) -> Option<Vec<u8>> {
    let mut file = File::open(dir.join(DIR).join(key.slot())).ok()?;
    let mut header = [0; HEADER_LEN];
    file.read_exact(&mut header).ok()?;
    if header[..KEY_LEN] != key.digest || header[KEY_LEN..EXPIRY_OFFSET] != key.total.to_be_bytes()
    {
        return None;
    }
    let expires = u64::from_be_bytes(header[EXPIRY_OFFSET..DIGEST_OFFSET].try_into().ok()?);
    if unix_now()? >= expires || file.metadata().ok()?.len() != key.total + HEADER_LEN as u64 {
        return None;
    }
    let len = key.total as usize;
    let mut payload = Vec::new();
    payload.try_reserve_exact(len).ok()?;
    payload.resize(len, 0);
    file.read_exact(&mut payload).ok()?;
    if file.read(&mut [0]).ok()? != 0 || Sha256::digest(&payload)[..] != header[DIGEST_OFFSET..] {
        return None;
    }
    (unix_now()? < expires).then_some(payload)
}

/// Write and hash incrementally, then atomically replace this bucket.
pub(crate) fn store(dir: &Path, key: Key, chunks: &[Bytes], expires_unix: u64) -> io::Result<()> {
    check_expiry(expires_unix)?;
    let len = chunks
        .iter()
        .try_fold(0u64, |total, chunk| total.checked_add(chunk.len() as u64));
    if len != Some(key.total) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "audio cache payload length does not match its key",
        ));
    }
    let cache = dir.join(DIR);
    match create_private_dir(&cache) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && is_directory(&cache) => {}
        Err(error) => return Err(error),
    }

    let gate = open_gate(&cache)?;
    gate.try_lock().map_err(io::Error::from)?;
    let temp = cache.join(TEMP);
    // Writers and Storage clear hold this same stable OS lock. At most one
    // temporary payload can exist, even if an earlier writer crashed.
    match fs::remove_file(&temp) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut file = private_write().create_new(true).open(&temp)?;
    let staged = write_payload(&mut file, key, chunks, expires_unix);
    drop(file);
    // Both paths are in the same directory. std::fs::rename replaces an
    // existing destination on Windows too; never pre-delete the good slot.
    let result = staged.and_then(|()| {
        check_expiry(expires_unix)?;
        fs::rename(&temp, cache.join(key.slot()))
    });
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn write_payload(file: &mut File, key: Key, chunks: &[Bytes], expires_unix: u64) -> io::Result<()> {
    let mut header = [0; HEADER_LEN];
    header[..KEY_LEN].copy_from_slice(&key.digest);
    header[KEY_LEN..EXPIRY_OFFSET].copy_from_slice(&key.total.to_be_bytes());
    header[EXPIRY_OFFSET..DIGEST_OFFSET].copy_from_slice(&expires_unix.to_be_bytes());
    file.write_all(&header)?;
    let mut hash = Sha256::new();
    for chunk in chunks {
        hash.update(chunk);
        file.write_all(chunk)?;
    }
    file.seek(SeekFrom::Start(DIGEST_OFFSET as u64))?;
    file.write_all(&hash.finalize())?;
    file.sync_all()
}

fn unix_now() -> Option<u64> {
    Some(SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs())
}

fn check_expiry(expires_unix: u64) -> io::Result<()> {
    if unix_now().is_some_and(|now| now < expires_unix) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "audio cache entry must have a fresh expiry",
        ))
    }
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

fn private_write() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}

fn open_gate(cache: &Path) -> io::Result<File> {
    private_write()
        .create(true)
        .truncate(false)
        .open(cache.join(LOCK))
}

fn is_directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn entries() -> impl Iterator<Item = &'static str> {
    SLOTS.into_iter().chain(std::iter::once(TEMP))
}

pub(super) fn size(dir: &Path) -> u64 {
    let cache = dir.join(DIR);
    if !is_directory(&cache) {
        return 0;
    }
    entries()
        .filter_map(|name| fs::symlink_metadata(cache.join(name)).ok())
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
        .sum()
}

pub(super) fn clear(dir: &Path) -> u64 {
    let cache = dir.join(DIR);
    if !is_directory(&cache) {
        return 0;
    }
    let Ok(gate) = open_gate(&cache) else {
        return 0;
    };
    if gate.lock().is_err() {
        return 0;
    }
    // Keep the empty lock file so every concurrently running copy uses the
    // same lock inode. Settings' generic directory clearing does not touch it.
    entries()
        .filter_map(|name| {
            let path = cache.join(name);
            let len = fs::symlink_metadata(&path).ok()?.len();
            fs::remove_file(&path).ok().map(|()| len)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let temp = std::env::temp_dir();
            loop {
                let n = NEXT.fetch_add(1, Ordering::Relaxed);
                let path = temp.join(format!("ytm-rs-playback-cache-{}-{n}", std::process::id()));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("create cache test directory: {error}"),
                }
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn store_future(dir: &Path, key: Key, chunks: &[Bytes]) -> io::Result<()> {
        store(dir, key, chunks, unix_now().unwrap() + 3600)
    }
    fn key() -> Key {
        Key::new("dQw4w9WgXcQ", 251, 44, "en", 4).unwrap()
    }

    #[test]
    fn a_lazy_lookup_validates_the_representation_and_replaces_a_colliding_bucket() {
        let dir = Fixture::new();
        let key = key();
        let other = (45..)
            .map(|revision| Key::new("dQw4w9WgXcQ", 251, revision, "en", 4).unwrap())
            .find(|candidate| candidate.slot() == key.slot())
            .unwrap();
        assert!(read(&dir.0, key).is_none());
        assert!(!dir.0.join(DIR).exists());
        store_future(
            &dir.0,
            key,
            &[Bytes::from_static(b"ab"), Bytes::from_static(b"cd")],
        )
        .unwrap();
        assert_eq!(read(&dir.0, key).as_deref(), Some(b"abcd".as_slice()));
        assert!(read(&dir.0, other).is_none());
        store_future(&dir.0, other, &[Bytes::from_static(b"efgh")]).unwrap();
        assert!(read(&dir.0, key).is_none());
        assert_eq!(read(&dir.0, other).as_deref(), Some(b"efgh".as_slice()));
        assert!(!dir.0.join(DIR).join(TEMP).exists());
    }

    #[test]
    fn keys_bind_format_revision_track_size_and_text_boundaries() {
        for invalid in [
            Key::new("", 251, 1, "", 1),
            Key::new("video", 0, 1, "", 1),
            Key::new("video", 251, 0, "", 1),
            Key::new("video", 251, 1, "", 0),
            Key::new("video", 251, 1, "", MAX_AUDIO_BYTES + 1),
        ] {
            assert!(invalid.is_none());
        }
        assert!(Key::new("video", 251, 1, "", MAX_AUDIO_BYTES).is_some());
        let keys = [
            Key::new("ab", 251, 1, "c", 4),
            Key::new("a", 251, 1, "bc", 4),
            Key::new("ab", 140, 1, "c", 4),
            Key::new("ab", 251, 2, "c", 4),
            Key::new("ab", 251, 1, "d", 4),
            Key::new("ab", 251, 1, "c", 5),
        ];
        let distinct: std::collections::HashSet<_> =
            keys.into_iter().map(|key| key.unwrap().digest).collect();
        assert_eq!(distinct.len(), keys.len());
    }

    #[test]
    fn malformed_cache_files_miss_and_invalid_writes_keep_the_previous_payload() {
        let dir = Fixture::new();
        let key = key();
        let correct = [Bytes::from_static(b"abcd")];
        store_future(&dir.0, key, &correct).unwrap();
        assert_eq!(
            store_future(&dir.0, key, &[Bytes::from_static(b"short")])
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(read(&dir.0, key).as_deref(), Some(b"abcd".as_slice()));

        let path = dir.0.join(DIR).join(key.slot());
        let blob = fs::read(&path).unwrap();
        let mut bad_digest = blob.clone();
        bad_digest[DIGEST_OFFSET] ^= 1;
        let mut bad_length = blob.clone();
        bad_length[KEY_LEN + 7] ^= 1;
        let mut trailing = blob.clone();
        trailing.push(0);
        for malformed in [
            &blob[..HEADER_LEN - 1],
            &blob[..blob.len() - 1],
            &bad_digest,
            &bad_length,
            &trailing,
        ] {
            fs::write(&path, malformed).unwrap();
            assert!(read(&dir.0, key).is_none());
        }
    }

    #[test]
    fn expired_data_is_neither_published_nor_read() {
        let dir = Fixture::new();
        let key = key();
        let current = [Bytes::from_static(b"abcd")];
        store_future(&dir.0, key, &current).unwrap();
        let now = unix_now().unwrap();
        assert_eq!(
            store(&dir.0, key, &[Bytes::from_static(b"efgh")], now)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(read(&dir.0, key).as_deref(), Some(b"abcd".as_slice()));

        let path = dir.0.join(DIR).join(key.slot());
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(EXPIRY_OFFSET as u64)).unwrap();
        file.write_all(&now.to_be_bytes()).unwrap();
        drop(file);
        assert!(read(&dir.0, key).is_none());
    }

    #[test]
    fn a_competing_writer_leaves_the_published_slot_in_place() {
        let dir = Fixture::new();
        let key = key();
        store_future(&dir.0, key, &[Bytes::from_static(b"abcd")]).unwrap();
        let gate = open_gate(&dir.0.join(DIR)).unwrap();
        gate.lock().unwrap();
        assert!(store_future(&dir.0, key, &[Bytes::from_static(b"efgh")]).is_err());
        assert_eq!(read(&dir.0, key).as_deref(), Some(b"abcd".as_slice()));
        drop(gate);
        store_future(&dir.0, key, &[Bytes::from_static(b"efgh")]).unwrap();
        assert_eq!(read(&dir.0, key).as_deref(), Some(b"efgh".as_slice()));
    }
    #[test]
    fn storage_clears_only_audio_cache_entries_and_its_known_temporary_file() {
        let dir = Fixture::new();
        let key = key();
        for n in 1..=32 {
            let other = Key::new("dQw4w9WgXcQ", 251, n, "en", 4).unwrap();
            store_future(&dir.0, other, &[Bytes::from_static(b"abcd")]).unwrap();
        }
        store_future(&dir.0, key, &[Bytes::from_static(b"abcd")]).unwrap();
        let cache = dir.0.join(DIR);
        let occupied = SLOTS
            .into_iter()
            .filter(|slot| cache.join(slot).is_file())
            .count();
        assert_eq!(occupied, SLOTS.len());
        fs::write(cache.join(TEMP), b"x").unwrap();
        fs::write(dir.0.join("app.ron"), b"settings").unwrap();
        let expected = (occupied * (HEADER_LEN + 4) + 1) as u64;
        assert_eq!(crate::cache::size(&dir.0), expected);
        assert_eq!(crate::cache::clear(&dir.0, true), expected);
        assert_eq!(crate::cache::size(&dir.0), 0);
        assert!(read(&dir.0, key).is_none());
        assert!(cache.join(LOCK).is_file());
        assert!(dir.0.join("app.ron").is_file());
        store_future(&dir.0, key, &[Bytes::from_static(b"new!")]).unwrap();
        assert_eq!(read(&dir.0, key).as_deref(), Some(b"new!".as_slice()));
    }
}
