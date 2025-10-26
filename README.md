# lg — Simple universal logger for any command

`lg` lets you run *any* command and capture its output and key metadata with zero friction:

```bash
lg ls -la
lg curl https://example.org
lg make test
```

By default, `lg` writes a single log file into the **current directory** named:
```
<command>_<YYYY-MM-DD>_<HH-mm-ss>.log
```

You can change behavior via a config file at `~/.lg` (TOML). See [Configuration](#configuration).

---

## Why?
- Zero learning curve: just prefix `lg` in front of any command.
- Sensible defaults, powerful configuration.
- Timestamps on every line, optional gzip compression, optional split streams.
- Returns the **same exit code** as the underlying command.

## Install

### Ubuntu PPA (recommended)
On Ubuntu and derivatives, install from the official PPA:

```bash
sudo apt-get update
sudo add-apt-repository ppa:batyan45/stable
sudo apt-get update
sudo apt-get install -y lg
```

Packages are published for supported Ubuntu releases when available
(e.g., LTS like 22.04 Jammy, 24.04 Noble). See the PPA page for build
status and supported series:
https://launchpad.net/~batyan45/+archive/ubuntu/stable

### Build from source
Requirements: Rust (stable) and Cargo.

```bash
git clone https://github.com/Batyan45/lg.git
cd lg
cargo build --release
sudo install -Dm755 target/release/lg /usr/local/bin/lg
```

### Build Debian package locally
Requires standard packaging tools:

```bash
sudo apt-get update
sudo apt-get install -y build-essential debhelper devscripts dh-cargo rustc cargo
dpkg-buildpackage -us -uc -b
sudo dpkg -i ../lg_0.1.0_amd64.deb  # filename may vary
```

The package installs `/usr/bin/lg` and the man page `lg(1)`.

## Usage

Basic:
```bash
lg <command> [args...]
```

Examples:
```bash
lg echo "hello"
lg python script.py --flag
lg --split-streams --compress gz -- make test

# Keep log lines untouched (no timestamps or [STDOUT]/[STDERR]):
lg --plain-lines -- make test
```

### Exit code passthrough
`lg` exits with the **same** code as the wrapped command. This way it can be used in scripts safely.

## Configuration

`lg` will automatically create `~/.lg` (TOML) with sensible defaults the first time you run it, so you can tweak it immediately. All keys are optional. Defaults are shown below.

```toml
# Where to write logs. If unset, current directory is used.
# output_dir = "/var/log/commands"

# Whether to include arguments into the file name.
# include_args_in_name = false

# If including args, whether to include full argument list (true) or only non-flag positional args (false).
# include_full_args = true

# Replace any characters not safe for file names. Turning this off may cause errors on some filesystems.
# sanitize_filename = true

# File name template. Supported placeholders:
# {cmd}, {args}, {date}, {time}, {ts}, {exit_code}, {hostname}, {cwd}
# filename_template = "{cmd}_{date}_{time}.log"

# Timestamp formatting used for {time} and for per-line timestamps.
# See chrono formatting: https://docs.rs/chrono/latest/chrono/format/strftime/index.html
# time_format = "%H:%M:%S%.3f"
# date_format = "%Y-%m-%d"

# Write timestamp per logged line.
# timestamp_each_line = true

# Write log lines exactly as emitted (no timestamps or stream labels).
# plain_lines = false

# Combine stdout and stderr into a single log file with stream markers.
# If false and split_streams=true, separate .out.log and .err.log are written.
# combine_streams = true

# If true, produce two files: <name>.out.log and <name>.err.log
# split_streams = false

# Also print the wrapped command's output to the terminal (tee behavior).
# tee = true

# Include environment variables in the header. (May expose secrets. Use with care)
# log_env = false

# Gzip compression: one of "none", "gz"
# compress = "none"

# Put exit code into the final file name by adding {exit_code} to the filename_template.
# If {exit_code} is present, the log file is first written to a temporary path and renamed on completion.
```

### Example configuration
See [`examples/lg.example.toml`](examples/lg.example.toml).

## File name templating

Supported placeholders in `filename_template`:

- `{cmd}` — base command.
- `{args}` — arguments string (may be sanitized).
- `{date}` — current local date formatted by `date_format`.
- `{time}` — current local time formatted by `time_format`.
- `{ts}` — UNIX epoch seconds.
- `{exit_code}` — the wrapped command exit code (if available, post-run).
- `{hostname}` — system hostname.
- `{cwd}` — current working directory (sanitized).

## Man page
A concise `lg(1)` man page is included; install via the Debian package or see `debian/lg.1`.

## Security considerations
- If `log_env = true`, be aware environment variables might contain secrets.
- When including arguments in filenames, consider `sanitize_filename = true` (default).

## License
MIT — see `LICENSE`.
