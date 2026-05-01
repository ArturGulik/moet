//! Shared mutable state for a moet session: the current working directory,
//! the keyboard-shortcut map, the bash PID, and the loaded config.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::config::Config;

#[derive(Clone)]
pub struct State {
  pub current_directory: Arc<Mutex<String>>,
  pub shortcuts: Arc<Mutex<HashMap<u32, String>>>,
  pub shell_pid: Arc<Mutex<Option<i32>>>,
  pub config: Arc<Config>,
}

impl State {
  pub fn new(initial_pwd_opt: Option<String>, config: Arc<Config>) -> Self {
    let pwd = initial_pwd_opt.unwrap_or_else(|| String::from("."));

    State {
      current_directory: Arc::new(Mutex::new(pwd)),
      shortcuts: Arc::new(Mutex::new(HashMap::new())),
      shell_pid: Arc::new(Mutex::new(None)),
      config,
    }
  }

  pub fn get_cd(&self) -> String {
    self.current_directory.lock().unwrap().clone()
  }

  pub fn set_cd(&self, new_cd: String) {
    *self.current_directory.lock().unwrap() = new_cd;
  }
}
