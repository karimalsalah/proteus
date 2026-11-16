"""Does Windows report a change to a directory for appends to a file its writer keeps open?"""
import ctypes, os, tempfile, time, json, shutil
k = ctypes.windll.kernel32
k.FindFirstChangeNotificationW.restype = ctypes.c_void_p
k.FindFirstChangeNotificationW.argtypes = [ctypes.c_wchar_p, ctypes.c_int, ctypes.c_uint]
k.WaitForSingleObject.argtypes = [ctypes.c_void_p, ctypes.c_uint]
k.FindNextChangeNotification.argtypes = [ctypes.c_void_p]
k.FindCloseChangeNotification.argtypes = [ctypes.c_void_p]
NAME, SIZE, LAST_WRITE = 0x1, 0x8, 0x10
d = tempfile.mkdtemp(prefix="proteus-review-notify-")
try:
    path = os.path.join(d, "journal.bin")
    f = open(path, "ab", buffering=0)
    f.write(b"header")
    time.sleep(0.3)
    h = k.FindFirstChangeNotificationW(d, 0, NAME | SIZE | LAST_WRITE)
    assert h and h != ctypes.c_void_p(-1).value

    def fired(ms):
        if k.WaitForSingleObject(h, ms) == 0:
            k.FindNextChangeNotification(h)
            return 1
        return 0
    held = sum((f.write(b"x" * 100), fired(100))[1] for _ in range(12))
    flushed = 0
    for _ in range(12):
        f.write(b"y" * 100); os.fsync(f.fileno()); flushed += fired(100)
    size_seen = os.path.getsize(path)  # asking for the size makes the file system bring the entry up to date
    after_stat = fired(300)
    f.close()
    after_close = fired(500)
    g = open(os.path.join(d, "other.bin"), "wb"); g.write(b"z"); g.close()
    control = fired(500)
    print("MEASURE", json.dumps({"what": "directory change notifications (name, size, last write) for a file its writer holds open",
                                 "12 appends, file held open": held, "12 appends each followed by FlushFileBuffers": flushed,
                                 "after another process asked for its size": after_stat, "after the writer closed it": after_close,
                                 "control: a new file created and closed": control, "size": size_seen}))
    k.FindCloseChangeNotification(h)
finally:
    shutil.rmtree(d, ignore_errors=True)
