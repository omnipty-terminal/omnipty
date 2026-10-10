pub mod integration;

use crate::config::schema::{CwdStyle, PromptConfig, SegmentConfig, SegmentKind};
use crate::config::theme::{hsla_to_rgb8, parse_hex};

/// "r;g;b" for SGR 38;2/48;2 parameters.
fn sgr_rgb(hex: Option<&str>, fallback: (u8, u8, u8)) -> String {
    let (r, g, b) = hex
        .and_then(parse_hex)
        .map(hsla_to_rgb8)
        .unwrap_or(fallback);
    format!("{r};{g};{b}")
}

const DEFAULT_FG: (u8, u8, u8) = (17, 17, 27);
const DEFAULT_BG: (u8, u8, u8) = (137, 180, 250);

/// Escape a string for inclusion inside zsh double quotes.
fn zsh_dq(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

fn segment_snippet(ix: usize, seg: &SegmentConfig) -> String {
    let fg = sgr_rgb(seg.fg.as_deref(), DEFAULT_FG);
    let bg = sgr_rgb(seg.bg.as_deref(), DEFAULT_BG);
    let bold = if seg.bold { "1" } else { "0" };
    let opts = &seg.options;
    match seg.kind {
        SegmentKind::Cwd => {
            let max_len = opts.max_len.unwrap_or(40).max(4);
            let style = opts.style.unwrap_or(CwdStyle::TruncateToRepo);
            let compute = match style {
                CwdStyle::Full => "local __cwd=\"${(%):-%~}\"\n".to_string(),
                CwdStyle::Basename => "local __cwd=\"${PWD:t}\"\n".to_string(),
                CwdStyle::TruncateToRepo => concat!(
                    "local __cwd=\"${(%):-%~}\"\n",
                    "local __root=\"\"\n(( __omnipty_git_ok )) && __root=$(command git rev-parse --show-toplevel 2>/dev/null)\n",
                    "if [[ -n \"$__root\" && \"$PWD\" == \"$__root\"* ]]; then __cwd=\"${__root:t}${PWD#$__root}\"; fi\n",
                )
                .to_string(),
            };
            format!(
                "{compute}(( ${{#__cwd}} > {max_len} )) && __cwd=\"…${{__cwd: -{max_len}}}\"\n\
                 __omnipty_seg \"$__cwd\" \"{fg}\" \"{bg}\" {bold}\n"
            )
        }
        SegmentKind::Git => {
            let dirty_bg = sgr_rgb(opts.dirty_bg.as_deref(), (249, 226, 175));
            let show_dirty = opts.show_dirty.unwrap_or(true);
            let ahead_behind = opts.ahead_behind.unwrap_or(true);
            let mut s = String::new();
            s.push_str(
                "local __br=\"\"\n(( __omnipty_git_ok )) && __br=$(command git symbolic-ref --short HEAD 2>/dev/null || command git rev-parse --short HEAD 2>/dev/null)\n",
            );
            s.push_str("if [[ -n \"$__br\" ]]; then\n");
            s.push_str(&format!("  local __gbg=\"{bg}\"\n"));
            // Literal UTF-8 bytes, not $'\ue0a0' — \u escapes fail with
            // "character not in range" when the shell runs under the C locale.
            s.push_str("  local __gtext=\"\u{e0a0} $__br\"\n");
            if show_dirty {
                s.push_str(
                    "  if [[ -n $(command git -c core.fsmonitor=false status --porcelain --untracked-files=no 2>/dev/null | command head -c1) ]]; then ",
                );
                s.push_str(&format!("__gbg=\"{dirty_bg}\"; fi\n"));
            }
            if ahead_behind {
                s.push_str("  local __ab=$(command git rev-list --left-right --count 'HEAD...@{upstream}' 2>/dev/null)\n");
                s.push_str("  if [[ -n \"$__ab\" ]]; then\n");
                s.push_str(
                    "    local __ahead=\"${__ab%%$'\\t'*}\" __behind=\"${__ab##*$'\\t'}\"\n",
                );
                s.push_str("    (( __ahead > 0 )) && __gtext+=\" ⇡$__ahead\"\n");
                s.push_str("    (( __behind > 0 )) && __gtext+=\" ⇣$__behind\"\n");
                s.push_str("  fi\n");
            }
            s.push_str(&format!(
                "  __omnipty_seg \"$__gtext\" \"{fg}\" \"$__gbg\" {bold}\n"
            ));
            s.push_str("fi\n");
            s
        }
        SegmentKind::ExitStatus => {
            let hide = opts.hide_on_success.unwrap_or(true);
            if hide {
                format!(
                    "if (( __omnipty_exit != 0 )); then __omnipty_seg \"✗ $__omnipty_exit\" \"{fg}\" \"{bg}\" {bold}; fi\n"
                )
            } else {
                format!("__omnipty_seg \"$__omnipty_exit\" \"{fg}\" \"{bg}\" {bold}\n")
            }
        }
        SegmentKind::Time => {
            let format = opts.format.clone().unwrap_or_else(|| "%H:%M".into());
            format!("__omnipty_seg \"${{(%):-%D{{{format}}}}}\" \"{fg}\" \"{bg}\" {bold}\n")
        }
        SegmentKind::User => {
            format!("__omnipty_seg \"${{(%):-%n}}\" \"{fg}\" \"{bg}\" {bold}\n")
        }
        SegmentKind::Host => {
            format!("__omnipty_seg \"${{(%):-%m}}\" \"{fg}\" \"{bg}\" {bold}\n")
        }
        SegmentKind::Duration => {
            format!(
                "if [[ -n \"$__omnipty_dur\" ]] && (( __omnipty_dur >= 2.0 )); then\n\
                 \x20 local __ds\n\
                 \x20 if (( __omnipty_dur >= 60 )); then __ds=\"$(( ${{__omnipty_dur%%.*}} / 60 ))m$(( ${{__omnipty_dur%%.*}} % 60 ))s\"; else __ds=$(printf '%.1fs' \"$__omnipty_dur\"); fi\n\
                 \x20 __omnipty_seg \"$__ds\" \"{fg}\" \"{bg}\" {bold}\n\
                 fi\n"
            )
        }
        SegmentKind::Text => {
            let text = zsh_dq(opts.text.as_deref().unwrap_or(""));
            format!("__omnipty_seg \"{text}\" \"{fg}\" \"{bg}\" {bold}\n")
        }
        SegmentKind::Env => match &opts.var {
            Some(var) if var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => format!(
                "[[ -n \"${{{var}}}\" ]] && __omnipty_seg \"${{{var}}}\" \"{fg}\" \"{bg}\" {bold} # segment {ix}\n"
            ),
            _ => String::new(),
        },
    }
}

/// Compile the `[prompt]` spec into a zsh init script: OSC 133 semantic prompt
/// markers plus a precmd that rebuilds PROMPT from powerline segments.
/// The C marker the silent-run widget emits for the command in `$c`. The
/// app already knows the text it handed over, so when `emit_cmdline` is off
/// it fills the log entry in itself and the shell sends nothing.
fn widget_command_start(emit_cmdline: bool) -> &'static str {
    if emit_cmdline {
        r#"printf '\033]133;C;cmdline=%s\033\\' "${c//[[:cntrl:]]/ }""#
    } else {
        r#"printf '\033]133;C\033\\'"#
    }
}

pub fn generate_init(prompt: &PromptConfig, style_prompt: bool, emit_cmdline: bool) -> String {
    let mut segments = String::new();
    for (ix, seg) in prompt.segments.iter().enumerate() {
        segments.push_str(&indent_lines(&segment_snippet(ix, seg), "  "));
    }
    let sep = zsh_dq(&prompt.separator);
    let end = zsh_dq(if prompt.end.is_empty() {
        &prompt.separator
    } else {
        &prompt.end
    });
    let newline = if prompt.newline_before_input {
        "p+=$'\\n'"
    } else {
        ":"
    };

    let style_prompt_flag = if style_prompt { 1 } else { 0 };
    // The typed line rides on the C marker so the app can name the command.
    // Control characters (a newline in a multi-line command, a stray ESC)
    // would end or corrupt the sequence, so they become spaces.
    let command_start = if emit_cmdline {
        r#"printf '\033]133;C;cmdline=%s\033\\' "${1//[[:cntrl:]]/ }""#
    } else {
        r#"printf '\033]133;C\033\\'"#
    };
    let widget_start = widget_command_start(emit_cmdline);
    format!(
        r#"# Generated by OmniPTY — do not edit; regenerated from config.toml on launch.
[[ -o interactive ]] || return
zmodload zsh/datetime 2>/dev/null
autoload -Uz add-zsh-hook

typeset -g __omnipty_style_prompt={style_prompt_flag}
typeset -g __omnipty_exit=0
__omnipty_git_ok=0
if command -v git >/dev/null 2>&1; then
  __omnipty_git_ok=1
  # Apple's /usr/bin/git is an installer shim until the CLT exist; running it
  # would pop a GUI dialog from inside the prompt. Check without invoking it.
  if [ "$(uname)" = Darwin ] && [ "$(command -v git)" = /usr/bin/git ] \
    && ! /usr/bin/xcode-select -p >/dev/null 2>&1; then
    __omnipty_git_ok=0
  fi
fi

typeset -g __omnipty_dur=""
typeset -g __omnipty_t0=""

__omnipty_preexec() {{
  __omnipty_t0=$EPOCHREALTIME
  {command_start}
}}

__omnipty_seg() {{
  __omnipty_texts+=("$1"); __omnipty_fgs+=("$2"); __omnipty_bgs+=("$3"); __omnipty_bolds+=("$4")
}}

__omnipty_precmd() {{
  # Capture $? before anything else or we report our own exit status.
  __omnipty_exit=$?
  if [[ -n "$__omnipty_t0" ]]; then
    __omnipty_dur=$(( EPOCHREALTIME - __omnipty_t0 ))
  else
    __omnipty_dur=""
  fi
  __omnipty_t0=""
  printf '\033]133;D;%s\033\\' "$__omnipty_exit"
  printf '\033]7;file://%s%s\033\\' "$HOST" "$PWD"

  local -a __omnipty_texts __omnipty_fgs __omnipty_bgs __omnipty_bolds
{segments}
  local sep="{sep}"
  local endc="{end}"
  local p=$'%{{\033]133;A\033\\%}}'
  local n=${{#__omnipty_texts}} i
  for (( i=1; i<=n; i++ )); do
    local b=""
    [[ "${{__omnipty_bolds[i]}}" == 1 ]] && b=$'\033[1m'
    # %{{...%}} zero-width markers keep zsh's printable-width math honest.
    p+="%{{"$'\033[38;2;'"${{__omnipty_fgs[i]}}m"$'\033[48;2;'"${{__omnipty_bgs[i]}}m${{b}}%}} ${{__omnipty_texts[i]//\%/%%}} "
    if (( i < n )); then
      p+="%{{"$'\033[0m\033[38;2;'"${{__omnipty_bgs[i]}}m"$'\033[48;2;'"${{__omnipty_bgs[i+1]}}m%}}${{sep}}"
    else
      p+="%{{"$'\033[0m\033[38;2;'"${{__omnipty_bgs[i]}}m%}}${{endc}}"
    fi
  done
  p+="%{{"$'\033[0m'"%}}"
  {newline}
  p+=" "
  p+=$'%{{\033]133;B\033\\%}}'
  if (( n > 0 && __omnipty_style_prompt )); then
    PROMPT="$p"
  else
    # Your own prompt (starship, p10k) is kept, so the A marker isn't in
    # PROMPT. Emit it here instead: prompt jumping and workspace startup
    # commands need to know a prompt is up.
    printf '\033]133;A\033\\'
  fi
}}

add-zsh-hook precmd __omnipty_precmd
add-zsh-hook preexec __omnipty_preexec

# Silent cd: the app writes a target path and sends the trigger sequence.
# Running it as a zle widget (rather than typing a command) means nothing is
# echoed and reset-prompt redraws the existing prompt with the new directory.
__omnipty_cd_widget() {{
  local f="${{HOME}}/.cache/omnipty/cd/${{OMNIPTY_SESSION:-none}}" d
  if [[ -r $f ]]; then
    d="$(<$f)"
    command rm -f -- "$f"
    [[ -d $d ]] && builtin cd -- "$d"
  fi
  zle reset-prompt
}}
zle -N __omnipty_cd_widget
bindkey '\e[9001~' __omnipty_cd_widget

# Silent run: same shape as the cd widget, for commands the app wants the
# shell to execute — opening a file in $EDITOR, which only the shell knows.
# The command runs where the user could see it (a full-screen editor takes
# the terminal, exactly as if they had typed it) but the command line itself
# is never echoed and never enters history. The target file is consumed so a
# stray trigger can't replay it.
__omnipty_run_widget() {{
  local f="${{HOME}}/.cache/omnipty/run/${{OMNIPTY_SESSION:-none}}" c="" rc=0
  if [[ -r $f ]]; then
    c="$(<$f)"
    command rm -f -- "$f"
  fi
  if [[ -n $c ]]; then
    # Let zle know the display is about to be clobbered, then hand the
    # terminal to the command. </dev/tty matters: inside a widget stdin is
    # not the terminal, and vim refuses to read a non-tty stdin.
    zle -I
    # preexec/precmd don't fire for a widget, so emit the command markers
    # here: the app logs the run and learns how it ended (workspace
    # startup commands key their on-exit behaviour off this).
    {widget_start}
    eval "$c" </dev/tty
    rc=$?
    printf '\033]133;D;%s\033\\' "$rc"
    # reset-prompt redraws one line above the cursor, which would eat the
    # command's last line of output. Give it a spare line to reclaim.
    print
  fi
  zle reset-prompt
}}
zle -N __omnipty_run_widget
bindkey '\e[9002~' __omnipty_run_widget
"#
    )
}

fn bash_segment_snippet(seg: &SegmentConfig) -> String {
    let fg = sgr_rgb(seg.fg.as_deref(), DEFAULT_FG);
    let bg = sgr_rgb(seg.bg.as_deref(), DEFAULT_BG);
    let bold = if seg.bold { "1" } else { "0" };
    let opts = &seg.options;
    match seg.kind {
        SegmentKind::Cwd => {
            let max_len = opts.max_len.unwrap_or(40).max(4);
            let style = opts.style.unwrap_or(CwdStyle::TruncateToRepo);
            let compute = match style {
                CwdStyle::Full => "local __cwd=\"${PWD/#$HOME/\\~}\"\n".to_string(),
                CwdStyle::Basename => "local __cwd=\"${PWD##*/}\"\n".to_string(),
                CwdStyle::TruncateToRepo => concat!(
                    "local __cwd=\"${PWD/#$HOME/\\~}\"\n",
                    "local __root=\"\"\n(( __omnipty_git_ok )) && __root=$(command git rev-parse --show-toplevel 2>/dev/null)\n",
                    "if [[ -n \"$__root\" && \"$PWD\" == \"$__root\"* ]]; then __cwd=\"${__root##*/}${PWD#$__root}\"; fi\n",
                )
                .to_string(),
            };
            format!(
                "{compute}(( ${{#__cwd}} > {max_len} )) && __cwd=\"…${{__cwd: -{max_len}}}\"\n\
                 __omnipty_seg \"$__cwd\" \"{fg}\" \"{bg}\" {bold}\n"
            )
        }
        SegmentKind::Git => {
            let dirty_bg = sgr_rgb(opts.dirty_bg.as_deref(), (249, 226, 175));
            let show_dirty = opts.show_dirty.unwrap_or(true);
            let ahead_behind = opts.ahead_behind.unwrap_or(true);
            let mut s = String::new();
            s.push_str(
                "local __br=\"\"\n(( __omnipty_git_ok )) && __br=$(command git symbolic-ref --short HEAD 2>/dev/null || command git rev-parse --short HEAD 2>/dev/null)\n",
            );
            s.push_str("if [[ -n \"$__br\" ]]; then\n");
            s.push_str(&format!("  local __gbg=\"{bg}\"\n"));
            s.push_str("  local __gtext=\"\u{e0a0} $__br\"\n");
            if show_dirty {
                s.push_str(
                    "  if [[ -n $(command git -c core.fsmonitor=false status --porcelain --untracked-files=no 2>/dev/null | command head -c1) ]]; then ",
                );
                s.push_str(&format!("__gbg=\"{dirty_bg}\"; fi\n"));
            }
            if ahead_behind {
                s.push_str("  local __ab=$(command git rev-list --left-right --count 'HEAD...@{upstream}' 2>/dev/null)\n");
                s.push_str("  if [[ -n \"$__ab\" ]]; then\n");
                s.push_str(
                    "    local __ahead=\"${__ab%%$'\\t'*}\" __behind=\"${__ab##*$'\\t'}\"\n",
                );
                s.push_str("    (( __ahead > 0 )) && __gtext+=\" ⇡$__ahead\"\n");
                s.push_str("    (( __behind > 0 )) && __gtext+=\" ⇣$__behind\"\n");
                s.push_str("  fi\n");
            }
            s.push_str(&format!(
                "  __omnipty_seg \"$__gtext\" \"{fg}\" \"$__gbg\" {bold}\n"
            ));
            s.push_str("fi\n");
            s
        }
        SegmentKind::ExitStatus => {
            let hide = opts.hide_on_success.unwrap_or(true);
            if hide {
                format!(
                    "if (( __omnipty_exit != 0 )); then __omnipty_seg \"✗ $__omnipty_exit\" \"{fg}\" \"{bg}\" {bold}; fi\n"
                )
            } else {
                format!("__omnipty_seg \"$__omnipty_exit\" \"{fg}\" \"{bg}\" {bold}\n")
            }
        }
        SegmentKind::Time => {
            let format = zsh_dq(&opts.format.clone().unwrap_or_else(|| "%H:%M".into()));
            format!("__omnipty_seg \"$(command date +\"{format}\")\" \"{fg}\" \"{bg}\" {bold}\n")
        }
        SegmentKind::User => format!("__omnipty_seg \"$USER\" \"{fg}\" \"{bg}\" {bold}\n"),
        SegmentKind::Host => {
            format!("__omnipty_seg \"${{HOSTNAME%%.*}}\" \"{fg}\" \"{bg}\" {bold}\n")
        }
        SegmentKind::Duration => {
            format!(
                "if [[ -n \"$__omnipty_dur\" ]] && (( __omnipty_dur >= 2 )); then\n\
                 \x20 local __ds=\"${{__omnipty_dur}}s\"\n\
                 \x20 (( __omnipty_dur >= 60 )) && __ds=\"$(( __omnipty_dur / 60 ))m$(( __omnipty_dur % 60 ))s\"\n\
                 \x20 __omnipty_seg \"$__ds\" \"{fg}\" \"{bg}\" {bold}\n\
                 fi\n"
            )
        }
        SegmentKind::Text => {
            let text = zsh_dq(opts.text.as_deref().unwrap_or(""));
            format!("__omnipty_seg \"{text}\" \"{fg}\" \"{bg}\" {bold}\n")
        }
        SegmentKind::Env => match &opts.var {
            Some(var) if var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => format!(
                "[[ -n \"${{{var}}}\" ]] && __omnipty_seg \"${{{var}}}\" \"{fg}\" \"{bg}\" {bold}\n"
            ),
            _ => String::new(),
        },
    }
}

/// Bash flavor of the init script, injected via `--init-file`. Emulates a
/// login shell (the flag is ignored by real login shells, so the caller strips
/// `-l`), then installs a PROMPT_COMMAND that rebuilds PS1 per prompt.
/// `\x01`/`\x02` are readline's zero-width markers (what `\[`/`\]` compile to).
pub fn generate_init_bash(prompt: &PromptConfig, style_prompt: bool, emit_cmdline: bool) -> String {
    let mut segments = String::new();
    for seg in &prompt.segments {
        segments.push_str(&indent_lines(&bash_segment_snippet(seg), "  "));
    }
    let sep = zsh_dq(&prompt.separator);
    let end = zsh_dq(if prompt.end.is_empty() {
        &prompt.separator
    } else {
        &prompt.end
    });
    let newline = if prompt.newline_before_input {
        "p+=$'\\n'"
    } else {
        ":"
    };

    let style_prompt_flag = if style_prompt { 1 } else { 0 };
    // $BASH_COMMAND is only the first simple command of a line; the history
    // entry (added before execution) is the whole line, the bash-preexec
    // trick. But history can lie — HISTCONTROL drops duplicates, and shared
    // history files let other shells' lines in — so the entry is only
    // trusted when it contains the command bash says it's about to run.
    // Strip the leading number and any control characters.
    // Printed with __omnipty_mark, not printf: see its definition below.
    let command_start = if emit_cmdline {
        r#"local __c; __c=$(HISTTIMEFORMAT= builtin history 1 2>/dev/null)
    __c="${__c#"${__c%%[![:space:]]*}"}"; __c="${__c#*[[:space:]]}"; __c="${__c#"${__c%%[![:space:]]*}"}"
    [[ "$__c" == *"$BASH_COMMAND"* ]] || __c="$BASH_COMMAND"
    __omnipty_mark '\033]133;C;cmdline=%s\033\\' "${__c//[[:cntrl:]]/ }""#
    } else {
        r#"__omnipty_mark '\033]133;C\033\\'"#
    };
    let widget_start = widget_command_start(emit_cmdline);
    format!(
        r#"# Generated by OmniPTY — do not edit; regenerated from config.toml on launch.
# Login-shell emulation: --init-file replaced -l, so run the profile chain.
if [[ -f /etc/profile ]]; then source /etc/profile; fi
if [[ -f "$HOME/.bash_profile" ]]; then source "$HOME/.bash_profile"
elif [[ -f "$HOME/.bash_login" ]]; then source "$HOME/.bash_login"
elif [[ -f "$HOME/.profile" ]]; then source "$HOME/.profile"
elif [[ -f "$HOME/.bashrc" ]]; then source "$HOME/.bashrc"
fi

[[ $- == *i* ]] || return 0

# The terminal settings bash runs commands with, taken before readline first
# switches the terminal to its own (no echo, raw input). The silent-run
# handler puts these back for the command it runs; see __omnipty_run_widget.
__omnipty_tty=$(stty -g 2>/dev/null)

__omnipty_style_prompt={style_prompt_flag}
__omnipty_cd_erase=0
__omnipty_git_ok=0
if command -v git >/dev/null 2>&1; then
  __omnipty_git_ok=1
  # Apple's /usr/bin/git is an installer shim until the CLT exist; running it
  # would pop a GUI dialog from inside the prompt. Check without invoking it.
  if [ "$(uname)" = Darwin ] && [ "$(command -v git)" = /usr/bin/git ] \
    && ! /usr/bin/xcode-select -p >/dev/null 2>&1; then
    __omnipty_git_ok=0
  fi
fi

# 0 until the first prompt: the rest of this file runs under the DEBUG trap
# too, and none of it is a command the user typed.
__omnipty_at_prompt=0
__omnipty_t0=""
__omnipty_dur=""
__omnipty_exit=0
# bash 5.1+ lets PROMPT_COMMAND be an array, and Arch's bashrc, starship,
# and zoxide all append to it. Keep every element (a plain string is a
# one-element array here), then drop the variable before ours replaces it:
# an element left behind would run at top level, where the DEBUG trap
# would log it as a command that never finishes. Skip our own hook, so
# sourcing this file twice cannot make it call itself.
__omnipty_original_prompt_commands=()
for __omnipty_pc in "${{PROMPT_COMMAND[@]}}"; do
  [[ "$__omnipty_pc" == *__omnipty_prompt_command* ]] && continue
  __omnipty_original_prompt_commands+=("$__omnipty_pc")
done
unset __omnipty_pc PROMPT_COMMAND

# Where the DEBUG trap's C marker goes. bash runs the trap after a compound
# command's redirections are in place, so during `{{ ...; }} > file` (or a
# loop redirected to a file) stdout is already that file, and a plain printf
# put the marker at the top of it. /dev/tty is still the terminal. With no
# controlling terminal (bash driven over pipes) there's nothing to protect,
# so the marker stays on stdout.
if {{ : >/dev/tty; }} 2>/dev/null; then
  __omnipty_mark() {{ builtin printf "$@" >/dev/tty; }}
else
  __omnipty_mark() {{ builtin printf "$@"; }}
fi

# Note: this installs a DEBUG trap (the bash-preexec pattern) for OSC 133;C
# and command timing; a pre-existing DEBUG trap would be replaced.
__omnipty_debug_trap() {{
  [[ -n "$COMP_LINE" ]] && return
  # Our own prompt hook and the cd/run handlers emit their own markers.
  [[ "$BASH_COMMAND" == __omnipty_* ]] && return
  if (( __omnipty_at_prompt )); then
    __omnipty_at_prompt=0
    __omnipty_t0=$SECONDS
    {command_start}
  fi
}}
trap '__omnipty_debug_trap' DEBUG

__omnipty_seg() {{
  __omnipty_texts+=("$1"); __omnipty_fgs+=("$2"); __omnipty_bgs+=("$3"); __omnipty_bolds+=("$4")
}}

__omnipty_prompt_command() {{
  # Capture $? before anything else or we report our own exit status.
  local __omnipty_status=$?
  # Re-sourcing ~/.bashrc re-runs `starship init bash`, which finds this
  # hook in PROMPT_COMMAND, stashes it in STARSHIP_PROMPT_COMMAND and evals
  # it from starship_precmd — while we eval starship_precmd from the hooks
  # captured above. Left alone, each calls the other until bash overflows
  # its stack and segfaults. FUNCNAME lists every function still running;
  # if we are already on it, this is the inner call: do nothing and let the
  # outer one finish the prompt.
  local __omnipty_fn
  for __omnipty_fn in "${{FUNCNAME[@]:1}}"; do
    [[ "$__omnipty_fn" == __omnipty_prompt_command ]] && return 0
  done
  __omnipty_exit=$__omnipty_status
  if [[ -n "$__omnipty_t0" ]]; then __omnipty_dur=$(( SECONDS - __omnipty_t0 )); else __omnipty_dur=""; fi
  __omnipty_t0=""
  __omnipty_at_prompt=1
  # Triggered by a silent cd: walk back over the prompt we just left behind
  # and clear it, so this prompt replaces it rather than stacking below.
  if (( __omnipty_cd_erase > 0 )); then
    printf '\033[%dA\033[J' "$__omnipty_cd_erase"
    __omnipty_cd_erase=0
  fi
  printf '\033]133;D;%s\033\\' "$__omnipty_exit"
  printf '\033]7;file://%s%s\033\\' "${{HOSTNAME:-localhost}}" "$PWD"
  local __omnipty_pc
  for __omnipty_pc in "${{__omnipty_original_prompt_commands[@]}}"; do
    [[ -n "$__omnipty_pc" ]] || continue
    # A hook that is itself running us (starship_precmd after a bashrc
    # re-source, see above) is already on the stack: running it again here
    # would draw its prompt twice. Skip it; it finishes after we return.
    [[ " ${{FUNCNAME[*]}} " == *" ${{__omnipty_pc%%[[:space:];]*}} "* ]] && continue
    # Each hook sees the real exit status, as it would without us.
    ( exit "$__omnipty_exit" )
    eval "$__omnipty_pc"
  done

  local __omnipty_texts=() __omnipty_fgs=() __omnipty_bgs=() __omnipty_bolds=()
{segments}
  local sep="{sep}"
  local endc="{end}"
  local p=$'\001\033]133;A\033\\\002'
  local n=${{#__omnipty_texts[@]}} i
  for (( i=0; i<n; i++ )); do
    local b=""
    [[ "${{__omnipty_bolds[i]}}" == 1 ]] && b=$'\033[1m'
    local t="${{__omnipty_texts[i]}}"
    t="${{t//\\/\\\\}}"; t="${{t//\$/\\\$}}"; t="${{t//\`/\\\`}}"
    p+=$'\001\033[38;2;'"${{__omnipty_fgs[i]}}m"$'\033[48;2;'"${{__omnipty_bgs[i]}}m$b"$'\002'" $t "
    if (( i+1 < n )); then
      p+=$'\001\033[0m\033[38;2;'"${{__omnipty_bgs[i]}}m"$'\033[48;2;'"${{__omnipty_bgs[i+1]}}m"$'\002'"$sep"
    else
      p+=$'\001\033[0m\033[38;2;'"${{__omnipty_bgs[i]}}m"$'\002'"$endc"
    fi
  done
  p+=$'\001\033[0m\002'
  {newline}
  p+=" "
  p+=$'\001\033]133;B\033\\\002'
  if (( n > 0 && __omnipty_style_prompt )); then
    PS1="$p"
  else
    # Same as zsh: with your own prompt kept, the A marker goes out here.
    printf '\033]133;A\033\\'
  fi
}}
PROMPT_COMMAND="__omnipty_prompt_command"

# Silent cd. bash has no `zle reset-prompt`, so the app follows the trigger
# with an empty line to get a freshly expanded prompt. Record how tall the
# outgoing prompt is; the next __omnipty_prompt_command erases it so the new
# prompt lands in place instead of stacking up.
__omnipty_cd_widget() {{
  local f="${{HOME}}/.cache/omnipty/cd/${{OMNIPTY_SESSION:-none}}" d
  if [[ -r $f ]]; then
    d="$(<$f)"
    command rm -f -- "$f"
    [[ -d $d ]] && builtin cd -- "$d"
  fi
  if (( BASH_VERSINFO[0] >= 5 )); then
    # Count the outgoing prompt's lines so the erase covers all of them.
    # ${{PS1@P}} needs bash 4.4+; older bash assumes a one-line prompt.
    local __p="${{PS1@P}}" __nl
    __nl="${{__p//[!$'\n']/}}"
    __omnipty_cd_erase=$(( ${{#__nl}} + 1 ))
  else
    __omnipty_cd_erase=1
  fi
}}
# Silent run: the app writes a command and sends the trigger. readline
# redraws the prompt line once the handler returns, so a full-screen editor
# leaves the terminal the way it found it — and the command line is never
# echoed or added to history. The target file is consumed so a stray trigger
# cannot replay it.
__omnipty_run_widget() {{
  local f="${{HOME}}/.cache/omnipty/run/${{OMNIPTY_SESSION:-none}}" c="" rc=0
  if [[ -r $f ]]; then
    c="$(<$f)"
    command rm -f -- "$f"
  fi
  if [[ -n $c ]]; then
    # The DEBUG trap skips our handlers, so emit the command markers here;
    # the app logs the run and learns its exit status.
    {widget_start}
    # A bind -x handler still has readline's terminal settings: no echo,
    # raw input. zsh's `zle -I` restores the shell's; bash has no
    # equivalent, so do it here. Otherwise an interactive command runs
    # without echo, and `ssh -t` copies that onto the remote terminal,
    # whose shell then never shows what you type. readline's settings go
    # back afterwards so the prompt keeps working.
    local __omnipty_rl_tty=""
    if [[ -n $__omnipty_tty ]]; then
      __omnipty_rl_tty=$(stty -g 2>/dev/null </dev/tty)
      stty "$__omnipty_tty" 2>/dev/null </dev/tty
    fi
    # </dev/tty for the same reason as zsh: a bind -x handler does not
    # inherit the terminal on stdin, and editors refuse to run without it.
    eval "$c" </dev/tty
    rc=$?
    [[ -n $__omnipty_rl_tty ]] && stty "$__omnipty_rl_tty" 2>/dev/null </dev/tty
    printf '\033]133;D;%s\033\\' "$rc"
  fi
  # readline redraws the prompt without running PROMPT_COMMAND; the next
  # typed line must still get its C marker.
  __omnipty_at_prompt=1
  return 0
}}
if (( BASH_VERSINFO[0] > 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 3) )); then
  bind -x '"\e[9001~": __omnipty_cd_widget' 2>/dev/null
  bind -x '"\e[9002~": __omnipty_run_widget' 2>/dev/null
else
  # bash < 4.3 cannot `bind -x` a multi-character sequence — invoking it dies
  # with "bash_execute_unix_command: cannot find keymap for command" (Apple's
  # /bin/bash 3.2 included). Trampoline through a two-key binding instead,
  # the same trick fzf uses.
  bind -x '"\C-x\C-a": __omnipty_cd_widget' 2>/dev/null
  bind '"\e[9001~": "\C-x\C-a"' 2>/dev/null
  bind -x '"\C-x\C-b": __omnipty_run_widget' 2>/dev/null
  bind '"\e[9002~": "\C-x\C-b"' 2>/dev/null
fi
"#
    )
}

fn indent_lines(s: &str, prefix: &str) -> String {
    s.lines()
        .map(|l| {
            if l.is_empty() {
                l.to_string()
            } else {
                format!("{prefix}{l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::PromptConfig;

    #[test]
    fn generates_osc_markers_and_hooks() {
        let script = generate_init(&PromptConfig::default(), true, true);
        assert!(script.contains("133;A"));
        assert!(script.contains("133;B"));
        assert!(script.contains("133;C;cmdline="));
        assert!(!generate_init(&PromptConfig::default(), true, false).contains("cmdline="));
        assert!(script.contains("133;D"));
        assert!(script.contains("add-zsh-hook precmd __omnipty_precmd"));
        // Default config has cwd + git + exit segments.
        assert!(script.contains("__cwd"));
        assert!(script.contains("__br"));
        assert!(script.contains("__omnipty_exit != 0"));
    }

    #[test]
    fn generated_scripts_pass_shell_syntax_check() {
        let dir = std::env::temp_dir().join("omnipty-prompt-syntax-test");
        std::fs::create_dir_all(&dir).unwrap();
        let zsh_path = dir.join("init.zsh");
        let bash_path = dir.join("init.bash");
        std::fs::write(
            &zsh_path,
            generate_init(&PromptConfig::default(), true, true),
        )
        .unwrap();
        std::fs::write(
            &bash_path,
            generate_init_bash(&PromptConfig::default(), true, true),
        )
        .unwrap();
        for (shell, path) in [("/bin/zsh", &zsh_path), ("/bin/bash", &bash_path)] {
            if !std::path::Path::new(shell).exists() {
                continue;
            }
            let status = std::process::Command::new(shell)
                .arg("-n")
                .arg(path)
                .status()
                .unwrap();
            assert!(status.success(), "{shell} rejected generated script");
        }
    }

    /// M8, headless: a real zsh under the ZDOTDIR shim renders the segmented
    /// prompt (with powerline separators) into the terminal grid.
    #[test]
    fn zsh_prompt_end_to_end() {
        use crate::terminal::session::{SessionOptions, TermSize, TerminalSession};
        use std::time::{Duration, Instant};

        if !std::path::Path::new("/bin/zsh").exists() {
            return;
        }
        let config = crate::config::Config::default();
        let integration = crate::prompt::integration::setup(&config, "/bin/zsh");
        let Some(zdotdir) = integration.env.get("ZDOTDIR").cloned() else {
            return; // cache dir unavailable in this environment
        };
        assert!(std::path::Path::new(&zdotdir).join(".zshrc").exists());

        let size = TermSize {
            columns: 120,
            screen_lines: 24,
            cell_width: 8.0,
            cell_height: 16.0,
            scale: 1.0,
        };
        let options = SessionOptions {
            program: "/bin/zsh".into(),
            args: vec![],
            working_directory: std::env::current_dir().ok(),
            scrollback: 100,
            env: {
                let mut e = integration.env;
                e.insert("HISTFILE".into(), "/dev/null".into());
                e
            },
            images: true,
        };
        let (session, _rx) = TerminalSession::spawn(options, size).expect("spawn zsh");

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            std::thread::sleep(Duration::from_millis(150));
            let text: String = {
                let term = session.term.lock();
                term.renderable_content()
                    .display_iter
                    .map(|i| i.cell.c)
                    .collect()
            };
            // Default segments end with the powerline arrow; cwd segment shows
            // the repo basename.
            if text.contains('\u{e0b0}') && text.contains("omnipty") {
                break;
            }
            if Instant::now() > deadline {
                panic!("prompt never rendered; grid: {text}");
            }
        }
    }

    #[test]
    fn env_segment_rejects_injection() {
        use crate::config::schema::{SegmentConfig, SegmentKind, SegmentOptions};
        let seg = SegmentConfig {
            kind: SegmentKind::Env,
            options: SegmentOptions {
                var: Some("$(rm -rf /)".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(segment_snippet(0, &seg), "");
    }
}

#[cfg(test)]
mod cd_tests {
    use crate::config::Config;
    use crate::terminal::session::{SessionOptions, TermSize, TerminalSession};
    use std::time::{Duration, Instant};

    fn grid(session: &TerminalSession) -> String {
        let term = session.term.lock();
        let mut out = String::new();
        let mut line = i32::MIN;
        for ix in term.renderable_content().display_iter {
            if ix.point.line.0 != line {
                out.push('\n');
                line = ix.point.line.0;
            }
            out.push(ix.cell.c);
        }
        out
    }

    /// A forced-interactive bash driven over pipes, started in its own session
    /// so it has no controlling terminal. Without `setsid`, `bash -i` opens
    /// /dev/tty and takes the terminal's foreground away from the test
    /// harness; a second one starting meanwhile sends SIGTTIN to the whole
    /// `cargo test` process group, and the run halts mid-way with the shell
    /// reporting it as stopped.
    fn detached_interactive_bash(
        bash: &str,
        init: &std::path::Path,
        home: &std::path::Path,
    ) -> std::process::Child {
        use std::os::unix::process::CommandExt as _;
        use std::process::{Command, Stdio};
        let mut cmd = Command::new(bash);
        cmd.arg("--init-file")
            .arg(init)
            .arg("-i")
            .env("HOME", home)
            .env("TERM", "xterm-256color")
            .env("HISTFILE", "/dev/null")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: setsid is async-signal-safe and touches no shared state.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        cmd.spawn().unwrap()
    }

    /// The tree's `c` must change the shell's directory with no `cd` echoed.
    /// Runs against every bash on the machine — Apple's /bin/bash 3.2 has a
    /// broken multi-char `bind -x` that needs the trampoline path, while a
    /// Homebrew bash 5 exercises the direct binding.
    /// Arch's bashrc, starship, and zoxide each append to PROMPT_COMMAND,
    /// which bash 5.1+ keeps as an array. Every hook must still run, and
    /// none may be logged as a command: only the typed line gets a C marker.
    #[test]
    fn bash_prompt_command_array_hooks_run_but_are_not_logged() {
        let bash = "/bin/bash";
        if !std::path::Path::new(bash).exists() {
            return;
        }
        let home = std::env::temp_dir().join(format!("omnipty-pc-array-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(".bashrc"),
            "PROMPT_COMMAND=('echo hook-one' 'echo hook-two')\n",
        )
        .unwrap();
        let init = home.join("init.bash");
        std::fs::write(
            &init,
            crate::prompt::generate_init_bash(&crate::prompt::PromptConfig::default(), false, true),
        )
        .unwrap();

        use std::io::Write as _;
        let mut child = detached_interactive_bash(bash, &init, &home);
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"echo user-command\nexit\n")
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = std::fs::remove_dir_all(&home);

        assert!(
            text.contains("hook-one") && text.contains("hook-two"),
            "{text}"
        );
        // Two typed lines, two C markers — none for the hooks, and none
        // for the init file itself before the first prompt.
        let starts: Vec<&str> = text.matches("133;C").collect();
        assert_eq!(starts.len(), 2, "one C marker per typed line:\n{text}");
        assert!(text.contains("cmdline=echo user-command"), "{text}");
        assert!(text.contains("cmdline=exit"), "{text}");
        assert!(!text.contains("cmdline=echo hook"), "{text}");
        let first_c = text.find("133;C").unwrap();
        let first_a = text.find("133;A").unwrap();
        assert!(first_a < first_c, "startup logged as a command:\n{text}");
    }

    /// Re-sourcing ~/.bashrc re-runs `starship init bash`, which finds our
    /// hook in PROMPT_COMMAND, stashes it in STARSHIP_PROMPT_COMMAND and
    /// evals it from starship_precmd — while we eval starship_precmd from
    /// the hooks captured at startup. Each then calls the other until bash
    /// overflows its stack and segfaults. The fake precmd here mirrors
    /// starship's install logic exactly; the shell must survive the next
    /// prompt and both hooks must still run once.
    #[test]
    fn bash_prompt_hook_survives_bashrc_resource() {
        let bash = "/bin/bash";
        if !std::path::Path::new(bash).exists() {
            return;
        }
        let home = std::env::temp_dir().join(format!("omnipty-pc-resource-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(".bashrc"),
            concat!(
                "fake_precmd() { echo fake-precmd; eval \"$FAKE_PROMPT_COMMAND\"; }\n",
                "if [[ -z \"${PROMPT_COMMAND-}\" ]]; then\n",
                "  PROMPT_COMMAND=fake_precmd\n",
                "elif [[ \"$PROMPT_COMMAND\" != *fake_precmd* ]]; then\n",
                "  FAKE_PROMPT_COMMAND=\"$PROMPT_COMMAND\"\n",
                "  PROMPT_COMMAND=fake_precmd\n",
                "fi\n",
            ),
        )
        .unwrap();
        let init = home.join("init.bash");
        std::fs::write(
            &init,
            crate::prompt::generate_init_bash(&crate::prompt::PromptConfig::default(), false, true),
        )
        .unwrap();

        use std::io::Write as _;
        let mut child = detached_interactive_bash(bash, &init, &home);
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"source ~/.bashrc\necho still-alive\nexit\n")
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = std::fs::remove_dir_all(&home);

        assert!(
            out.status.success(),
            "bash did not exit cleanly after re-sourcing .bashrc: {:?}\n{text}",
            out.status
        );
        assert!(text.contains("still-alive"), "{text}");
        // Three prompts (after init, after source, after echo): the wrapped
        // hook runs once per prompt, never recursively.
        let precmds = text.matches("fake-precmd").count();
        assert_eq!(precmds, 3, "fake precmd ran {precmds} times:\n{text}");
        let ds: Vec<&str> = text.matches("133;D").collect();
        assert_eq!(ds.len(), 3, "one D marker per prompt:\n{text}");
    }

    #[test]
    fn silent_cd_in_bash() {
        for bash in ["/opt/homebrew/bin/bash", "/bin/bash"] {
            if std::path::Path::new(bash).exists() {
                silent_cd_scenario(bash);
            }
        }
    }

    fn silent_cd_scenario(bash: &str) {
        let config = Config::default();
        let integration = crate::prompt::integration::setup(&config, bash);
        let Some(args) = integration.args_override.clone() else {
            return;
        };

        let size = TermSize {
            columns: 100,
            screen_lines: 24,
            cell_width: 8.0,
            cell_height: 16.0,
            scale: 1.0,
        };
        let options = SessionOptions {
            program: bash.to_string(),
            args,
            working_directory: Some(env!("CARGO_MANIFEST_DIR").into()),
            scrollback: 100,
            env: {
                let mut e = integration.env.clone();
                e.insert("HISTFILE".into(), "/dev/null".into());
                e
            },
            images: true,
        };
        let (session, _rx) = TerminalSession::spawn(options, size).expect("spawn bash");
        std::thread::sleep(Duration::from_millis(1500)); // let rc files load

        use crate::prompt::integration::{Channel, write_channel};
        assert!(write_channel(Channel::Cd, session.id(), b"/usr/local"));
        let before = grid(&session);
        // Mirrors TerminalPane::request_cd for bash: trigger + empty line.
        session.write_input(b"\x1b[9001~\r".to_vec());

        // Wait for the shell to move *and* for a refreshed prompt to render.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            std::thread::sleep(Duration::from_millis(200));
            let moved = session
                .foreground_cwd()
                .is_some_and(|p| p == std::path::Path::new("/usr/local"));
            let last_line = grid(&session)
                .lines()
                .rfind(|l| !l.trim().is_empty())
                .unwrap_or("")
                .to_string();
            if moved && last_line.contains("local") {
                break;
            }
            if Instant::now() > deadline {
                panic!(
                    "moved={moved}, prompt never refreshed; last line: {last_line:?}\ngrid:\n{}",
                    grid(&session)
                );
            }
        }
        let after = grid(&session);
        let new_text = after.replace(&before, "");
        assert!(
            !new_text.contains("cd /usr/local") && !new_text.contains("cd '"),
            "a cd command was echoed:\n{new_text}"
        );
        // The visible prompt must reflect the new directory immediately,
        // without the user having to run a command first.
        let last = after.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("");
        assert!(
            last.contains("local"),
            "prompt did not refresh to the new directory; last line: {last:?}\nfull:\n{after}"
        );
        // The prompt we left behind must be erased, not stacked above the new
        // one — only the current directory should be on screen.
        let start_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            !after.contains(start_dir),
            "stale prompt left on screen after cd:\n{after}"
        );
    }
}

#[cfg(test)]
mod run_tests {
    use crate::config::Config;
    use crate::prompt::integration::{Channel, channel_path, write_channel};
    use crate::terminal::osc::{Marker, MarkerKind};
    use crate::terminal::session::{SessionEvent, SessionOptions, TermSize, TerminalSession};
    use std::time::{Duration, Instant};

    fn grid(session: &TerminalSession) -> String {
        let term = session.term.lock();
        let mut out = String::new();
        let mut line = i32::MIN;
        for ix in term.renderable_content().display_iter {
            if ix.point.line.0 != line {
                out.push('\n');
                line = ix.point.line.0;
            }
            out.push(ix.cell.c);
        }
        out
    }

    /// "Open in $EDITOR" must run the command without typing it at the prompt.
    /// Covers every shell OmniPTY injects into, including Apple's bash 3.2 with
    /// its trampolined `bind -x`.
    #[test]
    fn silent_run_in_supported_shells() {
        for shell in ["/bin/zsh", "/opt/homebrew/bin/bash", "/bin/bash"] {
            if std::path::Path::new(shell).exists() {
                silent_run_scenario(shell);
            }
        }
    }

    fn silent_run_scenario(shell: &str) {
        let config = Config::default();
        let integration = crate::prompt::integration::setup(&config, shell);
        let args = integration
            .args_override
            .clone()
            .unwrap_or_else(|| config.shell.args.clone());

        let dir = std::env::temp_dir().join("omnipty-run-widget-test");
        std::fs::create_dir_all(&dir).unwrap();

        let size = TermSize {
            columns: 100,
            screen_lines: 24,
            cell_width: 8.0,
            cell_height: 16.0,
            scale: 1.0,
        };
        let options = SessionOptions {
            program: shell.to_string(),
            args,
            working_directory: Some(dir.clone()),
            scrollback: 100,
            env: {
                let mut e = integration.env.clone();
                e.insert("HISTFILE".into(), "/dev/null".into());
                e
            },
            images: true,
        };
        let (session, mut rx) = TerminalSession::spawn(options, size).expect("spawn shell");
        std::thread::sleep(Duration::from_millis(1500)); // let rc files load
        let target = channel_path(Channel::Run, session.id()).expect("run channel path");

        // The marker is in the output, not in the command name, so an echoed
        // command line is distinguishable from the command's own output.
        let after = trigger(&session, "printf 'OMNIPTY_RAN_OK\\n'", "OMNIPTY_RAN_OK", shell);
        assert!(
            !after.contains("printf"),
            "{shell}: the command line was echoed:\n{after}"
        );
        assert!(
            !target.exists(),
            "{shell}: run target was not consumed, a stray trigger would replay it"
        );
        // The widget reports the run the way a typed command would, so the
        // command log sees it and startup commands learn their exit status.
        let markers = drain_markers(&mut rx);
        assert!(
            markers.iter().any(|m| matches!(&m.kind, MarkerKind::CommandStart { cmdline: Some(c) } if c.contains("OMNIPTY_RAN_OK"))),
            "{shell}: no C marker with the command line: {markers:?}"
        );
        assert!(
            markers
                .iter()
                .any(|m| m.kind == MarkerKind::CommandEnd { exit: Some(0) }),
            "{shell}: no D marker with the exit status: {markers:?}"
        );

        // A failing command reports its status too.
        trigger(
            &session,
            "printf 'OMNIPTY_FAIL\\n'; false",
            "OMNIPTY_FAIL",
            shell,
        );
        let markers = drain_markers(&mut rx);
        assert!(
            markers
                .iter()
                .any(|m| m.kind == MarkerKind::CommandEnd { exit: Some(1) }),
            "{shell}: exit status 1 was not reported: {markers:?}"
        );

        // The command must inherit a real terminal. Widgets and `bind -x`
        // handlers do not get one on stdin by default, and an editor started
        // without it either warns or refuses to run.
        let after = trigger(
            &session,
            "if [ -t 0 ]; then printf 'OMNIPTY_TTY_OK\\n'; else printf 'OMNIPTY_TTY_MISSING\\n'; fi",
            "OMNIPTY_TTY",
            shell,
        );
        assert!(
            after.contains("OMNIPTY_TTY_OK"),
            "{shell}: the command ran without a terminal on stdin:\n{after}"
        );

        // ...with the terminal's normal settings, not the line editor's.
        // bash runs `bind -x` handlers under readline's (no echo, raw
        // input); an interactive command inherits them, and `ssh -t` copies
        // them to the remote end, where nothing you type shows up.
        let modes_file = dir.join("stty-modes");
        let _ = std::fs::remove_file(&modes_file);
        trigger(
            &session,
            &format!(
                "stty -a > '{}'; printf 'OMNIPTY_MODES_READ\\n'",
                modes_file.display()
            ),
            "OMNIPTY_MODES_READ",
            shell,
        );
        let modes = std::fs::read_to_string(&modes_file).expect("stty -a output");
        let flags: Vec<&str> = modes.split_whitespace().collect();
        for flag in ["echo", "icanon"] {
            assert!(
                flags.contains(&flag),
                "{shell}: the command ran with -{flag}:\n{modes}"
            );
        }
        // The line editor gets its settings back afterwards: a typed
        // command still echoes exactly once, and runs.
        session.write_input(b"echo OMNIPTY_TYPED_$((40+2))\r".to_vec());
        let after = wait_for(
            &session,
            "OMNIPTY_TYPED_42",
            &format!("{shell}: the prompt stopped taking input after a silent run"),
        );
        assert_eq!(
            after.matches("echo OMNIPTY_TYPED_").count(),
            1,
            "{shell}: typed input wasn't echoed exactly once:\n{after}"
        );
    }

    /// Everything the shell has reported since the last drain: the markers
    /// arrive on the same channel as terminal events.
    fn drain_markers(
        rx: &mut futures::channel::mpsc::UnboundedReceiver<SessionEvent>,
    ) -> Vec<Marker> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let SessionEvent::Marker(m) = ev {
                out.push(m);
            }
        }
        out
    }

    /// Hand the shell a command through its widget and wait for `expect` to
    /// show up on screen. Returns the rendered grid.
    fn trigger(session: &TerminalSession, command: &str, expect: &str, shell: &str) -> String {
        assert!(write_channel(
            Channel::Run,
            session.id(),
            command.as_bytes()
        ));
        session.write_input(b"\x1b[9002~".to_vec());
        wait_for(session, expect, &format!("{shell}: {command:?} never ran"))
    }

    /// The deadline is for a hung shell, not a slow one: CI runners start
    /// several shells at once on three cores, and 8s wasn't always enough.
    fn wait_for(session: &TerminalSession, expect: &str, what: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            std::thread::sleep(Duration::from_millis(200));
            let grid = grid(session);
            if grid.contains(expect) {
                return grid;
            }
            if Instant::now() > deadline {
                panic!("{what}; grid:\n{grid}");
            }
        }
    }

    /// Workspace restore fires a pane's startup command on the shell's
    /// *first* prompt marker, with no delay. That only works if the widget
    /// is already bound and reading by the time the marker reaches us — so
    /// prove it, for every shell we inject into.
    #[test]
    fn a_command_sent_on_the_first_prompt_marker_runs() {
        for shell in ["/bin/zsh", "/opt/homebrew/bin/bash", "/bin/bash"] {
            if std::path::Path::new(shell).exists() {
                first_prompt_scenario(shell, true);
                // `prompt.enabled = false` keeps the user's own prompt, so the
                // marker can't ride inside PS1 — it must still be emitted.
                first_prompt_scenario(shell, false);
            }
        }
    }

    fn first_prompt_scenario(shell: &str, style_prompt: bool) {
        let mut config = Config::default();
        config.prompt.enabled = style_prompt;
        let integration = crate::prompt::integration::setup(&config, shell);
        let args = integration
            .args_override
            .clone()
            .unwrap_or_else(|| config.shell.args.clone());
        let size = TermSize {
            columns: 100,
            screen_lines: 24,
            cell_width: 8.0,
            cell_height: 16.0,
            scale: 1.0,
        };
        let options = SessionOptions {
            program: shell.to_string(),
            args,
            working_directory: Some(std::env::temp_dir()),
            scrollback: 100,
            env: {
                let mut e = integration.env.clone();
                e.insert("HISTFILE".into(), "/dev/null".into());
                e
            },
            images: true,
        };
        let (session, mut rx) = TerminalSession::spawn(options, size).expect("spawn shell");

        // Block on the channel until the first A marker, then fire at once.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match rx.try_recv() {
                Ok(SessionEvent::Marker(m)) if m.kind == MarkerKind::PromptStart => break,
                Ok(_) => continue,
                Err(_) => {
                    assert!(
                        Instant::now() < deadline,
                        "{shell} (styled prompt: {style_prompt}): no prompt marker within 8s"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        assert!(write_channel(
            Channel::Run,
            session.id(),
            b"printf 'OMNIPTY_FIRST_OK\\n'"
        ));
        session.write_input(b"\x1b[9002~".to_vec());
        let grid = wait_for(
            &session,
            "OMNIPTY_FIRST_OK",
            &format!("{shell}: command sent on the first prompt never ran"),
        );
        assert!(
            !grid.contains("printf"),
            "{shell}: the command line was echoed:\n{grid}"
        );
        let markers = drain_markers(&mut rx);
        assert!(
            markers
                .iter()
                .any(|m| m.kind == MarkerKind::CommandEnd { exit: Some(0) }),
            "{shell}: the run wasn't reported: {markers:?}"
        );
    }

    /// bash runs its DEBUG trap after a compound command's redirections are
    /// set up, so the C marker printed from it used to land in the file:
    /// `{ printf 'plain\n'; } > out` wrote `ESC ]133;C…` ahead of "plain".
    /// The file must hold only what the command wrote, and the marker must
    /// still reach the terminal.
    #[test]
    fn bash_redirected_group_keeps_the_marker_out_of_the_file() {
        for shell in ["/opt/homebrew/bin/bash", "/bin/bash"] {
            if std::path::Path::new(shell).exists() {
                redirected_group_scenario(shell);
            }
        }
    }

    fn redirected_group_scenario(shell: &str) {
        let config = Config::default();
        let integration = crate::prompt::integration::setup(&config, shell);
        let args = integration
            .args_override
            .clone()
            .unwrap_or_else(|| config.shell.args.clone());
        let dir = std::env::temp_dir().join(format!(
            "omnipty-redirect-{}-{}",
            std::process::id(),
            shell.replace('/', "_")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let size = TermSize {
            columns: 100,
            screen_lines: 24,
            cell_width: 8.0,
            cell_height: 16.0,
            scale: 1.0,
        };
        let options = SessionOptions {
            program: shell.to_string(),
            args,
            working_directory: Some(dir.clone()),
            scrollback: 100,
            env: {
                let mut e = integration.env.clone();
                e.insert("HISTFILE".into(), "/dev/null".into());
                e
            },
            images: true,
        };
        let (session, mut rx) = TerminalSession::spawn(options, size).expect("spawn bash");

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match rx.try_recv() {
                Ok(SessionEvent::Marker(m)) if m.kind == MarkerKind::PromptStart => break,
                Ok(_) => continue,
                Err(_) => {
                    assert!(Instant::now() < deadline, "{shell}: no prompt marker");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        // Typed at the prompt, not sent through the run widget: the DEBUG
        // trap is what marks typed lines. The echo's arithmetic keeps the
        // typed text from satisfying the wait.
        session.write_input(b"{ printf 'plain\\n'; } > out.txt; echo GROUP_$((40+2))\r".to_vec());
        wait_for(&session, "GROUP_42", &format!("{shell}: the redirected group never ran"));

        let written = std::fs::read(dir.join("out.txt")).unwrap();
        let markers = drain_markers(&mut rx);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            String::from_utf8_lossy(&written),
            "plain\n",
            "{shell}: the file got more than the command wrote"
        );
        assert!(
            markers
                .iter()
                .any(|m| matches!(m.kind, MarkerKind::CommandStart { .. })),
            "{shell}: the C marker never reached the terminal: {markers:?}"
        );
    }

    /// The run channel is per session: two shells handed different commands
    /// at the same instant each run their own, and neither loses one. This
    /// is what makes restoring a workspace full of startup commands safe.
    #[test]
    fn concurrent_sessions_keep_their_own_commands() {
        let shell = ["/bin/zsh", "/opt/homebrew/bin/bash", "/bin/bash"]
            .into_iter()
            .find(|s| std::path::Path::new(s).exists())
            .expect("a supported shell");
        let config = Config::default();
        let integration = crate::prompt::integration::setup(&config, shell);
        let args = integration
            .args_override
            .clone()
            .unwrap_or_else(|| config.shell.args.clone());
        let size = TermSize {
            columns: 100,
            screen_lines: 24,
            cell_width: 8.0,
            cell_height: 16.0,
            scale: 1.0,
        };
        let spawn = || {
            let options = SessionOptions {
                program: shell.to_string(),
                args: args.clone(),
                working_directory: Some(std::env::temp_dir()),
                scrollback: 100,
                env: {
                    let mut e = integration.env.clone();
                    e.insert("HISTFILE".into(), "/dev/null".into());
                    e
                },
                images: true,
            };
            TerminalSession::spawn(options, size)
                .expect("spawn shell")
                .0
        };
        let a = spawn();
        let b = spawn();
        assert_ne!(a.id(), b.id());
        std::thread::sleep(Duration::from_millis(1500));

        // Both targets are written before either trigger is sent, which is
        // exactly the pattern that raced on a single shared file.
        assert!(write_channel(
            Channel::Run,
            a.id(),
            b"printf 'OMNIPTY_A_RAN\\n'"
        ));
        assert!(write_channel(
            Channel::Run,
            b.id(),
            b"printf 'OMNIPTY_B_RAN\\n'"
        ));
        a.write_input(b"\x1b[9002~".to_vec());
        b.write_input(b"\x1b[9002~".to_vec());
        let grid_a = wait_for(&a, "OMNIPTY_A_RAN", "session A's command never ran");
        let grid_b = wait_for(&b, "OMNIPTY_B_RAN", "session B's command never ran");
        assert!(
            !grid_a.contains("OMNIPTY_B_RAN"),
            "A ran B's command:\n{grid_a}"
        );
        assert!(
            !grid_b.contains("OMNIPTY_A_RAN"),
            "B ran A's command:\n{grid_b}"
        );
        assert!(!channel_path(Channel::Run, a.id()).unwrap().exists());
        assert!(!channel_path(Channel::Run, b.id()).unwrap().exists());
    }
}
