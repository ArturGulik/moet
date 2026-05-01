//! Loading, validating, and emitting the default for
//! `~/.config/moet/moet.conf`. Format is TOML.
//!
//! Validation policy: unknown keys and invalid value types warn to stderr
//! and fall back to the default. Parse failure on the whole file warns and
//! falls back to the default. Loading is never fatal.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::thread;

pub const DEFAULT_CONFIG: &str = r#"# moet configuration file (TOML).
#
# Reset to defaults:
#   moet --print-default-config > ~/.config/moet/moet.conf
#
# Tip: use TOML *literal* strings (single quotes) for regex values so you
# don't have to double-escape backslashes — write '\.git', not "\\.git".

# Filenames matching this regex are hidden from the listing. Anchored to
# the full filename; uses Rust regex syntax. Set to '' to disable.
ignore_pattern = 'node_modules|target|\.git'

# Per-extension handler overrides. Maps a lowercase extension to the
# command run when that file type is selected. Without an override, text
# files open in $EDITOR (fallback nano) and everything else opens with
# xdg-open. Values may include flags (e.g. "zathura --fork").
# [handlers]
# pdf = "zathura"
# md  = "code"
# zip = "engrampa"
"#;

#[derive(Default)]
pub struct Config {
  pub ignore_regex: Option<regex::Regex>,
  /// Lowercase extension → command to run. The command may include
  /// arguments (e.g. `"zathura --fork"`); for external launches it is
  /// split on whitespace, for inline shell commands it is fed verbatim.
  pub handlers: HashMap<String, String>,
}

impl Config {
  /// Lookup a handler override for a given extension (case-insensitive).
  pub fn handler_for_ext(&self, ext: &str) -> Option<&str> {
    self.handlers.get(&ext.to_lowercase()).map(String::as_str)
  }

  /// Read `~/.config/moet/moet.conf`, parse it, emit any warnings to
  /// stderr, and return the resulting Config. Missing file → default
  /// Config (no warning — that's the first-run case).
  pub fn load() -> Self {
    let path = match config_path() {
      Some(p) => p,
      None => return Self::default(),
    };
    let content = match fs::read_to_string(&path) {
      Ok(c) => c,
      Err(_) => return Self::default(),
    };
    let (cfg, warnings) = Self::parse(&content);
    for w in &warnings {
      eprintln!("moet: {w}");
    }
    cfg
  }

  /// Parse a TOML config string into a Config plus a list of warnings.
  /// Pure function — no I/O — so it's directly testable.
  pub fn parse(content: &str) -> (Self, Vec<String>) {
    let mut warnings = Vec::new();
    let mut cfg = Self::default();

    let value: toml::Value = match content.parse() {
      Ok(v) => v,
      Err(e) => {
        warnings.push(format!(
          "~/.config/moet/moet.conf is not valid TOML, using defaults: {e}"
        ));
        return (cfg, warnings);
      }
    };

    let table = match value.as_table() {
      Some(t) => t,
      None => return (cfg, warnings),
    };

    for (key, val) in table {
      match key.as_str() {
        "ignore_pattern" => match val.as_str() {
          Some(s) => cfg.ignore_regex = compile_ignore_pattern(s),
          None => warnings.push("config key 'ignore_pattern' must be a string, ignoring".into()),
        },
        "handlers" => match val.as_table() {
          Some(t) => {
            for (ext, cmd) in t {
              match cmd.as_str() {
                Some(s) => {
                  cfg.handlers.insert(ext.to_lowercase(), s.to_string());
                }
                None => warnings.push(format!(
                  "config key 'handlers.{ext}' must be a string, ignoring"
                )),
              }
            }
          }
          None => {
            warnings.push("config key 'handlers' must be a table, ignoring".into());
          }
        },
        other => warnings.push(format!("unknown config key '{other}', ignoring")),
      }
    }

    (cfg, warnings)
  }
}

fn config_path() -> Option<PathBuf> {
  let home = std::env::var("HOME").ok()?;
  Some(std::path::Path::new(&home).join(".config/moet/moet.conf"))
}

/// On first run (config file missing) spawn a background thread to write
/// the default file. Startup never waits on this — runtime behavior is
/// driven entirely by `Config::load`'s defaults; the file is just a
/// designed side effect so users have something to edit later.
pub fn ensure_default_file_in_background() {
  let path = match config_path() {
    Some(p) => p,
    None => return,
  };
  if path.exists() {
    return;
  }
  thread::spawn(move || {
    if let Some(parent) = path.parent() {
      if let Err(e) = fs::create_dir_all(parent) {
        eprintln!("moet: could not create {} ({e})", parent.display());
        return;
      }
    }
    if let Err(e) = fs::write(&path, DEFAULT_CONFIG) {
      eprintln!(
        "moet: could not write default config to {} ({e})",
        path.display()
      );
    }
  });
}

/// Compile a regex pattern, anchoring it so it must match the full
/// filename — without this, `Public` would match `PublicTwinApp`.
pub fn compile_ignore_pattern(raw: &str) -> Option<regex::Regex> {
  let pattern = raw.trim();
  if pattern.is_empty() {
    return None;
  }
  let anchored = format!("^(?:{})$", pattern);
  regex::Regex::new(&anchored).ok()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn ignore_pattern_anchors_full_name() {
    let re = compile_ignore_pattern("Public").expect("regex");
    assert!(re.is_match("Public"));
    assert!(!re.is_match("PublicTwinApp"));
  }

  #[test]
  fn ignore_pattern_trims_whitespace() {
    let re = compile_ignore_pattern("  node_modules  ").expect("regex");
    assert!(re.is_match("node_modules"));
  }

  #[test]
  fn ignore_pattern_rejects_empty() {
    assert!(compile_ignore_pattern("").is_none());
    assert!(compile_ignore_pattern("   ").is_none());
  }

  #[test]
  fn ignore_pattern_supports_alternation() {
    let re = compile_ignore_pattern("node_modules|target|\\.git").expect("regex");
    assert!(re.is_match("node_modules"));
    assert!(re.is_match("target"));
    assert!(re.is_match(".git"));
    assert!(!re.is_match("targets"));
  }

  #[test]
  fn parse_reads_known_field() {
    let (cfg, warnings) = Config::parse(r#"ignore_pattern = "node_modules""#);
    assert!(warnings.is_empty());
    assert!(cfg.ignore_regex.expect("regex").is_match("node_modules"));
  }

  #[test]
  fn parse_supports_literal_strings_for_regex() {
    let (cfg, warnings) = Config::parse(r"ignore_pattern = 'node_modules|target|\.git'");
    assert!(warnings.is_empty());
    let re = cfg.ignore_regex.expect("regex");
    assert!(re.is_match(".git"));
    assert!(re.is_match("target"));
  }

  #[test]
  fn parse_warns_on_invalid_toml() {
    let (cfg, warnings) = Config::parse("not = = valid");
    assert!(cfg.ignore_regex.is_none());
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("not valid TOML"));
  }

  #[test]
  fn parse_warns_on_unknown_key() {
    let (cfg, warnings) = Config::parse(r#"surprise = "feature""#);
    assert!(cfg.ignore_regex.is_none());
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("unknown config key 'surprise'"));
  }

  #[test]
  fn parse_warns_on_wrong_value_type() {
    let (cfg, warnings) = Config::parse("ignore_pattern = 42");
    assert!(cfg.ignore_regex.is_none());
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("must be a string"));
  }

  #[test]
  fn parse_missing_field_returns_default() {
    let (cfg, warnings) = Config::parse("");
    assert!(cfg.ignore_regex.is_none());
    assert!(warnings.is_empty());
  }

  #[test]
  fn parse_reads_handlers_table() {
    let (cfg, warnings) = Config::parse(
      r#"
[handlers]
pdf = "zathura --fork"
ZIP = "engrampa"
"#,
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(cfg.handler_for_ext("pdf"), Some("zathura --fork"));
    assert_eq!(cfg.handler_for_ext("zip"), Some("engrampa"));
    assert_eq!(cfg.handler_for_ext("PDF"), Some("zathura --fork"));
    assert_eq!(cfg.handler_for_ext("missing"), None);
  }

  #[test]
  fn parse_warns_on_handlers_table_with_non_string_value() {
    let (cfg, warnings) = Config::parse(
      r#"
[handlers]
pdf = 42
"#,
    );
    assert_eq!(cfg.handlers.len(), 0);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("handlers.pdf"));
  }

  #[test]
  fn parse_warns_when_handlers_is_not_a_table() {
    let (cfg, warnings) = Config::parse(r#"handlers = "oops""#);
    assert_eq!(cfg.handlers.len(), 0);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("must be a table"));
  }

  #[test]
  fn default_config_parses_without_warnings() {
    let (cfg, warnings) = Config::parse(DEFAULT_CONFIG);
    assert!(
      warnings.is_empty(),
      "DEFAULT_CONFIG produced warnings: {warnings:?}"
    );
    assert!(
      cfg.ignore_regex.is_some(),
      "DEFAULT_CONFIG should set ignore_regex"
    );
  }
}
