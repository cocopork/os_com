from pathlib import Path
from PIL import Image,ImageDraw
r=Path(r'E:\GGBoy\HSSL\Project\Ant\AchillesFS\relate_works\FAST-FAIR-extracted\analysis')
canvas=Image.new('RGB',(1224,1584),'white')
for n,idx in enumerate([4,6,10,11]):
 im=Image.open(r/f'pages/page-{idx:02d}.png'); canvas.paste(im,((n%2)*612,(n//2)*792))
canvas.save(r/'pages/overview.png')
