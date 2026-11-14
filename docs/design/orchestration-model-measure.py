#!/usr/bin/env python3
# Read-only, stdlib only. Measures docs/design/orchestration-model.md section 6 from ~/.local/state/wsx/tasks.
# Usage: python3 -I docs/design/orchestration-model-measure.py [sample]  (sample prints 30 manager messages after #396)
import json,glob,os,re,sys,statistics as st,random,collections,datetime
ROOT=os.path.expanduser('~/.local/state/wsx/tasks')
T396=1794404392028
def proj(store): return re.sub(r'-[0-9a-f]{8}$','',store)
def jl(p):
    out=[]
    for l in open(p):
        l=l.strip()
        if l:
            try: out.append(json.loads(l))
            except Exception: pass
    return out
def day(ms): return datetime.datetime.fromtimestamp(ms/1000,datetime.timezone.utc).strftime('%Y-%m-%d')
def med(x): return st.median(x) if x else None
def usd(x): return '-' if x is None else f'{x:.3f}'
stores={}
for d in sorted(os.listdir(ROOT)):
    files=[f for f in glob.glob(f'{ROOT}/{d}/manager/log*.jsonl')]
    if files:
        files.sort(key=lambda f:(0 if f.endswith('log.jsonl') else 1,f))
        rows=[]
        for f in sorted(files,key=lambda f:int(re.search(r'log\.?(\d*)\.?jsonl',f).group(1) or 10**9)): rows+=jl(f)
        rows.sort(key=lambda r:r['unix_ms'])  # stable
        stores[proj(d)]=dict(dir=d,rows=rows,files=len(files))
NOW=max(r['unix_ms'] for s in stores.values() for r in s['rows'])
# ---- turns
for p,s in stores.items():
    msgs={r['id']:r for r in s['rows'] if r['kind']=='message'}
    s['msgs']=msgs; turns=[]; pend=[]; rot=False; prev_t=0; agree=[0,0]
    for r in s['rows']:
        k=r['kind']
        if k=='delivered': pend+=r['ids']
        elif k=='rotated': rot=True
        elif k=='reply':
            if r.get('post') or 'usage' not in r:
                pend=[]; continue  # post reply / no usage: not a turn
            ids=set(pend)|set(r.get('to',[]))
            agree[0]+= set(pend)==set(r.get('to',[])); agree[1]+=1
            turns.append(dict(t=r['unix_ms'],start=prev_t,id=r['id'],cost=r['usage']['cost_micro_usd']/1e6,ids=ids,rot=rot,to=r.get('to',[])))
            pend=[]; rot=False; prev_t=r['unix_ms']
        if k=='reply': prev_t=r['unix_ms']
    s['turns']=turns; s['agree']=agree
    s['by_msg']={}
    for t in turns:
        for i in t['ids']: s['by_msg'].setdefault(i,t)
    s['tasks']={}
    for f in glob.glob(f"{ROOT}/{s['dir']}/[0-9]*/task.json"):
        try: d=json.load(open(f)); s['tasks'][d['id']]=d
        except Exception: pass
    s['chats']=glob.glob(f"{ROOT}/{s['dir']}/chats/*/log.jsonl")+glob.glob(f"{ROOT}/{s['dir']}/roles/*/log.jsonl")
def isev(s,i): return s['msgs'].get(i,{}).get('from')=='events'
out=[]
P=out.append
# a
P('## a. stores/chats')
P(f"stores with manager log: {len(stores)}; rolled logs found: {sum(s['files']-1 for s in stores.values())}; NOW={NOW} ({day(NOW)})")
for p,s in stores.items():
    for c in s['chats']:
        r=jl(c); P(f"chat: {p}/{os.path.basename(os.path.dirname(c))} lines={len(r)} {day(r[0]['unix_ms'])}..{day(r[-1]['unix_ms'])}")
cf=collections.defaultdict(lambda:[0,10**15,0])
for p,s in stores.items():
    for r in s['rows']:
        if r['kind']=='message' and r['from'].startswith('chat:'):
            c=cf[(p,r['from'][5:])]; c[0]+=1; c[1]=min(c[1],r['unix_ms']); c[2]=max(c[2],r['unix_ms'])
P('chats seen only as message senders chat:<name> in manager logs: '+'; '.join(f'{p}/{n} msgs={c[0]} {day(c[1])}..{day(c[2])}' for (p,n),c in sorted(cf.items())))
# b
P('## b. manager->manager')
mat=collections.Counter(); bef=aft=0; allm=[]
for p,s in stores.items():
    for r in s['rows']:
        if r['kind']=='message' and re.match(r'manager(-reply)?:',r['from']):
            snd=r['from'].split(':',1)[1]; kind='reply' if r['from'].startswith('manager-reply') else 'ask'
            mat[(snd,p,kind)]+=1; allm.append((r['unix_ms'],snd,p,kind,r['text'],r['id']))
            if r['unix_ms']<T396: bef+=1
            else: aft+=1
P(f"total={len(allm)} before T396={bef} after={aft}; manager:=%d manager-reply:=%d"%(sum(1 for m in allm if m[3]=='ask'),sum(1 for m in allm if m[3]=='reply')))
names=sorted({m[1] for m in allm}|{m[2] for m in allm})
for tag,f in(('all',lambda m:True),('after T396',lambda m:m[0]>=T396)):
    c=collections.Counter((m[1],m[2]) for m in allm if f(m)); P(f'matrix {tag} (rows=sender, cols=receiver)')
    P('sender\\recv '+' '.join(n[:6].rjust(6) for n in names))
    for a in names:
        if any(c[(a,b)] for b in names): P(a[:12].ljust(12)+' '+' '.join(str(c[(a,b)] or '.').rjust(6) for b in names))
random.seed(396)
samp=random.sample([m for m in allm if m[0]>=T396],min(30,aft))
SAMPLE=samp
# c
P('## c. owner msgs, rotations')
for p,s in stores.items():
    ow=sum(1 for r in s['rows'] if r['kind']=='message' and r['from']=='owner'); ro=[r for r in s['rows'] if r['kind']=='rotated']
    s['owner']=ow;s['rot']=len(ro)
    rc=collections.Counter('instr' if 'instructions' in (r.get('reason') or '') else 'idle' if 'idle' in (r.get('reason') or '') else 'none' if not r.get('reason') else 'other' for r in ro)
    P(f"{p}: owner={ow} rotations={len(ro)} {dict(rc)}")
# d
P('## d. turn cost')
allc=[];firstc=[];ev_all=[0,0.0];ev_7=[0,0.0];t7=[0,0.0]
fbr=collections.defaultdict(list)
for p,s in stores.items():
    c=[t['cost'] for t in s['turns']]; fr=[t['cost'] for t in s['turns'] if t['rot']]
    allc+=c; firstc+=fr
    for t in s['turns']:
        if t['rot']: pass
    P(f"{p}: turns={len(c)} total$={sum(c):.2f} median$={usd(med(c))} first-after-rot n={len(fr)} med$={usd(med(fr))} (own-all median {usd(med(c))})")
P(f"ALL: turns={len(allc)} total$={sum(allc):.2f} median$={usd(med(allc))}; first-after-rot n={len(firstc)} median$={usd(med(firstc))} mean$={usd(st.mean(firstc) if firstc else None)} vs all mean$={usd(st.mean(allc))}")
# rotation reason split
for p,s in stores.items():
    rs=[r for r in s['rows'] if r['kind']=='rotated']
ping=sum(r['usage']['cost_micro_usd'] for s in stores.values() for r in s['rows'] if r['kind']=='ping' and 'usage' in r)/1e6
P(f"excluded: ping rows $={ping:.2f}; turns whose reply lacks usage or are post replies are not counted (no cost recorded); reply.to == delivered-rows set in {sum(s['agree'][0] for s in stores.values())}/{sum(s['agree'][1] for s in stores.values())} turns")
# e
P('## e. event-only turns (all delivered ids from=events; union of delivered rows since previous reply and reply.to; empty set excluded)')
tot=[0,0,0,0.0,0,0.0,0,0.0]
for p,s in stores.items():
    ts=s['turns']; ev=[t for t in ts if t['ids'] and all(isev(s,i) for i in t['ids'])]
    t7s=[t for t in ts if t['t']>NOW-7*86400000]; ev7=[t for t in t7s if t in ev]
    a=(len(ts),sum(t['cost'] for t in ts),len(ev),sum(t['cost'] for t in ev),len(t7s),sum(t['cost'] for t in t7s),len(ev7),sum(t['cost'] for t in ev7))
    for i,v in enumerate(a): tot[i]+=v
    P(f"{p}: all {a[2]}/{a[0]} turns ${a[3]:.2f}/${a[1]:.2f} ({100*a[3]/max(a[1],1e-9):.0f}% $) | 7d {a[6]}/{a[4]} turns ${a[7]:.2f}/${a[5]:.2f} ({100*a[7]/max(a[5],1e-9):.0f}% $)")
a=tot
tot[1]=sum(t['cost'] for s in stores.values() for t in s['turns']);P(f"TOTAL: all {a[2]}/{a[0]} ({100*a[2]/a[0]:.0f}%) ${a[3]:.2f}/${a[1]:.2f} ({100*a[3]/a[1]:.0f}%) | 7d {a[6]}/{a[4]} ({100*a[6]/max(a[4],1):.0f}%) ${a[7]:.2f}/${a[5]:.2f} ({100*a[7]/max(a[5],1e-9):.0f}%)")
# f
P('## f. relay vs direct')
dirt=[]
for p,s in stores.items():
    n=[(i,d) for i,d in s['tasks'].items() if str(d.get('created_by','')).startswith('manager@')]
    P(f"{p}: tasks created_by manager@*: {len(n)}; by creator: "+str(dict(collections.Counter(d.get('created_by','(none)') for d in s['tasks'].values()))))
    for i,d in n: dirt.append((p,i,d))
def turn_at(s,ms):
    for t in s['turns']:
        if t['start']<ms<=t['t']: return t
def set_u(ts): return list({id(t):t for t in ts if t}.values())
def turn_cost(ts): 
    u={(id(t)):t for t in ts if t}; return len(u),sum(t['cost'] for t in u.values())
dres=[]
for p,i,d in dirt:
    o=d['created_by'].split('@',1)[1]; so=stores.get(o)
    if not so: P('origin store missing',o); continue
    pat=re.compile(rf"\b{re.escape(p)} #{i}\b")
    ts=[t for t in so['turns'] if any(pat.search(so['msgs'].get(m,{}).get('text','')) for m in t['ids'])]
    cre=turn_at(so,d['created_unix_ms'])
    n,c=turn_cost(ts+[cre]); dres.append((n,c))
    P(f"direct {p}#{i} (origin {o}, state {d['state']}): creating turn ${cre['cost'] if cre else 0:.3f}; turns delivering messages mentioning '{p} #{i}': {len(ts)}; unique turns incl creating={n} ${c:.3f}")
    P('  mention sample: '+(so['msgs'][sorted(ts[0]['ids'])[0]]['text'][:100] if ts else 'none'))
# relayed
cases=[];seen=set()
for a,sa in stores.items():
    rows=sa['rows']
    for idx,r in enumerate(rows):
        if r['kind']=='sent' and not r.get('reply') and r['to'] in stores and r['to']!=a:
            b=r['to']; sb=stores[b]
            cand=[m for m in sb['msgs'].values() if m['from']==f'manager:{a}' and r['unix_ms']-5000<=m['unix_ms']<=r['unix_ms']+180000]
            if not cand: continue
            m=min(cand,key=lambda m:abs(m['unix_ms']-r['unix_ms']))
            tb=sb['by_msg'].get(m['id'])
            if not tb: continue
            tasks=[(i,d) for i,d in sb['tasks'].items() if tb['start']<=d['created_unix_ms']<=tb['t']+1800000]
            tasks.sort(key=lambda x:x[1]['created_unix_ms'])
            if not tasks: continue
            strict=[x for x in tasks if x[1]['created_unix_ms']<=tb['t']]
            if (a,b,m['id']) in seen: continue
            seen.add((a,b,m['id']))
            ta=next((t for t in sa['turns'] if t['start']<r['unix_ms']<=t['t']),None)
            cases.append(dict(a=a,b=b,msg=m,sent=r,ta=ta,tb=tb,tasks=tasks,strict=strict))
P(f"relayed candidates (A sent -> B has manager:A message -> B task created in [turn start, turn end+30min]): {len(cases)}")
rel=[]
for c in cases:
    a,b=c['a'],c['b']; sa,sb=stores[a],stores[b]
    nums=[c['tasks'][0][0]]  # earliest task only
    evt=[t for t in sb['turns'] if any(re.match(rf"#({'|'.join(map(str,nums))}) ",sb['msgs'].get(m,{}).get('text','')) and isev(sb,m) for m in t['ids'])]
    # B reply to A after delivering turn
    bs=[r for r in sb['rows'] if r['kind']=='sent' and r['to']==a and r.get('reply') and r['unix_ms']>=c['tb']['start']]
    tbr=next((t for t in sb['turns'] if bs and t['start']<bs[0]['unix_ms']<=t['t']),None) if bs else None
    ra=[m for m in sa['msgs'].values() if m['from']==f'manager-reply:{b}' and bs and bs[0]['unix_ms']-5000<=m['unix_ms']<=bs[0]['unix_ms']+180000]
    tad=sa['by_msg'].get(ra[0]['id']) if ra else None
    ta_set=[x for x in (c['ta'],tad) if x]; tb_set=[c['tb'],tbr]+evt
    na,ca=turn_cost(ta_set); nb,cb=turn_cost(tb_set)
    evset={id(t) for t in evt}; sv=[t for t in set_u(ta_set+tb_set) if t is not c['ta'] and id(t) not in evset]
    rel.append(dict(unamb=len(c['strict'])==1,sv_n=len(sv),sv_c=sum(t['cost'] for t in sv),a=a,b=b,t=c['msg']['unix_ms'],tasks=nums,na=na,ca=ca,nb=nb,cb=cb,nev=len(evt),reply=bool(bs),back=bool(tad)))
for x in rel: P(f"  {day(x['t'])} {x['a']}->{x['b']} tasks {x['b']}#{x['tasks']}: A turns={x['na']} ${x['ca']:.3f}; B turns={x['nb']} (event turns {x['nev']}) ${x['cb']:.3f}; total {x['na']+x['nb']} turns ${x['ca']+x['cb']:.3f}; B replied={x['reply']} A got reply turn={x['back']}")
if rel:
    U=[x for x in rel if x['unamb']]
    P(f"unambiguous (exactly one B task created inside the delivering turn) n={len(U)}: median turns={med([x['na']+x['nb'] for x in U])} median $={med([x['ca']+x['cb'] for x in U]):.3f}; saving (turns not A-sending/not task-event: B delivering+B reply+A reply-delivery) median turns={med([x['sv_n'] for x in U])} median $={med([x['sv_c'] for x in U]):.3f}; all-rel saving median turns={med([x['sv_n'] for x in rel])} $={med([x['sv_c'] for x in rel]):.3f}; relayed-with-reply n={sum(x['back'] for x in rel)}")
    P(f"relayed n={len(rel)}: median turns={med([x['na']+x['nb'] for x in rel])} median $={med([x['ca']+x['cb'] for x in rel]):.3f} (A side median ${med([x['ca'] for x in rel]):.3f}, A turns {med([x['na'] for x in rel])})")
    P(f"relayed ~ B-only (what a direct job would also cost B, B turns on task events): median B turns={med([x['nb'] for x in rel])} ${med([x['cb'] for x in rel]):.3f}")
if dres: P(f"direct n={len(dres)}: median turns={med([n for n,_ in dres])} median ${med([c for _,c in dres]):.3f}")
# g
P('## g. total manager $')
for p,s in stores.items():
    c=sum(t['cost'] for t in s['turns']); P(f"{p}: ${c:.2f} turns={len(s['turns'])} span {day(s['rows'][0]['unix_ms'])}..{day(s['rows'][-1]['unix_ms'])}")
P(f"sum ${sum(t['cost'] for s in stores.values() for t in s['turns']):.2f}")
print('\n'.join(out))
if len(sys.argv)>1:
    print('## SAMPLE')
    for m in SAMPLE: print(f"[{m[5]}] {m[1]}->{m[2]} {m[3]}: {m[4][:260]!r}")
if rel:
    for tag,f in(('before T396',lambda x:x['t']<T396),('after T396',lambda x:x['t']>=T396)):
        X=[x for x in rel if f(x)]; U=[x for x in X if x['unamb']]
        if X: print(f"relayed {tag}: n={len(X)} unamb={len(U)} median turns(unamb)={med([x['na']+x['nb'] for x in U])} $={med([x['ca']+x['cb'] for x in U]) or 0:.3f}; saving turns={med([x['sv_n'] for x in U])} $={med([x['sv_c'] for x in U]) or 0:.3f}")
    U=[x for x in rel if x['unamb'] and x['back']]
    print(f"unamb+replied n={len(U)} total $ median {med([x['ca']+x['cb'] for x in U]):.3f} turns {med([x['na']+x['nb'] for x in U])}; A-side turns $ median {med([x['ca'] for x in U]):.3f}; saving $ median {med([x['sv_c'] for x in U]):.3f} mean {st.mean([x['sv_c'] for x in U]):.3f}; share of total {100*sum(x['sv_c'] for x in U)/sum(x['ca']+x['cb'] for x in U):.0f}%")
    print('sample5:'); 
    for x in [x for x in U][ -5:]: print(' ',day(x['t']),x['a'],'->',x['b'],x['tasks'],f"A {x['na']}t ${x['ca']:.2f} B {x['nb']}t ${x['cb']:.2f} saving {x['sv_n']}t ${x['sv_c']:.2f}")
