"""Safety: driving sessions not spawned here, terminal queries, resizes."""
import sys, time, json, os, subprocess
from mcpc import *

h = Home("safety")
findings = []


def note(tag, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + tag + (": " + str(detail)[:400] if detail != "" else ""))
    if not ok:
        findings.append((tag, str(detail)[:600]))


PROBE = r'''
param([string]$Out)
$e = [char]27
[Console]::Out.WriteLine('PROBE-READY')
Start-Sleep -Milliseconds 3000
[Console]::Out.Write("$e[c")
[Console]::Out.Write("$e]11;?$e\")
[Console]::Out.Write("$e]10;?$e\")
[Console]::Out.Write("$e]4;1;?$e\")
[Console]::Out.Write("$e[6n")
[Console]::Out.Write("$e[18t")
[Console]::Out.Write("$e[>c")
[Console]::Out.Write("$e[?2026`$p")
[Console]::Out.WriteLine('')
[Console]::Out.WriteLine('PROBE-ASKED')
Start-Sleep -Milliseconds 3000
$s = ''
while ([Console]::KeyAvailable) { $k = [Console]::ReadKey($true); $s += ('{0:x2}' -f [int]$k.KeyChar) }
[IO.File]::WriteAllText($Out, $s)
[Console]::Out.WriteLine("PROBE-REPLY:[$s]")
'''


def unhex(s):
    try:
        return bytes.fromhex(s).decode("latin-1")
    except Exception:
        return s


try:
    probe = os.path.join(h.dir, "probe.ps1")
    open(probe, "w").write(PROBE)

    # ---- positive control: the same program under a terminal that DOES answer (headless `shot --cmd`)
    ctl_out = os.path.join(h.dir, "reply-control.txt")
    r = h.run("shot", "--cmd", f"pwsh.exe -NoLogo -NoProfile -File {probe} {ctl_out}", "--wait", "12", "--out", os.path.join(h.dir, "ctl.png"), "--size", "1200x400", timeout=60)
    control = open(ctl_out).read() if os.path.exists(ctl_out) else None
    print("control (answering terminal) reply:", repr(unhex(control)) if control is not None else None, "| shot stdout:", r.stdout[-200:])

    owner = Client(h)
    owner.handshake()

    # ---- baseline: the probe in an agent session with nobody else looking
    base_out = os.path.join(h.dir, "reply-base.txt")
    f, _ = owner.ok("spawn", {"command": "pwsh.exe", "args": ["-NoLogo", "-NoProfile", "-File", probe, base_out], "cols": 100, "rows": 30})
    sid0 = f["session"]
    w, _ = owner.ok("wait", {"session": sid0, "until": {"regex": r"^PROBE-REPLY:\[(.*)\]"}, "timeout_ms": 30000})
    base = open(base_out).read() if os.path.exists(base_out) else None
    print("baseline (server that spawned it, only waiting) reply:", repr(unhex(base)), w.get("reason"))

    # ---- attack: everyone looks with every tool while the program asks
    atk_out = os.path.join(h.dir, "reply-attack.txt")
    f, _ = owner.ok("spawn", {"command": "pwsh.exe", "args": ["-NoLogo", "-NoProfile", "-File", probe, atk_out], "cols": 100, "rows": 30})
    sid = f["session"]
    journal = f["journal"]
    guest = Client(h); guest.handshake()
    driver = Client(h, "--drive-all"); driver.handshake()
    owner.ok("wait", {"session": sid, "until": {"regex": r"^PROBE-READY"}, "timeout_ms": 30000})
    before = [s for s in h.sessions()["sessions"] if s["id"] == sid][0]
    # the driver attaches now (a no-op key that types nothing visible would still be input; use a wait first, then a real attach by send of nothing harmful)
    t_end = time.time() + 7.5
    n = 0
    attached = False
    while time.time() < t_end:
        for c in (owner, guest, driver):
            c.ok("screen", {"session": sid})
            c.ok("read", {"session": sid, "cursor": 0})
            c.ok("read", {"session": sid, "cursor": 0, "mode": "plain"})
            c.ok("read", {"session": sid, "cursor": 0, "mode": "raw"})
            c.ok("wait", {"session": sid, "until": {"quiet_ms": 20}, "timeout_ms": 60, "cursor": 0})
            c.ok("screen", {"session": sid, "at": 3.5})
            c.ok("shot", {"session": sid})
            n += 1
        if not attached and time.time() > t_end - 6.0:
            # attach the --drive-all server for real before the questions are asked (they come 3 s after READY)
            res, facts, _ = driver.call("kill", {"session": 99999})  # no such session: nothing happens
            # hands() only attaches on send/kill; attach by sending to *this* session would be input.
            # Instead prove attach + liveness with a second session below; here we attach via a zero-effect key: none exists,
            # so the attach is done on a separate check further down.
            attached = True
    w, _ = owner.ok("wait", {"session": sid, "until": {"regex": r"^PROBE-REPLY:\[(.*)\]"}, "timeout_ms": 30000, "cursor": 0})
    atk = open(atk_out).read() if os.path.exists(atk_out) else None
    print(f"attack ({n} rounds of screen/read/wait/shot by three servers) reply:", repr(unhex(atk)))
    after = [s for s in h.sessions()["sessions"] if s["id"] == sid]
    note("looking never answers a query (reply identical to baseline)", atk == base, f"base={base!r} attack={atk!r}")
    if control is not None:
        note("control differs: an answering terminal does reply", control != base, f"control={unhex(control)!r}")
    for marker, what in [("5d31313b", "OSC 11 colour answer"), ("5d31303b", "OSC 10 colour answer"), ("1b5b3f3663", "alacritty DA answer ESC[?6c")]:
        note(f"no {what} in attack reply", marker not in (atk or ""), atk)
    resizes = [struct.unpack("<HH", p[:4]) for k, _, p in journal_records(journal) if k == 2]
    note("journal holds exactly the spawn size", resizes == [(100, 30)], resizes)

    # ---- attach for real with --drive-all while a program asks questions
    atk2_out = os.path.join(h.dir, "reply-attack2.txt")
    f, _ = owner.ok("spawn", {"command": "pwsh.exe", "args": ["-NoLogo", "-NoProfile", "-File", probe, atk2_out], "cols": 90, "rows": 28})
    sid2 = f["session"]; journal2 = f["journal"]
    owner.ok("wait", {"session": sid2, "until": {"regex": r"^PROBE-READY"}, "timeout_ms": 30000})
    # the program is sleeping 3 s before it asks: typing a space now attaches the driver (input is not read until ReadKey loop... it IS read by KeyAvailable).
    # So attach by 'kill'-less means is impossible; accept one typed space and account for it (hex 20) in the reply.
    f, _ = driver.ok("send", {"session": sid2, "text": " "})
    lst = [s for s in h.sessions()["sessions"] if s["id"] == sid2][0]
    note("--drive-all attach counts as a second client", lst["windows"] == 2, lst)
    note("--drive-all attach did not resize", (lst["cols"], lst["rows"]) == (90, 28), lst)
    t_end = time.time() + 5.0
    while time.time() < t_end:
        driver.ok("screen", {"session": sid2}); driver.ok("read", {"session": sid2, "cursor": 0}); driver.ok("shot", {"session": sid2})
        driver.ok("wait", {"session": sid2, "until": {"quiet_ms": 20}, "timeout_ms": 60})
    w, _ = owner.ok("wait", {"session": sid2, "until": {"regex": r"^PROBE-REPLY:\[(.*)\]"}, "timeout_ms": 30000, "cursor": 0})
    atk2 = open(atk2_out).read() if os.path.exists(atk2_out) else None
    print("attached --drive-all reply:", repr(unhex(atk2)))
    note("an attached --drive-all server answers nothing (reply = baseline + the one typed space)", atk2 is not None and atk2.replace("20", "", 1) == base, f"base={base!r} attack2={atk2!r}")
    resizes2 = [struct.unpack("<HH", p[:4]) for k, _, p in journal_records(journal2) if k == 2]
    note("journal of the attached session holds exactly the spawn size", resizes2 == [(90, 28)], resizes2)

    # ---- driving without the right
    f, _ = owner.ok("spawn", {"command": "cmd.exe /d", "cols": 100, "rows": 30})
    victim = f["session"]; vj = f["journal"]
    owner.ok("wait", {"session": victim, "until": {"regex": r">\s*$"}, "timeout_ms": 20000})
    f, _ = guest.ok("spawn", {"command": "cmd.exe /d"})
    own = f["session"]
    guest.ok("wait", {"session": own, "until": {"regex": r">\s*$"}, "timeout_ms": 20000})
    tries = [
        ("send", {"session": victim, "text": "echo intruder-1", "enter": True}),
        ("send", {"session": victim, "keys": ["Ctrl+C"]}),
        ("send", {"session": victim, "enter": True}),
        ("kill", {"session": victim}),
    ]
    for tool, args in tries:
        why = guest.call(tool, args)
        note(f"guest {tool} {json.dumps(args)[:50]} refused", why[0] is not None and why[0].get("isError") is True, why[1])
    # after looking at it in every way, still refused
    for tool, args in [("screen", {"session": victim}), ("read", {"session": victim}), ("shot", {"session": victim}), ("wait", {"session": victim, "until": {"quiet_ms": 10}, "timeout_ms": 100})]:
        guest.ok(tool, args)
    for tool, args in tries:
        why = guest.call(tool, args)
        note(f"guest {tool} after looking refused", why[0] is not None and why[0].get("isError") is True, why[1])
    # concurrent: its own session is driven while the victim is asked for at the same instant
    ids = []
    for i in range(40):
        ids.append((guest.start_call("send", {"session": own, "text": f"echo own-{i}", "enter": True}), True))
        ids.append((guest.start_call("send", {"session": victim, "text": f"echo intruder-c{i}", "enter": True}), False))
        ids.append((guest.start_call("kill", {"session": victim}), False))
    bad = 0
    for id, should in ids:
        res, facts, _ = guest.finish(id, 30)
        if res is None or (res.get("isError") is False) != should:
            bad += 1
    note("120 concurrent sends: own accepted, victim refused, every one", bad == 0, bad)
    time.sleep(0.5)
    _, all_text = owner.ok("read", {"session": victim, "cursor": 0, "mode": "plain"})
    note("nothing typed by the guest reached the victim", "intruder" not in all_text, all_text[-300:])
    lst = [s for s in h.sessions()["sessions"] if s["id"] == victim][0]
    note("victim still running, one client, same size", (lst["windows"], lst["cols"], lst["rows"]) == (1, 100, 30), lst)
    _, own_text = guest.ok("read", {"session": own, "cursor": 0, "mode": "plain"})
    note("the guest's own session got all 40 commands", all(f"own-{i}" in own_text for i in range(40)), own_text[-200:])

    # the guest goes away: the victim must survive, its own session must end
    guest.close()
    t = time.time()
    gone = None
    while time.time() - t < 12:
        ss = {s["id"] for s in h.sessions()["sessions"]}
        if own not in ss:
            gone = time.time() - t
            break
        time.sleep(0.1)
    note("guest's own session ended after the guest left", gone is not None, f"{gone and round(gone,2)} s")
    ss = {s["id"] for s in h.sessions()["sessions"]}
    note("victim survived the guest leaving", victim in ss, ss)
    # the driver attaches to the victim, then leaves: the victim must survive (its owner is still connected)
    driver.ok("send", {"session": victim, "text": "echo driven", "enter": True})
    owner.ok("wait", {"session": victim, "until": {"regex": "^driven"}, "timeout_ms": 10000})
    driver.close()
    time.sleep(1.0)
    lst = [s for s in h.sessions()["sessions"] if s["id"] == victim]
    note("victim survived an attached --drive-all server leaving", len(lst) == 1 and lst[0]["windows"] == 1, lst)
    rz = [struct.unpack("<HH", p[:4]) for k, _, p in journal_records(vj) if k == 2]
    note("victim was never resized", rz == [(100, 30)], rz)

    # the owner goes: everything it spawned ends
    owner.close()
    t = time.time(); left = None
    while time.time() - t < 15:
        left = h.sessions()["sessions"]
        if not left:
            break
        time.sleep(0.1)
    note("all agent sessions ended after their servers left", left == [], f"{round(time.time()-t,2)} s; left={[s['id'] for s in left]}")
finally:
    h.close()
print("\nFINDINGS:", json.dumps(findings, indent=1))
