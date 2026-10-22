use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::config::schema::PromptConfig;

/// The silent-cd and silent-run channels are one file per shell session,
/// keyed by the `OMNIPTY_SESSION` value the app puts in each shell's
/// environment. A shared file would race: restoring a workspace with four
/// startup commands writes four targets at nearly the same instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Cd,
    Run,
}

impl Channel {
    fn dir_name(self) -> &'static str {
        match self {
            Channel::Cd => "cd",
            Channel::Run => "run",
        }
    }
}

/// `~/.cache/omnipty/<cd|run>/<session>` — where the generated shell handlers
/// look for their target, using `$OMNIPTY_SESSION`.
pub fn channel_path(channel: Channel, session: &str) -> Option<PathBuf> {
    Some(cache_dir()?.join(channel.dir_name()).join(session))
}

/// Hand a target to one shell session's widget. The file is consumed by the
/// handler that reads it, so nothing is left behind on the normal path.
pub fn write_channel(channel: Channel, session: &str, payload: &[u8]) -> bool {
    let Some(path) = channel_path(channel, session) else {
        return false;
    };
    let Some(dir) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    std::fs::write(&path, payload).is_ok()
}

/// Remove channel files a previous instance left behind (a shell that was
/// killed between the write and the read). Anything older than a minute is
/// stale; a live handoff completes in milliseconds. Called once at launch.
pub fn clean_stale_channels() {
    let Some(cache) = cache_dir() else { return };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    for channel in [Channel::Cd, Channel::Run] {
        let Ok(entries) = std::fs::read_dir(cache.join(channel.dir_name())) else {
            continue;
        };
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t < cutoff)
                .unwrap_or(true);
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    // The pre-0.5 shared files, if an old shell left one.
    let _ = std::fs::remove_file(cache.join("cd_target"));
    let _ = std::fs::remove_file(cache.join("run_target"));
}

fn cache_dir() -> Option<PathBuf> {
    Some(crate::paths::cache_dir())
}

/// Hand a path to a shell without putting it on the command line. Returns the
/// file's name, which is deliberately made of characters every shell leaves
/// alone, so the caller can embed it in a `$HOME/.cache/omnipty/edit/…`
/// reference that needs no quoting anywhere.
///
/// Unique per call: two panes opening files at once must not race, and the
/// consuming shell deletes the file as it reads it.
pub fn write_edit_target(path: &Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let dir = cache_dir()?.join("edit");
    std::fs::create_dir_all(&dir).ok()?;
    let name = format!(
        "{}-{}.path",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    // Raw bytes, not a lossy string: a path is not required to be UTF-8.
    std::fs::write(dir.join(&name), path.as_os_str().as_bytes()).ok()?;
    Some(name)
}

#[derive(Default, Clone)]
pub struct ShellIntegration {
    pub env: HashMap<String, String>,
    /// Replacement argv for the shell, when injection needs different flags.
    pub args_override: Option<Vec<String>>,
}

/// Write the generated init scripts and return env/args for the child shell.
/// Never touches the user's real dotfiles.
///
/// zsh: a ZDOTDIR shim whose rc files each source the user's counterpart with
/// ZDOTDIR temporarily restored, then layer our init.zsh on top (so our precmd
/// hook — and therefore PROMPT — wins over anything the user's config set).
///
/// bash: `--init-file init.bash` (replacing `-l`, which would make bash skip
/// the init file); the script emulates the login profile chain first.
pub fn setup(config: &Config, shell_program: &str) -> ShellIntegration {
    let mut integration = ShellIntegration::default();
    let style_prompt = config.prompt.enabled;
    if !config.shell.integration && !style_prompt {
        return integration;
    }
    let shell_name = Path::new(shell_program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let Some(cache) = cache_dir() else {
        return integration;
    };
    // Each failure below quietly leaves a shell with no integration: no
    // prompt styling, no OSC markers, no command log. Report the first one
    // per run. An `io::Error` from `fs` carries no path, so nothing
    // personal goes out.
    if let Err(e) = std::fs::create_dir_all(&cache) {
        crate::telemetry::report_once(
            "shell integration setup failed",
            format_args!("cache dir: {e}"),
        );
        return integration;
    }

    if shell_name.starts_with("zsh") {
        let zdotdir = cache.join("zdotdir");
        if let Err(e) = std::fs::create_dir_all(&zdotdir) {
            crate::telemetry::report_once(
                "shell integration setup failed",
                format_args!("zdotdir: {e}"),
            );
            return integration;
        }
        if let Err(e) = write_zsh_shim(
            &cache,
            &zdotdir,
            &config.prompt,
            style_prompt,
            config.commands.emit_cmdline,
        ) {
            crate::telemetry::report_once(
                "shell integration setup failed",
                format_args!("zsh: {e}"),
            );
            return integration;
        }
        if let Ok(user_zdotdir) = std::env::var("ZDOTDIR") {
            integration
                .env
                .insert("_OMNIPTY_USER_ZDOTDIR".into(), user_zdotdir);
        }
        integration
            .env
            .insert("ZDOTDIR".into(), zdotdir.to_string_lossy().to_string());
    } else if shell_name.starts_with("bash") {
        let init_path = cache.join("init.bash");
        if let Err(e) = std::fs::write(
            &init_path,
            super::generate_init_bash(&config.prompt, style_prompt, config.commands.emit_cmdline),
        ) {
            crate::telemetry::report_once(
                "shell integration setup failed",
                format_args!("bash: {e}"),
            );
            return integration;
        }
        let mut args: Vec<String> = config
            .shell
            .args
            .iter()
            .filter(|a| a.as_str() != "-l" && a.as_str() != "--login")
            .cloned()
            .collect();
        args.push("--init-file".into());
        args.push(init_path.to_string_lossy().to_string());
        integration.args_override = Some(args);
    }
    integration
}

fn write_zsh_shim(
    cache: &Path,
    zdotdir: &Path,
    prompt: &PromptConfig,
    style_prompt: bool,
    emit_cmdline: bool,
) -> std::io::Result<()> {
    let init_path = cache.join("init.zsh");
    std::fs::write(
        &init_path,
        super::generate_init(prompt, style_prompt, emit_cmdline),
    )?;

    let sandwich = |file: &str, extra: &str| -> String {
        format!(
            r#"# OmniPTY ZDOTDIR shim — sources your real {file}, never modifies it.
_omnipty_shim="$ZDOTDIR"
export ZDOTDIR="${{_OMNIPTY_USER_ZDOTDIR:-$HOME}}"
[[ -f "$ZDOTDIR/{file}" ]] && builtin source "$ZDOTDIR/{file}"
export _OMNIPTY_USER_ZDOTDIR="$ZDOTDIR"
export ZDOTDIR="$_omnipty_shim"
unset _omnipty_shim
{extra}"#
        )
    };

    std::fs::write(zdotdir.join(".zshenv"), sandwich(".zshenv", ""))?;
    std::fs::write(zdotdir.join(".zprofile"), sandwich(".zprofile", ""))?;
    let zshrc_tail = format!(
        "builtin source \"{}\"\n# Hand rc-file resolution back to the user's zsh for subshells.\nexport ZDOTDIR=\"${{_OMNIPTY_USER_ZDOTDIR:-$HOME}}\"\nunset _OMNIPTY_USER_ZDOTDIR\n",
        init_path.to_string_lossy()
    );
    std::fs::write(zdotdir.join(".zshrc"), sandwich(".zshrc", &zshrc_tail))?;
    Ok(())
}
