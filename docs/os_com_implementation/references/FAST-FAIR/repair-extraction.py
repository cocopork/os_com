from pathlib import Path
import pymupdf as fitz
import re,json
root=Path(r'E:\GGBoy\HSSL\Project\Ant\AchillesFS\relate_works')
out=root/'FAST-FAIR-extracted'
doc=fitz.open(root/'FAST&FAIR.pdf')
texts=[]
for i,p in enumerate(doc):
    if i==0: t=p.get_text(sort=True)
    else:
        w,h=p.rect.width,p.rect.height
        top=105 if i==1 else 35
        t=p.get_text(clip=fitz.Rect(20,top,w/2,h-35),sort=True)+'\n'+p.get_text(clip=fitz.Rect(w/2,top,w-20,h-35),sort=True)
    texts.append(t)
(out/'analysis/full-text.md').write_text('# Full text\n\nPDF page 1 is the proceedings cover; PDF pages 2–15 correspond to printed pages 187–200. Columns are extracted left then right. Figure/algorithm layouts may require the original PDF.\n\n'+'\n\n'.join(f'<!-- PAGE {i+1} -->\n\n{t}' for i,t in enumerate(texts)),encoding='utf-8')
refs='\n'.join(texts[13:])
entries=re.split(r'(?m)(?=^\s*\[\d+\])',refs)
entries=[e.strip() for e in entries if re.match(r'\[\d+\]',e.strip())]
(out/'analysis/references.md').write_text('# References\n\n'+'\n\n'.join(entries),encoding='utf-8')
(out/'analysis/pages').mkdir(exist_ok=True)
for i,p in enumerate(doc):
    if 1<=i<=12: p.get_pixmap(matrix=fitz.Matrix(1,1)).save(out/f'analysis/pages/page-{i+1:02d}.png')
print('pages',len(doc),'references',len(entries))
print('\n'.join(texts[5:9]))
