//! Command-line arguments, parsed once at launch.
//!
//! `omnipty [<directory>] [--app-id <id>] [--no-startup-commands] [-e <command> [args...]]`
//!
//! `-e` and `--app-id` are what `xdg-terminal-exec` (and so Omarchy's
//! Super+Return and TUI launchers) pass to a terminal, so supporting them
//! lets OmniPTY be a system's default terminal on Linux.

use std::path::PathBuf;
use std::sync::OnceLock;

pub const USAGE: &str = "\
usage: omnipty [<directory>] [options] [-e <command> [args...]]

  <directory>              open the first pane there (default: the current directory)
  -e <command> [args...]   run this instead of the shell in the first pane; the pane closes when it exits
  --app-id <id>            Wayland app-id / X11 class for the window (default: omnipty)
  --no-startup-commands    restore pinned workspaces' layout without running their startup commands
  -V, --version            print the version and exit
  -h, --help               print this and exit";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Cli {
    /// The explicit directory argument, if one was given.
    pub directory: Option<PathBuf>,
    /// `-e`: the program and its arguments, run in the first pane instead
    /// of the shell. Never empty when set.
    pub command: Option<Vec<String>>,
    /// `--app-id`: what window rules match on under Wayland / X11.
    pub app_id: Option<String>,
    /// `--no-startup-commands`: the out for a startup command that wedges
    /// the app, so it must not depend on any state the app writes.
    pub no_startup_commands: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Run(Cli),
    Version,
    Help,
    /// `--server`: run the session server instead of the app. Internal —
    /// the app starts it itself (`terminal::remote::connect_or_start`).
    Server,
}

/// Parse the arguments after the program name. Everything after `-e` is
/// the command, untouched, so `omnipty -e vim --help` runs vim's help rather
/// than printing ours.
pub fn parse<I>(args: I) -> Result<Parsed, String>
where
    I: IntoIterator<Item = String>,
{
    let mut cli = Cli::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            "--server" => return Ok(Parsed::Server),
            "--no-startup-commands" => cli.no_startup_commands = true,
            "-e" => {
                let command: Vec<String> = args.collect();
                if command.is_empty() {
                    return Err("-e needs a command to run".into());
                }
                cli.command = Some(command);
                break;
            }
            "--app-id" => {
                cli.app_id = Some(app_id_value(args.next())?);
            }
            _ if arg.starts_with("--app-id=") => {
                cli.app_id = Some(app_id_value(Some(arg["--app-id=".len()..].to_string()))?);
            }
            // Launch Services used to hand Finder-launched apps a process
            // serial number; harmless to keep ignoring.
            _ if arg.starts_with("-psn_") => {}
            _ if arg.starts_with('-') && arg.len() > 1 => {
                return Err(format!("unknown option: {arg}"));
            }
            _ => {
                if cli.directory.is_some() {
                    return Err(format!("unexpected argument: {arg}"));
                }
                cli.directory = Some(PathBuf::from(arg));
            }
        }
    }
    Ok(Parsed::Run(cli))
}

fn app_id_value(value: Option<String>) -> Result<String, String> {
    match value {
        Some(id) if !id.is_empty() => Ok(id),
        _ => Err("--app-id needs a value".into()),
    }
}

static CLI: OnceLock<Cli> = OnceLock::new();

/// Record the parsed arguments for the rest of the app. Only `main` calls
/// this; a second call is ignored.
pub fn install(cli: Cli) {
    CLI.set(cli).ok();
}

/// The arguments OmniPTY was launched with. Defaults (no flags) until `main`
/// installs them, which keeps tests and tools independent of the process
/// arguments.
pub fn cli() -> &'static Cli {
    CLI.get_or_init(Cli::default)
}

impl Cli {
    /// Where the first pane opens: the directory argument, else the
    /// process's working directory when it is a real one. Finder-launched
    /// apps inherit "/", a useless place to open a terminal, so that falls
    /// through to the caller's home fallback. A missing directory argument
    /// also falls through rather than failing the launch.
    pub fn working_directory(&self) -> Option<PathBuf> {
        if let Some(dir) = &self.directory {
            return Some(dir.clone()).filter(|d| d.is_dir());
        }
        std::env::current_dir()
            .ok()
            .filter(|d| d.is_dir() && d.parent().is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(args: &[&str]) -> Cli {
        match parse(args.iter().map(|s| s.to_string())) {
            Ok(Parsed::Run(cli)) => cli,
            other => panic!("expected a run, got {other:?}"),
        }
    }

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_arguments_means_defaults() {
        assert_eq!(parse_ok(&[]), Cli::default());
    }

    #[test]
    fn directory_and_flags_in_any_order() {
        let cli = parse_ok(&["--no-startup-commands", "/tmp"]);
        assert_eq!(cli.directory, Some(PathBuf::from("/tmp")));
        assert!(cli.no_startup_commands);
        let cli = parse_ok(&["/tmp", "--no-startup-commands"]);
        assert_eq!(cli.directory, Some(PathBuf::from("/tmp")));
        assert!(cli.no_startup_commands);
    }

    #[test]
    fn dash_e_takes_the_rest_verbatim() {
        let cli = parse_ok(&["-e", "vim", "--help", "-e", "/tmp"]);
        assert_eq!(cli.command, Some(strs(&["vim", "--help", "-e", "/tmp"])));
        assert_eq!(cli.directory, None);
    }

    #[test]
    fn dash_e_needs_a_command() {
        assert_eq!(
            parse(strs(&["-e"])),
            Err("-e needs a command to run".to_string())
        );
    }

    #[test]
    fn app_id_in_both_spellings() {
        assert_eq!(
            parse_ok(&["--app-id", "org.omarchy.btop"])
                .app_id
                .as_deref(),
            Some("org.omarchy.btop")
        );
        assert_eq!(
            parse_ok(&["--app-id=org.omarchy.btop"]).app_id.as_deref(),
            Some("org.omarchy.btop")
        );
        assert!(parse(strs(&["--app-id"])).is_err());
        assert!(parse(strs(&["--app-id="])).is_err());
    }

    #[test]
    fn xdg_terminal_exec_shape() {
        // What `xdg-terminal-exec --app-id=X -e cmd args` hands us.
        let cli = parse_ok(&["--app-id=org.omarchy.btop", "-e", "btop", "--utf-force"]);
        assert_eq!(cli.app_id.as_deref(), Some("org.omarchy.btop"));
        assert_eq!(cli.command, Some(strs(&["btop", "--utf-force"])));
    }

    #[test]
    fn help_and_version_win() {
        assert_eq!(parse(strs(&["/tmp", "-h"])), Ok(Parsed::Help));
        assert_eq!(parse(strs(&["--version"])), Ok(Parsed::Version));
        // ...unless they belong to the command.
        assert!(matches!(
            parse(strs(&["-e", "cat", "--version"])),
            Ok(Parsed::Run(_))
        ));
    }

    #[test]
    fn unknown_options_and_extra_positionals_are_errors() {
        assert_eq!(
            parse(strs(&["--bogus"])),
            Err("unknown option: --bogus".to_string())
        );
        assert_eq!(
            parse(strs(&["/a", "/b"])),
            Err("unexpected argument: /b".to_string())
        );
        // Legacy Launch Services noise is still tolerated.
        assert_eq!(parse_ok(&["-psn_0_12345"]), Cli::default());
    }

    #[test]
    fn working_directory_prefers_the_argument_then_cwd() {
        let explicit = Cli {
            directory: Some(std::env::temp_dir()),
            ..Cli::default()
        };
        assert_eq!(explicit.working_directory(), Some(std::env::temp_dir()));

        let missing = Cli {
            directory: Some(PathBuf::from("/definitely/not/here")),
            ..Cli::default()
        };
        assert_eq!(missing.working_directory(), None);

        let cwd = std::env::current_dir().unwrap();
        assert_eq!(Cli::default().working_directory(), Some(cwd));
    }
}
