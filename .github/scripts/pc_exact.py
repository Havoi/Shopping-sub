"""Build and verify the exact primary executable emitted by the submission's wide stage."""
import hashlib,re,shutil,struct,subprocess as sp
from pathlib import Path

def elf_layout(path):
    data=path.read_bytes()
    if data[:6]!=b'\x7fELF\x02\x01':raise ValueError('little-endian ELF64 required')
    if struct.unpack_from('<HH',data,16)!=(2,62):raise ValueError('static x86-64 EXEC required')
    entry,ph,sh=struct.unpack_from('<QQQ',data,24)
    ps,pn,ss,sn,names=struct.unpack_from('<HHHHH',data,54)
    sections=[struct.unpack_from('<IIQQQQIIQQ',data,sh+i*ss) for i in range(sn)]
    _,_,_,_,start,nlen,*_=sections[names];strings=data[start:start+nlen]
    alloc={};executable=[]
    for ni,kind,flags,va,file,n,*_ in sections:
        if not flags&2 or not n:continue
        label=strings[ni:].split(b'\x00',1)[0].decode('ascii')
        raw=data[file:file+n] if kind!=8 else b''
        alloc[label]=(kind,flags,va,n,hashlib.sha256(raw).hexdigest())
        if flags&4 and raw:executable.append((va,raw))
    loads=[]
    for i in range(pn):
        t,flags,off,va,pa,fs,ms,align=struct.unpack_from('<IIQQQQQQ',data,ph+i*ps)
        if t==1:loads.append((flags,off,va,pa,fs,ms,align))
    if '.text' not in alloc:raise ValueError('ELF lacks text')
    return (entry,loads,alloc),executable

def checked_functions(shipped,symbols,exports):
    target,running=elf_layout(shipped)
    source,mapped=elf_layout(symbols)
    full=target==source
    verified=set();seen={}
    for base,size,name in exports:
        if not size:continue
        if base not in seen: seen[base]=set()
        seen[base].add((size,name))
    for base,options in seen.items():
        sizes={size for size,_ in options}
        if len(sizes)!=1 or (not full and len(options)!=1):continue
        n=max(sizes)
        matches=[(v,seg) for v,seg in mapped if v<=base and base+n<=v+len(seg)]
        targets=[(v,seg) for v,seg in running if v<=base and base+n<=v+len(seg)]
        if len(matches)!=1 or len(targets)!=1:continue
        va,seg=matches[0];tv,ts=targets[0]
        if full:verified.add(base);continue
        # Same address, exact complete bytes and only one occurrence across all
        # executable sections are required for a partial symbol association.
        body=seg[base-va:base-va+n]
        if n>=16 and body==ts[base-tv:base-tv+n] and sum(x.count(body) for _,x in running)==1:
            verified.add(base)
    return verified,dict(all_allocated_sections_and_load_layout_equal=full,entry_equal=target[0]==source[0],program_headers_equal=target[1]==source[1],text_section_equal=target[2]['.text']==source[2]['.text'],verified_symbol_starts=len(verified),symbol_candidates=len(seen),shipped_sha=hashlib.sha256(shipped.read_bytes()).hexdigest(),symbols_sha=hashlib.sha256(symbols.read_bytes()).hexdigest())

def release_tiers(original):
    final=original[list(re.finditer(r'(?im)^FROM\s+',original))[-1].start():]
    pairs=re.findall(r'(?im)^COPY\s+--from=wide-build\s+(/(?:vbmi|ifma|pipeline))\s+(/[-a-zA-Z0-9_./]+)\s*$',final)
    tiers={}
    # Derive the complete ISA required by each cargo-built leaf, including GFNI.
    joined=original.replace('\\\n',' ')
    for leaf,target in pairs:
        segments=joined.split('&&');features=set()
        for i,seg in enumerate(segments):
            if re.search(r'\bcp\s+\S+/pipeline\s+'+re.escape(leaf)+r'(?:\s|$)',seg) and i:
                flags=re.search(r'-Ctarget-feature=(\S+)',segments[i-1])
                if flags:features.update(v[1:] for v in flags.group(1).split(',') if v.startswith('+'))
        if leaf=='/ifma':features.add('avx512ifma')
        if leaf=='/vbmi':features.update(('avx512ifma','avx512vbmi'))
        obj=tiers.setdefault(leaf,{'paths':[],'requires':sorted(features)});obj['paths'].append(target)
    if '/pipeline' not in tiers:raise ValueError('production wide fallback missing')
    return tiers

def run_marker(image,paths,marker,cpu,sde=None):
    args=['docker','run','--rm','--network','none','--read-only','--cap-drop','ALL','--security-opt','no-new-privileges','--user','1000:1000','--pids-limit','256','--tmpfs','/tmp:rw,mode=1777,size=32m']
    if cpu.get('cpuset'):args+=['--cpuset-cpus',cpu['cpuset']]
    for path in paths:args+=['-v',str(marker)+':'+path+':ro']
    if sde:
        kit,model,entry,*override=sde
        args+=['-v',str(kit)+':/sde:ro']
        if override:args+=['-v',str(override[0])+':/pc-test-cpuid:ro']
        args+=['--entrypoint','/sde/sde64',image,'-'+model]
        if override:args+=['-cpuid_in','/pc-test-cpuid']
        args+=['--',entry]
    else:args.append(image)
    p=sp.run(args,stdout=sp.DEVNULL,stderr=sp.DEVNULL,timeout=45)
    return p.returncode

def detect_tier(image,tiers,work,cpu,sde=None):
    # Probe one leaf at a time, highest first, so intermediate executables keep
    # running the real image dispatch. Only paths actually installed are replaced.
    marker=work/'choice';source=work/'choice.c';source.write_text('int main(void){return 183;}\n')
    sp.run(['cc','-static','-O2',str(source),'-o',str(marker)],check=True)
    flags={v.replace('avx512_vbmi','avx512vbmi') for v in cpu['flags']}
    for tier in ('/vbmi','/ifma','/pipeline'):
        if tier not in tiers or not set(tiers[tier]['requires']).issubset(flags):continue
        for path in tiers[tier]['paths']:
            if run_marker(image,[path],marker,cpu,sde)==183:return tier,path
    raise ValueError('actual production dispatch did not reach a compatible wide leaf')

def build_exact(context,work,image,gate,cpu,sde=None):
    original=(context/'Dockerfile').read_text();tiers=release_tiers(original)
    prod=image+'-production';stageimg=image+'-maps'
    args=['docker','build','--platform','linux/amd64']
    try:
        sp.run([*args,'-t',prod,'-f',str(context/'Dockerfile'),str(context)],check=True,timeout=1200)
        tier,installed=detect_tier(prod,tiers,work,cpu,sde)
        cid=sp.check_output(['docker','create',prod],text=True).strip()
        try:sp.run(['docker','cp',cid+':'+installed,str(work/'shipped')],check=True)
        finally:sp.run(['docker','rm',cid],stdout=sp.DEVNULL,check=True)
        # Preserve this specific tier after the link and immediately before
        # the existing strip. This does not add flags or alter generated code.
        stages=list(re.finditer(r'(?im)^FROM\s+[^\n]+$',original))
        index=next((i for i,r in enumerate(stages) if re.search(r'\sAS\s+wide-build\s*$',r.group(),re.I)),None)
        if index is None or index+1>=len(stages):raise ValueError('wide stage missing')
        stage=original[:stages[index+1].start()]
        last=tier==('/vbmi' if '/vbmi' in tiers else '/ifma' if '/ifma' in tiers else '/pipeline')
        if last:
            # Cargo retains its last unstripped link. The original build layers
            # can be reused byte-for-byte in this common case.
            maps='/src/target/x86_64-unknown-linux-musl/release/pipeline'
        else:
            matches=list(re.finditer(r'\bstrip\s+(?:/[-\w]+[ \t]*)+',stage))
            at=next((m.start() for m in matches if tier in m.group().split()[1:]),None)
            if at is None:raise ValueError('unrecognized production strip step')
            stage=stage[:at]+'cp '+tier+' /pc-unstripped && '+stage[at:]
            maps='/pc-unstripped'
        df=context/'Dockerfile.exactmap'
        df.write_text(stage+'\nFROM scratch\nCOPY --from=wide-build '+maps+' /symbols\nENTRYPOINT ["/symbols"]\n')
        sp.run([*args,'-t',stageimg,'-f',str(df),str(context)],check=True,timeout=1200)
        cid=sp.check_output(['docker','create',stageimg],text=True).strip()
        try:sp.run(['docker','cp',cid+':/symbols',str(work/'sampled')],check=True)
        finally:sp.run(['docker','rm',cid],stdout=sp.DEVNULL,check=True)
        mini=work/'observer-image';mini.mkdir(exist_ok=True)
        shutil.copy2(work/'shipped',mini/'sampled')
        sp.run(['cc','-static','-O2',str(gate),'-o',str(mini/'gate')],check=True)
        (mini/'Dockerfile').write_text('FROM scratch\nCOPY sampled /sampled\nCOPY gate /gate\nUSER 1000:1000\nENTRYPOINT ["/sampled"]\n')
        sp.run([*args,'-t',image,str(mini)],check=True,timeout=180)
        cid=sp.check_output(['docker','create',image],text=True).strip()
        try:sp.run(['docker','cp',cid+':/sampled',str(work/'installed-check')],check=True)
        finally:sp.run(['docker','rm',cid],stdout=sp.DEVNULL,check=True)
        if (work/'installed-check').read_bytes()!=(work/'shipped').read_bytes():
            raise ValueError('profiling executable differs from production install')
        table=[];syms=sp.check_output(['nm','-n','-S','--defined-only','--demangle',str(work/'sampled')],text=True)
        for line in syms.splitlines():
            m=re.match(r'^([0-9a-fA-F]+)\s+([0-9a-fA-F]+)\s+[TtWw]\s+(.+)$',line)
            if m:table.append((int(m[1],16),int(m[2],16),m[3]))
        verified,report=checked_functions(work/'shipped',work/'sampled',table)
        report.update(selected_tier=tier,installed_path=installed,required_features=tiers[tier]['requires'],actual_launcher_marker_verified=True,profiling_bytes_equal_installed=True)
        return verified,report
    finally:sp.run(['docker','image','rm','-f',prod,stageimg],stdout=sp.DEVNULL,stderr=sp.DEVNULL)
