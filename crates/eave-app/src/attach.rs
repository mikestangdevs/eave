//! Composer attachments: local paths staged onto a send.
//!
//! # What an attachment is
//!
//! A path the operator dropped or picked. eave does not copy it into a
//! blob store — the session is a local CLI, and the path is the fact the
//! agent can `Read`. The chip names the basename; the send carries an
//! `@mention` (relative to the session cwd when the file is inside it,
//! absolute otherwise, directories with a trailing slash). That is waku's
//! mention form, which is also how T3 serializes a workspace file into
//! the composer.
//!
//! Images are classified by extension so the chip can thumbnail them.
//! Native image blocks (Claude/Codex/ACP) are a *second* encoding of the
//! same path: [`StagedFile::image_send_path`] is `Some` only for T3's
//! send-turn mime set (gif/jpeg/png/webp). Everything else stays a
//! mention. Pi has no image slot; it still receives the mention.
//!
//! # The cap
//!
//! [`MAX`] is T3 Code's `PROVIDER_SEND_TURN_MAX_ATTACHMENTS` (8) in
//! `packages/contracts/src/orchestration.ts`. Extra paths are refused
//! rather than truncated silently — `stage` reports how many were kept
//! and how many would not fit, so a caller can state the overflow.

use std::path::{Component, Path, PathBuf};

/// T3 Code's per-turn attachment cap.
pub const MAX: usize = 8;

/// One staged path, ready to chip and to mention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedFile {
    /// Absolute, `..`-collapsed. Symlinks are resolved when the path
    /// exists so `/tmp/foo` and a link to it are one chip.
    pub path: PathBuf,
    /// Basename drawn on the chip.
    pub name: String,
    pub is_dir: bool,
    /// Extension-based. Not a content sniff — that would open the file
    /// on the drop path for a fact the chip only uses to pick an icon.
    pub is_image: bool,
    /// What the send prepends as `@mention`.
    pub mention: String,
}

impl StagedFile {
    /// Path the engine should encode as a native image block. `None` for
    /// directories and for extensions T3 will not put on a provider turn
    /// (svg/bmp/heic/tiff stay mentions).
    pub fn image_send_path(&self) -> Option<&Path> {
        if self.is_dir {
            return None;
        }
        eave_core::media::send_media_type(&self.path).map(|_| self.path.as_path())
    }
}

/// How many of `incoming` were added, and how many the cap turned away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StageOutcome {
    pub added: usize,
    pub refused: usize,
}

/// Collapse `.` / `..` without requiring the path to exist.
fn lexical_absolute(path: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        out
    }
}

/// Resolve a dropped or picked path: canonicalize when the file is
/// there, otherwise the lexical absolute form so a missing file still
/// stages as a fact (the agent will be the one to fail the Read).
pub fn resolve_path(path: &Path) -> PathBuf {
    let abs = lexical_absolute(path);
    std::fs::canonicalize(&abs).unwrap_or(abs)
}

/// Image extensions the chip treats as pictures. Lowercased, no dot.
pub fn is_image_name(name: &str) -> bool {
    matches!(
        Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "heic" | "svg")
    )
}

/// The `@` body for a send. Relative to `cwd` when the file lives inside
/// it; absolute otherwise. Directories keep a trailing slash so a drop
/// of a folder and a drop of a file of the same name cannot collapse.
pub fn mention(path: &Path, cwd: Option<&Path>, is_dir: bool) -> String {
    let mut body = match cwd {
        Some(cwd) => path
            .strip_prefix(cwd)
            .map(|rel| rel.display().to_string())
            .unwrap_or_else(|_| path.display().to_string()),
        None => path.display().to_string(),
    };
    if is_dir && !body.ends_with('/') {
        body.push('/');
    }
    body
}

/// Build a staged file from a path. Always succeeds: a path that does
/// not exist yet is still a path the operator asked to send.
pub fn from_path(path: &Path, cwd: Option<&Path>) -> StagedFile {
    let path = resolve_path(path);
    let is_dir = path.is_dir();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let is_image = !is_dir && is_image_name(&name);
    let mention = mention(&path, cwd, is_dir);
    StagedFile {
        path,
        name,
        is_dir,
        is_image,
        mention,
    }
}

/// Append unique paths, stopping at [`MAX`]. Duplicates (same resolved
/// path) are skipped and do not count as refused — they were already
/// kept. `refused` is only the overflow past the cap.
pub fn stage(
    files: &mut Vec<StagedFile>,
    incoming: &[PathBuf],
    cwd: Option<&Path>,
) -> StageOutcome {
    let mut added = 0;
    let mut refused = 0;
    for path in incoming {
        let staged = from_path(path, cwd);
        if files.iter().any(|f| f.path == staged.path) {
            continue;
        }
        if files.len() >= MAX {
            refused += 1;
            continue;
        }
        files.push(staged);
        added += 1;
    }
    StageOutcome { added, refused }
}

/// The send body: the typed words, then each `@mention`. Empty text and
/// no files is not a send.
pub fn compose_send(text: &str, mentions: &[String]) -> Option<String> {
    let text = text.trim();
    let attached = mentions
        .iter()
        .map(|m| format!("@{m}"))
        .collect::<Vec<_>>()
        .join(" ");
    match (text.is_empty(), attached.is_empty()) {
        (true, true) => None,
        (false, true) => Some(text.to_string()),
        (true, false) => Some(attached),
        (false, false) => Some(format!("{text} {attached}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eave-attach-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn compose_send_is_none_when_there_is_nothing_to_say() {
        assert_eq!(compose_send("", &[]), None);
        assert_eq!(compose_send("   ", &[]), None);
    }

    #[test]
    fn compose_send_keeps_plain_text_alone() {
        assert_eq!(
            compose_send(" run tests ", &[]).as_deref(),
            Some("run tests")
        );
    }

    #[test]
    fn compose_send_files_only_is_the_mentions() {
        assert_eq!(
            compose_send("", &["src/a.rs".into(), "docs/".into()]).as_deref(),
            Some("@src/a.rs @docs/")
        );
    }

    #[test]
    fn compose_send_joins_text_and_mentions() {
        assert_eq!(
            compose_send("look", &["shot.png".into()]).as_deref(),
            Some("look @shot.png")
        );
    }

    #[test]
    fn a_file_inside_cwd_mentions_relative() {
        let cwd = Path::new("/Users/x/proj");
        let path = Path::new("/Users/x/proj/src/main.rs");
        assert_eq!(mention(path, Some(cwd), false), "src/main.rs");
    }

    #[test]
    fn a_file_outside_cwd_mentions_absolute() {
        let cwd = Path::new("/Users/x/proj");
        let path = Path::new("/tmp/shot.png");
        assert_eq!(mention(path, Some(cwd), false), "/tmp/shot.png");
    }

    #[test]
    fn a_directory_keeps_a_trailing_slash() {
        let path = Path::new("/tmp/docs");
        assert_eq!(mention(path, None, true), "/tmp/docs/");
        assert_eq!(
            mention(path, None, true).matches('/').count(),
            mention(&PathBuf::from("/tmp/docs/"), None, true)
                .matches('/')
                .count(),
            "a path that already had a slash must not grow a second"
        );
    }

    #[test]
    fn image_names_are_the_extension_not_the_basename() {
        assert!(is_image_name("Shot.PNG"));
        assert!(is_image_name("a.webp"));
        assert!(!is_image_name("a.rs"));
        assert!(!is_image_name("png"));
        assert!(!is_image_name("a.png.bak"));
    }

    #[test]
    fn parent_dots_collapse_so_aliases_dedup() {
        let a = resolve_path(Path::new("/tmp/foo/../bar"));
        let b = resolve_path(Path::new("/tmp/bar"));
        assert_eq!(a, b);
    }

    #[test]
    fn staging_the_same_path_twice_keeps_one() {
        let mut files = Vec::new();
        let p = PathBuf::from("/tmp/eave-attach-once.rs");
        let first = stage(&mut files, &[p.clone()], None);
        let second = stage(&mut files, &[p], None);
        assert_eq!(
            first,
            StageOutcome {
                added: 1,
                refused: 0
            }
        );
        assert_eq!(
            second,
            StageOutcome {
                added: 0,
                refused: 0
            }
        );
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn the_ninth_path_is_refused_not_silently_kept() {
        let mut files = Vec::new();
        let incoming: Vec<PathBuf> = (0..9)
            .map(|i| PathBuf::from(format!("/tmp/eave-attach-{i}.rs")))
            .collect();
        let out = stage(&mut files, &incoming, None);
        assert_eq!(out.added, MAX);
        assert_eq!(out.refused, 1);
        assert_eq!(files.len(), MAX);
    }

    #[test]
    fn a_real_directory_is_flagged_as_one() {
        let dir = tmp();
        let staged = from_path(&dir, None);
        assert!(staged.is_dir, "{staged:?}");
        assert!(staged.mention.ends_with('/'));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_still_stages() {
        let staged = from_path(Path::new("/no/such/eave-attach-missing.rs"), None);
        assert!(!staged.is_dir);
        assert_eq!(staged.name, "eave-attach-missing.rs");
    }

    #[test]
    fn only_t3_send_turn_images_travel_as_image_blocks() {
        let png = from_path(Path::new("/tmp/shot.PNG"), None);
        assert!(png.is_image);
        assert!(png.image_send_path().is_some());
        let svg = from_path(Path::new("/tmp/icon.svg"), None);
        assert!(svg.is_image, "the chip still treats svg as a picture");
        assert!(
            svg.image_send_path().is_none(),
            "svg is not a T3 send-turn mime"
        );
        let rs = from_path(Path::new("/tmp/main.rs"), None);
        assert!(!rs.is_image);
        assert!(rs.image_send_path().is_none());
    }
}
