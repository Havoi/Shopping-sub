"""Seal only named benchmark result files; publication consumes one ciphertext file."""
import base64,gzip,hashlib,json,os,struct,subprocess,sys,tempfile
from pathlib import Path
FILES=('machine.json','comparison.json','pc.json')
HEADER=b'R13E1\x00'
BLOCK=1<<20
def seal(inputs,target,certificate):
    packed={}
    for name in FILES:
        path=inputs/name
        if not path.exists() and not path.is_symlink():continue
        if path.is_symlink() or not path.is_file():raise ValueError('unsafe result file')
        with os.fdopen(os.open(path,os.O_RDONLY|getattr(os,'O_NOFOLLOW',0)),'rb') as fp:raw=fp.read()
        packed[name]={'sha256':hashlib.sha256(raw).hexdigest(),'data':base64.b64encode(raw).decode('ascii')}
    zipped=gzip.compress(json.dumps({'schema':1,'files':packed},separators=(',',':')).encode(),mtime=0)
    head=HEADER+struct.pack('>I',len(zipped))+zipped
    frame=head+os.urandom((-len(head))%BLOCK)
    target.parent.mkdir(parents=True,exist_ok=True,mode=0o700)
    if target.exists() or target.is_symlink():raise FileExistsError('unexpected ciphertext path')
    result=subprocess.run(['openssl','cms','-encrypt','-binary','-aes-256-gcm','-outform','DER','-recip',str(certificate),'-keyopt','rsa_padding_mode:oaep','-keyopt','rsa_oaep_md:sha256'],input=frame,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,check=True)
    if len(result.stdout)<=len(frame):raise ValueError('incomplete sealed result')
    # Write ciphertext atomically. Nothing in the published directory is plaintext.
    with tempfile.NamedTemporaryFile(dir=target.parent,prefix='.sealed-',delete=False) as fp:
        temp=Path(fp.name)
        try:
            fp.write(result.stdout);fp.flush()
        except BaseException:
            temp.unlink(missing_ok=True);raise
    try:os.replace(temp,target)
    finally:temp.unlink(missing_ok=True)
if __name__=='__main__':
    seal(Path(sys.argv[1]),Path(sys.argv[2]),Path(__file__).with_name('sampling.crt'))
