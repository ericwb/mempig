# 🐷 mempig

A small process-management CLI. It shows how much memory you're using and which processes are the pigs, and it can kill them.

```
🐷 mempig  v0.1.0

  Memory  ███████████████████████████████████████▉··········  19.2G / 24.0G  80%
          free 4.8G  ·  cached 4.1G  ·  swap 2.7G / 4.0G

  The pigs  (top 6 apps, 10.9G resident, 414 apps total)

    #   PROCS      MEM      %    CPU                  NAME
    1     ×46     4.3G  18.1%    1.1  ██████████████  Google Chrome 🐷🐷🐷
    2     ×41     3.4G  14.3%    9.2  ███████████···  plugin-container 🐷🐷🐷
    3      ×1     1.1G   4.6%    0.0  ███▋··········  sublime_text 🐷
```

It's a single native Rust binary for macOS and Linux.

## Install

```sh
cargo install --path .        # puts mempig in ~/.cargo/bin
# or build and copy it yourself:
cargo build --release && cp target/release/mempig ~/.local/bin/
```

## Usage

| Command | What it does |
|---|---|
| `mempig` | Memory summary and the top 10 pigs |
| `mempig -n 25` | Show the top 25 |
| `mempig -g` | Group by app, so all Chrome helpers count as one |
| `mempig -u` | Show only your own processes |
| `mempig -f node` | Show only processes whose name contains the text (case-insensitive) |
| `mempig -w` / `-w 5` | Watch mode, refreshing every 2s (or every N seconds) |
| `mempig kill 1234` | Kill by PID with SIGTERM (asks first) |
| `mempig kill Slack -9 -y` | SIGKILL every Slack process without asking |

Pig snouts mark heavy hitters: 🐷 at 2% or more of RAM, 🐷🐷 at 5% or more, 🐷🐷🐷 at 10% or more.

"Used" memory uses the same formula as Activity Monitor on macOS (app + wired + compressed) and `MemTotal − MemAvailable` on Linux. On macOS, ordinary users can't read the stats of other users' processes (for example WindowServer). mempig hides them, says how many it hid, and shows them when run with `sudo mempig`.

Set `NO_COLOR` or pass `--no-color` to turn off colors.
