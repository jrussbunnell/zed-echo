//! The fleet section: every dispatched Claude Code session, above the threads.
//!
//! Deliberately not a [`crate::ListEntry`] variant. A fleet session is not a
//! thread — it cannot be renamed, archived, or activated, its actions are its
//! own, and it belongs to the daemon rather than to this workspace. Rendering it
//! as a separate block keeps the thread list's selection, ordering, and folding
//! machinery untouched.
//!
//! See `docs/superpowers/specs/2026-08-22-fleet-and-workflows-design.md`.

use fleet::{Fleet, FleetSession, FleetState, Teammate, control};
use gpui::{AnyElement, Entity, Subscription, WeakEntity, px};
use std::collections::HashMap;
use task::{
    HideStrategy, RevealStrategy, RevealTarget, SaveStrategy, Shell, SpawnInTerminal, TaskId,
};
use terminal_view::terminal_panel::TerminalPanel;
use ui::{
    Color, CommonAnimationExt, Icon, IconName, IconSize, Label, LabelSize, Tooltip, prelude::*,
};
use workspace::Workspace;

use crate::Sidebar;

/// How a session's state reads in the list.
fn state_icon(state: &FleetState) -> IconName {
    match state {
        FleetState::Working => IconName::LoadCircle,
        FleetState::Blocked => IconName::Warning,
        FleetState::Done => IconName::Check,
        FleetState::Failed => IconName::XCircle,
        FleetState::Stopped => IconName::Close,
        FleetState::Unknown(_) => IconName::TodoPending,
    }
}

fn state_color(state: &FleetState) -> Color {
    match state {
        FleetState::Working => Color::Accent,
        FleetState::Blocked => Color::Warning,
        FleetState::Done => Color::Success,
        FleetState::Failed => Color::Error,
        FleetState::Stopped | FleetState::Unknown(_) => Color::Muted,
    }
}

/// Token counts get long. The list has room for a hint, not a figure.
fn abbreviate_tokens(tokens: u64) -> String {
    match tokens {
        0 => String::new(),
        count if count < 1_000 => format!("{count}"),
        count if count < 1_000_000 => format!("{}k", count / 1_000),
        count => format!("{}M", count / 1_000_000),
    }
}

pub struct FleetSection {
    fleet: Entity<Fleet>,
    expanded: bool,
    /// Sessions whose teammates are shown. Collapsed by default so a team of
    /// five does not push every other session off the screen.
    expanded_teams: HashMap<SharedString, bool>,
    _observation: Subscription,
}

impl FleetSection {
    pub fn new(cx: &mut Context<Sidebar>) -> Self {
        let fleet = Fleet::global(cx);
        let observation = cx.observe(&fleet, |_, _, cx| cx.notify());
        // Polling starts here and stops when the sidebar is dropped, so a
        // window with no sidebar never reads the disk.
        fleet.update(cx, |fleet, cx| fleet.watch(cx));
        Self {
            fleet,
            expanded: true,
            expanded_teams: HashMap::default(),
            _observation: observation,
        }
    }

    fn toggle(&mut self) {
        self.expanded = !self.expanded;
    }

    /// Everything the section needs to draw itself, copied out of the entity.
    ///
    /// Taken as a snapshot rather than read during rendering because building a
    /// row registers listeners, which needs the context mutably while a read
    /// borrow of it would still be live.
    pub fn snapshot(&self, cx: &App) -> FleetSnapshot {
        let fleet = self.fleet.read(cx);
        FleetSnapshot {
            sessions: fleet.sessions().to_vec(),
            waiting: fleet.needs_attention_count(),
            expanded: self.expanded,
            expanded_teams: self.expanded_teams.clone(),
        }
    }

    fn render_header(&self, total: usize, waiting: usize, cx: &mut Context<Sidebar>) -> AnyElement {
        let colors = cx.theme().colors();
        h_flex()
            .id("fleet-header")
            .w_full()
            .h(rems_from_px(24_f32))
            .px_1p5()
            .gap_1p5()
            .justify_between()
            .cursor_pointer()
            .hover(|style| style.bg(colors.element_hover))
            .child(
                h_flex()
                    .gap_1p5()
                    .min_w_0()
                    .child(
                        Icon::new(if self.expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        Label::new("Fleet")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(format!("{total}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            // The count of sessions waiting on the user is the one number worth
            // seeing with the section collapsed.
            .when(waiting > 0, |this| {
                this.child(
                    Label::new(format!("{waiting} waiting"))
                        .size(LabelSize::XSmall)
                        .color(Color::Warning),
                )
            })
            .on_click(cx.listener(|sidebar, _, _, cx| {
                sidebar.fleet_section.toggle();
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_session(
        &self,
        index: usize,
        session: &FleetSession,
        team_open: bool,
        workspace: &WeakEntity<Workspace>,
        cx: &mut Context<Sidebar>,
    ) -> Vec<AnyElement> {
        let colors = cx.theme().colors();
        let state = session.state.clone();
        let color = state_color(&state);
        let has_team = !session.teammates.is_empty();

        let status_icon = if matches!(state, FleetState::Working) {
            Icon::new(state_icon(&state))
                .size(IconSize::XSmall)
                .color(color)
                .with_rotate_animation(2)
                .into_any_element()
        } else {
            Icon::new(state_icon(&state))
                .size(IconSize::XSmall)
                .color(color)
                .into_any_element()
        };

        // The session's own account of what it did, falling back to the prompt
        // it was dispatched with. One of the two is always worth showing.
        let subtitle = session
            .detail
            .clone()
            .or_else(|| session.intent.clone())
            .unwrap_or_else(|| SharedString::from(state.label().to_string()));

        let tokens = abbreviate_tokens(session.tokens);
        let name = session.name.clone();
        let short_id = session.short_id.clone();
        let actions = control::actions_for(&state);
        let can_attach = actions.contains(&control::SessionAction::Attach);

        let mut rows = vec![
            h_flex()
                .id(("fleet-session", index))
                .w_full()
                .min_w_0()
                .h(rems_from_px(30_f32))
                .pl_1p5()
                .pr_1p5()
                .gap_1p5()
                .cursor_pointer()
                .hover(|style| style.bg(colors.element_hover))
                .child(
                    div()
                        .flex_none()
                        .w(px(2.))
                        .h(rems_from_px(18_f32))
                        .rounded_full()
                        .bg(color.color(cx)),
                )
                .child(status_icon)
                .child(
                    v_flex()
                        .min_w_0()
                        .flex_1()
                        .child(Label::new(name.clone()).size(LabelSize::Small).truncate())
                        .child(
                            Label::new(subtitle.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                )
                .when(!tokens.is_empty(), |this| {
                    this.child(
                        Label::new(tokens.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .flex_shrink_0(),
                    )
                })
                .when(has_team, |this| {
                    this.child(
                        Icon::new(if team_open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                })
                .tooltip({
                    let name = name.clone();
                    let subtitle = subtitle.clone();
                    let state_label = state.label().to_string();
                    let attachable = can_attach;
                    Tooltip::element(move |_, _| {
                        v_flex()
                            .gap_0p5()
                            .child(Label::new(name.clone()))
                            .child(
                                Label::new(format!("Fleet · {state_label}"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(subtitle.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(if attachable {
                                    "Click to attach in a terminal"
                                } else {
                                    "This session has already exited"
                                })
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                            )
                            .into_any_element()
                    })
                })
                .on_click({
                    let short_id = short_id.clone();
                    let workspace = workspace.clone();
                    let cwd = session.cwd.clone();
                    cx.listener(move |sidebar, _, window, cx| {
                        if has_team {
                            let open = sidebar
                                .fleet_section
                                .expanded_teams
                                .entry(short_id.clone())
                                .or_insert(false);
                            *open = !*open;
                            cx.notify();
                        }
                        if can_attach {
                            attach_to_session(&workspace, &short_id, cwd.clone(), window, cx);
                        }
                    })
                })
                .into_any_element(),
        ];

        if has_team && team_open {
            rows.extend(
                session
                    .teammates
                    .iter()
                    .enumerate()
                    .map(|(teammate_index, teammate)| {
                        render_teammate(index, teammate_index, teammate, cx)
                    }),
            );
        }
        rows
    }
}

/// A teammate row, nested under its session the way a subagent nests under the
/// thread that spawned it.
fn render_teammate(
    session_index: usize,
    teammate_index: usize,
    teammate: &Teammate,
    cx: &mut Context<Sidebar>,
) -> AnyElement {
    let colors = cx.theme().colors();
    let detail = teammate
        .model
        .clone()
        .or_else(|| teammate.agent_type.clone())
        .unwrap_or_else(|| SharedString::from("teammate"));

    h_flex()
        .id(("fleet-teammate", session_index * 100 + teammate_index))
        .w_full()
        .min_w_0()
        .h(rems_from_px(22_f32))
        .pl(rems_from_px(26_f32))
        .pr_1p5()
        .gap_1p5()
        .hover(|style| style.bg(colors.element_hover))
        .child(
            Icon::new(IconName::Person)
                .size(IconSize::XSmall)
                .color(Color::Muted),
        )
        .child(
            Label::new(teammate.name.clone())
                .size(LabelSize::Small)
                .color(Color::Muted)
                .truncate(),
        )
        .child(
            Label::new(detail)
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .flex_shrink_0(),
        )
        .into_any_element()
}

/// Open a terminal running `claude attach <short id>`.
///
/// This is the only supported way to put a turn into a session that is still
/// running: the daemon owns it, so ACP refuses to load it, and the CLI has no
/// `send` verb. The terminal is Echo's, so the session's own interface appears
/// in place rather than in another window.
fn attach_to_session(
    workspace: &WeakEntity<Workspace>,
    short_id: &str,
    cwd: Option<std::path::PathBuf>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    let Some(terminal_panel) = workspace.read(cx).panel::<TerminalPanel>(cx) else {
        log::warn!("fleet: no terminal panel, so there is nowhere to attach");
        return;
    };

    let argv = control::attach_argv(short_id);
    let Some((program, arguments)) = argv.split_first() else {
        return;
    };
    let command_label = argv.join(" ");

    let spawn = SpawnInTerminal {
        id: TaskId(format!("fleet-attach-{short_id}")),
        full_label: command_label.clone(),
        label: format!("attach {short_id}"),
        command: Some(program.clone()),
        args: arguments.to_vec(),
        command_label,
        cwd,
        env: Default::default(),
        // A second attach to the same session would fight the first for the
        // pty, so the existing tab is reused rather than duplicated.
        use_new_terminal: false,
        allow_concurrent_runs: false,
        reveal: RevealStrategy::Always,
        reveal_target: RevealTarget::Dock,
        // The session outlives the attach; leaving the tab up makes that
        // visible rather than implying the session ended with it.
        hide: HideStrategy::Never,
        shell: Shell::System,
        show_summary: false,
        show_command: false,
        show_rerun: true,
        save: SaveStrategy::None,
    };

    terminal_panel
        .update(cx, |terminal_panel, cx| {
            terminal_panel.spawn_task(&spawn, window, cx)
        })
        .detach_and_log_err(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_are_abbreviated_not_printed_in_full() {
        assert_eq!(abbreviate_tokens(0), "", "zero is absence, not a figure");
        assert_eq!(abbreviate_tokens(7), "7");
        assert_eq!(abbreviate_tokens(999), "999");
        assert_eq!(abbreviate_tokens(1_000), "1k");
        assert_eq!(abbreviate_tokens(88_500), "88k");
        assert_eq!(abbreviate_tokens(2_400_000), "2M");
    }

    #[test]
    fn every_state_has_an_icon_and_a_colour() {
        // A state from a newer CLI still renders; it must not fall through to a
        // panic or an invisible row.
        for state in [
            FleetState::Working,
            FleetState::Blocked,
            FleetState::Done,
            FleetState::Failed,
            FleetState::Stopped,
            FleetState::Unknown("hibernating".into()),
        ] {
            let _ = state_icon(&state);
            let _ = state_color(&state);
        }
    }

    #[test]
    fn a_session_that_needs_the_user_is_coloured_as_a_warning() {
        assert_eq!(state_color(&FleetState::Blocked), Color::Warning);
        assert_eq!(state_color(&FleetState::Done), Color::Success);
        assert_eq!(state_color(&FleetState::Failed), Color::Error);
    }
}

/// A copy of the fleet taken before rendering begins.
pub struct FleetSnapshot {
    pub sessions: Vec<FleetSession>,
    pub waiting: usize,
    pub expanded: bool,
    pub expanded_teams: HashMap<SharedString, bool>,
}

/// Render the section, or nothing at all.
///
/// A hidden section is the right answer for two different situations: nothing
/// has ever been dispatched, and the `claude` CLI is not installed. An empty
/// list would read as "no sessions" in both, and only one of them is true.
pub fn render_fleet_section(
    section: &FleetSection,
    snapshot: FleetSnapshot,
    workspace: &WeakEntity<Workspace>,
    cx: &mut Context<Sidebar>,
) -> Option<AnyElement> {
    if snapshot.sessions.is_empty() {
        return None;
    }
    let total = snapshot.sessions.len();
    let header = section.render_header(total, snapshot.waiting, cx);

    let rows: Vec<AnyElement> = if snapshot.expanded {
        snapshot
            .sessions
            .iter()
            .enumerate()
            .flat_map(|(index, session)| {
                let team_open = snapshot
                    .expanded_teams
                    .get(&session.short_id)
                    .copied()
                    .unwrap_or(false);
                section.render_session(index, session, team_open, workspace, cx)
            })
            .collect()
    } else {
        Vec::new()
    };

    Some(
        v_flex()
            .w_full()
            .child(header)
            .children(rows)
            .into_any_element(),
    )
}
