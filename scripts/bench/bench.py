"""Measures memory, CPU and wakeups for SwiftSpot, Spotifast and Spotify.

Each app in /Applications runs alone, signed in, with its window open:
30 s to settle, 60 s idle, then 60 s playing. The Mac is muted meanwhile.
Usage: python3 scripts/bench/bench.py [SwiftSpot,Spotifast,Spotify]
"""

import json, os, re, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
APPS = sys.argv[1].split(",") if len(sys.argv) > 1 else ["SwiftSpot", "Spotifast", "Spotify"]
SETTLE, WINDOW = 30, 60
OUT = os.path.join(HERE, "results.jsonl")


def build_tools():
    for tool in ("playkey", "playing"):
        binary = os.path.join(HERE, tool)
        if not os.path.exists(binary):
            subprocess.run(["swiftc", "-O", binary + ".swift", "-o", binary], check=True)


def say(text):
    print(text, flush=True)


def run(*args):
    return subprocess.run(args, capture_output=True, text=True).stdout


def main_pid(app):
    exe = f"/Applications/{app}.app/Contents/MacOS/{app}"
    for line in run("ps", "-axo", "pid=,command=").split("\n"):
        parts = line.strip().split(None, 1)
        if len(parts) == 2 and parts[1].split(" --")[0] == exe:
            return int(parts[0])
    return None


def tree(root):
    kids = {}
    for line in run("ps", "-axo", "pid=,ppid=").split("\n"):
        if line.strip():
            p, pp = map(int, line.split())
            kids.setdefault(pp, []).append(p)
    pids, todo = [], [root]
    while todo:
        p = todo.pop()
        pids.append(p)
        todo += kids.get(p, [])
    return pids


def cpu_seconds(pids):
    total = 0.0
    for t in run("ps", "-o", "time=", "-p", ",".join(map(str, pids))).split("\n"):
        m = re.match(r"\s*(?:(\d+):)?(\d+):(\d+(?:\.\d+)?)", t)
        if m:
            total += int(m.group(1) or 0) * 3600 + int(m.group(2)) * 60 + float(m.group(3))
    return total


def wakeups_threads(pids):
    out = run("top", "-l", "1", "-stats", "pid,idlew,th", *sum([["-pid", str(p)] for p in pids], []))
    w = t = 0
    for line in out.split("\n"):
        parts = line.split()
        if len(parts) >= 3 and parts[0].isdigit() and int(parts[0]) in pids:
            w += int(parts[1].rstrip("+"))
            t += int(parts[2].split("/")[0])
    return w, t


def footprint_mb(pids):
    total = 0.0
    for p in pids:
        m = re.search(r"phys_footprint:\s*([\d.]+)\s*([KMG]B)", run("footprint", str(p)))
        if m:
            v = float(m.group(1))
            total += v / 1024 if m.group(2) == "KB" else v * 1024 if m.group(2) == "GB" else v
    return total


def rss_mb(pids):
    return sum(int(x) for x in run("ps", "-o", "rss=", "-p", ",".join(map(str, pids))).split()) / 1024


def audio_active(pids):
    playing = {int(x) for x in run(os.path.join(HERE, "playing")).split()}
    return bool(playing & set(pids))


def measure(pid):
    pids = tree(pid)
    c0, (w0, _) = cpu_seconds(pids), wakeups_threads(pids)
    time.sleep(WINDOW)
    pids = tree(pid)
    c1, (w1, threads) = cpu_seconds(pids), wakeups_threads(pids)
    return {
        "processes": len(pids),
        "footprint_mb": round(footprint_mb(pids), 1),
        "rss_mb": round(rss_mb(pids), 1),
        "threads": threads,
        "cpu_ms_per_min": round((c1 - c0) * 1000 * 60 / WINDOW),
        "wakeups_per_min": round((w1 - w0) * 60 / WINDOW),
    }


def quit_all():
    for app in ["SwiftSpot", "Spotifast", "Spotify"]:
        if main_pid(app):
            run("osascript", "-e", f'quit app "{app}"')
    for _ in range(40):
        if not any(main_pid(a) for a in ["SwiftSpot", "Spotifast", "Spotify"]):
            return
        time.sleep(0.5)
    for app in ["SwiftSpot", "Spotifast", "Spotify"]:
        run("pkill", "-x", app)
    time.sleep(2)


build_tools()
was_muted = run("osascript", "-e", "output muted of (get volume settings)").strip()
run("osascript", "-e", "set volume output muted true")
try:
    for app in APPS:
        quit_all()
        size = run("du", "-sk", f"/Applications/{app}.app").split()[0]
        run("open", "-a", f"/Applications/{app}.app")
        pid = None
        for _ in range(60):
            pid = main_pid(app)
            if pid:
                break
            time.sleep(0.5)
        say(f"{app}: launched, settling {SETTLE}s")
        time.sleep(SETTLE)
        idle = measure(pid)
        say(f"{app}: idle done")
        if app == "Spotify":
            run("osascript", "-e", 'tell application "Spotify" to play')
        else:
            run(os.path.join(HERE, "playkey"))
        started = False
        for i in range(90):
            if audio_active(tree(pid)):
                started = True
                break
            if i == 10:
                say(f"PRESS PLAY in {app} now")
            time.sleep(1)
        if not started:
            say(f"{app}: playback never started, skipping playing test")
            play = None
        else:
            time.sleep(5)
            play = measure(pid)
            if app == "Spotify":
                run("osascript", "-e", 'tell application "Spotify" to pause')
            else:
                run(os.path.join(HERE, "playkey"))
        result = {"app": app, "bundle_mb": round(int(size) / 1024, 1), "idle": idle, "playing": play}
        with open(OUT, "a") as f:
            f.write(json.dumps(result) + "\n")
        say(f"RESULT {json.dumps(result)}")
    quit_all()
finally:
    if was_muted != "true":
        run("osascript", "-e", "set volume output muted false")
    say("DONE")
