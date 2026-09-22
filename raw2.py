import tomllib, os, http.client, json, time, urllib.parse

home = os.path.expanduser("~")
auth = tomllib.load(open(os.path.join(home, ".tabit", "auth.toml"), "rb"))["providers"]["kimi-coding"]
prov = tomllib.load(open(os.path.join(home, ".tabit", "providers.toml"), "rb"))["providers"]["kimi-coding"]
key = auth["api_key"]
base = prov["base_url"].rstrip("/")
model = prov["models"][0] if isinstance(prov["models"][0], str) else prov["models"][0].get("id")
u = urllib.parse.urlparse(base)
path = u.path + "/v1/messages"
print("host:", u.hostname, "| path:", path, "| model:", model)

body = json.dumps({"model": model, "max_tokens": 2000, "stream": True, "messages": [{"role": "user", "content": "Write a detailed 600-word technical explanation of how a river ecosystem works."}]})

for label, extra in [("minimal", {}), ("identity-encoding", {"Accept-Encoding": "identity"})]:
    headers = {"x-api-key": key, "anthropic-version": "2023-06-01", "content-type": "application/json", **extra}
    conn = http.client.HTTPSConnection(u.hostname, u.port or 443, timeout=180)
    t0 = time.time()
    conn.request("POST", path, body=body, headers=headers)
    resp = conn.getresponse()
    print(f"\n=== {label} :: {resp.status} {resp.getheader('content-type')} enc={resp.getheader('content-encoding')}")
    if resp.status != 200:
        print(resp.read()[:200]); conn.close(); continue
    arrivals = []
    while True:
        line = resp.readline()
        if not line: break
        arrivals.append((time.time() - t0, len(line)))
        if len(arrivals) > 500: break
    conn.close()
    gaps = [(arrivals[i-1][0], arrivals[i][0]-arrivals[i-1][0]) for i in range(1, len(arrivals)) if arrivals[i][0]-arrivals[i-1][0] > 0.8]
    print(f"lines: {len(arrivals)}, first {arrivals[0][0]:.2f}s last {arrivals[-1][0]:.2f}s")
    print(f"gaps > 0.8s: {[(f'{a:.1f}s', f'{g:.1f}s') for a, g in gaps][:10]}")
