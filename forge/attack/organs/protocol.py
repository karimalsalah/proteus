"""Protocol attacks: malformed input, huge messages, unknown methods/tools, wrong types, calls before initialize."""
import sys, time, json
from mcpc import *

h = Home("protocol")
findings = []


def note(tag, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + tag + (": " + str(detail)[:300] if detail != "" else ""))
    if not ok:
        findings.append((tag, str(detail)[:500]))


def short(v):
    s = json.dumps(v)
    return s[:260]


try:
    c = Client(h)
    # --- E. before initialize -------------------------------------------------
    r = c.request("tools/list")
    note("tools/list before initialize answers", r is not None and "result" in r, short(r)[:80])
    res, facts, body = c.call("sessions", {})
    note("tools/call before initialize answers", res is not None, short(facts))
    r = c.request("ping")
    note("ping before initialize", r and r.get("result") == {})

    # --- A. malformed lines ---------------------------------------------------
    cases = [
        (b"this is not json\n", -32700),
        (b'{"jsonrpc":"2.0","id":1,"method":"ping"\n', -32700),
        (b"[]\n", -32600),
        (b"42\n", -32600),
        (b'"str"\n', -32600),
        (b"null\n", -32600),
        (b"{" * 5000 + b"\n", -32700),
        (b"[" * 200000 + b"\n", -32700),
        (b'{"jsonrpc":"2.0","id":1,"method":"ping"}{"jsonrpc":"2.0","id":2,"method":"ping"}\n', -32700),
        (b"\x00\x00\x00\n", -32700),
    ]
    for data, want in cases:
        c.raw(data)
        m = c.other(5)
        got = (m or {}).get("error", {}).get("code")
        note(f"malformed {data[:30]!r} -> {want}", got == want, short(m))
    note("server alive after malformed lines", c.alive())

    # blank lines are ignored
    c.raw(b"\n\r\n   \n\t\n")
    r = c.request("ping")
    note("blank lines ignored, ping still answered", r and r.get("result") == {})

    # odd ids
    # (An id beyond 2^64 comes back as a float, 1.1805916207174113e21 for 2^70: not tried here.)
    for idv in [{"a": 1}, 1.5, True, "s", [1], -7, 2**53]:
        c.write({"jsonrpc": "2.0", "id": idv, "method": "ping"})
        m = c.answer(idv, 5)
        note(f"id {idv!r} echoed", m is not None and m.get("id") == idv, short(m))
    c.raw(b'{"id":7,"method":"ping"}\n')
    note("no jsonrpc member still answered or refused", c.answer(7, 5) is not None)
    c.raw(b'{"jsonrpc":"2.0","id":8,"method":42}\n')
    m = c.answer(8, 5)
    note("method not a string -> -32600", m and m.get("error", {}).get("code") == -32600, short(m))
    for i, params in enumerate(["x", [1, 2], 5, True, None]):
        m = c.request("tools/call", params, id=100 + i, timeout=5)
        note(f"tools/call params={params!r}", m is not None and "error" in m, short(m))
    m = c.request("tools/call", {"name": 5, "arguments": {}}, timeout=5)
    note("tool name not a string", m is not None and m.get("error", {}).get("code") == -32602, short(m))
    m = c.request("tools/call", {"name": "read", "arguments": "x"}, timeout=5)
    note("arguments a string", m is not None and m.get("error", {}).get("code") == -32602, short(m))
    m = c.request("initialize", "nonsense", timeout=5)
    note("initialize with string params answers", m is not None and "result" in m, short(m)[:120])

    # --- invalid UTF-8 on the wire ---------------------------------------------
    c.raw(b'{"jsonrpc":"2.0","id":11,"method":"ping","params":{"x":"\xff\xfe"}}\n')
    m11 = c.answer(11, 3)
    m_other = c.other(2) if m11 is None else None
    time.sleep(0.5)
    note("invalid UTF-8 line does not end the server", c.alive(), f"answer={short(m11)} other={short(m_other)} exit={c.p.poll()}")
    if not c.alive():
        c = Client(h)
    r = c.request("ping", timeout=5)
    note("ping after invalid UTF-8", r is not None and r.get("result") == {}, short(r))

    # --- B. a 10 MB message ----------------------------------------------------
    big = "x" * (10 << 20)
    t = time.time()
    r = c.request("ping", {"pad": big}, timeout=60)
    note("10 MB ping", r is not None and r.get("result") == {}, f"{(time.time()-t)*1000:.0f} ms")
    t = time.time()
    c.raw(b"y" * (10 << 20) + b"\n")
    m = c.other(30)
    note("10 MB of garbage -> parse error", m and m.get("error", {}).get("code") == -32700, f"{(time.time()-t)*1000:.0f} ms; reply {len(json.dumps(m)) if m else 0} bytes")
    t = time.time()
    res, facts, body = c.call("spawn", {"command": big}, timeout=60)
    note("10 MB command refused, not crashed", res is not None and res.get("isError") is True, f"{(time.time()-t)*1000:.0f} ms; {str(facts)[:120]}")
    note("server alive after 10 MB messages", c.alive(), rss_mb(c.p.pid))

    # --- C. unknown methods and tools -----------------------------------------
    for method in ["resources/list", "prompts/list", "tools/call2", "", "server/discover", "completion/complete", "logging/setLevel", "notifications/initialized"]:
        m = c.request(method, {}, timeout=5)
        code = (m or {}).get("error", {}).get("code")
        note(f"method {method!r}", m is not None and code in (-32601, -32602), short(m)[:160])
    for tool in ["format_disk", "", "Spawn", "sessions ", "../kill"]:
        m = c.request("tools/call", {"name": tool, "arguments": {}}, timeout=5)
        note(f"tool {tool!r} -> -32602", (m or {}).get("error", {}).get("code") == -32602, short(m)[:160])

    # --- D. wrong argument types ----------------------------------------------
    c.handshake()
    facts, _ = c.ok("spawn", {"command": "cmd.exe /d", "cols": 100, "rows": 30})
    sid = facts["session"]
    w, _ = c.ok("wait", {"session": sid, "until": {"regex": r">\s*$"}, "timeout_ms": 20000})
    print("spawn to prompt ms:", w.get("elapsed_ms"), w.get("reason"))
    bad = [
        ("send", {"session": sid, "text": 5}),
        ("send", {"session": sid, "keys": [5, None]}),
        ("send", {"session": sid}),
        ("send", {"session": str(sid), "text": "x"}),
        ("send", {"session": -1, "text": "x"}),
        ("send", {"session": 1e30, "text": "x"}),
        ("send", {"session": 2**64 - 1, "text": "x"}),
        ("send", {"session": 0, "text": "x"}),
        ("send", {"session": None, "text": "x"}),
        ("send", {"session": [sid], "text": "x"}),
        ("send", {"session": sid, "keys": ["NoSuchKey"]}),
        ("send", {"session": sid, "keys": ["Ctrl+Hyper+x"]}),
        ("wait", {"session": sid, "until": "x"}),
        ("wait", {"session": sid, "until": {"regex": 5}}),
        ("wait", {"session": sid, "until": {}}),
        ("wait", {"session": sid}),
        ("wait", {"session": sid, "until": {"regex": "(unclosed"}}),
        ("wait", {"session": sid, "until": {"quiet_ms": -5}}),
        ("wait", {"session": sid, "until": {"exit": "yes"}}),
        ("read", {"session": sid, "mode": "bogus"}),
        ("read", {"session": sid, "cursor": 10**18}),
        ("read", {"session": sid, "lines": [0, 5], "mode": "plain"}),
        ("read", {"session": sid, "lines": [5, 1], "mode": "plain"}),
        ("read", {"session": sid, "lines": ["a", "b"], "mode": "plain"}),
        ("read", {"session": sid, "lines": [1], "mode": "plain"}),
        ("screen", {"session": sid, "at": -1}),
        ("screen", {}),
        ("shot", {"session": "x"}),
        ("kill", {}),
        ("spawn", {"command": "definitely-not-a-program-xyz-123"}),
        ("spawn", {"command": "cmd.exe", "cwd": "Z:\\no\\such\\dir"}),
    ]
    for tool, args in bad:
        res, facts, body = c.call(tool, args, timeout=20)
        ok = res is not None and res.get("isError") is True
        note(f"refused {tool} {json.dumps(args)[:70]}", ok, facts if ok else short(res))
    # Types that are silently defaulted: they must at least answer and not crash.
    lenient = [
        ("wait", {"session": sid, "until": {"quiet_ms": 100}, "timeout_ms": "5"}),
        ("wait", {"session": sid, "until": {"quiet_ms": 100}, "timeout_ms": -1}),
        ("wait", {"session": sid, "until": {"quiet_ms": 2**64 - 1}, "timeout_ms": 300}),
        ("wait", {"session": sid, "until": {"quiet_ms": 100}, "timeout_ms": 2**64 - 1}),
        ("wait", {"session": sid, "until": {"regex": "zzz"}, "timeout_ms": 0}),
        ("wait", {"session": sid, "until": {"regex": "zzz"}, "timeout_ms": 200, "cursor": 10**18}),
        ("wait", {"session": sid, "until": {"regex": "zzz"}, "timeout_ms": 200, "cursor": -4}),
        ("read", {"session": sid, "mode": 5}),
        ("read", {"session": sid, "max_bytes": 0, "cursor": 0}),
        ("read", {"session": sid, "max_bytes": -1, "cursor": 0}),
        ("read", {"session": sid, "max_bytes": 2**64 - 1, "cursor": 0}),
        ("read", {"session": sid, "head": 0, "tail": 0, "cursor": 0}),
        ("read", {"session": sid, "head": 2**64 - 1, "tail": 2**64 - 1, "cursor": 0}),
        ("read", {"session": sid, "lines": [1, 10**18], "mode": "plain", "cursor": 0}),
        ("read", {"session": sid, "lines": [10**18, 10**18], "mode": "plain", "cursor": 0}),
        ("read", {"session": sid, "lines": "x", "cursor": 0}),
        ("read", {"session": sid, "mode": "raw", "cursor": 0, "max_bytes": 1}),
        ("screen", {"session": sid, "at": "x"}),
        ("screen", {"session": sid, "at": 1e300}),
        ("screen", {"session": sid, "at": 0}),
        ("shot", {"session": sid, "scale": 0}),
        ("shot", {"session": sid, "scale": 1e9}),
        ("shot", {"session": sid, "scale": "x", "exact": "yes"}),
        ("send", {"session": sid, "keys": "Escape"}),
        ("send", {"session": sid, "keys": [""]}),
        ("send", {"session": sid, "keys": ["Ctrl+"]}),
        ("send", {"session": sid, "keys": ["Escape"], "text": 7, "enter": "no"}),
        ("spawn", {"command": "cmd.exe /d /c echo sized", "cols": 0, "rows": 0}),
        ("spawn", {"command": "cmd.exe /d /c echo sized", "cols": 10**18, "rows": -3}),
        ("spawn", {"command": "cmd.exe", "args": "notalist"}),
        ("spawn", {"command": "cmd.exe", "args": [1, 2, "/d", "/c", "echo args-ok"], "cwd": 5}),
    ]
    for tool, args in lenient:
        t = time.time()
        res, facts, body = c.call(tool, args, timeout=40)
        ok = res is not None
        f = facts if not isinstance(facts, dict) else {k: facts[k] for k in list(facts)[:9]}
        note(f"answers {tool} {json.dumps(args)[:70]}", ok, f"isError={res.get('isError') if res else None} {(time.time()-t)*1000:.0f}ms {json.dumps(f)[:200]}")
    note("server alive after wrong types", c.alive())
    # what did those odd keys actually type?
    time.sleep(0.3)
    facts, body = c.ok("screen", {"session": sid})
    print("screen after odd keys:", repr(body[-200:]))
    facts, body = c.ok("sessions", {})
    print("sessions now:", json.dumps(facts)[:600])
    print("server rss/peak MB:", rss_mb(c.p.pid))
    closed = c.close()
    note("server leaves when stdin closes", closed)
    # A session spawned within the last 3 s is still inside the console host's start-up wait and ends when that is over.
    t = time.time()
    left = h.sessions()
    while left and left.get("sessions") and time.time() - t < 6:
        time.sleep(0.1)
        left = h.sessions()
    note("no session outlives the server", left is not None and left.get("sessions") == [], f"{time.time() - t:.1f} s; {short(left)[:300]}")
finally:
    h.close()
print("\nFINDINGS:", json.dumps(findings, indent=1))
