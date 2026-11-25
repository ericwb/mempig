#!/usr/bin/env python3
"""mempig - see how much memory you're using, and which processes are the pigs.

Usage:
  mempig                 show total/used memory and the top 10 pigs
  mempig -n 25           show the top 25 pigs
  mempig -g              group processes by name (e.g. all Chrome helpers together)
  mempig -u              only your own processes
  mempig -w [SECS]       watch mode, refresh every SECS seconds (default 2)
  mempig kill TARGET     kill a pig by PID or by name (asks first)

Works on macOS and Linux with no dependencies beyond the Python standard library.
"""

from __future__ import annotations

import argparse
import getpass
import os
import re
import shutil
import signal
import subprocess
import sys
import time
from dataclasses import dataclass

__version__ = "0.1.0"

PIG = "\U0001F437"


# ---------------------------------------------------------------- formatting

def human(n: float) -> str:
    for unit in ("B", "K", "M", "G", "T"):
        if abs(n) < 1024 or unit == "T":
            return f"{n:.0f}{unit}" if unit in ("B", "K") else f"{n:.1f}{unit}"
        n /= 1024
    return f"{n:.1f}T"


class Color:
    enabled = sys.stdout.isatty() and os.environ.get("NO_COLOR") is None

    @classmethod
    def wrap(cls, code: str, s: str) -> str:
        return f"\033[{code}m{s}\033[0m" if cls.enabled else s

    @classmethod
    def bold(cls, s): return cls.wrap("1", s)
    @classmethod
    def dim(cls, s): return cls.wrap("2", s)
    @classmethod
    def red(cls, s): return cls.wrap("31", s)
    @classmethod
    def yellow(cls, s): return cls.wrap("33", s)
    @classmethod
    def green(cls, s): return cls.wrap("32", s)
    @classmethod
    def pink(cls, s): return cls.wrap("95", s)


def heat(frac: float, s: str) -> str:
    if frac >= 0.85:
        return Color.red(s)
    if frac >= 0.6:
        return Color.yellow(s)
    return Color.green(s)


def bar(frac: float, width: int) -> str:
    frac = max(0.0, min(1.0, frac))
    full = int(frac * width)
    partial = "▏▎▍▌▋▊▉"[int((frac * width - full) * 7)] if full < width else ""
    if frac * width - full < 1 / 7:
        partial = ""
    filled = "█" * full + partial
    return filled + Color.dim("·" * (width - len(filled)))


# ---------------------------------------------------------------- memory info

@dataclass
class MemInfo:
    total: int
    used: int
    cached: int
    swap_used: int
    swap_total: int


def _run(cmd: list[str]) -> str:
    return subprocess.run(cmd, capture_output=True, text=True, check=True).stdout


def mem_macos() -> MemInfo:
    total = int(_run(["sysctl", "-n", "hw.memsize"]).strip())
    out = _run(["vm_stat"])
    page = int(re.search(r"page size of (\d+)", out).group(1))
    stats = {
        k.strip().strip('"'): int(v.strip().rstrip("."))
        for k, v in re.findall(r"^([^:]+):\s+(\d+)\.?$", out, re.M)
    }
    g = lambda k: stats.get(k, 0) * page  # noqa: E731
    # Same formula Activity Monitor uses: app memory + wired + compressed.
    app = g("Anonymous pages") - g("Pages purgeable")
    used = app + g("Pages wired down") + g("Pages occupied by compressor")
    cached = g("File-backed pages") + g("Pages purgeable")
    swap_used = swap_total = 0
    try:
        sw = _run(["sysctl", "-n", "vm.swapusage"])
        m = re.search(r"total = ([\d.]+)M.*used = ([\d.]+)M", sw)
        if m:
            swap_total = int(float(m.group(1)) * 1024 * 1024)
            swap_used = int(float(m.group(2)) * 1024 * 1024)
    except (subprocess.CalledProcessError, FileNotFoundError):
        pass
    return MemInfo(total, min(used, total), cached, swap_used, swap_total)


def mem_linux() -> MemInfo:
    info = {}
    with open("/proc/meminfo") as f:
        for line in f:
            k, v = line.split(":", 1)
            info[k] = int(v.split()[0]) * 1024
    total = info["MemTotal"]
    avail = info.get("MemAvailable", info["MemFree"] + info.get("Cached", 0))
    cached = info.get("Cached", 0) + info.get("Buffers", 0)
    return MemInfo(
        total, total - avail, cached,
        info.get("SwapTotal", 0) - info.get("SwapFree", 0), info.get("SwapTotal", 0),
    )


def mem_info() -> MemInfo:
    if sys.platform == "darwin":
        return mem_macos()
    if sys.platform.startswith("linux"):
        return mem_linux()
    sys.exit(f"mempig: unsupported platform {sys.platform}")


# ---------------------------------------------------------------- processes

@dataclass
class Proc:
    pid: int
    user: str
    rss: int  # bytes
    cpu: float
    name: str
    count: int = 1


def processes() -> list[Proc]:
    out = _run(["ps", "-axo", "pid=,user=,rss=,%cpu=,comm="])
    procs = []
    for line in out.splitlines():
        parts = line.split(None, 4)
        if len(parts) < 5:
            continue
        pid, user, rss, cpu, comm = parts
        try:
            procs.append(Proc(int(pid), user, int(rss) * 1024, float(cpu),
                              os.path.basename(comm.strip()) or comm.strip()))
        except ValueError:
            continue
    return procs


def app_name(name: str) -> str:
    """Fold helper processes into their parent app: 'Google Chrome Helper (Renderer)' -> 'Google Chrome'."""
    name = re.sub(r"\s*\(.*\)$", "", name)
    name = re.sub(r"\s+Helper$", "", name)
    return name


def group(procs: list[Proc]) -> list[Proc]:
    groups: dict[str, Proc] = {}
    for p in procs:
        key = app_name(p.name)
        g = groups.get(key)
        if g is None:
            groups[key] = Proc(p.pid, p.user, p.rss, p.cpu, key)
        else:
            g.rss += p.rss
            g.cpu += p.cpu
            g.count += 1
            if g.user != p.user:
                g.user = "*"
    return list(groups.values())


# ---------------------------------------------------------------- rendering

def render(args) -> str:
    cols = shutil.get_terminal_size((100, 24)).columns
    mem = mem_info()
    procs = processes()
    if args.user:
        me = getpass.getuser()
        procs = [p for p in procs if p.user == me]
    if args.filter:
        rx = re.compile(args.filter, re.I)
        procs = [p for p in procs if rx.search(p.name)]
    if args.group:
        procs = group(procs)
    procs.sort(key=lambda p: p.rss, reverse=True)

    lines = []
    frac = mem.used / mem.total if mem.total else 0
    bw = max(10, min(50, cols - 40))
    lines.append(Color.bold(f"{PIG} mempig") + Color.dim(f"  v{__version__}"))
    lines.append("")
    lines.append(f"  Memory  {heat(frac, bar(frac, bw))}  "
                 f"{Color.bold(human(mem.used))} / {human(mem.total)}  "
                 f"{heat(frac, f'{frac * 100:.0f}%')}")
    lines.append(Color.dim(f"          free {human(mem.total - mem.used)}"
                           f"  ·  cached {human(mem.cached)}"
                           + (f"  ·  swap {human(mem.swap_used)} / {human(mem.swap_total)}"
                              if mem.swap_total else "")))
    lines.append("")

    top = procs[: args.num]
    if not top:
        lines.append("  No processes matched.")
        return "\n".join(lines)

    pig_total = sum(p.rss for p in top)
    label = "apps" if args.group else "processes"
    lines.append(Color.bold(f"  The pigs") + Color.dim(
        f"  (top {len(top)} {label}, {human(pig_total)} resident, "
        f"{len(procs)} {label} total)"))
    lines.append("")

    idcol = "PROCS" if args.group else "PID"
    name_w = max(12, min(36, cols - 62))
    small_bw = max(6, min(20, cols - name_w - 50))
    lines.append(Color.dim(f"  {'#':>3}  {idcol:>6}  {'MEM':>7}  {'%':>5}  {'CPU':>5}  "
                           f"{'':<{small_bw}}  NAME"))
    biggest = top[0].rss or 1
    for i, p in enumerate(top, 1):
        pf = p.rss / mem.total if mem.total else 0
        ident = f"×{p.count}" if args.group else str(p.pid)
        name = p.name if len(p.name) <= name_w else p.name[: name_w - 1] + "…"
        snouts = PIG * (3 if pf >= 0.10 else 2 if pf >= 0.05 else 1 if pf >= 0.02 else 0)
        lines.append(
            f"  {i:>3}  {ident:>6}  {Color.bold(f'{human(p.rss):>7}')}  "
            f"{pf * 100:>4.1f}%  {p.cpu:>5.1f}  "
            f"{Color.pink(bar(p.rss / biggest, small_bw))}  {name} {snouts}".rstrip()
        )
    return "\n".join(lines)


# ---------------------------------------------------------------- kill

def kill(args) -> int:
    target = args.target
    me = os.getpid()
    if target.isdigit():
        victims = [p for p in processes() if p.pid == int(target)]
    else:
        rx = target.lower()
        victims = [p for p in processes()
                   if p.pid != me and (p.name.lower() == rx or app_name(p.name).lower() == rx)]
    if not victims:
        print(f"mempig: no process matching {target!r}", file=sys.stderr)
        return 1

    victims.sort(key=lambda p: p.rss, reverse=True)
    total = sum(p.rss for p in victims)
    sig = signal.SIGKILL if args.force else signal.SIGTERM
    print(f"{PIG} About to send {sig.name} to {len(victims)} process(es) "
          f"freeing up to {Color.bold(human(total))}:")
    for p in victims[:15]:
        print(f"    {p.pid:>7}  {human(p.rss):>7}  {p.user:<12} {p.name}")
    if len(victims) > 15:
        print(Color.dim(f"    … and {len(victims) - 15} more"))

    if not args.yes:
        try:
            ans = input("Proceed? [y/N] ").strip().lower()
        except EOFError:
            ans = ""
        if ans not in ("y", "yes"):
            print("Aborted. The pigs live another day.")
            return 1

    failed = 0
    for p in victims:
        try:
            os.kill(p.pid, sig)
        except ProcessLookupError:
            pass
        except PermissionError:
            failed += 1
            print(Color.red(f"  permission denied: {p.pid} {p.name} (try sudo)"), file=sys.stderr)
    done = len(victims) - failed
    print(f"Sent {sig.name} to {done} process(es)." if done else "Nothing killed.")
    return 1 if failed else 0


# ---------------------------------------------------------------- main

def main(argv: list[str] | None = None) -> int:
    argv = sys.argv[1:] if argv is None else argv

    if argv and argv[0] == "kill":
        kp = argparse.ArgumentParser(prog="mempig kill",
                                     description="Kill a pig by PID or by process/app name.")
        kp.add_argument("target", help="PID or name (e.g. 'Slack', 'node')")
        kp.add_argument("-9", "--force", action="store_true", help="use SIGKILL instead of SIGTERM")
        kp.add_argument("-y", "--yes", action="store_true", help="don't ask for confirmation")
        return kill(kp.parse_args(argv[1:]))

    ap = argparse.ArgumentParser(
        prog="mempig",
        description="Show total and used memory, and the processes hogging it.",
        epilog="Subcommand: mempig kill <pid|name> [-9] [-y]",
    )
    ap.add_argument("-n", "--num", type=int, default=10, help="how many pigs to show (default 10)")
    ap.add_argument("-g", "--group", action="store_true", help="group processes by app name")
    ap.add_argument("-u", "--user", action="store_true", help="only show your own processes")
    ap.add_argument("-f", "--filter", metavar="REGEX", help="only show processes whose name matches")
    ap.add_argument("-w", "--watch", nargs="?", type=float, const=2.0, metavar="SECS",
                    help="refresh continuously (default every 2s)")
    ap.add_argument("--no-color", action="store_true", help="disable colors")
    ap.add_argument("-V", "--version", action="version", version=f"mempig {__version__}")
    args = ap.parse_args(argv)
    if args.no_color:
        Color.enabled = False

    if args.watch is None:
        print(render(args))
        return 0

    try:
        while True:
            frame = render(args)
            sys.stdout.write("\033[H\033[2J" + frame + "\n"
                             + Color.dim(f"\n  refreshing every {args.watch:g}s · ctrl-c to quit") + "\n")
            sys.stdout.flush()
            time.sleep(args.watch)
    except KeyboardInterrupt:
        print()
        return 0


if __name__ == "__main__":
    sys.exit(main())
