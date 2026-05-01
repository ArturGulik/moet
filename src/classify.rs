//! File classification used by the listing renderer and by `--ls`.
//!
//! The "style" string returned by [`classify`] drives both the colour tag
//! applied to the entry and whether it gets a keyboard shortcut.

use std::collections::BTreeSet;

pub fn is_image_ext(ext: &str) -> bool {
  matches!(
    ext.to_lowercase().as_str(),
    "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "tif" | "tiff"
  )
}

pub fn is_text_ext(ext: &str) -> bool {
  matches!(
    ext.to_lowercase().as_str(),
    "txt"
      | "md"
      | "rs"
      | "toml"
      | "json"
      | "css"
      | "sh"
      | "conf"
      | "cfg"
      | "ini"
      | "js"
      | "py"
      | "c"
      | "h"
      | "cpp"
      | "html"
  )
}

/// The set of single-character labels available for menu shortcuts.
/// Excludes letters reserved by bash line-editing (`a`/`c`/`d`/`e`/`l`)
/// and `v` (paste). Both `--ls` and the GUI use this same pool.
pub fn label_pool() -> BTreeSet<char> {
  let mut pool: BTreeSet<char> = ('a'..='z').collect();
  pool.extend('0'..='9');
  for c in ['a', 'c', 'd', 'e', 'l', 'v'] {
    pool.remove(&c);
  }
  pool
}

/// Classify a directory entry into one of the styling buckets.
/// Mirrors `ls --color`: dirs/symlinks first, then README, then
/// extension-based (image/text), with the executable bit overriding text or
/// unmatched (but not image, since +x images are weird).
pub fn classify(name: &str, is_dir: bool, is_symlink: bool, mode: u32) -> &'static str {
  if is_dir {
    return "dir";
  }
  if is_symlink {
    return "symlink";
  }
  if name.to_uppercase().contains("README") {
    return "readme";
  }
  let ext_style = match std::path::Path::new(name)
    .extension()
    .and_then(|e| e.to_str())
  {
    Some(ext) if is_image_ext(ext) => "image",
    Some(ext) if is_text_ext(ext) => "text",
    _ => "unmatched",
  };
  if ext_style != "image" && (mode & 0o111) != 0 {
    return "exe";
  }
  ext_style
}

/// Whether an entry of the given style should receive a keyboard label.
/// Directories, symlinks, executables, and unmatched entries are always
/// labelable. For other styles (image / text / readme) we additionally
/// require a recognised extension — preserves the original behaviour where
/// a plain `README` (no extension) was unlabeled but `README.md` was.
pub fn should_have_label(style: &str, name: &str) -> bool {
  match style {
    "dir" | "symlink" | "exe" | "unmatched" => true,
    _ => match std::path::Path::new(name)
      .extension()
      .and_then(|e| e.to_str())
    {
      Some(ext) => is_image_ext(ext) || is_text_ext(ext),
      None => false,
    },
  }
}

/// Two-pass label assignment: first try each item's first letter, then fill
/// remaining items with whatever's left in the pool. Items whose `label()`
/// is already set (e.g. the synthetic `..` entry) are left alone.
pub fn assign_labels<I, F>(items: &mut [I], mut name_of: F)
where
  F: FnMut(&I) -> &str,
  I: LabelSlot,
{
  let mut pool = label_pool();
  // Pass 1: first-letter match.
  for item in items.iter_mut() {
    if !item.wants_label() || item.label().is_some() {
      continue;
    }
    if let Some(first) = name_of(item).chars().next() {
      let c = first.to_ascii_lowercase();
      if pool.remove(&c) {
        item.set_label(c);
      }
    }
  }
  // Pass 2: fill from remaining pool.
  for item in items.iter_mut() {
    if !item.wants_label() || item.label().is_some() {
      continue;
    }
    if let Some(&c) = pool.iter().next() {
      pool.remove(&c);
      item.set_label(c);
    }
  }
}

pub trait LabelSlot {
  fn wants_label(&self) -> bool;
  fn label(&self) -> Option<char>;
  fn set_label(&mut self, c: char);
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn image_ext_recognises_common_formats() {
    assert!(is_image_ext("png"));
    assert!(is_image_ext("PNG"));
    assert!(is_image_ext("jpeg"));
    assert!(is_image_ext("svg"));
    assert!(!is_image_ext("txt"));
    assert!(!is_image_ext(""));
  }

  #[test]
  fn text_ext_recognises_source_files() {
    assert!(is_text_ext("rs"));
    assert!(is_text_ext("MD"));
    assert!(is_text_ext("toml"));
    assert!(!is_text_ext("png"));
    assert!(!is_text_ext("xyz"));
  }

  #[test]
  fn label_pool_excludes_reserved_keys() {
    let pool = label_pool();
    for c in ['a', 'c', 'd', 'e', 'l', 'v'] {
      assert!(!pool.contains(&c), "pool must exclude {c}");
    }
    assert!(pool.len() >= 25);
    assert!(pool.contains(&'b'));
    assert!(pool.contains(&'0'));
  }

  #[test]
  fn classify_picks_dir_first() {
    assert_eq!(classify("anything", true, false, 0o755), "dir");
  }

  #[test]
  fn classify_symlink_beats_extension() {
    assert_eq!(classify("photo.png", false, true, 0), "symlink");
  }

  #[test]
  fn classify_readme_caseinsensitive() {
    assert_eq!(classify("README", false, false, 0), "readme");
    assert_eq!(classify("Readme.md", false, false, 0), "readme");
  }

  #[test]
  fn classify_image_ignores_executable_bit() {
    assert_eq!(classify("art.PNG", false, false, 0o755), "image");
  }

  #[test]
  fn classify_executable_overrides_text() {
    assert_eq!(classify("run.sh", false, false, 0o755), "exe");
    assert_eq!(classify("run.sh", false, false, 0o644), "text");
  }

  #[test]
  fn classify_executable_overrides_unmatched() {
    assert_eq!(classify("binary", false, false, 0o755), "exe");
  }

  #[test]
  fn classify_extensionless_nonexec_is_unmatched() {
    assert_eq!(classify("notes", false, false, 0o644), "unmatched");
  }

  #[test]
  fn should_have_label_handles_readme_correctly() {
    assert!(!should_have_label("readme", "README"));
    assert!(should_have_label("readme", "README.md"));
  }

  #[test]
  fn should_have_label_dir_and_unmatched_always_true() {
    assert!(should_have_label("dir", "anything"));
    assert!(should_have_label("symlink", "anything"));
    assert!(should_have_label("exe", "anything"));
    assert!(should_have_label("unmatched", "anything"));
  }

  struct TestItem {
    name: String,
    label: Option<char>,
    wants: bool,
  }
  impl LabelSlot for TestItem {
    fn wants_label(&self) -> bool {
      self.wants
    }
    fn label(&self) -> Option<char> {
      self.label
    }
    fn set_label(&mut self, c: char) {
      self.label = Some(c);
    }
  }
  fn item(name: &str) -> TestItem {
    TestItem {
      name: name.to_string(),
      label: None,
      wants: true,
    }
  }

  #[test]
  fn assign_labels_first_letter_when_available() {
    let mut items = vec![item("foo"), item("bar"), item("zap")];
    assign_labels(&mut items, |i| i.name.as_str());
    assert_eq!(items[0].label, Some('f'));
    assert_eq!(items[1].label, Some('b'));
    assert_eq!(items[2].label, Some('z'));
  }

  #[test]
  fn assign_labels_fills_collisions_from_pool() {
    let mut items = vec![item("foo"), item("foe")];
    assign_labels(&mut items, |i| i.name.as_str());
    assert_eq!(items[0].label, Some('f'));
    assert!(items[1].label.is_some());
    assert_ne!(items[1].label, items[0].label);
  }

  #[test]
  fn assign_labels_skips_reserved_letters() {
    let mut items = vec![item("alpha"), item("delta")];
    assign_labels(&mut items, |i| i.name.as_str());
    assert_ne!(items[0].label, Some('a'));
    assert_ne!(items[1].label, Some('d'));
    assert!(items[0].label.is_some());
    assert!(items[1].label.is_some());
  }

  #[test]
  fn assign_labels_respects_preassigned() {
    let mut items = vec![
      TestItem {
        name: "..".to_string(),
        label: Some('.'),
        wants: true,
      },
      item("foo"),
    ];
    assign_labels(&mut items, |i| i.name.as_str());
    assert_eq!(items[0].label, Some('.'));
    assert_eq!(items[1].label, Some('f'));
  }

  #[test]
  fn assign_labels_skips_items_that_dont_want_one() {
    let mut items = vec![
      item("foo"),
      TestItem {
        name: "skip".to_string(),
        label: None,
        wants: false,
      },
    ];
    assign_labels(&mut items, |i| i.name.as_str());
    assert_eq!(items[0].label, Some('f'));
    assert_eq!(items[1].label, None);
  }
}
