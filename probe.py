import subprocess, threading, time, json, queue

proc = subprocess.Popen(
    [r"C:\Users\Jerry\Projects\agents\tabit-tui-alt\tui-alt\dist\pkg\tabit-core.exe", "--json"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)

start = time.time()
lines = queue.Queue()
session_box = []

def reader():
    for line in proc.stdout:
        t = time.time() - start
        lines.put((t, line))
        try:
            f = json.loads(line)
        except Exception:
            continue
        if f.get("type") == "session_opened" and not session_box:
            session_box.append(f["id"])

t = threading.Thread(target=reader); t.start()
proc.stdin.write(json.dumps({"type":"initialize","protocol_version":16,"replay":True})+"\n"); proc.stdin.flush()

deadline = time.time() + 10
while not session_box and time.time() < deadline:
    time.sleep(0.05)
sid = session_box[0]
proc.stdin.write(json.dumps({"type":"message","session":sid,"text":"Write a 150-word story about a river."})+"\n"); proc.stdin.flush()

events = []
deadline = time.time() + 75
while time.time() < deadline:
    try:
        t, line = lines.get(timeout=1)
    except queue.Empty:
        if any(e[2] in ("run_finished",) for e in events): break
        continue
    try: f = json.loads(line)
    except Exception: continue
    if f.get("type") == "text_delta":
        events.append((t, len(f["text"]), "delta"))
    elif f.get("type") in ("turn_committed", "run_finished", "turn_started", "completion_call"):
        events.append((t, 0, f["type"]))
    if f.get("type") == "run_finished": break

print(f"{'t(s)':>7} {'chars':>6}  gap")
prev = None
for t, n, what in events:
    gap = "" if prev is None else f"{t-prev:5.2f}"
    print(f"{t:7.2f} {n:6}  {gap:>7}  {what}")
    prev = t
proc.kill()
