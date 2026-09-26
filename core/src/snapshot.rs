//! Outbox attachment snapshots (spec 2026-09-26-outbox-attachments-spec.md §3; offline-cache
//! design §6.1): a file the user queued, copied at enqueue into the user's store as
//! ciphertext, so later edits, moves or deletion of the source change nothing, and nothing of
//! it lies on disk in the clear.
//!
//! Format: the plaintext in `chunk`-byte chunks (the last may be shorter), each sealed with
//! AES-256-GCM under a key made for this one write, one after another. The nonce is the
//! chunk index (96-bit big-endian): unique, because the key is never used for another write.
//! The additional data binds each chunk to its file, its place and whether it ends the file:
//! `file_client_id` (16 bytes) | index (u64 BE) | last-chunk flag (1 byte). So reordering,
//! a chunk from another snapshot, truncation at a chunk boundary (the new "last" chunk was
//! sealed as not-last) and trailing bytes all fail to open.
//!
//! The functions here are blocking (whole files, crypto, fsync): callers run them in
//! `spawn_blocking`, never on the runtime's workers. The decrypting reader for uploads is
//! async.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use tokio::io::{AsyncRead, ReadBuf};

/// The production chunk size. Tests pass smaller ones through the same code.
pub(crate) const CHUNK: usize = 1 << 20;
const TAG: usize = 16;

/// What a write produced, all of it stored in the snapshot's outbox row.
pub(crate) struct Written {
    pub(crate) key: [u8; 32],
    /// Plaintext bytes actually copied (not a size read before the copy).
    pub(crate) size: u64,
    /// Hex sha256 of the plaintext.
    pub(crate) sha256: String,
}

/// Why a write failed: the user's file (gone, unreadable, empty), our store (full disk), or
/// the caller stopped it (a cancel while copying).
#[derive(Debug)]
pub(crate) enum WriteError {
    Source,
    Store,
    Stopped,
    /// The source grew past the limit while it was being copied.
    TooLarge,
}

/// A snapshot that can't be trusted: not uploaded, never retried (`outbox.snapshot_damaged`).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Damaged;

/// The 16 bytes of a canonical UUID (`canonical_client_id`'s form).
pub(crate) fn id_bytes(canonical: &str) -> Option<[u8; 16]> {
    let hex: Vec<u8> = canonical.bytes().filter(|&b| b != b'-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, pair) in hex.chunks(2).enumerate() {
        let s = std::str::from_utf8(pair).ok()?;
        out[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(out)
}

fn aad(id: &[u8; 16], index: u64, last: bool) -> [u8; 25] {
    let mut a = [0u8; 25];
    a[..16].copy_from_slice(id);
    a[16..24].copy_from_slice(&index.to_be_bytes());
    a[24] = u8::from(last);
    a
}

fn nonce(index: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&index.to_be_bytes());
    Nonce::assume_unique_for_key(n)
}

fn sealing_key(key: &[u8; 32]) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).expect("a 32-byte key"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Read until `buf` is full or the reader ends; the bytes read.
fn fill(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Copy `src` into a new snapshot at `dst`, sealed under a fresh key. `src` is opened once
/// and read once; `dst` must not exist, and its directory must (it is never created here:
/// a write racing a wipe of the store fails instead of recreating it). The file and its
/// directory are fsynced before this returns. `progress(done, total)` follows the copy
/// (`total` is the size seen when it started); returning false stops it (nothing is kept).
/// An empty source is refused.
pub(crate) fn write(
    src: &Path,
    dst: &Path,
    id: [u8; 16],
    chunk: usize,
    max: u64,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Written, WriteError> {
    let mut input = File::open(src).map_err(|_| WriteError::Source)?;
    let total = input.metadata().map(|m| m.len()).unwrap_or(0);
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|_| WriteError::Store)?;
    let sealing = sealing_key(&key);
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)
        .map_err(|_| WriteError::Store)?;
    let result = (|| {
        let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
        let mut current = vec![0u8; chunk];
        let mut n = fill(&mut input, &mut current).map_err(|_| WriteError::Source)?;
        if n == 0 {
            return Err(WriteError::Source); // empty
        }
        let mut next = vec![0u8; chunk];
        let (mut index, mut size) = (0u64, 0u64);
        loop {
            // A chunk is the last one if nothing follows it: read ahead before sealing.
            let m = if n == chunk {
                fill(&mut input, &mut next).map_err(|_| WriteError::Source)?
            } else {
                0
            };
            let last = m == 0;
            digest.update(&current[..n]);
            let sealed =
                seal_chunk(&sealing, &id, index, last, &current[..n]).ok_or(WriteError::Store)?;
            out.write_all(&sealed).map_err(|_| WriteError::Store)?;
            size += n as u64;
            // Checked on the bytes copied, not only a size read before: a file still being
            // written (a log, a download) can't grow a snapshot past the limit.
            if size > max {
                return Err(WriteError::TooLarge);
            }
            if !progress(size, total.max(size)) {
                return Err(WriteError::Stopped);
            }
            if last {
                break;
            }
            std::mem::swap(&mut current, &mut next);
            n = m;
            index += 1;
        }
        out.sync_all().map_err(|_| WriteError::Store)?;
        if let Some(dir) = dst.parent() {
            File::open(dir)
                .and_then(|d| d.sync_all())
                .map_err(|_| WriteError::Store)?;
        }
        Ok(Written {
            key,
            size,
            sha256: hex(digest.finish().as_ref()),
        })
    })();
    if result.is_err() {
        drop(out);
        let _ = std::fs::remove_file(dst); // a partial snapshot is never left for a row
    }
    result
}

/// Seal one chunk: the one sealing step every writer shares (the snapshot copy, the cache's
/// downloads), so both produce the same format.
fn seal_chunk(
    key: &LessSafeKey,
    id: &[u8; 16],
    index: u64,
    last: bool,
    plain: &[u8],
) -> Option<Vec<u8>> {
    let mut sealed = plain.to_vec();
    key.seal_in_place_append_tag(nonce(index), Aad::from(aad(id, index, last)), &mut sealed)
        .ok()?;
    Some(sealed)
}

/// How a snapshot of `size` bytes is laid out: the chunk count and each chunk's sealed length.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Layout {
    pub(crate) chunk: usize,
    pub(crate) size: u64,
}

impl Layout {
    pub(crate) fn chunks(&self) -> u64 {
        self.size.div_ceil(self.chunk as u64)
    }
    fn plain_len(&self, index: u64) -> usize {
        let start = index * self.chunk as u64;
        (self.size - start).min(self.chunk as u64) as usize
    }
    fn sealed_len(&self, index: u64) -> usize {
        self.plain_len(index) + TAG
    }
    pub(crate) fn file_len(&self) -> u64 {
        self.size + self.chunks() * TAG as u64
    }
    /// Where chunk `index` starts in the sealed file (every chunk before it is full).
    pub(crate) fn sealed_offset(&self, index: u64) -> u64 {
        index * (self.chunk + TAG) as u64
    }
}

/// Seals a stream whose total size is known up front (a download: `FileOut.size`), chunk by
/// chunk as the bytes arrive, so what's on disk is always ciphertext. The last-chunk flag
/// comes from the size, not from reading ahead. It can start at any chunk boundary (a resumed
/// download) under the key the earlier chunks were sealed with: the caller guarantees the
/// bytes it pushes are the same file's (the resume invariant: a `206` to `If-Range` with the
/// file's sha256).
pub(crate) struct Sealer {
    key: LessSafeKey,
    id: [u8; 16],
    layout: Layout,
    index: u64,
    buf: Vec<u8>,
}

impl Sealer {
    /// Start sealing at chunk `index` (0 for a new file).
    pub(crate) fn new(key: &[u8; 32], id: [u8; 16], layout: Layout, index: u64) -> Self {
        Self {
            key: sealing_key(key),
            id,
            layout,
            index,
            buf: Vec::with_capacity(layout.chunk),
        }
    }

    /// Take the next plaintext bytes; returns the sealed bytes of every chunk they completed
    /// (possibly none). More bytes than the size allows is an error.
    pub(crate) fn push(&mut self, mut bytes: &[u8]) -> Result<Vec<u8>, SealError> {
        let mut out = Vec::new();
        while !bytes.is_empty() {
            if self.index >= self.layout.chunks() {
                return Err(SealError::TooLong);
            }
            let want = self.layout.plain_len(self.index) - self.buf.len();
            let take = want.min(bytes.len());
            self.buf.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.buf.len() == self.layout.plain_len(self.index) {
                let last = self.index + 1 == self.layout.chunks();
                let sealed = seal_chunk(&self.key, &self.id, self.index, last, &self.buf)
                    .ok_or(SealError::Crypto)?;
                out.extend_from_slice(&sealed);
                self.buf.clear();
                self.index += 1;
            }
        }
        Ok(out)
    }

    /// Chunks sealed so far (the next one's index).
    pub(crate) fn sealed_chunks(&self) -> u64 {
        self.index
    }

    /// Plaintext bytes taken but not sealed yet (part of the next chunk).
    pub(crate) fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Every chunk is sealed, the last one flagged.
    pub(crate) fn is_complete(&self) -> bool {
        self.index == self.layout.chunks()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SealError {
    /// More bytes than the declared size.
    TooLong,
    Crypto,
}

/// Open one sealed chunk in place; its plaintext length on success.
fn open_chunk(
    key: &LessSafeKey,
    id: &[u8; 16],
    index: u64,
    last: bool,
    sealed: &mut [u8],
) -> Result<usize, Damaged> {
    key.open_in_place(nonce(index), Aad::from(aad(id, index, last)), sealed)
        .map(|p| p.len())
        .map_err(|_| Damaged)
}

/// Check a whole snapshot: every chunk opens in its place, the last one is flagged, nothing
/// trails it, and the plaintext's size and sha256 are the stored ones. A read only; no
/// plaintext is written anywhere.
pub(crate) fn verify(
    path: &Path,
    key: &[u8; 32],
    id: [u8; 16],
    size: u64,
    sha256: &str,
    chunk: usize,
) -> Result<(), Damaged> {
    let layout = Layout { chunk, size };
    let mut file = File::open(path).map_err(|_| Damaged)?;
    if size == 0 || file.metadata().map_err(|_| Damaged)?.len() != layout.file_len() {
        return Err(Damaged);
    }
    let opening = sealing_key(key);
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; chunk + TAG];
    let n = layout.chunks();
    for index in 0..n {
        let len = layout.sealed_len(index);
        if fill(&mut file, &mut buf[..len]).map_err(|_| Damaged)? != len {
            return Err(Damaged);
        }
        let plain = open_chunk(&opening, &id, index, index + 1 == n, &mut buf[..len])?;
        digest.update(&buf[..plain]);
    }
    if hex(digest.finish().as_ref()) != sha256 {
        return Err(Damaged);
    }
    Ok(())
}

/// A snapshot as an upload's bytes: decrypted chunk by chunk as the PUT reads it.
pub(crate) struct SnapshotSource {
    pub(crate) path: PathBuf,
    pub(crate) key: [u8; 32],
    pub(crate) id: [u8; 16],
    pub(crate) size: u64,
    pub(crate) sha256: String,
    pub(crate) chunk: usize,
    /// Set if a read met damage (a chunk that won't open): the upload's error then reads as
    /// a network failure, and the outbox needs to know it was the snapshot.
    pub(crate) broken: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl crate::transfer::UploadSource for SnapshotSource {
    fn len(&self) -> u64 {
        self.size
    }
    async fn sha256(&self) -> io::Result<String> {
        Ok(self.sha256.clone())
    }
    async fn reader(&self) -> io::Result<Box<dyn AsyncRead + Send + Unpin>> {
        let file = tokio::fs::File::open(&self.path).await?;
        Ok(Box::new(Decrypting {
            file,
            key: sealing_key(&self.key),
            id: self.id,
            layout: Layout {
                chunk: self.chunk,
                size: self.size,
            },
            index: 0,
            sealed: vec![0u8; self.chunk + TAG],
            filled: 0,
            plain: 0,
            pos: 0,
            broken: self.broken.clone(),
        }))
    }
}

/// An `AsyncRead` of a snapshot's plaintext. A chunk that fails to open (or a file that
/// ends early or runs on) is an `InvalidData` error: the PUT stops, and the outbox treats it
/// as a local fault that re-verifies before the next attempt.
struct Decrypting {
    file: tokio::fs::File,
    key: LessSafeKey,
    id: [u8; 16],
    layout: Layout,
    index: u64,
    /// The chunk being read, sealed; after opening, its plaintext is `sealed[..plain]`.
    sealed: Vec<u8>,
    filled: usize,
    plain: usize,
    pos: usize,
    broken: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

fn damaged() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "snapshot damaged")
}

impl AsyncRead for Decrypting {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        loop {
            if me.pos < me.plain {
                let n = (me.plain - me.pos).min(out.remaining());
                out.put_slice(&me.sealed[me.pos..me.pos + n]);
                me.pos += n;
                return Poll::Ready(Ok(()));
            }
            let chunks = me.layout.chunks();
            if me.index == chunks {
                // Past the last chunk: anything more in the file is not ours.
                let mut probe = [0u8; 1];
                let mut rb = ReadBuf::new(&mut probe);
                return match Pin::new(&mut me.file).poll_read(cx, &mut rb) {
                    Poll::Ready(Ok(())) if rb.filled().is_empty() => Poll::Ready(Ok(())),
                    Poll::Ready(Ok(())) => {
                        me.broken.store(true, std::sync::atomic::Ordering::SeqCst);
                        Poll::Ready(Err(damaged()))
                    }
                    Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                    Poll::Pending => Poll::Pending,
                };
            }
            let want = me.layout.sealed_len(me.index);
            while me.filled < want {
                let mut rb = ReadBuf::new(&mut me.sealed[me.filled..want]);
                match Pin::new(&mut me.file).poll_read(cx, &mut rb) {
                    Poll::Ready(Ok(())) if rb.filled().is_empty() => {
                        me.broken.store(true, std::sync::atomic::Ordering::SeqCst);
                        return Poll::Ready(Err(damaged())); // ended early
                    }
                    Poll::Ready(Ok(())) => me.filled += rb.filled().len(),
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => return Poll::Pending,
                }
            }
            let last = me.index + 1 == chunks;
            match open_chunk(&me.key, &me.id, me.index, last, &mut me.sealed[..want]) {
                Ok(n) => {
                    me.plain = n;
                    me.pos = 0;
                    me.filled = 0;
                    me.index += 1;
                }
                Err(Damaged) => {
                    me.broken.store(true, std::sync::atomic::Ordering::SeqCst);
                    return Poll::Ready(Err(damaged()));
                }
            }
        }
    }
}

/// Read a whole snapshot's plaintext (tests).
#[cfg(test)]
pub(crate) async fn read_all(source: &SnapshotSource) -> io::Result<Vec<u8>> {
    use crate::transfer::UploadSource;
    use tokio::io::AsyncReadExt;
    let mut r = source.reader().await?;
    let mut v = Vec::new();
    r.read_to_end(&mut v).await?;
    Ok(v)
}
