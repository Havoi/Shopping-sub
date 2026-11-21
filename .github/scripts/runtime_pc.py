"""Sample the native optimized application without granting privileges to its container."""
from collections import Counter
import base64, bisect, gzip, hashlib, json, os, re, shutil, signal, struct, subprocess as sp, tempfile, time
from pathlib import Path

import duckdb
import runtime_compare as bench

HERE = Path(__file__).resolve().parent
def cmd(*args, **kwargs):
    return sp.run(list(map(str,args)), check=True, **kwargs)
def output(*args):
    return cmd(*args,stdout=sp.PIPE,text=True).stdout.strip()
def build(context, work, image, branch_mode=False):
    original=(context/"Dockerfile").read_text()
    first=re.search(r"(?im)^FROM .*\sAS wide-build\s*$",original)
    if not first: raise ValueError("base stage unavailable")
    end=re.search(r"(?im)^FROM ",original[first.end():])
    stage=original[first.start():first.end()+end.start()] if end else original[first.start():]
    # Reuse the flags/assets of the shipped IFMA build (including code generation settings).
    join=stage.replace("\\\n"," ")
    matches=re.findall(r"((?:CARGO_PROFILE_[A-Z0-9_]+=\S+\s+)*cargo rustc\s+--locked\s+--release\s+--target\s+x86_64-unknown-linux-musl\s+--\s+[^;&|\n]*-Ctarget-feature=\+avx512ifma(?:\s+[^;&|\n]*)?)\s*&&\s*cp\s+[^&\n]+\s/ifma\b",join)
    if len(matches)!=1: raise ValueError("one IFMA release command required")
    selected=matches[0].strip()
    if "avx512vbmi" in selected: raise ValueError("select the IFMA checkpoint")
    # Ordinary release recipes build the baseline and IFMA executables in one RUN.
    # Those outputs are unnecessary here. Preserve setup commands in custom recipes.
    start=stage.find("\nRUN cc -O2 -march=x86-64 -c /tmp/choose.c")
    if start>=0:
        commands=stage[start:].replace("\\\n"," ").strip().split("&&")
        if all(x.strip().startswith(("RUN cc ", "cargo rustc ", "CARGO_PROFILE_", "cp target/", "strip /pipeline")) for x in commands):
            stage=stage[:start]+"\n"
    src=context/"wide/src"
    for f in ("pc_sample.c","pc_gate.c"): shutil.copy2(HERE/f,src/f)
    # Use the shipped build stage to retain any compiler setup and flags, then rebuild
    # the selected target retaining symbols and file/line information.
    recipe=stage+"\nRUN cc -O2 -g -c src/pc_sample.c -o /tmp/pc-sample.o && cc -static -O2 src/pc_gate.c -o /pc-gate \\\n && "+selected+" -Cdebuginfo=2 -Cstrip=none -Clink-arg=/tmp/pc-sample.o -Clink-arg=-Wl,--wrap=pthread_create -Clink-arg=\"$(cc -print-libgcc-file-name)\" -Clink-arg=-lc \\\n && cp target/x86_64-unknown-linux-musl/release/pipeline /sampled\nFROM scratch\nCOPY --from=wide-build /sampled /sampled\nCOPY --from=wide-build /pc-gate /gate\nUSER 1000:1000\nENTRYPOINT [\"/sampled\"]\n"
    if branch_mode:
        recipe=recipe.replace("cc -O2 -g -c src/pc_sample.c -o /tmp/pc-sample.o && ","")
        recipe=recipe.replace(' -Clink-arg=/tmp/pc-sample.o -Clink-arg=-Wl,--wrap=pthread_create -Clink-arg="$(cc -print-libgcc-file-name)" -Clink-arg=-lc',"")
    df=context/"Dockerfile.pc";df.write_text(recipe)
    cmd("docker","build","--platform","linux/amd64","-t",image,"-f",df,context,timeout=1200)
    cid=output("docker","create",image)
    try: cmd("docker","cp",cid+":/sampled",work/"sampled")
    finally: cmd("docker","rm",cid,stdout=sp.DEVNULL)
    if not re.search(r"Type:\s+EXEC",output("readelf","-h",work/"sampled")) or ".debug_line" not in output("readelf","-WS",work/"sampled"):
        raise ValueError("static executable with line tables required")
def perf_binary():
    names=sorted(Path("/usr/lib/linux-tools").glob("*/perf"),reverse=True)
    commands=[str(p) for p in names]+[shutil.which("perf")]
    if shutil.which("apt-get") and not any(p and sp.run([p,"--version"],stdout=sp.DEVNULL,stderr=sp.DEVNULL).returncode==0 for p in commands):
        sp.run(["sudo","-n","apt-get","-q","update"],timeout=100,stdout=sp.DEVNULL,stderr=sp.DEVNULL)
        sp.run(["sudo","-n","apt-get","-q","install","-y","--no-install-recommends","linux-tools-generic"],timeout=200,stdout=sp.DEVNULL,stderr=sp.DEVNULL)
        commands=[str(p) for p in sorted(Path("/usr/lib/linux-tools").glob("*/perf"),reverse=True)]+[shutil.which("perf")]
    return next((p for p in commands if p and sp.run([p,"--version"],stdout=sp.DEVNULL,stderr=sp.DEVNULL).returncode==0),None)
def choose_perf(perf,work):
    available=[]
    if not perf:return available
    for event in ("cycles:u","cpu-clock:u"):
        trial=sp.run(["sudo","-n",perf,"record","-q","-o",str(work/"trial.perf"),"-e",event,"-F","997","--","/bin/true"],stdout=sp.DEVNULL,stderr=sp.DEVNULL,timeout=15)
        if trial.returncode==0: available.append(event)
    return available
def gated(work,data,image,cpu,memory,rounds,perf,event,mode,db,gold,branch=None):
    root=work/mode; root.mkdir()
    scratch=root/"scratch"; out=root/"out"
    for p in (scratch,out): p.mkdir();p.chmod(0o777)
    # Pre-create this file as the observer's UID, leaving the unprivileged child append access.
    raw=scratch/"samples.bin";raw.touch();raw.chmod(0o666)
    args=["docker","create","--platform","linux/amd64","--network","none","--read-only","--cap-drop","ALL","--security-opt","no-new-privileges","--pids-limit","4096","--user","1000:1000","--cpus",str(cpu['workers']),"--cpuset-cpus",cpu['cpuset'],"--memory",str(memory)+"g","--memory-swap",str(memory)+"g","--tmpfs","/tmp:rw,size=64m,mode=1777","-e","NCPU="+str(cpu["workers"]),"-e","MEM_GB="+str(memory),"-e","DATA_DIR=/data","-e","SCRATCH_DIR=/scratch","-e","HOME=/scratch","-e","TMPDIR=/scratch","-v",f"{data}:/data:ro","-v",f"{out}:/output","-v",f"{scratch}:/scratch","--entrypoint","/gate"]
    if not event: args+=["-e","PC_SAMPLE_PATH=/scratch/samples.bin"]
    cid=output(*args,image,str(rounds)); records=[]; stats=None
    try:
        cmd("docker","start",cid,stdout=sp.DEVNULL)
        deadline=time.monotonic()+15
        while not (scratch/"ready").exists():
            if time.monotonic()>deadline: raise RuntimeError("gate not ready")
            time.sleep(.02)
        pid=output("docker","inspect","-f","{{.State.Pid}}",cid)
        if event:
            prefix=["sudo","-n",perf]
            # Observe only the target and descendants. Solver UID, capabilities and seccomp remain unchanged.
            branch_event=branch is not None and branch.get("kind","branch")=="branch"
            settings=["-c","4093"] if branch_event else ["-F","997"]
            if branch is not None and branch["lbr"]: settings+=["-j","any,u"]
            records.append(sp.Popen(prefix+["record","-q","--inherit","-e",event,*settings,"-o",str(root/"raw.perf"),"-p",pid],stdout=sp.DEVNULL,stderr=sp.DEVNULL))
            statfile=root/"counters.csv"; statfile.touch()
            counterspec="branches:u,branch-misses:u" if branch_event else "cycles:u,instructions:u,cache-references:u,cache-misses:u,branches:u,branch-misses:u"
            stats=sp.Popen(prefix+["stat","--inherit","-x,","-o",str(statfile),"-e",counterspec,"-p",pid],stdout=sp.DEVNULL,stderr=sp.DEVNULL)
            records.append(stats);time.sleep(.30)
            if records[0].poll() is not None: return Counter(),{},None
        (scratch/"go").touch()
        code=cmd("docker","wait",cid,stdout=sp.PIPE,text=True,timeout=240).stdout.strip()
        if code!="0": raise RuntimeError("solver failed")
        for proc in records:
            try: proc.wait(timeout=5)
            except sp.TimeoutExpired:
                sp.run(["sudo","-n","kill","-INT",str(proc.pid)],stdout=sp.DEVNULL,stderr=sp.DEVNULL);proc.wait(timeout=8)
        for i in range(rounds):
            bench.reclaim(out/str(i))
            rows=bench.check(gold,out/str(i),db,validate_gold=i==0)
            shutil.rmtree(out/str(i))
        if event:
            if records[0].returncode: return Counter(),{},None
            dump=sp.run(["sudo","-n",perf,"script","-i",str(root/"raw.perf"),"-F","ip,dso"],stdout=sp.PIPE,stderr=sp.DEVNULL,text=True,timeout=90)
            if dump.returncode:return Counter(),{},None
            counts=Counter()
            for ln in dump.stdout.splitlines():
                match=re.match(r"\s*([0-9a-fA-F]+)\s+\(([^)]+)\)",ln)
                if match and ("sampled" in match[2] or "unknown" in match[2].lower()):counts[int(match[1],16)]+=1
            counters=statfile.read_text(errors="replace")
            if branch is not None:
                branch["event_metadata"]=sp.run(prefix+["evlist","-v","-i",str(root/"raw.perf")],stdout=sp.PIPE,stderr=sp.DEVNULL,text=True,timeout=30).stdout
                if branch["lbr"]:
                    lines=sp.run(prefix+["script","-F","brstack","-i",str(root/"raw.perf")],stdout=sp.PIPE,stderr=sp.DEVNULL,text=True,timeout=90)
                    if lines.returncode==0:
                        points,total=lbr_points(lines.stdout)
                        branch["lbr_points"]=points;branch["lbr_records"]=total
        else:
            contents=raw.read_bytes()
            if len(contents)%8:raise ValueError("incomplete PC record")
            counts=Counter(x[0] for x in struct.iter_unpack("<Q",contents))
            counters=None
        return counts,rows,counters
    finally:
        for proc in records:
            if proc.poll() is None:
                sp.run(["sudo","-n","kill","-INT",str(proc.pid)],stdout=sp.DEVNULL,stderr=sp.DEVNULL)
                try: proc.wait(timeout=5)
                except sp.TimeoutExpired:
                    sp.run(["sudo","-n","kill","-KILL",str(proc.pid)],stdout=sp.DEVNULL,stderr=sp.DEVNULL);proc.wait(timeout=5)
        cmd("docker","rm","-f",cid,stdout=sp.DEVNULL)
        bench.reclaim(root)
def symbolize(binary,counts,branch_context=False,verified=None,shipped=None,allow_lines=True):
    functions=[]
    for line in output("nm","-n","-S","--defined-only","--demangle",binary).splitlines():
        found=re.match(r"^([0-9a-fA-F]+)\s+([0-9a-fA-F]+)\s+([TtWw])\s+(.+)$",line)
        if found and int(found[2],16):functions.append((int(found[1],16),int(found[2],16),found[4]))
    starts=[x[0] for x in functions]; qualified={}
    for addr,hits in counts.items():
        i=bisect.bisect_right(starts,addr)-1
        if i>=0 and (verified is None or starts[i] in verified) and addr<starts[i]+functions[i][1]:qualified[addr]=(hits,functions[i][2])
    if not qualified:return [],0
    result=cmd("addr2line","-e",binary,"-a","-f","-C","-i",input="".join(hex(x)+"\n" for x in sorted(qualified)),stdout=sp.PIPE,text=True).stdout.splitlines() if allow_lines else []
    frames={a:[] for a in qualified};cur=None
    for line in result:
        if re.fullmatch(r"0x[0-9a-fA-F]+",line):
            cur=int(line,16);frames[cur]=[]
        elif cur is not None:frames[cur].append(line)
    rows=[dict(pc=hex(pc),n=n,function=name,inline=list(zip(frames[pc][::2],frames[pc][1::2]))) for pc,(n,name) in qualified.items()]
    # Preserve exact binary instructions for sampled addresses without exporting an ELF.
    asm={};previous=[]; contexts={}
    for line in output("objdump","-d","-M","intel","--no-show-raw-insn",shipped or binary).splitlines():
        ins=re.match(r"^\s*([0-9a-fA-F]+):\s+(.*)$",line)
        if not ins:
            if line.rstrip().endswith(">:"):previous=[]
            continue
        addr=int(ins[1],16);instruction=ins[2].strip()
        if addr in qualified:
            asm[addr]=instruction
            if branch_context:contexts[addr]=list(previous)
        if branch_context and re.match(r"(?:j\w+|call\w*|ret\w*|loop\w*)\s|(?:ret)$",instruction):
            previous=(previous+[(hex(addr),instruction)])[-2:]
    for row in rows:
        addr=int(row["pc"],16);row["instruction"]=asm.get(addr,"")
        if branch_context:row["preceding_control_transfers"]=contexts.get(addr,[])
    rows.sort(key=lambda x:-x["n"])
    return rows,sum(x["n"] for x in rows)
def envelope(payload,cert,work):
    compressed=gzip.compress(json.dumps(payload,separators=(",",":")).encode(),mtime=0)
    sealed=cmd("openssl","cms","-encrypt","-binary","-aes-256-gcm","-outform","DER","-recip",cert,"-keyopt","rsa_padding_mode:oaep","-keyopt","rsa_oaep_md:sha256",input=compressed,stdout=sp.PIPE).stdout
    return base64.b64encode(sealed).decode()
def lbr_points(text):
    points=Counter(); records=0
    for line in text.splitlines():
        edges=re.findall(r"(?:0x)?([0-9a-fA-F]+)/(?:0x)?[0-9a-fA-F]+/([MP-])/",line)
        if edges:
            records+=1
            branch=next((int(ip,16) for ip,flag in edges if flag=="M"),None)
            if branch is not None:points[branch]+=1
    return points,records

def branch_options(perf,work):
    if not perf: return []
    choices=[]
    for precise in (3,2,1,0):
        event="branch-misses:u"+"p"*precise
        for lbr in (True,False):
            flags=["-j","any,u"] if lbr else []
            trial=sp.run(["sudo","-n",perf,"record","-q","-e",event,"-c","4093",*flags,"-o",str(work/"probe.perf"),"--","/bin/true"],stdout=sp.DEVNULL,stderr=sp.DEVNULL,timeout=12)
            if trial.returncode==0:choices.append((event,precise,lbr))
    return choices

def branch_profile(cfg,cpu,dest):
    exact=cfg["mode"] in ("exactcycles","exactbranches")
    cycles=cfg["mode"]=="exactcycles"
    event_count=cfg.get("profile_events",20000000)
    if event_count not in (20000000,60000000):raise ValueError("unsupported event count")
    replica=int(os.environ["REPLICA"]);rounds=int(cfg["rounds"])
    if not 3<=rounds<=14:raise ValueError("branch profile requires three to fourteen passes")
    bases=cfg["seeds"]
    if len(bases)==1:bases=[bases[0],bases[0]+8717827]
    if len(bases)!=2 or len(set(bases))!=2:raise ValueError("two distinct seeds required")
    seeds=[int(v)+replica*1000003 for v in bases]
    name=cfg["candidate_names"][-1]
    if not re.fullmatch("[a-z][a-z0-9-]{0,19}",name):raise ValueError("invalid label")
    memory=min(24,cpu["mem_bytes"]//bench.GIB-1);image="runtime-pc-image"
    record={"mode":"profile","schema":2,"replica":replica,"workers":4,"stage":"build"}
    def save():
        (dest/"pc.json").write_text(json.dumps(record,separators=(",",":")))
    save()
    try:
        with tempfile.TemporaryDirectory(dir=os.environ["RUNNER_TEMP"]) as tmp:
            work=Path(tmp);context=Path(os.environ["RUNNER_TEMP"])/"inputs"/name
            eligible=None;provenance={};shipped=None
            if exact:
                from pc_exact import build_exact
                eligible,provenance=build_exact(context,work,image,HERE/"pc_gate.c",cpu)
                shipped=work/"shipped"
            else:build(context,work,image,branch_mode=True)
            record["stage"]="official-dev";save()
            gen=bench.get_kit(work);dev=work/"dev"
            if event_count==60000000 and shutil.disk_usage(work).free<8.5*bench.GIB:raise ValueError("insufficient space for final-size profiling")
            generator=bench.stream_generator(work) if event_count==60000000 and cpu["mem_bytes"]<13*bench.GIB else gen
            cmd(gen,"--seed",1,"--events",1000000,"--clicks",250000,"--threads",4,"--output",dev)
            db=duckdb.connect(config={"threads":"4","memory_limit":"1500MiB","temp_directory":str(work/"duck")})
            try:
                checked=work/"devOut";bench.run_image(image,dev,checked,cpu,memory)
                bench.check(work/"dev-answer",checked,db,csv_gold=True,validate_gold=True)
                shutil.rmtree(checked);shutil.rmtree(dev)
                try:
                    perf=perf_binary()
                    choices=[(e,0,False) for e in choose_perf(perf,work)] if cycles else branch_options(perf,work)
                except (OSError,sp.SubprocessError):perf=None;choices=[]
                payload={"cpu":cpu["cpu"],"events_per_seed":event_count,"sha":hashlib.sha256((shipped or (work/"sampled")).read_bytes()).hexdigest(),"executable_provenance":provenance,"profiles":[],"sampling":"cycles / cpu-clock frequency 997" if cycles else "native branch misses; fixed period 4093","probed":[list(x) for x in choices],"workload_status":[]}
                for ix,seed in enumerate(seeds):
                    if not choices:
                        payload["workload_status"].append({"seed":seed,"error":"hardware event unavailable"})
                        continue
                    record.update(stage="workload",workload=ix);save()
                    inputs=work/"inputs";gold=work/"gold"
                    cmd(generator,"--seed",seed,"--events",event_count,"--clicks",event_count//4,"--threads",4,"--output",inputs)
                    bench.warm(inputs);bench.run_image(image,inputs,gold,cpu,memory)
                    complete=False
                    for ordinal,(event,level,lbr) in enumerate(choices):
                        meta={"lbr":lbr,"kind":"cycles" if cycles else "branch"};mode=f"work-{ix}-{ordinal}"
                        try:
                            bench.warm(inputs)
                            counts,rows,counters=gated(work,inputs,image,cpu,memory,rounds,perf,event,mode,db,gold,branch=meta)
                            pcs,total=symbolize(work/"sampled",counts,branch_context=not cycles,verified=eligible,shipped=shipped,allow_lines=(not exact or provenance["all_allocated_sections_and_load_layout_equal"]))
                            if total<100:continue
                            edges,edge_count=symbolize(work/"sampled",meta.pop("lbr_points",{}),verified=eligible,shipped=shipped,allow_lines=(not exact or provenance["all_allocated_sections_and_load_layout_equal"]))
                            notes=["arbitrary skid","constant skid","zero skid requested","zero skid required or randomized"][level]
                            payload["profiles"].append(dict(method=(event if cycles else "branch-misses"),seed=seed,rounds=rounds,samples=total,raw_samples=sum(counts.values()),event=event,precise_ip=level,precision_note=notes,sample_period=None if cycles else 4093,frequency_hz=997 if cycles else None,counters=counters,pc=pcs,lbr_pc=edges,lbr_samples=edge_count,lbr_explanation="Nearest flagged mispredicted FROM address in each last-branch stack, newest first; contextual, can repeat",**meta))
                            payload["workload_status"].append(dict(seed=seed,rows=rows,verified=rounds))
                            complete=True;break
                        except (sp.TimeoutExpired,PermissionError):continue
                        finally:
                            root=work/mode
                            if root.exists():bench.reclaim(root);shutil.rmtree(root)
                    if not complete:payload["workload_status"].append({"seed":seed,"error":"no attributable native event samples"})
                    shutil.rmtree(inputs);shutil.rmtree(gold)
                record.update(stage="finished",completed=len(payload["profiles"]),rounds=rounds,ciphertext=envelope(payload,HERE/"sampling.crt",work));save()
            finally:db.close()
    except Exception as err:
        record["error"]=type(err).__name__;save();raise
    finally:sp.run(["docker","image","rm","-f",image],stdout=sp.DEVNULL,stderr=sp.DEVNULL)
def main():
    cpu=bench.probe();dest=Path(os.environ["RESULT_DIR"]);cfg=json.loads(Path(".github/runtime-compare.json").read_text())
    cpu["workers"]=min(4,cpu["workers"]);cpu["cpuset"]=",".join(map(str,cpu["affinity"][:cpu["workers"]]))
    if cpu["workers"]!=4 or (not cpu["ifma"] and cfg["mode"] not in ("exactcycles","exactbranches")):raise ValueError("four workers and IFMA required")
    if cfg["mode"] in ("branchprofile","exactcycles","exactbranches") or cfg.get("profile_event")=="branch-misses":
        return branch_profile(cfg,cpu,dest)
    replica=int(os.environ["REPLICA"]);seed=cfg["seeds"][0]+replica*1000003;rounds=cfg["rounds"]
    if not 3<=rounds<=20:raise ValueError("invalid repeat count")
    name=cfg["candidate_names"][-1]
    if not re.fullmatch("[a-z][a-z0-9-]{0,19}",name):raise ValueError("invalid label")
    memory=min(24,cpu["mem_bytes"]//bench.GIB-1);image="runtime-pc-image"
    record={"mode":"profile","schema":1,"replica":replica,"seed":seed,"workers":4,"stage":"build"}
    def save(): (dest/"pc.json").write_text(json.dumps(record,separators=(",",":")))
    save()
    try:
        with tempfile.TemporaryDirectory(dir=os.environ["RUNNER_TEMP"]) as tmp:
            work=Path(tmp);context=Path(os.environ["RUNNER_TEMP"])/"inputs"/name
            build(context,work,image)
            record["stage"]="official-dev";save();gen=bench.get_kit(work);dev=work/"dev"
            cmd(gen,"--seed",1,"--events",1000000,"--clicks",250000,"--threads",4,"--output",dev)
            db=duckdb.connect(config={"threads":"4","memory_limit":"1500MiB","temp_directory":str(work/"duck")})
            try:
                checked=work/"devOut";bench.run_image(image,dev,checked,cpu,memory)
                bench.check(work/"dev-answer",checked,db,csv_gold=True,validate_gold=True);shutil.rmtree(checked);shutil.rmtree(dev)
                record["stage"]="generate";save();data=work/"inputs";cmd(gen,"--seed",seed,"--events",20000000,"--clicks",5000000,"--threads",4,"--output",data)
                bench.warm(data);gold=work/"gold";bench.run_image(image,data,gold,cpu,memory)
                try: perf=perf_binary();events=choose_perf(perf,work)
                except (OSError,sp.SubprocessError): perf=None;events=[]
                payload={"cpu":cpu["cpu"],"seed":seed,"sha":hashlib.sha256((work/"sampled").read_bytes()).hexdigest(),"profiles":[]}
                for method in [*events,None]:
                    key="hardware" if method=="cycles:u" else "cpu-clock" if method else "thread-cpu-timer"
                    record["stage"]="sample-"+key;save();bench.warm(data)
                    try:
                        counts,rows,counters=gated(work,data,image,cpu,memory,rounds,perf,method,key,db,gold)
                    except (sp.TimeoutExpired,PermissionError):
                        if method: continue
                        raise
                    addresses,total=symbolize(work/"sampled",counts)
                    if total<100 and method:continue
                    if total<100:raise ValueError("too few attributable samples")
                    payload["profiles"].append(dict(method=key,rounds=rounds,samples=total,counters=counters,raw_samples=sum(counts.values()),pc=addresses))
                    record.update(method=key,samples=total,rows=rows,rounds=rounds)
                    break
                record["ciphertext"]=envelope(payload,HERE/"sampling.crt",work)
                record["stage"]="finished";save()
            finally:db.close()
    except Exception as err:
        record["error"]=type(err).__name__;save();raise
    finally:sp.run(["docker","image","rm","-f",image],stdout=sp.DEVNULL,stderr=sp.DEVNULL)
if __name__=="__main__":main()