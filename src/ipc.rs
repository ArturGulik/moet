//! Per-session IPC server. Each `moet` instance binds
//! `/tmp/moet-<pid>.sock` and answers two messages:
//! `GET_DIR` and `MOVE_FILE:<src>:<dst>`.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::thread::spawn;

use crate::handlers::same_dir;
use crate::state::State;

/// Bind a socket at `/tmp/moet-<pid>.sock` and serve requests forever on a
/// background thread. The socket is removed first if it already exists.
pub fn start_server(state: State) {
  spawn(move || {
    let pid = std::process::id();
    let socket_path = std::env::temp_dir().join(format!("moet-{}.sock", pid));

    if socket_path.exists() {
      let _ = fs::remove_file(&socket_path);
    }

    let listener = match UnixListener::bind(&socket_path) {
      Ok(l) => l,
      Err(e) => {
        eprintln!("moet: failed to bind IPC socket: {e}");
        return;
      }
    };

    for stream in listener.incoming() {
      let mut stream = match stream {
        Ok(s) => s,
        Err(_) => continue,
      };

      let mut buffer = [0; 1024];
      let n = match stream.read(&mut buffer) {
        Ok(n) => n,
        Err(_) => continue,
      };
      let message = String::from_utf8_lossy(&buffer[..n]);

      if let Some(rest) = message.strip_prefix("MOVE_FILE:") {
        let parts: Vec<&str> = rest.splitn(2, ':').collect();
        if parts.len() != 2 {
          let _ = stream.write_all(b"ERROR:Invalid message format");
          continue;
        }
        let source_path = parts[0];
        let target_dir = parts[1];

        if same_dir(Path::new(source_path), Path::new(target_dir)) {
          let _ = stream.write_all(b"ERROR:Same directory");
        } else if let Some(filename) = Path::new(source_path).file_name() {
          let target_path = Path::new(target_dir).join(filename);
          match fs::rename(source_path, &target_path) {
            Ok(()) => {
              let _ = stream.write_all(b"OK:File moved");
            }
            Err(e) => {
              let _ = stream.write_all(format!("ERROR:{}", e).as_bytes());
            }
          }
        } else {
          let _ = stream.write_all(b"ERROR:Invalid filename");
        }
      } else {
        // GET_DIR or any unknown message returns the current directory.
        let _ = stream.write_all(state.get_cd().as_bytes());
      }
    }
  });
}
