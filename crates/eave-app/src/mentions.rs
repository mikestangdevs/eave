//! `@file` mentions in the composer — MonoCode's reference picker, in eave.
//!
//! Slash (`/`) is a command. `@` is a path. The two palettes never share a
//! moment: a `/` token at the start of the draft owns the keys, and an `@`
//! token at the caret owns them otherwise.
//!
//! Listing prefers `git ls-files` (so `.gitignore` is the ignore list). A
//! non-git cwd walks the tree and skips the usual vendor/build names.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Files the picker offers at once. MonoCode's `MAX_PICKER`.
pub const MAX_PICKER: usize = 30;
/// Query length past which the token is treated as ordinary text.
pub const MAX_QUERY: usize = 120;
/// Walk / git-ls cap. A 40k-file monorepo must not freeze the first `@`.
pub const MAX_INDEX: usize = 4000;

/// One project file the picker can insert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectFile {
    pub path: PathBuf,
    pub relative: String,
    pub name: String,
}

/// The `@…` token that contains `cursor`, if the operator is typing one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionToken {
    pub start: usize,
    pub end: usize,
    pub query: String,
}

/// Mention token containing `cursor`, or `None`.
///
/// The token starts at the `@` after whitespace (or the start of the
/// string) and runs to the next space. The query is the bytes between
/// `@` and the caret, so a caret in the middle of `@src/mai|n.rs` still
/// ranks `src/mai`.
pub fn mention_token_at(text: &str, cursor: usize) -> Option<MentionToken> {
    let i = cursor.min(text.len());
    if !text.is_char_boundary(i) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut start = i;
    while start > 0 && !bytes[start - 1].is_ascii_whitespace() {
        start -= 1;
    }
    if start >= text.len() || bytes[start] != b'@' {
        return None;
    }
    let mut end = start + 1;
    while end < text.len() && !bytes[end].is_ascii_whitespace() {
        end += 1;
    }
    let typed = &text[start + 1..i.min(end)];
    if typed.contains('@') || typed.len() > MAX_QUERY {
        return None;
    }
    Some(MentionToken {
        start,
        end,
        query: typed.to_string(),
    })
}

/// Replace `token` with `@label` and a trailing space if needed.
pub fn replace_mention_token(text: &str, token: &MentionToken, label: &str) -> String {
    let rest = &text[token.end.min(text.len())..];
    let spacer = if rest.starts_with(' ') { "" } else { " " };
    format!("{}@{}{}{}", &text[..token.start], label, spacer, rest)
}

/// Preferred label: basename when unique and space-free, otherwise the
/// relative path.
pub fn mention_label(file: &ProjectFile, files: &[ProjectFile]) -> String {
    if file.name.contains(char::is_whitespace) || file.relative.contains(char::is_whitespace) {
        return file.relative.clone();
    }
    let unique = files
        .iter()
        .filter(|f| f.name == file.name)
        .count()
        == 1;
    if unique && !file.name.contains(char::is_whitespace) {
        file.name.clone()
    } else {
        file.relative.clone()
    }
}

/// Rank files for the picker. Recents first with an empty query; fuzzy
/// after. Paths with spaces are skipped (they cannot round-trip as a
/// single `@token`).
pub fn rank_mention_files<'a>(
    files: &'a [ProjectFile],
    query: &str,
    recents: &[String],
    limit: usize,
) -> Vec<&'a ProjectFile> {
    let usable: Vec<&ProjectFile> = files
        .iter()
        .filter(|f| !f.relative.contains(char::is_whitespace))
        .collect();
    let q = query.trim();
    if !q.is_empty() {
        let mut scored: Vec<(i32, &ProjectFile)> = usable
            .into_iter()
            .filter_map(|f| score_path(q, &f.relative, &f.name).map(|s| (s, f)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.relative.cmp(&b.1.relative)));
        return scored.into_iter().take(limit).map(|(_, f)| f).collect();
    }

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for path in recents {
        if let Some(file) = usable.iter().find(|f| f.path.to_string_lossy() == *path) {
            if seen.insert(file.path.as_path()) {
                out.push(*file);
                if out.len() >= limit {
                    return out;
                }
            }
        }
    }
    let mut rest = usable;
    rest.sort_by(|a, b| {
        path_depth(&a.relative)
            .cmp(&path_depth(&b.relative))
            .then_with(|| a.relative.cmp(&b.relative))
    });
    for file in rest {
        if seen.insert(file.path.as_path()) {
            out.push(file);
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

fn path_depth(relative: &str) -> usize {
    relative.bytes().filter(|b| *b == b'/').count()
}

/// Sequential fuzzy: all query characters in order, with bonuses for
/// consecutive runs, path separators, and the filename.
fn score_path(query: &str, relative: &str, name: &str) -> Option<i32> {
    if let Some(s) = fuzzy(query, name) {
        return Some(s + 400);
    }
    fuzzy(query, relative)
}

fn fuzzy(query: &str, text: &str) -> Option<i32> {
    let needle = query.to_ascii_lowercase();
    let hay = text.to_ascii_lowercase();
    if needle.is_empty() {
        return Some(0);
    }
    let n = needle.as_bytes();
    let h = hay.as_bytes();
    let mut qi = 0;
    let mut score: i32 = 0;
    let mut consecutive = 0;
    for (i, &ch) in h.iter().enumerate() {
        if qi >= n.len() {
            break;
        }
        if ch != n[qi] {
            consecutive = 0;
            continue;
        }
        consecutive += 1;
        score += 1 + consecutive * 4;
        if i == 0 || is_break(text.as_bytes().get(i.saturating_sub(1)).copied()) {
            score += 14;
        } else if text.as_bytes()[i].is_ascii_uppercase()
            && text
                .as_bytes()
                .get(i.saturating_sub(1))
                .is_some_and(|c| !c.is_ascii_uppercase())
        {
            score += 10;
        }
        qi += 1;
    }
    if qi != n.len() {
        return None;
    }
    Some(score - (h.len() as i32 - n.len() as i32))
}

fn is_break(ch: Option<u8>) -> bool {
    matches!(ch, Some(b'/' | b'\\' | b'-' | b'_' | b'.' | b' ') | None)
}

/// Index the project at `cwd`. Git first; walk if git is missing or the
/// directory is not a repo.
pub fn list_project_files(cwd: &Path) -> Vec<ProjectFile> {
    if !cwd.is_dir() {
        return Vec::new();
    }
    // Only ask git when THIS directory is a repo (or a worktree). `git
    // ls-files` otherwise walks parent repos and can hang a picker on a
    // temp fixture that lives under a giant checkout.
    if cwd.join(".git").exists() {
        let from_git = git_ls_files(cwd);
        if !from_git.is_empty() {
            return from_git;
        }
    }
    walk_files(cwd)
}

fn git_ls_files(cwd: &Path) -> Vec<ProjectFile> {
    let out = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard", "-z"])
        .current_dir(cwd)
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let mut files = Vec::new();
    for rel in out.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        let Ok(relative) = std::str::from_utf8(rel) else {
            continue;
        };
        if relative.ends_with('/') {
            continue;
        }
        files.push(project_file(cwd, relative));
        if files.len() >= MAX_INDEX {
            break;
        }
    }
    files
}

fn walk_files(cwd: &Path) -> Vec<ProjectFile> {
    let mut files = Vec::new();
    let mut stack = vec![cwd.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if skip_name(&name) {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let relative = path
                .strip_prefix(cwd)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| path.display().to_string());
            files.push(ProjectFile {
                path: path.clone(),
                name: name.into_owned(),
                relative,
            });
            if files.len() >= MAX_INDEX {
                return files;
            }
        }
    }
    files
}

fn project_file(cwd: &Path, relative: &str) -> ProjectFile {
    let path = cwd.join(relative);
    let name = Path::new(relative)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| relative.to_string());
    ProjectFile {
        path,
        relative: relative.replace('\\', "/"),
        name,
    }
}

fn skip_name(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "target.nosync"
            | "dist"
            | ".next"
            | "build"
            | "__pycache__"
            | ".venv"
            | "venv"
            | ".turbo"
            | ".cache"
            | ".DS_Store"
            | "chromium.nosync"
    ) || (name.starts_with('.') && name != ".github" && name != ".claude")
}

/// Current git branch of `cwd`, or `None` when git is missing / not a repo.
pub fn git_branch(cwd: &Path) -> Option<String> {
    if !cwd.join(".git").exists() {
        return None;
    }
    let out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8(out.stdout).ok()?;
    let name = name.trim();
    if name.is_empty() || name == "HEAD" {
        None
    } else {
        Some(name.to_string())
    }
}

/// Display label for a project cwd: last path component, `~` collapsed.
pub fn project_label(cwd: &str, home: &str) -> String {
    let path = Path::new(cwd);
    if let Some(name) = path.file_name() {
        let name = name.to_string_lossy();
        if !name.is_empty() {
            return name.into_owned();
        }
    }
    if !home.is_empty() && cwd.starts_with(home) {
        return format!("~{}", &cwd[home.len()..]);
    }
    cwd.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn file(rel: &str) -> ProjectFile {
        ProjectFile {
            path: PathBuf::from("/proj").join(rel),
            relative: rel.to_string(),
            name: Path::new(rel)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        }
    }

    #[test]
    fn an_at_token_opens_at_the_caret_and_closes_on_space() {
        assert_eq!(mention_token_at("hello", 5), None);
        let t = mention_token_at("@src", 4).expect("token");
        assert_eq!(t.start, 0);
        assert_eq!(t.query, "src");
        let mid = mention_token_at("see @main.rs please", 12).expect("token");
        assert_eq!(mid.query, "main.rs");
        assert_eq!(mention_token_at("see @main.rs please", 13), None);
        assert_eq!(mention_token_at("/ship", 5), None);
    }

    #[test]
    fn replacing_a_token_inserts_the_label_and_a_space() {
        let token = mention_token_at("@mai", 4).unwrap();
        assert_eq!(
            replace_mention_token("@mai", &token, "main.rs"),
            "@main.rs "
        );
        let token = mention_token_at("x @mai y", 6).unwrap();
        assert_eq!(
            replace_mention_token("x @mai y", &token, "src/main.rs"),
            "x @src/main.rs y"
        );
    }

    #[test]
    fn unique_basenames_are_the_short_label() {
        let files = [file("src/main.rs"), file("README.md")];
        assert_eq!(mention_label(&files[0], &files), "main.rs");
        let clash = [file("src/main.rs"), file("pkg/main.rs")];
        assert_eq!(mention_label(&clash[0], &clash), "src/main.rs");
    }

    #[test]
    fn ranking_prefers_the_filename_then_the_path() {
        let files = [
            file("src/main.rs"),
            file("src/lib.rs"),
            file("docs/main.md"),
        ];
        let hits = rank_mention_files(&files, "main", &[], 10);
        assert!(
            hits[0].name.starts_with("main"),
            "filename hits should lead: {:?}",
            hits.iter().map(|f| &f.relative).collect::<Vec<_>>()
        );
        assert!(hits.iter().any(|f| f.relative == "docs/main.md"));
        assert!(!hits.iter().any(|f| f.relative == "src/lib.rs"));
    }

    #[test]
    fn walking_a_tree_skips_vendor_dirs_and_finds_a_real_file() {
        let root = std::env::temp_dir().join(format!(
            "eave-mentions-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), "nope").unwrap();
        let files = walk_files(&root);
        let _ = fs::remove_dir_all(&root);
        assert!(
            files.iter().any(|f| f.relative == "src/main.rs"),
            "walk missed the real file: {files:?}"
        );
        assert!(
            !files.iter().any(|f| f.relative.contains("node_modules")),
            "walk leaked vendor files: {files:?}"
        );
    }

    #[test]
    fn project_label_is_the_last_component() {
        assert_eq!(project_label("/Users/me/code/eave", "/Users/me"), "eave");
        assert_eq!(project_label("/", ""), "/");
    }
}
