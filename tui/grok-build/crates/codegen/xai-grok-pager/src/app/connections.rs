//! Per-connection bookkeeping for multi-profile mode.
//!
//! Historically the TUI held exactly one ACP connection: `AppView.acp_tx` /
//! `AppView.models` were "the" connection and `profile::active()` answered for
//! the whole process. `E`/`/pane` runs two kernels side by side, so the facts
//! that belong to a connection move here and the process-wide helpers stay as
//! the primary connection's view.
//!
//! A profile is the full combination **backend + model + workspace**: two panes
//! may share a kernel binary and a model catalog but never a workspace or a
//! session database. `[`ProfileSpec`]` carries exactly those three, plus the
//! label that names its private storage roots.

use std::path::PathBuf;

use xai_acp_lib::AcpAgentTx;

use super::agent::AgentId;
use super::agent_view::AgentView;
use crate::acp::ModelState;

/// Index of a connection in [`super::app_view::AppView::connections`].
///
/// Slot 0 is the connection the process started with, so every pre-E2 code path
/// maps to it unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct ConnectionId(pub usize);

impl ConnectionId {
    pub const PRIMARY: ConnectionId = ConnectionId(0);

    pub fn index(self) -> usize {
        self.0
    }
}

/// The "backend + model + workspace" combination one pane runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSpec {
    /// Directory-safe label naming this profile's private storage.
    pub label: String,
    /// Working directory the kernel and its sessions run in.
    pub workspace: PathBuf,
    /// Model this profile starts with (`provider/model`), when pinned.
    pub model: Option<String>,
}

impl ProfileSpec {
    pub fn new(label: impl Into<String>, workspace: PathBuf) -> Self {
        Self {
            label: label.into(),
            workspace,
            model: None,
        }
    }

    /// Two profiles are distinct when any part of the combination differs.
    pub fn same_as(&self, other: &ProfileSpec) -> bool {
        self.label == other.label
            && self.workspace == other.workspace
            && self.model == other.model
    }

    /// Short label for the pane header and `/backend`.
    pub fn display_label(&self) -> String {
        if self.label.is_empty() {
            "primary".to_string()
        } else {
            self.label.clone()
        }
    }
}

/// One live ACP connection and the state that belongs to it.
pub struct ConnectionHandle {
    pub id: ConnectionId,
    /// Send half of this connection. Agents cloned from it route their requests
    /// to this kernel only.
    pub acp_tx: AcpAgentTx,
    /// This connection's model catalog and selection.
    pub models: ModelState,
    /// Backend profile detected from this connection's `initialize`.
    pub profile: openbuild_backend::BackendProfile,
    /// The profile this connection was launched for.
    pub spec: ProfileSpec,
    /// Whether the kernel is still reachable (cleared by the death path).
    pub alive: bool,
    /// Cancellation token for this connection's kernel process.
    ///
    /// Dropping a handle is not enough to stop a child process: the bridge
    /// thread owns the pipe and only stops on this token.
    pub cancel: Option<tokio_util::sync::CancellationToken>,
}

impl ConnectionHandle {
    pub fn new(
        id: ConnectionId,
        acp_tx: AcpAgentTx,
        models: ModelState,
        profile: openbuild_backend::BackendProfile,
        spec: ProfileSpec,
    ) -> Self {
        Self {
            id,
            acp_tx,
            models,
            profile,
            spec,
            alive: true,
            cancel: None,
        }
    }

    /// Whether this connection is the OpenBuild kernel.
    pub fn is_opencode(&self) -> bool {
        self.profile.is_opencode()
    }
}

/// Which pane has the keyboard.
///
/// Panes are the visible columns; the focused pane owns typing, permission
/// answers and cancellation, so two turns can run at once without a shared
/// "pending" state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneLayout {
    /// Agents shown side by side, left to right.
    pub panes: Vec<AgentId>,
    /// Index into `panes` that receives input.
    pub focused: usize,
    /// Right pane's percentage of the width (0 means an even split).
    pub split_percent: u16,
}

impl Default for PaneLayout {
    fn default() -> Self {
        Self {
            panes: Vec::new(),
            focused: 0,
            split_percent: 0,
        }
    }
}

impl PaneLayout {
    /// Single-pane mode is the historical layout.
    pub fn is_single(&self) -> bool {
        self.panes.len() <= 1
    }

    /// Whether two panes are visible.
    pub fn is_split(&self) -> bool {
        self.panes.len() >= 2
    }

    /// The focused agent, if any.
    pub fn focused_agent(&self) -> Option<AgentId> {
        self.panes.get(self.focused).copied()
    }

    /// Focus one of the visible panes by id.
    pub fn focus(&mut self, id: AgentId) -> bool {
        match self.panes.iter().position(|pane| *pane == id) {
            Some(index) if index != self.focused => {
                self.focused = index;
                true
            }
            _ => false,
        }
    }

    /// Cycle focus between visible panes, wrapping around.
    pub fn cycle_focus(&mut self) -> bool {
        if self.panes.len() < 2 {
            return false;
        }
        self.focused = (self.focused + 1) % self.panes.len();
        true
    }

    /// Add a pane to the right. At most two panes are shown (the split is a
    /// strict side-by-side comparison, not a tiling window manager).
    pub fn open(&mut self, id: AgentId) -> bool {
        if self.panes.contains(&id) || self.panes.len() >= 2 {
            return false;
        }
        self.panes.push(id);
        if self.panes.len() == 2 {
            self.focused = 1;
        }
        true
    }

    /// Remove a pane; focus falls back to the remaining one.
    pub fn close(&mut self, id: AgentId) -> bool {
        let Some(index) = self.panes.iter().position(|pane| *pane == id) else {
            return false;
        };
        self.panes.remove(index);
        if self.focused >= self.panes.len() {
            self.focused = self.panes.len().saturating_sub(1);
        }
        true
    }

    /// Drop panes that no longer exist (session closed) and repair focus.
    pub fn retain_existing(&mut self, agents: &indexmap::IndexMap<AgentId, AgentView>) -> bool {
        let before = (self.panes.clone(), self.focused);
        self.panes.retain(|id| agents.contains_key(id));
        if self.focused >= self.panes.len() {
            self.focused = self.panes.len().saturating_sub(1);
        }
        before != (self.panes.clone(), self.focused)
    }

    /// Width split for two panes, as `(left, right)` column counts.
    ///
    /// The right pane gets `split_percent` percent when set, otherwise half.
    /// Both sides keep at least one column so a narrow terminal still draws two
    /// usable panes instead of a zero-width one.
    pub fn split_widths(width: u16) -> (u16, u16) {
        Self::split_widths_with(width, 0)
    }

    pub fn split_widths_with(width: u16, split_percent: u16) -> (u16, u16) {
        if width < 2 {
            return (width, 0);
        }
        let right = if (1..=99).contains(&split_percent) {
            ((u32::from(width) * u32::from(split_percent) + 50) / 100) as u16
        } else {
            width / 2
        };
        let right = right.clamp(1, width - 1);
        (width - right, right)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: usize) -> AgentId {
        AgentId(id)
    }

    #[test]
    fn default_layout_is_single_pane() {
        let layout = PaneLayout::default();
        assert!(layout.is_single());
        assert!(!layout.is_split());
        assert_eq!(layout.focused_agent(), None);
    }

    #[test]
    fn opening_a_second_pane_focuses_it() {
        let mut layout = PaneLayout::default();
        assert!(layout.open(agent(1)));
        assert!(layout.is_single());
        assert!(layout.open(agent(2)));
        assert!(layout.is_split());
        assert_eq!(layout.focused_agent(), Some(agent(2)));
    }

    #[test]
    fn a_third_pane_is_refused() {
        let mut layout = PaneLayout::default();
        layout.open(agent(1));
        layout.open(agent(2));
        assert!(!layout.open(agent(3)));
        assert_eq!(layout.panes.len(), 2);
    }

    #[test]
    fn closing_the_focused_pane_repairs_focus() {
        let mut layout = PaneLayout::default();
        layout.open(agent(1));
        layout.open(agent(2));
        assert!(layout.close(agent(2)));
        assert_eq!(layout.focused, 0);
        assert_eq!(layout.focused_agent(), Some(agent(1)));
    }

    #[test]
    fn focus_cycles_between_panes() {
        let mut layout = PaneLayout::default();
        layout.open(agent(1));
        layout.open(agent(2));
        assert!(layout.cycle_focus());
        assert_eq!(layout.focused_agent(), Some(agent(1)));
        assert!(layout.cycle_focus());
        assert_eq!(layout.focused_agent(), Some(agent(2)));
    }

    #[test]
    fn closing_an_unknown_pane_is_a_no_op() {
        let mut layout = PaneLayout::default();
        layout.open(agent(1));
        assert!(!layout.close(agent(9)));
    }

    #[test]
    fn split_widths_are_even_by_default_and_never_degenerate() {
        assert_eq!(PaneLayout::split_widths(80), (40, 40));
        assert_eq!(PaneLayout::split_widths(81), (41, 40));
        // A 1-column terminal cannot be split into two usable panes.
        assert_eq!(PaneLayout::split_widths(1), (1, 0));
        let (left, right) = PaneLayout::split_widths(3);
        assert!(left >= 1 && right >= 1);
    }

    #[test]
    fn split_widths_honour_an_explicit_ratio() {
        let (left, right) = PaneLayout::split_widths_with(100, 30);
        assert_eq!((left, right), (70, 30));
        // Out-of-range ratios fall back to an even split.
        assert_eq!(PaneLayout::split_widths_with(100, 0), (50, 50));
        assert_eq!(PaneLayout::split_widths_with(100, 100), (50, 50));
    }

    #[test]
    fn profile_identity_covers_backend_model_and_workspace() {
        let base = ProfileSpec::new("a", PathBuf::from("/w1"));
        assert!(base.same_as(&ProfileSpec::new("a", PathBuf::from("/w1"))));
        let mut other_model = base.clone();
        other_model.model = Some("p/m".to_string());
        assert!(!base.same_as(&other_model));
        assert!(!base.same_as(&ProfileSpec::new("b", PathBuf::from("/w1"))));
        assert!(!base.same_as(&ProfileSpec::new("a", PathBuf::from("/w2"))));
    }

    #[test]
    fn profile_label_defaults_to_primary() {
        let spec = ProfileSpec::new("", PathBuf::from("/w"));
        assert_eq!(spec.display_label(), "primary");
    }
}
