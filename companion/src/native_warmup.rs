//! Overlap a disposable native check with immutable source and plugin indexing.
//! Its diagnostics are discarded. The ordinary synchronized check still runs.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    thread::JoinHandle,
};

use anyhow::{Result, anyhow};

use crate::{checker::ProjectChecker, project::SourceSnapshot};

pub struct NativeWarmup {
    thread: Option<JoinHandle<Result<(ProjectChecker, usize)>>>,
}

impl NativeWarmup {
    pub fn start(
        binary: &Path,
        original: &Path,
        out: &Path,
        config: &Path,
        snapshots: &BTreeMap<PathBuf, SourceSnapshot>,
    ) -> Result<Self> {
        let sources: BTreeMap<_, _> = snapshots
            .values()
            .map(|source| (source.path.clone(), source.text.clone()))
            .collect();
        let revisions: BTreeMap<_, _> = snapshots
            .values()
            .map(|source| (source.path.clone(), source.revision))
            .collect();
        let binary = binary.to_owned();
        let original = original.to_owned();
        let out = out.to_owned();
        let config = config.to_owned();
        let thread = std::thread::Builder::new()
            .name("native-check-warmup".into())
            .spawn(move || {
                let mut checker = ProjectChecker::new_with_memory_projection(
                    &binary, &original, &out, &config, &sources,
                )?;
                let seeds = checker.seed_original_sources(
                    sources
                        .iter()
                        .map(|(path, text)| (path.as_path(), text.as_str(), revisions[path])),
                );
                let paths = sources.keys().cloned().collect::<Vec<_>>();
                // Raw diagnostics never become companion evidence. A later
                // request checks every final projection after synchronization.
                let _ = checker.check_workspace(&sources, &paths)?;
                Ok((checker, seeds))
            })?;
        Ok(Self {
            thread: Some(thread),
        })
    }

    pub fn finish(mut self) -> Result<(ProjectChecker, usize)> {
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
