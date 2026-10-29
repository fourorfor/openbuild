//! Side-by-side rendering of two agent panes.
//!
//! The single-pane renderer still owns the whole screen in the default mode.
//! When `/pane open` has a second kernel connected, the same agent renderer is
//! invoked once per rectangle instead: nothing about a pane's own layout changes,
//! only the rectangle it is given and the mouse coordinates it is told about.
//!
//! Two facts make that safe:
//!
//! - `AgentView::draw` already takes an `area` and derives every hit rectangle
//!   from `area.x`, so a pane drawn at `x = 40` reports hits at `x = 40+`.
//! - Mouse positions are absolute terminal coordinates, so each pane must be
//!   told the position only when the pointer is inside *its* rectangle,
//!   otherwise both panes hover the same row.

use ratatui::layout::Rect;

use super::agent::AgentId;
use super::agent_view::{AgentView, AppRenderParams, BannerSlotParams};
use super::connections::PaneLayout;
use crate::actions::ActionRegistry;
use crate::scrollback::render::ScratchBuffer;
use crate::views::shortcuts_bar::PendingHint;

/// Everything the split renderer needs from the app frame.
pub(crate) struct SplitPaneParams<'a> {
    pub layout: &'a PaneLayout,
    pub agents: &'a mut indexmap::IndexMap<AgentId, AgentView>,
    pub buf: &'a mut ratatui::buffer::Buffer,
    pub area: Rect,
    pub registry: &'a ActionRegistry,
    pub scratch: &'a mut ScratchBuffer,
    pub pending_hint: Option<PendingHint>,
    pub link_spans: &'a mut Vec<xai_ratatui_inline::LinkSpan>,
    pub announcements: &'a [xai_grok_announcements::RemoteAnnouncement],
    pub hidden_ids: &'a std::collections::BTreeSet<String>,
    pub privacy_banner: bool,
    /// Absolute terminal mouse position, if any.
    pub mouse_pos: Option<(u16, u16)>,
}

/// Render both panes. Returns the cursor to place, if the focused pane has one.
///
/// `None` means "this frame is not a split frame" (a pane is missing); the
/// caller then falls back to the normal single-view path instead of painting a
/// half-empty screen.
pub(crate) fn render_split_panes(params: SplitPaneParams<'_>) -> Option<Option<(u16, u16)>> {
    let SplitPaneParams {
        layout,
        agents,
        buf,
        area,
        registry,
        scratch,
        pending_hint,
        link_spans,
        announcements,
        hidden_ids,
        privacy_banner,
        mouse_pos,
    } = params;
    if !layout.is_split() || area.width < 20 {
        return None;
    }
    let panes: Vec<AgentId> = layout
        .panes
        .iter()
        .copied()
        .filter(|id| agents.contains_key(id))
        .collect();
    if panes.len() < 2 {
        return None;
    }

    let (left_width, right_width) = PaneLayout::split_widths_with(area.width, layout.split_percent);
    let rects = [
        Rect {
            x: area.x,
            y: area.y,
            width: left_width,
            height: area.height,
        },
        Rect {
            x: area.x + left_width,
            y: area.y,
            width: right_width,
            height: area.height,
        },
    ];

    let mut cursor = None;
    for (index, id) in panes.iter().enumerate().take(2) {
        let rect = rects[index];
        let focused = index == layout.focused;
        let Some(agent) = agents.get_mut(id) else {
            continue;
        };
        // Only the pane under the pointer gets that position, so hover states
        // cannot leak across the divider.
        let pane_mouse = mouse_pos.filter(|(mx, my)| {
            *mx >= rect.x && *mx < rect.x + rect.width && *my >= rect.y && *my < rect.y + rect.height
        });
        let (pane_cursor, _post_flush) = agent.draw(
            rect,
            buf,
            registry,
            scratch,
            if focused { pending_hint } else { None },
            false,
            BannerSlotParams {
                height: 0,
                announcements,
                hidden_ids,
                privacy_banner: false,
                mouse_pos: pane_mouse,
                tip: None,
            },
            false,
            link_spans,
            AppRenderParams {
                voice_available: false,
                voice_listening: false,
                voice_interim: None,
                status_line: crate::views::status_line::StatusLineFrame::Off,
                workspace_dashboard_enabled: false,
                overlay_header: super::agent_view::OverlayHeader::default(),
                overlay_stop_label: None,
            },
        );
        if focused {
            cursor = pane_cursor;
        }
        draw_pane_frame(buf, rect, index + 1, focused);
    }
    // The divider column belongs to no pane; paint it so the split reads as one
    // surface instead of two transcripts sharing a background.
    let theme = crate::theme::Theme::current();
    for y in rects[0].y..rects[0].y + rects[0].height {
        if let Some(cell) = buf.cell_mut((rects[1].x.saturating_sub(1), y)) {
            cell.set_symbol("│");
            cell.set_style(ratatui::style::Style::default().fg(theme.text_secondary));
        }
    }
    let _ = privacy_banner;
    Some(cursor)
}

/// Paint a header strip identifying the pane and whether it owns the keyboard.
fn draw_pane_frame(
    buf: &mut ratatui::buffer::Buffer,
    rect: Rect,
    index: usize,
    focused: bool,
) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let theme = crate::theme::Theme::current();
    let label = if focused {
        format!(" pane {index} * ")
    } else {
        format!(" pane {index} ")
    };
    let style = if focused {
        ratatui::style::Style::default()
            .fg(theme.bg_base)
            .bg(theme.accent_user)
            .add_modifier(ratatui::style::Modifier::BOLD)
    } else {
        ratatui::style::Style::default()
            .fg(theme.text_secondary)
            .bg(theme.bg_base)
    };
    for (offset, ch) in label.chars().enumerate() {
        let x = rect.x + offset as u16;
        if x >= rect.x + rect.width {
            break;
        }
        if let Some(cell) = buf.cell_mut((x, rect.y)) {
            cell.set_symbol(&ch.to_string());
            cell.set_style(style);
        }
    }
}

/// Which pane contains a terminal coordinate, if any.
pub(crate) fn pane_at(layout: &PaneLayout, area: Rect, column: u16, row: u16) -> Option<AgentId> {
    if !layout.is_split() || area.width < 20 {
        return None;
    }
    if row < area.y || row >= area.y + area.height {
        return None;
    }
    let (left_width, _) = PaneLayout::split_widths_with(area.width, layout.split_percent);
    let index = if column < area.x + left_width { 0 } else { 1 };
    layout.panes.get(index).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> PaneLayout {
        let mut layout = PaneLayout::default();
        layout.open(AgentId(1));
        layout.open(AgentId(2));
        layout
    }

    #[test]
    fn a_single_pane_is_never_split_rendered() {
        let mut layout = PaneLayout::default();
        layout.open(AgentId(1));
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(pane_at(&layout, area, 10, 10), None);
    }

    #[test]
    fn clicks_resolve_to_the_pane_under_them() {
        let layout = layout();
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(pane_at(&layout, area, 5, 10), Some(AgentId(1)));
        assert_eq!(pane_at(&layout, area, 70, 10), Some(AgentId(2)));
        // Outside the vertical extent belongs to no pane.
        assert_eq!(pane_at(&layout, area, 40, 30), None);
    }

    #[test]
    fn a_narrow_terminal_is_not_split() {
        let layout = layout();
        let area = Rect::new(0, 0, 10, 5);
        assert_eq!(pane_at(&layout, area, 3, 2), None);
    }

    #[test]
    fn the_divider_column_maps_to_the_right_pane() {
        let layout = layout();
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(pane_at(&layout, area, 39, 1), Some(AgentId(1)));
        assert_eq!(pane_at(&layout, area, 40, 1), Some(AgentId(2)));
    }

    /// `/pane split 30` must move the divider for both painting and hit-testing:
    /// they share `split_widths_with`, so the click at column 71 is the right
    /// pane and column 69 is still the left one.
    #[test]
    fn an_explicit_ratio_moves_the_divider_for_hits() {
        let mut layout = layout();
        layout.split_percent = 30;
        let area = Rect::new(0, 0, 100, 24);
        assert_eq!(pane_at(&layout, area, 69, 5), Some(AgentId(1)));
        assert_eq!(pane_at(&layout, area, 71, 5), Some(AgentId(2)));
    }
}
