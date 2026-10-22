"""Host headroom for benchmark guards, including conservative OpenZFS credit.

This does not change kernel settings or application memory limits. ARC credit
follows arc_evictable_memory() in OpenZFS's Linux arc_os.c, discounts that
estimate by half, and caps the credit at 64 GiB. Raw MemAvailable stays visible.
"""

from pathlib import Path

GIB = 1024**3
GUARD_POLICY = "openzfs-half64-reserve16-emergency4-v1"


def memory_state(meminfo, arcstats=None, pc_percent=None):
    info = {name: int(value.split()[0]) * 1024
            for line in meminfo.splitlines() if ":" in line
            for name, value in [line.split(":", 1)]}
    available = info["MemAvailable"]
    result = {"available_bytes": available, "effective_available_bytes": available,
              "zfs_arc_bytes": 0, "zfs_arc_reclaimable_bytes": 0,
              "zfs_arc_credit_bytes": 0}
    if arcstats is None or pc_percent is None:
        return result
    try:
        stats = {name: int(value) for line in arcstats.splitlines()
                 if len(line.split()) == 3
                 for name, kind, value in [line.split()]
                 if kind in ("3", "4")}
        percent = int(pc_percent)
        size, minimum, maximum = (stats[name] for name in ("size", "c_min", "c_max"))
        clean = sum(stats[name] for name in ("mru_evictable_data", "mru_evictable_metadata",
                                            "mfu_evictable_data", "mfu_evictable_metadata"))
        if not 0 <= percent <= 100 or min(size, minimum, maximum, clean) < 0 or minimum > maximum:
            return result
        file_pages = sum(info.get(name, 0) for name in ("Cached", "Buffers", "SwapCached"))
        floor = max(minimum, min(maximum, file_pages // 100 * percent))
        reclaimable = min(clean, max(0, size - floor))
        # A kernel may already account for ARC in miscellaneous reclaimable
        # memory. Subtract all of that counter to avoid crediting it twice.
        accounted = max(0, info["KReclaimable"] - info["SReclaimable"])
        credit = min(max(0, reclaimable - accounted) // 2, 64 * GIB)
    except (KeyError, ValueError):
        return result
    result.update(zfs_arc_bytes=size, zfs_arc_reclaimable_bytes=reclaimable,
                  zfs_arc_credit_bytes=credit, effective_available_bytes=available + credit)
    return result


def read_memory_state():
    meminfo = Path("/proc/meminfo").read_text()
    try:
        arcstats = Path("/proc/spl/kstat/zfs/arcstats").read_text()
        percent = Path("/sys/module/zfs/parameters/zfs_arc_pc_percent").read_text()
    except OSError:
        return memory_state(meminfo)
    return memory_state(meminfo, arcstats, percent)


def require_headroom(state, minimum_gib=16):
    if state["available_bytes"] < 4 * GIB:
        raise RuntimeError("host emergency guard: less than 4 GiB MemAvailable")
    if state["effective_available_bytes"] < minimum_gib * GIB:
        raise RuntimeError(f"host memory guard: less than {minimum_gib} GiB effective headroom")
