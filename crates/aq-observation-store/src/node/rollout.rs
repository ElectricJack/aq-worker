//! Bounded discovery of the earliest Codex rollout in a sessions tree.
use super::*;

fn rollout_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    Uuid::parse_str(id).ok().map(|_| id.to_owned())
}
struct RolloutSearch {
    remaining: usize,
    first: Option<PathBuf>,
}
impl RolloutSearch {
    fn scan_directory(
        &mut self,
        directory: &Path,
        depth: usize,
        next: &mut Vec<PathBuf>,
    ) -> Result<()> {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            self.remaining = self
                .remaining
                .checked_sub(1)
                .context("Codex sessions directory exceeds the discovery bound")?;
            let kind = entry.file_type()?; // Never follows symlinks.
            let path = entry.path();
            if depth < 3 && kind.is_dir() {
                next.push(path);
            } else if depth == 3 && kind.is_file() {
                self.consider(path);
            }
        }
        Ok(())
    }
    fn consider(&mut self, path: PathBuf) {
        if rollout_id(&path).is_some()
            && self.first.as_ref().is_none_or(|earliest| path < *earliest)
        {
            self.first = Some(path);
        }
    }
}
/// Find the earliest real rollout in the bounded YYYY/MM/DD directory tree.
pub(super) fn codex_rollout(sessions: &Path, entries: usize) -> Result<Option<(PathBuf, String)>> {
    let mut search = RolloutSearch {
        remaining: entries,
        first: None,
    };
    let mut level = vec![sessions.to_path_buf()];
    for depth in 0..4 {
        let mut next = vec![];
        for directory in level {
            search.scan_directory(&directory, depth, &mut next)?;
        }
        level = next;
    }
    Ok(search
        .first
        .and_then(|path| Some((path.parent()?.to_path_buf(), rollout_id(&path)?))))
}
