//! Non-interactive CLI subcommands: `--help` and `--ls`.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use crate::classify::label_pool;
use crate::config::Config;
use crate::state::State;

pub fn print_help() {
  println!("Moet - A file manager");
  println!("Usage: moet [OPTIONS]");
  println!();
  println!("Options:");
  println!("  -h, --help               Print help");
  println!("  --ls [PATH]              Print labeled listing to stdout, then exit");
  println!("  --join-session PATH      Join session at socket path");
  println!("  --print-default-config   Print the default moet.conf to stdout");
}

/// Print a labelled directory listing to stdout, mirroring the GUI's
/// label pool so that users running `moet --ls` see the same shortcuts
/// they would see in the GUI.
pub fn run_ls(initial_pwd: Option<String>, config: Arc<Config>) {
  let target_path = initial_pwd
    .map(PathBuf::from)
    .unwrap_or_else(|| PathBuf::from("."));
  let state = State::new(Some(target_path.to_string_lossy().to_string()), config);
  let cd = state.get_cd();

  let entries = match fs::read_dir(&cd) {
    Ok(e) => e,
    Err(e) => {
      eprintln!("Error reading directory: {}", e);
      return;
    }
  };

  let mut contents_data = Vec::new();
  for entry in entries.filter_map(|e| e.ok()) {
    let name = match entry.file_name().into_string() {
      Ok(n) => n,
      Err(_) => continue,
    };
    if name.starts_with('.') {
      continue;
    }
    if let Some(re) = state.config.ignore_regex.as_ref() {
      if re.is_match(&name) {
        continue;
      }
    }

    let mut style = "unmatched";
    if let Ok(ft) = entry.file_type() {
      if ft.is_dir() {
        style = "dir";
      } else if ft.is_symlink() {
        style = "symlink";
      }
    }
    contents_data.push((name, style));
  }

  // Sort: dirs/symlinks first, then alphabetical.
  contents_data.sort_by(|a, b| {
    let pri = |s: &str| if s == "dir" || s == "symlink" { 0 } else { 1 };
    pri(a.1).cmp(&pri(b.1)).then_with(|| a.0.cmp(&b.0))
  });

  let mut label_iter = label_pool().into_iter();
  for (name, _) in contents_data {
    match label_iter.next() {
      Some(label) => println!("[{}] {}", label, name),
      None => println!("    {}", name),
    }
  }
}
