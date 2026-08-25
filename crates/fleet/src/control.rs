//! Dispatching and controlling fleet sessions through the `claude` CLI.
//!
//! Echo drives the supervisor rather than reimplementing it, so everything here
//! builds a command line. The verbs are the CLI's own: `--bg` to dispatch, then
//! `stop`, `respawn`, `logs`, and `attach`. Only `attach` is interactive, and it
//! is the only supported way to put a turn into a session that is still running
//! — there is no `claude send`, verified against 2.1.241's subcommand list.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::FleetState;

/// The executable name. Resolved through `PATH` rather than an absolute path so
/// a version manager's shim keeps working.
const CLAUDE: &str = "claude";

/// Enabling teams is per dispatch, never per install: the variable changes
/// delegation for the whole session, so any subagent Claude names becomes a
/// teammate whether or not the user asked for a team.
const TEAMS_VAR: &str = "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS";

#[derive(Clone, Debug)]
pub struct Dispatch {
    pub prompt: String,
    /// A name the user gave. Without one the CLI generates a name from the cwd.
    pub name: Option<String>,
    pub cwd: PathBuf,
    pub enable_teams: bool,
    /// Overrides the model for the dispatched session only.
    pub model: Option<String>,
}

impl Dispatch {
    pub fn new(prompt: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            prompt: prompt.into(),
            name: None,
            cwd: cwd.into(),
            enable_teams: false,
            model: None,
        }
    }

    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn with_teams(mut self, enable: bool) -> Self {
        self.enable_teams = enable;
        self
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(CLAUDE);
        command.current_dir(&self.cwd);
        command.arg("--bg");
        if let Some(name) = &self.name {
            command.arg("--name").arg(name);
        }
        if let Some(model) = &self.model {
            command.arg("--model").arg(model);
        }
        // The prompt goes last and unflagged; it is a positional argument, so a
        // prompt beginning with a dash would otherwise be read as a flag.
        command.arg("--").arg(&self.prompt);
        if self.enable_teams {
            command.env(TEAMS_VAR, "1");
        }
        command
    }
}

/// A CLI verb that takes a session's short id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Stop,
    Respawn,
    Logs,
    /// Interactive, and the only way to talk to a running session.
    Attach,
    Remove,
}

impl Verb {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Respawn => "respawn",
            Self::Logs => "logs",
            Self::Attach => "attach",
            Self::Remove => "rm",
        }
    }

    /// Whether this verb needs a terminal. Only `attach` does, which is why it
    /// is run in a terminal Echo owns rather than captured.
    pub fn is_interactive(self) -> bool {
        matches!(self, Self::Attach)
    }
}

pub fn command(verb: Verb, short_id: &str) -> Command {
    let mut command = Command::new(CLAUDE);
    command.arg(verb.as_str()).arg(short_id);
    command
}

/// The shell words Echo hands a terminal to attach to a session.
pub fn attach_argv(short_id: &str) -> Vec<String> {
    vec![
        CLAUDE.to_string(),
        Verb::Attach.as_str().to_string(),
        short_id.to_string(),
    ]
}

/// What Echo offers for a session in a given state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionAction {
    /// Follow the transcript on disk. Always available, never destructive.
    Read,
    /// Talk to it live, in a terminal, without stopping it.
    Attach,
    /// Adopt a session that has already finished.
    OpenInEcho,
    /// Adopt a session that is still running, naming the stop in the label.
    StopAndOpen,
}

impl SessionAction {
    /// Whether taking this action ends the session's own process.
    pub fn stops_the_session(self) -> bool {
        matches!(self, Self::StopAndOpen)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Read => "Follow",
            Self::Attach => "Attach",
            Self::OpenInEcho => "Open in Echo",
            Self::StopAndOpen => "Stop and open",
        }
    }
}

/// The actions offered for a session, most conservative first.
///
/// The daemon owns a session while it runs, so ACP refuses to load one and
/// adopting means stopping first. Attach exists precisely so that talking to a
/// working session does not require killing it.
pub fn actions_for(state: &FleetState) -> Vec<SessionAction> {
    match state {
        // Still alive: attach is the non-destructive way in, and adopting is
        // offered with the stop named.
        FleetState::Working | FleetState::Blocked => {
            vec![
                SessionAction::Read,
                SessionAction::Attach,
                SessionAction::StopAndOpen,
            ]
        }
        // The process is gone. `claude attach` would fail, so it is not
        // offered; adopting is a no-op stop followed by a load.
        FleetState::Done | FleetState::Failed | FleetState::Stopped => {
            vec![SessionAction::Read, SessionAction::OpenInEcho]
        }
        // A state from a newer CLI. Whether the process is alive is unknown, so
        // only the action that is safe either way is offered: the stop in
        // `StopAndOpen` is a no-op on an exited session.
        FleetState::Unknown(_) => vec![SessionAction::Read, SessionAction::StopAndOpen],
    }
}

/// Run a non-interactive verb to completion, returning its stdout.
///
/// Async rather than blocking: these are spawned processes, and a stop that
/// takes a second must not hold the foreground. The CLI writes its errors to
/// stderr and exits non-zero, and both are surfaced rather than swallowed — a
/// stop that quietly failed would leave Echo showing a session it believes it
/// stopped.
///
/// Commands are built as [`std::process::Command`] so their arguments stay
/// inspectable, and converted here. Nothing sets stdio, which is the one thing
/// `smol`'s conversion does not preserve.
pub async fn run(command: Command) -> anyhow::Result<String> {
    let output = smol::process::Command::from(command)
        .output()
        .await
        .map_err(|error| anyhow::anyhow!("could not run the claude CLI: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        anyhow::bail!(
            "claude exited with {}{}",
            output.status,
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether the CLI is reachable at all. The fleet section stays hidden when it
/// is not, rather than showing an empty list that looks like "no sessions".
pub fn cli_is_available() -> bool {
    which_claude().is_some()
}

fn which_claude() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(CLAUDE))
        .find(|candidate| candidate.is_file())
}

/// Arguments of a built command, for tests and for logging what was run.
pub fn argv(command: &Command) -> Vec<OsString> {
    std::iter::once(command.get_program().to_os_string())
        .chain(command.get_args().map(|arg| arg.to_os_string()))
        .collect()
}

/// Working directory a command will run in, if one was set.
pub fn command_cwd(command: &Command) -> Option<&Path> {
    command.get_current_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(command: &Command) -> Vec<String> {
        argv(command)
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn env_of(command: &Command) -> Vec<(String, Option<String>)> {
        command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    #[test]
    fn dispatches_a_named_session_in_a_directory() {
        let dispatch = Dispatch::new("audit the routes", "/work/app").named("route-audit");
        let command = dispatch.command();

        assert_eq!(
            args_of(&command),
            vec![
                "claude",
                "--bg",
                "--name",
                "route-audit",
                "--",
                "audit the routes"
            ]
        );
        assert_eq!(command_cwd(&command), Some(Path::new("/work/app")));
    }

    #[test]
    fn an_unnamed_dispatch_omits_the_flag_rather_than_passing_empty() {
        let command = Dispatch::new("do a thing", "/work").command();
        assert_eq!(
            args_of(&command),
            vec!["claude", "--bg", "--", "do a thing"]
        );
    }

    #[test]
    fn a_prompt_starting_with_a_dash_is_not_read_as_a_flag() {
        // Without the `--` terminator this prompt would be parsed as options.
        let command = Dispatch::new("--help me understand this repo", "/work").command();
        let args = args_of(&command);
        let terminator = args.iter().position(|arg| arg == "--").expect("terminated");
        assert_eq!(args[terminator + 1], "--help me understand this repo");
    }

    #[test]
    fn teams_are_enabled_per_dispatch_not_per_install() {
        let without = Dispatch::new("p", "/work").command();
        assert!(
            !env_of(&without).iter().any(|(key, _)| key == TEAMS_VAR),
            "teams must not leak into an ordinary dispatch"
        );

        let with = Dispatch::new("p", "/work").with_teams(true).command();
        assert_eq!(
            env_of(&with)
                .iter()
                .find(|(key, _)| key == TEAMS_VAR)
                .and_then(|(_, value)| value.clone())
                .as_deref(),
            Some("1")
        );
    }

    #[test]
    fn builds_the_verbs_the_cli_actually_has() {
        for (verb, expected) in [
            (Verb::Stop, "stop"),
            (Verb::Respawn, "respawn"),
            (Verb::Logs, "logs"),
            (Verb::Attach, "attach"),
            (Verb::Remove, "rm"),
        ] {
            let command = command(verb, "91214c57");
            assert_eq!(args_of(&command), vec!["claude", expected, "91214c57"]);
        }
    }

    #[test]
    fn only_attach_needs_a_terminal() {
        assert!(Verb::Attach.is_interactive());
        for verb in [Verb::Stop, Verb::Respawn, Verb::Logs, Verb::Remove] {
            assert!(!verb.is_interactive(), "{verb:?} is captured, not shown");
        }
        assert_eq!(attach_argv("abc123"), vec!["claude", "attach", "abc123"]);
    }

    #[test]
    fn a_running_session_can_be_talked_to_without_being_stopped() {
        for state in [FleetState::Working, FleetState::Blocked] {
            let actions = actions_for(&state);
            assert!(
                actions.contains(&SessionAction::Attach),
                "{state:?} must offer a non-destructive way in"
            );
            assert!(
                !actions.contains(&SessionAction::OpenInEcho),
                "{state:?} cannot be loaded over ACP while the daemon owns it"
            );
            assert!(actions.contains(&SessionAction::StopAndOpen));
        }
    }

    #[test]
    fn a_finished_session_is_opened_without_a_stop_prompt() {
        for state in [FleetState::Done, FleetState::Failed, FleetState::Stopped] {
            let actions = actions_for(&state);
            assert!(actions.contains(&SessionAction::OpenInEcho));
            assert!(
                !actions.contains(&SessionAction::Attach),
                "attach fails once the process has exited"
            );
            assert!(
                !actions.iter().any(|action| action.stops_the_session()),
                "there is nothing left to stop"
            );
        }
    }

    #[test]
    fn an_unknown_state_offers_only_what_is_safe_either_way() {
        let actions = actions_for(&FleetState::Unknown("hibernating".into()));
        assert!(actions.contains(&SessionAction::Read));
        assert!(
            !actions.contains(&SessionAction::Attach),
            "attaching to an exited session fails, and liveness is unknown"
        );
        // The stop inside this action is a no-op on a session that already
        // exited, so it is correct whether or not the process is alive.
        assert!(actions.contains(&SessionAction::StopAndOpen));
    }

    #[test]
    fn every_state_can_at_least_be_read() {
        for state in [
            FleetState::Working,
            FleetState::Blocked,
            FleetState::Done,
            FleetState::Failed,
            FleetState::Stopped,
            FleetState::Unknown(String::new()),
        ] {
            assert_eq!(
                actions_for(&state).first(),
                Some(&SessionAction::Read),
                "following a transcript is never withheld"
            );
        }
    }

    #[test]
    fn a_failing_command_reports_why_instead_of_returning_empty() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("echo trouble >&2; exit 3");
        let error = smol::block_on(run(command)).expect_err("a non-zero exit is an error");
        let message = format!("{error}");
        assert!(message.contains("trouble"), "stderr is surfaced: {message}");
    }

    #[test]
    fn a_missing_executable_is_an_error_not_a_panic() {
        let command = Command::new("claude-does-not-exist-anywhere");
        assert!(smol::block_on(run(command)).is_err());
    }

    #[test]
    fn a_successful_command_returns_its_stdout() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("echo ok");
        assert_eq!(smol::block_on(run(command)).expect("succeeds").trim(), "ok");
    }
}
