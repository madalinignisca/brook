//! Snapshots (attachments spec §3): what goes in comes out, only from the right place, only
//! whole, and never from another snapshot.

use std::path::Path;

use crate::snapshot::{self, read_all, verify, Damaged, SnapshotSource, Written, CHUNK};

const ID: &str = "0190a000-0000-7000-8000-00000000f11e";
const SMALL: usize = 8;

fn id() -> [u8; 16] {
    snapshot::id_bytes(ID).unwrap()
}

fn write(dir: &Path, name: &str, bytes: &[u8], chunk: usize) -> (std::path::PathBuf, Written) {
    let src = dir.join(format!("{name}.src"));
    std::fs::write(&src, bytes).unwrap();
    let dst = dir.join(format!("{name}.snap"));
    let w = snapshot::write(&src, &dst, id(), chunk, u64::MAX, &mut |_, _| true).unwrap();
    (dst, w)
}

fn source(path: &Path, w: &Written, chunk: usize) -> SnapshotSource {
    SnapshotSource {
        path: path.to_path_buf(),
        key: w.key,
        id: id(),
        size: w.size,
        sha256: w.sha256.clone(),
        chunk,
        broken: Default::default(),
    }
}

fn check(path: &Path, w: &Written, chunk: usize) -> Result<(), Damaged> {
    verify(path, &w.key, id(), w.size, &w.sha256, chunk)
}

#[tokio::test]
async fn a_snapshot_reads_back_what_was_written() {
    let dir = tempfile::tempdir().unwrap();
    for (name, len) in [
        ("short", 3usize),
        ("exact", SMALL * 2),
        ("ragged", SMALL * 3 + 5),
    ] {
        let bytes: Vec<u8> = (0..len).map(|i| i as u8).collect();
        let (path, w) = write(dir.path(), name, &bytes, SMALL);
        assert_eq!(w.size, len as u64);
        check(&path, &w, SMALL).unwrap();
        assert_eq!(
            read_all(&source(&path, &w, SMALL)).await.unwrap(),
            bytes,
            "{name}"
        );
        let on_disk = std::fs::read(&path).unwrap();
        assert!(
            !on_disk.windows(3).any(|win| win == &bytes[..3]),
            "{name}: plaintext on disk"
        );
    }
}

/// The production chunk size, across a chunk boundary.
#[tokio::test]
async fn the_real_chunk_size_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let bytes: Vec<u8> = (0..CHUNK + 1000).map(|i| (i % 251) as u8).collect();
    let (path, w) = write(dir.path(), "big", &bytes, CHUNK);
    check(&path, &w, CHUNK).unwrap();
    assert_eq!(read_all(&source(&path, &w, CHUNK)).await.unwrap(), bytes);
}

/// Every write has its own key: the same file twice is different ciphertext.
#[test]
fn two_writes_never_share_a_key() {
    let dir = tempfile::tempdir().unwrap();
    let (a, wa) = write(dir.path(), "a", b"same bytes here", SMALL);
    let (b, wb) = write(dir.path(), "b", b"same bytes here", SMALL);
    assert_ne!(wa.key, wb.key);
    assert_ne!(std::fs::read(a).unwrap(), std::fs::read(b).unwrap());
}

async fn damaged(path: &Path, w: &Written) {
    assert_eq!(check(path, w, SMALL), Err(Damaged), "verify accepted it");
    assert!(
        read_all(&source(path, w, SMALL)).await.is_err(),
        "the reader accepted it"
    );
}

#[tokio::test]
async fn a_flipped_byte_is_damage() {
    let dir = tempfile::tempdir().unwrap();
    let (path, w) = write(dir.path(), "f", &[7u8; SMALL * 3], SMALL);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[SMALL + 20] ^= 1;
    std::fs::write(&path, bytes).unwrap();
    damaged(&path, &w).await;
}

#[tokio::test]
async fn swapped_chunks_are_damage() {
    let dir = tempfile::tempdir().unwrap();
    let data: Vec<u8> = (0..SMALL as u8 * 3).collect();
    let (path, w) = write(dir.path(), "s", &data, SMALL);
    let bytes = std::fs::read(&path).unwrap();
    let sealed = SMALL + 16;
    let mut swapped = bytes[sealed..2 * sealed].to_vec();
    swapped.extend_from_slice(&bytes[..sealed]);
    swapped.extend_from_slice(&bytes[2 * sealed..]);
    std::fs::write(&path, swapped).unwrap();
    damaged(&path, &w).await;
}

/// Cut at a chunk boundary: the new "last" chunk was sealed as not-last. (The stored size
/// is told the shorter length, as a damaged row might.)
#[tokio::test]
async fn a_truncated_snapshot_is_damage() {
    let dir = tempfile::tempdir().unwrap();
    let data: Vec<u8> = (0..SMALL as u8 * 3).collect();
    let (path, mut w) = write(dir.path(), "t", &data, SMALL);
    let bytes = std::fs::read(&path).unwrap();
    std::fs::write(&path, &bytes[..2 * (SMALL + 16)]).unwrap();
    w.size = (SMALL * 2) as u64;
    let digest = ring::digest::digest(&ring::digest::SHA256, &data[..SMALL * 2]);
    w.sha256 = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    damaged(&path, &w).await;
}

/// Bytes after the last chunk.
#[tokio::test]
async fn trailing_bytes_are_damage() {
    let dir = tempfile::tempdir().unwrap();
    let (path, w) = write(dir.path(), "x", &[1u8; SMALL + 3], SMALL);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend_from_slice(&[0u8; 20]);
    std::fs::write(&path, bytes).unwrap();
    damaged(&path, &w).await;
}

/// Another file's snapshot (same key even) doesn't open under this file's id.
#[tokio::test]
async fn a_snapshot_under_another_id_is_damage() {
    let dir = tempfile::tempdir().unwrap();
    let (path, w) = write(dir.path(), "o", &[5u8; SMALL * 2], SMALL);
    let other = snapshot::id_bytes("0190a000-0000-7000-8000-00000000beef").unwrap();
    assert_eq!(
        verify(&path, &w.key, other, w.size, &w.sha256, SMALL),
        Err(Damaged)
    );
}

/// The source changes after the write: the snapshot keeps what was queued.
#[tokio::test]
async fn a_changed_source_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (path, w) = write(dir.path(), "c", b"what was queued", SMALL);
    std::fs::write(dir.path().join("c.src"), b"edited later!!!").unwrap();
    assert_eq!(
        read_all(&source(&path, &w, SMALL)).await.unwrap(),
        b"what was queued"
    );
}

#[test]
fn progress_follows_the_copy_and_empty_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("p.src");
    std::fs::write(&src, [9u8; SMALL * 2 + 1]).unwrap();
    let mut seen = vec![];
    snapshot::write(
        &src,
        &dir.path().join("p.snap"),
        id(),
        SMALL,
        u64::MAX,
        &mut |d, t| {
            seen.push((d, t));
            true
        },
    )
    .unwrap();
    assert_eq!(
        seen.last(),
        Some(&((SMALL * 2 + 1) as u64, (SMALL * 2 + 1) as u64))
    );
    assert_eq!(seen.len(), 3);
    let empty = dir.path().join("e.src");
    std::fs::write(&empty, b"").unwrap();
    let dst = dir.path().join("e.snap");
    assert!(snapshot::write(&empty, &dst, id(), SMALL, u64::MAX, &mut |_, _| true).is_err());
    assert!(!dst.exists(), "a partial snapshot was left");
}

/// Never over an existing snapshot, never into a directory that isn't there.
#[test]
fn a_write_never_replaces_or_makes_directories() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = write(dir.path(), "n", b"first", SMALL);
    let src = dir.path().join("n.src");
    assert!(snapshot::write(&src, &path, id(), SMALL, u64::MAX, &mut |_, _| true).is_err());
    let gone = dir.path().join("no-such-dir").join("x.snap");
    assert!(snapshot::write(&src, &gone, id(), SMALL, u64::MAX, &mut |_, _| true).is_err());
    assert!(!dir.path().join("no-such-dir").exists());
}

/// A row whose stored checksum doesn't match (damaged, or written for other bytes) fails
/// verification even though every chunk opens: the ciphertext alone can't vouch for the row.
#[test]
fn a_wrong_stored_checksum_is_damage() {
    let dir = tempfile::tempdir().unwrap();
    let (path, mut w) = write(dir.path(), "h", b"checked twice", SMALL);
    w.sha256 = "0".repeat(64);
    assert_eq!(check(&path, &w, SMALL), Err(Damaged));
}

/// A source that grows past the limit while it's copied is stopped at the limit (a size read
/// before the copy can't vouch for it), and nothing is left.
#[test]
fn a_source_over_the_limit_is_stopped_mid_copy() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("g.src");
    std::fs::write(&src, [1u8; SMALL * 4]).unwrap();
    let dst = dir.path().join("g.snap");
    let got = snapshot::write(&src, &dst, id(), SMALL, (SMALL * 2) as u64, &mut |_, _| {
        true
    });
    assert!(
        matches!(got, Err(snapshot::WriteError::TooLarge)),
        "{:?}",
        got.err()
    );
    assert!(!dst.exists());
}

/// The download sealer writes the same format as the snapshot copy: byte for byte under the
/// same key, whatever sizes the bytes arrive in.
#[tokio::test]
async fn the_streaming_sealer_writes_the_snapshot_format() {
    use snapshot::{Layout, Sealer};
    let dir = tempfile::tempdir().unwrap();
    for len in [1usize, SMALL, SMALL * 2, SMALL * 3 + 5] {
        let bytes: Vec<u8> = (0..len).map(|i| (i * 7) as u8).collect();
        let (path, w) = write(dir.path(), &format!("s{len}"), &bytes, SMALL);
        let layout = Layout {
            chunk: SMALL,
            size: len as u64,
        };
        for piece in [1usize, 3, SMALL, 64] {
            let mut sealer = Sealer::new(&w.key, id(), layout, 0);
            let mut out = Vec::new();
            for part in bytes.chunks(piece) {
                out.extend(sealer.push(part).unwrap());
            }
            assert!(sealer.is_complete());
            assert_eq!(
                out,
                std::fs::read(&path).unwrap(),
                "len {len}, pieces of {piece}"
            );
        }
    }
}

/// A resumed download, sealing again from a chunk boundary under the same key, gives the same
/// ciphertext as one pass; a byte past the size is refused.
#[tokio::test]
async fn a_sealer_resumes_at_a_chunk_boundary_and_stops_at_the_size() {
    use snapshot::{Layout, SealError, Sealer};
    let key = [9u8; 32];
    let bytes: Vec<u8> = (0..SMALL * 3 + 2).map(|i| i as u8).collect();
    let layout = Layout {
        chunk: SMALL,
        size: bytes.len() as u64,
    };
    let mut one = Sealer::new(&key, id(), layout, 0);
    let whole = one.push(&bytes).unwrap();

    let mut first = Sealer::new(&key, id(), layout, 0);
    let mut resumed = first.push(&bytes[..SMALL * 2 + 3]).unwrap(); // 2 chunks + 3 buffered
    assert_eq!(first.sealed_chunks(), 2);
    // The buffered bytes were never on disk: the resume starts at chunk 2.
    let mut second = Sealer::new(&key, id(), layout, 2);
    resumed.extend(second.push(&bytes[SMALL * 2..]).unwrap());
    assert_eq!(resumed, whole);
    assert!(second.is_complete());
    assert_eq!(second.push(&[0]), Err(SealError::TooLong));
    assert_eq!(
        layout.sealed_offset(2) as usize,
        resumed.len() - (SMALL + 16 + 2 + 16)
    );
}
