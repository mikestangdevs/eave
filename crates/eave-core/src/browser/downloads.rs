//! Finished downloads, journalled so the panel survives a restart.
//!
//! The live transfer (percent, cancel callback) stays in the app: those
//! numbers change many times a second and the CEF callback is process-local.
//! What a person is entitled to still see after quit is the **finished**
//! file — url, name, path, size. That is this log.
//!
//! Same `EventLog` as tabs/history. A clear rewrites rather than tombstones,
//! for history's reason: a forgotten download is one that is gone.

use serde::{Deserialize, Serialize};

use crate::journal::now_ms;

/// Most finished downloads kept. A desktop panel, not an archive.
pub const MAX_ITEMS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinishedDownload {
    pub url: String,
    pub file_name: String,
    pub path: String,
    pub bytes: i64,
    pub ts_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadEvent {
    Finished {
        url: String,
        file_name: String,
        path: String,
        bytes: i64,
        #[serde(default)]
        ts_ms: u64,
    },
    Cleared,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Downloads {
    items: Vec<FinishedDownload>,
}

impl Downloads {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    pub fn items(&self) -> &[FinishedDownload] {
        &self.items
    }

    pub fn apply(&mut self, event: &DownloadEvent) -> bool {
        match event {
            DownloadEvent::Cleared => {
                if self.items.is_empty() {
                    return false;
                }
                self.items.clear();
                true
            }
            DownloadEvent::Finished {
                url,
                file_name,
                path,
                bytes,
                ts_ms,
            } => {
                if path.is_empty() {
                    return false;
                }
                self.items.retain(|d| d.path != *path);
                self.items.push(FinishedDownload {
                    url: url.clone(),
                    file_name: file_name.clone(),
                    path: path.clone(),
                    bytes: *bytes,
                    ts_ms: if *ts_ms == 0 { now_ms() } else { *ts_ms },
                });
                if self.items.len() > MAX_ITEMS {
                    let drop = self.items.len() - MAX_ITEMS;
                    self.items.drain(..drop);
                }
                true
            }
        }
    }

    pub fn retained(&self) -> Vec<DownloadEvent> {
        self.items
            .iter()
            .map(|d| DownloadEvent::Finished {
                url: d.url.clone(),
                file_name: d.file_name.clone(),
                path: d.path.clone(),
                bytes: d.bytes,
                ts_ms: d.ts_ms,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fin(path: &str) -> DownloadEvent {
        DownloadEvent::Finished {
            url: "https://x.test/a".into(),
            file_name: "a.bin".into(),
            path: path.into(),
            bytes: 12,
            ts_ms: 1,
        }
    }

    #[test]
    fn a_finished_download_is_kept_and_a_clear_forgets_it() {
        let mut d = Downloads::new();
        assert!(d.apply(&fin("/tmp/a.bin")));
        assert_eq!(d.items().len(), 1);
        assert!(d.apply(&DownloadEvent::Cleared));
        assert!(d.items().is_empty());
        assert!(!d.apply(&DownloadEvent::Cleared));
    }

    #[test]
    fn the_same_path_is_replaced_not_duplicated() {
        let mut d = Downloads::new();
        d.apply(&fin("/tmp/a.bin"));
        d.apply(&DownloadEvent::Finished {
            url: "https://x.test/b".into(),
            file_name: "b.bin".into(),
            path: "/tmp/a.bin".into(),
            bytes: 99,
            ts_ms: 2,
        });
        assert_eq!(d.items().len(), 1);
        assert_eq!(d.items()[0].bytes, 99);
        assert_eq!(d.items()[0].file_name, "b.bin");
    }

    #[test]
    fn the_cap_drops_the_oldest() {
        let mut d = Downloads::new();
        for i in 0..(MAX_ITEMS + 3) {
            d.apply(&DownloadEvent::Finished {
                url: format!("https://x.test/{i}"),
                file_name: format!("{i}.bin"),
                path: format!("/tmp/{i}.bin"),
                bytes: i as i64,
                ts_ms: i as u64,
            });
        }
        assert_eq!(d.items().len(), MAX_ITEMS);
        assert_eq!(d.items()[0].file_name, "3.bin");
    }
}
