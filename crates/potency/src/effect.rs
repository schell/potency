//! File-system side-effect helpers built on the generic [`Effect`](crate::Effect)
//! trait.
//!
//! The common case for a durable side-effect is "produce a directory of files."
//! [`FsEffect`] implements [`Effect`] for that case:
//!
//! - **staging** is a sibling `*.staging` directory next to the final output,
//! - **commit** is an atomic [`std::fs::rename`] of staging onto the final dir,
//! - **verify** checks the final directory exists and contains exactly
//!   [`FileManifest::file_count`] files.
//!
//! `rename` is atomic only when source and destination are on the same
//! filesystem; staging is placed alongside the output so this holds.

use std::future::Future;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Effect, Stored};

/// A serializable record of a committed directory-producing effect.
///
/// Per the agreed design, verification is by **file count**: cheap, and
/// sufficient to detect a missing or incompletely-produced output directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileManifest {
    /// The final (committed) output directory.
    pub output_dir: PathBuf,
    /// Number of files the committed directory should contain.
    pub file_count: u64,
}

/// Errors produced by [`FsEffect`].
#[derive(Debug)]
pub enum FsEffectError<E> {
    /// An I/O error from staging/commit/verify bookkeeping.
    Io(std::io::Error),
    /// An error from the user-supplied `produce` closure.
    Produce(E),
}

impl<E: std::fmt::Display> std::fmt::Display for FsEffectError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FsEffectError::Io(e) => write!(f, "fs effect io error: {e}"),
            FsEffectError::Produce(e) => write!(f, "fs effect produce error: {e}"),
        }
    }
}

impl<E: std::fmt::Debug + std::fmt::Display> std::error::Error for FsEffectError<E> {}

impl<E> From<std::io::Error> for FsEffectError<E> {
    fn from(e: std::io::Error) -> Self {
        FsEffectError::Io(e)
    }
}

/// A directory-producing [`Effect`].
///
/// Construct via [`fs_effect`]. The `produce` closure receives the staging
/// directory path and must write its output there, returning the number of
/// files it wrote (used to build the [`FileManifest`]).
pub struct FsEffect<P> {
    output_dir: PathBuf,
    staging_dir: PathBuf,
    produce: P,
}

/// Build a directory-producing durable effect.
///
/// - `output_dir` — where the committed files should ultimately live.
/// - `produce` — an async closure `Fn(&Path) -> Future<Output = Result<u64, E>>`
///   that writes files into the provided staging directory and returns the
///   count it wrote.
///
/// The staging directory is `output_dir` with `.staging` appended, kept as a
/// sibling so the commit rename is atomic.
pub fn fs_effect<P>(output_dir: impl Into<PathBuf>, produce: P) -> FsEffect<P> {
    let output_dir = output_dir.into();
    let staging_dir = staging_path(&output_dir);
    FsEffect {
        output_dir,
        staging_dir,
        produce,
    }
}

fn staging_path(output_dir: &Path) -> PathBuf {
    let mut s = output_dir.as_os_str().to_owned();
    s.push(".staging");
    PathBuf::from(s)
}

/// Count regular files (non-directories) directly inside `dir`.
fn count_files(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|entries| entries.flatten().filter(|e| e.path().is_file()).count() as u64)
        .unwrap_or(0)
}

impl<P, Fut, E> Effect for FsEffect<P>
where
    P: Fn(PathBuf) -> Fut,
    Fut: Future<Output = Result<u64, E>>,
{
    type Staging = PathBuf;
    type Manifest = FileManifest;
    type Error = FsEffectError<E>;

    fn fresh_staging<'a>(&'a self, _key: &'a str) -> Stored<'a, Self::Staging, Self::Error> {
        Box::pin(async move {
            // Clear any leftover staging from a crashed attempt, then recreate.
            if self.staging_dir.exists() {
                std::fs::remove_dir_all(&self.staging_dir)?;
            }
            std::fs::create_dir_all(&self.staging_dir)?;
            Ok(self.staging_dir.clone())
        })
    }

    fn produce<'a>(
        &'a self,
        staging: &'a Self::Staging,
    ) -> Stored<'a, Self::Manifest, Self::Error> {
        Box::pin(async move {
            let count = (self.produce)(staging.clone())
                .await
                .map_err(FsEffectError::Produce)?;
            Ok(FileManifest {
                output_dir: self.output_dir.clone(),
                file_count: count,
            })
        })
    }

    fn commit<'a>(
        &'a self,
        staging: &'a Self::Staging,
        _manifest: &'a Self::Manifest,
    ) -> Stored<'a, (), Self::Error> {
        Box::pin(async move {
            // Remove any pre-existing (stale) output dir, then atomically
            // rename staging into place.
            if self.output_dir.exists() {
                std::fs::remove_dir_all(&self.output_dir)?;
            }
            if let Some(parent) = self.output_dir.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(staging, &self.output_dir)?;
            Ok(())
        })
    }

    fn verify<'a>(&'a self, manifest: &'a Self::Manifest) -> Stored<'a, bool, Self::Error> {
        Box::pin(async move {
            if !manifest.output_dir.is_dir() {
                return Ok(false);
            }
            Ok(count_files(&manifest.output_dir) == manifest.file_count)
        })
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    use crate::{cpu_store::CpuStore, EffectError, Store};

    use super::*;

    /// A test work directory that is cleaned up on drop.
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("potency-effect-test-{name}"));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Build a produce closure that writes `n` files and bumps a call counter.
    fn make_produce(
        calls: Arc<AtomicU32>,
        n: u64,
    ) -> impl Fn(PathBuf) -> std::future::Ready<Result<u64, std::convert::Infallible>> {
        move |staging: PathBuf| {
            calls.fetch_add(1, Ordering::SeqCst);
            for i in 0..n {
                std::fs::write(staging.join(format!("{i:03}.txt")), b"x").unwrap();
            }
            std::future::ready(Ok(n))
        }
    }

    #[test]
    fn effect_hit_skips_recompute() {
        smol::block_on(async {
            let tmp = TmpDir::new("hit");
            let out = tmp.path().join("frames_up");
            let calls = Arc::new(AtomicU32::new(0));
            let store = Store::new(CpuStore::new());

            // First run: miss -> produces.
            let m1 = store
                .namespace("upscale")
                .effect(fs_effect(&out, make_produce(calls.clone(), 5)))
                .param("cfg-hash")
                .run()
                .await
                .unwrap();
            assert_eq!(m1.file_count, 5);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(count_files(&out), 5);

            // Second run: verified hit -> NO recompute.
            let m2 = store
                .namespace("upscale")
                .effect(fs_effect(&out, make_produce(calls.clone(), 5)))
                .param("cfg-hash")
                .run()
                .await
                .unwrap();
            assert_eq!(m2, m1);
            assert_eq!(calls.load(Ordering::SeqCst), 1, "should not recompute on hit");
        });
    }

    #[test]
    fn effect_stale_output_triggers_recompute() {
        smol::block_on(async {
            let tmp = TmpDir::new("stale");
            let out = tmp.path().join("frames_up");
            let calls = Arc::new(AtomicU32::new(0));
            let store = Store::new(CpuStore::new());

            store
                .effect(fs_effect(&out, make_produce(calls.clone(), 3)))
                .param("k")
                .run()
                .await
                .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);

            // Simulate the workdir being wiped between runs.
            std::fs::remove_dir_all(&out).unwrap();

            // Cache entry exists, but verify() fails -> recompute.
            store
                .effect(fs_effect(&out, make_produce(calls.clone(), 3)))
                .param("k")
                .run()
                .await
                .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 2, "stale entry must recompute");
            assert_eq!(count_files(&out), 3);
        });
    }

    #[test]
    fn effect_partial_output_count_mismatch_recomputes() {
        smol::block_on(async {
            let tmp = TmpDir::new("partial");
            let out = tmp.path().join("frames_up");
            let calls = Arc::new(AtomicU32::new(0));
            let store = Store::new(CpuStore::new());

            store
                .effect(fs_effect(&out, make_produce(calls.clone(), 4)))
                .param("k")
                .run()
                .await
                .unwrap();

            // Corrupt the committed output: remove one file so count != manifest.
            std::fs::remove_file(out.join("000.txt")).unwrap();
            assert_eq!(count_files(&out), 3);

            store
                .effect(fs_effect(&out, make_produce(calls.clone(), 4)))
                .param("k")
                .run()
                .await
                .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 2, "count mismatch must recompute");
            assert_eq!(count_files(&out), 4);
        });
    }

    #[cfg(feature = "sqlite-store")]
    #[test]
    fn effect_durable_with_sqlite_store() {
        use crate::sqlite_store::SqliteStore;
        smol::block_on(async {
            let tmp = TmpDir::new("sqlite");
            let db = tmp.path().join("state.db");
            let out = tmp.path().join("frames_up");
            let calls = Arc::new(AtomicU32::new(0));

            // Use a persistent DB file so a fresh Store (simulating a new
            // process invocation) sees the committed entry.
            {
                let store = Store::new(SqliteStore::open(&db).await.unwrap());
                store
                    .namespace("upscale")
                    .effect(fs_effect(&out, make_produce(calls.clone(), 6)))
                    .param("cfg")
                    .run()
                    .await
                    .unwrap();
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1);

            // New Store over the same DB: verified hit, no recompute.
            {
                let store = Store::new(SqliteStore::open(&db).await.unwrap());
                let m = store
                    .namespace("upscale")
                    .effect(fs_effect(&out, make_produce(calls.clone(), 6)))
                    .param("cfg")
                    .run()
                    .await
                    .unwrap();
                assert_eq!(m.file_count, 6);
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1, "persisted hit, no recompute");

            // Wipe output -> stale -> recompute (exercises DELETE + re-store).
            std::fs::remove_dir_all(&out).unwrap();
            {
                let store = Store::new(SqliteStore::open(&db).await.unwrap());
                store
                    .namespace("upscale")
                    .effect(fs_effect(&out, make_produce(calls.clone(), 6)))
                    .param("cfg")
                    .run()
                    .await
                    .unwrap();
            }
            assert_eq!(calls.load(Ordering::SeqCst), 2, "stale after wipe recomputes");
            assert_eq!(count_files(&out), 6);
        });
    }

    #[test]
    fn effect_crash_before_store_recomputes_cleanly() {
        // Simulate a crash AFTER commit but BEFORE the manifest is stored.
        // Per the invariant, no cache entry exists, so the next run must
        // recompute cleanly (and the prior committed dir is replaced, not
        // corrupted).
        smol::block_on(async {
            let tmp = TmpDir::new("crash");
            let out = tmp.path().join("frames_up");
            let calls = Arc::new(AtomicU32::new(0));

            // Manually drive produce + commit, but do NOT store (the "crash").
            let effect = fs_effect(&out, make_produce(calls.clone(), 2));
            let staging = effect.fresh_staging("k").await.unwrap();
            let manifest = effect.produce(&staging).await.unwrap();
            effect.commit(&staging, &manifest).await.unwrap();
            assert!(out.is_dir());
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            // (no store call — simulating a crash here)

            // Fresh run with an empty store: cache miss -> recompute, no corruption.
            let store = Store::new(CpuStore::new());
            let m = store
                .effect(fs_effect(&out, make_produce(calls.clone(), 2)))
                .param("k")
                .run()
                .await
                .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 2, "no entry -> recompute");
            assert_eq!(m.file_count, 2);
            assert_eq!(count_files(&out), 2, "clean output, not doubled/corrupt");
        });
    }

    #[test]
    fn effect_atomic_commit_no_partial_final_on_produce_failure() {
        smol::block_on(async {
            let tmp = TmpDir::new("atomic");
            let out = tmp.path().join("frames_up");
            let store = Store::new(CpuStore::new());

            // produce writes a file then fails: commit must NOT run, so the
            // final output dir must not exist (work stayed in staging).
            let produce = |staging: PathBuf| {
                std::fs::write(staging.join("partial.txt"), b"x").unwrap();
                std::future::ready(Err("boom"))
            };
            let result = store
                .effect(fs_effect(&out, produce))
                .param("k")
                .run()
                .await;

            assert!(matches!(result, Err(EffectError::Effect(_))));
            assert!(!out.exists(), "final dir must not exist after failed produce");
            // Staging may exist with the partial file; it is cleared on the
            // next attempt's fresh_staging.
            assert!(staging_path(&out).join("partial.txt").exists());
        });
    }
}
