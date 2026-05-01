//! What to do with the path argument before the GTK app boots.
//!
//! `moet <path>` has six distinct cases (no path, directory, archive/pdf/
//! media, image preview, missing image, anything else). [`classify_startup`]
//! decides; main.rs reacts.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::classify::is_image_ext;
use crate::config::Config;
use crate::handlers::{
  spawn_external, ArchiveHandler, EntryHandler, ImageHandler, MediaHandler, PdfHandler,
  DEFAULT_EXTERNAL_OPENER,
};

/// What main() decided to do with its CLI argument, before the GTK app boots.
/// Only constructed when the CLI included a positional path; "no path given"
/// is represented by simply not invoking [`classify_startup`].
pub enum StartupAction {
  /// Path resolved to an existing directory; start a session there.
  Directory(PathBuf),
  /// Already handled by spawning an external program (engrampa / xdg-open).
  /// Caller should exit.
  HandledExternally,
  /// Path is an image; open the preview window inside `connect_activate`.
  ImagePreview(PathBuf),
  /// Image path didn't exist — print an error and exit.
  MissingImage(PathBuf),
  /// Path is a file that needs the terminal session to act on it.
  RunInSession {
    initial_pwd: PathBuf,
    target: PathBuf,
  },
}

/// Resolve a possibly-relative CLI path against several sensible bases (the
/// shell's PWD, the process CWD, etc.) and return the first one that exists,
/// canonicalised when possible.
pub fn resolve_path(input: &Path) -> Option<PathBuf> {
  let candidates = [
    env::var("PWD").ok().map(|pwd| Path::new(&pwd).join(input)),
    fs::canonicalize(input).ok(),
    env::current_dir().ok().map(|cwd| cwd.join(input)),
    Some(input.to_path_buf()),
  ];
  for c in candidates.into_iter().flatten() {
    if c.exists() {
      return Some(fs::canonicalize(&c).unwrap_or(c));
    }
  }
  None
}

/// Decide what to do with the CLI's path argument *before* the GTK app
/// starts. Mirrors the in-session handler chain (`build_handlers`) for the
/// classes of file that can be dispatched without a running terminal.
/// Honours `[handlers]` overrides from the loaded `Config`.
pub fn classify_startup(input: &Path, config: &Config) -> StartupAction {
  let resolved = match resolve_path(input) {
    Some(p) => p,
    None => {
      // Path doesn't exist anywhere. Images are an error; everything else
      // gets a fresh session (matches original behaviour).
      let abs = input.to_path_buf();
      let is_image = abs
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(is_image_ext);
      if is_image {
        return StartupAction::MissingImage(abs);
      }
      let pwd = abs
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
      return StartupAction::RunInSession {
        initial_pwd: pwd,
        target: abs,
      };
    }
  };

  if resolved.is_dir() {
    return StartupAction::Directory(resolved);
  }
  let override_cmd = resolved
    .extension()
    .and_then(|e| e.to_str())
    .and_then(|ext| config.handler_for_ext(ext));
  if ArchiveHandler.can_handle(&resolved)
    || PdfHandler.can_handle(&resolved)
    || MediaHandler.can_handle(&resolved)
  {
    spawn_external(override_cmd.unwrap_or(DEFAULT_EXTERNAL_OPENER), &resolved);
    return StartupAction::HandledExternally;
  }
  if ImageHandler.can_handle(&resolved) {
    return StartupAction::ImagePreview(resolved);
  }
  let pwd = resolved
    .parent()
    .map(|p| p.to_path_buf())
    .unwrap_or_else(|| PathBuf::from("."));
  StartupAction::RunInSession {
    initial_pwd: pwd,
    target: resolved,
  }
}
