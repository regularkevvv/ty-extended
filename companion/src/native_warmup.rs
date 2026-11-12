//! Overlap a disposable native check with immutable source and plugin indexing.
//! Its diagnostics are discarded. The ordinary synchronized check still runs.
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    thread::JoinHandle,
    time::Instant,
};

use anyhow::{Context, Result, anyhow, ensure};

use crate::checker::ProjectChecker;

pub struct NativeWarmup {
    thread: Option<JoinHandle<Result<ProjectChecker>>>,
}

impl NativeWarmup {
    pub fn start(
        binary: &Path,
        original: &Path,
        out: &Path,
        config: &Path,
        paths: Vec<PathBuf>,
        extra_roots: Vec<PathBuf>,
        canonical_sources: Arc<HashSet<PathBuf>>,
    ) -> Result<Self> {
        let binary = binary.to_owned();
        let original = original.to_owned();
        let out = out.to_owned();
        let config = config.to_owned();
        let thread = std::thread::Builder::new()
            .name("native-check-warmup".into())
            .spawn(move || {
                let tracing = std::env::var_os("TY_COMPANION_TRACE_PHASES").is_some();
                let mut tick = Instant::now();
                let mut trace = |name| {
                    if tracing {
                        eprintln!(
                            "companion native warmup {name}: {:.6}s",
                            tick.elapsed().as_secs_f64()
                        );
                    }
                    tick = Instant::now();
                };
                // Only paths are needed for discarded disk warmup. Source
                // snapshots load concurrently and seed the cache when joined.
                let canonical = crate::parallel::map_ordered(
                    &paths,
                    crate::parallel::workers(paths.len()),
                    |path| {
                        if canonical_sources.contains(path) {
                            Ok(path.clone())
                        } else {
                            path.canonicalize()
                        }
                    },
                )?;
                let sources: BTreeMap<_, _> = canonical
                    .into_iter()
                    .map(|path| path.map(|path| (path, String::new())))
                    .collect::<std::io::Result<_>>()?;
                ensure!(
                    sources.keys().all(|path| path.starts_with(&original)),
                    "warmup source outside original project"
                );
                let first = sources
                    .keys()
                    .next()
                    .context("empty warmup source catalog")?;
                trace("input_maps");
                let mut checker = ProjectChecker::new_with_memory_projection_roots(
                    &binary,
                    &original,
                    &out,
                    &config,
                    &sources,
                    &extra_roots,
                )?;
                trace("initialize");
                // Raw diagnostics never become companion evidence. A later
                // request checks every final projection after synchronization.
                checker.warm_original_workspace(first)?;
                trace("native_check");
                Ok(checker)
            })?;
        Ok(Self {
            thread: Some(thread),
        })
    }

    pub fn finish(mut self) -> Result<ProjectChecker> {
        self.thread
            .take()
            .unwrap()
            .join()
            .map_err(|_| anyhow!("native warmup thread panicked"))?
    }
}

impl Drop for NativeWarmup {
    fn drop(&mut self) {
        // SDK/source failures must not leave a native child reading a temporary
        // view after its owner has returned and cleanup has started.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn source_catalog_rejects_escaping_symlinks_before_native_initialization() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("ty-warmup-catalog-{}", std::process::id()));
        let root = directory.join("project");
        let out = directory.join("generated");
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(&out)?;
        let result = (|| -> Result<()> {
            let root = root.canonicalize()?;
            let outside = directory.join("outside.py");
            std::fs::write(&outside, "value: int = 1\n")?;
            let outside = outside.canonicalize()?;
            let alias = root.join("alias.py");
            std::os::unix::fs::symlink(&outside, &alias)?;
            let config = directory.join("checker.toml");
            std::fs::write(&config, "[environment]\npython-version='3.13'\n")?;
            for paths in [vec![alias], vec![outside], Vec::new()] {
                let warmup = NativeWarmup::start(
                    &directory.join("not-a-checker"),
                    &root,
                    &out,
                    &config,
                    paths.clone(),
                    Vec::new(),
                    Arc::default(),
                )?;
                let error = warmup
                    .finish()
                    .err()
                    .context("catalog unexpectedly accepted")?;
                assert!(error.to_string().contains(if paths.is_empty() {
                    "empty warmup source catalog"
                } else {
                    "outside original project"
                }));
                assert!(!out.join("checker-workspace.toml").exists());
                assert!(!out.join("checker-server.stderr.log").exists());
            }
            Ok(())
        })();
        std::fs::remove_dir_all(&directory)?;
        result
    }
}
