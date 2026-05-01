//! Helpers for safely injecting commands into the embedded VTE terminal.
//!
//! All shell-bound paths must go through [`shell_quote`] — without it,
//! filenames containing `;`, `` ` ``, `$(...)` etc. become injection
//! vectors. The high-level helpers ([`feed_command`], [`feed_command_async`])
//! handle the input save/restore dance so the user's half-typed prompt
//! survives an injected `cd` or editor launch.

use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::Duration;

use vte4::TerminalExt;

use crate::state::State;

/// Bytes fed to the VTE *before* an injected command: Ctrl-A (Home), Space
/// (so the saved input doesn't get treated as a prompt-history match), Ctrl-E
/// (End), Ctrl-U (kill from cursor to start). Together this stashes whatever
/// the user had half-typed at the prompt into bash's kill-ring.
const PRE_SEQ: &[u8] = b"\x01 \x05\x15";
/// Bytes fed *after* the injected command finishes: Ctrl-Y (yank from
/// kill-ring), Ctrl-A, Ctrl-D (delete the leading space we inserted),
/// Ctrl-E. Restores the user's original input untouched.
const POST_SEQ: &[u8] = b"\x19\x01\x04\x05";

/// POSIX single-quote shell escaping. Wraps the whole string in single
/// quotes, and replaces any embedded `'` with `'\''`. Safe against every
/// shell metacharacter — `;`, `` ` ``, `$(...)`, `\`, newline, etc.
pub fn shell_quote(s: &str) -> String {
  let mut out = String::with_capacity(s.len() + 2);
  out.push('\'');
  for c in s.chars() {
    if c == '\'' {
      out.push_str("'\\''");
    } else {
      out.push(c);
    }
  }
  out.push('\'');
  out
}

/// Inject a quick shell command (one that completes immediately, like `cd`)
/// while preserving whatever the user had half-typed at the prompt.
/// Save → run → restore, all fed in one go.
pub fn feed_command(terminal: &vte4::Terminal, cmd: &str) {
  feed_command_async(terminal, cmd);
  feed_input_restore(terminal);
}

/// Inject a long-running shell command. Saves the user's input and submits
/// the command, but does *not* restore input — the caller must call
/// [`feed_input_restore`] once the child process exits (see
/// [`monitor_and_restore_input`]).
pub fn feed_command_async(terminal: &vte4::Terminal, cmd: &str) {
  let mut buf = Vec::with_capacity(PRE_SEQ.len() + cmd.len() + 2);
  buf.extend_from_slice(PRE_SEQ);
  buf.push(b' ');
  buf.extend_from_slice(cmd.as_bytes());
  buf.push(b'\n');
  terminal.feed_child(&buf);
}

/// Restore the user's previously-saved prompt input.
pub fn feed_input_restore(terminal: &vte4::Terminal) {
  terminal.feed_child(POST_SEQ);
}

/// Build the shell command for executing a file. With an explicit
/// `override_cmd` (e.g. from `[handlers]`), runs `<cmd> <path>`. Otherwise
/// `.py` runs through python, `.js` through node, anything else as
/// `./path` (or absolute path).
pub fn build_exec_command(path: &Path, override_cmd: Option<&str>) -> String {
  let quoted = shell_quote(&path.to_string_lossy());
  if let Some(cmd) = override_cmd {
    return format!("clear; {} {}", cmd, quoted);
  }
  let prefix = match path.extension().and_then(|e| e.to_str()) {
    Some(ext) if ext.eq_ignore_ascii_case("py") => "python ",
    Some(ext) if ext.eq_ignore_ascii_case("js") => "node ",
    _ => {
      if path.is_absolute() {
        ""
      } else {
        "./"
      }
    }
  };
  format!("clear; {}{}", prefix, quoted)
}

/// Build the shell command for opening a file in `editor`. The editor
/// string is fed to bash verbatim, so `${EDITOR:-nano}` and similar
/// expansions work.
pub fn build_edit_command(path: &Path, editor: &str) -> String {
  format!("clear; {} {}", editor, shell_quote(&path.to_string_lossy()))
}

/// Watch the terminal's foreground process group; once the launched child
/// (anything other than the shell itself) exits, restore the user's input.
/// Polls every 50 ms via the GTK main loop. Up to `MAX_STARTUP_CHECKS`
/// iterations are spent waiting for the child to *appear* — short-running
/// commands that finish before the first poll still get input restored.
pub fn monitor_and_restore_input(state: &State, terminal: &vte4::Terminal) {
  const MAX_STARTUP_CHECKS: u32 = 10;
  let state_monitor = state.clone();
  let terminal_monitor = terminal.clone();
  let mut has_started = false;
  let mut checks: u32 = 0;

  glib::timeout_add_local(Duration::from_millis(50), move || {
    let shell_pid = match *state_monitor.shell_pid.lock().unwrap() {
      Some(pid) => pid,
      None => return glib::ControlFlow::Break,
    };

    let fg_pgrp = match terminal_monitor.pty() {
      Some(pty) => unsafe { libc::tcgetpgrp(pty.fd().as_raw_fd()) },
      None => return glib::ControlFlow::Break,
    };

    if !has_started {
      if fg_pgrp != shell_pid {
        has_started = true;
        glib::ControlFlow::Continue
      } else {
        checks += 1;
        if checks > MAX_STARTUP_CHECKS {
          feed_input_restore(&terminal_monitor);
          glib::ControlFlow::Break
        } else {
          glib::ControlFlow::Continue
        }
      }
    } else if fg_pgrp == shell_pid {
      feed_input_restore(&terminal_monitor);
      glib::ControlFlow::Break
    } else {
      glib::ControlFlow::Continue
    }
  });
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn shell_quote_simple() {
    assert_eq!(shell_quote("hello"), "'hello'");
  }

  #[test]
  fn shell_quote_neutralises_metacharacters() {
    assert_eq!(shell_quote("a;b"), "'a;b'");
    assert_eq!(shell_quote("$(whoami)"), "'$(whoami)'");
    assert_eq!(shell_quote("`id`"), "'`id`'");
    assert_eq!(shell_quote("a\\b"), "'a\\b'");
    assert_eq!(shell_quote("a\nb"), "'a\nb'");
    assert_eq!(shell_quote("a\"b"), "'a\"b'");
    assert_eq!(shell_quote("a&b"), "'a&b'");
  }

  #[test]
  fn shell_quote_escapes_embedded_single_quote() {
    assert_eq!(shell_quote("it's"), "'it'\\''s'");
  }

  #[test]
  fn build_exec_command_wraps_paths() {
    assert_eq!(
      build_exec_command(Path::new("/abs/path with space"), None),
      "clear; '/abs/path with space'"
    );
    assert_eq!(
      build_exec_command(Path::new("relative.sh"), None),
      "clear; ./'relative.sh'"
    );
    assert_eq!(
      build_exec_command(Path::new("script.py"), None),
      "clear; python 'script.py'"
    );
    assert_eq!(
      build_exec_command(Path::new("/abs/script.JS"), None),
      "clear; node '/abs/script.JS'"
    );
  }

  #[test]
  fn build_exec_command_uses_override() {
    assert_eq!(
      build_exec_command(Path::new("script.py"), Some("python3")),
      "clear; python3 'script.py'"
    );
    assert_eq!(
      build_exec_command(Path::new("/abs/file.bin"), Some("./runner --debug")),
      "clear; ./runner --debug '/abs/file.bin'"
    );
  }

  #[test]
  fn build_edit_command_quotes_path() {
    assert_eq!(
      build_edit_command(Path::new("notes; rm -rf /.txt"), "nano"),
      "clear; nano 'notes; rm -rf /.txt'"
    );
  }

  #[test]
  fn build_edit_command_supports_shell_expansion() {
    assert_eq!(
      build_edit_command(Path::new("notes.md"), "${EDITOR:-nano}"),
      "clear; ${EDITOR:-nano} 'notes.md'"
    );
  }
}
