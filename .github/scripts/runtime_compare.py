"""Measure pre-materialized Docker contexts under the same CPU and dataset."""
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime
import csv
import hashlib
import json
import os
from pathlib import Path
import random
import re
import shutil
import statistics
import subprocess
import tempfile
import time
import zipfile

import duckdb
from runtime_probe import probe

KIT = "https://www.joinbuilders.org/buildathon/tracks/shopping-sessions/files"
DIGESTS = {
    "generator": "13724cadbac5a99d847e27cea911cd2e338ff972b46c2039da87452011bb101e",
    "starter-duckdb": "b84ff8726c43749043c33c250548a9ac9987b9132d28ac818a47515bd128f77c",
    "dev-answer": "7eb8e1f0b1f431d178d28ac9437ce85809c44eb6393fdd42fb59857f30834fa9",
}
SCHEMAS = {
    "user_daily": {"user_id": "VARCHAR", "day": "DATE", "sessions": "HUGEINT", "events": "HUGEINT",
                   "revenue_cents": "HUGEINT", "top_category": "VARCHAR"},
    "campaign_daily": {"campaign_id": "VARCHAR", "channel": "VARCHAR", "day": "DATE", "sessions": "HUGEINT",
                       "purchasing_sessions": "HUGEINT", "revenue_cents": "HUGEINT", "assisted_sessions": "HUGEINT"},
}
INTEGERS = set("TINYINT SMALLINT INTEGER BIGINT HUGEINT UTINYINT USMALLINT UINTEGER UBIGINT UHUGEINT".split())
GIB = 1024 ** 3


def run(*args, **kwargs):
    return subprocess.run(list(map(str, args)), check=True, **kwargs)


def capture(*args):
    return run(*args, text=True, stdout=subprocess.PIPE).stdout.strip()


def get_kit(work):
    for name, expected in DIGESTS.items():
        archive = work / f"{name}.zip"
        run("curl", "--fail", "--location", "--silent", "--show-error", "--retry", "3",
            f"{KIT}/{name}.zip", "--output", archive)
        if hashlib.sha256(archive.read_bytes()).hexdigest() != expected:
            raise ValueError(f"{name} differs from the verified challenge kit")
        with zipfile.ZipFile(archive) as source:
            source.extractall(work)
        archive.unlink()
    run("cargo", "build", "--offline", "--locked", "--release", "--manifest-path", work / "generator/Cargo.toml")
    return work / "generator/target/release/generate"


def official_fixtures(root):
    expected = {f"{prefix}{i}" for prefix, count in (("A", 12), ("E", 20), ("L", 4))
                for i in range(1, count + 1)}
    cases = {p.name: p for p in root.iterdir() if p.is_dir()
             and (p / "input").is_dir() and (p / "expected").is_dir()}
    if cases.keys() != expected:
        raise ValueError(f"official fixture inventory differs: missing={expected - cases.keys()}, "
                         f"unexpected={cases.keys() - expected}")
    return [cases[name] for name in sorted(cases)]


def warm(data):
    block = bytearray(4 * 1024 * 1024)
    for path in sorted(data.rglob("*")):
        if path.is_file():
            with path.open("rb", buffering=0) as stream:
                while stream.readinto(block):
                    pass


def reclaim(path):
    # Late ownership recovery for Docker UID 1000 on Actions hosts owned by UID 1001.
    for root, dirs, files in os.walk(path):
        directories = [Path(root), *(Path(root) / d for d in dirs)]
        if (any(not p.is_symlink() and not os.access(p, os.R_OK | os.W_OK | os.X_OK) for p in directories)
                or any(not p.is_symlink() and not os.access(p, os.R_OK) for p in (Path(root) / f for f in files))):
            run("sudo", "-n", "chown", "-R", "-h", "--", f"{os.getuid()}:{os.getgid()}", path)
            return


def run_image(image, data, output, cpu, memory, *, profile=False, native=False, calibration=None):
    output.mkdir()
    output.chmod(0o777)
    with tempfile.TemporaryDirectory(dir=output.parent) as tmp:
        os.chmod(tmp, 0o777)
        probe_args, app_command = [], []
        if calibration:
            static_probe, app_command = calibration
            probe_args = ["-v", f"{static_probe}:/__runtime_clock:ro", "--entrypoint", "/__runtime_clock"]
        cid = capture("docker", "create", "--platform", "linux/amd64", "--network", "none", "--read-only",
                      "--tmpfs", "/tmp:rw,size=64m,mode=1777", "--cap-drop", "ALL", "--security-opt",
                      "no-new-privileges", "--pids-limit", "4096", "--user", "1000:1000", "--cpus",
                      cpu["workers"], "--cpuset-cpus", cpu["cpuset"], "--memory", f"{memory}g",
                      "--memory-swap", f"{memory}g", "-e", f"NCPU={cpu.get('solver_workers',cpu['workers'])}", "-e", f"MEM_GB={memory}",
                      "-e", "DATA_DIR=/data", "-e", "OUTPUT_DIR=/output", "-e", "SCRATCH_DIR=/scratch",
                      "-e", "HOME=/scratch", "-e", "TMPDIR=/scratch",
                      "-v", f"{data}:/data:ro", "-v", f"{output}:/output", "-v", f"{tmp}:/scratch",
                      *(["-e", "XL_TIMING=1"] if profile else []),
                      *(["--entrypoint", "/usr/local/libexec/shopping-native", "-e", "LD_PRELOAD=/usr/lib/x86_64-linux-gnu/libjemalloc.so.2"] if native else []), *probe_args, image, *app_command)
        try:
            completed = subprocess.run(["docker","start","-a",cid], text=True, timeout=240,
                                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            state = json.loads(capture("docker", "inspect", "-f", "{{json .State}}", cid))
            if completed.returncode or state["ExitCode"] != 0 or state["OOMKilled"]:
                raise RuntimeError(f"{image} failed: {state}. Last stderr: {completed.stderr[-400:]}")
            start, end = (datetime.fromisoformat(state[k].replace("Z", "+00:00"))
                          for k in ("StartedAt", "FinishedAt"))
            timing = {}
            if profile:
                for line in completed.stderr.splitlines():
                    if line.startswith("[v2b] ms: "):
                        labels = {"total","setup","parse","st2sort","ev3b","write","complex","s.list","s.mmap","s.sample",
                                  "maps+keys","lookups","st2a","st2b","ev3a","stitch","c.a","c.pre","parse+links","dedup+route","sessions","free"}
                        timing["phases_ms"]={k:float(v) for k,v in re.findall(r"([+.a-zA-Z_0-9]+)=([0-9.]+)",line) if k in labels}
                        rss=re.search(r"peak_rss_kb=([0-9]+)",line)
                        if rss: timing["peak_rss_kb"]=int(rss[1])
                    if line.startswith("[v2b] prof (thread-ms): "):
                        keys={"scatter","compact","runs","st2b","st2b_max_bucket","st2b_pass1","st2b_pass2","st2b_sort"}
                        timing["thread_ms"]={k:float(v) for k,v in re.findall(r"([a-z_0-9]+)=([0-9.]+)",line) if k in keys}
                    if line.startswith("[v2b] exit at "):
                        found = re.search(r"exit at ([0-9.]+) ms",line)
                        if found: timing["whole_process_ms"]=float(found[1])
                timing["generic_fallback_observed"]=any("FALLBACK" in l for l in completed.stderr.splitlines())
            if calibration:
                found=re.search(r"^\[runtime-clock\] ((?:[0-9]+ ){9}[0-9]+)$",completed.stderr,re.MULTILINE)
                if not found: raise RuntimeError("clock probe failed to report")
                entered,child,marker,waited,leaving,user,system,rss,minor,major=map(int,found[1].split())
                docker_start=round(start.timestamp()*1e9)
                docker_end=round(end.timestamp()*1e9)
                ms=lambda a,b: round((b-a)/1e6,4)
                timing={"start_to_probe_ms":ms(docker_start,entered),"probe_to_finished_ms":ms(leaving,docker_end),
                        "container_ms":ms(docker_start,docker_end),"probe_body_ms":ms(entered,leaving),
                        "child_user_cpu_ms":user/1000,"child_sys_cpu_ms":system/1000,"child_max_rss_kib":rss,
                        "child_minor_faults":minor,"child_major_faults":major}
                if child:
                    timing.update(pre_exec_ms=ms(entered,child),child_to_wait_ms=ms(child,waited),
                                  post_wait_ms=ms(waited,leaving))
                if marker:
                    timing.update(exit_line_observed_to_wait_ms=ms(marker,waited),child_to_exit_line_seen_ms=ms(child,marker))
                internal=re.search(r"\[v2b\] exit at ([0-9.]+) ms",completed.stderr)
                if internal:
                    timing["solver_internal_ms"]=float(internal[1])
                    timing["child_minus_solver_internal_ms"]=round(timing["child_to_wait_ms"]-float(internal[1]),4)
            return (end - start).total_seconds(), timing
        finally:
            try:
                run("docker", "rm", "-f", cid, stdout=subprocess.DEVNULL)
            finally:
                reclaim(output)
                reclaim(tmp)


def load_relation(db, path, table, relation, expected_csv):
    schema = SCHEMAS[table]
    if expected_csv:
        filename = path / (table + ".csv")
        with filename.open(encoding="utf-8", newline="") as stream:
            if next(csv.reader(stream)) != list(schema):
                raise ValueError(f"wrong CSV header in {filename}")
        obj = db.read_csv(str(filename), columns=schema, header=True)
    else:
        folder = path / table
        if folder.is_symlink() or not folder.is_dir():
            raise ValueError(f"missing {folder}")
        files = sorted(str(p) for p in folder.glob("*.parquet")
                       if not p.name.startswith(("_", ".")) and not p.is_symlink() and p.is_file())
        if not files:
            raise ValueError(f"no Parquet files in {folder}")
        chunks = {}
        for file, name, kind in db.execute("SELECT file_name,name,duckdb_type FROM parquet_schema(?)", [files]).fetchall():
            if kind is not None:
                chunks.setdefault(file, []).append((name, kind))
        if len(chunks) != len(files):
            raise ValueError(f"invalid empty Parquet schema in {folder}")
        for file, fields in chunks.items():
            if len(fields) != len(schema) or {k for k, _ in fields} != set(schema):
                raise ValueError(f"{file} columns differ: {fields}")
            for column, actual in fields:
                expected = schema[column]
                if actual != expected and not (expected == "HUGEINT" and actual in INTEGERS):
                    raise ValueError(f"{file}:{column} got {actual}, expected {expected}")
        obj = db.read_parquet(files)
    obj.project(",".join(schema)).create_view(relation, replace=True)


def check(expected, actual, db, *, csv_gold=False, validate_gold=False):
    summary = {}
    for table, schema in SCHEMAS.items():
        load_relation(db, expected, table, "wanted", csv_gold)
        load_relation(db, actual, table, "got", False)
        key = next(iter(schema))
        if validate_gold:
            bad = db.execute(f"SELECT {key},day FROM wanted GROUP BY ALL "
                             f"HAVING count(*) <> 1 OR {key} IS NULL OR day IS NULL LIMIT 1").fetchone()
            if bad:
                raise ValueError(f"{table}: invalid reference key {bad}")
        a, b = db.execute("SELECT (SELECT count(*) FROM wanted),(SELECT count(*) FROM got)").fetchone()
        if a != b:
            raise ValueError(f"{table}: expected {a} rows, got {b}")
        different = " OR ".join(f"w.{col} IS DISTINCT FROM g.{col}" for col in schema if col not in (key, "day"))
        bad = db.execute(f"SELECT w.{key},w.day,g.{key},g.day FROM wanted w FULL JOIN got g "
                         f"ON w.{key}=g.{key} AND w.day=g.day WHERE w.{key} IS NULL OR g.{key} IS NULL "
                         f"OR {different} LIMIT 1").fetchone()
        if bad:
            raise ValueError(f"{table}: unmatched key or different values at {bad}")
        summary[table] = a
    return summary


def save(result, dest):
    (dest / "comparison.json").write_text(json.dumps(result, indent=2))




def executable_digests(tag):
    import tarfile
    cid = capture("docker", "create", tag)
    values = {}
    try:
        with subprocess.Popen(["docker","export",cid],stdout=subprocess.PIPE) as stream:
            with tarfile.open(fileobj=stream.stdout,mode="r|*") as tar:
                for member in tar:
                    name=member.name.removeprefix("./")
                    if member.isfile() and (name.startswith("usr/local/libexec/shopping-") or name=="usr/local/bin/shopping-sessions" or name in ("pipeline","ifma","choose")):
                        with tar.extractfile(member) as f:
                            values[name]=hashlib.file_digest(f,"sha256").hexdigest()
            # Consume tar's trailing blocks before the Docker client writes its final status.
            while stream.stdout.read(65536):pass
            if stream.wait()!=0:raise RuntimeError("executable fingerprint extraction failed")
        if not values:raise ValueError("empty executable fingerprint")
        return values
    finally:
        run("docker","rm",cid)


def build(name, screening):
    tag = "trial-" + name
    source = Path(os.environ["RUNNER_TEMP"]) / "inputs" / name
    recipe = (source / "Dockerfile").read_text()
    args = ["docker", "build", "--platform", "linux/amd64", "-t", tag]
    split_stage = screening and bool(re.search(r"(?mi)^FROM\s+.*?\s+AS\s+wide-build\s*$", recipe)) and "/ifma" in recipe
    if split_stage:
        recipe_path = source / "Dockerfile.bench"
        extra = "COPY --from=wide-build /vbmi /vbmi\n" if " /vbmi" in recipe else ""
        recipe_path.write_text(recipe + '\nFROM scratch AS bench-screen\nCOPY --from=wide-build /pipeline /pipeline\nCOPY --from=wide-build /ifma /ifma\n' + extra + 'USER 1000:1000\nENTRYPOINT ["/pipeline"]\n')
        args += ["--target", "bench-screen", "-f", recipe_path]
    started = time.monotonic()
    run(*args, source, timeout=1200)
    return {"tag": tag, "build_s": round(time.monotonic()-started,3), "wide_only": split_stage}

def stream_generator(work):
    crate = work / "generator"
    # Building from an official, digest-checked archive protects this rewrite.
    new = work / "generator-lowmem"
    new.mkdir()
    for name in ("Cargo.toml", "Cargo.lock"):
        shutil.copy2(crate / name, new / name)
    shutil.copytree(crate / "src", new / "src")
    main = new / "src/main.rs"
    source = main.read_text()
    a = source.index("fn run(arguments: &Arguments)")
    b = source.index("fn remove_surplus_parts(", a)
    replacement = Path(__file__).with_name("stream_generator.rs").read_text()
    main.write_text(source[:a] + replacement + "\n\n" + source[b:])
    run("cargo", "build", "--offline", "--locked", "--release", "--manifest-path", new / "Cargo.toml")
    alternative = new / "target/release/generate"
    def manifest(where):
        result = {}
        for f in where.rglob("*"):
            if f.is_file():
                with f.open("rb") as stream:
                    result[str(f.relative_to(where))] = hashlib.file_digest(stream, "sha256").hexdigest()
        return result
    expected = work / "dev"
    with tempfile.TemporaryDirectory(dir=work) as temp:
        check_dir = Path(temp)
        run(alternative, "--seed", 1, "--events", 1000000, "--clicks", 250000, "--output", check_dir)
        if manifest(expected) != manifest(check_dir):
            raise RuntimeError("streaming generator differs from official generator on canonical dev")
    return alternative


def page_residency(data):
    # mincore samples page residency without faulting file contents back in.
    import ctypes as ct
    libc = ct.CDLL(None, use_errno=True)
    libc.mmap.argtypes = [ct.c_void_p, ct.c_size_t, ct.c_int, ct.c_int, ct.c_int, ct.c_long]
    libc.mmap.restype = ct.c_void_p
    libc.mincore.argtypes = [ct.c_void_p, ct.c_size_t, ct.c_void_p]
    libc.munmap.argtypes = [ct.c_void_p, ct.c_size_t]
    block = os.sysconf("SC_PAGE_SIZE")
    pages = resident = 0
    for f in data.rglob("*"):
        if not f.is_file() or (size := f.stat().st_size) == 0: continue
        with f.open("rb") as fp:
            p = libc.mmap(None, size, 0, 2, fp.fileno(), 0)
            if p == ct.c_void_p(-1).value: raise OSError(ct.get_errno(), "mmap")
            try:
                n = (size + block - 1) // block
                flags = ct.create_string_buffer(n)
                if libc.mincore(p, size, flags) != 0: raise OSError(ct.get_errno(), "mincore")
                resident += sum(1 & k for k in flags.raw)
                pages += n
            finally:
                libc.munmap(p, size)
    return round(resident / pages,4) if pages else 0


def extra_final_shapes(config):
    if not config.get("alternate_sizes"):
        return None
    if not config.get("final_focus") or config.get("rounds")!=7:
        raise ValueError("size holdout requires full seven-run qualification")
    return ((54000000,9000000),(66000000,22000000))


def selected_datasets(config,mode,seeds,replica):
    if config.get("final_focus"):
        if mode!="confirm" or config.get("rounds")!=7 or len(seeds)!=2:
            raise ValueError("final focus needs full confirm, seven rounds and two independent seeds")
        shapes=extra_final_shapes(config)
        return [(row[0],seed) for row,seed in zip(shapes,seeds)] if shapes else [(60000000,seed) for seed in seeds]
    sizes=[(20000000,seed) for seed in seeds]
    if mode=="confirm":sizes.append((60000000,config.get("final_seed",697381405)+replica*1000003))
    return sizes


def paired(data, label, tags, oracle_tag, oracle_native, rounds, work, cpu, memory, db, result, dest):
    tags=[t for t in tags if t not in result["failures"]]
    root = work / "comparisons"; root.mkdir()
    cache=None
    verify=check
    try:
        gold = root/"gold"
        run_image(oracle_tag, data, gold, cpu, memory, native=oracle_native)
        state = {"samples": {t: [] for t in tags}, "trials": [], "profiles": {}, "reference": oracle_tag}
        result["datasets"][label] = state
        if result.get("check_cache"):
            from runtime_check_cache import ExactOutputCache
            state["verification_cache"]={}
            cache=ExactOutputCache(root/"verified-bytes",SCHEMAS,load_relation,gold,db,state["verification_cache"],materialize=result.get("materialize_gold",False))
            verify=cache
        # Includes an independent profiling launch, omitted from scoring.
        randomizer = random.Random(int(label.split("-")[-1]))
        order = list(tags)
        randomizer.shuffle(order)
        for iteration in range(rounds+1):
            idx = iteration % len(order)
            if result.get("final_focus"):
                randomizer.shuffle(order)
                scheduled=order
            else:
                scheduled=order[idx:]+order[:idx]
            for tag in scheduled:
                warm(data)
                resident = page_residency(data)
                output = root/"trial"
                result["active"]={"stage":"timed","data":label,"label":tag,"round":iteration}; save(result,dest)
                if tag in result["failures"]: continue
                try:
                    seconds, timing = run_image(tag, data, output, cpu, memory, profile=iteration==0)
                    rows = verify(gold, output, db, validate_gold=not state["trials"])
                except (ValueError, RuntimeError, duckdb.Error, subprocess.SubprocessError) as err:
                    if not skip_failed_experiment(result,result["mode"],tag.removeprefix("trial-"),tags[0].removeprefix("trial-"),oracle_tag.removeprefix("trial-"),"full-output",err):
                        raise
                    if output.exists(): shutil.rmtree(output)
                    state["samples"].pop(tag,None)
                    state["profiles"].pop(tag,None)
                    state["trials"]=[t for t in state["trials"] if t["tag"]!=tag]
                    # Store only the stage and class of data error on the public runner.
                    reason=str(err)
                    result["failures"][tag]["check"]=next((kind for label,kind in [
                        (" rows, got ","row-count"),("unmatched key","values"),
                        ("invalid reference key","reference-key"),("wrong type","schema"),
                        ("failed:","process")] if label in reason),"other")
                    save(result,dest)
                    continue
                shutil.rmtree(output)
                if iteration: state["samples"][tag].append(seconds)
                else: state["profiles"][tag] = timing
                state["trials"].append(dict(round=iteration, tag=tag, seconds=seconds, rows=rows, resident=resident))
                save(result,dest)
                print(label,iteration,tag,seconds,resident,rows,flush=True)
        state["medians"]={k:statistics.median(x) for k,x in state["samples"].items()}
        ratios={k:[a/b for a,b in zip(x,state["samples"][tags[0]])] for k,x in state["samples"].items()}
        state["paired_ratios"]=ratios
        state["median_ratios"]={k:statistics.median(x) for k,x in ratios.items()}
        if result["mode"]=="confirm":
            calibrator=work/"runtime-clock"
            if not calibrator.exists():
                run("cc","-O2","-static","-s","-Wall","-Wextra","-Werror",Path(__file__).with_name("runtime_clock.c"),"-o",calibrator)
            clocks=state["calibration"]={"noop":{}, "solver":{}}
            probe_output=root/"probe-output"
            _, clocks["noop"]=run_image(tags[0],data,probe_output,cpu,memory,calibration=(calibrator,["--noop"]))
            shutil.rmtree(probe_output)
            for tag in tags:
                conf=json.loads(capture("docker","image","inspect","-f","{{json .Config}}",tag))
                invocation=(conf.get("Entrypoint") or [])+(conf.get("Cmd") or [])
                if not invocation: raise ValueError("image has no executable")
                warm(data)
                _,clock=run_image(tag,data,probe_output,cpu,memory,profile=True,calibration=(calibrator,invocation))
                clock["rows"]=verify(gold,probe_output,db)
                clocks["solver"][tag]=clock
                shutil.rmtree(probe_output)
                if cpu["workers"]<8:
                    # Exercise the contest's eight-worker scheduling paths. CPU
                    # quota and affinity are unchanged, so these launches are
                    # correctness checks and do not enter timing distributions.
                    run_image(tag,data,probe_output,dict(cpu,solver_workers=8),memory)
                    state.setdefault("eight_worker_checks",{})[tag]=verify(gold,probe_output,db)
                    shutil.rmtree(probe_output)
                save(result,dest)
        diagnostic_prefix="60M-" if result.get("final_focus") else "20M-"
        if (result.get("worker_study") and label.startswith(diagnostic_prefix) and len(label.split("-"))==2
                and 4<=cpu["workers"]<8):
            # Vary the application's worker count while preserving VM affinity,
            # CPU quota, Docker image, dataset, full output checks and clock.
            diagnostics=state["worker_study"]={"quota":cpu["workers"],"cpuset":cpu["cpuset"],"blocks":[]}
            probe_output=root/"worker-shape-output"
            study_tags=list(dict.fromkeys((tags[0],tags[-1])))
            for block_index in range(4):
                seq=(4,8,8,4) if block_index%2==0 else (8,4,4,8)
                for tag in (study_tags if block_index%2==0 else reversed(study_tags)):
                    observations={4:[],8:[]}
                    for workers in seq:
                        warm(data)
                        elapsed,_=run_image(tag,data,probe_output,dict(cpu,solver_workers=workers),memory)
                        rows=verify(gold,probe_output,db)
                        shutil.rmtree(probe_output)
                        observations[workers].append(elapsed)
                    four,eight=observations[4],observations[8]
                    diagnostics["blocks"].append({"block":block_index,"tag":tag,"sequence":list(seq),"seconds":observations,
                        "eight_over_four":(eight[0]*eight[1]/(four[0]*four[1]))**0.5,"rows":rows})
                    save(result,dest)
        bracket_dataset=label.startswith("60M-" if result.get("final_focus") else "20M-")
        if bracket_dataset and result.get("bracketed") and len(tags)>=2:
            # Center the finalist and each reference at the same wall-clock
            # time. Alternating ABBA and BAAB offsets drift in VM clock speed.
            finalist=tags[-1]
            reference_tags=list(dict.fromkeys(tags[:min(3,len(tags)-1)]))
            block_out=root/"bracket-output"
            blocks=state["brackets"]=[]
            for repeat in range(3):
                refs=list(reference_tags)
                randomizer.shuffle(refs)
                for ref in refs:
                    sequence=[ref,finalist,finalist,ref] if repeat%2==0 else [finalist,ref,ref,finalist]
                    observed={ref:[],finalist:[]}
                    for member in sequence:
                        warm(data)
                        seconds,_=run_image(member,data,block_out,cpu,memory)
                        rows=verify(gold,block_out,db)
                        shutil.rmtree(block_out)
                        observed[member].append(seconds)
                    ratio=(observed[finalist][0]*observed[finalist][1]/(observed[ref][0]*observed[ref][1]))**0.5
                    blocks.append({"repeat":repeat,"reference":ref,"candidate":finalist,"times":observed,"ratio":ratio,"rows":rows})
                    save(result,dest)
        print(label,state["medians"],state["median_ratios"],flush=True)
        save(result,dest)
    finally:
        if cache is not None:cache.close()
        shutil.rmtree(root)


def skip_failed_experiment(result, mode, name, control, oracle, stage, error):
    if mode != "screen" or name == control or name == oracle:
        return False
    if isinstance(error, subprocess.TimeoutExpired):
        code = 124
    elif isinstance(error, subprocess.CalledProcessError):
        code = int(error.returncode)
    elif isinstance(error, (ValueError, duckdb.Error)):
        code = 2
    else:
        code = 1
    result["failures"]["trial-"+name] = {"stage":stage,"code":code}
    return True


def main():
    config = json.loads(Path(".github/runtime-compare.json").read_text())
    result_dir = Path(os.environ["RESULT_DIR"])
    cpu = probe()
    if not cpu["capable"]: raise ValueError("incompatible CPU")
    mode=config["mode"]
    names = config.get("candidate_names", [])
    if mode not in ("screen","confirm") or not 1<=len(names)<=16 or len(set(names))!=len(names) or not all(re.fullmatch("[a-z][a-z0-9-]{0,19}",n) for n in names):
        raise ValueError("one to sixteen distinct blind candidate names required")
    oracle = config.get("oracle_name")
    if oracle and not re.fullmatch("[a-z][a-z0-9-]{0,19}",oracle): raise ValueError("invalid oracle label")
    seeds = config.get("seeds",[34073597,91838679])
    rounds = config.get("rounds",5)
    if not 1<=len(seeds)<=4 or len(set(seeds))!=len(seeds) or not all(isinstance(s,int) and 0 < s < 2**63 for s in seeds):
        raise ValueError("one to four positive distinct seeds required")
    if not isinstance(rounds,int) or not 3<=rounds<=9: raise ValueError("three to nine rounds required")
    replica = int(os.environ.get("REPLICA", "0"))
    seeds = [s + replica * 1000003 for s in seeds]
    memory=min(24, int(cpu["mem_bytes"]/GIB)-1)
    if memory<4: raise ValueError("insufficient physical memory for a representative run")
    result={"machine":cpu,"mode":mode, "replica":replica, "rounds":rounds,"failures":{},"images":[],"datasets":{},"answer_checks":{},"skips":{},
            "bracketed":bool(config.get("bracketed",False)),"final_focus":bool(config.get("final_focus",False)),
            "worker_study":bool(config.get("worker_study",False)),
            "check_cache":bool(config.get("check_cache",False)),"materialize_gold":bool(config.get("materialize_gold",False)),
            "memory_limit_gib":memory,"clock":"Docker StartedAt -> FinishedAt, profiling launched separately"}
    save(result,result_dir)
    try:
        with tempfile.TemporaryDirectory(dir=os.environ['RUNNER_TEMP']) as temp:
            work=Path(temp)
            result["active"]={"stage":"official-kit"}; save(result,result_dir)
            gen=get_kit(work)
            workers=2 if mode=="screen" and cpu["workers"]>=4 and cpu["mem_bytes"]>=13*GIB and len(names)>2 else 1
            result["build_workers"]=workers
            result["active"]={"stage":"build","label":names[0]}; save(result,result_dir)
            result["images"].append(build(names[0],mode=="screen"))
            def candidate_build(name):
                try:
                    return build(name,mode=="screen"), None
                except (subprocess.CalledProcessError,subprocess.TimeoutExpired) as exc:
                    return None, exc
            result["active"]={"stage":"build-candidates","workers":workers}; save(result,result_dir)
            with ThreadPoolExecutor(max_workers=workers) as builders:
                for name, (image,error) in zip(names[1:],builders.map(candidate_build,names[1:])):
                    if error is not None:
                        if not skip_failed_experiment(result,mode,name,names[0],oracle,"build",error):
                            raise error
                    else:
                        result["images"].append(image)
                    save(result,result_dir)
            if config.get("executable_digests"):
                for image in result["images"]:
                    image["executable_digests"]=executable_digests(image["tag"])
            if oracle and oracle not in names:
                build(oracle,False)
            save(result,result_dir)
            tags=[i["tag"] for i in result["images"]]
            db=duckdb.connect(config={"threads":str(cpu["workers"]), "memory_limit":"1500MiB","temp_directory":str(work/"duckdb-spill")})
            try:
                dev=work/"dev"
                run(gen,"--seed",1,"--events",1000000,"--clicks",250000,"--threads",cpu["workers"],"--output",dev)
                cases=[("dev",dev,work/"dev-answer")]
                if mode=="confirm":
                    cases += [(p.name,p/"input",p/"expected") for p in official_fixtures(work/"starter-duckdb/fixtures")]
                qualified=[]
                for tag in tags:
                    checks=[]
                    try:
                        for name,folder,expected in cases:
                            out=work/"checked"
                            result["active"] = {"stage":"answer","label":tag,"case":name}; save(result,result_dir)
                            try:
                                run_image(tag,folder,out,cpu,memory)
                                counts=check(expected,out,db,csv_gold=True,validate_gold=True)
                                checks.append([name,counts])
                            finally:
                                if out.is_dir():
                                    reclaim(out)
                                    shutil.rmtree(out)
                    except (RuntimeError,ValueError,subprocess.CalledProcessError,subprocess.TimeoutExpired,duckdb.Error) as exc:
                        if not skip_failed_experiment(result,mode,tag.removeprefix("trial-"),names[0],oracle,"official-dev",exc):
                            raise
                        save(result,result_dir)
                        continue
                    qualified.append(tag)
                    result["answer_checks"][tag]=checks
                    save(result,result_dir)
                tags=qualified
                lowmem=stream_generator(work) if mode=="confirm" and cpu["mem_bytes"]<13*GIB else gen
                shutil.rmtree(dev)
                if config.get("only_dev"):
                    result["active"]={"stage":"finished"}; save(result,result_dir); return
                sizes=selected_datasets(config,mode,seeds,replica)
                for n,seed in sizes:
                    free=shutil.disk_usage(work).free
                    need=(4.5 if n==20000000 else (10.5 if n>60000000 else 8.5))*GIB
                    if free<need:
                        if n>=50000000 and result.get("final_focus"):
                            raise ValueError("space for final comparison and independent outputs unavailable")
                        if n==60000000:
                            result["skips"][str(n)] = f"Only {free/GIB:.1f} GiB free for dataset and spill"
                            save(result,result_dir); continue
                        raise ValueError("space for 20M input and independent outputs unavailable")
                    data=work/"inputs"
                    generator=lowmem if n>=50000000 else gen
                    click_count=dict(extra_final_shapes(config) or ()).get(n,n//4)
                    run(generator,"--seed",seed,"--events",n,"--clicks",click_count,"--threads",cpu["workers"],"--output",data)
                    label=f"{n//1000000}M-{seed}"
                    result["active"] = {"stage":"paired","data":label}; save(result,result_dir)
                    oracle_native = bool(config.get("oracle_native", mode=="confirm" and not oracle))
                    shapes=[(label,cpu)]
                    if config.get("cpu_shapes") and cpu["workers"]>=4:
                        for quota in (2,3):
                            shaped=dict(cpu,workers=quota)
                            shapes.append((label.split("-")[0]+"-q"+str(quota)+"-"+str(seed),shaped))
                    for case,resources in shapes:
                        paired(data,case,tags,("trial-"+oracle if oracle else tags[0]),oracle_native,rounds,work,resources,memory,db,result,result_dir)
                    shutil.rmtree(data)
            finally:db.close()
            result["active"]={"stage":"finished"}
    except Exception as exc:
        result["error"]=type(exc).__name__; raise
    finally:save(result,result_dir)

if __name__ == "__main__":
    if json.loads(Path(".github/runtime-compare.json").read_text())["mode"] in ("profile","branchprofile","exactcycles","exactbranches"):
        from runtime_pc import main as sample_native
        sample_native()
    else:
        main()