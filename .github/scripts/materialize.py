"""Materialize the public controls and the proposed source deltas for this comparison."""
import base64,gzip,hashlib,json,os,re,shutil,subprocess as sp,tarfile,tempfile
from pathlib import Path
def git(*args,**kw):
 return sp.run(["git",*args],stdout=sp.PIPE,stderr=sp.PIPE,check=True,**kw).stdout
def verify(b,h):return hashlib.sha256(b).hexdigest()==h
def main():
 tmp=Path(os.environ["RUNNER_TEMP"])
 chunks=[os.environ.pop("SHOPPING_R13_DELTA"+("_"+str(i) if i>1 else ""),"") for i in range(1,5)]
 count,digest,first=chunks[0].split(".",2)
 assert count in {"1","2","3","4"}
 payload=first+"".join(chunks[1:int(count)])
 assert hashlib.sha256(payload.encode()).hexdigest()==digest
 spec=json.loads(gzip.decompress(base64.b64decode(payload,validate=True)))
 dest=tmp/"inputs";dest.mkdir()
 base="d23a4e43f01a59f1c29cc369c11c6aac2d5a3fb8"
 allowed=set(spec["sources"]) | {"Havoi/Shopping-sub"}
 assert all(re.fullmatch(r"[a-zA-Z0-9_-]+/[a-zA-Z0-9_-][a-zA-Z0-9_.-]*", repo) for repo in allowed)
 with tempfile.TemporaryDirectory(dir=tmp) as workspace:
  work=Path(workspace)
  def source(repo,sha):
   assert repo in allowed and re.fullmatch("[0-9a-f]{40}",sha)
   root=work/sha
   if root.exists():return root
   git("init","-q",str(root))
   for attempt in range(3):
    try:
     git("-C",str(root),"fetch","-q","--no-tags","--depth=1","https://github.com/"+repo+".git",sha)
     break
    except sp.CalledProcessError:
     if attempt==2:raise
     __import__("time").sleep(2**attempt)
   git("-C",str(root),"checkout","--detach","-q","FETCH_HEAD")
   assert git("-C",str(root),"rev-parse","HEAD").decode().strip()==sha
   return root
  shell=source("Havoi/Shopping-sub",base)
  for name,conf in spec["sets"].items():
   assert re.fullmatch("[a-z0-9-]{1,25}",name)
   root=dest/name
   shutil.copytree(shell,root,ignore=shutil.ignore_patterns(".git"))
   upstream=source(conf["upstream"],conf["ref"]);wide=root/"wide";wide.mkdir()
   for part in ("src",".cargo","Cargo.toml","Cargo.lock","LICENSE"):
    path=upstream/part
    if path.is_dir():shutil.copytree(path,wide/part)
    else:shutil.copy2(path,wide/part)
   for item,body in spec["common"].items():
    assert item in ("Dockerfile",".dockerignore","src/launch.c","wide/build/choose.c")
    path=root/item;path.parent.mkdir(exist_ok=True,parents=True);path.write_bytes(body.encode())
   for item,h in conf.get("assets",{}).items():
    assert item in ("Dockerfile","wide/perf.profdata","wide/hot.fdata.gz","wide/hot.ld","src/launch.c","wide/build/choose.c")
    data=base64.b64decode(spec["assets"][h],validate=True)
    assert verify(data,h)
    (root/item).write_bytes(data)
   for name1,expected,key in conf["changes"]:
    diff=spec["patches"][key]
    assert verify(diff.encode(),key)
    path=wide/name1
    assert path.resolve().is_relative_to((wide/"src").resolve()) and path.is_file()
    git("apply","--whitespace=nowarn","-",cwd=wide,input=diff.encode())
    assert verify(path.read_bytes(),expected)
   total=hashlib.sha256()
   for path in sorted((wide/"src").rglob("*"),key=lambda p:p.relative_to(wide).as_posix()):
    if path.is_file():total.update(path.relative_to(wide).as_posix().encode()+b"\0"+path.read_bytes())
   assert total.hexdigest()==conf["content_hash"]
   print("Prepared",name,flush=True)
if __name__=="__main__":
 try:main()
 except Exception as err:
  import traceback
  frame=traceback.extract_tb(err.__traceback__)
  location=[s.lineno for s in frame if s.filename==__file__]
  Path(os.environ["RESULT_DIR"],"prepare.json").write_text(json.dumps({"error":type(err).__name__,"locations":location}))
  raise SystemExit(1)
