"""Reuse an independent full verification only after comparing every emitted byte."""
from collections import defaultdict
from pathlib import Path
import hashlib,os,shutil,time

CHUNK=1<<20

class ExactOutputCache:
    def __init__(self,folder,schemas,load,gold,db,stats,limit=512<<20,materialize=False):
        self.folder=folder;self.schemas=schemas;self.load=load;self.gold=gold;self.db=db;self.stats=stats
        self.limit=limit;self.entries=[];self.serial=0;self.held=0;self.seen=set();self.has_table=set();self.reference={};self.materialize=materialize
        stats.update(sql_tables=0,hit_tables=0,full_hits=0,snapshots=0,snapshot_bytes=0,check_seconds=0.0,gold_seconds=0.0,matching_seconds=0.0,copy_seconds=0.0)
        folder.mkdir()

    def manifest(self,path,table):
        folder=path/table
        if folder.is_symlink() or not folder.is_dir():raise ValueError('missing output table')
        sizes=defaultdict(list)
        for p in folder.glob('*.parquet'):
            if p.name.startswith(('_','.')) or p.is_symlink() or not p.is_file():continue
            sizes[p.stat().st_size].append(p)
        if not sizes:raise ValueError('missing output files')
        return tuple((size,len(files)) for size,files in sorted(sizes.items())),[files for _,files in sorted(sizes.items())]

    @staticmethod
    def fingerprint(path):
        with path.open('rb') as stream:return hashlib.file_digest(stream,'sha256').digest()

    def ordered(self,groups):
        files=[]
        for group in groups:
            # Length alone identifies nearly every shard. Sorting same-sized files
            # by a hash permits renamed/repartitioned filenames; equality below
            # always requires reading the complete bytes of each corresponding file.
            if len(group)>1:group=sorted(group,key=self.fingerprint)
            files.extend(group)
        return files

    @staticmethod
    def equal(a,b):
        with a.open('rb') as x,b.open('rb') as y:
            while True:
                p=x.read(CHUNK);q=y.read(CHUNK)
                if p!=q:return False
                if not p:return True

    def remember(self,shape,files,rows):
        size=sum(sz*count for sz,count in shape[1])
        if size>self.limit or shutil.disk_usage(self.folder).free<size+(1<<30):return
        while self.entries and (self.held+size>self.limit or len(self.entries)>=12):
            old=self.entries.pop(0);self.held-=old[4];shutil.rmtree(old[3])
        directory=self.folder/str(self.serial);self.serial+=1;directory.mkdir()
        dest=[]
        try:
            for n,f in enumerate(files):
                d=directory/str(n);shutil.copyfile(f,d);dest.append(d)
        except OSError:
            shutil.rmtree(directory,ignore_errors=True);return
        self.entries.append((shape,dest,rows,directory,size));self.held+=size
        self.stats['snapshots']+=1;self.stats['snapshot_bytes']=self.held

    def prepare(self,table):
        if table in self.reference:return
        start=time.perf_counter();db=self.db;s=self.schemas[table];k=next(iter(s));view='audit_gold_'+table
        self.load(db,self.gold,table,view,False)
        if self.materialize:
            name='audit_data_'+table
            db.execute(f'CREATE TEMP TABLE {name} AS SELECT * FROM {view}')
        else:name=view
        groups,bad=db.execute(f'SELECT count(*),coalesce(bool_or(n<>1 OR {k} IS NULL OR day IS NULL),false) FROM (SELECT {k},day,count(*) n FROM {name} GROUP BY ALL)').fetchone()
        if bad:raise ValueError(f'{table}: invalid reference key')
        self.reference[table]=(name,groups)
        self.stats['gold_seconds']+=time.perf_counter()-start

    def sql(self,table,path):
        self.prepare(table);db=self.db;s=self.schemas[table];key=next(iter(s));ref,count=self.reference[table];self.load(db,path,table,'audit_candidate',False)
        actual=db.execute('SELECT count(*) FROM audit_candidate').fetchone()[0]
        if actual!=count:raise ValueError(f'{table}: expected {count} rows, got {actual}')
        different=' OR '.join(f'w.{c} IS DISTINCT FROM g.{c}' for c in s if c not in (key,'day'))
        # Equal counts and unique reference keys suffice: every reference row
        # must have a matching candidate. Any duplicate/extra candidate forces
        # at least one missing or changed reference row.
        bad=db.execute(f'SELECT w.{key},w.day FROM {ref} w LEFT JOIN audit_candidate g ON w.{key}=g.{key} AND w.day=g.day WHERE g.{key} IS NULL OR {different} LIMIT 1').fetchone()
        if bad:raise ValueError(f'{table}: unmatched key or different values at {bad}')
        return count

    def __call__(self,gold,path,db,*,validate_gold=False):
        if gold!=self.gold or db is not self.db:raise ValueError('different immutable reference')
        rows={};hits=0
        for table in self.schemas:
            start=time.perf_counter();parts,groups=self.manifest(path,table);shape=(table,parts);files=self.ordered(groups)
            for entry in reversed(self.entries):
                if entry[0]==shape and all(self.equal(a,b) for a,b in zip(files,entry[1],strict=True)):
                    self.entries.remove(entry);self.entries.append(entry);rows[table]=entry[2];hits+=1;break
            self.stats['matching_seconds']+=time.perf_counter()-start
            if table in rows:continue
            start=time.perf_counter();rows[table]=self.sql(table,path);self.stats['sql_tables']+=1;self.stats['check_seconds']+=time.perf_counter()-start
            # A never-repeating user shard layout need not be copied to disk on
            # every round. Store the first successful table and recurring layouts.
            if table not in self.has_table or shape in self.seen:
                start=time.perf_counter();self.remember(shape,files,rows[table]);self.stats['copy_seconds']+=time.perf_counter()-start
            self.seen.add(shape);self.has_table.add(table)
        self.stats['hit_tables']+=hits;self.stats['full_hits']+=int(hits==len(self.schemas))
        return rows

    def close(self):
        for table in self.schemas:
            if self.materialize:self.db.execute('DROP TABLE IF EXISTS audit_data_'+table)
            self.db.execute('DROP VIEW IF EXISTS audit_gold_'+table)
        self.db.execute('DROP VIEW IF EXISTS audit_candidate')
