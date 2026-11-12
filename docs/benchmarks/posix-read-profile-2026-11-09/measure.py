from pathlib import Path
import os,subprocess,json,random,statistics,hashlib
cpu=int(Path('/tmp/vnfs-posix-profile-cpu').read_text())
rng=random.Random(92051)
report={'baseline':'a0e613e POSIX code','cpu':cpu,'toolchain':subprocess.check_output(['rustc','--version'],text=True).strip(),'profile':'release, debug=line-tables; separate allocation-instrumented and native runs','bytes_per_read':4096,'cases':[]}
cases=[(False,n,'success') for n in [1,8,64,256]]+[(False,64,'mixed'),(True,64,'success'),(True,64,'mixed')]
for metrics,n,case in cases:
 iterations=min(200000,640000//n)
 pairs=[]
 for _ in range(9):
  order=['before','after'];rng.shuffle(order);pair={}
  for label in order:
   binary='/tmp/vnfs-posix-read-'+label+('-metrics' if metrics else '')
   env=dict(os.environ,PROFILE_FILES=str(n),PROFILE_ITERATIONS=str(iterations))
   result=json.loads(subprocess.check_output(['taskset','-c',str(cpu),binary,'/tmp/vnfs-posix-read-profile-fixture','posix',case],env=env,text=True))
   assert result['successful']==(n-(case=='mixed'))*iterations,result
   assert result['failed']==(case=='mixed')*iterations,result
   assert result['returned_bytes']==result['successful']*4096,result
   pair[label]=result
  pairs.append(pair)
 summary={}
 for metric in ['wall_ns','cpu_us','allocations','allocated_bytes','peak_live_delta']:
  b=statistics.median(p['before'][metric] for p in pairs);a=statistics.median(p['after'][metric] for p in pairs)
  summary[metric]={'before':b,'after':a}
  if b:
   ratios=[p['after'][metric]/p['before'][metric] for p in pairs]
   boot=sorted(statistics.median(rng.choices(ratios,k=len(ratios))) for _ in range(5000))
   summary[metric].update(after_over_before=statistics.median(ratios),bootstrap_95=[boot[124],boot[4874]])
 entry={'allocation_instrumentation':metrics,'cohort':n,'case':case,'iterations':iterations,'summary':summary,'pairs':pairs}
 report['cases'].append(entry)
 print(json.dumps({k:v for k,v in entry.items() if k!='pairs'}),flush=True)
for label in ['before','after','before-metrics','after-metrics']:
 p=Path('/tmp/vnfs-posix-read-'+label);report[label+'_sha256']=hashlib.file_digest(p.open('rb'),'sha256').hexdigest()
Path('/tmp/vnfs-posix-profile-pairs.json').write_text(json.dumps(report,indent=2)+'\n')
