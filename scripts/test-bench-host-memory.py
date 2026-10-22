import unittest

from bench_host_memory import GIB, memory_state, require_headroom


def mem(available=10, misc=0, cache=40):
    values = {"MemAvailable": available, "Cached": cache, "Buffers": 0,
              "SwapCached": 0, "KReclaimable": 2 + misc, "SReclaimable": 2}
    return "\n".join(f"{key}: {value * GIB // 1024} kB" for key, value in values.items())


def arc(size=100, clean=90, minimum=8, maximum=200):
    values = {"size": size, "c_min": minimum, "c_max": maximum,
              "mru_evictable_data": clean, "mru_evictable_metadata": 0,
              "mfu_evictable_data": 0, "mfu_evictable_metadata": 0}
    return "\n".join(f"{key} 4 {value * GIB}" for key, value in values.items())


class MemoryTests(unittest.TestCase):
    def test_no_zfs_preserves_linux_available(self):
        state = memory_state(mem())
        self.assertEqual(state["effective_available_bytes"], 10 * GIB)
        self.assertEqual(state["zfs_arc_credit_bytes"], 0)

    def test_only_half_clean_arc_is_credited(self):
        state = memory_state(mem(), arc(), "0")
        self.assertEqual(state["zfs_arc_reclaimable_bytes"], 90 * GIB)
        self.assertEqual(state["effective_available_bytes"], 55 * GIB)

    def test_minimum_preserved_when_dirty_arc_is_small(self):
        state = memory_state(mem(), arc(size=100, clean=99), "0")
        self.assertEqual(state["zfs_arc_reclaimable_bytes"], 92 * GIB)

    def test_page_cache_floor_and_maximum_are_respected(self):
        state = memory_state(mem(cache=400), arc(maximum=60), "100")
        self.assertEqual(state["zfs_arc_reclaimable_bytes"], 40 * GIB)

    def test_below_arc_minimum_has_no_credit(self):
        self.assertEqual(memory_state(mem(), arc(size=4, clean=4), "0")["zfs_arc_credit_bytes"], 0)

    def test_existing_kernel_accounting_is_not_double_counted(self):
        state = memory_state(mem(misc=80), arc(), "0")
        self.assertEqual(state["zfs_arc_credit_bytes"], 5 * GIB)
        self.assertEqual(memory_state(mem(misc=100), arc(), "0")["zfs_arc_credit_bytes"], 0)

    def test_credit_capped_even_with_large_cache(self):
        self.assertEqual(memory_state(mem(), arc(size=500, clean=490, maximum=600), "0")["zfs_arc_credit_bytes"], 64 * GIB)

    def test_malformed_or_missing_stats_fail_closed(self):
        for stats, percent in [("bad", "0"), (arc(), None), (arc(), "bad"), (arc(), "101"), (arc(minimum=300), "0")]:
            self.assertEqual(memory_state(mem(), stats, percent)["zfs_arc_credit_bytes"], 0)

    def test_clean_counter_race_cannot_credit_more_than_size(self):
        self.assertEqual(memory_state(mem(), arc(size=10, clean=100), "0")["zfs_arc_credit_bytes"], GIB)

    def test_raw_emergency_floor_applies_despite_arc_credit(self):
        with self.assertRaisesRegex(RuntimeError, "emergency"):
            require_headroom(memory_state(mem(available=3), arc(), "0"))
        with self.assertRaisesRegex(RuntimeError, "effective"):
            require_headroom(memory_state(mem(available=10)))
        require_headroom(memory_state(mem(), arc(), "0"))


if __name__ == "__main__":
    unittest.main()
