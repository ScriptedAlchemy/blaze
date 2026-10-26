#!/usr/bin/env python3
"""frontimage_pressure.py: what a kernel does with a front-end image held as a mapped file and
as heap, when memory runs out (D3, docs/research/front-end-image.md).

    frontimage_pressure.py FILE [--runs 3] [--extra-mb 100] [--size-mb N] [--share]

Runs on a Mac or a Linux machine with docker: it starts `python:3-slim` containers whose memory
cgroup has no swap, so what the kernel can do with a full cgroup is drop clean pages or kill. In
each container a process A holds FILE (the image `RA_JIT_FRONT_IMAGE` wrote, or `--size-mb` MB of
random bytes in its place) in one of two ways, and `--extra-mb` of anonymous memory, which
stands for the decoded `DefMap`s and every other memo of a worker:

  heap   the bytes read into a bytearray, which is what the store hands the worker today
  map    the file mapped read-only (`mmap`, `MAP_SHARED`, `PROT_READ`) and read once, which is
         what the image gives it

then a process B allocates anonymous memory until the cgroup is full: `limit - extra - 60` MB, so
that the anonymous memory of both fits and the file does not. The result per run: whether
anything was killed, A's RssAnon and RssFile before and after, and the seconds A takes to read the
whole file again. `--share` starts two processes that hold FILE the same way and reports the
cgroup's memory, and the proportional sizes of both (`Pss_Anon`, `Pss_File`).

Python 3 standard library only; the containers need the network for their image once.
"""
import argparse, json, mmap, os, shutil, subprocess, sys, tempfile, time

IMAGE = "python:3-slim"


def status(pid="self"):
    out = {}
    for line in open(f"/proc/{pid}/status"):
        key, _, value = line.partition(":")
        if key in ("RssAnon", "RssFile", "RssShmem"):
            out[key] = int(value.split()[0]) // 1024
    return out


def rollup(pid="self"):
    out = {}
    for line in open(f"/proc/{pid}/smaps_rollup"):
        key, _, value = line.partition(":")
        if key in ("Pss_Anon", "Pss_File", "Pss_Shmem", "Pss"):
            out[key] = int(value.split()[0]) // 1024
    return out


def cgroup_mb():
    for path in ("/sys/fs/cgroup/memory.current", "/sys/fs/cgroup/memory/memory.usage_in_bytes"):
        try:
            return int(open(path).read()) >> 20
        except OSError:
            pass


def touch(buffer):
    total = 0
    for at in range(0, len(buffer), 4096):
        total += buffer[at]
    return total


def hold(mode, path):
    if mode == "heap":
        # Read into the bytearray: reading to bytes first would hold the file twice for a moment.
        buffer = bytearray(os.path.getsize(path))
        with open(path, "rb", buffering=0) as file:
            file.readinto(buffer)
        return buffer
    file = open(path, "rb")
    mapped = mmap.mmap(file.fileno(), 0, flags=mmap.MAP_SHARED, prot=mmap.PROT_READ)
    touch(mapped)
    return mapped


def anonymous(mb):
    buffer = bytearray(mb << 20)
    buffer[::4096] = b"\x01" * len(range(0, len(buffer), 4096))
    return buffer


def say(**kw):
    print(json.dumps(kw), flush=True)


def inside_pressure(mode, path, extra_mb, press_mb):
    held = hold(mode, path)
    extra = anonymous(extra_mb)
    say(step="loaded", mode=mode, **status(), cgroup_mb=cgroup_mb())
    b = subprocess.Popen([sys.executable, __file__, "inside-allocate", str(press_mb)], stdout=subprocess.PIPE, text=True)
    line = b.stdout.readline().strip()
    during = dict(status(), cgroup_mb=cgroup_mb())
    started = time.time()
    touch(held)
    again = time.time() - started
    after = dict(status(), cgroup_mb=cgroup_mb())
    say(step="pressed", mode=mode, b_got_its_memory=line == "done", b_exit=b.poll(), during=during,
        after_reread=after, reread_s=round(again, 3), extra_kept_mb=len(extra) >> 20)
    b.kill()


def inside_allocate(mb):
    keep = []
    while sum(len(c) for c in keep) >> 20 < mb:
        keep.append(anonymous(16))
    print("done", flush=True)
    time.sleep(30)


def inside_hold(mode, path):
    held = hold(mode, path)
    print("held", flush=True)
    time.sleep(30)
    return held


def inside_share(mode, path):
    holders = [subprocess.Popen([sys.executable, __file__, "inside-hold", mode, path], stdout=subprocess.PIPE, text=True)
               for _ in range(2)]
    for h in holders:
        h.stdout.readline()
    pids = [h.pid for h in holders]
    say(step="shared", mode=mode, cgroup_mb=cgroup_mb(), status=[status(p) for p in pids], pss=[rollup(p) for p in pids])
    for h in holders:
        h.kill()


def docker(limit, path, scratch, prep, then):
    cmd = ["docker", "run", "--rm", f"--memory={limit}m", f"--memory-swap={limit}m", "--cpus=2",
           "-v", f"{path}:/src/front.image:ro", "-v", f"{scratch}:/s:ro", IMAGE, "sh", "-c", f"{prep} && python3 /s/p.py {then}"]
    ran = subprocess.run(cmd, capture_output=True, text=True)
    steps = [json.loads(l) for l in ran.stdout.splitlines() if l.startswith("{")]
    return ran.returncode, steps, ran.stderr[-300:]


def host(a):
    path = os.path.abspath(a.path)
    size_mb = a.size_mb or ((os.path.getsize(path) + (1 << 20) - 1) >> 20)
    if size_mb <= 0 or a.extra_mb < 0 or a.runs < 1:
        raise ValueError("sizes and run count must be positive")
    if max(size_mb + a.extra_mb + 120, 2 * size_mb + 200 if a.share else 0) > 6144:
        raise ValueError("the Docker experiment must stay below the shared VM's 6 GiB container cap")
    scratch = tempfile.mkdtemp(prefix="frontimage-pressure-", dir=os.path.dirname(path))
    shutil.copy(__file__, os.path.join(scratch, "p.py"))
    prep = f"head -c {size_mb << 20} /dev/urandom > /data.img" if a.size_mb else "cp /src/front.image /data.img"
    try:
        for n in range(1, a.runs + 1):
            for mode in ("heap", "map"):
                limit = size_mb + a.extra_mb + 120
                press = limit - a.extra_mb - 60
                code, steps, err = docker(limit, path, scratch, prep,
                                          f"inside-pressure {mode} /data.img {a.extra_mb} {press}")
                pressed = any(s["step"] == "pressed" for s in steps)
                print(json.dumps(dict(run=n, mode=mode, size_mb=size_mb, limit_mb=limit, press_mb=press, exit=code,
                                      survived=code == 0 and pressed, steps=steps, stderr=err if code else "")), flush=True)
        if a.share:
            for mode in ("heap", "map"):
                limit = 2 * size_mb + 200
                code, steps, err = docker(limit, path, scratch, prep, f"inside-share {mode} /data.img")
                print(json.dumps(dict(share=mode, size_mb=size_mb, limit_mb=limit, exit=code, steps=steps)), flush=True)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    return 0


def main():
    inside = {"inside-pressure": lambda m, p, e, k: inside_pressure(m, p, int(e), int(k)),
              "inside-allocate": lambda mb: inside_allocate(int(mb)), "inside-hold": inside_hold,
              "inside-share": inside_share}
    if len(sys.argv) > 1 and sys.argv[1] in inside:
        return inside[sys.argv[1]](*sys.argv[2:])
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("path", help="the image, or any file (with --size-mb, only a name)")
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--extra-mb", type=int, default=100)
    ap.add_argument("--size-mb", type=int, help="random bytes of this size instead of FILE")
    ap.add_argument("--share", action="store_true", help="also: two processes holding FILE, heap and map")
    return host(ap.parse_args())


if __name__ == "__main__":
    sys.exit(main())
