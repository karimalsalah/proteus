"""Sessions whose server leaves during the 3 s start-up stall: do they end, and when?"""
import sys, time, json
from mcpc import *

h = Home("leaving")
try:
    for label, cmd, delay in [("one-shot command, server leaves 0.2 s after spawn", "cmd.exe /d /c echo x", 0.2),
                              ("interactive shell, server leaves 0.2 s after spawn", "cmd.exe /d", 0.2),
                              ("interactive shell, server leaves 1.5 s after spawn", "cmd.exe /d", 1.5),
                              ("interactive shell, server leaves 4 s after spawn (prompt shown)", "cmd.exe /d", 4.0),
                              ("interactive pwsh, server leaves 0.2 s after spawn", "pwsh.exe -NoLogo -NoProfile", 0.2)]:
        c = Client(h); c.handshake()
        f, _ = c.ok("spawn", {"command": cmd})
        sid = f["session"]
        time.sleep(delay)
        c.close()
        t = time.perf_counter()
        gone = None
        while time.perf_counter() - t < 20:
            ss = [s["id"] for s in h.sessions()["sessions"]]
            if sid not in ss:
                gone = time.perf_counter() - t
                break
            time.sleep(0.05)
        print("MEASURE", json.dumps({"what": "agent session gone after its server left", "case": label, "seconds": None if gone is None else round(gone, 2)}))
finally:
    h.close()
