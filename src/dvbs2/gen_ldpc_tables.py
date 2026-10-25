"""Generates ldpc_tables.rs from the DVB-S2 standard, ETSI EN 302 307-1
V1.4.1 (free from etsi.org -- its server wants a browser User-Agent):

    pdftotext -bbox en_30230701v010401p.pdf bbox.html
    python3 gen_ldpc_tables.py bbox.html ldpc_tables.rs

Reads annexes B and C by word position (their pages are set in several
columns, and some long rows wrap mid-line), rebuilds each table's rows
from its known shape (a block of high-degree rows, then rows of 3), and
refuses to write anything unless every table's row layout is unique,
every address is below n-k, and -- for the normal-frame codes, which are
check-regular -- every residue mod q appears equally often, which a
misplaced number would break.
"""
import re, sys, collections
html=open(sys.argv[1]).read()
pages=re.findall(r'<page[^>]*>(.*?)</page>', html, re.S)
W=re.compile(r'<word xMin="([\d.]+)" yMin="([\d.]+)" xMax="([\d.]+)" yMax="([\d.]+)">([^<]*)</word>')
kn={"1/4":16200,"1/3":21600,"2/5":25920,"1/2":32400,"3/5":38880,"2/3":43200,"3/4":48600,"4/5":51840,"5/6":54000,"8/9":57600,"9/10":58320}
ks={"1/4":3240,"1/3":5400,"2/5":6480,"1/2":7200,"3/5":9720,"2/3":10800,"3/4":11880,"4/5":12600,"5/6":13320,"8/9":14400}
tables=collections.OrderedDict()   # key -> list of lines (each list of ints), in reading order
cur=None; inannex=False
for pg in pages:
    words=[(float(a),float(b),float(c),float(d),t) for a,b,c,d,t in W.findall(pg)]
    text=" ".join(w[4] for w in words)
    if "Example of interpretation of table B.4" in text: inannex=True
    if "Annex D (normative)" in text and inannex and "Additional" in text: 
        # stop after annex C
        pass
    if not inannex: continue
    # titles on this page
    titles=[]
    for i,w in enumerate(words):
        if w[4]=="Table" and i+2<len(words) and re.fullmatch(r"[BC]\.\d+:",words[i+1][4]) and words[i+2][4]=="Rate":
            titles.append((w[1],words[i+1][4][0],words[i+3][4]))
    # group words into lines by yMin (rounded)
    lines=collections.defaultdict(list)
    for w in words:
        if w[1]<60 or w[1]>790: continue  # page header/footer
        lines[round(w[1],0)].append(w)
    # split each line into segments at big gaps
    segs=[]  # (y, x, [ints])
    for y,ws in sorted(lines.items()):
        ws.sort()
        cur_seg=[ws[0]]
        for a,b in zip(ws,ws[1:]):
            if b[0]-a[2]>8: segs.append((y,cur_seg)); cur_seg=[b]
            else: cur_seg.append(b)
        segs.append((y,cur_seg))
    # regions split by titles
    bounds=[(-1,None)]+[(t[0],(t[1],t[2])) for t in sorted(titles)]
    for bi,(ystart,key) in enumerate(bounds):
        yend=bounds[bi+1][0] if bi+1<len(bounds) else 1e9
        if key is not None: cur=key; tables.setdefault(cur,[])
        if cur is None: continue
        reg=[(y,s) for y,s in segs if ystart<y<yend and all(re.fullmatch(r"\d+",w[4]) for w in s)]
        if not reg: continue
        xs=sorted(set(round(s[0][0]/20) for y,s in reg))
        # column clustering: x starts
        starts=sorted(s[0][0] for y,s in reg)
        cols=[]
        for x in starts:
            if not cols or x-cols[-1][-1]>30: cols.append([x])
            else: cols[-1].append(x)
        cx=[min(c) for c in cols]
        def colof(x): return max(i for i,c in enumerate(cx) if x>=c-1)
        for ci in range(len(cx)):
            for y,s in reg:
                if colof(s[0][0])==ci: tables[cur].append([int(w[4]) for w in s])
    if "Annex D (normative)" in text and tables:
        break
ok=True; fixed=collections.OrderedDict()
for (ann,rate),lines in tables.items():
    if (ann,rate)==("C","9/10"): continue
    n=64800 if ann=="B" else 16200; k=(kn if ann=="B" else ks)[rate]; R=k//360
    flat=[v for l in lines for v in l]; breaks=set(); pos=0
    for l in lines: pos+=len(l); breaks.add(pos)
    T=len(flat); sols=[]
    for L1 in range(4,40):
        for r1 in range(1,R+1):
            if r1*L1+(R-r1)*3!=T: continue
            b=[(i+1)*L1 for i in range(r1)]+[r1*L1+(j+1)*3 for j in range(R-r1)]
            if all(x in breaks for x in b): sols.append((L1,r1))
    if T==3*R and all((j+1)*3 in breaks for j in range(R)): sols.append((3,R))
    if not sols:
        # Some tables' high-degree block is set as running text (rows
        # break mid-line); the trailing degree-3 rows are still one per
        # line, which pins the split.
        r2=0
        for l in reversed(lines):
            if len(l)!=3: break
            r2+=1
        r1=R-r2
        if r1>0 and (T-3*r2)%r1==0: sols.append(((T-3*r2)//r1,r1))
    bad=[v for v in flat if v>=n-k]
    q=(n-k)//360
    resid=collections.Counter(v%q for v in flat)
    uniform=len(resid)==q and len(set(resid.values()))==1
    if not uniform and ann=="B": bad.append(("residues not uniform",sorted(resid.items(),key=lambda kv:kv[1])[:3]))
    print(ann,rate,"R",R,"T",T,"q",q,"solutions",sols,"check degree",T//q if uniform else None,"bad",bad[:3])
    if len(sols)!=1 or bad: ok=False; continue
    L1,r1=sols[0]; rows=[]; i=0
    for _ in range(r1): rows.append(flat[i:i+L1]); i+=L1
    for _ in range(R-r1): rows.append(flat[i:i+3]); i+=3
    fixed[(ann,rate)]=rows
print("OK" if ok else "PROBLEM")
if ok and len(sys.argv)>2:
    out=["// Parity bit accumulator addresses for the DVB-S2 LDPC codes, from ETSI",
         "// EN 302 307-1 V1.4.1 annex B (normal frames) and annex C (short frames):",
         "// one row per group of 360 information bits. Generated from the",
         "// standard's PDF by gen_ldpc_tables.py; don't edit by hand.",""]
    for (ann,rate),rows in fixed.items():
        nm=("NORMAL_" if ann=="B" else "SHORT_")+rate.replace("/","_")
        out.append(f"pub static {nm}: &[&[u16]] = &[")
        for r in rows: out.append("    &["+", ".join(map(str,r))+"],")
        out.append("];"); out.append("")
    open(sys.argv[2],"w").write("\n".join(out))
