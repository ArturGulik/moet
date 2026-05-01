mod classify;
mod cli;
mod config;
mod handlers;
mod ipc;
mod shell_inject;
mod startup;
mod state;
mod watcher;

use std::cell::RefCell;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use gio::Cancellable;
use glib::translate::IntoGlib;
use glib::SpawnFlags;
use gtk4::gdk;
use gtk4::gdk_pixbuf;
use gtk4::gio;
use gtk4::gio::ApplicationFlags;
use gtk4::prelude::*;
use gtk4::{
  Application, ApplicationWindow, Box as GtkBox, GestureClick, Image, Label, Orientation, Overlay,
  Picture, Popover, PopoverMenu, PropagationPhase, ScrolledWindow, TextView,
};
use notify::{RecursiveMode, Watcher};
use vte4::PtyFlags;
use vte4::TerminalExt;
use vte4::TerminalExtManual;

use crate::classify::{assign_labels, classify, should_have_label, LabelSlot};
use crate::config::Config;
use crate::handlers::{build_handlers, dispatch, present_image, same_dir, DEFAULT_TEXT_EDITOR};
use crate::shell_inject::{build_edit_command, build_exec_command, feed_command_async};
use crate::startup::{classify_startup, StartupAction};
use crate::state::State;
use crate::watcher::{start_watcher_thread, WatcherCommand};

fn main() {
  let args: Vec<String> = env::args().collect();

  if args.iter().any(|arg| arg == "-h" || arg == "--help") {
    cli::print_help();
    return;
  }

  if args.iter().any(|arg| arg == "--print-default-config") {
    print!("{}", config::DEFAULT_CONFIG);
    return;
  }

  let config = Arc::new(Config::load());

  let mut initial_pwd: Option<String> = None;
  let mut initial_run_target: Option<PathBuf> = None;
  let mut image_preview: Option<PathBuf> = None;
  let mut is_session_mode = true;

  // --join-session takes precedence over a positional path arg.
  if let Some(idx) = args.iter().position(|a| a == "--join-session") {
    if let Some(socket_path) = args.get(idx + 1) {
      if let Ok(mut stream) = UnixStream::connect(socket_path) {
        let mut buffer = String::new();
        if stream.read_to_string(&mut buffer).is_ok() && !buffer.is_empty() {
          initial_pwd = Some(buffer.trim().to_string());
        }
      }
    }
  } else if let Some(path_arg) = args.iter().skip(1).find(|s| !s.starts_with('-')) {
    match classify_startup(Path::new(path_arg), &config) {
      StartupAction::Directory(dir) => {
        initial_pwd = Some(dir.to_string_lossy().to_string());
      }
      StartupAction::HandledExternally => return,
      StartupAction::MissingImage(p) => {
        eprintln!("Error: Image file not found: {}", p.display());
        return;
      }
      StartupAction::ImagePreview(p) => {
        image_preview = Some(p);
        is_session_mode = false;
      }
      StartupAction::RunInSession {
        initial_pwd: pwd,
        target,
      } => {
        initial_pwd = Some(pwd.to_string_lossy().to_string());
        initial_run_target = Some(target);
      }
    }
  }

  if args.iter().any(|a| a == "--ls") {
    cli::run_ls(initial_pwd, config);
    return;
  }

  let state = State::new(initial_pwd, config);
  config::ensure_default_file_in_background();

  if is_session_mode {
    ipc::start_server(state.clone());
  }

  let tmp_path = std::env::temp_dir()
    .join("moet")
    .join(std::process::id().to_string());
  if is_session_mode && !tmp_path.exists() {
    fs::create_dir_all(&tmp_path).expect("Failed to create temporary directory");
  }

  let app = Application::builder()
    .application_id("dev.arturgulik.moet")
    .flags(ApplicationFlags::NON_UNIQUE)
    .build();

  let tmp_path_str = tmp_path.to_str().unwrap_or(".").to_string();
  // GTK's connect_activate wants Fn (potentially called multiple times),
  // so the one-shot CLI inputs go through RefCell.
  let image_preview_cell: RefCell<Option<PathBuf>> = RefCell::new(image_preview);
  let initial_run_target_cell: RefCell<Option<PathBuf>> = RefCell::new(initial_run_target);
  app.connect_activate(move |app| {
    if let Some(image_path) = image_preview_cell.borrow_mut().take() {
      present_image(app, &image_path);
      return;
    }

    let mut cmd_tx: Sender<WatcherCommand>;
    let mut watcher_handle: JoinHandle<()>;

    let terminal = vte4::Terminal::new();
    terminal.set_color_background(&gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));
    terminal.set_vexpand(true);

    // Hyperlink support. Two paths:
    //   * OSC 8 escape sequences (emitted by `ls --hyperlink=auto`, cargo,
    //     etc.) — VTE renders these underlined and emphasises them on hover
    //     once `set_allow_hyperlink` is on.
    //   * Plain-text URLs — register a regex match so the cursor changes to
    //     a pointer on hover. (VTE doesn't underline regex matches; that's
    //     a VTE limitation we share with GNOME Terminal and Tilix.)
    terminal.set_allow_hyperlink(true);
    const PCRE2_MULTILINE: u32 = 0x00000400;
    const PCRE2_CASELESS: u32 = 0x00000008;
    let url_pattern = r#"(?:https?|ftp|file)://[^\s<>'"`]*[^\s<>'"`.,;:!?)\]]"#;
    match vte4::Regex::for_match(url_pattern, PCRE2_MULTILINE | PCRE2_CASELESS) {
      Ok(re) => {
        let tag = terminal.match_add_regex(&re, 0);
        terminal.match_set_cursor_name(tag, "pointer");
      }
      Err(e) => eprintln!("moet: URL regex compile failed: {e}"),
    }

    // Ctrl+Click on a URL (OSC 8 first, then regex match) opens via xdg-open.
    // We attach in capture phase so we see the press before VTE starts a
    // selection, but we only claim the gesture when we actually handle a URL
    // — non-Ctrl clicks (and Ctrl-clicks on non-URLs) fall through to VTE.
    let url_click = gtk4::GestureClick::builder().button(1).build();
    url_click.set_propagation_phase(PropagationPhase::Capture);
    let terminal_url = terminal.clone();
    url_click.connect_pressed(move |gesture, _, x, y| {
      if !gesture
        .current_event_state()
        .contains(gdk::ModifierType::CONTROL_MASK)
      {
        return;
      }
      let url = terminal_url
        .check_hyperlink_at(x, y)
        .map(|s| s.to_string())
        .or_else(|| {
          let (matched, _tag) = terminal_url.check_match_at(x, y);
          matched.map(|s| s.to_string())
        });
      if let Some(url) = url {
        if let Err(e) = std::process::Command::new("xdg-open").arg(&url).spawn() {
          eprintln!("moet: failed to open {url}: {e}");
        }
        gesture.set_state(gtk4::EventSequenceState::Claimed);
      }
    });
    terminal.add_controller(url_click);

    let app_clone = app.clone();

    let shell = "/bin/bash";

    // Write the rcfile into the session's tmp dir rather than a NamedTempFile
    // — `spawn_async` is async, and a NamedTempFile local would unlink the
    // path when activate's body returned, racing bash's first open() of it.
    // The session tmp dir already outlives the shell.
    let rcfile_path = tmp_path.join("moetrc.sh");
    fs::write(&rcfile_path, include_str!("../res/moetrc.sh")).expect("Failed to write rcfile");
    let rcfile_str = rcfile_path
      .to_str()
      .expect("rcfile path is not valid UTF-8");

    let argv = &[shell, "--rcfile", rcfile_str];
    let envv: &[&str] = &[&format!("MOET_TMP_PATH={}", tmp_path_str)];

    // Load CSS
    let css = include_str!("../res/style.css");
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(css);

    // Apply the CSS provider to the display
    gtk4::style_context_add_provider_for_display(
      &gtk4::gdk::Display::default().unwrap(),
      &provider,
      gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let text_view = TextView::new();
    text_view.set_hexpand(true);
    text_view.set_vexpand(true);
    text_view.set_focusable(false);
    text_view.set_editable(false);
    text_view.set_cursor_visible(false);
    text_view.set_can_focus(false); // Disable focus to prevent selection
    text_view.set_focusable(false); // GTK4 property
    text_view.set_monospace(true);
    text_view.set_wrap_mode(gtk4::WrapMode::Char);

    let buffer = text_view.buffer();

    // Create tag for clickable items
    let tag_table = buffer.tag_table();
    let clickable_tag = gtk4::TextTag::builder().name("clickable").build();
    tag_table.add(&clickable_tag);

    // Highlight tag for hover effect
    let highlight_tag = gtk4::TextTag::builder()
      .name("highlight")
      .background("rgba(255, 255, 255, 0.15)")
      .build();
    tag_table.add(&highlight_tag);

    // Styling tags - mimicking ls colors
    // Directory: Blue (often bold)
    let dir_tag = gtk4::TextTag::builder()
      .name("dir")
      .foreground("#3465A4") // Tango Sky Blue 2
      .weight(600) // Bold
      .build();
    tag_table.add(&dir_tag);

    // Executable: Green (often bold)
    let exe_tag = gtk4::TextTag::builder()
      .name("exe")
      .foreground("#4E9A06") // Tango Chameleon 2
      .weight(600) // Bold
      .build();
    tag_table.add(&exe_tag);

    // Symlink: Cyan (often bold/italic)
    let symlink_tag = gtk4::TextTag::builder()
      .name("symlink")
      .foreground("#06989A") // Cyan-ish
      .style(gtk4::pango::Style::Italic)
      .build();
    tag_table.add(&symlink_tag);

    // Special/Readme: Bold + Underline
    let readme_tag = gtk4::TextTag::builder()
      .name("readme")
      .weight(700) // Extra Bold
      .underline(gtk4::pango::Underline::Single)
      .build();
    tag_table.add(&readme_tag);

    // Image: Magenta
    let image_tag = gtk4::TextTag::builder()
      .name("image")
      .foreground("#AD7FA8")
      .build();
    tag_table.add(&image_tag);

    let text_tag = gtk4::TextTag::builder().name("text").build();
    tag_table.add(&text_tag);

    let unmatched_tag = gtk4::TextTag::builder().name("unmatched").build();
    tag_table.add(&unmatched_tag);

    let state1 = state.clone();
    let text_view_menu = text_view.clone();

    let update_menu: Rc<dyn Fn()> = Rc::new(move || {
      let menu_buffer = text_view_menu.buffer();

      // Read Dir entries with metadata
      let current_cd = state1.get_cd();
      let entries_result = fs::read_dir(&current_cd);

      let mut contents_data = Vec::new();

      if let Ok(entries) = entries_result {
        for entry in entries.filter_map(|e| e.ok()) {
          if let Ok(name) = entry.file_name().into_string() {
            if name.starts_with('.') {
              continue;
            }

            if let Some(re) = state1.config.ignore_regex.as_ref() {
              if re.is_match(&name) {
                continue;
              }
            }

            let mut style = "unmatched";
            if let Ok(ft) = entry.file_type() {
              use std::os::unix::fs::PermissionsExt;
              let mode = entry
                .metadata()
                .map(|m| m.permissions().mode())
                .unwrap_or(0);
              style = classify(&name, ft.is_dir(), ft.is_symlink(), mode);
            }

            contents_data.push((name, style));
          }
        }
      }

      // sort entries: directories and symlinks first, then alphabetical
      contents_data.sort_by(|a, b| {
        let a_is_priority = if a.1 == "dir" || a.1 == "symlink" {
          0
        } else {
          1
        };
        let b_is_priority = if b.1 == "dir" || b.1 == "symlink" {
          0
        } else {
          1
        };
        a_is_priority
          .cmp(&b_is_priority)
          .then_with(|| a.0.cmp(&b.0))
      });

      let mut shortcuts = state1.shortcuts.lock().unwrap();
      shortcuts.clear();

      struct PendingItem {
        name: String,
        style: String,
        label_char: Option<char>,
        wants_label: bool,
      }
      impl LabelSlot for PendingItem {
        fn wants_label(&self) -> bool {
          self.wants_label
        }
        fn label(&self) -> Option<char> {
          self.label_char
        }
        fn set_label(&mut self, c: char) {
          self.label_char = Some(c);
        }
      }

      let mut pending_items = Vec::new();
      // ".." gets the fixed label '.'
      pending_items.push(PendingItem {
        name: "..".to_string(),
        style: "dir".to_string(),
        label_char: Some('.'),
        wants_label: true,
      });
      for (name, style) in contents_data {
        let wants = should_have_label(style, &name);
        pending_items.push(PendingItem {
          name,
          style: style.to_string(),
          label_char: None,
          wants_label: wants,
        });
      }

      assign_labels(&mut pending_items, |i| i.name.as_str());

      struct Item {
        name: String,
        label: String,
        style: String,
        full_text: String,
      }

      let mut items = Vec::new();

      for p_item in pending_items {
        let label_str = if let Some(c) = p_item.label_char {
          shortcuts.insert(gdk::unicode_to_keyval(c as u32), p_item.name.clone());
          format!("[{}] ", c)
        } else {
          "    ".to_string()
        };

        items.push(Item {
          name: p_item.name.clone(),
          label: label_str.clone(),
          style: p_item.style,
          full_text: format!("{}{}", label_str, p_item.name),
        });
      }

      // Grid Layout Calculation
      // Measure M char width
      let layout = text_view_menu.create_pango_layout(Some("M"));
      let char_width_px = layout.pixel_size().0.max(1);
      let avail_width_px = text_view_menu.allocation().width();

      // Fix flicker: Do not render until we have a valid width
      if avail_width_px < 300 {
        return;
      }

      let avail_width_px = avail_width_px.max(100);

      let max_item_len = items
        .iter()
        .map(|i| i.full_text.chars().count())
        .max()
        .unwrap_or(0);
      let padding = 2; // Spaces between cols
      let col_width_chars = max_item_len + padding;
      let col_width_px = col_width_chars as i32 * char_width_px;

      let num_cols = (avail_width_px / col_width_px).max(1);
      let num_rows = (items.len() as f64 / num_cols as f64).ceil() as usize;

      menu_buffer.set_text("");
      let mut iter = menu_buffer.start_iter();

      let tag_table = menu_buffer.tag_table();
      let clickable_tag = tag_table
        .lookup("clickable")
        .expect("Tag 'clickable' not found");
      let dir_tag = tag_table.lookup("dir").expect("Tag 'dir' not found");
      let exe_tag = tag_table.lookup("exe").expect("Tag 'exe' not found");
      let symlink_tag = tag_table
        .lookup("symlink")
        .expect("Tag 'symlink' not found");
      let readme_tag = tag_table.lookup("readme").expect("Tag 'readme' not found");
      let image_tag = tag_table.lookup("image").expect("Tag 'image' not found");
      let text_tag = tag_table.lookup("text").expect("Tag 'text' not found");
      let unmatched_tag = tag_table
        .lookup("unmatched")
        .expect("Tag 'unmatched' not found");

      for r in 0..num_rows {
        for c in 0..num_cols {
          let idx = (c as usize) * num_rows + r;
          if idx < items.len() {
            let item = &items[idx];

            let len = item.full_text.chars().count();
            // Reserve 1 char for gap between columns so tags don't merge
            let inner_width = if col_width_chars > 0 {
              col_width_chars - 1
            } else {
              0
            };
            let pad_len = inner_width.saturating_sub(len);

            // Render in parts to control styling (underline usually) on name only

            // 1. Label
            menu_buffer.insert_with_tags(&mut iter, &item.label, &[&clickable_tag]);

            // 2. Name (with specific style)
            let mut name_tags = vec![&clickable_tag];
            match item.style.as_str() {
              "dir" => name_tags.push(&dir_tag),
              "exe" => name_tags.push(&exe_tag),
              "symlink" => name_tags.push(&symlink_tag),
              "readme" => name_tags.push(&readme_tag),
              "image" => name_tags.push(&image_tag),
              "text" => name_tags.push(&text_tag),
              "unmatched" => name_tags.push(&unmatched_tag),
              _ => {}
            }
            menu_buffer.insert_with_tags(&mut iter, &item.name, &name_tags);

            // 3. Padding
            let padding = " ".repeat(pad_len);
            menu_buffer.insert_with_tags(&mut iter, &padding, &[&clickable_tag]);

            // 4. Gap
            menu_buffer.insert(&mut iter, " "); // Insert gap without tags
          }
        }
        if r < num_rows - 1 {
          menu_buffer.insert(&mut iter, "\n");
        }
      }
    });

    // The PWD-watcher thread (set up below) blocks on this channel. We need
    // the sender to outlive the activate-closure body — otherwise it drops at
    // `window.present()`'s return, the recv() errors immediately, and the
    // watcher tears down before any `cd` happens, which leaves state pinned
    // to the relative initial path. Stash it in a long-lived signal closure
    // (terminal widget lifetime) so it's only dropped at app exit.
    let (pwd_watcher_stop_tx, pwd_watcher_stop_rx) = channel::<()>();
    terminal.connect_child_exited(move |_, _| {
      let _keep_alive = &pwd_watcher_stop_tx;
      app_clone.quit();
    });

    let state_pid = state.clone();
    let terminal_run = terminal.clone();
    // The Fn signature on connect_activate forces interior mutability for the
    // one-shot `initial_run_target`; in practice activate fires once per app
    // instance (NON_UNIQUE flag) so this take() always yields the value.
    let startup_target = initial_run_target_cell.borrow_mut().take();

    terminal.spawn_async(
      PtyFlags::DEFAULT,
      None,
      argv,
      envv,
      SpawnFlags::empty(),
      || {},
      -1,
      None::<&Cancellable>,
      move |res| {
        if let Ok(pid) = res {
          *state_pid.shell_pid.lock().unwrap() = Some(pid.0);

          if let Some(target) = startup_target {
            // Decide exec vs editor: py/js always run via interpreter; other
            // files need the +x bit. (Preserves the historical startup
            // behaviour, which differs subtly from in-session click — clicking
            // a non-executable .py opens nano.)
            let is_script = matches!(
              target.extension().and_then(|e| e.to_str()),
              Some(ext) if ext.eq_ignore_ascii_case("py") || ext.eq_ignore_ascii_case("js")
            );
            let is_executable = std::fs::metadata(&target)
              .map(|m| {
                use std::os::unix::fs::PermissionsExt;
                m.permissions().mode() & 0o111 != 0
              })
              .unwrap_or(false);
            let override_cmd = target
              .extension()
              .and_then(|e| e.to_str())
              .and_then(|ext| state_pid.config.handler_for_ext(ext));
            let cmd = if is_script || is_executable {
              build_exec_command(&target, override_cmd)
            } else {
              build_edit_command(&target, override_cmd.unwrap_or(DEFAULT_TEXT_EDITOR))
            };
            feed_command_async(&terminal_run, &cmd);
          }
        }
      },
    );

    update_menu();

    let (sender, receiver) = async_channel::unbounded::<notify::Event>();
    let (pwd_changed_sender, pwd_changed_receiver) = async_channel::unbounded::<notify::Event>();

    (cmd_tx, watcher_handle) = start_watcher_thread(Path::new("."), sender.clone());

    {
      let update_menu = update_menu.clone();

      glib::MainContext::default().spawn_local(async move {
        while let Ok(_event) = receiver.recv().await {
          update_menu();
        }
      });
    }

    let sender_clone = sender.clone();
    let last_pwd_update = Arc::new(Mutex::new(chrono::Local::now()));
    let state2 = state.clone();
    let update_menu_pwd = update_menu.clone();
    glib::MainContext::default().spawn_local(async move {
      while let Ok(event) = pwd_changed_receiver.recv().await {
        type ResultType = (String, (Sender<WatcherCommand>, JoinHandle<()>));
        let (result_tx, result_rx) = async_channel::unbounded::<ResultType>();

        // Debounce events - discard if last update was less than 50ms ago
        let now = chrono::Local::now();
        {
          let mut last_update = last_pwd_update.lock().unwrap();
          if now - *last_update >= chrono::Duration::milliseconds(50) {
            *last_update = now;
          } else {
            continue;
          }
        }

        let event_path = event.paths[0].clone();
        let sender_clone = sender_clone.clone();
        let state3 = state2.clone();
        std::thread::spawn(move || {
          let path_copy = event_path.clone();
          let uri = match fs::read_to_string(event_path) {
            Ok(content) => content.trim().to_string(),
            Err(err) => {
              // If the error is because the file does not exist, create it
              if err.kind() == io::ErrorKind::NotFound {
                let location = state3.get_cd();
                let mut file = fs::OpenOptions::new()
                  .write(true)
                  .create(true)
                  .truncate(true)
                  .open(path_copy)?;
                file
                  .write_all(location.as_bytes())
                  .expect("Failed to write to PWD file");
                location
              } else {
                // If it's a different error, propagate it
                return Err(err);
              }
            }
          };

          let result = (
            uri.clone(),
            start_watcher_thread(Path::new(&uri), sender_clone),
          );

          let _ = result_tx.send_blocking(result);
          Ok::<(), io::Error>(())
        });

        // Await result back in main context
        if let Ok((uri, (new_cmd_tx, new_watcher_handle))) = result_rx.recv().await {
          state2.set_cd(uri);
          update_menu_pwd();

          // Tell the previous watcher to stop. With the blocking-recv watcher
          // loop, the thread exits as soon as the message lands; join is fast.
          let _ = cmd_tx.send(WatcherCommand::Stop);
          let _ = watcher_handle.join();

          cmd_tx = new_cmd_tx;
          watcher_handle = new_watcher_handle;
        }
      }
    });
    let tmp_path_clone = tmp_path.clone();
    let state3 = state.clone();
    std::thread::spawn(move || {
      let path = tmp_path_clone.join("pwd");
      let path = Path::new(&path);

      if !path.exists() {
        fs::write(path, state3.get_cd()).expect("Failed to create pwd file");
      }
      let mut pwd_watcher = notify::INotifyWatcher::new(
        move |res: Result<notify::Event, notify::Error>| {
          if let Ok(event) = res {
            if matches!(event.kind, notify::EventKind::Modify(_)) {
              let _ = pwd_changed_sender.send_blocking(event);
            }
          }
        },
        notify::Config::default(),
      )
      .unwrap();

      pwd_watcher
        .watch(path, RecursiveMode::NonRecursive)
        .expect("Failed to watch path");

      // Block until the matching sender drops (== child exit / app quit).
      let _ = pwd_watcher_stop_rx.recv();
    });

    let font_size_px = Rc::new(RefCell::new(20));

    let perform_action = {
      let state = state.clone();
      let terminal = terminal.clone();
      let update_menu = update_menu.clone();
      let app = app.clone();
      let handlers = build_handlers();

      Rc::new(move |path: &Path| {
        dispatch(&handlers, &app, path, &state, &terminal, &update_menu);
      })
    };

    let state_key = state.clone();
    let terminal_key = terminal.clone();
    let perform_action_key = perform_action.clone();

    let key_controller = gtk4::EventControllerKey::new();
    key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
    key_controller.connect_key_pressed(move |_controller, keyval, _keycode, _state| {
      let is_ctrl = _state.contains(gdk::ModifierType::CONTROL_MASK);
      let is_shift = _state.contains(gdk::ModifierType::SHIFT_MASK);

      // 1. Global Shortcuts (Work even if terminal is busy)
      if is_ctrl && is_shift {
        match keyval {
          gdk::Key::C => {
            terminal_key.copy_clipboard_format(vte4::Format::Text);
            return glib::Propagation::Stop;
          }
          gdk::Key::V => {
            terminal_key.paste_clipboard();
            return glib::Propagation::Stop;
          }
          _ => {}
        }
      }

      // Disable shortcuts if terminal is busy (foreground process != shell)
      if let Some(pty) = terminal_key.pty() {
        let fd_raw = pty.fd().as_raw_fd();
        let shell_pid_lock = state_key.shell_pid.lock().unwrap();
        if let Some(shell_pid) = *shell_pid_lock {
          let fg_pgrp = unsafe { libc::tcgetpgrp(fd_raw) };
          if fg_pgrp != shell_pid {
            return glib::Propagation::Proceed;
          }
        }
      }

      // Check shortcuts first
      if _state.contains(gdk::ModifierType::CONTROL_MASK) {
        // 2. Shell Shortcuts (Only when idle)
        match keyval {
          gdk::Key::v => {
            terminal_key.paste_clipboard();
            return glib::Propagation::Stop;
          }
          // If no selection, fall through (Proceed) -> Send SIGINT
          gdk::Key::c if terminal_key.has_selection() => {
            terminal_key.copy_clipboard_format(vte4::Format::Text);
            return glib::Propagation::Stop;
          }
          _ => {}
        }

        let target_path = {
          let shortcuts = state_key.shortcuts.lock().unwrap();
          shortcuts
            .get(&keyval.into_glib())
            .map(|name| Path::new(&state_key.get_cd()).join(name))
        };

        if let Some(path) = target_path {
          perform_action_key(&path);
          return glib::Propagation::Stop;
        }
      }

      // Block Ctrl+q
      if keyval == gdk::Key::q && _state == gdk::ModifierType::CONTROL_MASK {
        return glib::Propagation::Stop;
      } else if (keyval == gdk::Key::equal || keyval == gdk::Key::KP_Add)
        && _state == gdk::ModifierType::CONTROL_MASK
      {
        // Increase font size
        *font_size_px.borrow_mut() += 1;
        let css = format!("* {{ font-size: {}px; }}", *font_size_px.borrow());
        let provider = gtk4::CssProvider::new();
        provider.load_from_data(css.as_str());
        gtk4::style_context_add_provider_for_display(
          &gdk::Display::default().unwrap(),
          &provider,
          gtk4::STYLE_PROVIDER_PRIORITY_USER,
        );
        return glib::Propagation::Stop;
      } else if (keyval == gdk::Key::minus || keyval == gdk::Key::KP_Subtract)
        && _state == gdk::ModifierType::CONTROL_MASK
      {
        // Decrease font size
        *font_size_px.borrow_mut() -= 1;
        let css = format!("* {{ font-size: {}px; }}", *font_size_px.borrow());
        let provider = gtk4::CssProvider::new();
        provider.load_from_data(css.as_str());
        gtk4::style_context_add_provider_for_display(
          &gdk::Display::default().unwrap(),
          &provider,
          gtk4::STYLE_PROVIDER_PRIORITY_USER,
        );
        return glib::Propagation::Stop;
      }

      glib::Propagation::Proceed
    });

    // Mouse click controller for directory navigation
    let gesture_click = gtk4::GestureClick::new();
    gesture_click.set_button(1);
    gesture_click.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let text_view_click = text_view.clone();
    let state_click = state.clone();
    let perform_action_click = perform_action.clone();
    let terminal_click = terminal.clone();

    gesture_click.connect_released(move |_, _, x, y| {
      // Disable click if terminal is busy
      if let Some(pty) = terminal_click.pty() {
        let fd_raw = pty.fd().as_raw_fd();
        let shell_pid_lock = state_click.shell_pid.lock().unwrap();
        if let Some(shell_pid) = *shell_pid_lock {
          let fg_pgrp = unsafe { libc::tcgetpgrp(fd_raw) };
          if fg_pgrp != shell_pid {
            return;
          }
        }
      }

      let buffer = text_view_click.buffer();
      let (bx, by) =
        text_view_click.window_to_buffer_coords(gtk4::TextWindowType::Widget, x as i32, y as i32);

      if let Some(iter) = text_view_click.iter_at_location(bx, by) {
        let tag_table = buffer.tag_table();
        if let Some(clickable_tag) = tag_table.lookup("clickable") {
          if iter.has_tag(&clickable_tag) {
            let mut start = iter;
            if !start.starts_tag(Some(&clickable_tag)) {
              start.backward_to_tag_toggle(Some(&clickable_tag));
            }

            let mut end = iter;
            if !end.ends_tag(Some(&clickable_tag)) {
              end.forward_to_tag_toggle(Some(&clickable_tag));
            }

            let text = buffer.text(&start, &end, false);
            // text format is "[x] name   "
            // Extract chars[1] as the key char
            let chars: Vec<char> = text.chars().collect();
            if chars.len() >= 2 && chars[0] == '[' && chars[2] == ']' {
              let key_char = chars[1];
              let keyval = gdk::unicode_to_keyval(key_char as u32);

              let target_path = {
                let shortcuts = state_click.shortcuts.lock().unwrap();
                shortcuts
                  .get(&keyval)
                  .map(|name| std::path::Path::new(&state_click.get_cd()).join(name))
              };

              if let Some(path) = target_path {
                perform_action_click(&path);
              }
            }
          }
        }
      }
    });
    text_view.add_controller(gesture_click);

    // Popover for image preview with transparent styling
    let preview_popover = Popover::builder()
      .has_arrow(true)
      .position(gtk4::PositionType::Bottom)
      .autohide(false)
      .build();
    preview_popover.set_parent(&text_view);
    preview_popover.add_css_class("preview-popover");

    let preview_picture = Picture::new();
    preview_picture.set_can_shrink(true);
    preview_picture.set_size_request(150, 150);
    preview_popover.set_child(Some(&preview_picture));

    // Shared state for hover
    struct HoverState {
      timeout_source: Option<glib::SourceId>,
      current_item_name: Option<String>,
    }
    let hover_state = Rc::new(RefCell::new(HoverState {
      timeout_source: None,
      current_item_name: None,
    }));

    // Motion controller for hover highlight & preview
    let motion = gtk4::EventControllerMotion::new();
    let text_view_motion = text_view.clone();
    let state_motion = state.clone();
    let hover_state_motion = hover_state.clone();
    let popover_motion = preview_popover.clone();
    let picture_motion = preview_picture.clone();

    motion.connect_motion(move |_, x, y| {
      let buffer = text_view_motion.buffer();
      let (bx, by) =
        text_view_motion.window_to_buffer_coords(gtk4::TextWindowType::Widget, x as i32, y as i32);

      // Clear previous highlight
      let start_bound = buffer.start_iter();
      let end_bound = buffer.end_iter();
      let tag_table = buffer.tag_table();
      if let Some(highlight_tag) = tag_table.lookup("highlight") {
        buffer.remove_tag(&highlight_tag, &start_bound, &end_bound);
      }

      let mut new_item_name: Option<String> = None;
      let mut item_rect_iter: Option<gtk4::TextIter> = None;

      if let Some(iter) = text_view_motion.iter_at_location(bx, by) {
        if let Some(tag_table) = buffer.tag_table().into() {
          // ensure tag_table exists
          if let Some(clickable_tag) = tag_table.lookup("clickable") {
            if iter.has_tag(&clickable_tag) {
              let mut start = iter;
              if !start.starts_tag(Some(&clickable_tag)) {
                start.backward_to_tag_toggle(Some(&clickable_tag));
              }
              let mut end = iter;
              if !end.ends_tag(Some(&clickable_tag)) {
                end.forward_to_tag_toggle(Some(&clickable_tag));
              }

              if let Some(highlight_tag) = tag_table.lookup("highlight") {
                buffer.apply_tag(&highlight_tag, &start, &end);
              }

              // For the hover popover we only care about image entries; scan
              // the tags inside the clickable region for the "image" tag and
              // extract the filename from there.
              if let Some(image_tag) = tag_table.lookup("image") {
                let mut curr = start;
                while curr.offset() < end.offset() {
                  if curr.has_tag(&image_tag) {
                    let mut name_start = curr;
                    if !name_start.starts_tag(Some(&image_tag)) {
                      name_start.backward_to_tag_toggle(Some(&image_tag));
                    }
                    let mut name_end = curr;
                    if !name_end.ends_tag(Some(&image_tag)) {
                      name_end.forward_to_tag_toggle(Some(&image_tag));
                    }

                    let filename = buffer.text(&name_start, &name_end, false);
                    new_item_name = Some(filename.to_string());
                    item_rect_iter = Some(end); // Point popover at end of item or something
                    break;
                  }
                  if !curr.forward_to_tag_toggle(None::<&gtk4::TextTag>) {
                    break;
                  }
                }
              }
            }
          }
        }
      }

      let mut state = hover_state_motion.borrow_mut();

      // Determine if hover changed
      let name_changed = state.current_item_name != new_item_name;

      if name_changed {
        // Cancel old timer/hide popover
        if let Some(source_id) = state.timeout_source.take() {
          source_id.remove();
        }
        popover_motion.popdown(); // Hide immediately if moved to another item (or empty space)

        state.current_item_name = new_item_name.clone();

        if let (Some(name), Some(target_iter)) = (new_item_name, item_rect_iter) {
          // Start new timer
          let state_timer = hover_state_motion.clone();
          let popover_timer = popover_motion.clone();
          let picture_timer = picture_motion.clone();
          let cd = state_motion.get_cd();
          let text_view_timer = text_view_motion.clone();

          let source_id =
            glib::timeout_add_local(std::time::Duration::from_millis(300), move || {
              let mut s = state_timer.borrow_mut();
              s.timeout_source = None; // One-shot

              let path = std::path::Path::new(&cd).join(&name);

              // Load image using Pixbuf to scale it down
              if let Ok(pixbuf) = gdk_pixbuf::Pixbuf::from_file_at_scale(&path, 150, 150, true) {
                let texture = gdk::Texture::for_pixbuf(&pixbuf);
                picture_timer.set_paintable(Some(&texture));

                // Position popover
                let rect = text_view_timer.iter_location(&target_iter);
                let (wx, wy) = text_view_timer.buffer_to_window_coords(
                  gtk4::TextWindowType::Widget,
                  rect.x(),
                  rect.y() + rect.height(),
                );

                let pointing_to = gdk::Rectangle::new(wx, wy, 1, 1);
                popover_timer.set_pointing_to(Some(&pointing_to));
                popover_timer.popup();
              }

              glib::ControlFlow::Break
            });
          state.timeout_source = Some(source_id);
        }
      }
    });

    let text_view_leave = text_view.clone();
    let hover_state_leave = hover_state.clone();
    let popover_leave = preview_popover.clone();
    motion.connect_leave(move |_| {
      let buffer = text_view_leave.buffer();
      let start = buffer.start_iter();
      let end = buffer.end_iter();
      if let Some(highlight_tag) = buffer.tag_table().lookup("highlight") {
        buffer.remove_tag(&highlight_tag, &start, &end);
      }

      // Cancel timer and hide popover
      let mut state = hover_state_leave.borrow_mut();
      if let Some(source_id) = state.timeout_source.take() {
        source_id.remove();
      }
      state.current_item_name = None;
      popover_leave.popdown();
    });

    text_view.add_controller(motion);

    // Track internal drags to prevent overlay on self
    let is_internal_drag = std::rc::Rc::new(std::cell::RefCell::new(false));

    // Make text view behave like a list (no editing/selection/focus)
    text_view.set_editable(false);
    text_view.set_cursor_visible(false);
    text_view.set_can_focus(true); // Re-enable focus for clicks
    text_view.set_focusable(true);

    // Create a vertical box layout
    let vbox = GtkBox::new(Orientation::Vertical, 0);
    vbox.add_css_class("main-vbox");
    vbox.set_homogeneous(true);

    let scrolled_window = ScrolledWindow::builder()
      .child(&text_view)
      .vexpand(true)
      .hexpand(true)
      .build();

    // Disable text selection completely by intercepting drag events
    // BUT allow clicks on items to pass through
    let drag_intercept = gtk4::GestureDrag::new();
    drag_intercept.set_propagation_phase(PropagationPhase::Capture);

    let text_view_intercept = text_view.clone();
    drag_intercept.connect_drag_begin(move |gesture, x, y| {
      let (bx, by) = text_view_intercept.window_to_buffer_coords(
        gtk4::TextWindowType::Widget,
        x as i32,
        y as i32,
      );

      if let Some(iter) = text_view_intercept.iter_at_location(bx, by) {
        let buffer = text_view_intercept.buffer();
        if let Some(tag_table) = buffer.tag_table().into() {
          if let Some(clickable_tag) = tag_table.lookup("clickable") {
            if iter.has_tag(&clickable_tag) {
              // User is dragging/clicking a valid item.
              // Do NOT claim. Let it bubble to DragSource or GestureClick.
              return;
            }
          }
        }
      }

      // Dragging on empty space / non-clickable text -> Claim it to block selection
      gesture.set_state(gtk4::EventSequenceState::Claimed);
    });
    text_view.add_controller(drag_intercept);

    // Drag Source Controller (Attached to ScrolledWindow parent to avoid TextView conflicts)
    let drag_source = gtk4::DragSource::new();
    drag_source.set_actions(gdk::DragAction::MOVE | gdk::DragAction::COPY);
    drag_source.set_propagation_phase(PropagationPhase::Capture);

    let _text_view_drag = text_view.clone();
    let _state_drag = state.clone();
    let _scrolled_source = scrolled_window.clone();

    drag_source.connect_prepare(move |_source, x, y| {
      // Translate coords from ScrolledWindow to TextView
      let (tx, ty) = match _scrolled_source.translate_coordinates(&_text_view_drag, x, y) {
        Some(coords) => coords,
        None => {
          return None;
        }
      };

      let (bx, by) =
        _text_view_drag.window_to_buffer_coords(gtk4::TextWindowType::Widget, tx as i32, ty as i32);

      let buffer = _text_view_drag.buffer();
      if let Some(iter) = _text_view_drag.iter_at_location(bx, by) {
        if let Some(tag_table) = buffer.tag_table().into() {
          if let Some(clickable_tag) = tag_table.lookup("clickable") {
            if iter.has_tag(&clickable_tag) {
              let mut start = iter;
              if !start.starts_tag(Some(&clickable_tag)) {
                start.backward_to_tag_toggle(Some(&clickable_tag));
              }

              let mut end = iter;
              if !end.ends_tag(Some(&clickable_tag)) {
                end.forward_to_tag_toggle(Some(&clickable_tag));
              }

              let text = buffer.text(&start, &end, false);
              // Format: "[x] name   "
              let chars: Vec<char> = text.chars().collect();
              if chars.len() >= 2 && chars[0] == '[' && chars[2] == ']' {
                let key_char = chars[1];
                let keyval = gdk::unicode_to_keyval(key_char as u32);

                let shortcuts = _state_drag.shortcuts.lock().unwrap();
                if let Some(filename) = shortcuts.get(&keyval) {
                  let current_dir = _state_drag.get_cd();
                  let full_path = std::path::Path::new(&current_dir).join(filename);

                  if full_path.exists() {
                    let file = gio::File::for_path(full_path);
                    return Some(gdk::ContentProvider::for_value(&file.to_value()));
                  }
                }
              }
            }
          }
        }
      }
      None
    });

    // Internal drag state management
    let is_internal_begin = is_internal_drag.clone();
    drag_source.connect_drag_begin(move |_source, _drag| {
      *is_internal_begin.borrow_mut() = true;
    });

    let update_menu_drag = update_menu.clone();
    let is_internal_end = is_internal_drag.clone();
    drag_source.connect_drag_end(move |_source, _drag, _delete_data| {
      *is_internal_end.borrow_mut() = false;
      update_menu_drag();
    });

    scrolled_window.add_controller(drag_source);

    vbox.append(&scrolled_window);
    vbox.append(&terminal);

    // Context Menu for Listing
    text_view.set_extra_menu(Some(&gio::Menu::new()));
    let menu_model = gio::Menu::new();
    menu_model.append(Some("fullscreen terminal"), Some("app.fullscreen-terminal"));

    let context_menu = PopoverMenu::builder()
      .menu_model(&menu_model)
      .has_arrow(false)
      .build();
    context_menu.set_parent(&text_view);

    let context_menu_click = GestureClick::new();
    context_menu_click.set_button(3); // Right click
    let context_menu_clone = context_menu.clone();
    context_menu_click.connect_pressed(move |gesture, _, x, y| {
      gesture.set_state(gtk4::EventSequenceState::Claimed);
      context_menu_clone.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
      context_menu_clone.popup();
    });
    text_view.add_controller(context_menu_click);

    let fullscreen_action = gio::SimpleAction::new("fullscreen-terminal", None);
    let scrolled_window_fs = scrolled_window.clone();
    fullscreen_action.connect_activate(move |_, _| {
      let is_visible = scrolled_window_fs.get_visible();
      scrolled_window_fs.set_visible(!is_visible);
    });
    app.add_action(&fullscreen_action);

    // Create the main overlaid layout
    let window_overlay = Overlay::new();
    window_overlay.set_child(Some(&vbox));

    // Create the drag overlay widget (hidden by default)
    // 1. Wrapper for full-screen dimming
    let drag_wrapper = GtkBox::new(Orientation::Vertical, 0);
    drag_wrapper.add_css_class("drag-dimmer");
    drag_wrapper.set_halign(gtk4::Align::Fill);
    drag_wrapper.set_valign(gtk4::Align::Fill);

    // 2. Content box (Centered inside wrapper)
    let drag_overlay_box = GtkBox::new(Orientation::Vertical, 10);
    drag_overlay_box.add_css_class("drag-overlay-content");
    drag_overlay_box.set_valign(gtk4::Align::Center);
    drag_overlay_box.set_halign(gtk4::Align::Center);
    drag_overlay_box.set_vexpand(true);
    drag_overlay_box.set_hexpand(true);

    let drag_icon = Image::from_icon_name("folder-drag-accept-symbolic");
    drag_icon.set_pixel_size(64);
    drag_icon.add_css_class("drag-file-icon");
    drag_overlay_box.append(&drag_icon);

    let drag_label = Label::new(Some("Drop to Move Here"));
    drag_label.add_css_class("drag-file-name");
    drag_overlay_box.append(&drag_label);

    let drag_sublabel = Label::new(Some("Checking file..."));
    drag_sublabel.add_css_class("drag-action-label");
    drag_overlay_box.append(&drag_sublabel);

    // Add content to wrapper
    drag_wrapper.append(&drag_overlay_box);

    drag_wrapper.set_visible(false);
    window_overlay.add_overlay(&drag_wrapper);

    // Create the main window
    let window = ApplicationWindow::builder()
      .application(app)
      .title("Moet")
      .default_width(800)
      .default_height(600)
      .child(&window_overlay)
      .build();

    window.add_controller(key_controller);

    // Simple drop target for testing - accept any drag action
    let drop_target = gtk4::DropTarget::new(gio::File::static_type(), gdk::DragAction::COPY);
    drop_target.set_preload(true);
    drop_target.set_propagation_phase(PropagationPhase::Capture);

    let state_drop = state.clone();
    let update_menu_drop = update_menu.clone();

    // Capture overlay widgets for updates
    let drag_overlay_enter = drag_wrapper.clone();
    let drag_overlay_leave = drag_wrapper.clone();
    let drag_overlay_drop = drag_wrapper.clone();

    let label_enter = drag_label.clone();
    let sublabel_enter = drag_sublabel.clone();
    let icon_enter = drag_icon.clone();
    let state_enter = state.clone();
    let is_internal_enter = is_internal_drag.clone();

    drop_target.connect_enter(move |target, _x, _y| {
      // 1. Check if internal drag
      if *is_internal_enter.borrow() {
        return gdk::DragAction::COPY; // Still allow drag, just don't show overlay
      }

      // 2. Reset defaults and show overlay
      label_enter.set_text("Drop to Move Here");
      sublabel_enter.set_text("Checking file...");
      drag_overlay_enter.set_visible(true);

      // 3. Try to update with specific info
      let value = target.value();
      if let Some(file) = value.as_ref().and_then(|v| v.get::<gio::File>().ok()) {
        let filename = file
          .path()
          .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
          .unwrap_or_else(|| "Unknown File".to_string());

        let source_path = file
          .parent()
          .and_then(|p| p.path())
          .map(|p| p.to_string_lossy().to_string())
          .unwrap_or_else(|| "External Source".to_string());

        let dest_path = state_enter.get_cd();

        label_enter.set_text(&format!("Move '{}'", filename));
        sublabel_enter.set_text(&format!("From: {}\nTo: {}", source_path, dest_path));

        let same = file
          .path()
          .map(|fp| same_dir(&fp, Path::new(&dest_path)))
          .unwrap_or(false);
        if same {
          label_enter.set_text("No Effect");
          sublabel_enter.set_text("Source and destination are the same.");
          icon_enter.set_icon_name(Some("dialog-warning-symbolic"));
        } else {
          icon_enter.set_icon_name(Some("folder-drag-accept-symbolic"));
        }
      } else {
        sublabel_enter.set_text("Waiting for data...");
      }

      gdk::DragAction::COPY
    });

    // Handle async value updates (fixes "Type: None")
    let label_notify = drag_label.clone();
    let sublabel_notify = drag_sublabel.clone();
    let icon_notify = drag_icon.clone();
    let state_notify = state.clone();

    drop_target.connect_notify_local(Some("value"), move |target, _| {
      let value = target.value();
      if let Some(file) = value.as_ref().and_then(|v| v.get::<gio::File>().ok()) {
        let filename = file
          .path()
          .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
          .unwrap_or_else(|| "Unknown File".to_string());

        let source_path = file
          .parent()
          .and_then(|p| p.path())
          .map(|p| p.to_string_lossy().to_string())
          .unwrap_or_else(|| "External Source".to_string());

        let dest_path = state_notify.get_cd();

        label_notify.set_text(&format!("Move '{}'", filename));
        sublabel_notify.set_text(&format!("From: {}\nTo: {}", source_path, dest_path));

        let same = file
          .path()
          .map(|fp| same_dir(&fp, Path::new(&dest_path)))
          .unwrap_or(false);
        if same {
          label_notify.set_text("No Effect");
          sublabel_notify.set_text("Source and destination are the same.");
          icon_notify.set_icon_name(Some("dialog-warning-symbolic"));
        } else {
          icon_notify.set_icon_name(Some("folder-drag-accept-symbolic"));
        }
      }
    });

    drop_target.connect_leave(move |_target| {
      drag_overlay_leave.set_visible(false);
    });

    drop_target.connect_drop(move |_target, value, _x, _y| {
      drag_overlay_drop.set_visible(false);

      // Try to get as file first
      if let Ok(file) = value.get::<gio::File>() {
        let source_path = file.path().unwrap().to_string_lossy().to_string();
        let target_dir = state_drop.get_cd();

        if same_dir(Path::new(&source_path), Path::new(&target_dir)) {
          return false;
        }

        // Perform the file move
        if let Some(filename) = std::path::Path::new(&source_path).file_name() {
          let target_path = std::path::Path::new(&target_dir).join(filename);

          match std::fs::rename(&source_path, &target_path) {
            Ok(()) => {
              update_menu_drop();
              true
            }
            Err(e) => {
              eprintln!("Failed to move file: {}", e);
              false
            }
          }
        } else {
          false
        }
      } else {
        // Non-file drop (e.g. text/uri-list as a String) — not handled.
        false
      }
    });

    window.add_controller(drop_target);

    let terminal_focus_clone = terminal.clone();
    // Automatically refocus terminal when app window is active
    window.connect_notify_local(Some("is-active"), move |win, _| {
      if win.is_active() {
        terminal_focus_clone.grab_focus();
      }
    });

    // Width-change re-render. GTK4 has no `allocation` notify signal and no
    // public size-allocate vfunc on TextView, so we ride the frame clock:
    // once per frame, check if the text view's width changed since last
    // render, and only call update_menu when it did. This handles three
    // things at once — initial sizing (the value goes 0 → final over the
    // first few frames), the post-`window.present()` settle, and any user
    // resize — without relying on a guess-and-poll timer.
    let last_rendered_width = Rc::new(RefCell::new(-1i32));
    let update_menu_tick = update_menu.clone();
    text_view.add_tick_callback(move |tv, _clock| {
      let w = tv.allocation().width();
      if w != *last_rendered_width.borrow() {
        *last_rendered_width.borrow_mut() = w;
        update_menu_tick();
      }
      glib::ControlFlow::Continue
    });

    window.present();
  });

  app.run_with_args(&[] as &[&str]);
}
