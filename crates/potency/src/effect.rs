//! File-system side-effect helpers built on the generic [`Effect`]
//! trait.
//!
//! ## When to use `Effect` vs `entry` / `entry_async`
//!
//! [`Store::entry`][crate::Store::entry] and
//! [`Store::entry_async`][crate::Store::entry_async] cache a function's
//! *return value*. Use them when the value is the product.
//!
//! [`Effect`] is for work whose product is *external state* — files on disk,
//! rows in a remote database, anything that can't be undone just by skipping
//! the call. Re-running the function doesn't undo a file being written, so
//! the question isn't "did we run this?" but "is the side effect still in
//! place?" `Effect` answers that by recording a small, serializable
//! [`Manifest`][Effect::Manifest] and verifying the external state on every
//! replay.
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

use crate::{Effect, StoreError};

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

/// Bridge `FsEffectError<E>` into `StoreError`.
///
/// Always defined: the `Io` arm goes via `From<io::Error> for StoreError`,
/// and the `Produce` arm wraps the inner error in a JSON-shaped error
/// carrying the display form so any `E: Display` works without requiring
/// `E: Into<StoreError>`.
impl<E: std::fmt::Display> From<FsEffectError<E>> for StoreError {
    fn from(e: FsEffectError<E>) -> Self {
        match e {
            FsEffectError::Io(io) => io.into(),
            FsEffectError::Produce(inner) => StoreError::Json {
                source: serde_json::Error::io(std::io::Error::other(format!(
                    "produce failed: {inner}"
                ))),
            },
        }
    }
}

/// A directory-producing [`Effect`].
pub struct FsEffect<P> {
    output_dir: PathBuf,
    staging_dir: PathBuf,
    produce: P,
}

/// Build a directory-producing durable effect.
///
/// # Examples
///
/// ```rust,no_run
/// # async fn doc() -> Result<(), potency::EffectError> {
/// use std::path::PathBuf;
/// use potency::{effect::fs_effect, Store};
///
/// async fn render_frames(staging: PathBuf) -> Result<u64, std::io::Error> {
///     std::fs::write(staging.join("frame_0.png"), b"x")?;
///     std::fs::write(staging.join("frame_1.png"), b"x")?;
///     Ok(2)
/// }
///
/// let output = std::env::temp_dir().join("potency-fs-effect-doc");
/// let store = Store::in_memory().await?;
/// let manifest = store
///     .effect(fs_effect(&output, render_frames))
///     .param("default")
///     .run()
///     .await?;
/// assert_eq!(manifest.file_count, 2);
///
/// # let _ = std::fs::remove_dir_all(&output);
/// # Ok(())
/// # }
/// ```
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
    E: std::fmt::Display + Send + 'static,
{
    type Staging = PathBuf;
    type Manifest = FileManifest;
    type Error = FsEffectError<E>;

    fn fresh_staging<'a>(
        &'a self,
        _key: &'a str,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Self::Staging, Self::Error>> + 'a>> {
        Box::pin(async move {
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
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Self::Manifest, Self::Error>> + 'a>> {
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
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), Self::Error>> + 'a>> {
        Box::pin(async move {
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

    fn verify<'a>(
        &'a self,
        manifest: &'a Self::Manifest,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<bool, Self::Error>> + 'a>> {
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

    use crate::{EffectError, Store};

    use super::*;

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

    async fn open_store() -> Store {
        Store::in_memory().await.unwrap()
    }

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
            let store = open_store().await;

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

            let m2 = store
                .namespace("upscale")
                .effect(fs_effect(&out, make_produce(calls.clone(), 5)))
                .param("cfg-hash")
                .run()
                .await
                .unwrap();
            assert_eq!(m2, m1);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "should not recompute on hit"
            );
        });
    }

    #[test]
    fn effect_stale_output_triggers_recompute() {
        smol::block_on(async {
            let tmp = TmpDir::new("stale");
            let out = tmp.path().join("frames_up");
            let calls = Arc::new(AtomicU32::new(0));
            let store = open_store().await;

            store
                .effect(fs_effect(&out, make_produce(calls.clone(), 3)))
                .param("k")
                .run()
                .await
                .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);

            std::fs::remove_dir_all(&out).unwrap();

            store
                .effect(fs_effect(&out, make_produce(calls.clone(), 3)))
                .param("k")
                .run()
                .await
                .unwrap();
            assert_eq!(
                calls.load(Ordering::SeqCst),
                2,
                "stale entry must recompute"
            );
            assert_eq!(count_files(&out), 3);
        });
    }

    #[test]
    fn effect_partial_output_count_mismatch_recomputes() {
        smol::block_on(async {
            let tmp = TmpDir::new("partial");
            let out = tmp.path().join("frames_up");
            let calls = Arc::new(AtomicU32::new(0));
            let store = open_store().await;

            store
                .effect(fs_effect(&out, make_produce(calls.clone(), 4)))
                .param("k")
                .run()
                .await
                .unwrap();

            std::fs::remove_file(out.join("000.txt")).unwrap();
            assert_eq!(count_files(&out), 3);

            store
                .effect(fs_effect(&out, make_produce(calls.clone(), 4)))
                .param("k")
                .run()
                .await
                .unwrap();
            assert_eq!(
                calls.load(Ordering::SeqCst),
                2,
                "count mismatch must recompute"
            );
            assert_eq!(count_files(&out), 4);
        });
    }

    #[test]
    fn effect_durable_with_persistent_store() {
        use std::path::Path;
        smol::block_on(async {
            let tmp = TmpDir::new("persistent");
            let db = tmp.path().join("state.db");
            let out = tmp.path().join("frames_up");
            let calls = Arc::new(AtomicU32::new(0));

            // First process: produce once.
            {
                let store = Store::open(&db).await.unwrap();
                store
                    .namespace("upscale")
                    .effect(fs_effect(&out, make_produce(calls.clone(), 6)))
                    .param("cfg")
                    .run()
                    .await
                    .unwrap();
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1);

            // Second process over the same DB: verified hit, no recompute.
            {
                let store = Store::open(&db).await.unwrap();
                let m = store
                    .namespace("upscale")
                    .effect(fs_effect(&out, make_produce(calls.clone(), 6)))
                    .param("cfg")
                    .run()
                    .await
                    .unwrap();
                assert_eq!(m.file_count, 6);
            }
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "persisted hit, no recompute"
            );

            // Wipe output -> stale -> recompute (exercises DELETE + re-store).
            std::fs::remove_dir_all(&out).unwrap();
            {
                let store = Store::open(&db).await.unwrap();
                store
                    .namespace("upscale")
                    .effect(fs_effect(&out, make_produce(calls.clone(), 6)))
                    .param("cfg")
                    .run()
                    .await
                    .unwrap();
            }
            assert_eq!(
                calls.load(Ordering::SeqCst),
                2,
                "stale after wipe recomputes"
            );
            assert_eq!(count_files(&out), 6);

            let _ = Path::new(&db);
        });
    }

    #[test]
    fn effect_crash_before_store_recomputes_cleanly() {
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

            let store = open_store().await;
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
            let store = open_store().await;

            let produce = |staging: PathBuf| {
                std::fs::write(staging.join("partial.txt"), b"x").unwrap();
                std::future::ready(Err("boom"))
            };
            let result = store
                .effect(fs_effect(&out, produce))
                .param("k")
                .run()
                .await;

            assert!(matches!(result, Err(EffectError::Store(_))));
            assert!(
                !out.exists(),
                "final dir must not exist after failed produce"
            );
            assert!(staging_path(&out).join("partial.txt").exists());
        });
    }
}
