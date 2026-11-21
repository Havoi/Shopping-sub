"""Record the effective Linux CPU and memory allocation before building images."""
import json
import os
from pathlib import Path
import shutil
import subprocess


def probe():
    records = []
    for section in Path("/proc/cpuinfo").read_text().strip().split("\n\n"):
        fields = {k.strip(): v.strip() for k, v in
                  (line.split(":", 1) for line in section.splitlines() if ":" in line)}
        if "processor" in fields:
            records.append(fields)
    affinity = sorted(os.sched_getaffinity(0))
    applicable = [r for r in records if int(r["processor"]) in affinity]
    flags = set.intersection(*(set(r.get("flags", "").split()) for r in applicable))
    required = set("avx2 avx512f avx512bw avx512vl avx512dq avx512cd avx512_vnni bmi1 bmi2 fma popcnt".split())
    limits = [len(affinity), int(subprocess.check_output(["nproc"], text=True))]
    memory_limits = []
    cgroups = [line[3:] for line in Path("/proc/self/cgroup").read_text().splitlines()
               if line.startswith("0::")]
    root = Path("/sys/fs/cgroup")
    for group in cgroups:
        group_path = (root / group.strip("/")).resolve()
        for directory in (group_path, *group_path.parents):
            if not directory.is_relative_to(root):
                break
            quota = directory / "cpu.max"
            if quota.is_file():
                q, period = quota.read_text().split()
                if q != "max":
                    limits.append(int(q) // int(period))
            memory = directory / "memory.max"
            if memory.is_file() and memory.read_text().strip() != "max":
                memory_limits.append(int(memory.read_text()))
    meminfo = {k: int(v.split()[0]) * 1024 for k, v in
               (line.split(":", 1) for line in Path("/proc/meminfo").read_text().splitlines())}
    workers = min(8, *limits)
    intel = all(r.get("vendor_id") == "GenuineIntel" for r in applicable)
    return {
        "cpu_limit": min(limits), "physical_core_ids": [[r.get("physical id"), r.get("core id")] for r in applicable],
        "cpu": applicable[0].get("model name"), "intel": intel,
        "system": subprocess.check_output(["uname", "-srmo"], text=True).strip(),
        "flags": sorted(flags), "missing": sorted(required - flags), "ifma": "avx512ifma" in flags,
        "affinity": affinity, "workers": workers, "cpuset": ",".join(map(str, affinity[:workers])),
        "mem_bytes": min([meminfo["MemTotal"], *memory_limits]),
        "mem_available_bytes": min([meminfo["MemAvailable"], *memory_limits]),
        "disk_free_bytes": shutil.disk_usage(os.environ.get("RUNNER_TEMP", "/tmp")).free,
        "capable": bool(not required - flags and workers >= 2),
    }


if __name__ == "__main__":
    state = probe()
    print(json.dumps(state, indent=2))
    conf = json.loads(Path(".github/runtime-compare.json").read_text())
    mode = conf.get("mode", "probe")
    if mode not in ("probe", "screen", "confirm", "profile", "branchprofile", "exactcycles", "exactbranches"):
        raise ValueError("unknown qualification mode")
    state["requested_mode"] = mode
    state["measure"] = mode != "probe" and state["capable"] and (mode not in ("profile","branchprofile","exactcycles","exactbranches") or (state["workers"] >= 4 and (state["ifma"] or mode in ("exactcycles","exactbranches"))))
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a") as stream:
            stream.write(f"measure={str(state['measure']).lower()}\n")
    if os.environ.get("RESULT_DIR"):
        output = Path(os.environ["RESULT_DIR"])
        output.mkdir(parents=True, exist_ok=True)
        (output / "machine.json").write_text(json.dumps(state, indent=2) + "\n")
