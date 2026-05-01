# Moet

Massively Over-Engineered Terminal — a GTK4 terminal emulator that pairs an
embedded VTE shell with an interactive, directory-aware listing pane.
Linux-only. Single-binary Rust app. Work in progress.

## Install

```bash
cargo install --path .
```

This builds `moet` in release mode and copies it to `~/.cargo/bin/moet`. Run
the same command after pulling new commits to refresh the installed version.

(Once published to GitHub, `cargo install --git https://github.com/ArturGulik/moet`
will work without cloning first.)

## Usage

```bash
moet [PATH]                       # PATH may be a directory or a file
moet --ls [PATH]                  # print the labelled listing to stdout, then exit
moet --join-session SOCKET        # attach to a running session's IPC socket
                                  # and inherit its current directory
moet -h | --help                  # print help
```

With no `PATH`, moet opens in the current working directory. If `PATH` is a
file, the appropriate handler runs (image preview, archive opener, PDF/media
viewer, executable runner, or text editor).

## Configuration

Optional config file at `~/.config/moet/moet.conf`, parsed as TOML.
Currently one supported key:

```toml
ignore_pattern = 'node_modules|target|\.git'
```

The pattern is anchored to the full filename and uses Rust regex syntax.
Matching entries are hidden from the listing.
