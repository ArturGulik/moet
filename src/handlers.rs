//! Per-entry actions ([`EntryHandler`] implementations) and the helpers that
//! dispatch an entry to the right handler.

use std::path::Path;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, Picture};

use crate::classify::{is_image_ext, is_text_ext};
use crate::shell_inject::{
  build_edit_command, build_exec_command, feed_command, feed_command_async,
  monitor_and_restore_input, shell_quote,
};
use crate::state::State;

/// Default editor command for text files when no `[handlers]` override is
/// set. Fed verbatim to bash, so `${EDITOR:-nano}` is expanded by the
/// shell at click-time and respects the user's environment.
pub const DEFAULT_TEXT_EDITOR: &str = "${EDITOR:-nano}";

/// Default external launcher for non-text files when no override is set.
pub const DEFAULT_EXTERNAL_OPENER: &str = "xdg-open";

/// Look up `state.config.handler_for_ext(path's extension)` returning
/// `Some(&str)` if the user has set an override, else `None`.
fn override_for<'a>(state: &'a State, path: &Path) -> Option<&'a str> {
  let ext = path.extension()?.to_str()?;
  state.config.handler_for_ext(ext)
}

pub trait EntryHandler {
  fn can_handle(&self, path: &Path) -> bool;
  fn handle(
    &self,
    app: &Application,
    path: &Path,
    state: &State,
    terminal: &vte4::Terminal,
    update_menu: &Rc<dyn Fn()>,
  );
}

/// True when `file_path`'s parent directory is the same as `target_dir`.
/// Both sides are canonicalised when possible so symlinks and trailing
/// slashes don't fool the comparison; falls back to literal `Path` equality
/// if canonicalize fails (e.g. the path doesn't exist on disk).
pub fn same_dir(file_path: &Path, target_dir: &Path) -> bool {
  let parent = match file_path.parent() {
    Some(p) => p,
    None => return false,
  };
  match (
    std::fs::canonicalize(parent).ok(),
    std::fs::canonicalize(target_dir).ok(),
  ) {
    (Some(p), Some(t)) => p == t,
    _ => parent == target_dir,
  }
}

/// Launch an external program for the given path. The command may include
/// arguments (e.g. `"zathura --fork"`); it's split on whitespace, so paths
/// with literal spaces don't work as the program. Reports failures on
/// stderr instead of swallowing them.
pub fn spawn_external(cmd: &str, path: &Path) {
  let mut parts = cmd.split_whitespace();
  let program = match parts.next() {
    Some(p) => p,
    None => {
      eprintln!("moet: empty handler command for {}", path.display());
      return;
    }
  };
  let args: Vec<&str> = parts.collect();
  if let Err(e) = std::process::Command::new(program)
    .args(&args)
    .arg(path.as_os_str())
    .spawn()
  {
    eprintln!("moet: failed to launch '{cmd}' for {}: {e}", path.display());
  }
}

/// Open a standalone preview window for an image file. Used both as the
/// ImageHandler action and at startup when moet is invoked with an image
/// argument (in which case there's no shell session at all).
pub fn present_image(app: &Application, path: &Path) {
  let path_str = path.to_string_lossy().to_string();
  let win = ApplicationWindow::builder()
    .application(app)
    .title(&path_str)
    .default_width(800)
    .default_height(600)
    .build();
  let pic = Picture::for_filename(&path_str);
  pic.set_can_shrink(true);
  win.set_child(Some(&pic));
  win.present();
}

pub struct DirectoryHandler;
impl EntryHandler for DirectoryHandler {
  fn can_handle(&self, path: &Path) -> bool {
    path.is_dir()
  }

  fn handle(
    &self,
    _app: &Application,
    path: &Path,
    state: &State,
    terminal: &vte4::Terminal,
    update_menu: &Rc<dyn Fn()>,
  ) {
    state.set_cd(path.to_string_lossy().to_string());
    update_menu();

    let cmd = format!("clear; cd {}", shell_quote(&path.to_string_lossy()));
    feed_command(terminal, &cmd);
  }
}

pub struct ImageHandler;
impl EntryHandler for ImageHandler {
  fn can_handle(&self, path: &Path) -> bool {
    path
      .extension()
      .and_then(|e| e.to_str())
      .is_some_and(is_image_ext)
  }

  fn handle(
    &self,
    app: &Application,
    path: &Path,
    _state: &State,
    _terminal: &vte4::Terminal,
    _update_menu: &Rc<dyn Fn()>,
  ) {
    present_image(app, path);
  }
}

pub struct ArchiveHandler;
impl EntryHandler for ArchiveHandler {
  fn can_handle(&self, path: &Path) -> bool {
    let p = path.to_string_lossy().to_lowercase();
    p.ends_with(".zip")
      || p.ends_with(".7z")
      || p.ends_with(".tar.gz")
      || p.ends_with(".tar")
      || p.ends_with(".gz")
      || p.ends_with(".rar")
      || p.ends_with(".xz")
  }

  fn handle(
    &self,
    _app: &Application,
    path: &Path,
    state: &State,
    _terminal: &vte4::Terminal,
    _update_menu: &Rc<dyn Fn()>,
  ) {
    spawn_external(
      override_for(state, path).unwrap_or(DEFAULT_EXTERNAL_OPENER),
      path,
    );
  }
}

pub struct PdfHandler;
impl EntryHandler for PdfHandler {
  fn can_handle(&self, path: &Path) -> bool {
    path
      .extension()
      .and_then(|e| e.to_str())
      .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
  }

  fn handle(
    &self,
    _app: &Application,
    path: &Path,
    state: &State,
    _terminal: &vte4::Terminal,
    _update_menu: &Rc<dyn Fn()>,
  ) {
    spawn_external(
      override_for(state, path).unwrap_or(DEFAULT_EXTERNAL_OPENER),
      path,
    );
  }
}

pub struct MediaHandler;
impl EntryHandler for MediaHandler {
  fn can_handle(&self, path: &Path) -> bool {
    matches!(
      path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase()),
      Some(ref e) if matches!(
        e.as_str(),
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "wma"
          | "mp4" | "mkv" | "avi" | "mov" | "webm" | "wmv" | "flv" | "mpg" | "mpeg"
      )
    )
  }

  fn handle(
    &self,
    _app: &Application,
    path: &Path,
    state: &State,
    _terminal: &vte4::Terminal,
    _update_menu: &Rc<dyn Fn()>,
  ) {
    spawn_external(
      override_for(state, path).unwrap_or(DEFAULT_EXTERNAL_OPENER),
      path,
    );
  }
}

pub struct ExecutableHandler;
impl EntryHandler for ExecutableHandler {
  fn can_handle(&self, path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path
      .metadata()
      .map(|m| m.permissions().mode() & 0o111 != 0)
      .unwrap_or(false)
  }

  fn handle(
    &self,
    _app: &Application,
    path: &Path,
    state: &State,
    terminal: &vte4::Terminal,
    _update_menu: &Rc<dyn Fn()>,
  ) {
    feed_command_async(
      terminal,
      &build_exec_command(path, override_for(state, path)),
    );
    monitor_and_restore_input(state, terminal);
  }
}

/// Handles plain text files — anything with a recognised text extension or
/// no extension at all (READMEs, Makefiles, etc.). Anything past this in
/// the chain falls through to [`DefaultHandler`].
pub struct TextFileHandler;
impl EntryHandler for TextFileHandler {
  fn can_handle(&self, path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
      Some(ext) => is_text_ext(ext),
      None => true,
    }
  }

  fn handle(
    &self,
    _app: &Application,
    path: &Path,
    state: &State,
    terminal: &vte4::Terminal,
    _update_menu: &Rc<dyn Fn()>,
  ) {
    let editor = override_for(state, path).unwrap_or(DEFAULT_TEXT_EDITOR);
    feed_command_async(terminal, &build_edit_command(path, editor));
    monitor_and_restore_input(state, terminal);
  }
}

/// Catch-all for any file class not claimed by an earlier handler. Hands
/// the file to xdg-open (or the user's per-extension override) so unknown
/// types like `.docx` or `.psd` get the system's default app.
pub struct DefaultHandler;
impl EntryHandler for DefaultHandler {
  fn can_handle(&self, _path: &Path) -> bool {
    true
  }

  fn handle(
    &self,
    _app: &Application,
    path: &Path,
    state: &State,
    _terminal: &vte4::Terminal,
    _update_menu: &Rc<dyn Fn()>,
  ) {
    spawn_external(
      override_for(state, path).unwrap_or(DEFAULT_EXTERNAL_OPENER),
      path,
    );
  }
}

/// The single source of truth for handler order. `TextFileHandler` claims
/// recognised text exts and extension-less files (READMEs etc.); anything
/// else falls through to `DefaultHandler`, which must remain last.
pub fn build_handlers() -> Vec<Box<dyn EntryHandler>> {
  vec![
    Box::new(DirectoryHandler),
    Box::new(ImageHandler),
    Box::new(ArchiveHandler),
    Box::new(PdfHandler),
    Box::new(MediaHandler),
    Box::new(ExecutableHandler),
    Box::new(TextFileHandler),
    Box::new(DefaultHandler),
  ]
}

/// Find the first handler that claims `path` and run it.
pub fn dispatch(
  handlers: &[Box<dyn EntryHandler>],
  app: &Application,
  path: &Path,
  state: &State,
  terminal: &vte4::Terminal,
  update_menu: &Rc<dyn Fn()>,
) {
  for h in handlers {
    if h.can_handle(path) {
      h.handle(app, path, state, terminal, update_menu);
      return;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::Write;

  #[test]
  fn same_dir_matches_when_target_is_files_parent() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("foo.txt");
    std::fs::File::create(&file_path)
      .unwrap()
      .write_all(b"x")
      .unwrap();
    assert!(same_dir(&file_path, dir.path()));
  }

  #[test]
  fn same_dir_rejects_different_directories() {
    let dir1 = tempfile::tempdir().unwrap();
    let dir2 = tempfile::tempdir().unwrap();
    let file_path = dir1.path().join("foo.txt");
    std::fs::File::create(&file_path)
      .unwrap()
      .write_all(b"x")
      .unwrap();
    assert!(!same_dir(&file_path, dir2.path()));
  }

  #[test]
  fn same_dir_handles_trailing_slash_via_canonicalize() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("foo.txt");
    std::fs::File::create(&file_path)
      .unwrap()
      .write_all(b"x")
      .unwrap();
    let with_slash = format!("{}/", dir.path().display());
    assert!(same_dir(&file_path, std::path::Path::new(&with_slash)));
  }
}
