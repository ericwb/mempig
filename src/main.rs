//! mempig - see how much memory you're using, and which processes are the pigs.

use std::collections::HashMap;
use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use sysinfo::{
    MINIMUM_CPU_UPDATE_INTERVAL, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind, Users,
};

const PIG: &str = "\u{1F437}";
const VERSION: &str = env!("CARGO_PKG_VERSION");

// ------------------------------------------------------------------ CLI

#[derive(Parser)]
#[command(
    name = "mempig",
    version,
    about = "Show total and used memory, and the processes hogging it.",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    view: ViewArgs,
}

#[derive(Args)]
struct ViewArgs {
    /// How many pigs to show
    #[arg(short, long, default_value_t = 10)]
    num: usize,
    /// Group processes by app name (e.g. all Chrome helpers together)
    #[arg(short, long)]
    group: bool,
    /// Only show your own processes
    #[arg(short, long)]
    user: bool,
    /// Only show processes whose name contains this text (case-insensitive)
    #[arg(short, long, value_name = "TEXT")]
    filter: Option<String>,
    /// Refresh continuously, every SECS seconds
    #[arg(short, long, value_name = "SECS", num_args = 0..=1, default_missing_value = "2")]
    watch: Option<f64>,
    /// Disable colors
    #[arg(long)]
    no_color: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Kill a pig by PID or by process/app name (asks first)
    Kill {
        /// PID or name (e.g. "Slack", "node")
        target: String,
        /// Use SIGKILL instead of SIGTERM
        #[arg(short = '9', long)]
        force: bool,
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
}

// ------------------------------------------------------------------ formatting

static COLOR: AtomicBool = AtomicBool::new(false);

fn paint(code: &str, s: &str) -> String {
    if COLOR.load(Ordering::Relaxed) {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}
fn bold(s: &str) -> String {
    paint("1", s)
}
fn dim(s: &str) -> String {
    paint("2", s)
}
fn red(s: &str) -> String {
    paint("31", s)
}
fn yellow(s: &str) -> String {
    paint("33", s)
}
fn green(s: &str) -> String {
    paint("32", s)
}
fn pink(s: &str) -> String {
    paint("95", s)
}

fn heat(frac: f64, s: &str) -> String {
    if frac >= 0.85 {
        red(s)
    } else if frac >= 0.6 {
        yellow(s)
    } else {
        green(s)
    }
}

fn human(bytes: u64) -> String {
    let mut n = bytes as f64;
    for unit in ["B", "K", "M", "G", "T"] {
        if n < 1024.0 || unit == "T" {
            return if unit == "B" || unit == "K" {
                format!("{n:.0}{unit}")
            } else {
                format!("{n:.1}{unit}")
            };
        }
        n /= 1024.0;
    }
    unreachable!()
}

fn bar(frac: f64, width: usize) -> String {
    const PARTIALS: [char; 7] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let frac = frac.clamp(0.0, 1.0);
    let exact = frac * width as f64;
    let full = exact as usize;
    let mut filled: String = "█".repeat(full);
    let rem = exact - full as f64;
    if full < width && rem >= 1.0 / 7.0 {
        filled.push(PARTIALS[((rem * 7.0) as usize).min(6)]);
    }
    let used = filled.chars().count();
    filled + &dim(&"·".repeat(width - used))
}

fn term_width() -> usize {
    // SAFETY: TIOCGWINSZ only writes into the winsize struct we pass.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            return ws.ws_col as usize;
        }
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(100)
}

fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        s.chars().take(width - 1).collect::<String>() + "…"
    }
}

// ------------------------------------------------------------------ memory

struct MemInfo {
    total: u64,
    used: u64,
    cached: u64,
    swap_used: u64,
    swap_total: u64,
}

#[cfg(target_os = "macos")]
#[allow(deprecated)] // libc's mach_host_self; fine for a single call
fn mem_info(sys: &System) -> MemInfo {
    // Same formula Activity Monitor uses: app memory + wired + compressed.
    // SAFETY: plain Mach calls writing into correctly sized out-params.
    unsafe {
        let host = libc::mach_host_self();
        let mut stats: libc::vm_statistics64 = std::mem::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        let kr = libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            &mut stats as *mut _ as libc::host_info64_t,
            &mut count,
        );
        let page = libc::sysconf(libc::_SC_PAGESIZE);
        let total = sys.total_memory();
        if kr != libc::KERN_SUCCESS || page <= 0 {
            return MemInfo {
                total,
                used: sys.used_memory(),
                cached: 0,
                swap_used: sys.used_swap(),
                swap_total: sys.total_swap(),
            };
        }
        let p = page as u64;
        let app = (stats.internal_page_count as u64).saturating_sub(stats.purgeable_count as u64);
        let used = (app + stats.wire_count as u64 + stats.compressor_page_count as u64) * p;
        let cached = (stats.external_page_count as u64 + stats.purgeable_count as u64) * p;
        MemInfo {
            total,
            used: used.min(total),
            cached,
            swap_used: sys.used_swap(),
            swap_total: sys.total_swap(),
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn mem_info(sys: &System) -> MemInfo {
    let mut cached = 0;
    if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
        for line in text.lines() {
            let mut it = line.split_whitespace();
            if let (Some(k), Some(v)) = (it.next(), it.next()) {
                if k == "Cached:" || k == "Buffers:" {
                    cached += v.parse::<u64>().unwrap_or(0) * 1024;
                }
            }
        }
    }
    let total = sys.total_memory();
    MemInfo {
        total,
        used: total.saturating_sub(sys.available_memory()),
        cached,
        swap_used: sys.used_swap(),
        swap_total: sys.total_swap(),
    }
}

// ------------------------------------------------------------------ processes

#[derive(Clone)]
struct Proc {
    pid: u32,
    user: String,
    rss: u64,
    cpu: f32,
    name: String,
    count: usize,
}

/// Per-process CPU% sampler. sysinfo's per-process CPU reads 0 on recent macOS,
/// so on macOS we diff task CPU time from proc_pidinfo ourselves.
#[derive(Default)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct CpuSampler {
    last: HashMap<u32, u64>,
    at: Option<Instant>,
    usage: HashMap<u32, f32>,
}

impl CpuSampler {
    #[cfg(target_os = "macos")]
    #[allow(deprecated)] // libc's mach_timebase_info
    fn sample(&mut self, sys: &System) {
        let (numer, denom) = unsafe {
            let mut tb: libc::mach_timebase_info = std::mem::zeroed();
            libc::mach_timebase_info(&mut tb);
            if tb.denom == 0 {
                (1, 1)
            } else {
                (tb.numer as u64, tb.denom as u64)
            }
        };
        let now = Instant::now();
        let wall_ns = self.at.map(|t| now.duration_since(t).as_nanos() as f64);
        let mut next = HashMap::with_capacity(sys.processes().len());
        self.usage.clear();
        for pid in sys.processes().keys() {
            let pid = pid.as_u32();
            // SAFETY: proc_pidinfo writes at most `size` bytes into `info`.
            let ns = unsafe {
                let mut info: libc::proc_taskinfo = std::mem::zeroed();
                let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
                let n = libc::proc_pidinfo(
                    pid as libc::c_int,
                    libc::PROC_PIDTASKINFO,
                    0,
                    &mut info as *mut _ as *mut libc::c_void,
                    size,
                );
                if n != size {
                    continue;
                }
                // Task times are in mach absolute-time units.
                (info.pti_total_user + info.pti_total_system) as u128 * numer as u128
                    / denom as u128
            } as u64;
            if let (Some(wall), Some(prev)) = (wall_ns, self.last.get(&pid)) {
                if wall > 0.0 {
                    self.usage
                        .insert(pid, (ns.saturating_sub(*prev) as f64 / wall * 100.0) as f32);
                }
            }
            next.insert(pid, ns);
        }
        self.last = next;
        self.at = Some(now);
    }

    #[cfg(not(target_os = "macos"))]
    fn sample(&mut self, _sys: &System) {}

    fn get(&self, pid: u32, fallback: f32) -> f32 {
        self.usage.get(&pid).copied().unwrap_or(fallback)
    }
}

fn refresh(sys: &mut System, cpu: &mut CpuSampler) {
    refresh_sys(sys);
    cpu.sample(sys);
}

fn refresh_sys(sys: &mut System) {
    sys.refresh_memory();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_memory()
            .with_cpu()
            .with_user(UpdateKind::OnlyIfNotSet)
            .with_exe(UpdateKind::OnlyIfNotSet)
            .with_cmd(UpdateKind::OnlyIfNotSet),
    );
}

/// Best display name: the executable's file name, unless that looks like a
/// version number (e.g. ~/.local/share/claude/versions/2.1.332), in which case
/// use the first word of argv[0].
fn display_name(p: &sysinfo::Process) -> String {
    let exe = p
        .exe()
        .and_then(|e| e.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.name().to_string_lossy().into_owned());
    let versiony = !exe.is_empty()
        && exe
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == '-');
    if versiony {
        if let Some(arg0) = p.cmd().first() {
            let arg0 = arg0.to_string_lossy();
            if let Some(word) = arg0.split_whitespace().next() {
                let base = word.rsplit('/').next().unwrap_or(word);
                if !base.is_empty() {
                    return base.to_string();
                }
            }
        }
    }
    exe
}

fn processes(sys: &System, users: &Users, cpu: &CpuSampler) -> Vec<Proc> {
    sys.processes()
        .values()
        .filter(|p| p.thread_kind().is_none()) // skip Linux threads
        .map(|p| {
            let name = display_name(p);
            let user = p
                .user_id()
                .and_then(|u| users.get_user_by_id(u))
                .map(|u| u.name().to_string())
                .unwrap_or_else(|| "?".into());
            Proc {
                pid: p.pid().as_u32(),
                user,
                rss: p.memory(),
                cpu: cpu.get(p.pid().as_u32(), p.cpu_usage()),
                name,
                count: 1,
            }
        })
        .collect()
}

/// Fold helper processes into their parent app:
/// "Google Chrome Helper (Renderer)" -> "Google Chrome".
fn app_name(name: &str) -> String {
    let mut n = name.trim();
    if n.ends_with(')') {
        if let Some(i) = n.rfind(" (") {
            n = n[..i].trim_end();
        }
    }
    if let Some(stripped) = n.strip_suffix(" Helper") {
        n = stripped.trim_end();
    }
    n.to_string()
}

fn group(procs: Vec<Proc>) -> Vec<Proc> {
    let mut groups: HashMap<String, Proc> = HashMap::new();
    for p in procs {
        let key = app_name(&p.name);
        groups
            .entry(key.clone())
            .and_modify(|g| {
                g.rss += p.rss;
                g.cpu += p.cpu;
                g.count += 1;
                if g.user != p.user {
                    g.user = "*".into();
                }
            })
            .or_insert(Proc { name: key, ..p });
    }
    groups.into_values().collect()
}

fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default()
}

// ------------------------------------------------------------------ rendering

fn render(args: &ViewArgs, sys: &System, users: &Users, cpu: &CpuSampler) -> String {
    let cols = term_width();
    let mem = mem_info(sys);
    let mut procs = processes(sys, users, cpu);
    // Processes we can't read (other users' on macOS without root) report 0 bytes.
    let before = procs.len();
    procs.retain(|p| p.rss > 0);
    let hidden = before - procs.len();
    if args.user {
        let me = current_user();
        procs.retain(|p| p.user == me);
    }
    if let Some(f) = &args.filter {
        let f = f.to_lowercase();
        procs.retain(|p| p.name.to_lowercase().contains(&f));
    }
    if args.group {
        procs = group(procs);
    }
    procs.sort_by(|a, b| b.rss.cmp(&a.rss).then(a.pid.cmp(&b.pid)));

    let mut out = Vec::new();
    // SAFETY: geteuid has no preconditions.
    let is_root = unsafe { libc::geteuid() } == 0;
    let hint = (cfg!(target_os = "macos") && !is_root && hidden > 0 && !args.user).then(|| {
        dim(&format!(
            "  {hidden} system processes hidden (macOS restricts access); run with sudo to include them"
        ))
    });
    let frac = if mem.total > 0 {
        mem.used as f64 / mem.total as f64
    } else {
        0.0
    };
    let bw = cols.saturating_sub(40).clamp(10, 50);
    out.push(format!(
        "{} {}",
        bold(&format!("{PIG} mempig")),
        dim(&format!(" v{VERSION}"))
    ));
    out.push(String::new());
    out.push(format!(
        "  Memory  {}  {} / {}  {}",
        heat(frac, &bar(frac, bw)),
        bold(&human(mem.used)),
        human(mem.total),
        heat(frac, &format!("{:.0}%", frac * 100.0)),
    ));
    let mut detail = format!(
        "          free {}  ·  cached {}",
        human(mem.total.saturating_sub(mem.used)),
        human(mem.cached)
    );
    if mem.swap_total > 0 {
        detail += &format!(
            "  ·  swap {} / {}",
            human(mem.swap_used),
            human(mem.swap_total)
        );
    }
    out.push(dim(&detail));
    out.push(String::new());

    let top = &procs[..procs.len().min(args.num)];
    if top.is_empty() {
        out.push("  No processes matched.".into());
        out.extend(hint);
        return out.join("\n");
    }

    let pig_total: u64 = top.iter().map(|p| p.rss).sum();
    let label = if args.group { "apps" } else { "processes" };
    out.push(format!(
        "{}{}",
        bold("  The pigs"),
        dim(&format!(
            "  (top {} {label}, {} resident, {} {label} total)",
            top.len(),
            human(pig_total),
            procs.len()
        ))
    ));
    out.push(String::new());

    let idcol = if args.group { "PROCS" } else { "PID" };
    let name_w = cols.saturating_sub(62).clamp(12, 36);
    let small_bw = cols.saturating_sub(name_w + 50).clamp(6, 20);
    out.push(dim(&format!(
        "  {:>3}  {idcol:>6}  {:>7}  {:>5}  {:>5}  {:small_bw$}  NAME",
        "#", "MEM", "%", "CPU", ""
    )));
    let biggest = top[0].rss.max(1) as f64;
    for (i, p) in top.iter().enumerate() {
        let pf = if mem.total > 0 {
            p.rss as f64 / mem.total as f64
        } else {
            0.0
        };
        let ident = if args.group {
            format!("×{}", p.count)
        } else {
            p.pid.to_string()
        };
        let snouts = PIG.repeat(if pf >= 0.10 {
            3
        } else if pf >= 0.05 {
            2
        } else if pf >= 0.02 {
            1
        } else {
            0
        });
        let line = format!(
            "  {:>3}  {ident:>6}  {}  {:>4.1}%  {:>5.1}  {}  {} {snouts}",
            i + 1,
            bold(&format!("{:>7}", human(p.rss))),
            pf * 100.0,
            p.cpu,
            pink(&bar(p.rss as f64 / biggest, small_bw)),
            truncate(&p.name, name_w),
        );
        out.push(line.trim_end().to_string());
    }
    if let Some(h) = hint {
        out.push(String::new());
        out.push(h);
    }
    out.join("\n")
}

// ------------------------------------------------------------------ kill

fn kill(target: &str, force: bool, yes: bool) -> i32 {
    let mut sys = System::new();
    refresh_sys(&mut sys);
    let users = Users::new_with_refreshed_list();
    let me = std::process::id();
    let all = processes(&sys, &users, &CpuSampler::default());
    let mut victims: Vec<Proc> = match target.parse::<u32>() {
        Ok(pid) => all.into_iter().filter(|p| p.pid == pid).collect(),
        Err(_) => {
            let t = target.to_lowercase();
            all.into_iter()
                .filter(|p| {
                    p.pid != me
                        && (p.name.to_lowercase() == t || app_name(&p.name).to_lowercase() == t)
                })
                .collect()
        }
    };
    if victims.is_empty() {
        eprintln!("mempig: no process matching {target:?}");
        return 1;
    }
    victims.sort_by_key(|p| std::cmp::Reverse(p.rss));
    let total: u64 = victims.iter().map(|p| p.rss).sum();
    let (sig, signame) = if force {
        (libc::SIGKILL, "SIGKILL")
    } else {
        (libc::SIGTERM, "SIGTERM")
    };

    println!(
        "{PIG} About to send {signame} to {} process(es) freeing up to {}:",
        victims.len(),
        bold(&human(total))
    );
    for p in victims.iter().take(15) {
        println!(
            "    {:>7}  {:>7}  {:<12} {}",
            p.pid,
            human(p.rss),
            p.user,
            p.name
        );
    }
    if victims.len() > 15 {
        println!("{}", dim(&format!("    … and {} more", victims.len() - 15)));
    }

    if !yes {
        print!("Proceed? [y/N] ");
        io::stdout().flush().ok();
        let mut ans = String::new();
        io::stdin().lock().read_line(&mut ans).ok();
        if !matches!(ans.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("Aborted. The pigs live another day.");
            return 1;
        }
    }

    let mut failed = 0;
    for p in &victims {
        // SAFETY: kill(2) with a pid and signal number.
        if unsafe { libc::kill(p.pid as libc::pid_t, sig) } != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EPERM) {
                failed += 1;
                eprintln!(
                    "{}",
                    red(&format!(
                        "  permission denied: {} {} (try sudo)",
                        p.pid, p.name
                    ))
                );
            }
        }
    }
    let done = victims.len() - failed;
    if done > 0 {
        println!("Sent {signame} to {done} process(es).");
    } else {
        println!("Nothing killed.");
    }
    if failed > 0 { 1 } else { 0 }
}

// ------------------------------------------------------------------ main

fn main() {
    let cli = Cli::parse();
    COLOR.store(
        io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() && !cli.view.no_color,
        Ordering::Relaxed,
    );

    if let Some(Command::Kill { target, force, yes }) = &cli.command {
        std::process::exit(kill(target, *force, *yes));
    }

    let args = cli.view;
    let mut sys = System::new();
    let mut cpu = CpuSampler::default();
    let users = Users::new_with_refreshed_list();
    // CPU usage needs two samples to measure a delta.
    refresh(&mut sys, &mut cpu);
    std::thread::sleep(MINIMUM_CPU_UPDATE_INTERVAL.max(Duration::from_millis(250)));
    refresh(&mut sys, &mut cpu);

    match args.watch {
        None => println!("{}", render(&args, &sys, &users, &cpu)),
        Some(secs) => {
            let secs = secs.max(0.2);
            loop {
                let frame = render(&args, &sys, &users, &cpu);
                let footer = dim(&format!("  refreshing every {secs}s · ctrl-c to quit"));
                print!("\x1b[H\x1b[2J{frame}\n\n{footer}\n");
                io::stdout().flush().ok();
                std::thread::sleep(Duration::from_secs_f64(secs));
                refresh(&mut sys, &mut cpu);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests that flip the global COLOR flag must not run concurrently.
    static COLOR_LOCK: Mutex<()> = Mutex::new(());

    fn proc(pid: u32, user: &str, rss: u64, cpu: f32, name: &str) -> Proc {
        Proc {
            pid,
            user: user.into(),
            rss,
            cpu,
            name: name.into(),
            count: 1,
        }
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human(0), "0B");
        assert_eq!(human(1023), "1023B");
        assert_eq!(human(1024), "1K");
        assert_eq!(human(1536 * 1024), "1.5M");
        assert_eq!(human(24 * 1024 * 1024 * 1024), "24.0G");
        assert_eq!(human(3 * 1024u64.pow(5)), "3072.0T");
    }

    #[test]
    fn bar_has_fixed_width() {
        let _guard = COLOR_LOCK.lock().unwrap();
        COLOR.store(false, Ordering::Relaxed);
        for frac in [0.0, 0.01, 0.33, 0.5, 0.999, 1.0, 1.5, -1.0] {
            assert_eq!(bar(frac, 20).chars().count(), 20, "frac {frac}");
        }
        assert_eq!(bar(1.0, 4), "████");
        assert_eq!(bar(0.0, 4), "····");
        assert_eq!(bar(0.5, 4), "██··");
    }

    #[test]
    fn truncate_names() {
        assert_eq!(truncate("firefox", 10), "firefox");
        assert_eq!(truncate("Google Chrome Helper", 10), "Google Ch…");
        assert_eq!(truncate("Google Chrome Helper", 10).chars().count(), 10);
    }

    #[test]
    fn app_name_folds_helpers() {
        assert_eq!(app_name("Google Chrome Helper (Renderer)"), "Google Chrome");
        assert_eq!(app_name("Google Chrome Helper"), "Google Chrome");
        assert_eq!(app_name("Codex (Renderer)"), "Codex");
        assert_eq!(app_name("firefox"), "firefox");
        assert_eq!(app_name("node"), "node");
    }

    #[test]
    fn group_sums_by_app() {
        let groups = group(vec![
            proc(1, "eric", 100, 1.0, "Google Chrome"),
            proc(2, "eric", 50, 2.0, "Google Chrome Helper (Renderer)"),
            proc(3, "root", 25, 0.5, "Google Chrome Helper (GPU)"),
            proc(4, "eric", 10, 0.0, "firefox"),
        ]);
        let chrome = groups.iter().find(|g| g.name == "Google Chrome").unwrap();
        assert_eq!(chrome.rss, 175);
        assert_eq!(chrome.count, 3);
        assert!((chrome.cpu - 3.5).abs() < 1e-6);
        assert_eq!(chrome.user, "*");
        let ff = groups.iter().find(|g| g.name == "firefox").unwrap();
        assert_eq!((ff.rss, ff.count, ff.user.as_str()), (10, 1, "eric"));
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn heat_thresholds() {
        let _guard = COLOR_LOCK.lock().unwrap();
        COLOR.store(true, Ordering::Relaxed);
        assert!(heat(0.1, "x").contains("[32m"));
        assert!(heat(0.7, "x").contains("[33m"));
        assert!(heat(0.9, "x").contains("[31m"));
        COLOR.store(false, Ordering::Relaxed);
    }

    #[test]
    fn mem_info_is_sane() {
        let mut sys = System::new();
        sys.refresh_memory();
        let m = mem_info(&sys);
        assert!(m.total > 0);
        assert!(m.used > 0 && m.used <= m.total);
        assert!(m.swap_used <= m.swap_total || m.swap_total == 0);
    }
}
