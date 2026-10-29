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
    /// This connection's read-only HTTP event stream, when the kernel advertised
    /// one. Optional by design: the ACP path is complete without it, and
    /// `OPENBUILD_HTTP_FEED=0` turns it off.
    ///
    /// Holding it here (rather than in one loop-local `Option`) is what lets
    /// each pane keep its own feed: the event loop drains every entry instead of
    /// only the primary's.
    pub http_feed: Option<openbuild_backend::http_feed::HttpFeed>,
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
            http_feed: None,
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
    /// Per-pane width weights, one entry per pane.
    ///
    /// Empty means "every pane gets the same width". A weight is a share of the
    /// whole row rather than a pixel count, so the layout survives a resize;
    /// [`Self::percents`] normalises whatever is stored into parts of 100 and
    /// [`Self::widths`] is the only place that turns it into columns. Rendering
    /// and hit-testing both call `widths`, which is what keeps them in sync
    /// across two or more panes.
    pub split_weights: Vec<u16>,
}

impl Default for PaneLayout {
    fn default() -> Self {
        Self {
            panes: Vec::new(),
            focused: 0,
            split_weights: Vec::new(),
        }
    }
}

impl PaneLayout {
    /// Single-pane mode is the historical layout.
    pub fn is_single(&self) -> bool {
        self.panes.len() <= 1
    }

    /// Whether two or more panes are visible.
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

    /// Move focus one pane to the left (`-1`) or right (`+1`), clamped.
    ///
    /// With two panes this is exactly the old left/right pair; with more it
    /// walks the row instead of jumping to a hard-coded index.
    pub fn focus_step(&mut self, delta: isize) -> bool {
        if self.panes.len() < 2 {
            return false;
        }
        let last = self.panes.len() - 1;
        let next = (self.focused as isize + delta).clamp(0, last as isize) as usize;
        if next == self.focused {
            return false;
        }
        self.focused = next;
        true
    }

    /// Add a pane to the right.
    ///
    /// The cap is the connection registry's slot count, not a fixed two: each
    /// pane owns a kernel, and `MAX_CONNECTIONS` is what the backend can hold.
    pub fn open(&mut self, id: AgentId) -> bool {
        if self.panes.contains(&id)
            || self.panes.len() >= openbuild_backend::profile::MAX_CONNECTIONS
        {
            return false;
        }
        self.panes.push(id);
        // A new pane changes the row, so a stale per-pane ratio would be applied
        // to the wrong pane. Reset to the documented even split instead.
        self.split_weights.clear();
        if self.panes.len() > 1 {
            self.focused = self.panes.len() - 1;
        }
        true
    }

    /// Remove a pane; focus falls back to the remaining one.
    pub fn close(&mut self, id: AgentId) -> bool {
        let Some(index) = self.panes.iter().position(|pane| *pane == id) else {
            return false;
        };
        self.panes.remove(index);
        if index < self.split_weights.len() {
            self.split_weights.remove(index);
        }
        if self.focused >= self.panes.len() {
            self.focused = self.panes.len().saturating_sub(1);
        }
        true
    }

    /// Drop panes that no longer exist (session closed) and repair focus.
    pub fn retain_existing(&mut self, agents: &indexmap::IndexMap<AgentId, AgentView>) -> bool {
        let before = (self.panes.clone(), self.focused);
        let mut index = 0;
        self.panes.retain(|id| {
            let keep = agents.contains_key(id);
            if !keep && index < self.split_weights.len() {
                self.split_weights.remove(index);
            } else {
                index += 1;
            }
            keep
        });
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

    /// Percent of the row each pane occupies, normalised to parts of 100.
    ///
    /// The vector always has one entry per visible pane; a single pane owns the
    /// whole row. Missing or zero weights fall back to an even split, so a
    /// stale vector can never produce a zero-width pane.
    pub fn percents(&self) -> Vec<u16> {
        let count = self.panes.len();
        if count == 0 {
            return Vec::new();
        }
        if count == 1 {
            return vec![100];
        }
        let weights: Vec<u32> = (0..count)
            .map(|index| u32::from(self.split_weights.get(index).copied().unwrap_or(0)).max(1))
            .collect();
        let total: u32 = weights.iter().sum();
        let mut out = Vec::with_capacity(count);
        let mut used = 0u32;
        for (index, weight) in weights.iter().enumerate() {
            let percent = if index + 1 == count {
                // The last pane absorbs the rounding remainder so the parts
                // always add up to exactly 100.
                100u32.saturating_sub(used)
            } else {
                ((weight * 100 + total / 2) / total).clamp(1, 99)
            };
            used += percent;
            out.push(percent as u16);
        }
        out
    }

    /// Column count per pane for a row `width` columns wide.
    ///
    /// Every pane keeps at least one column and the last one takes the rounding
    /// remainder, so `sum(widths) == width` and rendering can build rectangles by
    /// prefix sum without a gap or an off-by-one divider.
    pub fn widths(&self, width: u16) -> Vec<u16> {
        let count = self.panes.len();
        if count == 0 {
            return Vec::new();
        }
        if count == 1 || width == 0 {
            return vec![width];
        }
        let percents = self.percents();
        let mut out = Vec::with_capacity(count);
        let mut used = 0u16;
        for (index, percent) in percents.iter().enumerate() {
            let columns = if index + 1 == count {
                width.saturating_sub(used).max(1)
            } else {
                ((u32::from(width) * u32::from(*percent) + 50) / 100) as u16
            };
            let columns = columns.max(1);
            used = used.saturating_add(columns);
            out.push(columns);
        }
        out
    }

    /// Set the rightmost pane's share of the row.
    ///
    /// This keeps the pre-N semantics of `split_percent`: "the right pane is
    /// `percent` wide". With two panes `30` means `[70, 30]`; with more, the
    /// rightmost pane takes `percent` and the others share the remainder
    /// evenly, so the command stays meaningful without a per-boundary syntax.
    /// `0` (or anything out of range) restores the even split.
    pub fn set_split_percent(&mut self, percent: u16) {
        let count = self.panes.len();
        if count < 2 {
            return;
        }
        if !(1..=99).contains(&percent) {
            self.split_weights.clear();
            return;
        }
        let others = (count - 1) as u16;
        let remainder = 100u16.saturating_sub(percent);
        self.split_weights = (0..count)
            .map(|index| {
                if index + 1 == count {
                    percent
                } else {
                    (remainder / others).max(1)
                }
            })
            .collect();
    }

    /// The ratio the user last asked for: the rightmost pane's percent.
    ///
    /// A fresh layout answers 0, the documented "even split" value.
    pub fn split_percent(&self) -> u16 {
        if self.split_weights.is_empty() || self.panes.len() < 2 {
            return 0;
        }
        self.percents().last().copied().unwrap_or(0)
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
    fn a_third_pane_is_allowed_until_the_slot_cap() {
        let mut layout = PaneLayout::default();
        for slot in 1..=openbuild_backend::profile::MAX_CONNECTIONS {
            assert!(layout.open(agent(slot)), "slot {slot} must open");
        }
        assert_eq!(
            layout.panes.len(),
            openbuild_backend::profile::MAX_CONNECTIONS
        );
        assert!(
            !layout.open(agent(99)),
            "a pane beyond the connection cap must be refused"
        );
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
    fn closing_a_middle_pane_keeps_the_rest_and_repairs_focus() {
        let mut layout = PaneLayout::default();
        layout.open(agent(1));
        layout.open(agent(2));
        layout.open(agent(3));
        layout.focus(agent(2));
        assert!(layout.close(agent(2)));
        assert_eq!(layout.panes, vec![agent(1), agent(3)]);
        assert!(layout.focused < layout.panes.len());
        // The remaining row is split into exactly the terminal width.
        let widths = layout.widths(120);
        assert_eq!(widths.iter().map(|w| u32::from(*w)).sum::<u32>(), 120);
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

    /// `/pane split 30` must survive the trip through the vector geometry: the
    /// row is still 70/30 and a click at column 71 is the right pane.
    #[test]
    fn an_explicit_percent_round_trips_through_widths() {
        let mut layout = PaneLayout::default();
        layout.open(agent(1));
        layout.open(agent(2));
        layout.set_split_percent(30);
        assert_eq!(layout.split_percent(), 30);
        assert_eq!(layout.percents(), vec![70, 30]);
        assert_eq!(layout.widths(100), vec![70, 30]);
        // 0 restores the even split.
        layout.set_split_percent(0);
        assert_eq!(layout.split_percent(), 0);
        assert_eq!(layout.widths(100), vec![50, 50]);
    }

    #[test]
    fn widths_fill_the_row_exactly_for_every_pane_count() {
        for count in 2..=openbuild_backend::profile::MAX_CONNECTIONS {
            let mut layout = PaneLayout::default();
            for slot in 1..=count {
                layout.open(agent(slot));
            }
            for width in [24u16, 61, 80, 120, 200] {
                let widths = layout.widths(width);
                assert_eq!(widths.len(), count, "one width per pane at {count} panes");
                assert_eq!(
                    widths.iter().map(|w| u32::from(*w)).sum::<u32>(),
                    u32::from(width),
                    "widths must add up to the row at {count} panes / {width} columns"
                );
                assert!(
                    widths.iter().all(|w| *w >= 1),
                    "every pane keeps at least one column: {widths:?}"
                );
                let percents = layout.percents();
                assert_eq!(percents.len(), count);
                assert_eq!(
                    percents.iter().map(|p| u32::from(*p)).sum::<u32>(),
                    100,
                    "percents always add up to 100: {percents:?}"
                );
            }
        }
    }

    #[test]
    fn focus_steps_walk_the_row_and_clamp() {
        let mut layout = PaneLayout::default();
        layout.open(agent(1));
        layout.open(agent(2));
        layout.open(agent(3));
        assert_eq!(layout.focused, 2);
        assert!(layout.focus_step(-1));
        assert_eq!(layout.focused, 1);
        assert!(layout.focus_step(-1));
        assert_eq!(layout.focused, 0);
        assert!(!layout.focus_step(-1), "already at the left edge");
        assert!(layout.focus_step(1));
        assert_eq!(layout.focused, 1);
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
