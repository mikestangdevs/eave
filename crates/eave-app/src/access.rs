//! Composer access (runtime) modes — MonoCode's AccessPicker, mapped onto
//! eave's permission questions.
//!
//! eave still *spawns* Claude with `--permission-prompt-tool stdio` and
//! no `--permission-mode` (Slice K: a CLI that does not ask is refused).
//! The picker does not change that spawn. It is the operator's policy for
//! what eave does with a question that already arrived: supervised leaves
//! the card on screen; the other three send `Decide { allow: true }` for
//! the tools they cover.
//!
//! Default is supervised. Choosing another mode is an explicit click.

/// MonoCode's four runtime modes, in picker order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuntimeMode {
    #[default]
    Supervised,
    AutoAcceptEdits,
    Auto,
    FullAccess,
}

pub const MODES: [RuntimeMode; 4] = [
    RuntimeMode::Supervised,
    RuntimeMode::AutoAcceptEdits,
    RuntimeMode::Auto,
    RuntimeMode::FullAccess,
];

impl RuntimeMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Supervised => "Supervised",
            Self::AutoAcceptEdits => "Auto-accept edits",
            Self::Auto => "Auto",
            Self::FullAccess => "Full access",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Supervised => "Ask before commands and file changes.",
            Self::AutoAcceptEdits => "Auto-approve edits, ask before other actions.",
            Self::Auto => "Approve routine reads and edits; risky ones still ask.",
            Self::FullAccess => "Allow commands and edits without prompts.",
        }
    }

    /// Claude's `init.permissionMode` → the matching picker row.
    ///
    /// Unknown strings stay supervised: a mode eave has not named must
    /// not quietly become full access.
    pub fn from_permission_mode(mode: Option<&str>) -> Self {
        match mode {
            Some("acceptEdits") => Self::AutoAcceptEdits,
            Some("auto") => Self::Auto,
            Some("bypassPermissions" | "dontAsk") => Self::FullAccess,
            _ => Self::Supervised,
        }
    }

    /// What Claude's `--permission-mode` would be, if eave ever sent one.
    /// Supervised is `None` because that is "pass no flag".
    pub fn to_permission_mode(self) -> Option<&'static str> {
        match self {
            Self::Supervised => None,
            Self::AutoAcceptEdits => Some("acceptEdits"),
            Self::Auto => Some("auto"),
            Self::FullAccess => Some("bypassPermissions"),
        }
    }

    /// Whether this mode auto-answers `tool` as allow.
    ///
    /// `tool` is the provider's tool name on the approval row (Edit,
    /// Write, Bash, …). Classification is by name, not by parsing the
    /// input — the input is what the card shows the operator.
    pub fn auto_allows(self, tool: &str) -> bool {
        match self {
            Self::Supervised => false,
            Self::FullAccess => true,
            Self::AutoAcceptEdits => is_edit_tool(tool),
            Self::Auto => is_edit_tool(tool) || is_read_tool(tool),
        }
    }
}

fn is_edit_tool(tool: &str) -> bool {
    matches!(
        tool,
        "Edit"
            | "Write"
            | "NotebookEdit"
            | "StrReplace"
            | "ApplyPatch"
            | "create_file"
            | "edit_file"
            | "search_replace"
    ) || tool.eq_ignore_ascii_case("write")
        || tool.eq_ignore_ascii_case("edit")
}

fn is_read_tool(tool: &str) -> bool {
    matches!(
        tool,
        "Read" | "Glob" | "Grep" | "LS" | "WebFetch" | "WebSearch" | "TodoRead"
    ) || tool.eq_ignore_ascii_case("read")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervised_asks_and_full_access_does_not() {
        assert!(!RuntimeMode::Supervised.auto_allows("Bash"));
        assert!(!RuntimeMode::Supervised.auto_allows("Edit"));
        assert!(RuntimeMode::FullAccess.auto_allows("Bash"));
        assert!(RuntimeMode::FullAccess.auto_allows("Edit"));
    }

    #[test]
    fn auto_accept_edits_lets_writes_through_and_still_asks_for_bash() {
        assert!(RuntimeMode::AutoAcceptEdits.auto_allows("Edit"));
        assert!(RuntimeMode::AutoAcceptEdits.auto_allows("Write"));
        assert!(!RuntimeMode::AutoAcceptEdits.auto_allows("Bash"));
        assert!(!RuntimeMode::AutoAcceptEdits.auto_allows("Read"));
    }

    #[test]
    fn auto_covers_reads_and_edits_but_not_shell() {
        assert!(RuntimeMode::Auto.auto_allows("Read"));
        assert!(RuntimeMode::Auto.auto_allows("Edit"));
        assert!(!RuntimeMode::Auto.auto_allows("Bash"));
    }

    #[test]
    fn unknown_permission_modes_stay_supervised() {
        assert_eq!(
            RuntimeMode::from_permission_mode(Some("mystery")),
            RuntimeMode::Supervised
        );
        assert_eq!(
            RuntimeMode::from_permission_mode(Some("default")),
            RuntimeMode::Supervised
        );
        assert_eq!(
            RuntimeMode::from_permission_mode(Some("bypassPermissions")),
            RuntimeMode::FullAccess
        );
        assert_eq!(RuntimeMode::Supervised.to_permission_mode(), None);
        assert_eq!(
            RuntimeMode::FullAccess.to_permission_mode(),
            Some("bypassPermissions")
        );
    }

    #[test]
    fn picker_order_is_the_four_monocode_rows() {
        assert_eq!(
            MODES.map(RuntimeMode::label),
            [
                "Supervised",
                "Auto-accept edits",
                "Auto",
                "Full access"
            ]
        );
    }
}
