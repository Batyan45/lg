// lg: a tiny universal command logger
// Usage: lg <command> [args...]
// - Writes timestamped logs into the current directory by default.
// - Configurable via ~/.lg (TOML): output dir, filename template, include args, gzip, split streams, etc.
// - English comments throughout for clarity and maintenance.

use anyhow::{Context, Result};
use chrono::Local;
use clap::{ArgAction, Parser};
use flate2::write::GzEncoder;
use flate2::Compression;
use home::home_dir;
use hostname::get as get_hostname;
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::borrow::Cow;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

// Defaults
static DEFAULT_FILENAME_TEMPLATE: &str = "{cmd}_{date}_{time}.log";
static DEFAULT_DATE_FORMAT: &str = "%Y-%m-%d";
static DEFAULT_TIME_FORMAT: &str = "%H-%M-%S";
static DEFAULT_LINE_TIME_FORMAT: &str = "%H:%M:%S%.3f";
static DEFAULT_CONFIG_TEMPLATE: &str = include_str!("../examples/lg.example.toml");

// Cache hostname once
static HOSTNAME: Lazy<String> = Lazy::new(|| {
    get_hostname()
        .ok()
        .and_then(|o| o.into_string().ok())
        .unwrap_or_else(|| "unknown".into())
});

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
struct Config {
    output_dir: Option<PathBuf>,
    include_args_in_name: bool,
    include_full_args: bool,
    sanitize_filename: bool,
    filename_template: String,
    date_format: String,
    time_format: String,
    timestamp_each_line: bool,
    plain_lines: bool,
    combine_streams: bool,
    split_streams: bool,
    tee: bool,
    log_env: bool,
    #[serde(default = "default_compress")]
    compress: Compress,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Compress {
    None,
    Gz,
}

fn default_compress() -> Compress {
    Compress::None
}

impl Default for Config {
    fn default() -> Self {
        Self {
            output_dir: None,
            include_args_in_name: false,
            include_full_args: true,
            sanitize_filename: true,
            filename_template: DEFAULT_FILENAME_TEMPLATE.into(),
            date_format: DEFAULT_DATE_FORMAT.into(),
            time_format: DEFAULT_TIME_FORMAT.into(),
            timestamp_each_line: true,
            plain_lines: false,
            combine_streams: true,
            split_streams: false,
            tee: true,
            log_env: false,
            compress: Compress::None,
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "lg",
    version,
    about = "Log any command's output and metadata",
    disable_help_subcommand = true
)]
struct Cli {
    /// Override output directory
    #[arg(long)]
    output: Option<PathBuf>,

    /// Override filename template
    #[arg(long)]
    filename_template: Option<String>,

    /// Include arguments in filename
    #[arg(long, short = 'a', action = ArgAction::SetTrue)]
    include_args: bool,

    /// Split stdout/stderr into separate files
    #[arg(long, action = ArgAction::SetTrue)]
    split_streams: bool,

    /// Write logged lines without timestamps or stream markers
    #[arg(long, action = ArgAction::SetTrue)]
    plain_lines: bool,

    /// Compress logs: none|gz
    #[arg(long)]
    compress: Option<String>,

    /// Disable tee to terminal
    #[arg(long, action = ArgAction::SetTrue)]
    no_tee: bool,

    /// The command and its arguments to run
    #[arg(required = true, trailing_var_arg = true)]
    cmd: Vec<OsString>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let (exit_code, _) = run().await.unwrap_or((1, PathBuf::new()));
    // Exit with the wrapped command's status code
    std::process::exit(exit_code);
}

async fn run() -> Result<(i32, PathBuf)> {
    let cli = Cli::parse();

    // Read config from ~/.lg (TOML)
    let mut cfg = load_config()?;

    // Apply CLI overrides
    if let Some(out) = cli.output {
        cfg.output_dir = Some(out);
    }
    if let Some(tpl) = cli.filename_template {
        cfg.filename_template = tpl;
    }
    if cli.include_args {
        cfg.include_args_in_name = true;
    }
    if cli.split_streams {
        cfg.split_streams = true;
        cfg.combine_streams = false;
    }
    if cli.plain_lines {
        cfg.plain_lines = true;
    }
    if let Some(c) = cli.compress.as_deref() {
        cfg.compress = match c {
            "gz" => Compress::Gz,
            "none" | "" => Compress::None,
            other => {
                eprintln!("Unknown --compress value '{}', using 'none'", other);
                Compress::None
            }
        };
    }
    if cli.no_tee {
        cfg.tee = false;
    }

    // Command + args
    let cmd = cli.cmd.first().unwrap().clone();
    let args: Vec<OsString> = cli.cmd.iter().skip(1).cloned().collect();
    let cmd_str = cmd.to_string_lossy().to_string();
    let args_str = join_args(&args, cfg.include_full_args);

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let now = Local::now();
    let date_s = now.format(&cfg.date_format).to_string();
    let time_s = now.format(&cfg.time_format).to_string();
    let ts_s = now.timestamp().to_string();
    let cwd_s = cwd.to_string_lossy().to_string();

    // Prepare filename (may include exit_code which we don't know yet)
    let mut base_name = render_template(
        &cfg.filename_template,
        &cmd_str,
        &args_str,
        &date_s,
        &time_s,
        &ts_s,
        None,
        &HOSTNAME,
        &cwd_s,
        cfg.sanitize_filename,
        cfg.include_args_in_name,
    );

    // Output directory
    let out_dir = cfg.output_dir.clone().unwrap_or_else(|| cwd.clone());
    fs::create_dir_all(&out_dir).with_context(|| format!("create output dir {:?}", out_dir))?;

    // Temp path if {exit_code} is present
    let needs_rename = cfg.filename_template.contains("{exit_code}");
    let (mut log_path, final_template) = if needs_rename {
        // Use a hidden temp file to avoid partial-file confusion
        let tmp_name = format!(".{}.partial", base_name);
        (out_dir.join(tmp_name), Some(cfg.filename_template.clone()))
    } else {
        (out_dir.join(&base_name), None)
    };

    // Ensure extension for split/combined
    if cfg.split_streams {
        // We'll append .out.log and .err.log later
    } else {
        // Ensure it ends with .log (or .log.gz if compressed and user didn't set another extension)
        if std::path::Path::new(&base_name).extension().is_none() {
            base_name.push_str(".log");
            log_path = out_dir.join(&base_name);
        }
        if cfg.compress == Compress::Gz && !log_path.to_string_lossy().ends_with(".gz") {
            log_path.set_extension(format!(
                "{}gz",
                log_path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("log.")
            ));
        }
    }

    let exit_code: i32;

    // Write header and run process
    if cfg.split_streams {
        let (exit, out_path, err_path) = run_and_log_split(
            &cfg, &cmd, &args, &cwd, &log_path, &cmd_str, &args_str, &date_s, &time_s,
        )
        .await?;
        exit_code = exit;
        if let Some(tpl) = final_template {
            // We need to rename both files to include exit_code if requested.
            let out_final = out_dir.join(
                render_template(
                    &tpl,
                    &cmd_str,
                    &args_str,
                    &date_s,
                    &time_s,
                    &ts_s,
                    Some(exit_code),
                    &HOSTNAME,
                    &cwd_s,
                    cfg.sanitize_filename,
                    cfg.include_args_in_name,
                ) + ".out.log"
                    + if cfg.compress == Compress::Gz {
                        ".gz"
                    } else {
                        ""
                    },
            );
            let err_final = out_dir.join(
                render_template(
                    &tpl,
                    &cmd_str,
                    &args_str,
                    &date_s,
                    &time_s,
                    &ts_s,
                    Some(exit_code),
                    &HOSTNAME,
                    &cwd_s,
                    cfg.sanitize_filename,
                    cfg.include_args_in_name,
                ) + ".err.log"
                    + if cfg.compress == Compress::Gz {
                        ".gz"
                    } else {
                        ""
                    },
            );

            let _ = fs::rename(out_path, out_final);
            let _ = fs::rename(err_path, err_final);
        }
    } else {
        let (exit, path_written) = run_and_log_combined(
            &cfg, &cmd, &args, &cwd, &log_path, &cmd_str, &args_str, &date_s, &time_s,
        )
        .await?;
        exit_code = exit;
        if let Some(tpl) = final_template {
            // Compute final name with exit code and rename
            let final_name = render_template(
                &tpl,
                &cmd_str,
                &args_str,
                &date_s,
                &time_s,
                &ts_s,
                Some(exit_code),
                &HOSTNAME,
                &cwd_s,
                cfg.sanitize_filename,
                cfg.include_args_in_name,
            );
            let mut final_path = out_dir.join(final_name);
            // Preserve compression extension
            if path_written.to_string_lossy().ends_with(".gz")
                && !final_path.to_string_lossy().ends_with(".gz")
            {
                final_path.set_extension("log.gz");
            } else if std::path::Path::new(&final_path).extension().is_none() {
                final_path.set_extension("log");
            }
            let _ = fs::rename(path_written, final_path);
        }
    }

    Ok((exit_code, log_path))
}

fn ensure_config_file() -> Option<PathBuf> {
    let home = home_dir()?;
    let path = home.join(".lg");
    if !path.exists() {
        if let Err(err) = fs::write(&path, DEFAULT_CONFIG_TEMPLATE) {
            eprintln!("lg: failed to create default config at {:?}: {}", path, err);
            return Some(path);
        }
    }
    Some(path)
}

fn load_config() -> Result<Config> {
    let mut cfg = Config::default();
    if let Some(p) = ensure_config_file() {
        if p.exists() {
            let data = fs::read_to_string(&p).with_context(|| format!("reading config {:?}", p))?;
            let file_cfg: Config =
                toml::from_str(&data).with_context(|| format!("parsing config TOML {:?}", p))?;
            cfg = Config { ..file_cfg };
        }
    }
    Ok(cfg)
}

fn join_args(args: &[OsString], include_full: bool) -> String {
    let mut out = Vec::new();
    for a in args {
        let s = a.to_string_lossy().to_string();
        if include_full {
            out.push(s);
        } else {
            if !s.starts_with('-') {
                out.push(s);
            }
        }
    }
    out.join(" ")
}

fn sanitize_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    out.trim_matches('_').to_string()
}

fn maybe_sanitize_component<'a>(input: &'a str, sanitize: bool) -> Cow<'a, str> {
    if sanitize {
        Cow::Owned(sanitize_component(input))
    } else {
        Cow::Borrowed(input)
    }
}

fn render_template(
    tpl: &str,
    cmd: &str,
    args: &str,
    date: &str,
    time: &str,
    ts: &str,
    exit_code: Option<i32>,
    hostname: &str,
    cwd: &str,
    sanitize: bool,
    include_args_in_name: bool,
) -> String {
    let mut args_used = if include_args_in_name {
        args.to_string()
    } else {
        String::new()
    };
    if sanitize {
        args_used = sanitize_component(&args_used);
    }
    let cmd_fragment = maybe_sanitize_component(cmd, sanitize);
    let hostname_fragment = maybe_sanitize_component(hostname, sanitize);
    let cwd_fragment = maybe_sanitize_component(cwd, sanitize);
    let mut s = tpl
        .replace("{cmd}", cmd_fragment.as_ref())
        .replace("{args}", &args_used)
        .replace("{date}", date)
        .replace("{time}", time)
        .replace("{ts}", ts)
        .replace("{hostname}", hostname_fragment.as_ref())
        .replace("{cwd}", cwd_fragment.as_ref());
    if let Some(code) = exit_code {
        s = s.replace("{exit_code}", &code.to_string());
    } else {
        s = s.replace("{exit_code}", "NA");
    }
    s = s.replace("..", ".");
    while s.contains("__") {
        s = s.replace("__", "_");
    }
    s.trim_matches(|c| c == '_' || c == '.').to_string()
}

async fn run_and_log_combined(
    cfg: &Config,
    cmd: &OsString,
    args: &[OsString],
    cwd: &Path,
    log_path: &Path,
    cmd_str: &str,
    args_str: &str,
    date_s: &str,
    time_s: &str,
) -> Result<(i32, PathBuf)> {
    // Open writer (plain or gz)
    let (mut writer_box, final_path) = open_writer(cfg, log_path)?;

    // Header
    write_header(
        &mut *writer_box,
        cfg,
        cmd_str,
        args_str,
        cwd,
        date_s,
        time_s,
    )?;

    // Spawn process
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| "spawning child")?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let mut r_out = BufReader::new(stdout).lines();
    let mut r_err = BufReader::new(stderr).lines();

    let tee = cfg.tee;
    let ts_each = cfg.timestamp_each_line;
    let plain_lines = cfg.plain_lines;

    let mut out_done = false;
    let mut err_done = false;

    // Interleave lines with markers based on whichever channel yields first.
    loop {
        tokio::select! {
            line = r_out.next_line(), if !out_done => {
                match line? {
                    Some(l) => {
                        if tee { println!("{}", l); }
                        write_line(&mut *writer_box, "STDOUT", &l, ts_each, plain_lines)?;
                    }
                    None => { out_done = true; }
                }
            }
            line = r_err.next_line(), if !err_done => {
                match line? {
                    Some(l) => {
                        if tee { eprintln!("{}", l); }
                        write_line(&mut *writer_box, "STDERR", &l, ts_each, plain_lines)?;
                    }
                    None => { err_done = true; }
                }
            }
            else => { break; }
        }
    }

    let status = child.wait().await?;
    let code = status.code().unwrap_or(1);
    writeln!(
        &mut *writer_box,
        "
[exit_code] {}",
        code
    )?;
    writer_box.flush()?;

    Ok((code, final_path))
}

async fn run_and_log_split(
    cfg: &Config,
    cmd: &OsString,
    args: &[OsString],
    cwd: &Path,
    base_path: &Path,
    cmd_str: &str,
    args_str: &str,
    date_s: &str,
    time_s: &str,
) -> Result<(i32, PathBuf, PathBuf)> {
    // Paths
    let mut out_path = base_path.with_extension("out.log");
    let mut err_path = base_path.with_extension("err.log");
    if cfg.compress == Compress::Gz {
        out_path = out_path.with_extension("out.log.gz");
        err_path = err_path.with_extension("err.log.gz");
    }

    let (mut out_writer, out_final) = open_writer(cfg, &out_path)?;
    let (mut err_writer, err_final) = open_writer(cfg, &err_path)?;

    // Header
    write_header(
        &mut *out_writer,
        cfg,
        cmd_str,
        args_str,
        cwd,
        date_s,
        time_s,
    )?;
    write_header(
        &mut *err_writer,
        cfg,
        cmd_str,
        args_str,
        cwd,
        date_s,
        time_s,
    )?;

    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| "spawning child")?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let mut r_out = BufReader::new(stdout).lines();
    let mut r_err = BufReader::new(stderr).lines();

    let tee = cfg.tee;
    let ts_each = cfg.timestamp_each_line;
    let plain_lines = cfg.plain_lines;

    let mut out_done = false;
    let mut err_done = false;

    loop {
        tokio::select! {
            line = r_out.next_line(), if !out_done => {
                match line? {
                    Some(l) => {
                        if tee { println!("{}", l); }
                        write_line(&mut *out_writer, "STDOUT", &l, ts_each, plain_lines)?;
                    }
                    None => { out_done = true; }
                }
            }
            line = r_err.next_line(), if !err_done => {
                match line? {
                    Some(l) => {
                        if tee { eprintln!("{}", l); }
                        write_line(&mut *err_writer, "STDERR", &l, ts_each, plain_lines)?;
                    }
                    None => { err_done = true; }
                }
            }
            else => { break; }
        }
    }

    let status = child.wait().await?;
    let code = status.code().unwrap_or(1);
    writeln!(
        &mut *out_writer,
        "
[exit_code] {}",
        code
    )?;
    writeln!(
        &mut *err_writer,
        "
[exit_code] {}",
        code
    )?;
    out_writer.flush()?;
    err_writer.flush()?;

    Ok((code, out_final, err_final))
}

fn write_header<W: Write>(
    mut w: W,
    cfg: &Config,
    cmd: &str,
    args: &str,
    cwd: &Path,
    date_s: &str,
    time_s: &str,
) -> Result<()> {
    writeln!(w, "# lg log")?;
    writeln!(w, "cmd: {}", cmd)?;
    if !args.is_empty() {
        writeln!(w, "args: {}", args)?;
    }
    writeln!(w, "date: {} {}", date_s, time_s)?;
    writeln!(w, "cwd: {}", cwd.display())?;
    writeln!(w, "host: {}", *HOSTNAME)?;
    if cfg.log_env {
        for (k, v) in std::env::vars() {
            writeln!(w, "env[{}]={}", k, v)?;
        }
    }
    writeln!(w, "----- BEGIN OUTPUT -----")?;
    Ok(())
}

fn write_line<W: Write>(
    mut w: W,
    stream: &str,
    line: &str,
    ts_each: bool,
    plain_lines: bool,
) -> Result<()> {
    if plain_lines {
        writeln!(w, "{}", line)?;
        return Ok(());
    }
    if ts_each {
        let ts = Local::now().format(DEFAULT_LINE_TIME_FORMAT);
        writeln!(w, "[{}][{}] {}", ts, stream, line)?;
    } else {
        writeln!(w, "[{}] {}", stream, line)?;
    }
    Ok(())
}

fn open_writer(cfg: &Config, final_path: &Path) -> Result<(Box<dyn Write + Send>, PathBuf)> {
    let boxed: Box<dyn Write + Send> = match cfg.compress {
        Compress::None => {
            let file = File::create(&final_path)
                .with_context(|| format!("create file {:?}", final_path))?;
            Box::new(io::BufWriter::new(file))
        }
        Compress::Gz => {
            let file = File::create(&final_path)
                .with_context(|| format!("create file {:?}", final_path))?;
            let enc = GzEncoder::new(file, Compression::default());
            Box::new(enc)
        }
    };
    Ok((boxed, final_path.to_path_buf()))
}
