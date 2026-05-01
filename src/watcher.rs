//! Filesystem watching for the directory listing.
//!
//! Each session has one inotify watch on the current directory and another
//! on the bash-side `pwd` file (written by `res/moetrc.sh` whenever the
//! user `cd`s). Both are wired up via channels so they tear down cleanly
//! when commanded — no busy-poll, no infinite-sleep keepalive thread.

use std::path::Path;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread::{spawn, JoinHandle};

use notify::{RecursiveMode, Watcher};

pub enum WatcherCommand {
  Stop,
}

pub fn is_relevant(event: &notify::Event) -> bool {
  matches!(
    event.kind,
    notify::EventKind::Modify(_) | notify::EventKind::Create(_) | notify::EventKind::Remove(_)
  )
}

/// Spawn a file-watcher thread for `path`. Returns the channel used to stop
/// the thread and its join handle. The thread blocks on the stop channel and
/// only wakes when commanded — no polling.
pub fn start_watcher_thread(
  path: &Path,
  sender: async_channel::Sender<notify::Event>,
) -> (Sender<WatcherCommand>, JoinHandle<()>) {
  let (cmd_tx, cmd_rx): (Sender<WatcherCommand>, Receiver<WatcherCommand>) = channel();
  let path = path.to_path_buf();

  let handle = spawn(move || {
    let mut watcher = notify::recommended_watcher(move |res| {
      if let Ok(event) = res {
        if is_relevant(&event) {
          let _ = sender.send_blocking(event);
        }
      }
    })
    .expect("Failed to create watcher");

    watcher
      .watch(&path, RecursiveMode::NonRecursive)
      .expect("Failed to watch path");

    // Block until told to stop. The notify watcher runs on its own thread, so
    // we don't need to do any work here.
    let _ = cmd_rx.recv();
    // `watcher` drops here, unregistering the inotify watch.
  });

  (cmd_tx, handle)
}
