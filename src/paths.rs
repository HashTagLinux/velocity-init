use std::path::{Path, PathBuf};

use nix::unistd::{access, AccessFlags};

pub const DEFAULT_SOCKET_PATH: &str = "/run/velocity-init.sock";

pub const DEFAULT_JOURNAL_PATH: &str = "/run/velocity-init/journal";

const FALLBACK_SOCKET_NAME: &str = "velocity-init.sock";

const FALLBACK_JOURNAL_NAME: &str = "journal";

pub const RUNTIME_DIR_ENV: &str = "VELOCITY_RUNTIME_DIR";

fn is_writable_dir(path: &Path) -> bool {
    path.is_dir() && access(path, AccessFlags::W_OK).is_ok()
}

fn try_create_dir(path: &Path) -> bool {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    if std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .is_err()
        && !path.is_dir()
    {
        return false;
    }

    if let Ok(meta) = std::fs::metadata(path) {
        if meta.uid() == nix::unistd::getuid().as_raw() && (meta.mode() & 0o077) != 0 {
            let _ = std::fs::set_permissions(
                path,
                std::fs::Permissions::from_mode(meta.mode() & !0o077),
            );
        }
    }
    is_writable_dir(path)
}

fn first_usable(candidates: &[PathBuf], usable: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    candidates.iter().find(|p| usable(p)).cloned()
}

fn fallback_dir_with(uid: u32, xdg_runtime_dir: Option<String>) -> PathBuf {
    match xdg_runtime_dir {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("velocity-init"),
        _ => PathBuf::from(format!("/tmp/velocity-init-{uid}")),
    }
}

pub fn runtime_dir_candidates() -> Vec<PathBuf> {
    runtime_dir_candidates_with(|key| std::env::var(key).ok())
}

pub fn runtime_dir_candidates_with(
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = lookup(RUNTIME_DIR_ENV) {
        if !dir.is_empty() {
            dirs.push(PathBuf::from(dir));
            return dirs;
        }
    }
    dirs.push(PathBuf::from("/run/velocity-init"));

    dirs.push(fallback_dir_with(
        nix::unistd::getuid().as_raw(),
        lookup("XDG_RUNTIME_DIR"),
    ));
    dirs
}

pub fn runtime_dir() -> PathBuf {
    runtime_dir_for(runtime_dir_candidates())
}

pub fn runtime_dir_for(mut candidates: Vec<PathBuf>) -> PathBuf {
    if let Some(dir) = first_usable(&candidates, is_writable_dir) {
        return dir;
    }
    if let Some(dir) = first_usable(&candidates, try_create_dir) {
        return dir;
    }
    candidates.pop().unwrap_or_default()
}

pub fn control_socket_candidates() -> Vec<PathBuf> {
    control_socket_candidates_with(|key| std::env::var(key).ok(), &runtime_dir())
}

pub fn control_socket_candidates_with(
    lookup: impl Fn(&str) -> Option<String>,
    runtime_dir: &Path,
) -> Vec<PathBuf> {
    match lookup("VELOCITY_INIT_SOCKET") {
        Some(path) if !path.is_empty() => vec![PathBuf::from(path)],
        _ => {
            if let Some(dir) = lookup(RUNTIME_DIR_ENV) {
                if !dir.is_empty() {
                    return vec![PathBuf::from(dir).join(FALLBACK_SOCKET_NAME)];
                }
            }
            let mut paths = vec![PathBuf::from(DEFAULT_SOCKET_PATH)];
            let fallback = runtime_dir.join(FALLBACK_SOCKET_NAME);
            if fallback != paths[0] {
                paths.push(fallback);
            }
            paths
        }
    }
}

pub fn journal_candidates() -> Vec<PathBuf> {
    journal_candidates_with(|key| std::env::var(key).ok(), &runtime_dir())
}

pub fn journal_candidates_with(
    lookup: impl Fn(&str) -> Option<String>,
    runtime_dir: &Path,
) -> Vec<PathBuf> {
    match lookup("VELOCITY_JOURNAL") {
        Some(path) if !path.is_empty() => vec![PathBuf::from(path)],
        _ => {
            if let Some(dir) = lookup(RUNTIME_DIR_ENV) {
                if !dir.is_empty() {
                    return vec![PathBuf::from(dir).join(FALLBACK_JOURNAL_NAME)];
                }
            }
            let mut paths = vec![PathBuf::from(DEFAULT_JOURNAL_PATH)];
            let fallback = runtime_dir.join(FALLBACK_JOURNAL_NAME);
            if fallback != paths[0] {
                paths.push(fallback);
            }
            paths
        }
    }
}

pub fn is_pinned(candidates: &[PathBuf]) -> bool {
    candidates.len() == 1 && candidates[0] != PathBuf::from(DEFAULT_SOCKET_PATH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup_of(map: &HashMap<String, String>) -> impl Fn(&str) -> Option<String> + '_ {
        |key: &str| map.get(key).cloned()
    }

    fn with_vars(vars: &[(&str, &str)]) -> HashMap<String, String> {
        vars.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn scratch_rt(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "velocity-paths-rt-{}-{}",
            std::process::id(),
            name
        ))
    }

    #[test]
    fn test_env_override_yields_exactly_one_socket_candidate() {
        let map = with_vars(&[("VELOCITY_INIT_SOCKET", "/tmp/pinned.sock")]);
        let cands =
            control_socket_candidates_with(lookup_of(&map), Path::new("/tmp/rt"));
        assert_eq!(cands, vec![PathBuf::from("/tmp/pinned.sock")]);
        assert!(is_pinned(&cands));
    }

    #[test]
    fn test_empty_env_override_is_ignored() {
        let map = with_vars(&[("VELOCITY_INIT_SOCKET", "")]);
        let cands =
            control_socket_candidates_with(lookup_of(&map), Path::new("/tmp/rt"));
        assert_eq!(cands[0], PathBuf::from(DEFAULT_SOCKET_PATH));
        assert!(!is_pinned(&cands));
    }

    #[test]
    fn test_runtime_dir_env_pins_the_whole_runtime_dir() {
        let map = with_vars(&[(RUNTIME_DIR_ENV, "/tmp/custom-rt")]);
        let dirs = runtime_dir_candidates_with(lookup_of(&map));
        assert_eq!(dirs, vec![PathBuf::from("/tmp/custom-rt")]);
    }

    #[test]
    fn test_run_candidates_are_tried_before_the_per_user_fallback() {
        let map = HashMap::new();
        let dirs = runtime_dir_candidates_with(lookup_of(&map));
        assert_eq!(dirs[0], PathBuf::from("/run/velocity-init"));
        assert_eq!(
            dirs[1],
            PathBuf::from(format!("/tmp/velocity-init-{}", nix::unistd::getuid()))
        );
        assert_eq!(dirs.len(), 2, "no further fallbacks: {dirs:?}");
    }

    #[test]
    fn test_xdg_runtime_dir_selects_the_fallback() {
        assert_eq!(
            fallback_dir_with(1234, Some("/run/user/1234".to_string())),
            PathBuf::from("/run/user/1234/velocity-init")
        );
        assert_eq!(
            fallback_dir_with(1234, None),
            PathBuf::from("/tmp/velocity-init-1234")
        );
        assert_eq!(
            fallback_dir_with(1234, Some(String::new())),
            PathBuf::from("/tmp/velocity-init-1234")
        );
    }

    #[test]
    fn test_first_usable_picks_the_first_match() {
        let cands = vec![
            PathBuf::from("/nope/a"),
            PathBuf::from("/nope/b"),
            PathBuf::from("/nope/c"),
        ];
        let chosen = first_usable(&cands, |p| p.ends_with("b"));
        assert_eq!(chosen, Some(PathBuf::from("/nope/b")));
        assert_eq!(first_usable(&cands, |_| false), None);
    }

    #[test]
    fn test_fallback_socket_never_duplicates_the_canonical_path() {
        let map = with_vars(&[(RUNTIME_DIR_ENV, "/run")]);
        let cands =
            control_socket_candidates_with(lookup_of(&map), Path::new("/run"));

        assert_eq!(cands, vec![PathBuf::from(DEFAULT_SOCKET_PATH)]);
    }

    #[test]
    fn test_journal_candidates_start_with_the_canonical_path() {
        let map = HashMap::new();
        let rt = scratch_rt("journal");
        let cands = journal_candidates_with(lookup_of(&map), &rt);
        assert_eq!(cands[0], PathBuf::from(DEFAULT_JOURNAL_PATH));
        assert_eq!(cands[1], rt.join("journal"));
        assert_ne!(cands[1], cands[0], "fallback must not duplicate the canonical path");
    }

    fn impossible(name: &str) -> PathBuf {
        PathBuf::from(format!("/proc/velocity-paths-nope-{}-{name}", std::process::id()))
    }

    #[test]
    fn test_runtime_dir_for_returns_a_writable_candidate() {
        let dir = scratch_rt("writable");
        let _ = std::fs::remove_dir_all(&dir);
        let got = runtime_dir_for(vec![impossible("a"), dir.clone()]);
        assert_eq!(got, dir);
        assert!(is_writable_dir(&got));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_runtime_dir_for_falls_back_to_the_last_candidate() {
        let cands = vec![impossible("a"), impossible("b")];
        assert_eq!(runtime_dir_for(cands.clone()), PathBuf::from(impossible("b")));
    }
}
