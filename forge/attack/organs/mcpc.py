r"""A small MCP client for attacking `proteus mcp` in isolation (the review of lane organs, 2026-11-16).

    cargo build --release
    python forge\attack\organs\protocol.py     malformed input, 10 MB messages, unknown methods and tools, wrong types
    python forge\attack\organs\safety.py       driving without the right; terminal queries (with a control); resizes
    python forge\attack\organs\waits.py        timeouts early or late, exit while waiting, hostile patterns, concurrency
    python forge\attack\organs\lean.py         binary output, errors in the collapsed middle, page cursors, a 50 MB burst
    python forge\attack\organs\captures.py     lean against raw on captures made on the spot; memory of screen; wake-up
    python forge\attack\organs\leaving.py      how long a session outlives a server that leaves
    python forge\attack\organs\lateness.py     late timeouts: the server's doing or the machine's
    python forge\attack\organs\dirnotify.py    directory change notifications for a file its writer holds open
    python forge\attack\organs\turn.py         a pattern quick on one text and slow on the next: is the timeout kept
    python forge\attack\organs\more.py         an error in the middle of a long line; an endless message; rows repeated away; looking during a flood
    python forge\attack\organs\exits.py        how long the server takes to leave, and whether a picture drawn before matters
    python forge\attack\organs\looking.py      does a second server's looking slow the session looked at

Each prints PASS / FAIL lines and MEASURE lines of JSON. Nothing here opens a window. Every process
started runs from a copy of target\release\proteus.exe in dist\, against its own data folder and socket
under %TEMP%; the only process ever ended is the daemon this script started, by its PID, after checking
that its image lies in this script's own temporary folder. Python 3.10 or later, standard library only.
"""
import base64, ctypes, json, os, shutil, struct, subprocess, sys, tempfile, threading, time, zlib

WORKTREE = os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))
# PROTEUS_BUILT names another build to run instead (to compare two builds side by side).
BUILT = os.environ.get("PROTEUS_BUILT") or os.path.join(WORKTREE, "target", "release", "proteus.exe")


def image_path(pid):
    k = ctypes.windll.kernel32
    k.OpenProcess.restype = ctypes.c_void_p
    h = k.OpenProcess(0x1000, 0, int(pid))
    if not h:
        return None
    buf = ctypes.create_unicode_buffer(1024)
    n = ctypes.c_uint(1024)
    ok = k.QueryFullProcessImageNameW(ctypes.c_void_p(h), 0, buf, ctypes.byref(n))
    k.CloseHandle(ctypes.c_void_p(h))
    return buf.value if ok else None


def cycles(pid):
    """(cpu ms by GetProcessTimes, cycles)"""
    k = ctypes.windll.kernel32
    k.OpenProcess.restype = ctypes.c_void_p
    h = k.OpenProcess(0x1000, 0, int(pid))
    c = ctypes.c_ulonglong(); e = ctypes.c_ulonglong(); kk = ctypes.c_ulonglong(); u = ctypes.c_ulonglong(); cy = ctypes.c_ulonglong()
    k.GetProcessTimes(ctypes.c_void_p(h), ctypes.byref(c), ctypes.byref(e), ctypes.byref(kk), ctypes.byref(u))
    k.QueryProcessCycleTime(ctypes.c_void_p(h), ctypes.byref(cy))
    k.CloseHandle(ctypes.c_void_p(h))
    return (kk.value + u.value) / 10000.0, cy.value


def rss_mb(pid):
    class PMC(ctypes.Structure):
        _fields_ = [("cb", ctypes.c_uint), ("PageFaultCount", ctypes.c_uint), ("PeakWorkingSetSize", ctypes.c_size_t),
                    ("WorkingSetSize", ctypes.c_size_t), ("a", ctypes.c_size_t), ("b", ctypes.c_size_t), ("c", ctypes.c_size_t),
                    ("d", ctypes.c_size_t), ("PagefileUsage", ctypes.c_size_t), ("PeakPagefileUsage", ctypes.c_size_t)]
    k = ctypes.windll.kernel32
    k.OpenProcess.restype = ctypes.c_void_p
    h = k.OpenProcess(0x1010, 0, int(pid))
    pmc = PMC(); pmc.cb = ctypes.sizeof(PMC)
    ctypes.windll.psapi.GetProcessMemoryInfo(ctypes.c_void_p(h), ctypes.byref(pmc), pmc.cb)
    k.CloseHandle(ctypes.c_void_p(h))
    return pmc.WorkingSetSize / 1e6, pmc.PeakWorkingSetSize / 1e6


class Home:
    def __init__(self, tag):
        self.dir = tempfile.mkdtemp(prefix=f"proteus-review-{tag}-")
        os.makedirs(os.path.join(self.dir, "data"), exist_ok=True)
        dist = os.path.join(WORKTREE, "dist")
        os.makedirs(dist, exist_ok=True)
        self.exe = os.path.join(dist, f"proteus-review-{tag}-{os.getpid()}.exe")
        shutil.copy(BUILT, self.exe)
        self.env = dict(os.environ)
        self.env["PROTEUS_DATA"] = os.path.join(self.dir, "data")
        self.env["PROTEUS_SOCKET"] = os.path.join(self.dir, "d.sock")
        self.env["PROTEUS_SOUND"] = "off"
        self.env.pop("PROTEUS_SHELL", None)
        self.daemon_pid = None

    def run(self, *args, timeout=60):
        return subprocess.run([self.exe, *args], env=self.env, stdin=subprocess.DEVNULL, capture_output=True, timeout=timeout)

    def sessions(self):
        out = self.run("sessions").stdout
        try:
            v = json.loads(out)
        except Exception:
            return None
        if v and v.get("pid"):
            self.daemon_pid = v["pid"]
        return v

    def close(self):
        """End this home's own daemon, by PID, after checking the PID is ours."""
        v = self.sessions()
        pid = (v or {}).get("pid") or self.daemon_pid
        if pid:
            path = image_path(pid) or ""
            if os.path.normcase(path).startswith(os.path.normcase(self.dir)):
                subprocess.run(["taskkill", "/PID", str(pid), "/F", "/T"], capture_output=True)
            else:
                print(f"NOT killing pid {pid}: image {path!r} is not under {self.dir}")
        time.sleep(0.3)
        for _ in range(20):
            try:
                os.remove(self.exe)
                break
            except OSError:
                time.sleep(0.2)
        shutil.rmtree(self.dir, ignore_errors=True)


class Client:
    def __init__(self, home, *flags):
        self.home = home
        self.p = subprocess.Popen([home.exe, "mcp", *flags], env=home.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.answers = {}
        self.others = []
        self.bad = []
        self.cv = threading.Condition()
        self.next = 0
        self.eof = False
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.p.stdout:
            try:
                m = json.loads(line)
            except Exception as e:
                with self.cv:
                    self.bad.append(line[:200])
                    self.cv.notify_all()
                continue
            with self.cv:
                if isinstance(m, dict) and "id" in m and m["id"] is not None:
                    # A list for each id: two requests sent with the same id are both answered.
                    self.answers.setdefault(json.dumps(m["id"]), []).append(m)
                else:
                    self.others.append(m)
                self.cv.notify_all()
        with self.cv:
            self.eof = True
            self.cv.notify_all()

    def raw(self, data: bytes):
        self.p.stdin.write(data)
        self.p.stdin.flush()

    def write(self, msg):
        self.raw((json.dumps(msg) + "\n").encode())

    def answer(self, id, timeout=60):
        key = json.dumps(id)
        end = time.time() + timeout
        with self.cv:
            while not self.answers.get(key):
                left = end - time.time()
                if left <= 0 or self.eof:
                    return None
                self.cv.wait(left)
            return self.answers[key].pop(0)

    def other(self, timeout=10):
        end = time.time() + timeout
        with self.cv:
            while not self.others:
                left = end - time.time()
                if left <= 0 or self.eof:
                    return None
                self.cv.wait(left)
            return self.others.pop(0)

    def request(self, method, params=None, timeout=60, id=None):
        if id is None:
            self.next += 1
            id = self.next
        msg = {"jsonrpc": "2.0", "id": id, "method": method}
        if params is not None:
            msg["params"] = params
        self.write(msg)
        return self.answer(id, timeout)

    def start_call(self, tool, args, id=None):
        if id is None:
            self.next += 1
            id = self.next
        self.write({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": tool, "arguments": args}})
        return id

    def finish(self, id, timeout=60):
        r = self.answer(id, timeout)
        if r is None:
            return None, None, None
        res = r.get("result")
        if res is None:
            return r, None, None
        content = res.get("content", [])
        facts = None
        if content and content[0].get("type") == "text":
            try:
                facts = json.loads(content[0]["text"])
            except Exception:
                facts = content[0]["text"]
        body = content[1].get("text", "") if len(content) > 1 and content[1].get("type") == "text" else ""
        return res, facts, body

    def call(self, tool, args, timeout=60):
        return self.finish(self.start_call(tool, args), timeout)

    def ok(self, tool, args, timeout=60):
        res, facts, body = self.call(tool, args, timeout)
        assert res is not None, f"{tool} {args}: no answer"
        assert res.get("isError") is False, f"{tool} {args}: {res}"
        return facts, body

    def refused(self, tool, args, timeout=60):
        res, facts, body = self.call(tool, args, timeout)
        assert res is not None and res.get("isError") is True, f"{tool} {args}: expected refusal, got {res}"
        return res["content"][0]["text"]

    def handshake(self):
        r = self.request("initialize", {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "review", "version": "0"}})
        assert r["result"]["protocolVersion"] == "2025-11-25", r
        self.write({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def alive(self):
        return self.p.poll() is None

    def close(self, wait=10):
        try:
            self.p.stdin.close()
        except Exception:
            pass
        try:
            self.p.wait(wait)
            return True
        except subprocess.TimeoutExpired:
            self.p.kill()  # our own child, by handle
            return False


def journal_records(path):
    """[(kind, t_us, payload)] of a PROTEUS journal, parsed independently of the Rust code."""
    b = open(path, "rb").read()
    assert b[:8] == b"PRTJ0001", b[:8]
    mlen = struct.unpack_from("<I", b, 8)[0]
    i = 12 + mlen
    out = []
    while i + 13 <= len(b):
        kind = b[i]
        t = struct.unpack_from("<Q", b, i + 1)[0]
        n = struct.unpack_from("<I", b, i + 9)[0]
        if i + 13 + n > len(b):
            break
        out.append((kind, t, b[i + 13:i + 13 + n]))
        i += 13 + n
    return out


def journal_output(path):
    return b"".join(p for k, _, p in journal_records(path) if k == 1)


def png_info(b64):
    raw = base64.b64decode(b64)
    assert raw[:8] == b"\x89PNG\r\n\x1a\n"
    w, h, depth, ctype = struct.unpack_from(">IIBB", raw, 16)
    # inflate IDAT and unfilter enough to count distinct pixels
    idat = b""
    i = 8
    while i < len(raw):
        n = struct.unpack_from(">I", raw, i)[0]
        t = raw[i + 4:i + 8]
        if t == b"IDAT":
            idat += raw[i + 8:i + 8 + n]
        i += 12 + n
    data = zlib.decompress(idat)
    return w, h, depth, ctype, len(raw), data


def pct(samples, p):
    s = sorted(samples)
    return s[round((len(s) - 1) * p)]


def summary(samples):
    return {"n": len(samples), "min": round(min(samples), 2), "p50": round(pct(samples, .5), 2), "p95": round(pct(samples, .95), 2), "max": round(max(samples), 2)}
